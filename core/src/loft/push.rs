//! Notices before private files enter a remote repository: once at the
//! start of a run, and in the push approval. They never block a push.

use super::{entries, Cfg, Entry, Kind};
use crate::env::HostEnv;
use crate::speckit;
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

/// The most names that a notice line shows.
pub const MAX_NAMES: usize = 5;

/// `a, b, c` and at most MAX_NAMES names, then a count.
pub fn names(all: &BTreeSet<String>) -> String {
    let mut s: Vec<&str> = all.iter().take(MAX_NAMES).map(String::as_str).collect();
    let more = all.len().saturating_sub(MAX_NAMES);
    let more_text = format!("{more} more");
    if more > 0 {
        s.push(&more_text);
    }
    s.join(", ")
}

fn split(out: &str) -> impl Iterator<Item = String> + '_ {
    out.split('\0')
        .map(|n| n.trim_matches('\n'))
        .filter(|n| !n.is_empty())
        .map(str::to_string)
}

/// The private entries that the notices cover. Spec Kit has its own rule
/// (speckit::git).
fn watched(cfg: &Cfg) -> Vec<Entry> {
    entries(cfg)
        .into_iter()
        .filter(|e| !e.public && e.kind != Kind::SpecKit)
        .collect()
}

/// The kind of a repository path, from the first entry that covers it.
fn kind_of(list: &[Entry], name: &str) -> Option<Kind> {
    list.iter().find(|e| e.matches(name)).map(|e| e.kind)
}

/// Files of the private paths that git tracks (`untracked`: or could add),
/// below `project`. `ls-files` reads the index and folder names without
/// running filters.
fn files(project: &Path, list: &[Entry], untracked: bool) -> Option<Vec<String>> {
    let git = crate::hostgit::Layout::discover(project).ok()??;
    let rel = project.strip_prefix(&git.work_tree).ok()?;
    let base = rel.display().to_string();
    let mut cmd = git.command();
    cmd.current_dir(&git.work_tree);
    cmd.arg(format!("--work-tree={}", git.work_tree.display()))
        .args(["ls-files", "-z", "--full-name"]);
    if untracked {
        cmd.args(["--others", "--exclude-standard"]);
    } else {
        cmd.arg("--cached");
    }
    cmd.arg("--");
    cmd.args(list.iter().map(|e| e.pathspec(&base)));
    let (code, out, _) = crate::broker::run_capture(&mut cmd, Duration::from_secs(60)).ok()?;
    let pre = if base.is_empty() {
        String::new()
    } else {
        format!("{}/", base.trim_matches('/'))
    };
    (code == 0).then(|| {
        split(&out)
            .map(|n| n.strip_prefix(&pre).unwrap_or(&n).to_string())
            .collect()
    })
}

/// The lines of the start notice; empty when there is nothing to say.
pub fn notice_lines(env: &HostEnv, cfg: &Cfg, project: &Path) -> Vec<String> {
    let list = watched(cfg);
    let mut by_kind: BTreeMap<Kind, BTreeSet<String>> = BTreeMap::new();
    for untracked in [false, true] {
        for n in files(project, &list, untracked).unwrap_or_default() {
            if let Some(k) = kind_of(&list, &n) {
                by_kind.entry(k).or_default().insert(n);
            }
        }
    }
    let mut out: Vec<String> = Vec::new();
    if !by_kind.is_empty() {
        let what: Vec<String> = by_kind
            .iter()
            .map(|(k, n)| format!("{} ({})", k.label(), names(n)))
            .collect();
        out.push(format!(
            "paddock: this repository has {}. A push publishes them once they are committed.",
            what.join(" and ")
        ));
        out.push("`paddock loft init` and `paddock loft adopt` move them into the loft, paddock's private store. To keep a path in git, add it to [loft] public in ~/.config/paddock/config.toml.".into());
    }
    if speckit::git::needs_notice(env, &speckit::Cfg::load(env), project) {
        out.push(speckit::git::START_NOTICE.into());
    }
    out
}

/// The start notice, on stderr.
pub fn notice(env: &HostEnv, project: &Path) {
    for l in notice_lines(env, &Cfg::load(env), project) {
        eprintln!("{l}");
    }
}

/// The start notice of a sandbox that the user started.
pub fn start_notice(env: &HostEnv, built: &crate::build::Built) {
    if built.parent.is_none() && built.spawn.is_none() {
        notice(env, &built.home_key);
    }
}

/// What the commits of a push carry: files of private paths, and new
/// files under `docs/`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Found {
    pub names: BTreeSet<String>,
    pub docs: BTreeSet<String>,
}

/// The pathspecs that a push scan reads: every private path that is not
/// public, Spec Kit included.
pub fn pathspecs(cfg: &Cfg) -> Vec<String> {
    entries(cfg)
        .into_iter()
        .filter(|e| !e.public)
        .map(|e| e.pathspec(""))
        .collect()
}

fn log_names(mut cmd: Command, extra: &[&str], specs: &[String]) -> Result<Vec<String>> {
    cmd.args([
        "--format=",
        "--name-only",
        "-z",
        "--root",
        "-m",
        "--no-renames",
    ])
    .args(extra)
    .arg("--")
    .args(specs);
    let (code, out, err) = crate::broker::run_capture(&mut cmd, Duration::from_secs(60))?;
    anyhow::ensure!(
        code == 0,
        "cannot check the private files of the push: {}",
        err.trim()
    );
    Ok(split(&out).collect())
}

impl Found {
    /// Read the commits of one ref. `log` makes `git log` with its
    /// revision range; every commit counts, merges and root commits too.
    pub fn scan(&mut self, log: &dyn Fn() -> Command, specs: &[String]) -> Result<()> {
        if !specs.is_empty() {
            self.names.extend(log_names(log(), &[], specs)?);
        }
        let docs = [":(glob)docs/**".to_string()];
        self.docs
            .extend(log_names(log(), &["--diff-filter=A"], &docs)?);
        Ok(())
    }
}

/// The lines for the push approval: one per kind, and one for new files
/// under `docs/`.
pub fn notes(env: &HostEnv, found: &Found) -> Vec<String> {
    let cfg = Cfg::load(env);
    let all = entries(&cfg);
    let mut by_kind: BTreeMap<Kind, BTreeSet<String>> = BTreeMap::new();
    for n in &found.names {
        match all.iter().find(|e| e.matches(n)) {
            Some(e) if !e.public => {
                by_kind.entry(e.kind).or_default().insert(n.clone());
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    for (k, n) in &by_kind {
        if *k == Kind::SpecKit {
            if speckit::git::default_store(&speckit::Cfg::load(env)) {
                out.push(speckit::git::PUSH_NOTICE.to_string());
            }
            continue;
        }
        out.push(format!(
            "This push contains {} ({}): they become part of the remote.",
            k.label(),
            names(n)
        ));
    }
    let docs_public = cfg.public.iter().any(|p| {
        super::normalize(p)
            .as_deref()
            .map(|p| p.trim_end_matches('/'))
            == Some("docs")
    });
    if !found.docs.is_empty() && !docs_public {
        out.push(format!(
            "This push adds new files under docs/ ({}). Agents often keep their design notes and plans there: check that you want them public.",
            names(&found.docs)
        ));
    }
    out
}
