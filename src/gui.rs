//! Displays for desktop projects.
//!
//! The libraries a desktop app needs are not the problem: the bwrap backend
//! sees the host `/usr`, and the docker backend installs packages. The problem
//! is access to the display. A client of the host compositor can capture the
//! screen, read the clipboard and, on wlroots compositors, type into other
//! windows. So the default gives the sandbox a compositor of its own.

use crate::env::HostEnv;
use crate::spec::{sandbox_runtime_dir, GuiMode, SandboxSpec, SANDBOX_WAYLAND};
use anyhow::{bail, Context, Result};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub const SCREENSHOT_TOOL: &str = "paddock-screenshot";

const SCREENSHOT_SCRIPT: &str = r#"#!/bin/sh
# Capture the sandbox display to a PNG file and print its path.
# Usage: paddock-screenshot [output.png]
out="${1:-/tmp/paddock-screenshot-$(date +%s).png}"
case "$out" in /*) ;; *) out="$PWD/$out" ;; esac
dir=$(mktemp -d) || exit 1
cd "$dir" || exit 1
if ! weston-screenshooter >/dev/null 2>&1; then
  echo "paddock-screenshot: capture failed. The display must be in headless or window mode." >&2
  exit 1
fi
f=$(ls -t ./*.png 2>/dev/null | head -n 1)
if [ -z "$f" ]; then
  echo "paddock-screenshot: no image was written" >&2
  exit 1
fi
mv "$f" "$out" && cd / && rmdir "$dir" 2>/dev/null
echo "$out"
"#;

/// Add the display pieces to a spec whose `gui.mode` is already decided.
pub fn apply(env: &HostEnv, spec: &mut SandboxSpec) -> Result<()> {
    if !spec.gui.enabled() {
        return Ok(());
    }
    let origin = format!("gui {:?}", spec.gui.mode).to_lowercase();
    let rt = sandbox_runtime_dir(spec.uid);
    spec.set_env("XDG_RUNTIME_DIR", rt.display().to_string(), &origin);
    spec.set_env("WAYLAND_DISPLAY", SANDBOX_WAYLAND, &origin);
    spec.set_env("XDG_SESSION_TYPE", "wayland", &origin);
    spec.set_env("GDK_BACKEND", "wayland,x11", &origin);
    spec.set_env("QT_QPA_PLATFORM", "wayland;xcb", &origin);
    spec.set_env("ELECTRON_OZONE_PLATFORM_HINT", "auto", &origin);
    // WebKitGTK cannot use dmabuf without a GPU render node.
    if !spec.gui.gpu {
        spec.set_env("WEBKIT_DISABLE_DMABUF_RENDERER", "1", &origin);
        spec.set_env("LIBGL_ALWAYS_SOFTWARE", "1", &origin);
    }

    match spec.gui.mode {
        GuiMode::Headless => {
            spec.set_env("DISPLAY", ":0", &origin);
            let (w, h) = (spec.gui.width, spec.gui.height);
            spec.setup.push(format!(
                r#"if command -v weston >/dev/null 2>&1; then
  mkdir -p /tmp/.X11-unix && chmod 1777 /tmp/.X11-unix
  weston --backend=headless --renderer=pixman --socket={SANDBOX_WAYLAND} --width={w} --height={h} --debug --xwayland --idle-time=0 --log=/tmp/paddock-weston.log >/dev/null 2>&1 &
  i=0; while [ ! -S "$XDG_RUNTIME_DIR/{SANDBOX_WAYLAND}" ] && [ $i -lt 100 ]; do sleep 0.05; i=$((i+1)); done
  [ -S "$XDG_RUNTIME_DIR/{SANDBOX_WAYLAND}" ] || echo 'paddock: the headless display did not start; see /tmp/paddock-weston.log' >&2
else
  echo 'paddock: weston is not installed in the sandbox; the headless display is off' >&2
fi"#
            ));
        }
        GuiMode::Window | GuiMode::Host | GuiMode::App => {}
        GuiMode::Off => unreachable!(),
    }

    // App mode: the restricted socket and the filtered bus come from run.rs,
    // and the sandbox's display is the host desktop, which paddock-host
    // screenshot captures with approval.
    if spec.gui.mode == GuiMode::App {
        return Ok(());
    }

    let script = run_dir(env, spec).join(SCREENSHOT_TOOL);
    write_file(&script, SCREENSHOT_SCRIPT, 0o755)?;
    spec.add_tool(SCREENSHOT_TOOL, script, &origin);

    // A private session bus, so desktop apps that need D-Bus start, without the host bus.
    let mut cmd = vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        "if command -v dbus-run-session >/dev/null 2>&1; then exec dbus-run-session -- \"$@\"; else exec \"$@\"; fi".to_string(),
        "paddock-dbus".to_string(),
    ];
    cmd.append(&mut spec.command);
    spec.command = cmd;

    if spec.gui.mode == GuiMode::Host {
        let (Some(rt), Some(d)) = (&env.runtime_dir, env.var("WAYLAND_DISPLAY")) else {
            bail!("gui mode 'host' needs WAYLAND_DISPLAY and XDG_RUNTIME_DIR on the host");
        };
        spec.gui.host_socket = Some(rt.join(d));
        spec.warnings.push(
            "gui mode 'host': the sandbox can capture the screen, read the clipboard and send input to other windows".into(),
        );
    }
    Ok(())
}

fn run_dir(env: &HostEnv, spec: &SandboxSpec) -> PathBuf {
    env.paddock_run().join(&spec.id)
}

pub fn write_file(path: &Path, text: &str, mode: u32) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // A previous render can leave a read-only copy: replace it.
    let _ = std::fs::remove_file(path);
    std::fs::write(path, text)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

/// The nested compositor for `window` mode. It runs on the host as a normal
/// client window; the sandbox only receives its socket.
pub struct WindowCompositor {
    child: Child,
    pub socket: PathBuf,
}

impl WindowCompositor {
    pub fn start(env: &HostEnv, spec: &SandboxSpec) -> Result<Self> {
        let weston = env
            .which("weston")
            .context("gui mode 'window' needs weston on the host")?;
        let rt = env
            .runtime_dir
            .clone()
            .context("gui mode 'window' needs XDG_RUNTIME_DIR")?;
        if env.var("WAYLAND_DISPLAY").is_none() {
            bail!("gui mode 'window' needs a Wayland session on the host");
        }
        let name = format!("paddock-{}", spec.id);
        let socket = rt.join(&name);
        let log = run_dir(env, spec).join("weston.log");
        std::fs::create_dir_all(log.parent().unwrap())?;
        let child = Command::new(weston)
            .arg("--backend=wayland")
            .arg(format!("--socket={name}"))
            .arg(format!("--width={}", spec.gui.width))
            .arg(format!("--height={}", spec.gui.height))
            .arg("--debug")
            .arg("--idle-time=0")
            .arg(format!("--log={}", log.display()))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let mut me = Self { child, socket };
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            if is_socket(&me.socket) {
                return Ok(me);
            }
            if let Some(status) = me.child.try_wait()? {
                bail!(
                    "the nested compositor exited with {status}; see {}",
                    log.display()
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        me.stop();
        bail!(
            "the nested compositor did not create its socket; see {}",
            log.display()
        )
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    pub fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for WindowCompositor {
    fn drop(&mut self) {
        self.stop();
    }
}

pub fn is_socket(p: &Path) -> bool {
    std::fs::metadata(p)
        .map(|m| m.file_type().is_socket())
        .unwrap_or(false)
}

/// GPU render nodes. Render nodes allow GPU work but not display control.
pub fn render_nodes() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir("/dev/dri")
        .map(|it| {
            it.flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with("renderD"))
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

/// Markers of a desktop project. The detector turns on `headless`, which gives
/// no host access, so it needs no approval.
pub fn desktop_marker(project: &Path) -> Option<String> {
    let files = [
        "src-tauri/tauri.conf.json",
        "tauri.conf.json",
        "src-tauri/tauri.conf.json5",
        "electron-builder.json",
        "forge.config.js",
        "pubspec.yaml",
    ];
    for f in files {
        if project.join(f).is_file() {
            return Some(f.to_string());
        }
    }
    let contains = |file: &str, words: &[&str]| -> Option<String> {
        let text = std::fs::read_to_string(project.join(file)).ok()?;
        words
            .iter()
            .find(|w| text.contains(*w))
            .map(|w| format!("{file} mentions {w}"))
    };
    contains("package.json", &["\"electron\""])
        .or_else(|| {
            contains(
                "Cargo.toml",
                &[
                    "gtk4",
                    "gtk =",
                    "iced",
                    "eframe",
                    "egui",
                    "slint",
                    "winit",
                    "tauri",
                    "dioxus-desktop",
                    "gpui",
                ],
            )
        })
        .or_else(|| contains("CMakeLists.txt", &["Qt6", "Qt5", "GTK", "gtk"]))
        .or_else(|| contains("meson.build", &["gtk", "Qt"]))
        .or_else(|| {
            contains(
                "pyproject.toml",
                &["PySide6", "PyQt", "pygobject", "toga", "kivy"],
            )
        })
        .or_else(|| contains("go.mod", &["fyne.io", "gioui.org", "wails"]))
}
