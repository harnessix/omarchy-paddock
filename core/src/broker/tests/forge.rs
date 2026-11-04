use super::*;

#[test]
fn forge_reads_skip_the_question_and_writes_ask() {
    let (_t, mut b, fixed, _) = setup_repo(Some(Answer::Deny), HostPolicy::default());
    // No gh: the test must not reach a real forge with the user's login.
    b.env.path.clear();
    let forge = |argv: &[&str]| {
        req(
            "forge",
            serde_json::json!({ "tool": "gh", "argv": argv, "branch": "fix" }),
        )
    };
    // Without gh on the host the read fails, but it is never asked.
    let r = b.handle(&forge(&["pr", "list"]));
    assert_eq!(r.status, Status::Failed);
    assert!(r.message.contains("not installed"), "{}", r.message);
    assert_eq!(fixed.asked.load(Ordering::SeqCst), 0);
    assert_eq!(
        b.handle(&forge(&["pr", "create", "--title", "x"])).status,
        Status::Denied
    );
    assert_eq!(fixed.asked.load(Ordering::SeqCst), 1);
    assert_eq!(b.handle(&forge(&["api", "user"])).status, Status::Invalid);
    let mut p = HostPolicy::default();
    p.modes.insert("forge".into(), Mode::Deny);
    let (_t, b, _, _) = setup_repo(None, p);
    assert_eq!(b.handle(&forge(&["pr", "list"])).status, Status::Disabled);
}

#[test]
fn tea_runs_exactly_the_checked_arguments_with_the_host_login() {
    let (_t, b, fixed) = forge_setup(Some(Answer::Once));
    let r = b.handle(&forge_req("tea", &["pulls", "list", "-r", "demo/tool"]));
    assert_eq!(r.status, Status::Done, "{}", r.message);
    let (args, out) = fake_output(&r);
    assert!(out.starts_with("tool:tea\n"), "{out}");
    assert_eq!(
        args,
        ["pulls", "list", "--repo", "demo/tool", "--login", "cb"]
    );
    assert_eq!(fixed.asked.load(Ordering::SeqCst), 0, "a read asks nobody");
    let r = b.handle(&forge_req("tea", &["comment", "4", "Done", "here"]));
    assert_eq!(r.status, Status::Done, "{}", r.message);
    let (args, _) = fake_output(&r);
    assert_eq!(
        args,
        [
            "comment",
            "--repo",
            "demo/tool",
            "--login",
            "cb",
            "4",
            "Done here"
        ]
    );
    assert_eq!(fixed.asked.load(Ordering::SeqCst), 1, "a write asks");
    // The body stays out of the audit log.
    let lines = audit_lines(&b);
    assert!(
        !lines.iter().any(|l| l.to_string().contains("Done here")),
        "{lines:?}"
    );
    // A login, another repository or a host without a tea login: refused
    // before anything runs.
    for argv in [
        &["pulls", "list", "--login", "other"][..],
        &["pulls", "list", "--repo", "other/tool"],
        &["pulls", "list", "--repo", "github.com/demo/app"],
    ] {
        let r = b.handle(&forge_req("tea", argv));
        assert_eq!(r.status, Status::Invalid, "{argv:?}: {}", r.message);
    }
    assert_eq!(fixed.asked.load(Ordering::SeqCst), 1);
}

#[test]
fn glab_runs_exactly_the_checked_arguments_on_a_self_hosted_gitlab() {
    let (_t, b, fixed) = forge_setup(Some(Answer::Once));
    let web = "https://gitlab.example.org/team/apps/web";
    let r = b.handle(&forge_req("glab", &["mr", "view", "-R", "team/apps/web"]));
    assert_eq!(r.status, Status::Done, "{}", r.message);
    let (args, out) = fake_output(&r);
    assert!(out.starts_with("tool:glab\n"), "{out}");
    assert_eq!(args, ["mr", "view", "fix", "--repo", web]);
    assert_eq!(fixed.asked.load(Ordering::SeqCst), 0, "a read asks nobody");
    let r = b.handle(&forge_req(
        "glab",
        &["mr", "note", "2", "-R", web, "-m", "Looks good"],
    ));
    assert_eq!(r.status, Status::Done, "{}", r.message);
    let (args, _) = fake_output(&r);
    assert_eq!(
        args,
        ["mr", "note", "2", "--repo", web, "--message=Looks good"]
    );
    assert_eq!(fixed.asked.load(Ordering::SeqCst), 1, "a write asks");
    let lines = audit_lines(&b);
    assert!(
        !lines.iter().any(|l| l.to_string().contains("Looks good")),
        "{lines:?}"
    );
    for argv in [
        &["mr", "list", "--hostname", "evil.example"][..],
        &["mr", "list", "-R", "team/apps/other"],
        &["mr", "view", "1", "--web"],
        &["mr", "merge", "1"],
    ] {
        let r = b.handle(&forge_req("glab", argv));
        assert_eq!(r.status, Status::Invalid, "{argv:?}: {}", r.message);
    }
    assert_eq!(fixed.asked.load(Ordering::SeqCst), 1);
}
