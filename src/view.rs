//! A live view of a sandbox that runs without a terminal (README "Watch a
//! sandbox without a window").
//!
//! paddock runs the sandbox as before. It copies the harness's stdout and
//! stderr byte for byte to its own stdout and stderr (or to the capture of a
//! spawn), and to a run log in `<runtime>/paddock/view/ID.log`. A tmux pane
//! on paddock's own server (`<runtime>/paddock/view.sock`, never the user's
//! server or configuration) shows that log with `tail -f`. The caller sees
//! the same bytes, the same exit code and the same stdin as without the view.
//! The runtime folder is hidden in every sandbox, so no sandbox reaches the
//! socket or the logs.

use crate::env::HostEnv;
use anyhow::{bail, Context, Result};
use std::ffi::OsString;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// `paddock gc` removes the view of a sandbox that ended this long ago.
/// `[view] keep_ended` replaces it. Default 2 hours.
pub const KEEP_ENDED: u64 = 2 * 3600;
/// The scrollback of a view pane, in lines.
pub const HISTORY_LIMIT: &str = "100000";
/// The run log stops growing at this size.
const LOG_CAP: u64 = 64 << 20;
/// `paddock ps` reads only this much of the end of a view log.
pub const ACTIVITY_TAIL: u64 = 8 << 10;
/// The activity line keeps at most this many characters.
pub const ACTIVITY_CHARS: usize = 120;
/// The capture of a spawn keeps the last 4 MiB, as without the view.
const CAPTURE_KEEP: usize = 4 << 20;

pub fn socket(env: &HostEnv) -> PathBuf {
    env.paddock_run().join("view.sock")
}
pub fn dir(env: &HostEnv) -> PathBuf {
    env.paddock_run().join("view")
}
pub fn log_file(env: &HostEnv, id: &str) -> PathBuf {
    dir(env).join(format!("{id}.log"))
}
/// Written when the sandbox ends: the last line of the pane. Its time of
/// change is the end time for `paddock gc`.
pub fn end_file(env: &HostEnv, id: &str) -> PathBuf {
    dir(env).join(format!("{id}.end"))
}

/// A sandbox id names files and tmux sessions: letters, digits, `-`, `_`.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// `[view]` of `~/.config/paddock/config.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// Show the run in a tmux pane when tmux is on PATH. Default true.
    pub tmux: bool,
    /// Drop a view this many seconds after the run ended. Default 2 hours.
    pub keep_ended: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            tmux: true,
            keep_ended: KEEP_ENDED,
        }
    }
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    tmux: Option<bool>,
    keep_ended: Option<String>,
}

impl Config {
    pub fn load(env: &HostEnv) -> Result<Self> {
        let file = env.paddock_config().join("config.toml");
        let Ok(text) = std::fs::read_to_string(&file) else {
            return Ok(Self::default());
        };
        Self::parse(&text).with_context(|| format!("[view] in {} is invalid", file.display()))
    }

    pub fn parse(text: &str) -> Result<Self> {
        let v: toml::Value = toml::from_str(text)?;
        let raw: RawConfig = match v.get("view") {
            Some(t) => t
                .clone()
                .try_into()
                .context("[view] keys: tmux = true or false, keep_ended = \"2h\"")?,
            None => RawConfig::default(),
        };
        let keep_ended = match raw.keep_ended {
            None => KEEP_ENDED,
            Some(s) => crate::timer::parse_duration(&s)
                .context("keep_ended is not a duration, for example 2h")?,
        };
        Ok(Config {
            tmux: raw.tmux.unwrap_or(true),
            keep_ended,
        })
    }
}

/// What tmux reports for the last output of a pane.
#[derive(Debug, PartialEq, Eq)]
pub enum PaneOutput {
    /// The session is gone.
    Missing,
    /// tmux did not answer. Leave the idle state as it is.
    Unknown,
    /// The pane has not produced output.
    Quiet,
    /// Unix time of the last output.
    At(u64),
}

