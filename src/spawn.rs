//! `paddock-host spawn`: a sandbox asks the host to start a sibling sandbox
//! of another harness, with one task. The child never runs inside the
//! parent. The host starts it with the same `paddock` code as any other
//! sandbox, and the parent only gets results back.
//!
//! The rules:
//!
//! - A child never gets more than its parent. The `Ceiling` records what the
//!   parent had (network, lan, environment, mounts, backend, host modes and
//!   the project), and `Ceiling::check` refuses a child spec that is wider.
//!   The child gets no display.
//! - The limits come only from the user configuration (`[host] spawn` and
//!   `[spawn]`). A project `paddock.toml` cannot set them.
//! - No credentials move between sandboxes. The child logs in with its own
//!   harness login, in its own sandbox home. The parent receives the branch
//!   name, a diff summary and the final output in `.paddock/inbox/`.
//! - The depth and the number of running children are counted on the host.
//!   A child stops when its parent ends, when `paddock stop` stops the
//!   parent, and after a wall-clock timeout.
//! - The prompt never goes into the arguments of a process. It reaches the
//!   harness on stdin or as a file in the child's sandbox home.

use crate::broker::Mode;
use crate::build::Built;
use crate::env::HostEnv;
use crate::manifest::Manifest;
use crate::spec::{GuiMode, MountKind, Network};
use crate::state;
use anyhow::{bail, Context as _, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

pub const DEFAULT_MAX_RUNNING: u32 = 2;
pub const DEFAULT_MAX_DEPTH: u32 = 1;
/// Seconds that a child may run in total.
pub const DEFAULT_TIMEOUT: u64 = 1800;
/// The largest prompt file.
pub const MAX_PROMPT: u64 = 256 * 1024;
/// How much of the final output reaches the parent: the end of it.
pub const MAX_OUTPUT: usize = 64 * 1024;
/// How much of the diff summary reaches the parent.
const MAX_STAT: usize = 16 * 1024;
/// How much of the prompt the approval card shows.
pub const PROMPT_SHOWN: usize = 1000;
/// The prompt file in the child's sandbox home, for profiles that read it
/// from a file.
pub const PROMPT_IN_HOME: &str = ".paddock-spawn/prompt.md";
/// How long a stopped child gets to end before paddock kills it.
const STOP_GRACE: Duration = Duration::from_secs(15);

/// `[spawn]` in the user configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SpawnCfg {
    /// The harnesses that a sandbox may start. Empty: none.
    pub harnesses: Vec<String>,
    /// Children of one parent that run at the same time.
    pub max_running: u32,
    /// How many levels deep children can go. 1: a child cannot spawn.
    pub max_depth: u32,
    /// Seconds that one child may run in total. A spawn can ask for less.
    pub timeout: u64,
    /// The most memory of one child, in bytes. None: no limit.
    pub max_mem: Option<u64>,
    /// The most CPU time of one child, in percent of one CPU (150: 1.5
    /// CPUs). None: no limit.
    pub max_cpu_percent: Option<u32>,
}

impl Default for SpawnCfg {
    fn default() -> Self {
        Self {
            harnesses: Vec::new(),
            max_running: DEFAULT_MAX_RUNNING,
            max_depth: DEFAULT_MAX_DEPTH,
            timeout: DEFAULT_TIMEOUT,
            max_mem: None,
            max_cpu_percent: None,
        }
    }
}

/// A fixed set of limits for a child. paddock chooses the set; the agent
/// only names it, so it cannot combine its own.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Its own writable worktree, a clean environment.
    #[default]
    Implementer,
    /// The project read-only, a clean environment, no push and no forge
    /// writes. It reads and reports.
    Reviewer,
}

impl std::str::FromStr for Role {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "implementer" => Ok(Self::Implementer),
            "reviewer" => Ok(Self::Reviewer),
            _ => bail!("unknown role '{s}': use implementer or reviewer"),
        }
    }
}

impl Role {
    pub fn name(self) -> &'static str {
        match self {
            Role::Implementer => "implementer",
            Role::Reviewer => "reviewer",
        }
    }

    /// What the role means, for the approval card.
    pub fn describe(self) -> &'static str {
        match self {
            Role::Implementer => "implementer: its own writable worktree, a clean environment",
            Role::Reviewer => {
                "reviewer: the files read-only, a clean environment, no push and no forge writes"
            }
        }
    }
}

/// The time and resource limits of one child.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    /// Seconds in total.
    pub timeout: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_percent: Option<u32>,
}

impl Limits {
    pub fn describe(&self) -> String {
        let mut v = vec![format!("at most {} seconds", self.timeout)];
        if let Some(m) = self.mem {
            v.push(format!("memory {}", format_bytes(m)));
        }
        if let Some(c) = self.cpu_percent {
            v.push(format!("{} CPUs", format_cpus(c)));
        }
        v.join(", ")
    }
}

/// Parse a size such as 512M, 4G or 1073741824.
pub fn parse_bytes(s: &str) -> Result<u64> {
    let s = s.trim();
    let (num, mul) = match s.char_indices().last() {
        Some((i, c)) if c.is_ascii_alphabetic() => {
            let mul: u64 = match c.to_ascii_uppercase() {
                'K' => 1 << 10,
                'M' => 1 << 20,
                'G' => 1 << 30,
                'T' => 1 << 40,
                _ => bail!("{s}: use a size such as 512M or 4G"),
            };
            (&s[..i], mul)
        }
        _ => (s, 1),
    };
    let n: u64 = num
        .parse()
        .with_context(|| format!("{s}: use a size such as 512M or 4G"))?;
    let b = n
        .checked_mul(mul)
        .with_context(|| format!("{s} is too large"))?;
    if b < 64 << 20 {
        bail!("{s} is too small: a harness needs at least 64M");
    }
    Ok(b)
}

