//! The start screen watcher on a live terminal, next to the relay.
//!
//! The relay gives it a copy of the harness output, which it keeps in a
//! `vt100` parser. When the output has been quiet for a moment, the thread
//! looks for a rule's block. On a match it sends the rule's keys, and then
//! checks that the screen went away.

use super::{find, key_bytes, Found, Rule};
use crate::relay::{Log, Tap};
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Output quiet for this long counts as a settled screen.
pub const SETTLE: Duration = Duration::from_millis(300);
/// The screen must go within this time after the keys.
pub const STEP_TIMEOUT: Duration = Duration::from_secs(5);
/// The watcher reads the screen only in this first part of a run.
pub const WINDOW: Duration = Duration::from_secs(10 * 60);
/// Each rule answers at most this many times in a run.
pub const MAX_ANSWERS: u32 = 3;
const KEY_GAP: Duration = Duration::from_millis(120);
const TICK: Duration = Duration::from_millis(50);

/// One answer, for the notes and the audit line. `text` never holds a row
/// of the screen beyond the number for `{n}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub rule: String,
    pub keys: String,
    pub ok: bool,
    pub text: String,
}

type OnReport = Box<dyn Fn(&Report) + Send + Sync>;
type Watched = Box<dyn Fn() -> bool + Send + Sync>;

pub struct Watcher {
    inner: Arc<Inner>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

struct Inner {
    rules: Vec<Rule>,
    shared: Mutex<Shared>,
    active: AtomicBool,
    watched: Watched,
    on_report: OnReport,
}

struct Shared {
    parser: Option<vt100::Parser>,
    out_seq: u64,
    seen_seq: u64,
    last_out: Instant,
    caller: RawFd,
    master: RawFd,
    log: Option<Log>,
    /// The rule whose keys went out, the number on its screen, and when.
    pending: Option<(usize, Option<u64>, Instant)>,
    answers: Vec<u32>,
    reports: Vec<Report>,
}

impl Shared {
    fn rows(&self) -> Vec<String> {
        match &self.parser {
            Some(p) => {
                let cols = p.screen().size().1.get();
                p.screen().rows(0, cols).collect()
            }
            None => Vec::new(),
        }
    }
}

impl Watcher {
    /// A watcher for valid rules (`Rule::check`). `watched` tells whether
    /// a user watches the run now: then only `always` rules answer.
    pub fn new(
        rules: Vec<Rule>,
        watched: impl Fn() -> bool + Send + Sync + 'static,
        on_report: impl Fn(&Report) + Send + Sync + 'static,
    ) -> Arc<Watcher> {
        let n = rules.len();
        Arc::new(Watcher {
            inner: Arc::new(Inner {
                rules,
                shared: Mutex::new(Shared {
                    parser: None,
                    out_seq: 0,
                    seen_seq: 0,
                    last_out: Instant::now(),
                    caller: -1,
                    master: -1,
                    log: None,
                    pending: None,
                    answers: vec![0; n],
                    reports: Vec::new(),
                }),
                active: AtomicBool::new(true),
                watched: Box::new(watched),
                on_report: Box::new(on_report),
            }),
            thread: Mutex::new(None),
        })
    }

    pub fn active(&self) -> bool {
        self.inner.active.load(Ordering::SeqCst)
    }

    /// The answers so far.
    pub fn reports(&self) -> Vec<Report> {
        self.inner.shared.lock().unwrap().reports.clone()
    }

    /// Feed a screen without a terminal, for the tests: the bytes, then one
    /// decision as the thread makes it. Returns the keys it would send.
    #[cfg(test)]
    pub fn decide(&self, data: &[u8], now: Instant) -> Option<Vec<u8>> {
        let mut sh = self.inner.shared.lock().unwrap();
        if sh.parser.is_none() {
            sh.parser = Some(vt100::Parser::new(
                std::num::NonZeroU16::new(24).unwrap(),
                std::num::NonZeroU16::new(80).unwrap(),
                0,
            ));
        }
        sh.parser.as_mut().unwrap().process(data);
        self.inner.step(&mut sh, now)
    }
}

impl Tap for Watcher {
    fn attach(&self, caller: RawFd, master: RawFd, log: Log) {
        let (rows, cols) = winsize(caller).unwrap_or((24, 80));
        let nz = |v: u16, d: u16| std::num::NonZeroU16::new(v).or(std::num::NonZeroU16::new(d));
        let mut sh = self.inner.shared.lock().unwrap();
        sh.parser = Some(vt100::Parser::new(
            nz(rows, 24).unwrap_or(std::num::NonZeroU16::MIN),
            nz(cols, 80).unwrap_or(std::num::NonZeroU16::MIN),
            0,
        ));
        sh.caller = caller;
        sh.master = master;
        sh.log = Some(log);
        drop(sh);
        let inner = self.inner.clone();
        let t = std::thread::Builder::new()
            .name("paddock-screens".into())
            .spawn(move || run(inner));
        match t {
            Ok(t) => *self.thread.lock().unwrap() = Some(t),
            Err(e) => {
                log::debug!("screens: cannot start: {e}");
                self.inner.active.store(false, Ordering::SeqCst);
            }
        }
    }

