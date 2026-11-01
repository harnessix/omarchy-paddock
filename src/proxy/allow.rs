//! Network mode `api`: the hosts that the proxy lets through, the check of
//! the TLS ClientHello against the CONNECT host, and the audit of refusals.

use std::collections::HashMap;
use std::io::Read;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// One allowed host. Without a port, only 443.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Lowercase, without a trailing dot, without the leading `*.`.
    pub host: String,
    /// The entry started with `*.`: subdomains of `host` match, `host` itself
    /// does not.
    pub subdomains: bool,
    pub port: u16,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Allowlist {
    pub entries: Vec<Entry>,
}

fn norm(host: &str) -> String {
    host.trim_end_matches('.').to_ascii_lowercase()
}

fn is_ip(host: &str) -> bool {
    host.trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<IpAddr>()
        .is_ok()
}

impl Allowlist {
    /// Parse `HOST`, `HOST:PORT` and `*.HOST` entries. IP literals and
    /// entries that do not parse are dropped and returned as the second value.
    pub fn parse<S: AsRef<str>>(items: &[S]) -> (Self, Vec<String>) {
        let mut entries = Vec::new();
        let mut bad = Vec::new();
        for raw in items {
            let raw = raw.as_ref().trim();
            let (host, port) = match raw.rsplit_once(':') {
                Some((h, p)) if !h.contains(':') => match p.parse::<u16>() {
                    Ok(p) if p > 0 => (h, p),
                    _ => {
                        bad.push(raw.to_string());
                        continue;
                    }
                },
                _ => (raw, 443),
            };
            let (subdomains, host) = match host.strip_prefix("*.") {
                Some(h) => (true, h),
                None => (false, host),
            };
            let host = norm(host);
            let valid = !host.is_empty()
                && !is_ip(&host)
                && host
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.');
            if !valid {
                bad.push(raw.to_string());
                continue;
            }
            let e = Entry {
                host,
                subdomains,
                port,
            };
            if !entries.contains(&e) {
                entries.push(e);
            }
        }
        (Self { entries }, bad)
    }

    /// Whether `host:port` is on the list. The error is the reason, for the
    /// audit log.
    pub fn check(&self, host: &str, port: u16) -> Result<(), String> {
        if is_ip(host) {
            return Err("an IP address, not a host name".into());
        }
        let h = norm(host);
        let name_ok = |e: &Entry| {
            if e.subdomains {
                h.len() > e.host.len() + 1 && h.ends_with(&format!(".{}", e.host))
            } else {
                h == e.host
            }
        };
        let named: Vec<&Entry> = self.entries.iter().filter(|e| name_ok(e)).collect();
        if named.is_empty() {
            return Err("not on the list".into());
        }
        if !named.iter().any(|e| e.port == port) {
            return Err(format!("port {port} is not allowed"));
        }
        Ok(())
    }
}

const MAX_HELLO: usize = 64 * 1024;

/// Read the TLS records that carry the ClientHello. Returns the raw bytes
/// (to forward unchanged) and the handshake message.
pub fn read_client_hello(s: &mut impl Read) -> Result<(Vec<u8>, Vec<u8>), String> {
    let mut raw = Vec::new();
    let mut msg = Vec::new();
    loop {
        let mut hdr = [0u8; 5];
        s.read_exact(&mut hdr)
            .map_err(|_| "no TLS ClientHello".to_string())?;
        if hdr[0] != 0x16 {
            return Err("not TLS".into());
        }
        let len = u16::from_be_bytes([hdr[3], hdr[4]]) as usize;
        if len == 0 || raw.len() + 5 + len > MAX_HELLO {
            return Err("bad TLS record".into());
        }
        let mut body = vec![0u8; len];
        s.read_exact(&mut body)
            .map_err(|_| "a short TLS record".to_string())?;
        raw.extend_from_slice(&hdr);
        raw.extend_from_slice(&body);
        msg.extend_from_slice(&body);
        if msg.len() >= 4 {
            let want = 4 + (u32::from_be_bytes([0, msg[1], msg[2], msg[3]]) as usize);
            if msg.len() >= want {
                msg.truncate(want);
                return Ok((raw, msg));
            }
        }
    }
}

struct Cur<'a>(&'a [u8]);

impl<'a> Cur<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Some(a)
    }
    fn u8(&mut self) -> Option<usize> {
        Some(self.take(1)?[0] as usize)
    }
    fn u16(&mut self) -> Option<usize> {
        let b = self.take(2)?;
        Some(u16::from_be_bytes([b[0], b[1]]) as usize)
    }
    fn vec8(&mut self) -> Option<&'a [u8]> {
        let n = self.u8()?;
        self.take(n)
    }
    fn vec16(&mut self) -> Option<&'a [u8]> {
        let n = self.u16()?;
        self.take(n)
    }
}

