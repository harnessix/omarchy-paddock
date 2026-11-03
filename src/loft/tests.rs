use super::*;
use std::process::Command;

fn temp_env() -> (tempfile::TempDir, HostEnv) {
    let t = tempfile::tempdir().unwrap();
    let env = HostEnv::from_parts(
        t.path().join("home"),
        "tester".into(),
        1000,
        1000,
        Default::default(),
    );
    (t, env)
}

fn write_cfg(env: &HostEnv, text: &str) {
    let d = env.paddock_config();
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("config.toml"), text).unwrap();
}

fn git(dir: &Path, args: &[&str]) -> String {
    let o = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@localhost",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        o.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8_lossy(&o.stdout).into_owned()
}

/// A repository with one commit.
fn repo(t: &Path) -> PathBuf {
    let repo = t.join("app");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README"), "x\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    std::fs::canonicalize(repo).unwrap()
}

fn new_spec(project: &Path, tmp: &Path) -> SandboxSpec {
    SandboxSpec::new(
        "s-1".into(),
        "fake",
        project,
        tmp.join("home/h"),
        &tmp.join("home"),
        1000,
        1000,
        "tester",
    )
}

#[test]
fn the_list_has_the_built_in_paths_the_added_ones_and_public_marks() {
    let cfg = Cfg::parse(
        "[loft]\npaths = [\"notes/\", \"./ideas.md\", \"../x\", \".git/config\", \"a*\"]\npublic = [\"AGENTS.md\", \"notes\"]\n",
    )
    .unwrap();
    let list = entries(&cfg);
    let find = |p: &str| list.iter().find(|e| e.path == p).cloned();
    assert_eq!(find("CLAUDE.md").unwrap().kind, Kind::Instructions);
    assert_eq!(find("PLAN.md").unwrap().kind, Kind::Design);
    assert_eq!(find("specs/").unwrap().kind, Kind::SpecKit);
    assert!(find("AGENTS.md").unwrap().public);
    assert!(!find("CLAUDE.md").unwrap().public);
    let notes = find("notes/").unwrap();
    assert_eq!((notes.kind, notes.public), (Kind::User, true));
    assert_eq!(find("ideas.md").unwrap().kind, Kind::User);
    assert_eq!(list.iter().filter(|e| e.kind == Kind::User).count(), 2);
    assert_eq!(normalize(".paddock/x"), None);
    // Matching and pathspecs.
    let claude = find("CLAUDE.md").unwrap();
    assert!(claude.matches("CLAUDE.md") && claude.matches("web/CLAUDE.md"));
    assert!(!claude.matches("XCLAUDE.md"));
    assert!(find(".cursor/rules/")
        .unwrap()
        .matches(".cursor/rules/a.mdc"));
    assert_eq!(claude.pathspec("sub"), ":(glob)sub/**/CLAUDE.md");
    assert_eq!(notes.pathspec(""), ":(glob)notes/**");
    // Defaults, and no unknown keys.
    assert_eq!(Cfg::parse("").unwrap(), Cfg::default());
    assert_eq!(Cfg::default().store, Store::External);
    assert!(Cfg::parse("[loft]\ncolor = 1\n").is_err());
    assert_eq!(
        Cfg::parse("[loft]\nstore = \"repo\"\ncommit = \"manual\"\nauto = true\n").unwrap(),
        Cfg {
            store: Store::Repo,
            auto: true,
            commit: Commit::Manual,
            ..Cfg::default()
        }
    );
}

#[test]
fn a_project_file_cannot_set_the_loft() {
    let t = tempfile::tempdir().unwrap();
    let f = t.path().join("paddock.toml");
    std::fs::write(&f, "[loft]\nstore = \"repo\"\n").unwrap();
    let e = crate::manifest::Manifest::read(&f).unwrap_err();
    assert!(format!("{e:#}").contains("loft"), "{e:#}");
}

