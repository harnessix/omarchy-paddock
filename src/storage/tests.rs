use super::*;
use std::cell::RefCell;
use std::os::unix::fs::{symlink, PermissionsExt};

fn setup() -> (tempfile::TempDir, HostEnv) {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let env = HostEnv::from_parts(home, "tester".into(), 1000, 1000, BTreeMap::new());
    (tmp, env)
}

/// A home as paddock creates it, with an origin file.
fn make_home(env: &HostEnv, harness: &str, project: &Path, app: bool) -> PathBuf {
    let h = state::sandbox_home(env, harness, &AuthMode::Shared, project).unwrap();
    write_origin(&h, &Origin::new(project, harness, &AuthMode::Shared, app)).unwrap();
    std::fs::write(h.join(".bash_history"), "ls\n").unwrap();
    h
}

#[derive(Default)]
struct FakeContainers {
    /// (cli, joined args) to output. A missing entry fails.
    replies: BTreeMap<(String, String), String>,
    calls: RefCell<Vec<String>>,
}

impl FakeContainers {
    fn reply(mut self, cli: &str, args: &str, out: &str) -> Self {
        self.replies.insert((cli.into(), args.into()), out.into());
        self
    }
}

impl Containers for FakeContainers {
    fn run(&self, cli: &str, args: &[&str]) -> Result<String> {
        let a = args.join(" ");
        self.calls.borrow_mut().push(format!("{cli} {a}"));
        self.replies
            .get(&(cli.to_string(), a.clone()))
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("{cli} {a}: no reply"))
    }
    fn clis(&self) -> Vec<String> {
        let mut v: Vec<String> = self.replies.keys().map(|k| k.0.clone()).collect();
        v.dedup();
        v
    }
}

fn host<'a>(c: &'a dyn Containers, profiles: &[(&str, bool)]) -> Host<'a> {
    Host {
        containers: c,
        current_tags: ["paddock-base:current".to_string()].into(),
        profiles: Some(profiles.iter().map(|(n, a)| (n.to_string(), *a)).collect()),
    }
}

fn opts() -> GcOptions {
    GcOptions {
        older_than_days: DEFAULT_OLDER_THAN,
        ..Default::default()
    }
}

fn find<'a>(items: &'a [Item], what: &str) -> Option<&'a Item> {
    items.iter().find(|i| i.what.contains(what))
}

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

fn home(path: &str, harness: &str) -> Home {
    Home {
        path: PathBuf::from(path),
        harness: harness.into(),
        auth: "shared".into(),
        project: Some("/w/p".into()),
        origin: OriginCheck::Ok,
        project_exists: Some(true),
        last_used: Some(1_000 * DAY),
        modified: 1_000 * DAY,
        bytes: 10,
        app: false,
        profile: true,
        running: false,
        loses: String::new(),
    }
}

