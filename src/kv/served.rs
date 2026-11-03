//! A running kv service of one sandbox.

use super::{resume_leases, shard, Service, Store};
use std::path::PathBuf;
use std::sync::Arc;

/// A running service. `finish` records the end of the run in the store.
pub struct Served {
    pub(super) root: PathBuf,
    pub(super) sandbox: String,
    pub(super) monitor: Option<crate::monitor::Watch>,
    pub(super) svc: Arc<Service>,
}

impl Served {
    /// The service, for writes that paddock itself makes as this sandbox
    /// (the compaction record of broker/compacted.rs).
    pub fn service(&self) -> Arc<Service> {
        self.svc.clone()
    }

    /// Start the credential check of the sandbox home (its host path), with
    /// the login files of the harness profile (`state.auth`).
    pub fn watch_credentials(&self, home: PathBuf, login: Vec<String>) {
        if let Some(m) = &self.monitor {
            m.credentials(home, login);
        }
    }

    pub fn finish(self) {
        if let Some(m) = &self.monitor {
            m.finish();
        }
        // A sandbox that ends while it is frozen: its kept leases expire
        // again, with the time that they had left.
        if let Err(e) = resume_leases(&self.root, &self.sandbox, crate::state::now()) {
            log::debug!("cannot resume the leases of {}: {e:#}", self.sandbox);
        }
        if let Err(e) = Store::open(&shard::registry(&self.root))
            .and_then(|s| s.finish(&self.sandbox, crate::state::now()))
        {
            log::debug!(
                "cannot record the end of {} in the kv store: {e:#}",
                self.sandbox
            );
        }
    }
}
