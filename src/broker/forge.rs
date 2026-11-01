//! The `forge` capability: a fixed set of `gh`, `tea` and `glab` commands
//! that the host runs with the user's own login, for the project's own
//! repository. The token stays on the host. Only the output goes back to
//! the sandbox.
//!
//! Each tool is a table: its commands, flags and defaults.

use super::git::Remote;
use crate::env::HostEnv;
use anyhow::{bail, Context as _, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
}

/// How the tool names a repository in its repository flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoForm {
    /// HOST/OWNER/REPO (gh).
    HostPath,
    /// https://HOST/OWNER/REPO (glab: it also takes nested groups).
    Url,
    /// OWNER/REPO. The login flag chooses the host (tea).
    Slug,
}

/// A command, by its group and subcommand words.
pub type Cmd = (&'static str, &'static str);

pub struct Tool {
    pub name: &'static str,
    /// (group, subcommand, access). Anything else is refused. An empty
    /// subcommand means `TOOL GROUP INDEX`: the word after the group must be
    /// a number (tea shows a pull request with `tea pulls 12`).
    pub commands: &'static [(&'static str, &'static str, Access)],
    /// Flags that name the repository: (short, long).
    pub repo_flags: (&'static str, &'static str),
    pub repo_form: RepoForm,
    /// The flag that picks a login. The sandbox cannot pass it. The host
    /// adds it with its own login for the repository's host.
    pub login_flag: Option<&'static str>,
    /// Flags that read a host file, open an editor or a browser on the host,
    /// or pick another host or login.
    pub denied_flags: &'static [&'static str],
    /// Short flags of `denied_flags`, also refused inside a group like `-dw`.
    pub denied_short: &'static str,
    /// Flags that certainly take a value. Their value is not read as a flag.
    pub value_flags: &'static [&'static str],
    /// Subcommands that act on the current branch when no argument names one.
    pub branch_default: &'static [Cmd],
    /// (command, long, short): a flag that names the source branch. The host
    /// adds it with the current branch when it is missing.
    pub branch_flags: &'static [(Cmd, &'static str, &'static str)],
    /// Commands that name the repository as their argument.
    pub repo_positional: &'static [Cmd],
    /// (command, long, short): the flag that holds the body text. The group
    /// "*" stands for every command.
    pub body_flags: &'static [(Cmd, &'static str, &'static str)],
    /// Commands whose arguments after the index are the body text.
    pub body_args: &'static [Cmd],
    /// The arguments that read the body from stdin. Empty: the body stays
    /// in the arguments.
    pub body_stdin: &'static [&'static str],
    /// Commands that open an editor or a prompt without a body.
    pub needs_body: &'static [Cmd],
    /// The host adds its flags right after the command words, before any
    /// argument (tea stops reading flags at the first argument).
    pub flags_first: bool,
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
    repo_form: RepoForm::HostPath,
    login_flag: None,
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
    branch_flags: &[(("pr", "create"), "--head", "-H")],
    repo_positional: &[("repo", "view")],
    body_flags: &[(("*", "*"), "--body", "-b")],
    body_args: &[],
    body_stdin: &["--body-file", "-"],
    needs_body: &[],
    flags_first: false,
};

/// tea, the Gitea and Forgejo command line.
pub const TEA: Tool = Tool {
    name: "tea",
    commands: &[
        ("pulls", "list", Read),
        ("pulls", "", Read),
        ("pulls", "create", Write),
        ("issues", "list", Read),
        ("issues", "", Read),
        ("comment", "", Write),
        ("releases", "list", Read),
    ],
    repo_flags: ("-r", "--repo"),
    repo_form: RepoForm::Slug,
    login_flag: Some("--login"),
    denied_flags: &["-l", "--login", "-R", "--remote", "-w", "--web", "--asset"],
    denied_short: "lRw",
    value_flags: &[
        "--title",
        "-t",
        "--description",
        "-d",
        "--head",
        "--base",
        "-b",
        "--assignees",
        "-a",
        "--labels",
        "-L",
        "--milestone",
        "-m",
        "--milestones",
        "--deadline",
        "-D",
        "--referenced-version",
        "-v",
        "--state",
        "--limit",
        "--lm",
        "--page",
        "-p",
        "--output",
        "-o",
        "--fields",
        "-f",
        "--keyword",
        "-k",
        "--author",
        "-A",
        "--assignee",
        "--mentions",
        "-M",
        "--kind",
        "-K",
        "--from",
        "-F",
        "--until",
        "-u",
        "--owner",
        "-O",
    ],
    branch_default: &[],
    branch_flags: &[(("pulls", "create"), "--head", "")],
    repo_positional: &[],
    body_flags: &[(("pulls", "create"), "--description", "-d")],
    body_args: &[("comment", "")],
    body_stdin: &[],
    needs_body: &[("comment", "")],
    flags_first: true,
};

