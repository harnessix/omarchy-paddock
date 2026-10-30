---
name: paddock-host
description: Use when you run inside a paddock sandbox and a task needs the host machine, for example a screenshot of the real screen, a command on the host, a file outside the project, the host clipboard, opening a URL in the host browser, git push, a pull request with gh, or starting an agent of another harness (spawn).
---

# Asking the host for help from a paddock sandbox

You are in a paddock sandbox when the environment variable `PADDOCK` is `1`. The sandbox contains the project, your own home directory and package caches. Everything else on the host is absent. Do not search for host files, and do not try to reach host services: they are not there.

When a task needs the host, use `paddock-host`. Each request goes to paddock on the host. The user can approve it, deny it, or have configured it in advance.

1. Run `paddock-host capabilities --json` once. It lists each capability and its mode: `allow`, `ask` or `deny`. Do not use a capability in `deny` mode.
2. Send one request, with `--reason` and one short sentence that tells the user why you need it.
3. Wait for the command to finish. A request in `ask` mode waits until the user answers.
4. Read the result. Files go to `.paddock/inbox/` in the project, and the command prints the path.

| Need | Command |
| :--- | :--- |
| See the real screen | `paddock-host screenshot --reason "..."` |
| See the focused host window | `paddock-host screenshot --target window --reason "..."` |
| Run one command on the host | `paddock-host exec --reason "..." -- COMMAND ARG...` |
| A file outside the project | `paddock-host fetch /absolute/path --reason "..."` |
| The host clipboard | `paddock-host clipboard get` or `paddock-host clipboard set TEXT` |
| Open a page in the host browser | `paddock-host open https://...` |
| Tell the user that work is done | `paddock-host notify "Title" "Body"` |
| Give a task to another harness | `paddock-host spawn codex --worktree --prompt-file task.md` |

## Push and pull requests

The sandbox has no git credentials and no `gh` login. Do not look for a token, and do not ask the user for one. Plain `git push` and `gh` work anyway: the host does the work with the user's credentials after the user approves.

1. Commit, and run the project's checks. Hooks never run on the host.
2. Push the branch: `git push -u origin BRANCH`. To give a reason, set `PADDOCK_PUSH_REASON="..."`.
3. Open the pull request: `gh pr create --title "..." --body "..."`. For a long body, write it to a file and use `--body-file FILE`.
4. Watch CI: `gh pr checks --watch` or `gh run watch`.

Rules that you can rely on:

- Push to a new branch, not to `main` or `master`. Pushes to the default branch and force pushes ask the user every time.
- Do not add remotes. A remote that the sandbox names itself is shown to the user as suspicious.
- `--force-with-lease`, `--dry-run` and push options are not supported.
- `gh` allows only `pr create|view|list|checks|diff|comment`, `issue view|list|comment`, `run view|watch|list` and `repo view`, on the project's own repository. `gh api`, `gh auth`, `--web` and `--editor` are refused.
- Reads run at once. `pr create` and the comment commands wait for the user.

`paddock-host help git-push` and `paddock-host help forge` give the details.

## Other agents

`paddock-host spawn` starts an agent of another harness, for example codex, in its own sandbox. It runs one task and stops. Use it to give one part of the work to another harness, or to get a second opinion. Subagents of your own harness do not need it.

1. Check `paddock-host capabilities --json`. Use spawn only if its mode is not `deny`. The `spawn` object lists the allowed harnesses and the limits.
2. Commit first. The child starts from the commit that you have checked out, not from your uncommitted files.
3. Write the whole task to a file in the project, for example `task.md`. The child does not see this conversation. Tell it which files to change, how to check its work, and to commit its changes. Uncommitted changes stay in its worktree, and you cannot see them.
4. Start it: `id=$(paddock-host spawn codex --worktree --prompt-file task.md --reason "...")`. The command prints only the id.
5. Do other work, then run `paddock-host wait "$id" --timeout 600`. Exit code 0 means that the child finished with exit code 0. Exit code 1 means that it failed or was stopped. Exit code 5 means that it still runs.
6. Read `.paddock/inbox/spawn-ID/result.json` and `output.txt`. Look at the branch with `git log --stat paddock/spawn-codex-ID`, and merge or cherry-pick what you want to keep.

Limits that you can rely on:

- The child gets no more than your sandbox: the same network, environment and mounts, no display, and host capabilities that are at most as open as yours. It uses its own login of its harness. You never see it.
- The user decides which harnesses you may start, how many run at the same time, and how long one may run. By default a child cannot start children of its own.
- The first spawn of each harness asks the user. The user sees the task. Requests that the child sends show the user that you started it, and your session grants do not apply to it.
- A child stops when your sandbox ends. Stop one yourself with `paddock-host kill ID`. `paddock-host status` lists your children.

`paddock-host help spawn` gives the details.

## The sandbox display

If the sandbox has its own display (for a desktop app that you develop), capture it with `paddock-screenshot` instead. That needs no approval.

## Answers

Exit code 3 means that the user denied the request. Plain `git push` exits with 1 instead and prints that the host refused. `paddock-host git push` exits with 3. Do not send it again. Ask in the conversation what the user wants instead. Exit code 5 means that nobody answered in time. Tell the user, and continue with other work. The full reference, with every option and exit code, is `paddock-host --help`.
