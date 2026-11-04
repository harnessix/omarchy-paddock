use anyhow::Result;
use clap::Subcommand;
use paddock::env::HostEnv;

#[derive(Subcommand)]
pub(crate) enum WorktreeCmd {
    /// List the worktrees with their branch, unmerged commits and state.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Merge the branch into the current branch of the repository. No hook runs.
    Merge {
        /// The worktree path, its folder name or its branch.
        target: String,
    },
    /// Remove the worktree and delete its branch. To keep both, do nothing.
    Remove {
        /// The worktree path, its folder name or its branch.
        target: String,
        /// Also with unmerged commits or uncommitted changes.
        #[arg(long)]
        force: bool,
    },
}

pub(crate) fn worktree_cmd(env: &HostEnv, cmd: WorktreeCmd) -> Result<i32> {
    use paddock::worktree;
    let all = worktree::list(env);
    match cmd {
        WorktreeCmd::List { json: true } => println!(
            "{}",
            serde_json::json!({ "api_version": "paddock.worktrees/v1", "worktrees": all })
        ),
        WorktreeCmd::List { json: false } => print!("{}", worktree::render(&all)),
        WorktreeCmd::Merge { target } => {
            println!("{}", worktree::merge(env, &worktree::find(&all, &target)?)?)
        }
        WorktreeCmd::Remove { target, force } => println!(
            "{}",
            worktree::remove(env, &worktree::find(&all, &target)?, force)?
        ),
    }
    Ok(0)
}

pub(crate) fn run(env: &HostEnv, cmd: WorktreeCmd) -> Result<i32> {
    worktree_cmd(env, cmd)
}
