//! Tests for the parser and renderer of tasks.md, the task state machine,
//! leases, the store layout and the git exclusion.

use super::board::{self, Board, Event, Record, State};
use super::tasks::{self, Mark};
use super::*;
use std::collections::BTreeMap;

/// The output of `/speckit-tasks` from the Spec Kit tasks template: the
/// header, the template's comment with sample tasks, phases, `[P]` and
/// story labels, a dependency, and the parallel example in a code fence.
pub const TASKS_MD: &str = r#"---

description: "Task list template for feature implementation"
---

# Tasks: Photo Albums

**Input**: Design documents from `/specs/001-photo-albums/`

**Prerequisites**: plan.md (required), spec.md (required for user stories), research.md, data-model.md, contracts/

## Format: `[ID] [P?] [Story] Description`

- **[P]**: Can run in parallel (different files, no dependencies)
- **[Story]**: Which user story this task belongs to (e.g., US1, US2, US3)

<!--
  ============================================================================
  IMPORTANT: The tasks below are SAMPLE TASKS for illustration purposes only.

  - [ ] T999 Sample task that must not count
  ============================================================================
-->

## Phase 1: Setup (Shared Infrastructure)

**Purpose**: Project initialization and basic structure

- [x] T001 Create project structure per implementation plan
- [ ] T002 Initialize Python project with FastAPI dependencies
- [ ] T003 [P] Configure linting and formatting tools

---

## Phase 2: User Story 1 - Organize photos (Priority: P1) 🎯 MVP

**Goal**: Users group photos into albums by date

**Independent Test**: Create an album and drag a photo into it

### Implementation for User Story 1

- [ ] T010 [P] [US1] Create Album model in src/models/album.py
- [ ] T011 [P] [US1] Create Photo model in src/models/photo.py
- [ ] T012 [US1] Implement AlbumService in src/services/album.py (depends on T010, T011)
- [ ] T013 [US1] Add the album endpoint in src/api/albums.py

**Checkpoint**: At this point, User Story 1 should be fully functional and testable independently

---

## Dependencies & Execution Order

- **Setup (Phase 1)**: No dependencies - can start immediately

## Parallel Example: User Story 1

```bash
- [ ] T500 Task in a fence
Task: "Create Album model in src/models/album.py"
```
"#;

pub const SPEC_MD: &str = r#"# Feature Specification: Photo Albums

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Organize photos (Priority: P1)

Users sort photos into albums.

**Why this priority**: The core of the product.

**Independent Test**: Can be fully tested by making an album and moving a photo

**Acceptance Scenarios**:

1. **Given** an album, **When** I drop a photo on it, **Then** the photo is in the album
2. **Given** two albums, **When** I sort by date, **Then** the newest comes first

---

### User Story 2 - Share albums (Priority: P2)

Users share albums.
"#;

#[test]
fn the_parser_reads_tasks_phases_labels_and_dependencies() {
    let d = tasks::parse(TASKS_MD);
    let ids: Vec<&str> = d.tasks.iter().map(|t| t.id.as_str()).collect();
    // Not the sample in the comment, not the one in the code fence.
    assert_eq!(
        ids,
        ["T001", "T002", "T003", "T010", "T011", "T012", "T013"]
    );
    let t = |id: &str| d.task(id).unwrap();
    assert!(t("T001").checked && !t("T002").checked);
    assert!(t("T003").parallel && !t("T002").parallel);
    assert_eq!(t("T010").story.as_deref(), Some("US1"));
    assert_eq!(t("T010").text, "Create Album model in src/models/album.py");
    assert_eq!(t("T012").deps, ["T010", "T011"]);
    assert_eq!(t("T001").phase, t("T003").phase);
    assert!(t("T010").phase > t("T003").phase);
    assert_eq!(
        d.phase_title(t("T010")),
        "Phase 2: User Story 1 - Organize photos (Priority: P1) 🎯 MVP"
    );
    let goals = &d.phases[t("T010").phase].goals;
    assert!(
        goals
            .iter()
            .any(|g| g == "Independent Test: Create an album and drag a photo into it"),
        "{goals:?}"
    );
    assert!(goals.iter().any(|g| g.starts_with("Checkpoint:")));
    assert!(tasks::is_id("T001") && !tasks::is_id("TXXX") && !tasks::is_id("T"));
}

