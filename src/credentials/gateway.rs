//! The credential gateway (`via = "gateway"`): an HTTP server on a Unix
//! socket of one sandbox. Inside, `paddock relay` makes it reachable on
//! `127.0.0.1:PORT`, and the tool's base URL points at
//! `http://127.0.0.1:PORT/s/NAME`. The sandbox holds only a stand-in.
//!
//! For each request, paddock removes every credential that the client sent,
//! adds the real value, and sends the request over HTTPS to the host that
//! the credential is bound to, from the host, through the host's proxy
//! settings, with the system's certificates. No TLS is decrypted and no CA
//! is made. A request outside the bound paths, or after the user revoked
//! the credential, gets 403 and nothing leaves the host. The response
//! streams back as it comes, so server-sent events work.
//!
//! One request per connection: the gateway asks the upstream to close after
//! the response, and tells the client the same.

use super::secret::Secret;
use super::Credential;
use crate::env::HostEnv;
use anyhow::{bail, Context as _, Result};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The loopback port inside the sandbox.
pub const PORT: u16 = 3130;
/// Where the socket appears inside the sandbox.
pub const SOCKET_IN_SANDBOX: &str = "/run/paddock/credentials.sock";
const MAX_HEAD: usize = 64 * 1024;
const MAX_CONNECTIONS: usize = 32;
/// Refused requests in one run before paddock freezes the sandbox.
pub const MAX_REFUSED: usize = 20;

/// Headers that carry credentials or control the connection. The gateway
/// never forwards them from the sandbox.
const DROPPED: &[&str] = &[
    "host",
    "connection",
    "keep-alive",
    "proxy-connection",
    "proxy-authorization",
    "authorization",
    "x-api-key",
    "x-goog-api-key",
    "api-key",
    "cookie",
    "upgrade",
    "te",
    "trailer",
    // The gateway sends the whole body before it reads the response.
    "expect",
];

pub trait Stream: Read + Write + Send {}
impl<T: Read + Write + Send> Stream for T {}

/// Opens a TLS connection to an upstream host on port 443.
pub type Connector = Arc<dyn Fn(&str) -> Result<Box<dyn Stream>> + Send + Sync>;

/// The real connector: from the host, through the host's proxy settings,
/// TLS checked against the system's certificates. The hosts come from the
/// user configuration, never from the sandbox, so a host on the LAN or on
/// the host's loopback (with its port named) is reachable here.
pub fn host_connector(env: &HostEnv, hosts: &[String]) -> Connector {
    let mut proxy = crate::proxy::HostProxy::from_vars(|k| env.var(k).map(str::to_string));
    proxy.allow_lan = true;
    proxy.allow_loopback = hosts
        .iter()
        .filter_map(|h| split_host(h).ok())
        .filter(|(_, p)| *p != 443)
        .map(|(_, p)| p)
        .collect();
    Arc::new(move |bound: &str| -> Result<Box<dyn Stream>> {
        let (host, port) = split_host(bound)?;
        let tcp = crate::proxy::host_connect(&proxy, host, port, Duration::from_secs(30))?;
        let cfg = crate::proxy::tls::system_config()?;
        let name = rustls::pki_types::ServerName::try_from(host.to_string())?;
        let conn = rustls::ClientConnection::new(cfg, name)?;
        Ok(Box::new(rustls::StreamOwned::new(conn, tcp)))
    })
}

/// `HOST` or `HOST:PORT`; the port defaults to 443.
pub fn split_host(bound: &str) -> Result<(&str, u16)> {
    Ok(match bound.split_once(':') {
        Some((h, p)) => (h, p.parse().context("bad port")?),
        None => (bound, 443),
    })
}

/// One credential behind the gateway.
pub struct Route {
    pub host: String,
    pub paths: Vec<String>,
    pub header: String,
    pub prefix: String,
    pub query: Option<String>,
    /// Locked in RAM and wiped when the route goes (secret.rs).
    pub value: Secret,
    /// git smart HTTP: `info/refs` passes only for a service whose own path
    /// is bound.
    pub git: bool,
}

