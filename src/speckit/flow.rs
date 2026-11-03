//! What paddock does with the tasks of a feature: read `tasks.md` back,
//! merge it with the records, write the checkboxes, claim a task for a
//! worker, and record the end of the worker and the review.
//!
//! paddock is the only writer of `tasks.md` and of the records. One lock
//! keeps the threads of this process apart; kv versions keep the processes
//! apart.

use super::board::{self, Board, Event, Record, State};
use super::tasks::{self, Doc};
use super::Layout;
use crate::env::HostEnv;
use anyhow::{bail, Context as _, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

static SYNC: Mutex<()> = Mutex::new(());
const RETRIES: usize = 5;

/// One feature of one project, and the spawn tree that holds its records.
#[derive(Debug, Clone)]
pub struct Spot {
    pub layout: Layout,
    pub feature: String,
    pub board: Board,
    /// The repository key, for records of earlier runs.
    pub repo: PathBuf,
}

/// The merged state of a feature.
#[derive(Debug, Clone)]
pub struct View {
    pub feature: String,
    pub doc: Doc,
    pub records: BTreeMap<String, Record>,
    pub ready: Vec<String>,
}

pub fn quota(env: &HostEnv) -> crate::kv::store::Quota {
    let cfg = crate::kv::KvCfg::load(env).unwrap_or_default();
    crate::kv::store::Quota {
        bytes: cfg.project_bytes,
        keys: cfg.max_keys,
    }
}

impl Spot {
    /// The spot of `project` in the tree of `root`. None: no Spec Kit
    /// project, or Spec Kit mode is off.
    pub fn find(
        env: &HostEnv,
        project: &Path,
        root: &str,
        feature: Option<&str>,
    ) -> Result<Option<Self>> {
        let Some(layout) = Layout::resolve(env, project) else {
            return Ok(None);
        };
        let feature = match feature {
            Some(f) if super::valid_feature(f) => f.to_string(),
            Some(f) => bail!("'{f}' is not a feature folder name"),
            None => layout
                .feature()
                .context("this Spec Kit project has no feature with a tasks.md yet: run /speckit-tasks first")?,
        };
        let board = Board::tree(&crate::kv::kv_root(env), root, quota(env))?;
        Ok(Some(Self {
            layout,
            feature,
            board,
            repo: super::repo_of(project),
        }))
    }

    fn tasks_text(&self) -> Result<String> {
        let rel = Layout::tasks_rel(&self.feature);
        self.layout.read(&rel).with_context(|| {
            format!(
                "{} has no tasks.md: run /speckit-tasks first",
                Path::new(super::SPECS).join(&self.feature).display()
            )
        })
    }

    /// Read the file and the records, and merge them. With `write`, store
    /// the changed records and write the checkboxes into `tasks.md`.
    pub fn sync(&self, env: &HostEnv, write: bool, now: u64) -> Result<View> {
        let _g = SYNC.lock().unwrap_or_else(|e| e.into_inner());
        for _ in 0..RETRIES {
            if let Some(v) = self.try_sync(env, write, now)? {
                return Ok(v);
            }
        }
        bail!("the task records changed too often; try again")
    }

    fn try_sync(&self, env: &HostEnv, write: bool, now: u64) -> Result<Option<View>> {
        let text = self.tasks_text()?;
        let doc = tasks::parse(&text);
        if !write && !self.board.file.is_file() {
            // No records yet: the file alone, and no store file is made.
            let mut records = BTreeMap::new();
            board::merge(&self.feature, &doc, &mut records, &|_| false, now);
            let ready = board::ready(&doc, &records);
            return Ok(Some(View {
                feature: self.feature.clone(),
                doc,
                records,
                ready,
            }));
        }
        let mut st = self.board.open()?;
        let mut stored = self.board.load(&st, &self.feature, now)?;
        if stored.is_empty() && write {
            for r in earlier_records(env, &self.repo, &self.board, &self.feature, now) {
                stored.insert(r.id.clone(), (r, 0));
            }
        }
        let versions: BTreeMap<String, u64> =
            stored.iter().map(|(k, (_, v))| (k.clone(), *v)).collect();
        let mut records: BTreeMap<String, Record> =
            stored.into_iter().map(|(k, (r, _))| (k, r)).collect();
        let live = |r: &Record| self.board.lease_live(&st, &self.feature, &r.id, now);
        let mut changed = board::merge(&self.feature, &doc, &mut records, &live, now);
        if write {
            for r in records.values_mut().filter(|r| !r.removed) {
                let want = Some(r.state == State::Done);
                if r.rendered != want {
                    r.rendered = want;
                    changed.push(r.id.clone());
                }
            }
            changed.sort();
            changed.dedup();
            for id in &changed {
                let v = versions.get(id).copied().unwrap_or(0);
                match self.board.put(&mut st, &records[id], v, now) {
                    Ok(_) => {}
                    Err(crate::kv::store::WriteError::Conflict { .. }) => return Ok(None),
                    Err(e) => bail!("cannot store task {id}: {e:?}"),
                }
            }
            let marks = records
                .values()
                .filter(|r| !r.removed)
                .map(|r| (r.id.clone(), r.mark()))
                .collect();
            let out = tasks::render(&text, &marks);
            if out != text {
                // The file may have changed since the read: the user's edit wins.
                if self.tasks_text()? != text {
                    return Ok(None);
                }
                let root = crate::safefs::Root::open(&self.layout.root)?;
                root.write(&Layout::tasks_rel(&self.feature), out.as_bytes(), 0o644)?;
            }
        }
        let ready = board::ready(&doc, &records);
        Ok(Some(View {
            feature: self.feature.clone(),
            doc,
            records,
            ready,
        }))
    }

    /// Change one record with `f`, store it, and write the checkboxes.
    pub fn update(
        &self,
        env: &HostEnv,
        id: &str,
        now: u64,
        f: &dyn Fn(&mut Record) -> Result<()>,
    ) -> Result<Record> {
        for _ in 0..RETRIES {
            let mut st = self.board.open()?;
            let stored = self.board.load(&st, &self.feature, now)?;
            let Some((mut r, v)) = stored.get(id).cloned() else {
                bail!("no task {id} in feature {}", self.feature);
            };
            f(&mut r)?;
            r.updated = now;
            match self.board.put(&mut st, &r, v, now) {
                Ok(_) => {
                    drop(st);
                    if let Err(e) = self.sync(env, true, now) {
                        log::warn!("cannot write the checkboxes of {}: {e:#}", self.feature);
                    }
                    return Ok(r);
                }
                Err(crate::kv::store::WriteError::Conflict { .. }) => continue,
                Err(e) => bail!("cannot store task {id}: {e:?}"),
            }
        }
        bail!("task {id} changed too often; try again")
    }
}

/// Records of this feature from the newest earlier spawn tree of the same
/// repository, so a new run continues where the last one stopped. A held
/// task is free again: its worker is gone.
fn earlier_records(env: &HostEnv, repo: &Path, me: &Board, feature: &str, now: u64) -> Vec<Record> {
    let root = crate::kv::kv_root(env);
    let Ok(reg) = crate::kv::store::Store::open(&crate::kv::shard::registry(&root)) else {
        return Vec::new();
    };
    let mut roots = reg.sandboxes_of(repo).unwrap_or_default();
    roots.retain(|r| r.parent.is_none());
    roots.sort_by_key(|r| std::cmp::Reverse(r.started));
    for r in roots {
        let Ok(b) = Board::tree(&root, &r.id, me.quota) else {
            continue;
        };
        if b.file == me.file || !b.file.is_file() {
            continue;
        }
        let Ok(st) = b.open() else { continue };
        let found = b.load(&st, feature, now).unwrap_or_default();
        if found.is_empty() {
            continue;
        }
        return found
            .into_values()
            .map(|(mut r, _)| {
                if r.state.held() {
                    r.state = State::Todo;
                    r.lease = None;
                }
                r
            })
            .collect();
    }
    Vec::new()
}

/// The newest spawn tree of a repository with records of `feature`: what
/// `paddock spec status` on the host shows.
pub fn newest_tree(env: &HostEnv, repo: &Path, feature: &str, now: u64) -> Option<String> {
    let root = crate::kv::kv_root(env);
    let reg = crate::kv::store::Store::open(&crate::kv::shard::registry(&root)).ok()?;
    let mut roots = reg.sandboxes_of(repo).ok()?;
    roots.retain(|r| r.parent.is_none());
    roots.sort_by_key(|r| std::cmp::Reverse(r.started));
    roots.into_iter().map(|r| r.id).find(|id| {
        Board::tree(&root, id, quota(env))
            .ok()
            .filter(|b| b.file.is_file())
            .and_then(|b| Some(!b.load(&b.open().ok()?, feature, now).ok()?.is_empty()))
            .unwrap_or(false)
    })
}

/// The worker that takes a task.
#[derive(Debug, Clone)]
pub struct Owner {
    pub sandbox: String,
    pub spawn_id: String,
    pub harness: String,
    pub branch: String,
    pub timeout: u64,
}

impl Owner {
    pub fn writer(&self) -> crate::kv::store::Writer<'_> {
        crate::kv::store::Writer {
            sandbox: &self.sandbox,
            harness: &self.harness,
        }
    }
}

