//! The training gate: paddock refuses the optional training opt-in in a
//! harness's terminal interface.
//!
//! Some vendors ask on each new login whether the user's data can train
//! their models, with the box already checked, and keep the answer on the
//! account. No file can preset it. With `[privacy] training = "deny"` (the
//! default), paddock owns the harness's terminal for a short setup gate,
//! keeps the visible screen in a terminal model (`vt100`), and answers only
//! that one question, by the rules of the harness profile.
//!
//! The guards:
//! - A rule names whole trimmed rows of the visible screen. The complete
//!   dialog, with every choice and the focus, must be on screen as one block,
//!   exactly once. Otherwise paddock sends nothing.
//! - paddock sends one key, reads the screen again, and sends the next key
//!   only when the new screen is one of the states that the rule allows next.
//!   The state before the final key shows the box unchecked.
//! - A mismatch, a resize or a timeout stops the gate and leaves the screen
//!   to the user. A user key while a rule is under way cancels the gate
//!   before the key goes to the harness. One lock serializes all input.
//! - The gate runs only at setup, before the task and before model output,
//!   so neither the repository nor the model can draw a dialog for it.
//! - Rules come only with the built-in profiles. A user profile can name a
//!   built-in rule, and a project file has no part in it.
//!
//! The live part (the threads next to the relay) is in `training/live.rs`.

pub mod live;

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

/// `[privacy] training` in the user configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Policy {
    /// paddock refuses the optional training opt-in by itself.
    #[default]
    Deny,
    /// paddock types nothing. A run without a terminal stops, so that the
    /// user can answer in a terminal.
    Manual,
    /// paddock does not touch the setting.
    Vendor,
}

/// The policy of a merged manifest. Only the user configuration sets it
/// (`Manifest::merge` ignores `[privacy]` of a project file).
pub fn policy(m: &crate::manifest::Manifest) -> Policy {
    m.privacy.training.unwrap_or_default()
}

/// The keys that a rule can send. Nothing else: no letters, no raw bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Key {
    Up,
    Down,
    Left,
    Right,
    Tab,
    Enter,
    Space,
    Escape,
}

impl Key {
    /// The bytes a terminal sends for the key (normal cursor mode).
    pub fn bytes(self) -> &'static [u8] {
        match self {
            Key::Up => b"\x1b[A",
            Key::Down => b"\x1b[B",
            Key::Right => b"\x1b[C",
            Key::Left => b"\x1b[D",
            Key::Tab => b"\t",
            Key::Enter => b"\r",
            Key::Space => b" ",
            Key::Escape => b"\x1b",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Key::Up => "up",
            Key::Down => "down",
            Key::Left => "left",
            Key::Right => "right",
            Key::Tab => "tab",
            Key::Enter => "enter",
            Key::Space => "space",
            Key::Escape => "escape",
        }
    }
}

/// The `next` entry that means: the dialog is gone, the answer is in.
pub const GONE: &str = "gone";

/// At most this many keys for one dialog.
pub const MAX_ACTIONS: usize = 6;

/// `[[privacy.training.rule]]` in a built-in profile.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    /// Names the rule in the audit log, for example `agy-1.4-data-use`.
    pub id: String,
    /// The vendor page about the setting.
    pub url: String,
    /// A row that only this dialog shows. When it is on screen and no state
    /// matches, the dialog differs from the rule: paddock sends nothing.
    pub title: String,
    /// The terminal width of the recording. The harness wraps its text to
    /// the width, so paddock gives it this width during the gate, and the
    /// real width when the gate ends. A terminal narrower than this gets no
    /// gate.
    pub cols: Option<u16>,
    /// The dialog belongs to the onboarding that the profile's `[state]
    /// done_flags` record. When they are all true in the sandbox home, the
    /// dialog does not come, and paddock starts no gate.
    #[serde(default)]
    pub until_done: bool,
    /// Rows of the setup screens before the dialog, such as a login or a
    /// theme choice. A user key on such a screen goes to the harness and
    /// keeps the gate. A user key on any other screen ends the gate.
    #[serde(default)]
    pub before: Vec<String>,
    /// The states of the dialog. Any state can be the first one.
    pub state: Vec<State>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    pub id: String,
    /// The block of whole rows, trimmed, from the top of the dialog down.
    /// An empty string is an empty row.
    pub rows: Vec<String>,
    /// The one key for this state.
    pub send: Key,
    /// The states that can follow the key, or `gone`.
    pub next: Vec<String>,
}

