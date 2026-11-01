//! git over HTTPS through the credential gateway (`preset = "github-git"`).
//! A fake "github" on 127.0.0.1 checks the Basic header and serves a
//! temporary bare repository with `git http-backend`. git reaches it only
//! through the URL rewrite in its environment, the relay and the gateway.
//! Temporary folders only; no outside network.

use base64::Engine as _;
use paddock::credentials::gateway::{self, Connector, Gateway, Route, Stream};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

const SECRET: &str = "ghp_GITREAL0123456789abcdefghijklmnopq";

fn git(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("LC_ALL", "C")
        .output()
        .unwrap()
}

/// The fake host: TLS for `localhost`, then one CGI call per request.
/// Returns its port, the client config that trusts it, and the requests it
/// saw (method, path, whether the Authorization header was right).
fn fake_github(root: &Path) -> (u16, Arc<rustls::ClientConfig>, Arc<Mutex<Vec<String>>>) {
    use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
    let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(ck.signing_key.serialize_der()));
    let scfg = Arc::new(
        rustls::ServerConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![ck.cert.der().clone()], key)
            .unwrap(),
    );
    let mut roots = rustls::RootCertStore::empty();
    roots.add(ck.cert.der().clone()).unwrap();
    let client = Arc::new(
        rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    );
    let want = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{SECRET}"))
    );
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (seen2, root) = (seen.clone(), root.to_path_buf());
    std::thread::spawn(move || {
        for c in l.incoming().flatten() {
            let (scfg, seen, want, root) =
                (scfg.clone(), seen2.clone(), want.clone(), root.clone());
            std::thread::spawn(move || {
                let conn = rustls::ServerConnection::new(scfg).unwrap();
                let mut r = BufReader::new(rustls::StreamOwned::new(conn, c));
                let mut lines = Vec::new();
                loop {
                    let mut line = String::new();
                    if r.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    if line == "\r\n" {
                        break;
                    }
                    lines.push(line.trim_end().to_string());
                }
                let mut first = lines[0].split(' ');
                let (method, target) = (first.next().unwrap(), first.next().unwrap());
                let (path, query) = target.split_once('?').unwrap_or((target, ""));
                let header = |n: &str| {
                    lines[1..].iter().find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.eq_ignore_ascii_case(n).then(|| v.trim().to_string())
                    })
                };
                let auth_ok = header("authorization").as_deref() == Some(want.as_str());
                seen.lock()
                    .unwrap()
                    .push(format!("{method} {path} auth_ok={auth_ok}"));
                let s = r.get_mut();
                if !auth_ok {
                    let _ = s.write_all(
                        b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                    s.conn.send_close_notify();
                    let _ = s.flush();
                    return;
                }
                let len: u64 = header("content-length")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                let mut cgi = Command::new("git")
                    .arg("http-backend")
                    .env_clear()
                    .env("PATH", std::env::var("PATH").unwrap_or_default())
                    .env("GIT_PROJECT_ROOT", &root)
                    .env("GIT_HTTP_EXPORT_ALL", "1")
                    .env("GIT_CONFIG_NOSYSTEM", "1")
                    .env("GIT_CONFIG_GLOBAL", "/dev/null")
                    .env("REQUEST_METHOD", method)
                    .env("PATH_INFO", path)
                    .env("QUERY_STRING", query)
                    .env("CONTENT_TYPE", header("content-type").unwrap_or_default())
                    .env("CONTENT_LENGTH", len.to_string())
                    .env(
                        "HTTP_CONTENT_ENCODING",
                        header("content-encoding").unwrap_or_default(),
                    )
                    .env("GIT_PROTOCOL", header("git-protocol").unwrap_or_default())
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .spawn()
                    .unwrap();
                let mut body = Vec::new();
                (&mut r).take(len).read_to_end(&mut body).unwrap();
                cgi.stdin.take().unwrap().write_all(&body).unwrap();
                let mut out = Vec::new();
                cgi.stdout.take().unwrap().read_to_end(&mut out).unwrap();
                let _ = cgi.wait();
                let split = out
                    .windows(4)
                    .position(|w| w == b"\r\n\r\n")
                    .map(|i| (i, 4));
                let split =
                    split.or_else(|| out.windows(2).position(|w| w == b"\n\n").map(|i| (i, 2)));
                let (i, n) = split.unwrap();
                let head = String::from_utf8_lossy(&out[..i]).into_owned();
                let mut status = "200 OK".to_string();
                let mut resp = String::new();
                for l in head.lines() {
                    match l.strip_prefix("Status:") {
                        Some(st) => status = st.trim().to_string(),
                        None => resp.push_str(&format!("{l}\r\n")),
                    }
                }
                let s = r.get_mut();
                let _ = s.write_all(
                    format!("HTTP/1.1 {status}\r\n{resp}Connection: close\r\n\r\n").as_bytes(),
                );
                let _ = s.write_all(&out[i + n..]);
                s.conn.send_close_notify();
                let _ = s.flush();
            });
        }
    });
    (port, client, seen)
}

