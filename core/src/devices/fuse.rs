//! The FUSE file system that serves `/dev/paddock` on Linux.

use super::{catalog_with, Call, Device, CATALOG, SECRET_DIR};
use anyhow::{Context, Result};
use fuser::{
    Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, INodeNo, LockOwner, MountOption,
    OpenAccMode, OpenFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyOpen, ReplyWrite, Request, WriteFlags,
};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

/// Open device files at the same time, per sandbox.
const MAX_OPEN: usize = 64;
/// Requests that run at the same time, per sandbox.
const MAX_RUNNING: usize = 16;

const ROOT: u64 = 1;
const CATALOG_INO: u64 = 2;
const SECRET_DIR_INO: u64 = 3;
const FIRST_DEVICE_INO: u64 = 16;
/// The inode of the first secret file. Devices stay below it.
const FIRST_SECRET_INO: u64 = 1 << 20;
const TTL: Duration = Duration::from_secs(1);

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
    /// A secret file: its name and the value, held while the file is open.
    Secret(String, Arc<crate::credentials::secret::Secret>),
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
    secrets: Arc<SecretFiles>,
    /// The names of the secret files by inode, from `FIRST_SECRET_INO` on.
    /// A name keeps its inode after a revoke.
    secret_inos: Mutex<Vec<String>>,
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

use crate::credentials::secret::{FileError, SecretFiles};

impl DeviceFs {
    pub fn new(sandbox: &str, devices: Vec<Arc<dyn Device>>) -> Self {
        Self::with_secrets(sandbox, devices, SecretFiles::new(None, None))
    }

    /// A file system that also serves the secret files of `secrets`.
    pub fn with_secrets(
        sandbox: &str,
        devices: Vec<Arc<dyn Device>>,
        secrets: Arc<SecretFiles>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                sandbox: sandbox.to_string(),
                uid: unsafe { libc::getuid() },
                gid: unsafe { libc::getgid() },
                devices,
                secrets,
                secret_inos: Mutex::new(Vec::new()),
                handles: Mutex::new(HashMap::new()),
                next_fh: AtomicU64::new(1),
                running: AtomicU64::new(0),
                started: SystemTime::now(),
            }),
        }
    }

    fn catalog(&self) -> Arc<Vec<u8>> {
        Arc::new(catalog_with(
            &self.inner.devices,
            &self.inner.secrets.names(),
        ))
    }

    /// The name of a secret file inode.
    fn secret_name(&self, ino: u64) -> Option<String> {
        let idx = usize::try_from(ino.checked_sub(FIRST_SECRET_INO)?).ok()?;
        self.inner.secret_inos.lock().unwrap().get(idx).cloned()
    }

    /// The inode of a secret file that the store holds now.
    fn secret_ino(&self, name: &str) -> Option<u64> {
        if !self.inner.secrets.contains(name) {
            return None;
        }
        let mut inos = self.inner.secret_inos.lock().unwrap();
        let idx = match inos.iter().position(|n| n == name) {
            Some(i) => i,
            None => {
                inos.push(name.to_string());
                inos.len() - 1
            }
        };
        Some(FIRST_SECRET_INO + idx as u64)
    }

    fn attr(&self, ino: u64) -> Option<FileAttr> {
        let i = &self.inner;
        let (kind, perm, size, nlink) = match ino {
            ROOT => (FileType::Directory, 0o555, 0, 2),
            CATALOG_INO => (FileType::RegularFile, 0o444, self.catalog().len() as u64, 1),
            SECRET_DIR_INO => (FileType::Directory, 0o500, 0, 2),
            n if n >= FIRST_SECRET_INO => {
                let name = self.secret_name(n)?;
                let v = i.secrets.check(&name).ok()?;
                (FileType::RegularFile, 0o400, v.len() as u64, 1)
            }
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
                    Ok(Err(e)) => break Err(Errno::from_i32(e.errno())),
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
        let ino = match u64::from(parent) {
            ROOT => None,
            SECRET_DIR_INO => name.to_str().and_then(|n| self.secret_ino(n)),
            _ => return reply.error(Errno::ENOENT),
        };
        let ino = if ino.is_some() {
            ino
        } else if u64::from(parent) == SECRET_DIR_INO {
            None
        } else if name == CATALOG {
            Some(CATALOG_INO)
        } else if name == SECRET_DIR && !self.inner.secrets.names().is_empty() {
            Some(SECRET_DIR_INO)
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
        if !(FIRST_DEVICE_INO..FIRST_SECRET_INO).contains(&ino)
            || mode.is_some()
            || uid.is_some()
            || gid.is_some()
        {
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
            n if n >= FIRST_SECRET_INO => {
                if flags.acc_mode() != OpenAccMode::O_RDONLY {
                    return reply.error(Errno::EACCES);
                }
                let Some(name) = self.secret_name(n) else {
                    return reply.error(Errno::ENOENT);
                };
                match self.inner.secrets.open(&name) {
                    Ok(v) => {
                        log::info!("sandbox {}: secret file {name} opened", self.inner.sandbox);
                        Kind::Secret(name, v)
                    }
                    Err(FileError::Revoked) => return reply.error(Errno::EACCES),
                    Err(FileError::Missing) => return reply.error(Errno::ENOENT),
                }
            }
            SECRET_DIR_INO => return reply.error(Errno::EISDIR),
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
        let i = match &handle.kind {
            Kind::Catalog => {
                drop(h);
                return reply_slice(reply, &self.catalog(), offset, size);
            }
            Kind::Secret(name, value) => {
                // A revoke ends the file at once, also while it is open.
                if self.inner.secrets.check(name).is_err() {
                    return reply.error(Errno::EACCES);
                }
                let value = value.clone();
                drop(h);
                return reply_slice(reply, value.as_bytes(), offset, size);
            }
            Kind::Device(i) => *i,
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
        if !matches!(u64::from(ino), ROOT | SECRET_DIR_INO) {
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
        let names = self.inner.secrets.names();
        let mut entries: Vec<(u64, FileType, String)> = match u64::from(ino) {
            ROOT => {
                let mut e = vec![
                    (ROOT, FileType::Directory, ".".into()),
                    (ROOT, FileType::Directory, "..".into()),
                    (CATALOG_INO, FileType::RegularFile, CATALOG.into()),
                ];
                if !names.is_empty() {
                    e.push((SECRET_DIR_INO, FileType::Directory, SECRET_DIR.into()));
                }
                e
            }
            SECRET_DIR_INO => vec![
                (SECRET_DIR_INO, FileType::Directory, ".".into()),
                (ROOT, FileType::Directory, "..".into()),
            ],
            _ => return reply.error(Errno::ENOTDIR),
        };
        if u64::from(ino) == SECRET_DIR_INO {
            for n in names {
                if let Some(i) = self.secret_ino(&n) {
                    entries.push((i, FileType::RegularFile, n));
                }
            }
        } else {
            for (i, d) in self.inner.devices.iter().enumerate() {
                entries.push((
                    FIRST_DEVICE_INO + i as u64,
                    FileType::RegularFile,
                    d.name().to_string(),
                ));
            }
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

pub(super) fn lazy_unmount(dir: &Path) {
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