#[test]
fn the_renderer_changes_only_the_checkbox_and_the_note() {
    let mut marks = BTreeMap::new();
    marks.insert(
        "T002".to_string(),
        Mark {
            checked: true,
            note: Some("paddock/spawn-codex-1".into()),
        },
    );
    marks.insert(
        "T010".to_string(),
        Mark {
            checked: false,
            note: Some("running, paddock/spawn-codex-2".into()),
        },
    );
    // A user's lower-case mark stays lower case.
    marks.insert(
        "T001".to_string(),
        Mark {
            checked: true,
            note: None,
        },
    );
    let out = tasks::render(TASKS_MD, &marks);
    let changed: Vec<(&str, &str)> = TASKS_MD
        .lines()
        .zip(out.lines())
        .filter(|(a, b)| a != b)
        .collect();
    assert_eq!(
        changed,
        [
            (
                "- [ ] T002 Initialize Python project with FastAPI dependencies",
                "- [X] T002 Initialize Python project with FastAPI dependencies <!-- paddock: paddock/spawn-codex-1 -->"
            ),
            (
                "- [ ] T010 [P] [US1] Create Album model in src/models/album.py",
                "- [ ] T010 [P] [US1] Create Album model in src/models/album.py <!-- paddock: running, paddock/spawn-codex-2 -->"
            ),
        ]
    );
    assert_eq!(out.lines().count(), TASKS_MD.lines().count());
    // The note is not part of the task: parsing the output gives the same tasks.
    let back = tasks::parse(&out);
    assert_eq!(
        back.task("T010").unwrap().text,
        "Create Album model in src/models/album.py"
    );
    assert!(back.task("T002").unwrap().checked);
    // Rendering again replaces the note, and no marks give the file back.
    let mut again = marks.clone();
    again.get_mut("T010").unwrap().note = None;
    let twice = tasks::render(&out, &again);
    assert!(twice.contains("- [ ] T010 [P] [US1] Create Album model in src/models/album.py\n"));
    assert_eq!(tasks::render(TASKS_MD, &BTreeMap::new()), TASKS_MD);
    // CRLF line ends stay.
    let crlf = "- [ ] T001 One\r\n- [ ] T002 Two\r\n";
    let one = BTreeMap::from([(
        "T002".to_string(),
        Mark {
            checked: true,
            note: None,
        },
    )]);
    assert_eq!(
        tasks::render(crlf, &one),
        "- [ ] T001 One\r\n- [X] T002 Two\r\n"
    );
    // A note cannot close the comment early.
    let evil = BTreeMap::from([(
        "T001".to_string(),
        Mark {
            checked: false,
            note: Some("a --> b".into()),
        },
    )]);
    assert!(tasks::render(crlf, &evil).starts_with("- [ ] T001 One <!-- paddock: a -) b -->\r\n"));
}

#[test]
fn the_state_machine_allows_only_its_transitions() {
    use State::*;
    assert_eq!(board::next(Todo, Event::Claim).unwrap(), Claimed);
    assert_eq!(board::next(Failed, Event::Claim).unwrap(), Claimed);
    assert_eq!(board::next(Claimed, Event::Start).unwrap(), Running);
    assert_eq!(board::next(Running, Event::Finish(true)).unwrap(), Review);
    assert_eq!(board::next(Claimed, Event::Finish(false)).unwrap(), Failed);
    assert_eq!(board::next(Running, Event::Free).unwrap(), Todo);
    assert_eq!(board::next(Review, Event::Verdict(true)).unwrap(), Done);
    assert_eq!(board::next(Review, Event::Verdict(false)).unwrap(), Todo);
    assert_eq!(board::next(Running, Event::Checked).unwrap(), Done);
    assert_eq!(board::next(Done, Event::Unchecked).unwrap(), Todo);
    assert_eq!(board::next(Review, Event::Unchecked).unwrap(), Review);
    for (s, e) in [
        (Running, Event::Claim),
        (Done, Event::Claim),
        (Review, Event::Claim),
        (Todo, Event::Start),
        (Todo, Event::Finish(true)),
        (Todo, Event::Verdict(true)),
        (Running, Event::Verdict(true)),
        (Done, Event::Free),
    ] {
        assert!(board::next(s, e).is_err(), "{s:?} {e:?}");
    }
}

