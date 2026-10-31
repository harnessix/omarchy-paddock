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

/// What runs a request once it is in the ledger: the broker of the sandbox
/// (ask the user, then execute).
pub type Handler = std::sync::Arc<dyn Fn(Request) -> Response + Send + Sync>;

/// The largest request: the same limit as the broker socket.
pub const MAX_REQUEST: usize = 64 * 1024;
/// The largest response, for example the output of a command.
pub const MAX_RESPONSE: usize = 8 << 20;

/// `/dev/paddock/gw`: a host request as a device. Write a request (the
/// JSON of `paddock-host`, with an optional `timeout` for this read in
/// seconds), read the response. When the time limit of the read passes
/// first, the read fails with ETIME (still waiting) and the request keeps
/// waiting: send the same request again to attach to it.
pub struct GwDevice {
    sandbox: String,
    ledger: Arc<Ledger>,
    handler: Handler,
}

impl GwDevice {
    pub fn new(sandbox: &str, handler: Handler) -> Self {
        Self {
            sandbox: sandbox.to_string(),
            ledger: Ledger::new(),
            handler,
        }
    }
}

pub fn info() -> crate::devices::Info {
    crate::devices::Info {
        request: "a host request as JSON: {\"v\":1,\"cap\":\"...\",\"args\":{...},\"reason\":\"...\"}, and an optional \"timeout\" in seconds for this read".into(),
        response: "the response as JSON: {\"status\":\"done|denied|disabled|timeout|failed|invalid\",...}. ETIME: still waiting, send the same request again".into(),
        limits: crate::devices::Limits {
            max_request: MAX_REQUEST,
            max_response: MAX_RESPONSE,
            deadline_seconds: crate::waiting::MAX_READ_TIMEOUT + 5,
        },
    }
}

impl crate::devices::Device for GwDevice {
    fn name(&self) -> &str {
        "gw"
    }

    fn info(&self) -> crate::devices::Info {
        info()
    }

    fn call(
        &self,
        call: &crate::devices::Call,
        request: &[u8],
    ) -> Result<Vec<u8>, crate::devices::DevError> {
        use crate::devices::DevError;
        let mut v: serde_json::Value =
            serde_json::from_slice(request).map_err(|_| DevError::Invalid)?;
        let o = v.as_object_mut().ok_or(DevError::Invalid)?;
        let timeout = match o.remove("timeout") {
            None => crate::waiting::DEFAULT_READ_TIMEOUT,
            Some(t) => t
                .as_u64()
                .filter(|t| (1..=crate::waiting::MAX_READ_TIMEOUT).contains(t))
                .ok_or(DevError::Invalid)?,
        };
        let req: Request = serde_json::from_value(v).map_err(|_| DevError::Invalid)?;
        let handler = self.handler.clone();
        let hash = match self.ledger.submit(&self.sandbox, req, move |r| handler(r)) {
            Ok(h) => h,
            Err(resp) => return serde_json::to_vec(&resp).map_err(|_| DevError::Failed),
        };
        match self
            .ledger
            .wait(&hash, Duration::from_secs(timeout), || call.cancelled())
        {
            Wait::Done(resp) => serde_json::to_vec(&resp).map_err(|_| DevError::Failed),
            Wait::StillWaiting => Err(DevError::Waiting),
            Wait::Cancelled => Err(DevError::Cancelled),
        }
    }
}

// ---------------------------------------------------------------------------
// Grants: what the user approved, signed, for the executor.

/// HMAC-SHA256 (RFC 2104).
pub fn hmac(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(k.map(|b| b ^ 0x36));
    inner.update(msg);
    let mut outer = Sha256::new();
    outer.update(k.map(|b| b ^ 0x5c));
    outer.update(inner.finalize());
    outer.finalize().into()
}

/// Equal bytes, in a time that does not depend on where they differ.
pub fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

/// How long a grant is valid after the user approved it.
pub const GRANT_LIFETIME: u64 = 30;

/// One approved request for the executor. Valid once, for a short time.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Grant {
    pub v: u32,
    /// `request_hash(sandbox, request)`.
    pub hash: String,
    pub cap: String,
    pub sandbox: String,
    pub project: std::path::PathBuf,
    /// Unix time after which the executor refuses it.
    pub expires: u64,
    /// Random, so that a grant cannot be used twice.
    pub nonce: String,
}

