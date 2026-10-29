//! Sandbox artifacts: the files that an agent leaves in the private sandbox
//! home, and the way to bring them to the host.
//!
//! The project is bind-mounted, so writes there already land on the host.
//! Files that an agent saves to `~/Desktop`, `~/Downloads` and the other
//! recovery folders land in the sandbox home instead. paddock lists them, and
//! copies them out on request, the way Sandboxie Plus offers Quick Recovery.
//!
//! Safety rules:
//! - Recovery folders come from the user configuration only. A project
//!   `paddock.toml` is writable by the agent, so it cannot add folders.
//! - Only regular files are listed and copied. Symlinks, devices, sockets and
//!   FIFOs are skipped, and no path component inside the sandbox home may be a
//!   symlink, so nothing outside the sandbox home is read.
//! - A recovered file never replaces an existing file. It gets a new name.
//! - A recovered file is never written into a hidden path of the home
//!   (`~/.ssh`, `~/.config`, ...), into paddock's own directories, or into
//!   system directories, even when the user asks for it.

use crate::env::HostEnv;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

/// The XDG user directories, the defaults for recovery folders.
pub const DEFAULT_FOLDERS: &[&str] = &[
    "Desktop",
    "Documents",
    "Downloads",
    "Pictures",
    "Videos",
    "Music",
];

/// Files that are not listed by default: desktop launchers (they run
/// commands when opened, like `.lnk` files on Windows), and partial or lock
/// files of downloads and office programs.
pub const DEFAULT_IGNORE: &[&str] = &[
    "*.desktop",
    ".directory",
    "*.part",
    "*.crdownload",
    ".~lock.*",
    "*~",
];

/// Stop a scan after this many files, so a huge tree cannot stall the panel.
const MAX_FILES: usize = 5000;
const MAX_DEPTH: usize = 32;
/// Session records to keep.
const MAX_SESSIONS: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryCfg {
    pub folders: Vec<String>,
    pub ignore: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCfg {
    folders: Option<Vec<String>>,
    ignore: Option<Vec<String>>,
}

impl Default for RecoveryCfg {
    fn default() -> Self {
        Self {
            folders: DEFAULT_FOLDERS.iter().map(|s| s.to_string()).collect(),
            ignore: DEFAULT_IGNORE.iter().map(|s| s.to_string()).collect(),
        }
    }
}

impl RecoveryCfg {
    /// `[recovery]` of `~/.config/paddock/config.toml`.
    pub fn load(env: &HostEnv) -> Result<Self> {
        let file = env.paddock_config().join("config.toml");
        let text = match std::fs::read_to_string(&file) {
            Ok(t) => t,
            Err(_) => return Ok(Self::default()),
        };
        Self::parse(&text).with_context(|| format!("[recovery] in {} is invalid", file.display()))
    }

    pub fn parse(text: &str) -> Result<Self> {
        // Other tables belong to other readers; read only [recovery].
        let v: toml::Value = toml::from_str(text)?;
        let raw: RawCfg = match v.get("recovery") {
            Some(r) => r.clone().try_into()?,
            None => RawCfg::default(),
        };
        let d = Self::default();
        let cfg = Self {
            folders: raw.folders.unwrap_or(d.folders),
            ignore: raw.ignore.unwrap_or(d.ignore),
        };
        for f in &cfg.folders {
            check_folder(f)?;
        }
        Ok(cfg)
    }

    pub fn ignored(&self, rel: &str) -> bool {
        let name = rel.rsplit('/').next().unwrap_or(rel);
        self.ignore.iter().any(|g| {
            if g.contains('/') {
                glob(g, rel)
            } else {
                glob(g, name)
            }
        })
    }

    /// The recovery folder that holds `rel`, and the rest of the path.
    fn folder_of<'a>(&'a self, rel: &'a str) -> Option<(&'a str, &'a str)> {
        self.folders
            .iter()
            .filter_map(|f| {
                let f = f.trim_end_matches('/');
                rel.strip_prefix(f)
                    .and_then(|r| r.strip_prefix('/'))
                    .filter(|r| !r.is_empty())
                    .map(|r| (f, r))
            })
            .max_by_key(|(f, _)| f.len())
    }
}

/// A recovery folder is a plain relative path in the home, without hidden
/// components: `.config` or `.ssh` can never be a recovery folder.
fn check_folder(f: &str) -> Result<()> {
    let p = Path::new(f);
    if f.is_empty() || p.is_absolute() {
        bail!("recovery folder '{f}' must be a path relative to the home, such as Desktop");
    }
    for c in p.components() {
        match c {
            Component::Normal(s) if !s.to_string_lossy().starts_with('.') => {}
            _ => bail!("recovery folder '{f}' must not contain '..' or hidden names"),
        }
    }
    Ok(())
}

