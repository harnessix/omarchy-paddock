//! `paddock-host`: the client that runs inside a sandbox. It sends one
//! request to the host broker and prints the result. The help text is the full
//! reference, so an agent never needs to read this source to use it.

use crate::broker::{Request, Response, Status, PROTOCOL, SOCKET_IN_SANDBOX};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

pub mod device;
pub mod forage;
mod git;
pub mod kv;
pub mod pasture;
pub mod status;
pub mod timer;

pub const EXIT_USAGE: i32 = 2;
pub const EXIT_NO_HOST: i32 = 6;

pub const HELP: &str = r#"paddock-host: ask the host to do something that the sandbox cannot do.

You are inside a paddock sandbox. You see only the project, your own home
directory and package caches. paddock-host sends one request to paddock on the
host. Depending on the user's configuration, the host does it at once, asks the
user first, or refuses. Nothing here opens the sandbox itself.

USAGE
  paddock-host capabilities [--json]
  paddock-host boundary [--json]   (what the sandbox boundary denied)
  paddock-host screenshot [--target screen|window|output:NAME] [--out FILE] [--reason TEXT]
  paddock-host exec [--timeout SECONDS] [--reason TEXT] -- COMMAND [ARG...]
  paddock-host tool --list | tool [--timeout SECONDS] [--reason TEXT] NAME [ARG...]
  paddock-host fetch HOST_PATH [--reason TEXT]
  paddock-host borrow HOST_FOLDER [--reason TEXT]
  paddock-host return NAME [--reason TEXT]
  paddock-host clipboard get [--reason TEXT]
  paddock-host clipboard set TEXT [--reason TEXT]
  paddock-host open URL [--reason TEXT]
  paddock-host notify TITLE [BODY]
  paddock-host git push [REMOTE] [REFSPEC...]   (plain `git push` works too)
  paddock-host gh COMMAND [ARG...]              (plain `gh` works too)
  paddock-host spawn HARNESS --prompt-file FILE [--role ROLE] [--from-head] [--reason TEXT]
  paddock-host status [ID]
  paddock-host wait ID [--timeout SECONDS]
  paddock-host kill ID
  paddock-host kv COMMAND [ARG...]              (key-value store, see `paddock-host kv --help`)
  paddock-host timer COMMAND [ARG...]           (task timers, see `paddock-host timer --help`)
  paddock-host pasture share|unshare PORT       (ports for your pasture, see `paddock-host pasture --help`)
  paddock-host pasture members
  paddock-host forage [--refresh] [--json]      (limits left and how fast they go)
  paddock-host status                           (your timers and limits in a few lines)
  paddock-host device NAME [REQUEST]            (a device in /dev/paddock, see `paddock-host device --help`)
  paddock-host help [CAPABILITY]

Common options
  --reason TEXT   One sentence that tells the user why you ask. The user sees it
                  next to the request. Requests with a clear reason get answered.
  --json          Print the full response as one JSON object (all commands).

FIRST STEP
  Run `paddock-host capabilities --json` to see which capabilities exist and
  whether each one is allowed, asks the user, or is denied. Do not send
  requests for a capability whose mode is "deny": they fail at once.

WAITING
  A request in "ask" mode waits until the user answers on the host: in the
  Omarchy bar panel, in a desktop notification, or with `paddock approvals`.
  The wait is at most the time in "wait_seconds" (default 120). Do not run two
  requests at the same time. Do not retry a denied request: ask the user in
  the conversation instead.

RESULTS
  Files (screenshots, fetched files) are written to .paddock/inbox/ in the
  project. That folder has its own .gitignore, so results are never committed.
  The command prints the path of the file. exec prints the command's stdout to
  stdout and its stderr to stderr, and exits with the command's exit code.

PUSH AND PULL REQUESTS
  The sandbox has no git credentials and no gh login, and it never gets them.
  `git push` and `gh` still work: the host does the push or runs gh with the
  user's own login, after the user approves. Run `paddock-host help git-push`
  and `paddock-host help forge` for the rules.

