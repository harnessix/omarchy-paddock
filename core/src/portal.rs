//! `paddock portal`: the desktop services that an app sees on its private
//! session bus (app mode, `bus = "private"`).
//!
//! The app's bus is a `dbus-daemon` inside the sandbox, so nothing on it
//! reaches the host. This service gives the app the two host services that
//! it needs, and sends each request through the host passthrough channel
//! (broker.rs), where the user's policy applies:
//!
//! - `org.freedesktop.portal.OpenURI.OpenURI`: http and https links become an
//!   `open-url` request, which asks the user by default. Other links, and
//!   `OpenFile` and `OpenDirectory`, are refused: the host cannot open a file
//!   that exists only in the sandbox.
//! - `org.freedesktop.Notifications.Notify`: becomes a `notify` request.
//!
//! There is no file chooser. Apps then use their own dialog, which sees only
//! the files in the sandbox. There is no tray.

use crate::broker::{Request, Status, PROTOCOL};
use anyhow::Result;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::Arc;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

pub const PORTAL_NAME: &str = "org.freedesktop.portal.Desktop";
pub const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
pub const NOTIFY_NAME: &str = "org.freedesktop.Notifications";
pub const NOTIFY_PATH: &str = "/org/freedesktop/Notifications";

/// Link requests that may wait for the user at the same time. More are
/// refused at once, so an app cannot pile up approval cards.
const MAX_PENDING_LINKS: usize = 4;
/// Notifications that may wait to be shown. More are dropped.
const MAX_QUEUED_NOTIFICATIONS: usize = 16;

/// Portal response codes (org.freedesktop.portal.Request.Response).
pub const RESPONSE_OK: u32 = 0;
pub const RESPONSE_CANCELLED: u32 = 1;
pub const RESPONSE_OTHER: u32 = 2;

/// The object path of a portal request: the caller's unique name without
/// the colon and with dots as underscores, then the caller's token. A token
/// that is not a valid path element is replaced.
pub fn request_path(sender: &str, token: Option<&str>, fallback: u32) -> String {
    let s: String = sender
        .trim_start_matches(':')
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let t = match token {
        Some(t)
            if !t.is_empty()
                && t.len() <= 64
                && t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') =>
        {
            t.to_string()
        }
        _ => format!("paddock{fallback}"),
    };
    format!("{PORTAL_PATH}/request/{s}/{t}")
}

/// The links that the host may open: http and https, a sane length, no
/// control characters. The broker checks the scheme again.
pub fn openable(uri: &str) -> bool {
    uri.len() <= 8192
        && !uri.chars().any(|c| c.is_control())
        && url::Url::parse(uri).is_ok_and(|u| matches!(u.scheme(), "http" | "https"))
}

/// The portal response code for a broker answer.
pub fn response_code(status: &Status) -> u32 {
    match status {
        Status::Done => RESPONSE_OK,
        Status::Denied | Status::Timeout => RESPONSE_CANCELLED,
        _ => RESPONSE_OTHER,
    }
}

/// A broker request from the app. The reason tells the user that the app
/// asked, not an agent.
pub fn request(cap: &str, args: &[(&str, &str)], reason: &str) -> Request {
    Request {
        v: PROTOCOL,
        cap: cap.into(),
        args: args
            .iter()
            .map(|(k, v)| (k.to_string(), serde_json::json!(v)))
            .collect::<BTreeMap<_, _>>(),
        reason: reason.into(),
    }
}

fn send(req: &Request) -> Option<Status> {
    match crate::hostcli::send(req) {
        Ok(r) => Some(r.status),
        Err(e) => {
            log::warn!("paddock portal: {e}");
            None
        }
    }
}

fn token(options: &HashMap<String, OwnedValue>) -> Option<String> {
    match options.get("handle_token").map(|v| &**v) {
        Some(Value::Str(s)) => Some(s.to_string()),
        _ => None,
    }
}

/// Counts one pending link request while it lives.
struct Slot(Arc<AtomicUsize>);

