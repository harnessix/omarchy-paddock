//! The user's own harness hooks in a sandbox.
//!
//! A harness profile names its hook files in `[hooks]`. At the build,
//! paddock reads the user's hook entries from the host files and checks the
//! program of each command (hooks/command.rs). An entry whose program does
//! not resolve inside the sandbox is left out of the sandbox copy, and the
//! start prints one line about it. For Codex (`codex_trust`), an entry that
//! paddock does not trust is left out too: Codex does not run it in the
//! sandbox, and it would ask about it at each start. For Grok Build
//! (`folder_trust`), an entry that paddock does not trust is left out the
//! same way: the folder trust that paddock writes for its own hook would
//! turn it on. At the start, paddock writes the sandbox
//! copy: paddock's own entries first, then the user's (hooks/write.rs). The
//! host files never change.
//!
//! `[hooks]` of the user configuration:
//!
//! - `programs`: hook programs that a sandbox may run. Each one is bound
//!   read-only into the sandbox, at `/run/paddock/bin/NAME` and at its own
//!   path. Codex trusts the user's entries that run one of them, and Grok
//!   Build gets them with the folder trust.
//! - `ignore`: programs whose entries never reach the sandbox copy, without
//!   a notice. `ignore` wins over `programs`.
//!
//! A project `paddock.toml` cannot set either (manifest.rs `USER_ONLY`).

pub mod command;
mod folder_trust;
mod write;

pub use folder_trust::FolderTrust;

use crate::env::HostEnv;
use crate::profile::Profile;
use crate::spec::{Mount, SandboxSpec};
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Component, Path, PathBuf};

pub use write::write;

/// The longest host hook file that paddock reads.
const MAX_FILE: u64 = 1 << 20;

/// `[hooks]` of a harness profile: where the harness reads its hooks.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Files {
    /// Host files with the user's hooks, relative to HOME. A `*` in the
    /// last part matches file names (`.grok/hooks/*.json`).
    pub files: Vec<String>,
    /// The sandbox file that gets paddock's hooks and the user's.
    pub into: String,
    /// Where the hooks are in each file.
    #[serde(default = "default_path")]
    pub path: Vec<String>,
    #[serde(default)]
    pub style: Style,
    /// Codex's `config.toml`, relative to HOME: paddock trusts its own
    /// entries and the user's entries of a listed program, and no other.
    #[serde(default)]
    pub codex_trust: Option<String>,
    /// Grok Build's folder trust (hooks/folder_trust.rs).
    #[serde(default)]
    pub folder_trust: Option<FolderTrust>,
}

impl Files {
    /// Whether the harness runs only the entries that paddock trusts.
    fn trusts(&self) -> bool {
        self.codex_trust.is_some() || self.folder_trust.is_some()
    }
}

fn default_path() -> Vec<String> {
    vec!["hooks".into()]
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Style {
    /// `{EVENT: [{matcher, hooks: [handler]}]}`: Claude Code, Codex, Grok
    /// Build, Qwen Code, Gemini CLI.
    #[default]
    Groups,
    /// `{NAME: {EVENT: [handler]}}`: agy.
    Named,
}

/// `[hooks]` of the user configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Cfg {
    pub programs: Vec<String>,
    pub ignore: Vec<String>,
}

impl Cfg {
    pub fn parse(text: &str) -> Result<Self> {
        let v: toml::Value = toml::from_str(text)?;
        Ok(match v.get("hooks") {
            Some(t) => t.clone().try_into()?,
            None => Self::default(),
        })
    }

    /// The user configuration. A bad table is a warning and the default.
    pub fn load(env: &HostEnv) -> Self {
        let file = env.paddock_config().join("config.toml");
        let Ok(text) = std::fs::read_to_string(&file) else {
            return Self::default();
        };
        Self::parse(&text).unwrap_or_else(|e| {
            log::warn!("[hooks] in {}: {e:#}", file.display());
            Self::default()
        })
    }
}

/// The hooks of one sandbox file: what the start writes there.
#[derive(Debug, Clone, Serialize)]
pub struct Target {
    pub into: PathBuf,
    pub path: Vec<String>,
    pub style: Style,
    /// The host files that had user hooks, relative to HOME. A sandbox
    /// copy of one of them other than `into` loses its hooks.
    pub sources: Vec<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codex_trust: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub folder_trust: Option<FolderTrust>,
    /// The user's groups that the sandbox copy gets, in order.
    pub groups: Vec<Group>,
    /// Every user entry, for `paddock explain`.
    pub entries: Vec<Entry>,
}

