//! Waiting devices: a read blocks until an event, so an agent never needs
//! `sleep` to wait.
//!
//! - `wait`: the read returns at a time. The request is an RFC 3339 time or
//!   a duration.
//! - `ask`: the agent writes a question; the read returns the user's answer.
//!   The question appears on the approval card of the bar panel (and as a
//!   desktop notification when the bar does not run). It uses the same
//!   pending files as host requests, so `paddock approvals` lists it too.
//!
//! Both take plain text, or a JSON object with a `timeout` in seconds: the
//! longest wait of this one read (default `waiting::DEFAULT_READ_TIMEOUT`,
//! at most `waiting::MAX_READ_TIMEOUT`). When it passes before the event,
//! the read fails with ETIME (still waiting). An agent then sends the same
//! request again: a `wait` for the same time, or an `ask` with the same
//! question, which finds the question that is still open instead of a new
//! card. The answer text goes to the agent only. Logs and the audit log keep
//! sizes and outcomes, never the question or the answer.

use super::{Call, DevError, Device, DeviceCtx, Info, Limits};
use crate::broker;
use crate::waiting;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How often a waiting read checks for its event and for a cancel.
const POLL: Duration = Duration::from_millis(100);
/// The longest question, in characters.
pub const MAX_QUESTION: usize = 4000;
/// Choices of one question, and the length of each.
pub const MAX_CHOICES: usize = 8;
pub const MAX_CHOICE: usize = 64;
/// Questions that one sandbox can have open at the same time.
const MAX_OPEN_QUESTIONS: usize = 4;
/// A question that no read waited for during this time is withdrawn: the
/// agent stopped asking.
const ABANDONED: Duration = Duration::from_secs(120);

/// The limits of a waiting device: the hard deadline is a little longer than
/// the longest read, so the read itself always reports "still waiting".
fn limits(max_request: usize, max_response: usize) -> Limits {
    Limits {
        max_request,
        max_response,
        deadline_seconds: waiting::MAX_READ_TIMEOUT + 5,
    }
}

/// A request: plain text, or a JSON object. `field` names the main text in
/// the JSON form.
struct Parsed {
    text: String,
    timeout: u64,
    json: serde_json::Value,
}

fn parse(request: &[u8], field: &str) -> Result<Parsed, DevError> {
    let raw = std::str::from_utf8(request).map_err(|_| DevError::Invalid)?;
    let t = raw.trim();
    if t.starts_with('{') {
        let json: serde_json::Value = serde_json::from_str(t).map_err(|_| DevError::Invalid)?;
        let obj = json.as_object().ok_or(DevError::Invalid)?;
        for k in obj.keys() {
            if k != field && k != "timeout" && k != "choices" {
                return Err(DevError::Invalid);
            }
        }
        let text = obj
            .get(field)
            .and_then(|v| v.as_str())
            .ok_or(DevError::Invalid)?
            .to_string();
        let asked = match obj.get("timeout") {
            None => None,
            Some(v) => Some(v.as_u64().ok_or(DevError::Invalid)?),
        };
        let timeout = waiting::read_timeout(asked).map_err(|_| DevError::Invalid)?;
        return Ok(Parsed {
            text,
            timeout,
            json,
        });
    }
    Ok(Parsed {
        text: t.to_string(),
        timeout: waiting::DEFAULT_READ_TIMEOUT,
        json: serde_json::Value::Null,
    })
}

/// When this read stops waiting: after its own timeout, and never after the
/// deadline of the call.
fn read_end(call: &Call, timeout: u64) -> Instant {
    (Instant::now() + Duration::from_secs(timeout)).min(call.deadline - Duration::from_secs(2))
}

/// `/dev/paddock/wait`: the read returns at a time.
pub struct Wait;

