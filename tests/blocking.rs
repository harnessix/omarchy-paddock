//! Blocking reads end to end, in bubblewrap, with a fake harness: a read of
//! `/dev/paddock/wait` returns at its time, and `paddock-host kv watch
//! --timeout` and a waiting device read end with the "still waiting" exit
//! code 75. Skipped where the host cannot run bubblewrap or mount FUSE.

use std::path::PathBuf;
use std::process::{Command, Stdio};

fn bwrap_works() -> bool {
    Command::new("bwrap")
        .args(["--unshare-user", "--ro-bind", "/", "/", "true"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn fuse_works() -> bool {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/fuse")
        .is_ok()
        && ["fusermount3", "fusermount"].iter().any(|h| {
            Command::new("sh")
                .args(["-c", &format!("command -v {h}")])
                .stdout(Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        })
}

const SCRIPT: &str = r#"
if [ ! -e /dev/paddock/wait ]; then echo NODEV; exit 0; fi
paddock-host kv watch nothing:here --timeout 1 >/dev/null 2>&1; echo "WATCH_EXIT $?"
s=$(date +%s)
at=$(date -u -d "@$((s + 3))" +%Y-%m-%dT%H:%M:%SZ)
out=$(paddock-host device wait "$at" --timeout 30); e=$?
echo "WAIT_EXIT $e"
echo "WAIT_OUT $out"
echo "WAIT_AT $at"
echo "ELAPSED $(( $(date +%s) - s ))"
paddock-host device wait 1h --timeout 1 >/dev/null 2>&1; echo "STILL_EXIT $?"
exec 3<>/dev/paddock/wait; printf '{"until":"1h","timeout":1}' >&3
if cat <&3 >/dev/null 2>/tmp/err; then echo RAW_OK; else echo "RAW_ERR $(cat /tmp/err)"; fi
exec 3<&-
paddock-host device ask "x" --timeout 1 >/dev/null 2>&1; echo "ASK_OFF_EXIT $?"
"#;

struct World {
    _tmp: tempfile::TempDir,
    root: PathBuf,
}

fn world(config: &str, name: &str, script: &str) -> World {
    let base = std::env::var("HOME").map(PathBuf::from).unwrap();
    let tmp = tempfile::tempdir_in(base).unwrap();
    let root = tmp.path().to_path_buf();
    let cfg = root.join("cfg/paddock");
    std::fs::create_dir_all(cfg.join("harness.d")).unwrap();
    std::fs::create_dir_all(root.join("run")).unwrap();
    std::fs::create_dir_all(root.join("project")).unwrap();
    std::fs::write(cfg.join("config.toml"), config).unwrap();
    std::fs::write(
        cfg.join(format!("harness.d/{name}.toml")),
        format!(
            "name = \"{name}\"\n[locate]\ncommands = [\"sh\"]\n[launch]\nargs = [\"-c\", {}]\n",
            serde_json::to_string(script).unwrap()
        ),
    )
    .unwrap();
    World { _tmp: tmp, root }
}

fn paddock(w: &World) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_paddock"));
    c.env("XDG_CONFIG_HOME", w.root.join("cfg"))
        .env("XDG_DATA_HOME", w.root.join("data"))
        .env("XDG_STATE_HOME", w.root.join("state"))
        .env("XDG_CACHE_HOME", w.root.join("cache"))
        .env("XDG_RUNTIME_DIR", w.root.join("run"))
        // No desktop notification from a test.
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
        // English error texts from the tools in the sandbox.
        .env("LANG", "C")
        .env("LC_ALL", "C");
    c
}

fn run(w: &World, name: &str) -> std::process::Child {
    paddock(w)
        .args([
            "run",
            name,
            "--backend",
            "bwrap",
            "--network",
            "none",
            "--env",
            "clean",
            "-y",
        ])
        .arg(w.root.join("project"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn can_run() -> bool {
    if !bwrap_works() {
        eprintln!("skipped: bwrap cannot create a user namespace here");
        return false;
    }
    if !fuse_works() {
        eprintln!("skipped: no FUSE on this host");
        return false;
    }
    true
}

#[test]
fn a_wait_device_returns_at_its_time_and_timeouts_say_still_waiting() {
    if !can_run() {
        return;
    }
    // wait is on; ask stays off.
    let w = world("[devices]\nwait = true\n", "waitfake", SCRIPT);
    let out = run(&w, "waitfake").wait_with_output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{text}\n{err}");
    if text.contains("NODEV") {
        eprintln!("skipped: the sandbox has no /dev/paddock here: {err}");
        return;
    }
    let line = |k: &str| -> String {
        text.lines()
            .find_map(|l| l.strip_prefix(&format!("{k} ")))
            .unwrap_or_else(|| panic!("no {k} in:\n{text}\n{err}"))
            .to_string()
    };
    // kv watch: the time passed, nothing changed.
    assert_eq!(line("WATCH_EXIT"), "75");
    // The wait device returned at the time, not before and not much later.
    assert_eq!(line("WAIT_EXIT"), "0", "{text}");
    assert_eq!(line("WAIT_OUT"), format!("reached {}", line("WAIT_AT")));
    let elapsed: u64 = line("ELAPSED").parse().unwrap();
    assert!((2..=6).contains(&elapsed), "waited {elapsed} s");
    // A read timeout before the time: still waiting, 75 from paddock-host
    // and ETIME ("Timer expired") for a plain read.
    assert_eq!(line("STILL_EXIT"), "75");
    let raw = line("RAW_ERR");
    assert!(raw.contains("Timer expired"), "{raw}");
    // A device that the user did not turn on is not there.
    assert_eq!(line("ASK_OFF_EXIT"), "4");
}

/// Asks with a short read timeout in a loop, the way the skill tells agents
/// to, and prints the answer.
const ASKER: &str = r#"
if [ ! -e /dev/paddock/ask ]; then echo NODEV; exit 0; fi
n=0
while :; do
  a=$(paddock-host device ask "Which branch?" --choice main --choice dev --timeout 2); e=$?
  [ "$e" = 75 ] || break
  n=$((n + 1))
done
echo "ASK_EXIT $e"
echo "ROUNDS $n"
echo "ANSWER $a"
"#;

#[test]
fn a_question_waits_for_the_answer_of_the_user_across_reads() {
    if !can_run() {
        return;
    }
    let w = world("[devices]\nask = true\n", "askfake", ASKER);
    let child = run(&w, "askfake");
    // The question appears with its choices; answer it after a few reads.
    let start = std::time::Instant::now();
    let id = loop {
        let out = paddock(&w).args(["approvals", "--json"]).output().unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
        if let Some(p) = v["pending"].as_array().and_then(|a| a.first()) {
            assert_eq!(p["capability"], "ask");
            assert_eq!(p["detail"], "Which branch?");
            assert_eq!(p["choices"], serde_json::json!(["main", "dev"]));
            break p["id"].as_str().unwrap().to_string();
        }
        assert!(
            start.elapsed() < std::time::Duration::from_secs(60),
            "the question did not appear"
        );
        std::thread::sleep(std::time::Duration::from_millis(200));
    };
    std::thread::sleep(std::time::Duration::from_secs(5));
    // Still one card: each new read found the open question.
    let out = paddock(&w).args(["approvals", "--json"]).output().unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["pending"].as_array().unwrap().len(), 1, "{v}");
    let out = paddock(&w)
        .args(["approvals", "answer", &id, "--text", "dev, please"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let out = child.wait_with_output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    if text.contains("NODEV") {
        eprintln!("skipped: the sandbox has no /dev/paddock here");
        return;
    }
    assert!(text.contains("ASK_EXIT 0"), "{text}");
    assert!(text.contains("ANSWER dev, please"), "{text}");
    let rounds: u32 = text
        .lines()
        .find_map(|l| l.strip_prefix("ROUNDS "))
        .unwrap()
        .parse()
        .unwrap();
    assert!(rounds >= 1, "{text}");
    // The audit log has sizes, never the question or the answer.
    let log = std::fs::read_to_string(w.root.join("state/paddock/host-audit.jsonl")).unwrap();
    assert!(log.contains("\"capability\":\"ask\""), "{log}");
    assert!(
        !log.contains("Which branch") && !log.contains("please"),
        "{log}"
    );
}