OTHER AGENTS
  `paddock-host spawn` starts another harness, for example codex, in its own
  sandbox with one task. It works on its own branch and returns its results
  to .paddock/inbox/. Run `paddock-host help spawn` for the rules and limits.

STATE AND DEVICES
  `paddock-host kv` keeps state on the host: for this sandbox, for every
  sandbox of this project, for the sandbox that started this one, or for a
  whole spawn tree (--scope tree: tasks for children and their results). It
  needs no approval, except the scope that all projects share. Host devices, when
  the user turns them on, are files in /dev/paddock. Read
  /dev/paddock/.catalog for each device and its request format.

EXIT CODES
  0   done (for exec and gh: the command's own exit code is returned instead)
  2   wrong usage of paddock-host
  3   the user denied the request
  4   the capability is set to deny in the user's configuration
  5   nobody answered in time
  6   no host connection: not inside a paddock sandbox, or paddock is too old
  7   the host tried and failed; the message says why
  8   the host rejected the request as invalid, for example a protected path
  75  still waiting: a blocking read (wait, kv watch, a device) reached its
      --timeout before the event. Run the same command again.

WAITING FOR AN EVENT
  Do not wait with sleep. A blocking read returns the moment the event
  happens and uses no tokens while it waits: `paddock-host wait ID` for a
  child, `paddock-host kv watch` for a key, `paddock-host device wait TIME`
  for a time and `paddock-host device ask QUESTION` for an answer of the
  user. Give each one a --timeout below the time limit of your shell tool.
  Exit code 75 means that the time passed first: run the same read again.

EXAMPLES
  paddock-host screenshot --reason "check the layout of the settings window"
  paddock-host screenshot --target window --out settings.png
  paddock-host exec --reason "restart the dev database" -- systemctl --user restart postgres
  paddock-host fetch /etc/hosts --reason "compare host name resolution"
  paddock-host borrow /srv/shared-lib --reason "fix the parser bug there"
  paddock-host return shared-lib --reason "the parser fix, tests pass"
  paddock-host open https://example.com/docs
  paddock-host notify "Build finished" "All 214 tests passed"
  git push -u origin fix-login
  gh pr create --title "Fix the login timeout" --body "Closes #12"
  gh pr checks --watch
"#;

fn cap_help(cap: &str) -> Option<&'static str> {
    Some(match cap {
        "screenshot" => {
            r#"paddock-host screenshot: capture the host screen into the project.

  --target screen      every output (default)
  --target output:NAME one output, for example output:eDP-1
  --target window      the window that has focus on the host (Hyprland only)
  --out FILE           file name in .paddock/inbox/ (default screenshot-TIME.png)

Prints the path of the PNG file. Read it with your image tools.
The sandbox's own display, if it has one, is captured with paddock-screenshot
instead; that needs no approval."#
        }
        "exec" => {
            r#"paddock-host exec: run one command on the host.

  paddock-host exec [--timeout SECONDS] -- COMMAND [ARG...]

The command runs on the host, in the project directory, without a shell, in
an isolated unit: the system is read-only, the project read-write, the home
folder empty (no keys, browsers or other projects), and only PATH, the
language and the time zone pass from the host environment. The user can
widen this in [exec] of their configuration. To use shell syntax, run
sh -c 'TEXT' as the command. The user sees the exact command line before it runs. "Allow for this
session" covers only the identical command line. The configuration cannot set
exec to allow: every new command line needs an answer.
--timeout stops the command after SECONDS (default 120, at most 3600).
stdout and stderr are returned (at most 1 MiB each), and paddock-host exits
with the command's exit code."#
        }
        "tool" => {
            r#"paddock-host tool: run a named host tool that the user defined.

  paddock-host tool --list
  paddock-host tool [--timeout SECONDS] [--reason TEXT] NAME [ARG...]

The user defines tools in [host.tools] of their configuration: a fixed host
command and the argument shapes that it accepts. --list shows each tool, its
mode and its shapes. Options go before NAME; every word after NAME is an
argument. The arguments must match one shape word by word: `*` is one word
that does not start with -, `**` (last) is the rest of the words, none
starting with -. A tool without shapes takes no arguments. Other arguments are
refused at once, without a question to the user. Do not try variants: ask the
user to add a shape instead.
The tool runs like exec: isolated, without your environment, in the project
folder or the user's home. Mode allow runs it at once, ask shows the user the
tool and the exact arguments, deny refuses. stdout and stderr are returned,
and paddock-host exits with the tool's exit code."#
        }
        "fetch" => {
            r#"paddock-host fetch: copy one host file or directory into the project.

  paddock-host fetch HOST_PATH

HOST_PATH must be absolute. The copy goes to .paddock/inbox/ and is a one-time
snapshot. Symbolic links inside a directory are skipped. The limit is 64 MiB.
Protected paths (~/.ssh, ~/.gnupg, cloud credentials, browser profiles and the
other paths that paddock never mounts) are refused even if the user would
allow them."#
        }
        "borrow" | "return" => {
            r#"paddock-host borrow and return: change a host folder outside the project.

  paddock-host borrow HOST_FOLDER   copy the folder to /run/paddock/borrow/NAME
  paddock-host return NAME          write the changes of the copy back

HOST_FOLDER must be absolute, and its last part is no symlink. borrow prints
the path of the copy. Change the copy as you like. The original does not
change. return shows the user every added, changed and deleted file, and
writes them back on approval. It refuses when the original changed after the
borrow: borrow it again and redo the changes. A return never writes git
config, hooks or pointer files, gives new files no execute bit, and refuses
symlinks that point outside the folder. After a return the copy is gone.
Copies that are not returned are deleted when the sandbox ends.
Protected paths (~/.ssh, cloud credentials, browser profiles and the others
that fetch refuses) and folders that hold one are refused. Without reflinks
the limit is [borrow] max of the user configuration (default 1G)."#
        }
        "clipboard-read" | "clipboard" => {
            r#"paddock-host clipboard: read or set the host clipboard.

  paddock-host clipboard get       print the clipboard text
  paddock-host clipboard set TEXT  replace the clipboard text

The capabilities are named clipboard-read and clipboard-write."#
        }
        "open-url" | "open" => {
            r#"paddock-host open URL: open an http or https URL in the host browser.

The capability is named open-url. Other schemes are refused."#
        }
        "notify" => {
            r#"paddock-host notify TITLE [BODY]: show a desktop notification on the host.

Allowed by default. Use it to tell the user that long work is done."#
        }
        "git-push" | "git" | "push" => {
            r#"git push: the host pushes your commits with the user's credentials.

  git push [REMOTE] [REFSPEC...]
  paddock-host git push [REMOTE] [REFSPEC...]

Use plain `git push`. The sandbox git configuration sends every push to an
https, ssh or git@ URL, and to every remote of the project, to the host. The
user sees the remote URL, each local and remote branch, the commits (at most
20) and whether the push is a force push, then answers.

  - Force pushes, deleting a branch and pushes to the default branch (main,
    master or the remote HEAD) need an answer every time.
  - "Allow for this session" covers only the same remote and branch.
  - A remote that you add in the sandbox (git remote add) is marked as new
    and needs an answer every time. A changed URL of an existing remote is
    ignored: the host pushes to the URL that it had when the sandbox started.
  - Hooks never run on the host. Run your checks before you push.
  - --force-with-lease, --dry-run, --atomic and push options are not
    supported. Use --force if you must, and expect a question.

Set PADDOCK_PUSH_REASON to tell the user why you push. `paddock-host git
push` exits with the codes below; plain `git push` exits with 1 when the
host refuses."#
        }
        "forge" | "gh" | "tea" | "glab" => {
            r#"gh: pull requests, issues and CI runs, run on the host.

  gh COMMAND [ARG...]
  paddock-host gh COMMAND [ARG...]

Use plain `gh`. The host runs it with the user's login; the token never
enters the sandbox. Only these commands exist:

  read (no question):  pr view|list|checks|diff, issue view|list,
                       run view|watch|list, repo view
  write (asks first):  pr create, pr comment, issue comment

  - The repository is always one of the project's remotes. --repo can choose
    another remote's repository, but never an unrelated one.
  - Commands that default to the current branch (pr view, pr checks, pr diff,
    pr comment, and --head of pr create) get the sandbox's branch. Push the
    branch first.
  - --body-file is read in the sandbox and sent as --body. --web, --editor
    and --template-file are refused.
  - gh api, gh auth, gh secret and every other command are refused.

tea (Gitea and Forgejo) works the same way when the user has it on the host:

  read (no question):  pulls list, pulls INDEX, issues list, issues INDEX,
                       releases list
  write (asks first):  pulls create, comment INDEX TEXT

  - --login, --remote and --web are refused. The host uses its own tea login
    for the repository's host.
  - pulls create gets --head with the sandbox's branch.

Extra options: --paddock-reason TEXT, --paddock-timeout SECONDS (default 120,
1800 for watch), --paddock-json. The exit code is the tool's own."#
        }
        "spawn" | "status" | "wait" | "kill" => {
            r#"paddock-host spawn: start another harness in its own sandbox with one task.

  paddock-host spawn HARNESS --prompt-file FILE [--role implementer|reviewer]
                     [--from-head] [--timeout TIME] [--mem SIZE] [--cpus N]
                     [--worktree] [--reason TEXT]
  paddock-host status [ID]                 one child, or all of them
  paddock-host wait ID [--timeout SECONDS]
  paddock-host kill ID

spawn prints the id of the child. The child is a sibling sandbox on the host,
not a sandbox inside this one. It runs the harness once, without a terminal,
and stops when the task is done.

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
sandbox ends. Use `paddock-host capabilities --json` to see the limits."#
        }
        _ => return None,
    })
}

