use anyhow::Result;
use clap::Subcommand;
use paddock::env::HostEnv;
use paddock::horse::{self, dsh, Store};
use std::path::PathBuf;

#[derive(Subcommand)]
pub(crate) enum HorseCmd {
    /// List the sessions in the horse store.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Show a session, its horse and its lineage.
    Show {
        session: String,
        #[arg(long)]
        json: bool,
    },
    /// The nodes of a session, root first, with the message view.
    Log {
        session: String,
        /// Show only the last N nodes.
        #[arg(short = 'n', long)]
        lines: Option<usize>,
    },
    /// Make a new session whose head is a node of SESSION.
    Fork {
        session: String,
        /// A node hash (at least 6 digits) or a native key such as seq:12.
        #[arg(long)]
        at: String,
        /// The horse of the new session. Default: the horse of SESSION.
        #[arg(long)]
        horse: Option<String>,
    },
    /// Read a dsh session file or session folder into the store.
    IngestDsh {
        path: PathBuf,
        /// The dsh version that wrote the session. Only the pinned version works.
        #[arg(long)]
        dsh_version: String,
        /// Put a new session into this horse. Default: a new horse.
        #[arg(long)]
        horse: Option<String>,
    },
    /// Write a session as a native dsh session file under a dsh sessions folder.
    ProjectDsh {
        session: String,
        /// The dsh sessions folder, for example $DSH_HOME/sessions.
        #[arg(long)]
        root: PathBuf,
    },
}

pub(crate) fn run(env: &HostEnv, cmd: HorseCmd) -> Result<i32> {
    let st = Store::open(&horse::root(env))?;
    match cmd {
        HorseCmd::List { json } => {
            let all = st.sessions()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&all)?);
                return Ok(0);
            }
            for s in all {
                let n = st.session_nodes(&s)?.len();
                println!(
                    "{}  {}  {} {}  {} nodes  {}",
                    s.id,
                    s.horse,
                    s.binding.harness,
                    s.binding.native_id.as_deref().unwrap_or("(not projected)"),
                    n,
                    s.fork_of
                        .as_ref()
                        .map(|f| format!("fork of {} at {}", f.session, f.node.short()))
                        .unwrap_or_default()
                );
            }
        }
        HorseCmd::Show { session, json } => {
            let s = st.session(&session)?;
            let h = st.horse(&s.horse)?;
            if json {
                let v = serde_json::json!({ "session": s, "horse": h });
                println!("{}", serde_json::to_string_pretty(&v)?);
                return Ok(0);
            }
            println!("session   {}", s.id);
            println!("horse     {} (DNA {})", h.id, h.dna.short());
            println!(
                "harness   {} {} ({})",
                s.binding.harness, s.binding.version, s.binding.container
            );
            println!(
                "native id {}",
                s.binding.native_id.as_deref().unwrap_or("(not projected)")
            );
            println!(
                "head      {}",
                s.head.as_ref().map_or("(empty)", |h| h.short())
            );
            if let Some(f) = &s.fork_of {
                println!("fork of   {} at {}", f.session, f.node.short());
            }
            for l in &s.lineage {
                println!("lineage   {} (DNA {})", l.horse, l.dna.short());
            }
        }
        HorseCmd::Log { session, lines } => {
            let s = st.session(&session)?;
            let nodes = st.session_nodes(&s)?;
            let skip = lines.map_or(0, |n| nodes.len().saturating_sub(n));
            for n in nodes.iter().skip(skip) {
                let key = n.meta.key.as_deref().unwrap_or("-");
                let line = match (s.binding.harness.as_str(), dsh::view(&n.raw)) {
                    ("dsh", Some(v)) => format!("{}: {}", v.role, v.text),
                    ("dsh", None) => dsh::row_info(&n.raw)
                        .map(|(t, _, _)| t)
                        .unwrap_or_else(|_| "?".into()),
                    _ => format!("{} bytes", n.raw.len()),
                };
                let line: String = line.chars().take(100).collect();
                println!("{}  {key:>8}  {}", n.hash.short(), line.replace('\n', " "));
            }
        }
        HorseCmd::Fork { session, at, horse } => {
            let s = st.session(&session)?;
            let node = st.resolve_node(&s, &at)?;
            let f = st.fork(&session, &node, horse.as_deref())?;
            println!("{}", f.id);
        }
        HorseCmd::IngestDsh {
            path,
            dsh_version,
            horse,
        } => {
            let r = dsh::ingest(&st, &path, &dsh_version, horse.as_deref())?;
            println!(
                "{}: {} new, {} kept, {} dropped{}",
                r.session,
                r.added,
                r.kept,
                r.dropped,
                if r.torn { ", torn tail left out" } else { "" }
            );
        }
        HorseCmd::ProjectDsh { session, root } => {
            let p = dsh::project(&st, &session, &root, horse::now_ms())?;
            println!("{} {}", p.native_id, p.path.display());
        }
    }
    Ok(0)
}
