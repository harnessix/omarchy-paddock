//! Home-grade monitoring of a sandbox: the paddock process of a sandbox
//! looks at the command lines of the processes in the sandbox's scope and
//! tells the user once when one of them opens a tunnel (`ssh -R/-L/-D`,
//! `ngrok`, `cloudflared` and the like).
//!
//! - It reads `/proc/PID/cmdline` only, never what goes through a tunnel,
//!   and contacts nothing.
//! - It needs the scope of the sandbox (a systemd user instance, bwrap or
//!   srt). Without one (docker, podman, no systemd) it does nothing.
//! - Each tool counts once per sandbox: the run folder keeps the tools that
//!   paddock reported (`monitor.json`), so a new watch does not repeat them.
//! - `[monitor] tunnels = false` in the user configuration turns it off,
//!   and `[monitor] notify = false` keeps the audit line but shows no
//!   notification.

use crate::env::HostEnv;
use crate::kv::Identity;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The capability name in the audit log.
pub const CAPABILITY: &str = "monitor";

/// How often the watch looks.
const INTERVAL: Duration = Duration::from_secs(10);

/// A tunnel in one command line: the tool and the flags that make it one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tunnel {
    pub tool: &'static str,
    pub flags: Vec<String>,
}

/// The user's switches, from `[monitor]` in the user configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorCfg {
    pub tunnels: bool,
    pub notify: bool,
}

impl MonitorCfg {
    pub fn load(env: &HostEnv) -> Self {
        let text =
            std::fs::read_to_string(env.paddock_config().join("config.toml")).unwrap_or_default();
        Self::parse(&text)
    }

    fn parse(text: &str) -> Self {
        let v = toml::from_str::<toml::Value>(text).ok();
        let get = |key: &str| {
            v.as_ref()
                .and_then(|v| v.get("monitor")?.get(key)?.as_bool())
                .unwrap_or(true)
        };
        MonitorCfg {
            tunnels: get("tunnels"),
            notify: get("notify"),
        }
    }
}

fn base(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

/// Interpreters that run a tool as a script (`node /usr/bin/lt`).
fn interpreter(name: &str) -> bool {
    matches!(name, "node" | "nodejs" | "bun" | "sh" | "bash" | "dash") || name.starts_with("python")
}

/// The tunnel that a command line opens, if any. A pure function of argv.
pub fn tunnel_in(argv: &[String]) -> Option<Tunnel> {
    let first = base(argv.first()?);
    if let Some(t) = tool(first, &argv[1..]) {
        return Some(t);
    }
    if interpreter(first) {
        let i = argv.iter().skip(1).position(|a| !a.starts_with('-'))? + 1;
        return tool(base(&argv[i]), &argv[i + 1..]);
    }
    None
}

fn tool(name: &str, args: &[String]) -> Option<Tunnel> {
    let simple = |tool| {
        Some(Tunnel {
            tool,
            flags: Vec::new(),
        })
    };
    match name {
        "ssh" => ssh(args),
        "ngrok" => simple("ngrok"),
        "cloudflared" => simple("cloudflared"),
        "frpc" => simple("frpc"),
        "frps" => simple("frps"),
        "bore" => simple("bore"),
        "chisel" => simple("chisel"),
        "lt" | "localtunnel" => simple("localtunnel"),
        "tailscale" if args.iter().any(|a| a == "funnel") => Some(Tunnel {
            tool: "tailscale funnel",
            flags: vec!["funnel".into()],
        }),
        _ => None,
    }
}

/// `ssh` with `-R`, `-L` or `-D`, alone or combined (`-NR`). The options
/// that take an argument end a combined word; ssh reads options before and
/// after the host, and the second plain word starts the remote command.
fn ssh(args: &[String]) -> Option<Tunnel> {
    const WITH_ARG: &str = "BbcDEeFIiJLlmOoPpQRSWw";
    let mut found: Vec<String> = Vec::new();
    let mut plain = 0;
    let mut skip = false;
    for a in args {
        if skip {
            skip = false;
            continue;
        }
        if a == "--" {
            break;
        }
        let Some(opts) = a.strip_prefix('-').filter(|o| !o.is_empty()) else {
            plain += 1;
            if plain > 1 {
                break;
            }
            continue;
        };
        for (i, c) in opts.char_indices() {
            if matches!(c, 'R' | 'L' | 'D') {
                let flag = format!("-{c}");
                if !found.contains(&flag) {
                    found.push(flag);
                }
            }
            if WITH_ARG.contains(c) {
                skip = i + c.len_utf8() == opts.len();
                break;
            }
        }
    }
    (!found.is_empty()).then_some(Tunnel {
        tool: "ssh",
        flags: found,
    })
}

/// The file with the tools that paddock reported for a sandbox.
pub fn seen_file(env: &HostEnv, sandbox: &str) -> PathBuf {
    env.paddock_run().join(sandbox).join("monitor.json")
}

/// The tunnel tools that paddock reported for a sandbox, for `paddock ps`.
pub fn read_seen(env: &HostEnv, sandbox: &str) -> Vec<String> {
    load_seen(&seen_file(env, sandbox)).into_iter().collect()
}

fn load_seen(file: &Path) -> BTreeSet<String> {
    std::fs::read_to_string(file)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| serde_json::from_value(v.get("tunnels")?.clone()).ok())
        .unwrap_or_default()
}

