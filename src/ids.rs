//! Resolve sandbox IDs from the records kept on the host.
use crate::env::HostEnv;
use anyhow::{bail, Result};
use std::collections::BTreeSet;

pub fn short(id: &str) -> &str {
    id.rsplit('-').next().unwrap_or(id)
}

pub fn resolve_from<'a>(input: &str, ids: impl IntoIterator<Item = &'a str>) -> Result<String> {
    let ids: BTreeSet<&str> = ids.into_iter().collect();
    if ids.contains(input) {
        return Ok(input.to_owned());
    }
    let matches: Vec<&str> = if input.len() >= 4 && input.bytes().all(|c| c.is_ascii_hexdigit()) {
        ids.into_iter()
            .filter(|id| short(id).starts_with(input))
            .collect()
    } else {
        Vec::new()
    };
    match matches.as_slice() {
        [id] => Ok((*id).to_owned()),
        [] => bail!("no sandbox with id {input}; `paddock ps` and `paddock files` list sandboxes"),
        _ => bail!("ambiguous sandbox id {input}: {}", matches.join(", ")),
    }
}

pub fn resolve(env: &HostEnv, input: &str) -> Result<String> {
    let mut ids: Vec<String> = crate::state::Running::list(env)
        .into_iter()
        .map(|r| r.id)
        .collect();
    ids.extend(crate::recover::Session::all(env).into_iter().map(|s| s.id));
    if let Ok(entries) = std::fs::read_dir(env.paddock_state().join("detach")) {
        ids.extend(entries.flatten().filter_map(|e| {
            e.file_name()
                .to_str()
                .and_then(|s| s.strip_suffix(".json"))
                .map(str::to_owned)
        }));
    }
    if let Ok(projects) = std::fs::read_dir(crate::snapshot::root(env)) {
        for project in projects.flatten() {
            ids.extend(
                crate::snapshot::list(&project.path())
                    .into_iter()
                    .map(|(_, m)| m.id),
            );
        }
    }
    resolve_from(input, ids.iter().map(String::as_str))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn resolves_ids() {
        let ids = ["fake-app-1a2b3c", "fake-other-abcdef"];
        for input in ["fake-app-1a2b3c", "1a2b3c", "1a2b"] {
            assert_eq!(resolve_from(input, ids).unwrap(), ids[0]);
        }
        assert!(resolve_from("1a2", ids).is_err());
        assert!(resolve_from("ffff", ids).is_err());
        let err = resolve_from("1a2b", [ids[0], "fake-other-1a2b3c"])
            .unwrap_err()
            .to_string();
        assert!(err.contains(ids[0]) && err.contains("fake-other-1a2b3c"));
    }
    #[test]
    fn resolves_ended_record() {
        let tmp = tempfile::tempdir().unwrap();
        let env = HostEnv::from_parts(
            tmp.path().to_path_buf(),
            "test".into(),
            1,
            1,
            Default::default(),
        );
        let dir = env.paddock_state().join("detach");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("fake-project-123abc.json"), "{}").unwrap();
        assert_eq!(resolve(&env, "123a").unwrap(), "fake-project-123abc");
    }
}
