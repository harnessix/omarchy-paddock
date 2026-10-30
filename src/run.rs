//! Launch a built spec, wait for it, and clean up.

use crate::backend::{self, Backend, Launch};
use crate::build::Built;
use crate::env::HostEnv;
use crate::gui::{self, WindowCompositor};
use crate::manifest::hex;
use crate::spec::{GuiMode, SandboxSpec};
use crate::state::{self, AuthSync, Running};
use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::io::{IsTerminal, Read};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub fn write_files(launch: &Launch) -> Result<()> {
    for (path, text, mode) in &launch.files {
        gui::write_file(path, text, *mode)?;
    }
    Ok(())
}

/// Make sure that the harness still accepts its auto-approval flags. Flags get
/// renamed between versions; without this check a harness could start in a
/// mode that waits for approvals that nobody gives.
pub fn probe_flags(env: &HostEnv, built: &Built) -> Result<()> {
    probe_args(
        env,
        &built.harness_exe,
        &built.profile.launch.yolo_args,
        &built.profile.launch.probe_args,
        "yolo_args",
    )
}

/// Make sure that the help output (`exe probe`) still mentions every flag
/// in `args`. `what` names the profile entry in the error.
pub fn probe_args(
    env: &HostEnv,
    exe: &Path,
    args: &[String],
    probe: &[String],
    what: &str,
) -> Result<()> {
    let flags: Vec<&String> = args
        .iter()
        .filter(|a| a.starts_with('-') && a.len() > 1)
        .collect();
    if flags.is_empty() {
        return Ok(());
    }
    let meta = std::fs::metadata(exe)?;
    let key = format!(
        "{}|{}|{}|{:?}|{:?}",
        exe.display(),
        meta.len(),
        meta.mtime(),
        flags,
        probe
    );
    let cache = env
        .paddock_state()
        .join("probe")
        .join(&hex(&Sha256::digest(key.as_bytes()))[..16]);
    if cache.is_file() {
        return Ok(());
    }
    let text = help_text(exe, probe)?;
    let missing: Vec<&str> = flags
        .iter()
        .filter(|f| !text.contains(f.as_str()))
        .map(|f| f.as_str())
        .collect();
    if !missing.is_empty() {
        bail!(
            "{} {} no longer mentions {}. The harness may have renamed a flag. \
             Update {what} in a profile override under {}",
            exe.display(),
            probe.join(" "),
            missing.join(", "),
            env.paddock_config().join("harness.d").display()
        );
    }
    crate::privfs::write(&cache, "ok")?;
    Ok(())
}

