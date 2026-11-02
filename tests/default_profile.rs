//! `paddock run --profile default` on bwrap with a fake harness: the home
//! is an overlay on a private snapshot of the harness's data roots. The
//! harness sees its host data, its writes stay in the run, the host files
//! never change, and the run folder is gone at the end. A temp home only.

mod common;

use std::path::Path;
use std::process::Command;

/// bwrap starts, and it can mount an overlay in a user namespace.
fn overlay_works(dir: &Path) -> bool {
    let d: Vec<_> = ["low", "up", "work", "dst"]
        .iter()
        .map(|n| {
            let p = dir.join("probe").join(n);
            std::fs::create_dir_all(&p).unwrap();
            p
        })
        .collect();
    Command::new("bwrap")
        .args(["--unshare-user", "--ro-bind", "/", "/", "--overlay-src"])
        .arg(&d[0])
        .arg("--overlay")
        .args([&d[1], &d[2], &d[3]])
        .arg("true")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn cmd(root: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_paddock"));
    c.env("HOME", root.join("home"))
        .env("XDG_CONFIG_HOME", root.join("cfg"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_RUNTIME_DIR", root.join("run"))
        .env("PADDOCK_NO_PROMOTE", "1")
        .env("PADDOCK_NO_DEDUPE", "1")
        .env_remove("DBUS_SESSION_BUS_ADDRESS");
    for v in paddock::proxy::PROXY_VARS {
        c.env_remove(v);
    }
    c
}

const SCRIPT: &str = r#"
cat "$HOME/.fake/conv.jsonl" > seen-conv
cat "$HOME/.fake/auth.json" > seen-auth
ls -A "$HOME" > seen-home
echo turn2 >> "$HOME/.fake/conv.jsonl"
echo new > "$HOME/.fake/new.txt"
rm "$HOME/.fake/old.txt"
"#;

#[test]
fn a_shadow_run_sees_the_host_data_and_changes_only_its_snapshot() {
    let tmp = common::tempdir();
    let root = tmp.path();
    if !common::bwrap_works() || !overlay_works(root) {
        eprintln!("skipped: bwrap cannot start with an overlay here");
        return;
    }
    let home = root.join("home");
    for d in [
        "cfg/paddock/harness.d",
        "home/.fake",
        "home/.ssh",
        "p",
        "run",
    ] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    std::fs::set_permissions(
        root.join("run"),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    std::fs::write(home.join(".fake/conv.jsonl"), "turn1\n").unwrap();
    std::fs::write(home.join(".fake/old.txt"), "old\n").unwrap();
    std::fs::write(home.join(".fake/auth.json"), "{\"token\":\"host\"}").unwrap();
    std::fs::write(home.join(".fake.json"), "{}").unwrap();
    std::fs::write(home.join(".ssh/id_ed25519"), "key").unwrap();
    std::fs::write(home.join(".bashrc"), "# mine").unwrap();
    std::fs::write(
        root.join("cfg/paddock/config.toml"),
        "[devices]\nmount = false\n[kv]\nenabled = false\n",
    )
    .unwrap();
    std::fs::write(
        root.join("cfg/paddock/harness.d/fake.toml"),
        format!(
            "name = \"fake\"\n[locate]\ncommands = [\"sh\"]\n[launch]\nargs = [\"-c\", {}]\n[state]\ndata = [\".fake\", \".fake.json\"]\nauth = [\".fake/auth.json\"]\n",
            serde_json::to_string(SCRIPT).unwrap()
        ),
    )
    .unwrap();
    let project = root.join("p");
    let out = cmd(root)
        .args([
            "run",
            "fake",
            "--profile",
            "default",
            "--backend",
            "bwrap",
            "--network",
            "none",
            "-y",
        ])
        .arg(&project)
        .current_dir(&project)
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{err}");
    let read = |f: &str| std::fs::read_to_string(project.join(f)).unwrap_or_default();
    // The harness saw its host data and its login.
    assert_eq!(read("seen-conv"), "turn1\n");
    assert_eq!(read("seen-auth"), "{\"token\":\"host\"}");
    // The real home is never visible.
    let listed = read("seen-home");
    assert!(
        listed.contains(".fake") && listed.contains(".fake.json"),
        "{listed}"
    );
    assert!(
        !listed.contains(".ssh") && !listed.contains(".bashrc"),
        "{listed}"
    );
    // The host files are unchanged.
    assert_eq!(
        std::fs::read_to_string(home.join(".fake/conv.jsonl")).unwrap(),
        "turn1\n"
    );
    assert!(home.join(".fake/old.txt").exists());
    assert!(!home.join(".fake/new.txt").exists());
    // The summary, then the run folder is gone.
    assert!(
        err.contains("added 1, changed 1 and deleted 1 file(s)"),
        "{err}"
    );
    assert!(err.contains("+ ~/.fake/new.txt"), "{err}");
    let runs = root.join("data/paddock/shadow/runs");
    let left = std::fs::read_dir(&runs).map(|r| r.count()).unwrap_or(0);
    assert_eq!(left, 0, "a run folder is left in {}", runs.display());
}

#[test]
fn a_container_backend_refuses_the_default_profile() {
    let tmp = common::tempdir();
    let root = tmp.path();
    for d in ["cfg/paddock/harness.d", "home/.fake", "p"] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    std::fs::write(
        root.join("cfg/paddock/harness.d/fake.toml"),
        "name = \"fake\"\n[locate]\ncommands = [\"sh\"]\n[state]\ndata = [\".fake\"]\n",
    )
    .unwrap();
    let out = cmd(root)
        .args([
            "run",
            "fake",
            "--profile",
            "default",
            "--backend",
            "docker",
            "-y",
        ])
        .arg(root.join("p"))
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("only on the bwrap backend"), "{err}");
    let runs = root.join("data/paddock/shadow/runs");
    assert_eq!(std::fs::read_dir(&runs).map(|r| r.count()).unwrap_or(0), 0);
}
