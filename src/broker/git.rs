//! The `git-push` capability: the host pushes the sandbox's commits with the
//! user's own credentials, so no token or key ever enters the sandbox.
//!
//! The sandbox can write the project's `.git`, so the host never runs git
//! with that repository's configuration or hooks: an agent could put
//! `core.sshCommand`, `core.fsmonitor` or a pre-push hook there. Instead the
//! host pushes from a fresh scratch repository that borrows the project's
//! objects (`objects/info/alternates`) and has no configuration of its own.
//! The remote URL comes from a snapshot taken when the sandbox started, not
//! from the current `.git/config`.

use anyhow::{bail, Context as _, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// The most commits a card lists for one ref.
const MAX_COMMITS: usize = 20;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Remote {
    pub name: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub push_url: Option<String>,
    /// `remote.NAME.gh-resolved`, which `gh repo set-default` writes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gh_resolved: Option<String>,
}

impl Remote {
    /// Where a push goes.
    pub fn push_target(&self) -> &str {
        self.push_url.as_deref().unwrap_or(&self.url)
    }
}

/// The project repository as it was when the sandbox started.
#[derive(Debug, Clone, Default)]
pub struct Repo {
    /// The git directory that holds the objects (the common directory of a
    /// linked worktree).
    pub common_dir: PathBuf,
    /// Remotes in the repository configuration when the sandbox started.
    /// The sandbox cannot change that file (it is read-only inside), so a
    /// remote that it names otherwise, for example with `git -c`, is new.
    pub remotes: Vec<Remote>,
    /// `objects/info/alternates` at start. A change means the sandbox points
    /// the object store at other host repositories.
    pub alternates: Option<String>,
}

/// Read the remotes of a repository. The call names the git directory and
/// drops the host's own configuration, so only the repository's own remotes
/// are listed. Reading configuration runs no programs.
pub fn read_remotes(dir: &Path) -> Vec<Remote> {
    let Ok(Some(layout)) = crate::hostgit::Layout::discover(dir) else {
        return vec![];
    };
    let out = layout
        .command()
        .args([
            "config",
            "--null",
            "--get-regexp",
            r"^remote\..*\.(url|pushurl|gh-resolved)$",
        ])
        .output();
    let Ok(out) = out else { return vec![] };
    let mut by_name: BTreeMap<String, Remote> = BTreeMap::new();
    let mut order = Vec::new();
    for entry in String::from_utf8_lossy(&out.stdout).split('\0') {
        let Some((key, value)) = entry.split_once('\n') else {
            continue;
        };
        let Some(rest) = key.strip_prefix("remote.") else {
            continue;
        };
        let Some((name, field)) = rest.rsplit_once('.') else {
            continue;
        };
        let r = by_name.entry(name.to_string()).or_insert_with(|| {
            order.push(name.to_string());
            Remote {
                name: name.to_string(),
                url: String::new(),
                push_url: None,
                gh_resolved: None,
            }
        });
        match field {
            "url" if r.url.is_empty() => r.url = value.to_string(),
            "pushurl" if r.push_url.is_none() => r.push_url = Some(value.to_string()),
            "gh-resolved" => r.gh_resolved = Some(value.to_string()),
            _ => {}
        }
    }
    order
        .into_iter()
        .filter_map(|n| by_name.remove(&n))
        .filter(|r| !r.url.is_empty())
        .collect()
}

impl Repo {
    /// Take the snapshot when the sandbox starts. None if the project is not
    /// a git repository.
    pub fn snapshot(project: &Path) -> Option<Repo> {
        // The layout comes from the file system, not from `commondir` or
        // `core.worktree`, which a sandbox can write.
        let layout = crate::hostgit::Layout::discover(project).ok().flatten()?;
        let common_dir = std::fs::canonicalize(&layout.common_dir).ok()?;
        let remotes = read_remotes(project);
        let alternates = read_alternates(&common_dir).ok().flatten();
        Some(Repo {
            common_dir,
            remotes,
            alternates,
        })
    }

    pub fn find(&self, name: &str) -> Option<&Remote> {
        self.remotes.iter().find(|r| r.name == name)
    }
}

/// `objects/info/alternates` of a repository, read without following a
/// symlink, without blocking on a FIFO, and with a size limit.
fn read_alternates(common: &Path) -> Result<Option<String>> {
    let bytes = crate::safefs::Root::open(common)?.read(
        Path::new("objects/info/alternates"),
        crate::safefs::MAX_READ,
    )?;
    Ok(bytes.map(|b| String::from_utf8_lossy(&b).into_owned()))
}

/// What the object store of a repository looks like. The scratch repository
/// borrows these objects, so a change between the card and the push would
/// push objects of another repository. Each directory is checked with
/// `lstat`: a symlink here could point into any host repository.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ObjectStore {
    alternates: Option<String>,
    dirs: Vec<(u64, u64)>,
}

