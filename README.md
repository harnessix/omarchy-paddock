# paddock

<p align="center">
  <img src="docs/assets/hero.png" alt="paddock: full auto-approval, nothing outside the fence" width="100%">
</p>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-192327" alt="License: MIT"></a>
</p>

paddock runs coding agents with full auto-approval inside a sandbox: terminal harnesses such as Claude Code, Codex, Antigravity, Gemini CLI, Grok Build, opencode and Copilot CLI, and desktop apps such as ChatGPT, Claude Desktop and VS Code. The sandbox contains only what the agent needs: the project, the harness, a private home directory, and package caches. Everything else on the host is absent from the file system. The agent cannot read it, so you do not need to approve each command.

```sh
cd ~/src/my-app
paddock claude
```

> [!WARNING]
> By default, the sandbox inherits your host environment variables, and the agent can read every token in them. paddock prints the names of the variables that look like secrets. High-privilege cloud credentials (AWS, Azure, Google Cloud, Vault, Terraform and others) are not passed unless you name them in `[env] pass`. To drop single variables, hide them. paddock saves the list in `~/.config/paddock/config.toml`.

```sh
paddock env hide GITHUB_TOKEN OPENAI_API_KEY
paddock env unhide OPENAI_API_KEY
paddock env show
```

To pass only an allowlist, run `paddock claude --env clean`, or set `[env] inherit = false`. To drop single variables, run `paddock env hide NAME`.

## See it

`paddock claude` opens Claude Code's own TUI, as you know it, with full auto-approval. Inside the paddock, the agent cannot see the SSH key on the machine, the `.env` in the project is empty for it, and it still runs commands and writes code without asking for approval.

<p align="center">
  <img src="docs/assets/demo.gif" alt="A terminal recording: paddock claude opens Claude Code's TUI with bypass permissions on; the agent runs a shell command and writes src/greet.rs without approval prompts, and reports that the SSH key does not exist and .env is empty; paddock explain then lists the mounts" width="100%">
</p>

## Why

Harnesses tie two settings together in the wrong direction. Their built-in sandboxes restrict commands, so builds and tests fail for missing tools. Their auto-approval modes turn that sandbox off, so commands run on the host. The help text of Codex says that its bypass flag is "intended solely for running in environments that are externally sandboxed". paddock is that external sandbox.

paddock moves the boundary from "which commands can run" to "which files exist". Inside the boundary, the agent has every tool and approves itself. The boundary is kernel namespaces, which the agent cannot negotiate with.

## Install

### Quick start

1. On Omarchy, add the bar widget. It installs only the widget, not the `paddock` command:

   ```sh
   omarchy plugin add https://github.com/harnessix/omarchy-paddock --enable
   ```

2. Install the `paddock` command from the latest release, v0.1.7: the prebuilt binary, or `bin/install` from a clone of that tag. Both are described below.

