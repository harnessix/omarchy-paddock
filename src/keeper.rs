//! The credential keeper: one sandbox per OAuth login that is the only place
//! where the login's tokens are refreshed.
//!
//! A harness login holds an access token, which expires soon, and a refresh
//! token, which trades for a new pair and is then spent. Before this, every
//! agent sandbox got a copy of the whole login, and two copies could each
//! spend the same refresh token: one of them was then logged out, or the
//! service took the reuse for a stolen token and ended the login.
//!
//! Now, for a harness listed in `KINDS`:
//!
//! - The agent sandbox gets the login without its refresh token. The harness
//!   in it cannot refresh, and an agent cannot steal a token that lasts.
//! - The keeper runs the harness's own refresh code (`codex app-server`,
//!   `account/read` with `refreshToken`) in a sandbox of its own, before the
//!   access token runs out, on the original file. It writes the result back
//!   to where the login lives, and the new access token, again without the
//!   refresh token, into every running sandbox that uses the login.
//! - The keeper sandbox looks like the agent sandbox to the service: the same
//!   machine ID and boot ID (identity.rs), the host name `paddock`, egress
//!   through the host's routes. It runs no agent: only the harness's own
//!   program, with nothing of the host home but the login.
//! - Codex rereads `auth.json` from disk before it refreshes, and takes a
//!   newer file without a request. So the keeper only has to refresh before
//!   Codex's own window of 5 minutes.
//!
//! Among the paddock processes that use one login, the one that holds a lock
//! file is the keeper; when it ends, another takes over.

use crate::env::HostEnv;
use crate::profile::Harness;
use crate::state::AuthMode;
use anyhow::{bail, Context as _, Result};
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// A harness whose OAuth login paddock keeps.
pub struct Kind {
    pub harness: &'static str,
    /// The login file, relative to the home.
    pub file: &'static str,
    /// The folder that the harness takes from `home_var`.
    pub home_dir: &'static str,
    pub home_var: &'static str,
    /// Arguments that start the refresh server.
    pub args: &'static [&'static str],
}

pub const KINDS: &[Kind] = &[Kind {
    harness: "codex",
    file: ".codex/auth.json",
    home_dir: ".codex",
    home_var: "CODEX_HOME",
    args: &["-s", "read-only", "-a", "on-request", "app-server"],
}];

pub fn kind_for(harness: &str) -> Option<&'static Kind> {
    KINDS.iter().find(|k| k.harness == harness)
}

/// `[logins]` of the user configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cfg {
    /// Refresh when the access token has less than this many seconds left.
    pub margin: u64,
    /// How often the keeper looks, in seconds.
    pub interval: u64,
    /// Whether paddock keeps the logins of `KINDS` at all.
    pub enabled: bool,
}

impl Default for Cfg {
    fn default() -> Self {
        Self {
            margin: 30 * 60,
            interval: 60,
            enabled: true,
        }
    }
}

#[derive(serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawCfg {
    keep: Option<bool>,
    refresh_margin: Option<u64>,
    check_interval: Option<u64>,
}

impl Cfg {
    pub fn parse(text: &str) -> Result<Self> {
        let v: toml::Value = toml::from_str(text)?;
        let raw: RawCfg = match v.get("logins") {
            Some(t) => t.clone().try_into()?,
            None => RawCfg::default(),
        };
        let d = Self::default();
        let margin = raw.refresh_margin.unwrap_or(d.margin);
        let interval = raw.check_interval.unwrap_or(d.interval);
        if !(6 * 60..=24 * 3600).contains(&margin) {
            bail!("refresh_margin must be 360 to 86400 seconds: Codex refreshes by itself 5 minutes before the end");
        }
        if !(1..=3600).contains(&interval) {
            bail!("check_interval must be 1 to 3600 seconds");
        }
        Ok(Self {
            margin,
            interval,
            enabled: raw.keep.unwrap_or(true),
        })
    }

    pub fn load(env: &HostEnv) -> Result<Self> {
        let file = env.paddock_config().join("config.toml");
        match std::fs::read_to_string(&file) {
            Ok(t) => Self::parse(&t)
                .with_context(|| format!("[logins] in {} is invalid", file.display())),
            Err(_) => Ok(Self::default()),
        }
    }
}

fn tokens(v: &serde_json::Value) -> Option<&serde_json::Map<String, serde_json::Value>> {
    v.get("tokens")?.as_object()
}

