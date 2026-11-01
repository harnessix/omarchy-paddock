//! Per-sandbox state on the host: sandbox homes, credential sync, the list of
//! running sandboxes, and the watch list of auto-run files.

use crate::env::HostEnv;
use crate::manifest::hex;
use crate::profile::Profile;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn slug(project: &Path) -> String {
    let base: String = project
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "root".into())
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(32)
        .collect();
    let h = hex(&Sha256::digest(project.to_string_lossy().as_bytes()));
    format!("{base}-{}", &h[..10])
}

pub fn new_id(harness: &str, project: &Path) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seed = format!("{nanos}-{}-{}", std::process::id(), project.display());
    let h = hex(&Sha256::digest(seed.as_bytes()));
    let base: String = slug(project)
        .split('-')
        .next()
        .unwrap_or("p")
        .chars()
        .take(16)
        .collect();
    format!("{harness}-{base}-{}", &h[..6]).to_lowercase()
}

/// The sandbox `$HOME` for one harness in one project. Session history stays
/// here, so one project's transcripts are not visible from another.
pub fn sandbox_home(
    env: &HostEnv,
    harness: &str,
    auth: &AuthMode,
    project: &Path,
) -> Result<PathBuf> {
    // One home per login, so the account state of one login never shows in another.
    let dir = env
        .paddock_data()
        .join("homes")
        .join(harness)
        .join(crate::storage::login_dir(auth))
        .join(slug(project));
    crate::privfs::dir(&dir)?;
    Ok(dir)
}

/// Which login a sandbox uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthMode {
    /// The host's own login. Credential files in the host home are the store.
    Shared,
    /// A separate login kept by paddock, with a name for several accounts.
    Isolated(String),
    /// Another account folder of the host, as Omarchy's account switch
    /// selects it (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `GROK_HOME`). The
    /// folder is the store, like the home in the shared mode. `key` names
    /// the sandbox home; `dir` is empty when the mode was read back from a
    /// record.
    Account { key: String, dir: PathBuf },
}

impl AuthMode {
    /// The account mode for a host folder.
    pub fn account(dir: &Path) -> Self {
        Self::Account {
            key: slug(dir),
            dir: dir.to_path_buf(),
        }
    }

    /// The host's own logins: the home or an account folder of the host.
    pub fn is_host_login(&self) -> bool {
        matches!(self, Self::Shared | Self::Account { .. })
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

impl std::str::FromStr for AuthMode {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        if let Some(key) = s.strip_prefix("account:") {
            if !valid_name(key) {
                anyhow::bail!("account key '{key}' must use letters, digits, '-' or '_'");
            }
            return Ok(Self::Account {
                key: key.to_string(),
                dir: PathBuf::new(),
            });
        }
        let name = match s {
            "shared" => return Ok(Self::Shared),
            "isolated" => "default",
            other => other.strip_prefix("isolated:").ok_or_else(|| {
                anyhow::anyhow!("unknown auth mode '{s}': use shared, isolated or isolated:NAME")
            })?,
        };
        if !valid_name(name) {
            anyhow::bail!("auth name '{name}' must use letters, digits, '-' or '_'");
        }
        Ok(Self::Isolated(name.to_string()))
    }
}

impl std::fmt::Display for AuthMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Shared => write!(f, "shared"),
            Self::Isolated(n) => write!(f, "isolated:{n}"),
            Self::Account { key, .. } => write!(f, "account:{key}"),
        }
    }
}

/// The directory that holds the credential files for a mode.
pub fn auth_root(env: &HostEnv, harness: &str, mode: &AuthMode) -> PathBuf {
    match mode {
        AuthMode::Shared | AuthMode::Account { .. } => env.home.clone(),
        AuthMode::Isolated(name) => env.paddock_data().join("auth").join(harness).join(name),
    }
}

/// The isolated logins in paddock's own store, as `harness` and `name`.
pub fn isolated_logins(env: &HostEnv) -> Vec<(String, String)> {
    let names = |dir: &Path| -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();
        v.sort();
        v
    };
    let root = env.paddock_data().join("auth");
    names(&root)
        .into_iter()
        .flat_map(|h| {
            names(&root.join(&h))
                .into_iter()
                .map(move |n| (h.clone(), n))
        })
        .collect()
}

/// The account folder for `rel`, a path relative to HOME in the profile, in
/// the account mode (`[state.account]`). None in the other modes, and for
/// paths that do not move with the account.
fn account_file(profile: &Profile, mode: &AuthMode, rel: &str) -> Option<PathBuf> {
    match (mode, &profile.state.account) {
        (AuthMode::Account { dir, .. }, Some(a)) if !dir.as_os_str().is_empty() => a.map(dir, rel),
        _ => None,
    }
}

/// The host file that a settings entry (`import_ro`, `seed`, `json_import`)
/// reads. In the account mode, the files of the account folder come from the
/// selected folder.
pub fn host_file(env: &HostEnv, profile: &Profile, mode: &AuthMode, rel: &str) -> PathBuf {
    account_file(profile, mode, rel).unwrap_or_else(|| env.home.join(rel))
}

/// The stored copy of a credential file (`auth`) for a mode.
pub fn auth_file(env: &HostEnv, profile: &Profile, mode: &AuthMode, rel: &str) -> PathBuf {
    account_file(profile, mode, rel)
        .unwrap_or_else(|| auth_root(env, &profile.name, mode).join(rel))
}