impl Target {
    /// The sandbox files that the start writes, for a snapshot's copy-up.
    pub fn files(&self) -> Vec<PathBuf> {
        let mut v = vec![self.into.clone()];
        v.extend(self.sources.iter().cloned());
        v.extend(self.codex_trust.iter().cloned());
        if let Some(f) = &self.folder_trust {
            v.push(f.file.clone().into());
            v.extend(f.drop_hooks.iter().map(PathBuf::from));
        }
        v
    }
}

/// A user hook group with only its kept handlers.
#[derive(Debug, Clone, Serialize)]
pub struct Group {
    /// The event (style groups) or the name (style named).
    pub key: String,
    pub value: Value,
    /// For each kept handler: it runs a listed program (Codex trusts it).
    pub trusted: Vec<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    pub file: PathBuf,
    pub event: String,
    pub command: String,
    pub verdict: Verdict,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Verdict {
    /// Kept: why it can run.
    Kept { why: String },
    /// Kept: it runs a program of `[hooks] programs`.
    Listed { program: String },
    /// Left out: the program is not in the sandbox.
    Missing { program: String },
    /// Left out by `[hooks] ignore`.
    Ignored { rule: String },
    /// Left out: the harness does not trust it in the sandbox (not a
    /// program of `[hooks] programs`).
    Untrusted { program: String },
}

impl Verdict {
    fn kept(&self) -> bool {
        matches!(self, Verdict::Kept { .. } | Verdict::Listed { .. })
    }
}

/// A program of `[hooks] programs` as the sandbox has it.
struct Listed {
    name: String,
    host: PathBuf,
}

fn inside_home(rel: &str) -> bool {
    let p = Path::new(rel);
    !rel.is_empty() && p.components().all(|c| matches!(c, Component::Normal(_)))
}

/// Check a listed program: a regular file (not a symlink), owned by the
/// user or root, not writable by group or others, executable, and outside
/// the project.
pub fn check_program(env: &HostEnv, project: &Path, raw: &str) -> Result<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    let p = command::expand(&env.home, raw);
    if !p.is_absolute() {
        bail!("{raw} must be an absolute path or start with ~/");
    }
    let m = std::fs::symlink_metadata(&p).map_err(|e| anyhow::anyhow!("{raw}: {e}"))?;
    if m.file_type().is_symlink() {
        bail!("{raw} is a symlink; name the file itself");
    }
    if !m.is_file() {
        bail!("{raw} is not a regular file");
    }
    if m.uid() != env.uid && m.uid() != 0 {
        bail!("{raw} belongs to another user");
    }
    if m.mode() & 0o022 != 0 {
        bail!("{raw} is writable by group or others");
    }
    if m.mode() & 0o111 == 0 {
        bail!("{raw} is not executable");
    }
    let real = std::fs::canonicalize(&p)?;
    let proj = std::fs::canonicalize(project).unwrap_or_else(|_| project.to_path_buf());
    if real.starts_with(&proj) || p.starts_with(project) {
        bail!("{raw} is inside the project, which the sandbox can write");
    }
    Ok(real)
}

/// Bind the listed programs into the sandbox. A refused one is a warning.
fn bind_programs(env: &HostEnv, cfg: &Cfg, spec: &mut SandboxSpec) -> Vec<Listed> {
    let mut out: Vec<Listed> = Vec::new();
    for raw in &cfg.programs {
        let ignored = cfg
            .ignore
            .iter()
            .any(|r| command::matches(&env.home, r, raw));
        if ignored {
            continue;
        }
        let real = match check_program(env, &spec.project.clone(), raw) {
            Ok(r) => r,
            Err(e) => {
                spec.warnings.push(format!(
                    "[hooks] programs: {e:#}; the sandbox does not get it"
                ));
                continue;
            }
        };
        let path = command::expand(&env.home, raw);
        let name = command::base(raw).to_string();
        if command::RUNTIME_TOOLS.contains(&name.as_str())
            || spec.host_tools.iter().any(|t| t.name == name)
            || out.iter().any(|l| l.name == name)
        {
            spec.warnings.push(format!(
                "[hooks] programs: {raw}: the sandbox has a tool named {name} already"
            ));
            continue;
        }
        let origin = format!("[hooks] programs {raw}");
        spec.add_tool(&name, real.clone(), &origin);
        if !crate::profile::is_system_path(&path) {
            spec.add_mount(Mount::bind(&real, &path, false, &origin));
        }
        out.push(Listed { name, host: path });
    }
    out
}

