//! The host passthrough. A sandbox cannot touch the host, but its user
//! sometimes needs it to: a screenshot of the real screen, one command on the
//! host, one file from outside the project. The sandbox asks over a socket,
//! the user approves on the host, and paddock does the work on the host. The
//! boundary itself never opens.
//!
//! The policy comes only from the user configuration (`[host]` in
//! `$XDG_CONFIG_HOME/paddock/config.toml`). The project `paddock.toml` cannot
//! change it, because the agent can edit that file.

use crate::env::HostEnv;
use crate::state;
use anyhow::{bail, Context as _, Result};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

mod approval;
mod audit;
mod capture;
mod compacted;
mod describe;
mod fetch;
pub mod forge;
pub mod git;
pub mod notify;
mod policy;
mod prepare;
mod protocol;
mod spawn;
mod spec;

pub use approval::{
    answer, answer_reply, answer_with_note, approval_note, approval_ui_active, approval_ui_file,
    approvals_dir, ask_choice, clean_note, clean_text, denied_message, pending, write_pending,
    Answer, Approver, DenyAll, HostApprover, Pending, Verdict, ASK_CAPABILITY, BUDGET_CAPABILITY,
    MAX_NOTE, MAX_REPLY, TIMEOUT_CAPABILITY,
};
pub use audit::{audit_detail, audit_file, audit_line};
pub use capture::{run_capture, run_capture_input};
pub use compacted::RunFacts;
pub use policy::{HostPolicy, Mode};
pub use protocol::{Request, Response, Status};

pub(crate) use capture::{Pipe, PIPE_GRACE};
pub(crate) use describe::{safe_name, tool_args};
pub(crate) use fetch::{check_fetch, inbox, inbox_for};
pub(crate) use protocol::new_request_id;

pub const PROTOCOL: u32 = 1;
/// Where the socket appears inside the sandbox.
pub const SOCKET_IN_SANDBOX: &str = "/run/paddock/host.sock";
/// Results land here, inside the project.
pub const INBOX: &str = ".paddock/inbox";

const MAX_REQUEST: usize = 64 * 1024;

pub const CAPABILITIES: &[(&str, &str)] = &[
    (
        "screenshot",
        "capture the host screen, one output, or the active window into the project inbox",
    ),
    (
        "exec",
        "run one command on the host, in the project directory, and return its output",
    ),
    (
        "tool",
        "run a named host tool that the user defined in [host.tools], with allowed arguments only",
    ),
    (
        "fetch",
        "copy one host file or directory into the project inbox",
    ),
    (
        "borrow",
        "copy a host folder into /run/paddock/borrow to change it, and keep the original as it is",
    ),
    (
        "return",
        "write the changes of a borrowed copy back to its host folder, if the folder did not change",
    ),
    ("clipboard-read", "read the host clipboard"),
    ("clipboard-write", "set the host clipboard"),
    ("open-url", "open a URL in the host browser"),
    ("notify", "show a desktop notification on the host"),
    (
        "git-push",
        "push commits to a remote of the project with the user's own credentials",
    ),
    (
        "forge",
        "run allowed gh commands (pull requests, issues, CI runs) on the project's repository",
    ),
    (
        "spawn",
        "start another harness in its own sandbox and worktree with one task, and get its results",
    ),
];

/// The sandbox this broker serves.
#[derive(Debug, Clone)]
pub struct Context {
    pub id: String,
    pub harness: String,
    pub project: PathBuf,
    /// The project repository as it was when the sandbox started.
    pub repo: Option<git::Repo>,
    /// For a sandbox that another sandbox started: the sandboxes above it,
    /// root first. Empty when the user started it.
    pub chain: Vec<String>,
    /// 0 when the user started the sandbox, 1 for its child, and so on.
    pub depth: u32,
    /// The role of a child. A reviewer cannot push or change anything on
    /// the forge. None when the user started the sandbox.
    pub role: Option<crate::spawn::Role>,
}

pub struct Broker {
    pub env: HostEnv,
    pub ctx: Context,
    pub policy: HostPolicy,
    pub approver: Arc<dyn Approver>,
    /// This sandbox's children. None: spawn is not available here.
    pub spawner: Option<Arc<crate::spawn::Spawner>>,
    /// Where approved work runs that this process does not run itself: the
    /// executor (`paddock gatewayd`). The tests run it in this process.
    pub dispatch: Option<Arc<dyn crate::gatewayd::Dispatch>>,
    /// The docker and aws credential channels of the run, set when the
    /// user allowed them on the start card.
    pub channels: std::sync::OnceLock<Arc<crate::credentials::channels::Channels>>,
    /// A detached run (`paddock run --detach`): `paddock-host done` ends it.
    pub detach: Option<Arc<crate::detach::Ctl>>,
    /// What the note after a context compaction says (compaction.rs).
    pub run: compacted::RunFacts,
    /// The kv service of this sandbox, for the compaction record. Set once
    /// the store runs; it starts after the broker.
    pub kv: std::sync::OnceLock<Arc<crate::kv::Service>>,
    grants: Mutex<Vec<String>>,
    /// "Deny for this session": the keys of the request and the user's note.
    denials: Mutex<Vec<(String, Option<String>)>>,
}

