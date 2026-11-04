use super::*;

const ENTITLED: &str = r#"Executable=/Applications/X.app/Contents/MacOS/X
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>com.apple.security.app-sandbox</key>
	<true/>
	<key>com.apple.security.network.client</key>
	<true/>
</dict>
</plist>"#;

const ELECTRON: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>com.apple.security.cs.allow-jit</key><true/>
<key>com.apple.security.app-sandbox</key> <false />
</dict></plist>"#;

#[test]
fn detects_the_app_sandbox_entitlement() {
    assert!(app_sandboxed(ENTITLED));
    assert!(!app_sandboxed(ELECTRON));
    assert!(!app_sandboxed(""));
    assert!(!app_sandboxed("<plist><dict></dict></plist>"));
}

fn fake_bundle(t: &Path, name: &str, electron: bool) -> PathBuf {
    let b = t.join(format!("{name}.app"));
    std::fs::create_dir_all(b.join("Contents/MacOS")).unwrap();
    std::fs::write(b.join("Contents/MacOS").join(name), "").unwrap();
    std::fs::write(b.join("Contents/MacOS/helper-not-main"), "").unwrap();
    std::fs::write(
            b.join("Contents/Info.plist"),
            format!(
                "<?xml version=\"1.0\"?>\n<plist version=\"1.0\"><dict>\n<key>CFBundleName</key>\n<string>X</string>\n<key>CFBundleExecutable</key>\n\t<string>{name}</string>\n</dict></plist>"
            ),
        )
        .unwrap();
    if electron {
        std::fs::create_dir_all(b.join("Contents/Frameworks/Electron Framework.framework"))
            .unwrap();
    }
    b
}

#[test]
fn finds_the_binary_and_electron() {
    let t = tempfile::tempdir().unwrap();
    let code = fake_bundle(t.path(), "Code", true);
    let other = fake_bundle(t.path(), "Native", false);
    assert_eq!(bundle_exe(&code).unwrap(), code.join("Contents/MacOS/Code"));
    assert!(is_electron(&code));
    assert!(!is_electron(&other));
    // Without Info.plist and with two files, paddock cannot tell.
    std::fs::remove_file(other.join("Contents/Info.plist")).unwrap();
    assert!(bundle_exe(&other).is_err());
}

#[test]
fn info_plist_cannot_point_outside_the_bundle() {
    let t = tempfile::tempdir().unwrap();
    let b = fake_bundle(t.path(), "App", false);
    std::fs::write(
        b.join("Contents/Info.plist"),
        "<plist><dict><key>CFBundleExecutable</key><string>../../../evil</string></dict></plist>",
    )
    .unwrap();
    assert!(bundle_exe(&b).is_err());
}

#[test]
fn finds_the_first_bundle_that_exists() {
    let t = tempfile::tempdir().unwrap();
    let env = HostEnv::from_parts(
        t.path().join("home"),
        "demo".into(),
        1000,
        1000,
        Default::default(),
    );
    let b = fake_bundle(&env.home.join("Applications"), "Claude", true);
    let mac = MacApp {
        bundles: vec![
            "/nonexistent/Claude.app".into(),
            "~/Applications/Claude.app".into(),
        ],
        ..Default::default()
    };
    assert_eq!(find_bundle(&env, "claude-desktop", &mac).unwrap(), b);
    let none = MacApp {
        bundles: vec!["/nonexistent/X.app".into()],
        ..Default::default()
    };
    let e = find_bundle(&env, "x", &none).unwrap_err().to_string();
    assert!(e.contains("not installed"), "{e}");
}

#[test]
fn a_live_singleton_lock_names_its_process() {
    let t = tempfile::tempdir().unwrap();
    assert_eq!(singleton_owner(t.path()), None);
    let me = std::process::id();
    std::os::unix::fs::symlink(format!("my-mac.local-{me}"), t.path().join("SingletonLock"))
        .unwrap();
    assert_eq!(singleton_owner(t.path()), Some(me));
    std::fs::remove_file(t.path().join("SingletonLock")).unwrap();
    // A lock of a process that is gone does not count.
    std::os::unix::fs::symlink("my-mac.local-999999999", t.path().join("SingletonLock")).unwrap();
    assert_eq!(singleton_owner(t.path()), None);
}

fn spec(t: &Path) -> (HostEnv, SandboxSpec) {
    let env = HostEnv::from_parts(
        t.join("home"),
        "demo".into(),
        1000,
        1000,
        Default::default(),
    );
    let project = env.home.join("src/my project");
    std::fs::create_dir_all(&project).unwrap();
    let home_src = env.home.join(".local/share/paddock/homes/vscode-x");
    std::fs::create_dir_all(&home_src).unwrap();
    let mut s = SandboxSpec::new(
        "vscode-1".into(),
        "vscode",
        &project,
        home_src.clone(),
        &env.home,
        1000,
        1000,
        "demo",
    );
    s.add_mount(crate::spec::Mount::same(&project, true, "project"));
    s.add_mount(crate::spec::Mount::bind(&home_src, &env.home, true, "home"));
    s.add_mount(crate::spec::Mount::mask(
        project.join(".env"),
        false,
        "secrets",
    ));
    s.add_mount(crate::spec::Mount::bind(
        "/x",
        "/run/paddock/borrow",
        true,
        "borrow folder",
    ));
    (env, s)
}

