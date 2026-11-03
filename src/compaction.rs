//! Context compaction. A long run replaces its conversation with a summary,
//! and the agent can lose facts that paddock knows for certain: its role,
//! its parent, its task file, and for a detached run that it must end with
//! `paddock-host done`. A harness hook that runs after a compaction calls
//! `paddock-host compacted --harness NAME` (hostcli/compacted.rs). The broker
//! counts the compaction, tells the parent or the pasture through paddock
//! kv, writes one audit line, and returns these facts. The client prints
//! them as a short note in the form that the harness reads into its context.
//!
//! The hooks reach the harness through `[compaction]` of its profile: values
//! for settings files in the sandbox home, the same way as `[[state.settings]]`.
//! The host files are never changed.
//!
//! `[compaction]` in the user configuration: `enabled` (default true),
//! `note` (text appended to the note), `notify` (a desktop notification,
//! default false).

use crate::env::HostEnv;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// The kv key prefix of a compaction record.
pub const KEY_PREFIX: &str = "compacted:";
/// The longest `note` from the configuration that reaches the agent.
pub const MAX_NOTE: usize = 2000;

/// `[compaction]` of a harness profile: how the hook reaches the harness.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hook {
    /// Values written into settings files in the sandbox home at each
    /// start, after `seed`, like `[[state.settings]]`. Usually one entry with
    /// `append = true` that adds the hook beside the user's own hooks.
    #[serde(default)]
    pub settings: Vec<crate::profile::Setting>,
    /// Codex's `config.toml`, relative to HOME: paddock trusts the hooks of
    /// `settings` there, and no other hook (compaction/codex.rs).
    #[serde(default)]
    pub codex_trust: Option<String>,
}

/// Add the hook of a profile to the sandbox home, unless `[compaction]
/// enabled = false` in the user configuration.
pub fn apply(
    env: &HostEnv,
    profile: &crate::profile::Profile,
    spec: &mut crate::spec::SandboxSpec,
) -> Result<()> {
    let Some(h) = &profile.compaction else {
        return Ok(());
    };
    if !Cfg::load(env).enabled {
        return Ok(());
    }
    let first = spec.home_settings.len();
    crate::privacy::push_settings(
        profile,
        &h.settings,
        &format!("profile {} compaction hook", profile.name),
        spec,
    )?;
    // A profile with `[hooks] codex_trust` trusts its hooks there (hooks.rs).
    let hooks_trust = profile
        .hooks
        .as_ref()
        .is_some_and(|f| f.codex_trust.is_some());
    if let Some(config) = h.codex_trust.as_ref().filter(|_| !hooks_trust) {
        let rel = PathBuf::from(config);
        if !rel
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
        {
            anyhow::bail!(
                "profile {}: compaction codex_trust {config} must be a path inside the home",
                profile.name
            );
        }
        let home = spec.home.clone();
        for s in spec.home_settings[first..]
            .iter_mut()
            .filter(|s| s.format == "json")
        {
            s.codex_trust = Some(crate::spec::CodexTrust {
                config: rel.clone(),
                source: home.join(&s.file).display().to_string(),
            });
        }
    }
    Ok(())
}

/// `[compaction]` of the user configuration.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Cfg {
    pub enabled: bool,
    pub note: String,
    pub notify: bool,
}

impl Default for Cfg {
    fn default() -> Self {
        Self {
            enabled: true,
            note: String::new(),
            notify: false,
        }
    }
}

impl Cfg {
    pub fn parse(text: &str) -> Result<Self> {
        let v: toml::Value = toml::from_str(text)?;
        Ok(match v.get("compaction") {
            Some(t) => t.clone().try_into()?,
            None => Self::default(),
        })
    }

    /// The user configuration. A missing file is the default; a bad table
    /// is a warning and the default too, so a typo never stops a run.
    pub fn load(env: &HostEnv) -> Self {
        let file = env.paddock_config().join("config.toml");
        let Ok(text) = std::fs::read_to_string(&file) else {
            return Self::default();
        };
        Self::parse(&text).unwrap_or_else(|e| {
            log::warn!("[compaction] in {}: {e:#}", file.display());
            Self::default()
        })
    }
}

