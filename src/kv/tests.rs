use super::store::{self, Put, Quota, Store, WriteError, Writer};
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
fn gc_finds_finished_runs_only() {
    let (_d, mut s) = tmp_store();
    let day = 24 * 3600;
    let p = std::path::Path::new("/p");
    s.register("old", "t", p, None, 0).unwrap();
    s.finish("old", day).unwrap();
    s.register("running", "t", p, None, 0).unwrap();
    s.register("parent", "t", p, None, 0).unwrap();
    s.register("child", "t", p, Some("parent"), 0).unwrap();
    for id in ["old", "running", "parent"] {
        let w = Writer {
            sandbox: id,
            harness: "t",
        };
        s.put(&store::ns_self(id), "k", "v", None, None, &w, Q, day)
            .unwrap();
    }
    let cutoff = 40 * day;
    let stale = s
        .stale(cutoff, &["running".into(), "child".into()])
        .unwrap();
    assert_eq!(
        stale.iter().map(|x| x.sandbox.as_str()).collect::<Vec<_>>(),
        ["old"]
    );
    // Recent data stays.
    assert!(s.stale(day, &[]).unwrap().is_empty());
    s.remove_sandboxes(&["old".into()]).unwrap();
    assert!(s.get(&store::ns_self("old"), "k", day).unwrap().is_none());
    assert!(s.parent_of("child").unwrap().as_deref() == Some("parent"));
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
    store: PathBuf,
}

fn world() -> World {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let env = HostEnv::from_parts(home, "u".into(), 1000, 1000, BTreeMap::new());
    let store = dir.path().join("store.db");
    World {
        _dir: dir,
        env,
        store,
    }
}

fn service(
    w: &World,
    id: &str,
    parent: Option<&str>,
    cfg: KvCfg,
    answer: Option<Answer>,
) -> (Service, Arc<Fixed>) {
    let fixed = Arc::new(Fixed {
        answer,
        asked: Mutex::new(Vec::new()),
    });
    let svc = Service::new(
        w.env.clone(),
        Identity {
            sandbox: id.into(),
            harness: "t".into(),
            project: "/src/app".into(),
            parent: parent.map(String::from),
        },
        cfg,
        fixed.clone(),
        Duration::from_secs(1),
    )
    .with_store(w.store.clone());
    Store::open(&w.store)
        .unwrap()
        .register(id, "t", Path::new("/src/app"), parent, crate::state::now())
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
    a.handle(&put(Scope::Project, "note:x", "tip"), None);
    let r = b.handle(&req("get", Scope::Project, "note:x"), None);
    assert_eq!(r.status, Status::Done);
    assert_eq!(r.result["writer"], "a");
    assert_eq!(r.result["untrusted"], true);
    let l = b.handle(
        &Request {
            prefix: Some("note:".into()),
            ..req("list", Scope::Project, "")
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
    let (child, _) = service(&w, "child", Some("a"), KvCfg::default(), None);

    // The owner that the client sends is ignored.
    let r = a.handle(
        &Request {
            owner: Some("b".into()),
            ..claim(Scope::Project, "fix", 60)
        },
        None,
    );
    assert_eq!(r.status, Status::Done);
    assert_eq!(r.result["owner"], "a");
    let held = b.handle(&claim(Scope::Project, "fix", 60), None);
    assert_eq!(held.status, Status::Conflict);
    assert_eq!(held.result["owner"], "a");
    // Renewal by the owner.
    let r = a.handle(&claim(Scope::Project, "fix", 120), None);
    assert_eq!(r.result["renewed"], true);
    // A sibling cannot release it, not even with force.
    let rel = |s: &Service, force: bool| {
        s.handle(
            &Request {
                force: Some(force),
                ..req("release", Scope::Project, "fix")
            },
            None,
        )
    };
    assert_eq!(rel(&b, false).status, Status::Denied);
    assert_eq!(rel(&b, true).status, Status::Denied);
    assert_eq!(rel(&a, false).status, Status::Done);
    assert_eq!(rel(&a, false).status, Status::NotFound);

    // The parent can force the release of a child's lease.
    child.handle(&claim(Scope::Project, "job", 60), None);
    assert_eq!(rel_named(&b, "job", true).status, Status::Denied);
    assert_eq!(rel_named(&a, "job", false).status, Status::Denied);
    assert_eq!(rel_named(&a, "job", true).status, Status::Done);

    // Plain writes cannot forge a lease.
    for r in [
        put(Scope::Project, "lease:fix", "{\"owner\":\"b\"}"),
        req("delete", Scope::Project, "lease:fix"),
        Request {
            entries: Some(BTreeMap::from([("lease:x".into(), "y".into())])),
            ..req("mset", Scope::Project, "")
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

fn rel_named(s: &Service, name: &str, force: bool) -> Response {
    s.handle(
        &Request {
            force: Some(force),
            ..req("release", Scope::Project, name)
        },
        None,
    )
}

#[test]
fn expired_leases_are_free() {
    let w = world();
    let (b, _) = service(&w, "b", None, KvCfg::default(), None);
    let mut st = Store::open(&w.store).unwrap();
    let now = crate::state::now();
    let wa = Writer {
        sandbox: "a",
        harness: "t",
    };
    let ns = store::ns_project(Path::new("/src/app"));
    st.put(
        &ns,
        "lease:old",
        "{}",
        None,
        Some(now - 1),
        &wa,
        Q,
        now - 10,
    )
    .unwrap();
    let r = b.handle(&claim(Scope::Project, "old", 60), None);
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
        b2.handle(&put(Scope::Project, "result:x", "done"), None);
        std::thread::sleep(Duration::from_millis(300));
        b2.handle(&put(Scope::Project, "result:y", "done"), None);
    });
    let start = Instant::now();
    let r = a.handle(
        &Request {
            timeout: Some(10),
            ..req("watch", Scope::Project, "result:x")
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
            ..req("watch", Scope::Project, "")
        },
        None,
    );
    assert_eq!(r.status, Status::Done);
    assert_eq!(r.result["changes"][0]["key"], "result:y");
    t.join().unwrap();
    let r = a.handle(
        &Request {
            timeout: Some(1),
            ..req("watch", Scope::Project, "result:x")
        },
        None,
    );
    assert_eq!(r.status, Status::Timeout);
    // A known version that is old already returns at once.
    let r = a.handle(
        &Request {
            version: Some(0),
            timeout: Some(1),
            ..req("watch", Scope::Project, "result:x")
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
    let path = store_path(&w.env);
    let mut st = Store::open(&path).unwrap();
    let old = crate::state::now() - 100 * 24 * 3600;
    for id in ["gone", "alive"] {
        st.register(id, "t", Path::new("/p"), None, old).unwrap();
        let wr = Writer {
            sandbox: id,
            harness: "t",
        };
        st.put(&store::ns_self(id), "k", "v", None, None, &wr, Q, old)
            .unwrap();
    }
    let plan = gc_plan(&w.env, &["alive".into()], crate::state::now()).unwrap();
    assert_eq!(plan.stale.len(), 1);
    assert_eq!(plan.stale[0].sandbox, "gone");
    let n = gc_apply(&w.env, &["gone".into()], crate::state::now()).unwrap();
    assert_eq!(n, 1);
    assert!(gc_plan(&w.env, &["alive".into()], crate::state::now())
        .unwrap()
        .stale
        .is_empty());
}
