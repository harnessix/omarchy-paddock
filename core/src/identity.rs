//! The identity of the machine, as a sandbox sees it.
//!
//! A sandbox does not show the host's identity to the programs in it. With
//! an isolated login (`--auth isolated:NAME`), the sandbox is another
//! machine: `/etc/machine-id` holds an ID of that login, the same at every
//! start, and the boot ID is derived from it, so two isolated logins cannot
//! be linked to each other or to the host. A shared login, or an account
//! folder of the host, is the host's own login: it keeps the host's machine
//! ID and boot ID, so a login made on the host keeps working.
//!
//! The ID is never new at each start: a login that sees a new machine at
//! each start looks like a login copied to many machines.

use crate::env::HostEnv;
use crate::state::AuthMode;
use anyhow::{bail, Context as _, Result};
use std::io::Read;
use std::path::{Path, PathBuf};

/// What the sandbox shows instead of the host's values. None: the host's.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MachineIdentity {
    /// The text of `/etc/machine-id`: 32 lowercase hex digits and a newline.
    pub machine_id: Option<String>,
    /// The text of `/proc/sys/kernel/random/boot_id`.
    pub boot_id: Option<String>,
}

/// The file that keeps the machine ID of an isolated login, in the folder
/// of the login, so that it goes when the login goes.
pub fn id_file(env: &HostEnv, harness: &str, name: &str) -> PathBuf {
    crate::state::auth_root(env, harness, &AuthMode::Isolated(name.into()))
        .join(".paddock-machine-id")
}

/// The identity for a login. An isolated login gets its machine ID from its
/// file, and the file is made at the first start.
pub fn for_login(env: &HostEnv, harness: &str, auth: &AuthMode) -> Result<MachineIdentity> {
    let AuthMode::Isolated(name) = auth else {
        return Ok(MachineIdentity::default());
    };
    let id = machine_id(&id_file(env, harness, name))?;
    let host_boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap_or_default();
    Ok(MachineIdentity {
        boot_id: Some(boot_id(&id, host_boot.trim())),
        machine_id: Some(format!("{id}\n")),
    })
}

fn valid(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        && id.bytes().any(|b| b != b'0')
}

/// Read the machine ID in `file`, or make one.
fn machine_id(file: &Path) -> Result<String> {
    if let Ok(text) = std::fs::read_to_string(file) {
        let id = text.trim();
        if valid(id) {
            return Ok(id.to_string());
        }
        bail!(
            "{} does not hold a machine ID. Remove it, and paddock makes a new one at the next start",
            file.display()
        );
    }
    let id = random_id()?;
    crate::privfs::write(file, format!("{id}\n"))
        .with_context(|| format!("cannot write {}", file.display()))?;
    Ok(id)
}

/// 128 random bits as 32 hex digits, as systemd makes a machine ID.
fn random_id() -> Result<String> {
    let mut b = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .context("cannot read /dev/urandom")?;
    // Version 4 and the variant bits, as systemd sets them.
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    Ok(crate::manifest::hex(&b))
}

/// A boot ID for the login: the same during one boot of the host, new after
/// a reboot, and different for each login. It reveals neither the host's
/// boot ID nor the machine ID.
pub fn boot_id(machine_id: &str, host_boot_id: &str) -> String {
    let mac = crate::gateway::hmac(machine_id.as_bytes(), host_boot_id.as_bytes());
    let h = crate::manifest::hex(&mac[..16]);
    let mut v: Vec<char> = h.chars().collect();
    v[12] = '4';
    v[16] = ['8', '9', 'a', 'b'][(v[16].to_digit(16).unwrap_or(0) % 4) as usize];
    let s: String = v.into_iter().collect();
    format!(
        "{}-{}-{}-{}-{}\n",
        &s[..8],
        &s[8..12],
        &s[12..16],
        &s[16..20],
        &s[20..]
    )
}

/// The `.../net` folders under `/sys/devices` of the host's network
/// interfaces: each holds the interface's MAC address.
pub fn sys_net_dirs() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir("/sys/class/net")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| std::fs::canonicalize(e.path()).ok())
        .filter_map(|p| p.parent().map(Path::to_path_buf))
        .filter(|p| p.starts_with("/sys/devices") && p.ends_with("net"))
        .collect();
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(dir: &Path) -> HostEnv {
        HostEnv::from_parts(dir.join("home"), "t".into(), 1000, 1000, Default::default())
    }

    #[test]
    fn isolated_logins_keep_their_own_id_and_shared_ones_the_hosts() {
        let t = tempfile::tempdir().unwrap();
        let e = env(t.path());
        let iso = |n: &str| AuthMode::Isolated(n.into());
        let a1 = for_login(&e, "claude", &iso("a")).unwrap();
        let a2 = for_login(&e, "claude", &iso("a")).unwrap();
        let b = for_login(&e, "claude", &iso("b")).unwrap();
        let other_harness = for_login(&e, "codex", &iso("a")).unwrap();
        assert_eq!(a1, a2);
        assert_ne!(a1.machine_id, b.machine_id);
        assert_ne!(a1.machine_id, other_harness.machine_id);
        assert_ne!(a1.boot_id, b.boot_id);
        let id = a1.machine_id.unwrap();
        assert!(valid(id.trim()), "{id}");
        assert!(id.ends_with('\n'));
        // The file is private.
        use std::os::unix::fs::PermissionsExt;
        let f = id_file(&e, "claude", "a");
        assert_eq!(
            std::fs::metadata(&f).unwrap().permissions().mode() & 0o777,
            0o600
        );
        for host in [
            AuthMode::Shared,
            AuthMode::account(Path::new("/home/demo/.claude-work")),
        ] {
            assert_eq!(
                for_login(&e, "claude", &host).unwrap(),
                MachineIdentity::default()
            );
        }
        // A damaged file is an error, never a silent new identity.
        std::fs::write(&f, "nope\n").unwrap();
        assert!(for_login(&e, "claude", &iso("a")).is_err());
    }

    #[test]
    fn boot_ids_follow_the_login_and_the_boot() {
        let id = "0123456789abcdef0123456789abcdef";
        let a = boot_id(id, "boot-1");
        assert_eq!(a, boot_id(id, "boot-1"));
        assert_ne!(a, boot_id(id, "boot-2"));
        assert_ne!(a, boot_id("fedcba9876543210fedcba9876543210", "boot-1"));
        assert_eq!(a.len(), 37);
        assert_eq!(&a[14..15], "4");
        assert!(!a.contains("boot-1"));
    }
}