3. Run the setup. It checks the system packages, and offers the shell commands, the launchers, the bar widget and the default backend, one step at a time. Each step says what it changes and how to undo it. A second run shows the state and changes nothing unless you ask. The last step lists where you turn off the use of your data for training (see [Privacy defaults](#privacy-defaults)).

   ```sh
   paddock setup
   ```

   Without questions, for scripts and agents, name each step: `paddock setup --yes --shims --launchers --no-plugin --backend bwrap`. `--app vscode=~/src/api` saves a workspace folder for an app. With `--yes`, the setup only prints the command that installs missing packages and never runs `sudo`.

### From a clone or a release

paddock is a Rust command and an Omarchy bar plugin in one repository. The commands below install the latest release, v0.1.7. The `main` branch can hold changes that are not released yet.

Clone the release tag, and run the installer from its root. It installs the system packages, builds paddock, and runs `paddock doctor`.

```sh
git clone --branch v0.1.7 https://github.com/harnessix/omarchy-paddock
cd omarchy-paddock
bin/install
```

Without a Rust toolchain, use the prebuilt binary of the release. It is one file. Download it with its checksum, check the checksum, copy the binary to `~/.local/bin`, and run `paddock doctor`, which prints the command that installs the system packages below:

```sh
curl -fLO https://github.com/harnessix/omarchy-paddock/releases/download/v0.1.7/paddock-0.1.7-x86_64-linux.tar.gz
curl -fLO https://github.com/harnessix/omarchy-paddock/releases/download/v0.1.7/paddock-0.1.7-x86_64-linux.tar.gz.sha256
sha256sum -c paddock-0.1.7-x86_64-linux.tar.gz.sha256
tar -xzf paddock-0.1.7-x86_64-linux.tar.gz
install -m 755 paddock-0.1.7-x86_64-linux/paddock ~/.local/bin/paddock
paddock doctor
```

All releases are on the [releases page](https://github.com/harnessix/omarchy-paddock/releases).

On Arch Linux, the installer installs a package with makepkg. It also knows apt and dnf, and then copies the binary to `~/.local/bin`; that path is best effort and is not tested on every release (see [Support scope](#support-scope)). Use `bin/install --minimal` to skip the desktop packages.

| Package | Needed for |
| :--- | :--- |
| git | project and worktree detection (required) |
| bubblewrap | the default backend (required) |
| passt | outbound network on the bwrap backend (required) |
| iproute2 | hiding host addresses on the bwrap backend (required) |
| weston | displays for desktop projects, and `paddock-screenshot` |
| xwayland | X11 apps on the private display |
| dbus | a private session bus for desktop apps |
| docker or podman | the container backend |
| mise | per-project toolchains from `mise.toml` |
| gnome-keyring | a private keyring for app sandboxes and for harnesses that need one, such as Copilot CLI |

`paddock doctor` prints the same list with the state of each package and the install command for your distribution.

To add the bar widget on Omarchy, point `omarchy plugin add` at this repository:

```sh
omarchy plugin add https://github.com/harnessix/omarchy-paddock --enable
```

`omarchy plugin add` always installs the widget from the default branch, because it takes no tag. `paddock update` updates the widget together with the command.

The widget lists running sandboxes with their backend, network, display and login. Press Enter on a sandbox to go to the terminal that runs it (Hyprland). The row actions open the project folder, copy its path, show the display window of a sandbox in `window` mode, and stop the sandbox after a second press. The New button starts a sandbox in two steps: pick a recent project, then a harness that is installed on this machine, and paddock opens a terminal that runs it. The Checks button runs `paddock doctor`. Every action has a key, and the panel footer lists them.

### Update

```sh
paddock update --check    # is there a newer release, and what an update would do
paddock update            # asks first; --yes updates without asking
```

`paddock update` asks the GitHub API for the latest release. It downloads the tarball and its `.sha256` into a private folder, checks the checksum, and checks that the new binary prints the expected version. Then it writes the new binary next to the old one and renames it into place, so a cut download never leaves half a binary. It talks only to `api.github.com`, `github.com` and GitHub's download hosts over HTTPS, follows no redirect to another host, and uses your `HTTPS_PROXY`. If the bar widget is installed, it also runs `omarchy plugin update harnessix.paddock --yes`.

If a package owns the binary (the Arch package that `bin/install` builds), `paddock update` does not overwrite it. In your clone, check out the new release tag and run `bin/install` again, for example `git fetch --tags && git checkout vX.Y.Z && bin/install`, with the tag that `paddock update --check` names. `paddock update` runs only on the host, never in a sandbox.

`paddock doctor` says when a newer release exists. It checks at most once a day and waits at most two seconds; `PADDOCK_NO_UPDATE_CHECK=1` turns the check off. It also says when a desktop app was updated on the host while its sandbox runs, so that you restart it.

### Let your agent set it up

paddock ships two agent skills. Install them with the [skills](https://github.com/vercel-labs/skills) command, which supports Claude Code, Codex, Copilot, Gemini CLI, opencode and other agents:

```sh
npx skills add harnessix/omarchy-paddock
```

- `paddock` is for an agent on the host. It covers install, `paddock doctor`, shell commands, launchers, configuration and removal. It tells the agent to leave every change that weakens the sandbox to you: approvals, `[host]` modes set to `allow`, host network, and agent approvers.
- `paddock-host` is for an agent inside a sandbox. It explains how to ask the host for a screenshot, a file or a command, and how `git push` and `gh` work there. `paddock skill install` installs this one too.

Then ask your agent, for example: "Install paddock and make the sandbox the default for my agents."

## Remove

1. Remove the bar widget:

   ```sh
   omarchy plugin remove harnessix.paddock
   ```

2. If you turned them on, remove the shell commands and the launchers. Do this before you remove the command:

   ```sh
   paddock shims remove
   paddock launchers remove
   ```

   `paddock setup --yes --no-shims --no-launchers --no-plugin` does steps 1 and 2 in one command.

   If you forget this step, nothing breaks. The line in `~/.bashrc` runs only when its script exists, and each command falls back to the real program when paddock is gone. The launcher entries stay in the menu until you delete them.

3. Remove the command. If you installed the Arch package (`bin/install` does this on Arch Linux), run `sudo pacman -R omarchy-paddock`. Otherwise, delete `~/.local/bin/paddock`.

4. Optional: to free space and keep paddock, run `paddock gc` instead. See [Disk space](#disk-space).

5. Optional: delete the data that paddock keeps. These directories hold the sandbox homes, isolated logins, shared caches, worktrees and settings. Your own logins and files are not in them.

   ```sh
   rm -rf ~/.local/share/paddock ~/.cache/paddock ~/.local/state/paddock ~/.config/paddock
   ```

   If you set `XDG_DATA_HOME`, `XDG_CACHE_HOME`, `XDG_STATE_HOME` or `XDG_CONFIG_HOME`, the directories are under those paths instead.

6. Optional: if you used the docker or podman backend, run `paddock gc --yes` before you remove the command. It removes the old paddock images and paddock's build cache. To remove the current images too, run `docker image rm $(docker image ls -q paddock-base)`, and the same command with `podman`.

paddock does not change any file outside these directories, except the files that you name in `paddock env hide` (in `~/.config/paddock/config.toml`), the git worktrees that `--worktree` creates, the files that `paddock shims install` and `paddock launchers install` change (also through `paddock setup`), and the paddock binary that `paddock update` replaces. Step 2 undoes the shell commands and launchers. To remove a worktree, use the commands that paddock prints when the sandbox exits.

## Use

```sh
paddock claude                 # Claude Code in the current project
paddock codex ~/src/api        # Codex in another project
paddock claude -- -p "fix the failing test"   # arguments after -- go to the harness
paddock claude --worktree      # work on a new branch in a new worktree, review the diff at exit
paddock shell claude           # a shell in the sandbox that claude gets
paddock explain claude         # every mount, variable and rule, and why it is there
paddock ps                     # running sandboxes
paddock files                  # files a sandbox saved outside the project
paddock doctor                 # dependencies, backends, located harnesses, training opt-outs
paddock doctor --escape        # try to escape, and report each attempt
paddock setup                  # first-run steps; a second run changes nothing
paddock update --check         # is there a newer release
paddock mcp claude             # MCP servers, and whether each runs in the sandbox
paddock env show               # which host variables pass, by name
```

A sandbox uses your existing login of the harness. See [Logins and MCP](#logins-and-mcp) for a separate login.

## What the sandbox contains

| Inside the sandbox | Source on the host |
| :--- | :--- |
| The project (git root), read-write | the project directory |
| `.git`, read-write, its own mount point | the same directory. A mount point cannot be renamed, so the read-only files below it stay in place. |
| `.git/hooks`, `.git/config`, `.git/config.worktree`, `.git/commondir`, `.git/objects/info/alternates`, read-only | the same files. Git on the host reads them, and they can name a program or another directory. |
| `$HOME`, read-write | a private directory per harness, login and project, under `$XDG_DATA_HOME/paddock/homes` |
| Credential files of the harness | copied from your home directory, or from an isolated store |
| Harness settings and skills, read-only | the files that the harness profile lists in `import_ro` |
| The harness program, read-only | its install directory, found through PATH or mise |
| Package caches (cargo, npm, uv, go) | shared caches under `$XDG_CACHE_HOME/paddock/shared` |
| mise tools from the registry, read-only | `$XDG_CACHE_HOME/paddock/shared/mise-tools` |
| `/usr`, `/etc`, read-only (bwrap backend) | the host system |

The sandbox never contains your real home directory, `~/.ssh`, `~/.gnupg`, cloud credentials, browser profiles, the container runtime socket, or the session bus. Files with credential-shaped names in the project (`.env`, `*.pem`, `*.key`) are hidden. The policy engine refuses any configuration that breaks these rules, and names the line that asked for it.

### Git on the host

Git takes commands from the repository it reads. `core.fsmonitor`, `core.hooksPath`, a hook and a `filter` driver all start a program. The agent writes the project, so paddock treats every file in `.git` as hostile.

- paddock finds the repository with the file system: it walks up and looks for a real `.git`. It never believes `core.worktree`, `commondir` or a `.git` file that names another directory. A layout that does not match is refused with a message that says what is wrong.
- Every git call that paddock makes on a project names the git directory directly, drops your global and the system configuration, and turns off the settings that can start a program.
- Work-tree state, such as the list of uncommitted changes, comes from a `git status` that runs in a sandbox of its own, with no network. A `filter` driver cannot be turned off with a flag, so it runs in there, where it can do no more than the agent already can. On a host without bubblewrap, paddock says the state is unknown instead of guessing, and `paddock gc` then asks for `--force` before it removes such a worktree.
- `git worktree add` and `git worktree remove` write a work tree. Before one of them runs, paddock reads the repository configuration as text. If it holds a key that starts a program, paddock stops and prints the file and the key.
- Your own `git` commands in the project are protected by the read-only binds in the table above, as long as paddock started the sandbox. A repository that the agent creates inside the project is new, so it has no such binds: paddock's own calls stay safe, but take care with your own.
- The first start in a repository creates `.git/commondir` with `.`, and empty `.git/config.worktree` and `.git/objects/info/alternates` files, if they are missing. Git treats all three as no-ops. They exist so that paddock can bind them read-only; otherwise the agent could create them and point git elsewhere. While a repository is bound this way, `git config --worktree` cannot write.
- `paddock doctor --escape` checks all of this on every backend: that `.git` cannot be renamed in the sandbox, that the pointer files are read-only, and that a planted setting or hook does not run during paddock's host git calls.

### mise toolchains

If the project has a `mise.toml` or `.tool-versions` file, paddock installs its tools before the harness starts. This happens in two steps.

1. paddock reads the tool list on the host. Tools from mise's registry that use the `core` or `aqua` backends, with a plain version, go to the shared installs. Examples are `node = "22"`, `python = "3.12"` and `jq = "1.7.1"`. A clean sandbox installs them. That sandbox has no project, no project config and no sandbox home, so hooks, `[env]` scripts and tasks cannot run while the shared installs are writable.
2. `mise install` runs in the project sandbox with the project config. It installs every other tool into the private sandbox home, for example `cargo:`, `npm:`, `ubi:` or plugin tools, and tools with options. The shared installs are read-only in this step, and also while the harness runs.

If the shared installs already hold every version, paddock skips the first step. This does not apply to `latest`. Every project uses the shared tools. A tool in the sandbox home belongs to one project and its harness. If you set `mise_install = false` in `[setup]` of `paddock.toml`, paddock skips both steps. The srt backend cannot mount the shared installs, so it installs every tool in the sandbox home.

## Files outside the project

The project is shared with the host, so the agent's changes there are already yours. A file that the agent saves anywhere else, for example to `~/Desktop` or `~/Downloads`, stays in the private sandbox home. You do not see it until you recover it.

`paddock files` lists these files for each sandbox, also after the sandbox exits. It also shows the commits and the changed paths in the project since the sandbox started.

```sh
paddock files                                 # every sandbox that has files
paddock files claude-app-1a2b3c               # one sandbox
paddock recover claude-app-1a2b3c Desktop/report.pdf          # to ~/Desktop/report.pdf
paddock recover claude-app-1a2b3c --all --to ~/Inbox
paddock discard claude-app-1a2b3c Downloads/setup.zip
```

`paddock recover` copies the file to the host, then deletes the sandbox copy. Add `--keep` to keep it. The rules:

- A recovered file never replaces a file. If the name is taken, the file gets a new name, such as `report (1).pdf`.
- paddock never writes into a hidden path of your home, such as `~/.ssh` or `~/.config`, into its own directories, or into system directories. It refuses, even when you ask for it.
- paddock copies regular files only. It skips symlinks, devices and pipes, and it never follows a symlink in the sandbox home. A recovered file is never executable: paddock drops the execute bits, so one click on the host cannot run it.

The Omarchy panel lists the same files. Press Enter on a file to recover it. When a sandbox saves a new file, a desktop notification tells you. The notification does not take the keyboard.

paddock looks in the recovery folders only. The defaults are `Desktop`, `Documents`, `Downloads`, `Pictures`, `Videos` and `Music`, with all their subfolders. A folder of your host that has another name, set in `~/.config/user-dirs.dirs`, is the target. Change the folders in `~/.config/paddock/config.toml`. A project `paddock.toml` cannot change them, because the agent can edit it.

```toml
[recovery]
folders = ["Desktop", "Downloads", "out"]
ignore = ["*.desktop", "*.part", "*.tmp"]   # never listed
```

By default, paddock ignores `*.desktop` launchers, because they can run commands when you open them, and partial or lock files.

## Disk space

Each harness gets its own home in each project, so the homes grow with the number of projects. The docker and podman backends also keep one image for each configuration.

`paddock du` lists what paddock stores and how much space it uses:

- Each sandbox home: harness, login, project, size, last use, and what you lose when you delete it.
- The paddock images of docker and podman, and whether each is current or in use.
- The worktrees that `--worktree` created, with their unmerged commits.
- The session records of `paddock files`, and the keyring passwords of app homes.

On btrfs, a size counts only the data that no other file shares. On other file systems, it counts the allocated blocks, as `du` does. The first line names the kind. `paddock du --json` prints the same data as JSON.

`paddock gc` removes what is no longer needed. Without `--yes`, it removes nothing and prints the list.

```sh
paddock gc                                  # what --yes would remove
paddock gc --yes                            # remove the automatic items
paddock gc --homes --older-than 30          # also list homes unused for 30 days
paddock gc --homes --yes                    # remove them
```

`--yes` alone removes these items:

- Paddock images that are not current and that no container uses. Images share layers, so `gc` counts only the part that belongs to one image alone.
- paddock's own records in the docker build cache. paddock marks its build steps, so the build cache of your other projects stays. A current image keeps its own layers, so this only makes the next image build slower.
- Keyring passwords whose home is gone.
- Session records that ended more than 30 days ago.
- Homes of harnesses whose profile no longer exists.
- Old shared caches that paddock no longer uses, such as `~/.cache/paddock/shared/mise-installs`. paddock keeps them while a sandbox runs.

paddock also does this after it builds a new image. To turn that off, set `PADDOCK_GC_AFTER_BUILD=0`.

The other items need a flag that names them:

- `--homes` adds homes whose project is gone, or that were not used for `--older-than` days (default 90). A home holds the transcripts and settings of a harness in one project. A terminal harness keeps its login in your home or in `~/.local/share/paddock/auth`, so the login stays.
- A desktop app keeps its login in its home. If you delete the home, you must log in again. These homes also need `--include-app-logins`.
- `--worktrees` adds paddock worktrees. A worktree with unmerged commits or uncommitted changes also needs `--force`. paddock then keeps its branch, so the commits stay in the repository.
- `--scratch` adds scratch projects that were not used for `--older-than` days. An agent from `omarchy-agent` works in one when it starts in the home folder (see [Only the agents that Omarchy starts](#only-the-agents-that-omarchy-starts)). Its files are lost.

paddock never removes the home or worktree of a running sandbox. It deletes only inside its own directories. It never follows a symlink, so a link in a sandbox home cannot point it at your files. A sandbox can edit the `.paddock-origin` file in its home, which records the project. paddock uses that file only when it matches the folder name of the home, which contains a hash of the project path.

## Network

| Mode | Behavior |
| :--- | :--- |
| `egress` (default) | Internet access that leaves the same way as host traffic. Host services and the LAN are not reachable. |
| `none` | No network. |
| `host` | The host network namespace. Host-local services are reachable. Needs approval in `paddock.toml`. |

Many users depend on a system proxy or a TUN device for a stable exit address, and an account can be flagged when that address changes. So paddock never gives a sandbox a bridged network.

On the bwrap backend, `egress` uses pasta. pasta makes each outbound connection as an ordinary socket of a host process, so the connection follows the host routes, the TUN device, and the host DNS resolver. If the host sets `HTTPS_PROXY` or a similar variable, the sandbox gets the same value, and a proxy on the host loopback is the one loopback port that the sandbox can reach. paddock turns off the other port forwarding of pasta, and makes every address of the host unreachable inside the sandbox, so services on the host stay hidden.

The LAN is blocked too. A sandbox cannot reach private and link-local networks: 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16, 169.254.0.0/16, 100.64.0.0/10 (VPN peers such as Tailscale), and the IPv6 ranges fc00::/7 and fe80::/10. So your router, NAS, printers and other machines stay out of reach. A host proxy on the LAN stays reachable, because all traffic goes through it. To let a project reach the LAN, set this in `paddock.toml` (it needs your approval) or in `~/.config/paddock/config.toml`:

```toml
[network]
lan = true
```

On the docker and podman backends, the container has no network interface. Its only way out is a Unix socket to a proxy in the paddock process on the host. That proxy connects like a host program, through the host proxy variables or directly. It refuses connections to the host itself. Programs that ignore `HTTPS_PROXY` get no network, so no traffic leaves by another path.

A host proxy URL that starts with `https://` means TLS to the proxy itself. The paddock proxy then opens a TLS connection to that proxy and checks its certificate against the system trust store. A private CA that you installed system-wide works, and so do `SSL_CERT_FILE` and `SSL_CERT_DIR`. The request and the user name and password from the URL go only inside TLS. If the TLS connection fails, for example because of a certificate error, the request fails. paddock never falls back to plain TCP. A URL that starts with `http://` or `socks5://` sends the user name and password in cleartext, because those protocols do that, for host programs too. paddock allows this, but it logs a warning when the proxy is not on the host loopback. Use an `https://` proxy URL to protect them.

Who talks to the host proxy depends on the backend. On docker and podman, the paddock proxy described above does. On bwrap, the sandbox gets the host proxy variables, and the harness in the sandbox connects to the proxy itself, through pasta. The harness then handles `https://` proxy URLs and certificates the way it does on the host. On srt, the srt proxy on the host handles the traffic, and the paddock proxy is not used.

`paddock doctor --escape --network egress --online` checks all of this: DNS and HTTPS work, and the host loopback, the other host addresses, and direct connections that bypass the proxy do not.

## Logins and MCP

By default, a sandbox uses your existing login (auth mode `shared`). paddock copies the credential files of the harness from your home directory into the sandbox at start. If the harness refreshes a token in the sandbox, paddock writes it back when the sandbox exits. If the file also changed on the host in the meantime, the host version stays, and paddock tells you.

Some harnesses keep their login in the system keyring. Copilot CLI is one of them. Its sandbox gets a private keyring of its own (gnome-keyring, on a private session bus), encrypted with a password that paddock keeps on the host. The sandbox cannot reach your host keyring, and your host keyring does not see the sandbox's login. So you log in once inside the sandbox, and the login stays for later runs.

For a separate account, use an isolated login. The sandbox then starts logged out, and you log in inside it. Each name is a separate store.

```sh
paddock claude --auth isolated          # one separate login
paddock claude --auth isolated:work     # another one, named "work"
paddock auth import claude --auth isolated:work   # start it from a copy of your login
```

### MCP servers

MCP servers come from the same configuration as on the host, and MCP OAuth tokens come with the login. A stdio server runs a command on your computer, so paddock must mount that command into the sandbox. paddock mounts only the smallest part that runs: the binary, the interpreter in its script header, or the `node_modules` folder of a package. It never mounts the whole repository or install folder around a package.

paddock trusts the configuration in your home folder, such as `~/.claude.json`. A server there runs without a question when its command is in a tool install folder: system folders, mise, uv, pipx, a `node_modules` folder, or a single file in `~/.local/bin`.

paddock does not trust a configuration file in the project, such as `.mcp.json`, `opencode.json` or `.gemini/settings.json`. Anyone who can change the project can change these files, and so can the agent in the sandbox. A server from a project file runs without a question only when its command is in the project, in a system folder, or in the shared tool installs in your home (mise, asdf, uv, pipx, `~/.local/bin`). A command anywhere else, for example in another repository's `node_modules`, stays off. The same applies to a script header that names an interpreter outside these folders.

A server that needs other host paths, or a server on a host loopback port, stays off until you allow it. `paddock mcp` shows each server, where it comes from, and the exact host paths that paddock mounts when you allow it:

```sh
paddock mcp claude     # each server, and whether it runs in the sandbox
```

paddock hides `.env` files, keys and other credential-shaped files in every folder that it mounts for a server, as it does in the project. A folder in the shared tool installs is the exception. To show one such file, add it to `[secrets] reveal`.

```toml
# ~/.config/paddock/config.toml or the project paddock.toml
[mcp]
allow = ["my-server"]   # mount the host paths that this server needs, read-only.
                        # In a project paddock.toml, you approve this once on the host.

[network]
host_ports = [8765]     # a local MCP server on the host loopback
```

Some harnesses can keep MCP tokens in the system keyring instead of a file. The sandbox cannot reach the keyring. For Codex, set `mcp_oauth_credentials_store = "file"` to share the tokens.

## Environment variables

By default, the sandbox gets the host environment. The first time in a project, and each time the list changes, paddock prints how many variables it passed and the names of the ones that look like secrets. High-privilege cloud and infrastructure credentials are not passed either: AWS keys and session tokens, Azure client secrets, `GOOGLE_APPLICATION_CREDENTIALS`, Cloudflare, DigitalOcean and Hetzner tokens, `VAULT_TOKEN`, Terraform `TF_TOKEN_*`, forge tokens such as `GH_TOKEN` and `GITLAB_TOKEN`, and similar. They can create, delete or bill resources far beyond the project. The notice names the ones that it dropped, and `paddock env show` lists them. To pass one, name it in `[env] pass`. Variables that name a host socket or a control channel (`SSH_AUTH_SOCK`, `DBUS_SESSION_BUS_ADDRESS`, `HYPRLAND_INSTANCE_SIGNATURE`, `DOCKER_HOST`, terminal multiplexer sockets, the Claude app's `CLAUDE_CODE_MESSAGING_*`) are never passed, and the sandbox sets `HOME`, `PATH` and `TMPDIR` itself.

To pass only an allowlist (terminal, locale, the variables of the harness profile, `[env] pass`, and the host proxy), use `--env clean`, or set this in `~/.config/paddock/config.toml`:

```toml
[env]
inherit = false
pass = ["MY_TOOL_TOKEN"]
```

## Privacy defaults

paddock turns off vendor telemetry in every sandbox. It uses the switches that each vendor documents. It sets them only inside the sandbox: as environment variables, or in the sandbox's own copy of a settings file. Your host configuration does not change. `paddock explain` shows each switch and the profile that sets it.

| Program | What paddock turns off |
|---|---|
| Claude Code | telemetry (`DISABLE_TELEMETRY`), error reports (`DISABLE_ERROR_REPORTING`), the session survey (`CLAUDE_CODE_DISABLE_FEEDBACK_SURVEY`) |
| Codex | analytics and the metrics exporter (`[analytics] enabled = false` in `~/.codex/config.toml`) |
| Copilot CLI | the telemetry that the CLI sends (`--disable-client-telemetry`) |
| Gemini CLI | usage statistics (`privacy.usageStatisticsEnabled` in `~/.gemini/settings.json`) and telemetry (`GEMINI_TELEMETRY_ENABLED`) |
| Grok Build | telemetry, usage metrics, crash reports, Mixpanel and trace uploads (`GROK_TELEMETRY_*`, `DISABLE_TELEMETRY`, `DISABLE_ERROR_REPORTING`) |
| opencode | automatic sharing of sessions from a project `opencode.json` (`share` stays `manual`). opencode documents no telemetry switch. |
| VS Code | usage data, error telemetry and crash reports (`telemetry.telemetryLevel: off`), and experiments |

`DISABLE_TELEMETRY` also stops the feature flags of Claude Code. Some features need them, for example Remote Control. The feedback commands of the harnesses still work. They send data only when you use them.

Some settings belong to your account, and no program can change them. The use of your data for model training is one of them. The Antigravity telemetry switch is another. The Claude and ChatGPT desktop apps document no telemetry setting at all. `paddock doctor` prints, for each installed program, where you turn off training:

| Vendor | Where |
|---|---|
| Anthropic (Claude Code, Claude app) | [claude.ai privacy settings](https://claude.ai/settings/data-privacy-controls), "Help improve Claude" |
| OpenAI (Codex, ChatGPT app) | [ChatGPT data controls](https://chatgpt.com/#settings/DataControls), "Improve the model for everyone" |
| GitHub (Copilot CLI, Copilot in VS Code) | [Copilot settings](https://github.com/settings/copilot), "Allow GitHub to use my data for AI model training" |
| Google (Gemini CLI) | the [privacy notice](https://developers.google.com/gemini-code-assist/resources/privacy-notices) of your sign-in method |
| Google (Antigravity) | `/settings` in agy, "Enable Telemetry" ([docs](https://antigravity.google/docs/settings)) |
| xAI (Grok Build) | `/privacy` in grok, coding-data sharing ([docs](https://github.com/xai-org/grok-build/blob/main/crates/codegen/xai-grok-pager/docs/user-guide/02-authentication.md#related-settings)) |
| opencode | the data settings of the model provider that you use |

To keep the vendor defaults, set this in `~/.config/paddock/config.toml`. A project `paddock.toml` cannot change it, in either direction.

```toml
[privacy]
telemetry = "vendor"   # default: "off"
```

## Desktop projects

The sandbox has the libraries for desktop apps: the bwrap backend sees the host `/usr`, and the docker backend installs them in its image. Access to the display is the risk. A client of the host compositor can capture the screen and read the clipboard, and on wlroots compositors it can type into other windows. paddock has four display modes:

| Mode | What the agent gets |
| :--- | :--- |
| `off` | No display. The default for projects that are not desktop projects. |
| `headless` | A private compositor inside the sandbox, with Xwayland. The agent runs the app and inspects it with `paddock-screenshot`. Nothing on your desktop is reachable. paddock turns this mode on when it finds a desktop project (Tauri, Electron, GTK, Qt, egui, iced, Flutter and others). |
| `window` | A nested compositor that runs on your desktop as one window. You watch the app, and the agent sees only that window. |
| `host` | Your real compositor socket. Use it only when you must. Needs approval. |

```sh
paddock claude --gui window
```

Add `--gpu` to bind the GPU render nodes. Without it, apps render in software.

## Host requests

Sometimes the agent needs one thing from your computer: a screenshot of the real screen, one command on the host, or one file outside the project. paddock does not open the sandbox for that. The agent asks with `paddock-host`, you decide on the host, and paddock does the work on the host.

| Capability | What the host does | Default |
| :--- | :--- | :--- |
| `screenshot` | Captures the screen, one output or the focused window | ask |
| `exec` | Runs one command in the project folder, without a shell | ask, and never allow |
| `fetch` | Copies one host file or folder into the project, at most 64 MiB | ask |
| `clipboard-read`, `clipboard-write` | Reads or sets the host clipboard | ask |
| `open-url` | Opens an http or https link in your browser, for example a harness login page | ask |
| `notify` | Shows a desktop notification | allow |
| `git-push` | Pushes commits to a remote of the project, with your credentials | ask |
| `forge` | Runs a fixed set of `gh` commands on the project's repository, with your login | reads allow, writes ask |
| `spawn` | Starts another harness in its own sandbox with one task (see [Spawn other agents](#spawn-other-agents)) | ask, and off until `[spawn] harnesses` lists one |

A request travels through the gateway device `/dev/paddock/gw`, which every sandbox with `/dev/paddock` has. `paddock-host` writes the request there and reads the result. paddock normalizes each request and names it by a hash of its capability and arguments. The same request sent again while it waits joins the one that waits: you see one card, and the work runs once. A read waits at most 100 seconds (a `timeout` in the request sets 1 to 600); when that passes first, the read fails with `ETIME` ("still waiting"), and `paddock-host` sends the same request again on its own. An agent can also write to the device itself. A sandbox without `/dev/paddock` uses the host socket as before.

The paddock process that runs a sandbox decides and asks you, but it does not run an approved command itself. For `exec`, it signs a grant for exactly that request: a hash of the capability and the arguments, the sandbox, the project, a lifetime of 30 seconds and a random number. It signs with HMAC-SHA256 and a key in `~/.local/state/paddock/gateway.key` (mode 0600). A separate process, `paddock gatewayd`, checks the signature, the time, that the grant was not used before, and that it covers this request, and only then runs the command. paddock starts the executor when a request needs it, and the executor stops after two minutes without requests. `paddock gateway install` lets systemd start it instead, on the first request (socket activation); `paddock gateway remove` turns that off. The other capabilities move to the executor next, and each one gets its own limits there. `fetch` never copies paddock's own folders, with the key, into a sandbox.

A request that asks waits for your answer for up to two minutes. You can answer in three places, and the first answer wins:

- An approval card appears at the top of the screen as soon as a request arrives, without opening the panel. It shows the question, exactly what will run and in which folder, the reason that the agent gave, and the time left. Click No, Yes once or Yes for this session. The card never takes the keyboard when it appears, so typing in another window cannot answer it. To answer with the keyboard, bind a key to `omarchy-shell harnessix.paddock.approvals keys`: the card then starts on No.
- The bar widget turns red and shows the number of waiting requests. The panel lists them and the most recent answers, including the ones that the agent approver gave.
- Without the Omarchy plugin, a desktop notification offers the same three answers.
- In a terminal, `paddock approvals` lists the requests, and `paddock approvals answer <id> once|session|deny` answers one.

Some rules hold whatever your configuration says. `exec` is never allowed without asking, and "Allow for this session" covers only the identical command line. `fetch` refuses the protected paths (`~/.ssh`, `~/.gnupg`, cloud credentials, harness logins and the others), even if you approve. It also refuses a folder that holds one. paddock opens the path before it asks you, and the card shows where the path really leads. The copy reads what paddock opened, so the agent cannot swap the path while you decide. `fetch` copies regular files and folders only, and skips symlinks, pipes and devices. Results that are files go to `.paddock/inbox/` in the project, which git ignores.

Set the mode of each capability in `~/.config/paddock/config.toml`. A project `paddock.toml` cannot change these modes, because the agent can edit it.

```toml
[host]
screenshot = "allow"   # deny | ask | allow
clipboard-read = "deny"
wait = 60              # seconds to wait for an answer
```

### Push and pull requests

The sandbox has no SSH key, no git credential helper and no `gh` login. paddock also drops `GH_TOKEN`, `GITHUB_TOKEN`, `GITLAB_TOKEN` and similar variables from the inherited environment. The agent still runs plain `git push` and `gh pr create`. The host does the work with your own credentials, and the token never enters the sandbox.

For `git push`, the sandbox git configuration sends the push to paddock on the host. The approval card shows the remote URL, each local and remote branch, the commits (at most 20) and whether it is a force push. The host pushes from a scratch repository that has no configuration and no hooks of its own. A file that the agent writes in `.git` cannot run a program on the host.

- Force pushes, branch deletions and pushes to the default branch ask every time. "Allow for this session" is not offered for them.
- "Allow for this session" covers one remote and one branch.
- paddock reads the remotes when the sandbox starts. Inside the sandbox, `.git/config` is read-only. If the agent names another remote, for example with `git -c`, the card marks it as new and asks every time. The host always pushes to the URL that a remote had at the start.
- Hooks never run on the host, not even `pre-push`.
- Right before the push, paddock checks the object store of the repository again: `objects/info/alternates` must be unchanged, and `objects`, `objects/info` and `objects/pack` must still be plain directories. The scratch repository reads objects through them, so a symlink or a new alternates line would push objects of another repository of yours.

For `gh`, only these commands exist: `pr create`, `pr view`, `pr list`, `pr checks`, `pr diff`, `pr comment`, `issue view`, `issue list`, `issue comment`, `run view`, `run watch`, `run list` and `repo view`. The commands that only read run without a question. `pr create` and the comment commands ask. `--repo` must name the repository of one of the project's remotes, and paddock adds it when the agent leaves it out. paddock refuses `gh api`, `gh auth`, `gh secret` and every other command. It also refuses the flags that read a host file or open a program on the host, such as `--web` and `--editor`. The sandbox reads `--body-file` itself and sends the text. `gh` must be installed and logged in on the host.

To make reads ask too, or to turn either capability off, set it in `[host]`, for example `forge = "deny"`. `forge = "allow"` also allows the write commands without a question. `git-push = "allow"` allows ordinary pushes, but force pushes, pushes to the default branch and pushes to a new remote still ask.

`paddock log` shows every request with its decision and result. The log is `~/.local/state/paddock/host-audit.jsonl`. It does not keep text that can be private. For `clipboard-write`, it keeps only the length of the text. For `exec`, it keeps the program and the number of arguments, because arguments can hold tokens. For `forge`, it keeps the command without the body text. For `notify`, it keeps the lengths of the title and the body. For `open-url`, it keeps the scheme and the host. paddock makes its folders readable only by you (mode 0700) and its files too (mode 0600), and tightens older ones that other users can read. Request text never goes into the arguments of a host process, which other users can see: notifications go over the desktop bus, and `gh` reads the body on its standard input. To teach agents the client, run `paddock skill install`: it copies a short skill into the skill folders of your harnesses. Inside a sandbox, `paddock-host --help` is the full reference.

### Spawn other agents

An agent in a sandbox can start an agent of another harness. For example, Claude plans the work and Codex writes one part of it. The agent asks with `paddock-host spawn`. paddock then starts a second sandbox on the host, next to the first one. It does not put a sandbox inside a sandbox.

```sh
paddock-host spawn codex --worktree --prompt-file task.md   # prints an id
paddock-host spawn gemini --role reviewer --from-head --prompt-file review.md
paddock-host spawn codex --prompt-file task.md --timeout 30m --mem 4G --cpus 2
paddock-host status <id>
paddock-host wait <id> --timeout 600
paddock-host kill <id>
```

The child runs the harness once, without a terminal (`claude -p`, `codex exec`, `opencode run`, `copilot -p`, `agy --print` or `grok --prompt-file`), and stops when the task is done. It works in a new worktree on the branch `paddock/spawn-<harness>-<id>`, from the commit that the parent has checked out. The worktree uses the same repository, so the child's commits are in the parent's repository at once. `wait` and `status` give the branch name and the exit status. The parent reads the branch with git, for example `git diff main...paddock/spawn-codex-<id>`, and merges what it wants. paddock also writes a short summary to `.paddock/inbox/spawn-<id>/`: `result.json` with the state, the exit code, the branch, the commit count and a diff summary, and `output.txt` with the end of the child's final output. When the child ends, paddock also writes `result:<id>` into the `tree` scope of [paddock kv](#shared-state-paddock-kv), with `status`, `summary` and `ref` (the branch). So the parent can run `paddock-host kv watch --prefix result: --scope tree` instead of polling several children.

With `--from-head`, the child starts from the parent's files as they are now. paddock first makes a commit of them, with uncommitted and new files but without ignored ones. It uses a private index file, so the parent's files, index and branches do not change. The host git call is the hardened one that paddock uses for worktrees: it refuses a repository configuration that can start a program.

A role is a fixed set of limits. The agent names a role. It cannot combine its own limits.

| Role | Files | Environment | Push and `gh` writes |
| :--- | :--- | :--- | :--- |
| `implementer` (the default) | its own writable worktree | clean | as the parent allows |
| `reviewer` | read-only | clean | refused |

"Clean" means that only the allowlist and the harness's own variables (for example `OPENAI_API_KEY` for codex) pass. A reviewer keeps the network mode of the parent for now.

`--timeout`, `--mem` and `--cpus` can only lower the limits in your configuration. If the host has `systemd-run --user`, paddock starts the child's host process in a systemd scope with `MemoryMax`, `CPUQuota` and `RuntimeMaxSec`, so the limits cover the child sandbox and everything in it. paddock itself stops the child at the timeout, so the child can still save its login. The scope's runtime limit is a backstop one minute later. Without `systemd-run`, paddock enforces the timeout and refuses `--mem` and `--cpus`. The audit log says why a child stopped.

Spawn is off until you turn it on. Only `~/.config/paddock/config.toml` can turn it on. A project `paddock.toml` cannot.

```toml
[host]
spawn = "ask"            # deny | ask | allow; ask is the default

[spawn]
harnesses = ["codex"]    # the harnesses that a sandbox may start; empty by default
max_running = 2          # children of one sandbox that run at the same time
max_depth = 1            # 1: a child cannot start children of its own
timeout = 1800           # seconds that one child may run in total
# max_mem = "4G"         # the most memory of one child; no limit by default
# max_cpus = 2           # the most CPUs of one child; no limit by default
```

These rules always hold:

- A child never gets more than its parent. It gets the same backend, network mode, LAN setting and approved mounts, and the same project. Its environment is clean. It gets no display. Its host capability modes are at most those of the parent. Its `/dev/paddock` has at most the devices of the parent. In paddock kv it writes only its own scope and the `tree` scope. paddock uses the settings that the parent had when it started, so a change to `paddock.toml` during the run does not reach the child.
- No credentials move between sandboxes. The child uses its own login of its harness, in its own sandbox home. The parent never sees it. The parent gets only the results.
- The first spawn of each harness in a session asks you. The card shows the harness, the role, the login it uses, the limits and the task. "Allow for this session" covers only that harness in that role.
- Requests from a child, for example `git-push` or `exec`, show "Started by sandbox X" on the card. An "Allow for this session" answer for the parent does not apply to the child, and an answer for the child does not apply to the parent. Otherwise a sandbox could avoid a question by giving the work to a child.
- paddock counts the running children and the depth on the host. A child stops when its parent ends, when you run `paddock stop` on the parent, at its time limit, or with `paddock-host kill`. No child keeps using your quota after the parent is gone.
- The task never goes into the arguments of a process, which other users can see. The harness reads it on its standard input or from a private file. paddock reads the prompt file from the project without following symlinks, refuses pipes and devices, and allows at most 256 KiB.

`paddock ps` and the bar panel show children under their parent, and the approval card names the sandbox that started a child. `paddock log` records the parent, the child, the harness, the role, the branch, the limits, the exit status, the reason for the end and the durations. It never records the task or the output. Gemini CLI has no one-shot entry yet, because paddock did not check its flags on a real install.

You can let an agent on the host answer requests for you (`paddock approver`). paddock trusts that setting only from a root-owned file (`sudo paddock approver enable --system`) or from a file signed with a password that you type when a sandbox starts, so that a process on your computer cannot turn it on for itself. The agent approver can only answer the capabilities that you list, the rules above still hold, and if it does not answer clearly, the request comes to you.

## Shared state: paddock kv

Agents need to keep state between runs and hand work to each other. paddock kv is a small key-value store for that. It lives on the host, in `~/.local/state/paddock/kv/store.db`. Each sandbox reaches it through its own socket. The socket tells paddock which sandbox asks, so an agent cannot claim to be another sandbox.

Inside the sandbox, the agent uses `paddock-host kv`:

```sh
paddock-host kv put task:review '{"title":"review src/auth.rs","status":"todo"}' --scope project
paddock-host kv claim review --ttl 600 --scope project
paddock-host kv watch --prefix result: --scope project --timeout 300
```

In a spawn tree, the parent writes the tasks into the `tree` scope, each child claims one with a lease and writes its result, and the parent waits for `result:`:

```sh
# parent
paddock-host kv put task:parser '{"title":"fix the parser","status":"todo"}' --scope tree
paddock-host spawn codex --prompt-file task.md
paddock-host kv watch --prefix result: --scope tree --timeout 600
# each child
paddock-host kv claim parser --ttl 600 --scope tree    # exit code 9: a sibling has it
paddock-host kv put result:parser '{"status":"done","ref":"paddock/spawn-codex-ID"}' --scope tree
```

Each key belongs to a scope:

| Scope | Who reads and writes it | Default |
| :--- | :--- | :--- |
| `self` | This sandbox only | on |
| `project` | Every sandbox of the same project. Each entry records the sandbox that wrote it. | on |
| `parent` | Read-only: the `self` scope of the sandbox that started this one with `spawn` | only in a spawned sandbox |
| `tree` | The sandbox that you started and every sandbox that it started with `spawn`, and theirs. Each entry records its writer. | on as soon as a sandbox can use `spawn` |
| `user` | Every sandbox of every project | asks you at the first use |

An agent with a prompt injection can write a false tip into the `project` scope, and the next agent reads it. So paddock records the writer of every entry, and the client marks what another sandbox wrote as untrusted.

A spawned sandbox never gets more than its parent here either:

- It writes its own `self` scope and `tree`. It reads `project`, but does not write it, so that what a child picks up stays in the tree until its parent decides what the project keeps. `child_project_write = true` in `[kv]` changes that.
- For `user`, it inherits "Allow for this session" from the sandboxes above it. Without that, it is refused, and it never asks you itself: one prompt injection must not turn into many questions. Use the scope in the root sandbox first, or set `[kv] user = "allow"`.
- The `tree` scope belongs to the root of the tree: the sandbox that you started. When the root ends, its children stop and the tree is read-only. The data stays for you to review (`paddock kv list --tree ROOT-ID`) until `paddock gc` removes it with the root's `self` data.

A write can depend on the current version of a key (`--if-version`). A key can expire (`--ttl`). `mget` and `mset` read and write several keys at once. `history` shows the last versions of a key. `watch` waits for a change of one key or of a prefix. A lease (`claim NAME --ttl SECONDS`) makes sure that only one agent does a task. paddock sets the owner of a lease from the socket. Only the owner releases it, or with `--force` a sandbox above the owner in its spawn tree (its parent, or the parent of that one), or you on the host. A sibling cannot. A lease expires at the end of its time. Keep values small: put diffs, files and logs in a git branch or a file, and store the reference. `paddock-host kv --help` in the sandbox lists the key names that agents share: `task:`, `lease:`, `result:`, `heartbeat:` and `note:`.

On the host, `paddock kv list`, `paddock kv get KEY`, `paddock kv delete KEY` and `paddock kv release NAME` read the project scope of the current folder. `--user`, `--sandbox ID` and `--tree ROOT-ID` choose another scope.

The limits and the `user` scope are in `~/.config/paddock/config.toml`. A project `paddock.toml` cannot change them.

```toml
[kv]
user = "ask"           # ask | allow | deny
max_value = 65536      # bytes per value
self_bytes = 4194304   # bytes per scope; also project_bytes and user_bytes
max_keys = 4096        # keys per scope
keep_days = 30         # paddock gc removes the self scope of a run, and the tree of a root, this many days after it ended
child_project_write = false   # true: spawned sandboxes can write the project scope too
enabled = true
```

`paddock gc` removes the `self` data of runs, and the `tree` scope of spawn trees whose root ended, more than `keep_days` ago, and keys whose time to live passed. The audit log `~/.local/state/paddock/kv-audit.jsonl` records each write with its key name and size. It never records a value.

## Devices: /dev/paddock

A device is a host service that the sandbox uses as a file. Each sandbox gets its own small file system at `/dev/paddock`. The paddock process on the host serves it, so paddock knows which sandbox calls. No root and no credentials enter the sandbox.

One open is one request. The program opens the device for reading and writing, writes the request, and reads the response until the end of the file. The first read sends the request and waits for the answer. Closing the file early cancels the request. Each device has a time limit. After that time, the read fails with `ETIMEDOUT`. `/dev/paddock/.catalog` is read-only and describes each device: its request format, its limits and whether it is on.

```sh
exec 3<>/dev/paddock/echo; printf 'hello' >&3; cat <&3
```

All devices are off by default. Turn a device on in `~/.config/paddock/config.toml`. A project `paddock.toml` cannot turn on a device, because the agent can edit that file.

| Device | What it does |
| :--- | :--- |
| `echo` | A test device. It sends the request back. |
| `wait` | The read returns at a time. The request is an RFC 3339 time, such as `2026-10-30T15:00:00Z`, or a duration, such as `15m`. At most seven days ahead. |
| `ask` | The agent writes a question. The question appears on the approval card. The read returns your answer. |

```toml
[devices]
wait = true
ask = true
mount = true   # false: no /dev/paddock at all
```

`paddock-host device NAME` in the sandbox sends one request and turns errors into exit codes. See "Wait without sleep" below.

The mount needs FUSE: `/dev/fuse` and `fusermount3` (package `fuse3`). `paddock doctor` checks both. The bwrap backend binds the mount at the start. docker and podman bind an empty folder and paddock mounts the file system on it after the container starts. The container waits up to five seconds for it before the harness starts. This needs a shared mount for the paddock run folder, which is the systemd default. srt cannot place a mount at `/dev/paddock`: there, the devices are at the path in `$PADDOCK_DEVICES`. When FUSE is missing, the sandbox starts without devices, and `paddock doctor` says why.

Programs must read the response with `read`. `cat` from coreutils, `dd`, shells, Python and Rust do that. busybox `cat` uses `sendfile` and sees an empty file: use `dd` there. Requests and responses never go into logs or process arguments. The log records the device name, the sizes and the result.

### Wait without sleep

Agents often wait: for CI, for a child agent, for your answer. Many agents wait with `sleep 60` and then check again. Each check costs a model turn, and a result that arrives between two checks waits for the next one.

paddock gives agents blocking reads instead. The read waits until the event happens and returns at once. While it waits, the agent uses no CPU and no tokens.

| Wait for | Command in the sandbox |
| :--- | :--- |
| A key in paddock kv | `paddock-host kv watch KEY --timeout 100`, or `--prefix PREFIX` |
| A child agent | `paddock-host wait ID --timeout 100` |
| A time | `paddock-host device wait 2026-10-30T15:00:00Z --timeout 100` |
| Your answer | `paddock-host device ask "Deploy to staging?" --choice yes --choice no` |

A tool call of a harness has a time limit, often two minutes. So each read has a longest wait, `--timeout`. When that time passes before the event, the command exits with 75, "still waiting", and the agent runs the same read again. Exit code 75 means the same for every one of these commands. A plain read of a waiting device fails with `ETIME` ("Timer expired") instead. The `paddock-host` skill tells agents to loop on the read and never to use `sleep`.

A prefix watch also wakes when a key under the prefix expires. So `watch --prefix lease:` returns when a lease runs out and the task is free.

A question from `ask` shows on the approval card in the bar, with the choices as buttons. Click the text field to type another answer, and press Enter. Escape gives the keyboard back. "Dismiss" ends the question, and the agent gets `EACCES` (exit code 3). Without the bar, a desktop notification shows the question and its choices. On the command line, use `paddock approvals` and `paddock approvals answer ID --text -`, then type the answer and press Ctrl-D: read from standard input, it never shows in the process list or your shell history. Your answer goes to the agent only. The audit log records the sizes of the question and the answer, never the text. A question stays open while the agent reads it again; paddock removes it two minutes after the last read, or when the sandbox ends. At most 8 questions of all sandboxes wait at once, host requests come first on the card, and after you dismiss a question, that sandbox cannot ask again for 30 seconds. Control characters and invisible format characters are removed from questions before they are shown.

## App mode

Desktop agent apps run in a sandbox too: ChatGPT with Codex, Claude Desktop, VS Code, and flatpak apps. They get the same file, git, secrets, environment and network rules as a terminal harness, and their windows are normal windows on your desktop. Flatpak apps have some limits, described at the end of this section.

<p align="center">
  <img src="docs/assets/app-demo.gif" alt="A recording: a host terminal shows an SSH key and a .env with a token, then runs paddock app run vscode; in VS Code's own terminal, the SSH key does not exist, .env is 0 bytes, and an edit to the project works" width="100%">
</p>

```sh
paddock app list                          # the apps paddock knows, and their launcher entries
paddock app run claude-desktop ~/src/api  # run an app with one or more workspace folders
paddock app install chatgpt-desktop ~/src/web   # add a launcher entry that runs it in paddock
paddock app uninstall chatgpt-desktop
```

The app sees only the folders that you give it, from the command line or from `[apps.NAME] roots` in `~/.config/paddock/config.toml`. Its windows connect to your compositor through a restricted socket: the app cannot capture the screen, read the clipboard in the background, type into other windows or list them. Flatpak apps use flatpak's own Wayland socket instead. Add `--gpu` for hardware rendering.

The app gets a session bus of its own, inside the sandbox. It cannot reach your session bus, your keyring or the system services. Three services run on its bus:

- A private keyring (gnome-keyring). The app stores its login and other secrets there, encrypted with a password that paddock keeps on the host. Claude, ChatGPT and VS Code use it. You log in once inside the sandbox, and the login stays for later runs.
- Links. When the app opens a web link, paddock asks you, the same way as for `paddock-host open`, and then opens the link in your browser. The app's request for a link through the desktop portal works the same way.
- Notifications. They appear on your desktop, with the app's name.

The app gets no file chooser portal. It uses its own file dialog, which shows only the files in the sandbox. If an app needs the real desktop portals, give it the filtered host bus in `~/.config/paddock/config.toml`:

```toml
[apps.NAME]
bus = "host-filtered"
```

On that bus the app reaches only the desktop portals, notifications and the tray. It gets no keyring, so it keeps its login in a plain file in the sandbox home. Flatpak apps always use flatpak's own filtered host bus.

### One worktree per app

Two agents in one checkout switch branches and overwrite each other's changes. With `--worktree`, the app gets its own git worktree of the first workspace folder, on a new branch `paddock/<app>-<date>-<id>`. paddock creates it in `~/.local/share/paddock/worktrees`, the same way as `paddock <harness> --worktree`. Your checkout keeps its branch and its changes.

```sh
paddock app run vscode --worktree ~/src/api
```

To use a worktree on every start, set `worktree = true` in `[apps.NAME]` of `~/.config/paddock/config.toml`. When you start an app from the menu without saved folders, the folder dialog asks whether to open a git repository in a new worktree. "New worktree" is the default button.

When the app exits, paddock prints the commits and the uncommitted changes in the worktree, and the commands to review, merge or remove it. If no terminal shows that text, for example after a start from the menu, a notification gives the branch and the number of commits and changes. paddock never removes a worktree itself. Flatpak apps cannot use worktrees yet.

A flatpak app becomes an app profile with `[app] flatpak = "ID"`. paddock drops every permission that the app asks for, also those from `flatpak override`, and adds back only the workspace folders, the Wayland socket, the portals and, with `--gpu`, the GPU. Many flatpak apps ask for your whole home directory and the host bus, so their own sandbox is not enough. The network is `egress` or `none`, and the host passthrough (`paddock-host`) is not available.

The workspace folders of a flatpak app get the same protection as for other apps. paddock refuses the home directory and protected paths such as `~/.ssh`. In each folder, `.git` is a mount point of its own, its `config`, `config.worktree`, `commondir`, `gitdir`, `objects/info/alternates` and `hooks` are read-only, and the folders in `secrets.hide` are hidden. The host environment passes through the same filter. flatpak can hide a folder, but not a single file. So when the secrets detector finds a file such as `.env` or `id_ed25519` in a folder, paddock does not start the app, and names the files. Move them out, or put them in a folder and add that folder to `[secrets] hide`. To start the app anyway, with these files visible to it:

```toml
[apps.NAME]
unprotected_workspaces = true
```

This works only in `~/.config/paddock/config.toml`, never in a project's `paddock.toml`. paddock then prints a warning with the files on every start, `paddock app list` shows the setting, and the bar panel marks the running app.

Windows of app sandboxes get a sage green border, so you can tell a protected app at a glance. paddock sets the border on each window at run time (Hyprland), and your Hyprland configuration does not change. App sandboxes appear in the bar panel with the other sandboxes. Enter on one focuses its window.

Known limits:

- Desktop apps log in again inside the sandbox, because your keyring is out of reach. The login then stays in the private keyring of the sandbox. An app with `bus = "host-filtered"` has no keyring. Claude, for example, then warns that no system keyring is available and stores its secrets without encryption, in the sandbox home, which only your user can read.
- The app cannot open files with your desktop apps or pick files outside the sandbox. Its file dialog shows only the sandbox.
- A new window that gets focus can read the clipboard once. This is how Wayland clipboards work, and Flatpak has the same gap.
- Tray icons may not appear.
- Apps cannot update themselves inside the sandbox. Update them on the host, with your package manager or the app's own installer outside paddock.
- To add a folder, restart the app. A running sandbox cannot get new mounts.
- Custom links (`claude://`, `codex://`, `vscode://`) reach the app in its sandbox after `paddock app install`, which makes paddock their handler. `paddock app uninstall` gives them back to the old handler. A login page that the app opens through paddock and that returns to `http://127.0.0.1:PORT` reaches the app: paddock opens that one port on 127.0.0.1 of your machine for ten minutes, while the app listens on it. If an app opens its login page in another way, set `[apps.NAME] login_ports = true` in `~/.config/paddock/config.toml`. Then all the ports the app listens on are open on 127.0.0.1 of your machine while it listens, for any local program.

## Sandbox by default

An agent reads files with the same commands that you use. One wrong `grep -r` or `find ~` outside a sandbox sends your SSH keys, other projects and personal files into the agent's context, and to the vendor. `paddock shims install` makes the sandbox the default, so that you do not have to remember `paddock`:

```sh
paddock shims install    # claude, codex, copilot and the others now start in a sandbox
paddock shims list       # each command, and whether it is on
paddock shims remove     # takes everything away again
```

After `install`, typing `claude` in a new shell runs `paddock run claude` in the current project. Arguments pass through: `claude --resume` runs `paddock run claude -- --resume`. paddock writes one script, `~/.local/share/paddock/shims.sh`, with a shell function for each command of each installed harness. It adds one marked line to `~/.bashrc` and `~/.zshrc`, where they exist, that loads the script.

- To run a command without the sandbox once, prefix it: `PADDOCK_BYPASS=1 claude`.
- In your home directory, the command stops with an error, because paddock does not use the home directory as a project. Go to a project first.
- Shell functions win over PATH, so version managers such as mise do not bypass them. Scripts and programs that start `claude` themselves are not covered: only interactive bash and zsh shells. For the agents that Omarchy starts, see the next section.
- If paddock or the script is gone, each function runs the real program, and the line in the rc file does nothing.
- When you install another agent, or a newer paddock brings a profile, the next harness start updates the script in the background (see [Keeping the menu and the shims current](#keeping-the-menu-and-the-shims-current)).
- `paddock doctor` lists the commands that start in a sandbox, the rc file that loads them, and how to turn them off. Each run also prints a `paddock:` line with the sandbox it started, so you always see when a shim takes over.

### Only the agents that Omarchy starts

Omarchy starts the default agent with `omarchy-agent`: from the key Super+Shift+Ctrl+A, the drop-down console, the agents panel, `omarchy agent prompt`, crash reports and the alias `a`. These starts run unattended: `omarchy-agent` adds each agent's flag for no approvals, such as `codex --approve-for-me`. This scope puts only these starts in a sandbox:

```sh
paddock shims install --scope omarchy-agent   # agents from omarchy-agent start in a sandbox
paddock shims list                            # shows the scope and whether it is on
paddock shims remove                          # removes both scopes
```

- Commands that you type and scripts do not change. Omarchy's files do not change either.
- paddock writes one marked block into `~/.config/mise/config.toml`: a mise wrapper for `omarchy-agent`. It keeps the rest of the file, with its comments and order, and runs `mise reshim`. `paddock shims remove` takes the block out again, byte for byte. If the file already has its own `[wrappers.omarchy-agent]`, paddock does not replace it and stops with a message.
- The wrapper runs the real `omarchy-agent` with a private folder first on `PATH`, for that start only. Its shim for `omarchy-launch-tui` puts `paddock wrap --` in front of the agent command. The agent window keeps its app id, `org.omarchy.agent`. With `--inline`, the agent starts the same way in the current terminal.
- If paddock is gone, the wrapper runs `omarchy-agent` without a sandbox, so the key still works.
- `PADDOCK_BYPASS=1 omarchy-agent` starts once without a sandbox.

`paddock wrap -- COMMAND` runs the command as it is, in a sandbox:

- The arguments stay the same. paddock adds no auto-approval flags and removes none. It adds only the telemetry switches of the profile (see [Privacy defaults](#privacy-defaults)).
- The project is the current folder. The home folder is no project. If `omarchy-agent` starts in the home folder (when `~/Work` does not exist), the agent works in a scratch project of its own, `~/.local/share/paddock/scratch/<agent>`. The `paddock:` line names it, `paddock files` lists it, and `paddock gc --scratch` removes old ones.
- An `env` prefix keeps its variables. For Claude Code, Codex and Grok Build, Omarchy's account switch sets `CLAUDE_CONFIG_DIR`, `CODEX_HOME` or `GROK_HOME`. paddock uses that folder as the login of the sandbox: it copies the login in at the start, and copies a refreshed token back at the end, the same as for your main login. The pin `OMARCHY_AGENT_<AGENT>_HOME` stays.
- The shell form `bash -c '…' amp PROMPT` runs whole in the sandbox, with `$0` set to the agent program. An absolute path, such as a user install in `~/.local/bin`, runs that program.
- The exit code passes through.
- A command without a paddock profile runs without a sandbox. paddock shows a notification that names it, so a start never fails only because paddock does not know an agent yet.
- If paddock knows the agent but cannot start the sandbox, it says why and does not start the agent on the host.

`paddock doctor` shows whether the wrapper and the shim folder are in place, whether `mise reshim` made the dispatch link, whether `omarchy-agent` still starts agents the way paddock expects, and whether your default agent has a paddock profile.

Codex starts its own sandbox for each command. That sandbox cannot start inside paddock. With `--approve-for-me`, Codex runs the command again without it, inside paddock's sandbox.

paddock has a profile for every agent that `omarchy-agent` can start. `paddock doctor` lists the entries of each profile that come from documentation and not from a test.

| Agent | In a sandbox | Notes |
| :--- | :--- | :--- |
| claude, codex, copilot, opencode, agy, grok | yes | Tested. claude, codex and grok follow Omarchy's account switch. |
| hermes, pi | yes | Tested: they start in the sandbox. Their code is read-only there, so `hermes update` runs on the host. |
| crush, cursor-agent, omp, ori, kimi, qwen, mcode, kiro-cli, freebuff, kilo, devin, amp, commandcode | yes | From the vendors' documentation. They were not installed when the profiles were written. |
| dsh | yes | `dsh web` serves its page on port 3080. paddock forwards that one port from 127.0.0.1 of your machine into the sandbox. |
| muse | no | Muse runs each shell command in its own bubblewrap sandbox, and refuses the command when that sandbox cannot start. paddock blocks nested sandboxes, so muse starts on the host with its own sandbox, and paddock shows a notice. paddock never turns muse's sandbox off. |
| openclaw | no | Its agent runs in the OpenClaw gateway, a service on your machine. The terminal UI only talks to it, so a sandbox around the UI protects nothing. Starts from `omarchy-agent` run as before, with a notice. `paddock openclaw` runs OpenClaw's embedded runtime in a sandbox, with its own state folder in the sandbox home. |

#### Usage statistics in the Omarchy agents panel

The agents panel reads the session history of each agent from fixed places in your home folder. A sandbox keeps its history in its own home. For starts through `omarchy-agent` (`paddock wrap`), paddock brings it back. Other runs keep their history in the sandbox home: a transcript that a sandbox wrote and that your host harness loads later, for example with `claude --resume`, could carry instructions out of the sandbox.

- Claude Code keeps one folder per project in `~/.claude/projects`. paddock binds the host folder of the current project into the sandbox, read-write. The sandbox sees only this one folder, no other project's. `claude --resume` on the host also finds these sessions.
- Codex (`~/.codex/sessions`) and Grok Build (`~/.grok/sessions`) keep their history by date or by session. paddock copies new history files back every five minutes while the sandbox runs, and at the end of the session. It copies a file only when the host has none, or when the sandbox only added to it. It never follows a symlink in the sandbox home.
- With an account folder (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `GROK_HOME`), the history goes to that folder, the same as on the host.
- An isolated login (`--auth isolated:NAME`) keeps its history in the sandbox home.
- The panel's collectors count the sessions the same way as sessions on the host.

The panel also shows each agent's limits. Its collectors ask the vendor, with the login on the host. While a sandbox runs, it has a copy of that login, so a check from the host would use one login in two places at once. With this scope, a login is checked from one side only:

- paddock adds one marked block at the end of `~/.config/hypr/hyprland.lua`, where Omarchy asks for your own configuration. The block loads a file of paddock, which puts one folder first on the `PATH` of the programs that Hyprland starts, ahead of Omarchy's `bin` folder. The folder has one command in it: `omarchy-agent-usage-update`, which the panel runs for each refresh. Before paddock writes the file, `luac -p` (if installed) checks that it still compiles. It takes effect at your next login.
- If no sandbox holds a login, Omarchy's command runs as before.
- If a sandbox holds the login of Claude Code or Grok Build, the host skips that agent (`--except`), and paddock runs Omarchy's own collector in a sandbox instead. That sandbox has network mode egress, sees your home read-only, and sees the sandbox's copy of the login in place of the host file. Its record goes where Omarchy's goes. These collectors only read a login. They never refresh it.
- The Codex collector starts `codex app-server`, which can refresh the login itself. So while a sandbox holds a Codex login, nobody checks it. The panel shows the last numbers, marked "Limits paused", until the sandbox exits.
- paddock checks only when the panel asks. It adds no timer of its own.
- `paddock shims remove` removes the block, byte for byte, and the folder. `paddock doctor` shows whether the panel's checks go through paddock.

Inside the sandbox, the installs of the agents are read-only. mise updates them on the host before each start (`auto_update`). Where a vendor documents a switch, paddock turns the agent's own self-update off in the sandbox. Other self-updates fail with "Read-only file system".

## Launchers

`paddock launchers install` adds a launcher entry for every harness and app that paddock finds on this machine, with the paddock icon, in a "Paddock" folder:

- In the Omarchy menu (Super+Space), a Paddock submenu. paddock writes it as a marked block in `~/.config/omarchy/extensions/omarchy-menu.jsonc` and leaves the rest of the file as it is.
- In menus that follow the XDG menu standard (GNOME, KDE, Xfce), a Paddock folder.
- In any app launcher, search for "paddock".

A terminal harness entry opens a terminal with `paddock pick <harness>`: pick a recent project or type a path, and the harness starts there. An app entry runs `paddock app run`. If you did not save workspace folders for the app (`[apps.NAME] roots`), a dialog asks for one: pick a recent project or any other folder. The dialog needs zenity. Without it, a desktop notification says how to name the folders.

Logins work from these entries too. When a harness opens its login page, the approval card asks "Open the login page of claude in your browser?" and shows the link. After you allow it, the page opens in your browser on the host, and paddock forwards the one localhost port that the login returns to, for ten minutes and only while the sandbox listens on it. Login callbacks are on by default for claude, codex, copilot, gemini, opencode and agy (`[launch] login_callback` in the profile). They work on the bwrap, docker and podman backends; on srt, paddock prints a warning instead.

```sh
paddock launchers list      # what install would create
paddock launchers install
paddock launchers remove    # removes everything that install wrote, and nothing else
```

### Keeping the menu and the shims current

The menu entries, the Omarchy menu block and the shell shims list the harnesses that were installed when you turned them on. paddock keeps them current:

- On a harness start, paddock compares a cheap signal with the one of the last update: its version and the modification times of the folders on `PATH`. Installing an agent with mise or into `~/.local/bin` changes a folder on `PATH`. When the signal changed, paddock runs `paddock refresh --background` in the background, and the start does not wait.
- `paddock update` runs `paddock refresh` with the new version.
- `paddock refresh` adds the entries and shims of new harnesses and removes those of harnesses that are gone. The background run only adds: with a smaller `PATH`, it could miss a harness, and a missing shim would start that harness without the sandbox.
- Only the parts that you turned on change. `~/.bashrc` and `~/.zshrc` do not change.
- `paddock doctor` names the harnesses that the menu or the shims miss.

## Project settings

`paddock init` writes a commented `paddock.toml` into the project. The project directory is writable from inside the sandbox, so an agent can edit this file. Any setting that widens access (extra mounts, host network, host display, GPU, passed variables, setup commands, image changes) needs a one-time approval on the host. paddock asks on the terminal, or you run `paddock approve`.

paddock finds the folders in `mounts.rw`, `mounts.ro` and `secrets.hide` again at each start. The agent can create symlinks in the project and in other folders that it can write. So paddock refuses such an entry if its path goes through a symlink in one of those folders, and the start stops with a message. Name the real folder instead. The same rule applies to the workspace folders of an app.

## Backends

| Backend | Use it when |
| :--- | :--- |
| bwrap (default) | You want every host tool, and start in milliseconds. |
| podman, docker | The project needs system packages that the host does not have. The base image follows the host distribution. |

Prefer rootless podman over docker. paddock runs it with `--userns=keep-id`, so files in the project keep your user as owner. The default seccomp profile of podman allows nested user namespaces, a common start of kernel exploits, so paddock derives a profile that blocks only that.

## Add a harness

A harness is a TOML profile. Put a file in `$XDG_CONFIG_HOME/paddock/harness.d/`. A file with the name of a built-in profile replaces it. The built-in profiles are in [profiles/](profiles/).

```toml
name = "myagent"
description = "My agent"

[locate]
commands = ["myagent"]

[launch]
yolo_args = ["--yes-to-everything"]

[state]
auth = [".myagent/token.json"]
import_ro = [".myagent/config.toml"]
env_pass = ["MYAGENT_API_KEY"]

# Telemetry switches, set only inside the sandbox. Cite the vendor's documentation.
[privacy]
env = { MYAGENT_TELEMETRY = "0" }

[[privacy.settings]]
file = ".myagent/settings.json"   # in the sandbox home; json or toml (format = "toml")
path = ["telemetry", "enabled"]
value = false

[privacy.training]
setting = "Settings, Data, Use my data for training"
url = "https://example.com/settings/data"

# The one-shot mode for `paddock-host spawn`. Optional.
[oneshot]
args = ["run", "--yes-to-everything", "--task-file", "{prompt_file}"]
prompt = "file"          # or "stdin"
probe_args = ["run", "--help"]
```

More entries, for harnesses that need them:

```toml
[launch]
# paddock wrap runs the harness on the host, with this notice.
wrap_on_host = "it keeps its own sandbox, which cannot start inside paddock"

[state]
# An entry that ends with / is a folder: paddock copies every file in it.
auth = [".myagent/credentials/"]

# The folder that an account variable replaces, for paddock wrap.
[state.account]
env = "MYAGENT_HOME"
dir = ".myagent"

# Values written into the sandbox copy of a settings file, for example to
# turn off a self-update that cannot work on the read-only install.
[[state.settings]]
file = ".myagent/settings.json"
path = ["auto_update"]
value = false

[network]
# A loopback port of the sandbox that the host browser reaches on 127.0.0.1.
publish = [3080]
```

Before each start, paddock makes sure that the help text of the harness still mentions every flag in `yolo_args`. If a flag is renamed, paddock stops with an error and does not start a harness that waits for approvals. A spawn makes the same check for the flags in `[oneshot] args`, with the help text of `probe_args`. The task never goes into `args`: with `prompt = "stdin"` the harness reads it on its standard input, and with `prompt = "file"` paddock replaces `{prompt_file}` with the path of a private file in the sandbox home.

## Limits

The project directory is writable, and you use its files on the host later. Some host tools run project files without being asked: direnv runs `.envrc`, mise runs hooks in `mise.toml`, editors run tasks. paddock reports changes to these files when the sandbox exits. Review them before you use them on the host.

The agent can read its own API token and the variables that it inherits. With network access, it can send them elsewhere. Use `--env clean` and an isolated login to limit what it holds.

pasta hides the host addresses that exist when the sandbox starts. If the host gets a new address while a sandbox runs, for example from a VPN that starts later, services on it are reachable from that sandbox until the next start. paddock watches for new addresses, shows a warning in the bar panel, and prints it at exit.

Shared package caches connect the sandboxes of different projects. The shared mise installs are read-only in every project sandbox, and no project file runs while paddock writes them (see mise toolchains). The cargo, npm, uv and go caches stay writable, because builds write them. Those caches check the hashes of what they download.

Some profile entries come from documentation, not from a test on a real install. `paddock doctor` lists them for each installed harness.

On rootful docker, an escape from the container is an escape to root. Use the bwrap backend or rootless podman when you can.

## Logs and bug reports

paddock prints warnings and errors to the terminal. Other messages stay hidden.

- `-v` shows more: `-v` adds info, `-vv` debug, `-vvv` trace. `-q` shows errors only.
- `PADDOCK_LOG` sets the level too. `PADDOCK_LOG=debug` applies to everything. `PADDOCK_LOG=broker=debug,proxy=trace` applies to single parts.
- The parts that run for the whole session also write to the systemd journal, under the name `paddock`. These are the host request broker, the proxy, the login callback forwarder and the window border. Read the warnings with `journalctl -t paddock -p warning`. Without systemd, paddock skips the journal.
- The bar plugin is silent. It writes a warning to the shell log when it cannot read paddock's output. To see more, start the shell with `PADDOCK_DEBUG=true`. Read the shell log with `journalctl -t omarchy-shell`.

To report a bug, run `paddock doctor --report`. It prints the version, the `paddock doctor` output, the running sandboxes and recent paddock warnings from the journal. `--output FILE` writes it to a file. The report replaces your user name, host name, home path, variable values and tokens with placeholders. Read it before you share it.

## Support scope

paddock is built and tested on Omarchy: Arch Linux with Hyprland. That is the supported platform. Every release passes `bin/test` and `paddock doctor --escape` there.

- Other distributions and compositors are maintained by the community. The bwrap backend, the network isolation and the terminal harnesses work wherever bubblewrap, passt and user namespaces work, but nobody tests each release on them. The apt and dnf paths in `bin/install` are best effort.
- Window focus and the border on protected app windows need Hyprland. The bar widget and the approval card need Omarchy.
- Pull requests for other platforms are welcome when they meet the bar in [CONTRIBUTING.md](CONTRIBUTING.md). Issues about platforms outside Omarchy can be closed without a fix.
- Every issue needs the output of `paddock doctor --report`. The issue form asks for it.

## Development

```sh
bin/test
```

To contribute, read [CONTRIBUTING.md](CONTRIBUTING.md). `bin/test` runs the unit tests, the integration tests, the plugin manifest check, and `paddock doctor --escape` on each backend that the host can run. One test makes sure that tracked files contain no facts about the development machine: the home directory, the user name, the host name, the git server, or the git email.


## License

MIT
