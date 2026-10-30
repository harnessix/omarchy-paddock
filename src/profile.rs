//! Harness profiles and the factory that turns a profile name into a located,
//! ready-to-mount harness. A profile is a TOML file; adding a harness means
//! adding a file, not code.

use crate::env::{capture, is_executable, HostEnv};
use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub locate: Locate,
    #[serde(default)]
    pub launch: Launch,
    #[serde(default)]
    pub state: State,
    #[serde(default)]
    pub network: ProfileNetwork,
    #[serde(default)]
    pub mcp: Vec<McpSource>,
    /// Vendor telemetry switches that paddock turns off inside the sandbox,
    /// and where the user turns off the use of their data for training.
    #[serde(default)]
    pub privacy: Privacy,
    /// Profile entries that come from documentation, not from a test on a
    /// real install. `paddock doctor` lists them.
    #[serde(default)]
    pub unverified: Vec<String>,
    /// Present for desktop apps, which `paddock app` runs.
    pub app: Option<AppProfile>,
    /// The non-interactive mode that runs one task and exits. `paddock-host
    /// spawn` starts a harness only with this command.
    pub oneshot: Option<OneShot>,
}

/// How a prompt reaches a one-shot run. Never in the arguments: the
/// arguments of a process are visible to every local user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PromptVia {
    /// The prompt is the standard input of the harness.
    Stdin,
    /// The prompt is a file in the sandbox home. `{prompt_file}` in `args`
    /// becomes its path inside the sandbox.
    File,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OneShot {
    /// Arguments after the executable, including the auto-approval flags.
    /// Fixed text only: the prompt never goes here.
    pub args: Vec<String>,
    pub prompt: PromptVia,
    /// Arguments for the help output that must mention every flag in
    /// `args`, the same check as for `yolo_args`.
    #[serde(default = "default_probe")]
    pub probe_args: Vec<String>,
}

/// The placeholder for the prompt file in `OneShot::args`.
pub const PROMPT_FILE: &str = "{prompt_file}";