    fn output(&self, data: &[u8]) {
        if !self.active() {
            return;
        }
        let mut sh = self.inner.shared.lock().unwrap();
        if let Some(p) = sh.parser.as_mut() {
            p.process(data);
            sh.out_seq += 1;
            sh.last_out = Instant::now();
        }
    }

    /// A user key: the user has the screen now.
    fn user_key(&self) {
        if self.inner.active.swap(false, Ordering::SeqCst) {
            log::debug!("screens: a user key, paddock answers no screen");
        }
    }

    fn resize(&self) {
        if !self.active() {
            return;
        }
        let mut sh = self.inner.shared.lock().unwrap();
        let size = winsize(sh.caller).and_then(|(r, c)| {
            Some((std::num::NonZeroU16::new(r)?, std::num::NonZeroU16::new(c)?))
        });
        if let (Some((rows, cols)), Some(p)) = (size, sh.parser.as_mut()) {
            p.screen_mut().set_size(rows, cols);
        }
    }

    fn finish(&self) {
        self.inner.active.store(false, Ordering::SeqCst);
        if let Some(t) = self.thread.lock().unwrap().take() {
            let _ = t.join();
        }
    }
}

impl Inner {
    /// One decision on the screen as it is now. Returns the keys to send.
    fn step(&self, sh: &mut Shared, now: Instant) -> Option<Vec<u8>> {
        let rows = sh.rows();
        let found = find(&self.rules, &rows);
        if let Some((ri, n, at)) = sh.pending {
            let rule = &self.rules[ri];
            let still = matches!(found, Found::Rule(r, _) if r == ri);
            if !still {
                sh.pending = None;
                self.tell(sh, ri, true, rule.note_for(n));
            } else if now.duration_since(at) > STEP_TIMEOUT {
                sh.pending = None;
                // No more tries for this rule.
                sh.answers[ri] = MAX_ANSWERS;
                let text = format!("{} stayed on the screen after the answer", rule.name);
                self.tell(sh, ri, false, text);
            }
            return None;
        }
        let Found::Rule(ri, n) = found else {
            return None;
        };
        let rule = &self.rules[ri];
        if sh.answers[ri] >= MAX_ANSWERS || (!rule.always && (self.watched)()) {
            return None;
        }
        sh.answers[ri] += 1;
        sh.pending = Some((ri, n, now));
        Some(
            rule.send
                .iter()
                .flat_map(|k| key_bytes(k).unwrap_or_default())
                .collect(),
        )
    }

    fn tell(&self, sh: &mut Shared, ri: usize, ok: bool, text: String) {
        let r = Report {
            rule: self.rules[ri].id.clone(),
            keys: self.rules[ri].send.join(","),
            ok,
            text,
        };
        if let Some(log) = &sh.log {
            log(format!("\r\n[paddock: {}]\r\n", r.text).as_bytes());
        }
        (self.on_report)(&r);
        sh.reports.push(r);
    }

    /// Send the keys of the rule, one at a time.
    fn send(&self, sh: &Shared, ri: usize) -> bool {
        for (i, k) in self.rules[ri].send.iter().enumerate() {
            if i > 0 {
                std::thread::sleep(KEY_GAP);
            }
            if !self.active.load(Ordering::SeqCst) {
                return false;
            }
            let bytes = key_bytes(k).unwrap_or_default();
            if crate::relay::write_all_fd(sh.master, &bytes).is_err() {
                return false;
            }
        }
        true
    }
}

fn run(inner: Arc<Inner>) {
    let start = Instant::now();
    loop {
        std::thread::sleep(TICK);
        if !inner.active.load(Ordering::SeqCst) {
            break;
        }
        let mut sh = inner.shared.lock().unwrap();
        let settled = sh.out_seq != sh.seen_seq && sh.last_out.elapsed() >= SETTLE;
        if !settled && sh.pending.is_none() {
            if start.elapsed() > WINDOW {
                break;
            }
            continue;
        }
        if settled {
            sh.seen_seq = sh.out_seq;
        } else if sh.last_out.elapsed() < SETTLE {
            continue;
        }
        if inner.step(&mut sh, Instant::now()).is_some() {
            let ri = sh.pending.map(|p| p.0).unwrap_or(0);
            if !inner.send(&sh, ri) {
                break;
            }
            if let Some(p) = sh.pending.as_mut() {
                p.2 = Instant::now();
            }
        }
    }
    inner.active.store(false, Ordering::SeqCst);
}

fn winsize(fd: RawFd) -> Option<(u16, u16)> {
    if fd < 0 {
        return None;
    }
    // SAFETY: TIOCGWINSZ writes into a local winsize.
    unsafe {
        let mut size: libc::winsize = std::mem::zeroed();
        if libc::ioctl(fd, libc::TIOCGWINSZ, &mut size) < 0 {
            return None;
        }
        Some((size.ws_row, size.ws_col))
    }
}
