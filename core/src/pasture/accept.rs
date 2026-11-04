//! Accept loop that works on Linux and macOS.

use std::sync::atomic::{AtomicBool, Ordering};

/// How long the accept thread waits before it looks at the stop flag.
/// `shutdown` on a listening socket unblocks `accept` on Linux and does not
/// on macOS, and a connection to the socket path can reach a new owner of
/// that path. A short poll works on both.
const ACCEPT_TICK_MS: i32 = 100;

/// True when `fd` has a connection to accept.
fn readable(fd: i32) -> bool {
    let mut p = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    (unsafe { libc::poll(&mut p, 1, ACCEPT_TICK_MS) }) > 0
}

/// Wait until `fd` can accept, or `stop` is set. False means stop.
pub(super) fn ready(fd: i32, stop: &AtomicBool) -> bool {
    while !stop.load(Ordering::SeqCst) {
        if readable(fd) {
            return !stop.load(Ordering::SeqCst);
        }
    }
    false
}
