/*
 * paddock's pty library for Seatbelt terminal runs on macOS.
 *
 * The Seatbelt profile of a terminal run cannot open /dev/ptmx or a
 * /dev/ttysNNN path: Seatbelt cannot tell a new pty from the user's own
 * terminals by path. paddock loads this library into the harness with
 * DYLD_INSERT_LIBRARIES. It replaces openpty, forkpty, posix_openpt,
 * grantpt, unlockpt, ptsname, ptsname_r and open (for a pty name that it
 * gave out). Each new pty comes from paddock's broker on the host
 * (src/ptybroker.rs): the broker opens it and sends both file descriptors
 * back over the Unix socket in PADDOCK_PTY_SOCKET (SCM_RIGHTS).
 *
 * Without PADDOCK_PTY_SOCKET every function calls the real one. The same
 * file builds on Linux, where LD_PRELOAD loads it, so its tests run there.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdarg.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/uio.h>
#include <sys/un.h>
#include <termios.h>
#include <unistd.h>

#ifdef __APPLE__
#include <util.h>
/* macOS: the replacements have their own names, and dyld points the calls
 * of every other image at them (__DATA,__interpose). A call from inside this
 * library reaches the real function. */
#define REPLACE(name) paddock_##name
#define REAL(name) name
#define INTERPOSE(name)                                                      \
  __attribute__((used)) static struct {                                     \
    const void *replacement;                                                 \
    const void *original;                                                    \
  } interpose_##name __attribute__((section("__DATA,__interpose"))) = {     \
      (const void *)(unsigned long)&paddock_##name,                          \
      (const void *)(unsigned long)&name};
#define CONST_ARG
#else
#include <dlfcn.h>
#include <pty.h>
#include <stdlib.h>
/* Linux: the replacements carry the real names, and the real function is
 * the next one in the search order. */
#define REPLACE(name) name
#define REAL(name) ((__typeof__(&name))real_symbol(#name))
#define INTERPOSE(name)
#define CONST_ARG const
static void *real_symbol(const char *name) {
  void *f = dlsym(RTLD_NEXT, name);
  if (!f)
    abort();
  return f;
}
#endif

#define SOCKET_VAR "PADDOCK_PTY_SOCKET"
#define NAME_MAX_LEN 128

/* No broker in this process: the caller uses the real function. */
#define NO_BROKER (-2)

/* Ask the broker for a new pty. 0 and both fds on success, -1 with errno
 * on a refusal, NO_BROKER without PADDOCK_PTY_SOCKET. */
/* The names of the slaves that paddock gave out, by device number.
 * ttyname looks for the name under /dev, which the profile denies, so it
 * answers from here. A device number also finds a slave that the program
 * moved to another fd, such as fd 0 in a forkpty child. */
struct named {
  dev_t rdev;
  char name[NAME_MAX_LEN];
};
#define NAMED 64
static struct named named[NAMED];
static int named_next;
static pthread_mutex_t named_lock = PTHREAD_MUTEX_INITIALIZER;

static void named_add(int fd, const char *name) {
  struct stat st;
  if (fstat(fd, &st) != 0)
    return;
  pthread_mutex_lock(&named_lock);
  int slot = -1;
  for (int i = 0; i < NAMED; i++)
    if (named[i].name[0] && named[i].rdev == st.st_rdev)
      slot = i;
  if (slot < 0) {
    slot = named_next;
    named_next = (named_next + 1) % NAMED;
  }
  named[slot].rdev = st.st_rdev;
  strncpy(named[slot].name, name, NAME_MAX_LEN - 1);
  named[slot].name[NAME_MAX_LEN - 1] = 0;
  pthread_mutex_unlock(&named_lock);
}

/* Copy the name of fd's slave into buf. 0 when it is one of paddock's. */
static int named_get(int fd, char *buf, size_t len) {
  struct stat st;
  if (fstat(fd, &st) != 0 || !S_ISCHR(st.st_mode))
    return -1;
  int found = -1;
  pthread_mutex_lock(&named_lock);
  for (int i = 0; i < NAMED; i++) {
    if (named[i].name[0] && named[i].rdev == st.st_rdev &&
        strlen(named[i].name) < len) {
      strcpy(buf, named[i].name);
      found = 0;
      break;
    }
  }
  pthread_mutex_unlock(&named_lock);
  return found;
}

