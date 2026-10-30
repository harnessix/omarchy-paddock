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
    let login = match auth {
        AuthMode::Shared => "shared".to_string(),
        AuthMode::Isolated(n) => format!("isolated-{n}"),
    };
    let dir = env
        .paddock_data()
        .join("homes")
        .join(harness)
        .join(login)
        .join(slug(project));
    std::fs::create_dir_all(&dir)?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(dir)
}

/// Which login a sandbox uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthMode {
    /// The host's own login. Credential files in the host home are the store.
    Shared,
    /// A separate login kept by paddock, with a name for several accounts.
    Isolated(String),
}

impl std::str::FromStr for AuthMode {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        let name = match s {
            "shared" => return Ok(Self::Shared),
            "isolated" => "default",
            other => other.strip_prefix("isolated:").ok_or_else(|| {
                anyhow::anyhow!("unknown auth mode '{s}': use shared, isolated or isolated:NAME")
            })?,
        };
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
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
        }
    }
}

/// The directory that holds the credential files for a mode.
pub fn auth_root(env: &HostEnv, harness: &str, mode: &AuthMode) -> PathBuf {
    match mode {
        AuthMode::Shared => env.home.clone(),
        AuthMode::Isolated(name) => env.paddock_data().join("auth").join(harness).join(name),
    }
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
    if let Some(dir) = to.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Write next to the target, then rename: the same atomic pattern the
    // harnesses use, so a crash never leaves half a token.
    let tmp = to.with_extension("paddock-tmp");
    std::fs::copy(from, &tmp)
        .with_context(|| format!("copy {} -> {}", from.display(), tmp.display()))?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    std::fs::rename(&tmp, to)?;
    Ok(())
}

/// Write bytes to a host path that only the host can write, atomically.
fn write_private(bytes: &[u8], to: &Path) -> Result<()> {
    if let Some(dir) = to.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = to.with_extension("paddock-tmp");
    std::fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    std::fs::rename(&tmp, to)?;
    Ok(())
}

struct AuthItem {
    stored: PathBuf,
    /// Relative to the sandbox home, which the sandbox can write.
    rel: PathBuf,
    inside_at_start: Option<String>,
    stored_at_start: Option<String>,
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
        let root = auth_root(env, &profile.name, mode);
        let home = crate::safefs::Root::open(home_src)?;
        let mut items = Vec::new();
        for rel in &profile.state.auth {
            let stored = root.join(rel);
            let rel = PathBuf::from(rel);
            if stored.is_file() {
                let bytes = std::fs::read(&stored)
                    .with_context(|| format!("cannot read {}", stored.display()))?;
                home.write(&rel, &bytes, 0o600)
                    .with_context(|| format!("credential {}", rel.display()))?;
            } else if *mode == AuthMode::Shared {
                // The host is logged out: do not keep an older login inside.
                home.remove(&rel)?;
            }
            items.push(AuthItem {
                inside_at_start: inside_hash(&home, &rel)?,
                stored_at_start: file_hash(&stored),
                stored,
                rel,
            });
        }
        Ok(Self {
            mode: mode.clone(),
            home: home_src.to_path_buf(),
            items,
        })
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
        for it in &self.items {
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
            if file_hash(&it.stored) != it.stored_at_start {
                r.conflicts.push(it.stored.clone());
                continue;
            }
            write_private(&bytes, &it.stored)?;
            r.saved.push(it.stored.clone());
        }
        Ok(r)
    }
}

/// Copy the host's credential files for a harness into an isolated store.
pub fn auth_import(env: &HostEnv, profile: &Profile, mode: &AuthMode) -> Result<Vec<PathBuf>> {
    if *mode == AuthMode::Shared {
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
pub fn seed(env: &HostEnv, profile: &Profile, home_src: &Path) -> Result<()> {
    let home = crate::safefs::Root::open(home_src)?;
    for rel in &profile.state.seed {
        let host = env.home.join(rel);
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

fn json_set(v: &mut serde_json::Value, path: &[String], val: serde_json::Value) {
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
        let host_file = env.home.join(&ji.file);
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
        if *mode == AuthMode::Shared {
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
    std::fs::create_dir_all(f.parent().unwrap())?;
    std::fs::write(f, serde_json::to_string_pretty(&v)?)?;
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
        std::fs::create_dir_all(f.parent().unwrap())?;
        std::fs::write(f, serde_json::to_string(self)?)?;
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
    if let Some(d) = file.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let _ = std::fs::write(&file, &hash);
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
