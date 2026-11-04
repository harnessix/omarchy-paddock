//! The horse store: a persistent, content-addressed DAG.
//!
//! Layout under the store root (all folders 0700, all files 0600):
//!
//! - `objects/ab/cdef...`: one object per file, named by the sha256 of its
//!   bytes. An object is one line of JSON metadata, a newline, and the body.
//!   A native node has the raw row of the harness as its body and the hash
//!   of its parent node in the metadata, so equal rows on equal parents are
//!   one object (dedupe), and a node pins its whole history.
//! - `sessions/<id>.json`, `horses/<id>.json`: the small mutable objects.
//! - `tmp/`: files that are not complete yet.
//! - `lock`: the writer lock for the mutable objects.
//!
//! Every write goes to `tmp/` first and is synced. An object then gets a hard
//! link to its final name (an object that is there already is the same
//! object), and a mutable file gets a rename. A crash leaves at most a file
//! in `tmp/`, which the next open removes when it is old.

use crate::privfs;
use anyhow::{bail, Context as _, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// A temporary file older than this is left over from a crash.
const STALE_TMP: Duration = Duration::from_secs(3600);

/// The sha256 of an object, as 64 lower-case hex digits.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Hash(String);

impl Hash {
    pub fn of(bytes: &[u8]) -> Hash {
        Hash(super::hex(&Sha256::digest(bytes)))
    }

    pub fn parse(s: &str) -> Result<Hash> {
        if s.len() != 64
            || !s
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            bail!("not a node hash: {s}");
        }
        Ok(Hash(s.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The first 12 digits, for lists.
    pub fn short(&self) -> &str {
        &self.0[..12]
    }
}

impl std::fmt::Display for Hash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The metadata line of an object. The field order is fixed, so the same
/// object always has the same bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    /// `native` for a row of a harness, `blob` for other content.
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    /// The container format of the row, for example `dsh-session-v4-jsonl`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    /// The harness version that wrote the row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<Hash>,
    /// The identity of the record in its native session, for example `seq:12`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// The type of a blob, for example `toml`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media: Option<String>,
}

/// One object of the store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    pub hash: Hash,
    pub meta: Meta,
    /// The body: the raw native row, byte for byte, or the blob.
    pub raw: Vec<u8>,
}

/// Where a native row comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Origin<'a> {
    pub harness: &'a str,
    pub container: &'a str,
    pub version: &'a str,
}

/// The store at one root folder.
#[derive(Clone, Debug)]
pub struct Store {
    root: PathBuf,
}

/// The writer lock of the store. It is released on drop.
pub struct Lock {
    _file: File,
}

