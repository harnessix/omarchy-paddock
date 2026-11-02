//! The box for Omarchy's `omarchy-agent-usage-update`.
//!
//! The agents panel counts sessions from the harness folders in the home.
//! Sessions made in a paddock sandbox stay in its sandbox home. paddock runs
//! Omarchy's command, unchanged, in a bwrap box that shows both as one tree:
//!
//! - the system read-only, an empty home (tmpfs), its own `/tmp`, and the
//!   user's runtime folders hidden;
//! - for each harness data folder that a profile names (`[state] auth`,
//!   `[state.account]`, `[state.history]`): a read-only overlay at its place
//!   in the home. The lower layers are the same folder in every sandbox home
//!   of that harness, and the host folder on top. A single file, such as
//!   `.claude.json`, is the host file, read-only;
//! - read-only: Omarchy's tree and the tool folders on PATH when they are in
//!   the home (mise's data folder and config with them, so a collector finds
//!   `codex`), and the panel's own settings in `~/.config/omarchy/agents`;
//! - read-write: only the record and cache folders of the panel;
//! - the network of the host, and only HOME, PATH, OMARCHY_PATH, LANG,
//!   XDG_STATE_HOME and XDG_CACHE_HOME.
//!
//! The rest of the home (`~/.ssh`, documents, other projects) is absent.
//! When the box cannot start, the command does not run on the host either.

use crate::env::HostEnv;
use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The folders that the ticket's collectors read, also without a profile.
const BASE: &[&str] = &[".claude", ".claude.json", ".codex", ".grok", ".gemini"];

/// The longest list of lower layers for one overlay, in bytes. The kernel
/// takes the mount options in one page, and bwrap adds a prefix to each
/// path.
const LOWER_BUDGET: usize = 3000;

/// Where a harness keeps its data, from a path in its profile (relative to
/// HOME): the first part of the path that is not a shared XDG folder.
/// `.codex/auth.json` gives `.codex`, `.local/share/opencode/auth.json`
/// gives `.local/share/opencode`. None for a path that is not in the home.
pub fn data_root(rel: &str) -> Option<String> {
    let rel = rel.trim_end_matches('/');
    if rel.is_empty() || rel.starts_with('/') {
        return None;
    }
    let parts: Vec<&str> = rel.split('/').collect();
    if parts
        .iter()
        .any(|p| p.is_empty() || *p == "." || *p == "..")
    {
        return None;
    }
    let depth = match parts[0] {
        ".config" => 2,
        ".local" if matches!(parts.get(1), Some(&"share") | Some(&"state")) => 3,
        ".local" | ".cache" => return None,
        _ => 1,
    };
    (parts.len() >= depth).then(|| parts[..depth].join("/"))
}

/// The harness data folders, each with the harnesses whose sandbox homes
/// hold the same folder. A folder inside another one is left out: the outer
/// overlay already shows it.
pub fn folders(env: &HostEnv) -> BTreeMap<String, BTreeSet<String>> {
    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    if let Ok(f) = crate::profile::HarnessFactory::new(env) {
        for name in f.names() {
            let Some(p) = f.profile(&name) else { continue };
            let s = &p.state;
            let mut rels: Vec<&str> = s.auth.iter().map(String::as_str).collect();
            if let Some(a) = &s.account {
                rels.push(&a.dir);
                rels.extend(a.also.iter().map(String::as_str));
            }
            if let Some(h) = &s.history {
                rels.extend(h.project_folders.as_deref());
                rels.extend(h.copy_back.iter().map(String::as_str));
            }
            for r in rels.into_iter().filter_map(data_root) {
                out.entry(r).or_default().insert(name.clone());
            }
        }
    }
    for b in BASE {
        out.entry(b.to_string()).or_default();
    }
    let keys: Vec<String> = out.keys().cloned().collect();
    out.retain(|k, _| {
        !keys
            .iter()
            .any(|o| o != k && Path::new(k).starts_with(Path::new(o)))
    });
    out
}

/// One part of the view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mount {
    /// Read-only overlay at `dest`; `lowers` from the bottom to the top.
    Overlay { dest: PathBuf, lowers: Vec<PathBuf> },
    /// A host path, read-only, at the same place.
    Ro(PathBuf),
    /// A host folder, read-write, at the same place.
    Rw(PathBuf),
}