/// The host files of a pattern, relative to HOME, in name order.
fn matched(
    env: &HostEnv,
    profile: &Profile,
    auth: &crate::state::AuthMode,
    pat: &str,
) -> Vec<String> {
    let Some((dir, name)) = pat.rsplit_once('/').or(Some(("", pat))) else {
        return vec![];
    };
    let Some((pre, post)) = name.split_once('*') else {
        return vec![pat.to_string()];
    };
    let host = crate::state::host_file(env, profile, auth, dir);
    let mut v: Vec<String> = std::fs::read_dir(host)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| std::fs::metadata(e.path()).is_ok_and(|m| m.is_file()))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.len() >= pre.len() + post.len() && n.starts_with(pre) && n.ends_with(post))
        .map(|n| {
            if dir.is_empty() {
                n
            } else {
                format!("{dir}/{n}")
            }
        })
        .collect();
    v.sort();
    v
}

/// The home files of a profile that a start writes: paddock's compaction
/// hook (compaction.rs), the uploads that paddock turns off (uploads.rs),
/// then the user's hooks.
pub fn apply_all(
    env: &HostEnv,
    profile: &Profile,
    auth: &crate::state::AuthMode,
    spec: &mut SandboxSpec,
) -> Result<()> {
    crate::compaction::apply(env, profile, spec)?;
    crate::uploads::apply(env, profile, spec)?;
    apply(env, profile, auth, spec)
}

/// Read and check the user's hooks of a profile, bind the listed programs,
/// and note what the start writes. Call it after every mount is known.
pub fn apply(
    env: &HostEnv,
    profile: &Profile,
    auth: &crate::state::AuthMode,
    spec: &mut SandboxSpec,
) -> Result<()> {
    let Some(f) = &profile.hooks else {
        return Ok(());
    };
    let trust_files = f.folder_trust.iter().flat_map(|t| t.files());
    for rel in f
        .files
        .iter()
        .chain([&f.into])
        .chain(&f.codex_trust)
        .chain(trust_files)
    {
        let last_star = rel.rsplit('/').next().is_some_and(|n| n.contains('*'));
        let rest = rel.rsplit_once('/').map_or("", |(d, _)| d);
        if !inside_home(rel) || rest.contains('*') || (last_star && rel == &f.into) {
            bail!(
                "profile {}: hooks file {rel} must be a path inside the home",
                profile.name
            );
        }
    }
    let cfg = Cfg::load(env);
    let listed = bind_programs(env, &cfg, spec);
    let mut t = Target {
        into: f.into.clone().into(),
        path: f.path.clone(),
        style: f.style,
        sources: vec![],
        codex_trust: f.codex_trust.clone().map(PathBuf::from),
        folder_trust: f.folder_trust.clone(),
        groups: vec![],
        entries: vec![],
    };
    for pat in &f.files {
        for rel in matched(env, profile, auth, pat) {
            let host = crate::state::host_file(env, profile, auth, &rel);
            let Ok(m) = std::fs::metadata(&host) else {
                continue;
            };
            let doc: Option<Value> = (m.is_file() && m.len() <= MAX_FILE)
                .then(|| std::fs::read(&host).ok())
                .flatten()
                .and_then(|b| serde_json::from_slice(&b).ok());
            let Some(doc) = doc else {
                spec.warnings.push(format!(
                    "the hooks in ~/{rel} are not plain JSON; the sandbox copy gets none of them"
                ));
                t.sources.push(rel.into());
                continue;
            };
            t.sources.push(rel.clone().into());
            let section = crate::state::json_get(&doc, &f.path).cloned();
            let Some(Value::Object(section)) = section else {
                continue;
            };
            let rel = PathBuf::from(&rel);
            let mut check = |event: &str, h: &Value| -> Verdict {
                let mut v = judge(env, spec, &cfg, &listed, h);
                if f.trusts() && matches!(v, Verdict::Kept { .. }) {
                    v = Verdict::Untrusted {
                        program: command::first_program(h_command(h)),
                    };
                }
                t.entries.push(Entry {
                    file: rel.clone(),
                    event: event.into(),
                    command: h_command(h).into(),
                    verdict: v.clone(),
                });
                v
            };
            let mut groups = Vec::new();
            match f.style {
                Style::Groups => {
                    for (event, list) in &section {
                        for g in list.as_array().into_iter().flatten() {
                            let handlers = g.get("hooks").and_then(Value::as_array);
                            let mut kept = Vec::new();
                            let mut trusted = Vec::new();
                            for h in handlers.into_iter().flatten() {
                                let v = check(event, h);
                                if v.kept() {
                                    trusted.push(matches!(v, Verdict::Listed { .. }));
                                    kept.push(h.clone());
                                }
                            }
                            if !kept.is_empty() {
                                let mut value = g.clone();
                                value["hooks"] = Value::Array(kept);
                                groups.push(Group {
                                    key: event.clone(),
                                    value,
                                    trusted,
                                });
                            }
                        }
                    }
                }
                Style::Named => {
                    for (name, events) in &section {
                        let mut value = serde_json::Map::new();
                        for (event, list) in events.as_object().into_iter().flatten() {
                            let kept: Vec<Value> = list
                                .as_array()
                                .into_iter()
                                .flatten()
                                .filter(|h| check(event, h).kept())
                                .cloned()
                                .collect();
                            if !kept.is_empty() {
                                value.insert(event.clone(), Value::Array(kept));
                            }
                        }
                        if !value.is_empty() {
                            groups.push(Group {
                                key: name.clone(),
                                value: Value::Object(value),
                                trusted: vec![],
                            });
                        }
                    }
                }
            }
            t.groups.extend(groups);
        }
    }
    if let Some(n) = notice(&profile.name, &t.entries) {
        spec.notices.push(n);
    }
    spec.hooks.push(t);
    Ok(())
}

