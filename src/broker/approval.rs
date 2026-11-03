//! Approval cards, answers and the approver.

use super::notify;
use crate::env::HostEnv;
use crate::state;
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// A request that waits for the user, as `paddock approvals --json` shows it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pending {
    pub id: String,
    pub sandbox: String,
    pub harness: String,
    pub project: PathBuf,
    pub capability: String,
    /// One line for a list.
    pub summary: String,
    /// The exact thing that will happen, for example the full command line.
    pub detail: String,
    pub reason: String,
    pub created: u64,
    /// Unix time when the request stops waiting and times out.
    #[serde(default)]
    pub expires: u64,
    /// Whether "allow for this session" is offered.
    pub session_allowed: bool,
    /// For a request from a sandbox that another sandbox started: the
    /// sandboxes above it, the first one started by the user, the parent last.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub started_by: Vec<String>,
    /// For a question (`/dev/paddock/ask`): short answers that the card
    /// offers as buttons. The user can always type another answer. For a
    /// card of paddock itself (`spawn-timeout`): the only answers, besides
    /// deny.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<String>,
}

/// The capability name of a question from `/dev/paddock/ask` on the
/// approval card. It is not a host capability: the answer is text for the
/// agent, and nothing runs on the host.
pub const ASK_CAPABILITY: &str = "ask";
/// The capability name of the card for a spawned child at its time limit
/// (`[spawn] on_timeout = "ask"`). The answer is one of its choices (more
/// time), or deny (stop).
pub const TIMEOUT_CAPABILITY: &str = "spawn-timeout";
/// The capability name of the first-use card of a paid device. The answer
/// is one of its choices (the budget of the run), or deny.
pub const BUDGET_CAPABILITY: &str = "device-budget";
/// The longest typed answer to a question, in characters.
pub const MAX_REPLY: usize = 4000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Answer {
    Once,
    Session,
    Deny,
    /// Deny this request and every same request of this sandbox for the
    /// rest of its session, without a new card.
    #[serde(rename = "deny-session")]
    DenySession,
}

impl Answer {
    pub fn denies(self) -> bool {
        matches!(self, Self::Deny | Self::DenySession)
    }
}

/// The longest note for the agent on a denial, in characters.
pub const MAX_NOTE: usize = 300;

/// The note of a denial, made safe: one line, no control or invisible
/// characters, at most `MAX_NOTE` characters. None when nothing is left.
pub fn clean_note(s: &str) -> Option<String> {
    let line: String = s
        .chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .collect();
    let t: String = clean_text(&line).trim().chars().take(MAX_NOTE).collect();
    let t = t.trim_end().to_string();
    (!t.is_empty()).then_some(t)
}

/// The message of a denial for the agent: the user's note when there is
/// one, else `plain`.
pub fn denied_message(plain: &str, note: Option<&str>) -> String {
    match note {
        Some(n) => format!("the user denied this request: \"{n}\""),
        None => plain.to_string(),
    }
}

/// An answer with the user's note on a denial, and who gave it ("user" or
/// "agent").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub answer: Answer,
    pub note: Option<String>,
    pub who: &'static str,
}

/// The answer file: a plain answer (`"deny"`), or an answer with a note.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub(in crate::broker) enum AnswerFile {
    Plain(Answer),
    Noted {
        answer: Answer,
        #[serde(default)]
        note: String,
    },
}

impl AnswerFile {
    pub(in crate::broker) fn parse(text: &str) -> Option<(Answer, Option<String>)> {
        match serde_json::from_str::<AnswerFile>(text).ok()? {
            AnswerFile::Plain(a) => Some((a, None)),
            AnswerFile::Noted { answer, note } => {
                Some((answer, clean_note(&note).filter(|_| answer.denies())))
            }
        }
    }
}

impl std::str::FromStr for Answer {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "once" => Ok(Self::Once),
            "session" => Ok(Self::Session),
            "deny" => Ok(Self::Deny),
            "deny-session" => Ok(Self::DenySession),
            _ => bail!("answer must be once, session, deny or deny-session"),
        }
    }
}

pub fn approvals_dir(env: &HostEnv) -> PathBuf {
    env.paddock_run().join("approvals")
}

