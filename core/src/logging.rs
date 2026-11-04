//! One logging facility for paddock, on top of the `log` crate macros.
//!
//! - stderr: warnings and errors by default, in the old `paddock: warning: ...`
//!   form. `-v` adds info, `-vv` debug, `-vvv` trace; `-q` keeps only errors.
//! - `PADDOCK_LOG` overrides the level, for everything or per module:
//!   `PADDOCK_LOG=debug`, `PADDOCK_LOG=broker=debug,proxy=trace`.
//! - the systemd journal, identifier `paddock`, when a journal socket exists:
//!   records at info and worse (or the `PADDOCK_LOG` level if it is more
//!   verbose), with the matching journal priority. Without systemd nothing is
//!   sent and nothing fails.

use log::{Level, LevelFilter, Log, Metadata, Record};
use std::io::Write;
use std::os::unix::net::UnixDatagram;
use std::path::Path;

pub const IDENTIFIER: &str = "paddock";
const JOURNAL_SOCKET: &str = "/run/systemd/journal/socket";

/// Parsed level settings: a default and per-module overrides.
#[derive(Debug, Clone, PartialEq)]
pub struct Filter {
    pub default: LevelFilter,
    pub modules: Vec<(String, LevelFilter)>,
}

impl Filter {
    /// Build from the `-v`/`-q` count (positive = more verbose) and an
    /// optional `PADDOCK_LOG` value. Unknown parts of `PADDOCK_LOG` are ignored.
    pub fn new(verbosity: i32, spec: Option<&str>) -> Self {
        let default = match verbosity {
            i32::MIN..=-1 => LevelFilter::Error,
            0 => LevelFilter::Warn,
            1 => LevelFilter::Info,
            2 => LevelFilter::Debug,
            _ => LevelFilter::Trace,
        };
        let mut f = Filter {
            default,
            modules: Vec::new(),
        };
        for part in spec.unwrap_or("").split(',').map(str::trim) {
            match part.split_once('=') {
                Some((m, l)) => {
                    if let Ok(l) = l.trim().parse() {
                        let m = m.trim().trim_start_matches("paddock::").to_string();
                        f.modules.push((m, l));
                    }
                }
                None => {
                    if let Ok(l) = part.parse() {
                        f.default = l;
                    }
                }
            }
        }
        f
    }

    /// The level for a record target such as `paddock::broker` or
    /// `paddock::backend::docker`. The longest matching module wins.
    pub fn level(&self, target: &str) -> LevelFilter {
        let t = target.strip_prefix("paddock::").unwrap_or(target);
        self.modules
            .iter()
            .filter(|(m, _)| t == m || t.starts_with(&format!("{m}::")))
            .max_by_key(|(m, _)| m.len())
            .map(|(_, l)| *l)
            .unwrap_or(self.default)
    }

    fn max(&self) -> LevelFilter {
        self.modules
            .iter()
            .map(|(_, l)| *l)
            .fold(self.default, std::cmp::max)
    }
}

/// The stderr line for a record, as users saw it before this module existed.
pub fn stderr_line(level: Level, target: &str, msg: &str) -> String {
    let module = target.strip_prefix("paddock::").unwrap_or(target);
    match level {
        Level::Error => format!("paddock: error: {msg}"),
        Level::Warn => format!("paddock: warning: {msg}"),
        Level::Info => format!("paddock: {msg}"),
        Level::Debug => format!("paddock: debug: [{module}] {msg}"),
        Level::Trace => format!("paddock: trace: [{module}] {msg}"),
    }
}

/// syslog priority, as `journalctl -p` reads it.
pub fn priority(level: Level) -> u8 {
    match level {
        Level::Error => 3,
        Level::Warn => 4,
        Level::Info => 6,
        Level::Debug | Level::Trace => 7,
    }
}

/// One datagram in the journal native protocol. MESSAGE uses the binary
/// form, so it may contain newlines.
pub fn journal_datagram(level: Level, target: &str, msg: &str) -> Vec<u8> {
    let mut d = Vec::new();
    let _ = writeln!(d, "PRIORITY={}", priority(level));
    let _ = writeln!(d, "SYSLOG_IDENTIFIER={IDENTIFIER}");
    let _ = writeln!(d, "SYSLOG_PID={}", std::process::id());
    let _ = writeln!(d, "PADDOCK_MODULE={}", target.replace('\n', " "));
    d.extend_from_slice(b"MESSAGE\n");
    d.extend_from_slice(&(msg.len() as u64).to_le_bytes());
    d.extend_from_slice(msg.as_bytes());
    d.push(b'\n');
    d
}

