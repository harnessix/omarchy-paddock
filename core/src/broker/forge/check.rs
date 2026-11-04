//! Check one forge command against the allowlist.

use super::{repos, resolve, Access, Call, Cmd, Remote, RepoId, Tool};
use crate::env::HostEnv;
use anyhow::{bail, Context as _, Result};

/// A jq expression that reads the environment: `$ENV` or `env`.
fn reads_env(expr: &str) -> bool {
    let word = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let lower = expr.to_ascii_lowercase();
    lower.match_indices("env").any(|(i, _)| {
        !lower[..i].chars().next_back().is_some_and(word)
            && !lower[i + 3..].chars().next().is_some_and(word)
    })
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
            if let Some(v) = a.strip_prefix(&format!("{long}=")).or_else(|| {
                a.strip_prefix(&format!("{short}="))
                    .filter(|_| !short.is_empty())
            }) {
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
        let (jq_long, jq_short) = t.jq_flags;
        if !jq_long.is_empty() {
            // --jq X, --jq=X, -q X, -qX, and -q inside a group like -cq X.
            let attached = if flag == jq_long {
                Some(a.split_once('=').map_or("", |(_, v)| v))
            } else if a.starts_with('-') && !a.starts_with("--") {
                a[1..].split_once(&jq_short[1..]).map(|(_, v)| v)
            } else {
                None
            };
            let expr: Vec<&str> = attached
                .into_iter()
                .chain(attached.and(rest.get(i).map(String::as_str)))
                .collect();
            if expr.iter().any(|e| reads_env(e)) {
                bail!(
                    "a jq expression cannot read the environment ($ENV, env) through paddock-host: the host environment can hold a token"
                );
            }
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
        if t.only_known
            && a.starts_with('-')
            && a != "-"
            && !t.value_flags.contains(&flag)
            && !t.bool_flags.contains(&flag)
        {
            bail!(
                "{flag} is not allowed through paddock-host: {} takes only the flags that paddock knows ({})",
                t.name,
                t.value_flags
                    .iter()
                    .chain(t.bool_flags.iter())
                    .filter(|f| f.starts_with("--"))
                    .copied()
                    .collect::<Vec<_>>()
                    .join(", ")
            );
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
        // The sandbox names the branch: as an argument, a pull request URL
        // or OWNER/REPO#N would pick another repository over --repo.
        if let Some(b) = branch {
            check_positional(b, &known, &pick)?;
        }
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
pub(in crate::broker::forge) fn tea_login_in(text: &str, host: &str) -> Option<String> {
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
