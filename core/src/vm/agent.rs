//! The guest agent of the VM backend: `paddock vm-agent`. The init of the
//! guest (packaging/vm/agent-init) starts it as root. It listens on the
//! control port and the pty port (ports.rs), and starts one run per
//! request: Linux paddock with bwrap, as the unprivileged guest user. The
//! messages are in proto.rs.
//!
//! The transport is vsock. Tests use unix sockets in a folder instead:
//! `PORT.sock` for each port.

use super::{ports, proto};
use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::{Arc, Mutex};

/// The device of the writable guest disk.
pub const STATE_DEVICE: &str = "/dev/vdb";
/// Where the writable disk goes when the image has the folder.
pub const STATE_MOUNT: &str = "/var/lib/paddock";
/// Where it goes in an image without that folder: /run is a tmpfs.
pub const STATE_MOUNT_RUN: &str = "/run/paddock-state";
/// The folders of the runs: the guest ends of the broker and proxy sockets.
pub const RUN_DIR: &str = "/run/paddock-vm";
/// The variables that tell the paddock in the guest to use the host broker
/// and the host proxy (run.rs, login.rs).
pub const HOST_SOCKET_VAR: &str = "PADDOCK_VM_HOST_SOCKET";
pub const PROXY_SOCKET_VAR: &str = "PADDOCK_VM_PROXY_SOCKET";

/// The guest end of a host socket that the agent made for this run, from
/// the variable `var`. Only in the Linux guest, and only under the run
/// folder of the agent.
pub fn guest_socket(var: &str) -> Option<PathBuf> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let p = PathBuf::from(std::env::var_os(var)?);
    use std::os::unix::fs::FileTypeExt;
    let ok = p.is_absolute()
        && p.starts_with(RUN_DIR)
        && !p.components().any(|c| c == std::path::Component::ParentDir)
        && std::fs::metadata(&p).is_ok_and(|m| m.file_type().is_socket());
    ok.then_some(p)
}

/// The api proxy of a run: in the guest of the VM backend, the host proxy,
/// which applies the network mode of the host run. Elsewhere paddock
/// serves `sock` with `cfg`.
pub fn api_proxy(sock: PathBuf, cfg: crate::proxy::HostProxy) -> Result<PathBuf> {
    if let Some(guest) = guest_socket(PROXY_SOCKET_VAR) {
        return Ok(guest);
    }
    crate::proxy::serve(&sock, cfg)?;
    Ok(sock)
}

/// How the agent reaches the host.
#[derive(Debug, Clone)]
pub enum Transport {
    Vsock,
    /// Tests: `DIR/PORT.sock`. The agent listens on the guest ports and
    /// connects to the host ports there.
    Unix(PathBuf),
}

/// A listening socket of the agent.
pub enum Listener {
    Unix(UnixListener),
    Vsock(OwnedFd),
}

impl Transport {
    pub fn listen(&self, port: u32) -> Result<Listener> {
        match self {
            Transport::Unix(dir) => {
                let path = dir.join(format!("{port}.sock"));
                let _ = std::fs::remove_file(&path);
                Ok(Listener::Unix(UnixListener::bind(&path).with_context(
                    || format!("cannot bind {}", path.display()),
                )?))
            }
            Transport::Vsock => Ok(Listener::Vsock(vsock::listen(port)?)),
        }
    }

    /// Connect to a host port.
    pub fn dial(&self, port: u32) -> Result<OwnedFd> {
        match self {
            Transport::Unix(dir) => {
                let path = dir.join(format!("{port}.sock"));
                Ok(UnixStream::connect(&path)
                    .with_context(|| format!("cannot connect to {}", path.display()))?
                    .into())
            }
            Transport::Vsock => vsock::connect(port),
        }
    }
}

impl Listener {
    /// The next connection. A vsock connection that does not come from the
    /// host is closed: only the host starts runs.
    pub fn accept(&self) -> Result<OwnedFd> {
        loop {
            match self {
                Listener::Unix(l) => return Ok(l.accept()?.0.into()),
                Listener::Vsock(fd) => {
                    let (conn, cid) = vsock::accept(fd)?;
                    if vsock::from_host(cid) {
                        return Ok(conn);
                    }
                    say(
                        &Transport::Vsock,
                        &format!("refused a connection from vsock address {cid}"),
                    );
                }
            }
        }
    }
}

