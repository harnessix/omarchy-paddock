use super::*;
use crate::env::HostEnv;
use std::path::Path;

pub fn human(b: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1000.0 && i + 1 < U.len() {
        v /= 1000.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

pub fn date(unix: u64) -> String {
    let d = crate::build::ymd(unix);
    format!("{}-{}-{}", &d[..4], &d[4..6], &d[6..])
}

/// "2026-11-02 15:05" in the local time zone (`TZ` or /etc/localtime).
pub fn local_time(unix: u64) -> String {
    clock(unix, utc_offset(unix))
}

/// "YYYY-MM-DD HH:MM" at `offset` seconds east of UTC.
pub fn clock(unix: u64, offset: i64) -> String {
    let t = (unix as i64 + offset).max(0) as u64;
    let secs = t % 86_400;
    format!("{} {:02}:{:02}", date(t), secs / 3600, (secs % 3600) / 60)
}

/// The offset of the local time zone from UTC at `unix`, in seconds.
fn utc_offset(unix: u64) -> i64 {
    let t = unix as libc::time_t;
    // SAFETY: localtime_r writes into `tm` only and reads TZ.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        return 0;
    }
    tm.tm_gmtoff as i64
}

fn ago(now: u64, t: u64) -> String {
    let days = now.saturating_sub(t) / DAY;
    match days {
        0 => "today".into(),
        1 => "yesterday".into(),
        n => format!("{n} days ago"),
    }
}

fn tilde(home: &str, p: &Path) -> String {
    let s = p.display().to_string();
    match s.strip_prefix(home) {
        Some(rest) if rest.starts_with('/') => format!("~{rest}"),
        _ => s,
    }
}

/// `paddock du` as text.
pub fn render_du(env: &HostEnv, inv: &Inventory, now: u64) -> String {
    let home = env.home.display().to_string();
    let mut t = String::new();
    render_du_homes(&mut t, &home, inv, now);
    render_du_classes(&mut t, inv);
    render_du_scratch(&mut t, &home, inv, now);
    render_du_snapshots(&mut t, &home, inv, now);
    render_du_images(&mut t, inv);
    render_du_build_cache(&mut t, inv);
    render_du_worktrees(&mut t, &home, inv);
    render_du_sessions(&mut t, inv, now);
    render_du_layers(&mut t, &home, inv);
    render_du_old_caches(&mut t, &home, inv);
    render_du_keyrings_and_notes(&mut t, env, inv);
    t
}

fn render_du_homes(t: &mut String, home: &str, inv: &Inventory, now: u64) {
    let kind = match inv.size_kind {
        SizeKind::BtrfsExclusive => "exclusive bytes, from btrfs filesystem du",
        SizeKind::Allocated => "allocated bytes, as du counts them",
    };
    let mut homes: Vec<&Home> = inv.homes.iter().collect();
    homes.sort_by_key(|h| std::cmp::Reverse(h.bytes));
    let total_homes: u64 = homes.iter().map(|h| h.bytes).sum();
    t.push_str(&format!(
        "Sandbox homes: {} in {} homes ({kind})\n",
        human(total_homes),
        homes.len()
    ));
    for h in &homes {
        let project = match (&h.project, h.project_exists) {
            (Some(p), Some(false)) => format!("{} (gone)", tilde(home, p)),
            (Some(p), _) => tilde(home, p),
            (None, _) => format!(
                "unknown ({}), last modified {}",
                name(&h.path),
                date(h.modified)
            ),
        };
        let used = match h.last_used {
            Some(u) => ago(now, u),
            None => "-".into(),
        };
        let mut flags = Vec::new();
        if h.running {
            flags.push("running");
        }
        if !h.profile {
            flags.push("profile gone");
        }
        if h.origin == OriginCheck::Mismatch {
            flags.push("origin file ignored");
        }
        let flags = if flags.is_empty() {
            String::new()
        } else {
            format!(" [{}]", flags.join(", "))
        };
        t.push_str(&format!(
            "  {:>9}  {:<12}  {} {}  {project}{flags}\n            deleting loses: {}\n",
            human(h.bytes),
            used,
            h.harness,
            h.auth,
            h.loses
        ));
    }
}

fn render_du_classes(t: &mut String, inv: &Inventory) {
    if !inv.classes.is_empty() {
        t.push_str("\nCaches in the homes, by class:\n");
        for c in &inv.classes {
            t.push_str(&format!(
                "  {:>9}  {} in {} folders\n",
                human(c.bytes),
                c.class.label(),
                c.folders
            ));
        }
        if !inv.tmpfs_caches {
            t.push_str("  tmpfs and gc for pure caches are off ([storage] tmpfs_caches = false)\n");
        }
    }
}

fn render_du_scratch(t: &mut String, home: &str, inv: &Inventory, now: u64) {
    if !inv.scratch.is_empty() {
        t.push_str(&format!(
            "\nScratch projects (agents that started in the home folder): {}\n",
            human(inv.scratch.iter().map(|c| c.bytes).sum())
        ));
        for c in &inv.scratch {
            t.push_str(&format!(
                "  {:>9}  {:<12}  {}  {}{}\n",
                human(c.bytes),
                ago(now, c.last_active),
                c.harness,
                tilde(home, &c.path),
                if c.running { " [running]" } else { "" }
            ));
        }
    }
}

fn render_du_snapshots(t: &mut String, home: &str, inv: &Inventory, now: u64) {
    if !inv.snapshots.is_empty() {
        t.push_str(&format!(
            "\nProject snapshots and undo trash (`paddock undo`): {}\n",
            human(inv.snapshots.iter().map(|c| c.bytes).sum())
        ));
        for c in &inv.snapshots {
            let what = match &c.project {
                Some(p) if !c.trash => format!(
                    "{} {}{}",
                    c.harness,
                    tilde(home, p),
                    if c.project_exists {
                        ""
                    } else {
                        " [project gone]"
                    }
                ),
                _ => format!("trash {}", tilde(home, &c.path)),
            };
            t.push_str(&format!(
                "  {:>9}  {:<12}  {what}\n",
                human(c.bytes),
                ago(now, c.started),
            ));
        }
    }
}

fn render_du_images(t: &mut String, inv: &Inventory) {
    let total_images: u64 = inv.images.iter().map(|i| i.unique.unwrap_or(i.bytes)).sum();
    t.push_str(&format!(
        "\nImages: {} in {} (the first number is each image's own part, which removing it frees)\n",
        human(total_images),
        inv.images.len()
    ));
    for i in &inv.images {
        let state = match (i.current, i.in_use) {
            (true, true) => "current, in use",
            (true, false) => "current",
            (false, true) => "old, in use",
            (false, false) => "old, unused",
        };
        t.push_str(&format!(
            "  {:>9}  {:<12}  {} {}  ({state}, {} in all)\n",
            human(i.unique.unwrap_or(i.bytes)),
            i.created,
            i.cli,
            i.tags.join(", "),
            human(i.bytes)
        ));
    }
}

fn render_du_build_cache(t: &mut String, inv: &Inventory) {
    let cache: u64 = inv.build_cache.iter().map(|r| r.bytes).sum();
    if !inv.build_cache.is_empty() {
        t.push_str(&format!(
            "\nBuild cache: {} in {} records of paddock base image builds\n",
            human(cache),
            inv.build_cache.len()
        ));
    }
}

fn render_du_worktrees(t: &mut String, home: &str, inv: &Inventory) {
    let total_wt: u64 = inv.worktrees.iter().map(|w| w.bytes).sum();
    t.push_str(&format!(
        "\nWorktrees: {} in {}\n",
        human(total_wt),
        inv.worktrees.len()
    ));
    for w in &inv.worktrees {
        let commits = match (&w.repo, w.unmerged) {
            (None, _) => "repository unknown".to_string(),
            (_, Some(n)) => format!("{n} unmerged commits"),
            (_, None) => "unmerged commits unknown".into(),
        };
        let dirty = match w.dirty {
            Some(true) => ", uncommitted changes",
            _ => "",
        };
        let running = if w.running { " [running]" } else { "" };
        t.push_str(&format!(
            "  {:>9}  {}  {}{commits}{dirty}{running}\n",
            human(w.bytes),
            tilde(home, &w.path),
            w.branch
                .as_ref()
                .map(|b| format!("{b}, "))
                .unwrap_or_default(),
        ));
    }
}

fn render_du_sessions(t: &mut String, inv: &Inventory, now: u64) {
    let sb: u64 = inv.sessions.iter().map(|s| s.bytes).sum();
    let old = inv
        .sessions
        .iter()
        .filter(|s| {
            !s.running && s.ended.unwrap_or(s.started) < now.saturating_sub(SESSION_DAYS * DAY)
        })
        .count();
    t.push_str(&format!(
        "\nSession records: {} in {} ({old} ended more than {SESSION_DAYS} days ago)\n",
        human(sb),
        inv.sessions.len()
    ));
}

fn render_du_layers(t: &mut String, home: &str, inv: &Inventory) {
    for l in &inv.layers {
        match &l.project {
            None => t.push_str(&format!(
                "Shared cache layer: {} in {}\n",
                human(l.bytes),
                tilde(home, &l.path)
            )),
            Some(p) => t.push_str(&format!(
                "Project cache layers: {} in {} ({}{})\n",
                human(l.bytes),
                tilde(home, &l.path),
                tilde(home, p),
                if l.orphan { ", gone" } else { "" }
            )),
        }
    }
}

fn render_du_old_caches(t: &mut String, home: &str, inv: &Inventory) {
    for c in &inv.old_caches {
        t.push_str(&format!(
            "Old cache: {} in {} (paddock uses {} now)\n",
            human(c.bytes),
            tilde(home, &c.path),
            c.replaced_by
        ));
    }
}

fn render_du_keyrings_and_notes(t: &mut String, env: &HostEnv, inv: &Inventory) {
    let orphan = inv.keyrings.iter().filter(|k| !k.home_exists).count();
    t.push_str(&format!(
        "Keyring passwords: {} ({orphan} without a home)\n",
        inv.keyrings.len()
    ));
    if let Some(l) = crate::borrow::du_line(env) {
        t.push_str(&l);
        t.push('\n');
    }
    for n in &inv.notes {
        t.push_str(&format!("note: {n}\n"));
    }
    t.push_str("\nTo see what paddock can remove: paddock gc\n");
}

/// `paddock gc` as text: the plan, or what `--yes` removed.
pub fn render_plan(items: &[Item], yes: bool) -> String {
    let mut t = String::new();
    if items.is_empty() {
        return "paddock gc: nothing to remove.\n".into();
    }
    if !yes {
        t.push_str("paddock gc: dry run. Nothing was removed.\n\n");
    }
    let mut selected = 0;
    let mut more = 0;
    for i in items {
        let head = if i.selected() {
            selected += i.bytes;
            if yes {
                "remove".to_string()
            } else {
                "--yes removes".to_string()
            }
        } else {
            more += i.bytes;
            format!("needs {}", i.needs.join(" "))
        };
        t.push_str(&format!(
            "  {:>9}  {}  [{head}]\n            {}",
            human(i.bytes),
            i.what,
            i.reason
        ));
        if let Some(w) = &i.warning {
            t.push_str(&format!(". Warning: {w}"));
        }
        t.push('\n');
    }
    if !yes {
        t.push_str(&format!(
            "\n`paddock gc --yes` frees at least {}.",
            human(selected)
        ));
        if items
            .iter()
            .any(|i| matches!(i.action, Action::BuildCache { .. }))
        {
            t.push_str(" Build cache records that removed images still share free more space after the images are gone.");
        }
        if more > 0 {
            t.push_str(&format!(
                " The items that need more flags hold {} more.",
                human(more)
            ));
        }
        t.push('\n');
    }
    t
}
