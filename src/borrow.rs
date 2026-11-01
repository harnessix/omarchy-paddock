//! Borrow a host folder, change the copy, and return the changes
//! (`paddock-host borrow` and `paddock-host return`).
//!
//! At the start, paddock mounts an empty folder of its own at
//! `/run/paddock/borrow` in the sandbox. A borrow copies a host folder into
//! it, with reflinks where the file system has them, and records the state
//! of the original in a manifest that only the host can write. The agent
//! changes the copy. A return compares the copy with the manifest and
//! writes the changes back, only when the original is still as it was at
//! the borrow. So a change of the user or another agent is never
//! overwritten.
//!
//! The copy is sandbox-writable. The host reads it through `safefs` and
//! never follows a symlink in it. The changed files go into a snapshot that
//! only the host can write before the user sees the card, so the write-back
//! is exactly what the card shows.

use crate::env::HostEnv;
use anyhow::{bail, Context as _, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

/// Where the borrow folder appears inside the sandbox.
pub const MOUNT: &str = "/run/paddock/borrow";
/// The default of `[borrow] max`: the most bytes a borrow copies without
/// reflinks, and the most changed bytes a return takes.
pub const DEFAULT_MAX: u64 = 1 << 30;
/// Files up to this size get a hash in the manifest.
pub const HASH_MAX: u64 = 8 << 20;
const MAX_ENTRIES: usize = 200_000;
const MAX_DEPTH: usize = 256;
/// The most names of one list on the card.
const SHOWN: usize = 30;

/// The folders of all sandboxes. `<root>/<sandbox id>` is mounted.
pub fn root(env: &HostEnv) -> PathBuf {
    env.paddock_data().join("borrow")
}

/// The borrow folder of one sandbox, mounted read-write at `MOUNT`.
pub fn dir(env: &HostEnv, id: &str) -> PathBuf {
    root(env).join(id)
}

/// Manifests and snapshots. The sandbox never sees this folder.
pub fn meta_root(env: &HostEnv) -> PathBuf {
    env.paddock_data().join("borrow-meta")
}

fn meta(env: &HostEnv, id: &str) -> PathBuf {
    meta_root(env).join(id)
}

/// `[borrow]` of the user configuration.
#[derive(Debug, Clone)]
pub struct Cfg {
    pub max: u64,
}

impl Cfg {
    pub fn load(env: &HostEnv) -> Result<Self> {
        let mut c = Self { max: DEFAULT_MAX };
        let file = env.paddock_config().join("config.toml");
        let Ok(text) = std::fs::read_to_string(&file) else {
            return Ok(c);
        };
        let doc: toml::Value =
            toml::from_str(&text).with_context(|| format!("{} is invalid", file.display()))?;
        if let Some(v) = doc.get("borrow").and_then(|b| b.get("max")) {
            c.max = match v {
                toml::Value::Integer(n) if *n > 0 => *n as u64,
                toml::Value::String(s) => parse_size(s)?,
                _ => bail!("[borrow] max must be a size such as \"1G\""),
            };
        }
        Ok(c)
    }
}

/// "512M", "1G", "4096": a size in bytes.
pub fn parse_size(s: &str) -> Result<u64> {
    let s = s.trim();
    let bad = || anyhow::anyhow!("[borrow] max: {s} is not a size such as 512M or 1G");
    let (num, mul) = match s.chars().last() {
        Some(c) if c.is_ascii_alphabetic() => {
            let mul: u64 = match c.to_ascii_uppercase() {
                'K' => 1 << 10,
                'M' => 1 << 20,
                'G' => 1 << 30,
                'T' => 1 << 40,
                _ => return Err(bad()),
            };
            (&s[..s.len() - 1], mul)
        }
        _ => (s, 1),
    };
    let n: u64 = num.trim().parse().map_err(|_| bad())?;
    n.checked_mul(mul).filter(|b| *b > 0).ok_or_else(bad)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    File,
    Dir,
    Symlink,
}

/// One entry of a folder. Folders keep no size or time: their entries
/// carry the changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub kind: Kind,
    #[serde(default)]
    pub size: u64,
    /// Nanoseconds since 1970.
    #[serde(default)]
    pub mtime: i64,
    #[serde(default)]
    pub mode: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<PathBuf>,
}

