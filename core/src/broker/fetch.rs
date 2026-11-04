//! Copy one host file into the project inbox.

use super::describe::{arg_str, safe_name};
use super::{Context, Request, INBOX};
use crate::env::HostEnv;
use crate::policy;
use anyhow::{bail, Context as _, Result};
use std::io::Write;
use std::path::{Path, PathBuf};

const MAX_FETCH: u64 = 64 * 1024 * 1024;

/// The inbox in the project. The project is sandbox-writable, so the folder
/// is opened one step at a time without following symlinks, and every result
/// is written through it (see safefs).
pub(crate) fn inbox(project: &Path) -> Result<crate::safefs::Dir> {
    let root = crate::safefs::Root::open(project)?;
    let dot = root.dir(Path::new(".paddock"), true)?;
    // Keep results out of commits. An existing entry, whatever it is, stays.
    if let Ok(mut f) = dot.create_new(std::ffi::OsStr::new(".gitignore"), 0o644) {
        f.write_all(b"*\n")?;
    }
    dot.subdir(std::ffi::OsStr::new("inbox"), true)
}

/// The inbox of a sandbox of `project`. With the `.paddock` link of the
/// loft, it is in the loft's notes, opened from the loft, not the link.
pub(crate) fn inbox_for(env: &HostEnv, project: &Path) -> Result<crate::safefs::Dir> {
    if crate::loft::host::linked(env, project) {
        let tree = crate::loft::Loft::of(env, project).tree();
        return crate::safefs::Root::open(&tree)?.dir(Path::new(INBOX), true);
    }
    inbox(project)
}

/// A fetch source, opened before the user is asked. The copy reads from the
/// open fd, so changing the path during the wait changes nothing.
pub(in crate::broker) struct FetchPlan {
    pub(in crate::broker) src: crate::safefs::Source,
    /// Where the path led when it was opened; the prompt shows this.
    pub(in crate::broker) real: PathBuf,
    pub(in crate::broker) size: u64,
}

/// Refuse a fetch of a protected path: the path asked for, where it really
/// leads, or a folder that holds a protected path.
pub(crate) fn check_fetch(env: &HostEnv, asked: &Path, real: &Path, dir: bool) -> Result<()> {
    let mut denied = policy::denied_paths(env);
    // Harness logins too: paddock copies them in itself, never by request.
    if let Ok(f) = crate::profile::HarnessFactory::new(env) {
        for n in f.names() {
            if let Some(p) = f.profile(&n) {
                denied.extend(p.state.auth.iter().map(|a| env.home.join(a)));
            }
        }
    }
    // paddock's own folders: sandbox homes of other projects, approvals,
    // the audit log, paddock kv, the gateway key, the configuration.
    denied.extend([
        env.paddock_data(),
        env.paddock_state(),
        env.paddock_config(),
        env.paddock_cache(),
    ]);
    for d in denied {
        let canon = std::fs::canonicalize(&d).ok();
        let hit =
            |d: &Path| real.starts_with(d) || asked.starts_with(d) || (dir && d.starts_with(real));
        if hit(&d) || canon.as_deref().is_some_and(hit) {
            bail!(
                "{} is or holds a protected path; paddock never copies it into a sandbox",
                asked.display()
            );
        }
    }
    Ok(())
}

pub(in crate::broker) fn plan_fetch(env: &HostEnv, req: &Request) -> Result<FetchPlan> {
    let asked = PathBuf::from(arg_str(req, "path").context("fetch needs a path")?);
    let src = crate::safefs::Source::open(&asked)?;
    let real = src.real_path()?;
    check_fetch(env, &asked, &real, src.is_dir())?;
    let size = src.size(MAX_FETCH)?;
    if size > MAX_FETCH {
        bail!(
            "{} is {} MiB; the limit is {} MiB",
            real.display(),
            size >> 20,
            MAX_FETCH >> 20
        );
    }
    Ok(FetchPlan { src, real, size })
}

pub(in crate::broker) fn fetch(
    env: &HostEnv,
    ctx: &Context,
    req: &Request,
    f: &FetchPlan,
) -> Result<serde_json::Value> {
    // Check again after the wait: the open file or folder may have moved
    // into a protected place. A replaced path does not matter: the copy
    // reads what was opened, which is what the user saw.
    let asked = PathBuf::from(arg_str(req, "path").unwrap_or(""));
    check_fetch(env, &asked, &f.src.real_path()?, f.src.is_dir())?;
    let dir = inbox_for(env, &ctx.project)?;
    let want = safe_name(&f.real.to_string_lossy(), "fetched");
    // Never remove what is there: a free name instead.
    let name = dir.copy_source(&f.src, &want, MAX_FETCH)?;
    Ok(serde_json::json!({ "path": dir.path().join(name), "bytes": f.size }))
}
