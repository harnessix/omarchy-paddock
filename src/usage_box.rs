//! The box for Omarchy's `omarchy-agent-usage-update`.
//!
//! The agents panel counts sessions from the harness folders in the home and
//! checks rate limits online. Sessions and fresh logins made in a paddock
//! sandbox stay in its sandbox home. paddock runs Omarchy's command,
//! unchanged, in a bwrap box that shows every agent's data in one place:
//!
//! - the system read-only, an empty home (tmpfs), its own `/tmp`, and the
//!   user's runtime folders and session bus hidden (no host keyring);
//! - for each harness data folder from `COLLECTORS` and the profiles: an
//!   overlay at its place in the home. The lower layers are the same folder
//!   in every non-isolated sandbox home of that harness, and the host folder
//!   on top. The upper layer is a tmpfs inside the box: a collector can
//!   write there (`codex app-server` keeps a database in `~/.codex`), and
//!   the writes end with the box. When the layers do not fit in one mount,
//!   bwrap merges them in two steps (stage.rs);
//! - for each login file of `[state] auth` and `COLLECTORS`: the newest copy
//!   across the host and every non-isolated sandbox home (by token expiry when
//!   the file names one, otherwise by modification time, and from the keeper's
//!   copy for a kept login), mounted read-only on top of the overlays.
//!   Before the view is built, a lapsed login can get one renewal in a
//!   sandbox of its profile (refresh.rs);
//! - read-only: Omarchy's tree and the tool folders on PATH when they are in
//!   the home (mise's data folder and config with them, so a collector finds
//!   `codex`), and the panel's own settings in `~/.config/omarchy/agents`;
//! - read-write: only the record and cache folders of the panel;
//! - network egress only through paddock's proxy (`proxy.rs`), restricted to
//!   the endpoints of the profiles and the hosts of `COLLECTORS`.
//!
//! The rest of the home (`~/.ssh`, documents, other projects) is absent.
//! When the box cannot start, the command does not run on the host either.

mod collectors;
mod expiry;
pub mod refresh;
pub mod stage;
pub use collectors::{collector, Collector, COLLECTORS};
pub use expiry::token_expiry;

use crate::env::HostEnv;
use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The longest list of lower layers for one overlay, in bytes.
const LOWER_BUDGET: usize = 3000;

/// Where a harness keeps its data, from a path in its profile (relative to
/// HOME): the first part of the path that is not a shared XDG folder.
pub fn data_root(rel: &str) -> Option<String> {
    let rel = rel.trim_end_matches('/');
    if rel.is_empty() || rel.starts_with('/') {
        return None;
    }
    let parts: Vec<&str> = rel.split('/').collect();
    if parts
        .iter()
        .any(|p| p.is_empty() || *p == "." || *p == "..")
    {
        return None;
    }
    let depth = match parts[0] {
        ".config" => 2,
        ".local" if matches!(parts.get(1), Some(&"share") | Some(&"state")) => 3,
        ".local" | ".cache" => return None,
        _ => 1,
    };
    (parts.len() >= depth).then(|| parts[..depth].join("/"))
}

/// The harness data folders, each with the harnesses whose sandbox homes
/// hold the same folder. A folder inside another one is left out.
pub fn folders(env: &HostEnv) -> BTreeMap<String, BTreeSet<String>> {
    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for c in COLLECTORS {
        for r in c.folders.iter().filter_map(|f| data_root(f)) {
            let set = out.entry(r).or_default();
            if let Some(p) = c.profile {
                set.insert(p.to_string());
            }
        }
    }
    if let Ok(f) = crate::profile::HarnessFactory::new(env) {
        for name in f.names() {
            let Some(p) = f.profile(&name) else { continue };
            let s = &p.state;
            let mut rels: Vec<&str> = s
                .data
                .iter()
                .chain(s.auth.iter())
                .chain(s.seed.iter())
                .map(String::as_str)
                .collect();
            if let Some(a) = &s.account {
                rels.push(&a.dir);
                rels.extend(a.also.iter().map(String::as_str));
            }
            if let Some(h) = &s.history {
                rels.extend(h.project_folders.as_deref());
                rels.extend(h.copy_back.iter().map(String::as_str));
            }
            for r in rels.into_iter().filter_map(data_root) {
                out.entry(r).or_default().insert(name.clone());
            }
        }
    }
    let keys: Vec<String> = out.keys().cloned().collect();
    out.retain(|k, _| {
        !keys
            .iter()
            .any(|o| o != k && Path::new(k).starts_with(Path::new(o)))
    });
    out
}

