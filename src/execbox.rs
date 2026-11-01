//! Where an approved `exec` runs: a hardened, one-time unit.
//!
//! The executor (`paddock gatewayd`) never runs a command with the whole
//! host. By default the command sees the system read-only, the project
//! read-write, an empty home and its own `/tmp`. It can use the network,
//! but it cannot gain privileges or create namespaces. Only a few host
//! variables pass. `[exec]` in the user configuration widens this:
//!
//! ```toml
//! [exec]
//! home = "hidden"      # hidden (default), read-only, or full
//! network = true       # false: no network at all
//! paths = []           # more folders, read-write
//! ro_paths = []        # more folders, read-only
//! env = []             # more host variables to pass, by name
//! ```
//!
//! With a systemd user instance, the command runs in a transient unit
//! (`systemd-run --user`) with these properties. Without one, bwrap gives
//! the same view. A project `paddock.toml` cannot set `[exec]`.

use crate::env::HostEnv;
use anyhow::{bail, Context as _, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Host variables that every command gets.
pub const BASE_ENV: &[&str] = &[
    "HOME",
    "PATH",
    "LANG",
    "LANGUAGE",
    "LC_ALL",
    "LC_CTYPE",
    "LC_MESSAGES",
    "TZ",
    "TERM",
    "USER",
    "LOGNAME",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Home {
    Hidden,
    ReadOnly,
    Full,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecCfg {
    pub home: Home,
    pub network: bool,
    pub paths: Vec<PathBuf>,
    pub ro_paths: Vec<PathBuf>,
    pub env: Vec<String>,
    /// Keep the command line of an allowed exec of plain words in the audit
    /// log, for `paddock tools suggest`. Off by default: arguments can hold
    /// private text that no filter finds.
    pub remember_allowed: bool,
}

impl Default for ExecCfg {
    fn default() -> Self {
        Self {
            home: Home::Hidden,
            network: true,
            paths: Vec::new(),
            ro_paths: Vec::new(),
            env: Vec::new(),
            remember_allowed: false,
        }
    }
}

impl ExecCfg {
    /// `[exec]` of the user configuration, never of a project file.
    pub fn load(env: &HostEnv) -> Result<Self> {
        let file = env.paddock_config().join("config.toml");
        let text = match std::fs::read_to_string(&file) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e).with_context(|| format!("cannot read {}", file.display())),
        };
        Self::parse(env, &text).with_context(|| format!("{} [exec] is invalid", file.display()))
    }

    pub fn parse(env: &HostEnv, text: &str) -> Result<Self> {
        let doc: toml::Value = toml::from_str(text)?;
        let mut c = Self::default();
        let Some(t) = doc.get("exec").and_then(|e| e.as_table()) else {
            return Ok(c);
        };
        let paths = |k: &str| -> Result<Vec<PathBuf>> {
            let Some(v) = t.get(k) else {
                return Ok(Vec::new());
            };
            let list = v
                .as_array()
                .with_context(|| format!("{k} must be a list"))?;
            list.iter()
                .map(|p| {
                    let s = p.as_str().with_context(|| format!("{k} holds paths"))?;
                    let p = env.expand(s, &env.home);
                    if p == Path::new("/") || p == env.home {
                        bail!("{k}: {s} would open the whole system or home; use home = \"full\"");
                    }
                    Ok(p)
                })
                .collect()
        };
        for (k, v) in t {
            match k.as_str() {
                "home" => {
                    c.home = match v.as_str() {
                        Some("hidden") => Home::Hidden,
                        Some("read-only") => Home::ReadOnly,
                        Some("full") => Home::Full,
                        _ => bail!("home must be hidden, read-only or full"),
                    }
                }
                "network" => c.network = v.as_bool().context("network must be true or false")?,
                "remember_allowed" => {
                    c.remember_allowed = v
                        .as_bool()
                        .context("remember_allowed must be true or false")?
                }
                "paths" => c.paths = paths("paths")?,
                "ro_paths" => c.ro_paths = paths("ro_paths")?,
                "env" => {
                    c.env = v
                        .as_array()
                        .context("env must be a list of names")?
                        .iter()
                        .map(|n| {
                            n.as_str()
                                .filter(|n| {
                                    !n.is_empty()
                                        && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                                })
                                .map(str::to_string)
                                .context("env holds variable names")
                        })
                        .collect::<Result<_>>()?
                }
                other => bail!("unknown key {other}"),
            }
        }
        Ok(c)
    }

    /// The variables that the command gets, from the signed host
    /// environment.
    pub fn pass_env(&self, host: &BTreeMap<String, String>) -> BTreeMap<String, String> {
        host.iter()
            .filter(|(k, _)| BASE_ENV.contains(&k.as_str()) || self.env.iter().any(|e| e == *k))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }
}

/// How the command is isolated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Isolation {
    /// A transient systemd user unit.
    Systemd,
    /// bwrap, where there is no systemd user instance.
    Bwrap,
    /// No isolation: only for the unit tests of the broker.
    None,
}

