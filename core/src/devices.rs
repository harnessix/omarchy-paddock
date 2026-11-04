//! Devices: host services that a sandbox uses as files in `/dev/paddock`.
//!
//! Each sandbox gets its own small FUSE file system. The paddock process on
//! the host serves it and mounts it in the run folder of the sandbox, without
//! root: `fusermount3` does the mount for the user. The backend then binds
//! that folder to `/dev/paddock` inside the sandbox, before the harness
//! starts. Because each sandbox has its own file system, the mount itself
//! tells paddock which sandbox calls. No request carries an identity.
//!
//! The protocol is the same for every device. One `open` is one request:
//! open the device read-write, write the whole request, then read the
//! response until end of file. The first read sends the request. A read
//! blocks until the response is ready or the deadline of the device passes
//! (then it fails with ETIMEDOUT). Closing the file early cancels the
//! request. `/dev/paddock/.catalog` is read-only and describes each device:
//! its request format, its limits and whether it is on.
//!
//! Waiting devices (`wait`, `ask`, see blocking.rs) block until an event:
//! a time, or the user's answer. Each read waits at most the `timeout` of
//! its request; when that passes first, the read fails with ETIME ("still
//! waiting", `crate::waiting`), and the agent sends the same request again.
//!
//! A sandbox that another one started (`spawn`) gets at most the devices
//! of its parent: the parent's paddock records them in the child's job, and
//! the child's own `/dev/paddock` serves only those that the user
//! configuration still turns on.
//!
//! `/dev/paddock/secret/NAME` is not a device: it holds the value of a
//! `deliver` credential with `as = "file"` (credentials/secret.rs). A plain
//! read returns the value. Each open counts as one use, and after
//! `paddock credentials revoke` the file fails with EACCES.
//!
//! Devices are off unless `[devices]` in the user configuration turns them
//! on. The project `paddock.toml` cannot do this, because the agent can edit
//! that file. Requests and responses never go into logs or process
//! arguments; the log keeps the device name, the sizes and the outcome.

use crate::credentials::secret::SecretFiles;
use crate::env::HostEnv;
use crate::spec::SandboxSpec;
use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub mod blocking;
pub mod command;
pub mod ledger;
pub mod user;

#[cfg(target_os = "linux")]
mod fuse;
#[cfg(target_os = "linux")]
use fuse::lazy_unmount;
#[cfg(target_os = "linux")]
pub use fuse::{mount, DeviceFs, Mounted};

#[cfg(not(target_os = "linux"))]
mod fuse_stub;
#[cfg(not(target_os = "linux"))]
use fuse_stub::lazy_unmount;
#[cfg(not(target_os = "linux"))]
pub use fuse_stub::{mount, DeviceFs, Mounted};

/// Where the devices appear inside the sandbox.
pub const MOUNT_IN_SANDBOX: &str = "/dev/paddock";
/// The read-only description of the devices.
pub const CATALOG: &str = ".catalog";
/// The folder of the secret files.
pub const SECRET_DIR: &str = "secret";

/// Limits of one device. The catalog shows them.
#[derive(Debug, Clone, Serialize)]
pub struct Limits {
    pub max_request: usize,
    pub max_response: usize,
    /// The longest time a request can take before its read fails.
    pub deadline_seconds: u64,
}

/// What `.catalog` says about one device.
#[derive(Debug, Clone, Serialize)]
pub struct Info {
    /// The format of the request bytes.
    pub request: String,
    /// The format of the response bytes.
    pub response: String,
    pub limits: Limits,
}

/// Why a request failed. The reader gets the errno.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DevError {
    /// The request does not have the format the device expects (EINVAL).
    Invalid,
    /// The device cannot run the request now (EIO).
    Failed,
    /// The response is larger than the device limit (EMSGSIZE).
    TooLarge,
    /// The client closed the file or the deadline passed (ECANCELED).
    Cancelled,
    /// The time limit of this read passed before the event (ETIME, "still
    /// waiting"). The client sends the same request again.
    Waiting,
    /// The user declined, for example dismissed a question (EACCES).
    Denied,
    /// The budget of a paid device is used (EDQUOT).
    BudgetUsed,
}

