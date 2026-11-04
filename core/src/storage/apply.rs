use super::*;
use crate::env::HostEnv;
use crate::recover::Session;
use crate::state::{self, AuthMode, Running};
use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

/// The branches that paddock made for its worktrees, one file per worktree,
/// on the host where no sandbox writes.
fn worktree_branches_root(env: &HostEnv) -> PathBuf {
    env.paddock_state().join("worktree-branches")
}

/// The record of a worktree: `<slug>/<id>` below the worktree folder.
fn worktree_branch_file(env: &HostEnv, path: &Path) -> Option<PathBuf> {
    let rel = path.strip_prefix(worktrees_root(env)).ok()?;
    (rel.components().count() == 2).then(|| worktree_branches_root(env).join(rel))
}

/// Record the branch that paddock made for the worktree at `path`. The
/// worktree scan trusts this record, not the branch that the worktree is on
/// now: a sandbox can switch branches there.
pub fn record_worktree_branch(env: &HostEnv, path: &Path, branch: &str) {
    let Some(f) = worktree_branch_file(env, path) else {
        return;
    };
    let r = f
        .parent()
        .map_or(Ok(()), crate::privfs::dir)
        .and_then(|_| crate::privfs::write(&f, branch));
    if let Err(e) = r {
        log::warn!("cannot record the branch of {}: {e}", path.display());
    }
}

pub(super) fn recorded_worktree_branch(env: &HostEnv, path: &Path) -> Option<String> {
    let f = worktree_branch_file(env, path)?;
    let b = std::fs::read_to_string(f).ok()?.trim().to_string();
    (!b.is_empty()).then_some(b)
}

fn forget_worktree_branch(env: &HostEnv, path: &Path) {
    if let Some(f) = worktree_branch_file(env, path) {
        let _ = std::fs::remove_file(&f);
        if let Some(parent) = f.parent() {
            let _ = std::fs::remove_dir(parent);
        }
    }
}

/// Remove the homes of `paddock doctor --escape` runs that are over: the
/// verified origin names a project that is gone, and no sandbox runs in the
/// home. Each run has its own scratch project, so each run has its own home.
/// A home without a verified origin stays for `paddock gc`. Returns the
/// removed homes.
pub fn remove_doctor_homes(env: &HostEnv) -> Vec<PathBuf> {
    let running: BTreeSet<PathBuf> = Running::list(env)
        .into_iter()
        .filter(|r| r.harness == DOCTOR_HARNESS)
        .filter_map(|r| {
            let auth = r.auth.parse::<AuthMode>().ok()?;
            Some(
                homes_root(env)
                    .join(DOCTOR_HARNESS)
                    .join(login_dir(&auth))
                    .join(state::slug(&r.project)),
            )
        })
        .collect();
    let mut removed = Vec::new();
    for ldir in subdirs(&homes_root(env).join(DOCTOR_HARNESS)) {
        let Some(auth) = auth_from_dir(&name(&ldir)) else {
            continue;
        };
        let auth = auth.to_string();
        for path in subdirs(&ldir) {
            if running.contains(&path) {
                continue;
            }
            let over = read_origin(&path).is_some_and(|o| {
                origin_matches(&o, DOCTOR_HARNESS, &auth, &name(&path)) && !o.project.exists()
            });
            if over && crate::safefs::remove_tree_path(&path).is_ok() {
                removed.push(path);
            }
        }
    }
    removed
}

/// Refuse a path that is not strictly inside one of paddock's own
/// directories, that is a symlink, or whose parents lead out through a symlink.
/// Returns that directory, as a real path, and the path below it.
pub fn check_removable(env: &HostEnv, path: &Path) -> Result<(PathBuf, PathBuf)> {
    if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        bail!("{} is not a plain absolute path", path.display());
    }
    let meta = std::fs::symlink_metadata(path)
        .with_context(|| format!("{} does not exist", path.display()))?;
    if meta.file_type().is_symlink() {
        bail!(
            "{} is a symlink; paddock does not follow it",
            path.display()
        );
    }
    let (Some(parent), Some(file)) = (path.parent(), path.file_name()) else {
        bail!("{} has no parent", path.display());
    };
    let parent = std::fs::canonicalize(parent)?;
    // An old shared cache: only the exact folders that paddock replaced.
    if let Ok(shared) = std::fs::canonicalize(shared_cache(env)) {
        if parent == shared && OLD_CACHES.iter().any(|(old, _)| file == *old) {
            return Ok((shared, PathBuf::from(file)));
        }
    }
    for root in removable_roots(env) {
        let Ok(root) = std::fs::canonicalize(&root) else {
            continue;
        };
        if let Ok(rel) = parent.join(file).strip_prefix(&root) {
            if !rel.as_os_str().is_empty() {
                return Ok((root.clone(), rel.to_path_buf()));
            }
        }
    }
    bail!("{} is outside paddock's own directories", path.display())
}

/// Delete a file or a tree after `check_removable`. Never follows a symlink:
/// the walk starts at paddock's own folder and opens one step at a time, and
/// read-only folders (a Go module cache) get write permission through the
/// open folder, not by path (see safefs).
pub fn remove_path(env: &HostEnv, path: &Path) -> Result<()> {
    let (root, rel) = check_removable(env, path)?;
    crate::safefs::Root::open(&root)?
        .remove_tree(&rel)
        .with_context(|| format!("remove {}", path.display()))
}

/// A git command on the repository that may write or read its work tree, for
/// example `worktree remove`, which runs a status internally. It is refused
/// when the repository configuration can start a program (see hostgit.rs).
fn git_write(repo: &Path, args: &[&str]) -> Result<()> {
    crate::hostgit::Checkout::open(repo)?.run(args).map(|_| ())
}

