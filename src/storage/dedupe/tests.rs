use super::*;
use std::os::unix::fs::symlink;

fn data(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
    (0..len)
        .map(|_| {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (x >> 33) as u8
        })
        .collect()
}

fn entry(p: &Path) -> Entry {
    Entry {
        path: p.to_path_buf(),
        key: Key::of(&std::fs::symlink_metadata(p).unwrap()),
    }
}

/// A temporary folder on a file system that can share data, if the machine
/// has one: `PADDOCK_TEST_DEDUPE_DIR`, else the cargo target folder.
fn reflink_tmp() -> Option<tempfile::TempDir> {
    let mut c: Vec<PathBuf> = std::env::var_os("PADDOCK_TEST_DEDUPE_DIR")
        .map(PathBuf::from)
        .into_iter()
        .collect();
    c.push(Path::new(env!("CARGO_MANIFEST_DIR")).join("target"));
    c.push(std::env::temp_dir());
    for d in c {
        if matches!(fs_kind(&d), Some((_, true))) {
            if let Ok(t) = tempfile::Builder::new()
                .prefix("dedupe-test")
                .tempdir_in(&d)
            {
                return Some(t);
            }
        }
    }
    None
}

#[test]
fn chunks_cover_the_file_in_pieces_the_kernel_accepts() {
    assert!(chunks(0, CHUNK).is_empty());
    assert_eq!(chunks(5000, CHUNK), vec![(0, 5000)]);
    assert_eq!(chunks(CHUNK, CHUNK), vec![(0, CHUNK)]);
    assert_eq!(
        chunks(2 * CHUNK + 7, CHUNK),
        vec![(0, CHUNK), (CHUNK, CHUNK), (2 * CHUNK, 7)]
    );
}

#[test]
fn walk_skips_symlinks_small_files_and_other_kinds() {
    let t = tempfile::tempdir().unwrap();
    let r = t.path();
    std::fs::create_dir_all(r.join("a/b")).unwrap();
    std::fs::write(r.join("a/big"), data(8192, 1)).unwrap();
    std::fs::write(r.join("a/b/big2"), data(4096, 2)).unwrap();
    std::fs::write(r.join("a/small"), data(4095, 3)).unwrap();
    symlink(r.join("a/big"), r.join("a/link")).unwrap();
    // A symlinked folder outside the root: never entered.
    let out = tempfile::tempdir().unwrap();
    std::fs::write(out.path().join("outside"), data(8192, 1)).unwrap();
    symlink(out.path(), r.join("a/away")).unwrap();
    let (files, complete) = walk(r, None);
    assert!(complete);
    let mut names: Vec<_> = files
        .iter()
        .map(|e| e.path.strip_prefix(r).unwrap().display().to_string())
        .collect();
    names.sort();
    assert_eq!(names, vec!["a/b/big2", "a/big"]);
    // A root that is a symlink is not walked.
    symlink(r, out.path().join("root-link")).unwrap();
    assert!(walk(&out.path().join("root-link"), None).0.is_empty());
}

#[test]
fn size_groups_keep_one_path_per_inode_and_drop_single_sizes() {
    let t = tempfile::tempdir().unwrap();
    let r = t.path();
    std::fs::write(r.join("a"), data(5000, 1)).unwrap();
    std::fs::hard_link(r.join("a"), r.join("a-link")).unwrap();
    std::fs::write(r.join("b"), data(5000, 2)).unwrap();
    std::fs::write(r.join("c"), data(6000, 3)).unwrap();
    let files: Vec<Entry> = ["a", "a-link", "b", "c"]
        .iter()
        .map(|n| entry(&r.join(n)))
        .collect();
    let g = size_groups(&files);
    assert_eq!(g.len(), 1);
    assert_eq!(g[0].len(), 2);
    assert!(g[0].iter().all(|e| e.key.size == 5000));
}

#[test]
fn hash_groups_split_by_content_and_use_the_index() {
    let t = tempfile::tempdir().unwrap();
    let r = t.path();
    std::fs::write(r.join("a"), data(5000, 1)).unwrap();
    std::fs::write(r.join("b"), data(5000, 1)).unwrap();
    std::fs::write(r.join("c"), data(5000, 2)).unwrap();
    let files: Vec<Entry> = ["a", "b", "c"].iter().map(|n| entry(&r.join(n))).collect();
    let mut ix = Index::default();
    let (g, changed) = hash_groups(size_groups(&files), &mut ix, None);
    assert_eq!(changed, 0);
    assert_eq!(g.len(), 1);
    let names: Vec<_> = g[0].iter().map(|e| e.path.clone()).collect();
    assert_eq!(names, vec![r.join("a"), r.join("b")]);
    assert_eq!(ix.len(), 3);

    // The index answers for known keys: c gets the hash of a without a read.
    let ha = ix.get(&files[0].key).unwrap();
    ix.put(files[2].key, ha);
    let (g, _) = hash_groups(size_groups(&files), &mut ix, None);
    assert_eq!(g[0].len(), 3);

    // Save and load; with prune, keys that the walk did not see go away.
    let path = r.join("ix/index");
    ix.save(&path, false).unwrap();
    let mut back = Index::load(&path);
    assert_eq!(back.len(), 3);
    assert_eq!(back.get(&files[0].key), Some(ha));
    back.save(&path, true).unwrap();
    assert_eq!(Index::load(&path).len(), 1);
}

#[test]
fn a_file_that_changed_after_the_walk_is_not_hashed() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("f");
    std::fs::write(&p, data(5000, 1)).unwrap();
    let e = entry(&p);
    assert!(hash_file(&p, &e.key, None).is_some());
    let mut k = e.key;
    k.mtime += 1;
    assert!(hash_file(&p, &k, None).is_none());
    // A symlink is never opened.
    symlink(&p, t.path().join("l")).unwrap();
    assert!(hash_file(&t.path().join("l"), &e.key, None).is_none());
}

