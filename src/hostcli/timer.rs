//! `paddock-host timer`: timers that live on the host (timer.rs), through
//! the paddock kv socket of this sandbox.

use crate::kv::{Request, Scope, Status, PROTOCOL};
use crate::timer::{format_duration, parse_duration, parse_more, parse_quota, More};

pub const HELP: &str = r#"paddock-host timer: how long a task has run, and how long it may.

USAGE
  paddock-host timer start NAME [--budget DURATION] [--quota WINDOW=PCT] [--hard]
                           [--scope SCOPE]
  paddock-host timer status [NAME] [--scope SCOPE]
  paddock-host timer extend NAME --by DURATION|PCT [--reason TEXT] [--scope SCOPE]
  paddock-host timer stop NAME [--scope SCOPE]
Every command takes --json. DURATION: 90s, 30m, 2h, 1h30m, 1d. PCT: 5%.

A timer lives on the host, so it outlasts your context: after a compaction,
or when another agent takes over, `paddock-host timer status` still knows
when the task started and how much time is left. Time while the sandbox is
frozen does not count.

Run `paddock-host timer status` when you start working again, after a
compaction, and before you try one more fix for the same bug.

- start: you own the timer. It needs --budget, --quota, or both.
  --quota weekly=10% lets the task use 10% of the account's weekly limit
  window, counted from now (`paddock-host forage` shows the windows). It
  counts everything that the account uses meanwhile, also elsewhere, and
  its numbers are up to 15 minutes old.
  A --hard timer freezes this sandbox when a budget is used up; without
  --hard, the user gets a notice and you keep running.
- extend: --by 30m adds time, --by 5% adds quota. The owner adds at once. A timer that someone else set (the
  user, or the agent that started you) asks the user, who sees --reason.
- stop: only the owner stops a timer.

SCOPES (--scope, default self): self, pasture, tree, as for paddock-host kv.
Timers in pasture and tree are visible to the other agents there.

EXIT CODES
  0 done   1 no such timer   2 wrong usage   3 denied   5 nobody answered
  8 refused, for example an unknown window   9 a timer of that name runs already
"#;

fn usage(msg: &str) -> i32 {
    eprintln!("paddock-host timer: {msg}\nRun `paddock-host timer --help`.");
    super::EXIT_USAGE
}

pub fn main(args: &[String]) -> i32 {
    let Some(action) = args.first().map(String::as_str) else {
        print!("{HELP}");
        return super::EXIT_USAGE;
    };
    if matches!(action, "-h" | "--help" | "help") {
        print!("{HELP}");
        return 0;
    }
    let mut name: Option<String> = None;
    let mut req = Request {
        v: PROTOCOL,
        op: "timer".into(),
        action: Some(action.to_string()),
        ..Default::default()
    };
    let mut json = false;
    let mut it = args[1..].iter();
    while let Some(a) = it.next() {
        let mut value = |what: &str| {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{what} needs a value"))
        };
        let r: Result<(), String> = (|| {
            match a.as_str() {
                "--json" => json = true,
                "--hard" => req.hard = Some(true),
                "--budget" => {
                    req.budget =
                        Some(parse_duration(&value("--budget")?).map_err(|e| e.to_string())?)
                }
                "--by" => match parse_more(&value("--by")?).map_err(|e| e.to_string())? {
                    More::Time(n) => req.by = Some(n),
                    More::Share(x) => req.share = Some(x),
                },
                "--quota" => {
                    let (w, x) = parse_quota(&value("--quota")?).map_err(|e| e.to_string())?;
                    req.window = Some(w);
                    req.share = Some(x);
                }
                "--reason" => req.reason = Some(value("--reason")?),
                "--scope" => {
                    req.scope = Some(value("--scope")?.parse::<Scope>()?);
                }
                s if s.starts_with('-') => return Err(format!("unknown option {s}")),
                s if name.is_none() => name = Some(s.to_string()),
                s => return Err(format!("unexpected argument {s}")),
            }
            Ok(())
        })();
        if let Err(e) = r {
            return usage(&e);
        }
    }
    match action {
        "start" | "extend" | "stop" if name.is_none() => {
            return usage(&format!("{action} needs a NAME"))
        }
        "start" if req.budget.is_none() && req.window.is_none() => {
            return usage("start needs --budget, --quota, or both")
        }
        "extend" if req.by.is_none() && req.share.is_none() => return usage("extend needs --by"),
        "status" if name.is_none() => req.action = Some("list".into()),
        "start" | "status" | "extend" | "stop" => {}
        other => return usage(&format!("unknown command '{other}'")),
    }
    req.key = name;
    let resp = match super::kv::send(&req) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("paddock-host timer: {e}\nThis command works only inside a paddock sandbox.");
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
        eprintln!("paddock-host timer: {}", resp.message);
        return resp.status.exit_code();
    }
    let line = |t: &serde_json::Value| {
        let n = |k: &str| t[k].as_u64().unwrap_or(0);
        let used = serde_json::from_value::<crate::timer::Status>(t.clone())
            .map(|s| crate::timer::describe(&s))
            .unwrap_or_default();
        format!(
            "{}: used {}, {}{}{}",
            t["name"].as_str().unwrap_or(""),
            used,
            if t["over"] == true {
                "over".to_string()
            } else if n("budget") > 0 {
                format!("{} left", format_duration(n("left")))
            } else {
                "within budget".to_string()
            },
            if t["hard"] == true {
                " (hard: the sandbox is frozen at the end)"
            } else {
                ""
            },
            if t["stopped"] == true {
                ", stopped"
            } else {
                ""
            },
        )
    };
    match resp.result.get("timers").and_then(|t| t.as_array()) {
        Some(all) if all.is_empty() => println!("No timers in this scope."),
        Some(all) => all.iter().for_each(|t| println!("{}", line(t))),
        None => println!("{}", line(&resp.result)),
    }
    0
}
