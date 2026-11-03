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
    crate::privacy::push_settings(
        profile,
        &h.settings,
        &format!("profile {} compaction hook", profile.name),
        spec,
    )
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

/// The event of a hook input when it is a compaction, for the note. None:
/// the hook fired for something else (Claude Code's SessionStart also
/// fires at startup, resume and clear), so paddock records nothing.
pub fn hook_event(harness: &str, input: &serde_json::Value) -> Option<String> {
    let s = |k: &str| input.get(k).and_then(|v| v.as_str());
    match harness {
        // The same SessionStart hook with the source "compact".
        "claude" | "qwen" => match (s("hook_event_name"), s("source")) {
            (Some("SessionStart"), Some("compact")) => Some("SessionStart".into()),
            _ => None,
        },
        _ => None,
    }
}

/// The note in the form that the harness reads into its context.
pub fn render(harness: &str, event: &str, note: &str) -> Option<String> {
    match harness {
        "claude" | "qwen" => Some(
            serde_json::json!({"hookSpecificOutput": {"hookEventName": event, "additionalContext": note}})
                .to_string(),
        ),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
