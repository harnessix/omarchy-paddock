//! What paddock does with the tasks of a feature: read `tasks.md` back,
//! merge it with the records, write the checkboxes, claim a task for a
//! worker, and record the end of the worker and the review.
//!
//! The project records lock serializes task decisions across processes and
//! spawn trees. The tree records are the live view for watchers.

use super::board::{self, Board, Event, Record, State};
use super::tasks::Doc;
use super::Layout;
use crate::env::HostEnv;
use anyhow::{bail, Context as _, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// One feature of one project, and the spawn tree that holds its records.
#[derive(Debug, Clone)]
pub struct Spot {
    pub layout: Layout,
    pub feature: String,
    pub board: Board,
    /// The repository key, for host status and project identity.
    pub repo: PathBuf,
}

/// The merged state of a feature.
#[derive(Debug, Clone)]
pub struct View {
    pub feature: String,
    pub doc: Doc,
    pub records: BTreeMap<String, Record>,
    pub ready: Vec<String>,
    /// Lease expiry timestamps for the normalized project records.
    pub lease_expiries: BTreeMap<String, u64>,
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

    /// Read the file and the records, and merge them. With `write`, store
    /// the changed records and write the checkboxes into `tasks.md`.
    pub fn sync(&self, _env: &HostEnv, write: bool, now: u64) -> Result<View> {
        super::flow_store::transact(self, write, now, |_, _, _| Ok(())).map(|(view, ())| view)
    }

    /// Change a task under the project lock and publish it to the tree.
    pub fn update(
        &self,
        _env: &HostEnv,
        id: &str,
        now: u64,
        f: &dyn Fn(&mut Record) -> Result<()>,
    ) -> Result<Record> {
        super::flow_store::transact(self, true, now, |_, records, _| {
            let record = records
                .get_mut(id)
                .with_context(|| format!("no task {id}"))?;
            f(&mut record.task)?;
            record.task.updated = now;
            if !record.task.state.held() {
                record.lease_expiry = None;
            }
            Ok(record.task.clone())
        })
        .map(|(_, record)| record)
    }
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
/// `owner`. The project records lock makes the lease decision shared.
pub fn claim(env: &HostEnv, spot: &Spot, want: &str, owner: &Owner, now: u64) -> Result<Record> {
    let (_, record) = super::flow_store::transact(spot, true, now, |doc, records, _| {
        let tasks: BTreeMap<String, Record> = records
            .iter()
            .map(|(id, r)| (id.clone(), r.task.clone()))
            .collect();
        let ready = board::ready(doc, &tasks);
        let id = if want == "next" {
            ready.first().cloned().context("no task is ready")?
        } else {
            let r = tasks
                .get(want)
                .filter(|r| !r.removed)
                .with_context(|| format!("no task {want}"))?;
            if !matches!(r.state, State::Todo | State::Failed) {
                bail!("task {want} is {}", describe(r));
            }
            let waits = board::waits_for(doc, &tasks, want);
            if !waits.is_empty() {
                bail!("task {want} waits for {}", waits.join(", "));
            }
            want.to_string()
        };
        let acceptance = doc
            .task(&id)
            .map(|t| super::prompt::acceptance(&spot.layout, &spot.feature, doc, t))
            .unwrap_or_default();
        let record = records.get_mut(&id).expect("ready task has a record");
        let r = &mut record.task;
        r.state = board::next(r.state, Event::Claim)?;
        r.sandbox = Some(owner.sandbox.clone());
        r.spawn_id = Some(owner.spawn_id.clone());
        r.harness = Some(owner.harness.clone());
        r.branch = Some(owner.branch.clone());
        r.timer = Some(owner.timeout);
        r.lease = Some(board::lease_key(&spot.feature, &id));
        r.acceptance = acceptance;
        r.report = None;
        r.result = None;
        r.attempts += 1;
        r.updated = now;
        record.lease_expiry = Some(now.saturating_add(board::LEASE_TTL));
        Ok(r.clone())
    })?;
    let _ = env;
    Ok(record)
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
    super::flow_store::transact(spot, true, now, |_, records, _| {
        let r = records.get_mut(id).context("task disappeared")?;
        if !r.task.state.held()
            || r.task.sandbox.as_deref() != Some(&owner.sandbox)
            || r.task.spawn_id.as_deref() != Some(&owner.spawn_id)
        {
            bail!("task lease is no longer held by this worker");
        }
        r.lease_expiry = Some(now.saturating_add(board::LEASE_TTL));
        Ok(())
    })
    .is_ok()
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
    project: &Path,
    sandbox: &str,
    ok: bool,
    text: &str,
    now: u64,
) -> Result<Option<Record>> {
    let board = Board::tree(&crate::kv::kv_root(env), root, quota(env))?;
    let Some(rec) = held_by(&board, sandbox, now) else {
        return Ok(None);
    };
    let Some(layout) = Layout::resolve(env, project) else {
        return Ok(None);
    };
    let spot = Spot {
        layout,
        feature: rec.feature.clone(),
        board,
        repo: super::repo_of(project),
    };
    let result = spot.update(env, &rec.id, now, &|r| {
        if !r.state.held() || r.sandbox.as_deref() != Some(sandbox) || r.spawn_id != rec.spawn_id {
            bail!("task is no longer held by this worker");
        }
        r.report = Some(board::Report {
            ok,
            text: text.chars().take(2000).collect(),
            at: now,
        });
        Ok(())
    })?;
    Ok(Some(result))
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
        if !t.state.held() || t.spawn_id != rec.spawn_id {
            bail!("task is no longer held by this worker");
        }
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
        s.push_str(&format!(" on {b}"));
    }
    s
}

/// The view as JSON, for `spec status --json`.
pub fn to_json(v: &View, layout: &Layout) -> serde_json::Value {
    let tasks: Vec<serde_json::Value> = v
        .doc
        .tasks
        .iter()
        .filter_map(|t| v.records.get(&t.id))
        .map(|r| {
            let mut task = serde_json::to_value(r).unwrap_or_default();
            task["lease_expiry"] = v.lease_expiries.get(&r.id).copied().into();
            task
        })
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
        if let Some(lease) = &r.lease {
            let expiry = v
                .lease_expiries
                .get(&r.id)
                .map(|e| format!(" (expires {e})"))
                .unwrap_or_default();
            l.push(format!("      lease: {lease}{expiry}"));
        }
        if let Some(result) = &r.result {
            l.push(format!("      result: {result}"));
        }
        if let Some(note) = &r.review_note {
            l.push(format!("      review note: {note}"));
        }
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
