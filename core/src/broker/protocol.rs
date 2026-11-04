//! The request, the status and the response of one host call.

use crate::state;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::Instant;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub v: u32,
    pub cap: String,
    #[serde(default)]
    pub args: BTreeMap<String, serde_json::Value>,
    /// Why the agent asks. Shown to the user next to the request.
    #[serde(default)]
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Done,
    Denied,
    Disabled,
    Timeout,
    Failed,
    Invalid,
    /// A blocking read reached its time limit before the event, for example
    /// `paddock-host wait` on a child that still runs. Run it again.
    Waiting,
}

impl Status {
    /// The exit code of `paddock-host` for this status.
    pub fn exit_code(&self) -> i32 {
        match self {
            Status::Done => 0,
            Status::Denied => 3,
            Status::Disabled => 4,
            Status::Timeout => 5,
            Status::Waiting => crate::waiting::EXIT_STILL_WAITING,
            Status::Failed => 7,
            Status::Invalid => 8,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub status: Status,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub result: serde_json::Value,
}

impl Response {
    pub(crate) fn new(status: Status, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            result: serde_json::Value::Null,
        }
    }
    pub(crate) fn done(result: serde_json::Value) -> Self {
        Self {
            status: Status::Done,
            message: String::new(),
            result,
        }
    }
}

pub(crate) fn new_request_id() -> String {
    use sha2::{Digest, Sha256};
    let seed = format!("{:?}-{}", Instant::now(), std::process::id());
    let h = crate::manifest::hex(&Sha256::digest(seed.as_bytes()));
    format!("r-{}-{}", state::now(), &h[..8])
}
