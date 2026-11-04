use super::*;
use std::io::Read as _;
#[cfg(target_os = "linux")]
use std::process::Command;

/// The client side: ask `socket` for a pty, as the library does.
fn request(socket: &Path) -> Result<(String, Vec<OwnedFd>)> {
    let mut conn = UnixStream::connect(socket)?;
    conn.write_all(b"pty\n")?;
    let mut buf = [0u8; 256];
    let mut ctl = [0u64; 16];
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    // SAFETY: an all-zero msghdr is valid; the fields are set below.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = ctl.as_mut_ptr().cast();
    msg.msg_controllen = std::mem::size_of_val(&ctl) as _;
    // SAFETY: msg points at live buffers.
    let n = unsafe { libc::recvmsg(conn.as_raw_fd(), &mut msg, 0) };
    anyhow::ensure!(n > 0, "no answer");
    let mut fds = Vec::new();
    // SAFETY: the kernel filled the control buffer.
    unsafe {
        let mut c = libc::CMSG_FIRSTHDR(&msg);
        while !c.is_null() {
            if (*c).cmsg_type == libc::SCM_RIGHTS {
                let count = ((*c).cmsg_len as usize - libc::CMSG_LEN(0) as usize)
                    / std::mem::size_of::<libc::c_int>();
                let data = libc::CMSG_DATA(c).cast::<libc::c_int>();
                for i in 0..count {
                    fds.push(OwnedFd::from_raw_fd(*data.add(i)));
                }
            }
            c = libc::CMSG_NXTHDR(&msg, c);
        }
    }
    Ok((
        String::from_utf8_lossy(&buf[..n as usize]).into_owned(),
        fds,
    ))
}

fn broker() -> (tempfile::TempDir, Broker) {
    let t = tempfile::tempdir().unwrap();
    let b = serve(&t.path().join("run/pty.sock")).unwrap();
    (t, b)
}

