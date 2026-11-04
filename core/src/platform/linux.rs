//! Linux process calls used by the sandbox.

/// Ask the kernel to deliver SIGTERM to this process when its parent exits.
///
/// # Safety
/// `prctl` is async-signal-safe. Call this after fork and before exec.
pub unsafe fn arm_parent_death_signal() -> std::io::Result<()> {
    libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
    Ok(())
}

/// True when the other end closed its write side.
///
/// `POLLRDHUP` reports that close even while unread bytes remain.
pub fn peer_gone(fd: i32) -> bool {
    let mut p = libc::pollfd {
        fd,
        events: libc::POLLIN | libc::POLLRDHUP,
        revents: 0,
    };
    let n = unsafe { libc::poll(&mut p, 1, 0) };
    n > 0 && p.revents & (libc::POLLRDHUP | libc::POLLHUP | libc::POLLERR) != 0
}

/// Set this process to the idle I/O class.
///
/// # Safety
/// A plain `ioprio_set` on this process. The result is ignored, as before.
pub unsafe fn be_idle_io() {
    // IOPRIO_WHO_PROCESS, this process, class IDLE.
    libc::syscall(libc::SYS_ioprio_set, 1, 0, 3 << 13);
}
