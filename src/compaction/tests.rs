use super::*;

/// The event of a hook input that is a compaction with a direct note.
fn hook_event(harness: &str, input: &serde_json::Value) -> Option<String> {
    match action(harness, input)? {
        Action::Direct { event } => Some(event),
        _ => None,
    }
}

#[test]
fn the_configuration_has_defaults_and_rejects_unknown_keys() {
    assert_eq!(Cfg::parse("").unwrap(), Cfg::default());
    let c = Cfg::parse("[compaction]\nnote = \"Run the tests.\"\nnotify = true\n").unwrap();
    assert!(c.enabled && c.notify);
    assert_eq!(c.note, "Run the tests.");
    assert!(
        !Cfg::parse("[compaction]\nenabled = false\n")
            .unwrap()
            .enabled
    );
    assert!(Cfg::parse("[compaction]\nbogus = 1\n").is_err());
}

#[test]
fn the_note_names_role_parent_task_file_and_done() {
    let f = Facts {
        sandbox: "codex-spawn-abc".into(),
        harness: "codex".into(),
        role: Some("reviewer".into()),
        parent: Some("claude-x-1".into()),
        spawn_id: Some("abc".into()),
        pasture: None,
        task_file: Some("/home/demo/.paddock-spawn/codex-spawn-abc/prompt.md".into()),
        detached: true,
        note: "Keep the ticket rules.".into(),
        count: 2,
        spec: None,
    };
    let n = note(&f);
    assert!(n.contains("compaction 2"), "{n}");
    assert!(n.contains("role: reviewer"), "{n}");
    assert!(n.contains("parent: claude-x-1 (it knows this run as spawn id abc)"));
    assert!(n.contains("pasture: none"));
    assert!(n.contains(
        "task file: /home/demo/.paddock-spawn/codex-spawn-abc/prompt.md. Read it again."
    ));
    assert!(n.contains("paddock-host done"));
    assert!(n.ends_with("Keep the ticket rules."));
    let top = note(&Facts {
        sandbox: "s".into(),
        harness: "claude".into(),
        count: 1,
        ..Default::default()
    });
    assert!(top.contains("top-level run") && top.contains("parent: none"));
    assert!(top.contains("task file: none") && !top.contains("paddock-host done"));
}

#[test]
fn the_count_goes_up_per_run() {
    let t = tempfile::tempdir().unwrap();
    let env = HostEnv::from_parts(
        t.path().join("home"),
        "t".into(),
        1000,
        1000,
        Default::default(),
    );
    assert_eq!(count(&env, "run-1"), 0);
    assert_eq!(record(&env, "run-1").unwrap(), 1);
    assert_eq!(record(&env, "run-1").unwrap(), 2);
    assert_eq!(count(&env, "run-1"), 2);
    assert_eq!(count(&env, "run-2"), 0);
    assert!(record(&env, "../x").is_err());
}

/// A host world with every XDG folder outside the home, so a test can
/// check that the host home stays as it was.
pub(super) struct World {
    _tmp: tempfile::TempDir,
    pub(super) env: HostEnv,
    pub(super) project: std::path::PathBuf,
}

pub(super) fn world(config: &str) -> World {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let project = tmp.path().join("proj");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    let mut vars = std::collections::BTreeMap::new();
    for (k, d) in [
        ("XDG_CONFIG_HOME", "cfg"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_STATE_HOME", "state"),
        ("XDG_CACHE_HOME", "cache"),
        ("XDG_RUNTIME_DIR", "run"),
    ] {
        vars.insert(k.to_string(), tmp.path().join(d).display().to_string());
    }
    let env = HostEnv::from_parts(home, "t".into(), 1000, 1000, vars);
    std::fs::create_dir_all(env.paddock_config()).unwrap();
    std::fs::write(env.paddock_config().join("config.toml"), config).unwrap();
    World {
        _tmp: tmp,
        env,
        project,
    }
}

/// Every file under a folder with its bytes.
pub(super) fn tree(
    dir: &std::path::Path,
) -> std::collections::BTreeMap<std::path::PathBuf, Vec<u8>> {
    let mut out = std::collections::BTreeMap::new();
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                todo.push(p);
            } else {
                out.insert(p.clone(), std::fs::read(&p).unwrap_or_default());
            }
        }
    }
    out
}

