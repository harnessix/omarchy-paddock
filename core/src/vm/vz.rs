//! The Virtualization framework side of `paddock-vm`. macOS only.
//!
//! The VM runs on a serial dispatch queue. Every call on the
//! `VZVirtualMachine` happens on that queue. The main thread waits for
//! events: the start, the stop of the guest, an error, SIGTERM, or a stop
//! request on the control socket.
//!
//! The configuration has no network device. A NAT device would reach host
//! services through its gateway, so all traffic uses vsock.

use super::bridge::{self, Guest};
use super::helper::{Plan, PROJECT_TAG, STATE_DISK_BYTES};
use anyhow::{anyhow, bail, Context, Result};
use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::{Bool, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass};
use objc2_foundation::{NSArray, NSData, NSError, NSString, NSURL};
use objc2_virtualization::*;
use std::collections::BTreeMap;
use std::os::fd::{FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The exit code after a hard stop.
pub const EXIT_HARD_STOP: i32 = 3;
/// How long a `connect` on the control socket waits for the guest.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

enum Event {
    Started(Result<(), String>),
    GuestStopped,
    Failed(String),
    Stop,
    HardStopped(Result<(), String>),
}

/// The VM, shared between threads. Only closures on its queue touch it.
struct Vm(Retained<VZVirtualMachine>);
// SAFETY: every use of the inner object runs on the queue of the VM.
unsafe impl Send for Vm {}
unsafe impl Sync for Vm {}

/// An object that moves into a closure on the queue of the VM.
struct OnQueue<T>(T);
// SAFETY: the object is used only inside that closure.
unsafe impl<T> Send for OnQueue<T> {}

impl<T> OnQueue<T> {
    /// A method, so that a closure takes the whole wrapper and not a field.
    fn get(&self) -> &T {
        &self.0
    }
}

/// Boot the VM of `plan` and run until it stops. The exit code is 0 when
/// the guest stops, and `EXIT_HARD_STOP` after a hard stop.
pub fn run(plan: &Plan) -> Result<i32> {
    if !cfg!(target_arch = "aarch64") {
        bail!("the paddock VM needs a Mac with Apple silicon");
    }
    if !unsafe { VZVirtualMachine::isSupported() } {
        bail!("this Mac cannot run virtual machines");
    }
    plan.state.prepare()?;
    if plan.state.ensure_disk(STATE_DISK_BYTES)? {
        log::info!(
            "paddock-vm: made the guest disk {}",
            plan.state.disk.display()
        );
    }
    let config = configuration(plan)?;
    unsafe { config.validateWithError() }
        .map_err(|e| anyhow!("the VM configuration is not valid: {}", describe(&e)))?;

    let (tx, rx) = mpsc::channel::<Event>();
    watch_signals(tx.clone())?;
    let queue = DispatchQueue::new("paddock-vm", None);
    let vm = Arc::new(Vm(unsafe {
        VZVirtualMachine::initWithConfiguration_queue(VZVirtualMachine::alloc(), &config, &queue)
    }));
    let delegate = VmDelegate::new(tx.clone());
    let listener = Listener::new(plan.forwards.clone());
    let begin = Instant::now();
    {
        let vm = vm.clone();
        let tx = tx.clone();
        let objects = OnQueue((delegate.clone(), listener.clone()));
        let ports: Vec<u32> = plan.forwards.keys().copied().collect();
        queue.exec_sync(move || {
            let (delegate, listener) = objects.get();
            let vm = &vm.0;
            unsafe { vm.setDelegate(Some(ProtocolObject::from_ref(&**delegate))) };
            if let Some(dev) = socket_device(vm) {
                unsafe {
                    listener
                        .0
                        .setDelegate(Some(ProtocolObject::from_ref(&*listener.1)))
                };
                for port in ports {
                    unsafe { dev.setSocketListener_forPort(&listener.0, port) };
                }
            }
            let done = RcBlock::new(move |err: *mut NSError| {
                let _ = tx.send(Event::Started(error_text(err)));
            });
            unsafe { vm.startWithCompletionHandler(&done) };
        });
    }

    let code = wait(plan, &vm, &queue, &tx, &rx, begin);
    let _ = std::fs::remove_file(&plan.state.control);
    // The delegates are weak references of the VM. Keep them to the end.
    drop((delegate, listener));
    code
}

fn wait(
    plan: &Plan,
    vm: &Arc<Vm>,
    queue: &DispatchRetained<DispatchQueue>,
    tx: &Sender<Event>,
    rx: &mpsc::Receiver<Event>,
    begin: Instant,
) -> Result<i32> {
    let mut deadline: Option<Instant> = None;
    loop {
        let event = match deadline {
            None => rx.recv().context("the VM events ended")?,
            Some(d) => match rx.recv_timeout(d.saturating_duration_since(Instant::now())) {
                Ok(e) => e,
                Err(RecvTimeoutError::Timeout) => {
                    eprintln!("paddock-vm: the guest did not stop in time, stopping the VM");
                    hard_stop(vm, queue, tx);
                    deadline = Some(Instant::now() + Duration::from_secs(3600));
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => bail!("the VM events ended"),
            },
        };
        match event {
            Event::Started(Ok(())) => {
                eprintln!(
                    "paddock-vm: the VM runs, {} ms after the start",
                    begin.elapsed().as_millis()
                );
                let control = bridge::bind_control(&plan.state.control)?;
                let guest = VmGuest {
                    vm: vm.clone(),
                    queue: queue.clone(),
                    tx: tx.clone(),
                };
                bridge::serve_control(control, Arc::new(guest));
            }
            Event::Started(Err(e)) => bail!("cannot start the VM: {e}"),
            Event::GuestStopped => {
                eprintln!(
                    "paddock-vm: the guest stopped, {} ms after the start",
                    begin.elapsed().as_millis()
                );
                return Ok(0);
            }
            Event::Failed(e) => bail!("the VM stopped with an error: {e}"),
            Event::Stop if deadline.is_none() => {
                eprintln!("paddock-vm: asking the guest to stop");
                request_stop(vm, queue, tx);
                deadline = Some(Instant::now() + plan.stop_timeout);
            }
            // A second signal does not wait any longer.
            Event::Stop => hard_stop(vm, queue, tx),
            Event::HardStopped(Ok(())) => return Ok(EXIT_HARD_STOP),
            Event::HardStopped(Err(e)) => bail!("cannot stop the VM: {e}"),
        }
    }
}

fn request_stop(vm: &Arc<Vm>, queue: &DispatchQueue, tx: &Sender<Event>) {
    let (vm, tx) = (vm.clone(), tx.clone());
    queue.exec_async(move || {
        let vm = &vm.0;
        if unsafe { vm.canRequestStop() } {
            if let Err(e) = unsafe { vm.requestStopWithError() } {
                eprintln!("paddock-vm: the stop request failed: {}", describe(&e));
            }
        } else {
            // The guest cannot get the request yet. Stop the VM now.
            stop_now(vm, tx);
        }
    });
}

fn hard_stop(vm: &Arc<Vm>, queue: &DispatchQueue, tx: &Sender<Event>) {
    let (vm, tx) = (vm.clone(), tx.clone());
    queue.exec_async(move || stop_now(&vm.0, tx));
}

/// Stop the VM at once. Call this on the queue of the VM.
fn stop_now(vm: &VZVirtualMachine, tx: Sender<Event>) {
    if !unsafe { vm.canStop() } {
        let _ = tx.send(Event::HardStopped(Err(
            "the VM is in a state that cannot stop".into(),
        )));
        return;
    }
    let done = RcBlock::new(move |err: *mut NSError| {
        let _ = tx.send(Event::HardStopped(error_text(err)));
    });
    unsafe { vm.stopWithCompletionHandler(&done) };
}

/// The control socket side of the VM.
struct VmGuest {
    vm: Arc<Vm>,
    queue: DispatchRetained<DispatchQueue>,
    tx: Sender<Event>,
}

impl Guest for VmGuest {
    fn connect(&self, port: u32) -> Result<OwnedFd> {
        let (rtx, rrx) = mpsc::channel::<Result<OwnedFd, String>>();
        let vm = self.vm.clone();
        self.queue.exec_async(move || {
            let Some(dev) = socket_device(&vm.0) else {
                let _ = rtx.send(Err("the VM has no socket device".into()));
                return;
            };
            let done = RcBlock::new(
                move |conn: *mut VZVirtioSocketConnection, err: *mut NSError| {
                    let r = match unsafe { conn.as_ref() } {
                        Some(c) => dup_fd(unsafe { c.fileDescriptor() }).map_err(|e| e.to_string()),
                        None => Err(error_text(err)
                            .err()
                            .unwrap_or_else(|| "no connection".into())),
                    };
                    let _ = rtx.send(r);
                },
            );
            unsafe { dev.connectToPort_completionHandler(port, &done) };
        });
        match rrx.recv_timeout(CONNECT_TIMEOUT) {
            Ok(r) => r.map_err(|e| anyhow!("cannot connect to guest port {port}: {e}")),
            Err(_) => bail!("guest port {port} did not answer"),
        }
    }

    fn stop(&self) {
        let _ = self.tx.send(Event::Stop);
    }
}

fn configuration(plan: &Plan) -> Result<Retained<VZVirtualMachineConfiguration>> {
    let config = unsafe { VZVirtualMachineConfiguration::new() };
    let cpus = plan.cpus as usize;
    let (min_cpu, max_cpu) = unsafe {
        (
            VZVirtualMachineConfiguration::minimumAllowedCPUCount(),
            VZVirtualMachineConfiguration::maximumAllowedCPUCount(),
        )
    };
    if cpus < min_cpu || cpus > max_cpu {
        bail!("--cpus {cpus} is outside {min_cpu} to {max_cpu} on this Mac");
    }
    let (min_mem, max_mem) = unsafe {
        (
            VZVirtualMachineConfiguration::minimumAllowedMemorySize(),
            VZVirtualMachineConfiguration::maximumAllowedMemorySize(),
        )
    };
    if plan.memory_bytes < min_mem || plan.memory_bytes > max_mem {
        bail!(
            "--memory {} MiB is outside {} to {} MiB on this Mac",
            plan.memory_bytes >> 20,
            min_mem >> 20,
            max_mem >> 20
        );
    }
    unsafe {
        config.setCPUCount(cpus);
        config.setMemorySize(plan.memory_bytes);
    }

    let kernel = url(&plan.assets.kernel)?;
    let boot = unsafe { VZLinuxBootLoader::initWithKernelURL(VZLinuxBootLoader::alloc(), &kernel) };
    unsafe {
        boot.setCommandLine(&NSString::from_str(&plan.cmdline));
        if let Some(initrd) = &plan.assets.initrd {
            let initrd = url(initrd)?;
            boot.setInitialRamdiskURL(Some(&initrd));
        }
        config.setBootLoader(Some(&boot));
    }

    let platform = unsafe { VZGenericPlatformConfiguration::new() };
    let id = machine_id(plan)?;
    unsafe {
        platform.setMachineIdentifier(&id);
        config.setPlatform(&platform);
    }

    let rootfs = disk(&plan.assets.rootfs, true)?;
    let state = disk(&plan.state.disk, false)?;
    unsafe { config.setStorageDevices(&NSArray::from_retained_slice(&[rootfs, state])) };

    let project = url(&plan.project)?;
    let share = unsafe {
        let dir =
            VZSharedDirectory::initWithURL_readOnly(VZSharedDirectory::alloc(), &project, false);
        VZSingleDirectoryShare::initWithDirectory(VZSingleDirectoryShare::alloc(), &dir)
    };
    let fs = unsafe {
        VZVirtioFileSystemDeviceConfiguration::initWithTag(
            VZVirtioFileSystemDeviceConfiguration::alloc(),
            &NSString::from_str(PROJECT_TAG),
        )
    };
    unsafe {
        fs.setShare(Some(&share));
        config.setDirectorySharingDevices(&NSArray::from_retained_slice(&[fs.into_super()]));
    }

    let console_url = url(&plan.state.console)?;
    let console = unsafe {
        VZFileSerialPortAttachment::initWithURL_append_error(
            VZFileSerialPortAttachment::alloc(),
            &console_url,
            false,
        )
    }
    .map_err(|e| {
        anyhow!(
            "cannot open {}: {}",
            plan.state.console.display(),
            describe(&e)
        )
    })?;
    let serial = unsafe { VZVirtioConsoleDeviceSerialPortConfiguration::new() };
    unsafe {
        serial.setAttachment(Some(&console));
        config.setSerialPorts(&NSArray::from_retained_slice(&[serial.into_super()]));
        let vsock = VZVirtioSocketDeviceConfiguration::new();
        config.setSocketDevices(&NSArray::from_retained_slice(&[vsock.into_super()]));
        let entropy = VZVirtioEntropyDeviceConfiguration::new();
        config.setEntropyDevices(&NSArray::from_retained_slice(&[entropy.into_super()]));
        let balloon = VZVirtioTraditionalMemoryBalloonDeviceConfiguration::new();
        config.setMemoryBalloonDevices(&NSArray::from_retained_slice(&[balloon.into_super()]));
        // No network device: the guest reaches the host only over vsock.
        config.setNetworkDevices(&NSArray::new());
    }
    Ok(config)
}

/// A virtio block device for a raw disk image.
fn disk(path: &Path, read_only: bool) -> Result<Retained<VZStorageDeviceConfiguration>> {
    let path_url = url(path)?;
    let attachment = unsafe {
        VZDiskImageStorageDeviceAttachment::initWithURL_readOnly_error(
            VZDiskImageStorageDeviceAttachment::alloc(),
            &path_url,
            read_only,
        )
    }
    .map_err(|e| anyhow!("cannot open the disk {}: {}", path.display(), describe(&e)))?;
    let dev = unsafe {
        VZVirtioBlockDeviceConfiguration::initWithAttachment(
            VZVirtioBlockDeviceConfiguration::alloc(),
            &attachment,
        )
    };
    Ok(dev.into_super())
}

/// The machine identifier of the state folder, made on the first start.
fn machine_id(plan: &Plan) -> Result<Retained<VZGenericMachineIdentifier>> {
    if let Some(data) = plan.state.read_machine_id()? {
        let data = NSData::with_bytes(&data);
        return unsafe {
            VZGenericMachineIdentifier::initWithDataRepresentation(
                VZGenericMachineIdentifier::alloc(),
                &data,
            )
        }
        .ok_or_else(|| {
            anyhow!(
                "the machine identifier {} is damaged: remove the file to make a new one",
                plan.state.machine_id.display()
            )
        });
    }
    let id = unsafe { VZGenericMachineIdentifier::new() };
    plan.state
        .write_machine_id(&unsafe { id.dataRepresentation() }.to_vec())?;
    Ok(id)
}

fn socket_device(vm: &VZVirtualMachine) -> Option<Retained<VZVirtioSocketDevice>> {
    let first = unsafe { vm.socketDevices() }.firstObject()?;
    first.downcast::<VZVirtioSocketDevice>().ok()
}

fn url(path: &Path) -> Result<Retained<NSURL>> {
    let text = path
        .to_str()
        .ok_or_else(|| anyhow!("the path {} is not UTF-8", path.display()))?;
    Ok(NSURL::fileURLWithPath(&NSString::from_str(text)))
}

fn describe(e: &NSError) -> String {
    e.localizedDescription().to_string()
}

fn error_text(err: *mut NSError) -> Result<(), String> {
    match unsafe { err.as_ref() } {
        None => Ok(()),
        Some(e) => Err(describe(e)),
    }
}

/// A copy of a file descriptor that the framework owns.
fn dup_fd(fd: libc::c_int) -> std::io::Result<OwnedFd> {
    let new = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    if new < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(new) })
}

