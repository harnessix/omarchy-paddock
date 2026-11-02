# Starting an agent of another harness

`paddock-host spawn` starts an agent of another harness, for example codex, in its own sandbox. It runs one task and stops. Use it to give one part of the work to another harness, or to get a second opinion. Subagents of your own harness do not need it.

## As the parent

1. Run `paddock-host capabilities --json`. Use spawn only if its mode is not `deny`. The `spawn` object lists the allowed harnesses and the limits.
2. The child starts from the commit that you have checked out. Commit first, or add `--from-head`: paddock then makes a commit of your current files, uncommitted and new ones included, and the child starts from it. Your own files and branch do not change.
3. Write the whole task to a file in the project, for example `task.md`. The child does not see this conversation. Tell it which files to change, how to check its work, and to commit its changes. You cannot see uncommitted changes in its worktree.
4. Choose a role. `--role implementer` (the default) gets its own writable worktree. `--role reviewer` gets the files read-only and cannot push or change anything with `gh`. Use it to review or compare.
5. Start it: `id=$(paddock-host spawn codex --prompt-file task.md --reason "...")`. The command prints only the id. `--timeout 30m`, `--mem 4G` and `--cpus 2` lower the limits of the user's configuration. They cannot raise them.
6. Do other work, then run `paddock-host wait "$id" --timeout 100`. Exit code 0: the child ended with exit code 0. Exit code 1: it failed, was stopped or reached its time limit. Exit code 75: it still runs. Run the same wait again.
7. The result is the branch `paddock/spawn-HARNESS-ID`. Read it with `git log --stat paddock/spawn-codex-ID` or `git diff HEAD...paddock/spawn-codex-ID`, and merge or cherry-pick what you want to keep. `.paddock/inbox/spawn-ID/result.json` and `output.txt` hold a summary and the end of the child's output.

`paddock-host status` lists your children. `paddock-host kill ID` stops one. A child stops when your sandbox ends.

With several children, share the work in paddock kv `--scope tree`:

- Before you start the children, write one `task:NAME` for each part in `tree`. Put context for them in your own scope: they read it with `--scope parent`.
- Wait with `paddock-host kv watch --prefix result: --scope tree --timeout 100`. Pass the printed `cursor N` as `--version N` to the next watch. When a child ends, paddock writes `result:SPAWN-ID` there, with `status`, `summary` and `ref` (its branch).

## As a child

- Read your task from the prompt, from `--scope parent`, or from `tree`. Claim it in `tree`, and write `result:NAME` in `tree`.
- You have no `pasture` scope. Put what is worth keeping into your result. The parent decides.
- You never get more than your parent. Do not ask the user for a scope, a capability or a device that your parent does not have.

`paddock-host help spawn` gives the details.
