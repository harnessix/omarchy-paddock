//! The limits in the Omarchy agents panel, checked from one side at a time.
//!
//! The panel runs `omarchy-agent-usage-update` by name. Each collector reads
//! an agent's login on the host and asks the vendor for its limits. While a
//! paddock sandbox holds a copy of that login, the host must not use it too.
//!
//! - One marked block at the end of `~/.config/hypr/hyprland.lua`, where
//!   Omarchy asks for the user's own configuration, loads a file of paddock.
//!   It puts a folder first on the PATH of everything that Hyprland starts,
//!   ahead of `$OMARCHY_PATH/bin`, the same way as Omarchy's `envs.lua`. The
//!   folder holds one command, `omarchy-agent-usage-update`, which runs
//!   `paddock omarchy-usage-update`.
//! - Without a held login, that runs Omarchy's command unchanged.
//! - For an agent whose login a sandbox holds, it adds `--except AGENT`, so
//!   the host does not check that login. For Claude Code and Grok Build,
//!   paddock then runs Omarchy's own collector in a sandbox of its own, with
//!   network mode egress, which sees the host home read-only and the
//!   sandbox's copy of the login in place of the host file. Its record goes
//!   where Omarchy's would. Those collectors only read a login, they never
//!   refresh it.
//! - The Codex collector starts `codex app-server`, which can refresh the
//!   login. A Codex login that a keeper keeps (keeper.rs) is checked in a
//!   keeper sandbox, with the full login and under the keeper's refresh
//!   lock, so it never refreshes beside the keeper; a refresh that it makes
//!   goes back to the host and into the sandboxes. Without a keeper
//!   (`[logins] keep = false`), nobody checks a held Codex login, and the
//!   record keeps its last numbers, marked as paused.
//!
//! paddock checks only when the panel asks. It never adds a timer.

use crate::env::HostEnv;
use crate::state::{HeldLogin, Running};
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The command that the panel runs.
pub const COMMAND: &str = "omarchy-agent-usage-update";
const MARK: &str = "# Written by paddock (paddock shims remove deletes it).";

/// An agent of the panel, the paddock profile whose logins it reads, and
/// whether its collector only reads the login (never refreshes it).
pub struct Agent {
    pub id: &'static str,
    pub profile: &'static str,
    pub read_only: bool,
}

pub const AGENTS: &[Agent] = &[
    Agent {
        id: "claude",
        profile: "claude",
        read_only: true,
    },
    Agent {
        id: "grok",
        profile: "grok",
        read_only: true,
    },
    Agent {
        id: "codex",
        profile: "codex",
        read_only: false,
    },
];

/// The folder that goes first on the session PATH.
pub fn hook_bin(env: &HostEnv) -> PathBuf {
    env.paddock_data().join("omarchy-usage").join("bin")
}

/// The Lua file that Hyprland loads.
pub fn hypr_file(env: &HostEnv) -> PathBuf {
    env.paddock_data().join("omarchy-usage").join("hypr.lua")
}

/// The user's Hyprland configuration.
pub fn hypr_config(env: &HostEnv) -> PathBuf {
    env.config_dir.join("hypr").join("hyprland.lua")
}

/// The session file of the first version (uwsm), which Hyprland's PATH
/// overrides. install and remove delete it.
fn old_dropin(env: &HostEnv) -> PathBuf {
    env.config_dir
        .join("uwsm")
        .join("env.d")
        .join("60-paddock-usage")
}

const BEGIN: &str = "-- paddock: begin (managed by paddock, remove with `paddock shims remove`)";
const END: &str = "-- paddock: end";
const LUA_MARK: &str = "-- Written by paddock (paddock shims remove deletes it).";

fn omarchy_path(env: &HostEnv) -> PathBuf {
    PathBuf::from(env.var("OMARCHY_PATH").unwrap_or("/usr/share/omarchy"))
}