/// One line of `#{pane_last_output_time},#{window_activity}`.
pub fn parse_last_output(line: &str) -> PaneOutput {
    let (pane, window) = line.trim().split_once(',').unwrap_or((line.trim(), ""));
    let pick = if pane.trim().is_empty() { window } else { pane };
    let pick = pick.trim();
    if pick.is_empty() {
        return PaneOutput::Quiet;
    }
    match pick.parse::<u64>() {
        Ok(0) => PaneOutput::Quiet,
        Ok(t) => PaneOutput::At(t),
        Err(_) => PaneOutput::Unknown,
    }
}

/// paddock's own tmux server.
#[derive(Debug, Clone)]
pub struct Tmux {
    pub exe: PathBuf,
    pub sock: PathBuf,
    /// PATH, HOME and the locale for the server, nothing else.
    vars: Vec<(String, String)>,
    /// `systemd-run` and the variables it needs: the server starts in a
    /// scope of its own. Without it, the server is in the cgroup of the
    /// paddock that starts it, for example the scope of a spawned child,
    /// and ends when that scope stops.
    scope: Option<(PathBuf, Vec<(String, String)>)>,
}

impl Tmux {
    /// The server of this user, when tmux is on PATH.
    pub fn find(env: &HostEnv) -> Option<Tmux> {
        let exe = env.which("tmux")?;
        let vars = ["PATH", "HOME", "LANG", "LC_ALL", "LC_CTYPE"]
            .iter()
            .filter_map(|k| env.var(k).map(|v| (k.to_string(), v.to_string())))
            .collect();
        let scope = env.which("systemd-run").and_then(|run| {
            let vars: Vec<(String, String)> = ["XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS"]
                .iter()
                .filter_map(|k| env.var(k).map(|v| (k.to_string(), v.to_string())))
                .collect();
            (vars.len() == 2).then_some((run, vars))
        });
        Some(Tmux {
            exe,
            sock: socket(env),
            vars,
            scope,
        })
    }

