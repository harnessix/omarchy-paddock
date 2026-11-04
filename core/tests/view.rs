//! The live view end to end: `paddock run` without a terminal gives the
//! caller the same stdout, stderr and exit code with and without tmux, the
//! tmux session exists while the sandbox runs and stays after it, and
//! `paddock ps --json` says so. A fake harness, temporary folders, and
//! paddock's tmux server on a socket in the temporary runtime folder,
//! killed at the end. No terminal window opens.

mod common;

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const SCRIPT: &str = "printf 'out one\\n'; printf 'err one\\n' >&2; \
     [ -n \"$SLOW\" ] && sleep 3; printf 'out two'; printf 'err two' >&2; exit 7";

struct World {
    _tmp: tempfile::TempDir,
    root: PathBuf,
}

impl World {
    fn new(tmux: bool, slow: bool) -> Self {
        let tmp = common::tempdir();
        let root = tmp.path().to_path_buf();
        let cfg = root.join("cfg/paddock/harness.d");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::create_dir_all(root.join("project")).unwrap();
        let run = root.join("run");
        std::fs::create_dir_all(&run).unwrap();
        std::fs::set_permissions(&run, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .unwrap();
        let script = if slow {
            format!("SLOW=1; {SCRIPT}")
        } else {
            SCRIPT.to_string()
        };
        std::fs::write(
            cfg.join("fake.toml"),
            format!(
                "name = \"fake\"\n[locate]\ncommands = [\"sh\"]\n[launch]\nargs = [\"-c\", {}]\n",
                serde_json::to_string(&script).unwrap()
            ),
        )
        .unwrap();
        std::fs::write(
            root.join("cfg/paddock/config.toml"),
            format!("[view]\ntmux = {tmux}\n"),
        )
        .unwrap();
        World { _tmp: tmp, root }
    }

    fn paddock(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_paddock"));
        c.env("XDG_CONFIG_HOME", self.root.join("cfg"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("XDG_RUNTIME_DIR", self.root.join("run"))
            .env_remove("TMUX")
            .current_dir(self.root.join("project"));
        c
    }

    fn run(&self) -> Command {
        let mut c = self.paddock();
        c.args([
            "run",
            "fake",
            "--backend",
            "bwrap",
            "--network",
            "none",
            "--env",
            "clean",
            "-y",
        ])
        .arg(self.root.join("project"))
        .stdin(Stdio::null());
        c
    }

    fn sock(&self) -> PathBuf {
        self.root.join("run/paddock/view.sock")
    }

    fn tmux(&self, args: &[&str]) -> std::process::Output {
        Command::new("tmux")
            .arg("-S")
            .arg(self.sock())
            .args(args)
            .env_remove("TMUX")
            .output()
            .unwrap()
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = self.tmux(&["kill-server"]);
    }
}

fn has_tmux() -> bool {
    Command::new("tmux")
        .arg("-V")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// The harness's own lines: paddock's notices differ in ids and times.
fn harness_lines(b: &[u8]) -> String {
    String::from_utf8_lossy(b)
        .split_inclusive('\n')
        .filter(|l| !l.starts_with("paddock") && !l.starts_with('['))
        .collect()
}

#[test]
fn the_caller_sees_the_same_output_and_exit_code_with_and_without_tmux() {
    if !common::bwrap_works() || !has_tmux() {
        eprintln!("skipped: needs bwrap with a user namespace and tmux");
        return;
    }
    let off = World::new(false, false);
    let a = off.run().output().unwrap();
    assert!(!off.sock().exists(), "tmux = false started tmux");
    let on = World::new(true, false);
    let b = on.run().output().unwrap();
    assert_eq!(a.status.code(), Some(7), "{a:?}");
    assert_eq!(b.status.code(), Some(7), "{b:?}");
    assert_eq!(a.stdout, b"out one\nout two");
    assert_eq!(b.stdout, a.stdout);
    assert_eq!(harness_lines(&b.stderr), harness_lines(&a.stderr));
    assert!(harness_lines(&a.stderr).contains("err one\nerr two"));
    // The session stays after the end (remain-on-exit).
    // `tail --pid` notices the end of paddock within about a second.
    let mut text = String::new();
    for _ in 0..50 {
        let ls = on.tmux(&["list-sessions", "-F", "#{session_name} #{pane_dead}"]);
        text = String::from_utf8_lossy(&ls.stdout).into_owned();
        if text.trim_end().ends_with(" 1") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(text.trim_end().ends_with(" 1"), "{text}");
}

#[test]
fn ps_shows_the_view_while_the_sandbox_runs_and_stop_ends_it() {
    if !common::bwrap_works() || !has_tmux() {
        eprintln!("skipped: needs bwrap with a user namespace and tmux");
        return;
    }
    let w = World::new(true, true);
    let mut child = w
        .run()
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let start = Instant::now();
    let sb = loop {
        let out = w.paddock().args(["ps", "--json"]).output().unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
        if let Some(sb) = v["sandboxes"].as_array().and_then(|a| a.first().cloned()) {
            break sb;
        }
        assert!(start.elapsed() < Duration::from_secs(30), "no sandbox");
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(sb["view"], true, "{sb}");
    assert_eq!(sb["view_tmux"], true, "{sb}");
    let id = sb["id"].as_str().unwrap().to_string();
    assert!(w
        .tmux(&["has-session", "-t", &format!("={id}")])
        .status
        .success());
    assert!(w.paddock().args(["stop", &id]).status().unwrap().success());
    let _ = child.wait();
    assert!(!w
        .tmux(&["has-session", "-t", &format!("={id}")])
        .status
        .success());
}
