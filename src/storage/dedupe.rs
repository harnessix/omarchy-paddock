//! Kernel deduplication of sandbox homes (`FIDEDUPERANGE`).
//!
//! Harnesses download the same runtimes into the home of each project. On
//! btrfs, xfs and bcachefs the kernel can let identical files share their
//! data: it compares the bytes itself and refuses data that differs, and a
//! later write to one copy gets its own blocks (copy-on-write).
//!
//! paddock groups the regular files of the homes by size, then by SHA-256,
//! then asks the kernel to share the data. An index of (dev, inode, size,
//! mtime) keeps the hashes, so a file is hashed once. paddock never follows
//! a symlink, never leaves the file system of the homes folder, never works
//! outside it, and skips a file that changes while it reads it.

use crate::env::HostEnv;
use anyhow::{bail, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Smaller files are not worth an ioctl: one block at most.
pub const MIN_SIZE: u64 = 4096;
/// Bytes per `FIDEDUPERANGE` call. Linux and btrfs accept up to 16 MiB.
pub const CHUNK: u64 = 16 << 20;
/// The time limit of the run after a sandbox ends.
pub const AFTER_RUN: Duration = Duration::from_secs(120);

/// What makes a hash in the index still valid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Key {
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    /// Nanoseconds since the epoch.
    pub mtime: i64,
}

