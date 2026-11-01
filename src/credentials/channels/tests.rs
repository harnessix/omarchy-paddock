use super::*;
use crate::credentials::{parse, rows};
use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt as _;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A temp home with a paddock configuration, a bin folder as PATH, and the
/// given fake tools in it.
fn world(cfg: &str, tools: &[(&str, &str)]) -> (tempfile::TempDir, HostEnv) {
    let t = tempfile::tempdir().unwrap();
    let home = t.path().join("home");
    let bin = t.path().join("bin");
    std::fs::create_dir_all(home.join(".config/paddock")).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(home.join(".config/paddock/config.toml"), cfg).unwrap();
    for (name, body) in tools {
        let p = bin.join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut v = BTreeMap::new();
    v.insert(
        "PATH".to_string(),
        format!("{}:/usr/bin:/bin", bin.display()),
    );
    let env = HostEnv::from_parts(home, "t".into(), 1000, 1000, v);
    (t, env)
}

fn spec(env: &HostEnv) -> crate::spec::SandboxSpec {
    crate::spec::SandboxSpec::new(
        "sb-1".into(),
        "claude",
        std::path::Path::new("/home/demo/app"),
        env.home.join("sb"),
        &env.home,
        1000,
        1000,
        "t",
    )
}

/// Channels whose cards answer `yes`, and the number of cards.
fn channels(env: &HostEnv, yes: bool) -> (Channels, Arc<AtomicUsize>) {
    let asked = Arc::new(AtomicUsize::new(0));
    let a = asked.clone();
    let creds: Vec<(String, Credential)> =
        crate::credentials::load(env).unwrap().into_iter().collect();
    let ch = Channels::new(
        env.clone(),
        spec(env),
        creds,
        crate::credentials::revoked_file(env, "sb-1"),
        Arc::new(move |_: &str, _: &str| {
            a.fetch_add(1, Ordering::SeqCst);
            yes
        }),
    );
    (ch, asked)
}

fn args(pairs: &[(&str, &str)]) -> BTreeMap<String, serde_json::Value> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), serde_json::json!(v)))
        .collect()
}

fn revoke(env: &HostEnv, name: &str) {
    let f = crate::credentials::revoked_file(env, "sb-1");
    std::fs::create_dir_all(f.parent().unwrap()).unwrap();
    std::fs::write(f, format!("{name}\n")).unwrap();
}

const DOCKER: &str = r#"
[credentials.registry]
via = "agent"
kind = "docker"
registries = ["ghcr.io", "registry.example.com"]
"#;

/// A docker credential store: a login for ghcr.io only.
const FAKE_STORE: &str = r#"read -r server || true
[ "$1" = get ] || exit 1
case "$server" in
  ghcr.io|https://ghcr.io) printf '{"ServerURL":"ghcr.io","Username":"alice","Secret":"ghcr-s3cret"}' ;;
  *) echo "credentials not found in native keychain"; exit 1 ;;
esac
"#;

fn docker_world(cfg: &str) -> (tempfile::TempDir, HostEnv) {
    let (t, env) = world(cfg, &[("docker-credential-fake", FAKE_STORE)]);
    use base64::Engine as _;
    let auth = base64::engine::general_purpose::STANDARD.encode("bob:plain-s3cret");
    std::fs::create_dir_all(env.home.join(".docker")).unwrap();
    std::fs::write(
        env.home.join(".docker/config.json"),
        serde_json::json!({
            "credHelpers": { "ghcr.io": "fake" },
            "auths": { "https://registry.example.com": { "auth": auth } },
        })
        .to_string(),
    )
    .unwrap();
    (t, env)
}

#[test]
fn registry_names_have_one_form() {
    assert_eq!(
        registry_host("https://GHCR.io/v2/").as_deref(),
        Some("ghcr.io")
    );
    assert_eq!(
        registry_host("localhost:5000").as_deref(),
        Some("localhost:5000")
    );
    assert_eq!(
        registry_host("https://index.docker.io/v1/").as_deref(),
        Some("index.docker.io")
    );
    assert_eq!(
        registry_host("docker.io").as_deref(),
        Some("index.docker.io")
    );
    for bad in ["", "a b", "host:http", "user@host", "https://"] {
        assert_eq!(registry_host(bad), None, "{bad}");
    }
    let cfg: serde_json::Value = serde_json::from_str(&sandbox_docker_config(&[
        "ghcr.io".into(),
        "docker.io".into(),
    ]))
    .unwrap();
    assert_eq!(cfg["credHelpers"]["ghcr.io"], "paddock");
    assert_eq!(cfg["credHelpers"]["https://index.docker.io/v1/"], "paddock");
    assert_eq!(cfg.as_object().unwrap().len(), 1);
}