fn help_text(exe: &Path, args: &[String]) -> Result<String> {
    let mut child = Command::new(exe)
        .args(args)
        // Not the project: a harness may read settings from its working folder.
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("cannot run {}", exe.display()))?;
    let start = Instant::now();
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if start.elapsed() > Duration::from_secs(20) {
            let _ = child.kill();
            bail!(
                "{} {} did not finish in 20 seconds",
                exe.display(),
                args.join(" ")
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let mut text = String::new();
    child.stdout.take().unwrap().read_to_string(&mut text)?;
    child.stderr.take().unwrap().read_to_string(&mut text)?;
    Ok(text)
}

pub struct Outcome {
    pub code: i32,
    pub stdout: Option<String>,
}

/// Run the sandbox. With `capture`, stdout is collected instead of shown.
pub fn run(
    env: &HostEnv,
    mut built: Built,
    backend: &dyn Backend,
    capture: bool,
    quiet: bool,
) -> Result<Outcome> {
    let interactive = !capture && std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    // App mode: a restricted Wayland socket and a portal-only session bus,
    // both alive until the sandbox exits.
    let _app = if built.spec.gui.mode == GuiMode::App {
        Some(crate::app::start_session(
            env,
            &mut built.spec,
            &built.profile,
        )?)
    } else {
        None
    };
    let _compositor = if built.spec.gui.mode == GuiMode::Window {
        let c = WindowCompositor::start(env, &built.spec)?;
        built.spec.gui.host_socket = Some(c.socket.clone());
        Some(c)
    } else {
        None
    };
    backend.prepare(env, &built.spec, interactive)?;
    let (repo, spawner) = start_broker(env, &mut built, interactive, backend.name())?;
    // paddock kv and the devices. Each one is optional: when it cannot start,
    // the sandbox starts without it and a notice says why.
    let kv_approver: std::sync::Arc<dyn crate::broker::Approver> = built
        .approver
        .clone()
        .unwrap_or_else(|| std::sync::Arc::new(crate::broker::HostApprover));
    let kv_project = built
        .worktree
        .as_ref()
        .map(|w| w.repo.clone())
        .unwrap_or_else(|| built.spec.project.clone());
    let lineage = crate::kv::Lineage {
        parent: built.parent.clone(),
        chain: built
            .spawn
            .as_ref()
            .map(|c| c.chain.clone())
            .unwrap_or_default(),
        can_spawn: spawner.as_ref().is_some_and(|s| s.available()),
    };
    let kv = crate::kv::start(env, &mut built.spec, kv_project, kv_approver, lineage);
    let mut devices = crate::devices::start(
        env,
        &mut built.spec,
        backend.name(),
        built.test_devices,
        built.device_ceiling.as_deref(),
        &built
            .spawn
            .as_ref()
            .map(|c| c.chain.clone())
            .unwrap_or_default(),
    );
    // Terminal harnesses that log in through a browser and a localhost callback.
    let _login = if built.profile.launch.login_callback && built.spec.gui.mode != GuiMode::App {
        crate::login::prepare(env, &mut built.spec, backend.name() == "srt")?
    } else {
        None
    };
    if let Some(sock) = backend::relay_socket(env, backend.name(), &built.spec) {
        let mut cfg = crate::proxy::HostProxy::from_vars(|k| env.var(k).map(str::to_string));
        cfg.allow_loopback = built.spec.host_ports.iter().map(|(p, _)| *p).collect();
        cfg.allow_lan = built.spec.lan;
        for w in cfg.cleartext_warnings() {
            log::warn!("{w}");
        }
        crate::proxy::serve(&sock, cfg)?;
    }
    // The shared install step: a clean sandbox without the project installs
    // registry tools into the shared mise installs. srt cannot mount the
    // shared installs, so its sandboxes install everything privately.
    let scratch = env.paddock_cache().join("provision").join(&built.spec.id);
    let shared_dir = env
        .paddock_cache()
        .join("shared")
        .join(crate::mise::SHARED_CACHE);
    let shared = if backend.name() == "srt"
        || crate::mise::all_installed(&shared_dir, &built.spec.mise_shared)
    {
        None
    } else {
        built.spec.mise_shared_spec(scratch.clone())
    };
    if let Some(p) = shared {
        crate::privfs::dir(&scratch)?;
        let what = built.spec.mise_shared.join(" ");
        let result = provision(env, backend, &p, quiet, &format!("shared tools: {what}"));
        let _ = std::fs::remove_dir_all(&scratch);
        if !result? && !quiet {
            eprintln!(
                "paddock: warning: the shared install failed; the project installs the tools in its own home"
            );
        }
    }
    // The provision step, in the project sandbox with the harness's rights.
    if let Some(mut p) = built.spec.provision_spec() {
        // The devices are for the harness, not for the install step.
        p.devices = None;
        let what = built.spec.provision.join("; ");
        if !provision(env, backend, &p, quiet, &what)? && !quiet {
            eprintln!("paddock: warning: provisioning failed; the harness starts anyway");
        }
    }
    let launch = backend.render(env, &built.spec, interactive)?;
    write_files(&launch)?;

    let spec = &built.spec;
    state::seed(env, &built.profile, &built.auth, &spec.home_src)?;
    state::toml_set(&built.profile, &spec.home_src, &spec.project)?;
    state::json_import(
        env,
        &built.profile,
        &built.auth,
        &spec.home_src,
        &spec.project,
    )?;
    for w in crate::privacy::write_settings(&spec.home_src, &spec.home_settings)? {
        log::warn!("{w}");
    }
    let auth = AuthSync::copy_in(env, &built.profile, &built.auth, &spec.home_src)?;
    let history = built
        .shared_history
        .then(|| crate::history::CopyBack::new(env, &built.profile, &built.auth, &spec.home_src));
    state::write_gitconfig(env, &spec.home_src, repo.as_ref())?;
    let watched = state::snapshot(&spec.watch);

    if !quiet {
        eprintln!(
            "paddock: {} in {} (backend {}, network {}, display {})",
            spec.harness,
            spec.project.display(),
            backend.name(),
            format!("{:?}", spec.network).to_lowercase(),
            format!("{:?}", spec.gui.mode).to_lowercase()
        );
        for n in &spec.notices {
            eprintln!("paddock: {n}");
        }
        for w in &spec.warnings {
            log::warn!("{w}");
        }
    }

    let mut cmd = Command::new(&launch.program);
    cmd.args(&launch.args).env_clear().envs(&launch.env);
    if capture {
        cmd.stdout(Stdio::piped());
    }
    let stdin = built.stdin.take();
    if stdin.is_some() {
        cmd.stdin(Stdio::piped());
    }
    if built.spawn.is_some() {
        use std::os::unix::process::CommandExt;
        // A child sandbox ends with the paddock process that runs it, even
        // when that one is killed. SIGTERM, so a container CLI passes it on.
        // SAFETY: prctl is async-signal-safe.
        unsafe {
            cmd.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("cannot start {}", launch.program.display()))?;
    if let (Some(data), Some(mut w)) = (stdin, child.stdin.take()) {
        // A thread, so a harness that does not read cannot block paddock.
        std::thread::spawn(move || {
            let _ = std::io::Write::write_all(&mut w, &data);
        });
    }
    if let Some(d) = devices.as_mut() {
        d.after_start(env, backend.name(), &spec.id);
    }
    let running = Running {
        id: spec.id.clone(),
        harness: spec.harness.clone(),
        project: spec.project.clone(),
        backend: backend.name().to_string(),
        network: format!("{:?}", spec.network).to_lowercase(),
        gui: format!("{:?}", spec.gui.mode).to_lowercase(),
        pid: child.id(),
        started: state::now(),
        auth: built.auth.to_string(),
        window_pid: _compositor.as_ref().map(|c| c.pid()),
        // Short lines for the bar panel, which shows the first one.
        // Only `login_ports = true` opens every port. The filtered login
        // callback (one approved port, ten minutes) is not worth a warning.
        warnings: if spec
            .warnings
            .iter()
            .any(|w| w.starts_with("login_ports is on"))
        {
            vec!["login ports open on 127.0.0.1".to_string()]
        } else {
            Vec::new()
        },
        host_pid: std::process::id(),
        parent: built.spawn.as_ref().map(|c| c.parent.clone()),
        spawn_id: built.spawn.as_ref().map(|c| c.spawn_id.clone()),
    };
    running.register(env)?;
    // Kept after exit, so `paddock files` can list what the sandbox left.
    if let Err(e) =
        crate::recover::Session::begin(env, &spec.id, &spec.harness, &spec.project, &spec.home_src)
    {
        if !quiet {
            log::warn!("cannot record the session for `paddock files`: {e:#}");
        }
    }
    if !quiet {
        let home_key = built
            .worktree
            .as_ref()
            .map(|w| w.repo.clone())
            .unwrap_or_else(|| spec.project.clone());
        let _ = state::record_recent(env, &home_key, &spec.harness);
    }

    // pasta hides the host addresses that exist at start. Watch for new ones
    // (a VPN that comes up later) and report them: they stay reachable until
    // the next start.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let new_addrs = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let monitor =
        (backend.name() == "bwrap" && spec.network == crate::spec::Network::Egress).then(|| {
            let start_addrs = crate::proxy::local_addrs();
            let (stop, found, env2, mut info) = (
                stop.clone(),
                new_addrs.clone(),
                env.clone(),
                running.clone(),
            );
            std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                    std::thread::sleep(std::time::Duration::from_secs(3));
                    let mut f = found.lock().unwrap();
                    let mut changed = false;
                    for a in crate::proxy::local_addrs() {
                        let s = a.to_string();
                        if !a.is_loopback() && !start_addrs.contains(&a) && !f.contains(&s) {
                            f.push(s);
                            changed = true;
                        }
                    }
                    if changed {
                        // The new addresses go first: they are the bigger risk.
                        info.warnings
                            .retain(|w| !w.starts_with("new host addresses"));
                        info.warnings.insert(
                            0,
                            format!(
                                "new host addresses since start are reachable: {}",
                                f.join(", ")
                            ),
                        );
                        let _ = info.register(&env2);
                    }
                }
            })
        });

    let live = LiveSync::start(auth, history, AUTH_EVERY, HISTORY_EVERY);

    // The child owns the terminal. Ctrl-C reaches it directly; paddock waits.
    let old_int = unsafe { libc::signal(libc::SIGINT, libc::SIG_IGN) };
    let old_quit = unsafe { libc::signal(libc::SIGQUIT, libc::SIG_IGN) };
    let mut out = None;
    if capture {
        out = Some(read_capped(child.stdout.take().unwrap()));
    }
    let status = child.wait()?;
    unsafe {
        libc::signal(libc::SIGINT, old_int);
        libc::signal(libc::SIGQUIT, old_quit);
    }

    // The children of this sandbox end with it.
    if let Some(s) = &spawner {
        s.stop_all(Duration::from_secs(30));
    }
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    drop(monitor);
    let late = new_addrs.lock().unwrap().clone();
    if !late.is_empty() && !quiet {
        eprintln!(
            "paddock: warning: the host got new addresses while the sandbox ran ({}). Services on them were reachable from the sandbox. The next start hides them.",
            late.join(", ")
        );
    }
    let (mut auth, history, mut synced, copied) = live.finish();
    let last = auth.copy_out()?;
    synced.saved.extend(last.saved);
    synced.conflicts.extend(last.conflicts);
    for v in [&mut synced.saved, &mut synced.conflicts] {
        v.sort();
        v.dedup();
    }
    match history.as_ref().map(|h| h.copy_out()).unwrap_or(Ok(vec![])) {
        Ok(files) if copied + files.len() > 0 => log::info!(
            "copied {} session file(s) back to the host history",
            copied + files.len()
        ),
        Ok(_) => {}
        Err(e) => log::warn!("cannot copy the session history back: {e:#}"),
    }
    if !quiet {
        for f in &synced.saved {
            eprintln!("paddock: saved updated credentials to {}", f.display());
        }
        for f in &synced.conflicts {
            eprintln!(
                "paddock: warning: {} changed on the host while the sandbox ran, so the host version was kept. Log in again if a harness asks.",
                f.display()
            );
        }
        let changed = state::changed(&watched);
        if !changed.is_empty() {
            eprintln!("paddock: warning: these files changed. Host tools run them automatically, so review them before you use them on the host:");
            for f in changed {
                eprintln!("  {}", f.display());
            }
        }
    }
    // Unmount first: the removal must not walk into the device mount.
    drop(devices);
    if let Some(k) = kv {
        k.finish();
    }
    let _ = std::fs::remove_dir_all(env.paddock_run().join(&spec.id));
    crate::recover::Session::end(env, &spec.id);
    if !quiet {
        recovery_notice(env, &spec.id, &spec.home_src, running.started);
    }
    use std::os::unix::process::ExitStatusExt;
    let code = status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0));
    Ok(Outcome { code, stdout: out })
}

