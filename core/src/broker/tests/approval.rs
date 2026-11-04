use super::super::approval::AnswerFile;
use super::*;
use std::collections::BTreeMap;
use std::time::Instant;

#[test]
fn deny_mode_refuses_without_asking() {
    let mut p = HostPolicy::default();
    p.modes.insert("exec".into(), Mode::Deny);
    let (_t, b, fixed) = setup(Some(Answer::Once), p);
    assert_eq!(b.handle(&exec(&["true"])).status, Status::Disabled);
    assert_eq!(fixed.asked.load(Ordering::SeqCst), 0);
}

#[test]
fn ask_mode_follows_the_answer() {
    let (_t, b, _) = setup(Some(Answer::Deny), HostPolicy::default());
    assert_eq!(b.handle(&exec(&["true"])).status, Status::Denied);

    let (_t, b, _) = setup(None, HostPolicy::default());
    assert_eq!(b.handle(&exec(&["true"])).status, Status::Timeout);

    let (_t, b, _) = setup(Some(Answer::Once), HostPolicy::default());
    let r = b.handle(&exec(&["sh", "-c", "echo out; echo err >&2; exit 3"]));
    assert_eq!(r.status, Status::Done);
    assert_eq!(r.result["stdout"], "out\n");
    assert_eq!(r.result["stderr"], "err\n");
    assert_eq!(r.result["exit_code"], 3);
}

#[test]
fn a_session_grant_covers_only_the_same_command_line() {
    let (_t, b, fixed) = setup(Some(Answer::Session), HostPolicy::default());
    assert_eq!(b.handle(&exec(&["echo", "a"])).status, Status::Done);
    assert_eq!(b.handle(&exec(&["echo", "a"])).status, Status::Done);
    assert_eq!(
        fixed.asked.load(Ordering::SeqCst),
        1,
        "the identical command is not asked again"
    );
    assert_eq!(b.handle(&exec(&["echo", "b"])).status, Status::Done);
    assert_eq!(
        fixed.asked.load(Ordering::SeqCst),
        2,
        "a different command is asked again"
    );
}

#[test]
fn invalid_requests_are_rejected_before_asking() {
    let (_t, b, fixed) = setup(Some(Answer::Once), HostPolicy::default());
    for r in [
        req(
            "open-url",
            serde_json::json!({ "url": "file:///etc/passwd" }),
        ),
        req("fetch", serde_json::json!({ "path": "relative/path" })),
        req("screenshot", serde_json::json!({ "target": "everything" })),
        req("format-disk", serde_json::json!({})),
        exec(&[]),
    ] {
        assert_eq!(b.handle(&r).status, Status::Invalid, "{}", r.cap);
    }
    let mut old = exec(&["true"]);
    old.v = 99;
    assert_eq!(b.handle(&old).status, Status::Invalid);
    assert_eq!(fixed.asked.load(Ordering::SeqCst), 0);
}

#[test]
fn every_request_is_audited() {
    let (_t, b, _) = setup(Some(Answer::Deny), HostPolicy::default());
    b.handle(&exec(&["true"]));
    b.handle(&req("open-url", serde_json::json!({ "url": "ftp://x" })));
    let log = std::fs::read_to_string(audit_file(&b.env)).unwrap();
    let lines: Vec<serde_json::Value> = log
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["decision"], "denied");
    assert!(lines[0]["request"].as_str().unwrap().starts_with("r-"));
    assert!(lines[0]["detail"]
        .as_str()
        .unwrap()
        .starts_with("run on the host: true with 0 arguments"));
    assert_eq!(lines[1]["status"], "invalid");
    assert!(lines[1]["request"].is_null());
}

