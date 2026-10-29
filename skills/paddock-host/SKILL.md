---
name: paddock-host
description: Use when you run inside a paddock sandbox and a task needs the host machine, for example a screenshot of the real screen, a command on the host, a file outside the project, the host clipboard, or opening a URL in the host browser.
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

If the sandbox has its own display (for a desktop app that you develop), capture it with `paddock-screenshot` instead. That needs no approval.

Exit code 3 means that the user denied the request. Do not send it again. Ask in the conversation what the user wants instead. Exit code 5 means that nobody answered in time. Tell the user, and continue with other work. The full reference, with every option and exit code, is `paddock-host --help`.
