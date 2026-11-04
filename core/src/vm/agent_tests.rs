use super::*;
use std::io::{Read, Write};
use std::time::{Duration, Instant};

fn wait_for(path: &Path) {
    let end = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(Instant::now() < end, "{} never came", path.display());
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn request(token: &str, project: &Path, tty: bool) -> proto::RunRequest {
    proto::RunRequest {
        v: proto::VERSION,
        token: token.into(),
        id: "fake-p-0001".into(),
        harness: "fake".into(),
        project: project.to_path_buf(),
        network: proto::GuestNet::None,
        args: vec!["--task".into(), "two words".into()],
        tty,
        rows: 30,
        cols: 100,
        env: [("TERM".to_string(), "xterm".to_string())].into(),
        shell: false,
        no_yolo: false,
        profile: None,
        owner: None,
    }
}

/// The agent on unix sockets with a fake paddock: a shell script that
/// prints what it got, reads one line and exits with code 7.
fn agent_on_unix(t: &Path) -> (PathBuf, PathBuf) {
    let ports_dir = t.join("ports");
    let project = t.join("project");
    for d in [&ports_dir, &project] {
        std::fs::create_dir_all(d).unwrap();
    }
    let fake = t.join("paddock");
    std::fs::write(
        &fake,
        "#!/bin/sh\necho \"args: $*\"\necho \"pwd: $(pwd)\"\necho \"home: $HOME term: $TERM\"\necho \"host: ${PADDOCK_VM_HOST_SOCKET##*/}\"\nif [ -t 0 ]; then echo tty; else echo no tty; fi\nread line\necho \"got $line\"\nexit 7\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let agent = Agent::new(Config {
        transport: Transport::Unix(ports_dir.clone()),
        paddock: fake,
        user: None,
        project_owner: false,
        mounts: false,
        run_dir: t.join("run"),
        home: Some(t.join("home")),
    });
    std::thread::spawn(move || {
        let _ = serve(agent);
    });
    wait_for(&ports_dir.join("1024.sock"));
    wait_for(&ports_dir.join("1027.sock"));
    (ports_dir, project)
}

fn one_run(tty: bool) -> (String, proto::Reply, PathBuf) {
    let t = tempfile::tempdir().unwrap();
    // The host sends a canonical project path. On macOS the temporary
    // folder is under /var, a link to /private/var.
    let root = t.path().canonicalize().unwrap();
    let (ports_dir, project) = agent_on_unix(&root);
    let token = proto::new_token().unwrap();

    let mut control = UnixStream::connect(ports_dir.join("1024.sock")).unwrap();
    proto::write_line(
        &mut control,
        &proto::Request::Run(request(&token, &project, tty)),
    )
    .unwrap();
    let r: proto::Reply = proto::read_line(&mut control).unwrap().unwrap();
    assert_eq!(r, proto::Reply::ok());

    let mut pty = UnixStream::connect(ports_dir.join("1027.sock")).unwrap();
    proto::write_line(
        &mut pty,
        &proto::Request::Attach {
            token: token.clone(),
        },
    )
    .unwrap();
    if tty {
        proto::write_line(&mut control, &proto::Request::Resize { rows: 50, cols: 90 }).unwrap();
    }
    pty.write_all(b"hello\n").unwrap();
    let mut out = String::new();
    pty.read_to_string(&mut out).unwrap();
    let r: proto::Reply = proto::read_line(&mut control).unwrap().unwrap();
    // The run folder is gone after the run.
    assert!(!root.join("run").join(&token[..16]).exists());
    (out, r, project)
}

#[test]
fn one_run_end_to_end_with_stdio() {
    let (out, reply, project) = one_run(false);
    assert_eq!(reply, proto::Reply::Exit { code: 7 });
    assert!(
        out.contains(&format!(
            "args: run fake {} --backend bwrap --network none --yes -- --task two words",
            project.display()
        )),
        "{out}"
    );
    assert!(
        out.contains(&format!("pwd: {}", project.display())),
        "{out}"
    );
    assert!(out.contains("term: xterm"), "{out}");
    assert!(out.contains("host: host.sock"), "{out}");
    assert!(out.contains("no tty"), "{out}");
    assert!(out.contains("got hello"), "{out}");
}