/// Login files (relative to HOME) mapped to the profiles that own them and
/// any configured token-expiry JSON path.
pub fn login_files(env: &HostEnv) -> BTreeMap<String, (BTreeSet<String>, Option<String>)> {
    let mut out: BTreeMap<String, (BTreeSet<String>, Option<String>)> = BTreeMap::new();
    for c in COLLECTORS {
        let Some(p) = c.profile else { continue };
        for f in c.login_files.iter().filter(|f| !f.ends_with('/')) {
            out.entry((*f).to_string())
                .or_default()
                .0
                .insert(p.to_string());
        }
    }
    if let Ok(f) = crate::profile::HarnessFactory::new(env) {
        for name in f.names() {
            let Some(p) = f.profile(&name) else { continue };
            for a in p.state.auth.iter().filter(|a| !a.ends_with('/')) {
                let entry = out.entry(a.clone()).or_default();
                entry.0.insert(name.clone());
                if entry.1.is_none() {
                    entry.1 = p.state.auth_expiry.get(a).cloned();
                }
            }
        }
    }
    out
}

/// One part of the view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mount {
    /// Overlay at `dest`; `lowers` from the bottom to the top. Writes go to a
    /// tmpfs in the box and end with it.
    Overlay { dest: PathBuf, lowers: Vec<PathBuf> },
    /// The newest copy of a login file `src`, mounted read-only at `dest`.
    Login { src: PathBuf, dest: PathBuf },
    /// A host path, read-only, at the same place.
    Ro(PathBuf),
    /// A host folder, read-write, at the same place.
    Rw(PathBuf),
}

#[derive(Debug, Default)]
pub struct View {
    pub mounts: Vec<Mount>,
    /// The newest copy chosen for each login file (`dest` in home -> `src`).
    pub logins: BTreeMap<PathBuf, PathBuf>,
    /// Sandbox homes left out because an overlay had too many layers.
    pub left_out: usize,
    /// `(paddock_exe, proxy_socket)` when the proxy relay runs inside the box.
    pub proxy: Option<(PathBuf, PathBuf)>,
}

/// A real folder (or file) in a sandbox home: no symlink from the home on.
fn real_in_home(home: &Path, rel: &str, dir: bool) -> Option<PathBuf> {
    let mut p = home.to_path_buf();
    for part in rel.split('/') {
        p.push(part);
        if std::fs::symlink_metadata(&p).ok()?.file_type().is_symlink() {
            return None;
        }
    }
    let m = std::fs::symlink_metadata(&p).ok()?;
    (if dir { m.is_dir() } else { m.is_file() }).then_some(p)
}

fn is_isolated_home(login_dir: &Path, home_dir: &Path) -> bool {
    let name = login_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    if name == "isolated" || name.starts_with("isolated-") {
        return true;
    }
    std::fs::read_to_string(home_dir.join(".paddock-origin"))
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v.get("auth")?.as_str().map(str::to_string))
        .is_some_and(|a| a == "isolated" || a.starts_with("isolated:"))
}

/// The non-isolated sandbox homes of a harness: `homes/HARNESS/LOGIN/PROJECT`.
pub fn sandbox_homes(env: &HostEnv, harness: &str) -> Vec<PathBuf> {
    let root = env.paddock_data().join("homes").join(harness);
    let mut out = Vec::new();
    let dirs = |p: &Path| -> Vec<PathBuf> {
        std::fs::read_dir(p)
            .map(|r| {
                r.flatten()
                    .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                    .map(|e| e.path())
                    .collect()
            })
            .unwrap_or_default()
    };
    for login in dirs(&root) {
        for home in dirs(&login) {
            if !is_isolated_home(&login, &home) {
                out.push(home);
            }
        }
    }
    out
}

fn mtime(p: &Path) -> std::time::SystemTime {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .unwrap_or(std::time::UNIX_EPOCH)
}

