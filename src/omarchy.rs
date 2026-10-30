//! `paddock shims install --scope omarchy-agent`: only the agents that
//! Omarchy's `omarchy-agent` starts run in a sandbox.
//!
//! - One marked block in the user mise config declares a mise wrapper for
//!   `omarchy-agent`. Omarchy puts mise's wrapper folder first on PATH, so
//!   every start from a key, the menu, the agents panel or the alias `a`
//!   reaches `paddock shims exec omarchy-agent`.
//! - That runs the real `omarchy-agent`, unchanged, with one private folder
//!   first on PATH for its process tree only. The folder holds a shim for
//!   `omarchy-launch-tui`, which puts `paddock wrap --` in front of the agent
//!   command (wrap.rs).
//! - `--inline` takes the same path: paddock removes the flag and marks the
//!   start, and the shim runs `paddock wrap` in the current terminal instead
//!   of a new one. `omarchy-agent` builds the command the same way for both
//!   paths (only its last line differs), so paddock never copies its logic,
//!   also not the case of a user install in `~/.local/bin`.
//!
//! Commands that the user types and scripts do not change: nothing else is on
//! PATH, and Omarchy's own files stay as they are.

use crate::env::HostEnv;
use anyhow::{bail, Context, Result};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const SCOPE: &str = "omarchy-agent";
/// The app id that `omarchy-agent` gives every agent window.
pub const APP_ID_ARG: &str = "--app-id=org.omarchy.agent";
/// Set by `shims exec` when the start had `--inline`; read and removed by
/// the `omarchy-launch-tui` shim.
pub const INLINE_VAR: &str = "PADDOCK_OMARCHY_INLINE";

const BEGIN: &str =
    "# paddock shims: begin (managed by paddock, remove with `paddock shims remove`)";
const END: &str = "# paddock shims: end";

/// The private shim folder.
pub fn shim_root(env: &HostEnv) -> PathBuf {
    env.paddock_data().join("shims").join(SCOPE)
}

pub fn shim_bin(env: &HostEnv) -> PathBuf {
    shim_root(env).join("bin")
}

/// The user mise config that holds the block. The same file that mise reads
/// as its global config.
pub fn mise_config(env: &HostEnv) -> PathBuf {
    if let Some(f) = env
        .var("MISE_GLOBAL_CONFIG_FILE")
        .filter(|v| v.starts_with('/'))
    {
        return PathBuf::from(f);
    }
    let dir = env
        .var("MISE_CONFIG_DIR")
        .filter(|v| v.starts_with('/'))
        .map(PathBuf::from)
        .unwrap_or_else(|| env.config_dir.join("mise"));
    dir.join("config.toml")
}

/// mise's wrapper folder, which Omarchy puts first on PATH.
pub fn mise_wrapper_dir(env: &HostEnv) -> PathBuf {
    mise_data(env).join("command-wrappers").join("bin")
}

fn mise_data(env: &HostEnv) -> PathBuf {
    env.var("MISE_DATA_DIR")
        .filter(|v| v.starts_with('/'))
        .map(PathBuf::from)
        .unwrap_or_else(|| env.data_dir.join("mise"))
}

fn toml_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_default()
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The block for the mise config. The wrapper runs paddock while it is
/// installed, and the real `omarchy-agent` otherwise (mise takes its own
/// wrapper folder off PATH first), so a removed paddock never stops the key.
pub fn block(paddock: &Path) -> String {
    let script = format!(
        "p={}; if [ -x \"$p\" ] && [ -z \"${{PADDOCK_BYPASS:-}}\" ]; then exec \"$p\" shims exec omarchy-agent \"$@\"; fi; exec omarchy-agent \"$@\"",
        sh_quote(&paddock.display().to_string())
    );
    format!(
        "{BEGIN}\n[wrappers.omarchy-agent]\ncommand = \"sh\"\nargs = [\"-c\", {}, \"omarchy-agent\"]\n{END}\n",
        toml_str(&script)
    )
}

