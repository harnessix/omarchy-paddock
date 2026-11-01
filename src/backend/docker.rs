//! Docker and Podman backend. Use it when the sandbox needs system packages
//! that the host does not have. The base image follows the host distribution,
//! so host-built harness binaries find a compatible C library.

use super::{base_env, identity_files, Backend, Launch};
use crate::env::HostEnv;
use crate::manifest::hex;
use crate::spec::{
    sandbox_runtime_dir, MountKind, Network, SandboxSpec, SANDBOX_WAYLAND, TOOL_DIR,
};
use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

pub struct Docker {
    cli: &'static str,
}

#[derive(Debug, Clone, Default)]
pub struct ImageCfg {
    pub image: Option<String>,
    pub packages: Vec<String>,
    /// Add a compositor, Xwayland and a session bus for desktop projects.
    pub gui: bool,
    /// Add the runtime of Electron and Chromium apps (app display mode).
    pub app: bool,
    /// Add gnome-keyring and a session bus for the private keyring.
    pub keyring: bool,
}

/// Packages a desktop sandbox needs in an image, per distribution family.
/// Libraries that Electron and Chromium apps load, and the tools the app-mode
/// escape probe uses (wayland-info, grim, wtype, gdbus). Only the Arch list
/// was verified on a real install; the others follow the distributions'
/// package names.
fn app_packages(family: &str) -> &'static [&'static str] {
    match family {
        "arch" => &[
            "gtk3",
            "nss",
            "alsa-lib",
            "libxkbcommon",
            "mesa",
            "libcups",
            "libsecret",
            "libxss",
            "at-spi2-core",
            "glib2",
            "wayland-utils",
            "grim",
            "wtype",
            "xdg-utils",
        ],
        "debian" => &[
            "libgtk-3-0",
            "libnss3",
            "libasound2",
            "libxkbcommon0",
            "libgbm1",
            "libcups2",
            "libsecret-1-0",
            "libxss1",
            "libglib2.0-bin",
            "wayland-utils",
            "grim",
            "wtype",
            "xdg-utils",
        ],
        "fedora" => &[
            "gtk3",
            "nss",
            "alsa-lib",
            "libxkbcommon",
            "mesa-libgbm",
            "cups-libs",
            "libsecret",
            "libXScrnSaver",
            "glib2",
            "wayland-utils",
            "grim",
            "wtype",
            "xdg-utils",
        ],
        _ => &[],
    }
}

fn gui_packages(family: &str) -> &'static [&'static str] {
    match family {
        "arch" => &["weston", "xorg-xwayland", "dbus"],
        "debian" => &["weston", "xwayland", "dbus"],
        "fedora" => &["weston", "xorg-x11-server-Xwayland", "dbus-daemon"],
        _ => &[],
    }
}

thread_local! {
    static IMAGE_CFG: std::cell::RefCell<ImageCfg> = std::cell::RefCell::new(ImageCfg::default());
}

/// The manifest's `[docker]` table, set once before rendering.
pub fn set_image_cfg(cfg: ImageCfg) {
    IMAGE_CFG.with(|c| *c.borrow_mut() = cfg);
}

fn os_release() -> (String, String, String) {
    let text = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
    let get = |k: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(&format!("{k}=")))
            .map(|v| v.trim_matches('"').to_string())
            .unwrap_or_default()
    };
    (get("ID"), get("ID_LIKE"), get("VERSION_ID"))
}

