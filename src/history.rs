//! Session history on the host, for tools that read it there, such as the
//! usage statistics of the Omarchy agents panel (`[state.history]`).
//!
//! - Claude Code keeps one folder per project under `~/.claude/projects`.
//!   paddock binds the host folder of this project, read-write, at the same
//!   place in the sandbox home. The sandbox sees no other project's folder.
//! - Agents that keep history by date (Codex `sessions/YYYY/MM/DD`) get new
//!   history copied back at exit. paddock reads the sandbox home through
//!   `safefs`, so no symlink there is followed, and writes the host folder
//!   the same way. A file is copied only when the host has none, or when the
//!   sandbox only appended to it.
//!
//! paddock never writes the panel's own usage records: Omarchy's collectors
//! read this history as they read a session on the host.

use crate::env::HostEnv;
use crate::profile::Profile;
use crate::safefs::{Dir, Kind, Root};
use crate::spec::{Mount, SandboxSpec};
use crate::state::{self, AuthMode};
use anyhow::Result;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// The largest history file that paddock copies back.
const MAX_FILE: u64 = 256 << 20;
/// The most files that one exit copies back.
const MAX_FILES: usize = 10_000;
const MAX_DEPTH: usize = 8;

/// The folder name that Claude Code gives a project: every character that
/// is not an ASCII letter or digit becomes `-`.
pub fn claude_key(project: &Path) -> String {
    project
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Only the logins of the host share its history; an isolated login keeps
/// its own.
fn shares_history(auth: &AuthMode) -> bool {
    auth.is_host_login()
}

/// Bind the host folder of this project. The host folder is created when it
/// is missing, as the harness would create it on the host. A symlink on the
/// way is refused: paddock binds only a real folder of the host.
pub fn apply(
    env: &HostEnv,
    profile: &Profile,
    auth: &AuthMode,
    spec: &mut SandboxSpec,
) -> Result<()> {
    let Some(h) = &profile.state.history else {
        return Ok(());
    };
    let Some(folders) = &h.project_folders else {
        return Ok(());
    };
    if !shares_history(auth) {
        return Ok(());
    }
    let key = claude_key(&spec.project);
    // Claude Code shortens long names with a hash that paddock cannot
    // compute: those projects keep their history in the sandbox home.
    if key.len() > 200 {
        return Ok(());
    }
    let host_root = state::host_file(env, profile, auth, folders);
    let host = host_root.join(&key);
    let writable = crate::build::sandbox_writable(env, spec);
    if writable.iter().any(|w| {
        std::fs::canonicalize(&host_root)
            .unwrap_or_else(|_| host_root.clone())
            .starts_with(w)
    }) {
        anyhow::bail!(
            "{} is inside a folder that a sandbox can write; paddock does not bind history there",
            host_root.display()
        );
    }
    crate::privfs::dir(&host)?;
    for p in [&host_root, &host] {
        if std::fs::symlink_metadata(p)?.file_type().is_symlink() {
            anyhow::bail!(
                "{} is a symlink; paddock binds only a real folder",
                p.display()
            );
        }
    }
    let dst = spec.home.join(folders).join(&key);
    spec.add_mount(Mount::bind(
        &host,
        dst,
        true,
        &format!(
            "profile {} history: this project's sessions on the host, for tools that read them there",
            profile.name
        ),
    ));
    Ok(())
}

/// History folders that are copied back at exit.
pub struct CopyBack {
    home: PathBuf,
    /// The folder relative to the sandbox home, and its host folder.
    folders: Vec<(PathBuf, PathBuf)>,
    files: Vec<String>,
}

impl CopyBack {
    pub fn new(env: &HostEnv, profile: &Profile, auth: &AuthMode, home_src: &Path) -> Self {
        let mut folders = Vec::new();
        let mut files = Vec::new();
        if let Some(h) = &profile.state.history {
            if shares_history(auth) {
                for rel in &h.copy_back {
                    folders.push((
                        PathBuf::from(rel),
                        state::host_file(env, profile, auth, rel),
                    ));
                }
                files = h.files.clone();
            }
        }
        Self {
            home: home_src.to_path_buf(),
            folders,
            files,
        }
    }

    fn wanted(&self, name: &OsStr) -> bool {
        let n = name.to_string_lossy();
        self.files.is_empty() || self.files.iter().any(|g| crate::recover::glob(g, &n))
    }

    /// Copy new history to the host. Returns the files copied.
    pub fn copy_out(&self) -> Result<Vec<PathBuf>> {
        let mut done = Vec::new();
        if self.folders.is_empty() {
            return Ok(done);
        }
        let home = Root::open(&self.home)?;
        for (rel, host) in &self.folders {
            let Ok(inside) = home.dir(rel, false) else {
                continue;
            };
            std::fs::create_dir_all(host)?;
            let host_root = Root::open(host)?;
            self.walk(&inside, Path::new(""), &host_root, host, 0, &mut done);
        }
        Ok(done)
    }

    fn walk(
        &self,
        dir: &Dir,
        rel: &Path,
        host_root: &Root,
        host: &Path,
        depth: usize,
        done: &mut Vec<PathBuf>,
    ) {
        if depth > MAX_DEPTH || done.len() >= MAX_FILES {
            return;
        }
        let Ok(entries) = dir.entries() else {
            return;
        };
        for (name, kind) in entries {
            let path = rel.join(&name);
            match kind {
                Kind::Dir => {
                    if let Ok(sub) = dir.subdir(&name, false) {
                        self.walk(&sub, &path, host_root, host, depth + 1, done);
                    }
                }
                Kind::File if self.wanted(&name) => match copy_one(dir, &name, host_root, &path) {
                    Ok(true) => done.push(host.join(&path)),
                    Ok(false) => {}
                    Err(e) => log::debug!("history {}: {e:#}", path.display()),
                },
                // Symlinks and special files are never followed.
                _ => {}
            }
            if done.len() >= MAX_FILES {
                return;
            }
        }
    }
}

/// Copy one file when the host has none, or when the sandbox only appended
/// to the host's copy.
fn copy_one(dir: &Dir, name: &OsStr, host_root: &Root, rel: &Path) -> Result<bool> {
    let Some(bytes) = dir.read(name, MAX_FILE)? else {
        return Ok(false);
    };
    let old = host_root.read(rel, MAX_FILE)?;
    match old {
        Some(o) if o == bytes => return Ok(false),
        Some(o) if !bytes.starts_with(&o) => return Ok(false),
        _ => {}
    }
    host_root.write(rel, &bytes, 0o600)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_names_a_project_folder_like_this() {
        assert_eq!(
            claude_key(Path::new("/home/demo/env_manager_dir/paddock")),
            "-home-demo-env-manager-dir-paddock"
        );
        assert_eq!(
            claude_key(Path::new("/home/demo/x/.claude/worktrees/a")),
            "-home-demo-x--claude-worktrees-a"
        );
    }

    fn profile() -> Profile {
        toml::from_str(
            "name = \"t\"\n[state.history]\ncopy_back = [\".t/sessions\"]\nfiles = [\"*.jsonl\"]\n",
        )
        .unwrap()
    }

    #[test]
    fn new_and_appended_history_is_copied_back_and_nothing_else() {
        let t = tempfile::tempdir().unwrap();
        let host_home = t.path().join("host");
        let sb = t.path().join("sandbox");
        std::fs::create_dir_all(host_home.join(".t/sessions/2026/10")).unwrap();
        std::fs::create_dir_all(sb.join(".t/sessions/2026/10")).unwrap();
        let env = HostEnv::from_parts(host_home.clone(), "demo".into(), 1, 1, Default::default());
        let hs = host_home.join(".t/sessions/2026/10");
        let ss = sb.join(".t/sessions/2026/10");
        std::fs::write(hs.join("grown.jsonl"), "a\n").unwrap();
        std::fs::write(ss.join("grown.jsonl"), "a\nb\n").unwrap();
        std::fs::write(hs.join("changed.jsonl"), "host\n").unwrap();
        std::fs::write(ss.join("changed.jsonl"), "other\n").unwrap();
        std::fs::write(ss.join("new.jsonl"), "n\n").unwrap();
        std::fs::write(ss.join("notes.txt"), "x").unwrap();
        let secret = t.path().join("id_key");
        std::fs::write(&secret, "PRIVATE KEY").unwrap();
        std::os::unix::fs::symlink(&secret, ss.join("link.jsonl")).unwrap();
        let c = CopyBack::new(&env, &profile(), &AuthMode::Shared, &sb);
        let done = c.copy_out().unwrap();
        assert_eq!(done.len(), 2, "{done:?}");
        assert_eq!(
            std::fs::read_to_string(hs.join("grown.jsonl")).unwrap(),
            "a\nb\n"
        );
        assert_eq!(
            std::fs::read_to_string(hs.join("new.jsonl")).unwrap(),
            "n\n"
        );
        assert_eq!(
            std::fs::read_to_string(hs.join("changed.jsonl")).unwrap(),
            "host\n"
        );
        assert!(!hs.join("notes.txt").exists());
        assert!(!hs.join("link.jsonl").exists());
        // An isolated login keeps its history to itself.
        std::fs::write(ss.join("later.jsonl"), "l\n").unwrap();
        let iso: AuthMode = "isolated:work".parse().unwrap();
        assert!(CopyBack::new(&env, &profile(), &iso, &sb)
            .copy_out()
            .unwrap()
            .is_empty());
    }
}
