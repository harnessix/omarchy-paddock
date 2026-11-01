//! Project detectors. Each one looks at the project and adds what it needs to
//! the spec: mounts, caches, environment, setup lines. They do not depend on
//! each other, so each can be tested alone.

use crate::env::HostEnv;
use crate::manifest::Manifest;
use crate::spec::{Mount, SandboxSpec};
use anyhow::{bail, Context as _, Result};
use std::path::{Path, PathBuf};

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
        Box::new(Rustup),
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
    crate::privfs::dir(&src)?;
    Ok(Mount::bind(src, dst, true, origin))
}

/// The git root of a path, if the path is inside a work tree. It comes from
/// the file system, never from configuration that a sandbox can write.
pub fn git_toplevel(dir: &Path) -> Option<PathBuf> {
    crate::hostgit::Layout::toplevel(dir)
}

pub struct Git;

impl Git {
    /// Make a file of a git directory exist, so a read-only bind can cover
    /// it. Without the file the sandbox can create it later and point git at
    /// its own content. The git directory is sandbox-writable, so the path is
    /// walked without following a symlink, and an existing file is never
    /// replaced. A symlink or a special file in its place is an error: a bind
    /// of it would follow it on the host.
    fn placeholder(git_dir: &Path, rel: &str, content: &str) -> Result<()> {
        use crate::safefs::Kind;
        let root = crate::safefs::Root::open(git_dir)?;
        let (parent, name) = match rel.rsplit_once('/') {
            Some((p, n)) => (Path::new(p), n),
            None => (Path::new(""), rel),
        };
        let dir = root.dir(parent, true)?;
        match dir.kind(name.as_ref())? {
            Some(Kind::File) => Ok(()),
            Some(other) => bail!(
                "{} is a {:?}, not a file. paddock does not bind it, because git on the host reads it.",
                git_dir.join(rel).display(),
                other
            ),
            None => dir.write(name.as_ref(), content.as_bytes(), 0o644),
        }
    }
}

