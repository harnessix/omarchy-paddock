use clap::{Args, Parser, Subcommand};
use paddock::spec::{GuiMode, Network};
use std::path::PathBuf;

pub(crate) mod app;
pub(crate) mod approver;
pub(crate) mod auth;
pub(crate) mod catalog;
pub(crate) mod credentials;
pub(crate) mod devices;
pub(crate) mod env;
pub(crate) mod forage;
pub(crate) mod gateway;
pub(crate) mod kv;
pub(crate) mod logins;
pub(crate) mod monitor;
pub(crate) mod pasture;
pub(crate) mod run;
pub(crate) mod setup;
pub(crate) mod shims;
pub(crate) mod skill;
pub(crate) mod status;
pub(crate) mod storage;
pub(crate) mod timer;
pub(crate) mod tools;
pub(crate) mod update;
pub(crate) mod windows;
pub(crate) mod worktree;

#[derive(Parser)]
#[command(
    name = "paddock",
    version,
    about = "Run a coding-agent harness with full auto-approval inside an isolated sandbox",
    long_about = None,
    after_help = "Examples:\n  paddock claude              start Claude Code in the current project\n  paddock codex ~/src/app     start Codex in another project\n  paddock explain claude      show every mount, variable and rule, and why\n  paddock doctor --escape     try to escape the sandbox and report the result"
)]
pub(crate) struct Cli {
    /// Show more log output: -v info, -vv debug, -vvv trace. PADDOCK_LOG=debug
    /// or PADDOCK_LOG=broker=debug sets levels too.
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub(crate) verbose: u8,
    /// Show only errors.
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub(crate) quiet: u8,
    #[command(subcommand)]
    pub(crate) cmd: Cmd,
}

