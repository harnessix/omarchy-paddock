use super::*;
use std::os::unix::fs::symlink;

#[test]
fn writes_and_reads_never_follow_symlinks() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("home");
    let outside = t.path().join("outside");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("secret"), "host secret").unwrap();
    let r = Root::open(&root).unwrap();

    // A symlinked file is neither read nor written through.
    symlink(outside.join("secret"), root.join("token")).unwrap();
    assert!(r.read(Path::new("token"), MAX_READ).is_err());
    r.write(Path::new("token"), b"new", 0o600).unwrap();
    assert_eq!(
        std::fs::read_to_string(outside.join("secret")).unwrap(),
        "host secret"
    );
    assert!(!std::fs::symlink_metadata(root.join("token"))
        .unwrap()
        .file_type()
        .is_symlink());

    // A symlinked folder on the way is refused.
    symlink(&outside, root.join(".config")).unwrap();
    assert!(r
        .write(Path::new(".config/x/token"), b"new", 0o600)
        .is_err());
    assert!(r.read(Path::new(".config/secret"), MAX_READ).is_err());
    assert!(r.remove(Path::new(".config/secret")).is_err());
    assert!(outside.join("secret").exists());
    assert!(!outside.join("x").exists());

    // Plain files work, and missing ones read as None.
    r.write(Path::new("a/b/c.json"), b"{}", 0o600).unwrap();
    assert_eq!(
        r.read(Path::new("a/b/c.json"), MAX_READ).unwrap().unwrap(),
        b"{}"
    );
    assert!(r.read(Path::new("a/missing"), MAX_READ).unwrap().is_none());
    assert!(r.read(Path::new("nothere/x"), MAX_READ).unwrap().is_none());
    assert!(r.write(Path::new("../escape"), b"x", 0o600).is_err());
}

#[test]
fn copy_in_and_new_names() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("p");
    let src = t.path().join("src");
    std::fs::create_dir_all(src.join("d")).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(src.join("d/f"), "1").unwrap();
    symlink("/etc/passwd", src.join("link")).unwrap();
    let r = Root::open(&root).unwrap();
    r.copy_in(&src, Path::new("x/y")).unwrap();
    assert_eq!(std::fs::read_to_string(root.join("x/y/d/f")).unwrap(), "1");
    assert!(!root.join("x/y/link").exists());
    let d = r.dir(Path::new("x"), false).unwrap();
    let (_, a) = d.new_file("shot.png", 0o600).unwrap();
    let (_, b) = d.new_file("shot.png", 0o600).unwrap();
    assert_eq!((a.as_str(), b.as_str()), ("shot.png", "shot-1.png"));
    let (_, n) = d.new_subdir("y").unwrap();
    assert_eq!(n, "y-1");
}

fn mkfifo(p: &Path) {
    let c = CString::new(p.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
}

#[test]
fn reads_never_open_fifos_or_devices() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path();
    mkfifo(&root.join("fifo"));
    symlink("/dev/zero", root.join("zero")).unwrap();
    std::fs::write(root.join("big"), vec![b'x'; 100]).unwrap();
    let r = Root::open(root).unwrap();
    // Each of these would block or never end if it were opened.
    assert!(r.read(Path::new("fifo"), MAX_READ).is_err());
    assert!(r.read(Path::new("zero"), MAX_READ).is_err());
    assert!(read_path(&root.join("fifo"), true, MAX_READ).is_err());
    assert!(read_path(&root.join("zero"), true, MAX_READ).is_err());
    assert!(read_path(&root.join("zero"), false, MAX_READ).is_err());
    assert!(read_path(&root.join("big"), false, 10).is_err());
    assert_eq!(
        read_path(&root.join("big"), false, 100)
            .unwrap()
            .unwrap()
            .len(),
        100
    );
    assert!(read_path(&root.join("missing"), false, 10)
        .unwrap()
        .is_none());
    assert!(Source::open(&root.join("fifo")).is_err());
    assert!(Source::open(&root.join("zero")).is_err());
    let d = r.dir(Path::new(""), false).unwrap();
    assert_eq!(d.kind(OsStr::new("fifo")).unwrap(), Some(Kind::Other));
    assert_eq!(d.kind(OsStr::new("zero")).unwrap(), Some(Kind::Symlink));
    assert_eq!(d.kind(OsStr::new("none")).unwrap(), None);
}