#[test]
fn one_run_end_to_end_with_a_pty() {
    let (out, reply, _) = one_run(true);
    assert_eq!(reply, proto::Reply::Exit { code: 7 });
    assert!(out.contains("\ntty") || out.contains("tty\r\n"), "{out}");
    assert!(out.contains("got hello"), "{out}");
}

#[test]
fn a_bad_request_and_a_ping_get_a_reply() {
    let t = tempfile::tempdir().unwrap();
    let (ports_dir, project) = agent_on_unix(t.path());
    let mut c = UnixStream::connect(ports_dir.join("1024.sock")).unwrap();
    let mut r = request("nottoken", &project, false);
    r.v = proto::VERSION;
    proto::write_line(&mut c, &proto::Request::Run(r)).unwrap();
    match proto::read_line::<proto::Reply>(&mut c).unwrap().unwrap() {
        proto::Reply::Error { error } => assert!(error.contains("token"), "{error}"),
        other => panic!("{other:?}"),
    }
    let mut c = UnixStream::connect(ports_dir.join("1024.sock")).unwrap();
    proto::write_line(&mut c, &proto::Request::Ping).unwrap();
    assert_eq!(
        proto::read_line::<proto::Reply>(&mut c).unwrap().unwrap(),
        proto::Reply::Ok { runs: Some(0) }
    );
    // A pty connection without a waiting run gets nothing.
    let mut p = UnixStream::connect(ports_dir.join("1027.sock")).unwrap();
    proto::write_line(
        &mut p,
        &proto::Request::Attach {
            token: "0".repeat(32),
        },
    )
    .unwrap();
    let mut out = Vec::new();
    let _ = p.read_to_end(&mut out);
    assert!(out.is_empty());
}

#[test]
fn an_outlet_sends_the_token_first() {
    let t = tempfile::tempdir().unwrap();
    let ports_dir = t.path().join("ports");
    std::fs::create_dir(&ports_dir).unwrap();
    let host = UnixListener::bind(ports_dir.join("1025.sock")).unwrap();
    let token = proto::new_token().unwrap();
    let outlet = Outlet::start(
        Transport::Unix(ports_dir),
        t.path().join("host.sock"),
        ports::BROKER,
        token.clone(),
        None,
    )
    .unwrap();
    let mut c = UnixStream::connect(&outlet.path).unwrap();
    c.write_all(b"request").unwrap();
    c.shutdown(std::net::Shutdown::Write).unwrap();
    let (mut h, _) = host.accept().unwrap();
    let mut got = String::new();
    h.read_to_string(&mut got).unwrap();
    assert_eq!(got, format!("{token}\nrequest"));
    let path = outlet.path.clone();
    drop(outlet);
    assert!(!path.exists());
}

#[test]
fn the_guest_command_maps_the_network() {
    let t = tempfile::tempdir().unwrap();
    let mut r = request(&"a".repeat(32), Path::new("/p"), true);
    r.network = proto::GuestNet::Proxy;
    r.no_yolo = true;
    r.args.clear();
    r.env.insert("LD_PRELOAD".into(), "x".into());
    let (a, env) = super::super::agent_run::command(&r, Path::new("/h"), t.path());
    assert_eq!(
        a,
        [
            "run",
            "fake",
            "/p",
            "--backend",
            "bwrap",
            "--network",
            "api",
            "--yes",
            "--no-yolo"
        ]
    );
    assert_eq!(env["HOME"], "/h");
    assert!(env[PROXY_SOCKET_VAR].ends_with("proxy.sock"));
    assert!(!env.contains_key("LD_PRELOAD"));
}
