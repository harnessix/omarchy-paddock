//! Small helpers on file descriptors for the relay.

use std::io;
use std::os::fd::RawFd;

pub(super) fn pipe() -> io::Result<(RawFd, RawFd)> {
    let mut fds = [0; 2];
    // SAFETY: pipe writes two new fds.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((fds[0], fds[1]))
}

pub(super) fn dup_fd(fd: RawFd) -> io::Result<RawFd> {
    // SAFETY: dup of an open fd returns a new fd or -1.
    let d = unsafe { libc::dup(fd) };
    if d < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(d)
}

pub(super) fn cloexec(fd: RawFd) -> io::Result<()> {
    // SAFETY: fcntl on an open fd of this process.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        if libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

pub(super) fn set_nonblock(fd: RawFd) -> io::Result<()> {
    // SAFETY: fcntl on an open fd of this process.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        if libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}
