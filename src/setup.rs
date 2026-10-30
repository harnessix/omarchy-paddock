//! `paddock setup`: the first-run wizard.
//!
//! Each step shows the current state, says what it would change and how to
//! undo it, and then changes something only when the user asks: with a flag
//! (`--shims`, `--no-launchers`, `--backend podman`, `--app NAME=DIR`), or
//! with a yes on a terminal. With `--yes`, or without a terminal, nothing is
//! asked, and only the flags change something. A second run with the same
//! flags changes nothing.
//!
//! The wizard never runs sudo on its own: it prints the package command, and
//! runs it only after a yes on a terminal.

use crate::doctor;
use crate::env::HostEnv;
use crate::launchers::{self, Kind};
use crate::shims;
use crate::update;
use anyhow::{bail, Context, Result};
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

pub const BACKENDS: &[&str] = &["auto", "bwrap", "podman", "docker", "srt"];

/// Questions to the user. `None` in [`run`] means: ask nothing.
pub trait Ask {
    fn yes_no(&mut self, question: &str, default: bool) -> bool;
    fn line(&mut self, question: &str) -> String;
}

/// Questions on the terminal.
pub struct Terminal;

impl Terminal {
    /// A terminal, when stdin and stderr are one.
    pub fn detect() -> Option<Self> {
        (std::io::stdin().is_terminal() && std::io::stderr().is_terminal()).then_some(Self)
    }
}

impl Ask for Terminal {
    fn yes_no(&mut self, question: &str, default: bool) -> bool {
        let hint = if default { "[Y/n]" } else { "[y/N]" };
        let a = self.line(&format!("{question} {hint} "));
        match a.trim() {
            "" => default,
            a => matches!(a, "y" | "Y" | "yes" | "Yes"),
        }
    }
    fn line(&mut self, question: &str) -> String {
        eprint!("   {question}");
        std::io::stderr().flush().ok();
        let mut s = String::new();
        let _ = std::io::stdin().lock().read_line(&mut s);
        s.trim().to_string()
    }
}

#[derive(Debug, Default, Clone)]
pub struct Opts {
    pub shims: Option<bool>,
    pub launchers: Option<bool>,
    pub plugin: Option<bool>,
    pub backend: Option<String>,
    /// `[apps.NAME] roots` to write, from `--app NAME=DIR`.
    pub app_roots: Vec<(String, Vec<PathBuf>)>,
}

/// What a run changed, one line per change.
#[derive(Debug, Default)]
pub struct Outcome {
    pub changes: Vec<String>,
}

fn config_file(env: &HostEnv) -> PathBuf {
    env.paddock_config().join("config.toml")
}

fn tilde(env: &HostEnv, p: &Path) -> String {
    let s = p.display().to_string();
    let home = env.home.display().to_string();
    match s.strip_prefix(&home) {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => format!("~{rest}"),
        _ => s,
    }
}

/// Packages that `paddock doctor` reports missing: (required, optional).
fn missing(env: &HostEnv) -> (Vec<&'static str>, Vec<&'static str>) {
    let fam = doctor::family();
    let (mut req, mut opt) = (Vec::new(), Vec::new());
    for d in doctor::DEPS {
        if env.which(d.cmd).is_some() {
            continue;
        }
        let list = if d.required { &mut req } else { &mut opt };
        if !list.contains(&d.pkg[fam]) {
            list.push(d.pkg[fam]);
        }
    }
    (req, opt)
}

fn remove_command(pkgs: &[&str]) -> String {
    let list = pkgs.join(" ");
    match doctor::family() {
        0 => format!("sudo pacman -Rs {list}"),
        1 => format!("sudo apt remove {list}"),
        _ => format!("sudo dnf remove {list}"),
    }
}

fn packages(env: &HostEnv, ask: &mut Option<&mut dyn Ask>, out: &mut Outcome) -> Result<()> {
    println!("1. System packages (paddock doctor)");
    let (req, opt) = missing(env);
    if req.is_empty() {
        println!("   ok       every required package is installed");
    } else {
        let cmd = doctor::install_command(&req);
        println!("   MISSING  {}", req.join(" "));
        println!("            install them with: {cmd}");
        println!("            undo: {}", remove_command(&req));
        let run = match ask {
            Some(a) => a.yes_no(
                "Run this command now? It asks for your sudo password.",
                false,
            ),
            None => false,
        };
        if run {
            let argv: Vec<&str> = cmd.split_whitespace().collect();
            let st = Command::new(argv[0]).args(&argv[1..]).status()?;
            if !st.success() {
                bail!("{cmd} failed with {st}");
            }
            out.changes.push(format!("installed {}", req.join(" ")));
        } else if ask.is_none() {
            println!("            paddock setup does not run sudo without a terminal: run the command yourself");
        }
    }
    if !opt.is_empty() {
        println!(
            "   absent   optional: {} ({})",
            opt.join(" "),
            doctor::install_command(&opt)
        );
    }
    Ok(())
}

