//! The network path for container backends.
//!
//! A container on a bridge network sends its packets through NAT and the
//! forwarding path of the host. Whether a system proxy or a TUN device sees
//! those packets depends on how the user configured the proxy. Accounts that
//! depend on a stable exit address must not be put at risk by that. So a
//! container gets no network at all, only a Unix socket to this proxy. The
//! proxy runs in the paddock process on the host, so every connection it makes
//! is an ordinary host connection: it uses the host routes, the TUN device, and
//! the host proxy variables, exactly as a program on the host would.
//!
//! Protocol work is done by small, widely used crates: `httparse` parses
//! requests and upstream responses, `url` parses proxy and request URLs,
//! `socks` speaks SOCKS5 to an upstream, and `no-proxy` evaluates NO_PROXY.
//! This module holds only the policy: which targets are refused, which host
//! loopback ports are allowed, limits, and the socket plumbing.

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use no_proxy::NoProxy;
use std::io::{Read, Write};
use std::net::{IpAddr, Shutdown, TcpListener, TcpStream, ToSocketAddrs};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use url::Url;

pub const RELAY_PORT: u16 = 3128;
pub const SOCKET_IN_SANDBOX: &str = "/run/paddock/proxy.sock";

pub const PROXY_VARS: &[&str] = &[
    "HTTPS_PROXY",
    "https_proxy",
    "HTTP_PROXY",
    "http_proxy",
    "ALL_PROXY",
    "all_proxy",
    "NO_PROXY",
    "no_proxy",
];

const MAX_HEAD: usize = 64 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const HEAD_TIMEOUT: Duration = Duration::from_secs(30);

fn parse_any(raw: &str) -> Option<Url> {
    Url::parse(raw)
        .ok()
        .filter(|u| u.has_host())
        .or_else(|| Url::parse(&format!("http://{raw}")).ok())
}

fn host_of(url: &Url) -> Option<String> {
    Some(
        url.host_str()?
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_string(),
    )
}

/// Parse a proxy URL into scheme, host and port. A value without a scheme,
/// such as `localhost:8080`, is read as HTTP.
pub fn parse_url(raw: &str) -> Option<(String, String, u16)> {
    let url = parse_any(raw)?;
    let scheme = url.scheme().to_ascii_lowercase();
    let default = if scheme.starts_with("socks") {
        1080
    } else {
        url.port_or_known_default().unwrap_or(80)
    };
    Some((scheme, host_of(&url)?, url.port().unwrap_or(default)))
}

pub fn is_loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// The proxy that the host uses.
#[derive(Debug, Clone)]
pub enum Upstream {
    Http {
        host: String,
        port: u16,
        auth: Option<String>,
    },
    Socks5 {
        host: String,
        port: u16,
        auth: Option<(String, String)>,
    },
}

fn decode(s: &str) -> String {
    url::form_urlencoded::parse(format!("x={s}").as_bytes())
        .next()
        .map(|(_, v)| v.into_owned())
        .unwrap_or_default()
}

fn upstream(raw: &str) -> Option<Upstream> {
    let url = parse_any(raw)?;
    let (_, host, port) = parse_url(raw)?;
    let user = decode(url.username());
    let pass = url.password().map(decode).unwrap_or_default();
    match url.scheme() {
        "http" | "https" => Some(Upstream::Http {
            host,
            port,
            auth: (!user.is_empty()).then(|| {
                base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"))
            }),
        }),
        "socks5" | "socks5h" | "socks" => Some(Upstream::Socks5 {
            host,
            port,
            auth: (!user.is_empty()).then_some((user, pass)),
        }),
        _ => None,
    }
}

#[derive(Debug, Clone)]
pub struct HostProxy {
    pub https: Option<Upstream>,
    pub http: Option<Upstream>,
    pub no_proxy: Option<String>,
    /// Host loopback ports that the sandbox can reach through the proxy.
    pub allow_loopback: Vec<u16>,
    /// A tunnel with no traffic in either direction for this long is closed.
    pub idle: Duration,
    /// Connections handled at the same time.
    pub max_conns: usize,
}

impl Default for HostProxy {
    fn default() -> Self {
        Self {
            https: None,
            http: None,
            no_proxy: None,
            allow_loopback: Vec::new(),
            idle: Duration::from_secs(15 * 60),
            max_conns: 512,
        }
    }
}

impl HostProxy {
    pub fn from_vars(get: impl Fn(&str) -> Option<String>) -> Self {
        let first = |names: &[&str]| names.iter().find_map(|n| get(n).filter(|v| !v.is_empty()));
        let all = first(&["ALL_PROXY", "all_proxy"]);
        let https = first(&["HTTPS_PROXY", "https_proxy"]).or_else(|| all.clone());
        let http = first(&["HTTP_PROXY", "http_proxy"]).or(all);
        Self {
            https: https.and_then(|u| upstream(&u)),
            http: http.and_then(|u| upstream(&u)),
            no_proxy: first(&["NO_PROXY", "no_proxy"]),
            ..Self::default()
        }
    }

