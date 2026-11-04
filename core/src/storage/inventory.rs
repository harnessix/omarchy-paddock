use super::{caches, layers, OriginCheck};
use crate::env::HostEnv;
use serde::Serialize;
use std::path::PathBuf;

/// Folders under `$XDG_CACHE_HOME/paddock/shared` that paddock no longer
/// mounts, with the folder that replaced each.
/// The package caches here were writable by every sandbox, so their content
/// is not trusted: the layers start empty.
pub(super) const OLD_CACHES: &[(&str, &str)] = &[
    ("mise-installs", "mise-tools"),
    ("cargo-registry", "layers/cargo-registry"),
    ("cargo-git", "layers/cargo-git"),
    ("npm", "layers/npm"),
    ("pnpm-store", "layers/pnpm-store"),
    ("bun", "layers/bun"),
    ("yarn", "layers/yarn"),
    ("uv", "layers/uv"),
    ("pip", "layers/pip"),
    ("go-mod", "layers/go-mod"),
    ("go-build", "layers/go-build"),
];

#[derive(Debug, Clone, Serialize)]
pub struct Home {
    pub path: PathBuf,
    pub harness: String,
    pub auth: String,
    pub project: Option<PathBuf>,
    pub origin: OriginCheck,
    pub project_exists: Option<bool>,
    /// From the origin file or a session record. None when neither exists.
    pub last_used: Option<u64>,
    /// The newest change time of the home folder and its top-level entries.
    pub modified: u64,
    pub bytes: u64,
    /// A desktop app: the login lives in the home.
    pub app: bool,
    /// The harness profile still exists.
    pub profile: bool,
    pub running: bool,
    /// What deleting the home loses.
    pub loses: String,
    /// Cache folders in the home with their class, relative to it.
    #[serde(skip)]
    pub caches: Vec<(caches::Class, PathBuf)>,
}

impl Home {
    /// The age that gc uses: the newest of every sign of use. The sandbox
    /// can only make its home look newer than that, never older.
    pub(super) fn last_active(&self) -> u64 {
        self.last_used.unwrap_or(0).max(self.modified)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Image {
    pub cli: String,
    pub id: String,
    pub tags: Vec<String>,
    pub bytes: u64,
    /// The bytes that only this image holds, which removing it frees.
    /// None when the container CLI does not report it.
    pub unique: Option<u64>,
    pub created: String,
    /// The tag of a current configuration, or the image just built.
    pub current: bool,
    /// A container, running or stopped, uses it.
    pub in_use: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Worktree {
    pub path: PathBuf,
    /// The repository that owns the worktree. None when paddock cannot find it.
    pub repo: Option<PathBuf>,
    pub branch: Option<String>,
    /// Commits on the branch that no other branch has. None when unknown.
    pub unmerged: Option<usize>,
    /// Uncommitted changes. None when unknown.
    pub dirty: Option<bool>,
    pub bytes: u64,
    pub running: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionRecord {
    pub path: PathBuf,
    pub id: String,
    pub started: u64,
    pub ended: Option<u64>,
    pub running: bool,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct KeyringFile {
    pub path: PathBuf,
    pub bytes: u64,
    /// A sandbox home with this password still exists.
    pub home_exists: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SizeKind {
    /// Bytes only these files use, from `btrfs filesystem du`. Data shared
    /// with another file (reflinks, snapshots, dedupe) is not counted.
    BtrfsExclusive,
    /// Allocated blocks, as `du` counts them.
    Allocated,
}

#[derive(Debug, Clone, Serialize)]
pub struct Inventory {
    pub size_kind: SizeKind,
    pub homes: Vec<Home>,
    pub images: Vec<Image>,
    pub worktrees: Vec<Worktree>,
    pub sessions: Vec<SessionRecord>,
    pub keyrings: Vec<KeyringFile>,
    /// Things paddock could not check, such as a stopped docker daemon.
    pub notes: Vec<String>,
    /// Shared cache folders that this version no longer uses.
    pub old_caches: Vec<OldCache>,
    /// Some sandbox runs. A sandbox of an older paddock can still mount an
    /// old cache, so gc keeps them while any sandbox runs.
    pub any_running: bool,
    /// Records of paddock's base image builds in the docker build cache.
    pub build_cache: Vec<BuildRecord>,
    /// Scratch projects of `paddock wrap` starts in the home folder.
    pub scratch: Vec<Scratch>,
    /// Project snapshots (`paddock undo`) and the undo trash.
    pub snapshots: Vec<SnapshotDir>,
    /// `[storage] tmpfs_caches`: gc clears the pure caches in the homes.
    pub tmpfs_caches: bool,
    /// The bytes of each cache class, when measured.
    pub classes: Vec<ClassUse>,
    /// The shared cache layers and the layers of each project.
    pub layers: Vec<layers::Layer>,
}

/// A project snapshot, or a folder of the undo trash (`trash`).
#[derive(Debug, Clone, Serialize)]
pub struct SnapshotDir {
    pub path: PathBuf,
    pub project: Option<PathBuf>,
    pub harness: String,
    pub started: u64,
    pub bytes: u64,
    pub project_exists: bool,
    pub trash: bool,
}

/// The disk use of one cache class (storage/caches.rs).
#[derive(Debug, Clone, Serialize)]
pub struct ClassUse {
    pub class: caches::Class,
    pub bytes: u64,
    pub folders: usize,
}

/// A scratch project (`$XDG_DATA_HOME/paddock/scratch/<harness>`): where an
/// agent from `omarchy-agent` works when it starts in the home folder. Its
/// files are the user's work.
#[derive(Debug, Clone, Serialize)]
pub struct Scratch {
    pub harness: String,
    pub path: PathBuf,
    pub bytes: u64,
    /// Entries at the top of the project.
    pub entries: usize,
    /// The newest change of the project or a start in it.
    pub last_active: u64,
    pub running: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct BuildRecord {
    pub cli: String,
    pub id: String,
    /// Bytes that no image or other record shares.
    pub bytes: u64,
    pub description: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct OldCache {
    pub path: PathBuf,
    pub bytes: u64,
    pub replaced_by: String,
}

pub fn scratch_root(env: &HostEnv) -> PathBuf {
    env.paddock_data().join("scratch")
}

pub(super) fn homes_root(env: &HostEnv) -> PathBuf {
    env.paddock_data().join("homes")
}

pub(super) fn worktrees_root(env: &HostEnv) -> PathBuf {
    env.paddock_data().join("worktrees")
}

pub(super) fn keyring_root(env: &HostEnv) -> PathBuf {
    env.paddock_state().join("keyring")
}

pub(super) fn sessions_root(env: &HostEnv) -> PathBuf {
    env.paddock_state().join("sessions")
}

pub(super) fn shared_cache(env: &HostEnv) -> PathBuf {
    env.paddock_cache().join("shared")
}

/// The directories where gc may delete.
pub fn removable_roots(env: &HostEnv) -> Vec<PathBuf> {
    vec![
        homes_root(env),
        worktrees_root(env),
        keyring_root(env),
        sessions_root(env),
        scratch_root(env),
        crate::borrow::root(env),
        crate::borrow::meta_root(env),
        crate::snapshot::root(env),
        crate::snapshot::trash_root(env),
        layers::projects_root(env),
    ]
}

/// The harness name of the homes that `paddock doctor --escape` makes.
pub const DOCTOR_HARNESS: &str = "doctor";
