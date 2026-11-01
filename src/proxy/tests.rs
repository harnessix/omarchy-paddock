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
    // A proxy at its connection limit answers 503 and closes without reading,
    // so the write can fail. The answer is still there to read.
    let _ = s.write_all(text.as_bytes());
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

#[test]
fn lan_ranges() {
    for a in [
        "10.1.2.3",
        "172.16.0.1",
        "172.31.255.255",
        "192.168.5.1",
        "169.254.1.1",
        "100.64.0.1",
        "100.127.255.255",
        "fd00::1",
        "fe80::1",
    ] {
        assert!(is_lan(&a.parse().unwrap()), "{a}");
    }
    for a in [
        "8.8.8.8",
        "172.32.0.1",
        "100.128.0.1",
        "198.18.0.1",
        "1.1.1.1",
        "2001:db8::1",
    ] {
        assert!(!is_lan(&a.parse().unwrap()), "{a}");
    }
}

#[test]
fn mapped_ipv6_forms_count_as_their_ipv4_address() {
    for a in ["::ffff:127.0.0.1", "::127.0.0.1", "64:ff9b::7f00:1"] {
        assert!(canonical(a.parse().unwrap()).is_loopback(), "{a}");
    }
    for a in [
        "::ffff:192.168.5.1",
        "::ffff:10.0.0.1",
        "64:ff9b::a9fe:a9fe",
    ] {
        assert!(is_lan(&a.parse().unwrap()), "{a}");
    }
    assert!(is_loopback("::ffff:127.0.0.1"));
    assert!(!is_lan(&"::ffff:8.8.8.8".parse().unwrap()));
    // Real IPv6 loopback and unspecified stay what they are.
    assert!(canonical("::1".parse().unwrap()).is_loopback());
    assert!(canonical("::".parse().unwrap()).is_unspecified());
}

#[test]
fn proxy_refuses_mapped_loopback_and_lan() {
    let cfg = HostProxy::default();
    for host in ["::ffff:127.0.0.1", "::ffff:192.168.1.1", "64:ff9b::7f00:1"] {
        let e = check_target(&cfg, host, 8080).unwrap_err().to_string();
        assert!(e.starts_with("refused"), "{host}: {e}");
    }
    let ok = check_target(&cfg, "::ffff:8.8.8.8", 443).unwrap();
    assert_eq!(ok.len(), 1);
}

// An `https://` upstream proxy. The fake proxy records every byte that
// arrives on its TCP socket, so a test can check that nothing readable, and
// above all no credential, crossed the network outside TLS.

struct Recording {
    inner: std::net::TcpStream,
    raw: Arc<Mutex<Vec<u8>>>,
}

impl Read for Recording {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.raw.lock().unwrap().extend_from_slice(&buf[..n]);
        Ok(n)
    }
}

impl Write for Recording {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.inner.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

struct TlsProxy {
    port: u16,
    /// Every byte the proxy received on the wire.
    raw: Arc<Mutex<Vec<u8>>>,
    /// The request heads it decrypted.
    heads: Arc<Mutex<Vec<String>>>,
}

fn self_signed() -> rcgen::CertifiedKey<rcgen::KeyPair> {
    rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap()
}

/// A TLS proxy with the certificate `ck`. It answers CONNECT with 200 and then
/// echoes; it answers a forwarded GET with "ok".
fn tls_proxy(ck: &rcgen::CertifiedKey<rcgen::KeyPair>) -> TlsProxy {
    use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
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
    let raw = Arc::new(Mutex::new(Vec::new()));
    let heads = Arc::new(Mutex::new(Vec::new()));
    let (raw2, heads2) = (raw.clone(), heads.clone());
    std::thread::spawn(move || {
        for c in l.incoming().flatten() {
            let (scfg, raw, heads) = (scfg.clone(), raw2.clone(), heads2.clone());
            std::thread::spawn(move || {
                let conn = rustls::ServerConnection::new(scfg).unwrap();
                let mut s = rustls::StreamOwned::new(conn, Recording { inner: c, raw });
                let Ok((head, _)) = read_head(&mut s) else {
                    return;
                };
                let head = String::from_utf8_lossy(&head).into_owned();
                let connect = head.starts_with("CONNECT ");
                heads.lock().unwrap().push(head);
                if connect {
                    s.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                        .unwrap();
                    let mut buf = [0u8; 4096];
                    loop {
                        match s.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                if s.write_all(&buf[..n]).is_err() {
                                    break;
                                }
                            }
                        }
                    }
                } else {
                    let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
                }
                s.conn.send_close_notify();
                let _ = s.flush();
            });
        }
    });
    TlsProxy { port, raw, heads }
}

