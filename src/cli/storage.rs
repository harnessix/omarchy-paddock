use anyhow::{bail, Result};
use paddock::env::HostEnv;
use paddock::state::{self, Running};
use std::path::{Path, PathBuf};

pub(crate) const FILES_HELP: &str = "The project is shared with the host, so its changes are already yours. \
Files that a sandbox saves elsewhere, for example to ~/Desktop or ~/Downloads, stay in its private home. \
Recovery folders and ignore rules are set in [recovery] of ~/.config/paddock/config.toml:

  [recovery]
  folders = [\"Desktop\", \"Documents\", \"Downloads\", \"Pictures\", \"Videos\", \"Music\"]
  ignore = [\"*.desktop\", \"*.part\"]";

pub(crate) const DU_HELP: &str =
    "On btrfs, sizes are exclusive bytes from `btrfs filesystem du`: data that \
another file shares (reflinks, snapshots) does not count. On other file systems, sizes are \
allocated bytes, as `du` counts them. The header names the kind.";

pub(crate) const GC_HELP: &str = "Without --yes, paddock gc only prints what it would remove.

--yes alone removes: paddock images that are not current and that no container uses, keyring
passwords whose home is gone, session records that ended more than 30 days ago, homes of
harnesses whose profile no longer exists, and old shared caches (mise-installs). paddock also does this after it builds a new image
(PADDOCK_GC_AFTER_BUILD=0 turns that off).

--homes adds homes whose project is gone or that were not used for --older-than days (90).
A desktop app keeps its login in its home: those homes also need --include-app-logins.
--worktrees adds paddock worktrees. A worktree with unmerged commits or uncommitted changes
also needs --force, and paddock keeps its branch.
--scratch adds scratch projects (agents that `paddock wrap` started in the home folder) that
were not used for --older-than days. Their files are lost.
--dedupe lets identical files in the sandbox homes share their data (btrfs, xfs, bcachefs).
The kernel compares the bytes first, and a later write to one copy gets its own blocks.
Without --yes it only counts the files and their size.

paddock never removes the home or worktree of a running sandbox, and never deletes outside
its own directories or through a symlink.

Examples:
  paddock gc                      what --yes would remove
  paddock gc --homes --older-than 30
  paddock gc --yes
  paddock gc --dedupe --yes       share identical data between homes";

fn session_for(env: &HostEnv, id: &str) -> Result<paddock::recover::Session> {
    paddock::recover::Session::load(env, id)
        .ok_or_else(|| anyhow::anyhow!("no sandbox with id {id}; `paddock files` lists them"))
}

pub(crate) fn du_cmd(env: &HostEnv, json: bool) -> Result<i32> {
    use paddock::storage;
    let containers = storage::RealContainers(env);
    let host = storage::Host::real(env, &containers);
    let mut inv = storage::scan(env, &host, true);
    if let Some(n) = storage::dedupe::du_note(env) {
        inv.notes.push(n);
    }
    if json {
        let mut v = serde_json::to_value(&inv)?;
        v["api_version"] = "paddock.du/v1".into();
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        print!("{}", storage::render_du(env, &inv, state::now()));
    }
    Ok(0)
}

pub(crate) fn gc_cmd(env: &HostEnv, o: &paddock::storage::GcOptions, json: bool) -> Result<i32> {
    use paddock::storage;
    let containers = storage::RealContainers(env);
    let host = storage::Host::real(env, &containers);
    let mut inv = storage::scan(env, &host, false);
    let mut items = storage::plan(&inv, o, state::now());
    storage::measure_plan(&mut inv, &mut items);
    items.extend(paddock::borrow::gc_items(env));
    items.extend(paddock::view::gc_items(env, state::now()));
    match paddock::kv::gc_items(env, state::now()) {
        Ok(kv) => items.extend(kv),
        Err(e) => inv
            .notes
            .push(format!("cannot read the paddock kv store: {e:#}")),
    }
    let removed = if o.yes {
        storage::apply(env, &containers, &items)
    } else {
        vec![]
    };
    let dedupe = o.dedupe.then(|| storage::dedupe::all(env, o.yes));
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "api_version": "paddock.gc/v1",
                "dry_run": !o.yes,
                "items": items,
                "removed": removed,
                "notes": inv.notes,
                "dedupe": dedupe,
            }))?
        );
        return Ok(i32::from(removed.iter().any(|r| r.error.is_some())));
    }
    for n in &inv.notes {
        eprintln!("paddock: {n}");
    }
    if !o.yes {
        print!("{}", storage::render_plan(&items, false));
        if let Some(d) = &dedupe {
            print!("{}", storage::dedupe::render(d, false));
        }
        return Ok(0);
    }
    let mut freed = 0;
    let mut failed = false;
    for r in &removed {
        match &r.error {
            None => {
                freed += r.bytes;
                let note = r
                    .note
                    .as_ref()
                    .map(|n| format!(", {n}"))
                    .unwrap_or_default();
                println!("removed {} ({}{note})", r.what, storage::human(r.bytes));
            }
            Some(e) => {
                failed = true;
                eprintln!("paddock: cannot remove {}: {e}", r.what);
            }
        }
    }
    let left: Vec<_> = items.iter().filter(|i| !i.selected()).collect();
    for i in &left {
        println!(
            "kept {} ({}): needs {}",
            i.what,
            storage::human(i.bytes),
            i.needs.join(" ")
        );
    }
    println!("paddock gc: freed {}.", storage::human(freed));
    if let Some(d) = &dedupe {
        print!("{}", storage::dedupe::render(d, true));
    }
    Ok(i32::from(failed))
}