fn object_store(common: &Path) -> Result<ObjectStore> {
    let mut dirs = Vec::new();
    for rel in ["objects", "objects/info", "objects/pack"] {
        let p = common.join(rel);
        let m = match std::fs::symlink_metadata(&p) {
            Ok(m) => m,
            // `info` and `pack` are created on demand; absent is fine.
            Err(_) if rel != "objects" => {
                dirs.push((0, 0));
                continue;
            }
            Err(e) => return Err(e).context(format!("cannot read {}", p.display())),
        };
        if m.is_symlink() || !m.is_dir() {
            bail!(
                "{} is a symlink or not a directory; paddock does not push from this repository",
                p.display()
            );
        }
        use std::os::unix::fs::MetadataExt;
        dirs.push((m.dev(), m.ino()));
    }
    Ok(ObjectStore {
        alternates: read_alternates(common)?,
        dirs,
    })
}

/// Quote a value for a git configuration subsection name.
fn subsection(s: &str) -> String {
    s.replace('\\', r"\\").replace('"', "\\\"")
}

/// URL prefixes whose pushes always go through the host, also for remotes
/// that the sandbox adds itself. The host then flags them as new.
const PUSH_PREFIXES: &[&str] = &["https://", "http://", "ssh://", "git://", "git@"];

/// The part of the sandbox `.gitconfig` that sends `git push` to the host.
pub fn sandbox_gitconfig(repo: Option<&Repo>) -> String {
    let mut t = String::from(
        "# git push goes to the host through paddock-host. See `paddock-host help git-push`.\n\
         [credential]\n\thelper =\n",
    );
    let mut rules: Vec<(String, &str)> = PUSH_PREFIXES
        .iter()
        .map(|p| (p.to_string(), "pushInsteadOf"))
        .collect();
    for r in repo.map(|r| r.remotes.as_slice()).unwrap_or_default() {
        rules.push((r.url.clone(), "pushInsteadOf"));
        // pushInsteadOf skips remotes with an explicit pushurl.
        if let Some(p) = &r.push_url {
            if p != &r.url {
                rules.push((p.clone(), "insteadOf"));
            }
        }
    }
    for (prefix, key) in rules {
        if prefix.contains(['\n', '\0']) {
            continue;
        }
        t.push_str(&format!(
            "[url \"paddock::{}\"]\n\t{key} = \"{}\"\n",
            subsection(&prefix),
            subsection(&prefix)
        ));
    }
    t
}

/// One ref of a push, as the sandbox sends it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushRef {
    /// What the sandbox named, for example HEAD or refs/heads/fix. Display only.
    #[serde(default)]
    pub src: String,
    /// The object to push. Empty deletes the remote ref.
    #[serde(default)]
    pub sha: String,
    pub dst: String,
    #[serde(default)]
    pub force: bool,
}

#[derive(Debug, Clone)]
pub struct PlannedRef {
    pub r: PushRef,
    pub commits: Vec<String>,
    pub more_commits: bool,
    pub new_ref: bool,
    pub default_branch: bool,
}

