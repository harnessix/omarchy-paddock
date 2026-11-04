use anyhow::{bail, Result};
use paddock::env::HostEnv;
use paddock::state::{self, Running};

pub(crate) fn windows(env: &HostEnv, json: bool, mark: bool, focus: Option<String>) -> Result<i32> {
    use paddock::windows;
    let light = windows::current_theme_is_light(&env.home);
    let (found, clients) = protected_windows(env);
    if let Some(address) = focus {
        if !windows::valid_address(&address) || !found.iter().any(|p| p.client.address == address) {
            bail!("{address} is not a protected window");
        }
        return Ok(if focus_address(&address) { 0 } else { 1 });
    }
    if mark {
        if let Some(clients) = &clients {
            mark_windows(env, &found, clients, light);
        }
    }
    if json {
        println!("{}", windows::to_json(&found, state::now(), light));
    } else if !mark {
        for p in &found {
            println!(
                "{:<28} {:<9} {:<16} {:<10} {}",
                p.sandbox,
                p.kind.as_str(),
                p.harness,
                p.network,
                p.client.address
            );
        }
    }
    Ok(0)
}

pub(crate) fn focus(env: &HostEnv, id: String, display: bool) -> Result<i32> {
    let Some(r) = Running::list(env).into_iter().find(|r| r.id == id) else {
        bail!("no running sandbox with id {id}")
    };
    let Some(found) = paddock::relay::hypr_clients() else {
        bail!("focus needs Hyprland (hyprctl was not found)");
    };
    let windows: Vec<(u32, String)> = found.into_iter().map(|c| (c.pid, c.address)).collect();
    let address_of = |pid: u32| {
        windows
            .iter()
            .find(|(p, _)| *p == pid)
            .map(|(_, a)| a.clone())
    };
    let host_pid = if r.host_pid > 0 { r.host_pid } else { r.pid };
    let target = if display {
        r.window_pid.and_then(address_of)
    } else if r.gui == "app" {
        // An app sandbox draws its own host windows: find the window whose
        // process descends from this paddock process.
        windows.iter().find_map(|(pid, address)| {
            let mut p = *pid;
            for _ in 0..64 {
                if p == host_pid {
                    return Some(address.clone());
                }
                match state::parent_pid(p) {
                    Some(q) if q > 1 => p = q,
                    _ => return None,
                }
            }
            None
        })
    } else {
        // Walk up from paddock to the first process that owns a window: the terminal.
        let mut pid = host_pid;
        let mut found = None;
        for _ in 0..32 {
            if let Some(a) = address_of(pid) {
                found = Some(a);
                break;
            }
            match state::parent_pid(pid) {
                Some(p) if p > 1 => pid = p,
                _ => break,
            }
        }
        found
    };
    let Some(address) = target else {
        bail!("no window found for {id}; it may run without a terminal window")
    };
    Ok(if focus_address(&address) { 0 } else { 1 })
}

/// list is None without Hyprland.
fn protected_windows(
    env: &HostEnv,
) -> (
    Vec<paddock::windows::Protected>,
    Option<Vec<paddock::windows::Client>>,
) {
    use paddock::windows;
    let runs: Vec<windows::Run> = Running::list(env)
        .iter()
        .map(windows::Run::from_running)
        .collect();
    if runs.is_empty() {
        // No run: read the windows only when --mark has borders to take off.
        let marked = env.paddock_run().join("window-marks.json").exists();
        return (vec![], marked.then(windows::hypr_clients).flatten());
    }
    let Some(clients) = windows::hypr_clients() else {
        return (vec![], None);
    };
    let ttys = runs.iter().filter_map(|r| r.tty).collect();
    let holders = windows::pty_holders(&ttys);
    let procs = windows::Procs {
        parent: &state::parent_pid,
        mnt_ns: &windows::mnt_ns,
        holders: &holders,
    };
    let found = windows::find(&runs, &clients, &procs);
    (found, Some(clients))
}

/// `hyprctl ARGS`: true when Hyprland took the command.
fn hyprctl(args: &[String]) -> bool {
    std::process::Command::new("hyprctl")
        .args(args)
        .output()
        .map(|o| {
            let t = String::from_utf8_lossy(&o.stdout).to_lowercase();
            o.status.success() && !t.contains("error") && !t.contains("not found")
        })
        .unwrap_or(false)
}

/// paddock's mark on protected windows: a border, sage or amber by network
/// level, set on each window only, at run time. Nothing in the Hyprland
/// configuration changes. A window that lost its run (a terminal after the
/// run ends) gets its own border back.
fn mark_windows(
    env: &HostEnv,
    found: &[paddock::windows::Protected],
    clients: &[paddock::windows::Client],
    light: bool,
) {
    use paddock::windows::{self, Level};
    use std::collections::{BTreeMap, HashSet};
    let file = env.paddock_run().join("window-marks.json");
    let before: BTreeMap<String, Level> = std::fs::read_to_string(&file)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let existing: HashSet<String> = clients.iter().map(|c| c.address.clone()).collect();
    let (marks, unmark) = windows::plan_marks(&before, found, &existing);
    let apply = |address: &str, level: Option<Level>| {
        let [lua, classic] = windows::border_commands(address, level, light);
        if hyprctl(&lua[0]) {
            hyprctl(&lua[1]);
            return;
        }
        // Hyprland before the Lua configuration. Debug only: the plugin runs
        // this every few seconds, so anything louder would fill the journal.
        log::debug!("window {address}: Lua set_prop failed, using setprop");
        if !hyprctl(&classic[0]) {
            log::debug!("window {address}: setprop failed too, no border");
        }
        hyprctl(&classic[1]);
    };
    for (address, level) in &marks {
        apply(address, Some(*level));
    }
    for address in &unmark {
        apply(address, None);
    }
    if marks != before {
        if marks.is_empty() {
            let _ = std::fs::remove_file(&file);
        } else if let Ok(t) = serde_json::to_string(&marks) {
            if let Err(e) = paddock::privfs::write(&file, t) {
                log::debug!("cannot save the window marks: {e:#}");
            }
        }
    }
}

/// Focus a window and raise it (Hyprland). Hyprland with a Lua
/// configuration takes the hl.dsp form; older versions take the classic
/// dispatcher. Try both, as Omarchy does.
fn focus_address(address: &str) -> bool {
    let ok = hyprctl(&[
        "dispatch".into(),
        format!("hl.dsp.focus({{ window = \"address:{address}\" }})"),
    ]) || hyprctl(&[
        "dispatch".into(),
        "focuswindow".into(),
        format!("address:{address}"),
    ]);
    if ok {
        // Focus alone can leave a floating window under others: raise it.
        let _ = hyprctl(&[
            "dispatch".into(),
            format!("hl.dsp.window.bring_to_top({{ window = \"address:{address}\" }})"),
        ]);
    }
    ok
}
