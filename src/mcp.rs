//! MCP servers from the harness configuration.
//!
//! The sandbox reuses the host's MCP configuration and MCP OAuth tokens (see
//! the profile `auth` and `json_import` lists). This module makes the servers
//! runnable: a stdio server whose command lives in a tool install directory
//! (system, mise, node_modules, uv tools, ~/.local/bin) gets that directory
//! mounted read-only. A server that needs any other host path, or a host
//! loopback port, is reported and stays off until the configuration allows it.

use crate::env::HostEnv;
use crate::manifest::Manifest;
use crate::profile::{install_root, is_system_path, runtime_closure, Profile};
use crate::proxy::{is_loopback, parse_url};
use crate::spec::{Mount, SandboxSpec};
use crate::state::{expand_path, json_get};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Server {
    pub name: String,
    pub source: String,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub url: Option<String>,
}

#[derive(Debug, Clone)]
pub enum Status {
    /// Runs in the sandbox. The mounts and PATH entries are added.
    Ready {
        mounts: Vec<PathBuf>,
        path: Vec<PathBuf>,
        tools: Vec<(String, PathBuf)>,
        port: Option<u16>,
    },
    /// A remote server, reached through the normal network path.
    Remote,
    /// Needs host paths outside tool installs. Allow it in `[mcp] allow`.
    NeedsAllow(Vec<PathBuf>),
    /// Needs a host loopback port. Allow it in `[network] host_ports`.
    NeedsPort(u16),
    /// The command is not on the host.
    NotFound(String),
}

fn to_json(v: toml::Value) -> serde_json::Value {
    serde_json::to_value(v).unwrap_or(serde_json::Value::Null)
}

pub fn servers(env: &HostEnv, profile: &Profile, project: &Path) -> Vec<Server> {
    let mut out: Vec<Server> = Vec::new();
    for src in &profile.mcp {
        let file = if src.in_project {
            project.join(&src.file)
        } else {
            env.home.join(&src.file)
        };
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        let doc: serde_json::Value = if src.format == "toml" {
            match toml::from_str::<toml::Value>(&text) {
                Ok(v) => to_json(v),
                Err(_) => continue,
            }
        } else {
            match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(_) => continue,
            }
        };
        let path = expand_path(&src.path, project);
        let Some(table) = json_get(&doc, &path).and_then(|v| v.as_object()) else {
            continue;
        };
        for (name, def) in table {
            // Command as a string (claude, codex, gemini) or as an array (opencode).
            let (command, mut args) = match def.get("command") {
                Some(serde_json::Value::String(c)) => (Some(c.clone()), vec![]),
                Some(serde_json::Value::Array(a)) => {
                    let parts: Vec<String> = a
                        .iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect();
                    (
                        parts.first().cloned(),
                        parts.iter().skip(1).cloned().collect(),
                    )
                }
                _ => (None, vec![]),
            };
            if let Some(a) = def.get("args").and_then(|a| a.as_array()) {
                args.extend(a.iter().filter_map(|x| x.as_str().map(str::to_string)));
            }
            let url = ["url", "httpUrl", "serverUrl"]
                .iter()
                .find_map(|k| def.get(*k).and_then(|u| u.as_str()).map(str::to_string));
            out.retain(|s| s.name != *name);
            out.push(Server {
                name: name.clone(),
                source: file.display().to_string(),
                command,
                args,
                url,
            });
        }
    }
    out
}

/// A path inside a directory that holds installed tools, not user data.
pub fn is_tool_install(env: &HostEnv, p: &Path) -> bool {
    if is_system_path(p) {
        return true;
    }
    let s = p.to_string_lossy();
    let markers = [
        "/mise/installs/",
        "/.asdf/installs/",
        "/node_modules/",
        "/uv/tools/",
        "/uv/python/",
        "/pipx/venvs/",
        "/.nix-profile/",
        "/.volta/tools/",
        "/.rustup/toolchains/",
    ];
    if markers.iter().any(|m| s.contains(m)) {
        return true;
    }
    let bins = [
        ".local/bin",
        ".cargo/bin",
        "go/bin",
        ".bun/bin",
        ".deno/bin",
    ];
    bins.iter().any(|b| p.parent() == Some(&env.home.join(b)))
}