/// A Dockerfile that matches the host distribution.
pub fn dockerfile(cfg: &ImageCfg) -> Result<String> {
    let (id, like, ver) = os_release();
    let family = |name: &str| id == name || like.split_whitespace().any(|l| l == name);
    let fam = if family("arch") {
        "arch"
    } else if family("debian") || family("ubuntu") {
        "debian"
    } else if family("fedora") {
        "fedora"
    } else {
        ""
    };
    // Layers, so that variants share most of their data: the core tools,
    // then one desktop layer for every display, app and keyring variant, then
    // the project's own packages. Each RUN starts with a marker, so that
    // `paddock gc` can find paddock's records in the build cache.
    let mut desktop: Vec<String> = Vec::new();
    if cfg.keyring || cfg.app || cfg.gui {
        let k: &[&str] = match fam {
            "arch" => &["gnome-keyring", "libsecret", "dbus"],
            "debian" => &["gnome-keyring", "libsecret-tools", "dbus"],
            "fedora" => &["gnome-keyring", "libsecret", "dbus-daemon"],
            _ => &[],
        };
        for p in k.iter().chain(app_packages(fam)).chain(gui_packages(fam)) {
            if !desktop.iter().any(|d| d == p) {
                desktop.push(p.to_string());
            }
        }
    }
    let layer = |what: &str, pkgs: &[String]| -> String {
        let list = pkgs.join(" ");
        match fam {
            "arch" => format!("RUN : {BUILD_MARKER} {what} && pacman -S --noconfirm --needed {list} && rm -rf /var/cache/pacman/pkg/*"),
            "debian" => format!("RUN : {BUILD_MARKER} {what} && apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends {list} && rm -rf /var/lib/apt/lists/*"),
            _ => format!("RUN : {BUILD_MARKER} {what} && dnf install -y {list} && dnf clean all"),
        }
    };
    let (from, core) = if let Some(img) = &cfg.image {
        (img.clone(), String::new())
    } else if fam == "arch" {
        ("docker.io/library/archlinux:latest".to_string(), format!(
            "RUN : {BUILD_MARKER} core && pacman -Syu --noconfirm --needed base-devel git curl ca-certificates && rm -rf /var/cache/pacman/pkg/*"
        ))
    } else if fam == "debian" {
        let base = if id == "ubuntu" || id == "debian" {
            format!("docker.io/library/{id}:{ver}")
        } else {
            "docker.io/library/debian:stable".into()
        };
        (base, format!(
            "RUN : {BUILD_MARKER} core && apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends build-essential git curl ca-certificates && rm -rf /var/lib/apt/lists/*"
        ))
    } else if fam == "fedora" {
        (format!("registry.fedoraproject.org/fedora:{ver}"), format!("RUN : {BUILD_MARKER} core && dnf install -y gcc gcc-c++ make git curl ca-certificates && dnf clean all"))
    } else {
        bail!(
            "cannot pick a base image for distribution '{id}'. Set [docker] image in paddock.toml"
        );
    };
    let mut df = format!("FROM {from}\n");
    if core.is_empty() {
        if !desktop.is_empty() || !cfg.packages.is_empty() {
            bail!("[docker] packages and desktop support need a known distribution; with a custom image, install them in your own image");
        }
    } else {
        df.push_str(&core);
        df.push('\n');
        if !desktop.is_empty() {
            df.push_str(&layer("desktop", &desktop));
            df.push('\n');
        }
        if !cfg.packages.is_empty() {
            df.push_str(&layer("packages", &cfg.packages));
            df.push('\n');
        }
    }
    df.push_str("LABEL io.paddock.base=1\n");
    Ok(df)
}

/// Starts every RUN line of paddock's base images, so that `paddock gc` can
/// tell paddock's build-cache records from other builds.
pub const BUILD_MARKER: &str = "paddock-base";

/// Commands of base images that paddock built before the marker existed.
pub const OLD_BUILD_COMMANDS: &[&str] = &[
    "pacman -Syu --noconfirm --needed base-devel git curl ca-certificates",
    "apt-get install -y --no-install-recommends build-essential git curl ca-certificates",
    "dnf install -y gcc gcc-c++ make git curl ca-certificates",
];

pub fn image_tag(df: &str) -> String {
    format!(
        "paddock-base:{}",
        &hex(&Sha256::digest(df.as_bytes()))[..12]
    )
}

impl Docker {
    pub fn new(cli: &'static str) -> Self {
        Self { cli }
    }

    fn cfg() -> ImageCfg {
        IMAGE_CFG.with(|c| c.borrow().clone())
    }
}

