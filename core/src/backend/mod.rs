//! Backends render a `SandboxSpec` into a command. The second factory: one
//! spec, several isolation technologies.

pub mod bwrap;
pub mod docker;
pub mod seatbelt;
pub mod srt;
pub mod vm;

use crate::env::HostEnv;
use crate::spec::{Network, SandboxSpec};
use anyhow::{bail, Result};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// A rendered launch: program, arguments, and the exact environment to give it.
#[derive(Debug, Clone)]
pub struct Launch {
    pub program: PathBuf,
    pub args: Vec<String>,
    /// The process environment. It is cleared first, so nothing else leaks.
    pub env: BTreeMap<String, String>,
    /// Files to write before the launch (path, contents, mode).
    pub files: Vec<(PathBuf, String, u32)>,
}

pub trait Backend {
    fn name(&self) -> &'static str;
    /// `Ok(())` when the backend can run the spec on this host, else the reason.
    fn check(&self, env: &HostEnv, spec: &SandboxSpec) -> Result<()>;
    /// Work that must happen before `render`, such as building an image.
    fn prepare(&self, _env: &HostEnv, _spec: &SandboxSpec, _interactive: bool) -> Result<()> {
        Ok(())
    }
    fn render(&self, env: &HostEnv, spec: &SandboxSpec, interactive: bool) -> Result<Launch>;
}

pub fn by_name(name: &str) -> Result<Box<dyn Backend>> {
    match name {
        "bwrap" => Ok(Box::new(bwrap::Bwrap)),
        "docker" => Ok(Box::new(docker::Docker::new("docker"))),
        "podman" => Ok(Box::new(docker::Docker::new("podman"))),
        "srt" => Ok(Box::new(srt::Srt)),
        seatbelt::NAME => Ok(Box::new(seatbelt::run::SeatbeltRun)),
        vm::NAME => Ok(Box::new(vm::Vm)),
        _ => bail!(
            "unknown backend '{name}': use auto, bwrap, docker, podman, srt, seatbelt or vm (macOS)"
        ),
    }
}

/// Pick the first backend that can run the spec.
pub fn select(env: &HostEnv, spec: &SandboxSpec, wanted: &str) -> Result<Box<dyn Backend>> {
    if wanted != "auto" {
        let b = by_name(wanted)?;
        b.check(env, spec)?;
        return Ok(b);
    }
    let mut reasons = Vec::new();
    // Only srt filters by domain, so allowlist mode picks it. macOS has no
    // bubblewrap: Seatbelt comes first there.
    let order: &[&str] = if spec.network == Network::Allowlist {
        &["srt"]
    } else if cfg!(target_os = "macos") {
        &[seatbelt::NAME, "podman", "docker"]
    } else {
        &["bwrap", "podman", "docker"]
    };
    for &name in order {
        let b = by_name(name)?;
        match b.check(env, spec) {
            Ok(()) => return Ok(b),
            Err(e) => reasons.push(format!("{name}: {e}")),
        }
    }
    bail!(
        "no backend can run this sandbox:\n  {}",
        reasons.join("\n  ")
    )
}

/// The environment variables every sandbox gets, before profile and manifest.
pub fn base_env(env: &HostEnv, spec: &SandboxSpec) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    out.insert("HOME".into(), spec.home.display().to_string());
    out.insert("USER".into(), spec.user.clone());
    out.insert("LOGNAME".into(), spec.user.clone());
    out.insert("PATH".into(), spec.path_value());
    out.insert("SHELL".into(), "/bin/sh".into());
    out.insert("PADDOCK".into(), "1".into());
    out.insert("PADDOCK_ID".into(), spec.id.clone());
    out.insert("PADDOCK_HARNESS".into(), spec.harness.clone());
    // Tells some harnesses that an external sandbox is in place.
    out.insert("IS_SANDBOX".into(), "1".into());
    out.insert("TMPDIR".into(), "/tmp".into());
    for k in [
        "TERM",
        "COLORTERM",
        "LANG",
        "LANGUAGE",
        "TZ",
        "NO_COLOR",
        "FORCE_COLOR",
        "TERM_PROGRAM",
    ] {
        if let Some(v) = env.var(k) {
            out.insert(k.into(), v.into());
        }
    }
    for (k, v) in &env.vars {
        if k.starts_with("LC_") {
            out.insert(k.clone(), v.clone());
        }
    }
    for (k, v) in &spec.env {
        out.insert(k.clone(), v.value.clone());
    }
    out
}

/// A minimal passwd and group with only the sandbox user, so the host's user
/// list is not visible and tools that call getpwuid still work.
pub fn identity_files(spec: &SandboxSpec) -> (String, String) {
    let passwd = format!(
        "root:x:0:0:root:/root:/bin/sh\n{u}:x:{uid}:{gid}:{u}:{home}:/bin/sh\nnobody:x:65534:65534:nobody:/:/usr/bin/nologin\n",
        u = spec.user,
        uid = spec.uid,
        gid = spec.gid,
        home = spec.home.display()
    );
    let group = format!(
        "root:x:0:\n{u}:x:{gid}:\nnobody:x:65534:\n",
        u = spec.user,
        gid = spec.gid
    );
    (passwd, group)
}

