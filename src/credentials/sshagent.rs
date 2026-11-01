//! The ssh agent channel (`via = "agent"` with `keys`): a filtering
//! ssh-agent on a Unix socket of one sandbox. The paddock process of the
//! sandbox serves it and forwards to the user's ssh-agent on the host
//! (`SSH_AUTH_SOCK` of the paddock process). The private keys stay in the
//! host agent.
//!
//! The proxy knows two requests. A list of identities returns only the
//! configured public keys that the host agent holds. A signature request
//! passes only for a configured key, after the user allows it when the
//! credential has `confirm = "each"`. Every other request (add, remove,
//! lock, extensions) gets a failure and never reaches the host agent. A
//! message over 256 KiB or a malformed message gets a failure too.

use super::{Confirm, Credential};
use crate::env::HostEnv;
use anyhow::{bail, Context as _, Result};
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Where the socket appears inside the sandbox.
pub const SOCKET_IN_SANDBOX: &str = "/run/paddock/ssh-agent.sock";
/// The capability name on the approval card of one signature.
pub const SIGN_CAPABILITY: &str = "ssh-sign";
/// The largest message in either direction.
pub const MAX_MESSAGE: usize = 256 * 1024;
const MAX_CONNECTIONS: usize = 32;
/// The most identities that paddock reads from the host agent.
const MAX_IDENTITIES: u32 = 1024;

pub const FAILURE: u8 = 5;
pub const REQUEST_IDENTITIES: u8 = 11;
pub const IDENTITIES_ANSWER: u8 = 12;
pub const SIGN_REQUEST: u8 = 13;
pub const SIGN_RESPONSE: u8 = 14;

/// One request from the sandbox.
#[derive(Debug, PartialEq, Eq)]
pub enum Request {
    Identities,
    Sign {
        blob: Vec<u8>,
        data: Vec<u8>,
        flags: u32,
    },
    /// Any other type: it never leaves paddock.
    Other(u8),
}

struct Cursor<'a>(&'a [u8]);

impl<'a> Cursor<'a> {
    fn u32(&mut self) -> Result<u32> {
        if self.0.len() < 4 {
            bail!("short message");
        }
        let (a, b) = self.0.split_at(4);
        self.0 = b;
        Ok(u32::from_be_bytes([a[0], a[1], a[2], a[3]]))
    }
    fn string(&mut self) -> Result<&'a [u8]> {
        let n = self.u32()? as usize;
        if self.0.len() < n {
            bail!("short message");
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }
    fn end(&self) -> Result<()> {
        if !self.0.is_empty() {
            bail!("extra bytes at the end of the message");
        }
        Ok(())
    }
}

fn put_string(out: &mut Vec<u8>, s: &[u8]) {
    out.extend_from_slice(&(s.len() as u32).to_be_bytes());
    out.extend_from_slice(s);
}

/// The body of one message (type and contents, without the length).
pub fn parse_request(body: &[u8]) -> Result<Request> {
    let Some((&kind, rest)) = body.split_first() else {
        bail!("empty message");
    };
    let mut c = Cursor(rest);
    match kind {
        REQUEST_IDENTITIES => {
            c.end()?;
            Ok(Request::Identities)
        }
        SIGN_REQUEST => {
            let blob = c.string()?.to_vec();
            let data = c.string()?.to_vec();
            let flags = c.u32()?;
            c.end()?;
            if blob.is_empty() {
                bail!("empty key blob");
            }
            Ok(Request::Sign { blob, data, flags })
        }
        other => Ok(Request::Other(other)),
    }
}

/// The identities in an answer of the host agent: key blob and comment.
pub fn parse_identities(body: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let Some((&kind, rest)) = body.split_first() else {
        bail!("empty message");
    };
    if kind != IDENTITIES_ANSWER {
        bail!("the host agent answered with type {kind}");
    }
    let mut c = Cursor(rest);
    let n = c.u32()?;
    if n > MAX_IDENTITIES {
        bail!("the host agent lists {n} identities");
    }
    let mut out = Vec::new();
    for _ in 0..n {
        let blob = c.string()?.to_vec();
        let comment = c.string()?.to_vec();
        out.push((blob, comment));
    }
    c.end()?;
    Ok(out)
}

