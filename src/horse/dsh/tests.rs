//! Tests of the dsh codec, with a parent session built by hand in the
//! shapes of dsh's own fixtures (`snapshots/web/*/session.v4.jsonl` at the
//! tag), plus `seq` and `time` as the backend writes them.

use super::request::{build, derive_messages, prompt_pieces, J};
use super::*;
use crate::horse::store::Store;

const CWD: &str = "/work/demo";
const PARENT_ID: &str = "session-0b9e3f52-7a1c-4f0e-9d55-2c6a8e1b4d70";
const T0: u64 = 1_793_000_000_000;

const SYSTEM: &str = r#"{"turn":1,"step":1,"message":{"role":"system","content":[{"type":"text","text":"You are DeepSeek Harness. Answer in short sentences."}],"source":{"kind":"system-prompt"},"id":"msg-sys"}}"#;
const USER_1: &str = r#"{"content":[{"type":"text","text":"Read notes.txt and say what it holds."}],"source":{"kind":"user","rpcId":"rpc-1","clientTimeZone":"UTC"},"role":"user","id":"msg-u1"}"#;
const RUNTIME_1: &str = r#"{"content":[{"type":"text","text":"Current runtime context. This snapshot supersedes earlier runtime-context snapshots.\n\nCurrent working directory: \"/work/demo\"."}],"source":{"kind":"runtime-context","form":"snapshot","sections":[{"name":"working-directory:current","text":"Current working directory: \"/work/demo\"."}]},"role":"user","id":"msg-rc1"}"#;
const HEADER_1: &str = r#"{"header":{"config":{"provider":"deepseek-messages","model":"deepseek-flash","maxTokens":256000,"reasoningEffort":"high"},"adapterDefaults":{"reasoningEffort":true,"maxTokens":true},"tools":[{"name":"read","description":"Read a file. Lines are numbered.","parameters":{"type":"object","properties":{"file_path":{"type":"string","description":"Absolute path"},"offset":{"type":"integer","minimum":1}},"required":["file_path"],"additionalProperties":false}}]},"reason":"initial"}"#;
const CONTEXT_1: &str = r#"{"provider":"deepseek-messages","model":"deepseek-flash","contextWindow":1000000,"systemPromptUpdate":"in-history"}"#;
const ASSISTANT_1: &str = r#"{"turn":1,"step":1,"message":{"role":"assistant","content":[{"type":"reasoning","text":"I need the file. 先读文件。"},{"type":"tool-call","id":"call_00_a1","name":"read","arguments":"{\"offset\": 1, \"file_path\": \"/work/demo/notes.txt\"}"}],"source":{"kind":"model","provider":"deepseek-messages","model":"deepseek-flash","replayState":{"response":{"kind":"deepseek-messages","version":1,"model":"deepseek-flash"},"blocks":[{"type":"reasoning","signature":"6f1c2d4e-sig-one"},{"type":"tool-call"}]}},"id":"msg-a1"},"usage":{"inputTokens":6885,"outputTokens":31,"cacheReadTokens":0,"cacheWriteTokens":0,"totalTokens":6916}}"#;
const CALL_1: &str = r#"{"turn":1,"step":1,"callId":"call_00_a1","name":"read","arguments":"{\"offset\": 1, \"file_path\": \"/work/demo/notes.txt\"}"}"#;
const RESULT_1: &str = r#"{"turn":1,"step":1,"message":{"role":"tool","source":{"kind":"tool","callId":"call_00_a1"},"toolCallId":"call_00_a1","content":[{"type":"text","text":"<path>/work/demo/notes.txt</path>\n<content>\n1: the horse store works\n</content>"}],"isError":false,"id":"msg-t1"}}"#;
const ASSISTANT_2: &str = r#"{"turn":1,"step":2,"message":{"role":"assistant","content":[{"type":"reasoning","text":""},{"type":"text","text":"notes.txt says: the horse store works."}],"source":{"kind":"model","provider":"deepseek-messages","model":"deepseek-flash","replayState":{"response":{"kind":"deepseek-messages","version":1,"model":"deepseek-flash"},"blocks":[{"type":"reasoning","signature":"9a7b-sig-two"},{"type":"text"}]}},"id":"msg-a2"},"usage":{"inputTokens":40,"outputTokens":12,"cacheReadTokens":6912,"cacheWriteTokens":0,"totalTokens":6964}}"#;
const USER_2: &str = r#"{"content":[{"type":"text","text":"Now count its words."}],"source":{"kind":"user","rpcId":"rpc-2","clientTimeZone":"UTC"},"role":"user","id":"msg-u2"}"#;
const TIME_2: &str = r#"{"content":[{"type":"text","text":"Time sampled while preparing turn 2, step 1: 2026-11-04T02:00:00.000Z"}],"source":{"kind":"time-context","form":"snapshot","sections":[{"name":"time-context","text":"Time sampled while preparing turn 2, step 1: 2026-11-04T02:00:00.000Z"}]},"role":"user","id":"msg-tc2"}"#;
const ASSISTANT_3: &str = r#"{"turn":2,"step":1,"message":{"role":"assistant","content":[{"type":"text","text":"Four words."}],"source":{"kind":"model","provider":"deepseek-messages","model":"deepseek-flash"},"id":"msg-a3"},"usage":{"inputTokens":30,"outputTokens":3,"cacheReadTokens":6970,"cacheWriteTokens":0,"totalTokens":7003}}"#;

