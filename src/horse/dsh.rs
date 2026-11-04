//! The dsh codec: dsh session files to store nodes and back.
//!
//! Pinned to dsh 0.2.1-rc.5, read from its MIT source at the tag
//! `dsh-v0.2.1-rc.5` (commit b830ec950faba0a46dda564be3e2d854afed94f1).
//! Session files are `<root>/--<projectKey(cwd)>--/<encodeSegment(id)>/
//! session.v4.jsonl.zstd`: a header line, then one event row per line with
//! a contiguous `seq` from 0 (`session-persistence-jsonl/src/format.ts`).
//! A row is a node keyed by its `seq`, so ingest does not depend on byte
//! offsets: a truncated tail, a rewritten tail or a new file give the same
//! nodes for the same rows. Other harness versions and format versions are
//! refused (fail closed).

pub mod request;
pub mod zframe;

use super::store::{Hash, Origin, Store};
use super::Binding;
use anyhow::{bail, Context as _, Result};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

pub const HARNESS: &str = "dsh";
/// The dsh version whose source this codec follows.
pub const VERSION: &str = "0.2.1-rc.5";
/// `SESSION_FORMAT_VERSION` in `packages/core/session/src/types.ts`.
pub const FORMAT: u64 = 4;
pub const CONTAINER: &str = "dsh-session-v4-jsonl";
const FILE_ZSTD: &str = "session.v4.jsonl.zstd";
const FILE_RAW: &str = "session.v4.jsonl";

const ORIGIN: Origin = Origin {
    harness: HARNESS,
    container: CONTAINER,
    version: VERSION,
};

/// Refuse every dsh version but the pinned one.
pub fn check_version(version: &str) -> Result<()> {
    if version != VERSION {
        bail!(
            "dsh {version} is not supported: paddock reads and writes dsh sessions only for dsh {VERSION}"
        );
    }
    Ok(())
}

/// The header line (`HeaderLine` in `format.ts`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    pub id: String,
    pub created_at: u64,
    pub cwd: Option<String>,
    pub parent_session: Option<String>,
    pub is_seeded: bool,
    pub origin: Option<String>,
    pub delegation_depth: u64,
    pub agent_preset: Option<String>,
}

const HEADER_KEYS: [&str; 10] = [
    "type",
    "version",
    "id",
    "createdAt",
    "cwd",
    "parentSession",
    "isSeeded",
    "origin",
    "delegationDepth",
    "agentPreset",
];

impl Header {
    /// Parse and check a header line: the exact key set, the types, and
    /// format version 4 (`isHeaderLine` in `format.ts`).
    pub fn parse(raw: &[u8]) -> Result<Header> {
        let v: Map<String, Value> =
            serde_json::from_slice(raw).context("the dsh session header is not a JSON object")?;
        if let Some(k) = v.keys().find(|k| !HEADER_KEYS.contains(&k.as_str())) {
            bail!("the dsh session header has an unknown key {k}");
        }
        if v.get("type").and_then(Value::as_str) != Some("session") {
            bail!("the first row of the dsh session file is not a session header");
        }
        match v.get("version").and_then(Value::as_u64) {
            Some(FORMAT) => {}
            other => bail!(
                "dsh session format {} is not supported: paddock reads format {FORMAT} only (dsh {VERSION})",
                other.map_or("?".to_string(), |n| n.to_string())
            ),
        }
        let s = |k: &str| -> Result<Option<String>> {
            match v.get(k) {
                None => Ok(None),
                Some(Value::String(s)) => Ok(Some(s.clone())),
                Some(_) => bail!("the dsh session header has a bad {k}"),
            }
        };
        let n = |k: &str| {
            v.get(k)
                .and_then(Value::as_u64)
                .with_context(|| format!("the dsh session header has a bad {k}"))
        };
        let h = Header {
            id: s("id")?.context("the dsh session header has no id")?,
            created_at: n("createdAt")?,
            cwd: s("cwd")?,
            parent_session: s("parentSession")?,
            is_seeded: v
                .get("isSeeded")
                .and_then(Value::as_bool)
                .context("the dsh session header has a bad isSeeded")?,
            origin: s("origin")?,
            delegation_depth: n("delegationDepth")?,
            agent_preset: s("agentPreset")?,
        };
        if h.cwd.as_deref().is_some_and(|c| !c.starts_with('/')) {
            bail!("the dsh session header has a cwd that is not absolute");
        }
        if h.origin.as_deref().is_some_and(|o| o != "subagent") {
            bail!("the dsh session header has a bad origin");
        }
        Ok(h)
    }