impl Entry {
    fn dir() -> Self {
        Self {
            kind: Kind::Dir,
            size: 0,
            mtime: 0,
            mode: 0o755,
            hash: None,
            target: None,
        }
    }

    fn file(m: &std::fs::Metadata) -> Self {
        Self {
            kind: Kind::File,
            size: m.len(),
            mtime: m.mtime() * 1_000_000_000 + m.mtime_nsec(),
            mode: m.mode() & 0o7777,
            hash: None,
            target: None,
        }
    }

    fn symlink(target: PathBuf) -> Self {
        Self {
            kind: Kind::Symlink,
            size: 0,
            mtime: 0,
            mode: 0o777,
            hash: None,
            target: Some(target),
        }
    }

    /// The same content. A hash counts only when both sides have one.
    pub fn same(&self, o: &Entry) -> bool {
        if self.kind != o.kind {
            return false;
        }
        match self.kind {
            Kind::Dir => true,
            Kind::Symlink => self.target == o.target,
            Kind::File => {
                self.size == o.size
                    && self.mtime == o.mtime
                    && match (&self.hash, &o.hash) {
                        (Some(a), Some(b)) => a == b,
                        _ => true,
                    }
            }
        }
    }
}

/// The state of the original at the borrow. Only the host writes it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub name: String,
    pub original: PathBuf,
    pub borrowed: u64,
    pub bytes: u64,
    pub entries: BTreeMap<String, Entry>,
}

fn join_rel(rel: &str, name: &str) -> String {
    if rel.is_empty() {
        name.to_string()
    } else {
        format!("{rel}/{name}")
    }
}

fn utf8(name: &OsStr, rel: &str) -> Result<String> {
    name.to_str().map(str::to_string).with_context(|| {
        format!(
            "{}: paddock borrows only folders with UTF-8 file names",
            join_rel(rel, &name.to_string_lossy())
        )
    })
}

fn hash_reader(mut r: impl Read) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(crate::manifest::hex(&h.finalize()))
}

/// The entries of a host folder that only the host writes. Symlinks are
/// recorded, never followed. FIFOs, sockets and devices are left out.
pub fn scan(root: &Path) -> Result<BTreeMap<String, Entry>> {
    let mut out = BTreeMap::new();
    scan_at(root, "", 0, &mut out)?;
    Ok(out)
}

fn scan_at(dir: &Path, rel: &str, depth: usize, out: &mut BTreeMap<String, Entry>) -> Result<()> {
    if depth > MAX_DEPTH {
        bail!("{} is too deep", dir.display());
    }
    let mut names = std::fs::read_dir(dir)
        .with_context(|| format!("cannot read {}", dir.display()))?
        .map(|e| e.map(|e| e.file_name()))
        .collect::<std::io::Result<Vec<_>>>()?;
    names.sort();
    for n in names {
        let r = join_rel(rel, &utf8(&n, rel)?);
        let p = dir.join(&n);
        let m = std::fs::symlink_metadata(&p)?;
        let t = m.file_type();
        let e = if t.is_symlink() {
            Entry::symlink(std::fs::read_link(&p)?)
        } else if t.is_dir() {
            Entry::dir()
        } else if t.is_file() {
            let mut e = Entry::file(&m);
            if e.size <= HASH_MAX {
                e.hash = Some(hash_reader(std::fs::File::open(&p)?)?);
            }
            e
        } else {
            continue;
        };
        let is_dir = e.kind == Kind::Dir;
        out.insert(r.clone(), e);
        if out.len() > MAX_ENTRIES {
            bail!("the folder has more than {MAX_ENTRIES} entries");
        }
        if is_dir {
            scan_at(&p, &r, depth + 1, out)?;
        }
    }
    Ok(())
}

/// What differs between two states: added, changed, deleted names.
pub fn diff(
    old: &BTreeMap<String, Entry>,
    new: &BTreeMap<String, Entry>,
) -> (Vec<String>, Vec<String>, Vec<String>) {
    let (mut added, mut changed, mut deleted) = (vec![], vec![], vec![]);
    for (r, e) in new {
        match old.get(r) {
            None => added.push(r.clone()),
            Some(o) if !o.same(e) => changed.push(r.clone()),
            Some(_) => {}
        }
    }
    for r in old.keys() {
        if !new.contains_key(r) {
            deleted.push(r.clone());
        }
    }
    (added, changed, deleted)
}

