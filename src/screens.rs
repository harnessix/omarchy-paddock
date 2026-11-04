//! Start screens: paddock answers the known screens that stop a harness
//! before its task, such as a review of new hooks, in runs that nobody
//! watches.
//!
//! A run in a detached tmux pane (detach.rs) or on an invisible terminal
//! (relay.rs) has no one at the keyboard. When the harness shows a screen
//! that waits for a choice, the run sits there until the idle end. The
//! `[[screens.rule]]` entries of a harness profile name such screens and a
//! safe answer for each one.
//!
//! The guards:
//! - A rule names whole trimmed rows of the visible screen, the same way
//!   as the training rules (training.rs). The block must be on screen
//!   exactly once. `{n}` in a row stands for a number.
//! - The answer must not grant anything: a rule whose answer row trusts,
//!   approves, accepts, allows, agrees or logs in fails to load. The keys
//!   must choose the answer row: a digit key names its number, a lone
//!   `enter` needs the focus on it.
//! - In a run that the user watches, paddock answers only the rules marked
//!   `always`. A user key ends the watcher for the rest of the run.
//! - The watcher reads the screen only in the first minutes of a run, and
//!   answers each rule at most a few times.
//!
//! The live part (the thread next to the relay) is in `screens/live.rs`.
//! The stuck detector of detached runs is in `detach/stuck.rs`.

pub mod live;

use crate::training::{find_block, Key};
use anyhow::{bail, Result};
use serde::Deserialize;

/// At most this many keys for one answer.
pub const MAX_KEYS: usize = 4;

/// `[screens]` of a harness profile.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Screens {
    #[serde(default)]
    pub rule: Vec<Rule>,
    /// Messages that explain why a run stopped, such as a used quota.
    #[serde(default)]
    pub error: Vec<ErrorRule>,
}

/// `[[screens.rule]]`: a screen and its safe answer.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    /// Names the rule in the audit log, for example `codex-hooks-review`.
    pub id: String,
    /// The screen in plain words, for the notes: `Codex hooks review`.
    pub name: String,
    /// The block of whole rows, trimmed, from the top down. `{n}` stands
    /// for a number. An empty string is an empty row.
    pub rows: Vec<String>,
    /// The row of the choice that the keys select. It must be in `rows`.
    pub answer: String,
    /// The keys: `up`, `down`, `left`, `right`, `tab`, `enter`, `space`,
    /// `escape`, or one digit.
    pub send: Vec<String>,
    /// What the answer did, for the notes. `{n}` is the number of the rows.
    pub note: String,
    /// Answer also in a run that the user watches.
    #[serde(default)]
    pub always: bool,
}

/// `[[screens.error]]`: a message on the screen and what it means.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ErrorRule {
    /// Text in a row, compared without case.
    pub text: String,
    /// The clear words for the done text.
    pub says: String,
}

/// The messages that every harness can show.
const KNOWN_ERRORS: &[(&str, &str)] = &[
    ("quota reached", "the harness says its quota is used up"),
    ("quota exceeded", "the harness says its quota is used up"),
    ("usage limit", "the harness says its usage limit is reached"),
    ("rate limit", "the harness says it hit a rate limit"),
    (
        "out of credits",
        "the harness says the account has no credits",
    ),
];

/// Words of an answer that grants something. A rule with one of them in
/// its answer row fails to load.
const GRANTS: &[&str] = &[
    "trust", "approve", "accept", "allow", "agree", "consent", "authori", "log in", "login",
    "sign in", "sign-in", "opt in", "opt-in", "enable", "yes",
];

/// Phrases that say no to a grant. They go before the check of `GRANTS`.
const REFUSALS: &[&str] = &[
    "without trusting",
    "without trust",
    "don't trust",
    "do not trust",
    "not trusted",
];

/// The marks that harnesses put before the focused choice.
const FOCUS: &[char] = &['›', '>', '❯', '▶', '→'];

/// One key as bytes: a named key or a digit.
pub fn key_bytes(name: &str) -> Option<Vec<u8>> {
    if name.len() == 1 && name.as_bytes()[0].is_ascii_digit() {
        return Some(name.as_bytes().to_vec());
    }
    use Key::*;
    [Up, Down, Left, Right, Tab, Enter, Space, Escape]
        .into_iter()
        .find(|k| k.name() == name)
        .map(|k| k.bytes().to_vec())
}

/// The answer row without its focus mark.
fn choice(row: &str) -> &str {
    row.trim_start_matches(FOCUS).trim_start()
}

