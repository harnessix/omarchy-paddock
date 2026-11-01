//! Disk use of paddock's own data, and its cleanup: `paddock du` and
//! `paddock gc`.
//!
//! A sandbox can write its own home, so everything read from a home, such as
//! `.paddock-origin`, is a hint. The identity of a home comes from its
//! directory key (`homes/<harness>/<login>/<slug>`). A project path from an
//! origin file counts only when its slug matches that key. paddock deletes
//! only inside its own directories, never through a symlink, and never the
//! home or worktree of a running sandbox.

use crate::env::HostEnv;
use crate::manifest::hex;
use crate::recover::Session;
use crate::state::{self, AuthMode, Running};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

pub mod dedupe;

pub const ORIGIN_FILE: &str = ".paddock-origin";
const ORIGIN_VERSION: u32 = 1;
/// Larger origin files are not paddock's: ignore them.
const ORIGIN_MAX: u64 = 64 * 1024;
const DAY: u64 = 86_400;
/// Ended session records older than this go in the automatic tier.
pub const SESSION_DAYS: u64 = 30;
pub const DEFAULT_OLDER_THAN: u64 = 90;
/// Folders under `$XDG_CACHE_HOME/paddock/shared` that paddock no longer
/// mounts, with the folder that replaced each.
const OLD_CACHES: &[(&str, &str)] = &[("mise-installs", "mise-tools")];

/// Where a home came from, written by paddock at each start. The sandbox can
/// rewrite it, so it is a hint only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Origin {
    pub version: u32,
    pub project: PathBuf,
    pub harness: String,
    pub auth: String,
    pub last_used: u64,
    /// A desktop app: its login lives in the home.
    #[serde(default)]
    pub app: bool,
}

impl Origin {
    pub fn new(project: &Path, harness: &str, auth: &AuthMode, app: bool) -> Self {
        Self {
            version: ORIGIN_VERSION,
            project: project.to_path_buf(),
            harness: harness.to_string(),
            auth: auth.to_string(),
            last_used: state::now(),
            app,
        }
    }
}

/// Write `.paddock-origin` into a home. The sandbox may have left a symlink
/// or a pipe there: write a new file next to it, then rename over it, so the
/// write never follows a link out of the home.
pub fn write_origin(home: &Path, origin: &Origin) -> Result<()> {
    let tmp = home.join(format!("{ORIGIN_FILE}.{}.tmp", std::process::id()));
    let open = || {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&tmp)
    };
    let mut f = match open() {
        Ok(f) => f,
        Err(_) => {
            // A leftover of an earlier run, or something the sandbox put there.
            // remove_file removes a symlink itself, not its target.
            let _ = std::fs::remove_file(&tmp);
            open().with_context(|| format!("create {}", tmp.display()))?
        }
    };
    f.write_all(serde_json::to_string_pretty(origin)?.as_bytes())?;
    drop(f);
    std::fs::rename(&tmp, home.join(ORIGIN_FILE))?;
    Ok(())
}

/// Read `.paddock-origin` without following a symlink and without blocking on
/// a pipe. Anything that is not a small regular file with valid JSON is None.
pub fn read_origin(home: &Path) -> Option<Origin> {
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(home.join(ORIGIN_FILE))
        .ok()?;
    let meta = f.metadata().ok()?;
    if !meta.is_file() || meta.len() > ORIGIN_MAX {
        return None;
    }
    let mut text = String::new();
    Read::by_ref(&mut f)
        .take(ORIGIN_MAX)
        .read_to_string(&mut text)
        .ok()?;
    let o: Origin = serde_json::from_str(&text).ok()?;
    (o.version >= 1).then_some(o)
}

/// The directory name of a login under `homes/<harness>/`.
pub fn login_dir(auth: &AuthMode) -> String {
    match auth {
        AuthMode::Shared => "shared".into(),
        AuthMode::Isolated(n) => format!("isolated-{n}"),
        AuthMode::Account { key, .. } => format!("account-{key}"),
    }
}

fn auth_from_dir(login: &str) -> Option<AuthMode> {
    if login == "shared" {
        return Some(AuthMode::Shared);
    }
    if let Some(key) = login.strip_prefix("account-") {
        return format!("account:{key}").parse().ok();
    }
    let name = login.strip_prefix("isolated-")?;
    format!("isolated:{name}").parse().ok()
}

/// Does the origin describe this home? The slug carries a hash of the
/// project path, so a sandbox cannot claim another project for its home.
fn origin_matches(o: &Origin, harness: &str, auth: &str, key: &str) -> bool {
    o.harness == harness
        && o.auth == auth
        && o.project.is_absolute()
        && state::slug(&o.project) == key
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum OriginCheck {
    /// The origin file matches the home's directory key.
    Ok,
    Missing,
    /// The file names another project, harness or login: ignored.
    Mismatch,
}

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
}

impl Home {
    /// The age that gc uses: the newest of every sign of use. The sandbox
    /// can only make its home look newer than that, never older.
    fn last_active(&self) -> u64 {
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

/// The snapshots and the undo trash folders.
pub fn scan_snapshots(env: &HostEnv) -> Vec<SnapshotDir> {
    let mut out = Vec::new();
    for base in subdirs(&crate::snapshot::root(env)) {
        for (path, m) in crate::snapshot::list(&base) {
            out.push(SnapshotDir {
                path,
                project_exists: m.project.is_dir(),
                project: Some(m.project),
                harness: m.harness,
                started: m.started,
                bytes: m.bytes,
                trash: false,
            });
        }
    }
    for path in subdirs(&crate::snapshot::trash_root(env)) {
        let started = std::fs::metadata(&path)
            .map(|m| m.mtime().max(0) as u64)
            .unwrap_or(0);
        out.push(SnapshotDir {
            bytes: allocated(&path),
            path,
            project: None,
            harness: String::new(),
            started,
            project_exists: true,
            trash: true,
        });
    }
    out
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

pub fn scratch_root(env: &HostEnv) -> PathBuf {
    env.paddock_data().join("scratch")
}

#[derive(Debug, Clone, Serialize)]
pub struct BuildRecord {
    pub cli: String,
    pub id: String,
    /// Bytes that no image or other record shares.
    pub bytes: u64,
    pub description: String,
}

/// "1.509GB", "807.8MB", "4.096kB", "98B": docker's decimal sizes.
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let i = s.find(|c: char| !(c.is_ascii_digit() || c == '.'))?;
    let (num, unit) = s.split_at(i);
    let n: f64 = num.parse().ok()?;
    let mult = match unit.trim() {
        "B" => 1.0,
        "kB" | "KB" => 1e3,
        "MB" => 1e6,
        "GB" => 1e9,
        "TB" => 1e12,
        "KiB" => 1024.0,
        "MiB" => 1048576.0,
        "GiB" => 1073741824.0,
        _ => return None,
    };
    Some((n * mult) as u64)
}

/// paddock's records in the docker build cache: their RUN lines carry the
/// marker, or the commands of base images built before it existed. Only the
/// bytes that nothing else shares count.
fn scan_build_cache(host: &Host, cli: &str) -> Vec<BuildRecord> {
    use crate::backend::docker::{BUILD_MARKER, OLD_BUILD_COMMANDS};
    if cli != "docker" {
        return vec![];
    }
    let Ok(text) = host
        .containers
        .run(cli, &["buildx", "du", "--format", "json"])
    else {
        return vec![];
    };
    let marker = format!(": {BUILD_MARKER} ");
    text.lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["Reclaimable"].as_bool() == Some(true))
        .filter_map(|v| {
            let d = v["Description"].as_str().unwrap_or_default();
            let ours = d.contains(&marker) || OLD_BUILD_COMMANDS.iter().any(|c| d.contains(c));
            let id = v["ID"].as_str().unwrap_or_default();
            (ours && !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric())).then(|| {
                BuildRecord {
                    cli: cli.to_string(),
                    id: id.to_string(),
                    bytes: if v["Shared"].as_bool() == Some(true) {
                        0
                    } else {
                        parse_size(v["Size"].as_str().unwrap_or("0B")).unwrap_or(0)
                    },
                    description: d.chars().take(120).collect(),
                }
            })
        })
        .collect()
}

