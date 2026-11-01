//! The forage ledger: how much of each account's limits is used, how fast,
//! and whether it runs out before it resets.
//!
//! The numbers come from Omarchy's usage records
//! (`~/.local/state/omarchy/agents/usage/<agent>.json`): for each limit
//! window, the share used and when it resets. paddock asks for fresh ones
//! with `paddock omarchy-usage-update`, the command of the Omarchy panel,
//! which checks a login that a sandbox holds from paddock's side (and a
//! Codex login in its keeper sandbox). One agent is asked at most once in
//! `MIN_ASK` seconds.
//!
//! Each new record adds its numbers to a ledger of plain numbers. From it:
//!
//! - the rate now: the slope between the last two samples of the current
//!   window (a window ends when its reset time changes);
//! - the usual rate: the median of the average rates of earlier windows;
//! - when the window runs out at the rate now, and whether that is before
//!   it resets ("overgrazing").
//!
//! Simple statistics, no model: the agent that hands out work uses them to
//! pick the account with room, as cattle move to the grass that is left.

use crate::env::HostEnv;
use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Seconds between two refreshes of one agent's numbers.
pub const MIN_ASK: u64 = 5 * 60;
/// Seconds between two samples while a timed task runs.
pub const SAMPLE_EVERY: u64 = 15 * 60;
/// Samples older than this leave the ledger.
const KEEP: u64 = 60 * 24 * 3600;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    pub label: String,
    /// The share used, 0 to 1.
    pub used: f64,
    /// When the window resets, or 0 when the record does not say.
    pub resets: u64,
    /// When the numbers were fetched.
    pub at: u64,
}

/// One limit window of one agent, as `forage` reports it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Window {
    pub agent: String,
    pub label: String,
    pub used: f64,
    pub resets: Option<u64>,
    /// When the numbers were fetched.
    pub fetched: u64,
    /// The share used per hour now, when two samples allow it.
    pub rate: Option<f64>,
    /// The usual share per hour, from earlier windows.
    pub usual: Option<f64>,
    /// When the window runs out at the rate now.
    pub runs_out: Option<u64>,
    /// It runs out before it resets.
    pub overgrazing: bool,
    /// The record says that its last check failed.
    pub stale: bool,
}

fn usage_dir(env: &HostEnv) -> PathBuf {
    env.state_dir.join("omarchy").join("agents").join("usage")
}

fn ledger_dir(env: &HostEnv) -> PathBuf {
    env.paddock_state().join("forage")
}

fn ledger_file(env: &HostEnv, agent: &str) -> PathBuf {
    ledger_dir(env).join(format!("{agent}.jsonl"))
}

