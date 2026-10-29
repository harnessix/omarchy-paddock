//! Backends render a `SandboxSpec` into a command. The second factory: one
//! spec, several isolation technologies.

pub mod bwrap;
pub mod docker;
pub mod srt;

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
        _ => bail!("unknown backend '{name}': use auto, bwrap, docker, podman or srt"),
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
    // Only srt filters by domain, so allowlist mode picks it.
    let order: &[&str] = if spec.network == Network::Allowlist {
        &["srt"]
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
    (matches!(backend, "docker" | "podman") && spec.network == Network::Egress)
        .then(|| env.paddock_run().join(&spec.id).join("proxy.sock"))
}

pub fn needs_pasta(spec: &SandboxSpec) -> bool {
    spec.network == Network::Egress
}
