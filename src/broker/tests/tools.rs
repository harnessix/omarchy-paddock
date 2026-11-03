use super::*;
use std::collections::BTreeMap;
use std::process::Command;
use std::time::Instant;

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
fn exec_runs_in_the_project_directory() {
    let (_t, b, _) = setup(Some(Answer::Once), HostPolicy::default());
    let r = b.handle(&exec(&["pwd"]));
    assert_eq!(
        r.result["stdout"].as_str().unwrap().trim(),
        b.ctx.project.display().to_string()
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
fn a_process_left_behind_does_not_hang_a_capture() {
    let start = Instant::now();
    let mut c = Command::new("sh");
    c.args(["-c", "echo out; echo err >&2; sleep 30 &"]);
    let (code, out, err) = run_capture(&mut c, Duration::from_secs(30)).unwrap();
    assert_eq!((code, out.as_str(), err.as_str()), (0, "out\n", "err\n"));
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "{:?}",
        start.elapsed()
    );
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
