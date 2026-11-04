//! End to end: a sandbox in a Spec Kit project starts workers with
//! `spawn --task next`. The workers are a shell script.

use super::board::{self, State};
use super::flow;
use crate::broker::{
    Answer, Approver, Broker, Context, HostPolicy, Pending, Request, Status, PROTOCOL,
};
use crate::env::HostEnv;
use crate::spawn::{Ceiling, SpawnCfg, Spawner};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

const TASKS: &str = "# Tasks: Albums\n\n## Phase 1: Models\n\n**Purpose**: The data\n\n- [ ] T001 [P] [US1] Create Album model in src/album.py\n- [ ] T002 [P] [US1] Create Photo model in src/photo.py\n- [ ] T003 [US1] Album service in src/service.py (depends on T001, T002)\n";

struct Yes;

impl Approver for Yes {
    fn ask(&self, _: &HostEnv, _: &Pending, _: Duration) -> Option<Answer> {
        Some(Answer::Once)
    }
}

struct World {
    _tmp: tempfile::TempDir,
    env: HostEnv,
    project: PathBuf,
    seen: PathBuf,
}

fn world() -> World {
    let tmp = tempfile::tempdir().unwrap();
    let mut vars = BTreeMap::new();
    vars.insert("PATH".into(), std::env::var("PATH").unwrap_or_default());
    vars.insert(
        "XDG_RUNTIME_DIR".into(),
        tmp.path().join("run").display().to_string(),
    );
    std::fs::create_dir_all(tmp.path().join("run")).unwrap();
    let env = HostEnv::from_parts(tmp.path().join("home"), "tester".into(), 1000, 1000, vars);
    let project = tmp.path().join("work/app");
    std::fs::create_dir_all(project.join(".specify/memory")).unwrap();
    std::fs::create_dir_all(project.join("specs/001-albums")).unwrap();
    std::fs::write(
        project.join(".specify/memory/constitution.md"),
        "# Constitution\nTest first.\n",
    )
    .unwrap();
    std::fs::write(
        project.join(".specify/feature.json"),
        r#"{"feature_directory":"specs/001-albums"}"#,
    )
    .unwrap();
    std::fs::write(project.join("specs/001-albums/tasks.md"), TASKS).unwrap();
    std::fs::write(
        project.join("specs/001-albums/spec.md"),
        super::tests::SPEC_MD,
    )
    .unwrap();
    let seen = tmp.path().join("seen");
    std::fs::create_dir_all(&seen).unwrap();
    World {
        _tmp: tmp,
        env,
        project,
        seen,
    }
}

fn ceiling(parent: &str, project: &Path) -> Ceiling {
    Ceiling {
        parent: parent.into(),
        parent_harness: "claude".into(),
        chain: vec![parent.into()],
        depth: 0,
        repo: project.to_path_buf(),
        head_from: project.to_path_buf(),
        backend: "bwrap".into(),
        network: crate::spec::Network::None,
        lan: false,
        inherit_env: false,
        env_names: BTreeSet::new(),
        allow_domains: BTreeSet::new(),
        host_ports: BTreeSet::new(),
        mounts: vec![],
        manifest: Default::default(),
        modes: HostPolicy::default().modes,
        auth: "shared".into(),
        devices: vec![],
    }
}

fn broker(w: &World, id: &str, chain: Vec<String>, script: Option<&str>) -> Broker {
    let ctx = Context {
        id: id.into(),
        harness: "claude".into(),
        project: w.project.clone(),
        repo: None,
        depth: chain.len() as u32,
        chain,
        role: None,
    };
    let mut b = Broker::new(w.env.clone(), ctx, HostPolicy::default(), Arc::new(Yes));
    if let Some(script) = script {
        let cfg = SpawnCfg {
            harnesses: vec!["codex".into()],
            max_running: 2,
            ..Default::default()
        };
        let program: Vec<OsString> = ["sh", "-c", script, "fake-child"]
            .iter()
            .map(OsString::from)
            .collect();
        b.spawner = Some(Arc::new(Spawner::with_program(
            &w.env,
            cfg,
            ceiling(id, &w.project),
            &w.project,
            program,
        )));
    }
    b
}

fn req(cap: &str, args: serde_json::Value) -> Request {
    Request {
        v: PROTOCOL,
        cap: cap.into(),
        args: args
            .as_object()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .collect(),
        reason: "the next task".into(),
    }
}

