//! Home-grade monitoring of a sandbox: the paddock process of a sandbox
//! looks at the command lines of the processes in the sandbox's scope and
//! tells the user once when one of them opens a tunnel (`ssh -R/-L/-D`,
//! `ngrok`, `cloudflared` and the like).
//!
//! - It reads `/proc/PID/cmdline` only, never what goes through a tunnel,
//!   and contacts nothing.
//! - It needs the scope of the sandbox (a systemd user instance, bwrap or
//!   srt). Without one (docker, podman, no systemd) it does nothing.
//! - Each tool counts once per sandbox: the run folder keeps the tools that
//!   paddock reported (`monitor.json`), so a new watch does not repeat them.
//! - `[monitor] tunnels = false` in the user configuration turns it off,
//!   and `[monitor] notify = false` keeps the audit line but shows no
//!   notification.
//!
//! The same watch counts the bytes of the sandbox's own network namespace
//! (`/proc/PID/net/dev`, all interfaces but `lo`) and tells the user once
//! when the sent bytes of one hour pass a limit or the usual amount by far.
//! It keeps numbers only: totals in the run folder, and the sent bytes per
//! hour of each harness and project in `~/.local/state/paddock/monitor/`.
//!
//! The panel marks a sandbox with findings until the user says OK
//! (`paddock monitor ack`, a `monitor-ack.json` next to `monitor.json`).
//! `[monitor] mute` in the user configuration lists projects with no
//! notification and no mark: the audit lines stay.
//!
//! It also looks for new credential files in the sandbox home (`~/.ssh/`,
//! `~/.netrc`, token files and the like) every few minutes and once at the
//! end. It records paths and sizes, never content. A change to a login file
//! of the harness profile is a token refresh: it counts, with no alert.

use crate::env::HostEnv;
use crate::kv::Identity;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The capability name in the audit log.
pub const CAPABILITY: &str = "monitor";

/// How often the watch looks.
const INTERVAL: Duration = Duration::from_secs(10);

/// A tunnel in one command line: the tool and the flags that make it one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tunnel {
    pub tool: &'static str,
    pub flags: Vec<String>,
}

/// The user's switches, from `[monitor]` in the user configuration.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MonitorCfg {
    pub tunnels: bool,
    pub notify: bool,
    pub traffic: bool,
    /// Sent bytes in one hour that make an alert.
    pub upload_per_hour: u64,
    /// An hour that sends this many times the usual hour makes an alert.
    pub factor: f64,
    pub credentials: bool,
}

/// The default `upload_per_hour`: 5 GB.
const UPLOAD_PER_HOUR: u64 = 5 << 30;
/// The default `factor`.
const FACTOR: f64 = 10.0;
/// The hours with traffic that the baseline needs before `factor` counts.
const MIN_HOURS: usize = 5;
/// The baseline keeps the last 30 days.
const KEEP_HOURS: u64 = 30 * 24;

/// A size such as `5G`, `300M`, `64K` or `1000` (bytes). K is 1024 bytes.
pub fn parse_size(text: &str) -> Option<u64> {
    let t = text.trim();
    let (num, shift) = match t.chars().last()?.to_ascii_uppercase() {
        'K' => (&t[..t.len() - 1], 10),
        'M' => (&t[..t.len() - 1], 20),
        'G' => (&t[..t.len() - 1], 30),
        _ => (t, 0),
    };
    num.trim().parse::<u64>().ok()?.checked_mul(1u64 << shift)
}

impl MonitorCfg {
    pub fn load(env: &HostEnv) -> Self {
        let text =
            std::fs::read_to_string(env.paddock_config().join("config.toml")).unwrap_or_default();
        Self::parse(&text)
    }

    fn parse(text: &str) -> Self {
        let v = toml::from_str::<toml::Value>(text).ok();
        let get = |key: &str| {
            v.as_ref()
                .and_then(|v| v.get("monitor")?.get(key)?.as_bool())
                .unwrap_or(true)
        };
        let field = |key: &str| v.as_ref().and_then(|v| v.get("monitor")?.get(key));
        let upload_per_hour = match field("upload_per_hour") {
            None => UPLOAD_PER_HOUR,
            Some(toml::Value::Integer(n)) if *n > 0 => *n as u64,
            Some(toml::Value::String(s)) if parse_size(s).is_some_and(|n| n > 0) => {
                parse_size(s).unwrap_or(UPLOAD_PER_HOUR)
            }
            Some(other) => {
                log::warn!("[monitor] upload_per_hour = {other} is not a size such as \"5G\"");
                UPLOAD_PER_HOUR
            }
        };
        let factor = match field("factor") {
            None => FACTOR,
            Some(toml::Value::Integer(n)) if *n > 1 => *n as f64,
            Some(toml::Value::Float(f)) if *f > 1.0 => *f,
            Some(other) => {
                log::warn!("[monitor] factor = {other} is not a number above 1");
                FACTOR
            }
        };
        MonitorCfg {
            tunnels: get("tunnels"),
            notify: get("notify"),
            traffic: get("traffic"),
            upload_per_hour,
            factor,
            credentials: get("credentials"),
        }
    }
}

fn base(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

/// Interpreters that run a tool as a script (`node /usr/bin/lt`).
fn interpreter(name: &str) -> bool {
    matches!(name, "node" | "nodejs" | "bun" | "sh" | "bash" | "dash") || name.starts_with("python")
}

/// The tunnel that a command line opens, if any. A pure function of argv.
pub fn tunnel_in(argv: &[String]) -> Option<Tunnel> {
    let first = base(argv.first()?);
    if let Some(t) = tool(first, &argv[1..]) {
        return Some(t);
    }
    if interpreter(first) {
        let i = argv.iter().skip(1).position(|a| !a.starts_with('-'))? + 1;
        return tool(base(&argv[i]), &argv[i + 1..]);
    }
    None
}

fn tool(name: &str, args: &[String]) -> Option<Tunnel> {
    let simple = |tool| {
        Some(Tunnel {
            tool,
            flags: Vec::new(),
        })
    };
    match name {
        "ssh" => ssh(args),
        "ngrok" => simple("ngrok"),
        "cloudflared" => simple("cloudflared"),
        "frpc" => simple("frpc"),
        "frps" => simple("frps"),
        "bore" => simple("bore"),
        "chisel" => simple("chisel"),
        "lt" | "localtunnel" => simple("localtunnel"),
        "tailscale" if args.iter().any(|a| a == "funnel") => Some(Tunnel {
            tool: "tailscale funnel",
            flags: vec!["funnel".into()],
        }),
        _ => None,
    }
}

/// `ssh` with `-R`, `-L` or `-D`, alone or combined (`-NR`). The options
/// that take an argument end a combined word; ssh reads options before and
/// after the host, and the second plain word starts the remote command.
fn ssh(args: &[String]) -> Option<Tunnel> {
    const WITH_ARG: &str = "BbcDEeFIiJLlmOoPpQRSWw";
    let mut found: Vec<String> = Vec::new();
    let mut plain = 0;
    let mut skip = false;
    for a in args {
        if skip {
            skip = false;
            continue;
        }
        if a == "--" {
            break;
        }
        let Some(opts) = a.strip_prefix('-').filter(|o| !o.is_empty()) else {
            plain += 1;
            if plain > 1 {
                break;
            }
            continue;
        };
        for (i, c) in opts.char_indices() {
            if matches!(c, 'R' | 'L' | 'D') {
                let flag = format!("-{c}");
                if !found.contains(&flag) {
                    found.push(flag);
                }
            }
            if WITH_ARG.contains(c) {
                skip = i + c.len_utf8() == opts.len();
                break;
            }
        }
    }
    (!found.is_empty()).then_some(Tunnel {
        tool: "ssh",
        flags: found,
    })
}

/// The file with what paddock reported for a sandbox: tunnel tools,
/// alerts and traffic.
pub fn seen_file(env: &HostEnv, sandbox: &str) -> PathBuf {
    env.paddock_run().join(sandbox).join("monitor.json")
}

/// The tunnel tools that paddock reported for a sandbox, for `paddock ps`.
pub fn read_seen(env: &HostEnv, sandbox: &str) -> Vec<String> {
    load_record(&seen_file(env, sandbox))
        .tunnels
        .into_iter()
        .collect()
}

/// The bytes that a sandbox sent and received, for `paddock ps`.
pub fn read_traffic(env: &HostEnv, sandbox: &str) -> Option<Traffic> {
    load_record(&seen_file(env, sandbox)).traffic
}

/// The new credential files that paddock reported for a sandbox.
pub fn read_credential_files(env: &HostEnv, sandbox: &str) -> Vec<CredFile> {
    load_record(&seen_file(env, sandbox)).credential_files
}

/// One finding of a sandbox, for the panel and `paddock monitor list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Finding {
    /// `tunnel`, `upload` or `credential_file`.
    pub kind: &'static str,
    /// The tool, the bytes of the hour (`6.2 GB`), or the path (`~/.netrc`).
    pub detail: String,
    /// The user said OK to it (`paddock monitor ack`).
    pub acked: bool,
}

