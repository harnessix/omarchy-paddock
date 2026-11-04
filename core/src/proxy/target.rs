//! DNS lookup for a proxy target.

use std::net::{IpAddr, SocketAddr, ToSocketAddrs};

/// Resolve `host`. A name gets a trailing dot so a DNS search domain cannot
/// turn it into a different host. An address is used as written.
pub(super) fn resolve_target(host: &str, port: u16) -> std::io::Result<Vec<SocketAddr>> {
    if host.parse::<IpAddr>().is_ok() || host.ends_with('.') {
        return Ok((host, port).to_socket_addrs()?.collect());
    }
    let fqdn = format!("{host}.");
    Ok((fqdn.as_str(), port).to_socket_addrs()?.collect())
}