/// Build a sandbox of a built-in profile, write its settings into the
/// sandbox home as a start does, and return the sandbox home.
pub(super) fn sandbox_home(w: &World, name: &str) -> std::path::PathBuf {
    sandbox_spec(w, name, &[]).home_src
}

/// `sandbox_home`, with `extra` host files copied in first as a snapshot of
/// the host data does. Returns the spec.
pub(super) fn sandbox_spec(w: &World, name: &str, extra: &[&str]) -> crate::spec::SandboxSpec {
    let h = crate::profile::Harness {
        profile: crate::profile::builtin(name).unwrap(),
        exe: "/usr/bin/true".into(),
        install_mounts: vec![],
        path_dirs: vec![],
        source: "test".into(),
    };
    let opts = crate::build::Options {
        harness: name.into(),
        project: Some(w.project.clone()),
        assume_yes: true,
        ..Default::default()
    };
    let b = crate::build::build_with(&w.env, &opts, h).unwrap();
    // A start copies the `seed` files first.
    let seed = crate::profile::builtin(name).unwrap().state.seed;
    for rel in seed.iter().map(String::as_str).chain(extra.iter().copied()) {
        if let Ok(bytes) = std::fs::read(w.env.home.join(rel)) {
            let dst = b.spec.home_src.join(rel);
            std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
            std::fs::write(dst, bytes).unwrap();
        }
    }
    crate::privacy::write_settings(&b.spec.home_src, &b.spec.home_settings).unwrap();
    b.spec
}

/// The hook command of each supported harness: its settings file in the
/// sandbox home, and where the command is in it.
fn hook_command(name: &str, home: &std::path::Path) -> Option<String> {
    let json = |rel: &str| -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(home.join(rel)).unwrap_or_default())
            .unwrap_or_default()
    };
    let s = |v: &serde_json::Value| v.as_str().map(str::to_string);
    match name {
        "claude" | "qwen" => {
            let v = json(settings_file(name));
            let e = v["hooks"]["SessionStart"]
                .as_array()?
                .iter()
                .find(|e| e["matcher"] == "compact")?
                .clone();
            s(&e["hooks"][0]["command"])
        }
        _ => None,
    }
}

const SUPPORTED: &[&str] = &["claude", "qwen"];

/// The settings file that holds the hook, relative to the home.
fn settings_file(name: &str) -> &'static str {
    match name {
        "claude" => ".claude/settings.json",
        _ => ".qwen/settings.json",
    }
}

#[test]
fn each_supported_harness_gets_its_hook_in_the_sandbox_home_only() {
    for name in SUPPORTED {
        let w = world("");
        // A hook of the user's own stays next to paddock's.
        let own = w.env.home.join(settings_file(name));
        std::fs::create_dir_all(own.parent().unwrap()).unwrap();
        std::fs::write(
            &own,
            r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"my-hook"}]}]}}"#,
        )
        .unwrap();
        let before = tree(&w.env.home);
        let home = sandbox_home(&w, name);
        assert!(!home.starts_with(&w.env.home), "{}", home.display());
        assert_eq!(
            hook_command(name, &home).as_deref(),
            Some(format!("paddock-host compacted --harness {name}").as_str()),
            "{name}"
        );
        // The host home is unchanged: no hook, no trust setting.
        assert_eq!(tree(&w.env.home), before, "{name}");
        let v: serde_json::Value =
            serde_json::from_slice(&std::fs::read(home.join(settings_file(name))).unwrap())
                .unwrap();
        assert_eq!(
            v["hooks"]["SessionStart"][0]["hooks"][0]["command"],
            "my-hook"
        );
        assert_eq!(v["hooks"]["SessionStart"].as_array().unwrap().len(), 2);
        // `[compaction] enabled = false`: no hook.
        let w = world("[compaction]\nenabled = false\n");
        let home = sandbox_home(&w, name);
        assert_eq!(hook_command(name, &home), None, "{name}");
    }
}

