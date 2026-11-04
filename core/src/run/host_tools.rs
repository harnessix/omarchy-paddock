//! The host passthrough commands in the sandbox: paddock-host, the git
//! remote helper, gh, and tea and glab when the host has them.

use crate::build::Built;
use crate::env::HostEnv;
use crate::gui;
use anyhow::{Context, Result};
use std::path::Path;

/// The broker of the run. In the guest of the VM backend, the broker of
/// the host run answers: the agent passes its socket on to the sandbox.
pub(super) fn broker(
    env: &HostEnv,
    built: &mut Built,
    interactive: bool,
    backend: &str,
) -> Result<super::BrokerStart> {
    let Some(guest) = crate::vm::agent::guest_socket(crate::vm::agent::HOST_SOCKET_VAR) else {
        return super::start_broker(env, built, interactive, backend);
    };
    let run_dir = env.paddock_run().join(&built.spec.id);
    crate::privfs::dir(&run_dir)?;
    let repo = crate::broker::git::Repo::snapshot(&built.spec.project);
    built.spec.host_socket = Some(guest);
    add(env, built, &run_dir)?;
    Ok((repo, None, None))
}

pub(super) fn add(env: &HostEnv, built: &mut Built, run_dir: &Path) -> Result<()> {
    // The client is paddock itself. A small wrapper gives it the name that
    // agents use, the same way on every backend.
    let me = std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .context("cannot find the paddock executable")?;
    let wrapper = run_dir.join("paddock-host");
    gui::write_file(&wrapper, "#!/bin/sh\nexec paddock host \"$@\"\n", 0o755)?;
    built
        .spec
        .add_tool("paddock", me, "host passthrough client");
    built
        .spec
        .add_tool("paddock-host", wrapper, "host passthrough client");
    // Plain `git push` and `gh` reach the host too. The sandbox .gitconfig
    // sends pushes to the paddock:: remote helper.
    let helper = run_dir.join("git-remote-paddock");
    gui::write_file(
        &helper,
        "#!/bin/sh\nexec paddock host git-remote-helper \"$@\"\n",
        0o755,
    )?;
    built
        .spec
        .add_tool("git-remote-paddock", helper, "git push through the host");
    let gh = run_dir.join("gh");
    gui::write_file(&gh, "#!/bin/sh\nexec paddock host gh \"$@\"\n", 0o755)?;
    built.spec.add_tool("gh", gh, "gh through the host");
    // tea and glab only when the user has them: without them the sandbox
    // sees "not found", as before.
    for tool in ["tea", "glab"] {
        if env.which(tool).is_some() {
            let w = run_dir.join(tool);
            gui::write_file(
                &w,
                &format!("#!/bin/sh\nexec paddock host {tool} \"$@\"\n"),
                0o755,
            )?;
            built
                .spec
                .add_tool(tool, w, &format!("{tool} through the host"));
        }
    }
    Ok(())
}