#[derive(Subcommand)]
pub(crate) enum Cmd {
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
        cmd: shims::ShimsCmd,
    },
    /// Run an agent command as it is, in a sandbox: same arguments, folder,
    /// account and exit code (transparent mode). paddock adds no
    /// auto-approval flags. Commands without a profile run without a sandbox.
    #[command(after_help = run::WRAP_HELP)]
    Wrap {
        /// The command, after `--`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        command: Vec<String>,
    },
    Launchers {
        #[command(subcommand)]
        cmd: shims::LaunchersCmd,
    },
    /// List the protected windows: app sandbox windows, window mode displays
    /// and the terminal windows of runs (Hyprland). --mark gives them
    /// paddock's border, sage or amber by network level.
    Windows {
        #[arg(long)]
        json: bool,
        #[arg(long)]
        mark: bool,
        /// Focus this protected window (an address from --json).
        #[arg(long, value_name = "ADDRESS")]
        focus: Option<String>,
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
    /// Show what the sandbox boundary denied, grouped by kind and target.
    Boundary {
        id: String,
        #[arg(long)]
        json: bool,
        /// Keep running and print each new denial.
        #[arg(long)]
        follow: bool,
    },
    /// Stop a running sandbox.
    Stop { id: String },
    /// Wait until a sandbox ends. Prints what `paddock-host done` said.
    /// Exit code 0 for ok, 1 for failed or no `done`, 124 for a timeout.
    Wait {
        id: String,
        /// The longest wait, for example 90s, 30m or 2h.
        #[arg(long, value_name = "DURATION")]
        timeout: Option<String>,
    },
    /// Open a live view of a sandbox that runs without a terminal, in a new
    /// terminal window. Read-only unless --write.
    View {
        id: String,
        /// Attach with the keyboard. In a detached run (`run --detach`) the
        /// keys go to the agent: you take over. In a logged one-shot run,
        /// they only scroll the view.
        #[arg(long)]
        write: bool,
    },
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
    #[command(after_help = storage::DU_HELP)]
    Du {
        #[arg(long)]
        json: bool,
    },
    /// Remove paddock data that is no longer needed. A dry run unless --yes.
    #[command(after_help = storage::GC_HELP)]
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
        /// Also share the data of identical files in the sandbox homes
        /// (btrfs, xfs, bcachefs). Reports the space it gets back.
        #[arg(long)]
        dedupe: bool,
        /// Also scratch projects (starts in the home folder through `paddock
        /// wrap`) that were not used for --older-than days. Their files are lost.
        #[arg(long)]
        scratch: bool,
        /// Also project snapshots and undo trash folders older than --older-than
        /// days. paddock removes the snapshots of a project that is gone without it.
        #[arg(long)]
        snapshots: bool,
        /// With --worktrees: also worktrees with unmerged commits or uncommitted changes.
        #[arg(long)]
        force: bool,
        #[arg(long)]
        json: bool,
    },
    /// List the files a sandbox saved outside the project, and its project changes.
    #[command(after_help = storage::FILES_HELP)]
    Files {
        /// A sandbox id from `paddock ps` or an earlier run. Without it, every sandbox home with files.
        id: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Bring the project back to its snapshot from before a sandbox started.
    Undo {
        /// A session id from `paddock undo --list`. Without it, the last session of this project.
        session: Option<String>,
        /// List the snapshots of this project.
        #[arg(long)]
        list: bool,
        /// Restore without a question.
        #[arg(long, short = 'y')]
        yes: bool,
        /// Also restore files that changed after the session ended.
        #[arg(long)]
        force: bool,
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
    ///
    /// Release binaries exist for Linux on x86_64 and aarch64. On another
    /// architecture, build paddock from source with cargo.
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
        cmd: env::EnvCmd,
    },
    /// Manage the credentials that sandboxes use.
    Auth {
        #[command(subcommand)]
        cmd: auth::AuthCmd,
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
    #[command(after_help = approver::APPROVALS_HELP)]
    Approvals {
        /// Print JSON (api_version paddock.approvals/v1).
        #[arg(long)]
        json: bool,
        #[command(subcommand)]
        cmd: Option<approver::ApprovalsCmd>,
    },
    /// Pastures: sandboxes that you put together (--pasture NAME). They
    /// share paddock kv and the folder /pasture.
    Pasture {
        #[command(subcommand)]
        cmd: pasture::PastureCmd,
    },
    /// How much of each account's limits is used, how fast, and whether it
    /// runs out before it resets.
    Forage {
        /// Agents to show (claude, codex, ...). Default: all with numbers.
        agents: Vec<String>,
        /// Ask for fresh numbers first (at most once in 5 minutes each).
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        json: bool,
    },
    /// Timers of the running sandboxes: how long a task has run, and how
    /// long it may.
    Timer {
        #[command(subcommand)]
        cmd: timer::TimerCmd,
    },
    /// What the monitoring found in the running sandboxes (tunnels, large
    /// uploads, new credential files), and which projects stay quiet.
    #[command(after_help = monitor::MONITOR_HELP)]
    Monitor {
        #[command(subcommand)]
        cmd: monitor::MonitorCmd,
    },
    /// The OAuth logins that paddock keeps outside the sandboxes (Codex).
    Logins {
        #[command(subcommand)]
        cmd: logins::LoginsCmd,
    },
    /// The credentials registered in [credentials] of the user
    /// configuration, and how often each was delivered. Never a value.
    Credentials {
        #[command(subcommand)]
        cmd: credentials::CredentialsCmd,
    },
    /// The ledger and the budgets of paid devices.
    Devices {
        #[command(subcommand)]
        cmd: devices::DevicesCmd,
    },
    /// Read what agents wrote to paddock kv, and free their leases.
    Kv {
        #[command(subcommand)]
        cmd: kv::KvCmd,
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
    /// The git worktrees that paddock made for --worktree runs and apps: list, merge or remove them.
    Worktree {
        #[command(subcommand)]
        cmd: worktree::WorktreeCmd,
    },
    /// Run desktop agent apps (ChatGPT/Codex, Claude, VS Code, flatpak apps) in a sandbox.
    #[command(after_help = app::APP_HELP)]
    App {
        #[command(subcommand)]
        cmd: app::AppCmd,
    },
    /// Configure an agent that answers host requests in your place.
    #[command(after_help = approver::APPROVER_HELP)]
    Approver {
        #[command(subcommand)]
        cmd: approver::ApproverCmd,
    },
    /// Print the paddock-host skill for agents, or install it.
    Skill {
        #[command(subcommand)]
        cmd: Option<skill::SkillCmd>,
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
    /// The pane of a detached run (`paddock run --detach`). paddock starts
    /// this in its tmux session.
    #[command(hide = true)]
    DetachRun { job: PathBuf },
    /// After a sandbox ends: share the data of identical files of its home
    /// with the other homes, within a time limit. paddock starts this.
    #[command(hide = true)]
    DedupeHome { home: PathBuf },
    /// After a bwrap sandbox ends: copy the verified new packages of the
    /// project's cache layers into the shared layers. paddock starts this.
    #[command(hide = true)]
    PromoteCaches {
        project: PathBuf,
        /// The crates.io sparse index. Tests only: a local server.
        #[arg(long)]
        crates_index: Option<String>,
        /// The Go checksum database. Tests only: a local server.
        #[arg(long)]
        sumdb: Option<String>,
    },
    /// Update the launcher entries, the Omarchy menu and the shell shims to
    /// the harnesses installed now. Only the parts that are turned on.
    Refresh {
        /// Print nothing, and only add entries: the run that a harness start
        /// begins.
        #[arg(long)]
        background: bool,
    },
    /// Named host tools ([host.tools]): suggest blocks from the audit log.
    Tools {
        #[command(subcommand)]
        cmd: tools::ToolsCmd,
    },
    /// The executor of approved host requests: let systemd start it on the
    /// first request (socket activation), or turn that off again.
    Gateway {
        #[command(subcommand)]
        cmd: gateway::GatewayCmd,
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

/// Which scope a `paddock kv` command reads. Without a flag: the pastures
/// of the project of the current folder.
#[derive(Args)]
pub(crate) struct KvScope {
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

/// `--as NAME`: a principal from [kv.principals] of the user configuration.
#[derive(Args)]
pub(crate) struct KvAs {
    /// Act as this principal of [kv.principals]; needs --pasture.
    #[arg(long = "as", id = "as_", value_name = "NAME", requires = "pasture")]
    as_: Option<String>,
}

#[derive(Args)]
pub(crate) struct KvAsRequired {
    /// The principal of [kv.principals] to act as; needs --pasture.
    #[arg(long = "as", id = "as_", value_name = "NAME", requires = "pasture")]
    as_: String,
}

#[derive(Args, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct RunArgs {
    /// Harness name, for example claude or codex. Defaults to default_harness in paddock.toml.
    harness: Option<String>,
    /// Project directory. Defaults to the current directory, widened to its git root.
    path: Option<PathBuf>,
    /// auto, bwrap, docker or podman.
    #[arg(long, short)]
    backend: Option<String>,
    /// none, api, egress or host. api: only the model API hosts of the
    /// profile and the domains of --allow, over HTTPS through the paddock proxy.
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
    /// A domain the sandbox can reach in network modes api and allowlist
    /// (repeatable). HOST, HOST:PORT, or *.DOMAIN for its subdomains.
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
    /// default: start the harness with a private snapshot of its own host
    /// data (conversations, settings, plugins) in the sandbox home. The
    /// run changes only the snapshot, and paddock removes it at the end.
    /// bwrap only.
    #[arg(long, value_name = "PROFILE", value_parser = ["default"])]
    profile: Option<String>,
    /// Start the harness without its auto-approval flags.
    #[arg(long)]
    no_yolo: bool,
    /// Approve the project's paddock.toml without asking.
    #[arg(long, short = 'y')]
    yes: bool,
    /// Start the run in the background, in a session of paddock's own tmux
    /// server: the harness runs its interactive interface with the task as
    /// its first prompt. `paddock view ID --write` takes over, `paddock
    /// wait ID` waits for the end. Needs --prompt-file or --prompt.
    #[arg(long)]
    #[serde(skip)]
    detach: bool,
    /// With --detach: the file with the task.
    #[arg(
        long,
        value_name = "FILE",
        requires = "detach",
        conflicts_with = "prompt"
    )]
    #[serde(skip)]
    prompt_file: Option<PathBuf>,
    /// With --detach: the task as text.
    #[arg(long, value_name = "TEXT", requires = "detach")]
    #[serde(skip)]
    prompt: Option<String>,
    /// Arguments for the harness, after `--`.
    #[arg(last = true)]
    args: Vec<String>,
}

#[derive(Parser)]
#[command(name = "paddock")]
pub(crate) struct RunArgsCli {
    #[command(flatten)]
    run: RunArgs,
}
