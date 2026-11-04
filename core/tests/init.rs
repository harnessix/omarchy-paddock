//! `paddock init` writes a new paddock.toml and never writes through a
//! symlink that a sandbox left in the project.

use std::process::Command;

fn init(dir: &std::path::Path) -> bool {
    // The user's own folders stay out of tests.
    let xdg = dir.parent().unwrap().join("xdg");
    Command::new(env!("CARGO_BIN_EXE_paddock"))
        .env("XDG_CONFIG_HOME", xdg.join("cfg"))
        .env("XDG_DATA_HOME", xdg.join("data"))
        .env("XDG_STATE_HOME", xdg.join("state"))
        .env("XDG_CACHE_HOME", xdg.join("cache"))
        .arg("init")
        .arg(dir)
        .output()
        .unwrap()
        .status
        .success()
}

#[test]
fn init_never_follows_a_planted_link() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("p");
    std::fs::create_dir_all(&p).unwrap();
    let target = t.path().join("outside.toml");
    std::os::unix::fs::symlink(&target, p.join("paddock.toml")).unwrap();
    assert!(!init(&p));
    assert!(!target.exists());

    std::fs::remove_file(p.join("paddock.toml")).unwrap();
    assert!(init(&p));
    assert!(std::fs::read_to_string(p.join("paddock.toml"))
        .unwrap()
        .contains("[mounts]"));
    assert!(!init(&p), "an existing file is never replaced");
}
