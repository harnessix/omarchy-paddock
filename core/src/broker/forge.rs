//! The `forge` capability: a fixed set of `gh`, `tea` and `glab` commands
//! that the host runs with the user's own login, for the project's own
//! repository. The token stays on the host. Only the output goes back to
//! the sandbox.
//!
//! Each tool is a table: its commands, flags and defaults.

use super::git::Remote;
use anyhow::{bail, Context as _, Result};

mod check;
mod tools;

#[cfg(test)]
pub(in crate::broker::forge) use check::tea_login_in;
pub use check::{check, check_with, split_body, tea_login};
pub use tools::{tool, Access, Cmd, RepoForm, Tool, GH, GLAB, TEA, TOOLS};

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

/// Variables that every tool gets besides the base ones and its own login
/// variables (`Tool::env`): the config folder, the session bus for a
/// keyring, and proxies. A tool never gets the token of another forge.
pub const FORGE_ENV: &[&str] = &[
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

/// The host variables that the executor passes to a tool.
pub fn forge_env(name: &str) -> Vec<String> {
    let own = tool(name).map_or(&[][..], |t| t.env);
    own.iter().chain(FORGE_ENV).map(|s| s.to_string()).collect()
}

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

impl Call {
    pub fn grant_key(&self) -> String {
        format!("forge\0{}\0{}\0{}", self.tool, self.command, self.repo)
    }
}

#[cfg(test)]
mod tests;
