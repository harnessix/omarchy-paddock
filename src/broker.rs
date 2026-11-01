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
use crate::policy;
use crate::state;
use anyhow::{bail, Context as _, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const PROTOCOL: u32 = 1;
/// Where the socket appears inside the sandbox.
pub const SOCKET_IN_SANDBOX: &str = "/run/paddock/host.sock";
/// Results land here, inside the project.
pub const INBOX: &str = ".paddock/inbox";

const MAX_REQUEST: usize = 64 * 1024;
const MAX_FETCH: u64 = 64 * 1024 * 1024;
const MAX_OUTPUT: usize = 1024 * 1024;

pub mod forge;
pub mod git;
pub mod notify;

/// The capabilities, in the order `paddock-host capabilities` lists them.
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
        "fetch",
        "copy one host file or directory into the project inbox",
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Deny,
    Ask,
    Allow,
}

/// Per-capability modes. `exec` can never be `allow`: that would remove the
/// sandbox, so it is read as `ask`.
#[derive(Debug, Clone, Serialize)]
pub struct HostPolicy {
    pub modes: BTreeMap<String, Mode>,
    /// Seconds to wait for an answer before a request times out.
    pub wait: u64,
}

impl Default for HostPolicy {
    fn default() -> Self {
        let mut modes = BTreeMap::new();
        for (cap, _) in CAPABILITIES {
            modes.insert(cap.to_string(), Mode::Ask);
        }
        modes.insert("notify".into(), Mode::Allow);
        Self { modes, wait: 120 }
    }
}

impl HostPolicy {
    pub fn load(env: &HostEnv) -> Result<Self> {
        let mut p = Self::default();
        let file = env.paddock_config().join("config.toml");
        let Ok(text) = std::fs::read_to_string(&file) else {
            return Ok(p);
        };
        let doc: toml::Value =
            toml::from_str(&text).with_context(|| format!("{} is invalid", file.display()))?;
        let Some(host) = doc.get("host").and_then(|h| h.as_table()) else {
            return Ok(p);
        };
        for (k, v) in host {
            if k == "wait" {
                p.wait = v
                    .as_integer()
                    .filter(|n| (5..=3600).contains(n))
                    .context("[host] wait must be 5 to 3600 seconds")?
                    as u64;
                continue;
            }
            if !CAPABILITIES.iter().any(|(c, _)| c == k) {
                bail!(
                    "[host] {k}: unknown capability. Known: {}",
                    CAPABILITIES
                        .iter()
                        .map(|c| c.0)
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            let mode: Mode = v
                .clone()
                .try_into()
                .with_context(|| format!("[host] {k} must be deny, ask or allow"))?;
            p.modes.insert(k.clone(), mode);
        }
        if p.modes.get("exec") == Some(&Mode::Allow) {
            p.modes.insert("exec".into(), Mode::Ask);
        }
        Ok(p)
    }

    pub fn mode(&self, cap: &str) -> Mode {
        self.modes.get(cap).copied().unwrap_or(Mode::Deny)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub v: u32,
    pub cap: String,
    #[serde(default)]
    pub args: BTreeMap<String, serde_json::Value>,
    /// Why the agent asks. Shown to the user next to the request.
    #[serde(default)]
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Done,
    Denied,
    Disabled,
    Timeout,
    Failed,
    Invalid,
    /// A blocking read reached its time limit before the event, for example
    /// `paddock-host wait` on a child that still runs. Run it again.
    Waiting,
}

impl Status {
    /// The exit code of `paddock-host` for this status.
    pub fn exit_code(&self) -> i32 {
        match self {
            Status::Done => 0,
            Status::Denied => 3,
            Status::Disabled => 4,
            Status::Timeout => 5,
            Status::Waiting => crate::waiting::EXIT_STILL_WAITING,
            Status::Failed => 7,
            Status::Invalid => 8,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub status: Status,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub result: serde_json::Value,
}

impl Response {
    fn new(status: Status, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            result: serde_json::Value::Null,
        }
    }
    fn done(result: serde_json::Value) -> Self {
        Self {
            status: Status::Done,
            message: String::new(),
            result,
        }
    }
}

/// A request that waits for the user, as `paddock approvals --json` shows it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pending {
    pub id: String,
    pub sandbox: String,
    pub harness: String,
    pub project: PathBuf,
    pub capability: String,
    /// One line for a list.
    pub summary: String,
    /// The exact thing that will happen, for example the full command line.
    pub detail: String,
    pub reason: String,
    pub created: u64,
    /// Unix time when the request stops waiting and times out.
    #[serde(default)]
    pub expires: u64,
    /// Whether "allow for this session" is offered.
    pub session_allowed: bool,
    /// For a request from a sandbox that another sandbox started: the
    /// sandboxes above it, the first one started by the user, the parent last.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub started_by: Vec<String>,
    /// For a question (`/dev/paddock/ask`): short answers that the card
    /// offers as buttons. The user can always type another answer. For a
    /// card of paddock itself (`spawn-timeout`): the only answers, besides
    /// deny.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<String>,
}

/// The capability name of a question from `/dev/paddock/ask` on the
/// approval card. It is not a host capability: the answer is text for the
/// agent, and nothing runs on the host.
pub const ASK_CAPABILITY: &str = "ask";
/// The capability name of the card for a spawned child at its time limit
/// (`[spawn] on_timeout = "ask"`). The answer is one of its choices (more
/// time), or deny (stop).
pub const TIMEOUT_CAPABILITY: &str = "spawn-timeout";
/// The longest typed answer to a question, in characters.
pub const MAX_REPLY: usize = 4000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Answer {
    Once,
    Session,
    Deny,
    /// Deny this request and every same request of this sandbox for the
    /// rest of its session, without a new card.
    #[serde(rename = "deny-session")]
    DenySession,
}

impl Answer {
    pub fn denies(self) -> bool {
        matches!(self, Self::Deny | Self::DenySession)
    }
}

/// The longest note for the agent on a denial, in characters.
pub const MAX_NOTE: usize = 300;

/// The note of a denial, made safe: one line, no control or invisible
/// characters, at most `MAX_NOTE` characters. None when nothing is left.
pub fn clean_note(s: &str) -> Option<String> {
    let line: String = s
        .chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .collect();
    let t: String = clean_text(&line).trim().chars().take(MAX_NOTE).collect();
    let t = t.trim_end().to_string();
    (!t.is_empty()).then_some(t)
}

/// The message of a denial for the agent: the user's note when there is
/// one, else `plain`.
pub fn denied_message(plain: &str, note: Option<&str>) -> String {
    match note {
        Some(n) => format!("the user denied this request: \"{n}\""),
        None => plain.to_string(),
    }
}

/// An answer with the user's note on a denial, and who gave it ("user" or
/// "agent").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub answer: Answer,
    pub note: Option<String>,
    pub who: &'static str,
}

/// The answer file: a plain answer (`"deny"`), or an answer with a note.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum AnswerFile {
    Plain(Answer),
    Noted {
        answer: Answer,
        #[serde(default)]
        note: String,
    },
}

impl AnswerFile {
    fn parse(text: &str) -> Option<(Answer, Option<String>)> {
        match serde_json::from_str::<AnswerFile>(text).ok()? {
            AnswerFile::Plain(a) => Some((a, None)),
            AnswerFile::Noted { answer, note } => {
                Some((answer, clean_note(&note).filter(|_| answer.denies())))
            }
        }
    }
}

impl std::str::FromStr for Answer {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "once" => Ok(Self::Once),
            "session" => Ok(Self::Session),
            "deny" => Ok(Self::Deny),
            "deny-session" => Ok(Self::DenySession),
            _ => bail!("answer must be once, session, deny or deny-session"),
        }
    }
}