/// Choose the freshest copy of `rel` across the host, the credential keeper,
/// and every non-isolated sandbox home of `harnesses`.
pub fn newest_login(
    env: &HostEnv,
    rel: &str,
    harnesses: &BTreeSet<String>,
    expiry_field: Option<&str>,
) -> Option<PathBuf> {
    let is_kept_kind = harnesses
        .iter()
        .any(|h| crate::keeper::kind_for(h).is_some_and(|k| k.file == rel));
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(host_file) = real_in_home(&env.home, rel, false) {
        candidates.push(host_file);
    }
    if is_kept_kind {
        if let Ok(entries) = std::fs::read_dir(env.paddock_state().join("keepers")) {
            for e in entries.flatten() {
                if let Some(kf) = real_in_home(&e.path().join("home"), rel, false) {
                    if !candidates.contains(&kf) {
                        candidates.push(kf);
                    }
                }
            }
        }
    }
    for h in harnesses {
        for sb in sandbox_homes(env, h) {
            if let Some(f) = real_in_home(&sb, rel, false) {
                if !candidates.contains(&f) {
                    candidates.push(f);
                }
            }
        }
    }
    let mut loaded: Vec<(PathBuf, Vec<u8>)> = candidates
        .into_iter()
        .filter_map(|p| std::fs::read(&p).ok().map(|b| (p, b)))
        .collect();
    if is_kept_kind && loaded.iter().any(|(_, b)| crate::keeper::keepable(b)) {
        loaded.retain(|(_, b)| crate::keeper::keepable(b));
    }
    loaded
        .into_iter()
        .max_by_key(|(p, b)| (token_expiry(b, expiry_field), mtime(p)))
        .map(|(p, _)| p)
}

/// The panel's record and cache folders, as Omarchy's scripts compute them.
pub fn state_dir(env: &HostEnv) -> PathBuf {
    env.state_dir.join("omarchy").join("agents")
}

pub fn cache_dir(env: &HostEnv) -> PathBuf {
    env.cache_dir.join("omarchy").join("agent-usage")
}

fn omarchy_path(env: &HostEnv) -> PathBuf {
    PathBuf::from(env.var("OMARCHY_PATH").unwrap_or("/usr/share/omarchy"))
}

/// PATH in the box: without mise's wrapper folder and paddock's own hook
/// folder, so nothing in the box comes back to paddock.
pub fn box_path(env: &HostEnv) -> Vec<PathBuf> {
    let skip = [
        crate::omarchy::mise_wrapper_dir(env),
        crate::omarchy_usage::hook_bin(env),
    ];
    crate::omarchy::strip_shims(env, &env.path)
        .into_iter()
        .filter(|d| d.is_absolute() && !skip.contains(d))
        .collect()
}

pub fn view(env: &HostEnv) -> View {
    let home = &env.home;
    let mut v = View::default();
    for (rel, (harnesses, expiry_field)) in login_files(env) {
        if let Some(src) = newest_login(env, &rel, &harnesses, expiry_field.as_deref()) {
            v.logins.insert(home.join(&rel), src);
        }
    }
    let login_sources: BTreeSet<PathBuf> = v.logins.values().cloned().collect();
    v.mounts
        .extend(tool_folders(env).into_iter().map(Mount::Ro));
    for (rel, harnesses) in folders(env) {
        let host = home.join(&rel);
        let host_dir = std::fs::symlink_metadata(&host).is_ok_and(|m| m.is_dir());
        if !host_dir && host.is_file() {
            match v.logins.get(&host) {
                Some(src) if src != &host => v.mounts.push(Mount::Login {
                    src: src.clone(),
                    dest: host,
                }),
                _ => v.mounts.push(Mount::Ro(host)),
            }
            continue;
        }
        let mut theirs: Vec<PathBuf> = harnesses
            .iter()
            .flat_map(|h| sandbox_homes(env, h))
            .filter_map(|h| real_in_home(&h, &rel, true))
            .collect();
        theirs.sort_by_key(|p| {
            let holds_login = login_sources.iter().any(|s| s.starts_with(p));
            (std::cmp::Reverse(holds_login), std::cmp::Reverse(mtime(p)))
        });
        let mut lowers = theirs;
        lowers.reverse();
        if host_dir {
            lowers.push(host.clone());
        }
        if !lowers.is_empty() {
            v.mounts.push(Mount::Overlay { dest: host, lowers });
        }
    }
    // Every login file goes on top of the overlays, read-only, also when the
    // host file is the newest: the overlays are writable.
    for (dest, src) in &v.logins {
        if !v.mounts.iter().any(|m| match m {
            Mount::Login { dest: d, .. } | Mount::Ro(d) => d == dest,
            _ => false,
        }) {
            v.mounts.push(Mount::Login {
                src: src.clone(),
                dest: dest.clone(),
            });
        }
    }
    v.mounts.push(Mount::Rw(state_dir(env)));
    v.mounts.push(Mount::Rw(cache_dir(env)));
    v
}

