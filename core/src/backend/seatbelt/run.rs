//! Terminal runs on macOS: `paddock run HARNESS` under Seatbelt
//! (`sandbox-exec`).
//!
//! Seatbelt cannot move a host path to another place, the same as srt. So
//! the sandbox home is `HOME` at its own path, read-only imports are copied
//! into it, and the sockets and the pasture folders stay at their host paths
//! with variables that name them (`host_path_env`). The project and the
//! sandbox home are writable, the system is read-only, and everything else,
//! the real home too, is denied.
//!
//! Seatbelt only allows or denies network. In network modes egress and api
//! the profile allows connections to a few localhost ports only: a relay to
//! paddock's proxy (proxy.rs) and the host ports that the spec allows. The
//! proxy does the rest: the host proxy, the LAN rule and, in mode api, the
//! list of hosts.
//!
//! A spec feature that Seatbelt cannot give is refused (`refuse`), never left
//! out in silence. Most of this file is plain code, tested on every platform.

use super::{real, LINUX_ONLY, NAME, PROXY_KEYS, SANDBOX_EXEC};
use crate::backend::{base_env, host_path_env, Backend, Launch};
use crate::env::HostEnv;
use crate::seatbelt::{Access, Net, Policy};
use crate::spec::{GuiMode, MountKind, Network, SandboxSpec};
use anyhow::{bail, Context as _, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub struct SeatbeltRun;

/// Host requests that need a mount at a fixed path in the sandbox. The
/// broker refuses them for a Seatbelt run, with this reason.
pub const BROKER_REFUSALS: &[(&str, &str)] = &[
    (
        "borrow",
        "borrow is not there yet on macOS: a Seatbelt run cannot get the borrow folder at /run/paddock/borrow",
    ),
    (
        "return",
        "return is not there yet on macOS: a Seatbelt run has no borrow folder",
    ),
];

/// The host paths that the sandbox finds through a variable instead of a
/// mount (see `host_path_env`).
const BY_VARIABLE: &[&str] = &[
    crate::pasture::MOUNT,
    crate::pasture::workbench::MOUNT,
    crate::pasture::ports::PORTS_MOUNT,
    crate::pasture::ports::OWN_MOUNT,
];

/// The origin of the proxy relay port in `host_ports`.
pub const RELAY_ORIGIN: &str = "network: paddock proxy relay (seatbelt)";

/// `[macos] localhost_ports` of the user configuration: a range of localhost
/// ports ("3000-3999" or "8080") that a terminal run in network mode egress
/// can listen on and connect to. None by default. The project `paddock.toml`
/// cannot set it.
pub fn localhost_ports(env: &HostEnv) -> Result<Option<(u16, u16)>> {
    let file = env.paddock_config().join("config.toml");
    let Ok(text) = std::fs::read_to_string(&file) else {
        return Ok(None);
    };
    let doc: toml::Value =
        toml::from_str(&text).with_context(|| format!("{} is invalid", file.display()))?;
    match doc.get("macos").and_then(|m| m.get("localhost_ports")) {
        None => Ok(None),
        Some(toml::Value::String(s)) => parse_range(s).map(Some),
        Some(toml::Value::Integer(n)) => parse_range(&n.to_string()).map(Some),
        Some(_) => bail!("[macos] localhost_ports must be a port range such as \"3000-3999\""),
    }
}

/// "3000-3999" or "8080": the first and the last port.
pub fn parse_range(s: &str) -> Result<(u16, u16)> {
    let max = crate::seatbelt::MAX_RANGE;
    let bad = || {
        anyhow::anyhow!(
            "[macos] localhost_ports: {s:?} is not a range of 1 to {max} ports such as \"3000-3999\""
        )
    };
    let (a, b) = s.split_once('-').unwrap_or((s, s));
    let first: u16 = a.trim().parse().map_err(|_| bad())?;
    let last: u16 = b.trim().parse().map_err(|_| bad())?;
    if first == 0 || first > last || u32::from(last - first) >= max {
        return Err(bad());
    }
    Ok((first, last))
}

/// The line that `paddock run` prints at the start when a range is set.
pub fn range_line(range: (u16, u16), network: Network) -> String {
    let (a, b) = range;
    if network == Network::Egress {
        format!("localhost ports {a}-{b} are open in the sandbox ([macos] localhost_ports): it can listen on them and connect to them, also to the services of this Mac on these ports")
    } else {
        format!(
            "[macos] localhost_ports ({a}-{b}) is for network mode egress: this run in mode {} does not get it",
            format!("{network:?}").to_lowercase()
        )
    }
}

/// The private temporary folder of a run, in the sandbox home.
pub fn private_tmp(spec: &SandboxSpec) -> PathBuf {
    spec.home_src.join(".paddock-tmp").join(&spec.id)
}

/// The Seatbelt network of a network mode. Modes that Seatbelt cannot give
/// are refused.
pub fn net(mode: Network) -> Result<Net> {
    match mode {
        Network::None => Ok(Net::None),
        Network::Egress | Network::Api => Ok(Net::Localhost),
        Network::Host => bail!(
            "network mode 'host' is not there yet for Seatbelt runs on macOS: use egress (default), api or none"
        ),
        Network::Allowlist => bail!(
            "network mode 'allowlist' needs the srt backend, which is Linux only: on macOS use --network api with --allow"
        ),
    }
}

/// Spec features that Seatbelt cannot give. Each one stops the start with
/// one line, so nothing is left out in silence.
pub fn refuse(spec: &SandboxSpec) -> Result<()> {
    net(spec.network)?;
    if spec.shadow.is_some() {
        bail!("--profile default needs an overlay at the sandbox home, which Seatbelt cannot give: it runs on the bwrap backend only");
    }
    if spec.gui.mode != GuiMode::Off {
        bail!(
            "display mode '{}' is Linux only: a Seatbelt run on macOS has no display. Use --gui off, or paddock app run for a desktop app",
            format!("{:?}", spec.gui.mode).to_lowercase()
        );
    }
    if let Some(dir) = &spec.devices {
        bail!(
            "the devices of /dev/paddock ({}) are not there yet on macOS. Turn them off for this start ([devices] enabled = [])",
            dir.display()
        );
    }
    if spec
        .mounts
        .iter()
        .any(|m| m.dst == Path::new(crate::app::INBOUND_DIR))
    {
        bail!("links into the sandbox (schemes) and login_ports need a mount at a fixed path, which Seatbelt cannot give: they are Linux only");
    }
    if spec.keyring || spec.private_bus || spec.bus_socket.is_some() {
        bail!("the private keyring and the session bus are Linux only: turn them off for a Seatbelt run on macOS");
    }
    if !spec.publish.is_empty() && matches!(spec.network, Network::None | Network::Api) {
        bail!("ports cannot be published with network modes 'none' and 'api'");
    }
    if let Some(m) = spec.mounts.iter().find(|m| m.kind == MountKind::Overlay) {
        bail!(
            "{} needs an overlay mount ({}), which Seatbelt cannot give",
            m.dst.display(),
            m.origin
        );
    }
    Ok(())
}

/// The Seatbelt policy of a terminal run, and notes on what the run does
/// not get. Mounts that move a host path to another place (shared caches)
/// cannot be expressed: the sandbox home keeps its own copy instead.
pub fn policy(env: &HostEnv, spec: &SandboxSpec, tmp: &Path) -> Result<(Policy, Vec<String>)> {
    // Network mode api reaches the proxy only, as on Linux.
    let ports = spec
        .host_ports
        .iter()
        .filter(|(_, o)| spec.network != Network::Api || o == RELAY_ORIGIN)
        .map(|(p, _)| *p)
        .collect();
    let mut p = Policy::terminal(net(spec.network)?, ports);
    if spec.network == Network::Egress {
        p.localhost_range = localhost_ports(env)?;
    }
    if let Some(sock) = spec.env.get(crate::ptybroker::SOCKET_VAR) {
        p.add_file(
            real(Path::new(&sock.value)),
            Access::Connect,
            "paddock's pty broker: new ptys come from the host",
        );
    } else {
        // A hardened harness ignores the library: it makes its own ptys.
        p.own_ptys = pty_refusal(spec, &run_env(env, spec, tmp)).is_some();
    }
    if let Some(kc) = spec.env.get(super::keychain::VAR) {
        let cache = super::keychain::user_cache();
        super::keychain::allow(&mut p, Path::new(&kc.value), cache.as_deref());
    }
    let mut moved = Vec::new();
    p.add(real(&spec.project), Access::ReadWrite, "the project");
    p.add(
        real(&spec.home_src),
        Access::ReadWrite,
        "the sandbox home (HOME and CFFIXED_USER_HOME)",
    );
    p.add(
        real(tmp),
        Access::ReadWrite,
        "the private temporary folder (TMPDIR)",
    );
    p.add(
        real(&env.paddock_run().join(&spec.id)),
        Access::ReadOnly,
        "the run folder: paddock-host and the other host commands",
    );
    for (sock, why) in [
        (&spec.host_socket, "the host request broker (paddock-host)"),
        (&spec.kv_socket, "paddock kv"),
        (&spec.ssh_agent_socket, "the filtering ssh agent"),
    ] {
        if let Some(s) = sock {
            p.add_file(real(s), Access::Connect, why);
        }
    }
    for t in &spec.host_tools {
        p.add_file(
            real(&t.src),
            Access::ReadOnly,
            &format!("host command {}", t.name),
        );
    }
    for m in &spec.mounts {
        let why = format!("mount: {}", m.origin);
        let same = m.src.as_ref() == Some(&m.dst);
        let by_var = BY_VARIABLE.iter().any(|d| m.dst == Path::new(d));
        let src = m.src.as_deref().unwrap_or(&m.dst);
        match m.kind {
            MountKind::Rw if same || by_var => p.add(real(src), Access::ReadWrite, &why),
            MountKind::Ro if same || by_var => {
                if src.is_file() {
                    p.add_file(real(src), Access::ReadOnly, &why);
                } else {
                    p.add(real(src), Access::ReadOnly, &why);
                    // The port sockets of the other pasture members.
                    if by_var {
                        p.add(real(src), Access::Connect, &why);
                    }
                }
            }
            MountKind::MaskFile => p.add_file(real(&m.dst), Access::Deny, &why),
            // On Linux an empty folder covers the path. Seatbelt cannot make
            // it empty, so it denies it.
            MountKind::MaskDir | MountKind::Tmpfs => p.add(real(&m.dst), Access::Deny, &why),
            MountKind::Overlay => bail!(
                "{} needs an overlay mount ({}), which Seatbelt cannot give",
                m.dst.display(),
                m.origin
            ),
            // The home comes from home_src above. The borrow folder: the
            // broker refuses borrow and return (BROKER_REFUSALS).
            MountKind::Ro | MountKind::Rw
                if m.src.as_deref() == Some(spec.home_src.as_path())
                    || m.dst == Path::new(crate::borrow::MOUNT) => {}
            MountKind::Ro | MountKind::Rw => {
                moved.push(format!("{} ({})", m.dst.display(), m.origin))
            }
        }
    }
    let mut notes = Vec::new();
    if !moved.is_empty() {
        notes.push(format!(
            "not available under Seatbelt, the sandbox home keeps its own copy: {}",
            moved.join(", ")
        ));
    }
    Ok((p, notes))
}

/// `value` with a leading path in the host home moved to the same place in
/// the sandbox home. Paths that a mount allows at their own place stay.
fn into_home(spec: &SandboxSpec, kept: &[&Path], value: &str) -> String {
    let path = Path::new(value);
    if kept.iter().any(|k| path.starts_with(k)) {
        return value.to_string();
    }
    match path.strip_prefix(&spec.home) {
        Ok(rest) => spec.home_src.join(rest).display().to_string(),
        Err(_) => value.to_string(),
    }
}

/// The places that mounts allow at their own path.
fn kept(spec: &SandboxSpec) -> Vec<&Path> {
    spec.mounts
        .iter()
        .filter(|m| m.src.as_deref() == Some(m.dst.as_path()))
        .map(|m| m.dst.as_path())
        .collect()
}

/// The environment of a terminal run: the variables of every backend, with
/// paths in the host home moved into the sandbox home.
pub fn run_env(env: &HostEnv, spec: &SandboxSpec, tmp: &Path) -> BTreeMap<String, String> {
    let mut vars = base_env(env, spec);
    for k in LINUX_ONLY {
        vars.remove(*k);
    }
    let kept = kept(spec);
    for (k, v) in vars.iter_mut() {
        *v = if k == "PATH" {
            v.split(':')
                .map(|d| into_home(spec, &kept, d))
                .collect::<Vec<_>>()
                .join(":")
        } else {
            into_home(spec, &kept, v)
        };
    }
    let home = spec.home_src.display().to_string();
    vars.insert("HOME".into(), home.clone());
    // Moves NSHomeDirectory and the FileManager home too.
    vars.insert("CFFIXED_USER_HOME".into(), home);
    vars.insert("TMPDIR".into(), tmp.display().to_string());
    if spec.network == Network::None {
        for k in PROXY_KEYS {
            vars.remove(*k);
        }
    }
    host_path_env(spec, &mut vars);
    // Host paths, not moved into the sandbox home.
    for k in [
        crate::ptybroker::SOCKET_VAR,
        crate::ptybroker::LIBRARY_VAR,
        super::keychain::VAR,
    ] {
        if let Some(v) = spec.env.get(k) {
            vars.insert(k.into(), v.value.clone());
        }
    }
    // The host commands stay in their own folders. paddock's wrappers in the
    // run folder come first, other folders last, as for srt.
    let run = env.paddock_run().join(&spec.id);
    let dirs = |own: bool| {
        spec.host_tools
            .iter()
            .filter_map(|t| t.src.parent())
            .filter(|d| d.starts_with(&run) == own)
            .map(|d| d.display().to_string())
            .collect::<Vec<_>>()
    };
    let mut path = dirs(true);
    path.push(vars.get("PATH").cloned().unwrap_or_default());
    for d in dirs(false) {
        if !path.contains(&d) {
            path.push(d);
        }
    }
    vars.insert("PATH".into(), path.join(":"));
    vars
}

/// The command: `sandbox-exec` with the profile, then a shell that enters
/// the work folder, runs the setup lines and starts the spec's command.
///
/// macOS drops `DYLD_*` variables when it starts a protected program such
/// as `sandbox-exec` or `/bin/sh`. So the shell sets `DYLD_INSERT_LIBRARIES`
/// (paddock's pty library) itself, right before it starts the harness.
pub fn command(spec: &SandboxSpec, profile: &Path) -> Vec<String> {
    let kept = kept(spec);
    let mut script = "cd -- \"$1\" || exit 1\nshift\n".to_string();
    for line in &spec.setup {
        script.push_str(line);
        script.push('\n');
    }
    script.push_str(&format!(
        "[ -z \"${lib}\" ] || export DYLD_INSERT_LIBRARIES=\"${lib}\"\nexec \"$@\"",
        lib = crate::ptybroker::LIBRARY_VAR
    ));
    let mut a = vec![
        "-f".to_string(),
        profile.display().to_string(),
        "/bin/sh".into(),
        "-c".into(),
        script,
        "paddock".into(),
        into_home(spec, &kept, &spec.workdir.display().to_string()),
    ];
    a.extend(spec.command.iter().map(|c| into_home(spec, &kept, c)));
    a
}

/// The harness program: the first word of the command, found in the PATH
/// of the run.
pub fn harness_program(spec: &SandboxSpec, vars: &BTreeMap<String, String>) -> Option<PathBuf> {
    let first = spec.command.first()?;
    if first.contains('/') {
        return Some(PathBuf::from(first));
    }
    vars.get("PATH")?
        .split(':')
        .map(|d| Path::new(d).join(first))
        .find(|p| p.is_file())
}

/// Why the harness of `spec` cannot get ptys from paddock, or None when it
/// can. The backend choice on macOS asks it before the run starts.
pub fn pty_refusal_for(env: &HostEnv, spec: &SandboxSpec) -> Option<String> {
    pty_refusal(spec, &run_env(env, spec, &private_tmp(spec)))
}

/// Why the harness cannot get ptys from paddock, or None when it can.
fn pty_refusal(spec: &SandboxSpec, vars: &BTreeMap<String, String>) -> Option<String> {
    if crate::ptybroker::LIBRARY.is_empty() {
        return Some(
            "this paddock was built without its pty library (build.rs found no C compiler)".into(),
        );
    }
    let Some(bin) = harness_program(spec, vars) else {
        return Some(format!(
            "cannot find the program {:?}",
            spec.command.first()
        ));
    };
    #[cfg(target_os = "macos")]
    {
        crate::ptybroker::check(&bin)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = bin;
        None
    }
}

/// Start paddock's pty broker for the run, and give the harness the
/// library that asks it. A harness that would ignore the library gets one
/// line, the profile's `when = "no_pty"` settings, and the App Sandbox
/// rules for its own ptys (`policy`). True when the harness gets ptys from paddock.
pub fn start_pty(env: &HostEnv, spec: &mut SandboxSpec) -> Result<bool> {
    let vars = run_env(env, spec, &private_tmp(spec));
    if let Some(why) = pty_refusal(spec, &vars) {
        let fix = if spec.home_settings.iter().any(|s| s.no_pty) {
            " paddock sets the profile's settings for a harness without ptys."
        } else {
            ""
        };
        eprintln!(
            "paddock: no pty from paddock for {}: {why}. It can make its own ptys, but cannot open other terminals.{fix}",
            spec.harness
        );
        return Ok(false);
    }
    let run = env.paddock_run().join(&spec.id);
    crate::privfs::dir(&run)?;
    let lib = run.join(crate::ptybroker::LIBRARY_FILE);
    std::fs::write(&lib, crate::ptybroker::LIBRARY)
        .with_context(|| format!("cannot write {}", lib.display()))?;
    std::fs::set_permissions(&lib, std::os::unix::fs::PermissionsExt::from_mode(0o644))?;
    let sock = crate::sockpath::named(env, &spec.id, "pty.sock")?;
    crate::ptybroker::serve(&sock)?;
    let origin = "seatbelt: paddock's pty broker";
    spec.set_env(
        crate::ptybroker::SOCKET_VAR,
        sock.display().to_string(),
        origin,
    );
    spec.set_env(
        crate::ptybroker::LIBRARY_VAR,
        lib.display().to_string(),
        origin,
    );
    Ok(true)
}

/// paddock's proxy for network modes egress and api, and a relay on a free
/// localhost port. The port goes into `host_ports`, which the profile
/// allows, and into the proxy variables.
pub fn start_proxy(env: &HostEnv, spec: &mut SandboxSpec) -> Result<()> {
    if !matches!(spec.network, Network::Egress | Network::Api) {
        return Ok(());
    }
    let sock = crate::sockpath::named(env, &spec.id, "proxy.sock")?;
    let mut cfg = crate::backend::host_proxy_cfg(env, spec);
    if spec.network == Network::Api {
        let (list, _) = crate::proxy::allow::Allowlist::parse(&spec.allow_domains);
        // The api proxy reaches no host loopback port, as on Linux.
        cfg.allow_loopback = Vec::new();
        cfg.api = Some(crate::proxy::allow::ApiPolicy {
            list,
            refusals: Some(crate::backend::api_refusals(env, spec)),
        });
    }
    crate::proxy::serve(&sock, cfg)?;
    let port = super::tcp_relay(&sock)?;
    set_proxy(spec, port);
    Ok(())
}

/// Point the proxy variables at the relay on `port` and allow the port.
pub fn set_proxy(spec: &mut SandboxSpec, port: u16) {
    let origin = RELAY_ORIGIN;
    spec.add_host_port(port, origin);
    let url = format!("http://127.0.0.1:{port}");
    for k in PROXY_KEYS {
        spec.set_env(k, url.clone(), origin);
    }
    for k in ["NO_PROXY", "no_proxy"] {
        spec.set_env(k, "127.0.0.1,localhost,::1", origin);
    }
}

impl Backend for SeatbeltRun {
    fn name(&self) -> &'static str {
        NAME
    }

    fn check(&self, _env: &HostEnv, spec: &SandboxSpec) -> Result<()> {
        if !cfg!(target_os = "macos") {
            bail!("the seatbelt backend runs on macOS only: use bwrap, docker, podman or srt on Linux");
        }
        if !Path::new(SANDBOX_EXEC).is_file() {
            bail!("{SANDBOX_EXEC} is missing on this Mac");
        }
        refuse(spec)
    }

    /// Copy read-only imports into the sandbox home, the same way as for
    /// srt, make the private temporary folder, and tell what the run does
    /// not get.
    fn prepare(&self, env: &HostEnv, spec: &SandboxSpec, _interactive: bool) -> Result<()> {
        refuse(spec)?;
        Backend::prepare(&super::super::srt::Srt, env, spec, false)?;
        let tmp = private_tmp(spec);
        crate::safefs::Root::open(&spec.home_src)?.dir(tmp.strip_prefix(&spec.home_src)?, true)?;
        let (_, notes) = policy(env, spec, &tmp)?;
        for n in notes {
            log::warn!("{n}");
        }
        log::info!("the real home is denied, not absent: names stay visible");
        if let Some(r) = localhost_ports(env)? {
            log::warn!("{}", range_line(r, spec.network));
        }
        if spec.identity.machine_id.is_some() {
            log::warn!("an isolated login gets its own machine ID on Linux only: on macOS the sandbox can read the hardware UUID of this Mac");
        }
        Ok(())
    }

    fn render(&self, env: &HostEnv, spec: &SandboxSpec, _interactive: bool) -> Result<Launch> {
        refuse(spec)?;
        let tmp = private_tmp(spec);
        // In the sandbox home: a planted .paddock-tmp link must not make
        // paddock create a folder elsewhere and allow writes to it.
        crate::safefs::Root::open(&spec.home_src)?.dir(tmp.strip_prefix(&spec.home_src)?, true)?;
        let (pol, _) = policy(env, spec, &tmp)?;
        let profile = env.paddock_run().join(&spec.id).join("seatbelt.sb");
        Ok(Launch {
            program: PathBuf::from(SANDBOX_EXEC),
            args: command(spec, &profile),
            env: run_env(env, spec, &tmp),
            files: vec![(profile, crate::seatbelt::render(&pol)?, 0o600)],
        })
    }
}

#[cfg(test)]
#[path = "run_tests.rs"]
mod tests;

/// Terminal runs with `sandbox-exec` and a fake harness. The maintainer
/// runs them on a Mac.
#[cfg(all(test, target_os = "macos"))]
#[path = "run_macos_tests.rs"]
mod macos_tests;