fn records(doc: &tasks::Doc) -> BTreeMap<String, Record> {
    let mut r = BTreeMap::new();
    board::merge("f", doc, &mut r, &|_| false, 1);
    r
}

#[test]
fn ready_tasks_follow_phases_parallel_runs_and_dependencies() {
    let doc = tasks::parse(TASKS_MD);
    let mut r = records(&doc);
    // T001 is done: T002 is next; T003 [P] waits for T002 (not [P]).
    assert_eq!(board::ready(&doc, &r), ["T002"]);
    r.get_mut("T002").unwrap().state = State::Done;
    assert_eq!(board::ready(&doc, &r), ["T003"]);
    // Phase 2 waits for all of phase 1.
    r.get_mut("T003").unwrap().state = State::Review;
    assert!(board::ready(&doc, &r).is_empty());
    assert_eq!(board::waits_for(&doc, &r, "T010"), ["T003"]);
    r.get_mut("T003").unwrap().state = State::Done;
    // The [P] group starts together.
    assert_eq!(board::ready(&doc, &r), ["T010", "T011"]);
    r.get_mut("T010").unwrap().state = State::Running;
    assert_eq!(board::ready(&doc, &r), ["T011"]);
    r.get_mut("T010").unwrap().state = State::Done;
    r.get_mut("T011").unwrap().state = State::Done;
    assert_eq!(board::ready(&doc, &r), ["T012"]);
    // An explicit dependency counts too, also across the order.
    let d2 = tasks::parse("## P\n- [ ] T001 [P] A (depends on T002)\n- [ ] T002 [P] B\n");
    let r2 = records(&d2);
    assert_eq!(board::ready(&d2, &r2), ["T002"]);
}

#[test]
fn merge_adds_new_tasks_keeps_removed_ones_and_lets_the_user_edit_win() {
    let doc = tasks::parse(TASKS_MD);
    let mut r = records(&doc);
    for rec in r.values_mut() {
        rec.rendered = Some(rec.state == State::Done);
    }
    assert_eq!(r["T001"].state, State::Done);
    // The user checks T002, clears T001, removes T013 and adds T014.
    let edited = TASKS_MD
        .replace("- [ ] T002", "- [x] T002")
        .replace("- [x] T001", "- [ ] T001")
        .replace(
            "- [ ] T013 [US1] Add the album endpoint in src/api/albums.py",
            "- [ ] T014 [US1] Add the share button",
        );
    let d2 = tasks::parse(&edited);
    let changed = board::merge("f", &d2, &mut r, &|_| false, 2);
    assert_eq!(r["T002"].state, State::Done);
    assert_eq!(r["T001"].state, State::Todo);
    assert!(r["T013"].removed);
    assert_eq!(r["T014"].state, State::Todo);
    assert_eq!(r["T014"].text, "Add the share button");
    assert!(changed.contains(&"T013".to_string()) && changed.contains(&"T014".to_string()));
    // A removed task that comes back is a task again.
    board::merge("f", &doc, &mut r, &|_| false, 3);
    assert!(!r["T013"].removed);
}

fn temp_env() -> (tempfile::TempDir, HostEnv) {
    let t = tempfile::tempdir().unwrap();
    let env = HostEnv::from_parts(
        t.path().join("home"),
        "tester".into(),
        1000,
        1000,
        Default::default(),
    );
    (t, env)
}