impl DevError {
    pub fn errno(self) -> i32 {
        match self {
            DevError::Invalid => libc::EINVAL,
            DevError::Failed => libc::EIO,
            DevError::TooLarge => libc::EMSGSIZE,
            DevError::Cancelled => libc::ECANCELED,
            DevError::Waiting => crate::waiting::ERRNO_STILL_WAITING,
            DevError::Denied => libc::EACCES,
            DevError::BudgetUsed => libc::EDQUOT,
        }
    }
}

/// What a running request can see: the calling sandbox and whether the
/// client still waits.
pub struct Call {
    /// The sandbox id. It comes from the mount, never from the request.
    pub sandbox: String,
    pub deadline: Instant,
    cancel: Arc<AtomicBool>,
}

impl Call {
    /// A call for tests of a device without a mount.
    pub fn for_test(sandbox: &str, deadline: Duration) -> Self {
        Self {
            sandbox: sandbox.to_string(),
            deadline: Instant::now() + deadline,
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    /// True when the client closed the file or the deadline passed. A device
    /// that works for a long time checks this and stops early.
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst) || Instant::now() >= self.deadline
    }
}

/// One device. A backend runs on the host with the rights of the user.
pub trait Device: Send + Sync {
    fn name(&self) -> &str;
    fn info(&self) -> Info;
    fn call(&self, call: &Call, request: &[u8]) -> Result<Vec<u8>, DevError>;
    /// "on", or "off" for the file of a user device that is not turned on.
    fn state(&self) -> &'static str {
        "on"
    }
    /// More fields for the catalog entry, for example the configuration of
    /// a user device.
    fn details(&self) -> Option<serde_json::Value> {
        None
    }
}

/// The built-in test device: the response is the request.
pub struct Echo;

impl Device for Echo {
    fn name(&self) -> &str {
        "echo"
    }
    fn info(&self) -> Info {
        Info {
            request: "any bytes".into(),
            response: "the same bytes".into(),
            limits: Limits {
                max_request: 64 * 1024,
                max_response: 64 * 1024,
                deadline_seconds: 10,
            },
        }
    }
    fn call(&self, _call: &Call, request: &[u8]) -> Result<Vec<u8>, DevError> {
        Ok(request.to_vec())
    }
}

/// What a device may know about the sandbox it serves, from the host. The
/// `ask` device needs it for the approval card.
#[derive(Clone)]
pub struct DeviceCtx {
    pub env: HostEnv,
    pub sandbox: String,
    pub harness: String,
    pub project: PathBuf,
    /// The sandboxes above this one, root first, the parent last.
    pub started_by: Vec<String>,
    /// Who answers the approval card of a user device.
    pub approver: Arc<dyn crate::broker::Approver>,
}

/// A built-in device, ready to serve.
pub fn builtin(name: &str, ctx: &DeviceCtx) -> Option<Arc<dyn Device>> {
    match name {
        "echo" => Some(Arc::new(Echo)),
        "wait" => Some(Arc::new(blocking::Wait)),
        "ask" => Some(blocking::Ask::start(ctx.clone())),
        _ => None,
    }
}

/// The description of a built-in device, for the catalog when it is off.
pub fn builtin_info(name: &str) -> Option<Info> {
    match name {
        "echo" => Some(Echo.info()),
        "wait" => Some(blocking::Wait.info()),
        "ask" => Some(blocking::ask_info()),
        _ => None,
    }
}

pub const BUILTIN: &[&str] = &["echo", "ask", "wait"];

/// `[devices]` of `~/.config/paddock/config.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevicesCfg {
    /// Mount `/dev/paddock` at all. On by default: an empty mount costs
    /// nothing and changes no harness.
    pub mount: bool,
    /// Devices that the user turned on.
    pub enabled: Vec<String>,
    /// `[devices.NAME]` tables: http and command devices, on or off.
    pub user: Vec<user::UserDev>,
}

impl Default for DevicesCfg {
    fn default() -> Self {
        Self {
            mount: true,
            enabled: Vec::new(),
            user: Vec::new(),
        }
    }
}

impl DevicesCfg {
    pub fn load(env: &HostEnv) -> Result<Self> {
        let file = env.paddock_config().join("config.toml");
        let Ok(text) = std::fs::read_to_string(&file) else {
            return Ok(Self::default());
        };
        Self::parse(&text).with_context(|| format!("[devices] in {} is invalid", file.display()))
    }

