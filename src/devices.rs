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
//! Devices are off unless `[devices]` in the user configuration turns them
//! on. The project `paddock.toml` cannot do this, because the agent can edit
//! that file. Requests and responses never go into logs or process
//! arguments; the log keeps the device name, the sizes and the outcome.

use crate::env::HostEnv;
use crate::spec::SandboxSpec;
use anyhow::{bail, Context, Result};
use fuser::{
    Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, INodeNo, LockOwner, MountOption,
    OpenAccMode, OpenFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyOpen, ReplyWrite, Request, WriteFlags,
};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

pub mod blocking;

/// Where the devices appear inside the sandbox.
pub const MOUNT_IN_SANDBOX: &str = "/dev/paddock";
/// The read-only description of the devices.
pub const CATALOG: &str = ".catalog";
/// Open device files at the same time, per sandbox.
const MAX_OPEN: usize = 64;
/// Requests that run at the same time, per sandbox.
const MAX_RUNNING: usize = 16;

const ROOT: u64 = 1;
const CATALOG_INO: u64 = 2;
const FIRST_DEVICE_INO: u64 = 16;
const TTL: Duration = Duration::from_secs(1);

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
}

impl DevError {
    pub fn errno(self) -> Errno {
        match self {
            DevError::Invalid => Errno::EINVAL,
            DevError::Failed => Errno::EIO,
            DevError::TooLarge => Errno::EMSGSIZE,
            DevError::Cancelled => Errno::ECANCELED,
            DevError::Waiting => Errno::from_i32(crate::waiting::ERRNO_STILL_WAITING),
            DevError::Denied => Errno::EACCES,
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
}

/// A built-in device, ready to serve.
pub fn builtin(name: &str, ctx: &DeviceCtx) -> Option<Arc<dyn Device>> {
    match name {
        "echo" => Some(Arc::new(Echo)),
        "wait" => Some(Arc::new(blocking::Wait)),
        "ask" => Some(Arc::new(blocking::Ask::new(ctx.clone()))),
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
}

impl Default for DevicesCfg {
    fn default() -> Self {
        Self {
            mount: true,
            enabled: Vec::new(),
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
            let on = v
                .as_bool()
                .with_context(|| format!("[devices] {k} must be true or false"))?;
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
    let mut devices = Vec::new();
    let mut names: Vec<&str> = on.iter().map(|d| d.name()).collect();
    for b in BUILTIN {
        if !names.contains(b) {
            names.push(b);
        }
    }
    for name in names {
        let (state, info) = match on.iter().find(|d| d.name() == name) {
            Some(d) => ("on", d.info()),
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
        if state == "on" {
            entry["path"] = format!("{MOUNT_IN_SANDBOX}/{name}").into();
        } else {
            entry["how_to_enable"] = format!(
                "the user sets [devices] {name} = true in ~/.config/paddock/config.toml on the host"
            )
            .into();
        }
        devices.push(entry);
    }
    let doc = serde_json::json!({
        "version": 1,
        "protocol": "Open a device read-write, write the whole request, then read the response until end of file. One open is one request. The first read sends the request and blocks until the response is ready. Closing the file early cancels the request. A read fails with ETIMEDOUT after the deadline, EFBIG means the request is too large, EINVAL that the device rejected it. A waiting device (wait, ask) fails a read with ETIME (still waiting) when the timeout of the request passes before the event: send the same request again. EACCES means that the user declined. `paddock-host device NAME` does all of this and exits with 75 for still waiting.",
        "devices": devices,
    });
    let mut out = serde_json::to_vec_pretty(&doc).unwrap_or_default();
    out.push(b'\n');
    out
}

enum Phase {
    /// The client writes the request.
    Collect,
    /// The device works on it.
    Running,
    Done(Result<Arc<Vec<u8>>, Errno>),
}

enum Kind {
    Catalog,
    Device(usize),
}

struct Handle {
    kind: Kind,
    request: Vec<u8>,
    phase: Phase,
    /// Reads that wait for the response: the reply, its size and the
    /// thread that reads (a host thread id; FUSE translates it).
    waiting: Vec<(ReplyData, u32, u32)>,
    /// How much of the response the client has read. The kernel offset
    /// counts the request bytes too, so the response keeps its own.
    cursor: usize,
    cancel: Arc<AtomicBool>,
}

struct Inner {
    sandbox: String,
    uid: u32,
    gid: u32,
    devices: Vec<Arc<dyn Device>>,
    catalog: Arc<Vec<u8>>,
    handles: Mutex<HashMap<u64, Handle>>,
    next_fh: AtomicU64,
    running: AtomicU64,
    started: SystemTime,
}

/// The file system of one sandbox.
#[derive(Clone)]
pub struct DeviceFs {
    inner: Arc<Inner>,
}

fn reply_slice(reply: ReplyData, data: &[u8], offset: u64, size: u32) {
    let start = (offset as usize).min(data.len());
    let end = start.saturating_add(size as usize).min(data.len());
    reply.data(&data[start..end]);
}

/// The next part of the response for a read of `size` bytes.
fn take(cursor: &mut usize, data: &[u8], size: u32) -> std::ops::Range<usize> {
    let start = (*cursor).min(data.len());
    let end = start.saturating_add(size as usize).min(data.len());
    *cursor = end;
    start..end
}

impl DeviceFs {
    pub fn new(sandbox: &str, devices: Vec<Arc<dyn Device>>) -> Self {
        let catalog = Arc::new(catalog(&devices));
        Self {
            inner: Arc::new(Inner {
                sandbox: sandbox.to_string(),
                uid: unsafe { libc::getuid() },
                gid: unsafe { libc::getgid() },
                devices,
                catalog,
                handles: Mutex::new(HashMap::new()),
                next_fh: AtomicU64::new(1),
                running: AtomicU64::new(0),
                started: SystemTime::now(),
            }),
        }
    }

    fn attr(&self, ino: u64) -> Option<FileAttr> {
        let i = &self.inner;
        let (kind, perm, size, nlink) = match ino {
            ROOT => (FileType::Directory, 0o555, 0, 2),
            CATALOG_INO => (FileType::RegularFile, 0o444, i.catalog.len() as u64, 1),
            n if n >= FIRST_DEVICE_INO && ((n - FIRST_DEVICE_INO) as usize) < i.devices.len() => {
                (FileType::RegularFile, 0o600, 0, 1)
            }
            _ => return None,
        };
        Some(FileAttr {
            ino: INodeNo(ino),
            size,
            blocks: size.div_ceil(512),
            atime: i.started,
            mtime: i.started,
            ctime: i.started,
            crtime: i.started,
            kind,
            perm,
            nlink,
            uid: i.uid,
            gid: i.gid,
            rdev: 0,
            flags: 0,
            blksize: 4096,
        })
    }

    /// Store the result and answer every read that waits for it.
    fn finish(inner: &Inner, fh: u64, result: Result<Arc<Vec<u8>>, Errno>) {
        let waiting = {
            let mut h = inner.handles.lock().unwrap();
            let Some(handle) = h.get_mut(&fh) else {
                return;
            };
            if !matches!(handle.phase, Phase::Running) {
                return;
            }
            handle.phase = Phase::Done(result.clone());
            let mut out = Vec::new();
            for (reply, size, _) in std::mem::take(&mut handle.waiting) {
                let part = match &result {
                    Ok(data) => Ok(take(&mut handle.cursor, data, size)),
                    Err(e) => Err(*e),
                };
                out.push((reply, part));
            }
            out
        };
        for (reply, part) in waiting {
            match (&result, part) {
                (Ok(data), Ok(r)) => reply.data(&data[r]),
                (_, Err(e)) => reply.error(e),
                (Err(e), _) => reply.error(*e),
            }
        }
    }

    /// True when the handle is closed, or every read that waits on it comes
    /// from a thread with a fatal signal pending.
    fn readers_gone(inner: &Inner, fh: u64) -> bool {
        let h = inner.handles.lock().unwrap();
        let Some(handle) = h.get(&fh) else {
            return true;
        };
        !handle.waiting.is_empty() && handle.waiting.iter().all(|w| fatal_pending(w.2))
    }

    /// Run the request of `fh` on its own thread, under the deadline.
    fn dispatch(&self, fh: u64, dev: Arc<dyn Device>, request: Vec<u8>, cancel: Arc<AtomicBool>) {
        let inner = self.inner.clone();
        let info = dev.info();
        let deadline = Instant::now() + Duration::from_secs(info.limits.deadline_seconds);
        inner.running.fetch_add(1, Ordering::SeqCst);
        std::thread::spawn(move || {
            let start = Instant::now();
            let call = Call {
                sandbox: inner.sandbox.clone(),
                deadline,
                cancel: cancel.clone(),
            };
            let (tx, rx) = std::sync::mpsc::channel();
            let worker_dev = dev.clone();
            let req_len = request.len();
            std::thread::spawn(move || {
                let r = worker_dev.call(&call, &request);
                let _ = tx.send(r);
            });
            // Wait for the device, the deadline, a close, or the end of every
            // reader. A reader that gets a fatal signal cannot leave a FUSE
            // read on its own, so paddock ends the read for it.
            let result = loop {
                match rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(Ok(out)) if out.len() > info.limits.max_response => {
                        break Err(Errno::EMSGSIZE)
                    }
                    Ok(Ok(out)) => break Ok(Arc::new(out)),
                    Ok(Err(_)) if Instant::now() >= deadline => break Err(Errno::ETIMEDOUT),
                    Ok(Err(e)) => break Err(e.errno()),
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break Err(Errno::EIO),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                }
                if Instant::now() >= deadline {
                    cancel.store(true, Ordering::SeqCst);
                    break Err(Errno::ETIMEDOUT);
                }
                if cancel.load(Ordering::SeqCst) || Self::readers_gone(&inner, fh) {
                    cancel.store(true, Ordering::SeqCst);
                    break Err(Errno::EINTR);
                }
            };
            inner.running.fetch_sub(1, Ordering::SeqCst);
            // Sizes and the outcome only: never the request or the response.
            let outcome = match &result {
                Ok(d) => format!("{} bytes back", d.len()),
                Err(e) if cancel.load(Ordering::SeqCst) && *e != Errno::ETIMEDOUT => {
                    "cancelled".into()
                }
                Err(e) => format!("error {}", i32::from(*e)),
            };
            log::info!(
                "sandbox {}: device {}: request of {req_len} bytes, {outcome}, {} ms",
                inner.sandbox,
                dev.name(),
                start.elapsed().as_millis()
            );
            Self::finish(&inner, fh, result);
        });
    }
}

impl Filesystem for DeviceFs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        if u64::from(parent) != ROOT {
            return reply.error(Errno::ENOENT);
        }
        let ino = if name == CATALOG {
            Some(CATALOG_INO)
        } else {
            self.inner
                .devices
                .iter()
                .position(|d| OsStr::new(d.name()) == name)
                .map(|i| FIRST_DEVICE_INO + i as u64)
        };
        match ino.and_then(|i| self.attr(i)) {
            Some(a) => reply.entry(&TTL, &a, fuser::Generation(0)),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match self.attr(ino.into()) {
            Some(a) => reply.attr(&TTL, &a),
            None => reply.error(Errno::ENOENT),
        }
    }

    /// Only the truncation that `open(O_TRUNC)` and shell redirections do,
    /// on a device file. Nothing else changes.
    #[allow(clippy::too_many_arguments)]
    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<fuser::TimeOrNow>,
        _mtime: Option<fuser::TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let ino = u64::from(ino);
        if ino < FIRST_DEVICE_INO || mode.is_some() || uid.is_some() || gid.is_some() {
            return reply.error(Errno::EACCES);
        }
        if size.is_some_and(|s| s != 0) {
            return reply.error(Errno::EACCES);
        }
        match self.attr(ino) {
            Some(a) => reply.attr(&TTL, &a),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let ino = u64::from(ino);
        let kind = match ino {
            CATALOG_INO => {
                if flags.acc_mode() != OpenAccMode::O_RDONLY {
                    return reply.error(Errno::EACCES);
                }
                Kind::Catalog
            }
            n if self.attr(n).is_some() && n >= FIRST_DEVICE_INO => {
                Kind::Device((n - FIRST_DEVICE_INO) as usize)
            }
            ROOT => return reply.error(Errno::EISDIR),
            _ => return reply.error(Errno::ENOENT),
        };
        let mut h = self.inner.handles.lock().unwrap();
        if h.len() >= MAX_OPEN {
            return reply.error(Errno::EBUSY);
        }
        let fh = self.inner.next_fh.fetch_add(1, Ordering::SeqCst);
        h.insert(
            fh,
            Handle {
                kind,
                request: Vec::new(),
                phase: Phase::Collect,
                waiting: Vec::new(),
                cursor: 0,
                cancel: Arc::new(AtomicBool::new(false)),
            },
        );
        // Direct I/O: every read and write reaches paddock, whatever size
        // the file shows.
        reply.opened(
            FileHandle(fh),
            FopenFlags::FOPEN_DIRECT_IO | FopenFlags::FOPEN_NONSEEKABLE,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let mut h = self.inner.handles.lock().unwrap();
        let Some(handle) = h.get_mut(&u64::from(fh)) else {
            return reply.error(Errno::EBADF);
        };
        let Kind::Device(i) = handle.kind else {
            return reply.error(Errno::EACCES);
        };
        if !matches!(handle.phase, Phase::Collect) {
            // One open is one request: the response is being read.
            return reply.error(Errno::EBUSY);
        }
        let max = self.inner.devices[i].info().limits.max_request;
        if handle.request.len() + data.len() > max {
            return reply.error(Errno::EFBIG);
        }
        handle.request.extend_from_slice(data);
        reply.written(data.len() as u32);
    }

    #[allow(clippy::too_many_arguments)]
    fn read(
        &self,
        req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let fh = u64::from(fh);
        let mut h = self.inner.handles.lock().unwrap();
        let Some(handle) = h.get_mut(&fh) else {
            return reply.error(Errno::EBADF);
        };
        let i = match handle.kind {
            Kind::Catalog => {
                let c = self.inner.catalog.clone();
                drop(h);
                return reply_slice(reply, &c, offset, size);
            }
            Kind::Device(i) => i,
        };
        match &handle.phase {
            Phase::Done(Ok(data)) => {
                let data = data.clone();
                let r = take(&mut handle.cursor, &data, size);
                drop(h);
                reply.data(&data[r])
            }
            Phase::Done(Err(e)) => reply.error(*e),
            Phase::Running => {
                if handle.waiting.len() >= 4 {
                    return reply.error(Errno::EBUSY);
                }
                handle.waiting.push((reply, size, req.pid()));
            }
            Phase::Collect => {
                if self.inner.running.load(Ordering::SeqCst) >= MAX_RUNNING as u64 {
                    return reply.error(Errno::EBUSY);
                }
                handle.phase = Phase::Running;
                handle.waiting.push((reply, size, req.pid()));
                let request = std::mem::take(&mut handle.request);
                let cancel = handle.cancel.clone();
                let dev = self.inner.devices[i].clone();
                drop(h);
                self.dispatch(fh, dev, request, cancel);
            }
        }
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    /// The last close. A request that still runs is cancelled.
    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let removed = self.inner.handles.lock().unwrap().remove(&u64::from(fh));
        if let Some(handle) = removed {
            handle.cancel.store(true, Ordering::SeqCst);
            for (r, _, _) in handle.waiting {
                r.error(Errno::ECANCELED);
            }
        }
        reply.ok();
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        if u64::from(ino) != ROOT {
            return reply.error(Errno::ENOTDIR);
        }
        reply.opened(FileHandle(0), FopenFlags::empty());
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        if u64::from(ino) != ROOT {
            return reply.error(Errno::ENOTDIR);
        }
        let mut entries: Vec<(u64, FileType, String)> = vec![
            (ROOT, FileType::Directory, ".".into()),
            (ROOT, FileType::Directory, "..".into()),
            (CATALOG_INO, FileType::RegularFile, CATALOG.into()),
        ];
        for (i, d) in self.inner.devices.iter().enumerate() {
            entries.push((
                FIRST_DEVICE_INO + i as u64,
                FileType::RegularFile,
                d.name().to_string(),
            ));
        }
        for (n, (ino, kind, name)) in entries.into_iter().enumerate().skip(offset as usize) {
            if reply.add(INodeNo(ino), (n + 1) as u64, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn create(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        reply.error(Errno::EACCES);
    }

    fn mknod(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _name: &OsStr,
        _mode: u32,
        _umask: u32,
        _rdev: u32,
        reply: ReplyEntry,
    ) {
        reply.error(Errno::EACCES);
    }

    fn mkdir(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        reply.error(Errno::EACCES);
    }

    fn unlink(&self, _req: &Request, _parent: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(Errno::EACCES);
    }

    fn rmdir(&self, _req: &Request, _parent: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(Errno::EACCES);
    }

    fn rename(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _name: &OsStr,
        _newparent: INodeNo,
        _newname: &OsStr,
        _flags: fuser::RenameFlags,
        reply: ReplyEmpty,
    ) {
        reply.error(Errno::EACCES);
    }

    fn setxattr(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _name: &OsStr,
        _value: &[u8],
        _flags: i32,
        _position: u32,
        reply: ReplyEmpty,
    ) {
        reply.error(Errno::EACCES);
    }
}

/// Whether the thread `tid` has a fatal signal pending. The kernel marks
/// every fatal signal as SIGKILL in the pending set of each thread.
fn fatal_pending(tid: u32) -> bool {
    let Ok(text) = std::fs::read_to_string(format!("/proc/{tid}/status")) else {
        // Gone already.
        return true;
    };
    text.lines()
        .filter_map(|l| {
            l.strip_prefix("SigPnd:")
                .or_else(|| l.strip_prefix("ShdPnd:"))
        })
        .filter_map(|v| u64::from_str_radix(v.trim(), 16).ok())
        .any(|bits| bits & (1 << (libc::SIGKILL - 1)) != 0)
}

/// Whether this host can mount a user FUSE file system: `/dev/fuse` opens
/// and `fusermount3` (or `fusermount`) exists.
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

/// A mounted device file system. Dropping it unmounts it.
pub struct Mounted {
    pub dir: PathBuf,
    session: Option<fuser::BackgroundSession>,
}

impl Mounted {
    pub fn unmount(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        let Some(s) = self.session.take() else {
            return;
        };
        if let Err(e) = s.umount_and_join() {
            log::debug!("cannot unmount {}: {e}", self.dir.display());
            lazy_unmount(&self.dir);
        }
        let _ = std::fs::remove_dir(&self.dir);
    }
}

impl Drop for Mounted {
    fn drop(&mut self) {
        self.stop();
    }
}

fn lazy_unmount(dir: &Path) {
    for helper in ["fusermount3", "fusermount"] {
        let ok = std::process::Command::new(helper)
            .args(["-u", "-q", "-z", "--"])
            .arg(dir)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if ok {
            return;
        }
    }
}

/// Mount `fs` at `dir`, an empty folder that paddock owns.
pub fn mount(dir: &Path, fs: DeviceFs) -> Result<Mounted> {
    crate::privfs::dir(dir)?;
    let mut cfg = fuser::Config::default();
    cfg.mount_options = vec![
        MountOption::FSName("paddock".into()),
        MountOption::Subtype("paddock".into()),
        MountOption::DefaultPermissions,
        MountOption::NoSuid,
        MountOption::NoDev,
        MountOption::NoExec,
        MountOption::NoAtime,
    ];
    let session = fuser::spawn_mount(fs, dir, &cfg)
        .with_context(|| format!("cannot mount the devices at {}", dir.display()))?;
    Ok(Mounted {
        dir: dir.to_path_buf(),
        session: Some(session),
    })
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
    names.retain(|n| builtin_info(n).is_some() && ceiling.is_none_or(|c| c.contains(n)));
    names
}

/// The devices to serve. See `enabled_names`.
pub fn enabled(
    cfg: &DevicesCfg,
    test: bool,
    ceiling: Option<&[String]>,
    ctx: &DeviceCtx,
) -> Vec<Arc<dyn Device>> {
    enabled_names(cfg, test, ceiling)
        .iter()
        .filter_map(|n| builtin(n, ctx))
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
/// `started_by` the sandboxes above it.
pub fn start(
    env: &HostEnv,
    spec: &mut SandboxSpec,
    backend: &str,
    test: bool,
    ceiling: Option<&[String]>,
    started_by: &[String],
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
    };
    let devices = enabled(&cfg, test, ceiling, &ctx);
    // Without an enabled device the mount is only a catalog, so a missing
    // FUSE is not worth a notice at each start.
    let quiet = devices.is_empty();
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
    let fs = DeviceFs::new(&spec.id, devices);
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