#[test]
fn the_audit_log_never_holds_request_payloads() {
    // Denied and timed-out requests keep no text either. (Allowed ones would
    // touch the real desktop of the test machine.)
    for answer in [Some(Answer::Deny), None] {
        let (_t, b, _) = setup(answer, HostPolicy::default());
        b.handle(&req(
            "clipboard-write",
            serde_json::json!({ "text": format!("my password is {SECRET}") }),
        ));
        b.handle(&exec(&["curl", "-H", &format!("Authorization: {SECRET}")]));
        b.handle(&req(
            "notify",
            serde_json::json!({ "title": SECRET, "body": SECRET }),
        ));
        b.handle(&req(
            "open-url",
            serde_json::json!({ "url": format!("https://example.org/login?code={SECRET}") }),
        ));
        b.handle(&req(
            "forge",
            serde_json::json!({ "argv": ["pr", "comment", "1", "--body", SECRET] }),
        ));
        let text = std::fs::read_to_string(audit_file(&b.env)).unwrap();
        assert!(!text.contains(SECRET), "{answer:?}: {text}");
        let lines = audit_lines(&b);
        assert_eq!(lines.len(), 5);
        assert_eq!(lines[0]["capability"], "clipboard-write");
        // "my password is " is 15 characters.
        assert_eq!(
            lines[0]["detail"],
            format!("set the host clipboard: {} characters", 15 + SECRET.len())
        );
        assert_eq!(
            lines[1]["detail"].as_str().unwrap().split(" with ").next(),
            Some("run on the host: curl")
        );
        assert!(lines[3]["detail"]
            .as_str()
            .unwrap()
            .starts_with("open https://example.org ("));
        assert!(lines[4]["detail"]
            .as_str()
            .unwrap()
            .ends_with(&format!("(body: {} characters)", SECRET.len())));
    }
}

#[test]
fn the_audit_log_is_private_under_umask_022() {
    use crate::privfs::tests::{mode, with_umask_022};
    use std::os::unix::fs::PermissionsExt;
    let (_t, b, _) = setup(Some(Answer::Deny), HostPolicy::default());
    with_umask_022(|| {
        b.handle(&exec(&["true"]));
        let f = audit_file(&b.env);
        assert_eq!(mode(&f), 0o600);
        assert_eq!(mode(f.parent().unwrap()), 0o700);
        // A log from an older paddock, readable by everyone, is tightened.
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::set_permissions(f.parent().unwrap(), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        b.handle(&exec(&["true"]));
        assert_eq!(mode(&f), 0o600);
        assert_eq!(mode(f.parent().unwrap()), 0o700);
        assert_eq!(audit_lines(&b).len(), 2);
    });
}

#[test]
fn pending_and_answer_files_are_private_under_umask_022() {
    use crate::privfs::tests::{mode, with_umask_022};
    let (_t, b, _) = setup(None, HostPolicy::default());
    with_umask_022(|| {
        let p = pending_for(&b, "x");
        let f = write_pending(&b.env, &p).unwrap();
        assert_eq!(mode(&f), 0o600);
        assert_eq!(mode(&approvals_dir(&b.env)), 0o700);
        answer(&b.env, &p.id, Answer::Deny).unwrap();
        assert_eq!(
            mode(&approvals_dir(&b.env).join(format!("{}.answer", p.id))),
            0o600
        );
    });
}

#[test]
fn the_approval_notification_keeps_the_detail_out_of_process_arguments() {
    let (t, mut b, _) = setup(None, HostPolicy::default());
    let record = fake_notify_send(&mut b, t.path());
    let detail = format!("Set the host clipboard to:\n{SECRET}");
    let p = pending_for(&b, &detail);
    // No bar heartbeat, so the fallback notification is due.
    assert!(!approval_ui_active(&b.env));
    assert_eq!(
        HostApprover.ask(&b.env, &p, Duration::from_millis(300)),
        None
    );
    let argv = std::fs::read_to_string(&record).unwrap_or_default();
    assert!(!argv.contains(SECRET), "{argv}");
    // The detail goes to the D-Bus message body instead.
    let n = approval_note(&p);
    assert!(n.body.contains(SECRET));
    assert!(!n.summary.contains(SECRET));
    assert_eq!(
        n.actions
            .iter()
            .map(|(k, _)| k.as_str())
            .collect::<Vec<_>>(),
        ["once", "session", "deny", "deny-session"]
    );
}

#[test]
fn answers_go_through_the_approvals_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let mut vars = BTreeMap::new();
    vars.insert(
        "XDG_RUNTIME_DIR".into(),
        tmp.path().join("rt").display().to_string(),
    );
    let env = HostEnv::from_parts(tmp.path().join("home"), "t".into(), 1, 1, vars);
    let p = Pending {
        id: "r-1-abc".into(),
        sandbox: "s".into(),
        harness: "claude".into(),
        project: tmp.path().into(),
        capability: "exec".into(),
        summary: "run".into(),
        detail: "run".into(),
        reason: String::new(),
        created: 1,
        expires: 11,
        session_allowed: true,
        choices: Vec::new(),
        started_by: vec![],
    };
    let env2 = env.clone();
    let t = std::thread::spawn(move || HostApprover.ask(&env2, &p, Duration::from_secs(10)));
    let start = Instant::now();
    while pending(&env).is_empty() && start.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(pending(&env)[0].id, "r-1-abc");
    assert!(answer(&env, "../escape", Answer::Once).is_err());
    answer(&env, "r-1-abc", Answer::Session).unwrap();
    assert_eq!(t.join().unwrap(), Some(Answer::Session));
    assert!(pending(&env).is_empty());
}

