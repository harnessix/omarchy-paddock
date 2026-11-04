//! Child sandboxes that this sandbox starts.

use super::audit::{audit, audit_detail};
use super::describe::arg_str;
use super::prepare::{Prepared, Work};
use super::{read_prompt, Broker, Request, Response, Status, INBOX};
use std::time::Duration;

impl Broker {
    pub(in crate::broker) fn spawn_off_reason(&self) -> String {
        match &self.spawner {
            None => "spawn is not available in this sandbox".into(),
            Some(s) if !s.cfg.depth_allows(self.ctx.depth) => format!(
                "this sandbox was started by {}; [spawn] max_depth = {} does not let it start more",
                self.ctx.chain.last().map_or("the user", |p| p.as_str()),
                s.cfg.max_depth
            ),
            Some(s) if s.cfg.harnesses.is_empty() => {
                "no harness is allowed: the user lists them in [spawn] harnesses".into()
            }
            _ => "spawn is set to deny in [host] of the user configuration".into(),
        }
    }

    /// Work out a spawn request before the user is asked. Err is the
    /// response to send instead.
    pub(in crate::broker) fn prepare_spawn(
        &self,
        req: &Request,
    ) -> std::result::Result<Prepared, Response> {
        let Some(sp) = &self.spawner else {
            return Err(Response::new(Status::Disabled, self.spawn_off_reason()));
        };
        let action = arg_str(req, "action").unwrap_or("start");
        if action != "start" {
            return Ok(Prepared {
                summary: format!("spawn {action}"),
                detail: format!("spawn {action}"),
                keys: vec![],
                session_allowed: false,
                always_ask: false,
                read_only: true,
                audit: None,
                work: Work::SpawnControl,
            });
        }
        let harness = arg_str(req, "harness").unwrap_or_default().to_string();
        if !sp.cfg.allows(&harness) {
            return Err(Response::new(
                Status::Disabled,
                format!(
                    "{harness} is not in [spawn] harnesses of the user configuration (allowed: {})",
                    if sp.cfg.harnesses.is_empty() {
                        "none".to_string()
                    } else {
                        sp.cfg.harnesses.join(", ")
                    }
                ),
            ));
        }
        let invalid = |m: String| Response::new(Status::Invalid, m);
        let oneshot = match crate::profile::HarnessFactory::new(&self.env)
            .ok()
            .and_then(|f| f.profile(&harness).map(|p| p.oneshot.clone()))
        {
            None => return Err(invalid(format!("unknown harness {harness}"))),
            Some(None) => {
                return Err(invalid(format!(
                    "{harness} has no one-shot mode in its profile, so it cannot run a task without a terminal"
                )))
            }
            Some(Some(o)) => o,
        };
        let asked = asked_pick(req).map_err(invalid)?;
        if sp.running() as u32 >= sp.cfg.max_running {
            return Err(Response::new(
                Status::Failed,
                format!(
                    "{} children run already; [spawn] max_running is {}. Wait for one with `paddock-host wait`",
                    sp.running(),
                    sp.cfg.max_running
                ),
            ));
        }
        // A role is a fixed set of limits; the agent only names it.
        let role: crate::spawn::Role = arg_str(req, "role")
            .unwrap_or("implementer")
            .parse()
            .map_err(|e: anyhow::Error| invalid(format!("{e:#}")))?;
        let num = |k: &str| req.args.get(k).and_then(|v| v.as_u64());
        let limits = sp
            .cfg
            .limits(
                num("timeout"),
                num("mem"),
                num("cpu_percent").map(|c| c.min(u32::MAX as u64) as u32),
            )
            .map_err(|e| invalid(format!("{e:#}")))?;
        let scope = sp.scope_tool();
        if scope.is_none() && (limits.mem.is_some() || limits.cpu_percent.is_some()) {
            return Err(Response::new(
                Status::Failed,
                "paddock needs `systemd-run --user` to limit memory and CPUs, and it does not work on this host",
            ));
        }
        let from_head = req
            .args
            .get("from_head")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        // `--task`: paddock writes the prompt from the Spec Kit files.
        let (prompt, task, chosen) = match arg_str(req, "task") {
            Some(want) => {
                let (spot, line, id, hints) = self.task_preview(req, want)?;
                let hint = hint_pick(&id, &hints).map_err(invalid)?;
                let chosen =
                    crate::spawn::model::choose(&sp.cfg, &harness, &oneshot, &asked, &hint)
                        .map_err(|e| invalid(format!("{e:#}")))?;
                // A hint that counts holds for this task only: the worker
                // claims the task on the card, not the next one at that time.
                let used = (hint.model.is_some() && asked.model.is_none())
                    || (hint.effort.is_some() && asked.effort.is_none());
                let claim = if used { id } else { want.to_string() };
                (line.into_bytes(), Some((spot, claim)), chosen)
            }
            None => (
                read_prompt(&self.ctx.project, arg_str(req, "prompt_file").unwrap_or(""))
                    .map_err(|e| invalid(format!("{e:#}")))?,
                None,
                crate::spawn::model::choose(
                    &sp.cfg,
                    &harness,
                    &oneshot,
                    &asked,
                    &Default::default(),
                )
                .map_err(|e| invalid(format!("{e:#}")))?,
            ),
        };
        let text = String::from_utf8_lossy(&prompt);
        let chars = text.chars().count();
        let shown: String = text.chars().take(crate::spawn::PROMPT_SHOWN).collect();
        let c = &sp.ceiling;
        let detail = format!(
            "Start {harness} in a new sandbox with this task. It runs once and stops.\n\
             Role: {}\n\
             {}\n\
             Login: {} (the {harness} login of this computer; this sandbox never sees it)\n\
             Branch: a new worktree of {} from {}\n\
             Limits: network {}, backend {}, no display, {} (enforced by {}), at most {} at a time\n\
             Results go to {}/{INBOX}.\n\n\
             Task ({chars} characters{}):\n{shown}",
            role.describe(),
            chosen.card,
            c.auth,
            c.repo.display(),
            if from_head {
                "the files of this sandbox as they are now, uncommitted changes included"
            } else {
                "the current commit of this sandbox"
            },
            format!("{:?}", c.network).to_lowercase(),
            c.backend,
            limits.describe(),
            if scope.is_some() {
                "systemd-run"
            } else {
                "paddock"
            },
            sp.cfg.max_running,
            self.ctx.project.display(),
            if chars > crate::spawn::PROMPT_SHOWN {
                format!(", the first {} shown", crate::spawn::PROMPT_SHOWN)
            } else {
                String::new()
            },
        );
        Ok(Prepared {
            summary: format!("start {harness} as {} with a task", role.name()),
            detail,
            // A session grant covers this harness in this role only.
            keys: vec![format!("spawn\0{harness}\0{}", role.name())],
            session_allowed: true,
            always_ask: false,
            read_only: false,
            audit: Some(format!(
                "start {harness} as {} in a new sandbox (login {}; {}; {}; task {chars} characters, not logged)",
                role.name(),
                c.auth,
                limits.describe(),
                chosen.card.lines().take(2).collect::<Vec<_>>().join(", "),
            )),
            work: Work::Spawn {
                harness,
                prompt,
                opts: crate::spawn::StartOpts {
                    role,
                    limits,
                    from_head,
                    pick: chosen.pick,
                },
                task,
            },
        })
    }