/// Everything the card shows and the push needs, worked out before asking.
#[derive(Debug)]
pub struct PushPlan {
    pub remote: String,
    pub url: String,
    /// The remote existed on the host before the sandbox started.
    pub known: bool,
    pub notes: Vec<String>,
    pub refs: Vec<PlannedRef>,
    scratch: Scratch,
    /// The repository whose objects the scratch repository borrows.
    common_dir: PathBuf,
    /// Its object store when the card was made.
    store: ObjectStore,
}

impl PushPlan {
    /// Force pushes, deletions, pushes to the default branch and pushes to a
    /// new remote need an answer every time.
    pub fn session_allowed(&self) -> bool {
        self.known
            && self
                .refs
                .iter()
                .all(|r| !r.r.force && !r.r.sha.is_empty() && !r.default_branch)
    }

    /// The keys that "allow for this session" stores: one per remote and branch.
    pub fn grant_keys(&self) -> Vec<String> {
        self.refs
            .iter()
            .map(|r| format!("git-push\0{}\0{}", self.url, r.r.dst))
            .collect()
    }

    pub fn summary(&self) -> String {
        let r = &self.refs[0];
        let more = if self.refs.len() > 1 {
            format!(" and {} more", self.refs.len() - 1)
        } else {
            String::new()
        };
        format!(
            "{}git push {} {}{more}{}",
            if self.known { "" } else { "NEW REMOTE: " },
            self.remote,
            short_ref(&r.r.dst),
            if r.r.force { " (force)" } else { "" }
        )
    }

    pub fn detail(&self) -> String {
        let mut t = String::new();
        if !self.known {
            t.push_str(&format!(
                "WARNING: the remote {} was added or changed inside a sandbox. \
                 Allow this only if you know the URL.\n",
                self.remote
            ));
        }
        t.push_str(&format!(
            "Push with your credentials to:\n  {} ({})\n",
            self.url, self.remote
        ));
        for n in &self.notes {
            t.push_str(&format!("Note: {n}\n"));
        }
        for r in &self.refs {
            let mut flags = Vec::new();
            if r.r.sha.is_empty() {
                flags.push("DELETE");
            }
            if r.r.force {
                flags.push("FORCE");
            }
            if r.default_branch {
                flags.push("default branch");
            }
            if r.new_ref && !r.r.sha.is_empty() {
                flags.push("new");
            }
            let src = if r.r.sha.is_empty() {
                "(delete)".to_string()
            } else {
                format!("{} {}", clean(&r.r.src), &r.r.sha[..12.min(r.r.sha.len())])
            };
            t.push_str(&format!(
                "\n{src} -> {}{}\n",
                r.r.dst,
                if flags.is_empty() {
                    String::new()
                } else {
                    format!("  [{}]", flags.join(", "))
                }
            ));
            if !r.r.sha.is_empty() {
                if r.commits.is_empty() {
                    t.push_str("  no new commits\n");
                }
                for c in &r.commits {
                    t.push_str(&format!("  {c}\n"));
                }
                if r.more_commits {
                    t.push_str(&format!("  ... more than {MAX_COMMITS} commits\n"));
                }
            }
        }
        t.trim_end().to_string()
    }
}

fn short_ref(r: &str) -> &str {
    r.strip_prefix("refs/heads/")
        .or_else(|| r.strip_prefix("refs/tags/"))
        .unwrap_or(r)
}

/// Remove control characters: commit subjects come from the sandbox.
fn clean(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).take(200).collect()
}

/// A scratch repository, removed when the plan is dropped.
#[derive(Debug)]
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn is_sha(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64) && s.chars().all(|c| c.is_ascii_hexdigit())
}

pub fn valid_ref(r: &str) -> bool {
    (r.starts_with("refs/heads/") || r.starts_with("refs/tags/"))
        && r.len() <= 255
        && !r.ends_with('/')
        && !r.ends_with(".lock")
        && !r.contains("..")
        && !r.contains("@{")
        && !r.contains("//")
        && !r.split('/').any(|p| p.starts_with('.'))
        && !r.chars().any(|c| c.is_control() || " ~^:?*[\\".contains(c))
}

