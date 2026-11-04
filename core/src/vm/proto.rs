//! The wire protocol between paddock on the host and the guest agent
//! (`paddock vm-agent`). Each message is one JSON object on one line.
//!
//! Control port (1024), host to guest:
//! - `{"op":"run",...}` starts a run (see [`RunRequest`]). The agent answers
//!   `{"reply":"ok"}` when it accepts the run, or `{"reply":"error",...}`.
//!   The connection stays open: the host sends `{"op":"resize",...}` lines,
//!   and the agent sends `{"reply":"exit","code":N}` when the run ends.
//! - `{"op":"connect","token":T,"port":P}` connects to TCP port P on the
//!   guest loopback for the run with token T (a login callback). After
//!   `{"reply":"ok"}`, the connection carries the bytes of that port.
//! - `{"op":"ping"}` answers `{"reply":"ok","runs":N}`.
//!
//! Pty port (1027), host to guest: the first line is
//! `{"op":"attach","token":T}`. After it, the connection carries the
//! terminal or the stdio of the run with token T.
//!
//! Broker (1025) and proxy (1026), guest to host: the first line is the
//! token of the run, without JSON. The `paddock-vm` helper connects the
//! rest to the socket of that run (see `bridge::forward`).

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::PathBuf;

/// The protocol version in each run request. Version 2 carries the user
/// profile. Version 3 carries the owner of the project on the host.
pub const VERSION: u32 = 3;
/// The longest line, without the newline.
pub const LINE_MAX: usize = 256 * 1024;
/// The length of a run token: hex digits of 16 random bytes.
pub const TOKEN_LEN: usize = 32;

/// A message from the host to the agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Run(RunRequest),
    Resize { rows: u16, cols: u16 },
    Attach { token: String },
    Connect { token: String, port: u16 },
    Ping,
}

/// The network of a run in the guest. The VM has no network device: the
/// sandbox reaches the network only through the host proxy (port 1026),
/// which applies the network mode of the host run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuestNet {
    None,
    Proxy,
}

/// One run in the guest: Linux paddock with bwrap, as the guest user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRequest {
    pub v: u32,
    /// The secret of the run. It names the run on the pty, broker and
    /// proxy ports. Only the host and the agent know it.
    pub token: String,
    /// The sandbox id on the host, for the logs.
    pub id: String,
    pub harness: String,
    /// The project folder: the same path in the guest as on the host.
    pub project: PathBuf,
    pub network: GuestNet,
    /// The arguments for the harness.
    #[serde(default)]
    pub args: Vec<String>,
    /// True: the agent gives the run a pty. False: plain stdio.
    pub tty: bool,
    #[serde(default)]
    pub rows: u16,
    #[serde(default)]
    pub cols: u16,
    /// Terminal and locale variables (TERM, LANG, LC_*, COLORTERM, TZ).
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Start without a harness profile check: `paddock shell`.
    #[serde(default)]
    pub shell: bool,
    #[serde(default)]
    pub no_yolo: bool,
    /// The TOML text of the user's own profile of the harness
    /// (`harness.d/NAME.toml` on the host). None for a built-in profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// The uid that owns the project folder on the host. The guest refuses
    /// a project of root. None when the host cannot read it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<u32>,
}

/// A message from the agent to the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum Reply {
    Ok {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        runs: Option<u32>,
    },
    Error {
        error: String,
    },
    Exit {
        code: i32,
    },
}

impl Request {
    /// A short text for the logs, without the token.
    pub fn kind(&self) -> String {
        match self {
            Request::Run(r) => format!("run {} ({}) in {}", r.id, r.harness, r.project.display()),
            Request::Resize { .. } => "resize".into(),
            Request::Attach { .. } => "attach".into(),
            Request::Connect { port, .. } => format!("connect to port {port}"),
            Request::Ping => "ping".into(),
        }
    }
}

impl Reply {
    pub fn ok() -> Self {
        Reply::Ok { runs: None }
    }
    pub fn error(e: impl std::fmt::Display) -> Self {
        Reply::Error {
            error: e.to_string().replace(['\n', '\r'], " "),
        }
    }
}

