//! The model and the reasoning effort of a child: `paddock-host spawn
//! --model NAME --effort LEVEL`, a task hint such as `[effort:high]`
//! (speckit/tasks.rs), and `[spawn] effort` and `[spawn] models` of the
//! user configuration.
//!
//! The rules:
//!
//! - A flag of the request wins over the task hint, and the hint wins over
//!   `[spawn] effort`. paddock has no default model: without one, the child
//!   runs the harness's own default, which the user set up for that harness.
//! - The child gets them through the harness's own flags, from `[oneshot]
//!   model_args` and `effort_args` of its profile. A harness without these
//!   entries refuses a flag. A hint or the configured effort is then left
//!   out, and the card says so.
//! - `[spawn] models` lists the models that a spawn may ask for, by flag or
//!   by hint. A child never gets another one. Empty: any.

use super::SpawnCfg;
use crate::env::HostEnv;
use crate::profile::OneShot;
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

/// The placeholder for the model name in `[oneshot] model_args`.
pub const MODEL: &str = "{model}";
/// The placeholder for the level in `[oneshot] effort_args`.
pub const EFFORT: &str = "{effort}";

/// The reasoning effort of a child.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Low,
    Medium,
    High,
    Xhigh,
}

impl std::str::FromStr for Effort {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        Ok(match s {
            "low" => Self::Low,
            "medium" => Self::Medium,
            "high" => Self::High,
            "xhigh" => Self::Xhigh,
            _ => bail!("unknown effort '{s}': use low, medium, high or xhigh"),
        })
    }
}

impl Effort {
    pub fn name(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
        }
    }
}

/// A model name that paddock passes on. Letters, digits and `.-_:/@`
/// only, and no `-` first: codex reads `-c model=NAME` as TOML, so a quote,
/// a `=`, a space or a line break could set other keys, and a leading `-`
/// would read as a flag.
pub fn valid_model(s: &str) -> bool {
    s.len() <= 100
        && s.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || ".-_:/@".contains(c))
}

/// The model and the effort that a child gets. None: the harness default.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pick {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
}

impl Pick {
    /// The harness arguments for this pick, after `[oneshot] args`.
    pub fn args(&self, o: &OneShot) -> Vec<String> {
        let fill = |args: &[String], key: &str, v: &str| -> Vec<String> {
            args.iter().map(|a| a.replace(key, v)).collect()
        };
        let mut out = Vec::new();
        if let Some(m) = &self.model {
            out.extend(fill(&o.model_args, MODEL, m));
        }
        if let Some(e) = self.effort {
            out.extend(fill(&o.effort_args, EFFORT, e.name()));
        }
        out
    }
}

/// The pick for one child, and its lines on the approval card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chosen {
    pub pick: Pick,
    pub card: String,
}

/// Choose the model and the effort of a child of `harness`. `asked` comes
/// from the flags of the request, `hint` from the task line. Err: the
/// request is refused, with the reason.
pub fn choose(
    cfg: &SpawnCfg,
    harness: &str,
    o: &OneShot,
    asked: &Pick,
    hint: &Pick,
) -> Result<Chosen> {
    let mut notes = Vec::new();
    let mut pick = Pick::default();
    let model = match (&asked.model, &hint.model) {
        (Some(m), _) => Some((m, "asked with --model")),
        (None, Some(m)) => Some((m, "the hint of the task")),
        (None, None) => None,
    };
    if let Some((m, from)) = model {
        if !valid_model(m) {
            bail!("'{m}' is no model name: use letters, digits and .-_:/@");
        }
        if !cfg.models.is_empty() && !cfg.models.contains(m) {
            bail!(
                "model {m} ({from}) is not in [spawn] models of the user configuration (allowed: {})",
                cfg.models.join(", ")
            );
        }
        if !o.model_args.is_empty() {
            pick.model = Some(m.clone());
        } else if asked.model.is_some() {
            bail!(
                "{harness} has no [oneshot] model_args in its profile, so paddock cannot choose its model. Leave out --model"
            );
        } else {
            notes.push(format!(
                "The task asks for model {m}; {harness} cannot take one, so it uses its own."
            ));
        }
    }
    let effort = match (asked.effort, hint.effort, cfg.effort) {
        (Some(e), _, _) => Some((e, "asked with --effort")),
        (None, Some(e), _) => Some((e, "the hint of the task")),
        (None, None, Some(e)) => Some((e, "[spawn] effort")),
        (None, None, None) => None,
    };
    if let Some((e, from)) = effort {
        if !o.effort_args.is_empty() {
            pick.effort = Some(e);
        } else if asked.effort.is_some() {
            bail!(
                "{harness} has no [oneshot] effort_args in its profile, so paddock cannot choose its effort. Leave out --effort"
            );
        } else {
            notes.push(format!(
                "Effort {} ({from}) is left out: {harness} cannot take one.",
                e.name()
            ));
        }
    }
    let line = |what: &str, v: Option<&str>, from: Option<&str>| match (v, from) {
        (Some(v), Some(f)) => format!("{what}: {v} ({f})"),
        _ => format!("{what}: the default of {harness}"),
    };
    let mut card = vec![
        line("Model", pick.model.as_deref(), model.map(|(_, f)| f)),
        line(
            "Effort",
            pick.effort.map(Effort::name),
            effort.map(|(_, f)| f),
        ),
    ];
    card.extend(notes);
    Ok(Chosen {
        pick,
        card: card.join("\n"),
    })
}

/// The pick of a running child, from the job that its parent's paddock
/// wrote, for `paddock ps --json`. None: not a child, or no job file.
pub fn of_child(env: &HostEnv, parent: &str, spawn_id: &str) -> Option<Pick> {
    // Each one is a single path component.
    let part = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    };
    if !part(parent) || !part(spawn_id) {
        return None;
    }
    let f = env
        .paddock_run()
        .join(parent)
        .join("spawn")
        .join(spawn_id)
        .join("job.json");
    // Only the pick of the job (`Job::pick`).
    #[derive(Deserialize)]
    struct Job {
        #[serde(default)]
        pick: Pick,
    }
    let job: Job = serde_json::from_slice(&std::fs::read(f).ok()?).ok()?;
    Some(job.pick)
}

#[cfg(test)]
mod tests;