impl Rule {
    pub fn check(&self) -> Result<()> {
        if self.id.trim().is_empty() || self.title.trim().is_empty() {
            bail!("a training rule needs an id and a title");
        }
        if self.title.trim() != self.title {
            bail!("rule {}: the title must be a trimmed row", self.id);
        }
        if self.state.is_empty() {
            bail!("rule {} has no state", self.id);
        }
        if self.cols == Some(0) {
            bail!("rule {}: cols must be above 0", self.id);
        }
        let mut seen = std::collections::BTreeSet::new();
        for s in &self.state {
            if s.id == GONE || !seen.insert(s.id.as_str()) {
                bail!("rule {}: state id {} is taken", self.id, s.id);
            }
            if s.rows.is_empty() || s.rows.iter().all(|r| r.trim().is_empty()) {
                bail!("rule {} state {}: rows are empty", self.id, s.id);
            }
            if s.rows.iter().any(|r| r.trim() != r) {
                bail!("rule {} state {}: rows must be trimmed", self.id, s.id);
            }
            if !s.rows.iter().any(|r| r == &self.title) {
                bail!("rule {} state {}: rows must hold the title", self.id, s.id);
            }
            if s.next.is_empty() {
                bail!("rule {} state {}: next is empty", self.id, s.id);
            }
        }
        for s in &self.state {
            for n in &s.next {
                if n != GONE && !seen.contains(n.as_str()) {
                    bail!("rule {} state {}: unknown next state {n}", self.id, s.id);
                }
            }
        }
        Ok(())
    }

    fn state_index(&self, id: &str) -> Option<usize> {
        self.state.iter().position(|s| s.id == id)
    }
}

/// Where `block` is in `rows`, compared trimmed, row by row. `Some(None)`
/// when it is there more than once.
pub fn find_block(rows: &[String], block: &[String]) -> Option<Option<usize>> {
    if block.is_empty() || block.len() > rows.len() {
        return None;
    }
    let mut found = None;
    for start in 0..=rows.len() - block.len() {
        let hit = rows[start..start + block.len()]
            .iter()
            .zip(block)
            .all(|(r, b)| r.trim() == b);
        if hit {
            if found.is_some() {
                return Some(None);
            }
            found = Some(start);
        }
    }
    found.map(Some)
}

fn has_row(rows: &[String], row: &str) -> bool {
    rows.iter().any(|r| r.trim() == row)
}

/// What the gate does after it read a settled screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Nothing to do yet.
    Wait,
    /// Send this key.
    Send(Key),
    /// The gate ends.
    End(Outcome),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The rule ran to its end, and the dialog is gone.
    Declined,
    /// The screen did not match: paddock sent nothing more.
    Stopped(String),
    /// A user key came while the rule was under way.
    Cancelled,
    /// The gate ended before any dialog: setup is over, or it timed out.
    NotShown(String),
}

impl Outcome {
    pub fn text(&self) -> String {
        match self {
            Outcome::Declined => "declined".into(),
            Outcome::Stopped(why) => format!("stopped: {why}"),
            Outcome::Cancelled => "cancelled by a user key".into(),
            Outcome::NotShown(why) => format!("not shown: {why}"),
        }
    }
}

/// What the latest settled screen is, for a user key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A setup screen before the dialog (`before`).
    Before,
    /// Anything else.
    Other,
}

/// What the gate does with a user key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserKey {
    /// Forward it and keep the gate.
    Keep,
    /// End the gate first, then forward it.
    End(Outcome),
}

/// The decisions of the gate, without a terminal. `live.rs` feeds it.
#[derive(Debug, Clone)]
pub struct Machine {
    rules: Vec<Rule>,
    /// The rule under way and its last state.
    engaged: Option<(usize, usize)>,
    sent: Vec<Key>,
    outcome: Option<Outcome>,
    kind: Kind,
}

impl Machine {
    /// Only valid rules. The caller checks them first.
    pub fn new(rules: Vec<Rule>) -> Self {
        Machine {
            rules,
            engaged: None,
            sent: Vec::new(),
            outcome: None,
            kind: Kind::Other,
        }
    }

    pub fn outcome(&self) -> Option<&Outcome> {
        self.outcome.as_ref()
    }