    fn cmd(&self) -> Command {
        let mut c = Command::new(&self.exe);
        c.arg("-S")
            .arg(&self.sock)
            .arg("-f")
            .arg("/dev/null")
            .current_dir("/")
            .env_clear()
            .envs(self.vars.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        c
    }

    fn run(&self, args: &[&str]) -> bool {
        if !self.sock.exists() {
            return false;
        }
        self.cmd().args(args).status().is_ok_and(|s| s.success())
    }

    pub fn has(&self, id: &str) -> bool {
        valid_id(id) && self.run(&["has-session", "-t", &format!("={id}")])
    }

    /// The names of the sessions of the server. Empty when no server runs.
    pub fn sessions(&self) -> Vec<String> {
        if !self.sock.exists() {
            return vec![];
        }
        let out = self
            .cmd()
            .args(["list-sessions", "-F", "#{session_name}"])
            .stdout(Stdio::piped())
            .output();
        match out {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(str::to_string)
                .collect(),
            _ => vec![],
        }
    }

    pub fn kill(&self, id: &str) -> bool {
        valid_id(id) && self.run(&["kill-session", "-t", &format!("={id}")])
    }

    /// The last lines of pane `id`. A detached run has no view log. Its
    /// output is this pane.
    pub fn pane_tail(&self, id: &str) -> Option<String> {
        if !valid_id(id) || !self.sock.exists() {
            return None;
        }
        let target = format!("={id}:");
        let out = self
            .cmd()
            .args(["capture-pane", "-p", "-t", &target, "-S", "-120"])
            .stdout(Stdio::piped())
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// When the pane last wrote output. `pane_last_output_time` is empty
    /// until the pane writes something, and tmux before 3.5 does not know it.
    /// Then `window_activity` counts: each run has one window, and tmux sets
    /// that time when the window starts and on each output.
    pub fn last_output(&self, id: &str) -> PaneOutput {
        if !valid_id(id) || !self.sock.exists() {
            return PaneOutput::Missing;
        }
        let target = format!("={id}");
        let out = self
            .cmd()
            .args([
                "list-panes",
                "-t",
                &target,
                "-F",
                "#{pane_last_output_time},#{window_activity}",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output();
        let Ok(out) = out else {
            return PaneOutput::Unknown;
        };
        if !out.status.success() {
            return PaneOutput::Missing;
        }
        let text = String::from_utf8_lossy(&out.stdout);
        parse_last_output(text.lines().next().unwrap_or(""))
    }

    /// Start the session of a sandbox: a pane that prints `header` and then
    /// follows `log` until the process `pid` (the paddock that runs the
    /// sandbox) ends, then prints `end`. The pane stays after that.
    pub fn start(&self, id: &str, header: &str, log: &Path, pid: u32, end: &Path) -> Result<()> {
        let script = "printf '%s\\n\\n' \"$1\"; tail -n +1 --pid=\"$3\" -f -- \"$2\"; \
                      cat -- \"$4\" 2>/dev/null";
        let mut cmd: Vec<OsString> = ["sh", "-c", script, "paddock-view", header]
            .iter()
            .map(OsString::from)
            .collect();
        cmd.extend([log.into(), pid.to_string().into(), end.into()]);
        self.new_session(id, &cmd)
    }

    /// Start the session `id` with one pane that runs `cmd` (no shell in
    /// between). The pane stays after `cmd` ends.
    pub fn new_session(&self, id: &str, cmd: &[OsString]) -> Result<()> {
        if !valid_id(id) {
            bail!("{id} is not a sandbox id");
        }
        if let Some(d) = self.sock.parent() {
            crate::privfs::dir(d)?;
        }
        let mut args: Vec<OsString> = [
            "start-server",
            ";",
            "set-option",
            "-g",
            "history-limit",
            HISTORY_LIMIT,
            ";",
            "set-option",
            "-g",
            "remain-on-exit",
            "on",
            ";",
            "new-session",
            "-d",
            "-c",
            "/",
            "-s",
            id,
            "-x",
            "160",
            "-y",
            "48",
        ]
        .iter()
        .map(OsString::from)
        .collect();
        args.extend(cmd.iter().cloned());
        // A new server goes into a scope of its own when it can.
        let running = self.run(&["list-sessions"]);
        if let (false, Some((run, vars))) = (running, &self.scope) {
            let ok = Command::new(run)
                .args(["--user", "--scope", "--quiet", "--collect", "--"])
                .arg(&self.exe)
                .arg("-S")
                .arg(&self.sock)
                .args(["-f", "/dev/null"])
                .args(&args)
                .current_dir("/")
                .env_clear()
                .envs(self.vars.iter().map(|(k, v)| (k, v)))
                .envs(vars.iter().map(|(k, v)| (k, v)))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success());
            if ok {
                return Ok(());
            }
        }
        let status = self
            .cmd()
            .args(&args)
            .status()
            .context("cannot start tmux")?;
        if !status.success() {
            bail!("tmux new-session failed ({status})");
        }
        Ok(())
    }

    /// The clients attached to session `id` that can type into it (not
    /// read-only): `paddock view --write`.
    pub fn write_clients(&self, id: &str) -> usize {
        if !valid_id(id) || !self.sock.exists() {
            return 0;
        }
        let out = self
            .cmd()
            .args([
                "list-clients",
                "-t",
                &format!("={id}"),
                "-F",
                "#{client_readonly}",
            ])
            .stdout(Stdio::piped())
            .output();
        match out {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter(|l| l.trim() == "0")
                .count(),
            _ => 0,
        }
    }

    /// Show `text` to the clients of session `id`, on their status line.
    pub fn message(&self, id: &str, text: &str) -> bool {
        valid_id(id)
            && self.run(&[
                "display-message",
                "-t",
                &format!("={id}"),
                "-d",
                "10000",
                text,
            ])
    }
}

/// The local time as `YYYY-MM-DD HH:MM`.
pub fn local_time(secs: u64) -> String {
    // SAFETY: localtime_r writes only into `tm`.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let t = secs as libc::time_t;
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        return secs.to_string();
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min
    )
}

pub fn header(id: &str, harness: &str, started: u64) -> String {
    format!(
        "paddock: sandbox {id}, harness {harness}, started {}. One-shot output appears at the end.",
        local_time(started)
    )
}

/// Where a copied stream goes besides the log.
pub enum Sink {
    /// A file descriptor of this process (1 or 2), looked up at each write,
    /// so a later redirect of it (a closed terminal) applies.
    Fd(i32),
    /// Kept in memory: the capture of a spawn. The last 4 MiB.
    Capture,
}

struct LogFile {
    file: Option<std::fs::File>,
    written: u64,
}

impl LogFile {
    fn write(&mut self, data: &[u8]) {
        let Some(f) = self.file.as_mut() else { return };
        let room = LOG_CAP.saturating_sub(self.written) as usize;
        let n = data.len().min(room);
        if f.write_all(&data[..n]).is_err() {
            self.file = None;
            return;
        }
        self.written += n as u64;
        if n < data.len() {
            let _ = f.write_all(b"\n[paddock: the run log is full; the view shows no more]\n");
            self.file = None;
        }
    }
}

/// Writes harness output into a view log. The relay holds one.
#[derive(Clone)]
pub struct LogSink {
    log: Arc<Mutex<LogFile>>,
}

impl LogSink {
    pub fn append(&self, data: &[u8]) {
        if let Ok(mut log) = self.log.lock() {
            log.write(data);
        }
    }
}

/// The copies of one run: one thread per stream.
pub struct Tee {
    log: Arc<Mutex<LogFile>>,
    threads: Vec<(bool, std::thread::JoinHandle<Option<Vec<u8>>>)>,
}

impl Tee {
    /// A new run log at `path` (any old one goes).
    pub fn create(path: &Path) -> Result<Tee> {
        if let Some(d) = path.parent() {
            crate::privfs::dir(d)?;
        }
        let _ = std::fs::remove_file(path);
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("cannot create {}", path.display()))?;
        Ok(Tee {
            log: Arc::new(Mutex::new(LogFile {
                file: Some(file),
                written: 0,
            })),
            threads: vec![],
        })
    }

    /// A handle the relay uses to append harness output to this log.
    pub fn sink(&self) -> LogSink {
        LogSink {
            log: self.log.clone(),
        }
    }

    /// A tee with no log and no threads, for moving the real one out of `Live`.
    fn empty() -> Tee {
        Tee {
            log: Arc::new(Mutex::new(LogFile {
                file: None,
                written: 0,
            })),
            threads: Vec::new(),
        }
    }

    /// Copy `src` to `sink` and to the log until its end. When the sink
    /// fails (the caller closed its end), the copy stops and closes `src`,
    /// so the harness gets the same EPIPE as without the view.
    pub fn copy(&mut self, mut src: impl Read + Send + 'static, sink: Sink) {
        let log = self.log.clone();
        let capture = matches!(sink, Sink::Capture);
        let h = std::thread::spawn(move || {
            let mut kept = Vec::new();
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let n = match src.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                };
                let data = &buf[..n];
                match sink {
                    Sink::Fd(fd) => {
                        if write_fd(fd, data).is_err() {
                            log.lock().unwrap().write(data);
                            break;
                        }
                    }
                    Sink::Capture => {
                        kept.extend_from_slice(data);
                        if kept.len() > 2 * CAPTURE_KEEP {
                            kept.drain(..kept.len() - CAPTURE_KEEP);
                        }
                    }
                }
                log.lock().unwrap().write(data);
            }
            if kept.len() > CAPTURE_KEEP {
                kept.drain(..kept.len() - CAPTURE_KEEP);
            }
            capture.then_some(kept)
        });
        self.threads.push((capture, h));
    }

    /// Wait for the copies after the harness ended. A capture is read to
    /// its end, as without the view. Each other stream gets `grace` of its
    /// own. A process that the harness left behind can hold one stream open,
    /// and that wait must not eat the time of the other stream.
    pub fn finish(self, grace: Duration) -> Option<Vec<u8>> {
        let mut out = None;
        for (capture, h) in self.threads {
            if capture {
                out = h.join().ok().flatten();
                continue;
            }
            let deadline = Instant::now() + grace;
            while !h.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            if h.is_finished() {
                let _ = h.join();
            }
        }
        if let Ok(mut l) = self.log.lock() {
            if let Some(f) = l.file.as_mut() {
                let _ = f.flush();
            }
        }
        out
    }
}

