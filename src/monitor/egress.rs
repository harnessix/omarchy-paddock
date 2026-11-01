//! Egress targets: the host proxy tells this module each host that it
//! connects for a sandbox. It keeps the hosts of each harness, with numbers
//! only, and tells the user once per sandbox and reason when a harness with
//! some history reaches a host it never reached before, or many new hosts
//! in a short time.
//!
//! - It sees docker and podman sandboxes and network mode `api` on every
//!   backend. bwrap and srt sandboxes in `egress` mode do not use this
//!   proxy, so it does not see them.
//! - It records the host name, never a path or content.
//! - `[monitor] egress = false` in the user configuration turns it off.

use super::{notify_now, MonitorCfg, CAPABILITY};
use crate::env::HostEnv;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// The history keeps the hosts of the last 90 days.
const KEEP_SECS: u64 = 90 * 24 * 3600;
/// A harness has history after this many sandbox runs...
const MIN_RUNS: u64 = 20;
/// ...or after its first recorded run is this old.
const MIN_AGE: u64 = 7 * 24 * 3600;
/// More than this many new hosts in `BURST_SECS` make an alert.
const BURST: usize = 10;
const BURST_SECS: u64 = 600;

/// One host of a harness.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Host {
    pub first: u64,
    pub last: u64,
    pub count: u64,
}

/// The content of `egress-<harness>.json`.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct History {
    /// The time of the first recorded run. 0: none yet.
    #[serde(default)]
    pub since: u64,
    /// The sandbox runs that connected somewhere.
    #[serde(default)]
    pub runs: u64,
    #[serde(default)]
    pub hosts: BTreeMap<String, Host>,
}

impl History {
    pub fn load(file: &Path) -> Self {
        std::fs::read_to_string(file)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    fn save(&self, file: &Path) -> std::io::Result<()> {
        crate::privfs::write_atomic(file, serde_json::to_string(self).unwrap_or_default())
    }

    /// Drop the hosts older than 90 days. With no host left, the history
    /// starts again.
    fn trim(&mut self, now: u64) {
        self.hosts
            .retain(|_, h| h.last.saturating_add(KEEP_SECS) >= now);
        if self.hosts.is_empty() {
            self.since = 0;
            self.runs = 0;
        }
    }

    /// The harness has enough history for alerts.
    fn armed(&self, now: u64) -> bool {
        self.since > 0 && (self.runs >= MIN_RUNS || now.saturating_sub(self.since) >= MIN_AGE)
    }

    /// Count one connection. It returns true for a host that is new.
    fn add(&mut self, host: &str, now: u64) -> bool {
        let new = !self.hosts.contains_key(host);
        let h = self.hosts.entry(host.to_string()).or_insert(Host {
            first: now,
            ..Host::default()
        });
        h.last = now;
        h.count += 1;
        new
    }
}

/// The history file of a harness.
pub fn history_file(env: &HostEnv, harness: &str) -> PathBuf {
    env.paddock_state()
        .join("monitor")
        .join(format!("egress-{}.json", super::file_name(harness)))
}

/// The egress findings of a sandbox, `monitor-egress.json` in its run
/// folder: reason (`new_host`, `new_hosts`) and its detail.
pub fn found_file(env: &HostEnv, sandbox: &str) -> PathBuf {
    env.paddock_run().join(sandbox).join("monitor-egress.json")
}

pub(super) fn load_found(file: &Path) -> BTreeMap<String, String> {
    std::fs::read_to_string(file)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// One alert: the reason and its detail (a host, or a count of hosts).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alert {
    pub reason: &'static str,
    pub detail: String,
}

/// The egress record of one sandbox run.
pub struct Tracker {
    history: PathBuf,
    found_file: PathBuf,
    found: BTreeMap<String, String>,
    /// Whether the harness had history when this run began. None: the run
    /// made no connection yet.
    armed: Option<bool>,
    /// The times of the recent new hosts.
    recent: VecDeque<u64>,
}

impl Tracker {
    pub fn new(history: PathBuf, found_file: PathBuf) -> Self {
        let found = load_found(&found_file);
        Tracker {
            history,
            found_file,
            found,
            armed: None,
            recent: VecDeque::new(),
        }
    }