/// Refuse when the original differs from the manifest: someone changed it
/// after the borrow.
pub fn check_original(m: &Manifest) -> Result<()> {
    let now = scan(&m.original)
        .with_context(|| format!("cannot read the original {}", m.original.display()))?;
    let (a, c, d) = diff(&m.entries, &now);
    if a.is_empty() && c.is_empty() && d.is_empty() {
        return Ok(());
    }
    let names: Vec<String> = a.iter().chain(&c).chain(&d).cloned().collect();
    bail!(
        "the original {} changed after the borrow ({} added, {} changed, {} deleted: {}). paddock never overwrites it. Borrow it again and redo the changes",
        m.original.display(),
        a.len(),
        c.len(),
        d.len(),
        shown(&names)
    )
}

fn shown(names: &[String]) -> String {
    let mut s = names
        .iter()
        .take(SHOWN)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if names.len() > SHOWN {
        s.push_str(&format!(" +{} more", names.len() - SHOWN));
    }
    s
}

/// A git file that paddock binds read-only in a project (see detect.rs
/// `Git`): `.git/config`, the hooks, the pointer files. A return never
/// writes, changes or removes one.
pub fn git_protected(rel: &str, kind: Kind) -> bool {
    let parts: Vec<&str> = rel.split('/').collect();
    for (i, p) in parts.iter().enumerate() {
        if *p != ".git" {
            continue;
        }
        let mut tail = &parts[i + 1..];
        if tail.is_empty() {
            // A `.git` file names the git directory of a worktree.
            if kind != Kind::Dir {
                return true;
            }
            continue;
        }
        if tail.contains(&"hooks") {
            return true;
        }
        while let ["worktrees" | "modules", _, rest @ ..] = tail {
            if rest.is_empty() {
                break;
            }
            tail = rest;
        }
        let joined = tail.join("/");
        if crate::hostgit::PROTECTED_FILES.contains(&joined.as_str()) {
            return true;
        }
    }
    false
}

/// A symlink whose target leaves the borrowed folder.
pub fn escapes(rel: &str, target: &Path) -> bool {
    if target.is_absolute() {
        return true;
    }
    let mut depth = rel.split('/').count() as i64 - 1;
    for c in target.components() {
        match c {
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return true;
                }
            }
            _ => return true,
        }
    }
    false
}

/// A warning for a folder that host programs read or run.
pub fn host_read_warning(env: &HostEnv, real: &Path) -> Option<String> {
    const HOME: &[&str] = &[
        ".config",
        ".local",
        "bin",
        ".bin",
        ".profile",
        ".bashrc",
        ".bash_profile",
        ".zshrc",
        ".zprofile",
        ".zshenv",
        ".xprofile",
        ".pam_environment",
    ];
    const ABS: &[&str] = &["/etc", "/usr", "/opt", "/boot"];
    let hit = HOME.iter().any(|h| real.starts_with(env.home.join(h)))
        || ABS.iter().any(|a| real.starts_with(a));
    hit.then(|| {
        format!(
            "WARNING: host programs read or run files in {}. A change there takes effect outside the sandbox.",
            real.display()
        )
    })
}

// ---------------------------------------------------------------- borrow

/// A borrow, opened before the user is asked. The copy reads from the open
/// folder, so a path that changes during the wait changes nothing.
pub struct BorrowPlan {
    pub src: crate::safefs::Source,
    /// Where the path led when it was opened; the card shows this.
    pub real: PathBuf,
    pub bytes: u64,
}

