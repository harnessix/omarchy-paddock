//! An invisible terminal end to end: paddock's stdin and stdout are a pty,
//! Hyprland reports no windows, and `paddock ps --json` says the run has a
//! view, a relay and activity. A fake harness and temporary folders. No real
//! harness and no real Hyprland.

mod common;

use std::os::unix::io::FromRawFd;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const SCRIPT: &str = "printf 'relay-marker\\n'; sleep 30; exit 0";

struct World {
    _tmp: tempfile::TempDir,
    root: PathBuf,
}

impl World {
    fn new() -> Self {
        let tmp = common::tempdir();
        let root = tmp.path().to_path_buf();
        let cfg = root.join("cfg/paddock/harness.d");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::create_dir_all(root.join("project")).unwrap();
        let run = root.join("run");
        std::fs::create_dir_all(&run).unwrap();
        std::fs::set_permissions(&run, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .unwrap();
        std::fs::write(
            cfg.join("fake.toml"),
            format!(
                "name = \"fake\"\n[locate]\ncommands = [\"sh\"]\n[launch]\nargs = [\"-c\", {}]\n",
                serde_json::to_string(SCRIPT).unwrap()
            ),
        )
        .unwrap();
        std::fs::write(
            root.join("cfg/paddock/config.toml"),
            "[view]\ntmux = false\n",
        )
        .unwrap();
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        // Hyprland is present and has no windows, so a plain parent chain
        // is not a visible terminal.
        std::fs::write(bin.join("hyprctl"), "#!/bin/sh\nprintf '%s\\n' '[]'\n").unwrap();
        std::fs::set_permissions(
            bin.join("hyprctl"),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        World { _tmp: tmp, root }
    }

    fn paddock(&self) -> Command {
        let path = std::env::var("PATH").unwrap_or_default();
        let mut c = Command::new(env!("CARGO_BIN_EXE_paddock"));
        c.env(
            "PATH",
            format!("{}:{}", self.root.join("bin").display(), path),
        )
        .env("XDG_CONFIG_HOME", self.root.join("cfg"))
        .env("XDG_DATA_HOME", self.root.join("data"))
        .env("XDG_STATE_HOME", self.root.join("state"))
        .env("XDG_CACHE_HOME", self.root.join("cache"))
        .env("XDG_RUNTIME_DIR", self.root.join("run"))
        .env_remove("TMUX")
        .current_dir(self.root.join("project"));
        c
    }
}

fn caller_pty() -> (i32, i32) {
    let mut master = 0;
    let mut slave = 0;
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
    (master, slave)
}

#[test]
fn ps_reports_view_relay_and_activity_for_an_invisible_terminal() {
    if !common::bwrap_works() {
        eprintln!("skipped: needs bwrap with a user namespace");
        return;
    }
    let w = World::new();
    let (master, slave) = caller_pty();
    let mut cmd = w.paddock();
    cmd.args([
        "run",
        "fake",
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
    .arg(w.root.join("project"));
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
    let sb = loop {
        let out = w.paddock().args(["ps", "--json"]).output().unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
        if let Some(sb) = v["sandboxes"].as_array().and_then(|a| a.first().cloned()) {
            if sb["activity"].as_str() == Some("relay-marker") {
                break sb;
            }
        }
        if start.elapsed() > Duration::from_secs(40) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("no relay view in paddock ps --json");
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(sb["view"], true, "{sb}");
    assert_eq!(sb["relay"], true, "{sb}");
    assert_eq!(sb["watched"], false, "{sb}");
    assert_eq!(sb["activity"], "relay-marker", "{sb}");

    let mut saw = Vec::new();
    let mut buf = [0u8; 256];
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline
        && !saw
            .windows(b"relay-marker".len())
            .any(|w| w == b"relay-marker")
    {
        let mut pfd = libc::pollfd {
            fd: master,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll on the caller pty.
        let rc = unsafe { libc::poll(&mut pfd, 1, 50) };
        if rc <= 0 {
            continue;
        }
        // SAFETY: read from the caller pty master.
        let n = unsafe { libc::read(master, buf.as_mut_ptr().cast(), buf.len()) };
        if n > 0 {
            saw.extend_from_slice(&buf[..n as usize]);
        }
    }
    let text = String::from_utf8_lossy(&saw);
    assert!(text.contains("relay-marker"), "{text}");

    let id = sb["id"].as_str().unwrap();
    assert!(w.paddock().args(["stop", id]).status().unwrap().success());
    let _ = child.wait();
    // SAFETY: close the caller pty master.
    unsafe { libc::close(master) };
}
