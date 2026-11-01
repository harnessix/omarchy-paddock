//! Budgets and the ledger of paid devices.
//!
//! A user device with `cost` or `cost_header` costs money per request. It
//! needs a budget per run before its first request: `budget` in its table,
//! or the answer to the first-use card. A run is a spawn tree: a child
//! spends the budget of the root sandbox, never a budget of its own.
//!
//! Amounts are micro units (one millionth) of a currency that the user
//! picks. paddock does not convert.
//!
//! The ledger is one SQLite file on the host (`devices/ledger.db` in the
//! paddock state folder). Every sandbox's paddock process opens it, and a
//! write transaction checks and reserves the cost of a request in one
//! step, so two sandboxes of one tree cannot pass the budget together. A
//! row holds the time, the sandbox, its root, the project, the device, the
//! cost and the status. Never a body.

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::Serialize;
use std::path::{Path, PathBuf};

/// One unit in micro units.
pub const UNIT: u64 = 1_000_000;
/// The largest amount in a configuration or a command: one million units.
pub const MAX_AMOUNT: f64 = 1_000_000.0;
/// The budgets that the first-use card offers.
pub const CARD_CHOICES: &[&str] = &["0.10", "1", "5"];
/// The status of a request that the budget stopped.
pub const BUDGET_USED: &str = "budget used";

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS requests (
    id INTEGER PRIMARY KEY,
    time INTEGER NOT NULL,
    sandbox TEXT NOT NULL,
    root TEXT NOT NULL,
    project TEXT NOT NULL,
    device TEXT NOT NULL,
    cost INTEGER NOT NULL,
    status TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS requests_root ON requests(root, device);
CREATE TABLE IF NOT EXISTS budgets (
    root TEXT NOT NULL,
    device TEXT NOT NULL,
    budget INTEGER NOT NULL,
    notified INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (root, device)
);
";

/// The ledger file.
pub fn path(env: &crate::env::HostEnv) -> PathBuf {
    env.paddock_state().join("devices").join("ledger.db")
}

/// An amount from text or TOML: a number from 0 to one million, in micro
/// units.
pub fn amount(v: f64) -> Option<u64> {
    (v.is_finite() && (0.0..=MAX_AMOUNT).contains(&v)).then(|| (v * UNIT as f64).round() as u64)
}

/// Parse an amount like `1.0` or `0.10`.
pub fn parse_amount(s: &str) -> Result<u64> {
    let v: f64 = s
        .trim()
        .parse()
        .with_context(|| format!("{s:?} is not an amount like 1.0"))?;
    match amount(v) {
        Some(n) => Ok(n),
        None => bail!("{s:?}: an amount from 0 to {MAX_AMOUNT}"),
    }
}

/// An amount for people: at least two decimals, at most six.
pub fn money(micros: u64) -> String {
    let s = format!("{}.{:06}", micros / UNIT, micros % UNIT);
    let trimmed = s.trim_end_matches('0');
    let dot = trimmed.find('.').unwrap_or(trimmed.len());
    let decimals = trimmed.len() - dot - 1;
    if decimals < 2 {
        format!("{trimmed}{}", "0".repeat(2 - decimals))
    } else {
        trimmed.to_string()
    }
}

/// The cost a service reports in the header `name` of an HTTP response,
/// or None when the header is missing or not an amount.
pub fn header_cost(response: &[u8], name: &str) -> Option<u64> {
    let text = String::from_utf8_lossy(response);
    text.lines()
        .skip(1)
        .take_while(|l| !l.trim_end_matches('\r').is_empty())
        .find_map(|line| {
            let (n, v) = line.split_once(':')?;
            if !n.trim().eq_ignore_ascii_case(name) {
                return None;
            }
            amount(v.trim().parse::<f64>().ok()?)
        })
}

/// The cost of a request: the larger of the declared cost and the cost
/// that the service reports.
pub fn charge(declared: u64, reported: Option<u64>) -> u64 {
    declared.max(reported.unwrap_or(0))
}

/// The root of the spawn tree of a sandbox: the first sandbox of
/// `started_by`, or the sandbox itself.
pub fn root_of<'a>(sandbox: &'a str, started_by: &'a [String]) -> &'a str {
    started_by.first().map(String::as_str).unwrap_or(sandbox)
}

/// Who pays for one request.
#[derive(Debug, Clone)]
pub struct Payer<'a> {
    pub sandbox: &'a str,
    pub root: &'a str,
    pub project: &'a str,
    pub device: &'a str,
}

