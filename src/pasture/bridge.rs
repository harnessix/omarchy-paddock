//! The herd bridge: a member of a pasture asks another member a question,
//! and that member answers with one run of its own harness, in its own
//! sandbox, with its own login.
//!
//! The way of a question:
//!
//! 1. The asker sends `ask` to its own kv socket. Its paddock process
//!    checks `bridge = true` of the pasture, finds the target among the
//!    running members, and writes the question into the spool of the
//!    target: `<pasture folder>/bridge/ask/<target>/`. The spool is outside
//!    the shared folder (`ground`), so no sandbox reaches it.
//! 2. The helper of the target (`paddock-host pasture serve`) waits on its
//!    own kv socket (`ask` with action `next`). The paddock process of the
//!    target hands it the question and the `[oneshot]` command of its
//!    profile. The helper runs that command once, with the prompt on stdin
//!    or in a private file of its home, and sends the output back (`ask`
//!    with action `answer`).
//! 3. The paddock process of the asker reads the answer and compares it
//!    with the login secrets of every member (`Guard`). A hit replaces the
//!    whole answer with a refusal.
//!
//! The audit log gets who asked whom, the sizes, the time and the result.
//! It never gets the text.

use crate::env::HostEnv;
use crate::kv::{Identity, Request, Response, Status};
use crate::profile::PromptVia;
use crate::state::Running;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::io::{Read as _, Write as _};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The largest answer. The helper keeps the end of a longer output.
pub const MAX_ANSWER: usize = 256 * 1024;
/// The largest question.
pub const MAX_PROMPT: usize = 256 * 1024;
/// Seconds that the asker waits by default.
pub const DEFAULT_TIMEOUT: u64 = 300;
/// The shortest piece of a secret that counts as a hit, in characters.
pub const MIN_MATCH: usize = 16;
/// The prompt file of `prompt = "file"`, relative to the home of the target.
pub const PROMPT_IN_HOME: &str = ".paddock-bridge/prompt.md";
/// How long the helper of the target has to take a question.
pub const PICKUP: Duration = Duration::from_secs(20);
/// The longest wait of one `next`.
const MAX_NEXT: u64 = 30;
/// The largest login file that the guard reads.
const MAX_LOGIN_FILE: u64 = 1 << 20;

/// The one-shot command of a member, as its paddock process knows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Command {
    /// The executable and its arguments, with the prompt file filled in.
    /// The prompt itself is never an argument.
    pub argv: Vec<String>,
    pub via: PromptVia,
    /// The prompt file in the sandbox, for `prompt = "file"`.
    pub prompt_file: String,
}

impl Command {
    /// The bridge command of a profile: the harness, its privacy flags and
    /// the `[oneshot]` arguments. `home` is the home in the sandbox.
    pub fn of(exe: &Path, privacy: Vec<String>, o: &crate::profile::OneShot, home: &Path) -> Self {
        let file = home.join(PROMPT_IN_HOME);
        let mut argv = vec![exe.display().to_string()];
        argv.extend(privacy);
        argv.extend(o.render(&file));
        Self {
            argv,
            via: o.prompt,
            prompt_file: file.display().to_string(),
        }
    }
}

/// A question in the spool of the target.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Question {
    pub id: String,
    pub from: String,
    pub from_harness: String,
    pub prompt: String,
    pub timeout: u64,
}

/// The answer of the target's helper.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Answer {
    pub id: String,
    pub text: String,
    #[serde(default)]
    pub exit_code: Option<i32>,
    /// The output was longer than [`MAX_ANSWER`]: `text` is its end.
    #[serde(default)]
    pub truncated: bool,
    #[serde(default)]
    pub error: Option<String>,
}

/// What the paddock process of a member records for the bridge, in the
/// pasture folder outside every sandbox. No secret value: the delivered
/// credentials are there as hashes of their pieces only.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemberRecord {
    pub harness: String,
    /// The home of the sandbox on the host.
    pub home: PathBuf,
    /// The login files of the profile (`state.auth`), relative to `home`.
    pub login: Vec<String>,
    /// The host login files that the sandbox holds a copy of.
    #[serde(default)]
    pub stores: Vec<PathBuf>,
    /// Delivered credentials: name and the marks of the value.
    #[serde(default)]
    pub delivered: BTreeMap<String, Vec<u64>>,
    /// Whether the profile has a `[oneshot]` command.
    pub oneshot: bool,
}

// ---- places ------------------------------------------------------------

/// The bridge folder of a pasture on the host.
pub fn dir(env: &HostEnv, project: &Path, pasture: &str) -> PathBuf {
    super::folder(env, project, pasture).join("bridge")
}

fn spool(base: &Path, target: &str) -> PathBuf {
    base.join("ask").join(target)
}

fn member_file(base: &Path, id: &str) -> PathBuf {
    base.join("members").join(format!("{id}.json"))
}