/// glab, the GitLab command line.
pub const GLAB: Tool = Tool {
    name: "glab",
    commands: &[
        ("mr", "list", Read),
        ("mr", "view", Read),
        ("mr", "create", Write),
        ("mr", "diff", Read),
        ("mr", "note", Write),
        ("issue", "list", Read),
        ("issue", "view", Read),
        ("issue", "note", Write),
        ("ci", "view", Read),
        ("ci", "list", Read),
        ("ci", "status", Read),
        ("repo", "view", Read),
    ],
    repo_flags: ("-R", "--repo"),
    repo_form: RepoForm::Url,
    login_flag: None,
    denied_flags: &[
        "-w",
        "--web",
        "--hostname",
        "--description-file",
        "--notes-file",
        "--template-file",
        "--editor",
        "--push",
        "--create-source-branch",
    ],
    denied_short: "w",
    value_flags: &[
        "--title",
        "-t",
        "--assignee",
        "-a",
        "--reviewer",
        "--label",
        "--milestone",
        "-m",
        "--source-branch",
        "-s",
        "--target-branch",
        "-b",
        "--branch",
        "--author",
        "--per-page",
        "-P",
        "--page",
        "-p",
        "--output",
        "-F",
        "--status",
        "--sort",
        "--order",
        "--pipeline-id",
    ],
    branch_default: &[
        ("mr", "view"),
        ("mr", "diff"),
        ("mr", "note"),
        ("ci", "view"),
    ],
    branch_flags: &[
        (("mr", "create"), "--source-branch", "-s"),
        (("ci", "status"), "--branch", "-b"),
    ],
    repo_positional: &[("repo", "view")],
    body_flags: &[
        (("mr", "create"), "--description", "-d"),
        (("mr", "note"), "--message", "-m"),
        (("issue", "note"), "--message", "-m"),
    ],
    body_args: &[],
    body_stdin: &[],
    needs_body: &[("mr", "note"), ("issue", "note")],
    flags_first: false,
};

pub const TOOLS: &[&Tool] = &[&GH, &TEA, &GLAB];

pub fn tool(name: &str) -> Option<&'static Tool> {
    TOOLS.iter().copied().find(|t| t.name == name)
}

/// A repository on a forge, for example github.com/demo/app. The owner can
/// hold slashes: GitLab groups nest.
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
    let (owner, name) = path.rsplit_once('/')?;
    let ok = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
            && s != "."
            && s != ".."
    };
    if !ok(name) || !ok(host) || !owner.split('/').all(ok) {
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

    /// Parse a `--repo` value: [HOST/]OWNER/REPO or a URL. Without a host,
    /// the host is github.com.
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

    /// OWNER/REPO.
    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }

    /// The repository as the tool's repository flag takes it.
    pub fn form(&self, f: RepoForm) -> String {
        match f {
            RepoForm::HostPath => self.to_string(),
            RepoForm::Url => format!("https://{self}"),
            RepoForm::Slug => self.slug(),
        }
    }
}