#[test]
fn the_loft_mounts_its_paths_and_notes_and_keeps_them_out_of_git() {
    let (t, env) = temp_env();
    write_cfg(&env, "[loft]\npaths = [\"notes/\"]\n");
    let repo = repo(t.path());
    let loft = Loft::of(&env, &repo);
    git::init(&loft).unwrap();
    let tree = loft.tree();
    std::fs::write(tree.join("CLAUDE.md"), "# rules\n").unwrap();
    std::fs::create_dir_all(tree.join(".cursor/rules")).unwrap();
    std::fs::create_dir_all(tree.join("notes")).unwrap();
    let wt = t.path().join("wt");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "paddock/x",
            &wt.display().to_string(),
        ],
    );
    let wt = std::fs::canonicalize(wt).unwrap();
    for project in [&repo, &wt] {
        let mut spec = new_spec(project, t.path());
        mount(&env, &mut spec, &repo).unwrap();
        let by_dst = |d: &str| spec.mounts.iter().find(|m| m.dst == project.join(d));
        let m = by_dst("CLAUDE.md").unwrap();
        assert_eq!(m.src.as_deref(), Some(tree.join("CLAUDE.md").as_path()));
        assert_eq!(m.kind, crate::spec::MountKind::Rw);
        assert!(by_dst(".cursor/rules").is_some());
        assert!(by_dst("notes").is_some());
        // Not in the loft: not mounted.
        assert!(by_dst("AGENTS.md").is_none());
        let n = by_dst(".paddock").unwrap();
        assert_eq!(n.src.as_deref(), Some(loft.notes().as_path()));
        let inbox = by_dst(".paddock/inbox").unwrap();
        assert_eq!(
            inbox.src.as_deref(),
            Some(project.join(".paddock/inbox").as_path())
        );
        // The mount points: empty, and git does not show them, also when
        // a run writes into them.
        assert_eq!(
            std::fs::metadata(project.join("CLAUDE.md")).unwrap().len(),
            0
        );
        std::fs::write(project.join("CLAUDE.md"), "x").unwrap();
        std::fs::write(project.join(".cursor/rules/a.mdc"), "x").unwrap();
        std::fs::write(project.join("notes/n.md"), "x").unwrap();
        std::fs::write(project.join(".paddock/plan.md"), "x").unwrap();
        assert_eq!(
            git(project, &["status", "--porcelain"]),
            "",
            "{}",
            project.display()
        );
        std::fs::write(project.join("CLAUDE.md"), "").unwrap();
    }
    let ex = std::fs::read_to_string(repo.join(".git/info/exclude")).unwrap();
    assert_eq!(ex.matches("/CLAUDE.md\n").count(), 1, "{ex}");
    assert!(
        ex.contains("/.paddock\n") && ex.contains("/.cursor/rules/\n"),
        "{ex}"
    );
    // A file of the project's own is not hidden.
    std::fs::write(repo.join("CLAUDE.md"), "mine").unwrap();
    let mut spec = new_spec(&repo, t.path());
    mount(&env, &mut spec, &repo).unwrap();
    assert!(spec.mounts.iter().all(|m| m.dst != repo.join("CLAUDE.md")));
    // The repo store and a public path mount nothing but the notes.
    for cfg in [
        "[loft]\nstore = \"repo\"\n",
        "[loft]\npublic = [\"CLAUDE.md\", \".cursor/rules/\"]\n",
    ] {
        write_cfg(&env, cfg);
        let mut spec = new_spec(&wt, t.path());
        mount(&env, &mut spec, &repo).unwrap();
        let origins: Vec<&str> = spec.mounts.iter().map(|m| m.origin.as_str()).collect();
        assert!(!origins.contains(&"loft"), "{cfg}: {origins:?}");
        assert!(origins.contains(&"loft notes"));
    }
    // Without a loft: only the notes.
    let other = t.path().join("other");
    std::fs::create_dir_all(&other).unwrap();
    let mut spec = new_spec(&other, t.path());
    mount(&env, &mut spec, &other).unwrap();
    let origins: Vec<&str> = spec.mounts.iter().map(|m| m.origin.as_str()).collect();
    assert!(origins.contains(&"loft notes") && !origins.contains(&"loft"));
}

#[test]
fn auto_makes_the_loft_at_the_first_run_and_remove_keeps_it() {
    let (t, env) = temp_env();
    let repo = repo(t.path());
    let loft = Loft::of(&env, &repo);
    let mut spec = new_spec(&repo, t.path());
    mount(&env, &mut spec, &repo).unwrap();
    assert!(!loft.initialized(), "auto is off by default");
    write_cfg(&env, "[loft]\nauto = true\n");
    // The old inbox of the project moves along.
    std::fs::write(repo.join(".paddock/inbox/shot.png"), "png").unwrap();
    let mut spec = new_spec(&repo, t.path());
    mount(&env, &mut spec, &repo).unwrap();
    assert!(loft.active());
    assert!(host::linked(&env, &repo));
    assert_eq!(
        std::fs::read_to_string(loft.notes().join("inbox/shot.png")).unwrap(),
        "png"
    );
    assert!(!loft.notes().join(".gitignore").exists());
    // With the link, the sandbox sees the notes at the link's target.
    assert!(spec
        .mounts
        .iter()
        .any(|m| m.origin == "loft notes" && m.dst == loft.notes()));
    assert_eq!(git(&repo, &["status", "--porcelain"]), "");
    // The host inbox goes to the loft, opened from the loft.
    let d = crate::broker::inbox_for(&env, &repo).unwrap();
    assert_eq!(d.path(), loft.notes().join("inbox"));
    // remove: no link, the loft and its files stay, and auto does not
    // make it again.
    std::fs::write(loft.notes().join("plan.md"), "plan").unwrap();
    host::remove(&env, &repo).unwrap();
    assert!(!repo.join(".paddock").exists());
    assert!(loft.initialized() && !loft.active());
    assert!(loft.notes().join("plan.md").is_file());
    let mut spec = new_spec(&repo, t.path());
    mount(&env, &mut spec, &repo).unwrap();
    assert!(!loft.active() && !host::linked(&env, &repo));
    assert_eq!(git(&repo, &["status", "--porcelain"]), "");
    // init turns it on again, with the link.
    std::fs::remove_dir_all(repo.join(".paddock")).unwrap();
    host::init(&env, &repo).unwrap();
    assert!(loft.active() && host::linked(&env, &repo));
    assert_eq!(git(&repo, &["status", "--porcelain"]), "");
}

