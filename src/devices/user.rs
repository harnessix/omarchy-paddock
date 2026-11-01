//! User devices: `[devices.NAME]` tables of the user configuration.
//!
//! ```toml
//! [devices.ollama]
//! kind = "http"
//! enabled = true
//! target = "127.0.0.1:11434"     # one loopback address and port
//! methods = ["GET", "POST"]
//! paths = ["/api/"]               # path prefixes
//! max_body = "1M"
//!
//! [devices.jq]
//! kind = "command"
//! enabled = true
//! command = ["jq", "-c", "."]     # fixed arguments; the request is stdin
//! max_body = "256K"
//! timeout = 30
//! ```
//!
//! A device is off until its table says `enabled = true`. An off device is
//! still a file in `/dev/paddock`: it answers with a "disabled" status. The
//! first request of a sandbox puts a card on the host ("allow the device
//! `ollama` for this sandbox"). "Allow for this session" lets every later
//! request of this sandbox through without a card.
//!
//! - `http` (this file): the request is a small HTTP request. paddock checks
//!   the method and the path prefix, drops `Authorization`, `Cookie` and
//!   hop-by-hop headers, and sends it to `target` only. `target` must be a
//!   loopback address.
//! - `command` (command.rs): the request is the standard input of a fixed
//!   command, which runs through the `exec` isolation (execbox.rs).
//!
//! The audit log keeps the device, the method and path (http) or the byte
//! counts (command), the status and the duration. Never a body.

use super::{Call, DevError, Device, DeviceCtx, Info, Limits};
use crate::broker::{self, Answer};
use anyhow::{bail, Context, Result};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The largest `max_body` a device can set.
pub const MAX_BODY_CAP: usize = 64 << 20;
/// The default `max_body`.
pub const DEFAULT_MAX_BODY: usize = 1 << 20;
/// The default and the largest `timeout`, in seconds.
pub const DEFAULT_TIMEOUT: u64 = 30;
pub const MAX_TIMEOUT: u64 = 3600;
/// The longest wait for the user's answer on the card.
pub const APPROVAL_WAIT: u64 = 120;
/// Room for the request line and headers of an HTTP request or response.
const HEAD_ROOM: usize = 16 * 1024;
/// Headers of one request.
const MAX_HEADERS: usize = 64;

/// One `[devices.NAME]` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserDev {
    pub name: String,
    pub enabled: bool,
    pub max_body: usize,
    pub timeout: u64,
    pub kind: UserKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserKind {
    Http {
        target: SocketAddr,
        methods: Vec<String>,
        paths: Vec<String>,
    },
    Command {
        command: Vec<String>,
        network: bool,
    },
}

impl UserDev {
    pub fn kind_name(&self) -> &'static str {
        match self.kind {
            UserKind::Http { .. } => "http",
            UserKind::Command { .. } => "command",
        }
    }

    /// The device to serve: the device itself, or the stub of an off one.
    pub fn build(&self, ctx: &DeviceCtx, on: bool) -> Arc<dyn Device> {
        if !on {
            return Arc::new(Disabled(self.clone()));
        }
        match self.kind {
            UserKind::Http { .. } => Arc::new(Http::new(self.clone(), ctx.clone())),
            UserKind::Command { .. } => Arc::new(super::command::Command::new(
                self.clone(),
                ctx.clone(),
                None,
            )),
        }
    }

    /// The limits for the catalog and the file system.
    pub fn limits(&self) -> Limits {
        Limits {
            max_request: self.max_body + HEAD_ROOM,
            max_response: self.max_body + HEAD_ROOM,
            deadline_seconds: APPROVAL_WAIT + self.timeout + 5,
        }
    }

    /// What the catalog shows about the configuration.
    pub fn details(&self) -> serde_json::Value {
        let mut v =
            serde_json::json!({ "kind": self.kind_name(), "timeout_seconds": self.timeout });
        match &self.kind {
            UserKind::Http {
                target,
                methods,
                paths,
            } => {
                v["target"] = target.to_string().into();
                v["methods"] = serde_json::json!(methods);
                v["paths"] = serde_json::json!(paths);
            }
            UserKind::Command { command, network } => {
                v["command"] = serde_json::json!(command);
                v["network"] = (*network).into();
            }
        }
        v
    }

    /// One line for the approval card.
    pub fn describe(&self) -> String {
        match &self.kind {
            UserKind::Http {
                target,
                methods,
                paths,
            } => format!(
                "HTTP to {target}: methods {}, paths {}",
                methods.join(", "),
                paths.join(", ")
            ),
            UserKind::Command { command, network } => format!(
                "the command `{}` on the host, the request on its standard input{}",
                command.join(" "),
                if *network {
                    ", with network"
                } else {
                    ", no network"
                }
            ),
        }
    }

    pub fn info(&self) -> Info {
        match &self.kind {
            UserKind::Http { methods, paths, .. } => http_info(self, methods, paths),
            UserKind::Command { .. } => super::command::info(self),
        }
    }
}

