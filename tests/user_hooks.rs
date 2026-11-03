//! The user's own hooks in a sandbox, end to end without a sandbox: paddock
//! builds the spec of a fake harness profile and writes the sandbox home
//! as a start does. The fake harness, a shell script, reads its hooks file
//! from that home and runs each hook command with the sandbox PATH, where
//! `/run/paddock/bin` is a folder with the host tools of the spec. The fake
//! hook program writes its state into HOME.

mod common;

use paddock::env::HostEnv;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::process::Command;

const PROFILE: &str = r#"
name = "fakehooks"
description = "A fake harness with a hooks file"

[locate]
commands = ["sh"]

[state]
seed = [".fake/settings.json"]

[[compaction.settings]]
file = ".fake/settings.json"
path = ["hooks", "Stop"]
append = true
value = { hooks = [{ type = "command", command = "echo paddock-entry" }] }

[hooks]
files = [".fake/settings.json"]
into = ".fake/settings.json"
"#;

const HOOK: &str = "#!/bin/sh\nmkdir -p \"$HOME/.local/state/my-hook\"\necho \"$1\" >> \"$HOME/.local/state/my-hook/log\"\necho my-hook-ran\n";

/// The fake harness: every hook command of the file, in order, each one in
/// its own shell, as harnesses do. A failing hook does not stop the next.
const HARNESS: &str = r#"
sed -n 's/^ *"command": "\(.*\)",\{0,1\}$/\1/p' "$HOME/.fake/settings.json" | while IFS= read -r cmd; do
  sh -c "$cmd" >> "$HOME/hook-out.txt" 2>> "$HOME/hook-err.txt" || echo "failed: $cmd" >> "$HOME/hook-out.txt"
done
"#;

fn write(p: &Path, text: &str, mode: u32) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
}

#[test]
fn a_fake_harness_runs_paddock_first_then_the_kept_user_hooks() {
    let tmp = common::tempdir();
    let root = tmp.path();
    let uid = std::fs::metadata(root).unwrap().uid();
    let host_home = root.join("home");
    let project = root.join("proj");
    std::fs::create_dir_all(&project).unwrap();
    let mut vars = std::collections::BTreeMap::new();
    for (k, d) in [
        ("XDG_CONFIG_HOME", "cfg"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_STATE_HOME", "state"),
        ("XDG_CACHE_HOME", "cache"),
        ("XDG_RUNTIME_DIR", "run"),
    ] {
        vars.insert(k.to_string(), root.join(d).display().to_string());
    }
    let env = HostEnv::from_parts(host_home.clone(), "t".into(), uid, uid, vars);
    write(
        &env.paddock_config().join("config.toml"),
        "[hooks]\nprograms = [\"~/.local/bin/my-hook\"]\nignore = [\"dup-hook\"]\n",
        0o600,
    );
    write(&host_home.join(".local/bin/my-hook"), HOOK, 0o755);
    write(
        &host_home.join(".fake/settings.json"),
        r#"{"theme":"dark","hooks":{"Stop":[
            {"hooks":[{"type":"command","command":"false"}]},
            {"hooks":[{"type":"command","command":"my-hook stop"}]},
            {"hooks":[{"type":"command","command":"gone-hook stop"}]},
            {"hooks":[{"type":"command","command":"dup-hook stop"}]}]}}"#,
        0o644,
    );
    let before: Vec<(String, Vec<u8>)> = ["/.fake/settings.json", "/.local/bin/my-hook"]
        .iter()
        .map(|r| {
            (
                r.to_string(),
                std::fs::read(format!("{}{r}", host_home.display())).unwrap(),
            )
        })
        .collect();

    let h = paddock::profile::Harness {
        profile: toml::from_str(PROFILE).unwrap(),
        exe: "/bin/sh".into(),
        install_mounts: vec![],
        path_dirs: vec![],
        source: "test".into(),
    };
    let opts = paddock::build::Options {
        harness: "fakehooks".into(),
        project: Some(project.clone()),
        assume_yes: true,
        ..Default::default()
    };
    let b = paddock::build::build_with(&env, &opts, h).unwrap();
    let spec = &b.spec;
    let home = spec.home_src.clone();
    paddock::state::seed(&env, &b.profile, &b.auth, &home).unwrap();
    paddock::privacy::write_settings(&home, &spec.home_settings).unwrap();
    assert!(paddock::hooks::write(spec).unwrap().is_empty());
    assert!(
        spec.notices
            .iter()
            .any(|n| n.contains("gone-hook") && !n.contains("dup-hook")),
        "{:?}",
        spec.notices
    );

    // /run/paddock/bin as the sandbox has it.
    let tool_dir = root.join("toolbin");
    std::fs::create_dir_all(&tool_dir).unwrap();
    for t in &spec.host_tools {
        std::os::unix::fs::symlink(&t.src, tool_dir.join(&t.name)).unwrap();
    }
    let path = spec
        .path_value()
        .replace(paddock::spec::TOOL_DIR, &tool_dir.display().to_string());
    let out = Command::new("sh")
        .arg("-c")
        .arg(HARNESS)
        .current_dir(&project)
        .env_clear()
        .env("PATH", path)
        .env("HOME", &home)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let ran = std::fs::read_to_string(home.join("hook-out.txt")).unwrap();
    assert_eq!(ran, "paddock-entry\nfailed: false\nmy-hook-ran\n", "{ran}");
    assert!(std::fs::read_to_string(home.join("hook-err.txt"))
        .unwrap_or_default()
        .is_empty());
    // The hook program's state is in the sandbox home, never on the host.
    assert_eq!(
        std::fs::read_to_string(home.join(".local/state/my-hook/log")).unwrap(),
        "stop\n"
    );
    assert!(!host_home.join(".local/state").exists());
    // The other keys of the file stay.
    let v: serde_json::Value =
        serde_json::from_slice(&std::fs::read(home.join(".fake/settings.json")).unwrap()).unwrap();
    assert_eq!(v["theme"], "dark");
    for (r, bytes) in before {
        assert_eq!(
            std::fs::read(format!("{}{r}", host_home.display())).unwrap(),
            bytes
        );
    }
}
