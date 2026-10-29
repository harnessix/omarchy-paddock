---
name: paddock-host
description: Use when you run inside a paddock sandbox and a task needs the host machine, for example a screenshot of the real screen, a command on the host, a file outside the project, the host clipboard, opening a URL in the host browser, git push, or a pull request with gh.
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

## The sandbox display

If the sandbox has its own display (for a desktop app that you develop), capture it with `paddock-screenshot` instead. That needs no approval.

## Answers

Exit code 3 means that the user denied the request. Plain `git push` exits with 1 instead and prints that the host refused. `paddock-host git push` exits with 3. Do not send it again. Ask in the conversation what the user wants instead. Exit code 5 means that nobody answered in time. Tell the user, and continue with other work. The full reference, with every option and exit code, is `paddock-host --help`.
