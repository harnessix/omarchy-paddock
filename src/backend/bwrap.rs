//! bubblewrap backend. The host `/usr` is bound read-only, so every tool the
//! host has is available, and nothing private lives there. `$HOME` is the
//! sandbox's own directory. Outbound network goes through pasta, which gives
//! the sandbox its own network namespace without the host loopback.

use super::{base_env, identity_files, needs_pasta, Backend, Launch};
use crate::env::HostEnv;
use crate::spec::{
    sandbox_runtime_dir, MountKind, Network, SandboxSpec, SANDBOX_WAYLAND, TOOL_DIR,
};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct Bwrap;

/// A link-local address that pasta answers DNS queries on.
pub const DNS_FORWARD: &str = "169.254.0.53";

/// Top-level directories that may be symlinks into /usr (merged /usr) or real directories.
const ROOT_LINKS: &[&str] = &["/bin", "/sbin", "/lib", "/lib64", "/lib32", "/libx32"];

fn arg(v: &mut Vec<String>, items: &[&str]) {
    v.extend(items.iter().map(|s| s.to_string()));
}

fn p(path: &Path) -> String {
    path.display().to_string()
}

pub fn bwrap_version(env: &HostEnv) -> Option<(u32, u32)> {
    let exe = env.which("bwrap")?;
    let out = Command::new(exe).arg("--version").output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    let ver = s.split_whitespace().last()?;
    let mut it = ver.split('.').map(|x| x.parse::<u32>().unwrap_or(0));
    Some((it.next()?, it.next().unwrap_or(0)))
}

