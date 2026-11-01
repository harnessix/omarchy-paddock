//! The credential gateway end to end, with bwrap and network mode none: the
//! harness calls a local fake HTTPS upstream through the base URL that
//! paddock sets. The upstream gets the real value; the sandbox holds only a
//! stand-in; a path that is not bound gets 403 and never leaves the host.
//! The upstream's certificate is trusted only by the paddock process of the
//! test (SSL_CERT_FILE). Temporary folders only; no outside network.

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const SECRET: &str = "ghp_REAL0123456789abcdefghijklmnopqrstu";

fn can_run() -> bool {
    let bwrap = common::bwrap_works();
    let curl = Command::new("curl").arg("--version").output().is_ok();
    bwrap && curl
}

/// A fake upstream for `localhost`: it records each request head and
/// answers "ok". Returns its port, the heads and the PEM of its certificate.
fn upstream() -> (u16, Arc<Mutex<Vec<String>>>, String) {
    use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
    let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(ck.signing_key.serialize_der()));
    let scfg = Arc::new(
        rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![ck.cert.der().clone()], key)
            .unwrap(),
    );
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let heads = Arc::new(Mutex::new(Vec::new()));
    let h2 = heads.clone();
    std::thread::spawn(move || {
        for c in l.incoming().flatten() {
            let (scfg, heads) = (scfg.clone(), h2.clone());
            std::thread::spawn(move || {
                let conn = rustls::ServerConnection::new(scfg).unwrap();
                let mut r = BufReader::new(rustls::StreamOwned::new(conn, c));
                let mut head = String::new();
                loop {
                    let mut line = String::new();
                    if r.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    head.push_str(&line);
                    if line == "\r\n" {
                        break;
                    }
                }
                heads.lock().unwrap().push(head);
                let s = r.get_mut();
                let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
                s.conn.send_close_notify();
                let _ = s.flush();
                let mut rest = Vec::new();
                let _ = s.read_to_end(&mut rest);
            });
        }
    });
    (port, heads, ck.cert.pem())
}

#[test]
fn the_sandbox_uses_a_stand_in_and_the_bound_host_gets_the_value() {
    if !can_run() {
        eprintln!("skipped: needs bwrap and curl");
        return;
    }
    let (port, heads, pem) = upstream();
    let tmp = common::tempdir();
    let root: PathBuf = tmp.path().to_path_buf();
    let cfg = root.join("cfg/paddock");
    std::fs::create_dir_all(cfg.join("harness.d")).unwrap();
    std::fs::create_dir_all(root.join("project")).unwrap();
    std::fs::create_dir_all(root.join("home")).unwrap();
    let run = root.join("run");
    std::fs::create_dir_all(&run).unwrap();
    std::fs::set_permissions(&run, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    std::fs::write(root.join("ca.pem"), pem).unwrap();
    let script = r#"curl -s -H "Authorization: Bearer $GH_TOKEN" "$GH_BASE/repos/x" > ok.txt; curl -s -o /dev/null -w '%{http_code}' "$GH_BASE/user" > denied.txt; env > env.txt"#;
    std::fs::write(
        cfg.join("harness.d/gwcheck.toml"),
        format!(
            "name = \"gwcheck\"\n[locate]\ncommands = [\"sh\"]\n[launch]\nargs = [\"-c\", {}]\n",
            serde_json::to_string(script).unwrap()
        ),
    )
    .unwrap();
    std::fs::write(
        cfg.join("config.toml"),
        format!(
            "[credentials.GH]\nvia = \"gateway\"\nvar = \"HOST_GH\"\nenv = \"GH_TOKEN\"\nhosts = [\"localhost:{port}\"]\npaths = [\"/repos/\"]\nbase_url_env = \"GH_BASE\"\n\n[host]\nwait = 30\n"
        ),
    )
    .unwrap();
    let cmd = || {
        let mut c = Command::new(env!("CARGO_BIN_EXE_paddock"));
        c.env("HOME", root.join("home"))
            .env("XDG_CONFIG_HOME", root.join("cfg"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("XDG_RUNTIME_DIR", root.join("run"))
            .env("HOST_GH", SECRET)
            .env("SSL_CERT_FILE", root.join("ca.pem"))
            .env_remove("SSL_CERT_DIR")
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .current_dir(root.join("project"));
        for v in [
            "HTTPS_PROXY",
            "https_proxy",
            "HTTP_PROXY",
            "http_proxy",
            "ALL_PROXY",
            "all_proxy",
        ] {
            c.env_remove(v);
        }
        c
    };
    let mut child = cmd()
        .args([
            "run",
            "gwcheck",
            "--backend",
            "bwrap",
            "--network",
            "none",
            "-y",
        ])
        .arg(root.join("project"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Two answers, as from the panel.
    for what in ["give GH", "confirm"] {
        let start = Instant::now();
        let id = loop {
            let out = cmd().args(["approvals", "--json"]).output().unwrap();
            let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
            let found = v["pending"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|p| p["summary"].as_str().is_some_and(|s| s.contains(what)));
            if let Some(p) = found {
                assert!(!p.to_string().contains(SECRET));
                break p["id"].as_str().unwrap().to_string();
            }
            assert!(start.elapsed() < Duration::from_secs(30), "no card: {what}");
            std::thread::sleep(Duration::from_millis(100));
        };
        assert!(cmd()
            .args(["approvals", "answer", &id, "once"])
            .status()
            .unwrap()
            .success());
    }
    let start = Instant::now();
    while child.try_wait().unwrap().is_none() {
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "the sandbox did not end"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let mut err = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut err)
        .unwrap();
    let read = |f: &str| std::fs::read_to_string(root.join("project").join(f)).unwrap_or_default();
    let log = cmd().args(["log", "--json"]).output().unwrap();
    let log = String::from_utf8_lossy(&log.stdout).into_owned();
    assert_eq!(read("ok.txt"), "ok", "{err}\n{log}");
    assert_eq!(read("denied.txt"), "403");
    let env = read("env.txt");
    assert!(!env.contains(SECRET), "the real value is in the sandbox");
    assert!(!env.contains("HOST_GH="));
    assert!(env.contains("GH_TOKEN=ghp_PADDOCK"), "{env}");
    assert!(env.contains("GH_BASE=http://127.0.0.1:3130/s/GH"), "{env}");
    let heads = heads.lock().unwrap();
    assert_eq!(
        heads.len(),
        1,
        "only the bound path left the host: {heads:?}"
    );
    assert!(
        heads[0].starts_with("GET /repos/x HTTP/1.1"),
        "{}",
        heads[0]
    );
    assert!(
        heads[0].contains(&format!("Authorization: Bearer {SECRET}")),
        "{}",
        heads[0]
    );
    assert!(!heads[0].contains("PADDOCK"));
    // The audit log names each use, never the value.
    assert!(log.contains("gateway GH GET"), "{log}");
    assert!(log.contains("refused: path not bound"), "{log}");
    assert!(!log.contains(SECRET));
}
