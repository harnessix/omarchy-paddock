//! `paddock-vm`: the helper that owns the Linux VM of a paddock sandbox on
//! macOS. It needs the `com.apple.security.virtualization` entitlement
//! (packaging/macos/paddock-vm.entitlements).
//!
//! Exit codes: 0 when the guest stops, 1 on an error, 2 on a usage error,
//! 3 after a hard stop.

use clap::Parser;
use paddock::vm::helper::{self, Cli, Cmd};

fn main() {
    let cli = Cli::parse();
    paddock::logging::init(0);
    match run(cli) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("paddock-vm: {e:#}");
            std::process::exit(1);
        }
    }
}

fn run(cli: Cli) -> anyhow::Result<i32> {
    let Cmd::Start(args) = cli.cmd;
    let env = paddock::env::HostEnv::detect()?;
    let plan = helper::plan(&args, &env.home)?;
    boot(&plan)
}

#[cfg(target_os = "macos")]
fn boot(plan: &helper::Plan) -> anyhow::Result<i32> {
    paddock::vm::vz::run(plan)
}

#[cfg(not(target_os = "macos"))]
fn boot(_plan: &helper::Plan) -> anyhow::Result<i32> {
    anyhow::bail!("the paddock VM runs only on macOS")
}
