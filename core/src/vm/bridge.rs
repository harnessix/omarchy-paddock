//! The vsock bridge of `paddock-vm`. It copies bytes between a guest vsock
//! connection and a host unix socket. The code only sees file descriptors,
//! so the tests run on every system with socket pairs as the guest.
//!
//! The control socket speaks one line per connection:
//! `connect PORT` connects to a guest port and answers `ok`, then the
//! connection carries the bytes of that port. `stop` asks the guest to stop.
//! `ping` answers `ok`. An error answers `error TEXT` and closes.

use super::ports;
use anyhow::{bail, Context, Result};
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::Arc;

/// The longest request line, without the newline.
const LINE_MAX: usize = 64;

/// A request on the control socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    Connect(u32),
    Stop,
    Ping,
}

pub fn parse_request(line: &str) -> Result<Request> {
    let mut words = line.split(' ');
    let req = match (words.next(), words.next(), words.next()) {
        (Some("connect"), Some(port), None) => match ports::parse(port) {
            Some(p) => Request::Connect(p),
            None => bail!("unknown port {port}"),
        },
        (Some("stop"), None, None) => Request::Stop,
        (Some("ping"), None, None) => Request::Ping,
        _ => bail!("unknown request"),
    };
    Ok(req)
}

/// The VM side of the control socket.
pub trait Guest: Send + Sync + 'static {
    /// Connect to a port in the guest.
    fn connect(&self, port: u32) -> Result<OwnedFd>;
    /// Ask the guest to stop.
    fn stop(&self);
}

/// Copy bytes in both directions until both sides close.
pub fn splice(a: OwnedFd, b: OwnedFd) -> std::io::Result<()> {
    let a_read = File::from(a);
    let b_read = File::from(b);
    let a_write = a_read.try_clone()?;
    let b_write = b_read.try_clone()?;
    let back = std::thread::spawn(move || pump(b_read, a_write));
    pump(a_read, b_write);
    let _ = back.join();
    Ok(())
}

fn pump(mut from: File, mut to: File) {
    let _ = std::io::copy(&mut from, &mut to);
    // Pass the end of the data on. A file that is not a socket has no
    // shutdown, and the error does not matter.
    unsafe { libc::shutdown(to.as_raw_fd(), libc::SHUT_WR) };
}

/// Send a guest connection to the unix socket `path`, in a thread.
///
/// When `path` is a folder, the folder routes the connections of several
/// runs: the guest first sends the token of its run on one line, and the
/// connection goes to `path/TOKEN.sock`. paddock puts a link to the socket
/// of each run there. A wrong or unknown token closes the connection.
pub fn forward(guest: OwnedFd, path: &Path) {
    let path = path.to_path_buf();
    std::thread::spawn(move || {
        let target = if path.is_dir() {
            match route(&guest, &path) {
                Ok(t) => t,
                Err(e) => {
                    log::warn!("paddock-vm: guest connection to {}: {e:#}", path.display());
                    return;
                }
            }
        } else {
            path
        };
        match UnixStream::connect(&target) {
            Ok(host) => {
                let _ = splice(guest, host.into());
            }
            Err(e) => log::warn!("paddock-vm: cannot connect to {}: {e}", target.display()),
        }
    });
}

/// The socket of a routed guest connection, from the token line.
pub fn route(guest: &OwnedFd, dir: &Path) -> Result<std::path::PathBuf> {
    let mut f = File::from(guest.try_clone()?);
    let token = super::proto::read_raw_line(&mut f, super::proto::TOKEN_LEN + 1)?
        .context("the guest closed before it named its run")?;
    if !super::proto::valid_token(&token) {
        bail!("the guest sent no valid run token");
    }
    let sock = dir.join(format!("{token}.sock"));
    if std::fs::symlink_metadata(&sock).is_err() {
        bail!("no run has this token");
    }
    crate::sockpath::check(&sock)?;
    Ok(sock)
}

/// Bind the control socket with mode 0600. A stale socket goes first.
pub fn bind_control(path: &Path) -> Result<UnixListener> {
    if let Ok(m) = std::fs::symlink_metadata(path) {
        use std::os::unix::fs::FileTypeExt;
        if !m.file_type().is_socket() {
            bail!("{} exists and is not a socket", path.display());
        }
        std::fs::remove_file(path).with_context(|| format!("cannot remove {}", path.display()))?;
    }
    let listener =
        UnixListener::bind(path).with_context(|| format!("cannot bind {}", path.display()))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// Serve the control socket in a thread, one thread per connection.
pub fn serve_control(listener: UnixListener, guest: Arc<dyn Guest>) {
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(conn) = conn else { continue };
            let guest = guest.clone();
            std::thread::spawn(move || {
                if let Err(e) = handle(conn, guest.as_ref()) {
                    log::debug!("paddock-vm: control connection: {e:#}");
                }
            });
        }
    });
}

fn handle(mut conn: UnixStream, guest: &dyn Guest) -> Result<()> {
    let req = match read_line(&mut conn).and_then(|line| parse_request(&line)) {
        Ok(r) => r,
        Err(e) => {
            writeln!(conn, "error {e}")?;
            return Ok(());
        }
    };
    match req {
        Request::Ping => writeln!(conn, "ok")?,
        Request::Stop => {
            writeln!(conn, "ok")?;
            guest.stop();
        }
        Request::Connect(port) => match guest.connect(port) {
            Ok(fd) => {
                writeln!(conn, "ok")?;
                splice(conn.into(), fd)?;
            }
            Err(e) => writeln!(conn, "error {}", one_line(&format!("{e:#}")))?,
        },
    }
    Ok(())
}

/// Read one line byte by byte, so no byte after the newline is lost.
fn read_line(conn: &mut UnixStream) -> Result<String> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if conn.read(&mut byte)? == 0 {
            bail!("the connection closed before the request ended");
        }
        if byte[0] == b'\n' {
            break;
        }
        if line.len() == LINE_MAX {
            bail!("the request is too long");
        }
        line.push(byte[0]);
    }
    String::from_utf8(line).context("the request is not UTF-8")
}

fn one_line(text: &str) -> String {
    text.replace(['\n', '\r'], " ")
}

#[cfg(test)]
#[path = "bridge_tests.rs"]
mod tests;
