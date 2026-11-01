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

/// A value that paddock writes into a settings file in the sandbox home at
/// each start (privacy.rs). The host file is never changed.
#[derive(Debug, Clone, Serialize)]
pub struct HomeSetting {
    /// Relative to the sandbox home.
    pub file: PathBuf,
    /// json or toml.
    pub format: String,
    pub path: Vec<String>,
    pub value: serde_json::Value,
    pub origin: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct HostTool {
    pub name: String,
    pub src: PathBuf,
    pub origin: String,
}

/// Where the shared mise installs appear, relative to the sandbox home. mise
/// finds them as its "system" installs, read-only.
pub const MISE_SHARED_INSTALLS: &str = ".local/share/paddock-mise/installs";

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
    /// PATH entries after the host tools and before the system directories,
    /// so they cannot shadow a host tool such as paddock's `gh`.
    pub tool_path: Vec<PathBuf>,
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
    /// App mode: the session bus is a dbus-daemon inside the sandbox
    /// (keyring.rs), not the filtered host bus.
    pub private_bus: bool,
    pub gui: Gui,
    /// Shell lines that run inside the sandbox before the command.
    pub setup: Vec<String>,
    pub command: Vec<String>,
    pub workdir: PathBuf,
    /// Project files that host tools run automatically. Paddock warns when they change.
    pub watch: Vec<PathBuf>,
    pub warnings: Vec<String>,
    /// Shell lines for the provision step, a short run in this sandbox before
    /// the harness starts. It has the same rights as the harness.
    pub provision: Vec<String>,
    /// `tool@version` pairs that a clean sandbox installs into the shared mise
    /// installs before the provision step (see `mise_shared_spec`).
    pub mise_shared: Vec<String>,
    /// A filtered session bus socket on the host (app mode). Backends bind it
    /// to $XDG_RUNTIME_DIR/bus inside.
    pub bus_socket: Option<PathBuf>,
    /// The host passthrough socket on the host (see broker.rs). Backends make
    /// it reachable inside and set PADDOCK_HOST_SOCKET to where it appears.
    pub host_socket: Option<PathBuf>,
    /// The key-value socket of this sandbox on the host (kv.rs). Backends
    /// make it reachable inside and set PADDOCK_KV_SOCKET.
    pub kv_socket: Option<PathBuf>,
    /// The machine ID and boot ID that the sandbox sees instead of the
    /// host's (identity.rs). Empty: the host's. Left out of `explain`.
    #[serde(skip)]
    pub identity: crate::identity::MachineIdentity,
    /// The host folder where the device file system of this sandbox is
    /// mounted (devices.rs). Backends bind it to `/dev/paddock`.
    pub devices: Option<PathBuf>,
    /// Information for the user, printed at start.
    pub notices: Vec<String>,
    /// Host PATH entries and the harness folder, appended after the
    /// sandbox's own entries. An entry already on PATH is skipped.
    pub inherit_path: Vec<PathBuf>,
    /// Files generated for this run (passwd, group). Backends bind them read-only.
    pub generated: Vec<(PathBuf, PathBuf)>,
    /// Values written into settings files in the sandbox home before the start.
    pub home_settings: Vec<HomeSetting>,
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
            tool_path: Vec::new(),
            host_tools: Vec::new(),
            network: Network::Egress,
            publish: Vec::new(),
            host_ports: Vec::new(),
            allow_domains: Vec::new(),
            lan: false,
            keyring: false,
            private_bus: false,
            gui: Gui::default(),
            setup: Vec::new(),
            command: Vec::new(),
            workdir: project.to_path_buf(),
            watch: Vec::new(),
            warnings: Vec::new(),
            provision: Vec::new(),
            mise_shared: Vec::new(),
            bus_socket: None,
            host_socket: None,
            kv_socket: None,
            identity: Default::default(),
            devices: None,
            notices: Vec::new(),
            inherit_path: Vec::new(),
            generated: Vec::new(),
            home_settings: Vec::new(),
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

    /// The spec for the provision step: same sandbox and mounts, no display,
    /// and the provision lines as the command.
    pub fn provision_spec(&self) -> Option<SandboxSpec> {
        if self.provision.is_empty() {
            return None;
        }
        let mut p = self.clone();
        p.gui = Gui::default();
        p.setup.clear();
        p.command = vec![
            "/bin/sh".into(),
            "-c".into(),
            format!("set -e\n{}", self.provision.join("\n")),
        ];
        Some(p)
    }

