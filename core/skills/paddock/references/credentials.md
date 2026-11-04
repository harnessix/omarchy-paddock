# Logins, variables and credentials

## Logins

| The user wants | Do this |
| :--- | :--- |
| A separate account for a harness | `paddock claude --auth isolated:work`, then log in inside the sandbox. The isolated login has its own machine ID in `.paddock-machine-id` of its login folder. Do not edit or delete that file: a login that sees another machine can need a new sign-in. |
| Codex says its login expired in a sandbox | paddock keeps the Codex refresh token outside the sandbox, and a keeper refreshes the login. Check `paddock doctor` and the notifications. `paddock logins refresh` refreshes it now. Do not copy `auth.json` by hand. `[logins] keep = false` turns the keeper off. |
| To start an isolated login from the host login | `paddock auth import` copies a harness's credential files from the home folder into an isolated store. |

## Environment variables

- `paddock env show` lists the host variables by what a sandbox does with them (names only).
- `paddock env hide NAME` stops passing a variable.
- A variable in `[env] pass` reaches every sandbox at every start, without a question. For a secret, register a credential instead.

## Credentials

Register a credential in `[credentials.NAME]` of `~/.config/paddock/config.toml` only when the user asks for it, and say what the sandbox gets:

- `via = "gateway"`: the value stays on the host. The sandbox gets a stand-in and a base URL, and only the hosts and paths that the user bound work. Prefer it, with a preset: `anthropic`, `openai`, `github` or `github-git` (git fetch and clone).
- `via = "deliver"`: the agent can read the value, and it cannot be taken back. With `as = "file"`, the sandbox reads it in `/dev/paddock/secret/NAME`.
- `via = "agent"`: an agent channel. `kind = "ssh"` gives the ssh agent, `kind = "docker"` the login of listed registries, and `kind = "aws"` short-lived AWS credentials. The key or token stays on the host.

A credential can need the user's answer on a start card. Never answer it for the user. `paddock credentials list` shows the registered credentials and how often each was used, never a value. `paddock credentials revoke ID [NAME]` stops a running sandbox from using one at once.

The README section "Credentials" has the full key list and examples.
