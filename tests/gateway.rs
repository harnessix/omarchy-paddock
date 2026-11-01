//! The gateway `/dev/paddock/gw` end to end, in a bwrap sandbox: a host
//! request goes through the device, a read that reaches its time limit says
//! "still waiting", the same request sent again attaches to the one that
//! waits (one card, one run), and `paddock-host` gets the result through
//! the gateway. Nothing here talks to the network.

mod common;

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn fuse_works() -> bool {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/fuse")
        .is_ok()
        && ["fusermount3", "fusermount"].iter().any(|h| {
            Command::new("sh")
                .args(["-c", &format!("command -v {h}")])
                .stdout(Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        })
}

/// In the sandbox: the same exec request first as a raw device read that
/// gives up after one second, then through paddock-host.
const SCRIPT: &str = r#"
if [ ! -e /dev/paddock/gw ]; then echo NOGW; exit 0; fi
exec 3<>/dev/paddock/gw
printf '{"v":1,"cap":"exec","args":{"argv":["sh","-c","echo ran >> ran.log; echo hello"]},"reason":"","timeout":1}' >&3
if cat <&3 >/dev/null 2>/tmp/err; then echo RAW_OK; else echo "RAW_ERR $(cat /tmp/err)"; fi
exec 3<&-
out=$(paddock-host exec -- sh -c 'echo ran >> ran.log; echo hello'); e=$?
echo "EXEC_EXIT $e"
echo "EXEC_OUT $out"
"#;

struct World {
    _tmp: tempfile::TempDir,
    root: PathBuf,
}

fn world() -> World {
    let tmp = common::tempdir();
    let root = tmp.path().to_path_buf();
    let cfg = root.join("cfg/paddock");
    std::fs::create_dir_all(cfg.join("harness.d")).unwrap();
    std::fs::create_dir_all(root.join("run")).unwrap();
    std::fs::create_dir_all(root.join("project")).unwrap();
    std::fs::write(cfg.join("config.toml"), "").unwrap();
    std::fs::write(
        cfg.join("harness.d/gwfake.toml"),
        format!(
            "name = \"gwfake\"\n[locate]\ncommands = [\"sh\"]\n[launch]\nargs = [\"-c\", {}]\n",
            serde_json::to_string(SCRIPT).unwrap()
        ),
    )
    .unwrap();
    World { _tmp: tmp, root }
}

fn paddock(w: &World) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_paddock"));
    c.env("XDG_CONFIG_HOME", w.root.join("cfg"))
        .env("XDG_DATA_HOME", w.root.join("data"))
        .env("XDG_STATE_HOME", w.root.join("state"))
        .env("XDG_CACHE_HOME", w.root.join("cache"))
        .env("XDG_RUNTIME_DIR", w.root.join("run"))
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
        .env("LANG", "C")
        .env("LC_ALL", "C");
    c
}

fn pending(w: &World) -> Vec<serde_json::Value> {
    let out = paddock(w).args(["approvals", "--json"]).output().unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
    v["pending"].as_array().cloned().unwrap_or_default()
}

