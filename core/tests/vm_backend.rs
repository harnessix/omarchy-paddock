//! The VM backend without a VM: the guest agent on unix sockets (a
//! test-only transport) with a fake paddock, and `paddock vm status`.
//! On macOS, a test with the real VM files runs when
//! PADDOCK_TEST_VM_ASSETS names their folder.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_paddock");

#[cfg(target_os = "linux")] // the agent test runs on Linux only
fn wait_for(path: &std::path::Path) {
    let end = Instant::now() + Duration::from_secs(20);
    while !path.exists() {
        assert!(Instant::now() < end, "{} never came", path.display());
        std::thread::sleep(Duration::from_millis(20));
    }
}

struct Kill(Child);

impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn line(c: &mut UnixStream) -> String {
    let mut out = Vec::new();
    let mut b = [0u8; 1];
    while c.read(&mut b).unwrap() == 1 && b[0] != b'\n' {
        out.push(b[0]);
    }
    String::from_utf8(out).unwrap()
}

#[cfg(target_os = "linux")]
#[test]
fn the_agent_runs_one_request_end_to_end() {
    let t = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
    let root = t.path().canonicalize().unwrap();
    let ports = root.join("ports");
    let project = root.join("project");
    for d in [&ports, &project] {
        std::fs::create_dir_all(d).unwrap();
    }
    // The fake paddock in the guest: it prints what it got, echoes one
    // line of input and exits with 9.
    let fake = root.join("fake-paddock");
    std::fs::write(
        &fake,
        "#!/bin/sh\necho \"argv: $*\"\necho \"cwd: $(pwd)\"\ncat \"$XDG_CONFIG_HOME/paddock/harness.d/fake.toml\"\nread x\necho \"in: $x\"\nexit 9\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let _agent = Kill(
        Command::new(BIN)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .arg("vm-agent")
            .arg("--unix")
            .arg(&ports)
            .arg("--paddock")
            .arg(&fake)
            .arg("--home")
            .arg(root.join("home"))
            .stdin(Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait_for(&ports.join("1024.sock"));
    wait_for(&ports.join("1027.sock"));

    let token = "0123456789abcdef0123456789abcdef";
    let req = format!(
        r#"{{"op":"run","v":3,"token":"{token}","id":"fake-x-1","harness":"fake","project":"{}","network":"none","args":["hi"],"tty":false,"profile":"name = \"fake\"\n"}}"#,
        project.display()
    );
    let mut control = UnixStream::connect(ports.join("1024.sock")).unwrap();
    writeln!(control, "{req}").unwrap();
    assert_eq!(line(&mut control), r#"{"reply":"ok"}"#);
    let mut pty = UnixStream::connect(ports.join("1027.sock")).unwrap();
    writeln!(pty, r#"{{"op":"attach","token":"{token}"}}"#).unwrap();
    pty.write_all(b"typed\n").unwrap();
    let mut out = String::new();
    pty.read_to_string(&mut out).unwrap();
    assert_eq!(
        out,
        format!(
            "argv: run fake {p} --backend bwrap --network none --yes -- hi\ncwd: {p}\nname = \"fake\"\nin: typed\n",
            p = project.display()
        )
    );
    assert_eq!(line(&mut control), r#"{"reply":"exit","code":9}"#);
    // The project is not mounted in this transport: no mount, no state disk.
    assert!(root.join("home").is_dir());
    // A host that closes before the answer ends only its own connection:
    // the agent gets EPIPE, not a SIGPIPE that ends it and the VM.
    for _ in 0..3 {
        let mut c = UnixStream::connect(ports.join("1024.sock")).unwrap();
        c.write_all(b"{\"op\":\"ping\"}\n").unwrap();
    }
    std::thread::sleep(Duration::from_millis(200));
    let mut c = UnixStream::connect(ports.join("1024.sock")).unwrap();
    c.write_all(b"{\"op\":\"ping\"}\n").unwrap();
    assert_eq!(line(&mut c), r#"{"reply":"ok","runs":0}"#);
}

#[test]
fn vm_status_says_not_running() {
    let t = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
    let out = Command::new(BIN)
        .env_clear()
        .env("HOME", t.path())
        .env("PATH", "/usr/bin:/bin")
        .env("XDG_RUNTIME_DIR", t.path().join("run"))
        .args(["vm", "status"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // The running VM first, then the guest files.
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.starts_with("VM        not running\nVM files in "),
        "{text}"
    );
    assert!(text.contains("not set up"), "{text}");
    assert!(
        text.ends_with("Run `paddock vm setup` to install them.\n"),
        "{text}"
    );
    let out = Command::new(BIN)
        .env_clear()
        .env("HOME", t.path())
        .env("PATH", "/usr/bin:/bin")
        .env("XDG_RUNTIME_DIR", t.path().join("run"))
        .args(["vm", "stop"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "paddock: no VM runs\n"
    );
}

#[cfg(target_os = "macos")]
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

/// The real VM: the supervisor boots the guest files, the guest agent
/// answers a ping, and `paddock vm stop` ends it.
#[cfg(target_os = "macos")]
#[test]
fn the_vm_boots_its_agent_and_stops() {
    let Some(assets) = std::env::var_os("PADDOCK_TEST_VM_ASSETS")
        .filter(|a| !a.is_empty())
        .map(std::path::PathBuf::from)
    else {
        eprintln!("skipped: set PADDOCK_TEST_VM_ASSETS to the folder of the VM files");
        return;
    };
    sign_helper();
    let t = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
    let home = t.path().canonicalize().unwrap();
    let data = home.join("data");
    std::fs::create_dir_all(data.join("paddock")).unwrap();
    std::os::unix::fs::symlink(&assets, data.join("paddock/vm")).unwrap();
    let project = home.join("project");
    std::fs::create_dir_all(&project).unwrap();
    let cmd = |args: &[&str]| {
        let mut c = Command::new(BIN);
        c.env_clear()
            .env("HOME", &home)
            .env("PATH", "/usr/bin:/bin")
            .env("XDG_DATA_HOME", &data)
            .args(args);
        c
    };
    let _serve = Kill(
        cmd(&["vm-serve", project.to_str().unwrap()])
            .stdin(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let end = Instant::now() + Duration::from_secs(60);
    let control = loop {
        let out = cmd(&["vm", "status", "--json"]).output().unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        if v["running"] == true {
            break std::path::PathBuf::from(v["control"].as_str().unwrap());
        }
        assert!(Instant::now() < end, "the VM did not start");
        std::thread::sleep(Duration::from_millis(200));
    };
    // The guest agent answers on the control port once the guest is up.
    let end = Instant::now() + Duration::from_secs(60);
    loop {
        let mut c = UnixStream::connect(&control).unwrap();
        c.write_all(b"connect control\n").unwrap();
        if line(&mut c) == "ok" {
            c.write_all(b"{\"op\":\"ping\"}\n").unwrap();
            assert_eq!(line(&mut c), r#"{"reply":"ok","runs":0}"#);
            break;
        }
        assert!(Instant::now() < end, "the guest agent did not answer");
        std::thread::sleep(Duration::from_millis(500));
    }
    let out = cmd(&["vm", "stop"]).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = cmd(&["vm", "status"]).output().unwrap();
    // The running VM first, then the guest files.
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.starts_with("VM        not running\nVM files in "),
        "{text}"
    );
    // The guest files stay in place after the stop.
    assert!(text.contains("ok       kernel"), "{text}");
    assert!(text.contains("ok       rootfs.img"), "{text}");
}

/// The folders of a whole run through the VM. The home is in the long
/// `$TMPDIR` of macOS, as in a real test run.
#[cfg(target_os = "macos")]
struct VmRun {
    _tmp: tempfile::TempDir,
    root: std::path::PathBuf,
}

#[cfg(target_os = "macos")]
impl VmRun {
    fn paddock(&self) -> Command {
        let mut c = Command::new(BIN);
        c.env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("cfg"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("XDG_RUNTIME_DIR", self.root.join("run"))
            .env_remove("TMUX")
            .current_dir(self.root.join("project"))
            .stdin(Stdio::null());
        for v in paddock::proxy::PROXY_VARS {
            c.env_remove(v);
        }
        c
    }
}

/// Stop the VM and the tmux server of `paddock view`, also after a failure.
#[cfg(target_os = "macos")]
impl Drop for VmRun {
    fn drop(&mut self) {
        let _ = self.paddock().args(["vm", "stop"]).output();
        let uid = unsafe { libc::getuid() };
        let sock = paddock::sockpath::socket_dir(&self.root.join("run/paddock"), uid, "")
            .join("view.sock");
        if sock.exists() {
            let _ = Command::new("tmux")
                .arg("-S")
                .arg(&sock)
                .arg("kill-server")
                .stderr(Stdio::null())
                .status();
        }
    }
}

/// `paddock run fake --backend vm`: a fake harness (sh) runs in the Linux
/// guest, in the project at its host path, and writes a file that the host
/// sees. The exit code of the run is the exit code of paddock.
#[cfg(target_os = "macos")]
#[test]
fn a_fake_run_goes_through_the_vm() {
    let Some(assets) = std::env::var_os("PADDOCK_TEST_VM_ASSETS")
        .filter(|a| !a.is_empty())
        .map(std::path::PathBuf::from)
    else {
        eprintln!("skipped: set PADDOCK_TEST_VM_ASSETS to the folder of the VM files");
        return;
    };
    sign_helper();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let w = VmRun { _tmp: tmp, root };
    let root = &w.root;
    for d in [
        "home",
        "project",
        "cfg/paddock/harness.d",
        "data/paddock",
        "run",
    ] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(root.join("run"), std::fs::Permissions::from_mode(0o700)).unwrap();
    std::os::unix::fs::symlink(&assets, root.join("data/paddock/vm")).unwrap();
    std::fs::write(
        root.join("cfg/paddock/harness.d/fake.toml"),
        "name = \"fake\"\n[locate]\ncommands = [\"sh\"]\n[launch]\nargs = [\"-c\", \"echo fake-ok; uname -m; id -u; pwd; echo hi > vm.txt\"]\n",
    )
    .unwrap();
    let project = root.join("project");
    let out = w
        .paddock()
        .args(["run", "fake", "--backend", "vm", "--network", "none", "-y"])
        .arg(&project)
        .output()
        .unwrap();
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let console =
        std::fs::read_to_string(root.join("state/paddock/vm/console.log")).unwrap_or_default();
    let said = format!("{text}\n--- console.log ---\n{console}");
    assert!(out.status.success(), "{said}");
    let lines: Vec<&str> = text.lines().map(str::trim).collect();
    assert!(lines.contains(&"fake-ok"), "{said}");
    assert!(
        lines.contains(&"aarch64"),
        "the run was not in the guest: {said}"
    );
    assert!(
        lines.contains(&project.display().to_string().as_str()),
        "the project is not at its host path: {said}"
    );
    assert_eq!(
        std::fs::read_to_string(project.join("vm.txt")).unwrap_or_default(),
        "hi\n",
        "{said}"
    );
    // The run is over: no route of it is left.
    let out = w
        .paddock()
        .args(["vm", "status", "--json"])
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["running"], true, "{v}");
    assert_eq!(v["runs"], 0, "{v}");
}

/// `paddock vm setup --from DIR` installs the files of a build folder after
/// the sha256 checks, and `paddock vm remove` deletes them.
#[test]
fn vm_setup_from_a_folder_and_remove() {
    let t = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
    let from = t.path().join("build");
    std::fs::create_dir_all(&from).unwrap();
    let mut list = String::new();
    for name in ["kernel", "rootfs.img"] {
        let body = format!("fake {name}");
        std::fs::write(from.join(name), &body).unwrap();
        let sum = paddock::vm::assets::sha256_bytes(body.as_bytes());
        list.push_str(&format!(
            "[[file]]\nname = \"{name}\"\nurl = \"https://example.invalid/{name}\"\n\
             sha256 = \"{sum}\"\nsize = {}\nversion = \"1\"\n\n",
            body.len()
        ));
    }
    std::fs::write(from.join("assets.toml"), list).unwrap();
    let paddock = |args: &[&str]| {
        let out = Command::new(BIN)
            .env_clear()
            .env("HOME", t.path())
            .env("PATH", "/usr/bin:/bin")
            .env("XDG_RUNTIME_DIR", t.path().join("run"))
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).to_string(),
        )
    };
    let dir = from.display().to_string();
    // Without a terminal and without --yes: the list, then no change.
    let (code, text) = paddock(&["vm", "setup", "--from", &dir]);
    assert_eq!(code, Some(1), "{text}");
    assert!(text.contains("sha256: "), "{text}");
    assert!(text.contains("--yes` to go on"), "{text}");
    let vm = t.path().join(".local/share/paddock/vm");
    assert!(!vm.exists());

    let (code, text) = paddock(&["vm", "setup", "--from", &dir, "--yes"]);
    assert_eq!(code, Some(0), "{text}");
    assert_eq!(
        std::fs::read(vm.join("rootfs.img")).unwrap(),
        b"fake rootfs.img"
    );
    let (_, text) = paddock(&["vm", "status"]);
    assert!(text.contains("ok       kernel"), "{text}");

    let (code, text) = paddock(&["vm", "remove", "--yes"]);
    assert_eq!(code, Some(0), "{text}");
    assert!(!vm.exists());
}
