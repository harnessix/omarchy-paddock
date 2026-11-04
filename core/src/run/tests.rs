//! Tests of run.rs.

mod egress_addr_monitor_tests {
    use super::super::*;

    #[test]
    fn egress_addr_monitor_joins_on_finish() {
        let t = tempfile::tempdir().unwrap();
        let env = HostEnv::from_parts(
            t.path().join("host"),
            "demo".into(),
            1,
            1,
            Default::default(),
        );
        let info: crate::state::Running = serde_json::from_value(serde_json::json!({
            "id": "s-test",
            "harness": "h",
            "project": t.path().join("proj"),
            "backend": "bwrap",
            "network": "egress",
            "gui": "off",
            "pid": 1,
            "started": 100,
        }))
        .unwrap();
        let found = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let monitor = EgressAddrMonitor::start(env, info, found.clone());
        let start = std::time::Instant::now();
        monitor.finish();
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
    }
}

mod history_sync_tests {
    use super::super::*;
    use crate::profile::Profile;
    use std::collections::BTreeMap;

    #[test]
    fn new_history_reaches_the_host_before_the_sandbox_exits() {
        let t = tempfile::tempdir().unwrap();
        let host_home = t.path().join("host");
        let sb = t.path().join("sandbox");
        std::fs::create_dir_all(sb.join(".t/sessions")).unwrap();
        std::fs::create_dir_all(&host_home).unwrap();
        let env = HostEnv::from_parts(host_home.clone(), "demo".into(), 1, 1, BTreeMap::new());
        let p: Profile = toml::from_str(
            "name = \"t\"\n[state.history]\ncopy_back = [\".t/sessions\"]\nfiles = [\"*.jsonl\"]\n",
        )
        .unwrap();
        let history = crate::history::CopyBack::new(&env, &p, &state::AuthMode::Shared, &sb);
        let live = HistorySync::start(Some(history), Duration::ZERO);
        std::fs::write(sb.join(".t/sessions/a.jsonl"), "{}\n").unwrap();
        let host = host_home.join(".t/sessions/a.jsonl");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !host.exists() {
            assert!(
                Instant::now() < deadline,
                "the history did not reach the host"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        let (_, copied) = live.finish();
        assert_eq!(copied, 1);
    }
}

mod deliver_tests {
    use super::super::*;

    #[test]
    fn a_file_credential_sets_no_variable() {
        let all = crate::credentials::parse(
            "[credentials.GH_TOKEN]\nvia = \"deliver\"\nas = \"file\"\n\n[credentials.DB]\nvia = \"deliver\"\n",
        )
        .unwrap();
        let home = std::path::Path::new("/home/demo");
        let mut spec = crate::spec::SandboxSpec::new(
            "sb-1".into(),
            "claude",
            std::path::Path::new("/home/demo/app"),
            home.join("sb"),
            home,
            1000,
            1000,
            "t",
        );
        let secrets = crate::credentials::secret::SecretFiles::new(None, None);
        let mut out = RunCredentials::default();
        for (name, value) in [("GH_TOKEN", "ghp_file"), ("DB", "db_env")] {
            let c = &all[name];
            deliver(
                &mut spec,
                &secrets,
                &mut out,
                c,
                name.to_string(),
                c.sandbox_var(name),
                value.into(),
            );
        }
        assert!(!spec.env.contains_key("GH_TOKEN"));
        assert!(!spec.env.values().any(|v| v.value == "ghp_file"));
        assert_eq!(spec.env["DB"].value, "db_env");
        assert_eq!(secrets.check("GH_TOKEN").unwrap().as_str(), "ghp_file");
        assert_eq!(out.secret_files, ["GH_TOKEN"]);
        assert_eq!(out.delivered, ["DB"]);
        assert_eq!(out.marks.len(), 2);
    }
}

mod cleanup_tests {
    use super::super::*;
    use crate::backend::{Backend, Launch};
    use crate::build::{build_with, Options};
    use crate::profile::{Harness, Profile};
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    struct MissingProgram;

    impl Backend for MissingProgram {
        fn name(&self) -> &'static str {
            "test"
        }
        fn check(&self, _: &HostEnv, _: &crate::spec::SandboxSpec) -> Result<()> {
            Ok(())
        }
        fn render(
            &self,
            env: &HostEnv,
            spec: &crate::spec::SandboxSpec,
            _: bool,
        ) -> Result<Launch> {
            Ok(Launch {
                program: PathBuf::from("/no-such-paddock-harness"),
                args: vec![],
                env: crate::backend::base_env(env, spec),
                files: vec![],
            })
        }
    }

    #[test]
    fn an_early_return_still_removes_the_run_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let project = tmp.path().join("proj");
        std::fs::create_dir_all(home.join(".config/paddock")).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            home.join(".config/paddock/config.toml"),
            "[devices]\nmount = false\n[kv]\nenabled = false\n",
        )
        .unwrap();
        let mut vars = BTreeMap::new();
        vars.insert("PATH".into(), std::env::var("PATH").unwrap_or_default());
        let env = HostEnv::from_parts(home, "t".into(), 1000, 1000, vars);
        let profile: Profile = toml::from_str("name = \"t\"\n").unwrap();
        let h = Harness {
            profile,
            exe: PathBuf::from("/usr/bin/true"),
            install_mounts: vec![],
            path_dirs: vec![],
            source: "test".into(),
        };
        let opts = Options {
            harness: "t".into(),
            project: Some(project),
            assume_yes: true,
            inherit_env: Some(false),
            ..Default::default()
        };
        let built = build_with(&env, &opts, h).unwrap();
        let id = built.spec.id.clone();
        let run_dir = env.paddock_run().join(&id);
        let borrow_dir = crate::borrow::dir(&env, &id);
        let err = match run(&env, built, &MissingProgram, true, true) {
            Ok(_) => panic!("run succeeded"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("cannot start") || err.to_string().contains("No such file"),
            "{err:#}"
        );
        assert!(
            !run_dir.exists(),
            "run folder left behind: {}",
            run_dir.display()
        );
        assert!(
            !borrow_dir.exists(),
            "borrow folder left behind: {}",
            borrow_dir.display()
        );
    }
}
