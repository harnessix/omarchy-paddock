//! The `forge` capability: a fixed set of `gh` commands that the host runs
//! with the user's own login, for the project's own repository. The token
//! stays on the host; only the output goes back to the sandbox.
//!
//! Each tool is a table, so `glab` or `tea` can join later with their own
//! commands and flags.

use super::git::Remote;
use anyhow::{bail, Context as _, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
}

pub struct Tool {
    pub name: &'static str,
    /// (group, subcommand, access). Anything else is refused.
    pub commands: &'static [(&'static str, &'static str, Access)],
    /// Flags that name the repository.
    pub repo_flags: (&'static str, &'static str),
    /// Flags that read a host file, open an editor or a browser on the host.
    pub denied_flags: &'static [&'static str],
    /// Short flags of `denied_flags`, also refused inside a group like `-dw`.
    pub denied_short: &'static str,
    /// Flags that certainly take a value. Their value is not read as a flag.
    pub value_flags: &'static [&'static str],
    /// Subcommands that act on the current branch when no argument names one.
    pub branch_default: &'static [(&'static str, &'static str)],
}

use Access::{Read, Write};

pub const GH: Tool = Tool {
    name: "gh",
    commands: &[
        ("pr", "create", Write),
        ("pr", "view", Read),
        ("pr", "list", Read),
        ("pr", "checks", Read),
        ("pr", "diff", Read),
        ("pr", "comment", Write),
        ("issue", "view", Read),
        ("issue", "list", Read),
        ("issue", "comment", Write),
        ("run", "view", Read),
        ("run", "watch", Read),
        ("run", "list", Read),
        ("repo", "view", Read),
    ],
    repo_flags: ("-R", "--repo"),
    denied_flags: &[
        "-w",
        "--web",
        "-e",
        "--editor",
        "-F",
        "--body-file",
        "-T",
        "--template-file",
        "--recover",
        "--hostname",
    ],
    denied_short: "weFT",
    value_flags: &[
        "--title",
        "-t",
        "--body",
        "-b",
        "--base",
        "-B",
        "--head",
        "-H",
        "--label",
        "--assignee",
        "--reviewer",
        "--milestone",
        "--project",
        "--json",
        "--jq",
        "-q",
        "--template",
        "--search",
        "--state",
        "--limit",
        "-L",
        "--author",
        "--interval",
        "--branch",
        "--workflow",
        "--color",
        "--job",
        "--attempt",
    ],
    branch_default: &[
        ("pr", "view"),
        ("pr", "checks"),
        ("pr", "diff"),
        ("pr", "comment"),
    ],
};

pub const TOOLS: &[&Tool] = &[&GH];

pub fn tool(name: &str) -> Option<&'static Tool> {
    TOOLS.iter().copied().find(|t| t.name == name)
}

/// A repository on a forge, for example github.com/demo/app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoId {
    pub host: String,
    pub owner: String,
    pub name: String,
}

impl std::fmt::Display for RepoId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}/{}", self.host, self.owner, self.name)
    }
}

fn split_path(host: &str, path: &str) -> Option<RepoId> {
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut parts = path.split('/');
    let (owner, name) = (parts.next()?, parts.next()?);
    if parts.next().is_some() || owner.is_empty() || name.is_empty() || host.is_empty() {
        return None;
    }
    let ok = |s: &str| {
        s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
    };
    if !ok(owner) || !ok(name) || !ok(host) {
        return None;
    }
    Some(RepoId {
        host: host.to_ascii_lowercase(),
        owner: owner.to_ascii_lowercase(),
        name: name.to_ascii_lowercase(),
    })
}

impl RepoId {
    /// Parse a remote URL: https://HOST/OWNER/REPO, ssh://git@HOST/OWNER/REPO
    /// or git@HOST:OWNER/REPO.
    pub fn from_url(u: &str) -> Option<RepoId> {
        if let Some(rest) = u.split_once("://").map(|(_, r)| r) {
            let (authority, path) = rest.split_once('/')?;
            let host = authority.rsplit('@').next()?;
            let host = host.split(':').next()?;
            return split_path(host, path);
        }
        let (user_host, path) = u.split_once(':')?;
        if user_host.contains('/') {
            return None;
        }
        split_path(user_host.rsplit('@').next()?, path)
    }

