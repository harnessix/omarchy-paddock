//! srt backend: Anthropic's sandbox runtime
//! (https://github.com/anthropic-experimental/sandbox-runtime).
//!
//! srt runs the command in bubblewrap with its own network namespace and
//! filters network traffic by domain in a proxy on the host. paddock uses it
//! for network mode `allowlist`, which the other backends do not have.
//!
//! srt binds each path at its own location: it cannot put a different folder
//! at a host path. So `$HOME` is the sandbox home at its real path, read-only
//! imports are copied into it, and shared caches are not mounted. srt reads
//! everywhere by default, so the recipe denies every private area and then
//! allows back what the spec mounts. The spike that found this recipe also
//! found three traps, handled below: the host `/tmp` is visible and shared
//! unless it is denied, srt sets `TMPDIR` to a folder it does not create,
//! and denying all of `/run` breaks srt's own network bridge.

use super::{base_env, Backend, Launch};
use crate::env::HostEnv;
use crate::spec::{GuiMode, MountKind, Network, SandboxSpec};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct Srt;

/// The oldest srt whose settings format paddock writes.
pub const MIN_VERSION: (u32, u32, u32) = (0, 0, 81);

/// The srt launcher. A version-manager shim is resolved to the real file, as
/// for harnesses: a shim depends on the manager's configuration at run time.
pub fn srt_exe(env: &HostEnv) -> Option<PathBuf> {
    if let Some(mise) = env.which("mise") {
        if let Ok(p) = crate::env::capture(crate::mise::host_command(&mise).args(["which", "srt"]))
        {
            let p = PathBuf::from(p.lines().last().unwrap_or_default());
            if crate::env::is_executable(&p) {
                return Some(p);
            }
        }
    }
    env.which("srt")
        .filter(|p| !p.to_string_lossy().contains("/shims/"))
}

/// The installed srt package. Its seccomp helper runs inside the sandbox, so
/// the package must stay readable even when it lives under a denied home.
pub fn package_root(env: &HostEnv) -> Option<PathBuf> {
    let exe = srt_exe(env)?;
    let mut real = std::fs::canonicalize(&exe).ok()?;
    // A version-manager shim: ask the manager for the real file.
    if real.file_name().is_some_and(|n| n != "cli.js")
        && !real.to_string_lossy().contains("node_modules")
    {
        if let Some(mise) = env.which("mise") {
            if let Ok(p) =
                crate::env::capture(crate::mise::host_command(&mise).args(["which", "srt"]))
            {
                real = std::fs::canonicalize(p.lines().last().unwrap_or_default()).ok()?;
            }
        }
    }
    // Either inside the package, or a launcher next to node_modules (npm,
    // mise): the package directory, resolved to the path srt runs it from.
    for d in real.ancestors() {
        if d.join("vendor").is_dir() && d.join("package.json").is_file() {
            return Some(d.to_path_buf());
        }
        for nm in [d.join("node_modules"), d.to_path_buf()] {
            let pkg = nm.join("@anthropic-ai/sandbox-runtime");
            if pkg.join("package.json").is_file() {
                return std::fs::canonicalize(pkg).ok();
            }
        }
    }
    None
}

pub fn version(exe: &Path) -> Option<(u32, u32, u32)> {
    let out = Command::new(exe).arg("--version").output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    let v = s.split_whitespace().last()?;
    let mut it = v.split('.').map(|x| x.parse::<u32>().unwrap_or(0));
    Some((it.next()?, it.next()?, it.next()?))
}

/// Host areas that hold private data or control sockets. Each is denied, and
/// what the spec mounts inside them is allowed back. Denying all of `/run`
/// breaks srt's network bridge, so only the sensitive parts of it are listed.
fn denied_areas(env: &HostEnv) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = [
        "/home",
        "/root",
        "/run/user",
        "/tmp",
        "/var/tmp",
        "/mnt",
        "/media",
        "/srv",
        "/run/docker.sock",
        "/var/run/docker.sock",
        "/run/containerd",
        "/run/podman",
        "/run/dbus",
        "/var/run/dbus",
        "/run/libvirt",
        "/var/lib/docker",
        "/var/lib/containers",
    ]
    .iter()
    .map(PathBuf::from)
    .collect();
    v.push(env.home.clone());
    v
}

fn s(p: &Path) -> String {
    p.display().to_string()
}

