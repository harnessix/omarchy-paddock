//! `paddock doctor --report`: a diagnostic bundle for bug reports, in the
//! spirit of `omarchy-debug`. Everything goes through [`Redactor`] first, so
//! the report holds no user name, host name, home path, environment values or
//! tokens.

use crate::env::HostEnv;
use std::collections::BTreeMap;
use std::process::Command;

/// Variables whose values are harmless and help to read the report. All other
/// variable values are replaced. HOME is replaced by `~` instead.
const KEEP_VARS: &[&str] = &[
    "HOME",
    "PATH",
    "SHELL",
    "TERM",
    "LANG",
    "LC_ALL",
    "XDG_SESSION_TYPE",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "XDG_STATE_HOME",
    "XDG_RUNTIME_DIR",
    "XDG_CURRENT_DESKTOP",
    "XDG_SESSION_DESKTOP",
    "DESKTOP_SESSION",
    "WAYLAND_DISPLAY",
    "DISPLAY",
    "COLORTERM",
    "SHLVL",
    "_",
];

/// Prefixes of well-known token formats.
const TOKEN_PREFIXES: &[&str] = &[
    "sk-",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "github_pat_",
    "glpat-",
    "xoxb-",
    "xoxp-",
    "AKIA",
    "eyJ",
    "AIza",
    "hf_",
    "npm_",
];

const SECRET_WORDS: &[&str] = &[
    "TOKEN",
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "API_KEY",
    "APIKEY",
    "AUTHORIZATION",
    "COOKIE",
    "CREDENTIAL",
    "PRIVATE",
];

pub struct Redactor {
    home: String,
    user: String,
    host: String,
    values: Vec<(String, String)>,
}

impl Redactor {
    pub fn new(home: &str, user: &str, host: &str, vars: &BTreeMap<String, String>) -> Self {
        let mut values: Vec<(String, String)> = vars
            .iter()
            .filter(|(k, _)| !KEEP_VARS.contains(&k.as_str()))
            // Short values such as `wayland` or `1` would hit ordinary words;
            // short values of secret-looking variables go anyway.
            .filter(|(k, v)| v.trim().len() >= 12 || (secret_name(k) && v.trim().len() >= 4))
            .map(|(k, v)| (v.replace(home, "~"), format!("<env:{k}>")))
            .collect();
        values.sort_by_key(|(v, _)| std::cmp::Reverse(v.len()));
        Redactor {
            home: home.trim_end_matches('/').to_string(),
            user: user.to_string(),
            host: host.to_string(),
            values,
        }
    }

    pub fn from_env(env: &HostEnv) -> Self {
        Self::new(
            &env.home.to_string_lossy(),
            &env.user,
            &hostname(),
            &env.vars,
        )
    }

    pub fn redact(&self, text: &str) -> String {
        let mut t = text.to_string();
        if self.home.len() > 1 {
            t = t.replace(self.home.as_str(), "~");
        }
        for (v, name) in &self.values {
            t = t.replace(v.as_str(), name);
        }
        t = t.lines().map(redact_line).collect::<Vec<_>>().join("\n");
        if text.ends_with('\n') {
            t.push('\n');
        }
        for (word, with) in [(&self.host, "<host>"), (&self.user, "<user>")] {
            if word.len() >= 2 {
                t = replace_word(&t, word, with);
            }
        }
        t
    }
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '-'
}

/// Replace `word` where it is not part of a longer word.
fn replace_word(text: &str, word: &str, with: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find(word) {
        let before = rest[..i].chars().next_back().or(out.chars().next_back());
        let after = rest[i + word.len()..].chars().next();
        out.push_str(&rest[..i]);
        if before.is_some_and(is_word) || after.is_some_and(is_word) {
            out.push_str(word);
        } else {
            out.push_str(with);
        }
        rest = &rest[i + word.len()..];
    }
    out.push_str(rest);
    out
}

fn secret_name(name: &str) -> bool {
    let n = name.to_ascii_uppercase();
    SECRET_WORDS.iter().any(|w| n.contains(w)) || n.ends_with("_KEY") || n == "KEY"
}

/// Redact tokens, `Authorization:` headers and `NAME=value` pairs with a
/// secret-looking name in one line.
fn redact_line(line: &str) -> String {
    let lower = line.to_ascii_lowercase();
    for h in ["authorization:", "proxy-authorization:", "cookie:"] {
        if let Some(i) = lower.find(h) {
            return format!("{} <redacted>", &line[..i + h.len()]);
        }
    }
    // `password: value` and `token = value`: the value is the next word.
    let mut hide_next = false;
    line.split_inclusive(|c: char| c.is_whitespace())
        .map(|w| {
            let body = w.trim_end();
            let tail = &w[body.len()..];
            if body.is_empty() {
                return w.to_string();
            }
            if std::mem::take(&mut hide_next) {
                return format!("<redacted>{tail}");
            }
            let key = body.trim_end_matches([':', '=']);
            if key.len() < body.len() && key.chars().all(is_word) && secret_name(key) {
                hide_next = true;
            }
            format!("{}{tail}", redact_word(body))
        })
        .collect()
}