/// What paddock knows of a run, for the note after a compaction. It comes
/// from the host, never from the sandbox.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Facts {
    pub sandbox: String,
    pub harness: String,
    /// implementer or reviewer for a spawn child. None: a top-level run.
    #[serde(default)]
    pub role: Option<String>,
    /// The sandbox that started this one.
    #[serde(default)]
    pub parent: Option<String>,
    /// The id that `paddock-host spawn` printed in the parent.
    #[serde(default)]
    pub spawn_id: Option<String>,
    #[serde(default)]
    pub pasture: Option<String>,
    /// The task file, as a path inside the sandbox.
    #[serde(default)]
    pub task_file: Option<String>,
    #[serde(default)]
    pub detached: bool,
    /// The text from `[compaction] note`.
    #[serde(default)]
    pub note: String,
    /// Compactions of this run so far, this one included.
    #[serde(default)]
    pub count: u64,
}

/// The note: plain facts, one per line.
pub fn note(f: &Facts) -> String {
    let mut l = vec![format!(
        "paddock: the context of this run was compacted (compaction {} of this run). These facts come from paddock, not from the summary:",
        f.count
    )];
    l.push(format!("- sandbox: {}, harness: {}", f.sandbox, f.harness));
    l.push(match &f.role {
        Some(r) => {
            format!("- role: {r}, in a sandbox that another agent started with paddock-host spawn")
        }
        None => "- role: none, a top-level run that the user started".into(),
    });
    l.push(match (&f.parent, &f.spawn_id) {
        (Some(p), Some(s)) => format!("- parent: {p} (it knows this run as spawn id {s})"),
        (Some(p), None) => format!("- parent: {p}"),
        _ => "- parent: none".into(),
    });
    l.push(format!(
        "- pasture: {}",
        f.pasture.as_deref().unwrap_or("none")
    ));
    match &f.task_file {
        Some(t) => l.push(format!("- task file: {t}. Read it again.")),
        None => l.push("- task file: none".into()),
    }
    if f.detached {
        l.push("- this is a detached run: it ends with `paddock-host done` when the task is finished or cannot be finished".into());
    }
    let extra = f.note.trim();
    if !extra.is_empty() {
        l.push(extra.chars().take(MAX_NOTE).collect());
    }
    l.join("\n")
}

fn count_file(env: &HostEnv, id: &str) -> Option<PathBuf> {
    crate::view::valid_id(id).then(|| env.paddock_run().join(id).join("compactions"))
}

/// The compactions of a run so far, for `paddock ps --json`.
pub fn count(env: &HostEnv, id: &str) -> u64 {
    count_file(env, id)
        .and_then(|f| std::fs::read_to_string(f).ok())
        .and_then(|t| t.trim().parse().ok())
        .unwrap_or(0)
}

/// Count one more compaction of a run and return the new count. The broker
/// of the run is the only writer.
pub fn record(env: &HostEnv, id: &str) -> Result<u64> {
    let f = count_file(env, id).ok_or_else(|| anyhow::anyhow!("bad sandbox id {id}"))?;
    let n = count(env, id) + 1;
    if let Some(d) = f.parent() {
        crate::privfs::dir(d)?;
    }
    let tmp = f.with_extension("tmp");
    std::fs::write(&tmp, format!("{n}\n"))?;
    std::fs::rename(&tmp, &f)?;
    Ok(n)
}

/// The kv value of a compaction record. No text of the run goes into it.
pub fn record_value(f: &Facts, time: u64) -> String {
    serde_json::json!({
        "time": time,
        "count": f.count,
        "sandbox": f.sandbox,
        "harness": f.harness,
        "role": f.role,
    })
    .to_string()
}

/// What a hook input asks for. Harnesses differ in where a note can go:
///
/// - Claude Code, Qwen Code and Codex: a SessionStart hook with the source
///   "compact" runs after the compaction, and its output reaches the
///   context (`Direct`).
/// - Grok Build discards the output of its compaction hooks and of an
///   allowing UserPromptSubmit hook. PostCompact marks the compaction, and
///   the next PostToolUse hook adds the note (`Mark`, then `Deliver`).
/// - agy has no compaction hook. A compaction adds a CHECKPOINT step to the
///   transcript, and a PreInvocation hook looks for a new one (`Scan`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// A compaction: record it and print the note for `event`.
    Direct { event: String },
    /// A compaction: record it and keep the note for the session.
    Mark { session: String },
    /// Print the kept note of the session for `event`, if there is one.
    Deliver { session: String, event: String },
    /// Count the CHECKPOINT steps of a transcript. New ones are compactions.
    Scan {
        conversation: String,
        transcript: String,
        event: String,
    },
}

