use anyhow::Result;
use clap::Subcommand;
use paddock::env::HostEnv;

#[derive(Subcommand)]
pub(crate) enum ToolsCmd {
    /// Print a [host.tools.NAME] block for each exec command line that you
    /// allowed at least 3 times. It never changes the configuration.
    Suggest,
}

pub(crate) fn run(env: &HostEnv, cmd: ToolsCmd) -> Result<i32> {
    match cmd {
        ToolsCmd::Suggest => {
            let log = std::fs::read_to_string(paddock::broker::audit_file(env)).unwrap_or_default();
            let known = paddock::hosttools::load(env)?;
            let out = paddock::hosttools::suggest(&log, &known);
            let remember = paddock::execbox::ExecCfg::load(env)?.remember_allowed;
            if out.is_empty() && !remember {
                println!("No suggestion: the audit log keeps no exec command lines. Set remember_allowed = true in [exec] of your paddock config.toml to keep allowed command lines of plain words, then ask again later.");
            } else if out.is_empty() {
                println!(
                    "No suggestion: no exec command line was allowed {} times or more.",
                    paddock::hosttools::SUGGEST_MIN
                );
            } else {
                println!("# Copy the blocks that you want into your paddock config.toml.\n# Add args = [...] for the argument shapes. mode = \"allow\" runs without a card.\n");
                print!("{out}");
            }
            Ok(0)
        }
    }
}