impl Key {
    pub fn of(m: &std::fs::Metadata) -> Self {
        Self {
            dev: m.dev(),
            ino: m.ino(),
            size: m.size(),
            mtime: m.mtime() * 1_000_000_000 + m.mtime_nsec(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: PathBuf,
    pub key: Key,
}

/// Hashes of files, by key. Saved as text lines `dev ino size mtime hash`.
#[derive(Debug, Default)]
pub struct Index {
    map: HashMap<Key, [u8; 32]>,
    seen: HashSet<Key>,
}

impl Index {
    pub fn load(path: &Path) -> Self {
        let mut ix = Self::default();
        let Ok(text) = std::fs::read_to_string(path) else {
            return ix;
        };
        for line in text.lines() {
            let f: Vec<&str> = line.split(' ').collect();
            let [dev, ino, size, mtime, hash] = f[..] else {
                continue;
            };
            let (Ok(dev), Ok(ino), Ok(size), Ok(mtime)) =
                (dev.parse(), ino.parse(), size.parse(), mtime.parse())
            else {
                continue;
            };
            let Some(h) = unhex(hash) else { continue };
            ix.map.insert(
                Key {
                    dev,
                    ino,
                    size,
                    mtime,
                },
                h,
            );
        }
        ix
    }

    pub fn get(&mut self, k: &Key) -> Option<[u8; 32]> {
        self.seen.insert(*k);
        self.map.get(k).copied()
    }

    pub fn put(&mut self, k: Key, h: [u8; 32]) {
        self.seen.insert(k);
        self.map.insert(k, h);
    }

    /// Mark the keys of a complete walk: `save(.., true)` keeps only them.
    pub fn saw(&mut self, k: &Key) {
        self.seen.insert(*k);
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Write the index. With `prune`, entries of files that the walk did
    /// not see go away.
    pub fn save(&self, path: &Path, prune: bool) -> Result<()> {
        let mut keys: Vec<&Key> = self
            .map
            .keys()
            .filter(|k| !prune || self.seen.contains(k))
            .collect();
        keys.sort();
        let mut text = String::new();
        for k in keys {
            text.push_str(&format!(
                "{} {} {} {} {}\n",
                k.dev,
                k.ino,
                k.size,
                k.mtime,
                crate::manifest::hex(&self.map[k])
            ));
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("tmp");
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&tmp)?;
        f.write_all(text.as_bytes())?;
        drop(f);
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

fn unhex(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

fn expired(deadline: Option<Instant>) -> bool {
    deadline.is_some_and(|d| Instant::now() >= d)
}

/// Regular files of at least `MIN_SIZE` under `root`, on the file system of
/// `root`. No symlink is followed. The flag is false when the deadline
/// stopped the walk.
pub fn walk(root: &Path, deadline: Option<Instant>) -> (Vec<Entry>, bool) {
    let mut out = Vec::new();
    let Ok(rm) = std::fs::symlink_metadata(root) else {
        return (out, true);
    };
    if !rm.is_dir() {
        return (out, true);
    }
    let dev = rm.dev();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if expired(deadline) {
            return (out, false);
        }
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_symlink() {
                continue;
            }
            let p = e.path();
            let Ok(m) = std::fs::symlink_metadata(&p) else {
                continue;
            };
            if m.dev() != dev {
                continue;
            }
            if m.is_dir() {
                stack.push(p);
            } else if m.is_file() && m.size() >= MIN_SIZE {
                out.push(Entry {
                    path: p,
                    key: Key::of(&m),
                });
            }
        }
    }
    (out, true)
}

/// Files with the same size, one path per inode. Only groups of two or more.
pub fn size_groups(files: &[Entry]) -> Vec<Vec<Entry>> {
    let mut by: BTreeMap<u64, Vec<Entry>> = BTreeMap::new();
    let mut inodes = HashSet::new();
    for f in files {
        if inodes.insert((f.key.dev, f.key.ino)) {
            by.entry(f.key.size).or_default().push(f.clone());
        }
    }
    by.into_values().filter(|g| g.len() > 1).collect()
}

/// SHA-256 of a file, or None when it is not the file of `key` any more or
/// changes while paddock reads it.
pub fn hash_file(path: &Path, key: &Key, deadline: Option<Instant>) -> Option<[u8; 32]> {
    let mut f = open_ro(path).ok()?;
    if Key::of(&f.metadata().ok()?) != *key {
        return None;
    }
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut total = 0u64;
    loop {
        if expired(deadline) {
            return None;
        }
        let n = f.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        total += n as u64;
        h.update(&buf[..n]);
    }
    if total != key.size || Key::of(&f.metadata().ok()?) != *key {
        return None;
    }
    Some(h.finalize().into())
}

/// Split each size group by content hash. Hashes come from the index when
/// the key matches. Returns the groups of two or more, and the count of
/// files that changed while being read.
pub fn hash_groups(
    groups: Vec<Vec<Entry>>,
    index: &mut Index,
    deadline: Option<Instant>,
) -> (Vec<Vec<Entry>>, usize) {
    let mut out = Vec::new();
    let mut changed = 0;
    for g in groups {
        let mut by: BTreeMap<[u8; 32], Vec<Entry>> = BTreeMap::new();
        for e in g {
            if expired(deadline) {
                return (out, changed);
            }
            let h = match index.get(&e.key) {
                Some(h) => h,
                None => match hash_file(&e.path, &e.key, deadline) {
                    Some(h) => {
                        index.put(e.key, h);
                        h
                    }
                    None => {
                        changed += usize::from(!expired(deadline));
                        continue;
                    }
                },
            };
            by.entry(h).or_default().push(e);
        }
        out.extend(by.into_values().filter(|g| g.len() > 1));
    }
    (out, changed)
}

/// (offset, length) pieces of a file of `len` bytes, `chunk` bytes each.
pub fn chunks(len: u64, chunk: u64) -> Vec<(u64, u64)> {
    let mut v = Vec::new();
    let mut off = 0;
    while off < len {
        let n = chunk.min(len - off);
        v.push((off, n));
        off += n;
    }
    v
}

fn open_ro(p: &Path) -> std::io::Result<File> {
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(p)
}

/// Physical address of the first extent (FIEMAP), when the file system
/// tells it.
fn first_extent(f: &File) -> Option<u64> {
    #[repr(C)]
    #[derive(Default)]
    struct Extent {
        logical: u64,
        physical: u64,
        length: u64,
        reserved64: [u64; 2],
        flags: u32,
        reserved: [u32; 3],
    }
    #[repr(C)]
    #[derive(Default)]
    struct Fiemap {
        start: u64,
        length: u64,
        flags: u32,
        mapped: u32,
        count: u32,
        reserved: u32,
        extents: [Extent; 1],
    }
    const FS_IOC_FIEMAP: u64 = 0xC020_660B;
    const FIEMAP_EXTENT_UNKNOWN: u32 = 0x2;
    let mut m = Fiemap {
        length: u64::MAX,
        count: 1,
        ..Default::default()
    };
    // SAFETY: m is a valid fiemap with room for `count` extents.
    let r = unsafe { libc::ioctl(f.as_raw_fd(), FS_IOC_FIEMAP as _, &mut m) };
    let e = &m.extents[0];
    (r == 0 && m.mapped == 1 && e.flags & FIEMAP_EXTENT_UNKNOWN == 0 && e.physical != 0)
        .then_some(e.physical)
}

/// The two files already share their data (first extents).
pub fn already_shared(a: &File, b: &File) -> bool {
    matches!((first_extent(a), first_extent(b)), (Some(x), Some(y)) if x == y)
}

#[derive(Debug)]
pub enum PairError {
    /// The file system cannot share data (EOPNOTSUPP, EINVAL, ENOTTY, EXDEV).
    Unsupported,
    /// The bytes differ: one of the files changed.
    Differs,
    Io(std::io::Error),
}

fn unsupported(errno: i32) -> bool {
    errno == libc::EOPNOTSUPP
        || errno == libc::EINVAL
        || errno == libc::ENOTTY
        || errno == libc::EXDEV
}

/// Share the data of `src` with `dst`, `len` bytes, in `CHUNK` pieces.
/// Returns the bytes that the kernel shared.
pub fn dedupe_files(
    src: &File,
    dst: &File,
    len: u64,
    deadline: Option<Instant>,
) -> std::result::Result<u64, PairError> {
    #[repr(C)]
    struct Info {
        dest_fd: i64,
        dest_offset: u64,
        bytes_deduped: u64,
        status: i32,
        reserved: u32,
    }
    #[repr(C)]
    struct Range {
        src_offset: u64,
        src_length: u64,
        dest_count: u16,
        reserved1: u16,
        reserved2: u32,
        info: [Info; 1],
    }
    const FIDEDUPERANGE: u64 = 0xC018_9436;
    let mut done = 0;
    for (start, n) in chunks(len, CHUNK) {
        let mut off = start;
        let end = start + n;
        while off < end {
            if expired(deadline) {
                return Ok(done);
            }
            let mut r = Range {
                src_offset: off,
                src_length: end - off,
                dest_count: 1,
                reserved1: 0,
                reserved2: 0,
                info: [Info {
                    dest_fd: dst.as_raw_fd() as i64,
                    dest_offset: off,
                    bytes_deduped: 0,
                    status: 0,
                    reserved: 0,
                }],
            };
            // SAFETY: r is a valid file_dedupe_range with one info entry.
            let rc = unsafe { libc::ioctl(src.as_raw_fd(), FIDEDUPERANGE as _, &mut r) };
            if rc < 0 {
                let e = std::io::Error::last_os_error();
                return Err(match e.raw_os_error() {
                    Some(n) if unsupported(n) => PairError::Unsupported,
                    _ => PairError::Io(e),
                });
            }
            let i = &r.info[0];
            match i.status {
                0 => {}
                1 => return Err(PairError::Differs),
                s if unsupported(-s) => return Err(PairError::Unsupported),
                s => return Err(PairError::Io(std::io::Error::from_raw_os_error(-s))),
            }
            if i.bytes_deduped == 0 {
                return Err(PairError::Io(std::io::Error::other(
                    "the kernel shared no bytes",
                )));
            }
            done += i.bytes_deduped;
            off += i.bytes_deduped;
        }
    }
    Ok(done)
}

/// Name of the file system of `p`, and whether it can share file data.
pub fn fs_kind(p: &Path) -> Option<(&'static str, bool)> {
    let c = std::ffi::CString::new(p.as_os_str().as_encoded_bytes()).ok()?;
    let mut s: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: c is a valid C string and s a statfs buffer.
    if unsafe { libc::statfs(c.as_ptr(), &mut s) } != 0 {
        return None;
    }
    Some(match s.f_type as i64 {
        0x9123_683E => ("btrfs", true),
        0x5846_5342 => ("xfs", true),
        0xCA45_1A4E => ("bcachefs", true),
        0xEF53 => ("ext4", false),
        0x0102_1994 => ("tmpfs", false),
        0x6969 => ("nfs", false),
        0x2FC1_2FC1 => ("zfs", false),
        0x794C_7630 => ("overlayfs", false),
        _ => ("this file system", false),
    })
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Report {
    /// Files with the same data as another file, not yet shared.
    pub candidates: usize,
    pub candidate_bytes: u64,
    /// Files whose data the kernel shared in this run.
    pub deduped: usize,
    /// Bytes that the kernel shared: the space that came back.
    pub deduped_bytes: u64,
    /// Files that already share their data.
    pub already_shared: usize,
    /// Files that changed while paddock read them, or that the kernel
    /// refused.
    pub skipped: usize,
    /// Set when the file system cannot share data: its name.
    pub unsupported: Option<String>,
    /// The time limit ended the run.
    pub timed_out: bool,
    /// Another dedupe ran at the same time; this one did nothing.
    pub busy: bool,
}

/// One dedupe run over `root` (the homes folder).
pub struct Run<'a> {
    pub root: &'a Path,
    /// Only files under this folder receive shared data, and only groups
    /// with such a file count. None: all files.
    pub only: Option<&'a Path>,
    /// Without it, only count the candidates.
    pub apply: bool,
    pub index: &'a Path,
    pub deadline: Option<Instant>,
}

impl Run<'_> {
    pub fn go(&self) -> Report {
        let mut rep = Report::default();
        match fs_kind(self.root) {
            None => return rep,
            Some((name, false)) => {
                rep.unsupported = Some(name.to_string());
                return rep;
            }
            Some(_) => {}
        }
        let Some(_lock) = lock(self.index) else {
            rep.busy = true;
            return rep;
        };
        let mut index = Index::load(self.index);
        let (files, complete) = walk(self.root, self.deadline);
        for f in &files {
            index.saw(&f.key);
        }
        let mut groups = size_groups(&files);
        if let Some(only) = self.only {
            groups.retain(|g| g.iter().any(|e| e.path.starts_with(only)));
        }
        let (groups, changed) = hash_groups(groups, &mut index, self.deadline);
        rep.skipped += changed;
        self.share(groups, &mut rep);
        rep.timed_out = !complete || expired(self.deadline);
        if !index.is_empty() || self.index.exists() {
            let _ = index.save(self.index, complete && !rep.timed_out);
        }
        rep
    }

    fn share(&self, groups: Vec<Vec<Entry>>, rep: &mut Report) {
        for mut g in groups {
            // The source: a file that does not receive data (outside `only`).
            if let Some(only) = self.only {
                g.sort_by_key(|e| e.path.starts_with(only));
            }
            let src = g.remove(0);
            let Ok(sf) = open_ro(&src.path) else {
                rep.skipped += 1;
                continue;
            };
            for d in g {
                if expired(self.deadline) {
                    return;
                }
                if self.only.is_some_and(|o| !d.path.starts_with(o)) {
                    continue;
                }
                let Ok(df) = open_ro(&d.path) else {
                    rep.skipped += 1;
                    continue;
                };
                let same = |f: &File, k: &Key| f.metadata().is_ok_and(|m| Key::of(&m) == *k);
                if !same(&sf, &src.key) || !same(&df, &d.key) {
                    rep.skipped += 1;
                    continue;
                }
                if already_shared(&sf, &df) {
                    rep.already_shared += 1;
                    continue;
                }
                rep.candidates += 1;
                rep.candidate_bytes += d.key.size;
                if !self.apply {
                    continue;
                }
                match dedupe_files(&sf, &df, d.key.size, self.deadline) {
                    Ok(n) => {
                        if n == d.key.size {
                            rep.deduped += 1;
                        }
                        rep.deduped_bytes += n;
                    }
                    Err(PairError::Unsupported) => {
                        let name = fs_kind(&d.path).map_or("this file system", |k| k.0);
                        rep.unsupported = Some(name.to_string());
                        return;
                    }
                    Err(_) => rep.skipped += 1,
                }
            }
        }
    }
}

/// An exclusive lock next to the index, or None when another run holds it.
fn lock(index: &Path) -> Option<File> {
    let dir = index.parent()?;
    std::fs::create_dir_all(dir).ok()?;
    let f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(index.with_extension("lock"))
        .ok()?;
    // SAFETY: a valid fd; the lock goes with the file.
    (unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0).then_some(f)
}

pub fn homes_root(env: &HostEnv) -> PathBuf {
    super::homes_root(env)
}

pub fn index_path(env: &HostEnv) -> PathBuf {
    env.paddock_cache().join("dedupe").join("index")
}

/// `[storage] dedupe` of the user configuration. On by default.
pub fn enabled(env: &HostEnv) -> bool {
    let text =
        std::fs::read_to_string(env.paddock_config().join("config.toml")).unwrap_or_default();
    enabled_in(&text)
}

pub fn enabled_in(text: &str) -> bool {
    let Ok(v) = toml::from_str::<toml::Value>(text) else {
        return true;
    };
    v.get("storage")
        .and_then(|s| s.get("dedupe"))
        .and_then(|d| d.as_bool())
        .unwrap_or(true)
}

/// `home` when it is a real folder inside the homes folder, without a
/// symlink on the way.
pub fn check_home(env: &HostEnv, home: &Path) -> Result<PathBuf> {
    let root = homes_root(env);
    let Ok(rest) = home.strip_prefix(&root) else {
        bail!("{} is not a sandbox home", home.display());
    };
    if rest.as_os_str().is_empty()
        || rest
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        bail!("{} is not a sandbox home", home.display());
    }
    let mut p = root.clone();
    for c in std::iter::once(None).chain(rest.components().map(Some)) {
        if let Some(c) = c {
            p.push(c);
        }
        let m = std::fs::symlink_metadata(&p)?;
        if !m.is_dir() {
            bail!("{} is not a folder", p.display());
        }
    }
    Ok(p)
}

/// The whole homes folder: `paddock gc --dedupe`.
pub fn all(env: &HostEnv, apply: bool) -> Report {
    let root = homes_root(env);
    if std::fs::symlink_metadata(&root).map_or(true, |m| !m.is_dir()) {
        return Report::default();
    }
    Run {
        root: &root,
        only: None,
        apply,
        index: &index_path(env),
        deadline: None,
    }
    .go()
}

/// After a sandbox ends: its home against the others, within the time
/// limit. The hidden `paddock dedupe-home` runs this.
pub fn home(env: &HostEnv, home: &Path, limit: Duration) -> Result<Report> {
    let home = check_home(env, home)?;
    let root = homes_root(env);
    Ok(Run {
        root: &root,
        only: Some(&home),
        apply: true,
        index: &index_path(env),
        deadline: Some(Instant::now() + limit),
    }
    .go())
}

/// At the end of a sandbox: start `paddock dedupe-home` in the background,
/// at the lowest CPU and I/O priority, and return at once. Only from the
/// real paddock binary, only for a home under the homes folder.
pub fn spawn_after_run(env: &HostEnv, home: &Path) {
    if env.var("PADDOCK_NO_DEDUPE").is_some() || !enabled(env) {
        return;
    }
    if check_home(env, home).is_err() {
        return;
    }
    if !matches!(fs_kind(home), Some((_, true))) {
        return;
    }
    let Ok(me) = std::env::current_exe() else {
        return;
    };
    if me.file_name().and_then(|n| n.to_str()) != Some("paddock") {
        return;
    }
    let mut cmd = std::process::Command::new(me);
    cmd.arg("dedupe-home")
        .arg(home)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    use std::os::unix::process::CommandExt;
    // SAFETY: setsid is async-signal-safe. The run outlives the sandbox.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    if let Ok(mut child) = cmd.spawn() {
        // Reap it if this process lives longer.
        std::thread::spawn(move || child.wait());
    }
}

/// Lowest CPU and I/O priority for the background run.
pub fn be_idle() {
    // SAFETY: plain syscalls on this process.
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, 0, 19);
        // IOPRIO_WHO_PROCESS, this process, class IDLE.
        libc::syscall(libc::SYS_ioprio_set, 1, 0, 3 << 13);
    }
}

