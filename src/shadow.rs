//! `--profile default`: a harness runs on a private snapshot of its own host
//! data (shadow mode).
//!
//! - The data roots of the harness (`[state] data` of the profile, plus
//!   `[profiles.NAME] data` of the user configuration) are copied into
//!   `<paddock data>/shadow/runs/ID/lower`, at the same paths relative to
//!   the home. A copy is a reflink where the file system has them.
//! - The sandbox home is one overlay: that snapshot below, the run's
//!   `upper` and `work` folders above. paddock puts the credential files
//!   into `upper` before the start, as for every sandbox.
//! - At the end paddock prints what the run changed in `upper` and removes
//!   the run folder. Nothing goes back to the host.
//!
//! The real home is never visible: only the data roots come in. paddock
//! knows no vendor format here: folders and files come in as bytes.

use crate::env::HostEnv;
use crate::profile::Profile;
use crate::safefs::{Dir, Kind, Root};
use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

/// The value of `--profile` that turns shadow mode on.
pub const DEFAULT: &str = "default";

/// Folders that hold the data of many programs. A data root is never one of
/// them, only a folder inside.
pub const SHARED_PARENTS: &[&str] = &[
    ".config",
    ".local",
    ".local/share",
    ".local/state",
    ".cache",
    ".agents",
];

/// Folders with keys and passwords. A data root is never one of them or
/// inside one.
pub const SECRET_DIRS: &[&str] = &[".ssh", ".gnupg", ".pki", ".password-store"];

/// The size cap of a snapshot without reflinks, in MiB.
pub const DEFAULT_LIMIT_MIB: u64 = 2048;

/// Folders deeper than this are not copied.
const MAX_DEPTH: usize = 256;

/// How many changed paths the summary names.
const SUMMARY_PATHS: usize = 20;

/// The folder of all shadow runs.
pub fn runs_root(env: &HostEnv) -> PathBuf {
    env.paddock_data().join("shadow").join("runs")
}

/// The run folder of the sandbox `id`.
pub fn run_dir(env: &HostEnv, id: &str) -> PathBuf {
    runs_root(env).join(id)
}

/// Whether the sandbox `id` runs in shadow mode, for `paddock ps`.
pub fn is_shadow(env: &HostEnv, id: &str) -> bool {
    !id.is_empty() && !id.contains('/') && run_dir(env, id).is_dir()
}

/// Check one data root, relative to the home.
pub fn validate_root(env: &HostEnv, rel: &str) -> Result<()> {
    let p = Path::new(rel);
    let bad = |why: &str| -> Result<()> {
        bail!("the data root '{rel}' is refused: {why}");
    };
    if rel.trim().is_empty() || p.is_absolute() {
        return bad("it must be a path relative to the home");
    }
    if p.components().any(|c| !matches!(c, Component::Normal(_))) {
        return bad("it must be a plain relative path, without '..' or '.'");
    }
    let norm: PathBuf = p.components().collect();
    if norm.as_os_str().is_empty() {
        return bad("it is the home itself");
    }
    if SHARED_PARENTS.iter().any(|s| norm == Path::new(s)) {
        return bad("it holds the data of many programs; name a folder inside it");
    }
    if SECRET_DIRS.iter().any(|s| norm.starts_with(s)) {
        return bad("it holds keys or passwords");
    }
    if crate::policy::DENIED_HOME
        .iter()
        .any(|d| norm.starts_with(d) || Path::new(d).starts_with(&norm))
    {
        return bad("it overlaps a path that paddock never mounts");
    }
    let full = env.home.join(&norm);
    for own in [
        env.paddock_data(),
        env.paddock_cache(),
        env.paddock_state(),
        env.paddock_config(),
        env.paddock_run(),
    ] {
        if full.starts_with(&own) || own.starts_with(&full) {
            return bad("it holds paddock's own folders");
        }
    }
    Ok(())
}

/// `[profiles.NAME] data` of the user configuration text.
pub fn user_roots_in(text: &str, harness: &str) -> Result<Vec<String>> {
    let v: toml::Value = toml::from_str(text).context("the user configuration is invalid")?;
    let Some(t) = v.get("profiles").and_then(|p| p.get(harness)) else {
        return Ok(Vec::new());
    };
    let Some(list) = t.get("data") else {
        return Ok(Vec::new());
    };
    let arr = list
        .as_array()
        .with_context(|| format!("[profiles.{harness}] data must be a list of paths"))?;
    arr.iter()
        .map(|x| {
            x.as_str()
                .map(str::to_string)
                .with_context(|| format!("[profiles.{harness}] data must be a list of paths"))
        })
        .collect()
}