impl Finding {
    /// One line for people, the same words as the panel.
    pub fn text(&self) -> String {
        match self.kind {
            "tunnel" => format!("tunnel: {}", self.detail),
            "upload" => format!("upload: {} this hour", self.detail),
            _ => format!("new credential file: {}", self.detail),
        }
    }
}

/// The file with what the user said OK to, next to `monitor.json`. Only
/// `paddock monitor ack` writes it, so the watch never overwrites it.
pub fn ack_file(env: &HostEnv, sandbox: &str) -> PathBuf {
    env.paddock_run().join(sandbox).join("monitor-ack.json")
}

/// The findings that the user said OK to.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Ack {
    #[serde(default)]
    tunnels: BTreeSet<String>,
    #[serde(default)]
    alerts: BTreeSet<String>,
    #[serde(default)]
    credential_files: BTreeSet<String>,
}

fn load_ack(file: &Path) -> Ack {
    std::fs::read_to_string(file)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn findings_of(rec: &Record, ack: &Ack) -> Vec<Finding> {
    let mut out: Vec<Finding> = rec
        .tunnels
        .iter()
        .map(|t| Finding {
            kind: "tunnel",
            detail: t.clone(),
            acked: ack.tunnels.contains(t),
        })
        .collect();
    if !rec.alerts.is_empty() {
        let sent = if rec.alert_sent > 0 {
            rec.alert_sent
        } else {
            rec.traffic.as_ref().map_or(0, |t| t.hour_sent)
        };
        out.push(Finding {
            kind: "upload",
            detail: size(sent),
            acked: rec.alerts.iter().all(|a| ack.alerts.contains(a)),
        });
    }
    out.extend(rec.credential_files.iter().map(|f| Finding {
        kind: "credential_file",
        detail: format!("~/{}", f.path),
        acked: ack.credential_files.contains(&f.path),
    }));
    out
}

/// All findings of a sandbox, with the ones the user said OK to.
pub fn findings(env: &HostEnv, sandbox: &str) -> Vec<Finding> {
    findings_of(
        &load_record(&seen_file(env, sandbox)),
        &load_ack(&ack_file(env, sandbox)),
    )
}

/// The findings that make a mark in the panel: not said OK to, and not in
/// a muted project.
pub fn marks(env: &HostEnv, sandbox: &str, project: &Path) -> Vec<Finding> {
    if is_muted(&load_mute(env), project) {
        return Vec::new();
    }
    findings(env, sandbox)
        .into_iter()
        .filter(|f| !f.acked)
        .collect()
}

/// Say OK to the current findings of a sandbox: the panel mark goes away
/// until a new finding comes. The audit log keeps them. It returns how
/// many findings it acknowledged.
pub fn ack(env: &HostEnv, sandbox: &str) -> std::io::Result<usize> {
    let rec = load_record(&seen_file(env, sandbox));
    let ack = Ack {
        tunnels: rec.tunnels.clone(),
        alerts: rec.alerts.clone(),
        credential_files: rec
            .credential_files
            .iter()
            .map(|f| f.path.clone())
            .collect(),
    };
    let count = findings_of(&rec, &ack).len();
    let file = ack_file(env, sandbox);
    if let Some(d) = file.parent() {
        crate::privfs::dir(d)?;
    }
    crate::privfs::write_atomic(&file, serde_json::to_string(&ack).unwrap_or_default())?;
    Ok(count)
}

/// The muted projects: `[monitor] mute` of the user configuration. Only
/// absolute paths count. A project `paddock.toml` never comes here.
pub fn load_mute(env: &HostEnv) -> Vec<PathBuf> {
    parse_mute(
        &std::fs::read_to_string(env.paddock_config().join("config.toml")).unwrap_or_default(),
    )
}

fn parse_mute(text: &str) -> Vec<PathBuf> {
    let v = toml::from_str::<toml::Value>(text).ok();
    let Some(list) = v
        .as_ref()
        .and_then(|v| v.get("monitor")?.get("mute")?.as_array())
    else {
        return Vec::new();
    };
    list.iter()
        .filter_map(|p| p.as_str())
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .collect()
}

/// A project is muted when it is a muted path or a folder in one.
pub fn is_muted(mute: &[PathBuf], project: &Path) -> bool {
    mute.iter().any(|m| project.starts_with(m))
}

/// Whether a finding of a sandbox in `project` shows a notification now:
/// `[monitor] notify` and no mute. It reads the configuration each time,
/// so a mute counts at once.
fn notify_now(env: &HostEnv, project: &Path, notify: bool) -> bool {
    notify && !is_muted(&load_mute(env), project)
}

/// Add (`mute = true`) or remove a project in `[monitor] mute` of the user
/// configuration, and keep the rest of the file with its comments. It
/// returns the new list.
pub fn edit_mute(env: &HostEnv, path: &Path, mute: bool) -> anyhow::Result<Vec<String>> {
    use anyhow::Context;
    let file = env.paddock_config().join("config.toml");
    let text = std::fs::read_to_string(&file).unwrap_or_default();
    let new = edit_mute_text(&text, path, mute)
        .with_context(|| format!("cannot change {}", file.display()))?;
    crate::privfs::dir(file.parent().context("no configuration folder")?)?;
    crate::privfs::write_atomic(&file, new.0)?;
    Ok(new.1)
}

fn edit_mute_text(text: &str, path: &Path, mute: bool) -> anyhow::Result<(String, Vec<String>)> {
    use anyhow::{bail, Context};
    if !path.is_absolute() {
        bail!("{} is not an absolute path", path.display());
    }
    let path = path
        .to_str()
        .context("the path is not valid UTF-8")?
        .trim_end_matches('/');
    let path = if path.is_empty() { "/" } else { path };
    let mut doc: toml_edit::DocumentMut = text.parse().context("the file is not valid TOML")?;
    let table = doc.entry("monitor").or_insert(toml_edit::table());
    let table = table
        .as_table_mut()
        .context("[monitor] in the configuration is not a table")?;
    let arr = table
        .entry("mute")
        .or_insert(toml_edit::value(toml_edit::Array::new()));
    let arr = arr
        .as_array_mut()
        .context("[monitor] mute is not an array")?;
    let has = arr
        .iter()
        .any(|v| v.as_str().map(|s| s.trim_end_matches('/')) == Some(path));
    if mute && !has {
        arr.push(path);
    }
    if !mute {
        arr.retain(|v| v.as_str().map(|s| s.trim_end_matches('/')) != Some(path));
    }
    let list = arr
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    Ok((doc.to_string(), list))
}

#[cfg(test)]
fn load_seen(file: &Path) -> BTreeSet<String> {
    load_record(file).tunnels
}

/// The content of `monitor.json`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Record {
    #[serde(default)]
    tunnels: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    alerts: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    traffic: Option<Traffic>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    credential_files: Vec<CredFile>,
    /// Changes to the harness's own login files: token refreshes.
    #[serde(default, skip_serializing_if = "is_zero")]
    login_refreshes: u64,
    /// The hour (since 1970) of the last upload alert, and the bytes that
    /// the sandbox sent in that hour, so that the panel does not count.
    #[serde(default, skip_serializing_if = "is_zero")]
    alert_hour: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    alert_sent: u64,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

fn load_record(file: &Path) -> Record {
    std::fs::read_to_string(file)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// What the sandbox reported so far, kept in memory and in the run folder.
pub struct Seen {
    file: PathBuf,
    rec: Record,
}

impl Seen {
    pub fn open(file: PathBuf) -> Self {
        let rec = load_record(&file);
        Seen { file, rec }
    }

    fn save(&self) {
        let body = serde_json::to_string(&self.rec).unwrap_or_default();
        if let Err(e) = crate::privfs::write(&self.file, body) {
            log::warn!("cannot write {}: {e}", self.file.display());
        }
    }

    /// The tunnels that are new for this sandbox. It records them.
    pub fn fresh(&mut self, found: Vec<Tunnel>) -> Vec<Tunnel> {
        let mut out = Vec::new();
        for t in found {
            if self.rec.tunnels.insert(t.tool.to_string()) {
                out.push(t);
            }
        }
        if !out.is_empty() {
            self.save();
        }
        out
    }

    /// The alert reasons that are new for this sandbox. It records them,
    /// with the hour and its sent bytes. While that hour lasts, the bytes
    /// follow the count.
    pub fn fresh_alerts(
        &mut self,
        reasons: Vec<&'static str>,
        hour: u64,
        hour_sent: u64,
    ) -> Vec<&'static str> {
        let out: Vec<_> = reasons
            .into_iter()
            .filter(|r| self.rec.alerts.insert(r.to_string()))
            .collect();
        let follow = self.rec.alert_hour == hour && self.rec.alert_sent < hour_sent;
        if !out.is_empty() {
            self.rec.alert_hour = hour;
        }
        if !out.is_empty() || follow {
            self.rec.alert_sent = hour_sent;
            self.save();
        }
        out
    }

    /// The credential files that are new for this sandbox, and the count of
    /// login refreshes. It records both.
    pub fn fresh_creds(&mut self, found: Vec<CredFile>, refreshed: u64) -> Vec<CredFile> {
        let out: Vec<CredFile> = found
            .into_iter()
            .filter(|f| !self.rec.credential_files.iter().any(|c| c.path == f.path))
            .collect();
        self.rec.credential_files.extend(out.iter().cloned());
        self.rec.login_refreshes += refreshed;
        if !out.is_empty() || refreshed > 0 {
            self.save();
        }
        out
    }
}

/// How often the credential check looks.
const CRED_INTERVAL: Duration = Duration::from_secs(180);
/// The credential check stops after this many entries of the home.
const MAX_ENTRIES: usize = 50_000;
/// Credential files by their path relative to the home.
const CRED_PATHS: &[&str] = &[
    ".netrc",
    ".git-credentials",
    ".config/gh/hosts.yml",
    ".docker/config.json",
    ".aws/credentials",
    ".kube/config",
];
/// Words in a file name that make it a credential file.
const CRED_WORDS: &[&str] = &["token", "credential", "secret", "apikey"];
/// Folders that the check skips by name: caches, packages and builds.
const SKIP_NAMES: &[&str] = &[
    ".cache",
    ".npm",
    "node_modules",
    "target",
    ".git",
    ".rustup",
    "site-packages",
];
/// Folders that the check skips by their path relative to the home.
const SKIP_PATHS: &[&str] = &[".cargo/registry", ".local/share/mise"];

/// A new credential file: its path relative to the sandbox home and its size.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredFile {
    pub path: String,
    pub size: u64,
}

