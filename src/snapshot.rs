//! Project snapshots and `paddock undo`.
//!
//! Before a sandbox starts, paddock copies the project's files into
//! `<paddock data>/snapshots/<project slug>/<session id>/`. The copy shares
//! file data with the project where the file system supports it (`FICLONE`
//! on btrfs, xfs, bcachefs). Without that, paddock copies only while the
//! snapshot stays under `[snapshot] max`. `.git/` and the folders that git
//! ignores stay out: git has its own history, and build outputs are large.
//!
//! At exit paddock records the state of the project, so that `paddock undo`
//! can tell a change of the session from a later edit.

use crate::env::HostEnv;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::IsTerminal;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Snapshots kept per project.
pub const KEEP: usize = 10;
/// A snapshot that takes longer is dropped, and the sandbox starts anyway.
pub const TIME_LIMIT: Duration = Duration::from_secs(20);
/// `[snapshot] max` when the configuration does not set it.
pub const DEFAULT_MAX: u64 = 500_000_000;
/// Folders that stay out of a snapshot of a project outside git.
pub const NON_GIT_SKIP: &[&str] = &["target", "node_modules", ".venv", "dist", "build"];
const META: &str = "meta.json";
const FILES: &str = "files";
/// `_IOW(0x94, 9, int)`: share the data of one whole file with another.
const FICLONE: u64 = 0x4004_9409;

pub fn root(env: &HostEnv) -> PathBuf {
    env.paddock_data().join("snapshots")
}

pub fn trash_root(env: &HostEnv) -> PathBuf {
    env.paddock_data().join("undo-trash")
}

/// The snapshots of one project.
pub fn project_dir(root: &Path, project: &Path) -> PathBuf {
    root.join(crate::state::slug(project))
}

/// `[snapshot]` of the user configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub enabled: bool,
    pub max: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            enabled: true,
            max: DEFAULT_MAX,
        }
    }
}

impl Config {
    pub fn load(env: &HostEnv) -> Result<Self> {
        let f = env.paddock_config().join("config.toml");
        let text = std::fs::read_to_string(&f).unwrap_or_default();
        Self::parse(&text).with_context(|| format!("{} is invalid", f.display()))
    }

    pub fn parse(text: &str) -> Result<Self> {
        let mut c = Config::default();
        let v: toml::Value = toml::from_str(text)?;
        let Some(t) = v.get("snapshot") else {
            return Ok(c);
        };
        let Some(t) = t.as_table() else {
            bail!("[snapshot] is not a table");
        };
        for (k, v) in t {
            match (k.as_str(), v) {
                ("enabled", toml::Value::Boolean(b)) => c.enabled = *b,
                ("max", toml::Value::String(s)) => {
                    c.max = parse_size(s)
                        .with_context(|| format!("[snapshot] max = {s:?} is not a size"))?
                }
                ("max", toml::Value::Integer(n)) if *n >= 0 => c.max = *n as u64,
                (k, _) => bail!("[snapshot] {k} is unknown or has the wrong type"),
            }
        }
        Ok(c)
    }
}

/// "500M", "2G", "800K", "1000": decimal units, as `paddock du` shows them.
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let i = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (num, unit) = s.split_at(i);
    let n: u64 = num.parse().ok()?;
    let mult = match unit.trim().trim_end_matches(['B', 'b']) {
        "" => 1,
        "K" | "k" => 1_000,
        "M" | "m" => 1_000_000,
        "G" | "g" => 1_000_000_000,
        "T" | "t" => 1_000_000_000_000,
        _ => return None,
    };
    n.checked_mul(mult)
}

/// What paddock records of one file: enough to tell a change without
/// reading the content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fp {
    pub size: u64,
    /// Nanoseconds since 1970.
    pub mtime: i64,
    pub mode: u32,
    /// The target of a symbolic link.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
}

impl Fp {
    fn of(m: &std::fs::Metadata, link: Option<String>) -> Self {
        Fp {
            size: if link.is_some() { 0 } else { m.len() },
            mtime: m.mtime().saturating_mul(1_000_000_000) + m.mtime_nsec(),
            mode: m.mode() & 0o7777,
            link,
        }
    }
}

