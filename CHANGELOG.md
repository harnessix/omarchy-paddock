# Changelog

## Unreleased

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
