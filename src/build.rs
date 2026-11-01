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
    /// A sandbox that another sandbox asked for (`paddock-host spawn`). It
    /// uses the parent's settings snapshot and runs the one-shot command.
    pub child: Option<crate::spawn::ChildPlan>,
    /// `paddock wrap`: the command runs as given, with its `env` prefix and
    /// shell form, and the login comes from its account folder.
    pub transparent: Option<crate::wrap::Transparent>,
    /// Join this pasture of the project (`--pasture NAME`). None: the
    /// sandbox is a pasture of its own.
    pub pasture: Option<String>,
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
    crate::privfs::dir(path.parent().unwrap())?;
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
    /// For a child sandbox: where it came from. Its broker takes the depth
    /// and the host modes from here.
    pub spawn: Option<crate::spawn::ChildInfo>,
    /// Bytes for the harness's standard input (a spawn prompt).
    pub stdin: Option<Vec<u8>>,
    /// The repository or folder that the settings and the home belong to.
    /// With a worktree, the repository; otherwise the project.
    pub home_key: PathBuf,
    /// Whether the host environment was passed.
    pub inherit_env: bool,
    /// Who answers host passthrough requests. None means the user, through
    /// the panel, a notification or `paddock approvals`.
    pub approver: Option<std::sync::Arc<dyn crate::broker::Approver>>,
    /// The sandbox that started this one, for the `parent` scope of paddock
    /// kv. None for a sandbox that the user started. The spawn child build
    /// sets it from the job that the parent's paddock wrote.
    pub parent: Option<String>,
    /// For a spawned sandbox: the devices of its parent. It gets no other.
    pub device_ceiling: Option<Vec<String>>,
    /// The run shares session history with the host (`paddock wrap` starts).
    pub shared_history: bool,
    /// Serve the built-in `echo` device whatever the user configuration says.
    /// `paddock doctor --escape` tests the device mount with it.
    pub test_devices: bool,
    pub worktree: Option<Worktree>,
    /// The pasture it joined; None: a pasture of its own.
    pub pasture: Option<String>,
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

