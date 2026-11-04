//! The user of a run in the guest, from what the project mount shows.
//!
//! virtiofs of a Linux host shows the host owner of each file, so the run
//! uses that uid. The Virtualization framework of macOS can show the files
//! as owned by root: it writes each change as the host user that runs
//! `paddock-vm`, whatever the guest uid. Then the run uses the fixed guest
//! user when that user can write the project root.

use anyhow::{bail, Result};
use std::path::Path;

/// The fixed guest user of the image (packaging/vm/build.sh).
pub const GUEST_USER: (u32, u32) = (1000, 1000);

/// What the guest sees of the project root after the mount.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seen {
    pub uid: u32,
    pub gid: u32,
    /// The permission bits.
    pub mode: u32,
    /// The file system type of the mount at the project, from
    /// /proc/self/mounts. None: nothing is mounted there.
    pub fs: Option<String>,
}

impl Seen {
    /// Read it in the guest.
    pub fn read(project: &Path) -> Result<Self> {
        use anyhow::Context;
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::metadata(project)
            .with_context(|| format!("cannot read {}", project.display()))?;
        let mounts = std::fs::read_to_string("/proc/self/mounts").unwrap_or_default();
        Ok(Self {
            uid: m.uid(),
            gid: m.gid(),
            mode: m.mode() & 0o7777,
            fs: fs_type(&mounts, project),
        })
    }

    /// A short text for the console: `owner 501:20 mode 0755 fs virtiofs`.
    pub fn line(&self) -> String {
        format!(
            "owner {}:{} mode {:04o} fs {}",
            self.uid,
            self.gid,
            self.mode,
            self.fs.as_deref().unwrap_or("none")
        )
    }
}

/// The type of the last mount at `dir` in the text of /proc/self/mounts.
pub fn fs_type(mounts: &str, dir: &Path) -> Option<String> {
    let want = dir.to_str()?;
    mounts
        .lines()
        .filter_map(|l| {
            let mut f = l.split(' ');
            let (_src, at, ty) = (f.next()?, f.next()?, f.next()?);
            (unescape(at) == want).then(|| ty.to_string())
        })
        .next_back()
}

/// /proc/self/mounts writes space, tab, newline and backslash as `\ooo`.
fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let oct = b
            .get(i + 1..i + 4)
            .filter(|o| o.iter().all(|c| (b'0'..=b'7').contains(c)));
        match oct {
            Some(o) if b[i] == b'\\' => {
                out.push((o[0] - b'0') * 64 + (o[1] - b'0') * 8 + (o[2] - b'0'));
                i += 4;
            }
            _ => {
                out.push(b[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The user of a run. `host_owner` is the owner of the project on the
/// host, from the run request. `guest_can_write` tests if [`GUEST_USER`]
/// can write the project root; it runs only when it matters.
pub fn choose(
    project: &Path,
    seen: &Seen,
    host_owner: Option<u32>,
    guest_can_write: impl FnOnce() -> Result<bool>,
) -> Result<(u32, u32)> {
    let p = project.display();
    if host_owner == Some(0) {
        bail!("the project {p} belongs to root on the host: the VM runs only as a normal user");
    }
    if seen.uid != 0 {
        return Ok((seen.uid, seen.gid));
    }
    match seen.fs.as_deref() {
        Some("virtiofs") => {}
        Some(fs) => bail!(
            "the project {p} is on {fs} in the guest, not on virtiofs, and belongs to root there: the share did not mount"
        ),
        None => bail!(
            "nothing is mounted at {p} in the guest, and the folder belongs to root: the share did not mount"
        ),
    }
    if guest_can_write()? {
        return Ok(GUEST_USER);
    }
    bail!(
        "virtiofs shows the project {p} as owned by root with mode {:04o}, and the guest user uid {} cannot write it",
        seen.mode,
        GUEST_USER.0
    )
}

/// Can `user` make and remove a file in `dir`? A child process with that
/// uid and gid tries it, so the test goes through the same permission
/// checks as the run.
pub fn can_write(dir: &Path, user: (u32, u32)) -> Result<bool> {
    use anyhow::Context;
    use std::os::unix::process::CommandExt;
    let status = std::process::Command::new("/bin/sh")
        .args([
            "-c",
            "f=\"$1/.paddock-vm-probe-$$\"; : > \"$f\" && rm -f \"$f\"",
            "sh",
        ])
        .arg(dir)
        .uid(user.0)
        .gid(user.1)
        .env_clear()
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .context("cannot start the write probe")?;
    Ok(status.success())
}

#[cfg(test)]
#[path = "agent_user_tests.rs"]
mod tests;