#[test]
fn claude_reads_the_note_from_its_session_start_hook() {
    // https://code.claude.com/docs/en/hooks: SessionStart input.
    let input = serde_json::json!({
        "session_id": "abc123",
        "transcript_path": "/home/demo/.claude/projects/p/abc123.jsonl",
        "cwd": "/work/app",
        "hook_event_name": "SessionStart",
        "source": "compact",
        "model": "claude-sonnet-4-6"
    });
    let ev = hook_event("claude", &input).unwrap();
    let out: serde_json::Value =
        serde_json::from_str(&render("claude", &ev, "NOTE").unwrap()).unwrap();
    assert_eq!(out["hookSpecificOutput"]["hookEventName"], "SessionStart");
    assert_eq!(out["hookSpecificOutput"]["additionalContext"], "NOTE");
    // A session start for another reason is no compaction.
    for source in ["startup", "resume", "clear"] {
        let mut i = input.clone();
        i["source"] = source.into();
        assert_eq!(hook_event("claude", &i), None, "{source}");
    }
}

/// The broker of a spawn child and the kv services of the child and its
/// parent, all in this process.
pub(super) struct Tree {
    pub(super) w: World,
    pub(super) parent: crate::kv::Service,
    pub(super) broker: crate::broker::Broker,
}

pub(super) fn spawn_tree(config: &str, harness: &str) -> Tree {
    use std::sync::Arc;
    let w = world(config);
    let root = crate::kv::kv_root(&w.env);
    let svc = |id: &str, chain: Vec<String>| {
        crate::kv::store::Store::open(&crate::kv::shard::registry(&root))
            .unwrap()
            .register(
                id,
                harness,
                &w.project,
                chain.last().map(String::as_str),
                None,
                crate::state::now(),
            )
            .unwrap();
        let lineage = crate::kv::Lineage {
            parent: chain.last().cloned(),
            chain,
            can_spawn: true,
        };
        crate::kv::Service::new(
            w.env.clone(),
            crate::kv::Identity {
                tree: lineage.tree_root(id),
                sandbox: id.into(),
                harness: harness.into(),
                project: w.project.clone(),
                pasture: None,
                parent: lineage.parent.clone(),
                chain: lineage.chain.clone(),
            },
            crate::kv::KvCfg::default(),
            Arc::new(crate::broker::DenyAll),
            std::time::Duration::from_secs(1),
        )
        .with_root(root.clone())
    };
    let parent = svc("claude-parent-1", vec![]);
    let child = svc("codex-spawn-abc", vec!["claude-parent-1".into()]);
    let ctx = crate::broker::Context {
        id: "codex-spawn-abc".into(),
        harness: harness.into(),
        project: w.project.clone(),
        repo: None,
        chain: vec!["claude-parent-1".into()],
        depth: 1,
        role: Some(crate::spawn::Role::Implementer),
    };
    let mut broker = crate::broker::Broker::new(
        w.env.clone(),
        ctx,
        crate::broker::HostPolicy::default(),
        Arc::new(crate::broker::DenyAll),
    );
    broker.run = crate::broker::RunFacts {
        spawn_id: Some("abc".into()),
        pasture: None,
        task_file: Some("/home/demo/.paddock-spawn/codex-spawn-abc/prompt.md".into()),
    };
    let _ = broker.kv.set(Arc::new(child));
    Tree { w, parent, broker }
}

pub(super) fn compacted(harness: &str) -> crate::broker::Request {
    crate::broker::Request {
        v: crate::broker::PROTOCOL,
        cap: "compacted".into(),
        args: [("harness".to_string(), serde_json::json!(harness))].into(),
        reason: String::new(),
    }
}