/// A size: a number of bytes, or a number with K, M or G (1024 based).
pub fn parse_size(s: &str) -> Result<usize> {
    let t = s.trim();
    let (num, mult) = match t.char_indices().last() {
        Some((i, 'K' | 'k')) => (&t[..i], 1usize << 10),
        Some((i, 'M' | 'm')) => (&t[..i], 1 << 20),
        Some((i, 'G' | 'g')) => (&t[..i], 1 << 30),
        _ => (t, 1),
    };
    let n: usize = num
        .trim()
        .parse()
        .with_context(|| format!("{s:?} is not a size like 512, 256K or 1M"))?;
    let v = n
        .checked_mul(mult)
        .with_context(|| format!("{s:?} is too large"))?;
    if v == 0 || v > MAX_BODY_CAP {
        bail!("{s:?}: a size from 1 byte to 64M");
    }
    Ok(v)
}

/// A name for `/dev/paddock/NAME`: lower case letters, digits, `-` and
/// `_`, not the name of a built-in file.
pub fn check_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 32
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        && name.starts_with(|c: char| c.is_ascii_lowercase());
    if !ok {
        bail!("{name:?}: a device name has 1 to 32 lower case letters, digits, - or _, and starts with a letter");
    }
    if super::BUILTIN.contains(&name) || ["mount", "gw", "secret"].contains(&name) {
        bail!("{name:?} is a built-in name");
    }
    Ok(())
}

fn methods_ok(m: &str) -> bool {
    (1..=16).contains(&m.len()) && m.chars().all(|c| c.is_ascii_uppercase())
}