/// Container backends in network mode 'egress' reach the network only through
/// the host proxy on this socket.
pub fn relay_socket(env: &HostEnv, backend: &str, spec: &SandboxSpec) -> Option<PathBuf> {
    if !(matches!(backend, "docker" | "podman") && spec.network == Network::Egress) {
        return None;
    }
    crate::sockpath::host_socket(&env.paddock_run(), env.uid, &spec.id, "proxy.sock").ok()
}

pub fn needs_pasta(spec: &SandboxSpec) -> bool {
    spec.network == Network::Egress
}

/// The configuration of paddock's proxy for one sandbox: the host proxy
/// variables, the allowed loopback ports and the logs.
pub fn host_proxy_cfg(env: &HostEnv, spec: &SandboxSpec) -> crate::proxy::HostProxy {
    let mut cfg = crate::proxy::HostProxy::from_vars(|k| env.var(k).map(str::to_string));
    cfg.allow_loopback = spec.host_ports.iter().map(|(p, _)| *p).collect();
    cfg.allow_lan = spec.lan;
    cfg.boundary = Some(crate::boundary::proxy_sink(env, &spec.id));
    cfg.egress = crate::monitor::egress::sink(env, &spec.id, &spec.harness, &spec.project);
    for w in cfg.cleartext_warnings() {
        log::warn!("{w}");
    }
    cfg
}

/// Start paddock's proxy for the backends that reach it from the host side:
/// docker and podman in network mode egress (a socket), and seatbelt in
/// network modes egress and api (a localhost port). seatbelt also starts
/// its pty broker here.
pub fn start_host_proxy(env: &HostEnv, spec: &mut SandboxSpec, backend: &str) -> Result<()> {
    let ptys = if backend == seatbelt::NAME {
        seatbelt::run::start_proxy(env, spec)?;
        seatbelt::run::start_pty(env, spec)?
    } else if backend == vm::NAME {
        vm::start_proxy(env, spec)?;
        true
    } else {
        if let Some(sock) = relay_socket(env, backend, spec) {
            crate::proxy::serve(&sock, host_proxy_cfg(env, spec))?;
        }
        true
    };
    pty_settings(spec, ptys);
    Ok(())
}

/// The settings for a harness without new ptys (`when = "no_pty"`) stay
/// only when `ptys` is false.
pub fn pty_settings(spec: &mut SandboxSpec, ptys: bool) {
    spec.home_settings.retain(|s| !(ptys && s.no_pty));
}

/// Network mode api: each host that the proxy refuses goes into the audit log.
pub fn api_refusals(env: &HostEnv, spec: &SandboxSpec) -> crate::proxy::allow::Refusals {
    let (e1, spec) = (env.clone(), spec.clone());
    crate::proxy::allow::Refusals::new(std::sync::Arc::new(move |host: &str, why: &str| {
        crate::broker::audit_line(
            &e1,
            &serde_json::json!({
                "time": crate::state::now(),
                "sandbox": spec.id,
                "harness": spec.harness,
                "project": spec.project,
                "capability": "network",
                "detail": format!("api: {host}: {why}"),
                "decision": "refused",
            }),
        );
    }))
}

/// Backends that cannot move a host path (srt, seatbelt) leave the sockets
/// and the pasture folders at their host paths. These variables name them.
pub fn host_path_env(spec: &SandboxSpec, vars: &mut BTreeMap<String, String>) {
    let s = |p: &std::path::Path| p.display().to_string();
    if let Some(sock) = &spec.host_socket {
        vars.insert("PADDOCK_HOST_SOCKET".into(), s(sock));
    }
    if let Some(sock) = &spec.kv_socket {
        vars.insert("PADDOCK_KV_SOCKET".into(), s(sock));
    }
    if let Some(sock) = &spec.ssh_agent_socket {
        vars.insert("SSH_AUTH_SOCK".into(), s(sock));
    }
    if let Some(dir) = &spec.devices {
        vars.insert("PADDOCK_DEVICES".into(), s(dir));
    }
    for (mount, var) in [
        (crate::pasture::MOUNT, "PADDOCK_PASTURE"),
        (
            crate::pasture::workbench::MOUNT,
            crate::pasture::workbench::ENV,
        ),
        (
            crate::pasture::ports::PORTS_MOUNT,
            crate::pasture::ports::PORTS_ENV,
        ),
        (
            crate::pasture::ports::OWN_MOUNT,
            crate::pasture::ports::OWN_ENV,
        ),
    ] {
        if let Some(src) = spec
            .mounts
            .iter()
            .find(|m| m.dst == std::path::Path::new(mount))
            .and_then(|m| m.src.as_ref())
        {
            vars.insert(var.into(), s(src));
        }
    }
}
