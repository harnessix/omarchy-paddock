//! `paddock run --detach`: the harness runs its interactive interface on a
//! real terminal, a pane of paddock's own tmux server (view.rs), with the
//! task as its first prompt. The user attaches with `paddock view ID
//! --write` and takes over. README "Watch a sandbox without a window".
//!
//! - The caller writes a job (its folder, its environment, the run flags)
//!   and the prompt into `<runtime>/paddock/detach/`, which no sandbox
//!   sees, and starts the tmux session `ID` with `paddock detach-run JOB`.
//!   That process takes the caller's environment and folder and runs
//!   `paddock run` as the user would in that pane.
//! - The harness gets the `[detach]` args of its profile. `{prompt_file}`
//!   is a read-only copy of the prompt in the sandbox home.
//! - `paddock-host done` inside the sandbox records the end of the task in
//!   the run record and ends the run, unless a client with write access is
//!   attached: then the user has the run.
//! - `paddock wait ID` blocks until the run ends.

use crate::env::HostEnv;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// The `done` text keeps at most this many bytes.
pub const MAX_TEXT: usize = 4096;
/// The largest prompt.
pub const MAX_PROMPT: u64 = 1 << 20;
/// `paddock gc` is not needed: launch removes run records older than this.
const RECORD_AGE: u64 = 30 * 24 * 3600;
/// A run whose pane never started its paddock counts as ended after this.
const START_GRACE: u64 = 20;
/// The exit code of `paddock wait` when the time ran out.
pub const EXIT_TIMEOUT: i32 = 124;

pub const NOTICE_WRITER: &str = "paddock: the agent says it is done; close the view to end the run";

/// Where the jobs and prompts wait for their pane: hidden in every sandbox.
pub fn jobs_dir(env: &HostEnv) -> PathBuf {
    env.paddock_run().join("detach")
}
fn records_dir(env: &HostEnv) -> PathBuf {
    env.paddock_state().join("detach")
}
pub fn record_file(env: &HostEnv, id: &str) -> PathBuf {
    records_dir(env).join(format!("{id}.json"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ok,
    Failed,
}

impl std::str::FromStr for Status {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "ok" => Ok(Status::Ok),
            "failed" => Ok(Status::Failed),
            _ => bail!("the status is ok or failed, not {s:?}"),
        }
    }
}

