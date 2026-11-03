//! Spec Kit tasks through the broker (speckit.rs): `paddock-host spec`,
//! `paddock-host spawn --task`, and `paddock-host done` of a worker.
//! Never asked: a status is a read, and a review changes only the task
//! records and the checkboxes, which paddock writes for this sandbox's
//! own children.

use super::describe::arg_str;
use super::{audit, Broker, Request, Response, Status, PROTOCOL};
use crate::speckit::flow::{self, Owner, Spot};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How often paddock renews the claim of a running worker.
const RENEW: Duration = Duration::from_secs(30);

impl Broker {
    /// The root of this sandbox's spawn tree: the records live there.
    pub(in crate::broker) fn tree_root(&self) -> String {
        self.ctx
            .chain
            .first()
            .cloned()
            .unwrap_or_else(|| self.ctx.id.clone())
    }

    fn spot(&self, req: &Request) -> Result<Spot, Response> {
        match Spot::find(
            &self.env,
            &self.ctx.project,
            &self.tree_root(),
            arg_str(req, "feature"),
        ) {
            Ok(Some(s)) => Ok(s),
            Ok(None) => Err(Response::new(
                Status::Disabled,
                "this project is no Spec Kit project (it has no .specify/ folder), or [speckit] enabled = false in the user configuration",
            )),
            Err(e) => Err(Response::new(Status::Invalid, format!("{e:#}"))),
        }
    }

    /// `paddock-host spec status|review|note`.
    pub(in crate::broker) fn spec(&self, req: &Request) -> Response {
        if req.v != PROTOCOL {
            return Response::new(Status::Invalid, "unsupported protocol version");
        }
        let now = crate::state::now();
        let action = arg_str(req, "action").unwrap_or("status");
        if action == "note" {
            let f = flow::facts(
                &self.env,
                &self.ctx.project,
                &self.tree_root(),
                &self.ctx.id,
            );
            return Response::done(serde_json::json!({
                "facts": f,
                "lines": f.as_ref().map(crate::speckit::prompt::lines).unwrap_or_default(),
            }));
        }
        let spot = match self.spot(req) {
            Ok(s) => s,
            Err(r) => return r,
        };
        let resp = match action {
            // The sandbox that hands out tasks writes the checkboxes; any
            // other one only reads.
            "status" => match spot.sync(&self.env, self.spawner.is_some(), now) {
                Ok(v) => {
                    let mut j = flow::to_json(&v, &spot.layout);
                    j["text"] = flow::to_text(&v).into();
                    return Response::done(j);
                }
                Err(e) => return Response::new(Status::Failed, format!("{e:#}")),
            },
            "review" => {
                if self.spawner.is_none() {
                    return Response::new(
                        Status::Disabled,
                        "only the sandbox that starts the workers reviews their tasks",
                    );
                }
                let id = arg_str(req, "id").unwrap_or_default();
                let pass = match arg_str(req, "verdict") {
                    Some("pass") => true,
                    Some("changes") => false,
                    _ => return Response::new(Status::Invalid, "the verdict is pass or changes"),
                };
                match flow::review(
                    &self.env,
                    &spot,
                    id,
                    pass,
                    arg_str(req, "text").unwrap_or(""),
                    now,
                ) {
                    Ok(r) => Response::done(serde_json::to_value(&r).unwrap_or_default()),
                    Err(e) => Response::new(Status::Failed, format!("{e:#}")),
                }
            }
            other => return Response::new(Status::Invalid, format!("unknown spec action {other}")),
        };
        audit::audit(
            &self.env,
            &self.ctx,
            req,
            &format!("spec review {}", arg_str(req, "id").unwrap_or("")),
            None,
            "allowed",
            &resp,
            None,
        );
        resp
    }

