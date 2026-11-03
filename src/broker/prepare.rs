//! Work out a request, then run the part this process runs.

use super::check_fetch;
use super::describe::{
    arg_str, argv, describe, grant_key, safe_name, shell_quote, tool_args, tool_line,
};
use super::fetch::{fetch, inbox, plan_fetch, FetchPlan};
use super::{
    forge, git, new_request_id, notify, Broker, Context, Mode, Request, Response, Status, INBOX,
};
use crate::env::HostEnv;
use crate::state;
use anyhow::{bail, Context as _, Result};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Do the approved work on the host.
pub(in crate::broker) fn perform(
    env: &HostEnv,
    ctx: &Context,
    req: &Request,
    work: &Work,
) -> Result<serde_json::Value> {
    match work {
        Work::Spawn { .. } | Work::SpawnControl => bail!("spawn was not prepared"),
        // The executor pushes (Broker::execute), never this process.
        Work::Push(_) => bail!("the push runs in the executor"),
        Work::Forge(_) => bail!("gh runs in the executor"),
        Work::Fetch(f) => return fetch(env, ctx, req, f),
        Work::Borrow(b) => {
            // Check again after the wait, like fetch.
            let asked = PathBuf::from(arg_str(req, "path").unwrap_or(""));
            check_fetch(env, &asked, &b.src.real_path()?, true)?;
            let max = crate::borrow::Cfg::load(env)?.max;
            return crate::borrow::borrow(env, &ctx.id, b, max);
        }
        Work::Return(r) => return r.apply(env, &ctx.id),
        Work::Plain => {}
    }
    // Every other capability runs in the executor (Broker::run_elsewhere).
    let _ = (env, ctx);
    bail!("{} runs in the executor", req.cap)
}

/// The work behind a request, worked out before the user is asked.
pub(in crate::broker) enum Work {
    Plain,
    /// Start a child with this prompt.
    Spawn {
        harness: String,
        prompt: Vec<u8>,
        opts: crate::spawn::StartOpts,
    },
    /// status, wait, kill or list of this sandbox's own children.
    SpawnControl,
    Push(git::PushPlan),
    Forge(forge::Call),
    Fetch(FetchPlan),
    Borrow(crate::borrow::BorrowPlan),
    Return(crate::borrow::ReturnPlan),
}

/// What the user sees and what a session grant covers.
pub(in crate::broker) struct Prepared {
    pub(in crate::broker) summary: String,
    pub(in crate::broker) detail: String,
    /// "Allow for this session" stores these keys. A request is covered when
    /// every key is granted.
    pub(in crate::broker) keys: Vec<String>,
    /// A session grant can cover this request.
    pub(in crate::broker) session_allowed: bool,
    /// Ask even when the configuration says allow.
    pub(in crate::broker) always_ask: bool,
    /// Allowed without asking unless the capability is denied (forge reads).
    pub(in crate::broker) read_only: bool,
    /// What the audit log may keep, when it differs from `audit_detail`.
    pub(in crate::broker) audit: Option<String>,
    pub(in crate::broker) work: Work,
}

