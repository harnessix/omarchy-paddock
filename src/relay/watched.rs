//! Whether a person watches the caller's terminal, and what a run then
//! does with it.

/// One process in the chain from paddock up to init.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ancestor {
    pub pid: u32,
    pub comm: String,
}

/// A Hyprland window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HyprClient {
    pub pid: u32,
    pub address: String,
}

/// `hyprctl clients -j`. None when `hyprctl` is missing or its answer is not
/// a window list. An empty list means Hyprland is there and has no windows.
pub fn hypr_clients() -> Option<Vec<HyprClient>> {
    let out = std::process::Command::new("hyprctl")
        .args(["clients", "-j"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let clients: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).ok()?;
    Some(
        clients
            .iter()
            .filter_map(|c| {
                Some(HyprClient {
                    pid: u32::try_from(c.get("pid")?.as_u64()?).ok()?,
                    address: c.get("address")?.as_str()?.to_string(),
                })
            })
            .collect(),
    )
}

/// `pid` and then each parent. The parent comes from `/proc/PID/stat`.
/// The name comes from `/proc/PID/comm`. Stops at pid 1 or after 32 steps.
pub fn parent_chain(pid: u32) -> Vec<Ancestor> {
    let mut out = Vec::new();
    let mut cur = pid;
    for _ in 0..32 {
        if cur <= 1 {
            break;
        }
        out.push(Ancestor {
            pid: cur,
            comm: comm_of(cur),
        });
        match crate::state::parent_pid(cur) {
            Some(next) if next > 1 && next != cur => cur = next,
            _ => break,
        }
    }
    out
}

fn comm_of(pid: u32) -> String {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// True when a terminal run is watched by a person.
///
/// `windows` is None when there is no Hyprland window list. Any terminal
/// then counts as watched, which is the rule outside Omarchy.
///
/// `master` is the process that holds the other end of this terminal (the
/// pty master): a terminal emulator, a tmux server, or a background program
/// that gave the command a pty of its own. The terminal is visible when that
/// process owns a compositor window itself, or is a tmux server or `screen`.
/// Its parents do not count: a desktop app with a window can run a
/// background server that hands out ptys nobody sees (for example the Codex
/// app-server of the ChatGPT desktop app).
///
/// When the holder cannot be found, the parent chain decides as a fallback:
/// a pid in the chain that owns a window, or a tmux server or `screen`.
pub fn terminal_watched(
    windows: Option<&[u32]>,
    master: Option<&Ancestor>,
    chain: &[Ancestor],
) -> bool {
    let Some(windows) = windows else {
        return true;
    };
    match master {
        Some(m) => windows.contains(&m.pid) || session_client(&m.comm),
        None => chain
            .iter()
            .any(|a| windows.contains(&a.pid) || session_client(&a.comm)),
    }
}

/// The tmux server and GNU screen have no window of their own. A chain
/// through either one is a session the user attached to.
fn session_client(comm: &str) -> bool {
    let name = comm.trim();
    name == "tmux: server" || name == "screen"
}

/// The pts number of this process's terminal (stdin), if it is a pty.
fn own_pts_index() -> Option<u32> {
    let link = std::fs::read_link("/proc/self/fd/0").ok()?;
    link.to_str()?.strip_prefix("/dev/pts/")?.parse().ok()
}

/// The process that holds the master side of pts `index`: the one whose
/// open `/dev/ptmx` reports that index in its fdinfo (`tty-index`). Only
/// processes of this user are readable, which is where the holder is.
pub(super) fn pty_master_holder(index: u32) -> Option<Ancestor> {
    let want = format!("tty-index:\t{index}");
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
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
            if target != std::path::Path::new("/dev/ptmx")
                && target != std::path::Path::new("/dev/pts/ptmx")
            {
                continue;
            }
            let info = std::fs::read_to_string(format!(
                "/proc/{pid}/fdinfo/{}",
                fd.file_name().to_string_lossy()
            ))
            .unwrap_or_default();
            if info.lines().any(|l| l.trim_end() == want) {
                return Some(Ancestor {
                    pid,
                    comm: comm_of(pid),
                });
            }
        }
    }
    None
}

/// The decision for this process: the Hyprland window list, the holder of
/// its pty master, and the parent chain as a fallback.
pub fn this_terminal_is_watched() -> bool {
    let windows = hypr_clients().map(|c| c.into_iter().map(|w| w.pid).collect::<Vec<_>>());
    let master = own_pts_index().and_then(pty_master_holder);
    let chain = parent_chain(std::process::id());
    terminal_watched(windows.as_deref(), master.as_ref(), &chain)
}

/// How a run uses the caller's terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    /// A person watches the terminal (`terminal_watched`).
    pub watched: bool,
    /// paddock relays the terminal: nobody sees it (an invisible terminal).
    pub relay: bool,
    /// Nobody is at the keyboard, so every start screen rule can answer.
    pub unwatched: bool,
}

/// The plan of a run. `watched` runs only for a terminal.
///
/// A detached run is unwatched even when its terminal looks watched: the
/// pane is on paddock's own tmux server, so the holder of its pty is a tmux
/// server. A user who attaches with write access counts later
/// (screens.rs `prepare`).
pub fn plan(terminal: bool, watched: impl FnOnce() -> bool, detached: bool) -> Plan {
    let watched = terminal && watched();
    let relay = terminal && !watched;
    Plan {
        watched,
        relay,
        unwatched: relay || detached,
    }
}
