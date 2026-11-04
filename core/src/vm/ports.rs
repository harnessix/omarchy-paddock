//! The vsock ports of a paddock VM. The VM has no network
//! device, so all traffic between the guest and the host uses these ports.
//! The numbers are fixed: the guest image, the helper and paddock agree on
//! them.

/// The guest agent.
pub const CONTROL: u32 = 1024;
/// paddock's host request broker.
pub const BROKER: u32 = 1025;
/// paddock's network proxy.
pub const PROXY: u32 = 1026;
/// The pty or the stdio of the run.
pub const PTY: u32 = 1027;

/// Every port with its name, in port order.
pub const ALL: [(u32, &str); 4] = [
    (CONTROL, "control"),
    (BROKER, "broker"),
    (PROXY, "proxy"),
    (PTY, "pty"),
];

/// The name of a known port.
pub fn name(port: u32) -> Option<&'static str> {
    ALL.iter().find(|(p, _)| *p == port).map(|(_, n)| *n)
}

/// A known port from its number (`1025`) or its name (`broker`).
pub fn parse(text: &str) -> Option<u32> {
    if let Some((p, _)) = ALL.iter().find(|(_, n)| *n == text) {
        return Some(*p);
    }
    let port: u32 = text.parse().ok()?;
    name(port).map(|_| port)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ports_are_fixed() {
        assert_eq!((CONTROL, BROKER, PROXY, PTY), (1024, 1025, 1026, 1027));
    }

    #[test]
    fn parse_takes_a_number_or_a_name() {
        assert_eq!(parse("1025"), Some(BROKER));
        assert_eq!(parse("proxy"), Some(PROXY));
        assert_eq!(parse("pty"), Some(PTY));
        assert_eq!(name(CONTROL), Some("control"));
    }

    #[test]
    fn parse_refuses_other_ports() {
        for text in ["22", "1023", "1028", "-1", "", "Broker", "0x401"] {
            assert_eq!(parse(text), None, "{text}");
        }
    }
}
