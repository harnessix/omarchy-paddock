use anyhow::Result;
use clap::Subcommand;
use paddock::env::HostEnv;

#[derive(Subcommand)]
pub(crate) enum GatewayCmd {
    /// Write the systemd user units and turn the socket on.
    Install,
    /// Turn the socket off and remove the units that paddock wrote.
    Remove,
    /// How the executor starts.
    Status,
}

pub(crate) fn run(env: &HostEnv, cmd: GatewayCmd) -> Result<i32> {
    use paddock::gatewayd;
    match cmd {
        GatewayCmd::Install => {
            for p in gatewayd::install(env)? {
                println!("wrote {}", p.display());
            }
            println!("systemd now starts the gateway executor on the first request. Undo with: paddock gateway remove");
        }
        GatewayCmd::Remove => {
            for p in gatewayd::remove(env)? {
                println!("removed {}", p.display());
            }
            println!("paddock starts the gateway executor itself when a request needs it.");
        }
        GatewayCmd::Status => println!("{}", gatewayd::status_line(env)),
    }
    Ok(0)
}

pub(crate) fn gatewayd(env: &HostEnv, idle: u64) -> Result<i32> {
    paddock::gatewayd::main(env, std::time::Duration::from_secs(idle))
}
