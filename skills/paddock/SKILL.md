---
name: paddock
description: Install, configure, troubleshoot, clean up or remove paddock on the host. paddock is a sandbox for coding agents (Claude Code, Codex, Copilot CLI, Gemini CLI, opencode) and desktop apps such as VS Code. Use it outside a sandbox, when the user asks for any of these tasks or asks to run an agent in a sandbox.
---

# Setting up and using paddock on the host

paddock runs a coding-agent harness with full auto-approval in a sandbox. The sandbox sees the project, a private home directory and package caches. It never sees the real home directory, `~/.ssh`, cloud credentials or browser profiles.

If the environment variable `PADDOCK` is `1`, you are inside a sandbox. Then use the `paddock-host` skill instead. This skill is for the host.

## Ground rules

You configure a security boundary for the user. Some changes make it weaker. For each change below, wait until the user asks for it in their own words. Then tell them what it allows:

- Answering a host request: `paddock approvals answer`. The user answers their own requests.
- Approving a project file: `paddock approve`, or `-y` / `--yes` on a run. The agent in the sandbox can write `paddock.toml`, so the user must read what it asks for.
- An agent approver: `paddock approver enable`, and anything with `sudo`.
- Setting a capability to `allow` in `[host]`, `network = "host"`, `--gui host`, `[mcp] allow`, `[network] host_ports`, or `[env] pass` for a secret.

Do not edit files under `~/.local/share/paddock`, `~/.local/state/paddock` or `~/.cache/paddock` by hand. Use the commands.

## Install

paddock supports Omarchy (Arch Linux with Hyprland). Other distributions are best effort.

`omarchy plugin add` installs only the bar widget. The `paddock` command is a separate install: the prebuilt binary from the latest GitHub release (see the README), or `bin/install` from a clone.

```sh
git clone https://github.com/harnessix/omarchy-paddock
cd omarchy-paddock
bin/install                     # system packages, build, then paddock doctor
```

`bin/install` uses `sudo` for system packages. Tell the user before you run it. Use `bin/install --minimal` to skip the desktop packages.

Then run the setup without questions. Name every step that the user asked for, and only those:

```sh
paddock setup --yes --shims --launchers --plugin   # shell commands, launchers, bar widget
paddock setup --yes --backend podman               # the default backend
paddock setup --yes --app vscode=~/src/api         # a workspace folder for an app
paddock setup --yes                                # only show the state of each step
```

With `--yes`, `paddock setup` prints the package install command and never runs `sudo`. Give that command to the user. Each step prints what it changed and how to undo it, and a second run changes nothing. `--no-shims`, `--no-launchers` and `--no-plugin` undo a step.

## Update

```sh
paddock update --check          # is there a newer release, and what would an update do
```

Ask the user before you run `paddock update --yes`: it replaces the paddock binary and updates the bar widget. If a pacman package owns the binary, `paddock update` refuses and prints what to run instead (pull the clone and run `bin/install` again). `paddock doctor` also says when a newer release exists.

## Check

```sh
paddock doctor                  # dependencies, backends, found harnesses, shell commands
paddock doctor --escape         # tries to escape the sandbox; every check must pass
paddock list                    # known harnesses and where they are installed
```

If a check fails, `paddock doctor` prints the install command for the missing package. If paddock cannot find the program of a harness, the harness shows as `absent`.

## Make the sandbox the default

Most users never type `paddock`. Turn on both of these, and tell the user what they do:

```sh
paddock setup --yes --shims --launchers
```

`--shims`: `claude`, `codex` and others start in a sandbox in bash and zsh. `--launchers`: menu entries in a "Paddock" folder (Super+Space on Omarchy). `paddock shims install` and `paddock launchers install` do the same one at a time.

`paddock shims install` adds one marked line to `~/.bashrc` and `~/.zshrc`. It takes effect in new shells. `PADDOCK_BYPASS=1 claude` runs a harness once without the sandbox. A harness command in the home directory stops with an error, because the home directory is never a project.

## Run

```sh
paddock claude                  # Claude Code in the current project (the git root)
paddock codex ~/src/api         # another harness, another project
paddock claude -- --resume      # arguments after -- go to the harness
paddock claude --worktree       # a new branch in a new worktree; review the diff at exit
paddock app run vscode ~/src/api   # a desktop app in a sandbox
paddock ps                      # running sandboxes
paddock stop <id>               # stop one
paddock explain claude          # every mount, variable and rule, and where it comes from
```

A harness needs a terminal of its own. Do not start an interactive harness from inside your own session. Give the user the command to run in a new terminal. `paddock pick <harness>` in a terminal lets them choose a recent project.

## Configure

There are two files:

- `~/.config/paddock/config.toml` holds the user configuration. Only the host edits it. It sets the `[host]` capability modes, `[env]`, `[apps.NAME]` and defaults.
- `paddock.toml` in a project holds the project configuration. `paddock init` writes a commented one. Entries that widen access need the user's approval with `paddock approve`.

Common tasks:

| The user wants | Do this |
| :--- | :--- |
| A separate account for a harness | `paddock claude --auth isolated:work`, then log in inside the sandbox |
| To stop passing a variable | `paddock env hide NAME` |
| To see which variables pass | `paddock env show` |
| An MCP server in the sandbox | `paddock mcp claude` shows why it is off. Then ask the user before you add `[mcp] allow` |
| A desktop project with a visible window | `paddock claude --gui window` |
| No network | `paddock claude --network none` |
| Agents in the sandbox to know `paddock-host` | `paddock skill install` |

After a configuration change, run `paddock explain <harness>` and make sure that the change is there.

## Host requests

An agent in a sandbox can ask the host for a screenshot, one command, one file, the clipboard, or a link in the browser. The user answers on the approval card, in the bar panel, or with `paddock approvals`. `paddock log` shows every request and its answer. Report pending requests to the user. Do not answer them.

## Disk space

You can run `paddock du` and a `paddock gc` dry run at any time. Neither command changes anything. Add `--json` for data. A `paddock gc` without `--yes` only prints a list, also with `--homes`, `--worktrees` or `--include-app-logins`.

Ask the user before you run `paddock gc` with `--yes`. Show them the dry-run list first, and say which items they lose:

- `--homes` deletes transcripts and settings of old projects.
- `--include-app-logins` deletes the login of a desktop app. The user must log in again.
- `--worktrees --force` deletes uncommitted changes in a worktree. Its branch and commits stay.

Do not delete sandbox homes, images or worktrees with `rm`, `docker rmi` or `git worktree remove`. `paddock gc` checks that no sandbox runs in them.

## Remove

```sh
paddock setup --yes --no-shims --no-launchers --no-plugin
```

Then delete `~/.local/bin/paddock`, or run `sudo pacman -R omarchy-paddock` if `bin/install` installed the Arch package. The data directories are `~/.local/share/paddock`, `~/.cache/paddock`, `~/.local/state/paddock` and `~/.config/paddock`. They hold sandbox homes and isolated logins, so ask the user before you delete them. To free space and keep paddock, use `paddock gc` instead.
