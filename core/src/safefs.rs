//! File operations of the host inside a folder that a sandbox can write: a
//! sandbox home or a project.
//!
//! A sandbox can replace any entry there with a symlink, also between a check
//! and the next call. So every operation walks from the root one component
//! at a time with `openat(O_NOFOLLOW)`, and acts through the directory it
//! opened: `openat` to read, a new file plus `renameat` to write, `unlinkat`
//! to remove. A symlink anywhere on the way is an error, never followed.
//! Use plain `std::fs` only for paths that only the host can write.
//!
//! # Guarantees
//!
//! - `Root::open`: the last component is not a symlink. The folders above
//!   it belong to the host and are followed.
//! - `Root::dir`, `Dir::subdir`: every component opens with `O_NOFOLLOW`
//!   below the open folder before it. A symlink or a file on the way is an
//!   error. With `create`, missing folders get mode 0700.
//! - `Root::read`, `Dir::read`, `Dir::open_read`, `open_file`, `read_path`:
//!   only a regular file is read. A symlink (unless `follow`), a folder, a
//!   FIFO or a device is an error, so a read never hangs. A read stops after
//!   `max` bytes.
//! - `Root::write`, `Dir::write`, `Dir::write_from`: a new file next to the
//!   name, then a rename over the name. A symlink at the name gets replaced,
//!   never written through.
//! - `Dir::create_new`, `Dir::new_file`, `Dir::new_subdir`: a new entry
//!   only. An existing entry or a symlink there is never replaced or opened.
//! - `Dir::move_in`: a rename that never replaces an entry at the target.
//! - `Dir::exchange`: two entries of one folder swap names in one step.
//! - `Root::remove`, `Dir::remove`, `Dir::remove_dir`: `unlinkat` in the open
//!   folder. A symlink is removed itself.
//! - `Root::remove_tree`, `Dir::remove_tree`, `remove_tree_path`: everything
//!   below is removed through open folders. Symlinks are removed, never
//!   followed. A folder without write permission gets it through its open fd.
//! - `Dir::kind`, `Dir::entries`, `Dir::file_names`, `Dir::stats`: `fstatat`
//!   with `AT_SYMLINK_NOFOLLOW`. Nothing is opened.
//! - `Dir::connect`: only a socket, never a symlink to one.
//! - `Root::copy_in`, `Dir::copy_source`, `Source`: the source opens once.
//!   Later steps work on the open fd, so a path that changes cannot redirect
//!   the copy. Only regular files and folders get copied.
//! - `resolve_approved`: a symlink inside a folder that a sandbox can write
//!   is an error.
//!
//! # Linux and macOS
//!
//! Linux opens an entry to look at it with `O_PATH`. Such an fd opens
//! nothing, so a FIFO, a device or a socket is never opened. It also gets
//! reopened through `/proc/self/fd`. macOS has neither. There `Node::at`
//! first takes `fstatat` and then opens only a regular file (`O_RDONLY`,
//! `O_NONBLOCK`, `O_NOCTTY`) or a folder (`O_RDONLY | O_DIRECTORY`, or
//! `O_SEARCH` without read permission). Any other kind keeps only its stat.
//! The kind checks then use the stat of the open fd, so an entry swapped in
//! between the two calls is checked as what was opened. Inside a sandbox
//! folder, `O_NOFOLLOW` leaves only a FIFO to swap in, and a FIFO opens at
//! once with `O_NONBLOCK`. For a path open (`open_file`, `Source::open`) a
//! race on a folder above the file can make macOS open a device, but never
//! read it. The other macOS calls: `renameatx_np` with `RENAME_EXCL` or
//! `RENAME_SWAP`, `fcntl(F_GETPATH)`, `fdopendir`, `fchmod`.
//!
//! Not available on macOS yet: `Dir::connect` and `Source::proc_path`, as
//! no path names an open fd there, and removing a folder that has neither
//! read nor search permission. These return an error that says so.

use anyhow::{bail, Context, Result};
use std::collections::VecDeque;
use std::ffi::{CString, OsStr, OsString};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
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

