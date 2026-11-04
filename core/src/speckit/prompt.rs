//! The texts of a task: the prompt of a worker, its acceptance text, and
//! the lines of the session and compaction notes.
//!
//! A worker that sees the external store gets paths. A worker in a
//! worktree of a repository with the files in git can have another
//! version of them, so it gets copies of the parts that its task needs.

use super::board::Record;
use super::tasks::{Doc, Task};
use super::{Layout, SPECIFY, SPECS};
use std::path::{Path, PathBuf};

/// The most text of one file that a prompt copies.
const MAX_COPY: usize = 8 * 1024;
/// The longest acceptance text.
const MAX_ACCEPTANCE: usize = 2000;

pub fn constitution() -> PathBuf {
    Path::new(SPECIFY).join("memory/constitution.md")
}

/// The files of a feature that exist, as paths relative to the project.
pub fn paths(layout: &Layout, feature: &str) -> Vec<(&'static str, PathBuf)> {
    let dir = Path::new(SPECS).join(feature);
    let mut out = vec![("constitution", constitution())];
    for (what, name) in [
        ("spec", "spec.md"),
        ("plan", "plan.md"),
        ("contracts", "contracts"),
        ("data model", "data-model.md"),
        ("research", "research.md"),
        ("quickstart", "quickstart.md"),
        ("tasks (paddock keeps its checkboxes)", "tasks.md"),
    ] {
        out.push((what, dir.join(name)));
    }
    out.retain(|(_, p)| layout.exists(p));
    out
}

/// The number of a story label: `US2` is 2.
fn story_number(story: &str) -> Option<&str> {
    let n = story.strip_prefix("US")?;
    (!n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())).then_some(n)
}

/// The section of `spec.md` for one user story: from its `### User Story N`
/// heading to the next heading of that level or above.
pub fn story_section(spec: &str, story: &str) -> Option<String> {
    let n = story_number(story)?;
    let mut out = Vec::new();
    let mut inside = false;
    for line in spec.lines() {
        let t = line.trim_start();
        let level = t.chars().take_while(|c| *c == '#').count();
        if inside && (1..=3).contains(&level) {
            break;
        }
        if level == 3 {
            let rest = t[3..].trim_start();
            if let Some(after) = rest.strip_prefix("User Story ") {
                let num: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
                inside = num == n;
            }
        }
        if inside {
            out.push(line);
        }
    }
    let text = out
        .join("\n")
        .trim()
        .trim_end_matches("---")
        .trim()
        .to_string();
    (!text.is_empty()).then_some(text)
}

/// The acceptance scenarios and the independent test of a story section.
fn scenarios(section: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_list = false;
    for line in section.lines() {
        let t = line.trim();
        if t.starts_with("**Independent Test**") {
            out.push(t.replace("**", ""));
        } else if t.starts_with("**Acceptance Scenarios**") {
            in_list = true;
        } else if in_list && t.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            out.push(t.replace("**", ""));
        } else if in_list && !t.is_empty() {
            in_list = false;
        }
    }
    out
}

/// What a task must deliver: the goal lines of its phase in `tasks.md`,
/// and the acceptance scenarios of its user story in `spec.md`.
pub fn acceptance(layout: &Layout, feature: &str, doc: &Doc, t: &Task) -> String {
    let mut l: Vec<String> = doc
        .phases
        .get(t.phase)
        .map(|p| p.goals.clone())
        .unwrap_or_default();
    if let Some(story) = &t.story {
        let spec = layout.read(&Path::new(SPECS).join(feature).join("spec.md"));
        if let Some(s) = spec.as_deref().and_then(|s| story_section(s, story)) {
            for line in scenarios(&s) {
                if !l.contains(&line) {
                    l.push(line);
                }
            }
        }
    }
    let mut text = l.join("\n");
    if text.chars().count() > MAX_ACCEPTANCE {
        text = text.chars().take(MAX_ACCEPTANCE).collect();
        text.push_str(" [...]");
    }
    text
}

fn copy(text: &str) -> String {
    if text.len() <= MAX_COPY {
        return text.trim().to_string();
    }
    let mut end = MAX_COPY;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[... cut at {MAX_COPY} bytes]", text[..end].trim())
}

