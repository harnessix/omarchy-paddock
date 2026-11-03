//! Protected windows: the host windows that show a paddock sandbox.
//!
//! Three kinds of window count:
//! - an app sandbox window: its process descends from the paddock process
//!   that runs the sandbox;
//! - the nested display of window mode (`window_pid`);
//! - the terminal window of a terminal run: the window of the process that
//!   holds the master side of the run's pty (the `tty-index` of `/dev/ptmx`
//!   in `/proc/PID/fdinfo`, as in relay.rs).
//!
//! A run that another sandbox started, a detached run and a relayed run have
//! no terminal window of their own, so they mark nothing.
//!
//! paddock marks each window with a border at run time (Hyprland). The color
//! tells the network level: sage when traffic goes only through the proxy,
//! amber for a wider network. The Hyprland configuration does not change.
//! When a run ends, paddock takes its border off the terminal window, which
//! stays open after the run.

use crate::state::Running;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

/// How far the network of a sandbox reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    /// No network, or only through paddock's proxy: none, api, egress.
    Proxy,
    /// A wider network: host, allowlist, or a mode that paddock does not know.
    Wide,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Proxy => "proxy",
            Level::Wide => "wide",
        }
    }

    /// Hyprland border colors (active, inactive). Light themes get deeper
    /// tones, so the border keeps its contrast.
    pub fn border(self, light: bool) -> (&'static str, &'static str) {
        match (self, light) {
            (Level::Proxy, false) => ("rgb(9fb8a0) rgb(7fd29a) 45deg", "rgba(9fb8a088)"),
            (Level::Proxy, true) => ("rgb(4f7a55) rgb(3f8f5a) 45deg", "rgba(4f7a5599)"),
            (Level::Wide, false) => ("rgb(e0a33a) rgb(f2c46d) 45deg", "rgba(e0a33a88)"),
            (Level::Wide, true) => ("rgb(a8701a) rgb(c4861f) 45deg", "rgba(a8701a99)"),
        }
    }
}

/// The level of a network mode. An unknown mode counts as wide: a wrong
/// "protected" signal is worse than a careful one.
pub fn level(network: &str) -> Level {
    match network {
        "none" | "api" | "egress" => Level::Proxy,
        _ => Level::Wide,
    }
}

/// A Hyprland window, from `hyprctl clients -j`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Client {
    pub pid: u32,
    pub address: String,
    pub at: (i64, i64),
    pub size: (i64, i64),
    pub workspace: String,
    pub focused: bool,
    pub title: String,
    pub class: String,
}

/// Parse `hyprctl clients -j`. None when the answer is not a window list.
pub fn parse_clients(json: &[u8]) -> Option<Vec<Client>> {
    let list: Vec<serde_json::Value> = serde_json::from_slice(json).ok()?;
    let pair = |v: &serde_json::Value| -> (i64, i64) {
        let a = v.as_array();
        let n = |i: usize| {
            a.and_then(|a| a.get(i))
                .and_then(|x| x.as_i64())
                .unwrap_or(0)
        };
        (n(0), n(1))
    };
    let text = |v: Option<&serde_json::Value>, max: usize| -> String {
        v.and_then(|v| v.as_str())
            .unwrap_or("")
            .chars()
            .filter(|c| !c.is_control())
            .take(max)
            .collect()
    };
    Some(
        list.iter()
            .filter_map(|c| {
                Some(Client {
                    pid: u32::try_from(c.get("pid")?.as_i64()?).ok()?,
                    address: c.get("address")?.as_str()?.to_string(),
                    at: pair(c.get("at")?),
                    size: pair(c.get("size")?),
                    workspace: text(c.get("workspace").and_then(|w| w.get("name")), 64),
                    focused: c.get("focusHistoryID").and_then(|f| f.as_i64()) == Some(0),
                    title: text(c.get("title"), 200),
                    class: text(c.get("class"), 64),
                })
            })
            .collect(),
    )
}