/// The answer grants trust, approval, terms or a login.
pub fn grants(answer: &str) -> bool {
    let mut a = answer.to_lowercase();
    for r in REFUSALS {
        a = a.replace(r, " ");
    }
    GRANTS.iter().any(|g| a.contains(g))
}

impl Rule {
    pub fn check(&self) -> Result<()> {
        let id = &self.id;
        if id.trim().is_empty() || self.name.trim().is_empty() || self.note.trim().is_empty() {
            bail!("a screen rule needs an id, a name and a note");
        }
        if self.rows.is_empty() || self.rows.iter().all(|r| r.trim().is_empty()) {
            bail!("screen rule {id}: rows are empty");
        }
        if self.rows.iter().any(|r| r.trim() != r) {
            bail!("screen rule {id}: rows must be trimmed");
        }
        if !self.rows.contains(&self.answer) {
            bail!("screen rule {id}: the answer must be one of the rows");
        }
        if grants(&self.answer) {
            bail!(
                "screen rule {id}: the answer {:?} grants trust, an approval, terms or a login. paddock never chooses that",
                self.answer
            );
        }
        if self.send.is_empty() || self.send.len() > MAX_KEYS {
            bail!("screen rule {id}: send needs 1 to {MAX_KEYS} keys");
        }
        if let Some(k) = self.send.iter().find(|k| key_bytes(k).is_none()) {
            bail!("screen rule {id}: {k:?} is not a key");
        }
        let last = self.send.last().map(String::as_str).unwrap_or("");
        let digit = last.len() == 1 && last.as_bytes()[0].is_ascii_digit();
        if digit {
            let c = choice(&self.answer);
            if !c.starts_with(&format!("{last}.")) && !c.starts_with(&format!("{last})")) {
                bail!("screen rule {id}: the key {last} does not choose the answer row");
            }
        }
        if self.send == ["enter"] && !self.answer.starts_with(FOCUS) {
            bail!("screen rule {id}: enter alone needs the focus on the answer row");
        }
        Ok(())
    }

    /// The note with the number from the screen.
    pub fn note_for(&self, n: Option<u64>) -> String {
        let n = n.map(|n| n.to_string()).unwrap_or_else(|| "some".into());
        format!("answered {}: {}", self.name, self.note.replace("{n}", &n))
    }
}

/// The valid rules of a list. The others go, with a warning.
pub fn valid(profile: &str, rules: &[Rule]) -> Vec<Rule> {
    rules
        .iter()
        .filter(|r| match r.check() {
            Ok(()) => true,
            Err(e) => {
                log::warn!("profile {profile}: {e:#}");
                false
            }
        })
        .cloned()
        .collect()
}

/// A row against a pattern: `Some(n)` when it matches, with the number
/// for `{n}`.
fn row_match(pat: &str, row: &str) -> Option<Option<u64>> {
    let row = row.trim();
    match pat.split_once("{n}") {
        None => (row == pat).then_some(None),
        Some((pre, post)) => {
            let mid = row.strip_prefix(pre)?.strip_suffix(post)?;
            if mid.is_empty() || !mid.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            Some(mid.parse().ok())
        }
    }
}

/// Where a block of patterns is in `rows`: the number for `{n}` when it is
/// there exactly once. `Err(())` when it is there more than once.
fn find_pattern(rows: &[String], block: &[String]) -> Result<Option<Option<u64>>, ()> {
    if !block.iter().any(|b| b.contains("{n}")) {
        return match find_block(rows, block) {
            Some(Some(_)) => Ok(Some(None)),
            Some(None) => Err(()),
            None => Ok(None),
        };
    }
    if block.is_empty() || block.len() > rows.len() {
        return Ok(None);
    }
    let mut found = None;
    for start in 0..=rows.len() - block.len() {
        let mut n = None;
        let hit = rows[start..start + block.len()]
            .iter()
            .zip(block)
            .all(|(r, b)| match row_match(b, r) {
                Some(v) => {
                    n = n.or(v);
                    true
                }
                None => false,
            });
        if hit {
            if found.is_some() {
                return Err(());
            }
            found = Some(n);
        }
    }
    Ok(found)
}

/// What a screen shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Found {
    Nothing,
    /// One rule matches: its index and the number for `{n}`.
    Rule(usize, Option<u64>),
    /// A block is there twice, or two rules match: paddock sends nothing.
    Unclear,
}

pub fn find(rules: &[Rule], rows: &[String]) -> Found {
    let mut hit = None;
    for (i, r) in rules.iter().enumerate() {
        match find_pattern(rows, &r.rows) {
            Err(()) => return Found::Unclear,
            Ok(None) => {}
            Ok(Some(n)) => {
                if hit.is_some() {
                    return Found::Unclear;
                }
                hit = Some((i, n));
            }
        }
    }
    match hit {
        Some((i, n)) => Found::Rule(i, n),
        None => Found::Nothing,
    }
}

