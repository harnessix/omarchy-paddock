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
        chain: vec![],
        depth: 0,
        role: None,
    };
    let mut b = Broker::new(env.clone(), ctx, policy, fixed.clone());
    // The executor runs in this process: the same signature checks, no
    // isolation (the tests of execbox and tests/gateway.rs cover that).
    b.dispatch = Some(Arc::new(crate::gatewayd::InProcess {
        key: vec![9u8; 32],
        seen: Default::default(),
        env,
        isolation: crate::execbox::Isolation::None,
    }));
    (tmp, b, fixed)
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

/// Swaps the fetch source for a link to a credential file while the user
/// decides, as a sandbox could.
struct Swapper {
    from: PathBuf,
    to: PathBuf,
    seen: Mutex<Option<String>>,
}

impl Approver for Swapper {
    fn ask(&self, _: &HostEnv, p: &Pending, _: Duration) -> Option<Answer> {
        *self.seen.lock().unwrap() = Some(p.detail.clone());
        std::fs::remove_file(&self.from).unwrap();
        std::os::unix::fs::symlink(&self.to, &self.from).unwrap();
        Some(Answer::Once)
    }
}

#[test]
fn fetch_copies_what_the_user_saw_even_if_the_path_changes_while_waiting() {
    let (t, b, _) = setup(None, HostPolicy::default());
    let creds = b.env.home.join(".claude/.credentials.json");
    std::fs::create_dir_all(creds.parent().unwrap()).unwrap();
    std::fs::write(&creds, "token").unwrap();
    let notes = b.ctx.project.join("notes");
    std::fs::write(&notes, "harmless").unwrap();
    // The project path is a link at first: the card shows where it leads.
    let link = t.path().join("link-to-notes");
    std::os::unix::fs::symlink(&notes, &link).unwrap();
    let swap = Arc::new(Swapper {
        from: notes.clone(),
        to: creds.clone(),
        seen: Mutex::new(None),
    });
    let b = Broker::new(
        b.env.clone(),
        b.ctx.clone(),
        HostPolicy::default(),
        swap.clone(),
    );
    let r = b.handle(&req("fetch", serde_json::json!({ "path": link })));
    assert_eq!(r.status, Status::Done, "{}", r.message);
    let dst = PathBuf::from(r.result["path"].as_str().unwrap());
    assert_eq!(std::fs::read_to_string(dst).unwrap(), "harmless");
    let seen = swap.seen.lock().unwrap().clone().unwrap();
    assert!(seen.contains(&notes.display().to_string()), "{seen}");
    // A second request now resolves to the credential file and is refused.
    let r = b.handle(&req("fetch", serde_json::json!({ "path": notes })));
    assert_eq!(r.status, Status::Invalid, "{}", r.message);
}

