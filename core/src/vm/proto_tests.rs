use super::*;

fn req() -> RunRequest {
    RunRequest {
        v: VERSION,
        token: "0123456789abcdef0123456789abcdef".into(),
        id: "fake-p-1a2b".into(),
        harness: "fake".into(),
        project: PathBuf::from("/Users/u/src/p"),
        network: GuestNet::Proxy,
        args: vec!["--x".into(), "a b".into()],
        tty: true,
        rows: 40,
        cols: 120,
        env: [("TERM".to_string(), "xterm-256color".to_string())].into(),
        shell: false,
        no_yolo: false,
        profile: Some("name = \"fake\"\n".into()),
        owner: Some(501),
    }
}

#[test]
fn a_run_request_is_one_line_and_comes_back() {
    let mut buf = Vec::new();
    write_line(&mut buf, &Request::Run(req())).unwrap();
    assert_eq!(buf.iter().filter(|b| **b == b'\n').count(), 1);
    assert!(
        buf.starts_with(br#"{"op":"run","v":3,"#),
        "{}",
        String::from_utf8_lossy(&buf)
    );
    let back: Request = read_line(&mut buf.as_slice()).unwrap().unwrap();
    assert_eq!(back, Request::Run(req()));
}

#[test]
fn the_lines_have_a_fixed_form() {
    let text = |m: &Request| serde_json::to_string(m).unwrap();
    assert_eq!(text(&Request::Ping), r#"{"op":"ping"}"#);
    assert_eq!(
        text(&Request::Resize { rows: 24, cols: 80 }),
        r#"{"op":"resize","rows":24,"cols":80}"#
    );
    assert_eq!(
        text(&Request::Attach { token: "t".into() }),
        r#"{"op":"attach","token":"t"}"#
    );
    let reply = |m: &Reply| serde_json::to_string(m).unwrap();
    assert_eq!(reply(&Reply::ok()), r#"{"reply":"ok"}"#);
    assert_eq!(
        reply(&Reply::Ok { runs: Some(2) }),
        r#"{"reply":"ok","runs":2}"#
    );
    assert_eq!(
        reply(&Reply::Exit { code: 3 }),
        r#"{"reply":"exit","code":3}"#
    );
    assert_eq!(
        reply(&Reply::error("a\nb")),
        r#"{"reply":"error","error":"a b"}"#
    );
}

#[test]
fn read_line_keeps_the_bytes_after_the_newline() {
    let data = b"{\"op\":\"ping\"}\nraw bytes";
    let mut r = &data[..];
    let m: Request = read_line(&mut r).unwrap().unwrap();
    assert_eq!(m, Request::Ping);
    assert_eq!(r, b"raw bytes");
    let mut empty: &[u8] = b"";
    assert!(read_line::<Request>(&mut empty).unwrap().is_none());
    let mut cut: &[u8] = b"{\"op\"";
    assert!(read_line::<Request>(&mut cut).is_err());
    let mut junk: &[u8] = b"{\"op\":\"format_disk\"}\n";
    assert!(read_line::<Request>(&mut junk).is_err());
}

#[test]
fn a_bad_request_is_refused() {
    assert!(req().check().is_ok());
    let mut r = req();
    r.v = 1;
    assert!(r
        .check()
        .unwrap_err()
        .to_string()
        .contains("paddock vm setup"));
    r.v = VERSION + 1;
    let e = r.check().unwrap_err().to_string();
    assert!(
        e.contains(&format!("protocol {}", VERSION + 1)) && e.contains("vm stop"),
        "{e}"
    );
    for token in [
        "",
        "0123",
        "0123456789ABCDEF0123456789ABCDEF",
        "../../../../etc/passwd/xxxxxxxxxxx",
    ] {
        let mut r = req();
        r.token = token.into();
        assert!(r.check().is_err(), "{token}");
    }
    let mut r = req();
    r.project = PathBuf::from("rel/p");
    assert!(r.check().is_err());
    let mut r = req();
    r.project = PathBuf::from("/");
    assert!(r.check().is_err());
    for name in ["--backend", "../x", "a/b", ".fake", "", "a b"] {
        let mut r = req();
        r.harness = name.into();
        assert!(r.check().is_err(), "{name}");
    }
    for name in ["fake", "cursor-agent", "kiro_cli", "x.2"] {
        assert!(valid_harness(name), "{name}");
    }
    let mut r = req();
    r.profile = Some("x".repeat(MAX_PROFILE + 1));
    assert!(r.check().is_err());
    let mut r = req();
    r.env.insert("LD_PRELOAD".into(), "/x.so".into());
    assert!(r.check().is_err());
}

#[test]
fn only_terminal_and_locale_variables_pass() {
    for k in ["TERM", "LANG", "LC_ALL", "LC_MESSAGES", "TZ", "COLORTERM"] {
        assert!(allowed_var(k), "{k}");
    }
    for k in [
        "PATH",
        "HOME",
        "LC_",
        "LC_all",
        "LD_PRELOAD",
        "PADDOCK_VM_HOST_SOCKET",
    ] {
        assert!(!allowed_var(k), "{k}");
    }
}

#[test]
fn tokens_are_random_hex() {
    let a = new_token().unwrap();
    let b = new_token().unwrap();
    assert!(valid_token(&a) && valid_token(&b));
    assert_ne!(a, b);
}