/// A ChatGPT login with a refresh token: the kind that the keeper keeps.
/// An API-key login, or a file that is not JSON, is left alone.
pub fn keepable(bytes: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(bytes)
        .ok()
        .as_ref()
        .and_then(tokens)
        .and_then(|t| t.get("refresh_token"))
        .and_then(|r| r.as_str())
        .is_some_and(|r| !r.is_empty())
}

/// The login without its refresh token. Codex needs the field, so it stays,
/// empty. None when there is nothing to take out.
pub fn strip(bytes: &[u8]) -> Option<Vec<u8>> {
    if !keepable(bytes) {
        return None;
    }
    let mut v: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    v["tokens"]["refresh_token"] = serde_json::Value::String(String::new());
    serde_json::to_vec_pretty(&v).ok()
}

/// When the access token expires: the `exp` of its JWT.
pub fn access_expiry(bytes: &[u8]) -> Option<u64> {
    use base64::Engine as _;
    let v: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let at = tokens(&v)?.get("access_token")?.as_str()?;
    let payload = at.split('.').nth(1)?;
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    serde_json::from_slice::<serde_json::Value>(&raw).ok()?["exp"].as_u64()
}

/// Whether the login needs a refresh now. Without an expiry, Codex refreshes
/// after 8 days; the keeper does after 7.
pub fn due(bytes: &[u8], now: u64, cfg: &Cfg) -> bool {
    if let Some(exp) = access_expiry(bytes) {
        return exp <= now + cfg.margin;
    }
    let last = serde_json::from_slice::<serde_json::Value>(bytes)
        .ok()
        .and_then(|v| v["last_refresh"].as_str().map(str::to_string));
    match last.and_then(|l| parse_rfc3339(&l)) {
        Some(t) => t + 7 * 24 * 3600 <= now,
        None => false,
    }
}

fn parse_rfc3339(s: &str) -> Option<u64> {
    // YYYY-MM-DDTHH:MM:SS, the rest ignored: enough for a 7-day rule.
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, m, d) = (n(0..4)?, n(5..7)?, n(8..10)?);
    let (hh, mm, ss) = (n(11..13)?, n(14..16)?, n(17..19)?);
    // Days from civil (Howard Hinnant).
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = if y2 >= 0 { y2 } else { y2 - 399 } / 400;
    let yoe = y2 - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    u64::try_from(days * 86400 + hh * 3600 + mm * 60 + ss).ok()
}

fn hash(b: &[u8]) -> String {
    crate::manifest::hex(&Sha256::digest(b))
}

/// The key of a login: its file, so that sandboxes of one login share one
/// keeper.
pub fn key(store: &Path) -> String {
    hash(store.as_os_str().as_encoded_bytes())[..16].to_string()
}

/// The keeper's own folder for a login. Only the host writes it, except the
/// login folder inside, which the keeper sandbox writes.
fn keeper_dir(env: &HostEnv, store: &Path) -> PathBuf {
    env.paddock_state().join("keepers").join(key(store))
}

/// Refresh the login in `store` in a keeper sandbox. Returns whether the
/// file changed. A login that changed in the store meanwhile (the harness
/// also runs on the host) wins over the keeper's result.
pub fn refresh(
    env: &HostEnv,
    harness: &Harness,
    kind: &Kind,
    auth: &AuthMode,
    store: &Path,
) -> Result<bool> {
    let before =
        std::fs::read(store).with_context(|| format!("cannot read {}", store.display()))?;
    if !keepable(&before) {
        bail!("{} holds no login with a refresh token", store.display());
    }
    let dir = keeper_dir(env, store);
    let home = dir.join("home");
    let login_dir = home.join(kind.home_dir);
    crate::privfs::dir(&login_dir)?;
    let inside = home.join(kind.file);
    crate::privfs::write(&inside, &before)?;
    let mut child = keeper_command(env, harness, kind, auth, &dir, &home)?
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("cannot start the keeper sandbox")?;
    let talked = talk(&mut child);
    let _ = child.kill();
    let _ = child.wait();
    talked?;
    let after = std::fs::read(&inside)?;
    if after == before {
        return Ok(false);
    }
    if !keepable(&after) {
        bail!("the refresh left no login with a refresh token");
    }
    // Compare and swap: the store must still hold what the keeper started from.
    let now = std::fs::read(store).unwrap_or_default();
    if hash(&now) != hash(&before) {
        log::info!(
            "{} changed while the keeper refreshed it; keeping that one",
            store.display()
        );
        return Ok(true);
    }
    write_store(store, &after)?;
    Ok(true)
}