/// Open the folder to borrow. The last part of the path is no symlink.
/// Protected paths and folders that hold one are refused.
pub fn plan_borrow(
    env: &HostEnv,
    asked: &Path,
    protected: impl Fn(&Path, &Path) -> Result<()>,
) -> Result<BorrowPlan> {
    let lm = std::fs::symlink_metadata(asked)
        .with_context(|| format!("{} does not exist on the host", asked.display()))?;
    if lm.file_type().is_symlink() {
        bail!(
            "{} is a symlink. Borrow the folder by its own path",
            asked.display()
        );
    }
    let src = crate::safefs::Source::open(asked)?;
    if src.id()? != (lm.dev(), lm.ino()) {
        bail!("{} changed while paddock opened it", asked.display());
    }
    if !src.is_dir() {
        bail!(
            "{} is a file. borrow takes a folder; use paddock-host fetch for one file",
            asked.display()
        );
    }
    let real = src.real_path()?;
    protected(asked, &real)?;
    // The borrow folders themselves are paddock data, which `protected`
    // refuses; this is a second line.
    if real.starts_with(env.paddock_data()) {
        bail!("{} is a folder of paddock", real.display());
    }
    let bytes = src.size(u64::MAX)?;
    Ok(BorrowPlan { src, real, bytes })
}

const FICLONE: libc::c_ulong = 0x4004_9409;

/// Copy a file, with a reflink when the file system can. True: reflinked.
fn clone_file(from: &std::fs::File, to: &std::fs::File) -> Result<bool> {
    use std::os::fd::AsRawFd;
    // SAFETY: FICLONE takes the source fd as its argument.
    let r = unsafe { libc::ioctl(to.as_raw_fd(), FICLONE as _, from.as_raw_fd()) };
    if r == 0 {
        return Ok(true);
    }
    let mut from = from;
    let mut to = to;
    std::io::copy(&mut from, &mut to)?;
    Ok(false)
}

fn set_mtime(f: &std::fs::File, mtime: i64) -> Result<()> {
    let t = std::time::UNIX_EPOCH
        + std::time::Duration::new(
            mtime.div_euclid(1_000_000_000).max(0) as u64,
            mtime.rem_euclid(1_000_000_000) as u32,
        );
    f.set_times(std::fs::FileTimes::new().set_modified(t))?;
    Ok(())
}

/// Copy the scanned entries of `src` into `dst`. Bytes that cannot be
/// reflinked count against `max`.
fn copy_tree(src: &Path, dst: &Path, entries: &BTreeMap<String, Entry>, max: u64) -> Result<u64> {
    let mut plain = 0u64;
    for (r, e) in entries {
        let (from, to) = (src.join(r), dst.join(r));
        match e.kind {
            Kind::Dir => {
                std::fs::create_dir(&to)?;
                std::fs::set_permissions(&to, std::fs::Permissions::from_mode(0o755))?;
            }
            Kind::Symlink => {
                std::os::unix::fs::symlink(e.target.as_deref().unwrap_or(Path::new("")), &to)?
            }
            Kind::File => {
                let f = std::fs::File::open(&from)
                    .with_context(|| format!("cannot read {}", from.display()))?;
                let t = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&to)?;
                if !clone_file(&f, &t)? {
                    plain += e.size;
                    if plain > max {
                        bail!(
                            "the folder is larger than [borrow] max ({} MiB), and this file system cannot share blocks (reflink). Raise [borrow] max in the user configuration, or borrow a smaller folder",
                            max >> 20
                        );
                    }
                }
                set_mtime(&t, e.mtime)?;
                t.set_permissions(std::fs::Permissions::from_mode((e.mode & 0o777) | 0o600))?;
            }
        }
    }
    Ok(plain)
}

fn nonce() -> String {
    crate::broker::new_request_id()
}