    /// Check `spawn --task WANT` before the card: the feature and the task
    /// that the worker would get now. Returns the spot and a line for the
    /// card.
    pub(in crate::broker) fn task_preview(
        &self,
        req: &Request,
        want: &str,
    ) -> Result<(Spot, String), Response> {
        let spot = self.spot(req)?;
        let v = spot
            .sync(&self.env, true, crate::state::now())
            .map_err(|e| Response::new(Status::Failed, format!("{e:#}")))?;
        let id = if want == "next" {
            match v.ready.first() {
                Some(id) => id.clone(),
                None => {
                    return Err(Response::new(
                        Status::Failed,
                        format!(
                            "no task of {} is ready: `paddock-host spec status` shows why",
                            v.feature
                        ),
                    ))
                }
            }
        } else {
            want.to_string()
        };
        let Some(r) = v.records.get(&id).filter(|r| !r.removed) else {
            return Err(Response::new(
                Status::Invalid,
                format!("no task {id} in feature {}", v.feature),
            ));
        };
        let line = format!(
            "Task {id} of the Spec Kit feature {}{}: {}\nThe prompt comes from tasks.md, the spec, the plan and the constitution{}.",
            v.feature,
            if want == "next" { " (the next ready task)" } else { "" },
            r.text,
            if spot.layout.external { " (paths in the store)" } else { " (copies)" }
        );
        Ok((spot, line))
    }

    /// Start a worker for a task: claim it, write its prompt, start the
    /// child, and keep the claim alive while it runs.
    pub(in crate::broker) fn start_task(
        &self,
        sp: &Arc<crate::spawn::Spawner>,
        harness: &str,
        opts: crate::spawn::StartOpts,
        spot: &Spot,
        want: &str,
    ) -> Response {
        let now = crate::state::now();
        let id = crate::spawn::new_spawn_id();
        let owner = Owner {
            sandbox: crate::spawn::sandbox_id_for(harness, &id),
            spawn_id: id.clone(),
            harness: harness.to_string(),
            branch: crate::spawn::branch_for(harness, &id),
            timeout: opts.limits.timeout,
        };
        let rec = match flow::claim(&self.env, spot, want, &owner, now) {
            Ok(r) => r,
            Err(e) => return Response::new(Status::Failed, format!("{e:#}")),
        };
        let prompt = crate::speckit::prompt::worker(&spot.layout, &rec, spot.layout.external);
        match sp.start_with_id(id.clone(), harness, prompt.as_bytes(), opts) {
            Ok(s) => {
                if let Err(e) = flow::started(&self.env, spot, &rec.id, crate::state::now()) {
                    log::debug!("task {}: {e:#}", rec.id);
                }
                let (sp, spot, task) = (sp.clone(), spot.clone(), rec.id.clone());
                std::thread::spawn(move || {
                    let mut last = Instant::now();
                    loop {
                        std::thread::sleep(Duration::from_secs(1));
                        if !sp.status(&owner.spawn_id).is_some_and(|s| s.state.active()) {
                            return;
                        }
                        if last.elapsed() >= RENEW {
                            if !flow::renew(&spot, &task, &owner, crate::state::now()) {
                                log::warn!(
                                    "task {task}: cannot renew the claim of {}",
                                    owner.sandbox
                                );
                            }
                            last = Instant::now();
                        }
                    }
                });
                let mut j = serde_json::to_value(&s).unwrap_or_default();
                j["task"] = rec.id.clone().into();
                j["feature"] = rec.feature.clone().into();
                Response::done(j)
            }
            Err(e) => {
                let why = format!("the worker did not start: {e:#}");
                if let Err(u) = flow::unclaim(&self.env, spot, &rec.id, &why, crate::state::now()) {
                    log::warn!("task {}: {u:#}", rec.id);
                }
                Response::new(Status::Failed, format!("{e:#}"))
            }
        }
    }

    /// `paddock-host done` in a worker of a task that is not detached: keep
    /// the report for the task record. None: this sandbox holds no task.
    pub(in crate::broker) fn report_task(&self, req: &Request) -> Option<Response> {
        if self.ctx.chain.is_empty() {
            return None;
        }
        let ok = arg_str(req, "status").unwrap_or("ok") != "failed";
        let text = arg_str(req, "text").unwrap_or("");
        match flow::report(
            &self.env,
            &self.tree_root(),
            &self.ctx.id,
            ok,
            text,
            crate::state::now(),
        ) {
            Ok(Some(r)) => Some(Response::done(serde_json::json!({
                "message": format!("recorded for task {}. Finish your answer: the run ends with it, and paddock marks the task.", r.id),
                "task": r.id,
            }))),
            Ok(None) => None,
            Err(e) => Some(Response::new(Status::Failed, format!("{e:#}"))),
        }
    }
}
