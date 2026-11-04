//! The assets of a release, as `.github/workflows/release.yml` names them.

use super::{Version, REPO_URL};
use anyhow::{anyhow, Result};

/// A release and the names of its assets, as `.github/workflows/release.yml`
/// builds them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    pub tag: String,
    /// The CPU architecture of the assets, one of [`ARCHS`].
    pub arch: &'static str,
    /// The system of the assets, `linux` or `macos`.
    pub os: &'static str,
}

/// The CPU architectures that have a release binary, as
/// `std::env::consts::ARCH` names them.
pub const ARCHS: &[&str] = &["x86_64", "aarch64"];

/// The release architecture for `arch`, or an error for one without a
/// release binary.
pub fn release_arch(arch: &str) -> Result<&'static str> {
    ARCHS.iter().copied().find(|a| *a == arch).ok_or_else(|| {
        anyhow!(
            "paddock has no release binary for the {arch} architecture, only for {}. Build paddock from source with cargo instead",
            ARCHS.join(" and ")
        )
    })
}

impl Release {
    /// The release of a tag such as `v0.1.8` for the architecture `arch`
    /// (`std::env::consts::ARCH`) and the system of this binary. The tag
    /// must be a plain version: it goes into URLs and the tarball member.
    pub fn for_tag(tag: &str, arch: &str) -> Result<Self> {
        Self::for_target(tag, arch, std::env::consts::OS)
    }
    pub fn for_target(tag: &str, arch: &str, os: &'static str) -> Result<Self> {
        let arch = release_arch(arch)?;
        let v = Version::parse(tag).ok_or_else(|| {
            anyhow!("the latest release has the tag {tag:?}, which is not a version")
        })?;
        Ok(Self {
            version: v.to_string(),
            tag: format!("v{v}"),
            arch,
            os,
        })
    }
    fn stem(&self) -> String {
        format!("paddock-{}-{}-{}", self.version, self.arch, self.os)
    }
    pub fn tarball(&self) -> String {
        format!("{}.tar.gz", self.stem())
    }
    pub fn checksum(&self) -> String {
        format!("{}.sha256", self.tarball())
    }
    /// The binary inside the tarball.
    pub fn member(&self) -> String {
        format!("{}/paddock", self.stem())
    }
    fn asset_url(&self, name: &str) -> String {
        format!("{REPO_URL}/releases/download/{}/{name}", self.tag)
    }
    pub fn tarball_url(&self) -> String {
        self.asset_url(&self.tarball())
    }
    pub fn checksum_url(&self) -> String {
        self.asset_url(&self.checksum())
    }
}
