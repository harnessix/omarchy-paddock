//! A private keyring for each sandbox.
//!
//! Some CLIs keep their tokens in the system keyring (the Secret Service on
//! the session bus) and refuse, or fall back to a plain-text file, without
//! one. The host keyring must stay out of reach: its interface lets a client
//! list and read every secret in an unlocked collection. So a sandbox with
//! `keyring = true` gets its own gnome-keyring, secrets component only, on a
//! private session bus. Its files live in the sandbox home, encrypted with a
//! random password that paddock keeps on the host, outside the sandbox.
//! The sandbox logs in once; the token is then stored encrypted at rest.
//!
//! In app mode the keyring runs on the app's own session bus: a
//! `dbus-daemon` inside the sandbox, without service activation, so that no
//! host service starts in there. `paddock portal` (portal.rs) joins the same
//! bus and passes links and notifications to the host. The app never touches
//! the host bus. `[apps.NAME] bus = "host-filtered"` keeps the older filtered
//! host bus, without a keyring.

use crate::env::HostEnv;
use crate::manifest::{hex, AppBus};
use crate::spec::{GuiMode, SandboxSpec};
use anyhow::Result;
use sha2::{Digest, Sha256};
use std::io::Read;
#[cfg(test)]
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

pub const TOOL: &str = "paddock-keyring";

const WRAPPER: &str = r#"#!/bin/sh
# paddock: start a private keyring (Secret Service) for this sandbox, then
# the command. The keyring lives in the sandbox home and is encrypted with a
# password that paddock keeps on the host.
if [ -z "$DBUS_SESSION_BUS_ADDRESS" ] && command -v dbus-run-session >/dev/null 2>&1; then
  exec dbus-run-session -- "$0" "$@"
fi
pw="$PADDOCK_KEYRING_PW"
unset PADDOCK_KEYRING_PW
if [ -z "$XDG_RUNTIME_DIR" ]; then
  XDG_RUNTIME_DIR="/tmp/paddock-rt-$(id -u)"
  export XDG_RUNTIME_DIR
  mkdir -p "$XDG_RUNTIME_DIR" && chmod 700 "$XDG_RUNTIME_DIR"
fi
if command -v gnome-keyring-daemon >/dev/null 2>&1; then
  printf %s "$pw" | gnome-keyring-daemon --replace --unlock --components=secrets --daemonize >/dev/null 2>&1 \
    || echo "paddock: the private keyring did not start" >&2
else
  echo "paddock: gnome-keyring is not installed in the sandbox; the private keyring is off" >&2
fi
unset pw
exec "$@"
"#;

/// The app's session bus. The bus configuration has no service folders: a
/// name is there only when a process in the sandbox owns it.
const APP_WRAPPER: &str = r#"#!/bin/sh
# paddock: the app's private session bus, with a private keyring (Secret
# Service) and paddock portal (links and notifications to the host). The
# keyring is encrypted with a password that paddock keeps on the host.
if [ -z "$PADDOCK_APP_BUS" ]; then
  PADDOCK_APP_BUS=1
  export PADDOCK_APP_BUS
  if ! command -v dbus-run-session >/dev/null 2>&1; then
    echo "paddock: dbus is not installed in the sandbox; the app has no session bus and no keyring" >&2
    unset PADDOCK_KEYRING_PW
    exec "$@"
  fi
  conf="$XDG_RUNTIME_DIR/paddock-bus.conf"
  # A socket that an earlier run left behind would block the new bus.
  rm -f "$XDG_RUNTIME_DIR/bus"
  cat > "$conf" <<END
<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:path=$XDG_RUNTIME_DIR/bus</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
END
  exec dbus-run-session --config-file="$conf" -- "$0" "$@"
fi
pw="$PADDOCK_KEYRING_PW"
unset PADDOCK_KEYRING_PW
if command -v gnome-keyring-daemon >/dev/null 2>&1; then
  printf %s "$pw" | gnome-keyring-daemon --replace --unlock --components=secrets --daemonize >/dev/null 2>&1 \
    || echo "paddock: the private keyring did not start" >&2
else
  echo "paddock: gnome-keyring is not installed in the sandbox; the app has no keyring" >&2
fi
unset pw
paddock portal &
# Wait until the keyring and the portal are on the bus: there is no service
# activation, so an app that starts first finds neither.
i=0
while [ $i -lt 100 ]; do
  ok=1
  for n in org.freedesktop.secrets org.freedesktop.portal.Desktop; do
    dbus-send --session --print-reply --dest=org.freedesktop.DBus /org/freedesktop/DBus \
      org.freedesktop.DBus.NameHasOwner "string:$n" 2>/dev/null | grep -q "boolean true" || ok=
  done
  [ -n "$ok" ] && break
  sleep 0.05
  i=$((i+1))
done
exec "$@"
"#;