/// Omarchy's own command.
pub fn real_command(env: &HostEnv) -> PathBuf {
    omarchy_path(env).join("bin").join(COMMAND)
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn lua_quote(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The block in the user's Hyprland configuration. A missing file is no
/// error, so Hyprland starts the same after paddock is gone.
pub fn hypr_block(file: &Path) -> String {
    format!(
        "{BEGIN}\n-- The Omarchy agents panel checks limits through paddock.\npcall(dofile, {})\n{END}\n",
        lua_quote(&file.display().to_string())
    )
}

/// The Lua file: paddock's folder first, then Omarchy's, then the rest.
pub fn hypr_lua(bin: &Path) -> String {
    format!(
        "{LUA_MARK}\n\
         -- paddock's omarchy-agent-usage-update goes before Omarchy's on PATH, so a\n\
         -- login that a paddock sandbox holds is checked from one side only.\n\
         local ours = {}\n\
         local omarchy = (os.getenv(\"OMARCHY_PATH\") or \"/usr/share/omarchy\") .. \"/bin\"\n\
         local kept = {{ ours, omarchy }}\n\
         for entry in (os.getenv(\"PATH\") or \"/usr/local/bin:/usr/bin\"):gmatch(\"[^:]+\") do\n\
         \x20 if entry ~= ours and entry ~= omarchy then table.insert(kept, entry) end\n\
         end\n\
         hl.env(\"PATH\", table.concat(kept, \":\"))\n",
        lua_quote(&bin.display().to_string())
    )
}

pub fn strip_block(text: &str) -> String {
    let mut out = String::new();
    let mut inside = false;
    for line in text.split_inclusive('\n') {
        let l = line.trim_end();
        if l == BEGIN {
            inside = true;
        } else if inside && l == END {
            inside = false;
        } else if !inside {
            out.push_str(line);
        }
    }
    out
}

/// The configuration with the block at its end, where Omarchy asks for the
/// user's own configuration. Running it again changes nothing.
pub fn insert_block(text: &str, block: &str) -> String {
    let mut out = strip_block(text);
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    if !out.is_empty() && !out.ends_with("\n\n") {
        out.push('\n');
    }
    out.push_str(block);
    out
}

pub fn hook_script(paddock: &Path) -> String {
    format!(
        "#!/bin/sh\n\
         {MARK}\n\
         p={}\n\
         if [ -x \"$p\" ]; then exec \"$p\" omarchy-usage-update -- \"$@\"; fi\n\
         exec \"${{OMARCHY_PATH:-/usr/share/omarchy}}/bin/{COMMAND}\" \"$@\"\n",
        sh_quote(&paddock.display().to_string())
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    /// The block in hyprland.lua.
    pub block: bool,
    /// The Lua file and the command.
    pub files: bool,
    /// `omarchy-agent-usage-update` on this PATH is paddock's (it needs a
    /// new login, or a reload of Hyprland and the Omarchy shell).
    pub active: bool,
}

pub fn status(env: &HostEnv) -> Status {
    let has = |p: &Path, mark: &str| {
        std::fs::read_to_string(p)
            .map(|t| t.contains(mark))
            .unwrap_or(false)
    };
    let script = hook_bin(env).join(COMMAND);
    Status {
        block: has(&hypr_config(env), BEGIN),
        files: has(&hypr_file(env), LUA_MARK) && has(&script, MARK),
        active: env.which(COMMAND).as_deref() == Some(script.as_path()),
    }
}

fn write_keep_mode(path: &Path, text: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o7777)
        .unwrap_or(0o644);
    let tmp = path.with_extension("paddock-tmp");
    let _ = std::fs::remove_file(&tmp);
    std::fs::write(&tmp, text)?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn remove_old_dropin(env: &HostEnv) -> Result<Option<PathBuf>> {
    let f = old_dropin(env);
    if std::fs::read_to_string(&f)
        .map(|t| t.contains(MARK))
        .unwrap_or(false)
    {
        std::fs::remove_file(&f)?;
        return Ok(Some(f));
    }
    Ok(None)
}

/// Check Lua source with `luac -p` when a Lua compiler is installed. A
/// broken hyprland.lua would break the desktop.
fn lua_compiles(env: &HostEnv, text: &str) -> Result<()> {
    let Some(luac) = ["luac", "luac5.4", "luac5.3"]
        .iter()
        .find_map(|n| env.which(n))
    else {
        return Ok(());
    };
    let run = env.paddock_run();
    crate::privfs::dir(&run)?;
    let f = run.join(format!("hyprland-check-{}.lua", std::process::id()));
    crate::privfs::write(&f, text)?;
    let out = Command::new(luac).arg("-p").arg(&f).output();
    let _ = std::fs::remove_file(&f);
    let out = out.context("cannot run luac")?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// Write the folder, the Lua file and the block. Returns the changed
/// files. Without a `hyprland.lua`, nothing is changed.
pub fn install(env: &HostEnv) -> Result<Vec<PathBuf>> {
    use std::os::unix::fs::PermissionsExt;
    let config = hypr_config(env);
    let text = match std::fs::read_to_string(&config) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!(
                "{} does not exist, so the panel's limit checks stay on the host",
                config.display()
            )
        }
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", config.display())),
    };
    let paddock = crate::launchers::launcher_exe(env)?;
    let bin = hook_bin(env);
    crate::privfs::dir(&bin)?;
    let script = bin.join(COMMAND);
    crate::privfs::write_atomic(&script, hook_script(&paddock))?;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700))?;
    let lua = hypr_file(env);
    crate::privfs::write_atomic(&lua, hypr_lua(&bin))?;
    let mut changed = vec![];
    let new = insert_block(&text, &hypr_block(&lua));
    if new != text {
        lua_compiles(env, &new).with_context(|| {
            format!(
                "paddock's block would make {} invalid; nothing was changed",
                config.display()
            )
        })?;
        write_keep_mode(&config, &new)?;
        changed.push(config);
    }
    changed.push(lua);
    changed.push(script);
    changed.extend(remove_old_dropin(env)?);
    Ok(changed)
}

