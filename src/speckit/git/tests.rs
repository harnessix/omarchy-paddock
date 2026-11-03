use super::*;

fn git(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().into()
}

fn setup() -> (tempfile::TempDir, HostEnv) {
    let tmp = tempfile::tempdir().unwrap();
    git(tmp.path(), &["init", "-q", "-b", "main"]);
    let env = HostEnv::from_parts(
        tmp.path().join("home"),
        "tester".into(),
        1000,
        1000,
        Default::default(),
    );
    (tmp, env)
}

#[test]
fn notice_requires_default_store_and_files_git_can_publish() {
    let (tmp, env) = setup();
    let repo = tmp.path();
    let cfg = Cfg::parse("").unwrap();
    std::fs::create_dir_all(repo.join("specs")).unwrap();
    std::fs::write(repo.join("specs/plan.md"), "plan").unwrap();
    assert!(!needs_notice(&env, &cfg, repo), "no Spec Kit");
    std::fs::create_dir(repo.join(".specify")).unwrap();
    assert!(needs_notice(&env, &cfg, repo), "untracked files");
    std::fs::write(repo.join(".gitignore"), ".specify/\nspecs/\n").unwrap();
    assert!(!needs_notice(&env, &cfg, repo), "ignored files");
    git(repo, &["add", "-f", "specs/plan.md"]);
    assert!(needs_notice(&env, &cfg, repo), "tracked even if ignored");
    for text in [
        "[speckit]\nstore = \"repo\"",
        "[speckit]\nstore = \"external\"",
        "[speckit]\nenabled = false",
    ] {
        assert!(
            !needs_notice(&env, &Cfg::parse(text).unwrap(), repo),
            "{text}"
        );
    }
    assert!(needs_notice(
        &env,
        &Cfg::parse("[speckit]\nenabled = true").unwrap(),
        repo
    ));
    let layout = Layout::resolve(&env, repo).unwrap();
    assert!(location(&layout).contains("git tracks Spec Kit files"));
    git(repo, &["rm", "--cached", "-f", "specs/plan.md"]);
    assert!(location(&layout).contains("git tracks no Spec Kit files"));
    std::fs::write(repo.join(".specify/constitution.md"), "constitution").unwrap();
    std::fs::write(repo.join(".gitignore"), "specs/\n").unwrap();
    assert!(needs_notice(&env, &cfg, repo), ".specify alone");
}

#[test]
fn push_checks_commits_in_the_range_including_removed_files() {
    let (tmp, _) = setup();
    let repo = tmp.path();
    std::fs::write(repo.join("README"), "readme").unwrap();
    git(repo, &["add", "."]);
    git(repo, &["commit", "-qm", "initial"]);
    let base = git(repo, &["rev-parse", "HEAD"]);
    let contains = |range: &str| {
        let layout = crate::hostgit::Layout::discover(repo).unwrap().unwrap();
        let mut cmd = layout.command();
        cmd.args(["log", range]);
        push_contains(cmd).unwrap()
    };
    assert!(!contains("HEAD"));
    std::fs::create_dir(repo.join("specs")).unwrap();
    std::fs::write(repo.join("specs/plan.md"), "plan").unwrap();
    git(repo, &["add", "."]);
    git(repo, &["commit", "-qm", "plan"]);
    assert!(contains(&format!("{base}..HEAD")));
    git(repo, &["rm", "-q", "specs/plan.md"]);
    git(repo, &["commit", "-qm", "remove plan"]);
    assert!(contains("HEAD~1..HEAD"));
    // A later ordinary commit has no warning, even with specs in its history.
    std::fs::write(repo.join("README"), "updated").unwrap();
    git(repo, &["add", "."]);
    git(repo, &["commit", "-qm", "readme"]);
    assert!(!contains("HEAD~1..HEAD"));
    assert!(contains(&format!("{base}..HEAD")));
}