/// Check a URL for a remote that the host does not know.
fn valid_new_url(u: &str) -> Result<()> {
    if u.is_empty()
        || u.starts_with('-')
        || u.contains("::")
        || u.starts_with("ext:")
        || u.chars().any(|c| c.is_control())
    {
        bail!("{u:?} is not a URL that paddock pushes to");
    }
    Ok(())
}

pub fn parse_refs(v: Option<&serde_json::Value>) -> Result<Vec<PushRef>> {
    let refs: Vec<PushRef> = serde_json::from_value(v.cloned().context("git-push needs refs")?)
        .context("git-push refs are invalid")?;
    if refs.is_empty() || refs.len() > 50 {
        bail!("git-push needs 1 to 50 refs");
    }
    for r in &refs {
        if !valid_ref(&r.dst) {
            bail!(
                "{:?} is not a branch or tag ref (refs/heads/... or refs/tags/...)",
                r.dst
            );
        }
        if !r.sha.is_empty() && !is_sha(&r.sha) {
            bail!("{:?} is not an object id", r.sha);
        }
    }
    Ok(refs)
}

/// Check a request without touching the network.
pub fn validate(args: &BTreeMap<String, serde_json::Value>) -> Result<()> {
    let remote = args
        .get("remote")
        .and_then(|v| v.as_str())
        .context("git-push needs a remote")?;
    if remote.is_empty() || remote.len() > 1000 || remote.chars().any(|c| c.is_control()) {
        bail!("invalid remote name");
    }
    parse_refs(args.get("refs"))?;
    Ok(())
}

fn git_env(cmd: &mut Command) -> &mut Command {
    cmd.env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env_remove("GIT_CONFIG_COUNT")
}

fn scratch_git(scratch: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("-C")
        .arg(scratch)
        .args(["-c", "core.hooksPath=/dev/null"])
        .args(["-c", "push.recurseSubmodules=no"]);
    git_env(&mut c);
    c
}

fn capture(mut cmd: Command, secs: u64) -> Result<String> {
    let (code, out, err) = super::run_capture(&mut cmd, Duration::from_secs(secs))?;
    if code != 0 {
        bail!("{}", err.trim());
    }
    Ok(out)
}