/// The SNI host name of a ClientHello handshake message. `Ok(None)` means
/// that the message has no server_name extension.
pub fn sni(msg: &[u8]) -> Result<Option<String>, String> {
    let bad = || "a malformed ClientHello".to_string();
    let mut c = Cur(msg);
    if c.u8() != Some(1) {
        return Err("not a ClientHello".into());
    }
    let len = c.take(3).ok_or_else(bad)?;
    let len = u32::from_be_bytes([0, len[0], len[1], len[2]]) as usize;
    let mut c = Cur(c.take(len).ok_or_else(bad)?);
    c.take(2 + 32).ok_or_else(bad)?; // version, random
    c.vec8().ok_or_else(bad)?; // session id
    c.vec16().ok_or_else(bad)?; // cipher suites
    c.vec8().ok_or_else(bad)?; // compression methods
    if c.0.is_empty() {
        return Ok(None);
    }
    let mut exts = Cur(c.vec16().ok_or_else(bad)?);
    while !exts.0.is_empty() {
        let ty = exts.u16().ok_or_else(bad)?;
        let data = exts.vec16().ok_or_else(bad)?;
        if ty != 0 {
            continue;
        }
        let mut list = Cur(Cur(data).vec16().ok_or_else(bad)?);
        while !list.0.is_empty() {
            let kind = list.u8().ok_or_else(bad)?;
            let name = list.vec16().ok_or_else(bad)?;
            if kind == 0 {
                let name = std::str::from_utf8(name).map_err(|_| bad())?;
                return Ok(Some(name.to_string()));
            }
        }
        return Ok(None);
    }
    Ok(None)
}

/// Check that the ClientHello names `host`. Returns the reason on a refusal.
pub fn check_sni(msg: &[u8], host: &str) -> Result<(), String> {
    match sni(msg)? {
        None => Err("the TLS ClientHello has no server name".into()),
        Some(n) if norm(&n) == norm(host) => Ok(()),
        Some(_) => Err("the TLS server name differs from the CONNECT host".into()),
    }
}

pub type Sink = Arc<dyn Fn(&str, &str) + Send + Sync>;

/// Reports refused connections, at most once per host and minute. The sink
/// gets the host and the reason, never content.
#[derive(Clone)]
pub struct Refusals {
    sink: Sink,
    last: Arc<Mutex<HashMap<String, Instant>>>,
    every: Duration,
}

impl std::fmt::Debug for Refusals {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Refusals")
    }
}

impl Refusals {
    pub fn new(sink: Sink) -> Self {
        Self {
            sink,
            last: Arc::new(Mutex::new(HashMap::new())),
            every: Duration::from_secs(60),
        }
    }

    pub fn report(&self, host: &str, reason: &str) {
        let key = norm(host);
        {
            let mut last = self.last.lock().unwrap();
            let now = Instant::now();
            if last
                .get(&key)
                .is_some_and(|t| now.duration_since(*t) < self.every)
            {
                return;
            }
            last.retain(|_, t| now.duration_since(*t) < self.every);
            last.insert(key.clone(), now);
        }
        (self.sink)(&key, reason);
    }
}

/// The policy of network mode `api`.
#[derive(Debug, Clone, Default)]
pub struct ApiPolicy {
    pub list: Allowlist,
    pub refusals: Option<Refusals>,
}