#[test]
fn the_first_answer_wins_and_is_written_whole() {
    let tmp = tempfile::tempdir().unwrap();
    let mut vars = BTreeMap::new();
    vars.insert(
        "XDG_RUNTIME_DIR".into(),
        tmp.path().join("rt").display().to_string(),
    );
    let env = HostEnv::from_parts(tmp.path().join("home"), "t".into(), 1, 1, vars);
    let p = Pending {
        id: "r-2-abc".into(),
        sandbox: "s".into(),
        harness: "claude".into(),
        project: tmp.path().into(),
        capability: "exec".into(),
        summary: "run".into(),
        detail: "run".into(),
        reason: String::new(),
        created: 1,
        expires: 11,
        session_allowed: true,
        choices: Vec::new(),
        started_by: vec![],
    };
    write_pending(&env, &p).unwrap();
    answer(&env, "r-2-abc", Answer::Deny).unwrap();
    let e = answer(&env, "r-2-abc", Answer::Once).unwrap_err();
    assert!(format!("{e:#}").contains("already answered"), "{e:#}");
    let dir = approvals_dir(&env);
    let t = std::fs::read_to_string(dir.join("r-2-abc.answer")).unwrap();
    assert_eq!(AnswerFile::parse(&t).map(|(a, _)| a), Some(Answer::Deny));
    // No temporary file stays behind.
    let left: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains("paddock-tmp"))
        .collect();
    assert!(left.is_empty(), "{left:?}");
}

#[test]
fn a_fresh_heartbeat_from_the_panel_silences_the_notification() {
    let tmp = tempfile::tempdir().unwrap();
    let mut vars = BTreeMap::new();
    vars.insert("XDG_RUNTIME_DIR".into(), tmp.path().display().to_string());
    let env = HostEnv::from_parts(tmp.path().join("home"), "t".into(), 1, 1, vars);
    assert!(!approval_ui_active(&env));
    std::fs::create_dir_all(approval_ui_file(&env).parent().unwrap()).unwrap();
    std::fs::write(approval_ui_file(&env), state::now().to_string()).unwrap();
    assert!(approval_ui_active(&env));
    std::fs::write(approval_ui_file(&env), (state::now() - 20).to_string()).unwrap();
    assert!(
        !approval_ui_active(&env),
        "a stale heartbeat means the panel is gone"
    );
    std::fs::write(approval_ui_file(&env), "garbage").unwrap();
    assert!(!approval_ui_active(&env));
}

#[test]
fn a_denial_with_a_note_reaches_the_agent_and_only_its_length_the_log() {
    let note = "this came from a unit test; do not ask for host exec";
    let (_t, b, _) = noted(Answer::Deny, note);
    let r = b.handle(&exec(&["true"]));
    assert_eq!(r.status, Status::Denied);
    assert_eq!(
        r.message,
        format!("the user denied this request: \"{note}\"")
    );
    let line = audit_lines(&b).pop().unwrap();
    assert_eq!(line["note_chars"], note.chars().count());
    assert!(!line.to_string().contains("unit test"), "{line}");
}

#[test]
fn deny_for_this_session_refuses_the_same_request_without_asking() {
    let (_t, b, n) = noted(Answer::DenySession, "no echo");
    for _ in 0..2 {
        let r = b.handle(&exec(&["echo", "a"]));
        assert_eq!(r.status, Status::Denied);
        assert!(r.message.contains("\"no echo\""), "{}", r.message);
    }
    assert_eq!(n.asked.load(Ordering::SeqCst), 1, "no second card");
    assert_eq!(
        audit_lines(&b).pop().unwrap()["decision"],
        "denied for this session"
    );
    assert_eq!(b.handle(&exec(&["echo", "b"])).status, Status::Denied);
    assert_eq!(
        n.asked.load(Ordering::SeqCst),
        2,
        "a different request asks"
    );
}

