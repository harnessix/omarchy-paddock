//! `paddock gatewayd`: the executor of approved host requests.
//!
//! paddock, in the process that runs a sandbox, decides and asks the user.
//! It does not run the approved work itself: it signs a grant for the exact
//! request (`gateway::sign`) and sends it here. This process checks the
//! signature, the time, that the grant is new, and that it covers exactly
//! this request, and only then runs it.
//!
//! - It listens on `$XDG_RUNTIME_DIR/paddock/gatewayd.sock`. With the systemd
//!   user units of `paddock gateway install`, systemd owns the socket and
//!   starts this process on the first connection (socket activation).
//!   Without them, paddock starts it when a request needs it.
//! - It exits after a while without connections. The next request starts it
//!   again.
//! - The first capability that it runs is `exec`. The others follow.

use crate::broker::Request;
use crate::env::HostEnv;
use crate::gateway::{self, Job, Seen};
use anyhow::{bail, Context as _, Result};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The largest job: a request of the gateway and the environment.
const MAX_JOB: u64 = 1 << 20;
/// The longest push.
const PUSH_TIMEOUT: u64 = 600;
/// Exit after this long without a connection.
pub const IDLE: Duration = Duration::from_secs(120);

pub fn socket_path(env: &HostEnv) -> PathBuf {
    env.paddock_run().join("gatewayd.sock")
}

/// Run one job that `verify` accepted. The result is the JSON that the
/// broker returns to the sandbox. `how` is the isolation of a command.
fn run(
    env: &HostEnv,
    how: Result<crate::execbox::Isolation, String>,
    job: &Job,
) -> Result<serde_json::Value> {
    match job.request.cap.as_str() {
        "exec" => {
            let argv: Vec<String> = job
                .request
                .args
                .get("argv")
                .and_then(|a| serde_json::from_value(a.clone()).ok())
                .filter(|a: &Vec<String>| !a.is_empty())
                .context("exec needs argv")?;
            let timeout = job
                .request
                .args
                .get("timeout")
                .and_then(|t| t.as_u64())
                .unwrap_or(120)
                .clamp(1, 3600);
            let how = how.map_err(|e| anyhow::anyhow!("{e}"))?;
            let cfg = crate::execbox::ExecCfg::load(env)?;
            let unit = format!(
                "paddock-exec-{}",
                &job.grant.nonce[..12.min(job.grant.nonce.len())]
            );
            let mut cmd = crate::execbox::command(
                how,
                &cfg,
                &env.home,
                &job.grant.project,
                &argv,
                &cfg.pass_env(&job.env),
                timeout,
                &unit,
            );
            let (code, out, err) =
                crate::broker::run_capture(&mut cmd, Duration::from_secs(timeout + 10))?;
            if let Some(why) = crate::execbox::setup_failure(how, code, &err) {
                bail!("{why}");
            }
            Ok(serde_json::json!({ "exit_code": code, "stdout": out, "stderr": err }))
        }
        "git-push" => {
            let x: crate::broker::git::PushExec =
                serde_json::from_value(job.plan.clone()).context("a push needs its plan")?;
            let how = how.map_err(|e| anyhow::anyhow!("{e}"))?;
            // The user's credentials stay readable (git and ssh configuration,
            // keys, agent sockets) but nothing in the home folder can change;
            // only the scratch repository is writable.
            // A remote on this machine (a path or file:// URL) is the one
            // folder outside the scratch repository that the push writes.
            let local = x
                .url
                .strip_prefix("file://")
                .unwrap_or(&x.url)
                .starts_with('/')
                .then(|| PathBuf::from(x.url.strip_prefix("file://").unwrap_or(&x.url)))
                .filter(|p| p.exists());
            let cfg = crate::execbox::ExecCfg {
                home: crate::execbox::Home::ReadOnly,
                network: true,
                paths: local.into_iter().collect(),
                ro_paths: Vec::new(),
                env: crate::broker::git::PUSH_ENV
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
            };
            let unit = format!(
                "paddock-push-{}",
                &job.grant.nonce[..12.min(job.grant.nonce.len())]
            );
            crate::broker::git::push_exec(&x, |argv, extra| {
                let mut vars = cfg.pass_env(&job.env);
                vars.extend(extra.clone());
                let mut cmd = crate::execbox::command(
                    how,
                    &cfg,
                    &env.home,
                    &x.scratch,
                    argv,
                    &vars,
                    PUSH_TIMEOUT,
                    &unit,
                );
                let (code, out, err) =
                    crate::broker::run_capture(&mut cmd, Duration::from_secs(PUSH_TIMEOUT + 10))?;
                if let Some(why) = crate::execbox::setup_failure(how, code, &err) {
                    bail!("{why}");
                }
                Ok((code, out, err))
            })
        }
        other => bail!("the executor does not run {other} yet"),
    }
}

