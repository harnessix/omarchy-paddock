//! Small terminal helpers of the VM backend: a pty in the guest, the window
//! size, and raw mode on the host terminal.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

/// A new pty with this window size: (master, slave).
pub fn open_pty(rows: u16, cols: u16) -> io::Result<(OwnedFd, OwnedFd)> {
    let mut master = 0;
    let mut slave = 0;
    let mut size = winsize(rows, cols);
    // SAFETY: openpty writes two new fds to these locals. The name and the
    // termios may be null.
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            if rows > 0 && cols > 0 {
                &mut size
            } else {
                std::ptr::null_mut()
            },
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openpty returned two new fds that nothing else owns.
    let (m, s) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    for fd in [&m, &s] {
        // SAFETY: fcntl on an open fd of this process.
        unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    Ok((m, s))
}

fn winsize(rows: u16, cols: u16) -> libc::winsize {
    libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

/// Set the window size of a terminal.
pub fn set_size(fd: RawFd, rows: u16, cols: u16) {
    let size = winsize(rows, cols);
    // SAFETY: TIOCSWINSZ on an open fd with a valid winsize.
    unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &size) };
}

/// The window size of a terminal, or None.
pub fn get_size(fd: RawFd) -> Option<(u16, u16)> {
    // SAFETY: TIOCGWINSZ writes a winsize.
    unsafe {
        let mut size: libc::winsize = std::mem::zeroed();
        if libc::ioctl(fd, libc::TIOCGWINSZ, &mut size) < 0 || size.ws_row == 0 {
            return None;
        }
        Some((size.ws_row, size.ws_col))
    }
}

/// Raw mode on a terminal until the guard drops.
pub struct RawMode {
    fd: RawFd,
    saved: libc::termios,
}

impl RawMode {
    pub fn enter(fd: RawFd) -> io::Result<Self> {
        // SAFETY: tcgetattr and tcsetattr on an open terminal fd.
        unsafe {
            let mut saved: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut saved) < 0 {
                return Err(io::Error::last_os_error());
            }
            let mut raw = saved;
            libc::cfmakeraw(&mut raw);
            if libc::tcsetattr(fd, libc::TCSANOW, &raw) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self { fd, saved })
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        // SAFETY: restores the settings that `enter` read from this fd.
        unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.saved) };
    }
}

/// The exit code of a process as a shell reports it.
pub fn exit_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .or_else(|| status.signal().map(|s| 128 + s))
        .unwrap_or(1)
}
