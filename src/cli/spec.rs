use anyhow::{Context as _, Result};
use clap::Subcommand;
use paddock::env::HostEnv;
use paddock::speckit::flow::{self, Spot};
use std::path::PathBuf;

#[derive(Subcommand)]
pub(crate) enum SpecCmd {
    /// The active Spec Kit feature of a project, its tasks with state, owner and branch, what can start, and what waits for review.
    Status {
        /// The project. Default: the current folder.
        #[arg(long)]
        dir: Option<PathBuf>,
        /// A feature folder in specs/. Default: the active feature.
        #[arg(long)]
        feature: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Move .specify/ and specs/ of a repository into paddock's external store, after a question. Needs [speckit] store = "external".
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

pub(crate) fn run(env: &HostEnv, cmd: SpecCmd) -> Result<i32> {
    match cmd {
        SpecCmd::Status { dir, feature, json } => {
            let project = project(dir)?;
            let Some(layout) = paddock::speckit::Layout::resolve(env, &project) else {
                anyhow::bail!(
                    "{} is no Spec Kit project (no .specify/ folder), or [speckit] enabled = false",
                    project.display()
                );
            };
            let now = paddock::state::now();
            let feature = feature
                .or_else(|| layout.feature())
                .context("this Spec Kit project has no feature with a tasks.md yet")?;
            let repo = paddock::speckit::repo_of(&project);
            // The newest run of this repository with records of the feature.
            let root = flow::newest_tree(env, &repo, &feature, now);
            let spot = Spot::find(
                env,
                &project,
                root.as_deref().unwrap_or("no-run"),
                Some(&feature),
            )?
            .context("no Spec Kit project")?;
            let v = spot.sync(env, false, now)?;
            if json {
                let mut j = flow::to_json(&v, &spot.layout);
                j["api_version"] = "paddock.spec/v1".into();
                j["run"] = root.into();
                j["store_path"] = serde_json::json!(layout.root);
                j["git_tracked"] = if layout.external {
                    serde_json::Value::Null
                } else {
                    serde_json::json!(paddock::speckit::git::tracked(&layout.root))
                };
                println!("{j}");
            } else {
                println!("{}", paddock::speckit::git::location(&layout));
                println!("{}", flow::to_text(&v));
                if root.is_none() {
                    println!(
                        "(no run has worked on these tasks yet: the states come from tasks.md)"
                    );
                }
            }
            Ok(0)
        }
        SpecCmd::Adopt { dir, yes } => {
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
            println!(
                "{}",
                paddock::speckit::adopt::adopt(env, &project, &mut ask)?
            );
            Ok(0)
        }
    }
}