/// A line for `paddock du` when the homes cannot share data.
pub fn du_note(env: &HostEnv) -> Option<String> {
    let root = homes_root(env);
    if !root.is_dir() {
        return None;
    }
    match fs_kind(&root)? {
        (name, false) => Some(format!(
            "the sandbox homes are on {name}, which cannot share file data: paddock does not deduplicate them"
        )),
        _ if !enabled(env) => {
            Some("dedupe after each sandbox is off ([storage] dedupe = false)".into())
        }
        _ => None,
    }
}

/// Text for `paddock gc --dedupe`.
pub fn render(r: &Report, apply: bool) -> String {
    let human = super::human;
    if let Some(fs) = &r.unsupported {
        if r.deduped == 0 {
            return format!(
                "dedupe: the sandbox homes are on {fs}, which cannot share file data. Skipped.\n"
            );
        }
    }
    if r.busy {
        return "dedupe: another dedupe runs now. Skipped.\n".into();
    }
    let mut t = if apply {
        format!(
            "dedupe: shared the data of {} files, got back {}.\n",
            r.deduped,
            human(r.deduped_bytes)
        )
    } else {
        format!(
            "dedupe: {} files ({}) have the same data as another file. --dedupe --yes shares it.\n",
            r.candidates,
            human(r.candidate_bytes)
        )
    };
    if r.already_shared > 0 {
        t.push_str(&format!(
            "dedupe: {} files share their data already.\n",
            r.already_shared
        ));
    }
    if r.skipped > 0 {
        t.push_str(&format!(
            "dedupe: skipped {} files that changed or that the kernel refused.\n",
            r.skipped
        ));
    }
    t
}

#[cfg(test)]
mod tests;
