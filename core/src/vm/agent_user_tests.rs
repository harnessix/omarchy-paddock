use super::*;
use std::cell::Cell;

fn seen(uid: u32, mode: u32, fs: Option<&str>) -> Seen {
    Seen {
        uid,
        gid: if uid == 0 { 0 } else { 20 },
        mode,
        fs: fs.map(str::to_string),
    }
}

const P: &str = "/Users/u/src/p";

fn pick(s: &Seen, host: Option<u32>, writable: bool) -> (Result<(u32, u32)>, bool) {
    let probed = Cell::new(false);
    let r = choose(Path::new(P), s, host, || {
        probed.set(true);
        Ok(writable)
    });
    (r, probed.get())
}

#[test]
fn a_normal_owner_in_the_guest_is_the_run_user() {
    let (r, probed) = pick(&seen(501, 0o755, Some("virtiofs")), Some(501), false);
    assert_eq!(r.unwrap(), (501, 20));
    assert!(!probed);
}

#[test]
fn root_on_virtiofs_uses_the_guest_user_when_it_can_write() {
    let (r, probed) = pick(&seen(0, 0o755, Some("virtiofs")), Some(501), true);
    assert_eq!(r.unwrap(), GUEST_USER);
    assert!(probed);
    // An old host without the owner field: the same decision.
    let (r, _) = pick(&seen(0, 0o777, Some("virtiofs")), None, true);
    assert_eq!(r.unwrap(), GUEST_USER);
}

#[test]
fn root_on_virtiofs_is_refused_when_the_guest_user_cannot_write() {
    let (r, probed) = pick(&seen(0, 0o755, Some("virtiofs")), Some(501), false);
    let e = format!("{:#}", r.unwrap_err());
    assert!(probed);
    assert!(
        e.contains("mode 0755") && e.contains("uid 1000 cannot write"),
        "{e}"
    );
}

#[test]
fn a_project_of_root_on_the_host_is_refused_without_a_probe() {
    for s in [
        seen(0, 0o777, Some("virtiofs")),
        seen(501, 0o755, Some("virtiofs")),
    ] {
        let (r, probed) = pick(&s, Some(0), true);
        let e = format!("{:#}", r.unwrap_err());
        assert!(e.contains("belongs to root on the host"), "{e}");
        assert!(!probed);
    }
}

#[test]
fn a_root_folder_without_the_share_is_refused() {
    let (r, probed) = pick(&seen(0, 0o755, Some("tmpfs")), Some(501), true);
    let e = format!("{:#}", r.unwrap_err());
    assert!(e.contains("on tmpfs") && e.contains("did not mount"), "{e}");
    assert!(!probed);
    let (r, _) = pick(&seen(0, 0o755, None), Some(501), true);
    let e = format!("{:#}", r.unwrap_err());
    assert!(e.contains("nothing is mounted"), "{e}");
}

#[test]
fn the_mount_type_comes_from_the_last_line_of_the_folder() {
    let mounts = "\
rootfs / ext4 ro 0 0
tmpfs /private tmpfs rw,nosuid,nodev 0 0
paddock-project /private/var/f/my\\040project virtiofs rw 0 0
tmpfs /run tmpfs rw 0 0
";
    let at = |p: &str| fs_type(mounts, Path::new(p));
    assert_eq!(at("/private/var/f/my project").as_deref(), Some("virtiofs"));
    assert_eq!(at("/private").as_deref(), Some("tmpfs"));
    assert_eq!(at("/private/var/f"), None);
    let twice = "tmpfs /x tmpfs rw 0 0\nshare /x virtiofs rw 0 0\n";
    assert_eq!(fs_type(twice, Path::new("/x")).as_deref(), Some("virtiofs"));
}

#[test]
fn the_console_text_is_short() {
    assert_eq!(
        seen(0, 0o755, Some("virtiofs")).line(),
        "owner 0:0 mode 0755 fs virtiofs"
    );
    assert_eq!(
        seen(501, 0o700, None).line(),
        "owner 501:20 mode 0700 fs none"
    );
}

#[test]
fn the_probe_writes_as_the_given_user() {
    let t = tempfile::tempdir().unwrap();
    // SAFETY: getuid and getgid have no preconditions.
    let me = unsafe { (libc::getuid(), libc::getgid()) };
    assert!(can_write(t.path(), me).unwrap());
    assert_eq!(std::fs::read_dir(t.path()).unwrap().count(), 0);
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(t.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
    // Root writes anyway, so check the refusal only as a normal user.
    if me.0 != 0 {
        assert!(!can_write(t.path(), me).unwrap());
    }
    std::fs::set_permissions(t.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
}
