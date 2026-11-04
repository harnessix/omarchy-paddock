use anyhow::{bail, Result};
use paddock::build;
use paddock::doctor;
use paddock::env::HostEnv;
use paddock::manifest::{self, Manifest};
use paddock::spec::GuiMode;
use std::path::PathBuf;

#[allow(clippy::too_many_arguments)]
pub(crate) fn doctor(
    env: &HostEnv,
    escape: bool,
    backend: String,
    network: paddock::spec::Network,
    gui: bool,
    window: bool,
    app: bool,
    bus: Option<String>,
    env_mode: String,
    online: bool,
    report: bool,
    output: Option<PathBuf>,
) -> Result<i32> {
    if report {
        let text = paddock::report::build(env);
        match output {
            Some(f) => {
                std::fs::write(&f, &text)?;
                println!(
                    "Wrote the report to {}. Check it before you share it.",
                    f.display()
                );
            }
            None => print!("{text}"),
        }
        return Ok(0);
    }
    if escape {
        let display = if app {
            GuiMode::App
        } else if window {
            GuiMode::Window
        } else if gui {
            GuiMode::Headless
        } else {
            GuiMode::Off
        };
        let failed = doctor::escape(
            env,
            &doctor::EscapeOpts {
                backend,
                network,
                display,
                online,
                inherit: env_mode == "inherit",
                app_bus: if bus.as_deref() == Some("host-filtered") {
                    paddock::manifest::AppBus::HostFiltered
                } else {
                    paddock::manifest::AppBus::Private
                },
            },
        )?;
        Ok(if failed == 0 { 0 } else { 1 })
    } else {
        Ok(if doctor::report(env)? { 0 } else { 1 })
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn setup(
    env: &HostEnv,
    yes: bool,
    shims: bool,
    no_shims: bool,
    launchers: bool,
    no_launchers: bool,
    plugin: bool,
    no_plugin: bool,
    (loft, no_loft): (bool, bool),
    backend: Option<String>,
    apps: Vec<String>,
) -> Result<i32> {
    use paddock::setup;
    let flag = |on: bool, off: bool| {
        if on {
            Some(true)
        } else if off {
            Some(false)
        } else {
            None
        }
    };
    let cwd = std::env::current_dir()?;
    let mut app_roots: Vec<(String, Vec<PathBuf>)> = Vec::new();
    for a in apps {
        let Some((name, dir)) = a.split_once('=') else {
            bail!("--app takes NAME=DIR, for example --app vscode=~/src/api");
        };
        let dir = env.expand(dir, &cwd);
        match app_roots.iter_mut().find(|(n, _)| n == name) {
            Some((_, dirs)) => dirs.push(dir),
            None => app_roots.push((name.to_string(), vec![dir])),
        }
    }
    let o = setup::Opts {
        shims: flag(shims, no_shims),
        launchers: flag(launchers, no_launchers),
        plugin: flag(plugin, no_plugin),
        loft: flag(loft, no_loft),
        backend,
        app_roots,
    };
    let mut term = if yes { None } else { setup::Terminal::detect() };
    let ask = term.as_mut().map(|t| t as &mut dyn setup::Ask);
    setup::run(env, &o, ask)?;
    Ok(0)
}

pub(crate) fn approve(env: &HostEnv, path: Option<PathBuf>) -> Result<i32> {
    let project = build::resolve_project(path.as_deref())?;
    let file = project.join(manifest::PROJECT_FILE);
    let m = Manifest::read(&file)?;
    println!("{}", serde_json::to_string_pretty(&m.widening())?);
    manifest::approve(env, &file, &m)?;
    println!("Approved {}", file.display());
    Ok(0)
}

pub(crate) fn init(path: Option<PathBuf>) -> Result<i32> {
    let project = build::resolve_project(path.as_deref())?;
    let file = project.join(manifest::PROJECT_FILE);
    // A new file only: a dangling symlink there is not followed.
    let dir = paddock::safefs::Root::open(&project)?.dir(std::path::Path::new(""), false)?;
    let mut f = dir
        .create_new(std::ffi::OsStr::new(manifest::PROJECT_FILE), 0o644)
        .map_err(|e| anyhow::anyhow!("{} already exists: {e:#}", file.display()))?;
    std::io::Write::write_all(&mut f, manifest::TEMPLATE.as_bytes())?;
    println!("Wrote {}", file.display());
    Ok(0)
}