/// Whether a path relative to the home names a credential file.
pub fn is_credential(rel: &str) -> bool {
    if rel.starts_with(".ssh/") || CRED_PATHS.contains(&rel) {
        return true;
    }
    let name = base(rel);
    let low = name.to_ascii_lowercase();
    crate::detect::is_secret_name(name) || CRED_WORDS.iter().any(|w| low.contains(w))
}

/// Size and change time of one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    size: u64,
    mtime: (i64, i64),
}

/// The regular files below `home` that `keep` accepts, by their path
/// relative to `home`. It never follows a symlink.
fn scan(home: &Path, keep: &dyn Fn(&str) -> bool) -> BTreeMap<String, Stamp> {
    use std::os::unix::fs::MetadataExt;
    let mut out = BTreeMap::new();
    let mut left = MAX_ENTRIES;
    let mut stack = vec![(home.to_path_buf(), String::new())];
    while let Some((dir, rel)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            if left == 0 {
                return out;
            }
            left -= 1;
            let name = e.file_name().to_string_lossy().into_owned();
            let r = if rel.is_empty() {
                name.clone()
            } else {
                format!("{rel}/{name}")
            };
            // The file type of a directory entry is that of the link itself.
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                if !SKIP_NAMES.contains(&name.as_str()) && !SKIP_PATHS.contains(&r.as_str()) {
                    stack.push((e.path(), r));
                }
            } else if ft.is_file() && keep(&r) {
                if let Ok(m) = e.metadata() {
                    let mtime = (m.mtime(), m.mtime_nsec());
                    out.insert(
                        r,
                        Stamp {
                            size: m.len(),
                            mtime,
                        },
                    );
                }
            }
        }
    }
    out
}

/// The credential files of a sandbox home, as seen at the last look.
pub struct Creds {
    home: PathBuf,
    /// The login files of the harness profile (`state.auth`), relative to
    /// the home. A name that ends in `/` is a folder.
    login: Vec<String>,
    snap: BTreeMap<String, Stamp>,
}

impl Creds {
    /// Take the first snapshot.
    pub fn new(home: PathBuf, login: Vec<String>) -> Self {
        let mut c = Creds {
            home,
            login,
            snap: BTreeMap::new(),
        };
        c.snap = c.scan();
        c
    }

    fn is_login(&self, rel: &str) -> bool {
        self.login.iter().any(|l| match l.strip_suffix('/') {
            Some(dir) => rel
                .strip_prefix(dir)
                .is_some_and(|rest| rest.starts_with('/')),
            None => rel == l,
        })
    }

    fn scan(&self) -> BTreeMap<String, Stamp> {
        scan(&self.home, &|r| is_credential(r) || self.is_login(r))
    }

    /// Look again. It returns the credential files that are new or changed
    /// since the last look, and the number of login files that changed.
    pub fn check(&mut self) -> (Vec<CredFile>, u64) {
        let now = self.scan();
        let mut found = Vec::new();
        let mut refreshed = 0;
        for (rel, st) in &now {
            if self.snap.get(rel) == Some(st) {
                continue;
            }
            if self.is_login(rel) {
                refreshed += 1;
            } else {
                found.push(CredFile {
                    path: rel.clone(),
                    size: st.size,
                });
            }
        }
        self.snap = now;
        (found, refreshed)
    }
}

/// Bytes of a sandbox: totals since its start, and the sent bytes of the
/// current hour (`hour` counts hours since 1970).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Traffic {
    pub sent: u64,
    pub received: u64,
    #[serde(default)]
    pub hour: u64,
    #[serde(default)]
    pub hour_sent: u64,
}

impl Traffic {
    /// Add one step at the time `now`. When the hour changed, it returns
    /// the hour that ended and its sent bytes, if it sent any.
    pub fn add(&mut self, now: u64, received: u64, sent: u64) -> Option<(u64, u64)> {
        let hour = now / 3600;
        let mut ended = None;
        if hour != self.hour {
            if self.hour_sent > 0 {
                ended = Some((self.hour, self.hour_sent));
            }
            self.hour = hour;
            self.hour_sent = 0;
        }
        self.sent = self.sent.saturating_add(sent);
        self.received = self.received.saturating_add(received);
        self.hour_sent = self.hour_sent.saturating_add(sent);
        ended
    }
}

