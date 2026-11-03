use super::*;

fn remotes() -> Vec<Remote> {
    vec![
        Remote {
            name: "origin".into(),
            url: "git@github.com:demo/app.git".into(),
            push_url: None,
            gh_resolved: None,
        },
        Remote {
            name: "upstream".into(),
            url: "https://github.com/acme/app".into(),
            push_url: None,
            gh_resolved: None,
        },
    ]
}

fn w(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| s.to_string()).collect()
}

fn run(args: &[&str]) -> Result<Call> {
    let a: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    check(&GH, &a, Some("fix-login"), &remotes())
}

#[test]
fn urls_and_repo_arguments_parse() {
    let want = RepoId {
        host: "github.com".into(),
        owner: "demo".into(),
        name: "app".into(),
    };
    for u in [
        "git@github.com:demo/app.git",
        "https://github.com/demo/app",
        "https://github.com/demo/app.git",
        "ssh://git@github.com:22/demo/app.git",
        "https://token@GitHub.com/Demo/app/",
    ] {
        assert_eq!(RepoId::from_url(u).as_ref(), Some(&want), "{u}");
    }
    assert_eq!(RepoId::from_arg("demo/app").as_ref(), Some(&want));
    assert_eq!(
        RepoId::from_arg("github.com/demo/app").as_ref(),
        Some(&want)
    );
    assert!(RepoId::from_arg("demo").is_none());
    assert!(RepoId::from_url("/srv/app.git").is_none());
}

#[test]
fn only_the_allowlist_passes() {
    for bad in [
        &["api", "user"][..],
        &["auth", "token"],
        &["secret", "list"],
        &["repo", "delete"],
        &["pr", "merge"],
        &["pr", "checkout"],
        &["run", "download"],
        &["extension", "install"],
        &["pr"],
    ] {
        assert!(run(bad).is_err(), "{bad:?}");
    }
    assert_eq!(run(&["pr", "list"]).unwrap().access, Access::Read);
    assert_eq!(
        run(&["pr", "create", "--title", "x"]).unwrap().access,
        Access::Write
    );
    assert_eq!(
        run(&["issue", "comment", "3", "--body", "x"])
            .unwrap()
            .access,
        Access::Write
    );
}

#[test]
fn b_is_the_branch_of_run_list_not_a_body() {
    let (rest, body) = split_body(&GH, &w(&["run", "list", "-b", "main"]));
    assert_eq!(body, None);
    assert_eq!(rest, w(&["run", "list", "-b", "main"]));
    let c = run(&["run", "list", "-b", "main"]).unwrap();
    assert_eq!(c.stdin, None);
    assert!(!c.argv.contains(&"--body-file".to_string()), "{:?}", c.argv);
    // A comment still takes -b as its body.
    let c = run(&["pr", "comment", "3", "-b", "hi"]).unwrap();
    assert_eq!(c.stdin.as_deref(), Some("hi"));
}

#[test]
fn tea_short_body_with_equals_is_the_body() {
    for form in [
        &["-d=secret text"][..],
        &["-d", "secret text"],
        &["--description=secret text"],
    ] {
        let mut a = w(&["pulls", "create", "--title", "x"]);
        a.extend(w(form));
        let (rest, body) = split_body(&TEA, &a);
        assert_eq!(body.as_deref(), Some("secret text"), "{form:?}");
        assert!(!rest.iter().any(|x| x.contains("secret")), "{rest:?}");
    }
}