/// Record a member at its start.
pub fn record_member(base: &Path, id: &str, rec: &MemberRecord) -> anyhow::Result<()> {
    crate::kv::shard::check_id(id)?;
    let f = member_file(base, id);
    if let Some(d) = f.parent() {
        crate::privfs::dir(d)?;
    }
    crate::privfs::write_atomic(&f, serde_json::to_vec(rec)?)?;
    Ok(())
}

/// Remove the record and the spool of a member at its end.
pub fn forget_member(base: &Path, id: &str) {
    if crate::kv::shard::check_id(id).is_err() {
        return;
    }
    let _ = std::fs::remove_file(member_file(base, id));
    let _ = std::fs::remove_dir_all(spool(base, id));
}

fn read_record(base: &Path, id: &str) -> Option<MemberRecord> {
    crate::kv::shard::check_id(id).ok()?;
    serde_json::from_slice(&std::fs::read(member_file(base, id)).ok()?).ok()
}

// ---- the guard ---------------------------------------------------------

/// The text that the guard compares: no whitespace (and no zero-width
/// characters), lowercase.
pub fn normalize(s: &str) -> Vec<char> {
    s.chars()
        .filter(|c| {
            !c.is_whitespace() && !matches!(c, '\u{200b}'..='\u{200d}' | '\u{2060}' | '\u{feff}')
        })
        .flat_map(char::to_lowercase)
        .collect()
}

fn mark_of(w: &[char]) -> u64 {
    use sha2::{Digest, Sha256};
    let s: String = w.iter().collect();
    let h = Sha256::digest(s.as_bytes());
    u64::from_le_bytes(h[..8].try_into().unwrap_or_default())
}

/// The marks of a secret: a hash of each piece of [`MIN_MATCH`] characters
/// of its normalized text. A secret shorter than that has none.
pub fn marks(secret: &str) -> Vec<u64> {
    let n = normalize(secret);
    if n.len() < MIN_MATCH {
        return Vec::new();
    }
    n.windows(MIN_MATCH).map(mark_of).collect()
}

/// Whether a word from a login file looks like a secret value, not like a
/// name, a date, a path, an address or a URL.
pub fn looks_secret(s: &str) -> bool {
    let n = s.chars().count();
    let b = s.as_bytes();
    let date = b.len() >= 10 && b[..4].iter().all(u8::is_ascii_digit) && b[4] == b'-';
    let digit = s.chars().any(|c| c.is_ascii_digit());
    let mixed =
        s.chars().any(|c| c.is_ascii_uppercase()) && s.chars().any(|c| c.is_ascii_lowercase());
    n >= MIN_MATCH
        && !s.contains("://")
        && !s.contains('@')
        && !s.starts_with('/')
        && !s.starts_with('~')
        && !date
        && s.chars().any(|c| c.is_ascii_alphabetic())
        && (digit || (mixed && n >= 24))
}