/// What the parent reads at `compacted:abc` in the tree scope.
pub(super) fn parent_sees(t: &Tree) -> Option<serde_json::Value> {
    let r = t.parent.handle(
        &crate::kv::Request {
            v: crate::kv::PROTOCOL,
            op: "get".into(),
            scope: Some(crate::kv::Scope::Tree),
            key: Some("compacted:abc".into()),
            ..Default::default()
        },
        None,
    );
    (r.status == crate::kv::Status::Done)
        .then(|| serde_json::from_str(r.result["value"].as_str()?).ok())
        .flatten()
}

#[test]
fn a_child_compaction_reaches_the_tree_scope_and_counts_up() {
    let t = spawn_tree(
        "[compaction]\nnote = \"Keep the ticket rules.\"\n",
        "claude",
    );
    assert_eq!(parent_sees(&t), None);
    let r = t.broker.handle(&compacted("claude"));
    assert_eq!(r.status, crate::broker::Status::Done, "{}", r.message);
    let f: Facts = serde_json::from_value(r.result.clone()).unwrap();
    assert_eq!(f.count, 1);
    assert_eq!(f.role.as_deref(), Some("implementer"));
    assert_eq!(f.parent.as_deref(), Some("claude-parent-1"));
    assert_eq!(f.note, "Keep the ticket rules.");
    let n = note(&f);
    assert!(
        n.contains("/home/demo/.paddock-spawn/codex-spawn-abc/prompt.md. Read it again."),
        "{n}"
    );
    assert!(n.contains("role: implementer"), "{n}");
    assert_eq!(r.result["kv"]["scope"], "tree");
    let seen = parent_sees(&t).unwrap();
    assert_eq!(seen["count"], 1);
    assert!(seen["time"].as_u64().unwrap() > 0);

    let r = t.broker.handle(&compacted("claude"));
    assert_eq!(r.result["count"], 2);
    assert_eq!(parent_sees(&t).unwrap()["count"], 2);
    assert_eq!(count(&t.w.env, "codex-spawn-abc"), 2);
    // One audit line per compaction, without the note.
    let log = std::fs::read_to_string(crate::broker::audit_file(&t.w.env)).unwrap();
    let lines: Vec<&str> = log
        .lines()
        .filter(|l| l.contains("\"compacted\""))
        .collect();
    assert_eq!(lines.len(), 2, "{log}");
    assert!(lines[1].contains("context compaction 2"), "{log}");
    assert!(!log.contains("ticket rules"), "{log}");
}

#[test]
fn compaction_notes_can_be_turned_off() {
    let t = spawn_tree("[compaction]\nenabled = false\n", "claude");
    let r = t.broker.handle(&compacted("claude"));
    assert_eq!(r.status, crate::broker::Status::Disabled);
    assert_eq!(parent_sees(&t), None);
    assert_eq!(count(&t.w.env, "codex-spawn-abc"), 0);
}

#[test]
fn qwen_reads_the_note_from_its_session_start_hook() {
    // https://github.com/QwenLM/qwen-code docs/users/features/hooks.md
    let input = serde_json::json!({
        "session_id": "s-1",
        "transcript_path": "/home/demo/.qwen/tmp/s-1.jsonl",
        "cwd": "/work/app",
        "hook_event_name": "SessionStart",
        "timestamp": "2026-11-03T12:00:00.000Z",
        "permission_mode": "default",
        "source": "compact",
        "model": "qwen3-coder"
    });
    let ev = hook_event("qwen", &input).unwrap();
    let out: serde_json::Value =
        serde_json::from_str(&render("qwen", &ev, "NOTE").unwrap()).unwrap();
    assert_eq!(out["hookSpecificOutput"]["hookEventName"], "SessionStart");
    assert_eq!(out["hookSpecificOutput"]["additionalContext"], "NOTE");
    let mut i = input.clone();
    i["source"] = "startup".into();
    assert_eq!(hook_event("qwen", &i), None);
    // A harness without a supported hook prints nothing.
    assert_eq!(hook_event("gemini", &input), None);
    assert_eq!(render("gemini", "PreCompress", "NOTE"), None);
}

