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
use crate::spec::{GuiMode, Mount, Network};

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
    eprintln!("\n{}", build::worktree_report(w));
    if std::io::IsTerminal::is_terminal(&std::io::stderr()) {
        return;
    }
    let Some(ns) = env.which("notify-send") else {
        return;
    };
    let (commits, changed) = build::worktree_counts(w);
    let _ = std::process::Command::new(ns)
        .args([
            "--app-name=paddock",
            &format!("{name} worked on branch {}", w.branch),
            &format!(
                "{commits} commits and {changed} uncommitted changes in {}. Review: git -C {} log {}",
                w.path.display(),
                w.repo.display(),
                w.branch
            ),
        ])
        .status();
}

/// Flatpak apps run in flatpak's own sandbox, with every permission the app
/// asks for dropped (`--sandbox`) and only the workspace folders, the
/// display, the GPU and the portals added back. Never `--socket=session-bus`:
/// that is the unfiltered bus, through which `flatpak-spawn --host` runs
/// commands on the host. pasta gives it the same network path as bwrap.
pub fn flatpak_args(id: &str, command: Option<&str>, roots: &[PathBuf], gpu: bool) -> Vec<String> {
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
    for r in roots {
        a.push(format!("--filesystem={}", r.display()));
    }
    if let Some(c) = command {
        a.push(format!("--command={c}"));
    }
    a.push(id.to_string());
    a
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
    let mut cmd = flatpak_args(
        id,
        profile.app.as_ref().and_then(|a| a.command.as_deref()),
        roots,
        gpu,
    );
    cmd.extend(profile.launch.args.iter().cloned());
    cmd.extend(app_args(profile, roots, &o.args));
    let (program, args) = match net {
        Network::Egress => {
            let spec = SandboxSpec::new(
                crate::state::new_id(&profile.name, &roots[0]),
                &profile.name,
                &roots[0],
                env.home.clone(),
                &env.home,
                env.uid,
                env.gid,
                &env.user,
            );
            let (pasta, mut pa) = crate::backend::bwrap::pasta_prefix(env, &spec)?;
            pa.push(flatpak.display().to_string());
            pa.extend(cmd);
            (pasta, pa)
        }
        Network::None => {
            cmd.retain(|a| a != "--share=network");
            (flatpak, cmd)
        }
        other => bail!("flatpak apps support network modes egress and none, not {other:?}"),
    };
    let status = Command::new(&program)
        .args(&args)
        .status()
        .with_context(|| format!("cannot start {}", program.display()))?;
    Ok(status.code().unwrap_or(1))
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
    let me = std::env::current_exe().and_then(std::fs::canonicalize)?;
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
    let running = crate::state::Running::list(env)
        .into_iter()
        .find(|r| r.harness == name && r.gui == "app");
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
    let me = std::env::current_exe().and_then(std::fs::canonicalize)?;
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
        let a = flatpak_args(
            "org.example.App",
            None,
            &[PathBuf::from("/w/one"), PathBuf::from("/w/two")],
            false,
        );
        assert_eq!(&a[..2], ["run", "--sandbox"]);
        assert!(a.contains(&"--session-bus".to_string()));
        assert!(!a
            .iter()
            .any(|x| x == "--socket=session-bus" || x == "--socket=system-bus"));
        assert!(a.contains(&"--no-talk-name=org.freedesktop.Flatpak".to_string()));
        assert!(!a
            .iter()
            .any(|x| x.starts_with("--filesystem=home") || x.starts_with("--filesystem=host")));
        assert!(a.contains(&"--filesystem=/w/two".to_string()));
        assert_eq!(a.last().unwrap(), "org.example.App");
    }
}
