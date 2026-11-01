//! Pastures: sandboxes that the user puts together on purpose.
//!
//! A pasture is defined in the user configuration, never in a project
//! `paddock.toml`:
//!
//! ```toml
//! [pastures.review]
//! members = ["claude", "codex"]
//! ```
//!
//! `paddock <harness> --pasture review` starts a sandbox in the pasture
//! `review` of its project. A pasture belongs to one project: the same name
//! in two projects is two pastures. The members of one pasture share the
//! `pasture` scope of paddock kv. A sandbox started without `--pasture` is a
//! pasture of its own, so by default no two sandboxes share kv data.

pub mod bridge;
pub mod ports;

use crate::env::HostEnv;
use anyhow::{bail, Context as _, Result};
use serde::Deserialize;
use std::collections::BTreeMap;

/// The longest pasture name.
pub const MAX_NAME: usize = 32;
/// Names that start with this belong to the pasture of a single sandbox
/// (`solo-<sandbox id>`), so a configured pasture cannot use them.
pub const SOLO_PREFIX: &str = "solo-";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pasture {
    /// The harnesses that can join. Empty: none can.
    pub members: Vec<String>,
    /// Whether a member can ask another member a question (bridge.rs).
    pub bridge: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPasture {
    #[serde(default)]
    members: Vec<String>,
    #[serde(default)]
    bridge: bool,
}

/// Where the shared folder of a pasture appears in each member.
pub const MOUNT: &str = "/pasture";

/// The shared folder of a pasture on the host:
/// `~/.local/share/paddock/pastures/<project hash>/<name>/ground`.
pub fn ground(env: &HostEnv, project: &std::path::Path, name: &str) -> std::path::PathBuf {
    folder(env, project, name).join("ground")
}

/// Everything paddock keeps for one pasture outside paddock kv.
pub fn folder(env: &HostEnv, project: &std::path::Path, name: &str) -> std::path::PathBuf {
    env.paddock_data()
        .join("pastures")
        .join(crate::kv::shard::project_hash(project))
        .join(name)
}

/// Whether `name` can name a configured pasture: lowercase letters, digits
/// and `-`, starting with a letter, and not `solo-`.
pub fn check_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= MAX_NAME
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !ok {
        bail!(
            "'{name}' is not a pasture name: use 1 to {MAX_NAME} lowercase letters, digits and '-', starting with a letter"
        );
    }
    if name.starts_with(SOLO_PREFIX) {
        bail!("pasture names that start with '{SOLO_PREFIX}' are reserved for single sandboxes");
    }
    Ok(())
}

/// `[pastures.*]` of the user configuration.
pub fn parse(text: &str) -> Result<BTreeMap<String, Pasture>> {
    let v: toml::Value = toml::from_str(text)?;
    let Some(t) = v.get("pastures") else {
        return Ok(BTreeMap::new());
    };
    let raw: BTreeMap<String, RawPasture> = t.clone().try_into()?;
    let mut out = BTreeMap::new();
    for (name, p) in raw {
        check_name(&name)?;
        out.insert(
            name,
            Pasture {
                members: p.members,
                bridge: p.bridge,
            },
        );
    }
    Ok(out)
}

pub fn load(env: &HostEnv) -> Result<BTreeMap<String, Pasture>> {
    let file = env.paddock_config().join("config.toml");
    let Ok(text) = std::fs::read_to_string(&file) else {
        return Ok(BTreeMap::new());
    };
    parse(&text).with_context(|| format!("[pastures] in {} is invalid", file.display()))
}

/// Check that `harness` can join the pasture `name`. The error says what
/// to add to the user configuration.
pub fn admit(env: &HostEnv, name: &str, harness: &str) -> Result<()> {
    check_name(name)?;
    let all = load(env)?;
    let file = env.paddock_config().join("config.toml");
    let Some(p) = all.get(name) else {
        bail!(
            "there is no pasture '{name}'. Define it in {}:\n\n[pastures.{name}]\nmembers = [\"{harness}\"]",
            file.display()
        );
    };
    if !p.members.iter().any(|m| m == harness) {
        bail!(
            "{harness} is not a member of the pasture '{name}'. Add it to members of [pastures.{name}] in {}",
            file.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        for ok in ["review", "a", "team-2"] {
            check_name(ok).unwrap();
        }
        for bad in [
            "",
            "Review",
            "2team",
            "a b",
            "a/b",
            "solo-x",
            &"a".repeat(33),
        ] {
            assert!(check_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn admits_members_only() {
        let tmp = tempfile::tempdir().unwrap();
        let env = HostEnv::from_parts(
            tmp.path().join("home"),
            "t".into(),
            1000,
            1000,
            Default::default(),
        );
        let cfg = env.paddock_config();
        std::fs::create_dir_all(&cfg).unwrap();
        assert!(admit(&env, "review", "codex")
            .unwrap_err()
            .to_string()
            .contains("[pastures.review]"));
        std::fs::write(
            cfg.join("config.toml"),
            "[pastures.review]\nmembers = [\"claude\"]\n",
        )
        .unwrap();
        admit(&env, "review", "claude").unwrap();
        assert!(admit(&env, "review", "codex").is_err());
        assert!(admit(&env, "solo-x", "claude").is_err());
    }

    #[test]
    fn parses_members() {
        let p = parse("[pastures.review]\nmembers = [\"claude\", \"codex\"]\n").unwrap();
        assert_eq!(p["review"].members, ["claude", "codex"]);
        assert!(parse("[pastures.Bad]\nmembers = []\n").is_err());
        assert!(parse("[pastures.x]\nother = 1\n").is_err());
        assert!(parse("").unwrap().is_empty());
    }
}