impl ApiPolicy {
    pub fn refuse(&self, host: &str, reason: &str) {
        if let Some(r) = &self.refusals {
            r.report(host, reason);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(items: &[&str]) -> Allowlist {
        let (l, bad) = Allowlist::parse(items);
        assert!(bad.is_empty(), "{bad:?}");
        l
    }

    #[test]
    fn exact_names_on_port_443() {
        let l = list(&["api.example.com"]);
        assert!(l.check("api.example.com", 443).is_ok());
        assert!(l.check("API.Example.com.", 443).is_ok());
        assert!(l.check("x.api.example.com", 443).is_err());
        assert!(l.check("example.com", 443).is_err());
        assert!(l.check("api.example.com", 80).is_err());
        assert!(l.check("api.example.com", 8443).is_err());
    }

    #[test]
    fn star_matches_subdomains_only() {
        let l = list(&["*.example.com"]);
        assert!(l.check("a.example.com", 443).is_ok());
        assert!(l.check("a.b.example.com", 443).is_ok());
        assert!(l.check("example.com", 443).is_err());
        assert!(l.check("badexample.com", 443).is_err());
        assert!(l.check("a.example.com", 80).is_err());
    }

    #[test]
    fn a_listed_port() {
        let l = list(&["api.example.com:8443"]);
        assert!(l.check("api.example.com", 8443).is_ok());
        assert!(l.check("api.example.com", 443).is_err());
    }

    #[test]
    fn ip_literals_are_refused() {
        let (l, bad) = Allowlist::parse(&["1.2.3.4", "[::1]:443", "ok.example.com", "a b"]);
        assert_eq!(bad, vec!["1.2.3.4", "[::1]:443", "a b"]);
        assert_eq!(l.entries.len(), 1);
        assert!(l.check("1.2.3.4", 443).is_err());
        assert!(l.check("::1", 443).is_err());
        assert!(l.check("[::1]", 443).is_err());
    }

    /// A ClientHello as a client writes it, captured into bytes: TLS 1.2
    /// record, one cipher suite, and the given extensions.
    fn hello(exts: &[(u16, Vec<u8>)]) -> Vec<u8> {
        let mut body = vec![0x03, 0x03];
        body.extend([7u8; 32]);
        body.push(0); // session id
        body.extend([0, 2, 0x13, 0x01]); // TLS_AES_128_GCM_SHA256
        body.extend([1, 0]); // null compression
        let mut e = Vec::new();
        for (ty, data) in exts {
            e.extend(ty.to_be_bytes());
            e.extend((data.len() as u16).to_be_bytes());
            e.extend(data);
        }
        body.extend((e.len() as u16).to_be_bytes());
        body.extend(e);
        let mut msg = vec![1];
        msg.extend(&(body.len() as u32).to_be_bytes()[1..]);
        msg.extend(body);
        let mut rec = vec![0x16, 0x03, 0x01];
        rec.extend((msg.len() as u16).to_be_bytes());
        rec.extend(msg);
        rec
    }

    fn sni_ext(name: &str) -> (u16, Vec<u8>) {
        let mut entry = vec![0];
        entry.extend((name.len() as u16).to_be_bytes());
        entry.extend(name.as_bytes());
        let mut d = (entry.len() as u16).to_be_bytes().to_vec();
        d.extend(entry);
        (0, d)
    }

    fn parse(rec: &[u8]) -> Vec<u8> {
        let (raw, msg) = read_client_hello(&mut &rec[..]).unwrap();
        assert_eq!(raw, rec);
        msg
    }

    #[test]
    fn sni_of_a_client_hello() {
        let alpn = (16u16, vec![0, 3, 2, b'h', b'2']);
        let msg = parse(&hello(&[alpn.clone(), sni_ext("api.example.com")]));
        assert_eq!(sni(&msg).unwrap().as_deref(), Some("api.example.com"));
        assert!(check_sni(&msg, "API.example.com").is_ok());
        assert!(check_sni(&msg, "other.example.com")
            .unwrap_err()
            .contains("differs"));

        let none = parse(&hello(&[alpn]));
        assert_eq!(sni(&none).unwrap(), None);
        assert!(check_sni(&none, "api.example.com")
            .unwrap_err()
            .contains("no server name"));
    }

    #[test]
    fn a_client_hello_over_two_records() {
        let rec = hello(&[sni_ext("api.example.com")]);
        let msg = &rec[5..];
        let (a, b) = msg.split_at(10);
        let mut two = Vec::new();
        for part in [a, b] {
            two.extend([0x16, 0x03, 0x01]);
            two.extend((part.len() as u16).to_be_bytes());
            two.extend(part);
        }
        let (raw, m) = read_client_hello(&mut &two[..]).unwrap();
        assert_eq!(raw, two);
        assert_eq!(sni(&m).unwrap().as_deref(), Some("api.example.com"));
    }

    #[test]
    fn plain_text_is_not_a_client_hello() {
        let e = read_client_hello(&mut &b"GET / HTTP/1.1\r\n\r\n"[..]).unwrap_err();
        assert_eq!(e, "not TLS");
        let truncated = &hello(&[sni_ext("a.example.com")])[..20];
        assert!(read_client_hello(&mut &truncated[..]).is_err());
    }

    #[test]
    fn refusals_are_reported_once_per_host_and_minute() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s2 = seen.clone();
        let r = Refusals::new(Arc::new(move |h: &str, why: &str| {
            s2.lock().unwrap().push(format!("{h}: {why}"));
        }));
        r.report("a.example.com", "not on the list");
        r.report("A.example.com", "not on the list");
        r.report("b.example.com", "not on the list");
        assert_eq!(seen.lock().unwrap().len(), 2);
    }
}
