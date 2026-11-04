//! Checks for what a unit test needs from the machine. A test skips the
//! part that needs a missing condition, and says so.

use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::{Command, Stdio};

/// bubblewrap is on PATH and can start a sandbox. It cannot when the test
/// itself runs in a sandbox that does not allow a nested user namespace.
pub fn bwrap_works() -> bool {
    Command::new("bwrap")
        .args(["--unshare-user", "--ro-bind", "/", "/", "true"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// `path` and each folder above it are owned by uid 0 in this user
/// namespace. They are not in a namespace that maps no host root.
pub fn owned_by_uid0(path: &Path) -> bool {
    path.ancestors()
        .filter(|p| !p.as_os_str().is_empty())
        .all(|p| std::fs::symlink_metadata(p).is_ok_and(|m| m.uid() == 0))
}