fn write_fd(fd: i32, mut data: &[u8]) -> std::io::Result<()> {
    while !data.is_empty() {
        // SAFETY: a write from a valid buffer to a descriptor of this process.
        let n = unsafe { libc::write(fd, data.as_ptr().cast(), data.len()) };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        data = &data[n as usize..];
    }
    Ok(())
}

/// Which streams the view copies. True means paddock tees that stream into
/// the run log. A terminal stays the harness's own. Capture mode copies
/// both streams, so a spawned child does not print on the parent terminal
/// and the view log keeps stderr.
pub fn copy_streams(capture: bool, stdout_tty: bool, stderr_tty: bool) -> (bool, bool) {
    let out_tty = !capture && stdout_tty;
    let err_tty = !capture && stderr_tty;
    (!out_tty, !err_tty)
}

/// The view of one run, from the start of the sandbox to its end.
pub struct Live {
    pub tee: Tee,
    pub tee_out: bool,
    pub tee_err: bool,
    end: PathBuf,
    pub session: bool,
    /// The server that holds `session`, so a drop can end it.
    tmux: Option<Tmux>,
    id: String,
    /// `finish` wrote the end line. Drop then leaves the session up.
    closed: bool,
}

impl Live {
    /// Prepare the view of a run without a terminal. `tee_out` and
    /// `tee_err` say which streams paddock copies: a stream that is a
    /// terminal stays the harness's own. None when nothing is copied or the
    /// log cannot be made.
    pub fn start(
        env: &HostEnv,
        id: &str,
        harness: &str,
        tee_out: bool,
        tee_err: bool,
        quiet: bool,
    ) -> Option<Live> {
        Self::open(env, id, harness, tee_out, tee_err, quiet, false)
    }

