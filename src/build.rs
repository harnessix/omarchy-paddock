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
    /// Name the worktree branch after this app.
    pub worktree_app: Option<String>,
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

/// A git call on a repository that a sandbox can write. It never runs what
/// the repository's own configuration names (see hostgit.rs).
fn git(dir: &Path, args: &[&str]) -> Result<String> {
    crate::hostgit::Checkout::open(dir)?.run(args)
}

/// Today's date in UTC as YYYYMMDD, for branch names.
fn today() -> String {
    ymd(state::now())
}

pub(crate) fn ymd(unix: u64) -> String {
    let days = (unix / 86_400) as i64;
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}{m:02}{d:02}")
}

/// A worktree for one run. `app` names the branch after an app:
/// `paddock/<app>-<date>-<id>`. Terminal runs get `paddock/<id>`.
pub fn create_worktree(env: &HostEnv, repo: &Path, app: Option<&str>) -> Result<Worktree> {
    let layout = crate::hostgit::Layout::discover(repo)?;
    if layout.as_ref().map(|l| l.work_tree.as_path()) != Some(repo) {
        bail!(
            "--worktree needs a git repository: {} is not one",
            repo.display()
        );
    }
    let base = layout
        .as_ref()
        .unwrap()
        .query(&["rev-parse", "HEAD"])
        .map(|s| s.trim().to_string())
        .context("--worktree needs at least one commit")?;
    let id = state::new_id("wt", repo);
    let branch = match app {
        Some(a) => format!(
            "paddock/{a}-{}-{}",
            today(),
            id.rsplit('-').next().unwrap_or("0")
        ),
        None => format!("paddock/{id}"),
    };
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

/// What the run left in its worktree: commits, uncommitted changes and the
/// commands to review, merge or remove it. Empty lines are dropped.
pub fn worktree_report(env: &HostEnv, w: &Worktree) -> String {
    let repo = crate::hostgit::Layout::discover(&w.repo).ok().flatten();
    let query = |args: &[&str]| {
        repo.as_ref()
            .and_then(|l| l.try_query(args))
            .map(|s| s.trim_end().to_string())
            .unwrap_or_default()
    };
    let range = format!("{}..{}", w.base, w.branch);
    let commits = query(&["log", "--oneline", &range]);
    // `--no-textconv` keeps a textconv program in the repository
    // configuration from running on the host.
    let stat = query(&[
        "diff",
        "--stat",
        "--no-textconv",
        "--no-ext-diff",
        &format!("{}...{}", w.base, w.branch),
    ]);
    // The work tree is read in a sandbox: `git status` runs clean filters,
    // which no `-c` flag can turn off.
    let dirty = match worktree_status(env, &w.path) {
        Ok(Some(lines)) => lines.join("\n"),
        Ok(None) => String::new(),
        Err(e) => format!("paddock cannot read the worktree state: {e:#}"),
    };
    let mut t = format!(
        "paddock: the sandbox worked in {} on branch {}.\n",
        w.path.display(),
        w.branch
    );
    if commits.is_empty() && dirty.is_empty() {
        t.push_str(&format!(
            "paddock: it made no changes. Remove the worktree:\n  git -C {} worktree remove {} && git -C {0} branch -D {}",
            w.repo.display(),
            w.path.display(),
            w.branch
        ));
        return t;
    }
    if !commits.is_empty() {
        t.push_str(&format!("commits:\n{commits}\n{stat}\n"));
    }
    if !dirty.is_empty() {
        t.push_str(&format!("uncommitted changes in the worktree:\n{dirty}\n"));
    }
    t.push_str(&format!(
        "review:  git -C {r} diff {b}...{br}\nmerge:   git -C {r} merge {br}\nremove:  git -C {r} worktree remove --force {p} && git -C {r} branch -D {br}",
        r = w.repo.display(),
        b = &w.base[..w.base.len().min(12)],
        br = w.branch,
        p = w.path.display()
    ));
    t
}

/// The state of a worktree that a sandbox wrote. `Ok(None)` means this host
/// cannot run the check in a sandbox, so the state stays unknown.
pub fn worktree_status(env: &HostEnv, path: &Path) -> Result<Option<Vec<String>>> {
    let Some(layout) = crate::hostgit::Layout::discover(path)? else {
        return Ok(Some(vec![]));
    };
    layout.status(env, "normal")
}

/// One line for a notification: how many commits and changed files. The
/// change count is None when the worktree state could not be read.
pub fn worktree_counts(env: &HostEnv, w: &Worktree) -> (usize, Option<usize>) {
    let commits = crate::hostgit::Layout::discover(&w.repo)
        .ok()
        .flatten()
        .and_then(|l| l.try_query(&["rev-list", &format!("{}..{}", w.base, w.branch)]))
        .map(|s| s.lines().count())
        .unwrap_or(0);
    let changed = worktree_status(env, &w.path)
        .ok()
        .flatten()
        .map(|lines| lines.iter().filter(|l| !l.trim().is_empty()).count());
    (commits, changed)
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

/// The host folders that a sandbox can write, as real paths: the project,
/// every read-write mount so far, and all sandbox homes and worktrees (a
/// sandbox of another project can write those).
pub fn sandbox_writable(env: &HostEnv, spec: &SandboxSpec) -> Vec<PathBuf> {
    let mut v = vec![
        spec.project.clone(),
        spec.home_src.clone(),
        env.paddock_data().join("homes"),
        env.paddock_data().join("worktrees"),
    ];
    v.extend(
        spec.mounts
            .iter()
            .filter(|m| m.kind == crate::spec::MountKind::Rw)
            .filter_map(|m| m.src.clone()),
    );
    v.into_iter()
        .map(|p| std::fs::canonicalize(&p).unwrap_or(p))
        .collect()
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
        Some(create_worktree(
            env,
            &project,
            opts.worktree_app.as_deref(),
        )?)
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
    // For `paddock du` and `paddock gc`. The sandbox can rewrite it, so they
    // read it as a hint only.
    crate::storage::write_origin(
        &home_src,
        &crate::storage::Origin::new(&home_key, &profile.name, &auth, profile.app.is_some()),
    )?;
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
    // An approved entry is resolved again at each start. A symlink on its
    // way inside a folder the sandbox can write could point it anywhere, so
    // such a symlink is refused (see safefs::resolve_approved).
    for (list, rw, origin) in [
        (&m.mounts.rw, true, "manifest mounts.rw"),
        (&m.mounts.ro, false, "manifest mounts.ro"),
    ] {
        for raw in list {
            let p = env.expand(raw, &project);
            if !p.exists() {
                bail!("{origin}: {} does not exist", p.display());
            }
            let real = crate::safefs::resolve_approved(&p, &sandbox_writable(env, &spec))
                .with_context(|| origin.to_string())?;
            spec.add_mount(Mount::same(real, rw, origin));
        }
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
        // An editor that opens the repository opens the worktree instead.
        let repo_arg = home_key.display().to_string();
        c.extend(opts.args.iter().map(|a| match &worktree {
            Some(w) if *a == repo_arg => w.path.display().to_string(),
            _ => a.clone(),
        }));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_for_branch_names() {
        assert_eq!(ymd(0), "19700101");
        assert_eq!(ymd(951_782_400), "20000229");
        assert_eq!(ymd(4_102_444_799), "20991231");
    }

    #[test]
    fn an_app_worktree_has_its_own_branch_and_report() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("app");
        std::fs::create_dir_all(&repo).unwrap();
        let g = |dir: &Path, args: &[&str]| {
            let ok = std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(["-c", "user.name=t", "-c", "user.email=t@localhost"])
                .args(args)
                .status()
                .unwrap()
                .success();
            assert!(ok, "git {args:?}");
        };
        g(&repo, &["init", "-q", "-b", "main"]);
        g(&repo, &["commit", "-q", "--allow-empty", "-m", "one"]);
        let repo = std::fs::canonicalize(&repo).unwrap();
        let mut vars = std::collections::BTreeMap::new();
        vars.insert(
            "XDG_DATA_HOME".to_string(),
            tmp.path().join("data").display().to_string(),
        );
        // PATH so that the sandboxed work-tree check can find bwrap and git.
        if let Ok(p) = std::env::var("PATH") {
            vars.insert("PATH".to_string(), p);
        }
        let env = HostEnv::from_parts(tmp.path().join("home"), "t".into(), 1000, 1000, vars);
        let w = create_worktree(&env, &repo, Some("vscode")).unwrap();
        assert!(w
            .branch
            .starts_with(&format!("paddock/vscode-{}-", today())));
        assert!(w
            .path
            .starts_with(tmp.path().join("data/paddock/worktrees")));
        assert!(worktree_report(&env, &w).contains("made no changes"));
        g(
            &w.path,
            &["commit", "-q", "--allow-empty", "-m", "work in the app"],
        );
        std::fs::write(w.path.join("new.txt"), "x").unwrap();
        let r = worktree_report(&env, &w);
        assert!(r.contains("work in the app"), "{r}");
        if env.which("bwrap").is_some() {
            // The worktree state comes from a sandboxed `git status`.
            assert!(r.contains("?? new.txt"), "{r}");
            assert_eq!(worktree_counts(&env, &w), (1, Some(1)));
        } else {
            assert_eq!(worktree_counts(&env, &w), (1, None));
        }
        // The checkout itself stays on its branch.
        let head = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["branch", "--show-current"])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&head.stdout).trim(), "main");
    }
}