#[cfg(target_os = "linux")]
mod vsock {
    use anyhow::{Context, Result};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    fn addr(cid: u32, port: u32) -> libc::sockaddr_vm {
        libc::sockaddr_vm {
            svm_family: libc::AF_VSOCK as libc::sa_family_t,
            svm_reserved1: 0,
            svm_port: port,
            svm_cid: cid,
            svm_zero: [0; 4],
        }
    }

    fn socket() -> Result<OwnedFd> {
        // SAFETY: a new socket fd, owned below.
        let fd = unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error()).context("no vsock in this system");
        }
        // SAFETY: `fd` is new and nothing else owns it.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    const LEN: libc::socklen_t = std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t;

    pub fn listen(port: u32) -> Result<OwnedFd> {
        let fd = socket()?;
        let a = addr(libc::VMADDR_CID_ANY, port);
        // SAFETY: bind and listen on our socket with a valid address.
        unsafe {
            if libc::bind(fd.as_raw_fd(), (&a as *const libc::sockaddr_vm).cast(), LEN) < 0
                || libc::listen(fd.as_raw_fd(), 64) < 0
            {
                return Err(std::io::Error::last_os_error())
                    .with_context(|| format!("cannot listen on vsock port {port}"));
            }
        }
        Ok(fd)
    }

    pub fn accept(l: &OwnedFd) -> Result<(OwnedFd, u32)> {
        let mut a = addr(0, 0);
        let mut len = LEN;
        // SAFETY: accept writes at most `len` bytes of the peer address.
        let fd = unsafe {
            libc::accept4(
                l.as_raw_fd(),
                (&mut a as *mut libc::sockaddr_vm).cast(),
                &mut len,
                libc::SOCK_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error()).context("vsock accept");
        }
        // SAFETY: `fd` is new and nothing else owns it.
        Ok((unsafe { OwnedFd::from_raw_fd(fd) }, a.svm_cid))
    }

    pub fn connect(port: u32) -> Result<OwnedFd> {
        let fd = socket()?;
        let a = addr(libc::VMADDR_CID_HOST, port);
        // SAFETY: connect our socket to a valid address.
        if unsafe { libc::connect(fd.as_raw_fd(), (&a as *const libc::sockaddr_vm).cast(), LEN) }
            < 0
        {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("cannot connect to host vsock port {port}"));
        }
        Ok(fd)
    }

    pub fn from_host(cid: u32) -> bool {
        cid == libc::VMADDR_CID_HOST
    }
}

#[cfg(not(target_os = "linux"))]
mod vsock {
    use anyhow::{bail, Result};
    use std::os::fd::OwnedFd;
    pub fn listen(_port: u32) -> Result<OwnedFd> {
        bail!("the guest agent runs only in the Linux guest")
    }
    pub fn accept(_l: &OwnedFd) -> Result<(OwnedFd, u32)> {
        bail!("the guest agent runs only in the Linux guest")
    }
    pub fn connect(_port: u32) -> Result<OwnedFd> {
        bail!("the guest agent runs only in the Linux guest")
    }
    pub fn from_host(_cid: u32) -> bool {
        false
    }
}

/// The settings of the agent.
#[derive(Debug, Clone)]
pub struct Config {
    pub transport: Transport,
    /// The paddock program that runs in the guest.
    pub paddock: PathBuf,
    /// The guest user of the runs (uid, gid). None: the agent's own user
    /// (tests).
    pub user: Option<(u32, u32)>,
    /// Pick the run user from the mounted project instead of `user`
    /// (agent_user.rs): its owner, or the fixed guest user when virtiofs
    /// shows root and that user can write it.
    pub project_owner: bool,
    /// Mount the project and the state disk. Off in tests.
    pub mounts: bool,
    /// The folder of the run folders.
    pub run_dir: PathBuf,
    /// The home of the guest user when `mounts` is off.
    pub home: Option<PathBuf>,
}

/// The state that the connections share.
pub struct Agent {
    pub cfg: Config,
    /// Runs that wait for their pty connection, by token.
    pending: Mutex<HashMap<String, SyncSender<OwnedFd>>>,
    /// The tokens of the runs that run now.
    live: Mutex<HashMap<String, ()>>,
    /// The project folder that the VM holds, after the first mount.
    project: Mutex<Option<PathBuf>>,
    /// The home of the guest user, after the state disk is ready.
    home: Mutex<Option<PathBuf>>,
}

impl Agent {
    pub fn new(cfg: Config) -> Arc<Self> {
        Arc::new(Self {
            cfg,
            pending: Mutex::new(HashMap::new()),
            live: Mutex::new(HashMap::new()),
            project: Mutex::new(None),
            home: Mutex::new(None),
        })
    }