fn inventory(homes: Vec<Home>) -> Inventory {
    Inventory {
        size_kind: SizeKind::Allocated,
        homes,
        images: vec![],
        worktrees: vec![],
        sessions: vec![],
        keyrings: vec![],
        notes: vec![],
        old_caches: vec![],
        any_running: false,
        build_cache: vec![],
    }
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
fn images_sessions_keyrings_and_worktrees() {
    let now = 1_000 * DAY;
    let mut inv = inventory(vec![]);
    let img = |tag: &str, current, in_use| Image {
        cli: "docker".into(),
        id: tag.into(),
        tags: vec![tag.into()],
        bytes: 5,
        unique: None,
        created: String::new(),
        current,
        in_use,
    };
    inv.images = vec![
        img("paddock-base:old", false, false),
        img("paddock-base:used", false, true),
        img("paddock-base:current", true, false),
    ];
    let rec = |id: &str, started, ended, running| SessionRecord {
        path: format!("/s/{id}.json").into(),
        id: id.into(),
        started,
        ended,
        running,
        bytes: 1,
    };
    inv.sessions = vec![
        rec("old", now - 40 * DAY, Some(now - 31 * DAY), false),
        rec("recent", now - 40 * DAY, Some(now - 2 * DAY), false),
        rec("crashed", now - 40 * DAY, None, false),
        rec("running", now - 40 * DAY, None, true),
    ];
    inv.keyrings = vec![
        KeyringFile {
            path: "/k/aaa".into(),
            bytes: 64,
            home_exists: false,
        },
        KeyringFile {
            path: "/k/bbb".into(),
            bytes: 64,
            home_exists: true,
        },
    ];
    let wt = |p: &str, unmerged, dirty| Worktree {
        path: PathBuf::from(p),
        repo: Some("/w/repo".into()),
        branch: Some("paddock/x".into()),
        unmerged,
        dirty,
        bytes: 1,
        running: false,
    };
    let mut unknown = wt("/wt/unknown", None, None);
    unknown.repo = None;
    let mut busy = wt("/wt/busy", Some(0), Some(false));
    busy.running = true;
    inv.worktrees = vec![
        wt("/wt/clean", Some(0), Some(false)),
        wt("/wt/ahead", Some(2), Some(false)),
        wt("/wt/dirty", Some(0), Some(true)),
        unknown,
        busy,
    ];

    let items = plan(&inv, &opts(), now);
    let it = |w: &str| find(&items, w);
    assert!(it("paddock-base:old").unwrap().selected());
    assert!(it("paddock-base:used").is_none());
    assert!(it("paddock-base:current").is_none());
    assert!(it("session record old").unwrap().selected());
    assert!(it("session record recent").is_none());
    assert!(it("session record crashed").is_some());
    assert!(it("session record running").is_none());
    assert!(it("aaa").unwrap().selected());
    assert!(it("bbb").is_none());
    assert_eq!(it("/wt/clean").unwrap().needs, ["--worktrees"]);
    assert_eq!(it("/wt/ahead").unwrap().needs, ["--worktrees", "--force"]);
    assert_eq!(it("/wt/dirty").unwrap().needs, ["--worktrees", "--force"]);
    assert_eq!(it("/wt/unknown").unwrap().needs, ["--worktrees", "--force"]);
    assert!(it("/wt/busy").is_none());

    let o = GcOptions {
        worktrees: true,
        force: true,
        ..opts()
    };
    let items = plan(&inv, &o, now);
    let branch = |w: &str| match &find(&items, w).unwrap().action {
        Action::Worktree { delete_branch, .. } => *delete_branch,
        _ => unreachable!(),
    };
    assert!(branch("/wt/clean"));
    assert!(!branch("/wt/ahead"), "a branch with unmerged commits stays");
}

fn inspect(id: &str, tags: &[&str]) -> serde_json::Value {
    serde_json::json!({ "Id": format!("sha256:{id}"), "RepoTags": tags, "Size": 1000, "Created": "2026-10-01T00:00:00Z" })
}

#[test]
fn images_from_fake_docker_and_podman() {
    let (_t, env) = setup();
    let list = "images -q --no-trunc --filter label=io.paddock.base=1";
    let docker = serde_json::json!([
        inspect("aaa", &["paddock-base:current"]),
        inspect("bbb", &["paddock-base:old"]),
        inspect("ccc", &["paddock-base:used"]),
        // Built FROM paddock-base by the user: it inherits the label.
        inspect("ddd", &["myimage:latest"]),
    ]);
    let podman = serde_json::json!([inspect("eee", &["localhost/paddock-base:old2"])]);
    let c = FakeContainers::default()
        .reply(
            "docker",
            list,
            "sha256:aaa\nsha256:bbb\nsha256:ccc\nsha256:ddd",
        )
        .reply(
            "docker",
            "image inspect sha256:aaa sha256:bbb sha256:ccc sha256:ddd",
            &docker.to_string(),
        )
        .reply("docker", "ps -a -q --no-trunc", "c1")
        .reply(
            "docker",
            "container inspect --format {{.Image}} c1",
            "sha256:ccc",
        )
        .reply("docker", "rmi paddock-base:old", "")
        .reply("podman", list, "eee")
        .reply("podman", "image inspect eee", &podman.to_string())
        .reply("podman", "ps -a -q --no-trunc", "")
        .reply("podman", "rmi localhost/paddock-base:old2", "");
    let h = host(&c, &[]);
    let inv = scan(&env, &h, false);
    let tags: Vec<String> = inv.images.iter().flat_map(|i| i.tags.clone()).collect();
    assert_eq!(
        tags,
        [
            "paddock-base:current",
            "paddock-base:old",
            "paddock-base:used",
            "localhost/paddock-base:old2"
        ]
    );
    let items = plan(
        &inv,
        &GcOptions {
            yes: true,
            ..opts()
        },
        state::now(),
    );
    let removed = apply(&env, &c, &items);
    assert_eq!(removed.len(), 2);
    assert!(removed.iter().all(|r| r.error.is_none()));
    let calls = c.calls.borrow();
    let rmi: Vec<&String> = calls.iter().filter(|c| c.contains(" rmi ")).collect();
    assert_eq!(
        rmi,
        [
            "docker rmi paddock-base:old",
            "podman rmi localhost/paddock-base:old2"
        ]
    );
}

#[test]
fn a_failed_container_list_keeps_every_image() {
    let (_t, env) = setup();
    let list = "images -q --no-trunc --filter label=io.paddock.base=1";
    let c = FakeContainers::default()
        .reply("docker", list, "sha256:bbb")
        .reply(
            "docker",
            "image inspect sha256:bbb",
            &serde_json::json!([inspect("bbb", &["paddock-base:old"])]).to_string(),
        );
    let inv = scan(&env, &host(&c, &[]), false);
    assert!(inv.images.is_empty());
    assert!(inv.notes.iter().any(|n| n.contains("keeping every image")));
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
fn gc_yes_removes_only_the_automatic_tier_and_spares_running_homes() {
    let (_t, env) = setup();
    let project = env.home.join("src/app");
    std::fs::create_dir_all(&project).unwrap();
    let orphan = make_home(&env, "porttest", &project, false);
    let busy = make_home(&env, "porttest", &env.home.join("src/busy"), false);
    let gone = make_home(&env, "claude", &env.home.join("src/removed"), false);
    Running {
        id: "porttest-busy-1".into(),
        harness: "porttest".into(),
        project: env.home.join("src/busy"),
        backend: "bwrap".into(),
        network: "none".into(),
        gui: "off".into(),
        pid: std::process::id(),
        started: state::now(),
        auth: "shared".into(),
        window_pid: None,
        warnings: vec![],
        host_pid: std::process::id(),
    }
    .register(&env)
    .unwrap();

    let c = FakeContainers::default();
    let h = host(&c, &[("claude", false)]);
    let o = GcOptions {
        yes: true,
        ..opts()
    };
    let inv = scan(&env, &h, false);
    let items = plan(&inv, &o, state::now());
    let removed = apply(&env, &c, &items);
    assert_eq!(removed.len(), 1, "{removed:?}");
    assert!(!orphan.exists());
    assert!(busy.exists(), "a running sandbox keeps its home");
    assert!(gone.exists(), "a home of a gone project needs --homes");

    let o = GcOptions {
        yes: true,
        homes: true,
        ..opts()
    };
    let items = plan(&scan(&env, &h, false), &o, state::now());
    apply(&env, &c, &items);
    assert!(!gone.exists());
    assert!(busy.exists());
}

#[test]
fn removal_stays_inside_paddock_and_never_follows_symlinks() {
    let (_t, env) = setup();
    let outside = env.home.join("precious");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("file"), "keep").unwrap();

    // Outside paddock's directories, or with "..".
    assert!(check_removable(&env, &outside).is_err());
    let homes = env.paddock_data().join("homes");
    std::fs::create_dir_all(homes.join("claude/shared")).unwrap();
    assert!(check_removable(&env, &homes.join("claude/shared/../../../../precious")).is_err());
    // The root itself.
    assert!(check_removable(&env, &homes).is_err());

    // A home that is a symlink to a host folder.
    let link = homes.join("claude/shared/link-1");
    symlink(&outside, &link).unwrap();
    assert!(check_removable(&env, &link).is_err());
    assert!(remove_path(&env, &link).is_err());

    // A harness folder that is a symlink leads out of paddock.
    symlink(&outside, homes.join("evil")).unwrap();
    assert!(check_removable(&env, &homes.join("evil/file")).is_err());

    // A real home with a symlink inside: the link goes, its target stays.
    let h = homes.join("claude/shared/real-1");
    std::fs::create_dir_all(h.join("ro")).unwrap();
    symlink(&outside, h.join("to-host")).unwrap();
    std::fs::write(h.join("ro/f"), "x").unwrap();
    std::fs::set_permissions(h.join("ro"), std::fs::Permissions::from_mode(0o555)).unwrap();
    remove_path(&env, &h).unwrap();
    assert!(!h.exists());
    assert_eq!(
        std::fs::read_to_string(outside.join("file")).unwrap(),
        "keep"
    );

    // apply refuses an item outside paddock too.
    let item = Item {
        what: "x".into(),
        tier: Tier::Auto,
        bytes: 0,
        reason: String::new(),
        warning: None,
        needs: vec![],
        action: Action::Path {
            path: outside.clone(),
        },
    };
    let r = apply(&env, &FakeContainers::default(), &[item]);
    assert!(r[0].error.as_ref().unwrap().contains("outside"));
    assert!(outside.join("file").exists());
}

#[test]
fn worktrees_with_commits_need_force_and_keep_their_branch() {
    let (_t, env) = setup();
    let repo = env.home.join("src/repo");
    std::fs::create_dir_all(&repo).unwrap();
    let g = |dir: &Path, args: &[&str]| {
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
            .args(args)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    };
    g(&repo, &["init", "-q", "-b", "main"]);
    g(&repo, &["commit", "-q", "--allow-empty", "-m", "a"]);
    state::record_recent(&env, &repo, "claude").unwrap();
    let clean = crate::build::create_worktree(&env, &repo, None).unwrap();
    let ahead = crate::build::create_worktree(&env, &repo, None).unwrap();
    g(&ahead.path, &["commit", "-q", "--allow-empty", "-m", "b"]);

    let c = FakeContainers::default();
    let h = host(&c, &[]);
    let inv = scan(&env, &h, false);
    let w = |p: &Path| inv.worktrees.iter().find(|w| w.path == p).unwrap();
    assert_eq!(w(&clean.path).unmerged, Some(0));
    assert_eq!(w(&clean.path).dirty, Some(false));
    assert_eq!(w(&ahead.path).unmerged, Some(1));

    let o = GcOptions {
        yes: true,
        worktrees: true,
        ..opts()
    };
    let removed = apply(&env, &c, &plan(&inv, &o, state::now()));
    assert_eq!(removed.len(), 1, "{removed:?}");
    assert!(!clean.path.exists());
    assert!(ahead.path.exists());

    let o = GcOptions { force: true, ..o };
    let removed = apply(&env, &c, &plan(&scan(&env, &h, false), &o, state::now()));
    assert!(removed[0].error.is_none(), "{removed:?}");
    assert!(!ahead.path.exists());
    let branches = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["branch", "--format=%(refname:short)"])
        .output()
        .unwrap();
    let branches = String::from_utf8_lossy(&branches.stdout);
    assert!(
        branches.contains(&ahead.branch),
        "the branch keeps the commit"
    );
    assert!(!branches.contains(&clean.branch));
}