impl Detector for Git {
    fn name(&self) -> &'static str {
        "git"
    }
    fn apply(&self, cx: &Context, spec: &mut SandboxSpec) -> Result<()> {
        let Some(layout) = crate::hostgit::Layout::discover(cx.project)? else {
            return Ok(());
        };
        if !layout.common_dir.starts_with(cx.project) {
            // A linked worktree: commits write to the main repository's git directory.
            spec.add_mount(Mount::same(
                &layout.common_dir,
                true,
                "detector git: common dir of a linked worktree",
            ));
        }
        // `.git` gets a mount point of its own. A mount point cannot be
        // renamed or replaced in the sandbox, so the read-only files below it
        // cannot be moved aside with `mv .git .git2 && mkdir .git`.
        if layout.linked {
            spec.add_mount(Mount::same(
                &layout.dot_git,
                false,
                "detector git: the .git file names the git directory",
            ));
        } else {
            spec.add_mount(Mount::same(
                &layout.dot_git,
                true,
                "detector git: the git directory is its own mount point",
            ));
        }
        for dir in layout.protected_dirs() {
            if dir != layout.dot_git && dir.starts_with(cx.project) {
                // A worktree or submodule git directory inside the project
                // gets its own mount point as well.
                spec.add_mount(Mount::same(
                    &dir,
                    true,
                    "detector git: a git directory is its own mount point",
                ));
            }
            // The hooks folder: created without following a symlink, so a
            // read-only bind of it cannot land anywhere else.
            let hooks = dir.join("hooks");
            crate::safefs::Root::open(&dir)?.dir(Path::new("hooks"), true)?;
            spec.add_mount(Mount::same(
                &hooks,
                false,
                "detector git: hooks run on the host",
            ));
            let linked = dir.join("commondir").is_file();
            for f in crate::hostgit::PROTECTED_FILES {
                let p = dir.join(f);
                // `gitdir` exists only in a registered worktree. Never write
                // one: its content is a path paddock must not invent.
                if *f == "gitdir" {
                    if p.is_file() {
                        spec.add_mount(Mount::same(
                            &p,
                            false,
                            "detector git: gitdir names the work tree",
                        ));
                    }
                    continue;
                }
                // "." means "this directory", which is what a git directory
                // without a common directory of its own needs.
                let content = if *f == "commondir" && !linked {
                    ".\n"
                } else {
                    ""
                };
                Self::placeholder(&dir, f, content)?;
                spec.add_mount(Mount::same(
                    &p,
                    false,
                    "detector git: config and pointer files steer host git",
                ));
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

/// The host's rustup toolchains, for a Rust project: `~/.rustup` and the
/// proxies in `~/.cargo/bin`, read-only. Not the rest of `~/.cargo`, which
/// holds `credentials.toml`. Read-only, so a project's `rust-toolchain.toml`
/// cannot install or change the toolchains that every project shares.
pub struct Rustup;

impl Detector for Rustup {
    fn name(&self) -> &'static str {
        "rustup"
    }
    fn apply(&self, cx: &Context, spec: &mut SandboxSpec) -> Result<()> {
        if !cx.project.join("Cargo.toml").is_file() {
            return Ok(());
        }
        let (rustup, bin) = (cx.env.home.join(".rustup"), cx.env.home.join(".cargo/bin"));
        if !rustup.join("toolchains").is_dir() || !bin.join("cargo").is_file() {
            return Ok(());
        }
        let origin = "detector rustup: toolchains (read-only)";
        spec.add_mount(Mount::bind(
            &rustup,
            spec.home.join(".rustup"),
            false,
            origin,
        ));
        spec.add_mount(Mount::bind(
            &bin,
            spec.home.join(".cargo/bin"),
            false,
            origin,
        ));
        spec.set_env(
            "RUSTUP_HOME",
            spec.home.join(".rustup").display().to_string(),
            origin,
        );
        spec.tool_path.push(spec.home.join(".cargo/bin"));
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

/// Hide the credential-shaped files in a host folder that the sandbox sees,
/// except the ones in `reveal`. Returns how many it hid.
pub fn hide_secrets_in(
    dir: &Path,
    reveal: &[PathBuf],
    spec: &mut SandboxSpec,
    origin: &str,
) -> usize {
    let mut found = Vec::new();
    walk_secrets(dir, 0, &mut found);
    let mut n = 0;
    for f in found {
        if reveal.contains(&f) || spec.mounts.iter().any(|m| m.dst == f) {
            continue;
        }
        spec.add_mount(Mount::mask(&f, false, origin));
        n += 1;
    }
    n
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
        hide_secrets_in(
            cx.project,
            &reveal,
            spec,
            "detector secrets: credential-shaped file",
        );
        // Folders from `[mounts]` get the same treatment: the approval was
        // for the folder, not for the keys in it.
        let extra: Vec<PathBuf> = spec
            .mounts
            .iter()
            .filter(|m| m.origin.starts_with("manifest mounts"))
            .filter_map(|m| m.src.clone())
            .filter(|p| p.is_dir() && !p.starts_with(cx.project))
            .collect();
        for dir in extra {
            hide_secrets_in(
                &dir,
                &reveal,
                spec,
                "detector secrets: credential-shaped file in a [mounts] folder",
            );
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
    use crate::spec::MountKind;

    fn run_git(dir: &Path, args: &[&str]) {
        let ok = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@localhost"])
            .args(args)
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    }

    fn git_mounts(project: &Path) -> SandboxSpec {
        let env = HostEnv::from_parts(
            project.join("fake-home"),
            "t".into(),
            1000,
            1000,
            Default::default(),
        );
        let mut spec = SandboxSpec::new(
            "t".into(),
            "x",
            project,
            env.home.join("sandbox"),
            &env.home,
            1000,
            1000,
            "t",
        );
        let manifest = crate::manifest::Manifest::default();
        let cx = Context {
            env: &env,
            project,
            manifest: &manifest,
        };
        Git.apply(&cx, &mut spec).unwrap();
        spec
    }

    #[test]
    fn the_git_dir_and_its_pointer_files_are_mounted() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("repo");
        std::fs::create_dir_all(&project).unwrap();
        run_git(&project, &["init", "-q", "-b", "main"]);
        run_git(&project, &["commit", "-q", "--allow-empty", "-m", "one"]);
        let spec = git_mounts(&project);
        let kind = |p: &Path| spec.mounts.iter().find(|m| m.dst == p).map(|m| m.kind);
        let dot = project.join(".git");
        // Its own mount point, so it cannot be renamed in the sandbox.
        assert_eq!(kind(&dot), Some(MountKind::Rw));
        for f in ["hooks", "config", "config.worktree", "commondir"] {
            assert_eq!(kind(&dot.join(f)), Some(MountKind::Ro), "{f}");
        }
        assert_eq!(
            kind(&dot.join("objects/info/alternates")),
            Some(MountKind::Ro)
        );
        // The pointer files that paddock created are harmless to git.
        assert_eq!(
            std::fs::read_to_string(dot.join("commondir"))
                .unwrap()
                .trim(),
            "."
        );
        assert_eq!(
            std::fs::read_to_string(dot.join("config.worktree")).unwrap(),
            ""
        );
    }

    #[test]
    fn a_linked_worktree_keeps_its_dot_git_file_read_only() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("repo");
        std::fs::create_dir_all(&project).unwrap();
        run_git(&project, &["init", "-q", "-b", "main"]);
        run_git(&project, &["commit", "-q", "--allow-empty", "-m", "one"]);
        let wt = tmp.path().join("wt");
        run_git(&project, &["worktree", "add", "-q", wt.to_str().unwrap()]);
        let spec = git_mounts(&wt);
        let kind = |p: &Path| spec.mounts.iter().find(|m| m.dst == p).map(|m| m.kind);
        // The `.git` file names the git directory: read-only.
        assert_eq!(kind(&wt.join(".git")), Some(MountKind::Ro));
        // The repository's git directory is writable, so commits work.
        let common = project.join(".git");
        assert_eq!(kind(&common), Some(MountKind::Rw));
        assert_eq!(kind(&common.join("config")), Some(MountKind::Ro));
        // And the worktree's own pointer files in the repository.
        let wt_dir = common.join("worktrees");
        let entry = std::fs::read_dir(&wt_dir).unwrap().next().unwrap().unwrap();
        for f in ["commondir", "gitdir"] {
            assert_eq!(kind(&entry.path().join(f)), Some(MountKind::Ro), "{f}");
        }
    }

    #[test]
    fn secrets_are_hidden_in_extra_mounts_too() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("proj");
        let data = tmp.path().join("data");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(data.join("sub")).unwrap();
        std::fs::write(project.join(".env"), "x").unwrap();
        std::fs::write(data.join(".env"), "x").unwrap();
        std::fs::write(data.join("sub/tls.key"), "x").unwrap();
        std::fs::write(data.join("notes.txt"), "x").unwrap();
        let env = HostEnv::from_parts(
            tmp.path().join("home"),
            "t".into(),
            1000,
            1000,
            Default::default(),
        );
        let mut spec = SandboxSpec::new(
            "t".into(),
            "x",
            &project,
            env.home.join("sandbox"),
            &env.home,
            1000,
            1000,
            "t",
        );
        spec.add_mount(Mount::same(&data, false, "manifest mounts.ro"));
        let mut manifest = crate::manifest::Manifest::default();
        manifest
            .secrets
            .reveal
            .push(data.join("sub/tls.key").display().to_string());
        let cx = Context {
            env: &env,
            project: &project,
            manifest: &manifest,
        };
        Secrets.apply(&cx, &mut spec).unwrap();
        let kind = |p: &Path| spec.mounts.iter().find(|m| m.dst == p).map(|m| m.kind);
        assert_eq!(kind(&project.join(".env")), Some(MountKind::MaskFile));
        assert_eq!(kind(&data.join(".env")), Some(MountKind::MaskFile));
        assert_eq!(kind(&data.join("sub/tls.key")), None);
        assert_eq!(kind(&data.join("notes.txt")), None);
    }

    #[test]
    fn a_rust_project_gets_the_rustup_toolchains_read_only() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let env = HostEnv::from_parts(
            tmp.path().join("home"),
            "t".into(),
            1000,
            1000,
            Default::default(),
        );
        let spec_for = |env: &HostEnv| {
            let mut spec = SandboxSpec::new(
                "t".into(),
                "x",
                &project,
                env.home.join("sandbox"),
                &env.home,
                1000,
                1000,
                "t",
            );
            let manifest = crate::manifest::Manifest::default();
            let cx = Context {
                env,
                project: &project,
                manifest: &manifest,
            };
            Rustup.apply(&cx, &mut spec).unwrap();
            spec
        };
        // No Cargo.toml, or no rustup on the host: nothing.
        assert!(spec_for(&env).mounts.is_empty());
        std::fs::write(project.join("Cargo.toml"), "").unwrap();
        assert!(spec_for(&env).mounts.is_empty());
        std::fs::create_dir_all(env.home.join(".rustup/toolchains/stable")).unwrap();
        std::fs::create_dir_all(env.home.join(".cargo/bin")).unwrap();
        std::fs::write(env.home.join(".cargo/bin/cargo"), "").unwrap();
        std::fs::write(env.home.join(".cargo/credentials.toml"), "token").unwrap();
        let spec = spec_for(&env);
        let home = &spec.home;
        let kind = |p: &Path| spec.mounts.iter().find(|m| m.dst == p).map(|m| m.kind);
        assert_eq!(kind(&home.join(".rustup")), Some(MountKind::Ro));
        assert_eq!(kind(&home.join(".cargo/bin")), Some(MountKind::Ro));
        // Never the credentials beside them.
        assert!(spec.mounts.iter().all(|m| m.dst != home.join(".cargo")));
        assert!(spec.path_value().contains(".cargo/bin"));
        // After paddock's own tools: a cargo-installed `gh` cannot hide paddock's.
        let path = spec.path_value();
        assert!(
            path.find(crate::spec::TOOL_DIR) < path.find(".cargo/bin"),
            "{path}"
        );
    }

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