/// The image table of `podman system df -v`: columns are separated by two
/// or more spaces; IMAGE ID is short. Returns (short id, unique bytes).
pub fn parse_podman_df(text: &str) -> Vec<(String, u64)> {
    let mut out = Vec::new();
    let mut in_images = false;
    for line in text.lines() {
        if line.starts_with("Images space usage") {
            in_images = true;
            continue;
        }
        if in_images && line.trim().is_empty() && !out.is_empty() {
            break;
        }
        if !in_images || line.starts_with("REPOSITORY") || line.trim().is_empty() {
            continue;
        }
        let cols: Vec<&str> = line
            .split("  ")
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .collect();
        if cols.len() >= 8 {
            if let Some(u) = parse_size(cols[6]) {
                out.push((cols[2].to_string(), u));
            }
        }
    }
    out
}

/// Unique sizes of images, from `docker system df -v` or `podman system df -v`.
fn unique_sizes(host: &Host, cli: &str) -> BTreeMap<String, u64> {
    let mut out = BTreeMap::new();
    if cli == "podman" {
        if let Ok(text) = host.containers.run(cli, &["system", "df", "-v"]) {
            out.extend(parse_podman_df(&text));
        }
        return out;
    }
    if cli != "docker" {
        return out;
    }
    if let Ok(text) = host
        .containers
        .run(cli, &["system", "df", "-v", "--format", "json"])
    {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
            for i in v["Images"].as_array().into_iter().flatten() {
                if let (Some(id), Some(u)) = (
                    i["ID"].as_str(),
                    i["UniqueSize"].as_str().and_then(parse_size),
                ) {
                    out.insert(strip_sha(id).to_string(), u);
                }
            }
        }
    }
    out
}

#[derive(Debug, Clone, Serialize)]
pub struct OldCache {
    pub path: PathBuf,
    pub bytes: u64,
    pub replaced_by: String,
}

/// The docker and podman commands, behind a trait so tests can fake them.
pub trait Containers {
    /// Run `cli args...` and return stdout. Err when the command fails.
    fn run(&self, cli: &str, args: &[&str]) -> Result<String>;
    /// The installed CLIs among docker and podman.
    fn clis(&self) -> Vec<String>;
}

pub struct RealContainers<'a>(pub &'a HostEnv);

impl Containers for RealContainers<'_> {
    fn run(&self, cli: &str, args: &[&str]) -> Result<String> {
        let exe = self
            .0
            .which(cli)
            .with_context(|| format!("{cli} not found"))?;
        crate::env::capture(Command::new(exe).args(args).stdin(Stdio::null()))
    }
    fn clis(&self) -> Vec<String> {
        ["docker", "podman"]
            .into_iter()
            .filter(|c| self.0.which(c).is_some())
            .map(str::to_string)
            .collect()
    }
}

/// Facts from outside the data folders: containers, current image tags and
/// the known profiles.
pub struct Host<'a> {
    pub containers: &'a dyn Containers,
    pub current_tags: BTreeSet<String>,
    /// Profile name to "is a desktop app". None when the profiles cannot be
    /// read: then no home counts as orphaned.
    pub profiles: Option<BTreeMap<String, bool>>,
}

impl<'a> Host<'a> {
    pub fn real(env: &HostEnv, containers: &'a dyn Containers) -> Self {
        let profiles = crate::profile::HarnessFactory::new(env).ok().map(|f| {
            f.names()
                .into_iter()
                .map(|n| {
                    let app = f.profile(&n).is_some_and(|p| p.app.is_some());
                    (n, app)
                })
                .collect()
        });
        Self {
            containers,
            current_tags: current_tags(env),
            profiles,
        }
    }
}

/// The image tags that paddock would use now: the user configuration and
/// each recent project, with and without the desktop, app and keyring parts.
pub fn current_tags(env: &HostEnv) -> BTreeSet<String> {
    use crate::backend::docker::{dockerfile, image_tag, ImageCfg};
    let mut cfgs = Vec::new();
    let mut dirs = vec![env.home.join(".paddock-no-project")];
    dirs.extend(state::recent(env).into_iter().map(|r| r.project));
    for d in dirs {
        if let Ok(l) = crate::manifest::Manifest::load(env, &d) {
            let c = (l.merged.docker.image, l.merged.docker.packages);
            if !cfgs.contains(&c) {
                cfgs.push(c);
            }
        }
    }
    if cfgs.is_empty() {
        cfgs.push((None, vec![]));
    }
    let mut tags = BTreeSet::new();
    for (image, packages) in cfgs {
        for bits in 0..8u8 {
            let cfg = ImageCfg {
                image: image.clone(),
                packages: packages.clone(),
                gui: bits & 1 != 0,
                app: bits & 2 != 0,
                keyring: bits & 4 != 0,
            };
            if let Ok(df) = dockerfile(&cfg) {
                tags.insert(image_tag(&df));
            }
        }
    }
    tags
}

fn homes_root(env: &HostEnv) -> PathBuf {
    env.paddock_data().join("homes")
}
fn worktrees_root(env: &HostEnv) -> PathBuf {
    env.paddock_data().join("worktrees")
}
fn keyring_root(env: &HostEnv) -> PathBuf {
    env.paddock_state().join("keyring")
}
fn sessions_root(env: &HostEnv) -> PathBuf {
    env.paddock_state().join("sessions")
}
fn shared_cache(env: &HostEnv) -> PathBuf {
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
    ]
}

/// The harness name of the homes that `paddock doctor --escape` makes.
pub const DOCTOR_HARNESS: &str = "doctor";

/// Remove the homes of `paddock doctor --escape` runs that are over: the
/// verified origin names a project that is gone, and no sandbox runs in the
/// home. Each run has its own scratch project, so each run has its own home.
/// A home without a verified origin stays for `paddock gc`. Returns the
/// removed homes.
pub fn remove_doctor_homes(env: &HostEnv) -> Vec<PathBuf> {
    let running: BTreeSet<PathBuf> = Running::list(env)
        .into_iter()
        .filter(|r| r.harness == DOCTOR_HARNESS)
        .filter_map(|r| {
            let auth = r.auth.parse::<AuthMode>().ok()?;
            Some(
                homes_root(env)
                    .join(DOCTOR_HARNESS)
                    .join(login_dir(&auth))
                    .join(state::slug(&r.project)),
            )
        })
        .collect();
    let mut removed = Vec::new();
    for ldir in subdirs(&homes_root(env).join(DOCTOR_HARNESS)) {
        let Some(auth) = auth_from_dir(&name(&ldir)) else {
            continue;
        };
        let auth = auth.to_string();
        for path in subdirs(&ldir) {
            if running.contains(&path) {
                continue;
            }
            let over = read_origin(&path).is_some_and(|o| {
                origin_matches(&o, DOCTOR_HARNESS, &auth, &name(&path)) && !o.project.exists()
            });
            if over && crate::safefs::remove_tree_path(&path).is_ok() {
                removed.push(path);
            }
        }
    }
    removed
}

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

