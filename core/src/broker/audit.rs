//! The audit log of host requests.

use super::describe::{arg_str, argv, describe, shell_quote, tool_args, tool_line};
use super::{forge, git, Context, Request, Response};
use crate::env::HostEnv;
use crate::state;
use std::io::Write;
use std::path::{Path, PathBuf};

pub fn audit_file(env: &HostEnv) -> PathBuf {
    env.paddock_state().join("host-audit.jsonl")
}

/// What the audit log keeps about a request. The log lives on disk for a
/// long time, so it never holds request text that can be private:
///
/// - clipboard-write: the length of the text, never the text.
/// - exec: the program and the number of arguments. Arguments can hold
///   tokens or passwords (`curl -H "Authorization: ..."`), so they stay out.
///   An allowed command line of plain words (`hosttools::loggable_argv`)
///   goes into `argv`, for `paddock tools suggest`.
/// - tool: the tool name and the arguments, which match a shape that the
///   user wrote.
/// - forge: the command line without the body text, and the body length.
/// - notify: the lengths of the title and the body.
/// - open-url: the scheme and host. Paths and queries can hold login codes.
/// - fetch, git-push, screenshot: the paths, remote and refs, as the user saw
///   them. These name host locations, not private content.
/// - borrow, return: the host folder, the size and the counts of changes.
///
/// `prepared` is the detail that `prepare` worked out, for the capabilities
/// where it is safe to keep (fetch and git-push).
pub fn audit_detail(req: &Request, project: &Path, prepared: Option<&str>) -> String {
    let chars = |key: &str| arg_str(req, key).map_or(0, |t| t.chars().count());
    match req.cap.as_str() {
        "clipboard-write" => format!("set the host clipboard: {} characters", chars("text")),
        "exec" => {
            let a = argv(req);
            let total: usize =
                a.iter().map(|x| x.chars().count()).sum::<usize>() + a.len().saturating_sub(1);
            format!(
                "run on the host: {} with {} arguments ({total} characters; arguments are not logged)",
                a.first().map(|p| shell_quote(p)).unwrap_or_default(),
                a.len().saturating_sub(1),
            )
        }
        // The arguments match a shape that the user wrote: they stay.
        "tool" => format!("run the host tool {}", tool_line(req)),
        "forge" => {
            let tool = arg_str(req, "tool").unwrap_or("gh");
            let t = forge::tool(tool).unwrap_or(&forge::GH);
            let (rest, body) = forge::split_body(t, &argv(req));
            let mut line = std::iter::once(tool.to_string())
                .chain(rest)
                .map(|a| shell_quote(&a))
                .collect::<Vec<_>>()
                .join(" ");
            if let Some(b) = body {
                line.push_str(&format!(" (body: {} characters)", b.chars().count()));
            }
            line
        }
        "notify" => format!(
            "show a notification: title {} characters, body {} characters",
            chars("title"),
            chars("body")
        ),
        "open-url" => {
            let u = arg_str(req, "url").unwrap_or("");
            match url::Url::parse(u) {
                Ok(p) => format!(
                    "open {}://{} ({} characters)",
                    p.scheme(),
                    p.host_str().unwrap_or(""),
                    u.chars().count()
                ),
                Err(_) => format!("open a URL ({} characters)", u.chars().count()),
            }
        }
        "fetch" | "git-push" | "spawn" | "borrow" | "return" if prepared.is_some() => {
            prepared.unwrap_or_default().to_string()
        }
        "spawn" => format!(
            "spawn {} {}",
            arg_str(req, "action").unwrap_or("start"),
            arg_str(req, "harness").or(arg_str(req, "id")).unwrap_or("")
        ),
        "fetch" => format!(
            "copy {} into the sandbox",
            arg_str(req, "path").unwrap_or("")
        ),
        "borrow" => format!("borrow {}", arg_str(req, "path").unwrap_or("")),
        "return" => format!("return {}", arg_str(req, "name").unwrap_or("")),
        "git-push" => format!(
            "git push {}",
            git::redact(arg_str(req, "remote").unwrap_or(""))
        ),
        "screenshot" | "clipboard-read" => describe(req, project).1,
        other => other.to_string(),
    }
}

#[allow(clippy::too_many_arguments)]
pub(in crate::broker) fn audit(
    env: &HostEnv,
    ctx: &Context,
    req: &Request,
    detail: &str,
    request: Option<&str>,
    decision: &str,
    resp: &Response,
    note: Option<&str>,
) {
    // The note of a denial goes to the agent only: the log keeps its length.
    let message = match note {
        Some(_) => "the user denied the request with a note",
        None => resp.message.as_str(),
    };
    let line = serde_json::json!({
        "time": state::now(),
        "sandbox": ctx.id,
        "harness": ctx.harness,
        "project": ctx.project,
        "capability": req.cap,
        "request": request,
        "detail": detail,
        "reason": req.reason.chars().take(500).collect::<String>(),
        "decision": decision,
        "status": resp.status,
        "message": message,
    });
    let mut line = line;
    if let Some(n) = note {
        line["note_chars"] = n.chars().count().into();
    }
    if let Some(code) = resp.result.get("exit_code") {
        line["exit_code"] = code.clone();
    }
    if req.cap == "tool" {
        line["tool"] = arg_str(req, "name").unwrap_or("").into();
        line["args"] = serde_json::json!(tool_args(req).unwrap_or_default());
    }
    // A plain exec line that the user allowed, for `paddock tools suggest`,
    // only when the user asked for it (`[exec] remember_allowed = true`).
    if req.cap == "exec"
        && decision.starts_with("allowed")
        && crate::execbox::ExecCfg::load(env).is_ok_and(|c| c.remember_allowed)
    {
        if let Some(a) = crate::hosttools::loggable_argv(&argv(req)) {
            line["argv"] = serde_json::json!(a);
        }
    }
    if let Some(parent) = ctx.chain.last() {
        line["parent"] = parent.clone().into();
        line["chain"] = serde_json::json!(ctx.chain);
    }
    audit_line(env, &line);
    if let Some(rule) = boundary_rule(decision, resp) {
        crate::boundary::record(
            env,
            &ctx.id,
            crate::boundary::HOST,
            &format!("{}: {detail}", req.cap),
            &rule,
        );
    }
}

/// The rule for the boundary log when the broker refused or denied a
/// request. None for allowed requests, failures and timeouts. A denial note
/// of the user stays out: the log keeps paddock's own words only.
pub(in crate::broker) fn boundary_rule(decision: &str, resp: &Response) -> Option<String> {
    let refused = ["invalid", "disabled", "refused", "denied"]
        .iter()
        .any(|d| decision.starts_with(d));
    if !refused {
        return None;
    }
    if decision.starts_with("denied") {
        return Some(decision.to_string());
    }
    let msg: String = resp.message.chars().take(300).collect();
    Some(format!("{decision}: {msg}"))
}

/// Append one line to the audit log.
pub fn audit_line(env: &HostEnv, line: &serde_json::Value) {
    let f = audit_file(env);
    match crate::privfs::append(&f) {
        Ok(mut file) => {
            if let Err(e) = writeln!(file, "{line}") {
                log::warn!("cannot write the audit log {}: {e}", f.display());
            }
        }
        Err(e) => log::warn!("cannot open the audit log {}: {e}", f.display()),
    }
}