/// What the sandbox reported so far, kept in memory and in the run folder.
pub struct Seen {
    file: PathBuf,
    tools: BTreeSet<String>,
}

impl Seen {
    pub fn open(file: PathBuf) -> Self {
        let tools = load_seen(&file);
        Seen { file, tools }
    }

    /// The tunnels that are new for this sandbox. It records them.
    pub fn fresh(&mut self, found: Vec<Tunnel>) -> Vec<Tunnel> {
        let mut out = Vec::new();
        for t in found {
            if self.tools.insert(t.tool.to_string()) {
                out.push(t);
            }
        }
        if !out.is_empty() {
            let body = serde_json::json!({ "tunnels": self.tools });
            if let Err(e) = crate::privfs::write(&self.file, body.to_string()) {
                log::warn!("cannot write {}: {e}", self.file.display());
            }
        }
        out
    }
}

/// The cgroup folder of a sandbox's scope, from the cgroup of its first
/// process. None without a paddock scope.
fn scope_dir(pid: u32, unit: &str) -> Option<PathBuf> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    let path = text.lines().find_map(|l| l.strip_prefix("0::"))?;
    if base(path) != unit || !crate::freeze::ours(unit) || path.contains("..") {
        return None;
    }
    Some(PathBuf::from("/sys/fs/cgroup").join(path.trim_start_matches('/')))
}

/// The processes in a cgroup folder and in the folders below it.
fn procs(dir: &Path, depth: u32, out: &mut Vec<u32>) {
    if let Ok(t) = std::fs::read_to_string(dir.join("cgroup.procs")) {
        out.extend(t.lines().filter_map(|l| l.trim().parse::<u32>().ok()));
    }
    if depth == 0 {
        return;
    }
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                procs(&e.path(), depth - 1, out);
            }
        }
    }
}

fn cmdline(pid: u32) -> Option<Vec<String>> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let argv: Vec<String> = raw
        .split(|b| *b == 0)
        .filter(|w| !w.is_empty())
        .map(|w| String::from_utf8_lossy(w).into_owned())
        .collect();
    (!argv.is_empty()).then_some(argv)
}

/// The tunnels that run in a scope now.
fn scan(dir: &Path) -> Vec<Tunnel> {
    let mut pids = Vec::new();
    procs(dir, 4, &mut pids);
    pids.into_iter()
        .filter_map(|p| tunnel_in(&cmdline(p)?))
        .collect()
}

/// Start the watch for a sandbox, unless the user turned it off.
pub fn watch(env: HostEnv, id: Identity) {
    let cfg = MonitorCfg::load(&env);
    if !cfg.tunnels {
        return;
    }
    std::thread::spawn(move || {
        let mut seen = Seen::open(seen_file(&env, &id.sandbox));
        let mut dir: Option<PathBuf> = None;
        let mut tries = 0;
        loop {
            std::thread::sleep(if dir.is_none() {
                Duration::from_millis(500)
            } else {
                INTERVAL
            });
            if dir.is_none() {
                // The sandbox registers after it reached its scope.
                let r = crate::state::Running::list(&env)
                    .into_iter()
                    .find(|r| r.id == id.sandbox);
                dir = r.and_then(|r| scope_dir(r.pid, r.unit.as_deref()?));
                if dir.is_none() {
                    tries += 1;
                    if tries > 20 {
                        log::debug!("no scope for {}, so no tunnel watch", id.sandbox);
                        return;
                    }
                    continue;
                }
            }
            let Some(d) = dir.as_deref() else { continue };
            if !d.is_dir() {
                return;
            }
            for t in seen.fresh(scan(d)) {
                report(&env, &id, &t, cfg.notify);
            }
        }
    });
}

