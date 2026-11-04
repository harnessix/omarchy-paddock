//! `paddock gatewayd`: the executor of approved host requests.
//!
//! paddock, in the process that runs a sandbox, decides and asks the user.
//! It does not run the approved work itself: it signs a grant for the exact
//! request (`gateway::sign`) and sends it here. This process checks the
//! signature, the time, that the grant is new, and that it covers exactly
//! this request, and only then runs it.
//!
//! - It listens on `$XDG_RUNTIME_DIR/paddock/gatewayd.sock`. With the systemd
//!   user units of `paddock gateway install`, systemd owns the socket and
//!   starts this process on the first connection (socket activation).
//!   Without them, paddock starts it when a request needs it.
//! - It exits after a while without connections. The next request starts it
//!   again.
//! - The first capability that it runs is `exec`. The others follow.

use crate::broker::Request;
use crate::env::HostEnv;
use crate::gateway::{self, Job, Seen};
use anyhow::{bail, Context as _, Result};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The largest job: a request of the gateway and the environment.
const MAX_JOB: u64 = 1 << 20;
/// The longest push.
const PUSH_TIMEOUT: u64 = 600;
/// Exit after this long without a connection.
pub const IDLE: Duration = Duration::from_secs(120);

pub fn socket_path(env: &HostEnv) -> PathBuf {
    crate::sockpath::named_or_run(env, "", "gatewayd.sock")
}