/// Write the login where it lives, the way the harness does: a new file
/// renamed over the old one, mode 0600.
fn write_store(store: &Path, bytes: &[u8]) -> Result<()> {
    let dir = store.parent().context("the login file has no folder")?;
    let tmp = dir.join(format!(".paddock-keeper-{}", std::process::id()));
    crate::privfs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, store).with_context(|| format!("cannot replace {}", store.display()))?;
    Ok(())
}

/// One request to the refresh server and its answer.
fn ask(
    stdin: &mut std::process::ChildStdin,
    rx: &std::sync::mpsc::Receiver<String>,
    id: u64,
    method: &str,
    params: serde_json::Value,
    wait: Duration,
) -> Result<serde_json::Value> {
    writeln!(
        stdin,
        "{}",
        serde_json::json!({"id": id, "method": method, "params": params})
    )?;
    stdin.flush()?;
    let start = Instant::now();
    loop {
        let left = wait
            .checked_sub(start.elapsed())
            .with_context(|| format!("no answer to {method}"))?;
        let line = rx
            .recv_timeout(left)
            .map_err(|_| anyhow::anyhow!("no answer to {method}"))?;
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if v["id"].as_u64() != Some(id) {
            continue;
        }
        if let Some(e) = v.get("error") {
            bail!("{method} failed: {e}");
        }
        return Ok(v["result"].clone());
    }
}

/// The JSON-RPC exchange with the refresh server: initialize, then
/// `account/read` with `refreshToken`.
fn talk(child: &mut Child) -> Result<()> {
    let mut stdin = child.stdin.take().context("no stdin")?;
    let stdout = child.stdout.take().context("no stdout")?;
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    ask(
        &mut stdin,
        &rx,
        1,
        "initialize",
        serde_json::json!({"clientInfo": {"name": "paddock-keeper", "version": env!("CARGO_PKG_VERSION")}}),
        Duration::from_secs(15),
    )?;
    writeln!(
        stdin,
        "{}",
        serde_json::json!({"method": "initialized", "params": {}})
    )?;
    ask(
        &mut stdin,
        &rx,
        2,
        "account/read",
        serde_json::json!({"refreshToken": true}),
        Duration::from_secs(60),
    )?;
    Ok(())
}

/// The keeper sandbox: bwrap with the system read-only, an empty home but
/// for the login folder, the harness's install read-only, the login's
/// machine identity, the host name `paddock`, and network mode egress.
fn keeper_command(
    env: &HostEnv,
    harness: &Harness,
    kind: &Kind,
    auth: &AuthMode,
    dir: &Path,
    home: &Path,
) -> Result<Command> {
    use crate::backend::bwrap::{resolv_target, DNS_FORWARD, ROOT_LINKS};
    let bwrap = env.which("bwrap").context("bwrap not found")?;
    let s = |p: &Path| p.display().to_string();
    let mut a: Vec<String> = [
        "--die-with-parent",
        "--unshare-all",
        "--unshare-user",
        "--disable-userns",
        "--new-session",
        "--share-net",
        "--hostname",
        "paddock",
    ]
    .map(String::from)
    .to_vec();
    a.extend(["--ro-bind", "/usr", "/usr"].map(String::from));
    for d in ROOT_LINKS {
        let path = Path::new(d);
        match std::fs::read_link(path) {
            Ok(t) => a.extend(["--symlink".into(), s(&t), d.to_string()]),
            Err(_) if path.is_dir() => a.extend(["--ro-bind", d, d].map(String::from)),
            Err(_) => {}
        }
    }
    a.extend(["--ro-bind", "/etc", "/etc"].map(String::from));
    for d in ["/opt", "/nix"] {
        a.extend(["--ro-bind-try", d, d].map(String::from));
    }
    a.extend(
        [
            "--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp", "--tmpfs", "/run", "--tmpfs",
            "/var/tmp",
        ]
        .map(String::from),
    );
    let resolv = dir.join("resolv.conf");
    crate::privfs::write(
        &resolv,
        format!("nameserver {DNS_FORWARD}\noptions edns0\n"),
    )?;
    std::fs::set_permissions(&resolv, std::os::unix::fs::PermissionsExt::from_mode(0o644))?;
    a.extend(["--ro-bind".into(), s(&resolv), s(&resolv_target())]);
    // The same machine as the agent sandboxes of this login.
    let id = crate::identity::for_login(env, &harness.profile.name, auth)?;
    if let Some(m) = &id.machine_id {
        let f = dir.join("machine-id");
        crate::privfs::write(&f, m)?;
        a.extend(["--ro-bind".into(), s(&f), "/etc/machine-id".into()]);
    }
    if let Some(b) = &id.boot_id {
        let f = dir.join("boot_id");
        crate::privfs::write(&f, b)?;
        a.extend([
            "--ro-bind".into(),
            s(&f),
            "/proc/sys/kernel/random/boot_id".into(),
        ]);
    }
    // An empty home with the login folder in it, at the host home's path.
    a.extend(["--tmpfs".into(), s(&env.home)]);
    a.extend([
        "--bind".into(),
        s(&home.join(kind.home_dir)),
        s(&env.home.join(kind.home_dir)),
    ]);
    for m in &harness.install_mounts {
        a.extend(["--ro-bind".into(), s(m), s(m)]);
    }
    a.extend(["--ro-bind".into(), s(&harness.exe), s(&harness.exe)]);
    a.push("--".into());
    a.push(s(&harness.exe));
    a.extend(kind.args.iter().map(|x| x.to_string()));

    let spec = crate::spec::SandboxSpec::new(
        format!("keeper-{}", std::process::id()),
        "keeper",
        &env.home,
        env.home.clone(),
        &env.home,
        env.uid,
        env.gid,
        &env.user,
    );
    let (pasta, mut pa) = crate::backend::bwrap::pasta_prefix(env, &spec)?;
    pa.push(s(&bwrap));
    pa.extend(a);
    let mut cmd = Command::new(pasta);
    cmd.args(pa).env_clear();
    let mut path: Vec<String> = harness.path_dirs.iter().map(|p| s(p)).collect();
    path.extend(["/usr/local/bin", "/usr/bin", "/bin"].map(String::from));
    cmd.env("HOME", &env.home)
        .env("PATH", path.join(":"))
        .env(kind.home_var, env.home.join(kind.home_dir));
    for k in [
        "USER",
        "LANG",
        "LC_ALL",
        "TZ",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
    ] {
        if let Some(v) = env.var(k) {
            cmd.env(k, v);
        }
    }
    // The host's proxy, as the agent sandbox's egress uses it.
    for k in [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
        "NO_PROXY",
        "no_proxy",
    ] {
        if let Some(v) = env.var(k) {
            cmd.env(k, v);
        }
    }
    Ok(cmd)
}

