//! Timers: how long a task has run, and how long it may.
//!
//! An agent loses its sense of time with its context: after a compaction,
//! or when another agent takes over, it no longer knows how long it has
//! worked on a bug or when the task must end. A timer lives on the host, in
//! paddock kv (`timer:NAME` in a scope), so it outlives every context, and
//! the agent that it times cannot change it.
//!
//! - Whoever sets a deadline owns it: only the owner (a sandbox, or the user
//!   on the host) can extend or stop it. Anyone else can ask for more time;
//!   the user answers on the approval card.
//! - A soft deadline only tells: a notification and a mark in the panel. A
//!   hard deadline freezes the sandbox that it times (`paddock freeze`),
//!   which `paddock thaw` lets continue.
//! - The time that counts is wall time without the periods when the timed
//!   sandbox was frozen.
//!
//! The keys `timer:*` change only through the timer operations, as leases
//! change only through claim and release.

use crate::env::HostEnv;
use anyhow::{bail, Context as _, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const PREFIX: &str = "timer:";
/// The longest budget: 30 days.
pub const MAX_BUDGET: u64 = 30 * 24 * 3600;
/// The capability name on the approval card for a request for more time.
pub const CAPABILITY: &str = "timer";

/// One timer, as stored in paddock kv.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Timer {
    pub name: String,
    /// The sandbox that set it, or `user`.
    pub owner: String,
    /// The sandbox whose time it counts and that a hard deadline freezes.
    pub subject: String,
    pub started: u64,
    pub budget: u64,
    pub hard: bool,
    /// When the owner stopped it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped: Option<u64>,
    /// The soft deadline was announced; the hard one was enforced.
    #[serde(default)]
    pub announced: bool,
    #[serde(default)]
    pub enforced: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extensions: Vec<Extension>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Extension {
    pub seconds: u64,
    pub by: String,
    pub at: u64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
}

/// A timer as `status` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Status {
    pub name: String,
    pub owner: String,
    pub subject: String,
    pub hard: bool,
    pub started: u64,
    /// Seconds that count: wall time without frozen periods.
    pub used: u64,
    pub budget: u64,
    /// Seconds left; 0 when over.
    pub left: u64,
    pub over: bool,
    pub stopped: bool,
    /// Wall seconds since the start, frozen periods included.
    pub wall: u64,
}

impl Timer {
    pub fn status(&self, now: u64, frozen: &[(u64, u64)]) -> Status {
        let end = self.stopped.unwrap_or(now);
        let wall = end.saturating_sub(self.started);
        let paused: u64 = frozen
            .iter()
            .map(|(a, b)| {
                let (a, b) = ((*a).max(self.started), (*b).min(end));
                b.saturating_sub(a)
            })
            .sum();
        let used = wall.saturating_sub(paused);
        Status {
            name: self.name.clone(),
            owner: self.owner.clone(),
            subject: self.subject.clone(),
            hard: self.hard,
            started: self.started,
            used,
            budget: self.budget,
            left: self.budget.saturating_sub(used),
            over: used >= self.budget,
            stopped: self.stopped.is_some(),
            wall,
        }
    }
}

/// `2h`, `30m`, `90s`, `1h30m`, or a number of seconds.
pub fn parse_duration(s: &str) -> Result<u64> {
    let s = s.trim();
    if s.is_empty() {
        bail!("an empty duration: use for example 2h, 30m or 1h30m");
    }
    if let Ok(n) = s.parse::<u64>() {
        return Ok(n);
    }
    let mut total: u64 = 0;
    let mut num = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() {
            num.push(c);
            continue;
        }
        let n: u64 = num
            .parse()
            .with_context(|| format!("'{s}' is not a duration"))?;
        num.clear();
        total += n * match c {
            'd' => 86400,
            'h' => 3600,
            'm' => 60,
            's' => 1,
            _ => bail!("'{s}' is not a duration: use for example 2h, 30m or 1h30m"),
        };
    }
    if !num.is_empty() {
        bail!("'{s}' is not a duration: a number needs a unit (d, h, m, s)");
    }
    Ok(total)
}

pub fn format_duration(secs: u64) -> String {
    let (h, m) = (secs / 3600, secs % 3600 / 60);
    match (h, m) {
        (0, 0) => format!("{secs}s"),
        (0, m) => format!("{m}m"),
        (h, 0) => format!("{h}h"),
        (h, m) => format!("{h}h{m:02}m"),
    }
}