/// Run one job that `verify` accepted. The result is the JSON that the
/// broker returns to the sandbox. `how` is the isolation of a command.
fn run(
    env: &HostEnv,
    how: Result<crate::execbox::Isolation, String>,
    job: &Job,
) -> Result<serde_json::Value> {
    match job.request.cap.as_str() {
        "exec" => {
            let argv: Vec<String> = job
                .request
                .args
                .get("argv")
                .and_then(|a| serde_json::from_value(a.clone()).ok())
                .filter(|a: &Vec<String>| !a.is_empty())
                .context("exec needs argv")?;
            let timeout = job
                .request
                .args
                .get("timeout")
                .and_then(|t| t.as_u64())
                .unwrap_or(120)
                .clamp(1, 3600);
            let how = how.map_err(|e| anyhow::anyhow!("{e}"))?;
            let cfg = crate::execbox::ExecCfg::load(env)?;
            let unit = format!(
                "paddock-exec-{}",
                &job.grant.nonce[..12.min(job.grant.nonce.len())]
            );
            let mut cmd = crate::execbox::command(
                how,
                &cfg,
                &env.home,
                &job.grant.project,
                &argv,
                &cfg.pass_env(&job.env),
                timeout,
                &unit,
            );
            let (code, out, err) =
                crate::broker::run_capture(&mut cmd, Duration::from_secs(timeout + 10))?;
            if let Some(why) = crate::execbox::setup_failure(how, code, &err) {
                bail!("{why}");
            }
            Ok(serde_json::json!({ "exit_code": code, "stdout": out, "stderr": err }))
        }
        "tool" => {
            // The executor reads the tools itself and checks the request
            // again: a signed request still runs only what the user wrote.
            let name = job
                .request
                .args
                .get("name")
                .and_then(|n| n.as_str())
                .context("tool needs a name")?;
            let args = crate::broker::tool_args(&job.request)?;
            let tools = crate::hosttools::load(env)?;
            let tool = tools
                .get(name)
                .with_context(|| format!("no tool {name} in the user configuration"))?;
            if tool.mode == crate::broker::Mode::Deny {
                bail!("tool {name} is set to deny");
            }
            tool.check(&args)?;
            let argv = crate::hosttools::argv(env, tool, &args)?;
            let timeout = job
                .request
                .args
                .get("timeout")
                .and_then(|t| t.as_u64())
                .unwrap_or(120)
                .clamp(1, 3600);
            let how = how.map_err(|e| anyhow::anyhow!("{e}"))?;
            let mut cfg = crate::execbox::ExecCfg::load(env)?;
            let cwd = match tool.cwd {
                crate::hosttools::Cwd::Project => job.grant.project.clone(),
                crate::hosttools::Cwd::Home => {
                    // An empty home has no such folder: show it read-only.
                    if cfg.home == crate::execbox::Home::Hidden {
                        cfg.home = crate::execbox::Home::ReadOnly;
                    }
                    env.home.clone()
                }
            };
            let unit = unit_name("tool", job);
            let mut cmd = crate::execbox::command_in(
                how,
                &cfg,
                &env.home,
                &job.grant.project,
                &cwd,
                &argv,
                &cfg.pass_env(&job.env),
                timeout,
                &unit,
            );
            let (code, out, err) =
                crate::broker::run_capture(&mut cmd, Duration::from_secs(timeout + 10))?;
            if let Some(why) = crate::execbox::setup_failure(how, code, &err) {
                bail!("{why}");
            }
            Ok(serde_json::json!({ "exit_code": code, "stdout": out, "stderr": err }))
        }
        "git-push" => {
            let x: crate::broker::git::PushExec =
                serde_json::from_value(job.plan.clone()).context("a push needs its plan")?;
            let how = how.map_err(|e| anyhow::anyhow!("{e}"))?;
            // The user's credentials stay readable (git and ssh configuration,
            // keys, agent sockets) but nothing in the home folder can change;
            // only the scratch repository is writable.
            // A remote on this machine (a path or file:// URL) is the one
            // folder outside the scratch repository that the push writes.
            let local = x
                .url
                .strip_prefix("file://")
                .unwrap_or(&x.url)
                .starts_with('/')
                .then(|| PathBuf::from(x.url.strip_prefix("file://").unwrap_or(&x.url)))
                .filter(|p| p.exists());
            let cfg = crate::execbox::ExecCfg {
                home: crate::execbox::Home::ReadOnly,
                network: true,
                paths: local.into_iter().collect(),
                ro_paths: Vec::new(),
                env: crate::broker::git::PUSH_ENV
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
                remember_allowed: false,
                // Credential helpers and gh reach the keyring over the
                // session bus.
                hide: Vec::new(),
            };
            let unit = format!(
                "paddock-push-{}",
                &job.grant.nonce[..12.min(job.grant.nonce.len())]
            );
            crate::broker::git::push_exec(&x, |argv, extra| {
                let mut vars = cfg.pass_env(&job.env);
                vars.extend(extra.clone());
                let mut cmd = crate::execbox::command(
                    how,
                    &cfg,
                    &env.home,
                    &x.scratch,
                    argv,
                    &vars,
                    PUSH_TIMEOUT,
                    &unit,
                );
                let (code, out, err) =
                    crate::broker::run_capture(&mut cmd, Duration::from_secs(PUSH_TIMEOUT + 10))?;
                if let Some(why) = crate::execbox::setup_failure(how, code, &err) {
                    bail!("{why}");
                }
                Ok((code, out, err))
            })
        }
        "forge" => {
            let x: crate::broker::forge::ForgeExec =
                serde_json::from_value(job.plan.clone()).context("a forge call needs its plan")?;
            let name = Path::new(&x.tool)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");
            if !Path::new(&x.tool).is_absolute()
                || !crate::broker::forge::EXEC_TOOLS.contains(&name)
            {
                bail!("the executor does not run {}", x.tool);
            }
            let how = how.map_err(|e| anyhow::anyhow!("{e}"))?;
            // Like a push: the login stays readable, nothing in the home
            // folder can change, and it runs in an empty folder of paddock.
            let cfg = crate::execbox::ExecCfg {
                home: crate::execbox::Home::ReadOnly,
                network: true,
                paths: Vec::new(),
                ro_paths: Vec::new(),
                env: crate::broker::forge::forge_env(name),
                remember_allowed: false,
                // Credential helpers and gh reach the keyring over the
                // session bus.
                hide: Vec::new(),
            };
            let mut vars = cfg.pass_env(&job.env);
            vars.extend(crate::broker::forge::forge_vars());
            let mut argv = vec![x.tool.clone()];
            argv.extend(x.argv.iter().cloned());
            let unit = unit_name("forge", job);
            let mut cmd = crate::execbox::command(
                how, &cfg, &env.home, &x.workdir, &argv, &vars, x.timeout, &unit,
            );
            let (code, out, err) = crate::broker::run_capture_input(
                &mut cmd,
                Duration::from_secs(x.timeout + 10),
                x.stdin.as_deref().map(str::as_bytes),
            )?;
            if let Some(why) = crate::execbox::setup_failure(how, code, &err) {
                bail!("{why}");
            }
            Ok(serde_json::json!({ "exit_code": code, "stdout": out, "stderr": err }))
        }
        "screenshot" | "clipboard-read" | "clipboard-write" => {
            let x: DesktopExec = serde_json::from_value(job.plan.clone())
                .context("a desktop request needs its plan")?;
            let how = how.map_err(|e| anyhow::anyhow!("{e}"))?;
            desktop(env, how, job, &x)
        }
        "open-url" => {
            let url = job
                .request
                .args
                .get("url")
                .and_then(|u| u.as_str())
                .context("open-url needs a url")?;
            open_url(env, url, &job.env)?;
            Ok(serde_json::json!({}))
        }
        "notify" => {
            let n: crate::broker::notify::Note = serde_json::from_value(job.plan.clone())
                .context("a notification needs its text")?;
            crate::broker::notify::show(env, &n).context("cannot show the notification")?;
            Ok(serde_json::json!({}))
        }
        other => bail!("the executor does not run {other} yet"),
    }
}

