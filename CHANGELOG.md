# Changelog

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