/// Send one request to the host: through the gateway device
/// `/dev/paddock/gw` when the sandbox has it, else through the host socket.
pub fn send(req: &Request) -> Result<Response, String> {
    // A unit test talks to its own broker. Run in a paddock sandbox, the
    // gateway would take the request to the real host instead.
    if cfg!(test) && std::env::var_os("PADDOCK_HOST_SOCKET").is_some() {
        return send_socket(req);
    }
    let gw = device::root().join("gw");
    if gw.exists() {
        return send_gateway(req);
    }
    send_socket(req)
}

/// Through the gateway. A read that reaches its time limit (still waiting)
/// sends the same request again, which attaches to the one that waits.
/// When the device fails, paddock-host does not fall back to the socket:
/// the request may have run.
fn send_gateway(req: &Request) -> Result<Response, String> {
    let body = serde_json::to_vec(req).map_err(|e| e.to_string())?;
    loop {
        match device::call("gw", &body) {
            Ok(out) => {
                return serde_json::from_slice(&out)
                    .map_err(|e| format!("bad response from the host: {e}"))
            }
            Err(e) if e == crate::waiting::ERRNO_STILL_WAITING => continue,
            Err(e) => {
                return Err(format!(
                    "the gateway /dev/paddock/gw failed: {}",
                    device::describe(e)
                ))
            }
        }
    }
}

