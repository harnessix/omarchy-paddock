//! The mark of protected windows on macOS (`paddock mark`).
//!
//! Each window of an app sandbox gets a thin frame in a trust color, a
//! badge on demand, and an entry in a menu bar item. The frame is a separate
//! click-through window above the app window (mark/macos.rs). This file
//! holds the parts that need no screen: colors, badge text, window
//! matching and geometry. They are tested on every platform.

use crate::env::HostEnv;
use crate::state::Running;
use std::collections::{BTreeMap, BTreeSet};

#[cfg(target_os = "macos")]
mod macos;

/// The width of the frame stroke, in points.
pub const FRAME_WIDTH: f64 = 2.0;
/// The corner radius of a standard window, in points.
pub const WINDOW_RADIUS: f64 = 12.0;
/// Frame opacity of a window that does not have focus: 0x88, as on Omarchy.
pub const DIM_ALPHA: f64 = 136.0 / 255.0;
/// The badge shows this long after its window gets focus, in seconds.
pub const BADGE_FOCUS_SECS: f64 = 4.0;
/// The badge shows while the pointer is this close to it, in points.
pub const BADGE_HOVER_MARGIN: f64 = 18.0;
/// Space between the badge and the window corner, in points.
pub const BADGE_INSET: f64 = 12.0;
/// Windows smaller than this are menus, tooltips or helpers: no mark.
pub const MIN_WINDOW: f64 = 60.0;

/// What the color of a mark says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// Traffic goes only through paddock's proxy, or there is no network.
    Sage,
    /// A wider network.
    Amber,
}

impl Tone {
    /// The tone of a network mode, as `Running::network` names it. A mode
    /// that paddock does not know gets amber.
    pub fn of(network: &str) -> Tone {
        match network {
            "none" | "api" | "egress" => Tone::Sage,
            _ => Tone::Amber,
        }
    }

    /// sRGB components from 0 to 1.
    pub fn rgb(self) -> (f64, f64, f64) {
        match self {
            // #9FB8A0, the Omarchy base color.
            Tone::Sage => (159.0 / 255.0, 184.0 / 255.0, 160.0 / 255.0),
            // #E0A33A, the Omarchy base color.
            Tone::Amber => (224.0 / 255.0, 163.0 / 255.0, 58.0 / 255.0),
        }
    }

    /// What the tone means, for the details.
    pub fn meaning(self) -> &'static str {
        match self {
            Tone::Sage => "only through paddock's proxy",
            Tone::Amber => "a wider network",
        }
    }
}

/// A short age: `<1m`, `42m`, `2h`, `3d`.
pub fn age(secs: u64) -> String {
    match secs {
        0..=59 => "<1m".into(),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86_399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

/// The badge text, the same as on Omarchy: `paddock · vscode · egress · 2h`.
/// `name` is the pasture, or else the profile.
pub fn badge_text(name: &str, network: &str, age_secs: u64) -> String {
    format!("paddock · {name} · {network} · {}", age(age_secs))
}

/// The name in the badge: the pasture the sandbox joined, or its profile.
pub fn badge_name(r: &Running) -> &str {
    r.pasture.as_deref().unwrap_or(&r.harness)
}

/// The last part of the project path.
pub fn project_name(r: &Running) -> String {
    r.project
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| r.project.display().to_string())
}

/// The lines of the details that a click on the badge shows.
pub fn details(r: &Running, now: u64) -> Vec<String> {
    let since = now.saturating_sub(r.started);
    let mut out = vec![
        badge_text(badge_name(r), &r.network, since),
        format!("Project: {}", r.project.display()),
        format!("Network: {}, {}", r.network, Tone::of(&r.network).meaning()),
        format!("Backend: {}", r.backend),
    ];
    if !r.auth.is_empty() {
        out.push(format!("Login: {}", r.auth));
    }
    out.push(format!("Started: {} ago", age(since)));
    out.push(format!("Sandbox: {}", r.id));
    out.push(format!("Process: {}", r.pid));
    out
}

/// The menu bar entry of one window. `nth` counts the windows of one
/// sandbox from 1, and `of` is their number.
pub fn menu_title(r: &Running, now: u64, nth: usize, of: usize) -> String {
    let mut s = format!(
        "{} · {} · {} · {}",
        r.harness,
        project_name(r),
        r.network,
        age(now.saturating_sub(r.started))
    );
    if of > 1 {
        s.push_str(&format!(" ({nth}/{of})"));
    }
    s
}

/// The running sandboxes that get a mark: app mode only. Agent runs live
/// in a terminal window, which paddock cannot tell apart from the other
/// windows of the terminal app.
pub fn app_runs(list: Vec<Running>) -> Vec<Running> {
    list.into_iter().filter(|r| r.gui == "app").collect()
}

/// A rectangle in global screen coordinates with the origin at the top
/// left of the main screen and y down, as the window server reports it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    pub fn new(x: f64, y: f64, w: f64, h: f64) -> Self {
        Self { x, y, w, h }
    }

    /// Larger by `d` on each side.
    pub fn outset(self, d: f64) -> Self {
        Self::new(self.x - d, self.y - d, self.w + 2.0 * d, self.h + 2.0 * d)
    }

    pub fn contains(self, px: f64, py: f64) -> bool {
        px >= self.x && px <= self.x + self.w && py >= self.y && py <= self.y + self.h
    }

    /// The same rectangle in AppKit coordinates: origin at the bottom left
    /// of the main screen, y up. `main_h` is the height of the main screen.
    pub fn flip(self, main_h: f64) -> Self {
        Self::new(self.x, main_h - self.y - self.h, self.w, self.h)
    }

    /// Close enough to count as the same window frame.
    pub fn near(self, o: Rect, tolerance: f64) -> bool {
        (self.x - o.x).abs() <= tolerance
            && (self.y - o.y).abs() <= tolerance
            && (self.w - o.w).abs() <= tolerance
            && (self.h - o.h).abs() <= tolerance
    }
}