/// Check a job and run it. Every refusal is an error with the reason.
pub fn handle(
    env: &HostEnv,
    how: Result<crate::execbox::Isolation, String>,
    key: &[u8],
    seen: &Seen,
    job: &Job,
) -> Result<serde_json::Value> {
    gateway::verify(key, job, seen, crate::state::now())
        .map_err(|r| anyhow::anyhow!("the executor refused the request: {r}"))?;
    run(env, how, job)
}

/// The answer on the socket: `{"ok": RESULT}` or `{"error": "TEXT"}`.
fn answer(
    env: &HostEnv,
    how: Result<crate::execbox::Isolation, String>,
    key: &[u8],
    seen: &Seen,
    line: &str,
) -> serde_json::Value {
    let job: Job = match serde_json::from_str(line) {
        Ok(j) => j,
        Err(e) => return serde_json::json!({ "error": format!("bad job: {e}") }),
    };
    match handle(env, how, key, seen, &job) {
        Ok(v) => serde_json::json!({ "ok": v }),
        Err(e) => serde_json::json!({ "error": format!("{e:#}") }),
    }
}

fn serve_conn(
    env: &HostEnv,
    how: Result<crate::execbox::Isolation, String>,
    key: &[u8],
    seen: &Seen,
    conn: UnixStream,
) -> Result<()> {
    let mut reader = BufReader::new(conn.try_clone()?.take(MAX_JOB));
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let reply = answer(env, how, key, seen, line.trim());
    let mut w = conn;
    writeln!(w, "{reply}")?;
    Ok(())
}

/// The listener: the socket that systemd passes (`LISTEN_FDS`), or our own.
fn listener(env: &HostEnv) -> Result<Option<UnixListener>> {
    let pid_ok = std::env::var("LISTEN_PID")
        .ok()
        .and_then(|p| p.parse::<u32>().ok())
        == Some(std::process::id());
    if pid_ok && std::env::var("LISTEN_FDS").ok().as_deref() == Some("1") {
        use std::os::unix::io::FromRawFd;
        // SAFETY: systemd passes the listening socket as fd 3.
        return Ok(Some(unsafe { UnixListener::from_raw_fd(3) }));
    }
    let path = socket_path(env);
    if let Some(d) = path.parent() {
        crate::privfs::dir(d)?;
    }
    match UnixListener::bind(&path) {
        Ok(l) => Ok(Some(l)),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            // Another executor answers already: nothing to do.
            if UnixStream::connect(&path).is_ok() {
                return Ok(None);
            }
            // A socket of an executor that ended.
            std::fs::remove_file(&path)?;
            Ok(Some(UnixListener::bind(&path)?))
        }
        Err(e) => Err(e).with_context(|| format!("cannot listen on {}", path.display())),
    }
}

