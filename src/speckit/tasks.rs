//! `tasks.md` of a Spec Kit feature: read the tasks, and write back only
//! their checkboxes and a short trailing note. Every other byte of the file
//! stays as it is, so the Spec Kit commands keep reading it.
//!
//! A task line is `- [ ] T012 [P] [US1] Text (depends on T010, T011)`. The
//! phase of a task is the last level-2 heading above it (`## Phase 3: ...`).
//! Lines in code fences and in HTML comment blocks are not tasks: the
//! template keeps its sample tasks in a comment.

use std::collections::BTreeMap;

/// The start of the note that paddock adds to a task line.
pub const NOTE_START: &str = "<!-- paddock: ";
const NOTE_END: &str = " -->";

/// One task of `tasks.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub id: String,
    /// The line index in the file, from 0.
    pub line: usize,
    /// The checkbox: `[x]` or `[X]`.
    pub checked: bool,
    /// `[P]`: the task can run beside the other `[P]` tasks next to it.
    pub parallel: bool,
    /// The story label, for example `US1`.
    pub story: Option<String>,
    /// The text after the id and the labels, without paddock's note.
    pub text: String,
    /// The phase: the index of its level-2 heading, from 0.
    pub phase: usize,
    /// Task ids that the text names after "depends on".
    pub deps: Vec<String>,
}

/// A level-2 heading of `tasks.md`, with the lines that say what the phase
/// must deliver (`**Goal**`, `**Independent Test**`, `**Checkpoint**`,
/// `**Purpose**`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Phase {
    pub title: String,
    pub goals: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Doc {
    pub tasks: Vec<Task>,
    pub phases: Vec<Phase>,
}

impl Doc {
    pub fn task(&self, id: &str) -> Option<&Task> {
        self.tasks.iter().find(|t| t.id == id)
    }

    pub fn phase_title(&self, t: &Task) -> String {
        self.phases
            .get(t.phase)
            .map(|p| p.title.clone())
            .unwrap_or_default()
    }
}

/// A task id: `T` and digits.
pub fn is_id(s: &str) -> bool {
    s.len() > 1 && s.starts_with('T') && s[1..].bytes().all(|b| b.is_ascii_digit())
}

/// The parts of a task line: indent and bullet, checkbox state, the rest.
fn split_checkbox(line: &str) -> Option<(usize, bool, &str)> {
    let trimmed = line.trim_start();
    let indent = line.len() - trimmed.len();
    let rest = trimmed
        .strip_prefix("- [")
        .or_else(|| trimmed.strip_prefix("* ["))?;
    let mut ch = rest.chars();
    let mark = ch.next()?;
    let checked = match mark {
        ' ' => false,
        'x' | 'X' => true,
        _ => return None,
    };
    let after = ch.as_str().strip_prefix(']')?;
    // indent + "- [" is the offset of the mark.
    Some((indent + 3, checked, after))
}

/// Remove paddock's note from the end of a line. Returns the line without
/// it and the note text.
pub fn strip_note(line: &str) -> (&str, Option<&str>) {
    let body = line.trim_end_matches(['\n', '\r']);
    if let Some(at) = body.rfind(NOTE_START) {
        if body.ends_with(NOTE_END) && at + NOTE_START.len() <= body.len() - NOTE_END.len() {
            let note = &body[at + NOTE_START.len()..body.len() - NOTE_END.len()];
            return (body[..at].trim_end(), Some(note));
        }
    }
    (body, None)
}

fn deps_of(text: &str) -> Vec<String> {
    let lower = text.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(at) = lower[from..].find("depends on") {
        let start = from + at + "depends on".len();
        for word in text[start..].split(|c: char| c.is_whitespace() || c == ',') {
            let w = word.trim_matches(|c: char| matches!(c, '(' | ')' | '.' | ';' | ':'));
            if w.is_empty() || w.eq_ignore_ascii_case("and") {
                continue;
            }
            if !is_id(w) {
                break;
            }
            if !out.iter().any(|d| d == w) {
                out.push(w.to_string());
            }
            if word.ends_with(')') || word.ends_with('.') || word.ends_with(';') {
                break;
            }
        }
        from = start;
    }
    out
}

