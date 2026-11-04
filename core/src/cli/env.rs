use anyhow::{bail, Result};
use clap::Subcommand;
use paddock::build;
use paddock::env::HostEnv;
use paddock::manifest::Manifest;
use paddock::state;
use std::path::PathBuf;

#[derive(Subcommand)]
pub(crate) enum EnvCmd {
    /// List host variables by what a sandbox does with them. Names only.
    Show { path: Option<PathBuf> },
    /// Stop passing these variables (saved in the user configuration).
    Hide { names: Vec<String> },
    /// Pass these variables again.
    Unhide { names: Vec<String> },
}

pub(crate) fn run(env: &HostEnv, cmd: EnvCmd) -> Result<i32> {
    match cmd {
        EnvCmd::Hide { names } | EnvCmd::Unhide { names } if names.is_empty() => {
            bail!("name at least one variable")
        }
        EnvCmd::Hide { names } => {
            let all = state::edit_env_hide(env, &names, &[])?;
            println!("Hidden from sandboxes: {}", all.join(", "));
        }
        EnvCmd::Unhide { names } => {
            let all = state::edit_env_hide(env, &[], &names)?;
            println!(
                "Hidden from sandboxes: {}",
                if all.is_empty() {
                    "(none)".into()
                } else {
                    all.join(", ")
                }
            );
        }
        EnvCmd::Show { path } => {
            let project = build::resolve_project(path.as_deref())?;
            let m = Manifest::load(env, &project)?.merged;
            let inherit = m.env.inherit.unwrap_or(true);
            let (mut secret, mut plain, mut hidden, mut never, mut cloud) =
                (vec![], vec![], vec![], vec![], vec![]);
            for (k, v) in &env.vars {
                if !paddock::policy::inheritable_var(k, v) {
                    never.push(k.clone());
                } else if m.env.hide.contains(k) {
                    hidden.push(k.clone());
                } else if paddock::policy::is_cloud_secret(k) && !m.env.pass.contains(k) {
                    cloud.push(k.clone());
                } else if paddock::policy::looks_secret(k) && !m.env.pass.contains(k) {
                    secret.push(k.clone());
                } else {
                    plain.push(k.clone());
                }
            }
            println!(
                "mode: {}",
                if inherit {
                    "inherit (the default)"
                } else {
                    "clean: only the allowlist passes"
                }
            );
            if inherit {
                println!("\npassed ({}):\n  {}", plain.len(), plain.join(" "));
                println!(
                            "\nnot passed: look like secrets; a harness gets its own login variables, and [env] pass hands over others ({}):\n  {}",
                            secret.len(),
                            secret.join(" ")
                        );
            }
            println!(
                "\nhidden by [env] hide ({}):\n  {}",
                hidden.len(),
                hidden.join(" ")
            );
            println!(
                "\nnot passed: cloud credentials; add to [env] pass to allow ({}):\n  {}",
                cloud.len(),
                cloud.join(" ")
            );
            println!(
                "\nnever passed: host sockets and values the sandbox sets ({}):\n  {}",
                never.len(),
                never.join(" ")
            );
        }
    }
    Ok(0)
}
