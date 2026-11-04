use super::*;
use crate::spec::Mount;

/// A terminal spec as `paddock run` builds it, with paths that do not exist
/// on this machine: `real` keeps them as they are.
fn spec() -> (HostEnv, SandboxSpec) {
    let home = PathBuf::from("/Users/u");
    let env = HostEnv::from_parts(home.clone(), "u".into(), 501, 20, BTreeMap::new());
    let project = PathBuf::from("/Users/u/src/app");
    let mut s = SandboxSpec::new(
        "id1".into(),
        "fake",
        &project,
        "/Users/u/.local/share/paddock/homes/fake/x".into(),
        &home,
        501,
        20,
        "u",
    );
    s.add_mount(Mount::same(&project, true, "project"));
    s.add_mount(Mount::same(
        "/Users/u/src/app/.git/hooks",
        false,
        "detector git",
    ));
    s.add_mount(Mount::mask(
        "/Users/u/src/app/.env",
        false,
        "detector secrets",
    ));
    s.add_mount(Mount::same(
        "/Users/u/.local/share/mise/installs/node",
        false,
        "harness install",
    ));
    s.add_mount(Mount::bind(
        "/Users/u/.cache/paddock/shared/npm",
        "/Users/u/.npm",
        true,
        "cache",
    ));
    let run = env.paddock_run().join("id1");
    s.host_socket = Some(PathBuf::from("/private/tmp/paddock-501/ab/id1/host.sock"));
    s.kv_socket = Some(PathBuf::from("/private/tmp/paddock-501/ab/id1/kv.sock"));
    s.add_tool("paddock", "/Users/u/.cargo/bin/paddock".into(), "client");
    s.add_tool("paddock-host", run.join("paddock-host"), "client");
    s.network = Network::Egress;
    (env, s)
}

fn profile(env: &HostEnv, s: &SandboxSpec) -> String {
    let (p, _) = policy(env, s, &private_tmp(s)).unwrap();
    crate::seatbelt::render(&p).unwrap()
}

#[test]
fn the_project_is_writable_and_its_hidden_files_denied() {
    let (env, s) = spec();
    let sb = profile(&env, &s);
    assert!(sb.contains("(allow file-read* file-write* (subpath \"/Users/u/src/app\"))"));
    assert!(sb.contains("(deny file-write* (subpath \"/Users/u/src/app/.git/hooks\"))"));
    assert!(sb.contains("(deny file-read* file-write* (literal \"/Users/u/src/app/.env\"))"));
    let project = sb.find("(subpath \"/Users/u/src/app\")").unwrap();
    let env_file = sb.find("(literal \"/Users/u/src/app/.env\")").unwrap();
    assert!(project < env_file, "the deny must come after the project");
}

#[test]
fn the_sandbox_home_is_private_and_the_real_home_denied() {
    let (env, s) = spec();
    let sb = profile(&env, &s);
    assert!(sb.contains(
        "(allow file-read* file-write* (subpath \"/Users/u/.local/share/paddock/homes/fake/x\"))"
    ));
    for l in sb.lines().filter(|l| l.starts_with("(allow")) {
        assert!(!l.contains("(subpath \"/Users/u\")"), "{l}");
        assert!(!l.contains("(subpath \"/Users\")"), "{l}");
    }
    // The install folder of the harness, read-only, at its own path.
    assert!(
        sb.contains("(allow file-read* (subpath \"/Users/u/.local/share/mise/installs/node\"))")
    );
    // A host command: the file itself, not its folder in the real home.
    assert!(sb.contains("(allow file-read* (literal \"/Users/u/.cargo/bin/paddock\"))"));
    assert!(!sb.contains("(subpath \"/Users/u/.cargo/bin\")"));
}

#[test]
fn the_broker_and_kv_sockets_can_be_reached_but_not_replaced() {
    let (env, s) = spec();
    let sb = profile(&env, &s);
    for sock in ["host.sock", "kv.sock"] {
        let path = format!("/private/tmp/paddock-501/ab/id1/{sock}");
        assert!(
            sb.contains(&format!(
                "(allow network-outbound (remote unix-socket (path-literal \"{path}\")))"
            )),
            "{sock}"
        );
        assert!(!sb.contains(&format!("file-write* (literal \"{path}\")")));
    }
    let vars = run_env(&env, &s, &private_tmp(&s));
    assert_eq!(
        vars["PADDOCK_HOST_SOCKET"],
        "/private/tmp/paddock-501/ab/id1/host.sock"
    );
    assert_eq!(
        vars["PADDOCK_KV_SOCKET"],
        "/private/tmp/paddock-501/ab/id1/kv.sock"
    );
}

