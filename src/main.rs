use anyhow::Result;
use clap::Parser;
use paddock::env::HostEnv;

mod cli;
use cli::{Cli, Cmd};

fn main() {
    // The start of a sandbox in a scope of its own (freeze.rs): stop, let
    // paddock move this process into the scope, then become the sandbox.
    // Before anything else, so the environment and the arguments pass as
    // they are.
    let argv: Vec<std::ffi::OsString> = std::env::args_os().collect();
    if argv
        .get(1)
        .is_some_and(|a| a == paddock::freeze::SCOPE_EXEC)
        && argv.len() > 2
    {
        std::process::exit(paddock::freeze::scope_exec(&argv[2..]));
    }
    match real_main() {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("paddock: {e:#}");
            std::process::exit(2);
        }
    }
}

fn real_main() -> Result<i32> {
    // Exit quietly when stdout is closed, for example by `| head`.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    let cli = Cli::parse();
    paddock::logging::init(i32::from(cli.verbose) - i32::from(cli.quiet));
    if let Cmd::Inbound = &cli.cmd {
        return cli::app::inbound();
    }
    if let Cmd::Portal = &cli.cmd {
        return cli::app::portal();
    }
    if let Cmd::Host { args } = &cli.cmd {
        return Ok(paddock::hostcli::main(args));
    }
    // Before HostEnv::detect: the pane takes the caller's environment.
    if let Cmd::DetachRun { job } = &cli.cmd {
        return cli::run::detach_run(job);
    }
    if let Cmd::Relay {
        listen,
        socket,
        forward,
        ready_file,
    } = &cli.cmd
    {
        return cli::run::relay(listen, socket, forward, ready_file.as_deref());
    }
    let env = HostEnv::detect()?;
    paddock::privfs::secure_roots(&env);
    // A harness start: bring the menu and the shims up to date in the
    // background when harnesses or paddock changed.
    if matches!(
        cli.cmd,
        Cmd::Run(_) | Cmd::External(_) | Cmd::Wrap { .. } | Cmd::Shell(_)
    ) {
        paddock::refresh::spawn_if_changed(&env);
    }
    match cli.cmd {
        Cmd::Relay { .. }
        | Cmd::Host { .. }
        | Cmd::Inbound
        | Cmd::Portal
        | Cmd::DetachRun { .. } => unreachable!(),
        Cmd::SpawnChild { job } => cli::run::spawn_child(&env, job),
        Cmd::Approvals { json, cmd } => cli::approver::approvals(&env, json, cmd),
        Cmd::Worktree { cmd } => cli::worktree::run(&env, cmd),
        Cmd::Log { json, lines } => cli::approver::log(&env, json, lines),
        Cmd::Approver { cmd } => cli::approver::run(&env, cmd),
        Cmd::App { cmd } => cli::app::run(&env, cmd),
        Cmd::Skill { cmd } => cli::skill::run(&env, cmd),
        Cmd::Run(a) => cli::run::run(&env, a),
        Cmd::Shell(a) => cli::run::shell(&env, a),
        Cmd::Explain { run, json } => cli::run::explain(&env, &run, json).map(|_| 0),
        Cmd::External(argv) => cli::run::external(&env, argv),
        Cmd::Mcp { harness, path } => cli::catalog::mcp(&env, harness, path),
        Cmd::Env { cmd } => cli::env::run(&env, cmd),
        Cmd::List { json } => cli::catalog::list(&env, json),
        Cmd::Recent { json } => cli::catalog::recent(&env, json),
        Cmd::Pick { harness } => cli::catalog::pick_cmd(&env, harness),
        Cmd::Tools { cmd } => cli::tools::run(&env, cmd),
        Cmd::Gateway { cmd } => cli::gateway::run(&env, cmd),
        Cmd::Gatewayd { idle } => cli::gateway::gatewayd(&env, idle),
        Cmd::Refresh { background } => cli::shims::refresh(&env, background),
        Cmd::OmarchyUsageUpdate { args } => cli::shims::omarchy_usage_update(&env, args),
        Cmd::Wrap { command } => cli::run::wrap(env, command),
        Cmd::Shims { cmd } => cli::shims::run(&env, cmd),
        Cmd::Launchers { cmd } => cli::shims::launchers(&env, cmd),
        Cmd::Windows { json, mark, focus } => cli::windows::windows(&env, json, mark, focus),
        Cmd::Focus { id, display } => cli::windows::focus(&env, id, display),
        Cmd::Freeze { id, tree, reason } => cli::status::freeze(&env, id, tree, reason),
        Cmd::Thaw { id, tree } => cli::status::thaw(&env, id, tree),
        Cmd::Ps { json } => cli::status::ps(&env, json),
        Cmd::Files { id, json } => cli::storage::files_cmd(&env, id.as_deref(), json),
        Cmd::Undo {
            session,
            list,
            yes,
            force,
        } => cli::storage::undo(&env, session, list, yes, force),
        Cmd::Kv { cmd } => cli::kv::run(&env, cmd),
        Cmd::Du { json } => cli::storage::du_cmd(&env, json),
        Cmd::Gc {
            yes,
            homes,
            older_than,
            include_app_logins,
            worktrees,
            force,
            scratch,
            dedupe,
            snapshots,
            json,
        } => cli::storage::gc(
            &env,
            yes,
            homes,
            older_than,
            include_app_logins,
            worktrees,
            force,
            scratch,
            dedupe,
            snapshots,
            json,
        ),
        Cmd::DedupeHome { home } => cli::storage::dedupe_home(&env, &home),
        Cmd::PromoteCaches {
            project,
            crates_index,
            sumdb,
        } => cli::storage::promote_caches(&env, &project, crates_index, sumdb),
        Cmd::Recover {
            id,
            paths,
            all,
            to,
            keep,
        } => cli::storage::recover_cmd(&env, &id, paths, all, to.as_deref(), keep),
        Cmd::Discard { id, paths, all } => cli::storage::discard_cmd(&env, &id, paths, all),
        Cmd::Pasture { cmd } => cli::pasture::run(&env, cmd),
        Cmd::Forage {
            agents,
            refresh,
            json,
        } => cli::forage::run(&env, agents, refresh, json),
        Cmd::Monitor { cmd } => cli::monitor::run(&env, cmd),
        Cmd::Devices { cmd } => cli::devices::run(&env, cmd),
        Cmd::Timer { cmd } => cli::timer::run(&env, cmd),
        Cmd::Logins { cmd } => cli::logins::run(&env, cmd),
        Cmd::Credentials { cmd } => cli::credentials::run(&env, cmd),
        Cmd::Boundary { id, json, follow } => cli::status::boundary(&env, id, json, follow),
        Cmd::Stop { id } => cli::storage::stop(&env, &id),
        Cmd::Wait { id, timeout } => cli::status::wait(&env, id, timeout),
        Cmd::View { id, write } => cli::status::view(&env, id, write),
        Cmd::Doctor {
            escape,
            backend,
            network,
            gui,
            window,
            app,
            bus,
            online,
            env: env_mode,
            report,
            output,
        } => cli::setup::doctor(
            &env, escape, backend, network, gui, window, app, bus, env_mode, online, report, output,
        ),
        Cmd::Setup {
            yes,
            shims,
            no_shims,
            launchers,
            no_launchers,
            plugin,
            no_plugin,
            backend,
            apps,
        } => cli::setup::setup(
            &env,
            yes,
            shims,
            no_shims,
            launchers,
            no_launchers,
            plugin,
            no_plugin,
            backend,
            apps,
        ),
        Cmd::Update { check, yes } => cli::update::run(&env, check, yes),
        Cmd::Auth { cmd } => cli::auth::run(&env, cmd),
        Cmd::Approve { path } => cli::setup::approve(&env, path),
        Cmd::Init { path } => cli::setup::init(path),
    }
}
