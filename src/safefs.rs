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
use std::ffi::{CString, OsStr};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path};

/// Credentials, JSON settings and similar files are small.
pub const MAX_READ: u64 = 4 << 20;

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
    /// Symlinks in the source are skipped; nothing in the target is followed.
    pub fn copy_in(&self, from: &Path, rel: &Path) -> Result<()> {
        let meta = std::fs::symlink_metadata(from)?;
        if meta.is_dir() {
            let d = self.dir(rel, true)?;
            d.copy_tree_in(from)
        } else if meta.is_file() {
            let (parent, name) = Self::split(rel)?;
            self.dir(parent, true)?.copy_file_in(from, name)
        } else {
            Ok(())
        }
    }
}

impl Dir {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn read(&self, name: &OsStr, max: u64) -> Result<Option<Vec<u8>>> {
        let n = cstr(name)?;
        let what = self.path.join(name);
        let fd = match openat(
            self.fd.as_raw_fd(),
            &n,
            libc::O_RDONLY | libc::O_NONBLOCK,
            0,
        ) {
            Ok(f) => f,
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => return Ok(None),
            Err(e) => return Err(symlink_error(e, &what)),
        };
        let mut f = std::fs::File::from(fd);
        let m = f.metadata()?;
        if !m.is_file() {
            bail!("{} is not a regular file", what.display());
        }
        if m.len() > max {
            bail!("{} is larger than {} bytes", what.display(), max);
        }
        let mut buf = Vec::new();
        Read::by_ref(&mut f).take(max + 1).read_to_end(&mut buf)?;
        if buf.len() as u64 > max {
            bail!("{} is larger than {} bytes", what.display(), max);
        }
        Ok(Some(buf))
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

    fn copy_file_in(&self, from: &Path, name: &OsStr) -> Result<()> {
        let bytes =
            std::fs::read(from).with_context(|| format!("cannot read {}", from.display()))?;
        let mode = std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(from)?.permissions())
            & 0o755;
        self.write(name, &bytes, mode | 0o600)
    }

    /// Copy the contents of a host folder into this folder.
    pub fn copy_tree_in(&self, from: &Path) -> Result<()> {
        for e in std::fs::read_dir(from)? {
            let e = e?;
            let m = e.file_type()?;
            if m.is_dir() {
                self.subdir(&e.file_name(), true)?.copy_tree_in(&e.path())?;
            } else if m.is_file() {
                self.copy_file_in(&e.path(), &e.file_name())?;
            }
            // Symlinks in the host source are skipped: they could point anywhere.
        }
        Ok(())
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
}