/// Quick Recovery at exit: name the files that the sandbox left in its
/// recovery folders, and the command that brings them to the host.
fn recovery_notice(env: &HostEnv, id: &str, home: &Path, started: u64) {
    let cfg = match crate::recover::RecoveryCfg::load(env) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("paddock: warning: {e:#}");
            return;
        }
    };
    let items: Vec<_> = crate::recover::scan(env, &cfg, home, started)
        .into_iter()
        .filter(|i| i.new)
        .collect();
    if items.is_empty() {
        return;
    }
    eprintln!(
        "paddock: the sandbox saved {} file(s) outside the project. They stay in the sandbox until you recover them:",
        items.len()
    );
    for i in items.iter().take(10) {
        eprintln!("  {}", i.path);
    }
    if items.len() > 10 {
        eprintln!("  ... and {} more", items.len() - 10);
    }
    eprintln!(
        "paddock: list them with `paddock files {id}`, copy them with `paddock recover {id} PATH`"
    );
}

pub fn select_backend(env: &HostEnv, built: &Built) -> Result<Box<dyn Backend>> {
    backend::docker::set_image_cfg(backend::docker::ImageCfg {
        image: built.manifest.docker.image.clone(),
        packages: built.manifest.docker.packages.clone(),
        gui: built.spec.gui.enabled(),
        app: built.spec.gui.mode == crate::spec::GuiMode::App,
        keyring: built.spec.keyring,
    });
    backend::select(env, &built.spec, &built.backend)
}