/// The hash of a regular file, streamed, of at most 1 GiB. A FIFO or a device
/// is never opened, so this cannot hang the cleanup after a run.
fn file_hash(p: &Path) -> Option<String> {
    hash_open(p, true)
}

fn hash_open(p: &Path, follow: bool) -> Option<String> {
    let f = crate::safefs::open_file(p, follow).ok()??;
    let mut h = Sha256::new();
    let mut r = std::io::Read::take(f, 1 << 30);
    std::io::copy(&mut r, &mut h).ok()?;
    Some(hex(&h.finalize()))
}

/// The state of a watched file in the sandbox-writable project. A symlink is
/// not followed: its target text is the state, so a planted link shows as a
/// change. A FIFO or a device shows as a change too and is never opened.
fn watch_hash(p: &Path) -> Option<String> {
    let m = std::fs::symlink_metadata(p).ok()?;
    if m.file_type().is_symlink() {
        let t = std::fs::read_link(p).ok()?;
        return Some(format!("link:{}", t.display()));
    }
    if !m.is_file() {
        use std::os::unix::fs::MetadataExt;
        return Some(format!("special:{:o}", m.mode() & 0o170000));
    }
    hash_open(p, false).or_else(|| Some("unreadable".into()))
}

fn copy_private(from: &Path, to: &Path) -> Result<()> {
    // Write next to the target, then rename: the same atomic pattern the
    // harnesses use, so a crash never leaves half a token. The copy is 0600
    // from its first byte.
    crate::privfs::copy_atomic(from, to)
        .with_context(|| format!("copy {} -> {}", from.display(), to.display()))
}

/// Write bytes to a host path that only the host can write, atomically.
fn write_private(bytes: &[u8], to: &Path) -> Result<()> {
    crate::privfs::write_atomic(to, bytes).with_context(|| format!("write {}", to.display()))
}

/// Where a credential is stored on the host.
enum Store {
    /// A path that only the host can write: the home or paddock's store.
    Host,
    /// A file in an account folder of the host (the account mode). Every
    /// access goes through `safefs` from that folder, so no symlink in it is
    /// followed.
    Account { root: PathBuf, rel: PathBuf },
}

impl Store {
    fn for_file(mode: &AuthMode, stored: &Path) -> Self {
        match mode {
            AuthMode::Account { dir, .. } if !dir.as_os_str().is_empty() => {
                match stored.strip_prefix(dir) {
                    Ok(rel) if !rel.as_os_str().is_empty() => Store::Account {
                        root: dir.clone(),
                        rel: rel.to_path_buf(),
                    },
                    _ => Store::Host,
                }
            }
            _ => Store::Host,
        }
    }

    fn read(&self, stored: &Path) -> Result<Option<Vec<u8>>> {
        match self {
            Store::Host => {
                if !stored.is_file() {
                    return Ok(None);
                }
                std::fs::read(stored)
                    .map(Some)
                    .with_context(|| format!("cannot read {}", stored.display()))
            }
            Store::Account { root, rel } => {
                if !root.is_dir() {
                    return Ok(None);
                }
                crate::safefs::Root::open(root)?.read(rel, crate::safefs::MAX_READ)
            }
        }
    }

    fn hash(&self, stored: &Path) -> Option<String> {
        match self {
            Store::Host => file_hash(stored),
            Store::Account { .. } => self
                .read(stored)
                .ok()
                .flatten()
                .map(|b| hex(&Sha256::digest(&b))),
        }
    }

    fn write(&self, stored: &Path, bytes: &[u8]) -> Result<()> {
        match self {
            Store::Host => write_private(bytes, stored),
            Store::Account { root, rel } => crate::safefs::Root::open(root)?
                .write(rel, bytes, 0o600)
                .with_context(|| format!("write {}", stored.display())),
        }
    }
}

struct AuthItem {
    store: Store,
    stored: PathBuf,
    /// Relative to the sandbox home, which the sandbox can write.
    rel: PathBuf,
    inside_at_start: Option<String>,
    stored_at_start: Option<String>,
    /// A login that a keeper keeps (keeper.rs): the sandbox got it without
    /// its refresh token, and it is never copied back.
    kept: bool,
}

/// Credentials are copied, not bind-mounted: harnesses replace token files
/// with a rename, which a single-file bind mount does not survive.
///
/// The sandbox home is sandbox-controlled: every access to it goes through
/// `safefs`, so a symlink planted there is never followed by the host.
pub struct AuthSync {
    pub mode: AuthMode,
    home: PathBuf,
    items: Vec<AuthItem>,
    /// Folder entries (`auth = ["dir/"]`): the folder relative to the
    /// sandbox home, and its store on the host. A login that the sandbox
    /// adds there is copied out too.
    folders: Vec<(PathBuf, PathBuf)>,
}

/// The names of the regular files in a host folder. Symlinks are left out.
fn host_file_names(dir: &Path, store: &Store) -> Vec<String> {
    let names: Vec<std::ffi::OsString> = match store {
        Store::Account { root, rel } => crate::safefs::Root::open(root)
            .and_then(|r| r.dir(rel, false))
            .and_then(|d| d.file_names())
            .unwrap_or_default(),
        Store::Host => std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
            .map(|e| e.file_name())
            .collect(),
    };
    names
        .into_iter()
        .filter_map(|n| n.into_string().ok())
        .collect()
}

/// The names of the regular files in a folder of the sandbox home.
fn inside_file_names(home: &crate::safefs::Root, rel: &Path) -> Vec<String> {
    home.dir(rel, false)
        .and_then(|d| d.file_names())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|n| n.into_string().ok())
        .collect()
}

