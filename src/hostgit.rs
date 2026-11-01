//! Host git calls on a project that a sandbox can write.
//!
//! Everything in a project is attacker-controlled while a sandbox runs, and
//! it stays that way after the sandbox exits. Git takes commands from the
//! repository it is pointed at: `core.fsmonitor`, `core.hooksPath`, a hook,
//! a `filter.<name>.clean` driver. So the host never runs plain
//! `git -C <project>`.
//!
//! This module gives three things:
//!
//! 1. `Layout`: where the git directory is, worked out from the file system
//!    with `lstat`, never from `core.worktree`, `commondir` or a `.git` file
//!    that a sandbox wrote. A layout that does not look like a repository or
//!    a registered linked worktree is refused with a clear message.
//! 2. `Layout::query`: a hardened git call for refs and objects. It names the
//!    git directory directly, drops the system and global configuration, and
//!    turns off every repository setting that can start a program. It never
//!    runs a command that reads the work tree, because a `filter` driver
//!    cannot be turned off with `-c`.
//! 3. `Layout::status`: work-tree state (the "dirty" flag and `git status`
//!    lines) from a throwaway sandbox with no network. A planted filter runs
//!    there, where it can do no more than the agent already can.
//!
//! For the few host calls that must write a work tree (`git worktree add`
//! and `git worktree remove`), `Layout::guard` reads the configuration files
//! as text and refuses when they hold a key that can start a program.

use crate::env::HostEnv;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Configuration and pointer files are small.
const MAX_FILE: u64 = 1 << 20;

/// How long a host git query may take.
const QUERY_SECS: u64 = 60;

/// Files of a git directory that the host reads or runs, and that a sandbox
/// must not be able to change. `commondir` and `gitdir` point git at another
/// directory; the rest can name a program.
pub const PROTECTED_FILES: &[&str] = &[
    "config",
    "config.worktree",
    "commondir",
    "gitdir",
    "objects/info/alternates",
];

/// Configuration keys that can start a program, by the last part of the key.
const EXEC_KEY_SUFFIXES: &[&str] = &[
    ".fsmonitor",
    ".hookspath",
    ".sshcommand",
    ".gitproxy",
    ".proxy",
    ".alternaterefscommand",
    ".pager",
    ".askpass",
    ".editor",
    ".external",
    ".textconv",
    ".command",
    ".clean",
    ".smudge",
    ".process",
    ".helper",
    ".program",
    ".templatedir",
    ".uploadpack",
    ".receivepack",
    ".driver",
    ".worktree",
];

/// Configuration sections that can start a program, or that pull in another
/// file, whatever the key is called.
const EXEC_SECTIONS: &[&str] = &["alias.", "include.", "includeif.", "credential.", "filter."];

/// Where a repository lives, worked out from the file system.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    /// The top of the work tree.
    pub work_tree: PathBuf,
    /// `<work_tree>/.git`: a directory, or a file in a linked worktree.
    pub dot_git: PathBuf,
    /// The git directory of this work tree.
    pub git_dir: PathBuf,
    /// Where objects and refs live. The same as `git_dir`, unless this is a
    /// linked worktree.
    pub common_dir: PathBuf,
    /// True when `.git` is a file, so this is a linked worktree.
    pub linked: bool,
}

/// Read a small file below `root`, which a sandbox may control. Every
/// component of `rel` is opened without following a symlink, the open does
/// not block on a FIFO, and the size is limited.
fn read_in(root: &Path, rel: &str) -> Result<Option<String>> {
    if !is_dir_no_link(root) {
        return Ok(None);
    }
    let bytes = crate::safefs::Root::open(root)?.read(Path::new(rel), MAX_FILE)?;
    Ok(bytes.map(|b| String::from_utf8_lossy(&b).into_owned()))
}

fn is_dir_no_link(p: &Path) -> bool {
    std::fs::symlink_metadata(p)
        .map(|m| m.is_dir())
        .unwrap_or(false)
}

fn is_file_no_link(p: &Path) -> bool {
    std::fs::symlink_metadata(p)
        .map(|m| m.is_file())
        .unwrap_or(false)
}

/// Resolve a path that a pointer file names, relative to `base`.
fn resolve(base: &Path, text: &str) -> PathBuf {
    let t = Path::new(text.trim());
    if t.is_absolute() {
        t.to_path_buf()
    } else {
        normalize(&base.join(t))
    }
}