fn h_command(h: &Value) -> &str {
    h.get("command")
        .and_then(Value::as_str)
        .unwrap_or("(no command)")
}

/// The one start line about the left-out entries, if there are any.
fn notice(harness: &str, entries: &[Entry]) -> Option<String> {
    let names = |want: fn(&Verdict) -> Option<&String>| {
        let mut v: Vec<&str> = entries
            .iter()
            .filter_map(|e| want(&e.verdict).map(String::as_str))
            .collect();
        v.sort();
        v.dedup();
        v.join(", ")
    };
    let missing = names(|v| match v {
        Verdict::Missing { program } => Some(program),
        _ => None,
    });
    let untrusted = names(|v| match v {
        Verdict::Untrusted { program } => Some(program),
        _ => None,
    });
    let mut why = Vec::new();
    if !missing.is_empty() {
        why.push(format!("The program is not in the sandbox: {missing}."));
    }
    if !untrusted.is_empty() {
        why.push(format!(
            "paddock does not trust them for {harness} in the sandbox: {untrusted}."
        ));
    }
    (!why.is_empty()).then(|| format!(
        "{harness} hooks left out of the sandbox copy. {} To run one there, add its program to [hooks] programs in the user configuration. To drop it without this line, add it to [hooks] ignore",
        why.join(" ")
    ))
}

/// Whether a user hook handler can run in the sandbox: each simple
/// command of its line must (hooks/command.rs `split`).
fn judge(env: &HostEnv, spec: &SandboxSpec, cfg: &Cfg, listed: &[Listed], h: &Value) -> Verdict {
    let Some(cmd) = h.get("command").and_then(Value::as_str) else {
        return Verdict::Kept {
            why: "not a command".into(),
        };
    };
    let parts: Vec<command::Parsed> = command::split(cmd)
        .iter()
        .map(|p| command::parse(p))
        .collect();
    // An ignored program anywhere in the line drops the entry silently.
    for p in &parts {
        if let command::Parsed::Program { program, script } = p {
            let words: Vec<&String> = std::iter::once(program).chain(script).collect();
            if let Some(rule) = cfg
                .ignore
                .iter()
                .find(|r| words.iter().any(|w| command::matches(&env.home, r, w)))
            {
                return Verdict::Ignored { rule: rule.clone() };
            }
        }
    }
    // The strongest reason to keep it: a listed program, then a program in
    // the sandbox, then a builtin.
    let rank = |v: &Verdict| match v {
        Verdict::Listed { .. } => 2,
        Verdict::Kept { why } if why != BUILTIN => 1,
        _ => 0,
    };
    let mut best = Verdict::Kept {
        why: BUILTIN.into(),
    };
    for p in parts {
        let v = judge_one(spec, listed, p);
        if !v.kept() {
            return v;
        }
        if rank(&v) > rank(&best) {
            best = v;
        }
    }
    best
}

