use super::*;
use std::collections::BTreeMap;
use std::os::fd::{FromRawFd, OwnedFd};

fn env_in(t: &Path, extra: &[(&str, &str)]) -> HostEnv {
    let run = t.join("run");
    crate::privfs::dir(&run).unwrap();
    let mut vars = BTreeMap::new();
    vars.insert("HOME".into(), t.join("home").display().to_string());
    vars.insert("XDG_RUNTIME_DIR".into(), run.display().to_string());
    vars.insert(
        "XDG_CONFIG_HOME".into(),
        t.join("cfg").display().to_string(),
    );
    vars.insert(
        "XDG_STATE_HOME".into(),
        t.join("state").display().to_string(),
    );
    vars.insert("PATH".into(), std::env::var("PATH").unwrap_or_default());
    for (k, v) in extra {
        vars.insert(k.to_string(), v.to_string());
    }
    HostEnv::from_parts(t.join("home"), "demo".into(), 1000, 1000, vars)
}

/// The tmux server of a test: a private socket in its folder, killed at
/// the end.
struct Server(Option<Tmux>);
impl Drop for Server {
    fn drop(&mut self) {
        if let Some(t) = &self.0 {
            let _ = t.cmd().arg("kill-server").status();
        }
    }
}

/// A fake harness: output on both streams, bytes that are not text, exit 7.
const HARNESS: &str = "printf 'out one\\n'; printf 'err one\\n' >&2; printf '\\001\\377\\r\\n'; \
                       sleep 0.3; printf 'out two' ; printf 'err two' >&2; exit 7";

fn pipe() -> (OwnedFd, OwnedFd) {
    let mut fds = [0; 2];
    assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
    unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
}

/// Run the fake harness with the view: its streams go through a `Tee` into
/// two pipes that stand for paddock's stdout and stderr.
fn run_with_view(env: &HostEnv, id: &str) -> (Vec<u8>, Vec<u8>, i32, bool) {
    use std::os::fd::AsRawFd;
    let live = Live::start(env, id, "fake", true, true, true).expect("a view");
    let session = live.session;
    let mut live = live;
    let (out_r, out_w) = pipe();
    let (err_r, err_w) = pipe();
    let mut child = Command::new("sh")
        .args(["-c", HARNESS])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    live.tee
        .copy(child.stdout.take().unwrap(), Sink::Fd(out_w.as_raw_fd()));
    live.tee
        .copy(child.stderr.take().unwrap(), Sink::Fd(err_w.as_raw_fd()));
    let reader = |fd: OwnedFd| {
        std::thread::spawn(move || {
            let mut v = Vec::new();
            std::fs::File::from(fd).read_to_end(&mut v).unwrap();
            v
        })
    };
    let (ro, re) = (reader(out_r), reader(err_r));
    let code = child.wait().unwrap().code().unwrap();
    live.finish(code);
    drop(out_w);
    drop(err_w);
    (ro.join().unwrap(), re.join().unwrap(), code, session)
}

#[test]
fn the_caller_sees_the_same_output_and_exit_code_with_and_without_tmux() {
    let direct = Command::new("sh").args(["-c", HARNESS]).output().unwrap();
    let t = tempfile::tempdir().unwrap();

    // Without tmux: [view] tmux = false.
    let off = env_in(t.path(), &[]);
    crate::privfs::dir(&off.paddock_config()).unwrap();
    std::fs::write(
        off.paddock_config().join("config.toml"),
        "[view]\ntmux = false\n",
    )
    .unwrap();
    let (o, e, code, session) = run_with_view(&off, "sbnotmux");
    assert!(!session);
    assert_eq!(o, direct.stdout);
    assert_eq!(e, direct.stderr);
    assert_eq!(code, 7);
    assert_eq!(Some(code), direct.status.code());
    // The log has both streams, the end file the exit code.
    let log = std::fs::read(log_file(&off, "sbnotmux")).unwrap();
    assert_eq!(log.len(), direct.stdout.len() + direct.stderr.len());
    let end = std::fs::read_to_string(end_file(&off, "sbnotmux")).unwrap();
    assert!(end.contains("exit code 7"), "{end}");

    // With tmux, when it is installed.
    let t2 = tempfile::tempdir().unwrap();
    let on = env_in(t2.path(), &[]);
    let Some(tmux) = Tmux::find(&on) else {
        eprintln!("skipped: tmux is not installed");
        return;
    };
    assert!(tmux.sock.starts_with(t2.path()));
    let _server = Server(Some(tmux.clone()));
    let (o, e, code, session) = run_with_view(&on, "sbtmux");
    assert!(session, "the tmux session did not start");
    assert_eq!(o, direct.stdout);
    assert_eq!(e, direct.stderr);
    assert_eq!(code, 7);
}