#[test]
fn a_foreign_link_is_never_followed() {
    let (t, env) = temp_env();
    let repo = repo(t.path());
    let outside = t.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, repo.join(".paddock")).unwrap();
    let msg = host::init(&env, &repo).unwrap();
    assert!(msg.contains("No link"), "{msg}");
    let mut spec = new_spec(&repo, t.path());
    mount(&env, &mut spec, &repo).unwrap();
    assert!(spec.mounts.iter().all(|m| m.origin != "loft notes"));
    assert!(crate::broker::inbox_for(&env, &repo).is_err());
    assert!(std::fs::read_dir(&outside).unwrap().next().is_none());
}

#[test]
fn the_loft_commits_once_per_run() {
    let (t, env) = temp_env();
    let repo = repo(t.path());
    host::init(&env, &repo).unwrap();
    let loft = Loft::of(&env, &repo);
    let count = || {
        git::run(&loft, &["rev-list", "--count", "HEAD"])
            .unwrap()
            .1
            .trim()
            .parse::<u32>()
            .unwrap()
    };
    assert_eq!(count(), 1);
    std::fs::write(loft.notes().join("plan.md"), "one").unwrap();
    std::fs::write(loft.tree().join("CLAUDE.md"), "rules").unwrap();
    // An agent's .gitignore does not hide files from the history; the
    // inbox stays out.
    std::fs::write(loft.notes().join(".gitignore"), "*\n").unwrap();
    std::fs::create_dir_all(loft.notes().join("inbox")).unwrap();
    std::fs::write(loft.notes().join("inbox/shot.png"), "png").unwrap();
    assert!(git::status(&loft).unwrap().contains("plan.md"));
    end_of_run(&env, &repo, "s-1", "claude");
    assert_eq!(count(), 2);
    let files = git::run(&loft, &["ls-files"]).unwrap().1;
    assert!(
        files.contains(".paddock/plan.md") && files.contains("CLAUDE.md"),
        "{files}"
    );
    assert!(!files.contains("shot.png"), "{files}");
    let log = git::log(&loft, 1).unwrap();
    assert!(log.contains("Run s-1 (claude)"), "{log}");
    // No change: no commit.
    end_of_run(&env, &repo, "s-2", "codex");
    assert_eq!(count(), 2);
    std::fs::write(loft.notes().join("plan.md"), "two").unwrap();
    assert!(git::diff(&loft).unwrap().contains("+two"));
    // commit = "manual": only the command commits.
    write_cfg(&env, "[loft]\ncommit = \"manual\"\n");
    end_of_run(&env, &repo, "s-3", "codex");
    assert_eq!(count(), 2);
    assert!(git::commit(&loft, "by hand").unwrap());
    assert_eq!(count(), 3);
    // A worktree commits to the same loft.
    write_cfg(&env, "");
    let wt = t.path().join("wt");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "w",
            &wt.display().to_string(),
        ],
    );
    std::fs::write(loft.notes().join("plan.md"), "three").unwrap();
    end_of_run(&env, &wt, "s-4", "codex");
    assert_eq!(count(), 4);
}

