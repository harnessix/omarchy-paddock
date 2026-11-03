use anyhow::{bail, Result};
use clap::Subcommand;
use paddock::env::HostEnv;
use paddock::state::{self, Running};
use std::path::PathBuf;

use super::storage::stop_one;

#[derive(Subcommand)]
pub(crate) enum PastureCmd {
    /// The pastures of the project, with their members and keys.
    List {
        /// The project folder (default: the current folder).
        #[arg(long)]
        project: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Stop every running member of a pasture.
    Stop {
        name: String,
        #[arg(long)]
        project: Option<PathBuf>,
    },
    /// The workbench of a pasture: a shared terminal (tmux) that belongs to
    /// no member. Every member and you can type into it. Allow it with
    /// workbench = true in [pastures.NAME].
    Workbench {
        name: String,
        #[arg(long)]
        project: Option<PathBuf>,
        /// Start it when it does not run.
        #[arg(long, conflicts_with = "stop")]
        start: bool,
        /// Stop it.
        #[arg(long)]
        stop: bool,
        /// Attach this terminal to it.
        #[arg(long, conflicts_with = "stop")]
        attach: bool,
        #[arg(long)]
        json: bool,
        /// Run the workbench sandbox (paddock starts it so).
        #[arg(long, hide = true)]
        serve: bool,
    },
    /// Delete the data of a pasture: its paddock kv file and its shared
    /// folder. No member may run.
    Remove {
        name: String,
        #[arg(long)]
        project: Option<PathBuf>,
        /// Delete without asking.
        #[arg(long, short = 'y')]
        yes: bool,
    },
}

pub(crate) fn pasture_cmd(env: &HostEnv, cmd: PastureCmd) -> Result<i32> {
    use paddock::kv::{host, shard};
    let project = |p: Option<PathBuf>| host::project_of(p.as_deref());
    match cmd {
        PastureCmd::List { project: p, json } => {
            let project = project(p)?;
            let all = host::pastures(env, &project)?;
            if json {
                let rows: Vec<serde_json::Value> = all
                    .iter()
                    .map(|p| {
                        let mut v = serde_json::to_value(p).unwrap_or_default();
                        if !p.solo {
                            v["folder"] = paddock::pasture::ground(env, &project, &p.name)
                                .display()
                                .to_string()
                                .into();
                        }
                        v
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "api_version": "paddock.pasture/v1", "project": project, "pastures": rows,
                    }))?
                );
                return Ok(0);
            }
            let named: Vec<_> = all.iter().filter(|p| !p.solo).collect();
            if named.is_empty() {
                println!(
                    "No pasture of {} has data or a running member. Define one in {} ([pastures.NAME] members = [...]) and start agents with --pasture NAME.",
                    project.display(),
                    env.paddock_config().join("config.toml").display()
                );
            }
            for p in named {
                let running: Vec<String> = p
                    .members
                    .iter()
                    .filter(|m| m.running)
                    .map(|m| format!("{} ({})", m.harness, m.sandbox))
                    .collect();
                println!(
                    "{:<20} {:>5} keys  {}",
                    p.name,
                    p.keys,
                    if running.is_empty() {
                        "no member runs".to_string()
                    } else {
                        format!("running: {}", running.join(", "))
                    }
                );
                for m in &p.members {
                    if let Some(r) = &m.ration {
                        println!(
                            "{:<20} ration of {} ({}): {}{}{}",
                            "",
                            m.harness,
                            m.sandbox,
                            r.used,
                            if r.hard { ", hard" } else { "" },
                            if r.over { ", over" } else { "" }
                        );
                    }
                }
                for port in &p.ports {
                    println!(
                        "{:<20} port {} shared by {} ({})",
                        "", port.port, port.harness, port.sandbox
                    );
                }
                match &p.workbench {
                    Some(w) if w.running => println!(
                        "{:<20} workbench runs: paddock pasture workbench {} --attach",
                        "", p.name
                    ),
                    Some(w) if w.allowed => println!("{:<20} workbench allowed, not running", ""),
                    _ => {}
                }
            }
            let solo = all.iter().filter(|p| p.solo).count();
            if solo > 0 {
                println!("{solo} sandboxes without a pasture keep data of their own: `paddock kv list` lists them.");
            }
            Ok(0)
        }
        PastureCmd::Stop { name, project: p } => {
            paddock::pasture::check_name(&name)?;
            let project = project(p)?;
            let all = Running::list(env);
            let members = state::pasture_members(&all, &project, &name);
            if members.is_empty() {
                println!("No member of the pasture '{name}' runs.");
                return Ok(0);
            }
            for m in members {
                stop_one(env, &all, &m.id)?;
            }
            Ok(0)
        }
        PastureCmd::Workbench {
            name,
            project: p,
            start,
            stop,
            attach,
            json,
            serve,
        } => {
            paddock::pasture::check_name(&name)?;
            let project = project(p)?;
            workbench_cmd(env, &project, &name, start, stop, attach, json, serve)
        }
        PastureCmd::Remove {
            name,
            project: p,
            yes,
        } => {
            if name.starts_with(paddock::pasture::SOLO_PREFIX) {
                bail!("the data of a sandbox without a pasture goes with its run: `paddock gc` removes it");
            }
            paddock::pasture::check_name(&name)?;
            let project = project(p)?;
            let running = state::pasture_members(&Running::list(env), &project, &name)
                .iter()
                .map(|r| r.id.clone())
                .collect::<Vec<_>>();
            if !running.is_empty() {
                bail!(
                    "members of the pasture '{name}' still run: {}. Stop them first: paddock pasture stop {name}",
                    running.join(", ")
                );
            }
            let file = shard::Ns::pasture(&project, &name)?.file(&paddock::kv::kv_root(env))?;
            let folder = paddock::pasture::folder(env, &project, &name);
            let keys = if file.is_file() {
                paddock::kv::store::Store::open(&file)?.totals()?.0
            } else {
                0
            };
            if !file.is_file() && !folder.exists() {
                println!("The pasture '{name}' of {} has no data.", project.display());
                return Ok(0);
            }
            if !yes {
                println!(
                    "This deletes the pasture '{name}' of {}: {keys} keys in paddock kv ({}) and the shared folder {}. Run it again with --yes to delete.",
                    project.display(),
                    file.display(),
                    folder.display()
                );
                return Ok(1);
            }
            shard::remove_file(&file)?;
            if folder.exists() {
                std::fs::remove_dir_all(&folder)
                    .map_err(|e| anyhow::anyhow!("cannot remove {}: {e}", folder.display()))?;
            }
            println!("Deleted the pasture '{name}' of {}.", project.display());
            Ok(0)
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn workbench_cmd(
    env: &HostEnv,
    project: &std::path::Path,
    name: &str,
    start: bool,
    stop: bool,
    attach: bool,
    json: bool,
    serve: bool,
) -> Result<i32> {
    use paddock::pasture::workbench as wb;
    // An invalid [pastures] is an error here, not "no workbench".
    paddock::pasture::load(env)?;
    if serve {
        return wb::serve(env, project, name);
    }
    if stop {
        let stopped = wb::stop(env, project, name)?;
        if stopped {
            wb::audit(env, project, name, "stop", "user");
        }
        if json {
            println!("{}", serde_json::json!({ "stopped": stopped }));
        } else if stopped {
            println!("Stopped the workbench of the pasture '{name}'.");
        } else {
            println!("The workbench of the pasture '{name}' does not run.");
        }
        return Ok(0);
    }
    if start {
        // The project settings need the approval of the user before a
        // sandbox starts without a terminal.
        let loaded = paddock::manifest::Manifest::load(env, project)?;
        paddock::manifest::require_approval(env, &loaded, false)?;
        if wb::start(env, project, name, "user")? == wb::Started::Now && !json && !attach {
            println!("Started the workbench of the pasture '{name}'.");
        }
    }
    let st = wb::state(env, project, name);
    let sock = wb::socket(env, project, name);
    if attach {
        if !st.running {
            bail!("the workbench of the pasture '{name}' does not run. Start it: paddock pasture workbench {name} --start --attach");
        }
        let Some(tmux) = env.which("tmux") else {
            bail!("tmux is not installed on the host");
        };
        wb::audit(env, project, name, "join", "user");
        // A relative socket path: the host path can be longer than a Unix
        // socket address allows.
        use std::os::unix::process::CommandExt as _;
        let e = std::process::Command::new(tmux)
            .current_dir(wb::dir(env, project, name))
            .args(["-S", wb::SOCKET, "attach", "-t", wb::SESSION])
            .exec();
        bail!("cannot start tmux: {e}");
    }
    if json {
        let mut v = serde_json::to_value(&st)?;
        v["socket"] = sock.display().to_string().into();
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(0);
    }
    match (st.allowed, st.running) {
        (_, true) => {
            println!(
                "The workbench of '{name}' runs. Every member and you can type into it, so each one sees what the others run there."
            );
            println!("Attach: paddock pasture workbench {name} --attach");
            println!("   or: {}", wb::attach_command(&sock));
            println!("Members: paddock-host pasture workbench");
        }
        (true, false) => println!(
            "The workbench of '{name}' does not run. Start it: paddock pasture workbench {name} --start"
        ),
        (false, false) => println!(
            "The pasture '{name}' has no workbench. Allow it in {}:\n\n[pastures.{name}]\nworkbench = true",
            env.paddock_config().join("config.toml").display()
        ),
    }
    Ok(0)
}

pub(crate) fn run(env: &HostEnv, cmd: PastureCmd) -> Result<i32> {
    pasture_cmd(env, cmd)
}