impl UserDev {
    /// Parse `[devices.NAME]`.
    pub fn parse(name: &str, t: &toml::Table) -> Result<Self> {
        check_name(name)?;
        let ctx = || format!("[devices.{name}]");
        let str_list = |k: &str| -> Result<Option<Vec<String>>> {
            let Some(v) = t.get(k) else { return Ok(None) };
            let a = v
                .as_array()
                .with_context(|| format!("{}: {k} must be a list of strings", ctx()))?;
            a.iter()
                .map(|x| {
                    x.as_str()
                        .map(str::to_string)
                        .with_context(|| format!("{}: {k} must be a list of strings", ctx()))
                })
                .collect::<Result<Vec<_>>>()
                .map(Some)
        };
        let kind = t
            .get("kind")
            .and_then(|k| k.as_str())
            .with_context(|| format!("{}: kind must be \"http\" or \"command\"", ctx()))?;
        let allowed: &[&str] = match kind {
            "http" => &[
                "kind", "enabled", "target", "methods", "paths", "max_body", "timeout",
            ],
            "command" => &[
                "kind", "enabled", "command", "network", "max_body", "timeout",
            ],
            other => bail!("{}: unknown kind {other:?}; use http or command", ctx()),
        };
        for k in t.keys() {
            if !allowed.contains(&k.as_str()) {
                bail!("{}: unknown key {k} for kind {kind}", ctx());
            }
        }
        let flag = |k: &str| -> Result<bool> {
            match t.get(k) {
                None => Ok(false),
                Some(v) => v
                    .as_bool()
                    .with_context(|| format!("{}: {k} must be true or false", ctx())),
            }
        };
        let enabled = flag("enabled")?;
        let max_body = match t.get("max_body") {
            None => DEFAULT_MAX_BODY,
            Some(toml::Value::String(s)) => parse_size(s).with_context(ctx)?,
            Some(toml::Value::Integer(n)) => parse_size(&n.to_string()).with_context(ctx)?,
            Some(_) => bail!("{}: max_body must be a size like \"256K\"", ctx()),
        };
        let timeout = match t.get("timeout") {
            None => DEFAULT_TIMEOUT,
            Some(v) => v
                .as_integer()
                .filter(|n| (1..=MAX_TIMEOUT as i64).contains(n))
                .with_context(|| format!("{}: timeout is 1 to {MAX_TIMEOUT} seconds", ctx()))?
                as u64,
        };
        let kind = if kind == "http" {
            let raw = t
                .get("target")
                .and_then(|v| v.as_str())
                .with_context(|| format!("{}: target must be ADDRESS:PORT", ctx()))?;
            let target: SocketAddr = raw.parse().with_context(|| {
                format!(
                    "{}: target {raw:?} must be an IP address and a port, for example 127.0.0.1:11434",
                    ctx()
                )
            })?;
            if !target.ip().is_loopback() {
                bail!(
                    "{}: target {raw} is not a loopback address; an http device reaches 127.0.0.0/8 or ::1 only",
                    ctx()
                );
            }
            if target.port() == 0 {
                bail!("{}: target needs a port", ctx());
            }
            let methods = str_list("methods")?.unwrap_or_else(|| vec!["GET".into()]);
            if methods.is_empty() || !methods.iter().all(|m| methods_ok(m)) {
                bail!(
                    "{}: methods must be upper case HTTP methods, for example [\"GET\", \"POST\"]",
                    ctx()
                );
            }
            let paths = str_list("paths")?.unwrap_or_else(|| vec!["/".into()]);
            if paths.is_empty() || !paths.iter().all(|p| p.starts_with('/') && path_ok(p)) {
                bail!(
                    "{}: paths must be prefixes that start with /, for example [\"/api/\"]",
                    ctx()
                );
            }
            UserKind::Http {
                target,
                methods,
                paths,
            }
        } else {
            let command = str_list("command")?.unwrap_or_default();
            if command.is_empty() || command[0].is_empty() {
                bail!(
                    "{}: command must be a list with the program first, for example [\"jq\", \"-c\", \".\"]",
                    ctx()
                );
            }
            if command.iter().any(|a| a.contains('\0')) {
                bail!("{}: command holds a NUL byte", ctx());
            }
            UserKind::Command {
                command,
                network: flag("network")?,
            }
        };
        Ok(Self {
            name: name.to_string(),
            enabled,
            max_body,
            timeout,
            kind,
        })
    }
}

/// The approval of a device for one sandbox: one card at the first
/// request, and the session answer for the rest.
pub struct Gate {
    ctx: DeviceCtx,
    name: String,
    detail: String,
    /// The session answer: allow (true) or deny (false) without a card.
    session: Mutex<Option<bool>>,
}

impl Gate {
    pub fn new(ctx: DeviceCtx, def: &UserDev) -> Self {
        Self {
            ctx,
            name: def.name.clone(),
            detail: def.describe(),
            session: Mutex::new(None),
        }
    }

    /// Ask the user when no session answer exists. Requests that come
    /// during the card wait for the same answer. The decision text is for
    /// the audit log.
    pub fn check(&self, call: &Call) -> Result<&'static str, DevError> {
        let mut s = self.session.lock().unwrap();
        match *s {
            Some(true) => return Ok("allowed for the session"),
            Some(false) => return Err(DevError::Denied),
            None => {}
        }
        let left = call.deadline.saturating_duration_since(Instant::now());
        let wait = left
            .saturating_sub(Duration::from_secs(1))
            .min(Duration::from_secs(APPROVAL_WAIT));
        let now = crate::state::now();
        let p = broker::Pending {
            id: broker::new_request_id(),
            sandbox: self.ctx.sandbox.clone(),
            harness: self.ctx.harness.clone(),
            project: self.ctx.project.clone(),
            capability: "device".into(),
            summary: format!("allow the device `{}` for this sandbox", self.name),
            detail: self.detail.clone(),
            reason: String::new(),
            created: now,
            expires: now + wait.as_secs(),
            session_allowed: true,
            started_by: self.ctx.started_by.clone(),
            choices: Vec::new(),
        };
        match self.ctx.approver.ask(&self.ctx.env, &p, wait) {
            Some(Answer::Session) => {
                *s = Some(true);
                Ok("allowed for the session")
            }
            Some(Answer::Once) => Ok("allowed once"),
            Some(Answer::DenySession) => {
                *s = Some(false);
                Err(DevError::Denied)
            }
            Some(Answer::Deny) | None => Err(DevError::Denied),
        }
    }

    /// One audit line per request: never a body.
    pub fn audit(&self, detail: &str, decision: &str, status: &str, start: Instant) {
        let mut line = serde_json::json!({
            "time": crate::state::now(),
            "sandbox": self.ctx.sandbox,
            "harness": self.ctx.harness,
            "project": self.ctx.project,
            "capability": "device",
            "device": self.name,
            "detail": detail,
            "decision": decision,
            "status": status,
            "duration_ms": start.elapsed().as_millis() as u64,
        });
        if let Some(p) = self.ctx.started_by.last() {
            line["parent"] = p.clone().into();
            line["chain"] = serde_json::json!(self.ctx.started_by);
        }
        broker::audit_line(&self.ctx.env, &line);
    }
}