#[test]
fn jq_cannot_read_the_host_environment() {
    for bad in [
        &["repo", "view", "--json", "name", "--jq", "$ENV.GH_TOKEN"][..],
        &["repo", "view", "--json", "name", "--jq=env.GH_TOKEN"],
        &["pr", "list", "--json", "title", "-q", "env | .GH_TOKEN"],
        &["pr", "list", "--json", "title", "-qenv.GH_TOKEN"],
        &["pr", "list", "--json", "title", "-q=$ENV"],
        &[
            "pr",
            "view",
            "1",
            "--json",
            "title",
            "-cq",
            "$ENV.GITHUB_TOKEN",
        ],
    ] {
        assert!(run(bad).is_err(), "{bad:?}");
    }
    for ok in [
        &["pr", "list", "--json", "title", "--jq", ".[].title"][..],
        &["repo", "view", "--json", "name", "-q", ".environment"],
        &["pr", "create", "--title", "fix env"],
    ] {
        assert!(run(ok).is_ok(), "{ok:?}");
    }
}

#[test]
fn a_tool_gets_only_its_own_token_variables() {
    let gh = forge_env("gh");
    assert!(gh.contains(&"GH_TOKEN".to_string()));
    assert!(!gh.iter().any(|v| v.starts_with("GITLAB")));
    let glab = forge_env("glab");
    assert!(glab.contains(&"GITLAB_TOKEN".to_string()));
    assert!(!glab
        .iter()
        .any(|v| v.contains("GH_") || v.starts_with("GITHUB")));
    assert!(!forge_env("tea").iter().any(|v| v.contains("TOKEN")));
}

#[test]
fn the_repository_must_be_a_project_remote() {
    assert!(run(&["pr", "list", "-R", "other/app"]).is_err());
    assert!(run(&["pr", "list", "--repo=other/app"]).is_err());
    assert!(run(&["pr", "list", "-Rother/app"]).is_err());
    assert!(run(&["pr", "view", "https://github.com/other/app/pull/1"]).is_err());
    // The sandbox's branch goes into the arguments too.
    for b in ["https://github.com/other/app/pull/1", "other/app#1"] {
        assert!(
            check(&GH, &w(&["pr", "view"]), Some(b), &remotes()).is_err(),
            "{b}"
        );
    }
    assert!(check(&GH, &w(&["pr", "view"]), Some("fix-login"), &remotes()).is_ok());
    assert!(run(&["issue", "view", "other/app#4"]).is_err());
    assert!(run(&["repo", "view", "other/app"]).is_err());
    let c = run(&["pr", "list", "-R", "demo/app"]).unwrap();
    assert_eq!(c.argv, ["pr", "list", "--repo", "github.com/demo/app"]);
    let c = run(&["pr", "view", "https://github.com/demo/app/pull/1"]).unwrap();
    assert_eq!(c.repo.owner, "acme");
}

#[test]
fn the_default_repository_and_branch_are_explicit() {
    // upstream comes before origin, as in gh.
    let c = run(&["pr", "create", "--title", "Fix", "--body", "Text"]).unwrap();
    assert_eq!(
        c.argv,
        [
            "pr",
            "create",
            "--title",
            "Fix",
            "--head",
            "fix-login",
            "--repo",
            "github.com/acme/app",
            "--body-file",
            "-"
        ]
    );
    assert_eq!(c.stdin.as_deref(), Some("Text"));
    let c = run(&["pr", "checks", "--watch"]).unwrap();
    assert_eq!(c.argv[..4], ["pr", "checks", "fix-login", "--watch"]);
    let c = run(&["pr", "view", "12", "--json", "title"]).unwrap();
    assert_eq!(c.argv[2], "12");
    let c = run(&["repo", "view"]).unwrap();
    assert_eq!(c.argv, ["repo", "view", "github.com/acme/app"]);
    let mut r = remotes();
    r[0].gh_resolved = Some("base".into());
    let c = check(&GH, &["pr".into(), "list".into()], None, &r).unwrap();
    assert_eq!(c.repo.owner, "demo");
}

