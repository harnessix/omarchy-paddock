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
}

impl Status {
    /// The exit code of `paddock-host` for this status.
    pub fn exit_code(&self) -> i32 {
        match self {
            Status::Done => 0,
            Status::Denied => 3,
            Status::Disabled => 4,
            Status::Timeout => 5,
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Answer {
    Once,
    Session,
    Deny,
}

impl std::str::FromStr for Answer {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "once" => Ok(Self::Once),
            "session" => Ok(Self::Session),
            "deny" => Ok(Self::Deny),
            _ => bail!("answer must be once, session or deny"),
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
    out.sort_by_key(|p| p.created);
    out
}

/// Record the user's answer. Only the host can call this: the approvals
/// directory is not mounted in any sandbox.
pub fn answer(env: &HostEnv, id: &str, a: Answer) -> Result<()> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        bail!("invalid request id");
    }
    let dir = approvals_dir(env);
    let pending = dir.join(format!("{id}.json"));
    if !pending.is_file() {
        bail!("no pending request {id}");
    }
    let p: Pending = serde_json::from_str(&std::fs::read_to_string(&pending)?)?;
    if a == Answer::Session && !p.session_allowed {
        bail!("this request can only be allowed once");
    }
    crate::privfs::write(
        &dir.join(format!("{id}.answer")),
        serde_json::to_string(&a)?,
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
        "fetch" | "git-push" if prepared.is_some() => prepared.unwrap_or_default().to_string(),
        "fetch" => format!(
            "copy {} into the sandbox",
            arg_str(req, "path").unwrap_or("")
        ),
        "git-push" => format!("git push {}", arg_str(req, "remote").unwrap_or("")),
        "screenshot" | "clipboard-read" => describe(req, project).1,
        other => other.to_string(),
    }
}

fn audit(
    env: &HostEnv,
    ctx: &Context,
    req: &Request,
    detail: &str,
    request: Option<&str>,
    decision: &str,
    resp: &Response,
) {
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
        "message": resp.message,
    });
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
pub fn write_pending(env: &HostEnv, p: &Pending) -> std::io::Result<PathBuf> {
    let pending = approvals_dir(env).join(format!("{}.json", p.id));
    crate::privfs::write(&pending, serde_json::to_string(p).unwrap_or_default())?;
    Ok(pending)
}