/// The text without paddock's block and the line break that `insert` put
/// before it, so the file is the same as before, byte for byte.
pub fn strip_block(text: &str) -> String {
    let Some(start) = text.find(BEGIN) else {
        return text.to_string();
    };
    let Some(end_rel) = text[start..].find(END) else {
        return text.to_string();
    };
    let mut end = start + end_rel + END.len();
    if text[end..].starts_with('\n') {
        end += 1;
    }
    // `insert` adds one line break: an empty line after a file that ends
    // with a line break, or the missing final line break.
    let begin = if text[..start].ends_with('\n') {
        start - 1
    } else {
        start
    };
    format!("{}{}", &text[..begin], &text[end..])
}

/// The text with paddock's block at the end, after one added line break
/// (see `strip_block`).
pub fn insert(text: &str, block: &str) -> String {
    let base = strip_block(text);
    if base.is_empty() {
        return block.to_string();
    }
    format!("{base}\n{block}")
}

/// Whether the mise config, outside paddock's block, declares its own
/// wrapper for `omarchy-agent`.
pub fn user_wrapper(text: &str) -> Result<bool> {
    let rest = strip_block(text);
    let doc: toml_edit::DocumentMut = rest.parse().context("the mise config is not valid TOML")?;
    Ok(doc.get("wrappers").and_then(|w| w.get(SCOPE)).is_some())
}

/// The state of the scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub block: bool,
    pub shims: bool,
    /// The user's own `[wrappers.omarchy-agent]` outside the block.
    pub user_wrapper: bool,
    /// mise's dispatch link for `omarchy-agent` exists.
    pub dispatch: bool,
}

impl Status {
    pub fn on(&self) -> bool {
        self.block && self.shims && !self.user_wrapper
    }
}

pub fn status(env: &HostEnv) -> Status {
    let text = std::fs::read_to_string(mise_config(env)).unwrap_or_default();
    Status {
        block: text.contains(BEGIN),
        shims: shim_bin(env).join("omarchy-launch-tui").is_file(),
        user_wrapper: user_wrapper(&text).unwrap_or(false),
        dispatch: std::fs::symlink_metadata(mise_wrapper_dir(env).join(SCOPE)).is_ok(),
    }
}

/// The shim for `omarchy-launch-tui`. It runs paddock while it is installed,
/// and the real launcher otherwise.
pub fn launch_tui_shim(paddock: &Path) -> String {
    format!(
        "#!/bin/sh\n\
         # Written by `paddock shims install --scope omarchy-agent`; edits here are replaced.\n\
         # Only on PATH while omarchy-agent runs: agent windows start through `paddock wrap`.\n\
         p={}\n\
         if [ -x \"$p\" ]; then exec \"$p\" shims launch-tui \"$@\"; fi\n\
         d=$(dirname \"$0\")\n\
         PATH=$(printf '%s' \"$PATH\" | tr ':' '\\n' | grep -vxF \"$d\" | paste -sd: -)\n\
         exec omarchy-launch-tui \"$@\"\n",
        sh_quote(&paddock.display().to_string())
    )
}

