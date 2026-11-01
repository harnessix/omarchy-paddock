//! `paddock worktree`: the worktrees that paddock made for `--worktree`
//! runs and apps, and what to do with them after the run: merge the branch,
//! remove the worktree and the branch, or keep both.
//!
//! The branch and the worktree come from a sandbox. Every git call goes
//! through hostgit.rs: no hook, filter or other program that the repository
//! names runs on the host.

use crate::env::HostEnv;
use crate::hostgit::Checkout;
use crate::storage::Worktree;
use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::Command;

/// The worktrees that paddock made.
pub fn list(env: &HostEnv) -> Vec<Worktree> {
    crate::storage::worktrees(env)
}

/// The worktree that `target` names: its path, its folder name or its
/// branch (with or without `paddock/`).
pub fn find(all: &[Worktree], target: &str) -> Result<Worktree> {
    let path = std::fs::canonicalize(target).ok();
    let hits: Vec<&Worktree> = all
        .iter()
        .filter(|w| {
            path.is_some() && std::fs::canonicalize(&w.path).ok() == path
                || w.path.file_name().is_some_and(|n| n == target)
                || w.branch
                    .as_deref()
                    .is_some_and(|b| b == target || b.strip_prefix("paddock/") == Some(target))
        })
        .collect();
    match hits.as_slice() {
        [w] => Ok((*w).clone()),
        [] => bail!("no paddock worktree {target} (paddock worktree list shows them)"),
        _ => bail!("{target} names more than one worktree. Give the path."),
    }
}

/// Merge the branch of `w` into the current branch of its repository. It
/// refuses when the repository has uncommitted changes, and it aborts a
/// merge with conflicts, so the repository stays as it was.
pub fn merge(env: &HostEnv, w: &Worktree) -> Result<String> {
    let (repo, branch) = repo_and_branch(w)?;
    let co = Checkout::open(repo)?;
    let changes = co.run(&["status", "--porcelain=v1", "--untracked-files=no"])?;
    if !changes.trim().is_empty() {
        bail!(
            "{} has uncommitted changes. Commit or stash them, then merge again.",
            repo.display()
        );
    }
    let current = co
        .layout
        .query(&["symbolic-ref", "--short", "-q", "HEAD"])
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .with_context(|| format!("{} has no current branch", repo.display()))?;
    if current == branch {
        bail!("{branch} is the current branch of {}", repo.display());
    }
    let who = identity(env, &co);
    let who = who.as_ref().map(|(n, e)| (n.as_str(), e.as_str()));
    let msg = format!("Merge branch '{branch}'");
    // `--no-verify` and the hardened git (core.hooksPath=/dev/null): no hook
    // runs, neither from the repository nor from the worktree.
    if let Err(e) = co.run_as(
        who,
        &["merge", "--no-verify", "--no-edit", "-m", &msg, branch],
    ) {
        let _ = co.run(&["merge", "--abort"]);
        bail!("git merge of {branch} failed, the repository is as before. Merge it by hand: git -C {} merge {branch}\n{e:#}", repo.display());
    }
    let mut t = format!("Merged {branch} into {current} in {}.", repo.display());
    if w.dirty == Some(true) {
        t.push_str(" The worktree has uncommitted changes, which the merge leaves out.");
    }
    Ok(t)
}

/// Remove the worktree and delete its branch. Without `force`, it refuses
/// while the branch has commits that no other branch has, and git refuses
/// a worktree with uncommitted changes.
pub fn remove(env: &HostEnv, w: &Worktree, force: bool) -> Result<String> {
    if w.running {
        bail!("a sandbox works in {}. Stop it first.", w.path.display());
    }
    if !force {
        match w.unmerged {
            Some(0) => {}
            Some(n) => bail!(
                "{} has {n} commits that no other branch has. Merge it first (paddock worktree merge), or add --force.",
                w.branch.as_deref().unwrap_or("the branch")
            ),
            None if w.branch.is_some() => bail!(
                "paddock cannot count the unmerged commits of {}. Add --force to remove it anyway.",
                w.branch.as_deref().unwrap_or_default()
            ),
            None => {}
        }
        if w.dirty == Some(true) {
            bail!(
                "{} has uncommitted changes. Add --force to remove them too.",
                w.path.display()
            );
        }
    }
    crate::storage::remove_worktree(
        env,
        &w.path,
        w.repo.as_deref(),
        w.branch.as_deref(),
        true,
        force,
    )?;
    Ok(match &w.branch {
        Some(b) => format!("Removed {} and the branch {b}.", w.path.display()),
        None => format!("Removed {}.", w.path.display()),
    })
}

fn repo_and_branch(w: &Worktree) -> Result<(&Path, &str)> {
    let repo = w
        .repo
        .as_deref()
        .with_context(|| format!("paddock cannot find the repository of {}", w.path.display()))?;
    let branch = w
        .branch
        .as_deref()
        .with_context(|| format!("{} has no branch", w.path.display()))?;
    Ok((repo, branch))
}

