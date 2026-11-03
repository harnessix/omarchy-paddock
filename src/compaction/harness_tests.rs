//! Codex, Grok Build and agy: the hooks in the sandbox home, the trust of
//! Codex's hook, and each hook style with the harness's own hook input.

use super::tests::{
    compacted, parent_sees, pasture_member, pasture_sees, sandbox_home, sandbox_spec, spawn_tree,
    tree, world,
};
use super::*;
use crate::hostcli::compacted::run;
use std::path::Path;

/// The hash of paddock's hook entry in profiles/codex.toml, computed apart
/// from paddock: SHA-256 of
/// `{"event_name":"session_start","hooks":[{"async":false,"command":"paddock-host compacted --harness codex","timeout":20,"type":"command"}],"matcher":"compact"}`.
const PADDOCK_CODEX_HASH: &str =
    "sha256:371c933067cb6d5d4494380817fdb03cab5047e30dc69e36721fd913c6ec44e1";

#[test]
fn the_codex_trust_hash_matches_the_pairs_in_codex_own_tests() {
    // codex-rs/core/tests/suite/hooks.rs (rust-v0.166.0): a plugin's
    // hooks.json and the trusted_hash values that Codex accepts for it.
    let handler = serde_json::json!({
        "type": "command",
        "command": "python3 -I -S ${PLUGIN_ROOT}/hooks/pre_tool_use_hook.py"
    });
    assert_eq!(
        codex::hook_hash("PreToolUse", Some("^Bash$"), &handler).unwrap(),
        "sha256:c9ee1aca1b5d8c72b99df6571c87d61350b09f7f4594ae30e7250a2027e9d76b"
    );
    assert_eq!(
        codex::hook_hash("PostToolUse", Some("^Bash$"), &handler).unwrap(),
        "sha256:9eef43ad6f26b1f9b94f318a50445f8bb62c363da08152a2966b73d308bf42ba"
    );
    // The default timeout is part of the hash; the key names the position.
    let group = serde_json::json!({ "matcher": "^Bash$", "hooks": [handler] });
    let e = codex::trust_entries("/home/demo/.codex/hooks.json", "PreToolUse", 2, &group).unwrap();
    assert_eq!(e[0].0, "/home/demo/.codex/hooks.json:pre_tool_use:2:0");
    assert!(e[0].1.starts_with("sha256:c9ee1aca"));
    let mut h = group["hooks"][0].clone();
    h["timeout"] = 600.into();
    assert!(codex::hook_hash("PreToolUse", Some("^Bash$"), &h)
        .unwrap()
        .starts_with("sha256:c9ee1aca"));
    // Only command hooks.
    assert!(codex::hook_hash("PreToolUse", None, &serde_json::json!({"type": "prompt"})).is_err());
}

fn json(p: &Path) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap()
}

#[test]
fn codex_gets_its_hook_and_only_its_trust_in_the_sandbox_home() {
    let w = world("");
    // The user's own hooks stay untrusted, also with a trust in the host
    // copy of config.toml. A hook whose program is not in the sandbox is
    // left out.
    let codex = w.env.home.join(".codex");
    std::fs::create_dir_all(&codex).unwrap();
    std::fs::write(
        codex.join("hooks.json"),
        r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"my-hook"}]},{"hooks":[{"type":"command","command":"true"}]}]}}"#,
    )
    .unwrap();
    let src = format!("{}/.codex/hooks.json", w.env.home.display());
    std::fs::write(
        codex.join("config.toml"),
        format!("model = \"m\"\n[hooks.state.\"{src}:session_start:1:0\"]\ntrusted_hash = \"sha256:00\"\n"),
    )
    .unwrap();
    let before = tree(&w.env.home);
    // The snapshot of the host data has the user's hooks.json.
    let spec = sandbox_spec(&w, "codex", &[".codex/hooks.json"]);
    let home = spec.home_src.clone();
    assert!(!home.starts_with(&w.env.home));
    // The next start of the same home finds the hook where it is.
    crate::privacy::write_settings(&home, &spec.home_settings).unwrap();
    crate::hooks::write(&spec).unwrap();
    let hooks = json(&home.join(".codex/hooks.json"));
    let groups = hooks["hooks"]["SessionStart"].as_array().unwrap();
    assert_eq!(groups.len(), 2, "{groups:?}");
    assert_eq!(
        groups[0]["hooks"][0]["command"],
        "paddock-host compacted --harness codex"
    );
    assert_eq!(groups[1]["hooks"][0]["command"], "true");
    let cfg: toml::Value =
        toml::from_str(&std::fs::read_to_string(home.join(".codex/config.toml")).unwrap()).unwrap();
    assert_eq!(cfg["model"].as_str(), Some("m"));
    // The sandbox home has the host home's path.
    let state = cfg["hooks"]["state"].as_table().unwrap();
    assert_eq!(state.len(), 1, "{state:?}");
    assert_eq!(
        state[&format!("{src}:session_start:0:0")]["trusted_hash"].as_str(),
        Some(PADDOCK_CODEX_HASH)
    );
    // The host home did not change.
    assert_eq!(tree(&w.env.home), before);

    // Without `[compaction]`: no hook and no trust.
    let w = world("[compaction]\nenabled = false\n");
    let home = sandbox_home(&w, "codex");
    assert!(!home.join(".codex/hooks.json").exists());
    let cfg = std::fs::read_to_string(home.join(".codex/config.toml")).unwrap_or_default();
    assert!(!cfg.contains("trusted_hash"), "{cfg}");
}

