//! The task records: one kv entry per task, with paddock as the only
//! writer. A record is the superset of a `tasks.md` line: its state, who
//! holds it, the branch, the result and the review.
//!
//! Keys, in the `tree` scope of the spawn tree:
//!
//! - `spec/<feature>/task/<id>`: the record (JSON, `Record`).
//! - `lease:spec/<feature>/task/<id>`: the claim. Its owner is the sandbox
//!   that works on the task, and it expires unless paddock renews it, so a
//!   dead worker frees the task.
//!
//! A record that another writer put there is ignored: a sandbox can write
//! the tree scope, but only paddock writes these records.

use super::tasks::{self, Doc, Mark};
use crate::kv::shard::Ns;
use crate::kv::store::{Quota, Store, WriteError, Writer};
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The writer of every record.
pub const WRITER: &str = "paddock";
/// Seconds that a claim lives without a renewal.
pub const LEASE_TTL: u64 = 120;

pub fn prefix(feature: &str) -> String {
    format!("spec/{feature}/task/")
}

pub fn key(feature: &str, id: &str) -> String {
    format!("{}{id}", prefix(feature))
}

pub fn lease_key(feature: &str, id: &str) -> String {
    format!("{}{}", crate::kv::LEASE_PREFIX, key(feature, id))
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    #[default]
    Todo,
    /// Taken with a lease; the worker does not run yet.
    Claimed,
    Running,
    Done,
    Failed,
    /// The worker ended well; the work waits for a review.
    Review,
}

impl State {
    pub fn name(self) -> &'static str {
        match self {
            State::Todo => "todo",
            State::Claimed => "claimed",
            State::Running => "running",
            State::Done => "done",
            State::Failed => "failed",
            State::Review => "review",
        }
    }

    /// A state that a live lease holds.
    pub fn held(self) -> bool {
        matches!(self, State::Claimed | State::Running)
    }
}

/// What happens to a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Claim,
    Start,
    /// The worker ended. true: it reported success.
    Finish(bool),
    /// The start failed, or the lease is gone.
    Free,
    /// The review: true passes, false sends it back.
    Verdict(bool),
    /// The user checked the box in `tasks.md`.
    Checked,
    /// The user cleared the box in `tasks.md`.
    Unchecked,
}

/// The state machine of a task.
pub fn next(from: State, e: Event) -> Result<State> {
    use State::*;
    Ok(match (from, e) {
        (Todo | Failed, Event::Claim) => Claimed,
        (Claimed, Event::Start) => Running,
        (Claimed | Running, Event::Finish(true)) => Review,
        (Claimed | Running, Event::Finish(false)) => Failed,
        (Claimed | Running, Event::Free) => Todo,
        (Review, Event::Verdict(true)) => Done,
        (Review | Failed, Event::Verdict(false)) => Todo,
        (_, Event::Checked) => Done,
        (Done, Event::Unchecked) => Todo,
        (s, Event::Unchecked) => s,
        (s, e) => bail!("task is {}: {e:?} does not apply", s.name()),
    })
}

/// What the worker said with `paddock-host done`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    pub ok: bool,
    pub text: String,
    pub at: u64,
}

/// One task, as paddock keeps it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub id: String,
    pub feature: String,
    pub phase: usize,
    #[serde(default)]
    pub phase_title: String,
    pub parallel: bool,
    #[serde(default)]
    pub story: Option<String>,
    #[serde(default)]
    pub deps: Vec<String>,
    pub text: String,
    pub state: State,
    /// The sandbox that holds or held the task.
    #[serde(default)]
    pub sandbox: Option<String>,
    /// The id that `paddock-host spawn` printed.
    #[serde(default)]
    pub spawn_id: Option<String>,
    /// The lease key while a worker holds the task.
    #[serde(default)]
    pub lease: Option<String>,
    #[serde(default)]
    pub harness: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
    /// The time limit of the worker, in seconds.
    #[serde(default)]
    pub timer: Option<u64>,
    /// What the task must deliver, from the phase and the spec.
    #[serde(default)]
    pub acceptance: String,
    #[serde(default)]
    pub report: Option<Report>,
    /// One line about the end of the last worker.
    #[serde(default)]
    pub result: Option<String>,
    /// `pass` or `changes`, and the reviewer's note.
    #[serde(default)]
    pub verdict: Option<String>,
    #[serde(default)]
    pub review_note: Option<String>,
    #[serde(default)]
    pub attempts: u32,
    /// The checkbox that paddock last wrote. A different box in the file
    /// is a user's edit, and it wins.
    #[serde(default)]
    pub rendered: Option<bool>,
    /// The task is no longer in `tasks.md`. Its record stays.
    #[serde(default)]
    pub removed: bool,
    #[serde(default)]
    pub updated: u64,
}