impl Backend for Bwrap {
    fn name(&self) -> &'static str {
        "bwrap"
    }

    fn check(&self, env: &HostEnv, spec: &SandboxSpec) -> Result<()> {
        if spec.network == Network::Allowlist {
            bail!("network mode 'allowlist' needs the srt backend");
        }
        let Some(ver) = bwrap_version(env) else {
            bail!("bubblewrap (bwrap) is not installed")
        };
        if ver < (0, 8) {
            bail!(
                "bubblewrap {}.{} is too old: 0.8 or newer is needed for --disable-userns",
                ver.0,
                ver.1
            );
        }
        if needs_pasta(spec) && env.which("pasta").is_none() {
            bail!("network mode 'egress' needs pasta (package passt). Install it, or use --network none or --network host");
        }
        let ok = Command::new(env.which("bwrap").unwrap())
            .args(["--unshare-user", "--ro-bind", "/", "/", "true"])
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            bail!("bwrap cannot create a user namespace here (unprivileged user namespaces may be disabled)");
        }
        Ok(())
    }

    fn render(&self, env: &HostEnv, spec: &SandboxSpec, _interactive: bool) -> Result<Launch> {
        let bwrap = env.which("bwrap").context("bwrap not found")?;
        let run_dir = env.paddock_run().join(&spec.id);
        let (passwd, group) = identity_files(spec);
        let passwd_f = run_dir.join("passwd");
        let group_f = run_dir.join("group");
        let empty_f = run_dir.join("empty");

        let mut a: Vec<String> = Vec::new();
        arg(
            &mut a,
            &[
                "--die-with-parent",
                "--unshare-all",
                "--unshare-user",
                "--disable-userns",
            ],
        );
        if spec.network != Network::None {
            a.push("--share-net".into());
        }
        arg(&mut a, &["--hostname", "paddock"]);
        a.extend([
            "--uid".into(),
            spec.uid.to_string(),
            "--gid".into(),
            spec.gid.to_string(),
        ]);

        // The system, read-only.
        arg(&mut a, &["--ro-bind", "/usr", "/usr"]);
        for d in ROOT_LINKS {
            let path = Path::new(d);
            match std::fs::read_link(path) {
                Ok(target) => {
                    a.extend(["--symlink".into(), p(&target), d.to_string()]);
                }
                Err(_) if path.is_dir() => arg(&mut a, &["--ro-bind", d, d]),
                Err(_) => {}
            }
        }
        arg(&mut a, &["--ro-bind", "/etc", "/etc"]);
        for d in ["/opt", "/nix", "/gnu"] {
            arg(&mut a, &["--ro-bind-try", d, d]);
        }
        arg(
            &mut a,
            &[
                "--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp", "--tmpfs", "/run",
                "--tmpfs", "/var/tmp",
            ],
        );
        arg(&mut a, &["--dir", "/var"]);
        let resolv_f = run_dir.join("resolv.conf");
        let mut files = Vec::new();
        if spec.network == Network::Egress {
            // The sandbox has its own loopback, so a host stub resolver such as
            // 127.0.0.53 is not reachable. pasta forwards DNS, and only DNS,
            // from this address to the host resolver.
            files.push((
                resolv_f.clone(),
                format!("nameserver {DNS_FORWARD}\noptions edns0\n"),
                0o644,
            ));
            a.extend(["--ro-bind".into(), p(&resolv_f), p(&resolv_target())]);
        } else if let Ok(real) = std::fs::canonicalize("/etc/resolv.conf") {
            // /etc/resolv.conf is often a symlink into /run, which is now empty.
            if real.starts_with("/run") {
                a.extend(["--ro-bind-try".into(), p(&real), p(&real)]);
            }
        }
        a.extend(["--ro-bind".into(), p(&passwd_f), "/etc/passwd".into()]);
        a.extend(["--ro-bind".into(), p(&group_f), "/etc/group".into()]);

        // The sandbox home, then everything else in parent-before-child order.
        a.extend(["--bind".into(), p(&spec.home_src), p(&spec.home)]);
        for m in spec.ordered_mounts() {
            match m.kind {
                MountKind::Ro => {
                    a.extend(["--ro-bind".into(), p(m.src.as_ref().unwrap()), p(&m.dst)])
                }
                MountKind::Rw => a.extend(["--bind".into(), p(m.src.as_ref().unwrap()), p(&m.dst)]),
                MountKind::Tmpfs => a.extend(["--tmpfs".into(), p(&m.dst)]),
                MountKind::MaskFile => a.extend(["--ro-bind".into(), p(&empty_f), p(&m.dst)]),
                MountKind::MaskDir => {
                    a.extend([
                        "--tmpfs".into(),
                        p(&m.dst),
                        "--remount-ro".into(),
                        p(&m.dst),
                    ]);
                }
            }
        }
        for t in &spec.host_tools {
            a.extend([
                "--ro-bind".into(),
                p(&t.src),
                format!("{TOOL_DIR}/{}", t.name),
            ]);
        }
        if let Some(sock) = &spec.host_socket {
            a.extend([
                "--ro-bind".into(),
                p(sock),
                crate::broker::SOCKET_IN_SANDBOX.into(),
            ]);
        }
        if spec.gui.enabled() {
            let rt = sandbox_runtime_dir(spec.uid);
            a.extend(["--perms".into(), "0700".into(), "--dir".into(), p(&rt)]);
            if let Some(sock) = &spec.gui.host_socket {
                a.extend(["--ro-bind".into(), p(sock), p(&rt.join(SANDBOX_WAYLAND))]);
            }
            if let Some(bus) = &spec.bus_socket {
                a.extend(["--ro-bind".into(), p(bus), p(&rt.join("bus"))]);
            }
            if spec.gui.gpu {
                arg(&mut a, &["--dev-bind-try", "/dev/dri", "/dev/dri"]);
                // Mesa and Chromium find the GPU through sysfs. Read-only, and
                // only with --gpu.
                arg(
                    &mut a,
                    &[
                        "--ro-bind-try",
                        "/sys/dev/char",
                        "/sys/dev/char",
                        "--ro-bind-try",
                        "/sys/devices",
                        "/sys/devices",
                    ],
                );
            }
        }
        // Nothing outside the explicit mounts is writable.
        arg(&mut a, &["--remount-ro", "/"]);
        a.extend(["--chdir".into(), p(&spec.workdir), "--".into()]);
        a.extend(spec.final_command());

        files.push((passwd_f, passwd, 0o644));
        files.push((group_f, group, 0o644));
        files.push((empty_f, String::new(), 0o444));
        let mut env_map = base_env(env, spec);
        if spec.host_socket.is_some() {
            env_map.insert(
                "PADDOCK_HOST_SOCKET".into(),
                crate::broker::SOCKET_IN_SANDBOX.into(),
            );
        }

        if spec.network == Network::Egress {
            let (pasta, mut pa) = pasta_prefix(env, spec)?;
            pa.push(p(&bwrap));
            pa.extend(a);
            return Ok(Launch {
                program: pasta,
                args: pa,
                env: env_map,
                files,
            });
        }
        if !spec.publish.is_empty() && spec.network == Network::None {
            bail!("ports cannot be published with network mode 'none'");
        }
        Ok(Launch {
            program: bwrap,
            args: a,
            env: env_map,
            files,
        })
    }
}