/// Remove what `install` wrote, and keep the rest of hyprland.lua byte
/// for byte.
pub fn remove(env: &HostEnv) -> Result<Vec<PathBuf>> {
    let mut removed = Vec::new();
    let config = hypr_config(env);
    if let Ok(text) = std::fs::read_to_string(&config) {
        let mut clean = strip_block(&text);
        // insert_block added one empty line before the block.
        if text.contains(BEGIN) && clean.ends_with("\n\n") {
            clean.pop();
        }
        if clean != text {
            write_keep_mode(&config, &clean)?;
            removed.push(config);
        }
    }
    let root = env.paddock_data().join("omarchy-usage");
    if root.exists() {
        crate::safefs::remove_tree_path(&root)?;
        removed.push(root);
    }
    removed.extend(remove_old_dropin(env)?);
    Ok(removed)
}

/// The arguments of Omarchy's command.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Args {
    pub flags: Vec<String>,
    pub except: Vec<String>,
    pub only: Vec<String>,
}

impl Args {
    pub fn parse(args: &[String]) -> Self {
        let mut a = Args::default();
        let mut it = args.iter();
        while let Some(x) = it.next() {
            match x.as_str() {
                "--force" | "--limits-only" => a.flags.push(x.clone()),
                "--except" => {
                    if let Some(v) = it.next() {
                        a.except.push(v.clone());
                    }
                }
                _ => a.only.push(x.clone()),
            }
        }
        a
    }

    pub fn wanted(&self, agent: &str) -> bool {
        !self.except.iter().any(|e| e == agent)
            && (self.only.is_empty() || self.only.iter().any(|o| o == agent))
    }
}

/// The logins of each agent that running sandboxes hold. For a store that
/// several sandboxes hold, the newest one wins.
pub fn held(running: &[Running]) -> BTreeMap<&'static str, Vec<HeldLogin>> {
    let mut out: BTreeMap<&'static str, Vec<HeldLogin>> = BTreeMap::new();
    let mut list: Vec<&Running> = running.iter().collect();
    list.sort_by_key(|r| std::cmp::Reverse(r.started));
    for r in list {
        let Some(agent) = AGENTS.iter().find(|a| a.profile == r.harness) else {
            continue;
        };
        let v = out.entry(agent.id).or_default();
        for l in &r.logins {
            if !v.iter().any(|x| x.store == l.store) {
                v.push(l.clone());
            }
        }
        if v.is_empty() {
            out.remove(agent.id);
        }
    }
    out
}

