//! `paddock wrap -- COMMAND`: run an agent command as it is, plus the
//! sandbox (transparent mode). `omarchy-agent` builds these commands; see
//! omarchy.rs for how they reach paddock.
//!
//! - The arguments do not change: paddock adds no `yolo_args` and removes
//!   nothing. Only the privacy flags of the profile are added, as in every
//!   sandbox.
//! - The project is the current folder. The home folder is no project, so a
//!   start there works in a scratch project for the agent.
//! - An `env` prefix keeps its variables. The account variables of Omarchy's
//!   account switch (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `GROK_HOME`) select
//!   the host folder that is the login of the sandbox.
//! - A command without a profile runs as it is, with a notification.

use crate::build::{self, Options};
use crate::env::HostEnv;
use crate::profile::HarnessFactory;
use anyhow::{Context, Result};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A command as `omarchy-agent` builds it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Parsed {
    /// Assignments of an `env` prefix, in order.
    pub env_set: Vec<(String, String)>,
    /// Variables that an `env` prefix removes (`-u NAME`).
    pub env_unset: Vec<String>,
    /// The shell words before the agent in the shell form, for example
    /// `["bash", "-c", SCRIPT]`. The agent is the script's `$0`.
    pub shell: Vec<String>,
    /// The agent program as written: a name or an absolute path.
    pub program: String,
    pub args: Vec<String>,
}

impl Parsed {
    /// The command name of the agent: the base name of the program.
    pub fn name(&self) -> &str {
        self.program.rsplit('/').next().unwrap_or(&self.program)
    }

    /// The value of `var` that the command gives the agent: from the prefix,
    /// or from `host` unless the prefix removes it. Empty counts as unset.
    pub fn var(&self, var: &str, host: Option<&str>) -> Option<String> {
        if let Some((_, v)) = self.env_set.iter().rev().find(|(k, _)| k == var) {
            return Some(v.clone()).filter(|v| !v.is_empty());
        }
        if self.env_unset.iter().any(|k| k == var) {
            return None;
        }
        host.filter(|v| !v.is_empty()).map(str::to_string)
    }
}

fn base(s: &str) -> &str {
    s.rsplit('/').next().unwrap_or(s)
}