/// The text of an off device.
fn disabled_text(name: &str) -> String {
    format!(
        "The device {name} is disabled. The user turns it on with enabled = true in [devices.{name}] of ~/.config/paddock/config.toml on the host."
    )
}

/// The file of a device that the user defined but did not turn on.
pub struct Disabled(pub UserDev);

impl Device for Disabled {
    fn name(&self) -> &str {
        &self.0.name
    }
    fn info(&self) -> Info {
        self.0.info()
    }
    fn state(&self) -> &'static str {
        "off"
    }
    fn details(&self) -> Option<serde_json::Value> {
        let mut d = self.0.details();
        d["how_to_enable"] = format!(
            "the user sets enabled = true in [devices.{}] of ~/.config/paddock/config.toml on the host",
            self.0.name
        )
        .into();
        Some(d)
    }
    fn call(&self, _call: &Call, _request: &[u8]) -> Result<Vec<u8>, DevError> {
        let text = disabled_text(&self.0.name);
        Ok(match self.0.kind {
            UserKind::Http { .. } => paddock_answer(403, "Forbidden", "disabled", &text),
            UserKind::Command { .. } => {
                format!("status: disabled\nmessage: {text}\n\n").into_bytes()
            }
        })
    }
}

fn http_info(def: &UserDev, methods: &[String], paths: &[String]) -> Info {
    Info {
        request: format!(
            "an HTTP request as text: the first line METHOD PATH (methods {}; the path starts with one of {}), then header lines NAME: VALUE, an empty line, and the body (at most {} bytes). paddock sets Host, Content-Length and Connection, and drops Authorization, Cookie and hop-by-hop headers.",
            methods.join(", "),
            paths.join(", "),
            def.max_body
        ),
        response: format!(
            "the HTTP response as text: the status line HTTP/1.1 CODE REASON, header lines, an empty line, and the body (at most {} bytes, chunked bodies decoded). A Paddock-Status header means that paddock answered itself: disabled, refused (method or path not allowed) or unreachable. EACCES: the user declined the device. Lines end with LF.",
            def.max_body
        ),
        limits: def.limits(),
    }
}

/// A response that paddock makes itself, in the format of the device.
fn paddock_answer(code: u16, reason: &str, status: &str, text: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {code} {reason}\nPaddock-Status: {status}\nContent-Type: text/plain\nContent-Length: {}\n\n{text}\n",
        text.len() + 1
    )
    .into_bytes()
}

/// A parsed request of an http device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpReq {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

fn token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(c)
}