/// The parts of a project that a snapshot leaves out, and that undo never
/// touches.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skip {
    /// Leave out `NON_GIT_SKIP` folders at any depth.
    pub names: bool,
    /// Paths relative to the project, without a trailing `/`.
    pub paths: BTreeSet<String>,
}

impl Skip {
    pub fn skips(&self, rel: &str, name: &str, is_dir: bool) -> bool {
        name == ".git"
            || self.paths.contains(rel)
            || (self.names && is_dir && NON_GIT_SKIP.contains(&name))
    }

    pub fn union(&self, other: &Skip) -> Skip {
        Skip {
            names: self.names || other.names,
            paths: self.paths.union(&other.paths).cloned().collect(),
        }
    }

    /// The folders in the output of `git ls-files -z --others --ignored
    /// --exclude-standard --directory`. Ignored single files stay in the
    /// snapshot.
    pub fn from_git(out: &str) -> Skip {
        Skip {
            names: false,
            paths: out
                .split('\0')
                .filter_map(|p| p.strip_suffix('/'))
                .filter(|p| !p.is_empty())
                .map(str::to_string)
                .collect(),
        }
    }
}

/// The skip rules of `project`, from git's own view when it is a git work
/// tree that paddock can query safely, else the fixed folder names.
pub fn skip_rules(project: &Path, limit: Duration) -> Skip {
    let fallback = Skip {
        names: true,
        paths: BTreeSet::new(),
    };
    let Ok(Some(layout)) = crate::hostgit::Layout::discover(project) else {
        return fallback;
    };
    if layout.guard().is_err() {
        return fallback;
    }
    let mut cmd = crate::hostgit::hardened(&layout.git_dir, Some(&layout.work_tree));
    cmd.current_dir(project).args([
        "ls-files",
        "-z",
        "--others",
        "--ignored",
        "--exclude-standard",
        "--directory",
    ]);
    match crate::broker::run_capture(&mut cmd, limit) {
        Ok((0, out, _)) => Skip::from_git(&out),
        _ => fallback,
    }
}

/// The files and symbolic links of `root`, without what `skip` names.
/// Other file types and names that are not UTF-8 are left out.
pub fn walk(root: &Path, skip: &Skip, deadline: Option<Instant>) -> Result<BTreeMap<String, Fp>> {
    let mut out = BTreeMap::new();
    walk_in(root, "", skip, deadline, &mut out)?;
    Ok(out)
}

fn walk_in(
    dir: &Path,
    rel: &str,
    skip: &Skip,
    deadline: Option<Instant>,
    out: &mut BTreeMap<String, Fp>,
) -> Result<()> {
    if deadline.is_some_and(|d| Instant::now() > d) {
        bail!("the time limit ran out");
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if rel.is_empty() => {
            return Err(e).with_context(|| format!("cannot read {}", dir.display()))
        }
        Err(_) => return Ok(()),
    };
    for e in entries.flatten() {
        let Ok(name) = e.file_name().into_string() else {
            continue;
        };
        let r = if rel.is_empty() {
            name.clone()
        } else {
            format!("{rel}/{name}")
        };
        let Ok(m) = std::fs::symlink_metadata(e.path()) else {
            continue;
        };
        if skip.skips(&r, &name, m.is_dir()) {
            continue;
        }
        if m.is_dir() {
            walk_in(&e.path(), &r, skip, deadline, out)?;
        } else if m.is_symlink() {
            let Ok(t) = std::fs::read_link(e.path()) else {
                continue;
            };
            let Some(t) = t.to_str() else { continue };
            out.insert(r, Fp::of(&m, Some(t.to_string())));
        } else if m.is_file() {
            out.insert(r, Fp::of(&m, None));
        }
    }
    Ok(())
}

