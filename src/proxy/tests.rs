//! Tests for the host proxy. The fake servers here speak just enough of each
//! protocol to check what the proxy sends.

use super::*;
use std::io::{BufRead, BufReader};
use std::net::TcpListener;

fn echo_server() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for c in l.incoming().flatten() {
            std::thread::spawn(move || {
                let mut r = c.try_clone().unwrap();
                let mut w = c;
                let _ = std::io::copy(&mut r, &mut w);
            });
        }
    });
    port
}

fn start(cfg: HostProxy) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("p.sock");
    serve(&sock, cfg).unwrap();
    (dir, sock)
}

/// Send a request and return the status code and the open stream.
fn request(sock: &Path, text: &str) -> (u16, UnixStream) {
    let mut s = UnixStream::connect(sock).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s.write_all(text.as_bytes()).unwrap();
    let (head, _) = read_head(&mut s).unwrap();
    let mut h = [httparse::EMPTY_HEADER; 16];
    let mut r = httparse::Response::new(&mut h);
    r.parse(&head).unwrap();
    (r.code.unwrap(), s)
}

fn connect(sock: &Path, target: &str) -> (u16, UnixStream) {
    request(
        sock,
        &format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n"),
    )
}

fn roundtrip(s: &mut UnixStream, msg: &[u8]) {
    s.write_all(msg).unwrap();
    let mut buf = vec![0u8; msg.len()];
    s.read_exact(&mut buf).unwrap();
    assert_eq!(buf, msg);
}

#[test]
fn connect_tunnel_to_an_allowed_port() {
    let port = echo_server();
    let (_d, sock) = start(HostProxy {
        allow_loopback: vec![port],
        ..Default::default()
    });
    let (code, mut s) = connect(&sock, &format!("127.0.0.1:{port}"));
    assert_eq!(code, 200);
    roundtrip(&mut s, b"hello through the tunnel");
}

#[test]
fn refuses_loopback_ports_that_are_not_allowed() {
    let port = echo_server();
    let (_d, sock) = start(HostProxy::default());
    assert_eq!(connect(&sock, &format!("127.0.0.1:{port}")).0, 403);
    assert_eq!(connect(&sock, &format!("localhost:{port}")).0, 403);
}

#[test]
fn refuses_other_addresses_of_this_host() {
    let (_d, sock) = start(HostProxy::default());
    for ip in local_addrs().into_iter().filter(|i| !i.is_loopback()) {
        assert_eq!(
            connect(&sock, &authority(&ip.to_string(), 9)).0,
            403,
            "{ip}"
        );
    }
    assert_eq!(connect(&sock, "0.0.0.0:9").0, 403);
}

#[test]
fn unreachable_target_gives_502() {
    let (_d, sock) = start(HostProxy::default());
    assert_eq!(connect(&sock, "no-such-host.invalid:443").0, 502);
}

#[test]
fn forwards_plain_http_in_origin_form() {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(String::new()));
    let seen2 = seen.clone();
    std::thread::spawn(move || {
        let (c, _) = l.accept().unwrap();
        let mut r = BufReader::new(c.try_clone().unwrap());
        let mut line = String::new();
        r.read_line(&mut line).unwrap();
        *seen2.lock().unwrap() = line;
        let mut w = c;
        w.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
            .unwrap();
    });
    let (_d, sock) = start(HostProxy {
        allow_loopback: vec![port],
        ..Default::default()
    });
    let mut s = UnixStream::connect(&sock).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s.write_all(format!("GET http://127.0.0.1:{port}/a?b=1 HTTP/1.1\r\nHost: 127.0.0.1\r\nProxy-Connection: keep-alive\r\n\r\n").as_bytes()).unwrap();
    let mut all = Vec::new();
    let _ = s.read_to_end(&mut all);
    let text = String::from_utf8_lossy(&all);
    assert!(
        text.starts_with("HTTP/1.1 200 OK") && text.ends_with("ok"),
        "{text}"
    );
    assert_eq!(seen.lock().unwrap().as_str(), "GET /a?b=1 HTTP/1.1\r\n");
}

/// A fake upstream HTTP proxy: answers CONNECT with 200, then echoes.
fn fake_http_upstream() -> (u16, Arc<Mutex<Vec<String>>>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let log = Arc::new(Mutex::new(Vec::new()));
    let log2 = log.clone();
    std::thread::spawn(move || {
        for mut c in l.incoming().flatten() {
            let log = log2.clone();
            std::thread::spawn(move || {
                let (head, _) = read_head(&mut c).unwrap();
                log.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&head).into_owned());
                c.write_all(b"HTTP/1.1 200 OK\r\n\r\n").unwrap();
                let mut r = c.try_clone().unwrap();
                let _ = std::io::copy(&mut r, &mut c);
            });
        }
    });
    (port, log)
}