/// The author of a merge commit. The hardened git reads no global
/// configuration, so the user's name comes from the repository, else from
/// the user's git configuration file, else it is paddock.
fn identity(env: &HostEnv, co: &Checkout) -> Option<(String, String)> {
    if co
        .layout
        .try_query(&["config", "--get", "user.name"])
        .is_some()
    {
        return None;
    }
    let xdg = env
        .var("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| env.home.join(".config"));
    for f in [env.home.join(".gitconfig"), xdg.join("git/config")] {
        if !f.is_file() {
            continue;
        }
        let get = |key: &str| {
            Command::new("git")
                .args(["config", "--file"])
                .arg(&f)
                .args(["--get", key])
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .filter(|s| !s.is_empty())
        };
        if let (Some(n), Some(e)) = (get("user.name"), get("user.email")) {
            return Some((n, e));
        }
    }
    Some(("paddock".into(), "paddock@localhost".into()))
}

/// `paddock worktree list` as text.
pub fn render(all: &[Worktree]) -> String {
    if all.is_empty() {
        return "No paddock worktrees.\n".into();
    }
    let mut t = String::new();
    for w in all {
        let commits = match w.unmerged {
            Some(n) => format!("{n} unmerged commits"),
            None => "unmerged commits unknown".into(),
        };
        let dirty = match w.dirty {
            Some(true) => "uncommitted changes",
            Some(false) => "clean",
            None => "state unknown",
        };
        t.push_str(&format!(
            "{}\n  branch {}, {commits}, {dirty}{}\n",
            w.path.display(),
            w.branch.as_deref().unwrap_or("unknown"),
            if w.running { ", a sandbox uses it" } else { "" }
        ));
        if let Some(r) = &w.repo {
            t.push_str(&format!("  repository {}\n", r.display()));
        }
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn setup() -> (tempfile::TempDir, HostEnv, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let mut vars = BTreeMap::new();
        if let Ok(p) = std::env::var("PATH") {
            vars.insert("PATH".to_string(), p);
        }
        let env = HostEnv::from_parts(home, "tester".into(), 1000, 1000, vars);
        let repo = env.home.join("src/repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("a.txt"), "a\n").unwrap();
        git(&repo, &["add", "a.txt"]);
        git(&repo, &["commit", "-q", "-m", "a"]);
        crate::state::record_recent(&env, &repo, "claude").unwrap();
        (tmp, env, repo)
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let o = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(o.status.success(), "git {args:?}");
        String::from_utf8_lossy(&o.stdout).into_owned()
    }

    #[test]
    fn list_and_merge_with_a_refusal_on_a_dirty_tree() {
        let (_t, env, repo) = setup();
        let wt = crate::build::create_worktree(&env, &repo, Some("code")).unwrap();
        std::fs::write(wt.path.join("b.txt"), "b\n").unwrap();
        git(&wt.path, &["add", "b.txt"]);
        git(&wt.path, &["commit", "-q", "-m", "b"]);
        // A hook in the repository never runs.
        let hook = repo.join(".git/hooks/pre-merge-commit");
        std::fs::write(&hook, "#!/bin/sh\ntouch \"$GIT_DIR/../hook-ran\"\nexit 1\n").unwrap();
        std::fs::set_permissions(&hook, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();

        let all = list(&env);
        assert_eq!(all.len(), 1, "{all:?}");
        let w = &all[0];
        assert_eq!(w.branch.as_deref(), Some(wt.branch.as_str()));
        assert_eq!(w.unmerged, Some(1));
        assert!(!w.running);
        assert!(render(&all).contains("1 unmerged commits"));
        assert_eq!(find(&all, &wt.branch).unwrap().path, wt.path);
        let short = wt.branch.strip_prefix("paddock/").unwrap();
        assert_eq!(find(&all, short).unwrap().path, wt.path);
        assert_eq!(
            find(&all, &wt.path.display().to_string()).unwrap().path,
            wt.path
        );
        assert!(find(&all, "nothing").is_err());

        // Uncommitted changes in the repository: no merge.
        std::fs::write(repo.join("a.txt"), "changed\n").unwrap();
        let e = merge(&env, w).unwrap_err().to_string();
        assert!(e.contains("uncommitted changes"), "{e}");
        assert!(!repo.join("b.txt").exists());
        git(&repo, &["checkout", "-q", "a.txt"]);

        // A diverged branch gets a merge commit, so the hook had its chance.
        std::fs::write(repo.join("c.txt"), "c\n").unwrap();
        git(&repo, &["add", "c.txt"]);
        git(&repo, &["commit", "-q", "-m", "c"]);
        let t = merge(&env, w).unwrap();
        assert!(t.contains("into main"), "{t}");
        assert!(repo.join("b.txt").exists());
        assert!(!repo.join("hook-ran").exists());
        assert_eq!(list(&env)[0].unmerged, Some(0));
    }

    #[test]
    fn remove_refuses_unmerged_commits_without_force() {
        let (_t, env, repo) = setup();
        let wt = crate::build::create_worktree(&env, &repo, None).unwrap();
        git(&wt.path, &["commit", "-q", "--allow-empty", "-m", "b"]);
        let w = find(&list(&env), &wt.branch).unwrap();
        let e = remove(&env, &w, false).unwrap_err().to_string();
        assert!(e.contains("1 commits"), "{e}");
        assert!(wt.path.exists());

        let t = remove(&env, &w, true).unwrap();
        assert!(t.contains(&wt.branch), "{t}");
        assert!(!wt.path.exists());
        let branches = git(&repo, &["branch", "--format=%(refname:short)"]);
        assert!(!branches.contains(&wt.branch), "{branches}");
        assert!(list(&env).is_empty());
    }

    #[test]
    fn remove_takes_a_merged_worktree_without_force() {
        let (_t, env, repo) = setup();
        let wt = crate::build::create_worktree(&env, &repo, None).unwrap();
        let mut w = find(&list(&env), &wt.branch).unwrap();
        // A host without a sandboxed `git status` does not know the state;
        // git itself refuses a worktree with changes.
        assert_eq!(w.unmerged, Some(0));
        w.running = true;
        assert!(remove(&env, &w, false).is_err());
        w.running = false;
        remove(&env, &w, false).unwrap();
        assert!(!wt.path.exists());
    }
}