fn unit_name(kind: &str, job: &Job) -> String {
    format!(
        "paddock-{kind}-{}",
        &job.grant.nonce[..12.min(job.grant.nonce.len())]
    )
}

/// What a desktop request needs from the host: the folder where a
/// screenshot goes (an empty folder of paddock, which the broker reads
/// afterwards) and the request's own arguments.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DesktopExec {
    pub workdir: PathBuf,
}

#[cfg(target_os = "linux")]
/// Host variables that the desktop tools need: the Wayland display and the
/// compositor.
const DESKTOP_ENV: &[&str] = &[
    "WAYLAND_DISPLAY",
    "XDG_RUNTIME_DIR",
    "HYPRLAND_INSTANCE_SIGNATURE",
];

#[cfg(target_os = "linux")]
/// The isolation of a desktop tool: no home, no network, and of the runtime
/// folder only the Wayland socket and the compositor's sockets.
fn desktop_cfg(job: &Job, workdir: &Path) -> crate::execbox::ExecCfg {
    let mut ro = Vec::new();
    if let (Some(rt), Some(wl)) = (
        job.env.get("XDG_RUNTIME_DIR"),
        job.env.get("WAYLAND_DISPLAY"),
    ) {
        ro.push(Path::new(rt).join(wl));
        if let Some(sig) = job.env.get("HYPRLAND_INSTANCE_SIGNATURE") {
            ro.push(Path::new(rt).join("hypr").join(sig));
        }
    }
    crate::execbox::ExecCfg {
        home: crate::execbox::Home::Hidden,
        network: false,
        paths: vec![workdir.to_path_buf()],
        ro_paths: ro.into_iter().filter(|p| p.exists()).collect(),
        env: DESKTOP_ENV.iter().map(|s| s.to_string()).collect(),
        remember_allowed: false,
        // Only the Wayland and compositor sockets, bound above.
        hide: crate::execbox::session_dirs(),
    }
}

#[cfg(target_os = "linux")]
fn desktop(
    env: &HostEnv,
    how: crate::execbox::Isolation,
    job: &Job,
    x: &DesktopExec,
) -> Result<serde_json::Value> {
    let cfg = desktop_cfg(job, &x.workdir);
    let vars = cfg.pass_env(&job.env);
    let arg = |k: &str| {
        job.request
            .args
            .get(k)
            .and_then(|v| v.as_str())
            .unwrap_or("")
    };
    let run = |argv: Vec<String>, unit: &str| -> Result<(i32, String, String)> {
        let mut cmd =
            crate::execbox::command(how, &cfg, &env.home, &x.workdir, &argv, &vars, 30, unit);
        let r = crate::broker::run_capture(&mut cmd, Duration::from_secs(40))?;
        if let Some(why) = crate::execbox::setup_failure(how, r.0, &r.2) {
            bail!("{why}");
        }
        Ok(r)
    };
    match job.request.cap.as_str() {
        "screenshot" => {
            let mut argv = vec!["grim".to_string()];
            match arg("target") {
                "" | "screen" => {}
                "window" => {
                    let (_, out, _) = run(
                        vec!["hyprctl".into(), "activewindow".into(), "-j".into()],
                        &unit_name("hyprctl", job),
                    )?;
                    argv.extend(["-g".to_string(), window_geometry(&out)?]);
                }
                t => argv.extend([
                    "-o".to_string(),
                    t.trim_start_matches("output:").to_string(),
                ]),
            }
            argv.push(x.workdir.join("shot.png").display().to_string());
            let (code, _, err) = run(argv, &unit_name("screenshot", job))?;
            if code != 0 {
                bail!("grim failed: {}", err.trim());
            }
            Ok(serde_json::json!({ "file": "shot.png" }))
        }
        "clipboard-read" => {
            let (_, out, _) = run(
                vec!["wl-paste".into(), "--no-newline".into()],
                &unit_name("paste", job),
            )?;
            Ok(serde_json::json!({ "text": out }))
        }
        "clipboard-write" => {
            copy(env, how, &cfg, &vars, arg("text"), &unit_name("copy", job))?;
            Ok(serde_json::json!({}))
        }
        _ => unreachable!(),
    }
}