pub fn approvals_dir(env: &HostEnv) -> PathBuf {
    env.paddock_run().join("approvals")
}

/// Every request that waits for an answer, from every running sandbox.
pub fn pending(env: &HostEnv) -> Vec<Pending> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(approvals_dir(env)) else {
        return out;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().is_some_and(|x| x == "json") {
            if let Some(req) = std::fs::read_to_string(&p)
                .ok()
                .and_then(|t| serde_json::from_str::<Pending>(&t).ok())
            {
                out.push(req);
            }
        }
    }
    // A question of a sandbox that no longer runs: its paddock process was
    // killed before it could withdraw the question. Older than a minute, so
    // a question that is being put on the card is never removed.
    let now = state::now();
    if out
        .iter()
        .any(|p| p.capability == ASK_CAPABILITY && now.saturating_sub(p.created) > 60)
    {
        let running: Vec<String> = state::Running::list(env)
            .into_iter()
            .map(|r| r.id)
            .collect();
        out.retain(|p| {
            let gone = p.capability == ASK_CAPABILITY
                && now.saturating_sub(p.created) > 60
                && !running.contains(&p.sandbox);
            if gone {
                let dir = approvals_dir(env);
                let _ = std::fs::remove_file(dir.join(format!("{}.json", p.id)));
                let _ = std::fs::remove_file(dir.join(format!("{}.answer", p.id)));
            }
            !gone
        });
    }
    // Host requests first: questions of agents never push them off the card.
    out.sort_by_key(|p| (p.capability == ASK_CAPABILITY, p.created));
    out
}

/// Record the user's answer. Only the host can call this: the approvals
/// directory is not mounted in any sandbox.
pub fn answer(env: &HostEnv, id: &str, a: Answer) -> Result<()> {
    answer_with_note(env, id, a, None)
}

/// Record the user's answer with a note for the agent. Only a denial
/// carries a note.
pub fn answer_with_note(env: &HostEnv, id: &str, a: Answer, note: Option<&str>) -> Result<()> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        bail!("invalid request id");
    }
    let dir = approvals_dir(env);
    let pending = dir.join(format!("{id}.json"));
    if !pending.is_file() {
        bail!("no pending request {id}");
    }
    let p: Pending = serde_json::from_str(&std::fs::read_to_string(&pending)?)?;
    if p.capability == ASK_CAPABILITY && a != Answer::Deny {
        bail!("{id} is a question: answer it with --text TEXT, or dismiss it with deny");
    }
    if p.capability == ASK_CAPABILITY && a == Answer::DenySession {
        bail!("{id} is a question: dismiss it with deny");
    }
    if p.capability != ASK_CAPABILITY && !p.choices.is_empty() && a != Answer::Deny {
        bail!(
            "{id} offers choices: answer it with --text {}, or deny",
            p.choices.join("|")
        );
    }
    if a == Answer::Session && !p.session_allowed {
        bail!("this request can only be allowed once");
    }
    if a == Answer::DenySession && !p.session_allowed {
        bail!("this request has no session answer: deny it with deny");
    }
    if note.is_some() && !a.denies() {
        bail!("only a denial carries a note");
    }
    let file = match note.and_then(clean_note) {
        Some(note) => AnswerFile::Noted { answer: a, note },
        None => AnswerFile::Plain(a),
    };
    crate::privfs::write(
        &dir.join(format!("{id}.answer")),
        serde_json::to_string(&file)?,
    )?;
    Ok(())
}

