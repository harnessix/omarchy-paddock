//! macOS stand-ins for the Linux process calls.

use std::io::Error;

const IOPOL_TYPE_DISK: i32 = 0;
const IOPOL_SCOPE_THREAD: i32 = 1;
const IOPOL_THROTTLE: i32 = 3;

unsafe extern "C" {
    /// Thread disk policy. Present in libSystem. The result is ignored.
    fn setiopolicy_np(iotype: i32, scope: i32, policy: i32) -> i32;
}

/// SIGTERM this process when its parent exits.
///
/// The caller is about to exec. exec ends every thread in that process, so
/// a helper thread there would die before it could see the parent exit.
/// This forks a watcher. The watcher is not a child of the program that
/// execs. It waits on `kqueue` (`EVFILT_PROC`, `NOTE_EXIT`) and then sends
/// SIGTERM, the same signal Linux asks the kernel to send.
///
/// The watcher closes inherited descriptors before the second fork. Rust
/// waits in `Command::spawn` until a pipe write end closes. A watcher that
/// kept that pipe would block spawn until the child exited, so a time limit
/// would never run.
///
/// # Safety
/// Call this after fork and before exec. The process that returns only
/// calls `fork`, `close` and `waitpid`. The watcher calls `watch_parent`
/// and does not allocate.
pub unsafe fn arm_parent_death_signal() -> std::io::Result<()> {
    let parent = libc::getppid();
    let target = libc::getpid();
    let mid = libc::fork();
    if mid < 0 {
        return Err(Error::last_os_error());
    }
    if mid == 0 {
        // Close the spawn pipe before the second fork. `close` is
        // async-signal-safe. Do not allocate here.
        let mut fd = 3;
        while fd < 1024 {
            libc::close(fd);
            fd += 1;
        }
        if libc::setsid() < 0 {
            libc::_exit(1);
        }
        let grand = libc::fork();
        if grand != 0 {
            libc::_exit(if grand < 0 { 1 } else { 0 });
        }
        // One thread exists here. A new thread would allocate after fork.
        watch_parent(parent, target, libc::SIGTERM);
    }
    let mut status = 0;
    let reaped = libc::waitpid(mid, &mut status, 0);
    if reaped == mid && libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0 {
        Ok(())
    } else {
        Err(Error::other("cannot arm the parent-death watcher"))
    }
}

/// Wait until `parent` or `target` exits. On the parent's exit, signal `target`.
fn watch_parent(parent: i32, target: i32, sig: i32) -> ! {
    unsafe {
        let kq = libc::kqueue();
        if kq < 0 {
            libc::_exit(1);
        }
        let change = [
            proc_event(parent, libc::NOTE_EXIT),
            proc_event(target, libc::NOTE_EXIT),
        ];
        if libc::kevent(
            kq,
            change.as_ptr(),
            2,
            std::ptr::null_mut(),
            0,
            std::ptr::null(),
        ) < 0
        {
            libc::_exit(1);
        }
        // The parent can exit before the watch is registered.
        if libc::kill(parent, 0) < 0 && *libc::__error() == libc::ESRCH {
            libc::kill(target, sig);
            libc::_exit(0);
        }
        let mut got: libc::kevent = std::mem::zeroed();
        if libc::kevent(kq, std::ptr::null(), 0, &mut got, 1, std::ptr::null()) > 0
            && got.ident == parent as libc::uintptr_t
        {
            libc::kill(target, sig);
        }
        libc::_exit(0);
    }
}

fn proc_event(pid: i32, note: u32) -> libc::kevent {
    libc::kevent {
        ident: pid as libc::uintptr_t,
        filter: libc::EVFILT_PROC,
        flags: libc::EV_ADD | libc::EV_ENABLE,
        fflags: note,
        data: 0,
        udata: std::ptr::null_mut(),
    }
}

/// True when the other end closed its write side.
///
/// macOS has no `POLLRDHUP`. `POLLHUP` covers a full close. A zero-byte
/// read covers a write-side close that left the socket readable.
pub fn peer_gone(fd: i32) -> bool {
    let mut p = libc::pollfd {
        fd,
        events: libc::POLLIN | libc::POLLHUP,
        revents: 0,
    };
    let n = unsafe { libc::poll(&mut p, 1, 0) };
    if n <= 0 {
        return false;
    }
    if p.revents & (libc::POLLHUP | libc::POLLERR) != 0 {
        return true;
    }
    if p.revents & libc::POLLIN != 0 {
        let mut buf = [0u8; 1];
        let r = unsafe {
            libc::recv(
                fd,
                buf.as_mut_ptr().cast(),
                1,
                libc::MSG_PEEK | libc::MSG_DONTWAIT,
            )
        };
        return r == 0;
    }
    false
}

/// Set this thread to the throttle disk class.
///
/// # Safety
/// A plain `setiopolicy_np` on this thread. The result is ignored, the same
/// way the Linux `ioprio_set` call ignores its result.
pub unsafe fn be_idle_io() {
    let _ = setiopolicy_np(IOPOL_TYPE_DISK, IOPOL_SCOPE_THREAD, IOPOL_THROTTLE);
}