#[test]
fn egress_reaches_only_the_proxy_relay_and_allowed_host_ports() {
    let (env, mut s) = spec();
    s.add_host_port(8765, "manifest");
    set_proxy(&mut s, 41000);
    let sb = profile(&env, &s);
    let outbound: Vec<&str> = sb
        .lines()
        .filter(|l| l.starts_with("(allow network-outbound (remote ip"))
        .collect();
    assert_eq!(
        outbound,
        [
            "(allow network-outbound (remote ip \"localhost:8765\"))",
            "(allow network-outbound (remote ip \"localhost:41000\"))",
        ]
    );
    let vars = run_env(&env, &s, &private_tmp(&s));
    assert_eq!(vars["HTTPS_PROXY"], "http://127.0.0.1:41000");
    assert_eq!(vars["http_proxy"], "http://127.0.0.1:41000");
    assert!(vars["NO_PROXY"].contains("127.0.0.1"));
}

#[test]
fn api_reaches_only_the_proxy_relay() {
    let (env, mut s) = spec();
    s.network = Network::Api;
    s.add_host_port(8765, "manifest");
    set_proxy(&mut s, 41000);
    let sb = profile(&env, &s);
    assert!(sb.contains("(remote ip \"localhost:41000\")"));
    assert!(!sb.contains("localhost:8765"));
}

#[test]
fn network_none_has_no_ip_rule_and_no_proxy() {
    let (env, mut s) = spec();
    s.network = Network::None;
    s.set_env("HTTPS_PROXY", "http://127.0.0.1:7890", "host");
    s.add_host_port(7890, "host proxy");
    let sb = profile(&env, &s);
    assert!(!sb.contains("(remote ip"));
    assert!(!sb.contains("(local ip"));
    let vars = run_env(&env, &s, &private_tmp(&s));
    assert!(!vars.contains_key("HTTPS_PROXY"));
}

#[test]
fn remapped_mounts_are_named_not_dropped_in_silence() {
    let (env, s) = spec();
    let (_, notes) = policy(&env, &s, &private_tmp(&s)).unwrap();
    assert_eq!(notes.len(), 1);
    assert!(notes[0].contains("/Users/u/.npm (cache)"), "{notes:?}");
    assert!(!profile(&env, &s).contains("/Users/u/.npm"));
}

#[test]
fn the_pasture_stays_at_its_host_path() {
    let (env, mut s) = spec();
    let ground = PathBuf::from("/Users/u/.local/share/paddock/pastures/x/team");
    s.add_mount(Mount::bind(&ground, crate::pasture::MOUNT, true, "pasture"));
    let ports = PathBuf::from("/private/tmp/paddock-501/ports");
    s.add_mount(Mount::bind(
        &ports,
        crate::pasture::ports::PORTS_MOUNT,
        false,
        "ports",
    ));
    let sb = profile(&env, &s);
    assert!(sb.contains(&format!(
        "(allow file-read* file-write* (subpath \"{}\"))",
        ground.display()
    )));
    assert!(sb.contains(&format!(
        "(allow network-outbound (remote unix-socket (subpath \"{}\")))",
        ports.display()
    )));
    assert!(sb.contains(&format!(
        "(deny file-write* (subpath \"{}\"))",
        ports.display()
    )));
    let vars = run_env(&env, &s, &private_tmp(&s));
    assert_eq!(vars["PADDOCK_PASTURE"], ground.display().to_string());
}