#[test]
fn host_files_editors_and_browsers_are_refused() {
    for bad in [
        &["pr", "create", "--body-file", "/etc/passwd"][..],
        &["pr", "create", "-F", "x"],
        &["pr", "comment", "--editor"],
        &["pr", "view", "--web"],
        &["pr", "create", "-dw"],
        &["pr", "create", "--recover", "/tmp/x"],
        &["pr", "create", "-T", "bug.md"],
    ] {
        assert!(run(bad).is_err(), "{bad:?}");
    }
    // A value that looks like a flag stays a value.
    assert!(run(&["pr", "create", "--title", "--web is fixed"]).is_ok());
}

#[test]
fn body_text_never_stays_in_the_arguments() {
    // Short flags with an attached value must not hold w, e, F or T,
    // which the denied-flag check reads as a group of short flags.
    for (args, body) in [
        (
            &["pr", "comment", "1", "--body", "private text"][..],
            "private text",
        ),
        (
            &["pr", "comment", "1", "-b", "private text"][..],
            "private text",
        ),
        (
            &["pr", "comment", "1", "--body=private text"][..],
            "private text",
        ),
        (&["pr", "comment", "1", "-bsignal"][..], "signal"),
        (&["issue", "comment", "2", "-b=signal"][..], "signal"),
        (
            &[
                "pr",
                "comment",
                "1",
                "--body",
                "old",
                "--body",
                "private text",
            ][..],
            "private text",
        ),
    ] {
        let c = run(args).unwrap();
        assert!(
            !c.argv.iter().any(|a| a.contains(body) || a.contains("old")),
            "{:?}",
            c.argv
        );
        assert_eq!(c.argv[c.argv.len() - 2..], ["--body-file", "-"]);
        assert_eq!(c.stdin.as_deref(), Some(body), "{args:?}");
    }
    // The value of another flag that looks like --body stays a value.
    let c = run(&["pr", "create", "--title", "--body", "--head", "x"]).unwrap();
    assert_eq!(c.argv[2..4], ["--title", "--body"]);
    assert!(c.stdin.is_none());
}

#[test]
fn a_session_grant_key_covers_one_command_on_one_repository() {
    let a = run(&["pr", "comment", "1", "--body", "a"]).unwrap();
    let b = run(&["pr", "comment", "2", "--body", "b"]).unwrap();
    let c = run(&["issue", "comment", "2", "--body", "b"]).unwrap();
    assert_eq!(a.grant_key(), b.grant_key());
    assert_ne!(a.grant_key(), c.grant_key());
}

fn tea_remotes() -> Vec<Remote> {
    vec![
        Remote {
            name: "origin".into(),
            url: "git@codeberg.org:Demo/tool.git".into(),
            push_url: None,
            gh_resolved: None,
        },
        Remote {
            name: "mirror".into(),
            url: "https://git.example.org/demo/app.git".into(),
            push_url: None,
            gh_resolved: None,
        },
    ]
}

fn tea(args: &[&str]) -> Result<Call> {
    let a: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    check_with(
        &TEA,
        &a,
        Some("fix-login"),
        &tea_remotes(),
        &|r| match r.host.as_str() {
            "codeberg.org" => Ok("cb".into()),
            "git.example.org" => Ok("work".into()),
            h => bail!("no login for {h}"),
        },
    )
}