#[test]
fn the_start_notice_names_private_files_that_git_can_publish() {
    let (t, env) = temp_env();
    let repo = repo(t.path());
    let cfg = Cfg::default();
    let lines = |cfg: &Cfg| push::notice_lines(&env, cfg, &repo).join("\n");
    assert_eq!(lines(&cfg), "");
    std::fs::write(repo.join("CLAUDE.md"), "x").unwrap();
    std::fs::write(repo.join("PLAN.md"), "x").unwrap();
    let l = lines(&cfg);
    assert!(l.contains("agent instruction files (CLAUDE.md)"), "{l}");
    assert!(l.contains("design and plan files (PLAN.md)"), "{l}");
    assert!(l.contains("paddock loft adopt"), "{l}");
    // Ignored: nothing to say. Tracked: again a notice.
    std::fs::write(repo.join(".gitignore"), "CLAUDE.md\nPLAN.md\n").unwrap();
    assert_eq!(lines(&cfg), "");
    git(&repo, &["add", "-f", "CLAUDE.md"]);
    assert!(lines(&cfg).contains("(CLAUDE.md)"));
    // Public: no notice.
    let public = Cfg::parse("[loft]\npublic = [\"CLAUDE.md\"]\n").unwrap();
    assert_eq!(lines(&public), "");
    // A user path, in a subfolder of the project.
    let sub = repo.join("web");
    std::fs::create_dir_all(sub.join("notes")).unwrap();
    std::fs::write(sub.join("notes/a.md"), "x").unwrap();
    let mine = Cfg::parse("[loft]\npaths = [\"notes/\"]\npublic = [\"CLAUDE.md\"]\n").unwrap();
    let l = push::notice_lines(&env, &mine, &sub).join("\n");
    assert!(l.contains("files of your [loft] paths (notes/a.md)"), "{l}");
}

#[test]
fn adopt_moves_private_files_after_the_question() {
    let (t, env) = temp_env();
    let repo = repo(t.path());
    std::fs::write(repo.join("CLAUDE.md"), "rules").unwrap();
    std::fs::write(repo.join("TODO.md"), "todo").unwrap();
    git(&repo, &["add", "CLAUDE.md"]);
    git(&repo, &["commit", "-q", "-m", "rules"]);
    assert!(
        host::adopt(&env, &repo, &mut |_| true).is_err(),
        "no loft yet"
    );
    host::init(&env, &repo).unwrap();
    assert_eq!(
        host::adopt(&env, &repo, &mut |_| false).unwrap(),
        "Nothing moved."
    );
    let mut asked = String::new();
    let msg = host::adopt(&env, &repo, &mut |q| {
        asked = q.into();
        true
    })
    .unwrap();
    assert!(asked.contains("CLAUDE.md, TODO.md"), "{asked}");
    let loft = Loft::of(&env, &repo);
    assert_eq!(
        std::fs::read_to_string(loft.tree().join("CLAUDE.md")).unwrap(),
        "rules"
    );
    assert!(!repo.join("TODO.md").exists());
    assert!(
        msg.contains("git rm -r -q --cached CLAUDE.md TODO.md"),
        "{msg}"
    );
    assert!(msg.contains("filter-repo"), "{msg}");
    assert!(git::log(&loft, 1).unwrap().contains("Adopt"));
    let status = host::status(&env, &repo).unwrap();
    assert!(status.contains("state     on"), "{status}");
    let explain = host::explain(&env, &repo).join("\n");
    assert!(
        explain.contains("CLAUDE.md") && explain.contains("in the loft"),
        "{explain}"
    );
    assert!(explain.contains("git tracks it"), "{explain}");
    assert!(
        host::adopt(&env, &repo, &mut |_| true).is_err(),
        "nothing left"
    );
}

#[test]
fn an_old_spec_kit_store_moves_into_the_loft() {
    let (t, env) = temp_env();
    let repo = repo(t.path());
    let old = env
        .paddock_data()
        .join("speckit")
        .join(crate::kv::shard::project_hash(&repo));
    std::fs::create_dir_all(old.join(".specify")).unwrap();
    std::fs::create_dir_all(old.join("specs/001-a")).unwrap();
    let tree = speckit::store_dir(&env, &repo);
    assert_eq!(tree, Loft::of(&env, &repo).tree());
    assert!(tree.join(".specify").is_dir() && tree.join("specs/001-a").is_dir());
    assert!(!old.exists());
}

#[test]
fn setup_offers_the_loft() {
    struct Yes;
    impl crate::setup::Ask for Yes {
        fn yes_no(&mut self, _: &str, _: bool) -> bool {
            true
        }
        fn line(&mut self, _: &str) -> String {
            String::new()
        }
    }
    let (_t, env) = temp_env();
    write_cfg(&env, "backend = \"bwrap\"\n");
    let mut out = crate::setup::Outcome::default();
    let mut y = Yes;
    let mut ask: Option<&mut dyn crate::setup::Ask> = Some(&mut y);
    host::setup_step(&env, None, &mut ask, &mut out).unwrap();
    assert!(Cfg::load(&env).auto);
    assert_eq!(out.changes.len(), 1);
    host::setup_step(&env, None, &mut ask, &mut out).unwrap();
    assert_eq!(out.changes.len(), 1, "on stays on");
    host::setup_step(&env, Some(false), &mut None, &mut out).unwrap();
    assert!(!Cfg::load(&env).auto);
    let text = std::fs::read_to_string(env.paddock_config().join("config.toml")).unwrap();
    assert!(text.starts_with("backend = \"bwrap\""), "{text}");
}
