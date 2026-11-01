//! `paddock-host device`: one request to a device in `/dev/paddock`, with
//! the errno of a failed read turned into the exit codes of paddock-host.
//! A plain `exec 3<>/dev/paddock/NAME` works the same; this command only
//! saves the shell lines and names the errors.

use crate::waiting;
use std::io::{IsTerminal, Read, Write};

pub const HELP: &str = r#"paddock-host device: send one request to a device in /dev/paddock.

USAGE
  paddock-host device list
  paddock-host device wait TIME [--timeout SECONDS]
  paddock-host device ask QUESTION [--choice TEXT]... [--timeout SECONDS]
  paddock-host device NAME [REQUEST | -]

Devices are off until the user turns them on in the user configuration on
the host. `device list` prints the catalog: each device, its request format
and whether it is on.

WAIT AND ASK
  wait TIME returns at TIME: an RFC 3339 time such as 2026-10-30T15:00:00Z,
  or a duration such as 90, 15m or 2h (at most 7 days). A duration becomes a
  time at once, and the message of a still-waiting exit names it: use that
  time for the next read, so that every read waits for the same moment.
  ask QUESTION shows the question to the user on the host and prints the
  answer. --choice adds a short answer as a button (up to 8). The user can
  always type another answer.

  One read waits at most --timeout seconds (default 100, at most 600). When
  the time passes before the event, the command exits with 75 (still
  waiting) and prints nothing on stdout. Run the same command again. Choose
  a timeout below the time limit of your shell tool. Never use sleep to wait.

EXIT CODES
  0 done   2 wrong usage   3 the user dismissed the question
  4 the device is off or does not exist   5 the device did not answer in its
  time limit   6 no /dev/paddock in this sandbox   7 the device failed
  8 the device rejected the request   10 the request or response is too large
  75 still waiting: the timeout passed before the event

EXAMPLES
  paddock-host device wait 2026-10-30T15:00:00Z --timeout 100
  paddock-host device ask "Deploy to staging now?" --choice yes --choice no
"#;

/// Where the devices are: `$PADDOCK_DEVICES` (srt) or `/dev/paddock`.
pub fn root() -> std::path::PathBuf {
    std::env::var("PADDOCK_DEVICES")
        .unwrap_or_else(|_| crate::devices::MOUNT_IN_SANDBOX.to_string())
        .into()
}

/// The exit code for the errno of a failed device read or write.
pub fn exit_for_errno(errno: i32) -> i32 {
    match errno {
        e if e == waiting::ERRNO_STILL_WAITING => waiting::EXIT_STILL_WAITING,
        libc::EACCES | libc::EPERM => 3,
        libc::ENOENT => 4,
        libc::ETIMEDOUT => 5,
        libc::EINVAL => 8,
        libc::EFBIG | libc::EMSGSIZE => 10,
        libc::EDQUOT => 11,
        _ => 7,
    }
}

pub fn describe(errno: i32) -> &'static str {
    match errno {
        e if e == waiting::ERRNO_STILL_WAITING => "still waiting",
        libc::EACCES | libc::EPERM => "the user declined",
        libc::ENOENT => "no such device; it is off or unknown (see paddock-host device list)",
        libc::ETIMEDOUT => "the device did not answer within its time limit",
        libc::EINVAL => "the device rejected the request",
        libc::EFBIG => "the request is too large",
        libc::EMSGSIZE => "the response is too large",
        libc::EBUSY => "the device is busy; try again",
        libc::EDQUOT => "budget used: the user raises it on the host with paddock devices budget",
        _ => "the device failed",
    }
}

/// Send `request` to the device `name` and read the whole response.
pub fn call(name: &str, request: &[u8]) -> Result<Vec<u8>, i32> {
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(root().join(name))
        .map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))?;
    f.write_all(request)
        .map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))?;
    let mut out = Vec::new();
    f.read_to_end(&mut out)
        .map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))?;
    Ok(out)
}

fn usage(msg: &str) -> i32 {
    eprintln!(
        "paddock-host device: {msg}\nRun `paddock-host device --help` for the full reference."
    );
    super::EXIT_USAGE
}