#[test]
fn the_docker_helper_gives_listed_logins_once_asked() {
    let (_t, env) = docker_world(DOCKER);
    let (ch, asked) = channels(&env, true);
    // A credential helper of the host store.
    let r = ch
        .handle(&args(&[("kind", "docker"), ("server", "ghcr.io")]))
        .unwrap();
    assert_eq!(
        (
            r["ServerURL"].as_str(),
            r["Username"].as_str(),
            r["Secret"].as_str()
        ),
        (Some("ghcr.io"), Some("alice"), Some("ghcr-s3cret"))
    );
    // Plain auths, under another form of the same name.
    let r = ch
        .handle(&args(&[
            ("kind", "docker"),
            ("server", "registry.example.com"),
        ]))
        .unwrap();
    assert_eq!(
        (r["Username"].as_str(), r["Secret"].as_str()),
        (Some("bob"), Some("plain-s3cret"))
    );
    // Once per registry and run.
    for _ in 0..3 {
        ch.handle(&args(&[("kind", "docker"), ("server", "https://ghcr.io")]))
            .unwrap();
    }
    assert_eq!(asked.load(Ordering::SeqCst), 2);
    // A registry that is not listed never reaches the store or a card.
    let (s, m) = ch
        .handle(&args(&[("kind", "docker"), ("server", "evil.example")]))
        .unwrap_err();
    assert_eq!(s, Status::Denied);
    assert!(m.contains("not a registry"), "{m}");
    assert_eq!(asked.load(Ordering::SeqCst), 2);
    // Bad input.
    for bad in ["", "ghcr.io\nx", "a b"] {
        let (s, _) = ch
            .handle(&args(&[("kind", "docker"), ("server", bad)]))
            .unwrap_err();
        assert_eq!(s, Status::Invalid, "{bad:?}");
    }
    // Revoke stops it.
    revoke(&env, "registry");
    let (s, m) = ch
        .handle(&args(&[("kind", "docker"), ("server", "ghcr.io")]))
        .unwrap_err();
    assert_eq!(s, Status::Denied);
    assert!(m.contains("revoked"), "{m}");
    // The audit log names registries, never values, and uses count.
    let log = std::fs::read_to_string(crate::broker::audit_file(&env)).unwrap();
    assert!(log.contains("agent registry docker ghcr.io"), "{log}");
    assert!(log.contains("not a listed registry") && log.contains("denied: revoked"));
    assert!(!log.contains("s3cret") && !log.contains("alice"), "{log}");
    let r = rows(&env).unwrap();
    assert_eq!((r[0].via, r[0].used, r[0].ready), ("agent", 5, true));
    assert!(r[0].source.contains("docker"), "{}", r[0].source);
}

#[test]
fn a_denied_card_and_a_missing_login_give_nothing() {
    let (_t, env) = docker_world(DOCKER);
    let (ch, asked) = channels(&env, false);
    let (s, _) = ch
        .handle(&args(&[("kind", "docker"), ("server", "ghcr.io")]))
        .unwrap_err();
    assert_eq!(s, Status::Denied);
    // A denial is not remembered: the next use asks again.
    let _ = ch.handle(&args(&[("kind", "docker"), ("server", "ghcr.io")]));
    assert_eq!(asked.load(Ordering::SeqCst), 2);
    // The store has no login: a failure, with no value.
    std::fs::write(
        env.home.join(".docker/config.json"),
        r#"{"credsStore":"fake"}"#,
    )
    .unwrap();
    let (ch, _) = channels(&env, true);
    let (s, m) = ch
        .handle(&args(&[
            ("kind", "docker"),
            ("server", "registry.example.com"),
        ]))
        .unwrap_err();
    assert_eq!(s, Status::Failed);
    assert!(m.contains("no login"), "{m}");
    // A store name that is a path never runs.
    std::fs::write(
        env.home.join(".docker/config.json"),
        r#"{"credsStore":"../x"}"#,
    )
    .unwrap();
    assert!(docker_login(&env, "ghcr.io", "ghcr.io").is_err());
}

