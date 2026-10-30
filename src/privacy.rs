//! Privacy defaults. paddock turns vendor telemetry, usage statistics and
//! crash reports off inside every sandbox, with the switches that each
//! vendor documents (the `[privacy]` table of a profile). The host
//! configuration is never changed: variables go into the sandbox
//! environment, and settings go into files in the sandbox home.
//!
//! Data use for training is an account setting on the vendor's website. No
//! program can change it, so paddock only says where it is
//! (`training_notes`, printed by `paddock doctor`).
//!
//! `[privacy] telemetry = "vendor"` in the user configuration keeps the
//! vendor defaults. A project paddock.toml cannot change it.

use crate::env::HostEnv;
use crate::manifest::Manifest;
use crate::profile::{HarnessFactory, Profile};
use crate::spec::{HomeSetting, SandboxSpec};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Telemetry {
    /// paddock turns vendor telemetry off in every sandbox.
    #[default]
    Off,
    /// The harness's own defaults apply.
    Vendor,
}

/// The mode of a merged manifest. Only the user configuration sets it (see
/// `Manifest::merge`).
pub fn mode(m: &Manifest) -> Telemetry {
    m.privacy.telemetry.unwrap_or_default()
}

/// The mode from the user configuration alone.
pub fn user_mode(env: &HostEnv) -> Telemetry {
    Manifest::load(env, &env.home.join(".paddock-no-project"))
        .map(|l| mode(&l.merged))
        .unwrap_or_default()
}

fn origin(profile: &Profile) -> String {
    format!("profile {} privacy (telemetry off)", profile.name)
}

/// Add the profile's telemetry switches to the spec: variables, and values
/// for settings files in the sandbox home. Nothing in vendor mode.
pub fn apply(profile: &Profile, mode: Telemetry, spec: &mut SandboxSpec) -> Result<()> {
    if mode == Telemetry::Vendor {
        return Ok(());
    }
    let origin = origin(profile);
    for (k, v) in &profile.privacy.env {
        spec.set_env(k, v.clone(), &origin);
    }
    push_settings(profile, &profile.privacy.settings, &origin, spec)
}

/// Add settings entries of a profile to the values that paddock writes into
/// the sandbox home.
pub fn push_settings(
    profile: &Profile,
    settings: &[crate::profile::Setting],
    origin: &str,
    spec: &mut SandboxSpec,
) -> Result<()> {
    for s in settings {
        let file = PathBuf::from(&s.file);
        if file.as_os_str().is_empty()
            || !file.components().all(|c| matches!(c, Component::Normal(_)))
        {
            bail!(
                "profile {}: privacy setting file {} must be a path inside the home",
                profile.name,
                s.file
            );
        }
        if s.format != "json" && s.format != "toml" {
            bail!(
                "profile {}: privacy setting format {} must be json or toml",
                profile.name,
                s.format
            );
        }
        let value = serde_json::to_value(&s.value)
            .with_context(|| format!("profile {}: privacy setting {}", profile.name, s.file))?;
        spec.home_settings.push(HomeSetting {
            file,
            format: s.format.clone(),
            path: crate::state::expand_path(&s.path, &spec.project),
            value,
            origin: origin.to_string(),
        });
    }
    Ok(())
}

/// Arguments that turn telemetry off, for the harness command line.
pub fn args(profile: &Profile, mode: Telemetry) -> Vec<String> {
    match mode {
        Telemetry::Off => profile.privacy.args.clone(),
        Telemetry::Vendor => vec![],
    }
}

/// Write the spec's settings into the sandbox home, keeping the rest of each
/// file. The home is sandbox-controlled: every access goes through `safefs`,
/// so a symlink planted there is never followed. A file that does not parse
/// is left as it is, with a warning: overwriting it would lose the user's
/// settings.
pub fn write_settings(home_src: &Path, settings: &[HomeSetting]) -> Result<Vec<String>> {
    let home = crate::safefs::Root::open(home_src)?;
    let mut warnings = Vec::new();
    let mut files: Vec<&PathBuf> = settings.iter().map(|s| &s.file).collect();
    files.sort();
    files.dedup();
    for rel in files {
        let entries: Vec<&HomeSetting> = settings.iter().filter(|s| &s.file == rel).collect();
        let text = match home.read(rel, crate::safefs::MAX_READ)? {
            Some(b) => String::from_utf8(b).unwrap_or_default(),
            None => String::new(),
        };
        let out = match entries[0].format.as_str() {
            "toml" => set_toml(&text, &entries),
            _ => set_json(&text, &entries),
        };
        match out {
            Ok(new) => home
                .write(rel, new.as_bytes(), 0o600)
                .with_context(|| format!("privacy setting in {}", rel.display()))?,
            Err(e) => warnings.push(format!(
                "telemetry stays at the vendor default in ~/{}: {e:#}",
                rel.display()
            )),
        }
    }
    Ok(warnings)
}

fn set_json(text: &str, entries: &[&HomeSetting]) -> Result<String> {
    let mut doc: serde_json::Value = if text.trim().is_empty() {
        serde_json::json!({})
    } else {
        serde_json::from_str(text).context("the file is not plain JSON")?
    };
    if !doc.is_object() {
        bail!("the file is not a JSON object");
    }
    for e in entries {
        crate::state::json_set(&mut doc, &e.path, e.value.clone());
    }
    Ok(serde_json::to_string_pretty(&doc)? + "\n")
}

