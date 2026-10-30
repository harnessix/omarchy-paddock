//! Make the sandbox the default: typing `claude` in a shell runs
//! `paddock run claude` in the current project.
//!
//! - One script, `shims.sh` in paddock's data directory, defines a shell
//!   function for each command of each installed terminal harness.
//! - One marked line in `~/.bashrc` and `~/.zshrc` (where they exist) sources
//!   that script, only if it is still there.
//!
//! A function, not a PATH entry: version managers such as mise put their own
//! directories first on PATH at every prompt, but a function always wins.
//!
//! Nothing stays behind in a way that can break the shell. Each function runs
//! the real command when paddock is gone, inside a sandbox, or when
//! PADDOCK_BYPASS is set. `paddock shims remove` deletes the script and the
//! marked lines.

use crate::env::HostEnv;
use crate::launchers::{self, Kind};
use crate::profile::HarnessFactory;
use anyhow::Result;
use std::path::{Path, PathBuf};

const MARK: &str = "# paddock shims: `paddock shims remove` takes this line out";

pub fn script_file(env: &HostEnv) -> PathBuf {
    env.paddock_data().join("shims.sh")
}

fn rc_files(env: &HostEnv) -> Vec<PathBuf> {
    [".bashrc", ".zshrc"]
        .iter()
        .map(|f| env.home.join(f))
        .filter(|p| p.is_file())
        .collect()
}

/// The harness and command name of each shim, for installed terminal harnesses.
pub fn detect(env: &HostEnv) -> Result<Vec<(String, String)>> {
    let f = HarnessFactory::new(env)?;
    let mut out = Vec::new();
    for e in launchers::detect(env)? {
        if e.kind != Kind::Cli {
            continue;
        }
        let Some(p) = f.profile(&e.name) else {
            continue;
        };
        // A harness that cannot work in a sandbox keeps its own command.
        if p.launch.wrap_on_host.is_some() {
            continue;
        }
        let commands = if p.locate.commands.is_empty() {
            vec![e.name.clone()]
        } else {
            p.locate.commands.clone()
        };
        for c in commands {
            if is_safe_name(&c) && !out.iter().any(|(_, o)| o == &c) {
                out.push((e.name.clone(), c));
            }
        }
    }
    Ok(out)
}

fn is_safe_name(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('-')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

pub fn script(shims: &[(String, String)], paddock: &Path) -> String {
    let mut s = String::from(
        "# Written by `paddock shims install`; edits here are replaced.\n\
         # Each function runs its harness in a paddock sandbox in the current project.\n\
         # Run the command directly with: PADDOCK_BYPASS=1 <command>\n",
    );
    let exe = sh_quote(&paddock.display().to_string());
    for (harness, cmd) in shims {
        s.push_str(&format!(
            "{cmd}() {{\n  if [ -z \"${{PADDOCK:-}}\" ] && [ -z \"${{PADDOCK_BYPASS:-}}\" ] && [ -x {exe} ]; then\n    {exe} run {harness} -- \"$@\"\n  else\n    command {cmd} \"$@\"\n  fi\n}}\n"
        ));
    }
    s
}

fn rc_line(env: &HostEnv) -> String {
    let f = sh_quote(&script_file(env).display().to_string());
    format!("[ -f {f} ] && . {f}  {MARK}")
}

fn strip_line(text: &str) -> String {
    let mut out: String = text
        .lines()
        .filter(|l| !l.contains(MARK))
        .collect::<Vec<_>>()
        .join("\n");
    if text.ends_with('\n') && !out.is_empty() {
        out.push('\n');
    }
    out
}

pub struct Report {
    pub shims: Vec<(String, String)>,
    pub rc_files: Vec<PathBuf>,
}

/// Write the functions again for the harnesses installed now. The lines in
/// ~/.bashrc and ~/.zshrc stay as they are.
///
/// Without `prune`, a command that has a function now and that detection
/// does not find leaves the script as it is: a background refresh with a
/// smaller PATH must never let a harness start without the sandbox.
pub fn rewrite_script(env: &HostEnv, prune: bool) -> Result<Vec<(String, String)>> {
    let paddock = launchers::launcher_exe(env)?;
    let shims = detect(env)?;
    if !prune {
        let found: Vec<&String> = shims.iter().map(|(_, c)| c).collect();
        if active(env).iter().any(|c| !found.contains(&c)) {
            return Ok(shims);
        }
    }
    let file = script_file(env);
    let text = script(&shims, &paddock);
    if std::fs::read_to_string(&file).ok().as_deref() != Some(text.as_str()) {
        crate::privfs::write(&file, text)?;
    }
    Ok(shims)
}

pub fn install(env: &HostEnv) -> Result<Report> {
    let paddock = launchers::launcher_exe(env)?;
    let shims = detect(env)?;
    let file = script_file(env);
    crate::privfs::write(&file, script(&shims, &paddock))?;
    let line = rc_line(env);
    let rcs = rc_files(env);
    for rc in &rcs {
        let text = std::fs::read_to_string(rc)?;
        let mut new = strip_line(&text);
        if !new.is_empty() && !new.ends_with('\n') {
            new.push('\n');
        }
        new.push_str(&line);
        new.push('\n');
        if new != text {
            std::fs::write(rc, new)?;
        }
    }
    Ok(Report {
        shims,
        rc_files: rcs,
    })
}

pub fn remove(env: &HostEnv) -> Result<Vec<PathBuf>> {
    let mut removed = Vec::new();
    for rc in rc_files(env) {
        let text = std::fs::read_to_string(&rc)?;
        let clean = strip_line(&text);
        if clean != text {
            std::fs::write(&rc, clean)?;
            removed.push(rc);
        }
    }
    let file = script_file(env);
    if file.exists() {
        std::fs::remove_file(&file)?;
        removed.push(file);
    }
    Ok(removed)
}

/// The commands that the installed script takes over, in its order.
pub fn active(env: &HostEnv) -> Vec<String> {
    std::fs::read_to_string(script_file(env))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.strip_suffix("() {"))
        .map(str::to_string)
        .collect()
}

