//! An invisible terminal.
//!
//! A caller can hand paddock a terminal that no window shows, for example a
//! pty from a background process. The harness still needs a terminal. paddock
//! opens a pty, gives the slave to the sandbox, and copies bytes both ways.
//! The harness output also goes to the view log.

use anyhow::{Context, Result};
use std::io;
use std::os::fd::{FromRawFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

/// One process in the chain from paddock up to init.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ancestor {
    pub pid: u32,
    pub comm: String,
}

/// A Hyprland window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HyprClient {
    pub pid: u32,
    pub address: String,
}

/// `hyprctl clients -j`. None when `hyprctl` is missing or its answer is not
/// a window list. An empty list means Hyprland is there and has no windows.
pub fn hypr_clients() -> Option<Vec<HyprClient>> {
    let out = std::process::Command::new("hyprctl")
        .args(["clients", "-j"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let clients: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).ok()?;
    Some(
        clients
            .iter()
            .filter_map(|c| {
                Some(HyprClient {
                    pid: u32::try_from(c.get("pid")?.as_u64()?).ok()?,
                    address: c.get("address")?.as_str()?.to_string(),
                })
            })
            .collect(),
    )
}

/// `pid` and then each parent. The parent comes from `/proc/PID/stat`.
/// The name comes from `/proc/PID/comm`. Stops at pid 1 or after 32 steps.
pub fn parent_chain(pid: u32) -> Vec<Ancestor> {
    let mut out = Vec::new();
    let mut cur = pid;
    for _ in 0..32 {
        if cur <= 1 {
            break;
        }
        out.push(Ancestor {
            pid: cur,
            comm: comm_of(cur),
        });
        match crate::state::parent_pid(cur) {
            Some(next) if next > 1 && next != cur => cur = next,
            _ => break,
        }
    }
    out
}

fn comm_of(pid: u32) -> String {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// True when a terminal run is watched by a person.
///
/// `windows` is None when there is no Hyprland window list. Any terminal
/// then counts as watched, which is the rule outside Omarchy.
///
/// `master` is the process that holds the other end of this terminal (the
/// pty master): a terminal emulator, a tmux server, or a background program
/// that gave the command a pty of its own. The terminal is visible when that
/// process owns a compositor window itself, or is a tmux server or `screen`.
/// Its parents do not count: a desktop app with a window can run a
/// background server that hands out ptys nobody sees (for example the Codex
/// app-server of the ChatGPT desktop app).
///
/// When the holder cannot be found, the parent chain decides as a fallback:
/// a pid in the chain that owns a window, or a tmux server or `screen`.
pub fn terminal_watched(
    windows: Option<&[u32]>,
    master: Option<&Ancestor>,
    chain: &[Ancestor],
) -> bool {
    let Some(windows) = windows else {
        return true;
    };
    match master {
        Some(m) => windows.contains(&m.pid) || session_client(&m.comm),
        None => chain
            .iter()
            .any(|a| windows.contains(&a.pid) || session_client(&a.comm)),
    }
}

/// The tmux server and GNU screen have no window of their own. A chain
/// through either one is a session the user attached to.
fn session_client(comm: &str) -> bool {
    let name = comm.trim();
    name == "tmux: server" || name == "screen"
}

/// The pts number of this process's terminal (stdin), if it is a pty.
fn own_pts_index() -> Option<u32> {
    let link = std::fs::read_link("/proc/self/fd/0").ok()?;
    link.to_str()?.strip_prefix("/dev/pts/")?.parse().ok()
}

/// The process that holds the master side of pts `index`: the one whose
/// open `/dev/ptmx` reports that index in its fdinfo (`tty-index`). Only
/// processes of this user are readable, which is where the holder is.
fn pty_master_holder(index: u32) -> Option<Ancestor> {
    let want = format!("tty-index:\t{index}");
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
            continue;
        };
        for fd in fds.flatten() {
            let Ok(target) = std::fs::read_link(fd.path()) else {
                continue;
            };
            if target != std::path::Path::new("/dev/ptmx")
                && target != std::path::Path::new("/dev/pts/ptmx")
            {
                continue;
            }
            let info = std::fs::read_to_string(format!(
                "/proc/{pid}/fdinfo/{}",
                fd.file_name().to_string_lossy()
            ))
            .unwrap_or_default();
            if info.lines().any(|l| l.trim_end() == want) {
                return Some(Ancestor {
                    pid,
                    comm: comm_of(pid),
                });
            }
        }
    }
    None
}

