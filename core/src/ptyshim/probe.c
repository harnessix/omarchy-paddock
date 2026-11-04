/*
 * The fake harness of the pty tests. It writes one result line per check to
 * probe.txt in the work folder, then exits with 0.
 *
 *   probe openpty      openpty, a round trip through the pty, the window
 *                      size from the master, TIOCSTI on the slave
 *   probe forkpty      forkpty, and the child writes to its terminal
 *   probe posix        posix_openpt, grantpt, unlockpt, ptsname, open
 *   probe open PATH    open another terminal by its path
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/wait.h>
#include <termios.h>
#include <unistd.h>
#ifdef __APPLE__
#include <util.h>
#else
#include <pty.h>
#endif

static FILE *out;

static void say(const char *key, const char *value) {
  fprintf(out, "%s %s\n", key, value);
  fflush(out);
}

/* Write through one side and read it back on the other. */
static int round_trip(int from, int to) {
  if (write(from, "ping\n", 5) != 5)
    return 0;
  char buf[64] = {0};
  size_t got = 0;
  while (got < sizeof buf - 1) {
    ssize_t n = read(to, buf + got, sizeof buf - 1 - got);
    if (n <= 0)
      break;
    got += (size_t)n;
    if (strstr(buf, "ping"))
      return 1;
  }
  return 0;
}

static int raw(int fd) {
  struct termios t;
  if (tcgetattr(fd, &t) < 0)
    return 0;
  cfmakeraw(&t);
  return tcsetattr(fd, TCSANOW, &t) == 0;
}

static void check_openpty(void) {
  int m, s;
  char name[128] = {0};
  if (openpty(&m, &s, name, NULL, NULL) < 0) {
    say("openpty", strerror(errno));
    return;
  }
  say("openpty", "ok");
  say("name", name);
  const char *tn = ttyname(s);
  say("ttyname", tn && strcmp(tn, name) == 0 ? "same" : "other");
  say("raw", raw(s) ? "ok" : strerror(errno));
  say("roundtrip", round_trip(m, s) ? "ok" : "failed");
  struct winsize w = {.ws_row = 33, .ws_col = 101};
  struct winsize r = {0};
  int sized = ioctl(m, TIOCSWINSZ, &w) == 0 && ioctl(s, TIOCGWINSZ, &r) == 0 &&
              r.ws_row == 33 && r.ws_col == 101;
  say("winsize", sized ? "ok" : strerror(errno));
  char c = 'x';
  say("tiocsti", ioctl(s, TIOCSTI, &c) == 0 ? "allowed" : "refused");
}

static void check_forkpty(void) {
  int m;
  pid_t pid = forkpty(&m, NULL, NULL, NULL);
  if (pid < 0) {
    say("forkpty", strerror(errno));
    return;
  }
  if (pid == 0) {
    const char *msg = isatty(0) && isatty(1) ? "ping tty\n" : "ping notty\n";
    if (write(1, msg, strlen(msg)) < 0)
      _exit(2);
    _exit(0);
  }
  char buf[64] = {0};
  size_t got = 0;
  while (got < sizeof buf - 1) {
    ssize_t n = read(m, buf + got, sizeof buf - 1 - got);
    if (n <= 0)
      break;
    got += (size_t)n;
    if (strchr(buf, '\n'))
      break;
  }
  int status = 0;
  waitpid(pid, &status, 0);
  say("forkpty", strstr(buf, "ping tty") ? "ok" : "failed");
}

static void check_posix(void) {
  int m = posix_openpt(O_RDWR | O_NOCTTY);
  if (m < 0) {
    say("posix_openpt", strerror(errno));
    return;
  }
  if (grantpt(m) < 0 || unlockpt(m) < 0) {
    say("posix_openpt", "grant failed");
    return;
  }
  const char *name = ptsname(m);
  if (!name) {
    say("posix_openpt", "no name");
    return;
  }
  int s = open(name, O_RDWR | O_NOCTTY);
  if (s < 0) {
    say("posix_openpt", strerror(errno));
    return;
  }
  raw(s);
  say("posix_openpt", round_trip(m, s) ? "ok" : "failed");
}

int main(int argc, char **argv) {
  out = fopen("probe.txt", "a");
  if (!out)
    return 1;
  const char *what = argc > 1 ? argv[1] : "openpty";
  if (strcmp(what, "openpty") == 0)
    check_openpty();
  else if (strcmp(what, "forkpty") == 0)
    check_forkpty();
  else if (strcmp(what, "posix") == 0)
    check_posix();
  else if (strcmp(what, "open") == 0 && argc > 2) {
    int fd = open(argv[2], O_RDWR | O_NOCTTY);
    say("open", fd >= 0 ? "allowed" : "refused");
  } else {
    say("usage", "probe openpty|forkpty|posix|open PATH");
  }
  fclose(out);
  return 0;
}