/// Start the host passthrough for this sandbox and give the sandbox the
/// `paddock-host` command. See broker.rs.
/// Returns the repository snapshot that `git-push` and `forge` work from.
type BrokerStart = (
    Option<crate::broker::git::Repo>,
    Option<std::sync::Arc<crate::spawn::Spawner>>,
);

fn start_broker(
    env: &HostEnv,
    built: &mut Built,
    interactive: bool,
    backend: &str,
) -> Result<BrokerStart> {
    let run_dir = env.paddock_run().join(&built.spec.id);
    crate::privfs::dir(&run_dir)?;
    let sock = run_dir.join("host.sock");
    let mut policy = crate::broker::HostPolicy::load(env)?;
    // A child never gets a mode that its parent did not have.
    if let Some(c) = &built.spawn {
        policy.modes = crate::spawn::child_modes(&policy.modes, &c.modes);
        if c.role == crate::spawn::Role::Reviewer {
            policy
                .modes
                .insert("git-push".into(), crate::broker::Mode::Deny);
        }
    }
    let human: std::sync::Arc<dyn crate::broker::Approver> =
        std::sync::Arc::new(crate::broker::HostApprover);
    let approver: std::sync::Arc<dyn crate::broker::Approver> = match built.approver.clone() {
        Some(a) => a,
        None => match crate::approver::resolve(env, interactive) {
            Ok(Some((hook, source))) => {
                eprintln!(
                    "paddock: the agent approver answers host requests for: {}",
                    hook.capabilities.join(", ")
                );
                std::sync::Arc::new(crate::approver::Chain {
                    hook,
                    source,
                    human,
                })
            }
            Ok(None) => human,
            Err(e) => {
                eprintln!("paddock: the agent approver is off for this run: {e:#}");
                human
            }
        },
    };
    // Taken before the sandbox runs: only these remotes count as known.
    // The sandbox sees .git/config read-only, but it can name other remotes
    // with `git -c` or its own ~/.gitconfig.
    let repo = crate::broker::git::Repo::snapshot(&built.spec.project);
    let ctx = crate::broker::Context {
        id: built.spec.id.clone(),
        harness: built.spec.harness.clone(),
        project: built.spec.project.clone(),
        repo: repo.clone(),
        chain: built
            .spawn
            .as_ref()
            .map(|c| c.chain.clone())
            .unwrap_or_default(),
        depth: built.spawn.as_ref().map_or(0, |c| c.depth),
        role: built.spawn.as_ref().map(|c| c.role),
    };
    // Children of this sandbox (`paddock-host spawn`). A bad [spawn] table
    // turns spawn off for this run; the rest of the broker still works.
    let spawner = match crate::spawn::SpawnCfg::load(env) {
        Ok(cfg) => {
            let mut ceiling = crate::spawn::Ceiling::of(built, backend, &policy.modes);
            ceiling.devices =
                crate::devices::names_for(env, built.test_devices, built.device_ceiling.as_deref());
            match crate::spawn::Spawner::new(env, cfg, ceiling, &built.spec.project) {
                Ok(s) => Some(std::sync::Arc::new(s)),
                Err(e) => {
                    log::warn!("spawn is off for this run: {e:#}");
                    None
                }
            }
        }
        Err(e) => {
            eprintln!("paddock: spawn is off for this run: {e:#}");
            None
        }
    };
    let mut broker = crate::broker::Broker::new(env.clone(), ctx, policy, approver);
    broker.spawner = spawner.clone();
    crate::broker::serve(&sock, broker)?;
    built.spec.host_socket = Some(sock);
    // The client is paddock itself. A small wrapper gives it the name that
    // agents use, the same way on every backend.
    let me = std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .context("cannot find the paddock executable")?;
    let wrapper = run_dir.join("paddock-host");
    gui::write_file(&wrapper, "#!/bin/sh\nexec paddock host \"$@\"\n", 0o755)?;
    built
        .spec
        .add_tool("paddock", me, "host passthrough client");
    built
        .spec
        .add_tool("paddock-host", wrapper, "host passthrough client");
    // Plain `git push` and `gh` reach the host too. The sandbox .gitconfig
    // sends pushes to the paddock:: remote helper.
    let helper = run_dir.join("git-remote-paddock");
    gui::write_file(
        &helper,
        "#!/bin/sh\nexec paddock host git-remote-helper \"$@\"\n",
        0o755,
    )?;
    built
        .spec
        .add_tool("git-remote-paddock", helper, "git push through the host");
    let gh = run_dir.join("gh");
    gui::write_file(&gh, "#!/bin/sh\nexec paddock host gh \"$@\"\n", 0o755)?;
    built.spec.add_tool("gh", gh, "gh through the host");
    Ok((repo, spawner))
}

