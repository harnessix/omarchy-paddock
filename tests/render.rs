//! Backend rendering with a fake host, so the rules hold on any machine.

use paddock::backend::{bwrap::Bwrap, Backend};
use paddock::env::HostEnv;
use paddock::spec::{Mount, Network, SandboxSpec};
use std::collections::BTreeMap;
use std::path::PathBuf;

fn fake() -> (tempfile::TempDir, HostEnv, SandboxSpec) {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let project = home.join("src/app");
    std::fs::create_dir_all(&project).unwrap();
    let mut vars = BTreeMap::new();
    vars.insert("PATH".into(), std::env::var("PATH").unwrap_or_default());
    vars.insert("SECRET_FROM_HOST".into(), "x".into());
    vars.insert("TERM".into(), "xterm".into());
    let env = HostEnv::from_parts(home.clone(), "tester".into(), 4242, 4343, vars);
    let mut spec = SandboxSpec::new(
        "t1".into(),
        "demo",
        &project,
        home.join(".local/share/paddock/homes/demo/x"),
        &home,
        4242,
        4343,
        "tester",
    );
    spec.add_mount(Mount::same(&project, true, "project"));
    spec.command = vec!["/bin/true".into()];
    (tmp, env, spec)
}

#[test]
fn bwrap_isolates_by_default() {
    let (_t, env, mut spec) = fake();
    if env.which("bwrap").is_none() {
        return;
    }
    spec.network = Network::None;
    let l = Bwrap.render(&env, &spec, false).unwrap();
    let a = l.args.join(" ");
    for flag in [
        "--unshare-all",
        "--disable-userns",
        "--die-with-parent",
        "--remount-ro /",
        "--uid 4242",
        "--gid 4343",
    ] {
        assert!(a.contains(flag), "missing {flag} in {a}");
    }
    assert!(!a.contains("--share-net"));
    // The host home itself is never a bind source.
    let home = env.home.display().to_string();
    assert!(!a.contains(&format!("--bind {home} ")) && !a.contains(&format!("--ro-bind {home} ")));
    assert!(!l.env.contains_key("SECRET_FROM_HOST"));
    assert_eq!(l.env.get("HOME").map(PathBuf::from), Some(env.home.clone()));
}

#[test]
fn egress_goes_through_pasta_without_host_loopback() {
    let (_t, env, mut spec) = fake();
    if env.which("bwrap").is_none() || env.which("pasta").is_none() {
        return;
    }
    spec.network = Network::Egress;
    let l = Bwrap.render(&env, &spec, false).unwrap();
    assert!(l.program.ends_with("pasta"));
    let a = l.args.join(" ");
    for flag in [
        "-T none",
        "-U none",
        "-u none",
        "-t none",
        "--no-map-gw",
        "--dns-forward",
    ] {
        assert!(a.contains(flag), "missing {flag} in {a}");
    }
}
