use anyhow::Result;
use clap::Subcommand;
use paddock::env::HostEnv;
use paddock::profile::HarnessFactory;
use paddock::spec::Network;
use paddock::state;
use std::path::PathBuf;

#[derive(Subcommand)]
pub(crate) enum AppCmd {
    /// Start an app. Its windows appear on your desktop; it sees only the workspace folders.
    Run {
        name: String,
        /// Workspace folders, read-write. Default: [apps.NAME] roots in the user configuration.
        roots: Vec<PathBuf>,
        /// Bind GPU render nodes (faster rendering).
        #[arg(long)]
        gpu: bool,
        /// none, egress (default) or allowlist.
        #[arg(long, short)]
        network: Option<Network>,
        /// inherit (default) or clean.
        #[arg(long, value_parser = ["inherit", "clean"])]
        env: Option<String>,
        /// shared (default), isolated or isolated:NAME. Most desktop apps keep
        /// their login in the host keyring, which a sandbox cannot use, so they
        /// log in again inside the sandbox in any mode.
        #[arg(long)]
        auth: Option<String>,
        /// bwrap (default), docker, podman or srt. srt needs --network allowlist or none.
        #[arg(long, short)]
        backend: Option<String>,
        /// Open the first workspace folder in a new git worktree on its own
        /// branch. Default: [apps.NAME] worktree in the user configuration.
        #[arg(long)]
        worktree: bool,
    },
    /// Add a launcher entry (Super+Space) that starts the app in a sandbox.
    Install {
        name: String,
        /// Workspace folders to save in the user configuration.
        roots: Vec<PathBuf>,
        /// Do not make paddock the handler of the app's links.
        #[arg(long)]
        no_links: bool,
    },
    /// Remove the launcher entry, and give the app's links back to their old handler.
    Uninstall { name: String },
    /// Give a link (for example claude://...) to the running app sandbox, or start the app with it.
    Open { name: String, link: String },
    /// List the app profiles, whether each is installed, and its folders.
    List,
}

pub(crate) const APP_HELP: &str = "App mode runs a desktop agent app in a sandbox. Its windows are normal
windows on your desktop, but through a restricted Wayland socket: the app
cannot capture the screen, read the clipboard in the background, type into
other windows, or list them. Its session bus is private, inside the sandbox,
with a private keyring for its logins. Links that it opens go to your browser
after your approval, and its notifications appear on your desktop. With
`[apps.NAME] bus = \"host-filtered\"` in ~/.config/paddock/config.toml, its
bus is the host bus, filtered to the desktop portals, notifications and the
tray, without a keyring. It sees only its workspace folders, with the same git hook, secrets and network
rules as `paddock <harness>`. The host passthrough (paddock-host) works too.
Flatpak apps differ in some of this; see below.

Examples:
  paddock app run chatgpt-desktop ~/src/api ~/src/web
  paddock app install claude-desktop ~/src/api
  paddock app list