/// Stop one sandbox and the sandboxes it started.
pub(crate) fn stop_one(env: &HostEnv, all: &[Running], id: &str) -> Result<i32> {
    let Some(r) = all.iter().find(|r| r.id == id) else {
        bail!("no running sandbox with id {id}")
    };
    // The sandboxes that this one started stop with it. Its own
    // paddock stops them too when it ends; this does not wait for that.
    let children = state::descendants(all, id);
    // A frozen process gets SIGTERM only when it thaws.
    let _ = paddock::freeze::thaw(env, id, true);
    for v in children.iter().map(|c| c.id.as_str()).chain([id]) {
        let _ = paddock::detach::mark_stopped(env, v);
    }
    for c in &children {
        unsafe { libc::kill(c.pid as i32, libc::SIGTERM) };
    }
    unsafe { libc::kill(r.pid as i32, libc::SIGTERM) };
    println!("Sent SIGTERM to {id}.");
    // The live views end with them.
    for v in children.iter().map(|c| c.id.as_str()).chain([id]) {
        paddock::view::end_session(env, v);
    }
    if !children.is_empty() {
        println!(
            "Sent SIGTERM to the sandboxes it started: {}.",
            children
                .iter()
                .map(|c| c.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(0)
}

pub(crate) fn files_cmd(env: &HostEnv, id: Option<&str>, json: bool) -> Result<i32> {
    use paddock::recover::{self, RecoveryCfg, Session};
    let cfg = RecoveryCfg::load(env)?;
    let sessions = match id {
        Some(id) => vec![session_for(env, id)?],
        None => Session::latest_per_home(env),
    };
    let mut out = Vec::new();
    for s in sessions.into_iter().take(32) {
        let items = recover::scan(env, &cfg, &s.home, s.started);
        let running = s.running(env);
        if id.is_none() && items.is_empty() && !running {
            continue;
        }
        let changes = recover::project_changes(env, &s);
        out.push((s, running, items, changes));
    }
    // Scratch projects: where agents from omarchy-agent work when they start
    // in the home folder (`paddock wrap`).
    let scratch: Vec<(String, PathBuf, usize)> = if id.is_none() {
        std::fs::read_dir(paddock::storage::scratch_root(env))
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .map(|e| {
                let n = std::fs::read_dir(e.path()).map(|d| d.count()).unwrap_or(0);
                (e.file_name().to_string_lossy().into_owned(), e.path(), n)
            })
            .collect()
    } else {
        vec![]
    };
    if json {
        let list: Vec<_> = out
            .iter()
            .map(|(s, running, items, changes)| {
                serde_json::json!({
                    "id": s.id, "harness": s.harness, "project": s.project, "home": s.home,
                    "started": s.started, "ended": s.ended, "running": running,
                    "files": items, "project_changes": changes,
                })
            })
            .collect();
        let sc: Vec<_> = scratch
            .iter()
            .map(|(h, p, n)| serde_json::json!({ "harness": h, "project": p, "entries": n }))
            .collect();
        println!(
            "{}",
            serde_json::json!({ "api_version": "paddock.files/v1", "sandboxes": list, "scratch": sc })
        );
        return Ok(0);
    }
    if !scratch.is_empty() {
        println!("Scratch projects (agents from omarchy-agent that started in the home folder work here):");
        for (h, p, n) in &scratch {
            println!("  {h:<12} {}  ({n} files and folders)", p.display());
        }
        println!();
    }
    if out.is_empty() {
        println!("No sandbox has files outside its project.");
        return Ok(0);
    }
    for (s, running, items, changes) in &out {
        println!(
            "{}  {} in {}{}",
            s.id,
            s.harness,
            s.project.display(),
            if *running { "  (running)" } else { "" }
        );
        if items.is_empty() {
            println!("  no files in the recovery folders");
        }
        for i in items {
            println!(
                "  {} {:<48} {:>9}  -> {}",
                if i.new { "new" } else { "   " },
                i.path,
                i.size,
                i.target.display()
            );
        }
        if changes.git {
            let paths = if changes.changed_unknown {
                "an unknown number of".to_string()
            } else {
                changes.changed.len().to_string()
            };
            println!(
                "  project: {} new commit(s), {paths} changed path(s) since start",
                changes.commits.len(),
            );
            for c in changes.commits.iter().take(10) {
                println!("    commit {c}");
            }
            for c in changes.changed.iter().take(20) {
                println!("    {c}");
            }
        }
        println!();
    }
    println!("Copy a file to the host with `paddock recover ID PATH`, or delete it with `paddock discard ID PATH`.");
    Ok(0)
}

fn pick_paths(
    env: &HostEnv,
    cfg: &paddock::recover::RecoveryCfg,
    s: &paddock::recover::Session,
    paths: Vec<String>,
    all: bool,
) -> Vec<String> {
    if all {
        paddock::recover::scan(env, cfg, &s.home, s.started)
            .into_iter()
            .map(|i| i.path)
            .collect()
    } else {
        paths
    }
}

pub(crate) fn recover_cmd(
    env: &HostEnv,
    id: &str,
    paths: Vec<String>,
    all: bool,
    to: Option<&std::path::Path>,
    keep: bool,
) -> Result<i32> {
    let cfg = paddock::recover::RecoveryCfg::load(env)?;
    let s = session_for(env, id)?;
    let mut failed = 0;
    for p in pick_paths(env, &cfg, &s, paths, all) {
        match paddock::recover::recover(env, &cfg, &s.home, &p, to, keep) {
            Ok(r) => println!(
                "Recovered {} -> {}{}",
                r.from,
                r.to.display(),
                if r.renamed {
                    " (renamed: a file with that name exists)"
                } else {
                    ""
                }
            ),
            Err(e) => {
                failed += 1;
                eprintln!("paddock: cannot recover {p}: {e:#}");
            }
        }
    }
    Ok(if failed > 0 { 1 } else { 0 })
}

pub(crate) fn discard_cmd(env: &HostEnv, id: &str, paths: Vec<String>, all: bool) -> Result<i32> {
    let cfg = paddock::recover::RecoveryCfg::load(env)?;
    let s = session_for(env, id)?;
    let mut failed = 0;
    for p in pick_paths(env, &cfg, &s, paths, all) {
        match paddock::recover::discard(&cfg, &s.home, &p) {
            Ok(r) => println!("Deleted {r} from the sandbox"),
            Err(e) => {
                failed += 1;
                eprintln!("paddock: cannot delete {p}: {e:#}");
            }
        }
    }
    Ok(if failed > 0 { 1 } else { 0 })
}

pub(crate) fn undo(
    env: &HostEnv,
    session: Option<String>,
    list: bool,
    yes: bool,
    force: bool,
) -> Result<i32> {
    paddock::snapshot::undo_cmd(env, session.as_deref(), list, yes, force)
}

pub(crate) fn dedupe_home(env: &HostEnv, home: &Path) -> Result<i32> {
    paddock::storage::dedupe::be_idle();
    let r = paddock::storage::dedupe::home(env, home, paddock::storage::dedupe::AFTER_RUN)?;
    log::info!("dedupe after the run: {r:?}");
    Ok(0)
}

pub(crate) fn promote_caches(
    env: &HostEnv,
    project: &Path,
    crates_index: Option<String>,
    sumdb: Option<String>,
) -> Result<i32> {
    paddock::storage::dedupe::be_idle();
    let r = paddock::storage::layers::promote::command(env, project, crates_index, sumdb)?;
    for o in &r {
        println!("{} {} {} {}", o.cache, o.package, o.version, o.result);
    }
    Ok(0)
}

pub(crate) fn stop(env: &HostEnv, id: &str) -> Result<i32> {
    let all = Running::list(env);
    stop_one(env, &all, id)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn gc(
    env: &HostEnv,
    yes: bool,
    homes: bool,
    older_than: u64,
    include_app_logins: bool,
    worktrees: bool,
    force: bool,
    scratch: bool,
    dedupe: bool,
    snapshots: bool,
    json: bool,
) -> Result<i32> {
    gc_cmd(
        env,
        &paddock::storage::GcOptions {
            yes,
            homes,
            older_than_days: older_than,
            include_app_logins,
            worktrees,
            force,
            scratch,
            dedupe,
            snapshots,
        },
        json,
    )
}