    /// One line about what the agent does. In the guest it goes to the
    /// console, so the console log of the host shows where a run stops.
    /// The tests (unix transport) log it at debug level.
    pub fn say(&self, what: &str) {
        say(&self.cfg.transport, what);
    }

    pub fn runs(&self) -> usize {
        self.live.lock().unwrap().len()
    }

    /// Wait for the pty connection of a run, at most `wait`.
    pub(crate) fn expect_pty(&self, token: &str) -> std::sync::mpsc::Receiver<OwnedFd> {
        let (tx, rx) = sync_channel(1);
        self.pending.lock().unwrap().insert(token.to_string(), tx);
        rx
    }

    pub(crate) fn forget(&self, token: &str) {
        self.pending.lock().unwrap().remove(token);
        self.live.lock().unwrap().remove(token);
    }

    pub(crate) fn set_live(&self, token: &str) {
        self.live.lock().unwrap().insert(token.to_string(), ());
    }

    pub(crate) fn is_live(&self, token: &str) -> bool {
        self.live.lock().unwrap().contains_key(token)
    }

    /// Mount the project at its host path, once. The VM holds one project.
    pub(crate) fn mount_project(&self, project: &Path) -> Result<()> {
        let mut held = self.project.lock().unwrap();
        match held.as_deref() {
            Some(p) if p == project => return Ok(()),
            Some(p) => bail!(
                "this VM holds the project {}, not {}: stop it with `paddock vm stop` when no run uses it",
                p.display(),
                project.display()
            ),
            None => {}
        }
        if self.cfg.mounts {
            let tag = super::helper::PROJECT_TAG;
            let at = project.display();
            let done = mount_point(project)
                .and_then(|_| run_tool("mount", &["-t", "virtiofs", tag, &at.to_string()]));
            match &done {
                Ok(()) => match super::agent_user::Seen::read(project) {
                    Ok(s) => self.say(&format!("mount {tag} on {at}: ok, {}", s.line())),
                    Err(e) => self.say(&format!("mount {tag} on {at}: ok, {e:#}")),
                },
                Err(e) => self.say(&format!("mount {tag} on {at}: failed: {e:#}")),
            }
            done?;
        }
        *held = Some(project.to_path_buf());
        Ok(())
    }

    /// The user of a run: the configured user, or one that can write the
    /// mounted project (agent_user.rs). `host_owner` is the owner of the
    /// project on the host.
    pub(crate) fn run_user(
        &self,
        project: &Path,
        host_owner: Option<u32>,
    ) -> Result<Option<(u32, u32)>> {
        use super::agent_user::{can_write, choose, Seen, GUEST_USER};
        if !self.cfg.project_owner {
            return Ok(self.cfg.user);
        }
        let seen = Seen::read(project)?;
        let user = choose(project, &seen, host_owner, || {
            let ok = can_write(project, GUEST_USER)?;
            self.say(&format!(
                "write probe as uid {}: {}",
                GUEST_USER.0,
                if ok { "ok" } else { "refused" }
            ));
            Ok(ok)
        })?;
        let host = host_owner.map_or("unknown".into(), |u| u.to_string());
        self.say(&format!(
            "run user {}:{} (project {}, host owner {host})",
            user.0,
            user.1,
            seen.line()
        ));
        Ok(Some(user))
    }

    /// The home of the guest user on the writable disk, owned by `user`.
    /// The disk gets a file system on the first use.
    pub(crate) fn home(&self, user: Option<(u32, u32)>) -> Result<PathBuf> {
        let mut held = self.home.lock().unwrap();
        if let Some(h) = held.as_ref() {
            return Ok(h.clone());
        }
        let home = if self.cfg.mounts {
            let mount = if Path::new(STATE_MOUNT).is_dir() {
                PathBuf::from(STATE_MOUNT)
            } else {
                std::fs::create_dir_all(STATE_MOUNT_RUN)?;
                PathBuf::from(STATE_MOUNT_RUN)
            };
            // busybox blkid succeeds on an empty disk too, so read the ext
            // superblock magic (0xEF53 at byte 1080) instead.
            if !has_ext_magic(Path::new(STATE_DEVICE)) {
                say(&self.cfg.transport, "format the state disk");
                run_tool("mkfs.ext4", &["-q", "-L", "paddock-state", STATE_DEVICE])?;
            }
            run_tool("mount", &[STATE_DEVICE, &mount.display().to_string()])?;
            mount.join("home")
        } else {
            self.cfg
                .home
                .clone()
                .context("the agent has no home for the runs")?
        };
        std::fs::create_dir_all(&home)?;
        if let Some((uid, gid)) = user {
            std::os::unix::fs::chown(&home, Some(uid), Some(gid))?;
        }
        *held = Some(home.clone());
        Ok(home)
    }
}