impl Device for Wait {
    fn name(&self) -> &str {
        "wait"
    }
    fn info(&self) -> Info {
        Info {
            request: format!(
                "the time to wait for: an RFC 3339 time (2026-10-30T15:00:00Z) or a duration (90, 90s, 15m, 2h), at most {} days ahead; or JSON {{\"until\": TIME, \"timeout\": SECONDS}}. One read waits at most timeout seconds (default {}, at most {}), then fails with ETIME (still waiting): send the same time again. For a long wait, send an RFC 3339 time, not a duration, so that each new read waits for the same moment.",
                waiting::MAX_WAIT_AHEAD / 86400,
                waiting::DEFAULT_READ_TIMEOUT,
                waiting::MAX_READ_TIMEOUT
            ),
            response: "one line at the time: reached TIME (RFC 3339, UTC)".into(),
            limits: limits(1024, 128),
        }
    }
    fn call(&self, call: &Call, request: &[u8]) -> Result<Vec<u8>, DevError> {
        let p = parse(request, "until")?;
        let at =
            waiting::parse_when(&p.text, crate::state::now()).map_err(|_| DevError::Invalid)?;
        let end = read_end(call, p.timeout);
        loop {
            if crate::state::now() >= at {
                return Ok(format!("reached {}\n", waiting::format_rfc3339(at)).into_bytes());
            }
            if call.cancelled() {
                return Err(DevError::Cancelled);
            }
            if Instant::now() >= end {
                return Err(DevError::Waiting);
            }
            std::thread::sleep(POLL);
        }
    }
}

/// The description of `ask`, for the catalog.
pub fn ask_info() -> Info {
    Info {
        request: format!(
            "a question for the user, as text (at most {MAX_QUESTION} characters); or JSON {{\"question\": TEXT, \"choices\": [up to {MAX_CHOICES} short answers], \"timeout\": SECONDS}}. The question appears on the approval card on the host. One read waits at most timeout seconds (default {}, at most {}), then fails with ETIME (still waiting): send the same question again, and the read finds the open question. A question that nobody reads for {} seconds is withdrawn.",
            waiting::DEFAULT_READ_TIMEOUT,
            waiting::MAX_READ_TIMEOUT,
            ABANDONED.as_secs()
        ),
        response: format!(
            "the user's answer, as text (at most {} characters). EACCES: the user dismissed the question",
            broker::MAX_REPLY
        ),
        limits: limits(MAX_QUESTION * 4 + 4096, broker::MAX_REPLY * 4),
    }
}

/// A question on the card.
struct Open {
    id: String,
    chars: usize,
    choices: usize,
    /// Reads that wait for it now.
    readers: usize,
    /// When the last read stopped waiting.
    left: Instant,
    note: Option<broker::notify::Handle>,
}

/// `/dev/paddock/ask`: the read returns the user's answer.
pub struct Ask {
    ctx: DeviceCtx,
    open: Mutex<HashMap<String, Open>>,
}

/// What the user answered.
enum Reply {
    Text(String),
    Dismissed,
}

impl Ask {
    pub fn new(ctx: DeviceCtx) -> Self {
        Self {
            ctx,
            open: Mutex::new(HashMap::new()),
        }
    }

    fn pending_file(&self, id: &str) -> std::path::PathBuf {
        broker::approvals_dir(&self.ctx.env).join(format!("{id}.json"))
    }

    fn answer_file(&self, id: &str) -> std::path::PathBuf {
        broker::approvals_dir(&self.ctx.env).join(format!("{id}.answer"))
    }

    /// Remove the question from the card.
    fn withdraw(&self, q: &Open) {
        if let Some(n) = &q.note {
            n.close();
        }
        let _ = std::fs::remove_file(self.pending_file(&q.id));
        let _ = std::fs::remove_file(self.answer_file(&q.id));
    }

    /// One audit line per question: sizes and the outcome, never the text.
    fn audit(&self, q: &Open, decision: &str, reply_chars: Option<usize>) {
        let mut line = serde_json::json!({
            "time": crate::state::now(),
            "sandbox": self.ctx.sandbox,
            "harness": self.ctx.harness,
            "project": self.ctx.project,
            "capability": "ask",
            "request": q.id,
            "detail": format!(
                "a question of {} characters with {} choices (the text is not logged)",
                q.chars, q.choices
            ),
            "decision": decision,
            "status": if reply_chars.is_some() { "done" } else { "denied" },
        });
        if let Some(n) = reply_chars {
            line["message"] = format!("an answer of {n} characters").into();
        }
        if let Some(p) = self.ctx.started_by.last() {
            line["parent"] = p.clone().into();
            line["chain"] = serde_json::json!(self.ctx.started_by);
        }
        broker::audit_line(&self.ctx.env, &line);
    }

    /// Withdraw questions that no read waited for in a while.
    fn sweep(&self, open: &mut HashMap<String, Open>) {
        let stale: Vec<String> = open
            .iter()
            .filter(|(_, q)| q.readers == 0 && q.left.elapsed() > ABANDONED)
            .map(|(k, _)| k.clone())
            .collect();
        for k in stale {
            if let Some(q) = open.remove(&k) {
                self.withdraw(&q);
                self.audit(&q, "withdrawn: the agent stopped waiting", None);
            }
        }
    }

