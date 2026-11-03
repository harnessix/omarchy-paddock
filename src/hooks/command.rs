//! The program of a hook command, and whether it resolves inside a sandbox.
//!
//! A harness runs a hook command with a shell. paddock reads the first
//! simple command of the line: assignments (`A=1`) and the wrappers `env`,
//! `exec`, `command` and `nohup` are skipped, and the next word is the
//! program. A shell builtin and a shell with `-c` count as resolved. An
//! interpreter with a script path (`bash ~/hooks/x.sh`) needs both.

use crate::spec::{MountKind, SandboxSpec, TOOL_DIR};
use std::path::{Component, Path, PathBuf};

/// Tools that a start adds to `/run/paddock/bin` after the build
/// (run/host_tools.rs).
pub const RUNTIME_TOOLS: &[&str] = &["paddock-host"];

const BUILTINS: &[&str] = &[
    ":", "true", "false", "echo", "printf", "test", "[", "exit", "cd", "export", "read", "set",
    "unset", "return", "shift", "trap", "wait", "type", "pwd", "umask", "eval", ".", "source",
];

const SHELLS: &[&str] = &["sh", "bash", "zsh", "dash", "ksh", "fish"];

const INTERPRETERS: &[&str] = &[
    "python", "python3", "node", "perl", "ruby", "bun", "deno", "php", "lua",
];

/// Variables that a harness sets to the project folder.
const PROJECT_VARS: &[&str] = &[
    "CLAUDE_PROJECT_DIR",
    "GEMINI_PROJECT_DIR",
    "QWEN_PROJECT_DIR",
    "PWD",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parsed {
    /// A shell builtin, or a line with no command.
    Builtin,
    /// A shell with `-c`: the line inside is the shell's business.
    ShellC,
    /// A program, and the script that an interpreter runs.
    Program {
        program: String,
        script: Option<String>,
    },
}

/// The words of the first simple command, unquoted. None: the line starts
/// with something that is not a plain word (a subshell, a substitution).
fn words(line: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut have = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                have = true;
                for c in chars.by_ref() {
                    if c == '\'' {
                        break;
                    }
                    cur.push(c);
                }
            }
            '"' => {
                have = true;
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' if matches!(chars.peek(), Some('"' | '\\' | '$' | '`')) => {
                            cur.push(chars.next().unwrap_or_default())
                        }
                        // A substitution: the word is not known before the run.
                        '`' => return None,
                        '$' if chars.peek() == Some(&'(') => return None,
                        c => cur.push(c),
                    }
                }
            }
            '\\' => {
                have = true;
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            '`' => return None,
            '$' if chars.peek() == Some(&'(') => return None,
            ';' | '&' | '|' | '<' | '>' | '\n' => break,
            '(' | ')' | '{' | '}' if !have && out.is_empty() => return None,
            c if c.is_whitespace() => {
                if have {
                    out.push(std::mem::take(&mut cur));
                    have = false;
                }
            }
            c => {
                have = true;
                cur.push(c);
            }
        }
    }
    if have {
        out.push(cur);
    }
    Some(out)
}

fn is_assignment(w: &str) -> bool {
    w.split_once('=').is_some_and(|(k, _)| {
        !k.is_empty()
            && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && !k.starts_with(|c: char| c.is_ascii_digit())
    })
}

/// The file name of a program word.
pub fn base(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

pub fn parse(line: &str) -> Parsed {
    let Some(w) = words(line) else {
        return Parsed::ShellC;
    };
    let mut i = 0;
    while i < w.len() && is_assignment(&w[i]) {
        i += 1;
    }
    while let Some(word) = w.get(i) {
        match word.as_str() {
            "env" => {
                i += 1;
                while w
                    .get(i)
                    .is_some_and(|a| a.starts_with('-') || is_assignment(a))
                {
                    i += 1;
                }
            }
            "exec" | "command" | "nohup" => {
                i += 1;
                while w.get(i).is_some_and(|a| a.starts_with('-')) {
                    i += 1;
                }
            }
            _ => break,
        }
    }
    let Some(program) = w.get(i) else {
        return Parsed::Builtin;
    };
    if BUILTINS.contains(&program.as_str()) {
        return Parsed::Builtin;
    }
    let name = base(program);
    let args = &w[i + 1..];
    let first = args.iter().position(|a| !a.starts_with('-'));
    let flags = &args[..first.unwrap_or(args.len())];
    if SHELLS.contains(&name)
        && flags
            .iter()
            .any(|f| !f.starts_with("--") && f.contains('c'))
    {
        return Parsed::ShellC;
    }
    let interp = SHELLS.contains(&name)
        || INTERPRETERS.contains(&name)
        || (name.starts_with("python3.") && name[8..].chars().all(|c| c.is_ascii_digit()));
    let script = first
        .map(|f| args[f].clone())
        .filter(|s| interp && (s.contains('/') || s.starts_with('~')));
    Parsed::Program {
        program: program.clone(),
        script,
    }
}

/// Where a word leads inside the sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Found {
    /// The path inside the sandbox.
    Yes(PathBuf),
    No,
    /// The word depends on something that only the run knows.
    Unchecked(String),
}