/// Write the login, without its refresh token, into every running sandbox
/// that the keeper keeps it for. A sandbox whose copy is current is skipped.
pub fn push(env: &HostEnv, store: &Path) -> Result<usize> {
    let bytes = std::fs::read(store)?;
    let Some(stripped) = strip(&bytes) else {
        return Ok(0);
    };
    let mut n = 0;
    for r in crate::state::Running::list(env) {
        for l in r.kept_logins.iter().filter(|l| l.store == store) {
            let home = match crate::safefs::Root::open(&l.home) {
                Ok(h) => h,
                Err(_) => continue,
            };
            let now = home.read(&l.rel, crate::safefs::MAX_READ).ok().flatten();
            if now.as_deref() == Some(stripped.as_slice()) {
                continue;
            }
            if let Err(e) = home.write(&l.rel, &stripped, 0o600) {
                log::warn!("cannot update the login of {}: {e:#}", r.id);
                continue;
            }
            n += 1;
        }
    }
    Ok(n)
}

/// One look by the keeper: refresh when due, then bring every sandbox up to
/// date. Errors are reported, never fatal: the next look tries again.
pub fn tick(
    env: &HostEnv,
    harness: &str,
    kind: &Kind,
    auth: &AuthMode,
    store: &Path,
    cfg: &Cfg,
) -> Result<()> {
    let bytes = std::fs::read(store)?;
    if keepable(&bytes) && due(&bytes, crate::state::now(), cfg) {
        let h = crate::profile::HarnessFactory::new(env)?.create(harness)?;
        refresh(env, &h, kind, auth, store)?;
    }
    push(env, store)?;
    Ok(())
}

/// Keep the login in `store` while this process runs: a thread that takes
/// the login's lock when it is free, and then looks every `interval`.
pub fn start(env: &HostEnv, harness: &str, kind: &'static Kind, auth: &AuthMode, store: &Path) {
    let cfg = match Cfg::load(env) {
        Ok(c) if c.enabled => c,
        Ok(_) => return,
        Err(e) => {
            log::warn!("the login keeper is off: {e:#}");
            return;
        }
    };
    let (env, harness, auth, store) = (
        env.clone(),
        harness.to_string(),
        auth.clone(),
        store.to_path_buf(),
    );
    std::thread::spawn(move || {
        let lock_path = env
            .paddock_run()
            .join("keepers")
            .join(format!("{}.lock", key(&store)));
        let mut held: Option<std::fs::File> = None;
        let mut failures = 0u32;
        loop {
            if held.is_none() {
                held = try_lock(&lock_path);
            }
            if held.is_some() {
                match tick(&env, &harness, kind, &auth, &store, &cfg) {
                    Ok(()) => failures = 0,
                    Err(e) => {
                        failures += 1;
                        log::warn!("the login keeper of {} failed: {e:#}", store.display());
                        if failures == 3 {
                            notify_failure(&env, &store, &e);
                        }
                    }
                }
            }
            std::thread::sleep(Duration::from_secs(cfg.interval));
        }
    });
}