    /// Parse a `--repo` value: [HOST/]OWNER/REPO or a URL.
    pub fn from_arg(s: &str) -> Option<RepoId> {
        if s.contains("://") {
            return Self::from_url(s);
        }
        match s.split('/').count() {
            2 => split_path("github.com", s),
            3 => {
                let (host, path) = s.split_once('/')?;
                split_path(host, path)
            }
            _ => None,
        }
    }
}

/// The repositories that the project's remotes name, and the default one.
/// The default follows `gh`: the remote that `gh repo set-default` chose,
/// then upstream, github, origin, then the first.
pub fn repos(remotes: &[Remote]) -> (Vec<RepoId>, Option<RepoId>) {
    let ids: Vec<(String, Option<String>, RepoId)> = remotes
        .iter()
        .filter_map(|r| {
            RepoId::from_url(&r.url).map(|id| (r.name.clone(), r.gh_resolved.clone(), id))
        })
        .collect();
    let by_name = |n: &str| {
        ids.iter()
            .find(|(name, _, _)| name == n)
            .map(|x| x.2.clone())
    };
    let default = ids
        .iter()
        .find(|(_, g, _)| g.as_deref() == Some("base"))
        .map(|x| x.2.clone())
        .or_else(|| by_name("upstream"))
        .or_else(|| by_name("github"))
        .or_else(|| by_name("origin"))
        .or_else(|| ids.first().map(|x| x.2.clone()));
    let mut all: Vec<RepoId> = Vec::new();
    for (_, _, id) in ids {
        if !all.contains(&id) {
            all.push(id);
        }
    }
    (all, default)
}

/// A checked call, ready to run on the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    pub tool: &'static str,
    /// The arguments after the tool name, with the repository made explicit.
    pub argv: Vec<String>,
    pub access: Access,
    pub repo: RepoId,
    pub command: String,
    /// Body text for `--body-file -`. It goes on stdin, not in the arguments:
    /// every local user can read the arguments of a host process.
    pub stdin: Option<String>,
}

/// The approved call, for the executor.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ForgeExec {
    /// The program, found on the host's PATH by the broker.
    pub tool: String,
    pub argv: Vec<String>,
    pub stdin: Option<String>,
    pub timeout: u64,
    /// An empty folder of paddock to run in, never the project.
    pub workdir: std::path::PathBuf,
}

/// The tools that the executor runs for `forge`.
pub const EXEC_TOOLS: &[&str] = &["gh"];

/// Variables that the tool gets besides the base ones: its own login and
/// host settings, the session bus for a keyring, and proxies.
pub const FORGE_ENV: &[&str] = &[
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "GH_HOST",
    "GH_ENTERPRISE_TOKEN",
    "GITHUB_ENTERPRISE_TOKEN",
    "GH_CONFIG_DIR",
    "XDG_CONFIG_HOME",
    "DBUS_SESSION_BUS_ADDRESS",
    "XDG_RUNTIME_DIR",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
];

