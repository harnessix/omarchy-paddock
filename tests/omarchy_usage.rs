//! `paddock omarchy-usage-update` end to end, with a fake Omarchy tree (its
//! update command and collectors) in a temporary home. The fake collectors
//! never use the network, and no test here talks to a vendor.
//!
//! - Without a held login, Omarchy's command runs with the same arguments.
//! - A Claude Code login that a sandbox holds: the host command gets
//!   `--except claude`, and the collector runs in a paddock sandbox that sees
//!   the sandbox's copy of the login in place of the host file, and the home
//!   read-only.
//! - A held Codex login without a keeper is not checked: the record keeps
//!   its numbers, marked as paused. With a keeper, Omarchy's collector runs
//!   in the keeper sandbox, and a refresh that it makes goes back to the
//!   host and into the sandbox.

mod common;

use paddock::env::HostEnv;
use paddock::state::{HeldLogin, Running};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

fn tools_work() -> bool {
    let _g = EXEC.lock().unwrap_or_else(|e| e.into_inner());
    let bwrap = common::bwrap_works();
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

/// Records its arguments in the panel's state folder, the one place of the
/// home that the usage box can write.
const FAKE_UPDATE: &str =
    "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$HOME/.local/state/omarchy/agents/real.log\"\n";

/// The usage box can start here (bwrap with a user namespace and overlays).
fn box_works(w: &World) -> bool {
    let _g = EXEC.lock().unwrap_or_else(|e| e.into_inner());
    paddock::usage_box::probe(&w.env()).is_ok()
}

/// Prints a record with the login that it sees, its arguments, and whether
/// it can write the home.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
if touch "$HOME/.claude/written" 2>/dev/null; then home=rw; else home=ro; fi
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
        std::fs::read_to_string(self.home.join(".local/state/omarchy/agents/real.log"))
            .unwrap_or_default()
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
            agent: Vec::new(),
            secret_files: Vec::new(),
            kept_logins: Vec::new(),
            watched: false,
            relay: false,
        }
        .register(&self.env())
        .unwrap();
    }
}

