//! `command` user devices: the request is the standard input of a fixed
//! command on the host.

use super::user::UserDev;
use super::{Call, DevError, Device, DeviceCtx, Info};

pub fn info(def: &UserDev) -> Info {
    Info {
        request: "bytes for the standard input of the command".into(),
        response: "the standard output".into(),
        limits: def.limits(),
    }
}

pub struct Command {
    def: UserDev,
}

impl Command {
    pub fn new(def: UserDev, _ctx: DeviceCtx, _how: Option<crate::execbox::Isolation>) -> Self {
        Self { def }
    }
}

impl Device for Command {
    fn name(&self) -> &str {
        &self.def.name
    }
    fn info(&self) -> Info {
        info(&self.def)
    }
    fn details(&self) -> Option<serde_json::Value> {
        Some(self.def.details())
    }
    fn call(&self, _call: &Call, _request: &[u8]) -> Result<Vec<u8>, DevError> {
        Err(DevError::Failed)
    }
}