pub(in crate::broker) fn prepare(env: &HostEnv, ctx: &Context, req: &Request) -> Result<Prepared> {
    match req.cap.as_str() {
        "git-push" => {
            let plan = git::plan(
                ctx.repo.as_ref(),
                &env.paddock_run().join(&ctx.id),
                &req.args,
            )?;
            let session_allowed = plan.session_allowed();
            Ok(Prepared {
                summary: plan.summary(),
                detail: plan.detail(),
                keys: plan.grant_keys(),
                session_allowed,
                always_ask: !session_allowed,
                read_only: false,
                audit: None,
                work: Work::Push(plan),
            })
        }
        "forge" => {
            let t =
                forge::tool(arg_str(req, "tool").unwrap_or("gh")).context("unknown forge tool")?;
            let remotes = ctx
                .repo
                .as_ref()
                .map(|r| r.remotes.as_slice())
                .unwrap_or_default();
            // tea picks the host by its login: the host's own login for the
            // repository's host, never one that the sandbox names.
            let call = forge::check_with(t, &argv(req), arg_str(req, "branch"), remotes, &|r| {
                forge::tea_login(env, &r.host)
            })?;
            let line = std::iter::once(call.tool.to_string())
                .chain(call.argv.iter().cloned())
                .map(|a| shell_quote(&a))
                .collect::<Vec<_>>()
                .join(" ");
            let write = call.access == forge::Access::Write;
            let mut detail = format!(
                "{} on the host with your {} login:\n{line}",
                if write { "Change" } else { "Read" },
                call.tool
            );
            if let Some(b) = &call.stdin {
                detail.push_str(&format!("\n\nBody (read from stdin):\n{b}"));
            }
            Ok(Prepared {
                summary: format!("{} {} on {}", call.tool, call.command, call.repo),
                detail,
                keys: vec![call.grant_key()],
                session_allowed: true,
                always_ask: false,
                read_only: !write,
                audit: None,
                work: Work::Forge(call),
            })
        }
        "fetch" => {
            let plan = plan_fetch(env, req)?;
            let (real, asked) = (plan.real.display(), arg_str(req, "path").unwrap_or(""));
            let mut detail = format!(
                "Copy the host path {real} into {}/{INBOX}",
                ctx.project.display()
            );
            if Path::new(asked) != plan.real {
                detail.push_str(&format!(" (asked for as {asked})"));
            }
            Ok(Prepared {
                summary: format!("copy {real} into the sandbox"),
                detail,
                // A grant covers the real path the user saw.
                keys: vec![format!("fetch\0{real}")],
                session_allowed: true,
                always_ask: false,
                read_only: false,
                audit: None,
                work: Work::Fetch(plan),
            })
        }
        "borrow" => {
            let asked = PathBuf::from(arg_str(req, "path").unwrap_or(""));
            let plan =
                crate::borrow::plan_borrow(env, &asked, |a, r| check_fetch(env, a, r, true))?;
            let real = plan.real.display().to_string();
            let mut detail = format!(
                "Copy the host folder {real} ({}) into {} in the sandbox. The agent changes the copy, and the original stays as it is. A return asks again and shows every change.",
                crate::storage::human(plan.bytes),
                crate::borrow::MOUNT
            );
            if asked != plan.real {
                detail.push_str(&format!(" (asked for as {})", asked.display()));
            }
            Ok(Prepared {
                summary: format!("borrow {real}"),
                audit: Some(format!("borrow {real} ({} bytes)", plan.bytes)),
                detail,
                keys: vec![format!("borrow\0{real}")],
                session_allowed: true,
                always_ask: false,
                read_only: false,
                work: Work::Borrow(plan),
            })
        }
        "return" => {
            let name = arg_str(req, "name").unwrap_or("");
            let max = crate::borrow::Cfg::load(env)?.max;
            let plan = crate::borrow::plan_return(env, &ctx.id, name, max)?;
            if plan.changes.is_empty() {
                bail!(
                    "the copy {name} has no changes to return{}",
                    if plan.changes.kept.is_empty() {
                        String::new()
                    } else {
                        " (changes of git files are never returned)".into()
                    }
                );
            }
            let orig = plan.manifest.original.display().to_string();
            Ok(Prepared {
                summary: format!("return {name} to {orig} ({})", plan.changes.counts()),
                detail: plan.detail(),
                audit: Some(format!(
                    "return {name} to {orig}: {}, {} kept, {} skipped, {} changed bytes",
                    plan.changes.counts(),
                    plan.changes.kept.len(),
                    plan.changes.skipped.len(),
                    plan.changed_bytes
                )),
                // Each return shows its own changes: never a session grant.
                keys: vec![format!("return\0{orig}")],
                session_allowed: false,
                always_ask: false,
                read_only: false,
                work: Work::Return(plan),
            })
        }
        _ => {
            let (summary, detail) = describe(req, &ctx.project);
            Ok(Prepared {
                summary,
                detail,
                keys: vec![grant_key(req)],
                session_allowed: true,
                always_ask: false,
                read_only: false,
                audit: None,
                work: Work::Plain,
            })
        }
    }
}

