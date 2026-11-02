//! Package caches in layers.
//!
//! Each package cache of a sandbox is an overlay on the bwrap backend:
//!
//! - The lower layer `<paddock cache>/layers/<cache>` is shared by every
//!   project. It is read-only in every sandbox. Only paddock writes it, on
//!   the host, and only with files whose content it verified (promote.rs).
//! - The upper and work folders are per project:
//!   `<paddock cache>/projects/<slug>/<cache>/{upper,work}`.
//!
//! Other backends have no overlay: they bind the upper folder alone,
//! read-write. `[storage] layered_caches = false` does the same on every
//! backend. No setting gives back one writable folder for every project:
//! a project could then plant code that another project runs.

use crate::env::HostEnv;
use crate::spec::Mount;
use anyhow::Result;
use serde::Serialize;
use std::path::{Path, PathBuf};

/// The caches that the detectors mount in layers.
pub const CACHES: &[&str] = &[
    "cargo-registry",
    "cargo-git",
    "npm",
    "pnpm-store",
    "bun",
    "yarn",
    "uv",
    "pip",
    "go-mod",
    "go-build",
];

/// The file in a project folder that names the project, written by paddock.
const PROJECT_FILE: &str = "project";

pub fn layers_root(env: &HostEnv) -> PathBuf {
    env.paddock_cache().join("layers")
}

pub fn projects_root(env: &HostEnv) -> PathBuf {
    env.paddock_cache().join("projects")
}

pub fn lower(env: &HostEnv, cache: &str) -> PathBuf {
    layers_root(env).join(cache)
}

pub fn project_dir(env: &HostEnv, project: &Path) -> PathBuf {
    projects_root(env).join(crate::state::slug(project))
}

pub fn upper(env: &HostEnv, project: &Path, cache: &str) -> PathBuf {
    project_dir(env, project).join(cache).join("upper")
}

/// `[storage] layered_caches` of the user configuration. On by default.
pub fn enabled(env: &HostEnv) -> bool {
    let text =
        std::fs::read_to_string(env.paddock_config().join("config.toml")).unwrap_or_default();
    enabled_in(&text)
}

pub fn enabled_in(text: &str) -> bool {
    let Ok(v) = toml::from_str::<toml::Value>(text) else {
        return true;
    };
    v.get("storage")
        .and_then(|s| s.get("layered_caches"))
        .and_then(|d| d.as_bool())
        .unwrap_or(true)
}

/// The mount of one package cache at `dst` for `project`.
pub fn mount(
    env: &HostEnv,
    project: &Path,
    cache: &str,
    dst: PathBuf,
    origin: &str,
) -> Result<Mount> {
    mount_with(env, project, cache, dst, origin, enabled(env))
}

pub fn mount_with(
    env: &HostEnv,
    project: &Path,
    cache: &str,
    dst: PathBuf,
    origin: &str,
    layered: bool,
) -> Result<Mount> {
    let dir = project_dir(env, project);
    crate::privfs::dir(&projects_root(env))?;
    crate::privfs::dir(&dir)?;
    let name = dir.join(PROJECT_FILE);
    let text = project.to_string_lossy();
    if std::fs::read_to_string(&name).ok().as_deref() != Some(&*text) {
        crate::privfs::write_atomic(&name, text.as_bytes())?;
    }
    let up = dir.join(cache).join("upper");
    crate::privfs::dir(&dir.join(cache))?;
    crate::privfs::dir(&up)?;
    if !layered {
        return Ok(Mount::bind(up, dst, true, origin));
    }
    let work = dir.join(cache).join("work");
    crate::privfs::dir(&work)?;
    crate::privfs::dir(&layers_root(env))?;
    let low = lower(env, cache);
    crate::privfs::dir(&low)?;
    Ok(Mount::overlay(low, up, work, dst, origin))
}

/// A cache layer for `paddock du` and `paddock gc`.
#[derive(Debug, Clone, Serialize)]
pub struct Layer {
    pub path: PathBuf,
    /// The project of a project folder. None for the shared layers.
    pub project: Option<PathBuf>,
    /// A project folder whose project no longer exists.
    pub orphan: bool,
    pub bytes: u64,
}