fn user_config(env: &HostEnv) -> String {
    std::fs::read_to_string(env.paddock_config().join("config.toml")).unwrap_or_default()
}

/// The checked data roots of a harness: the profile's, then the user's.
pub fn roots(env: &HostEnv, profile: &Profile) -> Result<Vec<String>> {
    let mut all = profile.state.data.clone();
    all.extend(user_roots_in(&user_config(env), &profile.name)?);
    let mut out: Vec<String> = Vec::new();
    for r in all {
        validate_root(env, &r)?;
        let norm: PathBuf = Path::new(&r).components().collect();
        let s = norm.to_string_lossy().into_owned();
        if !out.contains(&s) {
            out.push(s);
        }
    }
    if out.is_empty() {
        bail!(
            "the {} profile has no data roots ([state] data), so --profile default does not know which host data to copy. Add them with [profiles.{}] data = [...] in the user configuration",
            profile.name,
            profile.name
        );
    }
    Ok(out)
}

/// `[storage] shadow_copy_limit_mib` of the user configuration, in bytes.
pub fn copy_limit(env: &HostEnv) -> u64 {
    copy_limit_in(&user_config(env))
}

pub fn copy_limit_in(text: &str) -> u64 {
    let mib = toml::from_str::<toml::Value>(text)
        .ok()
        .and_then(|v| v.get("storage")?.get("shadow_copy_limit_mib")?.as_integer())
        .filter(|n| *n >= 0)
        .map(|n| n as u64)
        .unwrap_or(DEFAULT_LIMIT_MIB);
    mib.saturating_mul(1 << 20)
}

/// What the spec records of a shadow run.
#[derive(Debug, Clone, Serialize)]
pub struct Plan {
    pub profile: String,
    pub dir: PathBuf,
    pub lower: PathBuf,
    pub upper: PathBuf,
    pub work: PathBuf,
    /// The data roots, relative to the home.
    pub roots: Vec<String>,
    /// The credential files (`[state] auth`), never in the snapshot.
    pub auth: Vec<String>,
}

impl Plan {
    /// Make the empty run folder of the sandbox `id`. It must be on the
    /// file system of the home, so a copy can be a reflink.
    pub fn create(env: &HostEnv, id: &str, roots: Vec<String>, auth: Vec<String>) -> Result<Self> {
        if id.is_empty() || id.contains('/') || id.starts_with('.') {
            bail!("invalid sandbox id {id}");
        }
        let dir = run_dir(env, id);
        crate::privfs::dir(&env.paddock_data())?;
        crate::privfs::dir(&env.paddock_data().join("shadow"))?;
        crate::privfs::dir(&runs_root(env))?;
        if dir.exists() {
            remove_dir(&dir)?;
        }
        crate::privfs::dir(&dir)?;
        let plan = Self {
            profile: DEFAULT.into(),
            lower: dir.join("lower"),
            upper: dir.join("upper"),
            work: dir.join("work"),
            dir,
            roots,
            auth,
        };
        for d in [&plan.lower, &plan.upper, &plan.work] {
            crate::privfs::dir(d)?;
        }
        let home_dev = std::fs::metadata(&env.home)
            .with_context(|| format!("cannot read {}", env.home.display()))?
            .dev();
        let run_dev = std::fs::metadata(&plan.dir)?.dev();
        if home_dev != run_dev {
            let d = plan.dir.clone();
            let _ = remove_dir(&d);
            bail!(
                "--profile default needs its run folder on the file system of the home, and {} is on another one. Set XDG_DATA_HOME to a folder on the file system of the home",
                runs_root(env).display()
            );
        }
        Ok(plan)
    }

    /// The root that holds the home-relative path `rel`, if any.
    pub fn root_of(&self, rel: &Path) -> Option<&str> {
        self.roots
            .iter()
            .find(|r| rel.starts_with(Path::new(r)))
            .map(String::as_str)
    }

    fn is_auth(&self, rel: &Path) -> bool {
        self.auth.iter().any(|a| {
            let a = a.trim_end_matches('/');
            rel.starts_with(Path::new(a))
        })
    }