/// Read the tasks of a `tasks.md`. A repeated id counts once: the first.
pub fn parse(text: &str) -> Doc {
    let mut doc = Doc::default();
    let (mut fence, mut comment) = (false, false);
    for (n, raw) in text.lines().enumerate() {
        let line = raw.trim_end();
        let t = line.trim_start();
        if t.starts_with("```") || t.starts_with("~~~") {
            fence = !fence;
            continue;
        }
        if fence {
            continue;
        }
        if comment {
            if t.contains("-->") {
                comment = false;
            }
            continue;
        }
        if t.starts_with("<!--") && !t.contains("-->") {
            comment = true;
            continue;
        }
        if let Some(h) = t.strip_prefix("## ") {
            doc.phases.push(Phase {
                title: h.trim().to_string(),
                goals: Vec::new(),
            });
            continue;
        }
        for label in [
            "**Goal**",
            "**Independent Test**",
            "**Checkpoint**",
            "**Purpose**",
        ] {
            if t.starts_with(label) {
                if let Some(p) = doc.phases.last_mut() {
                    p.goals.push(t.replace("**", ""));
                }
            }
        }
        let Some((_, checked, after)) = split_checkbox(line) else {
            continue;
        };
        let (body, _) = strip_note(after);
        let mut words = body.split_whitespace().peekable();
        let Some(id) = words.next().filter(|w| is_id(w)) else {
            continue;
        };
        if doc.tasks.iter().any(|t| t.id == id) {
            log::warn!("tasks.md: task {id} appears twice; paddock uses the first");
            continue;
        }
        let (mut parallel, mut story) = (false, None);
        let mut rest = body.trim_start()[id.len()..].trim_start();
        while let Some(label) = rest
            .strip_prefix('[')
            .and_then(|r| r.split_once(']'))
            .filter(|(l, _)| !l.is_empty() && !l.contains(' '))
        {
            if label.0 == "P" {
                parallel = true;
            } else if story.is_none() {
                story = Some(label.0.to_string());
            }
            rest = label.1.trim_start();
        }
        doc.tasks.push(Task {
            id: id.to_string(),
            line: n,
            checked,
            parallel,
            story,
            text: rest.to_string(),
            phase: doc.phases.len().saturating_sub(1),
            deps: deps_of(rest),
        });
    }
    doc
}

/// What paddock writes on one task line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mark {
    pub checked: bool,
    /// The trailing note. None: no note.
    pub note: Option<String>,
}

/// Write `marks` into the task lines of `text`. A line keeps every byte
/// but its checkbox mark and paddock's note. A task without a mark stays
/// as it is.
pub fn render(text: &str, marks: &BTreeMap<String, Mark>) -> String {
    let doc = parse(text);
    let by_line: BTreeMap<usize, &Mark> = doc
        .tasks
        .iter()
        .filter_map(|t| marks.get(&t.id).map(|m| (t.line, m)))
        .collect();
    let mut out = String::with_capacity(text.len() + 64);
    for (n, raw) in text.split_inclusive('\n').enumerate() {
        let Some(m) = by_line.get(&n) else {
            out.push_str(raw);
            continue;
        };
        let end = &raw[raw.trim_end_matches(['\n', '\r']).len()..];
        let (body, _) = strip_note(raw);
        let Some((at, _, _)) = split_checkbox(body) else {
            out.push_str(raw);
            continue;
        };
        let current = &body[at..at + 1];
        let want = if !m.checked {
            " "
        } else if current == "x" {
            // A user's lower-case mark stays.
            "x"
        } else {
            "X"
        };
        out.push_str(&body[..at]);
        out.push_str(want);
        out.push_str(&body[at + 1..]);
        if let Some(note) = m.note.as_deref().map(clean_note).filter(|n| !n.is_empty()) {
            out.push(' ');
            out.push_str(NOTE_START);
            out.push_str(&note);
            out.push_str(NOTE_END);
        }
        out.push_str(end);
    }
    out
}

/// A note is one short line and cannot end the comment early.
fn clean_note(note: &str) -> String {
    let one: String = note
        .replace("--", "-")
        .replace('>', ")")
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(120)
        .collect();
    one.trim().to_string()
}