/// The Hyprland windows. None without Hyprland.
pub fn hypr_clients() -> Option<Vec<Client>> {
    let out = std::process::Command::new("hyprctl")
        .args(["clients", "-j"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_clients(&out.stdout)
}

/// What paddock needs to know about a running sandbox to find its windows.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Run {
    pub id: String,
    pub harness: String,
    /// The pasture, or else the harness: the badge's profile word.
    pub label: String,
    pub network: String,
    /// An app sandbox: its windows descend from `host_pid`.
    pub app: bool,
    pub host_pid: u32,
    pub started: u64,
    pub window_pid: Option<u32>,
    /// The pts number of the run's terminal, for a terminal run that a
    /// person watches. None for app, spawned, detached and relayed runs.
    pub tty: Option<u32>,
}

impl Run {
    pub fn from_running(r: &Running) -> Run {
        let host_pid = if r.host_pid > 0 { r.host_pid } else { r.pid };
        let app = r.gui == "app";
        let terminal = !app && r.parent.is_none() && r.watched && !r.relay;
        Run {
            id: r.id.clone(),
            harness: r.harness.clone(),
            label: r.pasture.clone().unwrap_or_else(|| r.harness.clone()),
            network: r.network.clone(),
            app,
            host_pid,
            started: r.started,
            window_pid: r.window_pid,
            tty: if terminal { pts_of(host_pid) } else { None },
        }
    }
}

/// The pts number of the standard input of `pid`, if it is a pty.
fn pts_of(pid: u32) -> Option<u32> {
    let link = std::fs::read_link(format!("/proc/{pid}/fd/0")).ok()?;
    link.to_str()?.strip_prefix("/dev/pts/")?.parse().ok()
}

/// The kind of a protected window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    App,
    Display,
    Terminal,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::App => "app",
            Kind::Display => "display",
            Kind::Terminal => "terminal",
        }
    }
}

/// A window that shows a sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Protected {
    pub sandbox: String,
    pub harness: String,
    pub label: String,
    pub network: String,
    pub level: Level,
    pub kind: Kind,
    pub started: u64,
    pub client: Client,
}

/// What `find` reads about host processes. Tests give fakes.
pub struct Procs<'a> {
    /// The parent of a process.
    pub parent: &'a dyn Fn(u32) -> Option<u32>,
    /// The mount namespace of a process (`/proc/PID/ns/mnt`).
    pub mnt_ns: &'a dyn Fn(u32) -> Option<String>,
    /// The process that holds the master of each pts.
    pub holders: &'a HashMap<u32, u32>,
}

/// The mount namespace of a process, from the `/proc/PID/ns/mnt` link.
pub fn mnt_ns(pid: u32) -> Option<String> {
    std::fs::read_link(format!("/proc/{pid}/ns/mnt"))
        .ok()
        .map(|l| l.to_string_lossy().into_owned())
}

/// Match windows to runs. When in doubt, a window is not protected: a wrong
/// "protected" signal is worse than a missing one.
///
/// - A display window: its process is the run's `window_pid`.
/// - An app window: its process descends from the run's paddock process and
///   runs in another mount namespace than that paddock process. Host
///   programs that a host helper started from inside the app's process tree
///   (for example the gateway daemon) stay in the host namespace.
/// - A terminal window: its process holds the master of the run's pty, and
///   owns no other window. One process that draws several terminal windows
///   (Ghostty, `foot --server`, kitty single instance) cannot tell which
///   window holds the pty, so none of its windows counts.
///
/// One window shows one sandbox: an app or display window first, then the
/// newest terminal run in that terminal.
pub fn find(runs: &[Run], clients: &[Client], procs: &Procs) -> Vec<Protected> {
    let mut out = Vec::new();
    let make = |r: &Run, kind: Kind, c: &Client| Protected {
        sandbox: r.id.clone(),
        harness: r.harness.clone(),
        label: r.label.clone(),
        network: r.network.clone(),
        level: level(&r.network),
        kind,
        started: r.started,
        client: c.clone(),
    };
    for c in clients {
        if let Some(r) = runs.iter().find(|r| r.window_pid == Some(c.pid)) {
            out.push(make(r, Kind::Display, c));
            continue;
        }
        let apps: Vec<&Run> = runs.iter().filter(|r| r.app && r.host_pid > 1).collect();
        if let Some(r) = owner(c.pid, &apps, procs.parent) {
            let host = (procs.mnt_ns)(r.host_pid);
            let own = (procs.mnt_ns)(c.pid);
            if host.is_some() && own.is_some() && host != own {
                out.push(make(r, Kind::App, c));
            }
            continue;
        }
        if clients.iter().filter(|o| o.pid == c.pid).count() != 1 {
            continue;
        }
        let terminal = runs
            .iter()
            .filter(|r| r.tty.and_then(|t| procs.holders.get(&t)) == Some(&c.pid))
            .max_by_key(|r| r.started);
        if let Some(r) = terminal {
            out.push(make(r, Kind::Terminal, c));
        }
    }
    out
}