const BUILTIN: &str = "a shell builtin";

/// One simple command of a hook line.
fn judge_one(spec: &SandboxSpec, listed: &[Listed], p: command::Parsed) -> Verdict {
    let (program, script) = match p {
        command::Parsed::Builtin => {
            return Verdict::Kept {
                why: BUILTIN.into(),
            }
        }
        command::Parsed::ShellC => {
            return Verdict::Kept {
                why: "a shell command line".into(),
            }
        }
        command::Parsed::Program { program, script } => (program, script),
    };
    let found = command::find_program(spec, &program);
    let mut verdict = match &found {
        command::Found::No => return Verdict::Missing { program },
        command::Found::Unchecked(why) => Verdict::Kept { why: why.clone() },
        command::Found::Yes(p) => match listed
            .iter()
            .find(|l| *p == l.host || *p == Path::new(crate::spec::TOOL_DIR).join(&l.name))
        {
            Some(l) => Verdict::Listed {
                program: l.name.clone(),
            },
            None => Verdict::Kept {
                why: format!("{} is in the sandbox", p.display()),
            },
        },
    };
    if let Some(s) = script {
        match command::find_file(spec, &s) {
            command::Found::No => return Verdict::Missing { program: s },
            command::Found::Unchecked(why) => {
                if !matches!(verdict, Verdict::Listed { .. }) {
                    verdict = Verdict::Kept { why };
                }
            }
            command::Found::Yes(_) => {}
        }
    }
    verdict
}

/// Paddock's own entries of a target: event (or name) and command.
fn paddock_entries<'a>(
    spec: &'a SandboxSpec,
    t: &'a Target,
) -> impl Iterator<Item = &'a crate::spec::HomeSetting> {
    spec.home_settings.iter().filter(move |s| {
        s.file == t.into && s.path.len() == t.path.len() + 1 && s.path.starts_with(&t.path)
    })
}

/// The groups of a value of paddock: one group, or an array of them.
fn groups_of(v: &Value) -> Vec<&Value> {
    match v {
        Value::Array(a) => a.iter().collect(),
        v => vec![v],
    }
}

fn commands(style: Style, key: &str, v: &Value) -> Vec<(String, String)> {
    let cmd = |h: &Value| {
        h.get("command")
            .and_then(Value::as_str)
            .unwrap_or("(no command)")
            .to_string()
    };
    match style {
        Style::Groups => groups_of(v)
            .into_iter()
            .flat_map(|g| g["hooks"].as_array().into_iter().flatten())
            .map(|h| (key.to_string(), cmd(h)))
            .collect(),
        Style::Named => v
            .as_object()
            .into_iter()
            .flatten()
            .flat_map(|(ev, list)| {
                list.as_array()
                    .into_iter()
                    .flatten()
                    .map(move |h| (ev.clone(), cmd(h)))
            })
            .collect(),
    }
}

/// The lines of `paddock explain`: each hook entry of the sandbox copies
/// with its origin, and the left-out ones with the reason.
pub fn explain(spec: &SandboxSpec) -> Vec<String> {
    let mut out = Vec::new();
    for t in &spec.hooks {
        out.push(format!("  ~/{}", t.into.display()));
        for s in paddock_entries(spec, t) {
            let key = s.path.last().cloned().unwrap_or_default();
            for (ev, c) in commands(t.style, &key, &s.value) {
                out.push(format!("    {ev}  {c}\n           paddock ({})", s.origin));
            }
        }
        for e in &t.entries {
            let from = format!("~/{}", e.file.display());
            let why = match &e.verdict {
                Verdict::Kept { why } => format!("user {from}: {why}"),
                Verdict::Listed { program } => format!(
                    "user {from}: [hooks] programs {program}{}",
                    if t.codex_trust.is_some() || t.folder_trust.is_some() {
                        ", trusted"
                    } else {
                        ""
                    }
                ),
                Verdict::Missing { program } => format!(
                    "left out (user {from}): {program} is not in the sandbox; add it to [hooks] programs"
                ),
                Verdict::Ignored { rule } => {
                    format!("left out (user {from}): ignored by [hooks] ignore ({rule})")
                }
                Verdict::Untrusted { program } => format!(
                    "left out (user {from}): {program} is not in [hooks] programs, so the harness does not trust it in the sandbox"
                ),
            };
            out.push(format!("    {}  {}\n           {why}", e.event, e.command));
        }
    }
    out
}

#[cfg(test)]
mod tests;
