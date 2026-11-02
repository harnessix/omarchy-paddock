//! Shared ports. A member of a pasture puts a local TCP port on the ground.
//! The other members reach it on their own `127.0.0.1`.
//!
//! Each member keeps its own network namespace. Unix sockets cross the
//! mounts. Each member's sockets live in a host folder that the others
//! mount read-only at `/run/paddock/ports/<id>/<port>.sock`. The owner
//! binds the same files through its own read-write mount at
//! `/run/paddock/own-ports/<port>.sock`. A member cannot replace another
//! member's socket. The shared pasture folder stays writable and holds
//! no port sockets.
//!
//! The record of a shared port is the lease `lease:port:PORT` in the
//! `pasture` scope of paddock kv. The host sets its owner, decides
//! conflicts, and checks the port range and the limit per pasture. The
//! helper of the owner renews the lease, so the port frees itself when the
//! owner ends. A helper (`paddock-host pasture serve`) runs in every member.
//! paddock starts it with the sandbox. It watches `lease:port:` and starts
//! or stops its listeners. It notes what it does in [`log_path`].

use crate::kv::{Request, Response, Scope, Status, LEASE_PREFIX, PROTOCOL};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The most ports that one pasture shares at a time.
pub const MAX_PORTS: usize = 16;
/// The lowest port that a member can share.
pub const MIN_PORT: u16 = 1024;
/// The lease name of a port is `port:PORT`, its key `lease:port:PORT`.
pub const LEASE_NAME: &str = "port:";
/// The key of the member record that each helper keeps.
pub const MEMBER_PREFIX: &str = "member:";
/// Seconds that a lease or a member record lives without a renewal.
pub const TTL: u64 = 30;
/// How often the helper renews, and the longest wait between two checks.
pub const TICK: Duration = Duration::from_secs(10);

/// The lease key prefix of shared ports.
pub fn lease_prefix() -> String {
    format!("{LEASE_PREFIX}{LEASE_NAME}")
}

/// A port that a member can share: a number from 1024 to 65535.
pub fn check_port(text: &str) -> Result<u16, String> {
    let bad = || format!("'{text}' is no port to share: use a TCP port from {MIN_PORT} to 65535");
    let p: u16 = text.parse().map_err(|_| bad())?;
    if p < MIN_PORT || text.starts_with('0') {
        return Err(bad());
    }
    Ok(p)
}

/// The port of a lease name (`port:3000`), when it names a valid port.
pub fn port_of_name(name: &str) -> Option<u16> {
    check_port(name.strip_prefix(LEASE_NAME)?).ok()
}

/// The port of a lease key (`lease:port:3000`).
pub fn port_of_key(key: &str) -> Option<u16> {
    port_of_name(key.strip_prefix(LEASE_PREFIX)?)
}

/// The host rule for `claim port:PORT`: a valid port, and at most
/// [`MAX_PORTS`] per pasture. `taken` is the number of shared ports now,
/// `renew` whether the caller holds this lease already.
pub fn check_claim(name: &str, taken: usize, renew: bool) -> Result<(), (Status, String)> {
    if port_of_name(name).is_none() {
        return Err((
            Status::Invalid,
            format!(
                "'{name}' names no port: use port:PORT with a TCP port from {MIN_PORT} to 65535"
            ),
        ));
    }
    if !renew && taken >= MAX_PORTS {
        return Err((
            Status::TooLarge,
            format!("the pasture shares {MAX_PORTS} ports already; unshare one first"),
        ));
    }
    Ok(())
}

/// One shared port: who holds it, and since when.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Shared {
    pub port: u16,
    pub sandbox: String,
    pub harness: String,
    /// The time of the last renewal (seconds since the epoch).
    pub updated: u64,
}

/// What a member does for one port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// This member shares it: listen on the socket, connect to 127.0.0.1.
    Serve,
    /// Another member shares it: listen on 127.0.0.1, connect to the socket.
    Reach,
}

/// The role of this member for each shared port, at most [`MAX_PORTS`].
pub fn plan(me: &str, shared: &[Shared]) -> BTreeMap<u16, Role> {
    let mut out = BTreeMap::new();
    for s in shared {
        if out.len() >= MAX_PORTS {
            break;
        }
        let role = if s.sandbox == me {
            Role::Serve
        } else {
            Role::Reach
        };
        out.insert(s.port, role);
    }
    out
}