/// The app run whose paddock process is `pid` or one of its parents.
fn owner<'a>(pid: u32, apps: &[&'a Run], parent: &dyn Fn(u32) -> Option<u32>) -> Option<&'a Run> {
    if apps.is_empty() {
        return None;
    }
    let mut p = pid;
    for _ in 0..64 {
        if let Some(r) = apps.iter().find(|r| r.host_pid == p) {
            return Some(r);
        }
        match parent(p) {
            Some(q) if q > 1 && q != p => p = q,
            _ => return None,
        }
    }
    None
}

/// For each pts in `wanted`: the process that holds its master, from the
/// `tty-index` of `/dev/ptmx` in `/proc/PID/fdinfo`. Only processes of this
/// user are readable, which is where a terminal emulator is.
pub fn pty_holders(wanted: &HashSet<u32>) -> HashMap<u32, u32> {
    let mut found = HashMap::new();
    if wanted.is_empty() {
        return found;
    }
    let Ok(procs) = std::fs::read_dir("/proc") else {
        return found;
    };
    for entry in procs.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
            continue;
        };
        for fd in fds.flatten() {
            let Ok(target) = std::fs::read_link(fd.path()) else {
                continue;
            };
            if target != Path::new("/dev/ptmx") && target != Path::new("/dev/pts/ptmx") {
                continue;
            }
            let info = std::fs::read_to_string(format!(
                "/proc/{pid}/fdinfo/{}",
                fd.file_name().to_string_lossy()
            ))
            .unwrap_or_default();
            if let Some(i) = tty_index(&info) {
                if wanted.contains(&i) {
                    found.entry(i).or_insert(pid);
                }
            }
        }
        if found.len() == wanted.len() {
            break;
        }
    }
    found
}

/// The `tty-index` line of an fdinfo file.
fn tty_index(fdinfo: &str) -> Option<u32> {
    fdinfo
        .lines()
        .find_map(|l| l.strip_prefix("tty-index:"))
        .and_then(|v| v.trim().parse().ok())
}

/// A run time as 45s, 20m, 2h or 3d.
pub fn age(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86400),
    }
}

/// The badge text: `paddock · dev · egress · 2h`.
pub fn badge(p: &Protected, now: u64) -> String {
    format!(
        "paddock · {} · {} · {}",
        p.label,
        p.network,
        age(now.saturating_sub(p.started))
    )
}

/// The `paddock windows --json` answer.
pub fn to_json(list: &[Protected], now: u64, light: bool) -> serde_json::Value {
    let items: Vec<serde_json::Value> = list
        .iter()
        .map(|p| {
            serde_json::json!({
                "sandbox": p.sandbox,
                // The old name of the field, for older panels.
                "app": p.harness,
                "harness": p.harness,
                "label": p.label,
                "kind": p.kind.as_str(),
                "address": p.client.address,
                "network": p.network,
                "level": p.level.as_str(),
                "started": p.started,
                "badge": badge(p, now),
                "title": p.client.title,
                "class": p.client.class,
                "workspace": p.client.workspace,
                "focused": p.client.focused,
                "at": [p.client.at.0, p.client.at.1],
                "size": [p.client.size.0, p.client.size.1],
            })
        })
        .collect();
    serde_json::json!({
        "api_version": "paddock.windows/v1",
        "theme": if light { "light" } else { "dark" },
        "windows": items,
    })
}

