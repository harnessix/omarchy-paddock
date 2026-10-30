//! MCP servers from the harness configuration.
//!
//! The sandbox reuses the host's MCP configuration and MCP OAuth tokens (see
//! the profile `auth` and `json_import` lists). This module makes the servers
//! runnable: a stdio server whose command lives in a tool install directory
//! gets the smallest part of it that runs mounted read-only. A server that
//! needs any other host path, or a host loopback port, is reported and stays
//! off until the configuration allows it.
//!
//! Who names the command matters. A configuration file in the host home is
//! the user's own. A file in the project (`.mcp.json`, `opencode.json`,
//! `.gemini/settings.json`) is written by whoever controls the project,
//! including the sandbox itself. So a project file gets implicit trust only
//! for the project, system directories and the shared tool installs
//! (`is_shared_tool`). Anything else it names, for example a package in
//! another repository's `node_modules`, needs `[mcp] allow`, which a project
//! `paddock.toml` can only set with the user's approval.

use crate::env::HostEnv;
use crate::manifest::Manifest;
use crate::profile::{install_root, is_system_path, runtime_closure, Profile, HOME_BIN_DIRS};
use crate::proxy::{is_loopback, parse_url};
use crate::spec::{Mount, SandboxSpec};
use crate::state::{expand_path, json_get};
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Server {
    pub name: String,
    pub source: String,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub url: Option<String>,
    /// The definition comes from a file in the host home, not from the
    /// project, which the sandbox can write.
    pub trusted: bool,
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
    /// Needs host paths that its configuration file cannot grant. Allow it in
    /// `[mcp] allow`. The list holds every host path that would then be
    /// mounted read-only, not only the ones that need the approval.
    NeedsAllow(Vec<PathBuf>),
    /// Needs a host loopback port. Allow it in `[network] host_ports`.
    NeedsPort(u16),
    /// The command is not on the host.
    NotFound(String),
}

fn to_json(v: toml::Value) -> serde_json::Value {
    serde_json::to_value(v).unwrap_or(serde_json::Value::Null)
}

/// Harness configuration files can hold long histories (`~/.claude.json`).
const MAX_CONFIG: u64 = 64 << 20;

pub fn servers(env: &HostEnv, profile: &Profile, project: &Path) -> Vec<Server> {
    let mut out: Vec<Server> = Vec::new();
    for src in &profile.mcp {
        // A project file is sandbox-writable: no symlink is followed. Neither
        // read opens a FIFO or takes a huge file.
        let file = if src.in_project {
            project.join(&src.file)
        } else {
            env.home.join(&src.file)
        };
        let bytes = if src.in_project {
            crate::safefs::Root::open(project)
                .and_then(|r| r.read(Path::new(&src.file), MAX_CONFIG))
        } else {
            crate::safefs::read_path(&file, true, MAX_CONFIG)
        };
        let Ok(Some(bytes)) = bytes else {
            continue;
        };
        let Ok(text) = String::from_utf8(bytes) else {
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
                trusted: !src.in_project,
            });
        }
    }
    out
}

/// A path with `.` or `..` in it says nothing about where it leads.
fn plain(p: &Path) -> bool {
    p.is_absolute()
        && !p
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
}

/// A path inside a tool install that paddock shares with every sandbox:
/// system directories, the mise, asdf, uv, pipx, nix, volta and rustup
/// installs in the host home, and single files in the home bin directories.
/// Each one is anchored at its real location, so a folder with the same name
/// in another repository does not count. Any configuration may name these.
pub fn is_shared_tool(env: &HostEnv, p: &Path) -> bool {
    if !plain(p) {
        return false;
    }
    if is_system_path(p) {
        return true;
    }
    let mut roots = vec![
        env.data_dir.join("mise/installs"),
        env.home.join(".asdf/installs"),
        env.data_dir.join("uv/tools"),
        env.data_dir.join("uv/python"),
        env.data_dir.join("pipx/venvs"),
        env.home.join(".local/pipx/venvs"),
        env.home.join(".nix-profile"),
        env.home.join(".volta/tools"),
        env.home.join(".rustup/toolchains"),
    ];
    if let Some(d) = env
        .var("MISE_DATA_DIR")
        .filter(|d| Path::new(d).is_absolute())
    {
        roots.push(Path::new(d).join("installs"));
    }
    if roots.iter().any(|r| p.starts_with(r) && p != r.as_path()) {
        return true;
    }
    HOME_BIN_DIRS
        .iter()
        .any(|b| p.parent() == Some(&env.home.join(b)))
}