#[test]
fn the_broker_sends_a_working_pty() {
    let (_t, b) = broker();
    let (text, fds) = request(&b.socket).unwrap();
    assert!(text.starts_with("ok /dev/"), "{text}");
    assert_eq!(fds.len(), 2);
    let name = text.trim_end().strip_prefix("ok ").unwrap();
    let mut slave = std::fs::File::from(fds[1].try_clone().unwrap());
    let mut master = std::fs::File::from(fds[0].try_clone().unwrap());
    // The slave the broker sent is the pty it named.
    let mut buf = [0u8; 128];
    // SAFETY: the buffer is valid for its length.
    let r = unsafe { libc::ttyname_r(slave.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
    assert_eq!(r, 0);
    let end = buf.iter().position(|b| *b == 0).unwrap();
    assert_eq!(std::str::from_utf8(&buf[..end]).unwrap(), name);
    slave.write_all(b"hello\n").unwrap();
    let mut got = [0u8; 16];
    let n = master.read(&mut got).unwrap();
    assert!(String::from_utf8_lossy(&got[..n]).contains("hello"));
    assert_eq!(b.issued(), 1);
}

#[test]
fn the_broker_refuses_other_requests() {
    let (_t, b) = broker();
    let mut conn = UnixStream::connect(&b.socket).unwrap();
    conn.write_all(b"open /dev/ttys000\n").unwrap();
    let mut s = String::new();
    conn.read_to_string(&mut s).unwrap();
    assert_eq!(s, "error unknown request\n");
    assert_eq!(b.issued(), 0);
}

#[test]
fn the_broker_limits_the_ptys_of_a_burst() {
    let recent = Mutex::new(VecDeque::new());
    let t0 = Instant::now();
    for _ in 0..BURST {
        assert!(admit(&recent, t0));
    }
    assert!(!admit(&recent, t0 + Duration::from_secs(1)));
    assert!(admit(&recent, t0 + WINDOW + Duration::from_secs(1)));
}

#[test]
fn the_socket_folder_is_private() {
    let (_t, b) = broker();
    let mode = std::os::unix::fs::PermissionsExt::mode(
        &std::fs::metadata(b.socket.parent().unwrap())
            .unwrap()
            .permissions(),
    );
    assert_eq!(mode & 0o077, 0, "{mode:o}");
}

const DV_HARDENED: &str = "Executable=/opt/homebrew/Caskroom/codex/0.1/codex\nIdentifier=codex\nFormat=Mach-O thin (arm64)\nCodeDirectory v=20500 size=1 flags=0x10000(runtime) hashes=1+7 location=embedded\nSignature size=9000\n";
const DV_ADHOC: &str = "Executable=/Users/demo/bin/probe\nCodeDirectory v=20400 size=1 flags=0x20002(adhoc,linker-signed) hashes=1+0 location=embedded\n";

fn ents(keys: &[&str]) -> String {
    let body: String = keys
        .iter()
        .map(|k| format!("<key>{k}</key><true/>"))
        .collect();
    format!("<?xml version=\"1.0\"?><plist version=\"1.0\"><dict>{body}</dict></plist>")
}

#[test]
fn hardened_binaries_without_the_entitlements_are_refused() {
    let codex = Path::new("/opt/homebrew/bin/codex");
    assert!(hardened(DV_HARDENED));
    assert!(!hardened(DV_ADHOC));
    // Codex's release entitlements: JIT only.
    let why = dyld_refusal(
        codex,
        b"\xcf\xfa\xed\xfe",
        DV_HARDENED,
        &ents(&["com.apple.security.cs.allow-jit"]),
    )
    .unwrap();
    assert!(why.contains(DYLD_KEY), "{why}");
    let why = dyld_refusal(codex, b"\xcf\xfa", DV_HARDENED, &ents(&[DYLD_KEY])).unwrap();
    assert!(why.contains(LIBRARY_VALIDATION_KEY), "{why}");
    assert_eq!(
        dyld_refusal(
            codex,
            b"\xcf\xfa",
            DV_HARDENED,
            &ents(&[DYLD_KEY, LIBRARY_VALIDATION_KEY])
        ),
        None
    );
    // Built from source: an ad-hoc signature, no hardened runtime.
    assert_eq!(dyld_refusal(codex, b"\xcf\xfa", DV_ADHOC, ""), None);
}

#[test]
fn protected_programs_and_their_scripts_are_refused() {
    let why = dyld_refusal(Path::new("/bin/sh"), b"\xcf\xfa", DV_ADHOC, "").unwrap();
    assert!(why.contains("SIP"), "{why}");
    let why = dyld_refusal(
        Path::new("/opt/homebrew/bin/claude"),
        b"#!/usr/bin/env node\nrequire('x')",
        "",
        "",
    )
    .unwrap();
    assert!(why.contains("/usr/bin/env"), "{why}");
    assert_eq!(
        interpreter(b"#!/opt/homebrew/bin/node --flag\n"),
        Some(PathBuf::from("/opt/homebrew/bin/node"))
    );
    assert_eq!(
        dyld_refusal(
            Path::new("/opt/x/tool"),
            b"#!/opt/homebrew/bin/node\n",
            "",
            ""
        ),
        None
    );
    assert!(!protected(Path::new("/usr/local/bin/codex")));
}

#[cfg(target_os = "linux")]
/// Build the probe (src/ptyshim/probe.c) with the C compiler, or None when
/// there is none.
fn probe(dir: &Path) -> Option<PathBuf> {
    let out = dir.join("probe");
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ptyshim/probe.c");
    let mut c = Command::new("cc");
    c.arg("-o").arg(&out).arg(src).arg("-lutil");
    c.status().ok()?.success().then_some(out)
}

#[cfg(target_os = "linux")]
/// The library and the probe in `dir`, or None when they cannot be built.
fn world(dir: &Path) -> Option<(PathBuf, PathBuf)> {
    if LIBRARY.is_empty() {
        eprintln!("skipped: build.rs could not build the pty library");
        return None;
    }
    let lib = dir.join("libpaddock-pty.so");
    std::fs::write(&lib, LIBRARY).unwrap();
    let Some(probe) = probe(dir) else {
        eprintln!("skipped: no C compiler for the probe");
        return None;
    };
    Some((lib, probe))
}

#[cfg(target_os = "linux")]
fn run_probe(dir: &Path, probe: &Path, lib: &Path, socket: Option<&Path>, what: &str) -> String {
    let _ = std::fs::remove_file(dir.join("probe.txt"));
    let mut c = Command::new(probe);
    c.arg(what).current_dir(dir).env("LD_PRELOAD", lib);
    match socket {
        Some(s) => c.env(SOCKET_VAR, s),
        None => c.env_remove(SOCKET_VAR),
    };
    let st = c.status().unwrap();
    assert!(st.success(), "{what}: {st}");
    std::fs::read_to_string(dir.join("probe.txt")).unwrap()
}

#[cfg(target_os = "linux")]
#[test]
fn the_library_takes_its_ptys_from_the_broker() {
    let (t, b) = broker();
    let Some((lib, probe)) = world(t.path()) else {
        return;
    };
    let out = run_probe(t.path(), &probe, &lib, Some(&b.socket), "openpty");
    for line in [
        "openpty ok",
        "ttyname same",
        "raw ok",
        "roundtrip ok",
        "winsize ok",
    ] {
        assert!(out.contains(line), "{line}: {out}");
    }
    assert_eq!(b.issued(), 1, "{out}");
    let out = run_probe(t.path(), &probe, &lib, Some(&b.socket), "forkpty");
    assert!(out.contains("forkpty ok"), "{out}");
    assert_eq!(b.issued(), 2);
    let out = run_probe(t.path(), &probe, &lib, Some(&b.socket), "posix");
    assert!(out.contains("posix_openpt ok"), "{out}");
    assert_eq!(b.issued(), 3);
}

#[cfg(target_os = "linux")]
#[test]
fn without_a_broker_the_library_uses_the_real_calls() {
    let t = tempfile::tempdir().unwrap();
    let Some((lib, probe)) = world(t.path()) else {
        return;
    };
    let out = run_probe(t.path(), &probe, &lib, None, "openpty");
    assert!(
        out.contains("openpty ok") && out.contains("roundtrip ok"),
        "{out}"
    );
    let out = run_probe(t.path(), &probe, &lib, None, "posix");
    assert!(out.contains("posix_openpt ok"), "{out}");
    // A broker that is gone: the request fails, nothing falls back.
    let gone = t.path().join("gone.sock");
    let out = run_probe(t.path(), &probe, &lib, Some(&gone), "openpty");
    assert!(!out.contains("openpty ok"), "{out}");
}
