//! `paddock omarchy-usage-update` end to end, with a fake Omarchy tree (its
//! update command and collectors) in a temporary home. The fake collectors
//! never use the network, and no test here talks to a vendor.
//!
//! - Without a held login, Omarchy's command runs with the same arguments.
//! - A Claude Code login that a sandbox holds: the host command gets
//!   `--except claude`, and the collector runs in a paddock sandbox that sees
//!   the sandbox's copy of the login in place of the host file, and the home
//!   read-only.
//! - A held Codex login is not checked: the record keeps its numbers, marked
//!   as paused.

mod common;

use paddock::env::HostEnv;
use paddock::state::{HeldLogin, Running};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

fn tools_work() -> bool {
    let _g = EXEC.lock().unwrap_or_else(|e| e.into_inner());
    let bwrap = Command::new("bwrap")
        .args(["--unshare-user", "--ro-bind", "/", "/", "true"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    let pasta = Command::new("pasta")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    bwrap && pasta
}

/// Writing a script while another test thread starts a process lets the
/// child inherit the open file, and running the script then fails with
/// "Text file busy". Both happen under this lock.
static EXEC: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn write_exec(p: &Path, text: &str) {
    use std::os::unix::fs::PermissionsExt;
    let _g = EXEC.lock().unwrap_or_else(|e| e.into_inner());
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Records its arguments.
const FAKE_UPDATE: &str = "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$HOME/real.log\"\n";

/// Prints a record with the login that it sees, its arguments, and whether
/// it can write the home.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
if touch "$HOME/written" 2>/dev/null; then home=rw; else home=ro; fi
printf '{"id":"claude","login":%s,"args":"%s","home":"%s"}\n' "$(cat "$HOME/.claude/.credentials.json")" "$*" "$home"
"#;

struct World {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    vars: BTreeMap<String, String>,
}

fn world() -> World {
    // Not in /tmp: the sandbox has its own /tmp.
    let tmp = common::tempdir();
    let home = tmp.path().join("home");
    let omarchy = home.join("omarchy");
    write_exec(&omarchy.join("bin/omarchy-agent-usage-update"), FAKE_UPDATE);
    write_exec(&omarchy.join("bin/omarchy-agent-usage-claude"), FAKE_CLAUDE);
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    std::fs::write(home.join(".claude/.credentials.json"), r#"{"t":"host"}"#).unwrap();
    let run = tmp.path().join("run");
    std::fs::create_dir_all(&run).unwrap();
    std::fs::set_permissions(&run, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    let mut vars = BTreeMap::new();
    vars.insert("HOME".into(), home.display().to_string());
    vars.insert("OMARCHY_PATH".into(), omarchy.display().to_string());
    vars.insert("XDG_RUNTIME_DIR".into(), run.display().to_string());
    vars.insert("PATH".into(), std::env::var("PATH").unwrap());
    World {
        _tmp: tmp,
        home,
        vars,
    }
}

impl World {
    fn env(&self) -> HostEnv {
        HostEnv::from_parts(self.home.clone(), "demo".into(), 1, 1, self.vars.clone())
    }

    fn update(&self, args: &[&str]) -> std::process::Output {
        let child = {
            let _g = EXEC.lock().unwrap_or_else(|e| e.into_inner());
            Command::new(env!("CARGO_BIN_EXE_paddock"))
                .arg("omarchy-usage-update")
                .arg("--")
                .args(args)
                .env_clear()
                .envs(&self.vars)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        };
        child.wait_with_output().unwrap()
    }

    fn real_log(&self) -> String {
        std::fs::read_to_string(self.home.join("real.log")).unwrap_or_default()
    }

    fn record(&self, agent: &str) -> serde_json::Value {
        let f = self
            .home
            .join(".local/state/omarchy/agents/usage")
            .join(format!("{agent}.json"));
        serde_json::from_str(&std::fs::read_to_string(f).unwrap()).unwrap()
    }

    /// A running sandbox that holds a copy of a login. The test process
    /// stands in for its process, so it counts as running.
    fn hold(&self, id: &str, harness: &str, sandbox: &Path, rel: &str, store: &Path) {
        Running {
            id: id.into(),
            harness: harness.into(),
            project: self.home.clone(),
            backend: "bwrap".into(),
            network: "egress".into(),
            gui: "off".into(),
            pid: std::process::id(),
            started: 1,
            auth: "shared".into(),
            window_pid: None,
            warnings: vec![],
            host_pid: std::process::id(),
            parent: None,
            spawn_id: None,
            logins: vec![HeldLogin {
                home: sandbox.to_path_buf(),
                rel: rel.into(),
                store: store.to_path_buf(),
            }],
            unit: None,
            kv_project: None,
            pasture: None,
            credentials: Vec::new(),
            gateway: Vec::new(),
        }
        .register(&self.env())
        .unwrap();
    }
}

#[test]
fn without_a_held_login_omarchy_s_command_runs_as_asked() {
    let w = world();
    let out = w.update(&["--limits-only", "--except", "kimi"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(w.real_log(), "--limits-only --except kimi\n");
}

#[test]
fn a_held_claude_login_is_checked_only_from_paddock_s_side() {
    if !tools_work() {
        eprintln!("skipped: bwrap or pasta does not work here");
        return;
    }
    let w = world();
    let sb = w.home.join(".local/share/paddock/homes/claude/shared/p");
    std::fs::create_dir_all(sb.join(".claude")).unwrap();
    std::fs::write(sb.join(".claude/.credentials.json"), r#"{"t":"sandbox"}"#).unwrap();
    let host_login = w.home.join(".claude/.credentials.json");
    w.hold(
        "claude-p",
        "claude",
        &sb,
        ".claude/.credentials.json",
        &host_login,
    );

    let out = w.update(&["--force"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(w.real_log(), "--force --except claude\n");
    let r = w.record("claude");
    assert_eq!(r["login"]["t"], "sandbox", "{r}");
    assert_eq!(r["args"], "--force");
    assert_eq!(r["home"], "ro");
    assert_eq!(
        std::fs::read_to_string(&host_login).unwrap(),
        r#"{"t":"host"}"#
    );
    assert!(!w.home.join("written").exists());

    // An agent that the panel did not ask for is left alone.
    std::fs::remove_file(w.home.join("real.log")).unwrap();
    let out = w.update(&["grok"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(w.real_log(), "grok\n");
}

#[test]
fn a_held_codex_login_is_not_checked_at_all() {
    let w = world();
    let sb = w.home.join(".local/share/paddock/homes/codex/shared/p");
    std::fs::create_dir_all(sb.join(".codex")).unwrap();
    std::fs::write(sb.join(".codex/auth.json"), "{}").unwrap();
    std::fs::create_dir_all(w.home.join(".codex")).unwrap();
    let host_login = w.home.join(".codex/auth.json");
    std::fs::write(&host_login, "{}").unwrap();
    let usage = w.home.join(".local/state/omarchy/agents/usage");
    std::fs::create_dir_all(&usage).unwrap();
    std::fs::write(
        usage.join("codex.json"),
        r#"{"id":"codex","limits":[{"percent":0.98}],"limitsStale":false}"#,
    )
    .unwrap();
    // Omarchy's collector is there, but must not run.
    write_exec(
        &w.home.join("omarchy/bin/omarchy-agent-usage-codex"),
        "#!/bin/sh\ntouch \"$HOME/codex-ran\"\nexit 1\n",
    );
    w.hold("codex-p", "codex", &sb, ".codex/auth.json", &host_login);

    let out = w.update(&[]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(w.real_log(), "--except codex\n");
    assert!(!w.home.join("codex-ran").exists());
    let r = w.record("codex");
    assert_eq!(r["limits"][0]["percent"], 0.98);
    assert_eq!(r["limitsStale"], true);
    assert_eq!(r["usageStatusText"], "Limits paused");
}

/// Omarchy's real Claude Code collector, if this host has Omarchy, in the
/// collector sandbox with a fake login and without any network: nothing
/// leaves the machine. It must still print a record from a read-only home.
/// Opt in: `cargo test --test omarchy_usage -- --ignored`.
#[test]
#[ignore]
fn omarchy_s_collector_runs_in_the_sandbox_without_network() {
    let collector = Path::new("/usr/share/omarchy/bin/omarchy-agent-usage-claude");
    let env = HostEnv::detect().unwrap();
    let store = env.home.join(".claude/.credentials.json");
    if !collector.exists() || !store.is_file() || !tools_work() {
        eprintln!("skipped: no Omarchy collector or Claude Code login file here");
        return;
    }
    let t = common::tempdir();
    let sb = t.path().join("sb");
    std::fs::create_dir_all(sb.join(".claude")).unwrap();
    std::fs::write(
        sb.join(".claude/.credentials.json"),
        r#"{"claudeAiOauth":{"accessToken":"fake","refreshToken":"fake","expiresAt":4102444800000}}"#,
    )
    .unwrap();
    let run = t.path().join("run");
    std::fs::create_dir_all(&run).unwrap();
    let held = [HeldLogin {
        home: sb,
        rel: ".claude/.credentials.json".into(),
        store,
    }];
    let binds = paddock::omarchy_usage::login_copies(&run, &held).unwrap();
    assert_eq!(binds.len(), 1);
    let out = paddock::omarchy_usage::collector_command(
        &env,
        &run,
        &binds,
        collector,
        &["--limits-only".to_string()],
        false,
    )
    .unwrap()
    .output()
    .unwrap();
    let r: serde_json::Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&out.stderr)));
    assert_eq!(r["id"], "claude");
    eprintln!(
        "status: {} | help: {}",
        r["usageStatusText"], r["authHelpText"]
    );
}
