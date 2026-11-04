//! The hidden commands of the VM backend: `vm-agent` (in the guest),
//! `vm-attach` and `vm-serve` (on the host).

use anyhow::Result;
use paddock::env::HostEnv;
use paddock::vm::{agent, attach, host};
use std::path::PathBuf;

/// The arguments of `paddock vm-agent`.
#[derive(clap::Args)]
pub(crate) struct AgentArgs {
    /// Tests only: unix sockets `DIR/PORT.sock` instead of vsock, no
    /// mounts, and the runs as the agent's own user.
    #[arg(long, value_name = "DIR")]
    unix: Option<PathBuf>,
    /// Tests only: the paddock program of the runs.
    #[arg(long, requires = "unix")]
    paddock: Option<PathBuf>,
    /// Tests only: the home of the runs.
    #[arg(long, requires = "unix")]
    home: Option<PathBuf>,
    /// The guest user of the runs. The default is the owner of the
    /// project folder, which is the host user.
    #[arg(long)]
    uid: Option<u32>,
    #[arg(long, requires = "uid")]
    gid: Option<u32>,
}

pub(crate) fn vm_agent(a: AgentArgs) -> Result<i32> {
    // main() restores the default SIGPIPE, which ends the process on a
    // write to a closed socket. The agent serves many connections: a host
    // that goes away must end only its own connection, not the agent and
    // with it the VM. The standard library gives the runs the default
    // SIGPIPE again.
    // SAFETY: setting a signal disposition at the start, before threads.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };
    let me = std::env::current_exe()?;
    let cfg = match a.unix {
        Some(dir) => agent::Config {
            run_dir: dir.join("runs"),
            transport: agent::Transport::Unix(dir),
            paddock: a.paddock.unwrap_or(me),
            user: None,
            project_owner: false,
            mounts: false,
            home: a.home,
        },
        None => agent::Config {
            transport: agent::Transport::Vsock,
            paddock: me,
            user: a.uid.map(|u| (u, a.gid.unwrap_or(u))),
            project_owner: a.uid.is_none(),
            mounts: true,
            run_dir: paddock::vm::agent_run::run_dir(),
            home: None,
        },
    };
    agent::serve(agent::Agent::new(cfg))?;
    Ok(0)
}

pub(crate) fn vm_attach(env: &HostEnv, job: PathBuf) -> Result<i32> {
    attach::main(env, &job)
}

pub(crate) fn vm_serve(env: &HostEnv, project: PathBuf) -> Result<i32> {
    host::serve(env, &project)
}