    pub fn parse(text: &str) -> Result<Self> {
        let v: toml::Value = toml::from_str(text)?;
        let mut cfg = Self::default();
        let Some(t) = v.get("devices") else {
            return Ok(cfg);
        };
        let t = t.as_table().context("[devices] must be a table")?;
        for (k, v) in t {
            if let Some(table) = v.as_table() {
                let d = user::UserDev::parse(k, table)?;
                if d.enabled {
                    cfg.enabled.push(k.clone());
                }
                cfg.user.push(d);
                continue;
            }
            let on = v.as_bool().with_context(|| {
                format!("[devices] {k} must be true or false, or a [devices.{k}] table")
            })?;
            if k == "mount" {
                cfg.mount = on;
                continue;
            }
            if !BUILTIN.contains(&k.as_str()) {
                bail!(
                    "[devices] {k}: unknown device. Known: {}",
                    BUILTIN.join(", ")
                );
            }
            if on && !cfg.enabled.contains(k) {
                cfg.enabled.push(k.clone());
            }
        }
        Ok(cfg)
    }
}

/// The text of `.catalog`: every known device, with its state.
pub fn catalog(on: &[Arc<dyn Device>]) -> Vec<u8> {
    catalog_with(on, &[])
}

/// The catalog, with the secret files of the sandbox by name.
pub fn catalog_with(on: &[Arc<dyn Device>], secrets: &[String]) -> Vec<u8> {
    let mut devices = Vec::new();
    let mut names: Vec<&str> = on.iter().map(|d| d.name()).collect();
    for b in BUILTIN {
        if !names.contains(b) {
            names.push(b);
        }
    }
    for name in names {
        let served = on.iter().find(|d| d.name() == name);
        let (state, info) = match served {
            Some(d) => (d.state(), d.info()),
            None => match builtin_info(name) {
                Some(i) => ("off", i),
                None => continue,
            },
        };
        let mut entry = serde_json::json!({
            "name": name,
            "state": state,
            "request": info.request,
            "response": info.response,
            "limits": info.limits,
        });
        if let Some(serde_json::Value::Object(more)) = served.and_then(|d| d.details()) {
            for (k, v) in more {
                entry[k] = v;
            }
        }
        if served.is_some() {
            entry["path"] = format!("{MOUNT_IN_SANDBOX}/{name}").into();
        }
        if state != "on" && entry.get("how_to_enable").is_none() {
            entry["how_to_enable"] = format!(
                "the user sets [devices] {name} = true in ~/.config/paddock/config.toml on the host"
            )
            .into();
        }
        devices.push(entry);
    }
    let mut doc = serde_json::json!({
        "version": 1,
        "protocol": "Open a device read-write, write the whole request, then read the response until end of file. One open is one request. The first read sends the request and blocks until the response is ready. Closing the file early cancels the request. A read fails with ETIMEDOUT after the deadline, EFBIG means the request is too large, EINVAL that the device rejected it. A waiting device (wait, ask) fails a read with ETIME (still waiting) when the timeout of the request passes before the event: send the same request again. EACCES means that the user declined. EDQUOT means that the budget of a paid device is used: the user raises it on the host. An http or command device of the user asks the user once per sandbox at its first request; an off one answers with a disabled status. `paddock-host device NAME` does all of this and exits with 75 for still waiting.",
        "devices": devices,
    });
    if !secrets.is_empty() {
        doc["secrets"] = secrets
            .iter()
            .map(|n| {
                serde_json::json!({
                    "name": n,
                    "path": crate::credentials::secret_path(n),
                })
            })
            .collect::<Vec<_>>()
            .into();
        doc["secrets_how"] = "Each file holds the value of a credential that the user gave this sandbox. Read it with a plain read, no request: cat FILE. Each open counts as one use. Use it in one command only, for example GH_TOKEN=$(cat /dev/paddock/secret/GH_TOKEN) gh pr list, or give the path to a tool that reads a token file. Do not copy the value into files or logs. EACCES means that the user revoked it.".into();
    }
    let mut out = serde_json::to_vec_pretty(&doc).unwrap_or_default();
    out.push(b'\n');
    out
}

/// Whether this host can mount a user FUSE file system: `/dev/fuse` opens
/// and `fusermount3` (or `fusermount`) exists.
#[cfg(target_os = "linux")]
pub fn fuse_available(env: &HostEnv) -> Result<PathBuf, String> {
    let helper = env
        .which("fusermount3")
        .or_else(|| env.which("fusermount"))
        .ok_or("fusermount3 is not installed (package fuse3)")?;
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/fuse")
        .map_err(|e| format!("cannot open /dev/fuse: {e}"))?;
    Ok(helper)
}

