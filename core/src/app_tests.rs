use super::*;

#[test]
fn notification_text_that_starts_with_a_dash_is_never_an_argument() {
    use std::os::unix::fs::PermissionsExt;
    let t = tempfile::tempdir().unwrap();
    // A fake notify-send that records any arguments it gets.
    let bin = t.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let record = t.path().join("argv.txt");
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
    let mut vars = std::collections::BTreeMap::new();
    vars.insert("PATH".to_string(), bin.display().to_string());
    let env = HostEnv::from_parts(t.path().join("home"), "demo".into(), 1000, 1000, vars);

    // A branch and a worktree path that a sandbox chose, and error text.
    let w = build::Worktree {
        repo: PathBuf::from("/home/demo/src/app"),
        path: PathBuf::from("/home/demo/.local/share/paddock/worktrees/--help"),
        branch: "--version".into(),
        base: "main".into(),
    };
    let n = worktree_note("-x", &w, 2, Some(1));
    assert_eq!(n.summary, "-x worked on branch --version");
    assert!(n.body.contains("worktrees/--help"), "{}", n.body);
    assert!(n.body.ends_with("log --version"), "{}", n.body);
    let keys: Vec<&str> = n.actions.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(keys, ["merge", "remove"]);
    // A button for a worktree that paddock does not know changes nothing.
    let a = worktree_action(&env, &w.path, "remove");
    assert!(a.summary.contains("could not remove"), "{}", a.summary);
    let e = start_error_note("app", &anyhow::anyhow!("--icon=/home/demo/x"));
    assert_eq!(e.body, "--icon=/home/demo/x");

    // The text goes to the session bus as D-Bus strings, which are never
    // parsed as options. Without a bus there is no notification, and no
    // fallback to a process.
    desktop_notice(&env, &n);
    desktop_notice(&env, &e);
    assert!(!record.exists(), "notify-send ran");
}

#[test]
fn editors_get_their_folders_and_chat_apps_do_not() {
    let editor: Profile = toml::from_str("name = \"e\"\n[app]\nopen_roots = true\n").unwrap();
    let chat: Profile = toml::from_str("name = \"c\"\n[app]\n").unwrap();
    let roots = [PathBuf::from("/w/a"), PathBuf::from("/w/b")];
    let link = ["x://y".to_string()];
    assert_eq!(app_args(&editor, &roots, &link), ["/w/a", "/w/b", "x://y"]);
    assert_eq!(app_args(&chat, &roots, &link), ["x://y"]);
}

#[test]
fn links_must_use_the_app_scheme() {
    let s = vec!["claude".to_string()];
    assert!(valid_link("claude://login/callback?code=abc", &s).is_ok());
    assert!(valid_link("codex://x", &s).is_err());
    assert!(valid_link("https://example.com", &s).is_err());
    assert!(valid_link("claude://a\nb", &s).is_err());
    assert!(valid_link(&format!("claude://{}", "a".repeat(9000)), &s).is_err());
}

#[test]
fn flatpak_never_gets_the_full_bus_or_host_commands() {
    let fs = ["--filesystem=/w/two:rw".to_string()];
    let a = flatpak_args("org.example.App", None, &fs, false);
    assert_eq!(&a[..2], ["run", "--sandbox"]);
    assert!(a.contains(&"--session-bus".to_string()));
    assert!(!a
        .iter()
        .any(|x| x == "--socket=session-bus" || x == "--socket=system-bus"));
    assert!(a.contains(&"--no-talk-name=org.freedesktop.Flatpak".to_string()));
    assert!(!a
        .iter()
        .any(|x| x.starts_with("--filesystem=home") || x.starts_with("--filesystem=host")));
    assert!(a.contains(&"--filesystem=/w/two:rw".to_string()));
    assert_eq!(a.last().unwrap(), "org.example.App");
}

fn git(dir: &std::path::Path, args: &[&str]) {
    let ok = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@localhost"])
        .args(args)
        .stdout(Stdio::null())
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?}");
}

fn repo(dir: &std::path::Path) {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["commit", "-q", "--allow-empty", "-m", "one"]);
}

fn test_env(t: &std::path::Path) -> HostEnv {
    HostEnv::from_parts(
        t.join("home"),
        "demo".into(),
        1000,
        1000,
        Default::default(),
    )
}

fn test_spec(env: &HostEnv, project: &std::path::Path) -> SandboxSpec {
    SandboxSpec::new(
        "t".into(),
        "f",
        project,
        env.home.join("sandbox"),
        &env.home,
        1000,
        1000,
        "demo",
    )
}

