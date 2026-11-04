use super::*;
use std::cell::RefCell;

fn env(t: &Path) -> HostEnv {
    let mut vars = BTreeMap::new();
    vars.insert("PATH".into(), "/usr/bin".into());
    HostEnv::from_parts(t.join("home"), "demo".into(), 1, 1, vars)
}

const LOGIN: &str = ".fakeagent/login.json";

/// A fake harness profile with a refresh command. No real harness runs.
fn fake_profile(e: &HostEnv, name: &str, refresh: &str) {
    let d = e.paddock_config().join("harness.d");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(
        d.join(format!("{name}.toml")),
        format!(
            "name = \"{name}\"\n[locate]\ncommands = [\"sh\"]\n[state]\nauth = [\"{LOGIN}\"]\n{refresh}\n"
        ),
    )
    .unwrap();
}

fn login(exp: u64, refresh: &str) -> String {
    format!(r#"{{"access_token":"a","expires_at":{exp},"refresh_token":"{refresh}"}}"#)
}

fn write(p: &Path, text: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

const NOW: u64 = 2_000_000_000;

#[test]
fn a_lapsed_login_with_a_refresh_token_is_due() {
    let t = tempfile::tempdir().unwrap();
    let e = env(t.path());
    fake_profile(&e, "fakeagent", "refresh = [\"fakeagent\", \"renew\"]");
    let host = e.home.join(LOGIN);
    write(&host, &login(NOW - 10, "r1"));
    assert_eq!(
        due(&e, NOW),
        vec![Due {
            harness: "fakeagent".into(),
            args: vec!["renew".into()],
        }]
    );
    // Still valid: nothing to do.
    write(&host, &login(NOW + 600, "r1"));
    assert!(due(&e, NOW).is_empty());
    // Lapsed, but without a refresh token: a refresh cannot help.
    write(&host, &login(NOW - 10, ""));
    assert!(due(&e, NOW).is_empty());
    // A newer copy in a shared sandbox home counts too.
    let sb = e
        .paddock_data()
        .join("homes/fakeagent/shared/p1")
        .join(LOGIN);
    write(&sb, &login(NOW - 5, "r2"));
    assert_eq!(due(&e, NOW).len(), 1);
}

#[cfg(target_os = "linux")] // the keeper runs on Linux only
#[test]
fn isolated_account_kept_and_unconfigured_logins_are_never_refreshed() {
    let t = tempfile::tempdir().unwrap();
    let e = env(t.path());
    // No refresh key: nothing.
    fake_profile(&e, "fakeagent", "");
    write(&e.home.join(LOGIN), &login(NOW - 10, "r1"));
    assert!(due(&e, NOW).is_empty());

    fake_profile(&e, "fakeagent", "refresh = [\"fakeagent\", \"renew\"]");
    // The newest copy is in an account folder's home: the shared refresh
    // would not renew it.
    let acct = e
        .paddock_data()
        .join("homes/fakeagent/account-work/p1")
        .join(LOGIN);
    write(&acct, &login(NOW - 5, "r2"));
    assert!(due(&e, NOW).is_empty());
    std::fs::remove_file(&acct).unwrap();
    // An isolated copy is never the newest, so the host copy decides.
    let iso = e
        .paddock_data()
        .join("homes/fakeagent/isolated-x/p1")
        .join(LOGIN);
    write(&iso, &login(NOW - 5, "r3"));
    write(&e.home.join(LOGIN), &login(NOW + 600, "r1"));
    assert!(due(&e, NOW).is_empty());

    // The keeper keeps Codex logins: never, even with a refresh key.
    let d = e.paddock_config().join("harness.d");
    std::fs::write(
        d.join("codex.toml"),
        "name = \"codex\"\n[locate]\ncommands = [\"sh\"]\n[state]\nauth = [\".codex/auth.json\"]\nrefresh = [\"codex\", \"login\", \"status\"]\n",
    )
    .unwrap();
    write(
        &e.home.join(".codex/auth.json"),
        &format!(
            r#"{{"tokens":{{"access_token":"a","refresh_token":"r"}},"expires_at":{}}}"#,
            NOW - 10
        ),
    );
    assert!(due(&e, NOW).iter().all(|d| d.harness != "codex"));
}

#[test]
fn one_attempt_per_agent_in_ten_minutes() {
    let t = tempfile::tempdir().unwrap();
    let e = env(t.path());
    fake_profile(&e, "fakeagent", "refresh = [\"fakeagent\", \"renew\"]");
    let host = e.home.join(LOGIN);
    write(&host, &login(NOW - 10, "r1"));
    let calls = RefCell::new(Vec::new());
    // The fake runner does what the sandbox does at its end: it writes the
    // renewed login back to the store.
    let runner = |env: &HostEnv, d: &Due| -> Result<()> {
        calls.borrow_mut().push(d.args.clone());
        write(&env.home.join(LOGIN), &login(NOW + 3600, "r2"));
        Ok(())
    };
    assert_eq!(run_due(&e, NOW, &runner), vec!["fakeagent".to_string()]);
    assert_eq!(calls.borrow().len(), 1);
    assert_eq!(calls.borrow()[0], vec!["renew".to_string()]);
    assert_eq!(attempts(&e).get("fakeagent"), Some(&NOW));
    // The box picks the renewed copy.
    let (harnesses, field) = login_files(&e).remove(LOGIN).unwrap();
    let src = newest_login(&e, LOGIN, &harnesses, field.as_deref()).unwrap();
    assert!(std::fs::read_to_string(src).unwrap().contains("r2"));

    // Lapsed again within 10 minutes: no second attempt, also when the
    // first one failed.
    write(&host, &login(NOW - 10, "r1"));
    assert!(run_due(&e, NOW + 599, &runner).is_empty());
    assert_eq!(calls.borrow().len(), 1);
    let failing = |_: &HostEnv, _: &Due| -> Result<()> { bail!("no network") };
    assert_eq!(run_due(&e, NOW + 600, &failing).len(), 1);
    assert!(run_due(&e, NOW + 700, &runner).is_empty());
    assert_eq!(calls.borrow().len(), 1);
    assert_eq!(run_due(&e, NOW + 1200, &runner).len(), 1);
    assert_eq!(calls.borrow().len(), 2);
}

#[test]
fn the_refresh_runs_in_a_shared_sandbox_with_network_api() {
    let t = tempfile::tempdir().unwrap();
    let e = env(t.path());
    let a = run_args(
        &e,
        &Due {
            harness: "agy".into(),
            args: vec!["models".into()],
        },
    );
    let joined = a.join(" ");
    assert!(joined.starts_with("run agy "), "{joined}");
    assert!(joined.contains("--auth shared --network api --no-yolo"));
    assert!(joined.contains("--allow daily-cloudcode-pa.googleapis.com"));
    assert!(joined.ends_with("-- models"), "{joined}");
    assert_eq!(a[2], scratch(&e, "agy").display().to_string());
    // agy's profile names its refresh command.
    let p = crate::profile::builtin("agy").unwrap();
    assert_eq!(p.state.refresh, vec!["agy", "models"]);
}

fn script(dir: &Path, text: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join("fake-paddock");
    std::fs::write(&p, text).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

#[test]
fn a_refresh_that_takes_too_long_is_stopped() {
    let t = tempfile::tempdir().unwrap();
    let ok = script(t.path(), "#!/bin/sh\nexit 0\n");
    run_with_limit(&ok, &[], Duration::from_secs(5)).unwrap();
    std::fs::remove_file(&ok).unwrap();
    let mark = t.path().join("saved");
    // SIGTERM first, so paddock can save the login.
    let slow = script(
        t.path(),
        &format!(
            "#!/bin/sh\ntrap 'echo saved > {}; exit 0' TERM\nsleep 30 &\nwait\n",
            mark.display()
        ),
    );
    let start = Instant::now();
    let err = run_with_limit(&slow, &[], Duration::from_secs(1)).unwrap_err();
    assert!(err.to_string().contains("longer than 1 seconds"), "{err}");
    assert!(start.elapsed() < Duration::from_secs(7));
    assert!(mark.exists());
}
