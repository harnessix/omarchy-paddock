//! Which mise tools of a project may go into the shared installs.
//!
//! The shared installs are executed by every project's sandbox, so nothing a
//! project controls may write them. paddock reads the project's mise files on
//! the host and keeps only plain `tool@version` pairs whose backend downloads
//! a verified artifact (mise's built-in `core` backends and `aqua`). A clean
//! sandbox without the project, its config or the sandbox home installs that
//! list (see `SandboxSpec::mise_shared_spec`). Everything else, and every
//! hook, `[env]` directive and task, only runs in the project's own sandbox,
//! where the shared installs are read-only and other tools install into the
//! private sandbox home.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// The shared cache folder that holds the shared installs.
pub const SHARED_CACHE: &str = "mise-tools";

/// Backends that run project-chosen code or install project-chosen content
/// under a name another project can request too. The shared step disables
/// them, so a registry name never falls through to one of them.
pub const DISABLED_BACKENDS: &[&str] = &[
    "asdf", "cargo", "conda", "dotnet", "forgejo", "gem", "github", "gitlab", "go", "http", "npm",
    "packslip", "pipx", "pypi", "s3", "spinel", "spm", "ubi", "vfox",
];

/// Core tools that do not install into the mise installs directory alone
/// (rust uses rustup in the home), so a shared install does not work.
const PRIVATE_CORE: &[&str] = &["rust"];

/// A command for the host mise that loads no project configuration. mise
/// reads `mise.toml` files from its working directory and the folders above
/// it, and a trusted one can run commands (`exec()` in templates, `[env]`).
/// So it runs in `/`, its search stops there, and the extra trusted paths
/// of the caller do not apply.
pub fn host_command(mise: &Path) -> std::process::Command {
    let mut c = std::process::Command::new(mise);
    c.current_dir("/")
        .env("PWD", "/")
        .env("MISE_CEILING_PATHS", "/")
        .env_remove("MISE_TRUSTED_CONFIG_PATHS")
        .env_remove("MISE_ENV")
        .env_remove("MISE_ENV_FILE");
    c
}

/// mise's registry: short name to backends, in the order mise prefers them.
#[derive(Debug, Default)]
pub struct Registry(BTreeMap<String, Vec<String>>);

impl Registry {
    /// Parse the output of `mise registry`: one tool per line, the name and
    /// then its backends.
    pub fn parse(text: &str) -> Self {
        let mut map = BTreeMap::new();
        for line in text.lines() {
            let mut words = line.split_whitespace();
            if let Some(name) = words.next() {
                map.insert(name.to_string(), words.map(str::to_string).collect());
            }
        }
        Self(map)
    }

    /// Read the registry of the host mise. See `host_command`.
    pub fn load(mise: &Path) -> Self {
        let out = crate::env::capture(host_command(mise).arg("registry"));
        out.map(|t| Self::parse(&t)).unwrap_or_default()
    }

    /// The backend mise picks for a short name when every backend in
    /// `DISABLED_BACKENDS` is off: `core` or `aqua`, or none.
    fn safe_backend(&self, name: &str) -> Option<&str> {
        for b in self.0.get(name)? {
            let kind = b.split(':').next().unwrap_or("");
            match kind {
                "core" | "aqua" => return Some(b),
                k if DISABLED_BACKENDS.contains(&k) => continue,
                // A backend this list does not know: it could run code.
                _ => return None,
            }
        }
        None
    }
}

/// One `[tools]` entry or `.tool-versions` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub tool: String,
    pub versions: Vec<String>,
    /// The entry has options (`{ version = "1", postinstall = "..." }`), or
    /// a value that is not a plain version string.
    pub complex: bool,
}

/// Entries and alias names from a `mise.toml`-style file. A file that does not
/// parse gives nothing: the project step reports the error.
pub fn parse_toml(text: &str) -> (Vec<Entry>, BTreeSet<String>) {
    let Ok(doc) = text.parse::<toml::Table>() else {
        return (vec![], BTreeSet::new());
    };
    let mut aliases = BTreeSet::new();
    for key in ["tool_alias", "alias"] {
        if let Some(t) = doc.get(key).and_then(|v| v.as_table()) {
            aliases.extend(t.keys().cloned());
        }
    }
    let mut out = Vec::new();
    let Some(tools) = doc.get("tools").and_then(|v| v.as_table()) else {
        return (out, aliases);
    };
    for (tool, value) in tools {
        let mut e = Entry {
            tool: tool.clone(),
            versions: vec![],
            complex: false,
        };
        let push = |v: &toml::Value, e: &mut Entry| match v {
            toml::Value::String(s) => e.versions.push(s.clone()),
            toml::Value::Table(t)
                if t.len() == 1 && t.get("version").is_some_and(|v| v.is_str()) =>
            {
                e.versions.push(t["version"].as_str().unwrap().to_string())
            }
            _ => e.complex = true,
        };
        match value {
            toml::Value::Array(items) => items.iter().for_each(|v| push(v, &mut e)),
            v => push(v, &mut e),
        }
        out.push(e);
    }
    (out, aliases)
}