/// Every request that waits for an answer, from every running sandbox.
pub fn pending(env: &HostEnv) -> Vec<Pending> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(approvals_dir(env)) else {
        return out;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().is_some_and(|x| x == "json") {
            if let Some(req) = std::fs::read_to_string(&p)
                .ok()
                .and_then(|t| serde_json::from_str::<Pending>(&t).ok())
            {
                out.push(req);
            }
        }
    }
    // A question of a sandbox that no longer runs: its paddock process was
    // killed before it could withdraw the question. Older than a minute, so
    // a question that is being put on the card is never removed.
    let now = state::now();
    if out
        .iter()
        .any(|p| p.capability == ASK_CAPABILITY && now.saturating_sub(p.created) > 60)
    {
        let running: Vec<String> = state::Running::list(env)
            .into_iter()
            .map(|r| r.id)
            .collect();
        out.retain(|p| {
            let gone = p.capability == ASK_CAPABILITY
                && now.saturating_sub(p.created) > 60
                && !running.contains(&p.sandbox);
            if gone {
                let dir = approvals_dir(env);
                let _ = std::fs::remove_file(dir.join(format!("{}.json", p.id)));
                let _ = std::fs::remove_file(dir.join(format!("{}.answer", p.id)));
            }
            !gone
        });
    }
    // Host requests first: questions of agents never push them off the card.
    out.sort_by_key(|p| (p.capability == ASK_CAPABILITY, p.created));
    out
}

/// Record the user's answer. Only the host can call this: the approvals
/// directory is not mounted in any sandbox.
pub fn answer(env: &HostEnv, id: &str, a: Answer) -> Result<()> {
    answer_with_note(env, id, a, None)
}

/// Record the user's answer with a note for the agent. Only a denial
/// carries a note.
pub fn answer_with_note(env: &HostEnv, id: &str, a: Answer, note: Option<&str>) -> Result<()> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        bail!("invalid request id");
    }
    let dir = approvals_dir(env);
    let pending = dir.join(format!("{id}.json"));
    if !pending.is_file() {
        bail!("no pending request {id}");
    }
    let p: Pending = serde_json::from_str(&std::fs::read_to_string(&pending)?)?;
    if p.capability == ASK_CAPABILITY && a != Answer::Deny {
        bail!("{id} is a question: answer it with --text TEXT, or dismiss it with deny");
    }
    if p.capability == ASK_CAPABILITY && a == Answer::DenySession {
        bail!("{id} is a question: dismiss it with deny");
    }
    if p.capability != ASK_CAPABILITY && !p.choices.is_empty() && a != Answer::Deny {
        bail!(
            "{id} offers choices: answer it with --text {}, or deny",
            p.choices.join("|")
        );
    }
    if a == Answer::Session && !p.session_allowed {
        bail!("this request can only be allowed once");
    }
    if a == Answer::DenySession && !p.session_allowed {
        bail!("this request has no session answer: deny it with deny");
    }
    if note.is_some() && !a.denies() {
        bail!("only a denial carries a note");
    }
    let file = match note.and_then(clean_note) {
        Some(note) => AnswerFile::Noted { answer: a, note },
        None => AnswerFile::Plain(a),
    };
    write_answer(&dir, id, serde_json::to_string(&file)?)?;
    Ok(())
}

/// Write the answer file of `id` whole, at once, and only the first time:
/// a poller never reads half an answer, and a second answer does not
/// replace the first.
fn write_answer(dir: &Path, id: &str, text: String) -> Result<()> {
    match crate::privfs::write_new(&dir.join(format!("{id}.answer")), text) {
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            bail!("{id} was already answered")
        }
        r => Ok(r?),
    }
}

/// Answer a question from `/dev/paddock/ask` with text. The text goes to
/// the agent that asked; nothing runs on the host. Only the host can call
/// this, like `answer`.
pub fn answer_reply(env: &HostEnv, id: &str, text: &str) -> Result<()> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        bail!("invalid request id");
    }
    let dir = approvals_dir(env);
    let pending = dir.join(format!("{id}.json"));
    if !pending.is_file() {
        bail!("no pending request {id}");
    }
    let p: Pending = serde_json::from_str(&std::fs::read_to_string(&pending)?)?;
    // A card of paddock itself with fixed choices: only those.
    if p.capability != ASK_CAPABILITY && !p.choices.is_empty() {
        if !p.choices.iter().any(|c| c == text.trim()) {
            bail!(
                "{id} takes one of these answers: {}, or deny",
                p.choices.join(", ")
            );
        }
        write_answer(
            &dir,
            id,
            serde_json::json!({ "reply": text.trim() }).to_string(),
        )?;
        return Ok(());
    }
    if p.capability != ASK_CAPABILITY {
        bail!("{id} is a host request, not a question: answer it with once, session or deny");
    }
    if text.trim().is_empty() {
        bail!("the answer is empty");
    }
    if text.chars().count() > MAX_REPLY {
        bail!("the answer is longer than {MAX_REPLY} characters");
    }
    write_answer(&dir, id, serde_json::json!({ "reply": text }).to_string())?;
    Ok(())
}