impl Broker {
    pub fn new(
        env: HostEnv,
        ctx: Context,
        policy: HostPolicy,
        approver: Arc<dyn Approver>,
    ) -> Self {
        Self {
            env,
            ctx,
            policy,
            approver,
            spawner: None,
            dispatch: None,
            channels: std::sync::OnceLock::new(),
            detach: None,
            run: Default::default(),
            kv: std::sync::OnceLock::new(),
            grants: Mutex::new(Vec::new()),
            denials: Mutex::new(Vec::new()),
        }
    }

    /// The mode of a capability for this sandbox. Spawn is off when this
    /// sandbox has no spawner, when it is as deep as `[spawn] max_depth`
    /// allows, or when `[spawn] harnesses` names no harness. The depth comes
    /// from the host, never from the sandbox.
    pub fn mode(&self, cap: &str) -> Mode {
        let m = self.policy.mode(cap);
        if cap != "spawn" {
            return m;
        }
        match &self.spawner {
            Some(s) if s.cfg.depth_allows(self.ctx.depth) && !s.cfg.harnesses.is_empty() => m,
            _ => Mode::Deny,
        }
    }

    /// The capabilities and their current modes, for `paddock-host capabilities`.
    pub fn capabilities(&self) -> serde_json::Value {
        let grants = self.grants.lock().unwrap();
        let caps: Vec<serde_json::Value> = CAPABILITIES
            .iter()
            .map(|(c, desc)| {
                serde_json::json!({
                    "name": c,
                    "mode": self.mode(c),
                    "session_granted": grants.iter().any(|g| g == c || g.starts_with(&format!("{c}\0"))),
                    "description": desc,
                })
            })
            .collect();
        let mut v = serde_json::json!({ "protocol": PROTOCOL, "wait_seconds": self.policy.wait, "capabilities": caps });
        if let Some(s) = &self.spawner {
            v["spawn"] = serde_json::json!({
                "harnesses": s.cfg.harnesses,
                "max_running": s.cfg.max_running,
                "running": s.running(),
                "max_depth": s.cfg.max_depth,
                "depth": self.ctx.depth,
                "timeout_seconds": s.cfg.timeout,
                "max_mem": s.cfg.max_mem.map(crate::spawn::format_bytes),
                "max_cpus": s.cfg.max_cpu_percent.map(crate::spawn::format_cpus),
                "roles": ["implementer", "reviewer"],
            });
        }
        if let Some(p) = self.ctx.chain.last() {
            v["started_by"] = p.clone().into();
        }
        v["tools"] = self
            .policy
            .tools
            .values()
            .map(crate::hosttools::Tool::listing)
            .collect();
        v
    }

    /// `paddock-host done [--status ok|failed] [TEXT]`.
    fn done(&self, req: &Request) -> Response {
        if req.v != PROTOCOL {
            return Response::new(Status::Invalid, "unsupported protocol version");
        }
        let Some(ctl) = &self.detach else {
            // A worker of a Spec Kit task reports into its task record.
            if let Some(r) = self.report_task(req) {
                return r;
            }
            return Response::new(
                Status::Disabled,
                "this sandbox is not a detached run (paddock run --detach), so there is nothing to end: finish your answer, and the user ends the run",
            );
        };
        let status = match describe::arg_str(req, "status").unwrap_or("ok").parse() {
            Ok(s) => s,
            Err(e) => return Response::new(Status::Invalid, format!("{e:#}")),
        };
        let text = describe::arg_str(req, "text").unwrap_or("");
        log::info!("sandbox {}: the agent says the task is done", self.ctx.id);
        match ctl.done(status, text) {
            Ok(msg) => Response::done(serde_json::json!({ "message": msg })),
            Err(e) => Response::new(Status::Failed, format!("{e:#}")),
        }
    }

