//! ptys for Seatbelt terminal runs: paddock opens them on the host.
//!
//! The Seatbelt profile of a terminal run cannot allow `/dev/ptmx` or the
//! `/dev/ttys*` paths: Seatbelt cannot tell a new pty from the user's own
//! terminals by path, and an open terminal gives its keystrokes away. So
//! the broker opens each pty on the host and sends both file descriptors to
//! the sandbox over a Unix socket (`SCM_RIGHTS`). The sandbox never opens a
//! terminal by path.
//!
//! paddock's pty library (src/ptyshim/shim.c, built by build.rs) asks the
//! broker in place of `openpty` and the other pty calls. paddock loads it
//! into the harness with `DYLD_INSERT_LIBRARIES`. A harness that ignores
//! that variable (the hardened runtime without the entitlement, or a binary
//! that macOS protects) keeps the refusal: [`dyld_refusal`] tells which.
//!
//! The protocol: the client writes `pty\n`. The broker answers with one
//! message: `ok NAME\n` and the master and the slave as two descriptors, or
//! `error REASON\n` and none. The broker keeps no copy of either side.
//!
//! Most of this file is platform-neutral and tested on Linux.

use anyhow::{Context as _, Result};
use std::collections::VecDeque;
use std::io::{BufRead as _, BufReader, Write as _};
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The library, empty when build.rs could not build it.
pub static LIBRARY: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/paddock-pty.lib"));

/// The broker socket, for the library in the sandbox.
pub const SOCKET_VAR: &str = "PADDOCK_PTY_SOCKET";
/// The library path. The start command of the run copies it into
/// `DYLD_INSERT_LIBRARIES` right before the harness starts: macOS drops
/// `DYLD_*` variables when it starts a protected program such as
/// `sandbox-exec` or `/bin/sh`.
pub const LIBRARY_VAR: &str = "PADDOCK_PTY_LIB";
/// The file name of the library in the run folder.
pub const LIBRARY_FILE: &str = "libpaddock-pty.dylib";

/// The most ptys in [`WINDOW`]. A harness opens one per command at most, so
/// this stops a sandbox that uses up the ptys of the Mac (there are 511).
pub const BURST: usize = 64;
pub const WINDOW: Duration = Duration::from_secs(10);

/// A running broker.
#[derive(Clone)]
pub struct Broker {
    pub socket: PathBuf,
    issued: Arc<AtomicUsize>,
}

impl Broker {
    /// The ptys that the broker gave out.
    pub fn issued(&self) -> usize {
        self.issued.load(Ordering::SeqCst)
    }
}

/// Listen on `socket` and give a new pty to each request, in a thread that
/// lives as long as paddock.
pub fn serve(socket: &Path) -> Result<Broker> {
    let _ = std::fs::remove_file(socket);
    if let Some(d) = socket.parent() {
        crate::privfs::dir(d)?;
    }
    let listener = UnixListener::bind(socket)
        .with_context(|| format!("cannot listen on {}", socket.display()))?;
    let issued = Arc::new(AtomicUsize::new(0));
    let recent = Arc::new(Mutex::new(VecDeque::new()));
    let count = issued.clone();
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let (count, recent) = (count.clone(), recent.clone());
            std::thread::spawn(move || {
                if let Err(e) = handle(conn, &count, &recent) {
                    log::debug!("pty broker: {e:#}");
                }
            });
        }
    });
    Ok(Broker {
        socket: socket.to_path_buf(),
        issued,
    })
}

/// Whether one more pty fits into the limit, and count it.
fn admit(recent: &Mutex<VecDeque<Instant>>, now: Instant) -> bool {
    let mut r = recent.lock().unwrap_or_else(|e| e.into_inner());
    while r.front().is_some_and(|t| now.duration_since(*t) > WINDOW) {
        r.pop_front();
    }
    if r.len() >= BURST {
        return false;
    }
    r.push_back(now);
    true
}

