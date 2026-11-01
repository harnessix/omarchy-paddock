//! `paddock-host status`: the task timers and the account limits of this
//! sandbox in a few lines, for the agent's context. paddock runs it from a
//! harness hook (Claude Code: SessionStart, UserPromptSubmit and
//! PostToolUse), so an agent hears about its time and its limits again
//! after a compaction, without having to remember to ask.
//!
//! `--hook session` prints the whole state; `--hook prompt` and `--hook
//! tool` print only what needs attention (a timer near or past its end, a
//! limit that runs out before it resets or is almost used up), and `tool`
//! says the same thing at most once in 10 minutes. A hook never fails: when
//! paddock cannot answer, it prints nothing and exits 0.

use crate::kv::{Request, Scope, Status, PROTOCOL};
use crate::timer::format_duration;

pub const HELP: &str = r#"paddock-host status: your task timers and account limits, in a few lines.

USAGE
  paddock-host status [--hook session|prompt|tool]

Without --hook, it prints everything. paddock runs it from your harness's
hooks: at the start of a session and after a compaction (everything), and on
each prompt and after each tool (only what needs attention).
"#;

/// Say the same attention line again after this many seconds (tool hook).
const REPEAT: u64 = 10 * 60;

fn ask(op: &str, action: &str, scope: Option<Scope>) -> Option<serde_json::Value> {
    let req = Request {
        v: PROTOCOL,
        op: op.into(),
        action: Some(action.into()),
        scope,
        ..Default::default()
    };
    let r = super::kv::send(&req).ok()?;
    (r.status == Status::Done).then_some(r.result)
}

/// The lines: everything, and the ones that need attention.
pub fn lines(
    timers: &[serde_json::Value],
    windows: &[serde_json::Value],
    now: u64,
) -> (Vec<String>, Vec<String>) {
    let (mut all, mut urgent) = (Vec::new(), Vec::new());
    for t in timers {
        if t["stopped"] == true {
            continue;
        }
        let n = |k: &str| t[k].as_u64().unwrap_or(0);
        let (used, budget, left) = (n("used"), n("budget"), n("left"));
        let line = format!(
            "timer {}: used {} of {}, {}{}",
            t["name"].as_str().unwrap_or(""),
            format_duration(used),
            format_duration(budget),
            if t["over"] == true {
                "time is up".to_string()
            } else {
                format!("{} left", format_duration(left))
            },
            if t["hard"] == true {
                " (hard: the sandbox is frozen at the end)"
            } else {
                ""
            }
        );
        if t["over"] == true || left * 5 <= budget {
            urgent.push(line.clone());
        }
        all.push(line);
    }
    for w in windows {
        let used = w["used"].as_f64().unwrap_or(0.0);
        let when = |k: &str| match w[k].as_u64() {
            Some(t) if t > now => format!("in {}", format_duration(t - now)),
            Some(_) => "now".to_string(),
            None => "unknown".to_string(),
        };
        let over = w["overgrazing"] == true;
        let line = format!(
            "{} {}: {:.0}% used, resets {}{}",
            w["agent"].as_str().unwrap_or(""),
            w["label"].as_str().unwrap_or(""),
            used * 100.0,
            when("resets"),
            if over {
                format!(
                    ", runs out {} at this rate, before the reset",
                    when("runs_out")
                )
            } else {
                String::new()
            }
        );
        if over || used >= 0.9 {
            urgent.push(line.clone());
        }
        all.push(line);
    }
    (all, urgent)
}

fn text(lines: &[String], hint: bool) -> String {
    let mut s = format!("paddock: {}.", lines.join("; "));
    if hint {
        s.push_str(" If a fix keeps failing, stop and report what you tried. `paddock-host timer status` and `paddock-host forage` show more.");
    }
    s
}

/// Whether the tool hook said this already within `REPEAT` seconds.
fn said_lately(msg: &str, now: u64) -> bool {
    use sha2::{Digest, Sha256};
    let dir = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into());
    let f = std::path::Path::new(&dir).join(".paddock-status-said");
    let h = crate::manifest::hex(&Sha256::digest(msg.as_bytes()));
    let before = std::fs::read_to_string(&f).unwrap_or_default();
    let mut it = before.split_whitespace();
    let (Some(t), Some(old)) = (it.next(), it.next()) else {
        let _ = std::fs::write(&f, format!("{now} {h}"));
        return false;
    };
    if old == h && t.parse::<u64>().unwrap_or(0) + REPEAT > now {
        return true;
    }
    let _ = std::fs::write(&f, format!("{now} {h}"));
    false
}