/// What paddock sends to the executor: the grant, the request it covers,
/// the host environment for the command, and the signature over all of it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Job {
    pub grant: Grant,
    pub request: Request,
    /// From the host process of paddock, never from the sandbox.
    pub env: std::collections::BTreeMap<String, String>,
    /// What the host worked out for the request before the card, for
    /// example the scratch repository and URL of a push. Null for exec.
    #[serde(default)]
    pub plan: serde_json::Value,
    pub mac: String,
}

/// The bytes that the signature covers.
fn signed_bytes(
    grant: &Grant,
    request: &Request,
    env: &std::collections::BTreeMap<String, String>,
    plan: &serde_json::Value,
) -> Vec<u8> {
    let v = serde_json::json!({
        "plan": plan,
        "grant": grant,
        "request": serde_json::from_slice::<serde_json::Value>(&canonical(request))
            .expect("canonical() is JSON"),
        "env": env,
    });
    serde_json::to_vec(&sorted(&v)).expect("a JSON value serializes")
}

fn random_hex(n: usize) -> std::io::Result<String> {
    let mut b = vec![0u8; n];
    std::io::Read::read_exact(&mut std::fs::File::open("/dev/urandom")?, &mut b)?;
    Ok(crate::manifest::hex(&b))
}

/// The signing key, `gateway.key` in paddock's state folder (mode 0600).
/// The first caller makes it. Sandboxes never see paddock's folders, and
/// `fetch` refuses them.
pub fn key(env: &crate::env::HostEnv) -> std::io::Result<Vec<u8>> {
    let path = key_file(env);
    match std::fs::read(&path) {
        Ok(k) if k.len() == 32 => return Ok(k),
        Ok(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{} is not a 32-byte key", path.display()),
            ))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let mut k = vec![0u8; 32];
    std::io::Read::read_exact(&mut std::fs::File::open("/dev/urandom")?, &mut k)?;
    if let Some(d) = path.parent() {
        crate::privfs::dir(d)?;
    }
    // Two processes can start at once: the first file wins.
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
    {
        Ok(mut f) => {
            f.write_all(&k)?;
            f.sync_all()?;
            Ok(k)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            std::thread::sleep(Duration::from_millis(50));
            let k = std::fs::read(&path)?;
            if k.len() == 32 {
                Ok(k)
            } else {
                Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "the gateway key is incomplete",
                ))
            }
        }
        Err(e) => Err(e),
    }
}

pub fn key_file(env: &crate::env::HostEnv) -> std::path::PathBuf {
    env.paddock_state().join("gateway.key")
}

/// Sign an approved request for the executor.
pub fn sign(
    key: &[u8],
    sandbox: &str,
    project: &std::path::Path,
    request: &Request,
    env: std::collections::BTreeMap<String, String>,
    plan: serde_json::Value,
) -> std::io::Result<Job> {
    let grant = Grant {
        v: 1,
        hash: request_hash(sandbox, request),
        cap: request.cap.clone(),
        sandbox: sandbox.to_string(),
        project: project.to_path_buf(),
        expires: crate::state::now() + GRANT_LIFETIME,
        nonce: random_hex(16)?,
    };
    let mac = crate::manifest::hex(&hmac(key, &signed_bytes(&grant, request, &env, &plan)));
    Ok(Job {
        grant,
        request: request.clone(),
        env,
        plan,
        mac,
    })
}

/// Why the executor refuses a job.
#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    Signature,
    Expired,
    Replayed,
    Mismatch,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Refused::Signature => "the signature does not match",
            Refused::Expired => "the grant expired",
            Refused::Replayed => "the grant was used already",
            Refused::Mismatch => "the grant does not cover this request",
        })
    }
}

/// Nonces of grants that the executor accepted, until they expire.
#[derive(Default)]
pub struct Seen(Mutex<HashMap<String, u64>>);