    fn bypass(&self, host: &str) -> bool {
        self.no_proxy
            .as_deref()
            .is_some_and(|n| NoProxy::from(n).matches(host))
    }
}

/// Every address of a local interface. A connection to one of them reaches
/// services on this host, which the sandbox must not see.
pub fn local_addrs() -> Vec<IpAddr> {
    let mut out = Vec::new();
    unsafe {
        let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifap) != 0 {
            return out;
        }
        let mut cur = ifap;
        while !cur.is_null() {
            let a = (*cur).ifa_addr;
            if !a.is_null() {
                match (*a).sa_family as i32 {
                    libc::AF_INET => {
                        let sin = &*(a as *const libc::sockaddr_in);
                        out.push(IpAddr::from(
                            u32::from_be(sin.sin_addr.s_addr).to_be_bytes(),
                        ));
                    }
                    libc::AF_INET6 => {
                        let sin6 = &*(a as *const libc::sockaddr_in6);
                        out.push(IpAddr::from(sin6.sin6_addr.s6_addr));
                    }
                    _ => {}
                }
            }
            cur = (*cur).ifa_next;
        }
        libc::freeifaddrs(ifap);
    }
    out.sort();
    out.dedup();
    out
}

/// Refuse targets on this host, except allowed loopback ports.
fn check_target(cfg: &HostProxy, host: &str, port: u16) -> Result<()> {
    let addrs: Vec<IpAddr> = match (host, port).to_socket_addrs() {
        Ok(it) => it.map(|a| a.ip()).collect(),
        // Names that only the upstream proxy can resolve are not local.
        Err(_) => return Ok(()),
    };
    let local = local_addrs();
    for ip in addrs {
        if ip.is_loopback() {
            if !cfg.allow_loopback.contains(&port) {
                bail!("refused: {host}:{port} is a host loopback port that is not allowed");
            }
        } else if ip.is_unspecified() || local.contains(&ip) {
            bail!("refused: {host}:{port} is an address of this host");
        }
    }
    Ok(())
}

fn connect_tcp(host: &str, port: u16) -> Result<TcpStream> {
    let mut last = None;
    for addr in (host, port)
        .to_socket_addrs()
        .with_context(|| format!("cannot resolve {host}"))?
    {
        match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
            Ok(s) => return Ok(s),
            Err(e) => last = Some(e),
        }
    }
    Err(last
        .map(Into::into)
        .unwrap_or_else(|| anyhow!("no address for {host}")))
}