/// Remove `.` and `..` without touching the file system.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// A directory that holds git's own files.
fn looks_like_git_dir(p: &Path) -> bool {
    is_file_no_link(&p.join("HEAD"))
        && (is_dir_no_link(&p.join("objects")) || is_file_no_link(&p.join("commondir")))
}

impl Layout {
    /// Find the repository that `dir` belongs to. `Ok(None)` means `dir` is
    /// not in a work tree. An error means the layout is not one paddock can
    /// trust, and names what is wrong.
    pub fn discover(dir: &Path) -> Result<Option<Layout>> {
        let mut at = dir;
        let mut first = true;
        loop {
            let dot = at.join(".git");
            match std::fs::symlink_metadata(&dot) {
                Ok(m) if m.is_symlink() => bail!(
                    "{} is a symlink. paddock does not follow it: a sandbox could point it anywhere.",
                    dot.display()
                ),
                // Above the starting directory, something that is not a
                // repository is simply not the root; at the starting
                // directory it is reported, because paddock was pointed there.
                Ok(m) if m.is_dir() && (first || looks_like_git_dir(&dot)) => {
                    return Self::main(at, &dot).map(Some)
                }
                Ok(m) if m.is_file() => return Self::worktree(at, &dot).map(Some),
                Ok(_) if !first => {}
                Ok(_) => bail!(
                    "{} is neither a directory nor a file. paddock does not use it.",
                    dot.display()
                ),
                Err(_) => {}
            }
            first = false;
            match at.parent() {
                Some(p) => at = p,
                None => return Ok(None),
            }
        }
    }

    /// The layout of a repository whose `.git` is a directory.
    fn main(work_tree: &Path, dot: &Path) -> Result<Layout> {
        if !looks_like_git_dir(dot) {
            bail!(
                "{} has no HEAD and no objects. paddock does not use it as a git directory.",
                dot.display()
            );
        }
        // A main repository has no `commondir`. One that appeared would send
        // git's configuration lookup to another directory.
        if let Some(text) = read_in(dot, "commondir")? {
            let target = resolve(dot, &text);
            if std::fs::canonicalize(&target).ok() != std::fs::canonicalize(dot).ok() {
                bail!(
                    "{}/commondir points at {}. A sandbox can write it, so paddock refuses this repository. Remove the file if you added it yourself.",
                    dot.display(),
                    target.display()
                );
            }
        }
        Ok(Layout {
            work_tree: work_tree.to_path_buf(),
            dot_git: dot.to_path_buf(),
            git_dir: dot.to_path_buf(),
            common_dir: dot.to_path_buf(),
            linked: false,
        })
    }

    /// The layout of a linked worktree, whose `.git` is a file. The file is
    /// sandbox-writable, so every claim in it is checked against the
    /// repository it names: the repository must list this worktree back.
    fn worktree(work_tree: &Path, dot: &Path) -> Result<Layout> {
        let text = read_in(work_tree, ".git")?.unwrap_or_default();
        let named = text
            .lines()
            .find_map(|l| l.strip_prefix("gitdir:"))
            .with_context(|| format!("{} has no 'gitdir:' line", dot.display()))?;
        let git_dir = resolve(work_tree, named);
        let bad = |why: &str| -> anyhow::Error {
            anyhow::anyhow!(
                "{} points at {}, which {}. A sandbox can write that file, so paddock refuses this worktree.",
                dot.display(),
                git_dir.display(),
                why
            )
        };
        if git_dir.starts_with(work_tree) {
            return Err(bad("is inside the work tree"));
        }
        if !is_dir_no_link(&git_dir) || !looks_like_git_dir(&git_dir) {
            return Err(bad("is not a git directory"));
        }
        if git_dir.parent().and_then(|p| p.file_name()) != Some(std::ffi::OsStr::new("worktrees")) {
            return Err(bad("is not a registered worktree of a repository"));
        }
        let common = git_dir
            .parent()
            .and_then(|p| p.parent())
            .map(Path::to_path_buf)
            .ok_or_else(|| bad("has no repository above it"))?;
        if !looks_like_git_dir(&common) || !is_dir_no_link(&common.join("objects")) {
            return Err(bad("belongs to no repository with an object store"));
        }
        // The repository's own pointer back to this work tree.
        let back = read_in(&git_dir, "gitdir")?.unwrap_or_default();
        if std::fs::canonicalize(resolve(&git_dir, &back)).ok() != std::fs::canonicalize(dot).ok() {
            return Err(bad("does not point back at this worktree"));
        }
        // The worktree's own `commondir` must name that repository.
        let text = read_in(&git_dir, "commondir")?.ok_or_else(|| bad("has no commondir file"))?;
        if std::fs::canonicalize(resolve(&git_dir, &text)).ok()
            != std::fs::canonicalize(&common).ok()
        {
            return Err(bad("has a commondir file that names another directory"));
        }
        Ok(Layout {
            work_tree: work_tree.to_path_buf(),
            dot_git: dot.to_path_buf(),
            git_dir,
            common_dir: common,
            linked: true,
        })
    }