/// One row as `Session.append` builds it: type, seq, time, data, then the
/// surface metadata.
fn row(ty: &str, seq: usize, data: &str, surface: bool) -> Vec<u8> {
    let s = if surface {
        r#","surfaceOp":"append""#
    } else {
        ""
    };
    format!(
        r#"{{"type":"{ty}","seq":{seq},"time":{},"data":{data}{s}}}"#,
        T0 + seq as u64 * 100
    )
    .into_bytes()
}

/// Rows from `seq`, in order.
fn rows_from(seq: usize, spec: &[(&str, &str, bool)]) -> Vec<Vec<u8>> {
    spec.iter()
        .enumerate()
        .map(|(i, (t, d, s))| row(t, seq + i, d, *s))
        .collect()
}

/// Turn 1 of the parent: setup, a tool round, the answer. The last row is
/// the `turn/end` at seq 17, the fork point.
fn turn_1() -> Vec<Vec<u8>> {
    rows_from(
        0,
        &[
            (
                "permission/preset",
                r#"{"preset":"workspace-write"}"#,
                false,
            ),
            ("sandbox/mode", r#"{"mode":"workspace-write"}"#, false),
            ("approval/policy", r#"{"policy":"ask"}"#, false),
            ("turn/start", r#"{"turn":1}"#, false),
            ("step/start", r#"{"turn":1,"step":1}"#, false),
            ("system/message", SYSTEM, true),
            ("user/message", USER_1, true),
            ("user/message", RUNTIME_1, true),
            ("request/header", HEADER_1, false),
            ("request/context", CONTEXT_1, false),
            ("assistant/message", ASSISTANT_1, true),
            ("tool/call", CALL_1, false),
            ("tool/result", RESULT_1, true),
            ("step/end", r#"{"turn":1,"step":1}"#, false),
            ("step/start", r#"{"turn":1,"step":2}"#, false),
            ("assistant/message", ASSISTANT_2, true),
            ("step/end", r#"{"turn":1,"step":2}"#, false),
            (
                "turn/end",
                r#"{"turn":1,"reason":{"kind":"completed"}}"#,
                false,
            ),
        ],
    )
}

const B: usize = 17;

/// The rows of a new turn 2 from `seq`, up to the moment before its first
/// request. `resume` adds what dsh appends when it resumes a session.
fn turn_2_input(seq: usize, resume: bool) -> Vec<Vec<u8>> {
    let mut spec = vec![];
    if resume {
        let h = HEADER_1.replace(r#""reason":"initial""#, r#""reason":"resume""#);
        spec.push((
            "request/header",
            Box::leak(h.into_boxed_str()) as &str,
            false,
        ));
    }
    spec.extend([
        ("turn/start", r#"{"turn":2}"#, false),
        ("step/start", r#"{"turn":2,"step":1}"#, false),
        ("user/message", USER_2, true),
        ("user/message", TIME_2, true),
    ]);
    rows_from(seq, &spec)
}

fn parent_rows() -> Vec<Vec<u8>> {
    let mut r = turn_1();
    r.extend(turn_2_input(B + 1, false));
    r.extend(rows_from(
        r.len(),
        &[
            ("assistant/message", ASSISTANT_3, true),
            ("step/end", r#"{"turn":2,"step":1}"#, false),
            (
                "turn/end",
                r#"{"turn":2,"reason":{"kind":"completed"}}"#,
                false,
            ),
        ],
    ));
    r
}

fn parent_header() -> Header {
    Header {
        id: PARENT_ID.into(),
        created_at: T0 - 5,
        cwd: Some(CWD.into()),
        parent_session: None,
        is_seeded: false,
        origin: None,
        delegation_depth: 0,
        agent_preset: Some("standard".into()),
    }
}

/// Write a parent session file under a dsh sessions root.
fn write_parent(root: &Path, rows: &[Vec<u8>]) -> PathBuf {
    let h = parent_header();
    let dir = session_dir(root, h.cwd.as_deref(), &h.id);
    std::fs::create_dir_all(&dir).unwrap();
    let refs: Vec<&[u8]> = rows.iter().map(Vec::as_slice).collect();
    let path = dir.join(FILE_ZSTD);
    std::fs::write(&path, encode_file(&h.encode(), &refs).unwrap()).unwrap();
    path
}

fn parse_all(rows: &[Vec<u8>]) -> Vec<J> {
    rows.iter().map(|r| J::parse(r).unwrap()).collect()
}

fn store() -> (tempfile::TempDir, Store) {
    let t = tempfile::tempdir().unwrap();
    let s = Store::open(&t.path().join("horse")).unwrap();
    (t, s)
}

#[test]
fn header_encodes_in_dsh_key_order_and_parses_back() {
    let h = parent_header();
    let raw = h.encode();
    assert_eq!(
        std::str::from_utf8(&raw).unwrap(),
        r#"{"type":"session","version":4,"id":"session-0b9e3f52-7a1c-4f0e-9d55-2c6a8e1b4d70","createdAt":1792999999995,"cwd":"/work/demo","isSeeded":false,"delegationDepth":0,"agentPreset":"standard"}"#
    );
    assert_eq!(Header::parse(&raw).unwrap(), h);
    let extra = br#"{"type":"session","version":4,"id":"x","createdAt":1,"isSeeded":false,"delegationDepth":0,"sandboxMode":"a"}"#;
    assert!(Header::parse(extra).is_err());
    let relative = br#"{"type":"session","version":4,"id":"x","createdAt":1,"cwd":"a","isSeeded":false,"delegationDepth":0}"#;
    assert!(Header::parse(relative).is_err());
}

#[test]
fn paths_follow_dsh() {
    assert_eq!(project_key("/home/demo/my proj"), "--home-demo-my~0020proj--");
    assert_eq!(project_key("C:\\a\\\\b"), "--C-a-b--");
    assert_eq!(project_key("/"), "--root--");
    assert_eq!(encode_segment("session-ab_1.x"), "session-ab_1.x");
    assert_eq!(encode_segment("a/../~b"), "a~002F..~002F~007Eb");
    assert_eq!(encode_segment(".."), "~002E~002E");
    assert_eq!(encode_segment("é😀"), "~00E9~D83D~DE00");
    let d = session_dir(Path::new("/r"), Some(CWD), "s-1");
    assert_eq!(d, Path::new("/r/--work-demo--/s-1"));
    assert_eq!(
        session_dir(Path::new("/r"), None, "s"),
        Path::new("/r/_no-cwd/s")
    );
}

#[test]
fn version_pin_refuses_other_versions() {
    let (t, st) = store();
    let path = write_parent(&t.path().join("sessions"), &turn_1());
    let err = ingest(&st, &path, "0.2.2-alpha.3", None).unwrap_err();
    assert!(err.to_string().contains("not supported"), "{err}");
    assert!(st.sessions().unwrap().is_empty());
    // A format 5 header is refused even under the pinned version.
    let v5 = br#"{"type":"session","version":5,"id":"x","createdAt":1,"isSeeded":false,"delegationDepth":0}"#;
    assert!(Header::parse(v5)
        .unwrap_err()
        .to_string()
        .contains("format 5"));
    // A projection of a session bound to another version is refused.
    let r = ingest(&st, &path, VERSION, None).unwrap();
    st.update_session(&r.session, |s| s.binding.version = "0.2.0".into())
        .unwrap();
    let err = project(&st, &r.session, &t.path().join("out"), 1).unwrap_err();
    assert!(err.to_string().contains("not supported"), "{err}");
}

#[test]
fn ingest_then_project_gives_the_same_rows_and_file_content() {
    let (t, st) = store();
    let rows = parent_rows();
    let path = write_parent(&t.path().join("sessions"), &rows);
    let r = ingest(&st, path.parent().unwrap(), VERSION, None).unwrap();
    assert_eq!(
        (r.added, r.kept, r.dropped, r.torn),
        (rows.len(), 0, 0, false)
    );
    let s = st.session(&r.session).unwrap();
    assert_eq!(s.binding.native_id.as_deref(), Some(PARENT_ID));
    let nodes = st.session_nodes(&s).unwrap();
    assert_eq!(nodes.len(), rows.len());
    for (i, n) in nodes.iter().enumerate() {
        assert_eq!(n.raw, rows[i]);
        assert_eq!(n.meta.key.as_deref(), Some(format!("seq:{i}").as_str()));
        assert_eq!(n.meta.version.as_deref(), Some(VERSION));
        assert_eq!(n.meta.container.as_deref(), Some(CONTAINER));
    }
    // The same file again adds nothing.
    let again = ingest(&st, &path, VERSION, None).unwrap();
    assert_eq!(
        (again.added, again.kept, again.session),
        (0, rows.len(), r.session.clone())
    );

    let out = t.path().join("out");
    let p = project(&st, &r.session, &out, 1).unwrap();
    assert!(!p.forked);
    assert_eq!(
        p.path,
        session_dir(&out, Some(CWD), PARENT_ID).join(FILE_ZSTD)
    );
    let a = zframe::decode(&std::fs::read(&path).unwrap()).unwrap();
    let b = zframe::decode(&std::fs::read(&p.path).unwrap()).unwrap();
    assert_eq!(a, b);
    // The file is never replaced.
    assert!(project(&st, &r.session, &out, 1).is_err());
}

#[test]
fn a_truncated_tail_and_a_new_file_keep_the_rows() {
    let (t, st) = store();
    let rows = parent_rows();
    let root = t.path().join("sessions");
    let path = write_parent(&root, &rows);
    let r = ingest(&st, &path, VERSION, None).unwrap();

    // A torn last frame (a crash during a write) shortens the file.
    let bytes = std::fs::read(&path).unwrap();
    std::fs::write(&path, &bytes[..bytes.len() - 7]).unwrap();
    let torn = ingest(&st, &path, VERSION, None).unwrap();
    assert!(torn.torn);
    assert_eq!((torn.kept, torn.added, torn.dropped), (0, 0, 0));
    let s = st.session(&r.session).unwrap();
    assert_eq!(st.session_nodes(&s).unwrap().len(), rows.len());
    // The head waits for the repair. dsh rewrites the recovered rows in a new
    // file (another inode, other frames): ingest by seq finds them again.
    std::fs::remove_file(&path).unwrap();
    let mut body = Vec::new();
    for r in &rows[..10] {
        body.extend_from_slice(r);
        body.push(b'\n');
    }
    let mut file = zframe::encode(&[parent_header().encode(), b"\n".to_vec()].concat()).unwrap();
    file.extend(zframe::encode(&body).unwrap());
    for r in &rows[10..] {
        file.extend(zframe::encode(&[r.clone(), b"\n".to_vec()].concat()).unwrap());
    }
    std::fs::write(&path, file).unwrap();
    let back = ingest(&st, &path, VERSION, None).unwrap();
    assert_eq!((back.kept, back.added, back.dropped), (rows.len(), 0, 0));
    assert_eq!(back.session, r.session);
    let s = st.session(&r.session).unwrap();
    assert_eq!(st.session_nodes(&s).unwrap().len(), rows.len());

    // A rewritten tail branches: the old rows stay as a side branch.
    let mut other = rows[..20].to_vec();
    other.push(row("user/message", 20, USER_1, true));
    std::fs::write(
        &path,
        encode_file(
            &parent_header().encode(),
            &other.iter().map(Vec::as_slice).collect::<Vec<_>>(),
        )
        .unwrap(),
    )
    .unwrap();
    let br = ingest(&st, &path, VERSION, None).unwrap();
    assert_eq!((br.kept, br.added, br.dropped), (20, 1, rows.len() - 20));

    // A plain JSONL generation reads the same, a partial last line is torn.
    let raw = path.with_file_name(FILE_RAW);
    let mut text = [parent_header().encode(), b"\n".to_vec()].concat();
    for r in &other {
        text.extend_from_slice(r);
        text.push(b'\n');
    }
    text.extend_from_slice(b"{\"type\":\"turn/st");
    std::fs::write(&raw, &text).unwrap();
    let log = read_file(&raw).unwrap();
    assert!(log.torn);
    assert_eq!(log.rows, other);
}

#[test]
fn a_newer_generation_is_refused_and_the_store_stays() {
    let (t, st) = store();
    let path = write_parent(&t.path().join("sessions"), &turn_1());
    let dir = path.parent().unwrap();
    std::fs::write(dir.join("session.v3.jsonl.zstd"), b"old").unwrap();
    assert_eq!(current_file(dir).unwrap(), path);
    let r = ingest(&st, dir, VERSION, None).unwrap();
    std::fs::write(dir.join("session.v5.jsonl.zstd"), b"new").unwrap();
    let err = ingest(&st, dir, VERSION, None).unwrap_err();
    assert!(err.to_string().contains("format 5"), "{err}");
    let s = st.session(&r.session).unwrap();
    assert_eq!(st.session_nodes(&s).unwrap().len(), B + 1);
}

/// The fork rows that dsh's `session.fork {atSeq: B}` writes: the events
/// `0..=B` as the same objects, then `{type, seq, time, data}` of the
/// end-seed (`buildForkSeed`), each through `JSON.stringify`.
fn native_fork_rows(parent: &[Vec<u8>], b: usize) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = parse_all(&parent[..=b])
        .iter()
        .map(|j| j.text().into_bytes())
        .collect();
    let time = J::parse(&parent[b]).unwrap().get("time").unwrap().clone();
    let seed = J::Obj(vec![
        ("type".into(), J::Str("session/end-seed".into())),
        ("seq".into(), J::Num((b as u64 + 1).into())),
        ("time".into(), time),
        (
            "data".into(),
            J::Obj(vec![("inherited".into(), J::Bool(true))]),
        ),
    ]);
    out.push(seed.text().into_bytes());
    out
}

#[test]
fn fork_projects_to_a_native_dsh_fork() {
    let (t, st) = store();
    let rows = parent_rows();
    let root = t.path().join("sessions");
    write_parent(&root, &rows);
    let r = ingest(
        &st,
        &session_dir(&root, Some(CWD), PARENT_ID),
        VERSION,
        None,
    )
    .unwrap();
    let s = st.session(&r.session).unwrap();
    let at = st.resolve_node(&s, &format!("seq:{B}")).unwrap();
    let f = st.fork(&s.id, &at, None).unwrap();

    // A cut inside a turn is refused for dsh.
    let mid = st
        .fork(&s.id, &st.resolve_node(&s, "seq:12").unwrap(), None)
        .unwrap();
    let err = project(&st, &mid.id, &root, 7).unwrap_err();
    assert!(err.to_string().contains("turn/end"), "{err}");

    let p = project(&st, &f.id, &root, T0 + 9_999).unwrap();
    assert!(p.forked);
    assert!(p.native_id.starts_with("session-"));
    let log = read_file(&p.path).unwrap();
    assert_eq!(
        log.header,
        Header {
            id: p.native_id.clone(),
            created_at: T0 + 9_999,
            cwd: Some(CWD.into()),
            parent_session: Some(PARENT_ID.into()),
            is_seeded: true,
            origin: None,
            delegation_depth: 0,
            agent_preset: Some("standard".into()),
        }
    );
    // Rows 0..=B byte for byte, then the seed: what dsh's own fork writes.
    assert_eq!(&log.rows[..=B], &rows[..=B]);
    assert_eq!(log.rows, native_fork_rows(&rows, B));
    assert_eq!(
        std::str::from_utf8(&log.rows[B + 1]).unwrap(),
        format!(
            r#"{{"type":"session/end-seed","seq":18,"time":{},"data":{{"inherited":true}}}}"#,
            T0 + 1700
        )
    );
    // The store follows: the fork has its native id and the seed node.
    let f = st.session(&f.id).unwrap();
    assert_eq!(f.binding.native_id.as_deref(), Some(p.native_id.as_str()));
    assert_eq!(st.session_nodes(&f).unwrap().len(), B + 2);
    // Ingest of the projected file finds the same nodes.
    let again = ingest(&st, &p.path, VERSION, None).unwrap();
    assert_eq!((again.session, again.added), (f.id.clone(), 0));
    // A second projection of the same session does not replace the file.
    assert!(project(&st, &f.id, &root, 1).is_err());
}

#[test]
fn view_gives_role_and_text_of_model_visible_rows() {
    let rows = parent_rows();
    assert_eq!(view(&rows[3]), None);
    let v = view(&rows[10]).unwrap();
    assert_eq!(v.role, "assistant");
    assert_eq!(v.text, "I need the file. 先读文件。 [call read]");
    assert_eq!(
        view(&rows[6]).unwrap().text,
        "Read notes.txt and say what it holds."
    );
}

#[test]
fn ordered_json_writes_rows_as_json_stringify_does() {
    for r in parent_rows() {
        assert_eq!(J::parse(&r).unwrap().text().into_bytes(), r);
    }
}

/// The offline proof. dsh builds a request from the log (port in
/// `request.rs`). The request for turn 2 of the parent (the native
/// continuation) and the request for the same turn 2 in the projected fork
/// (after dsh resumes it) are the same bytes. The part from the inherited
/// rows is the same prefix in both, so the content-keyed DeepSeek cache
/// that the parent's turn 1 built serves the fork as it serves the parent.
#[test]
fn projected_fork_sends_the_same_request_prefix_as_the_native_session() {
    let (t, st) = store();
    let rows = parent_rows();
    let root = t.path().join("sessions");
    write_parent(&root, &rows);
    let r = ingest(
        &st,
        &root.join("--work-demo--").join(PARENT_ID),
        VERSION,
        None,
    )
    .unwrap();
    let s = st.session(&r.session).unwrap();
    let f = st
        .fork(
            &s.id,
            &st.resolve_node(&s, &format!("seq:{B}")).unwrap(),
            None,
        )
        .unwrap();
    let p = project(&st, &f.id, &root, T0 + 5_000).unwrap();
    let fork = read_file(&p.path).unwrap().rows;

    // Native continuation: the parent's log just before its turn 2 request.
    let native = &rows[..B + 5];
    let native_body = build(&parse_all(native)).unwrap();
    // The projected fork after resume: dsh adds a resume header, then the
    // same turn 2 input.
    let mut forked = fork.clone();
    forked.extend(turn_2_input(fork.len(), true));
    let fork_body = build(&parse_all(&forked)).unwrap();

    let (a, b) = (native_body.text(), fork_body.text());
    assert_eq!(a, b, "the whole request body is the same");

    // The inherited prefix alone: the messages of rows 0..=B.
    let inherited = build(&parse_all(&rows[..=B])).unwrap();
    let base = prompt_pieces(&inherited);
    let (pa, pb) = (prompt_pieces(&native_body), prompt_pieces(&fork_body));
    assert_eq!(
        base.len(),
        5 + 4,
        "system, tools, model, thinking, effort and 4 messages"
    );
    assert_eq!(&pa[..base.len()], &base[..]);
    assert_eq!(&pb[..base.len()], &base[..]);
    assert!(!base[0].is_empty() && base[1].contains("\"input_schema\""));
    // The cut is a request end of the parent: its turn 1 answer.
    assert!(base.last().unwrap().contains("the horse store works."));
    assert!(a.contains(r#""signature":"6f1c2d4e-sig-one""#));
    assert!(a.contains(r#""input":{"offset":1,"file_path":"/work/demo/notes.txt"}"#));
    assert_eq!(derive_messages(&parse_all(&fork)).unwrap().len(), 6);

    // Negative control: one changed character in an inherited row changes
    // the prompt inside the inherited prefix.
    let mut bad = forked.clone();
    bad[6] = String::from_utf8(bad[6].clone())
        .unwrap()
        .replace("notes.txt", "notes.txT")
        .into_bytes();
    let bad_pieces = prompt_pieces(&build(&parse_all(&bad)).unwrap());
    let first_diff = pa
        .iter()
        .zip(&bad_pieces)
        .position(|(x, y)| x != y)
        .unwrap();
    assert!(first_diff < base.len());
}