#[test]
fn the_storage_table_turns_dedupe_off() {
    assert!(enabled_in(""));
    assert!(enabled_in("[storage]\n"));
    assert!(enabled_in("[storage]\ndedupe = true\n"));
    assert!(!enabled_in("[storage]\ndedupe = false\n"));
}

#[test]
fn only_folders_inside_the_homes_folder_count_as_homes() {
    let t = tempfile::tempdir().unwrap();
    let env = HostEnv::from_parts(
        t.path().join("home"),
        "tester".into(),
        1000,
        1000,
        Default::default(),
    );
    let root = homes_root(&env);
    std::fs::create_dir_all(root.join("fake/shared/p1")).unwrap();
    std::fs::create_dir_all(t.path().join("elsewhere")).unwrap();
    symlink(t.path().join("elsewhere"), root.join("fake/shared/p2")).unwrap();
    assert!(check_home(&env, &root.join("fake/shared/p1")).is_ok());
    assert!(check_home(&env, &root.join("fake/shared/p2")).is_err());
    assert!(check_home(&env, &root.join("fake/shared/../../x")).is_err());
    assert!(check_home(&env, &root).is_err());
    assert!(check_home(&env, &t.path().join("elsewhere")).is_err());
}

#[test]
fn a_file_system_without_support_is_skipped_quietly() {
    let t = tempfile::tempdir().unwrap();
    let r = t.path();
    match fs_kind(r) {
        Some((_, false)) => {}
        k => {
            eprintln!("skipped: the temp folder can share data ({k:?})");
            return;
        }
    }
    std::fs::create_dir_all(r.join("homes/a")).unwrap();
    std::fs::create_dir_all(r.join("homes/b")).unwrap();
    std::fs::write(r.join("homes/a/f"), data(8192, 1)).unwrap();
    std::fs::write(r.join("homes/b/f"), data(8192, 1)).unwrap();
    let rep = Run {
        root: &r.join("homes"),
        only: None,
        apply: true,
        index: &r.join("cache/index"),
        deadline: None,
    }
    .go();
    assert!(rep.unsupported.is_some());
    assert_eq!(rep.deduped, 0);
    assert!(render(&rep, true).contains("cannot share file data"));
    // The ioctl itself reports it as unsupported, not as an error.
    let a = File::open(r.join("homes/a/f")).unwrap();
    let b = File::open(r.join("homes/b/f")).unwrap();
    assert!(matches!(
        dedupe_files(&a, &b, 8192, None),
        Err(PairError::Unsupported)
    ));
}

#[test]
fn identical_folders_share_their_data_and_stay_independent() {
    let Some(t) = reflink_tmp() else {
        eprintln!("skipped: no btrfs, xfs or bcachefs temp folder on this machine");
        return;
    };
    let r = t.path();
    let homes = r.join("homes");
    let big = data((CHUNK + 300_000) as usize, 7);
    let mid = data(64 * 1024, 8);
    for h in ["p1", "p2"] {
        let d = homes.join("fake/shared").join(h).join(".codex/packages");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("runtime.bin"), &big).unwrap();
        std::fs::write(d.join("lib.so"), &mid).unwrap();
        std::fs::write(d.join("small"), b"tiny").unwrap();
    }
    // Same size, other bytes: never shared.
    std::fs::write(homes.join("fake/shared/p2/other"), data(64 * 1024, 9)).unwrap();
    std::fs::write(homes.join("fake/shared/p1/other"), data(64 * 1024, 10)).unwrap();
    let index = r.join("cache/index");
    let run = |only: Option<&Path>, apply: bool| {
        Run {
            root: &homes,
            only,
            apply,
            index: &index,
            deadline: None,
        }
        .go()
    };
    let expect = big.len() as u64 + mid.len() as u64;

    let dry = run(None, false);
    assert_eq!(dry.unsupported, None);
    assert_eq!(dry.candidates, 2, "{dry:?}");
    assert_eq!(dry.candidate_bytes, expect);
    assert_eq!(dry.deduped_bytes, 0);
    assert!(index.exists());

    let p2 = homes.join("fake/shared/p2");
    let rep = run(Some(&p2), true);
    assert_eq!(rep.deduped, 2, "{rep:?}");
    assert_eq!(rep.deduped_bytes, expect);
    assert!(render(&rep, true).contains("got back"));

    let f1 = homes.join("fake/shared/p1/.codex/packages/runtime.bin");
    let f2 = p2.join(".codex/packages/runtime.bin");
    assert_eq!(std::fs::read(&f1).unwrap(), big);
    assert_eq!(std::fs::read(&f2).unwrap(), big);
    assert!(already_shared(
        &File::open(&f1).unwrap(),
        &File::open(&f2).unwrap()
    ));

    // A second run finds nothing new.
    let again = run(None, true);
    assert_eq!(again.deduped, 0, "{again:?}");
    assert_eq!(again.already_shared, 2);

    // A write to one copy leaves the other unchanged.
    {
        use std::io::{Seek, SeekFrom};
        let mut f = std::fs::OpenOptions::new().write(true).open(&f2).unwrap();
        f.seek(SeekFrom::Start(100)).unwrap();
        f.write_all(b"changed").unwrap();
    }
    assert_eq!(std::fs::read(&f1).unwrap(), big);
    let mut want = big.clone();
    want[100..107].copy_from_slice(b"changed");
    assert_eq!(std::fs::read(&f2).unwrap(), want);
}
