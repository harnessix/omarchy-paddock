//! Persistent project task records.

use super::{board, Layout};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

pub type TaskId = String;

/// Preserve the complete board record; its kv lease has no expiry field.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    #[serde(flatten)]
    pub task: board::Record,
    pub lease_expiry: Option<u64>,
}

pub struct ProjectRecords;

impl ProjectRecords {
    /// The layout must have an active feature. Linked worktrees share the
    /// common git directory so their project records are shared too.
    pub fn path(layout: &Layout) -> PathBuf {
        let feature = layout
            .feature()
            .expect("project records require an active feature");
        Self::path_for(layout, &feature)
    }

    pub fn path_for(layout: &Layout, feature: &str) -> PathBuf {
        let root = if layout.external {
            layout.root.join(".paddock")
        } else {
            match crate::hostgit::Layout::discover(&layout.root) {
                Ok(Some(git)) => git.common_dir.join("paddock"),
                _ => layout.root.join(".paddock"),
            }
        };
        root.join("spec").join(feature).join("tasks.json")
    }
}

fn lock(path: &Path) -> Result<File> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    crate::privfs::dir(parent)?;
    let mut name = path
        .file_name()
        .context("records path has no file name")?
        .to_os_string();
    name.push(".lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(crate::privfs::FILE_MODE)
        .open(path.with_file_name(name))?;
    // SAFETY: file owns a valid file descriptor for the duration of the lock.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(file)
}

fn read(path: &Path) -> Result<BTreeMap<TaskId, Record>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)
            .with_context(|| format!("invalid records in {}", path.display()))?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(e) => Err(e.into()),
    }
}

pub fn load(path: &Path) -> Result<BTreeMap<TaskId, Record>> {
    let _lock = lock(path)?;
    read(path)
}

pub fn update(path: &Path, f: impl FnOnce(&mut BTreeMap<TaskId, Record>)) -> Result<()> {
    let _lock = lock(path)?;
    let mut records = read(path)?;
    f(&mut records);
    crate::privfs::write_atomic(path, serde_json::to_vec(&records)?)?;
    Ok(())
}

/// An absent expiry is not an expired lease; callers decide how to handle it.
pub fn normalize(records: &mut BTreeMap<TaskId, Record>, now: u64) {
    for record in records.values_mut() {
        if record.task.state.held() && record.lease_expiry.is_some_and(|expiry| expiry <= now) {
            record.task.state = board::State::Todo;
            record.task.lease = None;
            record.lease_expiry = None;
        }
    }
}

#[cfg(test)]
mod tests;
