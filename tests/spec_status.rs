//! Host status reads the project record even when no run is registered.
use paddock::speckit::{board, records};
use std::process::Command;

struct World {
    tmp: tempfile::TempDir,
}

impl World {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("project/.specify")).unwrap();
        std::fs::create_dir_all(root.join("project/specs/001-test")).unwrap();
        std::fs::write(
            root.join("project/.specify/feature.json"),
            r#"{"feature_directory":"specs/001-test"}"#,
        )
        .unwrap();
        std::fs::write(
            root.join("project/specs/001-test/tasks.md"),
            "# Tasks\n\n## Phase 1\n\n- [ ] T001 First\n- [ ] T002 Second\n- [ ] T003 Third\n",
        )
        .unwrap();
        Self { tmp }
    }

    fn status(&self, json: bool) -> String {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_paddock"));
        cmd.env_clear()
            .env("HOME", self.tmp.path().join("home"))
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .current_dir(self.tmp.path().join("project"))
            .args(["spec", "status"]);
        if json {
            cmd.arg("--json");
        }
        let output = cmd.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn records(&self) {
        let path = self
            .tmp
            .path()
            .join("project/.paddock/spec/001-test/tasks.json");
        let expiry = paddock::state::now() + 600;
        records::update(&path, |map| {
            for (id, state, lease_expiry) in [
                ("T001", board::State::Review, None),
                ("T002", board::State::Running, Some(expiry)),
                ("T003", board::State::Running, Some(1)),
            ] {
                let task = board::Record {
                    id: id.into(),
                    feature: "001-test".into(),
                    state,
                    sandbox: Some("worker-1".into()),
                    lease: (state == board::State::Running).then(|| "lease:spec/test".into()),
                    branch: Some("work-branch".into()),
                    result: Some("implemented".into()),
                    review_note: Some("check tests".into()),
                    ..Default::default()
                };
                map.insert(id.into(), records::Record { task, lease_expiry });
            }
        })
        .unwrap();
    }
}

#[test]
fn project_records_show_review_and_live_and_expired_leases() {
    let w = World::new();
    w.records();
    let text = w.status(false);
    for expected in [
        "review on work-branch",
        "result: implemented",
        "review note: check tests",
        "running by worker-1 on work-branch",
        "lease: lease:spec/test (expires",
    ] {
        assert!(text.contains(expected), "missing {expected}: {text}");
    }
    assert!(!text.contains("no run has worked"), "{text}");
    let json: serde_json::Value = serde_json::from_str(&w.status(true)).unwrap();
    let tasks = json["tasks"].as_array().unwrap();
    assert_eq!(tasks[0]["state"], "review");
    assert_eq!(tasks[0]["branch"], "work-branch");
    assert_eq!(tasks[0]["result"], "implemented");
    assert_eq!(tasks[0]["review_note"], "check tests");
    assert_eq!(tasks[1]["state"], "running");
    assert_eq!(tasks[1]["sandbox"], "worker-1");
    assert!(tasks[1]["lease_expiry"].as_u64().unwrap() > paddock::state::now());
    assert_eq!(tasks[2]["state"], "todo");
    assert!(tasks[2]["lease"].is_null());
    assert!(tasks[2]["lease_expiry"].is_null());
}

#[test]
fn missing_project_file_reports_tasks_md_origin() {
    let w = World::new();
    assert!(w
        .status(false)
        .contains("no run has worked on these tasks yet: the states come from tasks.md"));
}