/// Entries from a `.tool-versions` file: `tool version [version...]`.
pub fn parse_tool_versions(text: &str) -> Vec<Entry> {
    text.lines()
        .filter_map(|l| {
            let l = l.split('#').next().unwrap_or("");
            let mut words = l.split_whitespace();
            let tool = words.next()?.to_string();
            Some(Entry {
                tool,
                versions: words.map(str::to_string).collect(),
                complex: false,
            })
        })
        .collect()
}

/// A version mise resolves from the backend alone: no `path:`, `ref:`,
/// `prefix:` or other forms, and nothing that looks like an option.
pub fn plain_version(v: &str) -> bool {
    let mut chars = v.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && v.len() <= 64
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-'))
}

/// The tool name as `mise install` gets it, if the shared step may install
/// it: a registry name that resolves to `core` or `aqua`, `core:NAME`, or
/// `aqua:OWNER/REPO` without options.
pub fn shared_tool<'a>(tool: &'a str, registry: &Registry) -> Option<&'a str> {
    let name_ok = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    };
    let core = |name: &str| !PRIVATE_CORE.contains(&name);
    match tool.split_once(':') {
        Some(("core", name)) if name_ok(name) && core(name) => Some(tool),
        Some(("aqua", path)) => {
            let parts: Vec<&str> = path.split('/').collect();
            (parts.len() >= 2 && parts.iter().all(|p| name_ok(p) && *p != "..")).then_some(tool)
        }
        Some(_) => None,
        None if name_ok(tool) => match registry.safe_backend(tool)?.split_once(':') {
            Some(("core", name)) if !core(name) => None,
            _ => Some(tool),
        },
        None => None,
    }
}

/// The `tool@version` list for the shared step, from every mise file in the
/// project. Entries that do not qualify install in the project step instead.
pub fn shared_installs(project: &Path, registry: &Registry) -> Vec<String> {
    let mut entries = Vec::new();
    let mut aliases = BTreeSet::new();
    // The project is sandbox-writable: no symlinks, FIFOs or huge files.
    let Ok(root) = crate::safefs::Root::open(project) else {
        return Vec::new();
    };
    for f in crate::detect::MISE_FILES {
        let Ok(Some(bytes)) = root.read(Path::new(f), crate::safefs::MAX_READ) else {
            continue;
        };
        let text = String::from_utf8_lossy(&bytes);
        if f.ends_with(".tool-versions") {
            entries.extend(parse_tool_versions(&text));
        } else {
            let (e, a) = parse_toml(&text);
            entries.extend(e);
            aliases.extend(a);
        }
    }
    select(&entries, &aliases, registry)
}

pub fn select(entries: &[Entry], aliases: &BTreeSet<String>, registry: &Registry) -> Vec<String> {
    let mut out = Vec::new();
    for e in entries {
        if e.complex || aliases.contains(&e.tool) {
            continue;
        }
        let Some(tool) = shared_tool(&e.tool, registry) else {
            continue;
        };
        for v in &e.versions {
            let spec = format!("{tool}@{v}");
            if plain_version(v) && !out.contains(&spec) {
                out.push(spec);
            }
        }
    }
    out
}