/// The decision for this process: the Hyprland window list, the holder of
/// its pty master, and the parent chain as a fallback.
pub fn this_terminal_is_watched() -> bool {
    let windows = hypr_clients().map(|c| c.into_iter().map(|w| w.pid).collect::<Vec<_>>());
    let master = own_pts_index().and_then(pty_master_holder);
    let chain = parent_chain(std::process::id());
    terminal_watched(windows.as_deref(), master.as_ref(), &chain)
}

static WINCH: AtomicBool = AtomicBool::new(false);

extern "C" fn on_winch(_: libc::c_int) {
    WINCH.store(true, Ordering::Relaxed);
}

fn take_winch() -> bool {
    WINCH.swap(false, Ordering::Relaxed)
}

/// Make the slave on fd 0 the controlling terminal of this process.
/// Called from `pre_exec`, after fork and before exec.
pub fn take_controlling_tty() -> io::Result<()> {
    // SAFETY: setsid and ioctl are async-signal-safe, and fd 0 is the slave.
    unsafe {
        if libc::setsid() < 0 {
            return Err(io::Error::last_os_error());
        }
        if libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// The copy between the caller's terminal and the harness pty.
pub struct Relay {
    caller_in: RawFd,
    master: RawFd,
    slave: RawFd,
    saved: Option<libc::termios>,
    old_winch: Option<libc::sigaction>,
    wake_w: RawFd,
    stop: Arc<AtomicBool>,
    input: Option<JoinHandle<()>>,
    output: Option<JoinHandle<()>>,
    closed_slave: bool,
}

impl Relay {
    /// `caller_in` and `caller_out` are the terminal the caller gave paddock.
    /// They are often the same pty. `log` receives every byte the harness writes.
    pub fn start(
        caller_in: RawFd,
        caller_out: RawFd,
        log: impl Fn(&[u8]) + Send + 'static,
    ) -> Result<Relay> {
        let (master, slave) = open_pty().context("cannot open a terminal for the harness")?;
        let _ = cloexec(master);
        let _ = cloexec(slave);
        copy_winsize(caller_in, master);
        let saved = enter_raw(caller_in).context("cannot set the caller terminal to raw mode")?;
        let old_winch = install_winch();
        let (wake_r, wake_w) = pipe().context("cannot open the relay wake pipe")?;
        let _ = cloexec(wake_r);
        let _ = cloexec(wake_w);
        let stop = Arc::new(AtomicBool::new(false));
        let master_out = dup_fd(master).context("cannot dup the harness terminal")?;
        let _ = cloexec(master_out);
        // dup shares the file status flags, so this makes `master` nonblocking too.
        let _ = set_nonblock(master_out);
        let wake_out = dup_fd(wake_r).context("cannot dup the relay wake pipe")?;
        let _ = cloexec(wake_out);

        let stop_in = stop.clone();
        let input = std::thread::Builder::new()
            .name("paddock-relay-in".into())
            .spawn(move || copy_in(caller_in, master, wake_r, stop_in))
            .context("cannot start the relay")?;
        let stop_out = stop.clone();
        let output = std::thread::Builder::new()
            .name("paddock-relay-out".into())
            .spawn(move || copy_out(master_out, caller_out, wake_out, stop_out, log))
            .context("cannot start the relay")?;

        Ok(Relay {
            caller_in,
            master,
            slave,
            saved: Some(saved),
            old_winch: Some(old_winch),
            wake_w,
            stop,
            input: Some(input),
            output: Some(output),
            closed_slave: false,
        })
    }

    /// stdin, stdout and stderr for the sandbox. Each one is a dup of the slave.
    pub fn child_stdio(
        &self,
    ) -> Result<(
        std::process::Stdio,
        std::process::Stdio,
        std::process::Stdio,
    )> {
        Ok((
            owned_stdio(self.slave)?,
            owned_stdio(self.slave)?,
            owned_stdio(self.slave)?,
        ))
    }

    /// The parent no longer holds the slave. The child keeps its dups.
    pub fn close_slave(&mut self) {
        if self.closed_slave {
            return;
        }
        self.closed_slave = true;
        // SAFETY: `slave` is an open fd owned by this relay.
        unsafe { libc::close(self.slave) };
    }

    /// Stop the copy and put the caller terminal back.
    pub fn finish(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if self.wake_w >= 0 {
            let b = [1u8];
            // SAFETY: `wake_w` is the write end of the wake pipe, or already closed.
            unsafe {
                libc::write(self.wake_w, b.as_ptr().cast(), 1);
            }
        }
        if let Some(t) = self.input.take() {
            let _ = t.join();
        }
        if let Some(t) = self.output.take() {
            let _ = t.join();
        }
        if !self.closed_slave {
            self.closed_slave = true;
            // SAFETY: `slave` is an open fd owned by this relay.
            unsafe { libc::close(self.slave) };
        }
        if self.master >= 0 {
            // SAFETY: `master` is an open fd owned by this relay.
            unsafe { libc::close(self.master) };
            self.master = -1;
        }
        if self.wake_w >= 0 {
            // SAFETY: `wake_w` is an open fd owned by this relay.
            unsafe { libc::close(self.wake_w) };
            self.wake_w = -1;
        }
        if let Some(saved) = self.saved.take() {
            // SAFETY: `caller_in` is the caller terminal, still open.
            unsafe {
                libc::tcsetattr(self.caller_in, libc::TCSANOW, &saved);
            }
        }
        if let Some(old) = self.old_winch.take() {
            // SAFETY: restores the handler this relay installed.
            unsafe {
                libc::sigaction(libc::SIGWINCH, &old, std::ptr::null_mut());
            }
        }
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn open_pty() -> io::Result<(RawFd, RawFd)> {
    let mut master = 0;
    let mut slave = 0;
    // SAFETY: openpty writes two new fds to these locals.
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((master, slave))
}

fn pipe() -> io::Result<(RawFd, RawFd)> {
    let mut fds = [0; 2];
    // SAFETY: pipe writes two new fds.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((fds[0], fds[1]))
}

fn dup_fd(fd: RawFd) -> io::Result<RawFd> {
    // SAFETY: dup of an open fd returns a new fd or -1.
    let d = unsafe { libc::dup(fd) };
    if d < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(d)
}

fn owned_stdio(fd: RawFd) -> io::Result<std::process::Stdio> {
    let d = dup_fd(fd)?;
    // SAFETY: `d` is a new fd this process owns, and Stdio takes that ownership.
    Ok(unsafe { std::process::Stdio::from_raw_fd(d) })
}

fn cloexec(fd: RawFd) -> io::Result<()> {
    // SAFETY: fcntl on an open fd of this process.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        if libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn set_nonblock(fd: RawFd) -> io::Result<()> {
    // SAFETY: fcntl on an open fd of this process.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        if libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn enter_raw(fd: RawFd) -> io::Result<libc::termios> {
    // SAFETY: tcgetattr and tcsetattr on the caller terminal.
    unsafe {
        let mut saved: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut saved) < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut raw = saved;
        libc::cfmakeraw(&mut raw);
        if libc::tcsetattr(fd, libc::TCSANOW, &raw) < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(saved)
    }
}

fn install_winch() -> libc::sigaction {
    // SAFETY: installs a handler that only stores an atomic flag.
    unsafe {
        let mut old: libc::sigaction = std::mem::zeroed();
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = on_winch as *const () as libc::sighandler_t;
        action.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(libc::SIGWINCH, &action, &mut old);
        old
    }
}

fn copy_winsize(from: RawFd, to: RawFd) {
    // SAFETY: TIOCGWINSZ and TIOCSWINSZ on the two terminals of this relay.
    unsafe {
        let mut size: libc::winsize = std::mem::zeroed();
        if libc::ioctl(from, libc::TIOCGWINSZ, &mut size) < 0 {
            return;
        }
        libc::ioctl(to, libc::TIOCSWINSZ, &size);
    }
}

fn read_fd(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    // SAFETY: read into `buf` from an open fd.
    let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(n as usize)
}

fn write_all_fd(fd: RawFd, mut data: &[u8]) -> io::Result<()> {
    while !data.is_empty() {
        // SAFETY: write from `data` to an open fd.
        let n = unsafe { libc::write(fd, data.as_ptr().cast(), data.len()) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            if e.kind() == io::ErrorKind::WouldBlock {
                let mut pfd = libc::pollfd {
                    fd,
                    events: libc::POLLOUT,
                    revents: 0,
                };
                // SAFETY: poll until the pty accepts more bytes.
                unsafe { libc::poll(&mut pfd, 1, 50) };
                continue;
            }
            return Err(e);
        }
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "terminal write wrote nothing",
            ));
        }
        data = &data[n as usize..];
    }
    Ok(())
}