fn workspace(env: &HostEnv, m: &Manifest, roots: &[PathBuf]) -> Result<FlatpakWorkspace> {
    let mut spec = test_spec(env, &roots[0]);
    flatpak_workspace(env, &mut spec, m, roots)
}

#[test]
fn flatpak_folders_keep_git_files_read_only() {
    let t = tempfile::tempdir().unwrap();
    let env = test_env(t.path());
    std::fs::create_dir_all(&env.home).unwrap();
    let (a, b) = (t.path().join("a"), t.path().join("b"));
    repo(&a);
    repo(&b);
    let ws = workspace(&env, &Manifest::default(), &[a.clone(), b.clone()]).unwrap();
    assert!(ws.exposed.is_empty());
    // The second folder is protected like the first.
    for root in [&a, &b] {
        let has = |p: PathBuf, mode: &str| {
            let want = format!("--filesystem={}:{mode}", p.display());
            assert!(ws.args.contains(&want), "{want} in {:?}", ws.args);
        };
        let dot = root.join(".git");
        has(root.to_path_buf(), "rw");
        // Its own mount point, so the app cannot rename it.
        has(dot.clone(), "rw");
        for f in [
            "hooks",
            "config",
            "config.worktree",
            "commondir",
            "objects/info/alternates",
        ] {
            has(dot.join(f), "ro");
        }
    }
}

// Flatpak app mode is Linux.
#[cfg(target_os = "linux")]
#[test]
fn flatpak_keeps_a_worktree_dot_git_file_read_only() {
    let t = tempfile::tempdir().unwrap();
    let env = test_env(t.path());
    std::fs::create_dir_all(&env.home).unwrap();
    let main = t.path().join("main");
    repo(&main);
    let wt = t.path().join("wt");
    git(&main, &["worktree", "add", "-q", wt.to_str().unwrap()]);
    let ws = workspace(&env, &Manifest::default(), std::slice::from_ref(&wt)).unwrap();
    let want = format!("--filesystem={}:ro", wt.join(".git").display());
    assert!(ws.args.contains(&want), "{:?}", ws.args);
    let want = format!("--filesystem={}:ro", main.join(".git/config").display());
    assert!(ws.args.contains(&want), "{:?}", ws.args);
}

