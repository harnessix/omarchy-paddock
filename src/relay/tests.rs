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

#[cfg(target_os = "linux")] // the pty index and /proc are Linux only
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
    unsafe { (libc::close(master), libc::close(slave)) };
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
    let mut relay = Relay::start(
        caller_s,
        caller_s,
        move |data| {
            log_thread.lock().unwrap().write_all(data).unwrap();
        },
        None,
    )
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
