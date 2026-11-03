//! `paddock omarchy-usage-update` end to end, with a fake Omarchy tree (its
//! update command and collectors) in a temporary home. The fake collectors
//! never contact a vendor endpoint, and never use a real harness or Omarchy's
//! real scripts.

mod common;

use paddock::env::HostEnv;
use paddock::state::{HeldLogin, Running};
use paddock::usage_box::Mount;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

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

fn set_mtime(p: &Path, secs: u64) {
    let c_path = std::ffi::CString::new(p.as_os_str().as_encoded_bytes()).unwrap();
    let t = libc::timespec {
        tv_sec: secs as libc::time_t,
        tv_nsec: 0,
    };
    let times = [t, t];
    unsafe {
        libc::utimensat(libc::AT_FDCWD, c_path.as_ptr(), times.as_ptr(), 0);
    }
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

struct World {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    run: PathBuf,
    vars: BTreeMap<String, String>,
}

fn world() -> World {
    // Not in /tmp: the sandbox has its own /tmp.
    let tmp = common::tempdir();
    let home = tmp.path().join("home");
    let omarchy = home.join("omarchy");
    write_exec(&omarchy.join("bin/omarchy-agent-usage-update"), FAKE_UPDATE);
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
        run,
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

    /// A running sandbox that holds a copy of a login.
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
fn the_except_patch_is_gone_and_the_panel_hook_still_installs_and_removes() {
    let w = world();
    let e = w.env();
    let hypr = paddock::omarchy_usage::hypr_config(&e);
    std::fs::create_dir_all(hypr.parent().unwrap()).unwrap();
    std::fs::write(&hypr, "require(\"default.hypr.omarchy\")\n").unwrap();
    paddock::omarchy_usage::install(&e).unwrap();
    assert!(paddock::omarchy_usage::status(&e).block);
    assert!(paddock::omarchy_usage::status(&e).files);
    paddock::omarchy_usage::remove(&e).unwrap();
    assert!(!paddock::omarchy_usage::status(&e).block);
    assert!(!paddock::omarchy_usage::status(&e).files);

    // Even when a running sandbox holds a Claude login, paddock never adds
    // `--except claude`: the whole update runs in the usage box.
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

    if !box_works(&w) {
        eprintln!("skipped: the usage box cannot start here");
        return;
    }
    let out = w.update(&["--force"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(w.real_log(), "--force\n");
}

const TWO_AGENTS_UPDATE: &str = r#"#!/bin/sh
set -e
u="$HOME/.local/state/omarchy/agents/usage"
mkdir -p "$u"
"$OMARCHY_PATH/bin/omarchy-agent-usage-claude" > "$u/claude.json"
"$OMARCHY_PATH/bin/omarchy-agent-usage-grok" > "$u/grok.json"
"#;

const FAKE_CLAUDE_COLLECTOR: &str = r#"#!/bin/sh
sessions=$(ls -1 "$HOME/.claude/projects" 2>/dev/null | sort | tr '\n' ',' | sed 's/,$//')
if touch "$HOME/.claude/projects/write-test" 2>/dev/null; then sess_w=rw; else sess_w=ro; fi
if sh -c 'echo x >> "$HOME/.claude/.credentials.json"' 2>/dev/null; then login_w=rw; else login_w=ro; fi
[ -e "$HOME/.ssh" ] && ssh=present || ssh=absent
[ -e "$HOME/secret.txt" ] && secret=present || secret=absent
[ -n "${DBUS_SESSION_BUS_ADDRESS:-}" ] && dbus_env=set || dbus_env=unset
[ -e "$ bus_sock_path" ] && bus_sock=present || bus_sock=absent
printf '{"id":"claude","sessions":"%s","login":%s,"sess_w":"%s","login_w":"%s","ssh":"%s","secret":"%s","dbus_env":"%s","bus_sock":"%s"}\n' \
  "$sessions" "$(cat "$HOME/.claude/.credentials.json")" "$sess_w" "$login_w" "$ssh" "$secret" "$dbus_env" "$bus_sock"
"#;

const FAKE_GROK_COLLECTOR: &str = r#"#!/bin/sh
sessions=$(ls -1 "$HOME/.grok/sessions" 2>/dev/null | sort | tr '\n' ',' | sed 's/,$//')
if touch "$HOME/.grok/sessions/write-test" 2>/dev/null; then sess_w=rw; else sess_w=ro; fi
if sh -c 'echo x >> "$HOME/.grok/auth.json"' 2>/dev/null; then login_w=rw; else login_w=ro; fi
printf '{"id":"grok","sessions":"%s","login":%s,"sess_w":"%s","login_w":"%s"}\n' \
  "$sessions" "$(cat "$HOME/.grok/auth.json")" "$sess_w" "$login_w"
"#;

#[test]
fn two_agents_see_merged_sessions_freshest_logins_no_isolated_and_no_session_bus() {
    let mut w = world();
    let bus_sock = w.run.join("bus");
    std::fs::write(&bus_sock, "fake-bus").unwrap();
    w.vars.insert(
        "DBUS_SESSION_BUS_ADDRESS".into(),
        format!("unix:path={}", bus_sock.display()),
    );
    std::fs::create_dir_all(w.home.join(".ssh")).unwrap();
    std::fs::write(w.home.join(".ssh/id_ed25519"), "private-key").unwrap();
    std::fs::write(w.home.join("secret.txt"), "top-secret").unwrap();

    // Agent 1 (claude): host + 2 shared sandbox homes + 1 isolated home.
    let c_host = w.home.join(".claude");
    let c_sb1 = w
        .home
        .join(".local/share/paddock/homes/claude/shared/p1/.claude");
    let c_sb2 = w
        .home
        .join(".local/share/paddock/homes/claude/shared/p2/.claude");
    let c_iso = w
        .home
        .join(".local/share/paddock/homes/claude/isolated/p3/.claude");
    for (dir, sname) in [
        (&c_host, "host.jsonl"),
        (&c_sb1, "sb1.jsonl"),
        (&c_sb2, "sb2.jsonl"),
        (&c_iso, "isolated.jsonl"),
    ] {
        std::fs::create_dir_all(dir.join("projects")).unwrap();
        std::fs::write(dir.join("projects").join(sname), "{}\n").unwrap();
    }
    // c_sb1 has the latest token expiry among non-isolated copies; c_iso has a
    // higher expiry but is isolated so it must never be shown.
    std::fs::write(
        c_host.join(".credentials.json"),
        r#"{"which":"host","claudeAiOauth":{"expiresAt":1000000000000}}"#,
    )
    .unwrap();
    std::fs::write(
        c_sb1.join(".credentials.json"),
        r#"{"which":"sb1","claudeAiOauth":{"expiresAt":3000000000000}}"#,
    )
    .unwrap();
    std::fs::write(
        c_sb2.join(".credentials.json"),
        r#"{"which":"sb2","claudeAiOauth":{"expiresAt":2000000000000}}"#,
    )
    .unwrap();
    std::fs::write(
        c_iso.join(".credentials.json"),
        r#"{"which":"isolated","claudeAiOauth":{"expiresAt":9000000000000}}"#,
    )
    .unwrap();
    set_mtime(&c_host.join(".credentials.json"), 100);
    set_mtime(&c_sb1.join(".credentials.json"), 200);
    set_mtime(&c_sb2.join(".credentials.json"), 300);

    // Agent 2 (grok): host + 2 shared sandbox homes + 1 isolated home.
    let g_host = w.home.join(".grok");
    let g_sb1 = w
        .home
        .join(".local/share/paddock/homes/grok/shared/p1/.grok");
    let g_sb2 = w
        .home
        .join(".local/share/paddock/homes/grok/shared/p2/.grok");
    let g_iso = w
        .home
        .join(".local/share/paddock/homes/grok/isolated-work/p3/.grok");
    for (dir, sname) in [
        (&g_host, "host.jsonl"),
        (&g_sb1, "sb1.jsonl"),
        (&g_sb2, "sb2.jsonl"),
        (&g_iso, "isolated.jsonl"),
    ] {
        std::fs::create_dir_all(dir.join("sessions")).unwrap();
        std::fs::write(dir.join("sessions").join(sname), "{}\n").unwrap();
    }
    // No expiry field in grok auth.json: g_sb2 has the latest mtime among
    // non-isolated copies.
    std::fs::write(g_host.join("auth.json"), r#"{"which":"host"}"#).unwrap();
    std::fs::write(g_sb1.join("auth.json"), r#"{"which":"sb1"}"#).unwrap();
    std::fs::write(g_sb2.join("auth.json"), r#"{"which":"sb2"}"#).unwrap();
    std::fs::write(g_iso.join("auth.json"), r#"{"which":"isolated"}"#).unwrap();
    set_mtime(&g_host.join("auth.json"), 100);
    set_mtime(&g_sb1.join("auth.json"), 200);
    set_mtime(&g_sb2.join("auth.json"), 400);
    set_mtime(&g_iso.join("auth.json"), 900);

    // Check the constructed view directly first.
    let mut env = w.env();
    env.runtime_dir = Some(w.run.clone());
    let v = paddock::usage_box::view(&env);
    assert_eq!(
        v.logins.get(&c_host.join(".credentials.json")),
        Some(&c_sb1.join(".credentials.json"))
    );
    assert_eq!(
        v.logins.get(&g_host.join("auth.json")),
        Some(&g_sb2.join("auth.json"))
    );
    assert!(v.mounts.contains(&Mount::Login {
        src: c_sb1.join(".credentials.json"),
        dest: c_host.join(".credentials.json"),
    }));
    assert!(v.mounts.contains(&Mount::Login {
        src: g_sb2.join("auth.json"),
        dest: g_host.join("auth.json"),
    }));
    let b_env = paddock::usage_box::box_env(&env);
    assert!(!b_env.contains_key("DBUS_SESSION_BUS_ADDRESS"));
    let args = paddock::usage_box::bwrap_args(&env, &v, Path::new("/x/u"), &[]);
    // The host runtime folder (with the session bus socket) is hidden: by
    // its own tmpfs, or by the tmpfs on /tmp when it lies under /tmp.
    assert!(args.windows(2).any(|w_arg| w_arg[0] == "--tmpfs"
        && (Path::new(&w_arg[1]) == w.run || w.run.starts_with(&w_arg[1]))));

    if !box_works(&w) {
        eprintln!("skipped: the usage box cannot start here (bwrap, user namespaces, overlayfs)");
        return;
    }

    write_exec(
        &w.home.join("omarchy/bin/omarchy-agent-usage-update"),
        TWO_AGENTS_UPDATE,
    );
    write_exec(
        &w.home.join("omarchy/bin/omarchy-agent-usage-claude"),
        &FAKE_CLAUDE_COLLECTOR.replace("$ bus_sock_path", &bus_sock.display().to_string()),
    );
    write_exec(
        &w.home.join("omarchy/bin/omarchy-agent-usage-grok"),
        FAKE_GROK_COLLECTOR,
    );

    let out = w.update(&[]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let claude = w.record("claude");
    assert_eq!(claude["sessions"], "host.jsonl,sb1.jsonl,sb2.jsonl");
    assert_eq!(claude["login"]["which"], "sb1");
    // Session folders take writes into the box's tmpfs; logins stay read-only.
    assert_eq!(claude["sess_w"], "rw");
    assert_eq!(claude["login_w"], "ro");
    assert_eq!(claude["ssh"], "absent");
    assert_eq!(claude["secret"], "absent");
    assert_eq!(claude["dbus_env"], "unset");
    assert_eq!(claude["bus_sock"], "absent");

    let grok = w.record("grok");
    assert_eq!(grok["sessions"], "host.jsonl,sb1.jsonl,sb2.jsonl");
    assert_eq!(grok["login"]["which"], "sb2");
    assert_eq!(grok["sess_w"], "rw");
    assert_eq!(grok["login_w"], "ro");
    for d in [&c_host, &c_sb1, &c_sb2] {
        assert!(!d.join("projects/write-test").exists());
    }
    for d in [&g_host, &g_sb1, &g_sb2] {
        assert!(!d.join("sessions/write-test").exists());
    }
}

#[test]
fn a_collector_that_calls_an_unlisted_host_fails_and_stderr_names_the_host() {
    let w = world();
    if !box_works(&w) {
        eprintln!("skipped: the usage box cannot start here");
        return;
    }
    write_exec(
        &w.home.join("omarchy/bin/omarchy-agent-usage-update"),
        r#"#!/bin/sh
python3 -c '
import os, socket, sys
proxy = os.environ["HTTPS_PROXY"].replace("http://", "")
host, port = proxy.split(":")
s = socket.create_connection((host, int(port)), timeout=5)
s.sendall(b"CONNECT outside-allowlist.example:443 HTTP/1.1\r\nHost: outside-allowlist.example:443\r\n\r\n")
resp = s.recv(1024).decode("utf-8", "replace")
s.close()
if "200" in resp.splitlines()[0]:
    sys.exit(0)
sys.exit(7)
'
"#,
    );
    let out = w.update(&[]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(7), "{err}");
    assert!(
        err.contains("outside-allowlist.example"),
        "stderr did not name the refused host: {err}"
    );
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
n=$(wc -l < "$HOME/.local/state/omarchy/agents/real.log" | tr -d ' ')
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
# Codex's app-server keeps a database in ~/.codex: the write works in the box.
if echo db > "$HOME/.codex/state_5.sqlite" && [ "$(cat "$HOME/.codex/state_5.sqlite")" = db ]; then
  echo codex-db-written >> "$s/seen.txt"
fi
# The login stays read-only.
echo x >> "$HOME/.codex/auth.json" || true
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
    std::fs::write(
        w.home.join(".codex/auth.json"),
        "{\"OPENAI_API_KEY\":\"k\"}",
    )
    .unwrap();
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
                "HTTPS_PROXY",
                "https_proxy",
                "HTTP_PROXY",
                "http_proxy",
                "ALL_PROXY",
                "all_proxy",
                "NO_PROXY",
                "no_proxy",
                "PWD",
                "SHLVL",
                "_",
                "OLDPWD"
            ]
            .contains(v),
            "{v} reached the box: {vars:?}"
        );
    }
    // The record landed. The writes into the harness folders worked in the
    // box, and none of them reached the host or a sandbox home.
    assert!(seen.contains("codex-db-written"), "{seen}");
    assert!(state.join("usage/fake.json").is_file());
    assert!(!w.home.join(".codex/state_5.sqlite").exists());
    assert_eq!(
        std::fs::read_to_string(w.home.join(".codex/auth.json")).unwrap(),
        "{\"OPENAI_API_KEY\":\"k\"}"
    );
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

/// A Codex login that the keeper keeps is checked as before the usage box:
/// out of the box (`--except codex`), in the keeper's sandbox, with the
/// writes in the keeper's own folder.
#[test]
fn a_kept_codex_login_is_checked_in_the_keeper_sandbox() {
    let mut w = world();
    if !box_works(&w)
        || !Command::new("python3")
            .arg("-c")
            .arg("pass")
            .status()
            .is_ok_and(|s| s.success())
    {
        eprintln!("skipped: needs bwrap, pasta and python3");
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

    let out = w.update(&[]);
    assert!(out.status.success(), "{out:?}");
    assert!(w.real_log().contains("--except codex"), "{}", w.real_log());
    let rec = w.record("codex");
    assert_eq!(rec["limits"][0]["percent"], 42, "{rec}");
    assert_eq!(rec["home"], "ro", "{rec}");
    // The refresh went back to the host login, once.
    let host: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&store).unwrap()).unwrap();
    assert_eq!(host["tokens"]["refresh_token"], "rt-2");
    // The host's own folder was never written: the overlay took the writes.
    assert!(!w.home.join(".codex/used.log").exists());
}
