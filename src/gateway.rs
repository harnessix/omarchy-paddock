//! The gateway: a host request is a pending, limited request, not a command.
//!
//! A sandbox sends a request (a capability and its arguments). The host
//! normalizes it and computes a hash over the normalized form. The hash
//! names the request:
//!
//! - The same request sent again while it waits attaches to the one that
//!   waits. No second approval card appears. A read that reached its time
//!   limit ("still waiting") sends the same request again.
//! - A result goes to one reader, once. A request sent again after that is a
//!   new request.
//! - The user approves what the normalized form says, and the work that runs
//!   is the work of that same form. Later phases sign the hash, so that a
//!   separate executor can check it.
//!
//! The reason that an agent gives is shown to the user, but it is not part
//! of the hash: the same action with other words is the same request.

use crate::broker::{Request, Response, Status};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// The domain of the hash, so that it never equals a hash of other data.
pub const DOMAIN: &str = "paddock-gw/v1";

/// Requests of one sandbox that can wait at the same time.
pub const MAX_OPEN: usize = 8;

/// A result that no reader collected in this time is dropped.
pub const KEEP_RESULT: Duration = Duration::from_secs(600);

/// How often a waiting read checks for a cancel.
const POLL: Duration = Duration::from_millis(100);

/// A JSON value with the keys of every object sorted, at every depth.
fn sorted(v: &serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            let mut out = serde_json::Map::new();
            for k in keys {
                out.insert(k.clone(), sorted(&m[k]));
            }
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(a) => serde_json::Value::Array(a.iter().map(sorted).collect()),
        other => other.clone(),
    }
}

/// The normalized form of a request: the protocol version, the capability
/// and the arguments, with sorted keys and no extra space. The reason is
/// left out.
pub fn canonical(req: &Request) -> Vec<u8> {
    let args = serde_json::Value::Object(req.args.clone().into_iter().collect());
    let v = serde_json::json!({ "v": req.v, "cap": req.cap, "args": sorted(&args) });
    serde_json::to_vec(&sorted(&v)).expect("a JSON value serializes")
}

/// The hash that names a request of one sandbox.
pub fn request_hash(sandbox: &str, req: &Request) -> String {
    let mut h = Sha256::new();
    h.update(DOMAIN.as_bytes());
    h.update([0]);
    h.update(sandbox.as_bytes());
    h.update([0]);
    h.update(canonical(req));
    crate::manifest::hex(&h.finalize())
}

enum Entry {
    Running,
    Done { resp: Response, at: Instant },
}

/// What a read gets.
#[derive(Debug)]
pub enum Wait {
    Done(Response),
    /// The time limit of this read passed first. The request still waits.
    StillWaiting,
    /// The reader went away.
    Cancelled,
}

/// The requests of one sandbox, by hash.
#[derive(Default)]
pub struct Ledger {
    entries: Mutex<HashMap<String, Entry>>,
    changed: Condvar,
}

impl Ledger {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn prune(entries: &mut HashMap<String, Entry>) {
        entries.retain(|_, e| match e {
            Entry::Running => true,
            Entry::Done { at, .. } => at.elapsed() < KEEP_RESULT,
        });
    }

    /// Start `req`, or attach to the same request that waits or has a
    /// result to collect. `run` does the work (ask the user, then execute)
    /// in its own thread. Returns the hash, or a response when the request
    /// cannot start.
    pub fn submit(
        self: &Arc<Self>,
        sandbox: &str,
        req: Request,
        run: impl FnOnce(Request) -> Response + Send + 'static,
    ) -> Result<String, Response> {
        let hash = request_hash(sandbox, &req);
        let mut entries = self.entries.lock().unwrap();
        Self::prune(&mut entries);
        if entries.contains_key(&hash) {
            return Ok(hash);
        }
        let open = entries
            .values()
            .filter(|e| matches!(e, Entry::Running))
            .count();
        if open >= MAX_OPEN {
            return Err(Response {
                status: Status::Invalid,
                message: format!(
                    "{MAX_OPEN} requests of this sandbox already wait; wait for one of them first"
                ),
                result: serde_json::Value::Null,
            });
        }
        entries.insert(hash.clone(), Entry::Running);
        drop(entries);
        let me = self.clone();
        let key = hash.clone();
        std::thread::spawn(move || {
            let resp = run(req);
            let mut entries = me.entries.lock().unwrap();
            entries.insert(
                key,
                Entry::Done {
                    resp,
                    at: Instant::now(),
                },
            );
            me.changed.notify_all();
        });
        Ok(hash)
    }

    /// Wait for the result of `hash`, at most `timeout`. The first reader
    /// that finds the result takes it. `cancelled` is checked while waiting.
    pub fn wait(&self, hash: &str, timeout: Duration, cancelled: impl Fn() -> bool) -> Wait {
        let start = Instant::now();
        let mut entries = self.entries.lock().unwrap();
        loop {
            match entries.get(hash) {
                Some(Entry::Done { .. }) => {
                    let Some(Entry::Done { resp, .. }) = entries.remove(hash) else {
                        unreachable!()
                    };
                    return Wait::Done(resp);
                }
                Some(Entry::Running) => {}
                None => {
                    return Wait::Done(Response {
                        status: Status::Invalid,
                        message: "no such request: send it again".into(),
                        result: serde_json::Value::Null,
                    })
                }
            }
            if cancelled() {
                return Wait::Cancelled;
            }
            let left = timeout.saturating_sub(start.elapsed());
            if left.is_zero() {
                return Wait::StillWaiting;
            }
            entries = self
                .changed
                .wait_timeout(entries, left.min(POLL))
                .unwrap()
                .0;
        }
    }

