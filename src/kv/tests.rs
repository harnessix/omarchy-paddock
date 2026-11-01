use super::shard::{self, Ns};
use super::store::{Put, Quota, Store, WriteError, Writer};
use super::*;
use crate::broker::{Answer, Approver, Pending};
use std::collections::BTreeMap;
use std::sync::Mutex;

const W: Writer = Writer {
    sandbox: "a",
    harness: "t",
};
const Q: Quota = Quota {
    bytes: 1 << 20,
    keys: 100,
};

fn tmp_store() -> (tempfile::TempDir, Store) {
    let d = tempfile::tempdir().unwrap();
    let s = Store::open(&d.path().join("kv/store.db")).unwrap();
    (d, s)
}

#[test]
fn store_file_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let (d, _s) = tmp_store();
    let f = d.path().join("kv/store.db");
    let mode = std::fs::metadata(&f).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let dir = std::fs::metadata(d.path().join("kv"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(dir, 0o700);
}

#[test]
fn compare_and_swap() {
    let (_d, mut s) = tmp_store();
    let v1 = s.put("n", "k", "1", Some(0), None, &W, Q, 10).unwrap();
    // Create-only fails once the key exists.
    match s.put("n", "k", "2", Some(0), None, &W, Q, 10) {
        Err(WriteError::Conflict { current, .. }) => assert_eq!(current, v1),
        other => panic!("{other:?}"),
    }
    let v2 = s.put("n", "k", "2", Some(v1), None, &W, Q, 10).unwrap();
    assert!(v2 > v1);
    assert!(matches!(
        s.put("n", "k", "3", Some(v1), None, &W, Q, 10),
        Err(WriteError::Conflict { .. })
    ));
    assert_eq!(s.get("n", "k", 10).unwrap().unwrap().value, "2");
    assert!(matches!(
        s.delete("n", "k", Some(v1), &W, Q, 10),
        Err(WriteError::Conflict { .. })
    ));
    s.delete("n", "k", Some(v2), &W, Q, 10).unwrap();
    assert!(matches!(
        s.delete("n", "k", None, &W, Q, 10),
        Err(WriteError::Missing)
    ));
    // A key written again never gets an old version back.
    let v3 = s.put("n", "k", "x", None, None, &W, Q, 10).unwrap();
    assert!(v3 > v2);
}

#[test]
fn ttl_expires_keys() {
    let (_d, mut s) = tmp_store();
    s.put("n", "k", "v", None, Some(100), &W, Q, 10).unwrap();
    assert!(s.get("n", "k", 99).unwrap().is_some());
    assert!(s.get("n", "k", 100).unwrap().is_none());
    assert_eq!(s.version("n", "k", 100).unwrap(), 0);
    // An expired key counts as absent for create-only writes.
    s.put("n", "k", "w", Some(0), None, &W, Q, 100).unwrap();
    assert_eq!(s.expired(1000).unwrap().0, 0);
}

#[test]
fn quotas_hold_per_namespace() {
    let (_d, mut s) = tmp_store();
    let q = Quota { bytes: 10, keys: 2 };
    s.put("n", "a", "12345", None, None, &W, q, 1).unwrap();
    assert!(matches!(
        s.put("n", "b", "1234567", None, None, &W, q, 1),
        Err(WriteError::Quota(_))
    ));
    // Replacing a value counts only the new size.
    s.put("n", "a", "123456789", None, None, &W, q, 1).unwrap();
    s.put("m", "a", "123456789", None, None, &W, q, 1).unwrap();
    let q = Quota {
        bytes: 1000,
        keys: 2,
    };
    s.put("o", "a", "1", None, None, &W, q, 1).unwrap();
    s.put("o", "b", "1", None, None, &W, q, 1).unwrap();
    assert!(matches!(
        s.put("o", "c", "1", None, None, &W, q, 1),
        Err(WriteError::Quota(_))
    ));
}

#[test]
fn batches_are_all_or_nothing() {
    let (_d, mut s) = tmp_store();
    let q = Quota {
        bytes: 20,
        keys: 10,
    };
    let puts = [
        Put {
            key: "a",
            value: "1",
            if_version: None,
            expires: None,
        },
        Put {
            key: "b",
            value: "this value is far too long",
            if_version: None,
            expires: None,
        },
    ];
    assert!(s.put_many("n", &puts, &W, q, 1).is_err());
    assert!(s.get("n", "a", 1).unwrap().is_none());
    let v = s.put_many("n", &puts[..1], &W, q, 1).unwrap();
    assert_eq!(v.len(), 1);
}

#[test]
fn list_history_and_changes() {
    let (_d, mut s) = tmp_store();
    for k in ["task:a", "task:b", "note:x", "task:c"] {
        s.put("n", k, "v", None, None, &W, Q, 1).unwrap();
    }
    let (l, more) = s.list("n", "task:", 2, 1).unwrap();
    assert_eq!(
        l.iter().map(|m| m.key.as_str()).collect::<Vec<_>>(),
        ["task:a", "task:b"]
    );
    assert!(more);
    let (l, more) = s.list("n", "task:", 10, 1).unwrap();
    assert_eq!(l.len(), 3);
    assert!(!more);

    let since = s.seq().unwrap();
    s.put("n", "task:a", "w", None, None, &W, Q, 2).unwrap();
    s.delete("n", "task:b", None, &W, Q, 2).unwrap();
    s.put("n", "note:y", "w", None, None, &W, Q, 2).unwrap();
    let c = s.changes("n", "task:", since, 100).unwrap();
    assert_eq!(
        c.iter()
            .map(|c| (c.key.as_str(), c.op.as_str()))
            .collect::<Vec<_>>(),
        [("task:a", "put"), ("task:b", "delete")]
    );
    assert!(c.iter().all(|c| c.value.is_none()));

    for i in 0..40 {
        s.put("n", "h", &i.to_string(), None, None, &W, Q, 3)
            .unwrap();
    }
    let h = s.history("n", "h", 100).unwrap();
    assert_eq!(h.len() as u64, store::HISTORY_PER_KEY);
    assert_eq!(h[0].value.as_deref(), Some("39"));
}

#[test]
fn history_stays_within_the_quota() {
    let (_d, mut s) = tmp_store();
    let q = Quota {
        bytes: 100,
        keys: 10,
    };
    for i in 0..10 {
        s.put("n", &format!("k{i}"), "0123456789", None, None, &W, q, 1)
            .unwrap_or(0);
    }
    let total: usize = (0..10)
        .flat_map(|i| s.history("n", &format!("k{i}"), 16).unwrap())
        .map(|c| c.key.len() + c.value.map_or(0, |v| v.len()))
        .sum();
    assert!(total <= 100, "history holds {total} bytes");
}

#[test]
fn config_defaults_and_limits() {
    let c = KvCfg::parse("").unwrap();
    assert_eq!(c, KvCfg::default());
    assert_eq!(c.user, Mode::Ask);
    let c = KvCfg::parse("[kv]\nuser = \"deny\"\nkeep_days = 7\nmax_value = 1024\n").unwrap();
    assert_eq!(c.user, Mode::Deny);
    assert_eq!(c.keep_days, 7);
    assert_eq!(c.max_value, 1024);
    assert!(KvCfg::parse("[kv]\nkeep_days = 0\n").is_err());
    assert!(KvCfg::parse("[kv]\nunknown = 1\n").is_err());
    // Other tables belong to other readers.
    assert!(KvCfg::parse("[host]\nexec = \"ask\"\n").is_ok());
}

/// The answer source of the tests: a fixed answer, and a count.
struct Fixed {
    answer: Option<Answer>,
    asked: Mutex<Vec<Pending>>,
}

impl Approver for Fixed {
    fn ask(&self, _: &HostEnv, p: &Pending, _: Duration) -> Option<Answer> {
        self.asked.lock().unwrap().push(p.clone());
        self.answer
    }
}

struct World {
    _dir: tempfile::TempDir,
    env: HostEnv,
    /// The kv folder.
    root: PathBuf,
}

impl World {
    fn reg(&self) -> Store {
        Store::open(&shard::registry(&self.root)).unwrap()
    }
    fn open(&self, ns: &Ns) -> Store {
        Store::open(&ns.file(&self.root).unwrap()).unwrap()
    }
}

/// The pasture that the sandboxes of these tests join, unless a test says
/// otherwise.
const TEAM: &str = "team";

fn team() -> Ns {
    Ns::Pasture {
        project: "/src/app".into(),
        name: TEAM.into(),
    }
}

fn world() -> World {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let env = HostEnv::from_parts(home, "u".into(), 1000, 1000, BTreeMap::new());
    let root = kv_root(&env);
    World {
        _dir: dir,
        env,
        root,
    }
}

fn service(
    w: &World,
    id: &str,
    parent: Option<&str>,
    cfg: KvCfg,
    answer: Option<Answer>,
) -> (Service, Arc<Fixed>) {
    let chain: Vec<&str> = parent.into_iter().collect();
    node(w, id, &chain, false, cfg, answer)
}

/// A sandbox in a spawn tree. `chain` holds the sandboxes above it, the root
/// first and the parent last, as the host records them.
fn node(
    w: &World,
    id: &str,
    chain: &[&str],
    can_spawn: bool,
    cfg: KvCfg,
    answer: Option<Answer>,
) -> (Service, Arc<Fixed>) {
    // A spawned sandbox never joins a pasture.
    let pasture = chain.is_empty().then(|| TEAM.to_string());
    in_pasture(w, id, chain, can_spawn, cfg, answer, pasture, "/src/app")
}

#[allow(clippy::too_many_arguments)]
fn in_pasture(
    w: &World,
    id: &str,
    chain: &[&str],
    can_spawn: bool,
    cfg: KvCfg,
    answer: Option<Answer>,
    pasture: Option<String>,
    project: &str,
) -> (Service, Arc<Fixed>) {
    let fixed = Arc::new(Fixed {
        answer,
        asked: Mutex::new(Vec::new()),
    });
    let lineage = Lineage {
        parent: chain.last().map(|p| p.to_string()),
        chain: chain.iter().map(|c| c.to_string()).collect(),
        can_spawn,
    };
    let svc = Service::new(
        w.env.clone(),
        Identity {
            tree: lineage.tree_root(id),
            sandbox: id.into(),
            harness: "t".into(),
            project: project.into(),
            pasture: pasture.clone(),
            parent: lineage.parent.clone(),
            chain: lineage.chain.clone(),
        },
        cfg,
        fixed.clone(),
        Duration::from_secs(1),
    )
    .with_root(w.root.clone());
    w.reg()
        .register(
            id,
            "t",
            Path::new(project),
            chain.last().copied(),
            pasture.as_deref(),
            crate::state::now(),
        )
        .unwrap();
    (svc, fixed)
}

fn req(op: &str, scope: Scope, key: &str) -> Request {
    Request {
        v: PROTOCOL,
        op: op.into(),
        scope: Some(scope),
        key: (!key.is_empty()).then(|| key.to_string()),
        ..Default::default()
    }
}

fn put(scope: Scope, key: &str, value: &str) -> Request {
    Request {
        value: Some(value.into()),
        ..req("put", scope, key)
    }
}

#[test]
fn self_scope_belongs_to_one_sandbox() {
    let w = world();
    let (a, _) = service(&w, "a", None, KvCfg::default(), None);
    let (b, _) = service(&w, "b", None, KvCfg::default(), None);
    assert_eq!(
        a.handle(&put(Scope::Own, "k", "secret"), None).status,
        Status::Done
    );
    assert_eq!(
        b.handle(&req("get", Scope::Own, "k"), None).status,
        Status::NotFound
    );
    let r = a.handle(&req("get", Scope::Own, "k"), None);
    assert_eq!(r.result["value"], "secret");
    assert_eq!(r.result["writer"], "a");
    assert!(r.result.get("untrusted").is_none());
}

#[test]
fn project_entries_record_the_writer() {
    let w = world();
    let (a, _) = service(&w, "a", None, KvCfg::default(), None);
    let (b, _) = service(&w, "b", None, KvCfg::default(), None);
    a.handle(&put(Scope::Pasture, "note:x", "tip"), None);
    let r = b.handle(&req("get", Scope::Pasture, "note:x"), None);
    assert_eq!(r.status, Status::Done);
    assert_eq!(r.result["writer"], "a");
    assert_eq!(r.result["untrusted"], true);
    let l = b.handle(
        &Request {
            prefix: Some("note:".into()),
            ..req("list", Scope::Pasture, "")
        },
        None,
    );
    assert_eq!(l.result["entries"][0]["writer"], "a");
    assert_eq!(l.result["untrusted"], true);
}

#[test]
fn parent_scope_is_read_only_and_needs_a_parent() {
    let w = world();
    let (p, _) = service(&w, "p", None, KvCfg::default(), None);
    let (c, _) = service(&w, "c", Some("p"), KvCfg::default(), None);
    p.handle(&put(Scope::Own, "task", "review"), None);
    let r = c.handle(&req("get", Scope::Parent, "task"), None);
    assert_eq!(r.result["value"], "review");
    assert_eq!(
        c.handle(&put(Scope::Parent, "task", "x"), None).status,
        Status::Denied
    );
    assert_eq!(
        p.handle(&req("get", Scope::Parent, "task"), None).status,
        Status::Disabled
    );
}

#[test]
fn requests_cannot_name_a_sandbox() {
    for line in [
        r#"{"v":1,"op":"get","key":"k","sandbox":"b"}"#,
        r#"{"v":1,"op":"get","key":"k","writer":"b"}"#,
        r#"{"v":1,"op":"get","scope":"self:b","key":"k"}"#,
    ] {
        assert!(serde_json::from_str::<Request>(line).is_err(), "{line}");
    }
}

#[test]
fn the_socket_is_the_identity() {
    let w = world();
    let dir = tempfile::tempdir().unwrap();
    let (a, _) = service(&w, "a", None, KvCfg::default(), None);
    let (b, _) = service(&w, "b", None, KvCfg::default(), None);
    let sa = dir.path().join("a.sock");
    let sb = dir.path().join("b.sock");
    let _ra = serve(&sa, a).unwrap();
    let _rb = serve(&sb, b).unwrap();
    let send = |sock: &Path, line: &str| -> Response {
        let mut s = UnixStream::connect(sock).unwrap();
        writeln!(s, "{line}").unwrap();
        let mut out = String::new();
        BufReader::new(s).read_line(&mut out).unwrap();
        serde_json::from_str(&out).unwrap()
    };
    let r = send(&sa, r#"{"v":1,"op":"put","key":"k","value":"from a"}"#);
    assert_eq!(r.status, Status::Done);
    let r = send(&sb, r#"{"v":1,"op":"get","key":"k"}"#);
    assert_eq!(r.status, Status::NotFound);
    let r = send(&sb, r#"{"v":1,"op":"get","key":"k","sandbox":"a"}"#);
    assert_eq!(r.status, Status::Invalid);
    let r = send(
        &sb,
        r#"{"v":1,"op":"put","key":"k","value":"x","writer":"a"}"#,
    );
    assert_eq!(r.status, Status::Invalid);
    let r = send(&sa, r#"{"v":1,"op":"get","key":"k"}"#);
    assert_eq!(r.result["value"], "from a");
    let info = send(&sb, r#"{"v":1,"op":"info"}"#);
    assert_eq!(info.result["sandbox"], "b");
}

#[test]
fn user_scope_asks_first() {
    let w = world();
    let (a, asked) = service(&w, "a", None, KvCfg::default(), Some(Answer::Deny));
    assert_eq!(
        a.handle(&req("get", Scope::User, "k"), None).status,
        Status::Denied
    );
    assert_eq!(asked.asked.lock().unwrap()[0].capability, USER_CAPABILITY);

    let (b, asked) = service(&w, "b", None, KvCfg::default(), Some(Answer::Session));
    assert_eq!(
        b.handle(&put(Scope::User, "k", "v"), None).status,
        Status::Done
    );
    assert_eq!(
        b.handle(&req("get", Scope::User, "k"), None).status,
        Status::Done
    );
    assert_eq!(asked.asked.lock().unwrap().len(), 1);

    let (c, asked) = service(&w, "c", None, KvCfg::default(), Some(Answer::Once));
    c.handle(&req("get", Scope::User, "k"), None);
    c.handle(&req("get", Scope::User, "k"), None);
    assert_eq!(asked.asked.lock().unwrap().len(), 2);

    let (d, _) = service(&w, "d", None, KvCfg::default(), None);
    assert_eq!(
        d.handle(&req("get", Scope::User, "k"), None).status,
        Status::Timeout
    );

    let off = KvCfg {
        user: Mode::Deny,
        ..KvCfg::default()
    };
    let (e, asked) = service(&w, "e", None, off, Some(Answer::Session));
    assert_eq!(
        e.handle(&req("get", Scope::User, "k"), None).status,
        Status::Disabled
    );
    assert!(asked.asked.lock().unwrap().is_empty());

    let on = KvCfg {
        user: Mode::Allow,
        ..KvCfg::default()
    };
    let (f, asked) = service(&w, "f", None, on, None);
    let r = f.handle(&req("get", Scope::User, "k"), None);
    assert_eq!(r.result["writer"], "b");
    assert_eq!(r.result["untrusted"], true);
    assert!(asked.asked.lock().unwrap().is_empty());
}

#[test]
fn an_invalid_request_asks_nothing() {
    let w = world();
    let (a, asked) = service(&w, "a", None, KvCfg::default(), Some(Answer::Session));
    let r = a.handle(&req("get", Scope::User, "bad\nkey"), None);
    assert_eq!(r.status, Status::Invalid);
    assert!(asked.asked.lock().unwrap().is_empty());
}

fn claim(scope: Scope, name: &str, ttl: u64) -> Request {
    Request {
        ttl: Some(ttl),
        ..req("claim", scope, name)
    }
}

#[test]
fn leases_belong_to_the_sandbox() {
    let w = world();
    let (a, _) = service(&w, "a", None, KvCfg::default(), None);
    let (b, _) = service(&w, "b", None, KvCfg::default(), None);

    // The owner that the client sends is ignored.
    let r = a.handle(
        &Request {
            owner: Some("b".into()),
            ..claim(Scope::Pasture, "fix", 60)
        },
        None,
    );
    assert_eq!(r.status, Status::Done);
    assert_eq!(r.result["owner"], "a");
    let held = b.handle(&claim(Scope::Pasture, "fix", 60), None);
    assert_eq!(held.status, Status::Conflict);
    assert_eq!(held.result["owner"], "a");
    // Renewal by the owner.
    let r = a.handle(&claim(Scope::Pasture, "fix", 120), None);
    assert_eq!(r.result["renewed"], true);
    // A sibling cannot release it, not even with force.
    let rel = |s: &Service, force: bool| {
        s.handle(
            &Request {
                force: Some(force),
                ..req("release", Scope::Pasture, "fix")
            },
            None,
        )
    };
    assert_eq!(rel(&b, false).status, Status::Denied);
    assert_eq!(rel(&b, true).status, Status::Denied);
    assert_eq!(rel(&a, false).status, Status::Done);
    assert_eq!(rel(&a, false).status, Status::NotFound);

    // Plain writes cannot forge a lease.
    for r in [
        put(Scope::Pasture, "lease:fix", "{\"owner\":\"b\"}"),
        req("delete", Scope::Pasture, "lease:fix"),
        Request {
            entries: Some(BTreeMap::from([("lease:x".into(), "y".into())])),
            ..req("mset", Scope::Pasture, "")
        },
    ] {
        assert_eq!(b.handle(&r, None).status, Status::Invalid);
    }
    // A lease must expire.
    assert_eq!(
        a.handle(&req("claim", Scope::Own, "x"), None).status,
        Status::Invalid
    );
}

#[test]
fn expired_leases_are_free() {
    let w = world();
    let (b, _) = service(&w, "b", None, KvCfg::default(), None);
    let mut st = w.open(&team());
    let now = crate::state::now();
    let wa = Writer {
        sandbox: "a",
        harness: "t",
    };
    st.put(
        &team().key(),
        "lease:old",
        "{}",
        None,
        Some(now - 1),
        &wa,
        Q,
        now - 10,
    )
    .unwrap();
    let r = b.handle(&claim(Scope::Pasture, "old", 60), None);
    assert_eq!(r.status, Status::Done, "{}", r.message);
    assert_eq!(r.result["owner"], "b");
}

#[test]
fn batches_and_history() {
    let w = world();
    let (a, _) = service(&w, "a", None, KvCfg::default(), None);
    let r = a.handle(
        &Request {
            entries: Some(BTreeMap::from([
                ("task:a".into(), "1".into()),
                ("task:b".into(), "2".into()),
            ])),
            ..req("mset", Scope::Own, "")
        },
        None,
    );
    assert_eq!(r.status, Status::Done);
    let r = a.handle(
        &Request {
            keys: Some(vec!["task:a".into(), "nope".into(), "task:b".into()]),
            ..req("mget", Scope::Own, "")
        },
        None,
    );
    let e = r.result["entries"].as_array().unwrap();
    assert_eq!(e[0]["value"], "1");
    assert!(e[1].is_null());
    assert_eq!(e[2]["value"], "2");
    a.handle(&put(Scope::Own, "task:a", "3"), None);
    let h = a.handle(&req("history", Scope::Own, "task:a"), None);
    let h = h.result["history"].as_array().unwrap();
    assert_eq!(h.len(), 2);
    assert_eq!(h[0]["value"], "3");
}

#[test]
fn limits_are_enforced() {
    let w = world();
    let cfg = KvCfg {
        max_value: 8,
        self_bytes: 20,
        ..KvCfg::default()
    };
    let (a, _) = service(&w, "a", None, cfg, None);
    assert_eq!(
        a.handle(&put(Scope::Own, "k", "123456789"), None).status,
        Status::TooLarge
    );
    assert_eq!(
        a.handle(&put(Scope::Own, "k1", "12345678"), None).status,
        Status::Done
    );
    assert_eq!(
        a.handle(&put(Scope::Own, "k2", "12345678"), None).status,
        Status::Done
    );
    assert_eq!(
        a.handle(&put(Scope::Own, "k3", "12345678"), None).status,
        Status::TooLarge
    );
    let long = "k".repeat(MAX_KEY + 1);
    assert_eq!(
        a.handle(&put(Scope::Own, &long, "v"), None).status,
        Status::Invalid
    );
    assert_eq!(
        a.handle(
            &Request {
                ttl: Some(0),
                ..put(Scope::Own, "t", "v")
            },
            None
        )
        .status,
        Status::Invalid
    );
}

#[test]
fn watch_wakes_on_a_change_from_another_sandbox() {
    let w = world();
    let (a, _) = service(&w, "a", None, KvCfg::default(), None);
    let (b, _) = service(&w, "b", None, KvCfg::default(), None);
    let b = Arc::new(b);
    let b2 = b.clone();
    let t = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        b2.handle(&put(Scope::Pasture, "result:x", "done"), None);
        std::thread::sleep(Duration::from_millis(300));
        b2.handle(&put(Scope::Pasture, "result:y", "done"), None);
    });
    let start = Instant::now();
    let r = a.handle(
        &Request {
            timeout: Some(10),
            ..req("watch", Scope::Pasture, "result:x")
        },
        None,
    );
    assert_eq!(r.status, Status::Done);
    assert_eq!(r.result["value"], "done");
    assert_eq!(r.result["writer"], "b");
    assert!(start.elapsed() < Duration::from_secs(5));
    let cursor = r.result["version"].as_u64().unwrap();
    let r = a.handle(
        &Request {
            prefix: Some("result:".into()),
            version: Some(cursor),
            timeout: Some(10),
            ..req("watch", Scope::Pasture, "")
        },
        None,
    );
    assert_eq!(r.status, Status::Done);
    assert_eq!(r.result["changes"][0]["key"], "result:y");
    t.join().unwrap();
    let r = a.handle(
        &Request {
            timeout: Some(1),
            ..req("watch", Scope::Pasture, "result:x")
        },
        None,
    );
    // The time passed first: still waiting, with its own exit code.
    assert_eq!(r.status, Status::Waiting);
    assert_eq!(r.status.exit_code(), crate::waiting::EXIT_STILL_WAITING);
    // A known version that is old already returns at once.
    let r = a.handle(
        &Request {
            version: Some(0),
            timeout: Some(1),
            ..req("watch", Scope::Pasture, "result:x")
        },
        None,
    );
    assert_eq!(r.status, Status::Done);
}

#[test]
fn the_audit_log_keeps_keys_not_values() {
    let w = world();
    let (a, _) = service(&w, "a", None, KvCfg::default(), Some(Answer::Deny));
    a.handle(&put(Scope::Own, "visible-key", "SECRET-VALUE-4711"), None);
    a.handle(&put(Scope::User, "user-key", "SECRET-VALUE-4712"), None);
    a.handle(
        &Request {
            entries: Some(BTreeMap::from([("m".into(), "SECRET-VALUE-4713".into())])),
            ..req("mset", Scope::Own, "")
        },
        None,
    );
    let log = std::fs::read_to_string(audit_file(&w.env)).unwrap();
    assert!(log.contains("visible-key"));
    assert!(log.contains("user-key"));
    assert!(!log.contains("SECRET-VALUE"), "{log}");
}

#[test]
fn disabled_store_answers_disabled() {
    let w = world();
    let cfg = KvCfg {
        enabled: false,
        ..KvCfg::default()
    };
    let (a, _) = service(&w, "a", None, cfg, None);
    assert_eq!(
        a.handle(&req("get", Scope::Own, "k"), None).status,
        Status::Disabled
    );
}

#[test]
fn gc_plan_skips_running_sandboxes() {
    let w = world();
    let old = crate::state::now() - 100 * 24 * 3600;
    // The files were just written, so the test looks from 100 days ahead.
    let later = crate::state::now() + 100 * 24 * 3600;
    for id in ["gone", "alive"] {
        w.reg()
            .register(id, "t", Path::new("/p"), None, None, old)
            .unwrap();
        w.reg().finish(id, old).unwrap();
        let wr = Writer {
            sandbox: id,
            harness: "t",
        };
        for ns in [
            Ns::Own(id.into()),
            Ns::Solo {
                project: "/p".into(),
                sandbox: id.into(),
            },
        ] {
            w.open(&ns)
                .put(&ns.key(), "k", "v", None, None, &wr, Q, old)
                .unwrap();
        }
    }
    let plan = gc_plan(&w.env, &["alive".into()], later).unwrap();
    assert_eq!(plan.stale.len(), 1);
    assert_eq!(plan.stale[0].sandbox, "gone");
    assert_eq!(plan.stale[0].entries, 2);
    // Recent files stay.
    assert!(gc_plan(&w.env, &[], crate::state::now())
        .unwrap()
        .stale
        .is_empty());
    let n = gc_apply(&w.env, &["gone".into()], later).unwrap();
    assert_eq!(n, 2);
    // Its self scope and its own pasture went with its file.
    assert!(!w.root.join("sandbox/gone.db").exists());
    assert!(w.root.join("sandbox/alive.db").exists());
    assert!(w.reg().times("gone").unwrap().is_none());
    assert!(gc_plan(&w.env, &["alive".into()], later)
        .unwrap()
        .stale
        .is_empty());
}

/// A spawn tree: root r started c1 and c2; c1 started g. Another root r2
/// has its own tree.
struct Tree {
    r: Service,
    c1: Service,
    c2: Service,
    g: Service,
    r2: Service,
}

fn tree(w: &World, cfg: KvCfg) -> Tree {
    Tree {
        r: node(w, "r", &[], true, cfg.clone(), None).0,
        c1: node(w, "c1", &["r"], false, cfg.clone(), None).0,
        c2: node(w, "c2", &["r"], false, cfg.clone(), None).0,
        g: node(w, "g", &["r", "c1"], false, cfg.clone(), None).0,
        r2: node(w, "r2", &[], true, cfg, None).0,
    }
}

fn get(s: &Service, scope: Scope, key: &str) -> Response {
    s.handle(&req("get", scope, key), None)
}

#[test]
fn scopes_resolve_by_place_in_the_spawn_tree() {
    let w = world();
    let t = tree(&w, KvCfg::default());
    for s in [&t.r, &t.c1, &t.c2, &t.g] {
        assert_eq!(s.identity().tree.as_deref(), Some("r"));
    }
    assert_eq!(t.r2.identity().tree.as_deref(), Some("r2"));

    // Each sandbox has its own self scope; siblings cannot read it.
    t.c1.handle(&put(Scope::Own, "plan", "c1 plan"), None);
    t.r.handle(&put(Scope::Own, "plan", "root plan"), None);
    assert_eq!(get(&t.c2, Scope::Own, "plan").status, Status::NotFound);
    // parent is the self scope of the sandbox one level up, not of a
    // sibling and not of the root for a grandchild.
    assert_eq!(
        get(&t.c1, Scope::Parent, "plan").result["value"],
        "root plan"
    );
    assert_eq!(
        get(&t.c2, Scope::Parent, "plan").result["value"],
        "root plan"
    );
    assert_eq!(get(&t.g, Scope::Parent, "plan").result["value"], "c1 plan");
    assert_eq!(get(&t.r, Scope::Parent, "plan").status, Status::Disabled);
    // parent is read-only, for every write op.
    for r in [
        put(Scope::Parent, "plan", "x"),
        req("delete", Scope::Parent, "plan"),
        claim(Scope::Parent, "plan", 60),
    ] {
        assert_eq!(t.g.handle(&r, None).status, Status::Denied);
    }
    assert_eq!(get(&t.c1, Scope::Own, "plan").result["value"], "c1 plan");

    // tree is shared by the whole tree, and each entry names its writer.
    let r =
        t.r.handle(&put(Scope::Tree, "task:a", "{\"status\":\"todo\"}"), None);
    assert_eq!(r.status, Status::Done);
    for s in [&t.c1, &t.c2, &t.g] {
        let r = get(s, Scope::Tree, "task:a");
        assert_eq!(r.result["writer"], "r");
        assert_eq!(r.result["untrusted"], true);
    }
    t.g.handle(&put(Scope::Tree, "note:g", "from g"), None);
    assert_eq!(get(&t.r, Scope::Tree, "note:g").result["writer"], "g");
    // Another tree does not see it.
    assert_eq!(get(&t.r2, Scope::Tree, "task:a").status, Status::NotFound);

    // A sandbox that cannot spawn and was not spawned has no tree.
    let (lone, _) = node(&w, "lone", &[], false, KvCfg::default(), None);
    assert_eq!(get(&lone, Scope::Tree, "task:a").status, Status::Disabled);
    let info = lone.handle(&req("info", Scope::Own, ""), None);
    assert!(info.result["scopes"]["tree"]
        .as_str()
        .unwrap()
        .starts_with("none"));
    let info = t.g.handle(&req("info", Scope::Own, ""), None);
    assert_eq!(info.result["tree_root"], "r");
    assert_eq!(info.result["started_by"][1], "c1");
}

#[test]
fn tree_leases_work_across_siblings_and_only_ancestors_force_them() {
    let w = world();
    let t = tree(&w, KvCfg::default());
    let rel = |s: &Service, name: &str, force: bool| {
        s.handle(
            &Request {
                force: Some(force),
                ..req("release", Scope::Tree, name)
            },
            None,
        )
        .status
    };
    // Two siblings race for one task: one wins.
    let won = t.c1.handle(&claim(Scope::Tree, "a", 60), None);
    assert_eq!(won.status, Status::Done);
    let lost = t.c2.handle(&claim(Scope::Tree, "a", 60), None);
    assert_eq!(lost.status, Status::Conflict);
    assert_eq!(lost.result["owner"], "c1");
    // A sibling, and a child of the owner, cannot free it.
    assert_eq!(rel(&t.c2, "a", true), Status::Denied);
    assert_eq!(rel(&t.g, "a", true), Status::Denied);
    // The root can, with force only.
    assert_eq!(rel(&t.r, "a", false), Status::Denied);
    assert_eq!(rel(&t.r, "a", true), Status::Done);
    // A grandchild's lease: its parent and the root can force it; its
    // parent's sibling cannot.
    t.g.handle(&claim(Scope::Tree, "b", 60), None);
    assert_eq!(rel(&t.c2, "b", true), Status::Denied);
    assert_eq!(rel(&t.c1, "b", true), Status::Done);
    t.g.handle(&claim(Scope::Tree, "b", 60), None);
    assert_eq!(rel(&t.r, "b", true), Status::Done);
    // Results with compare-and-swap: the second create-only write loses.
    let create = |s: &Service| {
        s.handle(
            &Request {
                version: Some(0),
                ..put(Scope::Tree, "result:a", "done")
            },
            None,
        )
        .status
    };
    assert_eq!(create(&t.c1), Status::Done);
    assert_eq!(create(&t.c2), Status::Conflict);
}

#[test]
fn a_child_has_no_pasture_scope() {
    let w = world();
    let t = tree(&w, KvCfg::default());
    t.r.handle(&put(Scope::Pasture, "note:x", "tip"), None);
    for c in [&t.c1, &t.g] {
        assert_eq!(get(c, Scope::Pasture, "note:x").status, Status::Disabled);
        for r in [
            put(Scope::Pasture, "note:x", "mine"),
            req("delete", Scope::Pasture, "note:x"),
            claim(Scope::Pasture, "job", 60),
            Request {
                entries: Some(BTreeMap::from([("k".into(), "v".into())])),
                ..req("mset", Scope::Pasture, "")
            },
        ] {
            assert_eq!(c.handle(&r, None).status, Status::Disabled);
        }
    }
    assert_eq!(get(&t.r, Scope::Pasture, "note:x").result["value"], "tip");
    // The switch of older versions still loads, and changes nothing.
    let cfg = KvCfg::parse("[kv]\nchild_project_write = true\n").unwrap();
    let (c, _) = node(&w, "c3", &["r"], false, cfg, None);
    let r = c.handle(&put(Scope::Pasture, "note:y", "ok"), None);
    assert_eq!(r.status, Status::Disabled);
    assert!(r.message.contains("tree"), "{}", r.message);
}

#[test]
fn the_tree_is_read_only_after_its_root_ends_and_gc_removes_it() {
    let w = world();
    let t = tree(&w, KvCfg::default());
    t.c1.handle(&put(Scope::Tree, "result:a", "done"), None);
    w.reg().finish("r", 1000).unwrap();
    let late = t.c1.handle(&put(Scope::Tree, "result:b", "late"), None);
    assert_eq!(late.status, Status::Denied);
    // The copy stays for review.
    assert_eq!(get(&t.c1, Scope::Tree, "result:a").result["value"], "done");
    // A running descendant keeps the root's data; without one it is stale.
    let far = crate::state::now() + 100 * 24 * 3600;
    let ids = |running: &[String]| {
        gc_plan(&w.env, running, far)
            .unwrap()
            .stale
            .into_iter()
            .map(|s| s.sandbox)
            .collect::<Vec<_>>()
    };
    assert!(!ids(&["g".into()]).contains(&"r".to_string()));
    assert!(ids(&[]).contains(&"r".to_string()));
    gc_apply(&w.env, &["r".into()], far).unwrap();
    assert!(!w.root.join("tree/r.db").exists());
}

#[test]
fn a_child_inherits_the_user_scope_grant_and_never_asks_itself() {
    let w = world();
    let cfg = KvCfg::default();
    let (r, asked_r) = node(&w, "r", &[], true, cfg.clone(), Some(Answer::Session));
    let (c1, asked_c1) = node(&w, "c1", &["r"], false, cfg.clone(), Some(Answer::Session));
    let (g, asked_g) = node(
        &w,
        "g",
        &["r", "c1"],
        false,
        cfg.clone(),
        Some(Answer::Session),
    );
    // Before the root has a grant, a child is refused without a question.
    let resp = get(&g, Scope::User, "k");
    assert_eq!(resp.status, Status::Denied);
    assert!(asked_g.asked.lock().unwrap().is_empty());
    // The root allows it for its session; the whole tree inherits it.
    assert_eq!(get(&r, Scope::User, "k").status, Status::NotFound);
    assert_eq!(asked_r.asked.lock().unwrap().len(), 1);
    assert_eq!(get(&c1, Scope::User, "k").status, Status::NotFound);
    assert!(asked_c1.asked.lock().unwrap().is_empty());
    let (g2, asked_g2) = node(
        &w,
        "g2",
        &["r", "c1"],
        false,
        cfg.clone(),
        Some(Answer::Deny),
    );
    assert_eq!(get(&g2, Scope::User, "k").status, Status::NotFound);
    assert!(asked_g2.asked.lock().unwrap().is_empty());
    // Children of a root without a grant never ask, whatever they would answer.
    let (r2, _) = node(&w, "r2", &[], true, cfg.clone(), Some(Answer::Deny));
    let (s1, asked_s1) = node(&w, "s1", &["r2"], false, cfg.clone(), Some(Answer::Session));
    assert_eq!(get(&s1, Scope::User, "k").status, Status::Denied);
    assert!(asked_s1.asked.lock().unwrap().is_empty());
    assert_eq!(get(&r2, Scope::User, "k").status, Status::Denied);
    // The grant ends with the root's session.
    w.reg().finish("r", 1).unwrap();
    let (c3, asked_c3) = node(&w, "c3", &["r"], false, cfg, Some(Answer::Session));
    assert_eq!(get(&c3, Scope::User, "k").status, Status::Denied);
    assert!(asked_c3.asked.lock().unwrap().is_empty());
}

#[test]
fn a_prefix_watch_wakes_at_once_and_when_a_lease_runs_out() {
    let w = world();
    let (a, _) = service(&w, "a", None, KvCfg::default(), None);
    let (b, _) = service(&w, "b", None, KvCfg::default(), None);
    let b = Arc::new(b);
    // A write from another sandbox wakes a prefix watch within a poll.
    let b2 = b.clone();
    let wrote = Arc::new(Mutex::new(None::<Instant>));
    let wrote2 = wrote.clone();
    let t = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(500));
        *wrote2.lock().unwrap() = Some(Instant::now());
        b2.handle(&put(Scope::Pasture, "result:z", "done"), None);
    });
    let r = a.handle(
        &Request {
            prefix: Some("result:".into()),
            timeout: Some(10),
            ..req("watch", Scope::Pasture, "")
        },
        None,
    );
    let woke = Instant::now();
    t.join().unwrap();
    assert_eq!(r.status, Status::Done);
    assert_eq!(r.result["changes"][0]["key"], "result:z");
    let lag = woke.duration_since(wrote.lock().unwrap().unwrap());
    assert!(
        lag < Duration::from_millis(600),
        "woke {lag:?} after the write"
    );
    // A lease of another sandbox expires: a watch on lease: wakes for it.
    let claim = Request {
        ttl: Some(2),
        ..req("claim", Scope::Pasture, "job")
    };
    assert_eq!(b.handle(&claim, None).status, Status::Done);
    let cursor = r.result["cursor"].as_u64().unwrap() + 1;
    let start = Instant::now();
    let r = a.handle(
        &Request {
            prefix: Some("lease:".into()),
            version: Some(cursor),
            timeout: Some(10),
            ..req("watch", Scope::Pasture, "")
        },
        None,
    );
    assert_eq!(r.status, Status::Done, "{:?}", r.message);
    assert_eq!(r.result["changes"][0]["key"], "lease:job");
    assert_eq!(r.result["changes"][0]["op"], "expire");
    assert!(start.elapsed() < Duration::from_secs(4));
    // The lease is free now.
    let (c, _) = service(&w, "c", None, KvCfg::default(), None);
    assert_eq!(c.handle(&claim, None).status, Status::Done);
}

#[test]
fn an_expiry_between_two_watches_is_not_lost() {
    let w = world();
    let (a, _) = service(&w, "a", None, KvCfg::default(), None);
    let (b, _) = service(&w, "b", None, KvCfg::default(), None);
    // The first watch ends with a cursor.
    assert_eq!(
        b.handle(&put(Scope::Pasture, "lease-x", "1"), None).status,
        Status::Done
    );
    let r = a.handle(
        &Request {
            prefix: Some("lease:".into()),
            version: Some(0),
            timeout: Some(1),
            ..req("watch", Scope::Pasture, "")
        },
        None,
    );
    let cursor = r.result["cursor"].as_u64().unwrap_or(0);
    // While no watch runs, a lease is taken and runs out, and nobody writes.
    let claim = Request {
        ttl: Some(1),
        ..req("claim", Scope::Pasture, "job")
    };
    assert_eq!(b.handle(&claim, None).status, Status::Done);
    std::thread::sleep(Duration::from_millis(2200));
    // The next watch with the cursor sees the claim and its expiry at once.
    let start = Instant::now();
    let r = a.handle(
        &Request {
            prefix: Some("lease:".into()),
            version: Some(cursor),
            timeout: Some(10),
            ..req("watch", Scope::Pasture, "")
        },
        None,
    );
    assert_eq!(r.status, Status::Done, "{:?}", r.message);
    let ops: Vec<&str> = r.result["changes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["key"] == "lease:job")
        .map(|c| c["op"].as_str().unwrap())
        .collect();
    assert_eq!(ops.last(), Some(&"expire"), "{}", r.result);
    assert!(start.elapsed() < Duration::from_secs(2));
}

#[test]
fn sandboxes_without_a_pasture_share_nothing() {
    let w = world();
    let solo =
        |id: &str| in_pasture(&w, id, &[], false, KvCfg::default(), None, None, "/src/app").0;
    let (a, b) = (solo("a"), solo("b"));
    assert_eq!(
        a.handle(&put(Scope::Pasture, "note:x", "a's"), None).status,
        Status::Done
    );
    // The other sandbox of the same project sees nothing, through either name.
    assert_eq!(get(&b, Scope::Pasture, "note:x").status, Status::NotFound);
    let old_name: Request =
        serde_json::from_str(r#"{"v":1,"op":"get","scope":"project","key":"note:x"}"#).unwrap();
    assert_eq!(b.handle(&old_name, None).status, Status::NotFound);
    assert_eq!(a.handle(&old_name, None).result["value"], "a's");
    // It lives in the sandbox's own file, next to its self scope.
    let st = w.open(&Ns::Own("a".into()));
    let ns = Ns::Solo {
        project: "/src/app".into(),
        sandbox: "a".into(),
    };
    assert!(st
        .get(&ns.key(), "note:x", crate::state::now())
        .unwrap()
        .is_some());
    assert!(!w.root.join("pasture").exists());
    let info = a.handle(&req("info", Scope::Own, ""), None);
    assert!(info.result["scopes"]["pasture"]
        .as_str()
        .unwrap()
        .contains("this sandbox only"));
}

#[test]
fn members_of_one_pasture_share_it_and_no_one_else_does() {
    let w = world();
    let mk = |id: &str, pasture: Option<&str>, project: &str| {
        in_pasture(
            &w,
            id,
            &[],
            false,
            KvCfg::default(),
            None,
            pasture.map(str::to_string),
            project,
        )
        .0
    };
    let a = mk("a", Some("review"), "/src/app");
    let b = mk("b", Some("review"), "/src/app");
    let other_pasture = mk("c", Some("build"), "/src/app");
    let other_project = mk("d", Some("review"), "/src/other");
    let alone = mk("e", None, "/src/app");
    a.handle(&put(Scope::Pasture, "task:1", "todo"), None);
    let r = get(&b, Scope::Pasture, "task:1");
    assert_eq!(r.result["value"], "todo");
    assert_eq!(r.result["writer"], "a");
    assert_eq!(r.result["untrusted"], true);
    for s in [&other_pasture, &other_project, &alone] {
        assert_eq!(get(s, Scope::Pasture, "task:1").status, Status::NotFound);
    }
    // A read leaves no file behind.
    assert_eq!(walk(&w.root.join("pasture")).len(), 1);
    // One file per pasture; the same name in another project is another file.
    other_pasture.handle(&put(Scope::Pasture, "k", "v"), None);
    other_project.handle(&put(Scope::Pasture, "k", "v"), None);
    let files: Vec<_> = walk(&w.root.join("pasture"));
    assert_eq!(files.len(), 3, "{files:?}");
    assert_eq!(
        crate::kv::host::pastures(&w.env, Path::new("/src/app"))
            .unwrap()
            .iter()
            .map(|p| (p.name.clone(), p.keys))
            .collect::<Vec<_>>(),
        [("build".to_string(), 1), ("review".to_string(), 1)]
    );
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if e.path().is_dir() {
            out.extend(walk(&e.path()));
        } else if e.path().extension().is_some_and(|x| x == "db") {
            out.push(e.path());
        }
    }
    out
}

#[test]
fn the_project_scope_of_older_versions_stays_out_of_reach() {
    let w = world();
    let legacy = Ns::Legacy("/src/app".into());
    let mut st = w.open(&legacy);
    st.put(
        &legacy.key(),
        "note:old",
        "from before",
        None,
        None,
        &W,
        Q,
        10,
    )
    .unwrap();
    let (a, _) = service(&w, "a", None, KvCfg::default(), None);
    assert_eq!(get(&a, Scope::Pasture, "note:old").status, Status::NotFound);
    // The host still finds it, and gc removes the whole file once old.
    assert_eq!(
        st.get(&legacy.key(), "note:old", 11)
            .unwrap()
            .unwrap()
            .value,
        "from before"
    );
    let far = crate::state::now() + 100 * 24 * 3600;
    assert!(gc_plan(&w.env, &[], far).unwrap().legacy_bytes.is_some());
    assert!(gc_plan(&w.env, &[], crate::state::now())
        .unwrap()
        .legacy_bytes
        .is_none());
    gc_apply(&w.env, &[], far).unwrap();
    assert!(!shard::legacy(&w.root).exists());
}
