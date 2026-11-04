use anyhow::{bail, Result};
use paddock::build;
use paddock::env::HostEnv;
use paddock::manifest::Manifest;
use paddock::profile::HarnessFactory;
use paddock::state;
use std::path::PathBuf;

pub(crate) fn list(env: &HostEnv, json: bool) -> Result<i32> {
    if json {
        let f = HarnessFactory::new(env)?;
        let items: Vec<serde_json::Value> = f
                .names()
                .into_iter()
                .map(|name| {
                    let desc = f.profile(&name).map(|p| p.description.clone()).unwrap_or_default();
                    let app = f.profile(&name).is_some_and(|p| p.app.is_some());
                    let exe = f.create(&name).ok().map(|h| h.exe.display().to_string());
                    serde_json::json!({ "name": name, "description": desc, "installed": exe.is_some(), "exe": exe, "app": app })
                })
                .collect();
        println!(
            "{}",
            serde_json::json!({ "api_version": "paddock.list/v1", "harnesses": items })
        );
        Ok(0)
    } else {
        let f = HarnessFactory::new(env)?;
        for name in f.names() {
            let desc = f
                .profile(&name)
                .map(|p| p.description.clone())
                .unwrap_or_default();
            match f.create(&name) {
                Ok(h) => println!("{name:<10} {desc:<24} {}", h.exe.display()),
                Err(_) => println!("{name:<10} {desc:<24} (not installed)"),
            }
        }
        Ok(0)
    }
}

pub(crate) fn recent(env: &HostEnv, json: bool) -> Result<i32> {
    let list = state::recent(env);
    if json {
        println!(
            "{}",
            serde_json::json!({ "api_version": "paddock.recent/v1", "projects": list })
        );
    } else {
        for r in list {
            println!("{:<10} {}", r.harness, r.project.display());
        }
    }
    Ok(0)
}

pub(crate) fn mcp(env: &HostEnv, harness: String, path: Option<PathBuf>) -> Result<i32> {
    use paddock::mcp::{assess, servers, Status};
    let f = HarnessFactory::new(env)?;
    let Some(p) = f.profile(&harness) else {
        bail!("unknown harness '{harness}'")
    };
    let project = build::resolve_project(path.as_deref())?;
    let m = Manifest::load(env, &project)?.merged;
    let list = servers(env, p, &project);
    if list.is_empty() {
        println!("No MCP servers found in the {harness} configuration.");
    }
    for s in list {
        let status = match assess(env, &s, &m, &project) {
                    Status::Ready { port: Some(p), .. } => format!("ready (host port {p})"),
                    Status::Ready { mounts, .. } if mounts.is_empty() => "ready".to_string(),
                    Status::Ready { mounts, .. } => format!(
                        "ready, mounts read-only: {}",
                        mounts.iter().map(|m| m.display().to_string()).collect::<Vec<_>>().join(", ")
                    ),
                    Status::Remote => "remote, uses the network".to_string(),
                    Status::NeedsAllow(paths) => format!(
                        "off: needs host paths that {} cannot grant. With \"{}\" in [mcp] allow, paddock mounts these read-only: {}",
                        if s.trusted { "this configuration" } else { "a project file" },
                        s.name,
                        paths.iter().map(|m| m.display().to_string()).collect::<Vec<_>>().join(", "),
                    ),
                    Status::NeedsPort(port) => format!("off: needs host port {port}. Add it to [network] host_ports"),
                    Status::NotFound(c) => format!("off: command {c} is not installed on the host"),
                };
        let from = if s.trusted { "" } else { " (project file)" };
        println!(
            "{:<20} {status}\n{:<20} from {}{from}",
            s.name, "", s.source
        );
    }
    println!("\nOutside the shared tool installs, paddock hides .env files and keys in every mounted folder.");
    println!(
        "\nMCP OAuth tokens and server settings come from your host login (auth mode shared)."
    );
    println!(
        "Put [mcp] and [network] settings in {} or in the project paddock.toml.",
        env.paddock_config().join("config.toml").display()
    );
    Ok(0)
}

/// `paddock pick`: a project chooser for launcher entries, which start in a
/// terminal without a project directory.
pub(crate) fn pick(env: &HostEnv, harness: &str) -> Result<i32> {
    use std::io::{BufRead, Write};
    let f = HarnessFactory::new(env)?;
    if f.profile(harness).is_none() {
        bail!("unknown harness '{harness}'");
    }
    let mut recent = state::recent(env);
    // Projects last used with this harness first, then the others.
    recent.sort_by_key(|r| (r.harness != harness, std::cmp::Reverse(r.last)));
    recent.truncate(9);
    let home = env.home.display().to_string();
    let short = |p: &std::path::Path| {
        let s = p.display().to_string();
        match s.strip_prefix(&home) {
            Some(rest) => format!("~{rest}"),
            None => s,
        }
    };
    println!("Start {harness} in a paddock sandbox. Which project?\n");
    for (i, r) in recent.iter().enumerate() {
        let name = r
            .project
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        println!(
            "  {}  {:<20} {:<40} last: {}",
            i + 1,
            name,
            short(&r.project),
            r.harness
        );
    }
    if recent.is_empty() {
        println!("  No recent projects yet.");
    }
    let stdin = std::io::stdin();
    let path = loop {
        print!(
            "\n{}Type a number or a path (q to quit): ",
            if recent.is_empty() {
                ""
            } else {
                "Enter for 1. "
            }
        );
        std::io::stdout().flush().ok();
        let mut line = String::new();
        if stdin.lock().read_line(&mut line)? == 0 {
            return Ok(0);
        }
        let line = line.trim();
        if line == "q" {
            return Ok(0);
        }
        if line.is_empty() && !recent.is_empty() {
            break recent[0].project.clone();
        }
        if let Ok(n) = line.parse::<usize>() {
            if (1..=recent.len()).contains(&n) {
                break recent[n - 1].project.clone();
            }
            println!("There is no project {n}.");
            continue;
        }
        let p = env.expand(line, &env.home);
        if p.is_dir() {
            break p;
        }
        println!("{} is not a folder.", p.display());
    };
    println!();
    let me = std::env::current_exe()?;
    let status = std::process::Command::new(me)
        .arg(harness)
        .arg(&path)
        .status()?;
    let code = status.code().unwrap_or(1);
    if code != 0 {
        // Launcher terminals close when the command ends: keep the error visible.
        print!("\npaddock exited with {code}. Press Enter to close.");
        std::io::stdout().flush().ok();
        let mut s = String::new();
        let _ = stdin.lock().read_line(&mut s);
    }
    Ok(code)
}

pub(crate) fn pick_cmd(env: &HostEnv, harness: String) -> Result<i32> {
    pick(env, &harness)
}
