//! The sandbox side of `git-push` and `forge`: the `paddock::` git remote
//! helper, `paddock-host git push` and `paddock-host gh`.

use super::{send, EXIT_NO_HOST, EXIT_USAGE};
use crate::broker::{Request, Status, PROTOCOL};
use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::process::Command;

/// The remote helper writes the broker status here, so `paddock-host git
/// push` can exit with the documented code instead of git's own.
const STATUS_VAR: &str = "PADDOCK_PUSH_STATUS";

fn git_out(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `paddock-host git push ARGS`: plain `git push`, which the sandbox
/// configuration sends through the remote helper.
pub fn git_main(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("push") => {}
        _ => {
            eprintln!("paddock-host: only `paddock-host git push [REMOTE] [REFSPEC...]` exists. Plain `git push` works the same way.");
            return EXIT_USAGE;
        }
    }
    let status_file = std::env::temp_dir().join(format!("paddock-push-{}", std::process::id()));
    let _ = std::fs::remove_file(&status_file);
    let code = Command::new("git")
        .args(args)
        .env(STATUS_VAR, &status_file)
        .status()
        .map(|s| s.code().unwrap_or(1))
        .unwrap_or(EXIT_NO_HOST);
    let host = std::fs::read_to_string(&status_file)
        .ok()
        .and_then(|t| t.trim().parse::<i32>().ok());
    let _ = std::fs::remove_file(&status_file);
    match host {
        Some(c) if c != 0 => c,
        _ => code,
    }
}

/// `git-remote-paddock REMOTE URL`, run by git for `paddock::URL` push URLs.
/// It speaks the remote helper protocol on stdin and stdout. See
/// gitremote-helpers(7).
pub fn remote_helper(args: &[String]) -> i32 {
    let (Some(remote), Some(url)) = (args.first(), args.get(1)) else {
        eprintln!("git-remote-paddock: git runs this with a remote and a URL");
        return EXIT_USAGE;
    };
    // Without a named remote, git passes the URL twice.
    let named = remote != url && !remote.contains([':', '/']);
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    let mut batch: Vec<String> = Vec::new();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { return 1 };
        let reply = if line == "capabilities" {
            "push\noption\n\n".to_string()
        } else if let Some(opt) = line.strip_prefix("option ") {
            match opt.split(' ').next() {
                Some("verbosity" | "progress") => "ok\n".into(),
                _ => "unsupported\n".into(),
            }
        } else if line == "list" || line == "list for-push" {
            list(remote, named)
        } else if let Some(spec) = line.strip_prefix("push ") {
            batch.push(spec.to_string());
            continue;
        } else if line.is_empty() {
            if batch.is_empty() {
                return 0;
            }
            let r = push(remote, url, &std::mem::take(&mut batch));
            r + "\n"
        } else {
            eprintln!("git-remote-paddock: unknown command {line:?}");
            return 1;
        };
        if out
            .write_all(reply.as_bytes())
            .and_then(|_| out.flush())
            .is_err()
        {
            return 1;
        }
    }
    0
}

/// The remote's refs as this repository last saw them. git compares
/// against them to tell a fast-forward from a force push.
fn list(remote: &str, named: bool) -> String {
    let mut t = String::new();
    if named {
        let prefix = format!("refs/remotes/{remote}/");
        if let Some(refs) = git_out(&["for-each-ref", "--format=%(objectname) %(refname)", &prefix])
        {
            for l in refs.lines() {
                if let Some((sha, name)) = l.split_once(' ') {
                    if let Some(branch) = name.strip_prefix(&prefix) {
                        if branch != "HEAD" {
                            t.push_str(&format!("{sha} refs/heads/{branch}\n"));
                        }
                    }
                }
            }
        }
    }
    t.push('\n');
    t
}