#[test]
fn grok_and_agy_get_their_hooks_in_the_sandbox_home_only() {
    let w = world("");
    let before = tree(&w.env.home);
    let home = sandbox_home(&w, "grok");
    let g = json(&home.join(".grok/hooks/paddock-compaction.json"));
    for ev in ["PostCompact", "PostToolUse"] {
        assert_eq!(
            g["hooks"][ev][0]["hooks"][0]["command"], "paddock-host compacted --harness grok",
            "{ev}"
        );
    }
    let home = sandbox_home(&w, "agy");
    let a = json(&home.join(".gemini/config/hooks.json"));
    assert_eq!(
        a["paddock-compaction"]["PreInvocation"][0]["command"],
        "paddock-host compacted --harness agy"
    );
    assert_eq!(tree(&w.env.home), before);
    let w = world("[compaction]\nenabled = false\n");
    assert!(!sandbox_home(&w, "grok").join(".grok/hooks").exists());
    assert!(!sandbox_home(&w, "agy").join(".gemini/config").exists());
}

/// The text that the harness reads into its context, from the hook output.
fn note_text(harness: &str, out: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(out).unwrap();
    let t = match harness {
        "agy" => &v["injectSteps"][0]["ephemeralMessage"],
        _ => &v["hookSpecificOutput"]["additionalContext"],
    };
    t.as_str().unwrap().to_string()
}

/// agy's transcript of conversation `c-1` in the sandbox home.
fn agy_transcript(home: &Path) -> std::path::PathBuf {
    home.join(".gemini/antigravity-cli/brain/c-1/.system_generated/logs/transcript.jsonl")
}

fn append(p: &Path, line: &str) {
    use std::io::Write;
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(p)
        .unwrap();
    writeln!(f, "{line}").unwrap();
}

/// The agy PreInvocation input, as in https://antigravity.google/docs/hooks.
fn agy_input(home: &Path, n: u64) -> serde_json::Value {
    serde_json::json!({
        "invocationNum": n,
        "initialNumSteps": 10,
        "conversationId": "c-1",
        "workspacePaths": ["/workspace/project"],
        "transcriptPath": agy_transcript(home),
        "artifactDirectoryPath": home.join(".gemini/antigravity-cli/brain/c-1"),
        "modelName": "gemini-3.6-flash-medium"
    })
}

/// The Grok Build input of `event` (crates/codegen/xai-grok-hooks/src/event.rs,
/// `HookEventEnvelope::to_hook_json`): camelCase keys with snake_case
/// aliases, and `hook_event_name` in PascalCase.
fn grok_input(event: &str, session: &str) -> serde_json::Value {
    let mut v = serde_json::json!({
        "hookEventName": event,
        "sessionId": session,
        "session_id": session,
        "cwd": "/work/app",
        "workspaceRoot": "/work/app",
        "timestamp": "2026-11-03T12:00:00Z",
    });
    match event {
        "post_compact" => {
            v["hook_event_name"] = "PostCompact".into();
            v["source"] = "auto".into();
        }
        _ => {
            v["hook_event_name"] = "PostToolUse".into();
            v["toolName"] = "run_terminal_command".into();
            v["tool_name"] = "run_terminal_command".into();
            v["toolInput"] = serde_json::json!({"command": "ls"});
        }
    }
    v
}

