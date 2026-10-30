//! `paddock refresh` through the binary, in a temporary home with one fake
//! harness on PATH. The command line must parse, and a refresh must add a
//! harness that was installed after the shims.

use std::path::Path;
use std::process::Command;

fn write_exec(p: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn paddock(t: &Path, args: &[&str]) -> std::process::Output {
    let home = t.join("home");
    Command::new(env!("CARGO_BIN_EXE_paddock"))
        .args(args)
        .env_clear()
        .env("HOME", &home)
        .env("USER", "demo")
        .env("PATH", t.join("bin"))
        .env("XDG_RUNTIME_DIR", t.join("run"))
        .output()
        .unwrap()
}

#[test]
fn refresh_parses_and_adds_a_new_harness() {
    let t = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(t.path().join("home")).unwrap();
    std::fs::create_dir_all(t.path().join("run")).unwrap();
    std::fs::write(t.path().join("home/.bashrc"), "# mine\n").unwrap();
    write_exec(&t.path().join("bin/codex"));
    let out = paddock(t.path(), &["shims", "install"]);
    assert!(out.status.success(), "{out:?}");

    write_exec(&t.path().join("bin/grok"));
    for args in [&["refresh", "--background"][..], &["refresh"][..]] {
        let out = paddock(t.path(), args);
        assert!(out.status.success(), "{args:?}: {out:?}");
    }
    let out = paddock(t.path(), &["refresh"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("shell shims: codex, grok"), "{text}");
    let script =
        std::fs::read_to_string(t.path().join("home/.local/share/paddock/shims.sh")).unwrap();
    assert!(script.contains("grok()"));
}
