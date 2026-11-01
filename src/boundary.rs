//! The boundary log: what the sandbox boundary denied, one file per sandbox
//! (`boundary.jsonl` in its run folder).
//!
//! A program in a sandbox rarely says "permission denied". A path is missing,
//! a socket is gone, a LAN address times out, and the program falls back to
//! defaults in silence. paddock knows its own rules, so it writes each denial
//! here: the time, the kind, the target, the rule and a count. The same
//! denial within a minute raises the count of its line. The log never holds
//! content: no request text, no variable values, no bodies.

use crate::env::HostEnv;
use serde::{Deserialize, Serialize};
use std::io::Write as _;
use std::path::{Path, PathBuf};

pub const FILE: &str = "boundary.jsonl";
/// Denials of the same kind, target and rule within this window share a line.
pub const MERGE_SECS: u64 = 60;
/// The log keeps at most this many lines. Older lines go first.
const MAX_LINES: usize = 5000;
/// Targets are names, hosts and paths. Longer text is cut.
const MAX_TARGET: usize = 300;

/// The kinds of denial.
pub const HOST: &str = "host";
pub const NETWORK: &str = "network";
pub const ENV: &str = "env";
pub const MCP: &str = "mcp";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// Unix seconds of the first denial on this line.
    pub time: u64,
    pub kind: String,
    pub target: String,
    pub rule: String,
    pub count: u64,
    /// Unix seconds of the last denial on this line.
    #[serde(default)]
    pub last: u64,
}

impl Entry {
    pub fn new(kind: &str, target: &str, rule: &str) -> Self {
        let now = crate::state::now();
        Self {
            time: now,
            kind: kind.to_string(),
            target: crate::broker::clean_text(target)
                .chars()
                .take(MAX_TARGET)
                .collect(),
            rule: crate::broker::clean_text(rule).chars().take(500).collect(),
            count: 1,
            last: now,
        }
    }

    fn same(&self, o: &Entry) -> bool {
        self.kind == o.kind && self.target == o.target && self.rule == o.rule
    }
}

/// The log file of a sandbox.
pub fn file(env: &HostEnv, id: &str) -> PathBuf {
    env.paddock_run().join(id).join(FILE)
}

/// Record one denial in the log of sandbox `id`. Errors go to the debug log
/// only: a denial never fails because its log line failed.
pub fn record(env: &HostEnv, id: &str, kind: &str, target: &str, rule: &str) {
    if id.is_empty() {
        return;
    }
    let e = Entry::new(kind, target, rule);
    if let Err(err) = record_at(&file(env, id), e) {
        log::debug!("cannot write the boundary log of {id}: {err}");
    }
}

/// Add `e` to the log at `path`: raise the count of a line for the same
/// denial that started less than a minute ago, or append a new line.
pub fn record_at(path: &Path, e: Entry) -> std::io::Result<()> {
    let Some(dir) = path.parent() else {
        return Ok(());
    };
    crate::privfs::dir(dir)?;
    // One writer at a time: the broker, the proxy and the start can write.
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("boundary.lock"))?;
    use std::os::fd::AsRawFd as _;
    // SAFETY: flock on an open file descriptor; the lock goes with the file.
    unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) };
    let mut lines = read_at(path);
    if let Some(l) = lines
        .iter_mut()
        .rev()
        .find(|l| l.same(&e) && e.time.saturating_sub(l.time) < MERGE_SECS)
    {
        l.count += e.count;
        l.last = l.last.max(e.last);
        return write_all(path, &lines);
    }
    if lines.len() >= MAX_LINES {
        lines.drain(..=lines.len() - MAX_LINES);
        lines.push(e);
        return write_all(path, &lines);
    }
    let mut f = crate::privfs::append(path)?;
    writeln!(f, "{}", line(&e))
}

fn line(e: &Entry) -> String {
    serde_json::to_string(e).unwrap_or_default()
}

fn write_all(path: &Path, lines: &[Entry]) -> std::io::Result<()> {
    let mut s = String::new();
    for l in lines {
        s.push_str(&line(l));
        s.push('\n');
    }
    crate::privfs::write_atomic(path, s)
}

