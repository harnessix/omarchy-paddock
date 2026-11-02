---
name: paddock-host
description: Use inside a paddock sandbox (PADDOCK=1) when a task needs the host machine, for example a screenshot of the real screen, a host command or file, the host clipboard, a URL in the host browser, git push or a pull request with gh, another harness (spawn), state shared with other agents (paddock kv, pastures), a task timer, or a device in /dev/paddock.
---

# Working from a paddock sandbox

You are in a paddock sandbox when the environment variable `PADDOCK` is `1`. The sandbox holds the project, your own home folder and package caches. The rest of the host is not there. Do not search for host files, and do not try to reach host services. For paddock on the host, use the `paddock` skill.

## Ask the host

1. Run `paddock-host capabilities --json` once. It lists each capability and its mode: `allow`, `ask` or `deny`. Do not use a capability in `deny` mode.
2. Send one request with `--reason "..."`: one short sentence that tells the user why you need it.
3. Wait until the command ends. A request in `ask` mode waits for the user's answer.
4. Read the result. Files go to `.paddock/inbox/` in the project, and the command prints the path.

| Need | Command |
| :--- | :--- |
| See the real screen | `paddock-host screenshot --reason "..."` (`--target window`: the focused window) |
| See why something failed: a missing path, a refused host, a withheld variable | `paddock-host boundary` (read-only, no approval) |
| Run one command on the host | `paddock-host exec --reason "..." -- COMMAND ARG...` |
| Run a host tool that the user defined | `paddock-host tool --list`, then `paddock-host tool NAME ARG...` |
| Get a host file outside the project | `paddock-host fetch /absolute/path --reason "..."` |
| Change a host folder outside the project | `paddock-host borrow /absolute/folder --reason "..."`, change the copy in `/run/paddock/borrow/NAME`, then `paddock-host return NAME --reason "..."` |
| Use the host clipboard | `paddock-host clipboard get` or `paddock-host clipboard set TEXT` |
| Open a page in the host browser | `paddock-host open https://...` |
| Tell the user that work is done | `paddock-host notify "Title" "Body"` |
| Push a branch, open a pull request | plain `git push` and `gh` (see [git and forges](references/git-and-forges.md)) |
| Give a task to another harness | `paddock-host spawn` (see [spawn](references/spawn.md)) |
| See your timers and limits | `paddock-host status` |
| See the sandbox's own display | `paddock-screenshot` (no approval) |

`exec` runs in an isolated unit on the host: the project is writable, the home folder is empty, and only `PATH` and the language pass from the host environment. A command that needs your keys, another folder or a token fails there. Tell the user what it needs. The user decides whether to widen `[exec]`.

## Answers and exit codes

- 0: done. `exec`, `gh`, `tea` and `glab` exit with the command's own code.
- 3: the user denied the request. If the denial carries a note, follow the note and do not ask for the same thing again. Without a note, do not send the request again. Ask in the conversation what the user wants instead.
- 4: the capability is `deny` in the user's configuration. Do not send it again.
- 5: nobody answered in time. Tell the user, and continue with other work.
- 75: still waiting (see "Waiting"). Run the same command again.

Plain `git push` exits with 1 when the host refuses. `paddock-host --help` lists every code.

## Secrets

Host variables that look like secrets (tokens, passwords, API keys) stay on the host. Your harness gets only its own login variables. If a task needs another token, do not search for it. Tell the user which variable it needs.

- A token whose value contains `PADDOCK` is a stand-in. It works only through the base URL that paddock set (`http://127.0.0.1:3130/s/NAME`), for the hosts and paths that the user bound it to. Do not send it anywhere else.
- A credential that the user gave as a file is in `/dev/paddock/secret/NAME`. Read it in one command only, for example `GH_TOKEN=$(cat /dev/paddock/secret/GH_TOKEN) gh pr list`. Never copy it into files or logs. If the file is gone or a read fails with EACCES, the user revoked it.

## Waiting

Never wait with `sleep`. Use a blocking read: it returns the moment the event happens.

| Wait for | Command |
| :--- | :--- |
| A key or a prefix in paddock kv | `paddock-host kv watch --prefix result: --scope pasture --timeout 100` |
| A child that you started | `paddock-host wait ID --timeout 100` |
| A time (device `wait`) | `paddock-host device wait 2026-10-30T15:00:00Z --timeout 100` |
| An answer of the user (device `ask`) | `paddock-host device ask "QUESTION" --choice yes --choice no --timeout 100` |

1. Choose a `--timeout` below the time limit of your shell tool call. 100 seconds is safe for a limit of two minutes.
2. Exit code 0: the event happened. Read the output.
3. Exit code 75: the timeout passed first. Run the same command with the same arguments again.

A request that waits for the user stays one request. If you send the identical request again, it joins the one that waits, and the user sees one card.

## Long tasks

A timer lives on the host, so it outlasts your context.

1. When a task can take long, run `paddock-host timer start NAME --budget 2h` or `--quota weekly=1%`.
2. Run `paddock-host timer status` after a compaction, when you resume work, and before you try one more fix for the same bug.
3. If most of the budget is used and the bug is still there, stop and report what you tried.

## More

Read the reference file when the task needs it:

- [references/git-and-forges.md](references/git-and-forges.md): before `git push`, `gh`, `tea` or `glab`.
- [references/spawn.md](references/spawn.md): before you start an agent of another harness, or when you are such a child.
- [references/kv.md](references/kv.md): before you share tasks, results or notes with other agents.
- [references/pastures.md](references/pastures.md): when `paddock-host kv info` says that you are in a pasture: the shared folder, shared ports, questions to other members, the workbench.
- [references/timers.md](references/timers.md): timers set by others, more time or quota, rations, `paddock-host forage`.
- [references/devices.md](references/devices.md): before you use a device in `/dev/paddock`.

`paddock-host --help` and `paddock-host help CAPABILITY` give every option.
