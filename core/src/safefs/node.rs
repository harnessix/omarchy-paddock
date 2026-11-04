//! One open directory entry.
//!
//! Linux looks at the entry through an `O_PATH` fd. macOS stats it first
//! and opens only a regular file or a folder.

#[cfg(target_os = "linux")]
use super::{fd_path, open_path};
use super::{fstat, kind_of, last_err, open_raw, reopen_dir, Kind};
use anyhow::{bail, Context, Result};
use std::ffi::CString;
#[cfg(target_os = "macos")]
use std::os::fd::AsRawFd;
use std::os::fd::OwnedFd;
use std::path::Path;

/// An entry opened to look at it, not to read it. `st` is the stat of the
/// open fd. See the module doc for Linux and macOS.
pub(super) struct Node {
    pub(super) fd: Option<OwnedFd>,
    pub(super) st: libc::stat,
}

impl Node {
    /// Open `name` in `dir` (or a path with `AT_FDCWD`). A symlink at the
    /// end is followed only with `follow`. Without it, the node is the link.
    #[cfg(target_os = "linux")]
    pub(super) fn at(dir: libc::c_int, name: &CString, follow: bool) -> std::io::Result<Self> {
        let mut flags = libc::O_PATH;
        if !follow {
            flags |= libc::O_NOFOLLOW;
        }
        let fd = open_raw(dir, name, flags)?;
        let st = fstat(&fd)?;
        Ok(Self { fd: Some(fd), st })
    }

    /// Open `name` in `dir` (or a path with `AT_FDCWD`). A symlink at the
    /// end is followed only with `follow`. Without it, the node is the link.
    /// Only a regular file or a folder gets opened.
    #[cfg(target_os = "macos")]
    pub(super) fn at(dir: libc::c_int, name: &CString, follow: bool) -> std::io::Result<Self> {
        let (at_flag, nofollow) = if follow {
            (0, 0)
        } else {
            (libc::AT_SYMLINK_NOFOLLOW, libc::O_NOFOLLOW)
        };
        // SAFETY: fstatat fills a zeroed stat for a NUL-terminated name.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstatat(dir, name.as_ptr(), &mut st, at_flag) } != 0 {
            return Err(last_err());
        }
        let opened = match kind_of(&st) {
            Kind::File => open_raw(
                dir,
                name,
                libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOCTTY | nofollow,
            ),
            Kind::Dir => open_raw(dir, name, libc::O_RDONLY | libc::O_DIRECTORY | nofollow)
                .or_else(|e| match e.raw_os_error() {
                    // A folder without read permission, but with search.
                    Some(libc::EACCES) => open_raw(dir, name, libc::O_SEARCH | nofollow),
                    _ => Err(e),
                }),
            // A symlink, socket, FIFO or device is never opened.
            _ => return Ok(Self { fd: None, st }),
        };
        match opened {
            Ok(fd) => {
                let st = fstat(&fd)?;
                Ok(Self { fd: Some(fd), st })
            }
            // No permission to open it: only the stat, as Linux has.
            Err(e) if e.raw_os_error() == Some(libc::EACCES) => Ok(Self { fd: None, st }),
            Err(e) => Err(e),
        }
    }

    pub(super) fn kind(&self) -> Kind {
        kind_of(&self.st)
    }

    pub(super) fn fd(&self) -> std::io::Result<&OwnedFd> {
        self.fd
            .as_ref()
            .ok_or_else(|| std::io::Error::from_raw_os_error(libc::EACCES))
    }

    /// A readable fd of the folder.
    pub(super) fn dir(&self) -> std::io::Result<OwnedFd> {
        reopen_dir(self.fd()?)
    }

    /// Open the regular file for reading. The kind is checked first, so a
    /// FIFO or a device is never opened.
    pub(super) fn read(&self, what: &Path) -> Result<std::fs::File> {
        match self.kind() {
            Kind::File => {}
            Kind::Symlink => bail!(
                "{} is a symlink; paddock does not follow it",
                what.display()
            ),
            _ => bail!("{} is not a regular file", what.display()),
        }
        self.reopen_file()
            .with_context(|| format!("cannot read {}", what.display()))
    }

    /// Open the regular file behind the `O_PATH` fd for reading.
    #[cfg(target_os = "linux")]
    pub(super) fn reopen_file(&self) -> std::io::Result<std::fs::File> {
        let p = CString::new(fd_path(self.fd()?)).expect("no NUL in a /proc path");
        let f = open_path(&p, libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOCTTY)?;
        Ok(std::fs::File::from(f))
    }

    /// The file is open for reading already. A copy of the fd shares its
    /// offset, so it starts again at 0 for each read.
    #[cfg(target_os = "macos")]
    pub(super) fn reopen_file(&self) -> std::io::Result<std::fs::File> {
        use std::io::Seek;
        let mut f = std::fs::File::from(self.fd()?.try_clone()?);
        f.seek(std::io::SeekFrom::Start(0))?;
        Ok(f)
    }

    /// Change the mode of the open folder, not of a path.
    #[cfg(target_os = "linux")]
    pub(super) fn chmod(&self, mode: libc::mode_t) -> std::io::Result<()> {
        // chmod of /proc/self/fd/N changes the open folder, not a path.
        let p = CString::new(fd_path(self.fd()?)).expect("no NUL in a /proc path");
        // SAFETY: chmod with a NUL-terminated path.
        if unsafe { libc::chmod(p.as_ptr(), mode) } != 0 {
            return Err(last_err());
        }
        Ok(())
    }

    /// Change the mode of the open folder, not of a path.
    #[cfg(target_os = "macos")]
    pub(super) fn chmod(&self, mode: libc::mode_t) -> std::io::Result<()> {
        let fd = self.fd.as_ref().ok_or_else(|| {
            std::io::Error::other(
                "changing a folder without read and search permission is not \
                 available on macOS yet",
            )
        })?;
        // SAFETY: fchmod on an open fd.
        if unsafe { libc::fchmod(fd.as_raw_fd(), mode) } != 0 {
            return Err(last_err());
        }
        Ok(())
    }
}
