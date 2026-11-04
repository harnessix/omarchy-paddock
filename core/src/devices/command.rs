//! `command` user devices: the request is the standard input of a fixed
//! command on the host.
//!
//! The command never gets arguments from the request: its argument list is
//! the `command` list of the user configuration. It runs through the same
//! isolation as an approved `exec` (execbox.rs): a clean environment, the
//! `[exec]` view of the system, an empty working folder, and no network
//! unless the device says `network = true`. `timeout` stops it.

use super::user::{Gate, UserDev, UserKind};
use super::{Call, DevError, Device, DeviceCtx, Info};
use crate::execbox::{ExecCfg, Isolation};
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Lines of stderr in the response, and the length of each.
const STDERR_LINES: usize = 5;
const STDERR_LINE: usize = 200;
/// How much stderr paddock reads at all.
const STDERR_READ: u64 = 64 * 1024;

pub fn info(def: &UserDev) -> Info {
    let (command, network) = match &def.kind {
        UserKind::Command { command, network } => (command.join(" "), *network),
        UserKind::Http { .. } => (String::new(), false),
    };
    Info {
        request: format!(
            "bytes for the standard input of the command `{command}` on the host (at most {} bytes). The request never adds arguments. The command runs isolated, {}, for at most {} seconds.",
            def.max_body,
            if network { "with network" } else { "without network" },
            def.timeout
        ),
        response: format!(
            "header lines, an empty line, then the standard output (at most {} bytes). Header lines: status: exit CODE, signal NUMBER, timeout, error or disabled; stderr: LINE (the first {STDERR_LINES} lines of the standard error); message: TEXT. EACCES: the user declined the device. EDQUOT: the budget of a paid device is used.",
            def.max_body
        ),
        limits: def.limits(),
    }
}

/// `/dev/paddock/NAME` for a `command` device.
pub struct Command {
    def: UserDev,
    ctx: DeviceCtx,
    gate: Gate,
    /// The isolation, found at the first request.
    how: OnceLock<Result<Isolation, String>>,
    runs: AtomicU64,
}

impl Command {
    /// `how`: a fixed isolation, for tests; None finds it on the host.
    pub fn new(def: UserDev, ctx: DeviceCtx, how: Option<Isolation>) -> Self {
        let gate = Gate::new(ctx.clone(), &def);
        let lock = OnceLock::new();
        if let Some(h) = how {
            let _ = lock.set(Ok(h));
        }
        Self {
            def,
            ctx,
            gate,
            how: lock,
            runs: AtomicU64::new(0),
        }
    }

    fn how(&self) -> Result<Isolation, String> {
        self.how
            .get_or_init(|| crate::execbox::detect(&self.ctx.env).map_err(|e| format!("{e:#}")))
            .clone()
    }

    /// Run the command with `input` on its standard input.
    fn run(&self, call: &Call, input: &[u8]) -> Result<Outcome, DevError> {
        let UserKind::Command { command, network } = &self.def.kind else {
            return Err(DevError::Failed);
        };
        let how = match self.how() {
            Ok(h) => h,
            Err(e) => return Ok(Outcome::error(e)),
        };
        let Some(program) = self.ctx.env.which(&command[0]) else {
            return Ok(Outcome::error(format!(
                "{} is not on the PATH of the host",
                command[0]
            )));
        };
        let mut argv = command.clone();
        argv[0] = program.display().to_string();
        let mut cfg = match ExecCfg::load(&self.ctx.env) {
            Ok(c) => c,
            Err(e) => return Ok(Outcome::error(format!("{e:#}"))),
        };
        cfg.network = *network;
        let n = self.runs.fetch_add(1, Ordering::SeqCst);
        let safe = |s: &str| -> String {
            s.chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                .collect()
        };
        let unit = format!(
            "paddock-device-{}-{}-{n}",
            safe(&self.ctx.sandbox),
            safe(&self.def.name)
        );
        // An empty working folder: the command sees no project.
        let work = self.ctx.env.paddock_run().join("device-work").join(&unit);
        if let Err(e) = crate::privfs::dir(&work) {
            return Ok(Outcome::error(format!(
                "cannot create a working folder: {e}"
            )));
        }
        let vars = cfg.pass_env(&self.ctx.env.vars);
        let mut cmd = crate::execbox::command(
            how,
            &cfg,
            &self.ctx.env.home,
            &work,
            &argv,
            &vars,
            self.def.timeout,
            &unit,
        );
        let r = self.capture(call, &mut cmd, how, &unit, input);
        let _ = std::fs::remove_dir_all(&work);
        r
    }