/// Whether the shared installs already hold every `tool@version`: mise keeps
/// a folder or a link for each version it resolved (`22` links to `22.11.0`).
/// `latest` and `lts` can move, so they always run the shared step.
pub fn all_installed(installs: &Path, specs: &[String]) -> bool {
    specs.iter().all(|s| {
        let Some((tool, version)) = s.rsplit_once('@') else {
            return false;
        };
        let dir = tool.replace([':', '/'], "-");
        !matches!(version, "latest" | "lts") && installs.join(dir).join(version).exists()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_mise_runs_outside_the_project() {
        let t = tempfile::tempdir().unwrap();
        let fake = t.path().join("mise");
        std::fs::write(
            &fake,
            "#!/bin/sh\necho \"$(pwd) $MISE_CEILING_PATHS ${MISE_TRUSTED_CONFIG_PATHS-none}\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let out = crate::env::capture(&mut host_command(&fake)).unwrap();
        assert_eq!(out, "/ / none");
    }

    #[test]
    fn shared_installs_skip_planted_links_and_fifos() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("p");
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(t.path().join("real.toml"), "[tools]\nnode = \"22\"\n").unwrap();
        std::os::unix::fs::symlink(t.path().join("real.toml"), p.join("mise.toml")).unwrap();
        let c = std::ffi::CString::new(p.join(".tool-versions").to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        assert!(shared_installs(&p, &registry()).is_empty());
    }

    fn registry() -> Registry {
        Registry::parse(
            "node core:node\n\
             rust core:rust asdf:code-lever/asdf-rust\n\
             jq aqua:jqlang/jq asdf:mise-plugins/asdf-jq\n\
             ripgrep ubi:BurntSushi/ripgrep aqua:BurntSushi/ripgrep\n\
             tf asdf:x/asdf-tf vfox:x/vfox-tf\n\
             odd newbackend:x/y aqua:x/y\n",
        )
    }

    fn pick(text: &str) -> Vec<String> {
        let (e, a) = parse_toml(text);
        select(&e, &a, &registry())
    }

    #[test]
    fn plain_registry_tools_are_shared() {
        assert_eq!(
            pick("[tools]\nnode = \"22\"\njq = { version = \"1.7.1\" }\nripgrep = [\"14\", \"latest\"]\n"),
            ["jq@1.7.1", "node@22", "ripgrep@14", "ripgrep@latest"]
        );
        assert_eq!(
            pick(
                "[tools]\n\"aqua:koalaman/shellcheck\" = \"0.10.0\"\n\"core:python\" = \"3.12\"\n"
            ),
            ["aqua:koalaman/shellcheck@0.10.0", "core:python@3.12"]
        );
    }

    #[test]
    fn code_running_backends_stay_private() {
        let toml = r#"
[tools]
"ubi:evil/tool" = "1"
"cargo:evil" = "1"
"npm:evil" = "1"
"go:example.com/evil" = "1"
"asdf:evil/asdf-x" = "1"
"vfox:evil/x" = "1"
"github:evil/x" = "1"
"http:x" = "1"
"aqua:x/y[asset=evil]" = "1"
"aqua:../../x" = "1"
tf = "1"
odd = "1"
unknown = "1"
rust = "1.80"
"core:rust" = "1.80"
"#;
        assert!(pick(toml).is_empty(), "{:?}", pick(toml));
    }

    #[test]
    fn options_and_odd_versions_stay_private() {
        let toml = r#"
[tools]
node = { version = "22", postinstall = "touch /x" }
jq = ["1.7", { version = "1.6", os = ["linux"] }]
ripgrep = "path:/tmp/rg"
"core:python" = "ref:main"
"core:go" = "-rf"
"core:java" = 21
"#;
        assert!(pick(toml).is_empty(), "{:?}", pick(toml));
    }

    #[test]
    fn hooks_and_env_do_not_matter_but_aliases_do() {
        let toml = r#"
[env]
_.source = "./evil.sh"
[hooks]
enter = "touch /x"
[tool_alias]
jq = "ubi:evil/jq"
[tools]
node = "22"
jq = "1.7.1"
"#;
        assert_eq!(pick(toml), ["node@22"]);
        assert!(pick("not = [toml").is_empty());
    }

    #[test]
    fn tool_versions_lines() {
        let e = parse_tool_versions("# c\nnodejs 22 20 # two\n\njq 1.7.1\nubi:x/y 1\n");
        assert_eq!(e[0].tool, "nodejs");
        assert_eq!(e[0].versions, ["22", "20"]);
        assert_eq!(
            select(&e, &BTreeSet::new(), &registry()),
            ["jq@1.7.1"],
            "nodejs is not in this registry; ubi stays private"
        );
    }

    #[test]
    fn installed_versions() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("node/22")).unwrap();
        std::fs::create_dir_all(tmp.path().join("aqua-koalaman-shellcheck/0.10.0")).unwrap();
        let has = |s: &[&str]| {
            all_installed(
                tmp.path(),
                &s.iter().map(|x| x.to_string()).collect::<Vec<_>>(),
            )
        };
        assert!(has(&["node@22", "aqua:koalaman/shellcheck@0.10.0"]));
        assert!(!has(&["node@22", "jq@1.7.1"]));
        std::fs::create_dir_all(tmp.path().join("node/latest")).unwrap();
        assert!(!has(&["node@latest"]));
    }

    #[test]
    fn versions() {
        for v in ["22", "3.12.1", "latest", "lts", "temurin-21", "1.0.0+build"] {
            assert!(plain_version(v), "{v}");
        }
        for v in [
            "",
            "-x",
            "ref:main",
            "path:/x",
            "sub-1:lts",
            "a b",
            "1;rm",
            "../x",
        ] {
            assert!(!plain_version(v), "{v}");
        }
    }
}
