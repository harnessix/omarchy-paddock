//! The guest files of the paddock VM.
//!
//! The files live in `<paddock data dir>/vm/`. The Homebrew formula can
//! also install them into its own share folder:
//! - `kernel`: the arm64 Linux `Image`, uncompressed (VZLinuxBootLoader)
//! - `initrd`: the initial ramdisk. It is optional.
//! - `rootfs.img`: the read-only root file system, raw ext4
//! - `assets.toml`: each file with its name, URL, sha256, size and version
//!
//! This module reads and checks the files. `paddock vm setup` installs
//! them (see [`super::assets_setup`]). No download happens at a run.

use crate::env::HostEnv;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};

/// The list of the guest files.
pub const MANIFEST: &str = "assets.toml";
/// The arm64 Linux `Image`, uncompressed.
pub const KERNEL: &str = "kernel";
/// The initial ramdisk. It is optional.
pub const INITRD: &str = "initrd";
/// The read-only root file system, raw ext4.
pub const ROOTFS: &str = "rootfs.img";

/// The file list that this paddock version installs with `paddock vm setup`.
#[cfg(not(target_env = "musl"))]
pub const PINNED: &str = include_str!("../../packaging/vm/assets.toml");
/// The guest binary (musl) is part of the root file system that the list
/// names. Without the list in it, its sha256 does not depend on the list.
#[cfg(target_env = "musl")]
pub const PINNED: &str = "";

/// One entry of `assets.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Asset {
    pub name: String,
    pub url: String,
    pub sha256: String,
    pub size: u64,
    pub version: String,
}

/// The parsed `assets.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, rename = "file")]
    pub files: Vec<Asset>,
}

impl Manifest {
    /// Parse and check a list: known names only, each name once, a kernel
    /// and a root file system, and sha256 values of 64 hex digits.
    pub fn parse(text: &str) -> Result<Self> {
        let m: Self = toml::from_str(text).context("cannot parse the list")?;
        for (i, a) in m.files.iter().enumerate() {
            // A known name only: a name could also be a path.
            if ![KERNEL, INITRD, ROOTFS].contains(&a.name.as_str()) {
                bail!("unknown VM file {:?}", a.name);
            }
            if m.files[..i].iter().any(|b| b.name == a.name) {
                bail!("{} is listed twice", a.name);
            }
            if a.sha256.len() != 64 || !a.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
                bail!("the sha256 of {} is not 64 hex digits", a.name);
            }
        }
        for name in [KERNEL, ROOTFS] {
            if m.get(name).is_none() {
                bail!("the list has no {name}");
            }
        }
        Ok(m)
    }

    /// Read and parse the `assets.toml` of a folder.
    pub fn read(dir: &Path) -> Result<Self> {
        let path = dir.join(MANIFEST);
        let text = std::fs::read_to_string(&path).with_context(|| {
            format!(
                "cannot read {}: the VM files are not set up",
                path.display()
            )
        })?;
        Self::parse(&text).with_context(|| path.display().to_string())
    }

    pub fn get(&self, name: &str) -> Option<&Asset> {
        self.files.iter().find(|a| a.name == name)
    }
}

/// Guest files that are present and match `assets.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assets {
    pub dir: PathBuf,
    pub kernel: PathBuf,
    pub initrd: Option<PathBuf>,
    pub rootfs: PathBuf,
    pub files: Vec<Asset>,
}

/// The folder that `paddock vm setup` installs into: `<paddock data dir>/vm/`.
pub fn default_assets_dir(env: &HostEnv) -> PathBuf {
    env.paddock_data().join("vm")
}

/// The share folders of the Homebrew formula, next to the binary first.
fn homebrew_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(prefix) = exe.parent().and_then(|p| p.parent()) {
            dirs.push(prefix.join("share/paddock/vm"));
        }
    }
    dirs.push(PathBuf::from("/opt/homebrew/share/paddock/vm"));
    dirs
}

/// The folder of the guest files: `<paddock data dir>/vm/` when it has an
/// `assets.toml`, else the Homebrew share folder that has one, else
/// `<paddock data dir>/vm/`.
pub fn assets_dir(env: &HostEnv) -> PathBuf {
    let user = default_assets_dir(env);
    if user.join(MANIFEST).exists() {
        return user;
    }
    homebrew_dirs()
        .into_iter()
        .find(|d| d.join(MANIFEST).exists())
        .unwrap_or(user)
}

/// Check that every file of `assets.toml` in `dir` is present and that its
/// size and sha256 match. The kernel and the root file system must be in
/// the list.
pub fn check(dir: &Path) -> Result<Assets> {
    let list = Manifest::read(dir)?;
    for a in &list.files {
        check_file(&dir.join(&a.name), a)?;
    }
    Ok(Assets {
        dir: dir.to_path_buf(),
        kernel: dir.join(KERNEL),
        initrd: list.get(INITRD).map(|_| dir.join(INITRD)),
        rootfs: dir.join(ROOTFS),
        files: list.files,
    })
}

/// Check one file against its entry: a regular file, the size, the sha256.
pub fn check_file(path: &Path, a: &Asset) -> Result<()> {
    let meta = std::fs::symlink_metadata(path)
        .with_context(|| format!("VM file {} is missing", path.display()))?;
    if !meta.is_file() {
        bail!("VM file {} is not a regular file", path.display());
    }
    if meta.len() != a.size {
        bail!(
            "VM file {} has {} bytes, {MANIFEST} says {}",
            path.display(),
            meta.len(),
            a.size
        );
    }
    let got = sha256_file(path)?;
    let want = a.sha256.to_ascii_lowercase();
    if got != want {
        bail!(
            "VM file {} does not match {MANIFEST}: sha256 {got}, expected {want}",
            path.display()
        );
    }
    Ok(())
}

/// The sha256 of a file, in lower case hex.
pub fn sha256_file(path: &Path) -> Result<String> {
    let mut f =
        std::fs::File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let mut hash = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f
            .read(&mut buf)
            .with_context(|| format!("cannot read {}", path.display()))?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    Ok(hex(&hash.finalize()))
}

/// The sha256 of bytes, in lower case hex.
pub fn sha256_bytes(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|b| format!("{b:02x}")).collect()
}

/// The lines of the guest files for `paddock vm status`.
pub fn status_lines(env: &HostEnv) -> Vec<String> {
    let dir = assets_dir(env);
    let mut out = vec![format!("VM files in {}:", dir.display())];
    match check(&dir) {
        Ok(a) => {
            for f in &a.files {
                out.push(format!(
                    "  ok       {:<11} {} bytes, version {}",
                    f.name, f.size, f.version
                ));
            }
        }
        Err(e) => {
            out.push(format!("  missing  {e:#}"));
            out.push("Run `paddock vm setup` to install them.".into());
        }
    }
    out
}

/// The line of the guest files for `paddock doctor` on macOS.
pub fn doctor_lines(env: &HostEnv) -> Vec<String> {
    let dir = assets_dir(env);
    match check(&dir) {
        Ok(_) => vec![format!("  ok       VM files in {}", dir.display())],
        Err(e) => vec![format!(
            "  missing  VM files: {e:#} (run: paddock vm setup)"
        )],
    }
}

#[cfg(test)]
#[path = "assets_tests.rs"]
pub(crate) mod tests;
