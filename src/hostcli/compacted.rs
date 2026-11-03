//! `paddock-host compacted --harness NAME`: the client side of the hook
//! after a context compaction (compaction.rs). It reads the hook input on
//! stdin, asks the broker to record the compaction, and prints the note in
//! the form that the harness reads into its context. A hook never fails:
//! when paddock cannot answer, it prints nothing and exits 0.

use crate::broker::{Request, Status, PROTOCOL};
use crate::compaction::{self, Facts};
use std::collections::BTreeMap;
use std::io::Read;

pub const HELP: &str = r#"paddock-host compacted --harness NAME: the hook after a context compaction.

paddock puts this command into the hooks of your harness. You do not run
it yourself. It reads the hook input on stdin, records the compaction (the
parent of a spawn child sees `compacted:<SPAWN-ID>` in the tree scope of
paddock kv), and prints a short note with the facts of this run: sandbox,
role, parent, pasture and task file. [compaction] in the user configuration
turns it off or adds a text to the note.
"#;

/// The most hook input that paddock reads.
const MAX_INPUT: u64 = 1 << 20;

pub fn main(args: &[String]) -> i32 {
    if args
        .iter()
        .any(|a| matches!(a.as_str(), "-h" | "--help" | "help"))
    {
        print!("{HELP}");
        return 0;
    }
    let harness = match args {
        [flag, h] if flag == "--harness" => h.clone(),
        _ => {
            eprintln!("paddock-host compacted: use --harness NAME");
            return super::EXIT_USAGE;
        }
    };
    let mut raw = String::new();
    let _ = std::io::stdin().take(MAX_INPUT).read_to_string(&mut raw);
    let input: serde_json::Value = serde_json::from_str(raw.trim()).unwrap_or_default();
    let Some(event) = compaction::hook_event(&harness, &input) else {
        // Not a compaction: a hook that also fires on other events.
        return 0;
    };
    let mut a = BTreeMap::new();
    a.insert("harness".into(), serde_json::json!(harness));
    a.insert("event".into(), serde_json::json!(event));
    let req = Request {
        v: PROTOCOL,
        cap: "compacted".into(),
        args: a,
        reason: String::new(),
    };
    let facts = match super::send(&req) {
        Ok(r) if r.status == Status::Done => {
            serde_json::from_value::<Facts>(r.result).unwrap_or_default()
        }
        Ok(r) => {
            log::debug!("no compaction note: {}", r.message);
            return 0;
        }
        Err(e) => {
            log::debug!("no compaction note: {e}");
            return 0;
        }
    };
    if let Some(out) = compaction::render(&harness, &event, &compaction::note(&facts)) {
        println!("{out}");
    }
    0
}
