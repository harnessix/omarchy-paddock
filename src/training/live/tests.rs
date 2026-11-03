//! The training gate on a real pty, with the relay and a fake agy dialog
//! (a shell script). No sandbox and no real harness.

use super::*;
use crate::relay::Relay;
use std::os::unix::process::CommandExt;
use std::process::Command;

use crate::training::screens;

fn caller_pty(rows: u16, cols: u16) -> (RawFd, RawFd) {
    let (mut master, mut slave) = (0, 0);
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
    assert_eq!(rc, 0, "openpty: {}", std::io::Error::last_os_error());
    set_winsize(master, rows, cols);
    (master, slave)
}

/// Read the caller side, as a terminal would, so the relay never blocks.
fn drain(fd: RawFd, stop: Arc<AtomicBool>) -> JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut all = Vec::new();
        let mut buf = [0u8; 4096];
        while !stop.load(Ordering::SeqCst) {
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: poll and read on the test caller pty.
            unsafe {
                if libc::poll(&mut pfd, 1, 50) <= 0 {
                    continue;
                }
                let n = libc::read(fd, buf.as_mut_ptr().cast(), buf.len());
                if n <= 0 {
                    break;
                }
                all.extend_from_slice(&buf[..n as usize]);
            }
        }
        all
    })
}

struct Run {
    dir: tempfile::TempDir,
    code: Option<i32>,
    gate: Arc<Gate>,
    seen: Vec<u8>,
}

/// The fake dialog behind the relay and a gate with the shipped agy rule.
/// `user` writes into the caller terminal once the dialog shows.
fn run_fake(first: &str, user: Option<&[u8]>) -> Run {
    let dir = tempfile::tempdir().unwrap();
    screens::write_screens(dir.path());
    let (cm, cs) = caller_pty(40, 100);
    let rules = crate::profile::builtin("agy").unwrap().training_rules();
    let ended = Arc::new(AtomicBool::new(false));
    let e = ended.clone();
    let gate = Gate::new(rules, move |_| e.store(true, Ordering::SeqCst));
    let mut relay = Relay::start(cs, cs, |_| {}, Some(gate.clone())).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let reader = drain(cm, stop.clone());
    let (i, o, er) = relay.child_stdio().unwrap();
    let mut cmd = Command::new("sh");
    cmd.args(["-c", screens::FAKE_TUI, "sh"])
        .arg(dir.path())
        .arg(first)
        .stdin(i)
        .stdout(o)
        .stderr(er);
    // SAFETY: take_controlling_tty uses setsid and ioctl, which are safe in
    // pre_exec. The slave is fd 0.
    unsafe {
        cmd.pre_exec(crate::relay::take_controlling_tty);
    }
    let mut child = cmd.spawn().unwrap();
    relay.close_slave();
    if let Some(keys) = user {
        // The user presses a key once the dialog is on screen, before the
        // screen settles.
        let size = dir.path().join("size");
        let start = Instant::now();
        while !size.exists() && start.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(60));
        assert!(crate::relay::write_all_fd(cm, keys).is_ok());
    }
    let start = Instant::now();
    let code = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s.code();
        }
        if user.is_some() && ended.load(Ordering::SeqCst) {
            // The gate gave the screen back: the fake waits for a person.
            std::thread::sleep(Duration::from_millis(300));
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        if start.elapsed() > Duration::from_secs(30) {
            let _ = child.kill();
            panic!("the fake dialog did not end");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    relay.finish();
    stop.store(true, Ordering::SeqCst);
    let seen = reader.join().unwrap();
    // SAFETY: close the test caller pty.
    unsafe {
        libc::close(cm);
        libc::close(cs);
    }
    Run {
        dir,
        code,
        gate,
        seen,
    }
}

fn read(dir: &tempfile::TempDir, name: &str) -> String {
    std::fs::read_to_string(dir.path().join(name)).unwrap_or_default()
}

#[test]
fn the_gate_unchecks_the_box_and_presses_done_on_a_live_pty() {
    for first in ["done", "prev"] {
        let r = run_fake(first, None);
        assert_eq!(r.code, Some(0), "{first}: {}", read(&r.dir, "other"));
        assert_eq!(read(&r.dir, "result").trim(), "box=o", "{first}");
        // The harness got the rule width and the caller's rows.
        assert_eq!(read(&r.dir, "size").trim(), "40 49", "{first}");
        assert_eq!(read(&r.dir, "other"), "", "{first}");
        let (id, o) = r.gate.result().unwrap();
        assert_eq!(id.as_deref(), Some("agy-1.4-data-use"));
        assert_eq!(o, Outcome::Declined);
        // The caller saw the harness output, forwarded as it came.
        let seen = String::from_utf8_lossy(&r.seen);
        assert!(seen.contains("Terms of Service & Data Use"), "{seen}");
        assert!(seen.contains("Do you trust the contents"), "{seen}");
    }
}

#[test]
fn a_user_key_cancels_the_gate_before_it_reaches_the_harness() {
    // Down moves the focus to the buttons and leaves the box checked.
    let r = run_fake("done", Some(b"\x1b[B"));
    assert_eq!(read(&r.dir, "result"), "", "nothing was submitted");
    // Cancelled when the dialog was drawn, or ended when the key came
    // first. paddock sent no key in either case.
    let (_, o) = r.gate.result().unwrap();
    assert!(
        matches!(o, Outcome::Cancelled | Outcome::NotShown(_)),
        "{o:?}"
    );
}
