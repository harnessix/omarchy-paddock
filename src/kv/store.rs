//! One SQLite file of the key-value store on the host (shard.rs says which
//! file holds what). Every sandbox's paddock process can open it; SQLite's
//! locking keeps concurrent writers apart, and a write transaction makes
//! compare-and-swap atomic across processes. SQLite is compiled into the
//! paddock binary, so the same binary still runs inside containers without
//! a system library.
//!
//! A key lives in a namespace (shard::Ns), and each namespace has a file
//! of its own. Versions come from one counter per file, so a key that
//! is deleted and written again never gets an old version back, and a
//! watcher never misses that change. Each write and delete also goes to a
//! short history per key; prefix watches read their changes from it.

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::Serialize;
use std::path::Path;

/// Versions of one key that the history keeps.
pub const HISTORY_PER_KEY: u64 = 16;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Entry {
    pub key: String,
    pub value: String,
    pub version: u64,
    /// The sandbox that wrote this version, or `user` for the host CLI.
    pub writer: String,
    pub writer_harness: String,
    pub updated: u64,
    pub expires: Option<u64>,
}

/// An entry without its value, for `list`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Meta {
    pub key: String,
    pub version: u64,
    pub bytes: u64,
    pub writer: String,
    pub writer_harness: String,
    pub updated: u64,
    pub expires: Option<u64>,
}

/// One write or delete, from the history.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Change {
    pub key: String,
    pub version: u64,
    /// `put` or `delete`.
    pub op: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    pub writer: String,
    pub writer_harness: String,
    pub time: u64,
}

/// Who writes: always the sandbox of the connection.
pub struct Writer<'a> {
    pub sandbox: &'a str,
    pub harness: &'a str,
}

/// The limits of one namespace.
#[derive(Debug, Clone, Copy)]
pub struct Quota {
    pub bytes: u64,
    pub keys: u64,
}

#[derive(Debug)]
pub enum WriteError {
    /// The version did not match. `current` is 0 when the key does not exist.
    Conflict {
        key: String,
        current: u64,
    },
    /// The namespace would hold too much.
    Quota(String),
    /// A delete of a key that does not exist.
    Missing,
    Db(anyhow::Error),
}

impl From<rusqlite::Error> for WriteError {
    fn from(e: rusqlite::Error) -> Self {
        WriteError::Db(e.into())
    }
}

/// A finished sandbox with `self` data or a `tree` scope, for `paddock gc`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Stale {
    pub sandbox: String,
    pub entries: u64,
    pub bytes: u64,
    /// The last time the sandbox wrote, started or ended.
    pub last: u64,
}

/// A sandbox as the registry recorded it.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Record {
    pub id: String,
    pub harness: String,
    pub parent: Option<String>,
    /// The pasture it joined; None: a pasture of its own.
    pub pasture: Option<String>,
    pub started: u64,
    pub ended: Option<u64>,
}

/// One write of a batch.
pub struct Put<'a> {
    pub key: &'a str,
    pub value: &'a str,
    /// `None` writes in any case, `Some(0)` only when the key does not
    /// exist, `Some(n)` only when its version is `n`.
    pub if_version: Option<u64>,
    pub expires: Option<u64>,
}