fn quota() -> crate::kv::store::Quota {
    crate::kv::store::Quota {
        bytes: 1 << 20,
        keys: 1000,
    }
}

#[test]
fn a_lease_keeps_a_second_worker_out_and_its_expiry_frees_the_task() {
    let w = |s: &'static str| crate::kv::store::Writer {
        sandbox: s,
        harness: "codex",
    };
    let (_t, env) = temp_env();
    let b = Board::tree(&crate::kv::kv_root(&env), "claude-root-1", quota()).unwrap();
    let mut st = b.open().unwrap();
    assert!(b
        .claim(&mut st, "f", "T010", &w("codex-spawn-a"), 120, 1000)
        .unwrap());
    // Another worker cannot take it; the owner can renew it.
    assert!(!b
        .claim(&mut st, "f", "T010", &w("codex-spawn-b"), 120, 1001)
        .unwrap());
    assert!(b
        .claim(&mut st, "f", "T010", &w("codex-spawn-a"), 120, 1060)
        .unwrap());
    assert!(b.lease_live(&st, "f", "T010", 1179));
    // The owner died: nobody renewed it, and it expires.
    assert!(!b.lease_live(&st, "f", "T010", 1181));
    let doc = tasks::parse("## P\n- [ ] T010 [P] A\n");
    let mut recs = records(&doc);
    let r = recs.get_mut("T010").unwrap();
    r.state = State::Running;
    r.sandbox = Some("codex-spawn-a".into());
    let live = |r: &Record| b.lease_live(&st, "f", &r.id, 1181);
    let changed = board::merge("f", &doc, &mut recs, &live, 1181);
    assert_eq!(changed, ["T010"]);
    assert_eq!(recs["T010"].state, State::Todo);
    assert_eq!(board::ready(&doc, &recs), ["T010"]);
    assert!(b
        .claim(&mut st, "f", "T010", &w("codex-spawn-b"), 120, 1181)
        .unwrap());
}

#[test]
fn only_records_that_paddock_wrote_count() {
    let (_t, env) = temp_env();
    let b = Board::tree(&crate::kv::kv_root(&env), "claude-root-1", quota()).unwrap();
    let mut st = b.open().unwrap();
    let r = Record {
        id: "T001".into(),
        feature: "f".into(),
        state: State::Done,
        ..Default::default()
    };
    b.put(&mut st, &r, 0, 5).unwrap();
    // A sandbox writes a fake record into the tree scope.
    let fake = Record {
        id: "T002".into(),
        feature: "f".into(),
        state: State::Done,
        ..Default::default()
    };
    let w = crate::kv::store::Writer {
        sandbox: "codex-spawn-x",
        harness: "codex",
    };
    st.put(
        &b.ns,
        &board::key("f", "T002"),
        &serde_json::to_string(&fake).unwrap(),
        None,
        None,
        &w,
        quota(),
        5,
    )
    .unwrap();
    let loaded = b.load(&st, "f", 6).unwrap();
    assert_eq!(loaded.keys().collect::<Vec<_>>(), ["T001"]);
}

fn write_cfg(env: &HostEnv, text: &str) {
    let d = env.paddock_config();
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("config.toml"), text).unwrap();
}

#[test]
fn the_config_is_user_only_and_has_defaults() {
    assert_eq!(
        Cfg::parse("").unwrap(),
        Cfg {
            enabled: true,
            store: Store::Repo
        }
    );
    let c = Cfg::parse("[speckit]\nenabled = false\nstore = \"external\"\n").unwrap();
    assert_eq!(
        c,
        Cfg {
            enabled: false,
            store: Store::External
        }
    );
    assert!(Cfg::parse("[speckit]\nstore = \"cloud\"\n").is_err());
    assert!(crate::manifest::USER_ONLY.contains(&"speckit"));
}

