//! `paddock run --backend seatbelt` on macOS, end to end: a fake harness (a
//! shell script) in a temporary project reads and writes the project,
//! cannot read the home, reaches the network only through paddock's proxy,
//! and reaches the host with `paddock-host kv put` and `paddock-host done`.
//! Temporary folders only, also for the home. No real harness, no login,
//! and no outside host: refused names are in reserved domains.
#![cfg(target_os = "macos")]

use std::path::PathBuf;
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
    fn new(script: &str) -> Self {
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
        std::fs::write(root.join("home/secret.txt"), "secret").unwrap();
        std::fs::write(
            cfg.join("fake.toml"),
            format!(
                "name = \"fake\"\n[locate]\ncommands = [\"sh\"]\n[launch]\nargs = [\"-c\", {}]\n[network]\nendpoints = [\"model.example.invalid\"]\n[detach]\nargs = [\"{{prompt_file}}\"]\n",
                serde_json::to_string(script).unwrap()
            ),
        )
        .unwrap();
        World { _tmp: tmp, root }
    }

    fn paddock(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_paddock"));
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

    fn run(&self, network: &str, extra: &[&str]) -> Output {
        self.paddock()
            .args([
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
            .args(extra)
            .arg(self.root.join("project"))
            .output()
            .unwrap()
    }

    fn read(&self, f: &str) -> String {
        std::fs::read_to_string(self.root.join("project").join(f)).unwrap_or_default()
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
fn the_harness_reads_and_writes_the_project_but_not_the_home() {
    let script = concat!(
        "echo made > made.txt; cat made.txt > read.txt; ",
        "cat \"$(pwd)/../home\"/secret.txt > home.txt 2>/dev/null; echo $? > home-code.txt; ",
        "pwd > pwd.txt",
    );
    let w = World::new(script);
    let out = w.run("none", &[]);
    assert!(out.status.success(), "{}", say(&out));
    assert_eq!(w.read("read.txt"), "made\n");
    assert_eq!(
        w.read("pwd.txt").trim(),
        w.root.join("project").display().to_string()
    );
    assert_eq!(w.read("home.txt"), "", "the home file was readable");
    assert_ne!(w.read("home-code.txt").trim(), "0");
}

#[test]
fn the_harness_reaches_the_network_only_through_the_proxy() {
    let other = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = other.local_addr().unwrap().port();
    std::thread::spawn(move || for _ in other.incoming() {});
    let script = format!(
        "env > env.txt; \
         /usr/bin/nc -z -G 2 127.0.0.1 {port}; echo $? > direct.txt; \
         /usr/bin/curl -s --max-time 10 -o /dev/null -w '%{{http_code}}' http://refused.example.invalid/ > refused.txt"
    );
    let w = World::new(&script);
    let out = w.run("api", &[]);
    assert!(out.status.success(), "{}", say(&out));
    let env = w.read("env.txt");
    assert!(env.contains("HTTPS_PROXY=http://127.0.0.1:"), "{env}");
    assert!(!env.contains("HTTPS_PROXY=http://127.0.0.1:3131"), "{env}");
    assert_ne!(
        w.read("direct.txt").trim(),
        "0",
        "a direct connection worked"
    );
    assert_eq!(w.read("refused.txt"), "403", "{}", say(&out));
}

#[test]
fn paddock_host_kv_put_and_done_reach_the_host() {
    let script = concat!(
        "paddock-host kv put note hello; echo $? > put.txt; ",
        "paddock-host kv get note > got.txt; ",
        "paddock-host done 'not detached' > done.txt 2>&1; echo $? > done-code.txt; ",
        "paddock-host borrow /tmp > borrow.txt 2>&1; echo $? > borrow-code.txt",
    );
    let w = World::new(script);
    let out = w.run("none", &[]);
    assert!(out.status.success(), "{}", say(&out));
    assert_eq!(w.read("put.txt").trim(), "0", "{}", say(&out));
    assert_eq!(w.read("got.txt"), "hello");
    // The broker answered: a run that is not detached has nothing to end.
    assert_eq!(w.read("done-code.txt").trim(), "4");
    assert!(w.read("done.txt").contains("not a detached run"));
    assert!(w.read("borrow.txt").contains("not there yet on macOS"));
}

#[test]
fn paddock_host_done_ends_a_detached_run() {
    if Command::new("tmux").arg("-V").output().is_err() {
        eprintln!("skipped: needs tmux");
        return;
    }
    let w = World::new("paddock-host done --status ok \"all good\"; exec sleep 60");
    let out = w.run("none", &["--detach", "--prompt", "finish"]);
    assert!(out.status.success(), "{}", say(&out));
    let id = String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .unwrap()
        .to_string();
    let wait = w
        .paddock()
        .args(["wait", &id, "--timeout", "60"])
        .output()
        .unwrap();
    let sock = w.root.join("run/paddock/view.sock");
    let _ = Command::new("tmux")
        .arg("-S")
        .arg(&sock)
        .arg("kill-server")
        .status();
    assert_eq!(wait.status.code(), Some(0), "{}", say(&wait));
    assert_eq!(String::from_utf8_lossy(&wait.stdout).trim(), "ok: all good");
}