/// Glob match with `*` (any text) and `?` (one character).
pub fn glob(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// One sandbox run, kept after the sandbox exits so its files can be listed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub harness: String,
    pub project: PathBuf,
    pub home: PathBuf,
    pub started: u64,
    #[serde(default)]
    pub ended: Option<u64>,
    /// `git rev-parse HEAD` of the project at start.
    #[serde(default)]
    pub git_head: Option<String>,
    /// `git status --porcelain` lines of the project at start.
    #[serde(default)]
    pub git_status: Vec<String>,
}

fn sessions_dir(env: &HostEnv) -> PathBuf {
    env.paddock_state().join("sessions")
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 96
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

impl Session {
    pub fn begin(
        env: &HostEnv,
        id: &str,
        harness: &str,
        project: &Path,
        home: &Path,
    ) -> Result<()> {
        let s = Session {
            id: id.to_string(),
            harness: harness.to_string(),
            project: project.to_path_buf(),
            home: home.to_path_buf(),
            started: crate::state::now(),
            ended: None,
            git_head: git(project, &["rev-parse", "HEAD"]).map(|s| s.trim().to_string()),
            git_status: git_status(project),
        };
        s.save(env)?;
        prune(env);
        Ok(())
    }

    pub fn end(env: &HostEnv, id: &str) {
        if let Some(mut s) = Self::load(env, id) {
            s.ended = Some(crate::state::now());
            let _ = s.save(env);
        }
    }

    fn save(&self, env: &HostEnv) -> Result<()> {
        let dir = sessions_dir(env);
        std::fs::create_dir_all(&dir)?;
        std::fs::write(
            dir.join(format!("{}.json", self.id)),
            serde_json::to_string(self)?,
        )?;
        Ok(())
    }

    pub fn load(env: &HostEnv, id: &str) -> Option<Self> {
        if !valid_id(id) {
            return None;
        }
        let text = std::fs::read_to_string(sessions_dir(env).join(format!("{id}.json"))).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Every recorded session, newest first.
    pub fn all(env: &HostEnv) -> Vec<Self> {
        let mut v: Vec<Self> = std::fs::read_dir(sessions_dir(env))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| std::fs::read_to_string(e.path()).ok())
            .filter_map(|t| serde_json::from_str(&t).ok())
            .collect();
        v.sort_by(|a: &Self, b| b.started.cmp(&a.started).then(b.id.cmp(&a.id)));
        v
    }

    /// The newest session of each sandbox home. Sessions of the same harness,
    /// login and project share a home, so they share their files.
    pub fn latest_per_home(env: &HostEnv) -> Vec<Self> {
        let mut seen = std::collections::BTreeSet::new();
        Self::all(env)
            .into_iter()
            .filter(|s| seen.insert(s.home.clone()))
            .collect()
    }

    pub fn running(&self, env: &HostEnv) -> bool {
        self.ended.is_none()
            && crate::state::Running::list(env)
                .iter()
                .any(|r| r.id == self.id)
    }
}

fn prune(env: &HostEnv) {
    for s in Session::all(env).into_iter().skip(MAX_SESSIONS) {
        let _ = std::fs::remove_file(sessions_dir(env).join(format!("{}.json", s.id)));
    }
}

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    // fsmonitor and other config could run commands; turn them off.
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.untrackedCache=false",
        ])
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn git_status(dir: &Path) -> Vec<String> {
    git(dir, &["status", "--porcelain=v1", "--untracked-files=all"])
        .map(|s| s.lines().map(str::to_string).collect())
        .unwrap_or_default()
}

/// What the agent changed in the project during a session.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ProjectChanges {
    pub git: bool,
    /// Commits made since the session started, `HASH SUBJECT`.
    pub commits: Vec<String>,
    /// `git status` lines that were not there at start.
    pub changed: Vec<String>,
}

pub fn project_changes(s: &Session) -> ProjectChanges {
    let Some(head) = git(&s.project, &["rev-parse", "HEAD"]).map(|h| h.trim().to_string()) else {
        return ProjectChanges::default();
    };
    let commits = match &s.git_head {
        Some(start) if *start != head => git(
            &s.project,
            &[
                "log",
                "--oneline",
                "--no-decorate",
                "-n",
                "50",
                &format!("{start}..{head}"),
            ],
        )
        .map(|t| t.lines().map(str::to_string).collect())
        .unwrap_or_default(),
        _ => vec![],
    };
    let changed = git_status(&s.project)
        .into_iter()
        .filter(|l| !s.git_status.contains(l))
        .collect();
    ProjectChanges {
        git: true,
        commits,
        changed,
    }
}

