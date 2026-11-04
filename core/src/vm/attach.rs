//! `paddock vm-attach JOB`: the launch of a run on the VM backend. It
//! starts the VM when none runs, routes the broker and proxy ports of the
//! guest to the sockets of the run, sends the run request on the control
//! port and connects the terminal to the pty port. Its exit code is the
//! exit code of the run.

use super::{host, ports, proto, term};
use crate::backend::vm::Job;
use crate::env::HostEnv;
use anyhow::{bail, Context, Result};
use std::io::{IsTerminal, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Read the job file and remove it: it names the run token.
pub fn read_job(path: &Path) -> Result<Job> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    let _ = std::fs::remove_file(path);
    serde_json::from_str(&text).with_context(|| format!("{} is not a VM job", path.display()))
}

pub fn main(env: &HostEnv, job: &Path) -> Result<i32> {
    let mut job = read_job(job)?;
    let layout = host::Layout::new(env);
    host::ensure(env, &job.request.project)?;
    let token = job.request.token.clone();
    let _route = host::Route::add(&layout, &token, &job.broker, job.proxy.as_deref())?;
    let tty = job.request.tty && std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    job.request.tty = tty;
    if let Some((rows, cols)) = tty.then(|| term::get_size(0)).flatten() {
        job.request.rows = rows;
        job.request.cols = cols;
    }
    log::debug!(
        "vm: run {} of {} in {}",
        job.request.id,
        job.request.harness,
        job.request.project.display()
    );
    let mut control = host::connect(&layout, ports::CONTROL)?;
    proto::write_line(&mut control, &proto::Request::Run(job.request.clone()))?;
    expect_ok(&mut control)?;
    log::debug!("vm: the guest agent took the run");
    let mut pty = host::connect(&layout, ports::PTY)?;
    proto::write_line(&mut pty, &proto::Request::Attach { token })?;
    log::debug!("vm: the terminal of the run is attached");
    let code = connect_terminal(&mut control, pty, tty)?;
    Ok(code)
}

fn expect_ok(control: &mut UnixStream) -> Result<()> {
    match proto::read_line::<proto::Reply>(control)? {
        Some(proto::Reply::Ok { .. }) => Ok(()),
        Some(proto::Reply::Error { error }) => bail!("the VM refused the run: {error}"),
        Some(proto::Reply::Exit { code }) => bail!("the run ended at once with code {code}"),
        None => bail!("the VM closed the connection"),
    }
}

/// Copy the terminal in both directions until the run ends, and send
/// window size changes. Returns the exit code of the run.
pub fn connect_terminal(control: &mut UnixStream, pty: UnixStream, tty: bool) -> Result<i32> {
    let _raw = if tty {
        term::RawMode::enter(0).ok()
    } else {
        None
    };
    let done = Arc::new(AtomicBool::new(false));
    if tty {
        let mut c = control.try_clone()?;
        let d = done.clone();
        std::thread::spawn(move || {
            let mut last = term::get_size(0);
            while !d.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(250));
                let now = term::get_size(0);
                if now != last {
                    if let Some((rows, cols)) = now {
                        if proto::write_line(&mut c, &proto::Request::Resize { rows, cols })
                            .is_err()
                        {
                            return;
                        }
                    }
                    last = now;
                }
            }
        });
    }
    let mut to_guest = pty.try_clone()?;
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buf = [0u8; 16 * 1024];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if to_guest.write_all(&buf[..n]).is_err() {
                        return;
                    }
                }
            }
        }
        // The end of the input. A terminal has none: the run decides.
        if !tty {
            let _ = to_guest.shutdown(std::net::Shutdown::Write);
        }
    });
    let mut from_guest = pty;
    let mut stdout = std::io::stdout().lock();
    let mut buf = [0u8; 16 * 1024];
    loop {
        match from_guest.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if stdout
                    .write_all(&buf[..n])
                    .and_then(|_| stdout.flush())
                    .is_err()
                {
                    break;
                }
            }
        }
    }
    done.store(true, Ordering::SeqCst);
    match proto::read_line::<proto::Reply>(control)? {
        Some(proto::Reply::Exit { code }) => Ok(code),
        Some(proto::Reply::Error { error }) => bail!("the run in the VM failed: {error}"),
        _ => bail!("the VM closed the run without an exit code"),
    }
}

/// Login callbacks: a port that the host broker allowed (login.rs) is
/// forwarded from the host loopback into the guest, where paddock passes
/// it on to the sandbox. The forwarder lives as long as the guard.
pub struct Callbacks {
    stop: Arc<AtomicBool>,
}

impl Callbacks {
    pub fn start(env: &HostEnv, token: String) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        let layout = host::Layout::new(env);
        std::thread::spawn(move || {
            let mut bound = std::collections::BTreeSet::new();
            while !s.load(Ordering::SeqCst) {
                for port in crate::login::allowed_ports() {
                    if !bound.insert(port) {
                        continue;
                    }
                    match std::net::TcpListener::bind(("127.0.0.1", port)) {
                        Ok(l) => {
                            let (layout, token, s) = (layout.clone(), token.clone(), s.clone());
                            std::thread::spawn(move || forward(l, port, &layout, &token, &s));
                        }
                        Err(e) => log::warn!("login callback port {port}: {e}"),
                    }
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        });
        Self { stop }
    }
}

impl Drop for Callbacks {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn forward(
    l: std::net::TcpListener,
    port: u16,
    layout: &host::Layout,
    token: &str,
    stop: &AtomicBool,
) {
    for conn in l.incoming() {
        if stop.load(Ordering::SeqCst) || !crate::login::allowed(port) {
            return;
        }
        let Ok(conn) = conn else { continue };
        let guest = host::connect(layout, ports::CONTROL).and_then(|mut c| {
            proto::write_line(
                &mut c,
                &proto::Request::Connect {
                    token: token.to_string(),
                    port,
                },
            )?;
            expect_ok(&mut c)?;
            Ok(c)
        });
        match guest {
            Ok(g) => {
                std::thread::spawn(move || super::bridge::splice(conn.into(), g.into()));
            }
            Err(e) => log::warn!("login callback port {port}: {e:#}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_terminal_copies_both_ways_and_returns_the_code() {
        let (mut control, mut agent_control) = UnixStream::pair().unwrap();
        let (pty, mut agent_pty) = UnixStream::pair().unwrap();
        std::thread::spawn(move || {
            agent_pty.write_all(b"output").unwrap();
            drop(agent_pty);
            proto::write_line(&mut agent_control, &proto::Reply::Exit { code: 5 }).unwrap();
        });
        assert_eq!(connect_terminal(&mut control, pty, false).unwrap(), 5);
    }

    #[test]
    fn a_job_file_is_read_once() {
        let t = tempfile::tempdir().unwrap();
        let f = t.path().join("job.json");
        let job = Job {
            request: proto::RunRequest {
                v: proto::VERSION,
                token: "a".repeat(32),
                id: "x".into(),
                harness: "fake".into(),
                project: "/p".into(),
                network: proto::GuestNet::None,
                args: vec![],
                tty: false,
                rows: 0,
                cols: 0,
                env: Default::default(),
                shell: false,
                no_yolo: false,
                profile: None,
                owner: None,
            },
            broker: "/b.sock".into(),
            proxy: None,
        };
        std::fs::write(&f, serde_json::to_string(&job).unwrap()).unwrap();
        assert_eq!(read_job(&f).unwrap(), job);
        assert!(!f.exists());
        assert!(read_job(&f).is_err());
    }
}