/// The settings that make the tool quiet and non-interactive.
pub fn forge_vars() -> std::collections::BTreeMap<String, String> {
    [
        ("GH_PROMPT_DISABLED", "1"),
        ("GH_NO_UPDATE_NOTIFIER", "1"),
        ("GH_SPINNER_DISABLED", "1"),
        ("NO_COLOR", "1"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

/// Split body text out of arguments: (the arguments without it, the body).
/// Handles `--body X`, `-b X`, `--body=X`, `-bX` and `-b=X`; the last body
/// wins, as in `gh`. Values of other flags and anything after `--` stay.
pub fn split_body(t: &Tool, argv: &[String]) -> (Vec<String>, Option<String>) {
    let mut out = Vec::new();
    let mut body = None;
    let mut i = 0;
    while i < argv.len() {
        let a = &argv[i];
        i += 1;
        if a == "--" {
            out.extend(argv[i - 1..].iter().cloned());
            break;
        }
        if a == "--body" || a == "-b" {
            body = Some(argv.get(i).cloned().unwrap_or_default());
            i += 1;
        } else if let Some(v) = a.strip_prefix("--body=") {
            body = Some(v.to_string());
        } else if let Some(v) = a.strip_prefix("-b").filter(|_| !a.starts_with("--")) {
            body = Some(v.strip_prefix('=').unwrap_or(v).to_string());
        } else {
            out.push(a.clone());
            if t.value_flags.contains(&a.as_str()) {
                if let Some(v) = argv.get(i) {
                    out.push(v.clone());
                    i += 1;
                }
            }
        }
    }
    (out, body)
}

impl Call {
    pub fn grant_key(&self) -> String {
        format!("forge\0{}\0{}\0{}", self.tool, self.command, self.repo)
    }
}

/// Check one call against the allowlist and the project's repositories.
/// `branch` is the sandbox's current branch, for commands that default to it.
pub fn check(
    t: &'static Tool,
    argv: &[String],
    branch: Option<&str>,
    remotes: &[Remote],
) -> Result<Call> {
    let allowed = || {
        t.commands
            .iter()
            .map(|(g, s, _)| format!("{g} {s}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let (group, sub) = match argv {
        [g, s, ..] => (g.as_str(), s.as_str()),
        _ => bail!("{} needs a command. Allowed: {}", t.name, allowed()),
    };
    let Some(&(_, _, access)) = t.commands.iter().find(|(g, s, _)| *g == group && *s == sub) else {
        bail!(
            "`{} {group} {sub}` is not allowed through paddock-host. Allowed: {}",
            t.name,
            allowed()
        );
    };
    let (known, default) = repos(remotes);
    if known.is_empty() {
        bail!("no remote of the project names a {} repository", t.name);
    }
    let pick = |value: &str| -> Result<RepoId> {
        let id = RepoId::from_arg(value)
            .with_context(|| format!("{value:?} is not a repository name"))?;
        if !known.contains(&id) {
            bail!(
                "{} is not a repository of this project. Allowed: {}",
                id,
                known
                    .iter()
                    .map(|k| k.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        Ok(id)
    };

    let (short, long) = t.repo_flags;
    let rest = &argv[2..];
    let mut out: Vec<String> = Vec::new();
    let mut repo: Option<RepoId> = None;
    let mut positional = 0;
    let mut has_head = false;
    let mut after_dashes = false;
    let mut i = 0;
    while i < rest.len() {
        let a = &rest[i];
        i += 1;
        if after_dashes {
            positional += 1;
            check_positional(a, &pick)?;
            out.push(a.clone());
            continue;
        }
        if a == "--" {
            after_dashes = true;
            out.push(a.clone());
            continue;
        }
        // The repository flag, in each form: -R X, -RX, --repo X, --repo=X.
        let repo_value = if a == short || a == long {
            Some(rest.get(i).context("--repo needs a value")?.clone()).inspect(|_| i += 1)
        } else if let Some(v) = a.strip_prefix(&format!("{long}=")) {
            Some(v.to_string())
        } else if a.starts_with(short) && a.len() > short.len() && !a.starts_with("--") {
            Some(a[short.len()..].trim_start_matches('=').to_string())
        } else {
            None
        };
        if let Some(v) = repo_value {
            let id = pick(&v)?;
            if repo.as_ref().is_some_and(|r| r != &id) {
                bail!("name the repository only once");
            }
            repo = Some(id);
            continue;
        }
        let flag = a.split('=').next().unwrap_or(a);
        if t.denied_flags.contains(&flag) {
            bail!(
                "{flag} is not allowed through paddock-host: it would read a host file or open a program on the host. Pass the text with --body instead"
            );
        }
        if a.starts_with('-')
            && !a.starts_with("--")
            && a.len() > 2
            && a[1..].chars().any(|c| t.denied_short.contains(c))
        {
            bail!("{a} is not allowed through paddock-host: write each flag on its own");
        }
        if matches!(flag, "--head" | "-H") {
            has_head = true;
        }
        out.push(a.clone());
        if a.starts_with('-') {
            if t.value_flags.contains(&a.as_str()) {
                // The next argument is this flag's value, never a flag.
                if let Some(v) = rest.get(i) {
                    out.push(v.clone());
                    i += 1;
                }
            }
        } else {
            positional += 1;
            check_positional(a, &pick)?;
        }
    }

    let repo = match repo {
        Some(r) => r,
        None => default.context("cannot tell which repository to use; pass --repo")?,
    };
    let mut argv_final = vec![group.to_string(), sub.to_string()];
    // Added flags go before a `--`, where the tool still reads flags.
    let mut tail: Vec<String> = match out.iter().position(|a| a == "--") {
        Some(p) => out.drain(p..).collect(),
        None => Vec::new(),
    };
    if (group, sub) == ("repo", "view") {
        // `repo view` names the repository as its argument.
        if positional > 0 {
            bail!("pass the repository with --repo");
        }
        argv_final.push(repo.to_string());
    } else {
        let branch = branch.filter(|b| !b.is_empty() && !b.starts_with('-'));
        if positional == 0 && t.branch_default.contains(&(group, sub)) {
            let b = branch.context("name the pull request: no current branch")?;
            out.insert(0, b.to_string());
        }
        if (group, sub) == ("pr", "create") && !has_head {
            let b = branch.context("pass --head BRANCH: no current branch")?;
            out.extend(["--head".to_string(), b.to_string()]);
        }
        out.extend([long.to_string(), repo.to_string()]);
    }
    let (mut out, stdin) = split_body(t, &out);
    if stdin.is_some() {
        out.extend(["--body-file".to_string(), "-".to_string()]);
    }
    out.append(&mut tail);
    argv_final.extend(out);
    Ok(Call {
        tool: t.name,
        argv: argv_final,
        access,
        repo,
        command: format!("{group} {sub}"),
        stdin,
    })
}

/// A URL or OWNER/REPO#N argument must point into a project repository.
fn check_positional(a: &str, pick: &dyn Fn(&str) -> Result<RepoId>) -> Result<()> {
    if a.contains("://") {
        let id = RepoId::from_url(a)
            .or_else(|| {
                // https://HOST/OWNER/REPO/pull/12
                let (scheme, rest) = a.split_once("://")?;
                let mut p = rest.splitn(4, '/');
                let (h, o, n) = (p.next()?, p.next()?, p.next()?);
                RepoId::from_url(&format!("{scheme}://{h}/{o}/{n}"))
            })
            .with_context(|| format!("{a:?} is not a URL of this project's repository"))?;
        pick(&id.to_string())?;
    } else if let Some((repo, _)) = a.split_once('#') {
        if repo.contains('/') {
            pick(repo)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remotes() -> Vec<Remote> {
        vec![
            Remote {
                name: "origin".into(),
                url: "git@github.com:demo/app.git".into(),
                push_url: None,
                gh_resolved: None,
            },
            Remote {
                name: "upstream".into(),
                url: "https://github.com/acme/app".into(),
                push_url: None,
                gh_resolved: None,
            },
        ]
    }

    fn run(args: &[&str]) -> Result<Call> {
        let a: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        check(&GH, &a, Some("fix-login"), &remotes())
    }

    #[test]
    fn urls_and_repo_arguments_parse() {
        let want = RepoId {
            host: "github.com".into(),
            owner: "demo".into(),
            name: "app".into(),
        };
        for u in [
            "git@github.com:demo/app.git",
            "https://github.com/demo/app",
            "https://github.com/demo/app.git",
            "ssh://git@github.com:22/demo/app.git",
            "https://token@GitHub.com/Demo/app/",
        ] {
            assert_eq!(RepoId::from_url(u).as_ref(), Some(&want), "{u}");
        }
        assert_eq!(RepoId::from_arg("demo/app").as_ref(), Some(&want));
        assert_eq!(
            RepoId::from_arg("github.com/demo/app").as_ref(),
            Some(&want)
        );
        assert!(RepoId::from_arg("demo").is_none());
        assert!(RepoId::from_url("/srv/app.git").is_none());
    }

    #[test]
    fn only_the_allowlist_passes() {
        for bad in [
            &["api", "user"][..],
            &["auth", "token"],
            &["secret", "list"],
            &["repo", "delete"],
            &["pr", "merge"],
            &["pr", "checkout"],
            &["run", "download"],
            &["extension", "install"],
            &["pr"],
        ] {
            assert!(run(bad).is_err(), "{bad:?}");
        }
        assert_eq!(run(&["pr", "list"]).unwrap().access, Access::Read);
        assert_eq!(
            run(&["pr", "create", "--title", "x"]).unwrap().access,
            Access::Write
        );
        assert_eq!(
            run(&["issue", "comment", "3", "--body", "x"])
                .unwrap()
                .access,
            Access::Write
        );
    }

    #[test]
    fn the_repository_must_be_a_project_remote() {
        assert!(run(&["pr", "list", "-R", "other/app"]).is_err());
        assert!(run(&["pr", "list", "--repo=other/app"]).is_err());
        assert!(run(&["pr", "list", "-Rother/app"]).is_err());
        assert!(run(&["pr", "view", "https://github.com/other/app/pull/1"]).is_err());
        assert!(run(&["issue", "view", "other/app#4"]).is_err());
        assert!(run(&["repo", "view", "other/app"]).is_err());
        let c = run(&["pr", "list", "-R", "demo/app"]).unwrap();
        assert_eq!(c.argv, ["pr", "list", "--repo", "github.com/demo/app"]);
        let c = run(&["pr", "view", "https://github.com/demo/app/pull/1"]).unwrap();
        assert_eq!(c.repo.owner, "acme");
    }

    #[test]
    fn the_default_repository_and_branch_are_explicit() {
        // upstream comes before origin, as in gh.
        let c = run(&["pr", "create", "--title", "Fix", "--body", "Text"]).unwrap();
        assert_eq!(
            c.argv,
            [
                "pr",
                "create",
                "--title",
                "Fix",
                "--head",
                "fix-login",
                "--repo",
                "github.com/acme/app",
                "--body-file",
                "-"
            ]
        );
        assert_eq!(c.stdin.as_deref(), Some("Text"));
        let c = run(&["pr", "checks", "--watch"]).unwrap();
        assert_eq!(c.argv[..4], ["pr", "checks", "fix-login", "--watch"]);
        let c = run(&["pr", "view", "12", "--json", "title"]).unwrap();
        assert_eq!(c.argv[2], "12");
        let c = run(&["repo", "view"]).unwrap();
        assert_eq!(c.argv, ["repo", "view", "github.com/acme/app"]);
        let mut r = remotes();
        r[0].gh_resolved = Some("base".into());
        let c = check(&GH, &["pr".into(), "list".into()], None, &r).unwrap();
        assert_eq!(c.repo.owner, "demo");
    }

    #[test]
    fn host_files_editors_and_browsers_are_refused() {
        for bad in [
            &["pr", "create", "--body-file", "/etc/passwd"][..],
            &["pr", "create", "-F", "x"],
            &["pr", "comment", "--editor"],
            &["pr", "view", "--web"],
            &["pr", "create", "-dw"],
            &["pr", "create", "--recover", "/tmp/x"],
            &["pr", "create", "-T", "bug.md"],
        ] {
            assert!(run(bad).is_err(), "{bad:?}");
        }
        // A value that looks like a flag stays a value.
        assert!(run(&["pr", "create", "--title", "--web is fixed"]).is_ok());
    }

    #[test]
    fn body_text_never_stays_in_the_arguments() {
        // Short flags with an attached value must not hold w, e, F or T,
        // which the denied-flag check reads as a group of short flags.
        for (args, body) in [
            (
                &["pr", "comment", "1", "--body", "private text"][..],
                "private text",
            ),
            (
                &["pr", "comment", "1", "-b", "private text"][..],
                "private text",
            ),
            (
                &["pr", "comment", "1", "--body=private text"][..],
                "private text",
            ),
            (&["pr", "comment", "1", "-bsignal"][..], "signal"),
            (&["issue", "comment", "2", "-b=signal"][..], "signal"),
            (
                &[
                    "pr",
                    "comment",
                    "1",
                    "--body",
                    "old",
                    "--body",
                    "private text",
                ][..],
                "private text",
            ),
        ] {
            let c = run(args).unwrap();
            assert!(
                !c.argv.iter().any(|a| a.contains(body) || a.contains("old")),
                "{:?}",
                c.argv
            );
            assert_eq!(c.argv[c.argv.len() - 2..], ["--body-file", "-"]);
            assert_eq!(c.stdin.as_deref(), Some(body), "{args:?}");
        }
        // The value of another flag that looks like --body stays a value.
        let c = run(&["pr", "create", "--title", "--body", "--head", "x"]).unwrap();
        assert_eq!(c.argv[2..4], ["--title", "--body"]);
        assert!(c.stdin.is_none());
    }

    #[test]
    fn a_session_grant_key_covers_one_command_on_one_repository() {
        let a = run(&["pr", "comment", "1", "--body", "a"]).unwrap();
        let b = run(&["pr", "comment", "2", "--body", "b"]).unwrap();
        let c = run(&["issue", "comment", "2", "--body", "b"]).unwrap();
        assert_eq!(a.grant_key(), b.grant_key());
        assert_ne!(a.grant_key(), c.grant_key());
    }
}
