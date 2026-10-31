//! Desktop notifications through the org.freedesktop.Notifications D-Bus API.
//!
//! Paddock does not run `notify-send`: the arguments of a host process are
//! visible to every local user in /proc, and a notification can hold request
//! text such as clipboard content. The session bus belongs to the user, so
//! the text stays with the user.

use crate::env::HostEnv;
use anyhow::{Context as _, Result};
use std::collections::HashMap;
use zbus::blocking::{connection::Builder, Connection, Proxy};
use zbus::zvariant::Value;

const DEST: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";

/// One notification. `actions` are (key, label) pairs.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Note {
    pub summary: String,
    pub body: String,
    pub actions: Vec<(String, String)>,
    pub critical: bool,
}

/// The session bus of the host user. The address comes from the host
/// environment that paddock read at start, not from the current process, so
/// tests with a fake environment never reach the real desktop.
pub fn session(env: &HostEnv) -> Result<Connection> {
    let addr = match env.var("DBUS_SESSION_BUS_ADDRESS") {
        Some(a) if !a.is_empty() => a.to_string(),
        _ => {
            let bus = env
                .runtime_dir
                .as_ref()
                .map(|d| d.join("bus"))
                .filter(|b| b.exists())
                .context("no session bus for notifications")?;
            format!("unix:path={}", bus.display())
        }
    };
    Ok(Builder::address(addr.as_str())?.build()?)
}

fn proxy(conn: &Connection) -> Result<Proxy<'static>> {
    Ok(Proxy::new(conn, DEST, PATH, DEST)?)
}

/// The body as plain text for a server that reads markup in bodies (the
/// `body-markup` capability, on by default in mako and dunst): `&`, `<` and
/// `>` are escaped, so text from a sandbox cannot add links, images or
/// formatting.
pub fn escape_markup(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn reads_markup(p: &Proxy<'_>) -> bool {
    p.call::<_, _, Vec<String>>("GetCapabilities", &())
        .map(|caps| caps.iter().any(|c| c == "body-markup"))
        // When the server does not say, escaping is the safe choice.
        .unwrap_or(true)
}

fn send(p: &Proxy<'_>, n: &Note) -> Result<u32> {
    let body = if reads_markup(p) {
        escape_markup(&n.body)
    } else {
        n.body.clone()
    };
    let actions: Vec<&str> = n
        .actions
        .iter()
        .flat_map(|(k, l)| [k.as_str(), l.as_str()])
        .collect();
    let mut hints: HashMap<&str, Value<'_>> = HashMap::new();
    hints.insert("urgency", Value::U8(if n.critical { 2 } else { 1 }));
    let id: u32 = p.call(
        "Notify",
        &(
            "paddock",
            0u32,
            "",
            n.summary.as_str(),
            body.as_str(),
            actions,
            hints,
            -1i32,
        ),
    )?;
    Ok(id)
}

/// Show a notification and return at once.
pub fn show(env: &HostEnv, n: &Note) -> Result<()> {
    let conn = session(env)?;
    send(&proxy(&conn)?, n)?;
    Ok(())
}

/// Show a notification with actions. `on_action` gets the key of the action
/// that the user picks. The call returns when the notification is shown; a
/// thread waits for the answer until the notification closes. The returned
/// handle closes the notification early.
pub fn ask(
    env: &HostEnv,
    n: &Note,
    on_action: impl FnOnce(&str) + Send + 'static,
) -> Result<Handle> {
    let conn = session(env)?;
    let p = proxy(&conn)?;
    // Subscribe before the notification exists, so no answer is lost.
    let signals = p.receive_all_signals()?;
    let id = send(&p, n)?;
    let conn2 = conn.clone();
    std::thread::spawn(move || {
        let _keep = conn2;
        for msg in signals {
            let h = msg.header();
            let member = h.member().map(|m| m.as_str().to_string());
            match member.as_deref() {
                Some("ActionInvoked") => {
                    if let Ok((nid, key)) = msg.body().deserialize::<(u32, String)>() {
                        if nid == id {
                            on_action(&key);
                            return;
                        }
                    }
                }
                Some("NotificationClosed") => {
                    if let Ok((nid, _)) = msg.body().deserialize::<(u32, u32)>() {
                        if nid == id {
                            return;
                        }
                    }
                }
                _ => {}
            }
        }
    });
    Ok(Handle { conn, id })
}

/// A shown notification.
pub struct Handle {
    conn: Connection,
    id: u32,
}

impl Handle {
    pub fn close(&self) {
        if let Ok(p) = proxy(&self.conn) {
            let _: Result<(), _> = p.call("CloseNotification", &(self.id,));
        }
    }
}

#[cfg(test)]
mod markup_tests {
    #[test]
    fn markup_from_a_sandbox_is_shown_as_text() {
        assert_eq!(
            super::escape_markup("<b>x</b> & <a href=\"u\">y</a>"),
            "&lt;b&gt;x&lt;/b&gt; &amp; &lt;a href=\"u\"&gt;y&lt;/a&gt;"
        );
    }
}
