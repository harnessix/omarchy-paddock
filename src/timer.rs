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
//! - A timer can also have a quota budget: a share of one limit window of
//!   the account that the sandbox uses ("at most 1% of the weekly limit").
//!   It counts what the account used since the timer started, from the
//!   forage ledger (forage.rs). The ledger knows the account, not the
//!   sandbox: what other sandboxes and apps on the same login use counts
//!   too. Over budget is handled as a deadline: soft tells, hard freezes.
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Timer {
    pub name: String,
    /// The sandbox that set it, or `user`.
    pub owner: String,
    /// The sandbox whose time it counts and that a hard deadline freezes.
    pub subject: String,
    pub started: u64,
    /// Seconds; 0 when only a quota budget limits the task.
    pub budget: u64,
    pub hard: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota: Option<Quota>,
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

/// A quota budget: a share of one limit window of an account.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Quota {
    /// The Omarchy agent whose limits count (forage.rs).
    pub agent: String,
    /// The label of the window, as the usage record names it.
    pub window: String,
    /// The share that the task may use, 0 to 1.
    pub share: f64,
    /// The numbers when the timer started: share used, reset, fetch time.
    pub base: f64,
    pub base_resets: u64,
    pub base_at: u64,
}

/// A quota budget as `status` reports it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaStatus {
    pub agent: String,
    pub window: String,
    pub share: f64,
    /// The share that the account used since the start.
    pub used: f64,
    pub left: f64,
    pub over: bool,
    /// When the newest numbers were fetched.
    pub fetched: u64,
}

impl Quota {
    /// What the account used since the start, from the window's samples.
    /// A window that reset (its reset time moved, or its share went down)
    /// starts again from 0.
    pub fn status(&self, samples: &[crate::forage::Sample]) -> QuotaStatus {
        let (mut used, mut resets, mut eaten, mut fetched) =
            (self.base, self.base_resets, 0.0, self.base_at);
        for s in samples.iter().filter(|s| s.at > self.base_at) {
            let reset = s.used < used || s.resets.abs_diff(resets) > 60;
            eaten += if reset { s.used } else { s.used - used };
            (used, resets, fetched) = (s.used, s.resets, s.at);
        }
        QuotaStatus {
            agent: self.agent.clone(),
            window: self.window.clone(),
            share: self.share,
            used: eaten,
            left: (self.share - eaten).max(0.0),
            over: eaten >= self.share,
            fetched,
        }
    }
}

/// `weekly=1%`: a window and a share. The window is a label of the usage
/// record, or the first word of one, in any case.
pub fn parse_quota(s: &str) -> Result<(String, f64)> {
    let (window, pct) = s
        .rsplit_once('=')
        .with_context(|| format!("'{s}' is not a quota budget: use for example weekly=1%"))?;
    let share = parse_share(pct)?;
    if window.trim().is_empty() {
        bail!("'{s}' names no window: use for example weekly=1%");
    }
    Ok((window.trim().to_string(), share))
}

/// `10%` or `10`: a share of a window, 0 to 1.
pub fn parse_share(s: &str) -> Result<f64> {
    let n: f64 = s
        .trim()
        .trim_end_matches('%')
        .parse()
        .with_context(|| format!("'{s}' is not a percentage"))?;
    if !(n > 0.0 && n <= 100.0) {
        bail!("a quota budget must be more than 0% and at most 100%");
    }
    Ok(n / 100.0)
}

/// The label that a window name means among an agent's labels.
pub fn find_window(labels: &[String], want: &str) -> Result<String> {
    let w = want.to_lowercase();
    if let Some(l) = labels.iter().find(|l| l.to_lowercase() == w) {
        return Ok(l.clone());
    }
    let first: Vec<&String> = labels
        .iter()
        .filter(|l| l.split_whitespace().next().map(str::to_lowercase) == Some(w.clone()))
        .collect();
    match first.as_slice() {
        [one] => Ok((*one).clone()),
        [] => bail!(
            "no limit window '{want}'; the account has: {}",
            if labels.is_empty() {
                "none".to_string()
            } else {
                labels.join(", ")
            }
        ),
        many => bail!(
            "'{want}' could be {}; name the whole window",
            many.iter()
                .map(|l| l.as_str())
                .collect::<Vec<_>>()
                .join(" or ")
        ),
    }
}

