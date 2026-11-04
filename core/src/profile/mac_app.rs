use serde::Deserialize;

/// `[app.macos]`: where the app lives on macOS and how it starts there.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MacApp {
    /// Candidate app bundles, in order. `~/` expands to the home directory.
    #[serde(default)]
    pub bundles: Vec<String>,
    /// Arguments that replace `[launch] args` on macOS.
    #[serde(default)]
    pub args: Vec<String>,
    /// The app's data folder, relative to the private home. Electron apps
    /// get it as `--user-data-dir`. Default: `Library/Application
    /// Support/DISPLAY_NAME`.
    pub user_data: Option<String>,
    /// The app uses Apple's App Sandbox, which cannot run inside
    /// `sandbox-exec`. paddock refuses it even before it reads the
    /// signature.
    #[serde(default)]
    pub app_sandbox: bool,
}

/// `[macos]` of a harness profile: the backend choice of a terminal run on
/// macOS (vm/select.rs).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileMacos {
    /// "seatbelt": `--backend auto` keeps this harness on Seatbelt, for
    /// tools that need macOS itself (Xcode, swift for Apple targets,
    /// codesign).
    pub backend: Option<String>,
    /// The harness needs new ptys. When its program ignores paddock's pty
    /// library on Seatbelt, `--backend auto` picks the VM.
    #[serde(default)]
    pub needs_pty: bool,
}