    /// The header line as dsh writes it: `JSON.stringify` of the object of
    /// `encodeHeader` in `session-format-v1-to-v2/src/codec.ts`, in its key
    /// order, without the newline.
    pub fn encode(&self) -> Vec<u8> {
        let q = |s: &str| serde_json::to_string(s).expect("a string encodes");
        let mut o = format!(
            "{{\"type\":\"session\",\"version\":{FORMAT},\"id\":{},\"createdAt\":{}",
            q(&self.id),
            self.created_at
        );
        if let Some(c) = &self.cwd {
            o += &format!(",\"cwd\":{}", q(c));
        }
        if let Some(p) = &self.parent_session {
            o += &format!(",\"parentSession\":{}", q(p));
        }
        o += &format!(",\"isSeeded\":{}", self.is_seeded);
        if let Some(x) = &self.origin {
            o += &format!(",\"origin\":{}", q(x));
        }
        o += &format!(",\"delegationDepth\":{}", self.delegation_depth);
        if let Some(a) = &self.agent_preset {
            o += &format!(",\"agentPreset\":{}", q(a));
        }
        o.push('}');
        o.into_bytes()
    }
}

/// A dsh session file, read.
#[derive(Debug)]
pub struct Log {
    pub header_raw: Vec<u8>,
    pub header: Header,
    /// The event rows, byte for byte, without their newline. Row `i` has
    /// `seq` `i`.
    pub rows: Vec<Vec<u8>>,
    /// An incomplete frame or line at the end was left out.
    pub torn: bool,
}

/// The type and seq of an event row.
pub fn row_info(raw: &[u8]) -> Result<(String, u64, Value)> {
    let v: Value = serde_json::from_slice(raw).context("a dsh session row is not JSON")?;
    let ty = v
        .get("type")
        .and_then(Value::as_str)
        .context("a dsh session row has no type")?
        .to_string();
    let seq = v
        .get("seq")
        .and_then(Value::as_u64)
        .context("a dsh session row has no seq")?;
    Ok((ty, seq, v))
}

/// Split plain text into its complete lines. A last line without a newline
/// is incomplete and left out.
fn lines(plain: &[u8]) -> (Vec<Vec<u8>>, bool) {
    let mut out: Vec<Vec<u8>> = plain.split(|&b| b == b'\n').map(<[u8]>::to_vec).collect();
    let partial = out.pop().is_some_and(|l| !l.is_empty());
    (out, partial)
}

/// Read a dsh session file: `session.v4.jsonl.zstd` or `session.v4.jsonl`.
pub fn read_file(path: &Path) -> Result<Log> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    let bytes = std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    let (plain, torn_frame) = match name {
        FILE_ZSTD => zframe::decode(&bytes)?,
        FILE_RAW => (bytes, false),
        _ => bail!(
            "{} is not a dsh session file of format {FORMAT} ({FILE_ZSTD})",
            path.display()
        ),
    };
    let (mut ls, partial) = lines(&plain);
    if ls.is_empty() {
        bail!("{} has no complete session header", path.display());
    }
    let header_raw = ls.remove(0);
    let header = Header::parse(&header_raw)?;
    for (i, r) in ls.iter().enumerate() {
        let (_, seq, _) = row_info(r)?;
        if seq != i as u64 {
            bail!(
                "{}: row {i} has seq {seq}; dsh writes seq from 0 without gaps",
                path.display()
            );
        }
    }
    Ok(Log {
        header_raw,
        header,
        rows: ls,
        torn: torn_frame || partial,
    })
}

/// The session file in a dsh session folder: the highest generation. A
/// generation other than format 4 is refused, also when it is newer.
pub fn current_file(dir: &Path) -> Result<PathBuf> {
    let mut best: Option<(u64, String)> = None;
    for e in std::fs::read_dir(dir).with_context(|| format!("cannot read {}", dir.display()))? {
        let name = e?.file_name().to_string_lossy().into_owned();
        let stem = name
            .strip_suffix(".jsonl.zstd")
            .or_else(|| name.strip_suffix(".jsonl"));
        let Some(stem) = stem else { continue };
        let v = match stem {
            "session" => 0,
            _ => match stem
                .strip_prefix("session.v")
                .and_then(|n| n.parse::<u64>().ok())
            {
                Some(n) if n > 0 && !stem.starts_with("session.v0") => n,
                _ => continue,
            },
        };
        if best.as_ref().is_none_or(|(b, _)| v > *b) {
            best = Some((v, name));
        }
    }
    match best {
        Some((FORMAT, name)) => Ok(dir.join(name)),
        Some((v, _)) => bail!(
            "{} holds dsh session format {v}: paddock reads format {FORMAT} only (dsh {VERSION})",
            dir.display()
        ),
        None => bail!("{} has no dsh session file", dir.display()),
    }
}

/// What an ingest did.
#[derive(Debug, PartialEq, Eq)]
pub struct Ingest {
    pub session: String,
    /// Rows that were in the store for this session already.
    pub kept: usize,
    /// New rows.
    pub added: usize,
    /// Nodes of the old head that the file does not have any more (a
    /// truncated or rewritten tail). They stay in the DAG.
    pub dropped: usize,
    pub torn: bool,
}

