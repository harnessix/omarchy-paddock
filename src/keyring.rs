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

use crate::env::HostEnv;
use crate::manifest::hex;
use crate::spec::{GuiMode, SandboxSpec};
use anyhow::Result;
use sha2::{Digest, Sha256};
use std::io::Read;
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
    std::fs::create_dir_all(file.parent().unwrap())?;
    std::fs::set_permissions(
        file.parent().unwrap(),
        std::fs::Permissions::from_mode(0o700),
    )?;
    std::fs::write(&file, &pw)?;
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600))?;
    Ok(pw)
}

/// Wrap the command so that the private keyring starts first. Call it before
/// the display is applied, so that the keyring joins the display's private
/// bus when there is one.
pub fn apply(env: &HostEnv, spec: &mut SandboxSpec) -> Result<()> {
    if spec.gui.mode == GuiMode::App {
        spec.warnings.push(
            "the private keyring is off in app mode: apps use the filtered host bus; most desktop apps store their login in a file instead".into(),
        );
        return Ok(());
    }
    let origin = "keyring: private Secret Service";
    let script = env.paddock_run().join(&spec.id).join(TOOL);
    crate::gui::write_file(&script, WRAPPER, 0o755)?;
    spec.add_tool(TOOL, script, origin);
    spec.set_secret_env("PADDOCK_KEYRING_PW", password(env, &spec.home_src)?, origin);
    let mut cmd = vec![TOOL.to_string()];
    cmd.append(&mut spec.command);
    spec.command = cmd;
    spec.keyring = true;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn password_is_stable_per_home_and_private() {
        let tmp = tempfile::tempdir().unwrap();
        let env = HostEnv::from_parts(tmp.path().to_path_buf(), "u".into(), 1, 1, BTreeMap::new());
        let a = password(&env, std::path::Path::new("/h/a")).unwrap();
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
    }
}