/// The temporary folder of one run. srt puts its network bridge sockets in
/// the temporary folder of its own process, and the sandbox gets it as
/// TMPDIR. The host /tmp is denied, so this folder takes its place. It sits
/// in the sandbox home, the one writable area srt does not have to re-allow
/// inside a denied region.
pub fn private_tmp(env: &HostEnv, spec: &SandboxSpec) -> PathBuf {
    let _ = env;
    spec.home_src.join(".paddock-tmp").join(&spec.id)
}

/// srt's own temporary folder, for its bridge sockets.
pub fn bridge_dir(env: &HostEnv, spec: &SandboxSpec) -> PathBuf {
    env.paddock_run().join(&spec.id).join("b")
}

/// The srt settings for a spec.
pub fn settings(env: &HostEnv, spec: &SandboxSpec) -> Result<serde_json::Value> {
    let mut deny_read: Vec<String> = denied_areas(env).iter().map(|p| s(p)).collect();
    let mut allow_read: Vec<String> = vec![s(&spec.home_src), s(&env.paddock_run().join(&spec.id))];
    if let Some(root) = package_root(env) {
        allow_read.push(s(&root));
    }
    let mut allow_write: Vec<String> = vec![s(&spec.home_src), s(&private_tmp(env, spec))];
    let mut deny_write: Vec<String> = Vec::new();
    for m in &spec.mounts {
        let same = m.src.as_ref().is_some_and(|src| *src == m.dst);
        match m.kind {
            MountKind::Ro if same => {
                allow_read.push(s(&m.dst));
                deny_write.push(s(&m.dst));
            }
            MountKind::Rw if same => {
                allow_read.push(s(&m.dst));
                allow_write.push(s(&m.dst));
            }
            MountKind::MaskFile | MountKind::MaskDir => {
                deny_read.push(s(&m.dst));
                deny_write.push(s(&m.dst));
            }
            // Shared caches and other remapped mounts cannot be expressed:
            // the sandbox home holds its own caches instead.
            _ => {}
        }
    }
    for t in &spec.host_tools {
        if let Some(d) = t.src.parent() {
            allow_read.push(s(d));
        }
    }
    let domains: Vec<String> = match spec.network {
        Network::None => vec![],
        Network::Allowlist => {
            let mut d = spec.allow_domains.clone();
            d.extend(
                spec.host_ports
                    .iter()
                    .map(|(p, _)| format!("127.0.0.1:{p}")),
            );
            d
        }
        Network::Egress | Network::Host => bail!(
            "the srt backend supports network modes 'allowlist' and 'none'; use bwrap for '{}'",
            format!("{:?}", spec.network).to_lowercase()
        ),
    };
    for v in [
        &mut deny_read,
        &mut allow_read,
        &mut allow_write,
        &mut deny_write,
    ] {
        v.sort();
        v.dedup();
    }
    Ok(serde_json::json!({
        "network": {
            "allowedDomains": domains,
            "deniedDomains": [],
            // srt blocks creating Unix sockets with seccomp. A display server and
            // a session bus need them. Nested user namespaces stay blocked either
            // way, and srt's own network namespace isolates abstract sockets.
            // The host passthrough socket (broker.rs) needs it too.
            "allowAllUnixSockets": spec.gui.enabled() || spec.host_socket.is_some() || spec.keyring,
        },
        "filesystem": {
            "denyRead": deny_read,
            "allowRead": allow_read,
            "allowWrite": allow_write,
            "denyWrite": deny_write,
        },
        "allowNestedUserNamespaces": false,
    }))
}