/// Where every member sees the port sockets. Read-only in the sandbox.
pub const PORTS_MOUNT: &str = "/run/paddock/ports";
/// Where this member creates its own sockets. Read-write in the sandbox.
pub const OWN_MOUNT: &str = "/run/paddock/own-ports";
/// The variable that names the read-only socket tree inside a sandbox.
/// srt cannot bind-mount, so there it is the host folder.
pub const PORTS_ENV: &str = "PADDOCK_PORTS";
/// The variable that names this member's own socket folder.
pub const OWN_ENV: &str = "PADDOCK_OWN_PORTS";

/// The host folder of every member's port sockets, next to the pasture folder.
pub fn host_dir(env: &crate::env::HostEnv, project: &Path, name: &str) -> PathBuf {
    super::folder(env, project, name).join("ports")
}

/// One member's socket folder on the host. `id` has to be a sandbox id.
pub fn member_dir(ports: &Path, id: &str) -> anyhow::Result<PathBuf> {
    crate::kv::shard::check_id(id)?;
    Ok(ports.join(id))
}

/// The read-only socket tree in this sandbox.
pub fn ports_dir() -> PathBuf {
    std::env::var_os(PORTS_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(PORTS_MOUNT))
}

/// This member's own socket folder in this sandbox.
pub fn own_dir() -> PathBuf {
    std::env::var_os(OWN_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(OWN_MOUNT))
}

/// The socket the owner binds for `port`.
pub fn serve_socket(own: &Path, port: u16) -> PathBuf {
    own.join(format!("{port}.sock"))
}

/// The socket a reacher opens for `owner`'s `port`.
/// None when `owner` is not a sandbox id, so a lease cannot name another path.
pub fn reach_socket(ports: &Path, owner: &str, port: u16) -> Option<PathBuf> {
    crate::kv::shard::check_id(owner).ok()?;
    Some(ports.join(owner).join(format!("{port}.sock")))
}

// ---- the relay -------------------------------------------------------

/// A stream that the relay copies: TCP or Unix.
pub trait Stream: Read + Write + Send + Sized + 'static {
    fn split(&self) -> std::io::Result<Self>;
    fn shut(&self, how: Shutdown);
}

impl Stream for TcpStream {
    fn split(&self) -> std::io::Result<Self> {
        self.try_clone()
    }
    fn shut(&self, how: Shutdown) {
        let _ = self.shutdown(how);
    }
}

impl Stream for UnixStream {
    fn split(&self) -> std::io::Result<Self> {
        self.try_clone()
    }
    fn shut(&self, how: Shutdown) {
        let _ = self.shutdown(how);
    }
}

