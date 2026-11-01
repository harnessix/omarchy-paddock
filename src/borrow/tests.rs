//! Borrow and return on temporary folders.

use super::*;
use std::os::unix::fs::symlink;

fn env(t: &Path) -> HostEnv {
    let home = t.join("home");
    std::fs::create_dir_all(&home).unwrap();
    HostEnv::from_parts(home, "tester".into(), 1000, 1000, BTreeMap::new())
}

fn no_check(_: &Path, _: &Path) -> Result<()> {
    Ok(())
}

/// A host folder with a file, a script, a subfolder, a git directory and a
/// symlink inside it.
fn original(t: &Path) -> PathBuf {
    let o = t.join("lib");
    std::fs::create_dir_all(o.join("src")).unwrap();
    std::fs::create_dir_all(o.join(".git/hooks")).unwrap();
    std::fs::write(o.join("README"), "old readme\n").unwrap();
    std::fs::write(o.join("src/a.rs"), "fn a() {}\n").unwrap();
    std::fs::write(o.join("src/gone.rs"), "fn gone() {}\n").unwrap();
    std::fs::write(o.join("run.sh"), "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(o.join("run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(o.join(".git/config"), "[core]\n").unwrap();
    symlink("src/a.rs", o.join("link")).unwrap();
    o
}

fn borrowed(e: &HostEnv, o: &Path) -> (String, PathBuf) {
    let p = plan_borrow(e, o, no_check).unwrap();
    let r = borrow(e, "s-1", &p, DEFAULT_MAX).unwrap();
    let name = r["name"].as_str().unwrap().to_string();
    let copy = dir(e, "s-1").join(&name);
    (name, copy)
}

#[test]
fn git_files_that_paddock_binds_read_only_are_protected() {
    for (r, k) in [
        (".git/config", Kind::File),
        (".git/hooks/pre-commit", Kind::File),
        (".git/hooks", Kind::Dir),
        (".git/commondir", Kind::File),
        (".git/objects/info/alternates", Kind::File),
        (".git/worktrees/w1/gitdir", Kind::File),
        ("sub/.git/config", Kind::File),
        (".git", Kind::File),
    ] {
        assert!(git_protected(r, k), "{r}");
    }
    for (r, k) in [
        (".git", Kind::Dir),
        (".git/HEAD", Kind::File),
        (".git/refs/heads/main", Kind::File),
        ("config", Kind::File),
        ("hooks/x", Kind::File),
    ] {
        assert!(!git_protected(r, k), "{r}");
    }
}

#[test]
fn symlinks_that_leave_the_folder_are_found() {
    assert!(escapes("a", Path::new("/etc/passwd")));
    assert!(escapes("a", Path::new("../x")));
    assert!(escapes("d/a", Path::new("../../x")));
    assert!(escapes("d/a", Path::new("x/../../../y")));
    assert!(!escapes("d/a", Path::new("../x")));
    assert!(!escapes("a", Path::new("d/./x")));
}

#[test]
fn sizes_parse() {
    assert_eq!(parse_size("1G").unwrap(), 1 << 30);
    assert_eq!(parse_size("512m").unwrap(), 512 << 20);
    assert_eq!(parse_size("4096").unwrap(), 4096);
    assert!(parse_size("0").is_err() && parse_size("1X").is_err() && parse_size("").is_err());
}

#[test]
fn the_manifest_finds_added_changed_and_deleted_entries() {
    let t = tempfile::tempdir().unwrap();
    let o = original(t.path());
    let before = scan(&o).unwrap();
    assert_eq!(before["README"].kind, Kind::File);
    assert!(before["README"].hash.is_some());
    assert_eq!(
        before["link"].target.as_deref(),
        Some(Path::new("src/a.rs"))
    );
    // Same size and time, other content: the hash finds it.
    let m = std::fs::metadata(o.join("README")).unwrap();
    std::fs::write(o.join("README"), "new readme\n").unwrap();
    let f = std::fs::File::options()
        .write(true)
        .open(o.join("README"))
        .unwrap();
    f.set_modified(m.modified().unwrap()).unwrap();
    std::fs::remove_file(o.join("src/gone.rs")).unwrap();
    std::fs::write(o.join("new"), "x").unwrap();
    let (a, c, d) = diff(&before, &scan(&o).unwrap());
    assert_eq!(a, vec!["new"]);
    assert_eq!(c, vec!["README"]);
    assert_eq!(d, vec!["src/gone.rs"]);
}

#[test]
fn a_borrow_copies_the_folder_and_keeps_the_original() {
    let t = tempfile::tempdir().unwrap();
    let e = env(t.path());
    let o = original(t.path());
    // The last part of the path is no symlink, and a file is no folder.
    symlink(&o, t.path().join("via")).unwrap();
    assert!(plan_borrow(&e, &t.path().join("via"), no_check).is_err());
    assert!(plan_borrow(&e, &o.join("README"), no_check).is_err());
    let (name, copy) = borrowed(&e, &o);
    assert_eq!(name, "lib");
    assert_eq!(
        std::fs::read_to_string(copy.join("src/a.rs")).unwrap(),
        "fn a() {}\n"
    );
    assert_eq!(
        std::fs::read_link(copy.join("link")).unwrap(),
        Path::new("src/a.rs")
    );
    assert_eq!(list(&e, "s-1"), vec!["lib"]);
    // A second borrow of the same folder gets a free name.
    let (second, _) = borrowed(&e, &o);
    assert_eq!(second, "lib-1");
    // The sandbox end removes every copy and manifest.
    end(&e, "s-1");
    assert!(!dir(&e, "s-1").exists());
    assert!(list(&e, "s-1").is_empty());
    assert!(o.join("README").exists());
}

#[test]
fn without_reflinks_the_size_limit_holds() {
    let t = tempfile::tempdir().unwrap();
    let e = env(t.path());
    let o = original(t.path());
    std::fs::write(o.join("big"), vec![7u8; 4096]).unwrap();
    let p = plan_borrow(&e, &o, no_check).unwrap();
    // A file system with reflinks copies it anyway; others refuse.
    let src = p.src.proc_path();
    let stage = t.path().join("stage");
    std::fs::create_dir(&stage).unwrap();
    let entries = scan(&src).unwrap();
    if let Err(err) = copy_tree(&src, &stage, &entries, 100) {
        assert!(format!("{err:#}").contains("[borrow] max"), "{err:#}");
    }
}

#[test]
fn a_return_writes_the_changes_back_with_the_safety_rules() {
    let t = tempfile::tempdir().unwrap();
    let e = env(t.path());
    let o = original(t.path());
    let (name, copy) = borrowed(&e, &o);
    std::fs::write(copy.join("README"), "new readme\n").unwrap();
    std::fs::remove_file(copy.join("src/gone.rs")).unwrap();
    std::fs::create_dir(copy.join("docs")).unwrap();
    std::fs::write(copy.join("docs/new.sh"), "echo\n").unwrap();
    std::fs::set_permissions(
        copy.join("docs/new.sh"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    std::fs::write(copy.join(".git/config"), "[core]\nfsmonitor = evil\n").unwrap();
    std::fs::write(copy.join(".git/hooks/pre-commit"), "evil").unwrap();
    symlink("../README", copy.join("docs/readme")).unwrap();
    let plan = plan_return(&e, "s-1", &name, DEFAULT_MAX).unwrap();
    let c = &plan.changes;
    assert_eq!(c.added, vec!["docs", "docs/new.sh", "docs/readme"]);
    assert_eq!(c.changed, vec!["README"]);
    assert_eq!(c.deleted, vec!["src/gone.rs"]);
    assert_eq!(c.kept, vec![".git/config", ".git/hooks/pre-commit"]);
    let card = plan.detail();
    assert!(card.contains("3 added, 1 changed, 1 deleted"), "{card}");
    assert!(card.contains("Not written"), "{card}");
    assert!(!card.contains("WARNING"), "{card}");
    plan.apply(&e, "s-1").unwrap();
    assert_eq!(
        std::fs::read_to_string(o.join("README")).unwrap(),
        "new readme\n"
    );
    assert!(!o.join("src/gone.rs").exists());
    assert_eq!(
        std::fs::read_to_string(o.join("docs/new.sh")).unwrap(),
        "echo\n"
    );
    let mode = std::fs::metadata(o.join("docs/new.sh")).unwrap().mode();
    assert_eq!(mode & 0o111, 0, "a new file has no execute bit");
    assert_eq!(
        std::fs::metadata(o.join("run.sh")).unwrap().mode() & 0o777,
        0o755
    );
    assert_eq!(
        std::fs::read_link(o.join("docs/readme")).unwrap(),
        Path::new("../README")
    );
    assert_eq!(
        std::fs::read_to_string(o.join(".git/config")).unwrap(),
        "[core]\n"
    );
    assert!(!o.join(".git/hooks/pre-commit").exists());
    // The copy and its manifest are gone.
    assert!(!copy.exists());
    assert!(list(&e, "s-1").is_empty());
}

#[test]
fn a_return_refuses_when_the_original_changed_after_the_borrow() {
    let t = tempfile::tempdir().unwrap();
    let e = env(t.path());
    let o = original(t.path());
    let (name, copy) = borrowed(&e, &o);
    std::fs::write(copy.join("README"), "agent\n").unwrap();
    std::fs::write(o.join("src/a.rs"), "fn a() { user() }\n").unwrap();
    let err = plan_return(&e, "s-1", &name, DEFAULT_MAX).err().unwrap();
    assert!(
        format!("{err:#}").contains("changed after the borrow"),
        "{err:#}"
    );
    assert_eq!(
        std::fs::read_to_string(o.join("README")).unwrap(),
        "old readme\n"
    );

    // A change while the user decides: the second check refuses.
    std::fs::write(o.join("src/a.rs"), "fn a() {}\n").unwrap();
    let (name, copy) = borrowed(&e, &o);
    std::fs::write(copy.join("README"), "agent\n").unwrap();
    let plan = plan_return(&e, "s-1", &name, DEFAULT_MAX).unwrap();
    std::fs::write(o.join("late"), "user").unwrap();
    assert!(plan.apply(&e, "s-1").is_err());
    assert_eq!(
        std::fs::read_to_string(o.join("README")).unwrap(),
        "old readme\n"
    );
}

#[test]
fn a_return_refuses_symlinks_that_point_outside() {
    let t = tempfile::tempdir().unwrap();
    let e = env(t.path());
    let o = original(t.path());
    let (name, copy) = borrowed(&e, &o);
    symlink("/etc/passwd", copy.join("pw")).unwrap();
    let err = plan_return(&e, "s-1", &name, DEFAULT_MAX).err().unwrap();
    assert!(format!("{err:#}").contains("outside the folder"), "{err:#}");
    assert!(!o.join("pw").exists());
}

#[test]
fn a_symlinked_folder_in_the_copy_is_never_followed() {
    let t = tempfile::tempdir().unwrap();
    let e = env(t.path());
    let o = original(t.path());
    let (name, copy) = borrowed(&e, &o);
    // The sandbox replaces a folder with a link to a host folder.
    let secret = t.path().join("secret");
    std::fs::create_dir(&secret).unwrap();
    std::fs::write(secret.join("key"), "k").unwrap();
    std::fs::remove_dir_all(copy.join("src")).unwrap();
    symlink(&secret, copy.join("src")).unwrap();
    let err = plan_return(&e, "s-1", &name, DEFAULT_MAX).err().unwrap();
    assert!(format!("{err:#}").contains("outside the folder"), "{err:#}");
}

#[test]
fn a_folder_that_host_programs_read_gets_a_warning() {
    let t = tempfile::tempdir().unwrap();
    let e = env(t.path());
    assert!(host_read_warning(&e, &e.home.join(".config/app")).is_some());
    assert!(host_read_warning(&e, Path::new("/etc/x")).is_some());
    assert!(host_read_warning(&e, &e.home.join("src/lib")).is_none());
}
