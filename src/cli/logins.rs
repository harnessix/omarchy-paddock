use anyhow::Result;
use clap::Subcommand;
use paddock::env::HostEnv;

#[derive(Subcommand)]
pub(crate) enum LoginsCmd {
    /// Refresh a login now, in a keeper sandbox, and update the running
    /// sandboxes that use it.
    Refresh {
        harness: String,
        /// shared (default), isolated or isolated:NAME.
        #[arg(long, default_value = "shared")]
        auth: String,
    },
}

pub(crate) fn run(env: &HostEnv, cmd: LoginsCmd) -> Result<i32> {
    match cmd {
        LoginsCmd::Refresh { harness, auth } => {
            println!(
                "{}",
                paddock::keeper::refresh_now(env, &harness, &auth.parse()?)?
            );
            Ok(0)
        }
    }
}