pub fn check_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'));
    if !ok {
        bail!("'{name}' is not a timer name: use 1 to 64 letters, digits, '-', '_', '.' or ':'");
    }
    Ok(())
}

pub fn check_budget(budget: u64) -> Result<()> {
    if budget == 0 || budget > MAX_BUDGET {
        bail!("a budget must be 1 second to 30 days");
    }
    Ok(())
}

/// The frozen periods of a running sandbox, from its run folder: the ended
/// ones, and the current one up to now.
pub fn frozen_periods(env: &HostEnv, sandbox: &str, now: u64) -> Vec<(u64, u64)> {
    let mut out: Vec<(u64, u64)> = std::fs::read_to_string(periods_file(env, sandbox))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let (a, b) = l.split_once(' ')?;
            Some((a.parse().ok()?, b.parse().ok()?))
        })
        .collect();
    if let Some((since, _)) = crate::freeze::frozen(env, sandbox) {
        out.push((since, now));
    }
    out
}

fn periods_file(env: &HostEnv, sandbox: &str) -> PathBuf {
    env.paddock_run().join(sandbox).join("frozen-periods")
}

/// `paddock thaw` records the period that ended.
pub fn record_frozen(env: &HostEnv, sandbox: &str, since: u64, until: u64) {
    let f = periods_file(env, sandbox);
    match crate::privfs::append(&f) {
        Ok(mut file) => {
            use std::io::Write as _;
            let _ = writeln!(file, "{since} {until}");
        }
        Err(e) => log::debug!("cannot record the frozen period of {sandbox}: {e}"),
    }
}

/// The summary for `paddock ps` and the panel: the timers of a sandbox.
pub fn summary_file(env: &HostEnv, sandbox: &str) -> PathBuf {
    env.paddock_run().join(sandbox).join("timers.json")
}

pub fn read_summary(env: &HostEnv, sandbox: &str) -> Vec<Status> {
    #[derive(Deserialize)]
    struct S {
        name: String,
        owner: String,
        subject: String,
        hard: bool,
        started: u64,
        used: u64,
        budget: u64,
        left: u64,
        over: bool,
        stopped: bool,
        wall: u64,
    }
    let Ok(text) = std::fs::read_to_string(summary_file(env, sandbox)) else {
        return Vec::new();
    };
    serde_json::from_str::<Vec<S>>(&text)
        .unwrap_or_default()
        .into_iter()
        .map(|s| Status {
            name: s.name,
            owner: s.owner,
            subject: s.subject,
            hard: s.hard,
            started: s.started,
            used: s.used,
            budget: s.budget,
            left: s.left,
            over: s.over,
            stopped: s.stopped,
            wall: s.wall,
        })
        .collect()
}

/// The timers stored in one namespace of a store.
pub fn in_store(st: &crate::kv::store::Store, ns: &str, now: u64) -> Result<Vec<(Timer, u64)>> {
    let (metas, _) = st.list(ns, PREFIX, 1000, now)?;
    let mut out = Vec::new();
    for m in metas {
        if let Some(e) = st.get(ns, &m.key, now)? {
            if let Ok(t) = serde_json::from_str::<Timer>(&e.value) {
                out.push((t, e.version));
            }
        }
    }
    Ok(out)
}

/// Write a timer back, if its version did not change meanwhile.
pub fn save(
    st: &mut crate::kv::store::Store,
    ns: &str,
    t: &Timer,
    version: Option<u64>,
    writer: &crate::kv::store::Writer,
    quota: crate::kv::store::Quota,
    now: u64,
) -> Result<u64> {
    let value = serde_json::to_string(t)?;
    st.put(
        ns,
        &format!("{PREFIX}{}", t.name),
        &value,
        version,
        None,
        writer,
        quota,
        now,
    )
    .map_err(|e| anyhow::anyhow!("cannot save the timer {}: {e:?}", t.name))
}

/// The host's file for the self scope of a sandbox, for `paddock timer`.
pub fn self_file(env: &HostEnv, sandbox: &str) -> Result<(PathBuf, String)> {
    let ns = crate::kv::shard::Ns::Own(sandbox.to_string());
    Ok((ns.file(&crate::kv::kv_root(env))?, ns.key()))
}

const HOST: crate::kv::store::Writer = crate::kv::store::Writer {
    sandbox: "user",
    harness: "host",
};