impl Store {
    /// Open the store, and create it when it is missing. Old temporary files
    /// from a crash are removed.
    pub fn open(root: &Path) -> Result<Store> {
        for d in ["", "objects", "sessions", "horses", "tmp"] {
            privfs::dir(&root.join(d))
                .with_context(|| format!("cannot create {}", root.join(d).display()))?;
        }
        let s = Store {
            root: root.to_path_buf(),
        };
        s.clean_tmp();
        Ok(s)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn clean_tmp(&self) {
        let Ok(rd) = std::fs::read_dir(self.root.join("tmp")) else {
            return;
        };
        let now = SystemTime::now();
        for e in rd.flatten() {
            let old = e
                .metadata()
                .and_then(|m| m.modified())
                .map(|t| now.duration_since(t).unwrap_or_default() > STALE_TMP)
                .unwrap_or(false);
            if old {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }

    /// Take the writer lock. Writers of sessions and horses hold it while
    /// they read, change and write them.
    pub fn lock(&self) -> Result<Lock> {
        let f = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(privfs::FILE_MODE)
            .open(self.root.join("lock"))?;
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
            bail!(
                "cannot lock the horse store: {}",
                std::io::Error::last_os_error()
            );
        }
        Ok(Lock { _file: f })
    }

    fn object_path(&self, h: &Hash) -> PathBuf {
        self.root.join("objects").join(&h.0[..2]).join(&h.0[2..])
    }

    /// Write `bytes` to a new synced file in `tmp/`. The caller links or
    /// renames it into place.
    pub(crate) fn write_tmp(&self, bytes: &[u8]) -> Result<PathBuf> {
        let name = format!(
            "{}-{}",
            std::process::id(),
            super::hex(&super::random_bytes::<8>()?)
        );
        let p = self.root.join("tmp").join(name);
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(privfs::FILE_MODE)
            .open(&p)?;
        let r = f.write_all(bytes).and_then(|_| f.sync_all());
        if let Err(e) = r {
            let _ = std::fs::remove_file(&p);
            return Err(e.into());
        }
        Ok(p)
    }

    /// Add an object. An object with the same bytes is there already when
    /// the hash is known: the store keeps one copy.
    pub fn put(&self, meta: &Meta, raw: &[u8]) -> Result<Hash> {
        let mut bytes = serde_json::to_vec(meta)?;
        bytes.push(b'\n');
        bytes.extend_from_slice(raw);
        let h = Hash::of(&bytes);
        let path = self.object_path(&h);
        if path.exists() {
            return Ok(h);
        }
        let dir = path.parent().expect("object path has a parent");
        privfs::dir(dir)?;
        let tmp = self.write_tmp(&bytes)?;
        let linked = std::fs::hard_link(&tmp, &path);
        let _ = std::fs::remove_file(&tmp);
        match linked {
            Ok(()) => sync_dir(dir)?,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e).context("cannot add an object to the horse store"),
        }
        Ok(h)
    }

    /// Add a native row with its parent node.
    pub fn put_native(
        &self,
        origin: &Origin,
        parent: Option<&Hash>,
        key: &str,
        raw: &[u8],
    ) -> Result<Hash> {
        let meta = Meta {
            kind: "native".into(),
            harness: Some(origin.harness.into()),
            container: Some(origin.container.into()),
            version: Some(origin.version.into()),
            parent: parent.cloned(),
            key: Some(key.into()),
            media: None,
        };
        self.put(&meta, raw)
    }

    /// Add a blob, for example a DNA or a native session header.
    pub fn put_blob(&self, media: &str, raw: &[u8]) -> Result<Hash> {
        let meta = Meta {
            kind: "blob".into(),
            harness: None,
            container: None,
            version: None,
            parent: None,
            key: None,
            media: Some(media.into()),
        };
        self.put(&meta, raw)
    }

    /// Read an object and check its hash. A damaged object is an error.
    pub fn get(&self, h: &Hash) -> Result<Node> {
        let path = self.object_path(h);
        let bytes = std::fs::read(&path)
            .with_context(|| format!("no node {} in the horse store", h.short()))?;
        if Hash::of(&bytes) != *h {
            bail!(
                "node {} is damaged: its content does not match its hash",
                h.short()
            );
        }
        let nl = bytes
            .iter()
            .position(|&b| b == b'\n')
            .with_context(|| format!("node {} has no metadata line", h.short()))?;
        let meta: Meta = serde_json::from_slice(&bytes[..nl])
            .with_context(|| format!("node {} has bad metadata", h.short()))?;
        Ok(Node {
            hash: h.clone(),
            meta,
            raw: bytes[nl + 1..].to_vec(),
        })
    }

    /// The nodes from the root to `head`, root first.
    pub fn chain(&self, head: Option<&Hash>) -> Result<Vec<Node>> {
        let mut out = Vec::new();
        let mut next = head.cloned();
        while let Some(h) = next {
            let n = self.get(&h)?;
            next = n.meta.parent.clone();
            out.push(n);
        }
        out.reverse();
        Ok(out)
    }

    /// Write a mutable file (`sessions/x.json`) atomically: a synced
    /// temporary file, a rename, and a synced folder.
    pub(crate) fn write_ref(&self, rel: &str, bytes: &[u8]) -> Result<()> {
        let path = self.root.join(rel);
        let dir = path.parent().expect("ref path has a parent").to_path_buf();
        privfs::dir(&dir)?;
        let tmp = self.write_tmp(bytes)?;
        if let Err(e) = std::fs::rename(&tmp, &path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e).with_context(|| format!("cannot write {}", path.display()));
        }
        sync_dir(&dir)
    }

    pub(crate) fn read_ref(&self, rel: &str) -> Result<Option<Vec<u8>>> {
        match std::fs::read(self.root.join(rel)) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("cannot read {rel} in the horse store")),
        }
    }

    /// The names of the files in a folder of the store, sorted.
    pub(crate) fn list_refs(&self, dir: &str) -> Result<Vec<String>> {
        let mut out: Vec<String> = std::fs::read_dir(self.root.join(dir))?
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();
        out.sort();
        Ok(out)
    }
}