fn toml_value(v: &serde_json::Value) -> Result<toml_edit::Value> {
    Ok(match v {
        serde_json::Value::Bool(b) => (*b).into(),
        serde_json::Value::String(s) => s.as_str().into(),
        serde_json::Value::Number(n) => match n.as_i64() {
            Some(i) => i.into(),
            None => n.as_f64().unwrap_or_default().into(),
        },
        _ => bail!("only strings, numbers and booleans are supported"),
    })
}

fn set_toml(text: &str, entries: &[&HomeSetting]) -> Result<String> {
    let mut doc: toml_edit::DocumentMut = text.parse().context("the file is not valid TOML")?;
    for e in entries {
        let Some((last, parents)) = e.path.split_last() else {
            continue;
        };
        let mut table = doc.as_table_mut();
        for k in parents {
            let item = table.entry(k).or_insert(toml_edit::table());
            if let Some(t) = item.as_table_mut() {
                t.set_implicit(true);
            }
            table = item
                .as_table_mut()
                .with_context(|| format!("{k} is not a table"))?;
        }
        table.insert(last, toml_edit::value(toml_value(&e.value)?));
    }
    Ok(doc.to_string())
}

/// Where one harness turns off the use of the user's data for training.
#[derive(Debug, Clone, Serialize)]
pub struct TrainingNote {
    pub harness: String,
    pub description: String,
    pub setting: String,
    pub url: String,
}

pub fn training_note(p: &Profile) -> Option<TrainingNote> {
    p.privacy.training.as_ref().map(|t| TrainingNote {
        harness: p.name.clone(),
        description: p.description.clone(),
        setting: t.setting.clone(),
        url: t.url.clone(),
    })
}

/// The training notes of every installed harness and app, for `paddock
/// doctor` and `paddock setup`.
pub fn training_notes(env: &HostEnv) -> Result<Vec<TrainingNote>> {
    let f = HarnessFactory::new(env)?;
    Ok(f.names()
        .iter()
        .filter(|n| f.create(n).is_ok())
        .filter_map(|n| f.profile(n).and_then(training_note))
        .collect())
}

/// One line about the telemetry mode, for `paddock doctor`.
pub fn telemetry_line(mode: Telemetry) -> &'static str {
    match mode {
        Telemetry::Off => "vendor telemetry is off in every sandbox; to keep it, set [privacy] telemetry = \"vendor\" in ~/.config/paddock/config.toml",
        Telemetry::Vendor => "vendor telemetry follows each program's own default ([privacy] telemetry = \"vendor\")",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setting(file: &str, format: &str, path: &[&str], value: serde_json::Value) -> HomeSetting {
        HomeSetting {
            file: file.into(),
            format: format.into(),
            path: path.iter().map(|s| s.to_string()).collect(),
            value,
            origin: "test".into(),
        }
    }

    #[test]
    fn settings_keep_the_rest_of_the_file() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path();
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        std::fs::write(home.join(".codex/config.toml"), "model = \"m\"\n").unwrap();
        std::fs::create_dir_all(home.join(".gemini")).unwrap();
        std::fs::write(home.join(".gemini/settings.json"), "{\"theme\": \"x\"}").unwrap();
        let s = vec![
            setting(
                ".codex/config.toml",
                "toml",
                &["analytics", "enabled"],
                false.into(),
            ),
            setting(
                ".gemini/settings.json",
                "json",
                &["privacy", "usageStatisticsEnabled"],
                false.into(),
            ),
        ];
        assert!(write_settings(home, &s).unwrap().is_empty());
        let codex = std::fs::read_to_string(home.join(".codex/config.toml")).unwrap();
        assert!(codex.contains("model = \"m\""));
        let v: toml::Value = toml::from_str(&codex).unwrap();
        assert_eq!(v["analytics"]["enabled"].as_bool(), Some(false));
        let g: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(home.join(".gemini/settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(g["theme"], "x");
        assert_eq!(g["privacy"]["usageStatisticsEnabled"], false);
    }

    #[test]
    fn a_file_that_does_not_parse_is_left_alone() {
        let t = tempfile::tempdir().unwrap();
        let rel = ".config/Code/User/settings.json";
        std::fs::create_dir_all(t.path().join(".config/Code/User")).unwrap();
        let text = "{ // a comment\n \"a\": 1 }";
        std::fs::write(t.path().join(rel), text).unwrap();
        let s = vec![setting(
            rel,
            "json",
            &["telemetry.telemetryLevel"],
            "off".into(),
        )];
        let w = write_settings(t.path(), &s).unwrap();
        assert_eq!(w.len(), 1);
        assert_eq!(std::fs::read_to_string(t.path().join(rel)).unwrap(), text);
    }

    #[test]
    fn settings_never_follow_a_planted_link_out_of_the_home() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("home");
        let outside = t.path().join("outside");
        std::fs::create_dir_all(home.join(".gemini")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("settings.json"), "{}").unwrap();
        // A link at the file, and a link at a folder on the way.
        std::os::unix::fs::symlink(
            outside.join("settings.json"),
            home.join(".gemini/settings.json"),
        )
        .unwrap();
        std::os::unix::fs::symlink(&outside, home.join(".codex")).unwrap();
        let s = vec![setting(
            ".gemini/settings.json",
            "json",
            &["a"],
            true.into(),
        )];
        assert!(write_settings(&home, &s).is_err());
        let s = vec![setting(".codex/config.toml", "toml", &["a"], true.into())];
        assert!(write_settings(&home, &s).is_err());
        assert_eq!(
            std::fs::read_to_string(outside.join("settings.json")).unwrap(),
            "{}"
        );
        assert!(!outside.join("config.toml").exists());
    }
}