/// Copy the opened folder into the borrow folder of sandbox `id` and write
/// its manifest. Returns the name of the copy.
pub fn borrow(env: &HostEnv, id: &str, p: &BorrowPlan, max: u64) -> Result<serde_json::Value> {
    let src = p.src.proc_path();
    let entries = scan(&src)?;
    let m = meta(env, id);
    crate::privfs::dir(&m)?;
    let stage = m.join(format!("stage-{}", nonce()));
    std::fs::create_dir(&stage)?;
    let plain = match copy_tree(&src, &stage, &entries, max) {
        Ok(n) => n,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&stage);
            return Err(e);
        }
    };
    std::fs::set_permissions(&stage, std::fs::Permissions::from_mode(0o755))?;
    let bdir = dir(env, id);
    crate::privfs::dir(&bdir)?;
    let target = crate::safefs::Root::open(&bdir)?.dir(Path::new(""), false)?;
    let want = crate::broker::safe_name(&p.real.to_string_lossy(), "borrowed");
    let mut name = None;
    for i in 0..1000 {
        let cand = if i == 0 {
            want.clone()
        } else {
            format!("{want}-{i}")
        };
        // A manifest without a copy still holds the name.
        if m.join(format!("{cand}.json")).exists() {
            continue;
        }
        match target.move_in(&stage, OsStr::new(&cand)) {
            Ok(()) => {
                name = Some(cand);
                break;
            }
            Err(e) if matches!(e.raw_os_error(), Some(libc::EEXIST) | Some(libc::ENOTEMPTY)) => {}
            Err(e) => {
                let _ = std::fs::remove_dir_all(&stage);
                return Err(anyhow::Error::from(e).context("cannot move the copy in place"));
            }
        }
    }
    let Some(name) = name else {
        let _ = std::fs::remove_dir_all(&stage);
        bail!("no free name for {want} in the borrow folder");
    };
    let files = entries.values().filter(|e| e.kind == Kind::File).count();
    let manifest = Manifest {
        name: name.clone(),
        original: p.real.clone(),
        borrowed: crate::state::now(),
        bytes: p.bytes,
        entries,
    };
    crate::privfs::write(
        &m.join(format!("{name}.json")),
        serde_json::to_string(&manifest)?,
    )?;
    Ok(serde_json::json!({
        "name": name,
        "path": format!("{MOUNT}/{name}"),
        "original": p.real,
        "files": files,
        "bytes": p.bytes,
        "reflinked": plain < p.bytes,
    }))
}

// ---------------------------------------------------------------- return

/// What a return changes in the original.
#[derive(Debug, Default, Clone)]
pub struct Changes {
    pub added: Vec<String>,
    pub changed: Vec<String>,
    pub deleted: Vec<String>,
    /// Git files that a return never writes.
    pub kept: Vec<String>,
    /// FIFOs, sockets and devices in the copy.
    pub skipped: Vec<String>,
}

impl Changes {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.changed.is_empty() && self.deleted.is_empty()
    }

    pub fn counts(&self) -> String {
        format!(
            "{} added, {} changed, {} deleted",
            self.added.len(),
            self.changed.len(),
            self.deleted.len()
        )
    }
}

/// A return, worked out before the user is asked.
pub struct ReturnPlan {
    pub name: String,
    pub manifest: Manifest,
    /// Changed and new file content, in a folder that only the host writes.
    snap: PathBuf,
    copy: BTreeMap<String, Entry>,
    pub changes: Changes,
    pub warning: Option<String>,
    pub changed_bytes: u64,
}

impl Drop for ReturnPlan {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.snap);
    }
}

/// A plain name of a copy: what `borrow` printed.
pub fn valid_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 120
        && !n.starts_with('.')
        && n.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
}

/// The names of the copies of sandbox `id` that wait for a return.
pub fn list(env: &HostEnv, id: &str) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(meta(env, id))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            e.file_name()
                .to_str()
                .and_then(|n| n.strip_suffix(".json"))
                .map(str::to_string)
        })
        .collect();
    v.sort();
    v
}

struct Snap<'a> {
    old: &'a BTreeMap<String, Entry>,
    root: &'a Path,
    out: BTreeMap<String, Entry>,
    stored: BTreeSet<String>,
    skipped: Vec<String>,
    budget: u64,
    max: u64,
}

