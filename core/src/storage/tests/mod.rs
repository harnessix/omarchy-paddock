use super::*;
use crate::env::HostEnv;
use crate::state::{self, AuthMode, Running};
use anyhow::Result;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::process::Command;

mod containers;
mod gc;
mod homes;

pub(in crate::storage::tests) fn setup() -> (tempfile::TempDir, HostEnv) {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    // PATH so that work-tree checks can find bwrap and git.
    let mut vars = BTreeMap::new();
    if let Ok(p) = std::env::var("PATH") {
        vars.insert("PATH".to_string(), p);
    }
    let env = HostEnv::from_parts(home, "tester".into(), 1000, 1000, vars);
    (tmp, env)
}

/// A home as paddock creates it, with an origin file.
pub(in crate::storage::tests) fn make_home(
    env: &HostEnv,
    harness: &str,
    project: &Path,
    app: bool,
) -> PathBuf {
    let h = state::sandbox_home(env, harness, &AuthMode::Shared, project).unwrap();
    write_origin(&h, &Origin::new(project, harness, &AuthMode::Shared, app)).unwrap();
    std::fs::write(h.join(".bash_history"), "ls\n").unwrap();
    h
}

#[derive(Default)]
pub(in crate::storage::tests) struct FakeContainers {
    /// (cli, joined args) to output. A missing entry fails.
    pub(in crate::storage::tests) replies: BTreeMap<(String, String), String>,
    pub(in crate::storage::tests) calls: RefCell<Vec<String>>,
}

impl FakeContainers {
    pub(in crate::storage::tests) fn reply(mut self, cli: &str, args: &str, out: &str) -> Self {
        self.replies.insert((cli.into(), args.into()), out.into());
        self
    }
}

impl Containers for FakeContainers {
    fn run(&self, cli: &str, args: &[&str]) -> Result<String> {
        let a = args.join(" ");
        self.calls.borrow_mut().push(format!("{cli} {a}"));
        self.replies
            .get(&(cli.to_string(), a.clone()))
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("{cli} {a}: no reply"))
    }
    fn clis(&self) -> Vec<String> {
        let mut v: Vec<String> = self.replies.keys().map(|k| k.0.clone()).collect();
        v.dedup();
        v
    }
}

pub(in crate::storage::tests) fn host<'a>(
    c: &'a dyn Containers,
    profiles: &[(&str, bool)],
) -> Host<'a> {
    Host {
        containers: c,
        current_tags: ["paddock-base:current".to_string()].into(),
        profiles: Some(profiles.iter().map(|(n, a)| (n.to_string(), *a)).collect()),
        scratch: BTreeMap::new(),
    }
}

pub(in crate::storage::tests) fn opts() -> GcOptions {
    GcOptions {
        older_than_days: DEFAULT_OLDER_THAN,
        ..Default::default()
    }
}

pub(in crate::storage::tests) fn find<'a>(items: &'a [Item], what: &str) -> Option<&'a Item> {
    items.iter().find(|i| i.what.contains(what))
}

pub(in crate::storage::tests) fn home(path: &str, harness: &str) -> Home {
    Home {
        path: PathBuf::from(path),
        harness: harness.into(),
        auth: "shared".into(),
        project: Some("/w/p".into()),
        origin: OriginCheck::Ok,
        project_exists: Some(true),
        last_used: Some(1_000 * DAY),
        modified: 1_000 * DAY,
        bytes: 10,
        app: false,
        profile: true,
        running: false,
        loses: String::new(),
        caches: vec![],
    }
}

pub(in crate::storage::tests) fn inventory(homes: Vec<Home>) -> Inventory {
    Inventory {
        size_kind: SizeKind::Allocated,
        homes,
        images: vec![],
        worktrees: vec![],
        sessions: vec![],
        keyrings: vec![],
        notes: vec![],
        old_caches: vec![],
        any_running: false,
        build_cache: vec![],
        scratch: vec![],
        snapshots: vec![],
        tmpfs_caches: true,
        classes: vec![],
        layers: vec![],
    }
}

pub(in crate::storage::tests) fn inspect(id: &str, tags: &[&str]) -> serde_json::Value {
    serde_json::json!({ "Id": format!("sha256:{id}"), "RepoTags": tags, "Size": 1000, "Created": "2026-10-01T00:00:00Z" })
}
