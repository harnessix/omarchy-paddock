//! Grok Build's folder trust. Grok Build 1.0.70 runs the hooks of
//! `~/.grok/hooks` only in a trusted folder (user guide 10-hooks.md, Hook
//! Locations). It keeps the trust in `~/.grok/trusted_folders.toml`:
//!
//! ```toml
//! [folders."/abs/repo/root"]
//! trusted = true
//! decided_at = 1780000000
//! ```
//!
//! (grok-build `xai-grok-permission-rules/src/trust.rs`). A grant covers
//! the folder and its subfolders in the same git repository.
//!
//! When the sandbox copy has paddock's hook or an entry of `[hooks]
//! programs`, paddock writes this file in the sandbox home with one entry:
//! the project folder. The trust turns on every hook source of the user,
//! so the sandbox copy keeps no other user hook: hooks.rs leaves out the
//! entries of `~/.grok/hooks` that run no listed program, and the `hooks`
//! table of each `drop_hooks` file goes. The host files never change.

use super::Target;
use crate::spec::SandboxSpec;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// `[hooks.folder_trust]` of a harness profile.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FolderTrust {
    /// The trust file, relative to HOME.
    pub file: String,
    /// TOML files, relative to HOME, whose `hooks` table the trust would
    /// turn on. The sandbox copy loses that table.
    #[serde(default)]
    pub drop_hooks: Vec<String>,
}

impl FolderTrust {
    pub fn files(&self) -> impl Iterator<Item = &String> {
        std::iter::once(&self.file).chain(&self.drop_hooks)
    }
}

/// The text of the trust file for `project`.
pub fn text(project: &Path, now: i64) -> String {
    let mut doc = toml_edit::DocumentMut::new();
    let mut folders = toml_edit::Table::new();
    folders.set_implicit(true);
    let mut entry = toml_edit::Table::new();
    entry.insert("trusted", toml_edit::value(true));
    entry.insert("decided_at", toml_edit::value(now));
    folders.insert(
        &project.display().to_string(),
        toml_edit::Item::Table(entry),
    );
    doc.insert("folders", toml_edit::Item::Table(folders));
    format!(
        "# paddock writes this file at each start of a sandbox. It trusts the\n\
         # project folder for paddock's own hook and the hooks of [hooks] programs.\n{doc}"
    )
}

/// Write the trust of target `t`, when `needed`: paddock's own hook or a
/// listed entry is in the sandbox copy.
pub fn write(
    home: &crate::safefs::Root,
    spec: &SandboxSpec,
    t: &Target,
    needed: bool,
) -> Result<()> {
    let Some(f) = &t.folder_trust else {
        return Ok(());
    };
    if !needed {
        return Ok(());
    }
    for rel in &f.drop_hooks {
        let rel = Path::new(rel);
        let Some(b) = home.read(rel, crate::safefs::MAX_READ)? else {
            continue;
        };
        let text = String::from_utf8(b).unwrap_or_default();
        let mut doc: toml_edit::DocumentMut = text
            .parse()
            .with_context(|| format!("~/{} is not valid TOML", rel.display()))?;
        if doc.remove("hooks").is_some() {
            home.write(rel, doc.to_string().as_bytes(), 0o600)?;
        }
    }
    let project = std::fs::canonicalize(&spec.project).unwrap_or_else(|_| spec.project.clone());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    home.write(Path::new(&f.file), text(&project, now).as_bytes(), 0o600)
}