/// `paddock omarchy-usage-update -- ARGS`.
pub fn update(env: &HostEnv, raw: &[String]) -> Result<i32> {
    let real = real_command(env);
    let args = Args::parse(raw);
    let held: BTreeMap<_, _> = held(&Running::list(env))
        .into_iter()
        .filter(|(agent, _)| args.wanted(agent))
        .collect();
    let mut cmd = Command::new(&real);
    cmd.args(raw).env("PATH", path_without_hook(env));
    if held.is_empty() {
        let e = cmd.exec();
        bail!("cannot run {}: {e}", real.display());
    }
    for agent in held.keys() {
        cmd.args(["--except", agent]);
    }
    let mut status = cmd
        .status()
        .with_context(|| format!("cannot run {}", real.display()))?
        .code()
        .unwrap_or(1);
    for (agent, logins) in &held {
        let read_only = AGENTS.iter().any(|a| &a.id == agent && a.read_only);
        let done = if read_only {
            collect(env, agent, logins, &args.flags)
        } else {
            collect_kept(env, agent, logins, &args.flags)
        };
        if let Err(e) = done {
            log::debug!("{agent}: {e:#}");
            if let Err(e) = mark_held(env, agent) {
                eprintln!("paddock: cannot mark the {agent} record: {e:#}");
                status = 1;
            }
        }
    }
    Ok(status)
}

fn path_without_hook(env: &HostEnv) -> String {
    let bin = hook_bin(env);
    env.path
        .iter()
        .filter(|d| **d != bin)
        .map(|d| d.display().to_string())
        .collect::<Vec<_>>()
        .join(":")
}

fn usage_dir(env: &HostEnv) -> PathBuf {
    env.state_dir.join("omarchy").join("agents").join("usage")
}

fn write_record(env: &HostEnv, agent: &str, record: &serde_json::Value) -> Result<()> {
    let dir = usage_dir(env);
    std::fs::create_dir_all(&dir)?;
    let tmp = dir.join(format!(".{agent}.paddock-{}", std::process::id()));
    std::fs::write(&tmp, format!("{record}\n"))?;
    std::fs::rename(&tmp, dir.join(format!("{agent}.json")))?;
    Ok(())
}

pub const HELD_HELP: &str = "A paddock sandbox is using this login, so the panel does not check its limits now. They update when the sandbox exits.";

/// Keep the last record, marked as paused.
pub fn mark_held(env: &HostEnv, agent: &str) -> Result<()> {
    let f = usage_dir(env).join(format!("{agent}.json"));
    let Ok(text) = std::fs::read_to_string(&f) else {
        return Ok(());
    };
    let mut v: serde_json::Value = serde_json::from_str(&text)?;
    let Some(o) = v.as_object_mut() else {
        return Ok(());
    };
    o.insert("limitsStale".into(), true.into());
    o.insert("usageStatusText".into(), "Limits paused".into());
    o.insert("authHelpText".into(), HELD_HELP.into());
    write_record(env, agent, &v)
}

/// The largest record that paddock accepts from a collector.
const MAX_RECORD: u64 = 8 << 20;
const COLLECT_TIMEOUT: Duration = Duration::from_secs(120);

/// Run Omarchy's collector for `agent` in a sandbox that sees the held
/// logins in place of the host files, and write its record.
fn collect(env: &HostEnv, agent: &str, logins: &[HeldLogin], flags: &[String]) -> Result<()> {
    let collector = omarchy_path(env)
        .join("bin")
        .join(format!("omarchy-agent-usage-{agent}"));
    if !crate::env::is_executable(&collector) {
        bail!("{} is missing", collector.display());
    }
    let run = env
        .paddock_run()
        .join(format!("usage-{agent}-{}", std::process::id()));
    crate::privfs::dir(&run)?;
    let result = (|| {
        let binds = login_copies(&run, logins)?;
        if binds.is_empty() {
            bail!("no copy of the login could be read");
        }
        let mut cmd = collector_command(env, &run, &binds, &collector, flags, true)?;
        let record = run_capped(&mut cmd)?;
        let v: serde_json::Value =
            serde_json::from_slice(&record).context("the collector did not print JSON")?;
        if v.get("id").and_then(|i| i.as_str()) != Some(agent) {
            bail!("the collector printed a record for another agent");
        }
        write_record(env, agent, &v)
    })();
    let _ = std::fs::remove_dir_all(&run);
    result
}