/// Received and sent bytes of all interfaces but `lo`, from the text of
/// `/proc/PID/net/dev`.
pub fn parse_net_dev(text: &str) -> (u64, u64) {
    let mut rx = 0u64;
    let mut tx = 0u64;
    for line in text.lines() {
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        if name.trim() == "lo" || name.contains('|') {
            continue;
        }
        let f: Vec<u64> = rest
            .split_whitespace()
            .map(|w| w.parse().unwrap_or(0))
            .collect();
        if f.len() >= 9 {
            rx = rx.saturating_add(f[0]);
            tx = tx.saturating_add(f[8]);
        }
    }
    (rx, tx)
}

/// Turns counter readings into steps. A counter that went down started
/// again from zero (a new namespace, or a wrap), so the step is the new
/// reading: a step is never negative.
#[derive(Debug, Default)]
pub struct Counter {
    last: Option<(u64, u64)>,
}

impl Counter {
    /// The received and sent bytes since the last reading.
    pub fn step(&mut self, now: (u64, u64)) -> (u64, u64) {
        let (r0, s0) = self.last.unwrap_or((0, 0));
        self.last = Some(now);
        let d = |new: u64, old: u64| new.checked_sub(old).unwrap_or(new);
        (d(now.0, r0), d(now.1, s0))
    }
}

/// The sent bytes per hour of one harness in one project, over all its
/// sandboxes: `[hour, bytes]` pairs, hours with traffic only.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Baseline {
    #[serde(default)]
    pub hours: Vec<(u64, u64)>,
}

/// The baseline file of a harness in a project.
pub fn baseline_file(env: &HostEnv, harness: &str, project: &Path) -> PathBuf {
    let name: String = harness
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    env.paddock_state().join("monitor").join(format!(
        "{name}-{}.json",
        crate::kv::shard::project_hash(project)
    ))
}

impl Baseline {
    pub fn load(file: &Path) -> Self {
        std::fs::read_to_string(file)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, file: &Path) -> std::io::Result<()> {
        if let Some(d) = file.parent() {
            crate::privfs::dir(d)?;
        }
        crate::privfs::write_atomic(file, serde_json::to_string(self).unwrap_or_default())
    }

    /// Add the sent bytes of an hour, and drop hours older than 30 days
    /// before `now_hour`.
    pub fn add(&mut self, hour: u64, sent: u64, now_hour: u64) {
        if sent > 0 {
            match self.hours.iter_mut().find(|(h, _)| *h == hour) {
                Some(e) => e.1 = e.1.saturating_add(sent),
                None => self.hours.push((hour, sent)),
            }
        }
        let oldest = now_hour.saturating_sub(KEEP_HOURS);
        self.hours.retain(|(h, b)| *h >= oldest && *b > 0);
        self.hours.sort_unstable();
    }

    /// The hours with traffic.
    pub fn busy(&self) -> usize {
        self.hours.iter().filter(|(_, b)| *b > 0).count()
    }

    /// The median of the hours with traffic.
    pub fn median(&self) -> Option<u64> {
        let mut v: Vec<u64> = self
            .hours
            .iter()
            .map(|(_, b)| *b)
            .filter(|b| *b > 0)
            .collect();
        if v.is_empty() {
            return None;
        }
        v.sort_unstable();
        let n = v.len();
        Some(if n % 2 == 1 {
            v[n / 2]
        } else {
            ((v[n / 2 - 1] as u128 + v[n / 2] as u128) / 2) as u64
        })
    }
}

/// The alert reasons for the sent bytes of the current hour.
pub fn reasons(hour_sent: u64, cfg: &MonitorCfg, base: &Baseline) -> Vec<&'static str> {
    let mut out = Vec::new();
    if hour_sent > cfg.upload_per_hour {
        out.push("upload_per_hour");
    }
    if base.busy() >= MIN_HOURS {
        if let Some(m) = base.median() {
            if hour_sent as f64 > cfg.factor * m as f64 {
                out.push("factor");
            }
        }
    }
    out
}

/// The cgroup folder of a sandbox's scope, from the cgroup of its first
/// process. None without a paddock scope.
fn scope_dir(pid: u32, unit: &str) -> Option<PathBuf> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    let path = text.lines().find_map(|l| l.strip_prefix("0::"))?;
    if base(path) != unit || !crate::freeze::ours(unit) || path.contains("..") {
        return None;
    }
    Some(PathBuf::from("/sys/fs/cgroup").join(path.trim_start_matches('/')))
}

/// The processes in a cgroup folder and in the folders below it.
fn procs(dir: &Path, depth: u32, out: &mut Vec<u32>) {
    if let Ok(t) = std::fs::read_to_string(dir.join("cgroup.procs")) {
        out.extend(t.lines().filter_map(|l| l.trim().parse::<u32>().ok()));
    }
    if depth == 0 {
        return;
    }
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                procs(&e.path(), depth - 1, out);
            }
        }
    }
}

fn cmdline(pid: u32) -> Option<Vec<String>> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let argv: Vec<String> = raw
        .split(|b| *b == 0)
        .filter(|w| !w.is_empty())
        .map(|w| String::from_utf8_lossy(w).into_owned())
        .collect();
    (!argv.is_empty()).then_some(argv)
}

/// The process of a scope whose network namespace is not ours: its
/// `/proc/PID/net/dev` shows the sandbox's own interfaces.
fn sandbox_net(pids: &[u32]) -> Option<(u64, u64)> {
    let ns = |p: &str| std::fs::read_link(format!("/proc/{p}/ns/net")).ok();
    let host = ns("self")?;
    pids.iter().find_map(|p| {
        if ns(&p.to_string())? == host {
            return None;
        }
        let text = std::fs::read_to_string(format!("/proc/{p}/net/dev")).ok()?;
        Some(parse_net_dev(&text))
    })
}

/// The traffic state of one sandbox, shared by the watch and `finish`.
struct Live {
    seen: Seen,
    counter: Counter,
    base_file: PathBuf,
    done: bool,
    creds: Option<Creds>,
    /// The last credential check ran.
    creds_done: bool,
}

impl Live {
    /// Add the sent bytes of an hour to the baseline file.
    fn flush(&self, hour: u64, sent: u64) {
        let mut b = Baseline::load(&self.base_file);
        b.add(hour, sent, crate::state::now() / 3600);
        if let Err(e) = b.save(&self.base_file) {
            log::warn!("cannot write {}: {e}", self.base_file.display());
        }
    }

    /// Count one reading. It returns the new alert reasons, the sent bytes
    /// of this hour and the baseline median.
    fn count(
        &mut self,
        reading: (u64, u64),
        cfg: &MonitorCfg,
    ) -> (Vec<&'static str>, u64, Option<u64>) {
        let (rx, tx) = self.counter.step(reading);
        let t = self.seen.rec.traffic.get_or_insert_with(Traffic::default);
        let ended = t.add(crate::state::now(), rx, tx);
        let (hour, hour_sent) = (t.hour, t.hour_sent);
        if let Some((h, b)) = ended {
            self.flush(h, b);
        }
        self.seen.save();
        let base = Baseline::load(&self.base_file);
        let new = self
            .seen
            .fresh_alerts(reasons(hour_sent, cfg, &base), hour, hour_sent);
        (new, hour_sent, base.median())
    }

    /// Look for new credential files. It returns the ones that are new for
    /// this sandbox.
    fn check_creds(&mut self) -> Vec<CredFile> {
        let Some(c) = self.creds.as_mut() else {
            return Vec::new();
        };
        let (found, refreshed) = c.check();
        self.seen.fresh_creds(found, refreshed)
    }

