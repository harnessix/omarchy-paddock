//! Wayland security context. macOS has no native compositor restriction yet.

use anyhow::Result;
use std::path::{Path, PathBuf};

/// A restricted Wayland socket. `create` does not return one on macOS.
pub struct RestrictedSocket {
    pub path: PathBuf,
}

/// The host compositor does not offer `wp_security_context_v1` here.
pub fn supported() -> bool {
    false
}

pub fn create(path: &Path, app_id: &str, instance_id: &str) -> Result<RestrictedSocket> {
    let _ = (path, app_id, instance_id);
    Err(crate::platform::unavailable("the Wayland security context"))
}