/// Find the project repository that a `--repo` value names: a URL, a git
/// URL, HOST/OWNER/REPO, or OWNER/REPO when only one remote has that name.
fn resolve(value: &str, known: &[RepoId]) -> Result<RepoId> {
    let v = value.trim().trim_end_matches('/');
    let v = v.strip_suffix(".git").unwrap_or(v).to_ascii_lowercase();
    let not_ours = || {
        anyhow::anyhow!(
            "{value} is not a repository of this project. Allowed: {}",
            known
                .iter()
                .map(|k| k.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    if v.contains(':') || v.contains('@') {
        let id =
            RepoId::from_url(&v).with_context(|| format!("{value:?} is not a repository name"))?;
        return known
            .iter()
            .find(|k| **k == id)
            .cloned()
            .ok_or_else(not_ours);
    }
    if let Some(k) = known.iter().find(|k| k.to_string() == v) {
        return Ok(k.clone());
    }
    let by_slug: Vec<&RepoId> = known.iter().filter(|k| k.slug() == v).collect();
    match by_slug.as_slice() {
        [one] => Ok((*one).clone()),
        [] => Err(not_ours()),
        _ => bail!("{value} names a repository on more than one host: pass HOST/{v}"),
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
pub const EXEC_TOOLS: &[&str] = &["gh", "tea", "glab"];

/// Variables that the tool gets besides the base ones: its own login and
/// host settings, the session bus for a keyring, and proxies.
pub const FORGE_ENV: &[&str] = &[
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "GH_HOST",
    "GH_ENTERPRISE_TOKEN",
    "GITHUB_ENTERPRISE_TOKEN",
    "GH_CONFIG_DIR",
    "GITLAB_TOKEN",
    "GITLAB_ACCESS_TOKEN",
    "GLAB_CONFIG_DIR",
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
        ("NO_PROMPT", "1"),
        ("NO_COLOR", "1"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

/// The command that an argument list starts with: (command, access, the
/// number of command words).
fn command(t: &Tool, argv: &[String]) -> Option<(Cmd, Access, usize)> {
    let (g, s) = (argv.first()?, argv.get(1)?);
    if let Some(&(cg, cs, a)) = t
        .commands
        .iter()
        .find(|(cg, cs, _)| cg == g && cs == s && !cs.is_empty())
    {
        return Some(((cg, cs), a, 2));
    }
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()) {
        if let Some(&(cg, cs, a)) = t
            .commands
            .iter()
            .find(|(cg, cs, _)| cg == g && cs.is_empty())
        {
            return Some(((cg, cs), a, 1));
        }
    }
    None
}

fn label(cmd: Cmd) -> String {
    if cmd.1.is_empty() {
        format!("{} INDEX", cmd.0)
    } else {
        format!("{} {}", cmd.0, cmd.1)
    }
}

/// tea reads flags like Go: `-login` is `--login`, and short flags never
/// form a group. Write each flag in one form, so the checks see its name.
fn normalize(t: &Tool, argv: &[String]) -> Vec<String> {
    if !t.flags_first {
        return argv.to_vec();
    }
    let mut out = Vec::new();
    let mut i = 0;
    while i < argv.len() {
        let a = &argv[i];
        i += 1;
        if a == "--" {
            out.extend(argv[i - 1..].iter().cloned());
            break;
        }
        if !a.starts_with('-') || a == "-" {
            out.push(a.clone());
            continue;
        }
        let rest = a.trim_start_matches('-');
        let (name, value) = match rest.split_once('=') {
            Some((n, v)) => (n, Some(v)),
            None => (rest, None),
        };
        let dashes = if name.chars().count() == 1 { "-" } else { "--" };
        let flag = format!("{dashes}{name}");
        let takes = value.is_none() && t.value_flags.contains(&flag.as_str());
        out.push(match value {
            Some(v) => format!("{flag}={v}"),
            None => flag,
        });
        if takes {
            if let Some(v) = argv.get(i) {
                out.push(v.clone());
                i += 1;
            }
        }
    }
    out
}

/// The body flag of a command: (long, short).
fn body_flag(t: &Tool, cmd: Option<Cmd>) -> Option<(&'static str, &'static str)> {
    t.body_flags
        .iter()
        .find(|(c, _, _)| c.0 == "*" || Some(*c) == cmd)
        .map(|&(_, l, s)| (l, s))
}

/// Split body text out of arguments: (the arguments without it, the body).
/// Handles `--body X`, `-b X`, `--body=X`, `-bX` and `-b=X` (for each tool
/// its own body flag), and the text arguments of `tea comment`. The last
/// body wins, as in `gh`. Values of other flags and anything after `--`
/// stay.
pub fn split_body(t: &Tool, argv: &[String]) -> (Vec<String>, Option<String>) {
    let argv = normalize(t, argv);
    let found = command(t, &argv);
    let cmd = found.map(|c| c.0);
    let words = found.map_or(0, |c| c.2);
    let flag = body_flag(t, cmd);
    let text_args = cmd.is_some_and(|c| t.body_args.contains(&c));
    let mut out: Vec<String> = argv[..words].to_vec();
    let mut body = None;
    let mut text: Vec<String> = Vec::new();
    let mut positional = 0;
    let mut i = words;
    while i < argv.len() {
        let a = &argv[i];
        i += 1;
        if a == "--" {
            if text_args {
                text.extend(argv[i..].iter().cloned());
            } else {
                out.extend(argv[i - 1..].iter().cloned());
            }
            break;
        }
        if let Some((long, short)) = flag {
            if a == long || (!short.is_empty() && a == short) {
                body = Some(argv.get(i).cloned().unwrap_or_default());
                i += 1;
                continue;
            }
            if let Some(v) = a.strip_prefix(&format!("{long}=")) {
                body = Some(v.to_string());
                continue;
            }
            if let Some(v) = a
                .strip_prefix(short)
                .filter(|_| !short.is_empty() && !a.starts_with("--") && !t.flags_first)
            {
                body = Some(v.strip_prefix('=').unwrap_or(v).to_string());
                continue;
            }
        }
        if a.starts_with('-') && a != "-" {
            out.push(a.clone());
            if t.value_flags.contains(&a.as_str()) {
                if let Some(v) = argv.get(i) {
                    out.push(v.clone());
                    i += 1;
                }
            }
            continue;
        }
        positional += 1;
        if text_args && positional > 1 {
            text.push(a.clone());
        } else {
            out.push(a.clone());
        }
    }
    if !text.is_empty() {
        body = Some(text.join(" "));
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
/// A tool with a login flag (tea) gets no login: use `check_with`.
pub fn check(
    t: &'static Tool,
    argv: &[String],
    branch: Option<&str>,
    remotes: &[Remote],
) -> Result<Call> {
    check_with(t, argv, branch, remotes, &|r| {
        bail!("{} has no login for {}", t.name, r.host)
    })
}

/// `check`, with the host's login for a repository's host. The host passes
/// it with the tool's login flag.
pub fn check_with(
    t: &'static Tool,
    argv: &[String],
    branch: Option<&str>,
    remotes: &[Remote],
    login: &dyn Fn(&RepoId) -> Result<String>,
) -> Result<Call> {
    let allowed = || {
        t.commands
            .iter()
            .map(|&(g, s, _)| label((g, s)))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let (argv, body) = split_body(t, argv);
    let Some((cmd, access, words)) = command(t, &argv) else {
        match argv.as_slice() {
            [g, s, ..] => bail!(
                "`{} {g} {s}` is not allowed through paddock-host. Allowed: {}",
                t.name,
                allowed()
            ),
            _ => bail!("{} needs a command. Allowed: {}", t.name, allowed()),
        }
    };
    let (known, default) = repos(remotes);
    if known.is_empty() {
        bail!("no remote of the project names a {} repository", t.name);
    }
    let pick = |value: &str| resolve(value, &known);

    let (short, long) = t.repo_flags;
    let text_args = t.body_args.contains(&cmd);
    let rest = &argv[words..];
    let mut out: Vec<String> = Vec::new();
    let mut repo: Option<RepoId> = None;
    let mut positional = 0;
    let mut has_branch = false;
    let mut after_dashes = false;
    let mut i = 0;
    while i < rest.len() {
        let a = &rest[i];
        i += 1;
        if after_dashes {
            positional += 1;
            check_positional(a, &known, &pick)?;
            out.push(a.clone());
            continue;
        }
        if a == "--" {
            if text_args {
                bail!("write the text without --");
            }
            after_dashes = true;
            out.push(a.clone());
            continue;
        }
        // The repository flag, in each form: -R X, -RX, --repo X, --repo=X.
        let repo_value = if a == short || a == long {
            Some(rest.get(i).context("--repo needs a value")?.clone()).inspect(|_| i += 1)
        } else if let Some(v) = a
            .strip_prefix(&format!("{long}="))
            .or_else(|| a.strip_prefix(&format!("{short}=")))
        {
            Some(v.to_string())
        } else if a.starts_with(short)
            && a.len() > short.len()
            && !a.starts_with("--")
            && !t.flags_first
        {
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
        if t.login_flag == Some(flag) || (t.login_flag.is_some() && flag == "-l") {
            bail!(
                "{flag} is not allowed through paddock-host: the host uses its own {} login for the repository's host",
                t.name
            );
        }
        if t.denied_flags.contains(&flag) {
            bail!(
                "{flag} is not allowed through paddock-host: it would read a host file, open a program on the host, or pick another host or login. Pass the text as an argument instead"
            );
        }
        if a.starts_with('-')
            && !a.starts_with("--")
            && a.len() > 2
            && !t.flags_first
            && a[1..].chars().any(|c| t.denied_short.contains(c))
        {
            bail!("{a} is not allowed through paddock-host: write each flag on its own");
        }
        if t.branch_flags
            .iter()
            .any(|&(c, l, s)| c == cmd && (flag == l || (!s.is_empty() && flag == s)))
        {
            has_branch = true;
        }
        out.push(a.clone());
        if a.starts_with('-') && a != "-" {
            if t.value_flags.contains(&a.as_str()) {
                // The next argument is this flag's value, never a flag.
                if let Some(v) = rest.get(i) {
                    out.push(v.clone());
                    i += 1;
                }
            }
        } else {
            positional += 1;
            check_positional(a, &known, &pick)?;
        }
    }

    let repo = match (repo, t.login_flag) {
        (Some(r), _) => r,
        // A tool that picks the host by its login (tea): the default
        // repository when the host has a login for it, or else the first
        // remote's repository that it has a login for.
        (None, Some(_)) => default
            .iter()
            .chain(known.iter())
            .find(|r| login(r).is_ok())
            .cloned()
            .with_context(|| {
                format!(
                    "{} has no login on the host for any remote of this project",
                    t.name
                )
            })?,
        (None, None) => default.context("cannot tell which repository to use; pass --repo")?,
    };
    let mut argv_final: Vec<String> = argv[..words].to_vec();
    // Added flags go before a `--`, where the tool still reads flags.
    let tail: Vec<String> = match out.iter().position(|a| a == "--") {
        Some(p) => out.drain(p..).collect(),
        None => Vec::new(),
    };
    let mut added: Vec<String> = Vec::new();
    if t.repo_positional.contains(&cmd) {
        // `repo view` names the repository as its argument.
        if positional > 0 {
            bail!("pass the repository with --repo");
        }
        argv_final.push(repo.form(t.repo_form));
    } else {
        let branch = branch.filter(|b| !b.is_empty() && !b.starts_with('-'));
        if positional == 0 && t.branch_default.contains(&cmd) {
            let b = branch.context("name the pull request: no current branch")?;
            out.insert(0, b.to_string());
        }
        if let Some(&(_, l, _)) = t.branch_flags.iter().find(|(c, _, _)| *c == cmd) {
            if !has_branch {
                let b = branch.with_context(|| format!("pass {l} BRANCH: no current branch"))?;
                added.extend([l.to_string(), b.to_string()]);
            }
        }
        added.extend([long.to_string(), repo.form(t.repo_form)]);
    }
    if let Some(f) = t.login_flag {
        added.extend([f.to_string(), login(&repo)?]);
    }
    let mut body_out: Vec<String> = Vec::new();
    let mut stdin = None;
    match body {
        None if t.needs_body.contains(&cmd) => {
            bail!("{} {} needs the text", t.name, label(cmd))
        }
        None => {}
        Some(b) if !t.body_stdin.is_empty() => {
            body_out.extend(t.body_stdin.iter().map(|s| s.to_string()));
            stdin = Some(b);
        }
        Some(b) if b.trim() == "-" => {
            bail!("a body of - would open an editor on the host")
        }
        Some(b) if text_args => {
            if b.starts_with('-') {
                bail!("the text cannot start with -");
            }
            body_out.push(b);
        }
        Some(b) => {
            let (l, _) = body_flag(t, Some(cmd)).context("this command takes no body")?;
            body_out.push(format!("{l}={b}"));
        }
    }
    if t.flags_first {
        argv_final.extend(added);
        argv_final.extend(out);
    } else {
        argv_final.extend(out);
        argv_final.extend(added);
    }
    argv_final.extend(body_out);
    argv_final.extend(tail);
    Ok(Call {
        tool: t.name,
        argv: argv_final,
        access,
        repo,
        command: label(cmd),
        stdin,
    })
}

/// A URL or OWNER/REPO#N argument must point into a project repository.
fn check_positional(
    a: &str,
    known: &[RepoId],
    pick: &dyn Fn(&str) -> Result<RepoId>,
) -> Result<()> {
    if let Some((_, rest)) = a.split_once("://") {
        // https://HOST/OWNER/REPO/pull/12, or .../-/merge_requests/3.
        let rest = rest.to_ascii_lowercase();
        let (authority, path) = rest.split_once('/').unwrap_or((&rest, ""));
        let host = authority.rsplit('@').next().unwrap_or("");
        let host = host.split(':').next().unwrap_or("");
        let inside = known.iter().any(|k| {
            let base = k.slug();
            k.host == host
                && path
                    .strip_prefix(&base)
                    .is_some_and(|r| r.is_empty() || r.starts_with('/') || r.starts_with('#'))
        });
        if !inside {
            bail!("{a:?} is not a URL of this project's repository");
        }
    } else if let Some((repo, _)) = a.split_once('#') {
        if repo.contains('/') {
            pick(repo)?;
        }
    }
    Ok(())
}

/// The name of tea's login for a host, from tea's config on the host:
/// the login marked default for that host, or else the first one.
pub fn tea_login(env: &HostEnv, host: &str) -> Result<String> {
    let file = env.config_dir.join("tea/config.yml");
    let text = std::fs::read_to_string(&file).with_context(|| {
        format!("tea has no logins on the host. Ask the user to run `tea login add` for {host}")
    })?;
    tea_login_in(&text, host).with_context(|| {
        format!("tea has no login for {host} on the host. Ask the user to run `tea login add`")
    })
}

/// Read the `logins:` list of tea's config.yml. Only name, url and default
/// are read. The token is never read.
fn tea_login_in(text: &str, host: &str) -> Option<String> {
    #[derive(Default)]
    struct Login {
        name: String,
        url: String,
        default: bool,
    }
    let mut logins: Vec<Login> = Vec::new();
    let mut inside = false;
    for line in text.lines() {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let top = !line.starts_with(' ') && !line.starts_with('-');
        if top {
            inside = line.trim_end() == "logins:";
            continue;
        }
        if !inside {
            continue;
        }
        let mut l = line.trim_start();
        if let Some(r) = l.strip_prefix("- ") {
            logins.push(Login::default());
            l = r.trim_start();
        }
        let Some(cur) = logins.last_mut() else {
            continue;
        };
        let Some((k, v)) = l.split_once(':') else {
            continue;
        };
        let v = v.trim().trim_matches(|c| c == '"' || c == '\'').to_string();
        match k.trim() {
            "name" => cur.name = v,
            "url" => cur.url = v,
            "default" => cur.default = v == "true",
            _ => {}
        }
    }
    let host_of = |u: &str| {
        url::Url::parse(u)
            .ok()
            .and_then(|p| p.host_str().map(|h| h.to_ascii_lowercase()))
    };
    let matching: Vec<&Login> = logins
        .iter()
        .filter(|l| !l.name.is_empty() && host_of(&l.url).as_deref() == Some(host))
        .collect();
    matching
        .iter()
        .find(|l| l.default)
        .or(matching.first())
        .map(|l| l.name.clone())
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

    fn tea_remotes() -> Vec<Remote> {
        vec![
            Remote {
                name: "origin".into(),
                url: "git@codeberg.org:Demo/tool.git".into(),
                push_url: None,
                gh_resolved: None,
            },
            Remote {
                name: "mirror".into(),
                url: "https://git.example.org/demo/app.git".into(),
                push_url: None,
                gh_resolved: None,
            },
        ]
    }

    fn tea(args: &[&str]) -> Result<Call> {
        let a: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        check_with(
            &TEA,
            &a,
            Some("fix-login"),
            &tea_remotes(),
            &|r| match r.host.as_str() {
                "codeberg.org" => Ok("cb".into()),
                "git.example.org" => Ok("work".into()),
                h => bail!("no login for {h}"),
            },
        )
    }

    #[test]
    fn tea_runs_its_allowlist_with_the_host_login() {
        let c = tea(&["pulls", "list", "--state", "open"]).unwrap();
        assert_eq!(c.access, Access::Read);
        assert_eq!(
            c.argv,
            [
                "pulls",
                "list",
                "--repo",
                "demo/tool",
                "--login",
                "cb",
                "--state",
                "open"
            ]
        );
        let c = tea(&["pulls", "12"]).unwrap();
        assert_eq!(
            c.argv,
            ["pulls", "--repo", "demo/tool", "--login", "cb", "12"]
        );
        assert_eq!(c.command, "pulls INDEX");
        let c = tea(&["issues", "list", "-r", "git.example.org/demo/app"]).unwrap();
        assert_eq!(
            c.argv,
            ["issues", "list", "--repo", "demo/app", "--login", "work"]
        );
        assert_eq!(tea(&["releases", "list"]).unwrap().access, Access::Read);
        let c = tea(&["comment", "3", "Looks", "good"]).unwrap();
        assert_eq!(c.access, Access::Write);
        assert_eq!(
            c.argv,
            [
                "comment",
                "--repo",
                "demo/tool",
                "--login",
                "cb",
                "3",
                "Looks good"
            ]
        );
        assert!(c.stdin.is_none());
        for bad in [
            &["pulls", "checkout", "5"][..],
            &["pulls", "merge", "5"],
            &["pulls", "close", "5"],
            &["pulls", "abc"],
            &["issues", "create"],
            &["releases", "create"],
            &["login", "list"],
            &["repos", "list"],
            &["api", "/user"],
            &["open"],
            &["comment"],
            &["comment", "3"],
            &["comment", "3", "--", "-x"],
        ] {
            assert!(tea(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn tea_refuses_logins_remotes_browsers_and_files() {
        for bad in [
            &["pulls", "list", "--login", "other"][..],
            &["pulls", "list", "--login=other"],
            &["pulls", "list", "-l", "other"],
            &["pulls", "list", "-login", "other"],
            &["pulls", "list", "---login", "other"],
            &["pulls", "list", "--remote", "origin"],
            &["pulls", "list", "-R", "origin"],
            &["pulls", "list", "-remote", "origin"],
            &["pulls", "list", "--web"],
            &["pulls", "list", "-w"],
            &["pulls", "create", "--asset", "/etc/passwd"],
            &["pulls", "create", "--title", "x", "--description", "-"],
            &["comment", "3", "-", "x"],
        ] {
            assert!(tea(bad).is_err(), "{bad:?}");
        }
        // A value that looks like a flag stays a value.
        let c = tea(&["pulls", "create", "--title", "--login is fixed"]).unwrap();
        assert!(c.argv.contains(&"--login is fixed".to_string()));
        assert_eq!(c.argv.iter().filter(|a| *a == "--login").count(), 1);
    }

    #[test]
    fn tea_names_only_project_repositories() {
        for bad in [
            &["pulls", "list", "--repo", "other/app"][..],
            &["pulls", "list", "-r", "other/app"],
            &["pulls", "list", "--repo=codeberg.org/other/tool"],
            &["pulls", "list", "-repo", "github.com/demo/tool"],
            &["pulls", "list", "--repo", "https://codeberg.org/other/tool"],
            &["pulls", "list", "--repo", "/srv/demo/tool"],
            &["issues", "12", "https://codeberg.org/other/tool/issues/1"],
        ] {
            assert!(tea(bad).is_err(), "{bad:?}");
        }
        for ok in [
            "demo/tool",
            "codeberg.org/demo/tool",
            "https://codeberg.org/demo/tool.git",
        ] {
            assert_eq!(
                tea(&["pulls", "list", "--repo", ok]).unwrap().repo.host,
                "codeberg.org"
            );
        }
        // Without the host's login for that host, nothing runs.
        let a: Vec<String> = ["pulls", "list"].iter().map(|s| s.to_string()).collect();
        assert!(check(&TEA, &a, None, &tea_remotes()).is_err());
    }

    #[test]
    fn tea_pull_requests_start_from_the_current_branch() {
        let c = tea(&["pulls", "create", "--title", "Fix", "--description", "Text"]).unwrap();
        assert_eq!(
            c.argv,
            [
                "pulls",
                "create",
                "--head",
                "fix-login",
                "--repo",
                "demo/tool",
                "--login",
                "cb",
                "--title",
                "Fix",
                "--description=Text"
            ]
        );
        let c = tea(&["pulls", "create", "--head", "other", "-d", "x"]).unwrap();
        assert_eq!(c.argv.iter().filter(|a| *a == "--head").count(), 1);
        assert!(c.argv.contains(&"other".to_string()));
        let a: Vec<String> = ["pulls", "create"].iter().map(|s| s.to_string()).collect();
        assert!(check_with(&TEA, &a, None, &tea_remotes(), &|_| Ok("cb".into())).is_err());
        // The body never shows in the audit line.
        let (rest, body) = split_body(&TEA, &["comment".into(), "3".into(), "secret".into()]);
        assert_eq!(rest, ["comment", "3"]);
        assert_eq!(body.as_deref(), Some("secret"));
    }

    #[test]
    fn tea_logins_come_from_its_config() {
        let cfg = "logins:\n- name: work\n  url: https://git.example.org\n  token: abc\n  default: false\n- name: cb\n  url: https://codeberg.org\n  default: false\n- name: cb2\n  url: \"https://Codeberg.org/\"\n  default: true\npreferences:\n  editor: false\n";
        assert_eq!(tea_login_in(cfg, "codeberg.org").as_deref(), Some("cb2"));
        assert_eq!(
            tea_login_in(cfg, "git.example.org").as_deref(),
            Some("work")
        );
        assert_eq!(tea_login_in(cfg, "github.com"), None);
        assert_eq!(tea_login_in("preferences:\n  x: 1\n", "codeberg.org"), None);
    }

    fn glab(args: &[&str]) -> Result<Call> {
        let remotes = vec![
            Remote {
                name: "origin".into(),
                url: "git@gitlab.example.org:team/apps/web.git".into(),
                push_url: None,
                gh_resolved: None,
            },
            Remote {
                name: "mirror".into(),
                url: "https://gitlab.com/demo/app".into(),
                push_url: None,
                gh_resolved: None,
            },
        ];
        let a: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        check(&GLAB, &a, Some("fix-login"), &remotes)
    }

    const WEB: &str = "https://gitlab.example.org/team/apps/web";

    #[test]
    fn glab_runs_its_allowlist() {
        let c = glab(&["mr", "list"]).unwrap();
        assert_eq!(c.access, Access::Read);
        assert_eq!(c.argv, ["mr", "list", "--repo", WEB]);
        let c = glab(&["mr", "create", "--title", "Fix", "-d", "Text"]).unwrap();
        assert_eq!(c.access, Access::Write);
        assert_eq!(
            c.argv,
            [
                "mr",
                "create",
                "--title",
                "Fix",
                "--source-branch",
                "fix-login",
                "--repo",
                WEB,
                "--description=Text"
            ]
        );
        let c = glab(&["mr", "note", "3", "-m", "-1 from me"]).unwrap();
        assert_eq!(
            c.argv,
            ["mr", "note", "3", "--repo", WEB, "--message=-1 from me"]
        );
        assert_eq!(
            glab(&["issue", "note", "5", "--message=ok"])
                .unwrap()
                .access,
            Access::Write
        );
        let c = glab(&["repo", "view", "-R", "gitlab.com/demo/app"]).unwrap();
        assert_eq!(c.argv, ["repo", "view", "https://gitlab.com/demo/app"]);
        for ok in [
            &["issue", "list", "--label", "bug"][..],
            &["issue", "view", "7"],
            &["ci", "list", "--status", "failed"],
            &["mr", "diff", "3"],
            &[
                "mr",
                "view",
                "https://gitlab.example.org/team/apps/web/-/merge_requests/1",
            ],
        ] {
            assert_eq!(glab(ok).unwrap().access, Access::Read, "{ok:?}");
        }
        for bad in [
            &["mr", "merge", "3"][..],
            &["mr", "approve", "3"],
            &["mr", "checkout", "3"],
            &["mr", "update", "3"],
            &["ci", "run"],
            &["ci", "retry", "4"],
            &["ci", "delete", "4"],
            &["api", "projects"],
            &["auth", "status"],
            &["variable", "list"],
            &["release", "create", "v1"],
            &["mr"],
            &["mr", "note", "3"],
            &["issue", "note", "3"],
        ] {
            assert!(glab(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn glab_refuses_browsers_files_and_other_hosts() {
        for bad in [
            &["mr", "view", "3", "--web"][..],
            &["mr", "view", "3", "-w"],
            &["mr", "view", "3", "-cw"],
            &["ci", "view", "-w"],
            &["mr", "list", "--hostname", "evil.example"],
            &["mr", "list", "--hostname=evil.example"],
            &["mr", "create", "--description-file", "/etc/passwd"],
            &["mr", "create", "--title", "x", "--push"],
            &["mr", "create", "--title", "x", "-d", "-"],
        ] {
            assert!(glab(bad).is_err(), "{bad:?}");
        }
        assert!(glab(&["mr", "create", "--title", "--web is fixed"]).is_ok());
    }

    #[test]
    fn glab_names_only_project_repositories() {
        for bad in [
            &["mr", "list", "-R", "team/apps/other"][..],
            &["mr", "list", "-Rteam/other"],
            &["mr", "list", "--repo=https://gitlab.example.org/team/apps"],
            &["mr", "list", "--repo", "gitlab.com/team/apps/web"],
            &[
                "mr",
                "view",
                "https://gitlab.example.org/team/apps/website/-/merge_requests/1",
            ],
            &[
                "mr",
                "view",
                "https://gitlab.com/other/app/-/merge_requests/1",
            ],
            &["repo", "view", "team/apps/web"],
        ] {
            assert!(glab(bad).is_err(), "{bad:?}");
        }
        for ok in ["team/apps/web", "gitlab.example.org/team/apps/web", WEB] {
            assert_eq!(
                glab(&["mr", "list", "-R", ok]).unwrap().repo.owner,
                "team/apps"
            );
        }
    }

    #[test]
    fn glab_defaults_to_the_current_branch() {
        let c = glab(&["mr", "view"]).unwrap();
        assert_eq!(c.argv, ["mr", "view", "fix-login", "--repo", WEB]);
        let c = glab(&["ci", "view"]).unwrap();
        assert_eq!(c.argv, ["ci", "view", "fix-login", "--repo", WEB]);
        let c = glab(&["ci", "status"]).unwrap();
        assert_eq!(
            c.argv,
            ["ci", "status", "--branch", "fix-login", "--repo", WEB]
        );
        let c = glab(&["ci", "status", "-b", "main"]).unwrap();
        assert_eq!(c.argv, ["ci", "status", "-b", "main", "--repo", WEB]);
        let c = glab(&["mr", "create", "-s", "other", "--title", "x"]).unwrap();
        assert_eq!(c.argv.iter().filter(|a| a.contains("source")).count(), 0);
        // The body never shows in the audit line.
        let (rest, body) = split_body(
            &GLAB,
            &[
                "mr".into(),
                "note".into(),
                "3".into(),
                "-m".into(),
                "secret".into(),
            ],
        );
        assert_eq!(rest, ["mr", "note", "3"]);
        assert_eq!(body.as_deref(), Some("secret"));
    }
}