/// Answer a question from `/dev/paddock/ask` with text. The text goes to
/// the agent that asked; nothing runs on the host. Only the host can call
/// this, like `answer`.
pub fn answer_reply(env: &HostEnv, id: &str, text: &str) -> Result<()> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        bail!("invalid request id");
    }
    let dir = approvals_dir(env);
    let pending = dir.join(format!("{id}.json"));
    if !pending.is_file() {
        bail!("no pending request {id}");
    }
    let p: Pending = serde_json::from_str(&std::fs::read_to_string(&pending)?)?;
    // A card of paddock itself with fixed choices: only those.
    if p.capability != ASK_CAPABILITY && !p.choices.is_empty() {
        if !p.choices.iter().any(|c| c == text.trim()) {
            bail!(
                "{id} takes one of these answers: {}, or deny",
                p.choices.join(", ")
            );
        }
        crate::privfs::write(
            &dir.join(format!("{id}.answer")),
            serde_json::json!({ "reply": text.trim() }).to_string(),
        )?;
        return Ok(());
    }
    if p.capability != ASK_CAPABILITY {
        bail!("{id} is a host request, not a question: answer it with once, session or deny");
    }
    if text.trim().is_empty() {
        bail!("the answer is empty");
    }
    if text.chars().count() > MAX_REPLY {
        bail!("the answer is longer than {MAX_REPLY} characters");
    }
    crate::privfs::write(
        &dir.join(format!("{id}.answer")),
        serde_json::json!({ "reply": text }).to_string(),
    )?;
    Ok(())
}

pub fn audit_file(env: &HostEnv) -> PathBuf {
    env.paddock_state().join("host-audit.jsonl")
}

/// What the audit log keeps about a request. The log lives on disk for a
/// long time, so it never holds request text that can be private:
///
/// - clipboard-write: the length of the text, never the text.
/// - exec: the program and the number of arguments. Arguments can hold
///   tokens or passwords (`curl -H "Authorization: ..."`), so they stay out.
/// - forge: the command line without the body text, and the body length.
/// - notify: the lengths of the title and the body.
/// - open-url: the scheme and host. Paths and queries can hold login codes.
/// - fetch, git-push, screenshot: the paths, remote and refs, as the user saw
///   them. These name host locations, not private content.
///
/// `prepared` is the detail that `prepare` worked out, for the capabilities
/// where it is safe to keep (fetch and git-push).
pub fn audit_detail(req: &Request, project: &Path, prepared: Option<&str>) -> String {
    let chars = |key: &str| arg_str(req, key).map_or(0, |t| t.chars().count());
    match req.cap.as_str() {
        "clipboard-write" => format!("set the host clipboard: {} characters", chars("text")),
        "exec" => {
            let a = argv(req);
            let total: usize =
                a.iter().map(|x| x.chars().count()).sum::<usize>() + a.len().saturating_sub(1);
            format!(
                "run on the host: {} with {} arguments ({total} characters; arguments are not logged)",
                a.first().map(|p| shell_quote(p)).unwrap_or_default(),
                a.len().saturating_sub(1),
            )
        }
        "forge" => {
            let tool = arg_str(req, "tool").unwrap_or("gh");
            let t = forge::tool(tool).unwrap_or(&forge::GH);
            let (rest, body) = forge::split_body(t, &argv(req));
            let mut line = std::iter::once(tool.to_string())
                .chain(rest)
                .map(|a| shell_quote(&a))
                .collect::<Vec<_>>()
                .join(" ");
            if let Some(b) = body {
                line.push_str(&format!(" (body: {} characters)", b.chars().count()));
            }
            line
        }
        "notify" => format!(
            "show a notification: title {} characters, body {} characters",
            chars("title"),
            chars("body")
        ),
        "open-url" => {
            let u = arg_str(req, "url").unwrap_or("");
            match url::Url::parse(u) {
                Ok(p) => format!(
                    "open {}://{} ({} characters)",
                    p.scheme(),
                    p.host_str().unwrap_or(""),
                    u.chars().count()
                ),
                Err(_) => format!("open a URL ({} characters)", u.chars().count()),
            }
        }
        "fetch" | "git-push" | "spawn" if prepared.is_some() => {
            prepared.unwrap_or_default().to_string()
        }
        "spawn" => format!(
            "spawn {} {}",
            arg_str(req, "action").unwrap_or("start"),
            arg_str(req, "harness").or(arg_str(req, "id")).unwrap_or("")
        ),
        "fetch" => format!(
            "copy {} into the sandbox",
            arg_str(req, "path").unwrap_or("")
        ),
        "git-push" => format!("git push {}", arg_str(req, "remote").unwrap_or("")),
        "screenshot" | "clipboard-read" => describe(req, project).1,
        other => other.to_string(),
    }
}

#[allow(clippy::too_many_arguments)]
fn audit(
    env: &HostEnv,
    ctx: &Context,
    req: &Request,
    detail: &str,
    request: Option<&str>,
    decision: &str,
    resp: &Response,
    note: Option<&str>,
) {
    // The note of a denial goes to the agent only: the log keeps its length.
    let message = match note {
        Some(_) => "the user denied the request with a note",
        None => resp.message.as_str(),
    };
    let line = serde_json::json!({
        "time": state::now(),
        "sandbox": ctx.id,
        "harness": ctx.harness,
        "project": ctx.project,
        "capability": req.cap,
        "request": request,
        "detail": detail,
        "reason": req.reason.chars().take(500).collect::<String>(),
        "decision": decision,
        "status": resp.status,
        "message": message,
    });
    let mut line = line;
    if let Some(n) = note {
        line["note_chars"] = n.chars().count().into();
    }
    if let Some(parent) = ctx.chain.last() {
        line["parent"] = parent.clone().into();
        line["chain"] = serde_json::json!(ctx.chain);
    }
    audit_line(env, &line);
}

