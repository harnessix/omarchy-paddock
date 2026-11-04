use super::*;
use std::sync::{Arc, Barrier};

fn record(state: board::State, expiry: Option<u64>) -> Record {
    Record {
        task: board::Record {
            state,
            branch: Some("branch".into()),
            result: Some("result".into()),
            review_note: Some("note".into()),
            ..Default::default()
        },
        lease_expiry: expiry,
    }
}

#[test]
fn round_trip_and_missing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("records/tasks.json");
    assert!(load(&path).unwrap().is_empty());
    let expected = record(board::State::Review, Some(15));
    update(&path, |map| {
        map.insert("T001".into(), expected.clone());
    })
    .unwrap();
    assert_eq!(load(&path).unwrap()["T001"], expected);
}

#[test]
fn concurrent_updates_keep_both_changes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tasks.json");
    let barrier = Arc::new(Barrier::new(2));
    let threads: Vec<_> = (0..2)
        .map(|n| {
            let path = path.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                for _ in 0..40 {
                    update(&path, |map| {
                        map.entry(format!("T{n}")).or_default().task.attempts += 1;
                    })
                    .unwrap();
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
    let map = load(&path).unwrap();
    assert_eq!(map["T0"].task.attempts, 40);
    assert_eq!(map["T1"].task.attempts, 40);
}

#[test]
fn normalize_only_expired_held_records() {
    let mut map = BTreeMap::from([
        ("running".into(), record(board::State::Running, Some(10))),
        ("claimed".into(), record(board::State::Claimed, Some(9))),
        ("live".into(), record(board::State::Running, Some(11))),
        ("missing".into(), record(board::State::Claimed, None)),
        ("review".into(), record(board::State::Review, Some(1))),
        ("done".into(), record(board::State::Done, Some(1))),
    ]);
    normalize(&mut map, 10);
    for key in ["running", "claimed"] {
        let r = &map[key];
        assert_eq!(r.task.state, board::State::Todo);
        assert_eq!(r.task.branch.as_deref(), Some("branch"));
        assert_eq!(r.task.result.as_deref(), Some("result"));
        assert_eq!(r.task.review_note.as_deref(), Some("note"));
        assert_eq!(r.lease_expiry, None);
    }
    assert_eq!(map["live"].task.state, board::State::Running);
    assert_eq!(map["missing"].task.state, board::State::Claimed);
    assert_eq!(map["review"].task.state, board::State::Review);
    assert_eq!(map["done"].task.state, board::State::Done);
}

#[test]
fn paths_follow_store() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    assert!(std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&repo)
        .status()
        .unwrap()
        .success());
    let store = dir.path().join("store");
    for root in [&repo, &store] {
        std::fs::create_dir_all(root.join("specs/001-feature")).unwrap();
        std::fs::write(root.join("specs/001-feature/tasks.md"), "").unwrap();
    }
    for (root, external, expected) in [
        (
            &repo,
            false,
            repo.join(".git/paddock/spec/001-feature/tasks.json"),
        ),
        (
            &store,
            true,
            store.join(".paddock/spec/001-feature/tasks.json"),
        ),
    ] {
        assert_eq!(
            ProjectRecords::path(&Layout {
                root: root.clone(),
                external
            }),
            expected
        );
    }
}
