//! Serialize project task decisions across spawn trees. The project record is
//! authoritative; the tree is a published view for watchers.
use super::{
    board,
    flow::{Spot, View},
    records, tasks, Layout,
};
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;

pub(super) fn transact<T, F>(spot: &Spot, write: bool, now: u64, action: F) -> Result<(View, T)>
where
    F: FnOnce(&tasks::Doc, &mut BTreeMap<String, records::Record>, u64) -> Result<T>,
{
    let path = records::ProjectRecords::path_for(&spot.layout, &spot.feature);
    let mut action = Some(action);
    let mut result = None;
    let run = |stored: &mut BTreeMap<String, records::Record>,
               action: &mut Option<F>|
     -> Result<(View, T)> {
        let text = spot
            .layout
            .read(&Layout::tasks_rel(&spot.feature))
            .context("feature has no tasks.md")?;
        let doc = tasks::parse(&text);
        let mut next = stored.clone();
        records::normalize(&mut next, now);
        let mut tasks: BTreeMap<_, _> = next
            .iter()
            .map(|(id, r)| (id.clone(), r.task.clone()))
            .collect();
        board::merge(
            &spot.feature,
            &doc,
            &mut tasks,
            &|r| {
                next.get(&r.id)
                    .is_some_and(|p| p.lease_expiry.is_none_or(|e| e > now))
            },
            now,
        );
        for (id, task) in tasks {
            next.entry(id).or_default().task = task;
        }
        let value = action.take().expect("transaction runs once")(&doc, &mut next, now)?;
        let mut tasks: BTreeMap<_, _> = next
            .iter()
            .map(|(id, r)| (id.clone(), r.task.clone()))
            .collect();
        let lease_expiries = next
            .iter()
            .filter_map(|(id, r)| r.lease_expiry.map(|e| (id.clone(), e)))
            .collect();
        if write {
            for r in tasks.values_mut().filter(|r| !r.removed) {
                r.rendered = Some(r.state == board::State::Done);
                next.get_mut(&r.id).expect("record exists").task = r.clone();
            }
            let mut st = spot.board.open()?;
            let old = spot.board.load(&st, &spot.feature, now)?;
            for (id, r) in &tasks {
                if old.get(id).is_none_or(|(previous, _)| previous != r) {
                    let version = old.get(id).map_or(0, |(_, v)| *v);
                    spot.board
                        .put(&mut st, r, version, now)
                        .map_err(|e| anyhow::anyhow!("cannot publish task {id}: {e:?}"))?;
                }
            }
            let marks = tasks
                .values()
                .filter(|r| !r.removed)
                .map(|r| (r.id.clone(), r.mark()))
                .collect();
            let out = tasks::render(&text, &marks);
            if out != text {
                if spot
                    .layout
                    .read(&Layout::tasks_rel(&spot.feature))
                    .as_deref()
                    != Some(&text)
                {
                    bail!("tasks.md changed during synchronization; try again");
                }
                crate::safefs::Root::open(&spot.layout.root)?.write(
                    &Layout::tasks_rel(&spot.feature),
                    out.as_bytes(),
                    0o644,
                )?;
            }
            *stored = next;
        }
        let ready = board::ready(&doc, &tasks);
        Ok((
            View {
                feature: spot.feature.clone(),
                doc,
                records: tasks,
                ready,
                lease_expiries,
            },
            value,
        ))
    };
    if write {
        records::update(&path, |stored| result = Some(run(stored, &mut action)))?;
        result.expect("transaction ran")
    } else {
        let mut stored = records::load(&path)?;
        run(&mut stored, &mut action)
    }
}