fn handle(conn: UnixStream, count: &AtomicUsize, recent: &Mutex<VecDeque<Instant>>) -> Result<()> {
    conn.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut line = String::new();
    BufReader::new(std::io::Read::take(&conn, 64))
        .read_line(&mut line)
        .context("no request")?;
    if line.trim_end() != "pty" {
        return reply_error(&conn, "unknown request");
    }
    if !admit(recent, Instant::now()) {
        log::warn!(
            "pty broker: the sandbox asked for more than {BURST} ptys in {} seconds, refused",
            WINDOW.as_secs()
        );
        return reply_error(&conn, "too many ptys");
    }
    let (master, slave, name) = open_pty()?;
    send(&conn, format!("ok {name}\n").as_bytes(), &[&master, &slave])?;
    count.fetch_add(1, Ordering::SeqCst);
    log::debug!("pty broker: gave {name} to the sandbox");
    Ok(())
}

fn reply_error(mut conn: &UnixStream, why: &str) -> Result<()> {
    conn.write_all(format!("error {why}\n").as_bytes())?;
    Ok(())
}

/// A new pty: the master, the slave and the slave's path.
pub fn open_pty() -> Result<(OwnedFd, OwnedFd, String)> {
    let (mut m, mut s) = (-1, -1);
    // SAFETY: openpty writes two fds; the other arguments may be null.
    let r = unsafe {
        libc::openpty(
            &mut m,
            &mut s,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if r != 0 {
        return Err(std::io::Error::last_os_error()).context("openpty failed");
    }
    // SAFETY: openpty returned two new fds that nothing else owns.
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(m), OwnedFd::from_raw_fd(s)) };
    for fd in [&master, &slave] {
        // Not into paddock's other child processes.
        // SAFETY: a valid fd.
        unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    let mut buf = [0u8; 128];
    // SAFETY: the buffer is valid for its length.
    let r = unsafe { libc::ttyname_r(slave.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
    if r != 0 {
        return Err(std::io::Error::from_raw_os_error(r)).context("no name for the new pty");
    }
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    let name = String::from_utf8_lossy(&buf[..end]).into_owned();
    Ok((master, slave, name))
}

/// Send `data` and `fds` in one message.
fn send(conn: &UnixStream, data: &[u8], fds: &[&OwnedFd]) -> Result<()> {
    let raw: Vec<libc::c_int> = fds.iter().map(|f| f.as_raw_fd()).collect();
    let bytes = std::mem::size_of_val(raw.as_slice()) as u32;
    // SAFETY: CMSG_SPACE only computes a size.
    let space = unsafe { libc::CMSG_SPACE(bytes) } as usize;
    let mut ctl = vec![0u64; space.div_ceil(8)];
    let mut iov = libc::iovec {
        iov_base: data.as_ptr() as *mut libc::c_void,
        iov_len: data.len(),
    };
    // SAFETY: an all-zero msghdr is valid; the fields are set below.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = ctl.as_mut_ptr().cast();
    msg.msg_controllen = space as _;
    // SAFETY: the control buffer has room for one header and the fds.
    unsafe {
        let c = libc::CMSG_FIRSTHDR(&msg);
        (*c).cmsg_level = libc::SOL_SOCKET;
        (*c).cmsg_type = libc::SCM_RIGHTS;
        (*c).cmsg_len = libc::CMSG_LEN(bytes) as _;
        std::ptr::copy_nonoverlapping(raw.as_ptr(), libc::CMSG_DATA(c).cast(), raw.len());
    }
    // SAFETY: msg points at live buffers.
    let n = unsafe { libc::sendmsg(conn.as_raw_fd(), &msg, 0) };
    if n < 0 {
        return Err(std::io::Error::last_os_error()).context("cannot send the pty");
    }
    Ok(())
}

/// The entitlement that lets a hardened binary read `DYLD_*` variables.
pub const DYLD_KEY: &str = "com.apple.security.cs.allow-dyld-environment-variables";
/// The entitlement that lets a hardened binary load a library that another
/// team signed (paddock's library has an ad-hoc signature).
pub const LIBRARY_VALIDATION_KEY: &str = "com.apple.security.cs.disable-library-validation";

/// Whether `codesign -dv` output names the hardened runtime
/// (`flags=0x10000(runtime)`).
pub fn hardened(codesign_dv: &str) -> bool {
    codesign_dv.lines().any(|l| {
        l.trim_start().starts_with("CodeDirectory")
            && l.split_whitespace()
                .find_map(|w| w.strip_prefix("flags="))
                .is_some_and(|f| f.contains("runtime"))
    })
}

/// The interpreter of a script (`#!/usr/bin/env node` gives
/// `/usr/bin/env`), or None for a binary.
pub fn interpreter(head: &[u8]) -> Option<PathBuf> {
    let rest = head.strip_prefix(b"#!")?;
    let line = rest.split(|b| *b == b'\n').next()?;
    let line = String::from_utf8_lossy(line);
    line.split_whitespace().next().map(PathBuf::from)
}

/// Programs in these folders are protected by macOS (SIP): it drops the
/// `DYLD_*` variables when it starts them.
pub const PROTECTED: &[&str] = &[
    "/usr/bin",
    "/bin",
    "/usr/sbin",
    "/sbin",
    "/usr/libexec",
    "/System",
];

pub fn protected(path: &Path) -> bool {
    PROTECTED
        .iter()
        .any(|d| path.starts_with(d) && !path.starts_with("/usr/local"))
}

/// Why `bin` ignores paddock's library, or None when it takes it. `head` is
/// the start of the file, `codesign_dv` the output of `codesign -dv` and
/// `entitlements` the XML of `codesign -d --entitlements :-`.
pub fn dyld_refusal(
    bin: &Path,
    head: &[u8],
    codesign_dv: &str,
    entitlements: &str,
) -> Option<String> {
    let b = bin.display();
    if let Some(i) = interpreter(head) {
        if protected(&i) {
            return Some(format!(
                "{b} is a script for {}, which macOS protects (SIP): it drops DYLD_INSERT_LIBRARIES",
                i.display()
            ));
        }
        return None;
    }
    if protected(bin) {
        return Some(format!(
            "{b} is protected by macOS (SIP): it ignores DYLD_INSERT_LIBRARIES"
        ));
    }
    if !hardened(codesign_dv) {
        return None;
    }
    let ent = |key: &str| {
        crate::backend::seatbelt::plist_xml(entitlements.as_bytes())
            .and_then(|x| crate::backend::seatbelt::plist_bool(&x, key))
            .unwrap_or(false)
    };
    if !ent(DYLD_KEY) {
        return Some(format!(
            "{b} has the hardened runtime without {DYLD_KEY}, so it ignores DYLD_INSERT_LIBRARIES"
        ));
    }
    if !ent(LIBRARY_VALIDATION_KEY) {
        return Some(format!(
            "{b} has the hardened runtime without {LIBRARY_VALIDATION_KEY}, so it loads only libraries of its own team"
        ));
    }
    None
}

/// The interpreter of a script is the program that loads the library.
/// Check it too when it is not protected.
#[cfg(target_os = "macos")]
pub fn check(bin: &Path) -> Option<String> {
    let bin = std::fs::canonicalize(bin).unwrap_or_else(|_| bin.to_path_buf());
    let mut head = vec![0u8; 512];
    let n = std::fs::File::open(&bin)
        .and_then(|mut f| std::io::Read::read(&mut f, &mut head))
        .unwrap_or(0);
    head.truncate(n);
    let dv = std::process::Command::new("/usr/bin/codesign")
        .args(["-d", "--verbose=1"])
        .arg(&bin)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stderr).into_owned())
        .unwrap_or_default();
    let ent = crate::backend::seatbelt::entitlements(&bin).unwrap_or_default();
    if let Some(why) = dyld_refusal(&bin, &head, &dv, &ent) {
        return Some(why);
    }
    match interpreter(&head) {
        Some(i) if i != bin => check(&i),
        _ => None,
    }
}

#[cfg(test)]
#[path = "ptybroker_tests.rs"]
mod tests;