    /// Record a connection to `host` and return the new alerts.
    pub fn connect(&mut self, host: &str, now: u64) -> Vec<Alert> {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        if host.is_empty() {
            return Vec::new();
        }
        let Some(new) = self.record(&host, now) else {
            return Vec::new();
        };
        if !new || self.armed != Some(true) {
            return Vec::new();
        }
        self.recent.push_back(now);
        while self
            .recent
            .front()
            .is_some_and(|t| now.saturating_sub(*t) > BURST_SECS)
        {
            self.recent.pop_front();
        }
        let mut out = Vec::new();
        if !self.found.contains_key("new_host") {
            out.push(Alert {
                reason: "new_host",
                detail: host,
            });
        }
        if self.recent.len() > BURST && !self.found.contains_key("new_hosts") {
            out.push(Alert {
                reason: "new_hosts",
                detail: self.recent.len().to_string(),
            });
        }
        if !out.is_empty() {
            for a in &out {
                self.found.insert(a.reason.into(), a.detail.clone());
            }
            let body = serde_json::to_string(&self.found).unwrap_or_default();
            if let Err(e) = crate::privfs::write(&self.found_file, body) {
                log::warn!("cannot write {}: {e}", self.found_file.display());
            }
        }
        out
    }

    /// Add the host to the history file. It returns whether it is new, or
    /// None when the file cannot be written.
    fn record(&mut self, host: &str, now: u64) -> Option<bool> {
        let dir = self.history.parent()?;
        if let Err(e) = crate::privfs::dir(dir) {
            log::warn!("cannot create {}: {e}", dir.display());
            return None;
        }
        // One writer at a time: each sandbox of the harness writes the file.
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.history.with_extension("lock"))
            .ok()?;
        use std::os::fd::AsRawFd as _;
        // SAFETY: flock on an open file descriptor; the lock goes with the file.
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) };
        let mut h = History::load(&self.history);
        h.trim(now);
        if self.armed.is_none() {
            self.armed = Some(h.armed(now));
            if h.since == 0 {
                h.since = now;
            }
            h.runs += 1;
        }
        let new = h.add(host, now);
        if let Err(e) = h.save(&self.history) {
            log::warn!("cannot write {}: {e}", self.history.display());
        }
        Some(new)
    }
}

/// The egress sink of the host proxy for one sandbox, or None when the user
/// turned it off.
pub fn sink(
    env: &HostEnv,
    sandbox: &str,
    harness: &str,
    project: &Path,
) -> Option<crate::proxy::Egress> {
    let cfg = MonitorCfg::load(env);
    if !cfg.egress {
        return None;
    }
    let tracker = Mutex::new(Tracker::new(
        history_file(env, harness),
        found_file(env, sandbox),
    ));
    let (env, sandbox, harness, project) = (
        env.clone(),
        sandbox.to_string(),
        harness.to_string(),
        project.to_path_buf(),
    );
    Some(crate::proxy::Egress(Arc::new(move |host: &str| {
        let alerts = match tracker.lock() {
            Ok(mut t) => t.connect(host, crate::state::now()),
            Err(_) => return,
        };
        for a in alerts {
            report(&env, &sandbox, &harness, &project, &a, cfg.notify);
        }
    })))
}