    /// The spec that installs `mise_shared` into the shared mise installs. It
    /// is a new sandbox, not a copy of this one: the home is the empty
    /// directory `scratch`, and the project, its mise files, the sandbox home,
    /// the manifest settings and the other caches are absent. The shared
    /// installs are the one writable mount, and mise gets the tool list on its
    /// command line, so no project file can run code while they are writable.
    pub fn mise_shared_spec(&self, scratch: PathBuf) -> Option<SandboxSpec> {
        if self.mise_shared.is_empty() {
            return None;
        }
        let dst = self.home.join(MISE_SHARED_INSTALLS);
        let shared = self
            .mounts
            .iter()
            .find(|m| m.dst == dst && m.is_bind())?
            .src
            .clone()?;
        let mise = self.host_tools.iter().find(|t| t.name == "mise")?;
        let mut p = SandboxSpec::new(
            self.id.clone(),
            &self.harness,
            &self.home,
            scratch,
            &self.home,
            self.uid,
            self.gid,
            &self.user,
        );
        p.network = self.network;
        p.allow_domains = self.allow_domains.clone();
        // The host proxy is the only host service the downloads need.
        for (k, v) in &self.env {
            if v.origin.starts_with("network: host proxy") {
                p.env.insert(k.clone(), v.clone());
            }
        }
        for (port, origin) in &self.host_ports {
            if origin.starts_with("network: host proxy") {
                p.add_host_port(*port, origin);
            }
        }
        p.add_tool("mise", mise.src.clone(), &mise.origin);
        p.add_mount(Mount::bind(shared, &dst, true, "mise shared install step"));
        let origin = "mise shared install step";
        let data = dst.parent().unwrap_or(&dst).display().to_string();
        p.set_env("MISE_DATA_DIR", data, origin);
        p.set_env("MISE_YES", "1", origin);
        p.set_env(
            "MISE_DISABLE_BACKENDS",
            crate::mise::DISABLED_BACKENDS.join(","),
            origin,
        );
        p.command = vec![format!("{TOOL_DIR}/mise"), "install".into()];
        p.command.extend(self.mise_shared.iter().cloned());
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
        dirs.extend(self.tool_path.iter().map(|p| p.display().to_string()));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mise_shared_spec_has_no_project() {
        let home = PathBuf::from("/home/demo");
        let project = PathBuf::from("/home/demo/src/app");
        let mut s = SandboxSpec::new(
            "id".into(),
            "claude",
            &project,
            "/data/homes/x".into(),
            &home,
            1000,
            1000,
            "demo",
        );
        let dst = home.join(MISE_SHARED_INSTALLS);
        s.add_mount(Mount::same(&project, true, "project"));
        s.add_mount(Mount::bind(
            "/cache/shared/npm",
            home.join(".npm"),
            true,
            "npm",
        ));
        s.add_mount(Mount::bind("/cache/shared/mise-tools", &dst, false, "mise"));
        s.add_tool("mise", "/usr/bin/mise".into(), "detector mise");
        s.set_env("MISE_TRUSTED_CONFIG_PATHS", "/home/demo/src/app", "mise");
        s.set_env("PROJECT_VAR", "1", "manifest env.set");
        s.set_secret_env(
            "HTTPS_PROXY",
            "http://127.0.0.1:3128",
            "network: host proxy",
        );
        s.add_host_port(3128, "network: host proxy in HTTPS_PROXY");
        s.add_host_port(5432, "manifest network.host_ports");
        s.setup.push("echo setup".into());
        s.provision.push("mise install".into());
        assert!(s.mise_shared_spec("/scratch".into()).is_none());

        s.mise_shared = vec!["node@22".into(), "jq@1.7.1".into()];
        let p = s.mise_shared_spec("/scratch".into()).unwrap();
        assert_eq!(p.home_src, PathBuf::from("/scratch"));
        assert_eq!(p.workdir, home);
        assert_eq!(p.mounts.len(), 1);
        assert_eq!(p.mounts[0].dst, dst);
        assert_eq!(p.mounts[0].kind, MountKind::Rw);
        assert!(p.setup.is_empty() && p.provision.is_empty());
        assert!(!p.env.contains_key("MISE_TRUSTED_CONFIG_PATHS"));
        assert!(!p.env.contains_key("PROJECT_VAR"));
        assert!(p.env.contains_key("HTTPS_PROXY"));
        assert_eq!(p.host_ports.len(), 1);
        assert_eq!(
            p.env["MISE_DATA_DIR"].value,
            "/home/demo/.local/share/paddock-mise"
        );
        assert_eq!(
            p.command,
            ["/run/paddock/bin/mise", "install", "node@22", "jq@1.7.1"]
        );

        // The project's own provision step keeps the shared installs read-only.
        let own = s.provision_spec().unwrap();
        let m = own.mounts.iter().find(|m| m.dst == dst).unwrap();
        assert_eq!(m.kind, MountKind::Ro);
    }
}
