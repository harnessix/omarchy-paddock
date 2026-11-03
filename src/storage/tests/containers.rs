use super::*;

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
