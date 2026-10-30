//! Project detectors. Each one looks at the project and adds what it needs to
//! the spec: mounts, caches, environment, setup lines. They do not depend on
//! each other, so each can be tested alone.

use crate::env::{capture, HostEnv};
use crate::manifest::Manifest;
use crate::spec::{Mount, SandboxSpec};
use anyhow::{Context as _, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct Context<'a> {
    pub env: &'a HostEnv,
    pub project: &'a Path,
    pub manifest: &'a Manifest,
}

pub trait Detector {
    fn name(&self) -> &'static str;
    fn apply(&self, cx: &Context, spec: &mut SandboxSpec) -> Result<()>;
}

pub fn all() -> Vec<Box<dyn Detector>> {
    vec![
        Box::new(Git),
        Box::new(Mise),
        Box::new(Cache::rust()),
        Box::new(Cache::node()),
        Box::new(Cache::python()),
        Box::new(Cache::go()),
        Box::new(Secrets),
        Box::new(AutoRun),
    ]
}

/// Bind a shared cache directory, kept under the paddock cache, over `dst`.
pub fn cache_mount(env: &HostEnv, name: &str, dst: PathBuf, origin: &str) -> Result<Mount> {
    let src = env.paddock_cache().join("shared").join(name);
    std::fs::create_dir_all(&src)?;
    Ok(Mount::bind(src, dst, true, origin))
}

/// The git root of a path, if the path is inside a work tree.
pub fn git_toplevel(dir: &Path) -> Option<PathBuf> {
    capture(
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["rev-parse", "--show-toplevel"]),
    )
    .ok()
    .map(PathBuf::from)
}

pub struct Git;

impl Git {
    fn dirs(project: &Path) -> Option<(PathBuf, PathBuf)> {
        let out = capture(Command::new("git").arg("-C").arg(project).args([
            "rev-parse",
            "--path-format=absolute",
            "--git-dir",
            "--git-common-dir",
        ]))
        .ok()?;
        let mut lines = out.lines();
        Some((PathBuf::from(lines.next()?), PathBuf::from(lines.next()?)))
    }

    /// Every git directory whose hooks and config the host runs: the main one,
    /// each worktree, and each submodule.
    pub fn protected_dirs(git_dir: &Path, common: &Path) -> Vec<PathBuf> {
        let mut out = vec![common.to_path_buf()];
        if git_dir != common {
            out.push(git_dir.to_path_buf());
        }
        for sub in ["worktrees", "modules"] {
            collect_git_dirs(&common.join(sub), 0, &mut out);
        }
        out.sort();
        out.dedup();
        out
    }
}

fn collect_git_dirs(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > 4 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if !p.is_dir() {
            continue;
        }
        if p.join("HEAD").is_file() {
            out.push(p.clone());
        }
        collect_git_dirs(&p.join("modules"), depth + 1, out);
        if !p.join("HEAD").is_file() {
            collect_git_dirs(&p, depth + 1, out);
        }
    }
}

impl Detector for Git {
    fn name(&self) -> &'static str {
        "git"
    }
    fn apply(&self, cx: &Context, spec: &mut SandboxSpec) -> Result<()> {
        let Some((git_dir, common)) = Self::dirs(cx.project) else {
            return Ok(());
        };
        if !common.starts_with(cx.project) {
            // A linked worktree: commits write to the main repository's git directory.
            spec.add_mount(Mount::same(
                &common,
                true,
                "detector git: common dir of a linked worktree",
            ));
        }
        for dir in Self::protected_dirs(&git_dir, &common) {
            let hooks = dir.join("hooks");
            if !hooks.exists() {
                std::fs::create_dir_all(&hooks)?;
            }
            spec.add_mount(Mount::same(
                &hooks,
                false,
                "detector git: hooks run on the host",
            ));
            for f in ["config", "config.worktree"] {
                let p = dir.join(f);
                if p.is_file() {
                    spec.add_mount(Mount::same(
                        &p,
                        false,
                        "detector git: config can run commands on the host",
                    ));
                }
            }
        }
        Ok(())
    }
}

pub struct Mise;

