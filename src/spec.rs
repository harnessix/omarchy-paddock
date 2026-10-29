//! `SandboxSpec` is the intermediate representation. Profiles, detectors and
//! manifests add to it; the policy engine removes from it; backends render it.
//! It is plain data, so `paddock explain` can print it and tests can assert on it.

use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MountKind {
    /// Read-only bind of a host path.
    Ro,
    /// Read-write bind of a host path.
    Rw,
    /// An empty tmpfs.
    Tmpfs,
    /// Hide a file: bind an empty read-only file over it.
    MaskFile,
    /// Hide a directory: an empty read-only tmpfs over it.
    MaskDir,
}

#[derive(Debug, Clone, Serialize)]
pub struct Mount {
    pub src: Option<PathBuf>,
    pub dst: PathBuf,
    pub kind: MountKind,
    /// Who asked for this mount, for `paddock explain` and policy errors.
    pub origin: String,
}

impl Mount {
    pub fn bind(src: impl Into<PathBuf>, dst: impl Into<PathBuf>, rw: bool, origin: &str) -> Self {
        Self {
            src: Some(src.into()),
            dst: dst.into(),
            kind: if rw { MountKind::Rw } else { MountKind::Ro },
            origin: origin.to_string(),
        }
    }
    pub fn same(path: impl Into<PathBuf>, rw: bool, origin: &str) -> Self {
        let p = path.into();
        Self::bind(p.clone(), p, rw, origin)
    }
    pub fn tmpfs(dst: impl Into<PathBuf>, origin: &str) -> Self {
        Self {
            src: None,
            dst: dst.into(),
            kind: MountKind::Tmpfs,
            origin: origin.to_string(),
        }
    }
    pub fn mask(dst: impl Into<PathBuf>, is_dir: bool, origin: &str) -> Self {
        Self {
            src: None,
            dst: dst.into(),
            kind: if is_dir {
                MountKind::MaskDir
            } else {
                MountKind::MaskFile
            },
            origin: origin.to_string(),
        }
    }
    pub fn is_bind(&self) -> bool {
        matches!(self.kind, MountKind::Ro | MountKind::Rw)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    /// No network at all.
    None,
    /// Outbound internet, but not the host loopback interface.
    #[default]
    Egress,
    /// The host network namespace. Host-local services are reachable.
    Host,
    /// Only the listed domains (srt backend).
    Allowlist,
}

impl std::str::FromStr for Network {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> anyhow::Result<Self> {
        match s {
            "none" => Ok(Self::None),
            "egress" => Ok(Self::Egress),
            "host" => Ok(Self::Host),
            "allowlist" => Ok(Self::Allowlist),
            _ => anyhow::bail!("unknown network mode '{s}': use none, egress, allowlist or host"),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct EnvVar {
    pub value: String,
    pub origin: String,
    /// Secret values are never printed by `explain`.
    pub secret: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct HostTool {
    pub name: String,
    pub src: PathBuf,
    pub origin: String,
}

/// Directory inside the sandbox where host tools (for example `mise`) appear.
pub const TOOL_DIR: &str = "/run/paddock/bin";

/// How the sandbox gets a display.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum GuiMode {
    /// No display.
    #[default]
    Off,
    /// A private compositor inside the sandbox. Agents take screenshots of it.
    /// Nothing on the host desktop is reachable.
    Headless,
    /// A nested compositor that runs on the host as one window. The sandbox
    /// talks only to that compositor, never to the host one.
    Window,
    /// The host compositor socket itself. On wlroots compositors a client can
    /// capture the screen, read the clipboard and type into other windows.
    Host,
    /// For desktop agent apps: the app's windows appear on the host desktop
    /// as normal windows, through a restricted socket that hides privileged
    /// protocols (see wlsec.rs). The session bus is filtered to portals.
    App,
}

impl std::str::FromStr for GuiMode {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> anyhow::Result<Self> {
        match s {
            "off" => Ok(Self::Off),
            "headless" => Ok(Self::Headless),
            "window" => Ok(Self::Window),
            "host" => Ok(Self::Host),
            "app" => Ok(Self::App),
            _ => anyhow::bail!("unknown gui mode '{s}': use off, headless, window, host or app"),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Gui {
    pub mode: GuiMode,
    /// Bind GPU render nodes (`/dev/dri/renderD*`).
    pub gpu: bool,
    pub width: u32,
    pub height: u32,
    /// The compositor socket on the host that the sandbox connects to, for
    /// `window` and `host` modes.
    pub host_socket: Option<PathBuf>,
    pub origin: String,
}

impl Default for Gui {
    fn default() -> Self {
        Self {
            mode: GuiMode::Off,
            gpu: false,
            width: 1600,
            height: 1000,
            host_socket: None,
            origin: "default".into(),
        }
    }
}

impl Gui {
    pub fn enabled(&self) -> bool {
        self.mode != GuiMode::Off
    }
}

/// The display socket name inside the sandbox, under its private runtime dir.
pub const SANDBOX_WAYLAND: &str = "wayland-0";

pub fn sandbox_runtime_dir(uid: u32) -> PathBuf {
    PathBuf::from(format!("/run/user/{uid}"))
}

#[derive(Debug, Clone, Serialize)]
pub struct SandboxSpec {
    pub id: String,
    pub harness: String,
    pub project: PathBuf,
    /// Host directory that becomes `$HOME` inside the sandbox.
    pub home_src: PathBuf,
    /// The path of `$HOME` inside the sandbox. It equals the host home path,
    /// so absolute paths in transcripts and error messages stay valid.
    pub home: PathBuf,
    pub uid: u32,
    pub gid: u32,
    pub user: String,
    pub mounts: Vec<Mount>,
    pub env: BTreeMap<String, EnvVar>,
    pub path: Vec<PathBuf>,
    pub host_tools: Vec<HostTool>,
    pub network: Network,
    pub publish: Vec<u16>,
    /// Host loopback ports the sandbox can reach: the host proxy, and ports
    /// that a manifest allows (for example a local MCP server). All other host
    /// loopback ports stay unreachable.
    pub host_ports: Vec<(u16, String)>,
    /// Domains reachable in network mode `allowlist`.
    pub allow_domains: Vec<String>,
    /// Private and link-local networks are reachable (network.lan).
    pub lan: bool,
    /// A private keyring starts with the command (keyring.rs).
    pub keyring: bool,
    pub gui: Gui,
    /// Shell lines that run inside the sandbox before the command.
    pub setup: Vec<String>,
    pub command: Vec<String>,
    pub workdir: PathBuf,
    /// Project files that host tools run automatically. Paddock warns when they change.
    pub watch: Vec<PathBuf>,
    pub warnings: Vec<String>,
    /// Shell lines for the provision step, a short run before the harness in
    /// which the `provision_rw` mounts are writable.
    pub provision: Vec<String>,
    pub provision_rw: Vec<PathBuf>,
    /// A filtered session bus socket on the host (app mode). Backends bind it
    /// to $XDG_RUNTIME_DIR/bus inside.
    pub bus_socket: Option<PathBuf>,
    /// The host passthrough socket on the host (see broker.rs). Backends make
    /// it reachable inside and set PADDOCK_HOST_SOCKET to where it appears.
    pub host_socket: Option<PathBuf>,
    /// Information for the user, printed at start.
    pub notices: Vec<String>,
    /// Host PATH entries, appended after the sandbox's own entries.
    pub inherit_path: Vec<PathBuf>,
    /// Files generated for this run (passwd, group). Backends bind them read-only.
    pub generated: Vec<(PathBuf, PathBuf)>,
}

impl SandboxSpec {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: String,
        harness: &str,
        project: &Path,
        home_src: PathBuf,
        host_home: &Path,
        uid: u32,
        gid: u32,
        user: &str,
    ) -> Self {
        Self {
            id,
            harness: harness.to_string(),
            project: project.to_path_buf(),
            home_src,
            home: host_home.to_path_buf(),
            uid,
            gid,
            user: user.to_string(),
            mounts: Vec::new(),
            env: BTreeMap::new(),
            path: Vec::new(),
            host_tools: Vec::new(),
            network: Network::Egress,
            publish: Vec::new(),
            host_ports: Vec::new(),
            allow_domains: Vec::new(),
            lan: false,
            keyring: false,
            gui: Gui::default(),
            setup: Vec::new(),
            command: Vec::new(),
            workdir: project.to_path_buf(),
            watch: Vec::new(),
            warnings: Vec::new(),
            provision: Vec::new(),
            provision_rw: Vec::new(),
            bus_socket: None,
            host_socket: None,
            notices: Vec::new(),
            inherit_path: Vec::new(),
            generated: Vec::new(),
        }
    }

    pub fn set_env(&mut self, key: &str, value: impl Into<String>, origin: &str) {
        self.env.insert(
            key.to_string(),
            EnvVar {
                value: value.into(),
                origin: origin.to_string(),
                secret: false,
            },
        );
    }

    pub fn set_secret_env(&mut self, key: &str, value: impl Into<String>, origin: &str) {
        self.env.insert(
            key.to_string(),
            EnvVar {
                value: value.into(),
                origin: origin.to_string(),
                secret: true,
            },
        );
    }

    pub fn add_path(&mut self, dir: impl Into<PathBuf>) {
        let d = dir.into();
        if !self.path.contains(&d) {
            self.path.push(d);
        }
    }

    pub fn add_host_port(&mut self, port: u16, origin: &str) {
        if !self.host_ports.iter().any(|(p, _)| *p == port) {
            self.host_ports.push((port, origin.to_string()));
        }
    }

    pub fn add_mount(&mut self, m: Mount) {
        // A later mount for the same destination replaces an earlier one.
        self.mounts.retain(|old| old.dst != m.dst);
        self.mounts.push(m);
    }

    pub fn add_tool(&mut self, name: &str, src: PathBuf, origin: &str) {
        if self.host_tools.iter().any(|t| t.name == name) {
            return;
        }
        self.host_tools.push(HostTool {
            name: name.to_string(),
            src,
            origin: origin.to_string(),
        });
    }

    /// The spec for the provision step: same sandbox, provision mounts
    /// writable, no display, and the provision lines as the command.
    pub fn provision_spec(&self) -> Option<SandboxSpec> {
        if self.provision.is_empty() {
            return None;
        }
        let mut p = self.clone();
        for m in p.mounts.iter_mut() {
            if self.provision_rw.contains(&m.dst) {
                m.kind = MountKind::Rw;
            }
        }
        p.gui = Gui::default();
        p.setup.clear();
        p.command = vec![
            "/bin/sh".into(),
            "-c".into(),
            format!("set -e\n{}", self.provision.join("\n")),
        ];
        Some(p)
    }

    /// Mounts in the order a backend must apply them: parents before children.
    pub fn ordered_mounts(&self) -> Vec<&Mount> {
        let mut v: Vec<&Mount> = self.mounts.iter().collect();
        v.sort_by_key(|m| m.dst.components().count());
        v
    }

    /// The final command line, with setup lines prepended when there are any.
    pub fn final_command(&self) -> Vec<String> {
        if self.setup.is_empty() {
            return self.command.clone();
        }
        let script = format!("{}\nexec \"$@\"", self.setup.join("\n"));
        let mut v = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            script,
            "paddock".to_string(),
        ];
        v.extend(self.command.iter().cloned());
        v
    }

    pub fn path_value(&self) -> String {
        let mut dirs: Vec<String> = self.path.iter().map(|p| p.display().to_string()).collect();
        dirs.push(TOOL_DIR.to_string());
        for d in [
            "/usr/local/sbin",
            "/usr/local/bin",
            "/usr/sbin",
            "/usr/bin",
            "/sbin",
            "/bin",
        ] {
            dirs.push(d.to_string());
        }
        for d in &self.inherit_path {
            let s = d.display().to_string();
            if !dirs.contains(&s) {
                dirs.push(s);
            }
        }
        dirs.join(":")
    }
}