/// The rc files that load the script.
pub fn loaded_from(env: &HostEnv) -> Vec<PathBuf> {
    rc_files(env)
        .into_iter()
        .filter(|rc| {
            std::fs::read_to_string(rc)
                .map(|t| t.contains(MARK))
                .unwrap_or(false)
        })
        .collect()
}

/// Whether the shims are on: the script exists and an rc file sources it.
pub fn installed(env: &HostEnv) -> bool {
    script_file(env).is_file()
        && rc_files(env).iter().any(|rc| {
            std::fs::read_to_string(rc)
                .map(|t| t.contains(MARK))
                .unwrap_or(false)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn env(home: &Path) -> HostEnv {
        HostEnv::from_parts(home.into(), "tester".into(), 1000, 1000, BTreeMap::new())
    }

    #[test]
    fn script_falls_back_to_the_real_command() {
        let s = script(
            &[("claude".into(), "claude".into())],
            Path::new("/opt/pad dock/paddock"),
        );
        assert!(s.contains("claude() {"));
        assert!(s.contains("'/opt/pad dock/paddock' run claude -- \"$@\""));
        assert!(s.contains("command claude \"$@\""));
        assert!(s.contains("PADDOCK_BYPASS"));
    }

    #[test]
    fn unsafe_names_are_skipped() {
        assert!(is_safe_name("gemini-cli"));
        assert!(!is_safe_name("rm -rf"));
        assert!(!is_safe_name("-x"));
        assert!(!is_safe_name("a;b"));
    }

    #[test]
    fn rc_line_is_added_once_and_removed_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let e = env(dir.path());
        let rc = dir.path().join(".bashrc");
        let original = "alias ll='ls -l'\nexport EDITOR=nvim\n";
        std::fs::write(&rc, original).unwrap();
        let line = rc_line(&e);
        for _ in 0..2 {
            let text = std::fs::read_to_string(&rc).unwrap();
            let mut new = strip_line(&text);
            new.push_str(&line);
            new.push('\n');
            std::fs::write(&rc, new).unwrap();
        }
        let text = std::fs::read_to_string(&rc).unwrap();
        assert_eq!(text.matches(MARK).count(), 1);
        remove(&e).unwrap();
        assert_eq!(std::fs::read_to_string(&rc).unwrap(), original);
        assert!(!installed(&e));
    }
}