/// Claim `want` (a task id, or `next` for the first ready task) for
/// `owner`. Two workers never get one task: the lease decides.
pub fn claim(env: &HostEnv, spot: &Spot, want: &str, owner: &Owner, now: u64) -> Result<Record> {
    let view = spot.sync(env, true, now)?;
    let candidates: Vec<String> = if want == "next" {
        if view.ready.is_empty() {
            bail!("{}", nothing_ready(&view));
        }
        view.ready.clone()
    } else {
        let Some(r) = view.records.get(want).filter(|r| !r.removed) else {
            bail!(
                "no task {want} in {}",
                Layout::tasks_rel(&spot.feature).display()
            );
        };
        if !matches!(r.state, State::Todo | State::Failed) {
            bail!("task {want} is {}", describe(r));
        }
        let waits = board::waits_for(&view.doc, &view.records, want);
        if !waits.is_empty() {
            bail!("task {want} waits for {}", waits.join(", "));
        }
        vec![want.to_string()]
    };
    let mut st = spot.board.open()?;
    for id in &candidates {
        if !spot.board.claim(
            &mut st,
            &spot.feature,
            id,
            &owner.writer(),
            board::LEASE_TTL,
            now,
        )? {
            continue;
        }
        drop(st);
        let task = view.doc.task(id).cloned();
        let acceptance = task
            .as_ref()
            .map(|t| super::prompt::acceptance(&spot.layout, &spot.feature, &view.doc, t))
            .unwrap_or_default();
        let o = owner.clone();
        let feature = spot.feature.clone();
        let r = spot.update(env, id, now, &|r| {
            r.state = board::next(r.state, Event::Claim)?;
            r.sandbox = Some(o.sandbox.clone());
            r.spawn_id = Some(o.spawn_id.clone());
            r.harness = Some(o.harness.clone());
            r.branch = Some(o.branch.clone());
            r.timer = Some(o.timeout);
            r.lease = Some(board::lease_key(&feature, &r.id));
            r.acceptance = acceptance.clone();
            r.report = None;
            r.result = None;
            r.attempts += 1;
            Ok(())
        });
        if r.is_err() {
            if let Ok(mut st) = spot.board.open() {
                let _ = spot.board.release(&mut st, &spot.feature, id, now);
            }
        }
        return r;
    }
    if want == "next" {
        bail!("another worker took each ready task at the same moment; try again")
    }
    bail!("another worker holds task {want}")
}