/// Make the mount point of the project at its host path. The root file
/// system of the guest is read-only, so the first folder of the path
/// (`/Users`, `/private`, `/Volumes` in the image) gets a tmpfs, when it is
/// an empty folder of the image. Other paths must exist or be on a tmpfs.
fn mount_point(project: &Path) -> Result<()> {
    let Err(first) = std::fs::create_dir_all(project) else {
        return Ok(());
    };
    let top = project
        .components()
        .nth(1)
        .map(|c| Path::new("/").join(c))
        .context("the project has no folder")?;
    let empty = std::fs::read_dir(&top).is_ok_and(|mut d| d.next().is_none());
    if !empty || first.raw_os_error() != Some(libc::EROFS) {
        return Err(first).with_context(|| {
            format!(
                "cannot make the mount point {} in the guest",
                project.display()
            )
        });
    }
    run_tool(
        "mount",
        &[
            "-t",
            "tmpfs",
            "-o",
            "nosuid,nodev,mode=0755",
            "tmpfs",
            &top.display().to_string(),
        ],
    )?;
    std::fs::create_dir_all(project).with_context(|| format!("cannot create {}", project.display()))
}

fn run_tool(prog: &str, args: &[&str]) -> Result<()> {
    let out = std::process::Command::new(prog)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .with_context(|| format!("cannot start {prog}"))?;
    if !out.status.success() {
        bail!(
            "{prog} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// A line on the console of the guest, or a debug line in tests.
pub fn say(transport: &Transport, what: &str) {
    match transport {
        Transport::Vsock => {
            let line = format!("paddock vm-agent: {what}\n");
            let console = std::fs::OpenOptions::new()
                .append(true)
                .open("/dev/console");
            let written =
                console.and_then(|mut c| std::io::Write::write_all(&mut c, line.as_bytes()));
            if written.is_err() {
                eprint!("{line}");
            }
        }
        Transport::Unix(_) => log::debug!("vm-agent: {what}"),
    }
}

/// Serve until the control listener fails.
pub fn serve(agent: Arc<Agent>) -> Result<()> {
    let listen = |port| {
        agent.cfg.transport.listen(port).inspect_err(|e| {
            agent.say(&format!("cannot listen on port {port}: {e:#}"));
        })
    };
    let control = listen(ports::CONTROL)?;
    let pty = listen(ports::PTY)?;
    agent.say(&format!(
        "listens on ports {} (control) and {} (pty)",
        ports::CONTROL,
        ports::PTY
    ));
    let a = agent.clone();
    std::thread::spawn(move || loop {
        match pty.accept() {
            Ok(conn) => {
                let a = a.clone();
                std::thread::spawn(move || {
                    if let Err(e) = attach(&a, conn) {
                        a.say(&format!("pty connection: {e:#}"));
                    }
                });
            }
            Err(e) => {
                a.say(&format!("pty port: {e:#}"));
                return;
            }
        }
    });
    loop {
        let conn = control.accept().inspect_err(|e| {
            agent.say(&format!("control port: {e:#}"));
        })?;
        let a = agent.clone();
        std::thread::spawn(move || {
            if let Err(e) = handle(&a, conn) {
                a.say(&format!("control connection: {e:#}"));
            }
        });
    }
}

/// The first line of a pty connection names its run.
fn attach(agent: &Agent, conn: OwnedFd) -> Result<()> {
    let mut f = std::fs::File::from(conn.try_clone()?);
    let Some(proto::Request::Attach { token }) = proto::read_line(&mut f)? else {
        bail!("the pty connection did not start with attach");
    };
    let Some(tx) = agent.pending.lock().unwrap().remove(&token) else {
        bail!("no run waits for this pty");
    };
    agent.say("pty connection of a run");
    let _ = tx.send(conn);
    Ok(())
}

fn handle(agent: &Arc<Agent>, conn: OwnedFd) -> Result<()> {
    let mut f = std::fs::File::from(conn);
    let Some(req) = proto::read_line::<proto::Request>(&mut f)? else {
        agent.say("control connection closed without a request");
        return Ok(());
    };
    agent.say(&format!("control connection: {}", req.kind()));
    match req {
        proto::Request::Ping => proto::write_line(
            &mut f,
            &proto::Reply::Ok {
                runs: Some(agent.runs() as u32),
            },
        ),
        proto::Request::Run(r) => super::agent_run::run(agent, r, f),
        proto::Request::Connect { token, port } => {
            if !agent.is_live(&token) {
                return proto::write_line(&mut f, &proto::Reply::error("no run has this token"));
            }
            match std::net::TcpStream::connect(("127.0.0.1", port)) {
                Ok(tcp) => {
                    proto::write_line(&mut f, &proto::Reply::ok())?;
                    let tcp = OwnedFd::from(tcp);
                    super::bridge::splice(f.into(), tcp)?;
                    Ok(())
                }
                Err(e) => proto::write_line(
                    &mut f,
                    &proto::Reply::error(format!("nothing listens on port {port}: {e}")),
                ),
            }
        }
        proto::Request::Resize { .. } | proto::Request::Attach { .. } => {
            proto::write_line(&mut f, &proto::Reply::error("this request needs a run"))
        }
    }
}

/// A unix socket in the guest whose connections go to a host port, after
/// the token line. It ends when the guard drops.
pub struct Outlet {
    pub path: PathBuf,
    stop: Arc<std::sync::atomic::AtomicBool>,
}

impl Outlet {
    pub fn start(
        transport: Transport,
        path: PathBuf,
        port: u32,
        token: String,
        owner: Option<(u32, u32)>,
    ) -> Result<Self> {
        let _ = std::fs::remove_file(&path);
        let l =
            UnixListener::bind(&path).with_context(|| format!("cannot bind {}", path.display()))?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        if let Some((uid, gid)) = owner {
            std::os::unix::fs::chown(&path, Some(uid), Some(gid))?;
        }
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let s = stop.clone();
        std::thread::spawn(move || {
            for conn in l.incoming() {
                if s.load(std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
                let Ok(conn) = conn else { continue };
                let (transport, token) = (transport.clone(), token.clone());
                std::thread::spawn(move || {
                    let host = match transport.dial(port) {
                        Ok(h) => h,
                        Err(e) => {
                            say(&transport, &format!("{e:#}"));
                            return;
                        }
                    };
                    let line = format!("{token}\n");
                    // SAFETY: write on an open socket fd.
                    let n =
                        unsafe { libc::write(host.as_raw_fd(), line.as_ptr().cast(), line.len()) };
                    if n == line.len() as isize {
                        let _ = super::bridge::splice(conn.into(), host);
                    }
                });
            }
        });
        Ok(Self { path, stop })
    }
}

impl Drop for Outlet {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        // Wake the accept loop so that it sees the stop.
        let _ = UnixStream::connect(&self.path);
        let _ = std::fs::remove_file(&self.path);
    }
}

/// A file from an fd that the caller owns as a raw number. Used for the
/// child's standard streams.
pub(crate) fn dup(fd: &OwnedFd) -> Result<OwnedFd> {
    // SAFETY: dup of an open fd returns a new fd or -1.
    let d = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
    if d < 0 {
        return Err(std::io::Error::last_os_error()).context("dup");
    }
    // SAFETY: `d` is new and nothing else owns it.
    Ok(unsafe { OwnedFd::from_raw_fd(d) })
}

#[cfg(test)]
#[path = "agent_tests.rs"]
mod tests;

/// The device holds an ext2, ext3 or ext4 file system.
fn has_ext_magic(dev: &Path) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    let mut b = [0u8; 2];
    std::fs::File::open(dev)
        .and_then(|mut f| {
            f.seek(SeekFrom::Start(1080))?;
            f.read_exact(&mut b)
        })
        .is_ok()
        && b == [0x53, 0xEF]
}

#[cfg(test)]
mod ext_magic_tests {
    #[test]
    fn an_empty_disk_has_no_magic_and_a_superblock_has() {
        let t = tempfile::tempdir().unwrap();
        let f = t.path().join("disk");
        std::fs::write(&f, vec![0u8; 4096]).unwrap();
        assert!(!super::has_ext_magic(&f));
        let mut b = vec![0u8; 4096];
        b[1080] = 0x53;
        b[1081] = 0xEF;
        std::fs::write(&f, b).unwrap();
        assert!(super::has_ext_magic(&f));
    }
}
