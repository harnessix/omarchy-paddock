use super::*;
use std::process::{Command, Stdio};
use std::sync::Mutex;

fn msg(body: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    write_message(&mut v, body).unwrap();
    v
}

#[test]
fn the_parser_takes_good_messages_and_refuses_bad_ones() {
    // Good.
    assert_eq!(
        parse_request(&[REQUEST_IDENTITIES]).unwrap(),
        Request::Identities
    );
    let mut sign = vec![SIGN_REQUEST];
    put_string(&mut sign, b"blob");
    put_string(&mut sign, b"data");
    sign.extend_from_slice(&2u32.to_be_bytes());
    assert_eq!(
        parse_request(&sign).unwrap(),
        Request::Sign {
            blob: b"blob".to_vec(),
            data: b"data".to_vec(),
            flags: 2
        }
    );
    // Short: a cut string, a missing flags field, nothing at all.
    assert!(parse_request(&sign[..sign.len() - 1]).is_err());
    assert!(parse_request(&sign[..7]).is_err());
    assert!(parse_request(&[]).is_err());
    // Extra bytes.
    let mut long = sign.clone();
    long.push(0);
    assert!(parse_request(&long).is_err());
    assert!(parse_request(&[REQUEST_IDENTITIES, 0]).is_err());
    // A string length beyond the message.
    let mut lie = vec![SIGN_REQUEST];
    lie.extend_from_slice(&u32::MAX.to_be_bytes());
    assert!(parse_request(&lie).is_err());
    // Other types pass the parser as Other and are never forwarded.
    for t in [17u8, 18, 19, 22, 23, 27, 200] {
        assert_eq!(parse_request(&[t, 1, 2]).unwrap(), Request::Other(t));
    }
}

#[test]
fn framing_refuses_empty_short_and_too_long_messages() {
    let good = msg(&[REQUEST_IDENTITIES]);
    assert_eq!(
        read_message(&mut &good[..]).unwrap(),
        Some(vec![REQUEST_IDENTITIES])
    );
    assert_eq!(read_message(&mut &[][..]).unwrap(), None);
    assert!(read_message(&mut &good[..2]).is_err());
    assert!(read_message(&mut &[0, 0, 0, 0][..]).is_err());
    assert!(read_message(&mut &[0, 0, 0, 5, 11][..]).is_err());
    let too_long = ((MAX_MESSAGE + 1) as u32).to_be_bytes();
    assert!(read_message(&mut &too_long[..]).is_err());
    let max = (MAX_MESSAGE as u32).to_be_bytes();
    let mut v = max.to_vec();
    v.resize(4 + MAX_MESSAGE, 0);
    assert!(read_message(&mut &v[..]).unwrap().is_some());
}

#[test]
fn identities_and_public_keys_round_trip() {
    let ids = vec![
        (b"one".to_vec(), b"c1".to_vec()),
        (b"two".to_vec(), Vec::new()),
    ];
    assert_eq!(parse_identities(&identities_answer(&ids)).unwrap(), ids);
    let mut bad = identities_answer(&ids);
    bad.push(1);
    assert!(parse_identities(&bad).is_err());
    assert!(parse_identities(&[FAILURE]).is_err());
    let mut many = vec![IDENTITIES_ANSWER];
    many.extend_from_slice(&u32::MAX.to_be_bytes());
    assert!(parse_identities(&many).is_err());

    // A public key line: the blob starts with its type.
    use base64::Engine as _;
    let mut blob = Vec::new();
    put_string(&mut blob, b"ssh-ed25519");
    put_string(&mut blob, &[7u8; 32]);
    let b64 = base64::engine::general_purpose::STANDARD.encode(&blob);
    let (b, c) = parse_public_key(&format!("ssh-ed25519 {b64} me@box\n")).unwrap();
    assert_eq!((b, c.as_str()), (blob.clone(), "me@box"));
    assert!(parse_public_key(&format!("ssh-rsa {b64}")).is_err());
    assert!(parse_public_key("ssh-ed25519 !!!").is_err());
    assert!(parse_public_key("").is_err());
    assert!(fingerprint(&blob).starts_with("SHA256:"));
    assert!(!fingerprint(&blob).ends_with('='));
}

// ---- With a real ssh-agent in a temporary folder ----

