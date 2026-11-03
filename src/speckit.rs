//! Spec Kit projects. A project with a `.specify/` folder (GitHub Spec Kit,
//! the `specify` CLI) gets this without a setting: `paddock-host spawn
//! --task` starts a worker for one task of `tasks.md`, paddock keeps the
//! state of each task in kv (board.rs) and writes the checkboxes back
//! (tasks.rs), and the session and compaction notes name the task of a run
//! (prompt.rs).
//!
//! `[speckit]` in the user configuration, never in a project `paddock.toml`:
//!
//! - `enabled` (default true): false turns all of this off.
//! - `store` (default `"repo"`): where the Spec Kit files live. `repo` is
//!   the Spec Kit default: `.specify/` and `specs/` in the project.
//!   `external` keeps them in paddock's data folder, one folder per
//!   repository (the same key as pastures), and mounts them into every
//!   sandbox of the repository at `.specify/` and `specs/`, worktrees
//!   included. They never become git files: paddock adds both paths to
//!   `info/exclude` of the repository, which git does not track.
//!
//! `paddock spec adopt` moves an in-repo `.specify/` and `specs/` into the
//! external store (adopt.rs).

pub mod adopt;
pub mod board;
pub mod flow;
pub mod git;
pub mod prompt;
pub mod tasks;

use crate::env::HostEnv;
use crate::spec::{Mount, SandboxSpec};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The Spec Kit folder: its presence turns Spec Kit mode on.
pub const SPECIFY: &str = ".specify";
/// The folder of the features.
pub const SPECS: &str = "specs";
/// The largest Spec Kit file that paddock reads.
pub const MAX_FILE: u64 = 1 << 20;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Store {
    /// `.specify/` and `specs/` in the project, as Spec Kit makes them.
    #[default]
    Repo,
    /// In paddock's data folder, mounted into each sandbox.
    External,
}

/// `[speckit]` of the user configuration.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Cfg {
    pub enabled: bool,
    pub store: Store,
    #[serde(skip)]
    pub store_explicit: bool,
}

impl Default for Cfg {
    fn default() -> Self {
        Self {
            enabled: true,
            store: Store::Repo,
            store_explicit: false,
        }
    }
}

impl Cfg {
    pub fn parse(text: &str) -> Result<Self> {
        let v: toml::Value = toml::from_str(text)?;
        Ok(match v.get("speckit") {
            Some(t) => {
                let mut cfg: Self = t.clone().try_into()?;
                cfg.store_explicit = t.get("store").is_some();
                cfg
            }
            None => Self::default(),
        })
    }

    /// The user configuration. A bad table is a warning and the default.
    pub fn load(env: &HostEnv) -> Self {
        let file = env.paddock_config().join("config.toml");
        let Ok(text) = std::fs::read_to_string(&file) else {
            return Self::default();
        };
        Self::parse(&text).unwrap_or_else(|e| {
            log::warn!("[speckit] in {}: {e:#}", file.display());
            Self::default()
        })
    }
}

/// The repository of a folder: the main work tree of a linked worktree,
/// the top of the work tree otherwise. A folder outside git is its own key.
pub fn repo_of(dir: &Path) -> PathBuf {
    match crate::hostgit::Layout::discover(dir) {
        Ok(Some(l)) if l.linked => match l.common_dir.file_name() {
            Some(n) if n == ".git" => l.common_dir.parent().unwrap_or(dir).to_path_buf(),
            _ => l.work_tree,
        },
        Ok(Some(l)) => l.work_tree,
        _ => dir.to_path_buf(),
    }
}

/// The external store of a repository.
pub fn store_dir(env: &HostEnv, repo: &Path) -> PathBuf {
    env.paddock_data()
        .join("speckit")
        .join(crate::kv::shard::project_hash(repo))
}

/// A real folder, not a symlink.
fn is_dir(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir())
}

/// Where the Spec Kit files of a project are on the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    /// The folder that holds `.specify/` and `specs/`.
    pub root: PathBuf,
    /// The files are in the external store, and every sandbox of the
    /// repository sees them.
    pub external: bool,
}