#[cfg(not(target_os = "linux"))]
pub fn fuse_available(_env: &HostEnv) -> Result<PathBuf, String> {
    Err(crate::platform::unavailable("the FUSE device mount").to_string())
}

/// Whether a backend can show a host FUSE mount inside its sandbox.
///
/// A FUSE mount answers only the user who mounted it, not even root. bwrap
/// and srt run as the user, so they bind the mount itself. The container
/// runtimes set up mounts as root or as a mapped root, which the mount
/// refuses. For them paddock binds the empty folder with slave propagation
/// and mounts the file system on it after the container started; the mount
/// then appears inside the container. That needs a run folder on a shared
/// mount, the systemd default.
pub fn backend_support(env: &HostEnv, backend: &str) -> Result<(), String> {
    match backend {
        "bwrap" | "srt" => Ok(()),
        "docker" | "podman" => {
            let run = env.paddock_run();
            if shared_mount(&run) {
                Ok(())
            } else {
                Err(format!(
                    "{} is not on a shared mount, so a later mount does not reach the container",
                    run.display()
                ))
            }
        }
        other => Err(format!("the {other} backend has no device support")),
    }
}

/// Where the device file system of a sandbox is mounted on the host. srt
/// binds the run folder of the sandbox read-only over everything in it, so
/// for srt the mount is outside that folder.
pub fn mount_dir(env: &HostEnv, backend: &str, id: &str) -> PathBuf {
    if backend == "srt" {
        env.paddock_run().join("devices").join(id)
    } else {
        env.paddock_run().join(id).join("dev")
    }
}

/// Container backends mount the devices after the start.
pub fn late(backend: &str) -> bool {
    matches!(backend, "docker" | "podman")
}

/// True when the mount that holds `path` has shared propagation.
fn shared_mount(path: &Path) -> bool {
    let Ok(info) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return false;
    };
    let mut best: Option<(usize, bool)> = None;
    for line in info.lines() {
        let f: Vec<&str> = line.split(' ').collect();
        let Some(mp) = f.get(4) else { continue };
        let mp = Path::new(mp);
        if !path.starts_with(mp) {
            continue;
        }
        let len = mp.as_os_str().len();
        let shared = f[6..]
            .iter()
            .take_while(|x| **x != "-")
            .any(|x| x.starts_with("shared:"));
        if best.is_none_or(|(l, _)| len >= l) {
            best = Some((len, shared));
        }
    }
    best.is_some_and(|(_, s)| s)
}

/// The names of the devices to serve: the user's choice, plus `echo` when
/// `test` is set (`paddock doctor --escape` tests the round trip). With a
/// `ceiling` (the devices of the parent of a spawned sandbox), only names
/// that are in it.
pub fn enabled_names(cfg: &DevicesCfg, test: bool, ceiling: Option<&[String]>) -> Vec<String> {
    if !cfg.mount {
        return Vec::new();
    }
    let mut names = cfg.enabled.clone();
    if test && !names.iter().any(|n| n == "echo") {
        names.push("echo".into());
    }
    names.retain(|n| {
        (builtin_info(n).is_some() || cfg.user.iter().any(|u| u.name == *n))
            && ceiling.is_none_or(|c| c.contains(n))
    });
    names
}

/// The devices to serve. See `enabled_names`.
pub fn enabled(
    cfg: &DevicesCfg,
    test: bool,
    ceiling: Option<&[String]>,
    ctx: &DeviceCtx,
) -> Vec<Arc<dyn Device>> {
    let names = enabled_names(cfg, test, ceiling);
    names
        .iter()
        .filter_map(|n| match cfg.user.iter().find(|u| u.name == *n) {
            Some(u) => Some(u.build(ctx, true)),
            None => builtin(n, ctx),
        })
        .collect()
}

/// The files of user devices that this sandbox does not get: they answer
/// with a disabled status.
pub fn disabled(
    cfg: &DevicesCfg,
    served: &[Arc<dyn Device>],
    ctx: &DeviceCtx,
) -> Vec<Arc<dyn Device>> {
    cfg.user
        .iter()
        .filter(|u| !served.iter().any(|d| d.name() == u.name))
        .map(|u| u.build(ctx, false))
        .collect()
}