/// `x,y wxh` of the active window from `hyprctl activewindow -j`.
#[cfg(target_os = "linux")]
fn window_geometry(json: &str) -> Result<String> {
    let v: serde_json::Value = serde_json::from_str(json).context("no active window")?;
    let at = v["at"].as_array().context("no active window")?;
    let size = v["size"].as_array().context("no active window")?;
    Ok(format!(
        "{},{} {}x{}",
        at[0].as_i64().unwrap_or(0),
        at[1].as_i64().unwrap_or(0),
        size[0].as_i64().unwrap_or(0),
        size[1].as_i64().unwrap_or(0)
    ))
}

/// Base64 (RFC 4648), for `StandardInputData=`.
pub fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= c.len() {
                out.push(T[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(target_os = "linux")]
/// Put text on the clipboard. `wl-copy --foreground` keeps it there until
/// something else is copied, so it runs in its own unit (or process) that
/// outlives this request.
fn copy(
    env: &HostEnv,
    how: crate::execbox::Isolation,
    cfg: &crate::execbox::ExecCfg,
    vars: &std::collections::BTreeMap<String, String>,
    text: &str,
    unit: &str,
) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let argv = vec!["wl-copy".to_string(), "--foreground".to_string()];
    // Its working folder: empty, paddock's own, and still there after this
    // request (the request's folder is removed when it ends).
    let cwd = env.paddock_run().join("clipboard");
    crate::privfs::dir(&cwd)?;
    // Not the request's folder, which is removed when the request ends.
    let cfg = &crate::execbox::ExecCfg {
        paths: Vec::new(),
        ..cfg.clone()
    };
    match how {
        crate::execbox::Isolation::Systemd => {
            let cmd = crate::execbox::command(how, cfg, &env.home, &cwd, &argv, vars, 86_400, unit);
            // No --pipe and no --wait: the unit stays after this request.
            let args: Vec<String> = cmd
                .get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .filter(|a| a != "--pipe" && a != "--wait")
                .collect();
            let mut c = std::process::Command::new("systemd-run");
            let i = args.iter().position(|a| a == "--").unwrap_or(args.len());
            c.args(&args[..i])
                .arg("-p")
                .arg(format!("StandardInputData={}", base64(text.as_bytes())))
                .args(&args[i..]);
            // --setenv names the variables; systemd-run takes the values
            // from its own environment.
            c.env_clear().envs(
                cmd.get_envs()
                    .filter_map(|(k, v)| v.map(|v| (k.to_os_string(), v.to_os_string()))),
            );
            for k in ["XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS"] {
                if let Ok(v) = std::env::var(k) {
                    c.env(k, v);
                }
            }
            let out = c.stdin(std::process::Stdio::null()).output()?;
            if !out.status.success() {
                bail!(
                    "cannot start wl-copy: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
        }
        _ => {
            let c = crate::execbox::command(how, cfg, &env.home, &cwd, &argv, vars, 86_400, unit);
            let args: Vec<String> = c
                .get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .filter(|a| a != "--die-with-parent")
                .collect();
            let prog = c.get_program().to_os_string();
            let envs: Vec<(std::ffi::OsString, std::ffi::OsString)> = c
                .get_envs()
                .filter_map(|(k, v)| v.map(|v| (k.to_os_string(), v.to_os_string())))
                .collect();
            let mut c = std::process::Command::new(prog);
            c.args(args).env_clear().envs(envs);
            // Its own session: it stays after the executor exits.
            unsafe {
                c.pre_exec(|| {
                    libc::setsid();
                    Ok(())
                });
            }
            let mut child = c
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .context("cannot start wl-copy")?;
            std::io::Write::write_all(&mut child.stdin.take().unwrap(), text.as_bytes())?;
        }
    }
    Ok(())
}

/// Open a link in the user's browser: through the desktop portal (the
/// desktop starts the browser, outside paddock), else with xdg-open and
/// only the variables that a browser needs.
#[cfg(target_os = "linux")]
fn open_url(
    env: &HostEnv,
    url: &str,
    host: &std::collections::BTreeMap<String, String>,
) -> Result<()> {
    if portal_open_uri(env, url).is_ok() {
        return Ok(());
    }
    let opener = env
        .which("xdg-open")
        .context("opening URLs needs xdg-open on the host")?;
    let mut c = std::process::Command::new(opener);
    c.arg(url).env_clear();
    for k in [
        "HOME",
        "PATH",
        "LANG",
        "LC_ALL",
        "WAYLAND_DISPLAY",
        "DISPLAY",
        "XDG_RUNTIME_DIR",
        "DBUS_SESSION_BUS_ADDRESS",
        "XDG_CURRENT_DESKTOP",
        "XDG_SESSION_TYPE",
        "BROWSER",
    ] {
        if let Some(v) = host.get(k) {
            c.env(k, v);
        }
    }
    c.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("cannot start xdg-open")?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn portal_open_uri(env: &HostEnv, url: &str) -> Result<()> {
    let conn = crate::broker::notify::session(env)?;
    let p = zbus::blocking::Proxy::new(
        &conn,
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.OpenURI",
    )?;
    let opts: std::collections::HashMap<&str, zbus::zvariant::Value<'_>> = Default::default();
    let _: zbus::zvariant::OwnedObjectPath = p.call("OpenURI", &("", url, opts))?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn desktop(
    env: &HostEnv,
    how: crate::execbox::Isolation,
    job: &Job,
    x: &DesktopExec,
) -> Result<serde_json::Value> {
    let _ = (env, how, job, x);
    Err(crate::platform::unavailable("this desktop action"))
}

/// Open a link in the user's browser with `open`, which asks Launch
/// Services. The broker allows http and https links only; this checks it
/// again, so no other scheme or option reaches `open`.
#[cfg(target_os = "macos")]
fn open_url(
    env: &HostEnv,
    url: &str,
    host: &std::collections::BTreeMap<String, String>,
) -> Result<()> {
    let _ = (env, host);
    if !web_link(url) {
        bail!("only http and https links can be opened");
    }
    std::process::Command::new("/usr/bin/open")
        .arg(url)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("cannot start /usr/bin/open")?;
    Ok(())
}

/// An http or https link.
pub fn web_link(url: &str) -> bool {
    url::Url::parse(url).is_ok_and(|u| matches!(u.scheme(), "http" | "https"))
        && (url.starts_with("http://") || url.starts_with("https://"))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn open_url(
    env: &HostEnv,
    url: &str,
    host: &std::collections::BTreeMap<String, String>,
) -> Result<()> {
    let _ = (env, url, host);
    Err(crate::platform::unavailable("this desktop action"))
}

/// Check a job and run it. Every refusal is an error with the reason.
pub fn handle(
    env: &HostEnv,
    how: Result<crate::execbox::Isolation, String>,
    key: &[u8],
    seen: &Seen,
    job: &Job,
) -> Result<serde_json::Value> {
    gateway::verify(key, job, seen, crate::state::now())
        .map_err(|r| anyhow::anyhow!("the executor refused the request: {r}"))?;
    run(env, how, job)
}

/// The answer on the socket: `{"ok": RESULT}` or `{"error": "TEXT"}`.
fn answer(
    env: &HostEnv,
    how: Result<crate::execbox::Isolation, String>,
    key: &[u8],
    seen: &Seen,
    line: &str,
) -> serde_json::Value {
    let job: Job = match serde_json::from_str(line) {
        Ok(j) => j,
        Err(e) => return serde_json::json!({ "error": format!("bad job: {e}") }),
    };
    match handle(env, how, key, seen, &job) {
        Ok(v) => serde_json::json!({ "ok": v }),
        Err(e) => serde_json::json!({ "error": format!("{e:#}") }),
    }
}

fn serve_conn(
    env: &HostEnv,
    how: Result<crate::execbox::Isolation, String>,
    key: &[u8],
    seen: &Seen,
    conn: UnixStream,
) -> Result<()> {
    let mut reader = BufReader::new(conn.try_clone()?.take(MAX_JOB));
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let reply = answer(env, how, key, seen, line.trim());
    let mut w = conn;
    writeln!(w, "{reply}")?;
    Ok(())
}

/// The listener: the socket that systemd passes (`LISTEN_FDS`), or our own.
fn listener(env: &HostEnv) -> Result<Option<UnixListener>> {
    let pid_ok = std::env::var("LISTEN_PID")
        .ok()
        .and_then(|p| p.parse::<u32>().ok())
        == Some(std::process::id());
    if pid_ok && std::env::var("LISTEN_FDS").ok().as_deref() == Some("1") {
        use std::os::unix::io::FromRawFd;
        // SAFETY: systemd passes the listening socket as fd 3.
        return Ok(Some(unsafe { UnixListener::from_raw_fd(3) }));
    }
    let path = socket_path(env);
    if let Some(d) = path.parent() {
        crate::privfs::dir(d)?;
    }
    match UnixListener::bind(&path) {
        Ok(l) => Ok(Some(l)),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            // Another executor answers already: nothing to do.
            if UnixStream::connect(&path).is_ok() {
                return Ok(None);
            }
            // A socket of an executor that ended.
            std::fs::remove_file(&path)?;
            Ok(Some(UnixListener::bind(&path)?))
        }
        Err(e) => Err(e).with_context(|| format!("cannot listen on {}", path.display())),
    }
}

/// `paddock gatewayd`: serve until `idle` passes without a connection.
pub fn main(env: &HostEnv, idle: Duration) -> Result<i32> {
    crate::update::refuse_in_sandbox(env, "paddock gatewayd")?;
    let key: Arc<Vec<u8>> = Arc::new(gateway::key(env).context("cannot read the gateway key")?);
    // How exec is isolated is decided at its first use: an executor on a
    // host without systemd or bwrap still starts, and refuses only exec.
    let how = Arc::new(std::sync::OnceLock::new());
    let shared = Arc::new(env.clone());
    let Some(l) = listener(env)? else {
        return Ok(0);
    };
    l.set_nonblocking(true)?;
    let seen = Arc::new(Seen::default());
    let start = Instant::now();
    let last = Arc::new(AtomicU64::new(0));
    let running = Arc::new(AtomicU64::new(0));
    loop {
        match l.accept() {
            Ok((conn, _)) => {
                conn.set_nonblocking(false)?;
                last.store(start.elapsed().as_secs(), Ordering::SeqCst);
                running.fetch_add(1, Ordering::SeqCst);
                let how = how.clone();
                let (env, key, seen, last, running) = (
                    shared.clone(),
                    key.clone(),
                    seen.clone(),
                    last.clone(),
                    running.clone(),
                );
                std::thread::spawn(move || {
                    let iso = how
                        .get_or_init(|| crate::execbox::detect(&env).map_err(|e| format!("{e:#}")));
                    if let Err(e) = serve_conn(&env, iso.clone(), &key, &seen, conn) {
                        log::debug!("gateway connection failed: {e:#}");
                    }
                    last.store(start.elapsed().as_secs(), Ordering::SeqCst);
                    running.fetch_sub(1, Ordering::SeqCst);
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                let quiet = quiet(start.elapsed().as_secs(), last.load(Ordering::SeqCst));
                if running.load(Ordering::SeqCst) == 0 && quiet >= idle.as_secs() {
                    return Ok(0);
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// Seconds without a request. A request can end between reading the clock
/// and reading `last`, so `last` can be ahead of `now`: that is no quiet
/// time, not an underflow.
fn quiet(now: u64, last: u64) -> u64 {
    now.saturating_sub(last)
}

/// Where the broker sends signed jobs.
pub trait Dispatch: Send + Sync {
    fn dispatch(&self, job: &Job, wait: Duration) -> Result<serde_json::Value>;
    /// The key that signs jobs for this executor.
    fn signing_key(&self, env: &HostEnv) -> Result<Vec<u8>> {
        gateway::key(env).context("cannot read the gateway key")
    }
}

/// The executor process, started when it is not there.
pub struct Remote {
    pub socket: PathBuf,
    pub program: PathBuf,
    pub env: HostEnv,
}

impl Remote {
    pub fn new(env: &HostEnv) -> Result<Self> {
        Ok(Self {
            socket: socket_path(env),
            program: std::env::current_exe()
                .and_then(std::fs::canonicalize)
                .context("cannot find the paddock executable")?,
            env: env.clone(),
        })
    }

    fn connect(&self) -> Result<UnixStream> {
        if let Ok(s) = UnixStream::connect(&self.socket) {
            return Ok(s);
        }
        start_executor(&self.program, &self.env)?;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match UnixStream::connect(&self.socket) {
                Ok(s) => return Ok(s),
                Err(e) if Instant::now() >= deadline => {
                    return Err(e).with_context(|| {
                        format!("cannot reach the executor at {}", self.socket.display())
                    })
                }
                Err(_) => std::thread::sleep(Duration::from_millis(50)),
            }
        }
    }
}

/// Start `paddock gatewayd` in its own session, detached.
fn start_executor(program: &Path, env: &HostEnv) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let mut cmd = std::process::Command::new(program);
    cmd.arg("gatewayd")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if let Some(rt) = &env.runtime_dir {
        cmd.env("XDG_RUNTIME_DIR", rt);
    }
    // SAFETY: setsid is async-signal-safe. The executor outlives this call.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().context("cannot start paddock gatewayd")?;
    Ok(())
}

impl Dispatch for Remote {
    fn dispatch(&self, job: &Job, wait: Duration) -> Result<serde_json::Value> {
        let mut s = self.connect()?;
        s.set_read_timeout(Some(wait))?;
        writeln!(s, "{}", serde_json::to_string(job)?)?;
        let mut line = String::new();
        BufReader::new(s.take(64 << 20)).read_line(&mut line)?;
        let v: serde_json::Value =
            serde_json::from_str(line.trim()).context("bad answer from the executor")?;
        match (v.get("ok"), v.get("error")) {
            (Some(ok), _) => Ok(ok.clone()),
            (_, Some(e)) => bail!("{}", e.as_str().unwrap_or("the executor failed")),
            _ => bail!("bad answer from the executor"),
        }
    }
}

/// The executor in this process, for tests: the same checks, no process.
pub struct InProcess {
    pub key: Vec<u8>,
    pub seen: Seen,
    /// The executor's view of the host (its user configuration).
    pub env: HostEnv,
    pub isolation: crate::execbox::Isolation,
}

impl Dispatch for InProcess {
    fn dispatch(&self, job: &Job, _wait: Duration) -> Result<serde_json::Value> {
        handle(&self.env, Ok(self.isolation), &self.key, &self.seen, job)
    }
    fn signing_key(&self, _env: &HostEnv) -> Result<Vec<u8>> {
        Ok(self.key.clone())
    }
}

/// The host environment for an executed command: the variables of this
/// paddock process, which the sandbox never sets.
pub fn host_env() -> std::collections::BTreeMap<String, String> {
    std::env::vars().collect()
}

/// For the broker: sign `req` and run it in the executor.
pub fn execute(
    dispatch: &dyn Dispatch,
    key: &[u8],
    sandbox: &str,
    project: &Path,
    req: &Request,
    plan: serde_json::Value,
) -> Result<serde_json::Value> {
    let job = gateway::sign(key, sandbox, project, req, host_env(), plan)?;
    let timeout = match req.cap.as_str() {
        "git-push" => PUSH_TIMEOUT,
        _ => req
            .args
            .get("timeout")
            .and_then(|t| t.as_u64())
            .unwrap_or(120)
            .clamp(1, 3600),
    };
    dispatch.dispatch(&job, Duration::from_secs(timeout + 30))
}

// ---------------------------------------------------------------------------
// systemd socket activation (`paddock gateway install|remove|status`).

const UNIT_MARK: &str = "# Written by paddock (paddock gateway remove deletes it).";
pub const SOCKET_UNIT: &str = "paddock-gatewayd.socket";
pub const SERVICE_UNIT: &str = "paddock-gatewayd.service";

fn unit_dir(env: &HostEnv) -> PathBuf {
    env.config_dir.join("systemd").join("user")
}

pub fn socket_unit() -> String {
    format!(
        "{UNIT_MARK}\n[Unit]\nDescription=paddock gateway executor socket\n\n\
         [Socket]\nListenStream=%t/paddock/gatewayd.sock\nSocketMode=0600\nDirectoryMode=0700\n\n\
         [Install]\nWantedBy=sockets.target\n"
    )
}

pub fn service_unit(paddock: &Path) -> String {
    format!(
        "{UNIT_MARK}\n[Unit]\nDescription=paddock gateway executor\nRequires={SOCKET_UNIT}\n\n\
         [Service]\nExecStart={} gatewayd\n",
        paddock.display()
    )
}

fn systemctl(args: &[&str]) -> Result<()> {
    let st = std::process::Command::new("systemctl")
        .arg("--user")
        .args(args)
        .stdin(std::process::Stdio::null())
        .status()
        .context("cannot run systemctl --user")?;
    if !st.success() {
        bail!("systemctl --user {} failed with {st}", args.join(" "));
    }
    Ok(())
}

fn ours(p: &Path) -> bool {
    std::fs::read_to_string(p)
        .map(|t| t.starts_with(UNIT_MARK))
        .unwrap_or(false)
}

/// Write the units and turn the socket on.
pub fn install(env: &HostEnv) -> Result<Vec<PathBuf>> {
    crate::update::refuse_in_sandbox(env, "paddock gateway install")?;
    let paddock = crate::launchers::launcher_exe(env)?;
    let dir = unit_dir(env);
    std::fs::create_dir_all(&dir)?;
    let files = [
        (dir.join(SOCKET_UNIT), socket_unit()),
        (dir.join(SERVICE_UNIT), service_unit(&paddock)),
    ];
    for (p, _) in &files {
        if p.exists() && !ours(p) {
            bail!(
                "{} exists and paddock did not write it; nothing was changed",
                p.display()
            );
        }
    }
    for (p, text) in &files {
        std::fs::write(p, text)?;
    }
    systemctl(&["daemon-reload"])?;
    // An executor that paddock started itself holds the socket path until it
    // is idle; systemd takes the path over after that.
    let _ = std::fs::remove_file(socket_path(env));
    systemctl(&["enable", "--now", SOCKET_UNIT])?;
    Ok(files.into_iter().map(|(p, _)| p).collect())
}

/// Turn the socket off and remove the units that paddock wrote.
pub fn remove(env: &HostEnv) -> Result<Vec<PathBuf>> {
    let dir = unit_dir(env);
    let mut removed = Vec::new();
    if ours(&dir.join(SOCKET_UNIT)) {
        let _ = systemctl(&["disable", "--now", SOCKET_UNIT]);
    }
    for name in [SOCKET_UNIT, SERVICE_UNIT] {
        let p = dir.join(name);
        if ours(&p) {
            std::fs::remove_file(&p)?;
            removed.push(p);
        }
    }
    if !removed.is_empty() {
        let _ = systemctl(&["daemon-reload"]);
    }
    Ok(removed)
}

/// One line for `paddock doctor` and `paddock gateway status`.
pub fn status_line(env: &HostEnv) -> String {
    let dir = unit_dir(env);
    if !ours(&dir.join(SOCKET_UNIT)) {
        return "  ok       the gateway executor starts when a request needs it (paddock gateway install hands this to systemd)".into();
    }
    let active = std::process::Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", SOCKET_UNIT])
        .status()
        .is_ok_and(|s| s.success());
    if active {
        "  ok       systemd starts the gateway executor on the first request (socket activation)"
            .into()
    } else {
        format!("  CHECK    {SOCKET_UNIT} is installed but not active: systemctl --user enable --now {SOCKET_UNIT}")
    }
}

#[cfg(test)]
#[path = "gatewayd_tests.rs"]
mod tests;
