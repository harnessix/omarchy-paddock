//! `paddock spec adopt`: move an in-repo `.specify/` and `specs/` into the
//! external store, once, after a question. It never rewrites git history:
//! when the files were committed, it says how to drop them.

use super::{store_dir, Cfg, Store, SPECIFY, SPECS};
use crate::env::HostEnv;
use anyhow::{bail, Context as _, Result};
use std::path::Path;

/// Copy a tree of the user's own files. Symlinks and special files stay
/// behind, with a warning.
fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    crate::privfs::dir(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let (src, dst) = (e.path(), to.join(e.file_name()));
        let t = e.file_type()?;
        if t.is_dir() {
            copy_tree(&src, &dst)?;
        } else if t.is_file() {
            std::fs::copy(&src, &dst).with_context(|| format!("cannot copy {}", src.display()))?;
        } else {
            log::warn!("{} is not a file or a folder; it stays", src.display());
        }
    }
    Ok(())
}

/// Move one folder: a rename, or a copy and a removal across file systems.
fn move_dir(from: &Path, to: &Path) -> Result<()> {
    if let Some(p) = to.parent() {
        crate::privfs::dir(p)?;
    }
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    copy_tree(from, to)?;
    std::fs::remove_dir_all(from).with_context(|| format!("cannot remove {}", from.display()))
}

/// Move the Spec Kit files of the repository at `project` into the store.
/// `ask` puts the question; false stops without a change. Returns what
/// the user must know.
pub fn adopt(env: &HostEnv, project: &Path, ask: &mut dyn FnMut(&str) -> bool) -> Result<String> {
    if Cfg::load(env).store != Store::External {
        bail!("[speckit] store is not \"external\" in the user configuration. Add this to {} first:\n\n[speckit]\nstore = \"external\"", env.paddock_config().join("config.toml").display());
    }
    let layout = crate::hostgit::Layout::discover(project)?;
    let repo = match &layout {
        Some(l) if l.linked => bail!(
            "{} is a worktree: run adopt in the main work tree",
            project.display()
        ),
        Some(l) => l.work_tree.clone(),
        None => project.to_path_buf(),
    };
    let src = repo.join(SPECIFY);
    if !std::fs::symlink_metadata(&src).is_ok_and(|m| m.is_dir()) {
        bail!("{} has no {SPECIFY}/ folder", repo.display());
    }
    let store = store_dir(env, &repo);
    if store.join(SPECIFY).exists() {
        bail!("the store {} has a {SPECIFY}/ already", store.display());
    }
    let specs = repo.join(SPECS);
    let has_specs = std::fs::symlink_metadata(&specs).is_ok_and(|m| m.is_dir());
    let what = if has_specs {
        format!("{SPECIFY}/ and {SPECS}/")
    } else {
        format!("{SPECIFY}/")
    };
    if !ask(&format!(
        "Move {what} of {} to the paddock store {}? Every sandbox of this repository sees them there.",
        repo.display(),
        store.display()
    )) {
        return Ok("Nothing moved.".into());
    }
    let tracked = layout
        .as_ref()
        .and_then(|l| l.try_query(&["ls-files", "--", SPECIFY, SPECS]))
        .is_some_and(|o| !o.trim().is_empty());
    move_dir(&src, &store.join(SPECIFY))?;
    if has_specs {
        move_dir(&specs, &store.join(SPECS))?;
    }
    super::exclude(&repo)?;
    let mut msg = format!(
        "Moved {what} to {}. paddock mounts them into each sandbox of this repository, and info/exclude keeps them out of git.",
        store.display()
    );
    if tracked {
        msg.push_str(&format!(
            "\n\nThese files were in git. Remove them from the index (the files stay in the store):\n  git rm -r -q --cached {SPECIFY} {SPECS}\n  git commit -m \"Move the Spec Kit files out of the repository\"\nThey stay in the history. To remove them from it, rewrite the history yourself, for example with\n  git filter-repo --invert-paths --path {SPECIFY} --path {SPECS}\nThat changes every commit id, so do it only on a repository whose other copies you can replace."
        ));
    }
    Ok(msg)
}