/// True when the Omarchy theme is light: `mode = "light"` in its
/// `colors.toml`, or else a bright `background`.
pub fn theme_is_light(colors_toml: &str) -> bool {
    let value = |key: &str| {
        colors_toml.lines().find_map(|l| {
            let (k, v) = l.split_once('=')?;
            (k.trim() == key).then(|| v.trim().trim_matches('"').to_string())
        })
    };
    if let Some(m) = value("mode") {
        return m == "light";
    }
    let Some(bg) = value("background") else {
        return false;
    };
    let hex = bg.trim_start_matches('#');
    if hex.len() < 6 || !hex.is_char_boundary(6) {
        return false;
    }
    let ch = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).map(f64::from).ok();
    match (ch(0), ch(2), ch(4)) {
        (Some(r), Some(g), Some(b)) => 0.2126 * r + 0.7152 * g + 0.0722 * b > 140.0,
        _ => false,
    }
}

/// The theme of the current Omarchy theme folder. Dark when there is none.
pub fn current_theme_is_light(home: &Path) -> bool {
    std::fs::read_to_string(home.join(".local/state/omarchy/current/theme/colors.toml"))
        .map(|t| theme_is_light(&t))
        .unwrap_or(false)
}

/// The `hyprctl` argument lists that give a window its border, or take it
/// off (`level` None). The Lua form first, for Hyprland with a Lua
/// configuration, then the classic `setprop` form.
pub fn border_commands(address: &str, level: Option<Level>, light: bool) -> [Vec<Vec<String>>; 2] {
    let (active, inactive) = match level {
        Some(l) => l.border(light),
        None => ("unset", "unset"),
    };
    let lua = |prop: &str, value: &str| {
        vec![
            "dispatch".to_string(),
            format!("hl.dsp.window.set_prop({{ window = \"address:{address}\", prop = \"{prop}\", value = \"{value}\" }})"),
        ]
    };
    let classic = |prop: &str, value: &str| {
        vec![
            "dispatch".to_string(),
            "setprop".to_string(),
            format!("address:{address}"),
            prop.to_string(),
            value.to_string(),
        ]
    };
    [
        vec![
            lua("active_border_color", active),
            lua("inactive_border_color", inactive),
        ],
        vec![
            classic("activebordercolor", active),
            classic("inactivebordercolor", inactive),
        ],
    ]
}

/// What to change since the last mark: the windows to give a border (all
/// protected ones, because a Hyprland reload drops window properties), and
/// the windows that lost protection and still exist, to take it off.
pub fn plan_marks(
    before: &BTreeMap<String, Level>,
    now: &[Protected],
    existing: &HashSet<String>,
) -> (BTreeMap<String, Level>, Vec<String>) {
    let mut marks = BTreeMap::new();
    for p in now {
        // A window with two runs keeps the wider level.
        let e = marks.entry(p.client.address.clone()).or_insert(p.level);
        if p.level == Level::Wide {
            *e = Level::Wide;
        }
    }
    let unmark = before
        .keys()
        .filter(|a| !marks.contains_key(*a) && existing.contains(*a))
        .cloned()
        .collect();
    (marks, unmark)
}

