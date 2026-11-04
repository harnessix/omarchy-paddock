//! `paddock-host spec`: the tasks of a Spec Kit feature (speckit.rs).

use crate::broker::{Request, Status, PROTOCOL};
use std::collections::BTreeMap;

pub const HELP: &str = r#"paddock-host spec: the tasks of the Spec Kit feature of this project.

  paddock-host spec status [--feature NAME] [--json]
  paddock-host spec review ID pass|changes [TEXT]
  paddock-host spec note

A project with a .specify/ folder is a Spec Kit project. paddock keeps the
state of each task of tasks.md in paddock kv and writes the checkboxes of
tasks.md itself. Do not edit tasks.md to mark a task: paddock marks it.

status    the active feature (from .specify/feature.json), each task with its
          state (todo, claimed, running, review, done, failed), owner and
          branch, the tasks that can start now, and the tasks that wait for
          a review. Ready [P] tasks in one group can run at the same time.
review    the verdict on a task in review, after you read and merged its
          branch: pass marks it done, changes sends it back to todo, and
          TEXT reaches the next worker. Only the sandbox that started the
          worker reviews.
note      the Spec Kit lines of the session note: the feature, the task of
          this run and its acceptance text, and the spec paths.

Start a worker for a task with
  paddock-host spawn HARNESS --task T012      one task
  paddock-host spawn HARNESS --task next      the next ready task
Run it again for each ready [P] task: they run side by side, up to the
limits of [spawn] in the user configuration. paddock claims each task with
a lease, so two workers never take one task, and a task whose worker died
is free again.

The worker reports with `paddock-host done "SUMMARY"`, or with
`paddock-host done --status failed "WHY"`. When it ends well, the task
waits for your review.

Each task is a record in the tree scope of paddock kv. Watch them with
  paddock-host kv watch --scope tree --prefix spec/FEATURE/task/
"#;

fn send(args: BTreeMap<String, serde_json::Value>) -> Result<crate::broker::Response, String> {
    super::send(&Request {
        v: PROTOCOL,
        cap: "spec".into(),
        args,
        reason: String::new(),
    })
}

/// The Spec Kit lines of the session note. Empty outside a Spec Kit
/// project, and when paddock cannot answer.
pub fn note_lines() -> Vec<String> {
    let mut a = BTreeMap::new();
    a.insert("action".into(), serde_json::json!("note"));
    match send(a) {
        Ok(r) if r.status == Status::Done => r.result["lines"]
            .as_array()
            .map(|l| {
                l.iter()
                    .filter_map(|s| s.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

pub fn main(args: &[String]) -> i32 {
    if args.is_empty()
        || args
            .iter()
            .any(|a| matches!(a.as_str(), "-h" | "--help" | "help"))
    {
        print!("{HELP}");
        return if args.is_empty() {
            super::EXIT_USAGE
        } else {
            0
        };
    }
    let p = match super::parse(&args[1..], &["feature"]) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("paddock-host spec: {e}");
            return super::EXIT_USAGE;
        }
    };
    let mut a = BTreeMap::new();
    let action = args[0].as_str();
    a.insert("action".into(), serde_json::json!(action));
    if let Some(f) = p.opts.get("feature") {
        a.insert("feature".into(), serde_json::json!(f));
    }
    match (action, p.rest.as_slice()) {
        ("status" | "note", []) => {}
        ("review", [id, verdict, text @ ..]) if text.len() <= 1 => {
            if !matches!(verdict.as_str(), "pass" | "changes") {
                eprintln!("paddock-host spec review: the verdict is pass or changes");
                return super::EXIT_USAGE;
            }
            a.insert("id".into(), serde_json::json!(id));
            a.insert("verdict".into(), serde_json::json!(verdict));
            if let Some(t) = text.first() {
                a.insert("text".into(), serde_json::json!(t));
            }
        }
        _ => {
            eprintln!("paddock-host spec: use status, review ID pass|changes [TEXT], or note\nRun `paddock-host spec --help`.");
            return super::EXIT_USAGE;
        }
    }
    let r = match send(a) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("paddock-host: {e}\nThis command works only inside a paddock sandbox.");
            return super::EXIT_NO_HOST;
        }
    };
    if p.json {
        println!("{}", serde_json::to_string_pretty(&r).unwrap_or_default());
    } else if r.status != Status::Done {
        eprintln!("paddock-host spec: {}", r.message);
    } else {
        match action {
            "status" => println!("{}", r.result["text"].as_str().unwrap_or("")),
            "note" => {
                for l in r.result["lines"].as_array().into_iter().flatten() {
                    println!("{}", l.as_str().unwrap_or(""));
                }
            }
            _ => println!(
                "{}: {}",
                r.result["id"].as_str().unwrap_or(""),
                r.result["state"].as_str().unwrap_or("")
            ),
        }
    }
    r.status.exit_code()
}
