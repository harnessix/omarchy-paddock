//! `paddock-host spawn`: the arguments of a start request, and the help.

use std::collections::BTreeMap;

pub const HELP: &str = r#"paddock-host spawn: start another harness in its own sandbox with one task.

  paddock-host spawn HARNESS --prompt-file FILE [--role implementer|reviewer]
                     [--from-head] [--timeout TIME] [--mem SIZE] [--cpus N]
                     [--model NAME] [--effort low|medium|high|xhigh]
                     [--worktree] [--reason TEXT]
  paddock-host spawn HARNESS --task ID|next [--feature NAME] [OPTIONS]
  paddock-host status [ID]                 one child, or all of them
  paddock-host wait ID [--timeout SECONDS]
  paddock-host kill ID

spawn prints the id of the child. The child is a sibling sandbox on the host,
not a sandbox inside this one. It runs the harness once, without a terminal,
and stops when the task is done.

  - --task takes one task of the Spec Kit feature (a project with .specify/):
    an id such as T012, or next for the next ready task. paddock claims the
    task and writes the prompt from tasks.md, the spec, the plan, the
    contracts and the constitution. Run `paddock-host help spec`.
  - FILE is a file in the project with the task. Write the whole task there:
    the child does not see this conversation. The prompt never goes into a
    command line.
  - The child works in a new git worktree on the branch
    paddock/spawn-HARNESS-ID, from the commit that this sandbox has checked
    out. With --from-head, paddock first makes a commit of your files as they
    are now, uncommitted and new files included (not the ignored ones), and
    the child starts from it. Your files, index and branches stay as they
    are. --worktree is the default and the only mode.
  - Tell the child to commit its work. Uncommitted changes stay in its
    worktree, and you cannot see them.
  - --role sets fixed limits. implementer (the default): its own writable
    worktree. reviewer: the files are read-only, and it cannot push or change
    anything with gh; use it to review or compare. Under a parent with
    network egress, a reviewer gets network mode api: only its model API.
    Both roles get a clean environment: only the allowlist and the
    harness's own variables.
  - --timeout (600, 30m, 2h), --mem (512M, 4G) and --cpus (2, 0.5) can only
    lower the limits in the user's configuration. A systemd scope enforces
    them when the host has `systemd-run --user`; without it, paddock enforces
    the timeout and refuses --mem and --cpus.
  - --model and --effort choose the model and the reasoning effort of the
    child, through the harness's own flags. A harness whose profile has no
    [oneshot] model_args or effort_args refuses them. Without them, a --task
    child takes the hints [model:NAME] and [effort:LEVEL] of its task line,
    then [spawn] effort of the user configuration, then the harness
    default. [spawn] models can limit the models a child may get.
  - The child uses its own login of that harness. You never see it.
  - The child never gets more than this sandbox: the same network, backend
    and mounts, no display, and host capabilities that are at most as open
    as yours. Its requests show the user that you started it.
  - The user's configuration decides which harnesses are allowed, how many
    children run at the same time, how long one may run, and whether a child
    may start children of its own (by default it may not).

When the child ends, its results are in .paddock/inbox/spawn-ID/:
  result.json  state, exit code, branch, commit count, diff summary
  output.txt   the end of the child's final output

Read the branch with git, for example `git log --stat paddock/spawn-codex-ID`,
and merge what you want to keep.

paddock also writes result:ID into the tree scope of paddock kv (status,
summary, ref). With several children, wait for all of them at once:
  paddock-host kv watch --prefix result: --scope tree --timeout 600
Children read your own kv scope with --scope parent, and share tasks with you
in --scope tree. Run `paddock-host kv --help` for the key names.

wait exits with 0 when the child ended with exit code 0, 1 when it failed or
was stopped, and 75 (still waiting) when it still runs after --timeout: run
the same wait again. A child stops when this
sandbox ends. Use `paddock-host capabilities --json` to see the limits."#;

