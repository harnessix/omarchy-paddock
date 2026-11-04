//! `paddock vm`: the guest files and the running VM of the VM backend.

use anyhow::{bail, Result};
use clap::Subcommand;
use paddock::env::HostEnv;
use paddock::vm::assets::{self, Manifest};
use paddock::vm::assets_setup::{self, Source};
use paddock::vm::host;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum VmCmd {
    /// Install the guest files (kernel, root file system). It shows the
    /// files with their size and sha256 and asks first.
    Setup {
        /// Copy the files from a folder that packaging/vm/build.sh made,
        /// and check them against that folder's assets.toml.
        #[arg(long, value_name = "DIR")]
        from: Option<PathBuf>,
        /// Install without asking.
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Show the guest files and the running VM: pid, uptime, runs, project.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Stop the running VM. Its runs end.
    Stop,
    /// Remove the guest files of `paddock vm setup`.
    Remove {
        /// Remove without asking.
        #[arg(long, short = 'y')]
        yes: bool,
    },
}

pub(crate) fn run(env: &HostEnv, cmd: VmCmd) -> Result<i32> {
    match cmd {
        VmCmd::Setup { from, yes } => setup(env, from, yes),
        VmCmd::Status { json } => {
            if json {
                let dir = assets::assets_dir(env);
                let mut v = host::status_json(env);
                v["files"] = match assets::check(&dir) {
                    Ok(a) => serde_json::json!({ "dir": dir, "ok": true, "list": a.files }),
                    Err(e) => {
                        serde_json::json!({ "dir": dir, "ok": false, "error": format!("{e:#}") })
                    }
                };
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                for l in host::status_lines(env)
                    .into_iter()
                    .chain(assets::status_lines(env))
                {
                    println!("{l}");
                }
            }
            Ok(0)
        }
        VmCmd::Stop => {
            let layout = host::Layout::new(env);
            if host::stop(&layout, Duration::from_secs(30))? {
                println!("paddock: the VM stopped");
            } else {
                println!("paddock: no VM runs");
            }
            Ok(0)
        }
        VmCmd::Remove { yes } => {
            refuse_while_running(env, "remove")?;
            let dir = assets::default_assets_dir(env);
            let names = assets_setup::present(&dir);
            if names.is_empty() {
                println!("No VM files in {}.", dir.display());
                return Ok(0);
            }
            println!(
                "paddock vm remove deletes {} from {}.",
                names.join(", "),
                dir.display()
            );
            if !ask(yes, "Remove the VM files?", "paddock vm remove --yes")? {
                return Ok(1);
            }
            assets_setup::remove(env, &dir)?;
            println!("Removed the VM files.");
            Ok(0)
        }
    }
}

fn setup(env: &HostEnv, from: Option<PathBuf>, yes: bool) -> Result<i32> {
    refuse_while_running(env, "setup")?;
    if !cfg!(target_os = "macos") {
        println!(
            "Note: the VM backend runs on macOS only. On Linux, paddock does not use these files."
        );
    }
    let target = assets::default_assets_dir(env);
    let github = paddock::update::GitHub::from_env(env, Duration::from_secs(60));
    let get = |url: &str| paddock::update::Source::download(&github, url);
    let (manifest, src) = match from {
        Some(dir) => {
            let dir = std::path::absolute(&dir)?;
            (Manifest::read(&dir)?, Source::Dir(dir))
        }
        None => (Manifest::parse(assets::PINNED)?, Source::Download(&get)),
    };
    for l in assets_setup::plan_lines(&manifest, &src, &target) {
        println!("{l}");
    }
    let again = match &src {
        Source::Dir(d) => format!("paddock vm setup --from {} --yes", d.display()),
        Source::Download(_) => "paddock vm setup --yes".into(),
    };
    if !ask(yes, "Install these files?", &again)? {
        return Ok(1);
    }
    let a = assets_setup::install(env, &manifest, &src, &target)?;
    println!("Installed the VM files into {}.", a.dir.display());
    Ok(0)
}

fn refuse_while_running(env: &HostEnv, what: &str) -> Result<()> {
    if let Some(r) = host::running(&host::Layout::new(env)) {
        bail!(
            "a VM runs (pid {}): stop it with `paddock vm stop`, then run `paddock vm {what}` again",
            r.helper_pid
        );
    }
    Ok(())
}

/// Ask on the terminal. Without a terminal, print the command with `--yes`
/// and answer no.
fn ask(yes: bool, question: &str, again: &str) -> Result<bool> {
    if yes {
        return Ok(true);
    }
    if !(std::io::stdin().is_terminal() && std::io::stderr().is_terminal()) {
        println!("Run `{again}` to go on.");
        return Ok(false);
    }
    eprint!("{question} [y/N] ");
    std::io::stderr().flush().ok();
    let mut a = String::new();
    std::io::stdin().read_line(&mut a)?;
    if !matches!(a.trim(), "y" | "Y" | "yes") {
        println!("Nothing changed.");
        return Ok(false);
    }
    Ok(true)
}