fn report(env: &HostEnv, id: &Identity, t: &Tunnel, notify: bool) {
    let flags = if t.flags.is_empty() {
        String::new()
    } else {
        format!(" {}", t.flags.join(" "))
    };
    let detail = format!("{} opened a tunnel: {}{flags}", id.sandbox, t.tool);
    crate::broker::audit_line(
        env,
        &serde_json::json!({
            "time": crate::state::now(),
            "sandbox": id.sandbox,
            "harness": id.harness,
            "project": id.project,
            "capability": CAPABILITY,
            "tool": t.tool,
            "flags": t.flags,
            "detail": detail,
            "decision": "reported",
        }),
    );
    if notify {
        let _ = crate::broker::notify::show(
            env,
            &crate::broker::notify::Note {
                summary: "a sandbox opened a tunnel".into(),
                body: format!(
                    "{detail}. It can reach or expose this machine from outside. paddock stop {} ends the sandbox.",
                    id.sandbox
                ),
                actions: Vec::new(),
                critical: false,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    fn tool_of(s: &str) -> Option<&'static str> {
        tunnel_in(&argv(s)).map(|t| t.tool)
    }

    #[test]
    fn each_tool_matches_by_name() {
        for (cmd, tool) in [
            ("/usr/local/bin/ngrok http 3000", "ngrok"),
            ("cloudflared tunnel run", "cloudflared"),
            ("frpc -c frpc.toml", "frpc"),
            ("frps", "frps"),
            ("bore local 8000 --to example.org", "bore"),
            ("chisel client example.org R:80", "chisel"),
            ("lt --port 3000", "localtunnel"),
            ("localtunnel --port 3000", "localtunnel"),
            (
                "node /usr/lib/node_modules/.bin/lt --port 3000",
                "localtunnel",
            ),
            ("tailscale funnel 443", "tailscale funnel"),
        ] {
            assert_eq!(tool_of(cmd), Some(tool), "{cmd}");
        }
    }

    #[test]
    fn ssh_matches_tunnel_flags_only() {
        let t = tunnel_in(&argv("ssh -R 8080:localhost:80 host")).unwrap();
        assert_eq!((t.tool, t.flags), ("ssh", vec!["-R".to_string()]));
        assert_eq!(
            tunnel_in(&argv("/usr/bin/ssh -L8080:a:80 host"))
                .unwrap()
                .flags,
            ["-L"]
        );
        assert_eq!(tunnel_in(&argv("ssh -D 1080 host")).unwrap().flags, ["-D"]);
        assert_eq!(
            tunnel_in(&argv("ssh -NR 9000:a:22 host")).unwrap().flags,
            ["-R"]
        );
        assert_eq!(
            tunnel_in(&argv("ssh -fNL 1:a:2 -D 3 host")).unwrap().flags,
            ["-L", "-D"]
        );
        // Options after the host count too.
        assert_eq!(tunnel_in(&argv("ssh host -R 1:a:2")).unwrap().flags, ["-R"]);
    }

    #[test]
    fn normal_commands_do_not_match() {
        for cmd in [
            "ssh host",
            "ssh -p 22 -i key host",
            // The argument of -l, and the remote command, are not flags.
            "ssh -l R host",
            "ssh -o ProxyJump=x host",
            "ssh host ls -R /",
            "ssh -- host -R",
            "git push",
            "cargo build -L",
            "tailscale status",
            "sleep 10",
            "bash -c ls",
            "",
        ] {
            assert_eq!(tool_of(cmd), None, "{cmd}");
        }
    }

    #[test]
    fn the_config_switches() {
        assert_eq!(
            MonitorCfg::parse(""),
            MonitorCfg {
                tunnels: true,
                notify: true
            }
        );
        let off = MonitorCfg::parse("[monitor]\ntunnels = false\nnotify = false\n");
        assert!(!off.tunnels && !off.notify);
    }

    #[test]
    fn a_tool_counts_once_per_sandbox() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("monitor.json");
        let ngrok = || tunnel_in(&argv("ngrok http 80")).unwrap();
        let ssh = || tunnel_in(&argv("ssh -R 1:a:2 h")).unwrap();
        let mut seen = Seen::open(file.clone());
        assert_eq!(seen.fresh(vec![ngrok(), ngrok()]).len(), 1);
        assert!(seen.fresh(vec![ngrok()]).is_empty());
        assert_eq!(seen.fresh(vec![ssh(), ngrok()]).len(), 1);
        // A new watch reads what the run folder holds.
        let mut again = Seen::open(file.clone());
        assert!(again.fresh(vec![ngrok(), ssh()]).is_empty());
        assert_eq!(
            load_seen(&file).into_iter().collect::<Vec<_>>(),
            ["ngrok", "ssh"]
        );
        // Another sandbox has a file of its own.
        let mut other = Seen::open(tmp.path().join("other.json"));
        assert_eq!(other.fresh(vec![ngrok()]).len(), 1);
    }
}