fn valid_var(k: &str) -> bool {
    !k.is_empty()
        && !k.starts_with(|c: char| c.is_ascii_digit())
        && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Read a command. None for a form that paddock does not know: it then runs
/// as it is.
pub fn parse(argv: &[String]) -> Option<Parsed> {
    let mut p = Parsed::default();
    let mut i = 0;
    if base(argv.first()?) == "env" {
        i = 1;
        while let Some(a) = argv.get(i) {
            if a == "--" {
                i += 1;
                break;
            } else if a == "-u" || a == "--unset" {
                p.env_unset.push(argv.get(i + 1)?.clone());
                i += 2;
            } else if let Some(k) = a.strip_prefix("--unset=") {
                p.env_unset.push(k.to_string());
                i += 1;
            } else if let Some(k) = a.strip_prefix("-u").filter(|k| !k.is_empty()) {
                p.env_unset.push(k.to_string());
                i += 1;
            } else if a.starts_with('-') {
                // -i, -C, -S and the others change more than variables.
                return None;
            } else if let Some((k, v)) = a.split_once('=') {
                if !valid_var(k) {
                    return None;
                }
                p.env_set.push((k.to_string(), v.to_string()));
                i += 1;
            } else {
                break;
            }
        }
    }
    let program = argv.get(i)?;
    if matches!(base(program), "bash" | "sh") {
        // bash -c SCRIPT $0 ARGS...: the agent is $0.
        if argv.get(i + 1).map(String::as_str) != Some("-c") {
            return None;
        }
        let script = argv.get(i + 2)?;
        let zero = argv.get(i + 3)?;
        p.shell = vec![program.clone(), "-c".into(), script.clone()];
        p.program = zero.clone();
        p.args = argv[i + 4..].to_vec();
    } else {
        p.program = program.clone();
        p.args = argv[i + 1..].to_vec();
    }
    if p.program.is_empty() || (p.program.contains('/') && !p.program.starts_with('/')) {
        return None;
    }
    Some(p)
}

/// The terminal profile for a command name: a profile with that name, or
/// one that lists the command.
pub fn profile_for(env: &HostEnv, cmd: &str) -> Option<String> {
    let f = HarnessFactory::new(env).ok()?;
    let usable = |n: &str| f.profile(n).is_some_and(|p| p.app.is_none());
    if usable(cmd) {
        return Some(cmd.to_string());
    }
    f.names().into_iter().find(|n| {
        usable(n)
            && f.profile(n)
                .is_some_and(|p| p.locate.commands.iter().any(|c| c == cmd))
    })
}

/// Scratch projects: where an agent works when it starts in the home folder.
pub use crate::storage::scratch_root;

/// The project for a start in `cwd`. The home folder, and a folder whose
/// project would be the home or contain it, get the scratch project of the
/// agent. Returns the project and whether it is the scratch project.
pub fn project_for(env: &HostEnv, cwd: &Path, agent: &str) -> Result<(PathBuf, bool)> {
    let project = build::resolve_project(Some(cwd))?;
    let home = std::fs::canonicalize(&env.home).unwrap_or_else(|_| env.home.clone());
    if project == home || home.starts_with(&project) {
        let dir = scratch_root(env).join(agent);
        crate::privfs::dir(&dir)?;
        return Ok((std::fs::canonicalize(&dir)?, true));
    }
    Ok((project, false))
}

/// Notify and print that a command runs without a sandbox.
fn tell_unsandboxed(env: &HostEnv, what: &str, why: &str) {
    eprintln!("paddock: {what} runs without a sandbox: {why}");
    let _ = crate::broker::notify::show(
        env,
        &crate::broker::notify::Note {
            summary: format!("{what} runs without a sandbox"),
            body: why.to_string(),
            ..Default::default()
        },
    );
}

/// Run the command on the host as it is. Returns only on an error.
fn exec_unchanged(env: &HostEnv, argv: &[String]) -> Result<i32> {
    let e = Command::new(&argv[0])
        .args(&argv[1..])
        .env("PATH", path_var(env))
        .env_remove(crate::omarchy::INLINE_VAR)
        .exec();
    Err(anyhow::Error::from(e).context(format!("cannot run {}", argv[0])))
}

fn path_var(env: &HostEnv) -> String {
    std::env::join_paths(&env.path)
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The environment of `paddock wrap`: paddock's shim folder is not on PATH,
/// so neither paddock nor the sandbox finds a shim again.
pub fn clean_env(mut env: HostEnv) -> HostEnv {
    env.path = crate::omarchy::strip_shims(&env, &env.path);
    let p = path_var(&env);
    env.vars.insert("PATH".into(), p);
    env.vars.remove(crate::omarchy::INLINE_VAR);
    env
}

/// What paddock starts for a parsed command.
pub struct Plan {
    pub profile: String,
    pub opts: Options,
    /// The user install that the command names, instead of the profile's
    /// search.
    pub exe: Option<PathBuf>,
    pub scratch: bool,
    pub account: Option<PathBuf>,
}

/// The variables that select an account folder, for each profile with one.
fn account_var(env: &HostEnv, profile: &str) -> Option<String> {
    let f = HarnessFactory::new(env).ok()?;
    f.profile(profile)?
        .state
        .account
        .as_ref()
        .map(|a| a.env.clone())
}

pub fn plan(env: &HostEnv, p: &Parsed, cwd: &Path) -> Result<Option<Plan>> {
    let Some(profile) = profile_for(env, p.name()) else {
        return Ok(None);
    };
    let (project, scratch) = project_for(env, cwd, &profile)?;
    let mut t = Transparent {
        shell: p.shell.clone(),
        env_set: p.env_set.clone(),
        env_unset: p.env_unset.clone(),
        account: None,
    };
    let mut account = None;
    if let Some(var) = account_var(env, &profile) {
        if let Some(dir) = p.var(&var, env.var(&var)) {
            let dir = PathBuf::from(&dir);
            if !dir.is_absolute() {
                anyhow::bail!("{var}={} is not an absolute path", dir.display());
            }
            let dir = std::fs::canonicalize(&dir).unwrap_or(dir);
            let home = std::fs::canonicalize(&env.home).unwrap_or_else(|_| env.home.clone());
            // The default folder (~/.claude for CLAUDE_CONFIG_DIR) is the
            // shared login.
            let default = HarnessFactory::new(env)?
                .profile(&profile)
                .and_then(|pr| pr.state.account.as_ref().map(|a| home.join(&a.dir)));
            if Some(&dir) != default.as_ref() {
                account = Some(dir.clone());
                t.account = Some(dir);
            }
        }
        // Inside the sandbox the agent uses its usual folder in the sandbox
        // home; the account files are copied there.
        t.env_set.retain(|(k, _)| *k != var);
        if !t.env_unset.contains(&var) {
            t.env_unset.push(var);
        }
    }
    let exe = p
        .program
        .starts_with('/')
        .then(|| PathBuf::from(&p.program));
    let opts = Options {
        harness: profile.clone(),
        project: Some(project),
        // Transparent: the arguments stay as omarchy-agent chose them.
        no_yolo: true,
        args: p.args.clone(),
        transparent: Some(t),
        ..Default::default()
    };
    Ok(Some(Plan {
        profile,
        opts,
        exe,
        scratch,
        account,
    }))
}

/// The parts of the command that `build` keeps (see `Options::transparent`).
#[derive(Debug, Clone, Default)]
pub struct Transparent {
    pub shell: Vec<String>,
    pub env_set: Vec<(String, String)>,
    pub env_unset: Vec<String>,
    /// The host account folder that is the login of the sandbox.
    pub account: Option<PathBuf>,
}

fn wait_for_enter() {
    use std::io::IsTerminal;
    if std::io::stdin().is_terminal() && std::io::stderr().is_terminal() {
        eprintln!("Press Enter to close.");
        let mut s = String::new();
        let _ = std::io::stdin().read_line(&mut s);
    }
}

/// `paddock wrap -- COMMAND`.
pub fn main(env: HostEnv, argv: &[String]) -> Result<i32> {
    let env = clean_env(env);
    if argv.is_empty() {
        anyhow::bail!("name a command: paddock wrap -- COMMAND [ARGS]");
    }
    if env.var("PADDOCK_BYPASS").is_some_and(|v| !v.is_empty()) || env.var("PADDOCK") == Some("1") {
        return exec_unchanged(&env, argv);
    }
    let Some(parsed) = parse(argv) else {
        tell_unsandboxed(&env, &argv[0], "paddock does not know this form of command");
        return exec_unchanged(&env, argv);
    };
    if let Some(why) = profile_for(&env, parsed.name()).and_then(|p| {
        HarnessFactory::new(&env)
            .ok()?
            .profile(&p)?
            .launch
            .wrap_on_host
            .clone()
    }) {
        tell_unsandboxed(&env, parsed.name(), &why);
        return exec_unchanged(&env, argv);
    }
    let cwd = std::env::current_dir()?;
    let plan = match plan(&env, &parsed, &cwd) {
        Ok(Some(p)) => p,
        Ok(None) => {
            tell_unsandboxed(
                &env,
                parsed.name(),
                &format!(
                    "paddock has no profile for {}. Add one in {}",
                    parsed.name(),
                    env.paddock_config().join("harness.d").display()
                ),
            );
            return exec_unchanged(&env, argv);
        }
        Err(e) => return fail(&env, parsed.name(), e),
    };
    match start(&env, plan) {
        Ok(code) => Ok(code),
        Err(e) => fail(&env, parsed.name(), e),
    }
}

/// A start that paddock knows but cannot make: say why, and keep the window
/// open long enough to read it. The agent does not start on the host.
fn fail(env: &HostEnv, name: &str, e: anyhow::Error) -> Result<i32> {
    eprintln!("paddock: {e:#}");
    let _ = crate::broker::notify::show(
        env,
        &crate::broker::notify::Note {
            summary: format!("paddock could not start {name} in a sandbox"),
            body: format!("{e:#}"),
            critical: true,
            ..Default::default()
        },
    );
    eprintln!(
        "paddock: run it without a sandbox with PADDOCK_BYPASS=1, or turn this off with: paddock shims remove"
    );
    wait_for_enter();
    Ok(2)
}

fn start(env: &HostEnv, plan: Plan) -> Result<i32> {
    let harness = match &plan.exe {
        Some(exe) => {
            let f = HarnessFactory::new(env)?;
            let profile = f
                .profile(&plan.profile)
                .cloned()
                .context("the profile disappeared")?;
            let exe = std::fs::canonicalize(exe)
                .with_context(|| format!("cannot resolve {}", exe.display()))?;
            let (install_mounts, path_dirs) = crate::profile::runtime_closure(env, &exe)?;
            crate::profile::Harness {
                profile,
                exe,
                install_mounts,
                path_dirs,
                source: "the command".into(),
            }
        }
        None => HarnessFactory::new(env)?.create(&plan.profile)?,
    };
    let mut built = build::build_with(env, &plan.opts, harness)?;
    if plan.scratch {
        built.spec.notices.push(format!(
            "the home folder is no project, so {} works in its scratch project {}. `paddock files` lists it",
            plan.profile,
            built.spec.project.display()
        ));
    }
    if let Some(a) = &plan.account {
        built
            .spec
            .notices
            .push(format!("the sandbox uses the account in {}", a.display()));
    }
    let backend = crate::run::select_backend(env, &built)?;
    Ok(crate::run::run(env, built, backend.as_ref(), false, false)?.code)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn plain_commands() {
        let p = parse(&v(&["codex", "--approve-for-me", "--", "fix it"])).unwrap();
        assert_eq!(p.program, "codex");
        assert_eq!(p.args, v(&["--approve-for-me", "--", "fix it"]));
        assert!(p.env_set.is_empty() && p.shell.is_empty());
        let p = parse(&v(&["grok", "--permission-mode", "bypassPermissions"])).unwrap();
        assert_eq!(p.args, v(&["--permission-mode", "bypassPermissions"]));
    }

    #[test]
    fn the_account_prefix() {
        let argv = v(&[
            "env",
            "-u",
            "CLAUDE_CONFIG_DIR",
            "OMARCHY_AGENT_CLAUDE_HOME=/home/demo/.local/state/omarchy/agents/accounts/claude/work",
            "CLAUDE_CONFIG_DIR=/home/demo/.local/state/omarchy/agents/accounts/claude/work",
            "claude",
            "--permission-mode",
            "auto",
        ]);
        let p = parse(&argv).unwrap();
        assert_eq!(p.program, "claude");
        assert_eq!(p.env_unset, v(&["CLAUDE_CONFIG_DIR"]));
        assert_eq!(p.env_set.len(), 2);
        assert_eq!(
            p.var("CLAUDE_CONFIG_DIR", Some("/elsewhere")).as_deref(),
            Some("/home/demo/.local/state/omarchy/agents/accounts/claude/work")
        );
        // The main account: an empty pin and no folder.
        let p = parse(&v(&[
            "env",
            "-u",
            "CODEX_HOME",
            "OMARCHY_AGENT_CODEX_HOME=",
            "codex",
            "--approve-for-me",
        ]))
        .unwrap();
        assert_eq!(p.var("CODEX_HOME", Some("/home/demo/.codex-old")), None);
        assert_eq!(p.var("OMARCHY_AGENT_CODEX_HOME", None), None);
        assert_eq!(
            p.env_set,
            vec![("OMARCHY_AGENT_CODEX_HOME".into(), String::new())]
        );
        // No prefix: a folder that the host environment sets still counts.
        let p = parse(&v(&["grok"])).unwrap();
        assert_eq!(p.var("GROK_HOME", Some("/x")).as_deref(), Some("/x"));
    }

    #[test]
    fn other_env_prefixes() {
        let p = parse(&v(&[
            "env",
            "DSH_PERMISSION_MODE=danger-full-access",
            "dsh",
            "web",
        ]))
        .unwrap();
        assert_eq!(p.program, "dsh");
        assert_eq!(p.args, v(&["web"]));
        assert_eq!(
            p.env_set,
            vec![("DSH_PERMISSION_MODE".into(), "danger-full-access".into())]
        );
        let p = parse(&v(&[
            "env",
            "-u",
            "HERMES_SESSION_SOURCE",
            "hermes",
            "chat",
            "--yolo",
            "--tui",
            "--query=hi",
        ]))
        .unwrap();
        assert_eq!(p.program, "hermes");
        assert_eq!(p.env_unset, v(&["HERMES_SESSION_SOURCE"]));
        let p = parse(&v(&["env", "-uFOO", "--unset=BAR", "--", "pi"])).unwrap();
        assert_eq!(p.env_unset, v(&["FOO", "BAR"]));
        assert_eq!(p.program, "pi");
        // Options that do more than set variables are not this form.
        assert!(parse(&v(&["env", "-i", "claude"])).is_none());
        assert!(parse(&v(&["env", "-C", "/tmp", "claude"])).is_none());
        assert!(parse(&v(&["env", "1X=y", "claude"])).is_none());
        assert!(parse(&v(&["env"])).is_none());
        assert!(parse(&v(&["env", "-u"])).is_none());
    }

    #[test]
    fn the_shell_form() {
        let script = "printf \"%s\\n\" \"$1\" | exec \"$0\" --dangerously-allow-all";
        let p = parse(&v(&["bash", "-c", script, "amp", "the prompt"])).unwrap();
        assert_eq!(p.program, "amp");
        assert_eq!(p.name(), "amp");
        assert_eq!(p.shell, v(&["bash", "-c", script]));
        assert_eq!(p.args, v(&["the prompt"]));
        let p = parse(&v(&[
            "bash",
            "-c",
            script,
            "/home/demo/.local/bin/amp",
            "x",
        ]))
        .unwrap();
        assert_eq!(p.name(), "amp");
        assert!(parse(&v(&["bash", "script.sh"])).is_none());
        assert!(parse(&v(&["bash", "-c", "true"])).is_none());
    }

    #[test]
    fn absolute_paths_and_launchers() {
        let p = parse(&v(&[
            "/home/demo/.local/bin/claude",
            "--permission-mode",
            "auto",
        ]))
        .unwrap();
        assert_eq!(p.name(), "claude");
        assert_eq!(p.program, "/home/demo/.local/bin/claude");
        let p = parse(&v(&["omarchy-launch-openclaw", "--tui"])).unwrap();
        assert_eq!(p.name(), "omarchy-launch-openclaw");
        assert!(parse(&v(&["./claude"])).is_none());
    }

    fn test_env(tmp: &Path) -> HostEnv {
        let mut vars = std::collections::BTreeMap::new();
        vars.insert(
            "XDG_DATA_HOME".into(),
            tmp.join("data").display().to_string(),
        );
        vars.insert(
            "XDG_CONFIG_HOME".into(),
            tmp.join("config").display().to_string(),
        );
        std::fs::create_dir_all(tmp.join("home/proj")).unwrap();
        HostEnv::from_parts(tmp.join("home"), "demo".into(), 1000, 1000, vars)
    }

    #[test]
    fn the_home_folder_gets_a_scratch_project() {
        let tmp = tempfile::tempdir().unwrap();
        let env = test_env(tmp.path());
        let (p, scratch) = project_for(&env, &env.home, "claude").unwrap();
        assert!(scratch);
        assert!(p.ends_with("data/paddock/scratch/claude"));
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        let (p, scratch) = project_for(&env, &env.home.join("proj"), "claude").unwrap();
        assert!(!scratch);
        assert!(p.ends_with("home/proj"));
        // The root folder contains the home: no project either.
        let (_, scratch) = project_for(&env, Path::new("/"), "codex").unwrap();
        assert!(scratch);
    }

    #[test]
    fn unknown_agents_have_no_profile_and_known_ones_do() {
        let tmp = tempfile::tempdir().unwrap();
        let env = test_env(tmp.path());
        for a in ["claude", "codex", "copilot", "opencode", "agy", "grok"] {
            assert_eq!(profile_for(&env, a).as_deref(), Some(a), "{a}");
        }
        assert_eq!(profile_for(&env, "antigravity").as_deref(), Some("agy"));
        assert_eq!(profile_for(&env, "no-such-agent-here"), None);
        // Desktop apps are no terminal command.
        assert_eq!(profile_for(&env, "vscode"), None);
        let p = parse(&v(&["no-such-agent-here", "--yolo"])).unwrap();
        assert!(plan(&env, &p, &env.home).unwrap().is_none());
    }

    #[test]
    fn the_plan_keeps_the_arguments_and_reads_the_account() {
        let tmp = tempfile::tempdir().unwrap();
        let env = test_env(tmp.path());
        let acct = tmp
            .path()
            .join("home/.local/state/omarchy/agents/accounts/claude/work");
        std::fs::create_dir_all(&acct).unwrap();
        let argv = vec![
            "env".to_string(),
            "-u".into(),
            "CLAUDE_CONFIG_DIR".into(),
            format!("OMARCHY_AGENT_CLAUDE_HOME={}", acct.display()),
            format!("CLAUDE_CONFIG_DIR={}", acct.display()),
            "claude".into(),
            "--permission-mode".into(),
            "auto".into(),
        ];
        let p = parse(&argv).unwrap();
        let pl = plan(&env, &p, &env.home.join("proj")).unwrap().unwrap();
        assert_eq!(pl.profile, "claude");
        assert!(pl.opts.no_yolo);
        assert_eq!(pl.opts.args, v(&["--permission-mode", "auto"]));
        assert_eq!(
            pl.account.as_deref(),
            Some(acct.canonicalize().unwrap().as_path())
        );
        let t = pl.opts.transparent.unwrap();
        // The pin stays; the folder variable is not passed in.
        assert!(t
            .env_set
            .iter()
            .any(|(k, v)| k == "OMARCHY_AGENT_CLAUDE_HOME" && *v == acct.display().to_string()));
        assert!(!t.env_set.iter().any(|(k, _)| k == "CLAUDE_CONFIG_DIR"));
        assert!(t.env_unset.contains(&"CLAUDE_CONFIG_DIR".to_string()));
        // The default folder is the shared login.
        std::fs::create_dir_all(env.home.join(".codex")).unwrap();
        let p = parse(&[
            "env".to_string(),
            format!("CODEX_HOME={}", env.home.join(".codex").display()),
            "codex".into(),
        ])
        .unwrap();
        let plan2 = plan(&env, &p, &env.home.join("proj")).unwrap().unwrap();
        assert!(plan2.account.is_none());
    }
}