/// The hash of a credential file inside the sandbox home. A symlink or a
/// special file in its place is an error, never followed.
fn inside_hash(root: &crate::safefs::Root, rel: &Path) -> Result<Option<String>> {
    Ok(root
        .read(rel, crate::safefs::MAX_READ)?
        .map(|b| hex(&Sha256::digest(&b))))
}

pub struct SyncResult {
    pub saved: Vec<PathBuf>,
    /// Files that changed in the store while the sandbox ran. The store wins.
    pub conflicts: Vec<PathBuf>,
}

impl AuthSync {
    pub fn copy_in(
        env: &HostEnv,
        profile: &Profile,
        mode: &AuthMode,
        home_src: &Path,
    ) -> Result<Self> {
        let home = crate::safefs::Root::open(home_src)?;
        let mut items = Vec::new();
        let mut folders = Vec::new();
        let mut rels = Vec::new();
        for rel in &profile.state.auth {
            // A folder of credential files whose names the profile cannot
            // know, such as one file per provider.
            if let Some(dir) = rel.strip_suffix('/') {
                let stored = auth_file(env, profile, mode, dir);
                let store = Store::for_file(mode, &stored);
                let mut names = host_file_names(&stored, &store);
                for n in inside_file_names(&home, Path::new(dir)) {
                    if !names.contains(&n) {
                        names.push(n);
                    }
                }
                names.sort();
                rels.extend(names.into_iter().map(|n| format!("{dir}/{n}")));
                folders.push((PathBuf::from(dir), stored));
            } else {
                rels.push(rel.clone());
            }
        }
        for rel in &rels {
            let stored = auth_file(env, profile, mode, rel);
            let store = Store::for_file(mode, &stored);
            let rel = PathBuf::from(rel);
            let mut kept = false;
            if let Some(bytes) = store.read(&stored)? {
                // A login that the keeper refreshes: the sandbox gets it
                // without the refresh token.
                let keep = crate::keeper::kind_for(&profile.name)
                    .filter(|k| Path::new(k.file) == rel)
                    .filter(|_| crate::keeper::Cfg::load(env).is_ok_and(|c| c.enabled))
                    .and_then(|_| crate::keeper::strip(&bytes));
                kept = keep.is_some();
                home.write(&rel, keep.as_deref().unwrap_or(&bytes), 0o600)
                    .with_context(|| format!("credential {}", rel.display()))?;
            } else if mode.is_host_login() {
                // The host is logged out: do not keep an older login inside.
                home.remove(&rel)?;
            }
            items.push(AuthItem {
                inside_at_start: inside_hash(&home, &rel)?,
                stored_at_start: store.hash(&stored),
                store,
                stored,
                rel,
                kept,
            });
        }
        Ok(Self {
            mode: mode.clone(),
            home: home_src.to_path_buf(),
            items,
            folders,
        })
    }

    /// The logins that a keeper keeps for this sandbox.
    pub fn kept_logins(&self) -> Vec<HeldLogin> {
        self.items
            .iter()
            .filter(|i| i.kept)
            .map(|i| HeldLogin {
                home: self.home.clone(),
                rel: i.rel.clone(),
                store: i.stored.clone(),
            })
            .collect()
    }

    /// The host logins that the sandbox got a copy of. None for an isolated
    /// login, which the host never reads.
    pub fn host_logins(&self) -> Vec<HeldLogin> {
        if !self.mode.is_host_login() {
            return Vec::new();
        }
        self.items
            .iter()
            .map(|i| HeldLogin {
                home: self.home.clone(),
                rel: i.rel.clone(),
                store: i.stored.clone(),
            })
            .collect()
    }

    /// Copy back every credential that changed inside the sandbox, for example
    /// after a login or a token refresh. If the store changed too (the harness
    /// also runs on the host), the store keeps its version.
    pub fn copy_out(&self) -> Result<SyncResult> {
        let mut r = SyncResult {
            saved: vec![],
            conflicts: vec![],
        };
        let home = crate::safefs::Root::open(&self.home)?;
        // Logins that the sandbox added to a folder entry.
        let mut added = Vec::new();
        for (dir, stored_dir) in &self.folders {
            for n in inside_file_names(&home, dir) {
                let rel = dir.join(&n);
                if self.items.iter().any(|i| i.rel == rel) {
                    continue;
                }
                let stored = stored_dir.join(&n);
                let store = Store::for_file(&self.mode, &stored);
                added.push(AuthItem {
                    stored_at_start: store.hash(&stored),
                    inside_at_start: None,
                    store,
                    stored,
                    rel,
                    kept: false,
                });
            }
        }
        for it in self.items.iter().chain(added.iter()) {
            // The keeper wrote the store; the sandbox's copy has no refresh
            // token and must never replace it.
            if it.kept {
                continue;
            }
            // A symlink or special file in place of a credential is refused:
            // reading through it would copy a host file into the store.
            let bytes = match home.read(&it.rel, crate::safefs::MAX_READ) {
                Ok(Some(b)) => b,
                Ok(None) => continue,
                Err(e) => {
                    log::warn!("not saving the credential {}: {e:#}", it.rel.display());
                    continue;
                }
            };
            let now = Some(hex(&Sha256::digest(&bytes)));
            if now == it.inside_at_start {
                continue;
            }
            if it.store.hash(&it.stored) != it.stored_at_start {
                r.conflicts.push(it.stored.clone());
                continue;
            }
            it.store.write(&it.stored, &bytes)?;
            r.saved.push(it.stored.clone());
        }
        Ok(r)
    }
}