fn copy_in(caller: RawFd, master: RawFd, wake: RawFd, stop: Arc<AtomicBool>) {
    let mut buf = [0u8; 8192];
    loop {
        if take_winch() {
            copy_winsize(caller, master);
        }
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let mut fds = [
            libc::pollfd {
                fd: caller,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: wake,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: poll on the caller terminal and the wake pipe.
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), 2, 50) };
        if rc < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        // A size change lands before the next input byte, so the harness
        // sees the new size when it reads that byte.
        if take_winch() {
            copy_winsize(caller, master);
        }
        if stop.load(Ordering::Relaxed) || fds[1].revents != 0 {
            break;
        }
        if fds[0].revents & libc::POLLIN == 0 {
            continue;
        }
        match read_fd(caller, &mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if write_all_fd(master, &buf[..n]).is_err() {
                    break;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    // SAFETY: this thread owns the read end.
    unsafe { libc::close(wake) };
}

fn copy_out(
    master: RawFd,
    caller_out: RawFd,
    wake: RawFd,
    stop: Arc<AtomicBool>,
    log: impl Fn(&[u8]),
) {
    let mut buf = [0u8; 8192];
    let mut caller_ok = true;
    loop {
        if stop.load(Ordering::Relaxed) {
            drain_out(master, caller_out, &mut caller_ok, &log, &mut buf);
            break;
        }
        let mut fds = [
            libc::pollfd {
                fd: master,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: wake,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: poll on the harness pty and the wake pipe.
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
        if rc < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if stop.load(Ordering::Relaxed) || fds[1].revents != 0 {
            drain_out(master, caller_out, &mut caller_ok, &log, &mut buf);
            break;
        }
        if fds[0].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) == 0 {
            continue;
        }
        match forward_out(master, caller_out, &mut caller_ok, &log, &mut buf) {
            Forward::Data | Forward::Idle => {}
            Forward::End => break,
        }
    }
    // SAFETY: this thread owns both fds.
    unsafe {
        libc::close(master);
        libc::close(wake);
    }
}

enum Forward {
    Data,
    Idle,
    End,
}

/// Read what is already buffered.
fn forward_out(
    master: RawFd,
    caller_out: RawFd,
    caller_ok: &mut bool,
    log: &impl Fn(&[u8]),
    buf: &mut [u8],
) -> Forward {
    match read_fd(master, buf) {
        Ok(0) => Forward::End,
        Ok(n) => {
            let data = &buf[..n];
            if *caller_ok && write_all_fd(caller_out, data).is_err() {
                *caller_ok = false;
            }
            log(data);
            Forward::Data
        }
        Err(e) if e.kind() == io::ErrorKind::Interrupted => Forward::Idle,
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => Forward::Idle,
        // The slave is gone. A pty master then returns EIO.
        Err(e) if e.raw_os_error() == Some(libc::EIO) => Forward::End,
        Err(_) => Forward::End,
    }
}

fn drain_out(
    master: RawFd,
    caller_out: RawFd,
    caller_ok: &mut bool,
    log: &impl Fn(&[u8]),
    buf: &mut [u8],
) {
    while let Forward::Data = forward_out(master, caller_out, caller_ok, log, buf) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::process::CommandExt;
    use std::process::Command;
    use std::time::{Duration, Instant};

    fn anc(pid: u32, comm: &str) -> Ancestor {
        Ancestor {
            pid,
            comm: comm.to_string(),
        }
    }

    #[test]
    fn a_window_pid_in_the_chain_is_visible() {
        let chain = [anc(10, "sh"), anc(20, "foot"), anc(30, "Hyprland")];
        assert!(terminal_watched(Some(&[20, 99]), None, &chain));
    }

    #[test]
    fn a_chain_without_a_window_is_not_visible() {
        let chain = [anc(10, "codex"), anc(11, "chatgpt")];
        assert!(!terminal_watched(Some(&[50]), None, &chain));
    }

    #[test]
    fn an_empty_window_list_hides_a_plain_chain() {
        let chain = [anc(10, "codex"), anc(11, "chatgpt")];
        assert!(!terminal_watched(Some(&[]), None, &chain));
    }

    #[test]
    fn without_a_window_list_any_terminal_is_watched() {
        let chain = [anc(10, "codex")];
        assert!(terminal_watched(None, None, &chain));
    }

    #[test]
    fn a_background_server_under_an_app_window_is_not_visible() {
        // The ChatGPT desktop app (pid 30, a window) runs the Codex
        // app-server (pid 20), which holds the pty of paddock (pid 10).
        let chain = [anc(10, "paddock"), anc(20, "codex"), anc(30, "ChatGPT")];
        let server = anc(20, "codex");
        assert!(!terminal_watched(Some(&[30]), Some(&server), &chain));
        // A terminal emulator holds the pty and owns its window.
        let alacritty = anc(40, "alacritty");
        assert!(terminal_watched(Some(&[40]), Some(&alacritty), &chain));
        // A tmux server holds the pty of its panes.
        let tmux = anc(50, "tmux: server");
        assert!(terminal_watched(Some(&[]), Some(&tmux), &chain));
    }

    #[test]
    fn the_holder_of_a_real_pty_is_found() {
        let (master, slave) = super::open_pty().unwrap();
        let name = std::fs::read_link(format!("/proc/self/fd/{slave}")).unwrap();
        let index: u32 = name
            .to_str()
            .unwrap()
            .strip_prefix("/dev/pts/")
            .unwrap()
            .parse()
            .unwrap();
        let holder = super::pty_master_holder(index).expect("holder");
        assert_eq!(holder.pid, std::process::id());
        // SAFETY: closing the two descriptors this test opened.
        unsafe {
            libc::close(master);
            libc::close(slave);
        }
    }

    #[test]
    fn a_tmux_server_in_the_chain_is_visible() {
        let chain = [anc(10, "sh"), anc(11, "tmux: server")];
        assert!(terminal_watched(Some(&[]), None, &chain));
    }

    #[test]
    fn screen_in_the_chain_is_visible() {
        let chain = [anc(10, "bash"), anc(12, "screen")];
        assert!(terminal_watched(Some(&[99]), None, &chain));
    }

    #[test]
    fn the_parent_chain_starts_at_this_process() {
        let chain = parent_chain(std::process::id());
        assert_eq!(chain.first().map(|a| a.pid), Some(std::process::id()));
    }

    fn caller_pty() -> (RawFd, RawFd) {
        let mut master = 0;
        let mut slave = 0;
        // SAFETY: openpty writes two new fds for the test caller.
        let rc = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(rc, 0, "openpty: {}", io::Error::last_os_error());
        (master, slave)
    }

    fn set_size(fd: RawFd, rows: u16, cols: u16) {
        let size = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: TIOCSWINSZ on the test pty.
        let rc = unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &size) };
        assert_eq!(rc, 0, "winsize: {}", io::Error::last_os_error());
    }

    fn read_until(fd: RawFd, needle: &[u8], timeout: Duration) -> Vec<u8> {
        let mut got = Vec::new();
        let mut buf = [0u8; 1024];
        let start = Instant::now();
        while Instant::now().duration_since(start) < timeout {
            if got.windows(needle.len()).any(|w| w == needle) {
                break;
            }
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: poll on the test caller pty.
            let rc = unsafe { libc::poll(&mut pfd, 1, 50) };
            if rc < 0 {
                continue;
            }
            if rc == 0 {
                continue;
            }
            match read_fd(fd, &mut buf) {
                Ok(0) => break,
                Ok(n) => got.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                Err(_) => break,
            }
        }
        got
    }

    #[test]
    fn the_relay_copies_bytes_size_exit_code_and_the_log() {
        let (caller_m, caller_s) = caller_pty();
        set_size(caller_m, 24, 80);
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("view.log");
        let log = Arc::new(std::sync::Mutex::new(
            std::fs::File::create(&log_path).unwrap(),
        ));
        let log_thread = log.clone();
        let mut relay = Relay::start(caller_s, caller_s, move |data| {
            log_thread.lock().unwrap().write_all(data).unwrap();
        })
        .unwrap();
        let script = "trap '' WINCH; stty -echo -onlcr; printf 'byte-exact\\n'; \
                      IFS= read -r line; printf 'got:%s\\n' \"$line\"; \
                      printf 'size:%s\\n' \"$(stty size)\"; exit 9";
        let (stdin, stdout, stderr) = relay.child_stdio().unwrap();
        let mut cmd = Command::new("sh");
        cmd.args(["-c", script])
            .stdin(stdin)
            .stdout(stdout)
            .stderr(stderr);
        // SAFETY: take_controlling_tty uses setsid and ioctl, which are safe
        // in pre_exec. The slave is fd 0.
        unsafe {
            cmd.pre_exec(take_controlling_tty);
        }
        let mut child = cmd.spawn().unwrap();
        relay.close_slave();
        let head = read_until(caller_m, b"byte-exact\n", Duration::from_secs(5));
        assert!(
            head.windows(b"byte-exact\n".len())
                .any(|w| w == b"byte-exact\n"),
            "caller output: {}",
            String::from_utf8_lossy(&head)
        );
        set_size(caller_m, 30, 100);
        // SAFETY: raise SIGWINCH in this test process, which runs the relay.
        unsafe { libc::raise(libc::SIGWINCH) };
        assert!(write_all_fd(caller_m, b"hello\n").is_ok());
        let rest = read_until(caller_m, b"size:30 100\n", Duration::from_secs(5));
        let text = String::from_utf8_lossy(&rest);
        assert!(text.contains("got:hello\n"), "{text}");
        assert!(text.contains("size:30 100\n"), "{text}");
        let status = wait_child(&mut child, Duration::from_secs(5));
        assert_eq!(status.code(), Some(9));
        relay.finish();
        let mut logged = String::new();
        std::fs::File::open(&log_path)
            .unwrap()
            .read_to_string(&mut logged)
            .unwrap();
        assert!(logged.contains("byte-exact\n"), "{logged}");
        assert!(logged.contains("got:hello\n"), "{logged}");
        assert!(logged.contains("size:30 100\n"), "{logged}");
        // SAFETY: close the test caller pty.
        unsafe {
            libc::close(caller_m);
            libc::close(caller_s);
        }
    }

    fn wait_child(child: &mut std::process::Child, timeout: Duration) -> std::process::ExitStatus {
        let start = Instant::now();
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                return status;
            }
            if Instant::now().duration_since(start) > timeout {
                let _ = child.kill();
                panic!("the harness did not exit");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