pub fn format_bytes(b: u64) -> String {
    for (unit, size) in [("T", 1u64 << 40), ("G", 1 << 30), ("M", 1 << 20)] {
        if b >= size && b.is_multiple_of(size) {
            return format!("{}{unit}", b / size);
        }
    }
    format!("{}M", b >> 20)
}

/// Parse a CPU count such as 2 or 1.5 into percent of one CPU.
pub fn parse_cpus(s: &str) -> Result<u32> {
    let f: f64 = s
        .trim()
        .parse()
        .with_context(|| format!("{s}: use a number of CPUs such as 2 or 0.5"))?;
    if !(0.1..=256.0).contains(&f) {
        bail!("{s}: use a number of CPUs from 0.1 to 256");
    }
    Ok((f * 100.0).round() as u32)
}

pub fn format_cpus(p: u32) -> String {
    if p.is_multiple_of(100) {
        format!("{}", p / 100)
    } else {
        format!("{:.2}", p as f64 / 100.0)
            .trim_end_matches('0')
            .to_string()
    }
}

/// Parse a duration such as 90, 90s, 30m or 2h into seconds.
pub fn parse_duration(s: &str) -> Result<u64> {
    let s = s.trim();
    let (num, mul) = match s.chars().last() {
        Some('s') => (&s[..s.len() - 1], 1),
        Some('m') => (&s[..s.len() - 1], 60),
        Some('h') => (&s[..s.len() - 1], 3600),
        _ => (s, 1),
    };
    let n: u64 = num
        .parse()
        .with_context(|| format!("{s}: use a time such as 600, 30m or 2h"))?;
    n.checked_mul(mul)
        .with_context(|| format!("{s} is too long"))
}

fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

impl SpawnCfg {
    /// Read `[spawn]` from the user configuration. Never from a project file.
    pub fn load(env: &HostEnv) -> Result<Self> {
        let file = env.paddock_config().join("config.toml");
        let Ok(text) = std::fs::read_to_string(&file) else {
            return Ok(Self::default());
        };
        let doc: toml::Value =
            toml::from_str(&text).with_context(|| format!("{} is invalid", file.display()))?;
        Self::from_doc(&doc)
    }

    pub fn from_doc(doc: &toml::Value) -> Result<Self> {
        let mut c = Self::default();
        let Some(t) = doc.get("spawn").and_then(|s| s.as_table()) else {
            return Ok(c);
        };
        let int = |k: &str, v: &toml::Value, lo: i64, hi: i64| {
            v.as_integer()
                .filter(|n| (lo..=hi).contains(n))
                .with_context(|| format!("[spawn] {k} must be a number from {lo} to {hi}"))
        };
        for (k, v) in t {
            match k.as_str() {
                "harnesses" => {
                    let list = v
                        .as_array()
                        .context("[spawn] harnesses must be a list of harness names")?;
                    c.harnesses = list
                        .iter()
                        .map(|h| {
                            h.as_str()
                                .filter(|s| valid_name(s))
                                .map(str::to_string)
                                .context("[spawn] harnesses must be a list of harness names")
                        })
                        .collect::<Result<_>>()?;
                }
                "max_running" => c.max_running = int(k, v, 1, 8)? as u32,
                "max_depth" => c.max_depth = int(k, v, 0, 4)? as u32,
                "timeout" => c.timeout = int(k, v, 60, 86_400)? as u64,
                "max_mem" => {
                    let b = match v {
                        toml::Value::String(s) => parse_bytes(s),
                        toml::Value::Integer(n) if *n > 0 => parse_bytes(&n.to_string()),
                        _ => bail!("[spawn] max_mem must be a size such as \"4G\""),
                    };
                    c.max_mem = Some(b.context("[spawn] max_mem")?);
                }
                "max_cpus" => {
                    let s = match v {
                        toml::Value::Integer(n) => n.to_string(),
                        toml::Value::Float(f) => f.to_string(),
                        _ => bail!("[spawn] max_cpus must be a number such as 2"),
                    };
                    c.max_cpu_percent = Some(parse_cpus(&s).context("[spawn] max_cpus")?);
                }
                other => bail!(
                    "[spawn] {other}: unknown setting. Known: harnesses, max_running, max_depth, timeout, max_mem, max_cpus"
                ),
            }
        }
        Ok(c)
    }

    /// The limits for one child. A spawn can ask for less than the user
    /// configuration allows, never for more. Without a request, the child
    /// gets the configured limit.
    pub fn limits(
        &self,
        timeout: Option<u64>,
        mem: Option<u64>,
        cpu_percent: Option<u32>,
    ) -> Result<Limits> {
        if let Some(t) = timeout {
            if t == 0 || t > self.timeout {
                bail!(
                    "--timeout {t} is more than [spawn] timeout = {} of the user configuration",
                    self.timeout
                );
            }
        }
        if let (Some(m), Some(max)) = (mem, self.max_mem) {
            if m > max {
                bail!(
                    "--mem {} is more than [spawn] max_mem = {}",
                    format_bytes(m),
                    format_bytes(max)
                );
            }
        }
        if let (Some(c), Some(max)) = (cpu_percent, self.max_cpu_percent) {
            if c > max {
                bail!(
                    "--cpus {} is more than [spawn] max_cpus = {}",
                    format_cpus(c),
                    format_cpus(max)
                );
            }
        }
        Ok(Limits {
            timeout: timeout.unwrap_or(self.timeout),
            mem: mem.or(self.max_mem),
            cpu_percent: cpu_percent.or(self.max_cpu_percent),
        })
    }

    pub fn allows(&self, harness: &str) -> bool {
        self.harnesses.iter().any(|h| h == harness)
    }

    /// Whether a sandbox at `depth` (0: started by the user) can start a child.
    pub fn depth_allows(&self, depth: u32) -> bool {
        depth < self.max_depth
    }
}

pub fn network_rank(n: Network) -> u8 {
    match n {
        Network::None => 0,
        Network::Allowlist => 1,
        Network::Egress => 2,
        Network::Host => 3,
    }
}

