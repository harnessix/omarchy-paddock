//! Run the escape probe in a real sandbox. Skipped where the host cannot run
//! bubblewrap (for example a CI container without user namespaces).

use std::process::Command;

fn bwrap_works() -> bool {
    Command::new("bwrap")
        .args(["--unshare-user", "--ro-bind", "/", "/", "true"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[test]
fn bwrap_offline_escape_probe_passes() {
    if !bwrap_works() {
        eprintln!("skipped: bwrap cannot create a user namespace here");
        return;
    }
    let out = Command::new(env!("CARGO_BIN_EXE_paddock"))
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
}
