//! Login callbacks for terminal harnesses.
//!
//! Many CLIs log in with OAuth: they listen on a local port, open the login
//! page in a browser, and wait for the browser to come back to
//! `http://localhost:PORT/...`. In a sandbox that fails twice: there is no
//! browser, and the host browser cannot reach a port inside the sandbox.
//!
//! paddock fixes both, narrowly. `xdg-open` and `$BROWSER` in the sandbox
//! send the URL through the host passthrough channel, which asks the user
//! and opens it in the host browser. When the host opens it, this module
//! reads the callback address from the URL (`redirect_uri` and similar) and
//! allows that one port for ten minutes. Only an allowed port that the
//! sandbox is listening on is forwarded from the host loopback, so a server
//! that an agent starts for other reasons stays unreachable from the host.

use crate::env::HostEnv;
use crate::spec::{Mount, SandboxSpec};
use anyhow::Result;
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a callback port stays open after the login page was opened.
pub const WINDOW: Duration = Duration::from_secs(600);

static ALLOWED: Mutex<BTreeMap<u16, Instant>> = Mutex::new(BTreeMap::new());

/// Query parameters that carry the address a login flow returns to.
const CALLBACK_KEYS: &[&str] = &[
    "redirect_uri",
    "redirect_url",
    "callback",
    "callback_url",
    "return_to",
    "returnTo",
];

fn local_port(raw: &str) -> Option<u16> {
    let u = url::Url::parse(raw).ok()?;
    if u.scheme() != "http" {
        return None;
    }
    let host = u.host_str()?.trim_start_matches('[').trim_end_matches(']');
    if !crate::proxy::is_loopback(host) {
        return None;
    }
    u.port()
}

/// The callback ports named by a URL that the sandbox asked to open: in its
/// callback parameters, or the URL itself when it points at localhost.
pub fn callback_ports(raw: &str) -> Vec<u16> {
    let mut out = Vec::new();
    if let Some(p) = local_port(raw) {
        out.push(p);
    }
    if let Ok(u) = url::Url::parse(raw) {
        for (k, v) in u.query_pairs() {
            if CALLBACK_KEYS.contains(&k.as_ref()) {
                if let Some(p) = local_port(&v) {
                    out.push(p);
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Called by the host passthrough channel after it opened a URL.
pub fn note_opened(raw: &str) {
    let until = Instant::now() + WINDOW;
    let mut m = ALLOWED.lock().unwrap();
    for p in callback_ports(raw) {
        log::debug!(
            "login callback port {p} open for {} seconds",
            WINDOW.as_secs()
        );
        m.insert(p, until);
    }
}

pub fn allowed(port: u16) -> bool {
    let mut m = ALLOWED.lock().unwrap();
    m.retain(|_, until| *until > Instant::now());
    m.contains_key(&port)
}

const OPEN_SHIM: &str = r#"#!/bin/sh
# paddock: open links in the host browser, through the host passthrough
# channel. The host asks before it opens anything.
for a in "$@"; do
  case "$a" in
    http://*|https://*) exec paddock host open "$a" ;;
  esac
done
echo "paddock: only http and https links can be opened from the sandbox" >&2
exit 1
"#;

/// `xdg-open` and `$BROWSER` in the sandbox: http and https links go to the
/// host browser through the host passthrough channel.
pub fn open_shim(env: &HostEnv, spec: &mut SandboxSpec, origin: &str) -> Result<()> {
    let shim = env.paddock_run().join(&spec.id).join("xdg-open");
    crate::gui::write_file(&shim, OPEN_SHIM, 0o755)?;
    spec.add_tool("xdg-open", shim, origin);
    spec.set_env("BROWSER", "xdg-open", origin);
    Ok(())
}

/// Wire login callbacks into a sandbox. Returns the forwarder, which lives as
/// long as the sandbox runs.
pub fn prepare(
    env: &HostEnv,
    spec: &mut SandboxSpec,
    srt: bool,
) -> Result<Option<crate::app::ports::Forwarder>> {
    let origin = "login callbacks";
    let run = env.paddock_run().join(&spec.id);
    open_shim(env, spec, origin)?;

    // srt cannot place the shared folder that the forwarder uses.
    if srt {
        spec.warnings.push(
            "login callbacks to localhost need the bwrap, docker or podman backend; on srt, log in with a device code or an API key".into(),
        );
        return Ok(None);
    }
    let inbound = run.join("inbound");
    crate::privfs::dir(&inbound)?;
    if !spec
        .mounts
        .iter()
        .any(|m| m.dst == std::path::Path::new(crate::app::INBOUND_DIR))
    {
        spec.add_mount(Mount::bind(&inbound, crate::app::INBOUND_DIR, true, origin));
    }
    spec.set_env("PADDOCK_INBOUND_PORTS", "1", origin);
    spec.setup.push("paddock inbound >/dev/null 2>&1 &".into());
    Ok(Some(crate::app::ports::Forwarder::start_filtered(
        inbound, allowed,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_callback_ports() {
        let u = "https://auth.example.com/oauth/authorize?client_id=x&redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback&state=s";
        assert_eq!(callback_ports(u), vec![1455]);
        assert_eq!(callback_ports("http://127.0.0.1:8123/start"), vec![8123]);
        assert!(callback_ports("https://github.com/login/device").is_empty());
        // A callback to another host is not a local port.
        assert!(callback_ports(
            "https://a.example/?redirect_uri=https%3A%2F%2Fevil.example%3A9%2F"
        )
        .is_empty());
    }

    #[test]
    fn ports_are_allowed_only_after_the_page_was_opened() {
        assert!(!allowed(47123));
        note_opened("https://x.example/?redirect_uri=http://localhost:47123/cb");
        assert!(allowed(47123));
        assert!(!allowed(47124));
    }
}
