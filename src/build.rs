//! Assemble a `SandboxSpec` from the harness profile, the project detectors,
//! the manifest and the command line, then run the policy engine on it.

use crate::detect::{self, git_toplevel, Context};
use crate::env::HostEnv;
use crate::gui;
use crate::manifest::{self, Manifest};
use crate::policy;
use crate::profile::{Harness, HarnessFactory, Profile};
use crate::spec::{GuiMode, Mount, Network, SandboxSpec};
use crate::state;
use anyhow::{bail, Context as _, Result};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default)]
pub struct Options {
    pub harness: String,
    pub project: Option<PathBuf>,
    pub backend: Option<String>,
    pub network: Option<Network>,
    pub gui: Option<GuiMode>,
    pub gpu: bool,
    pub no_yolo: bool,
    /// Pass the host environment. None means the manifest decides (default true).
    pub inherit_env: Option<bool>,
    /// shared, isolated, or isolated:NAME. None means the manifest decides.
    pub auth: Option<String>,
    pub shell: bool,
    pub assume_yes: bool,
    pub args: Vec<String>,
    /// Work in a new git worktree on its own branch.
    pub worktree: bool,
    /// Extra domains for network mode `allowlist`.
    pub allow_domains: Vec<String>,
    /// App mode: the session bus, instead of `[apps.NAME] bus`.
    pub app_bus: Option<crate::manifest::AppBus>,
}

/// A worktree that paddock created for one run.
#[derive(Debug, Clone)]
pub struct Worktree {
    pub repo: PathBuf,
    pub path: PathBuf,
    pub branch: String,
    pub base: String,
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    crate::env::capture(
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args),
    )
}

pub fn create_worktree(env: &HostEnv, repo: &Path) -> Result<Worktree> {
    if git(repo, &["rev-parse", "--show-toplevel"])
        .map(PathBuf::from)
        .ok()
        .as_deref()
        != Some(repo)
    {
        bail!(
            "--worktree needs a git repository: {} is not one",
            repo.display()
        );
    }
    let base = git(repo, &["rev-parse", "HEAD"]).context("--worktree needs at least one commit")?;
    let id = state::new_id("wt", repo);
    let branch = format!("paddock/{id}");
    let path = env
        .paddock_data()
        .join("worktrees")
        .join(state::slug(repo))
        .join(&id);
    std::fs::create_dir_all(path.parent().unwrap())?;
    git(
        repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            &branch,
            &path.display().to_string(),
            &base,
        ],
    )?;
    Ok(Worktree {
        repo: repo.to_path_buf(),
        path,
        branch,
        base,
    })
}

pub struct Built {
    /// Who answers host passthrough requests. None means the user, through
    /// the panel, a notification or `paddock approvals`.
    pub approver: Option<std::sync::Arc<dyn crate::broker::Approver>>,
    pub worktree: Option<Worktree>,
    pub auth: crate::state::AuthMode,
    pub spec: SandboxSpec,
    pub profile: Profile,
    pub harness_exe: PathBuf,
    pub manifest: Manifest,
    pub backend: String,
}

pub fn resolve_project(path: Option<&Path>) -> Result<PathBuf> {
    let raw = match path {
        Some(p) => p.to_path_buf(),
        None => std::env::current_dir()?,
    };
    let dir = std::fs::canonicalize(&raw)
        .with_context(|| format!("project directory {} does not exist", raw.display()))?;
    if !dir.is_dir() {
        bail!("{} is not a directory", dir.display());
    }
    Ok(git_toplevel(&dir).unwrap_or(dir))
}

/// Build a spec for a located harness.
pub fn build(env: &HostEnv, opts: &Options) -> Result<Built> {
    let factory = HarnessFactory::new(env)?;
    let harness = factory.create(&opts.harness)?;
    build_with(env, opts, harness)
}