fn capture(t: &Tmux, id: &str) -> String {
    let out = t
        .cmd()
        .args(["capture-pane", "-p", "-S", "-", "-t", &format!("={id}:")])
        .stdout(Stdio::piped())
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
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

#[test]
fn the_session_lives_while_the_harness_runs_stays_after_and_gc_removes_it() {
    let t = tempfile::tempdir().unwrap();
    let env = env_in(t.path(), &[]);
    let Some(tmux) = Tmux::find(&env) else {
        eprintln!("skipped: tmux is not installed");
        return;
    };
    let _server = Server(Some(tmux.clone()));
    let id = "sbabc123";
    // The pane follows the log until this process ends: here a stand-in
    // for the paddock process.
    let mut paddock = Command::new("sleep").arg("30").spawn().unwrap();
    let log = log_file(&env, id);
    let mut tee = Tee::create(&log).unwrap();
    let end = end_file(&env, id);
    tmux.start(id, &header(id, "fake", 0), &log, paddock.id(), &end)
        .unwrap();
    let (r, w) = pipe();
    tee.copy(std::fs::File::from(r), Sink::Capture);
    write_fd(std::os::fd::AsRawFd::as_raw_fd(&w), b"working on it\n").unwrap();
    assert!(tmux.has(id));
    assert_eq!(tmux.sessions(), vec![id.to_string()]);
    let (view, has_tmux) = available(&env, &tmux.sessions(), id);
    assert!(view && has_tmux);
    wait_for("the output in the pane", || {
        capture(&tmux, id).contains("working on it")
    });
    let text = capture(&tmux, id);
    assert!(text.contains(&format!("paddock: sandbox {id}, harness fake, started")));
    assert!(text.contains("One-shot output appears at the end."));
    drop(w);
    tee.finish(Duration::from_secs(1));
    crate::privfs::write(&end, b"paddock: the sandbox ended with exit code 7.\n").unwrap();
    // The paddock process ends: the pane shows the end and stays.
    paddock.kill().unwrap();
    paddock.wait().unwrap();
    wait_for("the end line", || {
        capture(&tmux, id).contains("exit code 7")
    });
    std::thread::sleep(Duration::from_millis(300));
    assert!(tmux.has(id), "remain-on-exit keeps the session");

    // gc: not while it runs, not within a day of its end, after that yes.
    let ended = std::fs::metadata(&end).unwrap().mtime() as u64;
    let sessions = tmux.sessions();
    assert!(stale(&env, &sessions, &[id.to_string()], ended + 2 * MAX_AGE).is_empty());
    assert!(stale(&env, &sessions, &[], ended + 3600).is_empty());
    assert_eq!(
        stale(&env, &sessions, &[], ended + MAX_AGE + 1),
        vec![id.to_string()]
    );
    remove(&env, id).unwrap();
    assert!(!tmux.has(id));
    assert!(!log.exists() && !end.exists());
}

#[test]
fn stop_ends_the_session() {
    let t = tempfile::tempdir().unwrap();
    let env = env_in(t.path(), &[]);
    let Some(tmux) = Tmux::find(&env) else {
        eprintln!("skipped: tmux is not installed");
        return;
    };
    let _server = Server(Some(tmux.clone()));
    let log = log_file(&env, "sbstop");
    Tee::create(&log).unwrap();
    tmux.start(
        "sbstop",
        "h",
        &log,
        std::process::id(),
        &end_file(&env, "sbstop"),
    )
    .unwrap();
    assert!(tmux.has("sbstop"));
    assert!(end_session(&env, "sbstop"));
    assert!(!tmux.has("sbstop"));
    // The log stays for `paddock view` and gc.
    assert!(log.exists());
}

#[test]
fn the_socket_folder_is_private() {
    let t = tempfile::tempdir().unwrap();
    let env = env_in(t.path(), &[]);
    let Some(tmux) = Tmux::find(&env) else {
        eprintln!("skipped: tmux is not installed");
        return;
    };
    let _server = Server(Some(tmux.clone()));
    let log = log_file(&env, "sbmode");
    Tee::create(&log).unwrap();
    tmux.start("sbmode", "h", &log, std::process::id(), Path::new("/x"))
        .unwrap();
    let mode = |p: &Path| std::fs::metadata(p).unwrap().mode() & 0o777;
    assert_eq!(mode(tmux.sock.parent().unwrap()), 0o700);
    assert_eq!(mode(&dir(&env)), 0o700);
    assert_eq!(mode(&log), 0o600);
}

#[test]
fn view_opens_tmux_or_the_log_in_the_right_terminal() {
    let t = tempfile::tempdir().unwrap();
    let env = env_in(t.path(), &[]);
    let tmux_exe = env.which("tmux");
    let mut o = Opener {
        xdg_terminal_exec: Some("/usr/bin/xdg-terminal-exec".into()),
        terminal: Some("foot".into()),
        tmux: None,
        log: log_file(&env, "sbview"),
    };
    let s = |v: Vec<OsString>| -> Vec<String> {
        v.into_iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    };
    // No session and no log: an error that says why.
    let e = o.command("sbview", false).unwrap_err().to_string();
    assert!(e.contains("tmux is not installed"), "{e}");
    assert!(o.command("../x", false).is_err());

    // The log fallback, through xdg-terminal-exec and then $TERMINAL.
    Tee::create(&o.log).unwrap();
    let log = o.log.display().to_string();
    assert_eq!(
        s(o.command("sbview", false).unwrap()),
        [
            "/usr/bin/xdg-terminal-exec",
            "tail",
            "-n",
            "+1",
            "-f",
            "--",
            &log
        ]
    );
    o.xdg_terminal_exec = None;
    assert_eq!(
        s(o.command("sbview", true).unwrap()),
        ["foot", "-e", "tail", "-n", "+1", "-f", "--", &log]
    );
    o.terminal = None;
    let e = o.command("sbview", false).unwrap_err().to_string();
    assert!(e.contains("xdg-terminal-exec"), "{e}");

    // A tmux session: attach, read-only unless --write.
    let Some(exe) = tmux_exe else {
        eprintln!("skipped: tmux is not installed");
        return;
    };
    let tmux = Tmux::find(&env).unwrap();
    let _server = Server(Some(tmux.clone()));
    tmux.start(
        "sbview",
        "h",
        &o.log,
        std::process::id(),
        &end_file(&env, "sbview"),
    )
    .unwrap();
    o.tmux = Some(tmux.clone());
    o.xdg_terminal_exec = Some("/usr/bin/xdg-terminal-exec".into());
    let exe = exe.display().to_string();
    let sock = tmux.sock.display().to_string();
    assert_eq!(
        s(o.command("sbview", false).unwrap()),
        [
            "/usr/bin/xdg-terminal-exec",
            &exe,
            "-S",
            &sock,
            "attach",
            "-r",
            "-t",
            "sbview"
        ]
    );
    o.xdg_terminal_exec = None;
    o.terminal = Some("alacritty".into());
    assert_eq!(
        s(o.command("sbview", true).unwrap()),
        [
            "alacritty",
            "-e",
            &exe,
            "-S",
            &sock,
            "attach",
            "-t",
            "sbview"
        ]
    );
}

#[test]
fn the_view_table_parses() {
    assert!(Config::parse("").unwrap().tmux);
    assert!(!Config::parse("[view]\ntmux = false\n").unwrap().tmux);
    assert!(Config::parse("[view]\ntmux = true\n").unwrap().tmux);
    assert!(Config::parse("[view]\nsocket = \"/x\"\n").is_err());
    // The user configuration takes it; a project file does not.
    assert!(crate::manifest::Manifest::parse_user("[view]\ntmux = false\n").is_ok());
    assert!(toml::from_str::<crate::manifest::Manifest>("[view]\ntmux = false\n").is_err());
}

#[test]
fn the_run_log_stops_at_its_cap() {
    let t = tempfile::tempdir().unwrap();
    let mut l = LogFile {
        file: Some(std::fs::File::create(t.path().join("l")).unwrap()),
        written: LOG_CAP - 3,
    };
    l.write(b"abcdef");
    assert!(l.file.is_none());
    let text = std::fs::read_to_string(t.path().join("l")).unwrap();
    assert!(
        text.starts_with("abc\n[paddock: the run log is full"),
        "{text}"
    );
}

#[test]
fn only_plain_ids_name_files_and_sessions() {
    for ok in ["paddock-16cc60", "a_b", "abc123"] {
        assert!(valid_id(ok), "{ok}");
    }
    for bad in ["", "../x", "a.b", "a:b", "a b", "=a"] {
        assert!(!valid_id(bad), "{bad}");
    }
}