#[derive(Debug, Default)]
pub struct View {
    pub mounts: Vec<Mount>,
    /// Sandbox homes left out because an overlay had too many layers.
    pub left_out: usize,
}

/// A real folder (or file) in a sandbox home: no symlink from the home on.
fn real_in_home(home: &Path, rel: &str, dir: bool) -> Option<PathBuf> {
    let mut p = home.to_path_buf();
    for part in rel.split('/') {
        p.push(part);
        let m = std::fs::symlink_metadata(&p).ok()?;
        if m.file_type().is_symlink() {
            return None;
        }
    }
    let m = std::fs::symlink_metadata(&p).ok()?;
    (if dir { m.is_dir() } else { m.is_file() }).then_some(p)
}

/// The sandbox homes of a harness: `homes/HARNESS/LOGIN/PROJECT`.
fn sandbox_homes(env: &HostEnv, harness: &str) -> Vec<PathBuf> {
    let root = env.paddock_data().join("homes").join(harness);
    let mut out = Vec::new();
    let dirs = |p: &Path| -> Vec<PathBuf> {
        std::fs::read_dir(p)
            .map(|r| {
                r.flatten()
                    .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                    .map(|e| e.path())
                    .collect()
            })
            .unwrap_or_default()
    };
    for login in dirs(&root) {
        out.extend(dirs(&login));
    }
    out
}

fn mtime(p: &Path) -> std::time::SystemTime {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .unwrap_or(std::time::UNIX_EPOCH)
}

/// The panel's record and cache folders, as Omarchy's scripts compute them.
pub fn state_dir(env: &HostEnv) -> PathBuf {
    env.state_dir.join("omarchy").join("agents")
}

pub fn cache_dir(env: &HostEnv) -> PathBuf {
    env.cache_dir.join("omarchy").join("agent-usage")
}

fn omarchy_path(env: &HostEnv) -> PathBuf {
    PathBuf::from(env.var("OMARCHY_PATH").unwrap_or("/usr/share/omarchy"))
}

/// PATH in the box: without mise's wrapper folder and paddock's own hook
/// folder, so nothing in the box comes back to paddock.
pub fn box_path(env: &HostEnv) -> Vec<PathBuf> {
    let skip = [
        crate::omarchy::mise_wrapper_dir(env),
        crate::omarchy_usage::hook_bin(env),
    ];
    crate::omarchy::strip_shims(env, &env.path)
        .into_iter()
        .filter(|d| d.is_absolute() && !skip.contains(d))
        .collect()
}

pub fn view(env: &HostEnv) -> View {
    let home = &env.home;
    let mut v = View::default();
    // Tools first: a harness overlay inside one of them stays on top.
    v.mounts
        .extend(tool_folders(env).into_iter().map(Mount::Ro));
    for (rel, harnesses) in folders(env) {
        let host = home.join(&rel);
        let host_dir = std::fs::symlink_metadata(&host).is_ok_and(|m| m.is_dir());
        let host_file = !host_dir && host.is_file();
        if host_file {
            v.mounts.push(Mount::Ro(host));
            continue;
        }
        let mut theirs: Vec<PathBuf> = harnesses
            .iter()
            .flat_map(|h| sandbox_homes(env, h))
            .filter_map(|h| real_in_home(&h, &rel, true))
            .collect();
        // The newest first, so a full budget leaves out the oldest homes.
        theirs.sort_by_key(|p| std::cmp::Reverse(mtime(p)));
        let cost = |p: &Path| p.as_os_str().len() + 10;
        let mut used = if host_dir { cost(&host) } else { 0 };
        let mut lowers = Vec::new();
        for p in theirs {
            if used + cost(&p) > LOWER_BUDGET {
                v.left_out += 1;
                continue;
            }
            used += cost(&p);
            lowers.push(p);
        }
        lowers.reverse();
        if host_dir {
            lowers.push(host.clone());
        }
        match lowers.len() {
            0 => {}
            1 if lowers[0] == host => v.mounts.push(Mount::Ro(host)),
            _ => v.mounts.push(Mount::Overlay { dest: host, lowers }),
        }
    }
    v.mounts.push(Mount::Rw(state_dir(env)));
    v.mounts.push(Mount::Rw(cache_dir(env)));
    v
}

