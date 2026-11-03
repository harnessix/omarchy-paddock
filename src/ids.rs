//! Sandbox IDs: the short form and the lookup of a typed ID.
//!
//! A sandbox ID is `HARNESS-PROJECT-HEX`. The short form is the hex part.
//! A command takes the full ID, the short form or a unique prefix of the hex
//! part of at least `MIN_PREFIX` characters.
use crate::env::HostEnv;
use anyhow::{bail, Result};
use std::collections::BTreeSet;
use std::path::Path;

/// The shortest prefix of the hex part that a command takes.
pub const MIN_PREFIX: usize = 4;

/// The hex part of a sandbox ID: `1a2b3c` of `claude-app-1a2b3c`.
pub fn short(id: &str) -> &str {
    id.rsplit('-').next().unwrap_or(id)
}

/// The one ID of `ids` that `input` names: the ID itself, or a unique prefix
/// of its hex part.
pub fn resolve_from<'a>(input: &str, ids: impl IntoIterator<Item = &'a str>) -> Result<String> {
    match lookup(input, ids)? {
        Some(id) => Ok(id.to_owned()),
        None => bail!("no sandbox with id {input}. paddock ps and paddock files list sandboxes"),
    }
}

/// The item of `items` whose ID `input` names, or `None` when no ID matches.
/// An ambiguous or too short `input` is an error.
pub fn find<'a, T>(input: &str, items: &'a [T], id: impl Fn(&T) -> &str) -> Result<Option<&'a T>> {
    let Some(hit) = lookup(input, items.iter().map(&id))? else {
        return Ok(None);
    };
    Ok(items.iter().find(|t| id(t) == hit))
}

fn lookup<'a>(input: &str, ids: impl IntoIterator<Item = &'a str>) -> Result<Option<&'a str>> {
    let ids: BTreeSet<&str> = ids.into_iter().collect();
    if let Some(id) = ids.get(input) {
        return Ok(Some(id));
    }
    if input.is_empty() || !input.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Ok(None);
    }
    if input.len() < MIN_PREFIX {
        bail!("sandbox id {input} is too short: type at least {MIN_PREFIX} characters of the id");
    }
    let matches: Vec<&str> = ids
        .into_iter()
        .filter(|id| short(id).starts_with(input))
        .collect();
    match matches.as_slice() {
        [] => Ok(None),
        [id] => Ok(Some(id)),
        _ => bail!(
            "sandbox id {input} is ambiguous, it matches {}",
            matches.join(", ")
        ),
    }
}

/// The full sandbox ID that `input` names, among the running sandboxes, the
/// recent ended runs, the sandbox homes and the snapshots of `paddock undo`.
/// A full ID that paddock does not know stays as it is, so the command can
/// say what is missing.
pub fn resolve(env: &HostEnv, input: &str) -> Result<String> {
    let ids = known(env);
    match resolve_from(input, ids.iter().map(String::as_str)) {
        Err(_) if input.contains('-') => Ok(input.to_owned()),
        r => r,
    }
}

/// Every sandbox ID that the host has a record of.
pub fn known(env: &HostEnv) -> Vec<String> {
    let mut ids: Vec<String> = crate::state::Running::list(env)
        .into_iter()
        .map(|r| r.id)
        .collect();
    ids.extend(crate::recover::Session::all(env).into_iter().map(|s| s.id));
    ids.extend(names(&env.paddock_state().join("detach"), ".json"));
    ids.extend(names(&crate::view::dir(env), ".log"));
    if let Ok(projects) = std::fs::read_dir(crate::snapshot::root(env)) {
        for project in projects.flatten() {
            ids.extend(
                crate::snapshot::list(&project.path())
                    .into_iter()
                    .map(|(_, m)| m.id),
            );
        }
    }
    ids
}

/// The file names in `dir` that end with `suffix`, without it.
fn names(dir: &Path, suffix: &str) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| {
            e.file_name()
                .to_str()
                .and_then(|s| s.strip_suffix(suffix))
                .map(str::to_owned)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDS: [&str; 2] = ["claude-app-1a2b3c", "codex-other-abcdef"];

    fn env(dir: &Path) -> HostEnv {
        HostEnv::from_parts(dir.to_path_buf(), "test".into(), 1, 1, Default::default())
    }

    #[test]
    fn short_is_the_hex_part() {
        assert_eq!(short("claude-app-1a2b3c"), "1a2b3c");
        assert_eq!(short("1a2b3c"), "1a2b3c");
    }

    #[test]
    fn full_id() {
        assert_eq!(resolve_from(IDS[0], IDS).unwrap(), IDS[0]);
    }

    #[test]
    fn short_id() {
        assert_eq!(resolve_from("1a2b3c", IDS).unwrap(), IDS[0]);
        assert_eq!(resolve_from("abcdef", IDS).unwrap(), IDS[1]);
    }

    #[test]
    fn prefix() {
        assert_eq!(resolve_from("1a2b", IDS).unwrap(), IDS[0]);
        assert_eq!(resolve_from("abcde", IDS).unwrap(), IDS[1]);
    }

    #[test]
    fn too_short() {
        let err = resolve_from("1a2", IDS).unwrap_err().to_string();
        assert!(err.contains("too short"), "{err}");
    }

    #[test]
    fn ambiguous_across_projects() {
        let ids = ["claude-app-1a2b3c", "claude-web-1a2b99"];
        let err = resolve_from("1a2b", ids).unwrap_err().to_string();
        assert!(err.contains("ambiguous"), "{err}");
        assert!(err.contains(ids[0]) && err.contains(ids[1]), "{err}");
        assert_eq!(resolve_from("1a2b3", ids).unwrap(), ids[0]);
    }

    #[test]
    fn unknown() {
        let err = resolve_from("ffff", IDS).unwrap_err().to_string();
        assert!(err.contains("no sandbox with id ffff"), "{err}");
        assert!(resolve_from("1a2b-x", IDS).is_err());
        let tmp = tempfile::tempdir().unwrap();
        assert!(resolve(&env(tmp.path()), "ffff").is_err());
        // A full id stays, so the command can say what is missing.
        assert_eq!(
            resolve(&env(tmp.path()), "claude-gone-123456").unwrap(),
            "claude-gone-123456"
        );
    }

    #[test]
    fn ended_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let env = env(tmp.path());
        let dir = env.paddock_state().join("detach");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("claude-app-123abc.json"), "{}").unwrap();
        std::fs::create_dir_all(crate::view::dir(&env)).unwrap();
        std::fs::write(crate::view::log_file(&env, "codex-app-456def"), "").unwrap();
        assert_eq!(resolve(&env, "123a").unwrap(), "claude-app-123abc");
        assert_eq!(resolve(&env, "456d").unwrap(), "codex-app-456def");
    }

    #[test]
    fn snapshot_records() {
        let tmp = tempfile::tempdir().unwrap();
        let env = env(tmp.path());
        let id = "claude-app-789fed";
        let dir =
            crate::snapshot::project_dir(&crate::snapshot::root(&env), Path::new("/p")).join(id);
        std::fs::create_dir_all(&dir).unwrap();
        let meta = crate::snapshot::Meta {
            id: id.into(),
            harness: "claude".into(),
            project: "/p".into(),
            started: 1,
            ended: None,
            reflink: false,
            bytes: 0,
            skip: Default::default(),
            files: Default::default(),
            end: None,
        };
        std::fs::write(dir.join("meta.json"), serde_json::to_vec(&meta).unwrap()).unwrap();
        assert_eq!(resolve(&env, "789f").unwrap(), id);
    }
}