/// The worker started.
pub fn started(env: &HostEnv, spot: &Spot, id: &str, now: u64) -> Result<Record> {
    spot.update(env, id, now, &|r| {
        r.state = board::next(r.state, Event::Start)?;
        Ok(())
    })
}

/// The worker did not start: free the task.
pub fn unclaim(env: &HostEnv, spot: &Spot, id: &str, why: &str, now: u64) -> Result<Record> {
    if let Ok(mut st) = spot.board.open() {
        spot.board.release(&mut st, &spot.feature, id, now)?;
    }
    let why = why.to_string();
    spot.update(env, id, now, &|r| {
        r.state = board::next(r.state, Event::Free)?;
        r.lease = None;
        r.result = Some(why.clone());
        Ok(())
    })
}

/// Renew the lease of a running worker. false: it is gone.
pub fn renew(spot: &Spot, id: &str, owner: &Owner, now: u64) -> bool {
    let Ok(mut st) = spot.board.open() else {
        return false;
    };
    spot.board
        .claim(
            &mut st,
            &spot.feature,
            id,
            &owner.writer(),
            board::LEASE_TTL,
            now,
        )
        .unwrap_or(false)
}

/// The review of a task in `review`: pass marks it done, changes sends it
/// back to todo with the note for the next worker.
pub fn review(
    env: &HostEnv,
    spot: &Spot,
    id: &str,
    pass: bool,
    note: &str,
    now: u64,
) -> Result<Record> {
    spot.sync(env, true, now)?;
    let note = note.trim().to_string();
    spot.update(env, id, now, &|r| {
        r.state = board::next(r.state, Event::Verdict(pass))?;
        r.verdict = Some(if pass { "pass" } else { "changes" }.into());
        r.review_note = (!note.is_empty()).then(|| note.clone());
        Ok(())
    })
}

