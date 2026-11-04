use super::*;

/// Some networks answer every name with a fake address (a "fake IP" DNS
/// of a VPN or proxy app). A test that needs a name to fail cannot run there.
pub(super) fn names_fail_to_resolve() -> bool {
    use std::net::ToSocketAddrs;
    let ok = "no-such-host.invalid:9".to_socket_addrs().is_err();
    if !ok {
        eprintln!("skipped: this network resolves .invalid names");
    }
    ok
}

#[test]
fn unreachable_target_gives_502() {
    if !names_fail_to_resolve() {
        return;
    }
    let (_d, sock) = start(HostProxy::default());
    assert_eq!(connect(&sock, "no-such-host.invalid:9").0, 502);
}
