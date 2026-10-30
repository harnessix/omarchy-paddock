//! The folder that the end-to-end tests work in.
//!
//! A sandbox gets its own /tmp, so a test project cannot live there. The
//! tests use `~/.cache/paddock-tests` (or `$XDG_CACHE_HOME/paddock-tests`),
//! not the home folder itself. A test that ends normally removes its
//! folder. A folder that stays, for example after a crash, is removed by a
//! later test run once it is a day old, after any device mount in it is
//! unmounted.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const MAX_AGE: Duration = Duration::from_secs(24 * 3600);

pub fn root() -> PathBuf {
    let cache = std::env::var("XDG_CACHE_HOME")
        .ok()
        .filter(|v| Path::new(v).is_absolute())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap()).join(".cache"));
    cache.join("paddock-tests")
}

/// A new folder for one test.
pub fn tempdir() -> tempfile::TempDir {
    let root = root();
    std::fs::create_dir_all(&root).unwrap();
    prune(&root);
    tempfile::Builder::new()
        .prefix("t-")
        .tempdir_in(&root)
        .unwrap()
}

/// Remove the folders of earlier runs that are older than a day.
fn prune(root: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let now = SystemTime::now();
    for e in entries.flatten() {
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age > MAX_AGE);
        if !old {
            continue;
        }
        let dir = e.path();
        for mp in mounts_under(&dir) {
            let _ = std::process::Command::new("fusermount3")
                .args(["-u", "-z"])
                .arg(&mp)
                .status();
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// The `fuse.paddock` mount points under `dir`.
fn mounts_under(dir: &Path) -> Vec<String> {
    let prefix = format!("{}/", dir.display());
    std::fs::read_to_string("/proc/self/mountinfo")
        .unwrap_or_default()
        .lines()
        .filter(|l| l.contains(" - fuse.paddock "))
        .filter_map(|l| l.split(' ').nth(4).map(str::to_string))
        .filter(|mp| mp.starts_with(&prefix))
        .collect()
}