/// Tools in the home that the collectors run: Omarchy's tree, PATH folders,
/// mise's data and config, and the panel's settings.
fn tool_folders(env: &HostEnv) -> Vec<PathBuf> {
    let home = &env.home;
    let mise_data = env
        .var("MISE_DATA_DIR")
        .filter(|v| v.starts_with('/'))
        .map(PathBuf::from)
        .unwrap_or_else(|| env.data_dir.join("mise"));
    let mise_config = env
        .var("MISE_CONFIG_DIR")
        .filter(|v| v.starts_with('/'))
        .map(PathBuf::from)
        .unwrap_or_else(|| env.config_dir.join("mise"));
    let mut want = vec![
        omarchy_path(env),
        mise_data,
        mise_config,
        env.config_dir.join("omarchy").join("agents"),
    ];
    want.extend(box_path(env));
    let paddock = env.paddock_data();
    let mut out: Vec<PathBuf> = Vec::new();
    for p in want {
        let ours = p.starts_with(&paddock) || paddock.starts_with(&p);
        if !p.starts_with(home) || ours || !p.exists() || out.iter().any(|o| p.starts_with(o)) {
            continue;
        }
        out.retain(|o| !o.starts_with(&p));
        out.push(p);
    }
    out
}

/// The hosts that the usage box proxy lets through: every collector host in
/// `COLLECTORS` and every `[network] endpoints` entry of the profiles.
pub fn allowed_hosts(env: &HostEnv) -> Vec<String> {
    let mut set = BTreeSet::new();
    for c in COLLECTORS {
        set.extend(c.hosts.iter().map(|h| (*h).to_string()));
    }
    if let Ok(f) = crate::profile::HarnessFactory::new(env) {
        for name in f.names() {
            if let Some(p) = f.profile(&name) {
                set.extend(p.network.endpoints.iter().cloned());
            }
        }
    }
    set.into_iter().collect()
}

/// Start the usage box proxy on `sock`. A connection to a host outside
/// `allowed_hosts(env)` is refused and named on standard error.
pub fn start_proxy(env: &HostEnv, sock: &Path) -> Result<()> {
    use crate::proxy::allow::{Allowlist, ApiPolicy, Refusals};
    let (list, _) = Allowlist::parse(&allowed_hosts(env));
    let refusals = Refusals::new(std::sync::Arc::new(|host: &str, why: &str| {
        eprintln!("paddock: the usage box refused a connection to {host} ({why})");
    }));
    let mut cfg = crate::proxy::HostProxy::from_vars(|k| env.var(k).map(str::to_string));
    cfg.api = Some(ApiPolicy {
        list,
        refusals: Some(refusals),
    });
    crate::proxy::serve(sock, cfg)
}

/// The bwrap arguments for the home of the view.
pub fn home_args(env: &HostEnv, view: &View) -> Vec<String> {
    let s = |p: &Path| p.display().to_string();
    let mut a = vec!["--tmpfs".to_string(), s(&env.home)];
    for m in &view.mounts {
        match m {
            Mount::Overlay { dest, lowers } => {
                for l in lowers {
                    a.extend(["--overlay-src".into(), s(l)]);
                }
                a.extend(["--tmp-overlay".into(), s(dest)]);
            }
            Mount::Login { src, dest } => a.extend(["--ro-bind".into(), s(src), s(dest)]),
            Mount::Ro(p) => a.extend(["--ro-bind".into(), s(p), s(p)]),
            Mount::Rw(p) => a.extend(["--bind".into(), s(p), s(p)]),
        }
    }
    a
}

fn dbus_socket_dir(env: &HostEnv) -> Option<PathBuf> {
    let raw = env.var("DBUS_SESSION_BUS_ADDRESS")?;
    for part in raw.split(';') {
        for kv in part.strip_prefix("unix:")?.split(',') {
            if let Some(p) = kv.strip_prefix("path=") {
                let dir = Path::new(p).parent()?;
                if dir.is_dir() && dir != Path::new("/") && !dir.starts_with("/tmp") {
                    return Some(dir.to_path_buf());
                }
            }
        }
    }
    None
}

