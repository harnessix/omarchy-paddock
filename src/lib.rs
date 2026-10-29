//! paddock: run coding-agent harnesses with full auto-approval inside a
//! sandbox that only contains what the agent should see.

pub mod app;
pub mod approver;
pub mod backend;
pub mod broker;
pub mod build;
pub mod detect;
pub mod doctor;
pub mod env;
pub mod gui;
pub mod hostcli;
pub mod keyring;
pub mod launchers;
pub mod login;
pub mod manifest;
pub mod mcp;
pub mod policy;
pub mod profile;
pub mod proxy;
pub mod run;
pub mod shims;
pub mod spec;
pub mod state;
pub mod wlsec;