/// Tools in the home that the collectors run: Omarchy's tree, PATH folders,
/// mise's data and config, and the panel's settings.
fn tool_folders(env: &HostEnv) -> Vec<PathBuf> {
    let home = &env.home;
    let mise_data = env
        .var("MISE_DATA_DIR")
        .filter(|v| v.starts_with('/'))
        .map(PathBuf::from)
        .unwrap_or_else(|| env.data_dir.join("mise"));
    let mise_config = env
        .var("MISE_CONFIG_DIR")
        .filter(|v| v.starts_with('/'))
        .map(PathBuf::from)
        .unwrap_or_else(|| env.config_dir.join("mise"));
    let mut want = vec![
        omarchy_path(env),
        mise_data,
        mise_config,
        env.config_dir.join("omarchy").join("agents"),
    ];
    want.extend(box_path(env));
    let paddock = env.paddock_data();
    let mut out: Vec<PathBuf> = Vec::new();
    for p in want {
        // Never paddock's data, nor a folder that holds it.
        let ours = p.starts_with(&paddock) || paddock.starts_with(&p);
        if !p.starts_with(home) || ours || !p.exists() {
            continue;
        }
        if out.iter().any(|o| p.starts_with(o)) {
            continue;
        }
        out.retain(|o| !o.starts_with(&p));
        out.push(p);
    }
    out
}

/// The bwrap arguments for the home of the view: an empty home, and the
/// mounts of the view over it.
pub fn home_args(env: &HostEnv, view: &View) -> Vec<String> {
    let s = |p: &Path| p.display().to_string();
    let mut a = vec!["--tmpfs".to_string(), s(&env.home)];
    for m in &view.mounts {
        match m {
            Mount::Overlay { dest, lowers } => {
                for l in lowers {
                    a.extend(["--overlay-src".into(), s(l)]);
                }
                a.extend(["--ro-overlay".into(), s(dest)]);
            }
            Mount::Ro(p) => a.extend(["--ro-bind".into(), s(p), s(p)]),
            Mount::Rw(p) => a.extend(["--bind".into(), s(p), s(p)]),
        }
    }
    a
}

/// The bwrap arguments for `program ARGS` in the view.
pub fn bwrap_args(env: &HostEnv, view: &View, program: &Path, args: &[String]) -> Vec<String> {
    let s = |p: &Path| p.display().to_string();
    let mut a: Vec<String> = [
        "--die-with-parent",
        "--new-session",
        "--unshare-user",
        "--unshare-pid",
        "--unshare-ipc",
        "--unshare-uts",
        "--unshare-cgroup-try",
        "--disable-userns",
        "--ro-bind",
        "/",
        "/",
        "--dev",
        "/dev",
        "--proc",
        "/proc",
        "--tmpfs",
        "/tmp",
        "--tmpfs",
        "/var/tmp",
    ]
    .map(String::from)
    .to_vec();
    for d in crate::execbox::session_dirs() {
        a.extend(["--tmpfs".into(), s(&d)]);
    }
    if let Some(rt) = &env.runtime_dir {
        if rt.is_dir() && !a.contains(&s(rt)) {
            a.extend(["--tmpfs".into(), s(rt)]);
        }
    }
    a.extend(home_args(env, view));
    a.extend(["--chdir".into(), s(&env.home), "--".into(), s(program)]);
    a.extend(args.iter().cloned());
    a
}

/// The variables in the box.
pub fn box_env(env: &HostEnv) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    out.insert("HOME".into(), env.home.display().to_string());
    out.insert(
        "PATH".into(),
        std::env::join_paths(box_path(env))
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
    );
    for k in ["OMARCHY_PATH", "LANG", "XDG_STATE_HOME", "XDG_CACHE_HOME"] {
        if let Some(v) = env.var(k) {
            out.insert(k.into(), v.into());
        }
    }
    out
}

