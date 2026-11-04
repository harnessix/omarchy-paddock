//! Privacy defaults in `paddock explain`: the telemetry switches of a profile
//! appear with their origin, and `[privacy] telemetry = "vendor"` in the user
//! configuration removes them. Nothing is written to the host home.

use std::path::Path;
use std::process::Command;

const PROFILE: &str = r#"name = "privtest"
[locate]
commands = ["sh"]
[launch]
yolo_args = ["-c", "true"]
[privacy]
env = { VENDOR_TELEMETRY_OFF = "1" }
args = ["--no-telemetry"]
[[privacy.settings]]
file = ".privtest/settings.json"
path = ["telemetry", "enabled"]
value = false
[privacy.training]
setting = "the account settings"
url = "https://example.com/settings"
"#;

fn explain(root: &Path, user: &str) -> String {
    let cfg = root.join("config");
    std::fs::create_dir_all(cfg.join("paddock/harness.d")).unwrap();
    std::fs::write(cfg.join("paddock/harness.d/privtest.toml"), PROFILE).unwrap();
    std::fs::write(cfg.join("paddock/config.toml"), user).unwrap();
    let proj = root.join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_paddock"))
        .args(["explain", "privtest"])
        .arg(&proj)
        .args(["--env", "clean", "--network", "none"])
        .env("XDG_CONFIG_HOME", &cfg)
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_STATE_HOME", root.join("state"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn explain_shows_the_privacy_defaults_and_vendor_mode_removes_them() {
    let tmp = tempfile::tempdir().unwrap();
    let off = explain(&tmp.path().join("off"), "");
    assert!(
        off.contains("VENDOR_TELEMETRY_OFF=1\n         profile privtest privacy (telemetry off)"),
        "{off}"
    );
    assert!(
        off.contains("~/.privtest/settings.json  telemetry.enabled = false"),
        "{off}"
    );
    assert!(off.contains("--no-telemetry"), "{off}");

    let vendor = explain(
        &tmp.path().join("vendor"),
        "[privacy]\ntelemetry = \"vendor\"\n",
    );
    assert!(!vendor.contains("VENDOR_TELEMETRY_OFF"), "{vendor}");
    assert!(!vendor.contains("settings.json"), "{vendor}");
    assert!(!vendor.contains("--no-telemetry"), "{vendor}");
}
