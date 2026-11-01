//! The machine identity that a bwrap sandbox sees: two isolated logins read
//! different machine IDs, one isolated login reads the same one at every
//! start, and a shared login reads the host's. Temporary folders only; the
//! harness is a shell that prints what it reads, and nothing leaves the
//! machine (network mode none).

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

fn can_run() -> bool {
    Command::new("bwrap")
        .args(["--unshare-user", "--ro-bind", "/", "/", "true"])
        .status()
        .is_ok_and(|s| s.success())
}

struct World {
    _tmp: tempfile::TempDir,
    root: PathBuf,
}

impl World {
    fn new() -> Self {
        let tmp = common::tempdir();
        let root = tmp.path().to_path_buf();
        let cfg = root.join("cfg/paddock/harness.d");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::create_dir_all(root.join("project")).unwrap();
        std::fs::create_dir_all(root.join("home")).unwrap();
        let run = root.join("run");
        std::fs::create_dir_all(&run).unwrap();
        std::fs::set_permissions(&run, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .unwrap();
        let script = "{ cat /etc/machine-id; cat /proc/sys/kernel/random/boot_id; } > seen";
        std::fs::write(
            cfg.join("idcheck.toml"),
            format!(
                "name = \"idcheck\"\n[locate]\ncommands = [\"sh\"]\n[launch]\nargs = [\"-c\", {}]\n",
                serde_json::to_string(script).unwrap()
            ),
        )
        .unwrap();
        World { _tmp: tmp, root }
    }

    /// Run the harness with one login, and return the machine ID and the
    /// boot ID that it read.
    fn seen(&self, auth: &str) -> (String, String) {
        let seen = self.root.join("project/seen");
        let _ = std::fs::remove_file(&seen);
        let out = Command::new(env!("CARGO_BIN_EXE_paddock"))
            .args([
                "run",
                "idcheck",
                "--backend",
                "bwrap",
                "--network",
                "none",
                "--env",
                "clean",
                "--auth",
                auth,
                "-y",
            ])
            .arg(self.root.join("project"))
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("cfg"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("XDG_RUNTIME_DIR", self.root.join("run"))
            .current_dir(self.root.join("project"))
            .output()
            .unwrap();
        assert!(out.status.success(), "{auth}: {out:?}");
        let text = std::fs::read_to_string(&seen).unwrap();
        let mut lines = text.lines();
        (
            lines.next().unwrap_or_default().to_string(),
            lines.next().unwrap_or_default().to_string(),
        )
    }
}

fn host(p: &str) -> String {
    std::fs::read_to_string(Path::new(p))
        .unwrap_or_default()
        .trim()
        .to_string()
}

#[test]
fn each_isolated_login_is_its_own_machine() {
    if !can_run() {
        eprintln!("skipped: needs bwrap");
        return;
    }
    let w = World::new();
    let (a1, boot_a1) = w.seen("isolated:a");
    let (a2, boot_a2) = w.seen("isolated:a");
    let (b, boot_b) = w.seen("isolated:b");
    let (shared, boot_shared) = w.seen("shared");
    let host_id = host("/etc/machine-id");
    let host_boot = host("/proc/sys/kernel/random/boot_id");

    assert_eq!(a1.len(), 32, "{a1:?}");
    assert_eq!(a1, a2, "one isolated login keeps its machine ID");
    assert_eq!(boot_a1, boot_a2, "and its boot ID during one boot");
    assert_ne!(a1, b, "two isolated logins differ");
    assert_ne!(boot_a1, boot_b);
    for (id, boot) in [(&a1, &boot_a1), (&b, &boot_b)] {
        assert_ne!(
            *id, host_id,
            "an isolated login hides the host's machine ID"
        );
        assert_ne!(*boot, host_boot, "and its boot ID");
    }
    assert_eq!(
        shared, host_id,
        "a shared login reads the host's machine ID"
    );
    assert_eq!(boot_shared, host_boot);
}
