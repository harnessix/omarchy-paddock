# Files, worktrees, undo, disk space and removal

## Files outside the project

A sandbox writes the project directly. Files that it saves elsewhere, for example in `~/Downloads` of its own home, stay in the sandbox.

- `paddock files [ID]` lists those files and the project changes since the sandbox started.
- `paddock recover ID PATH` moves a file to the same place in the real home, or into `--to DIR`. `--keep` leaves the sandbox copy. It never overwrites a file and never writes into hidden folders such as `~/.ssh`. Ask the user first, and say where the file goes.
- `paddock discard ID PATH` deletes a file from the recovery folders of the sandbox home.

## Worktrees

- `paddock worktree list` lists the worktrees of `--worktree` runs and apps, with their branch and unmerged commits.
- `paddock worktree merge PATH|BRANCH` merges the branch into the current branch of its repository. No hook runs.
- `paddock worktree remove PATH|BRANCH` removes the worktree and its branch. It refuses unmerged commits and uncommitted changes without `--force`.

## Undo

`paddock undo [SESSION]` brings the project back to its snapshot from before a sandbox started. It lists the changes and asks first. Added files go to the undo trash. `paddock undo --list` lists the snapshots.

## Disk space

`paddock du` and `paddock gc` without `--yes` change nothing. Run them at any time; add `--json` for data. A dry run lists what `--yes` removes, also with `--homes`, `--worktrees` or `--include-app-logins`.

Ask the user before `paddock gc` with `--yes`. Show them the dry-run list first, and say what they lose:

- `--homes`: transcripts and settings of old projects.
- `--include-app-logins`: the login of a desktop app. The user logs in again.
- `--worktrees --force`: uncommitted changes in a worktree. Its branch and commits stay.
- `--scratch`: the scratch projects of agents that started in the home folder, with their files.
- `--snapshots`: old snapshots of `paddock undo` and the undo trash, with the files that an undo moved there.
- `--dedupe` deletes nothing. Identical files in the sandbox homes share their data (btrfs, xfs, bcachefs) and keep their content.

## Remove paddock

```sh
paddock setup --yes --no-shims --no-launchers --no-plugin
```

Then delete `~/.local/bin/paddock`, or run `sudo pacman -R omarchy-paddock` if `bin/install` installed the Arch package. The data folders are `~/.local/share/paddock`, `~/.cache/paddock`, `~/.local/state/paddock` and `~/.config/paddock`. They hold sandbox homes and isolated logins: ask the user before you delete them. To free space and keep paddock, use `paddock gc` instead.
