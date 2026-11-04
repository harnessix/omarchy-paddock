//! `[spawn]` of the user configuration: reading it, and the limits that it
//! puts on one child.

use super::{format_bytes, format_cpus, parse_bytes, parse_cpus, Limits, OnTimeout, SpawnCfg};
use crate::env::HostEnv;
use anyhow::{bail, Context as _, Result};

fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

impl SpawnCfg {
    /// Read `[spawn]` from the user configuration. Never from a project file.
    pub fn load(env: &HostEnv) -> Result<Self> {
        let file = env.paddock_config().join("config.toml");
        let Ok(text) = std::fs::read_to_string(&file) else {
            return Ok(Self::default());
        };
        let doc: toml::Value =
            toml::from_str(&text).with_context(|| format!("{} is invalid", file.display()))?;
        Self::from_doc(&doc)
    }

    pub fn from_doc(doc: &toml::Value) -> Result<Self> {
        let mut c = Self::default();
        let Some(t) = doc.get("spawn").and_then(|s| s.as_table()) else {
            return Ok(c);
        };
        let int = |k: &str, v: &toml::Value, lo: i64, hi: i64| {
            v.as_integer()
                .filter(|n| (lo..=hi).contains(n))
                .with_context(|| format!("[spawn] {k} must be a number from {lo} to {hi}"))
        };
        for (k, v) in t {
            match k.as_str() {
                "harnesses" => {
                    let list = v
                        .as_array()
                        .context("[spawn] harnesses must be a list of harness names")?;
                    c.harnesses = list
                        .iter()
                        .map(|h| {
                            h.as_str()
                                .filter(|s| valid_name(s))
                                .map(str::to_string)
                                .context("[spawn] harnesses must be a list of harness names")
                        })
                        .collect::<Result<_>>()?;
                }
                "max_running" => c.max_running = int(k, v, 1, 8)? as u32,
                "max_depth" => c.max_depth = int(k, v, 0, 4)? as u32,
                "timeout" => c.timeout = int(k, v, 60, 86_400)? as u64,
                "max_mem" => {
                    let b = match v {
                        toml::Value::String(s) => parse_bytes(s),
                        toml::Value::Integer(n) if *n > 0 => parse_bytes(&n.to_string()),
                        _ => bail!("[spawn] max_mem must be a size such as \"4G\""),
                    };
                    c.max_mem = Some(b.context("[spawn] max_mem")?);
                }
                "max_cpus" => {
                    let s = match v {
                        toml::Value::Integer(n) => n.to_string(),
                        toml::Value::Float(f) => f.to_string(),
                        _ => bail!("[spawn] max_cpus must be a number such as 2"),
                    };
                    c.max_cpu_percent = Some(parse_cpus(&s).context("[spawn] max_cpus")?);
                }
                "on_timeout" => {
                    c.on_timeout = match v.as_str() {
                        Some("stop") => OnTimeout::Stop,
                        Some("ask") => OnTimeout::Ask,
                        _ => bail!("[spawn] on_timeout must be \"stop\" or \"ask\""),
                    }
                }
                "effort" => {
                    let e = v.as_str().context("[spawn] effort must be low, medium, high or xhigh")?;
                    c.effort = Some(e.parse().context("[spawn] effort")?);
                }
                "models" => {
                    let bad = "[spawn] models must be a list of model names";
                    c.models = v
                        .as_array()
                        .context(bad)?
                        .iter()
                        .map(|m| {
                            m.as_str()
                                .filter(|s| super::model::valid_model(s))
                                .map(str::to_string)
                                .context(bad)
                        })
                        .collect::<Result<_>>()?;
                }
                other => bail!(
                    "[spawn] {other}: unknown setting. Known: harnesses, max_running, max_depth, timeout, max_mem, max_cpus, on_timeout, effort, models"
                ),
            }
        }
        Ok(c)
    }

    /// The limits for one child. A spawn can ask for less than the user
    /// configuration allows, never for more. Without a request, the child
    /// gets the configured limit.
    pub fn limits(
        &self,
        timeout: Option<u64>,
        mem: Option<u64>,
        cpu_percent: Option<u32>,
    ) -> Result<Limits> {
        if let Some(t) = timeout {
            if t == 0 || t > self.timeout {
                bail!(
                    "--timeout {t} is more than [spawn] timeout = {} of the user configuration",
                    self.timeout
                );
            }
        }
        if let (Some(m), Some(max)) = (mem, self.max_mem) {
            if m > max {
                bail!(
                    "--mem {} is more than [spawn] max_mem = {}",
                    format_bytes(m),
                    format_bytes(max)
                );
            }
        }
        if let (Some(c), Some(max)) = (cpu_percent, self.max_cpu_percent) {
            if c > max {
                bail!(
                    "--cpus {} is more than [spawn] max_cpus = {}",
                    format_cpus(c),
                    format_cpus(max)
                );
            }
        }
        Ok(Limits {
            timeout: timeout.unwrap_or(self.timeout),
            mem: mem.or(self.max_mem),
            cpu_percent: cpu_percent.or(self.max_cpu_percent),
        })
    }

    pub fn allows(&self, harness: &str) -> bool {
        self.harnesses.iter().any(|h| h == harness)
    }

    /// Whether a sandbox at `depth` (0: started by the user) can start a child.
    pub fn depth_allows(&self, depth: u32) -> bool {
        depth < self.max_depth
    }
}