pub const MISE_FILES: &[&str] = &[
    "mise.toml",
    ".mise.toml",
    "mise.local.toml",
    ".mise.local.toml",
    ".tool-versions",
    ".config/mise.toml",
    ".config/mise/config.toml",
    ".mise/config.toml",
];

impl Detector for Mise {
    fn name(&self) -> &'static str {
        "mise"
    }
    fn apply(&self, cx: &Context, spec: &mut SandboxSpec) -> Result<()> {
        if !MISE_FILES.iter().any(|f| cx.project.join(f).is_file()) {
            return Ok(());
        }
        let origin = "detector mise";
        // The mise data directory belongs to this sandbox home: tools that only
        // this project uses install there. The shared installs hold registry
        // tools for every project. They are read-only in every sandbox. Only a
        // clean install step without the project writes them (mise.rs).
        let data = spec.home.join(".local/share/mise");
        let shared = spec.home.join(crate::spec::MISE_SHARED_INSTALLS);
        let mut m = cache_mount(
            cx.env,
            crate::mise::SHARED_CACHE,
            shared.clone(),
            "detector mise: shared installs, read-only",
        )?;
        m.kind = crate::spec::MountKind::Ro;
        spec.add_mount(m);
        spec.set_env("MISE_DATA_DIR", data.display().to_string(), origin);
        spec.set_env(
            "MISE_SYSTEM_INSTALLS_DIR",
            shared.display().to_string(),
            origin,
        );
        // Shims for the shared tools, and for the lazy tools of the host's
        // /etc/mise config, go to their own private directory. It comes after
        // paddock's host tools on PATH, so a `gh` shim cannot hide paddock's `gh`.
        let system_shims = data.join("system-shims");
        spec.set_env(
            "MISE_SYSTEM_SHIMS_DIR",
            system_shims.display().to_string(),
            origin,
        );
        spec.tool_path.push(system_shims);
        spec.set_env(
            "MISE_TRUSTED_CONFIG_PATHS",
            cx.project.display().to_string(),
            origin,
        );
        spec.set_env("MISE_YES", "1", origin);
        spec.add_path(data.join("shims"));
        let exe = cx
            .env
            .which("mise")
            .and_then(|p| std::fs::canonicalize(p).ok());
        match &exe {
            Some(exe) => spec.add_tool("mise", exe.clone(), origin),
            None => spec
                .warnings
                .push("the project has a mise file, but mise is not installed on the host".into()),
        }
        if cx.manifest.setup.mise_install.unwrap_or(true) {
            if let Some(exe) = &exe {
                let registry = crate::mise::Registry::load(exe);
                spec.mise_shared = crate::mise::shared_installs(cx.project, &registry);
            }
            spec.provision.push("mise reshim --system".into());
            spec.provision.push("mise install".into());
        }
        Ok(())
    }
}

/// A language package cache: shared across projects, never the host cache.
pub struct Cache {
    name: &'static str,
    markers: &'static [&'static str],
    caches: &'static [(&'static str, &'static str)],
}

impl Cache {
    fn rust() -> Self {
        Self {
            name: "rust",
            markers: &["Cargo.toml"],
            caches: &[
                ("cargo-registry", ".cargo/registry"),
                ("cargo-git", ".cargo/git"),
            ],
        }
    }
    fn node() -> Self {
        Self {
            name: "node",
            markers: &["package.json"],
            caches: &[
                ("npm", ".npm"),
                ("pnpm-store", ".local/share/pnpm/store"),
                ("bun", ".bun/install/cache"),
                ("yarn", ".cache/yarn"),
            ],
        }
    }
    fn python() -> Self {
        Self {
            name: "python",
            markers: &["pyproject.toml", "requirements.txt", "uv.lock", "setup.py"],
            caches: &[("uv", ".cache/uv"), ("pip", ".cache/pip")],
        }
    }
    fn go() -> Self {
        Self {
            name: "go",
            markers: &["go.mod"],
            caches: &[("go-mod", "go/pkg/mod"), ("go-build", ".cache/go-build")],
        }
    }
}

