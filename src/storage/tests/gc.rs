use super::*;

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
        parent: None,
        spawn_id: None,
        logins: Vec::new(),
        unit: None,
        kv_project: None,
        pasture: None,
        credentials: Vec::new(),
        gateway: Vec::new(),
        agent: Vec::new(),
        secret_files: Vec::new(),
        kept_logins: Vec::new(),
        watched: false,
        relay: false,
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
    // The worktree state comes from a sandboxed `git status`. A host that
    // cannot run one, such as a CI container without user namespaces,
    // reports it as unknown instead of guessing.
    assert!(
        matches!(w(&clean.path).dirty, Some(false) | None),
        "{:?}",
        w(&clean.path).dirty
    );
    assert_eq!(w(&ahead.path).unmerged, Some(1));

    let o = GcOptions {
        yes: true,
        worktrees: true,
        ..opts()
    };
    let removed = apply(&env, &c, &plan(&inv, &o, state::now()));
    if w(&clean.path).dirty.is_some() {
        assert_eq!(removed.len(), 1, "{removed:?}");
        assert!(!clean.path.exists());
        assert!(ahead.path.exists());
    } else {
        // An unknown state counts as a risk, so it needs --force as well.
        assert_eq!(removed.len(), 0, "{removed:?}");
    }

    let o = GcOptions { force: true, ..o };
    let removed = apply(&env, &c, &plan(&scan(&env, &h, false), &o, state::now()));
    assert!(removed.iter().all(|r| r.error.is_none()), "{removed:?}");
    assert!(!ahead.path.exists());
    assert!(!clean.path.exists());
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
fn the_old_shared_package_caches_go_and_so_do_layers_of_gone_projects() {
    let (t, env) = setup();
    let shared = env.paddock_cache().join("shared");
    std::fs::create_dir_all(shared.join("cargo-registry/registry")).unwrap();
    std::fs::create_dir_all(shared.join("npm")).unwrap();
    std::fs::create_dir_all(shared.join("mise-tools")).unwrap();
    assert!(check_removable(&env, &shared.join("npm")).is_ok());
    assert!(check_removable(&env, &shared.join("mise-tools")).is_err());
    let gone = t.path().join("gone");
    std::fs::create_dir_all(&gone).unwrap();
    layers::mount_with(&env, &gone, "npm", "/h/.npm".into(), "t", true).unwrap();
    std::fs::remove_dir(&gone).unwrap();

    let c = FakeContainers::default();
    let h = host(&c, &[]);
    let inv = scan(&env, &h, false);
    assert_eq!(inv.old_caches.len(), 2);
    let items = plan(&inv, &opts(), state::now());
    let l = find(&items, "cache layers").expect("the layers of a gone project go");
    assert_eq!(l.tier, Tier::Auto);
    let r = render_du(&env, &inv, state::now());
    assert!(r.contains("Shared cache layer:"), "{r}");
    assert!(r.contains("Project cache layers:"), "{r}");
    apply(&env, &c, &items);
    assert!(!shared.join("npm").exists());
    assert!(!layers::project_dir(&env, &gone).exists());
    assert!(layers::lower(&env, "npm").is_dir());
}

#[test]
fn the_old_mise_cache_goes_unless_a_sandbox_runs() {
    let (_t, env) = setup();
    let shared = env.paddock_cache().join("shared");
    let old = shared.join("mise-installs");
    std::fs::create_dir_all(old.join("jq/1.7.1")).unwrap();
    std::fs::create_dir_all(shared.join("mise-tools")).unwrap();
    // Only the exact old folder: never another shared cache.
    assert!(check_removable(&env, &old).is_ok());
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
    assert!(shared.join("mise-tools").exists());
    assert!(shared.join("mise-tools").exists());
}