fn mtime(p: &Path) -> u64 {
    std::fs::symlink_metadata(p)
        .map(|m| m.mtime().max(0) as u64)
        .unwrap_or(0)
}

fn newest_change(home: &Path) -> u64 {
    std::fs::read_dir(home)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| mtime(&e.path()))
        .fold(mtime(home), u64::max)
}

fn keyring_key(home: &Path) -> String {
    hex(&Sha256::digest(home.to_string_lossy().as_bytes()))[..24].to_string()
}

fn loses(app: bool, harness: &str, auth: &str) -> String {
    if app {
        return "the app's login, settings and local data in this home".into();
    }
    let login = match auth.strip_prefix("isolated:") {
        Some(n) => format!("the login stays in ~/.local/share/paddock/auth/{harness}/{n}"),
        None => "the login stays in your host home".into(),
    };
    format!("transcripts, history and settings of this project; {login}")
}

/// A query about refs and objects. The repositories and worktrees here are
/// writable by sandboxes, so the call never reads their configuration and
/// never touches a work tree (see hostgit.rs).
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    crate::hostgit::Layout::discover(dir)
        .ok()
        .flatten()?
        .try_query(args)
}

struct ListedWorktree {
    path: PathBuf,
    head: Option<String>,
    branch: Option<String>,
}

fn list_worktrees(repo: &Path) -> Vec<ListedWorktree> {
    let Some(out) = git(repo, &["worktree", "list", "--porcelain"]) else {
        return vec![];
    };
    let mut v = Vec::new();
    for block in out.split("\n\n") {
        let mut w = ListedWorktree {
            path: PathBuf::new(),
            head: None,
            branch: None,
        };
        for l in block.lines() {
            if let Some(p) = l.strip_prefix("worktree ") {
                w.path = PathBuf::from(p);
            } else if let Some(h) = l.strip_prefix("HEAD ") {
                w.head = Some(h.to_string());
            } else if let Some(b) = l.strip_prefix("branch refs/heads/") {
                w.branch = Some(b.to_string());
            }
        }
        if !w.path.as_os_str().is_empty() {
            v.push(w);
        }
    }
    v
}

/// Facts about one worktree. The git directory of a linked worktree is not
/// taken from its `.git` file: `Layout::discover` checks that the repository
/// registered this worktree (see hostgit.rs), so there is nothing to look up
/// by hand here.
fn worktree_facts(
    env: &HostEnv,
    repo: &Path,
    listed: &ListedWorktree,
) -> (Option<usize>, Option<bool>) {
    let unmerged = listed.head.as_ref().and_then(|head| {
        let ex = listed.branch.as_ref().map(|b| format!("--exclude={b}"));
        let mut args = vec!["rev-list", "--count", head.as_str(), "--not"];
        if let Some(ex) = &ex {
            args.push(ex);
        }
        args.extend(["--branches", "--remotes"]);
        git(repo, &args).and_then(|s| s.trim().parse().ok())
    });
    // The work tree holds what a sandbox wrote, so `git status` runs in a
    // sandbox of its own. The layout check also confirms that this worktree
    // is one the repository registered. None means "not known".
    let common = crate::hostgit::Layout::discover(repo)
        .ok()
        .flatten()
        .map(|l| l.common_dir);
    let dirty = crate::hostgit::Layout::discover(&listed.path)
        .ok()
        .flatten()
        .filter(|l| Some(&l.common_dir) == common.as_ref())
        .and_then(|l| l.status(env, "normal").ok().flatten())
        .map(|lines| lines.iter().any(|l| !l.trim().is_empty()));
    (unmerged, dirty)
}

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

fn file_len(p: &Path) -> u64 {
    std::fs::symlink_metadata(p).map(|m| m.len()).unwrap_or(0)
}

fn strip_registry(tag: &str) -> &str {
    tag.strip_prefix("localhost/")
        .or_else(|| tag.strip_prefix("docker.io/library/"))
        .unwrap_or(tag)
}

fn strip_sha(id: &str) -> &str {
    id.trim().strip_prefix("sha256:").unwrap_or(id.trim())
}