/// Parse the request format: `METHOD PATH`, header lines, an empty line,
/// the body. Lines end with LF or CRLF. Without an empty line the whole
/// request is the head and the body is empty.
pub fn parse_http(raw: &[u8], max_body: usize) -> Result<HttpReq, DevError> {
    let (head, body) = match find(raw, b"\n\n")
        .map(|i| (i, 2))
        .into_iter()
        .chain(find(raw, b"\r\n\r\n").map(|i| (i, 4)))
        .min_by_key(|(i, _)| *i)
    {
        Some((i, n)) => (&raw[..i], &raw[i + n..]),
        None => (raw, &raw[raw.len()..]),
    };
    if head.len() > HEAD_ROOM {
        return Err(DevError::Invalid);
    }
    if body.len() > max_body {
        return Err(DevError::TooLarge);
    }
    let head = std::str::from_utf8(head).map_err(|_| DevError::Invalid)?;
    let mut lines = head.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l));
    let first = lines.next().unwrap_or("").trim();
    let mut parts = first.split(' ').filter(|p| !p.is_empty());
    let method = parts.next().ok_or(DevError::Invalid)?;
    let path = parts.next().ok_or(DevError::Invalid)?;
    match parts.next() {
        None | Some("HTTP/1.1") | Some("HTTP/1.0") => {}
        Some(_) => return Err(DevError::Invalid),
    }
    if parts.next().is_some() || !methods_ok(method) {
        return Err(DevError::Invalid);
    }
    let mut headers = Vec::new();
    for l in lines {
        if l.is_empty() {
            continue;
        }
        let (n, v) = l.split_once(':').ok_or(DevError::Invalid)?;
        let v = v.trim();
        if n.is_empty()
            || !n.chars().all(token_char)
            || v.chars().any(|c| c.is_control() && c != '\t')
        {
            return Err(DevError::Invalid);
        }
        headers.push((n.to_string(), v.to_string()));
        if headers.len() > MAX_HEADERS {
            return Err(DevError::Invalid);
        }
    }
    Ok(HttpReq {
        method: method.to_string(),
        path: path.to_string(),
        headers,
        body: body.to_vec(),
    })
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// A path without tricks that a server could resolve outside a prefix:
/// visible ASCII only, no `.` or `..` segments, no `//`, no encoded dot,
/// slash or backslash, no fragment.
fn path_ok(p: &str) -> bool {
    if !p.starts_with('/') || !p.chars().all(|c| c.is_ascii_graphic()) || p.contains(['#', '\\']) {
        return false;
    }
    let path = p.split('?').next().unwrap_or("");
    let lower = path.to_ascii_lowercase();
    if ["%2e", "%2f", "%5c", "%00"]
        .iter()
        .any(|x| lower.contains(x))
        || path.contains("//")
    {
        return false;
    }
    !path.split('/').any(|s| s == "." || s == "..")
}

/// Why the device refuses this request, or None when it may go.
pub fn refusal(req: &HttpReq, methods: &[String], paths: &[String]) -> Option<String> {
    if !methods.contains(&req.method) {
        return Some(format!(
            "the method {} is not allowed; allowed: {}",
            req.method,
            methods.join(", ")
        ));
    }
    if !path_ok(&req.path) {
        return Some("the path has characters or segments that the device does not allow".into());
    }
    let path = req.path.split('?').next().unwrap_or("");
    if !paths.iter().any(|p| path.starts_with(p.as_str())) {
        return Some(format!(
            "the path is outside the allowed prefixes: {}",
            paths.join(", ")
        ));
    }
    None
}

/// Headers that never pass: hop-by-hop headers, credentials of the
/// sandbox, and the ones that paddock sets itself.
const DROP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-connection",
    "transfer-encoding",
    "te",
    "trailer",
    "upgrade",
    "proxy-authorization",
    "proxy-authenticate",
    "authorization",
    "cookie",
    "host",
    "content-length",
    "expect",
];

/// The headers that pass, without `DROP` and without the headers that a
/// `Connection` header names.
pub fn strip_headers(headers: &[(String, String)]) -> Vec<(String, String)> {
    let named: Vec<String> = headers
        .iter()
        .filter(|(n, _)| n.eq_ignore_ascii_case("connection"))
        .flat_map(|(_, v)| v.split(',').map(|x| x.trim().to_ascii_lowercase()))
        .collect();
    headers
        .iter()
        .filter(|(n, _)| {
            let l = n.to_ascii_lowercase();
            !DROP.contains(&l.as_str()) && !named.contains(&l)
        })
        .cloned()
        .collect()
}

/// `/dev/paddock/NAME` for an `http` device.
pub struct Http {
    def: UserDev,
    gate: Gate,
}

impl Http {
    pub fn new(def: UserDev, ctx: DeviceCtx) -> Self {
        let gate = Gate::new(ctx, &def);
        Self { def, gate }
    }