/// The sandbox path of a word with a slash, `~` or a variable. None: a bare
/// name, which the PATH resolves. Err: a variable paddock does not know.
fn sandbox_path(spec: &SandboxSpec, word: &str) -> Result<Option<PathBuf>, String> {
    let mut w = word.to_string();
    if let Some(rest) = w.strip_prefix("~/") {
        w = format!("{}/{rest}", spec.home.display());
    } else if w == "~" {
        w = spec.home.display().to_string();
    }
    let project = spec.project.display().to_string();
    let home = spec.home.display().to_string();
    for (var, val) in std::iter::once(("HOME", home.as_str()))
        .chain(PROJECT_VARS.iter().map(|v| (*v, project.as_str())))
    {
        w = w
            .replace(&format!("${{{var}}}"), val)
            .replace(&format!("${var}"), val);
    }
    if w.contains('$') || w.starts_with('~') {
        return Err(format!(
            "not checked: {word} depends on the environment of the run"
        ));
    }
    if !w.contains('/') {
        return Ok(None);
    }
    let p = Path::new(&w);
    if p.is_absolute() {
        return Ok(Some(normal(p)));
    }
    // A harness runs a hook in the project folder.
    Ok(Some(normal(&spec.project.join(p))))
}

/// The path without `.` and `..`, as the sandbox sees it.
fn normal(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            c => out.push(c),
        }
    }
    out
}

/// What a sandbox path is on the host side.
enum HostSide {
    File(PathBuf),
    /// A tool that the start adds.
    Runtime,
}

fn host_side(spec: &SandboxSpec, p: &Path) -> Option<HostSide> {
    if let Ok(rest) = p.strip_prefix(TOOL_DIR) {
        let name = rest.to_str()?;
        if RUNTIME_TOOLS.contains(&name) {
            return Some(HostSide::Runtime);
        }
        return spec
            .host_tools
            .iter()
            .find(|t| t.name == name)
            .map(|t| HostSide::File(t.src.clone()));
    }
    let m = spec
        .mounts
        .iter()
        .filter(|m| p.starts_with(&m.dst))
        .max_by_key(|m| m.dst.components().count());
    if let Some(m) = m {
        let rel = p.strip_prefix(&m.dst).ok()?;
        // `join("")` would add a slash.
        let join = |base: &Path| {
            if rel.as_os_str().is_empty() {
                base.to_path_buf()
            } else {
                base.join(rel)
            }
        };
        return match m.kind {
            MountKind::Ro | MountKind::Rw => m.src.as_deref().map(|s| HostSide::File(join(s))),
            MountKind::Overlay => {
                let upper = m.src.as_deref().map(join);
                let lower = m.layers.as_ref().map(|l| join(&l.lower));
                upper.filter(|u| u.exists()).or(lower).map(HostSide::File)
            }
            MountKind::Tmpfs | MountKind::MaskFile | MountKind::MaskDir => None,
        };
    }
    if let Ok(rel) = p.strip_prefix(&spec.home) {
        return Some(HostSide::File(spec.home_src.join(rel)));
    }
    let s = p.to_string_lossy();
    (crate::profile::is_system_path(p) || s.starts_with("/etc/")).then(|| HostSide::File(p.into()))
}

fn visible(spec: &SandboxSpec, p: &Path, exec: bool) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match host_side(spec, p) {
        Some(HostSide::Runtime) => true,
        Some(HostSide::File(h)) => std::fs::metadata(&h)
            .is_ok_and(|m| m.is_file() && (!exec || m.permissions().mode() & 0o111 != 0)),
        None => false,
    }
}

/// Find a program as the sandbox would: a path, or a name on its PATH.
pub fn find_program(spec: &SandboxSpec, word: &str) -> Found {
    match sandbox_path(spec, word) {
        Err(why) => Found::Unchecked(why),
        Ok(Some(p)) => {
            if visible(spec, &p, true) {
                Found::Yes(p)
            } else {
                Found::No
            }
        }
        Ok(None) => spec
            .path_value()
            .split(':')
            .filter(|d| d.starts_with('/'))
            .map(|d| Path::new(d).join(word))
            .find(|p| visible(spec, p, true))
            .map_or(Found::No, Found::Yes),
    }
}

/// Find a script file that an interpreter reads.
pub fn find_file(spec: &SandboxSpec, word: &str) -> Found {
    match sandbox_path(spec, word) {
        Err(why) => Found::Unchecked(why),
        Ok(Some(p)) if visible(spec, &p, false) => Found::Yes(p),
        _ => Found::No,
    }
}

/// The host path a configuration entry names: `~/` is the home.
pub fn expand(home: &Path, raw: &str) -> PathBuf {
    match raw.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None if raw == "~" => home.to_path_buf(),
        None => PathBuf::from(raw),
    }
}

/// Whether a `[hooks] ignore` entry matches a program word: by file name,
/// or by path when the entry has a slash (`~/` expands).
pub fn matches(home: &Path, rule: &str, word: &str) -> bool {
    if rule.contains('/') || rule.starts_with('~') {
        let w = word
            .replace("${HOME}", &home.display().to_string())
            .replace("$HOME", &home.display().to_string());
        normal(&expand(home, rule)) == normal(&expand(home, &w))
    } else {
        base(word) == rule
    }
}