/// The devices that a sandbox gets at its start, by name, for the ceiling
/// of its children. A configuration error gives none, the same as `start`.
pub fn names_for(env: &HostEnv, test: bool, ceiling: Option<&[String]>) -> Vec<String> {
    match DevicesCfg::load(env) {
        Ok(c) => enabled_names(&c, test, ceiling),
        Err(_) => Vec::new(),
    }
}

/// The devices of one sandbox. Dropping it unmounts them.
pub struct Devices {
    pub dir: PathBuf,
    mounted: Arc<Mutex<Option<Mounted>>>,
    late: Option<(DeviceFs, String)>,
    stop: Arc<AtomicBool>,
    waiter: Option<std::thread::JoinHandle<()>>,
}

impl Devices {
    /// For container backends: wait until the container `paddock-<id>`
    /// runs, then mount. The container waits a few seconds for the
    /// catalog before the harness starts.
    pub fn after_start(&mut self, env: &HostEnv, backend: &str, id: &str) {
        let Some((fs, _)) = self.late.take() else {
            return;
        };
        let Some(cli) = env.which(backend) else {
            return;
        };
        let (dir, mounted, stop) = (self.dir.clone(), self.mounted.clone(), self.stop.clone());
        let name = format!("paddock-{id}");
        self.waiter = Some(std::thread::spawn(move || {
            let start = Instant::now();
            while !stop.load(Ordering::SeqCst) && start.elapsed() < Duration::from_secs(120) {
                let running = std::process::Command::new(&cli)
                    .args(["inspect", "-f", "{{.State.Running}}", &name])
                    .stdin(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .output()
                    .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim() == "true");
                if running {
                    match mount(&dir, fs) {
                        Ok(m) => *mounted.lock().unwrap() = Some(m),
                        Err(e) => log::warn!("cannot mount the devices: {e:#}"),
                    }
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }));
    }
}

impl Drop for Devices {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(w) = self.waiter.take() {
            let _ = w.join();
        }
        if let Some(m) = self.mounted.lock().unwrap().take() {
            m.unmount();
        }
        let _ = std::fs::remove_dir(&self.dir);
    }
}

/// At sandbox start: mount the devices of this sandbox in its run folder and
/// record the folder in the spec, for the backend. Container backends get
/// the empty folder now and the mount after the start (`after_start`). A
/// problem is a notice, not an error: the sandbox starts without devices.
/// `ceiling` holds the devices of the parent, for a spawned sandbox, and
/// `started_by` the sandboxes above it. `secrets` holds the secret files.
#[allow(clippy::too_many_arguments)]
pub fn start(
    env: &HostEnv,
    spec: &mut SandboxSpec,
    backend: &str,
    test: bool,
    ceiling: Option<&[String]>,
    started_by: &[String],
    gateway: Option<crate::gateway::Handler>,
    secrets: Arc<SecretFiles>,
) -> Option<Devices> {
    let cfg = match DevicesCfg::load(env) {
        Ok(c) => c,
        Err(e) => {
            spec.notices.push(format!("devices are off: {e:#}"));
            return None;
        }
    };
    if !cfg.mount {
        return None;
    }
    let ctx = DeviceCtx {
        env: env.clone(),
        sandbox: spec.id.clone(),
        harness: spec.harness.clone(),
        project: spec.project.clone(),
        started_by: started_by.to_vec(),
        approver: Arc::new(crate::broker::HostApprover),
    };
    let mut devices = enabled(&cfg, test, ceiling, &ctx);
    // Without an enabled device the mount is only a catalog, so a missing
    // FUSE is not worth a notice at each start. The gateway and the files
    // of off devices do not count: without the mount, paddock-host uses
    // the host socket.
    let quiet = devices.is_empty();
    let off = disabled(&cfg, &devices, &ctx);
    devices.extend(off);
    if let Some(h) = gateway {
        devices.push(Arc::new(crate::gateway::GwDevice::new(&spec.id, h)));
    }
    let tell = |spec: &mut SandboxSpec, why: String| {
        if quiet {
            log::debug!("no /dev/paddock: {why}");
        } else {
            spec.notices
                .push(format!("devices are off in this sandbox: {why}"));
        }
    };
    if let Err(why) = fuse_available(env) {
        tell(spec, why);
        return None;
    }
    clear_stale(env);
    if let Err(why) = backend_support(env, backend) {
        tell(spec, why);
        return None;
    }
    let dir = mount_dir(env, backend, &spec.id);
    let fs = DeviceFs::with_secrets(&spec.id, devices, secrets);
    let mut out = Devices {
        dir: dir.clone(),
        mounted: Arc::new(Mutex::new(None)),
        late: None,
        stop: Arc::new(AtomicBool::new(false)),
        waiter: None,
    };
    if late(backend) {
        if let Err(e) = crate::privfs::dir(&dir) {
            tell(spec, format!("cannot create {}: {e}", dir.display()));
            return None;
        }
        out.late = Some((fs, backend.to_string()));
    } else {
        match mount(&dir, fs) {
            Ok(m) => *out.mounted.lock().unwrap() = Some(m),
            Err(e) => {
                tell(spec, format!("{e:#}"));
                return None;
            }
        }
    }
    spec.devices = Some(dir);
    Some(out)
}

/// Unmount device file systems of sandboxes that no longer run. A paddock
/// process that was killed leaves its mount behind, and every access to it
/// fails until it is unmounted.
pub fn clear_stale(env: &HostEnv) {
    let Ok(info) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return;
    };
    let run = env.paddock_run();
    let running: Vec<String> = crate::state::Running::list(env)
        .into_iter()
        .map(|r| r.id)
        .collect();
    for line in info.lines() {
        let f: Vec<&str> = line.split(' ').collect();
        let Some(sep) = f.iter().position(|x| *x == "-") else {
            continue;
        };
        if f.get(sep + 1) != Some(&"fuse.paddock") {
            continue;
        }
        let mp = Path::new(f[4]);
        let Ok(rel) = mp.strip_prefix(&run) else {
            continue;
        };
        let mut parts = rel.components();
        let (Some(id), Some(dev), None) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        // <id>/dev, or devices/<id> for srt.
        let (id, dev) = if id.as_os_str() == "devices" {
            (
                dev.as_os_str().to_string_lossy(),
                std::ffi::OsStr::new("dev"),
            )
        } else {
            (id.as_os_str().to_string_lossy(), dev.as_os_str())
        };
        if dev == "dev" && !running.iter().any(|r| *r == id) {
            log::debug!("unmounting the stale device mount {}", mp.display());
            lazy_unmount(mp);
        }
    }
}