static SIGNAL_FD: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_signal(_: libc::c_int) {
    let fd = SIGNAL_FD.load(Ordering::Relaxed);
    if fd >= 0 {
        // Only write(2) here: it is safe in a signal handler.
        unsafe { libc::write(fd, b"s".as_ptr().cast(), 1) };
    }
}

/// Turn SIGTERM and SIGINT into stop events.
fn watch_signals(tx: Sender<Event>) -> Result<()> {
    let mut fds = [0 as libc::c_int; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error()).context("cannot make the signal pipe");
    }
    let read = unsafe { std::fs::File::from_raw_fd(fds[0]) };
    SIGNAL_FD.store(fds[1], Ordering::Relaxed);
    let handler = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
    for sig in [libc::SIGTERM, libc::SIGINT] {
        unsafe { libc::signal(sig, handler) };
    }
    std::thread::spawn(move || {
        use std::io::Read;
        let mut read = read;
        let mut byte = [0u8; 1];
        while matches!(read.read(&mut byte), Ok(1)) {
            if tx.send(Event::Stop).is_err() {
                break;
            }
        }
    });
    Ok(())
}

struct VmIvars {
    tx: Sender<Event>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = AllocAnyThread]
    #[name = "PaddockVmDelegate"]
    #[ivars = VmIvars]
    struct VmDelegate;

    unsafe impl NSObjectProtocol for VmDelegate {}

    unsafe impl VZVirtualMachineDelegate for VmDelegate {
        #[unsafe(method(guestDidStopVirtualMachine:))]
        fn guest_did_stop(&self, _vm: &VZVirtualMachine) {
            let _ = self.ivars().tx.send(Event::GuestStopped);
        }

        #[unsafe(method(virtualMachine:didStopWithError:))]
        fn did_stop_with_error(&self, _vm: &VZVirtualMachine, error: &NSError) {
            let _ = self.ivars().tx.send(Event::Failed(describe(error)));
        }
    }
);