/// One compaction of `harness` through its hooks, as the harness runs them.
/// Returns the hook output that carries the note.
fn compact_once(
    harness: &str,
    home: &Path,
    record: &mut dyn FnMut(&str) -> Option<Facts>,
) -> Option<String> {
    match harness {
        // codex-rs/hooks/src/schema.rs, `SessionStartCommandInput`.
        "codex" => {
            let input = serde_json::json!({
                "session_id": "019a-s",
                "transcript_path": "/home/demo/.codex/sessions/2026/11/03/rollout.jsonl",
                "cwd": "/work/app",
                "hook_event_name": "SessionStart",
                "model": "gpt-5.5-codex",
                "permission_mode": "bypassPermissions",
                "source": "compact"
            });
            run("codex", &input, home, record)
        }
        "grok" => {
            assert_eq!(
                run("grok", &grok_input("post_compact", "s-1"), home, record),
                None
            );
            run("grok", &grok_input("post_tool_use", "s-1"), home, record)
        }
        "agy" => {
            append(
                &agy_transcript(home),
                r#"{"step_index":12,"type":"CHECKPOINT","content":"summary"}"#,
            );
            run("agy", &agy_input(home, 7), home, record)
        }
        _ => unreachable!(),
    }
}

/// Before the first compaction: agy takes the count of its transcript.
fn prepare(harness: &str, home: &Path) {
    if harness == "agy" {
        append(
            &agy_transcript(home),
            r#"{"step_index":0,"type":"USER_INPUT"}"#,
        );
        let mut never = |_: &str| -> Option<Facts> { panic!("no compaction yet") };
        assert_eq!(run("agy", &agy_input(home, 0), home, &mut never), None);
    }
}

const NEW: &[&str] = &["codex", "grok", "agy"];

#[test]
fn each_hook_style_gives_the_note_and_tells_the_parent() {
    for harness in NEW {
        let t = spawn_tree("[compaction]\nnote = \"Keep the ticket rules.\"\n", harness);
        let home = tempfile::tempdir().unwrap();
        prepare(harness, home.path());
        let mut record = |_: &str| {
            let r = t.broker.handle(&compacted(harness));
            serde_json::from_value::<Facts>(r.result).ok()
        };
        let out = compact_once(harness, home.path(), &mut record).expect(harness);
        let n = note_text(harness, &out);
        assert!(n.contains("compaction 1 of this run"), "{harness}: {n}");
        assert!(n.contains(&format!("harness: {harness}")), "{harness}: {n}");
        assert!(n.contains("role: implementer"), "{harness}: {n}");
        assert!(n.contains("parent: claude-parent-1 (it knows this run as spawn id abc)"));
        assert!(
            n.contains("/home/demo/.paddock-spawn/codex-spawn-abc/prompt.md. Read it again."),
            "{harness}: {n}"
        );
        assert!(n.contains("Keep the ticket rules."), "{harness}: {n}");
        assert_eq!(parent_sees(&t).unwrap()["count"], 1, "{harness}");
        assert_eq!(parent_sees(&t).unwrap()["harness"], *harness);
        let out = compact_once(harness, home.path(), &mut record).expect(harness);
        assert!(note_text(harness, &out).contains("compaction 2 of this run"));
        assert_eq!(parent_sees(&t).unwrap()["count"], 2, "{harness}");
        assert_eq!(count(&t.w.env, "codex-spawn-abc"), 2, "{harness}");
    }
}

#[test]
fn each_hook_style_records_a_pasture_member_in_the_pasture() {
    for harness in NEW {
        let w = world("");
        let (b, svc) = pasture_member(&w, harness);
        let home = tempfile::tempdir().unwrap();
        prepare(harness, home.path());
        let mut record = |_: &str| {
            let r = b.handle(&compacted(harness));
            assert_eq!(r.result["kv"]["scope"], "pasture", "{harness}");
            serde_json::from_value::<Facts>(r.result).ok()
        };
        let out = compact_once(harness, home.path(), &mut record).expect(harness);
        let n = note_text(harness, &out);
        assert!(n.contains("pasture: team"), "{harness}: {n}");
        assert!(n.contains("role: none"), "{harness}: {n}");
        let seen = pasture_sees(&svc).unwrap();
        assert_eq!(seen["count"], 1, "{harness}");
        assert_eq!(seen["harness"], *harness);
    }
}

fn fake_facts(calls: &std::cell::Cell<u64>) -> Option<Facts> {
    calls.set(calls.get() + 1);
    Some(Facts {
        sandbox: "s".into(),
        harness: "h".into(),
        count: calls.get(),
        ..Default::default()
    })
}