/// bwrap, when it can start a box with a read-only overlay here.
pub fn probe(env: &HostEnv) -> Result<PathBuf> {
    let bwrap = env.which("bwrap").context("bwrap is not on PATH")?;
    let quiet = |args: &[&str]| {
        Command::new(&bwrap)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    };
    let base = [
        "--unshare-user",
        "--disable-userns",
        "--ro-bind",
        "/",
        "/",
        "--tmpfs",
        "/tmp",
    ];
    if !quiet(&[&base[..], &["true"]].concat()) {
        bail!(
            "{} cannot start a sandbox here (no user namespace)",
            bwrap.display()
        );
    }
    let overlay = [
        "--overlay-src",
        "/usr",
        "--overlay-src",
        "/etc",
        "--ro-overlay",
        "/tmp/o",
        "true",
    ];
    if !quiet(&[&base[..], &overlay[..]].concat()) {
        bail!(
            "{} cannot mount a read-only overlay here (bwrap 0.10 or newer, and overlayfs in a user namespace)",
            bwrap.display()
        );
    }
    Ok(bwrap)
}

/// The one line for the panel when the box cannot start.
pub fn fail_line(e: &anyhow::Error) -> String {
    format!(
        "paddock: cannot start the box for omarchy-agent-usage-update: {e:#}. It does not run on the host; the panel keeps its last records. Remove the wrapper with: paddock shims remove"
    )
}

/// A line of the box's standard error that says a collector could not
/// write a read-only file.
fn read_only_write(line: &str) -> bool {
    line.contains("Read-only file system") || line.contains("EROFS")
}

pub const READ_ONLY_NOTE: &str = "paddock: a collector could not write into an agent folder (for example to save a refreshed login). The usage box keeps logins read-only; paddock's credential handling refreshes them.";

