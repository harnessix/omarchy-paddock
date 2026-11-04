//! The backend choice on macOS: `--backend seatbelt | vm | auto`. The rule
//! is a pure function of a few facts, so a table test covers it on every
//! system. `facts` reads them from a built run.

use crate::build::Built;
use crate::env::HostEnv;
use crate::spec::{GuiMode, MountKind};
use anyhow::{bail, Result};

/// The name of the VM backend.
pub const NAME: &str = "vm";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    Seatbelt,
    Vm,
}

impl Choice {
    pub fn name(self) -> &'static str {
        match self {
            Choice::Seatbelt => crate::backend::seatbelt::NAME,
            Choice::Vm => NAME,
        }
    }
}

/// What the choice depends on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Facts {
    /// `paddock app run`.
    pub app: bool,
    /// Linux-only features that the run needs, by name.
    pub linux_only: Vec<String>,
    /// The profile has `[macos] needs_pty = true` and its program ignores
    /// paddock's pty library: why.
    pub needs_pty: Option<String>,
    /// Who asks for Seatbelt: `[macos] backend = "seatbelt"` of the profile.
    pub wants_seatbelt: Option<String>,
    /// `[macos] backend = "vm"` of the user configuration.
    pub user_vm: bool,
    /// The VM files check: None when it passes, else the error.
    pub assets_error: Option<String>,
}

/// The text when a run needs the VM and the VM files are not there.
pub fn refusal(need: &str, assets_error: &str) -> String {
    format!(
        "this run needs {need}, and on macOS only the VM backend gives it. The VM files are not set up ({assets_error}). Run `paddock vm setup`, then start again"
    )
}

/// The backend for `--backend auto` on macOS, and the reason.
pub fn decide(f: &Facts) -> Result<(Choice, String)> {
    if f.app {
        return Ok((
            Choice::Seatbelt,
            "app mode runs desktop apps with Seatbelt".into(),
        ));
    }
    let need = if !f.linux_only.is_empty() {
        Some(format!("{} (Linux only)", f.linux_only.join(", ")))
    } else {
        f.needs_pty
            .as_ref()
            .map(|why| format!("new ptys, which Seatbelt cannot give this harness ({why})"))
    };
    if let Some(need) = need {
        return match &f.assets_error {
            None => Ok((Choice::Vm, format!("the run needs {need}"))),
            Some(e) => bail!(refusal(&need, e)),
        };
    }
    if let Some(who) = &f.wants_seatbelt {
        return Ok((Choice::Seatbelt, format!("{who} asks for Seatbelt")));
    }
    if f.user_vm {
        return match &f.assets_error {
            None => Ok((
                Choice::Vm,
                "[macos] backend = \"vm\" in the user configuration".into(),
            )),
            Some(e) => bail!(
                "[macos] backend = \"vm\" in the user configuration asks for the VM, and the VM files are not set up ({e}). Run `paddock vm setup`, or start with --backend seatbelt"
            ),
        };
    }
    Ok((Choice::Seatbelt, "the default for a terminal run".into()))
}

/// The backend that a run asks for. On macOS, `auto` becomes Seatbelt or
/// the VM by [`decide`], and paddock prints the choice and the reason.
pub fn wanted(env: &HostEnv, built: &Built) -> Result<String> {
    let rule = cfg!(target_os = "macos")
        && built.backend == "auto"
        && built.spec.network != crate::spec::Network::Allowlist;
    if !rule {
        return Ok(built.backend.clone());
    }
    let (choice, why) = decide(&facts(env, built)?)?;
    eprintln!("paddock: backend {}: {why}", choice.name());
    Ok(choice.name().to_string())
}

/// The `[macos] backend` of the user configuration.
pub fn user_backend(env: &HostEnv) -> Result<Option<String>> {
    let file = env.paddock_config().join("config.toml");
    let Ok(text) = std::fs::read_to_string(&file) else {
        return Ok(None);
    };
    let doc: toml::Value =
        toml::from_str(&text).map_err(|e| anyhow::anyhow!("{} is invalid: {e}", file.display()))?;
    match doc.get("macos").and_then(|m| m.get("backend")) {
        None => Ok(None),
        Some(toml::Value::String(s)) if s == "vm" || s == "seatbelt" || s == "auto" => {
            Ok(Some(s.clone()))
        }
        Some(_) => bail!("[macos] backend must be \"seatbelt\", \"vm\" or \"auto\""),
    }
}

/// The Linux-only features of a built run.
pub fn linux_only(env: &HostEnv, built: &Built) -> Vec<String> {
    let s = &built.spec;
    let mut out = Vec::new();
    if !crate::devices::names_for(env, built.test_devices, built.device_ceiling.as_deref())
        .is_empty()
    {
        out.push("the devices of /dev/paddock".to_string());
    }
    if s.gui.mode == GuiMode::Window {
        out.push("window mode".into());
    }
    if s.shadow.is_some() {
        out.push("--profile default".into());
    }
    if let Some(m) = s.mounts.iter().find(|m| m.kind == MountKind::Overlay) {
        out.push(format!("an overlay at {}", m.dst.display()));
    }
    if s.mounts
        .iter()
        .any(|m| m.dst == std::path::Path::new(crate::borrow::MOUNT))
    {
        out.push("borrow".into());
    }
    if s.keyring || s.private_bus {
        out.push("the private keyring and session bus".into());
    }
    out
}