fn write_exec(path: &Path, text: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    crate::privfs::write_atomic(path, text)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

/// The result of `mise reshim`: an error message when it did not run.
pub type Reshim = Result<(), String>;

pub struct Report {
    pub config: PathBuf,
    pub shims: PathBuf,
    pub reshim: Result<(), String>,
}

/// Run `mise reshim`, so that mise makes or removes the dispatch link.
fn reshim(env: &HostEnv) -> Result<(), String> {
    let Some(mise) = env.which("mise") else {
        return Err("mise is not on PATH".into());
    };
    let out = crate::mise::host_command(&mise)
        .arg("reshim")
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

pub fn install(env: &HostEnv) -> Result<Report> {
    let paddock = crate::launchers::launcher_exe(env)?;
    let config = mise_config(env);
    let text = match std::fs::read_to_string(&config) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", config.display())),
    };
    if user_wrapper(&text).with_context(|| config.display().to_string())? {
        bail!(
            "{} already has its own [wrappers.omarchy-agent]. paddock does not replace it. \
             Remove it, or keep it and start agents with `paddock wrap -- COMMAND` from it",
            config.display()
        );
    }
    let new = insert(&text, &block(&paddock));
    // The result must still be a valid mise config, for example when the
    // file has `wrappers` as an inline table that a new table cannot extend.
    new.parse::<toml_edit::DocumentMut>().with_context(|| {
        format!(
            "paddock's block would make {} invalid; nothing was changed",
            config.display()
        )
    })?;
    let bin = shim_bin(env);
    crate::privfs::dir(&bin)?;
    write_exec(&bin.join("omarchy-launch-tui"), &launch_tui_shim(&paddock))?;
    if new != text {
        if let Some(d) = config.parent() {
            std::fs::create_dir_all(d)?;
        }
        write_keep_mode(&config, &new)?;
    }
    Ok(Report {
        config,
        shims: bin,
        reshim: reshim(env),
    })
}

/// Write the user's file atomically and keep its mode.
fn write_keep_mode(path: &Path, text: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o7777)
        .unwrap_or(0o644);
    let tmp = path.with_extension("paddock-tmp");
    let _ = std::fs::remove_file(&tmp);
    std::fs::write(&tmp, text)?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Remove the block and the shim folder. Returns what changed, and the
/// result of `mise reshim` when the block was there.
pub fn remove(env: &HostEnv) -> Result<(Vec<PathBuf>, Option<Reshim>)> {
    let mut removed = Vec::new();
    let config = mise_config(env);
    let mut reshim_result = None;
    if let Ok(text) = std::fs::read_to_string(&config) {
        let clean = strip_block(&text);
        if clean != text {
            write_keep_mode(&config, &clean)?;
            removed.push(config);
            reshim_result = Some(reshim(env));
        }
    }
    let root = shim_root(env);
    if root.exists() {
        crate::safefs::remove_tree_path(&root)?;
        removed.push(root);
    }
    Ok((removed, reshim_result))
}

/// PATH without paddock's shim folder.
pub fn strip_shims(env: &HostEnv, path: &[PathBuf]) -> Vec<PathBuf> {
    let root = shim_root(env);
    path.iter()
        .filter(|d| !d.starts_with(&root))
        .cloned()
        .collect()
}

/// A mise wrapper or shim: a link to mise in one of its dispatch folders.
fn is_mise_dispatch(env: &HostEnv, p: &Path) -> bool {
    let dir = p.parent().unwrap_or(Path::new("/"));
    if dir == mise_wrapper_dir(env) || dir == mise_data(env).join("shims") {
        return true;
    }
    std::fs::canonicalize(p)
        .ok()
        .and_then(|r| r.file_name().map(|n| n == "mise"))
        .unwrap_or(false)
}

/// The real program `name` on `path`: not paddock's shim, not a mise wrapper.
pub fn real_program(env: &HostEnv, path: &[PathBuf], name: &str) -> Option<PathBuf> {
    strip_shims(env, path)
        .into_iter()
        .filter(|d| d.is_absolute())
        .map(|d| d.join(name))
        .find(|p| crate::env::is_executable(p) && !is_mise_dispatch(env, p))
}

fn join_path(dirs: &[PathBuf]) -> String {
    std::env::join_paths(dirs)
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The PATH for the process tree of `omarchy-agent`: the shim folder first,
/// then what the session had. mise removes its wrapper folder and its shims
/// before it runs a wrapper; they come back the way Omarchy's account
/// wrapper restores them, so the agents still find their tools.
pub fn agent_path(env: &HostEnv) -> Vec<PathBuf> {
    let mut rest = strip_shims(env, &env.path);
    let wrappers = mise_wrapper_dir(env);
    let shims = mise_data(env).join("shims");
    let mut out = vec![shim_bin(env)];
    if wrappers.is_dir() && !rest.contains(&wrappers) {
        out.push(wrappers);
    }
    let had_shims = rest.contains(&shims);
    out.append(&mut rest);
    if shims.is_dir() && !had_shims {
        out.push(shims);
    }
    out
}

/// `paddock shims exec omarchy-agent ARGS`: run the real `omarchy-agent`
/// with the shim folder first on PATH. Returns only on an error.
pub fn exec_agent(env: &HostEnv, args: &[String]) -> Result<i32> {
    let real = real_program(env, &env.path, SCOPE).with_context(|| {
        "cannot find the real omarchy-agent on PATH (paddock skips mise's wrappers and its own shims)"
    })?;
    let mut cmd = Command::new(&real);
    let bypass =
        env.var("PADDOCK_BYPASS").is_some_and(|v| !v.is_empty()) || env.var("PADDOCK") == Some("1");
    if bypass || !shim_bin(env).join("omarchy-launch-tui").is_file() {
        cmd.args(args);
        cmd.env("PATH", join_path(&strip_shims(env, &env.path)));
    } else {
        let inline = args.iter().any(|a| a == "--inline");
        cmd.args(args.iter().filter(|a| *a != "--inline"));
        cmd.env("PATH", join_path(&agent_path(env)));
        if inline {
            cmd.env(INLINE_VAR, "1");
        } else {
            cmd.env_remove(INLINE_VAR);
        }
    }
    let e = cmd.exec();
    Err(anyhow::Error::from(e).context(format!("cannot run {}", real.display())))
}

/// What the `omarchy-launch-tui` shim does with its arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchPlan {
    /// Run the real launcher with these arguments.
    Launcher(Vec<String>),
    /// Run `paddock wrap -- COMMAND` in the current terminal (`--inline`).
    Inline(Vec<String>),
    /// Run the command itself in the current terminal (`--inline` with
    /// PADDOCK_BYPASS).
    Direct(Vec<String>),
}

pub fn launch_plan(args: &[String], inline: bool, bypass: bool, paddock: &Path) -> LaunchPlan {
    let ours = args.first().map(String::as_str) == Some(APP_ID_ARG) && args.len() > 1;
    match (ours, inline, bypass) {
        (true, true, true) => LaunchPlan::Direct(args[1..].to_vec()),
        (true, true, false) => LaunchPlan::Inline(args[1..].to_vec()),
        (true, false, false) => {
            let mut a = vec![
                APP_ID_ARG.to_string(),
                paddock.display().to_string(),
                "wrap".into(),
                "--".into(),
            ];
            a.extend(args[1..].iter().cloned());
            LaunchPlan::Launcher(a)
        }
        _ => LaunchPlan::Launcher(args.to_vec()),
    }
}

/// `paddock shims launch-tui ARGS`, from the shim. Returns only on an error.
pub fn launch_tui(env: &HostEnv, args: &[String]) -> Result<i32> {
    let inline = env.var(INLINE_VAR) == Some("1");
    let bypass = env.var("PADDOCK_BYPASS").is_some_and(|v| !v.is_empty());
    let paddock = crate::launchers::launcher_exe(env)?;
    let path = join_path(&strip_shims(env, &env.path));
    let (program, rest) = match launch_plan(args, inline, bypass, &paddock) {
        LaunchPlan::Launcher(a) => (
            real_program(env, &env.path, "omarchy-launch-tui")
                .context("cannot find the real omarchy-launch-tui on PATH")?,
            a,
        ),
        LaunchPlan::Inline(a) => {
            let mut w = vec!["wrap".to_string(), "--".into()];
            w.extend(a);
            (paddock, w)
        }
        LaunchPlan::Direct(a) => (PathBuf::from(&a[0]), a[1..].to_vec()),
    };
    let e = Command::new(&program)
        .args(&rest)
        .env("PATH", path)
        .env_remove(INLINE_VAR)
        .exec();
    Err(anyhow::Error::from(e).context(format!("cannot run {}", program.display())))
}

/// What `bash -lc 'command -v NAME'` finds when it starts with Omarchy's bin
/// folder first on PATH, as the Omarchy shell does.
fn login_shell_resolves(env: &HostEnv, name: &str) -> Option<PathBuf> {
    let bash = env.which("bash")?;
    let mut path = vec![PathBuf::from("/usr/share/omarchy/bin")];
    path.extend(env.path.iter().cloned());
    let joined = std::env::join_paths(path).ok()?;
    let out = std::process::Command::new(bash)
        .args(["-lc", "command -v -- \"$0\"", name])
        .env("PATH", joined)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && p.starts_with('/')).then(|| PathBuf::from(p))
}