impl Route {
    pub fn new(c: &Credential, value: Secret) -> Self {
        let header = c.header.clone().unwrap_or_else(|| "Authorization".into());
        let value = match &c.basic_user {
            Some(u) => {
                use base64::Engine as _;
                let pair = Secret::from(format!("{u}:{}", value.as_str()));
                Secret::from(base64::engine::general_purpose::STANDARD.encode(pair.as_bytes()))
            }
            None => value,
        };
        let prefix = c.prefix.clone().unwrap_or_else(|| {
            if c.basic_user.is_some() {
                "Basic ".into()
            } else if header.eq_ignore_ascii_case("authorization") {
                "Bearer ".into()
            } else {
                String::new()
            }
        });
        Route {
            host: c.hosts[0].clone(),
            paths: c.paths.clone(),
            header,
            prefix,
            query: c.query.clone(),
            value,
            git: c.is_git(),
        }
    }

    /// Whether the route lets a request to `path` with `query` through.
    pub fn allows(&self, path: &str, query: Option<&str>) -> bool {
        let bound =
            |p: &str| self.paths.is_empty() || self.paths.iter().any(|b| path_matches(b, p));
        if !bound(path) {
            return false;
        }
        if !self.git {
            return true;
        }
        // `GET .../info/refs?service=git-upload-pack` opens a fetch, and
        // `git-receive-pack` a push: each needs its own path bound.
        match path.strip_suffix("/info/refs") {
            Some(repo) => query
                .unwrap_or("")
                .split('&')
                .find_map(|kv| kv.strip_prefix("service="))
                .is_some_and(|s| {
                    matches!(s, "git-upload-pack" | "git-receive-pack")
                        && bound(&format!("{repo}/{s}"))
                }),
            None => true,
        }
    }
}

/// A bound path: a prefix, or with `*` a whole path where each `*` stands
/// for text inside one segment (`/*/*/info/refs`).
pub fn path_matches(bound: &str, path: &str) -> bool {
    if !bound.contains('*') {
        return path.starts_with(bound);
    }
    // No escaped slashes or dots behind a wildcard, and no `.` or `..`
    // segment, which a server can resolve to another path.
    if path.contains('%') || path.split('/').any(|s| s == "." || s == "..") {
        return false;
    }
    let (b, p): (Vec<&str>, Vec<&str>) = (bound.split('/').collect(), path.split('/').collect());
    b.len() == p.len() && b.iter().zip(&p).all(|(b, p)| glob(b, p))
}

/// `*` matches any text, at least one character when it is the whole
/// pattern.
fn glob(pat: &str, s: &str) -> bool {
    if pat == "*" {
        return !s.is_empty();
    }
    let mut parts = pat.split('*');
    let first = parts.next().unwrap_or("");
    let Some(mut rest) = s.strip_prefix(first) else {
        return false;
    };
    let parts: Vec<&str> = parts.collect();
    let Some((last, mid)) = parts.split_last() else {
        return rest.is_empty();
    };
    for m in mid {
        match rest.find(m) {
            Some(i) => rest = &rest[i + m.len()..],
            None => return false,
        }
    }
    rest.len() >= last.len() && rest.ends_with(last)
}

/// What the gateway reports for each request: the name, the method, the
/// host, the path without its query, and the result. Never a value.
pub type Report = Arc<dyn Fn(&str, &str, &str, &str, &str) + Send + Sync>;

pub struct Gateway {
    /// A revoked route leaves the map, and the end of the sandbox clears
    /// it: the value is wiped when the last request with it ends.
    routes: Mutex<BTreeMap<String, Arc<Route>>>,
    pub connect: Connector,
    /// A file whose lines name revoked credentials. It is outside the
    /// sandbox; `paddock credentials revoke` writes it.
    pub revoked: PathBuf,
    pub report: Report,
    /// Called once when too many requests were refused.
    pub on_abuse: Arc<dyn Fn() + Send + Sync>,
    refused: AtomicUsize,
    connections: AtomicUsize,
}

