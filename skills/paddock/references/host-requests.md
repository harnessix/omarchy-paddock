# Host requests, approvals and spawn

## Requests from a sandbox

An agent in a sandbox can ask the host for a screenshot, one command, one file, the clipboard, or a link in the browser (`paddock-host`). The mode of each capability comes from `[host]` in `~/.config/paddock/config.toml`: `deny`, `ask` or `allow`.

- `paddock approvals` lists the requests that wait. Report them to the user. Do not answer them.
- The user answers with `paddock approvals answer ID once|session|deny|deny-session`, in the bar panel or on the approval card. With `deny`, `--text "..."` gives the agent a note.
- `paddock log` shows every request and its answer.

## Commands on the host

An approved `exec` runs in an isolated unit: the project is writable, the home folder is empty, and only `PATH` and the language pass. `[exec]` widens it: `home = "read-only"` or `"full"`, more `paths`, more `env` names. Then an approved command sees more of the host. Change it only when the user asks.

## Agent approver

`paddock approver enable --capabilities screenshot,open-url -- /absolute/command` lets a command on the host, usually a reviewing agent, answer requests in the user's place. Without `--system`, a password protects it. With `--system` it needs `sudo`. Run it only when the user asks. `paddock approver show` and `paddock approver test` check it, and `paddock approver disable` turns it off.

## Gateway

`paddock gateway install` writes two systemd user units, so that systemd starts the executor of approved requests on the first request. Without it, paddock starts the executor itself, and nothing breaks. `paddock gateway status` shows the state, and `paddock gateway remove` undoes it.

## Spawn

An agent in a sandbox can start an agent of another harness in a second sandbox (`paddock-host spawn`). It is off until the user lists harnesses in `[spawn] harnesses` of the user configuration. A project `paddock.toml` cannot turn it on.

Before you add a harness there, ask the user and explain: the child uses the user's own login and quota of that harness, it never gets more access than its parent, and it stops when its parent stops.

| Setting | Default |
| :--- | :--- |
| `[host] spawn` | `"ask"` |
| `[spawn] max_running` | 2 children at the same time |
| `[spawn] max_depth` | 1: a child cannot spawn |
| `[spawn] timeout` | 1800 seconds |
| `[spawn] on_timeout` | `"stop"`; `"ask"` freezes the child and asks the user for more time |
| `[spawn] max_mem`, `max_cpus` | no limit; they need `systemd-run --user` |

`paddock ps` shows children under their parent, and `paddock stop PARENT` stops them too.

`paddock view ID` opens a terminal with a read-only live view of a sandbox without a terminal, such as a spawned child (`--write` for keys). It needs tmux for a live session and shows the run log without it.
