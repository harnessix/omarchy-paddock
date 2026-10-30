use super::*;
use std::collections::BTreeMap;
use std::os::unix::fs::symlink;

fn setup() -> (tempfile::TempDir, HostEnv, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let real = tmp.path().join("home");
    std::fs::create_dir_all(&real).unwrap();
    // PATH so that the work-tree checks can find bwrap and git.
    let mut vars = BTreeMap::new();
    if let Ok(p) = std::env::var("PATH") {
        vars.insert("PATH".to_string(), p);
    }
    let env = HostEnv::from_parts(real, "tester".into(), 1000, 1000, vars);
    let sandbox = env.paddock_data().join("homes/claude/shared/p-1");
    std::fs::create_dir_all(&sandbox).unwrap();
    (tmp, env, sandbox)
}

fn write(p: &Path, text: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

#[test]
fn glob_matches() {
    assert!(glob("*.desktop", "x.desktop"));
    assert!(!glob("*.desktop", "x.desktop.txt"));
    assert!(glob("a?c", "abc"));
    assert!(glob(".~lock.*", ".~lock.doc#"));
    assert!(glob("*", ""));
    assert!(!glob("a*b", "acd"));
}

#[test]
fn config_defaults_and_overrides() {
    let d = RecoveryCfg::parse("").unwrap();
    assert_eq!(d, RecoveryCfg::default());
    let c = RecoveryCfg::parse(
        "[env]\nhide = [\"X\"]\n[recovery]\nfolders = [\"Desktop\", \"out/files\"]\nignore = [\"*.tmp\"]\n",
    )
    .unwrap();
    assert_eq!(c.folders, vec!["Desktop", "out/files"]);
    assert!(c.ignored("Desktop/a.tmp"));
    assert!(!c.ignored("Desktop/a.desktop"));
    for bad in ["/etc", "../x", ".ssh", "Desktop/.config", ""] {
        let t = format!("[recovery]\nfolders = [\"{bad}\"]\n");
        assert!(RecoveryCfg::parse(&t).is_err(), "{bad}");
    }
    assert!(RecoveryCfg::parse("[recovery]\nnope = 1\n").is_err());
}

#[test]
fn scan_lists_regular_files_only() {
    let (tmp, env, sb) = setup();
    let cfg = RecoveryCfg::default();
    write(&sb.join("Desktop/report.txt"), "r");
    write(&sb.join("Desktop/sub/deep.txt"), "d");
    write(&sb.join("Desktop/run.desktop"), "x");
    write(&sb.join(".bashrc"), "no");
    write(&sb.join("notes/other.txt"), "no");
    let outside = tmp.path().join("secret");
    write(&outside.join("key"), "k");
    symlink(&outside, sb.join("Desktop/link")).unwrap();
    symlink(outside.join("key"), sb.join("Desktop/key")).unwrap();
    // A recovery folder that is itself a symlink is not followed.
    symlink(&outside, sb.join("Downloads")).unwrap();
    let items = scan(&env, &cfg, &sb, 0);
    let mut paths: Vec<_> = items.iter().map(|i| i.path.as_str()).collect();
    paths.sort();
    assert_eq!(paths, vec!["Desktop/report.txt", "Desktop/sub/deep.txt"]);
    let r = items
        .iter()
        .find(|i| i.path == "Desktop/report.txt")
        .unwrap();
    assert_eq!(r.target, env.home.join("Desktop/report.txt"));
    assert!(r.new);
}

#[test]
fn target_follows_host_user_dirs() {
    let (_tmp, env, _sb) = setup();
    write(
        &env.config_dir.join("user-dirs.dirs"),
        "XDG_DESKTOP_DIR=\"$HOME/Schreibtisch\"\n",
    );
    let cfg = RecoveryCfg::default();
    assert_eq!(
        default_target(&env, &cfg, "Desktop/a/b.txt").unwrap(),
        env.home.join("Schreibtisch/a/b.txt")
    );
    assert_eq!(
        default_target(&env, &cfg, "Music/x").unwrap(),
        env.home.join("Music/x")
    );
    assert!(default_target(&env, &cfg, "src/x").is_none());
}

#[test]
fn recover_moves_and_never_overwrites() {
    let (_tmp, env, sb) = setup();
    let cfg = RecoveryCfg::default();
    write(&sb.join("Desktop/a.txt"), "new");
    write(&env.home.join("Desktop/a.txt"), "mine");
    let r = recover(&env, &cfg, &sb, "Desktop/a.txt", None, false).unwrap();
    assert!(r.renamed);
    assert!(r.to.ends_with("Desktop/a (1).txt"));
    assert_eq!(std::fs::read_to_string(&r.to).unwrap(), "new");
    assert_eq!(
        std::fs::read_to_string(env.home.join("Desktop/a.txt")).unwrap(),
        "mine"
    );
    assert!(!sb.join("Desktop/a.txt").exists());

    write(&sb.join("Documents/d/b"), "b");
    let dir = env.home.join("elsewhere");
    let r = recover(&env, &cfg, &sb, "Documents/d/b", Some(&dir), true).unwrap();
    assert_eq!(r.to, std::fs::canonicalize(&dir).unwrap().join("b"));
    assert!(sb.join("Documents/d/b").exists());
}

#[test]
fn recover_refuses_protected_targets_and_escapes() {
    let (tmp, env, sb) = setup();
    let cfg = RecoveryCfg::default();
    write(&sb.join("Desktop/id_ed25519"), "k");
    for to in [
        env.home.join(".ssh"),
        env.home.join(".config/autostart"),
        env.paddock_data(),
        PathBuf::from("/etc"),
    ] {
        assert!(
            recover(&env, &cfg, &sb, "Desktop/id_ed25519", Some(&to), true).is_err(),
            "{}",
            to.display()
        );
    }
    // A host symlink that points into a protected place is resolved first.
    std::fs::create_dir_all(env.home.join(".ssh")).unwrap();
    symlink(env.home.join(".ssh"), env.home.join("innocent")).unwrap();
    assert!(recover(
        &env,
        &cfg,
        &sb,
        "Desktop/id_ed25519",
        Some(&env.home.join("innocent")),
        true
    )
    .is_err());
    assert!(!env.home.join(".ssh/id_ed25519").exists());

    // Sources: outside the recovery folders, '..', symlinks, special files.
    write(&sb.join(".bashrc"), "x");
    assert!(recover(&env, &cfg, &sb, ".bashrc", None, true).is_err());
    assert!(recover(&env, &cfg, &sb, "Desktop/../.bashrc", None, true).is_err());
    let outside = tmp.path().join("outside.txt");
    write(&outside, "o");
    symlink(&outside, sb.join("Desktop/ln.txt")).unwrap();
    assert!(recover(&env, &cfg, &sb, "Desktop/ln.txt", None, true).is_err());
    symlink(tmp.path(), sb.join("Desktop/dir")).unwrap();
    assert!(recover(&env, &cfg, &sb, "Desktop/dir/outside.txt", None, true).is_err());
    let fifo = sb.join("Desktop/fifo");
    let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    assert!(recover(&env, &cfg, &sb, "Desktop/fifo", None, true).is_err());
}

#[test]
fn discard_deletes_only_recovery_files() {
    let (_tmp, env, sb) = setup();
    let cfg = RecoveryCfg::default();
    write(&sb.join("Downloads/x.zip"), "z");
    write(&sb.join(".profile"), "p");
    assert!(discard(&cfg, &sb, ".profile").is_err());
    assert_eq!(
        discard(&cfg, &sb, "Downloads/x.zip").unwrap(),
        "Downloads/x.zip"
    );
    assert!(scan(&env, &cfg, &sb, 0).is_empty());
}

#[test]
fn candidate_names() {
    assert_eq!(
        candidate(Path::new("/d/a.txt"), 0),
        PathBuf::from("/d/a.txt")
    );
    assert_eq!(
        candidate(Path::new("/d/a.txt"), 2),
        PathBuf::from("/d/a (2).txt")
    );
    assert_eq!(
        candidate(Path::new("/d/Makefile"), 1),
        PathBuf::from("/d/Makefile (1)")
    );
}

#[test]
fn sessions_persist_and_report_project_changes() {
    let (tmp, env, sb) = setup();
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    let g = |args: &[&str]| {
        assert!(std::process::Command::new("git")
            .arg("-C")
            .arg(&proj)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
            .args(args)
            .output()
            .unwrap()
            .status
            .success());
    };
    g(&["init", "-q"]);
    write(&proj.join("old.txt"), "o");
    g(&["add", "."]);
    g(&["commit", "-qm", "first"]);
    write(&proj.join("dirty.txt"), "d");
    Session::begin(&env, "claude-p-abc123", "claude", &proj, &sb).unwrap();
    write(&proj.join("made.txt"), "m");
    g(&["add", "made.txt"]);
    g(&["commit", "-qm", "agent work"]);
    write(&proj.join("later.txt"), "l");
    Session::end(&env, "claude-p-abc123");
    let s = Session::load(&env, "claude-p-abc123").unwrap();
    assert!(s.ended.is_some());
    assert!(!s.running(&env));
    let c = project_changes(&env, &s);
    assert!(c.git);
    assert_eq!(c.commits.len(), 1);
    assert!(c.commits[0].ends_with("agent work"));
    // The work-tree state comes from a sandboxed git run. A host without
    // bubblewrap reports it as unknown instead of guessing.
    if c.changed_unknown {
        assert!(env.which("bwrap").is_none());
    } else {
        assert_eq!(c.changed, vec!["?? later.txt".to_string()]);
    }
    assert!(Session::load(&env, "../x").is_none());
    assert_eq!(Session::latest_per_home(&env).len(), 1);
}

#[test]
fn symlinked_folders_never_reach_host_files() {
    let (tmp, env, sb) = setup();
    let cfg = RecoveryCfg::default();
    // The sandbox turns a recovery folder into a link to a host folder.
    let secret = tmp.path().join("host-ssh");
    write(&secret.join("id_key"), "secret");
    symlink(&secret, sb.join("Downloads")).unwrap();
    assert!(discard(&cfg, &sb, "Downloads/id_key").is_err());
    assert!(recover(&env, &cfg, &sb, "Downloads/id_key", None, false).is_err());
    assert_eq!(
        std::fs::read_to_string(secret.join("id_key")).unwrap(),
        "secret"
    );
    // The same one level deeper.
    std::fs::create_dir_all(sb.join("Documents")).unwrap();
    symlink(&secret, sb.join("Documents/sub")).unwrap();
    assert!(discard(&cfg, &sb, "Documents/sub/id_key").is_err());
    assert!(secret.join("id_key").exists());
}

#[test]
fn recovered_files_are_not_executable() {
    let (_tmp, env, sb) = setup();
    let cfg = RecoveryCfg::default();
    write(&sb.join("Desktop/run.sh"), "#!/bin/sh\n");
    std::fs::set_permissions(
        sb.join("Desktop/run.sh"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let r = recover(&env, &cfg, &sb, "Desktop/run.sh", None, false).unwrap();
    assert_eq!(
        std::fs::metadata(&r.to).unwrap().permissions().mode() & 0o777,
        0o644
    );
}
