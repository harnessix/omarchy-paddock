//! Write the hooks of the sandbox copies at a start, after the settings of
//! the profile (privacy.rs `write_settings`). Each copy gets paddock's own
//! entries first, then the user's kept entries. The hooks of the copy are
//! built again at every start, so a broken or ignored entry from the seed
//! or from an earlier start does not stay.

use super::{Style, Target};
use crate::spec::{HomeSetting, SandboxSpec};
use anyhow::{Context, Result};
use serde_json::{json, Map, Value};
use std::path::Path;

/// Write every target of the spec into the sandbox home. Returns warnings.
pub fn write(spec: &SandboxSpec) -> Result<Vec<String>> {
    let mut warnings = Vec::new();
    if spec.hooks.is_empty() {
        return Ok(warnings);
    }
    let home = crate::safefs::Root::open(&spec.home_src)?;
    for t in &spec.hooks {
        // A snapshot's file comes into the upper folder first (shadow.rs).
        if let Some(p) = &spec.shadow {
            for f in t.files() {
                p.copy_up(&f)?;
            }
        }
        if let Err(e) = write_one(&home, spec, t) {
            warnings.push(format!(
                "the hooks of ~/{} stay as they are: {e:#}",
                t.into.display()
            ));
        }
    }
    Ok(warnings)
}

fn read_json(home: &crate::safefs::Root, rel: &Path) -> Result<Option<Value>> {
    let Some(b) = home.read(rel, crate::safefs::MAX_READ)? else {
        return Ok(None);
    };
    if b.iter().all(u8::is_ascii_whitespace) {
        return Ok(Some(json!({})));
    }
    let v: Value = serde_json::from_slice(&b).context("the file is not plain JSON")?;
    if !v.is_object() {
        anyhow::bail!("the file is not a JSON object");
    }
    Ok(Some(v))
}

/// Replace the hooks at `path` of a document.
fn set_section(doc: &mut Value, path: &[String], section: Value) {
    if path.is_empty() {
        *doc = section;
    } else {
        crate::state::json_set(doc, path, section);
    }
}

fn write_one(home: &crate::safefs::Root, spec: &SandboxSpec, t: &Target) -> Result<()> {
    // A copy of another hooks file (a snapshot of the host data has them)
    // loses its hooks: the user's kept entries are in `into`.
    for rel in t.sources.iter().filter(|r| **r != t.into) {
        if let Some(mut doc) = read_json(home, rel)? {
            set_section(&mut doc, &t.path, json!({}));
            home.write(
                rel,
                (serde_json::to_string_pretty(&doc)? + "\n").as_bytes(),
                0o600,
            )?;
        }
    }
    let paddock: Vec<&HomeSetting> = super::paddock_entries(spec, t).collect();
    let current = read_json(home, &t.into)?;
    if current.is_none() && paddock.is_empty() && t.groups.is_empty() {
        return trust(home, spec, t, &json!({}), &[]);
    }
    let mut doc = current.unwrap_or_else(|| json!({}));
    let mut section = Map::new();
    // (event, group, handler) of each entry that Codex trusts.
    let mut trusted: Vec<(String, usize, usize)> = Vec::new();
    match t.style {
        Style::Groups => {
            for s in &paddock {
                let key = s.path.last().cloned().unwrap_or_default();
                let list = section.entry(key.clone()).or_insert_with(|| json!([]));
                let a = list.as_array_mut().expect("an array");
                // One group (`append`), or the whole array of groups.
                for g in super::groups_of(&s.value) {
                    if !a.contains(g) {
                        let n = g["hooks"].as_array().map_or(0, Vec::len);
                        trusted.extend((0..n).map(|h| (key.clone(), a.len(), h)));
                        a.push(g.clone());
                    }
                }
            }
            for g in &t.groups {
                let list = section.entry(g.key.clone()).or_insert_with(|| json!([]));
                let a = list.as_array_mut().expect("an array");
                if a.contains(&g.value) {
                    continue;
                }
                for (h, yes) in g.trusted.iter().enumerate() {
                    if *yes {
                        trusted.push((g.key.clone(), a.len(), h));
                    }
                }
                a.push(g.value.clone());
            }
        }
        Style::Named => {
            for s in &paddock {
                let key = s.path.last().cloned().unwrap_or_default();
                section.insert(key, s.value.clone());
            }
            // The user's names get a prefix: they sort after paddock's own
            // entry, and none can replace it.
            for g in &t.groups {
                section.insert(format!("user-{}", g.key), g.value.clone());
            }
        }
    }
    set_section(&mut doc, &t.path, Value::Object(section));
    home.write(
        &t.into,
        (serde_json::to_string_pretty(&doc)? + "\n").as_bytes(),
        0o600,
    )?;
    trust(home, spec, t, &doc, &trusted)
}

/// Codex: drop every trust that came with the copy of `config.toml`, then
/// trust paddock's entries and the user's entries of a listed program.
fn trust(
    home: &crate::safefs::Root,
    spec: &SandboxSpec,
    t: &Target,
    doc: &Value,
    trusted: &[(String, usize, usize)],
) -> Result<()> {
    let Some(config) = &t.codex_trust else {
        return Ok(());
    };
    let text = home
        .read(config, crate::safefs::MAX_READ)?
        .map(|b| String::from_utf8(b).unwrap_or_default())
        .unwrap_or_default();
    let mut toml: toml_edit::DocumentMut = text.parse().context("config.toml is not valid TOML")?;
    let had = toml
        .get_mut("hooks")
        .and_then(|h| h.as_table_like_mut())
        .and_then(|h| h.remove("state"))
        .is_some();
    if !had && trusted.is_empty() {
        return Ok(());
    }
    let source = spec.home.join(&t.into).display().to_string();
    let section = crate::state::json_get(doc, &t.path)
        .cloned()
        .unwrap_or_default();
    let mut values = Vec::new();
    for (event, g, h) in trusted {
        let group = &section[event][*g];
        let matcher = group.get("matcher").and_then(Value::as_str);
        let entry = crate::compaction::codex::hook_key(&source, event, *g, *h).and_then(|key| {
            Ok((
                key,
                crate::compaction::codex::hook_hash(event, matcher, &group["hooks"][*h])?,
            ))
        });
        let Ok((key, hash)) = entry else {
            log::warn!("Codex does not trust the hook {event} {g}:{h} in the sandbox");
            continue;
        };
        values.push(HomeSetting {
            file: config.clone(),
            format: "toml".into(),
            path: vec!["hooks".into(), "state".into(), key, "trusted_hash".into()],
            value: json!(hash),
            origin: "hooks".into(),
            append: false,
            codex_trust: None,
        });
    }
    let refs: Vec<&HomeSetting> = values.iter().collect();
    let out = crate::privacy::set_toml(&toml.to_string(), &refs)
        .with_context(|| format!("~/{}", config.display()))?;
    home.write(config, out.as_bytes(), 0o600)
}
