//! `paddock pasture` through the binary, in a temporary home: list, and
//! remove the data of a pasture only with --yes.

use paddock::env::HostEnv;
use paddock::kv::shard::Ns;
use paddock::kv::store::{Quota, Store, Writer};
use std::path::Path;
use std::process::Command;

fn paddock(t: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_paddock"))
        .args(args)
        .env_clear()
        .env("HOME", t.join("home"))
        .env("USER", "demo")
        .env("PATH", "/usr/bin:/bin")
        .env("XDG_RUNTIME_DIR", t.join("run"))
        .current_dir(t.join("project"))
        .output()
        .unwrap()
}

fn text(o: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

#[test]
fn list_and_remove_a_pasture() {
    let t = tempfile::tempdir().unwrap();
    let home = t.path().join("home");
    let project = t.path().join("project");
    for d in [&home, &project, &t.path().join("run")] {
        std::fs::create_dir_all(d).unwrap();
    }
    let project = project.canonicalize().unwrap();
    let env = HostEnv::from_parts(home.clone(), "demo".into(), 1000, 1000, Default::default());
    // A pasture with one key and a file in its shared folder.
    let ns = Ns::pasture(&project, "team").unwrap();
    let file = ns.file(&paddock::kv::kv_root(&env)).unwrap();
    Store::open(&file)
        .unwrap()
        .put(
            &ns.key(),
            "task:1",
            "todo",
            None,
            None,
            &Writer {
                sandbox: "a1",
                harness: "claude",
            },
            Quota {
                bytes: 1 << 20,
                keys: 10,
            },
            paddock::state::now(),
        )
        .unwrap();
    let ground = paddock::pasture::ground(&env, &project, "team");
    std::fs::create_dir_all(&ground).unwrap();
    std::fs::write(ground.join("notes.md"), "shared").unwrap();

    let out = paddock(t.path(), &["pasture", "list"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("team"), "{}", text(&out));
    let out = paddock(t.path(), &["pasture", "list", "--json"]);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["pastures"][0]["name"], "team");
    assert_eq!(v["pastures"][0]["keys"], 1);

    // Without --yes it only says what it would delete.
    let out = paddock(t.path(), &["pasture", "remove", "team"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(file.exists() && ground.join("notes.md").exists());
    for bad in ["solo-a1", "../x"] {
        let out = paddock(t.path(), &["pasture", "remove", bad, "--yes"]);
        assert!(!out.status.success(), "{bad}: {}", text(&out));
    }

    let out = paddock(t.path(), &["pasture", "remove", "team", "--yes"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(!file.exists());
    assert!(!ground.exists());
    let out = paddock(t.path(), &["pasture", "remove", "team", "--yes"]);
    assert!(text(&out).contains("has no data"), "{}", text(&out));
}
