//! Keep the launcher entries, the Omarchy menu block and the shell shims in
//! step with the installed harnesses.
//!
//! `paddock launchers install` and `paddock shims install` write entries for
//! the harnesses that exist at that moment. A harness installed later, or a
//! profile that a newer paddock brings, would stay missing. `refresh` writes
//! them again, only for the parts that the user turned on, and never edits
//! `~/.bashrc` or `~/.zshrc`.
//!
//! Detection takes a few hundred milliseconds, so it does not run on every
//! start. A cheap signal, the paddock version and the modification times of
//! the folders on PATH, is kept with the last refresh. When a harness starts
//! and the signal has changed, paddock runs `paddock refresh --quiet` in the
//! background. Installing an agent with mise, or into `~/.local/bin`,
//! changes a folder on PATH.

use crate::env::HostEnv;
use anyhow::Result;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn stamp_file(env: &HostEnv) -> PathBuf {
    env.paddock_state().join("refresh-signal")
}

/// The paddock version and, for each folder on PATH, its modification time.
pub fn signal(env: &HostEnv) -> String {
    let mut s = format!("paddock {}\n", env!("CARGO_PKG_VERSION"));
    if let Ok(me) = std::env::current_exe() {
        s.push_str(&format!("exe {}\n", me.display()));
    }
    let mut dirs: Vec<PathBuf> = env
        .path
        .iter()
        .filter(|d| d.is_absolute())
        .cloned()
        .collect();
    // Desktop apps can come from flatpak, outside PATH.
    dirs.push(env.data_dir.join("flatpak").join("app"));
    dirs.push(PathBuf::from("/var/lib/flatpak/app"));
    for d in dirs {
        let t = std::fs::metadata(&d)
            .map(|m| format!("{}.{}", m.mtime(), m.mtime_nsec()))
            .unwrap_or_else(|_| "-".into());
        s.push_str(&format!("{} {t}\n", d.display()));
    }
    s
}

/// Whether the signal differs from the one of the last refresh.
pub fn changed(env: &HostEnv) -> bool {
    std::fs::read_to_string(stamp_file(env)).ok().as_deref() != Some(signal(env).as_str())
}

#[derive(Debug, Default)]
pub struct Report {
    /// The launcher entries and the menu, when they are installed.
    pub launchers: Option<Vec<String>>,
    /// The shell shims, when they are installed.
    pub shims: Option<Vec<String>>,
}

/// Write the parts that are installed again, for the harnesses installed now.
/// With `prune`, entries of harnesses that are gone are removed too.
pub fn sync(env: &HostEnv, prune: bool) -> Result<Report> {
    let mut r = Report::default();
    if crate::launchers::installed(env) {
        let l = crate::launchers::install_with(env, prune)?;
        r.launchers = Some(l.entries.iter().map(|e| e.name.clone()).collect());
    }
    if crate::shims::installed(env) {
        let s = crate::shims::rewrite_script(env, prune)?;
        r.shims = Some(s.into_iter().map(|(_, c)| c).collect());
    }
    crate::privfs::write(&stamp_file(env), signal(env))?;
    Ok(r)
}