/// Copy the host's credential files for a harness into an isolated store.
pub fn auth_import(env: &HostEnv, profile: &Profile, mode: &AuthMode) -> Result<Vec<PathBuf>> {
    if mode.is_host_login() {
        anyhow::bail!("the shared mode already uses the host login; import into an isolated store, for example --auth isolated:work");
    }
    let root = auth_root(env, &profile.name, mode);
    let mut done = Vec::new();
    for rel in &profile.state.auth {
        let host = env.home.join(rel);
        if host.is_file() {
            copy_private(&host, &root.join(rel))?;
            done.push(host);
        }
    }
    if root.is_dir() {
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(done)
}

/// Copy `seed` paths from the host home into the sandbox home.
pub fn seed(env: &HostEnv, profile: &Profile, mode: &AuthMode, home_src: &Path) -> Result<()> {
    let home = crate::safefs::Root::open(home_src)?;
    for rel in &profile.state.seed {
        let host = host_file(env, profile, mode, rel);
        if host.exists() {
            home.copy_in(&host, Path::new(rel))
                .with_context(|| format!("seed {rel}"))?;
        }
    }
    Ok(())
}

pub fn json_get<'a>(v: &'a serde_json::Value, path: &[String]) -> Option<&'a serde_json::Value> {
    path.iter().try_fold(v, |cur, k| cur.get(k))
}

pub(crate) fn json_set(v: &mut serde_json::Value, path: &[String], val: serde_json::Value) {
    let mut cur = v;
    for (i, k) in path.iter().enumerate() {
        if !cur.is_object() {
            *cur = serde_json::json!({});
        }
        let obj = cur.as_object_mut().unwrap();
        if i + 1 == path.len() {
            obj.insert(k.clone(), val);
            return;
        }
        cur = obj
            .entry(k.clone())
            .or_insert_with(|| serde_json::json!({}));
    }
}

pub fn expand_path(path: &[String], project: &Path) -> Vec<String> {
    path.iter()
        .map(|k| k.replace("{project}", &project.display().to_string()))
        .collect()
}

/// Copy selected keys from host JSON files into the sandbox copies.
pub fn json_import(
    env: &HostEnv,
    profile: &Profile,
    mode: &AuthMode,
    home_src: &Path,
    project: &Path,
) -> Result<()> {
    for ji in &profile.state.json_import {
        let host_file = host_file(env, profile, mode, &ji.file);
        let Ok(text) = std::fs::read_to_string(&host_file) else {
            continue;
        };
        let Ok(host): std::result::Result<serde_json::Value, _> = serde_json::from_str(&text)
        else {
            continue;
        };
        let home = crate::safefs::Root::open(home_src)?;
        let rel = Path::new(&ji.file);
        let mut inside: serde_json::Value = home
            .read(rel, crate::safefs::MAX_READ)
            .ok()
            .flatten()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_else(|| serde_json::json!({}));
        let mut keys: Vec<&Vec<String>> = ji.keys.iter().collect();
        if mode.is_host_login() {
            keys.extend(ji.shared_keys.iter());
        }
        for k in keys {
            let k = expand_path(k, project);
            if let Some(v) = json_get(&host, &k) {
                json_set(&mut inside, &k, v.clone());
            }
        }
        home.write(
            rel,
            serde_json::to_string_pretty(&inside)?.as_bytes(),
            0o600,
        )?;
    }
    Ok(())
}

/// A minimal git configuration with only the identity, so commits work but
/// host credential helpers, aliases and includes stay outside. Pushes go to
/// the host through the `paddock::` remote helper (see broker/git.rs).
pub fn write_gitconfig(
    env: &HostEnv,
    home_src: &Path,
    repo: Option<&crate::broker::git::Repo>,
) -> Result<()> {
    let (name, email) = env.git_identity();
    let mut text = String::from("# Written by paddock on each start. Edits are lost.\n[user]\n");
    if let Some(n) = name {
        text.push_str(&format!("\tname = {n}\n"));
    }
    if let Some(e) = email {
        text.push_str(&format!("\temail = {e}\n"));
    }
    text.push_str("[init]\n\tdefaultBranch = main\n");
    text.push_str(&crate::broker::git::sandbox_gitconfig(repo));
    crate::safefs::Root::open(home_src)?.write(Path::new(".gitconfig"), text.as_bytes(), 0o644)?;
    Ok(())
}

pub fn snapshot(paths: &[PathBuf]) -> BTreeMap<PathBuf, Option<String>> {
    paths.iter().map(|p| (p.clone(), watch_hash(p))).collect()
}