/// Run Omarchy's command in the box. Standard output and the exit code
/// pass through, standard error too, with one note when a collector tried
/// to write a read-only file. When the box cannot start, the command never
/// runs: one line on standard error, and None.
pub fn run(env: &HostEnv, program: &Path, args: &[String]) -> Result<Option<i32>> {
    let bwrap = match probe(env) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("{}", fail_line(&e));
            return Ok(None);
        }
    };
    for d in [state_dir(env), cache_dir(env)] {
        std::fs::create_dir_all(&d).with_context(|| format!("cannot create {}", d.display()))?;
    }
    let view = view(env);
    if view.left_out > 0 {
        eprintln!(
            "paddock: {} sandbox homes are left out of the usage view (too many layers); the panel counts the newest ones",
            view.left_out
        );
    }
    let mut child = Command::new(&bwrap)
        .args(bwrap_args(env, &view, program, args))
        .env_clear()
        .envs(box_env(env))
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("cannot run {}", bwrap.display()))?;
    let err = child.stderr.take().context("no standard error")?;
    let relay = std::thread::spawn(move || {
        let mut noted = false;
        let mut r = std::io::BufReader::new(err);
        let mut line = Vec::new();
        let mut out = std::io::stderr();
        while r.read_until(b'\n', &mut line).is_ok_and(|n| n > 0) {
            let _ = out.write_all(&line);
            if !noted && read_only_write(&String::from_utf8_lossy(&line)) {
                noted = true;
                let _ = writeln!(out, "{READ_ONLY_NOTE}");
            }
            line.clear();
        }
    });
    let status = child.wait()?;
    let _ = relay.join();
    use std::os::unix::process::ExitStatusExt;
    Ok(Some(
        status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(1)),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_roots_skip_shared_xdg_folders() {
        assert_eq!(data_root(".codex/auth.json").as_deref(), Some(".codex"));
        assert_eq!(data_root(".claude.json").as_deref(), Some(".claude.json"));
        assert_eq!(
            data_root(".local/share/opencode/auth.json").as_deref(),
            Some(".local/share/opencode")
        );
        assert_eq!(
            data_root(".config/cursor/auth.json").as_deref(),
            Some(".config/cursor")
        );
        assert_eq!(
            data_root(".kimi-code/credentials/").as_deref(),
            Some(".kimi-code")
        );
        assert_eq!(
            data_root(".gemini/antigravity-cli/x").as_deref(),
            Some(".gemini")
        );
        for bad in [
            "",
            "/etc/passwd",
            "../x",
            ".local/bin/x",
            ".cache/x",
            ".config",
        ] {
            assert_eq!(data_root(bad), None, "{bad}");
        }
    }

    fn env(t: &Path) -> HostEnv {
        let mut vars = BTreeMap::new();
        vars.insert("PATH".into(), "/usr/bin".into());
        HostEnv::from_parts(t.join("home"), "demo".into(), 1, 1, vars)
    }

    #[test]
    fn the_view_merges_host_and_sandbox_homes_and_nothing_else() {
        let t = tempfile::tempdir().unwrap();
        let e = env(t.path());
        let home = &e.home;
        std::fs::create_dir_all(home.join(".codex/sessions")).unwrap();
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::write(home.join(".claude.json"), "{}").unwrap();
        let homes = e.paddock_data().join("homes/codex");
        let a = homes.join("shared/proj-a/.codex");
        let b = homes.join("shared/proj-b/.codex");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        // A link that a sandbox planted is never a layer.
        let c = homes.join("shared/proj-c");
        std::fs::create_dir_all(&c).unwrap();
        std::os::unix::fs::symlink(home.join(".ssh"), c.join(".codex")).unwrap();
        let v = view(&e);
        let codex = v
            .mounts
            .iter()
            .find_map(|m| match m {
                Mount::Overlay { dest, lowers } if dest.ends_with(".codex") => Some(lowers),
                _ => None,
            })
            .expect("no overlay for .codex");
        assert_eq!(codex.len(), 3, "{codex:?}");
        assert_eq!(
            codex.last(),
            Some(&home.join(".codex")),
            "the host is on top"
        );
        assert!(codex.contains(&a) && codex.contains(&b));
        assert!(v.mounts.contains(&Mount::Ro(home.join(".claude.json"))));
        let args = bwrap_args(&e, &v, Path::new("/x/update"), &["--force".into()]);
        let joined = args.join(" ");
        assert!(!joined.contains(".ssh"), "{joined}");
        assert!(joined.contains(&format!("--tmpfs {}", home.display())));
        assert!(joined.contains(&format!("--bind {0} {0}", state_dir(&e).display())));
        assert!(joined.ends_with("-- /x/update --force"));
        // Only the bind of the record and cache folders is writable.
        let rw: Vec<_> = args
            .windows(2)
            .filter(|w| w[0] == "--bind")
            .map(|w| w[1].clone())
            .collect();
        assert_eq!(
            rw,
            vec![
                state_dir(&e).display().to_string(),
                cache_dir(&e).display().to_string()
            ]
        );
    }

    #[test]
    fn many_sandbox_homes_stay_within_the_mount_option_limit() {
        let t = tempfile::tempdir().unwrap();
        let e = env(t.path());
        let homes = e.paddock_data().join("homes/grok/shared");
        for n in 0..200 {
            std::fs::create_dir_all(homes.join(format!("project-{n:04}/.grok"))).unwrap();
        }
        let v = view(&e);
        let lowers = v
            .mounts
            .iter()
            .find_map(|m| match m {
                Mount::Overlay { dest, lowers } if dest.ends_with(".grok") => Some(lowers),
                _ => None,
            })
            .unwrap();
        let size: usize = lowers.iter().map(|p| p.as_os_str().len() + 10).sum();
        assert!(size <= LOWER_BUDGET);
        assert_eq!(lowers.len() + v.left_out, 200);
    }

    #[test]
    fn only_the_listed_variables_pass() {
        let t = tempfile::tempdir().unwrap();
        let mut vars = BTreeMap::new();
        for (k, v) in [
            ("PATH", "/usr/bin"),
            ("LANG", "C"),
            ("OMARCHY_PATH", "/o"),
            ("SSH_AUTH_SOCK", "/s"),
            ("OPENAI_API_KEY", "k"),
        ] {
            vars.insert(k.to_string(), v.to_string());
        }
        let e = HostEnv::from_parts(t.path().join("h"), "demo".into(), 1, 1, vars);
        let keys: Vec<String> = box_env(&e).into_keys().collect();
        assert_eq!(keys, ["HOME", "LANG", "OMARCHY_PATH", "PATH"]);
    }

    #[test]
    fn without_bwrap_the_box_fails_closed() {
        let t = tempfile::tempdir().unwrap();
        let e = env(t.path());
        let mut e = e;
        e.path = vec![t.path().join("nothing")];
        let err = probe(&e).unwrap_err();
        let line = fail_line(&err);
        assert!(line.contains("bwrap is not on PATH"), "{line}");
        assert!(line.contains("paddock shims remove"));
        assert!(!line.contains('\n'));
    }
}