struct Agent {
    child: std::process::Child,
}
impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn have(p: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {p}")])
        .stdout(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// A command with a clean environment: only PATH, a temporary HOME and the
/// given agent socket. Never the user's agent.
fn tool(dir: &Path, sock: &Path, prog: &str) -> Command {
    let mut c = Command::new(prog);
    c.env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin:/usr/sbin")
        .env("HOME", dir)
        .env("SSH_AUTH_SOCK", sock)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    c
}

fn wait_for(p: &Path) {
    for _ in 0..200 {
        if p.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("{} did not appear", p.display());
}

#[derive(Default)]
struct Seen {
    asked: Mutex<Vec<crate::broker::Pending>>,
    answers: Mutex<Vec<crate::broker::Answer>>,
}
struct FakeApprover(Arc<Seen>);
impl crate::broker::Approver for FakeApprover {
    fn ask(
        &self,
        _: &HostEnv,
        p: &crate::broker::Pending,
        _: Duration,
    ) -> Option<crate::broker::Answer> {
        self.0.asked.lock().unwrap().push(p.clone());
        let mut a = self.0.answers.lock().unwrap();
        (!a.is_empty()).then(|| a.remove(0))
    }
}

#[test]
fn a_real_agent_behind_the_proxy_shows_and_signs_only_the_configured_key() {
    if !["ssh-agent", "ssh-add", "ssh-keygen"]
        .iter()
        .all(|p| have(p))
    {
        eprintln!("skipped: ssh-agent, ssh-add or ssh-keygen is missing");
        return;
    }
    let t = tempfile::tempdir().unwrap();
    let d = t.path();
    let agent_sock = d.join("agent.sock");
    let _agent = Agent {
        child: tool(d, &agent_sock, "ssh-agent")
            .args(["-D", "-a"])
            .arg(&agent_sock)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    };
    wait_for(&agent_sock);
    for k in ["one", "two"] {
        let out = tool(d, &agent_sock, "ssh-keygen")
            .args([
                "-q",
                "-t",
                "ed25519",
                "-N",
                "",
                "-C",
                &format!("test-{k}"),
                "-f",
            ])
            .arg(d.join(k))
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        let out = tool(d, &agent_sock, "ssh-add")
            .arg(d.join(k))
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        // Only the agent holds the private key now.
        std::fs::remove_file(d.join(k)).unwrap();
    }
    let pub_one = std::fs::read_to_string(d.join("one.pub")).unwrap();
    let pub_two = std::fs::read_to_string(d.join("two.pub")).unwrap();
    let data_of = |p: &str| p.split_whitespace().nth(1).unwrap().to_string();

    let env = HostEnv::from_parts(d.to_path_buf(), "t".into(), 1000, 1000, Default::default());
    let spec = crate::spec::SandboxSpec::new(
        "sb-ssh".into(),
        "claude",
        Path::new("/home/demo/app"),
        d.join("sb"),
        d,
        1000,
        1000,
        "t",
    );
    let cred = |confirm: &str| -> (String, Credential) {
        let all = super::super::parse(&format!(
            "[credentials.ssh]\nvia = \"agent\"\nkeys = [\"{}\"]\nconfirm = \"{confirm}\"\n",
            d.join("one.pub").display()
        ))
        .unwrap();
        all.into_iter().next().unwrap()
    };
    let reports = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen = Arc::new(Seen::default());
    let start = |confirm: &str, sock: &Path| {
        let keys = load_keys(&env, &[cred(confirm)]).unwrap();
        check_host(Some(&agent_sock), &keys).unwrap();
        let r = reports.clone();
        Proxy::new(
            keys,
            agent_sock.clone(),
            d.join("credentials-revoked"),
            approver_ask(
                env.clone(),
                spec.clone(),
                Arc::new(FakeApprover(seen.clone())),
                Duration::from_secs(1),
            ),
            Arc::new(move |k: &Key, res: &str| {
                r.lock()
                    .unwrap()
                    .push(format!("{} {} {res}", k.name, k.fingerprint));
            }),
        )
        .serve(sock)
        .unwrap();
    };
    let proxy = d.join("proxy.sock");
    start("session", &proxy);

    // Only the configured key shows.
    let out = tool(d, &proxy, "ssh-add").arg("-L").output().unwrap();
    let listed = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(out.status.success(), "{out:?}");
    assert!(listed.contains(&data_of(&pub_one)), "{listed}");
    assert!(!listed.contains(&data_of(&pub_two)), "{listed}");

    let sign = |sock: &Path, key: &str| {
        let file = d.join(format!("msg-{}", rand_suffix()));
        std::fs::write(&file, "secret data to sign\n").unwrap();
        tool(d, sock, "ssh-keygen")
            .args(["-Y", "sign", "-n", "test", "-f"])
            .arg(d.join(key))
            .arg(&file)
            .output()
            .unwrap()
            .status
            .success()
    };
    assert!(sign(&proxy, "one.pub"), "the configured key signs");
    assert!(!sign(&proxy, "two.pub"), "the other key does not");
    // The real agent signs with the second key: the failure came from the proxy.
    assert!(sign(&agent_sock, "two.pub"));

    // Remove all through the proxy fails, and both keys stay.
    let out = tool(d, &proxy, "ssh-add").arg("-D").output().unwrap();
    assert!(!out.status.success(), "{out:?}");
    let out = tool(d, &agent_sock, "ssh-add").arg("-L").output().unwrap();
    let real = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(real.contains(&data_of(&pub_one)) && real.contains(&data_of(&pub_two)));

    // A lock request and an extension get a failure on a raw connection.
    let mut raw = UnixStream::connect(&proxy).unwrap();
    for body in [&[22u8, 0, 0, 0, 1, b'x'][..], &[27u8, 0, 0, 0, 0][..]] {
        write_message(&mut raw, body).unwrap();
        assert_eq!(read_message(&mut raw).unwrap().unwrap(), [FAILURE]);
    }
    // A message over the limit gets a failure and the connection closes.
    raw.write_all(&((MAX_MESSAGE + 1) as u32).to_be_bytes())
        .unwrap();
    assert_eq!(read_message(&mut raw).unwrap().unwrap(), [FAILURE]);
    assert_eq!(read_message(&mut raw).unwrap(), None);
    assert!(seen.asked.lock().unwrap().is_empty(), "session asks nobody");

    // confirm = "each": the approver decides each signature.
    let each = d.join("each.sock");
    start("each", &each);
    seen.answers
        .lock()
        .unwrap()
        .extend([crate::broker::Answer::Deny, crate::broker::Answer::Once]);
    assert!(!sign(&each, "one.pub"), "a denied signature fails");
    assert!(sign(&each, "one.pub"), "an allowed signature passes");
    let asked = seen.asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 2);
    let fp = fingerprint(&parse_public_key(&pub_one).unwrap().0);
    for p in &asked {
        assert_eq!(p.capability, SIGN_CAPABILITY);
        assert!(
            p.detail.contains(&fp) && p.detail.contains("test-one"),
            "{}",
            p.detail
        );
        assert!(!p.detail.contains("secret data") && !p.summary.contains("secret data"));
    }

    // The revoke file stops the signatures.
    std::fs::write(d.join("credentials-revoked"), "ssh\n").unwrap();
    assert!(!sign(&proxy, "one.pub"), "a revoked key does not sign");
    // ssh-keygen sees no key in the list, so ask on a raw connection too:
    // for the revoked key and for the key that is not configured.
    let mut raw = UnixStream::connect(&proxy).unwrap();
    for p in [&pub_one, &pub_two] {
        let blob = parse_public_key(p).unwrap().0;
        write_message(&mut raw, &sign_request(&blob, b"secret data", 0)).unwrap();
        assert_eq!(read_message(&mut raw).unwrap().unwrap(), [FAILURE]);
    }
    write_message(&mut raw, &[REQUEST_IDENTITIES]).unwrap();
    assert_eq!(
        parse_identities(&read_message(&mut raw).unwrap().unwrap()).unwrap(),
        []
    );

    let r = reports.lock().unwrap().join("\n");
    assert!(r.contains(&format!("ssh {fp} allowed")), "{r}");
    assert!(
        r.contains("denied by the user") && r.contains("denied: revoked"),
        "{r}"
    );
    assert!(r.contains("not a configured key"), "{r}");
    assert!(!r.contains("secret data"));
}

fn rand_suffix() -> u64 {
    static N: AtomicUsize = AtomicUsize::new(0);
    N.fetch_add(1, Ordering::SeqCst) as u64
}