Electron apps run with --no-sandbox: their own sandbox needs nested user
namespaces, which paddock blocks, so paddock is the boundary. Flatpak apps
(profiles with [app] flatpak = \"ID\") run with flatpak run --sandbox inside
the same network isolation (egress or none), with flatpak's own Wayland
socket and filtered host bus, without the host passthrough or worktrees.
Their folders get the same path policy, read-only git config, hooks and
pointer files, and host environment filter as other apps. flatpak can hide
a folder but not a single file, so paddock refuses to start a flatpak app
when the secrets detector finds a file such as .env in its folders, and
names the files. `[apps.NAME] unprotected_workspaces = true` in
~/.config/paddock/config.toml starts it anyway, with a warning on every
start. To add folders later, change [apps.NAME] roots and start the app
again.";

pub(crate) fn app_cmd(env: &HostEnv, cmd: AppCmd) -> Result<i32> {
    use paddock::app;
    match cmd {
        AppCmd::Run {
            name,
            roots,
            gpu,
            network,
            env: env_mode,
            auth,
            backend,
            worktree,
        } => {
            let mut roots = roots;
            let mut worktree = worktree;
            // A launcher entry has no terminal to show "no workspace folders".
            // Ask for a folder on the desktop instead.
            if roots.is_empty()
                && app::roots(env, &name, &[]).is_err()
                && !std::io::IsTerminal::is_terminal(&std::io::stderr())
            {
                match pick_folder_gui(env, &name) {
                    Some((dir, new_worktree)) => {
                        roots.push(dir);
                        worktree |= new_worktree;
                    }
                    None if env.which("zenity").is_some() => return Ok(0),
                    None => {}
                }
            }
            let o = app::AppOptions {
                name: name.clone(),
                roots,
                gpu,
                network,
                inherit_env: env_mode.as_deref().map(|e| e == "inherit"),
                auth,
                args: vec![],
                backend,
                worktree,
            };
            match app::run(env, &o) {
                Ok(code) => Ok(code),
                Err(e) => {
                    // A launcher entry has no terminal: show the error on the desktop too.
                    if !std::io::IsTerminal::is_terminal(&std::io::stderr()) {
                        app::desktop_notice(env, &app::start_error_note(&name, &e));
                    }
                    Err(e)
                }
            }
        }
        AppCmd::Install {
            name,
            roots,
            no_links,
        } => {
            app::app_profile(env, &name)?;
            if !roots.is_empty() {
                let rs = app::roots(env, &name, &roots)?;
                state::set_app_roots(env, &name, &rs)?;
                println!("Saved the workspace folders in [apps.{name}] of the user configuration.");
            } else {
                app::roots(env, &name, &[]).map_err(|_| {
                    anyhow::anyhow!(
                        "name the workspace folders: paddock app install {name} ~/src/project"
                    )
                })?;
            }
            let f = app::install(env, &name)?;
            println!("Wrote {}. Start it from the app launcher.", f.display());
            if !no_links {
                for (scheme, old) in app::claim_links(env, &name)? {
                    println!("{scheme}:// links now open in the {name} sandbox (before: {}). paddock app uninstall {name} gives them back.", if old.is_empty() { "no handler" } else { &old });
                }
            }
            Ok(0)
        }
        AppCmd::Open { name, link } => app::open(env, &name, &link),
        AppCmd::Uninstall { name } => {
            for (scheme, old) in app::release_links(env, &name)? {
                println!(
                    "{scheme}:// links open with {} again",
                    if old.is_empty() { "no handler" } else { &old }
                );
            }
            let f = app::desktop_file(env, &name);
            match std::fs::remove_file(&f) {
                Ok(()) => println!("Removed {}", f.display()),
                Err(_) => println!("{} does not exist", f.display()),
            }
            Ok(0)
        }
        AppCmd::List => {
            let f = HarnessFactory::new(env)?;
            for name in f.names() {
                let Some(p) = f.profile(&name) else { continue };
                if p.app.is_none() {
                    continue;
                }
                let found = match p.app.as_ref().and_then(|a| a.flatpak.clone()) {
                    Some(id) => format!("flatpak {id}"),
                    None => f
                        .create(&name)
                        .map(|h| h.exe.display().to_string())
                        .unwrap_or_else(|_| "not installed".into()),
                };
                let launcher = if app::desktop_file(env, &name).exists() {
                    "launcher entry"
                } else {
                    "no launcher entry"
                };
                let roots = app::roots(env, &name, &[])
                    .map(|r| {
                        r.iter()
                            .map(|x| x.display().to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_else(|_| "no folders".into());
                println!(
                    "{name:<16} {found}\n{:<16} {launcher}; folders: {roots}",
                    ""
                );
                if app::unprotected_workspaces(env, &name).unwrap_or(false) {
                    let what = if p.app.as_ref().is_some_and(|a| a.flatpak.is_some()) {
                        "secret files in its folders are visible to it"
                    } else {
                        "has no effect: it applies only to flatpak apps"
                    };
                    println!("{:<16} warning: unprotected_workspaces is on: {what}", "");
                }
            }
            Ok(0)
        }
    }
}

/// Ask for a workspace folder with a desktop dialog: the recent projects, or
/// any folder. For a git repository, also ask whether to open it in a new
/// worktree; that is the default answer. None when the user cancels or
/// zenity is not installed.
fn pick_folder_gui(env: &HostEnv, name: &str) -> Option<(PathBuf, bool)> {
    let dir = pick_dir_gui(env, name)?;
    let is_repo = paddock::hostgit::Layout::discover(&dir)
        .ok()
        .flatten()
        .is_some_and(|l| {
            l.try_query(&["rev-parse", "--verify", "-q", "HEAD"])
                .is_some()
        });
    if !is_repo {
        return Some((dir, false));
    }
    let zenity = env.which("zenity")?;
    const THIS: &str = "Open this folder";
    let out = std::process::Command::new(zenity)
        .args(["--question", "--title", "Open in a new worktree?", "--width=480"])
        .arg(format!(
            "--text=Open {} in a new git worktree on its own branch?\n\nThe app then cannot switch branches or overwrite changes in your checkout. When it exits, paddock lists its commits and changes.",
            dir.display()
        ))
        .args(["--ok-label=New worktree", "--cancel-label=Cancel"])
        .arg(format!("--extra-button={THIS}"))
        .output()
        .ok()?;
    if out.status.success() {
        return Some((dir, true));
    }
    (String::from_utf8_lossy(&out.stdout).trim() == THIS).then_some((dir, false))
}

fn pick_dir_gui(env: &HostEnv, name: &str) -> Option<PathBuf> {
    let zenity = env.which("zenity")?;
    let label = paddock::app::app_profile(env, name)
        .ok()
        .and_then(|p| p.app.map(|a| a.display_name))
        .unwrap_or_else(|| name.to_string());
    let title = format!("Open {label} in paddock");
    const OTHER: &str = "Choose another folder…";
    // Recent projects, shown with ~. Hidden folders (caches, test runs) stay out.
    let mut projects: Vec<String> = Vec::new();
    for r in state::recent(env) {
        let Ok(rel) = r.project.strip_prefix(&env.home) else {
            continue;
        };
        if rel
            .components()
            .any(|c| c.as_os_str().to_string_lossy().starts_with('.'))
        {
            continue;
        }
        let p = format!("~/{}", rel.display());
        if !projects.contains(&p) {
            projects.push(p);
        }
    }
    let chosen = if projects.is_empty() {
        OTHER.to_string()
    } else {
        let out = std::process::Command::new(&zenity)
            .args(["--list", "--title", &title, "--width=640", "--height=420"])
            .args(["--text", "The app sees only this folder."])
            .args(["--column", "Project folder"])
            .args(&projects)
            .arg(OTHER)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    if chosen.is_empty() {
        return None;
    }
    if chosen != OTHER {
        return Some(env.expand(&chosen, &env.home));
    }
    let out = std::process::Command::new(&zenity)
        .args(["--file-selection", "--directory", "--title", &title])
        .arg(format!("--filename={}/", env.home.display()))
        .output()
        .ok()?;
    let dir = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !dir.is_empty()).then(|| PathBuf::from(dir))
}

pub(crate) fn run(env: &HostEnv, cmd: AppCmd) -> Result<i32> {
    app_cmd(env, cmd)
}

pub(crate) fn inbound() -> Result<i32> {
    paddock::app::inbound_serve()?;
    Ok(0)
}

pub(crate) fn portal() -> Result<i32> {
    paddock::portal::serve()?;
    Ok(0)
}
