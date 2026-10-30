//! Launcher entries for everything paddock can start, grouped in a "Paddock"
//! folder.
//!
//! - Each installed terminal harness gets a desktop entry that opens a terminal
//!   with `paddock pick <harness>`: choose a recent project, or type a path.
//! - Each installed app gets the entry of `paddock app install`.
//! - The folder: a "Paddock" submenu in the Omarchy menu (a marked block in the
//!   user's menu extension file), and an XDG menu folder for menu-based
//!   desktops (GNOME, KDE, Xfce).
//!
//! Nothing is written unless the user runs `paddock launchers install`, and
//! `paddock launchers remove` takes all of it away again.

use crate::app;
use crate::env::HostEnv;
use crate::profile::HarnessFactory;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

pub const ICON: &str = include_str!("../docs/assets/paddock-icon.svg");
const CLI_PREFIX: &str = "paddock-cli-";
const BLOCK_START: &str =
    "  // >>> paddock launchers: written by `paddock launchers install`; edits here are replaced";
const BLOCK_END: &str = "  // <<< paddock launchers";

#[derive(Debug, Clone)]
pub struct Entry {
    /// Harness or app name.
    pub name: String,
    pub label: String,
    pub kind: Kind,
    pub desktop_file: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Cli,
    App,
}

fn applications_dir(env: &HostEnv) -> PathBuf {
    env.data_dir.join("applications")
}

fn cli_desktop_file(env: &HostEnv, name: &str) -> PathBuf {
    applications_dir(env).join(format!("{CLI_PREFIX}{name}.desktop"))
}

fn icon_file(env: &HostEnv) -> PathBuf {
    env.data_dir.join("icons/hicolor/scalable/apps/paddock.svg")
}

fn omarchy_menu_file(env: &HostEnv) -> PathBuf {
    env.config_dir.join("omarchy/extensions/omarchy-menu.jsonc")
}

fn xdg_menu_file(env: &HostEnv) -> PathBuf {
    env.config_dir
        .join("menus/applications-merged/paddock.menu")
}

/// The files that `install` created, so that `remove` touches nothing else.
fn record_file(env: &HostEnv) -> PathBuf {
    env.paddock_state().join("launchers.json")
}

fn xdg_directory_file(env: &HostEnv) -> PathBuf {
    env.data_dir.join("desktop-directories/paddock.directory")
}

/// A menu label from a profile description: without the vendor in front, so
/// it fits a menu row ("Anthropic Claude Code" becomes "Claude Code").
fn short_label(description: &str, name: &str) -> String {
    let d = description.trim();
    if d.is_empty() {
        return name.to_string();
    }
    for vendor in ["Anthropic ", "OpenAI ", "Google ", "GitHub ", "Microsoft "] {
        if let Some(rest) = d.strip_prefix(vendor) {
            if !rest.is_empty() {
                return rest.to_string();
            }
        }
    }
    d.to_string()
}

/// Harnesses and apps that are installed on this machine.
pub fn detect(env: &HostEnv) -> Result<Vec<Entry>> {
    let f = HarnessFactory::new(env)?;
    let mut out = Vec::new();
    for name in f.names() {
        let Some(p) = f.profile(&name) else { continue };
        match &p.app {
            None => {
                if f.create(&name).is_err() {
                    continue;
                }
                let label = short_label(&p.description, &name);
                out.push(Entry {
                    desktop_file: cli_desktop_file(env, &name),
                    name,
                    label,
                    kind: Kind::Cli,
                });
            }
            Some(a) => {
                let installed = match &a.flatpak {
                    Some(id) => Command::new("flatpak")
                        .args(["info", "--show-ref", id])
                        .output()
                        .map(|o| o.status.success())
                        .unwrap_or(false),
                    None => f.create(&name).is_ok(),
                };
                if !installed {
                    continue;
                }
                let label = if a.display_name.is_empty() {
                    name.clone()
                } else {
                    a.display_name.clone()
                };
                out.push(Entry {
                    desktop_file: app::desktop_file(env, &name),
                    name,
                    label,
                    kind: Kind::App,
                });
            }
        }
    }
    Ok(out)
}

fn desktop_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\n', " ")
}

fn write_cli_entry(e: &Entry, paddock: &Path) -> Result<()> {
    let text = format!(
        "[Desktop Entry]\nType=Application\nName={label} (paddock)\nComment=Pick a project, then run {label} in a paddock sandbox\nExec={exe} pick {name}\nIcon=paddock\nTerminal=true\nCategories=Development;X-Paddock;\nKeywords=paddock;sandbox;agent;{name};\n",
        label = desktop_escape(&e.label),
        exe = paddock.display(),
        name = e.name
    );
    if let Some(d) = e.desktop_file.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(&e.desktop_file, text)?;
    Ok(())
}