/// What `paddock relay` does in the sandbox: loopback TCP to the socket.
fn relay(sock: &Path) -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let sock = sock.to_path_buf();
    std::thread::spawn(move || {
        for c in l.incoming().flatten() {
            let up = UnixStream::connect(&sock).unwrap();
            let pipe = |mut a: TcpStream, mut b: UnixStream, down: bool| {
                std::thread::spawn(move || {
                    if down {
                        let _ = std::io::copy(&mut b, &mut a);
                        let _ = a.shutdown(std::net::Shutdown::Write);
                    } else {
                        let _ = std::io::copy(&mut a, &mut b);
                        let _ = b.shutdown(std::net::Shutdown::Write);
                    }
                });
            };
            pipe(c.try_clone().unwrap(), up.try_clone().unwrap(), false);
            pipe(c, up, true);
        }
    });
    port
}

#[test]
fn git_clone_goes_through_the_gateway_and_the_token_stays_on_the_host() {
    let backend = Command::new("git").args(["http-backend"]).output();
    if !backend.is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("Status:")) {
        eprintln!("skipped: needs git http-backend");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    // The served repository: owner/repo.git with one commit.
    let work = root.join("work");
    std::fs::create_dir_all(&work).unwrap();
    assert!(git(&work, &["init", "-q", "-b", "main"]).status.success());
    std::fs::write(work.join("hello.txt"), "hello\n").unwrap();
    git(&work, &["add", "."]);
    let c = git(
        &work,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.invalid",
            "commit",
            "-qm",
            "one",
        ],
    );
    assert!(c.status.success(), "{c:?}");
    let served = root.join("served");
    std::fs::create_dir_all(served.join("owner")).unwrap();
    let b = git(
        root,
        &[
            "clone",
            "-q",
            "--bare",
            work.to_str().unwrap(),
            served.join("owner/repo.git").to_str().unwrap(),
        ],
    );
    assert!(b.status.success(), "{b:?}");

    let (up_port, client, seen) = fake_github(&served);
    let host = format!("localhost:{up_port}");
    // The test-only host override: `hosts` wins over the preset's github.com.
    let mut creds = paddock::credentials::parse(&format!(
        "[credentials.github_git]\nvia = \"gateway\"\npreset = \"github-git\"\nhosts = [\"{host}\"]\n"
    ))
    .unwrap();
    let c = creds.remove("github_git").unwrap();
    assert_eq!(c.basic_user.as_deref(), Some("x-access-token"));
    let connect: Connector = Arc::new(move |bound: &str| -> anyhow::Result<Box<dyn Stream>> {
        let (h, p) = gateway::split_host(bound)?;
        let tcp = TcpStream::connect(("127.0.0.1", p))?;
        let name = rustls::pki_types::ServerName::try_from(h.to_string())?;
        let conn = rustls::ClientConnection::new(client.clone(), name)?;
        Ok(Box::new(rustls::StreamOwned::new(conn, tcp)))
    });
    let mut routes = BTreeMap::new();
    routes.insert("github_git".to_string(), Route::new(&c, SECRET.to_string()));
    let revoked = root.join("revoked");
    std::fs::write(&revoked, "").unwrap();
    let log = Arc::new(Mutex::new(Vec::<String>::new()));
    let log2 = log.clone();
    let g = Gateway::new(
        routes,
        connect,
        revoked.clone(),
        Arc::new(move |n: &str, m: &str, h: &str, p: &str, r: &str| {
            log2.lock().unwrap().push(format!("{n} {m} {h}{p} {r}"));
        }),
        Arc::new(|| {}),
    );
    let sock = root.join("credentials.sock");
    g.serve(&sock).unwrap();
    let port = relay(&sock);

    // The sandbox side: git with only the rewrite in its environment.
    let rewrite = paddock::credentials::git_env(port, &[("github_git".into(), host.clone())]);
    let sandbox = root.join("sandbox");
    std::fs::create_dir_all(&sandbox).unwrap();
    let clone = |to: &str| {
        let mut cmd = Command::new("git");
        cmd.args(["clone", "-q", &format!("https://{host}/owner/repo.git"), to])
            .current_dir(&sandbox)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", &sandbox)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C");
        for (k, v) in &rewrite {
            cmd.env(k, v);
        }
        cmd.output().unwrap()
    };
    let ok = clone("one");
    let audit = log.lock().unwrap().join("\n");
    assert!(
        ok.status.success(),
        "{ok:?}\n{audit}\n{:?}",
        seen.lock().unwrap()
    );
    assert_eq!(
        std::fs::read_to_string(sandbox.join("one/hello.txt")).unwrap(),
        "hello\n"
    );
    let seen_now = seen.lock().unwrap().clone();
    assert!(
        seen_now.iter().all(|s| s.ends_with("auth_ok=true")),
        "{seen_now:?}"
    );
    assert!(
        seen_now
            .iter()
            .any(|s| s.starts_with("POST /owner/repo.git/git-upload-pack")),
        "{seen_now:?}"
    );
    // The token is nowhere on the sandbox side.
    for (k, v) in &rewrite {
        assert!(!k.contains(SECRET) && !v.contains(SECRET));
    }
    let cfg = std::fs::read_to_string(sandbox.join("one/.git/config")).unwrap();
    assert!(!cfg.contains(SECRET), "{cfg}");
    assert!(
        cfg.contains(&format!("url = https://{host}/owner/repo.git")),
        "{cfg}"
    );
    assert!(
        !cfg.contains("127.0.0.1"),
        "the rewrite is not in the repository: {cfg}"
    );
    let list = git(&sandbox.join("one"), &["config", "--list"]);
    assert!(!String::from_utf8_lossy(&list.stdout).contains(SECRET));
    // The audit names the repository, never the token.
    assert!(
        audit.contains("github_git POST localhost")
            && audit.contains("/owner/repo.git/git-upload-pack forwarded 200"),
        "{audit}"
    );
    assert!(!audit.contains(SECRET));
    let b64 = base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{SECRET}"));
    assert!(!audit.contains(&b64));

    // A push stays out: receive-pack is not bound.
    let before = seen.lock().unwrap().len();
    let mut cmd = Command::new("git");
    cmd.args(["push", "-q", "origin", "main"])
        .current_dir(sandbox.join("one"))
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", &sandbox)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0");
    for (k, v) in &rewrite {
        cmd.env(k, v);
    }
    assert!(!cmd.output().unwrap().status.success());
    assert_eq!(seen.lock().unwrap().len(), before, "a push left the host");

    // Revoked: the clone fails and nothing leaves the host.
    std::fs::write(&revoked, "github_git\n").unwrap();
    let before = seen.lock().unwrap().len();
    let no = clone("two");
    assert!(!no.status.success(), "{no:?}");
    assert_eq!(seen.lock().unwrap().len(), before);
    assert!(log.lock().unwrap().join("\n").contains("refused: revoked"));
}
