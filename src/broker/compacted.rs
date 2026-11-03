//! `paddock-host compacted`: a harness hook after a context compaction
//! (compaction.rs). The broker counts it, writes `compacted:ID` into the
//! kv scope where the parent or the pasture sees it, writes one audit line,
//! and returns the facts for the note. Never asked: it changes nothing
//! outside this run's own records.

use super::{audit, Broker, Request, Response, Status, PROTOCOL};
use crate::compaction::{self, Facts};
use crate::kv::Scope;
use std::path::PathBuf;

/// What the broker knows of its run beyond `Context`, for the note.
#[derive(Debug, Clone, Default)]
pub struct RunFacts {
    /// The id that `paddock-host spawn` printed in the parent.
    pub spawn_id: Option<String>,
    /// The pasture this sandbox joined (`--pasture NAME`).
    pub pasture: Option<String>,
    /// The task file as a path inside the sandbox: the prompt of a spawn
    /// child or of a detached run.
    pub task_file: Option<PathBuf>,
}

impl Broker {
    pub(super) fn compacted(&self, req: &Request) -> Response {
        if req.v != PROTOCOL {
            return Response::new(Status::Invalid, "unsupported protocol version");
        }
        let cfg = compaction::Cfg::load(&self.env);
        if !cfg.enabled {
            return Response::new(
                Status::Disabled,
                "compaction notes are off in [compaction] of the user configuration",
            );
        }
        let count = match compaction::record(&self.env, &self.ctx.id) {
            Ok(n) => n,
            Err(e) => return Response::new(Status::Failed, format!("{e:#}")),
        };
        let facts = Facts {
            sandbox: self.ctx.id.clone(),
            harness: self.ctx.harness.clone(),
            role: self.ctx.role.map(|r| r.name().to_string()),
            parent: self.ctx.chain.last().cloned(),
            spawn_id: self.run.spawn_id.clone(),
            pasture: self.run.pasture.clone(),
            task_file: self.run.task_file.as_ref().map(|p| p.display().to_string()),
            detached: self.detach.is_some(),
            note: cfg.note.clone(),
            count,
            spec: crate::speckit::flow::facts(
                &self.env,
                &self.ctx.project,
                &self.tree_root(),
                &self.ctx.id,
            ),
        };
        let recorded = self.record_in_kv(&facts);
        log::info!(
            "sandbox {}: context compaction {count}{}",
            self.ctx.id,
            recorded
                .as_ref()
                .map(|(s, k)| format!(", {k} in kv scope {s}"))
                .unwrap_or_default()
        );
        if cfg.notify {
            let n = super::notify::Note {
                summary: format!("{}: context compacted", self.ctx.harness),
                body: format!("{} (compaction {count})", self.ctx.id),
                ..Default::default()
            };
            if let Err(e) = super::notify::show(&self.env, &n) {
                log::debug!("no compaction notification: {e:#}");
            }
        }
        let mut result = serde_json::to_value(&facts).unwrap_or_default();
        if let Some((scope, key)) = &recorded {
            result["kv"] = serde_json::json!({ "scope": scope, "key": key });
        }
        let resp = Response::done(result);
        let detail = format!(
            "context compaction {count}{}",
            recorded
                .map(|(s, k)| format!(", recorded as {k} in kv scope {s}"))
                .unwrap_or_default()
        );
        audit::audit(
            &self.env, &self.ctx, req, &detail, None, "allowed", &resp, None,
        );
        resp
    }

    /// `compacted:<SPAWN-ID>` in `tree` for a spawn child, where the parent
    /// watches, or `compacted:<RUN-ID>` in `pasture` for a pasture member.
    /// Returns the scope and the key that were written.
    fn record_in_kv(&self, f: &Facts) -> Option<(&'static str, String)> {
        let svc = self.kv.get()?;
        let (scope, key) = if !self.ctx.chain.is_empty() {
            let id = f.spawn_id.as_deref().unwrap_or(&f.sandbox);
            (Scope::Tree, format!("{}{id}", compaction::KEY_PREFIX))
        } else if f.pasture.is_some() {
            (
                Scope::Pasture,
                format!("{}{}", compaction::KEY_PREFIX, f.sandbox),
            )
        } else {
            return None;
        };
        let req = crate::kv::Request {
            v: crate::kv::PROTOCOL,
            op: "put".into(),
            scope: Some(scope),
            key: Some(key.clone()),
            value: Some(compaction::record_value(f, crate::state::now())),
            ..Default::default()
        };
        let r = svc.handle(&req, None);
        if r.status != crate::kv::Status::Done {
            log::warn!(
                "sandbox {}: cannot record the compaction in kv: {:?} {}",
                self.ctx.id,
                r.status,
                r.message
            );
            return None;
        }
        Some((scope.name(), key))
    }
}
