//! The VM backend on macOS: Linux paddock inside a light Linux VM, made with the
//! Virtualization framework by the `paddock-vm` helper.

pub mod agent;
pub mod agent_run;
pub mod agent_user;
pub mod assets;
pub mod assets_setup;
pub mod attach;
pub mod bridge;
pub mod helper;
pub mod host;
pub mod ports;
pub mod proto;
pub mod select;
pub mod term;
#[cfg(target_os = "macos")]
pub mod vz;

pub use assets::{assets_dir, check, doctor_lines, Assets};