    /// The desktop notification when the bar panel does not run. Its buttons
    /// write the same answer file as the card.
    fn notify(&self, p: &broker::Pending) -> Option<broker::notify::Handle> {
        if broker::approval_ui_active(&self.ctx.env) {
            return None;
        }
        let mut actions: Vec<(String, String)> = p
            .choices
            .iter()
            .enumerate()
            .map(|(i, c)| (format!("choice:{i}"), c.clone()))
            .collect();
        actions.push(("deny".into(), "Dismiss".into()));
        let note = broker::notify::Note {
            summary: format!("paddock: {} asks you a question", p.harness),
            body: format!(
                "{}{}\n\nType an answer: paddock approvals answer {} --text 'ANSWER'",
                crate::spawn::chain_text(&p.started_by)
                    .map(|c| format!("{c}\n\n"))
                    .unwrap_or_default(),
                p.detail,
                p.id
            ),
            actions,
            critical: true,
        };
        let (env, id, choices) = (self.ctx.env.clone(), p.id.clone(), p.choices.clone());
        let on_action = move |key: &str| {
            if let Some(i) = key
                .strip_prefix("choice:")
                .and_then(|i| i.parse::<usize>().ok())
            {
                if let Some(c) = choices.get(i) {
                    let _ = broker::answer_reply(&env, &id, c);
                }
            } else if key == "deny" {
                let _ = broker::answer(&env, &id, broker::Answer::Deny);
            }
        };
        match broker::notify::ask(&self.ctx.env, &note, on_action) {
            Ok(h) => Some(h),
            Err(e) => {
                log::debug!("cannot show the question as a notification: {e:#}");
                None
            }
        }
    }

    /// The open question for this text, or a new one on the card.
    fn attach(&self, question: &str, choices: &[String]) -> Result<String, DevError> {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(question.as_bytes());
        for c in choices {
            h.update([0u8]);
            h.update(c.as_bytes());
        }
        let key = crate::manifest::hex(&h.finalize());
        let mut open = self.open.lock().unwrap();
        self.sweep(&mut open);
        if let Some(q) = open.get_mut(&key) {
            q.readers += 1;
            return Ok(key);
        }
        if open.len() >= MAX_OPEN_QUESTIONS {
            return Err(DevError::Failed);
        }
        let now = crate::state::now();
        let p = broker::Pending {
            id: broker::new_request_id(),
            sandbox: self.ctx.sandbox.clone(),
            harness: self.ctx.harness.clone(),
            project: self.ctx.project.clone(),
            capability: broker::ASK_CAPABILITY.into(),
            summary: "asks you a question".into(),
            detail: question.to_string(),
            reason: String::new(),
            created: now,
            // A question has no time limit; it waits until the agent stops.
            expires: 0,
            session_allowed: false,
            started_by: self.ctx.started_by.clone(),
            choices: choices.to_vec(),
        };
        if let Err(e) = broker::write_pending(&self.ctx.env, &p) {
            log::warn!("cannot put the question on the card: {e}");
            return Err(DevError::Failed);
        }
        let note = self.notify(&p);
        open.insert(
            key.clone(),
            Open {
                id: p.id.clone(),
                chars: question.chars().count(),
                choices: choices.len(),
                readers: 1,
                left: Instant::now(),
                note,
            },
        );
        Ok(key)
    }

    /// The answer, when the user gave one.
    fn answered(&self, id: &str) -> Option<Reply> {
        let text = std::fs::read_to_string(self.answer_file(id)).ok()?;
        let v: serde_json::Value = serde_json::from_str(&text).ok()?;
        if let Some(r) = v.get("reply").and_then(|r| r.as_str()) {
            return Some(Reply::Text(r.to_string()));
        }
        // `paddock approvals answer ID deny`, or a notification's Dismiss.
        Some(Reply::Dismissed)
    }
}

impl Drop for Ask {
    /// The sandbox ended: its open questions leave the card.
    fn drop(&mut self) {
        let open = std::mem::take(&mut *self.open.lock().unwrap());
        for q in open.values() {
            self.withdraw(q);
            self.audit(q, "withdrawn: the sandbox ended", None);
        }
    }
}