pub fn identities_answer(ids: &[(Vec<u8>, Vec<u8>)]) -> Vec<u8> {
    let mut out = vec![IDENTITIES_ANSWER];
    out.extend_from_slice(&(ids.len() as u32).to_be_bytes());
    for (b, c) in ids {
        put_string(&mut out, b);
        put_string(&mut out, c);
    }
    out
}

fn sign_request(blob: &[u8], data: &[u8], flags: u32) -> Vec<u8> {
    let mut out = vec![SIGN_REQUEST];
    put_string(&mut out, blob);
    put_string(&mut out, data);
    out.extend_from_slice(&flags.to_be_bytes());
    out
}

/// Whether a body from the host agent is a well-formed signature answer.
fn valid_sign_response(body: &[u8]) -> bool {
    match body.split_first() {
        Some((&SIGN_RESPONSE, rest)) => {
            let mut c = Cursor(rest);
            c.string().is_ok() && c.end().is_ok()
        }
        _ => false,
    }
}

/// Read one message. None: the peer closed the connection before a new
/// message. A length over MAX_MESSAGE or zero is an error.
pub fn read_message(r: &mut impl Read) -> Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    let mut got = 0;
    while got < 4 {
        match r.read(&mut len[got..])? {
            0 if got == 0 => return Ok(None),
            0 => bail!("short message"),
            n => got += n,
        }
    }
    let n = u32::from_be_bytes(len) as usize;
    if n == 0 {
        bail!("empty message");
    }
    if n > MAX_MESSAGE {
        bail!("message of {n} bytes is over the limit of {MAX_MESSAGE}");
    }
    let mut body = vec![0u8; n];
    r.read_exact(&mut body).context("short message")?;
    Ok(Some(body))
}

pub fn write_message(w: &mut impl Write, body: &[u8]) -> Result<()> {
    let mut out = Vec::with_capacity(body.len() + 4);
    put_string(&mut out, body);
    w.write_all(&out)?;
    Ok(())
}

/// `SHA256:...` as ssh-keygen prints it.
pub fn fingerprint(blob: &[u8]) -> String {
    use base64::Engine as _;
    use sha2::Digest as _;
    let d = sha2::Sha256::digest(blob);
    format!(
        "SHA256:{}",
        base64::engine::general_purpose::STANDARD_NO_PAD.encode(d)
    )
}

/// One key that a sandbox may use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Key {
    /// The credential it belongs to.
    pub name: String,
    pub blob: Vec<u8>,
    pub fingerprint: String,
    /// The comment of the public key file.
    pub comment: String,
    pub confirm: Confirm,
}

/// A public key line: `TYPE BASE64 [COMMENT]`.
pub fn parse_public_key(text: &str) -> Result<(Vec<u8>, String)> {
    use base64::Engine as _;
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))
        .context("no public key in the file")?;
    let mut parts = line.splitn(3, char::is_whitespace);
    let kind = parts.next().unwrap_or_default();
    let b64 = parts.next().context("no key data after the key type")?;
    let comment = parts.next().unwrap_or("").trim().to_string();
    let blob = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .context("the key data is not base64")?;
    // The blob starts with its own type name.
    let mut c = Cursor(&blob);
    if c.string().ok() != Some(kind.as_bytes()) {
        bail!("the key data does not match the key type {kind}");
    }
    Ok((blob, comment))
}

/// `~/` is the home folder of the user.
pub fn key_path(env: &HostEnv, p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => env.home.join(rest),
        None => PathBuf::from(p),
    }
}

/// The keys of the agent credentials, read from their public key files.
pub fn load_keys(env: &HostEnv, creds: &[(String, Credential)]) -> Result<Vec<Key>> {
    let mut out = Vec::new();
    for (name, c) in creds {
        for k in &c.keys {
            let path = key_path(env, k);
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("credential {name}: cannot read the key {k}"))?;
            let (blob, comment) = parse_public_key(&text)
                .with_context(|| format!("credential {name}: {k} is not a public key"))?;
            out.push(Key {
                name: name.clone(),
                fingerprint: fingerprint(&blob),
                blob,
                comment,
                confirm: c.confirm,
            });
        }
    }
    Ok(out)
}

