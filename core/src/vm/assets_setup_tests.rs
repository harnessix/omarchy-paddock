use super::*;
use crate::vm::assets::tests::fake_assets;
use std::collections::BTreeMap;

fn env_in(t: &Path, sandbox: bool) -> HostEnv {
    let mut vars = BTreeMap::new();
    vars.insert("XDG_DATA_HOME".into(), t.join("data").display().to_string());
    if sandbox {
        vars.insert("PADDOCK".into(), "1".into());
    }
    HostEnv::from_parts(t.join("home"), "demo".into(), 1000, 1000, vars)
}

/// A build folder with fake files, and the target folder of a setup.
fn build_dir(t: &Path, with_initrd: bool) -> (PathBuf, PathBuf) {
    let from = t.join("build");
    std::fs::create_dir_all(&from).unwrap();
    fake_assets(&from, with_initrd);
    (from, t.join("data/paddock/vm"))
}

#[test]
fn setup_from_a_folder_installs_matching_files() {
    let t = tempfile::tempdir().unwrap();
    let env = env_in(t.path(), false);
    let (from, target) = build_dir(t.path(), true);
    let m = Manifest::read(&from).unwrap();
    let src = Source::Dir(from.clone());
    let lines = plan_lines(&m, &src, &target).join("\n");
    assert!(
        lines.contains(&from.join(KERNEL).display().to_string()),
        "{lines}"
    );
    assert!(lines.contains(&m.files[0].sha256), "{lines}");
    let a = install(&env, &m, &src, &target).unwrap();
    assert_eq!(a.initrd, Some(target.join(INITRD)));
    assert_eq!(
        std::fs::read(target.join(KERNEL)).unwrap(),
        std::fs::read(from.join(KERNEL)).unwrap()
    );
    // No staging folder stays behind.
    let left: Vec<_> = std::fs::read_dir(target.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(left, vec![std::ffi::OsString::from("vm")]);

    // A second setup without an initrd removes the old one.
    let (from2, _) = build_dir(&t.path().join("b2"), false);
    let m2 = Manifest::read(&from2).unwrap();
    let a = install(&env, &m2, &Source::Dir(from2), &target).unwrap();
    assert_eq!(a.initrd, None);
    assert!(!target.join(INITRD).exists());
}

#[test]
fn setup_from_a_folder_refuses_a_changed_file() {
    let t = tempfile::tempdir().unwrap();
    let env = env_in(t.path(), false);
    let (from, target) = build_dir(t.path(), false);
    let m = Manifest::read(&from).unwrap();
    std::fs::write(from.join(ROOTFS), "fake rootfs.imX").unwrap();
    let e = format!(
        "{:#}",
        install(&env, &m, &Source::Dir(from), &target).unwrap_err()
    );
    assert!(e.contains("does not match"), "{e}");
    assert!(!target.join(MANIFEST).exists());
}

#[test]
fn a_failed_setup_keeps_the_installed_files() {
    let t = tempfile::tempdir().unwrap();
    let env = env_in(t.path(), false);
    let (from, target) = build_dir(t.path(), false);
    let m = Manifest::read(&from).unwrap();
    install(&env, &m, &Source::Dir(from.clone()), &target).unwrap();
    std::fs::remove_file(from.join(KERNEL)).unwrap();
    assert!(install(&env, &m, &Source::Dir(from), &target).is_err());
    assert!(assets::check(&target).is_ok());
}

#[test]
fn setup_by_download_checks_size_and_sha256() {
    let t = tempfile::tempdir().unwrap();
    let env = env_in(t.path(), false);
    let (from, target) = build_dir(t.path(), false);
    let m = Manifest::read(&from).unwrap();
    // A fake download: the bytes of the build folder, by the URL's name.
    let good = |url: &str| -> Result<Vec<u8>> {
        let name = url.rsplit('/').next().unwrap();
        Ok(std::fs::read(from.join(name))?)
    };
    install(&env, &m, &Source::Download(&good), &target).unwrap();

    let short = |_: &str| -> Result<Vec<u8>> { Ok(b"short".to_vec()) };
    let e = format!(
        "{:#}",
        install(&env, &m, &Source::Download(&short), &target).unwrap_err()
    );
    assert!(e.contains("bytes, the list says"), "{e}");

    let other = |url: &str| -> Result<Vec<u8>> {
        let mut b = good(url)?;
        b[0] ^= 1;
        Ok(b)
    };
    let e = format!(
        "{:#}",
        install(&env, &m, &Source::Download(&other), &target).unwrap_err()
    );
    assert!(e.contains("does not match"), "{e}");
    assert!(assets::check(&target).is_ok());
}

#[test]
fn setup_and_remove_refuse_in_a_sandbox() {
    let t = tempfile::tempdir().unwrap();
    let env = env_in(t.path(), true);
    let (from, target) = build_dir(t.path(), false);
    let m = Manifest::read(&from).unwrap();
    let e = install(&env, &m, &Source::Dir(from.clone()), &target).unwrap_err();
    assert!(e.to_string().contains("host only"), "{e}");
    let e = remove(&env, &from).unwrap_err();
    assert!(e.to_string().contains("host only"), "{e}");
}

#[test]
fn remove_deletes_the_guest_files_only() {
    let t = tempfile::tempdir().unwrap();
    let env = env_in(t.path(), false);
    let (from, target) = build_dir(t.path(), true);
    let m = Manifest::read(&from).unwrap();
    install(&env, &m, &Source::Dir(from), &target).unwrap();
    assert!(remove(&env, &target).unwrap());
    assert!(!target.exists());
    assert!(!remove(&env, &target).unwrap());

    // Other files stay, and so does the folder.
    std::fs::create_dir_all(&target).unwrap();
    fake_assets(&target, false);
    std::fs::write(target.join("notes"), "mine").unwrap();
    assert!(remove(&env, &target).unwrap());
    assert!(target.join("notes").is_file());
    assert!(present(&target).is_empty());
}