fn redact_word(w: &str) -> String {
    if let Some((k, v)) = w.split_once(['=', ':']) {
        let key = k.trim_matches(|c: char| !is_word(c));
        if !v.is_empty() && secret_name(key) {
            return format!("{}<redacted>", &w[..k.len() + 1]);
        }
    }
    let core = w.trim_matches(|c: char| !(is_word(c) || c == '.'));
    let long = core.len() >= 16;
    if long
        && (TOKEN_PREFIXES.iter().any(|p| core.starts_with(p))
            || (core.len() >= 32 && core.chars().all(|c| c.is_ascii_hexdigit())))
    {
        return w.replace(core, "<redacted>");
    }
    w.to_string()
}

pub fn hostname() -> String {
    let mut buf = [0u8; 256];
    let ok = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) } == 0;
    if !ok {
        return String::new();
    }
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).to_string()
}

fn capture(program: &str, args: &[&str]) -> String {
    match Command::new(program).args(args).output() {
        Ok(o) => {
            let mut s = String::from_utf8_lossy(&o.stdout).to_string();
            let e = String::from_utf8_lossy(&o.stderr);
            if !e.trim().is_empty() {
                s.push_str(&e);
            }
            if s.trim().is_empty() {
                s = format!("(no output, {})\n", o.status);
            }
            s
        }
        Err(e) => format!("(cannot run {program}: {e})\n"),
    }
}

fn section(out: &mut String, title: &str, body: &str) {
    out.push_str("=========================================\n");
    out.push_str(title);
    out.push_str("\n=========================================\n");
    out.push_str(body.trim_end());
    out.push_str("\n\n");
}

/// Build the report. Every section is redacted.
pub fn build(env: &HostEnv) -> String {
    let me = std::env::current_exe()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| "paddock".into());
    let os = std::fs::read_to_string("/etc/os-release")
        .unwrap_or_default()
        .lines()
        .find_map(|l| {
            l.strip_prefix("PRETTY_NAME=")
                .map(|v| v.trim_matches('"').to_string())
        })
        .unwrap_or_else(|| "unknown".into());
    let kernel = capture("uname", &["-srm"]);
    let omarchy = capture(
        "sh",
        &[
            "-c",
            "pacman -Q omarchy-dev 2>/dev/null || pacman -Q omarchy 2>/dev/null || echo unknown",
        ],
    );
    let mut out = String::new();
    section(
        &mut out,
        "PADDOCK",
        &format!(
            "Version: {}\nOS: {os}\nKernel: {}Omarchy: {}Session: {}",
            env!("CARGO_PKG_VERSION"),
            kernel,
            omarchy,
            env.vars
                .get("XDG_SESSION_TYPE")
                .map(String::as_str)
                .unwrap_or("unknown"),
        ),
    );
    section(&mut out, "DOCTOR", &capture(&me, &["doctor"]));
    section(&mut out, "RUNNING SANDBOXES", &capture(&me, &["ps"]));
    section(
        &mut out,
        "JOURNAL (paddock, CURRENT BOOT, WARNINGS AND ERRORS)",
        &capture(
            "journalctl",
            &[
                "-b",
                "-t",
                crate::logging::IDENTIFIER,
                "-p",
                "4..1",
                "-n",
                "200",
                "--no-pager",
                "-o",
                "short-iso",
            ],
        ),
    );
    Redactor::from_env(env).redact(&out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r() -> Redactor {
        let vars = BTreeMap::from([
            ("HOME".to_string(), "/home/alice".to_string()),
            (
                "PATH".to_string(),
                "/usr/bin:/home/alice/.local/bin".to_string(),
            ),
            (
                "ANTHROPIC_API_KEY".to_string(),
                "abc123-secret-value".to_string(),
            ),
            ("SOME_SETTING".to_string(), "a-long-plain-value".to_string()),
            ("EGL_PLATFORM".to_string(), "wayland".to_string()),
            ("SHORT".to_string(), "ab".to_string()),
        ]);
        Redactor::new("/home/alice", "alice", "alicebox", &vars)
    }

    #[test]
    fn removes_home_user_host_env_values_and_tokens() {
        let text = "\
project /home/alice/src/app on alicebox for alice
key abc123-secret-value and a-long-plain-value, Xwayland
token ghp_0123456789abcdefghijABCDEF here
OPENAI_API_KEY=sk-live-anything
password: hunter2
Authorization: Bearer abc.def.ghi
hash 0123456789abcdef0123456789abcdef
alicealice stays, PATH /usr/bin:~/.local/bin
";
        let out = r().redact(text);
        for leak in [
            "/home/alice",
            "alicebox",
            " alice\n",
            "abc123-secret-value",
            "a-long-plain-value",
            "ghp_0123",
            "sk-live",
            "hunter2",
            "abc.def.ghi",
            "0123456789abcdef0123",
        ] {
            assert!(!out.contains(leak), "{leak:?} leaked in:\n{out}");
        }
        assert!(out.contains("project ~/src/app on <host> for <user>"));
        assert!(out.contains("<env:ANTHROPIC_API_KEY>"));
        assert!(out.contains("<env:SOME_SETTING>"));
        assert!(out.contains("OPENAI_API_KEY=<redacted>"));
        assert!(out.contains("Authorization: <redacted>"));
        assert!(out.contains("alicealice stays"));
        assert!(out.contains("Xwayland"));
        assert!(out.ends_with('\n'));
    }
}