#[test]
fn the_environment_points_into_the_sandbox_home() {
    let (env, mut s) = spec();
    s.set_env("XDG_RUNTIME_DIR", "/run/user/501", "linux");
    s.set_env("CARGO_HOME", "/Users/u/.cargo", "host");
    s.path = vec![
        PathBuf::from("/Users/u/.local/bin"),
        PathBuf::from("/Users/u/.local/share/mise/installs/node/bin"),
    ];
    let tmp = private_tmp(&s);
    let vars = run_env(&env, &s, &tmp);
    let home = s.home_src.display().to_string();
    assert_eq!(vars["HOME"], home);
    assert_eq!(vars["CFFIXED_USER_HOME"], home);
    assert_eq!(vars["TMPDIR"], tmp.display().to_string());
    assert_eq!(vars["CARGO_HOME"], format!("{home}/.cargo"));
    assert!(!vars.contains_key("XDG_RUNTIME_DIR"));
    let path: Vec<&str> = vars["PATH"].split(':').collect();
    // paddock's wrappers first, the host command folder last.
    let run = env.paddock_run().join("id1").display().to_string();
    assert_eq!(path[0], run);
    assert_eq!(*path.last().unwrap(), "/Users/u/.cargo/bin");
    assert!(path.contains(&format!("{home}/.local/bin").as_str()));
    assert!(path.contains(&"/Users/u/.local/share/mise/installs/node/bin"));
}

#[test]
fn the_command_enters_the_work_folder_and_moves_home_paths() {
    let (_, mut s) = spec();
    s.command = vec![
        "fake".into(),
        "--prompt-file".into(),
        "/Users/u/.paddock/prompt.md".into(),
    ];
    let a = command(&s, Path::new("/run/x/seatbelt.sb"));
    assert_eq!(a[..4], ["-f", "/run/x/seatbelt.sb", "/bin/sh", "-c"]);
    assert_eq!(a[6], "/Users/u/src/app");
    assert_eq!(a[7], "fake");
    assert_eq!(a[9], format!("{}/.paddock/prompt.md", s.home_src.display()));
}

#[test]
fn each_feature_seatbelt_cannot_give_is_refused() {
    type Change = Box<dyn Fn(&mut SandboxSpec)>;
    let cases: Vec<(&str, Change)> = vec![
        (
            "network mode 'host'",
            Box::new(|s| s.network = Network::Host),
        ),
        (
            "needs the srt backend",
            Box::new(|s| s.network = Network::Allowlist),
        ),
        (
            "display mode 'window'",
            Box::new(|s| s.gui.mode = GuiMode::Window),
        ),
        (
            "display mode 'headless'",
            Box::new(|s| s.gui.mode = GuiMode::Headless),
        ),
        (
            "devices of /dev/paddock",
            Box::new(|s| s.devices = Some("/x/dev".into())),
        ),
        (
            "login_ports",
            Box::new(|s| s.add_mount(Mount::bind("/x/in", crate::app::INBOUND_DIR, true, "login"))),
        ),
        ("private keyring", Box::new(|s| s.keyring = true)),
        (
            "cannot be published",
            Box::new(|s| {
                s.network = Network::None;
                s.publish = vec![3000];
            }),
        ),
        (
            "overlay",
            Box::new(|s| s.add_mount(Mount::overlay("/l", "/u", "/w", "/Users/u/.x", "layers"))),
        ),
    ];
    for (want, change) in cases {
        let (env, mut s) = spec();
        change(&mut s);
        let e = refuse(&s).unwrap_err().to_string();
        assert!(e.contains(want), "{want}: {e}");
        assert!(!e.contains('\n'), "one line: {e}");
        assert!(SeatbeltRun.render(&env, &s, false).is_err(), "{want}");
    }
    let (_, s) = spec();
    assert!(refuse(&s).is_ok());
}

#[test]
fn borrow_and_return_are_refused_by_the_broker() {
    let caps: Vec<&str> = BROKER_REFUSALS.iter().map(|(c, _)| *c).collect();
    assert_eq!(caps, ["borrow", "return"]);
}

#[cfg(not(target_os = "macos"))]
#[test]
fn linux_refuses_the_backend() {
    let (env, s) = spec();
    let e = SeatbeltRun.check(&env, &s).unwrap_err().to_string();
    assert!(e.contains("macOS only"), "{e}");
    assert!(crate::backend::by_name("seatbelt").is_ok());
}