    fn send(&self, call: &Call, target: SocketAddr, req: &HttpReq) -> Result<Vec<u8>, Upstream> {
        let end = Instant::now() + Duration::from_secs(self.def.timeout);
        let connect = Duration::from_secs(5).min(Duration::from_secs(self.def.timeout));
        let mut s = TcpStream::connect_timeout(&target, connect)
            .map_err(|e| Upstream::Unreachable(format!("cannot connect to {target}: {e}")))?;
        let _ = s.set_nodelay(true);
        let mut out = format!("{} {} HTTP/1.1\r\nHost: {target}\r\n", req.method, req.path);
        for (n, v) in strip_headers(&req.headers) {
            out.push_str(&format!("{n}: {v}\r\n"));
        }
        if !req.body.is_empty() || matches!(req.method.as_str(), "POST" | "PUT" | "PATCH") {
            out.push_str(&format!("Content-Length: {}\r\n", req.body.len()));
        }
        out.push_str("Connection: close\r\n\r\n");
        let mut bytes = out.into_bytes();
        bytes.extend_from_slice(&req.body);
        s.set_write_timeout(Some(Duration::from_secs(self.def.timeout)))
            .ok();
        s.write_all(&bytes)
            .map_err(|e| Upstream::Unreachable(format!("cannot send to {target}: {e}")))?;
        s.set_read_timeout(Some(Duration::from_millis(200))).ok();
        let limit = self.def.max_body + HEAD_ROOM;
        let mut buf = Vec::new();
        let mut chunk = [0u8; 16 * 1024];
        loop {
            if let Some(r) = complete(&buf, req.method == "HEAD", false) {
                return r;
            }
            if call.cancelled() {
                return Err(Upstream::Cancelled);
            }
            if Instant::now() >= end {
                return Err(Upstream::Unreachable(format!(
                    "{target} did not answer in {} seconds",
                    self.def.timeout
                )));
            }
            match s.read(&mut chunk) {
                Ok(0) => {
                    return complete(&buf, req.method == "HEAD", true).unwrap_or_else(|| {
                        Err(Upstream::Unreachable(format!(
                            "{target} closed the connection before a full response"
                        )))
                    })
                }
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    if buf.len() > limit {
                        return Err(Upstream::TooLarge);
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock
                            | std::io::ErrorKind::TimedOut
                            | std::io::ErrorKind::Interrupted
                    ) => {}
                Err(e) => {
                    return Err(Upstream::Unreachable(format!(
                        "cannot read from {target}: {e}"
                    )))
                }
            }
        }
    }
}

/// Why a request to the service failed.
#[derive(Debug)]
pub enum Upstream {
    Unreachable(String),
    TooLarge,
    Cancelled,
}

/// The response for the agent when `buf` holds a whole HTTP response, or
/// None when more bytes must come. `eof`: the server closed the connection.
fn complete(buf: &[u8], head_only: bool, eof: bool) -> Option<Result<Vec<u8>, Upstream>> {
    let bad = |m: &str| Some(Err(Upstream::Unreachable(m.to_string())));
    let mut start = 0;
    loop {
        let rest = &buf[start..];
        let end = find(rest, b"\r\n\r\n")
            .map(|i| (i, 4))
            .into_iter()
            .chain(find(rest, b"\n\n").map(|i| (i, 2)))
            .min_by_key(|(i, _)| *i);
        let Some((hend, n)) = end else {
            return if eof {
                bad("the service sent no complete response head")
            } else {
                None
            };
        };
        let head = String::from_utf8_lossy(&rest[..hend]).into_owned();
        let body = &rest[hend + n..];
        let mut lines = head.split('\n').map(|l| l.trim_end_matches('\r'));
        let status = lines.next().unwrap_or("");
        let mut sp = status.splitn(3, ' ');
        let (ver, code) = (sp.next().unwrap_or(""), sp.next().unwrap_or(""));
        let reason = sp.next().unwrap_or("").trim();
        let Ok(code) = code.parse::<u16>() else {
            return bad("the service sent no HTTP status line");
        };
        if !ver.starts_with("HTTP/") {
            return bad("the service sent no HTTP status line");
        }
        let headers: Vec<(String, String)> = lines
            .filter_map(|l| l.split_once(':'))
            .map(|(a, b)| (a.trim().to_string(), b.trim().to_string()))
            .collect();
        if (100..200).contains(&code) && code != 101 {
            // An interim response: the real one follows.
            start += hend + n;
            continue;
        }
        let get = |name: &str| {
            headers
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str())
        };
        let no_body = head_only || code == 204 || code == 304 || code == 101;
        let body: Vec<u8> = if no_body {
            Vec::new()
        } else if get("transfer-encoding")
            .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"))
        {
            match dechunk(body) {
                Some(Ok(b)) => b,
                Some(Err(())) => return bad("the service sent a broken chunked body"),
                None if eof => return bad("the service closed the connection in a chunked body"),
                None => return None,
            }
        } else if let Some(len) = get("content-length") {
            let Ok(len) = len.parse::<usize>() else {
                return bad("the service sent a bad Content-Length");
            };
            if body.len() < len {
                return if eof {
                    bad("the service closed the connection before the end of the body")
                } else {
                    None
                };
            }
            body[..len].to_vec()
        } else if eof {
            body.to_vec()
        } else {
            return None;
        };
        let mut out = format!("HTTP/1.1 {code} {}\n", broker::clean_text(reason).trim());
        for (n, v) in strip_headers(&headers) {
            if n.eq_ignore_ascii_case("set-cookie") || !n.chars().all(token_char) {
                continue;
            }
            let v: String = v.chars().filter(|c| !c.is_control()).collect();
            out.push_str(&format!("{n}: {v}\n"));
        }
        out.push_str(&format!("Content-Length: {}\n\n", body.len()));
        let mut out = out.into_bytes();
        out.extend_from_slice(&body);
        return Some(Ok(out));
    }
}