fn push(remote: &str, url: &str, specs: &[String]) -> String {
    let mut refs = Vec::new();
    for spec in specs {
        let (force, spec) = match spec.strip_prefix('+') {
            Some(s) => (true, s),
            None => (false, spec.as_str()),
        };
        let (src, dst) = spec.split_once(':').unwrap_or((spec, spec));
        let sha = if src.is_empty() {
            String::new()
        } else {
            match git_out(&["rev-parse", "--verify", "--quiet", src]) {
                Some(s) => s,
                None => return format!("error {dst} cannot resolve {src}\n"),
            }
        };
        refs.push(serde_json::json!({ "src": src, "sha": sha, "dst": dst, "force": force }));
    }
    let dsts: Vec<String> = refs
        .iter()
        .map(|r| r["dst"].as_str().unwrap_or("").to_string())
        .collect();
    let mut args = BTreeMap::new();
    args.insert("remote".to_string(), serde_json::json!(remote));
    args.insert("url".to_string(), serde_json::json!(url));
    args.insert("refs".to_string(), serde_json::json!(refs));
    let req = Request {
        v: PROTOCOL,
        cap: "git-push".into(),
        args,
        reason: std::env::var("PADDOCK_PUSH_REASON").unwrap_or_default(),
    };
    eprintln!("paddock: asking the host to push to {remote}");
    let resp = send(&req);
    let code = match &resp {
        Ok(r) => r.status.exit_code(),
        Err(_) => EXIT_NO_HOST,
    };
    if let Ok(f) = std::env::var(STATUS_VAR) {
        let _ = std::fs::write(f, code.to_string());
    }
    let mut t = String::new();
    match resp {
        Ok(r) if r.status == Status::Done => {
            eprint!("{}", r.result["stderr"].as_str().unwrap_or(""));
            let results = r.result["refs"].as_array().cloned().unwrap_or_default();
            for dst in &dsts {
                let one = results.iter().find(|x| x["dst"] == dst.as_str());
                match one {
                    Some(x) if x["ok"] == true => t.push_str(&format!("ok {dst}\n")),
                    Some(x) => t.push_str(&format!(
                        "error {dst} {}\n",
                        one_line(x["message"].as_str().unwrap_or("rejected"))
                    )),
                    None => t.push_str(&format!("error {dst} the host push failed\n")),
                }
            }
        }
        Ok(r) => {
            let why = match r.status {
                Status::Denied => "denied on the host",
                Status::Disabled => "git-push is disabled on the host",
                Status::Timeout => "nobody answered on the host",
                Status::Invalid => "rejected by the host",
                _ => "failed on the host",
            };
            eprintln!("paddock-host: git-push {why}: {}", r.message);
            for dst in &dsts {
                t.push_str(&format!("error {dst} {why}\n"));
            }
        }
        Err(e) => {
            eprintln!("paddock-host: {e}");
            for dst in &dsts {
                t.push_str(&format!("error {dst} no connection to the host\n"));
            }
        }
    }
    t
}

fn one_line(s: &str) -> String {
    s.lines().next().unwrap_or("").chars().take(200).collect()
}

/// `paddock-host gh ARGS`: one allowed gh command, run on the host.
/// `--body-file` is read here, in the sandbox, and sent as `--body`.
pub fn gh_main(args: &[String]) -> i32 {
    forge_main("gh", args)
}

/// `paddock-host TOOL ARGS` for a forge tool (gh, tea, glab). For gh,
/// `--body-file` is read here, in the sandbox, and sent as `--body`.
pub fn forge_main(tool: &str, args: &[String]) -> i32 {
    let mut argv = Vec::new();
    let mut reason = String::new();
    let mut json = false;
    let mut timeout = None;
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        i += 1;
        let (flag, inline) = match a.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v.to_string())),
            _ => (a.as_str(), None),
        };
        let mut value = || -> Option<String> {
            inline.clone().or_else(|| {
                let v = args.get(i).cloned();
                i += 1;
                v
            })
        };
        match flag {
            "--paddock-reason" => reason = value().unwrap_or_default(),
            "--paddock-json" => json = true,
            "--paddock-timeout" => timeout = value().and_then(|v| v.parse::<u64>().ok()),
            "-F" | "--body-file" if tool == "gh" => {
                let Some(path) = value() else {
                    eprintln!("paddock-host: {flag} needs a file");
                    return EXIT_USAGE;
                };
                let text = if path == "-" {
                    let mut s = String::new();
                    let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut s);
                    Ok(s)
                } else {
                    std::fs::read_to_string(&path)
                };
                match text {
                    Ok(t) => argv.extend(["--body".to_string(), t]),
                    Err(e) => {
                        eprintln!("paddock-host: cannot read {path}: {e}");
                        return EXIT_USAGE;
                    }
                }
            }
            _ => argv.push(a.clone()),
        }
    }
    if argv.is_empty() {
        println!("{}", super::cap_help("forge").unwrap_or_default());
        return 0;
    }
    let mut a = BTreeMap::new();
    a.insert("tool".to_string(), serde_json::json!(tool));
    a.insert("argv".to_string(), serde_json::json!(argv));
    if let Some(b) = git_out(&["symbolic-ref", "--short", "-q", "HEAD"]) {
        a.insert("branch".to_string(), serde_json::json!(b));
    }
    if let Some(t) = timeout {
        a.insert("timeout".to_string(), serde_json::json!(t));
    }
    let req = Request {
        v: PROTOCOL,
        cap: "forge".into(),
        args: a,
        reason,
    };
    let resp = match send(&req) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("paddock-host: {e}\nThis command works only inside a paddock sandbox.");
            return EXIT_NO_HOST;
        }
    };
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&resp).unwrap_or_default()
        );
    } else {
        super::print_human(&req, &resp);
    }
    if resp.status == Status::Done {
        return resp.result["exit_code"].as_i64().unwrap_or(0) as i32;
    }
    resp.status.exit_code()
}
