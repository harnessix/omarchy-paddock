use super::*;
use crate::view::Tmux;
use std::collections::BTreeMap;
use std::process::{Command, Stdio};
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;

fn env_in(t: &Path) -> HostEnv {
    let run = t.join("run");
    crate::privfs::dir(&run).unwrap();
    let mut vars = BTreeMap::new();
    vars.insert("HOME".into(), t.join("home").display().to_string());
    vars.insert("XDG_RUNTIME_DIR".into(), run.display().to_string());
    vars.insert(
        "XDG_STATE_HOME".into(),
        t.join("state").display().to_string(),
    );
    vars.insert(
        "XDG_CONFIG_HOME".into(),
        t.join("cfg").display().to_string(),
    );
    vars.insert("PATH".into(), std::env::var("PATH").unwrap_or_default());
    HostEnv::from_parts(t.join("home"), "demo".into(), 1000, 1000, vars)
}

/// The private tmux server of a test, killed at the end.
struct Server(Tmux);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = Command::new(&self.0.exe)
            .arg("-S")
            .arg(&self.0.sock)
            .arg("kill-server")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let start = Instant::now();
    while !f() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "timed out: {what}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn record(env: &HostEnv, id: &str) {
    Record {
        id: id.into(),
        harness: "fake".into(),
        started: crate::state::now(),
        pid: Some(std::process::id()),
        ..Default::default()
    }
    .save(env)
    .unwrap();
}

fn ctl(env: &HostEnv, id: &str) -> (Arc<Ctl>, Arc<AtomicUsize>) {
    let stops = Arc::new(AtomicUsize::new(0));
    let s = stops.clone();
    let c = Ctl::with_stop(
        env.clone(),
        id.into(),
        Box::new(move || {
            s.fetch_add(1, Ordering::SeqCst);
        }),
    );
    (Arc::new(c), stops)
}

#[test]
fn done_without_a_writer_ends_the_run_and_records_status_and_text() {
    let t = tempfile::tempdir().unwrap();
    let env = env_in(t.path());
    record(&env, "fake-p-1");
    let (c, stops) = ctl(&env, "fake-p-1");
    let long = "x".repeat(10_000);
    let msg = c
        .done(Status::Failed, &format!("bad\x1b[2Jend {long}"))
        .unwrap();
    assert!(msg.contains("ends this run"), "{msg}");
    wait_for("the stop", || stops.load(Ordering::SeqCst) == 1);
    let d = Record::load(&env, "fake-p-1").unwrap().done.unwrap();
    assert_eq!(d.status, Status::Failed);
    assert!(d.text.starts_with("bad [2Jend x"), "{}", &d.text[..20]);
    assert!(d.text.len() <= MAX_TEXT);
}

#[test]
fn a_write_client_keeps_the_run_until_it_leaves() {
    let t = tempfile::tempdir().unwrap();
    let env = env_in(t.path());
    let Some(tmux) = Tmux::find(&env) else {
        eprintln!("skipped: needs tmux");
        return;
    };
    let server = Server(tmux.clone());
    let id = "fake-p-2";
    tmux.new_session(id, &["sleep".into(), "60".into()])
        .unwrap();
    record(&env, id);
    // A read-only client does not count.
    let attach = |ro: bool| {
        let mut c = Command::new(&server.0.exe);
        c.arg("-S").arg(&server.0.sock).args(["-C", "attach"]);
        if ro {
            c.arg("-r");
        }
        c.args(["-t", id])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    };
    let mut reader = attach(true);
    wait_for("the read-only client", || {
        tmux.sessions().len() == 1
            && Command::new(&server.0.exe)
                .arg("-S")
                .arg(&server.0.sock)
                .args(["list-clients", "-F", "#{client_readonly}"])
                .output()
                .is_ok_and(|o| !o.stdout.is_empty())
    });
    assert_eq!(tmux.write_clients(id), 0);
    let mut writer = attach(false);
    wait_for("the write client", || tmux.write_clients(id) == 1);
    let (c, stops) = ctl(&env, id);
    let msg = c.done(Status::Ok, "all good").unwrap();
    assert!(msg.contains("stays open"), "{msg}");
    let msg = c.done(Status::Ok, "again").unwrap();
    assert!(msg.contains("stays open"), "{msg}");
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(
        stops.load(Ordering::SeqCst),
        0,
        "done ended a run with a writer"
    );
    // The writer closes the view: the run ends, once.
    let _ = writer.kill();
    let _ = writer.wait();
    wait_for("the stop after the view closed", || {
        stops.load(Ordering::SeqCst) == 1
    });
    std::thread::sleep(Duration::from_millis(1200));
    assert_eq!(stops.load(Ordering::SeqCst), 1);
    let _ = reader.kill();
    let _ = reader.wait();
    let rec = Record::load(&env, id).unwrap();
    assert_eq!(rec.done.unwrap().text, "again");
}