    /// Requests that wait now.
    pub fn open(&self) -> usize {
        self.entries
            .lock()
            .unwrap()
            .values()
            .filter(|e| matches!(e, Entry::Running))
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn req(cap: &str, args: serde_json::Value, reason: &str) -> Request {
        let args: BTreeMap<String, serde_json::Value> = serde_json::from_value(args).unwrap();
        Request {
            v: 1,
            cap: cap.into(),
            args,
            reason: reason.into(),
        }
    }

    fn done(msg: &str) -> Response {
        Response {
            status: Status::Done,
            message: msg.into(),
            result: serde_json::Value::Null,
        }
    }

    #[test]
    fn the_normalized_form_ignores_order_and_the_reason() {
        let a = req(
            "exec",
            serde_json::json!({"argv": ["ls", "-l"], "opts": {"b": 1, "a": 2}}),
            "look",
        );
        let b = req(
            "exec",
            serde_json::json!({"opts": {"a": 2, "b": 1}, "argv": ["ls", "-l"]}),
            "other words",
        );
        assert_eq!(canonical(&a), canonical(&b));
        assert_eq!(request_hash("s1", &a), request_hash("s1", &b));
        // Argument order inside a list is part of the action.
        let c = req("exec", serde_json::json!({"argv": ["-l", "ls"]}), "");
        assert_ne!(canonical(&a), canonical(&c));
        // The same request of another sandbox is another request.
        assert_ne!(request_hash("s1", &a), request_hash("s2", &a));
        assert_eq!(
            String::from_utf8(canonical(&c)).unwrap(),
            r#"{"args":{"argv":["-l","ls"]},"cap":"exec","v":1}"#
        );
    }

    #[test]
    fn the_same_request_attaches_and_its_result_goes_to_one_reader() {
        let l = Ledger::new();
        let runs = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let start = |l: &Arc<Ledger>| {
            let (runs, gate) = (runs.clone(), gate.clone());
            l.submit(
                "s1",
                req("exec", serde_json::json!({"argv": ["ls"]}), ""),
                move |_| {
                    runs.fetch_add(1, Ordering::SeqCst);
                    let (m, c) = &*gate;
                    let mut open = m.lock().unwrap();
                    while !*open {
                        open = c.wait(open).unwrap();
                    }
                    done("listed")
                },
            )
            .unwrap()
        };
        let h = start(&l);
        // The user has not answered: the read reports still waiting.
        assert!(matches!(
            l.wait(&h, Duration::from_millis(50), || false),
            Wait::StillWaiting
        ));
        // The agent sends the same request again: no second run.
        assert_eq!(start(&l), h);
        {
            let (m, c) = &*gate;
            *m.lock().unwrap() = true;
            c.notify_all();
        }
        match l.wait(&h, Duration::from_secs(5), || false) {
            Wait::Done(r) => assert_eq!(r.message, "listed"),
            other => panic!("{other:?}"),
        }
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        // Collected once: a second read finds nothing.
        match l.wait(&h, Duration::from_millis(10), || false) {
            Wait::Done(r) => assert_eq!(r.status, Status::Invalid),
            other => panic!("{other:?}"),
        }
        // Sent again after that, it is a new request.
        let again = start(&l);
        assert!(matches!(
            l.wait(&again, Duration::from_secs(5), || false),
            Wait::Done(_)
        ));
        assert_eq!(runs.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_reader_that_goes_away_leaves_the_request_waiting() {
        let l = Ledger::new();
        let h = l
            .submit("s1", req("notify", serde_json::json!({}), ""), |_| {
                std::thread::sleep(Duration::from_millis(200));
                done("shown")
            })
            .unwrap();
        assert!(matches!(
            l.wait(&h, Duration::from_secs(5), || true),
            Wait::Cancelled
        ));
        assert_eq!(l.open(), 1);
        assert!(matches!(
            l.wait(&h, Duration::from_secs(5), || false),
            Wait::Done(_)
        ));
    }

    #[test]
    fn a_sandbox_cannot_open_too_many_requests() {
        let l = Ledger::new();
        for i in 0..MAX_OPEN {
            l.submit("s1", req("exec", serde_json::json!({"n": i}), ""), |_| {
                std::thread::sleep(Duration::from_secs(2));
                done("")
            })
            .unwrap();
        }
        let r = l
            .submit("s1", req("exec", serde_json::json!({"n": 99}), ""), |_| {
                done("")
            })
            .unwrap_err();
        assert_eq!(r.status, Status::Invalid);
        // A request that already waits still attaches.
        assert!(l
            .submit(
                "s1",
                req("exec", serde_json::json!({"n": 0}), ""),
                |_| done("")
            )
            .is_ok());
    }
}