#[test]
fn the_policy_covers_project_home_bundle_and_masks() {
    let t = tempfile::tempdir().unwrap();
    let (env, s) = spec(t.path());
    let tmp = s.home_src.join(".paddock-tmp/vscode-1");
    let bundle = t.path().join("Visual Studio Code.app");
    let (p, notes) = policy(&s, &bundle, &tmp, Net::Localhost).unwrap();
    let has = |path: &Path, a: Access| {
        p.paths
            .iter()
            .any(|r| r.path == real(path) && r.access == a)
    };
    assert!(has(&s.project, Access::ReadWrite));
    assert!(has(&s.home_src, Access::ReadWrite));
    assert!(has(&bundle, Access::ReadOnly));
    assert!(has(&s.project.join(".env"), Access::Deny));
    assert!(!p.paths.iter().any(|r| r.path == real(&env.home)));
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(notes[0].contains("borrow"));
    let sbpl = crate::seatbelt::render(&p).unwrap();
    assert!(sbpl.contains("my project"));
}

#[test]
fn overlays_are_refused() {
    let t = tempfile::tempdir().unwrap();
    let (_, mut s) = spec(t.path());
    s.add_mount(crate::spec::Mount::overlay(
        "/a",
        "/b",
        "/c",
        "/d",
        "shared cache",
    ));
    let e = policy(&s, Path::new("/A.app"), Path::new("/tmp"), Net::None)
        .unwrap_err()
        .to_string();
    assert!(e.contains("overlay"), "{e}");
}

#[test]
fn the_environment_moves_home_and_points_at_the_proxy() {
    let t = tempfile::tempdir().unwrap();
    let (env, mut s) = spec(t.path());
    s.set_env("WAYLAND_DISPLAY", "wayland-0", "gui");
    s.set_env(
        "XDG_CONFIG_HOME",
        env.home.join(".config").display().to_string(),
        "x",
    );
    s.set_env("HTTPS_PROXY", "http://127.0.0.1:3131", "network: api proxy");
    let tmp = s.home_src.join(".paddock-tmp/vscode-1");
    let v = app_env(&env, &s, &tmp, Some(4567));
    let home = s.home_src.display().to_string();
    assert_eq!(v["HOME"], home);
    assert_eq!(v["CFFIXED_USER_HOME"], home);
    assert_eq!(v["TMPDIR"], tmp.display().to_string());
    assert_eq!(v["HTTPS_PROXY"], "http://127.0.0.1:4567");
    assert_eq!(v["http_proxy"], "http://127.0.0.1:4567");
    assert_eq!(
        v["XDG_CONFIG_HOME"],
        s.home_src.join(".config").display().to_string()
    );
    assert!(!v.contains_key("WAYLAND_DISPLAY"));
    let none = app_env(&env, &s, &tmp, None);
    assert!(!none.contains_key("HTTPS_PROXY"));
}

#[test]
fn electron_gets_its_flags_once() {
    let mac = MacApp {
        args: vec!["--no-sandbox".into()],
        ..Default::default()
    };
    let c = command(
        Path::new("/A.app/Contents/MacOS/A"),
        &mac,
        Some(electron_args(
            Path::new("/h/Library/Application Support/A"),
            Some(9),
        )),
        &["/p".into()],
    );
    assert_eq!(
        c,
        [
            "/A.app/Contents/MacOS/A",
            "--no-sandbox",
            "--use-mock-keychain",
            "--user-data-dir=/h/Library/Application Support/A",
            "--proxy-server=http://127.0.0.1:9",
            "/p"
        ]
    );
}

#[test]
fn linux_only_settings_are_refused() {
    assert!(refuse_linux_only("x", Some("bwrap"), None, &[]).is_err());
    assert!(refuse_linux_only("x", Some(NAME), None, &[]).is_ok());
    assert!(refuse_linux_only("x", None, None, &["echo".into()]).is_err());
    assert!(net_for(Network::Host).is_err());
    assert!(net_for(Network::Allowlist).is_err());
    assert_eq!(net_for(Network::Egress).unwrap(), Net::Localhost);
    assert_eq!(net_for(Network::None).unwrap(), Net::None);
}

#[test]
fn profiles_name_macos_bundles() {
    let env = HostEnv::from_parts(
        "/nonexistent-home".into(),
        "demo".into(),
        1000,
        1000,
        Default::default(),
    );
    let f = crate::profile::HarnessFactory::new(&env).unwrap();
    let mac = |n: &str| f.profile(n).unwrap().app.clone().unwrap().macos.unwrap();
    assert!(mac("vscode").bundles[0] == "/Applications/Visual Studio Code.app");
    assert!(mac("claude-desktop").bundles[0] == "/Applications/Claude.app");
    assert!(mac("chatgpt-desktop").app_sandbox);
    assert!(!mac("vscode").app_sandbox);
}

#[test]
fn relays_tcp_to_the_proxy_socket() {
    use std::io::{Read, Write};
    let t = tempfile::tempdir().unwrap();
    let sock = t.path().join("p.sock");
    let l = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    std::thread::spawn(move || {
        let (mut c, _) = l.accept().unwrap();
        let mut b = [0u8; 4];
        c.read_exact(&mut b).unwrap();
        c.write_all(&b).unwrap();
    });
    let port = tcp_relay(&sock).unwrap();
    let mut c = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    c.write_all(b"ping").unwrap();
    let mut b = [0u8; 4];
    c.read_exact(&mut b).unwrap();
    assert_eq!(&b, b"ping");
}