    /// The run log and the tmux pane for a relayed terminal. The relay
    /// writes the log. The harness keeps its pty.
    pub fn start_relay(env: &HostEnv, id: &str, harness: &str, quiet: bool) -> Option<Live> {
        Self::open(env, id, harness, false, false, quiet, true)
    }

    fn open(
        env: &HostEnv,
        id: &str,
        harness: &str,
        tee_out: bool,
        tee_err: bool,
        quiet: bool,
        relay: bool,
    ) -> Option<Live> {
        if !valid_id(id) || !(relay || tee_out || tee_err) {
            return None;
        }
        reap(env);
        let log = log_file(env, id);
        let end = end_file(env, id);
        let _ = std::fs::remove_file(&end);
        let tee = match Tee::create(&log) {
            Ok(t) => t,
            Err(e) => {
                log::debug!("no view for this run: {e:#}");
                return None;
            }
        };
        let cfg = Config::load(env).unwrap_or_else(|e| {
            if !quiet {
                log::warn!("{e:#}");
            }
            Config::default()
        });
        let (session, tmux) = match Tmux::find(env).filter(|_| cfg.tmux) {
            Some(t) => {
                let h = header(id, harness, crate::state::now());
                match t.start(id, &h, &log, std::process::id(), &end) {
                    Ok(()) => (true, Some(t)),
                    Err(e) => {
                        if !quiet {
                            log::warn!("no tmux view for this sandbox: {e:#}");
                        }
                        (false, None)
                    }
                }
            }
            None => (false, None),
        };
        Some(Live {
            tee,
            tee_out,
            tee_err,
            end,
            session,
            tmux,
            id: id.to_string(),
            closed: false,
        })
    }