/// Through the host socket, for a sandbox without `/dev/paddock`.
fn send_socket(req: &Request) -> Result<Response, String> {
    let path =
        std::env::var("PADDOCK_HOST_SOCKET").unwrap_or_else(|_| SOCKET_IN_SANDBOX.to_string());
    let mut s =
        UnixStream::connect(&path).map_err(|e| format!("cannot reach the host at {path}: {e}"))?;
    let line = serde_json::to_string(req).map_err(|e| e.to_string())?;
    writeln!(s, "{line}").map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(s);
    let mut out = String::new();
    reader.read_line(&mut out).map_err(|e| e.to_string())?;
    serde_json::from_str(out.trim()).map_err(|e| format!("bad response from the host: {e}"))
}

struct Parsed {
    json: bool,
    reason: String,
    opts: BTreeMap<String, String>,
    rest: Vec<String>,
}

fn parse(args: &[String], with_values: &[&str]) -> Result<Parsed, String> {
    let mut p = Parsed {
        json: false,
        reason: String::new(),
        opts: BTreeMap::new(),
        rest: vec![],
    };
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--" {
            p.rest.extend(args[i + 1..].iter().cloned());
            break;
        } else if a == "--json" {
            p.json = true;
        } else if a == "--reason" {
            p.reason = args.get(i + 1).ok_or("--reason needs a value")?.clone();
            i += 1;
        } else if let Some(name) = a.strip_prefix("--") {
            if !with_values.contains(&name) {
                return Err(format!("unknown option {a}"));
            }
            p.opts.insert(
                name.to_string(),
                args.get(i + 1).ok_or(format!("{a} needs a value"))?.clone(),
            );
            i += 1;
        } else {
            p.rest.push(a.clone());
        }
        i += 1;
    }
    Ok(p)
}