/// The harness install mounts come from the executable and the script
/// headers it leads to (`profile::runtime_closure`). A file in a folder the
/// sandbox can write could name any host path in its header, and paddock
/// would mount that path. So the harness must not come from such a folder,
/// for example through a host PATH entry inside the project.
pub fn check_harness_install(harness: &Harness, writable: &[PathBuf]) -> Result<()> {
    let all = std::iter::once(&harness.exe)
        .chain(&harness.install_mounts)
        .chain(&harness.path_dirs);
    for p in all {
        if let Some(w) = writable.iter().find(|w| p.starts_with(w)) {
            bail!(
                "the {} harness resolves to {}, inside {}, which the sandbox can write. \
                 paddock does not mount host paths that such a file names. \
                 Install the harness outside the project and the sandbox homes.",
                harness.profile.name,
                p.display(),
                w.display()
            );
        }
    }
    Ok(())
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
    if let Some(name) = &opts.pasture {
        if opts.child.is_some() {
            bail!("a sandbox that another one starts cannot join a pasture");
        }
        crate::pasture::admit(env, name, &opts.harness)?;
    }
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
    // A child uses the settings that its parent had when it started. The
    // project file may have changed since: the parent can write it.
    let m = match &opts.child {
        Some(c) => c.manifest.clone(),
        None => {
            let loaded = Manifest::load(env, &project)?;
            manifest::require_approval(env, &loaded, opts.assume_yes)?;
            loaded.merged
        }
    };
    let profile = harness.profile.clone();
    // The settings and the sandbox home belong to the repository; the agent
    // works in the worktree.
    let home_key = project.clone();
    let worktree = if let Some(c) = &opts.child {
        Some(crate::spawn::create_worktree(env, &project, c)?)
    } else if opts.worktree {
        Some(create_worktree(
            env,
            &project,
            opts.worktree_app.as_deref(),
        )?)
    } else {
        None
    };
    let project = worktree.as_ref().map(|w| w.path.clone()).unwrap_or(project);

    let account = opts.transparent.as_ref().and_then(|t| t.account.as_ref());
    let auth: state::AuthMode = match account {
        Some(dir) => state::AuthMode::account(dir),
        None => opts
            .auth
            .clone()
            .or(m.auth.mode.clone())
            .unwrap_or_else(|| "shared".into())
            .parse()?,
    };
    if let state::AuthMode::Account { dir, .. } = &auth {
        if dir.as_os_str().is_empty() {
            bail!("the account mode is only for `paddock wrap`; use shared or isolated:NAME");
        }
    }
    let id = match &opts.child {
        Some(c) => c.id.clone(),
        None => state::new_id(&profile.name, &project),
    };
    let home_src = state::sandbox_home(env, &profile.name, &auth, &home_key)?;
    // For `paddock du` and `paddock gc`. The sandbox can rewrite it, so they
    // read it as a hint only.
    crate::storage::write_origin(
        &home_src,
        &crate::storage::Origin::new(&home_key, &profile.name, &auth, profile.app.is_some()),
    )?;
    let mut spec = SandboxSpec::new(
        id.clone(),
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
    // An isolated login is another machine; a host login keeps the host's
    // identity.
    spec.identity = crate::identity::for_login(env, &profile.name, &auth)?;

    spec.add_mount(Mount::same(&project, true, "project"));

    // The harness.
    let porigin = format!("profile {}", profile.name);
    for p in &harness.install_mounts {
        spec.add_mount(Mount::same(p, false, &format!("{porigin}: install")));
    }
    // The harness's own folder: a shell in the sandbox (`paddock shell`),
    // and tools that start the harness by name, find it. The folder is
    // already among the install mounts. It goes last on PATH, so it only
    // adds the harness: a harness in /usr/bin must not put the host's `gh`
    // before paddock's, or the host's `jq` before the project's mise tools.
    if let Some(d) = harness.exe.parent() {
        spec.inherit_path.push(d.to_path_buf());
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
        let mut withheld = Vec::new();
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
            // Real secrets stay on the host. The harness's own login
            // variables (env_pass) and the ones that the user hands over in
            // [env] pass are added below, with their values.
            if policy::looks_secret(k) {
                if !m.env.pass.contains(k) && !profile.state.env_pass.contains(k) {
                    withheld.push(k.clone());
                }
                continue;
            }
            count += 1;
            spec.set_env(k, v.clone(), "host env (inherited)");
        }
        for d in &env.path {
            spec.inherit_path.push(d.clone());
        }
        // Tell the user once per project, and again when the list of
        // secret-looking names changes.
        let login: Vec<String> = profile
            .state
            .env_pass
            .iter()
            .filter(|k| policy::looks_secret(k) && env.var(k).is_some())
            .cloned()
            .collect();
        let mut notice_key: Vec<String> =
            withheld.iter().map(|w| format!("withheld:{w}")).collect();
        notice_key.extend(login.iter().map(|l| format!("login:{l}")));
        notice_key.extend(dropped.iter().map(|d| format!("dropped:{d}")));
        if crate::state::env_notice_changed(env, &project, &notice_key) {
            let mut note = format!("passing {count} host environment variables");
            if !withheld.is_empty() {
                note.push_str(&format!(
                    ". These look like secrets and stay on the host: {}. To give one to the sandbox, add it to [env] pass in the user configuration (the agent can then read it)",
                    withheld.join(", ")
                ));
            }
            if !login.is_empty() {
                note.push_str(&format!(
                    ". The login variables of {} pass with their values, so the harness can sign in: {}. A later paddock keeps them on the host as well",
                    profile.name,
                    login.join(", ")
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
        let host = state::host_file(env, &profile, &auth, rel);
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
    check_harness_install(&harness, &sandbox_writable(env, &spec))?;
    // Host history only for starts through `paddock wrap` (omarchy-agent),
    // where the Omarchy agents panel needs it. A sandbox-written transcript
    // that the host harness loads later (`claude --resume`, `codex resume`)
    // can carry instructions out of the sandbox, so other runs keep their
    // history private in the sandbox home.
    let shared_history = opts.transparent.is_some();
    if shared_history {
        crate::history::apply(env, &profile, &auth, &mut spec)?;
    }
    if let state::AuthMode::Account { dir, .. } = &auth {
        // The host writes refreshed tokens back into the account folder. A
        // folder that a sandbox can write could lead those writes anywhere.
        let real = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.clone());
        if let Some(w) = sandbox_writable(env, &spec)
            .iter()
            .find(|w| real.starts_with(w))
        {
            bail!(
                "the account folder {} is inside {}, which a sandbox can write; paddock does not use it as a login",
                dir.display(),
                w.display()
            );
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
    // A registered credential reaches the sandbox only through its channel
    // (credentials.rs), never with the environment, env_pass or [env] pass.
    for (name, c) in crate::credentials::load(env)? {
        if c.from == crate::credentials::Source::Env {
            spec.env.remove(c.host_var(&name));
        }
        // Its name in the sandbox too: a deliver value or a gateway
        // stand-in replaces it only after the start card.
        spec.env.remove(c.sandbox_var(&name));
    }
    // The `env` prefix of a `paddock wrap` command, as the agent gets it on
    // the host.
    if let Some(t) = &opts.transparent {
        for k in &t.env_unset {
            spec.env.remove(k);
        }
        for (k, v) in &t.env_set {
            spec.set_env(k, v.clone(), "command: env prefix");
        }
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
    crate::privacy::push_settings(
        &profile,
        &profile.state.settings,
        &format!("{porigin} state.settings"),
        &mut spec,
    )?;
    if spec.network != Network::None {
        for p in &profile.network.publish {
            if !spec.publish.contains(p) {
                spec.publish.push(*p);
            }
        }
    }
    if let Some(why) = &profile.launch.wrap_on_host {
        spec.warnings.push(format!("{}: {why}", profile.name));
    }
    // Last, so neither the host environment nor a project file turns
    // telemetry back on. Only the user configuration can ([privacy]).
    let privacy = crate::privacy::mode(&m);
    crate::privacy::apply(&profile, privacy, &mut spec)?;
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
    } else if opts.child.is_some() {
        // The one-shot command. The prompt arrives on stdin or as a file in
        // the home, never as an argument.
        let o = profile
            .oneshot
            .as_ref()
            .with_context(|| format!("the {} profile has no [oneshot] command", profile.name))?;
        let mut c = vec![harness.exe.display().to_string()];
        // The privacy flags are global options: before a subcommand.
        c.extend(crate::privacy::args(&profile, privacy));
        c.extend(crate::hostcli::status::harness_args(env, &profile.name));
        c.extend(o.render(&spec.home.join(crate::spawn::PROMPT_IN_HOME)));
        c
    } else if let Some(t) = opts.transparent.as_ref().filter(|t| !t.shell.is_empty()) {
        // The shell form: the script starts its `$0`, which is the harness.
        // The script's arguments stay as they are.
        let mut c = t.shell.clone();
        c.push(harness.exe.display().to_string());
        c.extend(opts.args.iter().cloned());
        c
    } else {
        let mut c = vec![harness.exe.display().to_string()];
        if !opts.no_yolo {
            c.extend(profile.launch.yolo_args.iter().cloned());
        }
        c.extend(crate::privacy::args(&profile, privacy));
        // Timers and limits in the agent's context (hostcli/status.rs).
        if opts.transparent.is_none() {
            c.extend(crate::hostcli::status::harness_args(env, &profile.name));
        }
        // Transparent: the command already has the arguments it needs.
        if opts.transparent.is_none() {
            c.extend(profile.launch.args.iter().cloned());
        }
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

    // A reviewer child reads the files and reports. Its worktree and the
    // repository are read-only; its own home and the caches stay writable.
    if opts
        .child
        .as_ref()
        .is_some_and(|c| c.role == crate::spawn::Role::Reviewer)
    {
        crate::spawn::make_read_only(&mut spec, &[project.clone(), home_key.clone()]);
    }

    // The shared folder of the pasture. paddock sets the variables last,
    // so that nothing from the host or the project replaces them.
    if let Some(name) = &opts.pasture {
        let kv_project = worktree
            .as_ref()
            .map(|w| w.repo.clone())
            .unwrap_or_else(|| home_key.clone());
        let ground = crate::pasture::ground(env, &kv_project, name);
        crate::privfs::dir(&ground)?;
        spec.add_mount(Mount::bind(
            &ground,
            crate::pasture::MOUNT,
            true,
            &format!("pasture {name}"),
        ));
        spec.set_env("PADDOCK_PASTURE", crate::pasture::MOUNT, "pasture");
        spec.set_env("PADDOCK_PASTURE_NAME", name.clone(), "pasture");
        // The helper that relays the ports that members share.
        spec.setup
            .push("paddock host pasture serve >/dev/null 2>&1 &".into());
    }

    policy::enforce(env, &spec)?;

    let backend = opts
        .backend
        .clone()
        .or(m.backend.clone())
        .unwrap_or_else(|| "auto".into());
    Ok(Built {
        spawn: None,
        stdin: None,
        home_key,
        inherit_env: inherit,
        approver: None,
        parent: None,
        device_ceiling: None,
        test_devices: false,
        shared_history,
        worktree,
        pasture: opts.pasture.clone(),
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
    fn a_harness_from_a_sandbox_writable_folder_is_refused() {
        let profile: Profile = toml::from_str("name = \"t\"\n").unwrap();
        let mut h = Harness {
            profile,
            exe: PathBuf::from("/usr/bin/tool"),
            install_mounts: vec![PathBuf::from("/home/demo/.local/share/mise/installs/t/1")],
            path_dirs: vec![],
            source: "test".into(),
        };
        let writable = vec![PathBuf::from("/home/demo/proj")];
        assert!(check_harness_install(&h, &writable).is_ok());
        // An interpreter that a project script names: its folder is on PATH.
        h.path_dirs.push(PathBuf::from("/home/demo/proj/bin"));
        assert!(check_harness_install(&h, &writable).is_err());
        h.path_dirs.clear();
        h.exe = PathBuf::from("/home/demo/proj/node_modules/.bin/tool");
        assert!(check_harness_install(&h, &writable).is_err());
    }

    /// Build a spec for a built-in profile with a stand-in executable, a
    /// user configuration and a project file.
    fn privacy_spec(name: &str, user: &str, project: &str) -> (tempfile::TempDir, Built) {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let proj = tmp.path().join("proj");
        std::fs::create_dir_all(home.join(".config/paddock")).unwrap();
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(home.join(".config/paddock/config.toml"), user).unwrap();
        std::fs::write(proj.join("paddock.toml"), project).unwrap();
        let mut vars = std::collections::BTreeMap::new();
        vars.insert("DISABLE_TELEMETRY".to_string(), String::new());
        let env = HostEnv::from_parts(home, "t".into(), 1000, 1000, vars);
        let h = Harness {
            profile: crate::profile::builtin(name).unwrap(),
            exe: PathBuf::from("/usr/bin/true"),
            install_mounts: vec![],
            path_dirs: vec![],
            source: "test".into(),
        };
        let opts = Options {
            harness: name.into(),
            project: Some(proj),
            assume_yes: true,
            ..Default::default()
        };
        let built = build_with(&env, &opts, h).unwrap();
        (tmp, built)
    }

    #[test]
    fn the_harness_folder_is_on_the_sandbox_path() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let proj = tmp.path().join("proj");
        let tools = tmp.path().join("tools/codex/1.0/bin");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::create_dir_all(&tools).unwrap();
        let env = HostEnv::from_parts(home, "t".into(), 1000, 1000, Default::default());
        let h = Harness {
            profile: crate::profile::builtin("codex").unwrap(),
            exe: tools.join("codex"),
            install_mounts: vec![tools.parent().unwrap().to_path_buf()],
            path_dirs: vec![],
            source: "test".into(),
        };
        for shell in [false, true] {
            let opts = Options {
                harness: "codex".into(),
                project: Some(proj.clone()),
                assume_yes: true,
                shell,
                ..Default::default()
            };
            let b = build_with(&env, &opts, h.clone()).unwrap();
            let path = b.spec.path_value();
            let dirs: Vec<&str> = path.split(':').collect();
            let at = dirs.iter().position(|d| Path::new(d) == tools);
            assert!(at.is_some(), "shell={shell}: {path}");
            // Never before paddock's own tools (gh, git) or the system tools.
            let usr_bin = dirs.iter().position(|d| *d == "/usr/bin").unwrap();
            assert!(at.unwrap() > usr_bin, "{path}");
        }
        // A harness in /usr/bin changes nothing: no second /usr/bin, and
        // the mise shims and paddock's tools stay before it.
        let opts = Options {
            harness: "codex".into(),
            project: Some(proj.clone()),
            assume_yes: true,
            ..Default::default()
        };
        let before = build_with(&env, &opts, h.clone())
            .unwrap()
            .spec
            .path_value();
        let sys = Harness {
            exe: PathBuf::from("/usr/bin/codex"),
            ..h.clone()
        };
        let after = build_with(&env, &opts, sys).unwrap().spec.path_value();
        assert_eq!(after.matches(":/usr/bin:").count(), 1, "{after}");
        assert_eq!(
            after,
            before.replace(&format!(":{}", tools.display()), ""),
            "{after}"
        );
    }

    #[test]
    fn a_pasture_member_gets_the_shared_folder() {
        let (t, b) = privacy_spec("claude", "", "");
        assert!(b.pasture.is_none());
        assert!(!b.spec.mounts.iter().any(|m| m.dst == Path::new("/pasture")));
        assert!(!b.spec.env.contains_key("PADDOCK_PASTURE"));
        drop(t);

        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let proj = tmp.path().join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let env = HostEnv::from_parts(home, "t".into(), 1000, 1000, Default::default());
        let h = Harness {
            profile: crate::profile::builtin("claude").unwrap(),
            exe: PathBuf::from("/usr/bin/true"),
            install_mounts: vec![],
            path_dirs: vec![],
            source: "test".into(),
        };
        let opts = Options {
            harness: "claude".into(),
            project: Some(proj.clone()),
            assume_yes: true,
            pasture: Some("team".into()),
            ..Default::default()
        };
        let b = build_with(&env, &opts, h).unwrap();
        let m = b
            .spec
            .mounts
            .iter()
            .find(|m| m.dst == Path::new("/pasture"))
            .expect("no /pasture mount");
        assert_eq!(m.kind, crate::spec::MountKind::Rw);
        let ground = crate::pasture::ground(&env, &b.home_key, "team");
        assert_eq!(m.src.as_deref(), Some(ground.as_path()));
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&ground).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(b.spec.env["PADDOCK_PASTURE"].value, "/pasture");
        assert_eq!(b.spec.env["PADDOCK_PASTURE_NAME"].value, "team");
        // The admission check of `build` comes before this: a configured
        // member only.
        assert!(build(&env, &opts).is_err());
    }

    #[test]
    fn privacy_defaults_are_in_the_spec_with_their_origin() {
        let origin = |n: &str| format!("profile {n} privacy (telemetry off)");
        let (_t, b) = privacy_spec("claude", "", "");
        for k in ["DISABLE_TELEMETRY", "DISABLE_ERROR_REPORTING"] {
            let v = &b.spec.env[k];
            // Set over the empty host value, so the host cannot undo it.
            assert_eq!(v.value, "1", "{k}");
            assert_eq!(v.origin, origin("claude"));
        }
        let (_t, b) = privacy_spec("grok", "", "");
        assert_eq!(b.spec.env["DISABLE_ERROR_REPORTING"].origin, origin("grok"));
        let (_t, b) = privacy_spec("copilot", "", "");
        assert!(b
            .spec
            .command
            .contains(&"--disable-client-telemetry".to_string()));
        let (_t, b) = privacy_spec("codex", "", "");
        let s = &b.spec.home_settings[0];
        assert_eq!(s.file, PathBuf::from(".codex/config.toml"));
        assert_eq!(s.path, vec!["analytics", "enabled"]);
        assert_eq!(s.value, serde_json::json!(false));
        assert_eq!(s.origin, origin("codex"));
        let (_t, b) = privacy_spec("vscode", "", "");
        assert!(b
            .spec
            .home_settings
            .iter()
            .any(|s| s.path == ["telemetry.telemetryLevel"]
                && s.value == "off"
                && s.file == Path::new(".config/Code/User/settings.json")));
        // Every settings file is relative to the sandbox home.
        for name in ["codex", "gemini", "vscode"] {
            let (_t, b) = privacy_spec(name, "", "");
            assert!(!b.spec.home_settings.is_empty(), "{name}");
            for s in &b.spec.home_settings {
                assert!(s.file.is_relative(), "{name}: {}", s.file.display());
            }
        }
    }

    #[test]
    fn vendor_mode_removes_the_privacy_defaults() {
        let user = "[privacy]\ntelemetry = \"vendor\"\n";
        let (_t, b) = privacy_spec("claude", user, "");
        assert!(!b.spec.env.values().any(|v| v.origin.contains("privacy")));
        assert_eq!(b.spec.env["DISABLE_TELEMETRY"].value, "");
        let (_t, b) = privacy_spec("codex", user, "");
        assert!(b.spec.home_settings.is_empty());
        let (_t, b) = privacy_spec("copilot", user, "");
        assert!(!b
            .spec
            .command
            .contains(&"--disable-client-telemetry".to_string()));
    }

    #[test]
    fn a_project_file_cannot_change_the_privacy_mode() {
        let vendor = "[privacy]\ntelemetry = \"vendor\"\n";
        let off = "[privacy]\ntelemetry = \"off\"\n";
        // Turning telemetry on from the project is ignored ...
        let (_t, b) = privacy_spec("codex", "", vendor);
        assert_eq!(b.spec.home_settings.len(), 1);
        // ... and so are project variables with the same names.
        let (_t, b) = privacy_spec("claude", "", "[env.set]\nDISABLE_ERROR_REPORTING = \"\"\n");
        assert_eq!(b.spec.env["DISABLE_ERROR_REPORTING"].value, "1");
        // A project cannot turn vendor mode off either.
        let (_t, b) = privacy_spec("codex", vendor, off);
        assert!(b.spec.home_settings.is_empty());
    }

    #[test]
    fn profile_ports_settings_and_host_notices_reach_the_spec() {
        // dsh serves its UI to the host browser on one loopback port.
        let (_t, b) = privacy_spec("dsh", "", "");
        assert_eq!(b.spec.publish, vec![3080]);
        let (_t, b) = privacy_spec("dsh", "[network]\nmode = \"none\"\n", "");
        assert!(b.spec.publish.is_empty());
        // A self-update switch goes into the sandbox copy of the settings.
        let (_t, b) = privacy_spec("devin", "", "");
        assert!(b
            .spec
            .home_settings
            .iter()
            .any(|s| s.path == ["auto_update"]
                && s.value == serde_json::json!(false)
                && s.origin.contains("state.settings")));
        // muse keeps its own sandbox: `paddock muse` says what that means.
        let (_t, b) = privacy_spec("muse", "", "");
        assert!(b.spec.warnings.iter().any(|w| w.contains("own sandbox")));
    }

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
        // The worktree state comes from a sandboxed `git status`. A host that
        // cannot run one reports it as unknown instead of guessing.
        let (commits, changed) = worktree_counts(&env, &w);
        assert_eq!(commits, 1);
        match changed {
            Some(n) => {
                assert_eq!(n, 1);
                assert!(r.contains("?? new.txt"), "{r}");
            }
            None => assert!(r.contains("cannot read the worktree state") || r.contains("commits")),
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
