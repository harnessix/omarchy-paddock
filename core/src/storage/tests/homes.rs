use super::*;

#[test]
fn origin_round_trip_and_hint_checks() {
    let (_t, env) = setup();
    let project = env.home.join("src/app");
    std::fs::create_dir_all(&project).unwrap();
    let h = make_home(&env, "claude", &project, false);
    let o = read_origin(&h).unwrap();
    assert_eq!(o.project, project);
    assert_eq!(o.auth, "shared");
    assert_eq!(
        std::fs::metadata(h.join(ORIGIN_FILE))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );

    // The sandbox rewrites its origin to claim another project.
    let other = env.home.join("src/secret");
    std::fs::create_dir_all(&other).unwrap();
    write_origin(&h, &Origin::new(&other, "claude", &AuthMode::Shared, false)).unwrap();
    let c = FakeContainers::default();
    let inv = scan(&env, &host(&c, &[("claude", false)]), false);
    let home = inv.homes.iter().find(|x| x.path == h).unwrap();
    assert_eq!(home.origin, OriginCheck::Mismatch);
    assert_eq!(home.project, None);
}

#[test]
fn origin_is_never_read_or_written_through_a_link_or_pipe() {
    let (_t, env) = setup();
    let h = env.paddock_data().join("homes/claude/shared/x-1");
    std::fs::create_dir_all(&h).unwrap();
    let target = env.home.join("host-file");
    std::fs::write(&target, "host data").unwrap();
    let o = Origin::new(Path::new("/w/p"), "claude", &AuthMode::Shared, false);
    std::fs::write(&target, serde_json::to_string(&o).unwrap()).unwrap();
    symlink(&target, h.join(ORIGIN_FILE)).unwrap();
    assert_eq!(read_origin(&h), None, "a symlinked origin is not read");

    // A planted link at the temporary name, too.
    symlink(
        &target,
        h.join(format!("{ORIGIN_FILE}.{}.tmp", std::process::id())),
    )
    .unwrap();
    write_origin(&h, &o).unwrap();
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        serde_json::to_string(&o).unwrap(),
        "the host file is unchanged"
    );
    assert!(!std::fs::symlink_metadata(h.join(ORIGIN_FILE))
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(read_origin(&h), Some(o));

    // A pipe does not block the reader.
    std::fs::remove_file(h.join(ORIGIN_FILE)).unwrap();
    let c = std::ffi::CString::new(h.join(ORIGIN_FILE).as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    assert_eq!(read_origin(&h), None);
}

#[test]
fn escape_test_homes_go_in_the_automatic_tier_once_their_project_is_gone() {
    let now = 1_000 * DAY;
    let mut gone = home("/d/homes/doctor/shared/gone-1", "doctor");
    gone.project_exists = Some(false);
    // A run that is still setting up: its scratch project exists.
    let setup = home("/d/homes/doctor/shared/setup-1", "doctor");
    let mut unknown_old = home("/d/homes/doctor/shared/unknown-1", "doctor");
    unknown_old.project = None;
    unknown_old.project_exists = None;
    unknown_old.last_used = None;
    unknown_old.modified = now - 2 * DAY;
    let mut unknown_new = unknown_old.clone();
    unknown_new.path = "/d/homes/doctor/shared/unknown-2".into();
    unknown_new.modified = now - 60;
    let mut running = gone.clone();
    running.path = "/d/homes/doctor/shared/run-1".into();
    running.running = true;
    let inv = inventory(vec![gone, setup, unknown_old, unknown_new, running]);
    let items = plan(&inv, &opts(), now);
    let g = find(&items, "gone-1").unwrap();
    assert_eq!(g.tier, Tier::Auto);
    assert!(g.selected());
    assert_eq!(find(&items, "unknown-1").unwrap().tier, Tier::Auto);
    assert!(find(&items, "setup-1").is_none());
    assert!(find(&items, "unknown-2").is_none());
    assert!(find(&items, "run-1").is_none());
}

#[test]
fn doctor_removes_escape_test_homes_whose_project_is_gone() {
    let (_t, env) = setup();
    let live = env.home.join(".cache/paddock/doctor-runs/1-aa/project");
    let ended = env.home.join(".cache/paddock/doctor-runs/2-bb/project");
    std::fs::create_dir_all(&live).unwrap();
    let live_home = make_home(&env, DOCTOR_HARNESS, &live, false);
    let ended_home = make_home(&env, DOCTOR_HARNESS, &ended, false);
    // Another harness with a gone project needs consent: not touched here.
    let other = make_home(&env, "claude", &ended, false);
    // A sandbox rewrote its origin to name a gone project: not trusted.
    let forged = state::sandbox_home(&env, DOCTOR_HARNESS, &AuthMode::Shared, &live)
        .unwrap()
        .with_file_name("forged");
    std::fs::create_dir_all(&forged).unwrap();
    write_origin(
        &forged,
        &Origin::new(&ended, DOCTOR_HARNESS, &AuthMode::Shared, false),
    )
    .unwrap();
    assert_eq!(remove_doctor_homes(&env), vec![ended_home.clone()]);
    assert!(!ended_home.exists());
    assert!(live_home.exists());
    assert!(other.exists());
    assert!(forged.exists());
}

#[test]
fn home_tiers() {
    let now = 1_000 * DAY;
    let mut gone = home("/d/homes/claude/shared/gone-1", "claude");
    gone.project_exists = Some(false);
    let mut old = home("/d/homes/claude/shared/old-1", "claude");
    old.last_used = Some(now - 100 * DAY);
    old.modified = now - 100 * DAY;
    // The sandbox wrote an old time into its origin, but touched a file today.
    let mut touched = home("/d/homes/claude/shared/touched-1", "claude");
    touched.last_used = Some(now - 300 * DAY);
    let mut app_old = home("/d/homes/claude-desktop/shared/app-1", "claude-desktop");
    app_old.app = true;
    app_old.last_used = Some(now - 200 * DAY);
    app_old.modified = now - 200 * DAY;
    let mut orphan = home("/d/homes/porttest/shared/o-1", "porttest");
    orphan.profile = false;
    let mut orphan_app = home("/d/homes/oldapp/shared/o-2", "oldapp");
    orphan_app.profile = false;
    orphan_app.app = true;
    let mut running = home("/d/homes/claude/shared/run-1", "claude");
    running.project_exists = Some(false);
    running.running = true;
    let fresh = home("/d/homes/claude/shared/fresh-1", "claude");
    let inv = inventory(vec![
        gone, old, touched, app_old, orphan, orphan_app, running, fresh,
    ]);

    let items = plan(&inv, &opts(), now);
    let it = |w: &str| find(&items, w);
    assert!(it("fresh-1").is_none());
    assert!(it("touched-1").is_none());
    assert!(it("run-1").is_none(), "a running sandbox is never listed");
    assert_eq!(it("o-1").unwrap().tier, Tier::Auto);
    assert!(it("o-1").unwrap().selected());
    let oa = it("o-2").unwrap();
    assert_eq!(oa.tier, Tier::Consent);
    assert_eq!(oa.needs, ["--homes", "--include-app-logins"]);
    assert_eq!(it("gone-1").unwrap().needs, ["--homes"]);
    assert_eq!(it("old-1").unwrap().needs, ["--homes"]);
    let a = it("app-1").unwrap();
    assert_eq!(a.needs, ["--homes", "--include-app-logins"]);
    assert!(a.warning.as_ref().unwrap().contains("login"));

    let with_homes = GcOptions {
        homes: true,
        ..opts()
    };
    let items = plan(&inv, &with_homes, now);
    assert!(find(&items, "gone-1").unwrap().selected());
    assert!(find(&items, "old-1").unwrap().selected());
    assert_eq!(
        find(&items, "app-1").unwrap().needs,
        ["--include-app-logins"]
    );

    let all = GcOptions {
        homes: true,
        include_app_logins: true,
        ..opts()
    };
    assert!(plan(&inv, &all, now).iter().all(|i| i.selected()));

    // --older-than moves the age limit.
    let strict = GcOptions {
        homes: true,
        older_than_days: 150,
        ..opts()
    };
    assert!(find(&plan(&inv, &strict, now), "old-1").is_none());
}

#[test]
fn scan_reads_homes_sessions_and_keyrings() {
    let (_t, env) = setup();
    let project = env.home.join("src/app");
    std::fs::create_dir_all(&project).unwrap();
    let live = make_home(&env, "claude", &project, false);
    // Big enough for its own data blocks: btrfs keeps small files inline in
    // its metadata, and `btrfs filesystem du` reports 0 exclusive bytes then.
    // Incompressible (btrfs may compress) and synced (unflushed data counts 0).
    let mut x: u32 = 1;
    let data: Vec<u8> = (0..256 * 1024)
        .map(|_| {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (x >> 24) as u8
        })
        .collect();
    {
        use std::io::Write;
        let mut f = std::fs::File::create(live.join("data.bin")).unwrap();
        f.write_all(&data).unwrap();
        f.sync_all().unwrap();
    }
    let gone_project = env.home.join("src/removed");
    let gone = make_home(&env, "claude", &gone_project, false);
    let legacy = env
        .paddock_data()
        .join("homes/codex/isolated-work/old-0123456789");
    std::fs::create_dir_all(&legacy).unwrap();
    let orphan = make_home(&env, "porttest", &project, false);
    // A keyring password for a live home, and one for a deleted home.
    crate::keyring::password(&env, &live).unwrap();
    crate::keyring::password(&env, Path::new("/nowhere/home")).unwrap();

    let c = FakeContainers::default();
    let inv = scan(
        &env,
        &host(&c, &[("claude", false), ("codex", false)]),
        true,
    );
    let by = |p: &Path| inv.homes.iter().find(|h| h.path == p).unwrap();
    assert_eq!(by(&live).project_exists, Some(true));
    assert!(by(&live).last_used.is_some());
    assert_eq!(by(&gone).project_exists, Some(false));
    assert_eq!(by(&legacy).origin, OriginCheck::Missing);
    assert_eq!(by(&legacy).auth, "isolated:work");
    assert_eq!(by(&legacy).last_used, None);
    assert!(by(&legacy).loses.contains("auth/codex/work"));
    assert!(!by(&orphan).profile);
    assert!(by(&live).bytes > 0);
    assert_eq!(inv.keyrings.iter().filter(|k| k.home_exists).count(), 1);
    assert_eq!(inv.keyrings.iter().filter(|k| !k.home_exists).count(), 1);

    let text = render_du(&env, &inv, state::now());
    assert!(text.contains("~/src/removed (gone)"));
    assert!(text.contains("unknown (old-0123456789), last modified"));
}

#[test]
fn clock_shows_the_time_at_an_offset() {
    // 2026-11-02 14:05:30 UTC.
    let t = 1_793_628_330;
    assert_eq!(clock(t, 0), "2026-11-02 14:05");
    assert_eq!(clock(t, 3600), "2026-11-02 15:05");
    assert_eq!(clock(t, 10 * 3600), "2026-11-03 00:05");
    assert_eq!(clock(t, -15 * 3600), "2026-11-01 23:05");
    // The local form has the same shape, whatever the zone.
    let l = local_time(t);
    assert_eq!(l.len(), 16, "{l}");
    assert_eq!(&l[4..5], "-");
    assert_eq!(&l[10..11], " ");
    assert_eq!(&l[13..14], ":");
}
