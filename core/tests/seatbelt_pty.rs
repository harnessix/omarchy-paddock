//! `paddock run --backend seatbelt` on macOS: ptys from the host and the
//! localhost port range. The fake harness is the probe of
//! src/ptyshim/probe.c, built with `cc` into the temporary project, or a
//! shell script. Temporary folders only, also for the home and the
//! configuration. No real harness, no login, no outside host.
#![cfg(target_os = "macos")]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

struct World {
    _tmp: tempfile::TempDir,
    root: PathBuf,
}

/// A one-shot run starts a tmux server for `paddock view`. On macOS its
/// socket lives in `/tmp/paddock-<uid>`, outside the test folder, so stop it.
impl Drop for World {
    fn drop(&mut self) {
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

impl World {
    /// A fake harness `fake` that runs `program` with `args`, and the user
    /// configuration `config`.
    fn new(program: &str, args: &[&str], config: &str) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let cfg = root.join("cfg/paddock/harness.d");
        std::fs::create_dir_all(&cfg).unwrap();
        for d in ["project", "home"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        let run = root.join("run");
        std::fs::create_dir_all(&run).unwrap();
        std::fs::set_permissions(&run, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .unwrap();
        std::fs::write(root.join("cfg/paddock/config.toml"), config).unwrap();
        std::fs::write(
            cfg.join("fake.toml"),
            format!(
                "name = \"fake\"\n[locate]\ncommands = [{}]\n[launch]\nargs = {}\n",
                serde_json::to_string(program).unwrap(),
                serde_json::to_string(args).unwrap()
            ),
        )
        .unwrap();
        World { _tmp: tmp, root }
    }

    /// The probe, built in a tools folder next to the project: paddock
    /// refuses a harness in a folder that the sandbox can write, and it
    /// mounts the install folder (`tools`) read-only.
    fn with_probe(args: &[&str], config: &str) -> Self {
        let w = World::new("probe", args, config);
        std::fs::create_dir_all(w.root.join("tools/bin")).unwrap();
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ptyshim/probe.c");
        let st = Command::new("cc")
            .arg("-o")
            .arg(w.root.join("tools/bin/probe"))
            .arg(src)
            .status()
            .expect("these tests need cc (the Xcode command line tools)");
        assert!(st.success());
        w
    }

    fn project(&self) -> PathBuf {
        self.root.join("project")
    }

    fn run(&self, network: &str) -> Output {
        let path = format!(
            "{}:{}",
            self.root.join("tools/bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut c = Command::new(env!("CARGO_BIN_EXE_paddock"));
        c.env("HOME", self.root.join("home"))
            .env("PATH", path)
            .env("XDG_CONFIG_HOME", self.root.join("cfg"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("XDG_RUNTIME_DIR", self.root.join("run"))
            .env_remove("TMUX")
            .current_dir(self.project())
            .stdin(Stdio::null());
        for v in paddock::proxy::PROXY_VARS {
            c.env_remove(v);
        }
        c.args([
            "run",
            "fake",
            "--backend",
            "seatbelt",
            "--network",
            network,
            "--env",
            "clean",
            "-y",
        ])
        .arg(self.project())
        .output()
        .unwrap()
    }

    fn read(&self, f: &str) -> String {
        std::fs::read_to_string(self.project().join(f)).unwrap_or_default()
    }
}

fn say(o: &Output) -> String {
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

#[test]
fn the_harness_gets_a_pty_from_paddock_and_uses_it() {
    for (what, lines) in [
        (
            "openpty",
            &[
                "openpty ok",
                "ttyname same",
                "raw ok",
                "roundtrip ok",
                "winsize ok",
            ][..],
        ),
        ("forkpty", &["forkpty ok"][..]),
        ("posix", &["posix_openpt ok"][..]),
    ] {
        let w = World::with_probe(&[what], "");
        let out = w.run("none");
        assert!(out.status.success(), "{}", say(&out));
        assert!(!say(&out).contains("no pty for"), "{}", say(&out));
        let got = w.read("probe.txt");
        for l in lines {
            assert!(got.contains(l), "{what}: {l}\n{got}\n{}", say(&out));
        }
        assert!(got.contains("name /dev/ttys") || what != "openpty", "{got}");
    }
}

#[test]
fn tiocsti_fails_on_the_pty_from_paddock() {
    let w = World::with_probe(&["openpty"], "");
    let out = w.run("none");
    assert!(out.status.success(), "{}", say(&out));
    let got = w.read("probe.txt");
    assert!(got.contains("openpty ok"), "{got}\n{}", say(&out));
    assert!(got.contains("tiocsti refused"), "{got}");
}

#[test]
fn the_harness_cannot_open_a_terminal_that_paddock_did_not_give_it() {
    // The stand-in for the user's terminal: a pty that the test opens
    // outside the sandbox.
    let (_master, _slave, name) = paddock::ptybroker::open_pty().unwrap();
    assert!(name.starts_with("/dev/ttys"), "{name}");
    let w = World::with_probe(&["open", &name], "");
    let out = w.run("none");
    assert!(out.status.success(), "{}", say(&out));
    assert_eq!(w.read("probe.txt").trim(), "open refused", "{}", say(&out));
    // Also not by a shell. sh ignores paddock's pty library, so it may
    // open /dev/ptmx for a pty of its own (App Sandbox rules).
    let script = format!(
        "(exec 3<>{name}) 2>/dev/null; echo $? > tty.txt; (exec 3<>/dev/ptmx) 2>/dev/null; echo $? > ptmx.txt"
    );
    let w = World::new("sh", &["-c", &script], "");
    let out = w.run("none");
    assert!(out.status.success(), "{}", say(&out));
    assert_ne!(w.read("tty.txt").trim(), "0", "the other terminal opened");
    assert_eq!(w.read("ptmx.txt").trim(), "0", "/dev/ptmx for its own pty");
    // sh is protected by macOS: paddock says it gets no pty from paddock.
    assert!(
        say(&out).contains("no pty from paddock for fake"),
        "{}",
        say(&out)
    );
}

fn free_port() -> u16 {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[test]
fn a_server_in_the_port_range_is_reachable_and_one_outside_is_not() {
    let inside = free_port();
    // Outside the range: a server of the Mac, and one in the sandbox.
    let host = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let host_port = host.local_addr().unwrap().port();
    std::thread::spawn(move || for _ in host.incoming() {});
    let outside = free_port();
    let script = format!(
        "/usr/bin/nc -l 127.0.0.1 {inside} >/dev/null & s1=$!; \
         /usr/bin/nc -l 127.0.0.1 {outside} >/dev/null & s2=$!; sleep 1; \
         /usr/bin/nc -z -G 2 127.0.0.1 {inside}; echo $? > inside.txt; \
         /usr/bin/nc -z -G 2 127.0.0.1 {outside}; echo $? > outside.txt; \
         /usr/bin/nc -z -G 2 127.0.0.1 {host_port}; echo $? > host.txt; \
         kill $s1 $s2 2>/dev/null; exit 0"
    );
    let config = format!("[macos]\nlocalhost_ports = \"{inside}\"\n");
    let w = World::new("sh", &["-c", &script], &config);
    let out = w.run("egress");
    assert!(out.status.success(), "{}", say(&out));
    assert!(
        say(&out).contains(&format!("localhost ports {inside}-{inside} are open")),
        "{}",
        say(&out)
    );
    assert_eq!(w.read("inside.txt").trim(), "0", "{}", say(&out));
    assert_ne!(
        w.read("outside.txt").trim(),
        "0",
        "a port outside the range"
    );
    assert_ne!(w.read("host.txt").trim(), "0", "a port of the Mac");
    // Without the range, the port inside stays closed.
    let w = World::new("sh", &["-c", &script], "");
    let out = w.run("egress");
    assert!(out.status.success(), "{}", say(&out));
    assert_ne!(w.read("inside.txt").trim(), "0", "{}", say(&out));
}