fn json_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

/// The Omarchy menu block: a "Paddock" submenu with one row per entry.
fn omarchy_block(entries: &[Entry], paddock: &Path) -> String {
    let exe = paddock.display().to_string();
    let mut lines = vec![
        BLOCK_START.to_string(),
        format!(
            "  \"paddock\": {{\"icon\":\"󱞚\",\"label\":\"Paddock\",\"description\":\"Coding agents and apps in a sandbox\",\"when\":{}}},",
            json_str(&format!("test -x {}", shell_quote(&exe)))
        ),
    ];
    for e in entries {
        let (icon, action, description) = match e.kind {
            Kind::Cli => (
                "󰆍",
                format!(
                    "uwsm-app -- xdg-terminal-exec --title={} -e {} pick {}",
                    shell_quote(&format!("paddock · {}", e.label)),
                    shell_quote(&exe),
                    e.name
                ),
                "Pick a project, then start it in a sandbox",
            ),
            Kind::App => (
                "󰀻",
                format!("uwsm-app -- {} app run {}", shell_quote(&exe), e.name),
                "Open the app in a sandbox",
            ),
        };
        lines.push(format!(
            "  {}: {{\"icon\":\"{icon}\",\"label\":{},\"description\":{},\"action\":{}}},",
            json_str(&format!("paddock.{}", e.name)),
            json_str(&e.label),
            json_str(description),
            json_str(&action)
        ));
    }
    lines.push(BLOCK_END.to_string());
    lines.join("\n")
}

fn shell_quote(s: &str) -> String {
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || "/._-".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// Remove the marked block, if there is one.
fn strip_block(text: &str) -> String {
    let mut out = Vec::new();
    let mut inside = false;
    for line in text.lines() {
        if line.trim_start().starts_with(BLOCK_START.trim_start()) {
            inside = true;
            // The blank line that insert_block put before the block.
            if out.last().is_some_and(|l: &&str| l.trim().is_empty()) {
                out.pop();
            }
            continue;
        }
        if inside {
            if line.trim_start().starts_with(BLOCK_END.trim_start()) {
                inside = false;
            }
            continue;
        }
        out.push(line);
    }
    let mut s = out.join("\n");
    if text.ends_with('\n') {
        s.push('\n');
    }
    s
}

/// Put the block before the closing brace of the menu extension object.
fn insert_block(text: &str, block: &str) -> Result<String> {
    let clean = strip_block(text);
    let end = clean
        .rfind('}')
        .context("the Omarchy menu extension file has no closing brace")?;
    let (head, tail) = clean.split_at(end);
    let head = head.trim_end_matches([' ', '\t', '\n']);
    Ok(format!("{head}\n\n{block}\n{tail}"))
}

fn xdg_menu(entries: &[Entry]) -> String {
    let mut files = String::new();
    for e in entries {
        if let Some(n) = e.desktop_file.file_name() {
            files.push_str(&format!(
                "      <Filename>{}</Filename>\n",
                n.to_string_lossy()
            ));
        }
    }
    format!(
        "<!DOCTYPE Menu PUBLIC \"-//freedesktop//DTD Menu 1.0//EN\" \"http://www.freedesktop.org/standards/menu-spec/menu-1.0.dtd\">\n<!-- Written by `paddock launchers install`. -->\n<Menu>\n  <Name>Applications</Name>\n  <Menu>\n    <Name>Paddock</Name>\n    <Directory>paddock.directory</Directory>\n    <Include>\n{files}    </Include>\n  </Menu>\n</Menu>\n"
    )
}

/// The paddock path to write into entries: the one on PATH when it is this
/// same program (so an upgrade keeps the entries working), else this program.
pub fn launcher_exe(env: &HostEnv) -> Result<PathBuf> {
    let me = std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .context("cannot find the paddock executable")?;
    if let Some(on_path) = env.which("paddock") {
        if std::fs::canonicalize(&on_path).ok().as_deref() == Some(me.as_path()) {
            return Ok(on_path);
        }
    }
    Ok(me)
}

pub struct Report {
    pub entries: Vec<Entry>,
    pub omarchy_menu: Option<PathBuf>,
}

pub fn install(env: &HostEnv) -> Result<Report> {
    let paddock = launcher_exe(env)?;
    let entries = detect(env)?;

    let icon = icon_file(env);
    std::fs::create_dir_all(icon.parent().unwrap())?;
    std::fs::write(&icon, ICON)?;

    let mut created: Vec<PathBuf> = std::fs::read_to_string(record_file(env))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    for e in &entries {
        let existed = e.desktop_file.exists();
        match e.kind {
            Kind::Cli => write_cli_entry(e, &paddock)?,
            Kind::App => {
                // An app entry that the user already installed stays theirs.
                if !existed {
                    app::install(env, &e.name)
                        .with_context(|| format!("launcher entry for {}", e.name))?;
                }
            }
        }
        if !existed && !created.contains(&e.desktop_file) {
            created.push(e.desktop_file.clone());
        }
    }
    crate::privfs::write(&record_file(env), serde_json::to_string_pretty(&created)?)?;

    // A folder for menu-based desktops.
    let dir = xdg_directory_file(env);
    std::fs::create_dir_all(dir.parent().unwrap())?;
    std::fs::write(&dir, "[Desktop Entry]\nType=Directory\nName=Paddock\nComment=Coding agents and apps in a sandbox\nIcon=paddock\n")?;
    let menu = xdg_menu_file(env);
    std::fs::create_dir_all(menu.parent().unwrap())?;
    std::fs::write(&menu, xdg_menu(&entries))?;

    // The Omarchy menu, only where Omarchy keeps its menu extensions.
    let omenu = omarchy_menu_file(env);
    let omarchy_menu = if omenu.is_file() {
        let text = std::fs::read_to_string(&omenu)?;
        let new = insert_block(&text, &omarchy_block(&entries, &paddock))?;
        std::fs::write(&omenu, new)?;
        Some(omenu)
    } else {
        None
    };

    refresh_desktop_database(env);
    Ok(Report {
        entries,
        omarchy_menu,
    })
}

pub fn remove(env: &HostEnv) -> Result<Vec<PathBuf>> {
    let mut removed = Vec::new();
    if let Ok(dir) = std::fs::read_dir(applications_dir(env)) {
        for e in dir.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if n.starts_with(CLI_PREFIX) && n.ends_with(".desktop") {
                std::fs::remove_file(e.path())?;
                removed.push(e.path());
            }
        }
    }
    // App entries that `install` created; entries the user made stay.
    let created: Vec<PathBuf> = std::fs::read_to_string(record_file(env))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    for file in created {
        if file.exists() && file.starts_with(applications_dir(env)) {
            std::fs::remove_file(&file)?;
            removed.push(file);
        }
    }
    let _ = std::fs::remove_file(record_file(env));
    for p in [xdg_menu_file(env), xdg_directory_file(env), icon_file(env)] {
        if p.exists() {
            std::fs::remove_file(&p)?;
            removed.push(p);
        }
    }
    let omenu = omarchy_menu_file(env);
    if let Ok(text) = std::fs::read_to_string(&omenu) {
        let clean = strip_block(&text);
        if clean != text {
            std::fs::write(&omenu, clean)?;
            removed.push(omenu);
        }
    }
    refresh_desktop_database(env);
    Ok(removed)
}