fn safe_agent(a: &str) -> bool {
    !a.is_empty()
        && a.len() <= 40
        && a.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// An ISO 8601 time with an offset (`2026-11-08T03:54:00+00:00`, `...Z`).
pub fn parse_time(s: &str) -> Option<u64> {
    let base = crate::keeper::parse_rfc3339_utc(s)?;
    let rest = s.get(19..)?;
    let rest = rest.trim_start_matches(|c: char| c == '.' || c.is_ascii_digit());
    let offset: i64 = match rest.as_bytes().first() {
        None | Some(b'Z') | Some(b'z') => 0,
        Some(&sign) if sign == b'+' || sign == b'-' => {
            let h: i64 = rest.get(1..3)?.parse().ok()?;
            let m: i64 = rest.get(4..6).and_then(|m| m.parse().ok()).unwrap_or(0);
            let o = h * 3600 + m * 60;
            if sign == b'+' {
                o
            } else {
                -o
            }
        }
        _ => return None,
    };
    u64::try_from(base as i64 - offset).ok()
}

/// The record of one agent: its fetch time, whether it is stale, and its
/// windows.
fn read_record(env: &HostEnv, agent: &str) -> Option<(u64, bool, Vec<Sample>)> {
    let text = std::fs::read_to_string(usage_dir(env).join(format!("{agent}.json"))).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let at = v["limitsFetchedAt"].as_u64()? / 1000;
    let stale = v["limitsStale"].as_bool().unwrap_or(false);
    let samples = v["limits"]
        .as_array()?
        .iter()
        .filter_map(|l| {
            Some(Sample {
                label: l["label"].as_str()?.chars().take(80).collect(),
                used: l["percent"].as_f64()?.clamp(0.0, 1.0),
                resets: l["resetsAt"].as_str().and_then(parse_time).unwrap_or(0),
                at,
            })
        })
        .collect();
    Some((at, stale, samples))
}

fn read_ledger(env: &HostEnv, agent: &str) -> Vec<Sample> {
    std::fs::read_to_string(ledger_file(env, agent))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// Add an agent's record to its ledger when it is new. Returns whether it was.
pub fn record(env: &HostEnv, agent: &str) -> Result<bool> {
    if !safe_agent(agent) {
        return Ok(false);
    }
    let Some((at, stale, samples)) = read_record(env, agent) else {
        return Ok(false);
    };
    let mut ledger = read_ledger(env, agent);
    if stale || samples.is_empty() || ledger.iter().any(|s| s.at >= at) {
        return Ok(false);
    }
    ledger.retain(|s| s.at + KEEP >= at);
    ledger.extend(samples);
    let text: String = ledger
        .iter()
        .filter_map(|s| serde_json::to_string(s).ok())
        .map(|l| l + "\n")
        .collect();
    crate::privfs::write(&ledger_file(env, agent), text)?;
    Ok(true)
}

/// Ask Omarchy for fresh numbers for `agents`, at most once in `MIN_ASK`
/// seconds each, then record them. Returns the agents asked.
pub fn refresh(env: &HostEnv, agents: &[String]) -> Result<Vec<String>> {
    let now = crate::state::now();
    let due: Vec<String> = agents
        .iter()
        .filter(|a| safe_agent(a))
        .filter(|a| {
            let f = ledger_dir(env).join(format!("{a}.asked"));
            let last = std::fs::read_to_string(&f)
                .ok()
                .and_then(|t| t.trim().parse::<u64>().ok())
                .unwrap_or(0);
            last + MIN_ASK <= now
        })
        .cloned()
        .collect();
    if due.is_empty() {
        return Ok(due);
    }
    for a in &due {
        crate::privfs::write(&ledger_dir(env).join(format!("{a}.asked")), now.to_string())?;
    }
    let me = std::env::current_exe().context("cannot find the paddock executable")?;
    let status = std::process::Command::new(me)
        .arg("omarchy-usage-update")
        .arg("--")
        .args(&due)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if let Err(e) = status {
        log::debug!("cannot run omarchy-usage-update: {e}");
    }
    for a in &due {
        record(env, a)?;
    }
    Ok(due)
}

/// While a timed task runs: fresh numbers every `SAMPLE_EVERY` seconds, so
/// that the ledger has rates even when nobody asks.
pub fn sample_if_due(env: &HostEnv, agent: &str) -> Result<()> {
    let f = ledger_dir(env).join(format!("{agent}.asked"));
    let last = std::fs::read_to_string(&f)
        .ok()
        .and_then(|t| t.trim().parse::<u64>().ok())
        .unwrap_or(0);
    if last + SAMPLE_EVERY <= crate::state::now() {
        refresh(env, &[agent.to_string()])?;
    }
    Ok(())
}

/// The agents whose usage records exist.
pub fn known_agents(env: &HostEnv) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(usage_dir(env))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            e.file_name()
                .to_str()?
                .strip_suffix(".json")
                .map(str::to_string)
        })
        .filter(|a| safe_agent(a))
        .collect();
    out.sort();
    out
}

/// The Omarchy agent whose limits a harness uses.
pub fn agent_of(harness: &str) -> String {
    crate::omarchy_usage::AGENTS
        .iter()
        .find(|a| a.profile == harness)
        .map(|a| a.id.to_string())
        .unwrap_or_else(|| harness.to_string())
}

fn median(mut v: Vec<f64>) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = v.len();
    Some(if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    })
}

/// The windows of one agent, from its ledger and its record.
pub fn windows(env: &HostEnv, agent: &str) -> Vec<Window> {
    let ledger = read_ledger(env, agent);
    let rec = read_record(env, agent);
    let stale = rec.as_ref().is_some_and(|r| r.1);
    let mut labels: Vec<String> = ledger.iter().map(|s| s.label.clone()).collect();
    if let Some((_, _, s)) = &rec {
        labels.extend(s.iter().map(|x| x.label.clone()));
    }
    labels.sort();
    labels.dedup();
    labels
        .into_iter()
        .filter_map(|label| {
            let mut mine: Vec<&Sample> = ledger.iter().filter(|s| s.label == label).collect();
            mine.sort_by_key(|s| s.at);
            // The newest numbers: the record's, else the ledger's.
            let last = rec
                .as_ref()
                .and_then(|(_, _, s)| s.iter().find(|x| x.label == label).cloned())
                .or_else(|| mine.last().map(|s| (*s).clone()))?;
            Some(analyse(agent, &label, &mine, &last, stale))
        })
        .collect()
}

