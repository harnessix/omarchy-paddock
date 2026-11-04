//! The hook after a context compaction for Codex, Grok Build and agy, with
//! a fake harness for each of the three hook styles: direct (Codex), marked
//! and then put in on the next tool call (Grok Build), and found in the
//! transcript (agy). paddock writes the hooks into a sandbox home from the
//! built-in profile. The fake harness runs `paddock-host` from there with
//! the input that the real harness gives, against a broker of a spawn child
//! on a socket. The parent reads `compacted:abc` in the tree scope.
//!
//! No sandbox: the broker runs in this process, so the test also runs where
//! bubblewrap cannot.

mod common;

use paddock::env::HostEnv;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

struct Setup {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    env: HostEnv,
    /// The sandbox home, as paddock wrote it.
    home: PathBuf,
    parent: paddock::kv::Service,
    sock: PathBuf,
    bin: PathBuf,
}

fn kv_service(
    env: &HostEnv,
    project: &Path,
    harness: &str,
    id: &str,
    chain: Vec<String>,
) -> paddock::kv::Service {
    let root = paddock::kv::kv_root(env);
    paddock::kv::store::Store::open(&paddock::kv::shard::registry(&root))
        .unwrap()
        .register(
            id,
            harness,
            project,
            chain.last().map(String::as_str),
            None,
            paddock::state::now(),
        )
        .unwrap();
    let lineage = paddock::kv::Lineage {
        parent: chain.last().cloned(),
        chain,
        can_spawn: true,
    };
    paddock::kv::Service::new(
        env.clone(),
        paddock::kv::Identity {
            tree: lineage.tree_root(id),
            sandbox: id.into(),
            harness: harness.into(),
            project: project.to_path_buf(),
            pasture: None,
            parent: lineage.parent.clone(),
            chain: lineage.chain.clone(),
        },
        paddock::kv::KvCfg::default(),
        Arc::new(paddock::broker::DenyAll),
        std::time::Duration::from_secs(1),
    )
    .with_root(root)
}

