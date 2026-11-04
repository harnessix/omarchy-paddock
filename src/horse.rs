//! horse (experimental): paddock keeps the sessions of the harnesses.
//!
//! The store is a content-addressed DAG in paddock's data folder. Its
//! nodes are the raw native rows of a harness, byte for byte, with the
//! container format and the harness version. A session is a movable head
//! in the DAG with a harness binding and a lineage. A horse owns a DNA and
//! a namespace of sessions. A fork is a DAG operation of paddock: a new
//! session whose head is an existing node. A codec projects a session head
//! back into a native session file that the harness accepts.
//!
//! The store folder is 0700 and is never mounted into a sandbox.

pub mod dsh;
pub mod session;
pub mod store;

pub use session::{Binding, ForkOf, Horse, LineageStep, Session};
pub use store::{Hash, Node, Store};

use crate::env::HostEnv;
use anyhow::{Context as _, Result};
use std::io::Read as _;
use std::path::PathBuf;

/// The store folder: `$XDG_DATA_HOME/paddock/horse`.
pub fn root(env: &HostEnv) -> PathBuf {
    env.paddock_data().join("horse")
}

/// The DNA of a horse that a run makes when nothing else is chosen.
pub const DEFAULT_DNA: &str =
    "# horse DNA\norigin = \"paddock run\"\n\n[traits]\n\n[capabilities]\n\n[policies]\n";

/// Random bytes from the kernel.
pub(crate) fn random_bytes<const N: usize>() -> Result<[u8; N]> {
    let mut b = [0u8; N];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .context("cannot read /dev/urandom")?;
    Ok(b)
}

/// A new id: the prefix, a dash and 12 hex digits.
pub(crate) fn new_id(prefix: &str) -> Result<String> {
    Ok(format!("{prefix}-{}", hex(&random_bytes::<6>()?)))
}

/// A random UUID, version 4, in the usual text form.
pub(crate) fn uuid_v4() -> Result<String> {
    let mut b = random_bytes::<16>()?;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = hex(&b);
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    ))
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Milliseconds since the epoch.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_has_version_and_variant() {
        let u = uuid_v4().unwrap();
        assert_eq!(u.len(), 36);
        assert_eq!(&u[14..15], "4");
        assert!(matches!(&u[19..20], "8" | "9" | "a" | "b"));
        assert_ne!(u, uuid_v4().unwrap());
    }

    #[test]
    fn store_root_is_in_the_data_folder() {
        let t = tempfile::tempdir().unwrap();
        let env = crate::privfs::tests::env_in(t.path());
        assert_eq!(root(&env), env.paddock_data().join("horse"));
    }
}