/// For `paddock doctor`: one line per backend.
pub fn report(env: &HostEnv) -> BTreeMap<&'static str, Result<(), String>> {
    let mut out = BTreeMap::new();
    let fuse = fuse_available(env).map(|_| ());
    for b in ["bwrap", "podman", "docker", "srt"] {
        out.insert(b, fuse.clone().and_then(|_| backend_support(env, b)));
    }
    out
}

/// The lines of `paddock doctor` about devices.
pub fn doctor_lines(env: &HostEnv) -> Vec<String> {
    let mut v = Vec::new();
    match fuse_available(env) {
        Ok(helper) => v.push(format!(
            "  ok       FUSE: {} and /dev/fuse",
            helper.display()
        )),
        Err(e) => v.push(format!(
            "  no       FUSE: {e}. Sandboxes start without /dev/paddock"
        )),
    }
    for (b, r) in report(env) {
        match r {
            Ok(()) if b == "srt" => v.push(format!(
                "  ok       devices on {b}: at the path in $PADDOCK_DEVICES, not /dev/paddock"
            )),
            Ok(()) if late(b) => v.push(format!(
                "  ok       devices on {b}: /dev/paddock, mounted after the container starts"
            )),
            Ok(()) => v.push(format!("  ok       devices on {b}: /dev/paddock")),
            Err(e) => v.push(format!("  no       devices on {b}: {e}")),
        }
    }
    match DevicesCfg::load(env) {
        Ok(c) if !c.mount => v.push("  off      /dev/paddock: [devices] mount = false".into()),
        Ok(c) if c.enabled.is_empty() => v.push(
            "  off      no device is on; turn one on with [devices] NAME = true in the user configuration".into(),
        ),
        Ok(c) => v.push(format!("  on       devices: {}", c.enabled.join(", "))),
        Err(e) => v.push(format!("  error    {e:#}")),
    }
    v
}

#[cfg(test)]
mod tests;