/// The answer of `reserve`.
#[derive(Debug, PartialEq, Eq)]
pub enum Reserve {
    /// The request can run. The id names its ledger row.
    Ok(i64),
    /// No budget for this run and device.
    NoBudget,
    /// The request would pass the budget. `notify` is true the first time.
    Over {
        spent: u64,
        budget: u64,
        notify: bool,
    },
}

/// The sum of one sandbox and device.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Sum {
    pub sandbox: String,
    pub root: String,
    pub project: String,
    pub device: String,
    pub requests: u64,
    /// Micro units.
    pub spent: u64,
}

/// The spent amount and the budget of one run and device.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Spent {
    pub device: String,
    pub spent: u64,
    pub budget: u64,
}

pub struct Ledger {
    conn: Connection,
}

impl Ledger {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            crate::privfs::dir(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        }
        if !path.exists() {
            crate::privfs::append(path)
                .with_context(|| format!("cannot create {}", path.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("cannot open the ledger {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        crate::kv::store::wal(&conn)?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    /// The budget of a run for a device.
    pub fn budget(&self, root: &str, device: &str) -> Result<Option<u64>> {
        Ok(self
            .conn
            .query_row(
                "SELECT budget FROM budgets WHERE root = ?1 AND device = ?2",
                params![root, device],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
            .map(|b| b as u64))
    }

    /// Set the budget of a run when it has none yet.
    pub fn init_budget(&self, root: &str, device: &str, budget: u64) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO budgets (root, device, budget) VALUES (?1, ?2, ?3)",
            params![root, device, budget as i64],
        )?;
        Ok(())
    }

    /// Raise the budget of a run. The next "budget used" sends a new
    /// notification. Returns the new budget.
    pub fn add_budget(&self, root: &str, device: &str, add: u64) -> Result<u64> {
        self.conn.execute(
            "INSERT INTO budgets (root, device, budget) VALUES (?1, ?2, ?3)
             ON CONFLICT (root, device) DO UPDATE SET budget = budget + ?3, notified = 0",
            params![root, device, add as i64],
        )?;
        Ok(self.budget(root, device)?.unwrap_or(add))
    }

    /// What a run spent on a device.
    pub fn spent(&self, root: &str, device: &str) -> Result<u64> {
        let n: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(cost), 0) FROM requests WHERE root = ?1 AND device = ?2",
            params![root, device],
            |r| r.get(0),
        )?;
        Ok(n as u64)
    }

    /// The root of a sandbox in the ledger, or the sandbox itself.
    pub fn root(&self, sandbox: &str) -> Result<String> {
        Ok(self
            .conn
            .query_row(
                "SELECT root FROM requests WHERE sandbox = ?1 LIMIT 1",
                params![sandbox],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or_else(|| sandbox.to_string()))
    }

    /// Check the budget and reserve `cost` for one request, in one write
    /// transaction. A request that reaches the budget exactly passes.
    pub fn reserve(&mut self, p: &Payer, cost: u64) -> Result<Reserve> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row: Option<(i64, i64)> = tx
            .query_row(
                "SELECT budget, notified FROM budgets WHERE root = ?1 AND device = ?2",
                params![p.root, p.device],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((budget, notified)) = row else {
            return Ok(Reserve::NoBudget);
        };
        let budget = budget as u64;
        let spent: i64 = tx.query_row(
            "SELECT COALESCE(SUM(cost), 0) FROM requests WHERE root = ?1 AND device = ?2",
            params![p.root, p.device],
            |r| r.get(0),
        )?;
        let spent = spent as u64;
        let insert = |tx: &rusqlite::Transaction, cost: u64, status: &str| {
            tx.execute(
                "INSERT INTO requests (time, sandbox, root, project, device, cost, status)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    crate::state::now() as i64,
                    p.sandbox,
                    p.root,
                    p.project,
                    p.device,
                    cost as i64,
                    status
                ],
            )
        };
        // Once the budget is used, even a request with no declared cost
        // waits: its service can report a cost.
        if spent.saturating_add(cost) > budget || (spent >= budget && cost == 0) {
            insert(&tx, 0, BUDGET_USED)?;
            if notified == 0 {
                tx.execute(
                    "UPDATE budgets SET notified = 1 WHERE root = ?1 AND device = ?2",
                    params![p.root, p.device],
                )?;
            }
            tx.commit()?;
            return Ok(Reserve::Over {
                spent,
                budget,
                notify: notified == 0,
            });
        }
        insert(&tx, cost, "running")?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(Reserve::Ok(id))
    }

    /// The final cost and status of a reserved request.
    pub fn settle(&self, id: i64, cost: u64, status: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE requests SET cost = ?2, status = ?3 WHERE id = ?1",
            params![id, cost as i64, status],
        )?;
        Ok(())
    }

