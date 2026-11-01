//! Credential values in the memory of the paddock process.
//!
//! `Secret` owns the bytes of one value. It locks them in RAM (`mlock`) so
//! they never go to swap, and it overwrites them with zeros before it frees
//! them. Its `Debug` never shows the value.
//!
//! `SecretFiles` holds the values of `as = "file"` credentials for the
//! device file system of one sandbox (`/dev/paddock/secret/NAME`).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// One credential value, locked in RAM and wiped on drop.
pub struct Secret {
    buf: Vec<u8>,
    locked: bool,
}

static MLOCK_FAILED: AtomicBool = AtomicBool::new(false);

impl Secret {
    pub fn new(buf: Vec<u8>) -> Self {
        let cap = buf.capacity();
        let mut locked = false;
        if cap > 0 {
            // SAFETY: the range is the allocation of `buf`, which lives as
            // long as `self` and never grows.
            locked = unsafe { libc::mlock(buf.as_ptr().cast(), cap) } == 0;
            if !locked && !MLOCK_FAILED.swap(true, Ordering::Relaxed) {
                log::debug!(
                    "cannot lock a credential value in RAM: {}",
                    std::io::Error::last_os_error()
                );
            }
        }
        Self { buf, locked }
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.buf
    }

    /// The value as text. Values come from UTF-8 sources, so this is the
    /// whole value; bytes that are not UTF-8 give an empty text.
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.buf).unwrap_or("")
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Overwrite the whole allocation with zeros. Drop calls it.
    fn wipe(&mut self) {
        let cap = self.buf.capacity();
        let p = self.buf.as_mut_ptr();
        for i in 0..cap {
            // SAFETY: `i` is inside the allocation of `buf`. Volatile, so
            // the compiler keeps the writes before the free.
            unsafe { std::ptr::write_volatile(p.add(i), 0) };
        }
        std::sync::atomic::compiler_fence(Ordering::SeqCst);
        #[cfg(test)]
        tests::WIPED.with(|w| w.borrow_mut().push(self.buf.clone()));
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        self.wipe();
        if self.locked {
            // SAFETY: the same range that `new` locked.
            unsafe { libc::munlock(self.buf.as_ptr().cast(), self.buf.capacity()) };
        }
    }
}

impl From<String> for Secret {
    fn from(s: String) -> Self {
        Self::new(s.into_bytes())
    }
}

impl From<&str> for Secret {
    fn from(s: &str) -> Self {
        Self::new(s.as_bytes().to_vec())
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Secret({} bytes)", self.buf.len())
    }
}

/// Why a secret file cannot be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileError {
    /// No such secret in this sandbox.
    Missing,
    /// The user revoked it.
    Revoked,
}

/// Reports one read of a secret file, by name. Never the value.
pub type OnRead = Arc<dyn Fn(&str) + Send + Sync>;

/// The values of `as = "file"` credentials of one sandbox. paddock fills it
/// after the start card; the device file system reads it.
pub struct SecretFiles {
    values: Mutex<BTreeMap<String, Arc<Secret>>>,
    /// The file that `paddock credentials revoke` writes (`revoked_file`).
    revoked: Option<PathBuf>,
    on_read: Option<OnRead>,
}

impl SecretFiles {
    /// An empty store. `revoked`: the revoke file of the sandbox.
    pub fn new(revoked: Option<PathBuf>, on_read: Option<OnRead>) -> Arc<Self> {
        Arc::new(Self {
            values: Mutex::new(BTreeMap::new()),
            revoked,
            on_read,
        })
    }

    pub fn insert(&self, name: &str, value: Secret) {
        self.values
            .lock()
            .unwrap()
            .insert(name.to_string(), Arc::new(value));
    }

    pub fn names(&self) -> Vec<String> {
        self.values.lock().unwrap().keys().cloned().collect()
    }

    pub fn contains(&self, name: &str) -> bool {
        self.values.lock().unwrap().contains_key(name)
    }

    fn is_revoked(&self, name: &str) -> bool {
        let Some(f) = &self.revoked else {
            return false;
        };
        std::fs::read_to_string(f)
            .map(|t| t.lines().any(|l| l.trim() == name || l.trim() == "*"))
            .unwrap_or(false)
    }

    /// The value for one open of `/dev/paddock/secret/NAME`. Counts one use.
    /// A revoked value leaves the store and its bytes are wiped as soon as
    /// the last open file lets it go.
    pub fn open(&self, name: &str) -> Result<Arc<Secret>, FileError> {
        let v = self.check(name)?;
        if let Some(r) = &self.on_read {
            r(name);
        }
        Ok(v)
    }

    /// The value, without counting a use: for each read of an open file.
    pub fn check(&self, name: &str) -> Result<Arc<Secret>, FileError> {
        if self.is_revoked(name) {
            return match self.values.lock().unwrap().remove(name) {
                Some(_) => Err(FileError::Revoked),
                None => Err(FileError::Missing),
            };
        }
        self.values
            .lock()
            .unwrap()
            .get(name)
            .cloned()
            .ok_or(FileError::Missing)
    }

    /// Drop the values that the revoke file names.
    pub fn drop_revoked(&self) {
        let names = self.names();
        for n in names {
            if self.is_revoked(&n) {
                self.values.lock().unwrap().remove(&n);
            }
        }
    }

    /// Drop the revoked values every second, while the store lives.
    pub fn watch(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        std::thread::spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
            let Some(s) = weak.upgrade() else {
                return;
            };
            if s.values.lock().unwrap().is_empty() {
                continue;
            }
            s.drop_revoked();
        });
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::cell::RefCell;

    thread_local! {
        /// The buffers that `wipe` left behind, on this thread.
        pub(crate) static WIPED: RefCell<Vec<Vec<u8>>> = const { RefCell::new(Vec::new()) };
    }

    #[test]
    fn debug_hides_the_value() {
        let s = Secret::from("ghp_TOPsecret");
        let d = format!("{s:?} {:?}", Some(&s));
        assert!(!d.contains("TOP"), "{d}");
        assert!(d.contains("13 bytes"), "{d}");
        assert_eq!(s.as_str(), "ghp_TOPsecret");
        assert_eq!(s.len(), 13);
    }

    #[test]
    fn drop_zeroes_the_whole_buffer() {
        WIPED.with(|w| w.borrow_mut().clear());
        let mut v = Vec::with_capacity(32);
        v.extend_from_slice(b"hunter2");
        drop(Secret::new(v));
        let wiped = WIPED.with(|w| w.borrow().clone());
        assert_eq!(wiped, [vec![0u8; 7]]);
    }

    #[test]
    fn a_revoked_file_fails_and_leaves_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let revoked = dir.path().join("revoked");
        let reads = Arc::new(Mutex::new(Vec::new()));
        let r2 = reads.clone();
        let s = SecretFiles::new(
            Some(revoked.clone()),
            Some(Arc::new(move |n: &str| {
                r2.lock().unwrap().push(n.to_string())
            })),
        );
        s.insert("GH_TOKEN", Secret::from("v1"));
        assert_eq!(s.open("GH_TOKEN").unwrap().as_str(), "v1");
        assert_eq!(s.open("OTHER").unwrap_err(), FileError::Missing);
        assert_eq!(*reads.lock().unwrap(), ["GH_TOKEN"]);
        std::fs::write(&revoked, "GH_TOKEN\n").unwrap();
        assert_eq!(s.check("GH_TOKEN").unwrap_err(), FileError::Revoked);
        assert!(s.names().is_empty());
        assert_eq!(s.open("GH_TOKEN").unwrap_err(), FileError::Missing);
    }
}