/// Kind, size and change time of an entry, taken without following it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stat {
    pub kind: Kind,
    pub size: u64,
    pub mtime: (i64, i64),
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

/// The mode bits as `u32`. `mode_t` is `u16` on macOS.
#[allow(clippy::unnecessary_cast)]
fn mode_of(st: &libc::stat) -> u32 {
    st.st_mode as u32
}

/// The path that names an open fd itself. Opening, connecting or changing
/// it acts on the object the fd holds, never on a path that could change.
#[cfg(target_os = "linux")]
fn fd_path(fd: &OwnedFd) -> String {
    format!("/proc/self/fd/{}", fd.as_raw_fd())
}

/// Where an open file or folder is now, from the kernel.
#[cfg(target_os = "linux")]
fn fd_real_path(fd: &OwnedFd) -> Result<PathBuf> {
    Ok(std::fs::read_link(fd_path(fd))?)
}

/// Where an open file or folder is now, from the kernel.
#[cfg(target_os = "macos")]
fn fd_real_path(fd: &OwnedFd) -> Result<PathBuf> {
    let mut buf = vec![0u8; libc::PATH_MAX as usize];
    // SAFETY: F_GETPATH writes a NUL-terminated path of at most PATH_MAX
    // bytes into buf.
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETPATH, buf.as_mut_ptr()) } < 0 {
        return Err(last_err().into());
    }
    let len = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    buf.truncate(len);
    Ok(PathBuf::from(OsString::from_vec(buf)))
}

fn open_path(path: &CString, flags: libc::c_int) -> std::io::Result<OwnedFd> {
    open_raw(libc::AT_FDCWD, path, flags)
}

/// `openat` with only `O_CLOEXEC` added, so a symlink at the end is
/// followed unless `flags` has `O_NOFOLLOW`.
fn open_raw(dir: libc::c_int, name: &CString, flags: libc::c_int) -> std::io::Result<OwnedFd> {
    // SAFETY: openat with a NUL-terminated name; the fd is owned on success.
    let r = unsafe { libc::openat(dir, name.as_ptr(), flags | libc::O_CLOEXEC) };
    if r < 0 {
        Err(last_err())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(r) })
    }
}

mod node;
use node::Node;

/// The renameat2 system call, direct: the musl libc of the Rust musl
/// targets (the guest binary of the VM backend) has no wrapper for it.
#[cfg(target_os = "linux")]
fn renameat2(
    fd: libc::c_int,
    a: &CString,
    to: libc::c_int,
    b: &CString,
    flags: libc::c_uint,
) -> std::io::Result<()> {
    // SAFETY: renameat2 with NUL-terminated names that outlive the call.
    let r = unsafe { libc::syscall(libc::SYS_renameat2, fd, a.as_ptr(), to, b.as_ptr(), flags) };
    if r != 0 {
        return Err(last_err());
    }
    Ok(())
}

/// Rename `from` in `fromdir` to `to` in `todir`. An entry at `to` is never
/// replaced: that is an error.
#[cfg(target_os = "linux")]
fn rename_noreplace(
    fromdir: libc::c_int,
    from: &CString,
    todir: libc::c_int,
    to: &CString,
) -> std::io::Result<()> {
    renameat2(fromdir, from, todir, to, libc::RENAME_NOREPLACE)
}