impl Approver for HostApprover {
    fn ask(&self, env: &HostEnv, p: &Pending, wait: Duration) -> Option<Answer> {
        let answer_file = approvals_dir(env).join(format!("{}.answer", p.id));
        let Ok(pending) = write_pending(env, p) else {
            return None;
        };
        let shown = notify_with_actions(env, p);
        let start = Instant::now();
        let mut got = None;
        while start.elapsed() < wait {
            if let Ok(t) = std::fs::read_to_string(&answer_file) {
                got = serde_json::from_str(&t).ok();
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
    notify::Note {
        summary: format!("paddock: {} asks for {}", p.harness, p.capability),
        body: if p.reason.is_empty() {
            p.detail.clone()
        } else {
            format!("{}\n\nReason: {}", p.detail, p.reason)
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
fn inbox(project: &Path) -> Result<crate::safefs::Dir> {
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
        Work::Push(plan) => return git::push(plan),
        Work::Forge(call) => return run_forge(env, ctx, req, call),
        Work::Fetch(f) => return fetch(env, ctx, req, f),
        Work::Plain => {}
    }
    match req.cap.as_str() {
        "screenshot" => {
            let grim = env
                .which("grim")
                .context("screenshots need grim on the host")?;
            let dir = inbox(&ctx.project)?;
            let name = safe_name(
                arg_str(req, "out").unwrap_or(""),
                &format!("screenshot-{}.png", state::now()),
            );
            let mut cmd = Command::new(grim);
            match arg_str(req, "target").unwrap_or("screen") {
                "screen" => {}
                "window" => {
                    let geo = active_window_geometry(env)?;
                    cmd.args(["-g", &geo]);
                }
                t => {
                    cmd.args(["-o", t.trim_start_matches("output:")]);
                }
            }
            // grim writes the PNG to stdout; paddock writes the file through
            // the inbox folder it opened, so grim never resolves a project path.
            cmd.arg("-");
            let out = cmd.stdin(Stdio::null()).stderr(Stdio::piped()).output()?;
            if !out.status.success() {
                bail!(
                    "grim failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            let (mut f, name) = dir.new_file(&name, 0o644)?;
            f.write_all(&out.stdout)?;
            Ok(serde_json::json!({ "path": dir.path().join(name) }))
        }
        "exec" => {
            let a = argv(req);
            let timeout = req
                .args
                .get("timeout")
                .and_then(|t| t.as_u64())
                .unwrap_or(120)
                .clamp(1, 3600);
            let mut cmd = Command::new(&a[0]);
            cmd.args(&a[1..]).current_dir(&ctx.project);
            let (code, out, err) = run_capture(&mut cmd, Duration::from_secs(timeout))?;
            Ok(serde_json::json!({ "exit_code": code, "stdout": out, "stderr": err }))
        }
        "fetch" => bail!("fetch was not prepared"),
        "clipboard-read" => {
            let wp = env
                .which("wl-paste")
                .context("clipboard access needs wl-clipboard on the host")?;
            let (_, out, _) =
                run_capture(Command::new(wp).arg("--no-newline"), Duration::from_secs(5))?;
            Ok(serde_json::json!({ "text": out }))
        }
        "clipboard-write" => {
            let wc = env
                .which("wl-copy")
                .context("clipboard access needs wl-clipboard on the host")?;
            let mut child = Command::new(wc)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?;
            child
                .stdin
                .take()
                .unwrap()
                .write_all(arg_str(req, "text").unwrap_or("").as_bytes())?;
            child.wait()?;
            Ok(serde_json::json!({}))
        }
        "open-url" => {
            let opener = env
                .which("xdg-open")
                .context("opening URLs needs xdg-open on the host")?;
            Command::new(opener)
                .arg(arg_str(req, "url").unwrap_or(""))
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?;
            // A login page: let its localhost callback through for a while.
            crate::login::note_opened(arg_str(req, "url").unwrap_or(""));
            Ok(serde_json::json!({}))
        }
        "notify" => {
            // Over the session bus: the text never shows in a process list.
            let n = notify::Note {
                summary: format!("{}: {}", ctx.harness, arg_str(req, "title").unwrap_or("")),
                body: arg_str(req, "body").unwrap_or("").to_string(),
                ..Default::default()
            };
            notify::show(env, &n).context("cannot show the notification")?;
            Ok(serde_json::json!({}))
        }
        _ => unreachable!(),
    }
}

fn active_window_geometry(env: &HostEnv) -> Result<String> {
    let hc = env
        .which("hyprctl")
        .context("target window needs Hyprland; use screen or output:NAME")?;
    let out = Command::new(hc).args(["activewindow", "-j"]).output()?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).context("no active window")?;
    let at = v["at"].as_array().context("no active window")?;
    let size = v["size"].as_array().context("no active window")?;
    Ok(format!(
        "{},{} {}x{}",
        at[0].as_i64().unwrap_or(0),
        at[1].as_i64().unwrap_or(0),
        size[0].as_i64().unwrap_or(0),
        size[1].as_i64().unwrap_or(0)
    ))
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
    denied.push(env.paddock_data().join("auth"));
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
                work: Work::Plain,
            })
        }
    }
}

/// Run a checked forge call in an empty folder: the tool must not read the
/// project's `.git`, which the sandbox can write.
fn run_forge(
    env: &HostEnv,
    ctx: &Context,
    req: &Request,
    call: &forge::Call,
) -> Result<serde_json::Value> {
    let exe = env
        .which(call.tool)
        .with_context(|| format!("{} is not installed on the host", call.tool))?;
    let dir = env
        .paddock_run()
        .join(&ctx.id)
        .join(format!("forge-{}", new_request_id()));
    crate::privfs::dir(&dir)?;
    let long = call.argv.iter().any(|a| a == "watch" || a == "--watch");
    let timeout = req
        .args
        .get("timeout")
        .and_then(|t| t.as_u64())
        .unwrap_or(if long { 1800 } else { 120 })
        .clamp(1, 3600);
    let mut cmd = Command::new(exe);
    cmd.args(&call.argv)
        .current_dir(&dir)
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .env("GH_SPINNER_DISABLED", "1")
        .env("NO_COLOR", "1")
        .env_remove("GH_REPO")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE");
    // Body text goes on stdin (`--body-file -`): the arguments of a host
    // process are visible to every local user.
    let r = run_capture_input(
        &mut cmd,
        Duration::from_secs(timeout),
        call.stdin.as_deref().map(str::as_bytes),
    );
    let _ = std::fs::remove_dir_all(&dir);
    let (code, out, err) = r?;
    Ok(serde_json::json!({ "exit_code": code, "stdout": out, "stderr": err }))
}

pub struct Broker {
    pub env: HostEnv,
    pub ctx: Context,
    pub policy: HostPolicy,
    pub approver: Arc<dyn Approver>,
    grants: Mutex<Vec<String>>,
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
            grants: Mutex::new(Vec::new()),
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
                    "mode": self.policy.mode(c),
                    "session_granted": grants.iter().any(|g| g == c || g.starts_with(&format!("{c}\0"))),
                    "description": desc,
                })
            })
            .collect();
        serde_json::json!({ "protocol": PROTOCOL, "wait_seconds": self.policy.wait, "capabilities": caps })
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
            );
            r
        };
        if let Err(e) = validate(req) {
            return invalid(e);
        }
        let mode = self.policy.mode(&req.cap);
        if mode == Mode::Deny {
            let r = Response::new(
                Status::Disabled,
                format!(
                    "{} is set to deny in [host] of the user configuration",
                    req.cap
                ),
            );
            audit(
                &self.env,
                &self.ctx,
                req,
                &audit_detail(req, &self.ctx.project, None),
                None,
                "disabled",
                &r,
            );
            return r;
        }
        let prep = match prepare(&self.env, &self.ctx, req) {
            Ok(p) => p,
            Err(e) => return invalid(e),
        };
        let granted = || {
            let g = self.grants.lock().unwrap();
            prep.session_allowed && prep.keys.iter().all(|k| g.contains(k))
        };
        let mut request_id: Option<String> = None;
        let (decision, allowed) = if mode == Mode::Allow && !prep.always_ask {
            ("allowed by configuration", Some(true))
        } else if prep.read_only {
            ("allowed: read-only", Some(true))
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
            };
            match self.approver.ask_with_source(
                &self.env,
                &p,
                Duration::from_secs(self.policy.wait),
            ) {
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
                None => ("timed out", None),
            }
        };
        let resp = match (decision, allowed) {
            ("timed out", _) => Response::new(
                Status::Timeout,
                format!("nobody answered within {} seconds", self.policy.wait),
            ),
            (_, Some(false)) => Response::new(Status::Denied, "the user denied the request"),
            _ => match perform(&self.env, &self.ctx, req, &prep.work) {
                Ok(result) => Response::done(result),
                Err(e) => Response::new(Status::Failed, format!("{e:#}")),
            },
        };
        let safe = matches!(req.cap.as_str(), "fetch" | "git-push").then_some(prep.detail.as_str());
        audit(
            &self.env,
            &self.ctx,
            req,
            &audit_detail(req, &self.ctx.project, safe),
            request_id.as_deref(),
            decision,
            &resp,
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

fn new_request_id() -> String {
    use sha2::{Digest, Sha256};
    let seed = format!("{:?}-{}", Instant::now(), std::process::id());
    let h = crate::manifest::hex(&Sha256::digest(seed.as_bytes()));
    format!("r-{}-{}", state::now(), &h[..8])
}

/// Serve requests on `socket` until the process exits.
pub fn serve(socket: &Path, broker: Broker) -> Result<()> {
    let _ = std::fs::remove_file(socket);
    if let Some(d) = socket.parent() {
        crate::privfs::dir(d)?;
    }
    let listener = UnixListener::bind(socket)
        .with_context(|| format!("cannot listen on {}", socket.display()))?;
    let broker = Arc::new(broker);
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