/// One alert: an audit line with the host or the count, and a notification.
fn report(env: &HostEnv, sandbox: &str, harness: &str, project: &Path, a: &Alert, notify: bool) {
    let detail = match a.reason {
        "new_host" => format!(
            "{sandbox} connected to {}, a host that {harness} never reached before",
            a.detail
        ),
        _ => format!(
            "{sandbox} connected to {} new hosts within 10 minutes",
            a.detail
        ),
    };
    crate::broker::audit_line(
        env,
        &serde_json::json!({
            "time": crate::state::now(),
            "sandbox": sandbox,
            "harness": harness,
            "project": project,
            "capability": CAPABILITY,
            "reason": a.reason,
            "target": a.detail,
            "detail": detail,
            "decision": "reported",
        }),
    );
    if notify_now(env, project, notify) {
        let _ = crate::broker::notify::show(
            env,
            &crate::broker::notify::Note {
                summary: "a sandbox reached a new host".into(),
                body: format!(
                    "{detail}. paddock log shows it. paddock stop {sandbox} ends the sandbox."
                ),
                actions: Vec::new(),
                critical: false,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 24 * 3600;

    fn tracker(dir: &Path, sandbox: &str) -> Tracker {
        Tracker::new(
            dir.join("egress-h.json"),
            dir.join(format!("{sandbox}.json")),
        )
    }

    /// `runs` earlier runs that each reached `known.example`.
    fn with_runs(dir: &Path, runs: u64, now: u64) {
        for i in 0..runs {
            tracker(dir, &format!("old{i}")).connect("known.example", now);
        }
    }

    #[test]
    fn recording_and_the_90_day_trim() {
        let d = tempfile::tempdir().unwrap();
        let mut t = tracker(d.path(), "a");
        t.connect("Old.Example.", 100);
        t.connect("old.example", 200);
        let h = History::load(&d.path().join("egress-h.json"));
        assert_eq!(
            h.hosts["old.example"],
            Host {
                first: 100,
                last: 200,
                count: 2
            }
        );
        assert_eq!((h.since, h.runs), (100, 1));
        let later = 200 + 80 * DAY;
        tracker(d.path(), "b").connect("new.example", later);
        let h = History::load(&d.path().join("egress-h.json"));
        assert_eq!(h.hosts.len(), 2);
        assert_eq!(h.runs, 2);
        tracker(d.path(), "c").connect("new.example", 201 + 90 * DAY);
        let h = History::load(&d.path().join("egress-h.json"));
        assert_eq!(h.hosts.keys().collect::<Vec<_>>(), ["new.example"]);
        // Only numbers and host names in the file.
        let text = std::fs::read_to_string(d.path().join("egress-h.json")).unwrap();
        assert!(!text.contains('/'));
    }

    #[test]
    fn the_history_threshold() {
        let mut h = History::default();
        assert!(!h.armed(1000));
        h.since = 1000;
        h.runs = MIN_RUNS - 1;
        assert!(!h.armed(1000 + MIN_AGE - 1));
        assert!(h.armed(1000 + MIN_AGE));
        h.runs = MIN_RUNS;
        assert!(h.armed(1001));
        // With no host left after the trim, the history starts again.
        h.trim(1001 + KEEP_SECS);
        assert!(!h.armed(1001 + KEEP_SECS));
    }

    #[test]
    fn a_new_install_does_not_alert() {
        let d = tempfile::tempdir().unwrap();
        let mut t = tracker(d.path(), "a");
        for i in 0..30 {
            assert!(t.connect(&format!("h{i}.example"), 100).is_empty());
        }
        with_runs(d.path(), MIN_RUNS - 2, 100);
        assert!(tracker(d.path(), "b")
            .connect("other.example", 100)
            .is_empty());
    }

    #[test]
    fn a_new_host_alerts_once_per_sandbox() {
        let d = tempfile::tempdir().unwrap();
        with_runs(d.path(), MIN_RUNS, 100);
        let mut t = tracker(d.path(), "a");
        assert!(t.connect("known.example", 200).is_empty());
        let got = t.connect("new.example", 200);
        assert_eq!(
            got,
            [Alert {
                reason: "new_host",
                detail: "new.example".into()
            }]
        );
        assert!(t.connect("second.example", 200).is_empty());
        // A new tracker of the same sandbox keeps the finding.
        assert!(tracker(d.path(), "a")
            .connect("third.example", 200)
            .is_empty());
        // The host is known now, so another sandbox gets no alert for it.
        assert!(tracker(d.path(), "b")
            .connect("new.example", 200)
            .is_empty());
        assert_eq!(
            tracker(d.path(), "c").connect("fourth.example", 200).len(),
            1
        );
    }

    #[test]
    fn the_age_arms_the_rules() {
        let d = tempfile::tempdir().unwrap();
        with_runs(d.path(), 1, 100);
        assert!(tracker(d.path(), "a")
            .connect("x.example", 100 + DAY)
            .is_empty());
        assert_eq!(
            tracker(d.path(), "b")
                .connect("y.example", 100 + MIN_AGE)
                .len(),
            1
        );
    }

    #[test]
    fn the_burst_rule() {
        let d = tempfile::tempdir().unwrap();
        with_runs(d.path(), MIN_RUNS, 100);
        let mut t = tracker(d.path(), "a");
        // Eleven new hosts spread over more than 10 minutes: no burst.
        let mut reasons = Vec::new();
        for i in 0..11u64 {
            for a in t.connect(&format!("slow{i}.example"), 1000 + i * 61) {
                reasons.push(a.reason);
            }
        }
        assert_eq!(reasons, ["new_host"]);
        // Eleven new hosts in 10 minutes: one burst, once.
        let start = 10_000;
        let mut burst = Vec::new();
        for i in 0..20u64 {
            burst.extend(t.connect(&format!("fast{i}.example"), start + i * 20));
        }
        assert_eq!(
            burst,
            [Alert {
                reason: "new_hosts",
                detail: "11".into()
            }]
        );
        // Known hosts never count toward a burst.
        let mut u = tracker(d.path(), "b");
        for i in 0..20u64 {
            assert!(u.connect(&format!("fast{i}.example"), start).is_empty());
        }
    }
}