    pub fn handle(&self, req: &Request) -> Response {
        if req.cap == "capabilities" {
            return self.handle_capabilities();
        }
        // A docker or aws credential: the channel checks, asks and audits.
        if req.cap == crate::credentials::channels::CAPABILITY {
            return self.handle_channels(req);
        }
        // The end of the task in a detached run: never asked, it only ends
        // this sandbox.
        if req.cap == "done" {
            return self.done(req);
        }
        // A harness hook after a context compaction: never asked, it only
        // records the event and returns facts about this run.
        if req.cap == "compacted" {
            return self.compacted(req);
        }
        // Spec Kit tasks: a read, or a review of this sandbox's own workers.
        if req.cap == "spec" {
            return self.spec(req);
        }
        // The sandbox reads its own boundary log: a read, never asked.
        if req.cap == "boundary" {
            return self.handle_boundary();
        }
        self.handle_gated(req)
    }

    fn handle_capabilities(&self) -> Response {
        Response::done(self.capabilities())
    }

    /// A docker or aws credential: the channel checks, asks and audits.
    fn handle_channels(&self, req: &Request) -> Response {
        if req.v != PROTOCOL {
            return Response::new(Status::Invalid, "unsupported protocol version");
        }
        match self.channels.get() {
            Some(c) => match c.handle(&req.args) {
                Ok(v) => Response::done(v),
                Err((s, m)) => Response::new(s, m),
            },
            None => Response::new(
                Status::Disabled,
                "this sandbox has no docker or aws credential: the user registers them in [credentials] and allows them when the sandbox starts",
            ),
        }
    }

    /// The sandbox reads its own boundary log: a read, never asked.
    fn handle_boundary(&self) -> Response {
        let lines = crate::boundary::read(&self.env, &self.ctx.id);
        Response::done(crate::boundary::to_json(&lines))
    }

