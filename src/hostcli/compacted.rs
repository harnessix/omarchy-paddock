//! `paddock-host compacted --harness NAME`: the client side of the hook
//! after a context compaction (compaction.rs). It reads the hook input on
//! stdin, asks the broker to record the compaction, and prints the note in
//! the form that the harness reads into its context. A hook never fails:
//! when paddock cannot answer, it prints nothing and exits 0.

use crate::broker::{Request, Status, PROTOCOL};
use crate::compaction::{self, Action, Facts};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

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
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    if let Some(out) = run(&harness, &input, &home, &mut |event| ask(&harness, event)) {
        println!("{out}");
    }
    0
}

/// Do what the hook input asks, with `record` for the broker. Returns the
/// text to print. Every failure is silent: a hook never fails.
pub fn run(
    harness: &str,
    input: &serde_json::Value,
    home: &Path,
    record: &mut dyn FnMut(&str) -> Option<Facts>,
) -> Option<String> {
    let dir = compaction::state_dir(home);
    match compaction::action(harness, input)? {
        Action::Direct { event } => {
            let f = record(&event)?;
            compaction::render(harness, &event, &compaction::note(&f))
        }
        Action::Mark { session } => {
            // The harness discards this output: keep the note for the next
            // event that can carry it.
            let f = record("PostCompact")?;
            let file = dir.join(compaction::file_name("note", &session));
            if let Err(e) =
                crate::privfs::dir(&dir).and_then(|_| std::fs::write(&file, compaction::note(&f)))
            {
                log::debug!("cannot keep the compaction note: {e:#}");
            }
            None
        }
        Action::Deliver { session, event } => {
            let file = dir.join(compaction::file_name("note", &session));
            let note = std::fs::read_to_string(&file).ok()?;
            let _ = std::fs::remove_file(&file);
            compaction::render(harness, &event, &note)
        }
        Action::Scan {
            conversation,
            transcript,
            event,
        } => {
            let path = match transcript.strip_prefix("~/") {
                Some(rest) => home.join(rest),
                None => PathBuf::from(&transcript),
            };
            let name = path.file_name()?.to_str()?;
            if !matches!(name, "transcript.jsonl" | "transcript_full.jsonl") {
                return None;
            }
            let src = std::fs::File::open(&path).ok()?;
            let now = compaction::steps(std::io::BufReader::new(src));
            let file = dir.join(compaction::file_name("steps", &conversation));
            let seen: Option<u64> = std::fs::read_to_string(&file)
                .ok()
                .and_then(|t| t.trim().parse().ok());
            if seen != Some(now.lines) {
                let _ = crate::privfs::dir(&dir)
                    .map(|_| std::fs::write(&file, format!("{}\n", now.lines)));
            }
            // The first look at a conversation only takes the count: a
            // resumed one can have older checkpoints.
            if !now.compacted_since(seen?) {
                return None;
            }
            let f = record(&event)?;
            compaction::render(harness, &event, &compaction::note(&f))
        }
    }
}

/// Ask the broker to record one compaction, and return the facts.
fn ask(harness: &str, event: &str) -> Option<Facts> {
    let mut a = BTreeMap::new();
    a.insert("harness".into(), serde_json::json!(harness));
    a.insert("event".into(), serde_json::json!(event));
    let req = Request {
        v: PROTOCOL,
        cap: "compacted".into(),
        args: a,
        reason: String::new(),
    };
    match super::send(&req) {
        Ok(r) if r.status == Status::Done => serde_json::from_value::<Facts>(r.result).ok(),
        Ok(r) => {
            log::debug!("no compaction note: {}", r.message);
            None
        }
        Err(e) => {
            log::debug!("no compaction note: {e}");
            None
        }
    }
}
