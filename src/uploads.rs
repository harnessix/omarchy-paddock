//! Uploads of session data that a harness does by default, beyond the model
//! requests. paddock turns them off in the sandbox copy of the harness
//! configuration. The host files never change.
//!
//! dsh mounts the plugin `session-log-deepseek` in every shipped profile.
//! It sends the session log with the requests to the official DeepSeek API
//! (`packages/session/session-log-deepseek/README.md` of
//! deepseek-ai/deepseek-harness). Its config key `enabled: false` stops it.
//! paddock adds that row to the sandbox copy of the home patch
//! `~/.dsh/cordis.patch.yml`, which dsh applies after the profile patch
//! (`apps/cli/src/profile-boot.ts`, `homePatchPath`).
//!
//! `[profiles.NAME] SWITCH = true` in the user configuration keeps the
//! upload on, for example `[profiles.dsh] upload_session_logs = true`.

use crate::env::HostEnv;
use crate::profile::Profile;
use crate::spec::SandboxSpec;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

/// `[[privacy.uploads]]` of a harness profile.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Upload {
    /// A Cordis patch file (YAML) relative to HOME, such as dsh's home patch.
    pub cordis_patch: String,
    /// The id of the plugin row that does the upload.
    pub row: String,
    /// The key in `[profiles.NAME]` of the user configuration that keeps
    /// the upload on.
    pub switch: String,
    /// What the upload is, for the start line.
    pub says: String,
}

/// What a start writes for one upload.
#[derive(Debug, Clone, Serialize)]
pub struct Off {
    pub file: PathBuf,
    pub row: String,
    /// True: paddock turns it off. False: paddock takes out its own row.
    pub off: bool,
}

/// `[profiles.NAME] SWITCH` of the user configuration text.
pub fn switch_in(text: &str, harness: &str, switch: &str) -> Result<bool> {
    let v: toml::Value = toml::from_str(text)?;
    match v
        .get("profiles")
        .and_then(|p| p.get(harness))
        .and_then(|t| t.get(switch))
    {
        None => Ok(false),
        Some(toml::Value::Boolean(b)) => Ok(*b),
        Some(_) => anyhow::bail!("[profiles.{harness}] {switch} must be true or false"),
    }
}

/// Note the uploads of a profile, and the start line for each one that
/// paddock turns off.
pub fn apply(env: &HostEnv, profile: &Profile, spec: &mut SandboxSpec) -> Result<()> {
    if profile.privacy.uploads.is_empty() {
        return Ok(());
    }
    let text =
        std::fs::read_to_string(env.paddock_config().join("config.toml")).unwrap_or_default();
    for u in &profile.privacy.uploads {
        let file = PathBuf::from(&u.cordis_patch);
        if !file.components().all(|c| matches!(c, Component::Normal(_)))
            || file.as_os_str().is_empty()
        {
            anyhow::bail!(
                "profile {}: privacy upload file {} must be a path inside the home",
                profile.name,
                u.cordis_patch
            );
        }
        let on = switch_in(&text, &profile.name, &u.switch).unwrap_or_else(|e| {
            spec.warnings
                .push(format!("{e:#}; paddock turns the upload off"));
            false
        });
        if !on {
            spec.notices.push(format!(
                "{}: paddock turned off {} in the sandbox. To keep it on, set [profiles.{}] {} = true in the user configuration",
                profile.name, u.says, profile.name, u.switch
            ));
        }
        spec.uploads.push(Off {
            file,
            row: u.row.clone(),
            off: !on,
        });
    }
    Ok(())
}

const BEGIN: &str = "# paddock: begin";
const END: &str = "# paddock: end";

/// paddock's row, with the markers that let a later start find it.
fn block(row: &str) -> String {
    format!(
        "{BEGIN}. paddock turns this upload off in the sandbox. The user configuration\n\
         # can keep it on ([profiles.NAME] in paddock's config.toml).\n\
         - id: {row}\n  config:\n    enabled: false\n{END}\n"
    )
}

/// The text without paddock's earlier rows.
fn strip(text: &str) -> String {
    let mut out = String::new();
    let mut inside = false;
    for l in text.split_inclusive('\n') {
        if l.starts_with(BEGIN) {
            inside = true;
        } else if inside && l.trim_end() == END {
            inside = false;
        } else if !inside {
            out.push_str(l);
        }
    }
    out
}

fn is_comment(l: &str) -> bool {
    let t = l.trim();
    t.is_empty() || t.starts_with('#')
}

/// The new text of a Cordis patch file. A patch file is a YAML list of
/// rows. paddock adds its row at the end, so it wins over an earlier row
/// with the same id. Errors leave the file as it is.
pub fn patch_text(text: &str, row: &str, off: bool) -> Result<Option<String>> {
    let rest = strip(text);
    let had = rest != text;
    let body: Vec<&str> = rest.lines().filter(|l| !is_comment(l)).collect();
    if !off {
        return Ok(match (had, body.is_empty()) {
            (false, _) => None,
            // A file with comments only stops dsh at boot: `[]` is no row.
            (true, true) => {
                let mut r = rest.trim_end().to_string();
                if !r.is_empty() {
                    r.push('\n');
                }
                Some(r + "[]\n")
            }
            (true, false) => Some(rest),
        });
    }
    let base = if body.is_empty() {
        rest.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| format!("{l}\n"))
            .collect::<String>()
    } else if body.iter().all(|l| l.trim() == "[]") {
        rest.lines()
            .filter(|l| l.trim() != "[]")
            .map(|l| format!("{l}\n"))
            .collect()
    } else if body[0].starts_with("- ") || body[0] == "-" {
        let mut r = rest;
        if !r.is_empty() && !r.ends_with('\n') {
            r.push('\n');
        }
        r
    } else {
        // A list in JSON form: one row per line in flow style.
        let rows: Vec<serde_json::Value> = serde_json::from_str(&body.join("\n"))
            .map_err(|_| anyhow::anyhow!("it is not a list of rows that paddock can extend"))?;
        rows.iter().map(|r| format!("- {r}\n")).collect()
    };
    Ok(Some(base + &block(row)))
}

/// Write the uploads of the spec into the sandbox home. Returns warnings.
pub fn write(spec: &SandboxSpec) -> Result<Vec<String>> {
    let mut warnings = Vec::new();
    if spec.uploads.is_empty() {
        return Ok(warnings);
    }
    let home = crate::safefs::Root::open(&spec.home_src)?;
    for u in &spec.uploads {
        if let Some(p) = &spec.shadow {
            p.copy_up(&u.file)?;
        }
        if let Err(e) = write_one(&home, u) {
            warnings.push(format!(
                "the upload of {} stays as ~/{} has it: {e:#}",
                u.row,
                u.file.display()
            ));
        }
    }
    Ok(warnings)
}

fn write_one(home: &crate::safefs::Root, u: &Off) -> Result<()> {
    let text = match home.read(&u.file, crate::safefs::MAX_READ)? {
        Some(b) => String::from_utf8(b).map_err(|_| anyhow::anyhow!("it is not UTF-8"))?,
        None => String::new(),
    };
    if let Some(new) = patch_text(&text, &u.row, u.off)? {
        home.write(Path::new(&u.file), new.as_bytes(), 0o600)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