/// The common part of an on/off step. Asks only when the step is off and no
/// flag decides: a step that is on stays on unless a flag turns it off.
fn wanted(
    on: bool,
    flag: Option<bool>,
    ask: &mut Option<&mut dyn Ask>,
    question: &str,
) -> Option<bool> {
    if flag.is_some() {
        return flag;
    }
    if on {
        return None;
    }
    ask.as_mut().map(|a| a.yes_no(question, true))
}

fn shims_step(
    env: &HostEnv,
    o: &Opts,
    ask: &mut Option<&mut dyn Ask>,
    out: &mut Outcome,
) -> Result<()> {
    println!("\n2. Sandbox by default (shell commands)");
    println!(
        "   Typing claude, codex or another harness command in bash or zsh starts it in a sandbox.\n   It changes one marked line in ~/.bashrc and ~/.zshrc, and writes {}.\n   Undo: paddock shims remove, or paddock setup --no-shims",
        tilde(env, &shims::script_file(env))
    );
    let on = shims::installed(env);
    match (on, wanted(on, o.shims, ask, "Turn the shell commands on?")) {
        (false, Some(true)) => {
            let r = shims::install(env)?;
            let cmds: Vec<&str> = r.shims.iter().map(|(_, c)| c.as_str()).collect();
            println!(
                "   on       {}",
                if cmds.is_empty() {
                    "no harness is installed yet; run paddock shims install after you install one"
                        .to_string()
                } else {
                    format!("these commands start in a sandbox: {}", cmds.join(", "))
                }
            );
            if r.rc_files.is_empty() {
                println!(
                    "            no ~/.bashrc or ~/.zshrc: source {} from your shell configuration",
                    tilde(env, &shims::script_file(env))
                );
            }
            out.changes.push("turned on the shell commands".into());
        }
        (true, Some(false)) => {
            shims::remove(env)?;
            println!("   off      removed; open a new shell");
            out.changes.push("turned off the shell commands".into());
        }
        (true, _) => {
            let cmds = shims::active(env);
            if cmds.is_empty() {
                println!("   on       already on; no harness command yet (paddock shims install adds new ones)");
            } else {
                println!("   on       already on: {}", cmds.join(", "));
            }
        }
        (false, _) => println!("   off      not changed; turn on with: paddock setup --shims"),
    }
    Ok(())
}

fn launchers_step(
    env: &HostEnv,
    o: &Opts,
    ask: &mut Option<&mut dyn Ask>,
    out: &mut Outcome,
) -> Result<()> {
    println!("\n3. Launchers");
    println!(
        "   A \"Paddock\" folder in the app menu with an entry for each harness and app.\n   It writes desktop entries in {}, and a marked block in the Omarchy menu file where it exists.\n   Undo: paddock launchers remove, or paddock setup --no-launchers",
        tilde(env, &env.data_dir.join("applications"))
    );
    let on = launchers::installed(env);
    match (on, wanted(on, o.launchers, ask, "Add the launchers?")) {
        (false, Some(true)) => {
            let r = launchers::install(env)?;
            println!("   on       {} entries", r.entries.len());
            out.changes.push("added the launchers".into());
        }
        (true, Some(false)) => {
            launchers::remove(env)?;
            println!("   off      removed");
            out.changes.push("removed the launchers".into());
        }
        (true, _) => println!("   on       already installed"),
        (false, _) => println!("   off      not changed; turn on with: paddock setup --launchers"),
    }
    Ok(())
}