/// A valid Hyprland window address: `0x` and hex digits.
pub fn valid_address(a: &str) -> bool {
    a.strip_prefix("0x")
        .is_some_and(|h| !h.is_empty() && h.len() <= 16 && h.chars().all(|c| c.is_ascii_hexdigit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(pid: u32, address: &str) -> Client {
        Client {
            pid,
            address: address.into(),
            ..Default::default()
        }
    }

    fn run(id: &str, network: &str) -> Run {
        Run {
            id: id.into(),
            harness: "claude".into(),
            label: "claude".into(),
            network: network.into(),
            started: 100,
            ..Default::default()
        }
    }

    #[test]
    fn network_levels() {
        for n in ["none", "api", "egress"] {
            assert_eq!(level(n), Level::Proxy, "{n}");
        }
        for n in ["host", "allowlist", "lan", "", "something"] {
            assert_eq!(level(n), Level::Wide, "{n}");
        }
    }

    #[test]
    fn border_colors_differ_by_level_and_theme() {
        let all = [
            Level::Proxy.border(false),
            Level::Proxy.border(true),
            Level::Wide.border(false),
            Level::Wide.border(true),
        ];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a.0, b.0);
            }
        }
        // The sage of the existing border stays for dark themes.
        assert!(Level::Proxy.border(false).0.starts_with("rgb(9fb8a0)"));
    }

    #[test]
    fn parses_hyprctl_clients() {
        let json = br#"[
          {"address":"0x55aa","pid":42,"at":[10,40],"size":[800,600],
           "workspace":{"id":1,"name":"1"},"focusHistoryID":0,
           "title":"claude\u0007 ~/src","class":"Alacritty"},
          {"address":"0x77bb","pid":43,"at":[820,40],"size":[780,600],
           "workspace":{"id":2,"name":"2"},"focusHistoryID":3,"title":"x","class":"y"},
          {"address":"0x99","pid":-1,"at":[0,0],"size":[0,0]}
        ]"#;
        let c = parse_clients(json).unwrap();
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].pid, 42);
        assert_eq!(c[0].at, (10, 40));
        assert_eq!(c[0].size, (800, 600));
        assert!(c[0].focused);
        assert!(!c[1].focused);
        assert_eq!(c[0].title, "claude ~/src");
        assert_eq!(c[1].workspace, "2");
        assert!(parse_clients(b"{}").is_none());
    }

    #[test]
    fn finds_app_display_and_terminal_windows() {
        // Process tree: 10 paddock (app) -> 11 -> 12 (window).
        // 20 is the nested display of a window-mode run.
        // 30 is a terminal emulator that holds pts 7 of a terminal run.
        // 40 is an unrelated window.
        let parents: HashMap<u32, u32> = [(12, 11), (11, 10), (10, 1), (30, 1), (40, 1)].into();
        let parent = |p: u32| parents.get(&p).copied();
        let mut app = run("app-1", "egress");
        app.app = true;
        app.host_pid = 10;
        let mut display = run("win-1", "host");
        display.window_pid = Some(20);
        display.host_pid = 21;
        let mut term = run("term-1", "api");
        term.host_pid = 31;
        term.tty = Some(7);
        let holders: HashMap<u32, u32> = [(7, 30)].into();
        let clients = [
            client(12, "0x1"),
            client(20, "0x2"),
            client(30, "0x3"),
            client(40, "0x4"),
        ];
        // The app's window runs in the sandbox's own mount namespace.
        let ns = |p: u32| Some(if p == 12 { "mnt:[2]" } else { "mnt:[1]" }.to_string());
        let procs = Procs {
            parent: &parent,
            mnt_ns: &ns,
            holders: &holders,
        };
        let found = find(&[app, display, term], &clients, &procs);
        let got: Vec<(&str, &str, Kind, Level)> = found
            .iter()
            .map(|p| {
                (
                    p.client.address.as_str(),
                    p.sandbox.as_str(),
                    p.kind,
                    p.level,
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![
                ("0x1", "app-1", Kind::App, Level::Proxy),
                ("0x2", "win-1", Kind::Display, Level::Wide),
                ("0x3", "term-1", Kind::Terminal, Level::Proxy),
            ]
        );
    }

    #[test]
    fn terminal_needs_the_pty_holder() {
        // The paddock process descends from a desktop app window (pid 50),
        // but the pty belongs to a background server (pid 51). The app
        // window must not look protected.
        let parents: HashMap<u32, u32> = [(60, 51), (51, 50), (50, 1)].into();
        let parent = |p: u32| parents.get(&p).copied();
        let mut term = run("t", "egress");
        term.host_pid = 60;
        term.tty = Some(3);
        let holders: HashMap<u32, u32> = [(3, 51)].into();
        let procs = Procs {
            parent: &parent,
            mnt_ns: &|_| None,
            holders: &holders,
        };
        assert!(find(&[term.clone()], &[client(50, "0x5")], &procs).is_empty());
        // Without a terminal (relay, detached, spawned): nothing either.
        term.tty = None;
        let holders: HashMap<u32, u32> = [(3, 50)].into();
        let procs = Procs {
            parent: &parent,
            mnt_ns: &|_| None,
            holders: &holders,
        };
        assert!(find(&[term], &[client(50, "0x5")], &procs).is_empty());
    }

    #[test]
    fn a_terminal_process_with_several_windows_marks_none() {
        // One Ghostty process (pid 30) draws the sandbox terminal and a
        // plain shell window. paddock cannot tell them apart.
        let mut term = run("t", "egress");
        term.tty = Some(7);
        let holders: HashMap<u32, u32> = [(7, 30)].into();
        let procs = Procs {
            parent: &|_| None,
            mnt_ns: &|_| None,
            holders: &holders,
        };
        let clients = [client(30, "0xsandbox"), client(30, "0xplainshell")];
        assert!(find(&[term.clone()], &clients, &procs).is_empty());
        // One window: it counts.
        assert_eq!(find(&[term], &clients[..1], &procs).len(), 1);
    }

    #[test]
    fn an_app_window_must_run_in_the_sandbox_namespace() {
        // A host helper (pid 13) that descends from the app's paddock
        // process (10), for example the gateway daemon, starts a host
        // program with a window (14). It stays in the host namespace.
        let parents: HashMap<u32, u32> = [(14, 13), (13, 10), (12, 10), (10, 1)].into();
        let parent = |p: u32| parents.get(&p).copied();
        let ns = |p: u32| match p {
            12 => Some("mnt:[2]".to_string()),
            99 => None,
            _ => Some("mnt:[1]".to_string()),
        };
        let mut app = run("app", "egress");
        app.app = true;
        app.host_pid = 10;
        let holders = HashMap::new();
        let procs = Procs {
            parent: &parent,
            mnt_ns: &ns,
            holders: &holders,
        };
        let found = find(
            &[app.clone()],
            &[client(12, "0xapp"), client(14, "0xhost")],
            &procs,
        );
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].client.address, "0xapp");
        // An unreadable namespace counts as not protected.
        let parents: HashMap<u32, u32> = [(99, 10)].into();
        let parent = |p: u32| parents.get(&p).copied();
        let procs = Procs {
            parent: &parent,
            mnt_ns: &ns,
            holders: &holders,
        };
        assert!(find(&[app], &[client(99, "0x99")], &procs).is_empty());
    }

    #[test]
    fn newest_run_in_a_terminal_wins() {
        let mut old = run("old", "egress");
        old.tty = Some(4);
        old.started = 10;
        let mut new = run("new", "host");
        new.tty = Some(4);
        new.started = 20;
        let holders: HashMap<u32, u32> = [(4, 9)].into();
        let procs = Procs {
            parent: &|_| None,
            mnt_ns: &|_| None,
            holders: &holders,
        };
        let found = find(&[old, new], &[client(9, "0x9")], &procs);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].sandbox, "new");
        assert_eq!(found[0].level, Level::Wide);
    }

    #[test]
    fn run_from_running_picks_terminal_runs_only() {
        let mut r: Running = serde_json::from_value(serde_json::json!({
            "id": "a", "harness": "claude", "project": "/p", "backend": "bwrap",
            "network": "egress", "gui": "none", "pid": 1, "started": 5,
            "host_pid": std::process::id(), "pasture": "dev", "watched": false
        }))
        .unwrap();
        let x = Run::from_running(&r);
        assert_eq!(x.label, "dev");
        assert_eq!(x.tty, None, "not watched: no terminal window");
        r.gui = "app".into();
        r.watched = true;
        let x = Run::from_running(&r);
        assert!(x.app);
        assert_eq!(x.tty, None);
        r.gui = "none".into();
        r.relay = true;
        assert_eq!(Run::from_running(&r).tty, None);
        r.relay = false;
        r.parent = Some("p".into());
        assert_eq!(Run::from_running(&r).tty, None);
    }

    #[test]
    fn ages_and_badge() {
        assert_eq!(age(45), "45s");
        assert_eq!(age(20 * 60 + 5), "20m");
        assert_eq!(age(2 * 3600 + 100), "2h");
        assert_eq!(age(3 * 86400), "3d");
        let p = Protected {
            sandbox: "s".into(),
            harness: "claude".into(),
            label: "dev".into(),
            network: "egress".into(),
            level: Level::Proxy,
            kind: Kind::Terminal,
            started: 1000,
            client: client(1, "0x1"),
        };
        assert_eq!(badge(&p, 1000 + 7300), "paddock · dev · egress · 2h");
        let v = to_json(&[p], 1060, true);
        assert_eq!(v["api_version"], "paddock.windows/v1");
        assert_eq!(v["theme"], "light");
        let w = &v["windows"][0];
        assert_eq!(w["sandbox"], "s");
        assert_eq!(w["app"], "claude");
        assert_eq!(w["level"], "proxy");
        assert_eq!(w["kind"], "terminal");
        assert_eq!(w["badge"], "paddock · dev · egress · 1m");
        assert_eq!(w["size"], serde_json::json!([0, 0]));
    }

    #[test]
    fn theme_mode() {
        assert!(theme_is_light("mode = \"light\"\nbackground = \"#000000\""));
        assert!(!theme_is_light("mode = \"dark\"\nbackground = \"#ffffff\""));
        assert!(theme_is_light("background = \"#E8E3C3\""));
        assert!(!theme_is_light("background = \"#101315\""));
        assert!(!theme_is_light("background = \"#ab\""));
        assert!(!theme_is_light(""));
        let home = tempfile::tempdir().unwrap();
        assert!(!current_theme_is_light(home.path()));
        let d = home.path().join(".local/state/omarchy/current/theme");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("colors.toml"), "mode = \"light\"\n").unwrap();
        assert!(current_theme_is_light(home.path()));
    }

    #[test]
    fn border_and_unset_commands() {
        let [lua, classic] = border_commands("0xab", Some(Level::Wide), false);
        assert!(lua[0][1].contains("address:0xab"));
        assert!(lua[0][1].contains("active_border_color"));
        assert!(lua[0][1].contains("rgb(e0a33a)"));
        assert!(lua[1][1].contains("inactive_border_color"));
        assert_eq!(
            classic[0][1..4],
            ["setprop", "address:0xab", "activebordercolor"]
        );
        let [lua, classic] = border_commands("0xab", None, false);
        assert!(lua[0][1].contains("value = \"unset\""));
        assert_eq!(classic[1][4], "unset");
    }

    #[test]
    fn marks_follow_the_runs() {
        let mut p = Protected {
            sandbox: "s".into(),
            harness: "h".into(),
            label: "h".into(),
            network: "egress".into(),
            level: Level::Proxy,
            kind: Kind::Terminal,
            started: 0,
            client: client(1, "0x1"),
        };
        let mut wide = p.clone();
        wide.level = Level::Wide;
        let before: BTreeMap<String, Level> = [
            ("0x1".into(), Level::Proxy),
            ("0x2".into(), Level::Proxy),
            ("0x3".into(), Level::Wide),
        ]
        .into();
        // 0x2 lost its run and is still open; 0x3 closed.
        let existing: HashSet<String> = ["0x1".into(), "0x2".into()].into();
        let (marks, unmark) = plan_marks(&before, &[p.clone(), wide], &existing);
        assert_eq!(marks.get("0x1"), Some(&Level::Wide));
        assert_eq!(unmark, vec!["0x2".to_string()]);
        p.client.address = "0x2".into();
        // 0x2 got a run again; 0x1 lost its run.
        let (_, unmark) = plan_marks(&before, &[p], &existing);
        assert_eq!(unmark, vec!["0x1".to_string()]);
    }

    #[test]
    fn addresses() {
        assert!(valid_address("0x55d0aa12"));
        assert!(!valid_address("55d0"));
        assert!(!valid_address("0x"));
        assert!(!valid_address("0x12\"; rm"));
    }

    #[cfg(target_os = "linux")] // the pty index and /proc are Linux only
    #[test]
    fn finds_the_holder_of_a_pty() {
        // SAFETY: plain libc calls on a new pty master that this test owns.
        let index = unsafe {
            let fd = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
            assert!(fd >= 0);
            assert_eq!(libc::grantpt(fd), 0);
            assert_eq!(libc::unlockpt(fd), 0);
            let mut n: libc::c_uint = 0;
            assert_eq!(libc::ioctl(fd, libc::TIOCGPTN, &mut n), 0);
            (fd, n)
        };
        let found = pty_holders(&[index.1].into());
        unsafe { libc::close(index.0) }; // SAFETY: the fd from above.
        assert_eq!(found.get(&index.1), Some(&std::process::id()));
        assert!(pty_holders(&HashSet::new()).is_empty());
    }

    #[test]
    fn fdinfo_tty_index() {
        assert_eq!(tty_index("pos:\t0\nflags:\t02\ntty-index:\t7\n"), Some(7));
        assert_eq!(tty_index("pos:\t0\n"), None);
    }
}
