//! `paddock-host spawn` end to end, in bubblewrap, with fake harnesses: a
//! parent sandbox starts a child sandbox, the child commits on its own branch,
//! and the results reach the parent's inbox. Stopping the parent stops the
//! child. A parent hands tasks to two children through the tree scope of
//! paddock kv. Skipped where the host cannot run bubblewrap.

mod common;

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
    // Not in /tmp: the sandbox has its own /tmp.
    let tmp = common::tempdir();
    let root = tmp.path().to_path_buf();
    let cfg = root.join("cfg/paddock");
    std::fs::create_dir_all(cfg.join("harness.d")).unwrap();
    std::fs::create_dir_all(root.join("run")).unwrap();
    std::fs::write(
        cfg.join("config.toml"),
        "[host]\nspawn = \"allow\"\n[spawn]\nharnesses = [\"childfake\", \"childsleep\", \"childreview\", \"childkv\"]\ntimeout = 120\n",
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
    // Reads the parent's brief, tries the project scope, then takes the
    // first task in the tree scope that no other child holds.
    let kv = "cat >/dev/null; K='paddock-host kv'; \
              echo \"PARENT $($K get brief --scope parent)\"; \
              $K put note:x y --scope project >/dev/null 2>&1; echo \"PROJECT_EXIT $?\"; \
              for t in a b; do \
                $K claim $t --ttl 300 --scope tree >/dev/null 2>&1 || continue; \
                if $K get result:task-$t --scope tree >/dev/null 2>&1; then $K release $t --scope tree; continue; fi; \
                $K put result:task-$t \"done by $PADDOCK_ID\" --scope tree >/dev/null || exit 42; \
                echo \"CLAIMED $t\"; exit 0; \
              done; echo NOTHING";
    std::fs::write(
        cfg.join("harness.d/childkv.toml"),
        profile(
            "childkv",
            &format!(
                "[oneshot]\nargs = [\"-c\", {}]\nprompt = \"stdin\"\n",
                toml_str(kv)
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

/// The `fuse.paddock` mounts under a folder.
fn device_mounts(root: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string("/proc/self/mountinfo")
        .unwrap_or_default()
        .lines()
        .filter(|l| l.contains(" - fuse.paddock "))
        .filter_map(|l| l.split(' ').nth(4).map(str::to_string))
        .filter(|mp| mp.starts_with(&root.display().to_string()))
        .collect()
}

#[test]
fn a_sigterm_to_paddock_still_runs_its_exit_steps() {
    if !bwrap_works() {
        eprintln!("skipped: bwrap cannot create a user namespace here");
        return;
    }
    let w = world("sleep 300");
    let mut parent = start_parent(&w);
    let id = wait_for("the sandbox", 90, || {
        ps(&w)
            .first()
            .and_then(|s| s["id"].as_str().map(str::to_string))
    });
    let had_devices = !device_mounts(&w.root).is_empty();
    // SAFETY: a plain kill(2) on the paddock process of this test.
    unsafe { libc::kill(parent.id() as i32, libc::SIGTERM) };
    let status = wait_for("paddock to exit", 60, || parent.try_wait().unwrap());
    // paddock passed the signal on and ended with its sandbox, not killed by it.
    assert!(
        std::os::unix::process::ExitStatusExt::signal(&status).is_none(),
        "{status:?}"
    );
    assert!(ps(&w).is_empty());
    assert!(
        device_mounts(&w.root).is_empty(),
        "{:?}",
        device_mounts(&w.root)
    );
    if had_devices {
        eprintln!("the device mount of {id} was removed");
    }
}

/// What each child wrote, for the message of a failed test.
fn children_output(w: &World) -> String {
    let mut out = String::new();
    if let Ok(dir) = std::fs::read_dir(w.repo.join(".paddock/inbox")) {
        for e in dir.flatten() {
            for f in ["result.json", "output.txt"] {
                let text = std::fs::read_to_string(e.path().join(f)).unwrap_or_default();
                out.push_str(&format!(
                    "--- {}/{f}\n{text}\n",
                    e.file_name().to_string_lossy()
                ));
            }
        }
    }
    out
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

#[test]
fn children_claim_tasks_in_the_tree_scope_and_the_parent_watch_wakes() {
    if !bwrap_works() {
        eprintln!("skipped: bwrap cannot create a user namespace here");
        return;
    }
    let w = world(
        "K='paddock-host kv'; \
         $K put brief from-the-parent >/dev/null || exit 20; \
         $K put task:a '{\"status\":\"todo\"}' --scope tree >/dev/null || exit 21; \
         v=$($K put task:b '{\"status\":\"todo\"}' --scope tree | sed 's/^version //') || exit 22; \
         id1=$(paddock-host spawn childkv --prompt-file task.md) || exit 23; \
         id2=$(paddock-host spawn childkv --prompt-file task.md) || exit 24; \
         seen=0; \
         while [ $seen -lt 2 ]; do \
           out=$($K watch --prefix result:task- --version \"$v\" --scope tree --timeout 100) || exit 25; \
           echo \"$out\"; \
           v=$(echo \"$out\" | sed -n 's/^cursor //p'); \
           seen=$((seen + $(echo \"$out\" | grep -c '^put'))); \
         done; \
         echo WOKE; \
         paddock-host wait \"$id1\" --timeout 100 >/dev/null || exit 26; \
         paddock-host wait \"$id2\" --timeout 100 >/dev/null || exit 27; \
         echo \"R1 $($K get result:$id1 --scope tree)\"; \
         echo \"R2 $($K get result:$id2 --scope tree)\"",
    );
    let out = start_parent(&w).wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "parent failed: {stdout}\n{}\n{}",
        String::from_utf8_lossy(&out.stderr),
        children_output(&w)
    );
    // The watch woke on the children's results, not on a poll.
    assert!(stdout.contains("WOKE"), "{stdout}");
    assert!(stdout.contains("put\tresult:task-a"), "{stdout}");
    assert!(stdout.contains("put\tresult:task-b"), "{stdout}");
    // paddock wrote result:<spawn id> with the branch for each child.
    for tag in ["R1 ", "R2 "] {
        let line = stdout.lines().find(|l| l.starts_with(tag)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&line[3..]).unwrap();
        assert_eq!(v["status"], "done", "{line}");
        assert!(v["ref"]
            .as_str()
            .unwrap()
            .starts_with("paddock/spawn-childkv-"));
    }
    // Each child took a different task, read its parent's self scope and
    // has no pasture scope (`project` is its other name): exit 4, off.
    let mut claimed = Vec::new();
    for e in std::fs::read_dir(w.repo.join(".paddock/inbox"))
        .unwrap()
        .flatten()
    {
        let output = std::fs::read_to_string(e.path().join("output.txt")).unwrap();
        assert!(output.contains("PARENT from-the-parent"), "{output}");
        assert!(output.contains("PROJECT_EXIT 4"), "{output}");
        let t = output
            .lines()
            .find_map(|l| l.strip_prefix("CLAIMED "))
            .unwrap_or_else(|| panic!("no task claimed: {output}"));
        claimed.push(t.to_string());
    }
    claimed.sort();
    assert_eq!(claimed, ["a", "b"]);
    // The watch names the child that wrote each result.
    let writer = stdout
        .lines()
        .find_map(|l| l.split("by ").nth(1))
        .unwrap()
        .trim()
        .to_string();
    assert!(writer.contains("childkv-spawn-"), "{writer}");
    assert!(ps(&w).is_empty());
}