fn plugin_step(
    env: &HostEnv,
    o: &Opts,
    ask: &mut Option<&mut dyn Ask>,
    out: &mut Outcome,
) -> Result<()> {
    println!("\n4. Omarchy bar widget");
    let Some(omarchy) = env.which("omarchy") else {
        println!("   skipped  Omarchy is not installed");
        if o.plugin == Some(true) {
            println!("            --plugin needs the omarchy command");
        }
        return Ok(());
    };
    println!(
        "   Running sandboxes and their host requests in the bar. It runs: omarchy plugin add {} --enable\n   Undo: omarchy plugin remove {}, or paddock setup --no-plugin",
        update::REPO_URL,
        update::PLUGIN_ID
    );
    let on = update::plugin_installed(env).unwrap_or(false);
    let quiet = ask.is_none();
    match (on, wanted(on, o.plugin, ask, "Add the bar widget?")) {
        (false, Some(true)) => {
            let mut c = Command::new(&omarchy);
            c.args(["plugin", "add", update::REPO_URL, "--enable"]);
            if quiet {
                c.arg("--yes");
            }
            let st = c.status()?;
            if !st.success() {
                bail!("omarchy plugin add failed with {st}");
            }
            out.changes.push("added the bar widget".into());
        }
        (true, Some(false)) => {
            let st = Command::new(&omarchy)
                .args(["plugin", "remove", update::PLUGIN_ID, "--yes"])
                .status()?;
            if !st.success() {
                bail!("omarchy plugin remove failed with {st}");
            }
            out.changes.push("removed the bar widget".into());
        }
        (true, _) => println!("   on       already installed; paddock update also updates it"),
        (false, _) => println!("   off      not changed; turn on with: paddock setup --plugin"),
    }
    Ok(())
}

/// The `backend` key of the user configuration, or `auto`.
pub fn current_backend(env: &HostEnv) -> Result<String> {
    let text = std::fs::read_to_string(config_file(env)).unwrap_or_default();
    let doc: toml_edit::DocumentMut = text
        .parse()
        .with_context(|| format!("{} is invalid", config_file(env).display()))?;
    Ok(doc
        .get("backend")
        .and_then(|v| v.as_str())
        .unwrap_or("auto")
        .to_string())
}

/// Set the default backend, keeping the rest of the file. `auto` removes
/// the key.
pub fn set_backend(env: &HostEnv, name: &str) -> Result<()> {
    let file = config_file(env);
    let text = std::fs::read_to_string(&file).unwrap_or_default();
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .with_context(|| format!("{} is invalid", file.display()))?;
    if name == "auto" {
        doc.remove("backend");
    } else {
        doc.insert("backend", toml_edit::value(name));
    }
    std::fs::create_dir_all(file.parent().unwrap())?;
    std::fs::write(&file, doc.to_string())?;
    Ok(())
}

fn backend_step(
    env: &HostEnv,
    o: &Opts,
    ask: &mut Option<&mut dyn Ask>,
    out: &mut Outcome,
) -> Result<()> {
    println!("\n5. Default backend");
    let current = current_backend(env)?;
    println!(
        "   now: {current}. auto picks bwrap: every host tool, and starts in milliseconds. podman or docker\n   suit projects that need system packages the host does not have. It sets backend in {}.\n   Undo: paddock setup --backend {current}",
        tilde(env, &config_file(env))
    );
    let want = match &o.backend {
        Some(b) => Some(b.clone()),
        None => ask
            .as_mut()
            .map(|a| {
                a.line(&format!(
                    "Backend ({}), Enter keeps {current}: ",
                    BACKENDS.join(", ")
                ))
            })
            .filter(|s| !s.is_empty()),
    };
    let Some(want) = want else {
        println!("   ok       not changed; set with: paddock setup --backend NAME");
        return Ok(());
    };
    if !BACKENDS.contains(&want.as_str()) {
        bail!("unknown backend {want}; use one of {}", BACKENDS.join(", "));
    }
    if want == current {
        println!("   ok       already {current}");
        return Ok(());
    }
    set_backend(env, &want)?;
    println!("   changed  {current} -> {want}");
    out.changes.push(format!("default backend {want}"));
    Ok(())
}

fn saved_roots(env: &HostEnv, name: &str) -> Result<Vec<String>> {
    let user = crate::manifest::Manifest::load(env, &env.home.join(".paddock-no-project"))?.merged;
    Ok(user
        .apps
        .get(name)
        .map(|a| a.roots.clone())
        .unwrap_or_default())
}