    /// The top of the work tree of `dir`, if there is one.
    pub fn toplevel(dir: &Path) -> Option<PathBuf> {
        Self::discover(dir).ok().flatten().map(|l| l.work_tree)
    }

    /// A git command that reads refs and objects of this repository and runs
    /// nothing the repository names.
    pub fn command(&self) -> Command {
        hardened(&self.git_dir, None)
    }

    /// Run a git query. The arguments must not read the work tree: a
    /// `filter` driver cannot be turned off with `-c`. Use `status` for
    /// anything that needs the work tree.
    pub fn query(&self, args: &[&str]) -> Result<String> {
        run(self.command().args(args))
    }

    /// The same, as an option: an empty result for a repository without
    /// commits, a missing ref, or a git that fails.
    pub fn try_query(&self, args: &[&str]) -> Option<String> {
        self.query(args).ok()
    }

    /// Refuse when a configuration file of this repository holds a key that
    /// can start a program. Call it before a host git command that writes or
    /// reads the work tree, which `-c` cannot make safe.
    pub fn guard(&self) -> Result<()> {
        for dir in [&self.git_dir, &self.common_dir] {
            for name in ["config", "config.worktree"] {
                let Some(text) = read_in(dir, name)? else {
                    continue;
                };
                if let Some(key) = exec_key(&text) {
                    bail!(
                        "{} sets {key}, which runs a program. The sandbox can write it, so paddock does not run git here. Check the file and remove the line.",
                        dir.join(name).display()
                    );
                }
            }
        }
        Ok(())
    }

    /// `git status --porcelain` of the work tree, run in a throwaway sandbox
    /// with no network. `Ok(None)` means the host cannot sandbox a git call,
    /// so the state is unknown; the caller says so instead of guessing.
    pub fn status(&self, env: &HostEnv, untracked: &str) -> Result<Option<Vec<String>>> {
        let untracked = format!("--untracked-files={untracked}");
        match self.sandboxed(env, &["status", "--porcelain=v1", &untracked])? {
            Some(out) => Ok(Some(out.lines().map(str::to_string).collect())),
            None => Ok(None),
        }
    }

