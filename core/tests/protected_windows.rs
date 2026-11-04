//! `paddock windows`: the protected windows, their border by network level,
//! and the border coming off after the run ends. A fake `hyprctl` on PATH
//! answers with a window list and records each command. Nothing reaches the
//! real compositor.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

struct World {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    window: Child,
    host_window: Child,
    /// The app process really has a mount namespace of its own.
    isolated: bool,
}

impl Drop for World {
    fn drop(&mut self) {
        for c in [&mut self.window, &mut self.host_window] {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

impl World {
    /// A world with three windows. This test process plays the paddock
    /// process of an app sandbox. Its child in a mount namespace of its own
    /// plays the app. Its child in the host namespace plays a host program
    /// that a host helper started, such as the gateway daemon.
    fn new() -> Self {
        let tmp = common::tempdir();
        let root = tmp.path().to_path_buf();
        for d in ["home", "cfg/paddock", "project", "bin"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        let run = root.join("run");
        std::fs::create_dir_all(&run).unwrap();
        std::fs::set_permissions(&run, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .unwrap();
        // bwrap, as paddock uses it: some hosts (Ubuntu with AppArmor)
        // allow user namespaces for bwrap only, not for `unshare`.
        let window = Command::new("bwrap")
            .args(["--dev-bind", "/", "/", "sleep", "60"])
            .stderr(Stdio::null())
            .spawn()
            .or_else(|_| Command::new("sleep").arg("60").spawn())
            .unwrap();
        let host_window = Command::new("sleep").arg("60").spawn().unwrap();
        // Wait until unshare has made its namespace and run sleep.
        let own = format!("/proc/{}/ns/mnt", window.id());
        let mut isolated = false;
        for _ in 0..100 {
            if std::fs::read_link(&own).ok() != std::fs::read_link("/proc/self/ns/mnt").ok()
                && std::fs::read_to_string(format!("/proc/{}/comm", window.id()))
                    .is_ok_and(|c| c.trim() == "sleep")
            {
                isolated = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let w = World {
            _tmp: tmp,
            root,
            window,
            host_window,
            isolated,
        };
        w.set_clients(serde_json::json!([
            {"address": "0xa1", "pid": w.window.id(), "at": [10, 50], "size": [800, 600],
             "workspace": {"id": 1, "name": "1"}, "focusHistoryID": 0,
             "title": "Visual Studio Code", "class": "code"},
            {"address": "0xb2", "pid": 1, "at": [820, 50], "size": [700, 600],
             "workspace": {"id": 1, "name": "1"}, "focusHistoryID": 1,
             "title": "host terminal", "class": "Alacritty"},
            {"address": "0xc3", "pid": w.host_window.id(), "at": [0, 0], "size": [400, 300],
             "workspace": {"id": 2, "name": "2"}, "focusHistoryID": 2,
             "title": "host browser", "class": "firefox"}
        ]));
        let hyprctl = w.root.join("bin/hyprctl");
        std::fs::write(
            &hyprctl,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{log}'\ncase \"$1\" in\n  clients) cat '{clients}' ;;\n  dispatch) echo ok ;;\n  *) exit 1 ;;\nesac\n",
                log = w.log().display(),
                clients = w.root.join("clients.json").display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(
            &hyprctl,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        w
    }

    fn log(&self) -> PathBuf {
        self.root.join("hyprctl.log")
    }

    fn take_log(&self) -> String {
        let t = std::fs::read_to_string(self.log()).unwrap_or_default();
        let _ = std::fs::remove_file(self.log());
        t
    }

    fn set_clients(&self, v: serde_json::Value) {
        std::fs::write(self.root.join("clients.json"), v.to_string()).unwrap();
    }

    fn run_dir(&self, id: &str) -> PathBuf {
        self.root.join("run/paddock").join(id)
    }

    fn add_app(&self, id: &str, network: &str) {
        let dir = self.run_dir(id);
        std::fs::create_dir_all(&dir).unwrap();
        let info = serde_json::json!({
            "id": id, "harness": "vscode", "project": self.root.join("project"),
            "backend": "bwrap", "network": network, "gui": "app",
            "pid": std::process::id(), "host_pid": std::process::id(),
            "started": now() - 7300, "pasture": "dev",
        });
        std::fs::write(dir.join("info.json"), info.to_string()).unwrap();
    }

    fn paddock(&self, args: &[&str]) -> std::process::Output {
        let path = format!("{}:/usr/bin:/bin", self.root.join("bin").display());
        Command::new(env!("CARGO_BIN_EXE_paddock"))
            .args(args)
            .env_clear()
            .env("PATH", path)
            .env("USER", "demo")
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("cfg"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("XDG_RUNTIME_DIR", self.root.join("run"))
            .current_dir(self.root.join("project"))
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn json(&self, args: &[&str]) -> serde_json::Value {
        let out = self.paddock(args);
        assert!(out.status.success(), "{out:?}");
        serde_json::from_slice(&out.stdout).unwrap()
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn set_light_theme(home: &Path) {
    let d = home.join(".local/state/omarchy/current/theme");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("colors.toml"), "mode = \"light\"\n").unwrap();
}

#[test]
fn lists_and_marks_protected_windows_by_network_level() {
    let w = World::new();
    if !w.isolated {
        eprintln!("skipped: bwrap cannot create a namespace here");
        return;
    }
    w.add_app("app-1", "egress");

    let v = w.json(&["windows", "--json", "--mark"]);
    assert_eq!(v["api_version"], "paddock.windows/v1");
    assert_eq!(v["theme"], "dark");
    let ws = v["windows"].as_array().unwrap();
    assert_eq!(ws.len(), 1, "{v}");
    let x = &ws[0];
    assert_eq!(x["sandbox"], "app-1");
    assert_eq!(x["app"], "vscode");
    assert_eq!(x["kind"], "app");
    assert_eq!(x["address"], "0xa1");
    assert_eq!(x["level"], "proxy");
    assert_eq!(x["focused"], true);
    assert_eq!(x["at"], serde_json::json!([10, 50]));
    assert_eq!(x["badge"], "paddock · dev · egress · 2h");
    let log = w.take_log();
    assert!(log.contains("address:0xa1"), "{log}");
    assert!(log.contains("rgb(9fb8a0)"), "sage for egress: {log}");
    assert!(
        !log.contains("0xb2"),
        "the host terminal keeps its border: {log}"
    );
    assert!(
        !log.contains("0xc3"),
        "a host program in the app's tree: {log}"
    );

    // A wider network: amber.
    w.add_app("app-1", "host");
    let v = w.json(&["windows", "--json", "--mark"]);
    assert_eq!(v["windows"][0]["level"], "wide");
    let log = w.take_log();
    assert!(log.contains("rgb(e0a33a)"), "amber for host: {log}");

    // A light theme: the deeper tone.
    set_light_theme(&w.root.join("home"));
    let v = w.json(&["windows", "--json", "--mark"]);
    assert_eq!(v["theme"], "light");
    let log = w.take_log();
    assert!(log.contains("rgb(a8701a)"), "{log}");

    // The run ends, the window stays: the border comes off, once.
    std::fs::remove_dir_all(w.run_dir("app-1")).unwrap();
    let v = w.json(&["windows", "--json", "--mark"]);
    assert_eq!(v["windows"], serde_json::json!([]));
    let log = w.take_log();
    assert!(
        log.contains("address:0xa1") && log.contains("unset"),
        "{log}"
    );
    w.json(&["windows", "--json", "--mark"]);
    let log = w.take_log();
    assert!(!log.contains("unset"), "no second unset: {log}");
}

#[test]
fn focus_takes_only_protected_windows() {
    let w = World::new();
    if !w.isolated {
        eprintln!("skipped: bwrap cannot create a namespace here");
        return;
    }
    w.add_app("app-2", "api");
    let out = w.paddock(&["windows", "--focus", "0xb2"]);
    assert!(!out.status.success(), "an unprotected window: {out:?}");
    let out = w.paddock(&["windows", "--focus", "0xa1\"; x"]);
    assert!(!out.status.success());
    assert!(!w.take_log().contains("dispatch"));
    let out = w.paddock(&["windows", "--focus", "0xa1"]);
    assert!(out.status.success(), "{out:?}");
    let log = w.take_log();
    assert!(
        log.contains("dispatch") && log.contains("address:0xa1"),
        "{log}"
    );
}

#[test]
fn without_hyprland_the_list_is_empty() {
    let w = World::new();
    w.add_app("app-3", "egress");
    // A hyprctl that cannot reach a compositor. PATH also has /usr/bin, so
    // the fake stays in place instead of being removed.
    std::fs::write(w.root.join("bin/hyprctl"), "#!/bin/sh\nexit 1\n").unwrap();
    let v = w.json(&["windows", "--json", "--mark"]);
    assert_eq!(v["windows"], serde_json::json!([]));
}