pub fn changed(before: &BTreeMap<PathBuf, Option<String>>) -> Vec<PathBuf> {
    before
        .iter()
        .filter(|(p, h)| watch_hash(p) != **h)
        .map(|(p, _)| p.clone())
        .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Running {
    pub id: String,
    pub harness: String,
    pub project: PathBuf,
    pub backend: String,
    pub network: String,
    pub gui: String,
    pub pid: u32,
    pub started: u64,
    #[serde(default)]
    pub auth: String,
    /// The nested compositor of `window` mode, so the panel can focus it.
    #[serde(default)]
    pub window_pid: Option<u32>,
    #[serde(default)]
    pub warnings: Vec<String>,
    /// The paddock process. Its ancestors include the terminal that runs it.
    #[serde(default)]
    pub host_pid: u32,
    /// For a sandbox that another sandbox started (`paddock-host spawn`):
    /// the id of that sandbox.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// The id that the parent uses for it with `paddock-host status`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawn_id: Option<String>,
    /// The host logins that this sandbox holds a copy of (the main login or
    /// an Omarchy account). While it runs, paddock checks their limits for
    /// the Omarchy agents panel from its side (omarchy_usage.rs).
    #[serde(default)]
    pub logins: Vec<HeldLogin>,
    /// The systemd scope that holds the sandbox (`paddock-…`), which
    /// `paddock freeze` freezes. None for container backends and without a
    /// systemd user instance.
    #[serde(default)]
    pub unit: Option<String>,
    /// The project of paddock kv for this sandbox (the repository of a
    /// worktree), where paddock writes the sandbox's state.
    #[serde(default)]
    pub kv_project: Option<PathBuf>,
    /// The pasture the sandbox joined (`--pasture NAME`). None: a pasture
    /// of its own.
    #[serde(default)]
    pub pasture: Option<String>,
    /// The credentials whose values the sandbox holds (`deliver`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub credentials: Vec<String>,
    /// The credentials that the sandbox uses through the gateway: it holds
    /// only stand-ins.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gateway: Vec<String>,
    /// Logins that a keeper keeps for this sandbox (keeper.rs): the sandbox
    /// holds them without their refresh token, and the keeper writes the
    /// new access token here.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kept_logins: Vec<HeldLogin>,
}

/// A host login file and the sandbox's copy of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeldLogin {
    /// The sandbox home. The sandbox can write it, so it is read through
    /// `safefs` only.
    pub home: PathBuf,
    /// The copy, relative to `home`.
    pub rel: PathBuf,
    /// The login file on the host.
    pub store: PathBuf,
}

/// A project where a sandbox started, for the panel's "start again" list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Recent {
    pub project: PathBuf,
    pub harness: String,
    pub last: u64,
}

fn recent_file(env: &HostEnv) -> PathBuf {
    env.paddock_state().join("recent.json")
}

pub fn recent(env: &HostEnv) -> Vec<Recent> {
    let mut v: Vec<Recent> = std::fs::read_to_string(recent_file(env))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    v.retain(|r| r.project.is_dir());
    v
}

/// Remember the project and harness of a start. Keeps the 20 most recent projects.
pub fn record_recent(env: &HostEnv, project: &Path, harness: &str) -> Result<()> {
    let mut v = recent(env);
    v.retain(|r| r.project != project);
    v.insert(
        0,
        Recent {
            project: project.to_path_buf(),
            harness: harness.to_string(),
            last: now(),
        },
    );
    v.truncate(20);
    let f = recent_file(env);
    crate::privfs::write(&f, serde_json::to_string_pretty(&v)?)?;
    Ok(())
}

/// The parent of a process, from /proc.
pub fn parent_pid(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name can contain spaces and parentheses; fields follow the last ')'.
    let rest = stat.rsplit_once(')')?.1;
    rest.split_whitespace().nth(1)?.parse().ok()
}

impl Running {
    pub fn file(env: &HostEnv, id: &str) -> PathBuf {
        env.paddock_run().join(id).join("info.json")
    }

    pub fn register(&self, env: &HostEnv) -> Result<()> {
        let f = Self::file(env, &self.id);
        crate::privfs::write(&f, serde_json::to_string(self)?)?;
        Ok(())
    }

    pub fn list(env: &HostEnv) -> Vec<Running> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(env.paddock_run()) else {
            return out;
        };
        for e in entries.flatten() {
            let f = e.path().join("info.json");
            let Ok(text) = std::fs::read_to_string(&f) else {
                continue;
            };
            let Ok(r) = serde_json::from_str::<Running>(&text) else {
                continue;
            };
            if alive(r.pid) {
                out.push(r);
            } else {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
        out.sort_by_key(|r| r.started);
        out
    }
}

pub fn alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

/// Running sandboxes with each child right after its parent, in start
/// order. A child whose parent is gone is listed at the top level.
pub fn tree_order(list: Vec<Running>) -> Vec<Running> {
    fn add(r: &Running, all: &[Running], out: &mut Vec<Running>, depth: usize) {
        if depth > 8 || out.iter().any(|o| o.id == r.id) {
            return;
        }
        out.push(r.clone());
        for c in all.iter().filter(|c| c.parent.as_deref() == Some(&r.id)) {
            add(c, all, out, depth + 1);
        }
    }
    let roots: Vec<&Running> = list
        .iter()
        .filter(|r| {
            !r.parent
                .as_deref()
                .is_some_and(|p| list.iter().any(|o| o.id == p))
        })
        .collect();
    // The members of one pasture follow each other, where the first of
    // them is.
    let key = |r: &Running| r.pasture.clone().map(|n| (r.kv_project.clone(), n));
    let mut out = Vec::new();
    for (i, r) in roots.iter().enumerate() {
        if out.iter().any(|o: &Running| o.id == r.id) {
            continue;
        }
        add(r, &list, &mut out, 0);
        if let Some(k) = key(r) {
            for other in roots[i + 1..]
                .iter()
                .filter(|o| key(o).as_ref() == Some(&k))
            {
                add(other, &list, &mut out, 0);
            }
        }
    }
    out
}

/// How many running ancestors a sandbox has in `list`.
pub fn depth_in(list: &[Running], r: &Running) -> usize {
    let mut d = 0;
    let mut cur = r.parent.clone();
    while let Some(p) = cur {
        match list.iter().find(|o| o.id == p) {
            Some(o) if d < 8 => {
                d += 1;
                cur = o.parent.clone();
            }
            _ => break,
        }
    }
    d
}

/// The running members of the pasture `name` of `project`.
pub fn pasture_members<'a>(list: &'a [Running], project: &Path, name: &str) -> Vec<&'a Running> {
    list.iter()
        .filter(|r| r.pasture.as_deref() == Some(name) && r.kv_project.as_deref() == Some(project))
        .collect()
}