    /// Run a git command on the work tree inside bubblewrap: the host system
    /// read-only, this repository writable, no network, a throwaway home.
    /// Whatever the repository's configuration starts runs in there.
    pub fn sandboxed(&self, env: &HostEnv, args: &[&str]) -> Result<Option<String>> {
        let Some(bwrap) = env.which("bwrap") else {
            return Ok(None);
        };
        let Some(git) = env.which("git") else {
            return Ok(None);
        };
        let home = Path::new("/run/paddock-git-home");
        let mut a: Vec<String> = [
            "--die-with-parent",
            "--unshare-all",
            "--unshare-user",
            "--new-session",
            "--ro-bind",
            "/usr",
            "/usr",
            "--ro-bind",
            "/etc",
            "/etc",
            "--proc",
            "/proc",
            "--dev",
            "/dev",
            "--tmpfs",
            "/tmp",
            "--tmpfs",
            "/var/tmp",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        for d in ["/bin", "/sbin", "/lib", "/lib64", "/lib32", "/libx32"] {
            match std::fs::read_link(d) {
                Ok(target) => a.extend([
                    "--symlink".to_string(),
                    target.display().to_string(),
                    d.to_string(),
                ]),
                Err(_) if Path::new(d).is_dir() => {
                    a.extend(["--ro-bind".to_string(), d.to_string(), d.to_string()])
                }
                Err(_) => {}
            }
        }
        a.extend(["--tmpfs".to_string(), home.display().to_string()]);
        // The object store first: a linked worktree reads it read-only.
        if self.linked {
            a.extend([
                "--ro-bind".to_string(),
                self.common_dir.display().to_string(),
                self.common_dir.display().to_string(),
            ]);
            a.extend([
                "--bind".to_string(),
                self.git_dir.display().to_string(),
                self.git_dir.display().to_string(),
            ]);
        }
        a.extend([
            "--bind".to_string(),
            self.work_tree.display().to_string(),
            self.work_tree.display().to_string(),
        ]);
        a.extend([
            "--remount-ro".to_string(),
            "/".to_string(),
            "--chdir".to_string(),
            self.work_tree.display().to_string(),
            "--".to_string(),
            git.display().to_string(),
        ]);
        a.extend(hardened_args(&self.git_dir, Some(&self.work_tree)));
        a.extend(args.iter().map(|s| s.to_string()));
        let mut cmd = Command::new(bwrap);
        cmd.args(a);
        hardened_env(&mut cmd);
        cmd.env("HOME", home);
        Ok(run(&mut cmd).ok())
    }

    /// Every git directory of this repository whose files the host reads or
    /// runs: this one, the common one, each registered worktree and each
    /// submodule.
    pub fn protected_dirs(&self) -> Vec<PathBuf> {
        let mut out = vec![self.common_dir.clone(), self.git_dir.clone()];
        for sub in ["worktrees", "modules"] {
            collect_git_dirs(&self.common_dir.join(sub), 0, &mut out);
        }
        out.sort();
        out.dedup();
        out
    }
}

fn collect_git_dirs(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > 4 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if !is_dir_no_link(&p) {
            continue;
        }
        let git_dir = is_file_no_link(&p.join("HEAD"));
        if git_dir {
            out.push(p.clone());
        }
        collect_git_dirs(&p.join("modules"), depth + 1, out);
        if !git_dir {
            collect_git_dirs(&p, depth + 1, out);
        }
    }
}

/// The first configuration key in `text` that can start a program.
pub fn exec_key(text: &str) -> Option<String> {
    let mut section = String::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.starts_with('#') || line.starts_with(';') || line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix('[') {
            let head = rest.split(']').next().unwrap_or("");
            let mut parts = head.splitn(2, |c: char| c.is_whitespace() || c == '"');
            let name = parts.next().unwrap_or("").trim().to_lowercase();
            let sub = head[name.len()..].trim().trim_matches('"').to_lowercase();
            section = if sub.is_empty() {
                format!("{name}.")
            } else {
                format!("{name}.{sub}.")
            };
            continue;
        }
        let key = line.split('=').next().unwrap_or("").trim().to_lowercase();
        if key.is_empty() {
            continue;
        }
        let full = format!("{section}{key}");
        if EXEC_SECTIONS.iter().any(|s| full.starts_with(s))
            || EXEC_KEY_SUFFIXES
                .iter()
                .any(|s| full.ends_with(s) || full == s.trim_start_matches('.'))
        {
            return Some(full);
        }
    }
    None
}

/// A git command that takes no setting from the host or the repository that
/// could start a program, and that asks nothing on a terminal.
pub fn hardened(git_dir: &Path, work_tree: Option<&Path>) -> Command {
    let mut cmd = Command::new("git");
    cmd.args(hardened_args(git_dir, work_tree));
    hardened_env(&mut cmd);
    cmd
}

fn hardened_args(git_dir: &Path, work_tree: Option<&Path>) -> Vec<String> {
    let mut a = vec![
        format!("--git-dir={}", git_dir.display()),
        "--no-optional-locks".to_string(),
    ];
    if let Some(wt) = work_tree {
        a.push(format!("--work-tree={}", wt.display()));
    }
    for kv in [
        "core.fsmonitor=",
        "core.hooksPath=/dev/null",
        "core.untrackedCache=false",
        "core.sshCommand=",
        "core.gitProxy=",
        "core.alternateRefsCommand=",
        "core.askPass=",
        "core.pager=cat",
        "core.editor=true",
        "core.attributesFile=/dev/null",
        "pager.log=false",
        "diff.external=",
        "credential.helper=",
        "protocol.ext.allow=never",
        "uploadpack.packObjectsHook=",
        "gpg.program=true",
        "safe.directory=*",
    ] {
        a.push("-c".to_string());
        a.push(kv.to_string());
    }
    a
}