impl Snap<'_> {
    fn walk(&mut self, d: &crate::safefs::Dir, rel: &str, depth: usize) -> Result<()> {
        use crate::safefs::Kind as K;
        if depth > MAX_DEPTH {
            bail!("the copy is too deep");
        }
        let mut entries = d.entries()?;
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        for (n, k) in entries {
            let r = join_rel(rel, &utf8(&n, rel)?);
            if self.out.len() > MAX_ENTRIES {
                bail!("the copy has more than {MAX_ENTRIES} entries");
            }
            match k {
                K::Dir => {
                    self.out.insert(r.clone(), Entry::dir());
                    let sub = d.subdir(&n, false)?;
                    self.walk(&sub, &r, depth + 1)?;
                }
                K::Symlink => {
                    self.out.insert(r, Entry::symlink(d.read_link(&n)?));
                }
                K::File => {
                    let Some(mut f) = d.open_read(&n)? else {
                        continue;
                    };
                    let mut e = Entry::file(&f.metadata()?);
                    let old = self.old.get(&r);
                    let maybe = old.is_some_and(|o| {
                        o.kind == Kind::File && o.size == e.size && o.mtime == e.mtime
                    });
                    if maybe && old.is_some_and(|o| o.hash.is_none()) {
                        self.out.insert(r, e);
                        continue;
                    }
                    let mut bytes = Vec::new();
                    let small = e.size <= HASH_MAX;
                    if small {
                        (&mut f).take(HASH_MAX).read_to_end(&mut bytes)?;
                        e.hash = Some(hash_reader(bytes.as_slice())?);
                        if maybe && old.and_then(|o| o.hash.as_ref()) == e.hash.as_ref() {
                            self.out.insert(r, e);
                            continue;
                        }
                    }
                    let too_large = || {
                        anyhow::anyhow!(
                            "the changes are larger than [borrow] max ({} MiB)",
                            self.max >> 20
                        )
                    };
                    if e.size > self.budget {
                        return Err(too_large());
                    }
                    let to = self.root.join(&r);
                    if let Some(p) = to.parent() {
                        std::fs::create_dir_all(p)?;
                    }
                    let mut t = std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .mode(0o600)
                        .open(&to)?;
                    let n = if small {
                        t.write_all(&bytes)?;
                        bytes.len() as u64
                    } else {
                        std::io::copy(&mut (&mut f).take(self.budget + 1), &mut t)?
                    };
                    if n > self.budget {
                        return Err(too_large());
                    }
                    self.budget = self.budget.saturating_sub(n);
                    e.size = n;
                    self.stored.insert(r.clone());
                    self.out.insert(r, e);
                }
                K::Socket | K::Other => self.skipped.push(r),
            }
        }
        Ok(())
    }
}

use std::os::unix::fs::OpenOptionsExt;

/// Read the copy `name` of sandbox `id`, compare it with the manifest and
/// the original, and work out the changes. Refuse when the original changed
/// after the borrow, or when a new symlink leaves the folder.
pub fn plan_return(env: &HostEnv, id: &str, name: &str, max: u64) -> Result<ReturnPlan> {
    if !valid_name(name) {
        bail!("{name} is not the name of a borrowed copy");
    }
    let mfile = meta(env, id).join(format!("{name}.json"));
    let text = std::fs::read_to_string(&mfile).with_context(|| {
        let names = list(env, id);
        if names.is_empty() {
            format!("no borrowed copy {name}: this sandbox has none")
        } else {
            format!("no borrowed copy {name}. Copies: {}", names.join(", "))
        }
    })?;
    let manifest: Manifest = serde_json::from_str(&text)?;
    check_original(&manifest)?;
    let copy_dir = crate::safefs::Root::open(&dir(env, id))?
        .dir(Path::new(name), false)
        .with_context(|| format!("the copy {MOUNT}/{name} is gone"))?;
    let snap = meta(env, id).join(format!("snap-{}", nonce()));
    std::fs::create_dir(&snap)?;
    let mut s = Snap {
        old: &manifest.entries,
        root: &snap,
        out: BTreeMap::new(),
        stored: BTreeSet::new(),
        skipped: vec![],
        budget: max,
        max,
    };
    if let Err(e) = s.walk(&copy_dir, "", 0) {
        let _ = std::fs::remove_dir_all(&snap);
        return Err(e);
    }
    let changed_bytes = max - s.budget;
    let (copy, stored, skipped) = (s.out, s.stored, s.skipped);
    let mut ch = Changes {
        skipped,
        ..Default::default()
    };
    for (r, e) in &copy {
        let kind_old = manifest.entries.get(r).map(|o| o.kind);
        let list = match manifest.entries.get(r) {
            None => &mut ch.added,
            Some(o) if o.kind != e.kind => &mut ch.changed,
            Some(_) if e.kind == Kind::File && stored.contains(r) => &mut ch.changed,
            Some(o) if e.kind == Kind::Symlink && o.target != e.target => &mut ch.changed,
            Some(_) => continue,
        };
        if git_protected(r, e.kind) || kind_old.is_some_and(|k| git_protected(r, k)) {
            ch.kept.push(r.clone());
        } else {
            list.push(r.clone());
        }
    }
    for (r, o) in &manifest.entries {
        if !copy.contains_key(r) {
            if git_protected(r, o.kind) {
                ch.kept.push(r.clone());
            } else {
                ch.deleted.push(r.clone());
            }
        }
    }
    let out: Vec<String> = ch
        .added
        .iter()
        .chain(&ch.changed)
        .filter(|r| {
            let e = &copy[*r];
            e.kind == Kind::Symlink && escapes(r, e.target.as_deref().unwrap_or(Path::new("/")))
        })
        .cloned()
        .collect();
    let plan = ReturnPlan {
        name: name.to_string(),
        warning: host_read_warning(env, &manifest.original),
        manifest,
        snap,
        copy,
        changes: ch,
        changed_bytes,
    };
    if !out.is_empty() {
        bail!(
            "these symlinks of the copy point outside the folder, so paddock does not return it: {}",
            shown(&out)
        );
    }
    Ok(plan)
}

