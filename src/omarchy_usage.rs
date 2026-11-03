//! The session hook and the keeper lock for the Omarchy agents panel.
//!
//! The panel runs `omarchy-agent-usage-update` by name, without a shell.
//! Paddock runs that command as a whole inside the usage box (`usage_box.rs`),
//! where every collector sees the host and sandbox sessions as one read-only
//! tree and the newest copy of each login file.
//!
//! Two parts stay here:
//!
//! - The Hyprland session hook. Hyprland puts `$OMARCHY_PATH/bin` ahead of
//!   mise's wrappers on the session `PATH`, and the panel runs
//!   `omarchy-agent-usage-update` directly without a shell, so the mise
//!   wrapper of `omarchy.rs` alone never sees the panel's runs. One marked
//!   block at the end of `~/.config/hypr/hyprland.lua` loads a Lua file of
//!   paddock that puts `~/.local/share/paddock/omarchy-usage/bin` first on the
//!   session `PATH`, ahead of `$OMARCHY_PATH/bin`. That folder holds
//!   `omarchy-agent-usage-update`, which calls
//!   `paddock shims exec omarchy-agent-usage-update`.
//! - Kept logins. Omarchy's Codex collector starts `codex app-server`,
//!   which writes its state next to the login and can refresh the login.
//!   A login that the credential keeper (`keeper.rs`) keeps is therefore
//!   left out of the box (`--except AGENT`) and checked by
//!   `keeper::collect_usage` instead: in the keeper's sandbox, with the
//!   keeper's login and under its refresh lock, with writes in the keeper's
//!   own folder. This is the path that worked before the usage box.
//!
//! paddock checks only when the panel asks. It never adds a timer.