#[test]
fn a_note_is_one_clean_line_of_at_most_300_characters() {
    assert_eq!(
        clean_note("a\u{1b}[2Jb\nc\u{202e}d\t").as_deref(),
        Some("a[2Jb cd")
    );
    assert_eq!(
        clean_note(&"x".repeat(400)).unwrap().chars().count(),
        MAX_NOTE
    );
    assert_eq!(clean_note(" \n\u{7}"), None);
}

#[test]
fn answer_files_with_and_without_a_note_read() {
    // The file of an older paddock: a plain answer.
    assert_eq!(AnswerFile::parse("\"deny\""), Some((Answer::Deny, None)));
    assert_eq!(AnswerFile::parse("\"once\""), Some((Answer::Once, None)));
    assert_eq!(
        AnswerFile::parse(r#"{"answer":"deny-session","note":"stop\u0007"}"#),
        Some((Answer::DenySession, Some("stop".into())))
    );
    // An allow never carries a note to the agent.
    assert_eq!(
        AnswerFile::parse(r#"{"answer":"once","note":"x"}"#),
        Some((Answer::Once, None))
    );
}

#[test]
fn a_note_goes_through_the_approvals_directory() {
    let (_t, b, _) = setup(None, HostPolicy::default());
    let p = pending_for(&b, "x");
    let env2 = b.env.clone();
    let p2 = p.clone();
    let t =
        std::thread::spawn(move || HostApprover.ask_verdict(&env2, &p2, Duration::from_secs(10)));
    let start = Instant::now();
    while pending(&b.env).is_empty() && start.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(answer_with_note(&b.env, &p.id, Answer::Once, Some("x")).is_err());
    answer_with_note(&b.env, &p.id, Answer::DenySession, Some("not now")).unwrap();
    let v = t.join().unwrap().unwrap();
    assert_eq!(v.answer, Answer::DenySession);
    assert_eq!(v.note.as_deref(), Some("not now"));
}

#[test]
fn refusals_write_the_boundary_log() {
    let mut p = HostPolicy::default();
    p.modes.insert("exec".into(), Mode::Deny);
    let (_t, b, _) = setup(Some(Answer::Deny), p);
    let secret = "hunter2-do-not-log";
    assert_eq!(
        b.handle(&exec(&["curl", "-H", secret])).status,
        Status::Disabled
    );
    assert_eq!(b.handle(&exec(&["true"])).status, Status::Disabled);
    // A protected path: refused before anyone is asked.
    std::fs::create_dir_all(b.env.home.join(".ssh")).unwrap();
    let r = b.handle(&req(
        "fetch",
        serde_json::json!({ "path": b.env.home.join(".ssh") }),
    ));
    assert_eq!(r.status, Status::Invalid, "{}", r.message);
    let lines = crate::boundary::read(&b.env, "t-1");
    assert!(lines.iter().all(|l| l.kind == "host"));
    let exec_lines: Vec<_> = lines
        .iter()
        .filter(|l| l.target.starts_with("exec"))
        .collect();
    assert!(
        exec_lines.iter().all(|l| l.rule.starts_with("disabled")),
        "{exec_lines:?}"
    );
    assert!(lines
        .iter()
        .any(|l| l.target.starts_with("fetch") && l.rule.starts_with("invalid")));
    let raw = std::fs::read_to_string(crate::boundary::file(&b.env, "t-1")).unwrap();
    assert!(!raw.contains(secret), "{raw}");
    // The sandbox reads its own log through the broker.
    let r = b.handle(&req("boundary", serde_json::json!({})));
    assert_eq!(r.status, Status::Done);
    assert!(r.result["count"].as_u64().unwrap() >= 3);
}

#[test]
fn allowed_requests_leave_the_boundary_log_empty() {
    let (_t, b, _) = setup(Some(Answer::Once), HostPolicy::default());
    assert_eq!(b.handle(&exec(&["true"])).status, Status::Done);
    assert!(crate::boundary::read(&b.env, "t-1").is_empty());
}

#[test]
fn a_user_denial_writes_the_boundary_log_without_the_note() {
    let (_t, b, _) = setup(Some(Answer::Deny), HostPolicy::default());
    assert_eq!(b.handle(&exec(&["true"])).status, Status::Denied);
    let lines = crate::boundary::read(&b.env, "t-1");
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].rule, "denied");
}