/// The record of one snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    pub id: String,
    pub harness: String,
    pub project: PathBuf,
    pub started: u64,
    #[serde(default)]
    pub ended: Option<u64>,
    /// The copies share their data with the project files.
    pub reflink: bool,
    /// Bytes of the files, as the project has them.
    pub bytes: u64,
    pub skip: Skip,
    pub files: BTreeMap<String, Fp>,
    /// The state of the project when the session ended.
    #[serde(default)]
    pub end: Option<BTreeMap<String, Fp>>,
}

impl Meta {
    pub fn load(dir: &Path) -> Result<Meta> {
        let f = dir.join(META);
        let text =
            std::fs::read_to_string(&f).with_context(|| format!("cannot read {}", f.display()))?;
        serde_json::from_str(&text).with_context(|| format!("{} is invalid", f.display()))
    }

    fn save(&self, dir: &Path) -> Result<()> {
        crate::privfs::write_atomic(&dir.join(META), serde_json::to_vec(self)?)?;
        Ok(())
    }
}

/// How to take a snapshot.
#[derive(Debug, Clone)]
pub struct Opts {
    pub max: u64,
    pub time_limit: Duration,
    /// Try `FICLONE`. Off in tests that check the plain copy.
    pub reflink: bool,
}

impl Opts {
    pub fn from_config(c: &Config) -> Self {
        Opts {
            max: c.max,
            time_limit: TIME_LIMIT,
            reflink: true,
        }
    }
}

#[derive(Debug)]
pub enum Taken {
    Done {
        dir: PathBuf,
        files: usize,
        bytes: u64,
        reflink: bool,
    },
    /// Not taken, with the reason for the notice.
    Skipped(String),
}

