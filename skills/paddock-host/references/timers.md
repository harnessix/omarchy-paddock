# Timers and limits

A timer lives on the host, so it outlasts your context. `paddock-host timer --help` gives every option.

- `paddock-host timer start NAME --budget 2h` gives a task a time budget. `--quota weekly=1%` gives it a share of the account's limit window instead, or as well. The window names come from `paddock-host forage`. A quota counts everything that the account uses meanwhile, also elsewhere, split among the quota timers on the same window that run at the same time.
- Keep each task to about 1% of the weekly limit. If a piece of work needs more, split it into several small tasks, each with its own timer.
- `paddock-host timer extend NAME --by 30m` adds time, and `--by 1%` adds quota. On your own timer, it adds at once.
- A timer that someone else set (the user, or the agent that started you) is not yours. You cannot stop it. `paddock-host timer extend NAME --by 30m --reason "..."` asks the user for more. Say why in the reason.
- A hard timer freezes your sandbox when its budget is used up. Save your work and report before that.
- Timers in `--scope pasture` or `--scope tree` show the other agents there how long a shared task has run.

`paddock-host status` shows your timers and limits in a few lines. In Claude Code, paddock adds it as a hook, so you see it after a compaction and when something needs attention.

## Forage

Before you hand out a long task, run `paddock-host forage --refresh`. It shows how much of each account's limits is left, how fast it goes, and OVERGRAZING when a window runs out before it resets. Give the task to an account with room. When every account is overgrazing, keep tasks smaller.
