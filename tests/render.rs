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

/// An isolated login's machine ID and boot ID replace the host's; a host
/// login keeps them. The containers show the host's machine ID for a host
/// login, never the image's.
#[test]
fn the_machine_identity_follows_the_login() {
    use paddock::backend::docker::Docker;
    use paddock::identity::MachineIdentity;
    let own = MachineIdentity {
        machine_id: Some("0123456789abcdef0123456789abcdef\n".into()),
        boot_id: Some("01234567-89ab-4def-8123-456789abcdef\n".into()),
    };
    let (_t, env, mut spec) = fake();
    spec.network = Network::None;
    // The containers check the mount points in the sandbox home.
    std::fs::create_dir_all(&spec.home_src).unwrap();
    let run = env.paddock_run().join(&spec.id);
    let mid = run.join("machine-id").display().to_string();
    let file = |l: &paddock::backend::Launch, name: &str| {
        l.files
            .iter()
            .find(|(p, _, _)| p.ends_with(name))
            .map(|(_, t, _)| t.clone())
    };
    if env.which("bwrap").is_some() {
        let host = Bwrap.render(&env, &spec, false).unwrap();
        let a = host.args.join(" ");
        assert!(
            !a.contains("/etc/machine-id") && !a.contains("boot_id"),
            "{a}"
        );
        spec.identity = own.clone();
        let l = Bwrap.render(&env, &spec, false).unwrap();
        let a = l.args.join(" ");
        assert!(
            a.contains(&format!("--ro-bind {mid} /etc/machine-id")),
            "{a}"
        );
        assert!(a.contains("/proc/sys/kernel/random/boot_id"), "{a}");
        // After /proc, or the new procfs would hide it.
        assert!(a.find("--proc /proc").unwrap() < a.find("boot_id").unwrap());
        assert_eq!(file(&l, "machine-id"), own.machine_id);
        assert_eq!(file(&l, "boot_id"), own.boot_id);
        spec.identity = Default::default();
    }
    for cli in ["docker", "podman"] {
        if env.which(cli).is_none() {
            continue;
        }
        let backend = Docker::new(cli);
        let host = backend.render(&env, &spec, false).unwrap().args.join(" ");
        if std::path::Path::new("/etc/machine-id").is_file() {
            assert!(
                host.contains("src=/etc/machine-id,dst=/etc/machine-id,readonly"),
                "{host}"
            );
        }
        let mut s = spec.clone();
        s.identity = own.clone();
        let l = backend.render(&env, &s, false).unwrap();
        let a = l.args.join(" ");
        assert!(
            a.contains(&format!("src={mid},dst=/etc/machine-id,readonly")),
            "{a}"
        );
        assert_eq!(file(&l, "machine-id"), own.machine_id);
        if std::path::Path::new("/sys/devices/virtual/dmi").is_dir() {
            assert!(a.contains("dst=/sys/devices/virtual/dmi"), "{a}");
        }
    }
}