fn records(w: &World, root: &str) -> BTreeMap<String, board::Record> {
    let b = board::Board::tree(&crate::kv::kv_root(&w.env), root, flow::quota(&w.env)).unwrap();
    b.load(&b.open().unwrap(), "001-albums", crate::state::now())
        .unwrap()
        .into_iter()
        .map(|(k, (r, _))| (k, r))
        .collect()
}

fn tasks_md(w: &World) -> String {
    std::fs::read_to_string(w.project.join("specs/001-albums/tasks.md")).unwrap()
}

#[test]
fn two_parallel_tasks_start_at_once_each_claimed_once_with_results_and_checkboxes() {
    let w = world();
    // Each worker keeps its prompt, waits until both run, and ends well.
    let script = format!(
        "d=$(dirname \"$1\"); cp \"$d/prompt.md\" {seen}/$(basename \"$d\").md; \
         touch {seen}/$(basename \"$d\").up; \
         i=0; while [ $(ls {seen}/*.up | wc -l) -lt 2 ] && [ $i -lt 100 ]; do sleep 0.05; i=$((i+1)); done; \
         [ $(ls {seen}/*.up | wc -l) -ge 2 ] && touch {seen}/$(basename \"$d\").both; \
         printf '{{\"exit_code\":0,\"branch\":\"b\",\"base\":\"c\",\"commits\":1,\"uncommitted\":0,\"stat\":\"\",\"output\":\"ok\",\"output_truncated\":false,\"started\":1,\"ended\":2}}' > \"$d/result.json\"",
        seen = w.seen.display()
    );
    let b = Arc::new(broker(&w, "p-1", vec![], Some(&script)));
    let start = || {
        let b = b.clone();
        std::thread::spawn(move || {
            b.handle(&req(
                "spawn",
                serde_json::json!({"action": "start", "harness": "codex", "task": "next"}),
            ))
        })
    };
    let (one, two) = (start(), start());
    let (one, two) = (one.join().unwrap(), two.join().unwrap());
    assert_eq!(one.status, Status::Done, "{}", one.message);
    assert_eq!(two.status, Status::Done, "{}", two.message);
    let mut got = vec![
        one.result["task"].as_str().unwrap().to_string(),
        two.result["task"].as_str().unwrap().to_string(),
    ];
    got.sort();
    assert_eq!(got, ["T001", "T002"], "each task once");
    // A third start meets the spawn limit of two.
    let third = b.handle(&req(
        "spawn",
        serde_json::json!({"action": "start", "harness": "codex", "task": "next"}),
    ));
    assert!(third.message.contains("max_running"), "{}", third.message);

    let sp = b.spawner.clone().unwrap();
    for r in [&one, &two] {
        let id = r.result["id"].as_str().unwrap();
        let s = sp.wait(id, Duration::from_secs(20)).unwrap();
        assert_eq!(s.exit_code, Some(0), "{}", s.message);
    }
    // Now no task is ready: T003 waits for both, which wait for review.
    // The broker records the end of a child a moment after the child
    // exits, so ask again until it did (a slow CI machine needs it).
    let end = std::time::Instant::now() + Duration::from_secs(20);
    let third = loop {
        let r = b.handle(&req(
            "spawn",
            serde_json::json!({"action": "start", "harness": "codex", "task": "next"}),
        ));
        if r.message.contains("no task of 001-albums is ready") || std::time::Instant::now() > end {
            break r;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(
        third.message.contains("no task of 001-albums is ready"),
        "{}",
        third.message
    );
    // The two workers ran at the same time and each got its own task.
    let prompts: Vec<String> = std::fs::read_dir(&w.seen)
        .unwrap()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "md"))
        .map(|e| std::fs::read_to_string(e.path()).unwrap())
        .collect();
    assert_eq!(prompts.len(), 2);
    let both = std::fs::read_dir(&w.seen)
        .unwrap()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "both"))
        .count();
    assert_eq!(both, 2, "each worker saw the other one run");
    assert!(prompts.iter().any(|p| p.contains("Task T001 [P] [US1]")));
    assert!(prompts.iter().any(|p| p.contains("Task T002 [P] [US1]")));
    for p in &prompts {
        assert!(p.contains("paddock-host done"), "{p}");
        assert!(p.contains("Do not edit tasks.md"), "{p}");
        assert!(p.contains("Constitution"), "{p}");
    }

    // The results are in kv, with one claim each.
    let recs = records(&w, "p-1");
    for id in ["T001", "T002"] {
        let r = &recs[id];
        assert_eq!(r.state, State::Review, "{r:?}");
        assert_eq!(r.attempts, 1);
        assert!(r
            .branch
            .as_deref()
            .unwrap()
            .starts_with("paddock/spawn-codex-"));
        assert!(
            r.result.as_deref().unwrap().contains("exited with code 0"),
            "{r:?}"
        );
        assert!(r.lease.is_none());
    }
    assert_ne!(recs["T001"].sandbox, recs["T002"].sandbox);
    assert_eq!(recs["T003"].state, State::Todo);
    let md = tasks_md(&w);
    assert!(md.contains("- [ ] T001 [P] [US1] Create Album model in src/album.py <!-- paddock: review, paddock/spawn-codex-"), "{md}");

    // The parent reviews: pass marks the task done, changes sends it back.
    let pass = b.handle(&req(
        "spec",
        serde_json::json!({"action": "review", "id": "T001", "verdict": "pass"}),
    ));
    assert_eq!(pass.status, Status::Done, "{}", pass.message);
    let back = b.handle(&req("spec", serde_json::json!({"action": "review", "id": "T002", "verdict": "changes", "text": "add a test"})));
    assert_eq!(back.status, Status::Done, "{}", back.message);
    let md = tasks_md(&w);
    assert!(md.contains("- [X] T001 [P] [US1] Create Album model in src/album.py <!-- paddock: paddock/spawn-codex-"), "{md}");
    assert!(md.contains("- [ ] T002 [P] [US1] Create Photo model in src/photo.py <!-- paddock: changes asked, paddock/spawn-codex-"), "{md}");
    assert!(
        md.contains("- [ ] T003 [US1] Album service in src/service.py (depends on T001, T002)\n"),
        "{md}"
    );
    let status = b.handle(&req("spec", serde_json::json!({"action": "status"})));
    assert_eq!(status.result["ready"], serde_json::json!(["T002"]));
    assert_eq!(status.result["feature"], "001-albums");
    assert!(status.result["text"].as_str().unwrap().contains("T001"));
}