#[test]
fn without_a_held_login_omarchy_s_command_runs_as_asked() {
    let w = world();
    if !box_works(&w) {
        eprintln!("skipped: the usage box cannot start here");
        return;
    }
    let out = w.update(&["--limits-only", "--except", "kimi"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(w.real_log(), "--limits-only --except kimi\n");
}

#[test]
fn a_held_claude_login_is_checked_only_from_paddock_s_side() {
    let w = world();
    if !tools_work() || !box_works(&w) {
        eprintln!("skipped: bwrap, pasta or the usage box does not work here");
        return;
    }
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
    assert!(!w.home.join(".claude/written").exists());

    // An agent that the panel did not ask for is left alone.
    std::fs::remove_file(w.home.join(".local/state/omarchy/agents/real.log")).unwrap();
    let out = w.update(&["grok"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(w.real_log(), "grok\n");
}

#[test]
fn a_held_codex_login_is_not_checked_at_all() {
    let w = world();
    if !box_works(&w) {
        eprintln!("skipped: the usage box cannot start here");
        return;
    }
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

/// Plays `codex app-server`: rate limits, and the refresh that Codex does
/// by itself when the access token has less than 5 minutes left. It spends
/// the refresh token as a service would: once.
const FAKE_CODEX: &str = r#"#!/usr/bin/python3
import base64, json, os, sys, time
def b64(v): return base64.urlsafe_b64encode(json.dumps(v).encode()).decode().rstrip("=")
def exp_of(at):
    p = at.split(".")[1]
    return json.loads(base64.urlsafe_b64decode(p + "=" * (-len(p) % 4)))["exp"]
home = os.environ.get("CODEX_HOME") or os.path.expanduser("~/.codex")
auth = os.path.join(home, "auth.json")
for line in sys.stdin:
    msg = json.loads(line); mid = msg.get("id"); m = msg.get("method")
    if m == "initialize":
        print(json.dumps({"id": mid, "result": {}}), flush=True)
    elif m == "account/rateLimits/read":
        a = json.load(open(auth)); t = a["tokens"]
        if exp_of(t["access_token"]) <= time.time() + 300:
            rt = t["refresh_token"]
            with open(os.path.join(home, "used.log"), "a") as f: f.write(rt + "\n")
            t["refresh_token"] = "rt-%d" % (int(rt.split("-")[1]) + 1)
            t["access_token"] = "%s.%s.sig" % (b64({}), b64({"exp": int(time.time()) + 3600}))
            json.dump(a, open(auth + ".tmp", "w")); os.replace(auth + ".tmp", auth)
        print(json.dumps({"id": mid, "result": {"rateLimits": {"primary": {"usedPercent": 42}}}}), flush=True)
"#;

/// Omarchy's Codex collector, faked: it asks the app-server and prints a
/// record with the percent and what it saw of the home.
const FAKE_CODEX_COLLECTOR: &str = r#"#!/usr/bin/python3
import json, os, subprocess
if os.access(os.path.expanduser("~"), os.W_OK): home = "rw"
else: home = "ro"
p = subprocess.Popen(["codex", "app-server"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
def ask(i, m):
    p.stdin.write(json.dumps({"id": i, "method": m, "params": {}}) + "\n"); p.stdin.flush()
    return json.loads(p.stdout.readline())
ask(1, "initialize")
r = ask(2, "account/rateLimits/read")
p.kill()
print(json.dumps({"id": "codex", "limits": [{"percent": r["result"]["rateLimits"]["primary"]["usedPercent"]}], "limitsStale": False, "home": home, "host": open("/etc/hostname").read().strip() if os.path.exists("/etc/hostname") else os.uname().nodename}))
"#;

fn codex_login(exp: u64, rt: &str) -> String {
    use base64::Engine as _;
    let b64 = |v: serde_json::Value| {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v.to_string())
    };
    format!(
        r#"{{"auth_mode":"chatgpt","tokens":{{"id_token":"e30.e30.sig","access_token":"{}.{}.sig","refresh_token":"{rt}","account_id":"a"}},"last_refresh":"2026-11-01T00:00:00Z"}}"#,
        b64(serde_json::json!({})),
        b64(serde_json::json!({"exp": exp}))
    )
}

#[test]
fn a_kept_codex_login_is_checked_in_the_keeper_sandbox() {
    if !tools_work()
        || !Command::new("python3")
            .arg("-c")
            .arg("pass")
            .status()
            .is_ok_and(|s| s.success())
    {
        eprintln!("skipped: needs bwrap, pasta and python3");
        return;
    }
    let mut w = world();
    if !box_works(&w) {
        eprintln!("skipped: the usage box cannot start here");
        return;
    }
    // A fake codex first on PATH, and a profile without the mise lookup,
    // so that the real codex is never found.
    let bin = w.home.parent().unwrap().join("tools/bin");
    write_exec(&bin.join("codex"), FAKE_CODEX);
    let path = format!("{}:{}", bin.display(), w.vars["PATH"]);
    w.vars.insert("PATH".into(), path);
    std::fs::create_dir_all(w.home.join(".config/paddock/harness.d")).unwrap();
    std::fs::write(
        w.home.join(".config/paddock/harness.d/codex.toml"),
        "name = \"codex\"\n[locate]\ncommands = [\"codex\"]\n[state]\nauth = [\".codex/auth.json\"]\n",
    )
    .unwrap();
    write_exec(
        &w.home.join("omarchy/bin/omarchy-agent-usage-codex"),
        FAKE_CODEX_COLLECTOR,
    );
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    // Two minutes left: the app-server refreshes on its own.
    std::fs::create_dir_all(w.home.join(".codex")).unwrap();
    let store = w.home.join(".codex/auth.json");
    std::fs::write(&store, codex_login(now + 120, "rt-1")).unwrap();
    let sb = w.home.join(".local/share/paddock/homes/codex/shared/p");
    std::fs::create_dir_all(sb.join(".codex")).unwrap();
    std::fs::write(sb.join(".codex/auth.json"), codex_login(now + 120, "")).unwrap();
    w.hold("codex-p", "codex", &sb, ".codex/auth.json", &store);
    // The sandbox's login is kept: the record says so.
    let env = w.env();
    let mut r = Running::list(&env)
        .into_iter()
        .find(|r| r.id == "codex-p")
        .unwrap();
    r.kept_logins = r.logins.clone();
    r.register(&env).unwrap();

    w.vars.insert("PADDOCK_LOG".into(), "debug".into());
    let out = w.update(&[]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(w.real_log(), "--except codex\n");
    let f = w.home.join(".local/state/omarchy/agents/usage/codex.json");
    assert!(
        f.is_file(),
        "no record: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let rec = w.record("codex");
    assert_eq!(rec["limits"][0]["percent"], 42, "{rec}");
    assert_eq!(rec["home"], "ro", "{rec}");
    assert!(rec.get("usageStatusText").is_none(), "still paused: {rec}");
    // The refresh went back to the host, once, and to the sandbox without
    // its refresh token.
    let host: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&store).unwrap()).unwrap();
    assert_eq!(host["tokens"]["refresh_token"], "rt-2");
    let inside: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(sb.join(".codex/auth.json")).unwrap())
            .unwrap();
    assert_eq!(inside["tokens"]["refresh_token"], "");
    assert_eq!(
        inside["tokens"]["access_token"],
        host["tokens"]["access_token"]
    );
    // The host's own folder was never written: the overlay took the writes.
    assert!(!w.home.join(".codex/used.log").exists());
}

#[test]
fn forage_refreshes_at_most_once_in_five_minutes_and_keeps_a_ledger() {
    let w = world();
    if !box_works(&w) {
        eprintln!("skipped: the usage box cannot start here");
        return;
    }
    // Omarchy's update writes a fresh record each time it runs.
    write_exec(
        &w.home.join("omarchy/bin/omarchy-agent-usage-update"),
        r#"#!/bin/sh
mkdir -p "$HOME/.local/state/omarchy/agents"
printf '%s\n' "$*" >> "$HOME/.local/state/omarchy/agents/real.log"
n=$(wc -l < "$HOME/.local/state/omarchy/agents/real.log")
mkdir -p "$HOME/.local/state/omarchy/agents/usage"
printf '{"id":"grok","limitsFetchedAt":%s000,"limitsStale":false,"limits":[{"label":"Weekly","percent":0.%s,"resetsAt":"2099-01-01T00:00:00+00:00"}]}\n' "$(date +%s)" "$n" > "$HOME/.local/state/omarchy/agents/usage/grok.json"
"#,
    );
    let run = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_paddock"))
            .arg("forage")
            .args(args)
            .env_clear()
            .envs(&w.vars)
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()
    };
    let v = run(&["grok", "--refresh", "--json"]);
    assert_eq!(v["windows"][0]["agent"], "grok", "{v}");
    assert_eq!(v["windows"][0]["used"], 0.1);
    assert_eq!(w.real_log(), "grok\n");
    // Asked again within five minutes: Omarchy is not asked.
    run(&["grok", "--refresh", "--json"]);
    assert_eq!(w.real_log(), "grok\n");
    // The ledger holds the numbers only.
    let ledger =
        std::fs::read_to_string(w.home.join(".local/state/paddock/forage/grok.jsonl")).unwrap();
    assert_eq!(ledger.lines().count(), 1, "{ledger}");
    assert!(ledger.contains("\"used\":0.1"));
}

/// The fake Omarchy command for the usage box: it lists what it sees,
/// tries to write an inherited folder twice, writes a record, and ends with
/// its own output and exit code.
const FAKE_BOX_UPDATE: &str = r#"#!/bin/sh
s="$HOME/.local/state/omarchy/agents"
mkdir -p "$s/usage"
{
  ls -A "$HOME/.codex/sessions"
  [ -e "$HOME/.ssh" ] && echo ssh-visible
  [ -e "$HOME/notes.txt" ] && echo notes-visible
  [ -e "$HOME/.claude.json" ] && echo claude-json
  env | cut -d= -f1 | sort | tr '\n' ' '
  echo
} > "$s/seen.txt"
touch "$HOME/.codex/written" || true
touch "$HOME/.codex/sessions/written" || true
printf '{"id":"fake"}\n' > "$s/usage/fake.json"
echo "out $*"
echo "err line" >&2
exit 3
"#;

fn box_world() -> World {
    let w = world();
    write_exec(
        &w.home.join("omarchy/bin/omarchy-agent-usage-update"),
        FAKE_BOX_UPDATE,
    );
    std::fs::create_dir_all(w.home.join(".codex/sessions")).unwrap();
    std::fs::write(w.home.join(".codex/sessions/host-1.jsonl"), "{}\n").unwrap();
    let sb = w
        .home
        .join(".local/share/paddock/homes/codex/shared/proj/.codex/sessions");
    std::fs::create_dir_all(&sb).unwrap();
    std::fs::write(sb.join("sandbox-1.jsonl"), "{}\n").unwrap();
    std::fs::create_dir_all(w.home.join(".ssh")).unwrap();
    std::fs::write(w.home.join(".ssh/id_ed25519"), "key").unwrap();
    std::fs::write(w.home.join("notes.txt"), "private").unwrap();
    std::fs::write(w.home.join(".claude.json"), "{}").unwrap();
    w
}

fn shims_exec(w: &World, path: Option<&str>, args: &[&str]) -> std::process::Output {
    let mut vars = w.vars.clone();
    if let Some(p) = path {
        vars.insert("PATH".into(), p.into());
    }
    let child = {
        let _g = EXEC.lock().unwrap_or_else(|e| e.into_inner());
        Command::new(env!("CARGO_BIN_EXE_paddock"))
            .args(["shims", "exec", "omarchy-agent-usage-update"])
            .args(args)
            .env_clear()
            .envs(&vars)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap()
    };
    child.wait_with_output().unwrap()
}

#[test]
fn without_bwrap_the_wrapper_fails_closed() {
    let w = box_world();
    let marker = w.home.join(".local/state/omarchy/agents/seen.txt");
    // No bwrap on PATH; paddock itself needs nothing from PATH here.
    let out = shims_exec(&w, Some("/nonexistent"), &["--force"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert_ne!(out.status.code(), Some(0), "{out:?}");
    assert!(out.stdout.is_empty(), "{out:?}");
    assert_eq!(err.trim().lines().count(), 1, "{err}");
    assert!(err.contains("bwrap is not on PATH"), "{err}");
    assert!(err.contains("paddock shims remove"), "{err}");
    assert!(!marker.exists(), "Omarchy's command ran on the host");
    // A bwrap that cannot start a box: the same.
    let bin = w.home.parent().unwrap().join("broken-bin");
    write_exec(&bin.join("bwrap"), "#!/bin/sh\nexit 1\n");
    let out = shims_exec(&w, Some(&bin.display().to_string()), &[]);
    assert_ne!(out.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&out.stderr).contains("cannot start a sandbox"));
    assert!(!marker.exists());
}

#[test]
fn the_usage_box_merges_sessions_and_hides_the_rest_of_the_home() {
    let w = box_world();
    if !box_works(&w) {
        eprintln!("skipped: the usage box cannot start here (bwrap, user namespaces, overlayfs)");
        return;
    }
    let out = shims_exec(&w, None, &["--force", "codex"]);
    let err = String::from_utf8_lossy(&out.stderr);
    // Output and exit code pass through unchanged.
    assert_eq!(out.status.code(), Some(3), "{err}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "out --force codex\n");
    assert!(err.contains("err line\n"), "{err}");
    assert_eq!(
        err.matches(paddock::usage_box::READ_ONLY_NOTE).count(),
        1,
        "{err}"
    );
    let state = w.home.join(".local/state/omarchy/agents");
    let seen = std::fs::read_to_string(state.join("seen.txt")).unwrap();
    let mut lines = seen.lines();
    assert_eq!(lines.next(), Some("host-1.jsonl"), "{seen}");
    assert_eq!(lines.next(), Some("sandbox-1.jsonl"), "{seen}");
    assert_eq!(lines.next(), Some("claude-json"), "{seen}");
    assert!(!seen.contains("ssh-visible") && !seen.contains("notes-visible"));
    let vars: Vec<&str> = lines.next().unwrap().split_whitespace().collect();
    for v in &vars {
        assert!(
            [
                "HOME",
                "PATH",
                "OMARCHY_PATH",
                "LANG",
                "XDG_STATE_HOME",
                "XDG_CACHE_HOME",
                "PWD",
                "SHLVL",
                "_",
                "OLDPWD"
            ]
            .contains(v),
            "{v} reached the box: {vars:?}"
        );
    }
    // The record landed, and nothing was written into the inherited folders.
    assert!(state.join("usage/fake.json").is_file());
    assert!(!w.home.join(".codex/written").exists());
    assert!(!w.home.join(".codex/sessions/written").exists());
    assert!(!w
        .home
        .join(".local/share/paddock/homes/codex/shared/proj/.codex/sessions/written")
        .exists());
}

#[test]
fn codex_and_grok_copy_no_sessions_back() {
    let w = world();
    let env = w.env();
    let f = paddock::profile::HarnessFactory::new(&env).unwrap();
    for name in ["codex", "grok"] {
        let p = f.profile(name).unwrap();
        let copy_back = p
            .state
            .history
            .as_ref()
            .map(|h| h.copy_back.clone())
            .unwrap_or_default();
        assert!(copy_back.is_empty(), "{name}: {copy_back:?}");
    }
    // The usage box still shows their folders.
    let folders = paddock::usage_box::folders(&env);
    assert!(folders[".codex"].contains("codex"));
    assert!(folders[".grok"].contains("grok"));
}
