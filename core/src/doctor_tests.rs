use super::*;
use std::os::unix::fs::PermissionsExt;

#[test]
fn each_escape_run_gets_its_own_scratch_and_removes_only_that() {
    let t = tempfile::tempdir().unwrap();
    let base = t.path().join("cache/paddock/doctor-runs");
    let a = Scratch::new(&base).unwrap();
    let mut b = Scratch::new(&base).unwrap();
    assert_ne!(a.root, b.root);
    for s in [&a, &b] {
        assert_eq!(s.root.parent(), Some(base.as_path()));
        let m = std::fs::metadata(&s.root).unwrap().permissions().mode();
        assert_eq!(m & 0o777, 0o700);
    }
    // What a run leaves: a git object folder without write permission,
    // a link out of the scratch folder, and its own sandbox home.
    let outside = t.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("keep"), "x").unwrap();
    for s in [&a, &b] {
        let objects = s.root.join("project/.git/objects/ab");
        std::fs::create_dir_all(&objects).unwrap();
        std::fs::write(objects.join("cd"), "x").unwrap();
        std::fs::set_permissions(&objects, std::fs::Permissions::from_mode(0o500)).unwrap();
        std::os::unix::fs::symlink(&outside, s.root.join("project/link")).unwrap();
    }
    let home_b = t.path().join("data/homes/doctor/shared/b");
    std::fs::create_dir_all(&home_b).unwrap();
    b.also_remove(home_b.clone());

    let (a_root, b_root) = (a.root.clone(), b.root.clone());
    drop(b);
    assert!(!b_root.exists());
    assert!(!home_b.exists());
    assert!(a_root.join("project/.git/objects/ab/cd").exists());
    assert!(outside.join("keep").exists(), "the link was followed");

    // A new run removes what dead runs and older versions left, never
    // the folder of a live run.
    let dead = base.join(format!("{}-0011223344556677", i32::MAX));
    std::fs::create_dir_all(dead.join("project")).unwrap();
    std::fs::create_dir_all(base.join("project")).unwrap();
    std::fs::write(base.join("host-canary"), "x").unwrap();
    let c = Scratch::new(&base).unwrap();
    assert!(!dead.exists());
    assert!(!base.join("project").exists());
    assert!(!base.join("host-canary").exists());
    assert!(a_root.exists());
    drop(a);
    drop(c);
    assert_eq!(std::fs::read_dir(&base).unwrap().count(), 0);
    assert!(outside.join("keep").exists());
}
