//! Freezing a sandbox: it uses no CPU, keeps its memory, and continues
//! where it stopped when it is thawed (suspend, level 2).
//!
//! - With a systemd user instance, every bwrap or srt sandbox runs in a
//!   scope of its own, `paddock-sb-<id>`: paddock starts the sandbox stopped,
//!   moves it into a new transient scope over D-Bus, and lets it continue.
//!   Its environment does not change. A child that `spawn` started is in
//!   the scope of its limits (`paddock-spawn-…`) already.
//! - `paddock freeze` freezes that scope with the cgroup freezer (`systemctl
//!   --user freeze`), and only a scope whose name starts with `paddock-`:
//!   never the terminal's or another program's.
//! - docker and podman sandboxes are paused with `pause` / `unpause`.
//! - Only paddock on the host freezes and thaws. Each one goes to the audit
//!   log with its reason, and paddock kv shows the state of the sandbox
//!   (`sandbox:<id>` in the project scope).
//! - The leases of a frozen sandbox in paddock kv follow `[suspend] leases`
//!   of the user configuration: `keep` (default) stops their time to live
//!   until the thaw, `release` frees them at once. Every freeze goes through
//!   `freeze` here (`paddock freeze`, hard timers, a child at its time
//!   limit), so the rule holds for each one.

use crate::env::HostEnv;
use crate::state::Running;
use anyhow::{bail, Context as _, Result};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// What happens to the leases of a sandbox while it is frozen.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Leases {
    /// The leases stay, and their time to live stops until the thaw.
    #[default]
    Keep,
    /// paddock frees the leases when it freezes the sandbox.
    Release,
}

/// `[suspend]` of the user configuration. A project file cannot set it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SuspendCfg {
    pub leases: Leases,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSuspend {
    leases: Option<Leases>,
}

impl SuspendCfg {
    pub fn load(env: &HostEnv) -> Result<Self> {
        let file = env.paddock_config().join("config.toml");
        let Ok(text) = std::fs::read_to_string(&file) else {
            return Ok(Self::default());
        };
        Self::parse(&text).with_context(|| format!("[suspend] in {} is invalid", file.display()))
    }

    pub fn parse(text: &str) -> Result<Self> {
        let v: toml::Value = toml::from_str(text)?;
        let raw: RawSuspend = match v.get("suspend") {
            Some(t) => t
                .clone()
                .try_into()
                .context("[suspend] leases must be \"keep\" or \"release\"")?,
            None => RawSuspend::default(),
        };
        Ok(Self {
            leases: raw.leases.unwrap_or_default(),
        })
    }
}

/// The scope of a sandbox.
pub fn scope_name(id: &str) -> String {
    let clean: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("paddock-sb-{clean}.scope")
}

/// A unit that paddock may freeze: one of its own scopes.
pub fn ours(unit: &str) -> bool {
    unit.starts_with("paddock-")
        && unit.ends_with(".scope")
        && !unit.contains('/')
        && unit.len() < 200
}

/// The scope of a process, from `/proc/PID/cgroup`.
pub fn scope_of(pid: u32) -> Option<String> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    let path = text.lines().find_map(|l| l.strip_prefix("0::"))?;
    let leaf = path.rsplit('/').next()?;
    leaf.ends_with(".scope").then(|| leaf.to_string())
}

/// The paddock scope that this process runs in already, for a child that
/// `spawn` started inside the scope of its limits.
pub fn own_scope() -> Option<String> {
    scope_of(std::process::id()).filter(|s| ours(s))
}