#[test]
fn remove_tree_never_follows_links_and_handles_read_only_folders() {
    let t = tempfile::tempdir().unwrap();
    let (root, outside) = (t.path().join("home"), t.path().join("outside"));
    std::fs::create_dir_all(root.join("a/ro/deep")).unwrap();
    std::fs::create_dir_all(outside.join("keyrings")).unwrap();
    std::fs::write(outside.join("keyrings/login"), "secret").unwrap();
    std::fs::write(root.join("a/ro/deep/f"), "x").unwrap();
    symlink(&outside, root.join("a/ro/link")).unwrap();
    mkfifo(&root.join("a/fifo"));
    let ro = |p: &Path, m| std::fs::set_permissions(p, PermissionsExt::from_mode(m)).unwrap();
    use std::os::unix::fs::PermissionsExt;
    ro(&root.join("a/ro/deep"), 0o500);
    // macOS cannot open a folder without read and search permission, so
    // there the locked folder keeps search only.
    ro(
        &root.join("a/ro"),
        if cfg!(target_os = "macos") {
            0o100
        } else {
            0o000
        },
    );
    // A planted folder link on the way: nothing outside is removed.
    symlink(&outside, root.join(".local")).unwrap();
    let r = Root::open(&root).unwrap();
    assert!(r.remove_tree(Path::new(".local/keyrings")).is_err());
    assert!(outside.join("keyrings/login").exists());
    // The link itself goes, its target stays.
    r.remove_tree(Path::new(".local")).unwrap();
    assert!(outside.join("keyrings/login").exists());
    r.remove_tree(Path::new("a")).unwrap();
    assert!(!root.join("a").exists());
    assert!(outside.join("keyrings/login").exists());
    r.remove_tree(Path::new("missing/x")).unwrap();
    // By path, the last component is not followed either.
    symlink(&outside, t.path().join("l")).unwrap();
    remove_tree_path(&t.path().join("l")).unwrap();
    assert!(outside.join("keyrings/login").exists());
}

#[test]
fn approved_paths_refuse_links_in_writable_folders() {
    let t = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(t.path()).unwrap();
    let (project, host) = (base.join("project"), base.join("host"));
    std::fs::create_dir_all(project.join("vendor/lib")).unwrap();
    std::fs::create_dir_all(host.join("docs")).unwrap();
    let w = vec![project.clone()];
    // Plain folders and `..` resolve as they are.
    assert_eq!(
        resolve_approved(&project.join("vendor/lib"), &w).unwrap(),
        project.join("vendor/lib")
    );
    assert_eq!(
        resolve_approved(&project.join("vendor/../vendor"), &w).unwrap(),
        project.join("vendor")
    );
    // A host link (outside the writable folders) is followed.
    symlink(&host, base.join("host-link")).unwrap();
    assert_eq!(
        resolve_approved(&base.join("host-link/docs"), &w).unwrap(),
        host.join("docs")
    );
    // A link that the sandbox could plant in the project is refused,
    // also when it is reached through a host link.
    symlink(host.join("docs"), project.join("shared")).unwrap();
    assert!(resolve_approved(&project.join("shared"), &w).is_err());
    symlink(&project, base.join("p-link")).unwrap();
    assert!(resolve_approved(&base.join("p-link/shared"), &w).is_err());
    symlink(&host, project.join("vendor/up")).unwrap();
    assert!(resolve_approved(&project.join("vendor/up/docs"), &w).is_err());
    assert!(resolve_approved(&project.join("missing"), &w).is_err());
}

#[test]
fn sources_copy_what_was_opened() {
    let t = tempfile::tempdir().unwrap();
    let (src, dst) = (t.path().join("src"), t.path().join("dst"));
    std::fs::create_dir_all(src.join("d")).unwrap();
    std::fs::create_dir_all(&dst).unwrap();
    std::fs::write(src.join("d/f"), "abc").unwrap();
    std::fs::write(src.join("big"), vec![0u8; 50]).unwrap();
    mkfifo(&src.join("d/fifo"));
    symlink("/etc/passwd", src.join("d/link")).unwrap();
    let s = Source::open(&src).unwrap();
    assert!(s.is_dir());
    assert_eq!(s.size(1000).unwrap(), 53);
    // Renaming the folder after it was opened changes nothing.
    std::fs::rename(&src, t.path().join("moved")).unwrap();
    let base = std::fs::canonicalize(t.path()).unwrap();
    assert_eq!(s.real_path().unwrap(), base.join("moved"));
    let d = Root::open(&dst).unwrap().dir(Path::new(""), false).unwrap();
    let n = d.copy_source(&s, "src", 1000).unwrap();
    assert_eq!(
        std::fs::read_to_string(dst.join(&n).join("d/f")).unwrap(),
        "abc"
    );
    assert!(std::fs::symlink_metadata(dst.join(&n).join("d/fifo")).is_err());
    assert!(std::fs::symlink_metadata(dst.join(&n).join("d/link")).is_err());
    // Over the limit: nothing stays behind.
    assert!(d.copy_source(&s, "small", 10).is_err());
    assert!(!dst.join("small").exists());
}