#[test]
fn wait_reports_the_status_and_times_out() {
    let t = tempfile::tempdir().unwrap();
    let env = env_in(t.path());
    // A run that still runs: this process.
    record(&env, "fake-p-3");
    let start = Instant::now();
    let code = wait(&env, "fake-p-3", Some(Duration::from_millis(300))).unwrap();
    assert_eq!(code, EXIT_TIMEOUT);
    assert!(start.elapsed() < Duration::from_secs(5));
    // It ends with done ok, then with done failed, then without done.
    for (done, want) in [(Some(Status::Ok), 0), (Some(Status::Failed), 1), (None, 1)] {
        Record::update(&env, "fake-p-3", |r| {
            r.ended = Some(crate::state::now());
            r.done = done.map(|status| Done {
                status,
                text: "summary".into(),
                at: 1,
            });
        })
        .unwrap();
        assert_eq!(wait(&env, "fake-p-3", None).unwrap(), want);
    }
    assert!(wait(&env, "fake-p-none", None).is_err());
}

#[test]
fn a_pane_that_never_started_counts_as_ended() {
    let t = tempfile::tempdir().unwrap();
    let env = env_in(t.path());
    let r = Record {
        id: "fake-p-4".into(),
        started: 1,
        ..Default::default()
    };
    assert!(ended(&env, "fake-p-4", Some(&r), 1 + START_GRACE + 1));
    assert!(!ended(&env, "fake-p-4", Some(&r), 2));
}

#[test]
fn the_text_stays_printable_and_short() {
    assert_eq!(clean_text("a\x07b\nc\td"), "a b\nc\td");
    let s = "é".repeat(MAX_TEXT);
    let c = clean_text(&s);
    assert!(c.len() <= MAX_TEXT && c.len() > MAX_TEXT - 2);
    assert_eq!("ok".parse::<Status>().unwrap(), Status::Ok);
    assert!("maybe".parse::<Status>().is_err());
}

fn fake_harness(table: &str) -> crate::profile::Harness {
    let profile: crate::profile::Profile = toml::from_str(&format!(
        "name = \"fake\"\n[launch]\nargs = [\"-i\"]\n{table}"
    ))
    .unwrap();
    crate::profile::Harness {
        profile,
        exe: PathBuf::from("/bin/sh"),
        install_mounts: vec![],
        path_dirs: vec![],
        source: "test".into(),
    }
}

#[test]
fn a_detached_build_gets_the_id_and_the_first_prompt_args() {
    let t = tempfile::tempdir().unwrap();
    let env = env_in(t.path());
    let project = t.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let opts = crate::build::Options {
        harness: "fake".into(),
        project: Some(project.clone()),
        network: Some(crate::spec::Network::None),
        inherit_env: Some(false),
        assume_yes: true,
        detach: Some("fake-project-abc123".into()),
        ..Default::default()
    };
    let b = crate::build::build_with(
        &env,
        &opts,
        fake_harness("[detach]\nargs = [\"Read {prompt_file} now.\"]\n"),
    )
    .unwrap();
    assert!(b.detach);
    assert_eq!(b.spec.id, "fake-project-abc123");
    let last = b.spec.command.last().unwrap();
    let file = b.spec.home.join(crate::spawn::prompt_rel(&b.spec.id));
    assert_eq!(*last, format!("Read {} now.", file.display()));
    assert!(
        b.spec.command.iter().any(|a| a == "-i"),
        "{:?}",
        b.spec.command
    );
    // Without [detach], the build refuses.
    let err = crate::build::build_with(&env, &opts, fake_harness(""))
        .err()
        .expect("refused");
    assert!(format!("{err:#}").contains("no [detach] table"), "{err:#}");
}