pub fn mode_rank(m: Mode) -> u8 {
    match m {
        Mode::Deny => 0,
        Mode::Ask => 1,
        Mode::Allow => 2,
    }
}

/// The narrower of two modes.
pub fn narrower(a: Mode, b: Mode) -> Mode {
    if mode_rank(a) <= mode_rank(b) {
        a
    } else {
        b
    }
}

/// What the parent sandbox had. A child gets this much at most.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ceiling {
    pub parent: String,
    pub parent_harness: String,
    /// The sandboxes above the child, the first one started by the user.
    pub chain: Vec<String>,
    /// The depth of the parent: 0 when the user started it.
    pub depth: u32,
    /// The repository: the child gets a worktree of it.
    pub repo: PathBuf,
    /// The parent's project folder. Its HEAD is the base of the child.
    pub head_from: PathBuf,
    pub backend: String,
    pub network: Network,
    pub lan: bool,
    pub inherit_env: bool,
    /// Host and manifest variables that the parent had.
    pub env_names: BTreeSet<String>,
    pub allow_domains: BTreeSet<String>,
    pub host_ports: BTreeSet<u16>,
    /// Mounts that came from the manifest: (host path, writable).
    pub mounts: Vec<(PathBuf, bool)>,
    /// The parent's settings as they were approved at its start. The child
    /// uses this copy, never the project file again: the parent can edit it.
    pub manifest: Manifest,
    /// The parent's host capability modes.
    pub modes: BTreeMap<String, Mode>,
    /// The login mode for the child: the manifest's, or shared.
    pub auth: String,
}

fn from_env_or_manifest(origin: &str) -> bool {
    origin == "host env (inherited)" || origin.starts_with("manifest")
}

impl Ceiling {
    pub fn of(built: &Built, backend: &str, modes: &BTreeMap<String, Mode>) -> Self {
        let s = &built.spec;
        let (chain, depth) = match &built.spawn {
            Some(c) => (c.chain.clone(), c.depth),
            None => (Vec::new(), 0),
        };
        let mut chain = chain;
        chain.push(s.id.clone());
        let mut m = built.manifest.clone();
        // A child has no display and publishes no ports.
        m.gui = Default::default();
        m.network.publish.clear();
        m.apps.clear();
        m.default_harness = None;
        Self {
            parent: s.id.clone(),
            parent_harness: s.harness.clone(),
            chain,
            depth,
            repo: built.home_key.clone(),
            head_from: s.project.clone(),
            backend: backend.to_string(),
            network: s.network,
            lan: s.lan,
            inherit_env: built.inherit_env,
            env_names: s
                .env
                .iter()
                .filter(|(_, v)| from_env_or_manifest(&v.origin))
                .map(|(k, _)| k.clone())
                .collect(),
            allow_domains: s.allow_domains.iter().cloned().collect(),
            host_ports: s.host_ports.iter().map(|(p, _)| *p).collect(),
            mounts: s
                .mounts
                .iter()
                .filter(|m| m.origin.starts_with("manifest"))
                .filter_map(|m| m.src.clone().map(|p| (p, m.kind == MountKind::Rw)))
                .collect(),
            manifest: m,
            modes: modes.clone(),
            auth: built.auth.to_string(),
        }
    }

    /// Refuse a child that would get more than the parent. `endpoints` are
    /// the child harness's own API hosts, which it needs in allowlist mode.
    pub fn check(&self, built: &Built, backend: &str) -> Result<()> {
        let s = &built.spec;
        let no = |what: String| -> Result<()> {
            bail!("spawn: the child would get more than its parent: {what}")
        };
        if backend != self.backend {
            no(format!(
                "backend {backend}, the parent runs on {}",
                self.backend
            ))?;
        }
        if network_rank(s.network) > network_rank(self.network) {
            no(format!(
                "network {:?}, the parent has {:?}",
                s.network, self.network
            ))?;
        }
        if s.lan && !self.lan {
            no("lan access".into())?;
        }
        if built.inherit_env && !self.inherit_env {
            no("the host environment, the parent has --env clean".into())?;
        }
        if s.gui.mode != GuiMode::Off || s.gui.gpu {
            no("a display".into())?;
        }
        if !s.publish.is_empty() {
            no("published ports".into())?;
        }
        if built.home_key != self.repo {
            no(format!("the project {}", built.home_key.display()))?;
        }
        for (k, v) in &s.env {
            if from_env_or_manifest(&v.origin) && !self.env_names.contains(k) {
                no(format!("the variable {k} ({})", v.origin))?;
            }
        }
        for (p, _) in &s.host_ports {
            if !self.host_ports.contains(p) {
                no(format!("host port {p}"))?;
            }
        }
        let endpoints: BTreeSet<&String> = built.profile.network.endpoints.iter().collect();
        for d in &s.allow_domains {
            if !self.allow_domains.contains(d) && !endpoints.contains(d) {
                no(format!("the domain {d}"))?;
            }
        }
        for m in s.mounts.iter().filter(|m| m.origin.starts_with("manifest")) {
            let Some(src) = &m.src else { continue };
            let rw = m.kind == MountKind::Rw;
            let ok = self.mounts.iter().any(|(p, prw)| p == src && (*prw || !rw));
            if !ok {
                no(format!(
                    "the mount {} ({})",
                    src.display(),
                    if rw { "writable" } else { "read-only" }
                ))?;
            }
        }
        Ok(())
    }
}

/// What a child sandbox knows about where it came from. Its broker uses it:
/// the modes and the depth come from here, never from the sandbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChildInfo {
    pub spawn_id: String,
    pub parent: String,
    /// The sandboxes above this one, root first, the parent last.
    pub chain: Vec<String>,
    /// The depth of this sandbox: 1 for a child of a sandbox that the user
    /// started.
    pub depth: u32,
    pub modes: BTreeMap<String, Mode>,
    #[serde(default)]
    pub role: Role,
}