/// Put the stopped process `pid` into the new scope `unit`, over the user
/// manager's D-Bus API (what `systemd-run --scope` does for itself), and
/// wait until the kernel shows it there.
pub fn move_to_scope(env: &HostEnv, pid: u32, unit: &str, id: &str) -> Result<()> {
    use zbus::zvariant::Value;
    let conn = crate::broker::notify::session(env)?;
    let p = zbus::blocking::Proxy::new(
        &conn,
        "org.freedesktop.systemd1",
        "/org/freedesktop/systemd1",
        "org.freedesktop.systemd1.Manager",
    )?;
    let props: Vec<(&str, Value<'_>)> = vec![
        ("PIDs", Value::from(vec![pid])),
        ("CollectMode", Value::from("inactive-or-failed")),
        ("Description", Value::from(format!("paddock sandbox {id}"))),
    ];
    let aux: Vec<(&str, Vec<(&str, Value<'_>)>)> = Vec::new();
    let _: zbus::zvariant::OwnedObjectPath =
        p.call("StartTransientUnit", &(unit, "fail", props, aux))?;
    let start = Instant::now();
    while scope_of(pid).as_deref() != Some(unit) {
        if start.elapsed() > Duration::from_secs(3) {
            bail!("the sandbox did not reach the scope {unit}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

/// The first argument of the start stub.
pub const SCOPE_EXEC: &str = "__paddock-scope-exec";

/// The start stub: stop until paddock has moved this process into the
/// scope, then run the sandbox command in its place (same process, same
/// environment).
pub fn scope_exec(argv: &[std::ffi::OsString]) -> i32 {
    use std::os::unix::process::CommandExt;
    // SAFETY: raise(2) on this process.
    unsafe { libc::raise(libc::SIGSTOP) };
    let e = std::process::Command::new(&argv[0]).args(&argv[1..]).exec();
    eprintln!("paddock: cannot start {}: {e}", argv[0].to_string_lossy());
    127
}

/// Wait until the child `pid` has stopped itself (the start stub).
pub fn wait_stopped(pid: u32) -> Result<()> {
    let mut status = 0;
    // SAFETY: waitpid on our own child; WUNTRACED returns at the stop and
    // does not reap it.
    let r = unsafe { libc::waitpid(pid as i32, &mut status, libc::WUNTRACED) };
    if r != pid as i32 || !libc::WIFSTOPPED(status) {
        bail!("the sandbox did not stop before it started");
    }
    Ok(())
}

pub fn resume(pid: u32) {
    // SAFETY: a plain kill(2) on our own child.
    unsafe { libc::kill(pid as i32, libc::SIGCONT) };
}

fn frozen_file(env: &HostEnv, id: &str) -> PathBuf {
    env.paddock_run().join(id).join("frozen.json")
}

/// Since when a sandbox is frozen (Unix time), and why.
pub fn frozen(env: &HostEnv, id: &str) -> Option<(u64, String)> {
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(frozen_file(env, id)).ok()?).ok()?;
    Some((
        v["since"].as_u64()?,
        v["reason"].as_str().unwrap_or("").to_string(),
    ))
}

fn container_cli(env: &HostEnv, backend: &str) -> Option<PathBuf> {
    matches!(backend, "docker" | "podman")
        .then(|| env.which(backend))
        .flatten()
}

fn run(cmd: &mut std::process::Command) -> Result<()> {
    let out = cmd
        .stdin(std::process::Stdio::null())
        .output()
        .context("cannot run the freezer command")?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// Freeze or thaw one sandbox.
fn set(env: &HostEnv, r: &Running, freeze: bool) -> Result<()> {
    if let Some(cli) = container_cli(env, &r.backend) {
        let verb = if freeze { "pause" } else { "unpause" };
        return run(std::process::Command::new(cli).args([verb, &format!("paddock-{}", r.id)]));
    }
    let Some(unit) = r.unit.as_deref().filter(|u| ours(u)) else {
        bail!(
            "{} runs without a scope of its own (no systemd user instance when it started), so it cannot be frozen",
            r.id
        );
    };
    // Over D-Bus, like the scope itself: no systemctl, so no runtime folder
    // is needed besides the session bus.
    let conn = crate::broker::notify::session(env)?;
    let p = zbus::blocking::Proxy::new(
        &conn,
        "org.freedesktop.systemd1",
        "/org/freedesktop/systemd1",
        "org.freedesktop.systemd1.Manager",
    )?;
    let method = if freeze { "FreezeUnit" } else { "ThawUnit" };
    p.call::<_, _, ()>(method, &(unit,))
        .with_context(|| format!("{method} {unit}"))?;
    Ok(())
}

/// The state of a sandbox in paddock kv, for the other members of its
/// pasture: `sandbox:<id>` = `{"state": "suspended"|"running", "since": T}`.
/// A sandbox without a pasture has nobody to tell.
fn kv_state(env: &HostEnv, r: &Running, state: &str, now: u64) {
    let (Some(project), Some(name)) = (r.kv_project.as_deref(), r.pasture.as_deref()) else {
        return;
    };
    let write = || -> Result<()> {
        let ns = crate::kv::shard::Ns::pasture(project, name)?;
        let mut st = crate::kv::store::Store::open(&ns.file(&crate::kv::kv_root(env))?)?;
        let cfg = crate::kv::KvCfg::load(env)?;
        let value = serde_json::json!({ "state": state, "since": now }).to_string();
        st.put(
            &ns.key(),
            &format!("sandbox:{}", r.id),
            &value,
            None,
            None,
            &crate::kv::store::Writer {
                sandbox: "paddock",
                harness: "paddock",
            },
            crate::kv::store::Quota {
                bytes: cfg.project_bytes,
                keys: cfg.max_keys,
            },
            now,
        )
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        Ok(())
    };
    if let Err(e) = write() {
        log::debug!("cannot write the state of {} to paddock kv: {e:#}", r.id);
    }
}

/// Apply `[suspend] leases` to a sandbox that was just frozen. A bad
/// `[suspend]` table keeps the leases: the default, and nothing is lost.
/// Returns the leases that it released.
pub fn leases_frozen(env: &HostEnv, root: &Path, r: &Running, now: u64) -> Vec<String> {
    let rule = SuspendCfg::load(env).unwrap_or_else(|e| {
        log::warn!("{e:#}; the leases of {} stay", r.id);
        SuspendCfg::default()
    });
    match rule.leases {
        Leases::Keep => {
            if let Err(e) = crate::kv::pause_leases(root, &r.id, now) {
                log::warn!("cannot pause the leases of {}: {e:#}", r.id);
            }
            Vec::new()
        }
        Leases::Release => {
            let cfg = crate::kv::KvCfg::load(env).unwrap_or_default();
            match crate::kv::release_leases(root, &r.id, &cfg, now) {
                Ok(keys) => keys
                    .into_iter()
                    .map(|(scope, key)| {
                        // Key names only: never a value.
                        audit(
                            env,
                            r,
                            "lease released",
                            &format!("{key} ({scope} scope): the sandbox is frozen"),
                        );
                        key
                    })
                    .collect(),
                Err(e) => {
                    log::warn!("cannot release the leases of {}: {e:#}", r.id);
                    Vec::new()
                }
            }
        }
    }
}

/// A thawed sandbox: its paused leases run again with the time they had.
pub fn leases_thawed(root: &Path, id: &str, now: u64) {
    if let Err(e) = crate::kv::resume_leases(root, id, now) {
        log::warn!("cannot resume the leases of {id}: {e:#}");
    }
}

fn audit(env: &HostEnv, r: &Running, decision: &str, reason: &str) {
    crate::broker::audit_line(
        env,
        &serde_json::json!({
            "time": crate::state::now(),
            "sandbox": r.id,
            "harness": r.harness,
            "project": r.project,
            "capability": "freeze",
            "decision": decision,
            "detail": crate::broker::clean_text(reason),
            "by": "user",
        }),
    );
}

/// `paddock freeze`: freeze the sandbox, and with `tree` the sandboxes that
/// it started. Returns the ids that it froze.
pub fn freeze(env: &HostEnv, id: &str, tree: bool, reason: &str) -> Result<Vec<String>> {
    crate::update::refuse_in_sandbox(env, "paddock freeze")?;
    let all = Running::list(env);
    let r = all
        .iter()
        .find(|r| r.id == id)
        .with_context(|| format!("no running sandbox with id {id}"))?;
    let mut targets = vec![r.clone()];
    if tree {
        targets.extend(crate::state::descendants(&all, id));
    }
    let now = crate::state::now();
    let mut done = Vec::new();
    for t in &targets {
        if frozen(env, &t.id).is_some() {
            continue;
        }
        set(env, t, true).with_context(|| format!("cannot freeze {}", t.id))?;
        crate::privfs::write(
            &frozen_file(env, &t.id),
            serde_json::json!({ "since": now, "reason": reason }).to_string(),
        )?;
        kv_state(env, t, "suspended", now);
        audit(env, t, "frozen", reason);
        leases_frozen(env, &crate::kv::kv_root(env), t, now);
        done.push(t.id.clone());
    }
    Ok(done)
}

/// `paddock thaw`: thaw the sandbox, and with `tree` its descendants.
pub fn thaw(env: &HostEnv, id: &str, tree: bool) -> Result<Vec<String>> {
    crate::update::refuse_in_sandbox(env, "paddock thaw")?;
    let all = Running::list(env);
    let r = all
        .iter()
        .find(|r| r.id == id)
        .with_context(|| format!("no running sandbox with id {id}"))?;
    let mut targets = vec![r.clone()];
    if tree {
        targets.extend(crate::state::descendants(&all, id));
    }
    let now = crate::state::now();
    let mut done = Vec::new();
    for t in &targets {
        let Some((since, _)) = frozen(env, &t.id) else {
            continue;
        };
        set(env, t, false).with_context(|| format!("cannot thaw {}", t.id))?;
        // Timers do not count the frozen time.
        crate::timer::record_frozen(env, &t.id, since, now);
        let _ = std::fs::remove_file(frozen_file(env, &t.id));
        leases_thawed(&crate::kv::kv_root(env), &t.id, now);
        kv_state(env, t, "running", now);
        audit(
            env,
            t,
            "thawed",
            &format!("after {} seconds", now.saturating_sub(since)),
        );
        done.push(t.id.clone());
    }
    Ok(done)
}

/// For a path in a message.
pub fn tilde(env: &HostEnv, p: &Path) -> String {
    p.display()
        .to_string()
        .replacen(&env.home.display().to_string(), "~", 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_paddock_scopes_are_frozen() {
        assert!(ours("paddock-sb-claude-x_1.scope"));
        assert!(ours("paddock-spawn-abc.scope"));
        assert!(!ours("app-com.anthropic.Claude-2408241.scope"));
        assert!(!ours("paddock-sb-x.service"));
        assert!(!ours("paddock-sb/../x.scope"));
        assert_eq!(
            scope_name("claude-repo-8a48be"),
            "paddock-sb-claude-repo-8a48be.scope"
        );
        assert_eq!(scope_name("a b/c"), "paddock-sb-a_b_c.scope");
    }

    #[test]
    fn suspend_config_parses_both_lease_rules() {
        assert_eq!(SuspendCfg::parse("").unwrap().leases, Leases::Keep);
        assert_eq!(
            SuspendCfg::parse("[suspend]\n").unwrap().leases,
            Leases::Keep
        );
        assert_eq!(
            SuspendCfg::parse("[suspend]\nleases = \"keep\"\n")
                .unwrap()
                .leases,
            Leases::Keep
        );
        assert_eq!(
            SuspendCfg::parse("[suspend]\nleases = \"release\"\n")
                .unwrap()
                .leases,
            Leases::Release
        );
        assert!(SuspendCfg::parse("[suspend]\nleases = \"drop\"\n").is_err());
        assert!(SuspendCfg::parse("[suspend]\nother = 1\n").is_err());
    }

    #[test]
    fn this_test_runs_outside_a_paddock_scope() {
        // The terminal's or the test runner's scope is never paddock's.
        assert_eq!(own_scope(), None);
    }
}
