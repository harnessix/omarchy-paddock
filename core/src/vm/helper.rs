//! The plain logic of the `paddock-vm` helper: the arguments, the state
//! folder, the machine identifier file and the checks before a boot. It
//! runs on every system, so the tests run everywhere. The Virtualization
//! framework code is in `vz.rs`.

use super::{assets, ports};
use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The size of the writable disk of the guest, created sparse.
pub const STATE_DISK_BYTES: u64 = 2 << 30;
/// The virtiofs tag of the project folder.
pub const PROJECT_TAG: &str = "project";
/// The longest machine identifier file that the helper reads.
const MACHINE_ID_MAX: u64 = 4096;

#[derive(Debug, Parser)]
#[command(
    name = "paddock-vm",
    version,
    about = "Run the Linux VM of a paddock sandbox (macOS)"
)]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// Boot the VM and run until the guest stops or the helper gets SIGTERM.
    Start(StartArgs),
}

#[derive(Debug, Clone, clap::Args)]
pub struct StartArgs {
    /// The folder with the guest files and assets.toml.
    #[arg(long)]
    pub assets: PathBuf,
    /// The project folder. It is the only host folder in the VM.
    #[arg(long)]
    pub project: PathBuf,
    /// The folder for the machine identifier, the guest disk and the
    /// console log.
    #[arg(long)]
    pub state: PathBuf,
    /// The control socket for the parent paddock. The default is
    /// `control.sock` in the state folder. paddock passes a short path:
    /// a unix socket path on macOS has at most 103 bytes.
    #[arg(long, value_name = "PATH")]
    pub control: Option<PathBuf>,
    #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u32).range(1..=64))]
    pub cpus: u32,
    /// The memory of the VM, in MiB.
    #[arg(long, default_value_t = 2048, value_parser = clap::value_parser!(u64).range(256..=262144))]
    pub memory: u64,
    /// Send guest connections to a host vsock port to a unix socket:
    /// `PORT=SOCKET`, for example `1025=/tmp/broker.sock` or `broker=...`.
    #[arg(long = "forward", value_name = "PORT=SOCKET", value_parser = parse_forward)]
    pub forwards: Vec<(u32, PathBuf)>,
    /// Seconds between the stop request to the guest and the hard stop.
    #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u64).range(1..=600))]
    pub stop_timeout: u64,
    /// One more argument for the kernel command line.
    #[arg(long, value_name = "ARG", allow_hyphen_values = true)]
    pub append: Vec<String>,
}

fn parse_forward(text: &str) -> Result<(u32, PathBuf), String> {
    let (port, path) = text
        .split_once('=')
        .ok_or_else(|| format!("{text:?} is not PORT=SOCKET"))?;
    let port = ports::parse(port).ok_or_else(|| {
        let known: Vec<String> = ports::ALL
            .iter()
            .map(|(p, n)| format!("{p} ({n})"))
            .collect();
        format!("{port:?} is not a paddock VM port: {}", known.join(", "))
    })?;
    let path = PathBuf::from(path);
    if !path.is_absolute() {
        return Err(format!(
            "the socket {} is not an absolute path",
            path.display()
        ));
    }
    crate::sockpath::check(&path).map_err(|e| e.to_string())?;
    Ok((port, path))
}

/// The files of the state folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateLayout {
    pub dir: PathBuf,
    /// The `VZGenericMachineIdentifier` of the VM, as its data.
    pub machine_id: PathBuf,
    /// The writable disk of the guest, raw and sparse.
    pub disk: PathBuf,
    /// The serial console of the guest.
    pub console: PathBuf,
    /// The control socket for the parent paddock.
    pub control: PathBuf,
}

impl StateLayout {
    pub fn new(dir: &Path) -> Self {
        Self {
            dir: dir.to_path_buf(),
            machine_id: dir.join("machine-id"),
            disk: dir.join("disk.img"),
            console: dir.join("console.log"),
            control: dir.join("control.sock"),
        }
    }

    /// Create the folder with mode 0700, or check an existing one.
    pub fn prepare(&self) -> Result<()> {
        match std::fs::symlink_metadata(&self.dir) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => bail!("the state folder {} is not a folder", self.dir.display()),
            Err(_) => std::fs::create_dir_all(&self.dir)
                .with_context(|| format!("cannot create {}", self.dir.display()))?,
        }
        std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("cannot set the mode of {}", self.dir.display()))?;
        crate::sockpath::check(&self.control)
    }

    /// Create the guest disk on the first start, sparse. True when it is new.
    pub fn ensure_disk(&self, bytes: u64) -> Result<bool> {
        if let Ok(m) = std::fs::symlink_metadata(&self.disk) {
            if !m.is_file() {
                bail!(
                    "the guest disk {} is not a regular file",
                    self.disk.display()
                );
            }
            return Ok(false);
        }
        let f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&self.disk)
            .with_context(|| format!("cannot create {}", self.disk.display()))?;
        f.set_len(bytes)
            .with_context(|| format!("cannot size {}", self.disk.display()))?;
        Ok(true)
    }

    /// The saved machine identifier, or `None` before the first start.
    pub fn read_machine_id(&self) -> Result<Option<Vec<u8>>> {
        let path = &self.machine_id;
        let m = match std::fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
        };
        if !m.is_file() || m.len() == 0 || m.len() > MACHINE_ID_MAX {
            bail!(
                "the machine identifier {} is damaged: remove the file to make a new one",
                path.display()
            );
        }
        let data =
            std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
        Ok(Some(data))
    }

    /// Save a new machine identifier. An existing file stays as it is.
    pub fn write_machine_id(&self, data: &[u8]) -> Result<()> {
        if data.is_empty() || data.len() as u64 > MACHINE_ID_MAX {
            bail!("a machine identifier of {} bytes", data.len());
        }
        let path = &self.machine_id;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("cannot create {}", path.display()))?;
        f.write_all(data)
            .and_then(|_| f.sync_all())
            .with_context(|| format!("cannot write {}", path.display()))
    }
}

