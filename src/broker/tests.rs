//! Broker tests with a fixed answer instead of a person.

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Fixed {
    answer: Option<Answer>,
    asked: AtomicUsize,
}

impl Approver for Fixed {
    fn ask(&self, _: &HostEnv, _: &Pending, _: Duration) -> Option<Answer> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        self.answer
    }
}

fn setup(answer: Option<Answer>, policy: HostPolicy) -> (tempfile::TempDir, Broker, Arc<Fixed>) {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let project = tmp.path().join("work/app");
    std::fs::create_dir_all(&project).unwrap();
    let mut vars = BTreeMap::new();
    vars.insert("PATH".into(), std::env::var("PATH").unwrap_or_default());
    let env = HostEnv::from_parts(home, "tester".into(), 1000, 1000, vars);
    let fixed = Arc::new(Fixed {
        answer,
        asked: AtomicUsize::new(0),
    });
    let ctx = Context {
        id: "t-1".into(),
        harness: "claude".into(),
        project,
        repo: None,
    };
    (tmp, Broker::new(env, ctx, policy, fixed.clone()), fixed)
}

fn req(cap: &str, args: serde_json::Value) -> Request {
    let args = args
        .as_object()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .collect();
    Request {
        v: PROTOCOL,
        cap: cap.into(),
        args,
        reason: "test".into(),
    }
}

fn exec(argv: &[&str]) -> Request {
    req("exec", serde_json::json!({ "argv": argv }))
}

#[test]
fn capabilities_lists_every_capability_with_its_mode() {
    let (_t, b, _) = setup(None, HostPolicy::default());
    let r = b.handle(&req("capabilities", serde_json::json!({})));
    assert_eq!(r.status, Status::Done);
    let caps = r.result["capabilities"].as_array().unwrap();
    assert_eq!(caps.len(), CAPABILITIES.len());
    assert!(caps
        .iter()
        .any(|c| c["name"] == "exec" && c["mode"] == "ask"));
    assert!(caps
        .iter()
        .any(|c| c["name"] == "notify" && c["mode"] == "allow"));
}

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
fn exec_runs_in_the_project_directory() {
    let (_t, b, _) = setup(Some(Answer::Once), HostPolicy::default());
    let r = b.handle(&exec(&["pwd"]));
    assert_eq!(
        r.result["stdout"].as_str().unwrap().trim(),
        b.ctx.project.display().to_string()
    );
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
fn fetch_copies_into_the_inbox_and_refuses_protected_paths() {
    let (t, b, _) = setup(Some(Answer::Once), HostPolicy::default());
    let src = t.path().join("notes.txt");
    std::fs::write(&src, "hello").unwrap();
    let r = b.handle(&req("fetch", serde_json::json!({ "path": src })));
    assert_eq!(r.status, Status::Done, "{}", r.message);
    let dst = PathBuf::from(r.result["path"].as_str().unwrap());
    assert!(dst.starts_with(b.ctx.project.join(INBOX)));
    assert_eq!(std::fs::read_to_string(dst).unwrap(), "hello");
    assert_eq!(
        std::fs::read_to_string(b.ctx.project.join(".paddock/.gitignore")).unwrap(),
        "*\n"
    );

    let ssh = b.env.home.join(".ssh");
    std::fs::create_dir_all(&ssh).unwrap();
    std::fs::write(ssh.join("id_ed25519"), "secret").unwrap();
    let r = b.handle(&req(
        "fetch",
        serde_json::json!({ "path": ssh.join("id_ed25519") }),
    ));
    assert_eq!(r.status, Status::Invalid);
    assert!(r.message.contains("protected"));
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
    assert_eq!(
        lines[0]["detail"].as_str().unwrap().lines().last().unwrap(),
        "true"
    );
    assert_eq!(lines[1]["status"], "invalid");
}

#[test]
fn configuration_cannot_set_exec_to_allow() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().to_path_buf();
    std::fs::create_dir_all(home.join(".config/paddock")).unwrap();
    std::fs::write(
        home.join(".config/paddock/config.toml"),
        "[host]\nexec = \"allow\"\nscreenshot = \"deny\"\nwait = 30\n",
    )
    .unwrap();
    let env = HostEnv::from_parts(home, "t".into(), 1, 1, BTreeMap::new());
    let p = HostPolicy::load(&env).unwrap();
    assert_eq!(p.mode("exec"), Mode::Ask);
    assert_eq!(p.mode("screenshot"), Mode::Deny);
    assert_eq!(p.wait, 30);
    std::fs::write(
        env.home.join(".config/paddock/config.toml"),
        "[host]\nrootshell = \"allow\"\n",
    )
    .unwrap();
    assert!(HostPolicy::load(&env).is_err());
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
fn the_client_talks_to_the_broker_over_the_socket() {
    let (t, b, _) = setup(Some(Answer::Once), HostPolicy::default());
    let sock = t.path().join("host.sock");
    serve(&sock, b).unwrap();
    std::env::set_var("PADDOCK_HOST_SOCKET", &sock);
    let r = crate::hostcli::send(&exec(&["echo", "via socket"])).unwrap();
    assert_eq!(r.status, Status::Done);
    assert_eq!(r.result["stdout"], "via socket\n");
    assert_eq!(
        crate::hostcli::main(&[
            "exec".into(),
            "--".into(),
            "sh".into(),
            "-c".into(),
            "exit 4".into()
        ]),
        4
    );
    assert_eq!(
        crate::hostcli::main(&["bogus".into()]),
        crate::hostcli::EXIT_USAGE
    );
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

fn git_in(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@localhost"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A broker whose project is a git repository with a bare `origin`.
fn setup_repo(
    answer: Option<Answer>,
    policy: HostPolicy,
) -> (tempfile::TempDir, Broker, Arc<Fixed>, String) {
    let (tmp, mut b, fixed) = setup(answer, policy);
    let remote = tmp.path().join("remote.git");
    git_in(
        tmp.path(),
        &["init", "-q", "--bare", remote.to_str().unwrap()],
    );
    let p = b.ctx.project.clone();
    git_in(&p, &["init", "-q", "-b", "main"]);
    git_in(&p, &["commit", "-q", "--allow-empty", "-m", "one"]);
    git_in(&p, &["remote", "add", "origin", remote.to_str().unwrap()]);
    git_in(
        &p,
        &[
            "config",
            "remote.upstream.url",
            "https://github.com/demo/app.git",
        ],
    );
    b.ctx.repo = git::Repo::snapshot(&p);
    let sha = git_in(&p, &["rev-parse", "HEAD"]);
    (tmp, b, fixed, sha)
}

fn push_req(remote: &str, sha: &str, dst: &str, force: bool) -> Request {
    req(
        "git-push",
        serde_json::json!({ "remote": remote, "url": remote,
            "refs": [{ "src": "HEAD", "sha": sha, "dst": dst, "force": force }] }),
    )
}

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
