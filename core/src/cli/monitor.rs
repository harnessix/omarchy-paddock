use anyhow::{bail, Result};
use clap::Subcommand;
use paddock::env::HostEnv;
use paddock::state::{self, Running};
use std::path::PathBuf;

#[derive(Subcommand, Debug, PartialEq)]
pub(crate) enum MonitorCmd {
    /// The findings of every running sandbox.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Say OK to the findings of a sandbox: the panel mark goes away until
    /// a new finding. The audit log keeps them.
    Ack { id: String },
    /// No notification and no panel mark for a project and its folders.
    /// The audit log still gets the findings.
    Mute { path: PathBuf },
    /// Notify about a muted project again.
    Unmute { path: PathBuf },
}

pub(crate) const MONITOR_HELP: &str =
    "The muted projects are in [monitor] mute of ~/.config/paddock/config.toml.
mute and unmute change only that list and keep the rest of the file.
A project's own paddock.toml cannot mute it.

Examples:
  paddock monitor list
  paddock monitor ack 1a2b3c
  paddock monitor mute ~/src/tunnel-demo";

pub(crate) fn monitor_cmd(env: &HostEnv, cmd: MonitorCmd) -> Result<i32> {
    use paddock::monitor;
    // A path as given, made absolute; the real path when it exists.
    let absolute = |p: PathBuf| -> Result<PathBuf> {
        let p = if p.is_absolute() {
            p
        } else {
            std::env::current_dir()?.join(p)
        };
        Ok(std::fs::canonicalize(&p).unwrap_or(p))
    };
    match cmd {
        MonitorCmd::List { json } => {
            let mute = monitor::load_mute(env);
            let list = state::tree_order(Running::list(env));
            if json {
                let rows: Vec<serde_json::Value> = list
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "id": r.id,
                            "short": paddock::ids::short(&r.id),
                            "harness": r.harness,
                            "project": r.project,
                            "muted": monitor::is_muted(&mute, &r.project),
                            "findings": monitor::findings(env, &r.id),
                        })
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::json!({ "api_version": "paddock.monitor/v1", "sandboxes": rows })
                );
                return Ok(0);
            }
            let mut any = false;
            for r in &list {
                let found = monitor::findings(env, &r.id);
                if found.is_empty() {
                    continue;
                }
                any = true;
                let muted = if monitor::is_muted(&mute, &r.project) {
                    "  (muted)"
                } else {
                    ""
                };
                println!("{}  {}  {}{muted}", r.id, r.harness, r.project.display());
                for f in found {
                    let ok = if f.acked { "  (OK)" } else { "" };
                    println!("  {}{ok}", f.text());
                }
            }
            if !any {
                println!("No findings in the running sandboxes.");
            }
            Ok(0)
        }
        MonitorCmd::Ack { id } => {
            let id = paddock::ids::resolve(env, &id)?;
            if !Running::list(env).iter().any(|r| r.id == id) {
                bail!("no running sandbox {id}. paddock ps lists them.");
            }
            let n = monitor::ack(env, &id)?;
            println!("OK to {n} finding(s) of {id}. The audit log keeps them.");
            Ok(0)
        }
        MonitorCmd::Mute { path } => {
            let path = absolute(path)?;
            let list = monitor::edit_mute(env, &path, true)?;
            println!(
                "Muted {}: no notification and no panel mark. Muted projects: {}.",
                path.display(),
                list.len()
            );
            Ok(0)
        }
        MonitorCmd::Unmute { path } => {
            let path = absolute(path)?;
            let before = monitor::load_mute(env).len();
            let list = monitor::edit_mute(env, &path, false)?;
            if list.len() == before {
                println!("{} was not muted.", path.display());
                return Ok(1);
            }
            println!("Unmuted {}.", path.display());
            Ok(0)
        }
    }
}

pub(crate) fn run(env: &HostEnv, cmd: MonitorCmd) -> Result<i32> {
    monitor_cmd(env, cmd)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Cli, Cmd};
    use clap::Parser;

    fn monitor(args: &[&str]) -> Result<MonitorCmd, clap::Error> {
        let cli = Cli::try_parse_from(["paddock", "monitor"].iter().chain(args))?;
        match cli.cmd {
            Cmd::Monitor { cmd } => Ok(cmd),
            _ => panic!("not a monitor command"),
        }
    }

    #[test]
    fn the_monitor_commands_parse() {
        assert_eq!(
            monitor(&["list"]).unwrap(),
            MonitorCmd::List { json: false }
        );
        assert_eq!(
            monitor(&["list", "--json"]).unwrap(),
            MonitorCmd::List { json: true }
        );
        assert_eq!(
            monitor(&["ack", "claude-app-1"]).unwrap(),
            MonitorCmd::Ack {
                id: "claude-app-1".into()
            }
        );
        assert_eq!(
            monitor(&["mute", "/work/app"]).unwrap(),
            MonitorCmd::Mute {
                path: "/work/app".into()
            }
        );
        assert_eq!(
            monitor(&["unmute", "/work/app"]).unwrap(),
            MonitorCmd::Unmute {
                path: "/work/app".into()
            }
        );
        assert!(monitor(&["ack"]).is_err());
        assert!(monitor(&["mute"]).is_err());
        assert!(monitor(&["silence", "/a"]).is_err());
    }
}
