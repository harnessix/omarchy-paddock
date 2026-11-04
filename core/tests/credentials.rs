//! The deliver channel end to end, with bwrap: the harness does not start
//! before the user answers both cards, only two yes answers hand the value
//! over, and a registered credential never passes with the environment.
//! The answers go through `paddock approvals answer`, as from the panel.
//! Temporary folders only; network mode none.

mod common;

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const SECRET: &str = "s3cret-value-for-the-test";

fn can_run() -> bool {
    common::bwrap_works()
}

struct World {
    _tmp: tempfile::TempDir,
    root: PathBuf,
}

impl World {
    fn new() -> Self {
        let tmp = common::tempdir();
        let root = tmp.path().to_path_buf();
        let cfg = root.join("cfg/paddock");
        std::fs::create_dir_all(cfg.join("harness.d")).unwrap();
        std::fs::create_dir_all(root.join("project")).unwrap();
        std::fs::create_dir_all(root.join("home")).unwrap();
        let run = root.join("run");
        std::fs::create_dir_all(&run).unwrap();
        std::fs::set_permissions(&run, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .unwrap();
        // The harness writes what it sees: the delivered variable, and
        // whether the host variable came along with the environment.
        let script = "{ echo \"DB=${DB_PASSWORD:-none}\"; echo \"HOST=${HOST_DB:-none}\"; } > seen";
        std::fs::write(
            cfg.join("harness.d/credcheck.toml"),
            format!(
                "name = \"credcheck\"\n[locate]\ncommands = [\"sh\"]\n[launch]\nargs = [\"-c\", {}]\n",
                serde_json::to_string(script).unwrap()
            ),
        )
        .unwrap();
        std::fs::write(
            cfg.join("config.toml"),
            "[credentials.DB_PASSWORD]\nvia = \"deliver\"\nvar = \"HOST_DB\"\n\n[host]\nwait = 30\n",
        )
        .unwrap();
        World { _tmp: tmp, root }
    }

    fn cmd(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_paddock"));
        c.env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("cfg"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("XDG_RUNTIME_DIR", self.root.join("run"))
            // The source of the credential, and a name that looks like a
            // secret: neither may pass with the environment.
            .env("HOST_DB", SECRET)
            // No desktop notification on the machine that runs the tests.
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .current_dir(self.root.join("project"));
        c
    }

    fn start(&self) -> Child {
        let _ = std::fs::remove_file(self.seen_file());
        self.cmd()
            .args([
                "run",
                "credcheck",
                "--backend",
                "bwrap",
                "--network",
                "none",
                "-y",
            ])
            .arg(self.root.join("project"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    fn seen_file(&self) -> PathBuf {
        self.root.join("project/seen")
    }

    /// The id of the waiting card whose summary contains `what`.
    fn card(&self, what: &str) -> String {
        let start = Instant::now();
        loop {
            let out = self.cmd().args(["approvals", "--json"]).output().unwrap();
            let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
            for p in v["pending"].as_array().into_iter().flatten() {
                if p["capability"] == "credentials"
                    && p["summary"].as_str().is_some_and(|s| s.contains(what))
                {
                    // The card names the credential, never its value.
                    assert!(!p.to_string().contains(SECRET));
                    return p["id"].as_str().unwrap().to_string();
                }
            }
            assert!(start.elapsed() < Duration::from_secs(30), "no card: {what}");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn answer(&self, id: &str, a: &str) {
        let out = self
            .cmd()
            .args(["approvals", "answer", id, a])
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
    }

    fn finish(&self, mut child: Child) -> String {
        let start = Instant::now();
        while child.try_wait().unwrap().is_none() {
            assert!(
                start.elapsed() < Duration::from_secs(60),
                "the sandbox did not end"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
        std::fs::read_to_string(self.seen_file()).unwrap()
    }
}

#[test]
fn deliver_needs_two_answers_before_the_harness_starts() {
    if !can_run() {
        eprintln!("skipped: needs bwrap");
        return;
    }
    let w = World::new();

    // Allow, then confirm: the value is there, the host variable is not.
    let child = w.start();
    let first = w.card("give DB_PASSWORD");
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        !w.seen_file().exists(),
        "the harness started before the answer"
    );
    w.answer(&first, "once");
    let second = w.card("confirm");
    assert!(
        !w.seen_file().exists(),
        "the harness started before the second answer"
    );
    w.answer(&second, "once");
    let seen = w.finish(child);
    assert!(seen.contains(&format!("DB={SECRET}")), "{seen}");
    assert!(seen.contains("HOST=none"), "{seen}");

    // Allow, then no: the sandbox starts without it.
    let child = w.start();
    let first = w.card("give DB_PASSWORD");
    w.answer(&first, "once");
    let second = w.card("confirm");
    w.answer(&second, "deny");
    let seen = w.finish(child);
    assert!(
        seen.contains("DB=none") && seen.contains("HOST=none"),
        "{seen}"
    );

    // The audit log and the list name it, never the value.
    let out = w
        .cmd()
        .args(["credentials", "list", "--json"])
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["credentials"][0]["name"], "DB_PASSWORD");
    assert_eq!(v["credentials"][0]["delivered"], 1);
    let log = w.cmd().args(["log", "--json"]).output().unwrap();
    let log = String::from_utf8_lossy(&log.stdout);
    assert!(log.contains("credentials DB_PASSWORD"), "{log}");
    assert!(!log.contains(SECRET));
}