#[test]
fn a_workers_done_report_reaches_its_record_and_a_failure_wins() {
    let w = world();
    let spot = flow::Spot::find(&w.env, &w.project, "p-1", None)
        .unwrap()
        .unwrap();
    let owner = flow::Owner {
        sandbox: "codex-spawn-aa".into(),
        spawn_id: "aa".into(),
        harness: "codex".into(),
        branch: "paddock/spawn-codex-aa".into(),
        timeout: 600,
    };
    let now = crate::state::now();
    let r = flow::claim(&w.env, &spot, "T002", &owner, now).unwrap();
    assert_eq!((r.id.as_str(), r.state), ("T002", State::Claimed));
    assert!(
        r.acceptance.contains("Purpose: The data"),
        "{}",
        r.acceptance
    );
    // T003 waits for both; T002 is taken.
    let other = flow::Owner {
        sandbox: "codex-spawn-bb".into(),
        spawn_id: "bb".into(),
        ..owner.clone()
    };
    assert!(format!(
        "{:#}",
        flow::claim(&w.env, &spot, "T003", &other, now).unwrap_err()
    )
    .contains("waits for T001, T002"));
    assert!(format!(
        "{:#}",
        flow::claim(&w.env, &spot, "T002", &other, now).unwrap_err()
    )
    .contains("claimed"));
    flow::started(&w.env, &spot, "T002", now).unwrap();

    // The worker's broker: a child of p-1.
    let child = broker(&w, "codex-spawn-aa", vec!["p-1".into()], None);
    let done = child.handle(&req(
        "done",
        serde_json::json!({"status": "failed", "text": "the schema is missing"}),
    ));
    assert_eq!(done.status, Status::Done, "{}", done.message);
    assert_eq!(done.result["task"], "T002");
    // Its notes name the task.
    let f = flow::facts(&w.env, &w.project, "p-1", "codex-spawn-aa").unwrap();
    assert_eq!(f.feature, "001-albums");
    assert!(f
        .task
        .as_deref()
        .unwrap()
        .starts_with("T002 Create Photo model"));
    assert!(f.paths.iter().any(|p| p.ends_with("constitution.md")));
    let note = child.handle(&req("spec", serde_json::json!({"action": "note"})));
    assert!(
        note.result["lines"].to_string().contains("your task: T002"),
        "{}",
        note.result
    );

    // The child exits with 0, but it reported a failure.
    let s = crate::spawn::Status {
        id: "aa".into(),
        sandbox: owner.sandbox.clone(),
        harness: "codex".into(),
        branch: owner.branch.clone(),
        state: crate::spawn::State::Exited,
        exit_code: Some(0),
        started: now,
        ended: Some(now),
        inbox: None,
        message: String::new(),
        role: Default::default(),
        limits: Default::default(),
        enforced_by: "paddock".into(),
        from_head: None,
        frozen_s: 0,
        pick: Default::default(),
    };
    flow::child_ended(&w.env, "p-1", &w.project, &s, &Default::default());
    let r = &records(&w, "p-1")["T002"];
    assert_eq!(r.state, State::Failed);
    assert!(
        r.result
            .as_deref()
            .unwrap()
            .starts_with("the schema is missing"),
        "{r:?}"
    );
    assert!(tasks_md(&w).contains("T002 [P] [US1] Create Photo model in src/photo.py <!-- paddock: failed, paddock/spawn-codex-aa -->"));
    let parent = broker(&w, "p-1", vec![], Some("exit 0"));
    let pass = parent.handle(&req(
        "spec",
        serde_json::json!({"action": "review", "id": "T002", "verdict": "pass"}),
    ));
    assert_eq!(pass.status, Status::Failed);
    assert_eq!(pass.message, "T002 is failed: use `changes \"...\"` to give it back to a worker when it is in review or failed; `pass` needs a task in review");
    let back = parent.handle(&req("spec", serde_json::json!({"action": "review", "id": "T002", "verdict": "changes", "text": "schema is ready"})));
    assert_eq!(back.status, Status::Done, "{}", back.message);
    let path = super::records::ProjectRecords::path_for(&spot.layout, &spot.feature);
    let stored = super::records::load(&path).unwrap();
    let reopened = &stored["T002"].task;
    assert_eq!(reopened.state, State::Todo);
    assert_eq!(reopened.review_note.as_deref(), Some("schema is ready"));
    assert_eq!(reopened.branch.as_deref(), Some(owner.branch.as_str()));
    assert!(reopened
        .result
        .as_deref()
        .unwrap()
        .starts_with("the schema is missing"));
    assert_eq!(reopened.attempts, 1);
    let claimed = flow::claim(&w.env, &spot, "T002", &other, now).unwrap();
    assert_eq!(claimed.attempts, 2);
    assert_eq!(claimed.review_note.as_deref(), Some("schema is ready"));
    assert!(super::prompt::worker(&spot.layout, &claimed, false).contains("schema is ready"));
}

