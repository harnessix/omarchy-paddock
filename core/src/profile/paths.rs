//! The path rules of the locate step: shims, real paths and system paths.

use anyhow::{Context, Result};
use std::path::{Component, Path, PathBuf};

/// A version-manager shim re-enters the manager, which is not mounted in the sandbox.
pub(super) fn is_shim(path: &Path) -> bool {
    path.components()
        .any(|c| matches!(c, Component::Normal(s) if s == "shims" || s == "command-wrappers"))
}

pub(super) fn canonical(path: &Path) -> Result<PathBuf> {
    let real = std::fs::canonicalize(path)
        .with_context(|| format!("cannot resolve {}", path.display()))?;
    // A multi-call program picks its tool by the name it runs as: the
    // Alpine guest of the VM backend has `/bin/sh` as a link to busybox.
    if is_multi_call(&real) && real.file_name() != path.file_name() {
        return Ok(path.to_path_buf());
    }
    Ok(real)
}

fn is_multi_call(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|n| n == "busybox" || n == "toybox")
}

const SYSTEM_PREFIXES: &[&str] = &[
    "/usr/",
    "/bin/",
    "/sbin/",
    "/lib/",
    "/lib64/",
    "/opt/",
    "/nix/store/",
];

pub fn is_system_path(p: &Path) -> bool {
    let s = p.to_string_lossy();
    SYSTEM_PREFIXES.iter().any(|pre| s.starts_with(pre))
}