impl Gateway {
    pub fn new(
        routes: BTreeMap<String, Route>,
        connect: Connector,
        revoked: PathBuf,
        report: Report,
        on_abuse: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        Self {
            routes: Mutex::new(routes.into_iter().map(|(k, v)| (k, Arc::new(v))).collect()),
            connect,
            revoked,
            report,
            on_abuse,
            refused: AtomicUsize::new(0),
            connections: AtomicUsize::new(0),
        }
    }

    pub fn is_revoked(&self, name: &str) -> bool {
        std::fs::read_to_string(&self.revoked)
            .map(|t| t.lines().any(|l| l.trim() == name || l.trim() == "*"))
            .unwrap_or(false)
    }

    /// Drop the routes that the revoke file names.
    pub fn drop_revoked(&self) {
        let names: Vec<String> = self.routes.lock().unwrap().keys().cloned().collect();
        for n in names {
            if self.is_revoked(&n) {
                self.routes.lock().unwrap().remove(&n);
            }
        }
    }

    /// Drop every route: the sandbox ended.
    pub fn clear(&self) {
        self.routes.lock().unwrap().clear();
    }

    /// Serve on `socket` until the process ends. The returned handle can
    /// clear the routes.
    pub fn serve(self, socket: &Path) -> Result<Arc<Self>> {
        let _ = std::fs::remove_file(socket);
        let listener = UnixListener::bind(socket)
            .with_context(|| format!("cannot listen on {}", socket.display()))?;
        let me = Arc::new(self);
        let handle = me.clone();
        let sweep = Arc::downgrade(&me);
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_secs(1));
            match sweep.upgrade() {
                Some(g) => g.drop_revoked(),
                None => return,
            }
        });
        std::thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                if me.connections.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
                    me.connections.fetch_sub(1, Ordering::SeqCst);
                    let mut c = conn;
                    let _ = reply(&mut c, 503, "too many connections");
                    continue;
                }
                let g = me.clone();
                std::thread::spawn(move || {
                    if let Err(e) = g.handle(conn) {
                        log::debug!("credential gateway: {e:#}");
                    }
                    g.connections.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });
        Ok(handle)
    }

    fn refuse(
        &self,
        conn: &mut UnixStream,
        code: u16,
        why: &str,
        name: &str,
        method: &str,
        path: &str,
    ) -> Result<()> {
        (self.report)(name, method, "", path, &format!("refused: {why}"));
        if self.refused.fetch_add(1, Ordering::SeqCst) + 1 == MAX_REFUSED {
            (self.on_abuse)();
        }
        reply(conn, code, why)
    }

    /// One request.
    pub fn handle(&self, mut conn: UnixStream) -> Result<()> {
        conn.set_read_timeout(Some(Duration::from_secs(300)))?;
        let mut reader = BufReader::new(conn.try_clone()?);
        let head = read_head(&mut reader)?;
        let mut headers = [httparse::EMPTY_HEADER; 96];
        let mut req = httparse::Request::new(&mut headers);
        if !req.parse(&head)?.is_complete() {
            bail!("incomplete request head");
        }
        let method = req.method.unwrap_or("").to_string();
        let target = req.path.unwrap_or("").to_string();
        let (path, query) = match target.split_once('?') {
            Some((p, q)) => (p.to_string(), Some(q.to_string())),
            None => (target.clone(), None),
        };
        let Some(rest) = path.strip_prefix("/s/") else {
            return self.refuse(&mut conn, 404, "not a credential path", "", &method, &path);
        };
        let (name, upath) = match rest.find('/') {
            Some(i) => (&rest[..i], rest[i..].to_string()),
            None => (rest, "/".to_string()),
        };
        if self.is_revoked(name) {
            self.routes.lock().unwrap().remove(name);
            return self.refuse(&mut conn, 403, "revoked", name, &method, &upath);
        }
        let Some(route) = self.routes.lock().unwrap().get(name).cloned() else {
            return self.refuse(&mut conn, 404, "no such credential", name, &method, &upath);
        };
        let route = &*route;
        if upath.split('/').any(|seg| seg == ".." || seg == ".") {
            return self.refuse(
                &mut conn,
                403,
                "path with dot segments",
                name,
                &method,
                &upath,
            );
        }
        if !route.allows(&upath, query.as_deref()) {
            return self.refuse(&mut conn, 403, "path not bound", name, &method, &upath);
        }
        // The body: a length, or chunks.
        let mut length: Option<u64> = None;
        let mut chunked = false;
        let mut out = format!("{method} {}", upath);
        let q = rewrite_query(query.as_deref(), route);
        if !q.is_empty() {
            out.push('?');
            out.push_str(&q);
        }
        out.push_str(&format!(" HTTP/1.1\r\nHost: {}\r\n", route.host));
        for h in req.headers.iter() {
            let n = h.name.to_ascii_lowercase();
            if n == "content-length" {
                length = Some(
                    String::from_utf8_lossy(h.value)
                        .trim()
                        .parse()
                        .context("bad Content-Length")?,
                );
            }
            if n == "transfer-encoding" {
                chunked = String::from_utf8_lossy(h.value)
                    .to_ascii_lowercase()
                    .contains("chunked");
            }
            if DROPPED.contains(&n.as_str()) || n.eq_ignore_ascii_case(&route.header) {
                continue;
            }
            out.push_str(h.name);
            out.push_str(": ");
            out.push_str(&String::from_utf8_lossy(h.value));
            out.push_str("\r\n");
        }
        if route.query.is_none() {
            out.push_str(&format!(
                "{}: {}{}\r\n",
                route.header,
                route.prefix,
                route.value.as_str()
            ));
        }
        out.push_str("Connection: close\r\n\r\n");

        let mut up = match (self.connect)(&route.host) {
            Ok(u) => u,
            Err(e) => {
                (self.report)(
                    name,
                    &method,
                    &route.host,
                    &upath,
                    &format!("failed: {e:#}"),
                );
                return reply(&mut conn, 502, "cannot reach the bound host");
            }
        };
        up.write_all(out.as_bytes())?;
        if chunked {
            copy_chunked(&mut reader, &mut up)?;
        } else if let Some(n) = length {
            let copied = std::io::copy(&mut (&mut reader).take(n), &mut up)?;
            if copied != n {
                bail!("the client sent {copied} of {n} body bytes");
            }
        }
        up.flush()?;
        // The response: the head without connection headers, then the body
        // as it comes.
        let mut upr = BufReader::new(up);
        let rhead = read_head(&mut upr)?;
        let mut rh = [httparse::EMPTY_HEADER; 96];
        let mut resp = httparse::Response::new(&mut rh);
        if !resp.parse(&rhead)?.is_complete() {
            bail!("incomplete response head from {}", route.host);
        }
        let code = resp.code.unwrap_or(502);
        let mut back = format!("HTTP/1.1 {code} {}\r\n", resp.reason.unwrap_or(""));
        for h in resp.headers.iter() {
            let n = h.name.to_ascii_lowercase();
            if matches!(n.as_str(), "connection" | "keep-alive" | "proxy-connection") {
                continue;
            }
            back.push_str(h.name);
            back.push_str(": ");
            back.push_str(&String::from_utf8_lossy(h.value));
            back.push_str("\r\n");
        }
        back.push_str("Connection: close\r\n\r\n");
        conn.write_all(back.as_bytes())?;
        (self.report)(
            name,
            &method,
            &route.host,
            &upath,
            &format!("forwarded {code}"),
        );
        let mut buf = [0u8; 16 * 1024];
        loop {
            let n = match upr.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                // A TLS peer that closes without close_notify.
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e.into()),
            };
            conn.write_all(&buf[..n])?;
            conn.flush()?;
        }
        let _ = conn.shutdown(std::net::Shutdown::Both);
        Ok(())
    }
}