pub fn main(args: &[String]) -> i32 {
    if args
        .iter()
        .any(|a| matches!(a.as_str(), "-h" | "--help" | "help"))
    {
        print!("{HELP}");
        return 0;
    }
    let hook = match args {
        [] => None,
        [flag, h] if flag == "--hook" && matches!(h.as_str(), "session" | "prompt" | "tool") => {
            Some(h.as_str())
        }
        _ => {
            eprintln!("paddock-host status: use --hook session, prompt or tool\nRun `paddock-host status --help`.");
            return if args.first().is_some_and(|a| a == "--hook") {
                0
            } else {
                super::EXIT_USAGE
            };
        }
    };
    let mut timers = Vec::new();
    for scope in [Scope::Own, Scope::Pasture, Scope::Tree] {
        if let Some(r) = ask("timer", "list", Some(scope)) {
            timers.extend(r["timers"].as_array().cloned().unwrap_or_default());
        }
    }
    let windows = ask("forage", "read", None)
        .and_then(|r| r["windows"].as_array().cloned())
        .unwrap_or_default();
    let now = crate::state::now();
    let (all, urgent) = lines(&timers, &windows, now);
    match hook {
        None | Some("session") => {
            if !all.is_empty() {
                println!("{}", text(&all, hook.is_some()));
            }
        }
        Some("prompt") => {
            if !urgent.is_empty() {
                println!("{}", text(&urgent, true));
            }
        }
        _ => {
            let msg = text(&urgent, true);
            if !urgent.is_empty() && !said_lately(&msg, now) {
                println!(
                    "{}",
                    serde_json::json!({"hookSpecificOutput": {"hookEventName": "PostToolUse", "additionalContext": msg}})
                );
            }
        }
    }
    0
}

/// The harnesses whose hooks paddock fills, and the arguments that add
/// them. Claude Code merges hooks from every settings source, so these run
/// beside the user's own hooks.
pub fn harness_args(env: &crate::env::HostEnv, harness: &str) -> Vec<String> {
    if harness != "claude" || !enabled(env) {
        return Vec::new();
    }
    let hook = |which: &str| serde_json::json!([{"hooks": [{"type": "command", "command": format!("paddock-host status --hook {which}"), "timeout": 20}]}]);
    let settings = serde_json::json!({"hooks": {
        "SessionStart": hook("session"),
        "UserPromptSubmit": hook("prompt"),
        "PostToolUse": hook("tool"),
    }});
    vec!["--settings".into(), settings.to_string()]
}

/// `[status] hooks = false` in the user configuration turns the hooks off.
pub fn enabled(env: &crate::env::HostEnv) -> bool {
    let file = env.paddock_config().join("config.toml");
    let Ok(text) = std::fs::read_to_string(file) else {
        return true;
    };
    toml::from_str::<toml::Value>(&text)
        .ok()
        .and_then(|v| v.get("status")?.get("hooks")?.as_bool())
        .unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_what_needs_attention_is_urgent() {
        let t = |name: &str, used: u64, budget: u64, over: bool| serde_json::json!({"name": name, "used": used, "budget": budget, "left": budget.saturating_sub(used), "over": over, "hard": false, "stopped": false});
        let w = |used: f64, over: bool| serde_json::json!({"agent": "codex", "label": "Weekly", "used": used, "resets": 2000, "runs_out": 1500, "overgrazing": over});
        let timers = [
            t("early", 600, 7200, false),
            t("late", 6000, 7200, false),
            t("gone", 8000, 7200, true),
        ];
        let windows = [w(0.3, false), w(0.5, true), w(0.95, false)];
        let (all, urgent) = lines(&timers, &windows, 1000);
        assert_eq!(all.len(), 6);
        assert_eq!(urgent.len(), 4, "{urgent:?}");
        assert!(urgent
            .iter()
            .any(|l| l.contains("late") && l.contains("20m left")));
        assert!(urgent
            .iter()
            .any(|l| l.contains("gone") && l.contains("time is up")));
        assert!(urgent.iter().any(|l| l.contains("before the reset")));
        assert!(!urgent.iter().any(|l| l.contains("early")));
        // Stopped timers say nothing.
        let mut stopped = t("done", 8000, 7200, true);
        stopped["stopped"] = true.into();
        assert!(lines(&[stopped], &[], 1000).0.is_empty());
    }

    #[test]
    fn claude_gets_three_hooks_unless_the_user_turns_them_off() {
        let t = tempfile::tempdir().unwrap();
        let env = crate::env::HostEnv::from_parts(
            t.path().join("home"),
            "t".into(),
            1000,
            1000,
            Default::default(),
        );
        let a = harness_args(&env, "claude");
        assert_eq!(a[0], "--settings");
        let v: serde_json::Value = serde_json::from_str(&a[1]).unwrap();
        for (event, which) in [
            ("SessionStart", "session"),
            ("UserPromptSubmit", "prompt"),
            ("PostToolUse", "tool"),
        ] {
            assert_eq!(
                v["hooks"][event][0]["hooks"][0]["command"],
                format!("paddock-host status --hook {which}")
            );
        }
        assert!(harness_args(&env, "codex").is_empty());
        let cfg = env.paddock_config();
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(cfg.join("config.toml"), "[status]\nhooks = false\n").unwrap();
        assert!(harness_args(&env, "claude").is_empty());
    }
}
