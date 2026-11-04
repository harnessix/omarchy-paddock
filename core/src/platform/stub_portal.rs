//! XDG desktop portals. macOS has no native portal in this build.

use anyhow::Result;

pub fn serve() -> Result<()> {
    Err(crate::platform::unavailable("the portal"))
}