/// Append one line to the audit log.
pub fn audit_line(env: &HostEnv, line: &serde_json::Value) {
    let f = audit_file(env);
    match crate::privfs::append(&f) {
        Ok(mut file) => {
            if let Err(e) = writeln!(file, "{line}") {
                log::warn!("cannot write the audit log {}: {e}", f.display());
            }
        }
        Err(e) => log::warn!("cannot open the audit log {}: {e}", f.display()),
    }
}

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

/// The answer source. The real one waits for `paddock approvals answer` or a
/// notification action. Tests use a fixed answer.
pub trait Approver: Send + Sync {
    fn ask(&self, env: &HostEnv, p: &Pending, wait: Duration) -> Option<Answer>;

    /// The answer and who gave it ("user" or "agent"), for the audit log.
    fn ask_with_source(
        &self,
        env: &HostEnv,
        p: &Pending,
        wait: Duration,
    ) -> Option<(Answer, &'static str)> {
        self.ask(env, p, wait).map(|a| (a, "user"))
    }

    /// The answer with the user's note on a denial. Callers that tell the
    /// agent about a denial use this one.
    fn ask_verdict(&self, env: &HostEnv, p: &Pending, wait: Duration) -> Option<Verdict> {
        self.ask_with_source(env, p, wait)
            .map(|(answer, who)| Verdict {
                answer,
                note: None,
                who,
            })
    }
}

/// Answers every request with deny. `paddock doctor --escape` uses it.
pub struct DenyAll;

impl Approver for DenyAll {
    fn ask(&self, _: &HostEnv, _: &Pending, _: Duration) -> Option<Answer> {
        Some(Answer::Deny)
    }
}

pub struct HostApprover;

/// Write a pending request for the panel and the CLI: 0600 in a 0700 folder.
/// Text from a sandbox, made safe to show: no control characters except
/// newline and tab, and no invisible format characters. A terminal would run
/// escape sequences (clear the screen, rewrite a line), and bidi overrides
/// can reverse text on a card or in a notification.
pub fn clean_text(s: &str) -> String {
    s.chars()
        .filter(|&c| {
            if c == '\n' || c == '\t' {
                return true;
            }
            !c.is_control()
                && !matches!(c,
                    '\u{061C}' | '\u{180E}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}'
                    | '\u{2060}'..='\u{206F}' | '\u{FEFF}' | '\u{FFF9}'..='\u{FFFB}')
        })
        .collect()
}

impl Pending {
    /// The same request with every text that a sandbox can choose cleaned.
    pub fn cleaned(&self) -> Self {
        let mut p = self.clone();
        p.summary = clean_text(&p.summary);
        p.detail = clean_text(&p.detail);
        p.reason = clean_text(&p.reason);
        p.harness = clean_text(&p.harness);
        p.choices = p.choices.iter().map(|c| clean_text(c)).collect();
        p.started_by = p.started_by.iter().map(|c| clean_text(c)).collect();
        p
    }
}

pub fn write_pending(env: &HostEnv, p: &Pending) -> std::io::Result<PathBuf> {
    let p = &p.cleaned();
    let pending = approvals_dir(env).join(format!("{}.json", p.id));
    crate::privfs::write(&pending, serde_json::to_string(p).unwrap_or_default())?;
    Ok(pending)
}

impl Approver for HostApprover {
    fn ask(&self, env: &HostEnv, p: &Pending, wait: Duration) -> Option<Answer> {
        self.ask_verdict(env, p, wait).map(|v| v.answer)
    }

    fn ask_with_source(
        &self,
        env: &HostEnv,
        p: &Pending,
        wait: Duration,
    ) -> Option<(Answer, &'static str)> {
        self.ask_verdict(env, p, wait).map(|v| (v.answer, v.who))
    }