/// Share the data of `src` with `dst`.
fn clone_file(src: &File, dst: &File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: two open file descriptors; FICLONE reads only the source fd.
    let rc = unsafe { libc::ioctl(dst.as_raw_fd(), FICLONE as _, src.as_raw_fd()) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Open a regular file without following a link or blocking on a FIFO.
fn open_regular(p: &Path) -> std::io::Result<File> {
    let f = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(p)?;
    if !f.metadata()?.is_file() {
        return Err(std::io::Error::other("not a regular file"));
    }
    Ok(f)
}

/// Copy one file: shared data when `reflink` and the file system allow it,
/// else a full copy when it fits in `budget`. `Ok(None)`: it does not fit,
/// and `dst` is gone again. `Ok(Some(true))`: shared data.
fn copy_file(
    src: &File,
    dst: &Path,
    mode: u32,
    mtime: i64,
    reflink: bool,
    budget: &mut u64,
) -> Result<Option<bool>> {
    let out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(dst)
        .with_context(|| format!("cannot create {}", dst.display()))?;
    let shared = reflink && clone_file(src, &out).is_ok();
    if !shared {
        let size = src.metadata()?.len();
        if size > *budget {
            drop(out);
            let _ = std::fs::remove_file(dst);
            return Ok(None);
        }
        *budget -= size;
        let mut s = src;
        std::io::copy(&mut s, &mut &out)?;
    }
    let t = std::time::UNIX_EPOCH + Duration::from_nanos(u64::try_from(mtime).unwrap_or(0));
    let _ = out.set_modified(t);
    out.set_permissions(std::fs::Permissions::from_mode(mode & 0o7777))?;
    Ok(Some(shared))
}

/// A copy without a size limit.
fn copy_any(src: &File, dst: &Path, mode: u32, mtime: i64) -> Result<()> {
    let mut unlimited = u64::MAX;
    copy_file(src, dst, mode, mtime, true, &mut unlimited)?;
    Ok(())
}

enum Copied {
    /// All files; true when all share their data with the project.
    Done(bool),
    TooBig,
    Slow,
}

/// Take a snapshot of `project` for session `id` into `root`.
pub fn take(
    root: &Path,
    project: &Path,
    id: &str,
    harness: &str,
    started: u64,
    opts: &Opts,
) -> Result<Taken> {
    let deadline = Instant::now() + opts.time_limit;
    let mut skip = skip_rules(project, opts.time_limit);
    let mut files = match walk(project, &skip, Some(deadline)) {
        Ok(f) => f,
        Err(e) if Instant::now() > deadline => {
            return Ok(Taken::Skipped(format!(
                "the snapshot took longer than {} s: {e}",
                opts.time_limit.as_secs()
            )))
        }
        Err(e) => return Err(e),
    };
    let total: u64 = files.values().map(|f| f.size).sum();
    let base = project_dir(root, project);
    let dir = base.join(id);
    let tmp = base.join(format!(".{id}.tmp"));
    let _ = std::fs::remove_dir_all(&tmp);
    crate::privfs::dir(&tmp.join(FILES))?;
    // Without shared data, a project over the limit is not worth a start.
    let mut budget = opts.max;
    let result = (|| -> Result<Copied> {
        if !opts.reflink && total > opts.max {
            return Ok(Copied::TooBig);
        }
        // Try to share data until the first file that cannot.
        let mut reflink = opts.reflink;
        let mut unreadable = Vec::new();
        let mut made: BTreeSet<PathBuf> = BTreeSet::new();
        for (rel, fp) in files.iter_mut() {
            if Instant::now() > deadline {
                return Ok(Copied::Slow);
            }
            let dst = tmp.join(FILES).join(rel);
            if let Some(parent) = dst.parent() {
                if made.insert(parent.to_path_buf()) {
                    crate::privfs::dir(parent)?;
                }
            }
            if let Some(t) = &fp.link {
                std::os::unix::fs::symlink(t, &dst)?;
                continue;
            }
            let Ok(src) = open_regular(&project.join(rel)) else {
                unreadable.push(rel.clone());
                continue;
            };
            *fp = Fp::of(&src.metadata()?, None);
            match copy_file(&src, &dst, fp.mode, fp.mtime, reflink, &mut budget)? {
                None => return Ok(Copied::TooBig),
                Some(false) if fp.size > 0 => reflink = false,
                Some(_) => {}
            }
        }
        for r in unreadable {
            files.remove(&r);
            skip.paths.insert(r);
        }
        Ok(Copied::Done(reflink))
    })();
    let shared = match result {
        Ok(Copied::Done(r)) => r,
        Ok(Copied::TooBig) => {
            let _ = std::fs::remove_dir_all(&tmp);
            return Ok(Taken::Skipped(format!(
                "the file system cannot share file data, and a copy of the project ({}) passes the limit [snapshot] max = {}",
                crate::storage::human(total),
                crate::storage::human(opts.max)
            )));
        }
        Ok(Copied::Slow) => {
            let _ = std::fs::remove_dir_all(&tmp);
            return Ok(Taken::Skipped(format!(
                "the snapshot took longer than {} s",
                opts.time_limit.as_secs()
            )));
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(e);
        }
    };
    let meta = Meta {
        id: id.to_string(),
        harness: harness.to_string(),
        project: project.to_path_buf(),
        started,
        ended: None,
        reflink: shared,
        bytes: files.values().map(|f| f.size).sum(),
        skip,
        files,
        end: None,
    };
    meta.save(&tmp)?;
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::rename(&tmp, &dir)?;
    prune(&base, KEEP);
    Ok(Taken::Done {
        dir,
        files: meta.files.len(),
        bytes: meta.bytes,
        reflink: meta.reflink,
    })
}

/// The snapshots of one project, newest first.
pub fn list(base: &Path) -> Vec<(PathBuf, Meta)> {
    let mut out: Vec<(PathBuf, Meta)> = std::fs::read_dir(base)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .filter_map(|e| Meta::load(&e.path()).ok().map(|m| (e.path(), m)))
        .collect();
    out.sort_by(|a, b| b.1.started.cmp(&a.1.started).then(b.1.id.cmp(&a.1.id)));
    out
}

/// Keep the newest `keep` snapshots of one project.
pub fn prune(base: &Path, keep: usize) {
    for (dir, _) in list(base).into_iter().skip(keep) {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// At the end of a session: record the state of the project. Returns the
/// number of files that the session changed, added or deleted.
pub fn finish(root: &Path, project: &Path, id: &str, ended: u64) -> Result<Option<usize>> {
    let dir = project_dir(root, project).join(id);
    if !dir.join(META).is_file() {
        return Ok(None);
    }
    let mut meta = Meta::load(&dir)?;
    let skip = meta.skip.union(&skip_rules(project, TIME_LIMIT));
    meta.ended = Some(ended);
    let now = walk(project, &skip, Some(Instant::now() + TIME_LIMIT)).ok();
    let changes = now.as_ref().map(|n| plan(&meta, n).entries.len());
    meta.end = now;
    meta.save(&dir)?;
    Ok(changes)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Change {
    Added,
    Changed,
    Deleted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Entry {
    pub path: String,
    pub change: Change,
    /// The file is different from the state at the end of the session: the
    /// user or another agent changed it later.
    pub after_end: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Plan {
    pub entries: Vec<Entry>,
}

impl Plan {
    pub fn of(&self, c: Change) -> Vec<&Entry> {
        self.entries.iter().filter(|e| e.change == c).collect()
    }
}

/// The changes between the snapshot and `current`.
pub fn plan(meta: &Meta, current: &BTreeMap<String, Fp>) -> Plan {
    let mut entries = Vec::new();
    let paths: BTreeSet<&String> = meta.files.keys().chain(current.keys()).collect();
    for p in paths {
        let change = match (meta.files.get(p), current.get(p)) {
            (Some(_), None) => Change::Deleted,
            (None, Some(_)) => Change::Added,
            (Some(a), Some(b)) if a != b => Change::Changed,
            _ => continue,
        };
        let after_end = meta
            .end
            .as_ref()
            .is_some_and(|end| end.get(p) != current.get(p));
        entries.push(Entry {
            path: p.clone(),
            change,
            after_end,
        });
    }
    Plan { entries }
}

/// What `restore` did.
#[derive(Debug, Default, Serialize)]
pub struct Restored {
    pub restored: Vec<String>,
    pub trashed: Vec<String>,
    pub skipped: Vec<String>,
    pub trash: Option<PathBuf>,
}

/// Make the parent folders of `rel` in `project`. Each existing one must be
/// a real folder, not a link.
fn parents(project: &Path, rel: &str) -> Result<()> {
    let mut p = project.to_path_buf();
    let comps: Vec<&str> = rel.split('/').collect();
    for c in &comps[..comps.len().saturating_sub(1)] {
        p.push(c);
        match std::fs::symlink_metadata(&p) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => bail!("{} is not a folder", p.display()),
            Err(_) => {
                std::fs::create_dir(&p).with_context(|| format!("cannot create {}", p.display()))?
            }
        }
    }
    Ok(())
}

/// Move `from` to `to`, across file systems too.
fn move_path(from: &Path, to: &Path) -> Result<()> {
    if let Some(d) = to.parent() {
        crate::privfs::dir(d)?;
    }
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    let m = std::fs::symlink_metadata(from)?;
    if m.is_symlink() {
        std::os::unix::fs::symlink(std::fs::read_link(from)?, to)?;
    } else {
        let src = open_regular(from)?;
        copy_any(&src, to, m.mode(), Fp::of(&m, None).mtime)?;
    }
    std::fs::remove_file(from)?;
    Ok(())
}

/// Remove the folders that `rel` left empty, up to the first one that the
/// snapshot has.
fn remove_empty_parents(project: &Path, snap: &Path, rel: &str) {
    let mut r = Path::new(rel).parent();
    while let Some(d) = r.filter(|d| !d.as_os_str().is_empty()) {
        if snap.join(d).is_dir() || std::fs::remove_dir(project.join(d)).is_err() {
            break;
        }
        r = d.parent();
    }
}

/// Bring the project back to the snapshot in `dir`. Added files go to a new
/// folder below `trash_root`. Entries changed after the end of the session
/// stay unless `force`.
pub fn restore(
    dir: &Path,
    meta: &Meta,
    project: &Path,
    plan: &Plan,
    force: bool,
    trash_root: &Path,
    stamp: &str,
) -> Result<Restored> {
    let snap = dir.join(FILES);
    let mut done = Restored::default();
    let todo: Vec<&Entry> = plan
        .entries
        .iter()
        .filter(|e| {
            if e.after_end && !force {
                done.skipped.push(e.path.clone());
                false
            } else {
                true
            }
        })
        .collect();
    // Added files first: an added link or file can sit where the snapshot
    // has a folder.
    for e in todo.iter().filter(|e| e.change == Change::Added) {
        let trash = trash_root.join(stamp);
        let from = project.join(&e.path);
        parents(project, &e.path)?;
        move_path(&from, &trash.join(&e.path))
            .with_context(|| format!("cannot move {} to the trash", from.display()))?;
        remove_empty_parents(project, &snap, &e.path);
        done.trash = Some(trash);
        done.trashed.push(e.path.clone());
    }
    for e in todo.iter().filter(|e| e.change != Change::Added) {
        let Some(fp) = meta.files.get(&e.path) else {
            continue;
        };
        let to = project.join(&e.path);
        parents(project, &e.path)?;
        let tmp = to.with_file_name(format!(
            ".{}.paddock-undo",
            to.file_name().unwrap_or_default().to_string_lossy()
        ));
        let _ = std::fs::remove_file(&tmp);
        if let Some(t) = &fp.link {
            std::os::unix::fs::symlink(t, &tmp)?;
        } else {
            let src = open_regular(&snap.join(&e.path))
                .with_context(|| format!("the snapshot has no copy of {}", e.path))?;
            copy_any(&src, &tmp, fp.mode, fp.mtime)?;
        }
        if let Ok(m) = std::fs::symlink_metadata(&to) {
            if m.is_dir() {
                let _ = std::fs::remove_file(&tmp);
                bail!(
                    "{} is a folder now; move it away and run undo again",
                    to.display()
                );
            }
        }
        std::fs::rename(&tmp, &to).with_context(|| format!("cannot restore {}", to.display()))?;
        done.restored.push(e.path.clone());
    }
    Ok(done)
}

/// Names for a summary line: the first `n`, then `+N more`.
pub fn names(entries: &[&Entry], n: usize) -> String {
    let mut s: Vec<String> = entries.iter().take(n).map(|e| e.path.clone()).collect();
    if entries.len() > n {
        s.push(format!("+{} more", entries.len() - n));
    }
    s.join(", ")
}

/// The text that `paddock undo` shows before it asks.
pub fn render_plan(meta: &Meta, plan: &Plan) -> String {
    let mut t = format!(
        "Changes since the snapshot of {} ({}, {}):\n",
        meta.id,
        meta.harness,
        time(meta.started)
    );
    for (c, label) in [
        (Change::Added, "added (moved to the trash)"),
        (Change::Changed, "changed (restored)"),
        (Change::Deleted, "deleted (restored)"),
    ] {
        let v: Vec<&Entry> = plan.of(c).into_iter().filter(|e| !e.after_end).collect();
        if !v.is_empty() {
            t.push_str(&format!("  {} {label}: {}\n", v.len(), names(&v, 10)));
        }
    }
    let late: Vec<&Entry> = plan.entries.iter().filter(|e| e.after_end).collect();
    if !late.is_empty() {
        t.push_str(&format!(
            "  warning: {} changed after the session ended (skipped without --force): {}\n",
            late.len(),
            names(&late, 10)
        ));
    }
    t
}

/// "2026-11-02 14:05" in local time.
pub fn time(unix: u64) -> String {
    crate::storage::local_time(unix)
}

/// `paddock undo --list`.
pub fn render_list(all: &[(PathBuf, Meta)]) -> String {
    if all.is_empty() {
        return "No snapshots of this project.\n".into();
    }
    let mut t = String::new();
    for (_, m) in all {
        t.push_str(&format!(
            "{}  {:<10}  {:>9}{}  {}\n",
            time(m.started),
            m.harness,
            crate::storage::human(m.bytes),
            if m.reflink { " shared" } else { "       " },
            m.id
        ));
    }
    t
}

/// The snapshot step before a sandbox starts. Prints a notice when it
/// cannot take one; never stops the start.
pub fn before_start(env: &HostEnv, project: &Path, id: &str, harness: &str, quiet: bool) {
    let cfg = match Config::load(env) {
        Ok(c) => c,
        Err(e) => {
            if !quiet {
                eprintln!("paddock: no snapshot: {e:#}");
            }
            return;
        }
    };
    if !cfg.enabled {
        return;
    }
    let r = take(
        &root(env),
        project,
        id,
        harness,
        crate::state::now(),
        &Opts::from_config(&cfg),
    );
    match r {
        Ok(Taken::Done { .. }) => {}
        Ok(Taken::Skipped(why)) if !quiet => {
            eprintln!("paddock: no snapshot of the project, so `paddock undo` cannot help for this run: {why}")
        }
        Ok(Taken::Skipped(_)) => {}
        Err(e) if !quiet => eprintln!("paddock: no snapshot of the project: {e:#}"),
        Err(_) => {}
    }
}

/// The snapshot step after a sandbox ends: record the end state, and name
/// `paddock undo` when the run changed files.
pub fn after_end(env: &HostEnv, project: &Path, id: &str, quiet: bool) {
    match finish(&root(env), project, id, crate::state::now()) {
        Ok(Some(n)) if n > 0 && !quiet => eprintln!(
            "paddock: the run changed {n} file(s) in the project. `paddock undo` brings back the state before the start."
        ),
        Ok(_) => {}
        Err(e) => log::warn!("cannot record the end of the snapshot: {e:#}"),
    }
}

/// `paddock undo`.
pub fn undo_cmd(
    env: &HostEnv,
    session: Option<&str>,
    list_only: bool,
    yes: bool,
    force: bool,
) -> Result<i32> {
    let project = crate::build::resolve_project(None)?;
    let base = project_dir(&root(env), &project);
    let all = list(&base);
    if list_only {
        print!("{}", render_list(&all));
        return Ok(0);
    }
    let Some((dir, meta)) = (match session {
        Some(s) => all.iter().find(|(_, m)| m.id == s),
        None => all.first(),
    }) else {
        match session {
            Some(s) => bail!("no snapshot {s} for this project; `paddock undo --list` shows them"),
            None => bail!("no snapshot for this project"),
        }
    };
    if crate::state::Running::list(env)
        .iter()
        .any(|r| r.project == project)
    {
        bail!("a sandbox of this project runs; stop it first (`paddock ps`)");
    }
    let skip = meta.skip.union(&skip_rules(&project, TIME_LIMIT));
    let current = walk(&project, &skip, None)?;
    let p = plan(meta, &current);
    if p.entries.is_empty() {
        println!("The project is the same as the snapshot of {}.", meta.id);
        return Ok(0);
    }
    print!("{}", render_plan(meta, &p));
    if !yes {
        if !(std::io::stdin().is_terminal() && std::io::stderr().is_terminal()) {
            bail!("no terminal to ask; run `paddock undo --yes` to restore");
        }
        use crate::setup::Ask;
        if !crate::setup::Terminal.yes_no("Restore the project?", false) {
            return Ok(1);
        }
    }
    let stamp = format!("{}-{}", crate::state::now(), meta.id);
    let r = restore(dir, meta, &project, &p, force, &trash_root(env), &stamp)?;
    println!(
        "Restored {} file(s), moved {} added file(s) to the trash.",
        r.restored.len(),
        r.trashed.len()
    );
    if let Some(t) = &r.trash {
        println!("Trash: {}", t.display());
    }
    if !r.skipped.is_empty() {
        println!(
            "Skipped {} file(s) that changed after the session ended; `--force` restores them too.",
            r.skipped.len()
        );
    }
    Ok(0)
}

#[cfg(test)]
mod tests;
