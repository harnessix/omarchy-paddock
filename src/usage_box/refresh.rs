//! A fresh login for the usage box when no agent runs.
//!
//! Some harnesses (agy) renew their access token only while they run. The
//! usage box picks the newest copy of a login, read-only, and never renews
//! it, so after an hour without a run every copy has lapsed and the panel
//! shows no limits. A profile can name a short command in `[state] refresh`
//! that renews the login and does no model work (`agy models`).
//!
//! When the panel asks for limits and the newest copy of a login has a
//! lapsed access token and a refresh token, paddock runs that command once
//! in a normal sandbox of the profile: the shared login, network `api`, a
//! scratch project and a time limit of 30 seconds. The sandbox copies the
//! renewed login back to the login store, as after any run of the agent.
//! Then the box picks the newest copy again.
//!
//! - At most one attempt per agent in 10 minutes, recorded in paddock's
//!   state (`usage-refresh.json`).
//! - Never for an isolated login or an account folder: only the host file
//!   and copies in `homes/AGENT/shared` count.
//! - Never for a login that the credential keeper keeps (`keeper.rs`).

use super::{login_files, newest_login, token_expiry};
use crate::env::HostEnv;
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The shortest time between two attempts for one agent, in seconds.
pub const MIN_GAP: u64 = 600;
/// The time limit of one refresh run.
pub const TIME_LIMIT: Duration = Duration::from_secs(30);

/// One refresh to run: the profile and the arguments of its command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Due {
    pub harness: String,
    pub args: Vec<String>,
}

fn has_refresh_token(v: &serde_json::Value, depth: u8) -> bool {
    let Some(obj) = v.as_object() else {
        return false;
    };
    for k in ["refresh_token", "refreshToken"] {
        if obj
            .get(k)
            .and_then(|r| r.as_str())
            .is_some_and(|r| !r.trim().is_empty())
        {
            return true;
        }
    }
    depth > 0 && obj.values().any(|c| has_refresh_token(c, depth - 1))
}

/// A login whose access token lapsed at `now` and that holds a refresh
/// token. A login without a known expiry never counts as lapsed.
pub fn lapsed_with_refresh(bytes: &[u8], expiry_field: Option<&str>, now: u64) -> bool {
    let Some(exp) = token_expiry(bytes, expiry_field) else {
        return false;
    };
    exp <= now
        && serde_json::from_slice::<serde_json::Value>(bytes)
            .is_ok_and(|v| has_refresh_token(&v, 2))
}

/// The shared copies of a login: the host file, or a copy in a shared
/// sandbox home of `harness`.
fn shared_copy(env: &HostEnv, harness: &str, rel: &str, src: &Path) -> bool {
    src == env.home.join(rel)
        || src.starts_with(
            env.paddock_data()
                .join("homes")
                .join(harness)
                .join(crate::storage::login_dir(&crate::state::AuthMode::Shared)),
        )
}

/// The refreshes that the logins need at `now`, before the 10-minute rule.
pub fn due(env: &HostEnv, now: u64) -> Vec<Due> {
    let Ok(factory) = crate::profile::HarnessFactory::new(env) else {
        return Vec::new();
    };
    let mut out: Vec<Due> = Vec::new();
    for (rel, (harnesses, field)) in login_files(env) {
        let Some(src) = newest_login(env, &rel, &harnesses, field.as_deref()) else {
            continue;
        };
        if src.starts_with(env.paddock_state().join("keepers")) {
            continue;
        }
        let Ok(bytes) = std::fs::read(&src) else {
            continue;
        };
        if !lapsed_with_refresh(&bytes, field.as_deref(), now) {
            continue;
        }
        for h in &harnesses {
            if crate::keeper::kind_for(h).is_some() || out.iter().any(|d| &d.harness == h) {
                continue;
            }
            let Some(p) = factory.profile(h) else {
                continue;
            };
            if p.state.refresh.is_empty()
                || !p.state.auth.iter().any(|a| a == &rel)
                || !shared_copy(env, h, &rel, &src)
            {
                continue;
            }
            out.push(Due {
                harness: h.clone(),
                args: p.state.refresh[1..].to_vec(),
            });
        }
    }
    out
}

fn record_file(env: &HostEnv) -> PathBuf {
    env.paddock_state().join("usage-refresh.json")
}