/// The answer source. The real one waits for `paddock approvals answer` or a
/// notification action. Tests use a fixed answer.
pub trait Approver: Send + Sync {
    fn ask(&self, env: &HostEnv, p: &Pending, wait: Duration) -> Option<Answer>;

    /// The answer and who gave it ("user" or "agent"), for the audit log.
    fn ask_with_source(
        &self,
        env: &HostEnv,
        p: &Pending,
        wait: Duration,
    ) -> Option<(Answer, &'static str)> {
        self.ask(env, p, wait).map(|a| (a, "user"))
    }

    /// The answer with the user's note on a denial. Callers that tell the
    /// agent about a denial use this one.
    /// A card with fixed choices (`p.choices`): the chosen one, or None
    /// for deny or no answer.
    fn choose(&self, env: &HostEnv, p: &Pending, wait: Duration) -> Option<String> {
        match ask_choice(env, p, wait, &|| false) {
            crate::spawn::Choice::More(c) => Some(c),
            _ => None,
        }
    }

    fn ask_verdict(&self, env: &HostEnv, p: &Pending, wait: Duration) -> Option<Verdict> {
        self.ask_with_source(env, p, wait)
            .map(|(answer, who)| Verdict {
                answer,
                note: None,
                who,
            })
    }
}

/// Answers every request with deny. `paddock doctor --escape` uses it.
pub struct DenyAll;

impl Approver for DenyAll {
    fn ask(&self, _: &HostEnv, _: &Pending, _: Duration) -> Option<Answer> {
        Some(Answer::Deny)
    }
}

pub struct HostApprover;

/// Write a pending request for the panel and the CLI: 0600 in a 0700 folder.
/// Text from a sandbox, made safe to show: no control characters except
/// newline and tab, and no invisible format characters. A terminal would run
/// escape sequences (clear the screen, rewrite a line), and bidi overrides
/// can reverse text on a card or in a notification.
pub fn clean_text(s: &str) -> String {
    s.chars()
        .filter(|&c| {
            if c == '\n' || c == '\t' {
                return true;
            }
            !c.is_control()
                && !matches!(c,
                    '\u{061C}' | '\u{180E}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}'
                    | '\u{2060}'..='\u{206F}' | '\u{FEFF}' | '\u{FFF9}'..='\u{FFFB}')
        })
        .collect()
}

impl Pending {
    /// The same request with every text that a sandbox can choose cleaned.
    pub fn cleaned(&self) -> Self {
        let mut p = self.clone();
        p.summary = clean_text(&p.summary);
        p.detail = clean_text(&p.detail);
        p.reason = clean_text(&p.reason);
        p.harness = clean_text(&p.harness);
        p.choices = p.choices.iter().map(|c| clean_text(c)).collect();
        p.started_by = p.started_by.iter().map(|c| clean_text(c)).collect();
        p
    }
}

pub fn write_pending(env: &HostEnv, p: &Pending) -> std::io::Result<PathBuf> {
    let p = &p.cleaned();
    let pending = approvals_dir(env).join(format!("{}.json", p.id));
    crate::privfs::write(&pending, serde_json::to_string(p).unwrap_or_default())?;
    Ok(pending)
}

impl Approver for HostApprover {
    fn ask(&self, env: &HostEnv, p: &Pending, wait: Duration) -> Option<Answer> {
        self.ask_verdict(env, p, wait).map(|v| v.answer)
    }

    fn ask_with_source(
        &self,
        env: &HostEnv,
        p: &Pending,
        wait: Duration,
    ) -> Option<(Answer, &'static str)> {
        self.ask_verdict(env, p, wait).map(|v| (v.answer, v.who))
    }

