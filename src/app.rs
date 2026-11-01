//! App mode: desktop agent apps (Electron apps such as ChatGPT/Codex Desktop,
//! and flatpak apps) in a sandbox.
//!
//! The app's windows are normal windows on the host desktop, through a
//! restricted Wayland socket (wlsec.rs). Its session bus is a private
//! `dbus-daemon` inside the sandbox, with a private keyring and paddock's
//! link and notification services (keyring.rs, portal.rs). With
//! `[apps.NAME] bus = "host-filtered"` it is the host bus through a filter
//! that passes only the desktop portals (open a URL, choose a file), the
//! notification service and the tray. Either way, the host keyring, the
//! systemd user manager and every other host service stay out of reach.

use crate::env::HostEnv;
use crate::spec::{sandbox_runtime_dir, SandboxSpec};
use anyhow::{bail, Context as _, Result};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Bus names an app may talk to.
pub const BUS_TALK: &[&str] = &[
    "org.freedesktop.portal.Desktop",
    "org.freedesktop.portal.Documents",
    "org.freedesktop.Notifications",
    "org.kde.StatusNotifierWatcher",
];

/// Bus names an app may own. None: xdg-dbus-proxy accepts only whole names or
/// a ".*" suffix, and "org.kde.*" would let the app impersonate host
/// services. Tray icons that need their own bus name may not appear.
pub const BUS_OWN: &[&str] = &[];

/// Lives while the app runs. Dropping it stops the bus proxy and closes the
/// restricted Wayland socket.
pub struct AppSession {
    _wayland: crate::wlsec::RestrictedSocket,
    proxy: Option<Child>,
    _forwarder: Option<ports::Forwarder>,
}

impl Drop for AppSession {
    fn drop(&mut self) {
        if let Some(p) = &mut self.proxy {
            let _ = p.kill();
            let _ = p.wait();
        }
    }
}

/// The session bus of an app, from the user configuration only: a project
/// file cannot give an app the host bus.
pub fn bus_mode(env: &HostEnv, name: &str) -> Result<AppBus> {
    let user = Manifest::load(env, &env.home.join(".paddock-no-project"))?.merged;
    Ok(user.apps.get(name).and_then(|a| a.bus).unwrap_or_default())
}

/// Where links for the app arrive inside the sandbox.
pub const INBOUND_DIR: &str = "/run/paddock/inbound";