/// Rename `from` in `fromdir` to `to` in `todir`. An entry at `to` is never
/// replaced: that is an error.
#[cfg(target_os = "macos")]
fn rename_noreplace(
    fromdir: libc::c_int,
    from: &CString,
    todir: libc::c_int,
    to: &CString,
) -> std::io::Result<()> {
    // SAFETY: renameatx_np (<stdio.h>) with NUL-terminated names.
    let r = unsafe {
        libc::renameatx_np(
            fromdir,
            from.as_ptr(),
            todir,
            to.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    if r != 0 {
        return Err(last_err());
    }
    Ok(())
}

/// Swap the entries `a` and `b` of the folder `dir` in one step.
#[cfg(target_os = "linux")]
fn rename_exchange(dir: libc::c_int, a: &CString, b: &CString) -> std::io::Result<()> {
    renameat2(dir, a, dir, b, libc::RENAME_EXCHANGE)
}

/// Swap the entries `a` and `b` of the folder `dir` in one step.
#[cfg(target_os = "macos")]
fn rename_exchange(dir: libc::c_int, a: &CString, b: &CString) -> std::io::Result<()> {
    // SAFETY: renameatx_np (<stdio.h>) with NUL-terminated names.
    let r = unsafe { libc::renameatx_np(dir, a.as_ptr(), dir, b.as_ptr(), libc::RENAME_SWAP) };
    if r != 0 {
        return Err(last_err());
    }
    Ok(())
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
#[cfg(target_os = "linux")]
fn list(dir: &OwnedFd) -> Result<Vec<std::ffi::OsString>> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(fd_path(dir))? {
        out.push(e?.file_name());
    }
    Ok(out)
}

/// The names in an open folder, without `.` and `..`. A new fd of the
/// folder gets its own offset, so `dir` itself does not change.
#[cfg(target_os = "macos")]
fn list(dir: &OwnedFd) -> Result<Vec<std::ffi::OsString>> {
    use std::os::fd::IntoRawFd;
    let raw = reopen_dir(dir)?.into_raw_fd();
    // SAFETY: fdopendir owns raw on success. On failure raw stays ours.
    let dp = unsafe { libc::fdopendir(raw) };
    if dp.is_null() {
        let e = last_err();
        unsafe { libc::close(raw) };
        return Err(e.into());
    }
    let mut out = Vec::new();
    let result = loop {
        // SAFETY: errno is thread local. readdir leaves it at 0 at the end.
        unsafe { *libc::__error() = 0 };
        let ent = unsafe { libc::readdir(dp) };
        if ent.is_null() {
            let e = last_err();
            break match e.raw_os_error() {
                Some(0) | None => Ok(()),
                _ => Err(e),
            };
        }
        // SAFETY: d_name of a dirent from readdir is NUL-terminated.
        let name = unsafe { std::ffi::CStr::from_ptr((*ent).d_name.as_ptr()) }.to_bytes();
        if name != b"." && name != b".." {
            out.push(OsString::from_vec(name.to_vec()));
        }
    };
    // SAFETY: closedir closes the stream and its fd once.
    unsafe { libc::closedir(dp) };
    result?;
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
    let node = match Node::at(libc::AT_FDCWD, &cstr(path.as_os_str())?, follow) {
        Ok(n) => n,
        Err(e) if e.raw_os_error() == Some(libc::ENOENT) => return Ok(None),
        Err(e) => {
            return Err(anyhow::Error::from(e).context(format!("cannot open {}", path.display())))
        }
    };
    node.read(path).map(Some)
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
    node: Node,
    kind: Kind,
}

impl Source {
    /// Open `path`. Symlinks in it are followed, as the user named it;
    /// `real_path` tells where it led. Only a regular file or a folder opens.
    pub fn open(path: &Path) -> Result<Self> {
        let node = Node::at(libc::AT_FDCWD, &cstr(path.as_os_str())?, true)
            .with_context(|| format!("{} does not exist on the host", path.display()))?;
        let kind = node.kind();
        if !matches!(kind, Kind::File | Kind::Dir) {
            bail!(
                "{} is not a regular file or a folder; paddock copies only those",
                path.display()
            );
        }
        Ok(Self { node, kind })
    }

    /// Where the open file or folder is now.
    pub fn real_path(&self) -> Result<PathBuf> {
        fd_real_path(self.node.fd()?)
    }

    pub fn is_dir(&self) -> bool {
        self.kind == Kind::Dir
    }

    /// A path that names the open file or folder itself, valid in this
    /// process while the Source lives.
    #[cfg(target_os = "linux")]
    pub fn proc_path(&self) -> Result<PathBuf> {
        Ok(PathBuf::from(fd_path(self.node.fd()?)))
    }

    /// macOS has no path that names an open fd and can be walked below.
    #[cfg(target_os = "macos")]
    pub fn proc_path(&self) -> Result<PathBuf> {
        bail!("a path to an open host folder (/proc/self/fd) is not available on macOS yet")
    }

    /// The open folder as a `Root`, to walk it without following a symlink.
    pub fn root(&self) -> Result<Root> {
        if self.kind != Kind::Dir {
            bail!("not a folder");
        }
        Ok(Root {
            fd: self.node.dir()?,
            path: self.real_path()?,
        })
    }

    /// The device and inode of the open file or folder.
    #[allow(clippy::unnecessary_cast)]
    pub fn id(&self) -> Result<(u64, u64)> {
        let st = fstat(self.node.fd()?)?;
        // st_dev is i32 on macOS.
        Ok((st.st_dev as u64, st.st_ino))
    }

    /// The bytes a copy takes; it stops counting above `limit`. Symlinks,
    /// FIFOs and devices inside a folder do not count: a copy skips them.
    pub fn size(&self, limit: u64) -> Result<u64> {
        let mut total = 0;
        size_of(&self.node, limit, &mut total, 0)?;
        Ok(total)
    }
}

fn size_of(node: &Node, limit: u64, total: &mut u64, depth: usize) -> Result<()> {
    match node.kind() {
        Kind::File => *total += fstat(node.fd()?)?.st_size as u64,
        Kind::Dir => {
            if depth > MAX_DEPTH {
                bail!("the folder is too deep");
            }
            let d = node.dir()?;
            for name in list(&d)? {
                if *total > limit {
                    break;
                }
                let e = Node::at(d.as_raw_fd(), &cstr(&name)?, false)?;
                size_of(&e, limit, total, depth + 1)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Connect to the socket that `node` holds, through its own fd path.
#[cfg(target_os = "linux")]
fn connect_node(node: &Node, what: &Path) -> Result<std::os::unix::net::UnixStream> {
    std::os::unix::net::UnixStream::connect(fd_path(node.fd()?))
        .with_context(|| format!("cannot connect to {}", what.display()))
}

/// macOS connects a Unix socket only by path, and a path can be swapped for
/// a symlink after the check.
#[cfg(target_os = "macos")]
fn connect_node(_node: &Node, what: &Path) -> Result<std::os::unix::net::UnixStream> {
    bail!(
        "connecting to the socket {} inside a sandbox folder is not available on macOS yet",
        what.display()
    )
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
        let node = Node::at(libc::AT_FDCWD, &cstr(from.as_os_str())?, false)
            .with_context(|| format!("cannot open {}", from.display()))?;
        let mut budget = MAX_COPY;
        match node.kind() {
            Kind::Dir => self
                .dir(rel, true)?
                .copy_dir_fd(&node.dir()?, &mut budget, 0),
            Kind::File => {
                let (parent, name) = Self::split(rel)?;
                let bytes = read_limited(node.read(from)?, budget, from)?;
                self.dir(parent, true)?
                    .write(name, &bytes, (mode_of(&node.st) & 0o755) | 0o600)
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
        fd_real_path(&self.fd)
    }

    /// Read a regular file. None when it does not exist. A symlink, a folder
    /// or a special file is an error; a FIFO or a device is never opened.
    pub fn read(&self, name: &OsStr, max: u64) -> Result<Option<Vec<u8>>> {
        let what = self.path.join(name);
        let node = match Node::at(self.fd.as_raw_fd(), &cstr(name)?, false) {
            Ok(n) => n,
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => return Ok(None),
            Err(e) => return Err(symlink_error(e, &what)),
        };
        let f = node.read(&what)?;
        if f.metadata()?.len() > max {
            bail!("{} is larger than {} bytes", what.display(), max);
        }
        read_limited(f, max, &what).map(Some)
    }

    /// Open a regular file for reading, without following a symlink. None
    /// when it does not exist. A FIFO or a device is never opened.
    pub fn open_read(&self, name: &OsStr) -> Result<Option<std::fs::File>> {
        let what = self.path.join(name);
        let node = match Node::at(self.fd.as_raw_fd(), &cstr(name)?, false) {
            Ok(n) => n,
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => return Ok(None),
            Err(e) => return Err(symlink_error(e, &what)),
        };
        node.read(&what).map(Some)
    }

    /// The target of the symlink `name`.
    pub fn read_link(&self, name: &OsStr) -> Result<PathBuf> {
        let n = cstr(name)?;
        let mut buf = vec![0u8; 4096];
        // SAFETY: readlinkat writes at most buf.len() bytes into buf.
        let r = unsafe {
            libc::readlinkat(
                self.fd.as_raw_fd(),
                n.as_ptr(),
                buf.as_mut_ptr() as *mut libc::c_char,
                buf.len(),
            )
        };
        if r < 0 {
            return Err(anyhow::Error::from(last_err())
                .context(format!("cannot read {}", self.path.join(name).display())));
        }
        buf.truncate(r as usize);
        Ok(PathBuf::from(OsString::from_vec(buf)))
    }

    /// Move `from`, a path that only the host can write, into this folder
    /// as `name`. An existing entry is never replaced.
    pub fn move_in(&self, from: &Path, name: &OsStr) -> std::io::Result<()> {
        let (f, n) = (
            CString::new(from.as_os_str().as_bytes()).map_err(std::io::Error::other)?,
            CString::new(name.as_bytes()).map_err(std::io::Error::other)?,
        );
        // Never replaces an entry that the sandbox put there.
        rename_noreplace(libc::AT_FDCWD, &f, self.fd.as_raw_fd(), &n)
    }

    /// Swap the entries `a` and `b` of this folder in one step. Both must
    /// exist. Symlinks are swapped as they are, never followed.
    pub fn exchange(&self, a: &OsStr, b: &OsStr) -> Result<()> {
        rename_exchange(self.fd.as_raw_fd(), &cstr(a)?, &cstr(b)?).with_context(|| {
            format!(
                "cannot swap {} and {}",
                self.path.join(a).display(),
                self.path.join(b).display()
            )
        })
    }

    /// What the entry `name` is, without following it. None when missing.
    pub fn kind(&self, name: &OsStr) -> Result<Option<Kind>> {
        Ok(self.lstat(name)?.map(|st| kind_of(&st)))
    }

    /// The entries of this folder with their kind, size and change time,
    /// without following any.
    pub fn stats(&self) -> Result<Vec<(OsString, Stat)>> {
        let mut out = Vec::new();
        for n in list(&self.fd)? {
            if let Some(st) = self.lstat(&n)? {
                out.push((
                    n,
                    Stat {
                        kind: kind_of(&st),
                        size: st.st_size as u64,
                        mtime: (st.st_mtime, st.st_mtime_nsec),
                    },
                ));
            }
        }
        Ok(out)
    }

    fn lstat(&self, name: &OsStr) -> Result<Option<libc::stat>> {
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
        Ok(Some(st))
    }

    /// Connect to the Unix socket `name`. A symlink there is an error, so a
    /// sandbox cannot point the host at another socket of the host.
    pub fn connect(&self, name: &OsStr) -> Result<std::os::unix::net::UnixStream> {
        let what = self.path.join(name);
        let node = Node::at(self.fd.as_raw_fd(), &cstr(name)?, false)
            .map_err(|e| symlink_error(e, &what))?;
        if node.kind() != Kind::Socket {
            bail!("{} is not a socket", what.display());
        }
        connect_node(&node, &what)
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
        let e = match Node::at(self.fd.as_raw_fd(), &n, false) {
            Ok(f) => f,
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => return Ok(()),
            Err(e) => return Err(symlink_error(e, &path)),
        };
        let st = e.st;
        if kind_of(&st) != Kind::Dir {
            return self.remove(name);
        }
        if depth > MAX_DEPTH {
            bail!("{} is too deep to remove", path.display());
        }
        if st.st_mode & 0o700 != 0o700 {
            if let Err(err) = e.chmod((st.st_mode & 0o7777) | 0o700) {
                return Err(
                    anyhow::Error::from(err).context(format!("cannot change {}", path.display()))
                );
            }
        }
        let sub = Dir {
            fd: e.dir().map_err(|e| symlink_error(e, &path))?,
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
            let r = src
                .node
                .dir()
                .map_err(anyhow::Error::from)
                .and_then(|s| d.copy_dir_fd(&s, &mut budget, 0));
            if let Err(e) = r {
                let _ = self.remove_tree(OsStr::new(&name));
                return Err(e);
            }
            Ok(name)
        } else {
            let mode = mode_of(&fstat(src.node.fd()?)?) & 0o755;
            let (mut f, name) = self.new_file(want, mode | 0o600)?;
            let r = src
                .node
                .read(Path::new(want))
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
            let e = Node::at(src.as_raw_fd(), &cstr(&name)?, false)?;
            match e.kind() {
                Kind::Dir => {
                    let sub = self.subdir(&name, true)?;
                    sub.copy_dir_fd(&e.dir()?, budget, depth + 1)?;
                }
                Kind::File => {
                    let what = self.path.join(&name);
                    let bytes = read_limited(e.read(&what)?, *budget, &what)
                        .context("the copy is larger than the limit")?;
                    *budget -= bytes.len() as u64;
                    self.write(&name, &bytes, (mode_of(&e.st) & 0o755) | 0o600)?;
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
        self.write_with(name, &mut &*bytes, mode, false).map(|_| ())
    }

    /// `write` from a reader, and the file gets exactly `mode`, without the
    /// umask. Returns the bytes written.
    pub fn write_from(&self, name: &OsStr, from: &mut impl Read, mode: u32) -> Result<u64> {
        self.write_with(name, from, mode, true)
    }

    fn write_with(
        &self,
        name: &OsStr,
        from: &mut impl Read,
        mode: u32,
        exact: bool,
    ) -> Result<u64> {
        let mut tmp = name.to_os_string();
        tmp.push(format!(".paddock-tmp-{}", std::process::id()));
        let _ = self.remove(&tmp);
        let mut f = self.create_new(&tmp, if exact { 0o600 } else { mode })?;
        let written = std::io::copy(from, &mut f).and_then(|n| {
            f.sync_all()?;
            if exact {
                use std::os::unix::fs::PermissionsExt;
                f.set_permissions(std::fs::Permissions::from_mode(mode))?;
            }
            Ok(n)
        });
        drop(f);
        let (t, n) = (cstr(&tmp)?, cstr(name)?);
        let written = match written {
            Ok(w) => w,
            Err(e) => {
                let _ = self.remove(&tmp);
                return Err(e.into());
            }
        };
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
        Ok(written)
    }

    /// Remove the empty folder `name`. Missing is fine.
    pub fn remove_dir(&self, name: &OsStr) -> std::io::Result<()> {
        let n = CString::new(name.as_bytes()).map_err(std::io::Error::other)?;
        // SAFETY: unlinkat with AT_REMOVEDIR removes only an empty folder.
        if unsafe { libc::unlinkat(self.fd.as_raw_fd(), n.as_ptr(), libc::AT_REMOVEDIR) } != 0 {
            let e = last_err();
            if e.raw_os_error() != Some(libc::ENOENT) {
                return Err(e);
            }
        }
        Ok(())
    }

    /// Make the folder `name` with `mode` (less the umask). A folder that is
    /// there already is fine; anything else there is an error.
    pub fn make_dir(&self, name: &OsStr, mode: u32) -> Result<()> {
        let n = cstr(name)?;
        // SAFETY: mkdirat with a NUL-terminated name.
        if unsafe { libc::mkdirat(self.fd.as_raw_fd(), n.as_ptr(), mode as libc::mode_t) } != 0 {
            let e = last_err();
            if e.raw_os_error() != Some(libc::EEXIST) || self.kind(name)? != Some(Kind::Dir) {
                return Err(anyhow::Error::from(e)
                    .context(format!("cannot create {}", self.path.join(name).display())));
            }
        }
        Ok(())
    }

    /// Make the symlink `name` that points to `target`.
    pub fn symlink(&self, target: &Path, name: &OsStr) -> Result<()> {
        let (t, n) = (cstr(target.as_os_str())?, cstr(name)?);
        // SAFETY: symlinkat with NUL-terminated strings.
        if unsafe { libc::symlinkat(t.as_ptr(), self.fd.as_raw_fd(), n.as_ptr()) } != 0 {
            return Err(anyhow::Error::from(last_err())
                .context(format!("cannot create {}", self.path.join(name).display())));
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
mod tests;
