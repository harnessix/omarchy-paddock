//! Environment inheritance: the default passes the host environment, but never
//! host sockets, never hidden names, and nothing at all in clean mode.

use paddock::build::{build_with, Options};
use paddock::env::HostEnv;
use paddock::profile::{Harness, Profile};
use std::collections::BTreeMap;
use std::path::PathBuf;

fn setup(hide: &[&str]) -> (tempfile::TempDir, HostEnv, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let project = tmp.path().join("work/app");
    std::fs::create_dir_all(&project).unwrap();
    let cfg = home.join(".config/paddock");
    std::fs::create_dir_all(&cfg).unwrap();
    let list: Vec<String> = hide.iter().map(|h| format!("\"{h}\"")).collect();
    std::fs::write(
        cfg.join("config.toml"),
        format!("[env]\nhide = [{}]\n", list.join(", ")),
    )
    .unwrap();
    let sock = tmp.path().join("ctl.sock");
    let _l = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    std::mem::forget(_l);
    let mut vars = BTreeMap::new();
    vars.insert("PATH".into(), std::env::var("PATH").unwrap_or_default());
    vars.insert("PLAIN_SETTING".into(), "1".into());
    vars.insert("SERVICE_TOKEN".into(), "t".into());
    vars.insert("HIDE_ME".into(), "x".into());
    vars.insert("SSH_AUTH_SOCK".into(), "/nowhere".into());
    vars.insert("SOME_CONTROL".into(), sock.display().to_string());
    let env = HostEnv::from_parts(home, "tester".into(), 1000, 1000, vars);
    (tmp, env, project)
}

fn harness() -> Harness {
    let profile: Profile = toml::from_str("name = \"t\"\n").unwrap();
    Harness {
        profile,
        exe: PathBuf::from("/bin/sh"),
        install_mounts: vec![],
        path_dirs: vec![],
        source: "test".into(),
    }
}

#[test]
fn inherit_skips_sockets_and_hidden_names() {
    let (_t, env, project) = setup(&["HIDE_ME"]);
    let opts = Options {
        harness: "t".into(),
        project: Some(project),
        assume_yes: true,
        ..Default::default()
    };
    let b = build_with(&env, &opts, harness()).unwrap();
    let e = &b.spec.env;
    assert!(e.contains_key("PLAIN_SETTING"));
    // A secret stays on the host unless something names it.
    assert!(!e.contains_key("SERVICE_TOKEN"));
    assert!(b
        .spec
        .notices
        .iter()
        .any(|n| n.contains("SERVICE_TOKEN") && n.contains("stay on the host")));
    assert!(!e.contains_key("HIDE_ME"));
    assert!(!e.contains_key("SSH_AUTH_SOCK"));
    assert!(
        !e.contains_key("SOME_CONTROL"),
        "a variable that points at a socket must not pass"
    );
}

#[test]
fn clean_mode_passes_nothing_from_the_host() {
    let (_t, env, project) = setup(&[]);
    let opts = Options {
        harness: "t".into(),
        project: Some(project),
        assume_yes: true,
        inherit_env: Some(false),
        ..Default::default()
    };
    let b = build_with(&env, &opts, harness()).unwrap();
    for k in ["PLAIN_SETTING", "SERVICE_TOKEN", "HIDE_ME"] {
        assert!(!b.spec.env.contains_key(k), "{k}");
    }
}

#[test]
fn inherit_drops_cloud_credentials_unless_passed_by_name() {
    let (_t, mut env, project) = setup(&[]);
    env.vars.insert("AWS_SECRET_ACCESS_KEY".into(), "s".into());
    env.vars
        .insert("TF_TOKEN_app_terraform_io".into(), "t".into());
    env.vars
        .insert("GOOGLE_APPLICATION_CREDENTIALS".into(), "/k.json".into());
    let opts = Options {
        harness: "t".into(),
        project: Some(project.clone()),
        assume_yes: true,
        ..Default::default()
    };
    let b = build_with(&env, &opts, harness()).unwrap();
    for k in [
        "AWS_SECRET_ACCESS_KEY",
        "TF_TOKEN_app_terraform_io",
        "GOOGLE_APPLICATION_CREDENTIALS",
    ] {
        assert!(!b.spec.env.contains_key(k), "{k} must not pass by default");
    }
    assert!(b
        .spec
        .notices
        .iter()
        .any(|n| n.contains("AWS_SECRET_ACCESS_KEY")));

    // Named in the user configuration: it passes.
    let cfg = env.home.join(".config/paddock/config.toml");
    std::fs::write(&cfg, "[env]\npass = [\"AWS_SECRET_ACCESS_KEY\"]\n").unwrap();
    let b = build_with(&env, &opts, harness()).unwrap();
    assert!(b.spec.env.contains_key("AWS_SECRET_ACCESS_KEY"));
    assert!(!b.spec.env.contains_key("GOOGLE_APPLICATION_CREDENTIALS"));
}