impl Slot {
    fn take(pending: &Arc<AtomicUsize>) -> Option<Slot> {
        let mut n = pending.load(Ordering::SeqCst);
        loop {
            if n >= MAX_PENDING_LINKS {
                return None;
            }
            match pending.compare_exchange(n, n + 1, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => return Some(Slot(pending.clone())),
                Err(now) => n = now,
            }
        }
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

struct OpenUri {
    next: AtomicU32,
    pending: Arc<AtomicUsize>,
}

impl OpenUri {
    /// Return the request handle at once, and send the Response signal when
    /// the broker answers. Clients subscribe to the signal on the request
    /// path before they call, so the order of reply and signal does not
    /// matter.
    fn start(
        &self,
        conn: &zbus::Connection,
        sender: Option<&str>,
        options: &HashMap<String, OwnedValue>,
        uri: Option<String>,
    ) -> OwnedObjectPath {
        let n = self.next.fetch_add(1, Ordering::SeqCst);
        let sender = sender.unwrap_or_default().to_string();
        let path = request_path(&sender, token(options).as_deref(), n);
        let handle = OwnedObjectPath::try_from(path.clone())
            .unwrap_or_else(|_| OwnedObjectPath::try_from(PORTAL_PATH).unwrap());
        let conn = zbus::blocking::Connection::from(conn.clone());
        let slot = uri.as_ref().and_then(|_| Slot::take(&self.pending));
        std::thread::spawn(move || {
            let code = match (uri, slot) {
                (Some(u), Some(_slot)) => send(&request(
                    "open-url",
                    &[("url", &u)],
                    "the app asked to open a link",
                ))
                .map(|s| response_code(&s))
                .unwrap_or(RESPONSE_OTHER),
                (Some(_), None) => {
                    log::warn!("paddock portal: too many links wait for an answer; refused");
                    RESPONSE_OTHER
                }
                (None, _) => RESPONSE_OTHER,
            };
            let results: HashMap<&str, Value> = HashMap::new();
            let _ = conn.emit_signal(
                (!sender.is_empty()).then_some(sender.as_str()),
                path.as_str(),
                "org.freedesktop.portal.Request",
                "Response",
                &(code, results),
            );
        });
        handle
    }
}

#[zbus::interface(name = "org.freedesktop.portal.OpenURI")]
impl OpenUri {
    #[zbus(name = "OpenURI")]
    fn open_uri(
        &self,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(header)] hdr: zbus::message::Header<'_>,
        _parent_window: &str,
        uri: &str,
        options: HashMap<String, OwnedValue>,
    ) -> OwnedObjectPath {
        let sender = hdr.sender().map(|s| s.to_string());
        let uri = openable(uri).then(|| uri.to_string());
        if uri.is_none() {
            log::warn!("paddock portal: refused a link that is not http or https");
        }
        self.start(conn, sender.as_deref(), &options, uri)
    }

    /// A file in the sandbox: the host cannot open it.
    fn open_file(
        &self,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(header)] hdr: zbus::message::Header<'_>,
        _parent_window: &str,
        _fd: zbus::zvariant::OwnedFd,
        options: HashMap<String, OwnedValue>,
    ) -> OwnedObjectPath {
        let sender = hdr.sender().map(|s| s.to_string());
        self.start(conn, sender.as_deref(), &options, None)
    }

    fn open_directory(
        &self,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(header)] hdr: zbus::message::Header<'_>,
        _parent_window: &str,
        _fd: zbus::zvariant::OwnedFd,
        options: HashMap<String, OwnedValue>,
    ) -> OwnedObjectPath {
        let sender = hdr.sender().map(|s| s.to_string());
        self.start(conn, sender.as_deref(), &options, None)
    }

    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        4
    }
}

struct Notifications {
    next: AtomicU32,
    queue: SyncSender<(String, String)>,
}

#[zbus::interface(name = "org.freedesktop.Notifications")]
impl Notifications {
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        _app_name: &str,
        replaces_id: u32,
        _app_icon: &str,
        summary: &str,
        body: &str,
        _actions: Vec<String>,
        _hints: HashMap<String, OwnedValue>,
        _expire_timeout: i32,
    ) -> u32 {
        let (title, body) = notification_text(summary, body);
        if self.queue.try_send((title, body)).is_err() {
            log::warn!("paddock portal: too many notifications; dropped one");
        }
        if replaces_id != 0 {
            replaces_id
        } else {
            self.next.fetch_add(1, Ordering::SeqCst) + 1
        }
    }

    fn close_notification(&self, _id: u32) {}

    fn get_capabilities(&self) -> Vec<String> {
        vec!["body".into()]
    }

    fn get_server_information(&self) -> (String, String, String, String) {
        (
            "paddock".into(),
            "paddock".into(),
            env!("CARGO_PKG_VERSION").into(),
            "1.2".into(),
        )
    }
}