#[test]
fn the_old_mise_cache_goes_unless_a_sandbox_runs() {
    let (_t, env) = setup();
    let shared = env.paddock_cache().join("shared");
    let old = shared.join("mise-installs");
    std::fs::create_dir_all(old.join("jq/1.7.1")).unwrap();
    std::fs::create_dir_all(shared.join("cargo-registry")).unwrap();
    std::fs::create_dir_all(shared.join("mise-tools")).unwrap();
    // Only the exact old folder: never another shared cache.
    assert!(check_removable(&env, &old).is_ok());
    assert!(check_removable(&env, &shared.join("cargo-registry")).is_err());
    assert!(check_removable(&env, &shared.join("mise-tools")).is_err());
    assert!(check_removable(&env, &old.join("jq")).is_err());

    let c = FakeContainers::default();
    let h = host(&c, &[]);
    let mut inv = scan(&env, &h, false);
    assert_eq!(inv.old_caches.len(), 1);
    inv.any_running = true;
    assert!(find(&plan(&inv, &opts(), state::now()), "old cache").is_none());
    inv.any_running = false;
    let items = plan(&inv, &opts(), state::now());
    assert!(find(&items, "old cache").unwrap().selected());
    apply(&env, &c, &items);
    assert!(!old.exists());
    assert!(shared.join("cargo-registry").exists());
    assert!(shared.join("mise-tools").exists());
}

