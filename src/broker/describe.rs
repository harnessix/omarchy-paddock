//! The card text and the checks for one request.

use super::{forge, git, Request, INBOX, PROTOCOL};
use anyhow::{bail, Context as _, Result};
use std::path::{Path, PathBuf};

pub(in crate::broker) fn arg_str<'a>(req: &'a Request, key: &str) -> Option<&'a str> {
    req.args.get(key).and_then(|v| v.as_str())
}

/// The arguments of a `tool` request: a list of strings, or none.
pub(crate) fn tool_args(req: &Request) -> Result<Vec<String>> {
    match req.args.get("args") {
        None | Some(serde_json::Value::Null) => Ok(Vec::new()),
        Some(v) => serde_json::from_value(v.clone()).context("tool arguments must be strings"),
    }
}

pub(in crate::broker) fn tool_line(req: &Request) -> String {
    std::iter::once(arg_str(req, "name").unwrap_or("").to_string())
        .chain(tool_args(req).unwrap_or_default())
        .map(|a| shell_quote(&a))
        .collect::<Vec<_>>()
        .join(" ")
}

pub(in crate::broker) fn argv(req: &Request) -> Vec<String> {
    req.args
        .get("argv")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Quote one argument for display, the way a shell would read it.
pub(in crate::broker) fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:,@%+".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// (summary, detail) for the approval prompt and the audit log.
pub fn describe(req: &Request, project: &Path) -> (String, String) {
    match req.cap.as_str() {
        "screenshot" => {
            let t = arg_str(req, "target").unwrap_or("screen");
            (
                format!("screenshot of the {t}"),
                format!("Capture the host {t} into {}/{INBOX}", project.display()),
            )
        }
        "exec" => {
            let line = argv(req)
                .iter()
                .map(|a| shell_quote(a))
                .collect::<Vec<_>>()
                .join(" ");
            (
                format!(
                    "run on the host: {}",
                    line.chars().take(80).collect::<String>()
                ),
                format!(
                    "Run on the host in {}.\nThe sandbox writes that folder, so a command that reads files there, a git command for example, can take instructions from them.\n{line}",
                    project.display()
                ),
            )
        }
        "fetch" => {
            let p = arg_str(req, "path").unwrap_or("");
            (
                format!("copy {p} into the sandbox"),
                format!("Copy the host path {p} into {}/{INBOX}", project.display()),
            )
        }
        "clipboard-read" => (
            "read the host clipboard".into(),
            "Give the current host clipboard text to the sandbox".into(),
        ),
        "clipboard-write" => {
            let t = arg_str(req, "text").unwrap_or("");
            (
                "set the host clipboard".into(),
                format!(
                    "Set the host clipboard to:\n{}",
                    t.chars().take(500).collect::<String>()
                ),
            )
        }
        "open-url" => {
            let u = arg_str(req, "url").unwrap_or("");
            (
                format!("open {u}"),
                format!("Open in the host browser:\n{u}"),
            )
        }
        "notify" => (
            "show a notification".into(),
            arg_str(req, "title").unwrap_or("").to_string(),
        ),
        "git-push" => (
            format!("git push {}", arg_str(req, "remote").unwrap_or("")),
            format!(
                "git push {} {}",
                arg_str(req, "remote").unwrap_or(""),
                req.args
                    .get("refs")
                    .map(|r| r.to_string())
                    .unwrap_or_default()
            ),
        ),
        "forge" => {
            let line = std::iter::once(arg_str(req, "tool").unwrap_or("gh").to_string())
                .chain(argv(req))
                .map(|a| shell_quote(&a))
                .collect::<Vec<_>>()
                .join(" ");
            (line.chars().take(80).collect(), line)
        }
        other => (other.to_string(), other.to_string()),
    }
}

/// A key that "allow for this session" remembers. For `exec` it is the exact
/// command line, so a grant never covers a different command.
pub(in crate::broker) fn grant_key(req: &Request) -> String {
    match req.cap.as_str() {
        "exec" => format!("exec\0{}", argv(req).join("\0")),
        "open-url" => "open-url".into(),
        other => other.to_string(),
    }
}

pub(in crate::broker) fn validate(req: &Request) -> Result<()> {
    if req.v != PROTOCOL {
        bail!(
            "protocol version {} is not supported; this paddock speaks {PROTOCOL}",
            req.v
        );
    }
    match req.cap.as_str() {
        "screenshot" => {
            let t = arg_str(req, "target").unwrap_or("screen");
            if !(t == "screen"
                || t == "window"
                || t.strip_prefix("output:")
                    .is_some_and(|n| !n.is_empty() && n.len() < 64))
            {
                bail!("target must be screen, window or output:NAME");
            }
        }
        "exec" => {
            if argv(req).is_empty() {
                bail!("exec needs a command");
            }
        }
        "tool" => {
            let n = arg_str(req, "name").context("tool needs a tool name")?;
            if !crate::hosttools::valid_name(n) {
                bail!("not a valid tool name: {n}");
            }
            tool_args(req)?;
        }
        "fetch" => {
            let p = PathBuf::from(arg_str(req, "path").context("fetch needs a path")?);
            if !p.is_absolute() {
                bail!("fetch needs an absolute host path");
            }
            // `prepare` opens the path and checks where it really leads.
        }
        "borrow" => {
            let p = PathBuf::from(arg_str(req, "path").context("borrow needs a path")?);
            if !p.is_absolute() {
                bail!("borrow needs an absolute host path");
            }
        }
        "return" => {
            let n = arg_str(req, "name").context("return needs the name of a borrowed copy")?;
            if !crate::borrow::valid_name(n) {
                bail!("{n} is not the name of a borrowed copy");
            }
        }
        "clipboard-write" => {
            arg_str(req, "text").context("clipboard-write needs text")?;
        }
        "open-url" => {
            let u = arg_str(req, "url").context("open-url needs a url")?;
            let parsed = url::Url::parse(u).context("not a valid URL")?;
            if !matches!(parsed.scheme(), "http" | "https") {
                bail!("only http and https URLs can be opened");
            }
        }
        "notify" => {
            arg_str(req, "title").context("notify needs a title")?;
        }
        "clipboard-read" => {}
        "git-push" => git::validate(&req.args)?,
        "forge" => {
            let t = arg_str(req, "tool").unwrap_or("gh");
            if forge::tool(t).is_none() {
                bail!(
                    "forge supports {}",
                    forge::TOOLS
                        .iter()
                        .map(|t| t.name)
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            if argv(req).is_empty() {
                bail!("forge needs a command, for example pr view");
            }
        }
        "spawn" => {
            let name_ok = |s: &str| {
                !s.is_empty()
                    && s.len() <= 64
                    && s.chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            };
            match arg_str(req, "action").unwrap_or("start") {
                "start" => {
                    if !arg_str(req, "harness").is_some_and(name_ok) {
                        bail!("spawn needs a harness name, for example codex");
                    }
                    if arg_str(req, "prompt_file").is_none_or(str::is_empty) {
                        bail!("spawn needs --prompt-file FILE");
                    }
                }
                "status" | "wait" | "kill" => {
                    if !arg_str(req, "id").is_some_and(name_ok) {
                        bail!("give the spawn id that `paddock-host spawn` printed");
                    }
                }
                "list" => {}
                other => bail!("unknown spawn action {other}"),
            }
        }
        other => bail!("unknown capability {other}"),
    }
    Ok(())
}

pub(crate) fn safe_name(raw: &str, fallback: &str) -> String {
    let name: String = Path::new(raw)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "-_.".contains(c) {
                c
            } else {
                '_'
            }
        })
        .take(100)
        .collect();
    if name.is_empty() || name.starts_with('.') {
        fallback.to_string()
    } else {
        name
    }
}