fn p(path: &Path) -> String {
    path.display().to_string()
}

fn mount_arg(kind: &str, src: Option<&Path>, dst: &Path, ro: bool) -> Result<String> {
    for part in [src.map(p), Some(p(dst))].into_iter().flatten() {
        if part.contains(',') || part.contains('=') {
            bail!("path {part} contains ',' or '=', which the --mount syntax cannot express");
        }
    }
    let mut s = format!("type={kind}");
    if let Some(src) = src {
        s.push_str(&format!(",src={}", p(src)));
    }
    s.push_str(&format!(",dst={}", p(dst)));
    if ro {
        s.push_str(",readonly");
    }
    Ok(s)
}

impl Backend for Docker {
    fn name(&self) -> &'static str {
        self.cli
    }

    fn check(&self, env: &HostEnv, _spec: &SandboxSpec) -> Result<()> {
        if _spec.network == Network::Allowlist {
            bail!("network mode 'allowlist' needs the srt backend");
        }
        let Some(exe) = env.which(self.cli) else {
            bail!("{} is not installed", self.cli)
        };
        let ok = Command::new(exe)
            .arg("info")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            bail!("{} is installed but the daemon is not reachable", self.cli);
        }
        dockerfile(&Self::cfg()).map(|_| ())
    }

    fn prepare(&self, env: &HostEnv, _spec: &SandboxSpec, _interactive: bool) -> Result<()> {
        let exe = env.which(self.cli).context("container CLI not found")?;
        let df = dockerfile(&Self::cfg())?;
        let tag = image_tag(&df);
        let present = Command::new(&exe)
            .args(["image", "inspect", &tag])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?
            .success();
        if present {
            return Ok(());
        }
        eprintln!("paddock: building image {tag}");
        let mut child = Command::new(&exe)
            .args(["build", "-t", &tag, "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()?;
        child.stdin.take().unwrap().write_all(df.as_bytes())?;
        if !child.wait()?.success() {
            bail!("image build failed");
        }
        // Older paddock images that no container uses, and the rest of the
        // automatic tier of `paddock gc`.
        crate::storage::after_build(env, &tag);
        Ok(())
    }

    fn render(&self, env: &HostEnv, spec: &SandboxSpec, interactive: bool) -> Result<Launch> {
        precreate_mount_points(spec)?;
        let exe = env.which(self.cli).context("container CLI not found")?;
        let relay = spec.network == Network::Egress;
        let mut owned;
        let spec = if relay {
            if !spec.publish.is_empty() {
                bail!("publishing ports is not supported on the {} backend in network mode 'egress'; use the bwrap backend", self.cli);
            }
            // No network interface: only the host proxy socket. See proxy.rs.
            owned = spec.clone();
            let me = std::env::current_exe()
                .and_then(std::fs::canonicalize)
                .context("cannot find the paddock executable")?;
            owned.add_tool("paddock", me, "network: relay to the host proxy");
            let mut cmd = format!(
                "paddock relay --listen 127.0.0.1:{} --socket {} --ready-file /tmp/.paddock-relay",
                crate::proxy::RELAY_PORT,
                crate::proxy::SOCKET_IN_SANDBOX
            );
            for (port, _) in &owned.host_ports {
                cmd.push_str(&format!(" --forward {port}"));
            }
            owned.setup.insert(
                0,
                format!("{cmd} >/dev/null 2>&1 &\ni=0; while [ ! -e /tmp/.paddock-relay ] && [ $i -lt 100 ]; do sleep 0.05; i=$((i+1)); done"),
            );
            &owned
        } else {
            spec
        };
        // The harness starts when the devices are there, or after 5 seconds.
        let with_devices;
        let spec = if spec.devices.is_some() {
            let mut s = spec.clone();
            s.setup.insert(
                0,
                format!(
                    "i=0; while [ ! -e {}/{} ] && [ $i -lt 100 ]; do sleep 0.05; i=$((i+1)); done",
                    crate::devices::MOUNT_IN_SANDBOX,
                    crate::devices::CATALOG
                ),
            );
            with_devices = s;
            &with_devices
        } else {
            spec
        };
        let tag = image_tag(&dockerfile(&Self::cfg())?);
        let run_dir = env.paddock_run().join(&spec.id);
        let (passwd, group) = identity_files(spec);
        let passwd_f = run_dir.join("passwd");
        let group_f = run_dir.join("group");
        let env_f = run_dir.join("env");
        let empty_f = run_dir.join("empty");

        let mut a: Vec<String> = vec!["run".into(), "--rm".into(), "-i".into()];
        if interactive {
            a.push("-t".into());
        }
        a.extend([
            "--init".into(),
            "--name".into(),
            format!("paddock-{}", spec.id),
            "--label".into(),
            format!("io.paddock.id={}", spec.id),
            "--label".into(),
            format!("io.paddock.harness={}", spec.harness),
            "--hostname".into(),
            "paddock".into(),
            "--user".into(),
            format!("{}:{}", spec.uid, spec.gid),
            "--cap-drop".into(),
            "ALL".into(),
            "--security-opt".into(),
            "no-new-privileges".into(),
            "--pids-limit".into(),
            "8192".into(),
            // Nothing outside the explicit mounts is writable.
            "--read-only".into(),
            "--tmpfs".into(),
            "/tmp:exec".into(),
            "--tmpfs".into(),
            "/var/tmp".into(),
        ]);
        if self.cli == "podman" {
            // Rootless podman maps container UIDs to subordinate IDs. keep-id
            // maps the host user to the same UID inside, so project files
            // keep their owner.
            a.push("--userns=keep-id".into());
            let profile = podman_seccomp(env, &exe)?;
            a.extend([
                "--security-opt".into(),
                format!("seccomp={}", profile.display()),
            ]);
        }
        a.push("--network".into());
        a.push(match spec.network {
            Network::None => "none".into(),
            Network::Egress | Network::Api => "none".into(),
            Network::Host => "host".into(),
            Network::Allowlist => bail!("network mode 'allowlist' needs the srt backend"),
        });
        for port in &spec.publish {
            a.extend(["-p".into(), format!("127.0.0.1:{port}:{port}")]);
        }
        a.extend(["--env-file".into(), p(&env_f)]);

        let proxy_sock = run_dir.join("proxy.sock");
        let mut mounts = vec![
            mount_arg("bind", Some(&passwd_f), Path::new("/etc/passwd"), true)?,
            mount_arg("bind", Some(&group_f), Path::new("/etc/group"), true)?,
            mount_arg("bind", Some(&spec.home_src), &spec.home, false)?,
        ];
        // The machine ID (identity.rs): an isolated login's own, or the
        // host's for a host login, as with bwrap. Not the image's: a login
        // made on the host would see another machine. The boot ID stays the
        // host's, because container runtimes refuse mounts inside /proc.
        let machine_id_f = run_dir.join("machine-id");
        let mut identity_files = Vec::new();
        if let Some(id) = &spec.identity.machine_id {
            identity_files.push((machine_id_f.clone(), id.clone(), 0o444));
            mounts.push(mount_arg(
                "bind",
                Some(&machine_id_f),
                Path::new("/etc/machine-id"),
                true,
            )?);
        } else if Path::new("/etc/machine-id").is_file() {
            mounts.push(mount_arg(
                "bind",
                Some(Path::new("/etc/machine-id")),
                Path::new("/etc/machine-id"),
                true,
            )?);
        }
        // The board and firmware names of the host. The serials in there
        // are readable by root only, and the container never runs as root;
        // the names alone help to tell machines apart. podman copies the
        // folder into a new tmpfs unless told not to, and fails on the
        // serials.
        // Machines without DMI (many ARM boards) have no such folder, and
        // a mount point cannot be made in sysfs.
        if Path::new("/sys/devices/virtual/dmi").is_dir() {
            let mut dmi = mount_arg("tmpfs", None, Path::new("/sys/devices/virtual/dmi"), true)?;
            if self.cli == "podman" {
                dmi.push_str(",notmpcopyup");
            }
            mounts.push(dmi);
        }
        for m in spec.ordered_mounts() {
            mounts.push(match m.kind {
                MountKind::Ro => mount_arg("bind", m.src.as_deref(), &m.dst, true)?,
                MountKind::Rw => mount_arg("bind", m.src.as_deref(), &m.dst, false)?,
                MountKind::Tmpfs => mount_arg("tmpfs", None, &m.dst, false)?,
                MountKind::MaskFile => mount_arg("bind", Some(&empty_f), &m.dst, true)?,
                MountKind::MaskDir => mount_arg("tmpfs", None, &m.dst, true)?,
            });
        }
        for t in &spec.host_tools {
            mounts.push(mount_arg(
                "bind",
                Some(&t.src),
                &Path::new(TOOL_DIR).join(&t.name),
                true,
            )?);
        }
        if spec.gui.enabled() {
            let rt = sandbox_runtime_dir(spec.uid);
            // podman spells "owned by the container user" as the U option.
            let owner = if self.cli == "podman" {
                "U".to_string()
            } else {
                format!("uid={},gid={}", spec.uid, spec.gid)
            };
            a.extend([
                "--tmpfs".into(),
                format!("{}:mode=0700,{owner}", rt.display()),
            ]);
            if spec.gui.mode == crate::spec::GuiMode::App {
                // Chromium needs more than the default 64 MB of /dev/shm.
                a.extend(["--shm-size".into(), "1g".into()]);
            }
            if let Some(bus) = &spec.bus_socket {
                mounts.push(mount_arg("bind", Some(bus), &rt.join("bus"), true)?);
            }
            // The image has no fonts beyond what its packages pull in, so text
            // in other scripts (CJK, for example) shows as boxes. The host's
            // system fonts hold no private data: share them read-only.
            for d in ["/usr/share/fonts", "/usr/local/share/fonts"] {
                if Path::new(d).is_dir() {
                    mounts.push(mount_arg("bind", Some(Path::new(d)), Path::new(d), true)?);
                }
            }
            if let Some(sock) = &spec.gui.host_socket {
                mounts.push(mount_arg(
                    "bind",
                    Some(sock),
                    &rt.join(SANDBOX_WAYLAND),
                    true,
                )?);
            }
            if spec.gui.gpu {
                use std::os::unix::fs::MetadataExt;
                for node in crate::gui::render_nodes() {
                    a.extend(["--device".into(), p(&node)]);
                    if self.cli == "podman" {
                        // Host groups do not map into a rootless user namespace.
                        if !a.iter().any(|x| x == "keep-groups") {
                            a.extend(["--group-add".into(), "keep-groups".into()]);
                        }
                    } else if let Ok(meta) = std::fs::metadata(&node) {
                        a.extend(["--group-add".into(), meta.gid().to_string()]);
                    }
                }
            }
        }
        if let Some(sock) = &spec.host_socket {
            mounts.push(mount_arg(
                "bind",
                Some(sock),
                Path::new(crate::broker::SOCKET_IN_SANDBOX),
                true,
            )?);
        }
        if let Some(sock) = &spec.kv_socket {
            mounts.push(mount_arg(
                "bind",
                Some(sock),
                Path::new(crate::kv::SOCKET_IN_SANDBOX),
                true,
            )?);
        }
        if let Some(sock) = &spec.ssh_agent_socket {
            mounts.push(mount_arg(
                "bind",
                Some(sock),
                Path::new(crate::credentials::sshagent::SOCKET_IN_SANDBOX),
                true,
            )?);
        }
        // An empty folder now. paddock mounts the devices on it after the
        // container started, and slave propagation carries that mount in
        // (devices.rs explains why).
        if let Some(dir) = &spec.devices {
            let m = mount_arg(
                "bind",
                Some(dir),
                Path::new(crate::devices::MOUNT_IN_SANDBOX),
                false,
            )?;
            mounts.push(format!("{m},bind-propagation=rslave"));
        }
        if relay {
            mounts.push(mount_arg(
                "bind",
                Some(&proxy_sock),
                Path::new(crate::proxy::SOCKET_IN_SANDBOX),
                true,
            )?);
        }
        for m in mounts {
            a.extend(["--mount".into(), m]);
        }
        a.extend(["--workdir".into(), p(&spec.workdir), tag]);
        a.extend(spec.final_command());

        // Values go through an env file with mode 0600, not the command line,
        // so they do not show up in the host process list.
        let mut vars = base_env(env, spec);
        if spec.host_socket.is_some() {
            vars.insert(
                "PADDOCK_HOST_SOCKET".into(),
                crate::broker::SOCKET_IN_SANDBOX.into(),
            );
        }
        if spec.kv_socket.is_some() {
            vars.insert(
                "PADDOCK_KV_SOCKET".into(),
                crate::kv::SOCKET_IN_SANDBOX.into(),
            );
        }
        if spec.ssh_agent_socket.is_some() {
            vars.insert(
                "SSH_AUTH_SOCK".into(),
                crate::credentials::sshagent::SOCKET_IN_SANDBOX.into(),
            );
        }
        if spec.devices.is_some() {
            vars.insert(
                "PADDOCK_DEVICES".into(),
                crate::devices::MOUNT_IN_SANDBOX.into(),
            );
        }
        if relay {
            let url = format!("http://127.0.0.1:{}", crate::proxy::RELAY_PORT);
            for k in [
                "HTTP_PROXY",
                "http_proxy",
                "HTTPS_PROXY",
                "https_proxy",
                "ALL_PROXY",
                "all_proxy",
            ] {
                vars.insert(k.into(), url.clone());
            }
            for k in ["NO_PROXY", "no_proxy"] {
                vars.insert(k.into(), "localhost,127.0.0.1,::1".into());
            }
        }
        let mut env_text = String::new();
        for (k, v) in &vars {
            if v.contains('\n') {
                bail!("environment variable {k} contains a newline, which an env file cannot hold");
            }
            env_text.push_str(&format!("{k}={v}\n"));
        }
        let mut host_env = std::collections::BTreeMap::new();
        for k in [
            "PATH",
            "HOME",
            "DOCKER_HOST",
            "DOCKER_CONFIG",
            "DOCKER_CONTEXT",
            "XDG_RUNTIME_DIR",
            "CONTAINER_HOST",
        ] {
            if let Some(v) = env.var(k) {
                host_env.insert(k.to_string(), v.to_string());
            }
        }
        Ok(Launch {
            program: exe,
            args: a,
            env: host_env,
            files: [
                vec![
                    (passwd_f, passwd, 0o644),
                    (group_f, group, 0o644),
                    (env_f, env_text, 0o600),
                    (empty_f, String::new(), 0o444),
                ],
                identity_files,
            ]
            .concat(),
        })
    }
}

const CLONE_NEWUSER: u64 = 0x1000_0000;

/// The default seccomp profile of podman allows `clone`, `clone3` and
/// `unshare` without conditions, so a process in the container can create a
/// nested user namespace, a common start of kernel exploits. Derive a profile
/// that differs in one point: `clone` and `unshare` pass only without
/// CLONE_NEWUSER, and `clone3`, whose flags seccomp cannot inspect, returns
/// ENOSYS so that the C library falls back to `clone`. Docker's default
/// profile does the same.
pub fn derive_seccomp(default: &serde_json::Value) -> Result<serde_json::Value> {
    let mut p = default.clone();
    let rules = p
        .get_mut("syscalls")
        .and_then(|s| s.as_array_mut())
        .context("the seccomp profile has no syscalls list")?;
    for r in rules.iter_mut() {
        if let Some(names) = r.get_mut("names").and_then(|n| n.as_array_mut()) {
            names.retain(|n| !matches!(n.as_str(), Some("clone" | "clone3" | "unshare")));
        }
    }
    rules.retain(|r| {
        r.get("names")
            .and_then(|n| n.as_array())
            .is_none_or(|n| !n.is_empty())
    });
    for name in ["clone", "unshare"] {
        rules.push(serde_json::json!({
            "names": [name],
            "action": "SCMP_ACT_ALLOW",
            "args": [{ "index": 0, "value": CLONE_NEWUSER, "valueTwo": 0, "op": "SCMP_CMP_MASKED_EQ" }]
        }));
    }
    rules.push(
        serde_json::json!({ "names": ["clone3"], "action": "SCMP_ACT_ERRNO", "errnoRet": 38 }),
    );
    Ok(p)
}

fn podman_seccomp(env: &HostEnv, podman: &Path) -> Result<std::path::PathBuf> {
    let out = Command::new(podman)
        .args(["info", "--format", "{{.Host.Security.SECCOMPProfilePath}}"])
        .output()?;
    let src = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let candidates = [
        src.as_str(),
        "/usr/share/containers/seccomp.json",
        "/etc/containers/seccomp.json",
    ];
    let file = candidates
        .iter()
        .find(|c| !c.is_empty() && Path::new(c).is_file())
        .context("cannot find the default seccomp profile of podman")?;
    let text = std::fs::read_to_string(file)?;
    let derived = derive_seccomp(&serde_json::from_str(&text)?)?;
    let body = serde_json::to_string_pretty(&derived)?;
    let path = env.paddock_cache().join("seccomp").join(format!(
        "podman-{}.json",
        &hex(&Sha256::digest(body.as_bytes()))[..12]
    ));
    if !path.is_file() {
        crate::privfs::write(&path, body)?;
    }
    Ok(path)
}

/// Create, as the current user, every mount point that lies inside the
/// sandbox home. Rootful docker creates a missing mount point itself, as
/// root, on the host side of the home bind. A root-owned `~/.config` then
/// stops apps from creating their settings there: Electron apps lose their
/// crash-report folder and abort at start.
///
/// The sandbox can write its home, so every step goes through safefs: a
/// symlink planted there is an error and is never followed, and a file is
/// only ever created new, never truncated.
pub fn precreate_mount_points(spec: &SandboxSpec) -> Result<()> {
    use crate::safefs::{Kind, Root};
    let root = Root::open(&spec.home_src)?;
    for m in &spec.mounts {
        let Ok(rel) = m.dst.strip_prefix(&spec.home) else {
            continue;
        };
        if rel.as_os_str().is_empty() {
            continue;
        }
        let is_dir = match (&m.src, m.kind) {
            (_, MountKind::Tmpfs | MountKind::MaskDir) => true,
            (_, MountKind::MaskFile) => false,
            (Some(src), _) => src.is_dir(),
            (None, _) => true,
        };
        if is_dir {
            root.dir(rel, true)?;
            continue;
        }
        let (Some(name), parent) = (rel.file_name(), rel.parent().unwrap_or(Path::new(""))) else {
            continue;
        };
        let d = root.dir(parent, true)?;
        // Concurrent starts of the same project share the home: a file that
        // appeared meanwhile is fine, a symlink is not.
        if let Err(e) = d.create_new(name, 0o600) {
            match d.kind(name)? {
                Some(Kind::Symlink) => bail!(
                    "{} is a symlink; paddock does not follow it. Remove it and start again",
                    d.path().join(name).display()
                ),
                Some(_) => {}
                None => return Err(e),
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mount_points_never_follow_links_planted_in_the_home() {
        use crate::spec::Mount;
        use std::os::unix::fs::symlink;
        let t = tempfile::tempdir().unwrap();
        let (home_src, outside) = (t.path().join("hs"), t.path().join("outside"));
        std::fs::create_dir_all(&home_src).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("keep"), "host").unwrap();
        let home = Path::new("/home/demo");
        let spec = |mounts: Vec<Mount>| {
            let mut s = SandboxSpec::new(
                "t".into(),
                "claude",
                Path::new("/w"),
                home_src.clone(),
                home,
                1000,
                1000,
                "demo",
            );
            s.mounts = mounts;
            s
        };
        // Plain mount points are made as folders and new files.
        let ok = spec(vec![
            Mount::tmpfs(home.join(".cache/x"), "t"),
            Mount::mask(home.join(".config/a/token"), false, "t"),
        ]);
        precreate_mount_points(&ok).unwrap();
        precreate_mount_points(&ok).unwrap();
        assert!(home_src.join(".cache/x").is_dir());
        assert!(home_src.join(".config/a/token").is_file());

        // A planted folder link creates nothing outside.
        symlink(&outside, home_src.join(".cargo")).unwrap();
        let s = spec(vec![Mount::tmpfs(home.join(".cargo/registry"), "t")]);
        assert!(precreate_mount_points(&s).is_err());
        assert!(!outside.join("registry").exists());

        // A dangling file link creates no file anywhere; a link to a host
        // file does not truncate it.
        symlink(outside.join("new"), home_src.join("dangling")).unwrap();
        symlink(outside.join("keep"), home_src.join("keep")).unwrap();
        for n in ["dangling", "keep"] {
            let s = spec(vec![Mount::mask(home.join(n), false, "t")]);
            assert!(precreate_mount_points(&s).is_err(), "{n}");
        }
        assert!(!outside.join("new").exists());
        assert_eq!(
            std::fs::read_to_string(outside.join("keep")).unwrap(),
            "host"
        );
    }

    #[test]
    fn image_variants_share_layers() {
        let cfg = |gui, app, keyring| ImageCfg {
            image: None,
            packages: vec![],
            gui,
            app,
            keyring,
        };
        // Hosts that paddock cannot map to a base image have nothing to share.
        let Ok(plain) = dockerfile(&cfg(false, false, false)) else {
            return;
        };
        let desktop = dockerfile(&cfg(true, false, false)).unwrap();
        // Every desktop variant is one image.
        assert_eq!(desktop, dockerfile(&cfg(false, true, false)).unwrap());
        assert_eq!(desktop, dockerfile(&cfg(false, false, true)).unwrap());
        assert_eq!(desktop, dockerfile(&cfg(true, true, true)).unwrap());
        // The desktop image starts with the plain image's core layer.
        let core = plain.lines().nth(1).unwrap();
        assert!(core.starts_with("RUN : paddock-base core"));
        assert_eq!(desktop.lines().nth(1).unwrap(), core);
        assert!(desktop.contains(": paddock-base desktop"));
        let with_pkgs = dockerfile(&ImageCfg {
            packages: vec!["jq".into()],
            ..cfg(false, false, false)
        })
        .unwrap();
        assert_eq!(with_pkgs.lines().nth(1).unwrap(), core);
        assert!(with_pkgs.contains(": paddock-base packages"));
    }

    #[test]
    fn derived_seccomp_blocks_only_new_user_namespaces() {
        let default = serde_json::json!({
            "defaultAction": "SCMP_ACT_ERRNO",
            "syscalls": [
                { "names": ["read", "clone", "clone3", "unshare", "setns"], "action": "SCMP_ACT_ALLOW" },
                { "names": ["clone3"], "action": "SCMP_ACT_ALLOW" }
            ]
        });
        let d = derive_seccomp(&default).unwrap();
        let rules = d["syscalls"].as_array().unwrap();
        let first: Vec<&str> = rules[0]["names"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|n| n.as_str())
            .collect();
        assert_eq!(first, ["read", "setns"]);
        assert_eq!(
            rules.len(),
            4,
            "the empty clone3 rule is dropped, three rules are added"
        );
        let clone = rules.iter().find(|r| r["names"][0] == "clone").unwrap();
        assert_eq!(clone["args"][0]["value"], CLONE_NEWUSER);
        assert_eq!(clone["args"][0]["valueTwo"], 0);
        let clone3 = rules.iter().find(|r| r["names"][0] == "clone3").unwrap();
        assert_eq!(clone3["action"], "SCMP_ACT_ERRNO");
    }
}