/// The arguments, the reason and `--json` of `spawn HARNESS ...`.
pub(super) fn args(
    rest: &[String],
) -> Result<(BTreeMap<String, serde_json::Value>, String, bool), String> {
    // A child always gets a worktree; the flag only says so.
    let from_head = rest.iter().any(|a| a == "--from-head");
    let args: Vec<String> = rest
        .iter()
        .filter(|a| *a != "--worktree" && *a != "--from-head")
        .cloned()
        .collect();
    let p = super::parse(
        &args,
        &[
            "prompt-file",
            "task",
            "feature",
            "role",
            "timeout",
            "mem",
            "cpus",
            "model",
            "effort",
        ],
    )?;
    let [harness] = p.rest.as_slice() else {
        return Err(
            "spawn needs one harness name, for example: spawn codex --prompt-file task.md".into(),
        );
    };
    let mut a = BTreeMap::new();
    a.insert("action".into(), serde_json::json!("start"));
    a.insert("harness".into(), serde_json::json!(harness));
    match (p.opts.get("prompt-file"), p.opts.get("task")) {
        (Some(_), Some(_)) => return Err("use --prompt-file or --task, not both".into()),
        (None, None) => return Err(
            "spawn needs --prompt-file FILE with the task, or --task ID|next in a Spec Kit project"
                .into(),
        ),
        (Some(file), None) => {
            a.insert("prompt_file".into(), serde_json::json!(file));
        }
        (None, Some(t)) => {
            if t != "next" && !crate::speckit::tasks::is_id(t) {
                return Err(format!(
                    "--task takes a task id such as T012, or next; not '{t}'"
                ));
            }
            a.insert("task".into(), serde_json::json!(t));
        }
    }
    if let Some(f) = p.opts.get("feature") {
        a.insert("feature".into(), serde_json::json!(f));
    }
    a.insert("from_head".into(), serde_json::json!(from_head));
    if let Some(r) = p.opts.get("role") {
        let role: crate::spawn::Role = r.parse().map_err(|e| format!("{e:#}"))?;
        a.insert("role".into(), serde_json::json!(role.name()));
    }
    let bad = |e: anyhow::Error| format!("{e:#}");
    if let Some(t) = p.opts.get("timeout") {
        let t = crate::spawn::parse_duration(t).map_err(bad)?;
        a.insert("timeout".into(), serde_json::json!(t));
    }
    if let Some(m) = p.opts.get("mem") {
        let m = crate::spawn::parse_bytes(m).map_err(bad)?;
        a.insert("mem".into(), serde_json::json!(m));
    }
    if let Some(c) = p.opts.get("cpus") {
        let c = crate::spawn::parse_cpus(c).map_err(bad)?;
        a.insert("cpu_percent".into(), serde_json::json!(c));
    }
    if let Some(m) = p.opts.get("model") {
        if !crate::spawn::model::valid_model(m) {
            return Err(format!(
                "--model '{m}' is no model name: use letters, digits and .-_:/@"
            ));
        }
        a.insert("model".into(), serde_json::json!(m));
    }
    if let Some(e) = p.opts.get("effort") {
        let e: crate::spawn::model::Effort = e.parse().map_err(bad)?;
        a.insert("effort".into(), serde_json::json!(e.name()));
    }
    Ok((a, p.reason, p.json))
}

#[cfg(test)]
mod tests {
    fn args(v: &[&str]) -> Result<serde_json::Value, String> {
        let v: Vec<String> = v.iter().map(|s| s.to_string()).collect();
        super::args(&v).map(|(a, _, _)| serde_json::to_value(a).unwrap())
    }

    #[test]
    fn model_and_effort_reach_the_request() {
        let a = args(&[
            "codex", "--task", "next", "--model", "gpt-5.5", "--effort", "high",
        ])
        .unwrap();
        assert_eq!(a["model"], "gpt-5.5");
        assert_eq!(a["effort"], "high");
        let a = args(&["codex", "--task", "next"]).unwrap();
        assert!(a.get("model").is_none() && a.get("effort").is_none());
        let e = args(&["codex", "--task", "next", "--effort", "max"]).unwrap_err();
        assert!(e.contains("low, medium, high or xhigh"), "{e}");
        let e = args(&["codex", "--task", "next", "--model", "a\"b"]).unwrap_err();
        assert!(e.contains("no model name"), "{e}");
    }
}