pub struct Store {
    conn: Connection,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (name TEXT PRIMARY KEY, value INTEGER NOT NULL);
INSERT OR IGNORE INTO meta (name, value) VALUES ('seq', 0);
CREATE TABLE IF NOT EXISTS entries (
  ns TEXT NOT NULL,
  key TEXT NOT NULL,
  value TEXT NOT NULL,
  size INTEGER NOT NULL,
  version INTEGER NOT NULL,
  writer TEXT NOT NULL,
  writer_harness TEXT NOT NULL,
  updated INTEGER NOT NULL,
  expires INTEGER,
  PRIMARY KEY (ns, key)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS history (
  version INTEGER PRIMARY KEY,
  ns TEXT NOT NULL,
  key TEXT NOT NULL,
  op TEXT NOT NULL,
  value TEXT,
  size INTEGER NOT NULL,
  writer TEXT NOT NULL,
  writer_harness TEXT NOT NULL,
  time INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS history_key ON history (ns, key, version);
CREATE TABLE IF NOT EXISTS sandboxes (
  id TEXT PRIMARY KEY,
  harness TEXT NOT NULL,
  project TEXT NOT NULL,
  parent TEXT,
  pasture TEXT,
  started INTEGER NOT NULL,
  ended INTEGER
);
CREATE TABLE IF NOT EXISTS grants (
  sandbox TEXT NOT NULL,
  name TEXT NOT NULL,
  PRIMARY KEY (sandbox, name)
) WITHOUT ROWID;
";

/// How many sandboxes above one another `ancestors` follows at most.
const MAX_CHAIN: usize = 32;

/// The column `paused` of `entries`: the seconds that a lease of a frozen
/// holder has left (`[suspend] leases = "keep"`). Its `expires` is NULL
/// meanwhile, so it does not expire. Files of older versions get the column
/// when they open.
fn add_paused(conn: &Connection) -> Result<()> {
    let has = |c: &Connection| -> rusqlite::Result<bool> {
        c.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('entries') WHERE name = 'paused'",
            [],
            |r| r.get::<_, u64>(0),
        )
        .map(|n| n > 0)
    };
    if has(conn)? {
        return Ok(());
    }
    // Another process can add it at the same moment.
    if let Err(e) = conn.execute_batch("ALTER TABLE entries ADD COLUMN paused INTEGER") {
        if !has(conn)? {
            return Err(e.into());
        }
    }
    Ok(())
}

/// The keys of leases: `claim` writes them.
const LEASE_KEYS: &str = "substr(key, 1, 6) = 'lease:'";

fn live(expires: Option<u64>, now: u64) -> bool {
    expires.is_none_or(|e| e > now)
}

fn next_version(tx: &Transaction) -> rusqlite::Result<u64> {
    tx.query_row(
        "UPDATE meta SET value = value + 1 WHERE name = 'seq' RETURNING value",
        [],
        |r| r.get(0),
    )
}

/// Remove the keys that expired at or before `now`, in `ns` or in every
/// namespace. Each one becomes a change with the op `expire` in the history,
/// written by the last writer of the key, so a watch that passes its cursor
/// sees every expiry, also one that happened between two watches. Returns
/// how many keys expired.
fn expire_rows(
    tx: &Transaction,
    ns: Option<&str>,
    now: u64,
    max_bytes: u64,
) -> rusqlite::Result<u64> {
    let rows: Vec<(String, String, String, String)> = {
        let mut st = tx.prepare(
            "SELECT ns, key, writer, writer_harness FROM entries
             WHERE expires IS NOT NULL AND expires <= ?1 AND (?2 IS NULL OR ns = ?2)
             ORDER BY expires, ns, key",
        )?;
        let it = st.query_map(params![now, ns], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?;
        it.collect::<rusqlite::Result<_>>()?
    };
    for (n, key, sandbox, harness) in &rows {
        tx.execute(
            "DELETE FROM entries WHERE ns = ?1 AND key = ?2",
            params![n, key],
        )?;
        let version = next_version(tx)?;
        let writer = Writer { sandbox, harness };
        record(tx, n, key, version, "expire", None, &writer, max_bytes, now)?;
    }
    Ok(rows.len() as u64)
}

/// Add one change to the history and keep it bounded: at most
/// `HISTORY_PER_KEY` versions of a key, and at most `max_bytes` for the
/// namespace.
#[allow(clippy::too_many_arguments)]
fn record(
    tx: &Transaction,
    ns: &str,
    key: &str,
    version: u64,
    op: &str,
    value: Option<&str>,
    writer: &Writer,
    max_bytes: u64,
    now: u64,
) -> rusqlite::Result<()> {
    let size = (key.len() + value.map_or(0, str::len)) as u64;
    tx.execute(
        "INSERT INTO history (version, ns, key, op, value, size, writer, writer_harness, time)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            version,
            ns,
            key,
            op,
            value,
            size,
            writer.sandbox,
            writer.harness,
            now
        ],
    )?;
    tx.execute(
        "DELETE FROM history WHERE ns = ?1 AND key = ?2 AND version NOT IN
         (SELECT version FROM history WHERE ns = ?1 AND key = ?2 ORDER BY version DESC LIMIT ?3)",
        params![ns, key, HISTORY_PER_KEY],
    )?;
    loop {
        let total: u64 = tx.query_row(
            "SELECT COALESCE(SUM(size), 0) FROM history WHERE ns = ?1",
            params![ns],
            |r| r.get(0),
        )?;
        if total <= max_bytes {
            return Ok(());
        }
        let n = tx.execute(
            "DELETE FROM history WHERE version =
             (SELECT MIN(version) FROM history WHERE ns = ?1)",
            params![ns],
        )?;
        if n == 0 {
            return Ok(());
        }
    }
}

impl Store {
    /// Open or create the store. The file is 0600 in a 0700 folder; SQLite
    /// gives its journal files the mode of the database file.
    pub fn open(path: &Path) -> Result<Self> {
        if !path.exists() {
            crate::privfs::append(path)
                .with_context(|| format!("cannot create {}", path.display()))?;
        } else {
            crate::privfs::tighten(path, crate::privfs::FILE_MODE)?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("cannot open the store {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(SCHEMA)?;
        add_paused(&conn)?;
        Ok(Self { conn })
    }

    /// A store in memory with no keys: reads of a namespace that has no
    /// file yet use it, so that a read never creates a file.
    pub fn empty() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        add_paused(&conn)?;
        Ok(Self { conn })
    }

    /// The newest version in the whole store.
    pub fn seq(&self) -> Result<u64> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE name = 'seq'", [], |r| {
                r.get(0)
            })?)
    }

    /// The current version of a key, 0 when it does not exist or expired.
    pub fn version(&self, ns: &str, key: &str, now: u64) -> Result<u64> {
        let row: Option<(u64, Option<u64>)> = self
            .conn
            .query_row(
                "SELECT version, expires FROM entries WHERE ns = ?1 AND key = ?2",
                params![ns, key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(match row {
            Some((v, exp)) if live(exp, now) => v,
            _ => 0,
        })
    }

    pub fn get(&self, ns: &str, key: &str, now: u64) -> Result<Option<Entry>> {
        let e = self
            .conn
            .query_row(
                "SELECT key, value, version, writer, writer_harness, updated, expires
                 FROM entries WHERE ns = ?1 AND key = ?2",
                params![ns, key],
                |r| {
                    Ok(Entry {
                        key: r.get(0)?,
                        value: r.get(1)?,
                        version: r.get(2)?,
                        writer: r.get(3)?,
                        writer_harness: r.get(4)?,
                        updated: r.get(5)?,
                        expires: r.get(6)?,
                    })
                },
            )
            .optional()?;
        Ok(e.filter(|e| live(e.expires, now)))
    }

    /// Keys that start with `prefix`, in order, at most `limit`. The second
    /// value is true when more keys match.
    pub fn list(
        &self,
        ns: &str,
        prefix: &str,
        limit: usize,
        now: u64,
    ) -> Result<(Vec<Meta>, bool)> {
        let mut st = self.conn.prepare(
            "SELECT key, version, size, writer, writer_harness, updated, expires
             FROM entries WHERE ns = ?1 AND key >= ?2 ORDER BY key",
        )?;
        let rows = st.query_map(params![ns, prefix], |r| {
            Ok(Meta {
                key: r.get(0)?,
                version: r.get(1)?,
                bytes: r.get(2)?,
                writer: r.get(3)?,
                writer_harness: r.get(4)?,
                updated: r.get(5)?,
                expires: r.get(6)?,
            })
        })?;
        let mut out = Vec::new();
        for m in rows {
            let m = m?;
            if !m.key.starts_with(prefix) {
                break;
            }
            if !live(m.expires, now) {
                continue;
            }
            if out.len() == limit {
                return Ok((out, true));
            }
            out.push(m);
        }
        Ok((out, false))
    }

    /// Writes and deletes of keys with `prefix` after version `since`,
    /// oldest first, at most `limit`. Values are left out.
    pub fn changes(&self, ns: &str, prefix: &str, since: u64, limit: usize) -> Result<Vec<Change>> {
        let mut st = self.conn.prepare(
            "SELECT key, version, op, writer, writer_harness, time FROM history
             WHERE ns = ?1 AND version > ?2 ORDER BY version",
        )?;
        let rows = st.query_map(params![ns, since], |r| {
            Ok(Change {
                key: r.get(0)?,
                version: r.get(1)?,
                op: r.get(2)?,
                value: None,
                writer: r.get(3)?,
                writer_harness: r.get(4)?,
                time: r.get(5)?,
            })
        })?;
        let mut out = Vec::new();
        for c in rows {
            let c = c?;
            if c.key.starts_with(prefix) {
                out.push(c);
                if out.len() == limit {
                    break;
                }
            }
        }
        Ok(out)
    }

    /// The last versions of one key, newest first, with their values.
    pub fn history(&self, ns: &str, key: &str, limit: usize) -> Result<Vec<Change>> {
        let mut st = self.conn.prepare(
            "SELECT key, version, op, value, writer, writer_harness, time FROM history
             WHERE ns = ?1 AND key = ?2 ORDER BY version DESC LIMIT ?3",
        )?;
        let rows = st.query_map(params![ns, key, limit as u64], |r| {
            Ok(Change {
                key: r.get(0)?,
                version: r.get(1)?,
                op: r.get(2)?,
                value: r.get(3)?,
                writer: r.get(4)?,
                writer_harness: r.get(5)?,
                time: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Bytes and keys that a namespace holds now.
    pub fn usage(&self, ns: &str, now: u64) -> Result<(u64, u64)> {
        Ok(self.conn.query_row(
            "SELECT COALESCE(SUM(size), 0), COUNT(*) FROM entries
             WHERE ns = ?1 AND (expires IS NULL OR expires > ?2)",
            params![ns, now],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?)
    }

    /// Write one value. Returns the new version.
    #[allow(clippy::too_many_arguments)]
    pub fn put(
        &mut self,
        ns: &str,
        key: &str,
        value: &str,
        if_version: Option<u64>,
        expires: Option<u64>,
        writer: &Writer,
        quota: Quota,
        now: u64,
    ) -> Result<u64, WriteError> {
        let v = self.put_many(
            ns,
            &[Put {
                key,
                value,
                if_version,
                expires,
            }],
            writer,
            quota,
            now,
        )?;
        Ok(v[0])
    }

    /// Write several values in one transaction: all of them or none.
    /// Returns the new versions, in order.
    pub fn put_many(
        &mut self,
        ns: &str,
        puts: &[Put],
        writer: &Writer,
        quota: Quota,
        now: u64,
    ) -> Result<Vec<u64>, WriteError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        expire_rows(&tx, Some(ns), now, quota.bytes)?;
        let mut out = Vec::new();
        for p in puts {
            let cur: Option<(u64, u64)> = tx
                .query_row(
                    "SELECT version, size FROM entries WHERE ns = ?1 AND key = ?2",
                    params![ns, p.key],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let current = cur.map_or(0, |c| c.0);
            if let Some(want) = p.if_version {
                if want != current {
                    return Err(WriteError::Conflict {
                        key: p.key.to_string(),
                        current,
                    });
                }
            }
            let size = (p.key.len() + p.value.len()) as u64;
            let (bytes, keys): (u64, u64) = tx.query_row(
                "SELECT COALESCE(SUM(size), 0), COUNT(*) FROM entries WHERE ns = ?1",
                params![ns],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            let old = cur.map_or(0, |c| c.1);
            if bytes - old + size > quota.bytes {
                return Err(WriteError::Quota(format!(
                    "the scope would hold {} bytes; its limit is {}",
                    bytes - old + size,
                    quota.bytes
                )));
            }
            if cur.is_none() && keys + 1 > quota.keys {
                return Err(WriteError::Quota(format!(
                    "the scope already holds {keys} keys; its limit is {}",
                    quota.keys
                )));
            }
            let version = next_version(&tx)?;
            tx.execute(
                "INSERT OR REPLACE INTO entries
                 (ns, key, value, size, version, writer, writer_harness, updated, expires)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    ns,
                    p.key,
                    p.value,
                    size,
                    version,
                    writer.sandbox,
                    writer.harness,
                    now,
                    p.expires
                ],
            )?;
            record(
                &tx,
                ns,
                p.key,
                version,
                "put",
                Some(p.value),
                writer,
                quota.bytes,
                now,
            )?;
            out.push(version);
        }
        tx.commit()?;
        Ok(out)
    }

    pub fn delete(
        &mut self,
        ns: &str,
        key: &str,
        if_version: Option<u64>,
        writer: &Writer,
        quota: Quota,
        now: u64,
    ) -> Result<(), WriteError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let cur: Option<(u64, Option<u64>)> = tx
            .query_row(
                "SELECT version, expires FROM entries WHERE ns = ?1 AND key = ?2",
                params![ns, key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let current = match cur {
            Some((v, exp)) if live(exp, now) => v,
            _ => 0,
        };
        if let Some(want) = if_version {
            if want != current {
                return Err(WriteError::Conflict {
                    key: key.to_string(),
                    current,
                });
            }
        }
        tx.execute(
            "DELETE FROM entries WHERE ns = ?1 AND key = ?2",
            params![ns, key],
        )?;
        if current == 0 {
            tx.commit()?;
            return Err(WriteError::Missing);
        }
        let version = next_version(&tx)?;
        record(
            &tx,
            ns,
            key,
            version,
            "delete",
            None,
            writer,
            quota.bytes,
            now,
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Record a sandbox at its start. The parent is the sandbox that started
    /// it, when there is one.
    pub fn register(
        &self,
        id: &str,
        harness: &str,
        project: &Path,
        parent: Option<&str>,
        pasture: Option<&str>,
        now: u64,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO sandboxes (id, harness, project, parent, pasture, started, ended)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL)",
            params![id, harness, project.display().to_string(), parent, pasture, now],
        )?;
        Ok(())
    }

    /// Record the end of a sandbox. Its session grants end with it.
    pub fn finish(&self, id: &str, now: u64) -> Result<()> {
        self.conn.execute(
            "UPDATE sandboxes SET ended = ?2 WHERE id = ?1",
            params![id, now],
        )?;
        self.conn
            .execute("DELETE FROM grants WHERE sandbox = ?1", params![id])?;
        Ok(())
    }

    /// Whether a sandbox ended. None when paddock has no record of it.
    pub fn ended(&self, id: &str) -> Result<Option<bool>> {
        Ok(self
            .conn
            .query_row(
                "SELECT ended IS NOT NULL FROM sandboxes WHERE id = ?1",
                params![id],
                |r| r.get::<_, bool>(0),
            )
            .optional()?)
    }

    /// Record that the user allowed `name` for the session of a sandbox.
    /// Only the host process of that sandbox writes it; children read it.
    pub fn grant(&self, sandbox: &str, name: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO grants (sandbox, name) VALUES (?1, ?2)",
            params![sandbox, name],
        )?;
        Ok(())
    }

    pub fn granted(&self, sandbox: &str, name: &str) -> Result<bool> {
        Ok(self
            .conn
            .query_row(
                "SELECT 1 FROM grants g JOIN sandboxes s ON s.id = g.sandbox
                 WHERE g.sandbox = ?1 AND g.name = ?2 AND s.ended IS NULL",
                params![sandbox, name],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// The sandboxes above `id`, as paddock recorded them: the parent first.
    pub fn ancestors(&self, id: &str) -> Result<Vec<String>> {
        let mut out: Vec<String> = Vec::new();
        let mut cur = id.to_string();
        while out.len() < MAX_CHAIN {
            match self.parent_of(&cur)? {
                Some(p) if p != id && !out.contains(&p) => {
                    out.push(p.clone());
                    cur = p;
                }
                _ => break,
            }
        }
        Ok(out)
    }

    /// The sandbox that started `id`, as paddock recorded it.
    pub fn parent_of(&self, id: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT parent FROM sandboxes WHERE id = ?1",
                params![id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten())
    }

    /// Entries whose time to live has passed: count and bytes.
    pub fn expired(&self, now: u64) -> Result<(u64, u64)> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(size), 0) FROM entries
             WHERE expires IS NOT NULL AND expires <= ?1",
            params![now],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?)
    }

    pub fn purge_expired(&mut self, now: u64) -> Result<u64> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let n = expire_rows(&tx, None, now, u64::MAX)?;
        tx.commit()?;
        Ok(n)
    }

    /// Expire the keys of `ns` whose time to live has passed. A watch runs
    /// this on each poll, so an expiry shows without a write.
    pub fn expire(&mut self, ns: &str, now: u64) -> Result<u64> {
        // Most polls find nothing: a read first, so that a watch does not
        // take the write lock from the writers every time.
        let due: bool = self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM entries
             WHERE ns = ?1 AND expires IS NOT NULL AND expires <= ?2)",
            params![ns, now],
            |r| r.get(0),
        )?;
        if !due {
            return Ok(0);
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let n = expire_rows(&tx, Some(ns), now, u64::MAX)?;
        tx.commit()?;
        Ok(n)
    }

    /// Pause the leases that `sandbox` holds, in every namespace of this
    /// file: each keeps the seconds it has left and does not expire until
    /// `resume_leases`. Returns how many it paused.
    pub fn pause_leases(&mut self, sandbox: &str, now: u64) -> Result<u64> {
        let n = self.conn.execute(
            &format!(
                "UPDATE entries SET paused = MAX(expires - ?2, 1), expires = NULL
                 WHERE writer = ?1 AND {LEASE_KEYS} AND paused IS NULL
                 AND expires IS NOT NULL AND expires > ?2"
            ),
            params![sandbox, now],
        )?;
        Ok(n as u64)
    }

    /// The paused leases of `sandbox` run again from `now`, with the time
    /// that they had left. Returns how many.
    pub fn resume_leases(&mut self, sandbox: &str, now: u64) -> Result<u64> {
        let n = self.conn.execute(
            "UPDATE entries SET expires = ?2 + paused, paused = NULL
             WHERE writer = ?1 AND paused IS NOT NULL",
            params![sandbox, now],
        )?;
        Ok(n as u64)
    }

    /// Whether this lease waits for its frozen holder.
    pub fn is_paused(&self, ns: &str, key: &str) -> Result<bool> {
        Ok(self
            .conn
            .query_row(
                "SELECT paused IS NOT NULL FROM entries WHERE ns = ?1 AND key = ?2",
                params![ns, key],
                |r| r.get::<_, bool>(0),
            )
            .optional()?
            .unwrap_or(false))
    }

    /// Delete the live leases that `sandbox` holds, in every namespace of
    /// this file. Each delete goes to the history with `writer`. Returns
    /// the namespaces and keys.
    pub fn release_leases(
        &mut self,
        sandbox: &str,
        writer: &Writer,
        quota: Quota,
        now: u64,
    ) -> Result<Vec<(String, String)>> {
        let rows: Vec<(String, String, u64)> = {
            let mut st = self.conn.prepare(&format!(
                "SELECT ns, key, version FROM entries
                 WHERE writer = ?1 AND {LEASE_KEYS}
                 AND (paused IS NOT NULL OR expires IS NULL OR expires > ?2)
                 ORDER BY ns, key"
            ))?;
            let it = st.query_map(params![sandbox, now], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?;
            it.collect::<rusqlite::Result<_>>()?
        };
        let mut out = Vec::new();
        for (ns, key, version) in rows {
            match self.delete(&ns, &key, Some(version), writer, quota, now) {
                Ok(()) => out.push((ns, key)),
                // Changed or expired at the same moment: not ours to free.
                Err(WriteError::Conflict { .. }) | Err(WriteError::Missing) => {}
                Err(e) => anyhow::bail!("cannot release {key}: {e:?}"),
            }
        }
        Ok(out)
    }

    /// Registry: forget the records of sandboxes that ended before
    /// `cutoff` and have no files left. The sandboxes above a running one
    /// stay, because the running one reads them.
    pub fn forget_ended(&mut self, cutoff: u64, running: &[String]) -> Result<u64> {
        let mut keep: Vec<String> = running.to_vec();
        for id in running {
            keep.extend(self.ancestors(id)?);
        }
        let old: Vec<String> = {
            let mut st = self
                .conn
                .prepare("SELECT id FROM sandboxes WHERE ended IS NOT NULL AND ended < ?1")?;
            let rows = st.query_map(params![cutoff], |r| r.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let mut n = 0;
        for id in old.iter().filter(|i| !keep.contains(i)) {
            self.forget_sandbox(id)?;
            n += 1;
        }
        Ok(n)
    }

    /// Registry: the sandboxes recorded for a project, oldest first.
    pub fn sandboxes_of(&self, project: &Path) -> Result<Vec<Record>> {
        let mut st = self.conn.prepare(
            "SELECT id, harness, parent, pasture, started, ended FROM sandboxes
             WHERE project = ?1 ORDER BY started, id",
        )?;
        let rows = st.query_map(params![project.display().to_string()], |r| {
            Ok(Record {
                id: r.get(0)?,
                harness: r.get(1)?,
                parent: r.get(2)?,
                pasture: r.get(3)?,
                started: r.get(4)?,
                ended: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Registry: when a sandbox started and ended. None without a record.
    pub fn times(&self, id: &str) -> Result<Option<(u64, Option<u64>)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT started, ended FROM sandboxes WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    /// The namespaces that hold keys in this file.
    pub fn namespaces(&self) -> Result<Vec<String>> {
        let mut st = self
            .conn
            .prepare("SELECT DISTINCT ns FROM entries ORDER BY ns")?;
        let rows = st.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Registry: every project that a sandbox was recorded for.
    pub fn projects(&self) -> Result<Vec<String>> {
        let mut st = self
            .conn
            .prepare("SELECT DISTINCT project FROM sandboxes ORDER BY project")?;
        let rows = st.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// All keys of this file that have not expired: count and bytes.
    pub fn totals(&self) -> Result<(u64, u64)> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(size), 0) FROM entries",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?)
    }

    /// Remove a whole namespace with its history. `paddock doctor` cleans up
    /// with it.
    pub fn remove_ns(&mut self, ns: &str) -> Result<u64> {
        self.conn
            .execute("DELETE FROM history WHERE ns = ?1", params![ns])?;
        Ok(self
            .conn
            .execute("DELETE FROM entries WHERE ns = ?1", params![ns])? as u64)
    }

    pub fn forget_sandbox(&mut self, id: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM sandboxes WHERE id = ?1", params![id])?;
        self.conn
            .execute("DELETE FROM grants WHERE sandbox = ?1", params![id])?;
        Ok(())
    }
}