/// The record that a sandbox holds, in any feature of the tree.
pub fn held_by(board: &Board, sandbox: &str, now: u64) -> Option<Record> {
    let st = board.open().ok()?;
    board
        .all(&st, now)
        .ok()?
        .into_iter()
        .map(|(r, _)| r)
        .find(|r| r.sandbox.as_deref() == Some(sandbox) && r.state.held())
}

/// `paddock-host done` in a worker: keep its report for the end.
pub fn report(
    env: &HostEnv,
    root: &str,
    sandbox: &str,
    ok: bool,
    text: &str,
    now: u64,
) -> Result<Option<Record>> {
    let board = Board::tree(&crate::kv::kv_root(env), root, quota(env))?;
    for _ in 0..RETRIES {
        let mut st = board.open()?;
        let Some((mut r, v)) = board
            .all(&st, now)?
            .into_iter()
            .find(|(r, _)| r.sandbox.as_deref() == Some(sandbox) && r.state.held())
        else {
            return Ok(None);
        };
        r.report = Some(board::Report {
            ok,
            text: text.chars().take(2000).collect(),
            at: now,
        });
        r.updated = now;
        match board.put(&mut st, &r, v, now) {
            Ok(_) => return Ok(Some(r)),
            Err(crate::kv::store::WriteError::Conflict { .. }) => continue,
            Err(e) => bail!("cannot store the report: {e:?}"),
        }
    }
    bail!("the task record changed too often")
}

/// A child of the spawner ended: record the result, free the lease, and
/// write the checkboxes. A child without a task changes nothing.
pub fn child_ended(
    env: &HostEnv,
    root: &str,
    project: &Path,
    s: &crate::spawn::Status,
    r: &crate::spawn::ChildResult,
) {
    let now = crate::state::now();
    let Ok(board) = Board::tree(&crate::kv::kv_root(env), root, quota(env)) else {
        return;
    };
    let Some(rec) = board.open().ok().and_then(|st| {
        board
            .all(&st, now)
            .ok()?
            .into_iter()
            .map(|(r, _)| r)
            .find(|r| r.spawn_id.as_deref() == Some(s.id.as_str()) && r.state.held())
    }) else {
        return;
    };
    let layout = Layout::resolve(env, project);
    let spot = Spot {
        layout: layout.clone().unwrap_or(Layout {
            root: project.to_path_buf(),
            external: false,
        }),
        feature: rec.feature.clone(),
        board,
        repo: super::repo_of(project),
    };
    let exited_ok = s.state == crate::spawn::State::Exited && s.exit_code == Some(0);
    let summary = crate::spawn::result_value(s, r);
    let summary: String = serde_json::from_str::<serde_json::Value>(&summary)
        .ok()
        .and_then(|v| v["summary"].as_str().map(str::to_string))
        .unwrap_or_default();
    if let Ok(mut st) = spot.board.open() {
        let _ = spot.board.release(&mut st, &spot.feature, &rec.id, now);
    }
    let res = spot.update(env, &rec.id, now, &|t| {
        let ok = exited_ok && t.report.as_ref().is_none_or(|p| p.ok);
        t.state = board::next(t.state, Event::Finish(ok))?;
        t.lease = None;
        t.result = Some(match &t.report {
            Some(p) if !p.text.is_empty() => format!("{}; {summary}", p.text),
            _ => summary.clone(),
        });
        Ok(())
    });
    if layout.is_none() {
        log::debug!("spec: {} is no Spec Kit project now", project.display());
    }
    if let Err(e) = res {
        log::warn!(
            "spawn {}: cannot record the end of task {}: {e:#}",
            s.id,
            rec.id
        );
    }
}

/// The state, the owner and the branch in a few words.
pub fn describe(r: &Record) -> String {
    let mut s = r.state.name().to_string();
    if let (true, Some(sb)) = (r.state.held(), &r.sandbox) {
        s.push_str(&format!(" by {sb}"));
    }
    if let Some(b) = &r.branch {
        if r.state != State::Todo {
            s.push_str(&format!(" on {b}"));
        }
    }
    s
}

fn nothing_ready(v: &View) -> String {
    let open: Vec<&Record> = v
        .doc
        .tasks
        .iter()
        .filter_map(|t| v.records.get(&t.id))
        .filter(|r| r.state != State::Done && !r.removed)
        .collect();
    if open.is_empty() {
        return format!("every task of {} is done", v.feature);
    }
    let busy: Vec<String> = open
        .iter()
        .filter(|r| r.state != State::Todo)
        .map(|r| format!("{} {}", r.id, describe(r)))
        .collect();
    if busy.is_empty() {
        format!("no task of {} is ready", v.feature)
    } else {
        format!(
            "no task of {} is ready: they wait for {}",
            v.feature,
            busy.join(", ")
        )
    }
}

