use super::*;

fn sh(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@localhost"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A project with one commit and a bare repository as `origin`.
fn project(tmp: &Path) -> (PathBuf, PathBuf) {
    let remote = tmp.join("remote.git");
    let work = tmp.join("work");
    std::fs::create_dir_all(&work).unwrap();
    sh(
        tmp,
        &[
            "init",
            "-q",
            "--bare",
            "-b",
            "main",
            remote.to_str().unwrap(),
        ],
    );
    sh(&work, &["init", "-q", "-b", "main"]);
    sh(&work, &["commit", "-q", "--allow-empty", "-m", "first"]);
    sh(
        &work,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    sh(&work, &["push", "-q", "origin", "main"]);
    (work, remote)
}

fn args(remote: &str, url: &str, refs: serde_json::Value) -> BTreeMap<String, serde_json::Value> {
    let mut a = BTreeMap::new();
    a.insert("remote".into(), serde_json::json!(remote));
    a.insert("url".into(), serde_json::json!(url));
    a.insert("refs".into(), refs);
    a
}

#[test]
fn the_snapshot_reads_the_remotes() {
    let tmp = tempfile::tempdir().unwrap();
    let (work, remote) = project(tmp.path());
    sh(&work, &["config", "remote.origin.pushurl", "/srv/push.git"]);
    let repo = Repo::snapshot(&work).unwrap();
    assert_eq!(repo.remotes.len(), 1);
    assert_eq!(repo.remotes[0].url, remote.to_str().unwrap());
    assert_eq!(repo.find("origin").unwrap().push_target(), "/srv/push.git");
    assert!(repo.find("sneaky").is_none());
}

#[test]
fn the_sandbox_gitconfig_rewrites_pushes_and_drops_credential_helpers() {
    let repo = Repo {
        remotes: vec![Remote {
            name: "origin".into(),
            url: "/srv/a.git".into(),
            push_url: Some("/srv/b.git".into()),
            gh_resolved: None,
        }],
        ..Default::default()
    };
    let t = sandbox_gitconfig(Some(&repo));
    assert!(t.contains("[credential]\n\thelper =\n"));
    assert!(t.contains("[url \"paddock::https://\"]\n\tpushInsteadOf = \"https://\""));
    assert!(t.contains("[url \"paddock::/srv/a.git\"]\n\tpushInsteadOf = \"/srv/a.git\""));
    assert!(t.contains("[url \"paddock::/srv/b.git\"]\n\tinsteadOf = \"/srv/b.git\""));
}

#[test]
fn refs_must_be_branches_or_tags() {
    for bad in [
        "refs/heads/../x",
        "HEAD",
        "refs/notes/x",
        "refs/heads/a b",
        "refs/heads/x.lock",
    ] {
        assert!(!valid_ref(bad), "{bad}");
    }
    assert!(valid_ref("refs/heads/feature/one"));
    assert!(valid_ref("refs/tags/v1.0"));
}

#[test]
fn a_push_lists_commits_and_reaches_the_remote() {
    let tmp = tempfile::tempdir().unwrap();
    let (work, remote) = project(tmp.path());
    let repo = Repo::snapshot(&work).unwrap();
    sh(&work, &["commit", "-q", "--allow-empty", "-m", "second"]);
    sh(&work, &["commit", "-q", "--allow-empty", "-m", "third"]);
    let sha = sh(&work, &["rev-parse", "HEAD"]);
    // A hook in the project must never run on the host.
    let hook = work.join(".git/hooks/pre-push");
    std::fs::write(&hook, "#!/bin/sh\ntouch \"$GIT_DIR/../hook-ran\"\nexit 1\n").unwrap();
    std::fs::set_permissions(&hook, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let a = args(
        "origin",
        remote.to_str().unwrap(),
        serde_json::json!([{ "src": "HEAD", "sha": sha, "dst": "refs/heads/feature" }]),
    );
    let p = plan(Some(&repo), &tmp.path().join("run"), &a).unwrap();
    assert!(p.known && p.session_allowed());
    assert_eq!(p.refs[0].commits.len(), 2, "{:?}", p.refs[0].commits);
    assert!(p.detail().contains("third"));
    let r = push(&p).unwrap();
    assert_eq!(r["refs"][0]["ok"], true, "{r}");
    assert_eq!(sh(&remote, &["rev-parse", "refs/heads/feature"]), sha);
    assert!(!work.join("hook-ran").exists());
    let scratch = p.scratch.0.clone();
    drop(p);
    assert!(!scratch.exists());
}

#[test]
fn the_object_store_is_checked_again_before_the_push() {
    let tmp = tempfile::tempdir().unwrap();
    let (work, remote) = project(tmp.path());
    let repo = Repo::snapshot(&work).unwrap();
    let sha = sh(&work, &["rev-parse", "HEAD"]);
    let a = args(
        "origin",
        remote.to_str().unwrap(),
        serde_json::json!([{ "sha": sha, "dst": "refs/heads/feature" }]),
    );
    let p = plan(Some(&repo), &tmp.path().join("run"), &a).unwrap();
    // The sandbox keeps running while the card waits: a new alternates
    // line would borrow the objects of another repository.
    let info = work.join(".git/objects/info");
    std::fs::create_dir_all(&info).unwrap();
    std::fs::write(info.join("alternates"), "/srv/other/objects\n").unwrap();
    let e = push(&p).unwrap_err().to_string();
    assert!(e.contains("object store"), "{e}");
    std::fs::remove_file(info.join("alternates")).unwrap();
    // A symlink in place of a pack directory is refused as well.
    let pack = work.join(".git/objects/pack");
    std::fs::remove_dir_all(&pack).unwrap();
    std::os::unix::fs::symlink("/srv/other/objects/pack", &pack).unwrap();
    let e = push(&p).unwrap_err().to_string();
    assert!(e.contains("symlink"), "{e}");
}

#[test]
fn force_default_branch_and_new_remotes_cannot_be_session_granted() {
    let tmp = tempfile::tempdir().unwrap();
    let (work, remote) = project(tmp.path());
    let repo = Repo::snapshot(&work).unwrap();
    let sha = sh(&work, &["rev-parse", "HEAD"]);
    let run = tmp.path().join("run");
    let one =
        |dst: &str, force: bool| serde_json::json!([{ "sha": sha, "dst": dst, "force": force }]);
    let url = remote.to_str().unwrap();
    let p = plan(
        Some(&repo),
        &run,
        &args("origin", url, one("refs/heads/x", true)),
    )
    .unwrap();
    assert!(!p.session_allowed());
    let p = plan(
        Some(&repo),
        &run,
        &args("origin", url, one("refs/heads/main", false)),
    )
    .unwrap();
    assert!(p.refs[0].default_branch && !p.session_allowed());
    let p = plan(
        Some(&repo),
        &run,
        &args(
            "sneaky",
            "https://example.com/other.git",
            one("refs/heads/x", false),
        ),
    )
    .unwrap();
    assert!(!p.known && !p.session_allowed());
    assert!(p.detail().starts_with("WARNING"));
    // A changed URL of a known remote still pushes to the original one.
    let p = plan(
        Some(&repo),
        &run,
        &args(
            "origin",
            "https://example.com/evil.git",
            one("refs/heads/x", false),
        ),
    )
    .unwrap();
    assert_eq!(p.url, url);
    assert!(p.detail().contains("original URL"));
    assert!(plan(
        Some(&repo),
        &run,
        &args("x", "ext::sh -c evil", one("refs/heads/x", false))
    )
    .is_err());
}

#[test]
fn push_urls_lose_their_credentials_on_the_card_and_in_the_logs() {
    assert_eq!(
        redact("https://user:ghp_secret@github.com/a/b.git"),
        "https://***@github.com/a/b.git"
    );
    assert_eq!(
        redact("https://ghp_secret@github.com/a/b.git"),
        "https://***@github.com/a/b.git"
    );
    assert_eq!(
        redact("ssh://git:pw@host/a/b.git"),
        "ssh://git:***@host/a/b.git"
    );
    assert_eq!(redact("ssh://git@host/a.git"), "ssh://git@host/a.git");
    assert_eq!(redact("git@host:a/b.git"), "git@host:a/b.git");
    let tmp = tempfile::tempdir().unwrap();
    let (work, remote) = project(tmp.path());
    let repo = Repo::snapshot(&work).unwrap();
    let sha = sh(&work, &["rev-parse", "HEAD"]);
    let run = tmp.path().join("run");
    let one = serde_json::json!([{ "sha": sha, "dst": "refs/heads/x" }]);
    let url = "https://user:ghp_secret@example.com/a/b.git";
    // The URL as the remote name, as `git push URL` sends it.
    let p = plan(Some(&repo), &run, &args(url, url, one.clone())).unwrap();
    assert_eq!(p.url, url, "the push itself keeps the credentials");
    for text in [p.summary(), p.detail()] {
        assert!(!text.contains("ghp_secret"), "{text}");
    }
    // A changed URL of a known remote, in the note.
    let p = plan(Some(&repo), &run, &args("origin", url, one.clone())).unwrap();
    assert_eq!(p.url, remote.to_str().unwrap());
    assert!(!p.detail().contains("ghp_secret"), "{}", p.detail());
    let e = plan(
        Some(&repo),
        &run,
        &args("x", "ext::https://u:ghp_secret@x/", one),
    )
    .unwrap_err()
    .to_string();
    assert!(!e.contains("ghp_secret"), "{e}");
}

#[test]
fn a_relative_remote_path_is_read_from_the_work_tree() {
    let tmp = tempfile::tempdir().unwrap();
    let (work, remote) = project(tmp.path());
    sh(&work, &["remote", "set-url", "origin", "../remote.git"]);
    let repo = Repo::snapshot(&work).unwrap();
    let sha = sh(&work, &["rev-parse", "HEAD"]);
    let a = args(
        "origin",
        "../remote.git",
        serde_json::json!([{ "sha": sha, "dst": "refs/heads/rel" }]),
    );
    let p = plan(Some(&repo), &tmp.path().join("run"), &a).unwrap();
    assert!(p.notes.is_empty(), "{:?}", p.notes);
    let r = push(&p).unwrap();
    assert_eq!(r["refs"][0]["ok"], true, "{r}");
    assert_eq!(sh(&remote, &["rev-parse", "refs/heads/rel"]), sha);
}

#[test]
fn a_new_remote_on_this_machine_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let (work, _) = project(tmp.path());
    let repo = Repo::snapshot(&work).unwrap();
    let sha = sh(&work, &["rev-parse", "HEAD"]);
    let run = tmp.path().join("run");
    // The sandbox can write the project: a bare repository there with a
    // hook would run that hook on the host when the host pushes to it.
    let evil = work.join("evil.git");
    sh(
        tmp.path(),
        &["init", "-q", "--bare", evil.to_str().unwrap()],
    );
    let hook = evil.join("hooks/pre-receive");
    std::fs::write(&hook, "#!/bin/sh\ntouch ../RAN\n").unwrap();
    std::fs::set_permissions(&hook, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let one = serde_json::json!([{ "sha": sha, "dst": "refs/heads/x" }]);
    let path = evil.to_str().unwrap().to_string();
    for url in [
        path.clone(),
        format!("file://{path}"),
        "evil.git".to_string(),
        "../evil.git".to_string(),
        "./evil.git".to_string(),
        "~/evil.git".to_string(),
        "file:///srv/x.git".to_string(),
    ] {
        assert!(
            plan(Some(&repo), &run, &args("evil", &url, one.clone())).is_err(),
            "{url}"
        );
    }
    assert!(!work.join("RAN").exists());
    for url in [
        "https://example.com/a/b.git",
        "ssh://git@example.com/a/b.git",
        "git@example.com:a/b.git",
    ] {
        assert!(
            plan(Some(&repo), &run, &args("ok", url, one.clone())).is_ok(),
            "{url}"
        );
    }
}
