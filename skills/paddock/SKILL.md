---
name: paddock
description: Use on the host, outside a sandbox, to install, set up, update, configure, troubleshoot, clean up or remove paddock, or to run a coding agent (Claude Code, Codex, Copilot CLI, Gemini CLI, Grok Build, opencode) or a desktop app such as VS Code in a paddock sandbox.
---

# paddock on the host

paddock runs a coding-agent harness with full auto-approval in a sandbox. The sandbox sees the project, a private home folder and package caches. It never sees the real home folder, `~/.ssh`, cloud credentials or browser profiles.

If the environment variable `PADDOCK` is `1`, you are inside a sandbox: use the `paddock-host` skill instead.

## Rules

You configure a security boundary for the user.

1. Make a change that weakens the sandbox only when the user asks for it in their own words. Then tell them what it allows. These changes are:
   - `paddock approve`, or `-y` / `--yes` on a run. The agent in the sandbox can write `paddock.toml`, so the user reads what it asks for.
   - A `[host]` capability set to `allow`, `network = "host"`, `--gui host`, `[mcp] allow`, `[network] host_ports`, `[env] pass` for a secret, or a wider `[exec]`.
   - `paddock approver enable`, `paddock gateway install`, and every command with `sudo`.
   - `[devices]`, `[kv] user = "allow"`, `[pastures.NAME]`, `[credentials.NAME]`, `[spawn]`, `[host] spawn = "allow"`, and `[privacy] telemetry = "vendor"`. The reference files say what each one allows.
2. Never answer a host request, a question of an agent or a credential start card for the user. Report pending ones. The user answers with `paddock approvals answer`, in the bar panel or on the card.
3. Before a command deletes data (`paddock gc --yes`, `paddock pasture remove --yes`, `paddock undo`, the data folders), show the user what goes and ask first.
4. Do not edit or delete files under `~/.local/share/paddock`, `~/.local/state/paddock` or `~/.cache/paddock` by hand. Do not remove homes, images or worktrees with `rm`, `docker rmi` or `git worktree remove`. Use the paddock commands.
5. Do not copy login files between a sandbox and the host. Do not send test requests to a vendor's endpoints with the user's login.
6. A harness needs a terminal of its own. Do not start an interactive harness from inside your own session. Give the user the command to run in a new terminal.

## Install

paddock supports Omarchy (Arch Linux with Hyprland). Other distributions are best effort. `omarchy plugin add` installs only the bar widget. The `paddock` command is a separate install: the prebuilt binary of the latest GitHub release (see the README), or `bin/install` from a clone.

```sh
git clone --branch v0.2.0 https://github.com/harnessix/omarchy-paddock   # the latest release
cd omarchy-paddock
bin/install                     # system packages with sudo, build, then paddock doctor
```

Tell the user before you run `bin/install`: it uses `sudo`. `bin/install --minimal` skips the desktop packages.

Then run the setup without questions. Name every step that the user asked for, and only those:

```sh
paddock setup --yes --shims --launchers --plugin   # shell commands, menu entries, bar widget
paddock setup --yes --backend podman               # the default backend
paddock setup --yes --app vscode=~/src/api         # a workspace folder for an app
paddock setup --yes                                # only show the state of each step
```

With `--yes`, `paddock setup` never runs `sudo`. It prints the package install command: give it to the user. Each step prints what it changed and how to undo it. `--no-shims`, `--no-launchers` and `--no-plugin` undo a step.

`--shims` makes `claude`, `codex` and the other harness commands start in a sandbox in bash and zsh, from new shells on. `--launchers` adds menu entries in a "Paddock" folder. `paddock refresh` adds entries for agents installed later. `PADDOCK_BYPASS=1 claude` runs a harness once without the sandbox.

`paddock update --check` shows whether a newer release exists. Ask the user before `paddock update --yes`: it replaces the binary and updates the bar widget. If a package owns the binary, `paddock update` refuses and prints what to run instead.

## Check

```sh
paddock doctor                  # dependencies, backends, harnesses, shell commands, privacy
paddock doctor --escape         # tries to escape the sandbox; every check must pass
paddock list                    # known harnesses and where they are installed
paddock explain claude          # every mount, variable and rule, and where it comes from
paddock boundary                # what the sandbox boundary denied
```

If a check fails, `paddock doctor` prints the install command for the missing package. A harness whose program paddock cannot find shows as `absent`.

## Run

```sh
paddock claude                  # Claude Code in the current project (the git root)
paddock codex ~/src/api         # another harness, another project
paddock claude -- --resume      # arguments after -- go to the harness
paddock claude --worktree       # a new branch in a new worktree; review the diff at exit
paddock app run vscode ~/src/api   # a desktop app in a sandbox
paddock pick claude             # choose a recent project in the terminal
paddock ps                      # running sandboxes
paddock stop 1a2b3c             # stop one, by the short id from paddock ps
paddock freeze 1a2b3c           # no CPU, memory kept; paddock thaw 1a2b3c continues it
```

A sandbox id such as `claude-app-1a2b3c` also works as `1a2b3c`, or as its first 4 or more characters (`1a2b`).

A harness command in the home folder stops with an error: the home folder is never a project.

## Configure

- `~/.config/paddock/config.toml` is the user configuration. Only the host edits it. It holds the `[host]` capability modes, `[env]`, `[apps.NAME]`, `[pastures]`, `[spawn]`, `[devices]`, `[credentials]` and defaults.
- `paddock.toml` in a project is the project configuration. `paddock init` writes a commented one. Entries that widen access need `paddock approve`.

| The user wants | Do this |
| :--- | :--- |
| No network, or only the model API | `paddock claude --network none`, or `--network api` (add hosts with `--allow`) |
| A desktop project with a visible window | `paddock claude --gui window` |
| An MCP server in the sandbox | `paddock mcp claude` shows why it is off. Then ask before you add `[mcp] allow` |
| Agents in sandboxes to know `paddock-host` | `paddock skill install` |
| To see what telemetry paddock turns off | `paddock doctor` (section "Privacy") |

After a configuration change, run `paddock explain HARNESS` and check that the change is there.

## More

Read the reference file when the task needs it:

- [references/omarchy.md](references/omarchy.md): only the agents that Omarchy starts in a sandbox, the agents panel, its usage numbers.
- [references/credentials.md](references/credentials.md): logins, a separate account, environment variables, `[credentials]`.
- [references/host-requests.md](references/host-requests.md): approvals, the audit log, `[exec]`, an agent approver, the gateway, spawn settings.
- [references/pastures.md](references/pastures.md): pastures, paddock kv on the host, timers, forage.
- [references/devices.md](references/devices.md): devices in `/dev/paddock` and questions of agents.
- [references/files-and-cleanup.md](references/files-and-cleanup.md): files outside the project, worktrees, undo, disk space, removing paddock.