/// A spec whose host home is a temporary folder, with `config` as the user
/// configuration.
fn temp_spec(config: &str) -> (tempfile::TempDir, HostEnv, SandboxSpec) {
    let t = tempfile::tempdir().unwrap();
    let home = t.path().join("home");
    std::fs::create_dir_all(home.join(".config/paddock")).unwrap();
    std::fs::write(home.join(".config/paddock/config.toml"), config).unwrap();
    let env = HostEnv::from_parts(home.clone(), "u".into(), 501, 20, BTreeMap::new());
    let project = home.join("src/app");
    let mut s = SandboxSpec::new(
        "id2".into(),
        "fake",
        &project,
        home.join(".local/share/paddock/homes/fake/x"),
        &home,
        501,
        20,
        "u",
    );
    s.network = Network::Egress;
    s.command = vec!["/opt/fake/bin/fake".into(), "--yolo".into()];
    (t, env, s)
}

#[test]
fn a_localhost_range_comes_from_the_user_configuration() {
    assert_eq!(parse_range("3000-3999").unwrap(), (3000, 3999));
    assert_eq!(parse_range("8080").unwrap(), (8080, 8080));
    for bad in ["0-10", "4000-3000", "1-2000", "x", "3000-", "70000"] {
        assert!(parse_range(bad).is_err(), "{bad}");
    }
    let (_t, env, mut s) = temp_spec("[macos]\nlocalhost_ports = \"3000-3002\"\n");
    assert_eq!(localhost_ports(&env).unwrap(), Some((3000, 3002)));
    let sb = profile(&env, &s);
    for port in [3000, 3001, 3002] {
        assert!(
            sb.contains(&format!("(remote ip \"localhost:{port}\")")),
            "{port}"
        );
    }
    assert!(!sb.contains("localhost:3003"));
    assert!(range_line((3000, 3002), Network::Egress).contains("3000-3002"));
    // Network modes api and none do not get it.
    for mode in [Network::Api, Network::None] {
        s.network = mode;
        assert!(!profile(&env, &s).contains("localhost:3000"), "{mode:?}");
        assert!(range_line((3000, 3002), mode).contains("does not get it"));
    }
    let (_t, env, s) = temp_spec("");
    assert_eq!(localhost_ports(&env).unwrap(), None);
    assert!(!profile(&env, &s).contains("localhost:3000"));
    let (_t, env, _) = temp_spec("[macos]\nlocalhost_ports = [3000]\n");
    assert!(localhost_ports(&env).is_err());
}

#[test]
fn the_command_sets_the_pty_library_right_before_the_harness() {
    let (_, mut s) = spec();
    s.command = vec!["/opt/fake/bin/fake".into()];
    s.setup = vec!["echo setup".into()];
    let a = command(&s, Path::new("/run/x/seatbelt.sb"));
    let script = &a[4];
    let setup = script.find("echo setup").unwrap();
    let dyld = script
        .find("export DYLD_INSERT_LIBRARIES=\"$PADDOCK_PTY_LIB\"")
        .unwrap();
    let exec = script.find("exec \"$@\"").unwrap();
    assert!(setup < dyld && dyld < exec, "{script}");
    // The harness itself follows, not a second shell for the setup lines.
    assert_eq!(a.last().unwrap(), "/opt/fake/bin/fake");
    assert!(!a[5..].iter().any(|x| x == "/bin/sh"), "{a:?}");
}

#[test]
fn the_pty_broker_starts_and_the_sandbox_reaches_it() {
    let (t, env, mut s) = temp_spec("");
    let env = HostEnv {
        runtime_dir: Some(t.path().join("run")),
        ..env
    };
    start_pty(&env, &mut s).unwrap();
    if crate::ptybroker::LIBRARY.is_empty() {
        assert!(!s.env.contains_key(crate::ptybroker::SOCKET_VAR));
        return;
    }
    let sock = PathBuf::from(&s.env[crate::ptybroker::SOCKET_VAR].value);
    let lib = PathBuf::from(&s.env[crate::ptybroker::LIBRARY_VAR].value);
    assert!(std::os::unix::net::UnixStream::connect(&sock).is_ok());
    assert_eq!(std::fs::read(&lib).unwrap(), crate::ptybroker::LIBRARY);
    assert!(lib.starts_with(env.paddock_run().join(&s.id)));
    // The profile lets the sandbox connect to the broker, and the library
    // lies in the read-only run folder.
    let sb = profile(&env, &s);
    assert!(sb.contains(&format!(
        "(allow network-outbound (remote unix-socket (path-literal \"{}\")))",
        crate::backend::seatbelt::real(&sock).display()
    )));
    assert!(sb.contains(&format!(
        "(allow file-read* (subpath \"{}\"))",
        crate::backend::seatbelt::real(&env.paddock_run().join(&s.id)).display()
    )));
    // Both stay at their host paths, also when they are in the host home.
    let vars = run_env(&env, &s, &private_tmp(&s));
    assert_eq!(
        vars[crate::ptybroker::SOCKET_VAR],
        sock.display().to_string()
    );
    assert_eq!(
        vars[crate::ptybroker::LIBRARY_VAR],
        lib.display().to_string()
    );
}