    /// The sums by sandbox and device, since a time, for one project or
    /// all.
    pub fn sums(&self, project: Option<&str>, since: u64) -> Result<Vec<Sum>> {
        let mut st = self.conn.prepare(
            "SELECT sandbox, root, project, device, COUNT(*), COALESCE(SUM(cost), 0)
             FROM requests WHERE time >= ?1 AND (?2 IS NULL OR project = ?2)
             GROUP BY sandbox, root, project, device ORDER BY MIN(time), sandbox, device",
        )?;
        let rows = st.query_map(params![since as i64, project], |r| {
            Ok(Sum {
                sandbox: r.get(0)?,
                root: r.get(1)?,
                project: r.get(2)?,
                device: r.get(3)?,
                requests: r.get::<_, i64>(4)? as u64,
                spent: r.get::<_, i64>(5)? as u64,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The spent amount and budget of each device of a run, for the panel.
    pub fn run(&self, root: &str) -> Result<Vec<Spent>> {
        let mut st = self.conn.prepare(
            "SELECT b.device, b.budget, COALESCE(SUM(r.cost), 0) FROM budgets b
             LEFT JOIN requests r ON r.root = b.root AND r.device = b.device
             WHERE b.root = ?1 GROUP BY b.device ORDER BY b.device",
        )?;
        let rows = st.query_map(params![root], |r| {
            Ok(Spent {
                device: r.get(0)?,
                budget: r.get::<_, i64>(1)? as u64,
                spent: r.get::<_, i64>(2)? as u64,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

/// What the run of a sandbox spent, for `paddock ps --json`: only devices
/// with a spent amount above zero. Empty when the ledger does not exist.
pub fn spent_of(env: &crate::env::HostEnv, sandbox: &str) -> Vec<Spent> {
    let file = path(env);
    if !file.is_file() {
        return Vec::new();
    }
    Ledger::open(&file)
        .and_then(|l| l.run(&l.root(sandbox)?))
        .map(|v| v.into_iter().filter(|s| s.spent > 0).collect())
        .unwrap_or_default()
}

/// The text of `paddock devices ledger`: the sums by sandbox and device,
/// since `since` seconds ago, for one project or all.
pub fn report(
    env: &crate::env::HostEnv,
    project: Option<&Path>,
    since: Option<u64>,
    json: bool,
) -> Result<String> {
    let file = path(env);
    let from = since.map_or(0, |s| crate::state::now().saturating_sub(s));
    let project = project.map(|p| p.display().to_string());
    let sums = if file.is_file() {
        Ledger::open(&file)?.sums(project.as_deref(), from)?
    } else {
        Vec::new()
    };
    if json {
        let rows: Vec<serde_json::Value> = sums
            .iter()
            .map(|s| {
                let mut v = serde_json::to_value(s).unwrap_or_default();
                v["spent"] = money(s.spent).into();
                v
            })
            .collect();
        return Ok(format!(
            "{}\n",
            serde_json::json!({ "api_version": "paddock.ledger/v1", "sums": rows })
        ));
    }
    if sums.is_empty() {
        return Ok("No requests of paid devices.\n".into());
    }
    let mut out = String::new();
    for s in &sums {
        let run = if s.root == s.sandbox {
            String::new()
        } else {
            format!(" (run {})", s.root)
        };
        out.push_str(&format!(
            "{}{run}  {}  {} requests  spent {}  {}\n",
            s.sandbox,
            s.device,
            s.requests,
            money(s.spent),
            s.project
        ));
    }
    Ok(out)
}

/// Raise the budget of a run on a device by `add` (`paddock devices
/// budget`). `id` is the root sandbox or a child in the ledger. Returns the
/// root and the new budget.
pub fn raise(env: &crate::env::HostEnv, id: &str, device: &str, add: u64) -> Result<(String, u64)> {
    let l = Ledger::open(&path(env))?;
    let root = l.root(id)?;
    let budget = l.add_budget(&root, device, add)?;
    Ok((root, budget))
}

/// A duration like `7d`, `12h`, `30m` or `90s`, in seconds.
pub fn parse_since(s: &str) -> Result<u64> {
    let t = s.trim();
    let (num, mult) = match t.char_indices().last() {
        Some((i, 'd')) => (&t[..i], 86_400),
        Some((i, 'h')) => (&t[..i], 3_600),
        Some((i, 'm')) => (&t[..i], 60),
        Some((i, 's')) => (&t[..i], 1),
        _ => (t, 1),
    };
    let n: u64 = num
        .parse()
        .with_context(|| format!("{s:?} is not a duration like 7d, 12h or 30m"))?;
    n.checked_mul(mult)
        .with_context(|| format!("{s:?} is too long"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payer<'a>(sandbox: &'a str, root: &'a str) -> Payer<'a> {
        Payer {
            sandbox,
            root,
            project: "/p",
            device: "search",
        }
    }

    #[test]
    fn amounts_and_money() {
        assert_eq!(parse_amount("1.0").unwrap(), UNIT);
        assert_eq!(parse_amount("0.002").unwrap(), 2_000);
        assert!(parse_amount("-1").is_err());
        assert!(parse_amount("nan").is_err());
        assert!(parse_amount("1e9").is_err());
        assert_eq!(money(420_000), "0.42");
        assert_eq!(money(UNIT), "1.00");
        assert_eq!(money(2_000), "0.002");
        assert_eq!(parse_since("7d").unwrap(), 7 * 86_400);
        assert!(parse_since("x").is_err());
    }

    #[test]
    fn the_header_cost_is_the_larger_and_a_bad_header_is_ignored() {
        let r = b"HTTP/1.1 200 OK\nx-cost: 0.5\nContent-Length: 0\n\n";
        assert_eq!(header_cost(r, "X-Cost"), Some(500_000));
        assert_eq!(charge(2_000, header_cost(r, "X-Cost")), 500_000);
        let small = b"HTTP/1.1 200 OK\nX-Cost: 0.001\n\n";
        assert_eq!(charge(2_000, header_cost(small, "X-Cost")), 2_000);
        for bad in [
            &b"HTTP/1.1 200 OK\nX-Cost: lots\n\n"[..],
            b"HTTP/1.1 200 OK\nX-Cost: -3\n\n",
            b"HTTP/1.1 200 OK\nX-Cost: inf\n\n",
            b"HTTP/1.1 200 OK\n\nX-Cost: 9",
        ] {
            assert_eq!(header_cost(bad, "X-Cost"), None);
            assert_eq!(charge(2_000, header_cost(bad, "X-Cost")), 2_000);
        }
    }

    #[test]
    fn the_budget_passes_at_the_limit_and_stops_over_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut l = Ledger::open(&dir.path().join("devices/ledger.db")).unwrap();
        let p = payer("a", "a");
        assert_eq!(l.reserve(&p, 1).unwrap(), Reserve::NoBudget);
        l.init_budget("a", "search", 1_000).unwrap();
        // The config budget never replaces a budget that exists.
        l.init_budget("a", "search", 9_000).unwrap();
        let Reserve::Ok(id) = l.reserve(&p, 400).unwrap() else {
            panic!("pass")
        };
        l.settle(id, 600, "200").unwrap();
        assert!(matches!(l.reserve(&p, 400).unwrap(), Reserve::Ok(_)));
        assert_eq!(l.spent("a", "search").unwrap(), 1_000);
        assert_eq!(
            l.reserve(&p, 1).unwrap(),
            Reserve::Over {
                spent: 1_000,
                budget: 1_000,
                notify: true
            }
        );
        // One notification, until the budget grows.
        assert!(matches!(
            l.reserve(&p, 1).unwrap(),
            Reserve::Over { notify: false, .. }
        ));
        assert_eq!(l.add_budget("a", "search", 500).unwrap(), 1_500);
        assert!(matches!(l.reserve(&p, 500).unwrap(), Reserve::Ok(_)));
        assert!(matches!(
            l.reserve(&p, 0).unwrap(),
            Reserve::Over { notify: true, .. }
        ));
    }

    #[test]
    fn a_child_spends_the_budget_of_its_root() {
        let dir = tempfile::tempdir().unwrap();
        let mut l = Ledger::open(&dir.path().join("ledger.db")).unwrap();
        let chain = vec!["root1".to_string(), "mid".to_string()];
        assert_eq!(root_of("child", &chain), "root1");
        assert_eq!(root_of("root1", &[]), "root1");
        l.init_budget("root1", "search", 1_000).unwrap();
        assert!(matches!(
            l.reserve(&payer("root1", "root1"), 600).unwrap(),
            Reserve::Ok(_)
        ));
        // The child has no budget of its own: the root's is nearly used.
        assert!(matches!(
            l.reserve(&payer("child", root_of("child", &chain)), 600)
                .unwrap(),
            Reserve::Over { spent: 600, .. }
        ));
        assert!(matches!(
            l.reserve(&payer("child", "root1"), 400).unwrap(),
            Reserve::Ok(_)
        ));
        assert_eq!(l.budget("child", "search").unwrap(), None);
        assert_eq!(l.root("child").unwrap(), "root1");
        assert_eq!(l.root("unknown").unwrap(), "unknown");
    }

    #[test]
    fn the_ledger_sums_by_sandbox_and_device() {
        let dir = tempfile::tempdir().unwrap();
        let mut l = Ledger::open(&dir.path().join("ledger.db")).unwrap();
        l.init_budget("a", "search", UNIT).unwrap();
        l.init_budget("a", "llm", UNIT).unwrap();
        for (sb, dev, cost) in [
            ("a", "search", 100),
            ("a", "search", 200),
            ("b", "search", 300),
            ("a", "llm", 50),
        ] {
            let p = Payer {
                sandbox: sb,
                root: "a",
                project: if sb == "a" { "/p" } else { "/q" },
                device: dev,
            };
            assert!(matches!(l.reserve(&p, cost).unwrap(), Reserve::Ok(_)));
        }
        let all = l.sums(None, 0).unwrap();
        let get = |sb: &str, dev: &str| {
            all.iter()
                .find(|s| s.sandbox == sb && s.device == dev)
                .map(|s| (s.requests, s.spent))
        };
        assert_eq!(get("a", "search"), Some((2, 300)));
        assert_eq!(get("b", "search"), Some((1, 300)));
        assert_eq!(get("a", "llm"), Some((1, 50)));
        assert_eq!(l.sums(Some("/q"), 0).unwrap().len(), 1);
        assert!(l.sums(None, crate::state::now() + 100).unwrap().is_empty());
        let run = l.run("a").unwrap();
        assert_eq!(run.iter().map(|s| s.spent).sum::<u64>(), 650);
    }
}
