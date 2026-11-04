//! `paddock vm setup` and `paddock vm remove`: install and remove the guest
//! files. A setup downloads the files of the pinned list, or copies them
//! from a folder that `packaging/vm/build.sh` made. In both cases each file
//! must match its size and sha256 before it replaces an installed one.

use super::assets::{self, Assets, Manifest, INITRD, KERNEL, MANIFEST, ROOTFS};
use crate::env::HostEnv;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

/// Where a setup gets the files.
pub enum Source<'a> {
    /// Download each file from its URL in the list.
    Download(&'a dyn Fn(&str) -> Result<Vec<u8>>),
    /// Copy each file from a local folder with its own `assets.toml`.
    Dir(PathBuf),
}

/// The lines that a setup shows before its question: each file with its
/// origin, size and sha256.
pub fn plan_lines(m: &Manifest, src: &Source, target: &Path) -> Vec<String> {
    let mut out = vec![format!(
        "paddock vm setup installs into {}:",
        target.display()
    )];
    for a in &m.files {
        out.push(format!("  {} (version {})", a.name, a.version));
        match src {
            Source::Download(_) => out.push(format!("    URL:    {}", a.url)),
            Source::Dir(d) => out.push(format!("    file:   {}", d.join(&a.name).display())),
        }
        out.push(format!("    size:   {} bytes", a.size));
        out.push(format!("    sha256: {}", a.sha256));
    }
    out
}

/// Install the files of `m` into `target`. Each file goes into a staging
/// folder next to `target` first and must match the list there. Only then
/// do the files replace the installed ones, `assets.toml` last.
pub fn install(env: &HostEnv, m: &Manifest, src: &Source, target: &Path) -> Result<Assets> {
    crate::update::refuse_in_sandbox(env, "paddock vm setup")?;
    let parent = target
        .parent()
        .context("the VM folder has no parent folder")?;
    crate::privfs::dir(parent)?;
    let staging = parent.join(format!(".vm-setup-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    crate::privfs::dir(&staging)?;
    let r = stage(m, src, &staging).and_then(|()| replace(m, &staging, target));
    let _ = std::fs::remove_dir_all(&staging);
    r?;
    assets::check(target)
}

fn stage(m: &Manifest, src: &Source, staging: &Path) -> Result<()> {
    for a in &m.files {
        let dest = staging.join(&a.name);
        match src {
            Source::Download(get) => {
                println!("Downloading {}", a.url);
                let bytes = get(&a.url)?;
                if bytes.len() as u64 != a.size {
                    bail!(
                        "{} has {} bytes, the list says {}",
                        a.url,
                        bytes.len(),
                        a.size
                    );
                }
                let got = assets::sha256_bytes(&bytes);
                if !got.eq_ignore_ascii_case(&a.sha256) {
                    bail!(
                        "{} does not match the list: sha256 {got}, expected {}",
                        a.url,
                        a.sha256
                    );
                }
                crate::privfs::write(&dest, &bytes)?;
            }
            Source::Dir(dir) => {
                let from = dir.join(&a.name);
                // The same checks as at a boot, before and after the copy.
                assets::check_file(&from, a)?;
                println!("Copying {}", from.display());
                crate::privfs::copy_atomic(&from, &dest)
                    .with_context(|| format!("cannot copy {}", from.display()))?;
            }
        }
        assets::check_file(&dest, a)?;
    }
    let text = toml::to_string(m)?;
    crate::privfs::write(&staging.join(MANIFEST), text)?;
    Ok(())
}

fn replace(m: &Manifest, staging: &Path, target: &Path) -> Result<()> {
    crate::privfs::dir(target)?;
    // Without the list, a half replaced folder never passes a check.
    remove_if_there(&target.join(MANIFEST))?;
    for name in [KERNEL, INITRD, ROOTFS] {
        let to = target.join(name);
        if m.get(name).is_some() {
            std::fs::rename(staging.join(name), &to)
                .with_context(|| format!("cannot move {name} into {}", target.display()))?;
        } else {
            remove_if_there(&to)?;
        }
    }
    std::fs::rename(staging.join(MANIFEST), target.join(MANIFEST))
        .with_context(|| format!("cannot move {MANIFEST} into {}", target.display()))?;
    Ok(())
}

fn remove_if_there(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("cannot remove {}", path.display()))
        }
        _ => Ok(()),
    }
}

/// The guest files that `dir` holds, by name.
pub fn present(dir: &Path) -> Vec<&'static str> {
    [MANIFEST, KERNEL, INITRD, ROOTFS]
        .into_iter()
        .filter(|n| std::fs::symlink_metadata(dir.join(n)).is_ok())
        .collect()
}

/// Remove the guest files from `dir`, and `dir` when it is empty then.
/// Other files stay. False when there was nothing to remove.
pub fn remove(env: &HostEnv, dir: &Path) -> Result<bool> {
    crate::update::refuse_in_sandbox(env, "paddock vm remove")?;
    let names = present(dir);
    if names.is_empty() {
        return Ok(false);
    }
    for name in names {
        remove_if_there(&dir.join(name))?;
    }
    let _ = std::fs::remove_dir(dir);
    Ok(true)
}

#[cfg(test)]
#[path = "assets_setup_tests.rs"]
mod tests;