    fn ask_verdict(&self, env: &HostEnv, p: &Pending, wait: Duration) -> Option<Verdict> {
        let p = &p.cleaned();
        let answer_file = approvals_dir(env).join(format!("{}.answer", p.id));
        let Ok(pending) = write_pending(env, p) else {
            return None;
        };
        let shown = notify_with_actions(env, p);
        let start = Instant::now();
        let mut got = None;
        while start.elapsed() < wait {
            if let Ok(t) = std::fs::read_to_string(&answer_file) {
                got = AnswerFile::parse(&t).map(|(answer, note)| Verdict {
                    answer,
                    note,
                    who: "user",
                });
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        // Answered elsewhere or timed out: the buttons mean nothing now.
        if let Some(h) = shown {
            h.close();
        }
        let _ = std::fs::remove_file(&pending);
        let _ = std::fs::remove_file(&answer_file);
        got
    }
}

/// Put a card with fixed choices up (`TIMEOUT_CAPABILITY`) and wait for
/// the answer: one of `p.choices`, or deny. Stop waiting after `wait` or
/// when `cancelled` turns true.
pub fn ask_choice(
    env: &HostEnv,
    p: &Pending,
    wait: Duration,
    cancelled: &dyn Fn() -> bool,
) -> crate::spawn::Choice {
    use crate::spawn::Choice;
    let p = &p.cleaned();
    let answer_file = approvals_dir(env).join(format!("{}.answer", p.id));
    let Ok(pending) = write_pending(env, p) else {
        return Choice::NoAnswer;
    };
    let shown = notify_choices(env, p);
    let start = Instant::now();
    let mut got = Choice::NoAnswer;
    while start.elapsed() < wait && !cancelled() {
        if let Ok(t) = std::fs::read_to_string(&answer_file) {
            let reply = serde_json::from_str::<serde_json::Value>(&t)
                .ok()
                .and_then(|v| v["reply"].as_str().map(str::to_string));
            got = match reply {
                Some(r) if p.choices.contains(&r) => Choice::More(r),
                _ => Choice::Stop,
            };
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    if let Some(h) = shown {
        h.close();
    }
    let _ = std::fs::remove_file(&pending);
    let _ = std::fs::remove_file(&answer_file);
    got
}

/// The notification for a card with fixed choices, when the bar does not
/// run: one button per choice, and Stop.
fn notify_choices(env: &HostEnv, p: &Pending) -> Option<notify::Handle> {
    if approval_ui_active(env) {
        return None;
    }
    let mut actions: Vec<(String, String)> = p
        .choices
        .iter()
        .enumerate()
        .map(|(i, c)| (format!("choice:{i}"), format!("+{c}")))
        .collect();
    actions.push(("deny".into(), "Stop".into()));
    let note = notify::Note {
        summary: format!("paddock: {}", p.summary),
        body: p.detail.clone(),
        actions,
        critical: true,
    };
    let (env2, id, choices) = (env.clone(), p.id.clone(), p.choices.clone());
    let on_action = move |key: &str| {
        if let Some(c) = key
            .strip_prefix("choice:")
            .and_then(|i| i.parse::<usize>().ok())
            .and_then(|i| choices.get(i))
        {
            let _ = answer_reply(&env2, &id, c);
        } else if key == "deny" {
            let _ = answer(&env2, &id, Answer::Deny);
        }
    };
    match notify::ask(env, &note, on_action) {
        Ok(h) => Some(h),
        Err(e) => {
            log::debug!("cannot show the card as a notification: {e:#}");
            None
        }
    }
}

/// A desktop notification with buttons. Its answer goes through the same
/// answer file, so the panel, the CLI and the notification never disagree.
/// The bar plugin's approval service writes the Unix time to this file every
/// few seconds while it runs. Then it shows the request itself, and a second
/// popup would only repeat it.
pub fn approval_ui_file(env: &HostEnv) -> PathBuf {
    env.paddock_run().join("approval-ui")
}

pub fn approval_ui_active(env: &HostEnv) -> bool {
    std::fs::read_to_string(approval_ui_file(env))
        .ok()
        .and_then(|t| t.trim().parse::<u64>().ok())
        .is_some_and(|t| state::now().saturating_sub(t) < 15)
}

/// The fallback prompt when the bar does not run. The request detail can
/// hold clipboard text, so it goes over the user's session bus, never into
/// the arguments of a process, which every local user can read.
pub fn approval_note(p: &Pending) -> notify::Note {
    let mut actions = vec![("once".to_string(), "Allow once".to_string())];
    if p.session_allowed {
        actions.push(("session".into(), "Allow for this session".into()));
    }
    actions.push(("deny".into(), "Deny".into()));
    if p.session_allowed {
        actions.push(("deny-session".into(), "Deny for this session".into()));
    }
    notify::Note {
        summary: format!("paddock: {} asks for {}", p.harness, p.capability),
        body: {
            let mut b = match crate::spawn::chain_text(&p.started_by) {
                Some(c) => format!("{c}\n\n{}", p.detail),
                None => p.detail.clone(),
            };
            if !p.reason.is_empty() {
                b.push_str(&format!("\n\nReason: {}", p.reason));
            }
            b
        },
        actions,
        critical: true,
    }
}

fn notify_with_actions(env: &HostEnv, p: &Pending) -> Option<notify::Handle> {
    if approval_ui_active(env) {
        return None;
    }
    let (env2, id) = (env.clone(), p.id.clone());
    let on_action = move |key: &str| {
        if let Ok(a) = key.parse::<Answer>() {
            let _ = answer(&env2, &id, a);
        }
    };
    match notify::ask(env, &approval_note(p), on_action) {
        Ok(h) => Some(h),
        Err(e) => {
            log::debug!("cannot show the approval notification: {e:#}");
            None
        }
    }
}

fn arg_str<'a>(req: &'a Request, key: &str) -> Option<&'a str> {
    req.args.get(key).and_then(|v| v.as_str())
}

fn argv(req: &Request) -> Vec<String> {
    req.args
        .get("argv")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Quote one argument for display, the way a shell would read it.
fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:,@%+".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// (summary, detail) for the approval prompt and the audit log.
pub fn describe(req: &Request, project: &Path) -> (String, String) {
    match req.cap.as_str() {
        "screenshot" => {
            let t = arg_str(req, "target").unwrap_or("screen");
            (
                format!("screenshot of the {t}"),
                format!("Capture the host {t} into {}/{INBOX}", project.display()),
            )
        }
        "exec" => {
            let line = argv(req)
                .iter()
                .map(|a| shell_quote(a))
                .collect::<Vec<_>>()
                .join(" ");
            (
                format!(
                    "run on the host: {}",
                    line.chars().take(80).collect::<String>()
                ),
                format!(
                    "Run on the host in {}.\nThe sandbox writes that folder, so a command that reads files there, a git command for example, can take instructions from them.\n{line}",
                    project.display()
                ),
            )
        }
        "fetch" => {
            let p = arg_str(req, "path").unwrap_or("");
            (
                format!("copy {p} into the sandbox"),
                format!("Copy the host path {p} into {}/{INBOX}", project.display()),
            )
        }
        "clipboard-read" => (
            "read the host clipboard".into(),
            "Give the current host clipboard text to the sandbox".into(),
        ),
        "clipboard-write" => {
            let t = arg_str(req, "text").unwrap_or("");
            (
                "set the host clipboard".into(),
                format!(
                    "Set the host clipboard to:\n{}",
                    t.chars().take(500).collect::<String>()
                ),
            )
        }
        "open-url" => {
            let u = arg_str(req, "url").unwrap_or("");
            (
                format!("open {u}"),
                format!("Open in the host browser:\n{u}"),
            )
        }
        "notify" => (
            "show a notification".into(),
            arg_str(req, "title").unwrap_or("").to_string(),
        ),
        "git-push" => (
            format!("git push {}", arg_str(req, "remote").unwrap_or("")),
            format!(
                "git push {} {}",
                arg_str(req, "remote").unwrap_or(""),
                req.args
                    .get("refs")
                    .map(|r| r.to_string())
                    .unwrap_or_default()
            ),
        ),
        "forge" => {
            let line = std::iter::once(arg_str(req, "tool").unwrap_or("gh").to_string())
                .chain(argv(req))
                .map(|a| shell_quote(&a))
                .collect::<Vec<_>>()
                .join(" ");
            (line.chars().take(80).collect(), line)
        }
        other => (other.to_string(), other.to_string()),
    }
}

/// A key that "allow for this session" remembers. For `exec` it is the exact
/// command line, so a grant never covers a different command.
fn grant_key(req: &Request) -> String {
    match req.cap.as_str() {
        "exec" => format!("exec\0{}", argv(req).join("\0")),
        "open-url" => "open-url".into(),
        other => other.to_string(),
    }
}

fn validate(req: &Request) -> Result<()> {
    if req.v != PROTOCOL {
        bail!(
            "protocol version {} is not supported; this paddock speaks {PROTOCOL}",
            req.v
        );
    }
    match req.cap.as_str() {
        "screenshot" => {
            let t = arg_str(req, "target").unwrap_or("screen");
            if !(t == "screen"
                || t == "window"
                || t.strip_prefix("output:")
                    .is_some_and(|n| !n.is_empty() && n.len() < 64))
            {
                bail!("target must be screen, window or output:NAME");
            }
        }
        "exec" => {
            if argv(req).is_empty() {
                bail!("exec needs a command");
            }
        }
        "fetch" => {
            let p = PathBuf::from(arg_str(req, "path").context("fetch needs a path")?);
            if !p.is_absolute() {
                bail!("fetch needs an absolute host path");
            }
            // `prepare` opens the path and checks where it really leads.
        }
        "clipboard-write" => {
            arg_str(req, "text").context("clipboard-write needs text")?;
        }
        "open-url" => {
            let u = arg_str(req, "url").context("open-url needs a url")?;
            let parsed = url::Url::parse(u).context("not a valid URL")?;
            if !matches!(parsed.scheme(), "http" | "https") {
                bail!("only http and https URLs can be opened");
            }
        }
        "notify" => {
            arg_str(req, "title").context("notify needs a title")?;
        }
        "clipboard-read" => {}
        "git-push" => git::validate(&req.args)?,
        "forge" => {
            let t = arg_str(req, "tool").unwrap_or("gh");
            if forge::tool(t).is_none() {
                bail!(
                    "forge supports {}",
                    forge::TOOLS
                        .iter()
                        .map(|t| t.name)
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            if argv(req).is_empty() {
                bail!("forge needs a command, for example pr view");
            }
        }
        "spawn" => {
            let name_ok = |s: &str| {
                !s.is_empty()
                    && s.len() <= 64
                    && s.chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            };
            match arg_str(req, "action").unwrap_or("start") {
                "start" => {
                    if !arg_str(req, "harness").is_some_and(name_ok) {
                        bail!("spawn needs a harness name, for example codex");
                    }
                    if arg_str(req, "prompt_file").is_none_or(str::is_empty) {
                        bail!("spawn needs --prompt-file FILE");
                    }
                }
                "status" | "wait" | "kill" => {
                    if !arg_str(req, "id").is_some_and(name_ok) {
                        bail!("give the spawn id that `paddock-host spawn` printed");
                    }
                }
                "list" => {}
                other => bail!("unknown spawn action {other}"),
            }
        }
        other => bail!("unknown capability {other}"),
    }
    Ok(())
}

fn safe_name(raw: &str, fallback: &str) -> String {
    let name: String = Path::new(raw)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "-_.".contains(c) {
                c
            } else {
                '_'
            }
        })
        .take(100)
        .collect();
    if name.is_empty() || name.starts_with('.') {
        fallback.to_string()
    } else {
        name
    }
}

/// The inbox in the project. The project is sandbox-writable, so the folder
/// is opened one step at a time without following symlinks, and every result
/// is written through it (see safefs).
pub(crate) fn inbox(project: &Path) -> Result<crate::safefs::Dir> {
    let root = crate::safefs::Root::open(project)?;
    let dot = root.dir(Path::new(".paddock"), true)?;
    // Keep results out of commits. An existing entry, whatever it is, stays.
    if let Ok(mut f) = dot.create_new(std::ffi::OsStr::new(".gitignore"), 0o644) {
        f.write_all(b"*\n")?;
    }
    dot.subdir(std::ffi::OsStr::new("inbox"), true)
}

pub fn run_capture(cmd: &mut Command, timeout: Duration) -> Result<(i32, String, String)> {
    run_capture_input(cmd, timeout, None)
}

/// Like `run_capture`, with `input` written to the command's stdin.
pub fn run_capture_input(
    cmd: &mut Command,
    timeout: Duration,
    input: Option<&[u8]>,
) -> Result<(i32, String, String)> {
    let mut child = cmd
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    if let (Some(data), Some(mut stdin)) = (input, child.stdin.take()) {
        let data = data.to_vec();
        // A thread, so a command that does not read cannot block paddock.
        std::thread::spawn(move || {
            let _ = stdin.write_all(&data);
        });
    }
    let (mut so, mut se) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
    let t1 = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = so.by_ref().take(MAX_OUTPUT as u64).read_to_end(&mut b);
        b
    });
    let t2 = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = se.by_ref().take(MAX_OUTPUT as u64).read_to_end(&mut b);
        b
    });
    let start = Instant::now();
    let code = loop {
        if let Some(s) = child.try_wait()? {
            break s.code().unwrap_or(-1);
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "the command did not finish in {} seconds and was stopped",
                timeout.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let out = String::from_utf8_lossy(&t1.join().unwrap_or_default()).into_owned();
    let err = String::from_utf8_lossy(&t2.join().unwrap_or_default()).into_owned();
    Ok((code, out, err))
}

/// Do the approved work on the host.
fn perform(env: &HostEnv, ctx: &Context, req: &Request, work: &Work) -> Result<serde_json::Value> {
    match work {
        Work::Spawn { .. } | Work::SpawnControl => bail!("spawn was not prepared"),
        // The executor pushes (Broker::execute), never this process.
        Work::Push(_) => bail!("the push runs in the executor"),
        Work::Forge(_) => bail!("gh runs in the executor"),
        Work::Fetch(f) => return fetch(env, ctx, req, f),
        Work::Plain => {}
    }
    // Every other capability runs in the executor (Broker::run_elsewhere).
    let _ = (env, ctx);
    bail!("{} runs in the executor", req.cap)
}

/// A fetch source, opened before the user is asked. The copy reads from the
/// open fd, so changing the path during the wait changes nothing.
struct FetchPlan {
    src: crate::safefs::Source,
    /// Where the path led when it was opened; the prompt shows this.
    real: PathBuf,
    size: u64,
}

/// Refuse a fetch of a protected path: the path asked for, where it really
/// leads, or a folder that holds a protected path.
fn check_fetch(env: &HostEnv, asked: &Path, real: &Path, dir: bool) -> Result<()> {
    let mut denied = policy::denied_paths(env);
    // Harness logins too: paddock copies them in itself, never by request.
    if let Ok(f) = crate::profile::HarnessFactory::new(env) {
        for n in f.names() {
            if let Some(p) = f.profile(&n) {
                denied.extend(p.state.auth.iter().map(|a| env.home.join(a)));
            }
        }
    }
    // paddock's own folders: sandbox homes of other projects, approvals,
    // the audit log, paddock kv, the gateway key, the configuration.
    denied.extend([
        env.paddock_data(),
        env.paddock_state(),
        env.paddock_config(),
        env.paddock_cache(),
    ]);
    for d in denied {
        let canon = std::fs::canonicalize(&d).ok();
        let hit =
            |d: &Path| real.starts_with(d) || asked.starts_with(d) || (dir && d.starts_with(real));
        if hit(&d) || canon.as_deref().is_some_and(hit) {
            bail!(
                "{} is or holds a protected path; paddock never copies it into a sandbox",
                asked.display()
            );
        }
    }
    Ok(())
}

fn plan_fetch(env: &HostEnv, req: &Request) -> Result<FetchPlan> {
    let asked = PathBuf::from(arg_str(req, "path").context("fetch needs a path")?);
    let src = crate::safefs::Source::open(&asked)?;
    let real = src.real_path()?;
    check_fetch(env, &asked, &real, src.is_dir())?;
    let size = src.size(MAX_FETCH)?;
    if size > MAX_FETCH {
        bail!(
            "{} is {} MiB; the limit is {} MiB",
            real.display(),
            size >> 20,
            MAX_FETCH >> 20
        );
    }
    Ok(FetchPlan { src, real, size })
}

fn fetch(env: &HostEnv, ctx: &Context, req: &Request, f: &FetchPlan) -> Result<serde_json::Value> {
    // Check again after the wait: the open file or folder may have moved
    // into a protected place. A replaced path does not matter: the copy
    // reads what was opened, which is what the user saw.
    let asked = PathBuf::from(arg_str(req, "path").unwrap_or(""));
    check_fetch(env, &asked, &f.src.real_path()?, f.src.is_dir())?;
    let dir = inbox(&ctx.project)?;
    let want = safe_name(&f.real.to_string_lossy(), "fetched");
    // Never remove what is there: a free name instead.
    let name = dir.copy_source(&f.src, &want, MAX_FETCH)?;
    Ok(serde_json::json!({ "path": dir.path().join(name), "bytes": f.size }))
}

/// The work behind a request, worked out before the user is asked.
enum Work {
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
}

/// What the user sees and what a session grant covers.
struct Prepared {
    summary: String,
    detail: String,
    /// "Allow for this session" stores these keys. A request is covered when
    /// every key is granted.
    keys: Vec<String>,
    /// A session grant can cover this request.
    session_allowed: bool,
    /// Ask even when the configuration says allow.
    always_ask: bool,
    /// Allowed without asking unless the capability is denied (forge reads).
    read_only: bool,
    /// What the audit log may keep, when it differs from `audit_detail`.
    audit: Option<String>,
    work: Work,
}

fn prepare(env: &HostEnv, ctx: &Context, req: &Request) -> Result<Prepared> {
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
            let call = forge::check(t, &argv(req), arg_str(req, "branch"), remotes)?;
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
            grants: Mutex::new(Vec::new()),
            denials: Mutex::new(Vec::new()),
        }
    }

    /// The capabilities that the executor runs from a plan of this process:
    /// gh, the desktop tools, links and notifications.
    fn run_elsewhere(&self, req: &Request, work: &Work) -> Result<serde_json::Value> {
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
    fn execute(&self, req: &Request, plan: serde_json::Value) -> Result<serde_json::Value> {
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

    fn spawn_off_reason(&self) -> String {
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
    fn prepare_spawn(&self, req: &Request) -> std::result::Result<Prepared, Response> {
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
        let has_oneshot = crate::profile::HarnessFactory::new(&self.env)
            .ok()
            .and_then(|f| f.profile(&harness).map(|p| p.oneshot.is_some()));
        match has_oneshot {
            None => return Err(invalid(format!("unknown harness {harness}"))),
            Some(false) => {
                return Err(invalid(format!(
                    "{harness} has no one-shot mode in its profile, so it cannot run a task without a terminal"
                )))
            }
            Some(true) => {}
        }
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
        let prompt = read_prompt(&self.ctx.project, arg_str(req, "prompt_file").unwrap_or(""))
            .map_err(|e| invalid(format!("{e:#}")))?;
        let text = String::from_utf8_lossy(&prompt);
        let chars = text.chars().count();
        let shown: String = text.chars().take(crate::spawn::PROMPT_SHOWN).collect();
        let c = &sp.ceiling;
        let detail = format!(
            "Start {harness} in a new sandbox with this task. It runs once and stops.\n\
             Role: {}\n\
             Login: {} (the {harness} login of this computer; this sandbox never sees it)\n\
             Branch: a new worktree of {} from {}\n\
             Limits: network {}, backend {}, no display, {} (enforced by {}), at most {} at a time\n\
             Results go to {}/{INBOX}.\n\n\
             Task ({chars} characters{}):\n{shown}",
            role.describe(),
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
                "start {harness} as {} in a new sandbox (login {}; {}; task {chars} characters, not logged)",
                role.name(),
                c.auth,
                limits.describe()
            )),
            work: Work::Spawn {
                harness,
                prompt,
                opts: crate::spawn::StartOpts {
                    role,
                    limits,
                    from_head,
                },
            },
        })
    }

    fn refuse_reviewer(&self, req: &Request) -> Response {
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

    fn perform_spawn(&self, req: &Request, work: &Work) -> Response {
        let Some(sp) = &self.spawner else {
            return Response::new(Status::Disabled, self.spawn_off_reason());
        };
        let to_json = |s: &crate::spawn::Status| serde_json::to_value(s).unwrap_or_default();
        let id = arg_str(req, "id").unwrap_or("");
        let missing = || Response::new(Status::Invalid, format!("no child {id} of this sandbox"));
        match (work, arg_str(req, "action").unwrap_or("start")) {
            (
                Work::Spawn {
                    harness,
                    prompt,
                    opts,
                },
                _,
            ) => match sp.start(harness, prompt, *opts) {
                Ok(s) => Response::done(to_json(&s)),
                Err(e) => Response::new(Status::Failed, format!("{e:#}")),
            },
            (_, "list") => Response::done(serde_json::json!({
                "children": sp.list(),
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
        v
    }

    pub fn handle(&self, req: &Request) -> Response {
        if req.cap == "capabilities" {
            return Response::done(self.capabilities());
        }
        let invalid = |e: anyhow::Error| {
            let r = Response::new(Status::Invalid, format!("{e:#}"));
            log::info!(
                "sandbox {}: invalid {} request: {e:#}",
                self.ctx.id,
                req.cap
            );
            audit(
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
        if let Err(e) = validate(req) {
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
            audit(
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
                    audit(
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
        } else {
            match prepare(&self.env, &self.ctx, req) {
                Ok(p) => p,
                Err(e) => return invalid(e),
            }
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
            _ if matches!(prep.work, Work::Spawn { .. } | Work::SpawnControl) => {
                self.perform_spawn(req, &prep.work)
            }
            _ if matches!(
                req.cap.as_str(),
                "screenshot" | "clipboard-read" | "clipboard-write" | "open-url" | "notify"
            ) || matches!(prep.work, Work::Forge(_)) =>
            {
                match self.run_elsewhere(req, &prep.work) {
                    Ok(result) => Response::done(result),
                    Err(e) => Response::new(Status::Failed, format!("{e:#}")),
                }
            }
            _ if req.cap == "exec" => match self.execute(req, serde_json::Value::Null) {
                Ok(result) => Response::done(result),
                Err(e) => Response::new(Status::Failed, format!("{e:#}")),
            },
            // The push runs in the executor too, from the plan of the card.
            _ if matches!(prep.work, Work::Push(_)) => {
                let Work::Push(plan) = &prep.work else {
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
            _ => match perform(&self.env, &self.ctx, req, &prep.work) {
                Ok(result) => Response::done(result),
                Err(e) => Response::new(Status::Failed, format!("{e:#}")),
            },
        };
        // Status and wait calls are reads of this sandbox's own children.
        if matches!(prep.work, Work::SpawnControl)
            && arg_str(req, "action").is_some_and(|a| a != "kill")
        {
            return resp;
        }
        let safe = match &prep.audit {
            Some(a) => Some(a.as_str()),
            None => {
                matches!(req.cap.as_str(), "fetch" | "git-push").then_some(prep.detail.as_str())
            }
        };
        audit(
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

pub(crate) fn new_request_id() -> String {
    use sha2::{Digest, Sha256};
    let seed = format!("{:?}-{}", Instant::now(), std::process::id());
    let h = crate::manifest::hex(&Sha256::digest(seed.as_bytes()));
    format!("r-{}-{}", state::now(), &h[..8])
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
