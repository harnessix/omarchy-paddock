//! The host side of the VM backend: one long-lived VM per user. A small
//! supervisor (`paddock vm-serve`) starts the `paddock-vm` helper, records
//! it, and stops the VM after an idle time without runs.
//!
//! Guest connections to the broker and proxy ports go to a route folder per
//! port (`bridge::forward`). Each run puts a link named after its token
//! there, to its own broker and proxy sockets, for as long as it runs.

use super::{assets, helper, ports, proto};
use crate::env::HostEnv;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The VM stops after this time without a run.
pub const IDLE: Duration = Duration::from_secs(600);
/// The longest boot.
const BOOT_WAIT: Duration = Duration::from_secs(60);

/// The files of the VM on the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    /// The state folder of the helper (helper::StateLayout).
    pub state: PathBuf,
    /// The record of the running VM.
    pub record: PathBuf,
    /// The lock that keeps two starts apart.
    pub lock: PathBuf,
    /// The folder of the host sockets of the VM: the control socket and
    /// the route folders. It comes from sockpath.rs, so its paths fit in a
    /// unix socket address on macOS.
    pub sockets: PathBuf,
    /// The control socket of the helper (`paddock-vm start --control`).
    pub control: PathBuf,
    /// The route folders of the broker and proxy ports.
    pub broker_routes: PathBuf,
    pub proxy_routes: PathBuf,
}

impl Layout {
    pub fn new(env: &HostEnv) -> Self {
        let state = env.paddock_state().join("vm");
        let routes = crate::sockpath::socket_dir(&env.paddock_run(), env.uid, "").join("vm");
        Self {
            record: state.join("vm.json"),
            lock: state.join("vm.lock"),
            control: routes.join("control.sock"),
            broker_routes: routes.join("broker"),
            proxy_routes: routes.join("proxy"),
            sockets: routes,
            state,
        }
    }

    pub fn control(&self) -> PathBuf {
        self.control.clone()
    }

    /// Make the socket folders, each one private, and check the length of
    /// the control socket path.
    pub fn prepare_sockets(&self) -> Result<()> {
        crate::sockpath::check(&self.control)?;
        for d in [&self.sockets, &self.broker_routes, &self.proxy_routes] {
            crate::privfs::dir(d)?;
        }
        Ok(())
    }

    pub fn console(&self) -> PathBuf {
        helper::StateLayout::new(&self.state).console
    }

    /// The `--forward` arguments of the helper: the forward map.
    pub fn forwards(&self) -> Vec<(u32, PathBuf)> {
        vec![
            (ports::BROKER, self.broker_routes.clone()),
            (ports::PROXY, self.proxy_routes.clone()),
        ]
    }

    /// The runs that the VM serves now: one route per run.
    pub fn runs(&self) -> usize {
        std::fs::read_dir(&self.broker_routes)
            .map(|d| {
                d.flatten()
                    .filter(|e| e.file_name().to_string_lossy().ends_with(".sock"))
                    .count()
            })
            .unwrap_or(0)
    }
}

/// The record of the running VM.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// The supervisor (`paddock vm-serve`).
    pub serve_pid: u32,
    /// The `paddock-vm` helper.
    pub helper_pid: u32,
    /// Seconds since the epoch.
    pub started: u64,
    pub project: PathBuf,
}

fn alive(pid: u32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    pid > 0 && unsafe { libc::kill(pid as i32, 0) } == 0
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The running VM, or None.
pub fn running(layout: &Layout) -> Option<Record> {
    let text = std::fs::read_to_string(&layout.record).ok()?;
    let rec: Record = serde_json::from_str(&text).ok()?;
    (alive(rec.serve_pid) && alive(rec.helper_pid)).then_some(rec)
}

/// Send one line on the control socket of the helper. `ok` gives the
/// connection, which carries the bytes of a port after `connect`.
pub fn control(sock: &Path, line: &str) -> Result<UnixStream> {
    let mut c = UnixStream::connect(sock)
        .with_context(|| format!("cannot connect to the VM at {}", sock.display()))?;
    read_timeout(&c, Some(Duration::from_secs(10)));
    c.write_all(format!("{line}\n").as_bytes())?;
    let answer = proto::read_raw_line(&mut c, 4096)?.context("the VM closed the connection")?;
    read_timeout(&c, None);
    log::debug!("vm control {line}: {answer}");
    match answer.strip_prefix("error ") {
        None if answer == "ok" => Ok(c),
        Some(e) => bail!("the VM: {e}"),
        None => bail!("the VM sent an unknown answer"),
    }
}

/// Set the read timeout of a socket. macOS fails setsockopt with EINVAL
/// when the peer has closed the connection, which the helper does right
/// after the answer to `ping` and `stop`. Then a read gets the end of the
/// data at once, so the timeout does not matter and the error is ignored.
pub fn read_timeout(c: &UnixStream, d: Option<Duration>) {
    if let Err(e) = c.set_read_timeout(d) {
        log::debug!("vm socket read timeout: {e}");
    }
}

/// Connect to a port in the guest.
pub fn connect(layout: &Layout, port: u32) -> Result<UnixStream> {
    control(&layout.control(), &format!("connect {port}"))
}

/// The `paddock-vm` helper: next to paddock, else on PATH.
pub fn helper_path(env: &HostEnv) -> Result<PathBuf> {
    if let Some(p) = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join("paddock-vm")))
        .filter(|p| p.is_file())
    {
        return Ok(p);
    }
    env.which("paddock-vm")
        .context("paddock-vm is not installed next to paddock or on PATH")
}

