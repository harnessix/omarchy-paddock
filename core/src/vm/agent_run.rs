//! One run of the guest agent: the paddock command in the guest, its pty or
//! stdio on the pty connection, and the exit code on the control
//! connection.

use super::agent::{self, Agent, Outlet, HOST_SOCKET_VAR, PROXY_SOCKET_VAR};
use super::{ports, proto, term};
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::fs::File;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// How long the agent waits for the pty connection after it accepted a run.
const PTY_WAIT: Duration = Duration::from_secs(20);

/// The paddock command of a run in the guest: the arguments and the whole
/// environment. `dir` is the run folder with the guest ends of the sockets.
pub fn command(
    req: &proto::RunRequest,
    home: &Path,
    dir: &Path,
) -> (Vec<String>, BTreeMap<String, String>) {
    let mut a = vec![
        if req.shell { "shell" } else { "run" }.to_string(),
        req.harness.clone(),
        req.project.display().to_string(),
        "--backend".into(),
        "bwrap".into(),
        "--network".into(),
        match req.network {
            proto::GuestNet::None => "none",
            // The guest has no network device: the sandbox reaches the
            // host proxy as in network mode api. The host proxy applies
            // the network mode of the host run.
            proto::GuestNet::Proxy => "api",
        }
        .into(),
        // The host approved the project's paddock.toml before the run.
        "--yes".into(),
    ];
    if req.no_yolo {
        a.push("--no-yolo".into());
    }
    if !req.args.is_empty() {
        a.push("--".into());
        a.extend(req.args.iter().cloned());
    }
    let mut env = BTreeMap::new();
    for (k, v) in &req.env {
        if proto::allowed_var(k) {
            env.insert(k.clone(), v.clone());
        }
    }
    let s = |p: &Path| p.display().to_string();
    env.insert("HOME".into(), s(home));
    env.insert("USER".into(), "paddock".into());
    env.insert("LOGNAME".into(), "paddock".into());
    env.insert("PATH".into(), "/usr/local/bin:/usr/bin:/bin".into());
    env.insert("XDG_RUNTIME_DIR".into(), s(&dir.join("xdg")));
    // The configuration of the run: the user profile of the request.
    env.insert("XDG_CONFIG_HOME".into(), s(&dir.join("config")));
    env.insert(HOST_SOCKET_VAR.into(), s(&dir.join("host.sock")));
    if req.network == proto::GuestNet::Proxy {
        env.insert(PROXY_SOCKET_VAR.into(), s(&dir.join("proxy.sock")));
    }
    (a, env)
}

