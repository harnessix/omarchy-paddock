//! `paddock shims install --scope omarchy-agent` end to end, with a fake
//! `omarchy-agent`, a fake `omarchy-launch-tui` and a fake agent, in a
//! temporary home with its own XDG and mise folders. Never the user's mise
//! config, default agent or keys.
//!
//! - The launcher path and the `--inline` path start the agent through
//!   `paddock wrap`, in a sandbox.
//! - A command that the user types, and a script, start it on the host.
//! - `paddock shims remove` restores the mise config byte for byte.
//! - With the real `omarchy-agent` of this host, `--inline` and the launcher
//!   path build the same command, also for a user install in ~/.local/bin.
//!   paddock relies on that to treat both paths the same.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn bwrap_works() -> bool {
    Command::new("bwrap")
        .args(["--unshare-user", "--ro-bind", "/", "/", "true"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

const USER_CONFIG: &str =
    "# my mise settings\n[settings]\nexperimental = true # keep this\n\n[tools]\nnode = \"24\"";

/// The fake agent: one line per start, with whether it runs in a sandbox.
const AGENT: &str = "#!/bin/sh\nprintf 'inside=%s home=%s cfg=%s cred=%s args=%s\\n' \"${PADDOCK:-no}\" \"${OMARCHY_AGENT_CLAUDE_HOME-unset}\" \"${CLAUDE_CONFIG_DIR-unset}\" \"$(cat \"$HOME/.claude/.credentials.json\" 2>/dev/null)\" \"$*\" >> \"$PWD/agent.log\"\n";

/// Like omarchy-agent: one command for both paths; only the last line differs.
const FAKE_OMARCHY_AGENT: &str = r#"#!/bin/bash
inline=false
for a; do [[ $a == --inline ]] && inline=true; done
[[ $PWD == "$HOME" && -d $HOME/Work ]] && cd "$HOME/Work"
agent=claude
[[ -n ${FAKE_USER_INSTALL:-} ]] && agent=$HOME/.local/bin/claude
command=(env -u CLAUDE_CONFIG_DIR "OMARCHY_AGENT_CLAUDE_HOME=${FAKE_ACCOUNT:-}")
[[ -n ${FAKE_ACCOUNT:-} ]] && command+=("CLAUDE_CONFIG_DIR=$FAKE_ACCOUNT")
command+=("$agent" --permission-mode auto)
if [[ $inline == "true" ]]; then
  exec "${command[@]}"
else
  exec omarchy-launch-tui --app-id=org.omarchy.agent "${command[@]}"
fi
"#;

/// Records its arguments, then runs the command as a terminal would.
const FAKE_LAUNCH_TUI: &str = r#"#!/bin/bash
printf '%s\n' "$*" >> "$FAKE_LOG/launch.log"
[[ -n ${FAKE_LAUNCH_ONLY:-} ]] && exit 0
[[ $1 == --app-id=* ]] && shift
exec "$@"
"#;

struct World {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    proj: PathBuf,
    bin: PathBuf,
    tools: PathBuf,
}

fn write_exec(p: &Path, text: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn world() -> World {
    // In the home, as the other end-to-end tests: the sandbox has its own /tmp.
    let base = std::env::var("HOME").map(PathBuf::from).unwrap();
    let tmp = tempfile::tempdir_in(base).unwrap();
    let root = tmp.path().to_path_buf();
    let home = root.join("home");
    let proj = home.join("proj");
    let bin = root.join("bin");
    let tools = root.join("tools/claude/1.0/bin");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::create_dir_all(root.join("run")).unwrap();
    std::fs::create_dir_all(root.join("cfg/mise")).unwrap();
    std::fs::write(root.join("cfg/mise/config.toml"), USER_CONFIG).unwrap();
    std::fs::create_dir_all(root.join("cfg/paddock")).unwrap();
    std::fs::write(
        root.join("cfg/paddock/config.toml"),
        "backend = \"bwrap\"\n[network]\nmode = \"none\"\n",
    )
    .unwrap();
    write_exec(&bin.join("omarchy-agent"), FAKE_OMARCHY_AGENT);
    write_exec(&bin.join("omarchy-launch-tui"), FAKE_LAUNCH_TUI);
    write_exec(&tools.join("claude"), AGENT);
    write_exec(&home.join(".local/bin/claude"), AGENT);
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    std::fs::write(home.join(".claude/.credentials.json"), "main-token").unwrap();
    World {
        _tmp: tmp,
        root,
        home,
        proj,
        bin,
        tools,
    }
}

impl World {
    fn path(&self) -> String {
        format!(
            "{}:{}:/usr/local/bin:/usr/bin:/bin",
            self.bin.display(),
            self.tools.display()
        )
    }

    /// A command with the temporary home, XDG and mise folders.
    fn cmd(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut c = Command::new(program);
        for (k, _) in std::env::vars() {
            if k.starts_with("MISE_") || k.starts_with("XDG_") || k.starts_with("PADDOCK") {
                c.env_remove(&k);
            }
        }
        let r = &self.root;
        c.env("HOME", &self.home)
            .env("PATH", self.path())
            .env("XDG_CONFIG_HOME", r.join("cfg"))
            .env("XDG_DATA_HOME", r.join("data"))
            .env("XDG_STATE_HOME", r.join("state"))
            .env("XDG_CACHE_HOME", r.join("cache"))
            .env("XDG_RUNTIME_DIR", r.join("run"))
            .env("MISE_DATA_DIR", r.join("mise-data"))
            .env("MISE_CONFIG_DIR", r.join("cfg/mise"))
            .env("MISE_CACHE_DIR", r.join("mise-cache"))
            .env("MISE_STATE_DIR", r.join("mise-state"))
            .env("MISE_OFFLINE", "1")
            .env("LC_ALL", "C")
            .env("LANG", "C")
            .env("FAKE_LOG", r)
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("OMARCHY_AGENT_CLAUDE_HOME")
            .current_dir(&self.proj);
        c
    }

    fn paddock(&self, args: &[&str]) -> Output {
        self.cmd(env!("CARGO_BIN_EXE_paddock"))
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap()
    }

    fn agent_log(&self) -> Vec<String> {
        std::fs::read_to_string(self.proj.join("agent.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn launch_log(&self) -> Vec<String> {
        std::fs::read_to_string(self.root.join("launch.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }
}

fn ok(o: &Output) -> String {
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
    assert!(o.status.success(), "failed: {text}");
    text
}

#[test]
fn omarchy_agent_starts_run_in_a_sandbox_and_typed_commands_do_not() {
    if !bwrap_works() {
        eprintln!("skipped: bwrap cannot create a user namespace here");
        return;
    }
    let w = world();
    let config = w.root.join("cfg/mise/config.toml");
    let out = ok(&w.paddock(&["shims", "install", "--scope", "omarchy-agent"]));
    assert!(out.contains("omarchy-agent wrapper"), "{out}");
    let text = std::fs::read_to_string(&config).unwrap();
    assert!(text.starts_with(USER_CONFIG), "{text}");
    assert!(text.contains("[wrappers.omarchy-agent]"));
    // A second install changes nothing.
    ok(&w.paddock(&["shims", "install", "--scope", "omarchy-agent"]));
    assert_eq!(std::fs::read_to_string(&config).unwrap(), text);
    let list = ok(&w.paddock(&["shims", "list"]));
    assert!(list.contains("scope omarchy-agent: on"), "{list}");

    // The launcher path: the agent window gets `paddock wrap`, and the
    // agent runs in the sandbox with its arguments unchanged.
    let out = w.paddock(&["shims", "exec", "omarchy-agent"]);
    let text = ok(&out);
    let launch = w.launch_log();
    assert_eq!(launch.len(), 1, "{launch:?}");
    let paddock = env!("CARGO_BIN_EXE_paddock");
    assert!(
        launch[0].starts_with(&format!(
            "--app-id=org.omarchy.agent {paddock} wrap -- env -u CLAUDE_CONFIG_DIR OMARCHY_AGENT_CLAUDE_HOME= claude --permission-mode auto"
        )),
        "{launch:?}"
    );
    assert!(text.contains("paddock: claude in "), "{text}");
    assert_eq!(
        w.agent_log(),
        vec!["inside=1 home= cfg=unset cred=main-token args=--permission-mode auto"],
        "{text}"
    );

    // --inline: the same command, in the current terminal, in the sandbox.
    let text = ok(&w.paddock(&["shims", "exec", "omarchy-agent", "--inline"]));
    assert_eq!(
        w.launch_log().len(),
        1,
        "a terminal was opened for --inline"
    );
    assert!(text.contains("paddock: claude in "), "{text}");
    assert_eq!(w.agent_log().len(), 2);
    assert_eq!(
        w.agent_log()[1],
        "inside=1 home= cfg=unset cred=main-token args=--permission-mode auto"
    );

    // A user install in ~/.local/bin, which omarchy-agent runs by its path.
    let mut c = w.cmd(env!("CARGO_BIN_EXE_paddock"));
    c.args(["shims", "exec", "omarchy-agent", "--inline"])
        .env("FAKE_USER_INSTALL", "1");
    ok(&c.output().unwrap());
    assert_eq!(w.agent_log().len(), 3);
    assert!(
        w.agent_log()[2].starts_with("inside=1 "),
        "{:?}",
        w.agent_log()
    );

    // A second account: the sandbox logs in with its folder, and keeps the pin.
    let acct = w
        .home
        .join(".local/state/omarchy/agents/accounts/claude/work");
    std::fs::create_dir_all(&acct).unwrap();
    std::fs::write(acct.join(".credentials.json"), "work-token").unwrap();
    let mut c = w.cmd(env!("CARGO_BIN_EXE_paddock"));
    c.args(["shims", "exec", "omarchy-agent", "--inline"])
        .env("FAKE_ACCOUNT", &acct);
    let text = ok(&c.output().unwrap());
    assert!(text.contains("uses the account in"), "{text}");
    let line = w.agent_log().pop().unwrap();
    assert_eq!(
        line,
        format!(
            "inside=1 home={} cfg=unset cred=work-token args=--permission-mode auto",
            acct.display()
        )
    );

    // PADDOCK_BYPASS=1 skips the sandbox.
    let mut c = w.cmd(env!("CARGO_BIN_EXE_paddock"));
    c.args(["shims", "exec", "omarchy-agent", "--inline"])
        .env("PADDOCK_BYPASS", "1");
    ok(&c.output().unwrap());
    assert!(
        w.agent_log()[4].starts_with("inside=no "),
        "{:?}",
        w.agent_log()
    );

    // A command that the user types, and a script: on the host, unchanged.
    let typed = w.cmd("sh").args(["-c", "claude typed"]).output().unwrap();
    ok(&typed);
    let script = w.root.join("script.sh");
    write_exec(&script, "#!/bin/sh\nclaude from-a-script\n");
    ok(&w.cmd(&script).output().unwrap());
    let log = w.agent_log();
    assert_eq!(
        log[5],
        "inside=no home=unset cfg=unset cred=main-token args=typed"
    );
    assert_eq!(
        log[6],
        "inside=no home=unset cfg=unset cred=main-token args=from-a-script"
    );
    // Other TUIs through the shim are not wrapped.
    let shim = w
        .root
        .join("data/paddock/shims/omarchy-agent/bin/omarchy-launch-tui");
    let other = w.cmd(&shim).args(["true"]).output().unwrap();
    ok(&other);
    assert_eq!(w.launch_log().last().unwrap(), "true");

    // Removal restores the file byte for byte.
    ok(&w.paddock(&["shims", "remove"]));
    assert_eq!(std::fs::read_to_string(&config).unwrap(), USER_CONFIG);
    assert!(!w.root.join("data/paddock/shims/omarchy-agent").exists());
    let list = ok(&w.paddock(&["shims", "list"]));
    assert!(list.contains("scope omarchy-agent: off"), "{list}");
}

#[test]
fn a_user_wrapper_is_never_replaced() {
    let w = world();
    let config = w.root.join("cfg/mise/config.toml");
    let own = "[wrappers.omarchy-agent]\ncommand = \"mine\"\n";
    std::fs::write(&config, own).unwrap();
    let out = w.paddock(&["shims", "install", "--scope", "omarchy-agent"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("own [wrappers.omarchy-agent]"));
    assert_eq!(std::fs::read_to_string(&config).unwrap(), own);
}

#[test]
fn an_unknown_agent_runs_unchanged() {
    let w = world();
    write_exec(
        &w.bin.join("someagent"),
        "#!/bin/sh\nprintf 'inside=%s args=%s\\n' \"${PADDOCK:-no}\" \"$*\" >> \"$PWD/agent.log\"\nexit 7\n",
    );
    let out = w.paddock(&["wrap", "--", "env", "FOO=1", "someagent", "--yolo"]);
    assert_eq!(out.status.code(), Some(7), "the exit code passes through");
    assert!(String::from_utf8_lossy(&out.stderr).contains("someagent runs without a sandbox"));
    assert_eq!(w.agent_log(), vec!["inside=no args=--yolo"]);
}

/// The real omarchy-agent of this host, if it has one.
fn real_omarchy_agent() -> Option<PathBuf> {
    [
        "/usr/bin/omarchy-agent",
        "/usr/share/omarchy/bin/omarchy-agent",
    ]
    .iter()
    .map(PathBuf::from)
    .find(|p| p.is_file())
}

#[test]
fn the_real_omarchy_agent_builds_one_command_for_both_paths() {
    let Some(real) = real_omarchy_agent() else {
        eprintln!("skipped: no omarchy-agent on this host");
        return;
    };
    let w = world();
    // The real script, with fakes for the programs it ends in: `env` (the
    // account prefix, inline) and the launcher.
    std::fs::remove_file(w.bin.join("omarchy-agent")).unwrap();
    std::os::unix::fs::symlink(&real, w.bin.join("omarchy-agent")).unwrap();
    write_exec(
        &w.bin.join("env"),
        "#!/bin/bash\nprintf '%s\\n' \"$*\" >> \"$FAKE_LOG/inline.log\"\n",
    );
    let defaults = w.home.join(".config/omarchy/defaults");
    std::fs::create_dir_all(&defaults).unwrap();
    let read = |f: &str| std::fs::read_to_string(w.root.join(f)).unwrap_or_default();
    for (agent, user_install) in [("claude", false), ("claude", true), ("codex", false)] {
        std::fs::write(defaults.join("agent"), format!("{agent}\n")).unwrap();
        write_exec(&w.bin.join(agent), AGENT);
        let ui = w.home.join(".local/bin").join(agent);
        if user_install {
            write_exec(&ui, AGENT);
        } else {
            let _ = std::fs::remove_file(&ui);
        }
        let _ = std::fs::remove_file(w.root.join("inline.log"));
        let _ = std::fs::remove_file(w.root.join("launch.log"));
        let launch = w
            .cmd("omarchy-agent")
            .env("FAKE_LAUNCH_ONLY", "1")
            .output()
            .unwrap();
        let _ = std::fs::remove_file(w.root.join("inline.log"));
        let inline = w.cmd("omarchy-agent").arg("--inline").output().unwrap();
        assert!(inline.status.success() || !read("inline.log").is_empty());
        let launched = read("launch.log");
        let first = launched.lines().next().unwrap_or_default().to_string();
        let built = first
            .strip_prefix("--app-id=org.omarchy.agent ")
            .unwrap_or_else(|| panic!("{agent}: {first:?} {launch:?}"));
        let inline_cmd = read("inline.log");
        let inline_first = inline_cmd.lines().next().unwrap_or_default();
        assert_eq!(
            format!("env {inline_first}"),
            built,
            "{agent}: omarchy-agent builds a different command for --inline; paddock's omarchy-agent scope must follow"
        );
        if user_install {
            assert!(built.contains(&ui.display().to_string()), "{built}");
        }
    }
}

#[test]
fn the_shell_form_a_host_only_agent_and_a_read_only_install() {
    if !bwrap_works() {
        eprintln!("skipped: bwrap cannot create a user namespace here");
        return;
    }
    let w = world();
    // amp's form with a prompt: the script runs whole in the sandbox, and
    // its $0 is the agent program there.
    write_exec(
        &w.tools.join("amp"),
        "#!/bin/sh\nread -r first\nprintf 'inside=%s zero=%s args=%s stdin=%s\\n' \"${PADDOCK:-no}\" \"$0\" \"$*\" \"$first\" >> \"$PWD/agent.log\"\n",
    );
    let script = "printf \"%s\\n\" \"$1\" | exec \"$0\" --dangerously-allow-all";
    let out = w.paddock(&["wrap", "--", "bash", "-c", script, "amp", "the prompt"]);
    ok(&out);
    let line = w.agent_log().pop().unwrap();
    let amp = std::fs::canonicalize(w.tools.join("amp")).unwrap();
    assert_eq!(
        line,
        format!(
            "inside=1 zero={} args=--dangerously-allow-all stdin=the prompt",
            amp.display()
        )
    );

    // muse keeps its own sandbox, which cannot start in paddock's: it runs
    // as before, and paddock says so.
    write_exec(&w.tools.join("muse"), AGENT);
    let out = w.paddock(&["wrap", "--", "muse", "--approval-mode", "never"]);
    let text = ok(&out);
    assert!(text.contains("muse runs without a sandbox"), "{text}");
    assert!(w.agent_log().pop().unwrap().starts_with("inside=no "));

    // A self-update inside the sandbox fails: the install is read-only.
    write_exec(
        &w.tools.join("claude"),
        "#!/bin/sh\nif touch \"$(dirname \"$0\")/update\" 2>\"$PWD/update.err\"; then echo updated >> \"$PWD/agent.log\"; else echo refused >> \"$PWD/agent.log\"; fi\n",
    );
    ok(&w.paddock(&["wrap", "--", "claude"]));
    assert_eq!(w.agent_log().pop().unwrap(), "refused");
    let err = std::fs::read_to_string(w.proj.join("update.err")).unwrap();
    assert!(err.contains("Read-only file system"), "{err}");
    assert!(!w.tools.join("update").exists());
}

#[test]
fn history_reaches_the_host_and_other_projects_stay_hidden() {
    if !bwrap_works() {
        eprintln!("skipped: bwrap cannot create a user namespace here");
        return;
    }
    let w = world();
    let projects = w.home.join(".claude/projects");
    let other = projects.join("-home-demo-other-project");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join("secret-session.jsonl"), "{\"other\":1}\n").unwrap();
    // Claude Code: it lists what it sees, and writes a transcript.
    write_exec(
        &w.tools.join("claude"),
        "#!/bin/sh\nls -A \"$HOME/.claude/projects\" > \"$PWD/seen.txt\"\nfor d in \"$HOME\"/.claude/projects/*/; do ls -A \"$d\" >> \"$PWD/seen.txt\"; done\nkey=$(printf '%s' \"$PWD\" | sed 's/[^A-Za-z0-9]/-/g')\nprintf '{\"t\":1}\\n' > \"$HOME/.claude/projects/$key/s1.jsonl\"\n",
    );
    ok(&w.paddock(&["wrap", "--", "claude"]));
    let proj = std::fs::canonicalize(&w.proj).unwrap();
    let key: String = proj
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let seen = std::fs::read_to_string(w.proj.join("seen.txt")).unwrap();
    assert_eq!(seen.trim(), key, "the sandbox saw other projects: {seen}");
    assert_eq!(
        std::fs::read_to_string(projects.join(&key).join("s1.jsonl")).unwrap(),
        "{\"t\":1}\n"
    );

    // Codex: a session by date is copied back at exit.
    write_exec(
        &w.tools.join("codex"),
        "#!/bin/sh\nmkdir -p \"$HOME/.codex/sessions/2026/10/30\"\nprintf '{\"r\":1}\\n' > \"$HOME/.codex/sessions/2026/10/30/rollout-1.jsonl\"\nls -A \"$HOME/.codex/sessions\" > \"$PWD/codex-seen.txt\"\n",
    );
    std::fs::create_dir_all(w.home.join(".codex/sessions/2026/10/29")).unwrap();
    std::fs::write(
        w.home.join(".codex/sessions/2026/10/29/rollout-0.jsonl"),
        "old\n",
    )
    .unwrap();
    ok(&w.paddock(&["wrap", "--", "codex", "--approve-for-me"]));
    assert_eq!(
        std::fs::read_to_string(w.home.join(".codex/sessions/2026/10/30/rollout-1.jsonl")).unwrap(),
        "{\"r\":1}\n"
    );
    // The host's own sessions are not in the sandbox.
    let seen = std::fs::read_to_string(w.proj.join("codex-seen.txt")).unwrap();
    assert_eq!(seen.trim(), "2026");
    assert!(!w
        .root
        .join("data/paddock/homes/codex")
        .read_dir()
        .unwrap()
        .flatten()
        .any(|l| l
            .path()
            .read_dir()
            .unwrap()
            .flatten()
            .any(|h| h.path().join(".codex/sessions/2026/10/29").exists())));
}

#[test]
fn a_normal_run_keeps_its_history_in_the_sandbox() {
    if !bwrap_works() {
        eprintln!("skipped: bwrap cannot create a user namespace here");
        return;
    }
    let w = world();
    // Only `paddock wrap` starts share history with the host. A normal run
    // keeps it in the sandbox home, so a sandbox-written transcript never
    // reaches `claude --resume` or `codex resume` on the host.
    write_exec(
        &w.tools.join("claude"),
        "#!/bin/sh\nmkdir -p \"$HOME/.claude/projects/p\"\nprintf '{\"t\":1}\\n' > \"$HOME/.claude/projects/p/s.jsonl\"\n",
    );
    ok(&w.paddock(&["run", "--no-yolo", "claude"]));
    assert!(!w.home.join(".claude/projects/p").exists());
    write_exec(
        &w.tools.join("codex"),
        "#!/bin/sh\nmkdir -p \"$HOME/.codex/sessions/2026/10/31\"\nprintf 'x\\n' > \"$HOME/.codex/sessions/2026/10/31/rollout-9.jsonl\"\n",
    );
    ok(&w.paddock(&["run", "--no-yolo", "codex"]));
    assert!(!w.home.join(".codex/sessions/2026/10/31").exists());
}

#[test]
fn a_start_in_the_home_folder_works_in_a_scratch_project() {
    if !bwrap_works() {
        eprintln!("skipped: bwrap cannot create a user namespace here");
        return;
    }
    let w = world();
    let out = w
        .cmd(env!("CARGO_BIN_EXE_paddock"))
        .args(["wrap", "--", "claude", "--permission-mode", "auto"])
        .current_dir(&w.home)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let text = ok(&out);
    let scratch = w.root.join("data/paddock/scratch/claude");
    assert!(text.contains("scratch project"), "{text}");
    let log = std::fs::read_to_string(scratch.join("agent.log")).unwrap();
    assert!(log.starts_with("inside=1 "), "{log}");
    assert!(!w.home.join("agent.log").exists());
    let files = ok(&w.paddock(&["files"]));
    assert!(files.contains("scratch"), "{files}");
    assert!(files.contains(&scratch.display().to_string()), "{files}");
    let gc = ok(&w.paddock(&["gc", "--scratch", "--older-than", "0"]));
    assert!(gc.contains("scratch project claude"), "{gc}");
}
