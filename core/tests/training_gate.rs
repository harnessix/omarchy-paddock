//! The training gate end to end: `paddock run` on a caller pty starts a
//! fake agy dialog (a shell script that draws the recorded screens and
//! reads keys) with the shipped agy rule. paddock unchecks the box and
//! presses [Done]. A fake harness and temporary folders. No real harness.

mod common;
#[path = "common/agy_screens.rs"]
mod screens;

use std::os::unix::io::FromRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn caller_pty(rows: u16, cols: u16) -> (i32, i32) {
    let (mut master, mut slave) = (0, 0);
    // SAFETY: openpty writes two new fds. The test closes them.
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(rc, 0, "{}", std::io::Error::last_os_error());
    let size = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: TIOCSWINSZ on the test pty.
    unsafe { libc::ioctl(master, libc::TIOCSWINSZ, &size) };
    (master, slave)
}

fn files_with(dir: &Path, needle: &str, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            files_with(&p, needle, out);
        } else if let Ok(text) = std::fs::read_to_string(&p) {
            out.extend(
                text.lines()
                    .filter(|l| l.contains(needle))
                    .map(str::to_string),
            );
        }
    }
}

#[test]
fn paddock_unchecks_the_training_box_of_a_fake_agy_dialog() {
    if !common::bwrap_works() {
        eprintln!("skipped: needs bwrap with a user namespace");
        return;
    }
    let tmp = common::tempdir();
    let root: PathBuf = tmp.path().to_path_buf();
    let project = root.join("project");
    let cfg = root.join("cfg/paddock/harness.d");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    let run = root.join("run");
    std::fs::create_dir_all(&run).unwrap();
    std::fs::set_permissions(&run, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    screens::write_screens(&project);
    let args = serde_json::to_string(&[
        "-c",
        screens::FAKE_TUI,
        "sh",
        project.to_str().unwrap(),
        "done",
    ])
    .unwrap();
    std::fs::write(
        cfg.join("fakeagy.toml"),
        format!(
            "name = \"fakeagy\"\n[locate]\ncommands = [\"sh\"]\n[launch]\nargs = {args}\n\
             [privacy.training]\nsetting = \"s\"\nurl = \"https://example.com\"\nrules = [\"agy-1.4-data-use\"]\n"
        ),
    )
    .unwrap();
    std::fs::write(
        root.join("cfg/paddock/config.toml"),
        "[view]\ntmux = false\n",
    )
    .unwrap();

    let (master, slave) = caller_pty(40, 100);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_paddock"));
    cmd.args([
        "run",
        "fakeagy",
        "--backend",
        "bwrap",
        "--network",
        "none",
        "--env",
        "clean",
        "--gui",
        "off",
        "-y",
    ])
    .arg(&project)
    .env("XDG_CONFIG_HOME", root.join("cfg"))
    .env("XDG_DATA_HOME", root.join("data"))
    .env("XDG_STATE_HOME", root.join("state"))
    .env("XDG_CACHE_HOME", root.join("cache"))
    .env("XDG_RUNTIME_DIR", &run)
    .env_remove("TMUX")
    .current_dir(&project);
    // SAFETY: each dup is a new owned fd, and Stdio takes it.
    unsafe {
        cmd.stdin(Stdio::from_raw_fd(libc::dup(slave)))
            .stdout(Stdio::from_raw_fd(libc::dup(slave)))
            .stderr(Stdio::null());
    }
    let mut child = cmd.spawn().unwrap();
    // SAFETY: the child holds the dups. This closes the test's extra slave.
    unsafe { libc::close(slave) };
    let start = Instant::now();
    let mut buf = [0u8; 4096];
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if start.elapsed() > Duration::from_secs(60) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("paddock did not end");
        }
        let mut pfd = libc::pollfd {
            fd: master,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll and read on the caller pty, as a terminal would.
        unsafe {
            if libc::poll(&mut pfd, 1, 50) > 0 {
                libc::read(master, buf.as_mut_ptr().cast(), buf.len());
            }
        }
    };
    // SAFETY: close the caller pty master.
    unsafe { libc::close(master) };
    let read = |n: &str| std::fs::read_to_string(project.join(n)).unwrap_or_default();
    assert!(status.success(), "{status:?} {}", read("other"));
    assert_eq!(read("result").trim(), "box=o");
    assert_eq!(read("size").trim(), "40 49");
    assert_eq!(read("other"), "");
    // One audit line with the rule, the keys and the outcome, and no row
    // of the screen.
    let mut lines = Vec::new();
    for d in ["state", "data", "cache", "run"] {
        files_with(&root.join(d), "\"capability\":\"training\"", &mut lines);
    }
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(
        lines[0].contains("training rule agy-1.4-data-use: keys enter,down,enter: declined"),
        "{}",
        lines[0]
    );
    assert!(!lines[0].contains("Yes, I agree"), "{}", lines[0]);
}
