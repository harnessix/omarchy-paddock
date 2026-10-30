//! File operations of the host inside a folder that a sandbox can write: a
//! sandbox home or a project.
//!
//! A sandbox can replace any entry there with a symlink, also between a check
//! and the next call. So every operation walks from the root one component
//! at a time with `openat(O_NOFOLLOW)`, and acts through the directory it
//! opened: `openat` to read, a new file plus `renameat` to write, `unlinkat`
//! to remove. A symlink anywhere on the way is an error, never followed.
//! Use plain `std::fs` only for paths that only the host can write.

use anyhow::{bail, Context, Result};
use std::collections::VecDeque;
use std::ffi::{CString, OsStr, OsString};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

/// Credentials, JSON settings and similar files are small.
pub const MAX_READ: u64 = 4 << 20;

/// The most that `Root::copy_in` copies from the host.
const MAX_COPY: u64 = 256 << 20;

/// Folders deeper than this are refused when copying or removing.
const MAX_DEPTH: usize = 512;

fn cstr(s: &OsStr) -> Result<CString> {
    CString::new(s.as_bytes()).context("a file name contains a NUL byte")
}

fn openat(
    dir: libc::c_int,
    name: &CString,
    flags: libc::c_int,
    mode: u32,
) -> std::io::Result<OwnedFd> {
    // SAFETY: openat with a NUL-terminated name; the fd is owned on success.
    let fd = unsafe {
        libc::openat(
            dir,
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            mode as libc::c_uint,
        )
    };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

fn last_err() -> std::io::Error {
    std::io::Error::last_os_error()
}

/// What a file system entry is. A symlink is never followed to find out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    Symlink,
    Socket,
    /// A FIFO or a device: opening or reading it can block or have effects.
    Other,
}

fn fstat(fd: &OwnedFd) -> std::io::Result<libc::stat> {
    // SAFETY: fstat fills a zeroed stat for an open fd.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } != 0 {
        return Err(last_err());
    }
    Ok(st)
}

fn kind_of(st: &libc::stat) -> Kind {
    match st.st_mode & libc::S_IFMT {
        libc::S_IFREG => Kind::File,
        libc::S_IFDIR => Kind::Dir,
        libc::S_IFLNK => Kind::Symlink,
        libc::S_IFSOCK => Kind::Socket,
        _ => Kind::Other,
    }
}

/// The path that names an open fd itself. Opening, connecting or changing
/// it acts on the object the fd holds, never on a path that could change.
fn fd_path(fd: &OwnedFd) -> String {
    format!("/proc/self/fd/{}", fd.as_raw_fd())
}

fn open_path(path: &CString, flags: libc::c_int) -> std::io::Result<OwnedFd> {
    // SAFETY: open with a NUL-terminated path; the fd is owned on success.
    let r = unsafe { libc::open(path.as_ptr(), flags | libc::O_CLOEXEC) };
    if r < 0 {
        Err(last_err())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(r) })
    }
}

/// Open the regular file behind an `O_PATH` fd for reading. The fd is checked
/// first, so a FIFO or a device is never opened.
fn reopen_read(fd: &OwnedFd, what: &Path) -> Result<std::fs::File> {
    match kind_of(&fstat(fd)?) {
        Kind::File => {}
        Kind::Symlink => bail!(
            "{} is a symlink; paddock does not follow it",
            what.display()
        ),
        _ => bail!("{} is not a regular file", what.display()),
    }
    let p = CString::new(fd_path(fd)).expect("no NUL in a /proc path");
    let f = open_path(&p, libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOCTTY)
        .with_context(|| format!("cannot read {}", what.display()))?;
    Ok(std::fs::File::from(f))
}

/// Read at most `max` bytes; more is an error.
fn read_limited(f: std::fs::File, max: u64, what: &Path) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    f.take(max.saturating_add(1)).read_to_end(&mut buf)?;
    if buf.len() as u64 > max {
        bail!("{} is larger than {} bytes", what.display(), max);
    }
    Ok(buf)
}

/// The names in an open folder.
fn list(dir: &OwnedFd) -> Result<Vec<std::ffi::OsString>> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(fd_path(dir))? {
        out.push(e?.file_name());
    }
    Ok(out)
}

/// A readable folder fd from an `O_PATH` one.
fn reopen_dir(fd: &OwnedFd) -> std::io::Result<OwnedFd> {
    openat(
        fd.as_raw_fd(),
        &CString::new(".").expect("no NUL"),
        libc::O_RDONLY | libc::O_DIRECTORY,
        0,
    )
}

