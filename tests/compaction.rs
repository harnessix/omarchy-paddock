//! The hook after a context compaction, end to end in bubblewrap, with fake
//! harnesses: a parent starts a child. The child's profile puts a hook into
//! a settings file of its sandbox home, and the child's fake "compaction"
//! runs that hook twice. The note names the task file and the role, and the
//! parent reads `compacted:<SPAWN-ID>` in the tree scope with a count of 2.
//! Skipped where the host cannot run bubblewrap.

mod common;

use std::path::Path;
use std::process::{Command, Stdio};

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@localhost"])
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

fn toml_str(s: &str) -> String {
    serde_json::to_string(s).unwrap()
}

#[test]
fn a_child_compaction_gives_a_note_and_reaches_the_parent() {
    if !common::bwrap_works() {
        eprintln!("skipped: bwrap cannot create a user namespace here");
        return;
    }
    let tmp = common::tempdir();
    let root = tmp.path();
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let cfg = root.join("cfg/paddock");
    std::fs::create_dir_all(cfg.join("harness.d")).unwrap();
    std::fs::create_dir_all(root.join("run")).unwrap();
    std::fs::write(
        cfg.join("config.toml"),
        "[host]\nspawn = \"allow\"\n[spawn]\nharnesses = [\"compactfake\"]\ntimeout = 120\n[compaction]\nnote = \"NOTE-FROM-CONFIG\"\n",
    )
    .unwrap();
    let parent =
        "id=$(paddock-host spawn compactfake --prompt-file task.md --reason test) || exit 11; \
                  paddock-host wait \"$id\" --timeout 100 >/dev/null; \
                  echo \"RECORD $(paddock-host kv get compacted:$id --scope tree)\"";
    std::fs::write(
        cfg.join("harness.d/parentfake.toml"),
        format!(
            "name = \"parentfake\"\n[locate]\ncommands = [\"sh\"]\n[launch]\nargs = [\"-c\", {}]\n",
            toml_str(parent)
        ),
    )
    .unwrap();
    // The fake compaction: run the hook command from the settings file,
    // with the input that Claude Code gives a SessionStart hook.
    let child = "cat >/dev/null; \
                 cmd=$(sed -n 's/.*\"command\": \"\\([^\"]*\\)\".*/\\1/p' \"$HOME/.fake/settings.json\"); \
                 for n in 1 2; do \
                   echo '{\"hook_event_name\":\"SessionStart\",\"source\":\"compact\",\"session_id\":\"s\"}' | sh -c \"$cmd\"; \
                 done";
    std::fs::write(
        cfg.join("harness.d/compactfake.toml"),
        format!(
            "name = \"compactfake\"\n[locate]\ncommands = [\"sh\"]\n\
             [oneshot]\nargs = [\"-c\", {}]\nprompt = \"stdin\"\n\
             [[compaction.settings]]\nfile = \".fake/settings.json\"\npath = [\"hooks\", \"compact\"]\nappend = true\n\
             value = {{ command = \"paddock-host compacted --harness claude\" }}\n",
            toml_str(child)
        ),
    )
    .unwrap();
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README"), "x\n").unwrap();
    git(&repo, &["add", "README"]);
    git(&repo, &["commit", "-q", "-m", "one"]);
    std::fs::write(repo.join("task.md"), "the task\n").unwrap();
    let home_before = std::fs::read_dir(&home).unwrap().count();

    let out = Command::new(env!("CARGO_BIN_EXE_paddock"))
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", root.join("cfg"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_RUNTIME_DIR", root.join("run"))
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
        .arg(&repo)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "parent failed: {stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // The parent sees the record with the count of the second compaction.
    let record = stdout
        .lines()
        .find_map(|l| l.strip_prefix("RECORD "))
        .unwrap_or_else(|| panic!("no record: {stdout}"));
    assert!(record.contains("\"count\":2"), "{record}");
    // The child got the note, with its task file and role.
    let inbox = repo.join(".paddock/inbox");
    let dir = std::fs::read_dir(&inbox)
        .unwrap()
        .flatten()
        .find(|e| e.file_name().to_string_lossy().starts_with("spawn-"))
        .expect("no spawn results in the inbox")
        .path();
    let output = std::fs::read_to_string(dir.join("output.txt")).unwrap();
    assert!(output.contains("additionalContext"), "{output}");
    assert!(output.contains("role: implementer"), "{output}");
    assert!(output.contains("/prompt.md. Read it again."), "{output}");
    assert!(output.contains("compaction 2"), "{output}");
    assert!(output.contains("NOTE-FROM-CONFIG"), "{output}");
    // The host home of the test got no hook and no settings.
    assert_eq!(std::fs::read_dir(&home).unwrap().count(), home_before);
}