fn trusting(ck: &rcgen::CertifiedKey<rcgen::KeyPair>) -> Arc<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(ck.cert.der().clone()).unwrap();
    tls::config_with_roots(roots).unwrap()
}

fn https_cfg(var: &str, port: u16, tls: Arc<rustls::ClientConfig>) -> HostProxy {
    let url = format!("https://user:s3cret@localhost:{port}");
    HostProxy {
        tls: Some(tls),
        ..HostProxy::from_vars(|k| (k == var).then(|| url.clone()))
    }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

fn creds_b64() -> String {
    base64::engine::general_purpose::STANDARD.encode("user:s3cret")
}

#[test]
fn https_proxy_url_keeps_its_tls_scheme() {
    for var in ["HTTPS_PROXY", "HTTP_PROXY", "ALL_PROXY"] {
        let cfg =
            HostProxy::from_vars(|k| (k == var).then(|| "https://u:p@proxy.example:8443".into()));
        let up = cfg.https.as_ref().or(cfg.http.as_ref()).unwrap();
        assert!(
            matches!(
                up,
                Upstream::Http {
                    tls: true,
                    port: 8443,
                    ..
                }
            ),
            "{var}: {up:?}"
        );
    }
    let cfg =
        HostProxy::from_vars(|k| (k == "HTTPS_PROXY").then(|| "http://proxy.example:3128".into()));
    assert!(matches!(cfg.https, Some(Upstream::Http { tls: false, .. })));
    // An https URL without a port uses 443.
    let cfg =
        HostProxy::from_vars(|k| (k == "HTTPS_PROXY").then(|| "https://proxy.example".into()));
    assert!(matches!(
        cfg.https,
        Some(Upstream::Http {
            tls: true,
            port: 443,
            ..
        })
    ));
}

#[test]
fn connect_through_an_https_proxy_sends_credentials_only_inside_tls() {
    let ck = self_signed();
    let p = tls_proxy(&ck);
    let (_d, sock) = start(https_cfg("HTTPS_PROXY", p.port, trusting(&ck)));
    let (code, mut s) = connect(&sock, "api.example.invalid:443");
    assert_eq!(code, 200);
    roundtrip(&mut s, b"through the TLS tunnel");
    // A larger transfer in both directions at once.
    let big: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    let mut w = s.try_clone().unwrap();
    let send = big.clone();
    let t = std::thread::spawn(move || w.write_all(&send).unwrap());
    let mut back = vec![0u8; big.len()];
    s.read_exact(&mut back).unwrap();
    t.join().unwrap();
    assert!(back == big);
    let head = p.heads.lock().unwrap()[0].clone();
    assert!(
        head.starts_with("CONNECT api.example.invalid:443 HTTP/1.1"),
        "{head}"
    );
    assert!(
        head.contains(&format!("Proxy-Authorization: Basic {}", creds_b64())),
        "{head}"
    );
    let raw = p.raw.lock().unwrap();
    assert!(!contains(&raw, creds_b64().as_bytes()));
    assert!(!contains(&raw, b"CONNECT"));
    assert!(!contains(&raw, b"through the TLS tunnel"));
}

#[test]
fn plain_http_through_an_https_proxy_stays_inside_tls() {
    let ck = self_signed();
    let p = tls_proxy(&ck);
    let (_d, sock) = start(https_cfg("HTTP_PROXY", p.port, trusting(&ck)));
    let mut s = UnixStream::connect(&sock).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s.write_all(
        b"GET http://site.example.invalid/x HTTP/1.1\r\nHost: site.example.invalid\r\n\r\n",
    )
    .unwrap();
    let mut all = Vec::new();
    let _ = s.read_to_end(&mut all);
    let text = String::from_utf8_lossy(&all);
    assert!(
        text.starts_with("HTTP/1.1 200 OK") && text.ends_with("ok"),
        "{text}"
    );
    let head = p.heads.lock().unwrap()[0].clone();
    assert!(
        head.starts_with("GET http://site.example.invalid/x HTTP/1.1"),
        "{head}"
    );
    assert!(
        head.contains(&format!("Proxy-Authorization: Basic {}", creds_b64())),
        "{head}"
    );
    let raw = p.raw.lock().unwrap();
    assert!(!contains(&raw, creds_b64().as_bytes()));
    assert!(!contains(&raw, b"site.example.invalid/x"));
}

#[test]
fn an_untrusted_proxy_certificate_fails_closed() {
    let ck = self_signed();
    let p = tls_proxy(&ck);
    // The client trusts a different CA.
    let (_d, sock) = start(https_cfg("ALL_PROXY", p.port, trusting(&self_signed())));
    assert_eq!(connect(&sock, "api.example.invalid:443").0, 502);
    let mut s = UnixStream::connect(&sock).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s.write_all(b"GET http://site.example.invalid/ HTTP/1.1\r\nHost: site.example.invalid\r\n\r\n")
        .unwrap();
    let (head, _) = read_head(&mut s).unwrap();
    assert!(head.starts_with(b"HTTP/1.1 502"));
    std::thread::sleep(Duration::from_millis(200));
    assert!(p.heads.lock().unwrap().is_empty());
    let raw = p.raw.lock().unwrap();
    assert!(!raw.is_empty(), "the client hello arrived");
    assert!(!contains(&raw, creds_b64().as_bytes()));
    assert!(!contains(&raw, b"CONNECT"));
}

#[test]
fn an_https_proxy_that_speaks_plain_http_gets_no_credentials() {
    // A listener that records what arrives and answers like a plain proxy.
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let raw = Arc::new(Mutex::new(Vec::new()));
    let raw2 = raw.clone();
    std::thread::spawn(move || {
        for mut c in l.incoming().flatten() {
            c.set_read_timeout(Some(Duration::from_millis(500)))
                .unwrap();
            let mut buf = [0u8; 4096];
            if let Ok(n) = c.read(&mut buf) {
                raw2.lock().unwrap().extend_from_slice(&buf[..n]);
            }
            let _ = c.write_all(b"HTTP/1.1 200 OK\r\n\r\n");
        }
    });
    let ck = self_signed();
    let (_d, sock) = start(https_cfg("HTTPS_PROXY", port, trusting(&ck)));
    assert_eq!(connect(&sock, "api.example.invalid:443").0, 502);
    let raw = raw.lock().unwrap();
    assert!(!contains(&raw, creds_b64().as_bytes()));
    assert!(!contains(&raw, b"CONNECT"));
}

#[test]
fn https_tunnel_passes_a_half_close() {
    let ck = self_signed();
    let p = tls_proxy(&ck);
    let (_d, sock) = start(https_cfg("HTTPS_PROXY", p.port, trusting(&ck)));
    let (code, mut s) = connect(&sock, "api.example.invalid:443");
    assert_eq!(code, 200);
    // The client stops sending; the answer still comes back, then the end.
    s.write_all(b"last words").unwrap();
    s.shutdown(Shutdown::Write).unwrap();
    let mut all = Vec::new();
    s.read_to_end(&mut all).unwrap();
    assert_eq!(all, b"last words");
}

#[test]
fn https_tunnel_closes_when_idle() {
    let ck = self_signed();
    let p = tls_proxy(&ck);
    let mut cfg = https_cfg("HTTPS_PROXY", p.port, trusting(&ck));
    cfg.idle = Duration::from_millis(600);
    let (_d, sock) = start(cfg);
    let (code, mut s) = connect(&sock, "api.example.invalid:443");
    assert_eq!(code, 200);
    for i in 0..6 {
        roundtrip(&mut s, format!("frame {i}").as_bytes());
        std::thread::sleep(Duration::from_millis(200));
    }
    std::thread::sleep(Duration::from_millis(1500));
    let mut buf = [0u8; 8];
    assert_eq!(s.read(&mut buf).unwrap_or(0), 0);
}

#[test]
fn warns_about_cleartext_credentials_to_remote_proxies() {
    let warn = |var: &str, url: &str| {
        HostProxy::from_vars(|k| (k == var).then(|| url.to_string())).cleartext_warnings()
    };
    assert_eq!(
        warn("HTTPS_PROXY", "http://u:p@proxy.example:3128").len(),
        1
    );
    assert_eq!(
        warn("ALL_PROXY", "socks5://u:p@proxy.example:1080").len(),
        1
    );
    assert!(warn("HTTPS_PROXY", "https://u:p@proxy.example:8443").is_empty());
    assert!(warn("HTTPS_PROXY", "http://proxy.example:3128").is_empty());
    assert!(warn("ALL_PROXY", "socks5://u:p@127.0.0.1:1080").is_empty());
    assert!(!warn("HTTPS_PROXY", "http://u:p@proxy.example:3128")[0].contains("u:p"));
}

#[test]
fn unknown_proxy_scheme_never_goes_direct() {
    let cfg = HostProxy::from_vars(|k| {
        (k == "HTTPS_PROXY").then(|| "socks4://10.9.9.9:1080".to_string())
    });
    assert!(matches!(cfg.https, Some(Upstream::Unsupported { .. })));
    let e = open_tunnel(&cfg, "93.184.215.14", 443)
        .err()
        .unwrap()
        .to_string();
    assert!(e.starts_with("refused") && e.contains("socks4"), "{e}");
}

// Network mode api against a local TLS server. The server records every
// byte it receives, so a test can check that a refused hello never left.

struct TlsEcho {
    port: u16,
    raw: Arc<Mutex<Vec<u8>>>,
}

fn tls_echo(ck: &rcgen::CertifiedKey<rcgen::KeyPair>) -> TlsEcho {
    use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
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
    let raw = Arc::new(Mutex::new(Vec::new()));
    let raw2 = raw.clone();
    std::thread::spawn(move || {
        for c in l.incoming().flatten() {
            let (scfg, raw) = (scfg.clone(), raw2.clone());
            std::thread::spawn(move || {
                let conn = rustls::ServerConnection::new(scfg).unwrap();
                let mut s = rustls::StreamOwned::new(conn, Recording { inner: c, raw });
                let mut buf = [0u8; 1024];
                while let Ok(n @ 1..) = s.read(&mut buf) {
                    if s.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
            });
        }
    });
    TlsEcho { port, raw }
}

fn api_proxy(
    entries: &[String],
    port: u16,
) -> (tempfile::TempDir, PathBuf, Arc<Mutex<Vec<String>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s2 = seen.clone();
    let (list, bad) = allow::Allowlist::parse(entries);
    assert!(bad.is_empty());
    let (dir, sock) = start(HostProxy {
        // The test-only override: the local server is on the host loopback,
        // which network mode api never reaches otherwise.
        allow_loopback: vec![port],
        api: Some(allow::ApiPolicy {
            list,
            refusals: Some(allow::Refusals::new(Arc::new(move |h: &str, why: &str| {
                s2.lock().unwrap().push(format!("{h}: {why}"));
            }))),
        }),
        ..HostProxy::default()
    });
    (dir, sock, seen)
}

fn tls_through(
    sock: &Path,
    target: &str,
    name: &str,
    cfg: Arc<rustls::ClientConfig>,
) -> std::io::Result<Vec<u8>> {
    let (code, s) = connect(sock, target);
    assert_eq!(code, 200);
    let name = rustls::pki_types::ServerName::try_from(name.to_string()).unwrap();
    let conn = rustls::ClientConnection::new(cfg, name).unwrap();
    let mut t = rustls::StreamOwned::new(conn, s);
    t.write_all(b"hello")?;
    let mut buf = [0u8; 5];
    t.read_exact(&mut buf)?;
    Ok(buf.to_vec())
}

use std::path::PathBuf;

#[test]
fn api_mode_lets_an_allowed_name_through() {
    let ck = self_signed();
    let srv = tls_echo(&ck);
    let (_d, sock, seen) = api_proxy(&[format!("localhost:{}", srv.port)], srv.port);
    let got = tls_through(
        &sock,
        &format!("localhost:{}", srv.port),
        "localhost",
        trusting(&ck),
    )
    .unwrap();
    assert_eq!(got, b"hello");
    assert!(seen.lock().unwrap().is_empty());
}

#[test]
fn api_mode_refuses_a_different_or_missing_server_name() {
    let ck = self_signed();
    let srv = tls_echo(&ck);
    let (_d, sock, seen) = api_proxy(&[format!("localhost:{}", srv.port)], srv.port);
    let target = format!("localhost:{}", srv.port);
    assert!(tls_through(&sock, &target, "other.example.com", trusting(&ck)).is_err());
    let mut no_sni = (*trusting(&ck)).clone();
    no_sni.enable_sni = false;
    assert!(tls_through(&sock, &target, "localhost", Arc::new(no_sni)).is_err());
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        srv.raw.lock().unwrap().is_empty(),
        "no byte reached the server"
    );
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1, "one line per host and minute: {seen:?}");
    assert!(
        seen[0].starts_with("localhost: the TLS server name differs"),
        "{seen:?}"
    );
}

#[test]
fn api_mode_refuses_other_hosts_ports_ip_literals_and_plain_http() {
    let ck = self_signed();
    let srv = tls_echo(&ck);
    let (_d, sock, seen) = api_proxy(&[format!("localhost:{}", srv.port)], srv.port);
    assert_eq!(connect(&sock, "elsewhere.example.com:443").0, 403);
    assert_eq!(connect(&sock, "localhost:443").0, 403);
    assert_eq!(connect(&sock, &format!("127.0.0.1:{}", srv.port)).0, 403);
    let (code, _) = request(
        &sock,
        &format!(
            "GET http://localhost:{}/ HTTP/1.1\r\nHost: localhost\r\n\r\n",
            srv.port
        ),
    );
    assert_eq!(code, 403);
    let seen = seen.lock().unwrap();
    assert!(
        seen.iter()
            .any(|l| l == "elsewhere.example.com: not on the list"),
        "{seen:?}"
    );
    assert!(
        seen.iter()
            .any(|l| l.starts_with("127.0.0.1: an IP address")),
        "{seen:?}"
    );
    assert!(srv.raw.lock().unwrap().is_empty());
}