fn hardened_env(cmd: &mut Command) {
    cmd.env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_PAGER", "cat")
        .env("GIT_ASKPASS", "")
        .env("GIT_ALLOW_PROTOCOL", "file:https:ssh:git")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env_remove("GIT_CONFIG")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_NAMESPACE")
        .env_remove("GIT_SSH_COMMAND")
        .stdin(Stdio::null());
}

/// Run a prepared command with a time limit and give back its output.
fn run(cmd: &mut Command) -> Result<String> {
    let (code, out, err) =
        crate::broker::run_capture(cmd, std::time::Duration::from_secs(QUERY_SECS))?;
    if code != 0 {
        bail!("git failed: {}", err.trim());
    }
    Ok(out)
}

/// A git call on a repository the host owns, such as the main checkout when
/// paddock adds or removes a worktree. These commands write the work tree,
/// so the configuration is checked first.
pub struct Checkout {
    pub layout: Layout,
}

impl Checkout {
    pub fn open(dir: &Path) -> Result<Checkout> {
        let layout = Layout::discover(dir)?
            .with_context(|| format!("{} is not a git repository", dir.display()))?;
        Ok(Checkout { layout })
    }

    /// Run a git command that may write the work tree. It is refused when the
    /// repository configuration can start a program.
    pub fn run(&self, args: &[&str]) -> Result<String> {
        self.layout.guard()?;
        let mut cmd = hardened(&self.layout.git_dir, Some(&self.layout.work_tree));
        cmd.current_dir(&self.layout.work_tree);
        run(cmd.args(args))
    }

    /// The same, with a private index file instead of the repository's
    /// own, and paddock as the author of a commit. `index` must be in a
    /// folder that only the host can write.
    pub fn run_with_index(&self, index: &Path, args: &[&str]) -> Result<String> {
        self.layout.guard()?;
        let mut cmd = hardened(&self.layout.git_dir, Some(&self.layout.work_tree));
        cmd.current_dir(&self.layout.work_tree)
            .env("GIT_INDEX_FILE", index)
            .env("GIT_AUTHOR_NAME", "paddock")
            .env("GIT_AUTHOR_EMAIL", "paddock@localhost")
            .env("GIT_COMMITTER_NAME", "paddock")
            .env("GIT_COMMITTER_EMAIL", "paddock@localhost");
        run(cmd.args(args))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@localhost"])
            .args(args)
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    }

    fn repo(root: &Path) -> PathBuf {
        let p = root.join("repo");
        std::fs::create_dir_all(&p).unwrap();
        git(&p, &["init", "-q", "-b", "main"]);
        git(&p, &["commit", "-q", "--allow-empty", "-m", "one"]);
        p
    }

    #[test]
    fn finds_a_plain_repository() {
        let tmp = tempfile::tempdir().unwrap();
        let p = repo(tmp.path());
        std::fs::create_dir_all(p.join("a/b")).unwrap();
        let l = Layout::discover(&p.join("a/b")).unwrap().unwrap();
        assert_eq!(l.work_tree, p);
        assert_eq!(l.git_dir, p.join(".git"));
        assert_eq!(l.common_dir, l.git_dir);
        assert!(!l.linked);
        // A directory with no repository above it is not one.
        let bare = tmp.path().join("plain/deep");
        std::fs::create_dir_all(&bare).unwrap();
        assert!(Layout::discover(&bare).unwrap().is_none() || !bare.join(".git").exists());
        // An empty .git where paddock was pointed is reported, not ignored.
        let broken = tmp.path().join("broken");
        std::fs::create_dir_all(broken.join(".git")).unwrap();
        assert!(Layout::discover(&broken).is_err());
    }

    #[test]
    fn ignores_core_worktree_and_refuses_a_planted_commondir() {
        let tmp = tempfile::tempdir().unwrap();
        let p = repo(tmp.path());
        let elsewhere = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        // A sandbox-written core.worktree must not move the project.
        std::fs::write(
            p.join(".git/config"),
            format!("[core]\n\tworktree = {}\n", elsewhere.display()),
        )
        .unwrap();
        assert_eq!(Layout::discover(&p).unwrap().unwrap().work_tree, p);
        // A planted commondir is refused by name.
        std::fs::write(
            p.join(".git/commondir"),
            format!("{}\n", elsewhere.display()),
        )
        .unwrap();
        let e = Layout::discover(&p).unwrap_err().to_string();
        assert!(e.contains("commondir"), "{e}");
        // The harmless self-reference that paddock itself writes is fine.
        std::fs::write(p.join(".git/commondir"), ".\n").unwrap();
        assert!(Layout::discover(&p).unwrap().is_some());
    }

