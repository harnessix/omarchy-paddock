//! Named host tools: fixed host commands that the user defines once.
//!
//! ```toml
//! [host.tools.compose]
//! command = ["docker", "compose"]   # fixed; the sandbox cannot change it
//! args = ["up -d", "down", "logs *"] # the allowed argument shapes
//! cwd = "project"                    # project (default) or home
//! mode = "ask"                       # allow | ask (default) | deny
//! ```
//!
//! The sandbox runs `paddock-host tool compose up -d`. The arguments must
//! match one shape word by word: `*` is exactly one word that does not start
//! with `-`, and `**` (only as the last word) is zero or more such words. No
//! shape means no arguments. A request that matches no shape is refused
//! before anyone is asked.
//!
//! Tools come only from the user configuration. A project `paddock.toml`
//! cannot define them. The broker checks a request before it asks, and the
//! executor (`paddock gatewayd`) reads the configuration again and checks
//! once more before it runs the command through the same isolated path as
//! `exec`.

use crate::broker::Mode;
use crate::env::HostEnv;
use anyhow::{bail, Context as _, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

/// The most arguments that a tool request can carry.
pub const MAX_ARGS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Cwd {
    Project,
    Home,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Tool {
    pub name: String,
    pub command: Vec<String>,
    /// The allowed argument shapes, as the user wrote them.
    pub args: Vec<String>,
    pub cwd: Cwd,
    pub mode: Mode,
}

/// A valid tool name: lowercase letters, digits, `-` and `_`.
pub fn valid_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 64
        && n.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// Whether `args` match `shape`, word by word.
pub fn matches(shape: &str, args: &[String]) -> bool {
    let words: Vec<&str> = shape.split_whitespace().collect();
    let free = |a: &String| !a.is_empty() && !a.starts_with('-');
    for (i, w) in words.iter().enumerate() {
        if *w == "**" {
            return args.get(i..).is_some_and(|rest| rest.iter().all(free));
        }
        let Some(a) = args.get(i) else {
            return false;
        };
        let ok = if *w == "*" { free(a) } else { a == w };
        if !ok {
            return false;
        }
    }
    args.len() == words.len()
}

impl Tool {
    /// Refuse arguments that match no shape.
    pub fn check(&self, args: &[String]) -> Result<()> {
        if args.len() > MAX_ARGS {
            bail!("tool {} takes at most {MAX_ARGS} arguments", self.name);
        }
        if args.is_empty() && self.args.is_empty() {
            return Ok(());
        }
        if self.args.iter().any(|s| matches(s, args)) {
            return Ok(());
        }
        if self.args.is_empty() {
            bail!("tool {} takes no arguments", self.name);
        }
        bail!(
            "the arguments match no shape of tool {}. Allowed: {}",
            self.name,
            self.shapes_text()
        )
    }

    /// The shapes for a list, `(no arguments)` when there is none.
    pub fn shapes_text(&self) -> String {
        if self.args.is_empty() {
            "(no arguments)".into()
        } else {
            self.args
                .iter()
                .map(|s| format!("\"{s}\""))
                .collect::<Vec<_>>()
                .join(", ")
        }
    }

    /// For `paddock-host tool --list` and `capabilities`.
    pub fn listing(&self) -> serde_json::Value {
        serde_json::json!({
            "name": self.name,
            "command": self.command,
            "args": self.args,
            "cwd": self.cwd,
            "mode": self.mode,
        })
    }
}

fn parse_tool(name: &str, v: &toml::Value) -> Result<Tool> {
    if !valid_name(name) {
        bail!("tool name {name:?}: use lowercase letters, digits, - and _");
    }
    let t = v.as_table().context("must be a table")?;
    let strings = |k: &str| -> Result<Vec<String>> {
        let Some(v) = t.get(k) else {
            return Ok(Vec::new());
        };
        v.as_array()
            .with_context(|| format!("{k} must be a list of strings"))?
            .iter()
            .map(|x| {
                x.as_str()
                    .map(str::to_string)
                    .with_context(|| format!("{k} must be a list of strings"))
            })
            .collect()
    };
    let mut tool = Tool {
        name: name.to_string(),
        command: Vec::new(),
        args: Vec::new(),
        cwd: Cwd::Project,
        mode: Mode::Ask,
    };
    for (k, v) in t {
        match k.as_str() {
            "command" => tool.command = strings("command")?,
            "args" => tool.args = strings("args")?,
            "cwd" => {
                tool.cwd = match v.as_str() {
                    Some("project") => Cwd::Project,
                    Some("home") => Cwd::Home,
                    _ => bail!("cwd must be project or home"),
                }
            }
            "mode" => {
                tool.mode = v
                    .clone()
                    .try_into()
                    .context("mode must be allow, ask or deny")?
            }
            other => bail!("unknown key {other}. Known: command, args, cwd, mode"),
        }
    }
    match tool.command.first() {
        None => bail!("command is missing or empty"),
        Some(p) if p.trim().is_empty() || p.chars().any(char::is_whitespace) => {
            bail!("command: the first word is the program, without spaces")
        }
        Some(p) if p.contains('/') && !p.starts_with('/') => {
            bail!("command: the program is a name on PATH or an absolute path")
        }
        _ => {}
    }
    for s in &tool.args {
        let words: Vec<&str> = s.split_whitespace().collect();
        if let Some(i) = words.iter().position(|w| *w == "**") {
            if i + 1 != words.len() {
                bail!("args {s:?}: ** is allowed only as the last word");
            }
        }
        if words
            .iter()
            .any(|w| w.contains('*') && *w != "*" && *w != "**")
        {
            bail!("args {s:?}: * and ** stand alone as whole words");
        }
    }
    Ok(tool)
}

/// The tools of a `[host]` table of the user configuration.
pub fn parse_host(host: &toml::value::Table) -> Result<BTreeMap<String, Tool>> {
    let Some(v) = host.get("tools") else {
        return Ok(BTreeMap::new());
    };
    let t = v
        .as_table()
        .context("[host.tools] must hold one table per tool")?;
    t.iter()
        .map(|(n, v)| {
            parse_tool(n, v)
                .with_context(|| format!("[host.tools.{n}]"))
                .map(|tool| (n.clone(), tool))
        })
        .collect()
}

/// The tools of the user configuration, never of a project file.
pub fn load(env: &HostEnv) -> Result<BTreeMap<String, Tool>> {
    let file = env.paddock_config().join("config.toml");
    let text = match std::fs::read_to_string(&file) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", file.display())),
    };
    let doc: toml::Value =
        toml::from_str(&text).with_context(|| format!("{} is invalid", file.display()))?;
    match doc.get("host").and_then(|h| h.as_table()) {
        Some(h) => parse_host(h).with_context(|| format!("{} is invalid", file.display())),
        None => Ok(BTreeMap::new()),
    }
}

/// The program, the fixed words and the arguments, for the executor. The
/// program is found on the executor's own PATH, never the sandbox's.
pub fn argv(env: &HostEnv, tool: &Tool, args: &[String]) -> Result<Vec<String>> {
    let exe: PathBuf = env
        .which(&tool.command[0])
        .with_context(|| format!("{} is not installed on the host", tool.command[0]))?;
    let mut v = vec![exe.display().to_string()];
    v.extend(tool.command[1..].iter().cloned());
    v.extend(args.iter().cloned());
    Ok(v)
}

/// An exec command line that the audit log can keep for `paddock tools
/// suggest`: only plain words, so a token or a password stays out. A word
/// with a space, a quote, `=` or a long run of letters and digits (a key or a
/// token) keeps the whole line out.
pub fn loggable_argv(argv: &[String]) -> Option<Vec<String>> {
    if argv.is_empty() || argv.len() > 16 {
        return None;
    }
    let plain = |w: &String| {
        !w.is_empty()
            && w.len() <= 48
            && w.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_./:,@%+".contains(c))
            && !looks_secret(w)
    };
    let flag_names_secret = |w: &String| {
        let l = w.to_ascii_lowercase();
        w.starts_with('-')
            && ["token", "pass", "secret", "key", "auth", "cred"]
                .iter()
                .any(|s| l.contains(s))
    };
    (argv.iter().all(plain) && !argv.iter().any(flag_names_secret)).then(|| argv.to_vec())
}

/// A run of 20 or more letters and digits that mixes both, like an API key.
fn looks_secret(w: &str) -> bool {
    w.split(|c: char| !c.is_ascii_alphanumeric()).any(|run| {
        run.len() >= 20
            && run.chars().any(|c| c.is_ascii_digit())
            && run.chars().any(|c| c.is_ascii_alphabetic())
    })
}

/// How many allowed requests make a suggestion.
pub const SUGGEST_MIN: usize = 3;

/// `[host.tools.NAME]` blocks for the exec command lines that the user
/// allowed at least `SUGGEST_MIN` times, from the text of the audit log.
/// Tools that exist already (`known`, by command) are left out.
pub fn suggest(audit: &str, known: &BTreeMap<String, Tool>) -> String {
    let mut counts: BTreeMap<Vec<String>, usize> = BTreeMap::new();
    for line in audit.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let decision = v["decision"].as_str().unwrap_or("");
        if v["capability"] != "exec"
            || !decision.starts_with("allowed")
            || decision.contains("agent approver")
        {
            continue;
        }
        let Some(argv) = v["argv"].as_array() else {
            continue;
        };
        let argv: Vec<String> = argv
            .iter()
            .filter_map(|a| a.as_str().map(str::to_string))
            .collect();
        if !argv.is_empty() {
            *counts.entry(argv).or_default() += 1;
        }
    }
    let mut names: BTreeSet<String> = known.keys().cloned().collect();
    let mut out = String::new();
    let mut ranked: Vec<_> = counts
        .into_iter()
        .filter(|(a, n)| {
            *n >= SUGGEST_MIN && !known.values().any(|t| t.command == *a && t.args.is_empty())
        })
        .collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    for (argv, n) in ranked {
        let name = unique_name(&argv, &mut names);
        let cmd = argv
            .iter()
            .map(|a| toml::Value::String(a.clone()).to_string())
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!(
            "# allowed {n} times through exec\n[host.tools.{name}]\ncommand = [{cmd}]\ncwd = \"project\"\nmode = \"ask\"\n\n"
        ));
    }
    out
}