/// Ingest a dsh session file, or the current file of a session folder, into
/// the store. A session that the store does not know yet goes into `horse`,
/// or into a new horse with the default DNA.
pub fn ingest(store: &Store, path: &Path, version: &str, horse: Option<&str>) -> Result<Ingest> {
    check_version(version)?;
    let file = if path.is_dir() {
        current_file(path)?
    } else {
        path.to_path_buf()
    };
    let log = read_file(&file)?;
    let session = match store.session_by_native(HARNESS, &log.header.id)? {
        Some(s) => s,
        None => {
            let h = match horse {
                Some(h) => store.horse(h)?,
                None => store.new_horse(super::DEFAULT_DNA)?,
            };
            store.new_session(&h.id, binding(Some(log.header.id.clone())))?
        }
    };
    let old = store.session_nodes(&session)?;
    let header = store.put_blob("dsh-header", &log.header_raw)?;
    let (mut kept, mut parent) = (0, None::<Hash>);
    for (i, raw) in log.rows.iter().enumerate() {
        let h = store.put_native(&ORIGIN, parent.as_ref(), &format!("seq:{i}"), raw)?;
        if old.get(i).is_some_and(|n| n.hash == h) {
            kept += 1;
        }
        parent = Some(h);
    }
    // A torn tail is a write in progress or a crash that dsh repairs on
    // its next open. The head moves back only when complete rows differ.
    let wait = log.torn && kept == log.rows.len();
    store.update_session(&session.id, |s| {
        if !wait {
            s.head = parent;
        }
        s.native_header = Some(header);
    })?;
    Ok(Ingest {
        session: session.id,
        kept,
        added: log.rows.len() - kept,
        dropped: if wait { 0 } else { old.len() - kept },
        torn: log.torn,
    })
}

fn binding(native_id: Option<String>) -> Binding {
    Binding {
        harness: HARNESS.into(),
        native_id,
        version: VERSION.into(),
        container: CONTAINER.into(),
    }
}

/// `encodeSegment` in `format.ts`: `[A-Za-z0-9._-]` stay, every other
/// UTF-16 unit becomes `~XXXX`.
pub fn encode_segment(raw: &str) -> String {
    match raw {
        "." => return "~002E".into(),
        ".." => return "~002E~002E".into(),
        _ => {}
    }
    raw.encode_utf16().map(unit).collect()
}

fn unit(u: u16) -> String {
    match char::from_u32(u32::from(u)) {
        Some(c) if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') => c.to_string(),
        _ => format!("~{u:04X}"),
    }
}

/// `projectKey` in `format.ts`: the folder name of a project.
pub fn project_key(cwd: &str) -> String {
    let mut readable = String::new();
    let mut sep = false;
    for u in cwd.encode_utf16() {
        if matches!(u, 0x2F | 0x5C | 0x3A) {
            if !sep {
                readable.push('-');
            }
            sep = true;
        } else {
            readable += &unit(u);
            sep = false;
        }
    }
    let slug = readable.trim_start_matches('-');
    let slug = if slug.is_empty() { "root" } else { slug };
    format!("--{}--", &slug[..slug.len().min(251)])
}

/// The session folder of a session under a dsh sessions root.
pub fn session_dir(root: &Path, cwd: Option<&str>, id: &str) -> PathBuf {
    let project = cwd.map_or_else(|| "_no-cwd".to_string(), project_key);
    root.join(project).join(encode_segment(id))
}

/// The bytes of a session file: a frame with the header line, then a frame
/// with the rows, as `createGeneration` writes them.
pub fn encode_file(header: &[u8], rows: &[&[u8]]) -> Result<Vec<u8>> {
    let mut h = header.to_vec();
    h.push(b'\n');
    let mut out = zframe::encode(&h)?;
    if !rows.is_empty() {
        let mut body = Vec::new();
        for r in rows {
            body.extend_from_slice(r);
            body.push(b'\n');
        }
        out.extend_from_slice(&zframe::encode(&body)?);
    }
    Ok(out)
}

/// The normalized message view of a row: the role and the text of a
/// model-visible message, or `None` for a log-only row.
#[derive(Debug, PartialEq, Eq)]
pub struct View {
    pub role: String,
    pub text: String,
}

pub fn view(raw: &[u8]) -> Option<View> {
    let j = request::J::parse(raw).ok()?;
    let m = request::derive_messages(std::slice::from_ref(&j))
        .ok()?
        .pop()?;
    let mut text = Vec::new();
    if let Some(request::J::Arr(blocks)) = m.get("content") {
        for b in blocks {
            match b.get("type").and_then(request::J::str) {
                Some("text" | "reasoning") => text.push(
                    b.get("text")
                        .and_then(request::J::str)
                        .unwrap_or_default()
                        .to_string(),
                ),
                Some("tool-call") => text.push(format!(
                    "[call {}]",
                    b.get("name").and_then(request::J::str).unwrap_or_default()
                )),
                Some(t) => text.push(format!("[{t}]")),
                None => {}
            }
        }
    }
    Some(View {
        role: m
            .get("role")
            .and_then(request::J::str)
            .unwrap_or_default()
            .to_string(),
        text: text
            .into_iter()
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join(" "),
    })
}

mod project;
pub use project::{fork_rows, project, Projection};

#[cfg(test)]
mod tests;