/// `paddock gatewayd`: serve until `idle` passes without a connection.
pub fn main(env: &HostEnv, idle: Duration) -> Result<i32> {
    crate::update::refuse_in_sandbox(env, "paddock gatewayd")?;
    let key: Arc<Vec<u8>> = Arc::new(gateway::key(env).context("cannot read the gateway key")?);
    // How exec is isolated is decided at its first use: an executor on a
    // host without systemd or bwrap still starts, and refuses only exec.
    let how = Arc::new(std::sync::OnceLock::new());
    let shared = Arc::new(env.clone());
    let Some(l) = listener(env)? else {
        return Ok(0);
    };
    l.set_nonblocking(true)?;
    let seen = Arc::new(Seen::default());
    let start = Instant::now();
    let last = Arc::new(AtomicU64::new(0));
    let running = Arc::new(AtomicU64::new(0));
    loop {
        match l.accept() {
            Ok((conn, _)) => {
                conn.set_nonblocking(false)?;
                last.store(start.elapsed().as_secs(), Ordering::SeqCst);
                running.fetch_add(1, Ordering::SeqCst);
                let how = how.clone();
                let (env, key, seen, last, running) = (
                    shared.clone(),
                    key.clone(),
                    seen.clone(),
                    last.clone(),
                    running.clone(),
                );
                std::thread::spawn(move || {
                    let iso = how
                        .get_or_init(|| crate::execbox::detect(&env).map_err(|e| format!("{e:#}")));
                    if let Err(e) = serve_conn(&env, iso.clone(), &key, &seen, conn) {
                        log::debug!("gateway connection failed: {e:#}");
                    }
                    last.store(start.elapsed().as_secs(), Ordering::SeqCst);
                    running.fetch_sub(1, Ordering::SeqCst);
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                let quiet = start.elapsed().as_secs() - last.load(Ordering::SeqCst);
                if running.load(Ordering::SeqCst) == 0 && quiet >= idle.as_secs() {
                    return Ok(0);
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// Where the broker sends signed jobs.
pub trait Dispatch: Send + Sync {
    fn dispatch(&self, job: &Job, wait: Duration) -> Result<serde_json::Value>;
    /// The key that signs jobs for this executor.
    fn signing_key(&self, env: &HostEnv) -> Result<Vec<u8>> {
        gateway::key(env).context("cannot read the gateway key")
    }
}

/// The executor process, started when it is not there.
pub struct Remote {
    pub socket: PathBuf,
    pub program: PathBuf,
    pub env: HostEnv,
}

impl Remote {
    pub fn new(env: &HostEnv) -> Result<Self> {
        Ok(Self {
            socket: socket_path(env),
            program: std::env::current_exe()
                .and_then(std::fs::canonicalize)
                .context("cannot find the paddock executable")?,
            env: env.clone(),
        })
    }

    fn connect(&self) -> Result<UnixStream> {
        if let Ok(s) = UnixStream::connect(&self.socket) {
            return Ok(s);
        }
        start_executor(&self.program, &self.env)?;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match UnixStream::connect(&self.socket) {
                Ok(s) => return Ok(s),
                Err(e) if Instant::now() >= deadline => {
                    return Err(e).with_context(|| {
                        format!("cannot reach the executor at {}", self.socket.display())
                    })
                }
                Err(_) => std::thread::sleep(Duration::from_millis(50)),
            }
        }
    }
}

/// Start `paddock gatewayd` in its own session, detached.
fn start_executor(program: &Path, env: &HostEnv) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let mut cmd = std::process::Command::new(program);
    cmd.arg("gatewayd")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if let Some(rt) = &env.runtime_dir {
        cmd.env("XDG_RUNTIME_DIR", rt);
    }
    // SAFETY: setsid is async-signal-safe. The executor outlives this call.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().context("cannot start paddock gatewayd")?;
    Ok(())
}

impl Dispatch for Remote {
    fn dispatch(&self, job: &Job, wait: Duration) -> Result<serde_json::Value> {
        let mut s = self.connect()?;
        s.set_read_timeout(Some(wait))?;
        writeln!(s, "{}", serde_json::to_string(job)?)?;
        let mut line = String::new();
        BufReader::new(s.take(64 << 20)).read_line(&mut line)?;
        let v: serde_json::Value =
            serde_json::from_str(line.trim()).context("bad answer from the executor")?;
        match (v.get("ok"), v.get("error")) {
            (Some(ok), _) => Ok(ok.clone()),
            (_, Some(e)) => bail!("{}", e.as_str().unwrap_or("the executor failed")),
            _ => bail!("bad answer from the executor"),
        }
    }
}

/// The executor in this process, for tests: the same checks, no process.
pub struct InProcess {
    pub key: Vec<u8>,
    pub seen: Seen,
    /// The executor's view of the host (its user configuration).
    pub env: HostEnv,
    pub isolation: crate::execbox::Isolation,
}

impl Dispatch for InProcess {
    fn dispatch(&self, job: &Job, _wait: Duration) -> Result<serde_json::Value> {
        handle(&self.env, Ok(self.isolation), &self.key, &self.seen, job)
    }
    fn signing_key(&self, _env: &HostEnv) -> Result<Vec<u8>> {
        Ok(self.key.clone())
    }
}

/// The host environment for an executed command: the variables of this
/// paddock process, which the sandbox never sets.
pub fn host_env() -> std::collections::BTreeMap<String, String> {
    std::env::vars().collect()
}

/// For the broker: sign `req` and run it in the executor.
pub fn execute(
    dispatch: &dyn Dispatch,
    key: &[u8],
    sandbox: &str,
    project: &Path,
    req: &Request,
    plan: serde_json::Value,
) -> Result<serde_json::Value> {
    let job = gateway::sign(key, sandbox, project, req, host_env(), plan)?;
    let timeout = match req.cap.as_str() {
        "git-push" => PUSH_TIMEOUT,
        _ => req
            .args
            .get("timeout")
            .and_then(|t| t.as_u64())
            .unwrap_or(120)
            .clamp(1, 3600),
    };
    dispatch.dispatch(&job, Duration::from_secs(timeout + 30))
}

// ---------------------------------------------------------------------------
// systemd socket activation (`paddock gateway install|remove|status`).

const UNIT_MARK: &str = "# Written by paddock (paddock gateway remove deletes it).";
pub const SOCKET_UNIT: &str = "paddock-gatewayd.socket";
pub const SERVICE_UNIT: &str = "paddock-gatewayd.service";

fn unit_dir(env: &HostEnv) -> PathBuf {
    env.config_dir.join("systemd").join("user")
}

pub fn socket_unit() -> String {
    format!(
        "{UNIT_MARK}\n[Unit]\nDescription=paddock gateway executor socket\n\n\
         [Socket]\nListenStream=%t/paddock/gatewayd.sock\nSocketMode=0600\nDirectoryMode=0700\n\n\
         [Install]\nWantedBy=sockets.target\n"
    )
}

pub fn service_unit(paddock: &Path) -> String {
    format!(
        "{UNIT_MARK}\n[Unit]\nDescription=paddock gateway executor\nRequires={SOCKET_UNIT}\n\n\
         [Service]\nExecStart={} gatewayd\n",
        paddock.display()
    )
}

fn systemctl(args: &[&str]) -> Result<()> {
    let st = std::process::Command::new("systemctl")
        .arg("--user")
        .args(args)
        .stdin(std::process::Stdio::null())
        .status()
        .context("cannot run systemctl --user")?;
    if !st.success() {
        bail!("systemctl --user {} failed with {st}", args.join(" "));
    }
    Ok(())
}

fn ours(p: &Path) -> bool {
    std::fs::read_to_string(p)
        .map(|t| t.starts_with(UNIT_MARK))
        .unwrap_or(false)
}

/// Write the units and turn the socket on.
pub fn install(env: &HostEnv) -> Result<Vec<PathBuf>> {
    crate::update::refuse_in_sandbox(env, "paddock gateway install")?;
    let paddock = crate::launchers::launcher_exe(env)?;
    let dir = unit_dir(env);
    std::fs::create_dir_all(&dir)?;
    let files = [
        (dir.join(SOCKET_UNIT), socket_unit()),
        (dir.join(SERVICE_UNIT), service_unit(&paddock)),
    ];
    for (p, _) in &files {
        if p.exists() && !ours(p) {
            bail!(
                "{} exists and paddock did not write it; nothing was changed",
                p.display()
            );
        }
    }
    for (p, text) in &files {
        std::fs::write(p, text)?;
    }
    systemctl(&["daemon-reload"])?;
    // An executor that paddock started itself holds the socket path until it
    // is idle; systemd takes the path over after that.
    let _ = std::fs::remove_file(socket_path(env));
    systemctl(&["enable", "--now", SOCKET_UNIT])?;
    Ok(files.into_iter().map(|(p, _)| p).collect())
}

/// Turn the socket off and remove the units that paddock wrote.
pub fn remove(env: &HostEnv) -> Result<Vec<PathBuf>> {
    let dir = unit_dir(env);
    let mut removed = Vec::new();
    if ours(&dir.join(SOCKET_UNIT)) {
        let _ = systemctl(&["disable", "--now", SOCKET_UNIT]);
    }
    for name in [SOCKET_UNIT, SERVICE_UNIT] {
        let p = dir.join(name);
        if ours(&p) {
            std::fs::remove_file(&p)?;
            removed.push(p);
        }
    }
    if !removed.is_empty() {
        let _ = systemctl(&["daemon-reload"]);
    }
    Ok(removed)
}

/// One line for `paddock doctor` and `paddock gateway status`.
pub fn status_line(env: &HostEnv) -> String {
    let dir = unit_dir(env);
    if !ours(&dir.join(SOCKET_UNIT)) {
        return "  ok       the gateway executor starts when a request needs it (paddock gateway install hands this to systemd)".into();
    }
    let active = std::process::Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", SOCKET_UNIT])
        .status()
        .is_ok_and(|s| s.success());
    if active {
        "  ok       systemd starts the gateway executor on the first request (socket activation)"
            .into()
    } else {
        format!("  CHECK    {SOCKET_UNIT} is installed but not active: systemctl --user enable --now {SOCKET_UNIT}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_units_name_the_same_socket_as_paddock() {
        let s = socket_unit();
        assert!(s.contains("ListenStream=%t/paddock/gatewayd.sock"));
        assert!(s.contains("SocketMode=0600"));
        let svc = service_unit(Path::new("/home/demo/.local/bin/paddock"));
        assert!(svc.contains("ExecStart=/home/demo/.local/bin/paddock gatewayd"));
        let env = HostEnv::from_parts(
            "/home/demo".into(),
            "u".into(),
            1000,
            1000,
            [("XDG_RUNTIME_DIR".to_string(), "/run/user/1000".to_string())].into(),
        );
        assert_eq!(
            socket_path(&env),
            Path::new("/run/user/1000/paddock/gatewayd.sock")
        );
    }

    #[test]
    fn without_isolation_exec_is_refused_not_run() {
        let t = tempfile::tempdir().unwrap();
        let env = HostEnv::from_parts(t.path().join("home"), "u".into(), 1, 1, Default::default());
        let key = [3u8; 32];
        let req = Request {
            v: 1,
            cap: "exec".into(),
            args: [("argv".to_string(), serde_json::json!(["touch", "ran"]))]
                .into_iter()
                .collect(),
            reason: String::new(),
        };
        let job = gateway::sign(
            &key,
            "s",
            t.path(),
            &req,
            Default::default(),
            serde_json::Value::Null,
        )
        .unwrap();
        let seen = Seen::default();
        let no = || {
            Err(
                "exec needs a systemd user instance or bwrap to run a command in isolation"
                    .to_string(),
            )
        };
        let e = handle(&env, no(), &key, &seen, &job).unwrap_err();
        assert!(format!("{e:#}").contains("isolation"), "{e:#}");
        assert!(!t.path().join("ran").exists());
        let e = handle(&env, no(), &key, &seen, &job).unwrap_err();
        assert!(format!("{e:#}").contains("used already"), "{e:#}");
    }

    /// A push in a real systemd unit (home read-only, only the scratch
    /// repository and the local remote writable), where this host has a
    /// systemd user instance. Opt in: `cargo test --lib gatewayd -- --ignored`.
    #[test]
    #[ignore]
    fn a_push_runs_in_a_systemd_unit() {
        let host = HostEnv::detect().unwrap();
        if crate::execbox::detect(&host).ok() != Some(crate::execbox::Isolation::Systemd) {
            eprintln!("skipped: no systemd user instance");
            return;
        }
        let t = tempfile::Builder::new()
            .prefix("paddock-push-")
            .tempdir_in(host.home.join(".cache"))
            .unwrap();
        let git = |dir: &Path, args: &[&str]| -> String {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(["-c", "user.name=t", "-c", "user.email=t@localhost"])
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {out:?}");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        let work = t.path().join("work");
        let remote = t.path().join("remote.git");
        std::fs::create_dir_all(&work).unwrap();
        git(
            t.path(),
            &[
                "init",
                "-q",
                "--bare",
                "-b",
                "main",
                remote.to_str().unwrap(),
            ],
        );
        git(&work, &["init", "-q", "-b", "main"]);
        git(&work, &["commit", "-q", "--allow-empty", "-m", "first"]);
        git(
            &work,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git(&work, &["push", "-q", "origin", "main"]);
        git(&work, &["commit", "-q", "--allow-empty", "-m", "second"]);
        let head = git(&work, &["rev-parse", "HEAD"]);
        let repo = crate::broker::git::Repo::snapshot(&work).unwrap();
        let run_dir = t.path().join("run");
        std::fs::create_dir_all(&run_dir).unwrap();
        let mut args = std::collections::BTreeMap::new();
        args.insert("remote".to_string(), serde_json::json!("origin"));
        args.insert(
            "url".to_string(),
            serde_json::json!(remote.to_str().unwrap()),
        );
        args.insert(
            "refs".to_string(),
            serde_json::json!([{"src": "HEAD", "sha": head, "dst": "refs/heads/main"}]),
        );
        let plan = crate::broker::git::plan(Some(&repo), &run_dir, &args).unwrap();
        let req = Request {
            v: 1,
            cap: "git-push".into(),
            args,
            reason: String::new(),
        };
        let key = [5u8; 32];
        let job = gateway::sign(
            &key,
            "s",
            &work,
            &req,
            host_env(),
            serde_json::to_value(plan.exec()).unwrap(),
        )
        .unwrap();
        let r = handle(
            &host,
            Ok(crate::execbox::Isolation::Systemd),
            &key,
            &Seen::default(),
            &job,
        )
        .unwrap();
        assert_eq!(r["refs"][0]["ok"], true, "{r}");
        assert_eq!(git(&remote, &["rev-parse", "main"]), head);
    }

    #[test]
    fn remove_leaves_units_that_paddock_did_not_write() {
        let t = tempfile::tempdir().unwrap();
        let env = HostEnv::from_parts(t.path().join("home"), "u".into(), 1, 1, Default::default());
        let dir = unit_dir(&env);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(SOCKET_UNIT), "[Socket]\nmine\n").unwrap();
        assert!(remove(&env).unwrap().is_empty());
        assert!(dir.join(SOCKET_UNIT).exists());
    }
}