impl ReturnPlan {
    /// The card text.
    pub fn detail(&self) -> String {
        let c = &self.changes;
        let mut t = format!(
            "Write the changes of the borrowed copy {MOUNT}/{} back to the host folder {}: {}.",
            self.name,
            self.manifest.original.display(),
            c.counts()
        );
        for (label, v) in [
            ("Added", &c.added),
            ("Changed", &c.changed),
            ("Deleted", &c.deleted),
        ] {
            if !v.is_empty() {
                t.push_str(&format!("\n{label}: {}", shown(v)));
            }
        }
        if !c.kept.is_empty() {
            t.push_str(&format!(
                "\nNot written (git files that paddock never writes): {}",
                shown(&c.kept)
            ));
        }
        if !c.skipped.is_empty() {
            t.push_str(&format!(
                "\nSkipped (not a file, folder or symlink): {}",
                shown(&c.skipped)
            ));
        }
        t.push_str("\nNew files get no execute bit. The original did not change since the borrow. paddock checks it again before it writes.");
        if let Some(w) = &self.warning {
            t.push_str(&format!("\n{w}"));
        }
        t
    }

    /// Write the changes into the original, then remove the copy and its
    /// manifest.
    pub fn apply(&self, env: &HostEnv, id: &str) -> Result<serde_json::Value> {
        // Check again: the original can change while the user decides.
        check_original(&self.manifest)?;
        let orig = &self.manifest.original;
        let c = &self.changes;
        // A path in the original, with no symlink on the way.
        let place = |r: &str| -> Result<PathBuf> {
            let mut p = orig.clone();
            let parts: Vec<&str> = r.split('/').collect();
            for (i, part) in parts.iter().enumerate() {
                p.push(part);
                if i + 1 < parts.len()
                    && std::fs::symlink_metadata(&p).is_ok_and(|m| !m.file_type().is_dir())
                {
                    bail!("{} is not a folder in the original", p.display());
                }
            }
            Ok(p)
        };
        // Remove first, children before their folders: deletions and the
        // old entry of a changed kind.
        let mut gone: Vec<&String> = c
            .deleted
            .iter()
            .chain(c.changed.iter().filter(|r| {
                self.manifest
                    .entries
                    .get(*r)
                    .is_some_and(|o| o.kind != self.copy[*r].kind || o.kind == Kind::Symlink)
            }))
            .collect();
        gone.sort();
        gone.dedup();
        for r in gone.into_iter().rev() {
            let p = place(r)?;
            match std::fs::symlink_metadata(&p) {
                Ok(m) if m.file_type().is_dir() => {
                    // A folder that keeps a git file stays.
                    if let Err(e) = std::fs::remove_dir(&p) {
                        if e.raw_os_error() != Some(libc::ENOTEMPTY) {
                            return Err(e.into());
                        }
                    }
                }
                Ok(_) => std::fs::remove_file(&p)?,
                Err(_) => {}
            }
        }
        let mut put: Vec<&String> = c.added.iter().chain(&c.changed).collect();
        put.sort();
        let mut bytes = 0u64;
        for r in put {
            let e = &self.copy[r];
            let p = place(r)?;
            match e.kind {
                Kind::Dir => match std::fs::create_dir(&p) {
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && p.is_dir() => {}
                    r => r?,
                },
                Kind::Symlink => {
                    std::os::unix::fs::symlink(e.target.as_deref().unwrap_or(Path::new("")), &p)?
                }
                Kind::File => {
                    let mode = match self.manifest.entries.get(r) {
                        Some(o) if o.kind == Kind::File => o.mode & 0o7777,
                        // New files: no execute bit, no special bits.
                        _ => e.mode & 0o666,
                    };
                    let tmp = p.with_file_name(format!(
                        ".{}.paddock-return-{}",
                        p.file_name().unwrap_or_default().to_string_lossy(),
                        std::process::id()
                    ));
                    let _ = std::fs::remove_file(&tmp);
                    let mut from = std::fs::File::open(self.snap.join(r))?;
                    let mut to = std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .mode(0o600)
                        .open(&tmp)?;
                    bytes += std::io::copy(&mut from, &mut to)?;
                    to.sync_all()?;
                    drop(to);
                    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
                    if let Err(err) = std::fs::rename(&tmp, &p) {
                        let _ = std::fs::remove_file(&tmp);
                        return Err(err.into());
                    }
                }
            }
        }
        // The borrow is over: the copy and its manifest go.
        if let Ok(r) = crate::safefs::Root::open(&dir(env, id)) {
            let _ = r.remove_tree(Path::new(&self.name));
        }
        let _ = std::fs::remove_file(meta(env, id).join(format!("{}.json", self.name)));
        Ok(serde_json::json!({
            "name": self.name,
            "original": self.manifest.original,
            "added": c.added.len(),
            "changed": c.changed.len(),
            "deleted": c.deleted.len(),
            "kept": c.kept,
            "skipped": c.skipped,
            "bytes": bytes,
        }))
    }
}