/// The numbers of one window from its samples, oldest first.
pub fn analyse(
    agent: &str,
    label: &str,
    samples: &[&Sample],
    last: &Sample,
    stale: bool,
) -> Window {
    // Split by reset time: each part is one window period.
    let mut periods: Vec<Vec<&Sample>> = Vec::new();
    for s in samples {
        match periods.last_mut() {
            Some(p) if p[0].resets == s.resets => p.push(s),
            _ => periods.push(vec![s]),
        }
    }
    let per_hour = |a: &Sample, b: &Sample| -> Option<f64> {
        let dt = b.at.checked_sub(a.at)?;
        (dt >= 60 && b.used >= a.used).then(|| (b.used - a.used) * 3600.0 / dt as f64)
    };
    let current: Vec<&Sample> = periods
        .last()
        .filter(|p| p[0].resets == last.resets)
        .cloned()
        .unwrap_or_default();
    let rate = match current.len() {
        0 | 1 => None,
        n => per_hour(current[n - 2], current[n - 1])
            .or_else(|| per_hour(current[0], current[n - 1])),
    };
    let done = periods
        .len()
        .saturating_sub(if current.is_empty() { 0 } else { 1 });
    let usual = median(
        periods[..done]
            .iter()
            .filter_map(|p| per_hour(p[0], p[p.len() - 1]))
            .collect(),
    );
    let runs_out = rate
        .filter(|r| *r > 0.0)
        .map(|r| last.at + ((1.0 - last.used) / r * 3600.0) as u64);
    let resets = (last.resets > 0).then_some(last.resets);
    Window {
        agent: agent.into(),
        label: label.into(),
        used: last.used,
        resets,
        fetched: last.at,
        rate,
        usual,
        overgrazing: matches!((runs_out, resets), (Some(o), Some(r)) if o < r),
        runs_out,
        stale,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(used: f64, resets: u64, at: u64) -> Sample {
        Sample {
            label: "Weekly".into(),
            used,
            resets,
            at,
        }
    }

    #[test]
    fn times_with_offsets() {
        assert_eq!(parse_time("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_time("1970-01-01T01:00:00+01:00"), Some(0));
        assert_eq!(parse_time("1970-01-01T00:00:00.500-02:30"), Some(9000));
        assert_eq!(parse_time(""), None);
        assert_eq!(parse_time("soon"), None);
    }

    #[test]
    fn rates_and_overgrazing() {
        let h = 3600;
        let r = 100 * h;
        // An earlier week at 1% an hour; this one at 2% an hour, half used,
        // 10 hours before the reset.
        let past = [s(0.0, 10, 0), s(0.5, 10, 50 * h)];
        let now = [s(0.40, r, 85 * h), s(0.50, r, 90 * h)];
        let all: Vec<&Sample> = past.iter().chain(now.iter()).collect();
        let w = analyse("codex", "Weekly", &all, &now[1], false);
        assert!((w.rate.unwrap() - 0.02).abs() < 1e-9, "{w:?}");
        assert!((w.usual.unwrap() - 0.01).abs() < 1e-9, "{w:?}");
        // Half left at 2% an hour: 25 hours, after the reset in 10.
        assert_eq!(w.runs_out, Some(90 * h + 25 * h));
        assert!(!w.overgrazing);
        // At 10% an hour it runs out in 5 hours, before the reset.
        let fast = [s(0.0, r, 85 * h), s(0.5, r, 90 * h)];
        let all: Vec<&Sample> = fast.iter().collect();
        let w = analyse("codex", "Weekly", &all, &fast[1], false);
        assert!(w.overgrazing, "{w:?}");
        // One sample says nothing about a rate.
        let one = [s(0.3, r, 90 * h)];
        let all: Vec<&Sample> = one.iter().collect();
        let w = analyse("codex", "Weekly", &all, &one[0], false);
        assert!(w.rate.is_none() && !w.overgrazing);
    }

    #[test]
    fn the_ledger_takes_each_record_once() {
        let t = tempfile::tempdir().unwrap();
        let env = HostEnv::from_parts(
            t.path().join("home"),
            "t".into(),
            1000,
            1000,
            Default::default(),
        );
        let dir = usage_dir(&env);
        std::fs::create_dir_all(&dir).unwrap();
        let write = |ms: u64, pct: f64, stale: bool| {
            std::fs::write(
                dir.join("codex.json"),
                serde_json::json!({"limitsFetchedAt": ms, "limitsStale": stale,
                    "limits": [{"label": "Weekly (7-day)", "percent": pct, "resetsAt": "2026-11-08T03:54:00+00:00"}]})
                .to_string(),
            )
            .unwrap();
        };
        write(1_000_000, 0.1, false);
        assert!(record(&env, "codex").unwrap());
        assert!(!record(&env, "codex").unwrap(), "the same record twice");
        write(4_600_000, 0.2, false);
        assert!(record(&env, "codex").unwrap());
        write(8_200_000, 0.3, true);
        assert!(!record(&env, "codex").unwrap(), "a stale record");
        let w = &windows(&env, "codex")[0];
        assert_eq!(w.label, "Weekly (7-day)");
        assert!(w.stale);
        assert!((w.rate.unwrap() - 0.1).abs() < 1e-9, "{w:?}");
        assert!(!record(&env, "../x").unwrap());
        assert_eq!(known_agents(&env), ["codex"]);
        assert_eq!(agent_of("codex"), "codex");
    }
}