fn host_paths_in_args(env: &HostEnv, args: &[String], project: &Path) -> Vec<PathBuf> {
    let mut v = Vec::new();
    for a in args {
        // Flags in the form --opt=/some/path count as well.
        let raw = a.rsplit_once('=').map(|(_, r)| r).unwrap_or(a);
        if !(raw.starts_with('/') || raw.starts_with("~/")) {
            continue;
        }
        let p = env.expand(raw, project);
        if p.exists() && !p.starts_with(project) && !is_tool_install(env, &p) {
            v.push(p);
        }
    }
    v
}

pub fn assess(env: &HostEnv, s: &Server, m: &Manifest, project: &Path) -> Status {
    if let Some(url) = &s.url {
        if let Some((_, host, port)) = parse_url(url) {
            if is_loopback(&host) {
                if m.network.host_ports.contains(&port) {
                    return Status::Ready {
                        mounts: vec![],
                        path: vec![],
                        tools: vec![],
                        port: Some(port),
                    };
                }
                return Status::NeedsPort(port);
            }
        }
        return Status::Remote;
    }
    let Some(cmd) = &s.command else {
        return Status::NotFound("no command".into());
    };
    let raw = if cmd.contains('/') || cmd.starts_with('~') {
        let p = env.expand(cmd, project);
        if !p.exists() {
            return Status::NotFound(cmd.clone());
        }
        p
    } else {
        match env.which(cmd) {
            Some(p) => p,
            None => return Status::NotFound(cmd.clone()),
        }
    };
    let canonical = std::fs::canonicalize(&raw).unwrap_or(raw.clone());
    let allowed = m.mcp.allow.contains(&s.name);
    let mut outside = host_paths_in_args(env, &s.args, project);
    for p in [&raw, &canonical] {
        if !p.starts_with(project) && !is_tool_install(env, p) {
            outside.push(p.clone());
        }
    }
    outside.sort();
    outside.dedup();
    if !outside.is_empty() && !allowed {
        return Status::NeedsAllow(outside);
    }
    let (mut mounts, mut path) = runtime_closure(env, &canonical).unwrap_or_default();
    let mut tools = Vec::new();
    let root = install_root(&canonical);
    if root == canonical {
        // A single file in a shared bin directory: expose only that file.
        let name = Path::new(cmd)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        tools.push((name, canonical.clone()));
    } else if let Some(dir) = canonical.parent() {
        path.push(dir.to_path_buf());
    }
    if let Some(dir) = raw.parent() {
        if raw != canonical
            && !is_system_path(&raw)
            && is_tool_install(env, &raw)
            && install_root(&raw) != raw
        {
            path.push(dir.to_path_buf());
        }
    }
    if allowed {
        mounts.extend(outside);
    }
    mounts.retain(|p| !p.starts_with(project));
    Status::Ready {
        mounts,
        path,
        tools,
        port: None,
    }
}