fn check_question(p: &Parsed) -> Result<(String, Vec<String>), DevError> {
    let q = p.text.trim().to_string();
    if q.is_empty() || q.chars().count() > MAX_QUESTION {
        return Err(DevError::Invalid);
    }
    let mut choices = Vec::new();
    if let Some(c) = p.json.get("choices") {
        let list = c.as_array().ok_or(DevError::Invalid)?;
        if list.len() > MAX_CHOICES {
            return Err(DevError::Invalid);
        }
        for v in list {
            let s = v.as_str().ok_or(DevError::Invalid)?.trim();
            if s.is_empty() || s.chars().count() > MAX_CHOICE || s.chars().any(char::is_control) {
                return Err(DevError::Invalid);
            }
            choices.push(s.to_string());
        }
    }
    Ok((q, choices))
}

impl Device for Ask {
    fn name(&self) -> &str {
        "ask"
    }
    fn info(&self) -> Info {
        ask_info()
    }
    fn call(&self, call: &Call, request: &[u8]) -> Result<Vec<u8>, DevError> {
        let p = parse(request, "question")?;
        let (question, choices) = check_question(&p)?;
        let key = self.attach(&question, &choices)?;
        let end = read_end(call, p.timeout);
        let leave = |outcome: Result<Vec<u8>, DevError>| {
            let mut open = self.open.lock().unwrap();
            if let Some(q) = open.get_mut(&key) {
                q.readers = q.readers.saturating_sub(1);
                q.left = Instant::now();
            }
            outcome
        };
        loop {
            let id = match self.open.lock().unwrap().get(&key) {
                Some(q) => q.id.clone(),
                // Another read of the same question got the answer.
                None => return Err(DevError::Cancelled),
            };
            if let Some(reply) = self.answered(&id) {
                let q = self.open.lock().unwrap().remove(&key);
                if let Some(q) = q {
                    self.withdraw(&q);
                    match &reply {
                        Reply::Text(t) => self.audit(&q, "answered", Some(t.chars().count())),
                        Reply::Dismissed => self.audit(&q, "dismissed", None),
                    }
                }
                return match reply {
                    Reply::Text(t) => Ok(t.into_bytes()),
                    Reply::Dismissed => Err(DevError::Denied),
                };
            }
            if call.cancelled() {
                return leave(Err(DevError::Cancelled));
            }
            if Instant::now() >= end {
                return leave(Err(DevError::Waiting));
            }
            std::thread::sleep(POLL);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(dir: &std::path::Path) -> DeviceCtx {
        // No session bus: no desktop notification from a test.
        let vars = [("XDG_RUNTIME_DIR", "run"), ("XDG_STATE_HOME", "state")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), dir.join(v).display().to_string()))
            .collect();
        let env =
            crate::env::HostEnv::from_parts(dir.join("home"), "demo".into(), 1000, 1000, vars);
        DeviceCtx {
            env,
            sandbox: "asker".into(),
            harness: "fake".into(),
            project: dir.join("project"),
            started_by: vec![],
        }
    }

    fn errno(r: Result<Vec<u8>, DevError>) -> i32 {
        i32::from(r.unwrap_err().errno())
    }