impl Layout {
    /// The Spec Kit files of `project`, or None when it is no Spec Kit
    /// project or the user turned this off.
    pub fn resolve(env: &HostEnv, project: &Path) -> Option<Self> {
        let cfg = Cfg::load(env);
        Self::resolve_with(env, &cfg, project)
    }

    pub fn resolve_with(env: &HostEnv, cfg: &Cfg, project: &Path) -> Option<Self> {
        if !cfg.enabled {
            return None;
        }
        if cfg.store == Store::External {
            let store = store_dir(env, &repo_of(project));
            if is_dir(&store.join(SPECIFY)) {
                return Some(Self {
                    root: store,
                    external: true,
                });
            }
        }
        is_dir(&project.join(SPECIFY)).then(|| Self {
            root: project.to_path_buf(),
            external: false,
        })
    }

    pub fn read(&self, rel: &Path) -> Option<String> {
        let root = crate::safefs::Root::open(&self.root).ok()?;
        let bytes = root.read(rel, MAX_FILE).ok()??;
        Some(String::from_utf8_lossy(&bytes).into_owned())
    }

    pub fn exists(&self, rel: &Path) -> bool {
        std::fs::symlink_metadata(self.root.join(rel)).is_ok()
    }

    pub fn tasks_rel(feature: &str) -> PathBuf {
        Path::new(SPECS).join(feature).join("tasks.md")
    }

    /// The active feature: `feature_directory` of `.specify/feature.json`,
    /// as Spec Kit's scripts read it, or else the last feature folder that
    /// has a `tasks.md`.
    pub fn feature(&self) -> Option<String> {
        if let Some(f) = self
            .read(&Path::new(SPECIFY).join("feature.json"))
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .and_then(|v| v["feature_directory"].as_str().map(str::to_string))
            .and_then(|d| feature_name(&d))
        {
            if self.exists(&Path::new(SPECS).join(&f)) {
                return Some(f);
            }
        }
        self.features().pop()
    }

    /// The feature folders with a `tasks.md`, sorted by name.
    pub fn features(&self) -> Vec<String> {
        let Ok(dir) =
            crate::safefs::Root::open(&self.root).and_then(|r| r.dir(Path::new(SPECS), false))
        else {
            return Vec::new();
        };
        let mut out: Vec<String> = dir
            .entries()
            .unwrap_or_default()
            .into_iter()
            .filter(|(_, k)| *k == crate::safefs::Kind::Dir)
            .filter_map(|(n, _)| n.into_string().ok())
            .filter(|n| valid_feature(n) && self.exists(&Self::tasks_rel(n)))
            .collect();
        out.sort();
        out
    }
}