fn host_quota(env: &HostEnv) -> Result<crate::kv::store::Quota> {
    let c = crate::kv::KvCfg::load(env)?;
    Ok(crate::kv::store::Quota {
        bytes: c.self_bytes,
        keys: c.max_keys,
    })
}

fn running(env: &HostEnv, id: &str) -> Result<()> {
    if !crate::state::Running::list(env).iter().any(|r| r.id == id) {
        bail!("no running sandbox with id {id}");
    }
    Ok(())
}

/// `paddock timer set`: a timer that the user owns, on a running sandbox.
pub fn host_set(env: &HostEnv, id: &str, name: &str, budget: u64, hard: bool) -> Result<Status> {
    check_name(name)?;
    check_budget(budget)?;
    running(env, id)?;
    let (file, ns) = self_file(env, id)?;
    let mut st = crate::kv::store::Store::open(&file)?;
    let now = crate::state::now();
    let key = format!("{PREFIX}{name}");
    let cur = st.get(&ns, &key, now)?;
    let t = Timer {
        name: name.into(),
        owner: "user".into(),
        subject: id.into(),
        started: now,
        budget,
        hard,
        stopped: None,
        announced: false,
        enforced: false,
        extensions: vec![],
    };
    save(
        &mut st,
        &ns,
        &t,
        Some(cur.map_or(0, |e| e.version)),
        &HOST,
        host_quota(env)?,
        now,
    )?;
    Ok(t.status(now, &frozen_periods(env, id, now)))
}

/// `paddock timer extend` and `paddock timer stop`: the user may change any
/// timer of the sandbox's own scope.
pub fn host_change(env: &HostEnv, id: &str, name: &str, by: Option<u64>) -> Result<Status> {
    check_name(name)?;
    let (file, ns) = self_file(env, id)?;
    if !file.is_file() {
        bail!("sandbox {id} has no timer {name}");
    }
    let mut st = crate::kv::store::Store::open(&file)?;
    let now = crate::state::now();
    let e = st
        .get(&ns, &format!("{PREFIX}{name}"), now)?
        .with_context(|| format!("sandbox {id} has no timer {name} in its own scope"))?;
    let mut t: Timer = serde_json::from_str(&e.value)?;
    match by {
        Some(by) => {
            t.budget = (t.budget + by).min(MAX_BUDGET);
            t.announced = false;
            t.enforced = false;
            t.extensions.push(Extension {
                seconds: by,
                by: "user".into(),
                at: now,
                reason: String::new(),
            });
        }
        None => t.stopped = Some(now),
    }
    save(
        &mut st,
        &ns,
        &t,
        Some(e.version),
        &HOST,
        host_quota(env)?,
        now,
    )?;
    Ok(t.status(now, &frozen_periods(env, id, now)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timer(started: u64, budget: u64) -> Timer {
        Timer {
            name: "fix".into(),
            owner: "user".into(),
            subject: "sb".into(),
            started,
            budget,
            hard: false,
            stopped: None,
            announced: false,
            enforced: false,
            extensions: vec![],
        }
    }

    #[test]
    fn frozen_time_does_not_count() {
        let t = timer(1000, 3600);
        let s = t.status(1000 + 1800, &[]);
        assert_eq!((s.used, s.left, s.over), (1800, 1800, false));
        // 10 minutes frozen inside the window, and a period before it.
        let s = t.status(1000 + 1800, &[(1100, 1700), (0, 500)]);
        assert_eq!((s.used, s.wall), (1200, 1800));
        let s = t.status(1000 + 4000, &[]);
        assert!(s.over);
        assert_eq!(s.left, 0);
        // A stopped timer stops counting.
        let mut t2 = timer(1000, 3600);
        t2.stopped = Some(1600);
        assert_eq!(t2.status(99_999, &[]).used, 600);
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("90").unwrap(), 90);
        assert_eq!(parse_duration("2h").unwrap(), 7200);
        assert_eq!(parse_duration("1h30m").unwrap(), 5400);
        assert_eq!(parse_duration("1d").unwrap(), 86400);
        for bad in ["", "h", "2x", "1h30"] {
            assert!(parse_duration(bad).is_err(), "{bad}");
        }
        assert_eq!(format_duration(5400), "1h30m");
        assert_eq!(format_duration(45), "45s");
        assert_eq!(format_duration(7200), "2h");
        assert!(check_name("task:fix-auth").is_ok());
        assert!(check_name("a b").is_err());
        assert!(check_budget(0).is_err() && check_budget(MAX_BUDGET + 1).is_err());
    }
}