    /// After the sandbox ended: wait for the copies and write the last
    /// line of the pane. Gives back the capture, if there is one.
    pub fn finish(mut self, code: i32) -> Option<Vec<u8>> {
        let tee = std::mem::replace(&mut self.tee, Tee::empty());
        let out = tee.finish(Duration::from_secs(2));
        let line = format!(
            "\npaddock: the sandbox ended at {} with exit code {code}.\n",
            local_time(crate::state::now())
        );
        let _ = crate::privfs::write(&self.end, line.as_bytes());
        self.closed = true;
        out
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        if self.closed {
            return;
        }
        if self.session {
            if let Some(t) = &self.tmux {
                let _ = t.kill(&self.id);
            }
        }
        if !self.end.exists() {
            let line = "\npaddock: the sandbox ended before its view finished.\n";
            let _ = crate::privfs::write(&self.end, line.as_bytes());
        }
    }
}

/// End the view of a sandbox: its tmux session. The log stays until `gc`.
pub fn end_session(env: &HostEnv, id: &str) -> bool {
    Tmux::find(env).is_some_and(|t| t.kill(id))
}

/// Remove the view of a sandbox: its session and its files.
pub fn remove(env: &HostEnv, id: &str) -> Result<()> {
    if !valid_id(id) {
        bail!("{id} is not a sandbox id");
    }
    end_session(env, id);
    for f in [log_file(env, id), end_file(env, id)] {
        match std::fs::remove_file(&f) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("remove {}", f.display())),
        }
    }
    Ok(())
}

/// When a view ended, for `stale`.
enum EndedAt {
    /// Ended at this Unix time.
    At(u64),
    /// A run record exists and the run has not ended.
    Open,
    /// No end file, no log and no run record.
    Unknown,
}

fn ended_at(env: &HostEnv, id: &str) -> EndedAt {
    let file = [end_file(env, id), log_file(env, id)]
        .iter()
        .find_map(|f| std::fs::metadata(f).ok())
        .map(|m| m.mtime().max(0) as u64);
    if let Some(t) = file {
        return EndedAt::At(t);
    }
    if let Some(rec) = crate::detach::Record::load(env, id) {
        return match rec.ended {
            Some(t) => EndedAt::At(t),
            None => EndedAt::Open,
        };
    }
    EndedAt::Unknown
}

fn keep_ended(env: &HostEnv) -> u64 {
    Config::load(env)
        .map(|c| c.keep_ended)
        .unwrap_or(KEEP_ENDED)
}

/// The views whose sandbox ended more than `[view] keep_ended` before
/// `now`: the sessions and the logs. `running` are the ids of the running
/// sandboxes. A detached run with no view log uses the end time in its
/// run record. A record that is still open stays.
pub fn stale(env: &HostEnv, sessions: &[String], running: &[String], now: u64) -> Vec<String> {
    let keep = keep_ended(env);
    let mut ids: std::collections::BTreeSet<String> =
        sessions.iter().filter(|s| valid_id(s)).cloned().collect();
    if let Ok(rd) = std::fs::read_dir(dir(env)) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if let Some(id) = name
                .strip_suffix(".log")
                .or_else(|| name.strip_suffix(".end"))
            {
                if valid_id(id) {
                    ids.insert(id.to_string());
                }
            }
        }
    }
    ids.into_iter()
        .filter(|id| !running.contains(id))
        .filter(|id| match ended_at(env, id) {
            // The end of the sandbox. Without the end line (paddock was
            // killed), the last write of the log. A session with no files
            // and no run record has no time at all: it goes.
            EndedAt::At(t) => now.saturating_sub(t) > keep,
            EndedAt::Open => false,
            EndedAt::Unknown => true,
        })
        .collect()
}

