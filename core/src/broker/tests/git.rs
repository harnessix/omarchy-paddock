use super::*;

#[test]
fn a_push_session_grant_covers_the_same_branch_only() {
    let (_t, b, fixed, sha) = setup_repo(Some(Answer::Session), HostPolicy::default());
    let r = b.handle(&push_req("origin", &sha, "refs/heads/a", false));
    assert_eq!(r.status, Status::Done, "{}", r.message);
    assert_eq!(r.result["refs"][0]["ok"], true);
    b.handle(&push_req("origin", &sha, "refs/heads/a", false));
    assert_eq!(fixed.asked.load(Ordering::SeqCst), 1);
    b.handle(&push_req("origin", &sha, "refs/heads/b", false));
    assert_eq!(fixed.asked.load(Ordering::SeqCst), 2);
}

#[test]
fn force_and_default_branch_pushes_ask_every_time_even_when_allowed() {
    let mut p = HostPolicy::default();
    p.modes.insert("git-push".into(), Mode::Allow);
    let (_t, b, fixed, sha) = setup_repo(Some(Answer::Session), p);
    for _ in 0..2 {
        assert_eq!(
            b.handle(&push_req("origin", &sha, "refs/heads/f", true))
                .status,
            Status::Done
        );
    }
    assert_eq!(fixed.asked.load(Ordering::SeqCst), 2);
    b.handle(&push_req("origin", &sha, "refs/heads/main", false));
    b.handle(&push_req("origin", &sha, "refs/heads/main", false));
    assert_eq!(fixed.asked.load(Ordering::SeqCst), 4);
    // An ordinary branch follows the configuration.
    b.handle(&push_req("origin", &sha, "refs/heads/x", false));
    assert_eq!(fixed.asked.load(Ordering::SeqCst), 4);
}

#[test]
fn a_denied_push_reports_denied_and_a_bad_ref_is_invalid() {
    let (_t, b, _, sha) = setup_repo(Some(Answer::Deny), HostPolicy::default());
    assert_eq!(
        b.handle(&push_req("origin", &sha, "refs/heads/a", false))
            .status,
        Status::Denied
    );
    assert_eq!(Status::Denied.exit_code(), 3);
    assert_eq!(
        b.handle(&push_req("origin", &sha, "HEAD", false)).status,
        Status::Invalid
    );
}

#[test]
fn a_push_url_with_credentials_stays_out_of_the_audit_line() {
    let r = req(
        "git-push",
        serde_json::json!({ "remote": "https://u:ghp_secret@example.com/a.git" }),
    );
    let d = audit_detail(&r, Path::new("/srv/app"), None);
    assert!(!d.contains("ghp_secret"), "{d}");
    assert!(d.contains("example.com"), "{d}");
}

#[test]
fn push_approval_warns_for_spec_commits_only_with_the_default_store() {
    let (_tmp, b, _, sha) = setup_repo(None, HostPolicy::default());
    let prepare = |sha: &str| {
        super::super::prepare::prepare(
            &b.env,
            &b.ctx,
            &push_req("origin", sha, "refs/heads/feature", false),
        )
        .unwrap()
        .detail
    };
    let notice = crate::speckit::git::PUSH_NOTICE;
    assert!(!prepare(&sha).contains(notice));
    std::fs::create_dir(b.ctx.project.join("specs")).unwrap();
    std::fs::write(b.ctx.project.join("specs/plan.md"), "plan").unwrap();
    git_in(&b.ctx.project, &["add", "."]);
    git_in(&b.ctx.project, &["commit", "-qm", "plan"]);
    let sha = git_in(&b.ctx.project, &["rev-parse", "HEAD"]);
    assert!(prepare(&sha).contains(notice));
    std::fs::create_dir_all(b.env.paddock_config()).unwrap();
    for store in ["repo", "external"] {
        std::fs::write(
            b.env.paddock_config().join("config.toml"),
            format!("[speckit]\nstore = \"{store}\"\n"),
        )
        .unwrap();
        assert!(!prepare(&sha).contains(notice));
    }
}

#[test]
fn push_approval_names_private_files_and_new_docs() {
    let (_tmp, b, _, _) = setup_repo(None, HostPolicy::default());
    let p = &b.ctx.project;
    let detail = |sha: &str| {
        super::super::prepare::prepare(
            &b.env,
            &b.ctx,
            &push_req("origin", sha, "refs/heads/feature", false),
        )
        .unwrap()
        .detail
    };
    let commit = |files: &[&str]| {
        for f in files {
            if let Some(d) = Path::new(f).parent() {
                std::fs::create_dir_all(p.join(d)).unwrap();
            }
            std::fs::write(p.join(f), "x\n").unwrap();
        }
        git_in(p, &["add", "."]);
        git_in(p, &["commit", "-qm", "change"]);
        git_in(p, &["rev-parse", "HEAD"])
    };
    // A commit with neither: no line.
    let plain = commit(&["src/main.rs"]);
    let d = detail(&plain);
    assert!(!d.contains("This push"), "{d}");
    // An instruction file.
    let sha = commit(&["CLAUDE.md"]);
    let d = detail(&sha);
    assert!(
        d.contains("This push contains agent instruction files (CLAUDE.md)"),
        "{d}"
    );
    assert!(!d.contains("docs/"), "{d}");
    // New files under docs/: their names, at most five, then a count.
    let names: Vec<String> = (1..=7).map(|i| format!("docs/n{i}.md")).collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let sha = commit(&refs);
    let d = detail(&sha);
    assert!(
        d.contains("adds new files under docs/ (docs/n1.md, docs/n2.md, docs/n3.md, docs/n4.md, docs/n5.md, 2 more)"),
        "{d}"
    );
    // Public paths say nothing.
    std::fs::create_dir_all(b.env.paddock_config()).unwrap();
    std::fs::write(
        b.env.paddock_config().join("config.toml"),
        "[loft]\npublic = [\"CLAUDE.md\", \"docs/\"]\n",
    )
    .unwrap();
    let d = detail(&sha);
    assert!(!d.contains("This push"), "{d}");
}