/// On a harness start: when the signal changed, refresh in the background,
/// so the start does not wait. Never inside a sandbox.
pub fn spawn_if_changed(env: &HostEnv) {
    if env.var("PADDOCK").is_some() || !changed(env) {
        return;
    }
    if !crate::launchers::installed(env) && !crate::shims::installed(env) {
        return;
    }
    let Ok(me) = std::env::current_exe() else {
        return;
    };
    let mut cmd = Command::new(me);
    cmd.args(["refresh", "--quiet"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: setsid is async-signal-safe. The refresh outlives this start.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let _ = cmd.spawn();
}

/// Lines for `paddock doctor`: harnesses that the menu or the shims miss.
pub fn doctor_lines(env: &HostEnv) -> Vec<String> {
    let mut out = Vec::new();
    if crate::launchers::installed(env) {
        if let Ok(entries) = crate::launchers::detect(env) {
            let missing: Vec<String> = entries
                .iter()
                .filter(|e| !e.desktop_file.exists())
                .map(|e| e.name.clone())
                .collect();
            if !missing.is_empty() {
                out.push(format!(
                    "  CHECK    the menu has no entry for {}: paddock refresh adds them",
                    missing.join(", ")
                ));
            }
        }
    }
    if crate::shims::installed(env) {
        if let Ok(want) = crate::shims::detect(env) {
            let have = crate::shims::active(env);
            let missing: Vec<String> = want
                .into_iter()
                .map(|(_, c)| c)
                .filter(|c| !have.contains(c))
                .collect();
            if !missing.is_empty() {
                out.push(format!(
                    "  CHECK    the shell shims miss {}: paddock refresh adds them",
                    missing.join(", ")
                ));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::Path;

    fn env_in(t: &Path) -> HostEnv {
        let mut vars = BTreeMap::new();
        for (k, d) in [
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_DATA_HOME", "data"),
            ("XDG_CACHE_HOME", "cache"),
            ("XDG_STATE_HOME", "state"),
            ("XDG_RUNTIME_DIR", "run"),
        ] {
            vars.insert(k.to_string(), t.join(d).display().to_string());
        }
        vars.insert("PATH".into(), t.join("bin").display().to_string());
        let home = t.join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(t.join("bin")).unwrap();
        std::fs::write(home.join(".bashrc"), "# mine\n").unwrap();
        HostEnv::from_parts(home, "demo".into(), 1000, 1000, vars)
    }

    fn harness(t: &Path, name: &str) {
        use std::os::unix::fs::PermissionsExt;
        let p = t.join("bin").join(name);
        std::fs::write(&p, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn menu(env: &HostEnv) -> bool {
        crate::launchers::detect(env)
            .unwrap()
            .iter()
            .all(|e| e.desktop_file.exists())
    }

    #[test]
    fn a_harness_installed_later_reaches_the_menu_and_the_shims() {
        let t = tempfile::tempdir().unwrap();
        let env = env_in(t.path());
        harness(t.path(), "codex");
        crate::launchers::install(&env).unwrap();
        crate::shims::install(&env).unwrap();
        let rc = std::fs::read_to_string(env.home.join(".bashrc")).unwrap();
        sync(&env, true).unwrap();
        assert!(!changed(&env));

        harness(t.path(), "grok");
        assert!(changed(&env));
        assert!(!menu(&env));
        let lines = doctor_lines(&env);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("menu has no entry for grok")),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("shims miss grok")),
            "{lines:?}"
        );

        let r = sync(&env, false).unwrap();
        assert!(r.launchers.unwrap().contains(&"grok".to_string()));
        assert!(crate::shims::active(&env).contains(&"grok".to_string()));
        assert!(menu(&env));
        assert!(doctor_lines(&env).is_empty());
        assert!(!changed(&env));
        // The shell configuration is not touched.
        assert_eq!(
            std::fs::read_to_string(env.home.join(".bashrc")).unwrap(),
            rc
        );
    }

    #[test]
    fn only_an_explicit_refresh_removes_a_harness_that_is_gone() {
        let t = tempfile::tempdir().unwrap();
        let env = env_in(t.path());
        harness(t.path(), "codex");
        harness(t.path(), "grok");
        crate::launchers::install(&env).unwrap();
        crate::shims::install(&env).unwrap();
        let entry = env.data_dir.join("applications/paddock-cli-codex.desktop");
        assert!(entry.exists());

        std::fs::remove_file(t.path().join("bin/codex")).unwrap();
        // A background refresh never drops a working shim or entry.
        sync(&env, false).unwrap();
        assert!(entry.exists());
        assert!(crate::shims::active(&env).contains(&"codex".to_string()));

        sync(&env, true).unwrap();
        assert!(!entry.exists());
        let active = crate::shims::active(&env);
        assert!(!active.contains(&"codex".to_string()) && active.contains(&"grok".to_string()));
    }

    #[test]
    fn nothing_is_turned_on_that_was_off() {
        let t = tempfile::tempdir().unwrap();
        let env = env_in(t.path());
        harness(t.path(), "codex");
        let r = sync(&env, true).unwrap();
        assert!(r.launchers.is_none() && r.shims.is_none());
        assert!(!crate::launchers::installed(&env) && !crate::shims::installed(&env));
    }
}