fn apps_step(
    env: &HostEnv,
    o: &Opts,
    ask: &mut Option<&mut dyn Ask>,
    out: &mut Outcome,
) -> Result<()> {
    println!("\n6. App workspace folders");
    println!(
        "   The folders that a desktop app (VS Code, Claude Desktop, ...) can open, saved as [apps.NAME] roots\n   in {}. Only folders that you name are saved. Undo: delete the roots line of that app",
        tilde(env, &config_file(env))
    );
    let mut todo: Vec<(String, Vec<PathBuf>)> = o.app_roots.clone();
    if let Some(a) = ask.as_mut() {
        for e in launchers::detect(env)? {
            if e.kind != Kind::App
                || todo.iter().any(|(n, _)| n == &e.name)
                || !saved_roots(env, &e.name)?.is_empty()
            {
                continue;
            }
            let line = a.line(&format!(
                "Folders for {} ({}), separated by commas; Enter skips: ",
                e.label, e.name
            ));
            let dirs: Vec<PathBuf> = line
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| env.expand(s, &env.home))
                .collect();
            if !dirs.is_empty() {
                todo.push((e.name.clone(), dirs));
            }
        }
    }
    if todo.is_empty() {
        println!("   ok       no folders named; paddock app run asks for one at the start");
    }
    for (name, dirs) in todo {
        crate::app::app_profile(env, &name)?;
        for d in &dirs {
            if !d.is_dir() {
                bail!("{} is not a folder", d.display());
            }
        }
        let resolved = crate::app::roots(env, &name, &dirs)?;
        let new: Vec<String> = resolved.iter().map(|p| p.display().to_string()).collect();
        let old = saved_roots(env, &name)?;
        if old == new {
            println!("   ok       {name}: already {}", new.join(", "));
            continue;
        }
        crate::state::set_app_roots(env, &name, &resolved)?;
        println!("   changed  {name}: {}", new.join(", "));
        out.changes
            .push(format!("{name} folders {}", new.join(", ")));
    }
    Ok(())
}