impl Broker {
    /// The capabilities that the executor runs from a plan of this process:
    /// gh, the desktop tools, links and notifications.
    pub(in crate::broker) fn run_elsewhere(
        &self,
        req: &Request,
        work: &Work,
    ) -> Result<serde_json::Value> {
        let workdir = || -> Result<PathBuf> {
            let d = self
                .env
                .paddock_run()
                .join(&self.ctx.id)
                .join(format!("req-{}", new_request_id()));
            crate::privfs::dir(&d)?;
            Ok(d)
        };
        match req.cap.as_str() {
            _ if matches!(work, Work::Forge(_)) => {
                let Work::Forge(call) = work else {
                    unreachable!()
                };
                // Found with this process's PATH, as before: the executor
                // runs exactly this program.
                let exe = self
                    .env
                    .which(call.tool)
                    .with_context(|| format!("{} is not installed on the host", call.tool))?;
                let long = call.argv.iter().any(|a| a == "watch" || a == "--watch");
                let dir = workdir()?;
                let plan = forge::ForgeExec {
                    tool: exe.display().to_string(),
                    argv: call.argv.clone(),
                    stdin: call.stdin.clone(),
                    timeout: req
                        .args
                        .get("timeout")
                        .and_then(|t| t.as_u64())
                        .unwrap_or(if long { 1800 } else { 120 })
                        .clamp(1, 3600),
                    workdir: dir.clone(),
                };
                let r = self.execute(req, serde_json::to_value(&plan)?);
                let _ = std::fs::remove_dir_all(&dir);
                r
            }
            "screenshot" => {
                let name = safe_name(
                    arg_str(req, "out").unwrap_or(""),
                    &format!("screenshot-{}.png", state::now()),
                );
                let dir = workdir()?;
                let plan = crate::gatewayd::DesktopExec {
                    workdir: dir.clone(),
                };
                let r = self
                    .execute(req, serde_json::to_value(&plan)?)
                    .and_then(|_| {
                        // The folder is paddock's own; the inbox is written
                        // through the folder that paddock opened.
                        let png =
                            std::fs::read(dir.join("shot.png")).context("grim wrote no image")?;
                        let inbox = inbox(&self.ctx.project)?;
                        let (mut f, name) = inbox.new_file(&name, 0o644)?;
                        f.write_all(&png)?;
                        Ok(serde_json::json!({ "path": inbox.path().join(name) }))
                    });
                let _ = std::fs::remove_dir_all(&dir);
                r
            }
            "clipboard-read" | "clipboard-write" => {
                let dir = workdir()?;
                let plan = crate::gatewayd::DesktopExec {
                    workdir: dir.clone(),
                };
                let r = self.execute(req, serde_json::to_value(&plan)?);
                let _ = std::fs::remove_dir_all(&dir);
                r
            }
            "open-url" => {
                let r = self.execute(req, serde_json::Value::Null)?;
                // A login page: let its localhost callback through for a while.
                crate::login::note_opened(arg_str(req, "url").unwrap_or(""));
                Ok(r)
            }
            "notify" => {
                let n = notify::Note {
                    summary: format!(
                        "{}: {}",
                        self.ctx.harness,
                        arg_str(req, "title").unwrap_or("")
                    ),
                    body: arg_str(req, "body").unwrap_or("").to_string(),
                    ..Default::default()
                };
                self.execute(req, serde_json::to_value(&n)?)
            }
            other => bail!("{other} does not run in the executor"),
        }
    }

    /// exec does not run in this process: it is signed for exactly this
    /// request and run by the executor, which checks the signature first.
    pub(in crate::broker) fn execute(
        &self,
        req: &Request,
        plan: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let remote;
        let dispatch: &dyn crate::gatewayd::Dispatch = match &self.dispatch {
            Some(d) => d.as_ref(),
            None => {
                remote = crate::gatewayd::Remote::new(&self.env)?;
                &remote
            }
        };
        let key = dispatch.signing_key(&self.env)?;
        crate::gatewayd::execute(dispatch, &key, &self.ctx.id, &self.ctx.project, req, plan)
    }

    /// Check a tool request before the user is asked: the tool exists, its
    /// mode is not deny, and the arguments match one of its shapes. Err is
    /// the response to send instead.
    pub(in crate::broker) fn prepare_tool(
        &self,
        req: &Request,
    ) -> std::result::Result<Prepared, Response> {
        let name = arg_str(req, "name").unwrap_or("");
        let Some(tool) = self.policy.tools.get(name) else {
            let known: Vec<&str> = self.policy.tools.keys().map(String::as_str).collect();
            return Err(Response::new(
                Status::Invalid,
                if known.is_empty() {
                    format!("no tool {name}: the user defines none in [host.tools]")
                } else {
                    format!("no tool {name}. Tools: {}", known.join(", "))
                },
            ));
        };
        if tool.mode == Mode::Deny {
            return Err(Response::new(
                Status::Disabled,
                format!("tool {name} is set to deny in the user configuration"),
            ));
        }
        let args = tool_args(req).map_err(|e| Response::new(Status::Invalid, format!("{e:#}")))?;
        tool.check(&args)
            .map_err(|e| Response::new(Status::Invalid, format!("{e:#}")))?;
        let line = tool
            .command
            .iter()
            .chain(args.iter())
            .map(|a| shell_quote(a))
            .collect::<Vec<_>>()
            .join(" ");
        let place = match tool.cwd {
            crate::hosttools::Cwd::Project => self.ctx.project.display().to_string(),
            crate::hosttools::Cwd::Home => "your home folder".into(),
        };
        let mut key = format!("tool\0{name}");
        for a in &args {
            key.push('\0');
            key.push_str(a);
        }
        Ok(Prepared {
            summary: format!(
                "host tool {}",
                tool_line(req).chars().take(80).collect::<String>()
            ),
            detail: format!("Run the host tool {name} in {place}:\n{line}"),
            keys: vec![key],
            session_allowed: true,
            always_ask: false,
            read_only: false,
            audit: None,
            work: Work::Plain,
        })
    }
}