/// Decode a chunked body: None while it is incomplete.
fn dechunk(mut data: &[u8]) -> Option<Result<Vec<u8>, ()>> {
    let mut out = Vec::new();
    loop {
        let eol = find(data, b"\n")?;
        let line = std::str::from_utf8(&data[..eol]).map_err(|_| ());
        let Ok(line) = line else {
            return Some(Err(()));
        };
        let size = line.trim_end_matches('\r');
        let size = size.split(';').next().unwrap_or("").trim();
        let Ok(size) = usize::from_str_radix(size, 16) else {
            return Some(Err(()));
        };
        data = &data[eol + 1..];
        if size == 0 {
            // Trailers end with an empty line. paddock drops them.
            loop {
                let eol = find(data, b"\n")?;
                let empty = data[..eol].iter().all(|b| *b == b'\r');
                data = &data[eol + 1..];
                if empty {
                    return Some(Ok(out));
                }
            }
        }
        if data.len() < size {
            return None;
        }
        out.extend_from_slice(&data[..size]);
        data = &data[size..];
        if data.starts_with(b"\r\n") {
            data = &data[2..];
        } else if data.starts_with(b"\n") {
            data = &data[1..];
        } else if data.is_empty() || data == b"\r" {
            return None;
        } else {
            return Some(Err(()));
        }
    }
}

impl Device for Http {
    fn name(&self) -> &str {
        &self.def.name
    }
    fn info(&self) -> Info {
        self.def.info()
    }
    fn details(&self) -> Option<serde_json::Value> {
        Some(self.def.details())
    }
    fn call(&self, call: &Call, request: &[u8]) -> Result<Vec<u8>, DevError> {
        let UserKind::Http {
            target,
            methods,
            paths,
        } = &self.def.kind
        else {
            return Err(DevError::Failed);
        };
        let start = Instant::now();
        let req = parse_http(request, self.def.max_body)?;
        // The query can hold private values: the audit keeps the path only.
        let detail = format!(
            "{} {}",
            req.method,
            broker::clean_text(req.path.split('?').next().unwrap_or(""))
        );
        if let Some(why) = refusal(&req, methods, paths) {
            self.gate.audit(&detail, "refused", "refused", start);
            return Ok(paddock_answer(403, "Forbidden", "refused", &why));
        }
        let decision = match self.gate.check(call) {
            Ok(d) => d,
            Err(e) => {
                self.gate.audit(&detail, "denied", "denied", start);
                return Err(e);
            }
        };
        match self.send(call, *target, &req) {
            Ok(out) => {
                let code = String::from_utf8_lossy(out.get(9..12).unwrap_or_default()).to_string();
                self.gate.audit(&detail, decision, &code, start);
                Ok(out)
            }
            Err(Upstream::Unreachable(why)) => {
                self.gate.audit(&detail, decision, "unreachable", start);
                Ok(paddock_answer(502, "Bad Gateway", "unreachable", &why))
            }
            Err(Upstream::TooLarge) => {
                self.gate.audit(&detail, decision, "too large", start);
                Err(DevError::TooLarge)
            }
            Err(Upstream::Cancelled) => {
                self.gate.audit(&detail, decision, "cancelled", start);
                Err(DevError::Cancelled)
            }
        }
    }
}

#[cfg(test)]
#[path = "user_tests.rs"]
mod tests;