pub fn main(args: &[String]) -> i32 {
    let Some(name) = args.first().map(String::as_str) else {
        print!("{HELP}");
        return super::EXIT_USAGE;
    };
    if matches!(name, "-h" | "--help" | "help") || args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{HELP}");
        return 0;
    }
    if name == "list" {
        return match std::fs::read(root().join(crate::devices::CATALOG)) {
            Ok(c) => {
                let _ = std::io::stdout().write_all(&c);
                0
            }
            Err(e) => {
                eprintln!(
                    "paddock-host device: cannot read {}: {e}. This sandbox has no devices.",
                    root().join(crate::devices::CATALOG).display()
                );
                super::EXIT_NO_HOST
            }
        };
    }
    let mut timeout: Option<u64> = None;
    let mut choices: Vec<String> = Vec::new();
    let mut pos: Vec<String> = Vec::new();
    let mut i = 1;
    while i < args.len() {
        let a = &args[i];
        match a.as_str() {
            "--timeout" | "--choice" => {
                let Some(v) = args.get(i + 1) else {
                    return usage(&format!("{a} needs a value"));
                };
                if a == "--timeout" {
                    match v
                        .parse::<u64>()
                        .ok()
                        .filter(|t| (1..=waiting::MAX_READ_TIMEOUT).contains(t))
                    {
                        Some(t) => timeout = Some(t),
                        None => {
                            return usage(&format!(
                                "--timeout must be 1 to {} seconds",
                                waiting::MAX_READ_TIMEOUT
                            ))
                        }
                    }
                } else {
                    choices.push(v.clone());
                }
                i += 1;
            }
            "--" => {
                pos.extend(args[i + 1..].iter().cloned());
                break;
            }
            s if s.starts_with("--") => return usage(&format!("unknown option {a}")),
            _ => pos.push(a.clone()),
        }
        i += 1;
    }
    if !choices.is_empty() && name != "ask" {
        return usage("--choice applies only to ask");
    }
    if timeout.is_some() && !matches!(name, "ask" | "wait") {
        return usage("--timeout applies only to wait and ask");
    }
    // A duration becomes a time once, here, so that every read of the loop
    // waits for the same moment.
    let mut until = None;
    let request: Vec<u8> = match name {
        "wait" => {
            let [when] = pos.as_slice() else {
                return usage("wait needs one TIME or duration");
            };
            let at = match waiting::parse_when(when, crate::state::now()) {
                Ok(t) => t,
                Err(e) => return usage(&format!("{e:#}")),
            };
            let t = waiting::format_rfc3339(at);
            until = Some(t.clone());
            let mut v = serde_json::json!({ "until": t });
            if let Some(s) = timeout {
                v["timeout"] = s.into();
            }
            v.to_string().into_bytes()
        }
        "ask" => {
            let q = pos.join(" ");
            if q.trim().is_empty() {
                return usage("ask needs a QUESTION");
            }
            let mut v = serde_json::json!({ "question": q });
            if !choices.is_empty() {
                v["choices"] = serde_json::json!(choices);
            }
            if let Some(s) = timeout {
                v["timeout"] = s.into();
            }
            v.to_string().into_bytes()
        }
        _ => match pos.as_slice() {
            [] if !std::io::stdin().is_terminal() => read_stdin(),
            [d] if d == "-" => read_stdin(),
            [] => Vec::new(),
            [r] => r.clone().into_bytes(),
            _ => return usage("give the request as one argument, or - for standard input"),
        },
    };
    match call(name, &request) {
        Ok(out) => {
            let _ = std::io::stdout().write_all(&out);
            let _ = std::io::stdout().flush();
            0
        }
        Err(errno) => {
            let code = exit_for_errno(errno);
            if code == waiting::EXIT_STILL_WAITING {
                match &until {
                    Some(t) => eprintln!(
                        "paddock-host device: wait: still waiting until {t}. Run: paddock-host device wait {t}{}",
                        timeout.map(|s| format!(" --timeout {s}")).unwrap_or_default()
                    ),
                    None => eprintln!(
                        "paddock-host device: {name}: still waiting. Run the same command again."
                    ),
                }
            } else if errno == libc::ENOENT && !root().join(crate::devices::CATALOG).exists() {
                eprintln!(
                    "paddock-host device: this sandbox has no {}.",
                    root().display()
                );
                return super::EXIT_NO_HOST;
            } else {
                eprintln!(
                    "paddock-host device: {name}: {} ({})",
                    describe(errno),
                    std::io::Error::from_raw_os_error(errno)
                );
            }
            code
        }
    }
}

fn read_stdin() -> Vec<u8> {
    let mut v = Vec::new();
    let _ = std::io::stdin().read_to_end(&mut v);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errnos_map_to_the_exit_codes_of_paddock_host() {
        assert_eq!(exit_for_errno(libc::ETIME), 75);
        assert_eq!(exit_for_errno(libc::EACCES), 3);
        assert_eq!(exit_for_errno(libc::ENOENT), 4);
        assert_eq!(exit_for_errno(libc::ETIMEDOUT), 5);
        assert_eq!(exit_for_errno(libc::EINVAL), 8);
        assert_eq!(exit_for_errno(libc::EMSGSIZE), 10);
        assert_eq!(exit_for_errno(libc::EDQUOT), 11);
        assert_eq!(exit_for_errno(libc::EIO), 7);
        // The same code as a kv watch or a spawn wait that timed out.
        assert_eq!(
            exit_for_errno(waiting::ERRNO_STILL_WAITING),
            crate::kv::Status::Waiting.exit_code()
        );
        assert_eq!(
            crate::broker::Status::Waiting.exit_code(),
            waiting::EXIT_STILL_WAITING
        );
    }
}