/// Check a job: the signature, the time, that it is new, and that the
/// grant covers exactly this request.
pub fn verify(key: &[u8], job: &Job, seen: &Seen, now: u64) -> Result<(), Refused> {
    let want = hmac(
        key,
        &signed_bytes(&job.grant, &job.request, &job.env, &job.plan),
    );
    let got = crate::manifest::unhex(&job.mac).unwrap_or_default();
    if !same(&want, &got) {
        return Err(Refused::Signature);
    }
    let g = &job.grant;
    if g.expires < now || g.expires > now + GRANT_LIFETIME + 5 {
        return Err(Refused::Expired);
    }
    if g.v != 1 || g.cap != job.request.cap || g.hash != request_hash(&g.sandbox, &job.request) {
        return Err(Refused::Mismatch);
    }
    let mut seen = seen.0.lock().unwrap();
    seen.retain(|_, exp| *exp >= now);
    if seen.insert(g.nonce.clone(), g.expires).is_some() {
        return Err(Refused::Replayed);
    }
    Ok(())
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

    #[test]
    fn hmac_matches_rfc_4231() {
        // Test case 1.
        assert_eq!(
            crate::manifest::hex(&hmac(&[0x0b; 20], b"Hi There")),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        // Test case 2.
        assert_eq!(
            crate::manifest::hex(&hmac(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // Test case 6: a key longer than the block.
        assert_eq!(
            crate::manifest::hex(&hmac(
                &[0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            )),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn the_executor_runs_only_what_was_signed_once_and_in_time() {
        let key = [7u8; 32];
        let r = req("exec", serde_json::json!({"argv": ["ls"]}), "why");
        let env: std::collections::BTreeMap<String, String> =
            [("PATH".to_string(), "/usr/bin".to_string())].into();
        let job = sign(
            &key,
            "s1",
            std::path::Path::new("/p"),
            &r,
            env,
            serde_json::json!({"scratch": "/s"}),
        )
        .unwrap();
        let now = crate::state::now();
        let seen = Seen::default();
        assert_eq!(verify(&key, &job, &seen, now), Ok(()));
        // The same grant twice.
        assert_eq!(verify(&key, &job, &seen, now), Err(Refused::Replayed));
        // Another key.
        assert_eq!(
            verify(&[8u8; 32], &job, &Seen::default(), now),
            Err(Refused::Signature)
        );
        // A changed argument, a changed environment, a changed project.
        let mut j = job.clone();
        j.request
            .args
            .insert("argv".into(), serde_json::json!(["rm", "-rf", "/"]));
        assert_eq!(
            verify(&key, &j, &Seen::default(), now),
            Err(Refused::Signature)
        );
        let mut j = job.clone();
        j.env.insert("LD_PRELOAD".into(), "/x.so".into());
        assert_eq!(
            verify(&key, &j, &Seen::default(), now),
            Err(Refused::Signature)
        );
        let mut j = job.clone();
        j.grant.project = "/".into();
        assert_eq!(
            verify(&key, &j, &Seen::default(), now),
            Err(Refused::Signature)
        );
        // The reason is not part of what runs.
        let mut j = job.clone();
        j.request.reason = "other words".into();
        assert_eq!(verify(&key, &j, &Seen::default(), now), Ok(()));
        // Too late, or a grant that claims to be valid far ahead.
        assert_eq!(
            verify(&key, &job, &Seen::default(), now + GRANT_LIFETIME + 1),
            Err(Refused::Expired)
        );
        // A grant signed for one request does not cover another.
        let other = req("exec", serde_json::json!({"argv": ["id"]}), "");
        let mut j = sign(
            &key,
            "s1",
            std::path::Path::new("/p"),
            &other,
            Default::default(),
            serde_json::Value::Null,
        )
        .unwrap();
        j.grant.hash = job.grant.hash.clone();
        j.mac = crate::manifest::hex(&hmac(
            &key,
            &signed_bytes(&j.grant, &j.request, &j.env, &j.plan),
        ));
        assert_eq!(
            verify(&key, &j, &Seen::default(), now),
            Err(Refused::Mismatch)
        );
    }

    #[test]
    fn the_key_is_made_once_and_kept_private() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        let env = crate::env::HostEnv::from_parts(
            t.path().join("home"),
            "demo".into(),
            1,
            1,
            Default::default(),
        );
        let a = key(&env).unwrap();
        assert_eq!(a.len(), 32);
        assert_eq!(key(&env).unwrap(), a);
        let mode = std::fs::metadata(key_file(&env))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }
}