#[test]
fn codex_ignores_session_starts_that_are_no_compaction() {
    let home = tempfile::tempdir().unwrap();
    let calls = std::cell::Cell::new(0);
    for source in ["startup", "resume", "clear", "fork"] {
        let input = serde_json::json!({"hook_event_name": "SessionStart", "source": source});
        assert_eq!(action("codex", &input), None, "{source}");
        assert_eq!(
            run("codex", &input, home.path(), &mut |_| fake_facts(&calls)),
            None
        );
    }
    assert_eq!(calls.get(), 0);
}

#[test]
fn grok_keeps_the_note_for_its_own_session_until_a_tool_runs() {
    let home = tempfile::tempdir().unwrap();
    let h = home.path();
    let calls = std::cell::Cell::new(0);
    let mut rec = |_: &str| fake_facts(&calls);
    // A tool before any compaction: nothing.
    assert_eq!(
        run("grok", &grok_input("post_tool_use", "s-1"), h, &mut rec),
        None
    );
    assert_eq!(
        run("grok", &grok_input("post_compact", "s-1"), h, &mut rec),
        None
    );
    assert_eq!(calls.get(), 1);
    // Another session (a subagent) does not take the note.
    assert_eq!(
        run("grok", &grok_input("post_tool_use", "s-2"), h, &mut rec),
        None
    );
    let out = run("grok", &grok_input("post_tool_use", "s-1"), h, &mut rec).unwrap();
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PostToolUse");
    assert!(note_text("grok", &out).contains("compaction 1"));
    // Once only.
    assert_eq!(
        run("grok", &grok_input("post_tool_use", "s-1"), h, &mut rec),
        None
    );
    assert_eq!(calls.get(), 1);
    // Other Grok events are nothing.
    let start = serde_json::json!({"hook_event_name": "SessionStart", "source": "startup"});
    assert_eq!(action("grok", &start), None);
}

#[test]
fn agy_counts_new_checkpoint_steps_once() {
    let home = tempfile::tempdir().unwrap();
    let h = home.path();
    let calls = std::cell::Cell::new(0);
    let mut rec = |_: &str| fake_facts(&calls);
    let t = agy_transcript(h);
    // A resumed conversation with an old checkpoint: the first look only
    // takes the count.
    append(&t, r#"{"type":"CHECKPOINT"}"#);
    assert_eq!(run("agy", &agy_input(h, 0), h, &mut rec), None);
    assert_eq!(run("agy", &agy_input(h, 1), h, &mut rec), None);
    append(
        &t,
        r#"{"type":"PLANNER_RESPONSE","text":"no checkpoint here"}"#,
    );
    assert_eq!(run("agy", &agy_input(h, 2), h, &mut rec), None);
    append(&t, r#"{"step":{"stepType":"CORTEX_STEP_TYPE_CHECKPOINT"}}"#);
    let out = run("agy", &agy_input(h, 3), h, &mut rec).unwrap();
    assert!(note_text("agy", &out).contains("compaction 1"));
    assert_eq!(run("agy", &agy_input(h, 4), h, &mut rec), None);
    assert_eq!(calls.get(), 1);
    // agy cut transcript.jsonl: the full one counts.
    std::fs::write(&t, "").unwrap();
    std::fs::write(
        t.with_file_name("transcript_full.jsonl"),
        "{\"type\":\"CHECKPOINT\"}\n{\"type\":\"CHECKPOINT\"}\n{\"type\":\"CHECKPOINT\"}\n",
    )
    .unwrap();
    assert!(run("agy", &agy_input(h, 5), h, &mut rec).is_some());
    assert_eq!(calls.get(), 2);
    // A path that is no transcript is never read.
    let mut i = agy_input(h, 6);
    i["transcriptPath"] = "/etc/passwd".into();
    assert_eq!(run("agy", &i, h, &mut rec), None);
    // An input without invocationNum is no PreInvocation.
    let mut i = agy_input(h, 7);
    i.as_object_mut().unwrap().remove("invocationNum");
    assert_eq!(action("agy", &i), None);
}

#[test]
fn checkpoint_steps_are_counted_by_their_type() {
    let text = [
        r#"{"type":"CHECKPOINT"}"#,
        r#"{"type":"CORTEX_STEP_TYPE_CHECKPOINT"}"#,
        r#"{"step_type":"checkpoint"}"#,
        r#"{"type":"USER_INPUT","text":"make a CHECKPOINT please"}"#,
        "not json, checkpoint",
        r#"{"type":"CHECKPOINTS"}"#,
    ]
    .join("\n");
    assert_eq!(checkpoints(text.as_bytes()), 3);
    assert_eq!(file_name("note", "a/../b c"), "note-abc");
    assert_eq!(file_name("note", ""), "note-none");
}
