//! TLS to an `https://` upstream proxy.
//!
//! A proxy URL with the `https` scheme means that the connection to the proxy
//! itself is TLS. The CONNECT request, the forwarded HTTP request and the
//! `Proxy-Authorization` header go inside that TLS session, and a CONNECT
//! tunnel then runs inside it too.
//!
//! rustls (with the `ring` provider) is pure Rust apart from ring's own
//! statically built code, so the binary links no OpenSSL. That matters because
//! the same binary runs inside containers as `paddock-host`. The proxy
//! certificate is checked against the system trust store, read by
//! `rustls-native-certs`: the files that OpenSSL would use, including
//! `SSL_CERT_FILE` and `SSL_CERT_DIR`. A private CA that is installed
//! system-wide, as corporate proxies often need, is trusted like on the host.
//!
//! The tunnel copies both directions in two threads (see `splice`). Both share
//! one `ClientConnection` behind a mutex. No thread holds that mutex during
//! socket I/O, so a slow peer in one direction cannot block the other.

use super::Duplex;
use anyhow::{anyhow, Context, Result};
use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, RootCertStore};
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

fn config(roots: RootCertStore) -> Result<Arc<ClientConfig>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    Ok(Arc::new(
        ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()?
            .with_root_certificates(roots)
            .with_no_client_auth(),
    ))
}

/// The client config with the system trust store, loaded once.
pub fn system_config() -> Result<Arc<ClientConfig>> {
    static CFG: OnceLock<std::result::Result<Arc<ClientConfig>, String>> = OnceLock::new();
    CFG.get_or_init(|| {
        let found = rustls_native_certs::load_native_certs();
        let mut roots = RootCertStore::empty();
        let (added, _) = roots.add_parsable_certificates(found.certs);
        if added == 0 {
            return Err(format!(
                "no CA certificates found in the system trust store{}",
                found
                    .errors
                    .first()
                    .map(|e| format!(": {e}"))
                    .unwrap_or_default()
            ));
        }
        config(roots).map_err(|e| e.to_string())
    })
    .clone()
    .map_err(|e| anyhow!(e))
}

/// A client config that trusts only `roots`. Tests use it for a local CA.
#[cfg(test)]
pub fn config_with_roots(roots: RootCertStore) -> Result<Arc<ClientConfig>> {
    config(roots)
}

struct Shared {
    conn: Mutex<ClientConnection>,
    /// Socket reads. Only the reading thread uses it.
    rsock: TcpStream,
    /// Socket writes. Held while TLS records go out, so records stay in order.
    wsock: Mutex<TcpStream>,
    /// The reading thread has TLS records to send (an alert or a key update
    /// answer) but another thread was writing.
    pending: AtomicBool,
}

/// A TLS session over TCP that two threads can use at once, one to read and
/// one to write.
#[derive(Clone)]
pub struct TlsStream(Arc<Shared>);

fn tls_err(e: rustls::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e)
}

impl TlsStream {
    /// Do the TLS handshake with the proxy at `host`. Fails on any certificate
    /// error: there is no fallback to plain TCP.
    pub fn connect(
        cfg: Arc<ClientConfig>,
        host: &str,
        tcp: TcpStream,
        timeout: Duration,
    ) -> Result<Self> {
        let name = ServerName::try_from(host.to_string())
            .with_context(|| format!("{host} is not a valid TLS server name"))?;
        let mut conn = ClientConnection::new(cfg, name)?;
        tcp.set_read_timeout(Some(timeout))?;
        tcp.set_write_timeout(Some(timeout))?;
        let mut s = tcp.try_clone()?;
        while conn.is_handshaking() {
            conn.complete_io(&mut s)
                .with_context(|| format!("TLS handshake with the upstream proxy {host} failed"))?;
        }
        tcp.set_write_timeout(None)?;
        Ok(Self(Arc::new(Shared {
            conn: Mutex::new(conn),
            rsock: tcp.try_clone()?,
            wsock: Mutex::new(tcp),
            pending: AtomicBool::new(false),
        })))
    }

    /// Send the TLS records that the session has queued. `wait` is false for
    /// the reading thread: if another thread is writing, it leaves a flag and
    /// that thread sends the records when it is done.
    fn send_queued(&self, wait: bool) -> io::Result<()> {
        let sh = &self.0;
        loop {
            let mut sock = if wait {
                sh.wsock.lock().unwrap()
            } else {
                sh.pending.store(true, Ordering::SeqCst);
                match sh.wsock.try_lock() {
                    Ok(g) => g,
                    Err(_) => return Ok(()),
                }
            };
            sh.pending.store(false, Ordering::SeqCst);
            loop {
                let mut out = Vec::new();
                {
                    let mut c = sh.conn.lock().unwrap();
                    while c.wants_write() {
                        c.write_tls(&mut out)?;
                    }
                }
                if out.is_empty() {
                    break;
                }
                sock.write_all(&out)?;
            }
            drop(sock);
            if !sh.pending.load(Ordering::SeqCst) {
                return Ok(());
            }
        }
    }
}

impl Read for TlsStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut raw = [0u8; 16 * 1024];
        loop {
            {
                let mut c = self.0.conn.lock().unwrap();
                match c.reader().read(buf) {
                    Ok(n) => return Ok(n),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(e) => return Err(e),
                }
            }
            // No plaintext is buffered: read more records. The socket read
            // timeout applies here, so the idle check in `pump` still runs.
            let n = (&self.0.rsock).read(&mut raw)?;
            if n == 0 {
                // The proxy closed the TCP connection without close_notify.
                // The tunnel content has its own end-to-end protection, so
                // this is an ordinary end of stream.
                return Ok(0);
            }
            let mut data = &raw[..n];
            let mut c = self.0.conn.lock().unwrap();
            while !data.is_empty() {
                c.read_tls(&mut data)?;
                if let Err(e) = c.process_new_packets() {
                    drop(c);
                    let _ = self.send_queued(false);
                    return Err(tls_err(e));
                }
            }
            let reply = c.wants_write();
            drop(c);
            if reply {
                self.send_queued(false)?;
            }
        }
    }
}

impl Write for TlsStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.0.conn.lock().unwrap().writer().write(buf)?;
        self.send_queued(true)?;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.send_queued(true)
    }
}

impl Duplex for TlsStream {
    fn dup(&self) -> io::Result<Box<dyn Duplex>> {
        Ok(Box::new(self.clone()))
    }
    /// Half-close: close_notify, then a TCP FIN. TLS 1.3 lets the other side
    /// keep sending after that, so the reading direction stays open.
    fn close_write(&self) {
        self.0.conn.lock().unwrap().send_close_notify();
        let _ = self.send_queued(true);
        let _ = self.0.rsock.shutdown(Shutdown::Write);
    }
    fn set_tick(&self, d: Duration) -> io::Result<()> {
        self.0.rsock.set_read_timeout(Some(d))
    }
}