/// Run the wizard. `ask` is None for `--yes` and without a terminal.
pub fn run(env: &HostEnv, o: &Opts, mut ask: Option<&mut dyn Ask>) -> Result<Outcome> {
    update::refuse_in_sandbox(env, "paddock setup")?;
    if let Some(b) = &o.backend {
        if !BACKENDS.contains(&b.as_str()) {
            bail!("unknown backend {b}; use one of {}", BACKENDS.join(", "));
        }
    }
    let mut out = Outcome::default();
    packages(env, &mut ask, &mut out)?;
    shims_step(env, o, &mut ask, &mut out)?;
    launchers_step(env, o, &mut ask, &mut out)?;
    plugin_step(env, o, &mut ask, &mut out)?;
    backend_step(env, o, &mut ask, &mut out)?;
    apps_step(env, o, &mut ask, &mut out)?;
    println!();
    if out.changes.is_empty() {
        println!("Nothing changed.");
    } else {
        println!("Changed: {}.", out.changes.join("; "));
    }
    println!("Check the result with: paddock doctor. Start a harness in a project with: paddock <harness>, for example paddock codex");
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn env_in(t: &Path) -> HostEnv {
        let mut vars = BTreeMap::new();
        for (k, d) in [
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_DATA_HOME", "data"),
            ("XDG_CACHE_HOME", "cache"),
            ("XDG_STATE_HOME", "state"),
            ("XDG_RUNTIME_DIR", "run"),
        ] {
            vars.insert(k.to_string(), t.join(d).display().to_string());
        }
        let home = t.join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join(".bashrc"), "# mine\nalias ll='ls -l'\n").unwrap();
        HostEnv::from_parts(home, "demo".into(), 1000, 1000, vars)
    }

    /// Every file under `root` and its content.
    fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        let mut out = BTreeMap::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).unwrap().flatten() {
                let p = e.path();
                let m = std::fs::symlink_metadata(&p).unwrap();
                if m.is_dir() {
                    stack.push(p);
                } else {
                    out.insert(p.clone(), std::fs::read(&p).unwrap_or_default());
                }
            }
        }
        out
    }

    /// Questions are a test failure in non-interactive runs; this answers
    /// from a script otherwise.
    struct Script(Vec<String>);
    impl Ask for Script {
        fn yes_no(&mut self, q: &str, _default: bool) -> bool {
            let a = self.line(q);
            a == "y"
        }
        fn line(&mut self, q: &str) -> String {
            // Apps that this host has installed: skip their folders.
            if q.starts_with("Folders for ") {
                return String::new();
            }
            assert!(!self.0.is_empty(), "unexpected question: {q}");
            self.0.remove(0)
        }
    }

    #[test]
    fn setup_is_idempotent() {
        let t = tempfile::tempdir().unwrap();
        let env = env_in(t.path());
        let o = Opts {
            shims: Some(true),
            launchers: Some(true),
            plugin: Some(true),
            backend: Some("podman".into()),
            app_roots: vec![],
        };
        let first = run(&env, &o, None).unwrap();
        assert_eq!(first.changes.len(), 3, "{:?}", first.changes);
        let rc = std::fs::read_to_string(env.home.join(".bashrc")).unwrap();
        assert!(rc.starts_with("# mine\nalias ll='ls -l'\n") && rc.contains("paddock shims"));
        assert!(launchers::installed(&env));
        assert_eq!(current_backend(&env).unwrap(), "podman");

        let before = snapshot(t.path());
        let again = run(&env, &o, None).unwrap();
        assert!(again.changes.is_empty(), "{:?}", again.changes);
        let none = run(&env, &Opts::default(), None).unwrap();
        assert!(none.changes.is_empty(), "{:?}", none.changes);
        // An interactive run asks only about the packages (missing: PATH is
        // empty) and the backend, and changes nothing
        // when the user presses Enter.
        let mut s = Script(vec!["n".into(), String::new()]);
        let asked = run(&env, &Opts::default(), Some(&mut s)).unwrap();
        assert!(asked.changes.is_empty(), "{:?}", asked.changes);
        assert_eq!(snapshot(t.path()), before, "a second run changed files");

        // Flags undo each step.
        let undo = Opts {
            shims: Some(false),
            launchers: Some(false),
            plugin: Some(false),
            backend: Some("auto".into()),
            app_roots: vec![],
        };
        assert_eq!(run(&env, &undo, None).unwrap().changes.len(), 3);
        let rc = std::fs::read_to_string(env.home.join(".bashrc")).unwrap();
        assert_eq!(rc, "# mine\nalias ll='ls -l'\n");
        assert!(!launchers::installed(&env));
        assert_eq!(current_backend(&env).unwrap(), "auto");
        assert!(!std::fs::read_to_string(config_file(&env))
            .unwrap()
            .contains("backend"));
    }

    #[test]
    fn interactive_answers_turn_steps_on() {
        let t = tempfile::tempdir().unwrap();
        let env = env_in(t.path());
        // No sudo, shims yes, launchers no, backend docker. Omarchy is not on PATH,
        // so there is no plugin question.
        let mut s = Script(vec!["n".into(), "y".into(), "n".into(), "docker".into()]);
        let r = run(&env, &Opts::default(), Some(&mut s)).unwrap();
        assert_eq!(r.changes.len(), 2, "{:?}", r.changes);
        assert!(shims::installed(&env));
        assert!(!launchers::installed(&env));
        assert_eq!(current_backend(&env).unwrap(), "docker");
    }

    #[test]
    fn app_folders_are_written_only_when_named() {
        let t = tempfile::tempdir().unwrap();
        let env = env_in(t.path());
        let proj = t.path().join("src/api");
        std::fs::create_dir_all(&proj).unwrap();
        run(&env, &Opts::default(), None).unwrap();
        assert!(
            !config_file(&env).exists(),
            "nothing named, nothing written"
        );
        let o = Opts {
            app_roots: vec![("vscode".into(), vec![proj.clone()])],
            ..Default::default()
        };
        assert_eq!(run(&env, &o, None).unwrap().changes.len(), 1);
        let want = std::fs::canonicalize(&proj).unwrap().display().to_string();
        assert_eq!(saved_roots(&env, "vscode").unwrap(), vec![want]);
        assert!(run(&env, &o, None).unwrap().changes.is_empty());
        // The home directory, a terminal harness and a missing folder are refused.
        for (name, dir) in [
            ("vscode", env.home.clone()),
            ("codex", proj.clone()),
            ("vscode", t.path().join("missing")),
        ] {
            let bad = Opts {
                app_roots: vec![(name.into(), vec![dir])],
                ..Default::default()
            };
            assert!(run(&env, &bad, None).is_err());
        }
    }

    #[test]
    fn setup_refuses_inside_a_sandbox_and_bad_backends() {
        let t = tempfile::tempdir().unwrap();
        let mut env = env_in(t.path());
        let bad = Opts {
            backend: Some("chroot".into()),
            ..Default::default()
        };
        assert!(run(&env, &bad, None).is_err());
        env.vars.insert("PADDOCK".into(), "1".into());
        let e = run(&env, &Opts::default(), None).unwrap_err();
        assert!(e.to_string().contains("host only"), "{e}");
    }
}