/// The isolation that this host can give.
pub fn detect(env: &HostEnv) -> Result<Isolation> {
    let systemd = env.which("systemd-run").is_some()
        && Command::new("systemctl")
            .args(["--user", "is-system-running"])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .map(|o| {
                let s = String::from_utf8_lossy(&o.stdout);
                matches!(s.trim(), "running" | "degraded")
            })
            .unwrap_or(false);
    if systemd {
        return Ok(Isolation::Systemd);
    }
    let msg = "exec needs a systemd user instance or bwrap to run a command in isolation";
    match env.which("bwrap") {
        Some(bwrap) if bwrap_starts(&bwrap) => Ok(Isolation::Bwrap),
        Some(bwrap) => bail!(
            "{msg}: {} is on PATH but cannot start a sandbox here (for example inside another sandbox)",
            bwrap.display()
        ),
        None => bail!("{msg}"),
    }
}

/// bwrap can start a sandbox with a user namespace. It cannot inside a
/// sandbox that allows no nested namespace. paddock probes each bwrap once.
fn bwrap_starts(bwrap: &Path) -> bool {
    static SEEN: std::sync::Mutex<BTreeMap<PathBuf, bool>> = std::sync::Mutex::new(BTreeMap::new());
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    *seen.entry(bwrap.to_path_buf()).or_insert_with(|| {
        Command::new(bwrap)
            .args(["--unshare-user", "--ro-bind", "/", "/", "true"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    })
}

/// The command that runs `argv` in `project` with `cfg`, isolated.
#[allow(clippy::too_many_arguments)]
pub fn command(
    how: Isolation,
    cfg: &ExecCfg,
    home: &Path,
    project: &Path,
    argv: &[String],
    env: &BTreeMap<String, String>,
    timeout: u64,
    unit: &str,
) -> Command {
    command_in(how, cfg, home, project, project, argv, env, timeout, unit)
}

/// Like `command`, with another working folder than the project: the
/// project stays writable, and `cwd` must be visible in the view of `cfg`.
#[allow(clippy::too_many_arguments)]
pub fn command_in(
    how: Isolation,
    cfg: &ExecCfg,
    home: &Path,
    project: &Path,
    cwd: &Path,
    argv: &[String],
    env: &BTreeMap<String, String>,
    timeout: u64,
    unit: &str,
) -> Command {
    let s = |p: &Path| p.display().to_string();
    match how {
        Isolation::None => {
            let mut c = Command::new(&argv[0]);
            c.args(&argv[1..]).current_dir(cwd).env_clear().envs(env);
            c
        }
        Isolation::Systemd => {
            let mut c = Command::new("systemd-run");
            c.args(["--user", "--pipe", "--wait", "--collect", "--quiet"])
                .arg(format!("--unit={unit}"));
            let mut p: Vec<String> = vec![
                "ProtectSystem=strict".into(),
                "PrivateTmp=yes".into(),
                "NoNewPrivileges=yes".into(),
                "RestrictNamespaces=yes".into(),
                "RestrictSUIDSGID=yes".into(),
                "LockPersonality=yes".into(),
                "SystemCallFilter=@system-service".into(),
                format!("RuntimeMaxSec={}", timeout + 5),
                format!("WorkingDirectory={}", s(cwd)),
            ];
            match cfg.home {
                Home::Hidden => p.push("ProtectHome=tmpfs".into()),
                Home::ReadOnly => p.push("ProtectHome=read-only".into()),
                Home::Full => p.push(format!("ReadWritePaths={}", s(home))),
            }
            p.push(format!("BindPaths={}", s(project)));
            for d in &cfg.paths {
                p.push(format!("BindPaths={}", s(d)));
            }
            for d in &cfg.ro_paths {
                p.push(format!("BindReadOnlyPaths={}", s(d)));
            }
            if cfg.network {
                p.push("RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6 AF_NETLINK".into());
            } else {
                p.push("PrivateNetwork=yes".into());
                p.push("RestrictAddressFamilies=AF_UNIX".into());
            }
            for prop in p {
                c.arg("-p").arg(prop);
            }
            for (k, v) in env {
                c.arg(format!("--setenv={k}={v}"));
            }
            c.arg("--").args(argv);
            c.env_clear();
            // systemd-run talks to the user manager over the runtime folder.
            for k in ["XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS"] {
                if let Ok(v) = std::env::var(k) {
                    c.env(k, v);
                }
            }
            c
        }
        Isolation::Bwrap => {
            let mut c = Command::new("bwrap");
            c.args([
                "--die-with-parent",
                "--new-session",
                "--unshare-pid",
                "--unshare-ipc",
            ]);
            c.args([
                "--ro-bind",
                "/",
                "/",
                "--dev",
                "/dev",
                "--proc",
                "/proc",
                "--tmpfs",
                "/tmp",
            ]);
            match cfg.home {
                Home::Hidden => {
                    c.args(["--tmpfs", &s(home)]);
                }
                Home::ReadOnly => {}
                Home::Full => {
                    c.args(["--bind", &s(home), &s(home)]);
                }
            }
            c.args(["--bind", &s(project), &s(project)]);
            for d in &cfg.paths {
                c.args(["--bind", &s(d), &s(d)]);
            }
            for d in &cfg.ro_paths {
                c.args(["--ro-bind", &s(d), &s(d)]);
            }
            if !cfg.network {
                c.arg("--unshare-net");
            }
            c.args(["--chdir", &s(cwd), "--"]).args(argv);
            c.env_clear().envs(env);
            c
        }
    }
}

/// systemd's own exit codes when a unit cannot start (for example 218 for
/// capabilities or 226 for a namespace), as text for the agent.
pub fn setup_failure(how: Isolation, code: i32, stderr: &str) -> Option<String> {
    (how == Isolation::Systemd && (200..=245).contains(&code) && stderr.trim().is_empty()).then(
        || {
            format!(
                "the isolated unit could not start (systemd exit code {code}); see journalctl --user"
            )
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A host whose PATH holds only `bwrap`, a script with this body.
    fn fake_bwrap(dir: &Path, body: &str) -> HostEnv {
        use std::os::unix::fs::PermissionsExt;
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let f = bin.join("bwrap");
        std::fs::write(&f, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o755)).unwrap();
        let vars = [("PATH".to_string(), bin.display().to_string())].into();
        HostEnv::from_parts(dir.join("home"), "demo".into(), 1, 1, vars)
    }

    #[test]
    fn a_bwrap_that_cannot_start_gives_no_isolation() {
        let dir = tempfile::tempdir().unwrap();
        let e = detect(&fake_bwrap(dir.path(), "exit 1")).unwrap_err();
        let m = format!("{e:#}");
        assert!(m.contains("run a command in isolation"), "{m}");
        assert!(m.contains("cannot start a sandbox"), "{m}");
        let ok = tempfile::tempdir().unwrap();
        assert_eq!(
            detect(&fake_bwrap(ok.path(), "exit 0")).unwrap(),
            Isolation::Bwrap
        );
        // No bwrap at all: the same message, without the probe.
        let none = HostEnv::from_parts("/nowhere".into(), "demo".into(), 1, 1, Default::default());
        let m = format!("{:#}", detect(&none).unwrap_err());
        assert!(
            m.contains("run a command in isolation") && !m.contains("cannot start"),
            "{m}"
        );
    }

    fn env() -> HostEnv {
        HostEnv::from_parts("/home/demo".into(), "demo".into(), 1, 1, Default::default())
    }

    #[test]
    fn the_default_hides_home_and_keeps_the_network() {
        assert_eq!(ExecCfg::parse(&env(), "").unwrap(), ExecCfg::default());
        let c = ExecCfg::parse(
            &env(),
            "[exec]\nhome = \"read-only\"\nnetwork = false\npaths = [\"~/src/other\"]\nro_paths = [\"/srv/data\"]\nenv = [\"GOPATH\"]\n",
        )
        .unwrap();
        assert_eq!(c.home, Home::ReadOnly);
        assert!(!c.network);
        assert_eq!(c.paths, [PathBuf::from("/home/demo/src/other")]);
        assert_eq!(c.ro_paths, [PathBuf::from("/srv/data")]);
        assert_eq!(c.env, ["GOPATH"]);
        for bad in [
            "[exec]\nhome = \"open\"\n",
            "[exec]\npaths = [\"~\"]\n",
            "[exec]\nro_paths = [\"/\"]\n",
            "[exec]\nenv = [\"A=B\"]\n",
            "[exec]\nshell = true\n",
        ] {
            assert!(ExecCfg::parse(&env(), bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn only_a_few_host_variables_pass() {
        let host: BTreeMap<String, String> = [
            ("PATH", "/usr/bin"),
            ("GITHUB_TOKEN", "secret"),
            ("GOPATH", "/go"),
            ("LANG", "C.UTF-8"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let mut c = ExecCfg::default();
        let got = c.pass_env(&host);
        assert_eq!(got.keys().collect::<Vec<_>>(), ["LANG", "PATH"]);
        c.env = vec!["GOPATH".into()];
        assert!(c.pass_env(&host).contains_key("GOPATH"));
        assert!(!c.pass_env(&host).contains_key("GITHUB_TOKEN"));
    }

    #[test]
    fn the_systemd_unit_has_the_hardening() {
        let c = command(
            Isolation::Systemd,
            &ExecCfg::default(),
            Path::new("/home/demo"),
            Path::new("/home/demo/proj"),
            &["make".into(), "test".into()],
            &BTreeMap::new(),
            60,
            "paddock-exec-x",
        );
        let args: Vec<String> = c
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        for want in [
            "ProtectHome=tmpfs",
            "ProtectSystem=strict",
            "NoNewPrivileges=yes",
            "RestrictNamespaces=yes",
            "BindPaths=/home/demo/proj",
            "WorkingDirectory=/home/demo/proj",
            "RuntimeMaxSec=65",
        ] {
            assert!(args.iter().any(|a| a == want), "{want} missing: {args:?}");
        }
        assert_eq!(&args[args.len() - 3..], ["--", "make", "test"]);
    }

    /// The real systemd unit, where this host has a systemd user instance:
    /// what the command can see and change. Opt in:
    /// `cargo test --lib execbox -- --ignored`.
    #[test]
    #[ignore]
    fn the_systemd_unit_isolates_the_command() {
        let host = HostEnv::detect().unwrap();
        if detect(&host).ok() != Some(Isolation::Systemd) {
            eprintln!("skipped: no systemd user instance");
            return;
        }
        let t = tempfile::Builder::new()
            .prefix("paddock-execbox-")
            .tempdir_in(host.home.join(".cache"))
            .unwrap();
        let project = t.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(t.path().join("outside.txt"), "private").unwrap();
        let script = format!(
            "test -e '{}' && echo OUTSIDE_VISIBLE || echo OUTSIDE_HIDDEN; \
             touch /usr/paddock-x 2>/dev/null && echo USR_WRITABLE || echo USR_RO; \
             touch made && echo PROJECT_RW; unshare -U true 2>/dev/null && echo NS_OK || echo NS_BLOCKED; \
             echo TOKEN=${{PADDOCK_TEST_TOKEN:-none}}",
            t.path().join("outside.txt").display()
        );
        let mut env = BTreeMap::new();
        env.insert("PATH".to_string(), std::env::var("PATH").unwrap());
        let mut cmd = command(
            Isolation::Systemd,
            &ExecCfg::default(),
            &host.home,
            &project,
            &["sh".into(), "-c".into(), script],
            &env,
            30,
            "paddock-exec-test",
        );
        let out = cmd.output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success(), "{out:?}");
        for want in [
            "OUTSIDE_HIDDEN",
            "USR_RO",
            "PROJECT_RW",
            "NS_BLOCKED",
            "TOKEN=none",
        ] {
            assert!(text.contains(want), "{want} missing:\n{text}");
        }
        let cfg = ExecCfg {
            network: false,
            ..ExecCfg::default()
        };
        let mut cmd = command(
            Isolation::Systemd,
            &cfg,
            &host.home,
            &project,
            &[
                "sh".into(),
                "-c".into(),
                "tail -n +3 /proc/net/dev | cut -d: -f1 | tr -d ' '".into(),
            ],
            &env,
            30,
            "paddock-exec-test-net",
        );
        let out = cmd.output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "lo");
    }
}