fn try_lock(path: &Path) -> Option<std::fs::File> {
    use std::os::fd::AsRawFd;
    if let Some(d) = path.parent() {
        crate::privfs::dir(d).ok()?;
    }
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .ok()?;
    // SAFETY: flock on an open file; the lock goes with the file.
    let r = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    (r == 0).then_some(f)
}

fn notify_failure(env: &HostEnv, store: &Path, e: &anyhow::Error) {
    let _ = crate::broker::notify::show(
        env,
        &crate::broker::notify::Note {
            summary: "paddock cannot refresh a login".into(),
            body: format!(
                "{}: {e:#}. The sandboxes that use it may have to sign in again soon.",
                store.display()
            ),
            actions: Vec::new(),
            critical: true,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A ChatGPT login as Codex writes it, with an access token that
    /// expires at `exp`.
    pub fn login(exp: u64, refresh: &str) -> Vec<u8> {
        use base64::Engine as _;
        let b64 = |v: serde_json::Value| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v.to_string())
        };
        let at = format!(
            "{}.{}.sig",
            b64(serde_json::json!({"alg": "none"})),
            b64(serde_json::json!({"exp": exp}))
        );
        serde_json::to_vec(&serde_json::json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "tokens": {"id_token": "e30.e30.sig", "access_token": at, "refresh_token": refresh, "account_id": "acct-1"},
            "last_refresh": "2026-11-01T00:00:00Z"
        }))
        .unwrap()
    }

    #[test]
    fn a_login_loses_only_its_refresh_token() {
        let l = login(2_000_000_000, "rt-1");
        assert!(keepable(&l));
        let s = strip(&l).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&s).unwrap();
        // Codex needs the field; it is there, empty.
        assert_eq!(v["tokens"]["refresh_token"], "");
        assert_eq!(v["tokens"]["account_id"], "acct-1");
        assert_eq!(v["last_refresh"], "2026-11-01T00:00:00Z");
        assert!(!keepable(&s));
        assert!(strip(&s).is_none(), "nothing to take out twice");
        // An API-key login and other files are left alone.
        assert!(strip(br#"{"auth_mode":"apikey","OPENAI_API_KEY":"sk-x"}"#).is_none());
        assert!(strip(b"not json").is_none());
    }

    #[test]
    fn a_refresh_is_due_before_codex_would_refresh_itself() {
        let cfg = Cfg::default();
        let now = 1_900_000_000;
        assert_eq!(access_expiry(&login(now + 3600, "rt")), Some(now + 3600));
        assert!(!due(&login(now + 3600, "rt"), now, &cfg));
        assert!(due(&login(now + cfg.margin, "rt"), now, &cfg));
        assert!(due(&login(now - 10, "rt"), now, &cfg));
        // No expiry: the age of the last refresh decides.
        let no_exp = br#"{"tokens":{"access_token":"opaque","refresh_token":"rt"},"last_refresh":"2026-11-01T00:00:00Z"}"#;
        let t = parse_rfc3339("2026-11-01T00:00:00Z").unwrap();
        assert!(!due(no_exp, t + 6 * 86400, &cfg));
        assert!(due(no_exp, t + 7 * 86400, &cfg));
    }

    #[test]
    fn dates_and_settings() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339("2000-03-01T12:30:15.123Z"), Some(951_913_815));
        assert!(parse_rfc3339("yesterday").is_none());
        assert_eq!(Cfg::parse("").unwrap(), Cfg::default());
        let c = Cfg::parse("[logins]\nrefresh_margin = 3600\ncheck_interval = 5\n").unwrap();
        assert_eq!((c.margin, c.interval, c.enabled), (3600, 5, true));
        assert!(!Cfg::parse("[logins]\nkeep = false\n").unwrap().enabled);
        // Not later than Codex's own window of 5 minutes.
        assert!(Cfg::parse("[logins]\nrefresh_margin = 300\n").is_err());
        assert!(Cfg::parse("[logins]\nother = 1\n").is_err());
    }
}