#[test]
fn the_broker_answers_done_only_in_a_detached_run() {
    use crate::broker::{Broker, Context, HostPolicy, Request, Status as RStatus, PROTOCOL};
    let t = tempfile::tempdir().unwrap();
    let env = env_in(t.path());
    let ctx = Context {
        id: "fake-p-5".into(),
        harness: "fake".into(),
        project: t.path().to_path_buf(),
        repo: None,
        chain: vec![],
        depth: 0,
        role: None,
    };
    let mut req = Request {
        v: PROTOCOL,
        cap: "done".into(),
        args: BTreeMap::new(),
        reason: String::new(),
    };
    req.args
        .insert("status".into(), serde_json::json!("failed"));
    req.args.insert("text".into(), serde_json::json!("summary"));
    let plain = Broker::new(
        env.clone(),
        ctx.clone(),
        HostPolicy::default(),
        Arc::new(crate::broker::HostApprover),
    );
    assert_eq!(plain.handle(&req).status, RStatus::Disabled);
    record(&env, "fake-p-5");
    let mut b = Broker::new(
        env.clone(),
        ctx,
        HostPolicy::default(),
        Arc::new(crate::broker::HostApprover),
    );
    let (c, stops) = ctl(&env, "fake-p-5");
    b.detach = Some(c);
    let r = b.handle(&req);
    assert_eq!(r.status, RStatus::Done, "{}", r.message);
    wait_for("the stop", || stops.load(Ordering::SeqCst) == 1);
    let d = Record::load(&env, "fake-p-5").unwrap().done.unwrap();
    assert_eq!((d.status, d.text.as_str()), (Status::Failed, "summary"));
    req.args.insert("status".into(), serde_json::json!("maybe"));
    assert_eq!(b.handle(&req).status, RStatus::Invalid);
}

fn ended_record(id: &str, project: &std::path::Path, ended_at: u64) -> Record {
    Record {
        id: id.into(),
        harness: "fake".into(),
        project: project.to_path_buf(),
        started: 1,
        ended: Some(ended_at),
        ..Default::default()
    }
}