    /// Add the current hour to the baseline, once.
    fn finish(&mut self) {
        if self.done {
            return;
        }
        self.done = true;
        if let Some(t) = self.seen.rec.traffic.as_mut() {
            let (h, b) = (t.hour, t.hour_sent);
            t.hour_sent = 0;
            if b > 0 {
                self.flush(h, b);
            }
        }
    }
}

/// The handle of a running watch. `finish` at the end of the sandbox looks
/// for credential files a last time, adds its last hour to the baseline and
/// stops the watch.
#[derive(Clone)]
pub struct Watch {
    live: Arc<Mutex<Live>>,
    env: HostEnv,
    id: Identity,
    cfg: MonitorCfg,
}

impl Watch {
    pub fn finish(&self) {
        let new = {
            let Ok(mut l) = self.live.lock() else { return };
            let new = if l.creds_done {
                Vec::new()
            } else {
                l.creds_done = true;
                l.check_creds()
            };
            l.finish();
            new
        };
        report_creds(&self.env, &self.id, &new, self.cfg.notify);
    }

    /// Start the credential check of the sandbox home `home` (its host
    /// path), with the login files of the harness profile. Call it after
    /// paddock filled the home, so that the snapshot holds what paddock
    /// put there.
    pub fn credentials(&self, home: PathBuf, login: Vec<String>) {
        if !self.cfg.credentials || !home.is_dir() {
            return;
        }
        let creds = Creds::new(home, login);
        match self.live.lock() {
            Ok(mut l) => l.creds = Some(creds),
            Err(_) => return,
        }
        let w = self.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(CRED_INTERVAL);
            let new = {
                let Ok(mut l) = w.live.lock() else { return };
                if l.creds_done {
                    return;
                }
                l.check_creds()
            };
            report_creds(&w.env, &w.id, &new, w.cfg.notify);
        });
    }
}

/// Start the watch for a sandbox. The tunnel and traffic part runs unless
/// the user turned both off.
pub fn watch(env: HostEnv, id: Identity) -> Watch {
    let cfg = MonitorCfg::load(&env);
    let live = Arc::new(Mutex::new(Live {
        seen: Seen::open(seen_file(&env, &id.sandbox)),
        counter: Counter::default(),
        base_file: baseline_file(&env, &id.harness, &id.project),
        done: false,
        creds: None,
        creds_done: false,
    }));
    let handle = Watch {
        live: live.clone(),
        env: env.clone(),
        id: id.clone(),
        cfg,
    };
    if !cfg.tunnels && !cfg.traffic {
        return handle;
    }
    let shared = live;
    std::thread::spawn(move || {
        let mut dir: Option<PathBuf> = None;
        let mut tries = 0;
        loop {
            std::thread::sleep(if dir.is_none() {
                Duration::from_millis(500)
            } else {
                INTERVAL
            });
            if dir.is_none() {
                // The sandbox registers after it reached its scope.
                let r = crate::state::Running::list(&env)
                    .into_iter()
                    .find(|r| r.id == id.sandbox);
                dir = r.and_then(|r| scope_dir(r.pid, r.unit.as_deref()?));
                if dir.is_none() {
                    tries += 1;
                    if tries > 20 {
                        log::debug!("no scope for {}, so no monitor watch", id.sandbox);
                        return;
                    }
                    continue;
                }
            }
            let Some(d) = dir.as_deref() else { continue };
            let Ok(mut live) = shared.lock() else { return };
            if live.done {
                return;
            }
            if !d.is_dir() {
                live.finish();
                return;
            }
            let mut pids = Vec::new();
            procs(d, 4, &mut pids);
            if cfg.tunnels {
                let found = pids
                    .iter()
                    .filter_map(|p| tunnel_in(&cmdline(*p)?))
                    .collect();
                for t in live.seen.fresh(found) {
                    report(&env, &id, &t, cfg.notify);
                }
            }
            if cfg.traffic {
                if let Some(reading) = sandbox_net(&pids) {
                    let (new, hour_sent, median) = live.count(reading, &cfg);
                    for r in new {
                        alert(&env, &id, r, hour_sent, median, &cfg);
                    }
                }
            }
        }
    });
    handle
}

/// A byte count for people: `512 B`, `151 KB`, `4.2 MB`, `5.0 GB` (1024-based).
pub fn size(b: u64) -> String {
    const K: f64 = 1024.0;
    let f = b as f64;
    if f < K {
        format!("{b} B")
    } else if f < K * K {
        format!("{:.0} KB", f / K)
    } else if f < K * K * K {
        format!("{:.1} MB", f / (K * K))
    } else {
        format!("{:.1} GB", f / (K * K * K))
    }
}

/// One alert: an audit line with numbers only, and a notification.
fn alert(
    env: &HostEnv,
    id: &Identity,
    reason: &str,
    hour_sent: u64,
    median: Option<u64>,
    cfg: &MonitorCfg,
) {
    let detail = match reason {
        "factor" => format!(
            "{} sent {} this hour, more than {} times its usual {}",
            id.sandbox,
            size(hour_sent),
            cfg.factor,
            size(median.unwrap_or(0))
        ),
        _ => format!(
            "{} sent {} this hour, more than the limit of {}",
            id.sandbox,
            size(hour_sent),
            size(cfg.upload_per_hour)
        ),
    };
    crate::broker::audit_line(
        env,
        &serde_json::json!({
            "time": crate::state::now(),
            "sandbox": id.sandbox,
            "harness": id.harness,
            "project": id.project,
            "capability": CAPABILITY,
            "reason": reason,
            "hour_sent": hour_sent,
            "upload_per_hour": cfg.upload_per_hour,
            "factor": cfg.factor,
            "baseline_median": median,
            "detail": detail,
            "decision": "reported",
        }),
    );
    if notify_now(env, &id.project, cfg.notify) {
        let _ = crate::broker::notify::show(
            env,
            &crate::broker::notify::Note {
                summary: "a sandbox sends much data".into(),
                body: format!(
                    "{detail}. paddock does not know what it sends. paddock stop {} ends the sandbox.",
                    id.sandbox
                ),
                actions: Vec::new(),
                critical: false,
            },
        );
    }
}

/// New credential files: one audit line per file, with its path and size,
/// and one notification for all of them.
fn report_creds(env: &HostEnv, id: &Identity, files: &[CredFile], notify: bool) {
    if files.is_empty() {
        return;
    }
    for f in files {
        crate::broker::audit_line(
            env,
            &serde_json::json!({
                "time": crate::state::now(),
                "sandbox": id.sandbox,
                "harness": id.harness,
                "project": id.project,
                "capability": CAPABILITY,
                "path": f.path,
                "size": f.size,
                "detail": format!(
                    "{} has a new credential file: ~/{} ({})",
                    id.sandbox,
                    f.path,
                    size(f.size)
                ),
                "decision": "reported",
            }),
        );
    }
    if notify_now(env, &id.project, notify) {
        let mut names: Vec<String> = files
            .iter()
            .take(3)
            .map(|f| format!("~/{}", f.path))
            .collect();
        if files.len() > 3 {
            names.push(format!("and {} more", files.len() - 3));
        }
        let _ = crate::broker::notify::show(
            env,
            &crate::broker::notify::Note {
                summary: "new credential files in a sandbox".into(),
                body: format!(
                    "{} wrote {}. A key or token in its home can keep access later. paddock log shows the list.",
                    id.sandbox,
                    names.join(", ")
                ),
                actions: Vec::new(),
                critical: false,
            },
        );
    }
}