#[test]
fn the_sandbox_helper_refuses_store_erase_and_list() {
    for action in ["store", "erase", "list"] {
        assert_eq!(
            crate::hostcli::credential_main(&["docker".into(), action.into()]),
            1
        );
    }
    let ch = crate::broker::Broker::new(
        HostEnv::from_parts(
            "/nonexistent".into(),
            "t".into(),
            1000,
            1000,
            BTreeMap::new(),
        ),
        crate::broker::Context {
            id: "sb-1".into(),
            harness: "claude".into(),
            project: "/nonexistent".into(),
            repo: None,
            chain: Vec::new(),
            depth: 0,
            role: None,
        },
        crate::broker::HostPolicy::default(),
        Arc::new(crate::broker::HostApprover),
    );
    let r = ch.handle(&crate::broker::Request {
        v: crate::broker::PROTOCOL,
        cap: CAPABILITY.into(),
        args: args(&[("kind", "aws"), ("name", "x")]),
        reason: String::new(),
    });
    assert_eq!(r.status, Status::Disabled);
}

const AWS: &str = r#"
[credentials.aws-dev]
via = "agent"
kind = "aws"
profile = "dev"
duration = 1800
"#;

/// A fake aws: export-credentials gives the long-lived keys of the
/// profile, sts a session. It notes the duration it was asked for.
fn fake_aws(export: &str) -> String {
    format!(
        r#"here=$(dirname "$0")
case "$1 $2" in
  "configure export-credentials") printf '%s' '{export}' ;;
  "sts get-session-token")
    while [ $# -gt 0 ]; do [ "$1" = --duration-seconds ] && echo "$2" > "$here/duration"; shift; done
    printf '{{"Credentials":{{"AccessKeyId":"ASIATEMP","SecretAccessKey":"temp-secret","SessionToken":"temp-token","Expiration":"2026-11-02T12:00:00Z"}}}}' ;;
  *) exit 2 ;;
esac
"#
    )
}

const LONG_LIVED: &str = r#"{"Version":1,"AccessKeyId":"AKIALONGLIVEDMARKER","SecretAccessKey":"LONGLIVED-SECRET-MARKER"}"#;

#[test]
fn aws_gives_session_credentials_and_never_the_long_lived_keys() {
    let (t, env) = world(AWS, &[("aws", &fake_aws(LONG_LIVED))]);
    let (ch, asked) = channels(&env, true);
    let a = args(&[("kind", "aws"), ("name", "aws-dev")]);
    let r = ch.handle(&a).unwrap();
    assert_eq!(r["Version"], 1);
    assert_eq!(r["AccessKeyId"], "ASIATEMP");
    assert_eq!(r["SecretAccessKey"], "temp-secret");
    assert_eq!(r["SessionToken"], "temp-token");
    assert_eq!(r["Expiration"], "2026-11-02T12:00:00Z");
    assert_eq!(r.as_object().unwrap().len(), 5);
    assert!(!r.to_string().contains("LONGLIVED"), "{r}");
    assert_eq!(
        std::fs::read_to_string(t.path().join("bin/duration"))
            .unwrap()
            .trim(),
        "1800"
    );
    // Later uses get a new set without a card.
    ch.handle(&a).unwrap();
    assert_eq!(asked.load(Ordering::SeqCst), 1);
    // Another name is not an aws credential of the sandbox.
    let (s, _) = ch
        .handle(&args(&[("kind", "aws"), ("name", "aws-prod")]))
        .unwrap_err();
    assert_eq!(s, Status::Invalid);
    // Revoke stops it.
    revoke(&env, "*");
    let (s, _) = ch.handle(&a).unwrap_err();
    assert_eq!(s, Status::Denied);
    let log = std::fs::read_to_string(crate::broker::audit_file(&env)).unwrap();
    assert!(log.contains("agent aws-dev aws dev"), "{log}");
    assert!(
        !log.contains("temp-secret") && !log.contains("LONGLIVED"),
        "{log}"
    );
    let r = rows(&env).unwrap();
    assert_eq!(r[0].used, 2);
    assert!(r[0].source.contains("aws profile dev"), "{}", r[0].source);
}