/// Every running sandbox that `id` started, and the ones they started.
pub fn descendants(list: &[Running], id: &str) -> Vec<Running> {
    let mut out: Vec<Running> = Vec::new();
    let mut queue = vec![id.to_string()];
    while let Some(p) = queue.pop() {
        for c in list.iter().filter(|c| c.parent.as_deref() == Some(&p)) {
            if !out.iter().any(|o| o.id == c.id) && out.len() < 64 {
                out.push(c.clone());
                queue.push(c.id.clone());
            }
        }
    }
    out
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// True when the secret-looking variable names for a project differ from the
/// last notice. Records the new list.
pub fn env_notice_changed(env: &HostEnv, project: &Path, names: &[String]) -> bool {
    let mut sorted = names.to_vec();
    sorted.sort();
    let hash = hex(&Sha256::digest(sorted.join("\n").as_bytes()));
    let file = env.paddock_state().join("env-notice").join(slug(project));
    if std::fs::read_to_string(&file)
        .map(|s| s.trim() == hash)
        .unwrap_or(false)
    {
        return false;
    }
    let _ = crate::privfs::write(&file, &hash);
    true
}

/// Add or remove names in `[env] hide` of the user configuration file,
/// keeping the rest of the file as it is.
pub fn edit_env_hide(env: &HostEnv, add: &[String], remove: &[String]) -> Result<Vec<String>> {
    let file = env.paddock_config().join("config.toml");
    let text = std::fs::read_to_string(&file).unwrap_or_default();
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .with_context(|| format!("{} is invalid", file.display()))?;
    let envt = doc.entry("env").or_insert(toml_edit::table());
    let envt = envt
        .as_table_mut()
        .context("[env] in the configuration is not a table")?;
    let arr = envt
        .entry("hide")
        .or_insert(toml_edit::value(toml_edit::Array::new()));
    let arr = arr.as_array_mut().context("[env] hide is not an array")?;
    let mut names: Vec<String> = arr
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    for a in add {
        if !names.contains(a) {
            names.push(a.clone());
        }
    }
    names.retain(|n| !remove.contains(n));
    names.sort();
    arr.clear();
    for n in &names {
        arr.push(n.as_str());
    }
    std::fs::create_dir_all(file.parent().unwrap())?;
    std::fs::write(&file, doc.to_string())?;
    Ok(names)
}

/// Set `[apps.NAME] roots` in the user configuration, keeping the rest.
pub fn set_app_roots(env: &HostEnv, name: &str, roots: &[PathBuf]) -> Result<()> {
    let file = env.paddock_config().join("config.toml");
    let text = std::fs::read_to_string(&file).unwrap_or_default();
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .with_context(|| format!("{} is invalid", file.display()))?;
    let apps = doc.entry("apps").or_insert(toml_edit::table());
    let apps = apps.as_table_mut().context("[apps] is not a table")?;
    apps.set_implicit(true);
    let app = apps.entry(name).or_insert(toml_edit::table());
    let app = app.as_table_mut().context("[apps.NAME] is not a table")?;
    let mut arr = toml_edit::Array::new();
    for r in roots {
        arr.push(r.display().to_string());
    }
    app.insert("roots", toml_edit::value(arr));
    std::fs::create_dir_all(file.parent().unwrap())?;
    std::fs::write(&file, doc.to_string())?;
    Ok(())
}

/// Apply `toml_set` entries to files in the sandbox home, keeping the rest of
/// each file as it is.
///
/// The sandbox home is sandbox-controlled: the file is read and replaced
/// through `safefs`, so a symlink left there is never followed.
pub fn toml_set(profile: &Profile, home_src: &Path, project: &Path) -> Result<()> {
    let home = crate::safefs::Root::open(home_src)?;
    for t in &profile.state.toml_set {
        let file = home_src.join(&t.file);
        let rel = Path::new(&t.file);
        let text = match home.read(rel, crate::safefs::MAX_READ)? {
            Some(b) => {
                String::from_utf8(b).with_context(|| format!("{} is not UTF-8", file.display()))?
            }
            None => String::new(),
        };
        let mut doc: toml_edit::DocumentMut = text
            .parse()
            .with_context(|| format!("{} is invalid", file.display()))?;
        let path = expand_path(&t.path, project);
        let Some((last, parents)) = path.split_last() else {
            continue;
        };
        let mut table = doc.as_table_mut();
        for k in parents {
            let item = table.entry(k).or_insert(toml_edit::table());
            if let Some(t) = item.as_table_mut() {
                t.set_implicit(true);
            }
            table = item
                .as_table_mut()
                .with_context(|| format!("{k} in {} is not a table", file.display()))?;
        }
        table.insert(last, toml_edit::value(t.value.as_str()));
        home.write(rel, doc.to_string().as_bytes(), 0o600)?;
    }
    Ok(())
}

#[cfg(test)]
mod toml_set_tests {
    use super::*;

    #[test]
    fn sets_project_trust_and_keeps_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        std::fs::write(
            home.join(".codex/config.toml"),
            "# mine\nmodel = \"x\"\n\n[mcp_servers.a]\ncommand = \"a\"\n",
        )
        .unwrap();
        let profile: Profile = toml::from_str(
            "name = \"c\"\n[[state.toml_set]]\nfile = \".codex/config.toml\"\npath = [\"projects\", \"{project}\", \"trust_level\"]\nvalue = \"trusted\"\n",
        )
        .unwrap();
        toml_set(&profile, home, Path::new("/w/app")).unwrap();
        let text = std::fs::read_to_string(home.join(".codex/config.toml")).unwrap();
        let v: toml::Value = toml::from_str(&text).unwrap();
        assert_eq!(
            v["projects"]["/w/app"]["trust_level"].as_str(),
            Some("trusted")
        );
        assert_eq!(v["mcp_servers"]["a"]["command"].as_str(), Some("a"));
        assert!(text.starts_with("# mine"));
    }
}