    /// Every other capability: the same checks, in the same order, then the
    /// work for that capability.
    fn handle_gated(&self, req: &Request) -> Response {
        let invalid = |e: anyhow::Error| {
            let r = Response::new(Status::Invalid, format!("{e:#}"));
            log::info!(
                "sandbox {}: invalid {} request: {e:#}",
                self.ctx.id,
                req.cap
            );
            audit::audit(
                &self.env,
                &self.ctx,
                req,
                &audit_detail(req, &self.ctx.project, None),
                None,
                "invalid",
                &r,
                None,
            );
            r
        };
        if let Err(e) = describe::validate(req) {
            return invalid(e);
        }
        // A reviewer reads and reports. It never pushes and never changes
        // anything on the forge, whatever the configuration allows.
        let reviewer = self.ctx.role == Some(crate::spawn::Role::Reviewer);
        if reviewer && req.cap == "git-push" {
            return self.refuse_reviewer(req);
        }
        let mode = self.mode(&req.cap);
        if mode == Mode::Deny {
            let r = Response::new(
                Status::Disabled,
                if req.cap == "spawn" {
                    self.spawn_off_reason()
                } else {
                    format!(
                        "{} is set to deny in [host] of the user configuration",
                        req.cap
                    )
                },
            );
            audit::audit(
                &self.env,
                &self.ctx,
                req,
                &audit_detail(req, &self.ctx.project, None),
                None,
                "disabled",
                &r,
                None,
            );
            return r;
        }
        let prep = if req.cap == "spawn" {
            match self.prepare_spawn(req) {
                Ok(p) => p,
                Err(r) => {
                    audit::audit(
                        &self.env,
                        &self.ctx,
                        req,
                        &audit_detail(req, &self.ctx.project, None),
                        None,
                        "refused",
                        &r,
                        None,
                    );
                    return r;
                }
            }
        } else if req.cap == "tool" {
            match self.prepare_tool(req) {
                Ok(p) => p,
                Err(r) => {
                    let decision = if r.status == Status::Disabled {
                        "disabled"
                    } else {
                        "invalid"
                    };
                    audit::audit(
                        &self.env,
                        &self.ctx,
                        req,
                        &audit_detail(req, &self.ctx.project, None),
                        None,
                        decision,
                        &r,
                        None,
                    );
                    return r;
                }
            }
        } else {
            match prepare::prepare(&self.env, &self.ctx, req) {
                Ok(p) => p,
                Err(e) => return invalid(e),
            }
        };
        // A tool has its own mode; `[host] tool` only turns every tool off.
        let mode = match req.cap.as_str() {
            "tool" => describe::arg_str(req, "name")
                .and_then(|n| self.policy.tools.get(n))
                .map_or(Mode::Deny, |t| t.mode),
            _ => mode,
        };
        if reviewer && req.cap == "forge" && !prep.read_only {
            return self.refuse_reviewer(req);
        }
        let granted = || {
            let g = self.grants.lock().unwrap();
            prep.session_allowed && prep.keys.iter().all(|k| g.contains(k))
        };
        // The same keys as a session grant. A request without keys is
        // never the same request.
        let session_denied = || -> Option<Option<String>> {
            if !prep.session_allowed || prep.keys.is_empty() {
                return None;
            }
            let d = self.denials.lock().unwrap();
            let note = |k: &String| d.iter().find(|(x, _)| x == k).map(|(_, n)| n.clone());
            prep.keys
                .iter()
                .map(note)
                .collect::<Option<Vec<_>>>()
                .map(|n| n.into_iter().flatten().next())
        };
        let mut request_id: Option<String> = None;
        let mut note: Option<String> = None;
        let (decision, allowed) = if mode == Mode::Allow && !prep.always_ask {
            ("allowed by configuration", Some(true))
        } else if prep.read_only {
            ("allowed: read-only", Some(true))
        } else if let Some(n) = session_denied() {
            note = n;
            ("denied for this session", Some(false))
        } else if granted() {
            ("allowed for this session", Some(true))
        } else {
            request_id = Some(new_request_id());
            let p = Pending {
                id: request_id.clone().unwrap_or_default(),
                sandbox: self.ctx.id.clone(),
                harness: self.ctx.harness.clone(),
                project: self.ctx.project.clone(),
                capability: req.cap.clone(),
                summary: prep.summary.clone(),
                detail: prep.detail.clone(),
                reason: req.reason.chars().take(500).collect(),
                created: state::now(),
                expires: state::now() + self.policy.wait,
                session_allowed: prep.session_allowed,
                choices: Vec::new(),
                started_by: self.ctx.chain.clone(),
            };
            let verdict =
                self.approver
                    .ask_verdict(&self.env, &p, Duration::from_secs(self.policy.wait));
            note = verdict.as_ref().and_then(|v| v.note.clone());
            match verdict.map(|v| (v.answer, v.who)) {
                Some((Answer::Once, "agent")) => ("allowed once by the agent approver", Some(true)),
                Some((Answer::Once, _)) => ("allowed once", Some(true)),
                // A grant this request cannot have counts as once.
                Some((Answer::Session, _)) if !prep.session_allowed => (
                    "allowed once (no session grant for this request)",
                    Some(true),
                ),
                Some((Answer::Session, who)) => {
                    self.grants
                        .lock()
                        .unwrap()
                        .extend(prep.keys.iter().cloned());
                    if who == "agent" {
                        ("allowed for this session by the agent approver", Some(true))
                    } else {
                        ("allowed for this session", Some(true))
                    }
                }
                Some((Answer::Deny, "agent")) => ("denied by the agent approver", Some(false)),
                Some((Answer::Deny, _)) => ("denied", Some(false)),
                // A request without a session answer: a plain denial.
                Some((Answer::DenySession, _)) if !prep.session_allowed || prep.keys.is_empty() => {
                    ("denied", Some(false))
                }
                Some((Answer::DenySession, who)) => {
                    let mut d = self.denials.lock().unwrap();
                    for k in &prep.keys {
                        d.retain(|(x, _)| x != k);
                        d.push((k.clone(), note.clone()));
                    }
                    if who == "agent" {
                        ("denied for this session by the agent approver", Some(false))
                    } else {
                        ("denied for this session", Some(false))
                    }
                }
                None => ("timed out", None),
            }
        };
        let resp = match (decision, allowed) {
            ("timed out", _) => Response::new(
                Status::Timeout,
                format!("nobody answered within {} seconds", self.policy.wait),
            ),
            (_, Some(false)) => Response::new(
                Status::Denied,
                denied_message("the user denied the request", note.as_deref()),
            ),
            _ if matches!(
                prep.work,
                prepare::Work::Spawn { .. } | prepare::Work::SpawnControl
            ) =>
            {
                self.perform_spawn(req, &prep.work)
            }
            _ if matches!(
                req.cap.as_str(),
                "screenshot" | "clipboard-read" | "clipboard-write" | "open-url" | "notify"
            ) || matches!(prep.work, prepare::Work::Forge(_)) =>
            {
                self.respond_remote(req, &prep.work)
            }
            _ if req.cap == "exec" || req.cap == "tool" => self.respond_exec(req),
            // The push runs in the executor too, from the plan of the card.
            _ if matches!(prep.work, prepare::Work::Push(_)) => self.respond_push(req, &prep.work),
            _ => self.respond_local(req, &prep.work),
        };
        // Status and wait calls are reads of this sandbox's own children.
        if matches!(prep.work, prepare::Work::SpawnControl)
            && describe::arg_str(req, "action").is_some_and(|a| a != "kill")
        {
            return resp;
        }
        let safe = match &prep.audit {
            Some(a) => Some(a.as_str()),
            None => {
                matches!(req.cap.as_str(), "fetch" | "git-push").then_some(prep.detail.as_str())
            }
        };
        audit::audit(
            &self.env,
            &self.ctx,
            req,
            &audit_detail(req, &self.ctx.project, safe),
            request_id.as_deref(),
            decision,
            &resp,
            note.as_deref(),
        );
        log::info!(
            "sandbox {}: {} request {}: {:?}",
            self.ctx.id,
            req.cap,
            decision,
            resp.status
        );
        if matches!(resp.status, Status::Failed) {
            log::warn!(
                "sandbox {}: {} failed: {}",
                self.ctx.id,
                req.cap,
                resp.message
            );
        }
        resp
    }

