//! `paddock horse` (experimental) on a dsh session file that the test
//! writes itself: ingest, list, log, fork at a node, project the fork.
//! Temporary folders only; no dsh, no network.

use paddock::horse::dsh::{encode_file, read_file, session_dir, Header};
use std::path::Path;
use std::process::Command;

fn cmd(tmp: &Path, args: &[&str]) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_paddock"));
    c.arg("horse")
        .args(args)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", tmp.join("home"))
        .env("XDG_DATA_HOME", tmp.join("data"))
        .env("XDG_STATE_HOME", tmp.join("state"))
        .env("XDG_CONFIG_HOME", tmp.join("config"))
        .env("XDG_CACHE_HOME", tmp.join("cache"))
        .env("XDG_RUNTIME_DIR", tmp.join("run"));
    c
}

fn paddock(tmp: &Path, args: &[&str]) -> String {
    let out = cmd(tmp, args).output().unwrap();
    assert!(
        out.status.success(),
        "paddock horse {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn ingest_fork_and_project_a_dsh_session() {
    let tmp = tempfile::tempdir().unwrap();
    let t = tmp.path();
    let root = t.join("dsh/sessions");
    let header = Header {
        id: "session-11111111-2222-4333-8444-555555555555".into(),
        created_at: 1_793_000_000_000,
        cwd: Some("/work/demo".into()),
        parent_session: None,
        is_seeded: false,
        origin: None,
        delegation_depth: 0,
        agent_preset: None,
    };
    let rows: Vec<Vec<u8>> = [
        r#"{"type":"turn/start","seq":0,"time":1793000000100,"data":{"turn":1}}"#,
        r#"{"type":"user/message","seq":1,"time":1793000000200,"data":{"content":[{"type":"text","text":"hi"}],"source":{"kind":"user"},"role":"user","id":"m1"},"surfaceOp":"append"}"#,
        r#"{"type":"turn/end","seq":2,"time":1793000000300,"data":{"turn":1,"reason":{"kind":"completed"}}}"#,
        r#"{"type":"turn/start","seq":3,"time":1793000000400,"data":{"turn":2}}"#,
    ]
    .iter()
    .map(|r| r.as_bytes().to_vec())
    .collect();
    let dir = session_dir(&root, header.cwd.as_deref(), &header.id);
    std::fs::create_dir_all(&dir).unwrap();
    let refs: Vec<&[u8]> = rows.iter().map(Vec::as_slice).collect();
    std::fs::write(
        dir.join("session.v4.jsonl.zstd"),
        encode_file(&header.encode(), &refs).unwrap(),
    )
    .unwrap();

    let dir_s = dir.to_str().unwrap();
    let err = cmd(t, &["ingest-dsh", dir_s, "--dsh-version", "9.9.9"])
        .output()
        .unwrap();
    assert!(!err.status.success());

    let out = paddock(t, &["ingest-dsh", dir_s, "--dsh-version", "0.2.1-rc.5"]);
    let sid = out.split(':').next().unwrap().to_string();
    assert!(out.contains("4 new"), "{out}");
    let store = t.join("data/paddock/horse");
    assert_eq!(
        std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(&store).unwrap().permissions())
            & 0o777,
        0o700
    );

    let list: serde_json::Value = serde_json::from_str(&paddock(t, &["list", "--json"])).unwrap();
    assert_eq!(list[0]["binding"]["native_id"], header.id.as_str());
    let log = paddock(t, &["log", &sid]);
    assert!(log.contains("user: hi"), "{log}");
    assert!(log.contains("seq:3"), "{log}");
    assert!(paddock(t, &["show", &sid]).contains("dsh 0.2.1-rc.5"));

    let fork = paddock(t, &["fork", &sid, "--at", "seq:2"])
        .trim()
        .to_string();
    let p = paddock(t, &["project-dsh", &fork, "--root", root.to_str().unwrap()]);
    let path = p.split_whitespace().nth(1).unwrap();
    let log = read_file(Path::new(path)).unwrap();
    assert_eq!(
        log.header.parent_session.as_deref(),
        Some(header.id.as_str())
    );
    assert!(log.header.is_seeded);
    assert_eq!(&log.rows[..3], &rows[..3]);
    assert_eq!(
        log.rows[3],
        br#"{"type":"session/end-seed","seq":3,"time":1793000000300,"data":{"inherited":true}}"#
    );
}
