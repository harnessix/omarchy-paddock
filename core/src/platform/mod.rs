//! Differences between Linux and macOS.
//!
//! Linux keeps the calls the sandbox already uses. macOS uses a native
//! stand-in where one exists. A feature with no native implementation returns
//! [`unavailable`] so the caller refuses it out loud.

use anyhow::Error;

/// `feature` has no native macOS implementation in this build.
pub fn unavailable(feature: &str) -> Error {
    anyhow::anyhow!("{feature} is not available on macOS yet")
}

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "linux")]
pub use linux::{arm_parent_death_signal, be_idle_io, peer_gone};
#[cfg(target_os = "macos")]
pub use macos::{arm_parent_death_signal, be_idle_io, peer_gone};