impl Record {
    pub fn from_task(feature: &str, doc: &Doc, t: &tasks::Task) -> Self {
        let mut r = Record {
            id: t.id.clone(),
            feature: feature.to_string(),
            state: if t.checked { State::Done } else { State::Todo },
            ..Default::default()
        };
        r.refresh(doc, t);
        r
    }

    /// Take the text, the phase and the labels from the file.
    fn refresh(&mut self, doc: &Doc, t: &tasks::Task) {
        self.phase = t.phase;
        self.phase_title = doc.phase_title(t);
        self.parallel = t.parallel;
        self.story = t.story.clone();
        self.deps = t.deps.clone();
        self.text = t.text.clone();
    }

    /// The note on the task line.
    pub fn note(&self) -> Option<String> {
        let branch = self.branch.as_deref().unwrap_or("");
        match self.state {
            State::Todo | State::Done if branch.is_empty() => None,
            State::Todo if self.verdict.as_deref() == Some("changes") => {
                Some(format!("changes asked, {branch}"))
            }
            State::Todo => None,
            State::Done => Some(branch.to_string()),
            s => Some(if branch.is_empty() {
                s.name().to_string()
            } else {
                format!("{}, {branch}", s.name())
            }),
        }
    }

    pub fn mark(&self) -> Mark {
        Mark {
            checked: self.state == State::Done,
            note: self.note(),
        }
    }
}

/// Merge the file into the records. New tasks get a record, tasks that left
/// the file are marked removed, a changed checkbox is the user's edit and
/// wins, and a held task whose lease is gone is free again. Returns the
/// records that changed.
pub fn merge(
    feature: &str,
    doc: &Doc,
    records: &mut BTreeMap<String, Record>,
    lease_live: &dyn Fn(&Record) -> bool,
    now: u64,
) -> Vec<String> {
    let mut changed = Vec::new();
    for t in &doc.tasks {
        let before = records.get(&t.id).cloned();
        let r = records
            .entry(t.id.clone())
            .or_insert_with(|| Record::from_task(feature, doc, t));
        r.refresh(doc, t);
        r.removed = false;
        if let Some(c) = r.rendered {
            if c != t.checked {
                let e = if t.checked {
                    Event::Checked
                } else {
                    Event::Unchecked
                };
                r.state = next(r.state, e).unwrap_or(r.state);
            }
        }
        if before.as_ref() != Some(r) {
            changed.push(t.id.clone());
        }
    }
    for (id, r) in records.iter_mut() {
        if !r.removed && doc.task(id).is_none() {
            r.removed = true;
            changed.push(id.clone());
        }
    }
    for (id, r) in records.iter_mut() {
        if r.state.held() && !lease_live(r) {
            r.state = State::Todo;
            r.lease = None;
            r.result = Some("the worker's lease expired".into());
            changed.push(id.clone());
        }
    }
    for id in &changed {
        if let Some(r) = records.get_mut(id) {
            r.updated = now;
        }
    }
    changed.sort();
    changed.dedup();
    changed
}

/// The tasks that can start now, in file order: every task of an earlier
/// phase is done, every task before it in its phase is done (a run of
/// `[P]` tasks right before a `[P]` task may still run), and every task it
/// depends on is done.
pub fn ready(doc: &Doc, records: &BTreeMap<String, Record>) -> Vec<String> {
    let done = |id: &str| records.get(id).is_some_and(|r| r.state == State::Done);
    let mut out = Vec::new();
    for (i, t) in doc.tasks.iter().enumerate() {
        let Some(r) = records.get(&t.id) else {
            continue;
        };
        if r.state != State::Todo {
            continue;
        }
        let before = &doc.tasks[..i];
        if before.iter().any(|b| b.phase < t.phase && !done(&b.id)) {
            continue;
        }
        let same: Vec<&tasks::Task> = before.iter().filter(|b| b.phase == t.phase).collect();
        // The run of [P] tasks right before this one, when this one is [P].
        let run = if t.parallel {
            same.iter().rev().take_while(|b| b.parallel).count()
        } else {
            0
        };
        if same[..same.len() - run].iter().any(|b| !done(&b.id)) {
            continue;
        }
        if t.deps.iter().any(|d| d != &t.id && !done(d)) {
            continue;
        }
        out.push(t.id.clone());
    }
    out
}

/// The tasks that `id` waits for: not done, in the order of the file.
pub fn waits_for(doc: &Doc, records: &BTreeMap<String, Record>, id: &str) -> Vec<String> {
    let Some((i, t)) = doc.tasks.iter().enumerate().find(|(_, t)| t.id == id) else {
        return Vec::new();
    };
    let done = |id: &str| records.get(id).is_some_and(|r| r.state == State::Done);
    let mut out: Vec<String> = doc.tasks[..i]
        .iter()
        .filter(|b| b.phase < t.phase || (b.phase == t.phase && !(t.parallel && b.parallel)))
        .filter(|b| !done(&b.id))
        .map(|b| b.id.clone())
        .collect();
    out.extend(t.deps.iter().filter(|d| !done(d)).cloned());
    out.dedup();
    out
}