fn unique_name(argv: &[String], taken: &mut BTreeSet<String>) -> String {
    let base: String = argv
        .iter()
        .take(3)
        .map(|w| {
            let w = w.rsplit('/').next().unwrap_or(w);
            w.chars()
                .map(|c| c.to_ascii_lowercase())
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("-");
    let base: String = base
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
        .chars()
        .take(40)
        .collect();
    let base = if base.is_empty() { "tool".into() } else { base };
    let mut name = base.clone();
    let mut i = 2;
    while taken.contains(&name) {
        name = format!("{base}-{i}");
        i += 1;
    }
    taken.insert(name.clone());
    name
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    fn tools(text: &str) -> Result<BTreeMap<String, Tool>> {
        let doc: toml::Value = toml::from_str(text).unwrap();
        parse_host(doc["host"].as_table().unwrap())
    }

    #[test]
    fn exact_shapes_match_word_by_word() {
        assert!(matches("up -d", &w(&["up", "-d"])));
        assert!(!matches("up -d", &w(&["up"])));
        assert!(!matches("up -d", &w(&["up", "-d", "x"])));
        assert!(!matches("up -d", &w(&["up -d"])));
        assert!(matches("ps", &w(&["ps"])));
        assert!(matches("", &w(&[])));
    }

    #[test]
    fn a_star_is_one_word_without_a_dash() {
        assert!(matches("logs *", &w(&["logs", "web"])));
        assert!(!matches("logs *", &w(&["logs"])));
        assert!(!matches("logs *", &w(&["logs", "web", "db"])));
        assert!(!matches("logs *", &w(&["logs", "-f"])));
        assert!(!matches("logs *", &w(&["logs", "--volumes"])));
        assert!(!matches("logs *", &w(&["logs", ""])));
    }

    #[test]
    fn a_double_star_is_the_rest_without_dashes() {
        assert!(matches("run **", &w(&["run"])));
        assert!(matches("run **", &w(&["run", "a", "b", "c"])));
        assert!(!matches("run **", &w(&["run", "a", "-x"])));
        assert!(!matches("run **", &w(&["build", "a"])));
    }

    #[test]
    fn no_shape_means_no_arguments() {
        let t = &tools("[host.tools.r]\ncommand = [\"omarchy\", \"restart\", \"shell\"]\n")
            .unwrap()["r"];
        assert!(t.check(&[]).is_ok());
        let e = t.check(&w(&["now"])).unwrap_err();
        assert!(format!("{e}").contains("takes no arguments"), "{e}");
    }

    #[test]
    fn a_tool_checks_every_shape() {
        let t = &tools(
            "[host.tools.compose]\ncommand = [\"docker\", \"compose\"]\nargs = [\"up -d\", \"down\", \"logs *\"]\n",
        )
        .unwrap()["compose"];
        assert!(t.check(&w(&["up", "-d"])).is_ok());
        assert!(t.check(&w(&["logs", "web"])).is_ok());
        assert!(t.check(&w(&["down"])).is_ok());
        assert!(t.check(&w(&[])).is_err());
        let e = t.check(&w(&["run", "--privileged", "x"])).unwrap_err();
        assert!(format!("{e}").contains("\"up -d\""), "{e}");
    }

    #[test]
    fn the_configuration_parses_with_defaults() {
        let t = tools(
            "[host.tools.compose]\ncommand = [\"docker\", \"compose\"]\nargs = [\"ps\"]\ncwd = \"home\"\nmode = \"allow\"\n\
             [host.tools.x]\ncommand = [\"/usr/bin/true\"]\n",
        )
        .unwrap();
        assert_eq!(t["compose"].cwd, Cwd::Home);
        assert_eq!(t["compose"].mode, Mode::Allow);
        assert_eq!(t["x"].cwd, Cwd::Project);
        assert_eq!(t["x"].mode, Mode::Ask);
        assert!(t["x"].args.is_empty());
    }

    #[test]
    fn configuration_errors_name_the_tool() {
        for (text, want) in [
            ("[host.tools.a]\nargs = []\n", "command is missing"),
            ("[host.tools.a]\ncommand = []\n", "command is missing"),
            ("[host.tools.a]\ncommand = \"ls\"\n", "list of strings"),
            (
                "[host.tools.a]\ncommand = [\"ls\"]\nmode = \"maybe\"\n",
                "mode must be",
            ),
            (
                "[host.tools.a]\ncommand = [\"ls\"]\ncwd = \"/\"\n",
                "cwd must be",
            ),
            (
                "[host.tools.a]\ncommand = [\"ls\"]\nenv = []\n",
                "unknown key env",
            ),
            (
                "[host.tools.a]\ncommand = [\"ls\"]\nargs = [\"** x\"]\n",
                "last word",
            ),
            (
                "[host.tools.a]\ncommand = [\"ls\"]\nargs = [\"x*\"]\n",
                "whole words",
            ),
            ("[host.tools.a]\ncommand = [\"bin/ls\"]\n", "absolute path"),
            ("[host.tools.A]\ncommand = [\"ls\"]\n", "lowercase"),
            ("[host]\ntools = 1\n", "one table per tool"),
        ] {
            let e = format!("{:#}", tools(text).unwrap_err());
            assert!(e.contains(want), "{text}: {e}");
        }
    }

    #[test]
    fn only_plain_exec_lines_are_logged() {
        assert_eq!(
            loggable_argv(&w(&["omarchy", "restart", "shell"])),
            Some(w(&["omarchy", "restart", "shell"]))
        );
        assert_eq!(loggable_argv(&w(&["curl", "-H", "Authorization: x"])), None);
        assert_eq!(loggable_argv(&w(&["env", "TOKEN=abc"])), None);
        assert_eq!(loggable_argv(&w(&["gh", "--with-token", "abc"])), None);
        assert_eq!(
            loggable_argv(&w(&["login", "ghp_a1b2c3d4e5f6g7h8i9j0k1l2"])),
            None
        );
        assert_eq!(loggable_argv(&w(&["sh", "-c", "echo hi"])), None);
    }

    #[test]
    fn suggest_turns_repeated_allowed_exec_lines_into_blocks() {
        let line = |argv: &[&str], decision: &str| {
            serde_json::json!({"capability": "exec", "decision": decision, "argv": argv})
                .to_string()
        };
        let restart = ["omarchy", "restart", "shell"];
        let mut log = vec![
            line(&restart, "allowed once"),
            line(&restart, "allowed for this session"),
            line(&restart, "allowed once"),
            line(&["ls"], "allowed once"),
            line(&["ls"], "allowed once"),
            line(&["rm", "x"], "denied"),
            line(&["rm", "x"], "denied"),
            line(&["rm", "x"], "denied"),
            line(&["make"], "allowed once by the agent approver"),
            line(&["make"], "allowed once by the agent approver"),
            line(&["make"], "allowed once by the agent approver"),
            "not json".into(),
            serde_json::json!({"capability": "exec", "decision": "allowed once"}).to_string(),
        ];
        log.push(line(&["ls"], "allowed once"));
        let out = suggest(&log.join("\n"), &BTreeMap::new());
        assert!(out.contains("[host.tools.omarchy-restart-shell]\ncommand = [\"omarchy\", \"restart\", \"shell\"]"), "{out}");
        assert!(out.contains("[host.tools.ls]\ncommand = [\"ls\"]"), "{out}");
        assert!(!out.contains("rm"), "{out}");
        assert!(!out.contains("make"), "{out}");
        // The blocks parse as tools.
        let doc: toml::Value = toml::from_str(&out).unwrap();
        let parsed = parse_host(doc["host"].as_table().unwrap()).unwrap();
        assert_eq!(parsed.len(), 2);
        // An existing tool for the same command is not suggested again.
        let again = suggest(&log.join("\n"), &parsed);
        assert_eq!(again, "");
    }
}