#[test]
fn the_layout_follows_the_folder_the_config_and_the_store() {
    let (t, env) = temp_env();
    let project = t.path().join("app");
    std::fs::create_dir_all(project.join(".specify")).unwrap();
    std::fs::create_dir_all(project.join("specs/001-a")).unwrap();
    std::fs::create_dir_all(project.join("specs/002-b")).unwrap();
    std::fs::write(project.join("specs/001-a/tasks.md"), "").unwrap();
    std::fs::write(project.join("specs/002-b/tasks.md"), "").unwrap();
    let l = Layout::resolve(&env, &project).unwrap();
    assert_eq!(
        l,
        Layout {
            root: project.clone(),
            external: false
        }
    );
    // The last feature, unless feature.json names one.
    assert_eq!(l.feature().as_deref(), Some("002-b"));
    std::fs::write(
        project.join(".specify/feature.json"),
        r#"{"feature_directory":"specs/001-a"}"#,
    )
    .unwrap();
    assert_eq!(l.feature().as_deref(), Some("001-a"));
    std::fs::write(
        project.join(".specify/feature.json"),
        r#"{"feature_directory":"/x/app/specs/../../etc"}"#,
    )
    .unwrap();
    assert_eq!(l.feature().as_deref(), Some("002-b"));
    // The user turns it off; a project without .specify/ is none.
    write_cfg(&env, "[speckit]\nenabled = false\n");
    assert!(Layout::resolve(&env, &project).is_none());
    write_cfg(&env, "");
    assert!(Layout::resolve(&env, &t.path().join("other")).is_none());
    // A project file cannot turn it on or off.
    std::fs::write(project.join("paddock.toml"), "[speckit]\nenabled = false\n").unwrap();
    assert!(Layout::resolve(&env, &project).is_some());
    // The external store: per repository, in paddock's data folder.
    write_cfg(&env, "[speckit]\nstore = \"external\"\n");
    let store = store_dir(&env, &project);
    assert!(store.starts_with(env.paddock_data().join("speckit")));
    assert_eq!(store.file_name().unwrap().len(), 16);
    // Not adopted yet: the files in the project still count.
    assert!(!Layout::resolve(&env, &project).unwrap().external);
    std::fs::create_dir_all(store.join(".specify")).unwrap();
    assert_eq!(
        Layout::resolve(&env, &project).unwrap(),
        Layout {
            root: store,
            external: true
        }
    );
}

fn new_spec(project: &Path, tmp: &Path) -> crate::spec::SandboxSpec {
    crate::spec::SandboxSpec::new(
        "s-1".into(),
        "fake",
        project,
        tmp.join("home/h"),
        &tmp.join("home"),
        1000,
        1000,
        "tester",
    )
}

fn git(dir: &Path, args: &[&str]) -> String {
    let o = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@localhost",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        o.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn the_external_store_stays_out_of_git_in_the_repo_and_each_worktree() {
    let (t, env) = temp_env();
    write_cfg(&env, "[speckit]\nstore = \"external\"\n");
    let repo = t.path().join("app");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README"), "x\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    let exclude_before =
        std::fs::read_to_string(repo.join(".git/info/exclude")).unwrap_or_default();
    // The store, as adopt leaves it.
    let store = store_dir(&env, &repo);
    std::fs::create_dir_all(store.join(".specify/memory")).unwrap();
    std::fs::create_dir_all(store.join("specs/001-a")).unwrap();
    std::fs::write(store.join("specs/001-a/tasks.md"), TASKS_MD).unwrap();
    let wt = t.path().join("wt");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "paddock/x",
            &wt.display().to_string(),
        ],
    );
    for (project, key) in [(&repo, &repo), (&wt, &repo)] {
        let mut spec = new_spec(project, t.path());
        mount(&env, &mut spec, key).unwrap();
        let m: Vec<_> = spec
            .mounts
            .iter()
            .filter(|m| m.origin == "spec kit store")
            .collect();
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].src.as_deref(), Some(store.join(".specify").as_path()));
        assert_eq!(m[0].dst, project.join(".specify"));
        assert_eq!(m[1].dst, project.join("specs"));
        assert_eq!(m[0].kind, crate::spec::MountKind::Rw);
        // A run writes into the mount points; here, the same files appear.
        std::fs::create_dir_all(project.join("specs/001-a")).unwrap();
        std::fs::write(project.join("specs/001-a/tasks.md"), TASKS_MD).unwrap();
        std::fs::write(project.join(".specify/feature.json"), "{}").unwrap();
        assert_eq!(
            git(project, &["status", "--porcelain"]),
            "",
            "{}",
            project.display()
        );
    }
    // The worktree resolves to the same store.
    assert_eq!(Layout::resolve(&env, &wt).unwrap().root, store);
    // One info/exclude covers both, and nothing tracked changed.
    let ex = std::fs::read_to_string(repo.join(".git/info/exclude")).unwrap();
    assert!(ex.starts_with(&exclude_before));
    assert_eq!(ex.matches("/.specify/").count(), 1, "{ex}");
    assert!(!exclude(&repo).unwrap(), "a second run adds nothing");
    assert_eq!(git(&repo, &["diff", "HEAD", "--stat"]), "");
    // With the repo store, nothing is mounted.
    write_cfg(&env, "");
    let mut spec = new_spec(&repo, t.path());
    mount(&env, &mut spec, &repo).unwrap();
    assert!(spec.mounts.iter().all(|m| m.origin != "spec kit store"));
}