/// Remove view sessions of runs that ended longer ago than `keep_ended`.
/// The start of a new view session calls this. `paddock gc` lists the same
/// sessions and removes them when the user agrees.
pub fn reap(env: &HostEnv) {
    let now = crate::state::now();
    let sessions = Tmux::find(env).map(|t| t.sessions()).unwrap_or_default();
    let running: Vec<String> = crate::state::Running::list(env)
        .into_iter()
        .map(|r| r.id)
        .collect();
    for id in stale(env, &sessions, &running, now) {
        let _ = remove(env, &id);
    }
}

/// The `paddock gc` items: one per stale view.
pub fn gc_items(env: &HostEnv, now: u64) -> Vec<crate::storage::Item> {
    let sessions = Tmux::find(env).map(|t| t.sessions()).unwrap_or_default();
    let running: Vec<String> = crate::state::Running::list(env)
        .into_iter()
        .map(|r| r.id)
        .collect();
    stale(env, &sessions, &running, now)
        .into_iter()
        .map(|id| {
            let bytes = std::fs::metadata(log_file(env, &id))
                .map(|m| m.len())
                .unwrap_or(0);
            crate::storage::Item {
                what: format!("the view of {id} (tmux session and run log)"),
                tier: crate::storage::Tier::Auto,
                bytes,
                reason: "the sandbox ended longer ago than keep_ended".into(),
                warning: None,
                needs: vec![],
                action: crate::storage::Action::View { id },
            }
        })
        .collect()
}

/// The last visible line of the view log, for `paddock ps --json`.
///
/// Reads only the last 8 KiB. ANSI sequences and other control characters
/// go. The line is trimmed and cut at 120 characters. None when the sandbox
/// has no log, or the tail has no visible text.
///
/// A detached run keeps its output in the tmux pane, not in a view log.
/// The line then comes from that pane.
pub fn activity(env: &HostEnv, id: &str) -> Option<String> {
    if !valid_id(id) {
        return None;
    }
    if crate::detach::is_detached(env, id) {
        if let Some(tmux) = Tmux::find(env).filter(|t| t.has(id)) {
            if let Some(text) = tmux.pane_tail(id) {
                return last_visible(&text);
            }
        }
    }
    read_tail(&log_file(env, id))
        .as_deref()
        .and_then(last_visible)
}

/// Drop ANSI sequences and other control characters. The result is at most
/// `max` characters, cut on a character boundary.
pub fn plain_limit(text: &str, max: usize) -> String {
    take_chars(strip_controls(text).trim(), max)
}

fn read_tail(path: &Path) -> Option<String> {
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    if len == 0 {
        return None;
    }
    let start = len.saturating_sub(ACTIVITY_TAIL);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    if start > 0 {
        if let Some(i) = buf.iter().position(|b| *b == b'\n') {
            buf.drain(..=i);
        }
        let skip = buf
            .iter()
            .position(|b| is_utf8_start(*b))
            .unwrap_or(buf.len());
        buf.drain(..skip);
    }
    if buf.is_empty() {
        return None;
    }
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// A byte that can start a UTF-8 character. A continuation byte is `10xxxxxx`.
fn is_utf8_start(b: u8) -> bool {
    (b & 0b1100_0000) != 0b1000_0000
}

fn last_visible(text: &str) -> Option<String> {
    let mut found = None;
    // `\r` starts the line again, as on a terminal. The last piece is the
    // visible one.
    for line in text.split(['\n', '\r']) {
        let clean = strip_controls(line);
        let clean = clean.trim();
        if !clean.is_empty() {
            found = Some(take_chars(clean, ACTIVITY_CHARS));
        }
    }
    found
}

fn take_chars(text: &str, n: usize) -> String {
    if text.chars().count() <= n {
        text.to_string()
    } else {
        text.chars().take(n).collect()
    }
}

/// ANSI escape sequences and other control characters, removed.
fn strip_controls(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            skip_escape(&mut chars);
            continue;
        }
        if c == '\u{9b}' {
            skip_csi(&mut chars);
            continue;
        }
        if c.is_control() {
            continue;
        }
        out.push(c);
    }
    out
}

