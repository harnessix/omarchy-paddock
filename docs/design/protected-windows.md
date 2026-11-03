# Protected windows on Omarchy

This note records the visual design of the protected window marks on Omarchy (Hyprland). A protected window is a host window that shows a paddock sandbox: an app sandbox window, the nested display of window mode, or the terminal window that holds a paddock run.

## Parts

1. A border on every protected window, always on. The color tells the network level.
2. A corner badge on demand. After a protected window gets focus, the badge shows and then fades out. A click on it opens that sandbox in the bar panel.
3. A shield with the count of protected windows in the bar widget.
4. A "Protected windows" list in the bar panel. A click on a row focuses that window.

## Precedents

- Qubes OS colors the border of each window by the trust domain of the qube that owns it. The color comes from the domain, not from the app, and the user cannot get a window without it. paddock takes this idea for the border: always on, set by the compositor side, one color per trust level.
- The VS Code Remote indicator is a small colored block in a fixed corner with a short text, for example `SSH: host`. A click opens the remote menu. paddock takes the short text with a click target for the badge.
- Firefox containers give each container a color and draw a colored line under the tab. The color is a category, and the label stays short. paddock keeps two colors only, so the meaning stays clear.
- The macOS privacy dot is a small dot in a fixed place that says "something watches". It never covers content. paddock takes the dot inside the badge and the rule that the badge never stays over the content.
- Refero style research: Hyperstudio (hyperstudio.org) uses amber for small status markers and muted green for a good state, on near-black surfaces with monospace data text. Linear's changelog uses full-round capsules for pills, and borders and tone shifts in place of shadows.

## Reference lock

- Primary direction: Qubes domain borders. The border is the always-on signal, and the color is a trust level.
- Preserve: two semantic hues only (sage, amber). The border on every protected window. The pill text `paddock · <profile> · <network> · <age>`, aligned with the macOS side.
- Borrow only: the capsule shape and the 1 px tinted outline from Linear. The status dot from the macOS privacy dot.
- Role rules: sage and amber mean a network level and nothing else. The panel's urgent color stays for requests that need an answer. The theme accent is not used for protection.
- Reject: a new palette for the badge surface, shadows, glow, a badge that stays over the window, a third color for "very safe".

## Theme

The badge and the panel rows take their surface, text and font from the Omarchy theme (`Commons.Color`, `Style`). So they follow every theme. Only the two semantic hues are paddock's own. paddock reads the theme mode: `mode` in `colors.toml` of the current theme, or the brightness of its `background`. On light themes, paddock picks a deeper tone of each hue. The border keeps its contrast.

| Level | Network modes | Dark theme | Light theme |
|-------|---------------|------------|-------------|
| proxy (sage) | none, api, egress | `#9fb8a0` to `#7fd29a` | `#4f7a55` to `#3f8f5a` |
| wide (amber) | host, allowlist, other | `#e0a33a` to `#f2c46d` | `#a8701a` to `#c4861f` |

An unknown network mode counts as wide. A wrong "protected" signal is worse than a careful one.

## Decision ledger

| Decision | Source | Why |
|----------|--------|-----|
| Border on every protected window, always on | User brief, Qubes | The signal must be there without a hover or a click. |
| Sage for none, api and egress. Amber for host and allowlist | User brief | Traffic through the proxy is the protected state. A wider network needs care. |
| Gradient border, 45 degrees, with a dimmer inactive border | Existing sage border | It keeps the current look and tells the focused window apart. |
| Deeper tones on light themes | Theme constraint, color craft | Light sage on a light background is near 1.6:1 contrast. |
| Badge straddles the top edge at the right corner | VS Code Remote, privacy dot | Half of it sits on the gap and the border, so it covers little content. |
| Badge shows for 4 seconds after focus, and while the pointer is on it | User brief | It never covers the content for long. Omarchy focuses the window under the pointer, so focus covers hover too. |
| Badge surface from the theme background, 1 px outline in the level hue, a dot | Linear capsules, privacy dot | It matches each theme and keeps the hue for the meaning. |
| Text `paddock · <pasture or profile> · <network> · <age>` | User brief, macOS side | One format on both platforms. |
| Bar shows a shield with the window count | User brief | One glance tells how many windows are protected. |
| Panel row "Protected windows" opens a list. A click focuses | User brief | The same row pattern as "Shared data" and "Worktrees". |
| A terminal counts only if its terminal emulator holds the pty of the run | `src/relay.rs` | A background pty of a desktop app must not make that app look protected. |
| A terminal process with several windows marks none of them | Review | Ghostty and `foot --server` draw all windows from one process. A wrong mark is worse than no mark. |
| An app window must run in another mount namespace than paddock | Review | A host helper in the app's process tree, such as the gateway daemon, must not mark its host programs. |
| After the run ends, the mark goes away | Craft | A terminal outlives its run. A stale border tells a lie. |