/// A file in a recovery folder of a sandbox home.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Item {
    /// The path relative to the sandbox home, such as `Desktop/report.pdf`.
    pub path: String,
    pub size: u64,
    pub mtime: i64,
    /// Created or changed since the session started.
    pub new: bool,
    /// Where `paddock recover` puts it by default.
    pub target: PathBuf,
}

/// List the regular files in the recovery folders of `home`.
pub fn scan(env: &HostEnv, cfg: &RecoveryCfg, home: &Path, since: u64) -> Vec<Item> {
    let mut out = Vec::new();
    for f in &cfg.folders {
        if check_folder(f).is_err() || !no_symlinks(home, Path::new(f)) {
            continue;
        }
        walk(env, cfg, &home.join(f), f, since, 0, &mut out);
    }
    out.sort_by(|a, b| b.mtime.cmp(&a.mtime).then(a.path.cmp(&b.path)));
    out.dedup_by(|a, b| a.path == b.path);
    out
}

fn walk(
    env: &HostEnv,
    cfg: &RecoveryCfg,
    dir: &Path,
    rel: &str,
    since: u64,
    depth: usize,
    out: &mut Vec<Item>,
) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(meta) = std::fs::symlink_metadata(dir) else {
        return;
    };
    if !meta.is_dir() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        if out.len() >= MAX_FILES {
            return;
        }
        let name = e.file_name().to_string_lossy().into_owned();
        let r = format!("{rel}/{name}");
        let Ok(m) = std::fs::symlink_metadata(e.path()) else {
            continue;
        };
        if cfg.ignored(&r) {
            continue;
        }
        if m.is_dir() {
            walk(env, cfg, &e.path(), &r, since, depth + 1, out);
        } else if m.is_file() {
            out.push(Item {
                target: default_target(env, cfg, &r).unwrap_or_else(|| env.home.join(&r)),
                new: m.mtime() >= since as i64 || m.ctime() >= since as i64,
                size: m.len(),
                mtime: m.mtime(),
                path: r,
            });
        }
    }
}

/// True when no component of `rel` under `base` is a symlink.
fn no_symlinks(base: &Path, rel: &Path) -> bool {
    let mut p = base.to_path_buf();
    for c in rel.components() {
        p.push(c);
        match std::fs::symlink_metadata(&p) {
            Ok(m) if m.file_type().is_symlink() => return false,
            _ => {}
        }
    }
    true
}

/// The host folder for an XDG user directory name, from
/// `~/.config/user-dirs.dirs`, so `Desktop` maps to a localized desktop.
fn host_user_dir(env: &HostEnv, folder: &str) -> Option<PathBuf> {
    let key = match folder {
        "Desktop" => "XDG_DESKTOP_DIR",
        "Documents" => "XDG_DOCUMENTS_DIR",
        "Downloads" => "XDG_DOWNLOAD_DIR",
        "Pictures" => "XDG_PICTURES_DIR",
        "Videos" => "XDG_VIDEOS_DIR",
        "Music" => "XDG_MUSIC_DIR",
        _ => return None,
    };
    let text = std::fs::read_to_string(env.config_dir.join("user-dirs.dirs")).ok()?;
    let line = text
        .lines()
        .find_map(|l| l.trim().strip_prefix(key)?.strip_prefix('='))?;
    let v = line.trim().trim_matches('"');
    let p = match v.strip_prefix("$HOME") {
        Some(rest) => env.home.join(rest.trim_start_matches('/')),
        None if v.starts_with('/') => PathBuf::from(v),
        None => return None,
    };
    Some(p)
}

/// The same relative path under the real home, with XDG user directories
/// mapped to the host's own names.
pub fn default_target(env: &HostEnv, cfg: &RecoveryCfg, rel: &str) -> Option<PathBuf> {
    let (folder, rest) = cfg.folder_of(rel)?;
    let base = host_user_dir(env, folder).unwrap_or_else(|| env.home.join(folder));
    Some(base.join(rest))
}

/// A sandbox file, held open through its parent directory. Every directory
/// on the way is opened with O_NOFOLLOW, so a sandbox that swaps a folder for
/// a symlink while paddock works cannot point it at a host file.
struct Source {
    dir: std::os::fd::OwnedFd,
    name: std::ffi::CString,
    file: std::fs::File,
    rel: String,
}