static int pty_request(int *master, int *slave, char name[NAME_MAX_LEN]) {
  const char *path = getenv(SOCKET_VAR);
  if (!path || !*path)
    return NO_BROKER;
  struct sockaddr_un addr;
  memset(&addr, 0, sizeof addr);
  addr.sun_family = AF_UNIX;
  if (strlen(path) >= sizeof addr.sun_path) {
    errno = ENAMETOOLONG;
    return -1;
  }
  strcpy(addr.sun_path, path);
  int fd = socket(AF_UNIX, SOCK_STREAM, 0);
  if (fd < 0)
    return -1;
  fcntl(fd, F_SETFD, FD_CLOEXEC);
  if (connect(fd, (struct sockaddr *)&addr, sizeof addr) < 0 ||
      write(fd, "pty\n", 4) != 4) {
    int e = errno;
    close(fd);
    errno = e;
    return -1;
  }
  char buf[256];
  struct iovec iov = {buf, sizeof buf - 1};
  union {
    struct cmsghdr h;
    char c[CMSG_SPACE(2 * sizeof(int))];
  } ctl;
  struct msghdr msg;
  memset(&msg, 0, sizeof msg);
  memset(&ctl, 0, sizeof ctl);
  msg.msg_iov = &iov;
  msg.msg_iovlen = 1;
  msg.msg_control = ctl.c;
  msg.msg_controllen = sizeof ctl.c;
  ssize_t n;
  do {
    n = recvmsg(fd, &msg, 0);
  } while (n < 0 && errno == EINTR);
  close(fd);
  if (n <= 0) {
    errno = EIO;
    return -1;
  }
  buf[n] = 0;
  int fds[2] = {-1, -1};
  for (struct cmsghdr *c = CMSG_FIRSTHDR(&msg); c; c = CMSG_NXTHDR(&msg, c)) {
    if (c->cmsg_level != SOL_SOCKET || c->cmsg_type != SCM_RIGHTS)
      continue;
    size_t count = (c->cmsg_len - CMSG_LEN(0)) / sizeof(int);
    int got[8];
    if (count > 8)
      count = 8;
    memcpy(got, CMSG_DATA(c), count * sizeof(int));
    for (size_t i = 0; i < count; i++) {
      if (i < 2 && fds[i] < 0)
        fds[i] = got[i];
      else
        close(got[i]);
    }
  }
  char *end = strchr(buf, '\n');
  if (end)
    *end = 0;
  if (strncmp(buf, "ok ", 3) != 0 || fds[0] < 0 || fds[1] < 0 ||
      strlen(buf + 3) >= NAME_MAX_LEN) {
    if (fds[0] >= 0)
      close(fds[0]);
    if (fds[1] >= 0)
      close(fds[1]);
    errno = strncmp(buf, "ok ", 3) == 0 ? EIO : EAGAIN;
    return -1;
  }
  strcpy(name, buf + 3);
  *master = fds[0];
  *slave = fds[1];
  named_add(fds[1], name);
  return 0;
}

/* Ptys from posix_openpt: the slave waits here until the program opens its
 * name (grantpt, unlockpt and ptsname come first). */
struct held {
  int master;
  int slave;
  char name[NAME_MAX_LEN];
};
#define HELD 16
static struct held held[HELD];
static int held_ready;
static pthread_mutex_t held_lock = PTHREAD_MUTEX_INITIALIZER;

static void held_init(void) {
  if (held_ready)
    return;
  for (int i = 0; i < HELD; i++) {
    held[i].master = -1;
    held[i].slave = -1;
  }
  held_ready = 1;
}

/* The entry of a master fd, or NULL. Call with held_lock. */
static struct held *held_find(int master) {
  held_init();
  for (int i = 0; i < HELD; i++)
    if (held[i].master == master && master >= 0)
      return &held[i];
  return NULL;
}

int REPLACE(openpty)(int *amaster, int *aslave, char *name,
                     CONST_ARG struct termios *termp,
                     CONST_ARG struct winsize *winp) {
  int m, s;
  char n[NAME_MAX_LEN];
  int r = pty_request(&m, &s, n);
  if (r == NO_BROKER)
    return REAL(openpty)(amaster, aslave, name, termp, winp);
  if (r < 0)
    return -1;
  if (termp)
    tcsetattr(s, TCSAFLUSH, termp);
  if (winp)
    ioctl(s, TIOCSWINSZ, winp);
  if (name)
    strcpy(name, n);
  *amaster = m;
  *aslave = s;
  return 0;
}
INTERPOSE(openpty)

pid_t REPLACE(forkpty)(int *amaster, char *name,
                       CONST_ARG struct termios *termp,
                       CONST_ARG struct winsize *winp) {
  if (!getenv(SOCKET_VAR) || !*getenv(SOCKET_VAR))
    return REAL(forkpty)(amaster, name, termp, winp);
  int m, s;
  /* libc's forkpty calls its own openpty, which the interposing does not
   * reach. So this is forkpty again, around the broker's pty. */
  if (REPLACE(openpty)(&m, &s, name, termp, winp) < 0)
    return -1;
  pid_t pid = fork();
  if (pid < 0) {
    int e = errno;
    close(m);
    close(s);
    errno = e;
    return -1;
  }
  if (pid == 0) {
    close(m);
    setsid();
    ioctl(s, TIOCSCTTY, 0);
    dup2(s, 0);
    dup2(s, 1);
    dup2(s, 2);
    if (s > 2)
      close(s);
    return 0;
  }
  close(s);
  *amaster = m;
  return pid;
}
INTERPOSE(forkpty)