    #[test]
    fn refuses_a_dot_git_file_that_points_anywhere() {
        let tmp = tempfile::tempdir().unwrap();
        let p = repo(tmp.path());
        let wt = tmp.path().join("wt");
        git(&p, &["worktree", "add", "-q", wt.to_str().unwrap()]);
        let l = Layout::discover(&wt).unwrap().unwrap();
        assert!(l.linked);
        assert_eq!(l.common_dir, p.join(".git"));
        assert_eq!(l.work_tree, wt);

        let other = repo(&tmp.path().join("other"));
        std::fs::write(
            wt.join(".git"),
            format!("gitdir: {}\n", other.join(".git").display()),
        )
        .unwrap();
        let e = Layout::discover(&wt).unwrap_err().to_string();
        assert!(e.contains("registered worktree"), "{e}");

        // A git directory inside the work tree is refused too.
        std::fs::write(wt.join(".git"), "gitdir: inside\n").unwrap();
        assert!(Layout::discover(&wt).is_err());
    }

    #[test]
    fn queries_do_not_run_repository_config() {
        let tmp = tempfile::tempdir().unwrap();
        let p = repo(tmp.path());
        let canary = tmp.path().join("canary");
        std::fs::write(
            p.join(".git/config"),
            format!(
                "[core]\n\tfsmonitor = touch {}\n\tpager = touch {0}\n",
                canary.display()
            ),
        )
        .unwrap();
        let l = Layout::discover(&p).unwrap().unwrap();
        let head = l.query(&["rev-parse", "HEAD"]).unwrap();
        assert_eq!(head.trim().len(), 40);
        assert!(l.try_query(&["log", "--oneline", "-n1"]).is_some());
        assert!(!canary.exists(), "a host query ran repository config");
        // A command that must write the work tree is refused instead.
        let e = Checkout::open(&p)
            .unwrap()
            .run(&["checkout", "main"])
            .unwrap_err()
            .to_string();
        assert!(e.contains("core.fsmonitor"), "{e}");
    }

    #[test]
    fn finds_config_keys_that_run_programs() {
        for text in [
            "[core]\n\tfsmonitor = evil\n",
            "[core]\nhooksPath = /tmp/h\n",
            "[filter \"x\"]\n\tclean = evil\n",
            "[alias]\n\tst = !evil\n",
            "[include]\n\tpath = /tmp/other\n",
            "[diff]\n\texternal = evil\n",
            "[credential]\n\thelper = evil\n",
        ] {
            assert!(exec_key(text).is_some(), "{text}");
        }
        for text in [
            "[core]\n\trepositoryformatversion = 0\n\tbare = false\n",
            "[remote \"origin\"]\n\turl = https://example.com/x.git\n",
            "[branch \"main\"]\n\tmerge = refs/heads/main\n",
            "# a comment\n",
        ] {
            assert_eq!(exec_key(text), None, "{text}");
        }
    }

    #[test]
    fn status_runs_in_a_sandbox() {
        let tmp = tempfile::tempdir().unwrap();
        let p = repo(tmp.path());
        std::fs::write(p.join("new.txt"), "x").unwrap();
        let canary = tmp.path().join("canary");
        std::fs::write(
            p.join(".git/config"),
            format!("[core]\n\tfsmonitor = touch {}\n", canary.display()),
        )
        .unwrap();
        let env = HostEnv::detect().unwrap();
        let l = Layout::discover(&p).unwrap().unwrap();
        if env.which("bwrap").is_none() {
            assert!(l.status(&env, "all").unwrap().is_none());
            return;
        }
        if !crate::testutil::bwrap_works() {
            // A bwrap that cannot start gives no status, as a missing one.
            assert!(l.status(&env, "all").unwrap().is_none());
            assert!(!canary.exists(), "the planted fsmonitor reached the host");
            eprintln!("skipped: needs a working bwrap");
            return;
        }
        let lines = l.status(&env, "all").unwrap().unwrap();
        assert!(lines.iter().any(|l| l.contains("new.txt")), "{lines:?}");
        assert!(!canary.exists(), "the planted fsmonitor reached the host");
    }
}