#[test]
fn tea_runs_its_allowlist_with_the_host_login() {
    let c = tea(&["pulls", "list", "--state", "open"]).unwrap();
    assert_eq!(c.access, Access::Read);
    assert_eq!(
        c.argv,
        [
            "pulls",
            "list",
            "--repo",
            "demo/tool",
            "--login",
            "cb",
            "--state",
            "open"
        ]
    );
    let c = tea(&["pulls", "12"]).unwrap();
    assert_eq!(
        c.argv,
        ["pulls", "--repo", "demo/tool", "--login", "cb", "12"]
    );
    assert_eq!(c.command, "pulls INDEX");
    let c = tea(&["issues", "list", "-r", "git.example.org/demo/app"]).unwrap();
    assert_eq!(
        c.argv,
        ["issues", "list", "--repo", "demo/app", "--login", "work"]
    );
    assert_eq!(tea(&["releases", "list"]).unwrap().access, Access::Read);
    let c = tea(&["comment", "3", "Looks", "good"]).unwrap();
    assert_eq!(c.access, Access::Write);
    assert_eq!(
        c.argv,
        [
            "comment",
            "--repo",
            "demo/tool",
            "--login",
            "cb",
            "3",
            "Looks good"
        ]
    );
    assert!(c.stdin.is_none());
    for bad in [
        &["pulls", "checkout", "5"][..],
        &["pulls", "merge", "5"],
        &["pulls", "close", "5"],
        &["pulls", "abc"],
        &["issues", "create"],
        &["releases", "create"],
        &["login", "list"],
        &["repos", "list"],
        &["api", "/user"],
        &["open"],
        &["comment"],
        &["comment", "3"],
        &["comment", "3", "--", "-x"],
    ] {
        assert!(tea(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn tea_refuses_logins_remotes_browsers_and_files() {
    for bad in [
        &["pulls", "list", "--login", "other"][..],
        &["pulls", "list", "--login=other"],
        &["pulls", "list", "-l", "other"],
        &["pulls", "list", "-login", "other"],
        &["pulls", "list", "---login", "other"],
        &["pulls", "list", "--remote", "origin"],
        &["pulls", "list", "-R", "origin"],
        &["pulls", "list", "-remote", "origin"],
        &["pulls", "list", "--web"],
        &["pulls", "list", "-w"],
        &["pulls", "create", "--asset", "/etc/passwd"],
        &["pulls", "create", "--title", "x", "--description", "-"],
        &["comment", "3", "-", "x"],
    ] {
        assert!(tea(bad).is_err(), "{bad:?}");
    }
    // A value that looks like a flag stays a value.
    let c = tea(&["pulls", "create", "--title", "--login is fixed"]).unwrap();
    assert!(c.argv.contains(&"--login is fixed".to_string()));
    assert_eq!(c.argv.iter().filter(|a| *a == "--login").count(), 1);
}

#[test]
fn tea_and_glab_take_only_known_flags() {
    // Their flags were not checked against the real programs, so an
    // unknown flag, which could read a host file, is refused.
    for args in [
        &[
            "pulls",
            "create",
            "--title",
            "t",
            "--body-file",
            "/etc/passwd",
        ][..],
        &["pulls", "list", "--some-new-flag"][..],
        &["pulls", "list", "-x"][..],
    ] {
        let e = tea(args).unwrap_err();
        assert!(
            format!("{e:#}").contains("only the flags"),
            "{args:?}: {e:#}"
        );
    }
    for args in [
        &["mr", "list", "--some-new-flag"][..],
        &[
            "mr",
            "create",
            "--title",
            "t",
            "--description-from",
            "/etc/passwd",
        ][..],
    ] {
        assert!(glab(args).is_err(), "{args:?}");
    }
    // Known flags still pass.
    assert!(tea(&["pulls", "list", "--state", "open"]).is_ok());
    assert!(glab(&["mr", "list", "--all"]).is_ok());
}

#[test]
fn tea_names_only_project_repositories() {
    for bad in [
        &["pulls", "list", "--repo", "other/app"][..],
        &["pulls", "list", "-r", "other/app"],
        &["pulls", "list", "--repo=codeberg.org/other/tool"],
        &["pulls", "list", "-repo", "github.com/demo/tool"],
        &["pulls", "list", "--repo", "https://codeberg.org/other/tool"],
        &["pulls", "list", "--repo", "/srv/demo/tool"],
        &["issues", "12", "https://codeberg.org/other/tool/issues/1"],
    ] {
        assert!(tea(bad).is_err(), "{bad:?}");
    }
    for ok in [
        "demo/tool",
        "codeberg.org/demo/tool",
        "https://codeberg.org/demo/tool.git",
    ] {
        assert_eq!(
            tea(&["pulls", "list", "--repo", ok]).unwrap().repo.host,
            "codeberg.org"
        );
    }
    // Without the host's login for that host, nothing runs.
    let a: Vec<String> = ["pulls", "list"].iter().map(|s| s.to_string()).collect();
    assert!(check(&TEA, &a, None, &tea_remotes()).is_err());
}

#[test]
fn tea_pull_requests_start_from_the_current_branch() {
    let c = tea(&["pulls", "create", "--title", "Fix", "--description", "Text"]).unwrap();
    assert_eq!(
        c.argv,
        [
            "pulls",
            "create",
            "--head",
            "fix-login",
            "--repo",
            "demo/tool",
            "--login",
            "cb",
            "--title",
            "Fix",
            "--description=Text"
        ]
    );
    let c = tea(&["pulls", "create", "--head", "other", "-d", "x"]).unwrap();
    assert_eq!(c.argv.iter().filter(|a| *a == "--head").count(), 1);
    assert!(c.argv.contains(&"other".to_string()));
    let a: Vec<String> = ["pulls", "create"].iter().map(|s| s.to_string()).collect();
    assert!(check_with(&TEA, &a, None, &tea_remotes(), &|_| Ok("cb".into())).is_err());
    // The body never shows in the audit line.
    let (rest, body) = split_body(&TEA, &["comment".into(), "3".into(), "secret".into()]);
    assert_eq!(rest, ["comment", "3"]);
    assert_eq!(body.as_deref(), Some("secret"));
}

#[test]
fn tea_logins_come_from_its_config() {
    let cfg = "logins:\n- name: work\n  url: https://git.example.org\n  token: abc\n  default: false\n- name: cb\n  url: https://codeberg.org\n  default: false\n- name: cb2\n  url: \"https://Codeberg.org/\"\n  default: true\npreferences:\n  editor: false\n";
    assert_eq!(tea_login_in(cfg, "codeberg.org").as_deref(), Some("cb2"));
    assert_eq!(
        tea_login_in(cfg, "git.example.org").as_deref(),
        Some("work")
    );
    assert_eq!(tea_login_in(cfg, "github.com"), None);
    assert_eq!(tea_login_in("preferences:\n  x: 1\n", "codeberg.org"), None);
}

fn glab(args: &[&str]) -> Result<Call> {
    let remotes = vec![
        Remote {
            name: "origin".into(),
            url: "git@gitlab.example.org:team/apps/web.git".into(),
            push_url: None,
            gh_resolved: None,
        },
        Remote {
            name: "mirror".into(),
            url: "https://gitlab.com/demo/app".into(),
            push_url: None,
            gh_resolved: None,
        },
    ];
    let a: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    check(&GLAB, &a, Some("fix-login"), &remotes)
}

const WEB: &str = "https://gitlab.example.org/team/apps/web";

#[test]
fn glab_runs_its_allowlist() {
    let c = glab(&["mr", "list"]).unwrap();
    assert_eq!(c.access, Access::Read);
    assert_eq!(c.argv, ["mr", "list", "--repo", WEB]);
    let c = glab(&["mr", "create", "--title", "Fix", "-d", "Text"]).unwrap();
    assert_eq!(c.access, Access::Write);
    assert_eq!(
        c.argv,
        [
            "mr",
            "create",
            "--title",
            "Fix",
            "--source-branch",
            "fix-login",
            "--repo",
            WEB,
            "--description=Text"
        ]
    );
    let c = glab(&["mr", "note", "3", "-m", "-1 from me"]).unwrap();
    assert_eq!(
        c.argv,
        ["mr", "note", "3", "--repo", WEB, "--message=-1 from me"]
    );
    assert_eq!(
        glab(&["issue", "note", "5", "--message=ok"])
            .unwrap()
            .access,
        Access::Write
    );
    let c = glab(&["repo", "view", "-R", "gitlab.com/demo/app"]).unwrap();
    assert_eq!(c.argv, ["repo", "view", "https://gitlab.com/demo/app"]);
    for ok in [
        &["issue", "list", "--label", "bug"][..],
        &["issue", "view", "7"],
        &["ci", "list", "--status", "failed"],
        &["mr", "diff", "3"],
        &[
            "mr",
            "view",
            "https://gitlab.example.org/team/apps/web/-/merge_requests/1",
        ],
    ] {
        assert_eq!(glab(ok).unwrap().access, Access::Read, "{ok:?}");
    }
    for bad in [
        &["mr", "merge", "3"][..],
        &["mr", "approve", "3"],
        &["mr", "checkout", "3"],
        &["mr", "update", "3"],
        &["ci", "run"],
        &["ci", "retry", "4"],
        &["ci", "delete", "4"],
        &["api", "projects"],
        &["auth", "status"],
        &["variable", "list"],
        &["release", "create", "v1"],
        &["mr"],
        &["mr", "note", "3"],
        &["issue", "note", "3"],
    ] {
        assert!(glab(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn glab_refuses_browsers_files_and_other_hosts() {
    for bad in [
        &["mr", "view", "3", "--web"][..],
        &["mr", "view", "3", "-w"],
        &["mr", "view", "3", "-cw"],
        &["ci", "view", "-w"],
        &["mr", "list", "--hostname", "evil.example"],
        &["mr", "list", "--hostname=evil.example"],
        &["mr", "create", "--description-file", "/etc/passwd"],
        &["mr", "create", "--title", "x", "--push"],
        &["mr", "create", "--title", "x", "-d", "-"],
    ] {
        assert!(glab(bad).is_err(), "{bad:?}");
    }
    assert!(glab(&["mr", "create", "--title", "--web is fixed"]).is_ok());
}

#[test]
fn glab_names_only_project_repositories() {
    for bad in [
        &["mr", "list", "-R", "team/apps/other"][..],
        &["mr", "list", "-Rteam/other"],
        &["mr", "list", "--repo=https://gitlab.example.org/team/apps"],
        &["mr", "list", "--repo", "gitlab.com/team/apps/web"],
        &[
            "mr",
            "view",
            "https://gitlab.example.org/team/apps/website/-/merge_requests/1",
        ],
        &[
            "mr",
            "view",
            "https://gitlab.com/other/app/-/merge_requests/1",
        ],
        &["repo", "view", "team/apps/web"],
    ] {
        assert!(glab(bad).is_err(), "{bad:?}");
    }
    for ok in ["team/apps/web", "gitlab.example.org/team/apps/web", WEB] {
        assert_eq!(
            glab(&["mr", "list", "-R", ok]).unwrap().repo.owner,
            "team/apps"
        );
    }
}

#[test]
fn glab_defaults_to_the_current_branch() {
    let c = glab(&["mr", "view"]).unwrap();
    assert_eq!(c.argv, ["mr", "view", "fix-login", "--repo", WEB]);
    let c = glab(&["ci", "view"]).unwrap();
    assert_eq!(c.argv, ["ci", "view", "fix-login", "--repo", WEB]);
    let c = glab(&["ci", "status"]).unwrap();
    assert_eq!(
        c.argv,
        ["ci", "status", "--branch", "fix-login", "--repo", WEB]
    );
    let c = glab(&["ci", "status", "-b", "main"]).unwrap();
    assert_eq!(c.argv, ["ci", "status", "-b", "main", "--repo", WEB]);
    let c = glab(&["mr", "create", "-s", "other", "--title", "x"]).unwrap();
    assert_eq!(c.argv.iter().filter(|a| a.contains("source")).count(), 0);
    // The body never shows in the audit line.
    let (rest, body) = split_body(
        &GLAB,
        &[
            "mr".into(),
            "note".into(),
            "3".into(),
            "-m".into(),
            "secret".into(),
        ],
    );
    assert_eq!(rest, ["mr", "note", "3"]);
    assert_eq!(body.as_deref(), Some("secret"));
}