impl VmDelegate {
    fn new(tx: Sender<Event>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(VmIvars { tx });
        unsafe { msg_send![super(this), init] }
    }
}

struct ListenIvars {
    forwards: BTreeMap<u32, PathBuf>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = AllocAnyThread]
    #[name = "PaddockVmListener"]
    #[ivars = ListenIvars]
    struct ListenDelegate;

    unsafe impl NSObjectProtocol for ListenDelegate {}

    unsafe impl VZVirtioSocketListenerDelegate for ListenDelegate {
        #[unsafe(method(listener:shouldAcceptNewConnection:fromSocketDevice:))]
        fn should_accept(
            &self,
            _listener: &VZVirtioSocketListener,
            conn: &VZVirtioSocketConnection,
            _device: &VZVirtioSocketDevice,
        ) -> Bool {
            let port = unsafe { conn.destinationPort() };
            let Some(path) = self.ivars().forwards.get(&port) else {
                return Bool::NO;
            };
            match dup_fd(unsafe { conn.fileDescriptor() }) {
                Ok(fd) => {
                    bridge::forward(fd, path);
                    Bool::YES
                }
                Err(e) => {
                    log::warn!("paddock-vm: guest connection to port {port}: {e}");
                    Bool::NO
                }
            }
        }
    }
);

/// The listener of the forwarded host ports and its delegate.
#[derive(Clone)]
struct Listener(Retained<VZVirtioSocketListener>, Retained<ListenDelegate>);

impl Listener {
    fn new(forwards: BTreeMap<u32, PathBuf>) -> Self {
        let this = ListenDelegate::alloc().set_ivars(ListenIvars { forwards });
        let delegate: Retained<ListenDelegate> = unsafe { msg_send![super(this), init] };
        Self(unsafe { VZVirtioSocketListener::new() }, delegate)
    }
}
