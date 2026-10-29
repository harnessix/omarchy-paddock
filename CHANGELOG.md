# Changelog

## 0.1.3

This release fixes a security problem. Update from earlier versions: in 0.1.2 and before, a project's `mise.toml` was able to replace tools that sandboxes of other projects run. It also adds `paddock du` and `paddock gc`, and smaller docker and podman images.

- Security: a project's mise config was able to change tools that other projects run. The provision step ran `mise install` with the project mounted, and the shared mise installs were writable in that step. A hook, an `[env]` script or a plugin backend in `mise.toml` was able to replace a tool in them, and the agent can edit `mise.toml` for the next start. Now paddock reads the tool list on the host. A clean sandbox without the project installs only registry tools (`core` and `aqua` backends, plain versions) into the shared installs. Other tools install into the private sandbox home. The shared installs are read-only in every project sandbox. They moved to a new folder, `~/.cache/paddock/shared/mise-tools`. You can delete the old folder, `~/.cache/paddock/shared/mise-installs`.
- The docker and podman base images share their layers: one core layer, one desktop layer for every display, app and keyring variant, and the project's own packages. Before, each variant was a separate full image. The first start after the update builds the new image once.
- `paddock gc` counts only the part of an image that removing it frees, and also removes paddock's own records in the docker build cache, which held the data of removed images. Before, it added up full image sizes, so it reported space that it did not free.
- srt backend: a host tool in a system folder, such as mise in `/usr/bin`, no longer comes first on PATH. Before, `gh` in a project with a mise file ran the host `gh` instead of paddock's `gh`, which goes through the host.
- `paddock doctor --escape` gives its test project a `mise.toml` whose `[env]` script and hooks try to write into the shared installs, and checks that they cannot. With `--online`, it checks that a registry tool from `mise.toml` installs into the shared installs.
- `paddock du` shows the disk space of each sandbox home, with its harness, login, project, last use and what deleting it loses. It also lists the paddock images of docker and podman, the worktrees with their unmerged commits, the session records and the keyring passwords. On btrfs, sizes are exclusive bytes. `--json` prints JSON.
- `paddock gc` removes paddock data that is no longer needed. It is a dry run unless you add `--yes`. `--yes` removes old paddock images that no container uses, keyring passwords without a home, session records that ended more than 30 days ago, homes of harnesses without a profile, and the old `mise-installs` cache. paddock does the same after it builds a new image (`PADDOCK_GC_AFTER_BUILD=0` turns that off). `--homes` adds homes whose project is gone or that were not used for `--older-than` days, and desktop app homes also need `--include-app-logins`. `--worktrees` adds worktrees, and those with unmerged commits also need `--force`. gc never touches a running sandbox, deletes only inside paddock's own directories and never follows a symlink.
- Each sandbox home has a `.paddock-origin` file with its project, harness, login, last use and whether it is a desktop app. `du` and `gc` use it only when it matches the folder name of the home. Homes from before this release show "unknown" and their last change.

## 0.1.2

- The bar panel starts apps again. Its New flow ran every entry in a terminal as `paddock NAME`, which refuses apps, so the terminal closed at once. Apps now start with `paddock app run` and no terminal, and the list marks them with the app icon. `paddock list --json` has an `app` field.
- The New flow of the bar panel can start in any folder: its first row, "Choose another folder…", opens a folder dialog. The recent list leaves out hidden folders. Before, only projects that had run in paddock could be chosen.
- The panel warns "login ports open" only when `login_ports = true` opens every port, not for the filtered login callback that every app sandbox has.

## 0.1.1

Highlights: push and open pull requests from a sandbox without credentials, an encrypted keyring for desktop apps, a worktree for each app, files saved outside the project that you can recover, and the LAN blocked by default.

