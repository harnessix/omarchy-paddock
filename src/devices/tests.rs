use super::*;
use std::io::{Read, Write};
use std::sync::atomic::AtomicUsize;

#[test]
fn config_is_off_by_default_and_names_known_devices() {
    let c = DevicesCfg::parse("").unwrap();
    assert!(c.mount);
    assert!(c.enabled.is_empty());
    assert!(enabled(&c, false, None).is_empty());
    assert_eq!(enabled(&c, true, None).len(), 1);
    let c = DevicesCfg::parse("[devices]\necho = true\n").unwrap();
    assert_eq!(c.enabled, ["echo"]);
    let c = DevicesCfg::parse("[devices]\nmount = false\necho = false\n").unwrap();
    assert!(!c.mount && c.enabled.is_empty());
    assert!(DevicesCfg::parse("[devices]\nsearch = true\n").is_err());
    assert!(DevicesCfg::parse("[devices]\necho = \"yes\"\n").is_err());
}

#[test]
fn the_project_file_cannot_turn_devices_on() {
    let r: Result<crate::manifest::Manifest, _> = toml::from_str("[devices]\necho = true\n");
    assert!(r.is_err());
}

#[test]
fn catalog_lists_every_device_with_its_state() {
    let off: serde_json::Value = serde_json::from_slice(&catalog(&[])).unwrap();
    assert_eq!(off["devices"][0]["name"], "echo");
    assert_eq!(off["devices"][0]["state"], "off");
    assert!(off["devices"][0]["how_to_enable"]
        .as_str()
        .unwrap()
        .contains("config.toml"));
    let on: serde_json::Value = serde_json::from_slice(&catalog(&[Arc::new(Echo)])).unwrap();
    assert_eq!(on["devices"][0]["state"], "on");
    assert_eq!(on["devices"][0]["path"], "/dev/paddock/echo");
    assert_eq!(on["devices"][0]["limits"]["max_request"], 65536);
    assert!(on["protocol"]
        .as_str()
        .unwrap()
        .contains("One open is one request"));
}

/// A device that works until it is cancelled, and counts its calls.
struct Slow {
    calls: Arc<AtomicUsize>,
    saw_cancel: Arc<AtomicBool>,
    deadline: u64,
}

impl Device for Slow {
    fn name(&self) -> &str {
        "slow"
    }
    fn info(&self) -> Info {
        Info {
            request: "any".into(),
            response: "never".into(),
            limits: Limits {
                max_request: 16,
                max_response: 16,
                deadline_seconds: self.deadline,
            },
        }
    }
    fn call(&self, call: &Call, _request: &[u8]) -> Result<Vec<u8>, DevError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        while !call.cancelled() {
            std::thread::sleep(Duration::from_millis(20));
        }
        self.saw_cancel.store(true, Ordering::SeqCst);
        Err(DevError::Cancelled)
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    _m: Mounted,
    calls: Arc<AtomicUsize>,
    saw_cancel: Arc<AtomicBool>,
}

/// Mount a test file system, or None when this host has no FUSE.
fn mounted(deadline: u64) -> Option<Fixture> {
    let env = HostEnv::detect().ok()?;
    if let Err(why) = fuse_available(&env) {
        eprintln!("skipped: {why}");
        return None;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("dev");
    let calls = Arc::new(AtomicUsize::new(0));
    let saw_cancel = Arc::new(AtomicBool::new(false));
    let slow = Slow {
        calls: calls.clone(),
        saw_cancel: saw_cancel.clone(),
        deadline,
    };
    let fs = DeviceFs::new("test-sandbox", vec![Arc::new(Echo), Arc::new(slow)]);
    let m = match mount(&root, fs) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("skipped: {e:#}");
            return None;
        }
    };
    Some(Fixture {
        _dir: dir,
        root,
        _m: m,
        calls,
        saw_cancel,
    })
}

fn open_rw(p: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new().read(true).write(true).open(p)
}

#[test]
fn echo_round_trip_and_one_request_per_open() {
    let Some(f) = mounted(5) else { return };
    let mut h = open_rw(&f.root.join("echo")).unwrap();
    h.write_all(b"hello ").unwrap();
    h.write_all(b"world").unwrap();
    let mut out = Vec::new();
    h.read_to_end(&mut out).unwrap();
    assert_eq!(out, b"hello world");
    // The request was sent: no second one on the same open.
    assert!(h.write_all(b"again").is_err());
    drop(h);
    // A new open is a new request.
    let mut h = open_rw(&f.root.join("echo")).unwrap();
    h.write_all(b"2").unwrap();
    let mut s = String::new();
    h.read_to_string(&mut s).unwrap();
    assert_eq!(s, "2");
    // Shell style: open with O_TRUNC, as `3<>` and `>` do.
    let mut h = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .truncate(true)
        .open(f.root.join("echo"))
        .unwrap();
    h.write_all(b"t").unwrap();
    let mut s = String::new();
    h.read_to_string(&mut s).unwrap();
    assert_eq!(s, "t");
}