/// The captured output of a sandbox. Everything is read, so the sandbox
/// never blocks on a full pipe, but only the last 4 MiB are kept.
fn read_capped(mut r: impl Read) -> String {
    const KEEP: usize = 4 << 20;
    let mut kept: Vec<u8> = Vec::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        match r.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                kept.extend_from_slice(&buf[..n]);
                if kept.len() > 2 * KEEP {
                    kept.drain(..kept.len() - KEEP);
                }
            }
        }
    }
    if kept.len() > KEEP {
        kept.drain(..kept.len() - KEEP);
    }
    String::from_utf8_lossy(&kept).into_owned()
}

/// Run one provision sandbox to the end. Its output goes to stderr. Returns
/// whether it succeeded.
fn provision(
    env: &HostEnv,
    backend: &dyn Backend,
    spec: &SandboxSpec,
    quiet: bool,
    what: &str,
) -> Result<bool> {
    let l = backend.render(env, spec, false)?;
    write_files(&l)?;
    if !quiet {
        eprintln!("paddock: provisioning: {what}");
    }
    let mut cmd = Command::new(&l.program);
    cmd.args(&l.args)
        .env_clear()
        .envs(&l.env)
        .stdin(Stdio::null());
    if quiet {
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
    } else {
        cmd.stdout(Stdio::from(std::io::stderr()));
    }
    let status = cmd
        .status()
        .with_context(|| format!("cannot start {}", l.program.display()))?;
    Ok(status.success())
}