#[cfg(test)]
mod auth_sync_tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::os::unix::fs::symlink;

    fn setup() -> (tempfile::TempDir, HostEnv, Profile, PathBuf, PathBuf) {
        let t = tempfile::tempdir().unwrap();
        let host_home = t.path().join("host");
        let sandbox_home = t.path().join("sandbox");
        std::fs::create_dir_all(host_home.join(".tool")).unwrap();
        std::fs::create_dir_all(&sandbox_home).unwrap();
        let env = HostEnv::from_parts(host_home.clone(), "demo".into(), 1, 1, BTreeMap::new());
        let p: Profile =
            toml::from_str("name = \"t\"\n[state]\nauth = [\".tool/token\"]\n").unwrap();
        (t, env, p, host_home, sandbox_home)
    }

    #[test]
    fn the_account_mode_uses_the_account_folder_as_the_store() {
        let (t, env, _, host_home, sb) = setup();
        let p: Profile = toml::from_str(
            "name = \"t\"\n[state]\nauth = [\".tool/token\", \".tool.json\", \".other\"]\n\
             seed = [\".tool/settings\"]\n\
             [state.account]\nenv = \"TOOL_HOME\"\ndir = \".tool\"\nalso = [\".tool.json\"]\n",
        )
        .unwrap();
        let acct = t.path().join("accounts/work");
        std::fs::create_dir_all(&acct).unwrap();
        std::fs::write(host_home.join(".tool/token"), "main").unwrap();
        std::fs::write(host_home.join(".other"), "other").unwrap();
        std::fs::write(acct.join("token"), "work").unwrap();
        std::fs::write(acct.join(".tool.json"), "{}").unwrap();
        std::fs::write(acct.join("settings"), "work settings").unwrap();
        let mode = AuthMode::account(&acct);
        assert_eq!(
            host_file(&env, &p, &mode, ".tool/settings"),
            acct.join("settings")
        );
        assert_eq!(host_file(&env, &p, &mode, ".x"), host_home.join(".x"));
        let sync = AuthSync::copy_in(&env, &p, &mode, &sb).unwrap();
        seed(&env, &p, &mode, &sb).unwrap();
        // Inside, the files keep their usual places.
        assert_eq!(
            std::fs::read_to_string(sb.join(".tool/token")).unwrap(),
            "work"
        );
        assert_eq!(
            std::fs::read_to_string(sb.join(".tool.json")).unwrap(),
            "{}"
        );
        assert_eq!(std::fs::read_to_string(sb.join(".other")).unwrap(), "other");
        assert_eq!(
            std::fs::read_to_string(sb.join(".tool/settings")).unwrap(),
            "work settings"
        );
        // A refreshed token goes back to the account folder, not to the main login.
        std::fs::write(sb.join(".tool/token"), "work2").unwrap();
        let r = sync.copy_out().unwrap();
        assert_eq!(r.saved, vec![acct.join("token")]);
        assert_eq!(
            std::fs::read_to_string(acct.join("token")).unwrap(),
            "work2"
        );
        assert_eq!(
            std::fs::read_to_string(host_home.join(".tool/token")).unwrap(),
            "main"
        );
        // A symlink in the account folder is not followed.
        let secret = t.path().join("id_key");
        std::fs::write(&secret, "PRIVATE KEY").unwrap();
        std::fs::remove_file(acct.join("token")).unwrap();
        symlink(&secret, acct.join("token")).unwrap();
        assert!(AuthSync::copy_in(&env, &p, &mode, &sb).is_err());
        // The mode names its own sandbox home and reads back from records.
        let back: AuthMode = mode.to_string().parse().unwrap();
        assert!(matches!(back, AuthMode::Account { ref key, .. } if key == &slug(&acct)));
        assert!(auth_import(&env, &p, &mode).is_err());
    }

    #[test]
    fn a_folder_entry_syncs_every_credential_file_in_it() {
        let (t, env, _, host_home, sb) = setup();
        let p: Profile =
            toml::from_str("name = \"t\"\n[state]\nauth = [\".tool/creds/\"]\n").unwrap();
        let creds = host_home.join(".tool/creds");
        std::fs::create_dir_all(&creds).unwrap();
        std::fs::write(creds.join("a.json"), "a1").unwrap();
        let secret = t.path().join("id_key");
        std::fs::write(&secret, "PRIVATE KEY").unwrap();
        symlink(&secret, creds.join("link.json")).unwrap();
        let sync = AuthSync::copy_in(&env, &p, &AuthMode::Shared, &sb).unwrap();
        assert_eq!(
            std::fs::read_to_string(sb.join(".tool/creds/a.json")).unwrap(),
            "a1"
        );
        // A link in the store is not followed into the sandbox.
        assert!(!sb.join(".tool/creds/link.json").exists());
        // A refresh and a new login inside both come back.
        std::fs::write(sb.join(".tool/creds/a.json"), "a2").unwrap();
        std::fs::write(sb.join(".tool/creds/b.json"), "b1").unwrap();
        let r = sync.copy_out().unwrap();
        assert_eq!(r.saved.len(), 2, "{:?}", r.saved);
        assert_eq!(std::fs::read_to_string(creds.join("a.json")).unwrap(), "a2");
        assert_eq!(std::fs::read_to_string(creds.join("b.json")).unwrap(), "b1");
        assert_eq!(std::fs::read_to_string(&secret).unwrap(), "PRIVATE KEY");
    }

    #[test]
    fn copy_out_refuses_a_symlinked_credential() {
        let (t, env, p, host_home, sb) = setup();
        std::fs::write(host_home.join(".tool/token"), "old").unwrap();
        let secret = t.path().join("id_key");
        std::fs::write(&secret, "PRIVATE KEY").unwrap();
        let sync = AuthSync::copy_in(&env, &p, &AuthMode::Shared, &sb).unwrap();
        // The sandbox swaps its token for a link to a host file.
        std::fs::remove_file(sb.join(".tool/token")).unwrap();
        symlink(&secret, sb.join(".tool/token")).unwrap();
        let r = sync.copy_out().unwrap();
        assert!(r.saved.is_empty());
        assert_eq!(
            std::fs::read_to_string(host_home.join(".tool/token")).unwrap(),
            "old"
        );
    }

    #[test]
    fn watched_files_report_planted_links_and_fifos_without_hanging() {
        let t = tempfile::tempdir().unwrap();
        let (a, b, c) = (
            t.path().join(".envrc"),
            t.path().join("hook"),
            t.path().join("x"),
        );
        std::fs::write(&a, "echo hi").unwrap();
        std::fs::write(t.path().join("same"), "echo hi").unwrap();
        let before = snapshot(&[a.clone(), b.clone(), c.clone()]);
        // A link to a file with the same content, a FIFO, and /dev/zero.
        std::fs::remove_file(&a).unwrap();
        std::os::unix::fs::symlink(t.path().join("same"), &a).unwrap();
        let cs = std::ffi::CString::new(b.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(cs.as_ptr(), 0o600) }, 0);
        std::os::unix::fs::symlink("/dev/zero", &c).unwrap();
        let mut got = changed(&before);
        got.sort();
        let mut want = vec![a, b, c];
        want.sort();
        assert_eq!(got, want);
        assert!(file_hash(Path::new("/dev/zero")).is_none());
    }

    #[test]
    fn copy_in_never_writes_through_a_planted_link() {
        let (t, env, p, host_home, sb) = setup();
        std::fs::write(host_home.join(".tool/token"), "tok").unwrap();
        let victim = t.path().join("victim");
        std::fs::create_dir_all(&victim).unwrap();
        // A previous run left `.tool` as a link to a host folder.
        symlink(&victim, sb.join(".tool")).unwrap();
        assert!(AuthSync::copy_in(&env, &p, &AuthMode::Shared, &sb).is_err());
        assert!(!victim.join("token").exists());
        // A file link is replaced, not written through.
        std::fs::remove_file(sb.join(".tool")).unwrap();
        std::fs::create_dir_all(sb.join(".tool")).unwrap();
        std::fs::write(victim.join("f"), "keep").unwrap();
        symlink(victim.join("f"), sb.join(".tool/token")).unwrap();
        AuthSync::copy_in(&env, &p, &AuthMode::Shared, &sb).unwrap();
        assert_eq!(std::fs::read_to_string(victim.join("f")).unwrap(), "keep");
        assert_eq!(
            std::fs::read_to_string(sb.join(".tool/token")).unwrap(),
            "tok"
        );
    }

    #[test]
    fn gitconfig_replaces_a_link() {
        let (t, env, _p, _h, sb) = setup();
        let victim = t.path().join("bashrc");
        std::fs::write(&victim, "keep").unwrap();
        symlink(&victim, sb.join(".gitconfig")).unwrap();
        write_gitconfig(&env, &sb, None).unwrap();
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep");
        assert!(std::fs::read_to_string(sb.join(".gitconfig"))
            .unwrap()
            .contains("[init]"));
    }
}

#[cfg(test)]
mod toml_set_link_tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn never_writes_through_a_planted_link() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("home");
        let outside = t.path().join("outside");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let profile: Profile = toml::from_str(
            "name = \"c\"\n[[state.toml_set]]\nfile = \".codex/config.toml\"\npath = [\"projects\", \"{project}\", \"trust_level\"]\nvalue = \"trusted\"\n",
        )
        .unwrap();
        // A folder link: refused, nothing created outside.
        symlink(&outside, home.join(".codex")).unwrap();
        assert!(toml_set(&profile, &home, Path::new("/w/app")).is_err());
        assert!(std::fs::read_dir(&outside).unwrap().next().is_none());
        // A file link: refused to read; the host file stays as it was.
        std::fs::remove_file(home.join(".codex")).unwrap();
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        std::fs::write(outside.join("rc"), "keep").unwrap();
        symlink(outside.join("rc"), home.join(".codex/config.toml")).unwrap();
        assert!(toml_set(&profile, &home, Path::new("/w/app")).is_err());
        assert_eq!(std::fs::read_to_string(outside.join("rc")).unwrap(), "keep");
    }
}