fn scan_images(host: &Host, cli: &str, notes: &mut Vec<String>) -> Vec<Image> {
    let c = host.containers;
    let ids = match c.run(
        cli,
        &[
            "images",
            "-q",
            "--no-trunc",
            "--filter",
            "label=io.paddock.base=1",
        ],
    ) {
        Ok(s) => s,
        Err(_) => {
            notes.push(format!(
                "{cli}: cannot list images (is the daemon running?)"
            ));
            return vec![];
        }
    };
    let mut ids: Vec<&str> = ids
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    ids.sort();
    ids.dedup();
    if ids.is_empty() {
        return vec![];
    }
    let mut args = vec!["image", "inspect"];
    args.extend(&ids);
    let Ok(text) = c.run(cli, &args) else {
        notes.push(format!("{cli}: cannot inspect the paddock images"));
        return vec![];
    };
    let list: Vec<serde_json::Value> = serde_json::from_str(&text).unwrap_or_default();
    // Every container, running or stopped, keeps its image.
    let mut used = BTreeSet::new();
    match c.run(cli, &["ps", "-a", "-q", "--no-trunc"]) {
        Ok(cids) => {
            let cids: Vec<&str> = cids
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .collect();
            if !cids.is_empty() {
                let mut a = vec!["container", "inspect", "--format", "{{.Image}}"];
                a.extend(&cids);
                match c.run(cli, &a) {
                    Ok(out) => used.extend(out.lines().map(|l| strip_sha(l).to_string())),
                    Err(_) => {
                        notes.push(format!(
                            "{cli}: cannot inspect containers; keeping every image"
                        ));
                        return vec![];
                    }
                }
            }
        }
        Err(_) => {
            notes.push(format!(
                "{cli}: cannot list containers; keeping every image"
            ));
            return vec![];
        }
    }
    let unique = unique_sizes(host, cli);
    let mut out = Vec::new();
    for v in list {
        let id = strip_sha(v["Id"].as_str().unwrap_or_default()).to_string();
        let tags: Vec<String> = v["RepoTags"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|t| t.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        // An image built FROM paddock-base inherits the label. Only images
        // whose every tag is paddock's own are paddock's.
        if id.is_empty()
            || tags.is_empty()
            || !tags
                .iter()
                .all(|t| strip_registry(t).starts_with("paddock-base:"))
        {
            continue;
        }
        out.push(Image {
            cli: cli.to_string(),
            current: tags
                .iter()
                .any(|t| host.current_tags.contains(strip_registry(t))),
            in_use: used.contains(&id),
            bytes: v["Size"].as_u64().unwrap_or(0),
            // podman reports short ids.
            unique: unique.get(&id).copied().or_else(|| {
                unique
                    .iter()
                    .find(|(k, _)| k.len() >= 12 && id.starts_with(k.as_str()))
                    .map(|(_, v)| *v)
            }),
            created: v["Created"]
                .as_str()
                .unwrap_or_default()
                .chars()
                .take(10)
                .collect(),
            id,
            tags,
        });
    }
    out
}

/// Look at every piece of paddock data. With `with_sizes`, measure homes and
/// worktrees too, which can take some seconds.
pub fn scan(env: &HostEnv, host: &Host, with_sizes: bool) -> Inventory {
    let now = state::now();
    let mut notes = Vec::new();
    let running: Vec<Running> = Running::list(env);
    let running_ids: BTreeSet<&str> = running.iter().map(|r| r.id.as_str()).collect();
    let sessions = Session::all(env);

    let mut running_homes: BTreeSet<PathBuf> = sessions
        .iter()
        .filter(|s| running_ids.contains(s.id.as_str()))
        .map(|s| s.home.clone())
        .collect();
    for r in &running {
        if let Ok(auth) = r.auth.parse::<AuthMode>() {
            running_homes.insert(
                homes_root(env)
                    .join(&r.harness)
                    .join(login_dir(&auth))
                    .join(state::slug(&r.project)),
            );
        }
    }
    let running_projects: BTreeSet<&Path> = running.iter().map(|r| r.project.as_path()).collect();
    let mut session_last: BTreeMap<&Path, u64> = BTreeMap::new();
    for s in &sessions {
        let t = s.ended.unwrap_or(s.started).max(s.started).min(now);
        let e = session_last.entry(s.home.as_path()).or_insert(0);
        *e = (*e).max(t);
    }

    // Homes.
    let mut homes = Vec::new();
    for hdir in subdirs(&homes_root(env)) {
        let harness = name(&hdir);
        for ldir in subdirs(&hdir) {
            let Some(auth) = auth_from_dir(&name(&ldir)) else {
                continue;
            };
            let auth = auth.to_string();
            for path in subdirs(&ldir) {
                let key = name(&path);
                let origin = read_origin(&path);
                let check = match &origin {
                    None => OriginCheck::Missing,
                    Some(o) if origin_matches(o, &harness, &auth, &key) => OriginCheck::Ok,
                    Some(_) => OriginCheck::Mismatch,
                };
                let origin = origin.filter(|_| check == OriginCheck::Ok);
                let project = origin.as_ref().map(|o| o.project.clone());
                let used = [
                    origin.as_ref().map(|o| o.last_used.min(now)),
                    session_last.get(path.as_path()).copied(),
                ]
                .into_iter()
                .flatten()
                .max();
                let (profile, app) = match &host.profiles {
                    // The escape probe's own harness has no profile file.
                    _ if harness == DOCTOR_HARNESS => (true, false),
                    Some(p) => match p.get(&harness) {
                        Some(app) => (true, *app),
                        None => (false, origin.as_ref().is_some_and(|o| o.app)),
                    },
                    None => (true, origin.as_ref().is_some_and(|o| o.app)),
                };
                homes.push(Home {
                    project_exists: project.as_ref().map(|p| p.is_dir()),
                    project,
                    origin: check,
                    last_used: used,
                    modified: newest_change(&path),
                    bytes: 0,
                    loses: loses(app, &harness, &auth),
                    app,
                    profile,
                    running: running_homes.contains(&path),
                    harness: harness.clone(),
                    auth: auth.clone(),
                    path,
                });
            }
        }
    }
    if host.profiles.is_none() {
        notes.push("cannot read the harness profiles; no home counts as orphaned".into());
    }

    // Worktrees. The repository of each comes from host-side records and
    // verified origins, matched by the slug of the worktree folder.
    let mut repos: BTreeMap<String, PathBuf> = BTreeMap::new();
    let candidates = state::recent(env)
        .into_iter()
        .map(|r| r.project)
        .chain(sessions.iter().map(|s| s.project.clone()))
        .chain(homes.iter().filter_map(|h| h.project.clone()));
    for p in candidates {
        if p.is_dir() {
            repos.entry(state::slug(&p)).or_insert(p);
        }
    }
    let mut listed: BTreeMap<PathBuf, Vec<ListedWorktree>> = BTreeMap::new();
    let mut worktrees = Vec::new();
    for sdir in subdirs(&worktrees_root(env)) {
        let repo = repos.get(&name(&sdir)).cloned();
        for path in subdirs(&sdir) {
            let mut w = Worktree {
                running: running_projects.contains(path.as_path()),
                path: path.clone(),
                repo: None,
                branch: None,
                unmerged: None,
                dirty: None,
                bytes: 0,
            };
            if let Some(repo) = &repo {
                let list = listed
                    .entry(repo.clone())
                    .or_insert_with(|| list_worktrees(repo));
                if let Some(l) = list.iter().find(|l| l.path == path) {
                    let (unmerged, dirty) = worktree_facts(env, repo, l);
                    w.repo = Some(repo.clone());
                    w.branch = l.branch.clone();
                    w.unmerged = unmerged;
                    w.dirty = dirty;
                }
            }
            worktrees.push(w);
        }
    }

    // Session records.
    let session_records = sessions
        .iter()
        .map(|s| {
            let path = sessions_root(env).join(format!("{}.json", s.id));
            SessionRecord {
                bytes: file_len(&path),
                path,
                id: s.id.clone(),
                started: s.started,
                ended: s.ended,
                running: running_ids.contains(s.id.as_str()),
            }
        })
        .collect();

    // Keyring passwords: the file name is a hash of the home path.
    let mut known: BTreeSet<String> = homes.iter().map(|h| keyring_key(&h.path)).collect();
    known.extend(
        sessions
            .iter()
            .filter(|s| s.home.is_dir())
            .map(|s| keyring_key(&s.home)),
    );
    known.insert(keyring_key(&env.home));
    let mut keyrings: Vec<KeyringFile> = std::fs::read_dir(keyring_root(env))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .map(|e| KeyringFile {
            bytes: file_len(&e.path()),
            home_exists: known.contains(&name(&e.path())),
            path: e.path(),
        })
        .collect();
    keyrings.sort_by(|a, b| a.path.cmp(&b.path));

    let mut images = Vec::new();
    let mut build_cache = Vec::new();
    for cli in host.containers.clis() {
        images.extend(scan_images(host, &cli, &mut notes));
        build_cache.extend(scan_build_cache(host, &cli));
    }

    let old_caches = OLD_CACHES
        .iter()
        .map(|(old, new)| (shared_cache(env).join(old), new))
        .filter(|(p, _)| std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir()))
        .map(|(path, new)| OldCache {
            path,
            bytes: 0,
            replaced_by: new.to_string(),
        })
        .collect();

    let scratch = subdirs(&scratch_root(env))
        .into_iter()
        .map(|path| {
            let entries = std::fs::read_dir(&path).map(|d| d.count()).unwrap_or(0);
            let started = homes
                .iter()
                .filter(|h| h.project.as_deref() == Some(path.as_path()))
                .map(|h| h.last_active())
                .max()
                .unwrap_or(0);
            let changed = std::fs::metadata(&path)
                .map(|m| m.mtime().max(0) as u64)
                .unwrap_or(0);
            Scratch {
                harness: name(&path),
                last_active: changed.max(started),
                running: running_projects.contains(path.as_path()),
                entries,
                bytes: 0,
                path,
            }
        })
        .collect();
    let mut inv = Inventory {
        size_kind: SizeKind::Allocated,
        homes,
        images,
        worktrees,
        sessions: session_records,
        keyrings,
        notes,
        old_caches,
        any_running: !running.is_empty(),
        build_cache,
        scratch,
        snapshots: scan_snapshots(env),
    };
    if with_sizes {
        measure(&mut inv, |_| true);
    }
    inv
}