pub fn build_with(env: &HostEnv, opts: &Options, harness: Harness) -> Result<Built> {
    let project = resolve_project(opts.project.as_deref())?;
    if project == env.home || env.home.starts_with(&project) {
        bail!(
            "refusing to use {} as the project: it is the home directory or contains it",
            project.display()
        );
    }
    let loaded = Manifest::load(env, &project)?;
    manifest::require_approval(env, &loaded, opts.assume_yes)?;
    let m = loaded.merged.clone();
    let profile = harness.profile.clone();
    // The settings and the sandbox home belong to the repository; the agent
    // works in the worktree.
    let home_key = project.clone();
    let worktree = if opts.worktree {
        Some(create_worktree(env, &project)?)
    } else {
        None
    };
    let project = worktree.as_ref().map(|w| w.path.clone()).unwrap_or(project);

    let auth: state::AuthMode = opts
        .auth
        .clone()
        .or(m.auth.mode.clone())
        .unwrap_or_else(|| "shared".into())
        .parse()?;
    let id = state::new_id(&profile.name, &project);
    let home_src = state::sandbox_home(env, &profile.name, &auth, &home_key)?;
    let mut spec = SandboxSpec::new(
        id,
        &profile.name,
        &project,
        home_src,
        &env.home,
        env.uid,
        env.gid,
        &env.user,
    );
    spec.network = opts.network.or(m.network.mode).unwrap_or_default();
    spec.publish = m.network.publish.clone();

    spec.add_mount(Mount::same(&project, true, "project"));

    // The harness.
    let porigin = format!("profile {}", profile.name);
    for p in &harness.install_mounts {
        spec.add_mount(Mount::same(p, false, &format!("{porigin}: install")));
    }
    for d in &harness.path_dirs {
        spec.add_path(d.clone());
    }
    for (k, v) in &profile.launch.env {
        spec.set_env(k, v.clone(), &porigin);
    }
    // The host environment, first, so profile, manifest and detectors override it.
    let inherit = opts.inherit_env.or(m.env.inherit).unwrap_or(true);
    if inherit {
        let mut secret_names = Vec::new();
        let mut dropped = Vec::new();
        let mut count = 0;
        for (k, v) in &env.vars {
            if !policy::inheritable_var(k, v) || k.starts_with("LC_") || m.env.hide.contains(k) {
                continue;
            }
            // Named in [env] pass or env_pass: those loops below add it.
            if policy::is_cloud_secret(k) {
                if !m.env.pass.contains(k) && !profile.state.env_pass.contains(k) {
                    dropped.push(k.clone());
                }
                continue;
            }
            count += 1;
            if policy::looks_secret(k) {
                secret_names.push(k.clone());
                spec.set_secret_env(k, v.clone(), "host env (inherited)");
            } else {
                spec.set_env(k, v.clone(), "host env (inherited)");
            }
        }
        for d in &env.path {
            spec.inherit_path.push(d.clone());
        }
        // Tell the user once per project, and again when the list of
        // secret-looking names changes.
        let mut notice_key = secret_names.clone();
        notice_key.extend(dropped.iter().map(|d| format!("dropped:{d}")));
        if crate::state::env_notice_changed(env, &project, &notice_key) {
            let mut note = format!("passing {count} host environment variables");
            if !secret_names.is_empty() {
                note.push_str(&format!(
                    ". The agent can read these, which look like secrets: {}",
                    secret_names.join(", ")
                ));
            }
            if !dropped.is_empty() {
                note.push_str(&format!(
                    ". Cloud credentials were not passed: {}. To pass one, add it to [env] pass",
                    dropped.join(", ")
                ));
            }
            note.push_str(
                ". To drop one: paddock env hide NAME. To pass only the allowlist: --env clean",
            );
            spec.notices.push(note);
        }
    }
    // The host proxy, in every env mode: sandbox traffic must leave the way host
    // traffic does. A proxy on the host loopback is the one loopback port the
    // sandbox can reach.
    if spec.network != Network::None {
        for k in crate::proxy::PROXY_VARS {
            if let Some(v) = env.var(k).filter(|v| !v.is_empty()) {
                spec.set_secret_env(k, v, "network: host proxy");
                if let Some((_, host, port)) = crate::proxy::parse_url(v) {
                    if crate::proxy::is_loopback(&host)
                        && !k.to_ascii_uppercase().starts_with("NO_")
                    {
                        spec.add_host_port(port, &format!("network: host proxy in {k}"));
                    }
                }
            }
        }
    }
    for k in &profile.state.env_pass {
        if let Some(v) = env.var(k) {
            spec.set_secret_env(k, v, &format!("{porigin} env_pass"));
        }
    }
    for rel in &profile.state.import_ro {
        let host = env.home.join(rel);
        if host.exists() {
            spec.add_mount(Mount::same(&host, false, &format!("{porigin} import_ro")));
        }
    }

    // The manifest.
    for raw in &m.mounts.rw {
        let p = env.expand(raw, &project);
        if !p.exists() {
            bail!("manifest mounts.rw: {} does not exist", p.display());
        }
        spec.add_mount(Mount::same(
            std::fs::canonicalize(&p)?,
            true,
            "manifest mounts.rw",
        ));
    }
    for raw in &m.mounts.ro {
        let p = env.expand(raw, &project);
        if !p.exists() {
            bail!("manifest mounts.ro: {} does not exist", p.display());
        }
        spec.add_mount(Mount::same(
            std::fs::canonicalize(&p)?,
            false,
            "manifest mounts.ro",
        ));
    }
    for k in &m.env.pass {
        if let Some(v) = env.var(k) {
            spec.set_secret_env(k, v, "manifest env.pass");
        }
    }
    for (k, v) in &m.env.set {
        spec.set_env(k, v.clone(), "manifest env.set");
    }

    // The detectors.
    let cx = Context {
        env,
        project: &project,
        manifest: &m,
    };
    for d in detect::all() {
        d.apply(&cx, &mut spec)
            .with_context(|| format!("detector {}", d.name()))?;
    }
    spec.setup.extend(m.setup.run.iter().cloned());
    spec.lan = m.network.lan == Some(true);
    if spec.network != Network::None {
        for p in &m.network.host_ports {
            spec.add_host_port(*p, "manifest network.host_ports");
        }
    }
    crate::mcp::apply(env, &profile, &m, &project, &mut spec);
    if spec.network == Network::Allowlist {
        let mut d: Vec<String> = profile.network.endpoints.clone();
        d.extend(m.network.allow.iter().cloned());
        d.extend(opts.allow_domains.iter().cloned());
        d.sort();
        d.dedup();
        spec.allow_domains = d;
    }

    // The command.
    spec.command = if opts.shell {
        let sh = if env.which("bash").is_some() {
            "bash"
        } else {
            "sh"
        };
        vec![sh.to_string()]
    } else {
        let mut c = vec![harness.exe.display().to_string()];
        if !opts.no_yolo {
            c.extend(profile.launch.yolo_args.iter().cloned());
        }
        c.extend(profile.launch.args.iter().cloned());
        c.extend(opts.args.iter().cloned());
        c
    };

    // The display: command line, then manifest, then the desktop detector.
    let marker = gui::desktop_marker(&project);
    spec.gui.mode = match (opts.gui, m.gui.mode, &marker) {
        (Some(g), _, _) => {
            spec.gui.origin = "command line".into();
            g
        }
        (None, Some(g), _) => {
            spec.gui.origin = "manifest gui.mode".into();
            g
        }
        (None, None, Some(found)) => {
            spec.gui.origin = format!("detector desktop: {found}");
            GuiMode::Headless
        }
        _ => GuiMode::Off,
    };
    if spec.gui.mode == GuiMode::Host
        && opts.gui != Some(GuiMode::Host)
        && m.gui.mode != Some(GuiMode::Host)
    {
        bail!("gui mode 'host' must be requested explicitly");
    }
    spec.gui.gpu = opts.gpu || m.gui.gpu.unwrap_or(false);
    if let Some(w) = m.gui.width {
        spec.gui.width = w;
    }
    if let Some(h) = m.gui.height {
        spec.gui.height = h;
    }
    if spec.gui.enabled() {
        for rel in [
            ".local/share/fonts",
            ".fonts",
            ".config/fontconfig",
            ".icons",
            ".local/share/icons",
            ".config/gtk-3.0",
            ".config/gtk-4.0",
        ] {
            let host = env.home.join(rel);
            if host.exists() {
                spec.add_mount(Mount::same(&host, false, "gui: fonts and themes"));
            }
        }
    }
    // The keyring wrapper goes inside the display's private bus, so it is
    // applied first: gui::apply wraps whatever command it finds.
    // In app mode it decides between the private bus with a keyring and the
    // filtered host bus.
    if profile.launch.keyring || spec.gui.mode == GuiMode::App {
        let bus = match opts.app_bus {
            Some(b) => b,
            None => crate::app::bus_mode(env, &spec.harness)?,
        };
        crate::keyring::apply(env, &mut spec, bus)?;
    }
    gui::apply(env, &mut spec)?;

    policy::enforce(env, &spec)?;

    let backend = opts
        .backend
        .clone()
        .or(m.backend.clone())
        .unwrap_or_else(|| "auto".into());
    Ok(Built {
        approver: None,
        worktree,
        auth,
        spec,
        profile,
        harness_exe: harness.exe,
        manifest: m,
        backend,
    })
}
