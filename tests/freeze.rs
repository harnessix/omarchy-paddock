//! `paddock freeze` and `paddock thaw` end to end: a bwrap sandbox runs in
//! a scope of its own, freezing stops it (a counter that it writes stays
//! the same), thawing lets it continue, `paddock ps` shows the freeze, and
//! paddock kv holds the state of the sandbox. Needs a systemd user
//! instance and the session bus; temporary folders for everything else.

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
    bwrap && systemd && std::env::var("DBUS_SESSION_BUS_ADDRESS").is_ok()
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
        let run = root.join("run");
        std::fs::create_dir_all(&run).unwrap();
        std::fs::set_permissions(&run, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .unwrap();
        let script = "i=0; while :; do i=$((i+1)); echo $i > count; sleep 0.1; done";
        std::fs::write(
            cfg.join("counter.toml"),
            format!(
                "name = \"counter\"\n[locate]\ncommands = [\"sh\"]\n[launch]\nargs = [\"-c\", {}]\n",
                serde_json::to_string(script).unwrap()
            ),
        )
        .unwrap();
        World { _tmp: tmp, root }
    }

    fn paddock(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_paddock"));
        c.env("XDG_CONFIG_HOME", self.root.join("cfg"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("XDG_RUNTIME_DIR", self.root.join("run"))
            .current_dir(self.root.join("project"));
        c
    }

    fn count(&self) -> u64 {
        std::fs::read_to_string(self.root.join("project/count"))
            .ok()
            .and_then(|t| t.trim().parse().ok())
            .unwrap_or(0)
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
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn ok(out: std::process::Output) -> String {
    assert!(out.status.success(), "{out:?}");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn a_frozen_sandbox_stops_and_a_thawed_one_continues() {
    if !can_run() {
        eprintln!("skipped: needs bwrap, a systemd user instance and the session bus");
        return;
    }
    let w = World::new();
    let mut child = w
        .paddock()
        .args([
            "run",
            "counter",
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
    let unit = sb["unit"].as_str().unwrap_or_default().to_string();
    assert!(unit.starts_with("paddock-sb-"), "{sb}");
    wait_for("the counter", || (w.count() > 3).then_some(()));

    ok(w.paddock()
        .args(["freeze", &id, "--reason", "test"])
        .output()
        .unwrap());
    std::thread::sleep(Duration::from_millis(300));
    let a = w.count();
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(w.count(), a, "the counter moved while frozen");
    let sb = w.ps().into_iter().next().unwrap();
    assert!(sb["frozen_since"].is_u64(), "{sb}");
    assert_eq!(sb["frozen_reason"], "test");
    let kv = ok(w
        .paddock()
        .args(["kv", "get", &format!("sandbox:{id}")])
        .output()
        .unwrap());
    assert!(kv.contains("suspended"), "{kv}");

    ok(w.paddock().args(["thaw", &id]).output().unwrap());
    let b = w.count();
    wait_for("the counter after the thaw", || {
        (w.count() > b + 3).then_some(())
    });
    assert!(w.ps().into_iter().next().unwrap()["frozen_since"].is_null());
    let kv = ok(w
        .paddock()
        .args(["kv", "get", &format!("sandbox:{id}")])
        .output()
        .unwrap());
    assert!(kv.contains("running"), "{kv}");

    // A frozen sandbox can be stopped: paddock thaws it first.
    ok(w.paddock().args(["freeze", &id]).output().unwrap());
    ok(w.paddock().args(["stop", &id]).output().unwrap());
    wait_for("paddock to exit", || child.try_wait().unwrap());
    assert!(w.ps().is_empty());
}
