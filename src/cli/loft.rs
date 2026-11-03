use anyhow::{Context as _, Result};
use clap::Subcommand;
use paddock::env::HostEnv;
use paddock::loft::{self, host, Loft};
use std::path::PathBuf;

#[derive(Subcommand)]
pub(crate) enum LoftCmd {
    /// Make the loft of the current repository and turn it on. `.paddock` in the project becomes a link to its notes.
    Init {
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// The state of the loft, the private files that wait in the project, and the changes since the last commit.
    Status {
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// The history of the loft.
    Log {
        #[arg(long)]
        dir: Option<PathBuf>,
        #[arg(short = 'n', long, default_value_t = 20)]
        lines: usize,
    },
    /// The changes since the last commit of the loft.
    Diff {
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// Commit the changes of the loft now.
    Commit {
        #[arg(long)]
        dir: Option<PathBuf>,
        #[arg(short = 'm', long, default_value = "Commit by hand")]
        message: String,
    },
    /// Take the `.paddock` link away and turn the loft off. Its files and history stay.
    Remove {
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// Move the private files of the repository (instruction files, design files, Spec Kit folders) into the loft, after a question.
    Adopt {
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Do not ask.
        #[arg(long)]
        yes: bool,
    },
}

fn project(dir: Option<PathBuf>) -> Result<PathBuf> {
    let d = match dir {
        Some(d) => d,
        None => std::env::current_dir()?,
    };
    std::fs::canonicalize(&d).with_context(|| format!("no folder {}", d.display()))
}

fn made(env: &HostEnv, dir: Option<PathBuf>) -> Result<Loft> {
    let p = project(dir)?;
    let l = Loft::of(env, &p);
    anyhow::ensure!(
        l.initialized(),
        "{} has no loft: `paddock loft init` makes it",
        p.display()
    );
    Ok(l)
}

pub(crate) fn run(env: &HostEnv, cmd: LoftCmd) -> Result<i32> {
    paddock::update::refuse_in_sandbox(env, "paddock loft")?;
    match cmd {
        LoftCmd::Init { dir } => println!("{}", host::init(env, &project(dir)?)?),
        LoftCmd::Status { dir } => print!("{}", host::status(env, &project(dir)?)?),
        LoftCmd::Log { dir, lines } => print!("{}", loft::git::log(&made(env, dir)?, lines)?),
        LoftCmd::Diff { dir } => print!("{}", loft::git::diff(&made(env, dir)?)?),
        LoftCmd::Commit { dir, message } => {
            if loft::git::commit(&made(env, dir)?, &message)? {
                println!("Committed the loft.");
            } else {
                println!("Nothing changed since the last commit.");
            }
        }
        LoftCmd::Remove { dir } => println!("{}", host::remove(env, &project(dir)?)?),
        LoftCmd::Adopt { dir, yes } => {
            let project = project(dir)?;
            let mut term = paddock::setup::Terminal::detect();
            let mut ask = |q: &str| {
                if yes {
                    return true;
                }
                match term.as_mut() {
                    Some(t) => paddock::setup::Ask::yes_no(t, q, false),
                    None => {
                        eprintln!("paddock: {q}\nThere is no terminal to answer. Run it again with --yes.");
                        false
                    }
                }
            };
            println!("{}", host::adopt(env, &project, &mut ask)?);
        }
    }
    Ok(0)
}