// Flatpak app mode is Linux.
#[cfg(target_os = "linux")]
#[test]
fn a_flatpak_worktree_can_commit_and_keeps_git_files_read_only() {
    let t = tempfile::tempdir().unwrap();
    let env = test_env(t.path());
    std::fs::create_dir_all(&env.home).unwrap();
    let main = t.path().join("main");
    repo(&main);
    let other = t.path().join("other");
    std::fs::create_dir_all(&other).unwrap();
    let (w, roots) = flatpak_worktree(&env, "f", &[main.clone(), other.clone()]).unwrap();
    assert_eq!(roots, [w.path.clone(), other]);
    assert!(w.branch.starts_with("paddock/f-"), "{}", w.branch);
    let ws = workspace(&env, &Manifest::default(), &roots).unwrap();
    let has = |p: PathBuf, mode: &str| {
        let want = format!("--filesystem={}:{mode}", p.display());
        assert!(ws.args.contains(&want), "{want} in {:?}", ws.args);
    };
    let common = main.join(".git");
    let meta = std::fs::read_dir(common.join("worktrees"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    // The worktree, the object store and the worktree metadata are
    // writable: the common dir holds both.
    has(w.path.clone(), "rw");
    has(common.clone(), "rw");
    assert!(meta.starts_with(&common) && common.join("objects").is_dir());
    has(w.path.join(".git"), "ro");
    for dir in [&common, &meta] {
        has(dir.join("hooks"), "ro");
        for f in ["config", "config.worktree", "commondir"] {
            has(dir.join(f), "ro");
        }
    }
    has(meta.join("gitdir"), "ro");
    // Nothing hides or writes over the object store or the metadata.
    for p in [common.join("objects"), meta.clone()] {
        let p = p.display();
        for bad in [
            format!("--filesystem={p}:ro"),
            format!("--nofilesystem={p}"),
        ] {
            assert!(!ws.args.contains(&bad), "{bad} in {:?}", ws.args);
        }
    }
}

#[test]
fn a_flatpak_worktree_needs_a_git_repository() {
    let t = tempfile::tempdir().unwrap();
    let env = test_env(t.path());
    let plain = t.path().join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    let e = flatpak_worktree(&env, "f", std::slice::from_ref(&plain))
        .unwrap_err()
        .to_string();
    assert!(e.contains("--worktree needs a git repository"), "{e}");
}

#[test]
fn flatpak_refuses_secret_files_unless_the_user_opts_out() {
    let t = tempfile::tempdir().unwrap();
    let env = test_env(t.path());
    std::fs::create_dir_all(&env.home).unwrap();
    let (a, b) = (t.path().join("a"), t.path().join("b"));
    std::fs::create_dir_all(b.join("deploy")).unwrap();
    std::fs::create_dir_all(&a).unwrap();
    std::fs::write(b.join("deploy/.env"), "TOKEN=x").unwrap();
    std::fs::write(b.join(".env.example"), "TOKEN=").unwrap();
    let ws = workspace(&env, &Manifest::default(), &[a, b.clone()]).unwrap();
    assert_eq!(ws.exposed, [b.join("deploy/.env")]);
    let e = check_exposed("f", &ws.exposed, false)
        .unwrap_err()
        .to_string();
    assert!(
        e.contains(&b.join("deploy/.env").display().to_string()),
        "{e}"
    );
    assert!(e.contains("unprotected_workspaces = true"), "{e}");
    let w = check_exposed("f", &ws.exposed, true).unwrap().unwrap();
    assert!(w.contains("deploy/.env"), "{w}");
    assert_eq!(check_exposed("f", &[], false).unwrap(), None);
}

#[test]
fn flatpak_needs_approval_for_a_project_reveal() {
    let tmp = tempfile::tempdir().unwrap();
    let env = HostEnv::from_parts(
        tmp.path().join("home"),
        "demo".into(),
        1,
        1,
        Default::default(),
    );
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join(".env"), "TOKEN=x").unwrap();
    std::fs::write(ws.join("paddock.toml"), "[secrets]\nreveal = [\".env\"]\n").unwrap();
    // Tests have no terminal: an unapproved widening is refused.
    let e = approved_manifest(&env, &ws).unwrap_err().to_string();
    assert!(e.contains("not approved"), "{e}");
    // After approval it loads, with the reveal.
    let loaded = Manifest::load(&env, &ws).unwrap();
    let (file, m) = loaded.project.clone().unwrap();
    crate::manifest::approve(&env, &file, &m).unwrap();
    assert!(!approved_manifest(&env, &ws)
        .unwrap()
        .secrets
        .reveal
        .is_empty());
}

// Flatpak app mode is Linux.
#[cfg(target_os = "linux")]
#[test]
fn flatpak_hides_a_secret_folder() {
    let t = tempfile::tempdir().unwrap();
    let env = test_env(t.path());
    std::fs::create_dir_all(&env.home).unwrap();
    let a = t.path().join("a");
    std::fs::create_dir_all(a.join("private")).unwrap();
    let mut m = Manifest::default();
    m.secrets.hide.push("private".into());
    let ws = workspace(&env, &m, std::slice::from_ref(&a)).unwrap();
    let want = format!("--nofilesystem={}", a.join("private").display());
    assert!(ws.args.contains(&want), "{:?}", ws.args);
}

#[test]
fn flatpak_refuses_denied_folders() {
    let t = tempfile::tempdir().unwrap();
    let env = test_env(t.path());
    for d in [".ssh", ".config/gh", "src"] {
        std::fs::create_dir_all(env.home.join(d)).unwrap();
    }
    for root in [
        env.home.clone(),
        env.home.join(".ssh"),
        env.home.join(".config"),
        t.path().to_path_buf(),
    ] {
        let e = workspace(
            &env,
            &Manifest::default(),
            &[env.home.join("src"), root.clone()],
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("policy"), "{}: {e}", root.display());
    }
    assert!(workspace(&env, &Manifest::default(), &[env.home.join("src")]).is_ok());
    // flatpak would split a path at ':'.
    let odd = t.path().join("a:ro");
    std::fs::create_dir_all(&odd).unwrap();
    assert!(workspace(&env, &Manifest::default(), &[odd]).is_err());
}

#[test]
fn the_flatpak_opt_out_comes_from_the_user_configuration_only() {
    let t = tempfile::tempdir().unwrap();
    let env = test_env(t.path());
    let cfg = env.paddock_config();
    std::fs::create_dir_all(&cfg).unwrap();
    let project = t.path().join("p");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join(crate::manifest::PROJECT_FILE),
        "[apps.f]\nunprotected_workspaces = true\n",
    )
    .unwrap();
    // Even a manifest merged with the project file keeps it off.
    let merged = Manifest::load(&env, &project).unwrap().merged;
    assert!(!merged
        .apps
        .get("f")
        .is_some_and(|a| a.unprotected_workspaces));
    assert!(!unprotected_workspaces(&env, "f").unwrap());
    std::fs::write(
        cfg.join("config.toml"),
        "[apps.f]\nunprotected_workspaces = true\n",
    )
    .unwrap();
    assert!(unprotected_workspaces(&env, "f").unwrap());
    assert!(!unprotected_workspaces(&env, "other").unwrap());
}

