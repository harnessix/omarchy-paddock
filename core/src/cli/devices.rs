use anyhow::Result;
use clap::Subcommand;
use paddock::env::HostEnv;
use std::path::PathBuf;

#[derive(Subcommand)]
pub(crate) enum DevicesCmd {
    /// Raise the budget of a paid device for a run (a sandbox and the
    /// sandboxes it started).
    Budget {
        id: String,
        device: String,
        /// The amount to add, for example 1.0.
        #[arg(long)]
        add: String,
    },
    /// What paid devices spent, by sandbox and device.
    Ledger {
        #[arg(long)]
        json: bool,
        /// Only requests of this project folder.
        #[arg(long)]
        project: Option<PathBuf>,
        /// Only requests of this recent time, for example 7d or 12h.
        #[arg(long)]
        since: Option<String>,
    },
}

pub(crate) fn run(env: &HostEnv, cmd: DevicesCmd) -> Result<i32> {
    use paddock::devices::ledger;
    match cmd {
        DevicesCmd::Budget { id, device, add } => {
            let id = paddock::ids::resolve(env, &id)?;
            let add = ledger::parse_amount(&add)?;
            let (root, budget) = ledger::raise(env, &id, &device, add)?;
            println!(
                "The budget of {device} for {root} is now {}. The next request can pass.",
                ledger::money(budget)
            );
            Ok(0)
        }
        DevicesCmd::Ledger {
            json,
            project,
            since,
        } => {
            let project = project
                .map(|p| paddock::kv::host::project_of(Some(&p)))
                .transpose()?;
            let since = since.map(|s| ledger::parse_since(&s)).transpose()?;
            print!("{}", ledger::report(env, project.as_deref(), since, json)?);
            Ok(0)
        }
    }
}
