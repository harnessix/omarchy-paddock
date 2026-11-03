use anyhow::{bail, Result};
use clap::Subcommand;
use paddock::env::HostEnv;

#[derive(Subcommand)]
pub(crate) enum ShimsCmd {
    /// Write the shell functions, and source them from ~/.bashrc and ~/.zshrc.
    /// With --scope omarchy-agent: only the agents that Omarchy's
    /// omarchy-agent starts (its key, menu, agents panel and `a` alias).
    Install {
        /// shell (the default): typed commands in bash and zsh.
        /// omarchy-agent: only the starts from omarchy-agent.
        #[arg(long, default_value = "shell", value_parser = ["shell", "omarchy-agent"])]
        scope: String,
    },
    /// Remove the functions, the lines that `install` added, and the
    /// omarchy-agent scope.
    Remove,
    /// Show the commands that `install` would cover, and whether they are on.
    List,
    /// Run a program that a scope takes over (from paddock's mise wrapper).
    #[command(hide = true)]
    Exec {
        program: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// The omarchy-launch-tui shim of the omarchy-agent scope.
    #[command(hide = true)]
    LaunchTui {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

#[derive(Subcommand)]
pub(crate) enum LaunchersCmd {
    /// Write the entries, the icon and the Paddock folder.
    Install,
    /// Remove everything that `install` wrote.
    Remove,
    /// Show what `install` would create.
    List,
}

pub(crate) fn run(env: &HostEnv, cmd: ShimsCmd) -> Result<i32> {
    match cmd {
        ShimsCmd::Exec { program, args } => {
            if program == paddock::omarchy::USAGE {
                return paddock::omarchy_usage::update(env, &args);
            }
            if program != paddock::omarchy::SCOPE {
                bail!(
                    "paddock shims exec only runs omarchy-agent and {}",
                    paddock::omarchy::USAGE
                );
            }
            paddock::omarchy::exec_agent(env, &args)
        }
        ShimsCmd::LaunchTui { args } => paddock::omarchy::launch_tui(env, &args),
        ShimsCmd::Install { scope } if scope == "omarchy-agent" => {
            use paddock::omarchy;
            let r = omarchy::install(env)?;
            println!(
                "Added the omarchy-agent wrapper to {} (one marked block).",
                r.config.display()
            );
            println!("Wrote the shim folder {}.", r.shims.display());
            println!(
                "The same block wraps {}: the agents panel's usage scripts run in a box that sees the harness folders of the host and of the sandbox homes, read-only, and nothing else of the home.",
                omarchy::USAGE
            );
            match paddock::omarchy_usage::install(env) {
                Ok(_) => println!(
                    "Added one marked block to {}: the agents panel runs its usage update inside paddock's usage box. It takes effect at your next login.",
                    paddock::omarchy_usage::hypr_config(env).display()
                ),
                Err(e) => println!("The agents panel keeps checking limits on the host: {e:#}"),
            }
            if let Err(e) = &r.reshim {
                println!("mise reshim did not run: {e}. Run it yourself, or the key keeps starting agents without a sandbox.");
            }
            println!("\nAgents that omarchy-agent starts (its key, the menu, the agents panel, `a`) now run in a sandbox.");
            println!(
                "Commands that you type and scripts do not change. Undo with: paddock shims remove"
            );
            if let Some(agent) = omarchy::default_agent(env) {
                if paddock::wrap::profile_for(env, &agent).is_none() {
                    println!("Your default agent {agent} has no paddock profile yet: it still starts without a sandbox.");
                }
            }
            Ok(0)
        }
        cmd => {
            use paddock::shims;
            match cmd {
                ShimsCmd::Exec { .. } | ShimsCmd::LaunchTui { .. } => unreachable!(),
                ShimsCmd::List => {
                    let s = paddock::omarchy::status(env);
                    println!(
                        "scope omarchy-agent: {}",
                        if s.on() {
                            "on, agents that omarchy-agent starts run in a sandbox"
                        } else if s.user_wrapper {
                            "off, your mise config has its own wrapper for omarchy-agent or omarchy-agent-usage-update"
                        } else if s.block || s.shims {
                            "incomplete, run paddock shims install --scope omarchy-agent again"
                        } else {
                            "off"
                        }
                    );
                    if s.on() {
                        println!(
                            "  {}: wrapped, the agents panel's usage scripts run in the usage box",
                            paddock::omarchy::USAGE
                        );
                    }
                    println!("scope shell:");
                    let on = shims::installed(env);
                    let active = shims::active(env);
                    for (harness, cmd) in shims::detect(env)? {
                        let state = if on && active.contains(&cmd) {
                            "on "
                        } else {
                            "off"
                        };
                        println!("{state}  {cmd:<10} paddock run {harness}");
                    }
                    if on {
                        println!("\nShims are on. Run one command without the sandbox with PADDOCK_BYPASS=1. Turn them off with: paddock shims remove");
                    } else {
                        println!("\nShims are off. Turn them on with: paddock shims install");
                    }
                }
                ShimsCmd::Install { .. } => {
                    let r = shims::install(env)?;
                    for (harness, cmd) in &r.shims {
                        println!("{cmd:<10} paddock run {harness}");
                    }
                    if r.rc_files.is_empty() {
                        println!("\nNo ~/.bashrc or ~/.zshrc found. Source {} from your shell configuration.", shims::script_file(env).display());
                    } else {
                        for rc in &r.rc_files {
                            println!("\nAdded one line to {}.", rc.display());
                        }
                        println!("Open a new shell to use them. Run a command without the sandbox with PADDOCK_BYPASS=1. Undo with: paddock shims remove");
                    }
                }
                ShimsCmd::Remove => {
                    for p in shims::remove(env)? {
                        println!("removed {}", p.display());
                    }
                    let (removed, reshim) = paddock::omarchy::remove(env)?;
                    for p in removed
                        .into_iter()
                        .chain(paddock::omarchy_usage::remove(env)?)
                    {
                        println!("removed {}", p.display());
                    }
                    if let Some(Err(e)) = reshim {
                        println!("mise reshim did not run: {e}. Run it yourself to remove the omarchy-agent dispatch link.");
                    }
                    println!("Open a new shell, or run `unset -f` on the commands, to stop using them in this shell.");
                }
            }
            Ok(0)
        }
    }
}

pub(crate) fn launchers(env: &HostEnv, cmd: LaunchersCmd) -> Result<i32> {
    use paddock::launchers::{self, Kind};
    match cmd {
        LaunchersCmd::List => {
            for e in launchers::detect(env)? {
                let kind = if e.kind == Kind::Cli {
                    "terminal"
                } else {
                    "app"
                };
                println!("{:<10} {:<9} {}", e.name, kind, e.label);
            }
        }
        LaunchersCmd::Install => {
            let r = launchers::install(env)?;
            for e in &r.entries {
                println!("{}  {}", e.label, e.desktop_file.display());
            }
            match r.omarchy_menu {
                        Some(f) => println!("\nThe Paddock folder is in the Omarchy menu (Super+Space), from {}.", f.display()),
                        None => println!("\nThe Paddock folder is in the menu of desktops that follow the XDG menu standard."),
                    }
            println!("Search for \"paddock\" in your app launcher to see every entry. Undo with: paddock launchers remove");
        }
        LaunchersCmd::Remove => {
            let removed = launchers::remove(env)?;
            if removed.is_empty() {
                println!("Nothing to remove.");
            }
            for f in removed {
                println!("removed {}", f.display());
            }
        }
    }
    Ok(0)
}

pub(crate) fn refresh(env: &HostEnv, background: bool) -> Result<i32> {
    let r = paddock::refresh::sync(env, !background)?;
    if !background {
        match &r.launchers {
            Some(l) => println!("menu entries: {}", l.join(", ")),
            None => println!("menu entries: off (paddock launchers install)"),
        }
        match &r.shims {
            Some(s) => println!("shell shims: {}", s.join(", ")),
            None => println!("shell shims: off (paddock shims install)"),
        }
    }
    Ok(0)
}

pub(crate) fn omarchy_usage_update(env: &HostEnv, args: Vec<String>) -> Result<i32> {
    paddock::omarchy_usage::update(env, &args)
}