#[test]
fn a_harness_that_cannot_be_found_gets_no_pty() {
    let (_t, env, mut s) = temp_spec("");
    s.command = vec!["no-such-harness-here".into()];
    start_pty(&env, &mut s).unwrap();
    assert!(!s.env.contains_key(crate::ptybroker::SOCKET_VAR));
    assert!(!s.env.contains_key(crate::ptybroker::LIBRARY_VAR));
}

#[test]
fn codex_without_ptys_gets_its_shell_tool_without_a_terminal() {
    let profile = crate::profile::builtin("codex").unwrap();
    let with = |ptys: bool| {
        let (_t, env, mut s) = temp_spec("");
        crate::privacy::push_state_settings(&profile, false, "profile codex", &mut s).unwrap();
        if ptys {
            crate::backend::pty_settings(&mut s, true);
        } else {
            // A harness that paddock cannot find gets no ptys.
            s.command = vec!["no-such-harness-here".into()];
            assert!(!start_pty(&env, &mut s).unwrap());
            crate::backend::pty_settings(&mut s, false);
        }
        s.home_settings
            .iter()
            .any(|h| h.no_pty && h.path == ["features", "unified_exec_tty"] && h.value == false)
    };
    assert!(with(false));
    assert!(!with(true));
}

#[test]
fn settings_without_a_condition_stay_with_ptys_and_without() {
    let profile = crate::profile::builtin("claude").unwrap();
    for ptys in [true, false] {
        let (_t, _env, mut s) = temp_spec("");
        crate::privacy::push_state_settings(&profile, false, "profile claude", &mut s).unwrap();
        let n = s.home_settings.len();
        crate::backend::pty_settings(&mut s, ptys);
        assert_eq!(s.home_settings.len(), n);
        assert!(n > 0);
    }
}

#[test]
fn the_keychain_of_the_login_is_allowed_and_keeps_its_host_path() {
    let (_t, env, mut s) = temp_spec("");
    let store = super::super::keychain::store(&env, "fake", &crate::state::AuthMode::Shared);
    let file = store.file.display().to_string();
    s.set_env(super::super::keychain::VAR, file.clone(), "test");
    let tmp = private_tmp(&s);
    let (p, _) = policy(&env, &s, &tmp).unwrap();
    assert!(p
        .paths
        .iter()
        .any(|r| r.path == real(&store.dir) && r.access == Access::ReadWrite));
    assert!(p
        .services
        .iter()
        .any(|(n, _)| n == "com.apple.SecurityServer"));
    // The store is in the host home, but the variable is not moved into
    // the sandbox home.
    assert_eq!(run_env(&env, &s, &tmp)[super::super::keychain::VAR], file);
    // Without a keychain, no securityd.
    let (_t, env, s) = temp_spec("");
    let (p, _) = policy(&env, &s, &private_tmp(&s)).unwrap();
    assert!(p.services.is_empty());
}

#[test]
fn a_harness_without_the_pty_library_makes_its_own_ptys() {
    let (_t, env, mut s) = temp_spec("");
    s.command = vec!["no-such-harness-here".into()];
    let tmp = private_tmp(&s);
    let (p, _) = policy(&env, &s, &tmp).unwrap();
    assert!(p.own_ptys);
    assert!(crate::seatbelt::render(&p)
        .unwrap()
        .contains("(allow pseudo-tty)"));
    // With paddock's pty broker, the current rules stay.
    s.set_env(
        crate::ptybroker::SOCKET_VAR,
        "/private/tmp/p/pty.sock",
        "test",
    );
    let (p, _) = policy(&env, &s, &tmp).unwrap();
    assert!(!p.own_ptys);
    let sb = crate::seatbelt::render(&p).unwrap();
    assert!(!sb.contains("pseudo-tty"));
    assert!(!sb.contains("com.apple.sandbox.pty"));
}