    /// Screenshot, clipboard, open-url, notify and forge run in the executor.
    fn respond_remote(&self, req: &Request, work: &prepare::Work) -> Response {
        match self.run_elsewhere(req, work) {
            Ok(result) => Response::done(result),
            Err(e) => Response::new(Status::Failed, format!("{e:#}")),
        }
    }

    /// `exec` and `tool` run in the executor, with no plan of their own.
    fn respond_exec(&self, req: &Request) -> Response {
        match self.execute(req, serde_json::Value::Null) {
            Ok(result) => Response::done(result),
            Err(e) => Response::new(Status::Failed, format!("{e:#}")),
        }
    }

    /// The push runs in the executor, from the plan of the card.
    fn respond_push(&self, req: &Request, work: &prepare::Work) -> Response {
        let prepare::Work::Push(plan) = work else {
            unreachable!()
        };
        match serde_json::to_value(plan.exec())
            .map_err(anyhow::Error::from)
            .and_then(|p| self.execute(req, p))
        {
            Ok(result) => Response::done(result),
            Err(e) => Response::new(Status::Failed, format!("{e:#}")),
        }
    }

    /// Fetch, borrow, return and the other work this process runs itself.
    fn respond_local(&self, req: &Request, work: &prepare::Work) -> Response {
        match prepare::perform(&self.env, &self.ctx, req, work) {
            Ok(result) => Response::done(result),
            Err(e) => Response::new(Status::Failed, format!("{e:#}")),
        }
    }

    fn serve_conn(&self, conn: UnixStream) -> Result<()> {
        let mut reader = BufReader::new(conn.try_clone()?.take(MAX_REQUEST as u64));
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let resp = match serde_json::from_str::<Request>(line.trim()) {
            Ok(req) => self.handle(&req),
            Err(e) => Response::new(Status::Invalid, format!("bad request: {e}")),
        };
        let mut w = conn;
        writeln!(w, "{}", serde_json::to_string(&resp)?)?;
        Ok(())
    }
}

/// Read a prompt file that the sandbox names. The project is
/// sandbox-writable: the path must stay inside it, no symlink is followed, a
/// FIFO or a device is never opened, and the size is limited.
pub fn read_prompt(project: &Path, raw: &str) -> Result<Vec<u8>> {
    let p = Path::new(raw);
    let rel = if p.is_absolute() {
        p.strip_prefix(project)
            .with_context(|| {
                format!(
                    "the prompt file must be inside the project {}",
                    project.display()
                )
            })?
            .to_path_buf()
    } else {
        p.to_path_buf()
    };
    let root = crate::safefs::Root::open(project)?;
    let bytes = root
        .read(&rel, crate::spawn::MAX_PROMPT)
        .with_context(|| format!("cannot read the prompt file {raw}"))?
        .with_context(|| format!("the prompt file {raw} does not exist"))?;
    if bytes.iter().all(u8::is_ascii_whitespace) {
        bail!("the prompt file {raw} is empty");
    }
    Ok(bytes)
}

/// Serve requests on `socket` until the process exits.
pub fn serve(socket: &Path, broker: Arc<Broker>) -> Result<()> {
    let _ = std::fs::remove_file(socket);
    if let Some(d) = socket.parent() {
        crate::privfs::dir(d)?;
    }
    let listener = UnixListener::bind(socket)
        .with_context(|| format!("cannot listen on {}", socket.display()))?;
    log::debug!("serving host requests for sandbox {}", broker.ctx.id);
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let b = broker.clone();
            std::thread::spawn(move || {
                if let Err(e) = b.serve_conn(conn) {
                    log::debug!("connection failed: {e:#}");
                }
            });
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests;