fn usage(msg: &str) -> i32 {
    eprintln!("paddock-host: {msg}\nRun `paddock-host --help` for the full reference.");
    EXIT_USAGE
}

/// The entry point. Returns the process exit code.
pub fn main(args: &[String]) -> i32 {
    let Some(cmd) = args.first().map(String::as_str) else {
        print!("{HELP}");
        return EXIT_USAGE;
    };
    let rest = &args[1..];
    match cmd {
        "git" => return git::git_main(rest),
        "git-remote-helper" => return git::remote_helper(rest),
        "gh" => return git::gh_main(rest),
        "kv" => return kv::main(rest),
        "timer" => return timer::main(rest),
        "pasture" => return pasture::main(rest),
        "forage" => return forage::main(rest),
        "status" => return status::main(rest),
        "device" => return device::main(rest),
        "tea" | "glab" => return git::forge_main(cmd, rest),
        "forge"
            if rest
                .first()
                .is_some_and(|t| crate::broker::forge::tool(t).is_some()) =>
        {
            return git::forge_main(&rest[0], &rest[1..])
        }
        _ => {}
    }
    if matches!(cmd, "-h" | "--help" | "help") {
        match rest.first() {
            Some(c) => match cap_help(c) {
                Some(t) => println!("{t}"),
                None => return usage(&format!("no help for '{c}'")),
            },
            None => print!("{HELP}"),
        }
        return 0;
    }
    let help_asked = if cmd == "tool" {
        rest.first().is_some_and(|a| a == "-h" || a == "--help")
    } else {
        rest.iter().any(|a| a == "-h" || a == "--help")
    };
    if help_asked {
        println!("{}", cap_help(cmd).unwrap_or(HELP));
        return 0;
    }
    let req = |cap: &str, reason: String, args: BTreeMap<String, serde_json::Value>| Request {
        v: PROTOCOL,
        cap: cap.to_string(),
        args,
        reason,
    };
    let built = (|| -> Result<(Request, bool), String> {
        Ok(match cmd {
            "capabilities" => {
                let p = parse(rest, &[])?;
                (req("capabilities", String::new(), BTreeMap::new()), p.json)
            }
            "boundary" => {
                let p = parse(rest, &[])?;
                (req("boundary", String::new(), BTreeMap::new()), p.json)
            }
            "screenshot" => {
                let p = parse(rest, &["target", "out"])?;
                let mut a = BTreeMap::new();
                for k in ["target", "out"] {
                    if let Some(v) = p.opts.get(k) {
                        a.insert(k.to_string(), serde_json::json!(v));
                    }
                }
                (req("screenshot", p.reason, a), p.json)
            }
            "exec" => {
                let p = parse(rest, &["timeout"])?;
                if p.rest.is_empty() {
                    return Err("exec needs a command after --".into());
                }
                let mut a = BTreeMap::new();
                a.insert("argv".to_string(), serde_json::json!(p.rest));
                if let Some(t) = p.opts.get("timeout") {
                    a.insert(
                        "timeout".into(),
                        serde_json::json!(t
                            .parse::<u64>()
                            .map_err(|_| "--timeout needs a number")?),
                    );
                }
                (req("exec", p.reason, a), p.json)
            }
            "tool" => {
                let mut opts = Vec::new();
                let mut words = rest.iter();
                let mut list = false;
                let mut name = None;
                while let Some(a) = words.next() {
                    match a.as_str() {
                        "--list" => list = true,
                        "--json" => opts.push(a.clone()),
                        "--reason" | "--timeout" => {
                            opts.push(a.clone());
                            opts.push(words.next().ok_or(format!("{a} needs a value"))?.clone());
                        }
                        "--" => {
                            name = words.next().cloned();
                            break;
                        }
                        o if o.starts_with('-') => return Err(format!("unknown option {o}")),
                        _ => {
                            name = Some(a.clone());
                            break;
                        }
                    }
                }
                let p = parse(&opts, &["timeout"])?;
                if list {
                    if name.is_some() {
                        return Err("tool --list takes no tool name".into());
                    }
                    let mut a = BTreeMap::new();
                    a.insert("list_tools".into(), serde_json::json!(true));
                    (req("capabilities", String::new(), a), p.json)
                } else {
                    let name =
                        name.ok_or("tool needs a tool name; see paddock-host tool --list")?;
                    let mut a = BTreeMap::new();
                    a.insert("name".into(), serde_json::json!(name));
                    a.insert(
                        "args".into(),
                        serde_json::json!(words.cloned().collect::<Vec<_>>()),
                    );
                    if let Some(t) = p.opts.get("timeout") {
                        a.insert(
                            "timeout".into(),
                            serde_json::json!(t
                                .parse::<u64>()
                                .map_err(|_| "--timeout needs a number")?),
                        );
                    }
                    (req("tool", p.reason, a), p.json)
                }
            }
            "fetch" => {
                let p = parse(rest, &[])?;
                let [path] = p.rest.as_slice() else {
                    return Err("fetch needs exactly one host path".into());
                };
                let mut a = BTreeMap::new();
                a.insert("path".into(), serde_json::json!(path));
                (req("fetch", p.reason, a), p.json)
            }
            "borrow" => {
                let p = parse(rest, &[])?;
                let [path] = p.rest.as_slice() else {
                    return Err("borrow needs exactly one host folder".into());
                };
                let mut a = BTreeMap::new();
                a.insert("path".into(), serde_json::json!(path));
                (req("borrow", p.reason, a), p.json)
            }
            "return" => {
                let p = parse(rest, &[])?;
                let [name] = p.rest.as_slice() else {
                    return Err("return needs the name that borrow printed".into());
                };
                // The path of the copy works too.
                let name = name
                    .trim_end_matches('/')
                    .rsplit('/')
                    .next()
                    .unwrap_or_default();
                let mut a = BTreeMap::new();
                a.insert("name".into(), serde_json::json!(name));
                (req("return", p.reason, a), p.json)
            }
            "clipboard" => {
                let p = parse(rest, &[])?;
                match p.rest.as_slice() {
                    [g] if g == "get" => (req("clipboard-read", p.reason, BTreeMap::new()), p.json),
                    [s, text] if s == "set" => {
                        let mut a = BTreeMap::new();
                        a.insert("text".into(), serde_json::json!(text));
                        (req("clipboard-write", p.reason, a), p.json)
                    }
                    _ => return Err("use: clipboard get | clipboard set TEXT".into()),
                }
            }
            "open" => {
                let p = parse(rest, &[])?;
                let [url] = p.rest.as_slice() else {
                    return Err("open needs one URL".into());
                };
                let mut a = BTreeMap::new();
                a.insert("url".into(), serde_json::json!(url));
                (req("open-url", p.reason, a), p.json)
            }
            "notify" => {
                let p = parse(rest, &[])?;
                let mut a = BTreeMap::new();
                match p.rest.as_slice() {
                    [t] => {
                        a.insert("title".into(), serde_json::json!(t));
                    }
                    [t, b] => {
                        a.insert("title".into(), serde_json::json!(t));
                        a.insert("body".into(), serde_json::json!(b));
                    }
                    _ => return Err("notify needs a title and an optional body".into()),
                }
                (req("notify", p.reason, a), p.json)
            }
            "spawn" => {
                // A child always gets a worktree; the flag only says so.
                let from_head = rest.iter().any(|a| a == "--from-head");
                let args: Vec<String> = rest
                    .iter()
                    .filter(|a| *a != "--worktree" && *a != "--from-head")
                    .cloned()
                    .collect();
                let p = parse(&args, &["prompt-file", "role", "timeout", "mem", "cpus"])?;
                let [harness] = p.rest.as_slice() else {
                    return Err("spawn needs one harness name, for example: spawn codex --prompt-file task.md".into());
                };
                let file = p
                    .opts
                    .get("prompt-file")
                    .ok_or("spawn needs --prompt-file FILE with the task")?;
                let mut a = BTreeMap::new();
                a.insert("action".into(), serde_json::json!("start"));
                a.insert("harness".into(), serde_json::json!(harness));
                a.insert("prompt_file".into(), serde_json::json!(file));
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
                (req("spawn", p.reason, a), p.json)
            }
            "status" | "wait" | "kill" => {
                let p = parse(rest, if cmd == "wait" { &["timeout"] } else { &[] })?;
                let mut a = BTreeMap::new();
                match (cmd, p.rest.as_slice()) {
                    ("status", []) => {
                        a.insert("action".into(), serde_json::json!("list"));
                    }
                    (_, [id]) => {
                        a.insert("action".into(), serde_json::json!(cmd));
                        a.insert("id".into(), serde_json::json!(id));
                    }
                    _ => return Err(format!("{cmd} needs the id that spawn printed")),
                }
                if let Some(t) = p.opts.get("timeout") {
                    a.insert(
                        "timeout".into(),
                        serde_json::json!(t
                            .parse::<u64>()
                            .map_err(|_| "--timeout needs a number")?),
                    );
                }
                (req("spawn", p.reason, a), p.json)
            }
            other => return Err(format!("unknown command '{other}'")),
        })
    })();
    let (request, json) = match built {
        Ok(x) => x,
        Err(e) => return usage(&e),
    };
    let resp = match send(&request) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("paddock-host: {e}\nThis command works only inside a paddock sandbox.");
            return EXIT_NO_HOST;
        }
    };
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&resp).unwrap_or_default()
        );
    } else {
        print_human(&request, &resp);
    }
    if resp.status == Status::Done && (request.cap == "exec" || request.cap == "tool") {
        return resp.result["exit_code"].as_i64().unwrap_or(0) as i32;
    }
    if resp.status == Status::Done && cmd == "wait" {
        return wait_exit_code(&resp.result);
    }
    resp.status.exit_code()
}