/// The lines of the log at `path`. Lines that do not parse are skipped.
pub fn read_at(path: &Path) -> Vec<Entry> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

pub fn read(env: &HostEnv, id: &str) -> Vec<Entry> {
    read_at(&file(env, id))
}

/// Write the denials that the build decided (`SandboxSpec::deny`): one line
/// each at the start.
pub fn record_start(env: &HostEnv, spec: &crate::spec::SandboxSpec) {
    for (kind, target, rule) in &spec.boundary {
        record(env, &spec.id, kind, target, rule);
    }
}

/// The sink of the host proxy: each refused target goes into the log of
/// sandbox `id`.
pub fn proxy_sink(env: &HostEnv, id: &str) -> crate::proxy::Boundary {
    let (env, id) = (env.clone(), id.to_string());
    crate::proxy::Boundary(std::sync::Arc::new(move |target: &str, rule: &str| {
        record(&env, &id, NETWORK, target, rule)
    }))
}

/// The number of denials in the log of sandbox `id`.
pub fn count(env: &HostEnv, id: &str) -> u64 {
    read(env, id).iter().map(|e| e.count).sum()
}

/// The denials of one kind and target, for `paddock boundary`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Group {
    pub kind: String,
    pub target: String,
    pub rules: Vec<String>,
    pub count: u64,
    pub first: u64,
    pub last: u64,
}

/// Group the lines by kind and target, in the order of the first denial.
pub fn group(lines: &[Entry]) -> Vec<Group> {
    let mut out: Vec<Group> = Vec::new();
    for l in lines {
        match out
            .iter_mut()
            .find(|g| g.kind == l.kind && g.target == l.target)
        {
            Some(g) => {
                g.count += l.count;
                g.first = g.first.min(l.time);
                g.last = g.last.max(l.last.max(l.time));
                if !g.rules.contains(&l.rule) {
                    g.rules.push(l.rule.clone());
                }
            }
            None => out.push(Group {
                kind: l.kind.clone(),
                target: l.target.clone(),
                rules: vec![l.rule.clone()],
                count: l.count,
                first: l.time,
                last: l.last.max(l.time),
            }),
        }
    }
    out.sort_by(|a, b| a.kind.cmp(&b.kind).then(a.first.cmp(&b.first)));
    out
}

/// The groups as text: a heading per kind, then one line per target.
pub fn render(groups: &[Group]) -> String {
    if groups.is_empty() {
        return "The sandbox boundary denied nothing.\n".to_string();
    }
    let mut s = String::new();
    let mut kind = "";
    for g in groups {
        if g.kind != kind {
            if !kind.is_empty() {
                s.push('\n');
            }
            kind = &g.kind;
            s.push_str(&format!("{kind}:\n"));
        }
        s.push_str(&format!(
            "  {:>5}x  {}  ({})\n",
            g.count,
            g.target,
            g.rules.join("; ")
        ));
    }
    s
}

/// The groups as JSON, for `--json` and `paddock-host boundary`.
pub fn to_json(lines: &[Entry]) -> serde_json::Value {
    let groups = group(lines);
    serde_json::json!({
        "count": lines.iter().map(|e| e.count).sum::<u64>(),
        "groups": groups,
    })
}

/// `paddock boundary ID [--json] [--follow]`.
pub fn cmd(env: &HostEnv, id: &str, json: bool, follow: bool) -> anyhow::Result<i32> {
    if id.is_empty() || id.contains('/') || id.starts_with('.') {
        anyhow::bail!("{id} is not a sandbox id");
    }
    let path = file(env, id);
    let running = crate::state::Running::list(env).iter().any(|r| r.id == id);
    if !running && !path.exists() {
        anyhow::bail!("no running sandbox with id {id} (paddock ps lists them)");
    }
    let lines = read_at(&path);
    if json {
        let mut v = to_json(&lines);
        v["api_version"] = "paddock.boundary/v1".into();
        v["sandbox"] = id.into();
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        print!("{}", render(&group(&lines)));
    }
    if !follow {
        return Ok(0);
    }
    let mut seen = lines;
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
        let now = read_at(&path);
        for e in changed(&seen, &now) {
            if json {
                println!("{}", line(e));
            } else {
                println!(
                    "{}  {}  {}  ({}, {}x)",
                    e.last, e.kind, e.target, e.rule, e.count
                );
            }
        }
        seen = now;
        if !path.exists() && !crate::state::Running::list(env).iter().any(|r| r.id == id) {
            return Ok(0);
        }
    }
}

