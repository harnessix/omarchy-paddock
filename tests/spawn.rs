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
        "[host]\nspawn = \"allow\"\n[spawn]\nharnesses = [\"childfake\", \"childsleep\", \"childreview\"]\ntimeout = 120\n",
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
    // Reports whether it can write its files, and reads the parent's draft.
    let review = "cat >/dev/null; if touch probe-write 2>/dev/null; then echo WRITABLE; \
                  else echo READONLY; fi; cat draft.txt";
    std::fs::write(
        cfg.join("harness.d/childreview.toml"),
        profile(
            "childreview",
            &format!(
                "[oneshot]\nargs = [\"-c\", {}]\nprompt = \"stdin\"\n",
                toml_str(review)
            ),
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

fn inbox_result(w: &World) -> (serde_json::Value, String) {
    let dir = std::fs::read_dir(w.repo.join(".paddock/inbox"))
        .unwrap()
        .flatten()
        .find(|e| e.file_name().to_string_lossy().starts_with("spawn-"))
        .expect("no spawn results in the inbox")
        .path();
    let result = serde_json::from_slice(&std::fs::read(dir.join("result.json")).unwrap()).unwrap();
    let output = std::fs::read_to_string(dir.join("output.txt")).unwrap();
    (result, output)
}

#[test]
fn a_reviewer_reads_the_parents_uncommitted_files_and_cannot_write() {
    if !bwrap_works() {
        eprintln!("skipped: bwrap cannot create a user namespace here");
        return;
    }
    let w = world(
        "echo draft-content-9 > draft.txt; \
         id=$(paddock-host spawn childreview --role reviewer --from-head --prompt-file task.md) || exit 11; \
         paddock-host wait \"$id\" --timeout 100",
    );
    let out = start_parent(&w).wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "parent failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let (result, output) = inbox_result(&w);
    assert_eq!(result["role"], "reviewer", "{result}");
    assert!(result["from_head"].is_string(), "{result}");
    assert_eq!(result["commits"], 0, "{result}");
    // The child saw the uncommitted file and could not write.
    assert!(output.contains("READONLY"), "{output}");
    assert!(output.contains("draft-content-9"), "{output}");
    // The parent's file is still uncommitted, and its branch did not move.
    assert!(git(&w.repo, &["status", "--porcelain"]).contains("?? draft.txt"));
    assert_eq!(
        git(&w.repo, &["log", "--oneline", "main"]).lines().count(),
        1
    );
}

#[test]
fn a_child_stops_at_its_time_limit_and_the_log_says_why() {
    if !bwrap_works() {
        eprintln!("skipped: bwrap cannot create a user namespace here");
        return;
    }
    let w = world(
        "id=$(paddock-host spawn childsleep --timeout 3 --prompt-file task.md) || exit 11; \
         paddock-host wait \"$id\" --timeout 100; echo \"wait exit $?\"",
    );
    let start = Instant::now();
    let out = start_parent(&w).wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}");
    // wait reports a child that did not finish well with exit code 1.
    assert!(stdout.contains("wait exit 1"), "{stdout}");
    assert!(start.elapsed() < Duration::from_secs(60));
    let (result, _) = inbox_result(&w);
    assert_eq!(result["state"], "timed-out", "{result}");
    let log = std::fs::read_to_string(w.root.join("state/paddock/host-audit.jsonl")).unwrap();
    let end = log
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| v["decision"] == "child ended")
        .unwrap();
    assert_eq!(end["state"], "timed-out", "{end}");
    assert!(
        end["message"]
            .as_str()
            .unwrap()
            .contains("time limit of 3 seconds"),
        "{end}"
    );
    assert_eq!(end["limits"]["timeout"], 3);
}

/// With `systemd-run --user`, the child's host process and its sandbox run in
/// one scope with the requested limits. Skipped without a user manager. This
/// test uses the real runtime folder, because the user manager lives there.
#[test]
fn a_systemd_scope_holds_the_child_and_its_limits() {
    if !bwrap_works() {
        eprintln!("skipped: bwrap cannot create a user namespace here");
        return;
    }
    let scope_works = Command::new("systemd-run")
        .args(["--user", "--scope", "--quiet", "--collect", "--", "true"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !scope_works {
        eprintln!("skipped: systemd-run --user --scope does not work here");
        return;
    }
    let w = world(
        "id=$(paddock-host spawn childsleep --timeout 20 --mem 256M --cpus 0.5 --prompt-file task.md) || exit 11; \
         sleep 300",
    );
    let real = |w: &World| {
        let mut c = paddock(w);
        c.env_remove("XDG_RUNTIME_DIR");
        c
    };
    let mut parent = real(&w)
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
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let ours = |s: &serde_json::Value| {
        s["project"]
            .as_str()
            .is_some_and(|p| p.starts_with(&w.root.display().to_string()))
    };
    let list = |w: &World| -> Vec<serde_json::Value> {
        let out = real(w).args(["ps", "--json"]).output().unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
        v["sandboxes"].as_array().cloned().unwrap_or_default()
    };
    let child = wait_for("the child sandbox", 90, || {
        list(&w)
            .into_iter()
            .find(|s| ours(s) && s["parent"].is_string())
    });
    let cgroup =
        |pid: u64| std::fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap_or_default();
    let host = child["host_pid"].as_u64().unwrap();
    let sandbox = child["pid"].as_u64().unwrap();
    let id = child["spawn_id"].as_str().unwrap().to_string();
    let unit = format!("paddock-spawn-{id}.scope");
    let in_scope = cgroup(host).contains(&unit) && cgroup(sandbox).contains(&unit);
    let props = Command::new("systemctl")
        .args([
            "--user",
            "show",
            &unit,
            "-p",
            "MemoryMax",
            "-p",
            "CPUQuotaPerSecUSec",
        ])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    let parent_id = child["parent"].as_str().unwrap().to_string();
    let _ = real(&w).args(["stop", &parent_id]).output();
    wait_for("the parent to exit", 60, || parent.try_wait().unwrap());
    assert!(in_scope, "{unit}: {} / {}", cgroup(host), cgroup(sandbox));
    assert!(props.contains("MemoryMax=268435456"), "{props}");
    assert!(props.contains("CPUQuotaPerSecUSec=500ms"), "{props}");
    wait_for("the child to end", 30, || {
        list(&w).into_iter().all(|s| !ours(&s)).then_some(())
    });
}
