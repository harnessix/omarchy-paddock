//! Reminders before Spec Kit files enter a remote repository.
use super::{Cfg, Layout, Store, SPECIFY, SPECS};
use crate::env::HostEnv;
use anyhow::Result;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

pub const PUSH_NOTICE: &str = "This push contains Spec Kit files (.specify/, specs/): your specs and plans become part of the remote.";

pub fn default_store(cfg: &Cfg) -> bool {
    cfg.enabled && !cfg.store_explicit && cfg.store == Store::Repo
}

/// `ls-files` reads the index and directory names without running filters.
/// Keep the hardened command and its time limit, also for untracked files.
fn files(project: &Path, untracked: bool) -> Option<bool> {
    let git = crate::hostgit::Layout::discover(project).ok()??;
    let rel = project.strip_prefix(&git.work_tree).ok()?;
    let mut cmd = git.command();
    cmd.current_dir(&git.work_tree);
    cmd.arg(format!("--work-tree={}", git.work_tree.display()))
        .arg("ls-files")
        .arg("-z");
    if untracked {
        cmd.args(["--others", "--exclude-standard"]);
    } else {
        cmd.arg("--cached");
    }
    cmd.arg("--");
    for name in [SPECIFY, SPECS] {
        cmd.arg(rel.join(name));
    }
    let (code, out, _) = crate::broker::run_capture(&mut cmd, Duration::from_secs(60)).ok()?;
    (code == 0).then_some(!out.is_empty())
}

pub fn tracked(project: &Path) -> Option<bool> {
    files(project, false)
}

pub fn needs_notice(env: &HostEnv, cfg: &Cfg, project: &Path) -> bool {
    default_store(cfg)
        && Layout::resolve_with(env, cfg, project).is_some_and(|l| !l.external)
        && (tracked(project) == Some(true) || files(project, true) == Some(true))
}

/// The start notice of a Spec Kit project with the repo store (loft::push).
pub const START_NOTICE: &str = "paddock: Spec Kit keeps your constitution, specs, plans and tasks in .specify/ and specs/ in this repository. A push publishes committed files there.\nSet [speckit] store = \"external\" (or turn on the loft, see `paddock loft init`) and run `paddock spec adopt` to move them out.\nSet [speckit] store = \"repo\" to keep them here and silence this notice.";

/// The caller supplies `git log` and its revision range. Read every commit,
/// including merge parents and root commits, without a display list limit.
pub fn push_contains(mut cmd: Command) -> Result<bool> {
    cmd.args([
        "--format=",
        "--name-only",
        "-z",
        "--root",
        "-m",
        "--no-renames",
        "--",
        ".specify/",
        "specs/",
    ]);
    let (code, out, err) = crate::broker::run_capture(&mut cmd, Duration::from_secs(60))?;
    anyhow::ensure!(
        code == 0,
        "cannot check Spec Kit files in push: {}",
        err.trim()
    );
    Ok(!out.is_empty())
}

pub fn location(layout: &Layout) -> String {
    if layout.external {
        format!("store external: {}", layout.root.display())
    } else {
        let state = match tracked(&layout.root) {
            Some(true) => "git tracks Spec Kit files",
            Some(false) => "git tracks no Spec Kit files",
            None => "git tracking unknown",
        };
        format!("store repo: {} ({state})", layout.root.display())
    }
}

#[cfg(test)]
mod tests;