/// A path that looks like an installed tool, not user data: a shared tool,
/// or any path with an install marker such as `/node_modules/`. Only a
/// command that the user's own configuration in the host home names gets
/// this wider rule.
pub fn is_tool_install(env: &HostEnv, p: &Path) -> bool {
    if !plain(p) {
        return false;
    }
    if is_shared_tool(env, p) {
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
    markers.iter().any(|m| s.contains(m))
}

/// Whether a host path may be mounted without `[mcp] allow`. `trusted` says
/// that the user's own configuration named the path directly. Everything a
/// project file names, and everything found by following a script header,
/// gets the narrow rule.
pub fn implicit(env: &HostEnv, project: &Path, p: &Path, trusted: bool) -> bool {
    if !plain(p) {
        return false;
    }
    p.starts_with(project) || is_shared_tool(env, p) || (trusted && is_tool_install(env, p))
}

/// Host paths in the arguments, resolved, outside the project.
fn host_paths_in_args(env: &HostEnv, args: &[String], project: &Path) -> Vec<PathBuf> {
    let mut v = Vec::new();
    for a in args {
        // Flags in the form --opt=/some/path count as well.
        let raw = a.rsplit_once('=').map(|(_, r)| r).unwrap_or(a);
        if !(raw.starts_with('/') || raw.starts_with("~/")) {
            continue;
        }
        let p = env.expand(raw, project);
        // The real path: a symlink or `..` must not hide where it leads.
        let Ok(real) = std::fs::canonicalize(&p) else {
            continue;
        };
        if !real.starts_with(project) {
            v.push(real);
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
    // The user's own configuration names the command, unless the name leads
    // through the project, where the sandbox chooses a symlink's target.
    let trusted = s.trusted && !raw.starts_with(project);
    let (closure, mut path) = runtime_closure(env, &canonical).unwrap_or_default();
    let exe_root = install_root(&canonical);
    let exe_trusted = trusted && is_tool_install(env, &canonical);
    // Every host path the server would get, and whether it may get it
    // without approval.
    let mut wanted: Vec<(PathBuf, bool)> = Vec::new();
    for p in closure {
        // Interpreters come from script headers, which whoever wrote the
        // script chose: only the narrow rule applies to them.
        let ok = implicit(env, project, &p, false) || (p == exe_root && exe_trusted);
        wanted.push((p, ok));
    }
    if raw != canonical && !raw.starts_with(project) && !implicit(env, project, &raw, trusted) {
        wanted.push((raw.clone(), false));
    }
    for p in host_paths_in_args(env, &s.args, project) {
        // A tool install in the arguments was never mounted for it.
        if !implicit(env, project, &p, s.trusted) {
            wanted.push((p, false));
        }
    }
    wanted.retain(|(p, _)| !p.starts_with(project));
    wanted.sort();
    wanted.dedup_by(|a, b| a.0 == b.0);
    if wanted.iter().any(|(_, ok)| !ok) && !allowed {
        return Status::NeedsAllow(
            wanted
                .into_iter()
                .map(|(p, _)| p)
                .filter(|p| !is_system_path(p))
                .collect(),
        );
    }
    let mounts: Vec<PathBuf> = wanted
        .into_iter()
        .map(|(p, _)| p)
        .filter(|p| !is_system_path(p))
        .collect();
    let mut tools = Vec::new();
    if exe_root == canonical {
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
            && implicit(env, project, &raw, trusted)
            && install_root(&raw) != raw
        {
            path.push(dir.to_path_buf());
        }
    }
    Status::Ready {
        mounts,
        path,
        tools,
        port: None,
    }
}

/// Project paths that `[secrets] reveal` keeps visible.
pub fn reveal_list(env: &HostEnv, m: &Manifest, project: &Path) -> Vec<PathBuf> {
    m.secrets
        .reveal
        .iter()
        .map(|r| env.expand(r, project))
        .collect()
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
    let reveal = reveal_list(env, m, project);
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
                        // A folder outside the shared tool installs can hold
                        // a `.env` or keys, as the project can.
                        if p.is_dir() && !is_shared_tool(env, &p) {
                            crate::detect::hide_secrets_in(
                                &p,
                                &reveal,
                                spec,
                                &format!("{origin}: credential-shaped file"),
                            );
                        }
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
    use crate::spec::MountKind;
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
            trusted: true,
        }
    }

    /// A server from a project file, such as `.mcp.json`.
    fn project_server(cmd: &str, args: &[&str]) -> Server {
        Server {
            trusted: false,
            ..server(cmd, args)
        }
    }

    #[test]
    fn trust_decision() {
        let home = Path::new("/home/demo");
        let e = env(home, &[]);
        let project = Path::new("/home/demo/proj");
        let narrow = |p: &str| implicit(&e, project, Path::new(p), false);
        let wide = |p: &str| implicit(&e, project, Path::new(p), true);
        // A project file may name the project, system paths and shared installs.
        for p in [
            "/home/demo/proj/node_modules/pkg/cli.js",
            "/home/demo/proj/server.py",
            "/usr/bin/node",
            "/opt/tool/bin/tool",
            "/home/demo/.local/share/mise/installs/node/26/bin/node",
            "/home/demo/.local/share/mise/installs/node/26/lib/node_modules/npm/bin/npx-cli.js",
            "/home/demo/.local/share/uv/tools/serena/bin/serena",
            "/home/demo/.local/bin/tool",
        ] {
            assert!(narrow(p), "{p}");
            assert!(wide(p), "{p}");
        }
        // Not another repository, not a look-alike folder, not the home itself.
        for p in [
            "/home/demo/other/node_modules/pkg/cli.js",
            "/home/demo/other/node_modules",
            "/home/demo/other/mise/installs/x/bin/x",
            "/home/demo/notes/mcp.py",
            "/home/demo/.local/share/mise/installs",
            "/home/demo/.local/share/mise/installs/../../../../other/x",
            "/home/demo/proj/../other/x",
            "/home/demo/.local/bin/sub/tool",
        ] {
            assert!(!narrow(p), "{p}");
        }
        // The user's own configuration keeps the wider install markers.
        assert!(wide("/home/demo/other/node_modules/pkg/cli.js"));
        assert!(!wide("/home/demo/notes/mcp.py"));
        assert!(!wide("/home/demo/other/node_modules/../.env"));
    }

    /// A sibling repository with a package in its node_modules and a `.env`.
    fn sibling(home: &Path) -> PathBuf {
        let repo = home.join("other");
        exe(&repo.join("node_modules/pkg/cli.js"), "#!/bin/sh\nexit 0\n");
        std::fs::write(repo.join(".env"), "TOKEN=secret\n").unwrap();
        std::fs::write(repo.join("node_modules/pkg/.env"), "TOKEN=secret\n").unwrap();
        repo
    }

    #[test]
    fn a_project_file_cannot_mount_another_repository() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let project = home.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let repo = sibling(&home);
        let e = env(&home, &[]);
        let mut m = Manifest::default();
        let cli = repo.join("node_modules/pkg/cli.js").display().to_string();

        // The report lists exactly what an approval would mount: the
        // node_modules folder, never the repository.
        match assess(&e, &project_server(&cli, &[]), &m, &project) {
            Status::NeedsAllow(paths) => assert_eq!(paths, vec![repo.join("node_modules")]),
            other => panic!("{other:?}"),
        }
        // A symlink in the project changes nothing.
        std::os::unix::fs::symlink(&cli, project.join("link.js")).unwrap();
        assert!(matches!(
            assess(&e, &project_server("./link.js", &[]), &m, &project),
            Status::NeedsAllow(_)
        ));
        // Nor does a host config entry that goes through the project.
        assert!(matches!(
            assess(&e, &server("./link.js", &[]), &m, &project),
            Status::NeedsAllow(_)
        ));
        // A path argument into the repository needs the approval as well.
        let env_file = repo.join(".env").display().to_string();
        assert!(matches!(
            assess(&e, &project_server("/bin/sh", &[&env_file]), &m, &project),
            Status::NeedsAllow(_)
        ));

        m.mcp.allow.push("s".into());
        match assess(&e, &project_server(&cli, &[]), &m, &project) {
            Status::Ready { mounts, .. } => assert_eq!(mounts, vec![repo.join("node_modules")]),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_host_config_gets_the_minimal_closure() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let project = home.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let repo = sibling(&home);
        let e = env(&home, &[]);
        let m = Manifest::default();
        let cli = repo.join("node_modules/pkg/cli.js").display().to_string();
        match assess(&e, &server(&cli, &[]), &m, &project) {
            Status::Ready { mounts, .. } => assert_eq!(mounts, vec![repo.join("node_modules")]),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_script_header_cannot_name_host_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let project = home.join("proj");
        let venv = home.join("secret/venv");
        exe(&venv.join("bin/python"), "\x7fELF");
        let header = format!("#!{}\n", venv.join("bin/python").display());
        exe(&project.join("server.py"), &header);
        let e = env(&home, &[]);
        let m = Manifest::default();
        // Even the user's own configuration: the sandbox writes the script.
        for s in [
            server("./server.py", &[]),
            project_server("./server.py", &[]),
        ] {
            match assess(&e, &s, &m, &project) {
                Status::NeedsAllow(paths) => assert_eq!(paths, vec![venv.clone()]),
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn extra_mounts_hide_secrets() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let project = home.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let repo = sibling(&home);
        let cli = repo.join("node_modules/pkg/cli.js");
        std::fs::write(
            project.join(".mcp.json"),
            format!(
                "{{\"mcpServers\":{{\"s\":{{\"command\":\"{}\"}}}}}}",
                cli.display()
            ),
        )
        .unwrap();
        let profile: Profile = toml::from_str(
            "name = \"t\"\n[[mcp]]\nfile = \".mcp.json\"\nin_project = true\npath = [\"mcpServers\"]\n",
        )
        .unwrap();
        let e = env(&home, &[]);
        let mut spec = SandboxSpec::new(
            "t".into(),
            "t",
            &project,
            home.join("sandbox"),
            &home,
            1000,
            1000,
            "t",
        );
        let mut m = Manifest::default();
        apply(&e, &profile, &m, &project, &mut spec);
        assert!(
            !spec.mounts.iter().any(|x| x.dst.starts_with(&repo)),
            "{:?}",
            spec.mounts
        );
        assert!(spec
            .warnings
            .iter()
            .any(|w| w.contains("needs [mcp] allow")));

        m.mcp.allow.push("s".into());
        let mut spec2 = spec.clone();
        spec2.mounts.clear();
        apply(&e, &profile, &m, &project, &mut spec2);
        let kind = |p: &Path| spec2.mounts.iter().find(|x| x.dst == p).map(|x| x.kind);
        assert_eq!(kind(&repo.join("node_modules")), Some(MountKind::Ro));
        assert_eq!(
            kind(&repo.join("node_modules/pkg/.env")),
            Some(MountKind::MaskFile)
        );
        assert_eq!(kind(&repo), None);
        assert_eq!(kind(&repo.join(".env")), None);
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