#[test]
fn fetch_refuses_fifos_and_folders_that_hold_protected_paths() {
    let (t, b, fixed) = setup(Some(Answer::Once), HostPolicy::default());
    let fifo = t.path().join("pipe");
    let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    let r = b.handle(&req("fetch", serde_json::json!({ "path": fifo })));
    assert_eq!(r.status, Status::Invalid, "{}", r.message);
    std::fs::create_dir_all(b.env.home.join(".ssh")).unwrap();
    let r = b.handle(&req("fetch", serde_json::json!({ "path": b.env.home })));
    assert_eq!(r.status, Status::Invalid, "{}", r.message);
    assert_eq!(fixed.asked.load(Ordering::SeqCst), 0);

    // In a folder, FIFOs and links are skipped and nothing blocks.
    let d = t.path().join("tree");
    std::fs::create_dir_all(d.join("sub")).unwrap();
    std::fs::write(d.join("sub/a"), "a").unwrap();
    let c = std::ffi::CString::new(d.join("fifo").to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    std::os::unix::fs::symlink(b.env.home.join(".ssh"), d.join("ssh")).unwrap();
    let r = b.handle(&req("fetch", serde_json::json!({ "path": d })));
    assert_eq!(r.status, Status::Done, "{}", r.message);
    let dst = PathBuf::from(r.result["path"].as_str().unwrap());
    assert_eq!(std::fs::read_to_string(dst.join("sub/a")).unwrap(), "a");
    assert!(!dst.join("fifo").exists() && !dst.join("ssh").exists());
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

fn audit_lines(b: &Broker) -> Vec<serde_json::Value> {
    std::fs::read_to_string(audit_file(&b.env))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

const SECRET: &str = "hunter2-correct-horse-battery";

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

fn pending_for(b: &Broker, detail: &str) -> Pending {
    Pending {
        id: "r-1-abcdef12".into(),
        sandbox: b.ctx.id.clone(),
        harness: "claude".into(),
        project: b.ctx.project.clone(),
        capability: "clipboard-write".into(),
        summary: "set the host clipboard".into(),
        detail: detail.into(),
        reason: String::new(),
        created: state::now(),
        expires: state::now() + 1,
        session_allowed: true,
        choices: Vec::new(),
        started_by: vec![],
    }
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

/// A fake `notify-send` that records its arguments. paddock must not run
/// it: arguments of a host process are visible to every local user.
fn fake_notify_send(b: &mut Broker, dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let record = dir.join("argv.txt");
    let script = bin.join("notify-send");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{}'\n",
            record.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut vars = BTreeMap::new();
    vars.insert(
        "PATH".into(),
        format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        ),
    );
    b.env = HostEnv::from_parts(b.env.home.clone(), "tester".into(), 1000, 1000, vars);
    record
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
fn the_notify_capability_keeps_its_text_out_of_process_arguments() {
    let (t, mut b, _) = setup(None, HostPolicy::default());
    let record = fake_notify_send(&mut b, t.path());
    // The test environment has no session bus, so the request fails, but
    // it never falls back to a process.
    let r = b.handle(&req(
        "notify",
        serde_json::json!({ "title": SECRET, "body": SECRET }),
    ));
    assert_eq!(r.status, Status::Failed);
    let argv = std::fs::read_to_string(&record).unwrap_or_default();
    assert!(!argv.contains(SECRET), "{argv}");
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
fn the_client_talks_to_the_broker_over_the_socket() {
    let (t, b, _) = setup(Some(Answer::Once), HostPolicy::default());
    let sock = t.path().join("host.sock");
    serve(&sock, std::sync::Arc::new(b)).unwrap();
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

#[test]
fn inbox_never_follows_a_planted_link() {
    use std::os::unix::fs::symlink;
    let t = tempfile::tempdir().unwrap();
    let project = t.path().join("p");
    let outside = t.path().join("outside");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    // `.paddock` as a link to a host folder.
    symlink(&outside, project.join(".paddock")).unwrap();
    assert!(inbox(&project).is_err());
    assert!(std::fs::read_dir(&outside).unwrap().next().is_none());
    // `inbox` as a link.
    std::fs::remove_file(project.join(".paddock")).unwrap();
    std::fs::create_dir_all(project.join(".paddock")).unwrap();
    symlink(&outside, project.join(".paddock/inbox")).unwrap();
    assert!(inbox(&project).is_err());
    assert!(std::fs::read_dir(&outside).unwrap().next().is_none());
    // A real inbox works, and existing names are never replaced.
    std::fs::remove_file(project.join(".paddock/inbox")).unwrap();
    let d = inbox(&project).unwrap();
    std::fs::write(project.join(".paddock/inbox/shot.png"), "old").unwrap();
    let (_, name) = d.new_file("shot.png", 0o644).unwrap();
    assert_eq!(name, "shot-1.png");
    assert_eq!(
        std::fs::read_to_string(project.join(".paddock/inbox/shot.png")).unwrap(),
        "old"
    );
    assert_eq!(
        std::fs::read_to_string(project.join(".paddock/.gitignore")).unwrap(),
        "*\n"
    );
}

#[test]
fn fetch_refuses_paddock_s_own_folders_and_the_gateway_key() {
    let (_t, b, fixed) = setup(Some(Answer::Once), HostPolicy::default());
    let key = crate::gateway::key_file(&b.env);
    crate::gateway::key(&b.env).unwrap();
    let audit = b.env.paddock_state().join("host-audit.jsonl");
    std::fs::write(&audit, "{}\n").unwrap();
    let home = b.env.paddock_data().join("homes/claude/shared/other");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("notes.md"), "private").unwrap();
    for p in [key, audit, home.join("notes.md"), b.env.paddock_data()] {
        let r = b.handle(&req("fetch", serde_json::json!({ "path": p })));
        assert_eq!(r.status, Status::Invalid, "{}: {}", p.display(), r.message);
        assert!(r.message.contains("protected"), "{}", r.message);
    }
    // Refused before the user is asked.
    assert_eq!(fixed.asked.load(Ordering::SeqCst), 0);
}

/// A fixed answer with a note, as the user gives it on the card.
struct Noted {
    answer: Answer,
    note: Option<String>,
    asked: AtomicUsize,
}

impl Approver for Noted {
    fn ask(&self, _: &HostEnv, _: &Pending, _: Duration) -> Option<Answer> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        Some(self.answer)
    }
    fn ask_verdict(&self, env: &HostEnv, p: &Pending, wait: Duration) -> Option<Verdict> {
        self.ask(env, p, wait).map(|answer| Verdict {
            answer,
            note: self.note.clone(),
            who: "user",
        })
    }
}

fn noted(answer: Answer, note: &str) -> (tempfile::TempDir, Broker, Arc<Noted>) {
    let (t, mut b, _) = setup(None, HostPolicy::default());
    let n = Arc::new(Noted {
        answer,
        note: Some(note.into()),
        asked: AtomicUsize::new(0),
    });
    b.approver = n.clone();
    (t, b, n)
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

/// A broker whose user configuration holds `config`, with a fake `faketool`
/// on the executor's PATH. The tool prints its arguments, its folder and its
/// environment.
fn tool_setup(answer: Option<Answer>, config: &str) -> (tempfile::TempDir, Broker, Arc<Fixed>) {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let project = tmp.path().join("work/app");
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    let script = bin.join("faketool");
    std::fs::write(
        &script,
        "#!/bin/sh\necho \"args:$*\"\necho \"pwd:$(pwd)\"\nenv | sort\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut vars = BTreeMap::new();
    vars.insert("PATH".into(), format!("{}:/usr/bin:/bin", bin.display()));
    let env = HostEnv::from_parts(home, "tester".into(), 1000, 1000, vars);
    std::fs::create_dir_all(env.paddock_config()).unwrap();
    std::fs::write(env.paddock_config().join("config.toml"), config).unwrap();
    std::fs::create_dir_all(&env.home).unwrap();
    let policy = HostPolicy::load(&env).unwrap();
    let fixed = Arc::new(Fixed {
        answer,
        asked: AtomicUsize::new(0),
    });
    let ctx = Context {
        id: "t-1".into(),
        harness: "claude".into(),
        project,
        repo: None,
        chain: vec![],
        depth: 0,
        role: None,
    };
    let mut b = Broker::new(env.clone(), ctx, policy, fixed.clone());
    b.dispatch = Some(Arc::new(crate::gatewayd::InProcess {
        key: vec![9u8; 32],
        seen: Default::default(),
        env,
        isolation: crate::execbox::Isolation::None,
    }));
    (tmp, b, fixed)
}

fn tool(name: &str, args: &[&str]) -> Request {
    req(
        "tool",
        serde_json::json!({
            "name": name,
            "args": args,
            // A sandbox cannot hand the tool variables: this is ignored.
            "env": { "SANDBOX_SECRET": SECRET },
        }),
    )
}

const TOOLS: &str = "[host.tools.compose]\ncommand = [\"faketool\", \"compose\"]\nargs = [\"up -d\", \"logs *\"]\nmode = \"allow\"\n\
[host.tools.ask]\ncommand = [\"faketool\"]\nargs = [\"run **\"]\n\
[host.tools.home]\ncommand = [\"faketool\"]\ncwd = \"home\"\nmode = \"allow\"\n\
[host.tools.off]\ncommand = [\"faketool\"]\nmode = \"deny\"\n";

#[test]
fn a_tool_runs_allowed_shapes_with_a_clean_environment() {
    let (_t, b, fixed) = tool_setup(Some(Answer::Deny), TOOLS);
    let r = b.handle(&tool("compose", &["up", "-d"]));
    assert_eq!(r.status, Status::Done, "{}", r.message);
    let out = r.result["stdout"].as_str().unwrap();
    assert!(out.contains("args:compose up -d\n"), "{out}");
    assert!(
        out.contains(&format!("pwd:{}\n", b.ctx.project.display())),
        "{out}"
    );
    assert!(!out.contains(SECRET), "{out}");
    for line in out
        .lines()
        .filter(|l| !l.starts_with("args:") && !l.starts_with("pwd:"))
    {
        let k = line.split('=').next().unwrap();
        assert!(
            crate::execbox::BASE_ENV.contains(&k) || ["PWD", "SHLVL", "_", "OLDPWD"].contains(&k),
            "a variable outside the clean environment: {line}"
        );
    }
    assert_eq!(
        fixed.asked.load(Ordering::SeqCst),
        0,
        "mode allow asks nobody"
    );
    let r = b.handle(&tool("compose", &["logs", "web"]));
    assert!(r.result["stdout"]
        .as_str()
        .unwrap()
        .contains("args:compose logs web\n"));
    let r = b.handle(&tool("home", &[]));
    assert!(r.result["stdout"]
        .as_str()
        .unwrap()
        .contains(&format!("pwd:{}\n", b.env.home.display())));

    let lines = audit_lines(&b);
    assert_eq!(lines[0]["capability"], "tool");
    assert_eq!(lines[0]["tool"], "compose");
    assert_eq!(lines[0]["args"], serde_json::json!(["up", "-d"]));
    assert_eq!(lines[0]["decision"], "allowed by configuration");
    assert_eq!(lines[0]["exit_code"], 0);
}

#[test]
fn a_tool_refuses_other_arguments_before_asking() {
    let (_t, b, fixed) = tool_setup(Some(Answer::Once), TOOLS);
    for (name, args) in [
        ("compose", &["up"][..]),
        ("compose", &["run", "--privileged", "x"][..]),
        ("compose", &["logs", "-f"][..]),
        ("compose", &["logs", "a", "b"][..]),
        ("home", &["x"][..]),
        ("ask", &["run", "a", "--rm"][..]),
        ("missing", &[][..]),
    ] {
        let r = b.handle(&tool(name, args));
        assert_eq!(r.status, Status::Invalid, "{name} {args:?}: {}", r.message);
    }
    assert_eq!(b.handle(&tool("off", &[])).status, Status::Disabled);
    assert_eq!(fixed.asked.load(Ordering::SeqCst), 0);
    let lines = audit_lines(&b);
    assert_eq!(lines.len(), 8);
    assert_eq!(lines[0]["decision"], "invalid");
    assert_eq!(lines[7]["decision"], "disabled");
}

#[test]
fn an_ask_tool_asks_and_a_session_grant_covers_the_same_arguments() {
    let (_t, b, fixed) = tool_setup(Some(Answer::Deny), TOOLS);
    assert_eq!(b.handle(&tool("ask", &["run", "a"])).status, Status::Denied);
    assert_eq!(fixed.asked.load(Ordering::SeqCst), 1);

    let (_t, b, fixed) = tool_setup(Some(Answer::Session), TOOLS);
    let r = b.handle(&tool("ask", &["run", "a", "b"]));
    assert_eq!(r.status, Status::Done, "{}", r.message);
    assert!(r.result["stdout"]
        .as_str()
        .unwrap()
        .contains("args:run a b\n"));
    b.handle(&tool("ask", &["run", "a", "b"]));
    assert_eq!(
        fixed.asked.load(Ordering::SeqCst),
        1,
        "the same arguments are not asked again"
    );
    b.handle(&tool("ask", &["run", "c"]));
    assert_eq!(
        fixed.asked.load(Ordering::SeqCst),
        2,
        "other arguments are asked again"
    );
}

#[test]
fn host_tool_deny_turns_every_tool_off() {
    let (_t, b, _) = tool_setup(
        Some(Answer::Once),
        &format!("[host]\ntool = \"deny\"\n{TOOLS}"),
    );
    assert_eq!(
        b.handle(&tool("compose", &["up", "-d"])).status,
        Status::Disabled
    );
}

#[test]
fn the_executor_checks_a_tool_request_again() {
    // A signed request for arguments that the configuration does not allow
    // (the broker would never sign it) still does not run.
    let (_t, b, _) = tool_setup(None, TOOLS);
    let e = b
        .execute(&tool("compose", &["down"]), serde_json::Value::Null)
        .unwrap_err();
    assert!(format!("{e:#}").contains("no shape"), "{e:#}");
}

#[test]
fn capabilities_list_the_tools() {
    let (_t, b, _) = tool_setup(None, TOOLS);
    let r = b.handle(&req(
        "capabilities",
        serde_json::json!({ "list_tools": true }),
    ));
    let tools = r.result["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 4);
    let c = tools.iter().find(|t| t["name"] == "compose").unwrap();
    assert_eq!(c["args"], serde_json::json!(["up -d", "logs *"]));
    assert_eq!(c["mode"], "allow");
}

#[test]
fn an_allowed_plain_exec_line_is_logged_for_suggest_only_when_asked() {
    let (_t, b, _) = setup(Some(Answer::Once), HostPolicy::default());
    // Off by default: no command line in the log.
    b.handle(&exec(&["echo", "a"]));
    let cfg = b.env.paddock_config();
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(cfg.join("config.toml"), "[exec]\nremember_allowed = true\n").unwrap();
    b.handle(&exec(&["echo", "a"]));
    b.handle(&exec(&["echo", "--token", "x"]));
    let lines = audit_lines(&b);
    assert!(lines[0].get("argv").is_none(), "{}", lines[0]);
    assert_eq!(lines[0]["exit_code"], 0);
    assert_eq!(lines[1]["argv"], serde_json::json!(["echo", "a"]));
    assert!(lines[2].get("argv").is_none());
}

#[test]
fn borrow_and_return_through_the_broker() {
    let (t, b, fixed) = setup(Some(Answer::Once), HostPolicy::default());
    let o = t.path().join("shared/lib");
    std::fs::create_dir_all(o.join("src")).unwrap();
    std::fs::write(o.join("src/a.rs"), "old\n").unwrap();
    let r = b.handle(&req("borrow", serde_json::json!({ "path": o })));
    assert_eq!(r.status, Status::Done, "{}", r.message);
    assert_eq!(r.result["path"], "/run/paddock/borrow/lib");
    let copy = crate::borrow::dir(&b.env, "t-1").join("lib");
    std::fs::write(copy.join("src/a.rs"), "new\n").unwrap();
    std::fs::write(copy.join("added"), "x").unwrap();
    // The original does not change before the return.
    assert_eq!(
        std::fs::read_to_string(o.join("src/a.rs")).unwrap(),
        "old\n"
    );
    let r = b.handle(&req("return", serde_json::json!({ "name": "lib" })));
    assert_eq!(r.status, Status::Done, "{}", r.message);
    assert_eq!(r.result["added"], 1);
    assert_eq!(r.result["changed"], 1);
    assert_eq!(
        std::fs::read_to_string(o.join("src/a.rs")).unwrap(),
        "new\n"
    );
    assert_eq!(std::fs::read_to_string(o.join("added")).unwrap(), "x");
    assert_eq!(fixed.asked.load(Ordering::SeqCst), 2);

    // A change to the original after the borrow: the return refuses.
    let r = b.handle(&req("borrow", serde_json::json!({ "path": o })));
    assert_eq!(r.status, Status::Done, "{}", r.message);
    std::fs::write(copy.join("src/a.rs"), "agent\n").unwrap();
    std::fs::write(o.join("src/a.rs"), "user\n").unwrap();
    let r = b.handle(&req("return", serde_json::json!({ "name": "lib" })));
    assert_eq!(r.status, Status::Invalid, "{}", r.message);
    assert!(
        r.message.contains("changed after the borrow"),
        "{}",
        r.message
    );
    assert_eq!(
        std::fs::read_to_string(o.join("src/a.rs")).unwrap(),
        "user\n"
    );
    assert_eq!(fixed.asked.load(Ordering::SeqCst), 3);

    // The audit log keeps paths and counts, never content.
    let log = std::fs::read_to_string(audit_file(&b.env)).unwrap();
    assert!(log.contains("1 added, 1 changed, 0 deleted"), "{log}");
    assert!(!log.contains("agent") && !log.contains("new\\n"), "{log}");
}

#[test]
fn borrow_refuses_protected_paths_and_relative_paths() {
    let (t, b, fixed) = setup(Some(Answer::Once), HostPolicy::default());
    std::fs::create_dir_all(b.env.home.join(".ssh")).unwrap();
    for p in [b.env.home.clone(), b.env.home.join(".ssh")] {
        let r = b.handle(&req("borrow", serde_json::json!({ "path": p })));
        assert_eq!(r.status, Status::Invalid, "{}", r.message);
    }
    let r = b.handle(&req("borrow", serde_json::json!({ "path": "rel/dir" })));
    assert_eq!(r.status, Status::Invalid);
    let r = b.handle(&req("return", serde_json::json!({ "name": "../x" })));
    assert_eq!(r.status, Status::Invalid);
    let r = b.handle(&req("return", serde_json::json!({ "name": "none" })));
    assert_eq!(r.status, Status::Invalid);
    assert_eq!(fixed.asked.load(Ordering::SeqCst), 0);
    // A denied borrow copies nothing.
    let (t2, b2, _) = setup(Some(Answer::Deny), HostPolicy::default());
    let o = t2.path().join("lib");
    std::fs::create_dir_all(&o).unwrap();
    let r = b2.handle(&req("borrow", serde_json::json!({ "path": o })));
    assert_eq!(r.status, Status::Denied);
    assert!(crate::borrow::list(&b2.env, "t-1").is_empty());
    drop(t);
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

/// A broker with fake `tea` and `glab` on the host's PATH, and a project
/// with remotes on codeberg.org and a self-hosted GitLab. The fakes print
/// their arguments, one per line, and their environment.
fn forge_setup(answer: Option<Answer>) -> (tempfile::TempDir, Broker, Arc<Fixed>) {
    use std::os::unix::fs::PermissionsExt;
    let (tmp, mut b, fixed, _) = setup_repo(answer, HostPolicy::default());
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    for tool in ["tea", "glab"] {
        let script = bin.join(tool);
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho \"tool:{tool}\"\nfor a in \"$@\"; do echo \"arg:$a\"; done\necho \"stdin:$(cat)\"\nenv | sort\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    b.env.path = vec![bin, "/usr/bin".into(), "/bin".into()];
    let tea_cfg = b.env.config_dir.join("tea");
    std::fs::create_dir_all(&tea_cfg).unwrap();
    std::fs::write(
        tea_cfg.join("config.yml"),
        "logins:\n- name: cb\n  url: https://codeberg.org\n  token: host-token\n  default: true\n",
    )
    .unwrap();
    let p = b.ctx.project.clone();
    git_in(
        &p,
        &[
            "remote",
            "add",
            "forgejo",
            "https://codeberg.org/demo/tool.git",
        ],
    );
    git_in(
        &p,
        &[
            "remote",
            "add",
            "lab",
            "git@gitlab.example.org:team/apps/web.git",
        ],
    );
    b.ctx.repo = git::Repo::snapshot(&p);
    (tmp, b, fixed)
}

fn forge_req(tool: &str, argv: &[&str]) -> Request {
    req(
        "forge",
        serde_json::json!({
            "tool": tool,
            "argv": argv,
            "branch": "fix",
            // A sandbox cannot hand the tool variables: this is ignored.
            "env": { "GITEA_TOKEN": SECRET, "GITLAB_TOKEN": SECRET },
        }),
    )
}

/// The arguments and the environment that a fake forge tool printed.
fn fake_output(r: &Response) -> (Vec<String>, String) {
    let out = r.result["stdout"].as_str().unwrap_or("").to_string();
    assert!(!out.contains(SECRET), "{out}");
    for line in out
        .lines()
        .filter(|l| l.contains('=') && !l.starts_with("arg:"))
    {
        let k = line.split('=').next().unwrap();
        assert!(
            !k.contains("TOKEN"),
            "a token variable reached the tool: {line}"
        );
    }
    let args = out
        .lines()
        .filter_map(|l| l.strip_prefix("arg:"))
        .map(String::from)
        .collect();
    (args, out)
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
