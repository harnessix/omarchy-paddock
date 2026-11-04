//! The first-run commands on macOS, end to end, in a temporary home: the
//! Linux-only commands refuse with one line, and the shell shims go into a
//! new `~/.zshrc` and out again. A fake harness (a shell script), no real
//! harness, no network.
#![cfg(target_os = "macos")]

use std::path::Path;
use std::process::{Command, Output, Stdio};

fn paddock(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_paddock"))
        .args(args)
        .env_clear()
        .env("HOME", root.join("home"))
        .env("USER", "demo")
        .env("PATH", root.join("bin"))
        .env("XDG_RUNTIME_DIR", root.join("run"))
        .env("PADDOCK_NO_UPDATE_CHECK", "1")
        .current_dir(root)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn world() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    let t = tempfile::tempdir().unwrap();
    for d in ["home", "run", "bin"] {
        std::fs::create_dir_all(t.path().join(d)).unwrap();
    }
    let codex = t.path().join("bin/codex");
    std::fs::write(&codex, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&codex, std::fs::Permissions::from_mode(0o755)).unwrap();
    t
}

#[test]
fn linux_only_commands_refuse_with_one_line() {
    let t = world();
    for args in [
        &["launchers", "install"][..],
        &["launchers", "list"][..],
        &["shims", "install", "--scope", "omarchy-agent"][..],
    ] {
        let out = paddock(t.path(), args);
        assert!(!out.status.success(), "{args:?}: {out:?}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("Linux only"), "{args:?}: {err}");
        assert_eq!(err.trim().lines().count(), 1, "{args:?}: {err}");
    }
    // No shell file and no desktop entry was written.
    let home = t.path().join("home");
    assert!(!home.join(".zshrc").exists());
    assert!(!home.join(".local/share/applications").exists());
}

#[test]
fn shims_install_and_remove_leave_the_home_as_it_was() {
    let t = world();
    let out = paddock(t.path(), &["shims", "install"]);
    assert!(out.status.success(), "{out:?}");
    let zshrc = t.path().join("home/.zshrc");
    let text = std::fs::read_to_string(&zshrc).unwrap();
    assert!(text.contains("paddock shims"), "{text}");
    let list = paddock(t.path(), &["shims", "list"]);
    let list = String::from_utf8_lossy(&list.stdout);
    assert!(list.contains("on   codex"), "{list}");
    assert!(list.contains("scope omarchy-agent: off, "), "{list}");

    let out = paddock(t.path(), &["shims", "remove"]);
    assert!(out.status.success(), "{out:?}");
    assert!(!zshrc.exists());
    assert!(!t.path().join("home/.bashrc").exists());
    assert!(!t.path().join("home/.local/share/paddock/shims.sh").exists());
}

#[test]
fn setup_skips_the_linux_steps_and_doctor_checks_the_mac() {
    let t = world();
    let out = paddock(t.path(), &["setup", "--yes", "--launchers", "--plugin"]);
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8_lossy(&out.stdout);
    for want in [
        "skipped  paddock launchers",
        "skipped  the bar widget",
        "auto picks seatbelt",
    ] {
        assert!(text.contains(want), "{want}: {text}");
    }
    for never in ["pacman", "apt ", "dnf", "Hyprland"] {
        assert!(!text.contains(never), "{never}: {text}");
    }
    let out = paddock(t.path(), &["doctor"]);
    let text = String::from_utf8_lossy(&out.stdout);
    for want in [
        "macOS",
        "sandbox-exec",
        "command line tools",
        "seatbelt",
        "refused",
    ] {
        assert!(text.contains(want), "{want}: {text}");
    }
    assert!(!text.contains("sudo "), "{text}");
}