    pub(in crate::broker) fn refuse_reviewer(&self, req: &Request) -> Response {
        let r = Response::new(
            Status::Disabled,
            format!(
                "this sandbox runs as a reviewer, which cannot use {}",
                if req.cap == "git-push" {
                    "git push"
                } else {
                    "forge commands that change something"
                }
            ),
        );
        audit(
            &self.env,
            &self.ctx,
            req,
            &audit_detail(req, &self.ctx.project, None),
            None,
            "refused: reviewer",
            &r,
            None,
        );
        r
    }

    pub(in crate::broker) fn perform_spawn(&self, req: &Request, work: &Work) -> Response {
        let Some(sp) = &self.spawner else {
            return Response::new(Status::Disabled, self.spawn_off_reason());
        };
        let to_json = |s: &crate::spawn::Status| {
            let mut v = serde_json::to_value(s).unwrap_or_default();
            v["short"] = crate::ids::short(&s.sandbox).into();
            v
        };
        // A child by its spawn id, its sandbox id, or a unique prefix.
        let input = arg_str(req, "id").unwrap_or("");
        let children = sp.list();
        let resolved = match children.iter().find(|c| c.sandbox == input) {
            Some(c) => c.id.clone(),
            None => match crate::ids::find(input, &children, |c| c.id.as_str()) {
                Ok(c) => c.map_or(input.to_owned(), |c| c.id.clone()),
                Err(e) => return Response::new(Status::Invalid, e.to_string()),
            },
        };
        let id = resolved.as_str();
        let missing = || Response::new(Status::Invalid, format!("no child {id} of this sandbox"));
        match (work, arg_str(req, "action").unwrap_or("start")) {
            (
                Work::Spawn {
                    harness,
                    prompt,
                    opts,
                    task,
                },
                _,
            ) => match task {
                Some((spot, want)) => self.start_task(sp, harness, opts.clone(), spot, want),
                None => match sp.start(harness, prompt, opts.clone()) {
                    Ok(s) => Response::done(to_json(&s)),
                    Err(e) => Response::new(Status::Failed, format!("{e:#}")),
                },
            },
            (_, "list") => Response::done(serde_json::json!({
                "children": sp.list().iter().map(to_json).collect::<Vec<_>>(),
                "max_running": sp.cfg.max_running,
            })),
            (_, "status") => match sp.status(id) {
                Some(s) => Response::done(to_json(&s)),
                None => missing(),
            },
            (_, "kill") => match sp.status(id) {
                Some(_) => {
                    sp.stop_one(id, crate::spawn::State::Stopped);
                    let s = sp.wait(id, Duration::from_secs(20)).or(sp.status(id));
                    Response::done(s.as_ref().map(to_json).unwrap_or_default())
                }
                None => missing(),
            },
            (_, "wait") => {
                let max = sp.cfg.timeout + 120;
                let t = req
                    .args
                    .get("timeout")
                    .and_then(|t| t.as_u64())
                    .unwrap_or(max)
                    .min(max);
                match sp.wait(id, Duration::from_secs(t)) {
                    Some(s) if s.state.active() => Response {
                        status: Status::Waiting,
                        message: format!("still waiting: {id} still runs after {t} seconds; run the same wait again"),
                        result: to_json(&s),
                    },
                    Some(s) => Response::done(to_json(&s)),
                    None => missing(),
                }
            }
            (_, other) => Response::new(Status::Invalid, format!("unknown spawn action {other}")),
        }
    }
}

/// `--model` and `--effort` of a request. The sandbox wrote them, so they
/// are checked here again.
fn asked_pick(req: &Request) -> Result<crate::spawn::model::Pick, String> {
    let model = arg_str(req, "model").map(str::to_string);
    if let Some(m) = model
        .as_deref()
        .filter(|m| !crate::spawn::model::valid_model(m))
    {
        return Err(format!(
            "'{m}' is no model name: use letters, digits and .-_:/@"
        ));
    }
    let effort = arg_str(req, "effort")
        .map(str::parse)
        .transpose()
        .map_err(|e: anyhow::Error| format!("{e:#}"))?;
    Ok(crate::spawn::model::Pick { model, effort })
}

/// The `[model:NAME]` and `[effort:LEVEL]` hints of task `id`.
fn hint_pick(
    id: &str,
    h: &crate::speckit::tasks::Hints,
) -> Result<crate::spawn::model::Pick, String> {
    let effort = h
        .effort
        .as_deref()
        .map(str::parse)
        .transpose()
        .map_err(|e: anyhow::Error| format!("task {id}: [effort:...]: {e:#}"))?;
    Ok(crate::spawn::model::Pick {
        model: h.model.clone(),
        effort,
    })
}