/// What `build` needs to make a child sandbox.
#[derive(Debug, Clone)]
pub struct ChildPlan {
    pub id: String,
    pub manifest: Manifest,
    pub branch: String,
    /// The folder whose HEAD is the base commit.
    pub base_from: PathBuf,
    /// The base commit, when paddock made one from the parent's files
    /// (`--from-head`). None: HEAD of `base_from`.
    pub base: Option<String>,
    pub role: Role,
}

/// The host modes for a child: the user configuration as it is now, and at
/// most what the parent had.
pub fn child_modes(
    now: &BTreeMap<String, Mode>,
    parent: &BTreeMap<String, Mode>,
) -> BTreeMap<String, Mode> {
    now.iter()
        .map(|(k, m)| {
            let p = parent.get(k).copied().unwrap_or(Mode::Deny);
            (k.clone(), narrower(*m, p))
        })
        .collect()
}

/// One child, written by the parent's host process for the child's host
/// process. It lives in the parent's run folder, which no sandbox sees.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub spawn_id: String,
    pub sandbox_id: String,
    pub harness: String,
    pub branch: String,
    pub ceiling: Ceiling,
    #[serde(default)]
    pub role: Role,
    #[serde(default)]
    pub limits: Limits,
    /// The commit that paddock made from the parent's files (`--from-head`).
    #[serde(default)]
    pub base: Option<String>,
}

/// What a spawn request asks for, after the host checked it.
#[derive(Debug, Clone, Copy, Default)]
pub struct StartOpts {
    pub role: Role,
    pub limits: Limits,
    /// Start from the parent's files, uncommitted changes included.
    pub from_head: bool,
}

/// What the child's host process reports back.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChildResult {
    pub exit_code: Option<i32>,
    pub branch: String,
    pub base: String,
    pub commits: usize,
    /// Changed files that were not committed. They stay in the worktree and
    /// are not on the branch. None when the state could not be read.
    pub uncommitted: Option<usize>,
    pub stat: String,
    pub output: String,
    pub output_truncated: bool,
    pub started: u64,
    pub ended: u64,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum State {
    Starting,
    Running,
    /// The harness ran to its end. `exit_code` says how it went.
    Exited,
    /// paddock could not start or finish the child.
    Failed,
    /// Stopped by `paddock-host kill`, `paddock stop` or the parent's end.
    Stopped,
    TimedOut,
}

impl State {
    pub fn active(self) -> bool {
        matches!(self, State::Starting | State::Running)
    }
}

/// The state of one child, as `paddock-host status` shows it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    pub id: String,
    pub sandbox: String,
    pub harness: String,
    pub branch: String,
    pub state: State,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub started: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended: Option<u64>,
    /// The results folder in the parent project, relative to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inbox: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
    #[serde(default)]
    pub role: Role,
    #[serde(default)]
    pub limits: Limits,
    /// systemd-run when a scope enforces the limits, otherwise paddock.
    #[serde(default)]
    pub enforced_by: String,
    /// The commit that the child started from, when paddock made it from
    /// the parent's uncommitted files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_head: Option<String>,
}

struct Entry {
    status: Status,
    /// The child's host process.
    pid: u32,
    stop: Option<(State, Instant)>,
}

/// The children of one sandbox. The parent's broker owns it.
pub struct Spawner {
    env: HostEnv,
    pub cfg: SpawnCfg,
    pub ceiling: Ceiling,
    /// The parent's project, where results go.
    project: PathBuf,
    dir: PathBuf,
    /// The command that runs a job. The job file is its last argument.
    program: Vec<OsString>,
    /// Whether to look for `systemd-run --user`; tests turn it off.
    use_scope: bool,
    scope: std::sync::OnceLock<Option<PathBuf>>,
    children: Mutex<BTreeMap<String, Entry>>,
    changed: Condvar,
}

