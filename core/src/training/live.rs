//! The training gate on a live terminal, next to the relay (relay.rs).
//!
//! The relay forwards the harness output to the caller unchanged and gives
//! a copy to `Gate::output`, which keeps the visible screen in a `vt100`
//! parser. When the output has been quiet for a moment, the gate thread
//! gives the rows to the `Machine`, and sends the key that the machine
//! names. User input goes through `Gate::write_user`, under the same lock
//! as the gate's keys: one writer. A user key that ends the gate ends it
//! before the key goes to the harness, and paddock sends no key after that.

use super::{Machine, Outcome, Rule, Step, UserKey};
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Output quiet for this long counts as a settled screen.
pub const SETTLE: Duration = Duration::from_millis(250);
/// The harness must answer a key within this time.
pub const STEP_TIMEOUT: Duration = Duration::from_secs(5);
/// The gate ends after this time in any case. A login in the browser can
/// take a while.
pub const DEADLINE: Duration = Duration::from_secs(20 * 60);
const TICK: Duration = Duration::from_millis(50);

type OnEnd = Box<dyn Fn(&Machine) + Send + Sync>;

pub struct Gate {
    inner: Arc<Inner>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

struct Inner {
    shared: Mutex<Shared>,
    active: AtomicBool,
    cols: Option<u16>,
    on_end: OnEnd,
}

struct Shared {
    machine: Machine,
    parser: Option<vt100::Parser>,
    /// Output chunks so far, and the count at the last settled screen.
    out_seq: u64,
    seen_seq: u64,
    last_out: Instant,
    /// When the gate sent its last key, until the harness answers.
    sent_at: Option<Instant>,
    fds: Option<Fds>,
    /// Set once the gate ended: the outcome for the notice after the run.
    result: Option<(Option<String>, Outcome)>,
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

#[derive(Clone, Copy)]
struct Fds {
    caller: RawFd,
    master: RawFd,
    pinned: bool,
}

impl Gate {
    /// A gate for these rules. They must pass `Rule::check`. The first
    /// rule width counts. `on_end` runs once, with the machine at its end,
    /// for the audit line.
    pub fn new(rules: Vec<Rule>, on_end: impl Fn(&Machine) + Send + Sync + 'static) -> Arc<Gate> {
        let cols = rules.iter().find_map(|r| r.cols);
        Arc::new(Gate {
            inner: Arc::new(Inner {
                shared: Mutex::new(Shared {
                    machine: Machine::new(rules),
                    parser: None,
                    out_seq: 0,
                    seen_seq: 0,
                    last_out: Instant::now(),
                    sent_at: None,
                    fds: None,
                    result: None,
                }),
                active: AtomicBool::new(true),
                cols,
                on_end: Box::new(on_end),
            }),
            thread: Mutex::new(None),
        })
    }

    /// Start on the relay's terminals, before the harness starts. With a
    /// rule width, the harness pty gets that width and the caller's rows.
    /// A caller narrower than the rule gets no gate.
    pub fn attach(&self, caller: RawFd, master: RawFd) {
        let (rows, cols) = winsize(caller).unwrap_or((0, 0));
        let rows = if rows == 0 { 40 } else { rows };
        let mut sh = self.inner.shared.lock().unwrap();
        let narrow = self.inner.cols.filter(|want| cols != 0 && cols < *want);
        sh.fds = Some(Fds {
            caller,
            master,
            pinned: self.inner.cols.is_some() && narrow.is_none(),
        });
        if let Some(want) = narrow {
            let o = sh
                .machine
                .on_timeout(&format!("the terminal is narrower than {want} columns"));
            self.inner.end_locked(&mut sh, o);
            return;
        }
        let pcols = match self.inner.cols {
            Some(want) => {
                set_winsize(master, rows, want);
                want
            }
            None if cols == 0 => 80,
            None => cols,
        };
        sh.parser = Some(vt100::Parser::new(
            std::num::NonZeroU16::new(rows).unwrap_or(std::num::NonZeroU16::MIN),
            std::num::NonZeroU16::new(pcols).unwrap_or(std::num::NonZeroU16::MIN),
            0,
        ));
        drop(sh);
        let inner = self.inner.clone();
        let t = std::thread::Builder::new()
            .name("paddock-training-gate".into())
            .spawn(move || run(inner));
        match t {
            Ok(t) => *self.thread.lock().unwrap() = Some(t),
            Err(e) => {
                let mut sh = self.inner.shared.lock().unwrap();
                let o = sh.machine.on_timeout(&format!("cannot start: {e}"));
                self.inner.end_locked(&mut sh, o);
            }
        }
    }

