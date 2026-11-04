//! The paddock kv part of `paddock doctor --escape`. Before the run, the
//! host writes a canary into the `self` scope and the `tree` scope of
//! another, made-up sandbox and opens a kv socket in that sandbox's run
//! folder. The probe tries to
//! read the canary and to claim another identity. After the run, the host
//! checks the files: the canary is unchanged, and every entry that the
//! probe wrote carries the probe's own sandbox id.

use super::shard::{self, Ns};
use super::store::{Quota, Store, Writer};
use crate::env::HostEnv;
use anyhow::Result;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};

pub const CANARY: &str = "paddock-kv-canary";

pub struct EscapeProbe {
    pub other: String,
    other_dir: PathBuf,
    socket: PathBuf,
    _listener: UnixListener,
    /// The kv folder.
    root: PathBuf,
    version: u64,
}

impl EscapeProbe {
    pub fn setup(env: &HostEnv, suffix: &str) -> Result<Self> {
        let other = format!("doctor-other-{suffix}");
        let root = super::kv_root(env);
        let w = Writer {
            sandbox: &other,
            harness: "doctor",
        };
        let q = Quota {
            bytes: 1 << 20,
            keys: 16,
        };
        let now = crate::state::now();
        let own = Ns::Own(other.clone());
        let tree = Ns::Tree(other.clone());
        let version = Store::open(&own.file(&root)?)?
            .put(&own.key(), "canary", CANARY, None, None, &w, q, now)
            .map_err(|e| anyhow::anyhow!("cannot write the kv canary: {e:?}"))?;
        Store::open(&tree.file(&root)?)?
            .put(&tree.key(), "canary", CANARY, None, None, &w, q, now)
            .map_err(|e| anyhow::anyhow!("cannot write the kv tree canary: {e:?}"))?;
        Store::open(&shard::registry(&root))?.register(
            &other,
            "doctor",
            Path::new("/"),
            None,
            None,
            now,
        )?;
        let other_dir = env.paddock_run().join(&other);
        crate::privfs::dir(&other_dir)?;
        let socket = other_dir.join("kv.sock");
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket)?;
        Ok(Self {
            other,
            other_dir,
            socket,
            _listener: listener,
            root,
            version,
        })
    }

    /// Variables for the probe.
    pub fn vars(&self) -> Vec<(&'static str, String)> {
        vec![
            ("PADDOCK_PROBE_KV_OTHER", self.other.clone()),
            (
                "PADDOCK_PROBE_KV_OTHER_SOCK",
                self.socket.display().to_string(),
            ),
        ]
    }

    /// Host-side checks after the run. `sandbox` is the id of the probe's
    /// sandbox; `asked` tells whether the user scope showed an approval card.
    pub fn check(
        &self,
        sandbox: &str,
        project: &Path,
        asked: bool,
    ) -> Vec<(&'static str, Option<String>)> {
        let now = crate::state::now();
        let open = |ns: &Ns| -> Result<Store> { Store::open(&ns.file(&self.root)?) };
        let (other_ns, tree_ns) = (Ns::Own(self.other.clone()), Ns::Tree(self.other.clone()));
        // The probe runs without --pasture: its project scope is its own pasture.
        let own_ns = Ns::Own(sandbox.into());
        let pasture_ns = Ns::Solo {
            project: project.into(),
            sandbox: sandbox.into(),
        };
        let (other, tree, own, pasture) = match (
            open(&other_ns),
            open(&tree_ns),
            open(&own_ns),
            open(&pasture_ns),
        ) {
            (Ok(a), Ok(b), Ok(c), Ok(d)) => (a, b, c, d),
            (Err(e), ..) | (_, Err(e), ..) | (_, _, Err(e), _) | (.., Err(e)) => {
                return vec![("kv-store-opens", Some(format!("{e:#}")))]
            }
        };
        let canary = other.get(&other_ns.key(), "canary", now).ok().flatten();
        let tree_canary = tree.get(&tree_ns.key(), "canary", now).ok().flatten();
        let tree_keys = tree
            .list(&tree_ns.key(), "", 100, now)
            .map(|(v, _)| v.len())
            .unwrap_or(0);
        let other_keys = other
            .list(&other_ns.key(), "", 100, now)
            .map(|(v, _)| v.len())
            .unwrap_or(0);
        let own = own.get(&own_ns.key(), "probe/identity", now).ok().flatten();
        let note = pasture
            .get(&pasture_ns.key(), "probe/note", now)
            .ok()
            .flatten();
        let lease = pasture
            .get(&pasture_ns.key(), "lease:probe", now)
            .ok()
            .flatten();
        let check = |ok: bool, why: String| (!ok).then_some(why);
        vec![
            (
                "kv-other-sandbox-unchanged",
                check(
                    canary
                        .as_ref()
                        .is_some_and(|c| c.value == CANARY && c.version == self.version)
                        && other_keys == 1,
                    format!(
                        "the self scope of another sandbox changed: {} keys, canary {:?}",
                        other_keys,
                        canary.map(|c| c.version)
                    ),
                ),
            ),
            (
                "kv-other-tree-unchanged",
                check(
                    tree_canary.as_ref().is_some_and(|c| c.value == CANARY) && tree_keys == 1,
                    format!("the tree scope of another spawn tree changed: {tree_keys} keys"),
                ),
            ),
            (
                "kv-writes-carry-sandbox-id",
                check(
                    own.as_ref().is_some_and(|e| e.writer == sandbox),
                    format!(
                        "the probe's self write is missing or has writer {:?}",
                        own.map(|e| e.writer)
                    ),
                ),
            ),
            (
                "kv-pasture-entry-records-writer",
                check(
                    note.as_ref().is_some_and(|e| e.writer == sandbox),
                    format!(
                        "the pasture entry is missing or has writer {:?}",
                        note.map(|e| e.writer)
                    ),
                ),
            ),
            (
                "kv-lease-owner-from-socket",
                check(
                    lease.as_ref().is_some_and(|e| {
                        e.writer == sandbox
                            && serde_json::from_str::<serde_json::Value>(&e.value)
                                .is_ok_and(|v| v["owner"] == sandbox)
                    }),
                    format!(
                        "the lease is missing or names another owner: {:?}",
                        lease.map(|e| e.value)
                    ),
                ),
            ),
            (
                "kv-user-scope-asks",
                check(asked, "the user scope did not show an approval card".into()),
            ),
        ]
    }

    /// Remove what this run wrote: the canary, the probe's scopes and the
    /// fake run folder.
    pub fn cleanup(&self, sandbox: &str, _project: &Path) {
        // The probe's own pasture lives in its sandbox file.
        for ns in [
            Ns::Own(self.other.clone()),
            Ns::Tree(self.other.clone()),
            Ns::Own(sandbox.into()),
            Ns::Tree(sandbox.into()),
        ] {
            if let Ok(f) = ns.file(&self.root) {
                let _ = shard::remove_file(&f);
            }
        }
        if let Ok(mut st) = Store::open(&shard::registry(&self.root)) {
            let _ = st.forget_sandbox(&self.other);
            let _ = st.forget_sandbox(sandbox);
        }
        let _ = std::fs::remove_file(&self.socket);
        let _ = std::fs::remove_dir(&self.other_dir);
    }
}