/// The query without any value of the credential's parameter, and with the
/// real one when the credential goes in the query.
fn rewrite_query(q: Option<&str>, route: &Route) -> String {
    let mut parts: Vec<String> = q
        .unwrap_or("")
        .split('&')
        .filter(|p| !p.is_empty())
        .filter(|p| {
            let k = p.split('=').next().unwrap_or("");
            route.query.as_deref() != Some(k)
        })
        .map(str::to_string)
        .collect();
    if let Some(k) = &route.query {
        parts.push(format!("{k}={}", percent(route.value.as_str())));
    }
    parts.join("&")
}

fn percent(v: &str) -> String {
    v.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

fn read_head(r: &mut impl BufRead) -> Result<Vec<u8>> {
    let mut head = Vec::new();
    loop {
        let mut line = Vec::new();
        let n = r.read_until(b'\n', &mut line)?;
        if n == 0 {
            bail!("the connection closed before the head ended");
        }
        head.extend_from_slice(&line);
        if head.len() > MAX_HEAD {
            bail!("the head is larger than {MAX_HEAD} bytes");
        }
        if line == b"\r\n" || line == b"\n" {
            return Ok(head);
        }
    }
}

/// Copy a chunked body as it is, up to and including its last chunk and
/// trailers, and no further.
fn copy_chunked(r: &mut impl BufRead, w: &mut impl Write) -> Result<()> {
    loop {
        let mut line = Vec::new();
        if r.read_until(b'\n', &mut line)? == 0 {
            bail!("the chunked body ended early");
        }
        w.write_all(&line)?;
        let size_text = String::from_utf8_lossy(&line);
        let size = u64::from_str_radix(size_text.trim().split(';').next().unwrap_or("").trim(), 16)
            .context("bad chunk size")?;
        if size == 0 {
            // Trailers, then an empty line.
            loop {
                let mut t = Vec::new();
                if r.read_until(b'\n', &mut t)? == 0 {
                    bail!("the chunked body ended early");
                }
                w.write_all(&t)?;
                if t == b"\r\n" || t == b"\n" {
                    return Ok(());
                }
            }
        }
        let copied = std::io::copy(&mut r.take(size + 2), w)?;
        if copied != size + 2 {
            bail!("the chunked body ended early");
        }
    }
}

fn reply(conn: &mut UnixStream, code: u16, why: &str) -> Result<()> {
    let body = format!("paddock credential gateway: {why}\n");
    let reason = match code {
        403 => "Forbidden",
        404 => "Not Found",
        502 => "Bad Gateway",
        _ => "Service Unavailable",
    };
    write!(
        conn,
        "HTTP/1.1 {code} {reason}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    let _ = conn.shutdown(std::net::Shutdown::Both);
    Ok(())
}

/// A stand-in for a value: the same start (up to the first `_` or `-` in
/// the first 12 characters) and the same length, so that local format
/// checks pass, with a mark in the middle that tells which sandbox and
/// which credential it came from. It grants nothing.
pub fn stand_in(real: &str, mark: &str) -> String {
    let start = real
        .char_indices()
        .take(12)
        .filter(|(_, c)| *c == '_' || *c == '-')
        .map(|(i, _)| i + 1)
        .next()
        .unwrap_or(0);
    let keep = &real[..start];
    let want = real.chars().count().max(keep.len() + mark.len() + 8);
    let mut s = format!("{keep}PADDOCK{mark}");
    while s.chars().count() < want {
        s.push('0');
    }
    s.chars().take(want).collect()
}

/// A random mark for a stand-in: 12 hex digits.
pub fn new_mark() -> String {
    let mut b = [0u8; 6];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut b);
    }
    crate::manifest::hex(&b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::Mutex;

    /// A fake HTTPS upstream for `localhost`, with its own certificate. It
    /// records each request (head and body) and answers by path: `/sse`
    /// streams three events, everything else gets "ok".
    struct Upstream {
        port: u16,
        seen: Arc<Mutex<Vec<String>>>,
        client: Arc<rustls::ClientConfig>,
    }

    fn upstream() -> Upstream {
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
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen2 = seen.clone();
        std::thread::spawn(move || {
            for c in l.incoming().flatten() {
                let (scfg, seen) = (scfg.clone(), seen2.clone());
                std::thread::spawn(move || {
                    let conn = rustls::ServerConnection::new(scfg).unwrap();
                    let s = rustls::StreamOwned::new(conn, c);
                    let mut r = BufReader::new(s);
                    let Ok(head) = read_head(&mut r) else { return };
                    let head = String::from_utf8_lossy(&head).into_owned();
                    let mut body = Vec::new();
                    let lower = head.to_ascii_lowercase();
                    if let Some(l) = lower
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                    {
                        let n: u64 = l.trim().parse().unwrap();
                        (&mut r).take(n).read_to_end(&mut body).unwrap();
                    } else if lower.contains("transfer-encoding: chunked") {
                        copy_chunked(&mut r, &mut body).unwrap();
                    }
                    seen.lock()
                        .unwrap()
                        .push(format!("{head}{}", String::from_utf8_lossy(&body)));
                    let s = r.get_mut();
                    if head.starts_with("GET /sse") {
                        s.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: keep-alive\r\n\r\n").unwrap();
                        for i in 0..3 {
                            s.write_all(format!("data: {i}\n\n").as_bytes()).unwrap();
                            s.flush().unwrap();
                            std::thread::sleep(Duration::from_millis(300));
                        }
                    } else {
                        s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                            .unwrap();
                    }
                    s.conn.send_close_notify();
                    let _ = s.flush();
                });
            }
        });
        Upstream { port, seen, client }
    }

    struct World {
        up: Upstream,
        g: Arc<Gateway>,
        log: Arc<Mutex<Vec<String>>>,
        abuse: Arc<AtomicUsize>,
        revoked: tempfile::NamedTempFile,
    }

    fn world(paths: &[&str], query: Option<&str>) -> World {
        let up = upstream();
        let host = format!("localhost:{}", up.port);
        let client = up.client.clone();
        let connect: Connector = Arc::new(move |bound: &str| -> Result<Box<dyn Stream>> {
            let (h, p) = split_host(bound)?;
            let tcp = std::net::TcpStream::connect(("127.0.0.1", p))?;
            let name = rustls::pki_types::ServerName::try_from(h.to_string())?;
            let conn = rustls::ClientConnection::new(client.clone(), name)?;
            Ok(Box::new(rustls::StreamOwned::new(conn, tcp)))
        });
        let mut routes = BTreeMap::new();
        routes.insert(
            "GH".to_string(),
            Route {
                host,
                paths: paths.iter().map(|p| p.to_string()).collect(),
                header: "Authorization".into(),
                prefix: "Bearer ".into(),
                query: query.map(str::to_string),
                value: "ghp_REALvalue".into(),
                git: false,
            },
        );
        let log = Arc::new(Mutex::new(Vec::new()));
        let log2 = log.clone();
        let abuse = Arc::new(AtomicUsize::new(0));
        let abuse2 = abuse.clone();
        let revoked = tempfile::NamedTempFile::new().unwrap();
        let g = Gateway::new(
            routes,
            connect,
            revoked.path().to_path_buf(),
            Arc::new(move |n: &str, m: &str, h: &str, p: &str, r: &str| {
                log2.lock().unwrap().push(format!("{n} {m} {h}{p} {r}"));
            }),
            Arc::new(move || {
                abuse2.fetch_add(1, Ordering::SeqCst);
            }),
        );
        World {
            up,
            g: Arc::new(g),
            log,
            abuse,
            revoked,
        }
    }

    /// Send one raw request through the gateway and return the response.
    fn send(w: &World, req: &str) -> String {
        let (mut a, b) = UnixStream::pair().unwrap();
        let g = w.g.clone();
        let t = std::thread::spawn(move || g.handle(b));
        a.write_all(req.as_bytes()).unwrap();
        let mut out = String::new();
        a.read_to_string(&mut out).unwrap();
        let _ = t.join();
        out
    }

    #[test]
    fn the_bound_host_gets_the_real_value_and_nothing_the_sandbox_sent() {
        let w = world(&["/repos/"], None);
        let r = send(
            &w,
            "POST /s/GH/repos/a/b?x=1 HTTP/1.1\r\nHost: 127.0.0.1:3130\r\nAuthorization: Bearer ghp_PADDOCKfake\r\nX-Api-Key: other\r\nCookie: c=1\r\nConnection: keep-alive\r\nContent-Length: 5\r\nAccept: */*\r\n\r\nhello",
        );
        assert!(r.starts_with("HTTP/1.1 200"), "{r}");
        assert!(r.ends_with("ok") && r.contains("Connection: close"), "{r}");
        let seen = w.up.seen.lock().unwrap().join("\n");
        assert!(
            seen.starts_with("POST /repos/a/b?x=1 HTTP/1.1\r\n"),
            "{seen}"
        );
        assert!(
            seen.contains(&format!("Host: localhost:{}", w.up.port)),
            "{seen}"
        );
        assert!(
            seen.contains("Authorization: Bearer ghp_REALvalue"),
            "{seen}"
        );
        assert!(
            seen.contains("Accept: */*") && seen.ends_with("hello"),
            "{seen}"
        );
        for gone in [
            "PADDOCKfake",
            "X-Api-Key",
            "Cookie",
            "keep-alive",
            "127.0.0.1:3130",
        ] {
            assert!(!seen.contains(gone), "{gone} reached the upstream: {seen}");
        }
        let log = w.log.lock().unwrap().join("\n");
        assert!(
            log.contains("GH POST") && log.contains("/repos/a/b forwarded 200"),
            "{log}"
        );
        assert!(!log.contains("REALvalue") && !log.contains("x=1"));
    }

    #[test]
    fn other_paths_unknown_names_and_revoked_credentials_get_nothing() {
        let w = world(&["/repos/"], None);
        for (req, code) in [
            ("GET /s/GH/user HTTP/1.1\r\n\r\n", "403"),
            ("GET /s/GH/repos/../user HTTP/1.1\r\n\r\n", "403"),
            ("GET /s/NOPE/repos/x HTTP/1.1\r\n\r\n", "404"),
            ("GET /elsewhere HTTP/1.1\r\n\r\n", "404"),
        ] {
            let r = send(&w, req);
            assert!(r.starts_with(&format!("HTTP/1.1 {code}")), "{req}: {r}");
        }
        std::fs::write(w.revoked.path(), "GH\n").unwrap();
        assert!(send(&w, "GET /s/GH/repos/x HTTP/1.1\r\n\r\n").starts_with("HTTP/1.1 403"));
        assert!(
            w.up.seen.lock().unwrap().is_empty(),
            "a refused request left the host"
        );
        assert!(w
            .log
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.contains("refused: revoked")));
        // The revoked route left the gateway, so its value is wiped.
        assert!(w.g.routes.lock().unwrap().is_empty());
    }

    #[test]
    fn revoke_and_the_end_of_the_sandbox_drop_the_routes() {
        let w = world(&[], None);
        w.g.drop_revoked();
        assert_eq!(w.g.routes.lock().unwrap().len(), 1);
        std::fs::write(w.revoked.path(), "*\n").unwrap();
        w.g.drop_revoked();
        assert!(w.g.routes.lock().unwrap().is_empty());
        let w = world(&[], None);
        w.g.clear();
        assert!(w.g.routes.lock().unwrap().is_empty());
        assert!(!format!("{:?}", Secret::from("ghp_REALvalue")).contains("REAL"));
    }

    #[test]
    fn many_refusals_call_for_a_freeze_once() {
        let w = world(&["/repos/"], None);
        for _ in 0..MAX_REFUSED + 5 {
            send(&w, "GET /s/GH/user HTTP/1.1\r\n\r\n");
        }
        assert_eq!(w.abuse.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn events_stream_through_as_they_come() {
        let w = world(&[], None);
        let (mut a, b) = UnixStream::pair().unwrap();
        let g = w.g.clone();
        std::thread::spawn(move || g.handle(b));
        a.write_all(b"GET /s/GH/sse HTTP/1.1\r\nAccept: text/event-stream\r\n\r\n")
            .unwrap();
        let start = std::time::Instant::now();
        let mut r = BufReader::new(a);
        let mut first_event = None;
        let mut all = String::new();
        loop {
            let mut line = String::new();
            if r.read_line(&mut line).unwrap() == 0 {
                break;
            }
            if line.starts_with("data: 0") {
                first_event = Some(start.elapsed());
            }
            all.push_str(&line);
        }
        assert!(all.contains("data: 0") && all.contains("data: 2"), "{all}");
        // The first event came before the upstream finished (900 ms).
        assert!(
            first_event.unwrap() < Duration::from_millis(600),
            "{first_event:?}"
        );
    }

    #[test]
    fn a_chunked_body_and_a_query_credential_pass_correctly() {
        let w = world(&[], Some("key"));
        let r = send(
            &w,
            "POST /s/GH/v1/x?key=FAKE&a=b HTTP/1.1\r\nTransfer-Encoding: chunked\r\nAuthorization: Bearer FAKE\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n",
        );
        assert!(r.starts_with("HTTP/1.1 200"), "{r}");
        let seen = w.up.seen.lock().unwrap().join("\n");
        assert!(
            seen.starts_with("POST /v1/x?a=b&key=ghp_REALvalue HTTP/1.1"),
            "{seen}"
        );
        assert!(
            !seen.contains("FAKE") && !seen.contains("Authorization"),
            "{seen}"
        );
        // The chunks pass as they are, to the last one.
        assert!(
            seen.ends_with("5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n"),
            "{seen}"
        );
    }

    #[test]
    fn a_git_route_lets_only_fetch_through() {
        let c = crate::credentials::parse(
            "[credentials.g]\nvia = \"gateway\"\npreset = \"github-git\"\n",
        )
        .unwrap()
        .remove("g")
        .unwrap();
        let r = Route::new(&c, "t".into());
        let up = Some("service=git-upload-pack");
        let rp = Some("service=git-receive-pack");
        assert!(r.allows("/o/r.git/info/refs", up));
        assert!(r.allows("/o/r/info/refs", up));
        assert!(r.allows("/o/r.git/git-upload-pack", None));
        // Push, the API, other depths, empty or escaped segments: no.
        assert!(!r.allows("/o/r.git/info/refs", rp));
        assert!(!r.allows("/o/r.git/info/refs", None));
        assert!(!r.allows("/o/r.git/git-receive-pack", None));
        assert!(!r.allows("/user", None));
        assert!(!r.allows("/o/r.git/HEAD", None));
        assert!(!r.allows("/o/r/x/git-upload-pack", None));
        assert!(!r.allows("//r/git-upload-pack", None));
        assert!(!r.allows("/o%2Fx/r/git-upload-pack", None));
        // Push only when the user binds it.
        let mut c = c;
        c.paths.push("/*/*/git-receive-pack".into());
        let r = Route::new(&c, "t".into());
        assert!(r.allows("/o/r.git/info/refs", rp));
        assert!(r.allows("/o/r.git/git-receive-pack", None));
        assert!(path_matches("/*/*.git/x", "/o/r.git/x"));
        assert!(!path_matches("/*/*.git/x", "/o/r/x"));
        assert!(!path_matches("/*/*/info/refs", "/../x/info/refs"));
        assert!(!path_matches("/*/*/info/refs", "/o/./info/refs"));
    }

    #[test]
    fn a_stand_in_looks_like_the_value_and_carries_the_mark() {
        let real = "ghp_0123456789abcdefghijklmnopqrstuvwxyz";
        let s = stand_in(real, "a1b2c3d4e5f6");
        assert_eq!(s.len(), real.len());
        assert!(s.starts_with("ghp_PADDOCKa1b2c3d4e5f6"), "{s}");
        assert_ne!(s, real);
        let s = stand_in("sk-ant-api03-xyz", "a1b2c3d4e5f6");
        assert!(s.starts_with("sk-PADDOCK"), "{s}");
        assert!(stand_in("short", "a1b2c3d4e5f6").contains("a1b2c3d4e5f6"));
    }
}
