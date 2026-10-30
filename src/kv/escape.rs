//! The paddock kv part of `paddock doctor --escape`. Before the run, the
//! host writes a canary into the `self` scope and the `tree` scope of
//! another, made-up sandbox and opens a kv socket in that sandbox's run
//! folder. The probe tries to
//! read the canary and to claim another identity. After the run, the host
//! checks the store: the canary is unchanged, and every entry that the
//! probe wrote carries the probe's own sandbox id.

use super::store::{self, Quota, Store, Writer};
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
    store: PathBuf,
    version: u64,
}

impl EscapeProbe {
    pub fn setup(env: &HostEnv, suffix: &str) -> Result<Self> {
        let other = format!("doctor-other-{suffix}");
        let path = super::store_path(env);
        let mut st = Store::open(&path)?;
        let w = Writer {
            sandbox: &other,
            harness: "doctor",
        };
        let q = Quota {
            bytes: 1 << 20,
            keys: 16,
        };
        let now = crate::state::now();
        let version = st
            .put(
                &store::ns_self(&other),
                "canary",
                CANARY,
                None,
                None,
                &w,
                q,
                now,
            )
            .map_err(|e| anyhow::anyhow!("cannot write the kv canary: {e:?}"))?;
        st.put(
            &store::ns_tree(&other),
            "canary",
            CANARY,
            None,
            None,
            &w,
            q,
            now,
        )
        .map_err(|e| anyhow::anyhow!("cannot write the kv tree canary: {e:?}"))?;
        st.register(&other, "doctor", Path::new("/"), None, now)?;
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
            store: path,
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
        let st = match Store::open(&self.store) {
            Ok(s) => s,
            Err(e) => return vec![("kv-store-opens", Some(format!("{e:#}")))],
        };
        let now = crate::state::now();
        let other_ns = store::ns_self(&self.other);
        let own_ns = store::ns_self(sandbox);
        let project_ns = store::ns_project(project);
        let canary = st.get(&other_ns, "canary", now).ok().flatten();
        let tree_canary = st
            .get(&store::ns_tree(&self.other), "canary", now)
            .ok()
            .flatten();
        let tree_keys = st
            .list(&store::ns_tree(&self.other), "", 100, now)
            .map(|(v, _)| v.len())
            .unwrap_or(0);
        let other_keys = st
            .list(&other_ns, "", 100, now)
            .map(|(v, _)| v.len())
            .unwrap_or(0);
        let own = st.get(&own_ns, "probe/identity", now).ok().flatten();
        let note = st.get(&project_ns, "probe/note", now).ok().flatten();
        let lease = st.get(&project_ns, "lease:probe", now).ok().flatten();
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
                "kv-project-entry-records-writer",
                check(
                    note.as_ref().is_some_and(|e| e.writer == sandbox),
                    format!(
                        "the project entry is missing or has writer {:?}",
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
    pub fn cleanup(&self, sandbox: &str, project: &Path) {
        if let Ok(mut st) = Store::open(&self.store) {
            for ns in [
                store::ns_self(&self.other),
                store::ns_tree(&self.other),
                store::ns_self(sandbox),
                store::ns_tree(sandbox),
                store::ns_project(project),
            ] {
                let _ = st.remove_ns(&ns);
            }
            let _ = st.forget_sandbox(&self.other);
            let _ = st.forget_sandbox(sandbox);
        }
        let _ = std::fs::remove_file(&self.socket);
        let _ = std::fs::remove_dir(&self.other_dir);
    }
}
