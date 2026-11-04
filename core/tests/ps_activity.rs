//! `paddock ps --json` keeps its fields and adds `activity` and `ended`.
//! A registered sandbox and run records in a temporary folder. No harness.

mod common;

use std::path::PathBuf;
use std::process::{Command, Stdio};

struct World {
    _tmp: tempfile::TempDir,
    root: PathBuf,
}

impl World {
    fn new() -> Self {
        let tmp = common::tempdir();
        let root = tmp.path().to_path_buf();
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::fs::create_dir_all(root.join("cfg/paddock")).unwrap();
        std::fs::create_dir_all(root.join("project")).unwrap();
        let run = root.join("run");
        std::fs::create_dir_all(&run).unwrap();
        std::fs::set_permissions(&run, std::os::unix::fs::PermissionsExt::from_mode(0o700))
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
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .env_remove("TMUX")
            .current_dir(self.root.join("project"))
            .stdin(Stdio::null());
        c
    }
}

#[test]
fn ps_json_keeps_existing_fields_and_adds_activity_and_ended() {
    let w = World::new();
    let project = w.root.join("project");
    let id = "fake-p-1";
    let run = w.root.join("run/paddock").join(id);
    std::fs::create_dir_all(&run).unwrap();
    let info = serde_json::json!({
        "id": id,
        "harness": "fake",
        "project": project,
        "backend": "bwrap",
        "network": "none",
        "gui": "off",
        "pid": std::process::id(),
        "started": 10,
        "auth": "none",
        "warnings": ["login ports open on 127.0.0.1"],
        "host_pid": 1,
    });
    std::fs::write(run.join("info.json"), serde_json::to_vec(&info).unwrap()).unwrap();

    let quiet = "fake-p-2";
    let quiet_run = w.root.join("run/paddock").join(quiet);
    std::fs::create_dir_all(&quiet_run).unwrap();
    let mut quiet_info = info.clone();
    quiet_info["id"] = quiet.into();
    quiet_info["started"] = 20.into();
    std::fs::write(
        quiet_run.join("info.json"),
        serde_json::to_vec(&quiet_info).unwrap(),
    )
    .unwrap();

    let log_dir = w.root.join("run/paddock/view");
    std::fs::create_dir_all(&log_dir).unwrap();
    std::fs::write(
        log_dir.join(format!("{id}.log")),
        b"\x1b[32mcompiling crate\x1b[0m\n\n",
    )
    .unwrap();

    let records = w.root.join("state/paddock/detach");
    std::fs::create_dir_all(&records).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let ended = serde_json::json!({
        "id": "fake-p-ok",
        "harness": "fake",
        "project": project,
        "started": 1,
        "ended": now,
        "done": { "status": "ok", "text": "all good", "at": now },
    });
    std::fs::write(
        records.join("fake-p-ok.json"),
        serde_json::to_vec(&ended).unwrap(),
    )
    .unwrap();
    let old = serde_json::json!({
        "id": "fake-p-old",
        "harness": "fake",
        "project": project,
        "started": 1,
        "ended": now.saturating_sub(11 * 60),
        "done": { "status": "ok", "text": "too old", "at": 1 },
    });
    std::fs::write(
        records.join("fake-p-old.json"),
        serde_json::to_vec(&old).unwrap(),
    )
    .unwrap();

    let out = w.paddock().args(["ps", "--json"]).output().unwrap();
    assert!(out.status.success(), "{out:?}");
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["api_version"], "paddock.ps/v1");

    let sandboxes = v["sandboxes"].as_array().unwrap();
    assert_eq!(sandboxes.len(), 2, "{v}");
    let sb = sandboxes
        .iter()
        .find(|s| s["id"] == id)
        .unwrap_or_else(|| panic!("{v}"));
    assert_eq!(sb["harness"], "fake");
    assert_eq!(sb["project"].as_str(), project.to_str());
    assert_eq!(sb["backend"], "bwrap");
    assert_eq!(sb["network"], "none");
    assert_eq!(sb["gui"], "off");
    assert_eq!(sb["pid"].as_u64(), Some(std::process::id() as u64));
    assert_eq!(sb["started"].as_u64(), Some(10));
    assert_eq!(sb["auth"], "none");
    assert_eq!(sb["warnings"][0], "login ports open on 127.0.0.1");
    assert_eq!(sb["host_pid"].as_u64(), Some(1));
    assert!(sb["window_pid"].is_null());
    assert!(sb["logins"].as_array().unwrap().is_empty());
    assert!(sb["unit"].is_null());
    assert!(sb["kv_project"].is_null());
    assert!(sb["pasture"].is_null());
    assert_eq!(sb["view"], true);
    assert_eq!(sb["view_tmux"], false);
    assert_eq!(sb["detached"], false);
    assert_eq!(sb["boundary_count"], 0);
    assert_eq!(sb["activity"], "compiling crate");

    let blank = sandboxes.iter().find(|s| s["id"] == quiet).unwrap();
    assert!(blank["activity"].is_null(), "{blank}");
    assert_eq!(blank["view"], false);
    assert_eq!(blank["harness"], "fake");
    assert_eq!(blank["backend"], "bwrap");

    let ended_rows = v["ended"].as_array().unwrap();
    assert_eq!(ended_rows.len(), 1, "{v}");
    assert_eq!(ended_rows[0]["id"], "fake-p-ok");
    assert_eq!(ended_rows[0]["harness"], "fake");
    assert_eq!(ended_rows[0]["status"], "ok");
    assert_eq!(ended_rows[0]["text"], "all good");
    assert_eq!(ended_rows[0]["project"].as_str(), project.to_str());
    assert_eq!(ended_rows[0]["ended"].as_u64(), Some(now));
}
