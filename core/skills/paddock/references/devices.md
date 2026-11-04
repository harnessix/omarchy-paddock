# Devices and questions of agents

Devices in `/dev/paddock/` of a sandbox are off until the user turns one on in `[devices]` of the user configuration. A device runs on the host, so it can reach a host service or cost money. Turn one on only when the user asks, and say what it does.

- `wait = true` lets agents wait for a time without `sleep`.
- `ask = true` lets agents put a question on the approval card and wait for the answer. Every agent of every sandbox can then show the user questions.
- The user can define `http` devices (a loopback service) and `command` devices (a fixed host command). The first request of each sandbox asks the user.
- `paddock devices ledger` shows what paid devices spent. `paddock devices budget` raises the budget of a paid device for a run. Run it only when the user asks.
- `paddock doctor` shows whether FUSE works on each backend.

## Questions

A question of an agent waits in `paddock approvals` like a host request. Do not answer it yourself: report it to the user. The user answers with `paddock approvals answer ID --text -` (the answer on standard input, so it stays out of the process list and the shell history), or dismisses it with `paddock approvals answer ID deny`.

## Waiting agents

Agents wait with blocking reads (`paddock-host kv watch`, `paddock-host wait`, `paddock-host device wait` and `device ask`). Exit code 75 in the sandbox means "still waiting": the read reached its timeout before the event, and the agent runs it again. It is not an error.