/// The program that a command from `omarchy-agent` starts first, so doctor
/// can check that `omarchy-agent` still starts agents the way paddock knows.
pub fn agent_script_checks(text: &str) -> Vec<(&'static str, bool)> {
    vec![
        (
            "starts agent windows with `exec omarchy-launch-tui --app-id=org.omarchy.agent`",
            text.contains("exec omarchy-launch-tui --app-id=org.omarchy.agent"),
        ),
        (
            "builds one command for both paths and runs it inline with `exec \"${command[@]}\"`",
            text.contains("exec \"${command[@]}\""),
        ),
    ]
}

/// The default agent of Omarchy, from `~/.config/omarchy/defaults/agent`
/// (Omarchy reads it there, not under XDG_CONFIG_HOME).
pub fn default_agent(env: &HostEnv) -> Option<String> {
    let text = std::fs::read_to_string(env.home.join(".config/omarchy/defaults/agent")).ok()?;
    let a = text.lines().next()?.trim().to_string();
    (!a.is_empty()).then_some(a)
}

/// Lines for `paddock doctor`.
pub fn doctor_lines(env: &HostEnv) -> Vec<String> {
    let s = status(env);
    let mut out = Vec::new();
    let config = mise_config(env).display().to_string();
    let config = config.replacen(&env.home.display().to_string(), "~", 1);
    if s.user_wrapper {
        out.push(format!(
            "  CHECK    {config} has its own [wrappers.omarchy-agent]; paddock does not replace it"
        ));
    }
    if !s.block && !s.shims {
        out.push("  off      agents from omarchy-agent run without a sandbox; paddock shims install --scope omarchy-agent changes that".into());
        return out;
    }
    let mark = |ok: bool| if ok { "  ok      " } else { "  CHECK   " };
    out.push(format!(
        "{} the omarchy-agent wrapper in {config}",
        mark(s.block && !s.user_wrapper)
    ));
    out.push(format!(
        "{} the shim folder {}",
        mark(s.shims),
        shim_bin(env)
            .display()
            .to_string()
            .replacen(&env.home.display().to_string(), "~", 1)
    ));
    out.push(format!(
        "{} mise's dispatch link for omarchy-agent{}",
        mark(s.dispatch),
        if s.dispatch {
            ""
        } else {
            " is missing: run mise reshim"
        }
    ));
    // Omarchy starts agents from Hyprland (key, console) and from its shell
    // through `bash -lc` (agents panel, prompts, crash reports). The shell
    // itself puts Omarchy's bin folder first on PATH, so check what a login
    // shell started from that order really runs.
    let wrappers = mise_wrapper_dir(env);
    let resolved = login_shell_resolves(env, SCOPE);
    let ok = resolved.as_ref().is_some_and(|p| p.starts_with(&wrappers));
    out.push(format!(
        "{} a login shell from the Omarchy shell's PATH runs omarchy-agent through mise{}",
        mark(ok),
        match (&resolved, ok) {
            (_, true) => String::new(),
            (Some(p), false) => format!(
                ": it runs {}, so starts from the agents panel bypass paddock",
                p.display()
            ),
            (None, false) => ": the check could not run `bash -lc`".into(),
        }
    ));
    match real_program(env, &env.path, SCOPE) {
        Some(p) => {
            let text = std::fs::read_to_string(&p).unwrap_or_default();
            for (what, ok) in agent_script_checks(&text) {
                out.push(format!(
                    "{} omarchy-agent {what}{}",
                    mark(ok),
                    if ok {
                        ""
                    } else {
                        ": it changed, and paddock may not see its starts"
                    }
                ));
            }
        }
        None => out.push("  CHECK    omarchy-agent is not on PATH".into()),
    }
    if let Some(agent) = default_agent(env) {
        match crate::wrap::profile_for(env, &agent) {
            Some(p) => out.push(format!(
                "  ok       the default agent {agent} has the profile {p}"
            )),
            None => out.push(format!(
                "  CHECK    the default agent {agent} has no paddock profile: its starts run without a sandbox"
            )),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const USER: &str =
        "# my settings\n[settings]\nexperimental = true # keep\n\n[tools]\nnode = \"24\"\n";

    fn blk() -> String {
        block(Path::new("/opt/pad dock/paddock"))
    }

    #[test]
    fn the_block_is_added_once_and_removed_byte_for_byte() {
        for original in [
            USER.to_string(),
            USER.trim_end().to_string(),
            String::new(),
            "\n".to_string(),
            "[tools]\nnode = \"24\"\n\n\n".to_string(),
        ] {
            let once = insert(&original, &blk());
            let twice = insert(&once, &blk());
            assert_eq!(once, twice, "not idempotent for {original:?}");
            assert_eq!(once.matches(BEGIN).count(), 1);
            assert!(
                once.starts_with(original.trim_end_matches('\n')) || original.trim().is_empty()
            );
            once.parse::<toml_edit::DocumentMut>().unwrap();
            assert_eq!(strip_block(&once), original, "removal changed {original:?}");
        }
    }

    #[test]
    fn the_block_keeps_comments_and_order_and_declares_the_wrapper() {
        let text = insert(USER, &blk());
        assert!(text.starts_with(USER));
        let doc: toml_edit::DocumentMut = text.parse().unwrap();
        let w = &doc["wrappers"]["omarchy-agent"];
        assert_eq!(w["command"].as_str(), Some("sh"));
        let args: Vec<&str> = w["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(args[0], "-c");
        assert!(args[1].contains("'/opt/pad dock/paddock'"));
        assert!(args[1].contains("shims exec omarchy-agent \"$@\""));
        assert!(args[1].ends_with("exec omarchy-agent \"$@\""));
        assert_eq!(args[2], "omarchy-agent");
        // The user's content is not paddock's block.
        assert!(!user_wrapper(&text).unwrap());
    }

    #[test]
    fn a_user_wrapper_outside_the_block_is_found() {
        let own = "[wrappers.omarchy-agent]\ncommand = \"mine\"\n";
        assert!(user_wrapper(own).unwrap());
        assert!(user_wrapper(&insert(own, &blk())).unwrap());
        let inline = "[wrappers]\nomarchy-agent = { command = \"mine\" }\n";
        assert!(user_wrapper(inline).unwrap());
        assert!(!user_wrapper("[wrappers.claude]\ncommand = \"x\"\n").unwrap());
        assert!(user_wrapper("not toml [").is_err());
    }

    #[test]
    fn a_block_that_cannot_extend_the_file_is_detected() {
        let text = "wrappers = { claude = { command = \"x\" } }\n";
        let new = insert(text, &blk());
        assert!(new.parse::<toml_edit::DocumentMut>().is_err());
    }

    #[test]
    fn the_launcher_shim_wraps_only_agent_windows() {
        let p = Path::new("/usr/bin/paddock");
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            launch_plan(
                &a(&[APP_ID_ARG, "codex", "--approve-for-me"]),
                false,
                false,
                p
            ),
            LaunchPlan::Launcher(a(&[
                APP_ID_ARG,
                "/usr/bin/paddock",
                "wrap",
                "--",
                "codex",
                "--approve-for-me"
            ]))
        );
        assert_eq!(
            launch_plan(&a(&[APP_ID_ARG, "codex"]), true, false, p),
            LaunchPlan::Inline(a(&["codex"]))
        );
        assert_eq!(
            launch_plan(&a(&[APP_ID_ARG, "codex"]), true, true, p),
            LaunchPlan::Direct(a(&["codex"]))
        );
        // Other TUIs, and the agent with PADDOCK_BYPASS, run unchanged.
        for (args, inline, bypass) in [
            (a(&["btop"]), false, false),
            (a(&["--app-id=org.omarchy.other", "btop"]), true, false),
            (a(&[APP_ID_ARG, "codex"]), false, true),
        ] {
            assert_eq!(
                launch_plan(&args, inline, bypass, p),
                LaunchPlan::Launcher(args.clone())
            );
        }
    }
}
