//! What reads the harness screen next to the relay: the training gate
//! (training.rs) and the start screen watcher (screens.rs).

use std::io;
use std::os::fd::RawFd;
use std::sync::Arc;

/// Writes one line into the view log of the run, if it has one.
pub type Log = Arc<dyn Fn(&[u8]) + Send + Sync>;

/// A reader of the harness output that can also send keys.
pub trait Tap: Send + Sync {
    /// Start on the relay's terminals, before the harness starts.
    fn attach(&self, caller: RawFd, master: RawFd, log: Log);
    /// A copy of the harness output. The relay has already forwarded it.
    fn output(&self, data: &[u8]);
    /// A user key came. The tap that writes it is the first one.
    fn user_key(&self) {}
    /// User input for the harness: decide first, then write it.
    fn write_user(&self, master: RawFd, data: &[u8]) -> io::Result<()> {
        self.user_key();
        super::write_all_fd(master, data)
    }
    /// The caller's terminal changed its size.
    fn resize(&self);
    /// Stop. No key after this.
    fn finish(&self);
}

impl Tap for crate::training::live::Gate {
    fn attach(&self, caller: RawFd, master: RawFd, _log: Log) {
        crate::training::live::Gate::attach(self, caller, master)
    }
    fn output(&self, data: &[u8]) {
        crate::training::live::Gate::output(self, data)
    }
    fn write_user(&self, master: RawFd, data: &[u8]) -> io::Result<()> {
        crate::training::live::Gate::write_user(self, master, data)
    }
    fn resize(&self) {
        crate::training::live::Gate::resize(self)
    }
    fn finish(&self) {
        crate::training::live::Gate::finish(self)
    }
}

/// Several taps. The first one writes the user's keys. The others hear
/// about each key first.
pub struct Taps(Vec<Arc<dyn Tap>>);

impl Taps {
    /// The training gate first, then the start screen watcher. None when
    /// neither runs.
    pub fn of(
        gate: Option<Arc<crate::training::live::Gate>>,
        screens: Option<Arc<crate::screens::live::Watcher>>,
    ) -> Option<Arc<dyn Tap>> {
        let mut all: Vec<Arc<dyn Tap>> = Vec::new();
        if let Some(g) = gate {
            all.push(g);
        }
        if let Some(s) = screens {
            all.push(s);
        }
        match all.len() {
            0 => None,
            1 => all.pop(),
            _ => Some(Arc::new(Taps(all))),
        }
    }
}

impl Tap for Taps {
    fn attach(&self, caller: RawFd, master: RawFd, log: Log) {
        for t in &self.0 {
            t.attach(caller, master, log.clone());
        }
    }
    fn output(&self, data: &[u8]) {
        for t in &self.0 {
            t.output(data);
        }
    }
    fn user_key(&self) {
        for t in &self.0 {
            t.user_key();
        }
    }
    fn write_user(&self, master: RawFd, data: &[u8]) -> io::Result<()> {
        for t in &self.0[1..] {
            t.user_key();
        }
        self.0[0].write_user(master, data)
    }
    fn resize(&self) {
        for t in &self.0 {
            t.resize();
        }
    }
    fn finish(&self) {
        for t in &self.0 {
            t.finish();
        }
    }
}