fn skip_escape(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    match chars.peek().copied() {
        Some('[') => {
            chars.next();
            skip_csi(chars);
        }
        Some(']') => {
            chars.next();
            while let Some(c) = chars.next() {
                if c == '\u{7}' {
                    break;
                }
                if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                    chars.next();
                    break;
                }
            }
        }
        Some('(' | ')' | '*' | '+' | '-' | '.' | '/') => {
            chars.next();
            chars.next();
        }
        Some(_) => {
            chars.next();
        }
        None => {}
    }
}

fn skip_csi(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    for c in chars.by_ref() {
        if ('\u{40}'..='\u{7e}').contains(&c) {
            break;
        }
    }
}

/// What a sandbox row can open: `(view, tmux)`. `view` is a session or a
/// run log, `tmux` a session (`paddock view --write` takes it over).
pub fn available(env: &HostEnv, sessions: &[String], id: &str) -> (bool, bool) {
    let tmux = sessions.iter().any(|s| s == id);
    (tmux || log_file(env, id).is_file(), tmux)
}

/// The inputs of `paddock view`, so a test can check the command.
pub struct Opener {
    pub xdg_terminal_exec: Option<PathBuf>,
    pub terminal: Option<String>,
    pub tmux: Option<Tmux>,
    pub log: PathBuf,
}

impl Opener {
    pub fn from_env(env: &HostEnv, id: &str) -> Opener {
        Opener {
            xdg_terminal_exec: env.which("xdg-terminal-exec"),
            terminal: env
                .var("TERMINAL")
                .filter(|t| !t.trim().is_empty())
                .map(str::to_string),
            tmux: Tmux::find(env),
            log: log_file(env, id),
        }
    }

    /// The command that opens the view of `id` in a new terminal.
    pub fn command(&self, id: &str, write: bool) -> Result<Vec<OsString>> {
        if !valid_id(id) {
            bail!("{id} is not a sandbox id");
        }
        let inner: Vec<OsString> = match &self.tmux {
            Some(t) if t.has(id) => {
                let mut v: Vec<OsString> = vec![
                    t.exe.clone().into(),
                    "-S".into(),
                    t.sock.clone().into(),
                    "attach".into(),
                ];
                if !write {
                    v.push("-r".into());
                }
                v.extend(["-t".into(), id.into()]);
                v
            }
            _ if self.log.is_file() => vec![
                "tail".into(),
                "-n".into(),
                "+1".into(),
                "-f".into(),
                "--".into(),
                self.log.clone().into(),
            ],
            Some(_) => bail!(
                "sandbox {id} has no view: it has no tmux session and no run log. A sandbox started from a terminal has no view, it uses that terminal"
            ),
            None => bail!(
                "sandbox {id} has no run log, and tmux is not installed. A sandbox started from a terminal has no view, it uses that terminal"
            ),
        };
        let mut argv: Vec<OsString> = match (&self.xdg_terminal_exec, &self.terminal) {
            (Some(x), _) => vec![x.clone().into()],
            (None, Some(t)) => vec![t.into(), "-e".into()],
            (None, None) => {
                bail!("no terminal to open: install xdg-terminal-exec or set $TERMINAL")
            }
        };
        argv.extend(inner);
        Ok(argv)
    }
}

/// `paddock view ID [--write]`: open the view in a new terminal window.
pub fn open(env: &HostEnv, id: &str, write: bool) -> Result<()> {
    if write
        && crate::state::Running::list(env)
            .iter()
            .any(|r| r.id == id && r.relay)
    {
        bail!(
            "paddock view --write is not offered for {id}. A background process started this run and owns the input"
        );
    }
    let argv = Opener::from_env(env, id).command(id, write)?;
    let mut c = Command::new(&argv[0]);
    c.args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    c.spawn()
        .with_context(|| format!("cannot start {}", argv[0].to_string_lossy()))?;
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests;
