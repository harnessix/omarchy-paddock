//! Boot the real guest files with `paddock-vm` on macOS. The test runs
//! when PADDOCK_TEST_VM_ASSETS names the folder with the guest files and
//! assets.toml, and the helper is signed with the virtualization
//! entitlement. Without the variable it skips.

#![cfg(target_os = "macos")]

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_paddock-vm");

/// cargo links the helper again for the tests, which drops its signature.
/// Sign it in place with the virtualization entitlement, once.
fn sign_helper() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let ent = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/packaging/macos/paddock-vm.entitlements"
        );
        let ok = std::process::Command::new("codesign")
            .args(["--force", "--sign", "-", "--entitlements", ent])
            .arg(env!("CARGO_BIN_EXE_paddock-vm"))
            .status()
            .is_ok_and(|s| s.success());
        assert!(ok, "codesign of paddock-vm failed");
    });
}

fn assets() -> Option<PathBuf> {
    match std::env::var_os("PADDOCK_TEST_VM_ASSETS") {
        Some(dir) if !dir.is_empty() => Some(PathBuf::from(dir)),
        _ => {
            eprintln!("skipped: set PADDOCK_TEST_VM_ASSETS to the folder of the VM files");
            None
        }
    }
}

/// A short temporary home: the control socket path must fit in 104 bytes.
fn home() -> (tempfile::TempDir, PathBuf) {
    let t = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
    let home = t.path().canonicalize().unwrap();
    std::fs::create_dir_all(home.join("app")).unwrap();
    (t, home)
}

fn spawn(home: &Path, assets: &Path, extra: &[&str]) -> Child {
    sign_helper();
    Command::new(BIN)
        .env_clear()
        .env("HOME", home)
        .env("PATH", "/usr/bin:/bin")
        .arg("start")
        .arg("--assets")
        .arg(assets)
        .arg("--project")
        .arg(home.join("app"))
        .arg("--state")
        .arg(home.join("s"))
        .args(["--cpus", "2", "--memory", "512", "--stop-timeout", "5"])
        .args(extra)
        .spawn()
        .unwrap()
}

fn wait_for(child: &mut Child, limit: Duration) -> ExitStatus {
    let end = Instant::now() + limit;
    loop {
        if let Some(s) = child.try_wait().unwrap() {
            return s;
        }
        if Instant::now() > end {
            let _ = child.kill();
            panic!("paddock-vm did not stop in {limit:?}");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn console(home: &Path) -> String {
    std::fs::read_to_string(home.join("s/console.log")).unwrap_or_default()
}

#[test]
fn boots_runs_true_and_stops() {
    let Some(assets) = assets() else { return };
    let (_t, home) = home();
    // `true` is the first process of the guest. When it ends, the kernel
    // panics with its exit code and the VM stops.
    let mut child = spawn(
        &home,
        &assets,
        &["--append", "rdinit=/bin/true", "--append", "init=/bin/true"],
    );
    // The Virtualization framework restarts a guest whose kernel resets
    // after the panic, so the VM does not stop by itself: wait for the
    // exit of `true` in the console, then stop it.
    let end = Instant::now() + Duration::from_secs(60);
    while !console(&home).contains("exitcode=0x00000000") {
        assert!(
            Instant::now() < end,
            "no exit of true in the console:\n{}",
            console(&home)
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    // SAFETY: a signal to the helper that this test started.
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let status = wait_for(&mut child, Duration::from_secs(30));
    let log = console(&home);
    // 0: the guest stopped on request, 3: the helper stopped it.
    assert!(
        matches!(status.code(), Some(0 | 3)),
        "{status:?}, console:\n{log}"
    );
    assert!(home.join("s/machine-id").is_file());
    assert!(home.join("s/disk.img").is_file());
}

#[test]
fn answers_the_control_socket_and_stops_on_sigterm() {
    let Some(assets) = assets() else { return };
    let (_t, home) = home();
    let mut child = spawn(&home, &assets, &[]);
    let sock = home.join("s/control.sock");
    let end = Instant::now() + Duration::from_secs(30);
    let mut conn = loop {
        if let Ok(c) = UnixStream::connect(&sock) {
            break c;
        }
        assert!(Instant::now() < end, "no control socket");
        std::thread::sleep(Duration::from_millis(100));
    };
    conn.write_all(b"ping\n").unwrap();
    let mut answer = [0u8; 3];
    conn.read_exact(&mut answer).unwrap();
    assert_eq!(&answer, b"ok\n");
    // Give the kernel time to write to the console.
    std::thread::sleep(Duration::from_secs(3));
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let status = wait_for(&mut child, Duration::from_secs(30));
    // 0: the guest stopped on the request. 3: the hard stop after 5 s.
    assert!(
        matches!(status.code(), Some(0 | 3)),
        "{status:?}, console:\n{}",
        console(&home)
    );
    assert!(!console(&home).is_empty(), "the console log is empty");
    assert!(!sock.exists());
}