/// Add what each runnable server needs, and warn about the others.
pub fn apply(
    env: &HostEnv,
    profile: &Profile,
    m: &Manifest,
    project: &Path,
    spec: &mut SandboxSpec,
) {
    let mut blocked = Vec::new();
    for s in servers(env, profile, project) {
        let origin = format!("mcp {}", s.name);
        match assess(env, &s, m, project) {
            Status::Ready {
                mounts,
                path,
                tools,
                port,
            } => {
                for p in mounts {
                    if !spec.mounts.iter().any(|x| x.dst == p) && !is_system_path(&p) {
                        spec.add_mount(Mount::same(&p, false, &origin));
                    }
                }
                for d in path {
                    if !is_system_path(&d) {
                        spec.add_path(d);
                    }
                }
                for (name, src) in tools {
                    if !is_system_path(&src) {
                        spec.add_tool(&name, src, &origin);
                    }
                }
                if let Some(p) = port {
                    spec.add_host_port(p, &format!("{origin}: manifest network.host_ports"));
                }
            }
            Status::Remote => {}
            Status::NeedsAllow(_) => blocked.push(format!("{} (needs [mcp] allow)", s.name)),
            Status::NeedsPort(p) => blocked.push(format!(
                "{} (needs host port {p} in [network] host_ports)",
                s.name
            )),
            Status::NotFound(c) => blocked.push(format!("{} (command {c} not found)", s.name)),
        }
    }
    if !blocked.is_empty() {
        spec.warnings.push(format!(
            "these MCP servers will not start in the sandbox: {}. Run `paddock mcp {}` for details",
            blocked.join(", "),
            profile.name
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::os::unix::fs::PermissionsExt;

    fn env(home: &Path, path: &[PathBuf]) -> HostEnv {
        let mut vars = BTreeMap::new();
        vars.insert(
            "PATH".into(),
            std::env::join_paths(path)
                .unwrap()
                .to_string_lossy()
                .into_owned(),
        );
        HostEnv::from_parts(home.to_path_buf(), "tester".into(), 1, 1, vars)
    }

    fn exe(p: &Path, text: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn server(cmd: &str, args: &[&str]) -> Server {
        Server {
            name: "s".into(),
            source: "test".into(),
            command: Some(cmd.into()),
            args: args.iter().map(|a| a.to_string()).collect(),
            url: None,
        }
    }

    #[test]
    fn tool_install_rules() {
        let home = Path::new("/h");
        let e = env(home, &[]);
        for p in [
            "/usr/bin/node",
            "/h/.local/share/mise/installs/node/26/bin/npx",
            "/h/.asdf/installs/python/3/bin/python",
            "/h/x/node_modules/pkg/cli.js",
            "/h/.local/share/uv/tools/serena/bin/serena",
            "/h/.local/share/uv/python/cpython-3.13/bin/python3",
            "/h/.local/share/pipx/venvs/black/bin/black",
            "/h/.local/bin/tool",
            "/h/.cargo/bin/tool",
        ] {
            assert!(is_tool_install(&e, Path::new(p)), "{p}");
        }
        for p in [
            "/h/.hermes/venv/bin/python",
            "/h/Migrations/x/bin/serena",
            "/h/.local/bin/sub/tool",
            "/h/notes/mcp.py",
        ] {
            assert!(!is_tool_install(&e, Path::new(p)), "{p}");
        }
    }

    #[test]
    fn assess_classifies_servers() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let project = home.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let tools = home.join(".local/share/mise/installs/tool/1/bin");
        exe(&tools.join("good"), "#!/bin/sh\n");
        exe(&home.join("private/server"), "#!/bin/sh\n");
        std::fs::write(home.join("private/data.db"), "").unwrap();
        let e = env(&home, std::slice::from_ref(&tools));
        let mut m = Manifest::default();

        assert!(matches!(
            assess(&e, &server("good", &[]), &m, &project),
            Status::Ready { .. }
        ));
        assert!(matches!(
            assess(&e, &server("missing-cmd", &[]), &m, &project),
            Status::NotFound(_)
        ));
        let private = home.join("private/server").display().to_string();
        assert!(matches!(
            assess(&e, &server(&private, &[]), &m, &project),
            Status::NeedsAllow(_)
        ));
        let db = format!("--db={}", home.join("private/data.db").display());
        assert!(matches!(
            assess(&e, &server("good", &[&db]), &m, &project),
            Status::NeedsAllow(_)
        ));
        m.mcp.allow.push("s".into());
        match assess(&e, &server("good", &[&db]), &m, &project) {
            Status::Ready { mounts, .. } => assert!(mounts.contains(&home.join("private/data.db"))),
            other => panic!("{other:?}"),
        }

        let mut remote = server("x", &[]);
        remote.command = None;
        remote.url = Some("https://mcp.example.com/mcp".into());
        assert!(matches!(assess(&e, &remote, &m, &project), Status::Remote));
        remote.url = Some("http://127.0.0.1:8765/mcp".into());
        assert!(matches!(
            assess(&e, &remote, &m, &project),
            Status::NeedsPort(8765)
        ));
        m.network.host_ports.push(8765);
        assert!(matches!(
            assess(&e, &remote, &m, &project),
            Status::Ready {
                port: Some(8765),
                ..
            }
        ));
    }
}