struct Logger {
    filter: Filter,
    journal: Option<UnixDatagram>,
}

impl Logger {
    fn journal_level(&self, target: &str) -> LevelFilter {
        self.filter.level(target).max(LevelFilter::Info)
    }
}

impl Log for Logger {
    fn enabled(&self, m: &Metadata) -> bool {
        m.level() <= self.filter.level(m.target())
            || (self.journal.is_some() && m.level() <= self.journal_level(m.target()))
    }

    fn log(&self, r: &Record) {
        if !self.enabled(r.metadata()) {
            return;
        }
        let msg = r.args().to_string();
        if r.level() <= self.filter.level(r.target()) {
            eprintln!("{}", stderr_line(r.level(), r.target(), &msg));
        }
        if let Some(sock) = &self.journal {
            if r.level() <= self.journal_level(r.target()) {
                let _ = sock.send(&journal_datagram(r.level(), r.target(), &msg));
            }
        }
    }

    fn flush(&self) {}
}

fn connect_journal() -> Option<UnixDatagram> {
    if !Path::new(JOURNAL_SOCKET).exists() {
        return None;
    }
    let s = UnixDatagram::unbound().ok()?;
    s.connect(JOURNAL_SOCKET).ok()?;
    Some(s)
}

/// Install the logger. `verbosity` is the number of `-v` minus the number of
/// `-q`. Call once, early in `main`.
pub fn init(verbosity: i32) {
    let filter = Filter::new(verbosity, std::env::var("PADDOCK_LOG").ok().as_deref());
    let journal = connect_journal();
    let max = if journal.is_some() {
        filter.max().max(LevelFilter::Info)
    } else {
        filter.max()
    };
    if log::set_boxed_logger(Box::new(Logger { filter, journal })).is_ok() {
        log::set_max_level(max);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_warn_and_flags_shift_it() {
        assert_eq!(Filter::new(0, None).default, LevelFilter::Warn);
        assert_eq!(Filter::new(-1, None).default, LevelFilter::Error);
        assert_eq!(Filter::new(2, None).default, LevelFilter::Debug);
        assert_eq!(Filter::new(9, None).default, LevelFilter::Trace);
    }

    #[test]
    fn paddock_log_sets_levels_per_module() {
        let f = Filter::new(
            0,
            Some("info, broker=debug,paddock::backend=trace,bad=nope"),
        );
        assert_eq!(f.level("paddock::run"), LevelFilter::Info);
        assert_eq!(f.level("paddock::broker"), LevelFilter::Debug);
        assert_eq!(f.level("paddock::broker::tests"), LevelFilter::Debug);
        assert_eq!(f.level("paddock::brokerx"), LevelFilter::Info);
        assert_eq!(f.level("paddock::backend::docker"), LevelFilter::Trace);
        assert_eq!(f.max(), LevelFilter::Trace);
    }

    #[test]
    fn warning_line_keeps_the_old_form() {
        assert_eq!(
            stderr_line(Level::Warn, "paddock::run", "x"),
            "paddock: warning: x"
        );
        assert_eq!(
            stderr_line(Level::Debug, "paddock::proxy", "y"),
            "paddock: debug: [proxy] y"
        );
    }

    #[test]
    fn journal_datagram_has_identifier_priority_and_binary_message() {
        let d = journal_datagram(Level::Warn, "paddock::broker", "a\nb");
        let s = String::from_utf8_lossy(&d);
        assert!(s.contains("PRIORITY=4\n"));
        assert!(s.contains("SYSLOG_IDENTIFIER=paddock\n"));
        let at = d.windows(8).position(|w| w == b"MESSAGE\n").unwrap() + 8;
        assert_eq!(&d[at..at + 8], &3u64.to_le_bytes());
        assert_eq!(&d[at + 8..], b"a\nb\n");
    }
}