#[test]
fn a_host_request_goes_through_the_gateway_and_attaches_when_sent_again() {
    if !common::bwrap_works() || !fuse_works() {
        eprintln!("skipped: bwrap or FUSE does not work here");
        return;
    }
    let w = world();
    let child = paddock(&w)
        .args([
            "run",
            "gwfake",
            "--backend",
            "bwrap",
            "--network",
            "none",
            "--env",
            "clean",
            "-y",
        ])
        .arg(w.root.join("project"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // The card appears, and stays one card while the raw read gives up and
    // paddock-host sends the same request again.
    let start = Instant::now();
    let mut list = Vec::new();
    while start.elapsed() < Duration::from_secs(60) {
        list = pending(&w);
        if !list.is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if list.is_empty() {
        let out = child.wait_with_output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        if text.contains("NOGW") {
            eprintln!("skipped: the sandbox has no /dev/paddock/gw here");
            return;
        }
        panic!("no card:\n{text}\n{}", String::from_utf8_lossy(&out.stderr));
    }
    std::thread::sleep(Duration::from_secs(3));
    let list = pending(&w);
    assert_eq!(list.len(), 1, "{list:?}");
    assert_eq!(list[0]["capability"], "exec");
    let id = list[0]["id"].as_str().unwrap().to_string();
    let ans = paddock(&w)
        .args(["approvals", "answer", &id, "once"])
        .output()
        .unwrap();
    assert!(ans.status.success(), "{ans:?}");

    let out = child.wait_with_output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{text}\n{err}");
    let line = |k: &str| -> String {
        text.lines()
            .find_map(|l| l.strip_prefix(&format!("{k} ")))
            .unwrap_or_else(|| panic!("no {k} in:\n{text}\n{err}"))
            .to_string()
    };
    // The raw read reached its time limit: still waiting.
    assert!(line("RAW_ERR").contains("Timer expired"), "{text}");
    assert_eq!(line("EXEC_EXIT"), "0", "{text}\n{err}");
    assert_eq!(line("EXEC_OUT"), "hello");
    // One approval, one run.
    let ran = std::fs::read_to_string(w.root.join("project/ran.log")).unwrap();
    assert_eq!(ran, "ran\n");
}

/// The executor itself, on its socket: a job signed with another key is
/// refused, a good one runs once, and the same grant again is refused.
#[test]
fn the_executor_refuses_forged_and_replayed_grants() {
    use std::io::{BufRead, BufReader, Write};
    let w = world();
    let mut vars = std::collections::BTreeMap::new();
    for (k, d) in [
        ("XDG_STATE_HOME", "state"),
        ("XDG_RUNTIME_DIR", "run"),
        ("XDG_CONFIG_HOME", "cfg"),
    ] {
        vars.insert(k.to_string(), w.root.join(d).display().to_string());
    }
    let env = paddock::env::HostEnv::from_parts(
        w.root.join("home"),
        "demo".into(),
        1000,
        1000,
        vars.clone(),
    );
    let mut daemon = executor(&w, &vars);
    let sock = paddock::gatewayd::socket_path(&env);
    let ask = |job: &paddock::gateway::Job| -> serde_json::Value {
        let mut s = std::os::unix::net::UnixStream::connect(&sock).unwrap();
        writeln!(s, "{}", serde_json::to_string(job).unwrap()).unwrap();
        let mut line = String::new();
        BufReader::new(s).read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    };
    let project = w.root.join("project");
    let req = paddock::broker::Request {
        v: 1,
        cap: "exec".into(),
        args: [("argv".to_string(), serde_json::json!(["pwd"]))]
            .into_iter()
            .collect(),
        reason: String::new(),
    };
    let forged = paddock::gateway::sign(
        &[1u8; 32],
        "s1",
        &project,
        &req,
        Default::default(),
        serde_json::Value::Null,
    )
    .unwrap();
    let r = ask(&forged);
    assert!(
        r["error"].as_str().unwrap_or("").contains("signature"),
        "{r}"
    );
    let key = paddock::gateway::key(&env).unwrap();
    let mut path = std::collections::BTreeMap::new();
    path.insert("PATH".to_string(), std::env::var("PATH").unwrap());
    let good =
        paddock::gateway::sign(&key, "s1", &project, &req, path, serde_json::Value::Null).unwrap();
    let r = ask(&good);
    if common::bwrap_works() {
        assert_eq!(
            r["ok"]["stdout"].as_str().unwrap_or("").trim(),
            project.display().to_string(),
            "{r}"
        );
    } else {
        // No way to isolate the command here (no bwrap, or a bwrap that
        // cannot start a sandbox): the executor still runs and says so,
        // instead of running it without isolation.
        assert!(
            r["error"].as_str().unwrap_or("").contains("isolation"),
            "{r}"
        );
    }
    let r = ask(&good);
    assert!(
        r["error"].as_str().unwrap_or("").contains("used already"),
        "{r}"
    );
    let _ = daemon.kill();
    let _ = daemon.wait();
}

/// Start the executor with the folders of `w`, and wait for its socket.
fn executor(w: &World, vars: &std::collections::BTreeMap<String, String>) -> std::process::Child {
    let env = paddock::env::HostEnv::from_parts(
        w.root.join("home"),
        "demo".into(),
        1000,
        1000,
        vars.clone(),
    );
    let child = Command::new(env!("CARGO_BIN_EXE_paddock"))
        .args(["gatewayd", "--idle", "5"])
        .envs(vars)
        // The executor runs on the host only. When the tests themselves run
        // in a paddock sandbox, it must not see that sandbox's mark.
        .env_remove("PADDOCK")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let sock = paddock::gatewayd::socket_path(&env);
    let start = Instant::now();
    while !sock.exists() && start.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(50));
    }
    child
}

fn exec_job(
    w: &World,
    vars: &std::collections::BTreeMap<String, String>,
    script: &str,
) -> serde_json::Value {
    use std::io::{BufRead, BufReader, Write};
    let env = paddock::env::HostEnv::from_parts(
        w.root.join("home"),
        "demo".into(),
        1000,
        1000,
        vars.clone(),
    );
    let req = paddock::broker::Request {
        v: 1,
        cap: "exec".into(),
        args: [("argv".to_string(), serde_json::json!(["sh", "-c", script]))]
            .into_iter()
            .collect(),
        reason: String::new(),
    };
    let mut host = std::collections::BTreeMap::new();
    host.insert("PATH".to_string(), std::env::var("PATH").unwrap());
    host.insert("PADDOCK_TEST_TOKEN".to_string(), "secret".to_string());
    let key = paddock::gateway::key(&env).unwrap();
    let job = paddock::gateway::sign(
        &key,
        "s1",
        &w.root.join("project"),
        &req,
        host,
        serde_json::Value::Null,
    )
    .unwrap();
    let mut s =
        std::os::unix::net::UnixStream::connect(paddock::gatewayd::socket_path(&env)).unwrap();
    writeln!(s, "{}", serde_json::to_string(&job).unwrap()).unwrap();
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

fn exec_vars(w: &World) -> std::collections::BTreeMap<String, String> {
    let mut vars = std::collections::BTreeMap::new();
    for (k, d) in [
        ("XDG_STATE_HOME", "state"),
        ("XDG_RUNTIME_DIR", "run"),
        ("XDG_CONFIG_HOME", "cfg"),
    ] {
        vars.insert(k.to_string(), w.root.join(d).display().to_string());
    }
    // systemd-run needs the real runtime folder and session bus of the user
    // manager, and the real HOME for the unit; the test folders stay ours.
    for k in ["HOME", "PATH", "DBUS_SESSION_BUS_ADDRESS"] {
        if let Ok(v) = std::env::var(k) {
            vars.insert(k.to_string(), v);
        }
    }
    vars
}

#[test]
fn an_approved_exec_runs_isolated() {
    if !common::bwrap_works() {
        eprintln!("skipped: bwrap does not work here");
        return;
    }
    let w = world();
    std::fs::write(w.root.join("outside.txt"), "private").unwrap();
    let vars = exec_vars(&w);
    let mut daemon = executor(&w, &vars);
    let outside = w.root.join("outside.txt");
    let r = exec_job(
        &w,
        &vars,
        &format!(
            "test -e '{}' && echo OUTSIDE_VISIBLE || echo OUTSIDE_HIDDEN; \
             touch /usr/paddock-x 2>/dev/null && echo USR_WRITABLE || echo USR_RO; \
             touch made && echo PROJECT_RW; \
             echo TOKEN=${{PADDOCK_TEST_TOKEN:-none}}; \
             test -n \"$PATH\" && echo PATH_SET",
            outside.display()
        ),
    );
    let out = r["ok"]["stdout"].as_str().unwrap_or_default().to_string();
    assert_eq!(r["ok"]["exit_code"], 0, "{r}");
    for want in [
        "OUTSIDE_HIDDEN",
        "USR_RO",
        "PROJECT_RW",
        "TOKEN=none",
        "PATH_SET",
    ] {
        assert!(out.contains(want), "{want} missing:\n{r}");
    }
    assert!(w.root.join("project/made").exists());
    let _ = daemon.kill();
    let _ = daemon.wait();
    // The killed executor left its socket file behind.
    let _ = std::fs::remove_file(w.root.join("run/paddock/gatewayd.sock"));

    // [exec] network = false: only the loopback device.
    std::fs::write(
        w.root.join("cfg/paddock/config.toml"),
        "[exec]\nnetwork = false\n",
    )
    .unwrap();
    let mut daemon = executor(&w, &vars);
    // /proc/net/dev lists the devices of the command's own network
    // namespace (/sys can show the host's).
    let r = exec_job(
        &w,
        &vars,
        "tail -n +3 /proc/net/dev | cut -d: -f1 | tr -d ' '",
    );
    assert_eq!(
        r["ok"]["stdout"].as_str().unwrap_or_default().trim(),
        "lo",
        "{r}"
    );
    let _ = daemon.kill();
    let _ = daemon.wait();
}

/// An approved push runs in the real executor, isolated: from the scratch
/// repository of the card, to a bare repository on this machine, and the
/// home folder stays unchanged.
#[test]
fn an_approved_push_runs_in_the_executor() {
    if !common::bwrap_works() {
        eprintln!("skipped: bwrap does not work here");
        return;
    }
    let w = world();
    let git = |dir: &std::path::Path, args: &[&str]| -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@localhost"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    let work = w.root.join("project");
    let remote = w.root.join("remote.git");
    git(
        &w.root,
        &[
            "init",
            "-q",
            "--bare",
            "-b",
            "main",
            remote.to_str().unwrap(),
        ],
    );
    git(&work, &["init", "-q", "-b", "main"]);
    git(&work, &["commit", "-q", "--allow-empty", "-m", "first"]);
    git(
        &work,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    git(&work, &["push", "-q", "origin", "main"]);
    git(&work, &["commit", "-q", "--allow-empty", "-m", "second"]);
    let head = git(&work, &["rev-parse", "HEAD"]);

    let vars = exec_vars(&w);
    let env = paddock::env::HostEnv::from_parts(
        w.root.join("home"),
        "demo".into(),
        1000,
        1000,
        vars.clone(),
    );
    let repo = paddock::broker::git::Repo::snapshot(&work).unwrap();
    let run_dir = w.root.join("run/paddock/s1");
    std::fs::create_dir_all(&run_dir).unwrap();
    let mut args = std::collections::BTreeMap::new();
    args.insert("remote".to_string(), serde_json::json!("origin"));
    args.insert(
        "url".to_string(),
        serde_json::json!(remote.to_str().unwrap()),
    );
    args.insert(
        "refs".to_string(),
        serde_json::json!([{"src": "HEAD", "sha": head, "dst": "refs/heads/main"}]),
    );
    let plan = paddock::broker::git::plan(Some(&repo), &run_dir, &args).unwrap();
    let req = paddock::broker::Request {
        v: 1,
        cap: "git-push".into(),
        args,
        reason: String::new(),
    };
    let mut daemon = executor(&w, &vars);
    let key = paddock::gateway::key(&env).unwrap();
    let mut host = std::collections::BTreeMap::new();
    for k in ["PATH", "HOME"] {
        host.insert(k.to_string(), std::env::var(k).unwrap());
    }
    let job = paddock::gateway::sign(
        &key,
        "s1",
        &work,
        &req,
        host,
        serde_json::to_value(plan.exec()).unwrap(),
    )
    .unwrap();
    let r = {
        use std::io::{BufRead, BufReader, Write};
        let mut s =
            std::os::unix::net::UnixStream::connect(paddock::gatewayd::socket_path(&env)).unwrap();
        writeln!(s, "{}", serde_json::to_string(&job).unwrap()).unwrap();
        let mut line = String::new();
        BufReader::new(s).read_line(&mut line).unwrap();
        serde_json::from_str::<serde_json::Value>(&line).unwrap()
    };
    assert_eq!(r["ok"]["refs"][0]["ok"], true, "{r}");
    assert_eq!(git(&remote, &["rev-parse", "main"]), head);
    let _ = daemon.kill();
    let _ = daemon.wait();
}

/// An approved gh call runs in the real executor: the program that the
/// broker found, the body on standard input, its own login variables, and
/// a home folder that it cannot change.
#[test]
fn an_approved_gh_call_runs_in_the_executor() {
    if !common::bwrap_works() {
        eprintln!("skipped: bwrap does not work here");
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    let w = world();
    let bin = w.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let gh = bin.join("gh");
    std::fs::write(
        &gh,
        "#!/bin/sh\necho \"ARGS $*\"\necho \"BODY $(cat)\"\necho \"PROMPT ${GH_PROMPT_DISABLED:-}\"\necho \"TOKEN ${GITHUB_TOKEN:-none}\"\nif touch \"$HOME/paddock-gh-probe\" 2>/dev/null; then echo HOME_WRITABLE; rm -f \"$HOME/paddock-gh-probe\"; else echo HOME_READONLY; fi\n",
    )
    .unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let vars = exec_vars(&w);
    let env = paddock::env::HostEnv::from_parts(
        w.root.join("home"),
        "demo".into(),
        1000,
        1000,
        vars.clone(),
    );
    let workdir = w.root.join("run/paddock/s1/req-1");
    std::fs::create_dir_all(&workdir).unwrap();
    let plan = paddock::broker::forge::ForgeExec {
        tool: gh.display().to_string(),
        argv: vec![
            "pr".into(),
            "create".into(),
            "--body-file".into(),
            "-".into(),
        ],
        stdin: Some("the body".into()),
        timeout: 30,
        workdir,
    };
    let req = paddock::broker::Request {
        v: 1,
        cap: "forge".into(),
        args: Default::default(),
        reason: String::new(),
    };
    let mut host = std::collections::BTreeMap::new();
    for k in ["PATH", "HOME"] {
        host.insert(k.to_string(), std::env::var(k).unwrap());
    }
    host.insert("GITHUB_TOKEN".to_string(), "t0k".to_string());
    host.insert("AWS_SECRET_ACCESS_KEY".to_string(), "nope".to_string());
    let mut daemon = executor(&w, &vars);
    let key = paddock::gateway::key(&env).unwrap();
    let job = paddock::gateway::sign(
        &key,
        "s1",
        &w.root.join("project"),
        &req,
        host,
        serde_json::to_value(&plan).unwrap(),
    )
    .unwrap();
    let r = {
        use std::io::{BufRead, BufReader, Write};
        let mut s =
            std::os::unix::net::UnixStream::connect(paddock::gatewayd::socket_path(&env)).unwrap();
        writeln!(s, "{}", serde_json::to_string(&job).unwrap()).unwrap();
        let mut line = String::new();
        BufReader::new(s).read_line(&mut line).unwrap();
        serde_json::from_str::<serde_json::Value>(&line).unwrap()
    };
    let out = r["ok"]["stdout"].as_str().unwrap_or_default().to_string();
    for want in [
        "ARGS pr create --body-file -",
        "BODY the body",
        "PROMPT 1",
        "TOKEN t0k",
        "HOME_READONLY",
    ] {
        assert!(out.contains(want), "{want} missing:\n{r}");
    }
    let _ = daemon.kill();
    let _ = daemon.wait();
}
