use super::*;
use std::collections::BTreeMap;

fn env(home: &Path) -> HostEnv {
    HostEnv::from_parts(home.to_path_buf(), "u".into(), 501, 20, BTreeMap::new())
}

#[test]
fn each_login_has_its_own_keychain_outside_the_home() {
    let e = env(Path::new("/h"));
    let iso = store(&e, "claude", &AuthMode::Isolated("work".into()));
    let root = crate::state::auth_root(&e, "claude", &AuthMode::Isolated("work".into()));
    assert_eq!(iso.file, root.join("keychain").join(FILE));
    assert_eq!(iso.dir, root.join("keychain"));
    // The password is not in the folder that the sandbox can write.
    assert!(!iso.password.starts_with(&iso.dir));
    let shared = store(&e, "claude", &AuthMode::Shared);
    let account = store(&e, "claude", &AuthMode::account(Path::new("/h/.claude-b")));
    let other = store(&e, "codex", &AuthMode::Shared);
    let all = [&iso, &shared, &account, &other];
    for (i, a) in all.iter().enumerate() {
        assert!(a.file.starts_with(e.paddock_data()), "{a:?}");
        for b in &all[i + 1..] {
            assert_ne!(a.dir, b.dir);
        }
    }
}

#[test]
fn the_search_list_names_only_the_keychain_of_the_login() {
    let s = search_list(Path::new("/s/a&b/paddock.keychain-db")).unwrap();
    for key in ["DLDBSearchList", "DefaultKeychain", "LoginKeychain"] {
        assert!(s.contains(&format!("<key>{key}</key>")), "{s}");
    }
    assert_eq!(
        s.matches("<string>/s/a&amp;b/paddock.keychain-db</string>")
            .count(),
        3
    );
    assert_eq!(s.matches("<key>DbName</key>").count(), 3);
    assert_eq!(s.matches(CSPDL_GUID).count(), 3);
    assert!(s.contains("<integer>6</integer>"));
    assert!(!s.contains("Library/Keychains"));
    assert!(search_list(Path::new("rel/kc")).is_err());
    assert!(search_list(Path::new("/a\nb")).is_err());
}

#[test]
fn a_new_keychain_never_locks() {
    let f = Path::new("/s/paddock.keychain-db");
    let new = commands(f, "pw", true);
    assert_eq!(
        new[0],
        ["create-keychain", "-p", "pw", "/s/paddock.keychain-db"]
    );
    // No -l (lock on sleep), no -u or -t (lock after a time).
    assert_eq!(new[1], ["set-keychain-settings", "/s/paddock.keychain-db"]);
    assert_eq!(
        new[2],
        ["unlock-keychain", "-p", "pw", "/s/paddock.keychain-db"]
    );
    let old = commands(f, "pw", false);
    assert_eq!(old, vec![new[2].clone()]);
}

#[test]
fn the_profile_allows_the_keychain_and_securityd_and_denies_the_others() {
    let t = tempfile::tempdir().unwrap();
    let file = t.path().join("keychain").join(FILE);
    let mut p = Policy::terminal(crate::seatbelt::Net::None, vec![]);
    allow(&mut p, &file, Some(Path::new("/var/folders/x/y/C")));
    let sb = crate::seatbelt::render(&p).unwrap();
    let dir = crate::backend::seatbelt::real(&t.path().join("keychain"));
    assert!(sb.contains(&format!(
        "(allow file-read* file-write* (subpath \"{}\"))",
        dir.display()
    )));
    assert!(sb.contains("(allow mach-lookup (global-name \"com.apple.SecurityServer\"))"));
    // The data protection keychain and the keychain sandbox extensions.
    for name in [
        "com.apple.securityd.xpc",
        "com.apple.securityd",
        "com.apple.security.XPCKeychainSandboxCheck",
    ] {
        assert!(!sb.contains(&format!("\"{name}\"")), "{name}");
    }
    assert!(sb.contains("(deny file-read* file-write* (subpath \"/Library/Keychains\"))"));
    assert!(sb.contains("(allow file-read* (subpath \"/private/var/db/mds\"))"));
    assert!(sb.contains("(deny file-write* (subpath \"/private/var/db/mds\"))"));
    // The system keychains rule comes after the read rule for /Library.
    let lib = sb
        .find("(allow file-read* (subpath \"/Library\"))")
        .unwrap();
    let deny = sb
        .find("(deny file-read* file-write* (subpath \"/Library/Keychains\"))")
        .unwrap();
    assert!(lib < deny);
}

#[test]
fn a_bad_service_name_is_refused() {
    let mut p = Policy::terminal(crate::seatbelt::Net::None, vec![]);
    p.services
        .push(("x\")) (allow default".into(), "bad".into()));
    assert!(crate::seatbelt::render(&p).is_err());
}

#[test]
fn a_planted_link_is_refused() {
    let t = tempfile::tempdir().unwrap();
    let link = t.path().join("l");
    std::os::unix::fs::symlink("/etc/hosts", &link).unwrap();
    assert!(no_link(&link, false).is_err());
    assert!(no_link(t.path(), false).is_err());
    assert!(no_link(t.path(), true).unwrap());
    assert!(!no_link(&t.path().join("missing"), false).unwrap());
}

#[test]
fn the_password_is_made_once_and_kept() {
    let t = tempfile::tempdir().unwrap();
    let e = env(t.path());
    let s = store(&e, "claude", &AuthMode::Isolated("default".into()));
    let a = password(&s).unwrap();
    assert_eq!(a.len(), 64);
    assert_eq!(password(&s).unwrap(), a);
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&s.password).unwrap().permissions().mode();
    assert_eq!(mode & 0o077, 0);
}