/// A login that a keeper keeps (keeper.rs): Omarchy's collector runs in a
/// keeper sandbox with the full login, under the keeper's refresh lock, so
/// it never refreshes beside the keeper. Without a keeper, nobody checks a
/// login whose collector can refresh it.
fn collect_kept(env: &HostEnv, agent: &str, logins: &[HeldLogin], flags: &[String]) -> Result<()> {
    let profile = AGENTS
        .iter()
        .find(|a| a.id == agent)
        .map(|a| a.profile)
        .context("unknown agent")?;
    let kept = Running::list(env)
        .into_iter()
        .flat_map(|r| r.kept_logins)
        .any(|k| logins.iter().any(|l| l.store == k.store));
    if !kept || crate::keeper::kind_for(profile).is_none() {
        bail!("its collector can refresh the login, so nobody checks it while a sandbox holds it");
    }
    let collector = omarchy_path(env)
        .join("bin")
        .join(format!("omarchy-agent-usage-{agent}"));
    if !crate::env::is_executable(&collector) {
        bail!("{} is missing", collector.display());
    }
    let record = crate::keeper::collect_usage(env, profile, &logins[0].store, &collector, flags)?;
    let v: serde_json::Value =
        serde_json::from_slice(&record).context("the collector did not print JSON")?;
    if v.get("id").and_then(|i| i.as_str()) != Some(agent) {
        bail!("the collector printed a record for another agent");
    }
    write_record(env, agent, &v)
}

/// Private copies of the held logins: (copy, host file). A copy is read
/// through `safefs` and must be JSON, so a link that the sandbox planted is
/// never followed. A host file that does not exist cannot be replaced, so
/// its login is left out.
pub fn login_copies(run: &Path, logins: &[HeldLogin]) -> Result<Vec<(PathBuf, PathBuf)>> {
    let mut out = Vec::new();
    for (n, l) in logins.iter().enumerate() {
        if !l.store.is_file() {
            continue;
        }
        let bytes = match crate::safefs::Root::open(&l.home)
            .and_then(|r| r.read(&l.rel, crate::safefs::MAX_READ))
        {
            Ok(Some(b)) => b,
            Ok(None) => continue,
            Err(e) => {
                log::debug!("not reading {}: {e:#}", l.rel.display());
                continue;
            }
        };
        if serde_json::from_slice::<serde_json::Value>(&bytes).is_err() {
            continue;
        }
        let copy = run.join(format!("login-{n}"));
        crate::privfs::write(&copy, &bytes)?;
        out.push((copy, l.store.clone()));
    }
    Ok(out)
}

/// The sandbox for a collector: the system and the host home read-only, the
/// login copies over the host files, Omarchy's cache folder writable, and
/// with `network`, network mode egress (LAN blocked).
pub fn collector_command(
    env: &HostEnv,
    run: &Path,
    binds: &[(PathBuf, PathBuf)],
    collector: &Path,
    flags: &[String],
    network: bool,
) -> Result<Command> {
    use crate::backend::bwrap::{resolv_target, DNS_FORWARD, ROOT_LINKS};
    let bwrap = env.which("bwrap").context("bwrap not found")?;
    let s = |p: &Path| p.display().to_string();
    let mut a: Vec<String> = [
        "--die-with-parent",
        "--unshare-all",
        "--unshare-user",
        "--disable-userns",
        "--new-session",
    ]
    .map(String::from)
    .to_vec();
    if network {
        a.push("--share-net".into());
    }
    a.extend(["--ro-bind", "/usr", "/usr"].map(String::from));
    for d in ROOT_LINKS {
        let path = Path::new(d);
        match std::fs::read_link(path) {
            Ok(t) => a.extend(["--symlink".into(), s(&t), d.to_string()]),
            Err(_) if path.is_dir() => a.extend(["--ro-bind", d, d].map(String::from)),
            Err(_) => {}
        }
    }
    a.extend(["--ro-bind", "/etc", "/etc"].map(String::from));
    for d in ["/opt", "/nix"] {
        a.extend(["--ro-bind-try", d, d].map(String::from));
    }
    a.extend(
        [
            "--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp", "--tmpfs", "/run", "--tmpfs",
            "/var/tmp",
        ]
        .map(String::from),
    );
    if network {
        let resolv = run.join("resolv.conf");
        crate::privfs::write(
            &resolv,
            format!("nameserver {DNS_FORWARD}\noptions edns0\n"),
        )?;
        std::fs::set_permissions(&resolv, std::os::unix::fs::PermissionsExt::from_mode(0o644))?;
        a.extend(["--ro-bind".into(), s(&resolv), s(&resolv_target())]);
    }
    a.extend(["--ro-bind".into(), s(&env.home), s(&env.home)]);
    let cache = env.cache_dir.join("omarchy").join("agent-usage");
    std::fs::create_dir_all(&cache)?;
    a.extend(["--bind".into(), s(&cache), s(&cache)]);
    for (copy, store) in binds {
        a.extend(["--ro-bind".into(), s(copy), s(store)]);
    }
    a.push("--".into());
    a.push(s(collector));
    a.extend(flags.iter().cloned());

    let (program, args) = if network {
        let spec = crate::spec::SandboxSpec::new(
            format!("usage-{}", std::process::id()),
            "omarchy-usage",
            &env.home,
            env.home.clone(),
            &env.home,
            env.uid,
            env.gid,
            &env.user,
        );
        let (pasta, mut pa) = crate::backend::bwrap::pasta_prefix(env, &spec)?;
        pa.push(s(&bwrap));
        pa.extend(a);
        (pasta, pa)
    } else {
        (bwrap, a)
    };
    let mut cmd = Command::new(program);
    cmd.args(args).env_clear();
    cmd.env("HOME", &env.home)
        .env("PATH", path_without_hook(env));
    for k in [
        "USER",
        "LANG",
        "LC_ALL",
        "TZ",
        "OMARCHY_PATH",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_STATE_HOME",
        "XDG_CACHE_HOME",
    ] {
        if let Some(v) = env.var(k) {
            cmd.env(k, v);
        }
    }
    Ok(cmd)
}