#[test]
fn a_user_edit_of_tasks_md_wins_and_a_child_only_reads() {
    let w = world();
    let spot = flow::Spot::find(&w.env, &w.project, "p-1", None)
        .unwrap()
        .unwrap();
    let now = crate::state::now();
    spot.sync(&w.env, true, now).unwrap();
    // The user checks T001 by hand and adds a task.
    let md = tasks_md(&w).replace("- [ ] T001", "- [x] T001") + "- [ ] T004 Docs\n";
    std::fs::write(w.project.join("specs/001-albums/tasks.md"), &md).unwrap();
    let v = spot.sync(&w.env, true, now).unwrap();
    assert_eq!(v.records["T001"].state, State::Done);
    assert_eq!(v.records["T004"].state, State::Todo);
    assert_eq!(v.ready, ["T002"]);
    // The user's lower-case mark stays.
    assert!(tasks_md(&w).contains("- [x] T001"));
    // A child sees the state but does not write.
    let child = broker(&w, "codex-spawn-zz", vec!["p-1".into()], None);
    let st = child.handle(&req("spec", serde_json::json!({"action": "status"})));
    assert_eq!(st.status, Status::Done, "{}", st.message);
    assert!(st.result["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["id"] == "T001" && t["state"] == "done"));
}

mod cross_run;
mod model_hint;