    fn ask_verdict(&self, env: &HostEnv, p: &Pending, wait: Duration) -> Option<Verdict> {
        let p = &p.cleaned();
        let answer_file = approvals_dir(env).join(format!("{}.answer", p.id));
        let Ok(pending) = write_pending(env, p) else {
            return None;
        };
        let shown = notify_with_actions(env, p);
        let start = Instant::now();
        let mut got = None;
        while start.elapsed() < wait {
            if let Ok(t) = std::fs::read_to_string(&answer_file) {
                got = AnswerFile::parse(&t).map(|(answer, note)| Verdict {
                    answer,
                    note,
                    who: "user",
                });
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        // Answered elsewhere or timed out: the buttons mean nothing now.
        if let Some(h) = shown {
            h.close();
        }
        let _ = std::fs::remove_file(&pending);
        let _ = std::fs::remove_file(&answer_file);
        got
    }
}

/// Put a card with fixed choices up (`TIMEOUT_CAPABILITY`) and wait for
/// the answer: one of `p.choices`, or deny. Stop waiting after `wait` or
/// when `cancelled` turns true.
pub fn ask_choice(
    env: &HostEnv,
    p: &Pending,
    wait: Duration,
    cancelled: &dyn Fn() -> bool,
) -> crate::spawn::Choice {
    use crate::spawn::Choice;
    let p = &p.cleaned();
    let answer_file = approvals_dir(env).join(format!("{}.answer", p.id));
    let Ok(pending) = write_pending(env, p) else {
        return Choice::NoAnswer;
    };
    let shown = notify_choices(env, p);
    let start = Instant::now();
    let mut got = Choice::NoAnswer;
    while start.elapsed() < wait && !cancelled() {
        if let Ok(t) = std::fs::read_to_string(&answer_file) {
            let reply = serde_json::from_str::<serde_json::Value>(&t)
                .ok()
                .and_then(|v| v["reply"].as_str().map(str::to_string));
            got = match reply {
                Some(r) if p.choices.contains(&r) => Choice::More(r),
                _ => Choice::Stop,
            };
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    if let Some(h) = shown {
        h.close();
    }
    let _ = std::fs::remove_file(&pending);
    let _ = std::fs::remove_file(&answer_file);
    got
}

/// The notification for a card with fixed choices, when the bar does not
/// run: one button per choice, and Stop.
fn notify_choices(env: &HostEnv, p: &Pending) -> Option<notify::Handle> {
    if approval_ui_active(env) {
        return None;
    }
    let mut actions: Vec<(String, String)> = p
        .choices
        .iter()
        .enumerate()
        .map(|(i, c)| (format!("choice:{i}"), format!("+{c}")))
        .collect();
    actions.push(("deny".into(), "Stop".into()));
    let note = notify::Note {
        summary: format!("paddock: {}", p.summary),
        body: p.detail.clone(),
        actions,
        critical: true,
    };
    let (env2, id, choices) = (env.clone(), p.id.clone(), p.choices.clone());
    let on_action = move |key: &str| {
        if let Some(c) = key
            .strip_prefix("choice:")
            .and_then(|i| i.parse::<usize>().ok())
            .and_then(|i| choices.get(i))
        {
            let _ = answer_reply(&env2, &id, c);
        } else if key == "deny" {
            let _ = answer(&env2, &id, Answer::Deny);
        }
    };
    match notify::ask(env, &note, on_action) {
        Ok(h) => Some(h),
        Err(e) => {
            log::debug!("cannot show the card as a notification: {e:#}");
            None
        }
    }
}

/// A desktop notification with buttons. Its answer goes through the same
/// answer file, so the panel, the CLI and the notification never disagree.
/// The bar plugin's approval service writes the Unix time to this file every
/// few seconds while it runs. Then it shows the request itself, and a second
/// popup would only repeat it.
pub fn approval_ui_file(env: &HostEnv) -> PathBuf {
    env.paddock_run().join("approval-ui")
}

pub fn approval_ui_active(env: &HostEnv) -> bool {
    std::fs::read_to_string(approval_ui_file(env))
        .ok()
        .and_then(|t| t.trim().parse::<u64>().ok())
        .is_some_and(|t| state::now().saturating_sub(t) < 15)
}

/// The fallback prompt when the bar does not run. The request detail can
/// hold clipboard text, so it goes over the user's session bus, never into
/// the arguments of a process, which every local user can read.
pub fn approval_note(p: &Pending) -> notify::Note {
    let mut actions = vec![("once".to_string(), "Allow once".to_string())];
    if p.session_allowed {
        actions.push(("session".into(), "Allow for this session".into()));
    }
    actions.push(("deny".into(), "Deny".into()));
    if p.session_allowed {
        actions.push(("deny-session".into(), "Deny for this session".into()));
    }
    notify::Note {
        summary: format!("paddock: {} asks for {}", p.harness, p.capability),
        body: {
            let mut b = match crate::spawn::chain_text(&p.started_by) {
                Some(c) => format!("{c}\n\n{}", p.detail),
                None => p.detail.clone(),
            };
            if !p.reason.is_empty() {
                b.push_str(&format!("\n\nReason: {}", p.reason));
            }
            b
        },
        actions,
        critical: true,
    }
}

fn notify_with_actions(env: &HostEnv, p: &Pending) -> Option<notify::Handle> {
    if approval_ui_active(env) {
        return None;
    }
    let (env2, id) = (env.clone(), p.id.clone());
    let on_action = move |key: &str| {
        if let Ok(a) = key.parse::<Answer>() {
            let _ = answer(&env2, &id, a);
        }
    };
    match notify::ask(env, &approval_note(p), on_action) {
        Ok(h) => Some(h),
        Err(e) => {
            log::debug!("cannot show the approval notification: {e:#}");
            None
        }
    }
}