/// The lines of `now` that are new or have a higher count than in `before`.
fn changed<'a>(before: &[Entry], now: &'a [Entry]) -> Vec<&'a Entry> {
    now.iter()
        .filter(|e| {
            !before
                .iter()
                .any(|b| b.same(e) && b.time == e.time && b.count >= e.count)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(t: u64, kind: &str, target: &str, rule: &str) -> Entry {
        Entry {
            time: t,
            last: t,
            ..Entry::new(kind, target, rule)
        }
    }

    #[test]
    fn line_format() {
        let e = at(100, NETWORK, "10.0.0.5:22", "lan");
        let v: serde_json::Value = serde_json::from_str(&line(&e)).unwrap();
        assert_eq!(v["time"], 100);
        assert_eq!(v["kind"], "network");
        assert_eq!(v["target"], "10.0.0.5:22");
        assert_eq!(v["rule"], "lan");
        assert_eq!(v["count"], 1);
        assert_eq!(v["last"], 100);
    }

    #[test]
    fn control_characters_leave_the_target() {
        let e = Entry::new(HOST, "a\u{1b}[2Jb\u{202e}", "r");
        assert_eq!(e.target, "a[2Jb");
    }

    #[test]
    fn the_same_denial_within_a_minute_raises_the_count() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("run").join(FILE);
        record_at(&p, at(1000, HOST, "fetch ~/.ssh", "protected")).unwrap();
        record_at(&p, at(1030, HOST, "fetch ~/.ssh", "protected")).unwrap();
        record_at(&p, at(1059, HOST, "fetch ~/.ssh", "protected")).unwrap();
        // Another rule or target: a new line.
        record_at(&p, at(1040, HOST, "fetch ~/.ssh", "other")).unwrap();
        record_at(&p, at(1040, HOST, "fetch ~/.aws", "protected")).unwrap();
        // A minute after the first: a new line.
        record_at(&p, at(1060, HOST, "fetch ~/.ssh", "protected")).unwrap();
        let lines = read_at(&p);
        assert_eq!(lines.len(), 4);
        assert_eq!(
            (lines[0].count, lines[0].time, lines[0].last),
            (3, 1000, 1059)
        );
        assert_eq!(lines[3].count, 1);
        assert_eq!(lines[3].time, 1060);
    }

    #[test]
    fn grouping_by_kind_and_target() {
        let lines = vec![
            at(10, NETWORK, "10.0.0.5:22", "lan"),
            at(11, HOST, "exec", "deny"),
            Entry {
                count: 4,
                ..at(80, NETWORK, "10.0.0.5:22", "lan")
            },
            at(90, NETWORK, "10.0.0.5:22", "other"),
            at(12, ENV, "API_TOKEN", "secret"),
        ];
        let g = group(&lines);
        assert_eq!(g.len(), 3);
        assert_eq!(g[0].kind, "env");
        assert_eq!(g[1].kind, "host");
        let n = &g[2];
        assert_eq!((n.count, n.first, n.last), (6, 10, 90));
        assert_eq!(n.rules, vec!["lan", "other"]);
        let text = render(&g);
        assert!(text.contains("network:\n      6x  10.0.0.5:22  (lan; other)"));
        assert_eq!(to_json(&lines)["count"], 8);
        assert!(render(&[]).contains("denied nothing"));
    }

    #[test]
    fn follow_prints_new_and_raised_lines() {
        let a = at(10, HOST, "x", "r");
        let b = Entry {
            count: 2,
            ..a.clone()
        };
        let c = at(20, NETWORK, "y", "r");
        assert!(changed(std::slice::from_ref(&a), std::slice::from_ref(&a)).is_empty());
        assert_eq!(changed(&[a], &[b.clone(), c.clone()]), vec![&b, &c]);
    }
}