#[test]
fn a_harness_keeps_its_own_login_variables_and_pass_hands_over_others() {
    let (_t, mut env, project) = setup(&[]);
    env.vars.insert("T_API_KEY".into(), "login".into());
    env.vars.insert("OTHER_API_KEY".into(), "other".into());
    let mut h = harness();
    h.profile.state.env_pass = vec!["T_API_KEY".into()];
    let opts = Options {
        harness: "t".into(),
        project: Some(project),
        assume_yes: true,
        ..Default::default()
    };
    let b = build_with(&env, &opts, h.clone()).unwrap();
    let e = &b.spec.env;
    assert!(e
        .get("T_API_KEY")
        .is_some_and(|v| v.secret && v.value == "login"));
    assert!(!e.contains_key("OTHER_API_KEY"));
    let note = b.spec.notices.join("\n");
    assert!(
        note.contains("OTHER_API_KEY") && note.contains("T_API_KEY"),
        "{note}"
    );

    // [env] pass in the user configuration hands one over.
    let cfg = env.home.join(".config/paddock/config.toml");
    std::fs::write(&cfg, "[env]\npass = [\"OTHER_API_KEY\"]\n").unwrap();
    let b = build_with(&env, &opts, h).unwrap();
    assert!(b.spec.env.get("OTHER_API_KEY").is_some_and(|v| v.secret));
}

#[test]
fn a_registered_credential_never_passes_with_the_environment() {
    let (_t, mut env, project) = setup(&[]);
    env.vars.insert("T_API_KEY".into(), "login".into());
    env.vars.insert("DB_PASS".into(), "db".into());
    let cfg = env.home.join(".config/paddock/config.toml");
    std::fs::write(
        &cfg,
        "[env]\npass = [\"DB_PASS\"]\n\n[credentials.T_API_KEY]\nvia = \"deliver\"\n\n[credentials.DB]\nvia = \"deliver\"\nvar = \"DB_PASS\"\n",
    )
    .unwrap();
    let mut h = harness();
    h.profile.state.env_pass = vec!["T_API_KEY".into()];
    let opts = Options {
        harness: "t".into(),
        project: Some(project),
        assume_yes: true,
        ..Default::default()
    };
    let b = build_with(&env, &opts, h).unwrap();
    // Not as a login variable, not through [env] pass, not under its
    // sandbox name: only the deliver channel hands it over, after the card.
    for k in ["T_API_KEY", "DB_PASS", "DB"] {
        assert!(!b.spec.env.contains_key(k), "{k}");
    }
}

#[test]
fn withheld_names_go_into_the_boundary_log_without_values() {
    let (_t, mut env, project) = setup(&["HIDE_ME"]);
    env.vars
        .insert("SERVICE_TOKEN".into(), "value-that-must-not-leak".into());
    env.vars
        .insert("HIDE_ME".into(), "hidden-value-that-must-not-leak".into());
    let opts = Options {
        harness: "t".into(),
        project: Some(project),
        assume_yes: true,
        ..Default::default()
    };
    let b = build_with(&env, &opts, harness()).unwrap();
    paddock::boundary::record_start(&env, &b.spec);
    let raw = std::fs::read_to_string(paddock::boundary::file(&env, &b.spec.id)).unwrap();
    assert!(!raw.contains("must-not-leak"), "{raw}");
    let lines = paddock::boundary::read(&env, &b.spec.id);
    let rule = |name: &str| {
        lines
            .iter()
            .find(|l| l.kind == "env" && l.target == name)
            .map(|l| l.rule.clone())
    };
    assert!(rule("SERVICE_TOKEN").unwrap().contains("secret-looking"));
    assert_eq!(rule("HIDE_ME").unwrap(), "env hide");
    assert!(rule("PLAIN_SETTING").is_none());
}