#[test]
fn flatpak_gets_the_filtered_host_environment() {
    let t = tempfile::tempdir().unwrap();
    let sock = t.path().join("ctl.sock");
    let _l = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    let vars: std::collections::BTreeMap<String, String> = [
        ("HOME", "/home/demo"),
        ("PATH", "/usr/bin"),
        ("DBUS_SESSION_BUS_ADDRESS", "unix:path=/run/user/1000/bus"),
        ("XDG_RUNTIME_DIR", "/run/user/1000"),
        ("EDITOR", "nvim"),
        ("CLAUDE_CODE_MESSAGING_TOKEN", "x"),
        ("HYPRLAND_INSTANCE_SIGNATURE", "x"),
        ("SSH_AUTH_SOCK", "/run/user/1000/ssh"),
        ("AWS_SECRET_ACCESS_KEY", "x"),
        ("GITHUB_TOKEN", "x"),
        ("HIDDEN_ONE", "x"),
        ("ANY_CONTROL", sock.to_str().unwrap()),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    let env = HostEnv::from_parts("/home/demo".into(), "demo".into(), 1000, 1000, vars);
    let profile: Profile = toml::from_str("name = \"f\"\n[app]\nflatpak = \"org.x\"\n").unwrap();
    let mut m = Manifest::default();
    m.env.hide.push("HIDDEN_ONE".into());
    let e = flatpak_env(&env, &m, &profile, true);
    for k in [
        "HOME",
        "PATH",
        "DBUS_SESSION_BUS_ADDRESS",
        "XDG_RUNTIME_DIR",
        "EDITOR",
    ] {
        assert!(e.contains_key(k), "{k}");
    }
    for k in [
        "CLAUDE_CODE_MESSAGING_TOKEN",
        "HYPRLAND_INSTANCE_SIGNATURE",
        "SSH_AUTH_SOCK",
        "AWS_SECRET_ACCESS_KEY",
        "GITHUB_TOKEN",
        "HIDDEN_ONE",
        "ANY_CONTROL",
    ] {
        assert!(!e.contains_key(k), "{k}");
    }
    m.env.pass.push("GITHUB_TOKEN".into());
    assert!(flatpak_env(&env, &m, &profile, true).contains_key("GITHUB_TOKEN"));
    let clean = flatpak_env(&env, &m, &profile, false);
    assert!(!clean.contains_key("EDITOR"));
    assert!(clean.contains_key("GITHUB_TOKEN") && clean.contains_key("HOME"));
}

/// The extra folders of a bwrap app get the git and secrets detectors,
/// and the policy refuses them without.
#[test]
fn bwrap_app_extra_folders_are_protected_like_the_first() {
    let t = tempfile::tempdir().unwrap();
    let env = test_env(t.path());
    std::fs::create_dir_all(&env.home).unwrap();
    let (a, b) = (t.path().join("a"), t.path().join("b"));
    repo(&a);
    repo(&b);
    std::fs::write(b.join(".env"), "TOKEN=x").unwrap();
    let mut bare = test_spec(&env, &a);
    bare.add_mount(Mount::same(&b, true, "app workspace folder"));
    assert!(crate::policy::enforce(&env, &bare).is_err());
    let mut spec = test_spec(&env, &a);
    add_roots(
        &env,
        &mut spec,
        &Manifest::default(),
        std::slice::from_ref(&b),
    )
    .unwrap();
    crate::policy::enforce(&env, &spec).unwrap();
    let kind = |p: PathBuf| spec.mounts.iter().find(|m| m.dst == p).map(|m| m.kind);
    assert_eq!(kind(b.join(".env")), Some(MountKind::MaskFile));
    assert_eq!(kind(b.join(".git/config")), Some(MountKind::Ro));
    assert_eq!(kind(b.join(".git/hooks")), Some(MountKind::Ro));
}