/// The prompt of a worker for task `r`. `sees_store`: the worker sees the
/// Spec Kit files at the same paths (the external store).
pub fn worker(layout: &Layout, r: &Record, sees_store: bool) -> String {
    let mut p = vec![format!(
        "paddock: you work on one task of the Spec Kit feature {}.",
        r.feature
    )];
    let labels = format!(
        "{}{}",
        if r.parallel { " [P]" } else { "" },
        r.story
            .as_ref()
            .map(|s| format!(" [{s}]"))
            .unwrap_or_default()
    );
    p.push(format!("\nTask {}{labels}: {}", r.id, r.text));
    if !r.phase_title.is_empty() {
        p.push(format!("Phase: {}", r.phase_title));
    }
    if !r.acceptance.is_empty() {
        p.push(format!("\nAcceptance:\n{}", r.acceptance));
    }
    let files = paths(layout, &r.feature);
    if sees_store {
        p.push("\nThe Spec Kit files are in this project. Read the parts that this task needs before you change code:".into());
        for (what, path) in &files {
            p.push(format!("- {what}: {}", path.display()));
        }
    } else {
        p.push("\nThe Spec Kit files of the parent run can differ from the ones in your worktree. These are the parts for this task:".into());
        let spec = Path::new(SPECS).join(&r.feature);
        if let Some(c) = layout.read(&constitution()) {
            p.push(format!("\n## Constitution\n{}", copy(&c)));
        }
        let story = r.story.as_deref();
        if let Some(s) = layout.read(&spec.join("spec.md")) {
            let part = story.and_then(|st| story_section(&s, st)).unwrap_or(s);
            p.push(format!("\n## From spec.md\n{}", copy(&part)));
        }
        if let Some(plan) = layout.read(&spec.join("plan.md")) {
            p.push(format!("\n## From plan.md\n{}", copy(&plan)));
        }
        if layout.exists(&spec.join("contracts")) {
            p.push(format!(
                "\nThe contracts are in {} of the parent project.",
                spec.join("contracts").display()
            ));
        }
    }
    if let (Some(v), Some(n)) = (&r.verdict, &r.review_note) {
        if v == "changes" {
            p.push(format!(
                "\nAn earlier attempt got this review. Fix it:\n{n}"
            ));
        }
    }
    p.push(format!(
        "\nRules:\n\
         - Do only task {id}. Commit your work on your branch, with {id} in the commit message.\n\
         - Do not edit tasks.md or other files in specs/ and .specify/. paddock marks the task from your report.\n\
         - When the task is done, run: paddock-host done \"one line: what you did\"\n\
         - When you cannot finish it, run: paddock-host done --status failed \"why\"",
        id = r.id
    ));
    p.join("\n")
}

/// The spec facts of a run, for the session and compaction notes.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Facts {
    pub feature: String,
    /// The task that this run owns, with its text.
    #[serde(default)]
    pub task: Option<String>,
    #[serde(default)]
    pub acceptance: String,
    /// The Spec Kit files, relative to the project.
    #[serde(default)]
    pub paths: Vec<String>,
    /// Ready and review counts, for a run that hands out tasks.
    #[serde(default)]
    pub summary: Option<String>,
}

/// The note lines.
pub fn lines(f: &Facts) -> Vec<String> {
    let mut l = vec![format!("- Spec Kit feature: {}", f.feature)];
    match &f.task {
        Some(t) => {
            l.push(format!(
                "- your task: {t}. Do only this task, and do not edit tasks.md."
            ));
            if !f.acceptance.is_empty() {
                l.push(format!(
                    "- acceptance: {}",
                    f.acceptance.replace('\n', " / ")
                ));
            }
            l.push("- report the end with `paddock-host done \"SUMMARY\"` (or `--status failed \"WHY\"`)".into());
        }
        None => {
            if let Some(s) = &f.summary {
                l.push(format!("- tasks: {s}. `paddock-host spec status` shows them, `paddock-host spawn HARNESS --task next` starts the next one"));
            }
        }
    }
    if !f.paths.is_empty() {
        l.push(format!("- spec files: {}", f.paths.join(", ")));
    }
    l
}
