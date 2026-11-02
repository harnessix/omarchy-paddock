# paddock and Omarchy's agents

`paddock shims install --scope omarchy-agent` puts only the agents that `omarchy-agent` starts in a sandbox: the agent key, the console, the agents panel and the alias `a`. Typed commands and scripts do not change. Ask the user first, and tell them what it changes:

- One marked block in `~/.config/mise/config.toml`. From then on, every agent start from Omarchy runs through `paddock wrap`.
- One marked block at the end of `~/.config/hypr/hyprland.lua`. From the next login, the panel's limit checks go through paddock.

`paddock shims remove` undoes it. If paddock reports that the file already has its own `[wrappers.omarchy-agent]`, do not edit or remove the user's entry. Tell the user.

## The agents panel

- Sessions of claude, codex and grok that `omarchy-agent` starts reach the host history, so the panel counts them. claude gets only the host folder of its own project in `~/.claude/projects`. codex and grok copy new history back every five minutes and at exit. Do not bind more history folders by hand.
- The panel never shows an isolated login (`--auth isolated:NAME`). `paddock doctor` lists them.
- While a sandbox holds the login of Claude Code or Grok Build, paddock checks its limits from the sandbox, and the host does not. A held Codex login is not checked until the sandbox exits. `paddock doctor` shows the state.
- Do not run `omarchy-agent-usage-update --force` or a collector to test this. Each run asks the vendor with the user's login.

## Agents without a sandbox

Every agent that `omarchy-agent` can start has a profile. muse and openclaw still start on the host, with a notice: muse needs its own sandbox, and the agent of openclaw runs in a host service. Do not try to turn off muse's sandbox to make it fit. Many profiles come from documentation. `paddock doctor` lists what is not verified.
