//! A restricted Wayland socket for app mode, made with the
//! wp_security_context_v1 protocol. The compositor knows that clients on this
//! socket are sandboxed and does not offer them privileged protocols: screen
//! capture, virtual keyboards and pointers, clipboard monitoring, window
//! lists, input capture and global shortcuts. Tested on Hyprland: 71 globals
//! on the normal socket, 40 on the restricted one, and grim and wtype fail.

use anyhow::{bail, Context as _, Result};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use wayland_client::{protocol::wl_registry, Connection, Dispatch, QueueHandle};
use wayland_protocols::wp::security_context::v1::client::{
    wp_security_context_manager_v1::WpSecurityContextManagerV1,
    wp_security_context_v1::WpSecurityContextV1,
};

struct State {
    mgr: Option<WpSecurityContextManagerV1>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        s: &mut Self,
        r: &wl_registry::WlRegistry,
        e: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name, interface, ..
        } = e
        {
            if interface == "wp_security_context_manager_v1" {
                s.mgr = Some(r.bind(name, 1, qh, ()));
            }
        }
    }
}

impl Dispatch<WpSecurityContextManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &WpSecurityContextManagerV1,
        _: <WpSecurityContextManagerV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WpSecurityContextV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &WpSecurityContextV1,
        _: <WpSecurityContextV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

/// A socket that stays restricted while this value lives. Dropping it closes
/// the write end of the close pipe, and the compositor stops listening.
pub struct RestrictedSocket {
    pub path: PathBuf,
    _close: OwnedFd,
    _conn: Connection,
}

/// Whether the host compositor offers the protocol.
pub fn supported() -> bool {
    let Ok(conn) = Connection::connect_to_env() else {
        return false;
    };
    let mut q = conn.new_event_queue();
    let qh = q.handle();
    conn.display().get_registry(&qh, ());
    let mut s = State { mgr: None };
    q.roundtrip(&mut s).is_ok() && s.mgr.is_some()
}

pub fn create(path: &Path, app_id: &str, instance_id: &str) -> Result<RestrictedSocket> {
    let conn =
        Connection::connect_to_env().context("app mode needs a Wayland session on the host")?;
    let mut q = conn.new_event_queue();
    let qh = q.handle();
    conn.display().get_registry(&qh, ());
    let mut s = State { mgr: None };
    q.roundtrip(&mut s)?;
    let Some(mgr) = s.mgr.take() else {
        bail!("the compositor does not offer wp_security_context_v1, so app windows cannot be restricted. Use --gui window instead");
    };
    let _ = std::fs::remove_file(path);
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let listener =
        UnixListener::bind(path).with_context(|| format!("cannot listen on {}", path.display()))?;
    let (close_r, close_w) = std::io::pipe()?;
    let ctx = mgr.create_listener(listener.as_fd(), OwnedFd::from(close_r).as_fd(), &qh, ());
    ctx.set_sandbox_engine("io.paddock".into());
    ctx.set_app_id(app_id.into());
    ctx.set_instance_id(instance_id.into());
    ctx.commit();
    q.roundtrip(&mut s)?;
    Ok(RestrictedSocket {
        path: path.to_path_buf(),
        _close: OwnedFd::from(close_w),
        _conn: conn,
    })
}
