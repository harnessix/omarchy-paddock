//! Where each namespace of paddock kv lives: one SQLite file per pasture,
//! per sandbox, per spawn tree, and one for the `user` scope, all under
//! `~/.local/state/paddock/kv/`:
//!
//! | file | holds |
//! | :--- | :--- |
//! | `registry.db` | the sandbox records and the session grants: no values |
//! | `user.db` | the `user` scope |
//! | `pasture/<project hash>/<name>.db` | one named pasture |
//! | `sandbox/<id>.db` | the `self` scope of one run, and its own pasture |
//! | `tree/<root id>.db` | the `tree` scope of one spawn tree |
//! | `store.db` | the `project` scope of older paddock versions: only the host reads it |
//!
//! The service of a sandbox opens only the files of the scopes that it can
//! use, so an error in a scope check still cannot reach another pasture:
//! that file is never opened. Removing a run or a pasture removes files.

use crate::env::HostEnv;
use crate::manifest::hex;
use anyhow::{bail, Result};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// The folder of all kv files.
pub fn root(env: &HostEnv) -> PathBuf {
    env.paddock_state().join("kv")
}

pub fn registry(root: &Path) -> PathBuf {
    root.join("registry.db")
}

/// The single store of paddock versions before pastures.
pub fn legacy(root: &Path) -> PathBuf {
    root.join("store.db")
}

/// The folder name of a project: its path is not used as a file name.
pub fn project_hash(project: &Path) -> String {
    hex(&Sha256::digest(project.as_os_str().as_encoded_bytes()))[..16].to_string()
}

/// Sandbox ids become file names: letters, digits, `-`, `_` and `.`, not
/// starting with `.`.
pub fn check_id(id: &str) -> Result<()> {
    let ok = !id.is_empty()
        && id.len() <= 128
        && !id.starts_with('.')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !ok {
        bail!("'{id}' is not a sandbox id");
    }
    Ok(())
}

/// One namespace and the file that holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ns {
    /// The `self` scope of a run.
    Own(String),
    /// A named pasture of a project.
    Pasture {
        project: PathBuf,
        name: String,
    },
    /// The pasture of a single sandbox.
    Solo {
        project: PathBuf,
        sandbox: String,
    },
    /// The `tree` scope of a spawn tree, by its root.
    Tree(String),
    User,
    /// The `project` scope of older paddock versions.
    Legacy(PathBuf),
}

impl Ns {
    /// The namespace inside the file.
    pub fn key(&self) -> String {
        match self {
            Ns::Own(id) => format!("self:{id}"),
            Ns::Pasture { project, name } => format!("pasture:{}:{name}", project.display()),
            Ns::Solo { project, sandbox } => format!(
                "pasture:{}:{}{sandbox}",
                project.display(),
                crate::pasture::SOLO_PREFIX
            ),
            Ns::Tree(root) => format!("tree:{root}"),
            Ns::User => "user".into(),
            Ns::Legacy(project) => format!("project:{}", project.display()),
        }
    }

    pub fn file(&self, root: &Path) -> Result<PathBuf> {
        Ok(match self {
            Ns::Own(id) | Ns::Solo { sandbox: id, .. } => {
                check_id(id)?;
                root.join("sandbox").join(format!("{id}.db"))
            }
            Ns::Pasture { project, name } => {
                crate::pasture::check_name(name)?;
                root.join("pasture")
                    .join(project_hash(project))
                    .join(format!("{name}.db"))
            }
            Ns::Tree(id) => {
                check_id(id)?;
                root.join("tree").join(format!("{id}.db"))
            }
            Ns::User => root.join("user.db"),
            Ns::Legacy(_) => legacy(root),
        })
    }

    /// A pasture of a project, by name: `solo-<id>` names the pasture of a
    /// single sandbox.
    pub fn pasture(project: &Path, name: &str) -> Result<Ns> {
        if let Some(id) = name.strip_prefix(crate::pasture::SOLO_PREFIX) {
            check_id(id)?;
            return Ok(Ns::Solo {
                project: project.to_path_buf(),
                sandbox: id.into(),
            });
        }
        crate::pasture::check_name(name)?;
        Ok(Ns::Pasture {
            project: project.to_path_buf(),
            name: name.into(),
        })
    }
}

/// The named pastures of a project that have a file, by name.
pub fn pastures_of(root: &Path, project: &Path) -> Vec<String> {
    let dir = root.join("pasture").join(project_hash(project));
    let mut out: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.strip_suffix(".db").map(str::to_string)
        })
        .filter(|n| crate::pasture::check_name(n).is_ok())
        .collect();
    out.sort();
    out
}

/// The ids of the files in `sandbox/` or `tree/`.
pub fn ids_in(root: &Path, dir: &str) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(root.join(dir))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.strip_suffix(".db").map(str::to_string)
        })
        .filter(|n| check_id(n).is_ok())
        .collect();
    out.sort();
    out
}

/// Remove a store file with its SQLite side files. Missing files are fine.
pub fn remove_file(path: &Path) -> Result<u64> {
    let mut bytes = 0;
    for suffix in ["", "-wal", "-shm"] {
        let mut p = path.as_os_str().to_owned();
        p.push(suffix);
        let p = PathBuf::from(p);
        if let Ok(m) = std::fs::symlink_metadata(&p) {
            bytes += m.len();
            std::fs::remove_file(&p)?;
        }
    }
    Ok(bytes)
}

/// The size of a store file with its side files.
pub fn file_bytes(path: &Path) -> u64 {
    ["", "-wal", "-shm"]
        .iter()
        .map(|s| {
            let mut p = path.as_os_str().to_owned();
            p.push(s);
            std::fs::metadata(PathBuf::from(p))
                .map(|m| m.len())
                .unwrap_or(0)
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_per_namespace() {
        let r = Path::new("/k");
        let p = Path::new("/src/app");
        assert_eq!(
            Ns::Own("a1".into()).file(r).unwrap(),
            r.join("sandbox/a1.db")
        );
        let solo = Ns::pasture(p, "solo-a1").unwrap();
        assert_eq!(solo.file(r).unwrap(), r.join("sandbox/a1.db"));
        assert_eq!(solo.key(), "pasture:/src/app:solo-a1");
        let named = Ns::pasture(p, "review").unwrap();
        assert_eq!(
            named.file(r).unwrap(),
            r.join("pasture").join(project_hash(p)).join("review.db")
        );
        assert_ne!(
            named.file(r).unwrap(),
            Ns::pasture(Path::new("/src/other"), "review")
                .unwrap()
                .file(r)
                .unwrap()
        );
        assert_eq!(Ns::User.file(r).unwrap(), r.join("user.db"));
        assert_eq!(Ns::Tree("a1".into()).file(r).unwrap(), r.join("tree/a1.db"));
        assert_eq!(Ns::Legacy(p.into()).file(r).unwrap(), r.join("store.db"));
    }

    #[test]
    fn ids_cannot_leave_the_folder() {
        for bad in ["", "../x", "a/b", ".hidden", "a b"] {
            assert!(Ns::Own(bad.into()).file(Path::new("/k")).is_err(), "{bad}");
            assert!(Ns::pasture(Path::new("/p"), &format!("solo-{bad}")).is_err());
        }
        assert!(Ns::pasture(Path::new("/p"), "../x").is_err());
    }
}
