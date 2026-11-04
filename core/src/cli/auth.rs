use anyhow::{bail, Result};
use clap::Subcommand;
use paddock::env::HostEnv;
use paddock::profile::HarnessFactory;
use paddock::state;

#[derive(Subcommand)]
pub(crate) enum AuthCmd {
    /// Copy a harness's credential files from your home directory into an isolated store.
    Import {
        harness: String,
        /// isolated or isolated:NAME.
        #[arg(long, default_value = "isolated")]
        auth: String,
    },
}

pub(crate) fn run(env: &HostEnv, cmd: AuthCmd) -> Result<i32> {
    match cmd {
        AuthCmd::Import { harness, auth } => {
            let f = HarnessFactory::new(env)?;
            let Some(p) = f.profile(&harness) else {
                bail!("unknown harness '{harness}'")
            };
            let done = state::auth_import(env, p, &auth.parse()?)?;
            if done.is_empty() {
                println!("No credential files found for {harness}. Log in inside the sandbox instead: paddock {harness}");
            }
            for d in done {
                println!("Imported {}", d.display());
            }
            Ok(0)
        }
    }
}
