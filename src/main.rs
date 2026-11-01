use anyhow::{bail, Result};
use clap::{Args, Parser, Subcommand};
use paddock::build::{self, Options};
use paddock::doctor;
use paddock::env::HostEnv;
use paddock::manifest::{self, Manifest};
use paddock::profile::HarnessFactory;
use paddock::run;
use paddock::spec::{GuiMode, MountKind, Network};
use paddock::state::{self, Running};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "paddock",
    version,
    about = "Run a coding-agent harness with full auto-approval inside an isolated sandbox",
    long_about = None,
    after_help = "Examples:\n  paddock claude              start Claude Code in the current project\n  paddock codex ~/src/app     start Codex in another project\n  paddock explain claude      show every mount, variable and rule, and why\n  paddock doctor --escape     try to escape the sandbox and report the result"
)]
struct Cli {
    /// Show more log output: -v info, -vv debug, -vvv trace. PADDOCK_LOG=debug
    /// or PADDOCK_LOG=broker=debug sets levels too.
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,
    /// Show only errors.
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    quiet: u8,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start a harness in a sandbox (the same as `paddock <harness>`).
    Run(RunArgs),
    /// Print the sandbox that `run` would start, with the origin of each entry.
    Explain {
        #[command(flatten)]
        run: RunArgs,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Open a shell in the sandbox that a harness would get.
    Shell(RunArgs),
    /// List the known harnesses and where they are installed.
    List {
        #[arg(long)]
        json: bool,
    },
    /// List the projects where sandboxes started recently.
    Recent {
        #[arg(long)]
        json: bool,
    },
    /// Focus the terminal window that runs a sandbox (Hyprland).
    /// Choose a recent project in the terminal, then start a harness there.
    Pick { harness: String },
    /// Create or remove launcher entries for every installed harness and app,
    /// in a "Paddock" folder of the menu.
    /// Make `claude`, `codex` and the other harness commands start in a sandbox in your shell
    Shims {
        #[command(subcommand)]
        cmd: ShimsCmd,
    },
    /// Run an agent command as it is, in a sandbox: same arguments, folder,
    /// account and exit code (transparent mode). paddock adds no
    /// auto-approval flags. Commands without a profile run without a sandbox.
    #[command(after_help = WRAP_HELP)]
    Wrap {
        /// The command, after `--`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        command: Vec<String>,
    },
    Launchers {
        #[command(subcommand)]
        cmd: LaunchersCmd,
    },
    /// List the host windows of app sandboxes; --mark gives them paddock's border (Hyprland).
    Windows {
        #[arg(long)]
        json: bool,
        #[arg(long)]
        mark: bool,
    },
    /// Focus the terminal window, or the display window with --display, of a running sandbox (Hyprland).
    Focus {
        id: String,
        /// Focus the nested display window of a sandbox in window mode instead.
        #[arg(long)]
        display: bool,
    },
    /// Show the MCP servers of a harness and whether each one runs in the sandbox.
    Mcp {
        harness: String,
        path: Option<PathBuf>,
    },
    /// List running sandboxes.
    Ps {
        #[arg(long)]
        json: bool,
    },
    /// Stop a running sandbox.
    Stop { id: String },
    /// Freeze a running sandbox: it uses no CPU and keeps its memory until
    /// you thaw it.
    Freeze {
        id: String,
        /// Also the sandboxes that it started.
        #[arg(long)]
        tree: bool,
        /// Why, for the audit log and for the other agents in paddock kv.
        #[arg(long, default_value = "")]
        reason: String,
    },
    /// Thaw a frozen sandbox: it continues where it stopped.
    Thaw {
        id: String,
        /// Also the sandboxes that it started.
        #[arg(long)]
        tree: bool,
    },
    /// Show the disk space of sandbox homes, images, worktrees and records.
    #[command(after_help = DU_HELP)]
    Du {
        #[arg(long)]
        json: bool,
    },
    /// Remove paddock data that is no longer needed. A dry run unless --yes.
    #[command(after_help = GC_HELP)]
    Gc {
        /// Remove the listed items. Without it, paddock only prints them.
        #[arg(long, short = 'y')]
        yes: bool,
        /// Also homes whose project is gone or that were not used for --older-than days.
        #[arg(long)]
        homes: bool,
        /// With --homes: the age in days of an unused home.
        #[arg(long, value_name = "DAYS", default_value_t = paddock::storage::DEFAULT_OLDER_THAN)]
        older_than: u64,
        /// With --homes: also desktop app homes. Their login is lost.
        #[arg(long)]
        include_app_logins: bool,
        /// Also paddock worktrees.
        #[arg(long)]
        worktrees: bool,
        /// Also scratch projects (starts in the home folder through `paddock
        /// wrap`) that were not used for --older-than days. Their files are lost.
        #[arg(long)]
        scratch: bool,
        /// With --worktrees: also worktrees with unmerged commits or uncommitted changes.
        #[arg(long)]
        force: bool,
        #[arg(long)]
        json: bool,
    },
    /// List the files a sandbox saved outside the project, and its project changes.
    #[command(after_help = FILES_HELP)]
    Files {
        /// A sandbox id from `paddock ps` or an earlier run. Without it, every sandbox home with files.
        id: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Copy files from a sandbox home to the host, then remove the sandbox copy.
    Recover {
        id: String,
        /// Paths as `paddock files` shows them, such as Desktop/report.pdf.
        #[arg(required_unless_present = "all")]
        paths: Vec<String>,
        /// Recover every listed file.
        #[arg(long, conflicts_with = "paths")]
        all: bool,
        /// Put the files in this directory instead of the same path under your home.
        #[arg(long)]
        to: Option<PathBuf>,
        /// Leave the sandbox copy in place.
        #[arg(long)]
        keep: bool,
    },
    /// Delete files from the recovery folders of a sandbox home.
    Discard {
        id: String,
        #[arg(required_unless_present = "all")]
        paths: Vec<String>,
        #[arg(long, conflicts_with = "paths")]
        all: bool,
    },
    /// Check dependencies, or try to escape the sandbox with --escape.
    Doctor {
        #[arg(long)]
        escape: bool,
        #[arg(long, default_value = "auto")]
        backend: String,
        #[arg(long, default_value = "none")]
        network: Network,
        /// Also test the headless display and screenshots.
        #[arg(long)]
        gui: bool,
        /// Test the nested compositor window instead of the headless display.
        #[arg(long)]
        window: bool,
        /// Test app mode: the restricted Wayland socket and the session bus.
        #[arg(long)]
        app: bool,
        /// With --app: the session bus to test, private (the default) or
        /// host-filtered.
        #[arg(long, requires = "app", value_parser = ["private", "host-filtered"])]
        bus: Option<String>,
        /// inherit (the default) or clean.
        #[arg(long, default_value = "inherit", value_parser = ["inherit", "clean"])]
        env: String,
        /// Also check that DNS and HTTPS work from inside.
        #[arg(long)]
        online: bool,
        /// Print a diagnostic report for bug reports: version, doctor output
        /// and recent paddock warnings from the journal. Names, paths,
        /// variable values and tokens are removed.
        #[arg(long, conflicts_with = "escape")]
        report: bool,
        /// With --report: write the report to this file instead of stdout.
        #[arg(long, requires = "report")]
        output: Option<PathBuf>,
    },
    /// First-run setup: packages, shell commands, launchers, the bar widget,
    /// the default backend and app folders. Each step says what it changes
    /// and how to undo it. A second run changes nothing unless asked.
    Setup {
        /// Ask nothing: only the flags below change something.
        #[arg(long, short = 'y')]
        yes: bool,
        /// Make the sandbox the default for harness commands in bash and zsh.
        #[arg(long, conflicts_with = "no_shims")]
        shims: bool,
        #[arg(long)]
        no_shims: bool,
        /// Add the Paddock launcher folder to the app menu.
        #[arg(long, conflicts_with = "no_launchers")]
        launchers: bool,
        #[arg(long)]
        no_launchers: bool,
        /// Add the Omarchy bar widget.
        #[arg(long, conflicts_with = "no_plugin")]
        plugin: bool,
        #[arg(long)]
        no_plugin: bool,
        /// The default backend: auto, bwrap, podman, docker or srt.
        #[arg(long)]
        backend: Option<String>,
        /// Save a workspace folder for an app, as NAME=DIR. Repeat it for
        /// more folders.
        #[arg(long = "app", value_name = "NAME=DIR")]
        apps: Vec<String>,
    },
    /// Update paddock to the latest GitHub release, and the bar widget.
    Update {
        /// Only show whether a newer release exists, and what an update does.
        #[arg(long)]
        check: bool,
        /// Update without asking.
        #[arg(long, short = 'y', conflicts_with = "check")]
        yes: bool,
    },
    /// Show or change which host environment variables a sandbox inherits.
    Env {
        #[command(subcommand)]
        cmd: EnvCmd,
    },
    /// Manage the credentials that sandboxes use.
    Auth {
        #[command(subcommand)]
        cmd: AuthCmd,
    },
    /// Approve the access that a project's paddock.toml asks for.
    Approve { path: Option<PathBuf> },
    /// Write a commented paddock.toml into a project.
    Init { path: Option<PathBuf> },
    /// Inside a sandbox: ask the host to do something (see `paddock-host --help`).
    #[command(hide = true, disable_help_flag = true)]
    Host {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// List or answer requests that sandboxes sent to the host.
    #[command(after_help = APPROVALS_HELP)]
    Approvals {
        /// Print JSON (api_version paddock.approvals/v1).
        #[arg(long)]
        json: bool,
        #[command(subcommand)]
        cmd: Option<ApprovalsCmd>,
    },
    /// Read what agents wrote to paddock kv, and free their leases.
    Kv {
        #[command(subcommand)]
        cmd: KvCmd,
    },
    /// Show the audit log of host requests.
    Log {
        /// Print one JSON object per line.
        #[arg(long)]
        json: bool,
        /// How many recent entries to show.
        #[arg(short = 'n', long, default_value_t = 20)]
        lines: usize,
    },
    /// Run desktop agent apps (ChatGPT/Codex, Claude, VS Code, flatpak apps) in a sandbox.
    #[command(after_help = APP_HELP)]
    App {
        #[command(subcommand)]
        cmd: AppCmd,
    },
    /// Configure an agent that answers host requests in your place.
    #[command(after_help = APPROVER_HELP)]
    Approver {
        #[command(subcommand)]
        cmd: ApproverCmd,
    },
    /// Print the paddock-host skill for agents, or install it.
    Skill {
        #[command(subcommand)]
        cmd: Option<SkillCmd>,
    },
    /// Inside an app sandbox: start the app with links that the host hands over.
    #[command(hide = true)]
    Inbound,
    /// Inside an app sandbox: the link and notification services on its private bus.
    #[command(hide = true)]
    Portal,
    /// On the host: run one child sandbox that another sandbox asked for
    /// (`paddock-host spawn`). The parent's paddock starts this.
    #[command(hide = true)]
    SpawnChild { job: PathBuf },
    /// Update the launcher entries, the Omarchy menu and the shell shims to
    /// the harnesses installed now. Only the parts that are turned on.
    Refresh {
        /// Print nothing, and only add entries: the run that a harness start
        /// begins.
        #[arg(long)]
        background: bool,
    },
    /// The executor of approved host requests: let systemd start it on the
    /// first request (socket activation), or turn that off again.
    Gateway {
        #[command(subcommand)]
        cmd: GatewayCmd,
    },
    /// The executor of approved host requests (started by paddock or by
    /// systemd socket activation).
    #[command(hide = true)]
    Gatewayd {
        /// Exit after this many seconds without a connection.
        #[arg(long, default_value_t = 120)]
        idle: u64,
    },
    /// The Omarchy agents panel's `omarchy-agent-usage-update`, through
    /// paddock: a login that a sandbox holds is checked from paddock's side.
    #[command(hide = true)]
    OmarchyUsageUpdate {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Inside a container sandbox: pass connections to the host proxy socket.
    #[command(hide = true)]
    Relay {
        #[arg(long)]
        listen: String,
        #[arg(long)]
        socket: PathBuf,
        #[arg(long)]
        forward: Vec<u16>,
        #[arg(long)]
        ready_file: Option<PathBuf>,
    },
    #[command(external_subcommand)]
    External(Vec<String>),
}

#[derive(Subcommand)]
enum GatewayCmd {
    /// Write the systemd user units and turn the socket on.
    Install,
    /// Turn the socket off and remove the units that paddock wrote.
    Remove,
    /// How the executor starts.
    Status,
}

#[derive(Subcommand)]
enum ShimsCmd {
    /// Write the shell functions, and source them from ~/.bashrc and ~/.zshrc.
    /// With --scope omarchy-agent: only the agents that Omarchy's
    /// omarchy-agent starts (its key, menu, agents panel and `a` alias).
    Install {
        /// shell (the default): typed commands in bash and zsh.
        /// omarchy-agent: only the starts from omarchy-agent.
        #[arg(long, default_value = "shell", value_parser = ["shell", "omarchy-agent"])]
        scope: String,
    },
    /// Remove the functions, the lines that `install` added, and the
    /// omarchy-agent scope.
    Remove,
    /// Show the commands that `install` would cover, and whether they are on.
    List,
    /// Run a program that a scope takes over (from paddock's mise wrapper).
    #[command(hide = true)]
    Exec {
        program: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// The omarchy-launch-tui shim of the omarchy-agent scope.
    #[command(hide = true)]
    LaunchTui {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

#[derive(Subcommand)]
enum LaunchersCmd {
    /// Write the entries, the icon and the Paddock folder.
    Install,
    /// Remove everything that `install` wrote.
    Remove,
    /// Show what `install` would create.
    List,
}

#[derive(Subcommand)]
enum EnvCmd {
    /// List host variables by what a sandbox does with them. Names only.
    Show { path: Option<PathBuf> },
    /// Stop passing these variables (saved in the user configuration).
    Hide { names: Vec<String> },
    /// Pass these variables again.
    Unhide { names: Vec<String> },
}

/// Which scope a `paddock kv` command reads. Without a flag: the pastures
/// of the project of the current folder.
#[derive(Args)]
struct KvScope {
    /// The project folder (default: the current folder).
    #[arg(long, conflicts_with_all = ["user", "sandbox", "tree"])]
    project: Option<PathBuf>,
    /// A pasture of the project: a name from [pastures] of the user
    /// configuration, or solo-<sandbox id> for a sandbox started without one.
    #[arg(long, conflicts_with_all = ["user", "sandbox", "tree", "legacy"])]
    pasture: Option<String>,
    /// The project scope that all sandboxes of the project shared in older
    /// paddock versions. Sandboxes no longer read it.
    #[arg(long, conflicts_with_all = ["user", "sandbox", "tree"])]
    legacy: bool,
    /// The scope that all projects share.
    #[arg(long, conflicts_with_all = ["sandbox", "tree"])]
    user: bool,
    /// The self scope of one sandbox, by id.
    #[arg(long, conflicts_with = "tree")]
    sandbox: Option<String>,
    /// The tree scope of a spawn tree, by the id of its root sandbox.
    #[arg(long)]
    tree: Option<String>,
}

impl KvScope {
    fn target(&self) -> Result<paddock::kv::host::Target> {
        use paddock::kv::host::{project_of, Target};
        use paddock::kv::shard::Ns;
        Ok(if self.user {
            Target::One(Ns::User)
        } else if let Some(id) = &self.sandbox {
            Target::One(Ns::Own(id.clone()))
        } else if let Some(root) = &self.tree {
            Target::One(Ns::Tree(root.clone()))
        } else {
            let project = project_of(self.project.as_deref())?;
            if let Some(name) = &self.pasture {
                Target::One(Ns::pasture(&project, name)?)
            } else if self.legacy {
                Target::One(Ns::Legacy(project))
            } else {
                Target::Project(project)
            }
        })
    }
}

#[derive(Subcommand)]
enum KvCmd {
    /// List keys, with the sandbox that wrote each one.
    List {
        prefix: Option<String>,
        #[command(flatten)]
        scope: KvScope,
        #[arg(long)]
        json: bool,
    },
    /// Print one value.
    Get {
        key: String,
        #[command(flatten)]
        scope: KvScope,
        #[arg(long)]
        json: bool,
    },
    /// Delete one key.
    Delete {
        key: String,
        #[command(flatten)]
        scope: KvScope,
    },
    /// Free the lease lease:NAME, whatever sandbox holds it.
    Release {
        name: String,
        #[command(flatten)]
        scope: KvScope,
    },
}

#[derive(Subcommand)]
enum ApprovalsCmd {
    /// Answer one request: once, session (same request again in this sandbox), or deny.
    /// Answer a question from /dev/paddock/ask with --text TEXT, or dismiss it with deny.
    Answer {
        id: String,
        #[arg(required_unless_present = "text")]
        answer: Option<String>,
        /// The answer to a question (/dev/paddock/ask). It goes to the agent.
        /// `-` reads it from standard input, so it never shows in the process
        /// list or the shell history; prefer that for anything private.
        #[arg(long, conflicts_with = "answer", allow_hyphen_values = true)]
        text: Option<String>,
    },
}

#[derive(Subcommand)]
enum AppCmd {
    /// Start an app. Its windows appear on your desktop; it sees only the workspace folders.
    Run {
        name: String,
        /// Workspace folders, read-write. Default: [apps.NAME] roots in the user configuration.
        roots: Vec<PathBuf>,
        /// Bind GPU render nodes (faster rendering).
        #[arg(long)]
        gpu: bool,
        /// none, egress (default) or allowlist.
        #[arg(long, short)]
        network: Option<Network>,
        /// inherit (default) or clean.
        #[arg(long, value_parser = ["inherit", "clean"])]
        env: Option<String>,
        /// shared (default), isolated or isolated:NAME. Most desktop apps keep
        /// their login in the host keyring, which a sandbox cannot use, so they
        /// log in again inside the sandbox in any mode.
        #[arg(long)]
        auth: Option<String>,
        /// bwrap (default), docker, podman or srt. srt needs --network allowlist or none.
        #[arg(long, short)]
        backend: Option<String>,
        /// Open the first workspace folder in a new git worktree on its own
        /// branch. Default: [apps.NAME] worktree in the user configuration.
        #[arg(long)]
        worktree: bool,
    },
    /// Add a launcher entry (Super+Space) that starts the app in a sandbox.
    Install {
        name: String,
        /// Workspace folders to save in the user configuration.
        roots: Vec<PathBuf>,
        /// Do not make paddock the handler of the app's links.
        #[arg(long)]
        no_links: bool,
    },
    /// Remove the launcher entry, and give the app's links back to their old handler.
    Uninstall { name: String },
    /// Give a link (for example claude://...) to the running app sandbox, or start the app with it.
    Open { name: String, link: String },
    /// List the app profiles, whether each is installed, and its folders.
    List,
}

const APP_HELP: &str = "App mode runs a desktop agent app in a sandbox. Its windows are normal
windows on your desktop, but through a restricted Wayland socket: the app
cannot capture the screen, read the clipboard in the background, type into
other windows, or list them. Its session bus is private, inside the sandbox,
with a private keyring for its logins. Links that it opens go to your browser
after your approval, and its notifications appear on your desktop. With
`[apps.NAME] bus = \"host-filtered\"` in ~/.config/paddock/config.toml, its
bus is the host bus, filtered to the desktop portals, notifications and the
tray, without a keyring. It sees only its workspace folders, with the same git hook, secrets and network
rules as `paddock <harness>`. The host passthrough (paddock-host) works too.
Flatpak apps differ in some of this; see below.

Examples:
  paddock app run chatgpt-desktop ~/src/api ~/src/web
  paddock app install claude-desktop ~/src/api
  paddock app list

Electron apps run with --no-sandbox: their own sandbox needs nested user
namespaces, which paddock blocks, so paddock is the boundary. Flatpak apps
(profiles with [app] flatpak = \"ID\") run with flatpak run --sandbox inside
the same network isolation (egress or none), with flatpak's own Wayland
socket and filtered host bus, without the host passthrough or worktrees.
Their folders get the same path policy, read-only git config, hooks and
pointer files, and host environment filter as other apps. flatpak can hide
a folder but not a single file, so paddock refuses to start a flatpak app
when the secrets detector finds a file such as .env in its folders, and
names the files. `[apps.NAME] unprotected_workspaces = true` in
~/.config/paddock/config.toml starts it anyway, with a warning on every
start. To add folders later, change [apps.NAME] roots and start the app
again.";

#[derive(Subcommand)]
enum ApproverCmd {
    /// Show the configured approver and whether it is trusted.
    Show,
    /// Turn the approver on. Without --system it is protected by a password.
    Enable {
        /// Capabilities it may answer, comma-separated, for example screenshot,open-url.
        #[arg(long, value_delimiter = ',', required = true)]
        capabilities: Vec<String>,
        /// Seconds before it counts as abstaining.
        #[arg(long, default_value_t = 60)]
        timeout: u64,
        /// Write the root-owned system file (run with sudo).
        #[arg(long)]
        system: bool,
        /// With --system: the user it applies to (default: the user who ran sudo).
        #[arg(long)]
        user: Option<String>,
        /// The command, with an absolute path, after --.
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// Turn the approver off.
    Disable {
        #[arg(long)]
        system: bool,
        #[arg(long)]
        user: Option<String>,
    },
    /// Send a sample request to the approver and print its answer.
    Test {
        /// The capability of the sample request.
        #[arg(long, default_value = "screenshot")]
        capability: String,
    },
}

const APPROVER_HELP: &str =
    "The agent approver is a command on the host, usually a reviewing agent,
that answers host requests (paddock-host) in your place. It reads one request
as JSON on stdin:
  {\"api_version\":\"paddock.approval-request/v1\",\"request\":{id, harness, project,
   capability, summary, detail, reason, session_allowed, ...}}
and prints one JSON line on stdout:
  {\"answer\":\"once|session|deny|abstain\",\"note\":\"why\"}
abstain, a crash, other output or a timeout pass the request to you. The fixed
rules still hold: exec is never allowed in advance, session covers only the
identical command, and protected paths are refused before anybody is asked.

Turning it on needs one of:
  sudo paddock approver enable --system --capabilities screenshot,open-url -- /usr/local/bin/review
      writes /etc/paddock/approver.d/USER.toml, owned by root. Processes of your
      own user cannot change it.
  paddock approver enable --capabilities screenshot,open-url -- /usr/local/bin/review
      signs ~/.config/paddock/approver.toml with an application password. paddock
      asks for the password when a sandbox starts; Enter skips the approver for
      that run. A changed file or a wrong password turns it off.

Every answer of the approver is in the audit log (paddock log) and in
~/.local/state/paddock/approver-decisions.jsonl with its note.";

#[derive(Subcommand)]
enum SkillCmd {
    /// Copy the skill into ~/.claude/skills and ~/.agents/skills on the host.
    Install,
}

const APPROVALS_HELP: &str =
    "A sandbox asks the host for help with paddock-host (screenshots, one host
command, one host file, the clipboard, a URL). Requests in \"ask\" mode wait
here, in the bar panel and in a desktop notification, until you answer one of
them. The first answer wins.

Modes come only from [host] in ~/.config/paddock/config.toml:
  [host]
  screenshot = \"ask\"       # deny | ask | allow
  exec = \"ask\"             # deny | ask (allow is read as ask)
  fetch = \"ask\"
  clipboard-read = \"ask\"
  clipboard-write = \"ask\"
  open-url = \"ask\"
  notify = \"allow\"
  wait = 120                 # seconds before a request times out

Questions from /dev/paddock/ask ([devices] ask = true) wait here too. Answer
one with --text: the text goes to the agent, and nothing runs on the host.
--text - reads the answer from standard input, so it never shows in the
process list or the shell history. deny dismisses it.

Examples:
  paddock approvals
  paddock approvals answer r-1793321234-1a2b3c4d once
  paddock approvals answer r-1793321234-9f8e7d6c --text -
  paddock approvals --json";

#[derive(Subcommand)]
enum AuthCmd {
    /// Copy a harness's credential files from your home directory into an isolated store.
    Import {
        harness: String,
        /// isolated or isolated:NAME.
        #[arg(long, default_value = "isolated")]
        auth: String,
    },
}

#[derive(Args, Clone)]
struct RunArgs {
    /// Harness name, for example claude or codex. Defaults to default_harness in paddock.toml.
    harness: Option<String>,
    /// Project directory. Defaults to the current directory, widened to its git root.
    path: Option<PathBuf>,
    /// auto, bwrap, docker or podman.
    #[arg(long, short)]
    backend: Option<String>,
    /// none, egress or host.
    #[arg(long, short)]
    network: Option<Network>,
    /// off, headless, window or host.
    #[arg(long)]
    gui: Option<GuiMode>,
    /// Bind GPU render nodes.
    #[arg(long)]
    gpu: bool,
    /// inherit (default) passes the host environment; clean passes only the allowlist.
    #[arg(long, value_parser = ["inherit", "clean"])]
    env: Option<String>,
    /// shared (default) reuses your login; isolated or isolated:NAME uses a separate one.
    #[arg(long)]
    auth: Option<String>,
    /// A domain the sandbox can reach in network mode allowlist (repeatable).
    #[arg(long)]
    allow: Vec<String>,
    /// Work in a new git worktree on its own branch, and review the diff afterwards.
    #[arg(long)]
    worktree: bool,
    /// Join this pasture of the project: [pastures.NAME] of the user
    /// configuration. Its members share the pasture scope of paddock kv.
    /// Without it, the sandbox shares paddock kv with no other.
    #[arg(long, value_name = "NAME")]
    pasture: Option<String>,
    /// Start the harness without its auto-approval flags.
    #[arg(long)]
    no_yolo: bool,
    /// Approve the project's paddock.toml without asking.
    #[arg(long, short = 'y')]
    yes: bool,
    /// Arguments for the harness, after `--`.
    #[arg(last = true)]
    args: Vec<String>,
}

impl RunArgs {
    fn options(&self, env: &HostEnv, shell: bool) -> Result<Options> {
        let mut harness = self.harness.clone();
        let mut path = self.path.clone();
        // `paddock ./dir` with a default harness in the manifest.
        if let Some(h) = &harness {
            if path.is_none() && (h.contains('/') || h == ".") {
                path = Some(PathBuf::from(h));
                harness = None;
            }
        }
        let harness = match harness {
            Some(h) => h,
            None => {
                let project = build::resolve_project(path.as_deref())?;
                let loaded = Manifest::load(env, &project)?;
                match loaded.merged.default_harness {
                    Some(h) => h,
                    None => bail!("name a harness, for example `paddock claude`, or set default_harness in paddock.toml"),
                }
            }
        };
        Ok(Options {
            harness,
            project: path,
            backend: self.backend.clone(),
            network: self.network,
            gui: self.gui,
            gpu: self.gpu,
            no_yolo: self.no_yolo,
            worktree: self.worktree,
            worktree_app: None,
            allow_domains: self.allow.clone(),
            app_bus: None,
            inherit_env: self.env.as_deref().map(|e| e == "inherit"),
            auth: self.auth.clone(),
            shell,
            assume_yes: self.yes,
            args: self.args.clone(),
            child: None,
            transparent: None,
            pasture: self.pasture.clone(),
        })
    }
}

fn start(env: &HostEnv, a: &RunArgs, shell: bool) -> Result<i32> {
    let opts = a.options(env, shell)?;
    let built = build::build(env, &opts)?;
    if !shell && !opts.no_yolo {
        run::probe_flags(env, &built)?;
    }
    let backend = run::select_backend(env, &built)?;
    let wt = built.worktree.clone();
    let code = run::run(env, built, backend.as_ref(), false, false)?.code;
    if let Some(w) = wt {
        eprintln!("\n{}", build::worktree_report(env, &w));
    }
    Ok(code)
}

fn explain(env: &HostEnv, a: &RunArgs, json: bool) -> Result<()> {
    let opts = a.options(env, false)?;
    let built = build::build(env, &opts)?;
    let backend = run::select_backend(env, &built);
    let s = &built.spec;
    let cleanup = || {
        let _ = std::fs::remove_dir_all(env.paddock_run().join(&s.id));
    };
    if json {
        let mut v = serde_json::to_value(s)?;
        if let Some(obj) = v.get_mut("env").and_then(|e| e.as_object_mut()) {
            for (_, var) in obj.iter_mut() {
                if var["secret"] == true {
                    var["value"] = "(hidden)".into();
                }
            }
        }
        v["backend"] = match &backend {
            Ok(b) => b.name().into(),
            Err(e) => format!("unavailable: {e}").into(),
        };
        println!("{}", serde_json::to_string_pretty(&v)?);
        cleanup();
        return Ok(());
    }
    println!("harness   {}  ({})", s.harness, built.harness_exe.display());
    println!("project   {}", s.project.display());
    println!(
        "home      {}  (from {})",
        s.home.display(),
        s.home_src.display()
    );
    match &backend {
        Ok(b) => println!("backend   {}", b.name()),
        Err(e) => println!("backend   unavailable: {e}"),
    }
    println!(
        "network   {:?}{}",
        s.network,
        if s.publish.is_empty() {
            String::new()
        } else {
            format!(", publish {:?}", s.publish)
        }
    );
    println!(
        "display   {:?}  ({}){}",
        s.gui.mode,
        s.gui.origin,
        if s.gui.gpu { ", gpu" } else { "" }
    );
    println!("\nmounts (everything else on the host is absent):");
    for m in s.ordered_mounts() {
        let kind = match m.kind {
            MountKind::Ro => "ro    ",
            MountKind::Rw => "rw    ",
            MountKind::Tmpfs => "tmpfs ",
            MountKind::MaskFile | MountKind::MaskDir => "hidden",
        };
        let src = match &m.src {
            Some(src) if *src != m.dst => format!("  <- {}", src.display()),
            _ => String::new(),
        };
        println!("  {kind} {}{src}\n         {}", m.dst.display(), m.origin);
    }
    for t in &s.host_tools {
        println!(
            "  tool   {}/{}  <- {}\n         {}",
            paddock::spec::TOOL_DIR,
            t.name,
            t.src.display(),
            t.origin
        );
    }
    println!("\nenvironment (besides HOME, PATH, TERM and locale):");
    for (k, v) in &s.env {
        let val = if v.secret {
            "(hidden)".to_string()
        } else {
            v.value.clone()
        };
        println!("  {k}={val}\n         {}", v.origin);
    }
    if !s.home_settings.is_empty() {
        println!("\nsettings written into the sandbox home (the host files stay as they are):");
        for h in &s.home_settings {
            println!(
                "  ~/{}  {} = {}\n         {}",
                h.file.display(),
                h.path.join("."),
                h.value,
                h.origin
            );
        }
    }
    if !s.setup.is_empty() {
        println!("\nsetup inside the sandbox:");
        for l in &s.setup {
            println!("  {}", l.lines().next().unwrap_or_default());
        }
    }
    if !s.mise_shared.is_empty() {
        println!(
            "\nshared install step (no project): mise install {}",
            s.mise_shared.join(" ")
        );
    }
    if !s.provision.is_empty() {
        println!("\nprovision step inside the sandbox:");
        for l in &s.provision {
            println!("  {l}");
        }
    }
    println!(
        "\ncommand   {}",
        s.command
            .iter()
            .map(|c| if c.len() > 60 {
                format!("{}…", &c[..60])
            } else {
                c.clone()
            })
            .collect::<Vec<_>>()
            .join(" ")
    );
    for w in &s.warnings {
        println!("warning   {w}");
    }
    cleanup();
    Ok(())
}

fn main() {
    // The start of a sandbox in a scope of its own (freeze.rs): stop, let
    // paddock move this process into the scope, then become the sandbox.
    // Before anything else, so the environment and the arguments pass as
    // they are.
    let argv: Vec<std::ffi::OsString> = std::env::args_os().collect();
    if argv
        .get(1)
        .is_some_and(|a| a == paddock::freeze::SCOPE_EXEC)
        && argv.len() > 2
    {
        std::process::exit(paddock::freeze::scope_exec(&argv[2..]));
    }
    match real_main() {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("paddock: {e:#}");
            std::process::exit(2);
        }
    }
}

fn real_main() -> Result<i32> {
    // Exit quietly when stdout is closed, for example by `| head`.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    let cli = Cli::parse();
    paddock::logging::init(i32::from(cli.verbose) - i32::from(cli.quiet));
    if let Cmd::Inbound = &cli.cmd {
        paddock::app::inbound_serve()?;
        return Ok(0);
    }
    if let Cmd::Portal = &cli.cmd {
        paddock::portal::serve()?;
        return Ok(0);
    }
    if let Cmd::Host { args } = &cli.cmd {
        return Ok(paddock::hostcli::main(args));
    }
    if let Cmd::Relay {
        listen,
        socket,
        forward,
        ready_file,
    } = &cli.cmd
    {
        paddock::proxy::relay(listen, socket, forward, ready_file.as_deref())?;
        return Ok(0);
    }
    let env = HostEnv::detect()?;
    paddock::privfs::secure_roots(&env);
    // A harness start: bring the menu and the shims up to date in the
    // background when harnesses or paddock changed.
    if matches!(
        cli.cmd,
        Cmd::Run(_) | Cmd::External(_) | Cmd::Wrap { .. } | Cmd::Shell(_)
    ) {
        paddock::refresh::spawn_if_changed(&env);
    }
    match cli.cmd {
        Cmd::Relay { .. } | Cmd::Host { .. } | Cmd::Inbound | Cmd::Portal => unreachable!(),
        Cmd::SpawnChild { job } => paddock::spawn::child_main(&env, &job),
        Cmd::Approvals { json, cmd } => {
            use paddock::broker;
            if let Some(ApprovalsCmd::Answer { id, answer, text }) = cmd {
                match (answer, text) {
                    (_, Some(t)) => {
                        let t = if t == "-" {
                            let mut s = String::new();
                            std::io::Read::read_to_string(
                                &mut std::io::Read::take(std::io::stdin(), 64 * 1024),
                                &mut s,
                            )?;
                            s.trim_end_matches(['\n', '\r']).to_string()
                        } else {
                            t
                        };
                        if t.trim().is_empty() {
                            bail!("the answer is empty");
                        }
                        broker::answer_reply(&env, &id, &t)?;
                        println!("Answered {id} with {} characters.", t.chars().count());
                    }
                    (Some(a), None) => {
                        broker::answer(&env, &id, a.parse()?)?;
                        println!("Answered {id}: {a}");
                    }
                    (None, None) => bail!("give an answer, or --text TEXT for a question"),
                }
                return Ok(0);
            }
            let list = broker::pending(&env);
            if json {
                println!(
                    "{}",
                    serde_json::json!({ "api_version": "paddock.approvals/v1", "pending": list })
                );
            } else if list.is_empty() {
                println!("No requests wait for an answer.");
            } else {
                for p in list {
                    // Files of an older paddock can hold raw sandbox text.
                    let p = p.cleaned();
                    println!("{}  {} in {}", p.id, p.harness, p.project.display());
                    if let Some(c) = paddock::spawn::chain_text(&p.started_by) {
                        println!("    {c}");
                    }
                    for l in p.detail.lines() {
                        println!("    {l}");
                    }
                    if !p.reason.is_empty() {
                        println!("    reason given by the agent: {}", p.reason);
                    }
                    if p.capability == broker::ASK_CAPABILITY {
                        if !p.choices.is_empty() {
                            println!("    choices: {}", p.choices.join(" | "));
                        }
                        println!(
                            "    answer: paddock approvals answer {} --text -   (type the answer, then Ctrl-D), or deny\n",
                            p.id
                        );
                        continue;
                    }
                    println!(
                        "    answer: paddock approvals answer {} once|{}deny\n",
                        p.id,
                        if p.session_allowed { "session|" } else { "" }
                    );
                }
            }
            Ok(0)
        }
        Cmd::Log { json, lines } => {
            let text =
                std::fs::read_to_string(paddock::broker::audit_file(&env)).unwrap_or_default();
            let all: Vec<&str> = text.lines().collect();
            for l in &all[all.len().saturating_sub(lines)..] {
                if json {
                    println!("{l}");
                } else if let Ok(v) = serde_json::from_str::<serde_json::Value>(l) {
                    println!(
                        "{}  {:<8} {:<15} {:<26} {}",
                        v["time"],
                        v["harness"].as_str().unwrap_or(""),
                        v["capability"].as_str().unwrap_or(""),
                        v["decision"].as_str().unwrap_or(""),
                        v["detail"]
                            .as_str()
                            .unwrap_or("")
                            .lines()
                            .last()
                            .unwrap_or("")
                    );
                }
            }
            Ok(0)
        }
        Cmd::Approver { cmd } => approver_cmd(&env, cmd),
        Cmd::App { cmd } => app_cmd(&env, cmd),
        Cmd::Skill { cmd } => {
            const SKILL: &str = include_str!("../skills/paddock-host/SKILL.md");
            match cmd {
                None => print!("{SKILL}"),
                Some(SkillCmd::Install) => {
                    for base in [".claude/skills", ".agents/skills"] {
                        let dir = env.home.join(base).join("paddock-host");
                        std::fs::create_dir_all(&dir)?;
                        std::fs::write(dir.join("SKILL.md"), SKILL)?;
                        println!("Installed {}", dir.join("SKILL.md").display());
                    }
                }
            }
            Ok(0)
        }
        Cmd::Run(a) => start(&env, &a, false),
        Cmd::Shell(a) => start(&env, &a, true),
        Cmd::Explain { run, json } => explain(&env, &run, json).map(|_| 0),
        Cmd::External(argv) => {
            let mut full = vec!["paddock run".to_string()];
            full.extend(argv);
            let a = <RunArgsCli as Parser>::try_parse_from(full).unwrap_or_else(|e| e.exit());
            start(&env, &a.run, false)
        }
        Cmd::Mcp { harness, path } => {
            use paddock::mcp::{assess, servers, Status};
            let f = HarnessFactory::new(&env)?;
            let Some(p) = f.profile(&harness) else {
                bail!("unknown harness '{harness}'")
            };
            let project = build::resolve_project(path.as_deref())?;
            let m = Manifest::load(&env, &project)?.merged;
            let list = servers(&env, p, &project);
            if list.is_empty() {
                println!("No MCP servers found in the {harness} configuration.");
            }
            for s in list {
                let status = match assess(&env, &s, &m, &project) {
                    Status::Ready { port: Some(p), .. } => format!("ready (host port {p})"),
                    Status::Ready { mounts, .. } if mounts.is_empty() => "ready".to_string(),
                    Status::Ready { mounts, .. } => format!(
                        "ready, mounts read-only: {}",
                        mounts.iter().map(|m| m.display().to_string()).collect::<Vec<_>>().join(", ")
                    ),
                    Status::Remote => "remote, uses the network".to_string(),
                    Status::NeedsAllow(paths) => format!(
                        "off: needs host paths that {} cannot grant. With \"{}\" in [mcp] allow, paddock mounts these read-only: {}",
                        if s.trusted { "this configuration" } else { "a project file" },
                        s.name,
                        paths.iter().map(|m| m.display().to_string()).collect::<Vec<_>>().join(", "),
                    ),
                    Status::NeedsPort(port) => format!("off: needs host port {port}. Add it to [network] host_ports"),
                    Status::NotFound(c) => format!("off: command {c} is not installed on the host"),
                };
                let from = if s.trusted { "" } else { " (project file)" };
                println!(
                    "{:<20} {status}\n{:<20} from {}{from}",
                    s.name, "", s.source
                );
            }
            println!("\nOutside the shared tool installs, paddock hides .env files and keys in every mounted folder.");
            println!("\nMCP OAuth tokens and server settings come from your host login (auth mode shared).");
            println!(
                "Put [mcp] and [network] settings in {} or in the project paddock.toml.",
                env.paddock_config().join("config.toml").display()
            );
            Ok(0)
        }
        Cmd::Env { cmd } => {
            match cmd {
                EnvCmd::Hide { names } | EnvCmd::Unhide { names } if names.is_empty() => {
                    bail!("name at least one variable")
                }
                EnvCmd::Hide { names } => {
                    let all = state::edit_env_hide(&env, &names, &[])?;
                    println!("Hidden from sandboxes: {}", all.join(", "));
                }
                EnvCmd::Unhide { names } => {
                    let all = state::edit_env_hide(&env, &[], &names)?;
                    println!(
                        "Hidden from sandboxes: {}",
                        if all.is_empty() {
                            "(none)".into()
                        } else {
                            all.join(", ")
                        }
                    );
                }
                EnvCmd::Show { path } => {
                    let project = build::resolve_project(path.as_deref())?;
                    let m = Manifest::load(&env, &project)?.merged;
                    let inherit = m.env.inherit.unwrap_or(true);
                    let (mut secret, mut plain, mut hidden, mut never, mut cloud) =
                        (vec![], vec![], vec![], vec![], vec![]);
                    for (k, v) in &env.vars {
                        if !paddock::policy::inheritable_var(k, v) {
                            never.push(k.clone());
                        } else if m.env.hide.contains(k) {
                            hidden.push(k.clone());
                        } else if paddock::policy::is_cloud_secret(k) && !m.env.pass.contains(k) {
                            cloud.push(k.clone());
                        } else if paddock::policy::looks_secret(k) {
                            secret.push(k.clone());
                        } else {
                            plain.push(k.clone());
                        }
                    }
                    println!(
                        "mode: {}",
                        if inherit {
                            "inherit (the default)"
                        } else {
                            "clean: only the allowlist passes"
                        }
                    );
                    if inherit {
                        println!(
                            "\npassed, look like secrets ({}):\n  {}",
                            secret.len(),
                            secret.join(" ")
                        );
                        println!("\npassed ({}):\n  {}", plain.len(), plain.join(" "));
                    }
                    println!(
                        "\nhidden by [env] hide ({}):\n  {}",
                        hidden.len(),
                        hidden.join(" ")
                    );
                    println!(
                        "\nnot passed: cloud credentials; add to [env] pass to allow ({}):\n  {}",
                        cloud.len(),
                        cloud.join(" ")
                    );
                    println!(
                        "\nnever passed: host sockets and values the sandbox sets ({}):\n  {}",
                        never.len(),
                        never.join(" ")
                    );
                }
            }
            Ok(0)
        }
        Cmd::List { json: true } => {
            let f = HarnessFactory::new(&env)?;
            let items: Vec<serde_json::Value> = f
                .names()
                .into_iter()
                .map(|name| {
                    let desc = f.profile(&name).map(|p| p.description.clone()).unwrap_or_default();
                    let app = f.profile(&name).is_some_and(|p| p.app.is_some());
                    let exe = f.create(&name).ok().map(|h| h.exe.display().to_string());
                    serde_json::json!({ "name": name, "description": desc, "installed": exe.is_some(), "exe": exe, "app": app })
                })
                .collect();
            println!(
                "{}",
                serde_json::json!({ "api_version": "paddock.list/v1", "harnesses": items })
            );
            Ok(0)
        }
        Cmd::Recent { json } => {
            let list = state::recent(&env);
            if json {
                println!(
                    "{}",
                    serde_json::json!({ "api_version": "paddock.recent/v1", "projects": list })
                );
            } else {
                for r in list {
                    println!("{:<10} {}", r.harness, r.project.display());
                }
            }
            Ok(0)
        }
        Cmd::Pick { harness } => pick(&env, &harness),
        Cmd::Gateway { cmd } => {
            use paddock::gatewayd;
            match cmd {
                GatewayCmd::Install => {
                    for p in gatewayd::install(&env)? {
                        println!("wrote {}", p.display());
                    }
                    println!("systemd now starts the gateway executor on the first request. Undo with: paddock gateway remove");
                }
                GatewayCmd::Remove => {
                    for p in gatewayd::remove(&env)? {
                        println!("removed {}", p.display());
                    }
                    println!("paddock starts the gateway executor itself when a request needs it.");
                }
                GatewayCmd::Status => println!("{}", gatewayd::status_line(&env)),
            }
            Ok(0)
        }
        Cmd::Gatewayd { idle } => {
            paddock::gatewayd::main(&env, std::time::Duration::from_secs(idle))
        }
        Cmd::Refresh { background } => {
            let r = paddock::refresh::sync(&env, !background)?;
            if !background {
                match &r.launchers {
                    Some(l) => println!("menu entries: {}", l.join(", ")),
                    None => println!("menu entries: off (paddock launchers install)"),
                }
                match &r.shims {
                    Some(s) => println!("shell shims: {}", s.join(", ")),
                    None => println!("shell shims: off (paddock shims install)"),
                }
            }
            Ok(0)
        }
        Cmd::OmarchyUsageUpdate { args } => paddock::omarchy_usage::update(&env, &args),
        Cmd::Wrap { command } => paddock::wrap::main(env, &command),
        Cmd::Shims {
            cmd: ShimsCmd::Exec { program, args },
        } => {
            if program != paddock::omarchy::SCOPE {
                bail!("paddock shims exec only runs omarchy-agent");
            }
            paddock::omarchy::exec_agent(&env, &args)
        }
        Cmd::Shims {
            cmd: ShimsCmd::LaunchTui { args },
        } => paddock::omarchy::launch_tui(&env, &args),
        Cmd::Shims {
            cmd: ShimsCmd::Install { scope },
        } if scope == "omarchy-agent" => {
            use paddock::omarchy;
            let r = omarchy::install(&env)?;
            println!(
                "Added the omarchy-agent wrapper to {} (one marked block).",
                r.config.display()
            );
            println!("Wrote the shim folder {}.", r.shims.display());
            match paddock::omarchy_usage::install(&env) {
                Ok(_) => println!(
                    "Added one marked block to {}: the agents panel checks the limits of a login that a sandbox holds from paddock's side. It takes effect at your next login.",
                    paddock::omarchy_usage::hypr_config(&env).display()
                ),
                Err(e) => println!("The agents panel keeps checking limits on the host: {e:#}"),
            }
            if let Err(e) = &r.reshim {
                println!("mise reshim did not run: {e}. Run it yourself, or the key keeps starting agents without a sandbox.");
            }
            println!("\nAgents that omarchy-agent starts (its key, the menu, the agents panel, `a`) now run in a sandbox.");
            println!(
                "Commands that you type and scripts do not change. Undo with: paddock shims remove"
            );
            if let Some(agent) = omarchy::default_agent(&env) {
                if paddock::wrap::profile_for(&env, &agent).is_none() {
                    println!("Your default agent {agent} has no paddock profile yet: it still starts without a sandbox.");
                }
            }
            Ok(0)
        }
        Cmd::Shims { cmd } => {
            use paddock::shims;
            match cmd {
                ShimsCmd::Exec { .. } | ShimsCmd::LaunchTui { .. } => unreachable!(),
                ShimsCmd::List => {
                    let s = paddock::omarchy::status(&env);
                    println!(
                        "scope omarchy-agent: {}",
                        if s.on() {
                            "on, agents that omarchy-agent starts run in a sandbox"
                        } else if s.user_wrapper {
                            "off, your mise config has its own [wrappers.omarchy-agent]"
                        } else if s.block || s.shims {
                            "incomplete, run paddock shims install --scope omarchy-agent again"
                        } else {
                            "off"
                        }
                    );
                    println!("scope shell:");
                    let on = shims::installed(&env);
                    let active = shims::active(&env);
                    for (harness, cmd) in shims::detect(&env)? {
                        let state = if on && active.contains(&cmd) {
                            "on "
                        } else {
                            "off"
                        };
                        println!("{state}  {cmd:<10} paddock run {harness}");
                    }
                    if on {
                        println!("\nShims are on. Run one command without the sandbox with PADDOCK_BYPASS=1. Turn them off with: paddock shims remove");
                    } else {
                        println!("\nShims are off. Turn them on with: paddock shims install");
                    }
                }
                ShimsCmd::Install { .. } => {
                    let r = shims::install(&env)?;
                    for (harness, cmd) in &r.shims {
                        println!("{cmd:<10} paddock run {harness}");
                    }
                    if r.rc_files.is_empty() {
                        println!("\nNo ~/.bashrc or ~/.zshrc found. Source {} from your shell configuration.", shims::script_file(&env).display());
                    } else {
                        for rc in &r.rc_files {
                            println!("\nAdded one line to {}.", rc.display());
                        }
                        println!("Open a new shell to use them. Run a command without the sandbox with PADDOCK_BYPASS=1. Undo with: paddock shims remove");
                    }
                }
                ShimsCmd::Remove => {
                    for p in shims::remove(&env)? {
                        println!("removed {}", p.display());
                    }
                    let (removed, reshim) = paddock::omarchy::remove(&env)?;
                    for p in removed
                        .into_iter()
                        .chain(paddock::omarchy_usage::remove(&env)?)
                    {
                        println!("removed {}", p.display());
                    }
                    if let Some(Err(e)) = reshim {
                        println!("mise reshim did not run: {e}. Run it yourself to remove the omarchy-agent dispatch link.");
                    }
                    println!("Open a new shell, or run `unset -f` on the commands, to stop using them in this shell.");
                }
            }
            Ok(0)
        }
        Cmd::Launchers { cmd } => {
            use paddock::launchers::{self, Kind};
            match cmd {
                LaunchersCmd::List => {
                    for e in launchers::detect(&env)? {
                        let kind = if e.kind == Kind::Cli {
                            "terminal"
                        } else {
                            "app"
                        };
                        println!("{:<10} {:<9} {}", e.name, kind, e.label);
                    }
                }
                LaunchersCmd::Install => {
                    let r = launchers::install(&env)?;
                    for e in &r.entries {
                        println!("{}  {}", e.label, e.desktop_file.display());
                    }
                    match r.omarchy_menu {
                        Some(f) => println!("\nThe Paddock folder is in the Omarchy menu (Super+Space), from {}.", f.display()),
                        None => println!("\nThe Paddock folder is in the menu of desktops that follow the XDG menu standard."),
                    }
                    println!("Search for \"paddock\" in your app launcher to see every entry. Undo with: paddock launchers remove");
                }
                LaunchersCmd::Remove => {
                    let removed = launchers::remove(&env)?;
                    if removed.is_empty() {
                        println!("Nothing to remove.");
                    }
                    for f in removed {
                        println!("removed {}", f.display());
                    }
                }
            }
            Ok(0)
        }
        Cmd::Windows { json, mark } => {
            let found = app_windows(&env);
            if mark {
                for (_, _, address) in &found {
                    mark_window(address);
                }
            }
            if json {
                let items: Vec<serde_json::Value> = found
                    .iter()
                    .map(|(id, harness, address)| serde_json::json!({ "sandbox": id, "app": harness, "address": address }))
                    .collect();
                println!(
                    "{}",
                    serde_json::json!({ "api_version": "paddock.windows/v1", "windows": items })
                );
            } else if !mark {
                for (id, harness, address) in &found {
                    println!("{id:<28} {harness:<16} {address}");
                }
            }
            Ok(0)
        }
        Cmd::Focus { id, display } => {
            let Some(r) = Running::list(&env).into_iter().find(|r| r.id == id) else {
                bail!("no running sandbox with id {id}")
            };
            let out = std::process::Command::new("hyprctl")
                .args(["clients", "-j"])
                .output()
                .map_err(|_| anyhow::anyhow!("focus needs Hyprland (hyprctl was not found)"))?;
            let clients: Vec<serde_json::Value> =
                serde_json::from_slice(&out.stdout).unwrap_or_default();
            let windows: Vec<(u32, String)> = clients
                .iter()
                .filter_map(|c| {
                    Some((
                        c["pid"].as_u64()? as u32,
                        c["address"].as_str()?.to_string(),
                    ))
                })
                .collect();
            let address_of = |pid: u32| {
                windows
                    .iter()
                    .find(|(p, _)| *p == pid)
                    .map(|(_, a)| a.clone())
            };
            let host_pid = if r.host_pid > 0 { r.host_pid } else { r.pid };
            let target = if display {
                r.window_pid.and_then(address_of)
            } else if r.gui == "app" {
                // An app sandbox draws its own host windows: find the window whose
                // process descends from this paddock process.
                windows.iter().find_map(|(pid, address)| {
                    let mut p = *pid;
                    for _ in 0..64 {
                        if p == host_pid {
                            return Some(address.clone());
                        }
                        match state::parent_pid(p) {
                            Some(q) if q > 1 => p = q,
                            _ => return None,
                        }
                    }
                    None
                })
            } else {
                // Walk up from paddock to the first process that owns a window: the terminal.
                let mut pid = host_pid;
                let mut found = None;
                for _ in 0..32 {
                    if let Some(a) = address_of(pid) {
                        found = Some(a);
                        break;
                    }
                    match state::parent_pid(pid) {
                        Some(p) if p > 1 => pid = p,
                        _ => break,
                    }
                }
                found
            };
            let Some(address) = target else {
                bail!("no window found for {id}; it may run without a terminal window")
            };
            // Hyprland with a Lua configuration takes the hl.dsp form; older
            // versions take the classic dispatcher. Try both, as Omarchy does.
            let dispatch = |arg: &str| {
                std::process::Command::new("hyprctl")
                    .args(["dispatch", arg])
                    .output()
                    .map(|o| {
                        o.status.success() && {
                            let t = String::from_utf8_lossy(&o.stdout).to_lowercase();
                            !t.contains("error") && !t.contains("not found")
                        }
                    })
                    .unwrap_or(false)
            };
            let ok = dispatch(&format!(
                "hl.dsp.focus({{ window = \"address:{address}\" }})"
            )) || std::process::Command::new("hyprctl")
                .args(["dispatch", "focuswindow", &format!("address:{address}")])
                .output()
                .map(|o| {
                    o.status.success() && {
                        let t = String::from_utf8_lossy(&o.stdout).to_lowercase();
                        !t.contains("error") && !t.contains("not found")
                    }
                })
                .unwrap_or(false);
            if ok {
                // Focus alone can leave a floating window under others: raise it.
                let _ = dispatch(&format!(
                    "hl.dsp.window.bring_to_top({{ window = \"address:{address}\" }})"
                ));
            }
            Ok(if ok { 0 } else { 1 })
        }
        Cmd::List { json: false } => {
            let f = HarnessFactory::new(&env)?;
            for name in f.names() {
                let desc = f
                    .profile(&name)
                    .map(|p| p.description.clone())
                    .unwrap_or_default();
                match f.create(&name) {
                    Ok(h) => println!("{name:<10} {desc:<24} {}", h.exe.display()),
                    Err(_) => println!("{name:<10} {desc:<24} (not installed)"),
                }
            }
            Ok(0)
        }
        Cmd::Freeze { id, tree, reason } => {
            for f in paddock::freeze::freeze(&env, &id, tree, &reason)? {
                println!("Froze {f}. Thaw it with: paddock thaw {f}");
            }
            Ok(0)
        }
        Cmd::Thaw { id, tree } => {
            for f in paddock::freeze::thaw(&env, &id, tree)? {
                println!("Thawed {f}.");
            }
            Ok(0)
        }
        Cmd::Ps { json } => {
            // Children follow their parent.
            let list = state::tree_order(Running::list(&env));
            if json {
                let rows: Vec<serde_json::Value> = list
                    .iter()
                    .map(|r| {
                        let mut v = serde_json::to_value(r).unwrap_or_default();
                        if let (Some(o), Some((since, reason))) =
                            (v.as_object_mut(), paddock::freeze::frozen(&env, &r.id))
                        {
                            o.insert("frozen_since".into(), since.into());
                            o.insert("frozen_reason".into(), reason.into());
                        }
                        v
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::json!({ "api_version": "paddock.ps/v1", "sandboxes": rows })
                );
            } else if list.is_empty() {
                println!("No running sandboxes.");
            } else {
                let now = state::now();
                let depth = |r: &Running| state::depth_in(&list, r);
                for r in &list {
                    let id = format!("{}{}", "  ".repeat(depth(r)), r.id);
                    let frozen = paddock::freeze::frozen(&env, &r.id)
                        .map(|(since, _)| format!("  frozen {}m", now.saturating_sub(since) / 60))
                        .unwrap_or_default();
                    println!(
                        "{:<28} {:<8} {:<7} {:<7} {:>5}m  {}{frozen}",
                        id,
                        r.harness,
                        r.backend,
                        r.gui,
                        (now.saturating_sub(r.started)) / 60,
                        r.project.display()
                    );
                }
            }
            Ok(0)
        }
        Cmd::Files { id, json } => files_cmd(&env, id.as_deref(), json),
        Cmd::Kv { cmd } => {
            use paddock::kv::host;
            match cmd {
                KvCmd::List {
                    prefix,
                    scope,
                    json,
                } => host::list(
                    &env,
                    &scope.target()?,
                    prefix.as_deref().unwrap_or(""),
                    json,
                ),
                KvCmd::Get { key, scope, json } => host::get(&env, &scope.target()?, &key, json),
                KvCmd::Delete { key, scope } => host::delete(&env, &scope.target()?, &key),
                KvCmd::Release { name, scope } => host::release(&env, &scope.target()?, &name),
            }
        }
        Cmd::Du { json } => du_cmd(&env, json),
        Cmd::Gc {
            yes,
            homes,
            older_than,
            include_app_logins,
            worktrees,
            force,
            scratch,
            json,
        } => gc_cmd(
            &env,
            &paddock::storage::GcOptions {
                yes,
                homes,
                older_than_days: older_than,
                include_app_logins,
                worktrees,
                force,
                scratch,
            },
            json,
        ),
        Cmd::Recover {
            id,
            paths,
            all,
            to,
            keep,
        } => recover_cmd(&env, &id, paths, all, to.as_deref(), keep),
        Cmd::Discard { id, paths, all } => discard_cmd(&env, &id, paths, all),
        Cmd::Stop { id } => {
            let all = Running::list(&env);
            let Some(r) = all.iter().find(|r| r.id == id) else {
                bail!("no running sandbox with id {id}")
            };
            // The sandboxes that this one started stop with it. Its own
            // paddock stops them too when it ends; this does not wait for that.
            let children = state::descendants(&all, &id);
            // A frozen process gets SIGTERM only when it thaws.
            let _ = paddock::freeze::thaw(&env, &id, true);
            for c in &children {
                unsafe { libc::kill(c.pid as i32, libc::SIGTERM) };
            }
            unsafe { libc::kill(r.pid as i32, libc::SIGTERM) };
            println!("Sent SIGTERM to {id}.");
            if !children.is_empty() {
                println!(
                    "Sent SIGTERM to the sandboxes it started: {}.",
                    children
                        .iter()
                        .map(|c| c.id.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            Ok(0)
        }
        Cmd::Doctor {
            escape,
            backend,
            network,
            gui,
            window,
            app,
            bus,
            online,
            env: env_mode,
            report,
            output,
        } => {
            if report {
                let text = paddock::report::build(&env);
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
                    &env,
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
                Ok(if doctor::report(&env)? { 0 } else { 1 })
            }
        }
        Cmd::Setup {
            yes,
            shims,
            no_shims,
            launchers,
            no_launchers,
            plugin,
            no_plugin,
            backend,
            apps,
        } => {
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
                backend,
                app_roots,
            };
            let mut term = if yes { None } else { setup::Terminal::detect() };
            let ask = term.as_mut().map(|t| t as &mut dyn setup::Ask);
            setup::run(&env, &o, ask)?;
            Ok(0)
        }
        Cmd::Update { check, yes } => update_cmd(&env, check, yes),
        Cmd::Auth {
            cmd: AuthCmd::Import { harness, auth },
        } => {
            let f = HarnessFactory::new(&env)?;
            let Some(p) = f.profile(&harness) else {
                bail!("unknown harness '{harness}'")
            };
            let done = state::auth_import(&env, p, &auth.parse()?)?;
            if done.is_empty() {
                println!("No credential files found for {harness}. Log in inside the sandbox instead: paddock {harness}");
            }
            for d in done {
                println!("Imported {}", d.display());
            }
            Ok(0)
        }
        Cmd::Approve { path } => {
            let project = build::resolve_project(path.as_deref())?;
            let file = project.join(manifest::PROJECT_FILE);
            let m = Manifest::read(&file)?;
            println!("{}", serde_json::to_string_pretty(&m.widening())?);
            manifest::approve(&env, &file, &m)?;
            println!("Approved {}", file.display());
            Ok(0)
        }
        Cmd::Init { path } => {
            let project = build::resolve_project(path.as_deref())?;
            let file = project.join(manifest::PROJECT_FILE);
            // A new file only: a dangling symlink there is not followed.
            let dir =
                paddock::safefs::Root::open(&project)?.dir(std::path::Path::new(""), false)?;
            let mut f = dir
                .create_new(std::ffi::OsStr::new(manifest::PROJECT_FILE), 0o644)
                .map_err(|e| anyhow::anyhow!("{} already exists: {e:#}", file.display()))?;
            std::io::Write::write_all(&mut f, manifest::TEMPLATE.as_bytes())?;
            println!("Wrote {}", file.display());
            Ok(0)
        }
    }
}

#[derive(Parser)]
#[command(name = "paddock")]
struct RunArgsCli {
    #[command(flatten)]
    run: RunArgs,
}

const WRAP_HELP: &str = "Examples:
  paddock wrap -- codex --approve-for-me
  paddock wrap -- env -u CLAUDE_CONFIG_DIR OMARCHY_AGENT_CLAUDE_HOME= claude --permission-mode auto

An `env` prefix keeps its variables. CLAUDE_CONFIG_DIR, CODEX_HOME and GROK_HOME choose the
account folder that the sandbox logs in with. In the home folder, the agent works in a scratch
project under ~/.local/share/paddock/scratch. PADDOCK_BYPASS=1 runs the command without a sandbox.";

const FILES_HELP: &str = "The project is shared with the host, so its changes are already yours. \
Files that a sandbox saves elsewhere, for example to ~/Desktop or ~/Downloads, stay in its private home. \
Recovery folders and ignore rules are set in [recovery] of ~/.config/paddock/config.toml:

  [recovery]
  folders = [\"Desktop\", \"Documents\", \"Downloads\", \"Pictures\", \"Videos\", \"Music\"]
  ignore = [\"*.desktop\", \"*.part\"]";

fn update_cmd(env: &HostEnv, check: bool, yes: bool) -> Result<i32> {
    use paddock::update::{self, Source};
    use std::io::{IsTerminal, Write};
    update::refuse_in_sandbox(env, "paddock update")?;
    let gh = update::GitHub::from_env(env, std::time::Duration::from_secs(30));
    let rel = gh.latest()?;
    let target = update::current_binary()?;
    let owner = update::package_owner(env, &target);
    let plugin = update::plugin_installed(env) == Some(true);
    let newer = update::is_newer(&rel.version, update::CURRENT);
    if newer {
        println!(
            "paddock {} is out; this is {}.",
            rel.version,
            update::CURRENT
        );
    } else {
        println!("paddock {} is the latest release.", update::CURRENT);
    }
    if newer {
        match &owner {
            Some(o) => println!("{}", update::package_hint(o, &rel.version)),
            None => println!(
                "An update downloads {} and its .sha256 from GitHub into a private folder, checks the checksum and the version of the new binary, and then replaces {}.",
                rel.tarball(),
                target.display()
            ),
        }
    }
    if plugin {
        println!(
            "The bar widget is installed. An update also runs: omarchy plugin update {} --yes",
            update::PLUGIN_ID
        );
    }
    let binary = newer && owner.is_none();
    // A package-owned binary that is out of date: refused, see the hint.
    let code = i32::from(newer && owner.is_some());
    if check || (!binary && !plugin) {
        return Ok(if check { 0 } else { code });
    }
    if !yes {
        let tty = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
        if !tty {
            println!("Run `paddock update --yes` to update.");
            return Ok(0);
        }
        eprint!("Update now? [y/N] ");
        std::io::stderr().flush().ok();
        let mut a = String::new();
        std::io::stdin().read_line(&mut a)?;
        if !matches!(a.trim(), "y" | "Y" | "yes") {
            println!("Nothing changed.");
            return Ok(1);
        }
    }
    if binary {
        let tar = env
            .which("tar")
            .ok_or_else(|| anyhow::anyhow!("paddock update needs tar"))?;
        update::install(env, &gh as &dyn Source, &rel, &target, None, &tar)?;
        println!("Updated {} to paddock {}.", target.display(), rel.version);
        println!("Running sandboxes keep the old version until they exit.");
        // The new version may bring profiles: its menu entries and shims.
        let _ = std::process::Command::new(&target)
            .arg("refresh")
            .stdin(std::process::Stdio::null())
            .status();
    }
    if plugin {
        update::update_plugin(env)?;
        println!("Updated the bar widget.");
    }
    Ok(code)
}

fn session_for(env: &HostEnv, id: &str) -> Result<paddock::recover::Session> {
    paddock::recover::Session::load(env, id)
        .ok_or_else(|| anyhow::anyhow!("no sandbox with id {id}; `paddock files` lists them"))
}

const DU_HELP: &str = "On btrfs, sizes are exclusive bytes from `btrfs filesystem du`: data that \
another file shares (reflinks, snapshots) does not count. On other file systems, sizes are \
allocated bytes, as `du` counts them. The header names the kind.";

const GC_HELP: &str = "Without --yes, paddock gc only prints what it would remove.

--yes alone removes: paddock images that are not current and that no container uses, keyring
passwords whose home is gone, session records that ended more than 30 days ago, homes of
harnesses whose profile no longer exists, and old shared caches (mise-installs). paddock also does this after it builds a new image
(PADDOCK_GC_AFTER_BUILD=0 turns that off).

--homes adds homes whose project is gone or that were not used for --older-than days (90).
A desktop app keeps its login in its home: those homes also need --include-app-logins.
--worktrees adds paddock worktrees. A worktree with unmerged commits or uncommitted changes
also needs --force, and paddock keeps its branch.
--scratch adds scratch projects (agents that `paddock wrap` started in the home folder) that
were not used for --older-than days. Their files are lost.

paddock never removes the home or worktree of a running sandbox, and never deletes outside
its own directories or through a symlink.

Examples:
  paddock gc                      what --yes would remove
  paddock gc --homes --older-than 30
  paddock gc --yes";

fn du_cmd(env: &HostEnv, json: bool) -> Result<i32> {
    use paddock::storage;
    let containers = storage::RealContainers(env);
    let host = storage::Host::real(env, &containers);
    let inv = storage::scan(env, &host, true);
    if json {
        let mut v = serde_json::to_value(&inv)?;
        v["api_version"] = "paddock.du/v1".into();
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        print!("{}", storage::render_du(env, &inv, state::now()));
    }
    Ok(0)
}

fn gc_cmd(env: &HostEnv, o: &paddock::storage::GcOptions, json: bool) -> Result<i32> {
    use paddock::storage;
    let containers = storage::RealContainers(env);
    let host = storage::Host::real(env, &containers);
    let mut inv = storage::scan(env, &host, false);
    let mut items = storage::plan(&inv, o, state::now());
    storage::measure_plan(&mut inv, &mut items);
    match paddock::kv::gc_items(env, state::now()) {
        Ok(kv) => items.extend(kv),
        Err(e) => inv
            .notes
            .push(format!("cannot read the paddock kv store: {e:#}")),
    }
    let removed = if o.yes {
        storage::apply(env, &containers, &items)
    } else {
        vec![]
    };
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "api_version": "paddock.gc/v1",
                "dry_run": !o.yes,
                "items": items,
                "removed": removed,
                "notes": inv.notes,
            }))?
        );
        return Ok(i32::from(removed.iter().any(|r| r.error.is_some())));
    }
    for n in &inv.notes {
        eprintln!("paddock: {n}");
    }
    if !o.yes {
        print!("{}", storage::render_plan(&items, false));
        return Ok(0);
    }
    let mut freed = 0;
    let mut failed = false;
    for r in &removed {
        match &r.error {
            None => {
                freed += r.bytes;
                let note = r
                    .note
                    .as_ref()
                    .map(|n| format!(", {n}"))
                    .unwrap_or_default();
                println!("removed {} ({}{note})", r.what, storage::human(r.bytes));
            }
            Some(e) => {
                failed = true;
                eprintln!("paddock: cannot remove {}: {e}", r.what);
            }
        }
    }
    let left: Vec<_> = items.iter().filter(|i| !i.selected()).collect();
    for i in &left {
        println!(
            "kept {} ({}): needs {}",
            i.what,
            storage::human(i.bytes),
            i.needs.join(" ")
        );
    }
    println!("paddock gc: freed {}.", storage::human(freed));
    Ok(i32::from(failed))
}

fn files_cmd(env: &HostEnv, id: Option<&str>, json: bool) -> Result<i32> {
    use paddock::recover::{self, RecoveryCfg, Session};
    let cfg = RecoveryCfg::load(env)?;
    let sessions = match id {
        Some(id) => vec![session_for(env, id)?],
        None => Session::latest_per_home(env),
    };
    let mut out = Vec::new();
    for s in sessions.into_iter().take(32) {
        let items = recover::scan(env, &cfg, &s.home, s.started);
        let running = s.running(env);
        if id.is_none() && items.is_empty() && !running {
            continue;
        }
        let changes = recover::project_changes(env, &s);
        out.push((s, running, items, changes));
    }
    // Scratch projects: where agents from omarchy-agent work when they start
    // in the home folder (`paddock wrap`).
    let scratch: Vec<(String, PathBuf, usize)> = if id.is_none() {
        std::fs::read_dir(paddock::storage::scratch_root(env))
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .map(|e| {
                let n = std::fs::read_dir(e.path()).map(|d| d.count()).unwrap_or(0);
                (e.file_name().to_string_lossy().into_owned(), e.path(), n)
            })
            .collect()
    } else {
        vec![]
    };
    if json {
        let list: Vec<_> = out
            .iter()
            .map(|(s, running, items, changes)| {
                serde_json::json!({
                    "id": s.id, "harness": s.harness, "project": s.project, "home": s.home,
                    "started": s.started, "ended": s.ended, "running": running,
                    "files": items, "project_changes": changes,
                })
            })
            .collect();
        let sc: Vec<_> = scratch
            .iter()
            .map(|(h, p, n)| serde_json::json!({ "harness": h, "project": p, "entries": n }))
            .collect();
        println!(
            "{}",
            serde_json::json!({ "api_version": "paddock.files/v1", "sandboxes": list, "scratch": sc })
        );
        return Ok(0);
    }
    if !scratch.is_empty() {
        println!("Scratch projects (agents from omarchy-agent that started in the home folder work here):");
        for (h, p, n) in &scratch {
            println!("  {h:<12} {}  ({n} files and folders)", p.display());
        }
        println!();
    }
    if out.is_empty() {
        println!("No sandbox has files outside its project.");
        return Ok(0);
    }
    for (s, running, items, changes) in &out {
        println!(
            "{}  {} in {}{}",
            s.id,
            s.harness,
            s.project.display(),
            if *running { "  (running)" } else { "" }
        );
        if items.is_empty() {
            println!("  no files in the recovery folders");
        }
        for i in items {
            println!(
                "  {} {:<48} {:>9}  -> {}",
                if i.new { "new" } else { "   " },
                i.path,
                i.size,
                i.target.display()
            );
        }
        if changes.git {
            let paths = if changes.changed_unknown {
                "an unknown number of".to_string()
            } else {
                changes.changed.len().to_string()
            };
            println!(
                "  project: {} new commit(s), {paths} changed path(s) since start",
                changes.commits.len(),
            );
            for c in changes.commits.iter().take(10) {
                println!("    commit {c}");
            }
            for c in changes.changed.iter().take(20) {
                println!("    {c}");
            }
        }
        println!();
    }
    println!("Copy a file to the host with `paddock recover ID PATH`, or delete it with `paddock discard ID PATH`.");
    Ok(0)
}

fn pick_paths(
    env: &HostEnv,
    cfg: &paddock::recover::RecoveryCfg,
    s: &paddock::recover::Session,
    paths: Vec<String>,
    all: bool,
) -> Vec<String> {
    if all {
        paddock::recover::scan(env, cfg, &s.home, s.started)
            .into_iter()
            .map(|i| i.path)
            .collect()
    } else {
        paths
    }
}

fn recover_cmd(
    env: &HostEnv,
    id: &str,
    paths: Vec<String>,
    all: bool,
    to: Option<&std::path::Path>,
    keep: bool,
) -> Result<i32> {
    let cfg = paddock::recover::RecoveryCfg::load(env)?;
    let s = session_for(env, id)?;
    let mut failed = 0;
    for p in pick_paths(env, &cfg, &s, paths, all) {
        match paddock::recover::recover(env, &cfg, &s.home, &p, to, keep) {
            Ok(r) => println!(
                "Recovered {} -> {}{}",
                r.from,
                r.to.display(),
                if r.renamed {
                    " (renamed: a file with that name exists)"
                } else {
                    ""
                }
            ),
            Err(e) => {
                failed += 1;
                eprintln!("paddock: cannot recover {p}: {e:#}");
            }
        }
    }
    Ok(if failed > 0 { 1 } else { 0 })
}

fn discard_cmd(env: &HostEnv, id: &str, paths: Vec<String>, all: bool) -> Result<i32> {
    let cfg = paddock::recover::RecoveryCfg::load(env)?;
    let s = session_for(env, id)?;
    let mut failed = 0;
    for p in pick_paths(env, &cfg, &s, paths, all) {
        match paddock::recover::discard(&cfg, &s.home, &p) {
            Ok(r) => println!("Deleted {r} from the sandbox"),
            Err(e) => {
                failed += 1;
                eprintln!("paddock: cannot delete {p}: {e:#}");
            }
        }
    }
    Ok(if failed > 0 { 1 } else { 0 })
}

fn approver_cmd(env: &HostEnv, cmd: ApproverCmd) -> Result<i32> {
    use paddock::approver::{self as ap, HookConfig};
    let target_user = |user: Option<String>| -> Result<String> {
        let u = user
            .or_else(|| std::env::var("SUDO_USER").ok())
            .unwrap_or_else(|| env.user.clone());
        if u.is_empty()
            || !u
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        {
            bail!("invalid user name");
        }
        Ok(u)
    };
    match cmd {
        ApproverCmd::Show => {
            let user = target_user(None)?;
            let sys = ap::system_file(&user);
            match ap::load_system(&user) {
                Ok(Some(h)) => println!(
                    "system: {} (trusted)\n  command: {}\n  capabilities: {}",
                    sys.display(),
                    h.command.join(" "),
                    h.capabilities.join(", ")
                ),
                Ok(None) => println!("system: none ({})", sys.display()),
                Err(e) => println!("system: {} is ignored: {e:#}", sys.display()),
            }
            match ap::load_signed(env)? {
                Some(s) => println!(
                    "password: {} (checked with the password when a sandbox starts)\n  command: {}\n  capabilities: {}",
                    ap::user_file(env).display(),
                    s.hook.command.join(" "),
                    s.hook.capabilities.join(", ")
                ),
                None => println!("password: none"),
            }
            println!("The system file wins when both exist.");
            Ok(0)
        }
        ApproverCmd::Enable {
            capabilities,
            timeout,
            system,
            user,
            command,
        } => {
            let hook = HookConfig {
                command,
                capabilities,
                timeout,
            };
            hook.validate()?;
            if system {
                if unsafe { libc::geteuid() } != 0 {
                    bail!("--system writes a root-owned file: run it with sudo");
                }
                let user = target_user(user)?;
                let f = ap::system_file(&user);
                for d in [
                    std::path::Path::new("/etc/paddock"),
                    std::path::Path::new(ap::SYSTEM_DIR),
                ] {
                    std::fs::create_dir_all(d)?;
                    std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o755))?;
                }
                std::fs::write(&f, toml::to_string(&hook)?)?;
                std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644))?;
                ap::root_owned_chain(&f)?;
                println!(
                    "Wrote {}. It applies to {user} from the next sandbox start.",
                    f.display()
                );
            } else {
                let p1 = rpassword::prompt_password(
                    "New password for the agent approver (at least 8 characters): ",
                )?;
                if p1.chars().count() < 8 {
                    bail!("the password needs at least 8 characters");
                }
                if rpassword::prompt_password("Again: ")? != p1 {
                    bail!("the passwords differ");
                }
                let signed = ap::sign(&hook, &p1)?;
                let f = ap::user_file(env);
                std::fs::create_dir_all(f.parent().unwrap())?;
                std::fs::write(&f, toml::to_string(&signed)?)?;
                std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o600))?;
                println!(
                    "Wrote {}. paddock asks for this password when a sandbox starts.",
                    f.display()
                );
            }
            Ok(0)
        }
        ApproverCmd::Disable { system, user } => {
            let f = if system {
                ap::system_file(&target_user(user)?)
            } else {
                ap::user_file(env)
            };
            match std::fs::remove_file(&f) {
                Ok(()) => println!("Removed {}", f.display()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    println!("{} does not exist", f.display())
                }
                Err(e) if system => bail!("cannot remove {}: {e}; run it with sudo", f.display()),
                Err(e) => return Err(e.into()),
            }
            Ok(0)
        }
        ApproverCmd::Test { capability } => {
            let Some((hook, _)) = ap::resolve(env, true)? else {
                bail!("no trusted approver is configured")
            };
            let p = paddock::broker::Pending {
                id: "r-test".into(),
                sandbox: "test".into(),
                harness: "test".into(),
                project: env.home.join("example-project"),
                capability: capability.clone(),
                summary: format!("sample {capability} request"),
                detail: format!("A sample {capability} request from paddock approver test"),
                reason: "testing the approver".into(),
                created: paddock::state::now(),
                expires: paddock::state::now() + 60,
                session_allowed: true,
                choices: Vec::new(),
                started_by: vec![],
            };
            let (a, note) = ap::run_hook(&hook, &p);
            match a {
                Some(a) => println!("answer: {a:?}  note: {note}"),
                None => println!("abstained or failed, so the user would decide: {note}"),
            }
            Ok(0)
        }
    }
}

/// Ask for a workspace folder with a desktop dialog: the recent projects, or
/// any folder. For a git repository, also ask whether to open it in a new
/// worktree; that is the default answer. None when the user cancels or
/// zenity is not installed.
fn pick_folder_gui(env: &HostEnv, name: &str) -> Option<(PathBuf, bool)> {
    let dir = pick_dir_gui(env, name)?;
    let is_repo = paddock::hostgit::Layout::discover(&dir)
        .ok()
        .flatten()
        .is_some_and(|l| {
            l.try_query(&["rev-parse", "--verify", "-q", "HEAD"])
                .is_some()
        });
    if !is_repo {
        return Some((dir, false));
    }
    let zenity = env.which("zenity")?;
    const THIS: &str = "Open this folder";
    let out = std::process::Command::new(zenity)
        .args(["--question", "--title", "Open in a new worktree?", "--width=480"])
        .arg(format!(
            "--text=Open {} in a new git worktree on its own branch?\n\nThe app then cannot switch branches or overwrite changes in your checkout. When it exits, paddock lists its commits and changes.",
            dir.display()
        ))
        .args(["--ok-label=New worktree", "--cancel-label=Cancel"])
        .arg(format!("--extra-button={THIS}"))
        .output()
        .ok()?;
    if out.status.success() {
        return Some((dir, true));
    }
    (String::from_utf8_lossy(&out.stdout).trim() == THIS).then_some((dir, false))
}

fn pick_dir_gui(env: &HostEnv, name: &str) -> Option<PathBuf> {
    let zenity = env.which("zenity")?;
    let label = paddock::app::app_profile(env, name)
        .ok()
        .and_then(|p| p.app.map(|a| a.display_name))
        .unwrap_or_else(|| name.to_string());
    let title = format!("Open {label} in paddock");
    const OTHER: &str = "Choose another folder…";
    // Recent projects, shown with ~. Hidden folders (caches, test runs) stay out.
    let mut projects: Vec<String> = Vec::new();
    for r in state::recent(env) {
        let Ok(rel) = r.project.strip_prefix(&env.home) else {
            continue;
        };
        if rel
            .components()
            .any(|c| c.as_os_str().to_string_lossy().starts_with('.'))
        {
            continue;
        }
        let p = format!("~/{}", rel.display());
        if !projects.contains(&p) {
            projects.push(p);
        }
    }
    let chosen = if projects.is_empty() {
        OTHER.to_string()
    } else {
        let out = std::process::Command::new(&zenity)
            .args(["--list", "--title", &title, "--width=640", "--height=420"])
            .args(["--text", "The app sees only this folder."])
            .args(["--column", "Project folder"])
            .args(&projects)
            .arg(OTHER)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    if chosen.is_empty() {
        return None;
    }
    if chosen != OTHER {
        return Some(env.expand(&chosen, &env.home));
    }
    let out = std::process::Command::new(&zenity)
        .args(["--file-selection", "--directory", "--title", &title])
        .arg(format!("--filename={}/", env.home.display()))
        .output()
        .ok()?;
    let dir = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !dir.is_empty()).then(|| PathBuf::from(dir))
}

fn app_cmd(env: &HostEnv, cmd: AppCmd) -> Result<i32> {
    use paddock::app;
    match cmd {
        AppCmd::Run {
            name,
            roots,
            gpu,
            network,
            env: env_mode,
            auth,
            backend,
            worktree,
        } => {
            let mut roots = roots;
            let mut worktree = worktree;
            // A launcher entry has no terminal to show "no workspace folders".
            // Ask for a folder on the desktop instead.
            if roots.is_empty()
                && app::roots(env, &name, &[]).is_err()
                && !std::io::IsTerminal::is_terminal(&std::io::stderr())
            {
                match pick_folder_gui(env, &name) {
                    Some((dir, new_worktree)) => {
                        roots.push(dir);
                        worktree |= new_worktree;
                    }
                    None if env.which("zenity").is_some() => return Ok(0),
                    None => {}
                }
            }
            let o = app::AppOptions {
                name: name.clone(),
                roots,
                gpu,
                network,
                inherit_env: env_mode.as_deref().map(|e| e == "inherit"),
                auth,
                args: vec![],
                backend,
                worktree,
            };
            match app::run(env, &o) {
                Ok(code) => Ok(code),
                Err(e) => {
                    // A launcher entry has no terminal: show the error on the desktop too.
                    if !std::io::IsTerminal::is_terminal(&std::io::stderr()) {
                        app::desktop_notice(env, &app::start_error_note(&name, &e));
                    }
                    Err(e)
                }
            }
        }
        AppCmd::Install {
            name,
            roots,
            no_links,
        } => {
            app::app_profile(env, &name)?;
            if !roots.is_empty() {
                let rs = app::roots(env, &name, &roots)?;
                state::set_app_roots(env, &name, &rs)?;
                println!("Saved the workspace folders in [apps.{name}] of the user configuration.");
            } else {
                app::roots(env, &name, &[]).map_err(|_| {
                    anyhow::anyhow!(
                        "name the workspace folders: paddock app install {name} ~/src/project"
                    )
                })?;
            }
            let f = app::install(env, &name)?;
            println!("Wrote {}. Start it from the app launcher.", f.display());
            if !no_links {
                for (scheme, old) in app::claim_links(env, &name)? {
                    println!("{scheme}:// links now open in the {name} sandbox (before: {}). paddock app uninstall {name} gives them back.", if old.is_empty() { "no handler" } else { &old });
                }
            }
            Ok(0)
        }
        AppCmd::Open { name, link } => app::open(env, &name, &link),
        AppCmd::Uninstall { name } => {
            for (scheme, old) in app::release_links(env, &name)? {
                println!(
                    "{scheme}:// links open with {} again",
                    if old.is_empty() { "no handler" } else { &old }
                );
            }
            let f = app::desktop_file(env, &name);
            match std::fs::remove_file(&f) {
                Ok(()) => println!("Removed {}", f.display()),
                Err(_) => println!("{} does not exist", f.display()),
            }
            Ok(0)
        }
        AppCmd::List => {
            let f = HarnessFactory::new(env)?;
            for name in f.names() {
                let Some(p) = f.profile(&name) else { continue };
                if p.app.is_none() {
                    continue;
                }
                let found = match p.app.as_ref().and_then(|a| a.flatpak.clone()) {
                    Some(id) => format!("flatpak {id}"),
                    None => f
                        .create(&name)
                        .map(|h| h.exe.display().to_string())
                        .unwrap_or_else(|_| "not installed".into()),
                };
                let launcher = if app::desktop_file(env, &name).exists() {
                    "launcher entry"
                } else {
                    "no launcher entry"
                };
                let roots = app::roots(env, &name, &[])
                    .map(|r| {
                        r.iter()
                            .map(|x| x.display().to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_else(|_| "no folders".into());
                println!(
                    "{name:<16} {found}\n{:<16} {launcher}; folders: {roots}",
                    ""
                );
                if app::unprotected_workspaces(env, &name).unwrap_or(false) {
                    let what = if p.app.as_ref().is_some_and(|a| a.flatpak.is_some()) {
                        "secret files in its folders are visible to it"
                    } else {
                        "has no effect: it applies only to flatpak apps"
                    };
                    println!("{:<16} warning: unprotected_workspaces is on: {what}", "");
                }
            }
            Ok(0)
        }
    }
}

/// Host windows that belong to app sandboxes: (sandbox id, app name, address).
/// A window belongs to a sandbox when its process descends from the paddock
/// process that runs the sandbox.
fn app_windows(env: &HostEnv) -> Vec<(String, String, String)> {
    let apps: Vec<Running> = Running::list(env)
        .into_iter()
        .filter(|r| r.gui == "app")
        .collect();
    if apps.is_empty() {
        return vec![];
    }
    let Ok(out) = std::process::Command::new("hyprctl")
        .args(["clients", "-j"])
        .output()
    else {
        return vec![];
    };
    let clients: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).unwrap_or_default();
    let mut found = Vec::new();
    for c in &clients {
        let (Some(pid), Some(address)) = (c["pid"].as_u64(), c["address"].as_str()) else {
            continue;
        };
        let mut p = pid as u32;
        'walk: for _ in 0..64 {
            for r in &apps {
                let host = if r.host_pid > 0 { r.host_pid } else { r.pid };
                if p == host {
                    found.push((r.id.clone(), r.harness.clone(), address.to_string()));
                    break 'walk;
                }
            }
            match state::parent_pid(p) {
                Some(q) if q > 1 => p = q,
                _ => break,
            }
        }
    }
    found
}

/// paddock's mark on a protected window: a sage border, set on that window
/// only, at run time. Nothing in the Hyprland configuration changes, and the
/// border goes away with the window.
fn mark_window(address: &str) {
    let lua = |prop: &str, value: &str| {
        format!("hl.dsp.window.set_prop({{ window = \"address:{address}\", prop = \"{prop}\", value = \"{value}\" }})")
    };
    let run = |args: &[&str]| {
        std::process::Command::new("hyprctl")
            .args(args)
            .output()
            .map(|o| {
                let t = String::from_utf8_lossy(&o.stdout).to_lowercase();
                o.status.success() && !t.contains("error") && !t.contains("not found")
            })
            .unwrap_or(false)
    };
    let active = "rgb(9fb8a0) rgb(7fd29a) 45deg";
    let inactive = "rgba(9fb8a088)";
    let ok = run(&["dispatch", &lua("active_border_color", active)]);
    if ok {
        run(&["dispatch", &lua("inactive_border_color", inactive)]);
    } else {
        // Hyprland before the Lua configuration. Debug only: the plugin runs
        // this every few seconds, so anything louder would fill the journal.
        log::debug!("window {address}: Lua set_prop failed, using setprop");
        let ok = run(&[
            "dispatch",
            "setprop",
            &format!("address:{address}"),
            "activebordercolor",
            active,
        ]);
        if !ok {
            log::debug!("window {address}: setprop failed too, no border");
        }
        run(&[
            "dispatch",
            "setprop",
            &format!("address:{address}"),
            "inactivebordercolor",
            inactive,
        ]);
    }
}

/// `paddock pick`: a project chooser for launcher entries, which start in a
/// terminal without a project directory.
fn pick(env: &HostEnv, harness: &str) -> Result<i32> {
    use std::io::{BufRead, Write};
    let f = HarnessFactory::new(env)?;
    if f.profile(harness).is_none() {
        bail!("unknown harness '{harness}'");
    }
    let mut recent = state::recent(env);
    // Projects last used with this harness first, then the others.
    recent.sort_by_key(|r| (r.harness != harness, std::cmp::Reverse(r.last)));
    recent.truncate(9);
    let home = env.home.display().to_string();
    let short = |p: &std::path::Path| {
        let s = p.display().to_string();
        match s.strip_prefix(&home) {
            Some(rest) => format!("~{rest}"),
            None => s,
        }
    };
    println!("Start {harness} in a paddock sandbox. Which project?\n");
    for (i, r) in recent.iter().enumerate() {
        let name = r
            .project
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        println!(
            "  {}  {:<20} {:<40} last: {}",
            i + 1,
            name,
            short(&r.project),
            r.harness
        );
    }
    if recent.is_empty() {
        println!("  No recent projects yet.");
    }
    let stdin = std::io::stdin();
    let path = loop {
        print!(
            "\n{}Type a number or a path (q to quit): ",
            if recent.is_empty() {
                ""
            } else {
                "Enter for 1. "
            }
        );
        std::io::stdout().flush().ok();
        let mut line = String::new();
        if stdin.lock().read_line(&mut line)? == 0 {
            return Ok(0);
        }
        let line = line.trim();
        if line == "q" {
            return Ok(0);
        }
        if line.is_empty() && !recent.is_empty() {
            break recent[0].project.clone();
        }
        if let Ok(n) = line.parse::<usize>() {
            if (1..=recent.len()).contains(&n) {
                break recent[n - 1].project.clone();
            }
            println!("There is no project {n}.");
            continue;
        }
        let p = env.expand(line, &env.home);
        if p.is_dir() {
            break p;
        }
        println!("{} is not a folder.", p.display());
    };
    println!();
    let me = std::env::current_exe()?;
    let status = std::process::Command::new(me)
        .arg(harness)
        .arg(&path)
        .status()?;
    let code = status.code().unwrap_or(1);
    if code != 0 {
        // Launcher terminals close when the command ends: keep the error visible.
        print!("\npaddock exited with {code}. Press Enter to close.");
        std::io::stdout().flush().ok();
        let mut s = String::new();
        let _ = stdin.lock().read_line(&mut s);
    }
    Ok(code)
}