#[test]
fn gc_removes_snapshots_of_gone_projects_and_old_ones_with_the_flag() {
    let now = 1_000 * DAY;
    let snap = |path: &str, exists: bool, started: u64, trash: bool| SnapshotDir {
        path: path.into(),
        project: (!trash).then(|| PathBuf::from("/src/app")),
        harness: "fake".into(),
        started,
        bytes: 10,
        project_exists: exists,
        trash,
    };
    let mut inv = inventory(vec![]);
    inv.snapshots = vec![
        snap("/d/snapshots/app-1/gone", false, now, false),
        snap("/d/snapshots/app-1/new", true, now, false),
        snap("/d/snapshots/app-1/old", true, 0, false),
        snap("/d/undo-trash/1-old", true, 0, true),
    ];
    let items = plan(&inv, &opts(), now);
    let gone = find(&items, "snapshot gone").unwrap();
    assert!(gone.selected());
    assert_eq!(gone.tier, Tier::Auto);
    assert!(find(&items, "snapshot new").is_none());
    assert_eq!(
        find(&items, "snapshot old").unwrap().needs,
        vec!["--snapshots"]
    );
    assert!(!find(&items, "undo trash").unwrap().selected());
    let o = GcOptions {
        snapshots: true,
        ..opts()
    };
    let items = plan(&inv, &o, now);
    assert!(find(&items, "snapshot old").unwrap().selected());
    assert!(find(&items, "undo trash").unwrap().selected());
}

#[test]
fn gc_clears_pure_caches_in_the_automatic_tier_and_never_a_login() {
    let (_t, env) = setup();
    let project = env.home.join("src/app");
    std::fs::create_dir_all(&project).unwrap();
    let h = make_home(&env, "fake", &project, false);
    let keep = [
        ".config/App/Local Storage/leveldb/x",
        ".config/App/Cookies",
        ".fake/auth.json",
        ".fake/sessions/s.jsonl",
        ".npm/_cacache/x",
        ".codex/packages/runtime.bin",
    ];
    let clear = [
        ".config/App/Cache/data_0",
        ".config/App/GPUCache/data_1",
        ".cache/mesa_shader_cache/index",
        ".fake/cache/blob",
    ];
    for f in keep.iter().chain(&clear) {
        std::fs::create_dir_all(h.join(f).parent().unwrap()).unwrap();
        std::fs::write(h.join(f), "x").unwrap();
    }
    // A symlink that the sandbox planted: gc never follows it.
    let outside = env.home.join("outside/Cache");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("keep"), "x").unwrap();
    symlink(env.home.join("outside"), h.join(".config/Linked")).unwrap();

    let fake: crate::profile::Profile = toml::from_str(
        "name = \"fake\"\n[state]\nauth = [\".fake/auth.json\"]\nscratch = [\".fake/cache\"]\n",
    )
    .unwrap();
    let c = FakeContainers::default();
    let mut hst = host(&c, &[("fake", false)]);
    hst.scratch
        .insert("fake".into(), caches::scratch(Some(&fake)).0);
    let mut inv = scan(&env, &hst, true);
    let scratch = inv
        .classes
        .iter()
        .find(|c| c.class == caches::Class::Scratch)
        .unwrap();
    assert_eq!(scratch.folders, 4, "{:?}", inv.classes);
    assert!(render_du(&env, &inv, state::now()).contains("pure caches"));

    // Off in the user configuration: no item.
    inv.tmpfs_caches = false;
    assert!(find(&plan(&inv, &opts(), state::now()), "pure caches").is_none());
    inv.tmpfs_caches = true;
    let items = plan(&inv, &opts(), state::now());
    let item = find(&items, "pure caches").unwrap();
    assert_eq!(item.tier, Tier::Auto);
    assert!(item.selected());
    let removed = apply(&env, &c, &items);
    assert!(removed.iter().all(|r| r.error.is_none()), "{removed:?}");
    for f in clear {
        assert!(!h.join(f).exists(), "{f}");
    }
    for f in keep {
        assert!(h.join(f).exists(), "{f}");
    }
    assert!(outside.join("keep").exists());

    // A running sandbox keeps its caches.
    std::fs::create_dir_all(h.join(".cache/fontconfig")).unwrap();
    let mut inv = scan(&env, &hst, false);
    inv.homes[0].running = true;
    assert!(find(&plan(&inv, &opts(), state::now()), "pure caches").is_none());
}