/// A known error on the screen, in clear words with its row.
pub fn known_error(extra: &[ErrorRule], screen: &str) -> Option<String> {
    let known = KNOWN_ERRORS.iter().map(|(t, s)| (*t, *s));
    let all: Vec<(&str, &str)> = extra
        .iter()
        .map(|e| (e.text.as_str(), e.says.as_str()))
        .chain(known)
        .collect();
    for line in screen.lines() {
        let low = line.to_lowercase();
        if let Some((_, says)) = all
            .iter()
            .find(|(t, _)| !t.is_empty() && low.contains(&t.to_lowercase()))
        {
            return Some(format!(
                "{says}: {}",
                crate::view::plain_limit(line.trim(), 120)
            ));
        }
    }
    None
}

/// At most this many lines of the last screen go into a record.
pub const SCREEN_LINES: usize = 12;

/// The visible text of a screen, trimmed: no control characters, no empty
/// lines at the ends, one empty line at most between two others, the last
/// `SCREEN_LINES` lines.
pub fn last_screen(text: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    for l in text.lines() {
        let l = crate::view::plain_limit(l, 160);
        if l.is_empty() && lines.last().is_none_or(|p| p.is_empty()) {
            continue;
        }
        lines.push(l);
    }
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    let skip = lines.len().saturating_sub(SCREEN_LINES);
    lines[skip..].join("\n")
}

/// The valid rules that answer in a run: every one when nobody watches,
/// else only the ones marked `always`.
pub fn rules_for(profile: &crate::profile::Profile, unwatched: bool) -> Vec<Rule> {
    valid(&profile.name, &profile.screens.rule)
        .into_iter()
        .filter(|r| unwatched || r.always)
        .collect()
}

/// The rules of a harness for one run, or None.
///
/// - `unwatched`: a detached run or an invisible terminal. Every rule.
/// - Otherwise only the rules marked `always`.
/// - Without a terminal the harness has no screen to answer.
pub fn prepare(
    env: &crate::env::HostEnv,
    profile: &crate::profile::Profile,
    spec: &crate::spec::SandboxSpec,
    terminal: bool,
    unwatched: bool,
    detached: bool,
) -> Option<std::sync::Arc<live::Watcher>> {
    let rules = rules_for(profile, unwatched);
    if !terminal || rules.is_empty() {
        return None;
    }
    let (e, id) = (env.clone(), spec.id.clone());
    // A user with write access to a detached run watches it.
    let watched =
        move || detached && crate::view::Tmux::find(&e).is_some_and(|t| t.write_clients(&id) > 0);
    let (env, spec) = (env.clone(), spec.clone());
    Some(live::Watcher::new(
        rules,
        watched,
        move |r: &live::Report| report(&env, &spec, detached, r),
    ))
}

/// Tell about an answer: the view log (by the caller), the audit log, and
/// for a detached run its record and its tmux clients.
fn report(
    env: &crate::env::HostEnv,
    spec: &crate::spec::SandboxSpec,
    detached: bool,
    r: &live::Report,
) {
    log::info!("{}", r.text);
    crate::broker::audit_line(
        env,
        &serde_json::json!({
            "time": crate::state::now(),
            "sandbox": spec.id,
            "harness": spec.harness,
            "project": spec.project,
            "capability": "screens",
            "decision": if r.ok { "answered" } else { "stopped" },
            "detail": format!("screen rule {}: keys {}: {}", r.rule, r.keys, r.text),
        }),
    );
    if detached {
        let _ = crate::detach::add_note(env, &spec.id, &r.text);
        if let Some(t) = crate::view::Tmux::find(env) {
            t.message(&spec.id, &format!("paddock: {}", r.text));
        }
    }
}

/// The lines of `paddock explain` about the start screens of a profile.
pub fn explain(profile: &crate::profile::Profile) -> Vec<String> {
    let s = &profile.screens;
    let mut out = Vec::new();
    for r in &s.rule {
        let state = match r.check() {
            Ok(()) if r.always => "always".to_string(),
            Ok(()) => "when nobody watches".to_string(),
            Err(e) => format!("does not load: {e:#}"),
        };
        out.push(format!(
            "  {}  {}: keys {}, answer {:?} ({state})",
            r.id,
            r.name,
            r.send.join(","),
            r.answer
        ));
    }
    for e in &s.error {
        out.push(format!("  error  {:?}: {}", e.text, e.says));
    }
    out
}

#[cfg(test)]
mod tests;
