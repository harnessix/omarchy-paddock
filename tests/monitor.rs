//! The monitor watch end to end: a bwrap sandbox runs a copy of `sleep`
//! named `ngrok`, and paddock names it in the run folder and in
//! `paddock ps --json`, next to the traffic count. Needs a systemd user instance and the session bus
//! (for the scope); `[monitor] notify = false` keeps the desktop quiet.
//! Temporary folders for everything else.

mod common;

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn can_run() -> bool {
    let bwrap = Command::new("bwrap")
        .args(["--unshare-user", "--ro-bind", "/", "/", "true"])
        .status()
        .is_ok_and(|s| s.success());
    let systemd = Command::new("systemctl")
        .args(["--user", "is-system-running"])
        .output()
        .is_ok_and(|o| {
            matches!(
                String::from_utf8_lossy(&o.stdout).trim(),
                "running" | "degraded"
            )
        });
    bwrap
        && systemd
        && std::env::var("DBUS_SESSION_BUS_ADDRESS").is_ok()
        && std::path::Path::new("/sys/fs/cgroup/cgroup.procs").exists()
}

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
        std::fs::create_dir_all(root.join("home")).unwrap();
        let run = root.join("run");
        std::fs::create_dir_all(&run).unwrap();
        std::fs::set_permissions(&run, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .unwrap();
        let script = "cp \"$(command -v sleep)\" ./ngrok && ./ngrok 20; echo done > done";
        std::fs::write(
            cfg.join("tunnel.toml"),
            format!(
                "name = \"tunnel\"\n[locate]\ncommands = [\"sh\"]\n[launch]\nargs = [\"-c\", {}]\n",
                serde_json::to_string(script).unwrap()
            ),
        )
        .unwrap();
        std::fs::write(
            root.join("cfg/paddock/config.toml"),
            "[monitor]\nnotify = false\n",
        )
        .unwrap();
        World { _tmp: tmp, root }
    }

    fn paddock(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_paddock"));
        c.env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("cfg"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("XDG_RUNTIME_DIR", self.root.join("run"))
            .current_dir(self.root.join("project"));
        c
    }

    fn ps(&self) -> Vec<serde_json::Value> {
        let out = self.paddock().args(["ps", "--json"]).output().unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
        v["sandboxes"].as_array().cloned().unwrap_or_default()
    }
}

fn wait_for<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let start = Instant::now();
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "timed out: {what}"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[test]
fn a_tunnel_tool_in_a_sandbox_is_reported() {
    if !can_run() {
        eprintln!("skipped: needs bwrap, a systemd user instance, the session bus and cgroup v2");
        return;
    }
    let w = World::new();
    let mut child = w
        .paddock()
        .args([
            "run",
            "tunnel",
            "--backend",
            "bwrap",
            "--network",
            "none",
            "--env",
            "clean",
            "-y",
        ])
        .arg(w.root.join("project"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let sb = wait_for("the sandbox", || w.ps().into_iter().next());
    let id = sb["id"].as_str().unwrap().to_string();
    if !sb["unit"]
        .as_str()
        .is_some_and(|u| u.starts_with("paddock-sb-"))
    {
        let _ = w.paddock().args(["stop", &id]).output();
        let _ = child.wait();
        eprintln!("skipped: the sandbox got no scope");
        return;
    }
    let file = w.root.join("run/paddock").join(&id).join("monitor.json");
    // The traffic count writes the file too, so wait for the tunnel.
    let v = wait_for("monitor.json", || {
        let text = std::fs::read_to_string(&file).ok()?;
        let v: serde_json::Value = serde_json::from_str(&text).ok()?;
        v["tunnels"]
            .as_array()
            .is_some_and(|a| !a.is_empty())
            .then_some(v)
    });
    assert_eq!(v["tunnels"], serde_json::json!(["ngrok"]), "{v}");
    let sb = w.ps().into_iter().next().unwrap();
    assert_eq!(sb["tunnels"], serde_json::json!(["ngrok"]), "{sb}");
    // With --network none the sandbox has `lo` only: the count stays zero.
    assert!(sb["traffic"]["sent"].is_u64(), "{sb}");
    assert!(sb["traffic"]["received"].is_u64(), "{sb}");

    let _ = w.paddock().args(["stop", &id]).output();
    wait_for("paddock to exit", || child.try_wait().unwrap());
}