/// Remove a worktree that paddock made, and its branch with `delete_branch`.
/// Gives back a note when it kept the branch.
pub fn remove_worktree(
    env: &HostEnv,
    path: &Path,
    repo: Option<&Path>,
    branch: Option<&str>,
    delete_branch: bool,
    force: bool,
) -> Result<Option<String>> {
    check_removable(env, path)?;
    let mut note = None;
    match repo {
        Some(repo) => {
            let p = path.display().to_string();
            let mut a = vec!["worktree", "remove"];
            if force {
                a.push("--force");
            }
            a.push(&p);
            // The worktree itself is sandbox-written: its own
            // configuration must be harmless too, because the
            // removal reads that work tree.
            if let Ok(Some(l)) = crate::hostgit::Layout::discover(path) {
                l.guard()?;
            }
            git_write(repo, &a)?;
            match branch {
                Some(b) if delete_branch => git_write(repo, &["branch", "-D", b])?,
                Some(b) => note = Some(format!("kept the branch {b}")),
                None => {}
            }
        }
        None => remove_path(env, path)?,
    }
    forget_worktree_branch(env, path);
    // The per-repository folder, when it is empty now.
    if let Some(parent) = path.parent() {
        let _ = std::fs::remove_dir(parent);
    }
    Ok(note)
}

/// The result of one removal.
#[derive(Debug, Clone, Serialize)]
pub struct Removed {
    pub what: String,
    pub bytes: u64,
    pub error: Option<String>,
    pub note: Option<String>,
}

/// Remove the selected items. The running sandboxes are read again first, so
/// one that started after the scan keeps its home.
pub fn apply(env: &HostEnv, containers: &dyn Containers, items: &[Item]) -> Vec<Removed> {
    let running: Vec<Running> = Running::list(env);
    let running_homes: BTreeSet<PathBuf> = Session::all(env)
        .into_iter()
        .filter(|s| running.iter().any(|r| r.id == s.id))
        .map(|s| s.home)
        .chain(running.iter().filter_map(|r| {
            let auth: AuthMode = r.auth.parse().ok()?;
            Some(
                homes_root(env)
                    .join(&r.harness)
                    .join(login_dir(&auth))
                    .join(state::slug(&r.project)),
            )
        }))
        .collect();
    let mut out = Vec::new();
    for i in items.iter().filter(|i| i.selected()) {
        let mut note = None;
        let r: Result<()> = match &i.action {
            Action::Image { cli, tags } => tags
                .iter()
                .try_for_each(|t| containers.run(cli, &["rmi", t]).map(|_| ())),
            // Only the records that paddock found: one id filter each.
            Action::BuildCache { cli, ids } => ids.iter().try_for_each(|id| {
                let f = format!("id={id}");
                containers
                    .run(cli, &["builder", "prune", "--force", "--filter", &f])
                    .map(|_| ())
            }),
            Action::Path { path }
                if !running.is_empty() && path.parent() == Some(shared_cache(env).as_path()) =>
            {
                Err(anyhow::anyhow!("a sandbox runs; skipped"))
            }
            Action::Path { path } if running_homes.contains(path) => {
                Err(anyhow::anyhow!("a sandbox started in it; skipped"))
            }
            Action::Path { path } => remove_path(env, path),
            Action::Caches { home, .. } if running_homes.contains(home) => {
                Err(anyhow::anyhow!("a sandbox started in it; skipped"))
            }
            // The walk starts at paddock's own folder and never follows a
            // symlink that the sandbox planted on the way.
            Action::Caches { home, folders } => (|| {
                let (root, rel) = check_removable(env, home)?;
                let root = crate::safefs::Root::open(&root)?;
                for f in folders {
                    root.remove_tree(&rel.join(f))
                        .with_context(|| format!("remove {}", home.join(f).display()))?;
                }
                Ok(())
            })(),
            Action::View { id } => crate::view::remove(env, id),
            Action::Kv { sandboxes } => {
                crate::kv::gc_apply(env, sandboxes, state::now()).map(|_| ())
            }
            Action::Worktree { path, .. } if running.iter().any(|r| r.project == *path) => {
                Err(anyhow::anyhow!("a sandbox started in it; skipped"))
            }
            Action::Worktree {
                path,
                repo,
                branch,
                delete_branch,
                force,
            } => remove_worktree(
                env,
                path,
                repo.as_deref(),
                branch.as_deref(),
                *delete_branch,
                *force,
            )
            .map(|n| note = n),
        };
        out.push(Removed {
            what: i.what.clone(),
            bytes: i.bytes,
            error: r.err().map(|e| format!("{e:#}")),
            note,
        });
    }
    out
}

/// The automatic tier, after a new base image was built. `PADDOCK_GC_AFTER_BUILD=0`
/// turns it off.
pub fn after_build(env: &HostEnv, tag: &str) {
    if env.var("PADDOCK_GC_AFTER_BUILD") == Some("0") {
        return;
    }
    let containers = RealContainers(env);
    let mut host = Host::real(env, &containers);
    host.current_tags.insert(tag.to_string());
    let mut inv = scan(env, &host, false);
    let mut items = plan(
        &inv,
        &GcOptions {
            yes: true,
            older_than_days: DEFAULT_OLDER_THAN,
            ..Default::default()
        },
        state::now(),
    );
    items.retain(|i| i.selected());
    if items.is_empty() {
        return;
    }
    measure_plan(&mut inv, &mut items);
    for r in apply(env, &containers, &items) {
        match r.error {
            None => eprintln!("paddock: gc: removed {} ({})", r.what, human(r.bytes)),
            Some(e) => log::warn!("gc: cannot remove {}: {e}", r.what),
        }
    }
}