impl Source {
    /// Remove the file through the directory that was opened, not by path.
    fn unlink(&self) -> std::io::Result<()> {
        use std::os::fd::AsRawFd;
        // SAFETY: a valid directory fd and a NUL-terminated name.
        if unsafe { libc::unlinkat(self.dir.as_raw_fd(), self.name.as_ptr(), 0) } == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
}

fn open_at(
    dir: libc::c_int,
    name: &std::ffi::CStr,
    flags: libc::c_int,
) -> std::io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::FromRawFd;
    // SAFETY: openat with a NUL-terminated name; the fd is owned on success.
    let fd = unsafe {
        libc::openat(
            dir,
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) })
    }
}

/// Check a relative path from the user and open the sandbox file it names.
fn open_source(cfg: &RecoveryCfg, home: &Path, rel: &str) -> Result<Source> {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    let rel = rel.trim_start_matches("./").to_string();
    let p = Path::new(&rel);
    if p.is_absolute() || !p.components().all(|c| matches!(c, Component::Normal(_))) {
        bail!("'{rel}' must be a path relative to the sandbox home, as `paddock files` shows it");
    }
    if cfg.folder_of(&rel).is_none() {
        bail!(
            "'{rel}' is not in a recovery folder ({})",
            cfg.folders.join(", ")
        );
    }
    let cstr = |s: &std::ffi::OsStr| std::ffi::CString::new(s.as_bytes()).context("bad file name");
    let symlink =
        || anyhow::anyhow!("'{rel}' goes through a symlink; only regular files are recovered");
    let mut dir = open_at(
        libc::AT_FDCWD,
        &cstr(home.as_os_str())?,
        libc::O_DIRECTORY | libc::O_RDONLY,
    )
    .with_context(|| format!("cannot open the sandbox home {}", home.display()))?;
    let parts: Vec<_> = p
        .components()
        .map(|c| c.as_os_str().to_os_string())
        .collect();
    let (last, dirs) = parts.split_last().context("empty path")?;
    for d in dirs {
        dir = open_at(
            dir.as_raw_fd(),
            &cstr(d)?,
            libc::O_DIRECTORY | libc::O_RDONLY,
        )
        .map_err(|e| {
            if e.raw_os_error() == Some(libc::ELOOP) || e.raw_os_error() == Some(libc::ENOTDIR) {
                symlink()
            } else {
                anyhow::Error::from(e).context(format!("cannot open {rel} in the sandbox"))
            }
        })?;
    }
    let name = cstr(last)?;
    let fd = open_at(dir.as_raw_fd(), &name, libc::O_RDONLY | libc::O_NONBLOCK).map_err(|e| {
        if e.raw_os_error() == Some(libc::ELOOP) {
            symlink()
        } else {
            anyhow::Error::from(e).context(format!("cannot open {rel} in the sandbox"))
        }
    })?;
    let file = std::fs::File::from(fd);
    if !file.metadata()?.is_file() {
        bail!("'{rel}' is not a regular file");
    }
    Ok(Source {
        dir,
        name,
        file,
        rel,
    })
}

/// The reason a destination is refused, or None.
pub fn protected(env: &HostEnv, dest: &Path) -> Option<String> {
    let canon_home = std::fs::canonicalize(&env.home).unwrap_or_else(|_| env.home.clone());
    let own = [
        env.paddock_data(),
        env.paddock_config(),
        env.paddock_state(),
        env.paddock_cache(),
    ];
    for d in own {
        let c = std::fs::canonicalize(&d).unwrap_or(d);
        if dest.starts_with(&c) {
            return Some("paddock's own directories".into());
        }
    }
    for h in [&canon_home, &env.home] {
        if let Ok(rel) = dest.strip_prefix(h) {
            if rel
                .components()
                .any(|c| c.as_os_str().to_string_lossy().starts_with('.'))
            {
                return Some(
                    "a hidden path in the home, such as ~/.ssh or ~/.config, which holds credentials and settings"
                        .into(),
                );
            }
            return None;
        }
    }
    const SYSTEM: &[&str] = &[
        "/etc", "/usr", "/bin", "/sbin", "/lib", "/lib64", "/boot", "/proc", "/sys", "/dev",
        "/run", "/var", "/root", "/opt", "/srv",
    ];
    if dest == Path::new("/") || SYSTEM.iter().any(|s| dest.starts_with(s)) {
        return Some("a system directory".into());
    }
    None
}

