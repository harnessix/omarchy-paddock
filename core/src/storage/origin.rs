use crate::state::{self, AuthMode};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

pub const ORIGIN_FILE: &str = ".paddock-origin";
const ORIGIN_VERSION: u32 = 1;
/// Larger origin files are not paddock's: ignore them.
const ORIGIN_MAX: u64 = 64 * 1024;

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

pub(super) fn auth_from_dir(login: &str) -> Option<AuthMode> {
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
pub(super) fn origin_matches(o: &Origin, harness: &str, auth: &str, key: &str) -> bool {
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
