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
//!
//! `workbench = true` in `[pastures.NAME]` allows a shared terminal that
//! belongs to no member (workbench.rs).
//!
//! `[pastures.NAME.rations]` gives a member a ration: a timer named
//! `ration` that the user owns, set when the member starts (timer.rs).

pub mod bridge;
pub mod ports;
pub mod workbench;

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
    /// The ration of each member harness that has one.
    pub rations: BTreeMap<String, Ration>,
    /// Whether the pasture can have a workbench (workbench.rs).
    pub workbench: bool,
}

/// What a member can graze: the budgets of its `ration` timer.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Ration {
    /// Seconds; 0: no time budget.
    pub budget: u64,
    /// A window and a share of it, as `timer::parse_quota` gives them.
    pub quota: Option<(String, f64)>,
    pub hard: bool,
}

impl Eq for Ration {}

/// The name of the timer that holds a ration.
pub const RATION: &str = "ration";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPasture {
    #[serde(default)]
    members: Vec<String>,
    #[serde(default)]
    bridge: bool,
    #[serde(default)]
    rations: BTreeMap<String, RawRation>,
    #[serde(default)]
    workbench: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRation {
    quota: Option<String>,
    budget: Option<String>,
    #[serde(default)]
    hard: bool,
}

fn ration_of(pasture: &str, member: &str, members: &[String], r: RawRation) -> Result<Ration> {
    let at = format!("[pastures.{pasture}.rations] {member}");
    if !members.iter().any(|m| m == member) {
        bail!("{at}: {member} is not in members of [pastures.{pasture}]");
    }
    let budget = match &r.budget {
        Some(b) => {
            let s = crate::timer::parse_duration(b).with_context(|| at.clone())?;
            crate::timer::check_budget(s).with_context(|| at.clone())?;
            s
        }
        None => 0,
    };
    let quota = match &r.quota {
        Some(q) => Some(crate::timer::parse_quota(q).with_context(|| at.clone())?),
        None => None,
    };
    if budget == 0 && quota.is_none() {
        bail!("{at}: a ration needs a quota (for example \"weekly=1%\"), a budget (for example \"2h\"), or both");
    }
    Ok(Ration {
        budget,
        quota,
        hard: r.hard,
    })
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
    let tbl = t.as_table().context("[pastures] is not a table")?;
    let mut out = BTreeMap::new();
    for (name, v) in tbl {
        check_name(name)?;
        let p: RawPasture = v
            .clone()
            .try_into()
            .with_context(|| format!("[pastures.{name}] is invalid"))?;
        if p.members.iter().any(|m| m == workbench::NAME) {
            bail!(
                "[pastures.{name}]: '{}' is the shared terminal of a pasture, not a member: set workbench = true",
                workbench::NAME
            );
        }
        let mut rations = BTreeMap::new();
        for (member, r) in p.rations {
            let ration = ration_of(name, &member, &p.members, r)?;
            rations.insert(member, ration);
        }
        out.insert(
            name.clone(),
            Pasture {
                members: p.members,
                bridge: p.bridge,
                rations,
                workbench: p.workbench,
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

/// Set the ration of a member that just started: the timer `ration` in
/// its own scope, owned by the user. Without usage numbers for the account
/// the quota part is left out. Returns the notices for the user.
pub fn apply_ration(env: &HostEnv, id: &str, harness: &str, r: &Ration) -> Result<Vec<String>> {
    let mut notes = Vec::new();
    let agent = crate::forage::agent_of(harness);
    let quota = match &r.quota {
        Some(q) if crate::forage::windows(env, &agent).is_empty() => {
            notes.push(format!(
                "paddock has no usage numbers for {agent}, so the ration of {harness} counts no quota ({}={:.0}%)",
                q.0,
                q.1 * 100.0
            ));
            None
        }
        q => q.clone(),
    };
    if r.budget == 0 && quota.is_none() {
        return Ok(notes);
    }
    let s = crate::timer::host_set(env, id, RATION, r.budget, r.hard, quota)?;
    notes.push(format!(
        "ration of {harness}: {}{}",
        crate::timer::describe(&s),
        if r.hard { ", hard" } else { "" }
    ));
    Ok(notes)
}

/// The status of the `ration` timer of a sandbox, if it has one.
pub fn ration_status(env: &HostEnv, sandbox: &str) -> Option<crate::timer::Status> {
    let (file, ns) = crate::timer::self_file(env, sandbox).ok()?;
    if !file.is_file() {
        return None;
    }
    let st = crate::kv::store::Store::open(&file).ok()?;
    let now = crate::state::now();
    let e = st
        .get(&ns, &format!("{}{RATION}", crate::timer::PREFIX), now)
        .ok()??;
    let t: crate::timer::Timer = serde_json::from_str(&e.value).ok()?;
    Some(t.status_in(env, now))
}

/// A ration as `pasture list` and `pasture members` show it.
#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
pub struct RationUse {
    /// "10m of 2h, 0% of 1% of claude Weekly".
    pub used: String,
    pub over: bool,
    pub hard: bool,
}

pub fn ration_use(env: &HostEnv, sandbox: &str) -> Option<RationUse> {
    ration_status(env, sandbox).map(|s| RationUse {
        used: crate::timer::describe(&s),
        over: s.over,
        hard: s.hard,
    })
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
        assert!(!p["review"].workbench);
    }

    #[test]
    fn parses_the_workbench() {
        let p = parse("[pastures.review]\nmembers = [\"claude\"]\nworkbench = true\n").unwrap();
        assert!(p["review"].workbench);
        assert!(parse("[pastures.review]\nworkbench = \"yes\"\n").is_err());
        let e = format!(
            "{:#}",
            parse("[pastures.review]\nmembers = [\"workbench\"]\n").unwrap_err()
        );
        assert!(e.contains("workbench = true"), "{e}");
    }

    #[test]
    fn parses_and_checks_rations() {
        let p = parse(
            r#"[pastures.review]
members = ["claude", "codex"]
[pastures.review.rations]
claude = { quota = "weekly=1%", budget = "2h" }
codex  = { quota = "weekly=2%", hard = true }
"#,
        )
        .unwrap();
        let r = &p["review"].rations;
        assert_eq!(
            r["claude"],
            Ration {
                budget: 7200,
                quota: Some(("weekly".into(), 0.01)),
                hard: false
            }
        );
        assert_eq!(r["codex"].budget, 0);
        assert!(r["codex"].hard);
        let err = |rations: &str| {
            format!(
                "{:#}",
                parse(&format!(
                    "[pastures.review]\nmembers = [\"claude\"]\n[pastures.review.rations]\n{rations}\n"
                ))
                .unwrap_err()
            )
        };
        let e = err("codex = { budget = \"2h\" }");
        assert!(
            e.contains("[pastures.review.rations]") && e.contains("not in members"),
            "{e}"
        );
        let e = err("claude = { quota = \"weekly\" }");
        assert!(e.contains("[pastures.review.rations] claude"), "{e}");
        let e = err("claude = { budget = \"2x\" }");
        assert!(e.contains("[pastures.review.rations] claude"), "{e}");
        let e = err("claude = { budget = \"40d\" }");
        assert!(e.contains("30 days"), "{e}");
        let e = err("claude = { hard = true }");
        assert!(e.contains("needs a quota"), "{e}");
        let e = err("claude = { budget = \"2h\", other = 1 }");
        assert!(
            e.contains("[pastures.review]") && e.contains("other"),
            "{e}"
        );
    }
}