/// The frame window of an app window: around it, outside its edge.
pub fn frame_rect(win: Rect) -> Rect {
    win.outset(FRAME_WIDTH)
}

/// Where the badge goes. Above the top right corner of the window, outside
/// it, when the screen has room there. Else just inside the bottom right
/// corner. `screens` are the visible areas of the screens (without the menu
/// bar and the Dock).
pub fn badge_rect(win: Rect, w: f64, h: f64, screens: &[Rect]) -> Rect {
    let x = win.x + win.w - w - BADGE_INSET;
    let above = Rect::new(x, win.y - FRAME_WIDTH - 4.0 - h, w, h);
    let top = screens
        .iter()
        .find(|s| s.contains(win.x + win.w / 2.0, win.y.max(s.y)))
        .map(|s| s.y)
        .unwrap_or(0.0);
    if above.y >= top {
        above
    } else {
        Rect::new(x, win.y + win.h - h - BADGE_INSET, w, h)
    }
}

/// Whether the badge shows: for a while after its window gets focus, and
/// while the pointer is near. `focused_for` is the time since the window got
/// focus, None without focus.
pub fn badge_visible(focused_for: Option<f64>, hover: bool) -> bool {
    hover || focused_for.is_some_and(|t| t < BADGE_FOCUS_SECS)
}

/// One window from the window server list.
#[derive(Debug, Clone, PartialEq)]
pub struct Win {
    /// The window number of the window server.
    pub id: u32,
    pub pid: u32,
    /// 0 for normal windows. Menus, panels and the Dock use other layers.
    pub layer: i64,
    pub bounds: Rect,
    pub alpha: f64,
}

/// Each process and its parent.
pub type ProcTable = BTreeMap<u32, u32>;

/// A process and all below it.
pub fn tree(procs: &ProcTable, root: u32) -> BTreeSet<u32> {
    let mut children: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    for (&pid, &ppid) in procs {
        if pid != ppid {
            children.entry(ppid).or_default().push(pid);
        }
    }
    let mut out = BTreeSet::from([root]);
    let mut todo = vec![root];
    while let Some(p) = todo.pop() {
        for &c in children.get(&p).into_iter().flatten() {
            if out.insert(c) {
                todo.push(c);
            }
        }
    }
    out
}

/// The windows that get a mark, in the order of the list (front first),
/// each with the index of its sandbox in `trees`. Only normal, visible
/// windows of a useful size count. `own` is the marker process: it never
/// marks its own windows.
pub fn match_windows(wins: &[Win], trees: &[BTreeSet<u32>], own: u32) -> Vec<(Win, usize)> {
    wins.iter()
        .filter(|w| {
            w.pid != own
                && w.layer == 0
                && w.alpha > 0.0
                && w.bounds.w >= MIN_WINDOW
                && w.bounds.h >= MIN_WINDOW
        })
        .filter_map(|w| {
            trees
                .iter()
                .position(|t| t.contains(&w.pid))
                .map(|i| (w.clone(), i))
        })
        .collect()
}

/// The window that has focus: the front normal window of the front app.
/// `wins` is the list front first.
pub fn focused_window(wins: &[Win], front_pid: Option<u32>) -> Option<u32> {
    let pid = front_pid?;
    wins.iter()
        .find(|w| w.pid == pid && w.layer == 0 && w.alpha > 0.0)
        .map(|w| w.id)
}