#[test]
fn connect_refuses_a_link_to_another_socket() {
    let t = tempfile::tempdir().unwrap();
    let _l = std::os::unix::net::UnixListener::bind(t.path().join("host.sock")).unwrap();
    let _m = std::os::unix::net::UnixListener::bind(t.path().join("mine.sock")).unwrap();
    symlink(t.path().join("host.sock"), t.path().join("planted.sock")).unwrap();
    let d = Root::open(t.path())
        .unwrap()
        .dir(Path::new(""), false)
        .unwrap();
    assert!(d.connect(OsStr::new("planted.sock")).is_err());
    std::fs::write(t.path().join("file"), "").unwrap();
    assert!(d.connect(OsStr::new("file")).is_err());
    let mine = d.connect(OsStr::new("mine.sock"));
    if cfg!(target_os = "macos") {
        let e = format!("{:#}", mine.unwrap_err());
        assert!(e.contains("not available on macOS"), "{e}");
    } else {
        assert!(mine.is_ok());
    }
}

#[test]
fn lists_and_reads_repeat_on_one_open_entry() {
    let t = tempfile::tempdir().unwrap();
    std::fs::write(t.path().join("a"), "abc").unwrap();
    std::fs::create_dir(t.path().join("b")).unwrap();
    let d = Root::open(t.path())
        .unwrap()
        .dir(Path::new(""), false)
        .unwrap();
    // Each listing starts again at the first entry.
    for _ in 0..2 {
        let mut e = d.entries().unwrap();
        e.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(e, vec![("a".into(), Kind::File), ("b".into(), Kind::Dir)]);
    }
    // A file source reads in full each time.
    let s = Source::open(&t.path().join("a")).unwrap();
    assert_eq!(s.size(100).unwrap(), 3);
    let (x, y) = (
        d.copy_source(&s, "x", 100).unwrap(),
        d.copy_source(&s, "y", 100).unwrap(),
    );
    assert_eq!(std::fs::read(t.path().join(x)).unwrap(), b"abc");
    assert_eq!(std::fs::read(t.path().join(y)).unwrap(), b"abc");
}

#[test]
fn moves_never_replace_and_exchange_swaps() {
    let t = tempfile::tempdir().unwrap();
    let (root, host) = (t.path().join("root"), t.path().join("host"));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&host).unwrap();
    std::fs::write(root.join("taken"), "sandbox").unwrap();
    std::fs::write(host.join("secret"), "host").unwrap();
    symlink(host.join("secret"), root.join("link")).unwrap();
    std::fs::write(host.join("new"), "new").unwrap();
    let d = Root::open(&root)
        .unwrap()
        .dir(Path::new(""), false)
        .unwrap();
    // An entry or a symlink at the target stays as it is.
    let e = d
        .move_in(&host.join("new"), OsStr::new("taken"))
        .unwrap_err();
    assert_eq!(e.raw_os_error(), Some(libc::EEXIST));
    assert!(d.move_in(&host.join("new"), OsStr::new("link")).is_err());
    assert_eq!(
        std::fs::read_to_string(root.join("taken")).unwrap(),
        "sandbox"
    );
    assert_eq!(
        std::fs::read_to_string(host.join("secret")).unwrap(),
        "host"
    );
    d.move_in(&host.join("new"), OsStr::new("free")).unwrap();
    assert_eq!(std::fs::read_to_string(root.join("free")).unwrap(), "new");
    // create_new never opens or replaces an existing entry.
    assert!(d.create_new(OsStr::new("taken"), 0o600).is_err());
    assert!(d.create_new(OsStr::new("link"), 0o600).is_err());
    assert_eq!(
        std::fs::read_to_string(host.join("secret")).unwrap(),
        "host"
    );
    // An exchange swaps two entries. A symlink moves as it is.
    d.exchange(OsStr::new("taken"), OsStr::new("link")).unwrap();
    assert_eq!(
        std::fs::read_to_string(root.join("link")).unwrap(),
        "sandbox"
    );
    assert_eq!(d.kind(OsStr::new("taken")).unwrap(), Some(Kind::Symlink));
    assert_eq!(
        std::fs::read_to_string(host.join("secret")).unwrap(),
        "host"
    );
    assert!(d
        .exchange(OsStr::new("free"), OsStr::new("missing"))
        .is_err());
}

