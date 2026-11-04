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
/// callback parameters, or the URL itself when it points at localhost. Some
/// apps open their own site first and pass the login page on in a parameter
/// (`next=https://auth.../authorize?redirect_uri=...`), so links in parameter
/// values and in the fragment are searched too, a few levels deep.
pub fn callback_ports(raw: &str) -> Vec<u16> {
    let mut out = Vec::new();
    search(raw, None, 3, &mut out);
    out.sort();
    out.dedup();
    out
}

fn search(raw: &str, base: Option<&url::Url>, depth: u8, out: &mut Vec<u16>) {
    if let Some(p) = local_port(raw) {
        out.push(p);
    }
    let parsed = match base {
        // A path on the same site, such as `next=/auth/login?redirect_uri=...`.
        Some(b) if raw.starts_with('/') && !raw.starts_with("//") => b.join(raw),
        _ => url::Url::parse(raw),
    };
    let Ok(u) = parsed else {
        return;
    };
    let mut pairs: Vec<(String, String)> = u.query_pairs().into_owned().collect();
    if let Some(f) = u.fragment() {
        pairs.extend(url::form_urlencoded::parse(f.as_bytes()).into_owned());
    }
    for (k, v) in pairs {
        if CALLBACK_KEYS.contains(&k.as_str()) {
            if let Some(p) = local_port(&v) {
                out.push(p);
            }
        }
        let nested = v.starts_with("http://") || v.starts_with("https://") || v.starts_with('/');
        if depth > 0 && nested {
            search(&v, Some(&u), depth - 1, out);
        }
    }
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

/// The ports that are allowed now.
pub fn allowed_ports() -> Vec<u16> {
    let mut m = ALLOWED.lock().unwrap();
    m.retain(|_, until| *until > Instant::now());
    m.keys().copied().collect()
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
    backend: &str,
) -> Result<Option<crate::app::ports::Forwarder>> {
    let origin = "login callbacks";
    let run = env.paddock_run().join(&spec.id);
    open_shim(env, spec, origin)?;

    // Seatbelt shares the host network: the callback reaches the harness
    // on host localhost, and no forwarder is needed. Programs on macOS open
    // a link with `open URL` (Claude Code through Node's `open`), and
    // Seatbelt denies Launch Services, so `open` is the shim too. Codex and
    // Grok Build call Launch Services themselves: that fails, and they
    // print the link instead.
    if backend == crate::backend::seatbelt::NAME {
        let shim = run.join("open");
        crate::gui::write_file(&shim, OPEN_SHIM, 0o755)?;
        spec.add_tool("open", shim, origin);
        return Ok(None);
    }
    // srt cannot place the shared folder that the forwarder uses.
    // The VM backend: paddock in the guest prepares the sandbox, and the
    // host run forwards the allowed ports into the guest (vm/attach.rs).
    if backend == crate::backend::vm::NAME {
        return Ok(None);
    }
    if backend == "srt" {
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
    // In the guest of the VM backend, the host allows the ports: it
    // forwards only the ports of the login pages that it opened.
    let filter: fn(u16) -> bool =
        if crate::vm::agent::guest_socket(crate::vm::agent::HOST_SOCKET_VAR).is_some() {
            |_| true
        } else {
            allowed
        };
    Ok(Some(crate::app::ports::Forwarder::start_filtered(
        inbound, filter,
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
    fn finds_a_callback_port_in_a_nested_login_link() {
        // The app opens its own site, which passes the login page on.
        let login = "https://auth.example.com/oauth/authorize?client_id=x&redirect_uri=http%3A%2F%2F127.0.0.1%3A1455%2Fauth%2Fcallback";
        let enc = |s: &str| url::form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>();
        let once = format!("https://app.example.com/auth/login?next={}", enc(login));
        assert_eq!(callback_ports(&once), vec![1455]);
        let twice = format!("https://app.example.com/start?continue={}", enc(&once));
        assert_eq!(callback_ports(&twice), vec![1455]);
        // A path on the same site, and a fragment.
        let path = format!(
            "https://app.example.com/l?next={}",
            enc("/authorize?redirect_uri=http://localhost:1456/cb")
        );
        assert_eq!(callback_ports(&path), vec![1456]);
        let frag = format!("https://app.example.com/#next={}", enc(login));
        assert_eq!(callback_ports(&frag), vec![1455]);
        // A nested callback to another host stays out.
        let far = format!(
            "https://app.example.com/?next={}",
            enc("https://a.example/?redirect_uri=https://evil.example:9/")
        );
        assert!(callback_ports(&far).is_empty());
    }

    #[test]
    fn a_seatbelt_run_opens_links_with_open_too() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("home");
        let env = HostEnv::from_parts(home.clone(), "u".into(), 501, 20, Default::default());
        let mut s = SandboxSpec::new(
            "id".into(),
            "fake",
            &home.join("p"),
            home.join("sb"),
            &home,
            501,
            20,
            "u",
        );
        assert!(prepare(&env, &mut s, crate::backend::seatbelt::NAME)
            .unwrap()
            .is_none());
        for name in ["open", "xdg-open"] {
            let t = s.host_tools.iter().find(|t| t.name == name).unwrap();
            let text = std::fs::read_to_string(&t.src).unwrap();
            assert!(text.contains("exec paddock host open \"$a\""), "{name}");
        }
        assert!(s.mounts.is_empty());
    }

    #[test]
    fn ports_are_allowed_only_after_the_page_was_opened() {
        assert!(!allowed(47123));
        note_opened("https://x.example/?redirect_uri=http://localhost:47123/cb");
        assert!(allowed(47123));
        assert!(!allowed(47124));
    }
}