/// The view as JSON, for `spec status --json`.
pub fn to_json(v: &View, layout: &Layout) -> serde_json::Value {
    let tasks: Vec<serde_json::Value> = v
        .doc
        .tasks
        .iter()
        .filter_map(|t| v.records.get(&t.id))
        .map(|r| serde_json::to_value(r).unwrap_or_default())
        .collect();
    let review: Vec<&String> = v
        .records
        .values()
        .filter(|r| r.state == State::Review && !r.removed)
        .map(|r| &r.id)
        .collect();
    serde_json::json!({
        "feature": v.feature,
        "store": if layout.external { "external" } else { "repo" },
        "tasks_file": Layout::tasks_rel(&v.feature),
        "tasks": tasks,
        "ready": v.ready,
        "ready_groups": groups(v),
        "review": review,
        "key_prefix": board::prefix(&v.feature),
    })
}

/// The ready tasks in groups that can start together: a run of `[P]` tasks
/// is one group, any other task is a group of its own.
pub fn groups(v: &View) -> Vec<Vec<String>> {
    let mut out: Vec<Vec<String>> = Vec::new();
    let mut last_p = false;
    for id in &v.ready {
        let p = v.records.get(id).is_some_and(|r| r.parallel);
        match out.last_mut() {
            Some(g) if p && last_p => g.push(id.clone()),
            _ => out.push(vec![id.clone()]),
        }
        last_p = p;
    }
    out
}

/// The view as text, for `spec status`.
pub fn to_text(v: &View) -> String {
    let mut l = vec![format!("feature {}", v.feature)];
    let mut phase = usize::MAX;
    for t in &v.doc.tasks {
        let Some(r) = v.records.get(&t.id) else {
            continue;
        };
        if t.phase != phase {
            phase = t.phase;
            l.push(format!("  {}", v.doc.phase_title(t)));
        }
        let text: String = r.text.chars().take(60).collect();
        l.push(format!(
            "    {:<5} {:<4} {:<28} {text}",
            r.id,
            if r.parallel { "[P]" } else { "" },
            describe(r)
        ));
    }
    let removed: Vec<&str> = v
        .records
        .values()
        .filter(|r| r.removed)
        .map(|r| r.id.as_str())
        .collect();
    if !removed.is_empty() {
        l.push(format!("removed from tasks.md: {}", removed.join(", ")));
    }
    let g = groups(v);
    l.push(if g.is_empty() {
        "ready: none".into()
    } else {
        format!(
            "ready: {}",
            g.iter()
                .map(|g| g.join(" + "))
                .collect::<Vec<_>>()
                .join(", ")
        )
    });
    let review: Vec<&str> = v
        .records
        .values()
        .filter(|r| r.state == State::Review && !r.removed)
        .map(|r| r.id.as_str())
        .collect();
    if !review.is_empty() {
        l.push(format!("waits for review: {}", review.join(", ")));
    }
    l.join("\n")
}

/// The spec facts of a run for its notes: the feature, the task that the
/// sandbox holds, and for the root of the tree a count of the tasks.
pub fn facts(
    env: &HostEnv,
    project: &Path,
    root: &str,
    sandbox: &str,
) -> Option<super::prompt::Facts> {
    let now = crate::state::now();
    let board = Board::tree(&crate::kv::kv_root(env), root, quota(env)).ok()?;
    let held = board
        .file
        .is_file()
        .then(|| held_by(&board, sandbox, now))
        .flatten();
    let feature = held.as_ref().map(|r| r.feature.clone());
    let spot = Spot::find(env, project, root, feature.as_deref()).ok()??;
    let mut f = super::prompt::Facts {
        feature: spot.feature.clone(),
        paths: super::prompt::paths(&spot.layout, &spot.feature)
            .into_iter()
            .map(|(_, p)| p.display().to_string())
            .collect(),
        ..Default::default()
    };
    match held {
        Some(r) => {
            f.task = Some(format!("{} {}", r.id, r.text));
            f.acceptance = r.acceptance;
        }
        None => {
            let v = spot.sync(env, false, now).ok()?;
            let count = |s: State| {
                v.records
                    .values()
                    .filter(|r| !r.removed && r.state == s)
                    .count()
            };
            f.summary = Some(format!(
                "{} of {} done, {} ready, {} running, {} wait for review",
                count(State::Done),
                v.records.values().filter(|r| !r.removed).count(),
                v.ready.len(),
                count(State::Running) + count(State::Claimed),
                count(State::Review)
            ));
        }
    }
    Some(f)
}
