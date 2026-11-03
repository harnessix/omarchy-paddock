//! A stuck detached run: its pane shows the same screen for `[detach]
//! stuck_after` (3 minutes by default). paddock then records the last
//! screen in the run record, shows one desktop notification, and writes
//! `stuck:<RUN-ID>` into kv for a parent that watches: the tree scope for a
//! spawn child, the pasture scope for a pasture member. The idle end later
//! puts the last screen into its done text.

use super::{clean_text, idle_end_text, Ctl, Record};
use crate::env::HostEnv;
use crate::screens::ErrorRule;
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

/// `[detach] stuck_after`: seconds with the same screen. `0` turns it off.
pub const STUCK_AFTER: u64 = 3 * 60;
/// A record keeps at most this many notes.
const MAX_NOTES: usize = 16;

/// What the record says about a stuck run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stuck {
    /// Unix time of the last change on the screen.
    pub since: u64,
    /// The visible text, trimmed (`screens::last_screen`).
    pub screen: String,
    /// A known error on the screen, in clear words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Add a note to the run record of a detached run, such as an answered
/// start screen. `paddock wait` and `paddock ps` show them.
pub fn add_note(env: &HostEnv, id: &str, text: &str) -> Result<()> {
    Record::update(env, id, |r| {
        if r.notes.len() < MAX_NOTES {
            r.notes.push(clean_text(text));
        }
    })
}

/// What one look at the screen found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Nothing,
    /// The screen has not changed for the stuck time: the last change and
    /// the trimmed screen.
    Stuck {
        since: u64,
        screen: String,
    },
    /// A stuck screen changed.
    Moved,
}

/// Decides from screens and times alone, so a test can give it a fake clock.
#[derive(Debug, Default)]
pub struct Detector {
    screen: Option<String>,
    since: u64,
    reported: bool,
}

impl Detector {
    /// The screen at `now`. `after` is the stuck time, `0` for never.
    pub fn observe(&mut self, now: u64, screen: &str, after: u64) -> Event {
        if self.screen.as_deref() != Some(screen) {
            let was = self.reported;
            self.screen = Some(screen.to_string());
            self.since = now;
            self.reported = false;
            return if was { Event::Moved } else { Event::Nothing };
        }
        if after == 0 || self.reported || now.saturating_sub(self.since) < after {
            return Event::Nothing;
        }
        self.reported = true;
        Event::Stuck {
            since: self.since,
            screen: crate::screens::last_screen(screen),
        }
    }
}

/// The done text of an idle end: the quiet time, a known error, and the
/// last screen.
pub fn idle_done_text(quiet: u64, screen: Option<&str>, error: Option<&str>) -> String {
    let mut t = idle_end_text(quiet);
    if let Some(e) = error {
        t.push_str(". ");
        t.push_str(e);
    }
    if let Some(s) = screen.filter(|s| !s.trim().is_empty()) {
        t.push_str("\nlast screen:\n");
        t.push_str(s);
    }
    clean_text(&t)
}

impl Ctl {
    /// One look at the pane. Called from the idle loop.
    pub(super) fn poll_stuck(
        &self,
        tmux: &crate::view::Tmux,
        det: &mut Detector,
        rec: &Record,
        after: u64,
    ) {
        let Some(text) = tmux.pane_tail(&self.id) else {
            return;
        };
        match det.observe(crate::state::now(), &text, after) {
            Event::Nothing => {}
            Event::Moved => {
                let _ = Record::update(&self.env, &self.id, |r| r.stuck = None);
            }
            Event::Stuck { since, screen } => {
                let error = crate::screens::known_error(&self.errors(), &screen);
                let s = Stuck {
                    since,
                    screen,
                    error,
                };
                let _ = Record::update(&self.env, &self.id, |r| {
                    if r.ended.is_none() && r.done.is_none() {
                        r.stuck = Some(s.clone());
                    }
                });
                self.notify_stuck(rec, &s, after);
                if let Err(e) = write_kv(&self.env, &self.id, rec, &s) {
                    log::debug!("stuck:{}: {e:#}", self.id);
                }
            }
        }
    }

    fn notify_stuck(&self, rec: &Record, s: &Stuck, after: u64) {
        let why = match &s.error {
            Some(e) => e.clone(),
            None => format!("its screen has not changed for {}", minutes(after)),
        };
        let _ = crate::broker::notify::show(
            &self.env,
            &crate::broker::notify::Note {
                summary: "paddock".into(),
                body: format!("{} looks stuck: {why}", super::who(rec)),
                actions: Vec::new(),
                critical: false,
            },
        );
    }

    /// The done text of the idle end, with the screen of the pane now.
    pub(super) fn idle_text(&self, quiet: u64) -> String {
        let screen = self
            .tmux()
            .and_then(|t| t.pane_tail(&self.id))
            .map(|t| crate::screens::last_screen(&t));
        let error = screen
            .as_deref()
            .and_then(|s| crate::screens::known_error(&self.errors(), s));
        idle_done_text(quiet, screen.as_deref(), error.as_deref())
    }

    fn errors(&self) -> Vec<ErrorRule> {
        self.errors.lock().map(|e| e.clone()).unwrap_or_default()
    }
}

fn minutes(secs: u64) -> String {
    if secs >= 60 && secs.is_multiple_of(60) {
        format!("{} minutes", secs / 60)
    } else {
        format!("{secs} seconds")
    }
}

/// `stuck:<RUN-ID>` in the scope that a parent watches. A run that is
/// neither a spawn child nor a pasture member writes nothing.
fn write_kv(env: &HostEnv, id: &str, rec: &Record, s: &Stuck) -> Result<()> {
    use crate::kv::shard::Ns;
    use crate::kv::store::{Quota, Store, Writer};
    let all = crate::state::Running::list(env);
    let Some(r) = all.iter().find(|r| r.id == id) else {
        return Ok(());
    };
    let ident = crate::kv::identity_of(r, &all);
    let ns = match (&ident.tree, ident.is_child(), &ident.pasture) {
        (Some(t), true, _) => Ns::Tree(t.clone()),
        (_, false, Some(_)) => crate::kv::pasture_ns(&ident),
        _ => return Ok(()),
    };
    let cfg = crate::kv::KvCfg::load(env).unwrap_or_default();
    if !cfg.enabled {
        return Ok(());
    }
    let value = serde_json::json!({
        "id": id,
        "harness": rec.harness,
        "since": s.since,
        "screen": s.screen,
        "error": s.error,
    });
    let mut st = Store::open(&ns.file(&crate::kv::kv_root(env))?)?;
    st.put(
        &ns.key(),
        &format!("stuck:{id}"),
        &value.to_string(),
        None,
        None,
        &Writer {
            sandbox: "paddock",
            harness: "paddock",
        },
        Quota {
            bytes: cfg.project_bytes,
            keys: cfg.max_keys,
        },
        crate::state::now(),
    )
    .map_err(|e| anyhow!("{e:?}"))?;
    Ok(())
}

#[cfg(test)]
mod tests;