    /// Copy the data roots of `home` into the lower folder. `reflink`
    /// None: probe the file system; tests force it.
    pub fn fill(&self, home: &Path, limit: u64, reflink: Option<bool>) -> Result<Copied> {
        let root = Root::open(home)?;
        let lower = Root::open(&self.lower)?;
        // Pass one: the size and the first file, without copying.
        let mut size = Sizer::default();
        self.walk(home, &root, &mut size)?;
        let reflinks = match reflink {
            Some(r) => r,
            None => match &size.first {
                Some(rel) => probe_reflink(&root, &self.lower, rel),
                None => true,
            },
        };
        if !reflinks && size.bytes > limit {
            bail!(
                "--profile default copies {} of host data, and this file system has no reflinks, so each byte is a real copy. The cap is {}. Raise [storage] shadow_copy_limit_mib in the user configuration, or remove old data from the data roots ({})",
                human(size.bytes),
                human(limit),
                self.roots.join(", ")
            );
        }
        let mut copier = Copier {
            lower,
            reflinks,
            out: Copied {
                reflinks,
                ..Default::default()
            },
        };
        self.walk(home, &root, &mut copier)?;
        Ok(copier.out)
    }

    fn walk(&self, home_path: &Path, home: &Root, v: &mut dyn Visit) -> Result<()> {
        let home_dir = home_path;
        for r in &self.roots {
            let rel = PathBuf::from(r);
            let parent = rel.parent().unwrap_or(Path::new(""));
            let Some(name) = rel.file_name() else {
                continue;
            };
            let dir = match home.dir(parent, false) {
                Ok(d) => d,
                Err(_) => continue,
            };
            let stats = dir.stats()?;
            let Some((_, st)) = stats.iter().find(|(n, _)| n == name) else {
                continue;
            };
            if self.is_auth(&rel) {
                continue;
            }
            match st.kind {
                Kind::Dir => {
                    v.dir(&rel)?;
                    let sub = dir.subdir(name, false)?;
                    self.walk_dir(home_dir, &sub, &rel, &rel, v, 1)?;
                }
                Kind::File => v.file(&dir, name, &rel, st.size)?,
                // A root that is a symlink leaves the root.
                Kind::Symlink => v.skip_link(&rel),
                Kind::Socket | Kind::Other => v.skip_special(&rel),
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn walk_dir(
        &self,
        home: &Path,
        dir: &Dir,
        rel: &Path,
        root: &Path,
        v: &mut dyn Visit,
        depth: usize,
    ) -> Result<()> {
        if depth > MAX_DEPTH {
            bail!("{} is deeper than {MAX_DEPTH} folders", rel.display());
        }
        let mut stats = dir.stats()?;
        stats.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, st) in stats {
            let r = rel.join(&name);
            if self.is_auth(&r) {
                continue;
            }
            match st.kind {
                Kind::Dir => {
                    let Ok(sub) = dir.subdir(&name, false) else {
                        continue;
                    };
                    v.dir(&r)?;
                    self.walk_dir(home, &sub, &r, root, v, depth + 1)?;
                }
                Kind::File => v.file(dir, &name, &r, st.size)?,
                Kind::Symlink => match dir.read_link(&name) {
                    Ok(t) if link_stays_inside(home, &r, &t, root) => v.link(&r, &t)?,
                    _ => v.skip_link(&r),
                },
                Kind::Socket | Kind::Other => v.skip_special(&r),
            }
        }
        Ok(())
    }

    /// Before paddock writes a settings file into the upper folder, copy
    /// the snapshot's version up, so the write changes it instead of
    /// hiding it.
    pub fn copy_up(&self, rel: &Path) -> Result<()> {
        if self.upper.join(rel).symlink_metadata().is_ok() {
            return Ok(());
        }
        let Ok(m) = self.lower.join(rel).symlink_metadata() else {
            return Ok(());
        };
        if !m.is_file() {
            return Ok(());
        }
        let parent = rel.parent().unwrap_or(Path::new(""));
        let upper = Root::open(&self.upper)?;
        let low = Root::open(&self.lower)?;
        let to = upper.dir(parent, true)?;
        let from = low.dir(parent, false)?;
        let name = rel.file_name().context("no file name")?;
        let Some(src) = from.open_read(name)? else {
            return Ok(());
        };
        copy_file(&src, &to, name, true)?;
        Ok(())
    }

    /// The files in the upper folder now, to leave paddock's own writes out
    /// of the summary.
    pub fn baseline(&self) -> BTreeMap<PathBuf, (u64, (i64, i64))> {
        let mut out = BTreeMap::new();
        if let Ok(root) = Root::open(&self.upper) {
            if let Ok(d) = root.dir(Path::new(""), false) {
                let _ = scan_upper(
                    &d,
                    Path::new(""),
                    &mut |rel, st, _| {
                        out.insert(rel.to_path_buf(), (st.size, st.mtime));
                    },
                    0,
                );
            }
        }
        out
    }

    /// What the run changed in the upper folder, without paddock's own
    /// writes from before the start (`base`) and the mount points that
    /// bwrap made for the mounts at `skip` (relative to the home).
    pub fn summary(
        &self,
        base: &BTreeMap<PathBuf, (u64, (i64, i64))>,
        skip: &[PathBuf],
    ) -> Summary {
        let mut s = Summary::default();
        let Ok(root) = Root::open(&self.upper) else {
            return s;
        };
        let Ok(d) = root.dir(Path::new(""), false) else {
            return s;
        };
        let mut all = Vec::new();
        let _ = scan_upper(
            &d,
            Path::new(""),
            &mut |rel, st, whiteout| {
                if base.get(rel) == Some(&(st.size, st.mtime))
                    || (st.size == 0 && skip.iter().any(|m| m == rel))
                {
                    return;
                }
                let what = if whiteout {
                    Change::Deleted
                } else if self.lower.join(rel).symlink_metadata().is_ok() {
                    Change::Changed
                } else {
                    Change::Added
                };
                all.push((what, rel.to_path_buf()));
            },
            0,
        );
        all.sort_by(|a, b| a.1.cmp(&b.1));
        for (what, rel) in &all {
            match what {
                Change::Added => s.added += 1,
                Change::Changed => s.changed += 1,
                Change::Deleted => s.deleted += 1,
            }
            if self.root_of(rel).is_none() {
                s.outside += 1;
            }
        }
        s.paths = all.into_iter().take(SUMMARY_PATHS).collect();
        s
    }

    /// Remove the run folder.
    pub fn remove(&self) -> Result<()> {
        remove_dir(&self.dir)
    }
}

/// Remove a run folder whatever the modes inside (the overlay leaves a
/// work folder with mode 000, and a run can make read-only folders).
pub fn remove_dir(dir: &Path) -> Result<()> {
    if std::fs::symlink_metadata(dir).is_err() {
        return Ok(());
    }
    if std::fs::remove_dir_all(dir).is_ok() {
        return Ok(());
    }
    open_up(dir, 0);
    std::fs::remove_dir_all(dir).with_context(|| format!("cannot remove {}", dir.display()))
}

fn open_up(dir: &Path, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        if e.file_type().is_ok_and(|t| t.is_dir()) {
            open_up(&e.path(), depth + 1);
        }
    }
}

/// The sandbox ended or never started: remove its run folder.
pub fn discard(spec: &crate::spec::SandboxSpec) {
    if let Some(p) = &spec.shadow {
        if let Err(e) = p.remove() {
            log::warn!("{e:#}");
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Added,
    Changed,
    Deleted,
}

/// What a run changed in its private snapshot.
#[derive(Debug, Clone, Default)]
pub struct Summary {
    pub added: usize,
    pub changed: usize,
    pub deleted: usize,
    /// Changes outside the data roots.
    pub outside: usize,
    /// The first changed paths, relative to the home.
    pub paths: Vec<(Change, PathBuf)>,
}

impl Summary {
    pub fn total(&self) -> usize {
        self.added + self.changed + self.deleted
    }

    /// The lines for stderr.
    pub fn lines(&self) -> Vec<String> {
        let mut v = vec![format!(
            "profile default: the run added {}, changed {} and deleted {} file(s) in its private snapshot, {} of them outside the data roots. paddock removes the snapshot now, and nothing goes back to the host",
            self.added, self.changed, self.deleted, self.outside
        )];
        for (c, p) in &self.paths {
            let mark = match c {
                Change::Added => '+',
                Change::Changed => '~',
                Change::Deleted => '-',
            };
            v.push(format!("  {mark} ~/{}", p.display()));
        }
        if self.total() > self.paths.len() {
            v.push(format!("  and {} more", self.total() - self.paths.len()));
        }
        v
    }
}

/// What a snapshot copied.
#[derive(Debug, Clone, Default)]
pub struct Copied {
    pub files: usize,
    pub bytes: u64,
    pub reflinks: bool,
    /// Symlinks that leave their root.
    pub skipped_links: Vec<PathBuf>,
    /// Sockets, FIFOs and devices.
    pub skipped_special: Vec<PathBuf>,
}

impl Copied {
    pub fn line(&self) -> String {
        let mut s = format!(
            "profile default: a private snapshot of {} file(s), {}{}",
            self.files,
            human(self.bytes),
            if self.reflinks {
                ""
            } else {
                " (plain copies: no reflinks here)"
            }
        );
        if !self.skipped_links.is_empty() {
            s.push_str(&format!(
                ", {} symlink(s) that leave their root skipped",
                self.skipped_links.len()
            ));
        }
        if !self.skipped_special.is_empty() {
            s.push_str(&format!(
                ", {} socket(s), FIFO(s) or device(s) skipped",
                self.skipped_special.len()
            ));
        }
        s.push_str(". The run changes only this snapshot");
        s
    }
}

trait Visit {
    fn dir(&mut self, rel: &Path) -> Result<()>;
    fn file(&mut self, dir: &Dir, name: &OsStr, rel: &Path, size: u64) -> Result<()>;
    fn link(&mut self, _rel: &Path, _target: &Path) -> Result<()> {
        Ok(())
    }
    fn skip_link(&mut self, _rel: &Path) {}
    fn skip_special(&mut self, _rel: &Path) {}
}

#[derive(Default)]
struct Sizer {
    bytes: u64,
    first: Option<PathBuf>,
}

impl Visit for Sizer {
    fn dir(&mut self, _rel: &Path) -> Result<()> {
        Ok(())
    }
    fn file(&mut self, _dir: &Dir, _name: &OsStr, rel: &Path, size: u64) -> Result<()> {
        self.bytes = self.bytes.saturating_add(size);
        if self.first.is_none() && size > 0 {
            self.first = Some(rel.to_path_buf());
        }
        Ok(())
    }
}

struct Copier {
    lower: Root,
    reflinks: bool,
    out: Copied,
}

impl Visit for Copier {
    fn dir(&mut self, rel: &Path) -> Result<()> {
        self.lower.dir(rel, true)?;
        Ok(())
    }
    fn file(&mut self, dir: &Dir, name: &OsStr, rel: &Path, _size: u64) -> Result<()> {
        // A file that went away since the walk is no error.
        let Ok(Some(src)) = dir.open_read(name) else {
            return Ok(());
        };
        let parent = rel.parent().unwrap_or(Path::new(""));
        let to = self.lower.dir(parent, true)?;
        let n = copy_file(&src, &to, name, self.reflinks)
            .with_context(|| format!("cannot copy ~/{}", rel.display()))?;
        self.out.files += 1;
        self.out.bytes += n;
        Ok(())
    }
    fn link(&mut self, rel: &Path, target: &Path) -> Result<()> {
        let parent = rel.parent().unwrap_or(Path::new(""));
        let to = self.lower.dir(parent, true)?;
        to.symlink(target, rel.file_name().context("no file name")?)
    }
    fn skip_link(&mut self, rel: &Path) {
        self.out.skipped_links.push(rel.to_path_buf());
    }
    fn skip_special(&mut self, rel: &Path) {
        self.out.skipped_special.push(rel.to_path_buf());
    }
}

/// Whether the symlink at `rel` (relative to the home) with `target` stays
/// inside the data root `root`. Lexical: each symlink on the way is checked
/// on its own, so a chain of links inside the root stays inside.
pub fn link_stays_inside(home: &Path, rel: &Path, target: &Path, root: &Path) -> bool {
    // The home is at its host path in the sandbox too, so an absolute
    // target inside the root works there.
    let (start, target) = match target.strip_prefix(home) {
        Ok(t) if target.is_absolute() => (Path::new(""), t),
        _ if target.is_absolute() => return false,
        _ => (rel.parent().unwrap_or(Path::new("")), target),
    };
    let mut cur: Vec<&OsStr> = start
        .components()
        .filter_map(|c| match c {
            Component::Normal(n) => Some(n),
            _ => None,
        })
        .collect();
    for c in target.components() {
        match c {
            Component::Normal(n) => cur.push(n),
            Component::CurDir => {}
            Component::ParentDir => {
                if cur.pop().is_none() {
                    return false;
                }
            }
            _ => return false,
        }
    }
    let p: PathBuf = cur.iter().collect();
    p.starts_with(root)
}

const FICLONE: libc::c_ulong = 0x4004_9409;

fn reflink(src: &std::fs::File, dst: &std::fs::File) -> bool {
    use std::os::fd::AsRawFd;
    // SAFETY: FICLONE takes the source fd as its argument; both fds are open.
    unsafe { libc::ioctl(dst.as_raw_fd(), FICLONE as _, src.as_raw_fd()) == 0 }
}

/// Copy `src` into `to` as `name`, as a reflink when `reflinks`, else a
/// plain copy, with its mode and change time. Returns the bytes.
fn copy_file(src: &std::fs::File, to: &Dir, name: &OsStr, reflinks: bool) -> Result<u64> {
    let meta = src.metadata()?;
    let _ = to.remove(name);
    let mut dst = to.create_new(name, 0o600)?;
    let len = meta.len();
    if !(reflinks && reflink(src, &dst)) {
        let mut s = src;
        std::io::copy(&mut s, &mut dst)?;
    }
    dst.set_permissions(std::fs::Permissions::from_mode(meta.mode() & 0o7777))?;
    if let Ok(t) = meta.modified() {
        let _ = dst.set_modified(t);
    }
    Ok(len)
}

/// Whether a reflink of the home file `rel` into `lower` works.
fn probe_reflink(home: &Root, lower: &Path, rel: &Path) -> bool {
    let parent = rel.parent().unwrap_or(Path::new(""));
    let Some(name) = rel.file_name() else {
        return false;
    };
    let Ok(Some(src)) = home.dir(parent, false).and_then(|d| d.open_read(name)) else {
        return false;
    };
    let probe = lower.join(".paddock-reflink-probe");
    let _ = std::fs::remove_file(&probe);
    let Ok(dst) = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
    else {
        return false;
    };
    let ok = reflink(&src, &dst);
    drop(dst);
    let _ = std::fs::remove_file(&probe);
    ok
}

/// Every non-folder entry below `dir`: `f(rel, stat, whiteout)`.
fn scan_upper(
    dir: &Dir,
    rel: &Path,
    f: &mut dyn FnMut(&Path, &crate::safefs::Stat, bool),
    depth: usize,
) -> Result<()> {
    if depth > MAX_DEPTH {
        return Ok(());
    }
    let mut stats = dir.stats()?;
    stats.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, st) in stats {
        let r = rel.join(&name);
        match st.kind {
            Kind::Dir => {
                if let Ok(sub) = dir.subdir(&name, false) {
                    scan_upper(&sub, &r, f, depth + 1)?;
                }
            }
            // In an overlay upper folder, a character device 0/0 is a
            // whiteout: the run deleted the file.
            Kind::Other => f(&r, &st, true),
            _ => f(&r, &st, false),
        }
    }
    Ok(())
}

fn human(bytes: u64) -> String {
    const MIB: u64 = 1 << 20;
    const GIB: u64 = 1 << 30;
    if bytes >= GIB {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else {
        format!("{bytes} bytes")
    }
}

/// Host processes of this harness that run outside any sandbox now: same
/// user namespace as paddock, and the harness executable or one of its
/// command names.
pub fn host_copies(profile: &Profile, exe: &Path) -> Vec<u32> {
    let own_ns = std::fs::read_link("/proc/self/ns/user").ok();
    let me = std::process::id();
    let exe = std::fs::canonicalize(exe).unwrap_or_else(|_| exe.to_path_buf());
    let Ok(rd) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in rd.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        if pid == me {
            continue;
        }
        let base = e.path();
        if std::fs::read_link(base.join("ns/user")).ok() != own_ns {
            continue;
        }
        let by_exe = std::fs::read_link(base.join("exe")).is_ok_and(|p| p == exe);
        let by_name = std::fs::read(base.join("cmdline")).is_ok_and(|c| {
            let argv0 = c.split(|b| *b == 0).next().unwrap_or_default();
            let name = Path::new(OsStr::from_bytes(argv0))
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            profile.locate.commands.contains(&name)
        });
        if by_exe || by_name {
            out.push(pid);
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests;