impl Backend for Srt {
    fn name(&self) -> &'static str {
        "srt"
    }

    fn check(&self, env: &HostEnv, spec: &SandboxSpec) -> Result<()> {
        let Some(exe) = srt_exe(env) else {
            bail!("srt is not installed. Install it with: mise use -g npm:@anthropic-ai/sandbox-runtime")
        };
        match version(&exe) {
            Some(v) if v >= MIN_VERSION => {}
            Some(v) => bail!(
                "srt {}.{}.{} is too old: {}.{}.{} or newer is needed",
                v.0,
                v.1,
                v.2,
                MIN_VERSION.0,
                MIN_VERSION.1,
                MIN_VERSION.2
            ),
            None => bail!("cannot read the srt version"),
        }
        for dep in ["bwrap", "socat", "rg"] {
            if env.which(dep).is_none() {
                bail!("srt needs {dep} on the host");
            }
        }
        if matches!(spec.network, Network::Egress | Network::Host) {
            bail!("the srt backend supports network modes 'allowlist' and 'none'");
        }
        if matches!(spec.gui.mode, GuiMode::Window | GuiMode::Host) {
            bail!("the srt backend supports display modes 'off', 'headless' and 'app'");
        }
        // The link handoff folder sits at a fixed path that srt cannot place.
        if spec
            .mounts
            .iter()
            .any(|m| m.dst == std::path::Path::new(crate::app::INBOUND_DIR))
        {
            bail!("apps that receive links (schemes) or use login_ports need the bwrap, docker or podman backend for now");
        }
        Ok(())
    }

    /// Copy read-only imports from the host home into the sandbox home: srt
    /// cannot mount them at the sandbox's `$HOME`.
    fn prepare(&self, env: &HostEnv, spec: &SandboxSpec, _interactive: bool) -> Result<()> {
        for m in &spec.mounts {
            let (Some(src), MountKind::Ro) = (&m.src, m.kind) else {
                continue;
            };
            if *src != m.dst || !m.dst.starts_with(&env.home) || !m.origin.contains("import") {
                continue;
            }
            let rel = m.dst.strip_prefix(&env.home)?;
            // The sandbox home is sandbox-controlled: copy without following
            // anything that a sandbox planted there.
            let _ = crate::safefs::Root::open(&spec.home_src)?.copy_in(src, rel);
        }
        crate::safefs::Root::open(&spec.home_src)?.dir(Path::new(".paddock-run"), true)?;
        Ok(())
    }

    fn render(&self, env: &HostEnv, spec: &SandboxSpec, _interactive: bool) -> Result<Launch> {
        let exe = srt_exe(env).context("srt not found")?;
        let run_dir = env.paddock_run().join(&spec.id);
        let settings_f = run_dir.join("srt-settings.json");
        let mut vars = base_env(env, spec);
        // Values that point into the host home (XDG_DATA_HOME and the like,
        // when inherited) would land on srt's throwaway tmpfs over the denied
        // host home. Point them at the same place in the sandbox home, unless
        // the path is one that srt allows at its own location (the project,
        // install folders, imports).
        let kept: Vec<&std::path::Path> = spec
            .mounts
            .iter()
            .filter(|m| m.src.as_deref() == Some(m.dst.as_path()))
            .map(|m| m.dst.as_path())
            .collect();
        for v in vars.values_mut() {
            let path = std::path::Path::new(v.as_str());
            if kept.iter().any(|k| path.starts_with(k)) {
                continue;
            }
            if let Ok(rest) = path.strip_prefix(&env.home) {
                *v = s(&spec.home_src.join(rest));
            }
        }
        vars.insert("HOME".into(), s(&spec.home_src));
        // Two temporary folders. srt's own TMPDIR holds its network bridge
        // sockets: a short path in the run folder, because a Unix socket path
        // has at most 107 bytes. CLAUDE_CODE_TMPDIR is what srt gives the
        // sandboxed command as TMPDIR: a writable folder in the sandbox home.
        let bridge = bridge_dir(env, spec);
        std::fs::create_dir_all(&bridge)?;
        let tmp = private_tmp(env, spec);
        // In the sandbox home: a planted .paddock-tmp link must not make
        // paddock create a folder elsewhere and allow writes to it.
        crate::safefs::Root::open(&spec.home_src)?
            .dir(Path::new(".paddock-tmp").join(&spec.id).as_path(), true)?;
        vars.insert("TMPDIR".into(), s(&bridge));
        // srt cannot bind-mount: the socket stays at its host path, in the run
        // folder, which allowRead already covers.
        if let Some(sock) = &spec.host_socket {
            vars.insert("PADDOCK_HOST_SOCKET".into(), s(sock));
        }
        vars.insert("CLAUDE_CODE_TMPDIR".into(), s(&tmp));
        // Host tools appear in their own directories, not under /run/paddock/bin.
        // paddock's own wrappers in the run folder come first. Other tools,
        // such as mise in /usr/bin, come last: their directory would otherwise
        // hide the wrappers (gh) behind the host programs of the same name.
        let run = run_dir.as_path();
        let dirs = |own: bool| {
            spec.host_tools
                .iter()
                .filter_map(|t| t.src.parent())
                .filter(move |d| d.starts_with(run) == own)
                .map(s)
        };
        let mut path: Vec<String> = dirs(true).collect();
        path.push(vars.get("PATH").cloned().unwrap_or_default());
        path.extend(dirs(false));
        vars.insert("PATH".into(), path.join(":"));
        let mut setup = vec!["mkdir -p \"${TMPDIR:-/tmp}\"".to_string()];
        if spec.gui.enabled() {
            let rt = spec.home_src.join(".paddock-run");
            vars.insert("XDG_RUNTIME_DIR".into(), s(&rt));
            setup.push(format!("chmod 700 '{}'", s(&rt)));
            // App mode: srt cannot bind the restricted Wayland socket and the
            // filtered bus at their usual places, so link them there. They
            // stay at their host paths in the run folder, which allowRead
            // covers; connecting through a link reaches the same socket.
            if spec.gui.mode == GuiMode::App {
                if let Some(sock) = &spec.gui.host_socket {
                    setup.push(format!(
                        "ln -sf '{}' '{}'",
                        s(sock),
                        s(&rt.join(crate::spec::SANDBOX_WAYLAND))
                    ));
                }
                if let Some(bus) = &spec.bus_socket {
                    setup.push(format!("ln -sf '{}' '{}'", s(bus), s(&rt.join("bus"))));
                    vars.insert(
                        "DBUS_SESSION_BUS_ADDRESS".into(),
                        format!("unix:path={}", s(&rt.join("bus"))),
                    );
                }
            }
        }
        let mut spec2 = spec.clone();
        setup.extend(spec.setup.iter().cloned());
        spec2.setup = setup;
        let mut args = vec!["--settings".to_string(), s(&settings_f), "--".to_string()];
        args.extend(spec2.final_command());
        let text = serde_json::to_string_pretty(&settings(env, spec)?)?;
        Ok(Launch {
            program: exe,
            args,
            env: vars,
            files: vec![(settings_f, text, 0o600)],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::Mount;
    use std::collections::BTreeMap;

    fn spec() -> (HostEnv, SandboxSpec) {
        let home = PathBuf::from("/h");
        let env = HostEnv::from_parts(home.clone(), "u".into(), 1000, 1000, BTreeMap::new());
        let project = PathBuf::from("/h/src/app");
        let mut s = SandboxSpec::new(
            "id1".into(),
            "t",
            &project,
            "/h/.local/share/paddock/homes/t/x".into(),
            &home,
            1000,
            1000,
            "u",
        );
        s.add_mount(Mount::same(&project, true, "project"));
        s.add_mount(Mount::same("/h/src/app/.git/hooks", false, "detector git"));
        s.add_mount(Mount::mask("/h/src/app/.env", false, "detector secrets"));
        s.add_mount(Mount::bind(
            "/h/.cache/paddock/shared/npm",
            "/h/.npm",
            true,
            "cache",
        ));
        s.network = Network::Allowlist;
        s.allow_domains = vec!["api.example.com".into()];
        s.add_host_port(8765, "manifest");
        (env, s)
    }

    fn list<'a>(v: &'a serde_json::Value, k: &str) -> Vec<&'a str> {
        v["filesystem"][k]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|x| x.as_str())
            .collect()
    }

    #[test]
    fn settings_follow_the_spike_recipe() {
        let (env, s) = spec();
        let v = settings(&env, &s).unwrap();
        let deny_read = list(&v, "denyRead");
        for p in [
            "/h",
            "/home",
            "/tmp",
            "/var/tmp",
            "/run/user",
            "/run/docker.sock",
            "/h/src/app/.env",
        ] {
            assert!(deny_read.contains(&p), "denyRead lacks {p}");
        }
        assert!(list(&v, "allowWrite").contains(&"/h/src/app"));
        assert!(list(&v, "denyWrite").contains(&"/h/src/app/.git/hooks"));
        // A remapped mount cannot be expressed in srt and is left out.
        assert!(!list(&v, "allowWrite").contains(&"/h/.npm"));
        assert_eq!(
            v["network"]["allowedDomains"],
            serde_json::json!(["api.example.com", "127.0.0.1:8765"])
        );
        assert_eq!(v["network"]["allowAllUnixSockets"], false);
    }

    #[test]
    fn refuses_open_network_modes() {
        let (env, mut s) = spec();
        s.network = Network::Egress;
        assert!(settings(&env, &s).is_err());
    }
}