/// Run a command, return its standard output (at most MAX_RECORD bytes), and
/// stop it after COLLECT_TIMEOUT.
fn run_capped(cmd: &mut Command) -> Result<Vec<u8>> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .context("cannot start the collector sandbox")?;
    let out = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = out.take(MAX_RECORD).read_to_end(&mut buf);
        buf
    });
    let start = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break s;
        }
        if start.elapsed() > COLLECT_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            bail!("the collector did not finish in time");
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let buf = reader.join().unwrap_or_default();
    if !status.success() {
        bail!("the collector failed ({status})");
    }
    Ok(buf)
}

pub fn doctor_lines(env: &HostEnv) -> Vec<String> {
    let s = status(env);
    let mark = |ok: bool| if ok { "  ok      " } else { "  CHECK   " };
    if !s.block && !s.files {
        return vec![
            "  off      the agents panel checks limits on the host, also for a login that a sandbox holds; paddock shims install --scope omarchy-agent changes that"
                .into(),
        ];
    }
    let mut out = vec![
        format!(
            "{} the paddock block in {}",
            mark(s.block && s.files),
            hypr_config(env).display().to_string().replacen(
                &env.home.display().to_string(),
                "~",
                1
            )
        ),
        format!(
            "{} the panel's limit checks go through paddock{}",
            mark(s.active),
            if s.active {
                ""
            } else {
                ": log out and in again to start using it"
            }
        ),
    ];
    let real = real_command(env);
    let text = std::fs::read_to_string(&real).unwrap_or_default();
    out.push(format!(
        "{} {} still takes --except",
        mark(text.contains("--except")),
        real.display()
    ));
    for tool in ["bwrap", "pasta"] {
        if env.which(tool).is_none() {
            out.push(format!(
                "  CHECK    {tool} is missing: a held login is not checked until the sandbox exits"
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(t: &Path) -> HostEnv {
        HostEnv::from_parts(t.join("home"), "demo".into(), 1, 1, BTreeMap::new())
    }

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_arguments_of_omarchy_s_command_are_read_as_it_reads_them() {
        let a = Args::parse(&v(&["--force", "--except", "codex", "claude", "grok"]));
        assert_eq!(a.flags, v(&["--force"]));
        assert!(a.wanted("claude") && a.wanted("grok"));
        assert!(!a.wanted("codex") && !a.wanted("kimi"));
        assert!(Args::parse(&[]).wanted("codex"));
    }

    fn running(id: &str, harness: &str, started: u64, logins: Vec<HeldLogin>) -> Running {
        Running {
            id: id.into(),
            harness: harness.into(),
            project: "/p".into(),
            backend: "bwrap".into(),
            network: "egress".into(),
            gui: "off".into(),
            pid: 1,
            started,
            auth: "shared".into(),
            window_pid: None,
            warnings: vec![],
            host_pid: 1,
            parent: None,
            spawn_id: None,
            logins,
            unit: None,
            kv_project: None,
            pasture: None,
            credentials: Vec::new(),
            gateway: Vec::new(),
            agent: Vec::new(),
            secret_files: Vec::new(),
            kept_logins: Vec::new(),
        }
    }

    fn login(home: &str, store: &str) -> HeldLogin {
        HeldLogin {
            home: home.into(),
            rel: ".claude/.credentials.json".into(),
            store: store.into(),
        }
    }

    #[test]
    fn the_newest_sandbox_holds_a_login_and_isolated_ones_hold_none() {
        let list = vec![
            running("a", "claude", 1, vec![login("/old", "/h/main")]),
            running("b", "claude", 2, vec![login("/new", "/h/main")]),
            running("c", "claude", 3, vec![login("/acct", "/h/work")]),
            running("d", "codex", 4, vec![]),
            running("e", "hermes", 5, vec![login("/x", "/h/x")]),
        ];
        let h = held(&list);
        assert_eq!(h.keys().copied().collect::<Vec<_>>(), vec!["claude"]);
        let homes: Vec<_> = h["claude"].iter().map(|l| l.home.clone()).collect();
        assert_eq!(homes, vec![PathBuf::from("/acct"), PathBuf::from("/new")]);
    }

    const USER_HYPR: &str = "-- Learn how to configure Hyprland\nrequire(\"default.hypr.omarchy\")\n\n-- Add any other personal Hyprland configuration below.\n-- o.window(\"qemu\", { workspace = \"5\" })\n";

    #[test]
    fn install_and_remove_keep_the_user_s_hyprland_config() {
        let t = tempfile::tempdir().unwrap();
        let e = env(t.path());
        // No hyprland.lua: nothing is written.
        assert!(install(&e).is_err());
        assert!(!hook_bin(&e).exists());

        let config = hypr_config(&e);
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(&config, USER_HYPR).unwrap();
        // The file of the first version goes away.
        let old = old_dropin(&e);
        std::fs::create_dir_all(old.parent().unwrap()).unwrap();
        std::fs::write(&old, format!("{MARK}\nexport PATH=x\n")).unwrap();
        let mine = old.parent().unwrap().join("50-mine");
        std::fs::write(&mine, "export A=1\n").unwrap();

        install(&e).unwrap();
        let once = std::fs::read_to_string(&config).unwrap();
        assert!(once.starts_with(USER_HYPR));
        assert!(once.ends_with(&hypr_block(&hypr_file(&e))));
        assert!(!old.exists() && mine.exists());
        install(&e).unwrap();
        assert_eq!(std::fs::read_to_string(&config).unwrap(), once);
        let s = status(&e);
        assert!(s.block && s.files && !s.active);

        remove(&e).unwrap();
        assert_eq!(std::fs::read_to_string(&config).unwrap(), USER_HYPR);
        assert!(!hook_bin(&e).exists());
        assert_eq!(std::fs::read_to_string(&mine).unwrap(), "export A=1\n");
    }

    #[test]
    fn a_config_that_would_not_compile_is_left_alone() {
        let t = tempfile::tempdir().unwrap();
        let mut e = env(t.path());
        e.path = vec![PathBuf::from("/usr/bin")];
        if e.which("luac").is_none() {
            eprintln!("skipped: no luac");
            return;
        }
        let config = hypr_config(&e);
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        // An open long comment: anything appended stays inside it.
        let broken = "x = 1\n--[[ unfinished\n";
        std::fs::write(&config, broken).unwrap();
        assert!(install(&e).is_err());
        assert_eq!(std::fs::read_to_string(&config).unwrap(), broken);
    }

    #[test]
    fn a_config_without_a_final_newline_gets_the_block_on_its_own_lines() {
        let block = hypr_block(Path::new("/h/hypr.lua"));
        let out = insert_block("x = 1", &block);
        assert_eq!(out, format!("x = 1\n\n{block}"));
        assert_eq!(strip_block(&out), "x = 1\n\n");
    }

    /// The Lua file with the system Lua, with a stand-in for Hyprland's `hl`.
    #[test]
    fn the_lua_file_puts_paddock_first_and_omarchy_second() {
        let Some(lua) = ["lua5.4", "lua", "luajit"]
            .iter()
            .map(|l| Path::new("/usr/bin").join(l))
            .find(|p| p.exists())
        else {
            eprintln!("skipped: no lua");
            return;
        };
        let t = tempfile::tempdir().unwrap();
        let f = t.path().join("hypr.lua");
        std::fs::write(
            &f,
            hypr_lua(Path::new("/h/.local/share/paddock/omarchy-usage/bin")),
        )
        .unwrap();
        let run = |path: &str| {
            let out = Command::new(&lua)
                .arg("-e")
                .arg(format!(
                    "hl = {{ env = function(k, v) io.write(k .. '=' .. v) end }} dofile({})",
                    lua_quote(&f.display().to_string())
                ))
                .env("PATH", path)
                .env_remove("OMARCHY_PATH")
                .output()
                .unwrap();
            String::from_utf8(out.stdout).unwrap()
        };
        let want = "PATH=/h/.local/share/paddock/omarchy-usage/bin:/usr/share/omarchy/bin:/usr/bin";
        assert_eq!(run("/usr/bin"), want);
        assert_eq!(
            run("/usr/share/omarchy/bin:/h/.local/share/paddock/omarchy-usage/bin:/usr/bin"),
            want
        );
        // pcall(dofile, ...) of a missing file is no error.
        let out = Command::new(&lua)
            .arg("-e")
            .arg(format!(
                "\n{} print('ok')",
                hypr_block(&t.path().join("gone.lua"))
            ))
            .output()
            .unwrap();
        assert_eq!(String::from_utf8(out.stdout).unwrap(), "ok\n");
    }

    #[test]
    fn a_copy_must_be_a_json_file_and_never_a_link() {
        let t = tempfile::tempdir().unwrap();
        let sb = t.path().join("sb");
        let host = t.path().join("host.json");
        let run = t.path().join("run");
        std::fs::create_dir_all(sb.join(".claude")).unwrap();
        std::fs::create_dir_all(&run).unwrap();
        std::fs::write(&host, "{}").unwrap();
        let secret = t.path().join("id_key");
        std::fs::write(&secret, "{\"private\": 1}").unwrap();
        let l = HeldLogin {
            home: sb.clone(),
            rel: ".claude/.credentials.json".into(),
            store: host.clone(),
        };
        std::os::unix::fs::symlink(&secret, sb.join(".claude/.credentials.json")).unwrap();
        assert!(login_copies(&run, std::slice::from_ref(&l))
            .unwrap()
            .is_empty());
        std::fs::remove_file(sb.join(".claude/.credentials.json")).unwrap();
        std::fs::write(sb.join(".claude/.credentials.json"), "not json").unwrap();
        assert!(login_copies(&run, std::slice::from_ref(&l))
            .unwrap()
            .is_empty());
        std::fs::write(sb.join(".claude/.credentials.json"), "{\"t\":1}").unwrap();
        let c = login_copies(&run, std::slice::from_ref(&l)).unwrap();
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].1, host);
        assert_eq!(std::fs::read_to_string(&c[0].0).unwrap(), "{\"t\":1}");
        // No host file to cover: the login is left out.
        std::fs::remove_file(&host).unwrap();
        assert!(login_copies(&run, &[l]).unwrap().is_empty());
    }

    #[test]
    fn a_held_record_keeps_its_numbers_and_says_why() {
        let t = tempfile::tempdir().unwrap();
        let e = env(t.path());
        mark_held(&e, "codex").unwrap();
        let dir = usage_dir(&e);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("codex.json"),
            r#"{"id":"codex","limits":[{"percent":0.5}],"limitsStale":false}"#,
        )
        .unwrap();
        mark_held(&e, "codex").unwrap();
        let r: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("codex.json")).unwrap())
                .unwrap();
        assert_eq!(r["limits"][0]["percent"], 0.5);
        assert_eq!(r["limitsStale"], true);
        assert_eq!(r["usageStatusText"], "Limits paused");
        assert_eq!(r["authHelpText"], HELD_HELP);
    }
}