fn new_spawn_id() -> String {
    use sha2::{Digest, Sha256};
    let seed = format!(
        "{:?}-{}-{}",
        Instant::now(),
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    crate::manifest::hex(&Sha256::digest(seed.as_bytes()))[..8].to_string()
}

pub fn branch_for(harness: &str, id: &str) -> String {
    format!("paddock/spawn-{harness}-{id}")
}

pub fn sandbox_id_for(harness: &str, id: &str) -> String {
    format!("{harness}-spawn-{id}")
}

pub fn inbox_name(id: &str) -> String {
    format!("spawn-{id}")
}

impl Spawner {
    pub fn new(env: &HostEnv, cfg: SpawnCfg, ceiling: Ceiling, project: &Path) -> Result<Self> {
        let me = std::env::current_exe().context("cannot find the paddock executable")?;
        let mut s = Self::with_program(
            env,
            cfg,
            ceiling,
            project,
            vec![me.into_os_string(), "spawn-child".into()],
        );
        s.use_scope = true;
        Ok(s)
    }

    /// `systemd-run` when it can start a user scope here. A scope holds the
    /// child's host process and its sandbox, so its limits cover both.
    pub fn scope_tool(&self) -> Option<PathBuf> {
        if !self.use_scope {
            return None;
        }
        self.scope
            .get_or_init(|| {
                let tool = self.env.which("systemd-run")?;
                let mut c = std::process::Command::new(&tool);
                c.args(["--user", "--scope", "--quiet", "--collect", "--", "true"]);
                match crate::broker::run_capture(&mut c, Duration::from_secs(10)) {
                    Ok((0, _, _)) => Some(tool),
                    _ => {
                        log::info!("systemd-run --user --scope does not work here; paddock enforces the spawn timeout itself");
                        None
                    }
                }
            })
            .clone()
    }

    /// A spawner that runs `program JOB_FILE` for each child. Tests use a
    /// shell script here.
    pub fn with_program(
        env: &HostEnv,
        cfg: SpawnCfg,
        ceiling: Ceiling,
        project: &Path,
        program: Vec<OsString>,
    ) -> Self {
        let dir = env.paddock_run().join(&ceiling.parent).join("spawn");
        Self {
            env: env.clone(),
            cfg,
            ceiling,
            project: project.to_path_buf(),
            dir,
            program,
            use_scope: false,
            scope: std::sync::OnceLock::new(),
            children: Mutex::new(BTreeMap::new()),
            changed: Condvar::new(),
        }
    }

    pub fn running(&self) -> usize {
        self.children
            .lock()
            .unwrap()
            .values()
            .filter(|e| e.status.state.active())
            .count()
    }

    pub fn list(&self) -> Vec<Status> {
        self.children
            .lock()
            .unwrap()
            .values()
            .map(|e| e.status.clone())
            .collect()
    }

    pub fn status(&self, id: &str) -> Option<Status> {
        self.children
            .lock()
            .unwrap()
            .get(id)
            .map(|e| e.status.clone())
    }

    /// Start one child. The limit on running children is checked here, under
    /// the same lock that adds the child.
    pub fn start(self: &Arc<Self>, harness: &str, prompt: &[u8], o: StartOpts) -> Result<Status> {
        let scope = self.scope_tool();
        if scope.is_none() && (o.limits.mem.is_some() || o.limits.cpu_percent.is_some()) {
            bail!("paddock needs `systemd-run --user` to limit memory and CPUs, and it does not work here. Remove --mem and --cpus, or [spawn] max_mem and max_cpus");
        }
        let id = new_spawn_id();
        let sandbox = sandbox_id_for(harness, &id);
        let branch = branch_for(harness, &id);
        let job_dir = self.dir.join(&id);
        {
            let mut ch = self.children.lock().unwrap();
            let n = ch.values().filter(|e| e.status.state.active()).count();
            if n as u32 >= self.cfg.max_running {
                bail!(
                    "{n} children run already; [spawn] max_running is {}. Wait for one with `paddock-host wait`",
                    self.cfg.max_running
                );
            }
            ch.insert(
                id.clone(),
                Entry {
                    status: Status {
                        id: id.clone(),
                        sandbox: sandbox.clone(),
                        harness: harness.to_string(),
                        branch: branch.clone(),
                        state: State::Starting,
                        exit_code: None,
                        started: state::now(),
                        ended: None,
                        inbox: None,
                        message: String::new(),
                        role: o.role,
                        limits: o.limits,
                        enforced_by: if scope.is_some() {
                            "systemd-run"
                        } else {
                            "paddock"
                        }
                        .into(),
                        from_head: None,
                    },
                    pid: 0,
                    stop: None,
                },
            );
        }
        let fail = |e: anyhow::Error| {
            self.children.lock().unwrap().remove(&id);
            let _ = std::fs::remove_dir_all(&job_dir);
            e
        };
        let mut job = Job {
            spawn_id: id.clone(),
            sandbox_id: sandbox,
            harness: harness.to_string(),
            branch,
            ceiling: self.ceiling.clone(),
            role: o.role,
            limits: o.limits,
            base: None,
        };
        crate::privfs::dir(&job_dir).map_err(|e| fail(e.into()))?;
        if o.from_head {
            let c = snapshot_commit(&self.ceiling.head_from, &job_dir.join("index"), &id)
                .context("--from-head")
                .map_err(fail)?;
            if let Some(e) = self.children.lock().unwrap().get_mut(&id) {
                e.status.from_head = Some(c.clone());
            }
            job.base = Some(c);
        }
        let write = || -> Result<PathBuf> {
            crate::privfs::write(&job_dir.join("prompt.md"), prompt)?;
            let f = job_dir.join("job.json");
            crate::privfs::write(&f, serde_json::to_vec(&job)?)?;
            Ok(f)
        };
        let job_file = write().map_err(fail)?;
        // The watcher starts the child and lives as long as it runs: the
        // parent-death signal belongs to the thread that started the process.
        let (tx, rx) = std::sync::mpsc::channel();
        let me = self.clone();
        let (id2, dir2) = (id.clone(), job_dir.clone());
        std::thread::spawn(move || me.watch(&id2, &dir2, &job_file, o.limits, scope, tx));
        match rx.recv() {
            Ok(Ok(pid)) => {
                let mut ch = self.children.lock().unwrap();
                if let Some(e) = ch.get_mut(&id) {
                    e.pid = pid;
                    if e.status.state == State::Starting {
                        e.status.state = State::Running;
                    }
                }
                Ok(ch.get(&id).map(|e| e.status.clone()).unwrap())
            }
            Ok(Err(e)) => Err(fail(e)),
            Err(_) => Err(fail(anyhow::anyhow!("the child did not start"))),
        }
    }

    fn command(
        &self,
        job_file: &Path,
        log: std::fs::File,
        limits: Limits,
        scope: Option<&Path>,
        unit: &str,
    ) -> Result<std::process::Command> {
        use std::os::unix::process::CommandExt;
        let (first, rest) = self.program.split_first().context("no program")?;
        let mut cmd = match scope {
            // The scope runs the program in the same process, so the
            // settings below apply to it. paddock stops the child itself at
            // the timeout, so it can save its login; the scope's runtime
            // limit is the backstop a minute later.
            Some(tool) => {
                let mut c = std::process::Command::new(tool);
                c.args(["--user", "--scope", "--quiet", "--collect", "--unit", unit]);
                c.arg("-p")
                    .arg(format!("RuntimeMaxSec={}", limits.timeout + 60));
                if let Some(m) = limits.mem {
                    c.arg("-p").arg(format!("MemoryMax={m}"));
                    c.arg("-p").arg("MemorySwapMax=0");
                }
                if let Some(p) = limits.cpu_percent {
                    c.arg("-p").arg(format!("CPUQuota={p}%"));
                }
                c.arg("--").arg(first);
                c
            }
            None => std::process::Command::new(first),
        };
        cmd.args(rest)
            .arg(job_file)
            .stdin(std::process::Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log);
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                // Its own session: a Ctrl-C in the parent's terminal does
                // not reach it, and the group can be stopped as one.
                libc::setsid();
                // The child ends when the parent's paddock ends, even when
                // that one is killed.
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
        Ok(cmd)
    }

    fn watch(
        &self,
        id: &str,
        job_dir: &Path,
        job_file: &Path,
        limits: Limits,
        scope: Option<PathBuf>,
        started: std::sync::mpsc::Sender<Result<u32>>,
    ) {
        let unit = format!("paddock-spawn-{id}");
        let spawned = crate::privfs::append(&job_dir.join("log"))
            .map_err(anyhow::Error::from)
            .and_then(|log| self.command(job_file, log, limits, scope.as_deref(), &unit))
            .and_then(|mut c| c.spawn().context("cannot start the child"));
        let mut child = match spawned {
            Ok(c) => c,
            Err(e) => {
                let _ = started.send(Err(e));
                return;
            }
        };
        let pid = child.id();
        let _ = started.send(Ok(pid));
        let begin = Instant::now();
        let timeout = Duration::from_secs(limits.timeout);
        let code = loop {
            match child.try_wait() {
                Ok(Some(s)) => break s.code(),
                Ok(None) => {}
                Err(_) => break None,
            }
            let stopping = self
                .children
                .lock()
                .unwrap()
                .get(id)
                .is_some_and(|e| e.stop.is_some());
            if begin.elapsed() > timeout && !stopping {
                self.stop_one(id, State::TimedOut);
            }
            let late = self
                .children
                .lock()
                .unwrap()
                .get(id)
                .and_then(|e| e.stop)
                .is_some_and(|(_, at)| at.elapsed() > STOP_GRACE);
            if late {
                // SAFETY: plain kill(2) on the child's own process group.
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGKILL);
                }
                let _ = child.kill();
            }
            std::thread::sleep(Duration::from_millis(200));
        };
        let result: Option<ChildResult> = std::fs::read(job_dir.join("result.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok());
        self.finish(id, code, result);
    }

    /// Record the end of a child, give the parent its results and log it.
    fn finish(&self, id: &str, code: Option<i32>, result: Option<ChildResult>) {
        let (status, stop) = {
            let ch = self.children.lock().unwrap();
            let Some(e) = ch.get(id) else { return };
            (e.status.clone(), e.stop.map(|(s, _)| s))
        };
        let r = result.unwrap_or_default();
        // The scope's runtime limit ends the child without a result.
        let ran = state::now().saturating_sub(status.started);
        let stop = stop.or((r.exit_code.is_none()
            && r.error.is_none()
            && ran >= status.limits.timeout)
            .then_some(State::TimedOut));
        let state = match (stop, &r.error, r.exit_code) {
            (Some(s), _, _) => s,
            (None, Some(_), _) => State::Failed,
            (None, None, Some(_)) => State::Exited,
            (None, None, None) => State::Failed,
        };
        let message = match (&r.error, state) {
            (Some(e), _) => e.clone(),
            (None, State::TimedOut) => format!(
                "stopped by {} after the time limit of {} seconds",
                status.enforced_by, status.limits.timeout
            ),
            (None, State::Stopped) => "stopped before it finished".into(),
            (None, State::Failed) => format!(
                "the child ended without a result (exit {})",
                code.map_or("by a signal".into(), |c| c.to_string())
            ),
            _ => String::new(),
        };
        let ended = state::now();
        let mut done = Status {
            state,
            exit_code: r.exit_code,
            ended: Some(ended),
            message,
            ..status
        };
        match self.write_inbox(&done, &r) {
            Ok(rel) => done.inbox = Some(rel),
            Err(e) => {
                log::warn!("spawn {id}: cannot write the results to the inbox: {e:#}");
                if done.message.is_empty() {
                    done.message = format!("cannot write the results: {e:#}");
                }
            }
        }
        self.audit_end(&done, &r);
        if let Some(e) = self.children.lock().unwrap().get_mut(id) {
            e.status = done;
        }
        self.changed.notify_all();
    }

    fn write_inbox(&self, s: &Status, r: &ChildResult) -> Result<String> {
        let inbox = crate::broker::inbox(&self.project)?;
        let (dir, name) = inbox.new_subdir(&inbox_name(&s.id))?;
        let summary = serde_json::json!({
            "id": s.id,
            "sandbox": s.sandbox,
            "harness": s.harness,
            "state": s.state,
            "exit_code": s.exit_code,
            "message": s.message,
            "branch": s.branch,
            "base": r.base,
            "commits": r.commits,
            "uncommitted_files": r.uncommitted,
            "diff_stat": r.stat,
            "started": s.started,
            "ended": s.ended,
            "output_file": "output.txt",
            "output_truncated": r.output_truncated,
            "role": s.role,
            "from_head": s.from_head,
        });
        let mut f = dir.create_new(std::ffi::OsStr::new("result.json"), 0o644)?;
        f.write_all(serde_json::to_string_pretty(&summary)?.as_bytes())?;
        let mut f = dir.create_new(std::ffi::OsStr::new("output.txt"), 0o644)?;
        f.write_all(r.output.as_bytes())?;
        Ok(format!("{}/{name}", crate::broker::INBOX))
    }

    /// One audit line per finished child. It names the parent, the child,
    /// the harness, the branch, the exit status and the durations. It never
    /// holds the prompt or the output.
    fn audit_end(&self, s: &Status, r: &ChildResult) {
        let ended = s.ended.unwrap_or_else(state::now);
        let ran = if r.started > 0 && r.ended >= r.started {
            Some(r.ended - r.started)
        } else {
            None
        };
        let detail = format!(
            "child {} ({}) on {}: {}{} after {} s",
            s.sandbox,
            s.harness,
            s.branch,
            serde_json::to_value(s.state)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default(),
            s.exit_code
                .map(|c| format!(", exit {c}"))
                .unwrap_or_default(),
            ended.saturating_sub(s.started)
        );
        let line = serde_json::json!({
            "time": state::now(),
            "sandbox": self.ceiling.parent,
            "harness": self.ceiling.parent_harness,
            "project": self.project,
            "capability": "spawn",
            "detail": detail,
            "decision": "child ended",
            "status": if s.state == State::Exited && s.exit_code == Some(0) { "done" } else { "failed" },
            "message": s.message,
            "parent": self.ceiling.parent,
            "chain": self.ceiling.chain,
            "child": s.sandbox,
            "spawn_id": s.id,
            "child_harness": s.harness,
            "branch": s.branch,
            "state": s.state,
            "exit_code": s.exit_code,
            "started": s.started,
            "ended": ended,
            "duration_s": ended.saturating_sub(s.started),
            "run_s": ran,
            "role": s.role,
            "limits": s.limits,
            "enforced_by": s.enforced_by,
            "from_head": s.from_head.is_some(),
        });
        crate::broker::audit_line(&self.env, &line);
    }

    /// Stop one child: its sandbox first, so its own paddock cleans up and
    /// saves its login; the whole process group after `STOP_GRACE`.
    pub fn stop_one(&self, id: &str, why: State) -> bool {
        let mut ch = self.children.lock().unwrap();
        let Some(e) = ch.get_mut(id) else {
            return false;
        };
        if !e.status.state.active() {
            return false;
        }
        if e.stop.is_none() {
            e.stop = Some((why, Instant::now()));
        }
        let sandbox_pid = state::Running::list(&self.env)
            .into_iter()
            .find(|r| r.id == e.status.sandbox && r.parent.as_deref() == Some(&self.ceiling.parent))
            .map(|r| r.pid);
        // SAFETY: plain kill(2) calls.
        unsafe {
            match sandbox_pid {
                Some(p) => libc::kill(p as i32, libc::SIGTERM),
                None if e.pid > 0 => libc::kill(e.pid as i32, libc::SIGTERM),
                None => 0,
            };
        }
        true
    }

    /// Wait until the child ends or `timeout` passes.
    pub fn wait(&self, id: &str, timeout: Duration) -> Option<Status> {
        let start = Instant::now();
        let mut ch = self.children.lock().unwrap();
        loop {
            let s = ch.get(id)?.status.clone();
            if !s.state.active() {
                return Some(s);
            }
            let left = timeout.saturating_sub(start.elapsed());
            if left.is_zero() {
                return Some(s);
            }
            ch = self.changed.wait_timeout(ch, left).unwrap().0;
        }
    }

    /// Stop every child and wait for them to end. The parent sandbox ended.
    pub fn stop_all(&self, wait: Duration) {
        let ids: Vec<String> = self
            .list()
            .into_iter()
            .filter(|s| s.state.active())
            .map(|s| s.id)
            .collect();
        if ids.is_empty() {
            return;
        }
        for id in &ids {
            self.stop_one(id, State::Stopped);
        }
        let start = Instant::now();
        for id in &ids {
            let left = wait.saturating_sub(start.elapsed());
            self.wait(id, left);
        }
    }
}

/// The base commit of a child: HEAD of the parent's project folder.
fn base_commit(dir: &Path) -> Result<String> {
    let l = crate::hostgit::Layout::discover(dir)?
        .context("spawn needs a git repository: the child works in a worktree")?;
    Ok(l.query(&["rev-parse", "HEAD"])?.trim().to_string())
}

/// The end of `text`, at most `max` bytes, on a character boundary.
pub fn tail(text: &str, max: usize) -> (String, bool) {
    if text.len() <= max {
        return (text.to_string(), false);
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    (text[start..].to_string(), true)
}

/// The host process of one child (`paddock spawn-child JOB`). The parent's
/// paddock starts it; it builds and runs the child sandbox and writes
/// `result.json` next to the job.
pub fn child_main(env: &HostEnv, job_file: &Path) -> Result<i32> {
    let dir = job_file.parent().context("bad job file")?.to_path_buf();
    if !dir.starts_with(env.paddock_run()) {
        bail!("the job file must be in {}", env.paddock_run().display());
    }
    let job: Job = serde_json::from_slice(
        &std::fs::read(job_file).with_context(|| format!("cannot read {}", job_file.display()))?,
    )?;
    let prompt = std::fs::read(dir.join("prompt.md"))?;
    let _ = std::fs::remove_file(dir.join("prompt.md"));
    let mut result = ChildResult {
        branch: job.branch.clone(),
        started: state::now(),
        ..Default::default()
    };
    let r = run_child(env, &job, prompt, &mut result);
    if let Err(e) = &r {
        result.error = Some(format!("{e:#}"));
    }
    result.ended = state::now();
    crate::privfs::write(&dir.join("result.json"), serde_json::to_vec(&result)?)?;
    Ok(if r.is_ok() { 0 } else { 1 })
}

fn run_child(env: &HostEnv, job: &Job, prompt: Vec<u8>, result: &mut ChildResult) -> Result<()> {
    let c = &job.ceiling;
    let factory = crate::profile::HarnessFactory::new(env)?;
    let harness = factory.create(&job.harness)?;
    let oneshot = harness
        .profile
        .oneshot
        .clone()
        .with_context(|| format!("the {} profile has no [oneshot] command", job.harness))?;
    let opts = crate::build::Options {
        harness: job.harness.clone(),
        project: Some(c.repo.clone()),
        backend: Some(c.backend.clone()),
        // TODO: a reviewer gets network mode "api" (only the model API
        // hosts of its profile) once that mode exists. Until then it keeps
        // the parent's mode, which `Ceiling::check` caps.
        network: Some(c.network),
        gui: Some(GuiMode::Off),
        // Both roles get a clean environment: only the allowlist and the
        // harness's own variables pass.
        inherit_env: Some(false),
        auth: Some(c.auth.clone()),
        worktree: true,
        child: Some(ChildPlan {
            id: job.sandbox_id.clone(),
            manifest: c.manifest.clone(),
            branch: job.branch.clone(),
            base_from: c.head_from.clone(),
            base: job.base.clone(),
            role: job.role,
        }),
        ..Default::default()
    };
    let mut built = crate::build::build_with(env, &opts, harness)?;
    let wt = built.worktree.clone();
    if let Some(w) = &wt {
        result.base = w.base.clone();
    }
    let outcome = (|| -> Result<crate::run::Outcome> {
        let backend = crate::run::select_backend(env, &built)?;
        c.check(&built, backend.name())?;
        crate::run::probe_args(
            env,
            &built.harness_exe,
            &oneshot.args,
            &oneshot.probe_args,
            "[oneshot] args",
        )?;
        // The child's broker narrows these again with the configuration as
        // it is when the child starts.
        let modes = c.modes.clone();
        built.spawn = Some(ChildInfo {
            spawn_id: job.spawn_id.clone(),
            parent: c.parent.clone(),
            chain: c.chain.clone(),
            depth: c.depth + 1,
            modes,
            role: job.role,
        });
        match oneshot.prompt {
            crate::profile::PromptVia::Stdin => built.stdin = Some(prompt.clone()),
            crate::profile::PromptVia::File => {
                crate::safefs::Root::open(&built.spec.home_src)?.write(
                    Path::new(PROMPT_IN_HOME),
                    &prompt,
                    0o600,
                )?;
            }
        }
        let home = built.spec.home_src.clone();
        let out = crate::run::run(env, built, backend.as_ref(), true, true);
        if oneshot.prompt == crate::profile::PromptVia::File {
            if let Ok(r) = crate::safefs::Root::open(&home) {
                let _ = r.remove(Path::new(PROMPT_IN_HOME));
            }
        }
        out
    })();
    if let Some(w) = &wt {
        let (commits, uncommitted) = crate::build::worktree_counts(env, w);
        result.commits = commits;
        result.uncommitted = uncommitted;
        let stat = crate::hostgit::Layout::discover(&w.repo)
            .ok()
            .flatten()
            .and_then(|l| {
                l.try_query(&[
                    "diff",
                    "--stat",
                    "--no-textconv",
                    "--no-ext-diff",
                    "--no-color",
                    &format!("{}...{}", w.base, w.branch),
                ])
            })
            .unwrap_or_default();
        result.stat = tail(&stat, MAX_STAT).0;
    }
    let o = outcome?;
    result.exit_code = Some(o.code);
    let (text, cut) = tail(&o.stdout.unwrap_or_default(), MAX_OUTPUT);
    result.output = text;
    result.output_truncated = cut;
    Ok(())
}

/// Create the child's worktree: a new branch from the parent's HEAD.
pub fn create_worktree(
    env: &HostEnv,
    repo: &Path,
    plan: &ChildPlan,
) -> Result<crate::build::Worktree> {
    let layout = crate::hostgit::Layout::discover(repo)?;
    if layout.as_ref().map(|l| l.work_tree.as_path()) != Some(repo) {
        bail!(
            "spawn needs a git repository: {} is not one",
            repo.display()
        );
    }
    let base = match &plan.base {
        Some(b) => b.clone(),
        None => base_commit(&plan.base_from)?,
    };
    let path = env
        .paddock_data()
        .join("worktrees")
        .join(state::slug(repo))
        .join(&plan.id);
    crate::privfs::dir(path.parent().unwrap())?;
    crate::hostgit::Checkout::open(repo)?.run(&[
        "worktree",
        "add",
        "-q",
        "-b",
        &plan.branch,
        &path.display().to_string(),
        &base,
    ])?;
    Ok(crate::build::Worktree {
        repo: repo.to_path_buf(),
        path,
        branch: plan.branch.clone(),
        base,
    })
}

/// `--from-head`: a commit of the parent's files as they are now, with
/// uncommitted and untracked changes, on top of HEAD. It goes through a
/// private index file, so the parent's index and work tree stay as they are,
/// and no ref changes. The git call is the hardened one that refuses a
/// repository configuration that can start a program.
pub fn snapshot_commit(dir: &Path, index: &Path, id: &str) -> Result<String> {
    let co = crate::hostgit::Checkout::open(dir)?;
    let head = co.layout.query(&["rev-parse", "--verify", "HEAD"])?;
    let head = head.trim();
    let _ = std::fs::remove_file(index);
    co.run_with_index(index, &["read-tree", head])?;
    co.run_with_index(index, &["add", "-A", "--", "."])?;
    let tree = co.run_with_index(index, &["write-tree"])?;
    let tree = tree.trim();
    let _ = std::fs::remove_file(index);
    let head_tree = co
        .layout
        .query(&["rev-parse", &format!("{head}^{{tree}}")])?;
    if head_tree.trim() == tree {
        return Ok(head.to_string());
    }
    let msg = format!("paddock: uncommitted files for spawn {id}");
    let c = co.run_with_index(index, &["commit-tree", tree, "-p", head, "-m", &msg])?;
    Ok(c.trim().to_string())
}

/// Turn every writable mount below one of `roots` into a read-only one.
pub fn make_read_only(spec: &mut crate::spec::SandboxSpec, roots: &[PathBuf]) {
    for m in spec.mounts.iter_mut() {
        let under = m
            .src
            .as_ref()
            .is_some_and(|s| roots.iter().any(|r| s.starts_with(r)));
        if m.kind == MountKind::Rw && under {
            m.kind = MountKind::Ro;
            m.origin = format!("{} (read-only: reviewer)", m.origin);
        }
    }
}

/// The chain line on an approval card for a request from a child.
pub fn chain_text(chain: &[String]) -> Option<String> {
    let last = chain.last()?;
    let mut t = format!("Started by sandbox {last}.");
    if chain.len() > 1 {
        t.push_str(&format!(" Chain: {}.", chain.join(" > ")));
    }
    Some(t)
}

#[cfg(test)]
mod tests;
