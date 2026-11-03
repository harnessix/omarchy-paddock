//! The mounts and variables of a pasture member, set at the end of a
//! build so that nothing from the host or the project replaces them.

use crate::env::HostEnv;
use crate::spec::{Mount, SandboxSpec};
use anyhow::Result;
use std::path::Path;

/// The shared folder, the workbench sockets and the ports of pasture
/// `name` of `kv_project`. `workbench`: this member runs the workbench.
pub fn apply(
    env: &HostEnv,
    spec: &mut SandboxSpec,
    kv_project: &Path,
    name: &str,
    workbench: bool,
) -> Result<()> {
    let ground = crate::pasture::ground(env, kv_project, name);
    crate::privfs::dir(&ground)?;
    spec.add_mount(Mount::bind(
        &ground,
        crate::pasture::MOUNT,
        true,
        &format!("pasture {name}"),
    ));
    spec.set_env("PADDOCK_PASTURE", crate::pasture::MOUNT, "pasture");
    spec.set_env("PADDOCK_PASTURE_NAME", name, "pasture");
    // The socket folder of the workbench, from the start, so that a
    // member can join a workbench that starts later. Read-only for
    // members: a client can connect to a socket on a read-only mount.
    let bench = crate::pasture::workbench::dir(env, kv_project, name);
    crate::privfs::dir(&bench)?;
    spec.add_mount(Mount::bind(
        &bench,
        crate::pasture::workbench::MOUNT,
        workbench,
        &format!("pasture {name} workbench"),
    ));
    spec.set_env(
        crate::pasture::workbench::ENV,
        crate::pasture::workbench::MOUNT,
        "pasture",
    );
    // Port sockets live outside the shared folder. Every member sees
    // the tree read-only. Only this member can create its own sockets.
    let ports = crate::pasture::ports::host_dir(env, kv_project, name);
    crate::privfs::dir(&ports)?;
    let own = crate::pasture::ports::member_dir(&ports, &spec.id)?;
    crate::privfs::dir(&own)?;
    spec.add_mount(Mount::bind(
        &ports,
        crate::pasture::ports::PORTS_MOUNT,
        false,
        &format!("pasture {name} ports"),
    ));
    spec.add_mount(Mount::bind(
        &own,
        crate::pasture::ports::OWN_MOUNT,
        true,
        &format!("pasture {name} own ports"),
    ));
    spec.set_env(
        crate::pasture::ports::PORTS_ENV,
        crate::pasture::ports::PORTS_MOUNT,
        "pasture",
    );
    spec.set_env(
        crate::pasture::ports::OWN_ENV,
        crate::pasture::ports::OWN_MOUNT,
        "pasture",
    );
    // The helper that relays the ports that members share, and
    // answers the questions of other members.
    spec.setup
        .push("paddock host pasture serve >/dev/null 2>&1 &".into());
    Ok(())
}