/// Open a regular file for reading by path. The folders above it are
/// followed; the file itself only with `follow`. None when it does not exist.
/// A FIFO, a device or a folder is an error and is never opened, so a read
/// cannot hang.
pub fn open_file(path: &Path, follow: bool) -> Result<Option<std::fs::File>> {
    let mut flags = libc::O_PATH;
    if !follow {
        flags |= libc::O_NOFOLLOW;
    }
    let fd = match open_path(&cstr(path.as_os_str())?, flags) {
        Ok(f) => f,
        Err(e) if e.raw_os_error() == Some(libc::ENOENT) => return Ok(None),
        Err(e) => {
            return Err(anyhow::Error::from(e).context(format!("cannot open {}", path.display())))
        }
    };
    reopen_read(&fd, path).map(Some)
}

/// Read a regular file by path, at most `max` bytes. See `open_file`.
pub fn read_path(path: &Path, follow: bool, max: u64) -> Result<Option<Vec<u8>>> {
    match open_file(path, follow)? {
        Some(f) => read_limited(f, max, path).map(Some),
        None => Ok(None),
    }
}

/// Remove `path` and everything below it, without following any symlink in
/// it. The folders above `path` belong to the host and are followed. Missing
/// is fine.
pub fn remove_tree_path(path: &Path) -> Result<()> {
    let name = path.file_name().context("no file name")?;
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let fd = match open_path(
        &cstr(parent.as_os_str())?,
        libc::O_RDONLY | libc::O_DIRECTORY,
    ) {
        Ok(f) => f,
        Err(e) if e.raw_os_error() == Some(libc::ENOENT) => return Ok(()),
        Err(e) => {
            return Err(anyhow::Error::from(e).context(format!("cannot open {}", parent.display())))
        }
    };
    Dir {
        fd,
        path: parent.to_path_buf(),
    }
    .remove_tree(name)
}

/// Resolve `path`, which the user approved in a configuration, to the place
/// it names now. A symlink on the way that sits inside one of `writable`
/// (folders a sandbox can write, as real paths) is an error: a sandbox could
/// have planted it to point the path somewhere else. Symlinks in host
/// folders, such as a linked home, are followed.
pub fn resolve_approved(path: &Path, writable: &[PathBuf]) -> Result<PathBuf> {
    if !path.is_absolute() {
        bail!("{} is not an absolute path", path.display());
    }
    let mut real = PathBuf::from("/");
    let mut todo: VecDeque<OsString> = path
        .components()
        .filter(|c| *c != Component::RootDir)
        .map(|c| c.as_os_str().to_os_string())
        .collect();
    let mut links = 0;
    while let Some(c) = todo.pop_front() {
        if c == "." {
            continue;
        }
        if c == ".." {
            real.pop();
            continue;
        }
        let next = real.join(&c);
        let m = std::fs::symlink_metadata(&next)
            .with_context(|| format!("{} does not exist", path.display()))?;
        if !m.file_type().is_symlink() {
            real = next;
            continue;
        }
        if let Some(w) = writable.iter().find(|w| real.starts_with(w)) {
            bail!(
                "{} goes through the symlink {}, inside {}, which a sandbox can write. \
                 Name the real folder instead",
                path.display(),
                next.display(),
                w.display()
            );
        }
        links += 1;
        if links > 40 {
            bail!("{} has too many symlinks", path.display());
        }
        let target = std::fs::read_link(&next)?;
        if target.is_absolute() {
            real = PathBuf::from("/");
        }
        for c in target.components().rev() {
            if c != Component::RootDir {
                todo.push_front(c.as_os_str().to_os_string());
            }
        }
    }
    Ok(real)
}

/// A host file or folder that the user asked to copy, opened once. Every
/// later step works on the open fd, so a path that changes after the check,
/// for example while the user decides, cannot redirect the copy.
pub struct Source {
    fd: OwnedFd,
    kind: Kind,
}

impl Source {
    /// Open `path`. Symlinks in it are followed, as the user named it;
    /// `real_path` tells where it led. Only a regular file or a folder opens.
    pub fn open(path: &Path) -> Result<Self> {
        let fd = open_path(&cstr(path.as_os_str())?, libc::O_PATH)
            .with_context(|| format!("{} does not exist on the host", path.display()))?;
        let kind = kind_of(&fstat(&fd)?);
        if !matches!(kind, Kind::File | Kind::Dir) {
            bail!(
                "{} is not a regular file or a folder; paddock copies only those",
                path.display()
            );
        }
        Ok(Self { fd, kind })
    }

    /// Where the open file or folder is now.
    pub fn real_path(&self) -> Result<PathBuf> {
        Ok(std::fs::read_link(fd_path(&self.fd))?)
    }

    pub fn is_dir(&self) -> bool {
        self.kind == Kind::Dir
    }

    /// The bytes a copy takes; it stops counting above `limit`. Symlinks,
    /// FIFOs and devices inside a folder do not count: a copy skips them.
    pub fn size(&self, limit: u64) -> Result<u64> {
        let mut total = 0;
        size_of(&self.fd, self.kind, limit, &mut total, 0)?;
        Ok(total)
    }
}

