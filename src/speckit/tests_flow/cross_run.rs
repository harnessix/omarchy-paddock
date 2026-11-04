use super::*;

fn owner(id: &str) -> flow::Owner {
    flow::Owner {
        sandbox: format!("sandbox-{id}"),
        spawn_id: id.into(),
        harness: "codex".into(),
        branch: format!("branch-{id}"),
        timeout: 600,
    }
}

#[test]
fn review_and_feedback_survive_a_new_run() {
    let w = world();
    let now = crate::state::now();
    let first = flow::Spot::find(&w.env, &w.project, "run-one", None)
        .unwrap()
        .unwrap();
    let who = owner("one");
    flow::claim(&w.env, &first, "T001", &who, now).unwrap();
    flow::started(&w.env, &first, "T001", now).unwrap();
    flow::report(
        &w.env,
        "run-one",
        &w.project,
        &who.sandbox,
        true,
        "implemented",
        now,
    )
    .unwrap();
    let status = crate::spawn::Status {
        id: who.spawn_id.clone(),
        sandbox: who.sandbox.clone(),
        harness: who.harness.clone(),
        branch: who.branch.clone(),
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
    };
    flow::child_ended(&w.env, "run-one", &w.project, &status, &Default::default());
    let second = flow::Spot::find(&w.env, &w.project, "run-two", None)
        .unwrap()
        .unwrap();
    let view = second.sync(&w.env, true, now).unwrap();
    let r = &view.records["T001"];
    assert_eq!(r.state, State::Review);
    assert_eq!(r.branch.as_deref(), Some("branch-one"));
    assert!(r.result.as_deref().unwrap().contains("implemented"));
    let b = broker(&w, "run-two", vec![], Some("exit 0"));
    let response = b.handle(&req("spec", serde_json::json!({"action":"review", "id":"T001", "verdict":"changes", "text":"add a test"})));
    assert_eq!(response.status, Status::Done, "{}", response.message);
    let r = flow::claim(&w.env, &second, "T001", &owner("two"), now).unwrap();
    assert!(super::super::prompt::worker(&second.layout, &r, false).contains("add a test"));
}

#[test]
fn stale_lease_is_freed_in_another_run() {
    let w = world();
    let first = flow::Spot::find(&w.env, &w.project, "run-one", None)
        .unwrap()
        .unwrap();
    flow::claim(&w.env, &first, "T001", &owner("one"), 100).unwrap();
    let second = flow::Spot::find(&w.env, &w.project, "run-two", None)
        .unwrap()
        .unwrap();
    let view = second.sync(&w.env, true, 100 + board::LEASE_TTL).unwrap();
    assert_eq!(view.records["T001"].state, State::Todo);
    assert!(flow::claim(
        &w.env,
        &second,
        "T001",
        &owner("two"),
        100 + board::LEASE_TTL
    )
    .is_ok());
}

#[test]
fn separate_runs_cannot_claim_the_same_task() {
    let w = world();
    let a = flow::Spot::find(&w.env, &w.project, "run-one", None)
        .unwrap()
        .unwrap();
    let b = flow::Spot::find(&w.env, &w.project, "run-two", None)
        .unwrap()
        .unwrap();
    let barrier = std::sync::Barrier::new(2);
    std::thread::scope(|scope| {
        let run = |spot: flow::Spot, id| {
            let barrier = &barrier;
            let env = &w.env;
            scope.spawn(move || {
                barrier.wait();
                flow::claim(env, &spot, "T001", &owner(id), 100)
            })
        };
        let a = run(a, "one");
        let b = run(b, "two");
        assert_ne!(a.join().unwrap().is_ok(), b.join().unwrap().is_ok());
    });
}