/// A lock file, held until the guard drops.
struct Lock {
    _file: std::fs::File,
}

impl Lock {
    fn take(path: &Path) -> Result<Self> {
        let f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .with_context(|| format!("cannot open {}", path.display()))?;
        use std::os::fd::AsRawFd;
        // SAFETY: flock on an open file of this process.
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } < 0 {
            return Err(std::io::Error::last_os_error()).context("cannot lock the VM");
        }
        Ok(Self { _file: f })
    }
}

/// The running VM for `project`, started when none runs.
pub fn ensure(env: &HostEnv, project: &Path) -> Result<Record> {
    let layout = Layout::new(env);
    crate::privfs::dir(&layout.state)?;
    let _lock = Lock::take(&layout.lock)?;
    if let Some(rec) = running(&layout) {
        let ping = control(&layout.control(), "ping");
        if let Err(e) = &ping {
            log::debug!(
                "vm: the running VM (pid {}) does not answer: {e:#}",
                rec.helper_pid
            );
        }
        if ping.is_ok() {
            if rec.project == project {
                wait_for_guest(&layout, Instant::now() + BOOT_WAIT)?;
                return Ok(rec);
            }
            let runs = layout.runs();
            if runs > 0 {
                bail!(
                    "the VM serves {} now, with {runs} run(s). It holds one project at a time: start again when those runs end",
                    rec.project.display()
                );
            }
            stop(&layout, Duration::from_secs(20))?;
        }
    }
    let _ = std::fs::remove_file(&layout.record);
    let me = std::env::current_exe().context("cannot find the paddock executable")?;
    let mut cmd = std::process::Command::new(me);
    cmd.arg("vm-serve")
        .arg(project)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    use std::os::unix::process::CommandExt;
    // SAFETY: setsid is async-signal-safe. The VM outlives this run.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = cmd.spawn().context("cannot start the VM supervisor")?;
    log::debug!(
        "vm: started vm-serve (pid {}) for {}, control socket {}",
        child.id(),
        project.display(),
        layout.control().display()
    );
    let end = Instant::now() + BOOT_WAIT;
    let mut last = String::from("no record of the VM yet");
    loop {
        let why = match running(&layout) {
            None => "no record of the VM yet".to_string(),
            Some(rec) => match control(&layout.control(), "ping") {
                Ok(_) => {
                    log::debug!("vm: the helper (pid {}) answers", rec.helper_pid);
                    wait_for_guest(&layout, end)?;
                    return Ok(rec);
                }
                Err(e) => format!("the helper does not answer: {e:#}"),
            },
        };
        if why != last {
            log::debug!("vm: {why}");
            last = why;
        }
        if let Some(status) = child.try_wait()? {
            bail!(
                "the VM did not start ({status}). The console log is {}",
                layout.console().display()
            );
        }
        if Instant::now() > end {
            bail!(
                "the VM did not answer in {} seconds ({last}). The console log is {}",
                BOOT_WAIT.as_secs(),
                layout.console().display()
            );
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// The guest agent answers a ping on the control port. The helper answers
/// as soon as the VM runs, which is before the guest has booted.
fn guest_ping(layout: &Layout) -> Result<()> {
    let mut c = connect(layout, ports::CONTROL)?;
    read_timeout(&c, Some(Duration::from_secs(5)));
    proto::write_line(&mut c, &proto::Request::Ping)?;
    match proto::read_line::<proto::Reply>(&mut c)? {
        Some(proto::Reply::Ok { .. }) => Ok(()),
        Some(other) => bail!("the guest agent answered {other:?}"),
        None => bail!("the guest agent closed the connection"),
    }
}

fn wait_for_guest(layout: &Layout, end: Instant) -> Result<()> {
    let mut last = String::new();
    loop {
        let e = match guest_ping(layout) {
            Ok(()) => {
                log::debug!("vm: the guest agent answers");
                return Ok(());
            }
            Err(e) => format!("{e:#}"),
        };
        if e != last {
            log::debug!("vm: the guest agent does not answer yet: {e}");
            last = e;
        }
        if Instant::now() > end {
            bail!(
                "the guest agent of the VM did not answer ({last}). The console log is {}",
                layout.console().display()
            );
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Ask the guest to stop and wait for the helper to end.
pub fn stop(layout: &Layout, wait: Duration) -> Result<bool> {
    let Some(rec) = running(layout) else {
        return Ok(false);
    };
    if control(&layout.control(), "stop").is_err() {
        // SAFETY: a stop signal to the helper that the record names.
        unsafe { libc::kill(rec.helper_pid as i32, libc::SIGTERM) };
    }
    let end = Instant::now() + wait;
    while alive(rec.helper_pid) {
        if Instant::now() > end {
            bail!("the VM (pid {}) did not stop", rec.helper_pid);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(true)
}

/// `paddock vm-serve PROJECT`: start the helper and watch it. The VM
/// stops after [`IDLE`] without a run.
pub fn serve(env: &HostEnv, project: &Path) -> Result<i32> {
    let layout = Layout::new(env);
    crate::privfs::dir(&layout.state)?;
    layout.prepare_sockets()?;
    let mut cmd = std::process::Command::new(helper_path(env)?);
    cmd.arg("start")
        .arg("--assets")
        .arg(assets::assets_dir(env))
        .arg("--project")
        .arg(project)
        .arg("--state")
        .arg(&layout.state)
        .arg("--control")
        .arg(&layout.control);
    for (port, dir) in layout.forwards() {
        cmd.arg("--forward")
            .arg(format!("{port}={}", dir.display()));
    }
    let log = std::fs::File::create(layout.state.join("helper.log"))?;
    cmd.stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    let mut child = cmd.spawn().context("cannot start paddock-vm")?;
    let rec = Record {
        serve_pid: std::process::id(),
        helper_pid: child.id(),
        started: now(),
        project: project.to_path_buf(),
    };
    crate::privfs::write_atomic(&layout.record, serde_json::to_vec(&rec)?)?;
    let mut idle_since = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break s;
        }
        if layout.runs() > 0 {
            idle_since = Instant::now();
        } else if idle_since.elapsed() >= IDLE {
            let _ = control(&layout.control(), "stop");
            break child.wait()?;
        }
        std::thread::sleep(Duration::from_secs(2));
    };
    let _ = std::fs::remove_file(&layout.record);
    Ok(super::term::exit_code(status))
}

/// The links of one run in the route folders, until the guard drops.
pub struct Route {
    links: Vec<PathBuf>,
}

impl Route {
    pub fn add(layout: &Layout, token: &str, broker: &Path, proxy: Option<&Path>) -> Result<Self> {
        if !proto::valid_token(token) {
            bail!("not a run token");
        }
        let mut r = Self { links: Vec::new() };
        let name = format!("{token}.sock");
        let mut pairs = vec![(layout.broker_routes.join(&name), broker)];
        if let Some(p) = proxy {
            pairs.push((layout.proxy_routes.join(&name), p));
        }
        for (link, target) in pairs {
            std::os::unix::fs::symlink(target, &link)
                .with_context(|| format!("cannot link {}", link.display()))?;
            r.links.push(link);
        }
        Ok(r)
    }
}

impl Drop for Route {
    fn drop(&mut self) {
        for l in &self.links {
            let _ = std::fs::remove_file(l);
        }
    }
}

/// `paddock vm status`: the lines about the running VM.
pub fn status_lines(env: &HostEnv) -> Vec<String> {
    let layout = Layout::new(env);
    match running(&layout) {
        None => vec!["VM        not running".into()],
        Some(r) => vec![
            format!("VM        running, pid {}", r.helper_pid),
            format!("uptime    {}", duration(now().saturating_sub(r.started))),
            format!("runs      {}", layout.runs()),
            format!("project   {}", r.project.display()),
        ],
    }
}

/// `paddock vm status --json`: the running VM, or null.
pub fn status_json(env: &HostEnv) -> serde_json::Value {
    let layout = Layout::new(env);
    match running(&layout) {
        None => serde_json::json!({ "running": false }),
        Some(r) => serde_json::json!({
            "running": true,
            "pid": r.helper_pid,
            "uptime_secs": now().saturating_sub(r.started),
            "runs": layout.runs(),
            "project": r.project,
            "control": layout.control(),
        }),
    }
}

/// "1h 02m", "3m 05s".
pub fn duration(secs: u64) -> String {
    match secs {
        s if s >= 3600 => format!("{}h {:02}m", s / 3600, s % 3600 / 60),
        s => format!("{}m {:02}s", s / 60, s % 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn env(t: &Path) -> HostEnv {
        HostEnv::from_parts(t.into(), "u".into(), 501, 20, Default::default())
    }

    #[test]
    fn the_forward_map_names_two_route_folders() {
        let t = tempfile::tempdir().unwrap();
        let l = Layout::new(&env(t.path()));
        let f = l.forwards();
        assert_eq!(f.len(), 2);
        assert_eq!(f[0], (1025, l.broker_routes.clone()));
        assert_eq!(f[1], (1026, l.proxy_routes.clone()));
        assert!(l.broker_routes.ends_with("vm/broker"));
        assert!(l.proxy_routes.ends_with("vm/proxy"));
        // The control socket is with the routes, in the short socket
        // folder, not in the state folder.
        assert_eq!(l.control(), l.sockets.join("control.sock"));
        assert!(!l.control().starts_with(&l.state));
        assert!(l.broker_routes.starts_with(&l.sockets));
        // The helper accepts the map as it is.
        for (port, dir) in f {
            let arg = format!("{port}={}", dir.display());
            let cli = <helper::Cli as clap::Parser>::try_parse_from([
                "paddock-vm",
                "start",
                "--assets",
                "/a",
                "--project",
                "/p",
                "--state",
                "/s",
                "--control",
                l.control().to_str().unwrap(),
                "--forward",
                &arg,
            ]);
            assert!(cli.is_ok(), "{arg}");
        }
    }

    /// A temporary home on macOS is in the long `$TMPDIR`. The control
    /// socket still fits.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_control_socket_fits_with_a_long_home() {
        let home = std::path::PathBuf::from(format!("/private/var/folders/{}", "x".repeat(90)));
        let l = Layout::new(&env(&home));
        assert!(l.state.as_os_str().len() > 104, "{}", l.state.display());
        assert!(
            crate::sockpath::check(&l.control()).is_ok(),
            "{}",
            l.control().display()
        );
        assert!(
            crate::sockpath::check(&l.broker_routes.join(format!("{}.sock", "a".repeat(32))))
                .is_ok()
        );
    }

    #[test]
    fn a_route_lives_as_long_as_its_guard() {
        let t = tempfile::tempdir().unwrap();
        let l = Layout::new(&env(t.path()));
        for d in [&l.broker_routes, &l.proxy_routes] {
            std::fs::create_dir_all(d).unwrap();
        }
        assert_eq!(l.runs(), 0);
        let token = proto::new_token().unwrap();
        let r = Route::add(&l, &token, Path::new("/b.sock"), Some(Path::new("/p.sock"))).unwrap();
        assert_eq!(l.runs(), 1);
        assert_eq!(
            std::fs::read_link(l.proxy_routes.join(format!("{token}.sock"))).unwrap(),
            Path::new("/p.sock")
        );
        drop(r);
        assert_eq!(l.runs(), 0);
        assert!(Route::add(&l, "../x", Path::new("/b"), None).is_err());
    }

    #[test]
    fn no_record_means_not_running() {
        let t = tempfile::tempdir().unwrap();
        let e = env(t.path());
        assert_eq!(status_lines(&e), vec!["VM        not running"]);
        assert_eq!(status_json(&e)["running"], false);
        let l = Layout::new(&e);
        std::fs::create_dir_all(&l.state).unwrap();
        // A record of processes that are gone.
        let rec = Record {
            serve_pid: 0,
            helper_pid: 0,
            started: 1,
            project: "/p".into(),
        };
        std::fs::write(&l.record, serde_json::to_vec(&rec).unwrap()).unwrap();
        assert!(running(&l).is_none());
        assert!(!stop(&l, Duration::from_millis(10)).unwrap());
        assert_eq!(duration(3725), "1h 02m");
        assert_eq!(duration(185), "3m 05s");
    }

    #[test]
    fn control_reads_ok_and_error() {
        let t = tempfile::tempdir().unwrap();
        let sock = t.path().join("c.sock");
        let l = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        std::thread::spawn(move || {
            for (i, c) in l.incoming().enumerate() {
                let mut c = c.unwrap();
                let mut b = [0u8; 64];
                let _ = c.read(&mut b);
                let _ = c.write_all(if i == 0 {
                    b"ok\nrest"
                } else {
                    b"error no guest\n"
                });
            }
        });
        let mut c = control(&sock, "connect 1024").unwrap();
        let mut rest = String::new();
        c.read_to_string(&mut rest).unwrap();
        assert_eq!(rest, "rest");
        let e = control(&sock, "ping").unwrap_err().to_string();
        assert_eq!(e, "the VM: no guest");
    }
}