/// A feature folder name: one path component of letters, digits, `-`,
/// `_` and `.`.
pub fn valid_feature(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 100
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// The feature of a `feature_directory` value such as `specs/001-x`, also
/// as an absolute path.
fn feature_name(dir: &str) -> Option<String> {
    let mut parts = dir.trim_end_matches('/').rsplit('/');
    let name = parts.next()?;
    (parts.next() == Some(SPECS) && valid_feature(name)).then(|| name.to_string())
}

/// The lines that keep the spec paths out of git. They are relative to
/// each work tree, so one `info/exclude` covers every worktree.
pub fn exclude_lines(rel: &Path) -> Vec<String> {
    let base = rel.display().to_string();
    let base = base.trim_matches('/');
    [SPECIFY, SPECS]
        .iter()
        .map(|n| {
            if base.is_empty() {
                format!("/{n}/")
            } else {
                format!("/{base}/{n}/")
            }
        })
        .collect()
}

/// Add the spec paths of `project` to `info/exclude` of its repository.
/// The file is read and replaced through safefs: a sandbox can write the
/// repository. Returns true when it changed the file.
pub fn exclude(project: &Path) -> Result<bool> {
    let Some(l) = crate::hostgit::Layout::discover(project)? else {
        return Ok(false);
    };
    let rel = project.strip_prefix(&l.work_tree).unwrap_or(Path::new(""));
    let root = crate::safefs::Root::open(&l.common_dir)?;
    let file = Path::new("info/exclude");
    let before = root
        .read(file, MAX_FILE)?
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    let missing: Vec<String> = exclude_lines(rel)
        .into_iter()
        .filter(|l| !before.lines().any(|b| b.trim() == l))
        .collect();
    if missing.is_empty() {
        return Ok(false);
    }
    let mut text = before;
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str("# paddock: the external Spec Kit store\n");
    for l in missing {
        text.push_str(&l);
        text.push('\n');
    }
    root.write(file, text.as_bytes(), 0o644)?;
    Ok(true)
}

/// The build step: with the external store, mount it at `.specify/` and
/// `specs/` of the sandbox's project, and keep both out of git. `repo` is
/// the repository key (the main work tree of a worktree).
pub fn mount(env: &HostEnv, spec: &mut SandboxSpec, repo: &Path) -> Result<()> {
    let cfg = Cfg::load(env);
    if !cfg.enabled || cfg.store != Store::External {
        return Ok(());
    }
    let store = store_dir(env, &repo_of(repo));
    if !is_dir(&store.join(SPECIFY)) {
        if is_dir(&repo.join(SPECIFY)) {
            log::warn!("this project keeps .specify/ in the repository, and [speckit] store is external: `paddock spec adopt` moves it to the store");
        }
        return Ok(());
    }
    let project = spec.project.clone();
    for name in [SPECIFY, SPECS] {
        let dst = project.join(name);
        if std::fs::symlink_metadata(&dst).is_ok_and(|m| !m.is_dir()) {
            log::warn!(
                "{} is not a folder; paddock does not mount the Spec Kit store there",
                dst.display()
            );
            continue;
        }
        // A folder with files of its own is not ours to hide.
        if std::fs::read_dir(&dst).is_ok_and(|mut d| d.next().is_some()) {
            log::warn!(
                "{} has files; paddock does not mount the Spec Kit store over it (`paddock spec adopt` moves them)",
                dst.display()
            );
            continue;
        }
        let src = store.join(name);
        crate::privfs::dir(&src)?;
        // The mount point, an empty folder that git does not show.
        crate::safefs::Root::open(&project)?.dir(Path::new(name), true)?;
        spec.add_mount(Mount::bind(&src, &dst, true, "spec kit store"));
    }
    if let Err(e) = exclude(&project) {
        log::warn!("cannot keep the Spec Kit store out of git: {e:#}");
    }
    Ok(())
}

/// The root of the spawn tree of a running sandbox, from the list of
/// running sandboxes: the top of its chain of parents.
pub fn tree_root_of(env: &HostEnv, sandbox: &str) -> Option<String> {
    let all = crate::state::Running::list(env);
    let mut at = all.iter().find(|r| r.id == sandbox)?;
    for _ in 0..all.len() {
        match at.parent.as_deref() {
            Some(p) => match all.iter().find(|r| r.id == p) {
                Some(up) => at = up,
                None => return Some(p.to_string()),
            },
            None => return Some(at.id.clone()),
        }
    }
    Some(at.id.clone())
}

/// The task record of a sandbox in the tree of `root`: the one it holds,
/// or else the last one it worked on.
pub fn record_of(env: &HostEnv, root: &str, sandbox: &str) -> Option<board::Record> {
    let b = board::Board::tree(&crate::kv::kv_root(env), root, flow::quota(env)).ok()?;
    if !b.file.is_file() {
        return None;
    }
    let st = b.open().ok()?;
    let mut mine: Vec<board::Record> = b
        .all(&st, crate::state::now())
        .ok()?
        .into_iter()
        .map(|(r, _)| r)
        .filter(|r| r.sandbox.as_deref() == Some(sandbox))
        .collect();
    mine.sort_by_key(|r| (r.state.held(), r.updated));
    mine.pop()
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_flow;