pub fn start_session(
    env: &HostEnv,
    spec: &mut SandboxSpec,
    profile: &Profile,
) -> Result<AppSession> {
    let run = env.paddock_run().join(&spec.id);
    std::fs::create_dir_all(&run)?;

    // Link handoff: a helper in the sandbox listens on a socket in a folder
    // that the host can also reach. `paddock app open` writes a link there,
    // and the helper starts the app with it, inside the same sandbox, where
    // the app's single-instance lock passes it to the running window.
    let schemes = profile
        .app
        .as_ref()
        .map(|a| a.schemes.clone())
        .unwrap_or_default();
    let user = Manifest::load(env, &env.home.join(".paddock-no-project"))?.merged;
    let login_ports = user.apps.get(&profile.name).is_some_and(|a| a.login_ports);
    // On the private bus, links go through the host passthrough channel, so
    // paddock knows the login callback port of each login page it opens,
    // the same way as for terminal harnesses (login.rs).
    let callbacks = spec.private_bus && !login_ports;
    let inbound = run.join("inbound");
    if !schemes.is_empty() || login_ports || callbacks {
        std::fs::create_dir_all(&inbound)?;
        spec.add_mount(Mount::bind(
            &inbound,
            INBOUND_DIR,
            true,
            "app mode: link handoff",
        ));
        // Links start the app itself, on the bus that already runs.
        let app_cmd = match spec.command.split_first() {
            Some((w, rest)) if spec.private_bus && w == crate::keyring::TOOL => rest,
            _ => &spec.command[..],
        };
        spec.set_env(
            "PADDOCK_INBOUND_CMD",
            serde_json::to_string(app_cmd)?,
            "app mode: link handoff",
        );
        spec.set_env(
            "PADDOCK_INBOUND_SCHEMES",
            schemes.join(","),
            "app mode: link handoff",
        );
        if callbacks {
            spec.set_env("PADDOCK_INBOUND_PORTS", "1", "app mode: login callbacks");
        }
        if login_ports {
            spec.set_env("PADDOCK_INBOUND_PORTS", "1", "app mode: login_ports");
            spec.warnings.push(format!(
                "login_ports is on for {}: ports that it listens on are open on 127.0.0.1 of this host while it listens",
                profile.name
            ));
        }
        spec.setup.push("paddock inbound >/dev/null 2>&1 &".into());
    }
    let forwarder = if login_ports {
        Some(ports::Forwarder::start(inbound.clone()))
    } else if callbacks {
        Some(ports::Forwarder::start_filtered(
            inbound.clone(),
            crate::login::allowed,
        ))
    } else {
        None
    };

    let wl_path = run.join("wayland");
    let wayland = crate::wlsec::create(&wl_path, &format!("paddock.{}", spec.harness), &spec.id)?;
    spec.gui.host_socket = Some(wl_path);

    if spec.private_bus {
        // The bus itself starts inside the sandbox (keyring.rs). Apps that
        // open links with xdg-open instead of the portal reach the host the
        // same way.
        crate::login::open_shim(env, spec, "app mode: links to the host browser")?;
        spec.set_env(
            "DBUS_SESSION_BUS_ADDRESS",
            format!(
                "unix:path={}",
                sandbox_runtime_dir(spec.uid).join("bus").display()
            ),
            "app mode: private session bus",
        );
        return Ok(AppSession {
            _wayland: wayland,
            proxy: None,
            _forwarder: forwarder,
        });
    }
    let bus_addr = env
        .var("DBUS_SESSION_BUS_ADDRESS")
        .context("app mode needs the host session bus (DBUS_SESSION_BUS_ADDRESS is not set)")?
        .to_string();
    let proxy_exe = env
        .which("xdg-dbus-proxy")
        .context("app mode needs xdg-dbus-proxy (package xdg-dbus-proxy)")?;
    let bus_path = run.join("bus");
    let _ = std::fs::remove_file(&bus_path);
    let mut cmd = Command::new(proxy_exe);
    cmd.arg(&bus_addr).arg(&bus_path).arg("--filter");
    for n in BUS_TALK {
        cmd.arg(format!("--talk={n}"));
    }
    for n in BUS_OWN {
        cmd.arg(format!("--own={n}"));
    }
    let mut proxy = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let start = Instant::now();
    while !crate::gui::is_socket(&bus_path) {
        if let Some(st) = proxy.try_wait()? {
            bail!("xdg-dbus-proxy exited with {st}");
        }
        if start.elapsed() > Duration::from_secs(5) {
            let _ = proxy.kill();
            bail!("xdg-dbus-proxy did not create its socket");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    spec.bus_socket = Some(bus_path);
    let rt: PathBuf = sandbox_runtime_dir(spec.uid);
    spec.set_env(
        "DBUS_SESSION_BUS_ADDRESS",
        format!("unix:path={}", rt.join("bus").display()),
        "app mode: filtered host bus",
    );
    Ok(AppSession {
        _wayland: wayland,
        proxy: Some(proxy),
        _forwarder: forwarder,
    })
}

use crate::build::{self, Options};
use crate::detect::{self, Detector};
use crate::manifest::{AppBus, Manifest};
use crate::profile::{HarnessFactory, Profile};
use crate::spec::{GuiMode, Mount, MountKind, Network};

pub struct AppOptions {
    pub name: String,
    pub roots: Vec<PathBuf>,
    pub gpu: bool,
    pub network: Option<Network>,
    pub inherit_env: Option<bool>,
    pub auth: Option<String>,
    /// Extra arguments for the app, for example a link to open.
    pub args: Vec<String>,
    /// bwrap (default), docker, podman or srt. Flatpak apps need bwrap.
    pub backend: Option<String>,
    /// Open the first workspace folder in a new git worktree. `[apps.NAME]
    /// worktree` turns it on too.
    pub worktree: bool,
}

pub fn app_profile(env: &HostEnv, name: &str) -> Result<Profile> {
    let f = HarnessFactory::new(env)?;
    let p = f
        .profile(name)
        .with_context(|| format!("unknown app '{name}'. See paddock app list"))?;
    if p.app.is_none() {
        bail!("{name} is a terminal harness: run it with `paddock {name}`");
    }
    Ok(p.clone())
}

/// The workspace folders: from the command line, else `[apps.NAME] roots` in
/// the user configuration.
pub fn roots(env: &HostEnv, name: &str, cli: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let raw: Vec<PathBuf> = if !cli.is_empty() {
        cli.to_vec()
    } else {
        let user = Manifest::load(env, &env.home.join(".paddock-no-project"))?.merged;
        user.apps
            .get(name)
            .map(|a| a.roots.iter().map(|r| env.expand(r, &env.home)).collect())
            .unwrap_or_default()
    };
    if raw.is_empty() {
        bail!(
            "no workspace folders for {name}. Name them: paddock app run {name} ~/src/project, \
             or save them: paddock app install {name} ~/src/project"
        );
    }
    // The roots are resolved again at each start. A symlink on the way inside
    // a folder a sandbox can write (another root, a sandbox home) is refused:
    // a sandbox could have planted it to point the root somewhere else.
    let data = [
        env.paddock_data().join("homes"),
        env.paddock_data().join("worktrees"),
    ];
    let base: Vec<PathBuf> = data
        .iter()
        .map(|p| std::fs::canonicalize(p).unwrap_or_else(|_| p.clone()))
        .collect();
    let raw: Vec<PathBuf> = raw
        .into_iter()
        .map(|r| std::path::absolute(&r).unwrap_or(r))
        .collect();
    let first: Vec<PathBuf> = raw
        .iter()
        .map(|r| crate::safefs::resolve_approved(r, &base))
        .collect::<Result<_>>()?;
    let mut out = Vec::new();
    for (i, r) in raw.iter().enumerate() {
        let mut writable = base.clone();
        writable.extend(
            first
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .map(|(_, p)| p.clone()),
        );
        let c = crate::safefs::resolve_approved(r, &writable)?;
        if c == env.home || env.home.starts_with(&c) {
            bail!(
                "{} is the home directory or contains it; name project folders instead",
                c.display()
            );
        }
        if !out.contains(&c) {
            out.push(c);
        }
    }
    Ok(out)
}

/// Mount the other workspace folders, with the same git and secrets
/// protection as the first one.
fn add_roots(env: &HostEnv, spec: &mut SandboxSpec, m: &Manifest, extra: &[PathBuf]) -> Result<()> {
    for root in extra {
        spec.add_mount(Mount::same(root, true, "app workspace folder"));
        let cx = detect::Context {
            env,
            project: root,
            manifest: m,
        };
        detect::Git.apply(&cx, spec)?;
        detect::Secrets.apply(&cx, spec)?;
    }
    Ok(())
}

/// The extra arguments for the app: the workspace folders for editors that
/// open them (`open_roots`), then links or other arguments.
pub fn app_args(profile: &Profile, roots: &[PathBuf], extra: &[String]) -> Vec<String> {
    let mut v = Vec::new();
    if profile.app.as_ref().is_some_and(|a| a.open_roots) {
        v.extend(roots.iter().map(|r| r.display().to_string()));
    }
    v.extend(extra.iter().cloned());
    v
}

pub fn run(env: &HostEnv, o: &AppOptions) -> Result<i32> {
    let profile = app_profile(env, &o.name)?;
    let roots = roots(env, &o.name, &o.roots)?;
    let user = Manifest::load(env, &env.home.join(".paddock-no-project"))?.merged;
    let gpu = o.gpu || user.apps.get(&o.name).and_then(|a| a.gpu).unwrap_or(false);
    let worktree = o.worktree
        || user
            .apps
            .get(&o.name)
            .and_then(|a| a.worktree)
            .unwrap_or(false);
    if let Some(id) = profile.app.as_ref().and_then(|a| a.flatpak.clone()) {
        if worktree {
            bail!(
                "{} is a flatpak app: worktrees work only for apps that paddock sandboxes itself",
                o.name
            );
        }
        if o.backend.as_deref().is_some_and(|b| b != "bwrap") {
            bail!("{} is a flatpak app: flatpak runs its own bubblewrap sandbox, so it needs the bwrap backend", o.name);
        }
        return run_flatpak(env, &profile, &id, &roots, gpu, o);
    }
    let opts = Options {
        harness: o.name.clone(),
        project: Some(roots[0].clone()),
        backend: Some(o.backend.clone().unwrap_or_else(|| "bwrap".into())),
        network: o.network,
        gui: Some(GuiMode::App),
        gpu,
        no_yolo: true,
        inherit_env: o.inherit_env,
        auth: o.auth.clone(),
        args: app_args(&profile, &roots, &o.args),
        worktree,
        worktree_app: Some(o.name.clone()),
        ..Default::default()
    };
    let mut built = build::build(env, &opts)?;
    add_roots(env, &mut built.spec, &built.manifest, &roots[1..])?;
    crate::policy::enforce(env, &built.spec)?;
    let backend = crate::run::select_backend(env, &built)?;
    let wt = built.worktree.clone();
    let code = crate::run::run(env, built, backend.as_ref(), false, false)?.code;
    if let Some(w) = wt {
        report_worktree(env, &o.name, &w);
    }
    Ok(code)
}

/// After the app exits: print what it left in its worktree, and send a
/// notification when no terminal shows the text (a launcher start).
fn report_worktree(env: &HostEnv, name: &str, w: &build::Worktree) {
    eprintln!("\n{}", build::worktree_report(env, w));
    if std::io::IsTerminal::is_terminal(&std::io::stderr()) {
        return;
    }
    let (commits, changed) = build::worktree_counts(env, w);
    desktop_notice(env, &worktree_note(name, w, commits, changed));
}

/// The notification after an app worked in a worktree. The branch name and
/// the paths come from a repository that the sandbox wrote, so they go only
/// into the D-Bus message, never into the arguments of a host process.
pub fn worktree_note(
    name: &str,
    w: &build::Worktree,
    commits: usize,
    changed: Option<usize>,
) -> crate::broker::notify::Note {
    let changed = match changed {
        Some(n) => format!("{n}"),
        None => "an unknown number of".to_string(),
    };
    crate::broker::notify::Note {
        summary: format!("{name} worked on branch {}", w.branch),
        body: format!(
            "{commits} commits and {changed} uncommitted changes in {}. Review: git -C {} log {}",
            w.path.display(),
            w.repo.display(),
            w.branch
        ),
        ..Default::default()
    }
}

/// The notification when an app from a launcher entry cannot start.
pub fn start_error_note(name: &str, e: &anyhow::Error) -> crate::broker::notify::Note {
    crate::broker::notify::Note {
        summary: format!("paddock could not start {name}"),
        body: format!("{e:#}"),
        ..Default::default()
    }
}

/// Show a notification on the desktop through the session bus. The text can
/// come from a sandbox: no process gets it as arguments, where a leading "-"
/// would read as an option and every local user could read it in /proc.
/// Without a session bus, the terminal text is all there is.
pub fn desktop_notice(env: &HostEnv, n: &crate::broker::notify::Note) {
    if let Err(e) = crate::broker::notify::show(env, n) {
        log::debug!("no desktop notification: {e:#}");
    }
}

/// Flatpak apps run in flatpak's own sandbox, with every permission the app
/// asks for dropped (`--sandbox`, which also drops `flatpak override`
/// permissions) and only the workspace folders, the display, the GPU and the
/// portals added back. Never `--socket=session-bus`: that is the unfiltered
/// bus, through which `flatpak-spawn --host` runs commands on the host.
/// pasta gives it the same network path as bwrap. `fs` comes from
/// [`flatpak_workspace`].
pub fn flatpak_args(id: &str, command: Option<&str>, fs: &[String], gpu: bool) -> Vec<String> {
    let mut a: Vec<String> = [
        "run",
        "--sandbox",
        "--session-bus",
        "--share=network",
        "--socket=wayland",
        "--no-talk-name=org.freedesktop.Flatpak",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    if gpu {
        a.push("--device=dri".into());
    }
    for n in BUS_TALK {
        a.push(format!("--talk-name={n}"));
    }
    a.extend(fs.iter().cloned());
    if let Some(c) = command {
        a.push(format!("--command={c}"));
    }
    a.push(id.to_string());
    a
}

/// The workspace folders of a flatpak app, as flatpak options.
#[derive(Debug, Default)]
pub struct FlatpakWorkspace {
    /// `--filesystem` and `--nofilesystem` options.
    pub args: Vec<String>,
    /// Secret files that stay visible: flatpak can hide a folder below a
    /// shared folder (it puts an empty tmpfs there), but not a file.
    pub exposed: Vec<PathBuf>,
}

/// A path as flatpak reads it in `--filesystem=PATH[:MODE]`. A `:` would
/// split the path, and `~` or a name such as `home` would mean something else.
fn flatpak_path(p: &std::path::Path) -> Result<String> {
    let s = p
        .to_str()
        .with_context(|| format!("{} is not valid UTF-8", p.display()))?;
    if !s.starts_with('/') || s.contains([':', '\\']) || s.chars().any(|c| c.is_control()) {
        bail!("{s}: flatpak cannot share a path with ':', '\\' or control characters; rename it");
    }
    Ok(s.to_string())
}

/// The same workspace protection as a bwrap app: each folder gets the git
/// and secrets detectors, and the policy checks the result (no home
/// directory, no protected path, git config, hooks and pointer files
/// read-only). Then each mount becomes a flatpak option. flatpak binds the
/// longest path last, so a read-only file inside a shared folder stays
/// read-only, and the git directory is a mount point of its own, which the
/// app cannot rename.
pub fn flatpak_workspace(
    env: &HostEnv,
    spec: &mut SandboxSpec,
    m: &Manifest,
    roots: &[PathBuf],
) -> Result<FlatpakWorkspace> {
    add_roots(env, spec, m, roots)?;
    crate::policy::enforce(env, spec)?;
    let mut ws = FlatpakWorkspace::default();
    for mnt in &spec.mounts {
        if mnt.is_bind() && mnt.src.as_ref() != Some(&mnt.dst) {
            bail!(
                "flatpak cannot bind {} at another path (from {})",
                mnt.dst.display(),
                mnt.origin
            );
        }
        let p = flatpak_path(&mnt.dst)?;
        match mnt.kind {
            MountKind::Rw => ws.args.push(format!("--filesystem={p}:rw")),
            MountKind::Ro => ws.args.push(format!("--filesystem={p}:ro")),
            MountKind::MaskDir => ws.args.push(format!("--nofilesystem={p}")),
            MountKind::MaskFile => ws.exposed.push(mnt.dst.clone()),
            MountKind::Tmpfs => bail!(
                "flatpak cannot put an empty folder at {} (from {})",
                p,
                mnt.origin
            ),
        }
    }
    Ok(ws)
}

/// `[apps.NAME] unprotected_workspaces = true`, from the user configuration
/// only: a project file cannot turn the protection off.
pub fn unprotected_workspaces(env: &HostEnv, name: &str) -> Result<bool> {
    let user = Manifest::load(env, &env.home.join(".paddock-no-project"))?.merged;
    Ok(user
        .apps
        .get(name)
        .is_some_and(|a| a.unprotected_workspaces))
}

/// Refuse a start that would show secret files to a flatpak app, unless the
/// user turned the protection off for it. Returns the warning to print then.
pub fn check_exposed(name: &str, exposed: &[PathBuf], unprotected: bool) -> Result<Option<String>> {
    if exposed.is_empty() {
        return Ok(None);
    }
    let list = exposed
        .iter()
        .map(|p| format!("  {}", p.display()))
        .collect::<Vec<_>>()
        .join("\n");
    if !unprotected {
        bail!(
            "{name} is a flatpak app, and flatpak cannot hide single files, so it would read these secret files:\n{list}\n\
             Move them out of the workspace folders, or put them in a folder and hide the folder with [secrets] hide. \
             To start it anyway, with these files visible, set unprotected_workspaces = true in [apps.{name}] of ~/.config/paddock/config.toml."
        );
    }
    Ok(Some(format!(
        "unprotected_workspaces is on for {name}: it can read these secret files:\n{list}"
    )))
}

/// Host variables that flatpak itself needs on the host to start the app.
const FLATPAK_HOST_ENV: &[&str] = &[
    "HOME",
    "USER",
    "LOGNAME",
    "PATH",
    "LANG",
    "XDG_RUNTIME_DIR",
    "DBUS_SESSION_BUS_ADDRESS",
    "WAYLAND_DISPLAY",
    "XDG_SESSION_TYPE",
];

/// The environment of `flatpak run`. flatpak passes almost all of it to the
/// app, so it gets the same rules as a bwrap sandbox: no host control
/// channels or sockets, no cloud credentials unless named in `[env] pass`,
/// nothing named in `[env] hide`, and with `--env clean` nothing else.
pub fn flatpak_env(
    env: &HostEnv,
    m: &Manifest,
    profile: &Profile,
    inherit: bool,
) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    for k in FLATPAK_HOST_ENV {
        if let Some(v) = env.var(k) {
            out.insert(k.to_string(), v.to_string());
        }
    }
    for (k, v) in &env.vars {
        let named = m.env.pass.contains(k) || profile.state.env_pass.contains(k);
        if out.contains_key(k)
            || !crate::policy::inheritable_var(k, v)
            || m.env.hide.contains(k)
            || !(inherit || named)
            || (crate::policy::is_cloud_secret(k) && !named)
        {
            continue;
        }
        out.insert(k.clone(), v.clone());
    }
    out
}

/// The merged configuration for an app's first folder. The same rule as
/// every other sandbox: a project file that widens access (for example
/// `[secrets] reveal`) needs the user's approval first. The app can edit that
/// file, so it is never trusted on its own.
fn approved_manifest(env: &HostEnv, root: &std::path::Path) -> Result<Manifest> {
    let loaded = Manifest::load(env, root)?;
    crate::manifest::require_approval(env, &loaded, false)?;
    Ok(loaded.merged)
}

fn run_flatpak(
    env: &HostEnv,
    profile: &Profile,
    id: &str,
    roots: &[PathBuf],
    gpu: bool,
    o: &AppOptions,
) -> Result<i32> {
    let flatpak = env.which("flatpak").context("this app needs flatpak")?;
    let net = o.network.unwrap_or(Network::Egress);
    if !matches!(net, Network::Egress | Network::None) {
        bail!("flatpak apps support network modes egress and none, not {net:?}");
    }
    let mut spec = SandboxSpec::new(
        crate::state::new_id(&profile.name, &roots[0]),
        &profile.name,
        &roots[0],
        env.home.clone(),
        &env.home,
        env.uid,
        env.gid,
        &env.user,
    );
    let m = approved_manifest(env, &roots[0])?;
    let ws = flatpak_workspace(env, &mut spec, &m, roots)?;
    let warning = check_exposed(
        &profile.name,
        &ws.exposed,
        unprotected_workspaces(env, &profile.name)?,
    )?;
    if let Some(w) = &warning {
        eprintln!("paddock: warning: {w}");
    }
    let mut cmd = flatpak_args(
        id,
        profile.app.as_ref().and_then(|a| a.command.as_deref()),
        &ws.args,
        gpu,
    );
    cmd.extend(profile.launch.args.iter().cloned());
    cmd.extend(app_args(profile, roots, &o.args));
    let (program, args) = match net {
        Network::Egress => {
            let (pasta, mut pa) = crate::backend::bwrap::pasta_prefix(env, &spec)?;
            pa.push(flatpak.display().to_string());
            pa.extend(cmd);
            (pasta, pa)
        }
        _ => {
            cmd.retain(|a| a != "--share=network");
            (flatpak, cmd)
        }
    };
    let inherit = o.inherit_env.or(m.env.inherit).unwrap_or(true);
    let mut child = Command::new(&program)
        .args(&args)
        .env_clear()
        .envs(flatpak_env(env, &m, profile, inherit))
        .spawn()
        .with_context(|| format!("cannot start {}", program.display()))?;
    // Listed with the other sandboxes, so the panel shows it and its warning.
    let running = crate::state::Running {
        id: spec.id.clone(),
        harness: profile.name.clone(),
        project: roots[0].clone(),
        backend: "flatpak".into(),
        network: format!("{net:?}").to_lowercase(),
        gui: "app".into(),
        pid: child.id(),
        started: crate::state::now(),
        auth: String::new(),
        window_pid: None,
        warnings: if warning.is_some() {
            vec!["secret files visible (unprotected_workspaces)".to_string()]
        } else {
            Vec::new()
        },
        host_pid: std::process::id(),
        parent: None,
        spawn_id: None,
        logins: Vec::new(),
        unit: None,
        kv_project: None,
        pasture: None,
        credentials: Vec::new(),
        gateway: Vec::new(),
    };
    if let Err(e) = running.register(env) {
        log::warn!("cannot list the app with the running sandboxes: {e:#}");
    }
    let status = child.wait();
    let _ = std::fs::remove_dir_all(env.paddock_run().join(&spec.id));
    Ok(status?.code().unwrap_or(1))
}

pub fn desktop_file(env: &HostEnv, name: &str) -> PathBuf {
    env.data_dir
        .join("applications")
        .join(format!("paddock-{name}.desktop"))
}

/// A launcher entry that starts the app in a sandbox.
pub fn install(env: &HostEnv, name: &str) -> Result<PathBuf> {
    let p = app_profile(env, name)?;
    let app = p.app.clone().unwrap_or_default();
    // Never a build: see launchers::launcher_exe.
    let me = crate::launchers::launcher_exe(env)?;
    let display = if app.display_name.is_empty() {
        name.to_string()
    } else {
        app.display_name.clone()
    };
    let text = format!(
        "[Desktop Entry]\nType=Application\nName={display} (paddock)\nComment={display} in a paddock sandbox\nExec={} app run {name}\nIcon={}\nTerminal=false\nCategories=Development;\n",
        me.display(),
        app.icon.unwrap_or_else(|| "application-x-executable".into())
    );
    let f = desktop_file(env, name);
    std::fs::create_dir_all(f.parent().unwrap())?;
    std::fs::write(&f, text)?;
    Ok(f)
}

/// A link the app may receive: one of its own schemes, a sane length, and no
/// control characters.
pub fn valid_link(link: &str, schemes: &[String]) -> Result<()> {
    if link.len() > 8192 || link.chars().any(|c| c.is_control()) {
        bail!("the link is too long or has control characters");
    }
    let url = url::Url::parse(link).context("not a valid link")?;
    if !schemes.iter().any(|s| s == url.scheme()) {
        bail!(
            "{}:// links do not belong to this app (it handles: {})",
            url.scheme(),
            schemes.join(", ")
        );
    }
    Ok(())
}

/// `paddock inbound`, inside an app sandbox: start the app with each link
/// that arrives on the socket.
pub fn inbound_serve() -> Result<()> {
    use std::io::{BufRead, BufReader, Write};
    // Login callbacks of a terminal harness: ports only, no app to start.
    if std::env::var("PADDOCK_INBOUND_CMD").is_err()
        && std::env::var("PADDOCK_INBOUND_PORTS").is_ok_and(|v| v == "1")
    {
        ports::serve_inside(PathBuf::from(INBOUND_DIR))?;
        loop {
            std::thread::park();
        }
    }
    let cmd: Vec<String> = serde_json::from_str(
        &std::env::var("PADDOCK_INBOUND_CMD").context("not in an app sandbox")?,
    )?;
    let schemes: Vec<String> = std::env::var("PADDOCK_INBOUND_SCHEMES")
        .unwrap_or_default()
        .split(',')
        .map(str::to_string)
        .collect();
    if cmd.is_empty() {
        bail!("no app command");
    }
    if std::env::var("PADDOCK_INBOUND_PORTS").is_ok_and(|v| v == "1") {
        ports::serve_inside(PathBuf::from(INBOUND_DIR))?;
    }
    let sock = PathBuf::from(INBOUND_DIR).join("open.sock");
    let _ = std::fs::remove_file(&sock);
    let listener = std::os::unix::net::UnixListener::bind(&sock)?;
    for conn in listener.incoming().flatten() {
        let mut line = String::new();
        let mut reader = BufReader::new(std::io::Read::take(&conn, 9000));
        if reader.read_line(&mut line).is_err() {
            continue;
        }
        let link = line.trim_end_matches(['\r', '\n']);
        let reply = match valid_link(link, &schemes) {
            Err(e) => format!("error: {e}"),
            Ok(()) => match Command::new(&cmd[0])
                .args(&cmd[1..])
                .arg(link)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(mut child) => {
                    std::thread::spawn(move || child.wait());
                    "ok".to_string()
                }
                Err(e) => format!("error: cannot start the app: {e}"),
            },
        };
        let mut w = &conn;
        let _ = writeln!(w, "{reply}");
    }
    Ok(())
}

/// `paddock app open NAME LINK`, on the host: give a link to the running app
/// sandbox, or start the app with it.
pub fn open(env: &HostEnv, name: &str, link: &str) -> Result<i32> {
    use std::io::{BufRead, BufReader, Write};
    let profile = app_profile(env, name)?;
    let schemes = profile
        .app
        .as_ref()
        .map(|a| a.schemes.clone())
        .unwrap_or_default();
    valid_link(link, &schemes)?;
    // A running flatpak app has no link socket: a second `flatpak run` hands
    // the link to it.
    let running = crate::state::Running::list(env)
        .into_iter()
        .find(|r| r.harness == name && r.gui == "app" && r.backend != "flatpak");
    if let Some(r) = running {
        let inbound = env.paddock_run().join(&r.id).join("inbound");
        let mut s = ports::connect_inside(&inbound, "open.sock")
            .with_context(|| format!("the {name} sandbox is not ready for links yet"))?;
        writeln!(s, "{link}")?;
        let mut reply = String::new();
        BufReader::new(&s).read_line(&mut reply)?;
        let reply = reply.trim();
        if reply != "ok" {
            bail!("the {name} sandbox refused the link: {reply}");
        }
        return Ok(0);
    }
    run(
        env,
        &AppOptions {
            name: name.to_string(),
            roots: vec![],
            gpu: false,
            network: None,
            inherit_env: None,
            auth: None,
            args: vec![link.to_string()],
            backend: None,
            worktree: false,
        },
    )
}

pub fn link_desktop_file(env: &HostEnv, name: &str) -> PathBuf {
    env.data_dir
        .join("applications")
        .join(format!("paddock-{name}-links.desktop"))
}

fn handlers_file(env: &HostEnv) -> PathBuf {
    env.paddock_state().join("link-handlers.json")
}

fn xdg_mime(env: &HostEnv, args: &[&str]) -> Result<String> {
    let exe = env
        .which("xdg-mime")
        .context("link handling needs xdg-mime (package xdg-utils)")?;
    crate::env::capture(Command::new(exe).args(args))
}

/// Make paddock the handler of the app's URL schemes, and remember the old
/// handlers so that `release_links` can give them back.
pub fn claim_links(env: &HostEnv, name: &str) -> Result<Vec<(String, String)>> {
    let p = app_profile(env, name)?;
    let schemes = p.app.map(|a| a.schemes).unwrap_or_default();
    if schemes.is_empty() {
        return Ok(vec![]);
    }
    // Never a build: see launchers::launcher_exe.
    let me = crate::launchers::launcher_exe(env)?;
    let mime: String = schemes
        .iter()
        .map(|s| format!("x-scheme-handler/{s};"))
        .collect();
    let f = link_desktop_file(env, name);
    std::fs::create_dir_all(f.parent().unwrap())?;
    std::fs::write(
        &f,
        format!("[Desktop Entry]\nType=Application\nName={name} links (paddock)\nExec={} app open {name} %u\nNoDisplay=true\nTerminal=false\nMimeType={mime}\n", me.display()),
    )?;
    let mut saved: std::collections::BTreeMap<String, String> =
        std::fs::read_to_string(handlers_file(env))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
    let ours = f.file_name().unwrap().to_string_lossy().into_owned();
    let mut out = Vec::new();
    for s in schemes {
        let mime = format!("x-scheme-handler/{s}");
        let old = xdg_mime(env, &["query", "default", &mime]).unwrap_or_default();
        if old != ours {
            saved.insert(s.clone(), old.clone());
        }
        xdg_mime(env, &["default", &ours, &mime])?;
        out.push((s, saved.get(&out_key(&mime)).cloned().unwrap_or(old)));
    }
    std::fs::create_dir_all(handlers_file(env).parent().unwrap())?;
    std::fs::write(handlers_file(env), serde_json::to_string_pretty(&saved)?)?;
    Ok(out)
}

fn out_key(mime: &str) -> String {
    mime.trim_start_matches("x-scheme-handler/").to_string()
}

/// Give the app's URL schemes back to the handlers they had before.
pub fn release_links(env: &HostEnv, name: &str) -> Result<Vec<(String, String)>> {
    let p = app_profile(env, name)?;
    let schemes = p.app.map(|a| a.schemes).unwrap_or_default();
    let mut saved: std::collections::BTreeMap<String, String> =
        std::fs::read_to_string(handlers_file(env))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
    let mut out = Vec::new();
    for s in schemes {
        if let Some(old) = saved.remove(&s) {
            if !old.is_empty() {
                xdg_mime(env, &["default", &old, &format!("x-scheme-handler/{s}")])?;
            }
            out.push((s, old));
        }
    }
    let _ = std::fs::remove_file(link_desktop_file(env, name));
    if handlers_file(env).exists() {
        std::fs::write(handlers_file(env), serde_json::to_string_pretty(&saved)?)?;
    }
    Ok(out)
}

/// Login callbacks to http://127.0.0.1:PORT. The app listens in the
/// sandbox's own network namespace, which the host browser cannot reach. When
/// `[apps.NAME] login_ports = true`, the helper in the sandbox reports the
/// ports that the app listens on, and paddock on the host listens on the same
/// ports on 127.0.0.1 only, and passes each connection through a Unix socket
/// to the helper, which connects to the port inside. A port is open on the
/// host only while the app listens on it.
pub mod ports {
    use anyhow::Result;
    use std::collections::{BTreeMap, BTreeSet};
    use std::io::{Read, Write};
    use std::net::{Shutdown, TcpListener, TcpStream};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    const PORTS_FILE: &str = "ports.json";
    const FWD_SOCK: &str = "fwd.sock";

    /// TCP ports in LISTEN state, from the text of /proc/net/tcp or
    /// /proc/net/tcp6. Privileged ports are left out.
    pub fn listening(tcp: &str) -> BTreeSet<u16> {
        let mut out = BTreeSet::new();
        for line in tcp.lines().skip(1) {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 4 || f[3] != "0A" {
                continue;
            }
            if let Some(port) = f[1]
                .rsplit(':')
                .next()
                .and_then(|h| u16::from_str_radix(h, 16).ok())
            {
                if port >= 1024 {
                    out.insert(port);
                }
            }
        }
        out
    }

    fn scan() -> BTreeSet<u16> {
        let mut s = BTreeSet::new();
        for f in ["/proc/net/tcp", "/proc/net/tcp6"] {
            s.extend(listening(&std::fs::read_to_string(f).unwrap_or_default()));
        }
        s
    }

    fn copy_both(a: TcpStream, b: UnixStream) {
        let (Ok(mut a2), Ok(mut b2)) = (a.try_clone(), b.try_clone()) else {
            return;
        };
        let t = std::thread::spawn(move || {
            let _ = std::io::copy(&mut a2, &mut b2);
            let _ = b2.shutdown(Shutdown::Write);
        });
        let (mut a, mut b) = (a, b);
        let _ = std::io::copy(&mut b, &mut a);
        let _ = a.shutdown(Shutdown::Write);
        let _ = t.join();
    }

    /// Inside the sandbox: report listening ports, and connect forwarded
    /// connections to them.
    pub fn serve_inside(dir: PathBuf) -> Result<()> {
        let fwd = dir.join(FWD_SOCK);
        let _ = std::fs::remove_file(&fwd);
        let listener = UnixListener::bind(&fwd)?;
        std::thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                std::thread::spawn(move || {
                    // Read the port line one byte at a time: a buffered read
                    // could take the first bytes of the forwarded stream too.
                    let mut line = Vec::new();
                    let mut b = [0u8; 1];
                    let mut r = &conn;
                    while line.len() < 8 {
                        match r.read(&mut b) {
                            Ok(1) if b[0] == b'\n' => break,
                            Ok(1) => line.push(b[0]),
                            _ => return,
                        }
                    }
                    let Ok(port) = String::from_utf8_lossy(&line).trim().parse::<u16>() else {
                        return;
                    };
                    // Only ports the app listens on right now.
                    if !scan().contains(&port) {
                        return;
                    }
                    let Ok(tcp) = TcpStream::connect(("127.0.0.1", port)) else {
                        return;
                    };
                    copy_both(tcp, conn);
                });
            }
        });
        std::thread::spawn(move || {
            let mut last = BTreeSet::new();
            loop {
                let now = scan();
                if now != last {
                    let tmp = dir.join("ports.json.tmp");
                    if std::fs::write(&tmp, serde_json::to_string(&now).unwrap_or_default()).is_ok()
                    {
                        let _ = std::fs::rename(&tmp, dir.join(PORTS_FILE));
                    }
                    last = now;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        });
        Ok(())
    }

    /// On the host: follow the reported ports and listen on 127.0.0.1 for
    /// each, while it is reported. Stops when dropped.
    pub struct Forwarder {
        stop: Arc<AtomicBool>,
    }

    impl Drop for Forwarder {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
        }
    }

    impl Forwarder {
        pub fn start(dir: PathBuf) -> Self {
            Self::start_filtered(dir, |_| true)
        }

        /// Forward only the reported ports that `filter` accepts, for example
        /// a login callback port while its login is under way.
        pub fn start_filtered(dir: PathBuf, filter: fn(u16) -> bool) -> Self {
            let stop = Arc::new(AtomicBool::new(false));
            let s2 = stop.clone();
            std::thread::spawn(move || manage(&dir, &s2, filter));
            Self { stop }
        }
    }

    /// The sandbox writes this folder, so the ports file is read without
    /// following a symlink, never from a FIFO, and only while it is small.
    fn read_ports(dir: &Path) -> BTreeSet<u16> {
        crate::safefs::Root::open(dir)
            .and_then(|r| r.read(Path::new(PORTS_FILE), 64 * 1024))
            .ok()
            .flatten()
            .and_then(|t| serde_json::from_slice(&t).ok())
            .unwrap_or_default()
    }

    /// Connect to a socket that the sandbox made in `dir`. A symlink there
    /// is refused, so the sandbox cannot point paddock at a host socket.
    pub fn connect_inside(dir: &Path, name: &str) -> Result<UnixStream> {
        let root = crate::safefs::Root::open(dir)?;
        root.dir(Path::new(""), false)?
            .connect(std::ffi::OsStr::new(name))
    }

    fn manage(dir: &Path, stop: &Arc<AtomicBool>, filter: fn(u16) -> bool) {
        let mut open: BTreeMap<u16, Arc<AtomicBool>> = BTreeMap::new();
        while !stop.load(Ordering::SeqCst) {
            let mut wanted = read_ports(dir);
            wanted.retain(|p| filter(*p));
            open.retain(|p, flag| {
                let keep = wanted.contains(p);
                if !keep {
                    flag.store(true, Ordering::SeqCst);
                }
                keep
            });
            for p in wanted {
                if open.contains_key(&p) {
                    continue;
                }
                // Loopback only, never another address of the host.
                let l = match TcpListener::bind(("127.0.0.1", p)) {
                    Ok(l) => l,
                    Err(e) => {
                        log::debug!("cannot forward port {p}: {e}");
                        continue;
                    }
                };
                log::debug!("forwarding port {p} into the sandbox");
                let _ = l.set_nonblocking(true);
                let flag = Arc::new(AtomicBool::new(false));
                open.insert(p, flag.clone());
                let (sock, stop) = (dir.to_path_buf(), stop.clone());
                std::thread::spawn(move || {
                    while !flag.load(Ordering::SeqCst) && !stop.load(Ordering::SeqCst) {
                        match l.accept() {
                            Ok((tcp, _)) => {
                                let _ = tcp.set_nonblocking(false);
                                let sock = sock.clone();
                                std::thread::spawn(move || {
                                    let mut u = match connect_inside(&sock, FWD_SOCK) {
                                        Ok(u) => u,
                                        Err(e) => {
                                            log::warn!("port {p}: cannot reach the sandbox: {e}");
                                            return;
                                        }
                                    };
                                    if writeln!(u, "{p}").is_err() {
                                        return;
                                    }
                                    copy_both(tcp, u);
                                });
                            }
                            Err(_) => std::thread::sleep(Duration::from_millis(50)),
                        }
                    }
                });
            }
            std::thread::sleep(Duration::from_millis(300));
        }
        for f in open.values() {
            f.store(true, Ordering::SeqCst);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn reads_listening_ports_from_proc() {
            let tcp = "  sl  local_address rem_address   st\n   0: 0100007F:B8AC 00000000:0000 0A x\n   1: 0100007F:B8AD 0100007F:1F90 01 x\n   2: 00000000:0016 00000000:0000 0A x\n";
            assert_eq!(listening(tcp), BTreeSet::from([0xB8AC]));
        }

        #[test]
        fn never_follows_links_or_fifos_the_sandbox_plants() {
            let t = tempfile::tempdir().unwrap();
            let (inbound, host) = (t.path().join("inbound"), t.path().join("host"));
            std::fs::create_dir_all(&inbound).unwrap();
            std::fs::create_dir_all(&host).unwrap();
            // A host socket the sandbox must not reach.
            let _l = UnixListener::bind(host.join("bus")).unwrap();
            std::os::unix::fs::symlink(host.join("bus"), inbound.join(FWD_SOCK)).unwrap();
            assert!(connect_inside(&inbound, FWD_SOCK).is_err());
            std::fs::remove_file(inbound.join(FWD_SOCK)).unwrap();
            let _own = UnixListener::bind(inbound.join(FWD_SOCK)).unwrap();
            assert!(connect_inside(&inbound, FWD_SOCK).is_ok());

            // A FIFO as the ports file reads as no ports and does not hang.
            let c = std::ffi::CString::new(inbound.join(PORTS_FILE).to_str().unwrap()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
            assert!(read_ports(&inbound).is_empty());
            std::fs::remove_file(inbound.join(PORTS_FILE)).unwrap();
            std::fs::write(host.join("p.json"), "[8080]").unwrap();
            std::os::unix::fs::symlink(host.join("p.json"), inbound.join(PORTS_FILE)).unwrap();
            assert!(read_ports(&inbound).is_empty());
            std::fs::remove_file(inbound.join(PORTS_FILE)).unwrap();
            std::fs::write(inbound.join(PORTS_FILE), "[8080]").unwrap();
            assert_eq!(read_ports(&inbound), BTreeSet::from([8080]));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_text_that_starts_with_a_dash_is_never_an_argument() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        // A fake notify-send that records any arguments it gets.
        let bin = t.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let record = t.path().join("argv.txt");
        let script = bin.join("notify-send");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{}'\n",
                record.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut vars = std::collections::BTreeMap::new();
        vars.insert("PATH".to_string(), bin.display().to_string());
        let env = HostEnv::from_parts(t.path().join("home"), "demo".into(), 1000, 1000, vars);

        // A branch and a worktree path that a sandbox chose, and error text.
        let w = build::Worktree {
            repo: PathBuf::from("/home/demo/src/app"),
            path: PathBuf::from("/home/demo/.local/share/paddock/worktrees/--help"),
            branch: "--version".into(),
            base: "main".into(),
        };
        let n = worktree_note("-x", &w, 2, Some(1));
        assert_eq!(n.summary, "-x worked on branch --version");
        assert!(n.body.contains("worktrees/--help"), "{}", n.body);
        assert!(n.body.ends_with("log --version"), "{}", n.body);
        let e = start_error_note("app", &anyhow::anyhow!("--icon=/home/demo/x"));
        assert_eq!(e.body, "--icon=/home/demo/x");

        // The text goes to the session bus as D-Bus strings, which are never
        // parsed as options. Without a bus there is no notification, and no
        // fallback to a process.
        desktop_notice(&env, &n);
        desktop_notice(&env, &e);
        assert!(!record.exists(), "notify-send ran");
    }

    #[test]
    fn editors_get_their_folders_and_chat_apps_do_not() {
        let editor: Profile = toml::from_str("name = \"e\"\n[app]\nopen_roots = true\n").unwrap();
        let chat: Profile = toml::from_str("name = \"c\"\n[app]\n").unwrap();
        let roots = [PathBuf::from("/w/a"), PathBuf::from("/w/b")];
        let link = ["x://y".to_string()];
        assert_eq!(app_args(&editor, &roots, &link), ["/w/a", "/w/b", "x://y"]);
        assert_eq!(app_args(&chat, &roots, &link), ["x://y"]);
    }

    #[test]
    fn links_must_use_the_app_scheme() {
        let s = vec!["claude".to_string()];
        assert!(valid_link("claude://login/callback?code=abc", &s).is_ok());
        assert!(valid_link("codex://x", &s).is_err());
        assert!(valid_link("https://example.com", &s).is_err());
        assert!(valid_link("claude://a\nb", &s).is_err());
        assert!(valid_link(&format!("claude://{}", "a".repeat(9000)), &s).is_err());
    }

    #[test]
    fn flatpak_never_gets_the_full_bus_or_host_commands() {
        let fs = ["--filesystem=/w/two:rw".to_string()];
        let a = flatpak_args("org.example.App", None, &fs, false);
        assert_eq!(&a[..2], ["run", "--sandbox"]);
        assert!(a.contains(&"--session-bus".to_string()));
        assert!(!a
            .iter()
            .any(|x| x == "--socket=session-bus" || x == "--socket=system-bus"));
        assert!(a.contains(&"--no-talk-name=org.freedesktop.Flatpak".to_string()));
        assert!(!a
            .iter()
            .any(|x| x.starts_with("--filesystem=home") || x.starts_with("--filesystem=host")));
        assert!(a.contains(&"--filesystem=/w/two:rw".to_string()));
        assert_eq!(a.last().unwrap(), "org.example.App");
    }

    fn git(dir: &std::path::Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@localhost"])
            .args(args)
            .stdout(Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    }

    fn repo(dir: &std::path::Path) {
        std::fs::create_dir_all(dir).unwrap();
        git(dir, &["init", "-q", "-b", "main"]);
        git(dir, &["commit", "-q", "--allow-empty", "-m", "one"]);
    }

    fn test_env(t: &std::path::Path) -> HostEnv {
        HostEnv::from_parts(
            t.join("home"),
            "demo".into(),
            1000,
            1000,
            Default::default(),
        )
    }

    fn test_spec(env: &HostEnv, project: &std::path::Path) -> SandboxSpec {
        SandboxSpec::new(
            "t".into(),
            "f",
            project,
            env.home.join("sandbox"),
            &env.home,
            1000,
            1000,
            "demo",
        )
    }

    fn workspace(env: &HostEnv, m: &Manifest, roots: &[PathBuf]) -> Result<FlatpakWorkspace> {
        let mut spec = test_spec(env, &roots[0]);
        flatpak_workspace(env, &mut spec, m, roots)
    }

    #[test]
    fn flatpak_folders_keep_git_files_read_only() {
        let t = tempfile::tempdir().unwrap();
        let env = test_env(t.path());
        std::fs::create_dir_all(&env.home).unwrap();
        let (a, b) = (t.path().join("a"), t.path().join("b"));
        repo(&a);
        repo(&b);
        let ws = workspace(&env, &Manifest::default(), &[a.clone(), b.clone()]).unwrap();
        assert!(ws.exposed.is_empty());
        // The second folder is protected like the first.
        for root in [&a, &b] {
            let has = |p: PathBuf, mode: &str| {
                let want = format!("--filesystem={}:{mode}", p.display());
                assert!(ws.args.contains(&want), "{want} in {:?}", ws.args);
            };
            let dot = root.join(".git");
            has(root.to_path_buf(), "rw");
            // Its own mount point, so the app cannot rename it.
            has(dot.clone(), "rw");
            for f in [
                "hooks",
                "config",
                "config.worktree",
                "commondir",
                "objects/info/alternates",
            ] {
                has(dot.join(f), "ro");
            }
        }
    }

    #[test]
    fn flatpak_keeps_a_worktree_dot_git_file_read_only() {
        let t = tempfile::tempdir().unwrap();
        let env = test_env(t.path());
        std::fs::create_dir_all(&env.home).unwrap();
        let main = t.path().join("main");
        repo(&main);
        let wt = t.path().join("wt");
        git(&main, &["worktree", "add", "-q", wt.to_str().unwrap()]);
        let ws = workspace(&env, &Manifest::default(), std::slice::from_ref(&wt)).unwrap();
        let want = format!("--filesystem={}:ro", wt.join(".git").display());
        assert!(ws.args.contains(&want), "{:?}", ws.args);
        let want = format!("--filesystem={}:ro", main.join(".git/config").display());
        assert!(ws.args.contains(&want), "{:?}", ws.args);
    }

    #[test]
    fn flatpak_refuses_secret_files_unless_the_user_opts_out() {
        let t = tempfile::tempdir().unwrap();
        let env = test_env(t.path());
        std::fs::create_dir_all(&env.home).unwrap();
        let (a, b) = (t.path().join("a"), t.path().join("b"));
        std::fs::create_dir_all(b.join("deploy")).unwrap();
        std::fs::create_dir_all(&a).unwrap();
        std::fs::write(b.join("deploy/.env"), "TOKEN=x").unwrap();
        std::fs::write(b.join(".env.example"), "TOKEN=").unwrap();
        let ws = workspace(&env, &Manifest::default(), &[a, b.clone()]).unwrap();
        assert_eq!(ws.exposed, [b.join("deploy/.env")]);
        let e = check_exposed("f", &ws.exposed, false)
            .unwrap_err()
            .to_string();
        assert!(
            e.contains(&b.join("deploy/.env").display().to_string()),
            "{e}"
        );
        assert!(e.contains("unprotected_workspaces = true"), "{e}");
        let w = check_exposed("f", &ws.exposed, true).unwrap().unwrap();
        assert!(w.contains("deploy/.env"), "{w}");
        assert_eq!(check_exposed("f", &[], false).unwrap(), None);
    }

    #[test]
    fn flatpak_needs_approval_for_a_project_reveal() {
        let tmp = tempfile::tempdir().unwrap();
        let env = HostEnv::from_parts(
            tmp.path().join("home"),
            "demo".into(),
            1,
            1,
            Default::default(),
        );
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join(".env"), "TOKEN=x").unwrap();
        std::fs::write(ws.join("paddock.toml"), "[secrets]\nreveal = [\".env\"]\n").unwrap();
        // Tests have no terminal: an unapproved widening is refused.
        let e = approved_manifest(&env, &ws).unwrap_err().to_string();
        assert!(e.contains("not approved"), "{e}");
        // After approval it loads, with the reveal.
        let loaded = Manifest::load(&env, &ws).unwrap();
        let (file, m) = loaded.project.clone().unwrap();
        crate::manifest::approve(&env, &file, &m).unwrap();
        assert!(!approved_manifest(&env, &ws)
            .unwrap()
            .secrets
            .reveal
            .is_empty());
    }

    #[test]
    fn flatpak_hides_a_secret_folder() {
        let t = tempfile::tempdir().unwrap();
        let env = test_env(t.path());
        std::fs::create_dir_all(&env.home).unwrap();
        let a = t.path().join("a");
        std::fs::create_dir_all(a.join("private")).unwrap();
        let mut m = Manifest::default();
        m.secrets.hide.push("private".into());
        let ws = workspace(&env, &m, std::slice::from_ref(&a)).unwrap();
        let want = format!("--nofilesystem={}", a.join("private").display());
        assert!(ws.args.contains(&want), "{:?}", ws.args);
    }

    #[test]
    fn flatpak_refuses_denied_folders() {
        let t = tempfile::tempdir().unwrap();
        let env = test_env(t.path());
        for d in [".ssh", ".config/gh", "src"] {
            std::fs::create_dir_all(env.home.join(d)).unwrap();
        }
        for root in [
            env.home.clone(),
            env.home.join(".ssh"),
            env.home.join(".config"),
            t.path().to_path_buf(),
        ] {
            let e = workspace(
                &env,
                &Manifest::default(),
                &[env.home.join("src"), root.clone()],
            )
            .unwrap_err()
            .to_string();
            assert!(e.contains("policy"), "{}: {e}", root.display());
        }
        assert!(workspace(&env, &Manifest::default(), &[env.home.join("src")]).is_ok());
        // flatpak would split a path at ':'.
        let odd = t.path().join("a:ro");
        std::fs::create_dir_all(&odd).unwrap();
        assert!(workspace(&env, &Manifest::default(), &[odd]).is_err());
    }

    #[test]
    fn the_flatpak_opt_out_comes_from_the_user_configuration_only() {
        let t = tempfile::tempdir().unwrap();
        let env = test_env(t.path());
        let cfg = env.paddock_config();
        std::fs::create_dir_all(&cfg).unwrap();
        let project = t.path().join("p");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            project.join(crate::manifest::PROJECT_FILE),
            "[apps.f]\nunprotected_workspaces = true\n",
        )
        .unwrap();
        // Even a manifest merged with the project file keeps it off.
        let merged = Manifest::load(&env, &project).unwrap().merged;
        assert!(!merged
            .apps
            .get("f")
            .is_some_and(|a| a.unprotected_workspaces));
        assert!(!unprotected_workspaces(&env, "f").unwrap());
        std::fs::write(
            cfg.join("config.toml"),
            "[apps.f]\nunprotected_workspaces = true\n",
        )
        .unwrap();
        assert!(unprotected_workspaces(&env, "f").unwrap());
        assert!(!unprotected_workspaces(&env, "other").unwrap());
    }

    #[test]
    fn flatpak_gets_the_filtered_host_environment() {
        let t = tempfile::tempdir().unwrap();
        let sock = t.path().join("ctl.sock");
        let _l = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let vars: std::collections::BTreeMap<String, String> = [
            ("HOME", "/home/demo"),
            ("PATH", "/usr/bin"),
            ("DBUS_SESSION_BUS_ADDRESS", "unix:path=/run/user/1000/bus"),
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
            ("EDITOR", "nvim"),
            ("CLAUDE_CODE_MESSAGING_TOKEN", "x"),
            ("HYPRLAND_INSTANCE_SIGNATURE", "x"),
            ("SSH_AUTH_SOCK", "/run/user/1000/ssh"),
            ("AWS_SECRET_ACCESS_KEY", "x"),
            ("GITHUB_TOKEN", "x"),
            ("HIDDEN_ONE", "x"),
            ("ANY_CONTROL", sock.to_str().unwrap()),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let env = HostEnv::from_parts("/home/demo".into(), "demo".into(), 1000, 1000, vars);
        let profile: Profile =
            toml::from_str("name = \"f\"\n[app]\nflatpak = \"org.x\"\n").unwrap();
        let mut m = Manifest::default();
        m.env.hide.push("HIDDEN_ONE".into());
        let e = flatpak_env(&env, &m, &profile, true);
        for k in [
            "HOME",
            "PATH",
            "DBUS_SESSION_BUS_ADDRESS",
            "XDG_RUNTIME_DIR",
            "EDITOR",
        ] {
            assert!(e.contains_key(k), "{k}");
        }
        for k in [
            "CLAUDE_CODE_MESSAGING_TOKEN",
            "HYPRLAND_INSTANCE_SIGNATURE",
            "SSH_AUTH_SOCK",
            "AWS_SECRET_ACCESS_KEY",
            "GITHUB_TOKEN",
            "HIDDEN_ONE",
            "ANY_CONTROL",
        ] {
            assert!(!e.contains_key(k), "{k}");
        }
        m.env.pass.push("GITHUB_TOKEN".into());
        assert!(flatpak_env(&env, &m, &profile, true).contains_key("GITHUB_TOKEN"));
        let clean = flatpak_env(&env, &m, &profile, false);
        assert!(!clean.contains_key("EDITOR"));
        assert!(clean.contains_key("GITHUB_TOKEN") && clean.contains_key("HOME"));
    }

    /// The extra folders of a bwrap app get the git and secrets detectors,
    /// and the policy refuses them without.
    #[test]
    fn bwrap_app_extra_folders_are_protected_like_the_first() {
        let t = tempfile::tempdir().unwrap();
        let env = test_env(t.path());
        std::fs::create_dir_all(&env.home).unwrap();
        let (a, b) = (t.path().join("a"), t.path().join("b"));
        repo(&a);
        repo(&b);
        std::fs::write(b.join(".env"), "TOKEN=x").unwrap();
        let mut bare = test_spec(&env, &a);
        bare.add_mount(Mount::same(&b, true, "app workspace folder"));
        assert!(crate::policy::enforce(&env, &bare).is_err());
        let mut spec = test_spec(&env, &a);
        add_roots(
            &env,
            &mut spec,
            &Manifest::default(),
            std::slice::from_ref(&b),
        )
        .unwrap();
        crate::policy::enforce(&env, &spec).unwrap();
        let kind = |p: PathBuf| spec.mounts.iter().find(|m| m.dst == p).map(|m| m.kind);
        assert_eq!(kind(b.join(".env")), Some(MountKind::MaskFile));
        assert_eq!(kind(b.join(".git/config")), Some(MountKind::Ro));
        assert_eq!(kind(b.join(".git/hooks")), Some(MountKind::Ro));
    }
}
