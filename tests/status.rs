//! `paddock-host status` in a bwrap sandbox, as the Claude Code hooks run
//! it: the session hook prints the timers; the prompt and tool hooks print
//! only a timer whose time is up, and the tool hook says it once. A shell
//! harness plays the agent. Temporary folders only; network mode none.

mod common;

use std::process::Command;

fn can_run() -> bool {
    Command::new("bwrap")
        .args(["--unshare-user", "--ro-bind", "/", "/", "true"])
        .status()
        .is_ok_and(|s| s.success())
}

#[test]
fn the_hooks_say_what_needs_attention() {
    if !can_run() {
        eprintln!("skipped: needs bwrap");
        return;
    }
    let tmp = common::tempdir();
    let root = tmp.path().to_path_buf();
    let cfg = root.join("cfg/paddock/harness.d");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::create_dir_all(root.join("project")).unwrap();
    std::fs::create_dir_all(root.join("home")).unwrap();
    let run = root.join("run");
    std::fs::create_dir_all(&run).unwrap();
    std::fs::set_permissions(&run, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    let script = "paddock-host timer start calm --budget 1h >/dev/null; \
        paddock-host status --hook session > session.txt; \
        paddock-host status --hook prompt > prompt-calm.txt; \
        paddock-host timer start late --budget 1s >/dev/null; sleep 2; \
        paddock-host status --hook prompt > prompt.txt; \
        paddock-host status --hook tool > tool1.txt; \
        paddock-host status --hook tool > tool2.txt";
    std::fs::write(
        cfg.join("hookcheck.toml"),
        format!(
            "name = \"hookcheck\"\n[locate]\ncommands = [\"sh\"]\n[launch]\nargs = [\"-c\", {}]\n",
            serde_json::to_string(script).unwrap()
        ),
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_paddock"))
        .args([
            "run",
            "hookcheck",
            "--backend",
            "bwrap",
            "--network",
            "none",
            "--env",
            "clean",
            "-y",
        ])
        .arg(root.join("project"))
        .env("HOME", root.join("home"))
        .env("XDG_CONFIG_HOME", root.join("cfg"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_RUNTIME_DIR", &run)
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
        .current_dir(root.join("project"))
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let read = |f: &str| std::fs::read_to_string(root.join("project").join(f)).unwrap_or_default();
    let session = read("session.txt");
    assert!(
        session.starts_with("paddock: timer calm: used"),
        "{session}"
    );
    assert!(session.contains("of 1h"), "{session}");
    assert_eq!(read("prompt-calm.txt"), "", "nothing needs attention yet");
    let prompt = read("prompt.txt");
    assert!(
        prompt.contains("timer late") && prompt.contains("time is up"),
        "{prompt}"
    );
    assert!(!prompt.contains("calm"), "{prompt}");
    let tool: serde_json::Value = serde_json::from_str(&read("tool1.txt")).unwrap();
    assert_eq!(tool["hookSpecificOutput"]["hookEventName"], "PostToolUse");
    assert!(tool["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap()
        .contains("late"));
    assert_eq!(
        read("tool2.txt"),
        "",
        "the tool hook says the same thing once"
    );
}
