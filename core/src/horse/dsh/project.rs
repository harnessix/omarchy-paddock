//! Project a session head back into a native dsh session file.
//!
//! A session with a native id gets its stored header and rows back, byte
//! for byte. A fork that has no native id yet gets what dsh's own fork
//! (`session.fork`, `buildForkSeed` in `packages/core/session/src/fork.ts`)
//! writes: the rows up to the cut byte for byte, a `session/end-seed` row
//! with `inherited: true`, and a new header with `parentSession` and
//! `isSeeded: true`. The file is published like dsh publishes one: a synced
//! temporary file in the session folder, then a link that never replaces a
//! file.

use super::{encode_file, row_info, session_dir, Header, CONTAINER, HARNESS, ORIGIN};
use crate::horse::store::{sync_dir, Store};
use anyhow::{bail, Context as _, Result};
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

/// A projected session file.
#[derive(Debug)]
pub struct Projection {
    pub path: PathBuf,
    pub native_id: String,
    /// The projection made a new native session for a fork.
    pub forked: bool,
}

/// The rows of a dsh fork whose cut is the last of `rows`: the rows as
/// they are, then `{"type":"session/end-seed","seq":B+1,"time":<time of
/// row B>,"data":{"inherited":true}}`. The cut must close a turn, so no
/// synthetic closers are needed (`openTurnClosers` adds none).
pub fn fork_rows(rows: &[Vec<u8>]) -> Result<Vec<Vec<u8>>> {
    let last = rows.last().context("a dsh fork needs at least one row")?;
    let (ty, seq, v) = row_info(last)?;
    if ty != "turn/end" {
        bail!("a dsh fork must cut at a turn/end row; seq {seq} is {ty}");
    }
    let time = v
        .get("time")
        .and_then(serde_json::Value::as_u64)
        .context("the cut row has no time")?;
    let mut out = rows.to_vec();
    out.push(
        format!(
            "{{\"type\":\"session/end-seed\",\"seq\":{},\"time\":{time},\"data\":{{\"inherited\":true}}}}",
            seq + 1
        )
        .into_bytes(),
    );
    Ok(out)
}

/// Write the session file of `session` under the dsh sessions root `root`
/// (`$DSH_HOME/sessions`). `created_ms` is the `createdAt` of a new fork.
pub fn project(store: &Store, session: &str, root: &Path, created_ms: u64) -> Result<Projection> {
    let s = store.session(session)?;
    if s.binding.harness != HARNESS || s.binding.container != CONTAINER {
        bail!("session {session} is not a dsh session");
    }
    super::check_version(&s.binding.version)?;
    let header_raw = store
        .get(
            s.native_header
                .as_ref()
                .context("the session has no dsh header")?,
        )?
        .raw;
    let header = Header::parse(&header_raw)?;
    let rows: Vec<Vec<u8>> = store
        .session_nodes(&s)?
        .into_iter()
        .map(|n| n.raw)
        .collect();

    if let Some(id) = &s.binding.native_id {
        if &header.id != id {
            bail!("the stored header of session {session} has another id");
        }
        let refs: Vec<&[u8]> = rows.iter().map(Vec::as_slice).collect();
        let path = publish(root, &header, &encode_file(&header_raw, &refs)?)?;
        return Ok(Projection {
            path,
            native_id: id.clone(),
            forked: false,
        });
    }

    let fork = s
        .fork_of
        .as_ref()
        .context("the session has no native id and is no fork")?;
    let parent_id = store
        .session(&fork.session)?
        .binding
        .native_id
        .with_context(|| format!("project the source session {} first", fork.session))?;
    let rows = fork_rows(&rows)?;
    let child = Header {
        id: format!("session-{}", crate::horse::uuid_v4()?),
        created_at: created_ms,
        cwd: header.cwd.clone(),
        parent_session: Some(parent_id),
        is_seeded: true,
        origin: None,
        delegation_depth: 0,
        agent_preset: header.agent_preset.clone(),
    };
    let child_raw = child.encode();
    let refs: Vec<&[u8]> = rows.iter().map(Vec::as_slice).collect();
    let path = publish(root, &child, &encode_file(&child_raw, &refs)?)?;
    // The store follows the native file: the end-seed row is a node, and
    // the new header and id belong to the session now.
    let seed = rows.last().expect("fork rows end with the seed");
    let key = format!("seq:{}", rows.len() - 1);
    let seed = store.put_native(&ORIGIN, s.head.as_ref(), &key, seed)?;
    let hdr = store.put_blob("dsh-header", &child_raw)?;
    store.update_session(session, |s| {
        s.head = Some(seed);
        s.native_header = Some(hdr);
        s.binding.native_id = Some(child.id.clone());
    })?;
    Ok(Projection {
        path,
        native_id: child.id,
        forked: true,
    })
}

/// Publish a session file without replacing anything.
fn publish(root: &Path, header: &Header, bytes: &[u8]) -> Result<PathBuf> {
    let dir = session_dir(root, header.cwd.as_deref(), &header.id);
    let seg = dir.file_name().expect("session dir has a name").to_owned();
    // dsh's list() fails when one id is in two project folders.
    if let Ok(rd) = std::fs::read_dir(root) {
        for e in rd.flatten() {
            let other = e.path().join(&seg);
            if e.path() != dir.parent().unwrap_or(root) && other.exists() {
                bail!(
                    "dsh session {} is in {} already",
                    header.id,
                    e.path().display()
                );
            }
        }
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    refuse_if_locked(&dir)?;
    let path = dir.join(super::FILE_ZSTD);
    let tmp = dir.join(format!(
        ".{}.paddock-tmp-{}",
        super::FILE_ZSTD,
        std::process::id()
    ));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("cannot create {}", tmp.display()))?;
    let written = f.write_all(bytes).and_then(|_| f.sync_all());
    drop(f);
    let linked = written.and_then(|_| std::fs::hard_link(&tmp, &path));
    let _ = std::fs::remove_file(&tmp);
    match linked {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            bail!(
                "{} exists; paddock never replaces a dsh session file",
                path.display()
            )
        }
        Err(e) => return Err(e).with_context(|| format!("cannot write {}", path.display())),
    }
    sync_dir(&dir)?;
    Ok(path)
}

/// dsh holds `session.lock` with flock while it writes a session.
fn refuse_if_locked(dir: &Path) -> Result<()> {
    let Ok(f) = std::fs::File::open(dir.join("session.lock")) else {
        return Ok(());
    };
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("dsh has {} open; stop it first", dir.display());
    }
    unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_UN) };
    Ok(())
}
