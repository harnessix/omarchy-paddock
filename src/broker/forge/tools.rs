//! The gh, tea and glab command tables.

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
    /// Refuse every flag that is not in `value_flags` or `bool_flags`. For
    /// tools whose flags paddock did not check against the real program:
    /// an unknown flag could read a host file into a comment.
    pub only_known: bool,
    /// Flags without a value, for `only_known`.
    pub bool_flags: &'static [&'static str],
    /// The flag that takes a jq expression: (long, short). gh's jq reads
    /// the host environment (`$ENV`, `env`), where a token can be.
    pub jq_flags: (&'static str, &'static str),
    /// The token and login variables of the host that the tool reads.
    pub env: &'static [&'static str],
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
    // Only these commands have a body: `-b` is `--branch` of `run list`.
    body_flags: &[
        (("pr", "create"), "--body", "-b"),
        (("pr", "comment"), "--body", "-b"),
        (("issue", "comment"), "--body", "-b"),
    ],
    body_args: &[],
    body_stdin: &["--body-file", "-"],
    needs_body: &[],
    flags_first: false,
    only_known: false,
    bool_flags: &[],
    jq_flags: ("--jq", "-q"),
    env: &[
        "GH_TOKEN",
        "GITHUB_TOKEN",
        "GH_HOST",
        "GH_ENTERPRISE_TOKEN",
        "GITHUB_ENTERPRISE_TOKEN",
        "GH_CONFIG_DIR",
    ],
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
    only_known: true,
    bool_flags: &["--allow-maintainer-edits", "--comments"],
    jq_flags: ("", ""),
    // tea keeps its logins in its own config.yml.
    env: &[],
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
    only_known: true,
    bool_flags: &[
        "--draft",
        "--fill",
        "-f",
        "--remove-source-branch",
        "--squash-before-merge",
        "--comments",
        "-c",
        "--all",
        "--yes",
        "-y",
    ],
    jq_flags: ("", ""),
    env: &["GITLAB_TOKEN", "GITLAB_ACCESS_TOKEN", "GLAB_CONFIG_DIR"],
};

pub const TOOLS: &[&Tool] = &[&GH, &TEA, &GLAB];

pub fn tool(name: &str) -> Option<&'static Tool> {
    TOOLS.iter().copied().find(|t| t.name == name)
}