- A menu start of an app without saved workspace folders asks for one in a dialog: a recent project or any folder. Before, the start failed without a message.
- `paddock doctor` lists the commands that shims take over, the rc file that loads them, the one-off bypass and how to turn them off. `paddock shims list` marks each command on or off.
- Releases include a prebuilt `x86_64` Linux binary and its SHA-256 checksum.
- App mode: each app gets a private session bus inside the sandbox, with a private keyring. Claude, ChatGPT and VS Code store their logins and secrets encrypted there, and Claude no longer warns that no system keyring is available. `paddock portal` on that bus sends the app's web links to the host browser after your approval, and its notifications to your desktop. The app gets no file chooser portal and cannot reach the host bus. `[apps.NAME] bus = "host-filtered"` keeps the filtered host bus. `paddock doctor --escape --app` checks the private bus, and `--bus host-filtered` checks the other one.
- App mode: a login page that an app opens through paddock and that returns to a localhost port reaches the app without `login_ports`, the same way as for terminal harnesses.
- Push and open pull requests from a sandbox without credentials in it. Plain `git push` goes to the new `git-push` host capability: the host pushes with your credentials after you see the remote, the branches, the commits and the force flag. Force pushes, pushes to the default branch and pushes to a remote that the sandbox names itself ask every time. Hooks never run on the host.
- The `forge` host capability runs a fixed set of `gh` commands (pull requests, issues, CI runs) on the project's own repository with your login. Reads run without a question and writes ask. Plain `gh` in the sandbox uses it.
- Inherit mode drops forge tokens such as `GH_TOKEN`, `GITHUB_TOKEN` and `GITLAB_TOKEN`, unless `[env] pass` or a profile names them.
- `paddock doctor --escape` checks that the sandbox has no git credentials, that `git push` becomes a `git-push` request, that a remote named in the sandbox is flagged, that a force push cannot get a session grant, and that a denied push exits with code 3.
- App worktrees: `paddock app run NAME --worktree`, or `[apps.NAME] worktree = true`, opens the first workspace folder in a new git worktree on the branch `paddock/<app>-<date>-<id>`. The folder dialog of a menu start offers it as the default for git repositories. When the app exits, paddock lists the worktree's commits and uncommitted changes.
- A sandbox started from a Claude app session no longer inherits the app's control channel (`CLAUDE_CODE_MESSAGING_TOKEN` and `CLAUDE_CODE_MESSAGING_SOCKET`).
- Network mode `egress` blocks the LAN by default: private, link-local and CGNAT ranges, on every backend. `[network] lan = true` allows it again. `paddock doctor --escape` checks it.
- Logging with levels. `-v` and `-q` change how much paddock prints, and `PADDOCK_LOG=debug` or `PADDOCK_LOG=broker=debug` sets the level for everything or for one part. The broker, the proxy, the login callback forwarder and the window border also write to the systemd journal: `journalctl -t paddock -p warning`.
- `paddock doctor --report` prints a diagnostic report for bug reports, without user names, paths, variable values or tokens.
- The bar plugin writes a warning to the shell log when it cannot read paddock's output. `PADDOCK_DEBUG=true` shows more.
- Sandbox artifacts: `paddock files` lists the files that a sandbox saved outside the project, in recovery folders such as `~/Desktop` and `~/Downloads`, and the project changes since the start. `paddock recover` copies them to the host without overwriting anything, and `paddock discard` deletes them. The Omarchy panel lists them and sends a notice when new ones appear. Set the folders in `[recovery]` of `~/.config/paddock/config.toml`.
- `paddock shims install` makes the sandbox the default: typing `claude`, `codex` and the other harness commands in bash or zsh starts them in a sandbox. `paddock shims remove` undoes it, and nothing breaks if paddock is removed first.
- A `paddock` agent skill for the host, next to `paddock-host`. Install both with `npx skills add harnessix/omarchy-paddock`.

## 0.1.0

The first public release.

- `paddock <harness>` runs a coding-agent harness with full auto-approval in a sandbox. The sandbox sees the project, a private home and nothing else of your files.
- Backends: bwrap (the default), docker, podman and srt.
- Network: outbound only. The bwrap backend uses pasta and blocks the addresses of the host. Containers reach the internet through a relay proxy only. Your system proxy and VPN routes stay as they are.
- Logins: isolated logins per harness, login callbacks through an approval, and a private keyring for harnesses that need one.
- Host requests: a sandbox can ask to open a link or use a host capability. You answer once, for the session, or deny, from an approval card or from `paddock approvals`.
- App mode: `paddock app run` runs a desktop app, such as VS Code, in a sandbox, with a marked window border.
- Displays for desktop projects: a private headless or window display, and `paddock-screenshot`.
- Launchers: `paddock launchers install` adds menu entries for the harnesses and apps that paddock finds.
- Omarchy bar plugin: lists sandboxes, starts new ones, shows recent requests and runs `paddock doctor`.
- `paddock doctor` and `paddock doctor --escape` test the setup and the sandbox limits.