/// The action for a hook input. None: the hook fired for something else
/// (Claude Code's SessionStart also fires at startup, resume and clear), so
/// paddock records nothing.
pub fn action(harness: &str, input: &serde_json::Value) -> Option<Action> {
    let s = |k: &str| input.get(k).and_then(|v| v.as_str());
    match harness {
        "claude" | "qwen" | "codex" => match (s("hook_event_name"), s("source")) {
            (Some("SessionStart"), Some("compact")) => Some(Action::Direct {
                event: "SessionStart".into(),
            }),
            _ => None,
        },
        // `hook_event_name` has the PascalCase name, `sessionId` and
        // `session_id` the session.
        "grok" => {
            let session = s("sessionId")
                .or(s("session_id"))
                .unwrap_or_default()
                .to_string();
            match s("hook_event_name") {
                Some("PostCompact") => Some(Action::Mark { session }),
                Some("PostToolUse") => Some(Action::Deliver {
                    session,
                    event: "PostToolUse".into(),
                }),
                _ => None,
            }
        }
        // agy's input has no event name: only PreInvocation (and
        // PostInvocation) have `invocationNum`, and paddock adds the hook to
        // PreInvocation only.
        "agy" => {
            input.get("invocationNum")?;
            Some(Action::Scan {
                conversation: s("conversationId")?.to_string(),
                transcript: s("transcriptPath")?.to_string(),
                event: "PreInvocation".into(),
            })
        }
        _ => None,
    }
}

/// The note in the form that the harness reads into its context.
pub fn render(harness: &str, event: &str, note: &str) -> Option<String> {
    match harness {
        "claude" | "qwen" | "codex" | "grok" => Some(
            serde_json::json!({"hookSpecificOutput": {"hookEventName": event, "additionalContext": note}})
                .to_string(),
        ),
        "agy" => Some(serde_json::json!({"injectSteps": [{"ephemeralMessage": note}]}).to_string()),
        _ => None,
    }
}

/// Where the hook keeps its state in the sandbox home: the kept notes of
/// Grok Build and the CHECKPOINT counts of agy.
pub fn state_dir(home: &std::path::Path) -> PathBuf {
    home.join(".local/state/paddock/compaction")
}

/// A session or conversation id as a file name.
pub fn file_name(prefix: &str, id: &str) -> String {
    let id: String = id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .take(128)
        .collect();
    format!("{prefix}-{}", if id.is_empty() { "none" } else { &id })
}

/// agy writes the whole transcript to `transcript_full.jsonl` when it cuts
/// `transcript.jsonl`. The full one counts when it exists.
pub fn transcript_file(path: &std::path::Path) -> PathBuf {
    let full = path.with_file_name("transcript_full.jsonl");
    if full.is_file() {
        full
    } else {
        path.to_path_buf()
    }
}

/// The CHECKPOINT steps of an agy transcript, one JSON object per line. A
/// step counts when its type (`type`, `step_type` or `stepType`, at the top
/// or in `step`) is CHECKPOINT, also with a prefix such as
/// `CORTEX_STEP_TYPE_CHECKPOINT`.
pub fn checkpoints(transcript: impl std::io::BufRead) -> u64 {
    fn is_checkpoint(v: &serde_json::Value) -> bool {
        ["type", "step_type", "stepType"].iter().any(|k| {
            v.get(k).and_then(|t| t.as_str()).is_some_and(|t| {
                let t = t.to_ascii_uppercase();
                t == "CHECKPOINT" || t.ends_with("_CHECKPOINT")
            })
        })
    }
    transcript
        .split(b'\n')
        .map_while(|l| l.ok())
        .filter(|l| l.windows(10).any(|w| w.eq_ignore_ascii_case(b"checkpoint")))
        .filter_map(|l| serde_json::from_slice::<serde_json::Value>(&l).ok())
        .filter(|v| is_checkpoint(v) || v.get("step").is_some_and(is_checkpoint))
        .count() as u64
}

pub mod codex;

#[cfg(test)]
mod harness_tests;
#[cfg(test)]
mod tests;