#[test]
fn chains_through_an_upstream_http_proxy_with_auth() {
    let (up, log) = fake_http_upstream();
    let cfg = HostProxy::from_vars(|k| {
        (k == "HTTPS_PROXY").then(|| format!("http://user:p%40ss@127.0.0.1:{up}"))
    });
    let (_d, sock) = start(cfg);
    let (code, mut s) = connect(&sock, "api.example.invalid:443");
    assert_eq!(code, 200);
    roundtrip(&mut s, b"via upstream");
    let head = log.lock().unwrap()[0].clone();
    assert!(
        head.starts_with("CONNECT api.example.invalid:443 HTTP/1.1"),
        "{head}"
    );
    let expected = base64::engine::general_purpose::STANDARD.encode("user:p@ss");
    assert!(
        head.contains(&format!("Proxy-Authorization: Basic {expected}")),
        "{head}"
    );
}

#[test]
fn no_proxy_goes_direct() {
    let (up, log) = fake_http_upstream();
    let cfg = HostProxy::from_vars(|k| match k {
        "HTTPS_PROXY" => Some(format!("http://127.0.0.1:{up}")),
        "NO_PROXY" => Some(".bypass.invalid,10.0.0.0/8".into()),
        _ => None,
    });
    assert!(
        cfg.bypass("x.bypass.invalid") && cfg.bypass("10.1.2.3") && !cfg.bypass("api.example.com")
    );
    let (_d, sock) = start(cfg);
    // Direct: the name does not resolve, so 502, and the upstream sees nothing.
    assert_eq!(connect(&sock, "x.bypass.invalid:443").0, 502);
    assert!(log.lock().unwrap().is_empty());
}

#[test]
fn chains_through_an_upstream_socks5_proxy() {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let target = Arc::new(Mutex::new(String::new()));
    let t2 = target.clone();
    std::thread::spawn(move || {
        let (mut c, _) = l.accept().unwrap();
        let mut b = [0u8; 2];
        c.read_exact(&mut b).unwrap();
        let mut methods = vec![0u8; b[1] as usize];
        c.read_exact(&mut methods).unwrap();
        c.write_all(&[5, 0]).unwrap();
        let mut h = [0u8; 4];
        c.read_exact(&mut h).unwrap();
        assert_eq!(
            h[3], 3,
            "the proxy must send the name, not a local resolution"
        );
        let mut len = [0u8; 1];
        c.read_exact(&mut len).unwrap();
        let mut name = vec![0u8; len[0] as usize + 2];
        c.read_exact(&mut name).unwrap();
        *t2.lock().unwrap() = String::from_utf8_lossy(&name[..len[0] as usize]).into_owned();
        c.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).unwrap();
        let mut r = c.try_clone().unwrap();
        let _ = std::io::copy(&mut r, &mut c);
    });
    let cfg =
        HostProxy::from_vars(|k| (k == "ALL_PROXY").then(|| format!("socks5h://127.0.0.1:{port}")));
    let (_d, sock) = start(cfg);
    let (code, mut s) = connect(&sock, "chat.example.invalid:443");
    assert_eq!(code, 200);
    roundtrip(&mut s, b"via socks");
    assert_eq!(target.lock().unwrap().as_str(), "chat.example.invalid");
}

#[test]
fn long_connections_stay_open_while_active_and_close_when_idle() {
    let port = echo_server();
    let cfg = HostProxy {
        allow_loopback: vec![port],
        idle: Duration::from_millis(600),
        ..Default::default()
    };
    let (_d, sock) = start(cfg);
    let (code, mut s) = connect(&sock, &format!("127.0.0.1:{port}"));
    assert_eq!(code, 200);
    // Like WebSocket frames: traffic every 200 ms for 2 seconds, longer than the idle limit.
    for i in 0..10 {
        roundtrip(&mut s, format!("frame {i}").as_bytes());
        std::thread::sleep(Duration::from_millis(200));
    }
    // Then silence: the tunnel closes.
    std::thread::sleep(Duration::from_millis(1500));
    let mut buf = [0u8; 8];
    assert_eq!(s.read(&mut buf).unwrap_or(0), 0);
}

#[test]
fn limits_concurrent_connections() {
    let port = echo_server();
    let (_d, sock) = start(HostProxy {
        allow_loopback: vec![port],
        max_conns: 1,
        ..Default::default()
    });
    let (code, _held) = connect(&sock, &format!("127.0.0.1:{port}"));
    assert_eq!(code, 200);
    assert_eq!(connect(&sock, &format!("127.0.0.1:{port}")).0, 503);
}

#[test]
fn parses_proxy_urls() {
    assert_eq!(
        parse_url("http://127.0.0.1:7890").unwrap(),
        ("http".into(), "127.0.0.1".into(), 7890)
    );
    assert_eq!(
        parse_url("socks5h://user:pw@proxy.example:1081/").unwrap(),
        ("socks5h".into(), "proxy.example".into(), 1081)
    );
    assert_eq!(parse_url("socks5://proxy.example").unwrap().2, 1080);
    assert_eq!(
        parse_url("localhost:8080").unwrap(),
        ("http".into(), "localhost".into(), 8080)
    );
    assert_eq!(parse_url("http://[::1]:9").unwrap().1, "::1");
    assert!(
        is_loopback("127.0.0.1")
            && is_loopback("localhost")
            && is_loopback("::1")
            && !is_loopback("10.0.0.1")
    );
}