impl Detector for Cache {
    fn name(&self) -> &'static str {
        self.name
    }
    fn apply(&self, cx: &Context, spec: &mut SandboxSpec) -> Result<()> {
        if !self.markers.iter().any(|m| cx.project.join(m).is_file()) {
            return Ok(());
        }
        let origin = format!("detector {}: package cache", self.name);
        for (cache, rel) in self.caches {
            spec.add_mount(cache_mount(cx.env, cache, spec.home.join(rel), &origin)?);
        }
        Ok(())
    }
}

/// Hide credential-shaped files in the project.
pub struct Secrets;

const SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    ".venv",
    "venv",
    "dist",
    "build",
    "vendor",
    ".direnv",
];
const SAFE_SUFFIXES: &[&str] = &[".example", ".sample", ".template", ".dist", ".defaults"];

pub fn is_secret_name(name: &str) -> bool {
    if SAFE_SUFFIXES.iter().any(|s| name.ends_with(s)) {
        return false;
    }
    name == ".env"
        || name.starts_with(".env.")
        || name == ".netrc"
        || name == ".git-credentials"
        || name.starts_with("id_rsa")
        || name.starts_with("id_ed25519")
        || name.starts_with("id_ecdsa")
        || [".pem", ".key", ".p12", ".pfx", ".keystore", ".jks"]
            .iter()
            .any(|e| name.ends_with(e))
}

fn walk_secrets(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > 3 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            if !SKIP_DIRS.contains(&name.as_str()) {
                walk_secrets(&e.path(), depth + 1, out);
            }
        } else if ft.is_file() && is_secret_name(&name) {
            out.push(e.path());
        }
    }
}

impl Detector for Secrets {
    fn name(&self) -> &'static str {
        "secrets"
    }
    fn apply(&self, cx: &Context, spec: &mut SandboxSpec) -> Result<()> {
        let reveal: Vec<PathBuf> = cx
            .manifest
            .secrets
            .reveal
            .iter()
            .map(|r| cx.env.expand(r, cx.project))
            .collect();
        let mut found = Vec::new();
        walk_secrets(cx.project, 0, &mut found);
        for f in found {
            if reveal.contains(&f) {
                continue;
            }
            spec.add_mount(Mount::mask(
                &f,
                false,
                "detector secrets: credential-shaped file",
            ));
        }
        for h in &cx.manifest.secrets.hide {
            let p = cx.env.expand(h, cx.project);
            if std::fs::symlink_metadata(&p).is_err() {
                continue;
            }
            // A symlink in a sandbox-writable folder could move the mask
            // away from the secret, so it is refused.
            let writable = crate::build::sandbox_writable(cx.env, spec);
            let real =
                crate::safefs::resolve_approved(&p, &writable).context("manifest secrets.hide")?;
            spec.add_mount(Mount::mask(&real, real.is_dir(), "manifest secrets.hide"));
        }
        Ok(())
    }
}

/// Files that host tools run without being asked. Paddock reports changes to them.
pub const AUTORUN_FILES: &[&str] = &[
    ".envrc",
    "mise.toml",
    ".mise.toml",
    "mise.local.toml",
    ".mise.local.toml",
    ".config/mise.toml",
    ".vscode/tasks.json",
    ".vscode/settings.json",
    ".vscode/launch.json",
    ".devcontainer/devcontainer.json",
    ".pre-commit-config.yaml",
    "paddock.toml",
];

pub struct AutoRun;

impl Detector for AutoRun {
    fn name(&self) -> &'static str {
        "autorun"
    }
    fn apply(&self, cx: &Context, spec: &mut SandboxSpec) -> Result<()> {
        spec.watch = AUTORUN_FILES.iter().map(|f| cx.project.join(f)).collect();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_names() {
        for n in [
            ".env",
            ".env.local",
            "server.pem",
            "tls.key",
            "id_ed25519",
            ".netrc",
        ] {
            assert!(is_secret_name(n), "{n}");
        }
        for n in [
            ".env.example",
            "README.md",
            "keyboard.rs",
            "env.ts",
            ".envrc",
        ] {
            assert!(!is_secret_name(n), "{n}");
        }
    }
}