fn setup(harness: &str) -> Setup {
    let tmp = common::tempdir();
    let root = tmp.path().to_path_buf();
    let host_home = root.join("home");
    let project = root.join("proj");
    std::fs::create_dir_all(&host_home).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    let mut vars = std::collections::BTreeMap::new();
    for (k, d) in [
        ("XDG_CONFIG_HOME", "cfg"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_STATE_HOME", "state"),
        ("XDG_CACHE_HOME", "cache"),
        ("XDG_RUNTIME_DIR", "run"),
    ] {
        vars.insert(k.to_string(), root.join(d).display().to_string());
    }
    let env = HostEnv::from_parts(host_home, "t".into(), 1000, 1000, vars);
    std::fs::create_dir_all(env.paddock_config()).unwrap();
    std::fs::write(
        env.paddock_config().join("config.toml"),
        "[compaction]\nnote = \"NOTE-FROM-CONFIG\"\n",
    )
    .unwrap();

    // The sandbox home with the hooks of the built-in profile.
    let h = paddock::profile::Harness {
        profile: paddock::profile::builtin(harness).unwrap(),
        exe: "/usr/bin/true".into(),
        install_mounts: vec![],
        path_dirs: vec![],
        source: "test".into(),
    };
    let opts = paddock::build::Options {
        harness: harness.into(),
        project: Some(project.clone()),
        assume_yes: true,
        ..Default::default()
    };
    let b = paddock::build::build_with(&env, &opts, h).unwrap();
    paddock::privacy::write_settings(&b.spec.home_src, &b.spec.home_settings).unwrap();
    paddock::hooks::write(&b.spec).unwrap();
    let home = b.spec.home_src.clone();

    let parent = kv_service(&env, &project, "claude", "claude-parent-1", vec![]);
    let child = kv_service(
        &env,
        &project,
        harness,
        "x-spawn-abc",
        vec!["claude-parent-1".into()],
    );
    let ctx = paddock::broker::Context {
        id: "x-spawn-abc".into(),
        harness: harness.into(),
        project: project.clone(),
        repo: None,
        chain: vec!["claude-parent-1".into()],
        depth: 1,
        role: Some(paddock::spawn::Role::Implementer),
    };
    let mut broker = paddock::broker::Broker::new(
        env.clone(),
        ctx,
        paddock::broker::HostPolicy::default(),
        Arc::new(paddock::broker::DenyAll),
    );
    broker.run = paddock::broker::RunFacts {
        spawn_id: Some("abc".into()),
        pasture: None,
        task_file: Some("/home/demo/.paddock-spawn/x-spawn-abc/prompt.md".into()),
    };
    let _ = broker.kv.set(Arc::new(child));
    let sock = root.join("run/host.sock");
    paddock::broker::serve(&sock, Arc::new(broker)).unwrap();

    // `paddock-host` as the sandbox has it.
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let shim = bin.join("paddock-host");
    std::fs::write(
        &shim,
        format!(
            "#!/bin/sh\nexec '{}' host \"$@\"\n",
            env!("CARGO_BIN_EXE_paddock")
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
    Setup {
        _tmp: tmp,
        root,
        env,
        home,
        parent,
        sock,
        bin,
    }
}

/// Run the fake harness: a shell script in the sandbox home, with
/// `paddock-host` on PATH. It writes the hook outputs into files there.
fn fake_harness(s: &Setup, script: &str, extra: &[(&str, String)]) {
    let path = format!(
        "{}:{}",
        s.bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let out = Command::new("sh")
        .arg("-c")
        .arg(script)
        .current_dir(&s.home)
        .env_clear()
        .env("PATH", path)
        .env("HOME", &s.home)
        .env("PADDOCK_HOST_SOCKET", &s.sock)
        // No gateway device: in a paddock sandbox, /dev/paddock/gw would
        // take the requests to the real host.
        .env("PADDOCK_DEVICES", s.root.join("no-devices"))
        .envs(extra.iter().map(|(k, v)| (*k, v.as_str())))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "fake harness: {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn read(s: &Setup, name: &str) -> String {
    std::fs::read_to_string(s.home.join(name)).unwrap_or_default()
}

/// What the parent reads at `compacted:abc` in the tree scope.
fn parent_sees(s: &Setup) -> serde_json::Value {
    let r = s.parent.handle(
        &paddock::kv::Request {
            v: paddock::kv::PROTOCOL,
            op: "get".into(),
            scope: Some(paddock::kv::Scope::Tree),
            key: Some("compacted:abc".into()),
            ..Default::default()
        },
        None,
    );
    assert_eq!(r.status, paddock::kv::Status::Done, "{}", r.message);
    serde_json::from_str(r.result["value"].as_str().unwrap()).unwrap()
}

/// The note: the facts of the run and the text from the configuration.
fn assert_note(text: &str, n: u64) {
    assert!(
        text.contains(&format!("compaction {n} of this run")),
        "{text}"
    );
    assert!(text.contains("role: implementer"), "{text}");
    assert!(
        text.contains("x-spawn-abc/prompt.md. Read it again."),
        "{text}"
    );
    assert!(text.contains("NOTE-FROM-CONFIG"), "{text}");
}

/// The hook command, from the hooks file that paddock wrote.
const CMD: &str = r#"cmd=$(sed -n 's/.*"command": "\(paddock-host compacted[^"]*\)".*/\1/p' "$HOOKS" | head -n 1); [ -n "$cmd" ] || exit 9"#;

#[test]
fn codex_runs_its_trusted_hook_after_a_compaction() {
    let s = setup("codex");
    // Like Codex, the fake runs the hook only with the trusted hash under
    // the key of the hook: the hooks file's path in the sandbox (the host
    // home's path), the event, and the positions.
    let script = format!(
        r#"HOOKS="$HOME/.codex/hooks.json"; {CMD}
key="$KEY_HOME/.codex/hooks.json:session_start:0:0"
grep -qF "[hooks.state.\"$key\"]" .codex/config.toml || {{ echo untrusted; exit 3; }}
grep -qF 'trusted_hash = "sha256:371c933067cb6d5d4494380817fdb03cab5047e30dc69e36721fd913c6ec44e1"' .codex/config.toml || exit 4
for n in 1 2; do
  echo '{{"session_id":"019a","transcript_path":null,"cwd":"/w","hook_event_name":"SessionStart","model":"gpt-5.5-codex","permission_mode":"bypassPermissions","source":"compact"}}' | sh -c "$cmd" > "out$n.txt"
done
echo '{{"session_id":"019a","transcript_path":null,"cwd":"/w","hook_event_name":"SessionStart","model":"m","permission_mode":"default","source":"startup"}}' | sh -c "$cmd" > start.txt"#
    );
    fake_harness(
        &s,
        &script,
        &[("KEY_HOME", s.env.home.display().to_string())],
    );
    for n in [1, 2] {
        let v: serde_json::Value = serde_json::from_str(&read(&s, &format!("out{n}.txt"))).unwrap();
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "SessionStart");
        assert_note(
            v["hookSpecificOutput"]["additionalContext"]
                .as_str()
                .unwrap(),
            n,
        );
    }
    assert_eq!(read(&s, "start.txt"), "");
    assert_eq!(parent_sees(&s)["count"], 2);
    assert_eq!(paddock::compaction::count(&s.env, "x-spawn-abc"), 2);
}

#[test]
fn grok_gets_the_note_on_the_next_tool_call() {
    let s = setup("grok");
    let script = format!(
        r#"HOOKS="$HOME/.grok/hooks/paddock-compaction.json"; {CMD}
ev() {{ printf '{{"hookEventName":"%s","hook_event_name":"%s","sessionId":"s-1","session_id":"s-1","cwd":"/w","workspaceRoot":"/w","timestamp":"t"%s}}' "$1" "$2" "$3"; }}
ev post_tool_use PostToolUse ',"toolName":"read_file"' | sh -c "$cmd" > before.txt
ev post_compact PostCompact ',"source":"auto"' | sh -c "$cmd" > compact.txt
ev post_tool_use PostToolUse ',"toolName":"read_file"' | sh -c "$cmd" > tool1.txt
ev post_tool_use PostToolUse ',"toolName":"read_file"' | sh -c "$cmd" > tool2.txt"#
    );
    fake_harness(&s, &script, &[]);
    assert_eq!(read(&s, "before.txt"), "");
    assert_eq!(read(&s, "compact.txt"), "");
    // The parent knows at the compaction, before the note goes in.
    let v: serde_json::Value = serde_json::from_str(&read(&s, "tool1.txt")).unwrap();
    assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PostToolUse");
    assert_note(
        v["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap(),
        1,
    );
    assert_eq!(read(&s, "tool2.txt"), "");
    assert_eq!(parent_sees(&s)["count"], 1);
    assert_eq!(parent_sees(&s)["harness"], "grok");
}

#[test]
fn agy_finds_the_compaction_in_its_transcript() {
    let s = setup("agy");
    let script = format!(
        r#"HOOKS="$HOME/.gemini/config/hooks.json"; {CMD}
log="$HOME/.gemini/antigravity-cli/brain/c-1/.system_generated/logs"
mkdir -p "$log"
inv() {{ printf '{{"invocationNum":%s,"initialNumSteps":3,"conversationId":"c-1","workspacePaths":["/w"],"transcriptPath":"%s/transcript.jsonl","artifactDirectoryPath":"/a","modelName":"m"}}' "$1" "$log"; }}
echo '{{"type":"USER_INPUT"}}' >> "$log/transcript.jsonl"
inv 0 | sh -c "$cmd" > inv0.txt
echo '{{"type":"PLANNER_RESPONSE"}}' >> "$log/transcript.jsonl"
inv 1 | sh -c "$cmd" > inv1.txt
echo '{{"type":"CHECKPOINT","content":"title"}}' >> "$log/transcript.jsonl"
inv 2 | sh -c "$cmd" > plain.txt
echo '{{"type":"CHECKPOINT","content":"summary"}}' > "$log/transcript.jsonl"
inv 3 | sh -c "$cmd" > inv2.txt
inv 4 | sh -c "$cmd" > inv3.txt"#
    );
    fake_harness(&s, &script, &[]);
    assert_eq!(read(&s, "inv0.txt"), "");
    assert_eq!(read(&s, "inv1.txt"), "");
    // A checkpoint of a plain run is no compaction. agy writes the
    // transcript again with fewer lines when it compacts.
    assert_eq!(read(&s, "plain.txt"), "");
    let v: serde_json::Value = serde_json::from_str(&read(&s, "inv2.txt")).unwrap();
    assert_note(v["injectSteps"][0]["ephemeralMessage"].as_str().unwrap(), 1);
    assert_eq!(read(&s, "inv3.txt"), "");
    assert_eq!(parent_sees(&s)["count"], 1);
    assert_eq!(parent_sees(&s)["harness"], "agy");
}