/// A quota budget from now: the newest numbers of the window are the base.
/// Numbers older than `forage::MIN_ASK` are refreshed first.
pub fn quota_from_now(env: &HostEnv, agent: &str, window: &str, share: f64) -> Result<Quota> {
    let now = crate::state::now();
    let fresh = |env: &HostEnv| {
        crate::forage::windows(env, agent)
            .iter()
            .any(|w| !w.stale && w.fetched + crate::forage::MIN_ASK > now)
    };
    let known = !crate::forage::windows(env, agent).is_empty();
    if !known {
        bail!("paddock has no usage numbers for {agent}, so it cannot count a quota budget; `paddock forage` lists the accounts that have them");
    }
    if !fresh(env) {
        crate::forage::refresh(env, &[agent.to_string()])?;
    }
    crate::forage::record(env, agent)?;
    let ws = crate::forage::windows(env, agent);
    let labels: Vec<String> = ws.iter().map(|w| w.label.clone()).collect();
    let label = find_window(&labels, window)?;
    let w = ws.iter().find(|w| w.label == label).expect("found above");
    Ok(Quota {
        agent: agent.into(),
        window: label,
        share,
        base: w.used,
        base_resets: w.resets.unwrap_or(0),
        base_at: w.fetched,
    })
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Extension {
    pub seconds: u64,
    /// The share of the quota budget added, 0 to 1.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub share: f64,
    pub by: String,
    pub at: u64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
}

fn is_zero(x: &f64) -> bool {
    *x == 0.0
}

/// A timer as `status` reports it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Status {
    pub name: String,
    pub owner: String,
    pub subject: String,
    pub hard: bool,
    pub started: u64,
    /// Seconds that count: wall time without frozen periods.
    pub used: u64,
    pub budget: u64,
    /// Seconds left; 0 when over, or when there is no time budget.
    pub left: u64,
    /// The time or the quota budget is used up.
    pub over: bool,
    pub stopped: bool,
    /// Wall seconds since the start, frozen periods included.
    pub wall: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota: Option<QuotaStatus>,
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
            over: self.budget > 0 && used >= self.budget,
            stopped: self.stopped.is_some(),
            wall,
            quota: None,
        }
    }

    /// The status with the quota budget counted from the forage ledger.
    pub fn status_in(&self, env: &HostEnv, now: u64) -> Status {
        let mut s = self.status(now, &frozen_periods(env, &self.subject, now));
        if let Some(q) = &self.quota {
            let mut samples = crate::forage::samples(env, &q.agent, &q.window);
            // A stopped timer counts up to its stop.
            if let Some(end) = self.stopped {
                samples.retain(|x| x.at <= end);
            }
            let qs = q.status(&samples);
            s.over |= qs.over;
            s.quota = Some(qs);
        }
        s
    }

    /// What the timer used up, for notices: "its 2h", "10% of codex Weekly".
    pub fn what_is_up(&self, s: &Status) -> String {
        let time = self.budget > 0 && s.used >= self.budget;
        match (&s.quota, time) {
            (Some(q), false) if q.over => format!(
                "its quota budget, {:.0}% of {} {}",
                q.share * 100.0,
                q.agent,
                q.window
            ),
            _ => format!("its {}", format_duration(self.budget)),
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
    std::fs::read_to_string(summary_file(env, sandbox))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Write the summary at once after a change, so that readers do not wait
/// for the 30-second tick. A failure only delays it until the tick.
fn write_summary_now(env: &HostEnv, sandbox: &str) {
    if let Err(e) = crate::kv::refresh_timer_summary(env, &crate::kv::kv_root(env), sandbox) {
        log::debug!("timer summary of {sandbox}: {e:#}");
    }
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

fn running(env: &HostEnv, id: &str) -> Result<crate::state::Running> {
    crate::state::Running::list(env)
        .into_iter()
        .find(|r| r.id == id)
        .with_context(|| format!("no running sandbox with id {id}"))
}

/// Check the budgets of a new timer: a time budget, a quota budget, or both.
pub fn check_budgets(budget: u64, quota: bool) -> Result<()> {
    if budget == 0 && !quota {
        bail!("a timer needs a time budget (--budget), a quota budget (--quota), or both");
    }
    if budget > 0 {
        check_budget(budget)?;
    }
    Ok(())
}

/// `paddock timer set`: a timer that the user owns, on a running sandbox.
/// `quota` is a window name and a share of it, on the sandbox's account.
pub fn host_set(
    env: &HostEnv,
    id: &str,
    name: &str,
    budget: u64,
    hard: bool,
    quota: Option<(String, f64)>,
) -> Result<Status> {
    check_name(name)?;
    check_budgets(budget, quota.is_some())?;
    let r = running(env, id)?;
    let quota = match quota {
        Some((window, share)) => Some(quota_from_now(
            env,
            &crate::forage::agent_of(&r.harness),
            &window,
            share,
        )?),
        None => None,
    };
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
        quota,
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
    write_summary_now(env, id);
    Ok(t.status_in(env, now))
}

/// "1h10m of 2h, 4% of 10% of codex Weekly": what a timer used, for people.
pub fn describe(s: &Status) -> String {
    let mut parts = Vec::new();
    if s.budget > 0 {
        parts.push(format!(
            "{} of {}",
            format_duration(s.used),
            format_duration(s.budget)
        ));
    }
    if let Some(q) = &s.quota {
        parts.push(format!(
            "{:.0}% of {:.0}% of {} {}",
            q.used * 100.0,
            q.share * 100.0,
            q.agent,
            q.window
        ));
    }
    parts.join(", ")
}

/// What `extend` adds: time, or a share of the quota budget.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum More {
    Time(u64),
    Share(f64),
}

/// `30m` or `1%`.
pub fn parse_more(s: &str) -> Result<More> {
    if s.trim().ends_with('%') {
        Ok(More::Share(parse_share(s)?))
    } else {
        Ok(More::Time(parse_duration(s)?))
    }
}

/// Add time or quota to a timer. Fails when the timer has no budget of that
/// kind. Both deadlines count again.
pub fn add_more(t: &mut Timer, more: More, by: &str, at: u64, reason: String) -> Result<()> {
    let (seconds, share) = match more {
        More::Time(n) => {
            if t.budget == 0 {
                bail!(
                    "the timer {} has no time budget; add quota with --by 1%",
                    t.name
                );
            }
            t.budget = (t.budget + n).min(MAX_BUDGET);
            (n, 0.0)
        }
        More::Share(x) => {
            let Some(q) = t.quota.as_mut() else {
                bail!(
                    "the timer {} has no quota budget; add time with --by 30m",
                    t.name
                );
            };
            q.share = (q.share + x).min(1.0);
            (0, x)
        }
    };
    t.announced = false;
    t.enforced = false;
    t.extensions.push(Extension {
        seconds,
        share,
        by: by.into(),
        at,
        reason,
    });
    Ok(())
}

/// `paddock timer extend` and `paddock timer stop`: the user may change any
/// timer of the sandbox's own scope.
pub fn host_change(env: &HostEnv, id: &str, name: &str, by: Option<More>) -> Result<Status> {
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
        Some(more) => add_more(&mut t, more, "user", now, String::new())?,
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
    write_summary_now(env, id);
    Ok(t.status_in(env, now))
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
            quota: None,
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

    fn sample(used: f64, resets: u64, at: u64) -> crate::forage::Sample {
        crate::forage::Sample {
            label: "Weekly (7-day)".into(),
            used,
            resets,
            at,
        }
    }

    #[test]
    fn a_quota_budget_counts_what_the_account_used_since_the_start() {
        let q = Quota {
            agent: "codex".into(),
            window: "Weekly (7-day)".into(),
            share: 0.10,
            base: 0.40,
            base_resets: 9000,
            base_at: 100,
        };
        // Nothing new yet.
        let s = q.status(&[sample(0.40, 9000, 100)]);
        assert_eq!((s.used, s.over), (0.0, false));
        // Older samples do not count; 4% since the start.
        let s = q.status(&[sample(0.10, 9000, 50), sample(0.44, 9000, 200)]);
        assert!(
            (s.used - 0.04).abs() < 1e-9 && (s.left - 0.06).abs() < 1e-9,
            "{s:?}"
        );
        assert_eq!(s.fetched, 200);
        // The window resets: what was used before it, and all after it.
        let s = q.status(&[
            sample(0.47, 9000, 200),
            sample(0.03, 20000, 300),
            sample(0.05, 20000, 400),
        ]);
        assert!((s.used - 0.12).abs() < 1e-9 && s.over, "{s:?}");
        // A reset time that moves by a few seconds is the same window.
        let s = q.status(&[sample(0.45, 9030, 200)]);
        assert!((s.used - 0.05).abs() < 1e-9, "{s:?}");
    }

    #[test]
    fn quota_budgets_from_the_command_line() {
        assert_eq!(parse_quota("weekly=10%").unwrap(), ("weekly".into(), 0.10));
        assert_eq!(
            parse_quota("Fable Weekly=2.5").unwrap(),
            ("Fable Weekly".into(), 0.025)
        );
        for bad in ["weekly", "=10%", "weekly=0%", "weekly=101%", "weekly=lots"] {
            assert!(parse_quota(bad).is_err(), "{bad}");
        }
        assert_eq!(parse_more("30m").unwrap(), More::Time(1800));
        assert_eq!(parse_more("5%").unwrap(), More::Share(0.05));
        let labels: Vec<String> = ["Session (5-hour)", "Weekly (7-day)", "Fable Weekly"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(find_window(&labels, "weekly").unwrap(), "Weekly (7-day)");
        assert_eq!(find_window(&labels, "SESSION").unwrap(), "Session (5-hour)");
        assert_eq!(
            find_window(&labels, "fable weekly").unwrap(),
            "Fable Weekly"
        );
        let e = find_window(&labels, "monthly").unwrap_err().to_string();
        assert!(e.contains("Weekly (7-day)"), "{e}");
        let two: Vec<String> = vec!["Weekly a".into(), "Weekly b".into()];
        assert!(find_window(&two, "weekly").is_err());
    }

    #[test]
    fn extend_adds_the_kind_of_budget_that_the_timer_has() {
        let mut t = timer(1000, 0);
        assert!(add_more(&mut t, More::Time(600), "user", 1, String::new()).is_err());
        t.quota = Some(Quota {
            agent: "codex".into(),
            window: "Weekly".into(),
            share: 0.10,
            base: 0.0,
            base_resets: 0,
            base_at: 0,
        });
        t.announced = true;
        add_more(&mut t, More::Share(0.05), "user", 1, String::new()).unwrap();
        assert!((t.quota.as_ref().unwrap().share - 0.15).abs() < 1e-9);
        assert!(!t.announced);
        assert_eq!(t.extensions[0].share, 0.05);
        // A quota-only timer is never over for time.
        assert!(!t.status(99_999, &[]).over);
        let mut timed = timer(1000, 60);
        assert!(add_more(&mut timed, More::Share(0.05), "user", 1, String::new()).is_err());
        assert!(check_budgets(0, false).is_err() && check_budgets(0, true).is_ok());
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