    #[test]
    fn wait_returns_at_the_time_and_reports_still_waiting() {
        let now = crate::state::now();
        let call = Call::for_test("s", Duration::from_secs(60));
        // A time in the past returns at once.
        let past = waiting::format_rfc3339(now - 5);
        let out = Wait.call(&call, past.as_bytes()).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), format!("reached {past}\n"));
        // Two seconds ahead, with a longer read timeout: it waits for it.
        let at = waiting::format_rfc3339(now + 2);
        let start = Instant::now();
        let req = format!("{{\"until\": \"{at}\", \"timeout\": 10}}");
        let out = Wait.call(&call, req.as_bytes()).unwrap();
        assert!(start.elapsed() >= Duration::from_millis(900));
        assert!(start.elapsed() < Duration::from_secs(5));
        assert_eq!(String::from_utf8(out).unwrap(), format!("reached {at}\n"));
        // An hour ahead with a read timeout of one second: still waiting.
        let start = Instant::now();
        let r = Wait.call(&call, br#"{"until": "1h", "timeout": 1}"#);
        assert_eq!(errno(r), libc::ETIME);
        assert!(start.elapsed() < Duration::from_secs(3));
        // Wrong requests.
        for bad in [
            &b"soon"[..],
            b"",
            b"9d",
            br#"{"until": "1h", "timeout": 0}"#,
            br#"{"until": "1h", "timeout": 601}"#,
            br#"{"until": "1h", "sandbox": "other"}"#,
            b"\xff",
        ] {
            assert_eq!(errno(Wait.call(&call, bad)), libc::EINVAL, "{bad:?}");
        }
    }

    #[test]
    fn ask_puts_the_question_on_the_card_and_returns_the_answer() {
        let dir = tempfile::tempdir().unwrap();
        let c = ctx(dir.path());
        let env = c.env.clone();
        let ask = std::sync::Arc::new(Ask::new(c));
        let call = Call::for_test("asker", Duration::from_secs(60));
        let req = br#"{"question": "Deploy to staging?", "choices": ["yes", "no"], "timeout": 1}"#;
        // Nobody answers: still waiting, and the card stays.
        assert_eq!(errno(ask.call(&call, req)), libc::ETIME);
        let p = broker::pending(&env);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].capability, "ask");
        assert_eq!(p[0].detail, "Deploy to staging?");
        assert_eq!(p[0].choices, ["yes", "no"]);
        assert!(!p[0].session_allowed);
        let id = p[0].id.clone();
        // The same question again finds the same card.
        assert_eq!(errno(ask.call(&call, req)), libc::ETIME);
        assert_eq!(broker::pending(&env).len(), 1);
        // A host answer word cannot answer a question; a text can.
        assert!(broker::answer(&env, &id, broker::Answer::Once).is_err());
        let a2 = ask.clone();
        let t = std::thread::spawn(move || {
            let call = Call::for_test("asker", Duration::from_secs(60));
            a2.call(
                &call,
                br#"{"question": "Deploy to staging?", "choices": ["yes", "no"], "timeout": 10}"#,
            )
        });
        std::thread::sleep(Duration::from_millis(300));
        broker::answer_reply(&env, &id, "yes, but only the API").unwrap();
        let out = t.join().unwrap().unwrap();
        assert_eq!(out, b"yes, but only the API");
        assert!(broker::pending(&env).is_empty());
        // The audit log has the sizes, not the text.
        let log = std::fs::read_to_string(broker::audit_file(&env)).unwrap();
        assert!(log.contains("\"capability\":\"ask\""));
        assert!(log.contains("an answer of 21 characters"));
        assert!(!log.contains("staging"));
        assert!(!log.contains("only the API"));
    }

    #[test]
    fn a_dismissed_question_fails_with_eacces_and_wrong_questions_are_invalid() {
        let dir = tempfile::tempdir().unwrap();
        let c = ctx(dir.path());
        let env = c.env.clone();
        let ask = std::sync::Arc::new(Ask::new(c));
        let a2 = ask.clone();
        let t = std::thread::spawn(move || {
            let call = Call::for_test("asker", Duration::from_secs(60));
            a2.call(&call, b"Can I delete the old branches?")
        });
        let start = Instant::now();
        while broker::pending(&env).is_empty() && start.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(50));
        }
        let id = broker::pending(&env)[0].id.clone();
        // A question cannot take a host request's text answer path the
        // other way round either.
        broker::answer(&env, &id, broker::Answer::Deny).unwrap();
        assert_eq!(errno(t.join().unwrap()), libc::EACCES);
        assert!(broker::pending(&env).is_empty());
        let call = Call::for_test("asker", Duration::from_secs(60));
        let long = "x".repeat(MAX_QUESTION + 1);
        let many: Vec<String> = (0..MAX_CHOICES + 1).map(|i| format!("c{i}")).collect();
        for bad in [
            String::new(),
            "   ".into(),
            long,
            serde_json::json!({ "question": "q", "choices": many }).to_string(),
            serde_json::json!({ "question": "q", "choices": ["a\nb"] }).to_string(),
            serde_json::json!({ "question": "q", "timeout": 9999 }).to_string(),
        ] {
            assert_eq!(errno(ask.call(&call, bad.as_bytes())), libc::EINVAL);
        }
        assert!(broker::pending(&env).is_empty());
    }

    #[test]
    fn the_end_of_the_sandbox_withdraws_its_questions() {
        let dir = tempfile::tempdir().unwrap();
        let c = ctx(dir.path());
        let env = c.env.clone();
        let ask = Ask::new(c);
        let call = Call::for_test("asker", Duration::from_secs(60));
        assert_eq!(
            errno(ask.call(&call, br#"{"question": "q?", "timeout": 1}"#)),
            libc::ETIME
        );
        assert_eq!(broker::pending(&env).len(), 1);
        drop(ask);
        assert!(broker::pending(&env).is_empty());
    }
}