/// Ask the host agent for its identities.
pub fn host_identities(agent: &Path) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let mut s = UnixStream::connect(agent)
        .with_context(|| format!("cannot reach the ssh agent at {}", agent.display()))?;
    s.set_read_timeout(Some(Duration::from_secs(10)))?;
    write_message(&mut s, &[REQUEST_IDENTITIES])?;
    let body = read_message(&mut s)?.context("the ssh agent closed the connection")?;
    parse_identities(&body)
}

/// Check that the host agent is there and holds every key.
pub fn check_host(agent: Option<&Path>, keys: &[Key]) -> Result<()> {
    let agent = agent
        .context("SSH_AUTH_SOCK is not set where paddock started, so there is no ssh agent")?;
    let held = host_identities(agent)?;
    for k in keys {
        if !held.iter().any(|(b, _)| *b == k.blob) {
            bail!(
                "the ssh agent does not hold the key {} ({}); add it with ssh-add",
                k.fingerprint,
                k.comment
            );
        }
    }
    Ok(())
}

/// Asks the user for one signature. True: allowed.
pub type Ask = Arc<dyn Fn(&Key) -> bool + Send + Sync>;
/// Reports one signature request: the key and the result. Never the data.
pub type Report = Arc<dyn Fn(&Key, &str) + Send + Sync>;

pub struct Proxy {
    pub keys: Vec<Key>,
    /// The socket of the host agent.
    pub upstream: PathBuf,
    /// The revoke file of the run (`credentials-revoked`).
    pub revoked: PathBuf,
    pub ask: Ask,
    pub report: Report,
    connections: AtomicUsize,
}

impl Proxy {
    pub fn new(
        keys: Vec<Key>,
        upstream: PathBuf,
        revoked: PathBuf,
        ask: Ask,
        report: Report,
    ) -> Self {
        Self {
            keys,
            upstream,
            revoked,
            ask,
            report,
            connections: AtomicUsize::new(0),
        }
    }

    fn is_revoked(&self, name: &str) -> bool {
        std::fs::read_to_string(&self.revoked)
            .map(|t| t.lines().any(|l| l.trim() == name || l.trim() == "*"))
            .unwrap_or(false)
    }