// ---------------------------------------------------------------- cleanup

/// The sandbox ended: remove its copies that were not returned, and their
/// manifests. Only inside its own borrow folder.
pub fn end(env: &HostEnv, id: &str) {
    if id.is_empty() || id.contains('/') || id.starts_with('.') {
        return;
    }
    if let Ok(r) = crate::safefs::Root::open(&root(env)) {
        let _ = r.remove_tree(Path::new(id));
    }
    let _ = std::fs::remove_dir_all(meta(env, id));
}

fn subdirs(d: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(d)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .collect();
    v.sort();
    v
}

/// The borrow folders of sandboxes that no longer run, for `paddock gc`.
pub fn gc_items(env: &HostEnv) -> Vec<crate::storage::Item> {
    let running: BTreeSet<String> = crate::state::Running::list(env)
        .into_iter()
        .map(|r| r.id)
        .collect();
    let mut items = Vec::new();
    for base in [root(env), meta_root(env)] {
        for p in subdirs(&base) {
            let id = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if running.contains(&id) {
                continue;
            }
            items.push(crate::storage::Item {
                what: format!("borrowed copies of {id} ({})", p.display()),
                tier: crate::storage::Tier::Auto,
                bytes: crate::storage::sizes(std::slice::from_ref(&p)).0[0],
                reason: "the sandbox ended without removing them; nothing was returned from them"
                    .into(),
                warning: None,
                needs: vec![],
                action: crate::storage::Action::Path { path: p },
            });
        }
    }
    items
}

/// One line for `paddock du`, when there are borrow folders.
pub fn du_line(env: &HostEnv) -> Option<String> {
    let dirs = subdirs(&root(env));
    if dirs.is_empty() {
        return None;
    }
    let copies: usize = dirs
        .iter()
        .map(|d| std::fs::read_dir(d).map(|r| r.count()).unwrap_or(0))
        .sum();
    let bytes: u64 = crate::storage::sizes(&dirs).0.iter().sum();
    Some(format!(
        "Borrowed copies: {} in {copies} copies of {} sandboxes ({})",
        crate::storage::human(bytes),
        dirs.len(),
        root(env).display()
    ))
}

#[cfg(test)]
mod tests;