/// The macOS calls: `fstatat` before an open, `O_SEARCH`, `F_GETPATH`,
/// `fdopendir`, `fchmod` and `renameatx_np`.
#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn a_folder_swapped_for_a_symlink_is_not_followed() {
        let t = tempfile::tempdir().unwrap();
        let (root, outside) = (t.path().join("root"), t.path().join("outside"));
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        std::fs::create_dir_all(outside.join("b")).unwrap();
        std::fs::write(root.join("a/b/f"), "mine").unwrap();
        std::fs::write(outside.join("b/f"), "host").unwrap();
        let r = Root::open(&root).unwrap();
        let held = r.dir(Path::new("a"), false).unwrap();
        // The sandbox moves the folder away and plants a symlink.
        std::fs::rename(root.join("a"), root.join("moved")).unwrap();
        symlink(&outside, root.join("a")).unwrap();
        assert!(r.read(Path::new("a/b/f"), MAX_READ).is_err());
        assert!(r.write(Path::new("a/b/g"), b"x", 0o600).is_err());
        assert!(r.remove_tree(Path::new("a/b")).is_err());
        assert!(!outside.join("b/g").exists());
        assert!(outside.join("b/f").exists());
        // The open folder stays the one that was opened.
        let base = std::fs::canonicalize(t.path()).unwrap();
        assert_eq!(held.real_path().unwrap(), base.join("root/moved"));
        assert_eq!(
            held.subdir(OsStr::new("b"), false)
                .unwrap()
                .read(OsStr::new("f"), MAX_READ)
                .unwrap()
                .unwrap(),
            b"mine"
        );
        // A symlink as the last name is the link itself.
        let d = r.dir(Path::new(""), false).unwrap();
        assert_eq!(d.kind(OsStr::new("a")).unwrap(), Some(Kind::Symlink));
        assert!(d.read(OsStr::new("a"), MAX_READ).is_err());
        assert!(read_path(&root.join("a"), false, MAX_READ).is_err());
    }

    #[test]
    fn renameatx_np_excl_never_replaces() {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(t.path().join("from"), "1").unwrap();
        std::fs::write(t.path().join("to"), "2").unwrap();
        let dir = Root::open(t.path()).unwrap().fd;
        let (f, to) = (
            cstr(OsStr::new("from")).unwrap(),
            cstr(OsStr::new("to")).unwrap(),
        );
        let e = rename_noreplace(dir.as_raw_fd(), &f, dir.as_raw_fd(), &to).unwrap_err();
        assert_eq!(e.raw_os_error(), Some(libc::EEXIST));
        assert_eq!(std::fs::read_to_string(t.path().join("to")).unwrap(), "2");
        rename_exchange(dir.as_raw_fd(), &f, &to).unwrap();
        assert_eq!(std::fs::read_to_string(t.path().join("to")).unwrap(), "1");
        assert_eq!(std::fs::read_to_string(t.path().join("from")).unwrap(), "2");
    }

    #[test]
    fn special_files_are_never_opened() {
        let t = tempfile::tempdir().unwrap();
        mkfifo(&t.path().join("fifo"));
        symlink("/dev/null", t.path().join("null")).unwrap();
        let fifo = cstr(t.path().join("fifo").as_os_str()).unwrap();
        let n = Node::at(libc::AT_FDCWD, &fifo, false).unwrap();
        assert!(n.fd.is_none());
        assert_eq!(n.kind(), Kind::Other);
        let null = cstr(t.path().join("null").as_os_str()).unwrap();
        let n = Node::at(libc::AT_FDCWD, &null, true).unwrap();
        assert!(n.fd.is_none());
        let n = Node::at(libc::AT_FDCWD, &null, false).unwrap();
        assert!(n.fd.is_none());
        assert_eq!(n.kind(), Kind::Symlink);
    }

    #[test]
    fn a_folder_without_read_and_search_is_refused_clearly() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("home");
        std::fs::create_dir_all(root.join("locked/x")).unwrap();
        std::fs::set_permissions(root.join("locked"), PermissionsExt::from_mode(0o000)).unwrap();
        let r = Root::open(&root).unwrap();
        let e = format!("{:#}", r.remove_tree(Path::new("locked")).unwrap_err());
        assert!(e.contains("not available on macOS"), "{e}");
        std::fs::set_permissions(root.join("locked"), PermissionsExt::from_mode(0o700)).unwrap();
        r.remove_tree(Path::new("locked")).unwrap();
        assert!(!root.join("locked").exists());
    }

    #[test]
    fn proc_path_is_refused_clearly() {
        let t = tempfile::tempdir().unwrap();
        let s = Source::open(t.path()).unwrap();
        let e = format!("{:#}", s.proc_path().unwrap_err());
        assert!(e.contains("not available on macOS"), "{e}");
    }
}