#[test]
fn adopt_moves_the_files_after_the_question_and_explains_the_history() {
    let (t, env) = temp_env();
    let repo = t.path().join("app");
    std::fs::create_dir_all(repo.join(".specify/memory")).unwrap();
    std::fs::create_dir_all(repo.join("specs/001-a")).unwrap();
    std::fs::write(repo.join(".specify/memory/constitution.md"), "# C\n").unwrap();
    std::fs::write(repo.join("specs/001-a/tasks.md"), TASKS_MD).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    // The repo store refuses.
    assert!(adopt::adopt(&env, &repo, &mut |_| true).is_err());
    write_cfg(&env, "[speckit]\nstore = \"external\"\n");
    // No: nothing moves.
    assert_eq!(
        adopt::adopt(&env, &repo, &mut |_| false).unwrap(),
        "Nothing moved."
    );
    assert!(repo.join(".specify").is_dir());
    let mut asked = String::new();
    let msg = adopt::adopt(&env, &repo, &mut |q| {
        asked = q.to_string();
        true
    })
    .unwrap();
    assert!(asked.contains(".specify/ and specs/"), "{asked}");
    let store = store_dir(&env, &repo);
    assert_eq!(
        std::fs::read_to_string(store.join("specs/001-a/tasks.md")).unwrap(),
        TASKS_MD
    );
    assert!(store.join(".specify/memory/constitution.md").is_file());
    assert!(!repo.join(".specify").exists() && !repo.join("specs").exists());
    assert!(
        msg.contains("git rm -r -q --cached .specify specs"),
        "{msg}"
    );
    assert!(msg.contains("filter-repo"), "{msg}");
    // A second adopt finds nothing to move.
    assert!(adopt::adopt(&env, &repo, &mut |_| true).is_err());
}

#[test]
fn the_acceptance_text_comes_from_the_phase_and_the_story() {
    let (t, _env) = temp_env();
    let root = t.path().join("p");
    std::fs::create_dir_all(root.join("specs/001-a")).unwrap();
    std::fs::write(root.join("specs/001-a/spec.md"), SPEC_MD).unwrap();
    let layout = Layout {
        root,
        external: true,
    };
    let doc = tasks::parse(TASKS_MD);
    let a = prompt::acceptance(&layout, "001-a", &doc, doc.task("T010").unwrap());
    assert!(
        a.contains("Goal: Users group photos into albums by date"),
        "{a}"
    );
    assert!(
        a.contains("1. Given an album, When I drop a photo on it"),
        "{a}"
    );
    assert!(!a.contains("Share albums"), "{a}");
    let s = prompt::story_section(SPEC_MD, "US2").unwrap();
    assert!(s.starts_with("### User Story 2") && !s.contains("Story 1"));
    assert!(prompt::story_section(SPEC_MD, "US9").is_none());
}
