//! `paddock pasture` through the binary, in a temporary home: list, and
//! remove the data of a pasture only with --yes.

use paddock::env::HostEnv;
use paddock::kv::shard::Ns;
use paddock::kv::store::{Quota, Store, Writer};
use std::path::Path;
use std::process::Command;

fn paddock(t: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_paddock"))
        .args(args)
        .env_clear()
        .env("HOME", t.join("home"))
        .env("USER", "demo")
        .env("PATH", "/usr/bin:/bin")
        .env("XDG_RUNTIME_DIR", t.join("run"))
        .current_dir(t.join("project"))
        .output()
        .unwrap()
}

fn text(o: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

#[test]
fn list_and_remove_a_pasture() {
    let t = tempfile::tempdir().unwrap();
    let home = t.path().join("home");
    let project = t.path().join("project");
    for d in [&home, &project, &t.path().join("run")] {
        std::fs::create_dir_all(d).unwrap();
    }
    let project = project.canonicalize().unwrap();
    let env = HostEnv::from_parts(home.clone(), "demo".into(), 1000, 1000, Default::default());
    // A pasture with one key and a file in its shared folder.
    let ns = Ns::pasture(&project, "team").unwrap();
    let file = ns.file(&paddock::kv::kv_root(&env)).unwrap();
    Store::open(&file)
        .unwrap()
        .put(
            &ns.key(),
            "task:1",
            "todo",
            None,
            None,
            &Writer {
                sandbox: "a1",
                harness: "claude",
            },
            Quota {
                bytes: 1 << 20,
                keys: 10,
            },
            paddock::state::now(),
        )
        .unwrap();
    let ground = paddock::pasture::ground(&env, &project, "team");
    std::fs::create_dir_all(&ground).unwrap();
    std::fs::write(ground.join("notes.md"), "shared").unwrap();

    let out = paddock(t.path(), &["pasture", "list"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("team"), "{}", text(&out));
    let out = paddock(t.path(), &["pasture", "list", "--json"]);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["pastures"][0]["name"], "team");
    assert_eq!(v["pastures"][0]["keys"], 1);

    // Without --yes it only says what it would delete.
    let out = paddock(t.path(), &["pasture", "remove", "team"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(file.exists() && ground.join("notes.md").exists());
    for bad in ["solo-a1", "../x"] {
        let out = paddock(t.path(), &["pasture", "remove", bad, "--yes"]);
        assert!(!out.status.success(), "{bad}: {}", text(&out));
    }

    let out = paddock(t.path(), &["pasture", "remove", "team", "--yes"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(!file.exists());
    assert!(!ground.exists());
    let out = paddock(t.path(), &["pasture", "remove", "team", "--yes"]);
    assert!(text(&out).contains("has no data"), "{}", text(&out));
}

// ---- shared ports ------------------------------------------------------

mod common;

use paddock::broker::{Answer, Approver, Pending};
use paddock::kv::{Identity, KvCfg, Service};
use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::time::{Duration, Instant};

struct NoOne;
impl Approver for NoOne {
    fn ask(&self, _: &HostEnv, _: &Pending, _: Duration) -> Option<Answer> {
        None
    }
}

/// A TCP server that answers each line with "echo:" and the line.
fn echo_server() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for c in l.incoming().flatten() {
            std::thread::spawn(move || {
                let mut c = c;
                let mut buf = [0u8; 1024];
                while let Ok(n) = c.read(&mut buf) {
                    if n == 0 || c.write_all(b"echo:").is_err() || c.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
            });
        }
    });
    port
}

fn until(what: &str, mut f: impl FnMut() -> bool) {
    let start = Instant::now();
    while !f() {
        assert!(
            start.elapsed() < Duration::from_secs(15),
            "timed out: {what}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

struct Member {
    sock: std::path::PathBuf,
    ground: std::path::PathBuf,
    _served: paddock::kv::Served,
}

impl Member {
    fn host(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_paddock"));
        c.arg("host")
            .args(args)
            .env("PADDOCK_KV_SOCKET", &self.sock)
            .env("PADDOCK_PASTURE", &self.ground);
        c
    }
    fn run(&self, args: &[&str]) -> std::process::Output {
        self.host(args).output().unwrap()
    }
}

struct Kill(std::process::Child);
impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Two members in one network: the owner side of a shared port works
/// through the socket in the pasture folder, the other member skips the
/// port because its own 127.0.0.1 has it already, and the lease decides
/// conflicts. The far side on 127.0.0.1 needs its own network: see the
/// bubblewrap test below and the unit tests of the relay.
#[test]
fn a_member_shares_a_port_through_the_pasture_folder() {
    let t = tempfile::tempdir().unwrap();
    let home = t.path().join("home");
    let ground = t.path().join("ground");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&ground).unwrap();
    let env = HostEnv::from_parts(home, "demo".into(), 1000, 1000, Default::default());
    let member = |id: &str| {
        let svc = Service::new(
            env.clone(),
            Identity {
                sandbox: id.into(),
                harness: "sh".into(),
                project: "/src/app".into(),
                pasture: Some("team".into()),
                parent: None,
                chain: vec![],
                tree: None,
            },
            KvCfg::default(),
            std::sync::Arc::new(NoOne),
            Duration::from_secs(1),
        );
        let sock = t.path().join(format!("{id}.sock"));
        let served = paddock::kv::serve(&sock, svc).unwrap();
        Member {
            sock,
            ground: ground.clone(),
            _served: served,
        }
    };
    let a = member("a1");
    let b = member("b1");
    let port = echo_server();
    let p = port.to_string();
    // Ports below 1024 never.
    assert_eq!(a.run(&["pasture", "share", "80"]).status.code(), Some(2));

    let _ha = Kill(a.host(&["pasture", "serve"]).spawn().unwrap());
    let _hb = Kill(b.host(&["pasture", "serve"]).spawn().unwrap());
    let out = a.run(&["pasture", "share", &p]);
    assert!(out.status.success(), "{}", text(&out));
    let sock = ground.join(".ports").join(format!("{port}.sock"));
    assert!(sock.exists());
    let mut s = std::os::unix::net::UnixStream::connect(&sock).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(b"hi").unwrap();
    let mut buf = [0u8; 7];
    s.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"echo:hi");

    // The second member is refused with the name of the owner.
    let out = b.run(&["pasture", "share", &p]);
    assert_eq!(out.status.code(), Some(9), "{}", text(&out));
    assert!(text(&out).contains("sh (sandbox a1)"), "{}", text(&out));

    // members: both, the port of a1, and why b1 skips it.
    until("b1 records the skipped port", || {
        let out = b.run(&["pasture", "members", "--json"]);
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
        let m = v["members"].as_array().cloned().unwrap_or_default();
        let a1 = m.iter().find(|m| m["sandbox"] == "a1");
        let b1 = m.iter().find(|m| m["sandbox"] == "b1");
        a1.is_some_and(|a| a["ports"][0] == port && a["harness"] == "sh")
            && b1.is_some_and(|b| b["you"] == true && b["skipped"][&p].is_string())
    });
    let out = b.run(&["pasture", "members"]);
    assert!(
        text(&out).contains(&format!("skips {port}")),
        "{}",
        text(&out)
    );

    // The host lists the port.
    let ps = paddock::kv::host::pastures(&env, Path::new("/src/app")).unwrap();
    assert_eq!(ps[0].ports[0].port, port);
    assert_eq!(ps[0].ports[0].sandbox, "a1");

    // b1 cannot unshare it; a1 can, and the socket goes.
    assert_ne!(b.run(&["pasture", "unshare", &p]).status.code(), Some(0));
    let out = a.run(&["pasture", "unshare", &p]);
    assert!(out.status.success(), "{}", text(&out));
    until("the socket goes", || !sock.exists());
    assert_eq!(a.run(&["pasture", "unshare", &p]).status.code(), Some(1));
    // Now b1 can take it.
    let out = b.run(&["pasture", "share", &p]);
    assert!(out.status.success(), "{}", text(&out));
}

// ---- end to end, in bubblewrap ---------------------------------------------

/// Each wait prints WAIT, the key and the exit code when it fails, and each
/// member ends with its view of the pasture (LIST) and its helper log
/// (HELPER), so that a failure explains itself. The keys are written once,
/// so every watch starts at version 0: a write before the watch counts.
const LOG: &str = r#"
wait_for() { paddock-host kv watch "$1" --version 0 --scope pasture --timeout "${2:-12}" >/dev/null 2>/tmp/w; c=$?; [ $c = 0 ] || echo "WAIT $1 $c $(cat /tmp/w)"; }
report() {
  paddock-host kv list --scope pasture 2>&1 | sed 's/^/LIST /'
  sed 's/^/HELPER /' "${TMPDIR:-/tmp}/paddock-pasture-serve.log" 2>/dev/null
}
paddock-host kv list --scope pasture >/dev/null 2>/tmp/k; echo "KV_EXIT $? $(cat /tmp/k)"
"#;

/// Member A serves on 3000 and shares it. It writes ready:a, then waits
/// for go:unshare and unshares.
const OWNER: &str = r#"
python3 - <<'PY' &
import socket, threading
s = socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(("127.0.0.1", 3000)); s.listen(8)
def h(c):
    d = c.recv(1024); c.sendall(b"from-a:" + d); c.close()
while True:
    c, _ = s.accept(); threading.Thread(target=h, args=(c,)).start()
PY
sleep 1
paddock-host pasture share 3000; echo "SHARE_EXIT $?"
paddock-host kv put ready:a yes --scope pasture >/dev/null
# B asks for up to 15 seconds before it writes go:unshare.
wait_for go:unshare 25
paddock-host pasture unshare 3000; echo "UNSHARE_EXIT $?"
paddock-host kv put done:a yes --scope pasture >/dev/null
wait_for go:end
report
"#;

/// Member B waits for ready:a, reaches 127.0.0.1:3000, tries to share it,
/// lets A unshare, and checks that the port is gone.
const OTHER: &str = r#"
ask() { python3 -c '
import socket, sys
try:
    c = socket.create_connection(("127.0.0.1", 3000), timeout=1); c.sendall(b"ping")
    print(c.recv(1024).decode() or "EMPTY")
except Exception as e:
    print("ERR", type(e).__name__)
'; }
wait_for ready:a
for i in $(seq 1 40); do r=$(ask); case "$r" in from-a:*) break;; esac; sleep 0.2; done
echo "REACH $r"
paddock-host pasture share 3000 2>/tmp/err; echo "SHARE_EXIT $?"; echo "SHARE_ERR $(cat /tmp/err)"
paddock-host pasture members > /tmp/m; echo "MEMBERS $(tr '\n' ' ' </tmp/m)"
paddock-host kv put go:unshare yes --scope pasture >/dev/null
wait_for done:a
for i in $(seq 1 40); do r=$(ask); case "$r" in from-a:*) ;; *) break;; esac; sleep 0.2; done
echo "AFTER $r"
report
paddock-host kv put go:end yes --scope pasture >/dev/null
"#;

#[test]
fn two_members_reach_a_shared_port_in_bubblewrap() {
    if !common::bwrap_works() {
        eprintln!("skipped: bwrap cannot create a user namespace here");
        return;
    }
    if Command::new("python3").arg("-V").output().is_err() {
        eprintln!("skipped: no python3");
        return;
    }
    let tmp = common::tempdir();
    let root = tmp.path();
    let cfg = root.join("cfg/paddock");
    std::fs::create_dir_all(cfg.join("harness.d")).unwrap();
    std::fs::create_dir_all(root.join("run")).unwrap();
    std::fs::create_dir_all(root.join("project")).unwrap();
    std::fs::create_dir_all(root.join("home")).unwrap();
    std::fs::write(
        cfg.join("config.toml"),
        "[pastures.team]\nmembers = [\"owner\", \"other\"]\n",
    )
    .unwrap();
    for (name, script) in [("owner", OWNER), ("other", OTHER)] {
        std::fs::write(
            cfg.join(format!("harness.d/{name}.toml")),
            format!(
                "name = \"{name}\"\n[locate]\ncommands = [\"sh\"]\n[launch]\nargs = [\"-c\", {}]\n",
                serde_json::to_string(&format!("{LOG}{script}")).unwrap()
            ),
        )
        .unwrap();
    }
    let host = |args: &[&str]| {
        let mut c = Command::new(env!("CARGO_BIN_EXE_paddock"));
        c.env("HOME", root.join("home"))
            .env("XDG_CONFIG_HOME", root.join("cfg"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("XDG_RUNTIME_DIR", root.join("run"))
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .env("LANG", "C")
            .env("LC_ALL", "C")
            .args(args)
            .current_dir(root.join("project"))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        c
    };
    let start = |name: &str| {
        let project = root.join("project");
        host(&[
            "run",
            name,
            "--backend",
            "bwrap",
            "--network",
            "none",
            "--env",
            "clean",
            "--pasture",
            "team",
            "-y",
            project.to_str().unwrap(),
        ])
        .spawn()
        .unwrap()
    };
    let mut a = start("owner");
    let mut b = start("other");
    // The scripts end within about 50 seconds; a hang ends here.
    let deadline = Instant::now() + Duration::from_secs(90);
    let mut killed = false;
    while a.try_wait().unwrap().is_none() || b.try_wait().unwrap().is_none() {
        if Instant::now() > deadline {
            let _ = a.kill();
            let _ = b.kill();
            killed = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let b = b.wait_with_output().unwrap();
    let a = a.wait_with_output().unwrap();
    // The host view of the pasture: which member wrote which key.
    let list = host(&["kv", "list", "--pasture", "team"]).output().unwrap();
    let out_a = String::from_utf8_lossy(&a.stdout).into_owned();
    let out_b = String::from_utf8_lossy(&b.stdout).into_owned();
    let all = format!(
        "A ({}):\n{out_a}\n{}\nB ({}):\n{out_b}\n{}\nhost kv list --pasture team:\n{}",
        a.status,
        String::from_utf8_lossy(&a.stderr),
        b.status,
        String::from_utf8_lossy(&b.stderr),
        text(&list),
    );
    assert!(!killed, "the members did not end in 90 seconds:\n{all}");
    let line = |text: &str, k: &str| -> String {
        text.lines()
            .find_map(|l| l.strip_prefix(&format!("{k} ")))
            .unwrap_or_else(|| panic!("no {k} in:\n{all}"))
            .to_string()
    };
    for out in [&out_a, &out_b] {
        assert_eq!(line(out, "KV_EXIT").trim(), "0", "{all}");
        assert!(!out.contains("WAIT "), "{all}");
    }
    assert_eq!(line(&out_a, "SHARE_EXIT"), "0", "{all}");
    assert_eq!(line(&out_b, "REACH"), "from-a:ping", "{all}");
    assert_eq!(line(&out_b, "SHARE_EXIT"), "9", "{all}");
    assert!(line(&out_b, "SHARE_ERR").contains("owner"), "{all}");
    assert!(line(&out_b, "MEMBERS").contains("3000"), "{all}");
    assert_eq!(line(&out_a, "UNSHARE_EXIT"), "0", "{all}");
    assert_ne!(line(&out_b, "AFTER"), "from-a:ping", "{all}");
}

// ---- the bridge: one member asks another -----------------------------------

const TOKEN: &str = "fake-Tok3n-Ab9cD8eF7gH6iJ5kL4mN";

/// The one-shot command of member B: it reads the question on stdin. A
/// question about the login prints its login file.
const ANSWER: &str = r#"q=$(cat); case "$q" in *login*) cat "$0";; *) echo "B answers: $q";; esac"#;

#[test]
fn a_member_asks_another_and_the_guard_withholds_a_login() {
    use paddock::pasture::bridge;
    let t = tempfile::tempdir().unwrap();
    let home = t.path().join("home");
    let ground = t.path().join("ground");
    let b_home = t.path().join("b-home");
    std::fs::create_dir_all(home.join(".config/paddock")).unwrap();
    std::fs::create_dir_all(&ground).unwrap();
    std::fs::create_dir_all(b_home.join(".fake")).unwrap();
    let login = b_home.join(".fake/auth.json");
    std::fs::write(
        &login,
        format!(r#"{{"tokens":{{"access_token":"{TOKEN}"}}}}"#),
    )
    .unwrap();
    let env = HostEnv::from_parts(home.clone(), "demo".into(), 1000, 1000, Default::default());
    let config = env.paddock_config().join("config.toml");
    let set_bridge = |on: bool| {
        std::fs::write(
            &config,
            format!("[pastures.team]\nmembers = [\"sh\", \"fake\"]\nbridge = {on}\n"),
        )
        .unwrap();
    };
    set_bridge(true);
    let project = Path::new("/src/app");
    let base = bridge::dir(&env, project, "team");
    let member = |id: &str, harness: &str, cmd: Option<bridge::Command>| {
        // The run record and the bridge record that paddock writes at start.
        let info = paddock::state::Running::file(&env, id);
        std::fs::create_dir_all(info.parent().unwrap()).unwrap();
        std::fs::write(
            &info,
            serde_json::json!({
                "id": id, "harness": harness, "project": project, "backend": "bwrap",
                "network": "none", "gui": "off", "pid": std::process::id(),
                "started": 1, "kv_project": project, "pasture": "team",
            })
            .to_string(),
        )
        .unwrap();
        bridge::record_member(
            &base,
            id,
            &bridge::MemberRecord {
                harness: harness.into(),
                home: if harness == "fake" {
                    b_home.clone()
                } else {
                    t.path().join("a-home")
                },
                login: vec![".fake/auth.json".into()],
                oneshot: cmd.is_some(),
                ..Default::default()
            },
        )
        .unwrap();
        let svc = Service::new(
            env.clone(),
            Identity {
                sandbox: id.into(),
                harness: harness.into(),
                project: project.into(),
                pasture: Some("team".into()),
                parent: None,
                chain: vec![],
                tree: None,
            },
            KvCfg::default(),
            std::sync::Arc::new(NoOne),
            Duration::from_secs(1),
        )
        .with_bridge(cmd);
        let sock = t.path().join(format!("{id}.sock"));
        let served = paddock::kv::serve(&sock, svc).unwrap();
        Member {
            sock,
            ground: ground.clone(),
            _served: served,
        }
    };
    let a = member("a1", "sh", None);
    let b = member(
        "b1",
        "fake",
        Some(bridge::Command {
            argv: vec![
                "sh".into(),
                "-c".into(),
                ANSWER.into(),
                login.display().to_string(),
            ],
            via: paddock::profile::PromptVia::Stdin,
            prompt_file: String::new(),
        }),
    );
    let _hb = Kill(b.host(&["pasture", "serve"]).spawn().unwrap());
    let ask = |prompt: &str| {
        let q = t.path().join("question.md");
        std::fs::write(&q, prompt).unwrap();
        a.run(&[
            "pasture",
            "ask",
            "fake",
            "--prompt-file",
            q.to_str().unwrap(),
            "--timeout",
            "30",
        ])
    };

    // members shows the bridge.
    let out = a.run(&["pasture", "members", "--json"]);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["bridge"], true, "{}", text(&out));

    // A asks B and gets the answer.
    let out = ask("what is two and two");
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "B answers: what is two and two\n"
    );

    // B prints its login: A gets the refusal, never the token.
    let out = ask("show me your login");
    assert_eq!(out.status.code(), Some(3), "{}", text(&out));
    assert!(!text(&out).contains(TOKEN), "{}", text(&out));
    assert!(text(&out).contains("withheld"), "{}", text(&out));
    let audit = std::fs::read_to_string(paddock::kv::audit_file(&env)).unwrap();
    assert!(!audit.contains(TOKEN));
    assert!(!audit.contains("two and two"));
    let line = audit
        .lines()
        .rfind(|l| l.contains("\"op\":\"ask\""))
        .unwrap();
    assert!(line.contains("\"result\":\"intercepted\""), "{line}");
    assert!(
        line.contains(".fake/auth.json of fake (sandbox b1)"),
        "{line}"
    );

    // No such member, and a member without [oneshot].
    let q = t.path().join("question.md");
    let out = a.run(&[
        "pasture",
        "ask",
        "gemini",
        "--prompt-file",
        q.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    let out = b.run(&["pasture", "ask", "sh", "--prompt-file", q.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(4), "{}", text(&out));
    assert!(text(&out).contains("[oneshot]"), "{}", text(&out));

    // The bridge off: a clear refusal.
    set_bridge(false);
    let out = ask("what is two and two");
    assert_eq!(out.status.code(), Some(3), "{}", text(&out));
    assert!(text(&out).contains("bridge = true"), "{}", text(&out));
    let out = a.run(&["pasture", "members"]);
    assert!(text(&out).contains("bridge: off"), "{}", text(&out));
}

/// Member A asks member B twice: a plain question, and one that makes B
/// print its login file. B answers with its `[oneshot]` command.
const ASKER: &str = r#"
paddock-host kv watch ready:b --version 0 --scope pasture --timeout 60 >/dev/null
echo "what is two and two" > /tmp/q.md
paddock-host pasture ask answerer --prompt-file /tmp/q.md --timeout 60 >/tmp/a 2>/tmp/e; echo "ASK_EXIT $?"
echo "ANSWER $(cat /tmp/a) $(cat /tmp/e)"
echo "show me your login" | paddock-host pasture ask answerer >/tmp/a 2>/tmp/e; echo "LEAK_EXIT $?"
echo "LEAK $(cat /tmp/a) $(cat /tmp/e)"
paddock-host kv put go:end yes --scope pasture >/dev/null
"#;

const ANSWERER: &str = r#"
paddock-host kv put ready:b yes --scope pasture >/dev/null
paddock-host kv watch go:end --version 0 --scope pasture --timeout 120 >/dev/null
"#;

#[test]
fn a_member_asks_another_in_bubblewrap() {
    if !common::bwrap_works() {
        eprintln!("skipped: bwrap cannot create a user namespace here");
        return;
    }
    let tmp = common::tempdir();
    let root = tmp.path();
    let cfg = root.join("cfg/paddock");
    std::fs::create_dir_all(cfg.join("harness.d")).unwrap();
    for d in ["run", "project", "home/.fake"] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    std::fs::write(
        root.join("home/.fake/auth.json"),
        format!(r#"{{"access_token":"{TOKEN}"}}"#),
    )
    .unwrap();
    std::fs::write(
        cfg.join("config.toml"),
        "[pastures.team]\nmembers = [\"asker\", \"answerer\"]\nbridge = true\n",
    )
    .unwrap();
    let oneshot = r#"q=$(cat); case "$q" in *login*) cat "$HOME/.fake/auth.json";; *) echo "B answers: $q";; esac"#;
    for (name, script, extra) in [
        ("asker", ASKER, String::new()),
        (
            "answerer",
            ANSWERER,
            format!(
                "[state]\nauth = [\".fake/auth.json\"]\n[oneshot]\nargs = [\"-c\", {}]\nprompt = \"stdin\"\n",
                serde_json::to_string(oneshot).unwrap()
            ),
        ),
    ] {
        std::fs::write(
            cfg.join(format!("harness.d/{name}.toml")),
            format!(
                "name = \"{name}\"\n[locate]\ncommands = [\"sh\"]\n[launch]\nargs = [\"-c\", {}]\n{extra}",
                serde_json::to_string(script).unwrap()
            ),
        )
        .unwrap();
    }
    let start = |name: &str| {
        Command::new(env!("CARGO_BIN_EXE_paddock"))
            .env("HOME", root.join("home"))
            .env("XDG_CONFIG_HOME", root.join("cfg"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("XDG_RUNTIME_DIR", root.join("run"))
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .env("LANG", "C")
            .env("LC_ALL", "C")
            .args([
                "run",
                name,
                "--backend",
                "bwrap",
                "--network",
                "none",
                "--env",
                "clean",
                "--pasture",
                "team",
                "-y",
            ])
            .arg(root.join("project"))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap()
    };
    let b = start("answerer");
    let a = start("asker");
    let a = a.wait_with_output().unwrap();
    let b = b.wait_with_output().unwrap();
    let out = String::from_utf8_lossy(&a.stdout).into_owned();
    let all = format!(
        "A:\n{out}\n{}\nB:\n{}\n{}",
        String::from_utf8_lossy(&a.stderr),
        String::from_utf8_lossy(&b.stdout),
        String::from_utf8_lossy(&b.stderr)
    );
    let line = |k: &str| -> String {
        out.lines()
            .find_map(|l| l.strip_prefix(&format!("{k} ")))
            .unwrap_or_else(|| panic!("no {k} in:\n{all}"))
            .to_string()
    };
    assert_eq!(line("ASK_EXIT"), "0", "{all}");
    assert!(
        line("ANSWER").contains("B answers: what is two and two"),
        "{all}"
    );
    assert_eq!(line("LEAK_EXIT"), "3", "{all}");
    assert!(line("LEAK").contains("withheld"), "{all}");
    assert!(!all.contains(TOKEN), "{all}");
    let audit = std::fs::read_to_string(root.join("state/paddock/kv-audit.jsonl")).unwrap();
    assert!(audit.contains("\"result\":\"intercepted\""), "{audit}");
    assert!(!audit.contains(TOKEN));
}

// ---- the workbench ---------------------------------------------------------

#[test]
fn the_workbench_needs_the_configuration() {
    let t = tempfile::tempdir().unwrap();
    for d in ["home/.config/paddock", "project", "run"] {
        std::fs::create_dir_all(t.path().join(d)).unwrap();
    }
    let out = paddock(t.path(), &["pasture", "workbench", "team"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("workbench = true"), "{}", text(&out));
    let out = paddock(t.path(), &["pasture", "workbench", "team", "--start"]);
    assert!(!out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("workbench = true"), "{}", text(&out));
    std::fs::write(
        t.path().join("home/.config/paddock/config.toml"),
        "[pastures.team]\nmembers = [\"sh\"]\nworkbench = true\n",
    )
    .unwrap();
    let out = paddock(t.path(), &["pasture", "workbench", "team", "--json"]);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["allowed"], true);
    assert_eq!(v["running"], false);
    let out = paddock(t.path(), &["pasture", "workbench", "team", "--attach"]);
    assert!(text(&out).contains("--start"), "{}", text(&out));
    let out = paddock(t.path(), &["pasture", "workbench", "team", "--stop"]);
    assert!(text(&out).contains("does not run"), "{}", text(&out));
    // workbench is no member name.
    std::fs::write(
        t.path().join("home/.config/paddock/config.toml"),
        "[pastures.team]\nmembers = [\"workbench\"]\n",
    )
    .unwrap();
    let out = paddock(t.path(), &["pasture", "workbench", "team"]);
    assert!(!out.status.success(), "{}", text(&out));
}

/// A member types into the workbench through its read-only socket folder.
const POKER: &str = r#"
S=/run/paddock/workbench/tmux
for i in $(seq 1 100); do [ -S "$S" ] && break; sleep 0.1; done
tmux -S "$S" send-keys -t workbench 'echo hi > /pasture/hi' Enter; echo "SEND_EXIT $?"
for i in $(seq 1 100); do [ -s /pasture/hi ] && break; sleep 0.1; done
echo "SEEN $(cat /pasture/hi 2>/dev/null)"
echo "HOME_FILES $(find "$HOME" -name 'tmux*' -o -name 'workbench*' | tr '\n' ' ')"
touch /run/paddock/workbench/x 2>/dev/null; echo "RO_EXIT $?"
paddock-host pasture workbench --print >/tmp/o 2>&1; echo "JOIN_EXIT $?"
echo "JOIN $(head -n 1 /tmp/o)"
paddock-host pasture members > /tmp/m; echo "MEMBERS $(tr '\n' ' ' </tmp/m)"
"#;

#[test]
fn a_member_types_into_the_workbench_in_bubblewrap() {
    if !common::bwrap_works() {
        eprintln!("skipped: bwrap cannot create a user namespace here");
        return;
    }
    if Command::new("tmux").arg("-V").output().is_err() {
        eprintln!("skipped: no tmux");
        return;
    }
    let tmp = common::tempdir();
    let root = tmp.path();
    let cfg = root.join("cfg/paddock");
    std::fs::create_dir_all(cfg.join("harness.d")).unwrap();
    for d in ["run", "project", "home"] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    std::fs::write(
        cfg.join("config.toml"),
        "backend = \"bwrap\"\n[pastures.team]\nmembers = [\"poker\"]\nworkbench = true\n",
    )
    .unwrap();
    std::fs::write(
        cfg.join("harness.d/poker.toml"),
        format!(
            "name = \"poker\"\n[locate]\ncommands = [\"sh\"]\n[launch]\nargs = [\"-c\", {}]\n",
            serde_json::to_string(POKER).unwrap()
        ),
    )
    .unwrap();
    let project = root.join("project").canonicalize().unwrap();
    let cmd = |args: &[&str]| {
        let mut c = Command::new(env!("CARGO_BIN_EXE_paddock"));
        c.env("HOME", root.join("home"))
            .env("XDG_CONFIG_HOME", root.join("cfg"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("XDG_RUNTIME_DIR", root.join("run"))
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .env("LANG", "C")
            .env("LC_ALL", "C")
            .current_dir(&project)
            .args(args)
            .stdin(std::process::Stdio::null());
        c
    };
    let env = HostEnv::from_parts(
        root.join("home"),
        "demo".into(),
        1000,
        1000,
        [
            ("XDG_DATA_HOME", root.join("data")),
            ("XDG_STATE_HOME", root.join("state")),
            ("XDG_CONFIG_HOME", root.join("cfg")),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.display().to_string()))
        .collect(),
    );
    let sock = paddock::pasture::workbench::socket(&env, &project, "team");
    let ground = paddock::pasture::ground(&env, &project, "team");

    let out = cmd(&["pasture", "workbench", "team", "--start"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out));
    assert!(sock.exists(), "{}", sock.display());
    let out = cmd(&["pasture", "list", "--json"]).output().unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["pastures"][0]["workbench"]["running"], true, "{v}");

    let member = cmd(&[
        "run",
        "poker",
        "--backend",
        "bwrap",
        "--network",
        "none",
        "--env",
        "clean",
        "--pasture",
        "team",
        "-y",
    ])
    .output()
    .unwrap();
    let out = String::from_utf8_lossy(&member.stdout).into_owned();
    let all = format!("{out}\n{}", String::from_utf8_lossy(&member.stderr));
    let line = |k: &str| -> String {
        out.lines()
            .find_map(|l| l.strip_prefix(&format!("{k} ")))
            .unwrap_or_else(|| panic!("no {k} in:\n{all}"))
            .trim()
            .to_string()
    };
    assert_eq!(line("SEND_EXIT"), "0", "{all}");
    assert_eq!(line("SEEN"), "hi", "{all}");
    assert_eq!(std::fs::read_to_string(ground.join("hi")).unwrap(), "hi\n");
    assert!(
        out.lines().any(|l| l.trim() == "HOME_FILES"),
        "the member home has workbench files:\n{all}"
    );
    assert_ne!(line("RO_EXIT"), "0", "{all}");
    assert_eq!(line("JOIN_EXIT"), "0", "{all}");
    assert!(
        line("JOIN").contains("tmux -S /run/paddock/workbench/tmux attach"),
        "{all}"
    );
    assert!(line("MEMBERS").contains("workbench: runs"), "{all}");

    // The last member ended: the workbench stops, and its socket goes.
    until("the workbench stops after the last member", || {
        !sock.exists()
    });

    // Started again, then stopped by the user.
    let out = cmd(&["pasture", "workbench", "team", "--start"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out));
    assert!(sock.exists());
    let out = cmd(&["pasture", "workbench", "team", "--stop"])
        .output()
        .unwrap();
    assert!(text(&out).contains("Stopped"), "{}", text(&out));
    until("the socket goes", || !sock.exists());

    let audit = std::fs::read_to_string(root.join("state/paddock/kv-audit.jsonl")).unwrap();
    let events: Vec<&str> = audit
        .lines()
        .filter(|l| l.contains("\"op\":\"workbench\""))
        .collect();
    assert!(
        events
            .iter()
            .any(|l| l.contains("\"event\":\"join\"") && l.contains("poker (sandbox")),
        "{audit}"
    );
    assert!(events.iter().any(|l| l.contains("\"event\":\"start\"")));
    assert!(events.iter().any(|l| l.contains("\"event\":\"stop\"")));
    assert!(!audit.contains("echo hi"));
}