/// The facts of a built run.
pub fn facts(env: &HostEnv, built: &Built) -> Result<Facts> {
    let mac = &built.profile.macos;
    if let Some(b) = mac.backend.as_deref().filter(|b| *b != "seatbelt") {
        bail!(
            "[macos] backend = {b:?} in the {} profile: only \"seatbelt\" is known",
            built.profile.name
        );
    }
    let needs_pty = if mac.needs_pty {
        crate::backend::seatbelt::run::pty_refusal_for(env, &built.spec)
    } else {
        None
    };
    Ok(Facts {
        app: built.spec.gui.mode == GuiMode::App,
        linux_only: linux_only(env, built),
        needs_pty,
        wants_seatbelt: (mac.backend.as_deref() == Some("seatbelt"))
            .then(|| format!("the {} profile ([macos] backend)", built.profile.name)),
        user_vm: user_backend(env)?.as_deref() == Some(NAME),
        assets_error: crate::vm::assets::check(&crate::vm::assets::assets_dir(env))
            .err()
            .map(|e| format!("{e:#}")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f() -> Facts {
        Facts::default()
    }

    #[test]
    fn the_auto_rule() {
        let missing = Some("cannot read assets.toml".to_string());
        let lin = vec!["window mode".to_string()];
        let pty = Some("agy has the hardened runtime".to_string());
        let sb = Some("the xcode profile ([macos] backend)".to_string());
        // (facts, the choice or None for a refusal, a word of the reason)
        let table: Vec<(Facts, Option<Choice>, &str)> = vec![
            (f(), Some(Choice::Seatbelt), "default"),
            (
                Facts {
                    app: true,
                    linux_only: lin.clone(),
                    user_vm: true,
                    ..f()
                },
                Some(Choice::Seatbelt),
                "app mode",
            ),
            (
                Facts {
                    linux_only: lin.clone(),
                    ..f()
                },
                Some(Choice::Vm),
                "window mode",
            ),
            (
                Facts {
                    linux_only: lin.clone(),
                    assets_error: missing.clone(),
                    ..f()
                },
                None,
                "paddock vm setup",
            ),
            (
                Facts {
                    needs_pty: pty.clone(),
                    ..f()
                },
                Some(Choice::Vm),
                "hardened runtime",
            ),
            (
                Facts {
                    needs_pty: pty.clone(),
                    assets_error: missing.clone(),
                    ..f()
                },
                None,
                "paddock vm setup",
            ),
            (
                Facts {
                    wants_seatbelt: sb.clone(),
                    ..f()
                },
                Some(Choice::Seatbelt),
                "xcode",
            ),
            (
                Facts {
                    wants_seatbelt: sb.clone(),
                    user_vm: true,
                    ..f()
                },
                Some(Choice::Seatbelt),
                "xcode",
            ),
            (
                Facts {
                    wants_seatbelt: sb.clone(),
                    linux_only: lin.clone(),
                    ..f()
                },
                Some(Choice::Vm),
                "window mode",
            ),
            (
                Facts {
                    user_vm: true,
                    ..f()
                },
                Some(Choice::Vm),
                "user configuration",
            ),
            (
                Facts {
                    user_vm: true,
                    assets_error: missing.clone(),
                    ..f()
                },
                None,
                "--backend seatbelt",
            ),
            (
                Facts {
                    assets_error: missing.clone(),
                    ..f()
                },
                Some(Choice::Seatbelt),
                "default",
            ),
        ];
        for (i, (facts, want, word)) in table.into_iter().enumerate() {
            match (decide(&facts), want) {
                (Ok((c, why)), Some(w)) => {
                    assert_eq!(c, w, "row {i}: {why}");
                    assert!(why.contains(word), "row {i}: {why}");
                }
                (Err(e), None) => assert!(e.to_string().contains(word), "row {i}: {e}"),
                (got, want) => panic!("row {i}: got {got:?}, want {want:?}"),
            }
        }
    }

    #[test]
    fn the_refusal_names_the_need_and_the_setup() {
        let facts = Facts {
            linux_only: vec!["--profile default".into(), "window mode".into()],
            assets_error: Some("cannot read /x/assets.toml".into()),
            ..f()
        };
        let e = decide(&facts).unwrap_err().to_string();
        assert_eq!(
            e,
            "this run needs --profile default, window mode (Linux only), and on macOS only the VM backend gives it. The VM files are not set up (cannot read /x/assets.toml). Run `paddock vm setup`, then start again"
        );
    }

    #[test]
    fn user_backend_reads_the_macos_table() {
        let t = tempfile::tempdir().unwrap();
        let env = HostEnv::from_parts(t.path().into(), "u".into(), 501, 20, Default::default());
        assert_eq!(user_backend(&env).unwrap(), None);
        let cfg = env.paddock_config();
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(cfg.join("config.toml"), "[macos]\nbackend = \"vm\"\n").unwrap();
        assert_eq!(user_backend(&env).unwrap().as_deref(), Some("vm"));
        std::fs::write(cfg.join("config.toml"), "[macos]\nbackend = \"bwrap\"\n").unwrap();
        assert!(user_backend(&env).is_err());
    }
}