fn report(env: &HostEnv, id: &Identity, t: &Tunnel, notify: bool) {
    let flags = if t.flags.is_empty() {
        String::new()
    } else {
        format!(" {}", t.flags.join(" "))
    };
    let detail = format!("{} opened a tunnel: {}{flags}", id.sandbox, t.tool);
    crate::broker::audit_line(
        env,
        &serde_json::json!({
            "time": crate::state::now(),
            "sandbox": id.sandbox,
            "harness": id.harness,
            "project": id.project,
            "capability": CAPABILITY,
            "tool": t.tool,
            "flags": t.flags,
            "detail": detail,
            "decision": "reported",
        }),
    );
    if notify_now(env, &id.project, notify) {
        let _ = crate::broker::notify::show(
            env,
            &crate::broker::notify::Note {
                summary: "a sandbox opened a tunnel".into(),
                body: format!(
                    "{detail}. It can reach or expose this machine from outside. paddock stop {} ends the sandbox.",
                    id.sandbox
                ),
                actions: Vec::new(),
                critical: false,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_for_people() {
        assert_eq!(size(512), "512 B");
        assert_eq!(size(154_786), "151 KB");
        assert_eq!(size(5 << 30), "5.0 GB");
        assert_eq!(size(3 << 20), "3.0 MB");
    }

    fn argv(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    fn tool_of(s: &str) -> Option<&'static str> {
        tunnel_in(&argv(s)).map(|t| t.tool)
    }

    #[test]
    fn each_tool_matches_by_name() {
        for (cmd, tool) in [
            ("/usr/local/bin/ngrok http 3000", "ngrok"),
            ("cloudflared tunnel run", "cloudflared"),
            ("frpc -c frpc.toml", "frpc"),
            ("frps", "frps"),
            ("bore local 8000 --to example.org", "bore"),
            ("chisel client example.org R:80", "chisel"),
            ("lt --port 3000", "localtunnel"),
            ("localtunnel --port 3000", "localtunnel"),
            (
                "node /usr/lib/node_modules/.bin/lt --port 3000",
                "localtunnel",
            ),
            ("tailscale funnel 443", "tailscale funnel"),
        ] {
            assert_eq!(tool_of(cmd), Some(tool), "{cmd}");
        }
    }

    #[test]
    fn ssh_matches_tunnel_flags_only() {
        let t = tunnel_in(&argv("ssh -R 8080:localhost:80 host")).unwrap();
        assert_eq!((t.tool, t.flags), ("ssh", vec!["-R".to_string()]));
        assert_eq!(
            tunnel_in(&argv("/usr/bin/ssh -L8080:a:80 host"))
                .unwrap()
                .flags,
            ["-L"]
        );
        assert_eq!(tunnel_in(&argv("ssh -D 1080 host")).unwrap().flags, ["-D"]);
        assert_eq!(
            tunnel_in(&argv("ssh -NR 9000:a:22 host")).unwrap().flags,
            ["-R"]
        );
        assert_eq!(
            tunnel_in(&argv("ssh -fNL 1:a:2 -D 3 host")).unwrap().flags,
            ["-L", "-D"]
        );
        // Options after the host count too.
        assert_eq!(tunnel_in(&argv("ssh host -R 1:a:2")).unwrap().flags, ["-R"]);
    }

    #[test]
    fn normal_commands_do_not_match() {
        for cmd in [
            "ssh host",
            "ssh -p 22 -i key host",
            // The argument of -l, and the remote command, are not flags.
            "ssh -l R host",
            "ssh -o ProxyJump=x host",
            "ssh host ls -R /",
            "ssh -- host -R",
            "git push",
            "cargo build -L",
            "tailscale status",
            "sleep 10",
            "bash -c ls",
            "",
        ] {
            assert_eq!(tool_of(cmd), None, "{cmd}");
        }
    }

    #[test]
    fn the_config_switches() {
        assert_eq!(
            MonitorCfg::parse(""),
            MonitorCfg {
                tunnels: true,
                notify: true,
                traffic: true,
                upload_per_hour: 5 << 30,
                factor: 10.0,
                credentials: true,
            }
        );
        let off = MonitorCfg::parse(
            "[monitor]\ntunnels = false\nnotify = false\ntraffic = false\nupload_per_hour = \"300M\"\nfactor = 4\ncredentials = false\n",
        );
        assert!(!off.tunnels && !off.notify && !off.traffic && !off.credentials);
        assert_eq!((off.upload_per_hour, off.factor), (300 << 20, 4.0));
        let num = MonitorCfg::parse("[monitor]\nupload_per_hour = 1000\nfactor = 2.5\n");
        assert_eq!((num.upload_per_hour, num.factor), (1000, 2.5));
        // Bad values fall back to the defaults.
        let bad = MonitorCfg::parse("[monitor]\nupload_per_hour = \"lots\"\nfactor = 0\n");
        assert_eq!((bad.upload_per_hour, bad.factor), (5 << 30, 10.0));
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("5G"), Some(5 << 30));
        assert_eq!(parse_size("300m"), Some(300 << 20));
        assert_eq!(parse_size("64K"), Some(64 << 10));
        assert_eq!(parse_size(" 1000 "), Some(1000));
        for bad in ["", "G", "5T", "-1G", "1.5G", "lots"] {
            assert_eq!(parse_size(bad), None, "{bad}");
        }
        assert_eq!(parse_size("99999999999G"), None);
    }

    const NET_DEV: &str = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo: 9000000      10    0    0    0     0          0         0  9000000      10    0    0    0     0       0          0
  tap0:    1500       5    0    0    0     0          0         0      700       4    0    0    0     0       0          0
  eth1:     500       1    0    0    0     0          0         0      300       2    0    0    0     0       0          0
";

    #[test]
    fn net_dev_skips_lo() {
        assert_eq!(parse_net_dev(NET_DEV), (2000, 1000));
        assert_eq!(parse_net_dev(""), (0, 0));
        assert_eq!(parse_net_dev("garbage: 1 2\n"), (0, 0));
    }

    #[test]
    fn a_counter_never_steps_back() {
        let mut c = Counter::default();
        assert_eq!(c.step((100, 50)), (100, 50));
        assert_eq!(c.step((150, 80)), (50, 30));
        assert_eq!(c.step((150, 80)), (0, 0));
        // A new namespace starts from zero: the reading is the step.
        assert_eq!(c.step((10, 5)), (10, 5));
        assert_eq!(c.step((20, 5)), (10, 0));
    }

    #[test]
    fn the_hourly_window() {
        let h = 500_000u64;
        let mut t = Traffic::default();
        assert_eq!(t.add(h * 3600 + 10, 7, 100), None);
        assert_eq!(t.add(h * 3600 + 3599, 3, 50), None);
        assert_eq!((t.hour, t.hour_sent, t.sent, t.received), (h, 150, 150, 10));
        // The next hour starts at zero and hands back the hour that ended.
        assert_eq!(t.add((h + 1) * 3600, 0, 20), Some((h, 150)));
        assert_eq!((t.hour, t.hour_sent, t.sent), (h + 1, 20, 170));
        // An hour without sent bytes hands back nothing.
        assert_eq!(t.add((h + 2) * 3600, 5, 0), Some((h + 1, 20)));
        assert_eq!(t.add((h + 3) * 3600, 5, 0), None);
    }

    #[test]
    fn the_baseline_median() {
        let mut b = Baseline::default();
        assert_eq!(b.median(), None);
        let now = 1000;
        for (h, sent) in [(990, 10), (991, 0), (992, 30), (993, 20)] {
            b.add(h, sent, now);
        }
        assert_eq!(b.busy(), 3);
        assert_eq!(b.median(), Some(20));
        // Two sandboxes in the same hour add up.
        b.add(993, 20, now);
        assert_eq!(b.hours, [(990, 10), (992, 30), (993, 40)]);
        b.add(994, 50, now);
        assert_eq!(b.median(), Some(35));
        // Older than 30 days goes.
        b.add(1700, 1, 991 + 30 * 24);
        assert_eq!(b.hours.first(), Some(&(992, 30)));
        // The file holds numbers only, in a temp folder.
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("monitor/h-1.json");
        b.save(&file).unwrap();
        assert_eq!(Baseline::load(&file), b);
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text
            .chars()
            .all(|c| c.is_ascii_digit() || "{}[],:\"hours".contains(c)));
    }

    fn base_of(hours: &[u64]) -> Baseline {
        Baseline {
            hours: hours
                .iter()
                .enumerate()
                .map(|(i, b)| (i as u64, *b))
                .collect(),
        }
    }

    #[test]
    fn both_alert_rules() {
        let cfg = MonitorCfg::parse("[monitor]\nupload_per_hour = \"1M\"\nfactor = 10\n");
        let none = Baseline::default();
        assert!(reasons(1 << 20, &cfg, &none).is_empty());
        assert_eq!(reasons((1 << 20) + 1, &cfg, &none), ["upload_per_hour"]);
        // Four hours are not a baseline yet.
        let four = base_of(&[100, 100, 100, 100]);
        assert!(reasons(5000, &cfg, &four).is_empty());
        let five = base_of(&[100, 100, 100, 100, 100]);
        assert!(reasons(1000, &cfg, &five).is_empty());
        assert_eq!(reasons(1001, &cfg, &five), ["factor"]);
        assert_eq!(reasons(2 << 20, &cfg, &five), ["upload_per_hour", "factor"]);
    }

    #[test]
    fn an_alert_counts_once_per_sandbox_and_reason() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("monitor.json");
        let mut seen = Seen::open(file.clone());
        assert_eq!(seen.fresh_alerts(vec!["factor"], 7, 100), ["factor"]);
        assert!(seen.fresh_alerts(vec!["factor"], 7, 100).is_empty());
        assert_eq!(
            seen.fresh_alerts(vec!["factor", "upload_per_hour"], 7, 100),
            ["upload_per_hour"]
        );
        let mut again = Seen::open(file);
        assert!(again
            .fresh_alerts(vec!["upload_per_hour", "factor"], 7, 100)
            .is_empty());
        let mut other = Seen::open(tmp.path().join("other.json"));
        assert_eq!(other.fresh_alerts(vec!["factor"], 7, 100), ["factor"]);
    }

    #[test]
    fn traffic_goes_to_the_run_folder_and_the_baseline() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = MonitorCfg::parse("[monitor]\nupload_per_hour = \"1K\"\n");
        let mut live = Live {
            seen: Seen::open(tmp.path().join("run/monitor.json")),
            counter: Counter::default(),
            base_file: tmp.path().join("state/monitor/h-1.json"),
            done: false,
            creds: None,
            creds_done: false,
        };
        let (new, sent, _) = live.count((300, 200), &cfg);
        assert!(new.is_empty());
        assert_eq!(sent, 200);
        let (new, sent, _) = live.count((400, 2000), &cfg);
        assert_eq!((new, sent), (vec!["upload_per_hour"], 2000));
        let (new, _, _) = live.count((500, 3000), &cfg);
        assert!(new.is_empty());
        let rec = load_record(&tmp.path().join("run/monitor.json"));
        let t = rec.traffic.unwrap();
        assert_eq!((t.sent, t.received), (3000, 500));
        // The alert flag and the bytes of its hour, for the panel.
        assert!(rec.alerts.contains("upload_per_hour"));
        assert_eq!(rec.alert_sent, 3000);
        assert_eq!(rec.alert_hour, t.hour);
        live.finish();
        live.finish();
        let b = Baseline::load(&live.base_file);
        assert_eq!(b.hours.iter().map(|h| h.1).sum::<u64>(), 3000);
    }

    #[test]
    fn a_tool_counts_once_per_sandbox() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("monitor.json");
        let ngrok = || tunnel_in(&argv("ngrok http 80")).unwrap();
        let ssh = || tunnel_in(&argv("ssh -R 1:a:2 h")).unwrap();
        let mut seen = Seen::open(file.clone());
        assert_eq!(seen.fresh(vec![ngrok(), ngrok()]).len(), 1);
        assert!(seen.fresh(vec![ngrok()]).is_empty());
        assert_eq!(seen.fresh(vec![ssh(), ngrok()]).len(), 1);
        // A new watch reads what the run folder holds.
        let mut again = Seen::open(file.clone());
        assert!(again.fresh(vec![ngrok(), ssh()]).is_empty());
        assert_eq!(
            load_seen(&file).into_iter().collect::<Vec<_>>(),
            ["ngrok", "ssh"]
        );
        // Another sandbox has a file of its own.
        let mut other = Seen::open(tmp.path().join("other.json"));
        assert_eq!(other.fresh(vec![ngrok()]).len(), 1);
    }

    fn put(home: &Path, rel: &str, body: &str) {
        let f = home.join(rel);
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, body).unwrap();
    }

    fn paths(files: &[CredFile]) -> Vec<&str> {
        files.iter().map(|f| f.path.as_str()).collect()
    }

    #[test]
    fn credential_names() {
        for yes in [
            ".ssh/id_ed25519",
            ".ssh/authorized_keys",
            ".ssh/config",
            ".netrc",
            ".git-credentials",
            ".config/gh/hosts.yml",
            ".docker/config.json",
            ".aws/credentials",
            ".kube/config",
            "work/server.pem",
            ".config/x/GitHub_Token.txt",
            "notes/apikey",
        ] {
            assert!(is_credential(yes), "{yes}");
        }
        for no in [".bashrc", ".config/x/config.json", ".gitconfig"] {
            assert!(!is_credential(no), "{no}");
        }
    }

    #[test]
    fn new_credential_files_are_found_and_login_refreshes_counted() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        put(&home, ".bashrc", "x");
        put(&home, ".fake/auth.json", "login-1");
        put(&home, ".fake/keys/a", "k");
        put(&home, ".ssh/known_hosts", "h");
        let mut c = Creds::new(
            home.clone(),
            vec![".fake/auth.json".into(), ".fake/keys/".into()],
        );
        assert_eq!(c.check(), (vec![], 0));
        // The harness refreshes its token and adds a key to its login folder.
        put(&home, ".fake/auth.json", "login-2-longer");
        put(&home, ".fake/keys/b", "k");
        assert_eq!(c.check(), (vec![], 2));
        // A planted key, an authorized key and a file in a cache.
        put(&home, ".ssh/id_ed25519", "PRIVATE");
        put(&home, ".ssh/authorized_keys", "ssh-ed25519 AAA");
        put(&home, ".cache/x/token", "t");
        put(&home, "proj/node_modules/m/secret.js", "s");
        let (found, n) = c.check();
        assert_eq!(n, 0);
        assert_eq!(paths(&found), [".ssh/authorized_keys", ".ssh/id_ed25519"]);
        assert_eq!(found[1].size, 7);
        // A change to a credential file that is not a login is news.
        put(&home, ".ssh/known_hosts", "h2");
        assert_eq!(paths(&c.check().0), [".ssh/known_hosts"]);
        assert_eq!(c.check(), (vec![], 0));
    }

    #[test]
    fn a_symlink_is_not_followed() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let outside = tmp.path().join("outside");
        put(&outside, ".ssh/id_rsa", "k");
        put(&outside, "token", "t");
        std::fs::create_dir_all(&home).unwrap();
        let mut c = Creds::new(home.clone(), vec![]);
        std::os::unix::fs::symlink(outside.join(".ssh"), home.join(".ssh")).unwrap();
        std::os::unix::fs::symlink(outside.join("token"), home.join("token")).unwrap();
        assert_eq!(c.check(), (vec![], 0));
    }

    #[test]
    fn a_credential_file_counts_once_per_sandbox() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("monitor.json");
        let key = |size| CredFile {
            path: ".ssh/id_ed25519".into(),
            size,
        };
        let mut seen = Seen::open(file.clone());
        assert_eq!(seen.fresh_creds(vec![key(7)], 0), [key(7)]);
        assert!(seen.fresh_creds(vec![key(9)], 2).is_empty());
        let mut again = Seen::open(file.clone());
        assert!(again.fresh_creds(vec![key(9)], 1).is_empty());
        let rec = load_record(&file);
        assert_eq!(rec.credential_files, [key(7)]);
        assert_eq!(rec.login_refreshes, 3);
    }

    #[test]
    fn the_audit_line_has_path_and_size_but_no_content() {
        let tmp = tempfile::tempdir().unwrap();
        let env = HostEnv::from_parts(
            tmp.path().join("host"),
            "u".into(),
            1,
            1,
            Default::default(),
        );
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let id = Identity {
            sandbox: "s-1".into(),
            harness: "fake".into(),
            project: tmp.path().join("p"),
            pasture: None,
            parent: None,
            chain: Vec::new(),
            tree: None,
        };
        let cfg = MonitorCfg {
            notify: false,
            ..MonitorCfg::parse("")
        };
        let w = Watch {
            live: Arc::new(Mutex::new(Live {
                seen: Seen::open(tmp.path().join("run/monitor.json")),
                counter: Counter::default(),
                base_file: tmp.path().join("base.json"),
                done: false,
                creds: None,
                creds_done: false,
            })),
            env: env.clone(),
            id,
            cfg,
        };
        w.credentials(home.clone(), vec![]);
        let secret = "SECRET-KEY-MATERIAL-1234";
        put(&home, ".ssh/id_ed25519", secret);
        w.finish();
        w.finish();
        let log = std::fs::read_to_string(crate::broker::audit_file(&env)).unwrap();
        assert_eq!(log.lines().count(), 1);
        let line: serde_json::Value = serde_json::from_str(log.trim()).unwrap();
        assert_eq!(line["capability"], CAPABILITY);
        assert_eq!(line["path"], ".ssh/id_ed25519");
        assert_eq!(line["size"], secret.len());
        assert!(!log.contains(secret));
    }

    #[test]
    fn the_alert_keeps_the_bytes_of_its_hour() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("monitor.json");
        let mut seen = Seen::open(file.clone());
        assert_eq!(
            seen.fresh_alerts(vec!["upload_per_hour"], 10, 6 << 30),
            ["upload_per_hour"]
        );
        // Later in the same hour the bytes follow.
        assert!(seen
            .fresh_alerts(vec!["upload_per_hour"], 10, 7 << 30)
            .is_empty());
        // The next hour does not change them.
        seen.fresh_alerts(vec![], 11, 1 << 30);
        let text = std::fs::read_to_string(&file).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["alerts"], serde_json::json!(["upload_per_hour"]));
        assert_eq!(v["alert_sent"], 7u64 << 30);
        assert_eq!(v["alert_hour"], 10);
    }

    fn test_env(tmp: &Path) -> HostEnv {
        let mut vars = BTreeMap::new();
        vars.insert(
            "XDG_RUNTIME_DIR".to_string(),
            tmp.join("run").display().to_string(),
        );
        HostEnv::from_parts(tmp.join("home"), "demo".into(), 1000, 1000, vars)
    }

    #[test]
    fn ack_clears_the_mark_until_a_new_finding() {
        let tmp = tempfile::tempdir().unwrap();
        let env = test_env(tmp.path());
        let project = Path::new("/work/app");
        let mut seen = Seen::open(seen_file(&env, "s1"));
        seen.fresh(vec![tunnel_in(&argv("ngrok http 80")).unwrap()]);
        seen.fresh_alerts(vec!["upload_per_hour"], 3, 6 << 30);
        seen.fresh_creds(
            vec![CredFile {
                path: ".ssh/id_ed25519".into(),
                size: 400,
            }],
            0,
        );
        let m = marks(&env, "s1", project);
        let texts: Vec<String> = m.iter().map(Finding::text).collect();
        assert_eq!(
            texts,
            [
                "tunnel: ngrok",
                "upload: 6.0 GB this hour",
                "new credential file: ~/.ssh/id_ed25519"
            ]
        );
        assert_eq!(ack(&env, "s1").unwrap(), 3);
        assert!(marks(&env, "s1", project).is_empty());
        // The findings stay, marked as acknowledged.
        assert!(findings(&env, "s1").iter().all(|f| f.acked));
        // A new finding marks the sandbox again, alone.
        seen.fresh(vec![tunnel_in(&argv("cloudflared tunnel run")).unwrap()]);
        let m = marks(&env, "s1", project);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].text(), "tunnel: cloudflared");
        // Another sandbox has its own acknowledgement.
        let mut other = Seen::open(seen_file(&env, "s2"));
        other.fresh(vec![tunnel_in(&argv("ngrok http 80")).unwrap()]);
        assert_eq!(marks(&env, "s2", project).len(), 1);
    }

    #[test]
    fn mute_matches_the_project_and_its_folders() {
        let mute =
            parse_mute("[monitor]\nmute = [\"/work/noisy\", \"relative/path\", \"/srv/x/\"]\n");
        assert_eq!(
            mute,
            [PathBuf::from("/work/noisy"), PathBuf::from("/srv/x/")]
        );
        assert!(is_muted(&mute, Path::new("/work/noisy")));
        assert!(is_muted(&mute, Path::new("/work/noisy/sub/dir")));
        assert!(is_muted(&mute, Path::new("/srv/x")));
        assert!(!is_muted(&mute, Path::new("/work/noisy2")));
        assert!(!is_muted(&mute, Path::new("/work")));
        assert!(!is_muted(&mute, Path::new("/elsewhere/relative/path")));
        assert!(parse_mute("").is_empty());
        assert!(parse_mute("[monitor]\nmute = \"/work\"\n").is_empty());
    }

    #[test]
    fn a_muted_project_has_no_mark() {
        let tmp = tempfile::tempdir().unwrap();
        let env = test_env(tmp.path());
        let mut seen = Seen::open(seen_file(&env, "s1"));
        seen.fresh(vec![tunnel_in(&argv("ngrok http 80")).unwrap()]);
        let project = Path::new("/work/noisy/app");
        assert_eq!(marks(&env, "s1", project).len(), 1);
        edit_mute(&env, Path::new("/work/noisy"), true).unwrap();
        assert!(marks(&env, "s1", project).is_empty());
        assert!(!notify_now(&env, project, true));
        assert!(notify_now(&env, Path::new("/work/other"), true));
        // The findings stay for the list.
        assert_eq!(findings(&env, "s1").len(), 1);
        edit_mute(&env, Path::new("/work/noisy/"), false).unwrap();
        assert_eq!(marks(&env, "s1", project).len(), 1);
    }

    #[test]
    fn mute_edits_keep_the_rest_of_the_file() {
        let text =
            "# my settings\n[monitor]\n# quiet here\nnotify = true\n\n[env]\nhide = [\"A\"]\n";
        let (out, list) = edit_mute_text(text, Path::new("/work/a/"), true).unwrap();
        assert_eq!(list, ["/work/a"]);
        assert!(out.contains("# my settings"));
        assert!(out.contains("# quiet here"));
        assert!(out.contains("hide = [\"A\"]"));
        assert_eq!(parse_mute(&out), [PathBuf::from("/work/a")]);
        // Twice is once.
        let (out, list) = edit_mute_text(&out, Path::new("/work/a"), true).unwrap();
        assert_eq!(list, ["/work/a"]);
        let (out, list) = edit_mute_text(&out, Path::new("/work/b"), true).unwrap();
        assert_eq!(list, ["/work/a", "/work/b"]);
        let (out, list) = edit_mute_text(&out, Path::new("/work/a"), false).unwrap();
        assert_eq!(list, ["/work/b"]);
        assert!(out.contains("# quiet here"));
        assert!(edit_mute_text("", Path::new("rel"), true).is_err());
        assert!(edit_mute_text("[monitor]\nmute = 3\n", Path::new("/a"), true).is_err());
        assert!(edit_mute_text("not toml [", Path::new("/a"), true).is_err());
    }
}