/// The bwrap arguments for `program ARGS` in the view.
pub fn bwrap_args(env: &HostEnv, view: &View, program: &Path, args: &[String]) -> Vec<String> {
    let s = |p: &Path| p.display().to_string();
    let mut a: Vec<String> = [
        "--die-with-parent",
        "--new-session",
        "--unshare-user",
        "--unshare-pid",
        "--unshare-ipc",
        "--unshare-uts",
        "--unshare-net",
        "--unshare-cgroup-try",
        "--disable-userns",
        "--ro-bind",
        "/",
        "/",
        "--dev",
        "/dev",
        "--proc",
        "/proc",
        "--tmpfs",
        "/tmp",
        "--tmpfs",
        "/var/tmp",
    ]
    .map(String::from)
    .to_vec();
    for d in crate::execbox::session_dirs() {
        a.extend(["--tmpfs".into(), s(&d)]);
    }
    if let Some(rt) = &env.runtime_dir {
        if rt.is_dir() && !rt.starts_with("/tmp") && !a.contains(&s(rt)) {
            a.extend(["--tmpfs".into(), s(rt)]);
        }
    }
    if let Some(bus_dir) = dbus_socket_dir(env) {
        if !a.contains(&s(&bus_dir)) {
            a.extend(["--tmpfs".into(), s(&bus_dir)]);
        }
    }
    a.extend(home_args(env, view));
    if let Some((paddock, sock)) = &view.proxy {
        a.extend([
            "--ro-bind".into(),
            s(paddock),
            "/tmp/.paddock-bin".into(),
            "--ro-bind".into(),
            s(sock),
            "/tmp/.paddock-proxy.sock".into(),
        ]);
    }
    a.extend(["--chdir".into(), s(&env.home), "--".into()]);
    if view.proxy.is_some() {
        let script = format!(
            "/tmp/.paddock-bin relay --listen 127.0.0.1:{} --socket /tmp/.paddock-proxy.sock --ready-file /tmp/.paddock-ready >/dev/null 2>&1 &\n\
             i=0; while [ ! -e /tmp/.paddock-ready ] && [ $i -lt 100 ]; do sleep 0.02; i=$((i+1)); done\n\
             exec \"$@\"",
            crate::proxy::API_PORT
        );
        a.extend(["/bin/sh".into(), "-c".into(), script, "paddock".into()]);
    }
    a.push(s(program));
    a.extend(args.iter().cloned());
    a
}

/// The arguments of bwrap for the box. With `groups`, bwrap starts the
/// stage first, and the stage starts the box (stage.rs).
pub fn box_command(
    env: &HostEnv,
    bwrap: &Path,
    view: &View,
    groups: &[stage::Group],
    program: &Path,
    args: &[String],
) -> Vec<String> {
    let inner = bwrap_args(env, view, program, args);
    if groups.is_empty() {
        return inner;
    }
    let mut a = stage::args(groups);
    a.push(bwrap.display().to_string());
    a.extend(inner);
    a
}

/// The variables in the box. `DBUS_SESSION_BUS_ADDRESS` is never passed.
pub fn box_env(env: &HostEnv) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    out.insert("HOME".into(), env.home.display().to_string());
    out.insert(
        "PATH".into(),
        std::env::join_paths(box_path(env))
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
    );
    for k in ["OMARCHY_PATH", "LANG", "XDG_STATE_HOME", "XDG_CACHE_HOME"] {
        if let Some(v) = env.var(k) {
            out.insert(k.into(), v.into());
        }
    }
    let url = format!("http://127.0.0.1:{}", crate::proxy::API_PORT);
    for k in [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
    ] {
        out.insert(k.into(), url.clone());
    }
    for k in ["NO_PROXY", "no_proxy"] {
        out.insert(k.into(), "127.0.0.1,localhost".into());
    }
    out
}

