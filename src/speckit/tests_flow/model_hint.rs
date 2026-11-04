//! `spawn --task` with a `[effort:...]` hint in the task line, and
//! `[spawn] models` (spawn/model.rs).

use super::*;
use std::sync::Mutex;

/// Says yes and keeps the text of each card.
#[derive(Default)]
struct Cards(Mutex<Vec<String>>);

impl Approver for Cards {
    fn ask(&self, _: &HostEnv, p: &Pending, _: Duration) -> Option<Answer> {
        self.0.lock().unwrap().push(p.detail.clone());
        Some(Answer::Once)
    }
}

fn hinted_broker(w: &World, cards: Arc<Cards>, script: &str) -> Broker {
    let ctx = Context {
        id: "p-1".into(),
        harness: "claude".into(),
        project: w.project.clone(),
        repo: None,
        depth: 0,
        chain: vec![],
        role: None,
    };
    let mut b = Broker::new(w.env.clone(), ctx, HostPolicy::default(), cards);
    let cfg = SpawnCfg {
        harnesses: vec!["codex".into()],
        models: vec!["gpt-5.5".into()],
        ..Default::default()
    };
    let program: Vec<OsString> = ["sh", "-c", script, "fake-child"]
        .iter()
        .map(OsString::from)
        .collect();
    b.spawner = Some(Arc::new(Spawner::with_program(
        &w.env,
        cfg,
        ceiling("p-1", &w.project),
        &w.project,
        program,
    )));
    b
}

#[test]
fn the_task_hint_sets_the_effort_and_the_card_shows_it() {
    let w = world();
    let md = TASKS.replace("T001 [P] [US1]", "T001 [P] [effort:high] [US1]");
    std::fs::write(w.project.join("specs/001-albums/tasks.md"), &md).unwrap();
    let doc = super::super::tasks::parse(&md);
    let t = doc.task("T001").unwrap();
    assert_eq!(t.story.as_deref(), Some("US1"), "a hint is no story");
    assert!(t.parallel);
    assert_eq!(t.text, "[effort:high] Create Album model in src/album.py");

    let script = format!(
        "d=$(dirname \"$1\"); cp \"$1\" {seen}/job.json; \
         printf '{{\"exit_code\":0,\"branch\":\"b\",\"base\":\"c\",\"commits\":1,\"uncommitted\":0,\"stat\":\"\",\"output\":\"ok\",\"output_truncated\":false,\"started\":1,\"ended\":2}}' > \"$d/result.json\"",
        seen = w.seen.display()
    );
    let cards = Arc::new(Cards::default());
    let b = hinted_broker(&w, cards.clone(), &script);

    // A model that [spawn] models does not list: refused before any card.
    let r = b.handle(&req(
        "spawn",
        serde_json::json!({"action": "start", "harness": "codex", "task": "next", "model": "gpt-4o"}),
    ));
    assert_eq!(r.status, Status::Invalid, "{}", r.message);
    assert!(
        r.message.contains("gpt-4o") && r.message.contains("[spawn] models"),
        "{}",
        r.message
    );
    assert!(cards.0.lock().unwrap().is_empty());

    let r = b.handle(&req(
        "spawn",
        serde_json::json!({"action": "start", "harness": "codex", "task": "next"}),
    ));
    assert_eq!(r.status, Status::Done, "{}", r.message);
    assert_eq!(r.result["task"], "T001");
    assert_eq!(r.result["effort"], "high");
    assert!(r.result.get("model").is_none(), "{}", r.result);
    let card = cards.0.lock().unwrap()[0].clone();
    assert!(card.contains("Model: the default of codex"), "{card}");
    assert!(
        card.contains("Effort: high (the hint of the task)"),
        "{card}"
    );

    let sp = b.spawner.clone().unwrap();
    let id = r.result["id"].as_str().unwrap();
    sp.wait(id, Duration::from_secs(20)).unwrap();
    let job: serde_json::Value =
        serde_json::from_slice(&std::fs::read(w.seen.join("job.json")).unwrap()).unwrap();
    assert_eq!(job["pick"], serde_json::json!({"effort": "high"}));
}
