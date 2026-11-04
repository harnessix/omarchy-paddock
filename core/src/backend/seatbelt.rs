//! App mode on macOS: the app bundle runs under Seatbelt (`sandbox-exec`).
//!
//! paddock finds the bundle and its binary, refuses apps that use Apple's
//! App Sandbox (they crash inside `sandbox-exec`), and starts the binary
//! itself: `open -a` hands the start to launchd, outside the sandbox. The
//! app gets the private sandbox home as `HOME` and `CFFIXED_USER_HOME`, so
//! `NSHomeDirectory` and Application Support move there too. Seatbelt only
//! allows or denies network, so the profile allows localhost and the app
//! reaches the internet through paddock's host proxy (proxy.rs).
//!
//! Electron apps start their own Chromium sandbox, which cannot nest inside
//! Seatbelt: they get `--no-sandbox`. They also get their own data folder
//! (`--user-data-dir`), so a copy of the app that already runs outside the
//! sandbox does not take over the start.
//!
//! Most of this file is plain code, tested on every platform. The start
//! itself (`run_app`) and the signature check are macOS only.

use crate::env::HostEnv;
use crate::profile::{MacApp, Profile};
use crate::seatbelt::{Access, Net, Policy};
use crate::spec::{MountKind, Network, SandboxSpec};
use anyhow::{bail, Context as _, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Terminal runs (`paddock run`) under Seatbelt.
pub mod keychain;
pub mod run;

/// The backend name in the run record and in `paddock ps`.
pub const NAME: &str = "seatbelt";

pub const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// Why an App-Sandboxed app is refused.
pub fn app_sandbox_message(name: &str) -> String {
    format!(
        "{name} uses Apple's App Sandbox, which cannot run inside paddock's Seatbelt sandbox (it stops with forbidden-sandbox-reinit). Apple already confines it: start it as usual, outside paddock"
    )
}

/// The first bundle of the profile that exists.
pub fn find_bundle(env: &HostEnv, name: &str, mac: &MacApp) -> Result<PathBuf> {
    for b in &mac.bundles {
        let p = env.expand(b, &env.home);
        if p.join("Contents").is_dir() {
            return Ok(p);
        }
    }
    if mac.bundles.is_empty() {
        bail!("the {name} profile names no app bundle for macOS ([app.macos] bundles)");
    }
    bail!(
        "{name} is not installed: no app bundle at {}",
        mac.bundles.join(", ")
    )
}

/// The binary inside a bundle: `CFBundleExecutable` of `Contents/Info.plist`,
/// else the only file in `Contents/MacOS`.
pub fn bundle_exe(bundle: &Path) -> Result<PathBuf> {
    let macos = bundle.join("Contents/MacOS");
    let info = bundle.join("Contents/Info.plist");
    let name = std::fs::read(&info)
        .ok()
        .and_then(|b| plist_xml(&b))
        .and_then(|x| plist_string(&x, "CFBundleExecutable"));
    if let Some(n) = name.filter(|n| !n.contains('/') && n != ".." && !n.is_empty()) {
        let p = macos.join(n);
        if p.is_file() {
            return Ok(p);
        }
    }
    let files: Vec<PathBuf> = std::fs::read_dir(&macos)
        .with_context(|| format!("{} is not an app bundle", bundle.display()))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    match files.as_slice() {
        [one] => Ok(one.clone()),
        _ => bail!(
            "cannot tell the binary of {}: Info.plist names none and Contents/MacOS has {} files",
            bundle.display(),
            files.len()
        ),
    }
}

/// An Electron app has the Electron framework in its bundle.
pub fn is_electron(bundle: &Path) -> bool {
    bundle
        .join("Contents/Frameworks/Electron Framework.framework")
        .is_dir()
}

/// A property list as XML text. A binary plist is converted with `plutil`
/// on macOS; elsewhere it gives None.
pub fn plist_xml(bytes: &[u8]) -> Option<String> {
    if bytes.starts_with(b"bplist") {
        return binary_plist_xml(bytes);
    }
    let s = String::from_utf8_lossy(bytes);
    // codesign puts a blob header before the XML.
    let start = s.find("<?xml").or_else(|| s.find("<plist"))?;
    Some(s[start..].to_string())
}

#[cfg(target_os = "macos")]
fn binary_plist_xml(bytes: &[u8]) -> Option<String> {
    use std::io::Write as _;
    let mut c = std::process::Command::new("/usr/bin/plutil")
        .args(["-convert", "xml1", "-o", "-", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    c.stdin.take()?.write_all(bytes).ok()?;
    let out = c.wait_with_output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(not(target_os = "macos"))]
fn binary_plist_xml(_bytes: &[u8]) -> Option<String> {
    None
}

/// The text right after `<key>KEY</key>`, with spaces trimmed.
fn after_key<'a>(xml: &'a str, key: &str) -> Option<&'a str> {
    let tag = format!("<key>{}</key>", xml_escape(key));
    let i = xml.find(&tag)?;
    Some(xml[i + tag.len()..].trim_start())
}

/// A boolean value of a top-level key.
pub fn plist_bool(xml: &str, key: &str) -> Option<bool> {
    let rest = after_key(xml, key)?;
    let compact: String = rest
        .chars()
        .take(64)
        .filter(|c| !c.is_whitespace())
        .collect();
    // codesign prints either <true/> or <true></true>, depending on the tool.
    if compact.starts_with("<true/>") || compact.starts_with("<true></true>") {
        Some(true)
    } else if compact.starts_with("<false/>") || compact.starts_with("<false></false>") {
        Some(false)
    } else {
        None
    }
}

/// A string value of a top-level key.
pub fn plist_string(xml: &str, key: &str) -> Option<String> {
    let rest = after_key(xml, key)?.strip_prefix("<string>")?;
    let end = rest.find("</string>")?;
    Some(xml_unescape(&rest[..end]))
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// The entitlement of Apple's App Sandbox.
pub const APP_SANDBOX_KEY: &str = "com.apple.security.app-sandbox";

/// Whether entitlements (the XML that `codesign -d --entitlements :-` prints)
/// turn on the App Sandbox.
pub fn app_sandboxed(entitlements: &str) -> bool {
    plist_xml(entitlements.as_bytes())
        .and_then(|x| plist_bool(&x, APP_SANDBOX_KEY))
        .unwrap_or(false)
}

/// The entitlements of a signed binary, from its code signature.
#[cfg(target_os = "macos")]
pub fn entitlements(exe: &Path) -> Result<String> {
    let out = std::process::Command::new("/usr/bin/codesign")
        .args(["-d", "--entitlements", ":-"])
        .arg(exe)
        .stderr(std::process::Stdio::null())
        .output()
        .context("cannot run codesign")?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The process that holds Chromium's single-instance lock of a data folder:
/// `SingletonLock` is a symlink to `HOSTNAME-PID`. None when no lock exists
/// or its process is gone.
pub fn singleton_owner(data_dir: &Path) -> Option<u32> {
    let target = std::fs::read_link(data_dir.join("SingletonLock")).ok()?;
    let t = target.to_string_lossy();
    let pid: u32 = t.rsplit_once('-')?.1.parse().ok()?;
    (pid > 0 && crate::state::alive(pid)).then_some(pid)
}

/// The app's data folder in the private home.
pub fn data_dir(spec: &SandboxSpec, profile: &Profile, mac: &MacApp) -> PathBuf {
    let rel = mac.user_data.clone().unwrap_or_else(|| {
        let n = profile
            .app
            .as_ref()
            .map(|a| a.display_name.clone())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| profile.name.clone());
        format!("Library/Application Support/{n}")
    });
    spec.home_src.join(rel)
}

/// A short path to the data folder, for `--user-data-dir`. Electron apps
/// put Unix sockets in that folder (VS Code: `<version>-main.sock`), and
/// the folder in the private home is too long for the 104 byte limit. The
/// link lives in the run's socket folder and goes when the run ends.
pub fn short_data_link(env: &HostEnv, spec: &SandboxSpec, data: &Path) -> Result<PathBuf> {
    let link = crate::sockpath::host_socket(&env.paddock_run(), env.uid, &spec.id, "ud")?;
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(data, &link)
        .with_context(|| format!("cannot link {} to {}", link.display(), data.display()))?;
    Ok(link)
}

/// The arguments for an Electron app, after its binary.
pub fn electron_args(data_dir: &Path, proxy: Option<u16>) -> Vec<String> {
    let mut v = vec![
        // Chromium's own sandbox cannot start inside Seatbelt.
        "--no-sandbox".to_string(),
        // Logins stay in the private home, never in the user's Keychain.
        "--use-mock-keychain".to_string(),
        format!("--user-data-dir={}", data_dir.display()),
    ];
    if let Some(port) = proxy {
        v.push(format!("--proxy-server=http://127.0.0.1:{port}"));
    }
    v
}

/// The real path of `p`, also for a path that does not exist yet (its
/// nearest existing ancestor is resolved). Seatbelt matches real paths.
pub fn real(p: &Path) -> PathBuf {
    if let Ok(r) = std::fs::canonicalize(p) {
        return r;
    }
    match (p.parent(), p.file_name()) {
        (Some(parent), Some(name)) if parent != p => real(parent).join(name),
        _ => p.to_path_buf(),
    }
}

/// The Seatbelt policy for an app spec: the project and the private home
/// writable, the bundle read-only, mounts at their own paths as they are,
/// hidden files denied. Mounts that move a host path to another place
/// cannot be expressed; they are left out (the app does not get them) and
/// listed in the returned notes.
pub fn policy(
    spec: &SandboxSpec,
    bundle: &Path,
    tmp: &Path,
    net: Net,
) -> Result<(Policy, Vec<String>)> {
    let mut p = Policy::new(net);
    let mut notes = Vec::new();
    p.add(real(&spec.project), Access::ReadWrite, "the project");
    p.add(
        real(&spec.home_src),
        Access::ReadWrite,
        "the private sandbox home (HOME and CFFIXED_USER_HOME)",
    );
    p.add(
        real(tmp),
        Access::ReadWrite,
        "the private temporary folder (TMPDIR)",
    );
    p.add(real(bundle), Access::ReadOnly, "the app bundle");
    for m in &spec.mounts {
        let same = m.src.as_ref() == Some(&m.dst);
        let why = format!("mount: {}", m.origin);
        match m.kind {
            MountKind::Rw if same => p.add(real(&m.dst), Access::ReadWrite, &why),
            MountKind::Ro if same => {
                if m.dst.is_file() {
                    p.add_file(real(&m.dst), Access::ReadOnly, &why);
                } else {
                    p.add(real(&m.dst), Access::ReadOnly, &why);
                }
            }
            MountKind::MaskFile => p.add_file(real(&m.dst), Access::Deny, &why),
            MountKind::MaskDir => p.add(real(&m.dst), Access::Deny, &why),
            // On Linux an empty folder covers the path. Seatbelt cannot make
            // it empty, so it denies it.
            MountKind::Tmpfs => p.add(real(&m.dst), Access::Deny, &why),
            MountKind::Overlay => bail!(
                "{} needs an overlay mount ({}), which Seatbelt cannot express",
                m.dst.display(),
                m.origin
            ),
            // The home itself comes from home_src above.
            MountKind::Ro | MountKind::Rw if m.src.as_deref() == Some(spec.home_src.as_path()) => {}
            MountKind::Ro | MountKind::Rw => notes.push(format!(
                "not available under Seatbelt: {} ({})",
                m.dst.display(),
                m.origin
            )),
        }
    }
    Ok((p, notes))
}

/// Variables that belong to the Linux display, bus and keyring.
pub(crate) const LINUX_ONLY: &[&str] = &[
    "XDG_RUNTIME_DIR",
    "WAYLAND_DISPLAY",
    "XDG_SESSION_TYPE",
    "GDK_BACKEND",
    "QT_QPA_PLATFORM",
    "ELECTRON_OZONE_PLATFORM_HINT",
    "WEBKIT_DISABLE_DMABUF_RENDERER",
    "LIBGL_ALWAYS_SOFTWARE",
    "DISPLAY",
    "DBUS_SESSION_BUS_ADDRESS",
    "PADDOCK_KEYRING_PW",
    "PADDOCK_INBOUND_CMD",
    "PADDOCK_INBOUND_SCHEMES",
    "PADDOCK_INBOUND_PORTS",
];

/// The proxy variables, upper and lower case.
pub(crate) const PROXY_KEYS: &[&str] = &[
    "HTTPS_PROXY",
    "https_proxy",
    "HTTP_PROXY",
    "http_proxy",
    "ALL_PROXY",
    "all_proxy",
];

/// The environment of the app.
pub fn app_env(
    env: &HostEnv,
    spec: &SandboxSpec,
    tmp: &Path,
    proxy: Option<u16>,
) -> BTreeMap<String, String> {
    let mut vars = super::base_env(env, spec);
    for k in LINUX_ONLY {
        vars.remove(*k);
    }
    // Paths into the host home move to the same place in the private home,
    // unless a mount allows them at their own path.
    let kept: Vec<&Path> = spec
        .mounts
        .iter()
        .filter(|m| m.src.as_deref() == Some(m.dst.as_path()))
        .map(|m| m.dst.as_path())
        .collect();
    for v in vars.values_mut() {
        let path = Path::new(v.as_str());
        if kept.iter().any(|k| path.starts_with(k)) {
            continue;
        }
        if let Ok(rest) = path.strip_prefix(&env.home) {
            *v = spec.home_src.join(rest).display().to_string();
        }
    }
    let home = spec.home_src.display().to_string();
    vars.insert("HOME".into(), home.clone());
    // Moves NSHomeDirectory, the FileManager home and Application Support.
    vars.insert("CFFIXED_USER_HOME".into(), home);
    vars.insert("TMPDIR".into(), tmp.display().to_string());
    vars.insert("PATH".into(), "/usr/bin:/bin:/usr/sbin:/sbin".into());
    for k in PROXY_KEYS {
        vars.remove(*k);
    }
    vars.remove("NO_PROXY");
    vars.remove("no_proxy");
    if let Some(port) = proxy {
        let url = format!("http://127.0.0.1:{port}");
        for k in PROXY_KEYS {
            vars.insert((*k).into(), url.clone());
        }
        for k in ["NO_PROXY", "no_proxy"] {
            vars.insert(k.into(), "localhost,127.0.0.1,::1".into());
        }
    }
    vars
}

/// The network of the Seatbelt profile for a network mode. Modes that
/// Seatbelt cannot give are refused.
pub fn net_for(mode: Network) -> Result<Net> {
    match mode {
        Network::None => Ok(Net::None),
        Network::Egress | Network::Api => Ok(Net::Localhost),
        Network::Host => bail!(
            "network mode 'host' is not there yet on macOS: use egress (default), api or none"
        ),
        Network::Allowlist => bail!(
            "network mode 'allowlist' needs the srt backend, which app mode on macOS does not use: use api with --allow instead"
        ),
    }
}

/// Settings of the Linux app mode that macOS does not have yet. paddock
/// refuses them instead of running without them.
pub fn refuse_linux_only(
    name: &str,
    backend: Option<&str>,
    user: Option<&crate::manifest::AppCfg>,
    devices: &[String],
) -> Result<()> {
    if let Some(b) = backend.filter(|b| *b != NAME) {
        bail!("app mode on macOS runs with Seatbelt only: the {b} backend is Linux only");
    }
    if let Some(a) = user {
        if a.bus.is_some() {
            bail!(
                "[apps.{name}] bus is a Linux setting: the private D-Bus and the filtered host bus are not there yet on macOS. Remove it to run {name} on macOS"
            );
        }
        if a.login_ports {
            bail!("[apps.{name}] login_ports is not there yet on macOS");
        }
    }
    if !devices.is_empty() {
        bail!(
            "the devices of /dev/paddock ({}) are not there yet on macOS. Turn them off for this start ([devices] enabled = [])",
            devices.join(", ")
        );
    }
    Ok(())
}

/// The command: the bundle binary, its macOS arguments, the Electron flags,
/// then the workspace folders and links.
pub fn command(
    exe: &Path,
    mac: &MacApp,
    electron: Option<Vec<String>>,
    app_args: &[String],
) -> Vec<String> {
    let mut c = vec![exe.display().to_string()];
    c.extend(mac.args.iter().cloned());
    for a in electron.unwrap_or_default() {
        if !c.contains(&a) {
            c.push(a);
        }
    }
    c.extend(app_args.iter().cloned());
    c
}

/// A TCP port on 127.0.0.1 that passes each connection to paddock's proxy
/// on a Unix socket. Seatbelt cannot let the app reach a socket outside its
/// folders, and Electron's `--proxy-server` needs TCP.
pub fn tcp_relay(socket: &Path) -> Result<u16> {
    use std::net::TcpListener;
    use std::os::unix::net::UnixStream;
    let l = TcpListener::bind(("127.0.0.1", 0)).context("cannot listen on 127.0.0.1")?;
    let port = l.local_addr()?.port();
    let socket = socket.to_path_buf();
    std::thread::spawn(move || {
        for conn in l.incoming().flatten() {
            let socket = socket.clone();
            std::thread::spawn(move || {
                let Ok(up) = UnixStream::connect(&socket) else {
                    return;
                };
                let (Ok(mut c2), Ok(mut u2)) = (conn.try_clone(), up.try_clone()) else {
                    return;
                };
                let t = std::thread::spawn(move || {
                    let _ = std::io::copy(&mut c2, &mut u2);
                    let _ = u2.shutdown(std::net::Shutdown::Write);
                });
                let (mut c, mut u) = (conn, up);
                let _ = std::io::copy(&mut u, &mut c);
                let _ = c.shutdown(std::net::Shutdown::Write);
                let _ = t.join();
            });
        }
    });
    Ok(port)
}

/// `paddock app run NAME` on macOS.
#[cfg(target_os = "macos")]
pub fn run_app(env: &HostEnv, o: &crate::app::AppOptions) -> Result<i32> {
    use crate::build::{self, Options};
    use crate::manifest::Manifest;
    use crate::spec::GuiMode;
    use std::process::Command;

    let mut profile = crate::app::app_profile(env, &o.name)?;
    let app = profile.app.clone().unwrap_or_default();
    if app.flatpak.is_some() {
        bail!("{} is a flatpak app: flatpak is Linux only", o.name);
    }
    let mac = app.macos.clone().with_context(|| {
        format!(
            "{} has no macOS location ([app.macos] in its profile)",
            o.name
        )
    })?;
    if mac.app_sandbox {
        bail!(app_sandbox_message(&o.name));
    }
    let user = Manifest::load(env, &env.home.join(".paddock-no-project"))?.merged;
    let devices = crate::devices::DevicesCfg::load(env)
        .map(|c| crate::devices::enabled_names(&c, false, None))
        .unwrap_or_default();
    refuse_linux_only(
        &o.name,
        o.backend.as_deref(),
        user.apps.get(&o.name),
        &devices,
    )?;

    let bundle = find_bundle(env, &o.name, &mac)?;
    let exe = bundle_exe(&bundle)?;
    if app_sandboxed(&entitlements(&exe)?) {
        bail!(app_sandbox_message(&o.name));
    }
    let electron = is_electron(&bundle);

    let roots = crate::app::roots(env, &o.name, &o.roots)?;
    let ucfg = user.apps.get(&o.name);
    let worktree = o.worktree || ucfg.and_then(|a| a.worktree).unwrap_or(false);
    profile.launch.args = mac.args.clone();
    let harness = crate::profile::Harness {
        profile: profile.clone(),
        exe: exe.clone(),
        install_mounts: vec![bundle.clone()],
        path_dirs: vec![],
        source: "builtin".into(),
    };
    let opts = Options {
        harness: o.name.clone(),
        project: Some(roots[0].clone()),
        backend: Some(NAME.into()),
        network: o.network,
        gui: Some(GuiMode::App),
        gpu: o.gpu,
        no_yolo: true,
        inherit_env: o.inherit_env,
        auth: o.auth.clone(),
        worktree,
        worktree_app: Some(o.name.clone()),
        app_bus: Some(crate::manifest::AppBus::Private),
        ..Default::default()
    };
    let built = build::build_with(env, &opts, harness)?;
    crate::policy::enforce(env, &built.spec)?;
    let spec = &built.spec;
    let net = net_for(spec.network)?;

    // One instance per data folder: a running copy would take over the start.
    let data = data_dir(spec, &profile, &mac);
    if let Some(pid) = singleton_owner(&data) {
        bail!(
            "{} already runs with the data folder {} (process {pid}). Close it first: a second start would hand over to it",
            o.name,
            data.display()
        );
    }
    if let Some(r) = crate::state::Running::list(env)
        .into_iter()
        .find(|r| r.harness == o.name && r.project == spec.project)
    {
        bail!(
            "{} already runs in a sandbox for {} ({}). Stop it first: paddock stop {}",
            o.name,
            spec.project.display(),
            r.id,
            r.id
        );
    }

    // Read-only imports go into the private home, the same way as for srt.
    super::Backend::prepare(&super::srt::Srt, env, spec, false)?;
    let tmp_rel = Path::new(".paddock-tmp").join(&spec.id);
    crate::safefs::Root::open(&spec.home_src)?.dir(&tmp_rel, true)?;
    let tmp = spec.home_src.join(&tmp_rel);
    crate::safefs::Root::open(&spec.home_src)?.dir(
        data.strip_prefix(&spec.home_src)
            .unwrap_or(Path::new("Library")),
        true,
    )?;
    let run_dir = env.paddock_run().join(&spec.id);
    crate::privfs::dir(&run_dir)?;

    let proxy = if net == Net::Localhost {
        let sock =
            crate::sockpath::host_socket(&env.paddock_run(), env.uid, &spec.id, "proxy.sock")?;
        let mut cfg = crate::proxy::HostProxy::from_vars(|k| env.var(k).map(str::to_string));
        cfg.allow_loopback = spec.host_ports.iter().map(|(p, _)| *p).collect();
        cfg.allow_lan = spec.lan;
        cfg.boundary = Some(crate::boundary::proxy_sink(env, &spec.id));
        cfg.egress = crate::monitor::egress::sink(env, &spec.id, &spec.harness, &spec.project);
        if spec.network == Network::Api {
            let (list, _) = crate::proxy::allow::Allowlist::parse(&spec.allow_domains);
            cfg.api = Some(crate::proxy::allow::ApiPolicy {
                list,
                refusals: None,
            });
        }
        for w in cfg.cleartext_warnings() {
            log::warn!("{w}");
        }
        crate::proxy::serve(&sock, cfg)?;
        Some(tcp_relay(&sock)?)
    } else {
        None
    };

    let (pol, notes) = policy(spec, &bundle, &tmp, net)?;
    let sb = run_dir.join("seatbelt.sb");
    crate::privfs::write(&sb, crate::seatbelt::render(&pol)?)?;

    let mut open = vec![spec.project.clone()];
    open.extend(roots[1..].iter().cloned());
    let args = crate::app::app_args(&profile, &open, &o.args);
    let ud = if electron {
        short_data_link(env, spec, &data)?
    } else {
        data.clone()
    };
    let cmd = command(
        &exe,
        &mac,
        electron.then(|| electron_args(&ud, proxy)),
        &args,
    );
    let vars = app_env(env, spec, &tmp, proxy);
    for w in crate::privacy::write_settings(&spec.home_src, &spec.home_settings)? {
        log::warn!("{w}");
    }

    eprintln!(
        "paddock: {} in {} (backend {NAME}, network {})",
        o.name,
        spec.project.display(),
        format!("{:?}", spec.network).to_lowercase()
    );
    eprintln!("paddock: the real home is denied, not absent: names stay visible");
    for n in notes.iter().chain(&spec.notices) {
        eprintln!("paddock: {n}");
    }

    let mut child = Command::new(SANDBOX_EXEC)
        .arg("-f")
        .arg(&sb)
        .args(&cmd)
        .env_clear()
        .envs(&vars)
        .current_dir(&spec.project)
        .spawn()
        .with_context(|| format!("cannot start {SANDBOX_EXEC}"))?;
    let running: crate::state::Running = serde_json::from_value(serde_json::json!({
        "id": spec.id,
        "harness": spec.harness,
        "project": spec.project,
        "backend": NAME,
        "network": format!("{:?}", spec.network).to_lowercase(),
        "gui": "app",
        "pid": child.id(),
        "started": crate::state::now(),
        "auth": built.auth.to_string(),
        "host_pid": std::process::id(),
    }))?;
    if let Err(e) = running.register(env) {
        log::warn!("cannot register the running sandbox: {e:#}");
    }
    crate::mark::spawn(env);
    let status = child.wait()?;
    let _ = std::fs::remove_dir_all(&run_dir);
    crate::sockpath::remove_run_sockets(&env.paddock_run(), env.uid, &spec.id);
    let _ = std::fs::remove_dir_all(&tmp);
    if let Some(w) = &built.worktree {
        eprintln!("\n{}", build::worktree_report(env, w));
    }
    Ok(status.code().unwrap_or(1))
}

#[cfg(test)]
#[path = "seatbelt_tests.rs"]
mod tests;

/// Tests that run `sandbox-exec`. The maintainer runs them on a Mac.
#[cfg(all(test, target_os = "macos"))]
#[path = "seatbelt_macos_tests.rs"]
mod macos_tests;