use crate::env::HostEnv;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// The command that the panel runs.
pub const COMMAND: &str = "omarchy-agent-usage-update";
const MARK: &str = "# Written by paddock (paddock shims remove deletes it).";

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
         -- paddock's omarchy-agent-usage-update goes before Omarchy's on PATH, so\n\
         -- the panel's usage update runs inside paddock.\n\
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
         if [ -x \"$p\" ]; then exec \"$p\" shims exec {COMMAND} \"$@\"; fi\n\
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

/// `paddock omarchy-usage-update -- ARGS`.
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

/// A panel agent whose login the credential keeper keeps: its collector
/// runs through `keeper::collect_usage`, not in the usage box.
#[derive(Debug, PartialEq, Eq)]
pub struct Kept {
    pub agent: &'static str,
    pub profile: &'static str,
    pub store: PathBuf,
}

/// The kept agents that this update checks. The store is the host login
/// when it holds a refresh token (the keeper pushes refreshes there), else
/// the newest copy of the sandbox homes.
pub fn kept(env: &HostEnv, args: &Args) -> Vec<Kept> {
    let mut out = Vec::new();
    for c in crate::usage_box::collectors::COLLECTORS {
        let Some(profile) = c.profile else { continue };
        let Some(kind) = crate::keeper::kind_for(profile) else {
            continue;
        };
        if !args.wanted(c.id) {
            continue;
        }
        let keepable = |p: &Path| std::fs::read(p).is_ok_and(|b| crate::keeper::keepable(&b));
        let host = env.home.join(kind.file);
        let store = if keepable(&host) {
            Some(host)
        } else {
            let set = std::iter::once(profile.to_string()).collect();
            crate::usage_box::newest_login(env, kind.file, &set, None)
                .filter(|p| !p.starts_with(env.paddock_state().join("keepers")) && keepable(p))
        };
        if let Some(store) = store {
            out.push(Kept {
                agent: c.id,
                profile,
                store,
            });
        }
    }
    out
}

/// The arguments for the box: the panel's own, and `--except` for each
/// kept agent.
pub fn box_args(raw: &[String], kept: &[Kept]) -> Vec<String> {
    let mut a = raw.to_vec();
    for k in kept {
        a.extend(["--except".to_string(), k.agent.to_string()]);
    }
    a
}

pub fn update(env: &HostEnv, raw: &[String]) -> Result<i32> {
    let real = real_command(env);
    let args = Args::parse(raw);
    let kept = kept(env, &args);
    // Omarchy's command runs in the usage box (usage_box.rs), never on the
    // host. When the box cannot start, the panel keeps its last records.
    let Some(mut status) = crate::usage_box::run(env, &real, &box_args(raw, &kept))? else {
        return Ok(1);
    };
    for k in &kept {
        if let Err(e) = collect_kept(env, k, &args.flags) {
            log::warn!("{}: the limits stay as they were: {e:#}", k.agent);
            status = status.max(1);
        }
    }
    Ok(status)
}

fn collect_kept(env: &HostEnv, k: &Kept, flags: &[String]) -> Result<()> {
    let collector = omarchy_path(env)
        .join("bin")
        .join(format!("omarchy-agent-usage-{}", k.agent));
    if !crate::env::is_executable(&collector) {
        bail!("{} is missing", collector.display());
    }
    let record = crate::keeper::collect_usage(env, k.profile, &k.store, &collector, flags)?;
    let v: serde_json::Value =
        serde_json::from_slice(&record).context("the collector did not print JSON")?;
    if v.get("id").and_then(|i| i.as_str()) != Some(k.agent) {
        bail!("the collector printed a record for another agent");
    }
    write_record(env, k.agent, &v)
}

fn write_record(env: &HostEnv, agent: &str, record: &serde_json::Value) -> Result<()> {
    let dir = env.state_dir.join("omarchy").join("agents").join("usage");
    std::fs::create_dir_all(&dir)?;
    let tmp = dir.join(format!(".{agent}.paddock-{}", std::process::id()));
    std::fs::write(&tmp, format!("{record}\n"))?;
    std::fs::rename(&tmp, dir.join(format!("{agent}.json")))?;
    Ok(())
}

pub fn doctor_lines(env: &HostEnv) -> Vec<String> {
    let s = status(env);
    let mark = |ok: bool| if ok { "  ok      " } else { "  CHECK   " };
    if !s.block && !s.files {
        return vec![
            "  off      the agents panel runs usage updates on the host; paddock shims install --scope omarchy-agent moves them into the usage box"
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
    if env.which("bwrap").is_none() {
        out.push(
            "  CHECK    bwrap is missing: the usage box cannot start, so the panel keeps its last records"
                .into(),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn sv(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_panel_arguments_keep_flags_and_name_agents() {
        let a = Args::parse(&sv(&["--force", "--except", "grok", "claude"]));
        assert_eq!(a.flags, sv(&["--force"]));
        assert!(a.wanted("claude") && !a.wanted("grok") && !a.wanted("codex"));
        assert!(Args::parse(&[]).wanted("codex"));
    }

    /// A Codex login with a refresh token goes to the keeper path and out
    /// of the box. An API-key login, or none, stays in the box.
    #[test]
    fn a_kept_codex_login_leaves_the_box_for_the_keeper() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("home");
        let env = HostEnv::from_parts(home.clone(), "demo".into(), 1, 1, Default::default());
        assert!(kept(&env, &Args::default()).is_empty());
        let auth = home.join(".codex/auth.json");
        std::fs::create_dir_all(auth.parent().unwrap()).unwrap();
        std::fs::write(&auth, r#"{"OPENAI_API_KEY":"sk-test"}"#).unwrap();
        assert!(kept(&env, &Args::default()).is_empty());
        std::fs::write(
            &auth,
            r#"{"tokens":{"access_token":"a","refresh_token":"r"}}"#,
        )
        .unwrap();
        let k = kept(&env, &Args::default());
        assert_eq!(
            k,
            vec![Kept {
                agent: "codex",
                profile: "codex",
                store: auth.clone()
            }]
        );
        assert_eq!(
            box_args(&sv(&["--force"]), &k),
            sv(&["--force", "--except", "codex"])
        );
        // The panel asked for another agent only: Codex is not checked.
        assert!(kept(&env, &Args::parse(&sv(&["claude"]))).is_empty());
    }

    fn env(t: &Path) -> HostEnv {
        HostEnv::from_parts(t.join("home"), "demo".into(), 1, 1, BTreeMap::new())
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
}