/// Where /etc/resolv.conf really lives. bwrap cannot mount over a symlink,
/// so a replacement goes on the symlink's target.
fn resolv_target() -> PathBuf {
    let link = Path::new("/etc/resolv.conf");
    match std::fs::read_link(link) {
        Ok(t) if t.is_absolute() => t,
        Ok(t) => normalize(&Path::new("/etc").join(t)),
        Err(_) => link.to_path_buf(),
    }
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

pub fn run_dir(env: &HostEnv, id: &str) -> PathBuf {
    env.paddock_run().join(id)
}

/// The pasta command that gives a sandbox its own network namespace with
/// outbound traffic only (see network mode 'egress'). The caller appends the
/// sandboxed command. Flatpak apps use it too.
pub fn pasta_prefix(env: &HostEnv, spec: &SandboxSpec) -> Result<(PathBuf, Vec<String>)> {
    let pasta = env.which("pasta").context("pasta not found")?;
    let mut pa: Vec<String> = vec![
        "--config-net".into(),
        "--quiet".into(),
        "--no-map-gw".into(),
    ];
    pa.extend(["--dns-forward".into(), DNS_FORWARD.into()]);
    // pasta forwards every bound port in both directions by default.
    // Turn all of that off: the sandbox must not reach host loopback
    // services, except the ports in `host_ports` (the host proxy and
    // explicitly allowed ports). Outbound connections are made by pasta
    // as ordinary host sockets, so they follow the host routes and any
    // TUN device, the same as a program on the host.
    let to_host = if spec.host_ports.is_empty() {
        "none".to_string()
    } else {
        spec.host_ports
            .iter()
            .map(|(p, _)| p.to_string())
            .collect::<Vec<_>>()
            .join(",")
    };
    pa.extend(["-T".into(), to_host]);
    arg(&mut pa, &["-U", "none", "-u", "none"]);
    if spec.publish.is_empty() {
        arg(&mut pa, &["-t", "none"]);
    } else {
        let ports: Vec<String> = spec.publish.iter().map(u16::to_string).collect();
        pa.extend(["-t".into(), format!("127.0.0.1/{}", ports.join(","))]);
    }
    // pasta sends traffic for any address that the namespace does not
    // have to the host, so the host's other addresses (bridges, VPN and
    // TUN interfaces) would reach services bound to 0.0.0.0. Make each of
    // them unreachable inside the namespace before bwrap drops the
    // capability to change routes. Traffic to other addresses on those
    // networks, such as TUN fake-ip ranges, is not affected.
    let ip = env
        .which("ip")
        .context("network mode 'egress' needs the ip command (package iproute2)")?;
    let mut script = String::from("set -e\n");
    for addr in crate::proxy::local_addrs() {
        match addr {
            std::net::IpAddr::V4(v4) if !v4.is_loopback() && !v4.is_unspecified() => {
                script.push_str(&format!(
                    "\"$PADDOCK_IP\" -4 route add unreachable {v4}/32\n"
                ));
            }
            std::net::IpAddr::V6(v6)
                if !v6.is_loopback()
                    && !v6.is_unspecified()
                    && (v6.segments()[0] & 0xffc0) != 0xfe80 =>
            {
                script.push_str(&format!(
                    "\"$PADDOCK_IP\" -6 route add unreachable {v6}/128 || true\n"
                ));
            }
            _ => {}
        }
    }
    script.push_str("exec \"$@\"");
    pa.push("--".into());
    pa.extend([
        "/bin/sh".into(),
        "-c".into(),
        format!("PADDOCK_IP='{}'\n{script}", p(&ip)),
        "paddock-net".into(),
    ]);
    Ok((pasta, pa))
}
