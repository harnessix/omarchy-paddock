use super::*;
use crate::broker::{Approver, Pending};
use crate::devices::{catalog, DevicesCfg};
use crate::env::HostEnv;
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Answers every card with one answer, and counts the cards.
pub struct Fixed(pub Answer, pub AtomicUsize);

impl Approver for Fixed {
    fn ask(&self, _: &HostEnv, p: &Pending, _: Duration) -> Option<Answer> {
        assert_eq!(p.capability, "device");
        assert!(p.session_allowed);
        assert!(p.summary.contains("for this sandbox"), "{}", p.summary);
        self.1.fetch_add(1, Ordering::SeqCst);
        Some(self.0)
    }
}

pub fn ctx(dir: &std::path::Path, answer: Answer) -> (DeviceCtx, Arc<Fixed>) {
    let vars = [
        ("XDG_RUNTIME_DIR", dir.join("run").display().to_string()),
        ("XDG_STATE_HOME", dir.join("state").display().to_string()),
        (
            "PATH",
            format!("{}:/usr/bin:/bin", dir.join("bin").display()),
        ),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    let env = HostEnv::from_parts(dir.join("home"), "demo".into(), 1000, 1000, vars);
    let fixed = Arc::new(Fixed(answer, AtomicUsize::new(0)));
    let ctx = DeviceCtx {
        env,
        sandbox: "box1".into(),
        harness: "fake".into(),
        project: dir.join("project"),
        started_by: vec![],
        approver: fixed.clone(),
    };
    (ctx, fixed)
}

fn http_dev(target: SocketAddr, max_body: &str) -> UserDev {
    let text = format!(
        "[devices.svc]\nkind = \"http\"\nenabled = true\ntarget = \"{target}\"\nmethods = [\"GET\", \"POST\"]\npaths = [\"/api/\"]\nmax_body = \"{max_body}\"\ntimeout = 5\n"
    );
    DevicesCfg::parse(&text).unwrap().user.remove(0)
}

fn call() -> Call {
    Call::for_test("box1", Duration::from_secs(30))
}

#[test]
fn config_reads_http_and_command_tables() {
    let c = DevicesCfg::parse(
        r#"
[devices]
echo = true
[devices.ollama]
kind = "http"
enabled = true
target = "127.0.0.1:11434"
methods = ["GET", "POST"]
paths = ["/api/"]
max_body = "1M"
[devices.jq]
kind = "command"
command = ["jq", "-c", "."]
max_body = "256K"
timeout = 30
"#,
    )
    .unwrap();
    assert_eq!(c.enabled, ["echo", "ollama"]);
    assert_eq!(c.user.len(), 2);
    let o = &c.user.iter().find(|u| u.name == "ollama").unwrap();
    assert_eq!(o.max_body, 1 << 20);
    assert!(o.enabled);
    let j = &c.user.iter().find(|u| u.name == "jq").unwrap();
    assert!(!j.enabled, "a device is off until enabled = true");
    assert_eq!(j.max_body, 256 << 10);
    assert_eq!(j.timeout, 30);
    assert_eq!(
        j.kind,
        UserKind::Command {
            command: vec!["jq".into(), "-c".into(), ".".into()],
            network: false
        }
    );
    // The off device is no device to serve, but a ceiling name for none.
    assert_eq!(
        crate::devices::enabled_names(&c, false, None),
        ["echo", "ollama"]
    );
    assert_eq!(
        crate::devices::enabled_names(&c, false, Some(&["echo".into()])),
        ["echo"]
    );
}

fn err(text: &str) -> String {
    format!("{:#}", DevicesCfg::parse(text).unwrap_err())
}

#[test]
fn config_refuses_targets_off_loopback_and_bad_values() {
    let http = |target: &str| format!("[devices.svc]\nkind = \"http\"\ntarget = \"{target}\"\n");
    for t in ["192.168.1.5:80", "0.0.0.0:11434", "10.0.0.1:1", "[::]:80"] {
        assert!(err(&http(t)).contains("not a loopback address"), "{t}");
    }
    for t in ["localhost:11434", "127.0.0.1", "example.com:80"] {
        assert!(err(&http(t)).contains("an IP address and a port"), "{t}");
    }
    assert!(err(&http("127.0.0.1:0")).contains("needs a port"));
    // The IPv6 loopback and any 127/8 address pass.
    for t in ["[::1]:8080", "127.0.0.2:80"] {
        assert!(DevicesCfg::parse(&http(t)).is_ok(), "{t}");
    }
    for size in ["\"1X\"", "\"0\"", "\"100G\"", "\"-1K\"", "\"\"", "true"] {
        let t = format!("{}max_body = {size}\n", http("127.0.0.1:1"));
        assert!(
            err(&t).contains("max_body") || err(&t).contains("size"),
            "{size}: {}",
            err(&t)
        );
    }
    let base = http("127.0.0.1:1");
    assert!(err(&format!("{base}timeout = 0\n")).contains("timeout"));
    assert!(err(&format!("{base}methods = [\"get\"]\n")).contains("methods"));
    assert!(err(&format!("{base}paths = [\"api\"]\n")).contains("paths"));
    assert!(err(&format!("{base}paths = [\"/a/../b\"]\n")).contains("paths"));
    assert!(err(&format!("{base}command = [\"x\"]\n")).contains("unknown key command"));
    assert!(err("[devices.x]\nkind = \"ftp\"\n").contains("unknown kind"));
    assert!(err("[devices.x]\nkind = \"command\"\ncommand = []\n").contains("command must be"));
    assert!(err("[devices.Bad]\nkind = \"command\"\ncommand = [\"x\"]\n").contains("device name"));
    assert!(err("[devices.ask]\nkind = \"command\"\ncommand = [\"x\"]\n").contains("built-in"));
}

#[test]
fn sizes_take_k_m_and_g() {
    assert_eq!(parse_size("512").unwrap(), 512);
    assert_eq!(parse_size("256K").unwrap(), 256 * 1024);
    assert_eq!(parse_size("1m").unwrap(), 1 << 20);
    assert_eq!(parse_size("64M").unwrap(), MAX_BODY_CAP);
    assert!(parse_size("65M").is_err());
    assert!(parse_size("1G").is_err());
    assert!(parse_size("K").is_err());
    assert!(parse_size("1.5M").is_err());
}

#[test]
fn the_project_file_cannot_define_a_device() {
    let r: Result<crate::manifest::Manifest, _> = toml::from_str(
        "[devices.svc]\nkind = \"http\"\nenabled = true\ntarget = \"127.0.0.1:1\"\n",
    );
    assert!(r.is_err());
}

#[test]
fn the_request_format_has_a_line_headers_and_a_body() {
    let r = parse_http(
        b"POST /api/chat\nContent-Type: application/json\nX-A:  b \n\n{\"x\":1}",
        100,
    )
    .unwrap();
    assert_eq!(r.method, "POST");
    assert_eq!(r.path, "/api/chat");
    assert_eq!(
        r.headers,
        [
            ("Content-Type".to_string(), "application/json".to_string()),
            ("X-A".to_string(), "b".to_string())
        ]
    );
    assert_eq!(r.body, b"{\"x\":1}");
    // CRLF and a version work too; no empty line means no body.
    let r = parse_http(b"GET /api/tags HTTP/1.1\r\nAccept: */*\r\n\r\n", 100).unwrap();
    assert_eq!(
        (r.method.as_str(), r.headers.len(), r.body.len()),
        ("GET", 1, 0)
    );
    let r = parse_http(b"GET /api/tags\n", 100).unwrap();
    assert!(r.headers.is_empty() && r.body.is_empty());
    // The body keeps its empty lines.
    let r = parse_http(b"POST /api/x\n\na\n\nb", 100).unwrap();
    assert_eq!(r.body, b"a\n\nb");
    for bad in [
        &b""[..],
        b"GET",
        b"get /api/",
        b"GET /api/ HTTP/2 x",
        b"GET /api/ FTP/1",
        b"GET /api/\nno colon\n\n",
        b"GET /api/\nBad Name: x\n\n",
        b"GET /api/\nX: a\x1bb\n\n",
        b"\xff\xfe /api/",
    ] {
        assert_eq!(
            parse_http(bad, 100).unwrap_err(),
            DevError::Invalid,
            "{bad:?}"
        );
    }
    assert_eq!(
        parse_http(b"POST /api/x\n\n12345", 4).unwrap_err(),
        DevError::TooLarge
    );
    assert!(parse_http(b"POST /api/x\n\n1234", 4).is_ok());
}

#[test]
fn methods_and_path_prefixes_are_checked() {
    let m = vec!["GET".to_string(), "POST".to_string()];
    let p = vec!["/api/".to_string()];
    let r = |line: &str| refusal(&parse_http(line.as_bytes(), 10).unwrap(), &m, &p);
    assert_eq!(r("GET /api/tags"), None);
    assert_eq!(r("POST /api/generate?stream=false"), None);
    assert!(r("DELETE /api/x").unwrap().contains("method DELETE"));
    assert!(r("GET /admin").unwrap().contains("outside"));
    assert!(r("GET /ap").unwrap().contains("outside"));
    assert!(r("GET /x?/api/").unwrap().contains("outside"));
    for tricky in [
        "GET /api/../admin",
        "GET /api/./x",
        "GET /api/%2e%2e/admin",
        "GET /api/%2E%2E/admin",
        "GET /api/..%2Fadmin",
        "GET /api//x",
        "GET /api/x#y",
        "GET /api/a\\..\\b",
        "GET api/x",
        "GET http://127.0.0.1:1/api/x",
    ] {
        assert!(r(tricky).is_some(), "{tricky}");
    }
}

#[test]
fn credentials_and_hop_by_hop_headers_never_pass() {
    let h = |n: &str, v: &str| (n.to_string(), v.to_string());
    let kept = strip_headers(&[
        h("Authorization", "Bearer x"),
        h("cookie", "a=b"),
        h("Proxy-Authorization", "x"),
        h("Connection", "keep-alive, X-Private"),
        h("X-Private", "secret"),
        h("Keep-Alive", "5"),
        h("Transfer-Encoding", "chunked"),
        h("TE", "trailers"),
        h("Upgrade", "websocket"),
        h("Host", "evil.example"),
        h("Content-Length", "99"),
        h("Expect", "100-continue"),
        h("Content-Type", "application/json"),
        h("Accept", "*/*"),
    ]);
    assert_eq!(
        kept,
        [h("Content-Type", "application/json"), h("Accept", "*/*")]
    );
}

#[test]
fn chunked_and_sized_responses_decode() {
    let r = complete(
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\nX-A: 1\r\nSet-Cookie: s=1\r\n\r\n3\r\nabc\r\n2;x=y\r\nde\r\n0\r\nT: 1\r\n\r\n",
        false,
        false,
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        String::from_utf8(r).unwrap(),
        "HTTP/1.1 200 OK\nX-A: 1\nContent-Length: 5\n\nabcde"
    );
    // Incomplete: more bytes must come.
    assert!(complete(
        b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nab",
        false,
        false
    )
    .is_none());
    assert!(complete(
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nab",
        false,
        false
    )
    .is_none());
    // The end of the connection ends a body without a length.
    let r = complete(b"HTTP/1.0 404 Not Found\n\nnope", false, true)
        .unwrap()
        .unwrap();
    assert!(String::from_utf8(r)
        .unwrap()
        .ends_with("Content-Length: 4\n\nnope"));
    // An interim 100 is skipped; HEAD and 204 have no body.
    let r = complete(
        b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 204 No Content\r\n\r\n",
        false,
        false,
    )
    .unwrap()
    .unwrap();
    assert!(String::from_utf8(r).unwrap().starts_with("HTTP/1.1 204"));
    assert!(complete(b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\n", true, false).is_some());
    assert!(matches!(
        complete(b"SSH-2.0-x\r\n\r\n", false, false),
        Some(Err(Upstream::Unreachable(_)))
    ));
    assert!(matches!(dechunk(b"zz\r\n"), Some(Err(()))));
}

/// A tiny HTTP server: it records each request and answers with `reply`.
fn server(reply: &'static [u8]) -> (SocketAddr, Arc<Mutex<Vec<String>>>, TcpListener) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (l2, seen2) = (l.try_clone().unwrap(), seen.clone());
    std::thread::spawn(move || {
        for s in l2.incoming() {
            let Ok(mut s) = s else { return };
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let n = s.read(&mut chunk).unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf).to_string();
                if let Some(i) = text.find("\r\n\r\n") {
                    let len = text
                        .lines()
                        .find_map(|l| l.strip_prefix("Content-Length: "))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if buf.len() >= i + 4 + len {
                        break;
                    }
                }
            }
            seen2
                .lock()
                .unwrap()
                .push(String::from_utf8_lossy(&buf).to_string());
            let _ = s.write_all(reply);
        }
    });
    (addr, seen, l)
}

#[test]
fn a_request_reaches_the_loopback_service_and_returns_its_answer() {
    let dir = tempfile::tempdir().unwrap();
    let (addr, seen, _l) = server(
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n7\r\n{\"a\":1}\r\n0\r\n\r\n",
    );
    let (ctx, approver) = ctx(dir.path(), Answer::Session);
    let dev = http_dev(addr, "1K").build(&ctx, true);
    let req = b"POST /api/generate\nContent-Type: application/json\nAuthorization: Bearer sk-x\nCookie: s=1\nConnection: X-Private\nX-Private: p\nX-Keep: k\n\n{\"model\":\"m\"}";
    let out = dev.call(&call(), req).unwrap();
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "HTTP/1.1 200 OK\nContent-Type: application/json\nContent-Length: 7\n\n{\"a\":1}"
    );
    let got = seen.lock().unwrap()[0].clone();
    assert!(got.starts_with("POST /api/generate HTTP/1.1\r\n"), "{got}");
    assert!(got.contains(&format!("Host: {addr}\r\n")), "{got}");
    assert!(
        got.contains("X-Keep: k\r\n") && got.contains("Content-Length: 13\r\n"),
        "{got}"
    );
    assert!(got.contains("Connection: close\r\n"), "{got}");
    for gone in ["Authorization", "Cookie", "X-Private", "sk-x"] {
        assert!(!got.contains(gone), "{gone} reached the service: {got}");
    }
    assert!(got.ends_with("{\"model\":\"m\"}"));
    // "Allow for this session": the second request asks nobody.
    dev.call(&call(), b"GET /api/tags").unwrap();
    assert_eq!(approver.1.load(Ordering::SeqCst), 1);
    assert_eq!(seen.lock().unwrap().len(), 2);
    // The audit keeps the method, the path and the status, never a body.
    let audit = std::fs::read_to_string(crate::broker::audit_file(&ctx.env)).unwrap();
    assert!(
        audit.contains("POST /api/generate") && audit.contains("\"device\":\"svc\""),
        "{audit}"
    );
    assert!(
        audit.contains("\"status\":\"200\"") && audit.contains("duration_ms"),
        "{audit}"
    );
    assert!(
        !audit.contains("model") && !audit.contains("sk-x"),
        "{audit}"
    );
}

#[test]
fn a_refused_or_declined_request_never_reaches_the_service() {
    let dir = tempfile::tempdir().unwrap();
    let (addr, seen, _l) = server(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
    let (ctx, approver) = ctx(dir.path(), Answer::Session);
    let dev = http_dev(addr, "1K").build(&ctx, true);
    for req in [&b"GET /admin"[..], b"DELETE /api/x", b"GET /api/../admin"] {
        let out = String::from_utf8(dev.call(&call(), req).unwrap()).unwrap();
        assert!(
            out.starts_with("HTTP/1.1 403 Forbidden\nPaddock-Status: refused\n"),
            "{out}"
        );
    }
    // A refusal asks nobody.
    assert_eq!(approver.1.load(Ordering::SeqCst), 0);
    // A body over max_body.
    let mut big = b"POST /api/x\n\n".to_vec();
    big.extend(vec![b'x'; 1025]);
    assert_eq!(dev.call(&call(), &big).unwrap_err(), DevError::TooLarge);
    // The user denies for the session: no more cards.
    let (ctx, approver) = super::tests::ctx(dir.path(), Answer::DenySession);
    let dev = http_dev(addr, "1K").build(&ctx, true);
    for _ in 0..2 {
        assert_eq!(
            dev.call(&call(), b"GET /api/tags").unwrap_err(),
            DevError::Denied
        );
    }
    assert_eq!(approver.1.load(Ordering::SeqCst), 1);
    std::thread::sleep(Duration::from_millis(100));
    assert!(seen.lock().unwrap().is_empty());
}

#[test]
fn a_response_over_max_body_fails_and_a_dead_port_is_unreachable() {
    let dir = tempfile::tempdir().unwrap();
    let (ctx, _) = ctx(dir.path(), Answer::Session);
    static BIG: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    let big = BIG.get_or_init(|| {
        let mut v = b"HTTP/1.1 200 OK\r\nContent-Length: 40000\r\n\r\n".to_vec();
        v.extend(vec![b'x'; 40000]);
        v
    });
    let (addr, _, _l) = server(big);
    let dev = http_dev(addr, "1K").build(&ctx, true);
    assert_eq!(
        dev.call(&call(), b"GET /api/x").unwrap_err(),
        DevError::TooLarge
    );
    // Nothing listens: paddock answers itself.
    let free = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let dev = http_dev(free, "1K").build(&ctx, true);
    let out = String::from_utf8(dev.call(&call(), b"GET /api/x").unwrap()).unwrap();
    assert!(
        out.starts_with("HTTP/1.1 502 Bad Gateway\nPaddock-Status: unreachable\n"),
        "{out}"
    );
}

#[test]
fn an_off_device_answers_disabled_and_the_catalog_shows_it() {
    let dir = tempfile::tempdir().unwrap();
    let (ctx, approver) = ctx(dir.path(), Answer::Session);
    let cfg = DevicesCfg::parse(
        "[devices.svc]\nkind = \"http\"\ntarget = \"127.0.0.1:9\"\n[devices.tool]\nkind = \"command\"\nenabled = true\ncommand = [\"cat\"]\n",
    )
    .unwrap();
    let mut served = crate::devices::enabled(&cfg, false, None, &ctx);
    assert_eq!(served.len(), 1);
    served.extend(crate::devices::disabled(&cfg, &served, &ctx));
    let svc = served.iter().find(|d| d.name() == "svc").unwrap();
    let out = String::from_utf8(svc.call(&call(), b"GET /x").unwrap()).unwrap();
    assert!(
        out.contains("Paddock-Status: disabled") && out.contains("enabled = true"),
        "{out}"
    );
    assert_eq!(approver.1.load(Ordering::SeqCst), 0);
    let cat: serde_json::Value = serde_json::from_slice(&catalog(&served)).unwrap();
    let find = |n: &str| {
        cat["devices"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["name"] == n)
            .unwrap()
            .clone()
    };
    let svc = find("svc");
    assert_eq!(svc["state"], "off");
    assert_eq!(svc["kind"], "http");
    assert_eq!(svc["target"], "127.0.0.1:9");
    assert_eq!(svc["path"], "/dev/paddock/svc");
    assert!(svc["how_to_enable"]
        .as_str()
        .unwrap()
        .contains("[devices.svc]"));
    assert!(svc["request"].as_str().unwrap().contains("METHOD PATH"));
    let tool = find("tool");
    assert_eq!(tool["state"], "on");
    assert_eq!(tool["kind"], "command");
    assert_eq!(tool["command"], serde_json::json!(["cat"]));
    assert_eq!(tool["limits"]["max_request"], DEFAULT_MAX_BODY + HEAD_ROOM);
    // A built-in that is off keeps its own text.
    assert!(find("echo")["how_to_enable"]
        .as_str()
        .unwrap()
        .contains("echo = true"));
}

#[test]
fn an_http_device_answers_through_the_mount() {
    let Ok(host) = HostEnv::detect() else { return };
    if let Err(why) = crate::devices::fuse_available(&host) {
        eprintln!("skipped: {why}");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (addr, seen, _l) = server(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
    let (ctx, _) = ctx(dir.path(), Answer::Session);
    let cfg =
        DevicesCfg::parse("[devices.off]\nkind = \"command\"\ncommand = [\"cat\"]\n").unwrap();
    let mut devs = vec![http_dev(addr, "1K").build(&ctx, true)];
    devs.extend(crate::devices::disabled(&cfg, &devs, &ctx));
    let root = dir.path().join("dev");
    let _m = match crate::devices::mount(&root, crate::devices::DeviceFs::new("box1", devs)) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("skipped: {e:#}");
            return;
        }
    };
    let ask = |name: &str, req: &[u8]| {
        let mut h = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(root.join(name))
            .unwrap();
        h.write_all(req).unwrap();
        let mut out = String::new();
        h.read_to_string(&mut out).unwrap();
        out
    };
    assert_eq!(
        ask("svc", b"GET /api/tags\n"),
        "HTTP/1.1 200 OK\nContent-Length: 2\n\nok"
    );
    assert_eq!(seen.lock().unwrap().len(), 1);
    assert!(ask("off", b"x").starts_with("status: disabled\n"));
    let cat = std::fs::read_to_string(root.join(".catalog")).unwrap();
    assert!(cat.contains("\"/dev/paddock/svc\"") && cat.contains("\"/dev/paddock/off\""));
}