    /// Serve on `socket` until the process ends.
    pub fn serve(self, socket: &Path) -> Result<()> {
        let _ = std::fs::remove_file(socket);
        let listener = UnixListener::bind(socket)
            .with_context(|| format!("cannot listen on {}", socket.display()))?;
        let me = Arc::new(self);
        std::thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                if me.connections.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
                    me.connections.fetch_sub(1, Ordering::SeqCst);
                    continue;
                }
                let p = me.clone();
                std::thread::spawn(move || {
                    if let Err(e) = p.handle(conn) {
                        log::debug!("ssh agent proxy: {e:#}");
                    }
                    p.connections.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });
        Ok(())
    }

    /// One connection from the sandbox, many requests.
    pub fn handle(&self, mut conn: UnixStream) -> Result<()> {
        let mut upstream: Option<UnixStream> = None;
        loop {
            let body = match read_message(&mut conn) {
                Ok(Some(b)) => b,
                Ok(None) => return Ok(()),
                Err(e) => {
                    // The frame is lost: answer and close.
                    let _ = write_message(&mut conn, &[FAILURE]);
                    return Err(e);
                }
            };
            let answer = match parse_request(&body) {
                Ok(r) => self.answer(r, &mut upstream).unwrap_or_else(|e| {
                    log::debug!("ssh agent proxy: {e:#}");
                    upstream = None;
                    vec![FAILURE]
                }),
                Err(_) => vec![FAILURE],
            };
            write_message(&mut conn, &answer)?;
        }
    }

    fn host<'a>(&self, upstream: &'a mut Option<UnixStream>) -> Result<&'a mut UnixStream> {
        if upstream.is_none() {
            let s = UnixStream::connect(&self.upstream).with_context(|| {
                format!("cannot reach the ssh agent at {}", self.upstream.display())
            })?;
            s.set_read_timeout(Some(Duration::from_secs(300)))?;
            *upstream = Some(s);
        }
        Ok(upstream.as_mut().expect("set"))
    }

    fn ask_host(&self, upstream: &mut Option<UnixStream>, body: &[u8]) -> Result<Vec<u8>> {
        let s = self.host(upstream)?;
        write_message(s, body)?;
        read_message(s)?.context("the ssh agent closed the connection")
    }

    fn answer(&self, r: Request, upstream: &mut Option<UnixStream>) -> Result<Vec<u8>> {
        match r {
            Request::Identities => {
                let held = parse_identities(&self.ask_host(upstream, &[REQUEST_IDENTITIES])?)?;
                let shown: Vec<_> = held
                    .into_iter()
                    .filter(|(b, _)| {
                        self.keys
                            .iter()
                            .any(|k| k.blob == *b && !self.is_revoked(&k.name))
                    })
                    .collect();
                Ok(identities_answer(&shown))
            }
            Request::Sign { blob, data, flags } => {
                let Some(key) = self.keys.iter().find(|k| k.blob == blob) else {
                    let unknown = Key {
                        name: String::new(),
                        fingerprint: fingerprint(&blob),
                        blob,
                        comment: String::new(),
                        confirm: Confirm::Each,
                    };
                    (self.report)(&unknown, "denied: not a configured key");
                    return Ok(vec![FAILURE]);
                };
                if self.is_revoked(&key.name) {
                    (self.report)(key, "denied: revoked");
                    return Ok(vec![FAILURE]);
                }
                if key.confirm == Confirm::Each && !(self.ask)(key) {
                    (self.report)(key, "denied by the user");
                    return Ok(vec![FAILURE]);
                }
                let resp = self.ask_host(upstream, &sign_request(&key.blob, &data, flags))?;
                if valid_sign_response(&resp) {
                    (self.report)(key, "allowed");
                    Ok(resp)
                } else {
                    (self.report)(key, "denied: the ssh agent refused");
                    Ok(vec![FAILURE])
                }
            }
            Request::Other(_) => Ok(vec![FAILURE]),
        }
    }
}

/// The approval card for one signature: the key's fingerprint and comment,
/// never the data to sign.
pub fn approver_ask(
    env: HostEnv,
    spec: crate::spec::SandboxSpec,
    approver: Arc<dyn crate::broker::Approver>,
    wait: Duration,
) -> Ask {
    Arc::new(move |k: &Key| {
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let now = crate::state::now();
        let p = crate::broker::Pending {
            id: format!(
                "ssh-{}-{now}-{}",
                spec.id,
                SEQ.fetch_add(1, Ordering::SeqCst)
            ),
            sandbox: spec.id.clone(),
            harness: spec.harness.clone(),
            project: spec.project.clone(),
            capability: SIGN_CAPABILITY.into(),
            summary: format!("sign with the ssh key {} ({})", k.name, k.fingerprint),
            detail: format!(
                "The sandbox asks the ssh agent for one signature.\nCredential: {}\nKey: {} {}",
                k.name, k.fingerprint, k.comment
            ),
            reason: String::new(),
            created: now,
            expires: now + wait.as_secs(),
            session_allowed: false,
            started_by: Vec::new(),
            choices: Vec::new(),
        };
        matches!(
            approver.ask(&env, &p, wait),
            Some(crate::broker::Answer::Once | crate::broker::Answer::Session)
        )
    })
}

/// One signature request in the audit log of host requests.
pub fn audit(env: &HostEnv, spec: &crate::spec::SandboxSpec, k: &Key, result: &str) {
    crate::broker::audit_line(
        env,
        &serde_json::json!({
            "time": crate::state::now(),
            "sandbox": spec.id,
            "harness": spec.harness,
            "project": spec.project,
            "capability": super::CAPABILITY,
            "detail": format!("agent {} sign {}", if k.name.is_empty() { "-" } else { &k.name }, k.fingerprint),
            "decision": result,
        }),
    );
}

#[cfg(test)]
mod tests;
