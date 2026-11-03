use anyhow::Result;
use clap::Subcommand;
use paddock::env::HostEnv;
use paddock::state::Running;

#[derive(Subcommand)]
pub(crate) enum TimerCmd {
    /// The timers of every running sandbox.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Give a running sandbox a timer that you own. --hard freezes the
    /// sandbox when the time or the quota budget is used up.
    Set {
        id: String,
        name: String,
        /// For example 2h, 30m, 1h30m.
        #[arg(long, required_unless_present = "quota")]
        budget: Option<String>,
        /// A share of a limit window of the sandbox's account, counted from
        /// now, for example weekly=1%. `paddock forage` shows the windows.
        #[arg(long)]
        quota: Option<String>,
        #[arg(long)]
        hard: bool,
    },
    /// Add time (30m) or quota (1%) to a timer of a sandbox's own scope.
    Extend {
        id: String,
        name: String,
        #[arg(long)]
        by: String,
    },
    /// Stop a timer of a sandbox's own scope.
    Stop { id: String, name: String },
}

pub(crate) fn run(env: &HostEnv, cmd: TimerCmd) -> Result<i32> {
    use paddock::timer;
    let show = |s: &timer::Status| {
        format!(
            "{}: used {}, {}{}",
            s.name,
            timer::describe(s),
            if s.over {
                "over".to_string()
            } else if s.budget > 0 {
                format!("{} left", timer::format_duration(s.left))
            } else {
                "within budget".to_string()
            },
            if s.hard { " (hard)" } else { "" }
        )
    };
    match cmd {
        TimerCmd::List { json } => {
            let all: Vec<(String, Vec<timer::Status>)> = Running::list(env)
                .into_iter()
                .map(|r| {
                    let t = timer::read_summary(env, &r.id);
                    (r.id, t)
                })
                .filter(|(_, t)| !t.is_empty())
                .collect();
            if json {
                let v: Vec<serde_json::Value> = all
                    .iter()
                    .map(|(id, t)| {
                        serde_json::json!({"sandbox": id, "short": paddock::ids::short(id), "timers": t})
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &serde_json::json!({"api_version": "paddock.timer/v1", "sandboxes": v})
                    )?
                );
            } else if all.is_empty() {
                println!("No running sandbox has a timer.");
            } else {
                for (id, ts) in &all {
                    for t in ts {
                        println!("{id}  {}", show(t));
                    }
                }
            }
        }
        TimerCmd::Set {
            id,
            name,
            budget,
            quota,
            hard,
        } => {
            let id = paddock::ids::resolve(env, &id)?;
            let budget = match budget {
                Some(b) => timer::parse_duration(&b)?,
                None => 0,
            };
            let quota = quota.as_deref().map(timer::parse_quota).transpose()?;
            let s = timer::host_set(env, &id, &name, budget, hard, quota)?;
            println!("{id}  {}", show(&s));
        }
        TimerCmd::Extend { id, name, by } => {
            let id = paddock::ids::resolve(env, &id)?;
            let s = timer::host_change(env, &id, &name, Some(timer::parse_more(&by)?))?;
            println!("{id}  {}", show(&s));
        }
        TimerCmd::Stop { id, name } => {
            let id = paddock::ids::resolve(env, &id)?;
            let s = timer::host_change(env, &id, &name, None)?;
            println!("{id}  {} (stopped)", show(&s));
        }
    }
    Ok(0)
}