/// True for a token of [`TOKEN_LEN`] lower-case hex digits.
pub fn valid_token(t: &str) -> bool {
    t.len() == TOKEN_LEN && t.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// A new random token.
pub fn new_token() -> Result<String> {
    let mut buf = [0u8; TOKEN_LEN / 2];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .context("cannot read /dev/urandom")?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

impl RunRequest {
    /// Refuse a request that the agent must not run.
    pub fn check(&self) -> Result<()> {
        if self.v > VERSION {
            bail!(
                "the host speaks VM protocol {} and this VM image speaks {VERSION}: update the VM files with `paddock vm setup` and stop the VM with `paddock vm stop`",
                self.v
            );
        }
        if self.v != VERSION {
            bail!(
                "the host speaks VM protocol {} and this VM image speaks {VERSION}: install the VM files of this paddock with `paddock vm setup` and stop the VM with `paddock vm stop`",
                self.v
            );
        }
        if !valid_token(&self.token) {
            bail!("the run token is not valid");
        }
        if !self.project.is_absolute() || self.project.parent().is_none() {
            bail!(
                "the project {} is not an absolute folder",
                self.project.display()
            );
        }
        let bad = |s: &str| s.is_empty() || s.chars().any(|c| c.is_control());
        if bad(&self.id) {
            bail!("the id is empty or has a control character");
        }
        // The harness name is also a file name in the guest.
        if !valid_harness(&self.harness) {
            bail!("the harness name {:?} is not a profile name", self.harness);
        }
        if self.profile.as_ref().is_some_and(|p| p.len() > MAX_PROFILE) {
            bail!("the profile is longer than {MAX_PROFILE} bytes");
        }
        for k in self.env.keys() {
            if !allowed_var(k) {
                bail!("the variable {k} is not a terminal or locale variable");
            }
        }
        Ok(())
    }
}

/// The longest user profile in a run request.
pub const MAX_PROFILE: usize = 64 * 1024;

/// A profile name: letters, digits, `.`, `_` and `-`, not first `.` or `-`.
pub fn valid_harness(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with(['.', '-'])
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// The host variables that a run request can carry into the guest.
pub fn allowed_var(k: &str) -> bool {
    matches!(
        k,
        "TERM" | "COLORTERM" | "LANG" | "LANGUAGE" | "TZ" | "NO_COLOR" | "FORCE_COLOR"
    ) || (k.starts_with("LC_")
        && k.len() > 3
        && k.len() < 32
        && k[3..].bytes().all(|b| b.is_ascii_uppercase() || b == b'_'))
}

/// Write one message and its newline.
pub fn write_line<T: Serialize>(w: &mut impl Write, msg: &T) -> Result<()> {
    let mut line = serde_json::to_vec(msg)?;
    line.push(b'\n');
    w.write_all(&line)?;
    w.flush()?;
    Ok(())
}

/// Read one line byte by byte, so no byte after the newline is lost. None
/// at the end of the stream before any byte.
pub fn read_raw_line(r: &mut impl Read, max: usize) -> Result<Option<String>> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match r.read(&mut byte) {
            Ok(0) if line.is_empty() => return Ok(None),
            Ok(0) => bail!("the connection closed in the middle of a line"),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
        if byte[0] == b'\n' {
            break;
        }
        if line.len() == max {
            bail!("the line is longer than {max} bytes");
        }
        line.push(byte[0]);
    }
    String::from_utf8(line)
        .map(Some)
        .context("the line is not UTF-8")
}

/// Read one message. None at the end of the stream.
pub fn read_line<T: for<'de> Deserialize<'de>>(r: &mut impl Read) -> Result<Option<T>> {
    match read_raw_line(r, LINE_MAX)? {
        None => Ok(None),
        Some(line) => serde_json::from_str(&line)
            .map(Some)
            .with_context(|| format!("not a valid message: {}", short(&line))),
    }
}

fn short(line: &str) -> String {
    line.chars().take(80).collect()
}

#[cfg(test)]
#[path = "proto_tests.rs"]
mod tests;