/// 0 when the child ended with exit code 0, else 1.
pub fn wait_exit_code(status: &serde_json::Value) -> i32 {
    let ok = status["state"] == "exited" && status["exit_code"] == 0;
    if ok {
        0
    } else {
        1
    }
}

fn print_child(c: &serde_json::Value) {
    let s = |k: &str| c[k].as_str().unwrap_or("").to_string();
    let mut line = format!("{}  {:<10} {:<9}", s("id"), s("harness"), s("state"));
    if let Some(code) = c["exit_code"].as_i64() {
        line.push_str(&format!(" exit {code}"));
    }
    line.push_str(&format!("  {}", s("branch")));
    if let Some(i) = c["inbox"].as_str() {
        line.push_str(&format!("  results in {i}"));
    }
    if let Some(m) = c["message"].as_str() {
        line.push_str(&format!("  ({m})"));
    }
    println!("{line}");
}

fn print_human(req: &Request, resp: &Response) {
    if req.cap == "spawn" && resp.status == Status::Waiting && resp.result.is_object() {
        eprintln!("paddock-host: {}", resp.message);
        print_child(&resp.result);
        return;
    }
    if resp.status != Status::Done {
        let what = match resp.status {
            Status::Denied => "denied by the user",
            Status::Disabled => "disabled in the user's configuration",
            Status::Timeout => "not answered in time",
            Status::Waiting => "still waiting",
            Status::Failed => "failed on the host",
            Status::Invalid => "rejected as invalid",
            Status::Done => unreachable!(),
        };
        eprintln!("paddock-host: {} {what}: {}", req.cap, resp.message);
        return;
    }
    let r = &resp.result;
    match req.cap.as_str() {
        "capabilities" if req.args.contains_key("list_tools") => {
            let tools = r["tools"].as_array().cloned().unwrap_or_default();
            if tools.is_empty() {
                println!("No tools. The user defines them in [host.tools] of the configuration.");
            }
            for t in &tools {
                let shapes: Vec<String> = t["args"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|s| s.as_str().map(|s| format!("\"{s}\"")))
                    .collect();
                println!(
                    "{:<20} {:<5}  {}",
                    t["name"].as_str().unwrap_or(""),
                    t["mode"].as_str().unwrap_or(""),
                    if shapes.is_empty() {
                        "(no arguments)".to_string()
                    } else {
                        shapes.join(", ")
                    }
                );
            }
        }
        "boundary" => {
            let groups: Vec<crate::boundary::Group> =
                serde_json::from_value(r["groups"].clone()).unwrap_or_default();
            print!("{}", crate::boundary::render(&groups));
        }
        "capabilities" => {
            println!("wait for an answer: {} seconds", r["wait_seconds"]);
            for c in r["capabilities"].as_array().into_iter().flatten() {
                println!(
                    "{:<16} {:<6}{} {}",
                    c["name"].as_str().unwrap_or(""),
                    c["mode"].as_str().unwrap_or(""),
                    if c["session_granted"] == true {
                        " (allowed this session)"
                    } else {
                        ""
                    },
                    c["description"].as_str().unwrap_or("")
                );
            }
        }
        "exec" | "forge" | "tool" => {
            print!("{}", r["stdout"].as_str().unwrap_or(""));
            eprint!("{}", r["stderr"].as_str().unwrap_or(""));
        }
        "clipboard-read" => print!("{}", r["text"].as_str().unwrap_or("")),
        "return" => println!(
            "returned {} to {}: {} added, {} changed, {} deleted",
            r["name"].as_str().unwrap_or(""),
            r["original"].as_str().unwrap_or(""),
            r["added"],
            r["changed"],
            r["deleted"]
        ),
        "spawn" => match req.args.get("action").and_then(|a| a.as_str()) {
            Some("start") => {
                // Only the id on stdout, so `id=$(paddock-host spawn ...)` works.
                println!("{}", r["id"].as_str().unwrap_or(""));
                eprintln!(
                    "paddock-host: started {} on branch {}. Wait for it: paddock-host wait {}",
                    r["harness"].as_str().unwrap_or(""),
                    r["branch"].as_str().unwrap_or(""),
                    r["id"].as_str().unwrap_or("")
                );
            }
            Some("list") => {
                let list = r["children"].as_array().cloned().unwrap_or_default();
                if list.is_empty() {
                    println!("No children.");
                }
                for c in &list {
                    print_child(c);
                }
            }
            _ => print_child(r),
        },
        _ => {
            if let Some(p) = r["path"].as_str() {
                println!("{p}");
            }
        }
    }
}