/// Sync a folder, so a new name in it survives a crash.
pub(crate) fn sync_dir(dir: &Path) -> Result<()> {
    File::open(dir)
        .and_then(|f| f.sync_all())
        .with_context(|| format!("cannot sync {}", dir.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::privfs::tests::{mode, with_umask_022};

    const O: Origin = Origin {
        harness: "dsh",
        container: "test-jsonl",
        version: "1",
    };

    #[test]
    fn objects_dedupe_by_hash_and_link_parents() {
        let t = tempfile::tempdir().unwrap();
        let s = Store::open(t.path()).unwrap();
        let a = s.put_native(&O, None, "seq:0", b"{\"a\":1}").unwrap();
        let a2 = s.put_native(&O, None, "seq:0", b"{\"a\":1}").unwrap();
        assert_eq!(a, a2);
        let b = s.put_native(&O, Some(&a), "seq:1", b"{\"b\":2}").unwrap();
        // The same row on another parent is another node.
        let b_other = s.put_native(&O, None, "seq:1", b"{\"b\":2}").unwrap();
        assert_ne!(b, b_other);
        let chain = s.chain(Some(&b)).unwrap();
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0].hash, a);
        assert_eq!(chain[1].raw, b"{\"b\":2}");
        assert_eq!(chain[1].meta.parent.as_ref(), Some(&a));
        assert_eq!(chain[1].meta.key.as_deref(), Some("seq:1"));
        let files = std::fs::read_dir(t.path().join("objects"))
            .unwrap()
            .flatten()
            .map(|d| std::fs::read_dir(d.path()).unwrap().count())
            .sum::<usize>();
        assert_eq!(files, 3);
    }

    #[test]
    fn store_is_private_under_umask_022() {
        let t = tempfile::tempdir().unwrap();
        with_umask_022(|| {
            let root = t.path().join("horse");
            let s = Store::open(&root).unwrap();
            let h = s.put_blob("toml", b"x = 1\n").unwrap();
            s.write_ref("sessions/a.json", b"{}").unwrap();
            assert_eq!(mode(&root), 0o700);
            assert_eq!(mode(&root.join("objects")), 0o700);
            assert_eq!(mode(&s.object_path(&h)), 0o600);
            assert_eq!(mode(&root.join("sessions/a.json")), 0o600);
        });
    }

    #[test]
    fn rows_stay_byte_exact() {
        let t = tempfile::tempdir().unwrap();
        let s = Store::open(t.path()).unwrap();
        let raw = b"{\"x\":\"\\u00e9\\n\",\"y\":1.50}\r\0\xff";
        let h = s.put_native(&O, None, "seq:0", raw).unwrap();
        assert_eq!(s.get(&h).unwrap().raw, raw);
    }

    #[test]
    fn a_crash_in_the_middle_leaves_no_object_and_no_half_ref() {
        let t = tempfile::tempdir().unwrap();
        let s = Store::open(t.path()).unwrap();
        s.write_ref("sessions/a.json", b"{\"head\":1}").unwrap();
        // A writer dies after its temporary file and before the link or the
        // rename: readers see the old state only.
        let tmp = s.write_tmp(b"{\"head\":2").unwrap();
        assert_eq!(
            s.read_ref("sessions/a.json").unwrap().unwrap(),
            b"{\"head\":1}"
        );
        assert_eq!(s.list_refs("sessions").unwrap(), vec!["a.json"]);
        // A new open keeps a fresh temporary file (another writer may own
        // it) and removes an old one.
        Store::open(t.path()).unwrap();
        assert!(tmp.exists());
        let old = SystemTime::now() - Duration::from_secs(7200);
        File::options()
            .write(true)
            .open(&tmp)
            .unwrap()
            .set_modified(old)
            .unwrap();
        Store::open(t.path()).unwrap();
        assert!(!tmp.exists());
        // Writing again works.
        s.write_ref("sessions/a.json", b"{\"head\":2}").unwrap();
        assert_eq!(
            s.read_ref("sessions/a.json").unwrap().unwrap(),
            b"{\"head\":2}"
        );
    }

    #[test]
    fn a_damaged_object_is_refused() {
        let t = tempfile::tempdir().unwrap();
        let s = Store::open(t.path()).unwrap();
        let h = s.put_blob("text", b"hello").unwrap();
        let p = s.object_path(&h);
        let mut bytes = std::fs::read(&p).unwrap();
        bytes.truncate(bytes.len() - 2);
        std::fs::write(&p, bytes).unwrap();
        let err = s.get(&h).unwrap_err().to_string();
        assert!(err.contains("damaged"), "{err}");
    }

    #[test]
    fn hash_parse_refuses_other_text() {
        assert!(Hash::parse("../etc").is_err());
        assert!(Hash::parse(&"A".repeat(64)).is_err());
        let h = Hash::of(b"x");
        assert_eq!(Hash::parse(h.as_str()).unwrap(), h);
    }
}