/// Logins and history synced while the sandbox runs, so the host sees a
/// refreshed token and new sessions before the sandbox exits.
struct LiveSync {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: std::thread::JoinHandle<(
        AuthSync,
        Option<crate::history::CopyBack>,
        state::SyncResult,
        usize,
    )>,
}

/// How often a login is compared, and how often history is copied.
const AUTH_EVERY: Duration = Duration::from_secs(60);
const HISTORY_EVERY: Duration = Duration::from_secs(300);

impl LiveSync {
    fn start(
        mut auth: AuthSync,
        history: Option<crate::history::CopyBack>,
        auth_every: Duration,
        history_every: Duration,
    ) -> Self {
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::spawn(move || {
            let mut synced = state::SyncResult {
                saved: vec![],
                conflicts: vec![],
            };
            let mut copied = 0;
            let (mut auth_at, mut history_at) = (Instant::now(), Instant::now());
            while !flag.load(std::sync::atomic::Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(200));
                if auth_at.elapsed() >= auth_every {
                    auth_at = Instant::now();
                    match auth.sync_running() {
                        Ok(r) => {
                            for f in &r.saved {
                                log::debug!("saved a refreshed login to {}", f.display());
                            }
                            synced.saved.extend(r.saved);
                            synced.conflicts.extend(r.conflicts);
                        }
                        Err(e) => log::debug!("cannot sync the login: {e:#}"),
                    }
                }
                if let Some(h) = history
                    .as_ref()
                    .filter(|_| history_at.elapsed() >= history_every)
                {
                    history_at = Instant::now();
                    match h.copy_out() {
                        Ok(files) => copied += files.len(),
                        Err(e) => log::debug!("cannot copy the session history: {e:#}"),
                    }
                }
            }
            (auth, history, synced, copied)
        });
        Self { stop, thread }
    }

    /// Stop the thread and return what it synced.
    fn finish(
        self,
    ) -> (
        AuthSync,
        Option<crate::history::CopyBack>,
        state::SyncResult,
        usize,
    ) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        self.thread.join().expect("the sync thread does not panic")
    }
}

#[cfg(test)]
mod live_sync_tests {
    use super::*;
    use crate::profile::Profile;
    use std::collections::BTreeMap;

    #[test]
    fn a_refresh_reaches_the_host_before_the_sandbox_exits() {
        let t = tempfile::tempdir().unwrap();
        let host_home = t.path().join("host");
        let sb = t.path().join("sandbox");
        std::fs::create_dir_all(host_home.join(".tool")).unwrap();
        std::fs::create_dir_all(&sb).unwrap();
        std::fs::write(host_home.join(".tool/token"), "t1").unwrap();
        let env = HostEnv::from_parts(host_home.clone(), "demo".into(), 1, 1, BTreeMap::new());
        let p: Profile =
            toml::from_str("name = \"t\"\n[state]\nauth = [\".tool/token\"]\n").unwrap();
        let auth = AuthSync::copy_in(&env, &p, &state::AuthMode::Shared, &sb).unwrap();
        let live = LiveSync::start(auth, None, Duration::ZERO, Duration::MAX);
        std::fs::write(sb.join(".tool/token"), "t2").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while std::fs::read_to_string(host_home.join(".tool/token")).unwrap() != "t2" {
            assert!(
                Instant::now() < deadline,
                "the refresh did not reach the host"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        let (_, _, synced, copied) = live.finish();
        assert_eq!(synced.saved, vec![host_home.join(".tool/token")]);
        assert_eq!(copied, 0);
    }
}