impl OneShot {
    /// The arguments with the prompt file path filled in.
    pub fn render(&self, prompt_file: &Path) -> Vec<String> {
        let f = prompt_file.display().to_string();
        self.args
            .iter()
            .map(|a| a.replace(PROMPT_FILE, &f))
            .collect()
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppProfile {
    /// Display name for launcher entries.
    #[serde(default)]
    pub display_name: String,
    /// A flatpak application ID. paddock then runs it with `flatpak run
    /// --sandbox` inside pasta, instead of bwrap.
    pub flatpak: Option<String>,
    /// Icon name or path for launcher entries.
    pub icon: Option<String>,
    /// For flatpak: the command to run instead of the app's own (`--command`).
    pub command: Option<String>,
    /// URL schemes the app handles, without "://", for example "claude".
    /// `paddock app install` makes paddock their handler, and links reach the
    /// running sandbox.
    #[serde(default)]
    pub schemes: Vec<String>,
    /// Pass the workspace folders to the app as arguments, for editors that
    /// open the folders they are given.
    #[serde(default)]
    pub open_roots: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Locate {
    /// Command names to look up on PATH, in order.
    #[serde(default)]
    pub commands: Vec<String>,
    /// Executable name for `mise which` (not the mise tool name). Tried first
    /// when mise is installed, so harnesses are found without mise on PATH.
    pub mise: Option<String>,
    /// Extra candidate paths. `~/` expands to the home directory.
    #[serde(default)]
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Launch {
    /// Arguments that turn on full auto-approval.
    #[serde(default)]
    pub yolo_args: Vec<String>,
    /// Arguments that are always passed.
    #[serde(default)]
    pub args: Vec<String>,
    /// Environment variables set inside the sandbox.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Arguments for the help output that must mention every `yolo_args` flag.
    #[serde(default = "default_probe")]
    pub probe_args: Vec<String>,
    /// The harness logs in with a browser and a localhost callback. paddock
    /// opens login pages in the host browser and forwards the callback port.
    #[serde(default)]
    pub login_callback: bool,
    /// The harness keeps its login in the system keyring. The sandbox gets a
    /// private keyring of its own (keyring.rs), never the host one.
    #[serde(default)]
    pub keyring: bool,
    /// `paddock wrap` runs this harness on the host, with this reason in a
    /// notice, because it cannot work in a sandbox (for example, it needs
    /// its own sandbox, or its agent runs in a host service).
    pub wrap_on_host: Option<String>,
}

impl Default for Launch {
    fn default() -> Self {
        Self {
            yolo_args: vec![],
            args: vec![],
            env: BTreeMap::new(),
            probe_args: default_probe(),
            login_callback: false,
            keyring: false,
            wrap_on_host: None,
        }
    }
}

fn default_probe() -> Vec<String> {
    vec!["--help".to_string()]
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    /// Credential files, relative to HOME. Paddock copies them in and out.
    #[serde(default)]
    pub auth: Vec<String>,
    /// Host files and directories, relative to HOME, mounted read-only.
    #[serde(default)]
    pub import_ro: Vec<String>,
    /// Host environment variables passed through when they are set.
    #[serde(default)]
    pub env_pass: Vec<String>,
    /// Host files and directories, relative to HOME, copied into the sandbox
    /// home at each start. The copy is writable and is not copied back.
    #[serde(default)]
    pub seed: Vec<String>,
    /// Keys copied from a host JSON file into the sandbox copy at each start.
    #[serde(default)]
    pub json_import: Vec<JsonImport>,
    /// Values set in a TOML file in the sandbox home at each start, after
    /// `seed`. For example, marking the project trusted: paddock is the
    /// external sandbox, so a second trust prompt only gets in the way.
    #[serde(default)]
    pub toml_set: Vec<TomlSet>,
    /// Values written into settings files in the sandbox home at each start,
    /// after `seed`, for example to turn off a self-update that cannot work
    /// on the read-only install. The host files are never changed.
    #[serde(default)]
    pub settings: Vec<Setting>,
    /// The variable that points the harness at another account folder, as
    /// Omarchy's account switch sets it. `paddock wrap` reads it from the
    /// command and uses that folder as the login of the sandbox.
    pub account: Option<Account>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Account {
    /// The variable, for example CLAUDE_CONFIG_DIR.
    pub env: String,
    /// The folder that the variable replaces, relative to HOME.
    pub dir: String,
    /// Files outside `dir`, relative to HOME, that move into the account
    /// folder with it, for example `.claude.json`.
    #[serde(default)]
    pub also: Vec<String>,
}

impl Account {
    /// The host path of `rel` (relative to HOME) for the account folder
    /// `folder`. None: the path does not move with the account.
    pub fn map(&self, folder: &Path, rel: &str) -> Option<PathBuf> {
        let dir = self.dir.trim_end_matches('/');
        if rel == dir {
            return Some(folder.to_path_buf());
        }
        if let Some(rest) = rel.strip_prefix(dir).and_then(|r| r.strip_prefix('/')) {
            return Some(folder.join(rest));
        }
        if self.also.iter().any(|a| a == rel) {
            return Path::new(rel).file_name().map(|n| folder.join(n));
        }
        None
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TomlSet {
    /// File relative to HOME.
    pub file: String,
    /// Key path. `{project}` is the project path.
    pub path: Vec<String>,
    pub value: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JsonImport {
    /// File relative to HOME.
    pub file: String,
    /// Key paths copied in every auth mode. `{project}` is the project path.
    #[serde(default)]
    pub keys: Vec<Vec<String>>,
    /// Key paths copied only in the shared auth mode (account identity).
    #[serde(default)]
    pub shared_keys: Vec<Vec<String>>,
}

/// Where a harness lists its MCP servers.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpSource {
    /// File relative to HOME, or to the project when `in_project` is true.
    pub file: String,
    #[serde(default)]
    pub in_project: bool,
    /// json or toml.
    #[serde(default = "default_json")]
    pub format: String,
    /// Key path to the table of servers. `{project}` is the project path.
    pub path: Vec<String>,
}

fn default_json() -> String {
    "json".into()
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileNetwork {
    /// Hosts the harness needs. Reserved for the allowlist network mode.
    #[serde(default)]
    pub endpoints: Vec<String>,
    /// Ports on the sandbox loopback that the host reaches on its own
    /// 127.0.0.1, for a harness that serves its UI to the host browser.
    #[serde(default)]
    pub publish: Vec<u16>,
}

/// The privacy defaults of a harness (see privacy.rs). Each entry must come
/// from the vendor's documentation, with the link in a comment next to it.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Privacy {
    /// Environment variables that turn telemetry, usage statistics or crash
    /// reports off.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Arguments that turn telemetry off, passed after `yolo_args`.
    #[serde(default)]
    pub args: Vec<String>,
    /// Values written into settings files in the sandbox home at each start,
    /// after `seed`. The host files are never changed.
    #[serde(default)]
    pub settings: Vec<Setting>,
    /// Data use for training is an account setting that no program can
    /// change: where the user turns it off.
    pub training: Option<Training>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Setting {
    /// File relative to HOME.
    pub file: String,
    /// json or toml.
    #[serde(default = "default_json")]
    pub format: String,
    /// Key path. `{project}` is the project path.
    pub path: Vec<String>,
    pub value: toml::Value,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Training {
    /// Where the setting is, in plain words.
    pub setting: String,
    /// The official page with the setting, or the documentation about it.
    pub url: String,
}

const BUILTIN: &[(&str, &str)] = &[
    ("agy", include_str!("../profiles/agy.toml")),
    (
        "chatgpt-desktop",
        include_str!("../profiles/chatgpt-desktop.toml"),
    ),
    (
        "claude-desktop",
        include_str!("../profiles/claude-desktop.toml"),
    ),
    ("vscode", include_str!("../profiles/vscode.toml")),
    ("claude", include_str!("../profiles/claude.toml")),
    ("codex", include_str!("../profiles/codex.toml")),
    ("copilot", include_str!("../profiles/copilot.toml")),
    ("gemini", include_str!("../profiles/gemini.toml")),
    ("grok", include_str!("../profiles/grok.toml")),
    ("opencode", include_str!("../profiles/opencode.toml")),
    // The other agents that omarchy-agent starts.
    ("amp", include_str!("../profiles/amp.toml")),
    ("commandcode", include_str!("../profiles/commandcode.toml")),
    ("crush", include_str!("../profiles/crush.toml")),
    (
        "cursor-agent",
        include_str!("../profiles/cursor-agent.toml"),
    ),
    ("devin", include_str!("../profiles/devin.toml")),
    ("dsh", include_str!("../profiles/dsh.toml")),
    ("freebuff", include_str!("../profiles/freebuff.toml")),
    ("hermes", include_str!("../profiles/hermes.toml")),
    ("kilo", include_str!("../profiles/kilo.toml")),
    ("kimi", include_str!("../profiles/kimi.toml")),
    ("kiro-cli", include_str!("../profiles/kiro-cli.toml")),
    ("mcode", include_str!("../profiles/mcode.toml")),
    ("muse", include_str!("../profiles/muse.toml")),
    ("omp", include_str!("../profiles/omp.toml")),
    ("openclaw", include_str!("../profiles/openclaw.toml")),
    ("ori", include_str!("../profiles/ori.toml")),
    ("pi", include_str!("../profiles/pi.toml")),
    ("qwen", include_str!("../profiles/qwen.toml")),
];

/// A built-in profile by name.
pub fn builtin(name: &str) -> Option<Profile> {
    BUILTIN
        .iter()
        .find(|(n, _)| *n == name)
        .and_then(|(_, text)| toml::from_str(text).ok())
}

/// A harness that the factory located on this host.
#[derive(Debug, Clone)]
pub struct Harness {
    pub profile: Profile,
    /// The real executable, after resolving symlinks and version-manager shims.
    pub exe: PathBuf,
    /// Host paths to mount read-only so the executable can run.
    pub install_mounts: Vec<PathBuf>,
    /// Directories to put on PATH inside the sandbox.
    pub path_dirs: Vec<PathBuf>,
    /// Where each profile came from.
    pub source: String,
}

pub struct HarnessFactory<'a> {
    env: &'a HostEnv,
    profiles: BTreeMap<String, (Profile, String)>,
}

impl<'a> HarnessFactory<'a> {
    pub fn new(env: &'a HostEnv) -> Result<Self> {
        let mut profiles = BTreeMap::new();
        for (name, text) in BUILTIN {
            let p: Profile = toml::from_str(text)
                .with_context(|| format!("built-in profile '{name}' is invalid"))?;
            profiles.insert(p.name.clone(), (p, "built-in".to_string()));
        }
        let user_dir = env.paddock_config().join("harness.d");
        if let Ok(entries) = std::fs::read_dir(&user_dir) {
            let mut files: Vec<PathBuf> = entries
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|x| x == "toml"))
                .collect();
            files.sort();
            for file in files {
                let text = std::fs::read_to_string(&file)?;
                let p: Profile = toml::from_str(&text)
                    .with_context(|| format!("profile {} is invalid", file.display()))?;
                profiles.insert(p.name.clone(), (p, file.display().to_string()));
            }
        }
        Ok(Self { env, profiles })
    }

    pub fn names(&self) -> Vec<String> {
        self.profiles.keys().cloned().collect()
    }

    pub fn profile(&self, name: &str) -> Option<&Profile> {
        self.profiles.get(name).map(|(p, _)| p)
    }

    pub fn create(&self, name: &str) -> Result<Harness> {
        let (profile, source) = self.profiles.get(name).ok_or_else(|| {
            anyhow!(
                "unknown harness '{name}'. Known harnesses: {}. Add a profile in {}",
                self.names().join(", "),
                self.env.paddock_config().join("harness.d").display()
            )
        })?;
        let exe = self.locate(profile)?;
        let (install_mounts, path_dirs) = runtime_closure(self.env, &exe)?;
        Ok(Harness {
            profile: profile.clone(),
            exe,
            install_mounts,
            path_dirs,
            source: source.clone(),
        })
    }

    fn locate(&self, p: &Profile) -> Result<PathBuf> {
        let mut tried = Vec::new();
        if let (Some(tool), Some(mise)) = (&p.locate.mise, self.env.which("mise")) {
            tried.push(format!("mise which {tool}"));
            if let Ok(out) = capture(crate::mise::host_command(&mise).args(["which", tool])) {
                let path = PathBuf::from(out.lines().last().unwrap_or_default());
                if is_executable(&path) {
                    return canonical(&path);
                }
            }
        }
        for cmd in &p.locate.commands {
            tried.push(format!("PATH lookup of {cmd}"));
            // The first match that is not a version-manager shim: Omarchy
            // puts mise's wrapper folder first on PATH.
            let found = self
                .env
                .path
                .iter()
                .filter(|d| d.is_absolute())
                .map(|d| d.join(cmd))
                .find(|p| is_executable(p) && !is_shim(p));
            if let Some(path) = found {
                return canonical(&path);
            }
        }
        for raw in &p.locate.paths {
            let path = self.env.expand(raw, &self.env.home);
            tried.push(path.display().to_string());
            if is_executable(&path) {
                return canonical(&path);
            }
        }
        bail!(
            "could not find the '{}' executable. Tried: {}",
            p.name,
            tried.join("; ")
        )
    }
}

/// A version-manager shim re-enters the manager, which is not mounted in the sandbox.
fn is_shim(path: &Path) -> bool {
    path.components()
        .any(|c| matches!(c, Component::Normal(s) if s == "shims" || s == "command-wrappers"))
}

fn canonical(path: &Path) -> Result<PathBuf> {
    std::fs::canonicalize(path).with_context(|| format!("cannot resolve {}", path.display()))
}

const SYSTEM_PREFIXES: &[&str] = &[
    "/usr/",
    "/bin/",
    "/sbin/",
    "/lib/",
    "/lib64/",
    "/opt/",
    "/nix/store/",
];

pub fn is_system_path(p: &Path) -> bool {
    let s = p.to_string_lossy();
    SYSTEM_PREFIXES.iter().any(|pre| s.starts_with(pre))
}

/// Find what an executable needs from the host: its install directory and, for
/// scripts, the interpreter named in the shebang line.
pub fn runtime_closure(env: &HostEnv, exe: &Path) -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
    let mut mounts = Vec::new();
    let mut path_dirs = Vec::new();
    let mut queue = vec![exe.to_path_buf()];
    let mut seen = 0;
    while let Some(file) = queue.pop() {
        seen += 1;
        if seen > 4 {
            break;
        }
        mounts.push(install_root(&file));
        if let Some(interp) = shebang_interpreter(&file) {
            let resolved = if interp.contains('/') {
                Some(PathBuf::from(&interp))
            } else {
                env.which(&interp)
            };
            if let Some(found) = resolved {
                if let Some(dir) = found.parent() {
                    if !is_system_path(&found) {
                        path_dirs.push(dir.to_path_buf());
                    }
                }
                if let Ok(real) = std::fs::canonicalize(&found) {
                    if let Some(dir) = real.parent() {
                        path_dirs.push(dir.to_path_buf());
                    }
                    queue.push(real);
                }
            }
        }
    }
    mounts.sort();
    mounts.dedup();
    // Drop mounts that sit inside another mount.
    let all = mounts.clone();
    mounts.retain(|m| !all.iter().any(|o| o != m && m.starts_with(o)));
    path_dirs.dedup();
    Ok((mounts, path_dirs))
}

/// Bin directories in the home that hold single, self-contained executables
/// next to unrelated ones. The folder above them can hold credentials
/// (`~/.cargo/credentials.toml`), so only the file is mounted.
pub const HOME_BIN_DIRS: &[&str] = &[
    ".local/bin",
    ".cargo/bin",
    "go/bin",
    ".bun/bin",
    ".deno/bin",
];

/// The directory to mount for an executable. For npm packages this is the
/// outermost `node_modules` folder: it holds the package and the packages it
/// loads, and nothing else of the project or install around it (a `.env`, the
/// sources). For a binary in a shared bin directory it is the file itself, so
/// the directory is not shadowed in an image and its neighbours stay hidden.
pub fn install_root(exe: &Path) -> PathBuf {
    let comps: Vec<_> = exe.components().collect();
    if let Some(i) = comps
        .iter()
        .position(|c| matches!(c, Component::Normal(s) if *s == "node_modules"))
    {
        return comps[..=i].iter().collect();
    }
    let parent = exe.parent().unwrap_or(Path::new("/"));
    let shared = [
        "/usr/bin",
        "/usr/sbin",
        "/bin",
        "/sbin",
        "/usr/local/bin",
        "/usr/local/sbin",
    ];
    if shared.iter().any(|d| parent == Path::new(d))
        || HOME_BIN_DIRS.iter().any(|d| parent.ends_with(d))
    {
        return exe.to_path_buf();
    }
    // A versioned install directory such as `<tool>/1.2.3/bin/<exe>`.
    if parent.file_name().is_some_and(|n| n == "bin") {
        if let Some(gp) = parent.parent() {
            return gp.to_path_buf();
        }
    }
    parent.to_path_buf()
}

fn shebang_interpreter(file: &Path) -> Option<String> {
    let mut buf = [0u8; 256];
    let n = std::fs::File::open(file).ok()?.read(&mut buf).ok()?;
    let head = std::str::from_utf8(&buf[..n]).ok()?;
    let line = head.strip_prefix("#!")?.lines().next()?.trim();
    let mut parts = line.split_whitespace();
    let first = parts.next()?;
    if first.ends_with("/env") {
        parts.find(|a| !a.starts_with('-')).map(str::to_string)
    } else {
        Some(first.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Harnesses without a flag for no approvals: they ask none (pi, ori,
    /// freebuff, mcode's terminal UI), take the mode from a variable (dsh),
    /// or run on the host (openclaw).
    const NO_APPROVAL_FLAG: &[&str] = &["pi", "ori", "freebuff", "mcode", "dsh", "openclaw"];

    #[test]
    fn builtin_profiles_parse_and_have_yolo_args() {
        for (name, text) in BUILTIN {
            let p: Profile = toml::from_str(text).unwrap();
            assert_eq!(&p.name, name);
            // Desktop apps keep their approval setting in their own settings.
            // Some harnesses ask for no approvals, or take the mode from a
            // variable that omarchy-agent sets.
            if p.app.is_none() && !NO_APPROVAL_FLAG.contains(name) {
                assert!(!p.launch.yolo_args.is_empty(), "{name} has no yolo_args");
            }
        }
    }

    #[test]
    fn builtin_privacy_entries_are_well_formed() {
        for (name, text) in BUILTIN {
            let p: Profile = toml::from_str(text).unwrap();
            let t = p.privacy.training.as_ref();
            assert!(t.is_some(), "{name} has no privacy.training note");
            assert!(t.unwrap().url.starts_with("https://"), "{name}");
            for s in &p.privacy.settings {
                assert!(
                    s.format == "json" || s.format == "toml",
                    "{name}: {}",
                    s.file
                );
                assert!(!s.path.is_empty(), "{name}: {}", s.file);
                let rel = Path::new(&s.file);
                assert!(
                    rel.is_relative()
                        && rel.components().all(|c| matches!(c, Component::Normal(_))),
                    "{name}: {} must stay inside the sandbox home",
                    s.file
                );
            }
        }
    }

    #[test]
    fn one_shot_commands_never_carry_the_prompt_in_argv() {
        for (name, text) in BUILTIN {
            let p: Profile = toml::from_str(text).unwrap();
            let Some(o) = p.oneshot else { continue };
            let file_args = o.args.iter().filter(|a| a.contains(PROMPT_FILE)).count();
            match o.prompt {
                PromptVia::Stdin => assert_eq!(file_args, 0, "{name}"),
                PromptVia::File => assert_eq!(file_args, 1, "{name} must name the prompt file"),
            }
            let secret = "PROMPT-TEXT-THAT-MUST-NOT-LEAK";
            let argv = o.render(Path::new("/home/demo/.paddock-spawn/prompt.md"));
            assert!(argv.iter().all(|a| !a.contains(secret)), "{name}");
        }
    }

    #[test]
    fn install_root_rules() {
        assert_eq!(
            install_root(Path::new(
                "/x/opencode/2.2.0/node_modules/opencode/bin/cli.js"
            )),
            PathBuf::from("/x/opencode/2.2.0/node_modules")
        );
        // Another repository's package: its node_modules, never the repository.
        assert_eq!(
            install_root(Path::new(
                "/home/demo/other/node_modules/pkg/node_modules/dep/cli.js"
            )),
            PathBuf::from("/home/demo/other/node_modules")
        );
        for single in ["/home/demo/.cargo/bin/tool", "/home/demo/go/bin/tool"] {
            assert_eq!(install_root(Path::new(single)), PathBuf::from(single));
        }
        assert_eq!(
            install_root(Path::new("/usr/bin/claude")),
            PathBuf::from("/usr/bin/claude")
        );
        assert_eq!(
            install_root(Path::new("/x/claude/2.1/claude")),
            PathBuf::from("/x/claude/2.1")
        );
        assert_eq!(
            install_root(Path::new("/x/node/26/bin/node")),
            PathBuf::from("/x/node/26")
        );
    }
}