/// Start the marker in the background, unless one runs or the user turned
/// it off with `PADDOCK_MARK=0`. Never inside a sandbox. macOS only: other
/// platforms have their own window marks.
pub fn spawn(env: &HostEnv) {
    let off = env.var("PADDOCK").is_some() || env.var("PADDOCK_MARK") == Some("0");
    #[cfg(target_os = "macos")]
    if !off {
        macos::spawn(env);
    }
    #[cfg(not(target_os = "macos"))]
    let _ = off;
}

/// `paddock mark`: draw the marks until no app sandbox runs.
pub fn main(env: &HostEnv) -> anyhow::Result<i32> {
    #[cfg(target_os = "macos")]
    {
        macos::main(env)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = env;
        anyhow::bail!("paddock mark draws window marks on macOS only")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn running(network: &str, gui: &str) -> Running {
        serde_json::from_value(serde_json::json!({
            "id": "vscode-api-1a2b",
            "harness": "vscode",
            "project": "/work/api",
            "backend": "seatbelt",
            "network": network,
            "gui": gui,
            "pid": 4242,
            "started": 1000,
            "auth": "isolated",
        }))
        .unwrap()
    }

    #[test]
    fn proxy_only_modes_are_sage_and_the_rest_amber() {
        for n in ["none", "api", "egress"] {
            assert_eq!(Tone::of(n), Tone::Sage, "{n}");
        }
        for n in ["host", "allowlist", "", "something-new"] {
            assert_eq!(Tone::of(n), Tone::Amber, "{n}");
        }
        assert_ne!(Tone::Sage.rgb(), Tone::Amber.rgb());
    }

    #[test]
    fn ages_are_short() {
        assert_eq!(age(0), "<1m");
        assert_eq!(age(59), "<1m");
        assert_eq!(age(60), "1m");
        assert_eq!(age(3599), "59m");
        assert_eq!(age(7200), "2h");
        assert_eq!(age(86_399), "23h");
        assert_eq!(age(3 * 86_400 + 5), "3d");
    }

    #[test]
    fn badge_text_matches_omarchy() {
        assert_eq!(
            badge_text("vscode", "egress", 2 * 3600 + 10),
            "paddock · vscode · egress · 2h"
        );
    }

    #[test]
    fn the_badge_names_the_pasture_if_there_is_one() {
        let mut r = running("egress", "app");
        assert_eq!(badge_name(&r), "vscode");
        r.pasture = Some("api-team".into());
        assert_eq!(badge_name(&r), "api-team");
        assert_eq!(details(&r, 1000)[0], "paddock · api-team · egress · <1m");
    }

    #[test]
    fn details_name_the_sandbox() {
        let r = running("egress", "app");
        let d = details(&r, 1000 + 7200);
        assert_eq!(d[0], "paddock · vscode · egress · 2h");
        assert!(d.contains(&"Project: /work/api".to_string()));
        assert!(d.contains(&"Network: egress, only through paddock's proxy".to_string()));
        assert!(d.contains(&"Login: isolated".to_string()));
        assert!(d.contains(&"Sandbox: vscode-api-1a2b".to_string()));
        assert!(d.contains(&"Process: 4242".to_string()));
        let d = details(&running("host", "app"), 1000);
        assert!(d.contains(&"Network: host, a wider network".to_string()));
    }

    #[test]
    fn menu_titles_count_windows_only_when_there_are_several() {
        let r = running("api", "app");
        assert_eq!(menu_title(&r, 1100, 1, 1), "vscode · api · api · 1m");
        assert_eq!(menu_title(&r, 1100, 2, 3), "vscode · api · api · 1m (2/3)");
        let mut r = r;
        r.project = PathBuf::from("/");
        assert_eq!(project_name(&r), "/");
    }

    #[test]
    fn only_app_sandboxes_get_marks() {
        let list = vec![running("egress", "app"), running("egress", "none")];
        let apps = app_runs(list);
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].gui, "app");
    }

    #[test]
    fn the_tree_holds_all_descendants_and_nothing_else() {
        // launchd 1 -> 100 (sandbox) -> 101 -> 103, 100 -> 102. 200 is apart.
        let procs: ProcTable = [
            (1, 0),
            (100, 1),
            (101, 100),
            (102, 100),
            (103, 101),
            (200, 1),
        ]
        .into_iter()
        .collect();
        assert_eq!(tree(&procs, 100), BTreeSet::from([100, 101, 102, 103]));
        // A process that is gone from the table still counts as its own root.
        assert_eq!(tree(&procs, 999), BTreeSet::from([999]));
        // A loop in a broken table ends.
        let looped: ProcTable = [(5, 6), (6, 5)].into_iter().collect();
        assert_eq!(tree(&looped, 5), BTreeSet::from([5, 6]));
    }

    fn win(id: u32, pid: u32, layer: i64, w: f64) -> Win {
        Win {
            id,
            pid,
            layer,
            bounds: Rect::new(10.0, 50.0, w, w),
            alpha: 1.0,
        }
    }

    #[test]
    fn windows_match_by_process_tree() {
        let trees = vec![BTreeSet::from([100, 101]), BTreeSet::from([300])];
        let wins = vec![
            win(1, 101, 0, 800.0), // sandbox 0, from a child process
            win(2, 555, 0, 800.0), // not protected
            win(3, 100, 8, 800.0), // a menu layer
            win(4, 100, 0, 20.0),  // a tiny helper window
            win(5, 300, 0, 400.0), // sandbox 1
            win(6, 9, 0, 400.0),   // the marker itself
            Win {
                alpha: 0.0,
                ..win(7, 100, 0, 800.0)
            }, // invisible
        ];
        let m = match_windows(&wins, &trees, 9);
        let got: Vec<(u32, usize)> = m.iter().map(|(w, i)| (w.id, *i)).collect();
        assert_eq!(got, vec![(1, 0), (5, 1)]);
    }

    #[test]
    fn the_focused_window_is_the_front_window_of_the_front_app() {
        let wins = vec![
            win(1, 7, 25, 800.0),
            win(2, 5, 0, 800.0),
            win(3, 5, 0, 800.0),
        ];
        assert_eq!(focused_window(&wins, Some(5)), Some(2));
        assert_eq!(focused_window(&wins, Some(7)), None);
        assert_eq!(focused_window(&wins, None), None);
    }

    #[test]
    fn the_frame_sits_outside_the_window() {
        let f = frame_rect(Rect::new(100.0, 100.0, 800.0, 600.0));
        assert!(f.x < 100.0 && f.y < 100.0);
        assert!(f.x + f.w > 900.0 && f.y + f.h > 700.0);
        assert_eq!(f.w - 800.0, 2.0 * FRAME_WIDTH);
    }

    #[test]
    fn flipping_moves_the_origin_to_the_bottom() {
        let r = Rect::new(10.0, 20.0, 100.0, 50.0);
        assert_eq!(r.flip(1000.0), Rect::new(10.0, 930.0, 100.0, 50.0));
        assert_eq!(r.flip(1000.0).flip(1000.0), r);
    }

    #[test]
    fn the_badge_goes_above_the_window_when_there_is_room() {
        // A screen of 1440x900 with a 30 pt menu bar.
        let screens = [Rect::new(0.0, 30.0, 1440.0, 870.0)];
        let win = Rect::new(100.0, 200.0, 800.0, 500.0);
        let b = badge_rect(win, 220.0, 22.0, &screens);
        assert!(b.y + b.h < win.y, "above the window: {b:?}");
        assert_eq!(b.x + b.w, win.x + win.w - BADGE_INSET);
        // A window at the top of the screen: the badge goes inside, at the
        // bottom right, never over the menu bar.
        let full = Rect::new(0.0, 30.0, 1440.0, 870.0);
        let b = badge_rect(full, 220.0, 22.0, &screens);
        assert_eq!(b.y + b.h, full.y + full.h - BADGE_INSET);
        assert!(b.y > full.y);
    }

    #[test]
    fn the_badge_finds_the_screen_of_its_window() {
        // A second screen to the right, without a menu bar, top at 0.
        let screens = [
            Rect::new(0.0, 30.0, 1440.0, 870.0),
            Rect::new(1440.0, 0.0, 1920.0, 1080.0),
        ];
        let win = Rect::new(1500.0, 10.0, 800.0, 500.0);
        let b = badge_rect(win, 200.0, 22.0, &screens);
        assert!(b.y > win.y, "no room above on the second screen: {b:?}");
        let win = Rect::new(1500.0, 100.0, 800.0, 500.0);
        let b = badge_rect(win, 200.0, 22.0, &screens);
        assert!(b.y < win.y);
    }

    #[test]
    fn the_badge_shows_on_focus_for_a_while_and_on_hover() {
        assert!(badge_visible(Some(0.0), false));
        assert!(badge_visible(Some(BADGE_FOCUS_SECS - 0.1), false));
        assert!(!badge_visible(Some(BADGE_FOCUS_SECS), false));
        assert!(badge_visible(Some(60.0), true));
        assert!(badge_visible(None, true));
        assert!(!badge_visible(None, false));
    }

    #[test]
    fn near_frames_compare_with_a_tolerance() {
        let a = Rect::new(10.0, 10.0, 100.0, 100.0);
        assert!(a.near(Rect::new(11.0, 9.0, 101.0, 100.0), 2.0));
        assert!(!a.near(Rect::new(20.0, 10.0, 100.0, 100.0), 2.0));
    }
}