    fn capture(
        &self,
        call: &Call,
        cmd: &mut std::process::Command,
        how: Isolation,
        unit: &str,
        input: &[u8],
    ) -> Result<Outcome, DevError> {
        // Its own process group, so a stop reaches every process it started.
        cmd.process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return Ok(Outcome::error(format!("cannot start the command: {e}"))),
        };
        if let Some(mut stdin) = child.stdin.take() {
            let data = input.to_vec();
            // A thread, so a command that does not read cannot block paddock.
            std::thread::spawn(move || {
                let _ = stdin.write_all(&data);
            });
        }
        let max = self.def.max_body;
        let (tx, rx) = mpsc::channel();
        let mut so = child.stdout.take().expect("piped");
        std::thread::spawn(move || {
            let mut b = Vec::new();
            let _ = so.by_ref().take(max as u64 + 1).read_to_end(&mut b);
            let _ = tx.send(b);
        });
        let mut se = child.stderr.take().expect("piped");
        let errs = std::thread::spawn(move || {
            let mut b = Vec::new();
            let _ = se.by_ref().take(STDERR_READ).read_to_end(&mut b);
            b
        });
        let end = Instant::now() + Duration::from_secs(self.def.timeout);
        let stop = |child: &mut std::process::Child| {
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let _ = child.kill();
            if how == Isolation::Systemd {
                let _ = std::process::Command::new("systemctl")
                    .args(["--user", "kill", "--signal=SIGKILL", unit])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
            let _ = child.wait();
        };
        let mut stdout = None;
        let status = loop {
            if stdout.is_none() {
                if let Ok(b) = rx.try_recv() {
                    if b.len() > max {
                        stop(&mut child);
                        return Err(DevError::TooLarge);
                    }
                    stdout = Some(b);
                }
            }
            match child.try_wait() {
                Ok(Some(s)) => break Some(s),
                Ok(None) => {}
                Err(_) => break None,
            }
            if call.cancelled() {
                stop(&mut child);
                return Err(DevError::Cancelled);
            }
            if Instant::now() >= end {
                stop(&mut child);
                break None;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let timed_out = status.is_none();
        let stdout = match stdout {
            Some(b) => b,
            // The output ends when the last process that holds it ends.
            None => rx.recv_timeout(Duration::from_secs(2)).unwrap_or_default(),
        };
        if stdout.len() > max {
            return Err(DevError::TooLarge);
        }
        let stderr = errs.join().unwrap_or_default();
        let (code, status) = match status {
            None if timed_out => (None, "timeout".to_string()),
            None => (None, "error".to_string()),
            Some(s) => match (s.code(), std::os::unix::process::ExitStatusExt::signal(&s)) {
                (Some(c), _) => (Some(c), format!("exit {c}")),
                (None, Some(sig)) => (None, format!("signal {sig}")),
                _ => (None, "error".to_string()),
            },
        };
        let stderr_text = String::from_utf8_lossy(&stderr);
        let message = code
            .and_then(|c| crate::execbox::setup_failure(how, c, &stderr_text))
            .or_else(|| {
                timed_out.then(|| {
                    format!(
                        "the command did not finish in {} seconds and was stopped",
                        self.def.timeout
                    )
                })
            });
        Ok(Outcome {
            status,
            message,
            stderr: stderr_text.into_owned(),
            stdout,
        })
    }
}

/// What one run gave.
struct Outcome {
    status: String,
    message: Option<String>,
    stderr: String,
    stdout: Vec<u8>,
}

impl Outcome {
    fn error(message: String) -> Self {
        Self {
            status: "error".into(),
            message: Some(message),
            stderr: String::new(),
            stdout: Vec::new(),
        }
    }

    /// The response: header lines, an empty line, the standard output.
    fn response(&self) -> Vec<u8> {
        let mut head = format!("status: {}\n", self.status);
        if let Some(m) = &self.message {
            head.push_str(&format!("message: {}\n", one_line(m, 500)));
        }
        let lines: Vec<&str> = self
            .stderr
            .lines()
            .filter(|l| !l.trim().is_empty())
            .collect();
        for l in lines.iter().take(STDERR_LINES) {
            head.push_str(&format!("stderr: {}\n", one_line(l, STDERR_LINE)));
        }
        if lines.len() > STDERR_LINES {
            head.push_str(&format!(
                "stderr: ({} more lines)\n",
                lines.len() - STDERR_LINES
            ));
        }
        head.push('\n');
        let mut out = head.into_bytes();
        out.extend_from_slice(&self.stdout);
        out
    }
}

/// One line without control characters, at most `max` characters.
fn one_line(s: &str, max: usize) -> String {
    let clean: String = crate::broker::clean_text(s)
        .chars()
        .map(|c| if c == '\n' || c == '\t' { ' ' } else { c })
        .collect();
    if clean.chars().count() > max {
        let mut t: String = clean.chars().take(max).collect();
        t.push_str("...");
        t
    } else {
        clean
    }
}

impl Device for Command {
    fn name(&self) -> &str {
        &self.def.name
    }
    fn info(&self) -> Info {
        info(&self.def)
    }
    fn details(&self) -> Option<serde_json::Value> {
        let mut v = self.def.details();
        self.gate.budget_details(&mut v);
        Some(v)
    }
    fn call(&self, call: &Call, request: &[u8]) -> Result<Vec<u8>, DevError> {
        let start = Instant::now();
        if request.len() > self.def.max_body {
            return Err(DevError::TooLarge);
        }
        let decision = match self.gate.check(call) {
            Ok(d) => d,
            Err(e) => {
                self.gate.audit(
                    &format!("{} bytes in", request.len()),
                    "denied",
                    "denied",
                    start,
                );
                return Err(e);
            }
        };
        let charge = match self.gate.reserve() {
            Ok(c) => c,
            Err(e) => {
                let status = match e {
                    DevError::BudgetUsed => super::ledger::BUDGET_USED,
                    DevError::Denied => "no budget",
                    _ => "failed",
                };
                self.gate.audit(
                    &format!("{} bytes in", request.len()),
                    decision,
                    status,
                    start,
                );
                return Err(e);
            }
        };
        let r = self.run(call, request);
        let (detail, status) = match &r {
            Ok(o) => (
                format!(
                    "{} bytes in, {} bytes out, {} bytes of stderr",
                    request.len(),
                    o.stdout.len(),
                    o.stderr.len()
                ),
                o.status.clone(),
            ),
            Err(DevError::TooLarge) => (
                format!("{} bytes in, too much output", request.len()),
                "too large".into(),
            ),
            Err(_) => (format!("{} bytes in", request.len()), "cancelled".into()),
        };
        // A command that could not start reached no service.
        self.gate.settle(charge, None, status != "error", &status);
        self.gate.audit(&detail, decision, &status, start);
        r.map(|o| o.response())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::broker::Answer;
    use crate::devices::user::tests::ctx;
    use crate::devices::DevicesCfg;
    use std::os::unix::fs::PermissionsExt;

    fn script(dir: &std::path::Path, name: &str, body: &str) {
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let f = bin.join(name);
        std::fs::write(&f, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn dev(dir: &std::path::Path, cmd: &str, extra: &str, answer: Answer) -> Command {
        let cfg = DevicesCfg::parse(&format!(
            "[devices.tool]\nkind = \"command\"\nenabled = true\ncommand = {cmd}\n{extra}"
        ))
        .unwrap();
        let (ctx, _) = ctx(dir, answer);
        Command::new(cfg.user[0].clone(), ctx, Some(Isolation::None))
    }

    fn call() -> Call {
        Call::for_test("box1", Duration::from_secs(60))
    }

    #[test]
    fn the_request_is_stdin_and_never_an_argument() {
        let dir = tempfile::tempdir().unwrap();
        script(
            dir.path(),
            "fake-tool",
            "echo \"argc=$# args=$*\"\ncat\necho oops >&2",
        );
        let d = dev(
            dir.path(),
            "[\"fake-tool\", \"-c\", \".\"]",
            "max_body = \"1K\"",
            Answer::Session,
        );
        let out = d.call(&call(), b"--evil; rm -rf /\n$(id)").unwrap();
        let out = String::from_utf8(out).unwrap();
        let (head, body) = out.split_once("\n\n").unwrap();
        assert_eq!(head, "status: exit 0\nstderr: oops");
        let mut lines = body.lines();
        assert_eq!(lines.next(), Some("argc=2 args=-c ."));
        assert_eq!(lines.next(), Some("--evil; rm -rf /"));
        assert_eq!(lines.next(), Some("$(id)"));
        // The audit has byte counts, never the input.
        let audit = std::fs::read_to_string(crate::broker::audit_file(&d.ctx.env)).unwrap();
        assert!(
            audit.contains("bytes in") && audit.contains("\"status\":\"exit 0\""),
            "{audit}"
        );
        assert!(!audit.contains("evil"), "{audit}");
    }

    #[test]
    fn exit_codes_stderr_and_limits_come_back() {
        let dir = tempfile::tempdir().unwrap();
        script(
            dir.path(),
            "fail",
            "for i in 1 2 3 4 5 6 7; do echo \"line $i\" >&2; done\nexit 3",
        );
        script(dir.path(), "loud", "head -c 5000 /dev/zero");
        let d = dev(dir.path(), "[\"fail\"]", "", Answer::Session);
        let out = String::from_utf8(d.call(&call(), b"").unwrap()).unwrap();
        assert!(out.starts_with("status: exit 3\nstderr: line 1\n"), "{out}");
        assert!(
            out.contains("stderr: line 5\nstderr: (2 more lines)\n\n"),
            "{out}"
        );
        assert!(!out.contains("line 6"), "{out}");
        let d = dev(
            dir.path(),
            "[\"loud\"]",
            "max_body = \"1K\"",
            Answer::Session,
        );
        assert_eq!(d.call(&call(), b"").unwrap_err(), DevError::TooLarge);
        let mut big = vec![b'x'; 1025];
        big.push(b'\n');
        assert_eq!(d.call(&call(), &big).unwrap_err(), DevError::TooLarge);
        let d = dev(dir.path(), "[\"no-such-tool\"]", "", Answer::Session);
        let out = String::from_utf8(d.call(&call(), b"").unwrap()).unwrap();
        assert!(
            out.starts_with("status: error\nmessage: no-such-tool is not on the PATH"),
            "{out}"
        );
    }

    #[test]
    fn the_timeout_stops_a_sleeping_command() {
        let dir = tempfile::tempdir().unwrap();
        script(dir.path(), "sleeper", "echo started\nsleep 30\necho never");
        let d = dev(dir.path(), "[\"sleeper\"]", "timeout = 1", Answer::Session);
        let t = Instant::now();
        let out = String::from_utf8(d.call(&call(), b"").unwrap()).unwrap();
        assert!(t.elapsed() < Duration::from_secs(6), "{:?}", t.elapsed());
        assert!(
            out.starts_with("status: timeout\nmessage: the command did not finish in 1 seconds"),
            "{out}"
        );
        assert!(!out.contains("never"), "{out}");
    }

    #[test]
    fn a_declined_device_runs_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mark = dir.path().join("ran");
        script(dir.path(), "marker", &format!("touch {}", mark.display()));
        let d = dev(dir.path(), "[\"marker\"]", "", Answer::Deny);
        assert_eq!(d.call(&call(), b"x").unwrap_err(), DevError::Denied);
        assert!(!mark.exists());
    }
}