#[test]
fn docker_sizes_parse() {
    assert_eq!(parse_size("1.509GB"), Some(1_509_000_000));
    assert_eq!(parse_size("807.8MB"), Some(807_800_000));
    assert_eq!(parse_size("4.096kB"), Some(4_096));
    assert_eq!(parse_size("98B"), Some(98));
    assert_eq!(parse_size("0B"), Some(0));
    assert_eq!(parse_size("lots"), None);
}

#[test]
fn image_items_count_only_the_unique_part() {
    let mut inv = inventory(vec![]);
    inv.images = vec![Image {
        cli: "docker".into(),
        id: "abc".into(),
        tags: vec!["paddock-base:old".into()],
        bytes: 2_000,
        unique: Some(300),
        created: String::new(),
        current: false,
        in_use: false,
    }];
    let items = plan(&inv, &opts(), 1_000 * DAY);
    let img = items.iter().find(|i| i.what.starts_with("image")).unwrap();
    assert_eq!(img.bytes, 300);
}

#[test]
fn build_cache_keeps_other_builds() {
    let du = [
        r#"{"Description":"mount / from exec /bin/sh -c : paddock-base core && pacman -Syu","ID":"aaa111","Reclaimable":true,"Shared":false,"Size":"807.8MB"}"#,
        r#"{"Description":"mount / from exec /bin/sh -c pacman -Syu --noconfirm --needed base-devel git curl ca-certificates  && rm","ID":"bbb222","Reclaimable":true,"Shared":true,"Size":"568MB"}"#,
        r#"{"Description":"mount / from exec /bin/sh -c useradd someone","ID":"ccc333","Reclaimable":true,"Shared":false,"Size":"65kB"}"#,
        r#"{"Description":"[1/2] RUN : paddock-base desktop && x","ID":"ddd;rm","Reclaimable":true,"Shared":false,"Size":"1MB"}"#,
    ]
    .join("\n");
    let c = FakeContainers::default().reply("docker", "buildx du --format json", &du);
    let host = host(&c, &[]);
    let recs = scan_build_cache(&host, "docker");
    let ids: Vec<&str> = recs.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, vec!["aaa111", "bbb222"]);
    // A shared record frees nothing by itself.
    assert_eq!(recs[0].bytes, 807_800_000);
    assert_eq!(recs[1].bytes, 0);
    assert!(scan_build_cache(&host, "podman").is_empty());
}

#[test]
fn podman_df_table_parses() {
    let t = "Images space usage:\n\nREPOSITORY                   TAG           IMAGE ID      CREATED     SIZE        SHARED SIZE  UNIQUE SIZE  CONTAINERS\ndocker.io/library/archlinux  latest        bf2533a05fd2  2 days      410.8MB     410.7MB      8.78kB       0\nlocalhost/paddock-base       ba4127c06f76  8f76cb6270bc  22 hours    1.597GB     1.004GB      593.1MB      0\n\nContainers space usage:\n";
    let v = parse_podman_df(t);
    assert_eq!(
        v,
        vec![
            ("bf2533a05fd2".to_string(), 8_780),
            ("8f76cb6270bc".to_string(), 593_100_000)
        ]
    );
}
