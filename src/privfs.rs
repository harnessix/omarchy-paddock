//! Private files for paddock's own state.
//!
//! Paddock keeps approvals, the audit log, keyring passwords, session records
//! and sandbox homes under `$XDG_STATE_HOME/paddock`, `$XDG_DATA_HOME/paddock`,
//! `$XDG_CACHE_HOME/paddock` and `$XDG_RUNTIME_DIR/paddock`. Other local users
//! must not read them. A umask of 022 would give new files mode 0644 and new
//! folders 0755, so paddock never relies on the umask there: folders get 0700
//! and files 0600 when they are created, and existing ones that group or
//! others can read are tightened when paddock opens them.

use crate::env::HostEnv;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub const DIR_MODE: u32 = 0o700;
pub const FILE_MODE: u32 = 0o600;

/// The roots of paddock's own state.
pub fn roots(env: &HostEnv) -> Vec<PathBuf> {
    vec![
        env.paddock_state(),
        env.paddock_data(),
        env.paddock_cache(),
        env.paddock_run(),
    ]
}

/// Create the roots that are missing with 0700, and take group and other
/// access away from those that exist. Errors are logged, not returned: a
/// read-only home must not stop `paddock status`.
pub fn secure_roots(env: &HostEnv) {
    for r in roots(env) {
        if let Err(e) = dir(&r) {
            log::debug!("cannot secure {}: {e}", r.display());
        }
    }
}

/// Take group and other access away from `path`, if it has any. Symbolic
/// links are left alone: their target is not paddock's.
pub fn tighten(path: &Path, mode: u32) -> io::Result<()> {
    let m = std::fs::symlink_metadata(path)?;
    if m.file_type().is_symlink() {
        return Ok(());
    }
    if m.permissions().mode() & 0o077 != 0 {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

/// Create a folder and its missing parents with 0700. The folder itself is
/// tightened when it already exists. Existing parents are not changed.
pub fn dir(path: &Path) -> io::Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(DIR_MODE)
        .create(path)?;
    tighten(path, DIR_MODE)
}

fn open(path: &Path, opts: &mut OpenOptions) -> io::Result<File> {
    if let Some(d) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        dir(d)?;
    }
    let f = opts.mode(FILE_MODE).open(path)?;
    // An existing file keeps its old mode on open: tighten it.
    let m = f.metadata()?;
    if m.permissions().mode() & 0o077 != 0 {
        f.set_permissions(std::fs::Permissions::from_mode(FILE_MODE))?;
    }
    Ok(f)
}

/// Write a whole file with 0600, in a 0700 folder.
pub fn write(path: &Path, bytes: impl AsRef<[u8]>) -> io::Result<()> {
    let mut f = open(
        path,
        OpenOptions::new().write(true).create(true).truncate(true),
    )?;
    f.write_all(bytes.as_ref())
}

/// Write a file atomically: a private temporary file next to it, then a
/// rename. Readers never see half a file.
pub fn write_atomic(path: &Path, bytes: impl AsRef<[u8]>) -> io::Result<()> {
    let tmp = path.with_extension("paddock-tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut f = open(&tmp, OpenOptions::new().write(true).create_new(true))?;
    f.write_all(bytes.as_ref())?;
    drop(f);
    std::fs::rename(&tmp, path)
}

/// Open a log file for appending, with 0600 in a 0700 folder.
pub fn append(path: &Path) -> io::Result<File> {
    open(path, OpenOptions::new().create(true).append(true))
}

/// Copy a file to a private path atomically. The copy is 0600 from the start.
pub fn copy_atomic(from: &Path, to: &Path) -> io::Result<()> {
    let tmp = to.with_extension("paddock-tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut src = File::open(from)?;
    let mut f = open(&tmp, OpenOptions::new().write(true).create_new(true))?;
    io::copy(&mut src, &mut f)?;
    drop(f);
    std::fs::rename(&tmp, to)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// Run `f` with umask 022, the common default that makes new files
    /// world-readable. The umask is per process, so tests that change it
    /// take this lock; other tests only create files in their own temporary
    /// folders, where a stricter umask changes nothing they check.
    pub fn with_umask_022<T>(f: impl FnOnce() -> T) -> T {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let old = unsafe { libc::umask(0o022) };
        let r = f();
        unsafe { libc::umask(old) };
        r
    }

    pub fn mode(p: &Path) -> u32 {
        std::fs::symlink_metadata(p).unwrap().permissions().mode() & 0o777
    }

    pub fn env_in(t: &Path) -> HostEnv {
        let mut vars = BTreeMap::new();
        vars.insert(
            "XDG_RUNTIME_DIR".to_string(),
            t.join("run").display().to_string(),
        );
        HostEnv::from_parts(t.join("home"), "demo".into(), 1000, 1000, vars)
    }

    #[test]
    fn new_folders_and_files_are_private_under_umask_022() {
        let t = tempfile::tempdir().unwrap();
        with_umask_022(|| {
            let f = t.path().join("a/b/c.json");
            write(&f, "{}").unwrap();
            assert_eq!(mode(&t.path().join("a")), 0o700);
            assert_eq!(mode(&t.path().join("a/b")), 0o700);
            assert_eq!(mode(&f), 0o600);
            let l = t.path().join("a/log.jsonl");
            append(&l).unwrap();
            assert_eq!(mode(&l), 0o600);
            let at = t.path().join("a/atomic");
            write_atomic(&at, "x").unwrap();
            assert_eq!(mode(&at), 0o600);
            let src = t.path().join("src");
            std::fs::write(&src, "token").unwrap();
            assert_eq!(mode(&src), 0o644);
            let cp = t.path().join("a/copy");
            copy_atomic(&src, &cp).unwrap();
            assert_eq!(mode(&cp), 0o600);
            assert_eq!(std::fs::read_to_string(&cp).unwrap(), "token");
        });
    }

    #[test]
    fn existing_loose_folders_and_files_are_tightened() {
        let t = tempfile::tempdir().unwrap();
        with_umask_022(|| {
            let d = t.path().join("state");
            std::fs::create_dir(&d).unwrap();
            std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o755)).unwrap();
            let f = d.join("host-audit.jsonl");
            std::fs::write(&f, "old\n").unwrap();
            std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
            append(&f).unwrap();
            assert_eq!(mode(&d), 0o700);
            assert_eq!(mode(&f), 0o600);
            assert_eq!(std::fs::read_to_string(&f).unwrap(), "old\n");
        });
    }

    #[test]
    fn every_root_is_created_or_tightened_to_0700() {
        let t = tempfile::tempdir().unwrap();
        let env = env_in(t.path());
        with_umask_022(|| {
            // One root exists and is world-readable already.
            std::fs::create_dir_all(env.paddock_state()).unwrap();
            std::fs::set_permissions(env.paddock_state(), std::fs::Permissions::from_mode(0o755))
                .unwrap();
            secure_roots(&env);
            for r in roots(&env) {
                assert_eq!(mode(&r), 0o700, "{}", r.display());
            }
        });
    }
}
