//! Caches in the sandbox homes, sorted into classes.
//!
//! - Scratch: pure caches that rebuild themselves (shader and font caches,
//!   the browser caches of Electron apps). Paths that paddock knows before
//!   the start are an empty tmpfs in the sandbox: they vanish at exit. Paths
//!   with a `*` are only known after the app ran: `paddock gc` clears them in
//!   its automatic tier.
//! - Shared: package caches that check each entry themselves (npm, pip, uv).
//!   The detectors mount one shared copy for every project (detect.rs).
//! - Runtime: runtimes that a harness downloads into its home. They stay,
//!   and `paddock gc --dedupe` shares their data between homes.
//!
//! A profile adds its own scratch paths with `[state] scratch`. No entry
//! covers a login, settings or history file of the profile.

use crate::env::HostEnv;
use crate::profile::Profile;
use serde::Serialize;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Class {
    Scratch,
    Shared,
    Runtime,
}

impl Class {
    pub fn label(self) -> &'static str {
        match self {
            Class::Scratch => "pure caches (tmpfs, or cleared by gc)",
            Class::Shared => "package caches (one shared copy for all projects)",
            Class::Runtime => "runtimes (kept, gc --dedupe shares them)",
        }
    }
}

/// Paths relative to HOME. `*` stands for one folder name.
pub const TABLE: &[(&str, Class)] = &[
    (".cache/mesa_shader_cache", Class::Scratch),
    (".cache/mesa_shader_cache_db", Class::Scratch),
    (".cache/radv_builtin_shaders", Class::Scratch),
    (".cache/fontconfig", Class::Scratch),
    // Chromium and Electron apps keep these next to their login and settings.
    (".config/*/Cache", Class::Scratch),
    (".config/*/Code Cache", Class::Scratch),
    (".config/*/GPUCache", Class::Scratch),
    (".config/*/DawnGraphiteCache", Class::Scratch),
    (".config/*/DawnWebGPUCache", Class::Scratch),
    (".config/*/GrShaderCache", Class::Scratch),
    (".config/*/ShaderCache", Class::Scratch),
    (".npm", Class::Shared),
    (".cache/pip", Class::Shared),
    (".cache/uv", Class::Shared),
    (".codex/packages", Class::Runtime),
    (".copilot/pkg", Class::Runtime),
    (".cache/codex-runtimes", Class::Runtime),
    (".codex/codex-runtimes", Class::Runtime),
];

/// `[storage] tmpfs_caches` of the user configuration. On by default.
pub fn enabled(env: &HostEnv) -> bool {
    let text =
        std::fs::read_to_string(env.paddock_config().join("config.toml")).unwrap_or_default();
    enabled_in(&text)
}

pub fn enabled_in(text: &str) -> bool {
    let Ok(v) = toml::from_str::<toml::Value>(text) else {
        return true;
    };
    v.get("storage")
        .and_then(|s| s.get("tmpfs_caches"))
        .and_then(|d| d.as_bool())
        .unwrap_or(true)
}

/// A plain relative path: names and `*` only.
fn plain(rel: &str) -> bool {
    let p = Path::new(rel);
    !rel.is_empty() && p.is_relative() && p.components().all(|c| matches!(c, Component::Normal(_)))
}

/// The files of a profile that hold a login or settings. A scratch path
/// must not cover one of them.
fn kept(p: &Profile) -> Vec<String> {
    let s = &p.state;
    let mut v: Vec<String> = s.auth.iter().chain(&s.seed).cloned().collect();
    v.extend(s.json_import.iter().map(|j| j.file.clone()));
    v.extend(s.toml_set.iter().map(|t| t.file.clone()));
    v.extend(s.settings.iter().map(|t| t.file.clone()));
    v.extend(folders(p));
    v
}

/// History folders and host imports. A scratch path must not cover one,
/// and must not lie inside one either.
fn folders(p: &Profile) -> Vec<String> {
    let s = &p.state;
    let mut v = s.import_ro.clone();
    if let Some(h) = &s.history {
        v.extend(h.project_folders.clone());
        v.extend(h.copy_back.iter().cloned());
    }
    v
}

/// Does the pattern match this path, or a folder above it?
fn covers(pattern: &str, path: &str) -> bool {
    let pat: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();
    let p: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    pat.len() <= p.len() && pat.iter().zip(&p).all(|(a, b)| *a == "*" || a == b)
}

/// The scratch paths of a profile: the table and `[state] scratch`. An
/// entry that is not a plain relative path, or that covers a file the
/// profile keeps, is left out and named in the second list.
pub fn scratch(profile: Option<&Profile>) -> (Vec<String>, Vec<String>) {
    let mut ok: Vec<String> = TABLE
        .iter()
        .filter(|(_, c)| *c == Class::Scratch)
        .map(|(p, _)| p.to_string())
        .collect();
    let mut bad = Vec::new();
    if let Some(p) = profile {
        let (kept, folders) = (kept(p), folders(p));
        for s in &p.state.scratch {
            let s = s.trim_end_matches('/');
            if !plain(s)
                || kept.iter().any(|k| covers(s, k))
                || folders.iter().any(|f| covers(f, s))
            {
                bad.push(s.to_string());
            } else if !ok.iter().any(|o| o == s) {
                ok.push(s.to_string());
            }
        }
    }
    (ok, bad)
}