#[test]
fn recent_ended_lists_done_stopped_and_drops_old_runs() {
    assert_eq!(ENDED_WINDOW, 10 * 60);
    assert_eq!(ENDED_KEEP, 8);
    assert_eq!(ENDED_TEXT, 200);
    let t = tempfile::tempdir().unwrap();
    let env = env_in(t.path());
    let project = t.path().join("project");
    let now = crate::state::now();

    record(&env, "fake-p-ok");
    Record::update(&env, "fake-p-ok", |r| {
        r.project = project.clone();
        r.harness = "fake".into();
    })
    .unwrap();
    let (ok, _) = ctl(&env, "fake-p-ok");
    ok.done(Status::Ok, "all good").unwrap();
    Record::update(&env, "fake-p-ok", |r| r.ended = Some(now - 2)).unwrap();

    record(&env, "fake-p-bad");
    let (bad, _) = ctl(&env, "fake-p-bad");
    bad.done(Status::Failed, "it broke").unwrap();
    Record::update(&env, "fake-p-bad", |r| {
        r.project = project.clone();
        r.ended = Some(now - 1);
    })
    .unwrap();

    record(&env, "fake-p-stop");
    mark_stopped(&env, "fake-p-stop").unwrap();
    Record::update(&env, "fake-p-stop", |r| {
        r.project = project.clone();
        r.ended = Some(now);
    })
    .unwrap();

    let unknown = ended_record("fake-p-none", &project, now - 20);
    unknown.save(&env).unwrap();

    let mut old = ended_record("fake-p-old", &project, now - ENDED_WINDOW - 1);
    old.done = Some(Done {
        status: Status::Ok,
        text: "too old".into(),
        at: 1,
    });
    old.save(&env).unwrap();

    let mut kept = ended_record("fake-p-edge", &project, now - ENDED_WINDOW);
    kept.done = Some(Done {
        status: Status::Ok,
        text: "just inside".into(),
        at: 1,
    });
    kept.save(&env).unwrap();

    let mut long = ended_record("fake-p-long", &project, now - 30);
    long.done = Some(Done {
        status: Status::Failed,
        text: format!("\u{0007}{}", "é".repeat(250)),
        at: 1,
    });
    long.save(&env).unwrap();

    // Still running: no end time.
    record(&env, "fake-p-live");

    let rows = recent_ended(&env, now);
    let ids: Vec<&str> = rows.iter().filter_map(|r| r["id"].as_str()).collect();
    assert_eq!(
        ids,
        vec![
            "fake-p-stop",
            "fake-p-bad",
            "fake-p-ok",
            "fake-p-none",
            "fake-p-long",
            "fake-p-edge",
        ],
        "{rows:?}"
    );
    assert!(!ids.contains(&"fake-p-old"));
    assert!(!ids.contains(&"fake-p-live"));

    let stop = &rows[0];
    assert_eq!(stop["status"], "stopped");
    assert_eq!(stop["text"], "");
    assert_eq!(stop["harness"], "fake");
    assert_eq!(stop["project"].as_str(), project.to_str());
    assert_eq!(stop["ended"].as_u64(), Some(now));

    assert_eq!(rows[1]["status"], "failed");
    assert_eq!(rows[1]["text"], "it broke");
    assert_eq!(rows[2]["status"], "ok");
    assert_eq!(rows[2]["text"], "all good");
    assert_eq!(rows[3]["status"], "unknown");
    assert_eq!(rows[3]["text"], "");

    let text = rows[4]["text"].as_str().unwrap();
    assert_eq!(text.chars().count(), 200);
    assert!(text.chars().all(|c| c == 'é'), "{text}");
    assert!(!text.contains('\u{0007}'));
}

#[test]
fn recent_ended_keeps_at_most_eight() {
    let t = tempfile::tempdir().unwrap();
    let env = env_in(t.path());
    let project = t.path().join("project");
    let now = crate::state::now();
    for i in 0..10 {
        let mut r = ended_record(&format!("fake-p-{i}"), &project, now - i);
        r.done = Some(Done {
            status: Status::Ok,
            text: "n".into(),
            at: 1,
        });
        r.save(&env).unwrap();
    }
    let rows = recent_ended(&env, now);
    assert_eq!(rows.len(), 8);
    assert_eq!(rows[0]["id"], "fake-p-0");
    assert_eq!(rows[7]["id"], "fake-p-7");
}

#[test]
fn a_view_run_records_its_end_and_a_stop() {
    let t = tempfile::tempdir().unwrap();
    let env = env_in(t.path());
    let project = t.path().join("project");
    record_view_end(&env, "fake-p-view", "fake", &project);
    let rows = recent_ended(&env, crate::state::now());
    assert_eq!(rows[0]["id"], "fake-p-view");
    assert_eq!(rows[0]["status"], "unknown");
    assert_eq!(rows[0]["harness"], "fake");

    mark_stopped(&env, "fake-p-halt").unwrap();
    record_view_end(&env, "fake-p-halt", "fake", &project);
    assert!(!stop_file(&env, "fake-p-halt").exists());
    let rec = Record::load(&env, "fake-p-halt").unwrap();
    assert!(rec.stopped);
    assert!(rec.ended.is_some());

    // A detached record keeps its done text.
    record(&env, "fake-p-det");
    Record::update(&env, "fake-p-det", |r| {
        r.done = Some(Done {
            status: Status::Ok,
            text: "kept".into(),
            at: 1,
        });
        r.ended = Some(crate::state::now());
    })
    .unwrap();
    record_view_end(&env, "fake-p-det", "other", &project);
    let rec = Record::load(&env, "fake-p-det").unwrap();
    assert_eq!(rec.harness, "fake");
    assert_eq!(rec.done.unwrap().text, "kept");
}
