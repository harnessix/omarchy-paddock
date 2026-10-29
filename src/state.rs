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

fn file_hash(p: &Path) -> Option<String> {
    std::fs::read(p).ok().map(|b| hex(&Sha256::digest(&b)))
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

struct AuthItem {
    stored: PathBuf,
    inside: PathBuf,
    inside_at_start: Option<String>,
    stored_at_start: Option<String>,
}

/// Credentials are copied, not bind-mounted: harnesses replace token files
/// with a rename, which a single-file bind mount does not survive.
pub struct AuthSync {
    pub mode: AuthMode,
    items: Vec<AuthItem>,
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
        let mut items = Vec::new();
        for rel in &profile.state.auth {
            let stored = root.join(rel);
            let inside = home_src.join(rel);
            if stored.is_file() {
                copy_private(&stored, &inside)?;
            } else if inside.is_file() && *mode == AuthMode::Shared {
                // The host is logged out: do not keep an older login inside.
                std::fs::remove_file(&inside)?;
            }
            items.push(AuthItem {
                inside_at_start: file_hash(&inside),
                stored_at_start: file_hash(&stored),
                stored,
                inside,
            });
        }
        Ok(Self {
            mode: mode.clone(),
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
        for it in &self.items {
            let now = file_hash(&it.inside);
            if now.is_none() || now == it.inside_at_start {
                continue;
            }
            if file_hash(&it.stored) != it.stored_at_start {
                r.conflicts.push(it.stored.clone());
                continue;
            }
            if let Some(dir) = it.stored.parent() {
                std::fs::create_dir_all(dir)?;
            }
            copy_private(&it.inside, &it.stored)?;
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

pub fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    let meta = std::fs::symlink_metadata(from)?;
    if meta.is_dir() {
        std::fs::create_dir_all(to)?;
        for e in std::fs::read_dir(from)? {
            let e = e?;
            copy_tree(&e.path(), &to.join(e.file_name()))?;
        }
    } else if meta.is_file() {
        if let Some(d) = to.parent() {
            std::fs::create_dir_all(d)?;
        }
        // A read-only leftover, such as an old bind-mount point, cannot be
        // overwritten in place.
        let _ = std::fs::remove_file(to);
        std::fs::copy(from, to)?;
    }
    Ok(())
}

/// Copy `seed` paths from the host home into the sandbox home.
pub fn seed(env: &HostEnv, profile: &Profile, home_src: &Path) -> Result<()> {
    for rel in &profile.state.seed {
        let host = env.home.join(rel);
        if host.exists() {
            copy_tree(&host, &home_src.join(rel)).with_context(|| format!("seed {rel}"))?;
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
        let inside_file = home_src.join(&ji.file);
        let mut inside: serde_json::Value = std::fs::read_to_string(&inside_file)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
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
        if let Some(d) = inside_file.parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::write(&inside_file, serde_json::to_string_pretty(&inside)?)?;
        std::fs::set_permissions(&inside_file, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// A minimal git configuration with only the identity, so commits work but
/// host credential helpers, aliases and includes stay outside.
pub fn write_gitconfig(env: &HostEnv, home_src: &Path) -> Result<()> {
    let (name, email) = env.git_identity();
    let mut text = String::from("# Written by paddock on each start. Edits are lost.\n[user]\n");
    if let Some(n) = name {
        text.push_str(&format!("\tname = {n}\n"));
    }
    if let Some(e) = email {
        text.push_str(&format!("\temail = {e}\n"));
    }
    text.push_str("[init]\n\tdefaultBranch = main\n");
    std::fs::write(home_src.join(".gitconfig"), text)?;
    Ok(())
}

pub fn snapshot(paths: &[PathBuf]) -> BTreeMap<PathBuf, Option<String>> {
    paths.iter().map(|p| (p.clone(), file_hash(p))).collect()
}

pub fn changed(before: &BTreeMap<PathBuf, Option<String>>) -> Vec<PathBuf> {
    before
        .iter()
        .filter(|(p, h)| file_hash(p) != **h)
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
pub fn toml_set(profile: &Profile, home_src: &Path, project: &Path) -> Result<()> {
    for t in &profile.state.toml_set {
        let file = home_src.join(&t.file);
        let text = std::fs::read_to_string(&file).unwrap_or_default();
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
        if let Some(d) = file.parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::write(&file, doc.to_string())?;
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
