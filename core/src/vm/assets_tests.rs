use super::*;
use std::collections::BTreeMap;

/// Write fake guest files and a matching `assets.toml` into `dir`.
pub(crate) fn fake_assets(dir: &Path, with_initrd: bool) {
    let mut names = vec![KERNEL, ROOTFS];
    if with_initrd {
        names.push(INITRD);
    }
    let mut toml = String::new();
    for name in names {
        let body = format!("fake {name}");
        std::fs::write(dir.join(name), &body).unwrap();
        let sum = sha256_file(&dir.join(name)).unwrap();
        toml.push_str(&format!(
            "[[file]]\nname = \"{name}\"\nurl = \"https://example.invalid/{name}\"\n\
             sha256 = \"{sum}\"\nsize = {}\nversion = \"1\"\n\n",
            body.len()
        ));
    }
    std::fs::write(dir.join(MANIFEST), toml).unwrap();
}

fn err(dir: &Path) -> String {
    format!("{:#}", check(dir).unwrap_err())
}

fn env_in(t: &Path) -> HostEnv {
    let mut vars = BTreeMap::new();
    vars.insert("XDG_DATA_HOME".into(), t.join("data").display().to_string());
    HostEnv::from_parts(t.join("home"), "demo".into(), 1000, 1000, vars)
}

#[test]
fn assets_dir_is_under_the_data_dir() {
    let t = tempfile::tempdir().unwrap();
    let env = env_in(t.path());
    let want = t.path().join("data/paddock/vm");
    assert_eq!(default_assets_dir(&env), want);
    std::fs::create_dir_all(&want).unwrap();
    fake_assets(&want, false);
    assert_eq!(assets_dir(&env), want);
}

#[test]
fn the_pinned_list_parses() {
    let m = Manifest::parse(PINNED).unwrap();
    assert!(m.get(KERNEL).is_some() && m.get(ROOTFS).is_some());
    for a in &m.files {
        assert!(a.url.starts_with("https://github.com/"), "{}", a.url);
        assert!(a.size > 0);
    }
}

#[test]
fn a_list_survives_a_round_trip() {
    let m = Manifest::parse(PINNED).unwrap();
    let again = Manifest::parse(&toml::to_string(&m).unwrap()).unwrap();
    assert_eq!(m, again);
}

#[test]
fn check_accepts_matching_files() {
    let t = tempfile::tempdir().unwrap();
    fake_assets(t.path(), false);
    let a = check(t.path()).unwrap();
    assert_eq!(a.kernel, t.path().join(KERNEL));
    assert_eq!(a.rootfs, t.path().join(ROOTFS));
    assert_eq!(a.initrd, None);
    assert_eq!(a.files.len(), 2);
    fake_assets(t.path(), true);
    assert_eq!(check(t.path()).unwrap().initrd, Some(t.path().join(INITRD)));
}

#[test]
fn check_refuses_a_missing_list_or_file() {
    let t = tempfile::tempdir().unwrap();
    let e = err(t.path());
    assert!(e.contains("not set up"), "{e}");
    fake_assets(t.path(), true);
    std::fs::remove_file(t.path().join(INITRD)).unwrap();
    let e = err(t.path());
    assert!(e.contains("missing"), "{e}");
}

#[test]
fn check_refuses_a_changed_file() {
    let t = tempfile::tempdir().unwrap();
    fake_assets(t.path(), false);
    // Same size, other bytes.
    std::fs::write(t.path().join(ROOTFS), "fake rootfs.imX").unwrap();
    let e = err(t.path());
    assert!(e.contains("does not match"), "{e}");
    std::fs::write(t.path().join(ROOTFS), "longer than before").unwrap();
    let e = err(t.path());
    assert!(e.contains("bytes"), "{e}");
}

#[test]
fn check_refuses_a_symlink() {
    let t = tempfile::tempdir().unwrap();
    fake_assets(t.path(), false);
    let real = t.path().join("real");
    std::fs::rename(t.path().join(KERNEL), &real).unwrap();
    std::os::unix::fs::symlink(&real, t.path().join(KERNEL)).unwrap();
    let e = err(t.path());
    assert!(e.contains("not a regular file"), "{e}");
}

#[test]
fn check_refuses_a_bad_list() {
    let t = tempfile::tempdir().unwrap();
    fake_assets(t.path(), false);
    let list = std::fs::read_to_string(t.path().join(MANIFEST)).unwrap();
    // No kernel in the list.
    let no_kernel: String = list.split("\n\n").skip(1).collect::<Vec<_>>().join("\n\n");
    std::fs::write(t.path().join(MANIFEST), no_kernel).unwrap();
    let e = err(t.path());
    assert!(e.contains("no kernel"), "{e}");
    // An unknown name, which could also be a path.
    let other = list.replace("name = \"kernel\"", "name = \"../kernel\"");
    std::fs::write(t.path().join(MANIFEST), other).unwrap();
    let e = err(t.path());
    assert!(e.contains("unknown VM file"), "{e}");
    // A short sha256.
    let short = list.replacen("sha256 = \"", "sha256 = \"00", 1);
    std::fs::write(t.path().join(MANIFEST), short).unwrap();
    let e = err(t.path());
    assert!(e.contains("64 hex digits"), "{e}");
    // A name twice.
    let twice = format!("{list}{}", list.split("\n\n").next().unwrap());
    std::fs::write(t.path().join(MANIFEST), twice).unwrap();
    let e = err(t.path());
    assert!(e.contains("listed twice"), "{e}");
}

#[test]
fn status_and_doctor_lines_name_the_state() {
    let t = tempfile::tempdir().unwrap();
    let env = env_in(t.path());
    let dir = default_assets_dir(&env);
    let lines = doctor_lines(&env);
    assert!(lines[0].contains("missing") && lines[0].contains("paddock vm setup"));
    let lines = status_lines(&env);
    assert!(
        lines.iter().any(|l| l.contains("paddock vm setup")),
        "{lines:?}"
    );

    std::fs::create_dir_all(&dir).unwrap();
    fake_assets(&dir, false);
    let lines = doctor_lines(&env);
    assert!(lines[0].starts_with("  ok"), "{lines:?}");
    let lines = status_lines(&env);
    assert!(
        lines.iter().any(|l| l.contains("ok       kernel")),
        "{lines:?}"
    );

    std::fs::write(dir.join(KERNEL), "fake kernem").unwrap();
    let lines = doctor_lines(&env);
    assert!(lines[0].contains("does not match"), "{lines:?}");
}