    pub fn engaged(&self) -> bool {
        self.engaged.is_some()
    }

    /// The rule id for the audit line: the rule under way, or the rule
    /// whose dialog did not match.
    pub fn rule_id(&self) -> Option<&str> {
        self.engaged.map(|(r, _)| self.rules[r].id.as_str())
    }

    pub fn sent(&self) -> &[Key] {
        &self.sent
    }

    fn end(&mut self, o: Outcome) -> Step {
        self.outcome = Some(o.clone());
        Step::End(o)
    }

    /// Read a settled screen: its rows, as `vt100` shows them.
    pub fn on_screen(&mut self, rows: &[String]) -> Step {
        if self.outcome.is_some() {
            return Step::Wait;
        }
        let mut hits = Vec::new();
        for (ri, rule) in self.rules.iter().enumerate() {
            for (si, s) in rule.state.iter().enumerate() {
                match find_block(rows, &s.rows) {
                    Some(Some(_)) => hits.push((ri, si)),
                    Some(None) => {
                        self.engaged.get_or_insert((ri, si));
                        return self.end(Outcome::Stopped("the dialog is on screen twice".into()));
                    }
                    None => {}
                }
            }
        }
        if hits.len() > 1 {
            self.engaged.get_or_insert(hits[0]);
            return self.end(Outcome::Stopped("more than one state matches".into()));
        }
        let titled = self.rules.iter().position(|r| has_row(rows, &r.title));
        match (self.engaged, hits.first().copied()) {
            (None, Some((ri, si))) => self.act(ri, si),
            (None, None) => {
                if let Some(ri) = titled {
                    self.engaged = Some((ri, 0));
                    return self.end(Outcome::Stopped("the dialog differs from the rule".into()));
                }
                self.kind = if self.before(rows) {
                    Kind::Before
                } else {
                    Kind::Other
                };
                Step::Wait
            }
            (Some((ri, si)), hit) => {
                let rule = &self.rules[ri];
                let next = &rule.state[si].next;
                match hit {
                    Some((hr, hs))
                        if hr == ri && next.iter().any(|n| rule.state_index(n) == Some(hs)) =>
                    {
                        self.act(hr, hs)
                    }
                    None if next.iter().any(|n| n == GONE) && titled.is_none() => {
                        self.end(Outcome::Declined)
                    }
                    _ => {
                        let key = self.sent.last().map(|k| k.name()).unwrap_or("?");
                        self.end(Outcome::Stopped(format!(
                            "the screen after {key} does not match the rule"
                        )))
                    }
                }
            }
        }
    }

    fn act(&mut self, ri: usize, si: usize) -> Step {
        self.engaged = Some((ri, si));
        if self.sent.len() >= MAX_ACTIONS {
            return self.end(Outcome::Stopped("too many keys".into()));
        }
        let key = self.rules[ri].state[si].send;
        self.sent.push(key);
        Step::Send(key)
    }

    /// A user key, with the screen as it is now, settled or not. A key on
    /// the dialog, or while a rule is under way, cancels the gate. A key on
    /// a setup screen before the dialog keeps it. Any other key ends it.
    pub fn on_user_key(&mut self, rows: &[String]) -> UserKey {
        if self.outcome.is_some() {
            return UserKey::Keep;
        }
        let titled = self.rules.iter().position(|r| has_row(rows, &r.title));
        let o = if self.engaged.is_some() {
            Outcome::Cancelled
        } else if let Some(ri) = titled {
            self.engaged = Some((ri, 0));
            Outcome::Cancelled
        } else if self.kind == Kind::Before || self.before(rows) {
            return UserKey::Keep;
        } else {
            Outcome::NotShown("a user key on another screen".into())
        };
        self.outcome = Some(o.clone());
        UserKey::End(o)
    }

    fn before(&self, rows: &[String]) -> bool {
        self.rules
            .iter()
            .any(|r| r.before.iter().any(|b| has_row(rows, b)))
    }

    /// The caller's terminal changed its size.
    pub fn on_resize(&mut self) -> Option<Outcome> {
        self.stop_with(if self.engaged.is_some() {
            Outcome::Stopped("the terminal changed its size".into())
        } else {
            Outcome::NotShown("the terminal changed its size".into())
        })
    }

