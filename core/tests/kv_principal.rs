//! `paddock kv put --as NAME`: a host principal writes a pasture, the user
//! allows it on the approval card, and a sandbox reads the entry with its
//! writer. Temporary XDG folders only.

use paddock::broker::{Answer, Approver, Pending};
use paddock::env::HostEnv;
use paddock::kv::{Identity, KvCfg, Request, Scope, Service, Status, PROTOCOL};
use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct Never;
impl Approver for Never {
    fn ask(&self, _: &HostEnv, _: &Pending, _: Duration) -> Option<Answer> {
        None
    }
}

fn paddock(root: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_paddock"));
    for (k, d) in [
        ("XDG_CONFIG_HOME", "cfg"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_STATE_HOME", "state"),
        ("XDG_CACHE_HOME", "cache"),
        ("XDG_RUNTIME_DIR", "run"),
    ] {
        c.env(k, root.join(d));
    }
    c.env("HOME", root.join("home"));
    c
}

#[test]
fn a_principal_writes_and_a_sandbox_reads_it() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path();
    let project = root.join("project");
    for d in ["cfg/paddock", "run", "home", "project"] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    std::fs::write(
        root.join("cfg/paddock/config.toml"),
        "[kv.principals.ci]\npastures = [\"review\"]\n",
    )
    .unwrap();
    let put = |value: &str| {
        paddock(root)
            .args([
                "kv",
                "put",
                "task",
                value,
                "--as",
                "ci",
                "--pasture",
                "review",
            ])
            .arg("--project")
            .arg(&project)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    // The first write waits for the user on an approval card.
    let child = put("check the diff");
    let approvals = root.join("run/paddock/approvals");
    let end = Instant::now() + Duration::from_secs(20);
    let id = loop {
        let found = std::fs::read_dir(&approvals)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                e.file_name()
                    .to_str()?
                    .strip_suffix(".json")
                    .map(String::from)
            })
            .next();
        if let Some(id) = found {
            break id;
        }
        assert!(Instant::now() < end, "no approval card");
        std::thread::sleep(Duration::from_millis(50));
    };
    let card = std::fs::read_to_string(approvals.join(format!("{id}.json"))).unwrap();
    assert!(
        card.contains("let `ci` write to the pasture `review`"),
        "{card}"
    );
    let st = paddock(root)
        .args(["approvals", "answer", &id, "session"])
        .status()
        .unwrap();
    assert!(st.success());
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // The session grant covers the second write: no card.
    let out = put("check it again").wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // A sandbox of the pasture reads the value and its writer.
    let mut vars = BTreeMap::new();
    for (k, d) in [
        ("XDG_CONFIG_HOME", "cfg"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_STATE_HOME", "state"),
        ("XDG_RUNTIME_DIR", "run"),
    ] {
        vars.insert(k.to_string(), root.join(d).display().to_string());
    }
    let env = HostEnv::from_parts(root.join("home"), "u".into(), 1000, 1000, vars);
    let resolved = paddock::kv::host::project_of(Some(&project)).unwrap();
    let svc = Service::new(
        env,
        Identity {
            sandbox: "s1".into(),
            harness: "t".into(),
            project: resolved,
            pasture: Some("review".into()),
            parent: None,
            chain: Vec::new(),
            tree: None,
        },
        KvCfg::default(),
        Arc::new(Never),
        Duration::from_secs(1),
    );
    let r = svc.handle(
        &Request {
            v: PROTOCOL,
            op: "get".into(),
            scope: Some(Scope::Pasture),
            key: Some("task".into()),
            ..Default::default()
        },
        None,
    );
    assert_eq!(r.status, Status::Done, "{}", r.message);
    assert_eq!(r.result["value"], "check it again");
    assert_eq!(r.result["writer"], "principal:ci");
    assert_eq!(r.result["untrusted"], true);
}