/// The project folder as an absolute, real path. Refuse the home folder and
/// any folder that contains it: the home must never be in the VM.
pub fn check_project(project: &Path, home: &Path) -> Result<PathBuf> {
    let real = project
        .canonicalize()
        .with_context(|| format!("the project folder {} is missing", project.display()))?;
    if !real.is_dir() {
        bail!("the project {} is not a folder", real.display());
    }
    let home = home.canonicalize().unwrap_or_else(|_| home.to_path_buf());
    if home.starts_with(&real) {
        bail!(
            "the project folder {} is the home folder or contains it: the VM never gets the home",
            real.display()
        );
    }
    Ok(real)
}

/// The init of the guest image: `packaging/vm/agent-init`. It starts the
/// guest agent.
pub const GUEST_INIT: &str = "/sbin/paddock-init";

/// The kernel command line: the console, the read-only root disk, the init
/// of the guest image, and the extra arguments. A later `init=` in the
/// extra arguments wins over the first one.
pub fn kernel_cmdline(extra: &[String]) -> Result<String> {
    let mut args = vec![
        "console=hvc0".to_string(),
        "root=/dev/vda".into(),
        "rootfstype=ext4".into(),
        "ro".into(),
        format!("init={GUEST_INIT}"),
    ];
    for a in extra {
        if a.is_empty() || a.chars().any(|c| c.is_whitespace() || c.is_control()) {
            bail!("the kernel argument {a:?} is empty or has a space or a control character");
        }
        args.push(a.clone());
    }
    Ok(args.join(" "))
}

/// Everything a boot needs, checked.
#[derive(Debug, Clone)]
pub struct Plan {
    pub assets: assets::Assets,
    pub project: PathBuf,
    pub state: StateLayout,
    pub cpus: u32,
    pub memory_bytes: u64,
    pub forwards: BTreeMap<u32, PathBuf>,
    pub cmdline: String,
    pub stop_timeout: Duration,
}

/// Check the arguments and the files before a boot. It does not touch the
/// state folder: `StateLayout::prepare` does that.
pub fn plan(args: &StartArgs, home: &Path) -> Result<Plan> {
    let project = check_project(&args.project, home)?;
    let assets = assets::check(&args.assets)?;
    let state_dir = absolute(&args.state)?;
    // The guest writes the project. It must not reach the files that the
    // helper trusts.
    for (what, path) in [("state", &state_dir), ("assets", &absolute(&args.assets)?)] {
        if path.starts_with(&project) || project.starts_with(path) {
            bail!(
                "the {what} folder {} and the project {} must not contain each other",
                path.display(),
                project.display()
            );
        }
    }
    let mut state = StateLayout::new(&state_dir);
    if let Some(c) = &args.control {
        if !c.is_absolute() {
            bail!("the control socket {} is not an absolute path", c.display());
        }
        crate::sockpath::check(c)?;
        state.control = c.clone();
    }
    let mut forwards = BTreeMap::new();
    for (port, path) in &args.forwards {
        if forwards.insert(*port, path.clone()).is_some() {
            bail!("port {port} has two --forward sockets");
        }
    }
    Ok(Plan {
        assets,
        project,
        state,
        cpus: args.cpus,
        memory_bytes: args.memory << 20,
        forwards,
        cmdline: kernel_cmdline(&args.append)?,
        stop_timeout: Duration::from_secs(args.stop_timeout),
    })
}

/// An absolute path, with the real path of its parent when it exists.
fn absolute(path: &Path) -> Result<PathBuf> {
    let abs =
        std::path::absolute(path).with_context(|| format!("cannot resolve {}", path.display()))?;
    if let Ok(real) = abs.canonicalize() {
        return Ok(real);
    }
    match (abs.parent(), abs.file_name()) {
        (Some(parent), Some(name)) => Ok(parent
            .canonicalize()
            .map(|p| p.join(name))
            .unwrap_or(abs.clone())),
        _ => Ok(abs),
    }
}

#[cfg(test)]
#[path = "helper_tests.rs"]
mod tests;