    pub fn active(&self) -> bool {
        self.inner.active.load(Ordering::SeqCst)
    }

    /// A copy of the harness output. The relay has already forwarded it.
    pub fn output(&self, data: &[u8]) {
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

    /// User input for the harness. The gate decides first, then the bytes
    /// go to the harness, under the lock of the one writer.
    pub fn write_user(&self, master: RawFd, data: &[u8]) -> std::io::Result<()> {
        if !self.active() {
            return crate::relay::write_all_fd(master, data);
        }
        let mut sh = self.inner.shared.lock().unwrap();
        if self.active() {
            let rows = sh.rows();
            if let UserKey::End(o) = sh.machine.on_user_key(&rows) {
                self.inner.end_locked(&mut sh, Some(o));
            }
        }
        crate::relay::write_all_fd(master, data)
    }

    /// The caller's terminal changed its size. The relay copies the new
    /// size to the harness right after.
    pub fn resize(&self) {
        if !self.active() {
            return;
        }
        let mut sh = self.inner.shared.lock().unwrap();
        let o = sh.machine.on_resize();
        self.inner.end_locked(&mut sh, o);
    }

    /// Stop the gate and its thread. The relay calls it before it closes
    /// the terminals.
    pub fn finish(&self) {
        if self.active() {
            let mut sh = self.inner.shared.lock().unwrap();
            let o = sh.machine.on_timeout("the harness ended");
            self.inner.end_locked(&mut sh, o);
        }
        if let Some(t) = self.thread.lock().unwrap().take() {
            let _ = t.join();
        }
    }

    /// The rule id (when a dialog came) and the outcome, once the gate ended.
    pub fn result(&self) -> Option<(Option<String>, Outcome)> {
        self.inner.shared.lock().unwrap().result.clone()
    }
}

impl Inner {
    /// End the gate, with the lock held: no key after this. The harness
    /// gets the caller's size again.
    fn end_locked(&self, sh: &mut Shared, o: Option<Outcome>) {
        if !self.active.swap(false, Ordering::SeqCst) {
            return;
        }
        sh.parser = None;
        if let Some(f) = sh.fds {
            if f.pinned {
                if let Some((rows, cols)) = winsize(f.caller) {
                    set_winsize(f.master, rows, cols);
                }
            }
        }
        let o = o
            .or_else(|| sh.machine.outcome().cloned())
            .unwrap_or_else(|| Outcome::NotShown("ended".into()));
        sh.result = Some((sh.machine.rule_id().map(str::to_string), o));
        (self.on_end)(&sh.machine);
    }
}

fn run(inner: Arc<Inner>) {
    let start = Instant::now();
    loop {
        std::thread::sleep(TICK);
        let mut sh = inner.shared.lock().unwrap();
        if !inner.active.load(Ordering::SeqCst) {
            break;
        }
        if sh.out_seq != sh.seen_seq && sh.last_out.elapsed() >= SETTLE {
            sh.seen_seq = sh.out_seq;
            sh.sent_at = None;
            let rows = sh.rows();
            match sh.machine.on_screen(&rows) {
                Step::Wait => {}
                Step::Send(k) => {
                    let master = sh.fds.map(|f| f.master).unwrap_or(-1);
                    if crate::relay::write_all_fd(master, k.bytes()).is_err() {
                        let o = sh.machine.on_timeout("cannot write to the harness");
                        inner.end_locked(&mut sh, o);
                        break;
                    }
                    sh.sent_at = Some(Instant::now());
                }
                Step::End(o) => {
                    inner.end_locked(&mut sh, Some(o));
                    break;
                }
            }
            continue;
        }
        let late = sh.sent_at.is_some_and(|t| t.elapsed() > STEP_TIMEOUT);
        if late || start.elapsed() > DEADLINE {
            let what = if late {
                "the harness did not answer a key"
            } else {
                "the setup took too long"
            };
            let o = sh.machine.on_timeout(what);
            inner.end_locked(&mut sh, o);
            break;
        }
    }
}

fn winsize(fd: RawFd) -> Option<(u16, u16)> {
    // SAFETY: TIOCGWINSZ writes into a local winsize.
    unsafe {
        let mut size: libc::winsize = std::mem::zeroed();
        if libc::ioctl(fd, libc::TIOCGWINSZ, &mut size) < 0 {
            return None;
        }
        Some((size.ws_row, size.ws_col))
    }
}

fn set_winsize(fd: RawFd, rows: u16, cols: u16) {
    let size = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: TIOCSWINSZ on the harness pty.
    unsafe {
        libc::ioctl(fd, libc::TIOCSWINSZ, &size);
    }
}

#[cfg(test)]
mod tests;