int REPLACE(posix_openpt)(int flags) {
  int m, s;
  char n[NAME_MAX_LEN];
  int r = pty_request(&m, &s, n);
  if (r == NO_BROKER)
    return REAL(posix_openpt)(flags);
  if (r < 0)
    return -1;
  if (flags & O_CLOEXEC)
    fcntl(m, F_SETFD, FD_CLOEXEC);
  pthread_mutex_lock(&held_lock);
  struct held *h = held_find(m);
  for (int i = 0; !h && i < HELD; i++)
    if (held[i].master < 0 || held[i].slave < 0)
      h = &held[i];
  if (!h) {
    pthread_mutex_unlock(&held_lock);
    close(m);
    close(s);
    errno = EAGAIN;
    return -1;
  }
  if (h->slave >= 0 && h->master != m)
    close(h->slave);
  h->master = m;
  h->slave = s;
  strcpy(h->name, n);
  pthread_mutex_unlock(&held_lock);
  return m;
}
INTERPOSE(posix_openpt)

int REPLACE(grantpt)(int fd) {
  pthread_mutex_lock(&held_lock);
  int known = held_find(fd) != NULL;
  pthread_mutex_unlock(&held_lock);
  return known ? 0 : REAL(grantpt)(fd);
}
INTERPOSE(grantpt)

int REPLACE(unlockpt)(int fd) {
  pthread_mutex_lock(&held_lock);
  int known = held_find(fd) != NULL;
  pthread_mutex_unlock(&held_lock);
  return known ? 0 : REAL(unlockpt)(fd);
}
INTERPOSE(unlockpt)

char *REPLACE(ptsname)(int fd) {
  pthread_mutex_lock(&held_lock);
  struct held *h = held_find(fd);
  pthread_mutex_unlock(&held_lock);
  return h ? h->name : REAL(ptsname)(fd);
}
INTERPOSE(ptsname)

int REPLACE(ptsname_r)(int fd, char *buf, size_t len) {
  pthread_mutex_lock(&held_lock);
  struct held *h = held_find(fd);
  int r = -1;
  if (h) {
    if (strlen(h->name) < len) {
      strcpy(buf, h->name);
      r = 0;
    } else {
      errno = ERANGE;
    }
  }
  pthread_mutex_unlock(&held_lock);
  return h ? r : REAL(ptsname_r)(fd, buf, len);
}
INTERPOSE(ptsname_r)

char *REPLACE(ttyname)(int fd) {
  static __thread char buf[NAME_MAX_LEN];
  if (named_get(fd, buf, sizeof buf) == 0)
    return buf;
  return REAL(ttyname)(fd);
}
INTERPOSE(ttyname)

int REPLACE(ttyname_r)(int fd, char *buf, size_t len) {
  if (named_get(fd, buf, len) == 0)
    return 0;
  return REAL(ttyname_r)(fd, buf, len);
}
INTERPOSE(ttyname_r)

/* The slave of a pty from posix_openpt, once, when the program opens its
 * name. -1 when the name is not one of them. */
static int held_take(const char *path, int flags) {
  if (strncmp(path, "/dev/", 5) != 0)
    return -1;
  pthread_mutex_lock(&held_lock);
  held_init();
  int fd = -1;
  for (int i = 0; i < HELD; i++) {
    if (held[i].slave >= 0 && strcmp(held[i].name, path) == 0) {
      fd = held[i].slave;
      held[i].slave = -1;
      break;
    }
  }
  pthread_mutex_unlock(&held_lock);
  if (fd >= 0 && (flags & O_CLOEXEC))
    fcntl(fd, F_SETFD, FD_CLOEXEC);
  return fd;
}

int REPLACE(open)(const char *path, int flags, ...) {
  int mode = 0;
  if (flags & O_CREAT) {
    va_list ap;
    va_start(ap, flags);
    mode = va_arg(ap, int);
    va_end(ap);
  }
  int fd = path ? held_take(path, flags) : -1;
  return fd >= 0 ? fd : REAL(open)(path, flags, mode);
}
INTERPOSE(open)

#ifndef __APPLE__
int open64(const char *path, int flags, ...) {
  int mode = 0;
  if (flags & O_CREAT) {
    va_list ap;
    va_start(ap, flags);
    mode = va_arg(ap, int);
    va_end(ap);
  }
  int fd = path ? held_take(path, flags) : -1;
  return fd >= 0 ? fd : REAL(open64)(path, flags, mode);
}
#endif