#[test]
fn a_profile_with_temporary_credentials_gives_them_as_they_are() {
    let export = r#"{"Version":1,"AccessKeyId":"ASIASSO","SecretAccessKey":"sso-secret","SessionToken":"sso-token","Expiration":"2026-11-02T13:00:00Z"}"#;
    let (t, env) = world(AWS, &[("aws", &fake_aws(export))]);
    let v = aws_credentials(&env, "dev", 900).unwrap();
    assert_eq!(v["AccessKeyId"], "ASIASSO");
    assert!(!t.path().join("bin/duration").exists());
    // An sts answer without a session token is refused.
    let (_t, env) = world(
        AWS,
        &[(
            "aws",
            r#"case "$1" in sts) printf '{"Credentials":{"AccessKeyId":"AKIALONGLIVEDMARKER","SecretAccessKey":"x"}}' ;; *) exit 1 ;; esac
"#,
        )],
    );
    let e = aws_credentials(&env, "dev", 900).unwrap_err();
    assert!(!format!("{e:#}").contains("LONGLIVED"));
    assert!(aws_credentials(&env, "dev;rm", 900).is_err());
}

#[test]
fn docker_and_aws_parse_with_their_limits() {
    let all = parse(&format!("{DOCKER}{AWS}")).unwrap();
    assert_eq!(all["registry"].kind(), Kind::Docker);
    assert_eq!(all["aws-dev"].aws_duration(), 1800);
    assert!(all.values().all(Credential::available));
    let default =
        parse("[credentials.a]\nvia = \"agent\"\nkind = \"aws\"\nprofile = \"p\"\n").unwrap();
    assert_eq!(default["a"].aws_duration(), 900);
    for bad in [
        "[credentials.a]\nvia = \"agent\"\nkind = \"aws\"\nprofile = \"p\"\nduration = 899\n",
        "[credentials.a]\nvia = \"agent\"\nkind = \"aws\"\nprofile = \"p\"\nduration = 3601\n",
        "[credentials.a]\nvia = \"agent\"\nkind = \"aws\"\n",
        "[credentials.a]\nvia = \"agent\"\nkind = \"aws\"\nprofile = \"a b\"\n",
        "[credentials.a]\nvia = \"agent\"\nkind = \"docker\"\n",
        "[credentials.a]\nvia = \"agent\"\nkind = \"docker\"\nregistries = [\"a/b c\"]\n",
        "[credentials.a]\nvia = \"agent\"\nkind = \"docker\"\nregistries = [\"r.io\"]\nkeys = [\"k.pub\"]\n",
        "[credentials.a]\nvia = \"agent\"\nregistries = [\"r.io\"]\n",
        "[credentials.a]\nvia = \"deliver\"\nkind = \"docker\"\n",
        "[credentials.a]\nvia = \"agent\"\nkind = \"docker\"\nregistries = [\"r.io\"]\nconfirm = \"each\"\n",
        "[credentials.a]\nvia = \"agent\"\nkind = \"aws\"\nprofile = \"p\"\n[credentials.b]\nvia = \"agent\"\nkind = \"aws\"\nprofile = \"p\"\n",
    ] {
        assert!(parse(bad).is_err(), "{bad}");
    }
}

#[test]
fn the_sandbox_gets_an_aws_configuration_without_keys() {
    let (t, env) = world(AWS, &[]);
    let creds: Vec<(String, Credential)> = crate::credentials::load(&env)
        .unwrap()
        .into_iter()
        .collect();
    let mut s = spec(&env);
    s.set_env("AWS_ACCESS_KEY_ID", "AKIAX", "test");
    setup_aws(&mut s, t.path(), &creds).unwrap();
    let text = std::fs::read_to_string(t.path().join("aws-config/config")).unwrap();
    assert_eq!(
        text,
        "[profile dev]\ncredential_process = paddock-host credential aws aws-dev\n\n"
    );
    assert_eq!(
        std::fs::read_to_string(t.path().join("aws-config/credentials")).unwrap(),
        ""
    );
    let v = |k: &str| s.env.get(k).map(|e| e.value.clone());
    assert_eq!(v("AWS_PROFILE").as_deref(), Some("dev"));
    assert_eq!(
        v("AWS_CONFIG_FILE").as_deref(),
        Some("/run/paddock/aws/config")
    );
    assert_eq!(
        v("AWS_SHARED_CREDENTIALS_FILE").as_deref(),
        Some("/run/paddock/aws/credentials")
    );
    assert_eq!(v("AWS_ACCESS_KEY_ID"), None);
    // The plan names them on the start card; aws is not on this PATH.
    let p = crate::credentials::plan(&env, "claude", false).unwrap();
    assert!(p.deliver.is_empty() || env.which("aws").is_some());
}