/// Read an HTTP head (up to the empty line). Returns the head and any bytes
/// that arrived after it.
fn read_head(s: &mut impl Read) -> Result<(Vec<u8>, Vec<u8>)> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let rest = buf.split_off(i + 4);
            return Ok((buf, rest));
        }
        if buf.len() > MAX_HEAD {
            bail!("HTTP head too large");
        }
        let n = s.read(&mut chunk)?;
        if n == 0 {
            bail!("connection closed before the HTTP head ended");
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

fn authority(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// Open a tunnel to host:port the way a host program would.
fn open_tunnel(cfg: &HostProxy, host: &str, port: u16) -> Result<TcpStream> {
    check_target(cfg, host, port)?;
    let up = if cfg.bypass(host) || is_loopback(host) {
        None
    } else {
        cfg.https.as_ref()
    };
    match up {
        None => connect_tcp(host, port),
        Some(Upstream::Http {
            host: ph,
            port: pp,
            auth,
        }) => {
            let mut s = connect_tcp(ph, *pp)?;
            let target = authority(host, port);
            let mut req = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n");
            if let Some(a) = auth {
                req.push_str(&format!("Proxy-Authorization: Basic {a}\r\n"));
            }
            req.push_str("\r\n");
            s.write_all(req.as_bytes())?;
            s.set_read_timeout(Some(HEAD_TIMEOUT))?;
            let (head, rest) = read_head(&mut s)?;
            let mut headers = [httparse::EMPTY_HEADER; 64];
            let mut resp = httparse::Response::new(&mut headers);
            resp.parse(&head)?;
            match resp.code {
                Some(c) if (200..300).contains(&c) => {}
                other => bail!("upstream proxy refused CONNECT {target}: status {other:?}"),
            }
            if !rest.is_empty() {
                bail!("upstream proxy sent data before the tunnel opened");
            }
            s.set_read_timeout(None)?;
            Ok(s)
        }
        Some(Upstream::Socks5 {
            host: ph,
            port: pp,
            auth,
        }) => {
            let proxy = (ph.as_str(), *pp);
            let s = match auth {
                Some((u, p)) => {
                    socks::Socks5Stream::connect_with_password(proxy, (host, port), u, p)
                }
                None => socks::Socks5Stream::connect(proxy, (host, port)),
            }
            .with_context(|| format!("upstream SOCKS proxy could not reach {host}:{port}"))?;
            Ok(s.into_inner())
        }
    }
}

trait Duplex: Read + Write + Send + 'static {
    fn dup(&self) -> std::io::Result<Box<dyn Duplex>>;
    fn close_write(&self);
    fn set_tick(&self, d: Duration) -> std::io::Result<()>;
}

impl Duplex for TcpStream {
    fn dup(&self) -> std::io::Result<Box<dyn Duplex>> {
        Ok(Box::new(self.try_clone()?))
    }
    fn close_write(&self) {
        let _ = self.shutdown(Shutdown::Write);
    }
    fn set_tick(&self, d: Duration) -> std::io::Result<()> {
        self.set_read_timeout(Some(d))
    }
}

impl Duplex for UnixStream {
    fn dup(&self) -> std::io::Result<Box<dyn Duplex>> {
        Ok(Box::new(self.try_clone()?))
    }
    fn close_write(&self) {
        let _ = self.shutdown(Shutdown::Write);
    }
    fn set_tick(&self, d: Duration) -> std::io::Result<()> {
        self.set_read_timeout(Some(d))
    }
}

fn pump(
    mut from: Box<dyn Duplex>,
    mut to: Box<dyn Duplex>,
    last: Arc<Mutex<Instant>>,
    idle: Duration,
) {
    let mut buf = [0u8; 16 * 1024];
    loop {
        match from.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if to.write_all(&buf[..n]).is_err() {
                    break;
                }
                *last.lock().unwrap() = Instant::now();
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                if last.lock().unwrap().elapsed() >= idle {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    to.close_write();
}

/// Copy both directions until one side closes, or until the tunnel has no
/// traffic in either direction for `idle`. Long-lived connections such as
/// WebSockets stay open while they carry traffic.
fn splice(a: impl Duplex, b: impl Duplex, idle: Duration) {
    let tick = Duration::from_millis(200).min(idle);
    let _ = a.set_tick(tick);
    let _ = b.set_tick(tick);
    let (Ok(a_read), Ok(a_write), Ok(b_read), Ok(b_write)) = (a.dup(), a.dup(), b.dup(), b.dup())
    else {
        return;
    };
    let last = Arc::new(Mutex::new(Instant::now()));
    let l2 = last.clone();
    let t = std::thread::spawn(move || pump(a_read, b_write, l2, idle));
    pump(b_read, a_write, last, idle);
    let _ = t.join();
}

fn respond(client: &mut impl Write, code: u16, reason: &str) {
    let _ = write!(
        client,
        "HTTP/1.1 {code} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
}

fn fail(client: &mut impl Write, e: &anyhow::Error) {
    if e.to_string().starts_with("refused") {
        respond(client, 403, "Forbidden");
    } else {
        respond(client, 502, "Bad Gateway");
    }
}

/// Hop-by-hop headers that a proxy must not forward.
const HOP: &[&str] = &[
    "proxy-connection",
    "proxy-authorization",
    "connection",
    "keep-alive",
    "te",
    "trailer",
];

fn handle(cfg: &HostProxy, mut client: UnixStream) -> Result<()> {
    client.set_read_timeout(Some(HEAD_TIMEOUT))?;
    let (head, rest) = read_head(&mut client)?;
    let mut headers = [httparse::EMPTY_HEADER; 100];
    let mut req = httparse::Request::new(&mut headers);
    if req.parse(&head)?.is_partial() {
        respond(&mut client, 400, "Bad Request");
        bail!("incomplete request head");
    }
    let method = req.method.unwrap_or("").to_string();
    let target = req.path.unwrap_or("").to_string();
    client.set_read_timeout(None)?;

    if method.eq_ignore_ascii_case("CONNECT") {
        let parsed = Url::parse(&format!("http://{target}")).ok();
        let (Some(host), Some(port)) = (
            parsed.as_ref().and_then(host_of),
            parsed.as_ref().and_then(Url::port),
        ) else {
            respond(&mut client, 400, "Bad Request");
            bail!("bad CONNECT target {target}");
        };
        let mut up = match open_tunnel(cfg, &host, port) {
            Ok(up) => up,
            Err(e) => {
                fail(&mut client, &e);
                return Err(e);
            }
        };
        client.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")?;
        if !rest.is_empty() {
            up.write_all(&rest)?;
        }
        splice(client, up, cfg.idle);
        return Ok(());
    }

    // A plain HTTP request in absolute form.
    let url = match Url::parse(&target) {
        Ok(u) if u.scheme() == "http" && u.has_host() => u,
        _ => {
            respond(&mut client, 400, "Bad Request");
            bail!("unsupported request target {target}");
        }
    };
    let host = host_of(&url).unwrap_or_default();
    let port = url.port_or_known_default().unwrap_or(80);
    let via = if cfg.bypass(&host) || is_loopback(&host) {
        None
    } else {
        cfg.http.clone()
    };
    let version = req.version.unwrap_or(1);
    let opened = match &via {
        Some(Upstream::Http {
            host: ph, port: pp, ..
        }) => check_target(cfg, &host, port)
            .and_then(|_| connect_tcp(ph, *pp))
            .map(|s| (s, format!("{method} {url} HTTP/1.{version}\r\n"))),
        _ => {
            let tunnel_cfg = HostProxy {
                https: via.clone(),
                ..cfg.clone()
            };
            let mut path = url.path().to_string();
            if let Some(q) = url.query() {
                path.push('?');
                path.push_str(q);
            }
            open_tunnel(&tunnel_cfg, &host, port)
                .map(|s| (s, format!("{method} {path} HTTP/1.{version}\r\n")))
        }
    };
    let (mut up, line) = match opened {
        Ok(x) => x,
        Err(e) => {
            fail(&mut client, &e);
            return Err(e);
        }
    };
    let mut out = line.into_bytes();
    for h in req.headers.iter() {
        if HOP.contains(&h.name.to_ascii_lowercase().as_str()) {
            continue;
        }
        out.extend_from_slice(h.name.as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(h.value);
        out.extend_from_slice(b"\r\n");
    }
    if let Some(Upstream::Http { auth: Some(a), .. }) = &via {
        out.extend_from_slice(format!("Proxy-Authorization: Basic {a}\r\n").as_bytes());
    }
    out.extend_from_slice(b"Connection: close\r\n\r\n");
    up.write_all(&out)?;
    if !rest.is_empty() {
        up.write_all(&rest)?;
    }
    splice(client, up, cfg.idle);
    Ok(())
}

/// Serve the proxy on a Unix socket until the process exits.
pub fn serve(socket: &Path, cfg: HostProxy) -> Result<()> {
    let _ = std::fs::remove_file(socket);
    if let Some(d) = socket.parent() {
        std::fs::create_dir_all(d)?;
    }
    let listener = UnixListener::bind(socket)
        .with_context(|| format!("cannot listen on {}", socket.display()))?;
    let active = Arc::new(AtomicUsize::new(0));
    std::thread::spawn(move || {
        for mut conn in listener.incoming().flatten() {
            if active.load(Ordering::SeqCst) >= cfg.max_conns {
                respond(&mut conn, 503, "Service Unavailable");
                continue;
            }
            active.fetch_add(1, Ordering::SeqCst);
            let cfg = cfg.clone();
            let active = active.clone();
            std::thread::spawn(move || {
                let _ = handle(&cfg, conn);
                active.fetch_sub(1, Ordering::SeqCst);
            });
        }
    });
    Ok(())
}

/// `paddock relay`: inside the container, pass TCP connections on 127.0.0.1
/// to the host proxy socket.
pub fn relay(listen: &str, socket: &Path, forward: &[u16], ready: Option<&Path>) -> Result<()> {
    let idle = HostProxy::default().idle;
    // Allowed host loopback ports appear on the sandbox loopback as well.
    for &port in forward {
        let l = TcpListener::bind(("127.0.0.1", port))
            .with_context(|| format!("cannot listen on port {port}"))?;
        let socket = socket.to_path_buf();
        std::thread::spawn(move || {
            for conn in l.incoming().flatten() {
                let socket = socket.clone();
                std::thread::spawn(move || {
                    let Ok(mut up) = UnixStream::connect(&socket) else {
                        return;
                    };
                    let t = authority("127.0.0.1", port);
                    if write!(up, "CONNECT {t} HTTP/1.1\r\nHost: {t}\r\n\r\n").is_err() {
                        return;
                    }
                    let Ok((head, rest)) = read_head(&mut up) else {
                        return;
                    };
                    let mut headers = [httparse::EMPTY_HEADER; 16];
                    let mut resp = httparse::Response::new(&mut headers);
                    if resp.parse(&head).is_err() || resp.code != Some(200) || !rest.is_empty() {
                        return;
                    }
                    splice(conn, up, idle);
                });
            }
        });
    }
    let listener =
        TcpListener::bind(listen).with_context(|| format!("cannot listen on {listen}"))?;
    if let Some(r) = ready {
        std::fs::write(r, "ready")?;
    }
    for conn in listener.incoming().flatten() {
        let socket = socket.to_path_buf();
        std::thread::spawn(move || {
            if let Ok(up) = UnixStream::connect(&socket) {
                splice(conn, up, idle);
            }
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests;