/// Run one request. `control` is the control connection after the request
/// line.
pub fn run(agent: &Arc<Agent>, req: proto::RunRequest, mut control: File) -> Result<()> {
    let prepared = req
        .check()
        .and_then(|_| agent.mount_project(&req.project))
        .and_then(|_| agent.run_user(&req.project, req.owner))
        .and_then(|user| Ok((user, agent.home(user)?)));
    let (user, home) = match prepared {
        Ok(h) => h,
        Err(e) => {
            agent.say(&format!("run {} refused: {e:#}", req.id));
            return proto::write_line(&mut control, &proto::Reply::error(format!("{e:#}")));
        }
    };
    let pty_rx = agent.expect_pty(&req.token);
    agent.set_live(&req.token);
    let dir = agent.cfg.run_dir.join(&req.token[..16]);
    let result = start(agent, user, &req, &home, &dir, &mut control, pty_rx);
    agent.forget(&req.token);
    let _ = std::fs::remove_dir_all(&dir);
    match result {
        Ok(code) => {
            agent.say(&format!("run {} ended with code {code}", req.id));
            proto::write_line(&mut control, &proto::Reply::Exit { code })
        }
        Err(e) => {
            agent.say(&format!("run {} failed: {e:#}", req.id));
            proto::write_line(&mut control, &proto::Reply::error(format!("{e:#}")))
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn start(
    agent: &Agent,
    user: Option<(u32, u32)>,
    req: &proto::RunRequest,
    home: &Path,
    dir: &Path,
    control: &mut File,
    pty_rx: std::sync::mpsc::Receiver<OwnedFd>,
) -> Result<i32> {
    let profiles = dir.join("config/paddock/harness.d");
    for d in [
        dir.to_path_buf(),
        dir.join("xdg"),
        dir.join("config"),
        dir.join("config/paddock"),
        profiles.clone(),
    ] {
        std::fs::create_dir_all(&d).with_context(|| format!("cannot create {}", d.display()))?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700))?;
        if let Some((uid, gid)) = user {
            std::os::unix::fs::chown(&d, Some(uid), Some(gid))?;
        }
    }
    if let Some(text) = &req.profile {
        // check() allows only plain file names for the harness.
        let file = profiles.join(format!("{}.toml", req.harness));
        std::fs::write(&file, text).with_context(|| format!("cannot write {}", file.display()))?;
        if let Some((uid, gid)) = user {
            std::os::unix::fs::chown(&file, Some(uid), Some(gid))?;
        }
    }
    let t = &agent.cfg.transport;
    let _host = Outlet::start(
        t.clone(),
        dir.join("host.sock"),
        ports::BROKER,
        req.token.clone(),
        user,
    )?;
    let _proxy = match req.network {
        proto::GuestNet::Proxy => Some(Outlet::start(
            t.clone(),
            dir.join("proxy.sock"),
            ports::PROXY,
            req.token.clone(),
            user,
        )?),
        proto::GuestNet::None => None,
    };
    proto::write_line(control, &proto::Reply::ok())?;
    let conn = pty_rx
        .recv_timeout(PTY_WAIT)
        .context("the host did not connect the terminal of the run")?;

    let (args, env) = command(req, home, dir);
    let mut cmd = Command::new(&agent.cfg.paddock);
    cmd.args(&args)
        .env_clear()
        .envs(&env)
        .current_dir(&req.project);
    if let Some((uid, gid)) = user {
        cmd.uid(uid).gid(gid);
    }
    let master = if req.tty {
        let (master, slave) = term::open_pty(req.rows, req.cols)?;
        cmd.stdin(Stdio::from(agent::dup(&slave)?))
            .stdout(Stdio::from(agent::dup(&slave)?))
            .stderr(Stdio::from(slave));
        // SAFETY: setsid and TIOCSCTTY are async-signal-safe. The child
        // makes the pty its controlling terminal.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Some(master)
    } else {
        cmd.stdin(Stdio::from(agent::dup(&conn)?))
            .stdout(Stdio::from(agent::dup(&conn)?))
            .stderr(Stdio::from(agent::dup(&conn)?));
        // SAFETY: setsid is async-signal-safe. A group of its own, so that
        // a stop reaches the whole run.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        None
    };
    let mut child = cmd
        .spawn()
        .with_context(|| format!("cannot start {}", agent.cfg.paddock.display()))?;
    agent.say(&format!("run {} started, pid {}", req.id, child.id()));
    // The command holds its copies now.
    drop(cmd);

    let pid = Arc::new(AtomicI32::new(child.id() as i32));
    let done = Arc::new(AtomicBool::new(false));
    watch_control(
        control.try_clone()?,
        master.as_ref(),
        pid.clone(),
        done.clone(),
    )?;

    let output = match &master {
        Some(m) => Some(pump_pty(m, &conn)?),
        None => None,
    };
    let status = child.wait()?;
    done.store(true, Ordering::SeqCst);
    // The master gives EIO when no process holds the terminal any more.
    if let Some(out) = output {
        let _ = out.join();
    }
    // SAFETY: shutdown on an open socket fd: the host sees the end.
    unsafe { libc::shutdown(conn.as_raw_fd(), libc::SHUT_WR) };
    Ok(term::exit_code(status))
}

/// Resize lines from the host go to the pty. When the host closes the
/// control connection, the run stops.
fn watch_control(
    mut control: File,
    master: Option<&OwnedFd>,
    pid: Arc<AtomicI32>,
    done: Arc<AtomicBool>,
) -> Result<()> {
    let master = match master {
        Some(m) => Some(agent::dup(m)?),
        None => None,
    };
    std::thread::spawn(move || loop {
        match proto::read_line::<proto::Request>(&mut control) {
            Ok(Some(proto::Request::Resize { rows, cols })) => {
                if let Some(m) = &master {
                    term::set_size(m.as_raw_fd(), rows, cols);
                }
            }
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => {
                if !done.load(Ordering::SeqCst) {
                    // SAFETY: a signal to the process group of the run,
                    // which still runs.
                    unsafe { libc::kill(-pid.load(Ordering::SeqCst), libc::SIGTERM) };
                }
                return;
            }
        }
    });
    Ok(())
}

/// Copy the terminal of the run in both directions. Returns the thread of
/// the output.
fn pump_pty(master: &OwnedFd, conn: &OwnedFd) -> Result<std::thread::JoinHandle<()>> {
    let mut m_read = File::from(agent::dup(master)?);
    let mut m_write = File::from(agent::dup(master)?);
    let mut c_read = File::from(agent::dup(conn)?);
    let mut c_write = File::from(agent::dup(conn)?);
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut c_read, &mut m_write);
    });
    Ok(std::thread::spawn(move || {
        let _ = std::io::copy(&mut m_read, &mut c_write);
    }))
}

/// The folder of the run folders, as a path.
pub fn run_dir() -> PathBuf {
    PathBuf::from(agent::RUN_DIR)
}