/// Mount each scratch path without a `*` as an empty tmpfs.
pub fn mount(env: &HostEnv, profile: &Profile, spec: &mut crate::spec::SandboxSpec) {
    if !enabled(env) {
        return;
    }
    let (ok, bad) = scratch(Some(profile));
    for rel in ok.iter().filter(|r| !r.split('/').any(|c| c == "*")) {
        let dst = spec.home.join(rel);
        // A mount that the user, a detector or the profile asked for wins.
        if spec
            .mounts
            .iter()
            .any(|m| m.dst.starts_with(&dst) || (dst.starts_with(&m.dst) && m.dst != spec.home))
        {
            continue;
        }
        spec.add_mount(crate::spec::Mount::tmpfs(
            dst,
            "storage: pure cache (tmpfs_caches)",
        ));
    }
    for b in bad {
        spec.warnings.push(format!(
            "profile {}: [state] scratch entry '{b}' is not used: it is not a plain path inside the home, or it covers a login, settings or history file",
            profile.name
        ));
    }
}

/// The real folders that match a pattern below `home`. The walk never
/// follows a symlink: the sandbox can plant one anywhere in its home.
pub fn expand(home: &Path, pattern: &str) -> Vec<PathBuf> {
    let mut cur = vec![PathBuf::new()];
    for part in pattern.split('/').filter(|s| !s.is_empty()) {
        let mut next = Vec::new();
        for rel in &cur {
            if part == "*" {
                let Ok(rd) = std::fs::read_dir(home.join(rel)) else {
                    continue;
                };
                let mut names: Vec<PathBuf> = rd
                    .flatten()
                    .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                    .map(|e| rel.join(e.file_name()))
                    .collect();
                names.sort();
                next.extend(names);
            } else {
                let r = rel.join(part);
                if std::fs::symlink_metadata(home.join(&r)).is_ok_and(|m| m.is_dir()) {
                    next.push(r);
                }
            }
        }
        cur = next;
    }
    cur.retain(|r| !r.as_os_str().is_empty());
    cur
}

/// The folders of each class in a home, relative to it. `scratch` comes
/// from [`scratch`] for the harness of the home.
pub fn find(home: &Path, scratch: &[String]) -> Vec<(Class, PathBuf)> {
    let mut out: Vec<(Class, PathBuf)> = scratch
        .iter()
        .flat_map(|p| expand(home, p))
        .map(|r| (Class::Scratch, r))
        .collect();
    for (p, c) in TABLE.iter().filter(|(_, c)| *c != Class::Scratch) {
        out.extend(expand(home, p).into_iter().map(|r| (*c, r)));
    }
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(text: &str) -> Profile {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn the_table_is_plain_and_sorted_into_classes() {
        for (p, _) in TABLE {
            assert!(plain(p), "{p}");
        }
        let has = |c: Class, p: &str| TABLE.contains(&(p, c));
        assert!(has(Class::Scratch, ".cache/mesa_shader_cache"));
        assert!(has(Class::Scratch, ".cache/fontconfig"));
        assert!(has(Class::Scratch, ".config/*/GPUCache"));
        assert!(has(Class::Shared, ".npm"));
        assert!(has(Class::Shared, ".cache/uv"));
        assert!(has(Class::Runtime, ".codex/packages"));
        // No login, settings or history folder of a built-in profile.
        for (name, text) in crate::profile::BUILTIN {
            let p: Profile = toml::from_str(text).unwrap();
            let (_, bad) = scratch(Some(&p));
            assert!(bad.is_empty(), "{name}: {bad:?}");
            for k in kept(&p) {
                for (s, c) in TABLE {
                    assert!(
                        *c != Class::Scratch || !covers(s, &k),
                        "{name}: {s} covers {k}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_profile_adds_scratch_paths_but_never_its_login() {
        let p = profile(
            r#"
name = "t"
[state]
auth = [".t/auth.json"]
seed = [".t/settings.json"]
scratch = [".t/cache", ".t", "../x", "/abs", ".t/*", ".t/sessions/tmp", ".cache/fontconfig"]
[state.history]
copy_back = [".t/sessions"]
"#,
        );
        let (ok, bad) = scratch(Some(&p));
        assert!(ok.contains(&".t/cache".to_string()));
        assert_eq!(ok.iter().filter(|s| *s == ".cache/fontconfig").count(), 1);
        assert_eq!(bad, vec![".t", "../x", "/abs", ".t/*", ".t/sessions/tmp"]);
        let codex = crate::profile::builtin("codex").unwrap();
        assert!(scratch(Some(&codex))
            .0
            .contains(&".codex/cache".to_string()));
    }

    #[test]
    fn the_user_configuration_turns_tmpfs_caches_off() {
        assert!(enabled_in(""));
        assert!(enabled_in("[storage]\ndedupe = false\n"));
        assert!(!enabled_in("[storage]\ntmpfs_caches = false\n"));
    }

    #[test]
    fn expand_matches_real_folders_and_never_a_symlink() {
        let t = tempfile::tempdir().unwrap();
        let h = t.path();
        for d in [
            ".config/App/Cache",
            ".config/App/Local Storage",
            ".config/Other/GPUCache",
            ".npm/_cacache",
            "outside/Cache",
        ] {
            std::fs::create_dir_all(h.join(d)).unwrap();
        }
        std::os::unix::fs::symlink(h.join("outside"), h.join(".config/Linked")).unwrap();
        assert_eq!(
            expand(h, ".config/*/Cache"),
            vec![PathBuf::from(".config/App/Cache")]
        );
        let found = find(h, &scratch(None).0);
        assert!(found.contains(&(Class::Scratch, ".config/Other/GPUCache".into())));
        assert!(found.contains(&(Class::Shared, ".npm".into())));
        assert!(!found.iter().any(|(_, p)| p.ends_with("Local Storage")));
    }
}