#[test]
fn catalog_is_read_only_and_the_folder_fixed() {
    let Some(f) = mounted(5) else { return };
    let text = std::fs::read_to_string(f.root.join(CATALOG)).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    let names: Vec<&str> = v["devices"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["echo", "slow"]);
    assert!(open_rw(&f.root.join(CATALOG)).is_err());
    assert!(std::fs::write(f.root.join(CATALOG), "x").is_err());
    assert!(std::fs::write(f.root.join("new"), "x").is_err());
    assert!(std::fs::create_dir(f.root.join("d")).is_err());
    assert!(std::fs::remove_file(f.root.join("echo")).is_err());
    assert!(std::fs::rename(f.root.join("echo"), f.root.join("e2")).is_err());
    let mut list: Vec<String> = std::fs::read_dir(&f.root)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    list.sort();
    assert_eq!(list, [".catalog", "echo", "slow"]);
}

#[test]
fn a_request_over_the_limit_fails() {
    let Some(f) = mounted(5) else { return };
    let mut h = open_rw(&f.root.join("slow")).unwrap();
    let e = h.write_all(&[0u8; 17]).unwrap_err();
    assert_eq!(e.raw_os_error(), Some(libc::EFBIG));
}

#[test]
fn a_read_fails_at_the_deadline() {
    let Some(f) = mounted(1) else { return };
    let mut h = open_rw(&f.root.join("slow")).unwrap();
    h.write_all(b"x").unwrap();
    let start = Instant::now();
    let mut out = Vec::new();
    let e = h.read_to_end(&mut out).unwrap_err();
    assert_eq!(e.raw_os_error(), Some(libc::ETIMEDOUT));
    assert!(start.elapsed() < Duration::from_secs(4));
    let wait = Instant::now();
    while !f.saw_cancel.load(Ordering::SeqCst) && wait.elapsed() < Duration::from_secs(2) {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(f.saw_cancel.load(Ordering::SeqCst));
}

#[test]
fn closing_before_the_read_sends_nothing() {
    let Some(f) = mounted(5) else { return };
    let mut h = open_rw(&f.root.join("slow")).unwrap();
    h.write_all(b"x").unwrap();
    drop(h);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(f.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn a_killed_reader_cancels_the_request() {
    let Some(f) = mounted(30) else { return };
    let dev = f.root.join("slow");
    let mut child = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg("exec 3<>\"$1\"; printf x >&3; exec cat <&3")
        .arg("sh")
        .arg(&dev)
        .spawn()
        .unwrap();
    let wait = Instant::now();
    while f.calls.load(Ordering::SeqCst) == 0 && wait.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(f.calls.load(Ordering::SeqCst), 1);
    let killed = Instant::now();
    child.kill().unwrap();
    child.wait().unwrap();
    // Far before the deadline of 30 seconds.
    assert!(killed.elapsed() < Duration::from_secs(5));
    let wait = Instant::now();
    while !f.saw_cancel.load(Ordering::SeqCst) && wait.elapsed() < Duration::from_secs(2) {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(f.saw_cancel.load(Ordering::SeqCst));
}

#[test]
fn unmount_leaves_an_empty_folder() {
    let Some(f) = mounted(5) else { return };
    let root = f.root.clone();
    let Fixture { _m, _dir, .. } = f;
    _m.unmount();
    assert!(!root.join(CATALOG).exists());
}

/// For manual checks of a backend: mount at $PADDOCK_TEST_MOUNT for
/// $PADDOCK_TEST_SECONDS. `cargo test -- --ignored hold_a_mount`.
#[test]
#[ignore]
fn hold_a_mount() {
    let Ok(dir) = std::env::var("PADDOCK_TEST_MOUNT") else {
        return;
    };
    let secs: u64 = std::env::var("PADDOCK_TEST_SECONDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30);
    let fs = DeviceFs::new("manual", vec![Arc::new(Echo)]);
    let _m = mount(Path::new(&dir), fs).unwrap();
    std::thread::sleep(Duration::from_secs(secs));
}