/// The title and body of a notification for the host: plain text of a sane
/// length, without control characters, and never starting with "-", which
/// the host's notify-send would read as an option.
pub fn notification_text(summary: &str, body: &str) -> (String, String) {
    fn clean(s: &str, max: usize) -> String {
        let t: String = s
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .take(max)
            .collect();
        t.trim().trim_start_matches('-').trim_start().to_string()
    }
    let mut title = clean(summary, 200);
    if title.is_empty() {
        title = "notification".into();
    }
    (title, clean(body, 1000))
}

/// Serve the portal and the notification service on the session bus, until
/// the bus goes away.
pub fn serve() -> Result<()> {
    let (tx, rx) = sync_channel::<(String, String)>(MAX_QUEUED_NOTIFICATIONS);
    std::thread::spawn(move || {
        for (title, body) in rx {
            let mut args = vec![("title", title.as_str())];
            if !body.is_empty() {
                args.push(("body", body.as_str()));
            }
            send(&request("notify", &args, "the app shows a notification"));
        }
    });
    let conn = zbus::blocking::connection::Builder::session()?
        .name(PORTAL_NAME)?
        .name(NOTIFY_NAME)?
        .serve_at(
            PORTAL_PATH,
            OpenUri {
                next: AtomicU32::new(0),
                pending: Arc::new(AtomicUsize::new(0)),
            },
        )?
        .serve_at(
            NOTIFY_PATH,
            Notifications {
                next: AtomicU32::new(0),
                queue: tx,
            },
        )?
        .build()?;
    // Exit with the bus: dbus-daemon closes the connection when it stops.
    let proxy = zbus::blocking::fdo::DBusProxy::new(&conn)?;
    let mut changes = proxy.receive_name_owner_changed()?;
    while changes.next().is_some() {}
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_paths_follow_the_portal_rules() {
        assert_eq!(
            request_path(":1.42", Some("gtk1234"), 0),
            "/org/freedesktop/portal/desktop/request/1_42/gtk1234"
        );
        // A token that is not a path element is replaced.
        assert_eq!(
            request_path(":1.7", Some("../x"), 3),
            "/org/freedesktop/portal/desktop/request/1_7/paddock3"
        );
        assert_eq!(
            request_path(":1.7", None, 5),
            "/org/freedesktop/portal/desktop/request/1_7/paddock5"
        );
        assert!(
            zbus::zvariant::ObjectPath::try_from(request_path(":1.9", Some("a_B9"), 0)).is_ok()
        );
    }

    #[test]
    fn only_web_links_go_to_the_host() {
        assert!(openable("https://claude.ai/login?x=1"));
        assert!(openable("http://127.0.0.1:8123/cb"));
        assert!(!openable("file:///etc/passwd"));
        assert!(!openable("claude://x"));
        assert!(!openable("https://a.example/\nb"));
        assert!(!openable(&format!(
            "https://a.example/{}",
            "a".repeat(9000)
        )));
    }

    #[test]
    fn broker_answers_map_to_portal_codes() {
        assert_eq!(response_code(&Status::Done), RESPONSE_OK);
        assert_eq!(response_code(&Status::Denied), RESPONSE_CANCELLED);
        assert_eq!(response_code(&Status::Disabled), RESPONSE_OTHER);
        let r = request("open-url", &[("url", "https://x.example")], "why");
        assert_eq!(r.cap, "open-url");
        assert_eq!(r.args["url"], "https://x.example");
    }

    #[test]
    fn notification_text_is_safe_for_notify_send() {
        assert_eq!(
            notification_text("--wait", "-u critical\nmore"),
            ("wait".to_string(), "u critical more".to_string())
        );
        assert_eq!(notification_text("", "").0, "notification");
        assert_eq!(notification_text(&"x".repeat(500), "").0.len(), 200);
    }

    #[test]
    fn link_requests_are_limited() {
        let p = Arc::new(AtomicUsize::new(0));
        let slots: Vec<_> = (0..MAX_PENDING_LINKS)
            .filter_map(|_| Slot::take(&p))
            .collect();
        assert_eq!(slots.len(), MAX_PENDING_LINKS);
        assert!(Slot::take(&p).is_none());
        drop(slots);
        assert!(Slot::take(&p).is_some());
    }
}
