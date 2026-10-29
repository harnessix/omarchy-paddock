//! `paddock.toml`: explicit choices for one project, plus the same schema in
//! the user configuration file for defaults.
//!
//! The project file lives inside the writable project, so an agent can edit it.
//! Any field that widens access therefore needs a one-time approval on the host.

use crate::env::HostEnv;
use crate::spec::{GuiMode, Network};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

pub const PROJECT_FILE: &str = "paddock.toml";

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub default_harness: Option<String>,
    pub backend: Option<String>,
    #[serde(default)]
    pub mounts: Mounts,
    #[serde(default)]
    pub secrets: Secrets,
    #[serde(default)]
    pub network: NetworkCfg,
    #[serde(default)]
    pub gui: GuiCfg,
    #[serde(default)]
    pub env: EnvCfg,
    #[serde(default)]
    pub docker: DockerCfg,
    #[serde(default)]
    pub setup: SetupCfg,
    #[serde(default)]
    pub auth: AuthCfg,
    #[serde(default)]
    pub mcp: McpCfg,
    /// Host passthrough modes. Read by broker.rs from the user configuration
    /// only; a project file cannot set them.
    #[serde(default, skip_serializing)]
    pub host: Option<toml::Value>,
    /// Desktop apps (`paddock app`). Read from the user configuration only.
    #[serde(default)]
    pub apps: BTreeMap<String, AppCfg>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AppCfg {
    /// Workspace folders the app can open, read-write. The first one is the
    /// project for settings and the sandbox home.
    #[serde(default)]
    pub roots: Vec<String>,
    /// Bind GPU render nodes.
    pub gpu: Option<bool>,
    /// Forward ports that the app listens on to 127.0.0.1 on the host, while
    /// it listens, so login callbacks from the host browser reach it. Any
    /// local process can connect to them meanwhile. Off by default.
    #[serde(default)]
    pub login_ports: bool,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AuthCfg {
    /// shared (default), isolated, or isolated:NAME.
    pub mode: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct McpCfg {
    /// MCP servers whose host paths outside tool install directories may be
    /// mounted read-only.
    #[serde(default)]
    pub allow: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Mounts {
    #[serde(default)]
    pub rw: Vec<String>,
    #[serde(default)]
    pub ro: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Secrets {
    /// Extra project paths to hide.
    #[serde(default)]
    pub hide: Vec<String>,
    /// Project paths that the secrets detector must not hide.
    #[serde(default)]
    pub reveal: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkCfg {
    pub mode: Option<Network>,
    #[serde(default)]
    pub publish: Vec<u16>,
    /// Host loopback ports the sandbox can reach, for example a local MCP server.
    #[serde(default)]
    pub host_ports: Vec<u16>,
    /// Extra domains for network mode `allowlist`, besides the harness's own.
    #[serde(default)]
    pub allow: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GuiCfg {
    /// off | headless | window | host. Unset lets the desktop detector decide.
    pub mode: Option<GuiMode>,
    /// Bind GPU render nodes.
    pub gpu: Option<bool>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EnvCfg {
    /// Pass the host environment (default true). False passes only the
    /// allowlist: locale, terminal, the profile's env_pass, and `pass`.
    pub inherit: Option<bool>,
    /// Host variables that are not inherited. `paddock env hide` edits this list.
    #[serde(default)]
    pub hide: Vec<String>,
    /// Host variables to pass through.
    #[serde(default)]
    pub pass: Vec<String>,
    /// Fixed variables.
    #[serde(default)]
    pub set: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DockerCfg {
    pub image: Option<String>,
    #[serde(default)]
    pub packages: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SetupCfg {
    /// Run `mise install` inside the sandbox when the project has a mise file.
    pub mise_install: Option<bool>,
    /// Extra shell lines to run inside the sandbox before the harness.
    #[serde(default)]
    pub run: Vec<String>,
}

pub struct Loaded {
    pub merged: Manifest,
    pub project: Option<(PathBuf, Manifest)>,
}

impl Manifest {
    pub fn read(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)?;
        toml::from_str(&text).with_context(|| format!("{} is invalid", path.display()))
    }

    /// User configuration first, then the project file on top of it.
    pub fn load(env: &HostEnv, project: &Path) -> Result<Loaded> {
        let user_file = env.paddock_config().join("config.toml");
        let user = if user_file.is_file() {
            Self::read(&user_file)?
        } else {
            Self::default()
        };
        let project_file = project.join(PROJECT_FILE);
        let proj = if project_file.is_file() {
            Some((project_file.clone(), Self::read(&project_file)?))
        } else {
            None
        };
        let mut merged = user;
        if let Some((_, p)) = &proj {
            merged.merge(p.clone());
        }
        Ok(Loaded {
            merged,
            project: proj,
        })
    }

    fn merge(&mut self, o: Manifest) {
        // [host] and [apps] are trusted only from the user configuration.
        if o.host.is_some() || !o.apps.is_empty() {
            eprintln!("paddock: warning: [host] and [apps] in a project paddock.toml are ignored");
        }
        if o.default_harness.is_some() {
            self.default_harness = o.default_harness;
        }
        if o.backend.is_some() {
            self.backend = o.backend;
        }
        self.mounts.rw.extend(o.mounts.rw);
        self.mounts.ro.extend(o.mounts.ro);
        self.secrets.hide.extend(o.secrets.hide);
        self.secrets.reveal.extend(o.secrets.reveal);
        if o.network.mode.is_some() {
            self.network.mode = o.network.mode;
        }
        self.network.publish.extend(o.network.publish);
        self.network.host_ports.extend(o.network.host_ports);
        self.network.allow.extend(o.network.allow);
        if o.auth.mode.is_some() {
            self.auth.mode = o.auth.mode;
        }
        self.mcp.allow.extend(o.mcp.allow);
        if o.gui.mode.is_some() {
            self.gui.mode = o.gui.mode;
        }
        if o.gui.gpu.is_some() {
            self.gui.gpu = o.gui.gpu;
        }
        if o.gui.width.is_some() {
            self.gui.width = o.gui.width;
        }
        if o.gui.height.is_some() {
            self.gui.height = o.gui.height;
        }
        if o.env.inherit.is_some() {
            self.env.inherit = o.env.inherit;
        }
        self.env.pass.extend(o.env.pass);
        self.env.hide.extend(o.env.hide);
        self.env.set.extend(o.env.set);
        if o.docker.image.is_some() {
            self.docker.image = o.docker.image;
        }
        self.docker.packages.extend(o.docker.packages);
        if o.setup.mise_install.is_some() {
            self.setup.mise_install = o.setup.mise_install;
        }
        self.setup.run.extend(o.setup.run);
    }

    /// The part of a project file that widens access. Approval covers exactly this.
    pub fn widening(&self) -> serde_json::Value {
        serde_json::json!({
            "mounts": { "rw": self.mounts.rw, "ro": self.mounts.ro },
            "secrets.reveal": self.secrets.reveal,
            "network.host": self.network.mode == Some(Network::Host),
            "network.publish": self.network.publish,
            "network.host_ports": self.network.host_ports,
            "mcp.allow": self.mcp.allow,
            "auth.shared": self.auth.mode.as_deref() == Some("shared"),
            "env.inherit": self.env.inherit == Some(true),
            "gui.host": self.gui.mode == Some(GuiMode::Host),
            "gui.gpu": self.gui.gpu == Some(true),
            "env.pass": self.env.pass,
            "setup.run": self.setup.run,
            "docker": { "image": self.docker.image, "packages": self.docker.packages },
        })
    }

    pub fn widens(&self) -> bool {
        !self.mounts.rw.is_empty()
            || !self.mounts.ro.is_empty()
            || !self.secrets.reveal.is_empty()
            || self.network.mode == Some(Network::Host)
            || !self.network.publish.is_empty()
            || !self.network.host_ports.is_empty()
            || !self.mcp.allow.is_empty()
            || self.auth.mode.as_deref() == Some("shared")
            || self.env.inherit == Some(true)
            || self.gui.mode == Some(GuiMode::Host)
            || self.gui.gpu == Some(true)
            || !self.env.pass.is_empty()
            || !self.setup.run.is_empty()
            || self.docker.image.is_some()
            || !self.docker.packages.is_empty()
    }
}

fn approval_file(env: &HostEnv, project_file: &Path) -> PathBuf {
    let key = hex(&Sha256::digest(project_file.to_string_lossy().as_bytes()))[..16].to_string();
    env.paddock_state().join("approved").join(key)
}

fn widening_hash(m: &Manifest) -> String {
    hex(&Sha256::digest(m.widening().to_string().as_bytes()))
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn is_approved(env: &HostEnv, project_file: &Path, m: &Manifest) -> bool {
    std::fs::read_to_string(approval_file(env, project_file))
        .map(|s| s.trim() == widening_hash(m))
        .unwrap_or(false)
}

pub fn approve(env: &HostEnv, project_file: &Path, m: &Manifest) -> Result<()> {
    let f = approval_file(env, project_file);
    std::fs::create_dir_all(f.parent().unwrap())?;
    std::fs::write(f, widening_hash(m))?;
    Ok(())
}

/// Make sure that any widening in the project file is approved. Ask on a
/// terminal; refuse without one.
pub fn require_approval(env: &HostEnv, loaded: &Loaded, assume_yes: bool) -> Result<()> {
    let Some((file, m)) = &loaded.project else {
        return Ok(());
    };
    if !m.widens() || is_approved(env, file, m) {
        return Ok(());
    }
    let summary = serde_json::to_string_pretty(&m.widening())?;
    eprintln!(
        "paddock: {} asks for more access than the defaults:\n{summary}\n\
         The project directory is writable from inside the sandbox, so an agent can edit this file.",
        file.display()
    );
    if assume_yes {
        return approve(env, file, m);
    }
    if !std::io::stdin().is_terminal() {
        bail!(
            "not approved. Review the file, then run: paddock approve {}",
            file.parent().unwrap().display()
        );
    }
    eprint!("Approve this access? [y/N] ");
    std::io::stderr().flush().ok();
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    if matches!(answer.trim(), "y" | "Y" | "yes") {
        approve(env, file, m)
    } else {
        bail!("not approved")
    }
}

pub const TEMPLATE: &str = r#"# paddock.toml: sandbox settings for this project.
# Fields that widen access need a one-time approval on the host.

# default_harness = "claude"
# backend = "auto"            # auto | bwrap | docker

[mounts]
# rw = []                     # extra writable host paths
# ro = ["../shared-protos"]   # extra read-only host paths

[secrets]
# hide = ["secrets/"]         # hide more project paths
# reveal = [".env.test"]      # do not hide these

[network]
# mode = "egress"             # none | egress | allowlist | host
# publish = [5173]            # ports reachable from the host
# allow = ["crates.io"]       # extra domains for mode = "allowlist" (srt backend)
# host_ports = [8765]         # host loopback ports reachable from the sandbox (local MCP servers)

[auth]
# mode = "shared"             # shared: your login | isolated | isolated:NAME: a separate login

[mcp]
# allow = ["my-server"]       # MCP servers whose host files may be mounted read-only

[gui]
# mode = "headless"           # off | headless | window | host
#                             # headless: private display inside the sandbox (default for desktop projects)
#                             # window:   a nested compositor window on your desktop
#                             # host:     your real desktop; the agent can read the screen and type
# gpu = false                 # bind /dev/dri render nodes
# width = 1600
# height = 1000

[env]
# inherit = true              # pass the host environment; false passes only the allowlist
# pass = []                   # host variables to pass through when inherit = false
# set = { RUST_LOG = "info" }

[setup]
# mise_install = true
# run = []

[docker]
# image = "debian:stable"
# packages = ["libssl-dev"]
"#;