/// The broker of a pasture member "claude-p-1" in pasture "team" and its kv
/// service.
pub(super) fn pasture_member(
    w: &World,
    harness: &str,
) -> (crate::broker::Broker, std::sync::Arc<crate::kv::Service>) {
    use std::sync::Arc;
    let root = crate::kv::kv_root(&w.env);
    crate::kv::store::Store::open(&crate::kv::shard::registry(&root))
        .unwrap()
        .register(
            "claude-p-1",
            harness,
            &w.project,
            None,
            Some("team"),
            crate::state::now(),
        )
        .unwrap();
    let id = crate::kv::Identity {
        tree: None,
        sandbox: "claude-p-1".into(),
        harness: harness.into(),
        project: w.project.clone(),
        pasture: Some("team".into()),
        parent: None,
        chain: vec![],
    };
    let svc = Arc::new(
        crate::kv::Service::new(
            w.env.clone(),
            id,
            crate::kv::KvCfg::default(),
            Arc::new(crate::broker::DenyAll),
            std::time::Duration::from_secs(1),
        )
        .with_root(root),
    );
    let ctx = crate::broker::Context {
        id: "claude-p-1".into(),
        harness: harness.into(),
        project: w.project.clone(),
        repo: None,
        chain: vec![],
        depth: 0,
        role: None,
    };
    let mut b = crate::broker::Broker::new(
        w.env.clone(),
        ctx,
        crate::broker::HostPolicy::default(),
        Arc::new(crate::broker::DenyAll),
    );
    b.run.pasture = Some("team".into());
    let _ = b.kv.set(svc.clone());
    (b, svc)
}

/// The record at `compacted:claude-p-1` in the pasture scope.
pub(super) fn pasture_sees(svc: &crate::kv::Service) -> Option<serde_json::Value> {
    let r = svc.handle(
        &crate::kv::Request {
            v: crate::kv::PROTOCOL,
            op: "get".into(),
            scope: Some(crate::kv::Scope::Pasture),
            key: Some("compacted:claude-p-1".into()),
            ..Default::default()
        },
        None,
    );
    (r.status == crate::kv::Status::Done)
        .then(|| serde_json::from_str(r.result["value"].as_str()?).ok())
        .flatten()
}

#[test]
fn a_pasture_member_records_its_compaction_in_the_pasture() {
    let w = world("");
    let (b, svc) = pasture_member(&w, "claude");
    let r = b.handle(&compacted("claude"));
    assert_eq!(r.result["kv"]["scope"], "pasture", "{:?}", r);
    assert_eq!(r.result["role"], serde_json::Value::Null);
    assert_eq!(pasture_sees(&svc).unwrap()["count"], 1);
}

#[test]
fn the_note_names_the_spec_kit_task_of_a_worker() {
    let f = Facts {
        sandbox: "codex-spawn-abc".into(),
        harness: "codex".into(),
        role: Some("implementer".into()),
        count: 1,
        spec: Some(crate::speckit::prompt::Facts {
            feature: "001-albums".into(),
            task: Some("T012 Create Album model in src/models/album.py".into()),
            acceptance:
                "Independent Test: drag a photo\n1. Given an album, When I drop, Then it moves"
                    .into(),
            paths: vec![
                "specs/001-albums/spec.md".into(),
                "specs/001-albums/plan.md".into(),
            ],
            summary: None,
        }),
        ..Default::default()
    };
    let n = note(&f);
    assert!(n.contains("Spec Kit feature: 001-albums"), "{n}");
    assert!(n.contains("your task: T012 Create Album model"), "{n}");
    assert!(
        n.contains("acceptance: Independent Test: drag a photo / 1. Given"),
        "{n}"
    );
    assert!(
        n.contains("spec files: specs/001-albums/spec.md, specs/001-albums/plan.md"),
        "{n}"
    );
    // The facts travel as JSON from the broker to the hook.
    let back: Facts = serde_json::from_value(serde_json::to_value(&f).unwrap()).unwrap();
    assert_eq!(back, f);
}
