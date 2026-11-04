use super::SizeKind;
use std::collections::BTreeSet;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Sizes of trees, in one `btrfs filesystem du` call when every path is on
/// btrfs and the tool works, else by walking them.
pub fn sizes(paths: &[PathBuf]) -> (Vec<u64>, SizeKind) {
    if !paths.is_empty() && paths.iter().all(|p| on_btrfs(p)) {
        if let Some(v) = btrfs_du(paths) {
            return (v, SizeKind::BtrfsExclusive);
        }
    }
    (
        paths.iter().map(|p| allocated(p)).collect(),
        SizeKind::Allocated,
    )
}

fn on_btrfs(p: &Path) -> bool {
    const BTRFS_SUPER_MAGIC: i64 = 0x9123_683E;
    let Ok(c) = std::ffi::CString::new(p.as_os_str().as_encoded_bytes()) else {
        return false;
    };
    let mut s: libc::statfs = unsafe { std::mem::zeroed() };
    unsafe { libc::statfs(c.as_ptr(), &mut s) == 0 && s.f_type as i64 == BTRFS_SUPER_MAGIC }
}

fn btrfs_du(paths: &[PathBuf]) -> Option<Vec<u64>> {
    let out = Command::new("btrfs")
        .args(["filesystem", "du", "-s", "--raw"])
        .args(paths)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    // Columns: Total Exclusive Set-shared Filename. One line per path, in order.
    let v: Vec<u64> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .skip(1)
        .filter_map(|l| l.split_whitespace().nth(1)?.parse().ok())
        .collect();
    (v.len() == paths.len()).then_some(v)
}

/// Allocated bytes of a tree: no symlinks followed, each inode once, one
/// file system only.
pub fn allocated(root: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(root) else {
        return 0;
    };
    let dev = meta.dev();
    let mut seen = BTreeSet::new();
    let mut total = 0;
    let mut stack = vec![root.to_path_buf()];
    while let Some(p) = stack.pop() {
        let Ok(m) = std::fs::symlink_metadata(&p) else {
            continue;
        };
        if m.dev() != dev || !seen.insert(m.ino()) {
            continue;
        }
        total += m.blocks() * 512;
        if m.is_dir() {
            for e in std::fs::read_dir(&p).into_iter().flatten().flatten() {
                stack.push(e.path());
            }
        }
    }
    total
}

pub(super) fn file_len(p: &Path) -> u64 {
    std::fs::symlink_metadata(p).map(|m| m.len()).unwrap_or(0)
}