/// bwrap, when it can start a box with a read-only overlay here.
pub fn probe(env: &HostEnv) -> Result<PathBuf> {
    let bwrap = env.which("bwrap").context("bwrap is not on PATH")?;
    let quiet = |args: &[&str]| {
        Command::new(&bwrap)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    };
    let base = [
        "--unshare-user",
        "--disable-userns",
        "--ro-bind",
        "/",
        "/",
        "--tmpfs",
        "/tmp",
    ];
    if !quiet(&[&base[..], &["true"]].concat()) {
        bail!(
            "{} cannot start a sandbox here (no user namespace)",
            bwrap.display()
        );
    }
    let overlay = [
        "--overlay-src",
        "/usr",
        "--overlay-src",
        "/etc",
        "--tmp-overlay",
        "/tmp/o",
        "true",
    ];
    if !quiet(&[&base[..], &overlay[..]].concat()) {
        bail!(
            "{} cannot mount an overlay here (bwrap 0.10 or newer, and overlayfs in a user namespace)",
            bwrap.display()
        );
    }
    Ok(bwrap)
}

/// The one line for the panel when the box cannot start.
pub fn fail_line(e: &anyhow::Error) -> String {
    format!(
        "paddock: cannot start the box for omarchy-agent-usage-update: {e:#}. It does not run on the host; the panel keeps its last records. Remove the wrapper with: paddock shims remove"
    )
}

fn read_only_write(line: &str) -> bool {
    line.contains("Read-only file system") || line.contains("EROFS")
}

pub const READ_ONLY_NOTE: &str = "paddock: a collector could not write into an agent folder (for example to save a refreshed login). The usage box keeps logins read-only; paddock's credential handling refreshes them.";

/// Run Omarchy's command in the box. Standard output and the exit code
/// pass through, standard error too, with one note when a collector tried
/// to write a read-only file. When the box cannot start, the command never
/// runs: one line on standard error, and None.
pub fn run(env: &HostEnv, program: &Path, args: &[String]) -> Result<Option<i32>> {
    let bwrap = match probe(env) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("{}", fail_line(&e));
            return Ok(None);
        }
    };
    for d in [state_dir(env), cache_dir(env)] {
        std::fs::create_dir_all(&d).with_context(|| format!("cannot create {}", d.display()))?;
    }
    let tried = refresh::run_due(env, crate::state::now(), &refresh::sandbox_runner);
    if !tried.is_empty() {
        log::info!(
            "refreshed the logins of {} for the usage box",
            tried.join(", ")
        );
    }
    let mut view = view(env);
    let run_dir = env
        .paddock_run()
        .join(format!("usage-box-{}", std::process::id()));
    let layer_dir = run_dir.join("l");
    let mut groups = stage::split(&mut view, &layer_dir);
    if !groups.is_empty() {
        let ready = stage::probe(&bwrap, &layer_dir).and_then(|()| stage::prepare(&groups));
        if let Err(e) = ready {
            log::warn!("{e:#}");
            view = self::view(env);
            stage::trim(&mut view);
            groups.clear();
        }
    }
    if view.left_out > 0 {
        eprintln!(
            "paddock: {} sandbox homes are left out of the usage view (too many layers); the panel counts the newest ones",
            view.left_out
        );
    }
    if let Ok(paddock) = crate::launchers::launcher_exe(env) {
        let sock = run_dir.join("api-proxy.sock");
        if start_proxy(env, &sock).is_ok() {
            view.proxy = Some((paddock, sock));
        }
    }
    let mut child = Command::new(&bwrap)
        .args(box_command(env, &bwrap, &view, &groups, program, args))
        .env_clear()
        .envs(box_env(env))
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("cannot run {}", bwrap.display()))?;
    let err = child.stderr.take().context("no standard error")?;
    let relay = std::thread::spawn(move || {
        let mut noted = false;
        let mut r = std::io::BufReader::new(err);
        let mut line = Vec::new();
        let mut out = std::io::stderr();
        while r.read_until(b'\n', &mut line).is_ok_and(|n| n > 0) {
            let _ = out.write_all(&line);
            if !noted && read_only_write(&String::from_utf8_lossy(&line)) {
                noted = true;
                let _ = writeln!(out, "{READ_ONLY_NOTE}");
            }
            line.clear();
        }
    });
    let status = child.wait()?;
    let _ = relay.join();
    let _ = std::fs::remove_dir_all(&run_dir);
    use std::os::unix::process::ExitStatusExt;
    Ok(Some(
        status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(1)),
    ))
}

#[cfg(test)]
mod tests;
