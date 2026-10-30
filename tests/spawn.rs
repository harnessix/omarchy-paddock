//! `paddock-host spawn` end to end, in bubblewrap, with fake harnesses: a
//! parent sandbox starts a child sandbox, the child commits on its own branch,
//! and the results reach the parent's inbox. Stopping the parent stops the
//! child. Skipped where the host cannot run bubblewrap.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn bwrap_works() -> bool {
    Command::new("bwrap")
        .args(["--unshare-user", "--ro-bind", "/", "/", "true"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

const TASK: &str = "write-the-note-42";

struct World {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    repo: PathBuf,
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@localhost"])
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn profile(name: &str, body: &str) -> String {
    format!("name = \"{name}\"\n[locate]\ncommands = [\"sh\"]\n{body}")
}

fn toml_str(s: &str) -> String {
    serde_json::to_string(s).unwrap()
}

fn world(parent_script: &str) -> World {
    let base = std::env::var("HOME").map(PathBuf::from).unwrap();
    let tmp = tempfile::tempdir_in(base).unwrap();
    let root = tmp.path().to_path_buf();
    let cfg = root.join("cfg/paddock");
    std::fs::create_dir_all(cfg.join("harness.d")).unwrap();
    std::fs::create_dir_all(root.join("run")).unwrap();
    std::fs::write(
        cfg.join("config.toml"),
        "[host]\nspawn = \"allow\"\n[spawn]\nharnesses = [\"childfake\", \"childsleep\"]\ntimeout = 120\n",
    )
    .unwrap();
    std::fs::write(
        cfg.join("harness.d/parentfake.toml"),
        profile(
            "parentfake",
            &format!("[launch]\nargs = [\"-c\", {}]\n", toml_str(parent_script)),
        ),
    )
    .unwrap();
    // Reads the task on stdin, commits a file and prints a final answer.
    let child = "read -r line; printf '%s\\n' \"$line\" > note.txt; git add note.txt; \
                 git -c user.name=t -c user.email=t@localhost commit -q -m 'child work'; \
                 echo \"FINAL ANSWER: $line\"";
    std::fs::write(
        cfg.join("harness.d/childfake.toml"),
        profile(
            "childfake",
            &format!(
                "[oneshot]\nargs = [\"-c\", {}]\nprompt = \"stdin\"\n",
                toml_str(child)
            ),
        ),
    )
    .unwrap();
    std::fs::write(
        cfg.join("harness.d/childsleep.toml"),
        profile(
            "childsleep",
            "[oneshot]\nargs = [\"-c\", \"cat >/dev/null; sleep 300\"]\nprompt = \"stdin\"\n",
        ),
    )
    .unwrap();
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README"), "x\n").unwrap();
    git(&repo, &["add", "README"]);
    git(&repo, &["commit", "-q", "-m", "one"]);
    std::fs::write(repo.join("task.md"), format!("{TASK}\n")).unwrap();
    World {
        _tmp: tmp,
        root,
        repo,
    }
}

fn paddock(w: &World) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_paddock"));
    c.env("XDG_CONFIG_HOME", w.root.join("cfg"))
        .env("XDG_DATA_HOME", w.root.join("data"))
        .env("XDG_STATE_HOME", w.root.join("state"))
        .env("XDG_CACHE_HOME", w.root.join("cache"))
        .env("XDG_RUNTIME_DIR", w.root.join("run"));
    c
}

fn start_parent(w: &World) -> Child {
    paddock(w)
        .args([
            "run",
            "parentfake",
            "--backend",
            "bwrap",
            "--network",
            "none",
            "--env",
            "clean",
            "-y",
        ])
        .arg(&w.repo)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn ps(w: &World) -> Vec<serde_json::Value> {
    let out = paddock(w).args(["ps", "--json"]).output().unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
    v["sandboxes"].as_array().cloned().unwrap_or_default()
}

fn wait_for<T>(what: &str, secs: u64, mut f: impl FnMut() -> Option<T>) -> T {
    let start = Instant::now();
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(
            start.elapsed() < Duration::from_secs(secs),
            "timed out waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(300));
    }
}

#[test]
fn a_child_runs_in_its_own_sandbox_and_returns_results_to_the_inbox() {
    if !bwrap_works() {
        eprintln!("skipped: bwrap cannot create a user namespace here");
        return;
    }
    let w = world(
        "id=$(paddock-host spawn childfake --worktree --prompt-file task.md --reason test) || exit 11; \
         paddock-host wait \"$id\" --timeout 100",
    );
    let out = start_parent(&w).wait_with_output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "parent failed: {}\n{err}",
        String::from_utf8_lossy(&out.stdout)
    );
    let inbox = w.repo.join(".paddock/inbox");
    let dir = std::fs::read_dir(&inbox)
        .unwrap()
        .flatten()
        .find(|e| e.file_name().to_string_lossy().starts_with("spawn-"))
        .expect("no spawn results in the inbox")
        .path();
    let result: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("result.json")).unwrap()).unwrap();
    assert_eq!(result["state"], "exited", "{result}");
    assert_eq!(result["exit_code"], 0);
    assert_eq!(result["commits"], 1, "{result}");
    let id = result["id"].as_str().unwrap();
    let branch = result["branch"].as_str().unwrap();
    assert_eq!(branch, format!("paddock/spawn-childfake-{id}"));
    assert!(result["diff_stat"].as_str().unwrap().contains("note.txt"));
    let output = std::fs::read_to_string(dir.join("output.txt")).unwrap();
    assert!(
        output.contains(&format!("FINAL ANSWER: {TASK}")),
        "{output}"
    );
    // The child's commit is on its branch; the parent's checkout is untouched.
    assert!(git(&w.repo, &["log", "--oneline", branch]).contains("child work"));
    assert_eq!(git(&w.repo, &["branch", "--show-current"]).trim(), "main");
    assert!(!w.repo.join("note.txt").exists());
    // The audit log names the chain and never holds the task.
    let log = std::fs::read_to_string(w.root.join("state/paddock/host-audit.jsonl")).unwrap();
    assert!(!log.contains(TASK), "{log}");
    assert!(log.contains("\"child ended\""), "{log}");
    assert!(log.contains(&format!("childfake-spawn-{id}")), "{log}");
    // Nothing keeps running.
    assert!(ps(&w).is_empty());
}