fn size_of(fd: &OwnedFd, kind: Kind, limit: u64, total: &mut u64, depth: usize) -> Result<()> {
    match kind {
        Kind::File => *total += fstat(fd)?.st_size as u64,
        Kind::Dir => {
            if depth > MAX_DEPTH {
                bail!("the folder is too deep");
            }
            let d = reopen_dir(fd)?;
            for name in list(&d)? {
                if *total > limit {
                    break;
                }
                let e = openat(d.as_raw_fd(), &cstr(&name)?, libc::O_PATH, 0)?;
                let k = kind_of(&fstat(&e)?);
                size_of(&e, k, limit, total, depth + 1)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn symlink_error(e: std::io::Error, what: &Path) -> anyhow::Error {
    match e.raw_os_error() {
        Some(libc::ELOOP) | Some(libc::ENOTDIR) => {
            anyhow::anyhow!(
                "{} goes through a symlink or a file; paddock does not follow it",
                what.display()
            )
        }
        _ => anyhow::Error::from(e).context(format!("cannot open {}", what.display())),
    }
}

/// The plain names of a relative path. Absolute paths, `..` and `.` fail.
fn names(rel: &Path) -> Result<Vec<CString>> {
    let mut out = Vec::new();
    for c in rel.components() {
        match c {
            Component::Normal(n) => out.push(cstr(n)?),
            _ => bail!("{} must be a plain relative path", rel.display()),
        }
    }
    Ok(out)
}

/// A directory that a sandbox can write, opened from the host.
pub struct Root {
    fd: OwnedFd,
    path: std::path::PathBuf,
}

/// A directory below a `Root`, and its path for messages.
pub struct Dir {
    fd: OwnedFd,
    path: std::path::PathBuf,
}

impl Root {
    /// Open `path`, which the host chose. Its last component must not be a
    /// symlink; the folders above it belong to the host.
    pub fn open(path: &Path) -> Result<Self> {
        let c = cstr(path.as_os_str())?;
        let fd = openat(libc::AT_FDCWD, &c, libc::O_RDONLY | libc::O_DIRECTORY, 0)
            .map_err(|e| symlink_error(e, path))?;
        Ok(Self {
            fd,
            path: path.to_path_buf(),
        })
    }

    /// The directory `rel` below the root. With `create`, missing folders are
    /// made with mode 0700.
    pub fn dir(&self, rel: &Path, create: bool) -> Result<Dir> {
        let mut fd = self.fd.try_clone()?;
        let mut path = self.path.clone();
        for n in names(rel)? {
            path.push(OsStr::from_bytes(n.as_bytes()));
            let flags = libc::O_RDONLY | libc::O_DIRECTORY;
            fd = match openat(fd.as_raw_fd(), &n, flags, 0) {
                Ok(f) => f,
                Err(e) if create && e.raw_os_error() == Some(libc::ENOENT) => {
                    // SAFETY: mkdirat with a NUL-terminated name.
                    if unsafe { libc::mkdirat(fd.as_raw_fd(), n.as_ptr(), 0o700) } != 0 {
                        let e = last_err();
                        if e.raw_os_error() != Some(libc::EEXIST) {
                            return Err(anyhow::Error::from(e)
                                .context(format!("cannot create {}", path.display())));
                        }
                    }
                    openat(fd.as_raw_fd(), &n, flags, 0).map_err(|e| symlink_error(e, &path))?
                }
                Err(e) => return Err(symlink_error(e, &path)),
            };
        }
        Ok(Dir { fd, path })
    }

    fn split(rel: &Path) -> Result<(&Path, &OsStr)> {
        let name = rel.file_name().context("no file name")?;
        Ok((rel.parent().unwrap_or(Path::new("")), name))
    }

    /// Read a regular file. None when it does not exist. A symlink, a folder
    /// or a special file is an error.
    pub fn read(&self, rel: &Path, max: u64) -> Result<Option<Vec<u8>>> {
        let (parent, name) = Self::split(rel)?;
        let dir = match self.dir(parent, false) {
            Ok(d) => d,
            Err(_) if !self.path.join(parent).exists() => return Ok(None),
            Err(e) => return Err(e),
        };
        dir.read(name, max)
    }

    /// Replace `rel` with `bytes` atomically, creating missing folders.
    pub fn write(&self, rel: &Path, bytes: &[u8], mode: u32) -> Result<()> {
        let (parent, name) = Self::split(rel)?;
        self.dir(parent, true)?.write(name, bytes, mode)
    }

    /// Remove the file or symlink `rel`. Missing is fine; a folder is an error.
    pub fn remove(&self, rel: &Path) -> Result<()> {
        let (parent, name) = Self::split(rel)?;
        match self.dir(parent, false) {
            Ok(d) => d.remove(name),
            Err(_) if !self.path.join(parent).exists() => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Copy a host tree (`from`, which only the host can write) to `rel`.
    /// Symlinks, FIFOs and devices in the source are skipped; nothing in the
    /// target is followed.
    pub fn copy_in(&self, from: &Path, rel: &Path) -> Result<()> {
        let fd = openat(libc::AT_FDCWD, &cstr(from.as_os_str())?, libc::O_PATH, 0)
            .with_context(|| format!("cannot open {}", from.display()))?;
        let mut budget = MAX_COPY;
        let st = fstat(&fd)?;
        match kind_of(&st) {
            Kind::Dir => self
                .dir(rel, true)?
                .copy_dir_fd(&reopen_dir(&fd)?, &mut budget, 0),
            Kind::File => {
                let (parent, name) = Self::split(rel)?;
                let bytes = read_limited(reopen_read(&fd, from)?, budget, from)?;
                self.dir(parent, true)?
                    .write(name, &bytes, (st.st_mode & 0o755) | 0o600)
            }
            _ => Ok(()),
        }
    }

    /// Remove `rel` and everything below it; see `Dir::remove_tree`.
    pub fn remove_tree(&self, rel: &Path) -> Result<()> {
        let (parent, name) = Self::split(rel)?;
        match self.dir(parent, false) {
            Ok(d) => d.remove_tree(name),
            Err(_) if !self.path.join(parent).exists() => Ok(()),
            Err(e) => Err(e),
        }
    }
}

impl Dir {
    /// The entries of this folder with their kind, without following any.
    pub fn entries(&self) -> Result<Vec<(OsString, Kind)>> {
        let mut out = Vec::new();
        for n in list(&self.fd)? {
            if let Some(k) = self.kind(&n)? {
                out.push((n, k));
            }
        }
        Ok(out)
    }

    /// The names of the regular files directly in this folder. Symlinks and
    /// everything else are left out.
    pub fn file_names(&self) -> Result<Vec<OsString>> {
        let mut out = Vec::new();
        for n in list(&self.fd)? {
            if self.kind(&n)? == Some(Kind::File) {
                out.push(n);
            }
        }
        Ok(out)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Where this open folder really is now, from the kernel, not from the
    /// path that was walked.
    pub fn real_path(&self) -> Result<PathBuf> {
        Ok(std::fs::read_link(format!(
            "/proc/self/fd/{}",
            self.fd.as_raw_fd()
        ))?)
    }

    /// Read a regular file. None when it does not exist. A symlink, a folder
    /// or a special file is an error; a FIFO or a device is never opened.
    pub fn read(&self, name: &OsStr, max: u64) -> Result<Option<Vec<u8>>> {
        let what = self.path.join(name);
        let fd = match openat(self.fd.as_raw_fd(), &cstr(name)?, libc::O_PATH, 0) {
            Ok(f) => f,
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => return Ok(None),
            Err(e) => return Err(symlink_error(e, &what)),
        };
        let f = reopen_read(&fd, &what)?;
        if f.metadata()?.len() > max {
            bail!("{} is larger than {} bytes", what.display(), max);
        }
        read_limited(f, max, &what).map(Some)
    }

    /// What the entry `name` is, without following it. None when missing.
    pub fn kind(&self, name: &OsStr) -> Result<Option<Kind>> {
        let n = cstr(name)?;
        // SAFETY: fstatat fills a zeroed stat; AT_SYMLINK_NOFOLLOW keeps links.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        let r = unsafe {
            libc::fstatat(
                self.fd.as_raw_fd(),
                n.as_ptr(),
                &mut st,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if r != 0 {
            let e = last_err();
            if e.raw_os_error() == Some(libc::ENOENT) {
                return Ok(None);
            }
            return Err(e.into());
        }
        Ok(Some(kind_of(&st)))
    }

    /// Connect to the Unix socket `name`. A symlink there is an error, so a
    /// sandbox cannot point the host at another socket of the host.
    pub fn connect(&self, name: &OsStr) -> Result<std::os::unix::net::UnixStream> {
        let what = self.path.join(name);
        let fd = openat(self.fd.as_raw_fd(), &cstr(name)?, libc::O_PATH, 0)
            .map_err(|e| symlink_error(e, &what))?;
        if kind_of(&fstat(&fd)?) != Kind::Socket {
            bail!("{} is not a socket", what.display());
        }
        std::os::unix::net::UnixStream::connect(fd_path(&fd))
            .with_context(|| format!("cannot connect to {}", what.display()))
    }

    /// Remove `name` and everything below it. Symlinks are removed, never
    /// followed. A folder without write permission gets it first, through
    /// the open folder. Missing is fine.
    pub fn remove_tree(&self, name: &OsStr) -> Result<()> {
        self.remove_tree_at(name, 0)
    }

    fn remove_tree_at(&self, name: &OsStr, depth: usize) -> Result<()> {
        let n = cstr(name)?;
        let path = self.path.join(name);
        let e = match openat(self.fd.as_raw_fd(), &n, libc::O_PATH, 0) {
            Ok(f) => f,
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => return Ok(()),
            Err(e) => return Err(symlink_error(e, &path)),
        };
        let st = fstat(&e)?;
        if kind_of(&st) != Kind::Dir {
            return self.remove(name);
        }
        if depth > MAX_DEPTH {
            bail!("{} is too deep to remove", path.display());
        }
        if st.st_mode & 0o700 != 0o700 {
            // chmod of /proc/self/fd/N changes the open folder, not a path.
            let p = CString::new(fd_path(&e)).expect("no NUL in a /proc path");
            // SAFETY: chmod with a NUL-terminated path.
            if unsafe { libc::chmod(p.as_ptr(), (st.st_mode & 0o7777) | 0o700) } != 0 {
                return Err(anyhow::Error::from(last_err())
                    .context(format!("cannot change {}", path.display())));
            }
        }
        let sub = Dir {
            fd: reopen_dir(&e).map_err(|e| symlink_error(e, &path))?,
            path: path.clone(),
        };
        for child in list(&sub.fd)? {
            sub.remove_tree_at(&child, depth + 1)?;
        }
        // SAFETY: unlinkat with AT_REMOVEDIR removes only an empty folder.
        if unsafe { libc::unlinkat(self.fd.as_raw_fd(), n.as_ptr(), libc::AT_REMOVEDIR) } != 0 {
            let err = last_err();
            if err.raw_os_error() != Some(libc::ENOENT) {
                return Err(
                    anyhow::Error::from(err).context(format!("cannot remove {}", path.display()))
                );
            }
        }
        Ok(())
    }

    /// Copy an opened host source into a new entry with a free name, at most
    /// `max` bytes. Returns the name. A partial copy is removed.
    pub fn copy_source(&self, src: &Source, want: &str, max: u64) -> Result<String> {
        let mut budget = max;
        if src.kind == Kind::Dir {
            let (d, name) = self.new_subdir(want)?;
            let r = reopen_dir(&src.fd)
                .map_err(anyhow::Error::from)
                .and_then(|s| d.copy_dir_fd(&s, &mut budget, 0));
            if let Err(e) = r {
                let _ = self.remove_tree(OsStr::new(&name));
                return Err(e);
            }
            Ok(name)
        } else {
            let mode = fstat(&src.fd)?.st_mode & 0o755;
            let (mut f, name) = self.new_file(want, mode | 0o600)?;
            let r = reopen_read(&src.fd, Path::new(want))
                .and_then(|s| read_limited(s, budget, Path::new(want)))
                .and_then(|b| Ok(f.write_all(&b)?));
            if let Err(e) = r {
                drop(f);
                let _ = self.remove(OsStr::new(&name));
                return Err(e);
            }
            Ok(name)
        }
    }

    /// Copy the entries of an open host folder in here. Only regular files
    /// and folders are copied; symlinks, FIFOs and devices are skipped.
    fn copy_dir_fd(&self, src: &OwnedFd, budget: &mut u64, depth: usize) -> Result<()> {
        if depth > MAX_DEPTH {
            bail!("{} is too deep to copy", self.path.display());
        }
        for name in list(src)? {
            let e = openat(src.as_raw_fd(), &cstr(&name)?, libc::O_PATH, 0)?;
            let st = fstat(&e)?;
            match kind_of(&st) {
                Kind::Dir => {
                    let sub = self.subdir(&name, true)?;
                    sub.copy_dir_fd(&reopen_dir(&e)?, budget, depth + 1)?;
                }
                Kind::File => {
                    let what = self.path.join(&name);
                    let bytes = read_limited(reopen_read(&e, &what)?, *budget, &what)
                        .context("the copy is larger than the limit")?;
                    *budget -= bytes.len() as u64;
                    self.write(&name, &bytes, (st.st_mode & 0o755) | 0o600)?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// A new file that did not exist; it never replaces anything.
    pub fn create_new(&self, name: &OsStr, mode: u32) -> Result<std::fs::File> {
        let n = cstr(name)?;
        let fd = openat(
            self.fd.as_raw_fd(),
            &n,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
            mode,
        )
        .map_err(|e| {
            anyhow::Error::from(e)
                .context(format!("cannot create {}", self.path.join(name).display()))
        })?;
        Ok(std::fs::File::from(fd))
    }

    /// Write a new file next to `name`, then rename it over `name`. renameat
    /// replaces a symlink at `name` itself and never writes through it.
    pub fn write(&self, name: &OsStr, bytes: &[u8], mode: u32) -> Result<()> {
        let mut tmp = name.to_os_string();
        tmp.push(format!(".paddock-tmp-{}", std::process::id()));
        let _ = self.remove(&tmp);
        let mut f = self.create_new(&tmp, mode)?;
        let written = f.write_all(bytes).and_then(|_| f.sync_all());
        drop(f);
        let (t, n) = (cstr(&tmp)?, cstr(name)?);
        if let Err(e) = written {
            let _ = self.remove(&tmp);
            return Err(e.into());
        }
        // SAFETY: renameat between names in the same open directory.
        if unsafe {
            libc::renameat(
                self.fd.as_raw_fd(),
                t.as_ptr(),
                self.fd.as_raw_fd(),
                n.as_ptr(),
            )
        } != 0
        {
            let e = last_err();
            let _ = self.remove(&tmp);
            return Err(anyhow::Error::from(e)
                .context(format!("cannot write {}", self.path.join(name).display())));
        }
        Ok(())
    }

    /// Remove a file or symlink. Missing is fine; a folder is an error.
    pub fn remove(&self, name: &OsStr) -> Result<()> {
        let n = cstr(name)?;
        // SAFETY: unlinkat without AT_REMOVEDIR removes only non-directories.
        if unsafe { libc::unlinkat(self.fd.as_raw_fd(), n.as_ptr(), 0) } != 0 {
            let e = last_err();
            if e.raw_os_error() != Some(libc::ENOENT) {
                return Err(anyhow::Error::from(e)
                    .context(format!("cannot remove {}", self.path.join(name).display())));
            }
        }
        Ok(())
    }

    /// A subfolder, made if missing. Never follows a symlink.
    pub fn subdir(&self, name: &OsStr, create: bool) -> Result<Dir> {
        let n = cstr(name)?;
        let path = self.path.join(name);
        let flags = libc::O_RDONLY | libc::O_DIRECTORY;
        let fd = match openat(self.fd.as_raw_fd(), &n, flags, 0) {
            Ok(f) => f,
            Err(e) if create && e.raw_os_error() == Some(libc::ENOENT) => {
                // SAFETY: mkdirat with a NUL-terminated name.
                if unsafe { libc::mkdirat(self.fd.as_raw_fd(), n.as_ptr(), 0o700) } != 0 {
                    let e = last_err();
                    if e.raw_os_error() != Some(libc::EEXIST) {
                        return Err(anyhow::Error::from(e)
                            .context(format!("cannot create {}", path.display())));
                    }
                }
                openat(self.fd.as_raw_fd(), &n, flags, 0).map_err(|e| symlink_error(e, &path))?
            }
            Err(e) => return Err(symlink_error(e, &path)),
        };
        Ok(Dir { fd, path })
    }

    /// Make a new subfolder whose name is free: `name`, `name-1`, `name-2`…
    pub fn new_subdir(&self, name: &str) -> Result<(Dir, String)> {
        for i in 0..1000 {
            let cand = if i == 0 {
                name.to_string()
            } else {
                format!("{name}-{i}")
            };
            let n = cstr(OsStr::new(&cand))?;
            // SAFETY: mkdirat with a NUL-terminated name.
            if unsafe { libc::mkdirat(self.fd.as_raw_fd(), n.as_ptr(), 0o700) } == 0 {
                return Ok((self.subdir(OsStr::new(&cand), false)?, cand));
            }
            let e = last_err();
            if e.raw_os_error() != Some(libc::EEXIST) {
                return Err(e.into());
            }
        }
        bail!("no free name for {name} in {}", self.path.display())
    }

    /// Make a new file whose name is free: `stem.ext`, `stem-1.ext`…
    pub fn new_file(&self, name: &str, mode: u32) -> Result<(std::fs::File, String)> {
        let (stem, ext) = match name.rsplit_once('.') {
            Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
            _ => (name.to_string(), String::new()),
        };
        for i in 0..1000 {
            let cand = if i == 0 {
                name.to_string()
            } else {
                format!("{stem}-{i}{ext}")
            };
            match self.create_new(OsStr::new(&cand), mode) {
                Ok(f) => return Ok((f, cand)),
                Err(e) => {
                    let exists = e
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|io| io.raw_os_error() == Some(libc::EEXIST));
                    if !exists {
                        return Err(e);
                    }
                }
            }
        }
        bail!("no free name for {name} in {}", self.path.display())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn writes_and_reads_never_follow_symlinks() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("home");
        let outside = t.path().join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret"), "host secret").unwrap();
        let r = Root::open(&root).unwrap();

        // A symlinked file is neither read nor written through.
        symlink(outside.join("secret"), root.join("token")).unwrap();
        assert!(r.read(Path::new("token"), MAX_READ).is_err());
        r.write(Path::new("token"), b"new", 0o600).unwrap();
        assert_eq!(
            std::fs::read_to_string(outside.join("secret")).unwrap(),
            "host secret"
        );
        assert!(!std::fs::symlink_metadata(root.join("token"))
            .unwrap()
            .file_type()
            .is_symlink());

        // A symlinked folder on the way is refused.
        symlink(&outside, root.join(".config")).unwrap();
        assert!(r
            .write(Path::new(".config/x/token"), b"new", 0o600)
            .is_err());
        assert!(r.read(Path::new(".config/secret"), MAX_READ).is_err());
        assert!(r.remove(Path::new(".config/secret")).is_err());
        assert!(outside.join("secret").exists());
        assert!(!outside.join("x").exists());

        // Plain files work, and missing ones read as None.
        r.write(Path::new("a/b/c.json"), b"{}", 0o600).unwrap();
        assert_eq!(
            r.read(Path::new("a/b/c.json"), MAX_READ).unwrap().unwrap(),
            b"{}"
        );
        assert!(r.read(Path::new("a/missing"), MAX_READ).unwrap().is_none());
        assert!(r.read(Path::new("nothere/x"), MAX_READ).unwrap().is_none());
        assert!(r.write(Path::new("../escape"), b"x", 0o600).is_err());
    }

    #[test]
    fn copy_in_and_new_names() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("p");
        let src = t.path().join("src");
        std::fs::create_dir_all(src.join("d")).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(src.join("d/f"), "1").unwrap();
        symlink("/etc/passwd", src.join("link")).unwrap();
        let r = Root::open(&root).unwrap();
        r.copy_in(&src, Path::new("x/y")).unwrap();
        assert_eq!(std::fs::read_to_string(root.join("x/y/d/f")).unwrap(), "1");
        assert!(!root.join("x/y/link").exists());
        let d = r.dir(Path::new("x"), false).unwrap();
        let (_, a) = d.new_file("shot.png", 0o600).unwrap();
        let (_, b) = d.new_file("shot.png", 0o600).unwrap();
        assert_eq!((a.as_str(), b.as_str()), ("shot.png", "shot-1.png"));
        let (_, n) = d.new_subdir("y").unwrap();
        assert_eq!(n, "y-1");
    }

    fn mkfifo(p: &Path) {
        let c = CString::new(p.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    }

    #[test]
    fn reads_never_open_fifos_or_devices() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path();
        mkfifo(&root.join("fifo"));
        symlink("/dev/zero", root.join("zero")).unwrap();
        std::fs::write(root.join("big"), vec![b'x'; 100]).unwrap();
        let r = Root::open(root).unwrap();
        // Each of these would block or never end if it were opened.
        assert!(r.read(Path::new("fifo"), MAX_READ).is_err());
        assert!(r.read(Path::new("zero"), MAX_READ).is_err());
        assert!(read_path(&root.join("fifo"), true, MAX_READ).is_err());
        assert!(read_path(&root.join("zero"), true, MAX_READ).is_err());
        assert!(read_path(&root.join("zero"), false, MAX_READ).is_err());
        assert!(read_path(&root.join("big"), false, 10).is_err());
        assert_eq!(
            read_path(&root.join("big"), false, 100)
                .unwrap()
                .unwrap()
                .len(),
            100
        );
        assert!(read_path(&root.join("missing"), false, 10)
            .unwrap()
            .is_none());
        assert!(Source::open(&root.join("fifo")).is_err());
        assert!(Source::open(&root.join("zero")).is_err());
        let d = r.dir(Path::new(""), false).unwrap();
        assert_eq!(d.kind(OsStr::new("fifo")).unwrap(), Some(Kind::Other));
        assert_eq!(d.kind(OsStr::new("zero")).unwrap(), Some(Kind::Symlink));
        assert_eq!(d.kind(OsStr::new("none")).unwrap(), None);
    }

    #[test]
    fn remove_tree_never_follows_links_and_handles_read_only_folders() {
        let t = tempfile::tempdir().unwrap();
        let (root, outside) = (t.path().join("home"), t.path().join("outside"));
        std::fs::create_dir_all(root.join("a/ro/deep")).unwrap();
        std::fs::create_dir_all(outside.join("keyrings")).unwrap();
        std::fs::write(outside.join("keyrings/login"), "secret").unwrap();
        std::fs::write(root.join("a/ro/deep/f"), "x").unwrap();
        symlink(&outside, root.join("a/ro/link")).unwrap();
        mkfifo(&root.join("a/fifo"));
        let ro = |p: &Path, m| std::fs::set_permissions(p, PermissionsExt::from_mode(m)).unwrap();
        use std::os::unix::fs::PermissionsExt;
        ro(&root.join("a/ro/deep"), 0o500);
        ro(&root.join("a/ro"), 0o000);
        // A planted folder link on the way: nothing outside is removed.
        symlink(&outside, root.join(".local")).unwrap();
        let r = Root::open(&root).unwrap();
        assert!(r.remove_tree(Path::new(".local/keyrings")).is_err());
        assert!(outside.join("keyrings/login").exists());
        // The link itself goes, its target stays.
        r.remove_tree(Path::new(".local")).unwrap();
        assert!(outside.join("keyrings/login").exists());
        r.remove_tree(Path::new("a")).unwrap();
        assert!(!root.join("a").exists());
        assert!(outside.join("keyrings/login").exists());
        r.remove_tree(Path::new("missing/x")).unwrap();
        // By path, the last component is not followed either.
        symlink(&outside, t.path().join("l")).unwrap();
        remove_tree_path(&t.path().join("l")).unwrap();
        assert!(outside.join("keyrings/login").exists());
    }

    #[test]
    fn approved_paths_refuse_links_in_writable_folders() {
        let t = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(t.path()).unwrap();
        let (project, host) = (base.join("project"), base.join("host"));
        std::fs::create_dir_all(project.join("vendor/lib")).unwrap();
        std::fs::create_dir_all(host.join("docs")).unwrap();
        let w = vec![project.clone()];
        // Plain folders and `..` resolve as they are.
        assert_eq!(
            resolve_approved(&project.join("vendor/lib"), &w).unwrap(),
            project.join("vendor/lib")
        );
        assert_eq!(
            resolve_approved(&project.join("vendor/../vendor"), &w).unwrap(),
            project.join("vendor")
        );
        // A host link (outside the writable folders) is followed.
        symlink(&host, base.join("host-link")).unwrap();
        assert_eq!(
            resolve_approved(&base.join("host-link/docs"), &w).unwrap(),
            host.join("docs")
        );
        // A link that the sandbox could plant in the project is refused,
        // also when it is reached through a host link.
        symlink(host.join("docs"), project.join("shared")).unwrap();
        assert!(resolve_approved(&project.join("shared"), &w).is_err());
        symlink(&project, base.join("p-link")).unwrap();
        assert!(resolve_approved(&base.join("p-link/shared"), &w).is_err());
        symlink(&host, project.join("vendor/up")).unwrap();
        assert!(resolve_approved(&project.join("vendor/up/docs"), &w).is_err());
        assert!(resolve_approved(&project.join("missing"), &w).is_err());
    }

    #[test]
    fn sources_copy_what_was_opened() {
        let t = tempfile::tempdir().unwrap();
        let (src, dst) = (t.path().join("src"), t.path().join("dst"));
        std::fs::create_dir_all(src.join("d")).unwrap();
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::write(src.join("d/f"), "abc").unwrap();
        std::fs::write(src.join("big"), vec![0u8; 50]).unwrap();
        mkfifo(&src.join("d/fifo"));
        symlink("/etc/passwd", src.join("d/link")).unwrap();
        let s = Source::open(&src).unwrap();
        assert!(s.is_dir());
        assert_eq!(s.size(1000).unwrap(), 53);
        // Renaming the folder after it was opened changes nothing.
        std::fs::rename(&src, t.path().join("moved")).unwrap();
        assert_eq!(s.real_path().unwrap(), t.path().join("moved"));
        let d = Root::open(&dst).unwrap().dir(Path::new(""), false).unwrap();
        let n = d.copy_source(&s, "src", 1000).unwrap();
        assert_eq!(
            std::fs::read_to_string(dst.join(&n).join("d/f")).unwrap(),
            "abc"
        );
        assert!(std::fs::symlink_metadata(dst.join(&n).join("d/fifo")).is_err());
        assert!(std::fs::symlink_metadata(dst.join(&n).join("d/link")).is_err());
        // Over the limit: nothing stays behind.
        assert!(d.copy_source(&s, "small", 10).is_err());
        assert!(!dst.join("small").exists());
    }

    #[test]
    fn connect_refuses_a_link_to_another_socket() {
        let t = tempfile::tempdir().unwrap();
        let _l = std::os::unix::net::UnixListener::bind(t.path().join("host.sock")).unwrap();
        let _m = std::os::unix::net::UnixListener::bind(t.path().join("mine.sock")).unwrap();
        symlink(t.path().join("host.sock"), t.path().join("planted.sock")).unwrap();
        let d = Root::open(t.path())
            .unwrap()
            .dir(Path::new(""), false)
            .unwrap();
        assert!(d.connect(OsStr::new("planted.sock")).is_err());
        assert!(d.connect(OsStr::new("mine.sock")).is_ok());
        std::fs::write(t.path().join("file"), "").unwrap();
        assert!(d.connect(OsStr::new("file")).is_err());
    }
}