/// Whether `install` ran and `remove` did not run since.
pub fn installed(env: &HostEnv) -> bool {
    record_file(env).is_file()
        || std::fs::read_dir(applications_dir(env)).is_ok_and(|d| {
            d.flatten().any(|e| {
                let n = e.file_name().to_string_lossy().into_owned();
                n.starts_with(CLI_PREFIX) && n.ends_with(".desktop")
            })
        })
}

fn refresh_desktop_database(env: &HostEnv) {
    let _ = Command::new("update-desktop-database")
        .arg(applications_dir(env))
        .output();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_drop_the_vendor() {
        assert_eq!(
            short_label("Anthropic Claude Code", "claude"),
            "Claude Code"
        );
        assert_eq!(
            short_label("Google Antigravity CLI", "agy"),
            "Antigravity CLI"
        );
        assert_eq!(short_label("opencode", "opencode"), "opencode");
        assert_eq!(short_label("", "x"), "x");
    }

    #[test]
    fn block_round_trip_keeps_user_entries() {
        let user = "{\n  // mine\n  \"personal\": {\"label\":\"Personal\"},\n}\n";
        let e = vec![Entry {
            name: "claude".into(),
            label: "Claude Code".into(),
            kind: Kind::Cli,
            desktop_file: PathBuf::from("/x/paddock-cli-claude.desktop"),
        }];
        let with = insert_block(user, &omarchy_block(&e, Path::new("/usr/bin/paddock"))).unwrap();
        assert!(with.contains("\"paddock.claude\"") && with.contains("\"personal\""));
        assert!(with.trim_end().ends_with('}'));
        // Installing twice does not duplicate the block.
        let twice = insert_block(&with, &omarchy_block(&e, Path::new("/usr/bin/paddock"))).unwrap();
        assert_eq!(twice.matches("\"paddock.claude\"").count(), 1);
        // Removing gives back the user's file.
        assert_eq!(strip_block(&with).trim(), user.trim());
    }
}