/// The kv side: the namespace and the file of one spawn tree.
#[derive(Debug, Clone)]
pub struct Board {
    pub file: PathBuf,
    pub ns: String,
    pub quota: Quota,
}

/// A record with its kv version (0: not stored yet).
pub type Stored = BTreeMap<String, (Record, u64)>;

impl Board {
    /// The tree scope of `root`.
    pub fn tree(kv_root: &Path, root: &str, quota: Quota) -> Result<Self> {
        let ns = Ns::Tree(root.to_string());
        Ok(Self {
            file: ns.file(kv_root)?,
            ns: ns.key(),
            quota,
        })
    }

    pub fn open(&self) -> Result<Store> {
        if let Some(d) = self.file.parent() {
            crate::privfs::dir(d)?;
        }
        Store::open(&self.file)
    }

    fn writer(&self) -> Writer<'static> {
        Writer {
            sandbox: WRITER,
            harness: "spec",
        }
    }

    /// The records of a feature that paddock wrote.
    pub fn load(&self, st: &Store, feature: &str, now: u64) -> Result<Stored> {
        let mut out = BTreeMap::new();
        let (metas, _) = st.list(&self.ns, &prefix(feature), 10_000, now)?;
        for m in metas {
            if m.writer != WRITER {
                log::warn!("{}: written by {}, not paddock; ignored", m.key, m.writer);
                continue;
            }
            let Some(e) = st.get(&self.ns, &m.key, now)? else {
                continue;
            };
            match serde_json::from_str::<Record>(&e.value) {
                Ok(r) => {
                    out.insert(r.id.clone(), (r, e.version));
                }
                Err(err) => log::warn!("{}: not a task record: {err}", m.key),
            }
        }
        Ok(out)
    }

    /// Every record of every feature, for a lookup by sandbox.
    pub fn all(&self, st: &Store, now: u64) -> Result<Vec<(Record, u64)>> {
        let (metas, _) = st.list(&self.ns, "spec/", 10_000, now)?;
        let mut out = Vec::new();
        for m in metas.iter().filter(|m| m.writer == WRITER) {
            if let Some(e) = st.get(&self.ns, &m.key, now)? {
                if let Ok(r) = serde_json::from_str::<Record>(&e.value) {
                    out.push((r, e.version));
                }
            }
        }
        Ok(out)
    }

    /// Write a record if its version is still `version`. Err(Conflict)
    /// when another process changed it.
    pub fn put(
        &self,
        st: &mut Store,
        r: &Record,
        version: u64,
        now: u64,
    ) -> Result<u64, WriteError> {
        let value = serde_json::to_string(r).unwrap_or_default();
        st.put(
            &self.ns,
            &key(&r.feature, &r.id),
            &value,
            Some(version),
            None,
            &self.writer(),
            self.quota,
            now,
        )
    }

    /// Whether a live lease holds the task.
    pub fn lease_live(&self, st: &Store, feature: &str, id: &str, now: u64) -> bool {
        st.get(&self.ns, &lease_key(feature, id), now)
            .ok()
            .flatten()
            .is_some()
    }

    /// Take the lease for `owner`. false: another one holds it.
    pub fn claim(
        &self,
        st: &mut Store,
        feature: &str,
        id: &str,
        who: &Writer,
        ttl: u64,
        now: u64,
    ) -> Result<bool> {
        let k = lease_key(feature, id);
        let cur = st.get(&self.ns, &k, now)?;
        let version = match &cur {
            Some(e) if e.writer != who.sandbox => return Ok(false),
            Some(e) => e.version,
            None => 0,
        };
        let expires = now + ttl;
        let value =
            serde_json::json!({ "owner": who.sandbox, "harness": who.harness, "expires": expires })
                .to_string();
        match st.put(
            &self.ns,
            &k,
            &value,
            Some(version),
            Some(expires),
            who,
            self.quota,
            now,
        ) {
            Ok(_) => Ok(true),
            Err(WriteError::Conflict { .. }) => Ok(false),
            Err(e) => bail!("cannot claim {k}: {e:?}"),
        }
    }

    /// Free the lease of a task. A lease that is gone already is fine.
    pub fn release(&self, st: &mut Store, feature: &str, id: &str, now: u64) -> Result<()> {
        match st.delete(
            &self.ns,
            &lease_key(feature, id),
            None,
            &self.writer(),
            self.quota,
            now,
        ) {
            Ok(()) | Err(WriteError::Missing) => Ok(()),
            Err(e) => bail!("cannot free the lease of {id}: {e:?}"),
        }
    }
}