/// The password of one sandbox home's keyring, created on first use. It is
/// kept under the paddock state folder, which no sandbox can see.
pub fn password(env: &HostEnv, home_src: &std::path::Path) -> Result<String> {
    let key = hex(&Sha256::digest(home_src.to_string_lossy().as_bytes()));
    let file: PathBuf = env.paddock_state().join("keyring").join(&key[..24]);
    if let Ok(pw) = std::fs::read_to_string(&file) {
        if pw.trim().len() >= 32 {
            return Ok(pw.trim().to_string());
        }
    }
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    let pw = hex(&bytes);
    // 0600 from the start: no moment where another user can read it.
    crate::privfs::write(&file, &pw)?;
    Ok(pw)
}

/// Wrap the command so that the private keyring starts first. Call it before
/// the display is applied, so that the keyring joins the display's private
/// bus when there is one.
pub fn apply(env: &HostEnv, spec: &mut SandboxSpec, bus: AppBus) -> Result<()> {
    let app = spec.gui.mode == GuiMode::App;
    if app && bus == AppBus::HostFiltered {
        spec.warnings.push(format!(
            "{} uses the filtered host bus (bus = \"host-filtered\"): it has no keyring, so it may keep its login in a plain file",
            spec.harness
        ));
        // Without a keyring, Chromium-based apps keep the basic store they
        // had before the private bus.
        for a in &mut spec.command {
            if a == "--password-store=gnome-libsecret" {
                *a = "--password-store=basic".into();
            }
        }
        return Ok(());
    }
    let origin = if app {
        "app mode: private session bus and keyring"
    } else {
        "keyring: private Secret Service"
    };
    let script = env.paddock_run().join(&spec.id).join(TOOL);
    crate::gui::write_file(&script, if app { APP_WRAPPER } else { WRAPPER }, 0o755)?;
    spec.add_tool(TOOL, script, origin);
    spec.set_secret_env("PADDOCK_KEYRING_PW", password(env, &spec.home_src)?, origin);
    let mut cmd = vec![TOOL.to_string()];
    cmd.append(&mut spec.command);
    spec.command = cmd;
    spec.keyring = true;
    spec.private_bus = app;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn app_spec(env: &HostEnv) -> SandboxSpec {
        let mut s = SandboxSpec::new(
            "app-1".into(),
            "claude-desktop",
            std::path::Path::new("/w/p"),
            env.home.join("sandbox-home"),
            &env.home,
            1,
            1,
            "u",
        );
        s.gui.mode = GuiMode::App;
        s.command = vec!["/app".into(), "--password-store=gnome-libsecret".into()];
        s
    }

    #[test]
    fn apps_get_a_private_bus_with_a_keyring() {
        let tmp = tempfile::tempdir().unwrap();
        let env = HostEnv::from_parts(tmp.path().to_path_buf(), "u".into(), 1, 1, BTreeMap::new());
        let mut s = app_spec(&env);
        apply(&env, &mut s, AppBus::Private).unwrap();
        assert!(s.keyring && s.private_bus);
        assert_eq!(s.command[0], TOOL);
        assert_eq!(s.command[2], "--password-store=gnome-libsecret");
        let script =
            std::fs::read_to_string(&s.host_tools.iter().find(|t| t.name == TOOL).unwrap().src)
                .unwrap();
        // No service activation: nothing from the host starts on this bus.
        assert!(script.contains("dbus-run-session --config-file="));
        assert!(!script.contains("servicedir"));
        assert!(script.contains("paddock portal &"));
    }

    #[test]
    fn host_filtered_apps_keep_the_basic_store() {
        let tmp = tempfile::tempdir().unwrap();
        let env = HostEnv::from_parts(tmp.path().to_path_buf(), "u".into(), 1, 1, BTreeMap::new());
        let mut s = app_spec(&env);
        apply(&env, &mut s, AppBus::HostFiltered).unwrap();
        assert!(!s.keyring && !s.private_bus);
        assert_eq!(s.command, ["/app", "--password-store=basic"]);
    }

    #[test]
    fn password_is_stable_per_home_and_private() {
        let tmp = tempfile::tempdir().unwrap();
        let env = HostEnv::from_parts(tmp.path().to_path_buf(), "u".into(), 1, 1, BTreeMap::new());
        let a = crate::privfs::tests::with_umask_022(|| {
            password(&env, std::path::Path::new("/h/a")).unwrap()
        });
        assert_eq!(a, password(&env, std::path::Path::new("/h/a")).unwrap());
        assert_ne!(a, password(&env, std::path::Path::new("/h/b")).unwrap());
        assert_eq!(a.len(), 64);
        let dir = env.paddock_state().join("keyring");
        let f = std::fs::read_dir(&dir)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert_eq!(
            std::fs::metadata(f).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(crate::privfs::tests::mode(&dir), 0o700);
        assert_eq!(crate::privfs::tests::mode(&env.paddock_state()), 0o700);
    }
}