/// The shared lower layers, then one entry per project folder.
pub fn scan(env: &HostEnv) -> Vec<Layer> {
    let mut out = Vec::new();
    for cache in CACHES {
        let p = lower(env, cache);
        if std::fs::symlink_metadata(&p).is_ok_and(|m| m.is_dir()) {
            out.push(Layer {
                path: p,
                project: None,
                orphan: false,
                bytes: 0,
            });
        }
    }
    let Ok(rd) = std::fs::read_dir(projects_root(env)) else {
        return out;
    };
    let mut projects: Vec<Layer> = rd
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| {
            let project = std::fs::read_to_string(e.path().join(PROJECT_FILE))
                .ok()
                .map(PathBuf::from);
            let orphan = project.as_ref().is_none_or(|p| !p.is_dir());
            Layer {
                path: e.path(),
                project,
                orphan,
                bytes: 0,
            }
        })
        .collect();
    projects.sort_by(|a, b| a.path.cmp(&b.path));
    out.extend(projects);
    out
}

/// A host environment with its home in `dir`, for tests.
#[cfg(test)]
pub fn test_env(dir: &Path) -> HostEnv {
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    HostEnv::from_parts(home, "tester".into(), 1000, 1000, Default::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::MountKind;

    #[test]
    fn the_user_configuration_turns_layers_off() {
        assert!(enabled_in(""));
        assert!(enabled_in("[storage]\ndedupe = false\n"));
        assert!(!enabled_in("[storage]\nlayered_caches = false\n"));
    }

    #[test]
    fn a_cache_is_an_overlay_or_a_plain_folder_per_project() {
        let t = tempfile::tempdir().unwrap();
        let env = test_env(t.path());
        let proj = t.path().join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let dst = PathBuf::from("/home/u/.npm");
        let m = mount_with(&env, &proj, "npm", dst.clone(), "t", true).unwrap();
        assert_eq!(m.kind, MountKind::Overlay);
        let up = upper(&env, &proj, "npm");
        assert_eq!(m.src.as_deref(), Some(up.as_path()));
        let l = m.layers.clone().unwrap();
        assert_eq!(l.lower, lower(&env, "npm"));
        assert_eq!(l.work, up.with_file_name("work"));
        assert!(l.lower.is_dir() && l.work.is_dir() && up.is_dir());
        let args = crate::backend::bwrap::overlay_args(&m);
        let s = |p: &Path| p.to_string_lossy().into_owned();
        assert_eq!(
            args,
            vec![
                "--overlay-src".to_string(),
                s(&l.lower),
                "--overlay".into(),
                s(&up),
                s(&l.work),
                s(&dst),
            ]
        );

        // Off: the upper folder alone, read-write. Never the old shared one.
        let m = mount_with(&env, &proj, "npm", dst.clone(), "t", false).unwrap();
        assert_eq!(m.kind, MountKind::Rw);
        assert_eq!(m.src.as_deref(), Some(up.as_path()));
        assert!(!env.paddock_cache().join("shared").join("npm").exists());
    }

    #[test]
    fn scan_marks_the_project_folders_of_gone_projects() {
        let t = tempfile::tempdir().unwrap();
        let env = test_env(t.path());
        let live = t.path().join("live");
        let gone = t.path().join("gone");
        std::fs::create_dir_all(&live).unwrap();
        std::fs::create_dir_all(&gone).unwrap();
        for p in [&live, &gone] {
            mount_with(&env, p, "npm", "/h/.npm".into(), "t", true).unwrap();
        }
        std::fs::remove_dir(&gone).unwrap();
        let v = scan(&env);
        assert_eq!(v.iter().filter(|l| l.project.is_none()).count(), 1);
        let orphan: Vec<_> = v.iter().filter(|l| l.orphan).collect();
        assert_eq!(orphan.len(), 1);
        assert_eq!(orphan[0].project.as_deref(), Some(gone.as_path()));
    }
}