    /// No answer of the harness after a key, the gate deadline, or the end
    /// of the harness.
    pub fn on_timeout(&mut self, what: &str) -> Option<Outcome> {
        self.stop_with(if self.engaged.is_some() {
            Outcome::Stopped(what.into())
        } else {
            Outcome::NotShown(what.into())
        })
    }

    fn stop_with(&mut self, o: Outcome) -> Option<Outcome> {
        if self.outcome.is_some() {
            return None;
        }
        self.outcome = Some(o.clone());
        Some(o)
    }

    /// The one audit line: the rule id, the keys and the outcome. Never a
    /// row of the screen, which can hold secrets. None when no dialog came.
    pub fn audit_detail(&self) -> Option<String> {
        let id = self.rule_id()?;
        let keys: Vec<&str> = self.sent.iter().map(|k| k.name()).collect();
        let keys = if keys.is_empty() {
            "none".to_string()
        } else {
            keys.join(",")
        };
        let outcome = self
            .outcome
            .as_ref()
            .map(Outcome::text)
            .unwrap_or_else(|| "open".into());
        Some(format!("training rule {id}: keys {keys}: {outcome}"))
    }
}

/// The gate for one run, or None. `terminal` is true when the caller gave
/// paddock a terminal, so the harness runs on a paddock pty.
///
/// - `vendor`, a harness without rules, or a finished onboarding: no gate.
/// - `manual`: no gate. Without a terminal, the run stops here while the
///   dialog can still come, so that the user answers it in a terminal.
/// - `deny`: a gate when there is a terminal. Without one the harness runs
///   on pipes, as before, and has no screen to answer.
pub fn prepare(
    env: &crate::env::HostEnv,
    profile: &crate::profile::Profile,
    policy: Policy,
    spec: &crate::spec::SandboxSpec,
    terminal: bool,
) -> Result<Option<std::sync::Arc<live::Gate>>> {
    if policy == Policy::Vendor {
        return Ok(None);
    }
    let onboarding_open = profile.state.done_flags.is_empty()
        || crate::privacy::open_onboarding(&spec.home_src, profile).is_some();
    let rules: Vec<Rule> = profile
        .training_rules()
        .into_iter()
        .filter(|r| !r.until_done || onboarding_open)
        .collect();
    if rules.is_empty() {
        return Ok(None);
    }
    if policy == Policy::Manual {
        if !terminal {
            bail!(
                "{} can ask in its setup whether its vendor can use your data for training, and [privacy] training = \"manual\" leaves that answer to you. Start it once in a terminal (paddock run {}), answer the question, and run this again",
                profile.name,
                profile.name
            );
        }
        return Ok(None);
    }
    if !terminal {
        return Ok(None);
    }
    let (env, spec) = (env.clone(), spec.clone());
    Ok(Some(live::Gate::new(rules, move |m: &Machine| {
        let Some(detail) = m.audit_detail() else {
            log::debug!(
                "training gate: {}",
                m.outcome().map(Outcome::text).unwrap_or_default()
            );
            return;
        };
        let decision = match m.outcome() {
            Some(Outcome::Declined) => "declined",
            Some(Outcome::Cancelled) => "cancelled",
            _ => "stopped",
        };
        crate::broker::audit_line(
            &env,
            &serde_json::json!({
                "time": crate::state::now(),
                "sandbox": spec.id,
                "harness": spec.harness,
                "project": spec.project,
                "capability": "training",
                "decision": decision,
                "detail": detail,
            }),
        );
    })))
}

/// The notice before the harness starts.
pub fn notice(harness: &str) -> String {
    format!(
        "{harness} can ask in its setup whether its vendor can use your data for training. paddock unchecks that box for you ([privacy] training = \"deny\"). A key of yours on that screen gives it back to you"
    )
}

/// The line after the run, when a dialog came.
pub fn summary(result: &(Option<String>, Outcome), setting: Option<&str>) -> Option<String> {
    let (Some(id), o) = result else {
        return None;
    };
    let mut line = format!("training rule {id}: {}", o.text());
    if *o != Outcome::Declined {
        line.push_str(". Check the data-use choice");
        if let Some(s) = setting {
            line.push_str(": ");
            line.push_str(s);
        }
    }
    Some(line)
}

/// The recorded agy screens, for the tests of this module and of live.rs.
#[cfg(test)]
#[path = "../tests/common/agy_screens.rs"]
mod screens;
#[cfg(test)]
mod tests;