/// Copy `from` to `to` until the end. At the end, close the write side of
/// `to`, so the other side sees the end too, and the other direction goes
/// on. After an error, close both streams.
fn pump<A: Stream, B: Stream>(mut from: A, mut to: B) {
    let mut buf = [0u8; 16 * 1024];
    loop {
        match from.read(&mut buf) {
            Ok(0) => {
                to.shut(Shutdown::Write);
                return;
            }
            Ok(n) => {
                if to.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    from.shut(Shutdown::Both);
    to.shut(Shutdown::Both);
}

/// Copy both directions between `a` and `b` until both ends close.
pub fn splice<A: Stream, B: Stream>(a: A, b: B) {
    let (Ok(a2), Ok(b2)) = (a.split(), b.split()) else {
        return;
    };
    let t = std::thread::spawn(move || pump(a2, b2));
    pump(b, a);
    let _ = t.join();
}

// ---- listeners -------------------------------------------------------

/// The work for one accepted connection.
type Job = Box<dyn FnOnce() + Send>;

/// A listener that runs in its own thread until [`Listener::stop`].
pub struct Listener {
    stop: Arc<AtomicBool>,
    fd: i32,
    thread: Option<std::thread::JoinHandle<()>>,
    /// The socket file and its inode, for a socket listener.
    file: Option<(PathBuf, u64)>,
}

impl Listener {
    fn run<L, F>(l: L, fd: i32, file: Option<(PathBuf, u64)>, accept: F) -> Self
    where
        L: Send + 'static,
        F: Fn(&L) -> std::io::Result<Job> + Send + 'static,
    {
        let stop = Arc::new(AtomicBool::new(false));
        let s2 = stop.clone();
        let thread = std::thread::spawn(move || {
            while !s2.load(Ordering::SeqCst) {
                match accept(&l) {
                    Ok(job) => {
                        std::thread::spawn(job);
                    }
                    Err(_) if s2.load(Ordering::SeqCst) => break,
                    Err(_) => std::thread::sleep(Duration::from_millis(100)),
                }
            }
        });
        Listener {
            stop,
            fd,
            thread: Some(thread),
            file,
        }
    }

    /// The owner side: accept on the socket at `path`, connect each stream
    /// to `127.0.0.1:port`. A stale socket file from an earlier owner goes.
    pub fn serve(path: &Path, port: u16) -> std::io::Result<Self> {
        let _ = std::fs::remove_file(path);
        let l = UnixListener::bind(path)?;
        let ino = std::fs::symlink_metadata(path)?.ino();
        let fd = l.as_raw_fd();
        Ok(Self::run(
            l,
            fd,
            Some((path.to_path_buf(), ino)),
            move |l: &UnixListener| {
                let (c, _) = l.accept()?;
                Ok(Box::new(move || {
                    if let Ok(t) = TcpStream::connect(("127.0.0.1", port)) {
                        splice(c, t);
                    }
                }) as Job)
            },
        ))
    }

    /// The other side: accept on `127.0.0.1:port`, connect each stream to
    /// the socket at `path`.
    pub fn reach(port: u16, path: &Path) -> std::io::Result<Self> {
        let l = TcpListener::bind(("127.0.0.1", port))?;
        let fd = l.as_raw_fd();
        let path = path.to_path_buf();
        Ok(Self::run(l, fd, None, move |l: &TcpListener| {
            let (c, _) = l.accept()?;
            let path = path.clone();
            Ok(Box::new(move || {
                if let Ok(u) = UnixStream::connect(&path) {
                    splice(c, u);
                }
            }) as Job)
        }))
    }

    /// Stop accepting. Open connections go on until they close. A socket
    /// file goes, unless another member bound a new one there.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wakes the blocked accept on Linux; the thread then drops the
        // listener.
        unsafe { libc::shutdown(self.fd, libc::SHUT_RDWR) };
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        if let Some((path, ino)) = &self.file {
            if std::fs::symlink_metadata(path).is_ok_and(|m| m.ino() == *ino) {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

// ---- the helper in each member -----------------------------------------

fn kv(op: &str, f: impl FnOnce(&mut Request)) -> Result<Response, String> {
    let mut r = Request {
        v: PROTOCOL,
        op: op.into(),
        scope: Some(Scope::Pasture),
        ..Default::default()
    };
    f(&mut r);
    crate::hostcli::kv::send(&r)
}

/// This sandbox in paddock kv: its id, harness and pasture name.
pub fn whoami() -> Result<(String, String, Option<String>), String> {
    let r = kv("info", |r| r.scope = None)?;
    if r.status != Status::Done {
        return Err(r.message);
    }
    let s = |k: &str| r.result[k].as_str().map(str::to_string);
    Ok((
        s("sandbox").unwrap_or_default(),
        s("harness").unwrap_or_default(),
        s("pasture"),
    ))
}

/// The shared ports of the pasture, by port.
pub fn shared() -> Result<Vec<Shared>, String> {
    let r = kv("list", |r| {
        r.prefix = Some(lease_prefix());
        r.limit = Some(1000);
    })?;
    if r.status != Status::Done {
        return Err(r.message);
    }
    let mut out: Vec<Shared> = r.result["entries"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| {
            Some(Shared {
                port: port_of_key(e["key"].as_str()?)?,
                sandbox: e["writer"].as_str()?.to_string(),
                harness: e["writer_harness"].as_str().unwrap_or("").to_string(),
                updated: e["updated"].as_u64().unwrap_or(0),
            })
        })
        .collect();
    out.sort_by_key(|s| s.port);
    Ok(out)
}

/// Take or renew the lease of a port. Err: the status and message.
pub fn claim(port: u16) -> Result<Response, String> {
    kv("claim", |r| {
        r.key = Some(format!("{LEASE_NAME}{port}"));
        r.ttl = Some(TTL);
    })
}

pub fn release(port: u16) -> Result<Response, String> {
    kv("release", |r| r.key = Some(format!("{LEASE_NAME}{port}")))
}

/// The lock that keeps one helper per member.
fn lock_path(own: &Path, me: &str) -> PathBuf {
    own.join(format!(".serve-{me}.lock"))
}

/// Take the helper lock without waiting. None: a helper runs already.
pub fn try_lock(own: &Path, me: &str) -> Option<std::fs::File> {
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(lock_path(own, me))
        .ok()?;
    let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    (rc == 0).then_some(f)
}

/// Make this member's socket folder. Mode 0700, like the pasture folder.
pub fn make_dir(own: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    if !own.is_dir() {
        std::fs::DirBuilder::new()
            .mode(0o700)
            .recursive(true)
            .create(own)?;
    }
    Ok(())
}

/// The log of the helper: the private temporary folder of the sandbox.
pub fn log_path() -> PathBuf {
    std::env::temp_dir().join("paddock-pasture-serve.log")
}

/// Append one line to the helper log. No log when the file cannot open.
fn note(msg: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path())
    {
        let _ = writeln!(f, "{} {msg}", crate::state::now());
    }
}

/// The helper of one member: run until paddock kv goes away.
pub fn serve() -> i32 {
    let (me, harness, pasture) = match whoami() {
        Ok(w) => w,
        Err(e) => {
            note(&format!("no paddock kv, the helper ends: {e}"));
            eprintln!("paddock-host pasture serve: {e}");
            return crate::hostcli::EXIT_NO_HOST;
        }
    };
    if pasture.is_none() {
        eprintln!("paddock-host pasture serve: this sandbox is in no pasture");
        return 1;
    }
    let own = own_dir();
    let ports = ports_dir();
    if let Err(e) = make_dir(&own) {
        eprintln!("paddock-host pasture serve: {}: {e}", own.display());
        return 1;
    }
    let Some(_lock) = try_lock(&own, &me) else {
        return 0;
    };
    note(&format!(
        "start: sandbox {me}, pasture {}",
        pasture.unwrap_or_default()
    ));
    // The bridge: questions of other members, on the own kv socket.
    std::thread::spawn(super::bridge::helper);
    let mut active: BTreeMap<u16, (Role, Listener)> = BTreeMap::new();
    let mut skipped: BTreeMap<u16, String> = BTreeMap::new();
    let mut renewed: BTreeMap<u16, Instant> = BTreeMap::new();
    let mut last_record: Option<(Instant, String)> = None;
    // From 0: the first watch returns at once with the cursor of now, so
    // no change between a list and the next watch is lost.
    let mut cursor: Option<u64> = Some(0);
    let mut fails = 0;
    loop {
        let list = match shared() {
            Ok(l) => {
                fails = 0;
                l
            }
            Err(e) => {
                note(&format!("cannot list the shared ports: {e}"));
                fails += 1;
                if fails >= 6 {
                    break;
                }
                std::thread::sleep(Duration::from_secs(2));
                continue;
            }
        };
        let want = plan(&me, &list);
        // Stop what changed or went.
        let gone: Vec<u16> = active
            .iter()
            .filter(|(p, (role, _))| want.get(p) != Some(role))
            .map(|(p, _)| *p)
            .collect();
        for p in gone {
            if let Some((_, l)) = active.remove(&p) {
                note(&format!("stop {p}"));
                l.stop();
            }
        }
        skipped.retain(|p, _| want.get(p) == Some(&Role::Reach));
        renewed.retain(|p, _| want.get(p) == Some(&Role::Serve));
        for (&port, &role) in &want {
            if role == Role::Serve
                && renewed
                    .get(&port)
                    .is_none_or(|t| t.elapsed() >= TICK - Duration::from_secs(1))
            {
                // The lease of a port that this member shares. When the
                // claim fails, the next round sees the new owner.
                if claim(port).is_ok_and(|r| r.status == Status::Done) {
                    renewed.insert(port, Instant::now());
                }
            }
            if active.contains_key(&port) {
                continue;
            }
            let path = match role {
                Role::Serve => Some(serve_socket(&own, port)),
                Role::Reach => list
                    .iter()
                    .rev()
                    .find(|s| s.port == port)
                    .and_then(|s| reach_socket(&ports, &s.sandbox, port)),
            };
            let Some(path) = path else {
                let why = "the owner is not a sandbox id".to_string();
                note(&format!("skip {port}: {why}"));
                skipped.insert(port, why);
                continue;
            };
            let started = match role {
                Role::Serve => Listener::serve(&path, port),
                Role::Reach => Listener::reach(port, &path),
            };
            match started {
                Ok(l) => {
                    note(&format!("{role:?} {port}"));
                    skipped.remove(&port);
                    active.insert(port, (role, l));
                }
                Err(e) => {
                    let why = if e.kind() == std::io::ErrorKind::AddrInUse {
                        "something in this sandbox listens on it already".to_string()
                    } else {
                        e.to_string()
                    };
                    note(&format!("skip {port}: {why}"));
                    skipped.insert(port, why);
                }
            }
        }
        // The member record: harness and skipped ports, with a ttl.
        let record = serde_json::json!({ "harness": harness, "skipped": skipped }).to_string();
        let due = last_record
            .as_ref()
            .is_none_or(|(t, v)| *v != record || t.elapsed() >= TICK - Duration::from_secs(1));
        if due {
            let r = kv("put", |r| {
                r.key = Some(format!("{MEMBER_PREFIX}{me}"));
                r.value = Some(record.clone());
                r.ttl = Some(TTL);
            });
            if r.is_ok_and(|r| r.status == Status::Done) {
                last_record = Some((Instant::now(), record));
            }
        }
        // Wait for a change of a port, at most one tick.
        match kv("watch", |r| {
            r.prefix = Some(lease_prefix());
            r.version = cursor;
            r.timeout = Some(TICK.as_secs());
        }) {
            Ok(r) => {
                if let Some(c) = r.result["cursor"].as_u64() {
                    cursor = Some(c);
                }
            }
            Err(e) => {
                note(&format!("watch failed: {e}"));
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }
    note("paddock kv went away, the helper ends");
    for (_, (_, l)) in std::mem::take(&mut active) {
        l.stop();
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ports() {
        for ok in ["1024", "3000", "65535"] {
            assert!(check_port(ok).is_ok(), "{ok}");
        }
        for bad in ["", "80", "1023", "65536", "03000", "x", "-1", "3000/udp"] {
            assert!(check_port(bad).is_err(), "{bad}");
        }
        assert_eq!(port_of_key("lease:port:3000"), Some(3000));
        assert_eq!(port_of_key("lease:port:80"), None);
        assert_eq!(port_of_key("lease:task"), None);
        assert_eq!(port_of_name("port:8080"), Some(8080));
    }

    #[test]
    fn the_claim_rule() {
        assert!(check_claim("port:3000", 0, false).is_ok());
        assert_eq!(
            check_claim("port:22", 0, false).unwrap_err().0,
            Status::Invalid
        );
        assert_eq!(
            check_claim("port:3000", MAX_PORTS, false).unwrap_err().0,
            Status::TooLarge
        );
        // The owner renews even at the limit.
        assert!(check_claim("port:3000", MAX_PORTS, true).is_ok());
    }

    fn s(port: u16, sandbox: &str) -> Shared {
        Shared {
            port,
            sandbox: sandbox.into(),
            harness: "sh".into(),
            updated: 0,
        }
    }

    #[test]
    fn plans_serve_for_own_ports_and_reach_for_others() {
        let p = plan("a", &[s(3000, "a"), s(5173, "b")]);
        assert_eq!(p[&3000], Role::Serve);
        assert_eq!(p[&5173], Role::Reach);
        let many: Vec<Shared> = (0..20).map(|i| s(4000 + i, "b")).collect();
        assert_eq!(plan("a", &many).len(), MAX_PORTS);
    }

    /// An echo server that answers each connection, then closes.
    fn echo() -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for c in l.incoming().flatten() {
                std::thread::spawn(move || {
                    let mut c = c;
                    let mut buf = Vec::new();
                    // Read to the end, then answer: shows that the end of
                    // one direction passes through the relay.
                    c.read_to_end(&mut buf).unwrap();
                    c.write_all(b"echo:").unwrap();
                    c.write_all(&buf).unwrap();
                });
            }
        });
        port
    }

    #[test]
    fn the_relay_copies_both_ways_and_passes_the_end() {
        let port = echo();
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("p.sock");
        let a = Listener::serve(&sock, port).unwrap();
        // The far side: a free port on 127.0.0.1 that reaches the socket.
        let free = TcpListener::bind("127.0.0.1:0").unwrap();
        let near = free.local_addr().unwrap().port();
        drop(free);
        let b = Listener::reach(near, &sock).unwrap();

        let big: Vec<u8> = (0..200_000u32).map(|i| i as u8).collect();
        let mut c = TcpStream::connect(("127.0.0.1", near)).unwrap();
        let w = c.try_clone().unwrap();
        let data = big.clone();
        let t = std::thread::spawn(move || {
            let mut w = w;
            w.write_all(&data).unwrap();
            w.shutdown(Shutdown::Write).unwrap();
        });
        let mut got = Vec::new();
        c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        c.read_to_end(&mut got).unwrap();
        t.join().unwrap();
        assert_eq!(&got[..5], b"echo:");
        assert_eq!(&got[5..], &big[..]);

        // Stopped: the socket file goes, and the port takes no connection.
        a.stop();
        assert!(!sock.exists());
        b.stop();
        assert!(TcpStream::connect(("127.0.0.1", near)).is_err());
    }

    #[test]
    fn a_closed_target_closes_the_client() {
        // Nothing listens on the target port: the client sees the end.
        let free = TcpListener::bind("127.0.0.1:0").unwrap();
        let dead = free.local_addr().unwrap().port();
        drop(free);
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("p.sock");
        let a = Listener::serve(&sock, dead).unwrap();
        let mut u = UnixStream::connect(&sock).unwrap();
        u.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut buf = [0u8; 8];
        assert_eq!(u.read(&mut buf).unwrap(), 0);
        a.stop();
    }

    #[test]
    fn a_stop_keeps_the_socket_of_a_new_owner() {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("p.sock");
        let old = Listener::serve(&sock, 1).unwrap();
        // A new owner binds the same path.
        let new = Listener::serve(&sock, 1).unwrap();
        old.stop();
        assert!(sock.exists());
        new.stop();
        assert!(!sock.exists());
    }

    /// A reacher opens the lease owner's socket. A socket in the shared
    /// pasture folder, or in another member's folder, is not that socket.
    #[test]
    fn a_reacher_uses_the_owner_socket_not_a_planted_one() {
        let tmp = tempfile::tempdir().unwrap();
        let ground = tmp.path().join("ground");
        let ports = tmp.path().join("ports");
        let own_a = ports.join("sandbox-a");
        std::fs::create_dir_all(&own_a).unwrap();
        std::fs::create_dir_all(ground.join(".ports")).unwrap();
        std::fs::create_dir_all(ports.join("sandbox-b")).unwrap();

        let owner = serve_socket(&own_a, 3000);
        let reach = reach_socket(&ports, "sandbox-a", 3000).unwrap();
        assert_eq!(owner, reach);
        assert_ne!(reach, ground.join(".ports").join("3000.sock"));
        assert_ne!(reach, serve_socket(&ports.join("sandbox-b"), 3000));
        assert!(reach_socket(&ports, "../sandbox-a", 3000).is_none());
        assert!(reach_socket(&ports, "sandbox-a/../b", 3000).is_none());

        fn plant(path: &Path) {
            let _ = std::fs::remove_file(path);
            let l = UnixListener::bind(path).unwrap();
            std::thread::spawn(move || {
                for c in l.incoming().flatten() {
                    let mut c = c;
                    let _ = c.write_all(b"stolen");
                    let _ = c.shutdown(Shutdown::Both);
                }
            });
        }
        plant(&ground.join(".ports").join("3000.sock"));
        plant(&serve_socket(&ports.join("sandbox-b"), 3000));

        let target = echo();
        let _kept = Listener::serve(&owner, target).unwrap();
        let free = TcpListener::bind("127.0.0.1:0").unwrap();
        let near = free.local_addr().unwrap().port();
        drop(free);
        let relay = Listener::reach(near, &reach).unwrap();
        let mut c = TcpStream::connect(("127.0.0.1", near)).unwrap();
        c.write_all(b"hi").unwrap();
        c.shutdown(Shutdown::Write).unwrap();
        let mut got = Vec::new();
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        c.read_to_end(&mut got).unwrap();
        assert_eq!(&got, b"echo:hi");
        relay.stop();
    }

    #[test]
    fn the_lock_keeps_one_helper() {
        let tmp = tempfile::tempdir().unwrap();
        make_dir(tmp.path()).unwrap();
        let first = try_lock(tmp.path(), "a1").unwrap();
        assert!(try_lock(tmp.path(), "a1").is_none());
        assert!(try_lock(tmp.path(), "b1").is_some());
        drop(first);
        assert!(try_lock(tmp.path(), "a1").is_some());
    }
}