/// The last attempt for each agent, in Unix seconds.
pub fn attempts(env: &HostEnv) -> BTreeMap<String, u64> {
    std::fs::read(record_file(env))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn record(env: &HostEnv, harness: &str, now: u64) -> Result<()> {
    let mut all = attempts(env);
    all.insert(harness.to_string(), now);
    let f = record_file(env);
    if let Some(dir) = f.parent() {
        crate::privfs::dir(dir)?;
    }
    crate::privfs::write_atomic(&f, serde_json::to_vec_pretty(&all)?)?;
    Ok(())
}

/// Held while one usage update refreshes, so two updates at the same time
/// never both run the command. None: another update holds it.
fn try_lock(env: &HostEnv) -> Option<std::fs::File> {
    use std::os::unix::io::AsRawFd;
    let run = env.paddock_run();
    crate::privfs::dir(&run).ok()?;
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(run.join("usage-refresh.lock"))
        .ok()?;
    // SAFETY: flock on a file descriptor that `f` owns.
    let r = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    (r == 0).then_some(f)
}

/// Run the due refreshes that the 10-minute rule allows, each with `runner`.
/// Returns the agents that were tried.
pub fn run_due(
    env: &HostEnv,
    now: u64,
    runner: &dyn Fn(&HostEnv, &Due) -> Result<()>,
) -> Vec<String> {
    let due = due(env, now);
    if due.is_empty() {
        return Vec::new();
    }
    let Some(_lock) = try_lock(env) else {
        return Vec::new();
    };
    let last = attempts(env);
    let mut tried = Vec::new();
    for d in due {
        if last
            .get(&d.harness)
            .is_some_and(|t| now < t.saturating_add(MIN_GAP) && *t <= now)
        {
            continue;
        }
        if let Err(e) = record(env, &d.harness, now) {
            log::warn!("cannot record the login refresh of {}: {e:#}", d.harness);
            continue;
        }
        if let Err(e) = runner(env, &d) {
            eprintln!(
                "paddock: the login refresh of {} for the usage box failed: {e:#}",
                d.harness
            );
        }
        tried.push(d.harness);
    }
    tried
}

/// The scratch project of a refresh run.
pub fn scratch(env: &HostEnv, harness: &str) -> PathBuf {
    crate::storage::scratch_root(env).join(format!("{harness}-login-refresh"))
}

/// The `paddock run` arguments of a refresh: the shared login, network
/// `api` with the hosts of the agent's collector, no auto-approval flags.
pub fn run_args(env: &HostEnv, d: &Due) -> Vec<String> {
    let mut a = vec![
        "run".to_string(),
        d.harness.clone(),
        scratch(env, &d.harness).display().to_string(),
        "--auth".into(),
        "shared".into(),
        "--network".into(),
        "api".into(),
        "--no-yolo".into(),
        "--yes".into(),
    ];
    for c in super::COLLECTORS
        .iter()
        .filter(|c| c.profile == Some(d.harness.as_str()))
    {
        for h in c.hosts {
            a.extend(["--allow".into(), (*h).to_string()]);
        }
    }
    a.push("--".into());
    a.extend(d.args.iter().cloned());
    a
}

/// Run `program ARGS` and stop it after `limit`: SIGTERM first, so paddock
/// can copy the login back, then SIGKILL after 5 more seconds.
pub fn run_with_limit(program: &Path, args: &[String], limit: Duration) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: only async-signal-safe calls between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("cannot run {}", program.display()))?;
    let start = Instant::now();
    let mut termed = false;
    loop {
        if let Some(status) = child.try_wait()? {
            if termed {
                bail!("it took longer than {} seconds", limit.as_secs());
            }
            if !status.success() {
                bail!("it ended with {status}");
            }
            return Ok(());
        }
        let waited = start.elapsed();
        if !termed && waited >= limit {
            // SAFETY: a signal to our own child.
            unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
            termed = true;
        } else if termed && waited >= limit + Duration::from_secs(5) {
            let _ = child.kill();
            let _ = child.wait();
            bail!("it took longer than {} seconds", limit.as_secs());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The runner of `run_due` in the real update: `paddock run` of the profile.
pub fn sandbox_runner(env: &HostEnv, d: &Due) -> Result<()> {
    let paddock = crate::launchers::launcher_exe(env)?;
    let dir = scratch(env, &d.harness);
    crate::privfs::dir(&dir)?;
    run_with_limit(&paddock, &run_args(env, d), TIME_LIMIT)
}

#[cfg(test)]
mod tests;
