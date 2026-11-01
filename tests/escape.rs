//! Run the escape probe in a real sandbox. Skipped where the host cannot run
//! bubblewrap (for example a CI container without user namespaces).

mod common;

use std::process::Command;

#[test]
fn bwrap_offline_escape_probe_passes() {
    if !common::bwrap_works() {
        eprintln!("skipped: bwrap cannot create a user namespace here");
        return;
    }
    // Never the user's own folders: a run of a build with them could change
    // the user's paddock state.
    let tmp = common::tempdir();
    let out = Command::new(env!("CARGO_BIN_EXE_paddock"))
        .env("XDG_CONFIG_HOME", tmp.path().join("cfg"))
        .env("XDG_DATA_HOME", tmp.path().join("data"))
        .env("XDG_STATE_HOME", tmp.path().join("state"))
        .env("XDG_CACHE_HOME", tmp.path().join("cache"))
        .env("XDG_RUNTIME_DIR", {
            let r = tmp.path().join("run");
            std::fs::create_dir_all(&r).unwrap();
            std::fs::set_permissions(&r, std::os::unix::fs::PermissionsExt::from_mode(0o700))
                .unwrap();
            r
        })
        .args([
            "doctor",
            "--escape",
            "--backend",
            "bwrap",
            "--network",
            "none",
        ])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "escape probe failed:\n{text}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("ok git-hooks-read-only"));
    for check in [
        "ok git-dir-cannot-be-renamed",
        "ok git-commondir-read-only",
        "ok git-planted-config-never-runs",
        "ok git-layout-still-trusted",
    ] {
        assert!(text.contains(check), "missing {check}:\n{text}");
    }
}