#[test]
fn stopping_the_parent_stops_the_child() {
    if !bwrap_works() {
        eprintln!("skipped: bwrap cannot create a user namespace here");
        return;
    }
    let w = world("paddock-host spawn childsleep --prompt-file task.md || exit 11; sleep 300");
    let mut parent = start_parent(&w);
    let (parent_id, child) = wait_for("the child sandbox", 90, || {
        let list = ps(&w);
        let child = list.iter().find(|s| s["parent"].is_string())?.clone();
        Some((child["parent"].as_str()?.to_string(), child))
    });
    // ps lists the child right after its parent.
    let list = ps(&w);
    let pos = |id: &str| list.iter().position(|s| s["id"] == id).unwrap();
    assert_eq!(pos(child["id"].as_str().unwrap()), pos(&parent_id) + 1);
    let child_host = child["host_pid"].as_u64().unwrap() as i32;

    let out = paddock(&w).args(["stop", &parent_id]).output().unwrap();
    assert!(out.status.success(), "{out:?}");
    wait_for("both sandboxes to end", 60, || {
        ps(&w).is_empty().then_some(())
    });
    wait_for("the parent to exit", 60, || parent.try_wait().unwrap());
    // The child's own paddock process is gone too: no orphaned quota use.
    wait_for("the child's host process to exit", 30, || {
        (unsafe { libc::kill(child_host, 0) } != 0).then_some(())
    });
    let inbox = w.repo.join(".paddock/inbox");
    let dir = std::fs::read_dir(&inbox)
        .unwrap()
        .flatten()
        .find(|e| e.file_name().to_string_lossy().starts_with("spawn-"))
        .expect("no spawn results in the inbox")
        .path();
    let result: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("result.json")).unwrap()).unwrap();
    assert_eq!(result["state"], "stopped", "{result}");
}