impl Status {
    pub fn name(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Done {
    pub status: Status,
    #[serde(default)]
    pub text: String,
    pub at: u64,
}

/// The run record of a detached run. It stays after the run, so `paddock
/// wait` reads the end.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Record {
    pub id: String,
    pub harness: String,
    pub project: PathBuf,
    pub started: u64,
    /// The paddock in the pane, once it runs.
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub ended: Option<u64>,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub done: Option<Done>,
}

static RECORD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

impl Record {
    pub fn load(env: &HostEnv, id: &str) -> Option<Record> {
        if !crate::view::valid_id(id) {
            return None;
        }
        let bytes = std::fs::read(record_file(env, id)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    pub fn save(&self, env: &HostEnv) -> Result<()> {
        crate::privfs::write_atomic(&record_file(env, &self.id), serde_json::to_vec(self)?)
            .with_context(|| format!("cannot write the run record of {}", self.id))
    }

    /// Change the record of `id`, if there is one.
    pub fn update(env: &HostEnv, id: &str, f: impl FnOnce(&mut Record)) -> Result<()> {
        let _g = RECORD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let Some(mut r) = Self::load(env, id) else {
            bail!("sandbox {id} has no run record")
        };
        f(&mut r);
        r.save(env)
    }
}

/// A sandbox is a detached run.
pub fn is_detached(env: &HostEnv, id: &str) -> bool {
    crate::view::valid_id(id) && record_file(env, id).is_file()
}

/// Keep `text` printable and at most `MAX_TEXT` bytes: no terminal control
/// sequences reach the user's terminal through `paddock wait`.
pub fn clean_text(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        let c = if c.is_control() && c != '\n' && c != '\t' {
            ' '
        } else {
            c
        };
        if out.len() + c.len_utf8() > MAX_TEXT {
            break;
        }
        out.push(c);
    }
    out
}

/// What the pane runs: written by the caller, read once by `detach-run`.
#[derive(Debug, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    /// The flags of `paddock run` (main.rs RunArgs), without --detach.
    pub run: serde_json::Value,
}

/// The variables that the pane sets itself: the terminal is tmux's.
const PANE_VARS: &[&str] = &["TERM", "TMUX", "TMUX_PANE"];

/// The caller's side of `--detach`: write the job, the prompt and the run
/// record, and start the session `id` on paddock's tmux server.
pub fn launch(
    env: &HostEnv,
    tmux: &crate::view::Tmux,
    job: &Job,
    harness: &str,
    project: &Path,
    prompt: &[u8],
) -> Result<()> {
    if !crate::view::valid_id(&job.id) {
        bail!("{} is not a sandbox id", job.id);
    }
    if tmux.has(&job.id) {
        bail!("a tmux session {} exists already", job.id);
    }
    prune(env);
    let dir = jobs_dir(env);
    crate::privfs::dir(&dir)?;
    let job_file = dir.join(format!("{}.job", job.id));
    let prompt_file = dir.join(format!("{}.prompt", job.id));
    crate::privfs::write(&prompt_file, prompt)?;
    crate::privfs::write(&job_file, serde_json::to_vec(job)?)?;
    let rec = Record {
        id: job.id.clone(),
        harness: harness.to_string(),
        project: project.to_path_buf(),
        started: crate::state::now(),
        ..Default::default()
    };
    let started = rec.save(env).and_then(|()| {
        let me = std::env::current_exe().context("cannot find the paddock executable")?;
        let cmd: Vec<OsString> = vec![me.into(), "detach-run".into(), job_file.clone().into()];
        tmux.new_session(&job.id, &cmd)
    });
    if let Err(e) = started {
        let _ = std::fs::remove_file(&job_file);
        let _ = std::fs::remove_file(&prompt_file);
        let _ = std::fs::remove_file(record_file(env, &job.id));
        return Err(e);
    }
    Ok(())
}

/// Remove the run records of runs that ended long ago.
fn prune(env: &HostEnv) {
    let Ok(rd) = std::fs::read_dir(records_dir(env)) else {
        return;
    };
    let now = crate::state::now();
    for e in rd.flatten() {
        let old = e
            .metadata()
            .map(|m| now.saturating_sub(m.mtime().max(0) as u64) > RECORD_AGE)
            .unwrap_or(false);
        if old {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// The job that the pane runs, with its prompt. Both files go at once.
/// Before anything reads the environment: this process then takes the
/// caller's environment and folder.
pub fn take_job(job_file: &Path) -> Result<(Job, Vec<u8>)> {
    let read_private = |p: &Path| -> Result<Vec<u8>> {
        let m =
            std::fs::symlink_metadata(p).with_context(|| format!("cannot read {}", p.display()))?;
        // SAFETY: getuid has no preconditions.
        let me = unsafe { libc::getuid() };
        if !m.is_file() || m.uid() != me || m.mode() & 0o077 != 0 || m.len() > 2 * MAX_PROMPT {
            bail!("{} is not a private file of this user", p.display());
        }
        let b = std::fs::read(p)?;
        let _ = std::fs::remove_file(p);
        Ok(b)
    };
    let job: Job = serde_json::from_slice(&read_private(job_file)?)?;
    if !crate::view::valid_id(&job.id) {
        bail!("{} is not a sandbox id", job.id);
    }
    let prompt = read_private(&job_file.with_extension("prompt"))?;
    // The caller's environment, with the terminal of the pane.
    let term = std::env::var("TERM").ok();
    for (k, _) in std::env::vars_os() {
        std::env::remove_var(k);
    }
    for (k, v) in &job.env {
        if !PANE_VARS.contains(&k.as_str()) && !k.is_empty() && !k.contains('=') {
            std::env::set_var(k, v);
        }
    }
    if let Some(t) = term {
        std::env::set_var("TERM", t);
    }
    std::env::set_current_dir(&job.cwd)
        .with_context(|| format!("cannot enter {}", job.cwd.display()))?;
    Ok((job, prompt))
}

/// The caller's environment for a job: everything that is valid UTF-8.
pub fn caller_env() -> Vec<(String, String)> {
    std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
        .filter(|(k, _)| !PANE_VARS.contains(&k.as_str()))
        .collect()
}

/// The host side of `paddock-host done` in one detached run.
pub struct Ctl {
    pub env: HostEnv,
    pub id: String,
    notified: AtomicBool,
    watching: AtomicBool,
    /// How the run ends. The real one signals this process, as `paddock
    /// stop` signals the sandbox: SIGTERM, then the normal end steps.
    stop: Box<dyn Fn() + Send + Sync>,
}

impl Ctl {
    pub fn new(env: HostEnv, id: String) -> Ctl {
        let (e, i) = (env.clone(), id.clone());
        Ctl::with_stop(
            env,
            id,
            Box::new(move || {
                let _ = crate::freeze::thaw(&e, &i, true);
                // SAFETY: a plain kill(2) on this process. run.rs passes
                // SIGTERM on to the sandbox and runs the end steps.
                unsafe { libc::kill(libc::getpid(), libc::SIGTERM) };
            }),
        )
    }

    pub fn with_stop(env: HostEnv, id: String, stop: Box<dyn Fn() + Send + Sync>) -> Ctl {
        Ctl {
            env,
            id,
            notified: AtomicBool::new(false),
            watching: AtomicBool::new(false),
            stop,
        }
    }

    fn tmux(&self) -> Option<crate::view::Tmux> {
        crate::view::Tmux::find(&self.env)
    }

    /// The agent says the task ended. Records it, then ends the run or
    /// leaves it to the user who is attached. Returns what the agent reads.
    pub fn done(self: &std::sync::Arc<Self>, status: Status, text: &str) -> Result<String> {
        let d = Done {
            status,
            text: clean_text(text),
            at: crate::state::now(),
        };
        Record::update(&self.env, &self.id, |r| r.done = Some(d))?;
        let writers = self.tmux().map_or(0, |t| t.write_clients(&self.id));
        if writers > 0 {
            if !self.notified.swap(true, Ordering::SeqCst) {
                if let Some(t) = self.tmux() {
                    t.message(&self.id, NOTICE_WRITER);
                }
            }
            if !self.watching.swap(true, Ordering::SeqCst) {
                let me = self.clone();
                std::thread::spawn(move || me.end_when_view_closes());
            }
            return Ok(
                "recorded. The user is attached to this run with write access, so the run stays open until they close the view."
                    .into(),
            );
        }
        let me = self.clone();
        std::thread::spawn(move || {
            // The answer reaches the agent first.
            std::thread::sleep(Duration::from_millis(300));
            (me.stop)();
        });
        Ok("recorded. paddock ends this run now.".into())
    }

    /// After a `done` with a writer attached: end the run when no client
    /// with write access is left.
    fn end_when_view_closes(&self) {
        loop {
            std::thread::sleep(Duration::from_millis(500));
            if self.tmux().map_or(0, |t| t.write_clients(&self.id)) == 0 {
                (self.stop)();
                return;
            }
        }
    }
}

fn alive(pid: u32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    let r = unsafe { libc::kill(pid as i32, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// The run ended, by its record, its process or its missing pane.
fn ended(env: &HostEnv, id: &str, rec: Option<&Record>, now: u64) -> bool {
    let running = crate::state::Running::list(env).iter().any(|r| r.id == id);
    match rec {
        None => !running,
        Some(r) if r.ended.is_some() => true,
        Some(r) => match r.pid {
            Some(pid) => !alive(pid) && !running,
            None => {
                let pane = crate::view::Tmux::find(env).is_some_and(|t| t.has(id));
                !pane && now.saturating_sub(r.started) > START_GRACE
            }
        },
    }
}

/// `paddock wait ID [--timeout]`: block until the run ends. Prints the
/// `done` status and text. 0 for ok, 1 for failed or no `done`, 124 when
/// the time ran out.
pub fn wait(env: &HostEnv, id: &str, timeout: Option<Duration>) -> Result<i32> {
    let known = |env: &HostEnv| {
        Record::load(env, id).is_some()
            || crate::state::Running::list(env).iter().any(|r| r.id == id)
    };
    if !crate::view::valid_id(id) || !known(env) {
        bail!("no sandbox with id {id}: `paddock ps` lists the running ones");
    }
    let start = Instant::now();
    loop {
        let rec = Record::load(env, id);
        if ended(env, id, rec.as_ref(), crate::state::now()) {
            // The last write of the record can come just after the process.
            let rec = Record::load(env, id).or(rec);
            return Ok(report(id, rec.as_ref()));
        }
        if timeout.is_some_and(|t| start.elapsed() >= t) {
            eprintln!("paddock: sandbox {id} still runs");
            return Ok(EXIT_TIMEOUT);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn report(id: &str, rec: Option<&Record>) -> i32 {
    match rec.and_then(|r| r.done.as_ref()) {
        Some(d) => {
            if d.text.is_empty() {
                println!("{}", d.status.name());
            } else {
                println!("{}: {}", d.status.name(), d.text);
            }
            match d.status {
                Status::Ok => 0,
                Status::Failed => 1,
            }
        }
        None => {
            eprintln!("paddock: sandbox {id} ended without `paddock-host done`");
            1
        }
    }
}

#[cfg(test)]
mod tests;
