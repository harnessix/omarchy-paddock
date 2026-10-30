//! Facts about the host, read at runtime. Nothing in this crate hard-codes a
//! user name, a home directory, a UID, or a host name: everything comes from here.

use anyhow::{anyhow, Context, Result};
use std::collections::BTreeMap;
use std::ffi::CStr;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone)]
pub struct HostEnv {
    pub home: PathBuf,
    pub user: String,
    pub uid: u32,
    pub gid: u32,
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub state_dir: PathBuf,
    pub runtime_dir: Option<PathBuf>,
    pub path: Vec<PathBuf>,
    pub vars: BTreeMap<String, String>,
}

impl HostEnv {
    pub fn detect() -> Result<Self> {
        let vars: BTreeMap<String, String> = std::env::vars().collect();
        let uid = unsafe { libc::getuid() };
        let gid = unsafe { libc::getgid() };
        let (pw_name, pw_home) = passwd_entry(uid);
        let home = vars
            .get("HOME")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from)
            .or(pw_home)
            .ok_or_else(|| anyhow!("cannot determine the home directory: HOME is not set"))?;
        let user = vars
            .get("USER")
            .filter(|u| !u.is_empty())
            .cloned()
            .or(pw_name)
            .unwrap_or_else(|| format!("u{uid}"));
        Ok(Self::from_parts(home, user, uid, gid, vars))
    }

    /// Build an environment from explicit parts. Tests use this with a fake home.
    pub fn from_parts(
        home: PathBuf,
        user: String,
        uid: u32,
        gid: u32,
        vars: BTreeMap<String, String>,
    ) -> Self {
        let xdg = |key: &str, fallback: &str| {
            vars.get(key)
                .filter(|v| Path::new(v).is_absolute())
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(fallback))
        };
        let config_dir = xdg("XDG_CONFIG_HOME", ".config");
        let data_dir = xdg("XDG_DATA_HOME", ".local/share");
        let cache_dir = xdg("XDG_CACHE_HOME", ".cache");
        let state_dir = xdg("XDG_STATE_HOME", ".local/state");
        let runtime_dir = vars
            .get("XDG_RUNTIME_DIR")
            .filter(|v| Path::new(v).is_absolute())
            .map(PathBuf::from);
        let path = vars
            .get("PATH")
            .map(|p| std::env::split_paths(p).collect())
            .unwrap_or_default();
        Self {
            home,
            user,
            uid,
            gid,
            config_dir,
            data_dir,
            cache_dir,
            state_dir,
            runtime_dir,
            path,
            vars,
        }
    }

    pub fn var(&self, key: &str) -> Option<&str> {
        self.vars.get(key).map(String::as_str)
    }

    /// Expand a leading `~/` and make relative paths absolute against `base`.
    pub fn expand(&self, raw: &str, base: &Path) -> PathBuf {
        if raw == "~" {
            self.home.clone()
        } else if let Some(rest) = raw.strip_prefix("~/") {
            self.home.join(rest)
        } else if Path::new(raw).is_absolute() {
            PathBuf::from(raw)
        } else {
            base.join(raw)
        }
    }

    pub fn which(&self, name: &str) -> Option<PathBuf> {
        if name.contains('/') {
            let p = PathBuf::from(name);
            return is_executable(&p).then_some(p);
        }
        // A relative PATH entry (`node_modules/.bin`, `.`, or an empty one)
        // resolves against the working directory, usually the project. The
        // sandbox writes the project, so paddock never looks there.
        self.path
            .iter()
            .filter(|dir| dir.is_absolute())
            .map(|dir| dir.join(name))
            .find(|p| is_executable(p))
    }

    /// Paddock's own directories. All of them live under XDG locations.
    pub fn paddock_data(&self) -> PathBuf {
        self.data_dir.join("paddock")
    }
    pub fn paddock_cache(&self) -> PathBuf {
        self.cache_dir.join("paddock")
    }
    pub fn paddock_state(&self) -> PathBuf {
        self.state_dir.join("paddock")
    }
    pub fn paddock_config(&self) -> PathBuf {
        self.config_dir.join("paddock")
    }
    pub fn paddock_run(&self) -> PathBuf {
        match &self.runtime_dir {
            Some(dir) => dir.join("paddock"),
            None => self.paddock_state().join("run"),
        }
    }

    /// Read a global git identity so commits made in the sandbox carry it,
    /// without exposing the rest of the host git configuration.
    pub fn git_identity(&self) -> (Option<String>, Option<String>) {
        let get = |key: &str| -> Option<String> {
            let out = Command::new("git")
                .args(["config", "--global", "--get", key])
                .output()
                .ok()?;
            let v = String::from_utf8(out.stdout).ok()?.trim().to_string();
            (!v.is_empty()).then_some(v)
        };
        (get("user.name"), get("user.email"))
    }
}

pub fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

fn passwd_entry(uid: u32) -> (Option<String>, Option<PathBuf>) {
    unsafe {
        let pw = libc::getpwuid(uid);
        if pw.is_null() {
            return (None, None);
        }
        let name = CStr::from_ptr((*pw).pw_name).to_string_lossy().into_owned();
        let dir = CStr::from_ptr((*pw).pw_dir).to_string_lossy().into_owned();
        (Some(name), Some(PathBuf::from(dir)))
    }
}

/// Run a command and return trimmed stdout when it succeeds.
pub fn capture(cmd: &mut Command) -> Result<String> {
    let out = cmd
        .output()
        .with_context(|| format!("failed to run {:?}", cmd.get_program()))?;
    if !out.status.success() {
        return Err(anyhow!(
            "{:?} exited with {}: {}",
            cmd.get_program(),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}
