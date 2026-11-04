//! Disk use of paddock's own data, and its cleanup: `paddock du` and
//! `paddock gc`.
//!
//! A sandbox can write its own home, so everything read from a home, such as
//! `.paddock-origin`, is a hint. The identity of a home comes from its
//! directory key (`homes/<harness>/<login>/<slug>`). A project path from an
//! origin file counts only when its slug matches that key. paddock deletes
//! only inside its own directories, never through a symlink, and never the
//! home or worktree of a running sandbox.

use std::path::{Path, PathBuf};

mod apply;
pub mod caches;
pub mod dedupe;
mod images;
mod inventory;
pub mod layers;
mod origin;
mod plan;
mod render;
mod scan;
mod sizes;

pub use apply::{
    after_build, apply, check_removable, record_worktree_branch, remove_doctor_homes, remove_path,
    remove_worktree, Removed,
};
pub use images::{current_tags, parse_podman_df, parse_size};
pub use inventory::{
    removable_roots, scratch_root, BuildRecord, ClassUse, Home, Image, Inventory, KeyringFile,
    OldCache, Scratch, SessionRecord, SizeKind, SnapshotDir, Worktree, DOCTOR_HARNESS,
};
pub use origin::{login_dir, read_origin, write_origin, Origin, OriginCheck, ORIGIN_FILE};
pub use plan::{measure_plan, plan, Action, GcOptions, Item, Tier};
pub use render::{clock, date, human, local_time, render_du, render_plan};
pub use scan::{scan, scan_snapshots, worktrees, Containers, Host, RealContainers};
pub use sizes::{allocated, sizes};

use apply::recorded_worktree_branch;
use images::{scan_build_cache, scan_images};
use inventory::{
    homes_root, keyring_root, sessions_root, shared_cache, worktrees_root, OLD_CACHES,
};
use origin::{auth_from_dir, origin_matches};
use scan::measure;
use sizes::file_len;

const DAY: u64 = 86_400;
/// Ended session records older than this go in the automatic tier.
pub const SESSION_DAYS: u64 = 30;
pub const DEFAULT_OLDER_THAN: u64 = 90;

/// Subdirectories, without symlinks.
fn subdirs(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .collect();
    v.sort();
    v
}

fn name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests;