/// Work out what the push does, for the card. Reads the remote with
/// `git ls-remote` only for remotes that the host knows.
pub fn plan(
    repo: Option<&Repo>,
    run_dir: &Path,
    args: &BTreeMap<String, serde_json::Value>,
) -> Result<PushPlan> {
    let repo = repo.context("the project is not a git repository on the host")?;
    let name = args
        .get("remote")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let sandbox_url = args.get("url").and_then(|v| v.as_str()).unwrap_or(name);
    let refs = parse_refs(args.get("refs"))?;
    let mut notes = Vec::new();
    let (url, known) = match repo.find(name) {
        Some(r) => {
            let target = r.push_target().to_string();
            if sandbox_url != r.url && sandbox_url != target && sandbox_url != name {
                notes.push(format!(
                    "the sandbox changed this remote's URL to {}; paddock pushes to the original URL",
                    clean(sandbox_url)
                ));
            }
            (target, true)
        }
        None => {
            valid_new_url(sandbox_url)?;
            (sandbox_url.to_string(), false)
        }
    };
    let store = object_store(&repo.common_dir)?;
    if store.alternates != repo.alternates {
        bail!(
            "objects/info/alternates of the repository changed while the sandbox ran; paddock does not push from it"
        );
    }

    crate::privfs::dir(run_dir)?;
    let scratch = Scratch(run_dir.join(format!("push-{}", super::new_request_id())));
    let out = git_env(&mut Command::new("git"))
        .args(["init", "-q", "--bare"])
        .arg(&scratch.0)
        .output()?;
    if !out.status.success() {
        bail!(
            "cannot create a scratch repository: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    std::fs::write(
        scratch.0.join("objects/info/alternates"),
        format!("{}\n", repo.common_dir.join("objects").display()),
    )?;

    // The remote's refs: old values for the commit lists, and its HEAD.
    let mut remote_refs: BTreeMap<String, String> = BTreeMap::new();
    let mut remote_head = None;
    if known {
        let mut c = scratch_git(&scratch.0);
        c.args(["ls-remote", "--symref", "--", &url]);
        match capture(c, 30) {
            Ok(text) => {
                for line in text.lines() {
                    if let Some(rest) = line.strip_prefix("ref: ") {
                        if let Some((target, "HEAD")) = rest.split_once('\t') {
                            remote_head = Some(target.to_string());
                        }
                    } else if let Some((sha, r)) = line.split_once('\t') {
                        if r.starts_with("refs/heads/") || r.starts_with("refs/tags/") {
                            remote_refs.insert(r.to_string(), sha.to_string());
                        }
                    }
                }
            }
            Err(e) => notes.push(format!(
                "cannot read the remote ({}); the commit list may be longer than the push",
                clean(e.to_string().lines().last().unwrap_or(""))
            )),
        }
    }
    let present = |sha: &str| {
        let mut c = scratch_git(&scratch.0);
        c.args(["cat-file", "-e", sha]);
        capture(c, 10).is_ok()
    };
    let known_tips: Vec<String> = remote_refs
        .values()
        .filter(|s| present(s))
        .cloned()
        .collect();

    let mut planned = Vec::new();
    for r in refs {
        let old = remote_refs.get(&r.dst).cloned();
        let mut commits = Vec::new();
        let mut more = false;
        if !r.sha.is_empty() {
            if !present(&r.sha) {
                bail!("object {} is not in the repository", r.sha);
            }
            let mut c = scratch_git(&scratch.0);
            c.args(["log", "--oneline", "--no-decorate", "--no-color"])
                .arg(format!("-n{}", MAX_COMMITS + 1))
                .arg(&r.sha);
            let not: Vec<&String> = match &old {
                Some(o) if present(o) => vec![o],
                _ => known_tips.iter().collect(),
            };
            if !not.is_empty() {
                c.arg("--not").args(not);
            }
            c.arg("--");
            commits = capture(c, 20)?.lines().map(clean).collect();
            if commits.len() > MAX_COMMITS {
                commits.truncate(MAX_COMMITS);
                more = true;
            }
        }
        let default_branch = matches!(r.dst.as_str(), "refs/heads/main" | "refs/heads/master")
            || remote_head.as_deref() == Some(r.dst.as_str());
        planned.push(PlannedRef {
            new_ref: old.is_none(),
            r,
            commits,
            more_commits: more,
            default_branch,
        });
    }
    Ok(PushPlan {
        remote: clean(name),
        url,
        known,
        notes,
        refs: planned,
        scratch,
        common_dir: repo.common_dir.clone(),
        store,
    })
}

/// Push after approval. Never runs hooks and never reads the project config.
pub fn push(plan: &PushPlan) -> Result<serde_json::Value> {
    // The sandbox keeps running while the card waits. Check the object store
    // again now: the scratch repository reads objects through it.
    if object_store(&plan.common_dir)? != plan.store {
        bail!(
            "the object store of {} changed while the request waited; paddock does not push from it",
            plan.common_dir.display()
        );
    }
    let mut c = scratch_git(&plan.scratch.0);
    c.args(["push", "--no-verify", "--porcelain", "--", &plan.url]);
    for r in &plan.refs {
        c.arg(format!(
            "{}{}:{}",
            if r.r.force { "+" } else { "" },
            r.r.sha,
            r.r.dst
        ));
    }
    let (code, out, err) = super::run_capture(&mut c, Duration::from_secs(600))?;
    // Porcelain lines: FLAG \t FROM:TO \t SUMMARY. A `!` flag is a rejection.
    let mut results = Vec::new();
    for r in &plan.refs {
        let line = out.lines().find(|l| {
            l.split('\t')
                .nth(1)
                .is_some_and(|ft| ft.ends_with(&format!(":{}", r.r.dst)))
        });
        let (ok, message) = match line {
            Some(l) => {
                let summary = l.split('\t').nth(2).unwrap_or("").to_string();
                (!l.starts_with('!'), summary)
            }
            None => (false, "the host push failed".to_string()),
        };
        results.push(serde_json::json!({ "dst": r.r.dst, "ok": ok, "message": message }));
    }
    Ok(serde_json::json!({ "exit_code": code, "refs": results, "stderr": err }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@localhost"])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A project with one commit and a bare repository as `origin`.
    fn project(tmp: &Path) -> (PathBuf, PathBuf) {
        let remote = tmp.join("remote.git");
        let work = tmp.join("work");
        std::fs::create_dir_all(&work).unwrap();
        sh(
            tmp,
            &[
                "init",
                "-q",
                "--bare",
                "-b",
                "main",
                remote.to_str().unwrap(),
            ],
        );
        sh(&work, &["init", "-q", "-b", "main"]);
        sh(&work, &["commit", "-q", "--allow-empty", "-m", "first"]);
        sh(
            &work,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        sh(&work, &["push", "-q", "origin", "main"]);
        (work, remote)
    }

    fn args(
        remote: &str,
        url: &str,
        refs: serde_json::Value,
    ) -> BTreeMap<String, serde_json::Value> {
        let mut a = BTreeMap::new();
        a.insert("remote".into(), serde_json::json!(remote));
        a.insert("url".into(), serde_json::json!(url));
        a.insert("refs".into(), refs);
        a
    }

    #[test]
    fn the_snapshot_reads_the_remotes() {
        let tmp = tempfile::tempdir().unwrap();
        let (work, remote) = project(tmp.path());
        sh(&work, &["config", "remote.origin.pushurl", "/srv/push.git"]);
        let repo = Repo::snapshot(&work).unwrap();
        assert_eq!(repo.remotes.len(), 1);
        assert_eq!(repo.remotes[0].url, remote.to_str().unwrap());
        assert_eq!(repo.find("origin").unwrap().push_target(), "/srv/push.git");
        assert!(repo.find("sneaky").is_none());
    }

    #[test]
    fn the_sandbox_gitconfig_rewrites_pushes_and_drops_credential_helpers() {
        let repo = Repo {
            remotes: vec![Remote {
                name: "origin".into(),
                url: "/srv/a.git".into(),
                push_url: Some("/srv/b.git".into()),
                gh_resolved: None,
            }],
            ..Default::default()
        };
        let t = sandbox_gitconfig(Some(&repo));
        assert!(t.contains("[credential]\n\thelper =\n"));
        assert!(t.contains("[url \"paddock::https://\"]\n\tpushInsteadOf = \"https://\""));
        assert!(t.contains("[url \"paddock::/srv/a.git\"]\n\tpushInsteadOf = \"/srv/a.git\""));
        assert!(t.contains("[url \"paddock::/srv/b.git\"]\n\tinsteadOf = \"/srv/b.git\""));
    }

    #[test]
    fn refs_must_be_branches_or_tags() {
        for bad in [
            "refs/heads/../x",
            "HEAD",
            "refs/notes/x",
            "refs/heads/a b",
            "refs/heads/x.lock",
        ] {
            assert!(!valid_ref(bad), "{bad}");
        }
        assert!(valid_ref("refs/heads/feature/one"));
        assert!(valid_ref("refs/tags/v1.0"));
    }

    #[test]
    fn a_push_lists_commits_and_reaches_the_remote() {
        let tmp = tempfile::tempdir().unwrap();
        let (work, remote) = project(tmp.path());
        let repo = Repo::snapshot(&work).unwrap();
        sh(&work, &["commit", "-q", "--allow-empty", "-m", "second"]);
        sh(&work, &["commit", "-q", "--allow-empty", "-m", "third"]);
        let sha = sh(&work, &["rev-parse", "HEAD"]);
        // A hook in the project must never run on the host.
        let hook = work.join(".git/hooks/pre-push");
        std::fs::write(&hook, "#!/bin/sh\ntouch \"$GIT_DIR/../hook-ran\"\nexit 1\n").unwrap();
        std::fs::set_permissions(&hook, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let a = args(
            "origin",
            remote.to_str().unwrap(),
            serde_json::json!([{ "src": "HEAD", "sha": sha, "dst": "refs/heads/feature" }]),
        );
        let p = plan(Some(&repo), &tmp.path().join("run"), &a).unwrap();
        assert!(p.known && p.session_allowed());
        assert_eq!(p.refs[0].commits.len(), 2, "{:?}", p.refs[0].commits);
        assert!(p.detail().contains("third"));
        let r = push(&p).unwrap();
        assert_eq!(r["refs"][0]["ok"], true, "{r}");
        assert_eq!(sh(&remote, &["rev-parse", "refs/heads/feature"]), sha);
        assert!(!work.join("hook-ran").exists());
        let scratch = p.scratch.0.clone();
        drop(p);
        assert!(!scratch.exists());
    }

    #[test]
    fn the_object_store_is_checked_again_before_the_push() {
        let tmp = tempfile::tempdir().unwrap();
        let (work, remote) = project(tmp.path());
        let repo = Repo::snapshot(&work).unwrap();
        let sha = sh(&work, &["rev-parse", "HEAD"]);
        let a = args(
            "origin",
            remote.to_str().unwrap(),
            serde_json::json!([{ "sha": sha, "dst": "refs/heads/feature" }]),
        );
        let p = plan(Some(&repo), &tmp.path().join("run"), &a).unwrap();
        // The sandbox keeps running while the card waits: a new alternates
        // line would borrow the objects of another repository.
        let info = work.join(".git/objects/info");
        std::fs::create_dir_all(&info).unwrap();
        std::fs::write(info.join("alternates"), "/srv/other/objects\n").unwrap();
        let e = push(&p).unwrap_err().to_string();
        assert!(e.contains("object store"), "{e}");
        std::fs::remove_file(info.join("alternates")).unwrap();
        // A symlink in place of a pack directory is refused as well.
        let pack = work.join(".git/objects/pack");
        std::fs::remove_dir_all(&pack).unwrap();
        std::os::unix::fs::symlink("/srv/other/objects/pack", &pack).unwrap();
        let e = push(&p).unwrap_err().to_string();
        assert!(e.contains("symlink"), "{e}");
    }

    #[test]
    fn force_default_branch_and_new_remotes_cannot_be_session_granted() {
        let tmp = tempfile::tempdir().unwrap();
        let (work, remote) = project(tmp.path());
        let repo = Repo::snapshot(&work).unwrap();
        let sha = sh(&work, &["rev-parse", "HEAD"]);
        let run = tmp.path().join("run");
        let one = |dst: &str, force: bool| serde_json::json!([{ "sha": sha, "dst": dst, "force": force }]);
        let url = remote.to_str().unwrap();
        let p = plan(
            Some(&repo),
            &run,
            &args("origin", url, one("refs/heads/x", true)),
        )
        .unwrap();
        assert!(!p.session_allowed());
        let p = plan(
            Some(&repo),
            &run,
            &args("origin", url, one("refs/heads/main", false)),
        )
        .unwrap();
        assert!(p.refs[0].default_branch && !p.session_allowed());
        let p = plan(
            Some(&repo),
            &run,
            &args("sneaky", url, one("refs/heads/x", false)),
        )
        .unwrap();
        assert!(!p.known && !p.session_allowed());
        assert!(p.detail().starts_with("WARNING"));
        // A changed URL of a known remote still pushes to the original one.
        let p = plan(
            Some(&repo),
            &run,
            &args(
                "origin",
                "https://example.com/evil.git",
                one("refs/heads/x", false),
            ),
        )
        .unwrap();
        assert_eq!(p.url, url);
        assert!(p.detail().contains("original URL"));
        assert!(plan(
            Some(&repo),
            &run,
            &args("x", "ext::sh -c evil", one("refs/heads/x", false))
        )
        .is_err());
    }
}