/// The secret values in the text of a login file: the string values of a
/// JSON file, or the words of another file, that look like secrets.
pub fn tokens(text: &str) -> Vec<String> {
    fn strings(v: &serde_json::Value, out: &mut Vec<String>) {
        match v {
            serde_json::Value::String(s) => out.push(s.clone()),
            serde_json::Value::Array(a) => a.iter().for_each(|v| strings(v, out)),
            serde_json::Value::Object(o) => o.values().for_each(|v| strings(v, out)),
            _ => {}
        }
    }
    let mut raw = Vec::new();
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(v) => strings(&v, &mut raw),
        Err(_) => raw.push(text.to_string()),
    }
    let mut out: Vec<String> = raw
        .iter()
        .flat_map(|s| {
            s.split(|c: char| c.is_whitespace() || "\"',;=<>()[]{}".contains(c))
                .filter(|w| looks_secret(w))
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

/// The login secrets of a pasture, as marks with the name of their source.
#[derive(Debug, Default)]
pub struct Guard {
    marks: HashMap<u64, usize>,
    names: Vec<String>,
}

impl Guard {
    fn name(&mut self, name: &str) -> usize {
        match self.names.iter().position(|n| n == name) {
            Some(i) => i,
            None => {
                self.names.push(name.to_string());
                self.names.len() - 1
            }
        }
    }

    /// Add a secret value.
    pub fn add(&mut self, name: &str, secret: &str) {
        let m = marks(secret);
        self.add_marks(name, &m);
    }

    pub fn add_marks(&mut self, name: &str, marks: &[u64]) {
        if marks.is_empty() {
            return;
        }
        let i = self.name(name);
        for m in marks {
            self.marks.entry(*m).or_insert(i);
        }
    }

    /// Add the secrets of a login file.
    pub fn add_file(&mut self, name: &str, text: &str) {
        for t in tokens(text) {
            self.add(name, &t);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.marks.is_empty()
    }

    /// The names of the secrets that `text` holds a piece of.
    pub fn find(&self, text: &str) -> Vec<String> {
        let n = normalize(text);
        let mut hits: Vec<usize> = Vec::new();
        if n.len() >= MIN_MATCH {
            for w in n.windows(MIN_MATCH) {
                if let Some(i) = self.marks.get(&mark_of(w)) {
                    if !hits.contains(i) {
                        hits.push(*i);
                    }
                }
            }
        }
        hits.sort();
        hits.into_iter().map(|i| self.names[i].clone()).collect()
    }

    /// The login secrets that paddock knows for the members of a pasture:
    /// the login files of their profiles in their homes, the host login
    /// files they hold a copy of, and their delivered credentials.
    pub fn of_members(base: &Path, members: &[Running]) -> Self {
        let mut g = Guard::default();
        for r in members {
            let Some(rec) = read_record(base, &r.id) else {
                continue;
            };
            let who = format!("{} (sandbox {})", r.harness, r.id);
            if let Ok(root) = crate::safefs::Root::open(&rec.home) {
                for rel in &rec.login {
                    let name = format!("the login file {rel} of {who}");
                    match rel.strip_suffix('/') {
                        Some(d) => {
                            let Ok(dir) = root.dir(Path::new(d), false) else {
                                continue;
                            };
                            for f in dir.file_names().unwrap_or_default().into_iter().take(64) {
                                if let Ok(Some(b)) = dir.read(&f, MAX_LOGIN_FILE) {
                                    g.add_file(&name, &String::from_utf8_lossy(&b));
                                }
                            }
                        }
                        None => {
                            if let Ok(Some(b)) = root.read(Path::new(rel), MAX_LOGIN_FILE) {
                                g.add_file(&name, &String::from_utf8_lossy(&b));
                            }
                        }
                    }
                }
            }
            let stores = rec
                .stores
                .iter()
                .chain(r.logins.iter().map(|l| &l.store))
                .chain(r.kept_logins.iter().map(|l| &l.store));
            for s in stores {
                if let Ok(Some(b)) = crate::safefs::read_path(s, true, MAX_LOGIN_FILE) {
                    g.add_file(
                        &format!("the host login of {who}"),
                        &String::from_utf8_lossy(&b),
                    );
                }
            }
            for (name, m) in &rec.delivered {
                g.add_marks(&format!("the credential {name} of {who}"), m);
            }
        }
        g
    }
}

// ---- routing -------------------------------------------------------------

/// The running members of a pasture, the ones that the user started.
pub fn members(env: &HostEnv, project: &Path, pasture: &str) -> Vec<Running> {
    Running::list(env)
        .into_iter()
        .filter(|r| {
            r.parent.is_none()
                && r.pasture.as_deref() == Some(pasture)
                && r.kv_project.as_deref() == Some(project)
        })
        .collect()
}

/// The member that `target` names: a sandbox id, or a harness that only
/// one other member runs.
pub fn route<'a>(
    members: &'a [Running],
    me: &str,
    target: &str,
) -> Result<&'a Running, (Status, String)> {
    let others: Vec<&Running> = members.iter().filter(|r| r.id != me).collect();
    let list = || {
        if others.is_empty() {
            "the pasture has no other running member".to_string()
        } else {
            let l: Vec<String> = others
                .iter()
                .map(|r| format!("{} ({})", r.harness, r.id))
                .collect();
            format!("the other members: {}", l.join(", "))
        }
    };
    if target == me {
        return Err((Status::Invalid, "a member cannot ask itself".into()));
    }
    if let Some(r) = others.iter().find(|r| r.id == target) {
        return Ok(r);
    }
    let same: Vec<&&Running> = others.iter().filter(|r| r.harness == target).collect();
    match same.as_slice() {
        [one] => Ok(one),
        [] => Err((
            Status::NotFound,
            format!("no running member is '{target}'; {}", list()),
        )),
        many => {
            let ids: Vec<&str> = many.iter().map(|r| r.id.as_str()).collect();
            Err((
                Status::Invalid,
                format!(
                    "{} members run {target}: name one by its sandbox id ({})",
                    many.len(),
                    ids.join(", ")
                ),
            ))
        }
    }
}

/// The end of `bytes`, at most `max` long, and whether it was longer.
pub fn tail(bytes: &[u8], max: usize) -> (String, bool) {
    if bytes.len() <= max {
        return (String::from_utf8_lossy(bytes).into_owned(), false);
    }
    let mut cut = &bytes[bytes.len() - max..];
    // Start at a character.
    while let [b, rest @ ..] = cut {
        if (b & 0xC0) != 0x80 {
            break;
        }
        cut = rest;
    }
    (String::from_utf8_lossy(cut).into_owned(), true)
}

// ---- the host side -------------------------------------------------------

fn resp(status: Status, msg: impl Into<String>) -> Response {
    Response {
        status,
        message: msg.into(),
        result: serde_json::Value::Null,
    }
}

/// Whether the bridge of the pasture is on in the user configuration.
pub fn enabled(env: &HostEnv, pasture: &str) -> bool {
    super::load(env)
        .ok()
        .and_then(|all| all.get(pasture).map(|p| p.bridge))
        .unwrap_or(false)
}

/// The longest timeout: `[spawn] timeout` of the user configuration.
pub fn max_timeout(env: &HostEnv) -> u64 {
    crate::spawn::SpawnCfg::load(env)
        .map(|c| c.timeout)
        .unwrap_or(crate::spawn::DEFAULT_TIMEOUT)
}

/// Take the lock of a spool without waiting. It holds until the file closes.
fn try_lock(path: &Path) -> Option<std::fs::File> {
    let f = crate::privfs::append(path).ok()?;
    let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    (rc == 0).then_some(f)
}

fn clear(sp: &Path) {
    for f in ["question.json", "taken.json", "answer.json"] {
        let _ = std::fs::remove_file(sp.join(f));
    }
}

fn audit(env: &HostEnv, id: &Identity, line: serde_json::Value) {
    let mut line = line;
    line["time"] = crate::state::now().into();
    line["sandbox"] = id.sandbox.clone().into();
    line["harness"] = id.harness.clone().into();
    line["project"] = serde_json::to_value(&id.project).unwrap_or_default();
    line["op"] = "ask".into();
    line["scope"] = "pasture".into();
    let f = crate::kv::audit_file(env);
    match crate::privfs::append(&f) {
        Ok(mut file) => {
            let _ = writeln!(file, "{line}");
        }
        Err(e) => log::warn!("cannot open the kv audit log {}: {e}", f.display()),
    }
}

fn new_id() -> String {
    use sha2::{Digest, Sha256};
    let seed = format!("{:?}-{}-ask", Instant::now(), std::process::id());
    let h = crate::manifest::hex(&Sha256::digest(seed.as_bytes()));
    format!("q-{}-{}", crate::state::now(), &h[..8])
}

/// `ask` from the asker: route the question, wait for the answer, check it.
pub fn ask(env: &HostEnv, id: &Identity, req: &Request) -> Response {
    let target = req.key.as_deref().unwrap_or("");
    let prompt = req.value.as_deref().unwrap_or("");
    let start = Instant::now();
    let mut line = serde_json::json!({
        "target": target,
        "prompt_bytes": prompt.len(),
    });
    let r = ask_inner(env, id, req, &mut line);
    line["status"] = serde_json::to_value(r.status).unwrap_or_default();
    line["seconds"] = start.elapsed().as_secs().into();
    audit(env, id, line);
    r
}

fn ask_inner(
    env: &HostEnv,
    id: &Identity,
    req: &Request,
    line: &mut serde_json::Value,
) -> Response {
    let (Some(pasture), false) = (id.pasture.as_deref(), id.is_child()) else {
        return resp(
            Status::Disabled,
            "this sandbox is in no pasture: the user starts members with --pasture NAME",
        );
    };
    if !enabled(env, pasture) {
        line["result"] = "bridge off".into();
        return resp(
            Status::Denied,
            format!(
                "the bridge of the pasture '{pasture}' is off: the user turns it on with bridge = true in [pastures.{pasture}] of the user configuration"
            ),
        );
    }
    let target = req.key.as_deref().unwrap_or("");
    if target.is_empty() {
        return resp(
            Status::Invalid,
            "ask needs a member: a harness or a sandbox id",
        );
    }
    let prompt = req.value.as_deref().unwrap_or("");
    if prompt.trim().is_empty() {
        return resp(Status::Invalid, "the question is empty");
    }
    if prompt.len() > MAX_PROMPT {
        return resp(
            Status::TooLarge,
            format!(
                "the question has {} bytes; the limit is {MAX_PROMPT}",
                prompt.len()
            ),
        );
    }
    let limit = max_timeout(env);
    let timeout = req.timeout.unwrap_or(DEFAULT_TIMEOUT.min(limit));
    if timeout == 0 || timeout > limit {
        return resp(
            Status::Invalid,
            format!("the timeout must be 1 to {limit} seconds ([spawn] timeout of the user configuration)"),
        );
    }
    let all = members(env, &id.project, pasture);
    let t = match route(&all, &id.sandbox, target) {
        Ok(t) => t.clone(),
        Err((s, m)) => return resp(s, m),
    };
    line["target"] = t.id.clone().into();
    line["target_harness"] = t.harness.clone().into();
    let who = format!("{} (sandbox {})", t.harness, t.id);
    if crate::freeze::frozen(env, &t.id).is_some() {
        line["result"] = "frozen".into();
        return resp(
            Status::Conflict,
            format!("{who} is frozen; it answers after the user resumes it"),
        );
    }
    if let Some(r) = crate::pasture::ration_status(env, &t.id).filter(|r| r.over) {
        line["result"] = "ration over".into();
        return resp(
            Status::Denied,
            format!(
                "{who} used up its ration ({}); it answers no more questions until the user extends it",
                crate::timer::describe(&r)
            ),
        );
    }
    let base = dir(env, &id.project, pasture);
    match read_record(&base, &t.id) {
        None => {
            return resp(
                Status::Failed,
                format!(
                "{who} has no bridge: it started with an older paddock, or it is still starting"
            ),
            )
        }
        Some(r) if !r.oneshot => {
            return resp(
                Status::Disabled,
                format!("{who} cannot answer: its harness profile has no [oneshot] command"),
            )
        }
        Some(_) => {}
    }
    let sp = spool(&base, &t.id);
    if let Err(e) = crate::privfs::dir(&sp) {
        return resp(Status::Failed, format!("{}: {e}", sp.display()));
    }
    let Some(_lock) = try_lock(&sp.join("lock")) else {
        line["result"] = "busy".into();
        return resp(
            Status::Conflict,
            format!("{who} answers another question now; ask again later"),
        );
    };
    clear(&sp);
    let q = Question {
        id: new_id(),
        from: id.sandbox.clone(),
        from_harness: id.harness.clone(),
        prompt: prompt.to_string(),
        timeout,
    };
    let bytes = serde_json::to_vec(&q).unwrap_or_default();
    if let Err(e) = crate::privfs::write_atomic(&sp.join("question.json"), bytes) {
        return resp(Status::Failed, format!("cannot write the question: {e}"));
    }
    let start = Instant::now();
    // The target's helper enforces the timeout too; a few seconds more here
    // let its answer arrive.
    let deadline = Duration::from_secs(timeout + 5);
    let answer = loop {
        if let Ok(b) = std::fs::read(sp.join("answer.json")) {
            if let Ok(a) = serde_json::from_slice::<Answer>(&b) {
                if a.id == q.id {
                    break a;
                }
            }
        }
        let waiting = sp.join("question.json").exists();
        let err = if start.elapsed() > deadline {
            Some((
                Status::Timeout,
                format!("{who} did not answer within {timeout} seconds"),
                "timeout",
            ))
        } else if waiting && start.elapsed() > PICKUP {
            Some((
                Status::Failed,
                format!("{who} did not take the question: its pasture helper does not run"),
                "not taken",
            ))
        } else if !crate::state::alive(t.pid) {
            Some((Status::NotFound, format!("{who} ended"), "gone"))
        } else {
            None
        };
        if let Some((s, m, r)) = err {
            clear(&sp);
            line["result"] = r.into();
            return resp(s, m);
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    clear(&sp);
    line["answer_bytes"] = answer.text.len().into();
    // The login guard: every member, the asker too.
    let guard = Guard::of_members(&base, &all);
    let hits = guard.find(&answer.text);
    if !hits.is_empty() {
        line["result"] = "intercepted".into();
        line["intercepted"] = hits.clone().into();
        return Response {
            status: Status::Denied,
            message: format!(
                "paddock withheld the answer of {who}: it held a piece of a login secret of the pasture"
            ),
            result: serde_json::json!({ "intercepted": true, "target": t.id, "harness": t.harness }),
        };
    }
    let failed = answer.error.is_some() || answer.exit_code.is_some_and(|c| c != 0);
    line["result"] = if failed { "failed" } else { "answered" }.into();
    Response {
        status: if failed { Status::Failed } else { Status::Done },
        message: answer
            .error
            .clone()
            .or_else(|| {
                failed.then(|| {
                    format!(
                        "the harness of {who} exited with {}",
                        answer.exit_code.unwrap_or(-1)
                    )
                })
            })
            .unwrap_or_default(),
        result: serde_json::json!({
            "answer": answer.text,
            "target": t.id,
            "harness": t.harness,
            "exit_code": answer.exit_code,
            "truncated": answer.truncated,
            "seconds": start.elapsed().as_secs(),
            "untrusted": true,
        }),
    }
}

/// `ask` with action `next`, from the target's own helper: wait for a
/// question, take it, and hand it over with the one-shot command.
pub fn next(env: &HostEnv, id: &Identity, cmd: Option<&Command>, req: &Request) -> Response {
    let Some(pasture) = id.pasture.as_deref().filter(|_| !id.is_child()) else {
        return resp(Status::Disabled, "this sandbox is in no pasture");
    };
    let Some(cmd) = cmd else {
        return resp(
            Status::Disabled,
            "the harness profile of this sandbox has no [oneshot] command",
        );
    };
    let sp = spool(&dir(env, &id.project, pasture), &id.sandbox);
    let wait = Duration::from_secs(req.timeout.unwrap_or(10).clamp(1, MAX_NEXT));
    let start = Instant::now();
    loop {
        let q = sp.join("question.json");
        if let Ok(b) = std::fs::read(&q) {
            if std::fs::rename(&q, sp.join("taken.json")).is_ok() {
                if let Ok(q) = serde_json::from_slice::<Question>(&b) {
                    return Response {
                        status: Status::Done,
                        message: String::new(),
                        result: serde_json::json!({
                            "question": q,
                            "command": cmd,
                        }),
                    };
                }
            }
        }
        if start.elapsed() >= wait {
            return resp(Status::Waiting, "no question");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// `ask` with action `answer`, from the target's own helper.
pub fn answer(env: &HostEnv, id: &Identity, req: &Request) -> Response {
    let Some(pasture) = id.pasture.as_deref().filter(|_| !id.is_child()) else {
        return resp(Status::Disabled, "this sandbox is in no pasture");
    };
    let mut a: Answer = match serde_json::from_str(req.value.as_deref().unwrap_or("")) {
        Ok(a) => a,
        Err(e) => return resp(Status::Invalid, format!("bad answer: {e}")),
    };
    let sp = spool(&dir(env, &id.project, pasture), &id.sandbox);
    let taken: Option<Question> = std::fs::read(sp.join("taken.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok());
    if taken.is_none_or(|q| q.id != a.id) {
        return resp(
            Status::NotFound,
            "the asker stopped waiting for this answer",
        );
    }
    if a.text.len() > MAX_ANSWER {
        let (t, _) = tail(a.text.as_bytes(), MAX_ANSWER);
        a.text = t;
        a.truncated = true;
    }
    let bytes = serde_json::to_vec(&a).unwrap_or_default();
    if let Err(e) = crate::privfs::write_atomic(&sp.join("answer.json"), bytes) {
        return resp(Status::Failed, format!("cannot write the answer: {e}"));
    }
    let _ = std::fs::remove_file(sp.join("taken.json"));
    resp(Status::Done, "")
}

// ---- the helper in the target --------------------------------------------

fn kv(action: &str, f: impl FnOnce(&mut Request)) -> Result<Response, String> {
    let mut r = Request {
        v: crate::kv::PROTOCOL,
        op: "ask".into(),
        action: Some(action.into()),
        ..Default::default()
    };
    f(&mut r);
    crate::hostcli::kv::send(&r)
}

/// Run the one-shot command once for a question.
pub fn run(cmd: &Command, q: &Question) -> Answer {
    let mut a = Answer {
        id: q.id.clone(),
        ..Default::default()
    };
    if let Err(e) = run_into(cmd, q, &mut a) {
        a.error = Some(e);
    }
    if cmd.via == PromptVia::File {
        let _ = std::fs::remove_file(&cmd.prompt_file);
    }
    a
}

fn run_into(cmd: &Command, q: &Question, a: &mut Answer) -> Result<(), String> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    let Some((exe, args)) = cmd.argv.split_first() else {
        return Err("the one-shot command is empty".into());
    };
    if cmd.via == PromptVia::File {
        let f = Path::new(&cmd.prompt_file);
        if let Some(d) = f.parent() {
            std::fs::DirBuilder::new()
                .mode(0o700)
                .recursive(true)
                .create(d)
                .map_err(|e| format!("{}: {e}", d.display()))?;
        }
        let _ = std::fs::remove_file(f);
        let mut w = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(f)
            .map_err(|e| format!("{}: {e}", f.display()))?;
        w.write_all(q.prompt.as_bytes())
            .map_err(|e| e.to_string())?;
    }
    let mut child = std::process::Command::new(exe)
        .args(args)
        .stdin(if cmd.via == PromptVia::Stdin {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|e| format!("cannot start {exe}: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let p = q.prompt.clone();
        std::thread::spawn(move || {
            let _ = stdin.write_all(p.as_bytes());
        });
    }
    let mut out = child.stdout.take().ok_or("no stdout")?;
    let reader = std::thread::spawn(move || {
        // Keep the end: the final text of a harness comes last.
        let mut keep: Vec<u8> = Vec::new();
        let mut longer = false;
        let mut buf = [0u8; 16 * 1024];
        loop {
            match out.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    keep.extend_from_slice(&buf[..n]);
                    if keep.len() > 2 * MAX_ANSWER {
                        keep.drain(..keep.len() - MAX_ANSWER - 1);
                        longer = true;
                    }
                }
            }
        }
        (keep, longer)
    });
    let deadline = Duration::from_secs(q.timeout.max(1));
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if start.elapsed() > deadline => {
                unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err(e.to_string()),
        }
    };
    let (bytes, longer) = reader.join().unwrap_or_default();
    let (text, cut) = tail(&bytes, MAX_ANSWER);
    a.text = text;
    a.truncated = cut || longer;
    match status {
        Some(s) => a.exit_code = s.code(),
        None => {
            return Err(format!(
                "the harness did not finish within {} seconds",
                q.timeout
            ))
        }
    }
    Ok(())
}

/// The bridge part of the member's helper: wait for questions on the own
/// kv socket and answer each with one run. Returns when the kv socket goes
/// away.
pub fn helper() {
    let mut fails = 0;
    loop {
        let r = match kv("next", |r| r.timeout = Some(10)) {
            Ok(r) => {
                fails = 0;
                r
            }
            Err(_) => {
                fails += 1;
                if fails >= 6 {
                    return;
                }
                std::thread::sleep(Duration::from_secs(2));
                continue;
            }
        };
        match r.status {
            Status::Done => {}
            Status::Waiting => continue,
            // Off, or an older host: look again later.
            _ => {
                std::thread::sleep(Duration::from_secs(60));
                continue;
            }
        }
        let (Ok(q), Ok(cmd)) = (
            serde_json::from_value::<Question>(r.result["question"].clone()),
            serde_json::from_value::<Command>(r.result["command"].clone()),
        ) else {
            continue;
        };
        let a = run(&cmd, &q);
        let value = serde_json::to_string(&a).unwrap_or_default();
        let _ = kv("answer", |r| {
            r.key = Some(q.id.clone());
            r.value = Some(value);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "sk-test-Ab3dEf9hIj2kLm5nOp8qRs";

    #[test]
    fn the_guard_finds_a_token_in_the_answer() {
        let mut g = Guard::default();
        g.add_file(
            "the login file .x/auth.json of fake",
            &format!(r#"{{"tokens":{{"access_token":"{TOKEN}","expires_at":"2026-11-02T10:00:00Z"}},"email":"someone@example.org"}}"#),
        );
        assert!(!g.is_empty());
        assert_eq!(
            g.find(&format!("Here it is: {TOKEN}. Done.")),
            vec!["the login file .x/auth.json of fake".to_string()]
        );
        // A piece of 16 characters is enough.
        assert!(!g.find(&TOKEN[4..20]).is_empty());
        // Split by whitespace, or in other case.
        let split: String = TOKEN
            .chars()
            .enumerate()
            .flat_map(|(i, c)| if i % 3 == 0 { vec![' ', c] } else { vec![c] })
            .collect();
        assert!(!g.find(&split).is_empty(), "{split}");
        assert!(!g.find(&format!("{}\n", TOKEN.to_uppercase())).is_empty());
        assert!(!g.find(&TOKEN.replace("Ab3d", "Ab3\u{200b}d")).is_empty());
    }

    #[test]
    fn short_words_and_names_never_match() {
        let mut g = Guard::default();
        g.add_file(
            "f",
            &format!(r#"{{"access_token":"{TOKEN}","refreshTokenExpiresAt":"x","path":"/home/demo/.config/tool","email":"someone@example.org","expires":"2026-11-02T10:00:00.000Z"}}"#),
        );
        for ok in [
            "sk-test",
            "access_token",
            "refreshTokenExpiresAt",
            "the answer is 42",
            "/home/demo/.config/tool",
            "someone@example.org",
            "2026-11-02T10:00:00.000Z",
            &TOKEN[..15],
        ] {
            assert!(g.find(ok).is_empty(), "{ok}");
        }
        // A secret shorter than the minimum has no marks.
        assert!(marks("short-secret-1").is_empty());
    }

    #[test]
    fn delivered_marks_match_like_values() {
        let mut g = Guard::default();
        g.add_marks("the credential API of b", &marks("ghp_0123456789abcdefXYZ"));
        assert_eq!(
            g.find("token: GHP_0123456789ABCDEF xyz"),
            vec!["the credential API of b".to_string()]
        );
        assert!(g.find("ghp_0123").is_empty());
    }

    #[test]
    fn plain_files_give_their_words() {
        let t = tokens("machine example.org\nlogin demo\npassword Zq7wX2pL9mNb4vC8kD\n");
        assert_eq!(t, vec!["Zq7wX2pL9mNb4vC8kD".to_string()]);
    }

    #[test]
    fn the_tail_keeps_the_end_within_the_limit() {
        let big = format!("{}END", "x".repeat(MAX_ANSWER + 100));
        let (t, cut) = tail(big.as_bytes(), MAX_ANSWER);
        assert!(cut);
        assert_eq!(t.len(), MAX_ANSWER);
        assert!(t.ends_with("END"));
        let (t, cut) = tail("short".as_bytes(), MAX_ANSWER);
        assert_eq!((t.as_str(), cut), ("short", false));
        // A cut inside a character starts at the next one.
        let (t, cut) = tail("ééé".as_bytes(), 3);
        assert_eq!((t.as_str(), cut), ("é", true));
    }

    fn r(id: &str, harness: &str) -> Running {
        serde_json::from_value(serde_json::json!({
            "id": id, "harness": harness, "project": "/p", "backend": "bwrap",
            "network": "none", "gui": "off", "pid": 1, "started": 0,
        }))
        .unwrap()
    }

    #[test]
    fn routes_by_harness_or_sandbox_id() {
        let all = vec![
            r("a1", "claude"),
            r("b1", "codex"),
            r("c1", "sh"),
            r("c2", "sh"),
        ];
        assert_eq!(route(&all, "a1", "codex").unwrap().id, "b1");
        assert_eq!(route(&all, "a1", "c2").unwrap().id, "c2");
        let (s, m) = route(&all, "a1", "sh").unwrap_err();
        assert_eq!(s, Status::Invalid);
        assert!(m.contains("c1, c2"), "{m}");
        let (s, m) = route(&all, "a1", "gemini").unwrap_err();
        assert_eq!(s, Status::NotFound);
        assert!(m.contains("codex (b1)"), "{m}");
        assert_eq!(route(&all, "a1", "a1").unwrap_err().0, Status::Invalid);
        // Its own harness names only the others.
        assert_eq!(route(&all, "c1", "sh").unwrap().id, "c2");
    }

    #[test]
    fn the_host_checks_size_timeout_and_busy_before_it_asks() {
        let tmp = tempfile::tempdir().unwrap();
        let env = HostEnv::from_parts(
            tmp.path().join("home"),
            "demo".into(),
            1000,
            1000,
            Default::default(),
        );
        std::fs::create_dir_all(env.paddock_config()).unwrap();
        std::fs::write(
            env.paddock_config().join("config.toml"),
            "[pastures.team]\nmembers = [\"sh\"]\nbridge = true\n",
        )
        .unwrap();
        let project = Path::new("/p");
        let mut b = r("b1", "codex");
        b.pid = std::process::id();
        b.kv_project = Some(project.into());
        b.pasture = Some("team".into());
        b.register(&env).unwrap();
        let base = dir(&env, project, "team");
        let rec = MemberRecord {
            oneshot: true,
            ..Default::default()
        };
        record_member(&base, "b1", &rec).unwrap();
        let id = Identity {
            sandbox: "a1".into(),
            harness: "sh".into(),
            project: project.into(),
            pasture: Some("team".into()),
            parent: None,
            chain: vec![],
            tree: None,
        };
        let req = |prompt: String, timeout: Option<u64>| Request {
            v: crate::kv::PROTOCOL,
            op: "ask".into(),
            key: Some("codex".into()),
            value: Some(prompt),
            timeout,
            ..Default::default()
        };
        let big = "x".repeat(MAX_PROMPT + 1);
        assert_eq!(ask(&env, &id, &req(big, None)).status, Status::TooLarge);
        // More than [spawn] timeout.
        let r = ask(&env, &id, &req("q".into(), Some(100_000)));
        assert_eq!(r.status, Status::Invalid, "{}", r.message);
        // Another question holds the target.
        let sp = spool(&base, "b1");
        crate::privfs::dir(&sp).unwrap();
        let held = try_lock(&sp.join("lock")).unwrap();
        let r = ask(&env, &id, &req("q".into(), Some(5)));
        assert_eq!(r.status, Status::Conflict, "{}", r.message);
        assert!(r.message.contains("another question"), "{}", r.message);
        drop(held);
        // The audit names the sizes, never the text.
        let audit = std::fs::read_to_string(crate::kv::audit_file(&env)).unwrap();
        assert!(audit.contains("\"result\":\"busy\""), "{audit}");
        assert!(audit.contains("\"target_harness\":\"codex\""), "{audit}");
        assert!(!audit.contains("xxxx"));
    }

    #[test]
    fn one_run_answers_on_stdin_and_through_a_file() {
        let tmp = tempfile::tempdir().unwrap();
        let q = Question {
            id: "q1".into(),
            from: "a1".into(),
            from_harness: "sh".into(),
            prompt: "what is two and two".into(),
            timeout: 10,
        };
        let stdin = Command {
            argv: vec![
                "sh".into(),
                "-c".into(),
                "printf 'log\\n'; read l; echo \"answer: $l\"".into(),
            ],
            via: PromptVia::Stdin,
            prompt_file: String::new(),
        };
        let a = run(&stdin, &q);
        assert_eq!(a.exit_code, Some(0), "{a:?}");
        assert!(a.text.ends_with("answer: what is two and two\n"), "{a:?}");
        let file = tmp.path().join("h/.paddock-bridge/prompt.md");
        let o: crate::profile::OneShot = toml::from_str(
            "args = [\"-c\", \"cat \\\"$0\\\"\", \"{prompt_file}\"]\nprompt = \"file\"\n",
        )
        .unwrap();
        let c = Command::of(Path::new("sh"), vec![], &o, &tmp.path().join("h"));
        assert_eq!(c.prompt_file, file.display().to_string());
        let a = run(&c, &q);
        assert_eq!(a.text, "what is two and two", "{a:?}");
        assert!(!file.exists());
        // A run that takes too long ends with an error.
        let slow = Command {
            argv: vec!["sh".into(), "-c".into(), "sleep 30".into()],
            via: PromptVia::Stdin,
            prompt_file: String::new(),
        };
        let a = run(&slow, &Question { timeout: 1, ..q });
        assert!(a.error.unwrap().contains("did not finish"));
    }
}