/// Fill in the sizes of the homes and worktrees that `want` names.
fn measure(inv: &mut Inventory, want: impl Fn(&Path) -> bool) {
    let paths: Vec<PathBuf> = inv
        .homes
        .iter()
        .map(|h| &h.path)
        .chain(inv.worktrees.iter().map(|w| &w.path))
        .chain(inv.old_caches.iter().map(|c| &c.path))
        .chain(inv.scratch.iter().map(|c| &c.path))
        .filter(|p| want(p))
        .cloned()
        .collect();
    let (v, kind) = sizes(&paths);
    inv.size_kind = kind;
    let by: BTreeMap<&PathBuf, u64> = paths.iter().zip(v).collect();
    for h in &mut inv.homes {
        if let Some(b) = by.get(&h.path) {
            h.bytes = *b;
        }
    }
    for w in &mut inv.worktrees {
        if let Some(b) = by.get(&w.path) {
            w.bytes = *b;
        }
    }
    for c in &mut inv.scratch {
        if let Some(b) = by.get(&c.path) {
            c.bytes = *b;
        }
    }
    for c in &mut inv.old_caches {
        if let Some(b) = by.get(&c.path) {
            c.bytes = *b;
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct GcOptions {
    pub yes: bool,
    pub homes: bool,
    pub older_than_days: u64,
    pub include_app_logins: bool,
    pub worktrees: bool,
    pub force: bool,
    /// Also scratch projects that were not used for `older_than_days`.
    pub scratch: bool,
    /// Also share the data of identical files in the sandbox homes.
    pub dedupe: bool,
    /// Also project snapshots and undo trash folders older than
    /// `older_than_days`.
    pub snapshots: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    /// Safe to remove without a question: `--yes` alone.
    Auto,
    /// Needs a flag that names it, such as `--homes`.
    Consent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Action {
    Image {
        cli: String,
        tags: Vec<String>,
    },
    BuildCache {
        cli: String,
        ids: Vec<String>,
    },
    Path {
        path: PathBuf,
    },
    Worktree {
        path: PathBuf,
        repo: Option<PathBuf>,
        branch: Option<String>,
        /// Delete the branch too: it has no commits of its own.
        delete_branch: bool,
        /// Remove with uncommitted changes.
        force: bool,
    },
    /// paddock kv: the `self` data of these finished runs, and expired keys.
    Kv {
        sandboxes: Vec<String>,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct Item {
    pub what: String,
    pub tier: Tier,
    pub bytes: u64,
    pub reason: String,
    pub warning: Option<String>,
    /// Flags still missing before `--yes` removes it. Empty: selected.
    pub needs: Vec<String>,
    pub action: Action,
}

impl Item {
    pub fn selected(&self) -> bool {
        self.needs.is_empty()
    }
}

fn need(v: &mut Vec<String>, have: bool, flag: &str) {
    if !have {
        v.push(flag.to_string());
    }
}

fn home_label(h: &Home) -> String {
    format!(
        "home {}/{}/{}",
        h.harness,
        login_dir(&h.auth.parse().unwrap_or(AuthMode::Shared)),
        name(&h.path)
    )
}

/// Decide what gc removes. Pure: everything comes from the inventory.
pub fn plan(inv: &Inventory, o: &GcOptions, now: u64) -> Vec<Item> {
    let mut items = Vec::new();
    for c in &inv.old_caches {
        if inv.any_running {
            continue;
        }
        items.push(Item {
            what: format!("old cache {}", c.path.display()),
            tier: Tier::Auto,
            bytes: c.bytes,
            reason: format!("paddock no longer uses it; it uses {} now", c.replaced_by),
            warning: None,
            needs: vec![],
            action: Action::Path {
                path: c.path.clone(),
            },
        });
    }
    for i in &inv.images {
        if i.current || i.in_use {
            continue;
        }
        items.push(Item {
            what: format!("image {} ({})", i.tags.join(", "), i.cli),
            tier: Tier::Auto,
            // Images share layers: only the unique part is freed.
            bytes: i.unique.unwrap_or(i.bytes),
            reason: "not a current paddock image, and no container uses it".into(),
            warning: None,
            needs: vec![],
            action: Action::Image {
                cli: i.cli.clone(),
                tags: i.tags.clone(),
            },
        });
    }
    // paddock's base image builds in the docker build cache. They hold the
    // layers of removed images; current images keep their own layers, so a
    // removed record only costs time at the next build.
    let mut by_cli: BTreeMap<&str, Vec<&BuildRecord>> = BTreeMap::new();
    for r in &inv.build_cache {
        by_cli.entry(&r.cli).or_default().push(r);
    }
    for (cli, recs) in by_cli {
        items.push(Item {
            what: format!("paddock build cache ({cli}, {} records)", recs.len()),
            tier: Tier::Auto,
            bytes: recs.iter().map(|r| r.bytes).sum(),
            reason: "build steps of paddock base images; images keep their own layers".into(),
            warning: None,
            needs: vec![],
            action: Action::BuildCache {
                cli: cli.to_string(),
                ids: recs.iter().map(|r| r.id.clone()).collect(),
            },
        });
    }
    for k in inv.keyrings.iter().filter(|k| !k.home_exists) {
        items.push(Item {
            what: format!("keyring password {}", name(&k.path)),
            tier: Tier::Auto,
            bytes: k.bytes,
            reason: "its sandbox home is gone".into(),
            warning: None,
            needs: vec![],
            action: Action::Path {
                path: k.path.clone(),
            },
        });
    }
    let cutoff = now.saturating_sub(SESSION_DAYS * DAY);
    for s in &inv.sessions {
        // A sandbox that crashed never wrote its end: its start counts.
        let end = s.ended.unwrap_or(s.started);
        if s.running || end >= cutoff {
            continue;
        }
        items.push(Item {
            what: format!("session record {}", s.id),
            tier: Tier::Auto,
            bytes: s.bytes,
            reason: format!("ended more than {SESSION_DAYS} days ago"),
            warning: None,
            needs: vec![],
            action: Action::Path {
                path: s.path.clone(),
            },
        });
    }
    let old = now.saturating_sub(o.older_than_days * DAY);
    for h in &inv.homes {
        if h.running {
            continue;
        }
        let login = h
            .app
            .then(|| "the app's login in this home will be lost".to_string());
        let mut needs = Vec::new();
        let (tier, reason) = if h.harness == DOCTOR_HARNESS {
            // Each escape run has its own scratch project and home. A home
            // whose project is gone is a leftover; one without a verified
            // origin is one when it is a day old. A run that is still
            // setting up has its project and is not touched.
            let left = h.project_exists == Some(false)
                || (h.project.is_none() && h.last_active() < now.saturating_sub(DAY));
            if !left {
                continue;
            }
            (
                Tier::Auto,
                "a leftover of an escape test (paddock doctor --escape)".to_string(),
            )
        } else if !h.profile {
            if h.app {
                need(&mut needs, o.homes, "--homes");
                need(&mut needs, o.include_app_logins, "--include-app-logins");
            }
            let tier = if h.app { Tier::Consent } else { Tier::Auto };
            (
                tier,
                format!("the harness profile '{}' no longer exists", h.harness),
            )
        } else if h.project_exists == Some(false) {
            need(&mut needs, o.homes, "--homes");
            if h.app {
                need(&mut needs, o.include_app_logins, "--include-app-logins");
            }
            let p = h.project.as_ref().map(|p| p.display().to_string());
            (
                Tier::Consent,
                format!("the project {} is gone", p.unwrap_or_default()),
            )
        } else if h.last_active() < old {
            need(&mut needs, o.homes, "--homes");
            if h.app {
                need(&mut needs, o.include_app_logins, "--include-app-logins");
            }
            (
                Tier::Consent,
                format!(
                    "not used for {} days",
                    now.saturating_sub(h.last_active()) / DAY
                ),
            )
        } else {
            continue;
        };
        items.push(Item {
            what: home_label(h),
            tier,
            bytes: h.bytes,
            reason,
            warning: login,
            needs,
            action: Action::Path {
                path: h.path.clone(),
            },
        });
    }
    for c in &inv.snapshots {
        let gone = !c.trash && !c.project_exists;
        if !gone && c.started > old {
            continue;
        }
        let mut needs = Vec::new();
        if !gone {
            need(&mut needs, o.snapshots, "--snapshots");
        }
        let what = match &c.project {
            Some(p) if !c.trash => format!("snapshot {} of {}", name(&c.path), p.display()),
            _ => format!("undo trash {}", c.path.display()),
        };
        items.push(Item {
            what,
            tier: if gone { Tier::Auto } else { Tier::Consent },
            bytes: c.bytes,
            reason: if gone {
                "its project is gone".into()
            } else {
                format!("{} days old", now.saturating_sub(c.started) / DAY)
            },
            warning: c
                .trash
                .then(|| "the files that an undo moved away are lost".to_string()),
            needs,
            action: Action::Path {
                path: c.path.clone(),
            },
        });
    }
    for c in &inv.scratch {
        if c.running || c.last_active > old {
            continue;
        }
        let mut needs = Vec::new();
        need(&mut needs, o.scratch, "--scratch");
        items.push(Item {
            what: format!("scratch project {} {}", c.harness, c.path.display()),
            tier: Tier::Consent,
            bytes: c.bytes,
            reason: format!(
                "not used for {} days",
                now.saturating_sub(c.last_active) / DAY
            ),
            warning: (c.entries > 0)
                .then(|| format!("its {} files and folders are lost", c.entries)),
            needs,
            action: Action::Path {
                path: c.path.clone(),
            },
        });
    }
    for w in &inv.worktrees {
        if w.running {
            continue;
        }
        let mut needs = Vec::new();
        need(&mut needs, o.worktrees, "--worktrees");
        let mut risks = Vec::new();
        if w.repo.is_none() {
            risks.push("paddock cannot find its repository, so it cannot check it".to_string());
        } else {
            match w.unmerged {
                Some(0) => {}
                Some(n) => risks.push(format!(
                    "{n} commits on no other branch (the branch is kept)"
                )),
                None => risks.push("paddock cannot count its unmerged commits".into()),
            }
            match w.dirty {
                Some(false) => {}
                Some(true) => risks.push("uncommitted changes will be lost".into()),
                None => risks.push("paddock cannot check for uncommitted changes".into()),
            }
        }
        let risky = !risks.is_empty();
        if risky {
            need(&mut needs, o.force, "--force");
        }
        let delete_branch =
            w.unmerged == Some(0) && w.branch.as_ref().is_some_and(|b| b.starts_with("paddock/"));
        items.push(Item {
            what: format!(
                "worktree {}{}",
                w.path.display(),
                w.branch
                    .as_ref()
                    .map(|b| format!(" ({b})"))
                    .unwrap_or_default()
            ),
            tier: Tier::Consent,
            bytes: w.bytes,
            reason: match &w.repo {
                Some(r) => format!("a paddock worktree of {}", r.display()),
                None => "a paddock worktree".into(),
            },
            warning: risky.then(|| risks.join("; ")),
            needs,
            action: Action::Worktree {
                path: w.path.clone(),
                repo: w.repo.clone(),
                branch: w.branch.clone(),
                delete_branch,
                force: risky,
            },
        });
    }
    items
}

/// Measure only the homes and worktrees that the plan lists.
pub fn measure_plan(inv: &mut Inventory, items: &mut [Item]) {
    let listed: BTreeSet<PathBuf> = items
        .iter()
        .filter_map(|i| match &i.action {
            Action::Path { path } | Action::Worktree { path, .. } => Some(path.clone()),
            _ => None,
        })
        .collect();
    let homes: BTreeSet<&PathBuf> = inv.homes.iter().map(|h| &h.path).collect();
    let wts: BTreeSet<&PathBuf> = inv
        .worktrees
        .iter()
        .map(|w| &w.path)
        .chain(inv.old_caches.iter().map(|c| &c.path))
        .chain(inv.scratch.iter().map(|c| &c.path))
        .collect();
    let want: BTreeSet<PathBuf> = listed
        .into_iter()
        .filter(|p| homes.contains(p) || wts.contains(p))
        .collect();
    if want.is_empty() {
        return;
    }
    measure(inv, |p| want.contains(p));
    let size: BTreeMap<&PathBuf, u64> = inv
        .homes
        .iter()
        .map(|h| (&h.path, h.bytes))
        .chain(inv.worktrees.iter().map(|w| (&w.path, w.bytes)))
        .chain(inv.old_caches.iter().map(|c| (&c.path, c.bytes)))
        .chain(inv.scratch.iter().map(|c| (&c.path, c.bytes)))
        .collect();
    for i in items.iter_mut() {
        if let Action::Path { path } | Action::Worktree { path, .. } = &i.action {
            if let Some(b) = size.get(path) {
                i.bytes = *b;
            }
        }
    }
}

/// Refuse a path that is not strictly inside one of paddock's own
/// directories, that is a symlink, or whose parents lead out through a symlink.
/// Returns that directory, as a real path, and the path below it.
pub fn check_removable(env: &HostEnv, path: &Path) -> Result<(PathBuf, PathBuf)> {
    if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        bail!("{} is not a plain absolute path", path.display());
    }
    let meta = std::fs::symlink_metadata(path)
        .with_context(|| format!("{} does not exist", path.display()))?;
    if meta.file_type().is_symlink() {
        bail!(
            "{} is a symlink; paddock does not follow it",
            path.display()
        );
    }
    let (Some(parent), Some(file)) = (path.parent(), path.file_name()) else {
        bail!("{} has no parent", path.display());
    };
    let parent = std::fs::canonicalize(parent)?;
    // An old shared cache: only the exact folders that paddock replaced.
    if let Ok(shared) = std::fs::canonicalize(shared_cache(env)) {
        if parent == shared && OLD_CACHES.iter().any(|(old, _)| file == *old) {
            return Ok((shared, PathBuf::from(file)));
        }
    }
    for root in removable_roots(env) {
        let Ok(root) = std::fs::canonicalize(&root) else {
            continue;
        };
        if let Ok(rel) = parent.join(file).strip_prefix(&root) {
            if !rel.as_os_str().is_empty() {
                return Ok((root.clone(), rel.to_path_buf()));
            }
        }
    }
    bail!("{} is outside paddock's own directories", path.display())
}

/// Delete a file or a tree after `check_removable`. Never follows a symlink:
/// the walk starts at paddock's own folder and opens one step at a time, and
/// read-only folders (a Go module cache) get write permission through the
/// open folder, not by path (see safefs).
pub fn remove_path(env: &HostEnv, path: &Path) -> Result<()> {
    let (root, rel) = check_removable(env, path)?;
    crate::safefs::Root::open(&root)?
        .remove_tree(&rel)
        .with_context(|| format!("remove {}", path.display()))
}

/// A git command on the repository that may write or read its work tree, for
/// example `worktree remove`, which runs a status internally. It is refused
/// when the repository configuration can start a program (see hostgit.rs).
fn git_write(repo: &Path, args: &[&str]) -> Result<()> {
    crate::hostgit::Checkout::open(repo)?.run(args).map(|_| ())
}

/// The result of one removal.
#[derive(Debug, Clone, Serialize)]
pub struct Removed {
    pub what: String,
    pub bytes: u64,
    pub error: Option<String>,
    pub note: Option<String>,
}

/// Remove the selected items. The running sandboxes are read again first, so
/// one that started after the scan keeps its home.
pub fn apply(env: &HostEnv, containers: &dyn Containers, items: &[Item]) -> Vec<Removed> {
    let running: Vec<Running> = Running::list(env);
    let running_homes: BTreeSet<PathBuf> = Session::all(env)
        .into_iter()
        .filter(|s| running.iter().any(|r| r.id == s.id))
        .map(|s| s.home)
        .chain(running.iter().filter_map(|r| {
            let auth: AuthMode = r.auth.parse().ok()?;
            Some(
                homes_root(env)
                    .join(&r.harness)
                    .join(login_dir(&auth))
                    .join(state::slug(&r.project)),
            )
        }))
        .collect();
    let mut out = Vec::new();
    for i in items.iter().filter(|i| i.selected()) {
        let mut note = None;
        let r: Result<()> = match &i.action {
            Action::Image { cli, tags } => tags
                .iter()
                .try_for_each(|t| containers.run(cli, &["rmi", t]).map(|_| ())),
            // Only the records that paddock found: one id filter each.
            Action::BuildCache { cli, ids } => ids.iter().try_for_each(|id| {
                let f = format!("id={id}");
                containers
                    .run(cli, &["builder", "prune", "--force", "--filter", &f])
                    .map(|_| ())
            }),
            Action::Path { path }
                if !running.is_empty() && path.parent() == Some(shared_cache(env).as_path()) =>
            {
                Err(anyhow::anyhow!("a sandbox runs; skipped"))
            }
            Action::Path { path } if running_homes.contains(path) => {
                Err(anyhow::anyhow!("a sandbox started in it; skipped"))
            }
            Action::Path { path } => remove_path(env, path),
            Action::Kv { sandboxes } => {
                crate::kv::gc_apply(env, sandboxes, state::now()).map(|_| ())
            }
            Action::Worktree { path, .. } if running.iter().any(|r| r.project == *path) => {
                Err(anyhow::anyhow!("a sandbox started in it; skipped"))
            }
            Action::Worktree {
                path,
                repo,
                branch,
                delete_branch,
                force,
            } => (|| {
                check_removable(env, path)?;
                match repo {
                    Some(repo) => {
                        let p = path.display().to_string();
                        let mut a = vec!["worktree", "remove"];
                        if *force {
                            a.push("--force");
                        }
                        a.push(&p);
                        // The worktree itself is sandbox-written: its own
                        // configuration must be harmless too, because the
                        // removal reads that work tree.
                        if let Ok(Some(l)) = crate::hostgit::Layout::discover(path) {
                            l.guard()?;
                        }
                        git_write(repo, &a)?;
                        match branch {
                            Some(b) if *delete_branch => git_write(repo, &["branch", "-D", b])?,
                            Some(b) => note = Some(format!("kept the branch {b}")),
                            None => {}
                        }
                    }
                    None => remove_path(env, path)?,
                }
                // The per-repository folder, when it is empty now.
                if let Some(parent) = path.parent() {
                    let _ = std::fs::remove_dir(parent);
                }
                Ok(())
            })(),
        };
        out.push(Removed {
            what: i.what.clone(),
            bytes: i.bytes,
            error: r.err().map(|e| format!("{e:#}")),
            note,
        });
    }
    out
}

/// The automatic tier, after a new base image was built. `PADDOCK_GC_AFTER_BUILD=0`
/// turns it off.
pub fn after_build(env: &HostEnv, tag: &str) {
    if env.var("PADDOCK_GC_AFTER_BUILD") == Some("0") {
        return;
    }
    let containers = RealContainers(env);
    let mut host = Host::real(env, &containers);
    host.current_tags.insert(tag.to_string());
    let mut inv = scan(env, &host, false);
    let mut items = plan(
        &inv,
        &GcOptions {
            yes: true,
            older_than_days: DEFAULT_OLDER_THAN,
            ..Default::default()
        },
        state::now(),
    );
    items.retain(|i| i.selected());
    if items.is_empty() {
        return;
    }
    measure_plan(&mut inv, &mut items);
    for r in apply(env, &containers, &items) {
        match r.error {
            None => eprintln!("paddock: gc: removed {} ({})", r.what, human(r.bytes)),
            Some(e) => log::warn!("gc: cannot remove {}: {e}", r.what),
        }
    }
}

pub fn human(b: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1000.0 && i + 1 < U.len() {
        v /= 1000.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

pub fn date(unix: u64) -> String {
    let d = crate::build::ymd(unix);
    format!("{}-{}-{}", &d[..4], &d[4..6], &d[6..])
}

fn ago(now: u64, t: u64) -> String {
    let days = now.saturating_sub(t) / DAY;
    match days {
        0 => "today".into(),
        1 => "yesterday".into(),
        n => format!("{n} days ago"),
    }
}

/// `paddock du` as text.
pub fn render_du(env: &HostEnv, inv: &Inventory, now: u64) -> String {
    let home = env.home.display().to_string();
    let tilde = |p: &Path| {
        let s = p.display().to_string();
        match s.strip_prefix(&home) {
            Some(rest) if rest.starts_with('/') => format!("~{rest}"),
            _ => s,
        }
    };
    let mut t = String::new();
    let kind = match inv.size_kind {
        SizeKind::BtrfsExclusive => "exclusive bytes, from btrfs filesystem du",
        SizeKind::Allocated => "allocated bytes, as du counts them",
    };
    let mut homes: Vec<&Home> = inv.homes.iter().collect();
    homes.sort_by_key(|h| std::cmp::Reverse(h.bytes));
    let total_homes: u64 = homes.iter().map(|h| h.bytes).sum();
    t.push_str(&format!(
        "Sandbox homes: {} in {} homes ({kind})\n",
        human(total_homes),
        homes.len()
    ));
    for h in &homes {
        let project = match (&h.project, h.project_exists) {
            (Some(p), Some(false)) => format!("{} (gone)", tilde(p)),
            (Some(p), _) => tilde(p),
            (None, _) => format!(
                "unknown ({}), last modified {}",
                name(&h.path),
                date(h.modified)
            ),
        };
        let used = match h.last_used {
            Some(u) => ago(now, u),
            None => "-".into(),
        };
        let mut flags = Vec::new();
        if h.running {
            flags.push("running");
        }
        if !h.profile {
            flags.push("profile gone");
        }
        if h.origin == OriginCheck::Mismatch {
            flags.push("origin file ignored");
        }
        let flags = if flags.is_empty() {
            String::new()
        } else {
            format!(" [{}]", flags.join(", "))
        };
        t.push_str(&format!(
            "  {:>9}  {:<12}  {} {}  {project}{flags}\n            deleting loses: {}\n",
            human(h.bytes),
            used,
            h.harness,
            h.auth,
            h.loses
        ));
    }
    if !inv.scratch.is_empty() {
        t.push_str(&format!(
            "\nScratch projects (agents that started in the home folder): {}\n",
            human(inv.scratch.iter().map(|c| c.bytes).sum())
        ));
        for c in &inv.scratch {
            t.push_str(&format!(
                "  {:>9}  {:<12}  {}  {}{}\n",
                human(c.bytes),
                ago(now, c.last_active),
                c.harness,
                tilde(&c.path),
                if c.running { " [running]" } else { "" }
            ));
        }
    }
    if !inv.snapshots.is_empty() {
        t.push_str(&format!(
            "\nProject snapshots and undo trash (`paddock undo`): {}\n",
            human(inv.snapshots.iter().map(|c| c.bytes).sum())
        ));
        for c in &inv.snapshots {
            let what = match &c.project {
                Some(p) if !c.trash => format!(
                    "{} {}{}",
                    c.harness,
                    tilde(p),
                    if c.project_exists {
                        ""
                    } else {
                        " [project gone]"
                    }
                ),
                _ => format!("trash {}", tilde(&c.path)),
            };
            t.push_str(&format!(
                "  {:>9}  {:<12}  {what}\n",
                human(c.bytes),
                ago(now, c.started),
            ));
        }
    }
    let total_images: u64 = inv.images.iter().map(|i| i.unique.unwrap_or(i.bytes)).sum();
    t.push_str(&format!(
        "\nImages: {} in {} (the first number is each image's own part, which removing it frees)\n",
        human(total_images),
        inv.images.len()
    ));
    for i in &inv.images {
        let state = match (i.current, i.in_use) {
            (true, true) => "current, in use",
            (true, false) => "current",
            (false, true) => "old, in use",
            (false, false) => "old, unused",
        };
        t.push_str(&format!(
            "  {:>9}  {:<12}  {} {}  ({state}, {} in all)\n",
            human(i.unique.unwrap_or(i.bytes)),
            i.created,
            i.cli,
            i.tags.join(", "),
            human(i.bytes)
        ));
    }
    let cache: u64 = inv.build_cache.iter().map(|r| r.bytes).sum();
    if !inv.build_cache.is_empty() {
        t.push_str(&format!(
            "\nBuild cache: {} in {} records of paddock base image builds\n",
            human(cache),
            inv.build_cache.len()
        ));
    }
    let total_wt: u64 = inv.worktrees.iter().map(|w| w.bytes).sum();
    t.push_str(&format!(
        "\nWorktrees: {} in {}\n",
        human(total_wt),
        inv.worktrees.len()
    ));
    for w in &inv.worktrees {
        let commits = match (&w.repo, w.unmerged) {
            (None, _) => "repository unknown".to_string(),
            (_, Some(n)) => format!("{n} unmerged commits"),
            (_, None) => "unmerged commits unknown".into(),
        };
        let dirty = match w.dirty {
            Some(true) => ", uncommitted changes",
            _ => "",
        };
        let running = if w.running { " [running]" } else { "" };
        t.push_str(&format!(
            "  {:>9}  {}  {}{commits}{dirty}{running}\n",
            human(w.bytes),
            tilde(&w.path),
            w.branch
                .as_ref()
                .map(|b| format!("{b}, "))
                .unwrap_or_default(),
        ));
    }
    let sb: u64 = inv.sessions.iter().map(|s| s.bytes).sum();
    let old = inv
        .sessions
        .iter()
        .filter(|s| {
            !s.running && s.ended.unwrap_or(s.started) < now.saturating_sub(SESSION_DAYS * DAY)
        })
        .count();
    t.push_str(&format!(
        "\nSession records: {} in {} ({old} ended more than {SESSION_DAYS} days ago)\n",
        human(sb),
        inv.sessions.len()
    ));
    for c in &inv.old_caches {
        t.push_str(&format!(
            "Old cache: {} in {} (paddock uses {} now)\n",
            human(c.bytes),
            tilde(&c.path),
            c.replaced_by
        ));
    }
    let orphan = inv.keyrings.iter().filter(|k| !k.home_exists).count();
    t.push_str(&format!(
        "Keyring passwords: {} ({orphan} without a home)\n",
        inv.keyrings.len()
    ));
    if let Some(l) = crate::borrow::du_line(env) {
        t.push_str(&l);
        t.push('\n');
    }
    for n in &inv.notes {
        t.push_str(&format!("note: {n}\n"));
    }
    t.push_str("\nTo see what paddock can remove: paddock gc\n");
    t
}

/// `paddock gc` as text: the plan, or what `--yes` removed.
pub fn render_plan(items: &[Item], yes: bool) -> String {
    let mut t = String::new();
    if items.is_empty() {
        return "paddock gc: nothing to remove.\n".into();
    }
    if !yes {
        t.push_str("paddock gc: dry run. Nothing was removed.\n\n");
    }
    let mut selected = 0;
    let mut more = 0;
    for i in items {
        let head = if i.selected() {
            selected += i.bytes;
            if yes {
                "remove".to_string()
            } else {
                "--yes removes".to_string()
            }
        } else {
            more += i.bytes;
            format!("needs {}", i.needs.join(" "))
        };
        t.push_str(&format!(
            "  {:>9}  {}  [{head}]\n            {}",
            human(i.bytes),
            i.what,
            i.reason
        ));
        if let Some(w) = &i.warning {
            t.push_str(&format!(". Warning: {w}"));
        }
        t.push('\n');
    }
    if !yes {
        t.push_str(&format!(
            "\n`paddock gc --yes` frees at least {}.",
            human(selected)
        ));
        if items
            .iter()
            .any(|i| matches!(i.action, Action::BuildCache { .. }))
        {
            t.push_str(" Build cache records that removed images still share free more space after the images are gone.");
        }
        if more > 0 {
            t.push_str(&format!(
                " The items that need more flags hold {} more.",
                human(more)
            ));
        }
        t.push('\n');
    }
    t
}

#[cfg(test)]
mod tests;
