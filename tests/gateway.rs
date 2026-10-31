//! The gateway `/dev/paddock/gw` end to end, in a bwrap sandbox: a host
//! request goes through the device, a read that reaches its time limit says
//! "still waiting", the same request sent again attaches to the one that
//! waits (one card, one run), and `paddock-host` gets the result through
//! the gateway. Nothing here talks to the network.

mod common;

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

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

/// In the sandbox: the same exec request first as a raw device read that
/// gives up after one second, then through paddock-host.
const SCRIPT: &str = r#"
if [ ! -e /dev/paddock/gw ]; then echo NOGW; exit 0; fi
exec 3<>/dev/paddock/gw
printf '{"v":1,"cap":"exec","args":{"argv":["sh","-c","echo ran >> ran.log; echo hello"]},"reason":"","timeout":1}' >&3
if cat <&3 >/dev/null 2>/tmp/err; then echo RAW_OK; else echo "RAW_ERR $(cat /tmp/err)"; fi
exec 3<&-
out=$(paddock-host exec -- sh -c 'echo ran >> ran.log; echo hello'); e=$?
echo "EXEC_EXIT $e"
echo "EXEC_OUT $out"
"#;

struct World {
    _tmp: tempfile::TempDir,
    root: PathBuf,
}

fn world() -> World {
    let tmp = common::tempdir();
    let root = tmp.path().to_path_buf();
    let cfg = root.join("cfg/paddock");
    std::fs::create_dir_all(cfg.join("harness.d")).unwrap();
    std::fs::create_dir_all(root.join("run")).unwrap();
    std::fs::create_dir_all(root.join("project")).unwrap();
    std::fs::write(cfg.join("config.toml"), "").unwrap();
    std::fs::write(
        cfg.join("harness.d/gwfake.toml"),
        format!(
            "name = \"gwfake\"\n[locate]\ncommands = [\"sh\"]\n[launch]\nargs = [\"-c\", {}]\n",
            serde_json::to_string(SCRIPT).unwrap()
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
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
        .env("LANG", "C")
        .env("LC_ALL", "C");
    c
}

fn pending(w: &World) -> Vec<serde_json::Value> {
    let out = paddock(w).args(["approvals", "--json"]).output().unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
    v["pending"].as_array().cloned().unwrap_or_default()
}

#[test]
fn a_host_request_goes_through_the_gateway_and_attaches_when_sent_again() {
    if !bwrap_works() || !fuse_works() {
        eprintln!("skipped: bwrap or FUSE does not work here");
        return;
    }
    let w = world();
    let child = paddock(&w)
        .args([
            "run",
            "gwfake",
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
        .unwrap();
    // The card appears, and stays one card while the raw read gives up and
    // paddock-host sends the same request again.
    let start = Instant::now();
    let mut list = Vec::new();
    while start.elapsed() < Duration::from_secs(60) {
        list = pending(&w);
        if !list.is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if list.is_empty() {
        let out = child.wait_with_output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        if text.contains("NOGW") {
            eprintln!("skipped: the sandbox has no /dev/paddock/gw here");
            return;
        }
        panic!("no card:\n{text}\n{}", String::from_utf8_lossy(&out.stderr));
    }
    std::thread::sleep(Duration::from_secs(3));
    let list = pending(&w);
    assert_eq!(list.len(), 1, "{list:?}");
    assert_eq!(list[0]["capability"], "exec");
    let id = list[0]["id"].as_str().unwrap().to_string();
    let ans = paddock(&w)
        .args(["approvals", "answer", &id, "once"])
        .output()
        .unwrap();
    assert!(ans.status.success(), "{ans:?}");

    let out = child.wait_with_output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{text}\n{err}");
    let line = |k: &str| -> String {
        text.lines()
            .find_map(|l| l.strip_prefix(&format!("{k} ")))
            .unwrap_or_else(|| panic!("no {k} in:\n{text}\n{err}"))
            .to_string()
    };
    // The raw read reached its time limit: still waiting.
    assert!(line("RAW_ERR").contains("Timer expired"), "{text}");
    assert_eq!(line("EXEC_EXIT"), "0", "{text}\n{err}");
    assert_eq!(line("EXEC_OUT"), "hello");
    // One approval, one run.
    let ran = std::fs::read_to_string(w.root.join("project/ran.log")).unwrap();
    assert_eq!(ran, "ran\n");
}