/// Resolve the existing part of `dest` without trusting symlinks blindly:
/// the deepest existing ancestor is canonicalized, then the rest is added.
fn resolve_dest(dest: &Path) -> Result<PathBuf> {
    let dest = if dest.is_absolute() {
        dest.to_path_buf()
    } else {
        std::env::current_dir()?.join(dest)
    };
    let mut existing = dest.clone();
    let mut rest = Vec::new();
    while std::fs::symlink_metadata(&existing).is_err() {
        match (existing.file_name(), existing.parent()) {
            (Some(n), Some(p)) => {
                rest.push(n.to_os_string());
                existing = p.to_path_buf();
            }
            _ => break,
        }
    }
    let mut out = std::fs::canonicalize(&existing)?;
    for n in rest.into_iter().rev() {
        if n == ".." || n == "." {
            bail!("{} must not contain '..'", dest.display());
        }
        out.push(n);
    }
    Ok(out)
}

/// `name.ext`, then `name (1).ext`, `name (2).ext`, ...
pub fn candidate(path: &Path, n: u32) -> PathBuf {
    if n == 0 {
        return path.to_path_buf();
    }
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = match path.extension() {
        Some(e) => format!("{stem} ({n}).{}", e.to_string_lossy()),
        None => format!("{stem} ({n})"),
    };
    path.with_file_name(name)
}

/// Create the missing directories of `dir` one by one, refusing symlinks.
fn make_dirs(dir: &Path) -> Result<()> {
    let mut p = PathBuf::new();
    for c in dir.components() {
        p.push(c);
        match std::fs::create_dir(&p) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e).with_context(|| format!("cannot create {}", p.display())),
        }
        let m = std::fs::symlink_metadata(&p)?;
        if !m.is_dir() {
            bail!("{} is not a directory", p.display());
        }
    }
    Ok(())
}

pub struct Recovered {
    pub from: String,
    pub to: PathBuf,
    /// True when the file got a new name because the target existed.
    pub renamed: bool,
}

/// Copy one file out of the sandbox home. `to` is a directory; without it,
/// the file goes to the same relative path under the real home. With
/// `keep` false, the sandbox copy is removed after a successful copy.
pub fn recover(
    env: &HostEnv,
    cfg: &RecoveryCfg,
    home: &Path,
    rel: &str,
    to: Option<&Path>,
    keep: bool,
) -> Result<Recovered> {
    let src = open_source(cfg, home, rel)?;
    let rel = src.rel.clone();
    let name = Path::new(&rel).file_name().context("no file name")?;
    let wanted = match to {
        Some(d) => d.join(name),
        None => default_target(env, cfg, &rel).context("no target")?,
    };
    let wanted = resolve_dest(&wanted)?;
    if let Some(why) = protected(env, &wanted) {
        bail!("refusing to write {}: it is in {why}", wanted.display());
    }
    let dir = wanted.parent().context("no target directory")?;
    make_dirs(dir)?;
    // The directory may be a symlink target now; check the real location again.
    let real_dir = std::fs::canonicalize(dir)?;
    if let Some(why) = protected(env, &real_dir) {
        bail!(
            "refusing to write into {}: it is in {why}",
            real_dir.display()
        );
    }
    let wanted = real_dir.join(wanted.file_name().context("no file name")?);
    // Never recover execute or special bits: a file from the sandbox must not
    // become a program that one click on the host runs.
    let mode = src.file.metadata()?.permissions().mode() & 0o666;
    let mut n = 0;
    let (mut out, dest) = loop {
        let c = candidate(&wanted, n);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(libc::O_NOFOLLOW)
            .mode(mode)
            .open(&c)
        {
            Ok(f) => break (f, c),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && n < 999 => n += 1,
            Err(e) => return Err(e).with_context(|| format!("cannot create {}", c.display())),
        }
    };
    let copied = std::io::copy(&mut &src.file, &mut out)
        .and_then(|_| out.flush())
        .and_then(|_| out.sync_all());
    if let Err(e) = copied {
        let _ = std::fs::remove_file(&dest);
        return Err(e).with_context(|| format!("cannot copy {rel} to {}", dest.display()));
    }
    if !keep {
        src.unlink().with_context(|| {
            format!(
                "copied to {}, but cannot remove the sandbox copy",
                dest.display()
            )
        })?;
    }
    Ok(Recovered {
        from: rel,
        renamed: n > 0,
        to: dest,
    })
}

/// Delete one file from a recovery folder of the sandbox home.
pub fn discard(cfg: &RecoveryCfg, home: &Path, rel: &str) -> Result<String> {
    let src = open_source(cfg, home, rel)?;
    src.unlink()
        .with_context(|| format!("cannot delete {}", src.rel))?;
    Ok(src.rel)
}

#[cfg(test)]
mod tests;
