//! Capability modes from the user configuration.

use super::CAPABILITIES;
use crate::env::HostEnv;
use anyhow::{bail, Context as _, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Deny,
    Ask,
    Allow,
}

/// Per-capability modes. `exec` can never be `allow`: that would remove the
/// sandbox, so it is read as `ask`.
#[derive(Debug, Clone, Serialize)]
pub struct HostPolicy {
    pub modes: BTreeMap<String, Mode>,
    /// Seconds to wait for an answer before a request times out.
    pub wait: u64,
    /// The named host tools of `[host.tools]`.
    pub tools: BTreeMap<String, crate::hosttools::Tool>,
}

impl Default for HostPolicy {
    fn default() -> Self {
        let mut modes = BTreeMap::new();
        for (cap, _) in CAPABILITIES {
            modes.insert(cap.to_string(), Mode::Ask);
        }
        modes.insert("notify".into(), Mode::Allow);
        Self {
            modes,
            wait: 120,
            tools: BTreeMap::new(),
        }
    }
}

impl HostPolicy {
    pub fn load(env: &HostEnv) -> Result<Self> {
        let mut p = Self::default();
        let file = env.paddock_config().join("config.toml");
        let Ok(text) = std::fs::read_to_string(&file) else {
            return Ok(p);
        };
        let doc: toml::Value =
            toml::from_str(&text).with_context(|| format!("{} is invalid", file.display()))?;
        let Some(host) = doc.get("host").and_then(|h| h.as_table()) else {
            return Ok(p);
        };
        p.tools = crate::hosttools::parse_host(host)
            .with_context(|| format!("{} is invalid", file.display()))?;
        for (k, v) in host {
            if k == "tools" {
                continue;
            }
            if k == "wait" {
                p.wait = v
                    .as_integer()
                    .filter(|n| (5..=3600).contains(n))
                    .context("[host] wait must be 5 to 3600 seconds")?
                    as u64;
                continue;
            }
            if !CAPABILITIES.iter().any(|(c, _)| c == k) {
                bail!(
                    "[host] {k}: unknown capability. Known: {}",
                    CAPABILITIES
                        .iter()
                        .map(|c| c.0)
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            let mode: Mode = v
                .clone()
                .try_into()
                .with_context(|| format!("[host] {k} must be deny, ask or allow"))?;
            p.modes.insert(k.clone(), mode);
        }
        if p.modes.get("exec") == Some(&Mode::Allow) {
            p.modes.insert("exec".into(), Mode::Ask);
        }
        Ok(p)
    }

    pub fn mode(&self, cap: &str) -> Mode {
        self.modes.get(cap).copied().unwrap_or(Mode::Deny)
    }
}
