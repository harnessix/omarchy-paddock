//! `paddock-host forage`: the limits of the accounts that this sandbox can
//! hand work to, how fast they are used, and whether they run out before
//! they reset (forage.rs). Read-only.

use crate::kv::{Request, Status, PROTOCOL};

pub const HELP: &str = r#"paddock-host forage: how much of each account's limits is left, and how fast it goes.

USAGE
  paddock-host forage [--refresh] [--json]

It lists the accounts of your own harness and of the harnesses that you may
start with paddock-host spawn. For each limit window: the share used, when it
resets, the rate now (from the last two checks), the usual rate (earlier
windows), when it runs out at the rate now, and OVERGRAZING when that is
before the reset.

--refresh asks for fresh numbers first (at most once in 5 minutes per
account; otherwise the last ones are shown with their age).

Use it before you hand out a long task: give it to the account with room,
and keep the task smaller when every account is overgrazing.
"#;

fn pct(v: &serde_json::Value) -> String {
    v.as_f64()
        .map(|f| format!("{:.0}%", f * 100.0))
        .unwrap_or_else(|| "?".into())
}

fn when(v: &serde_json::Value, now: u64) -> String {
    match v.as_u64() {
        Some(t) if t > now => format!("in {}", crate::timer::format_duration(t - now)),
        Some(_) => "now".into(),
        None => "unknown".into(),
    }
}

pub fn main(args: &[String]) -> i32 {
    if args
        .iter()
        .any(|a| matches!(a.as_str(), "-h" | "--help" | "help"))
    {
        print!("{HELP}");
        return 0;
    }
    let mut json = false;
    let mut refresh = false;
    for a in args {
        match a.as_str() {
            "--json" => json = true,
            "--refresh" => refresh = true,
            other => {
                eprintln!("paddock-host forage: unknown argument {other}\nRun `paddock-host forage --help`.");
                return super::EXIT_USAGE;
            }
        }
    }
    let req = Request {
        v: PROTOCOL,
        op: "forage".into(),
        action: refresh.then(|| "refresh".to_string()),
        ..Default::default()
    };
    let resp = match super::kv::send(&req) {
        Ok(r) => r,
        Err(e) => {
            eprintln!(
                "paddock-host forage: {e}\nThis command works only inside a paddock sandbox."
            );
            return super::EXIT_NO_HOST;
        }
    };
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&resp).unwrap_or_default()
        );
        return resp.status.exit_code();
    }
    if resp.status != Status::Done {
        eprintln!("paddock-host forage: {}", resp.message);
        return resp.status.exit_code();
    }
    print!("{}", render(&resp.result, crate::state::now()));
    0
}

/// The text form of a forage result, shared with `paddock forage`.
pub fn render(result: &serde_json::Value, now: u64) -> String {
    let ws = result["windows"].as_array().cloned().unwrap_or_default();
    if ws.is_empty() {
        return "No limits are known for these accounts yet. Try --refresh.\n".into();
    }
    let mut out = String::new();
    for w in ws {
        let rate = |k: &str| {
            w[k].as_f64()
                .map(|r| format!("{:.1}%/h", r * 100.0))
                .unwrap_or_else(|| "?".into())
        };
        out.push_str(&format!(
            "{} {}: {} used, resets {}; rate {} (usual {}); runs out {}{}{}\n",
            w["agent"].as_str().unwrap_or(""),
            w["label"].as_str().unwrap_or(""),
            pct(&w["used"]),
            when(&w["resets"], now),
            rate("rate"),
            rate("usual"),
            when(&w["runs_out"], now),
            if w["overgrazing"] == true {
                "  OVERGRAZING: before the reset"
            } else {
                ""
            },
            {
                let age = now.saturating_sub(w["fetched"].as_u64().unwrap_or(0));
                if w["stale"] == true || age > 3600 {
                    format!("  (numbers {} old)", crate::timer::format_duration(age))
                } else {
                    String::new()
                }
            }
        ));
    }
    out
}
