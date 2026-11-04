use super::*;
use std::io::BufRead;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

#[test]
fn parse_request_knows_three_requests() {
    assert_eq!(
        parse_request("connect 1027").unwrap(),
        Request::Connect(1027)
    );
    assert_eq!(
        parse_request("connect pty").unwrap(),
        Request::Connect(1027)
    );
    assert_eq!(parse_request("stop").unwrap(), Request::Stop);
    assert_eq!(parse_request("ping").unwrap(), Request::Ping);
    for bad in [
        "connect 22",
        "connect",
        "connect 1027 x",
        "stop now",
        "",
        "PING",
    ] {
        assert!(parse_request(bad).is_err(), "{bad}");
    }
}

#[test]
fn splice_copies_both_ways_and_passes_the_end() {
    let (a_out, a_in) = UnixStream::pair().unwrap();
    let (b_out, b_in) = UnixStream::pair().unwrap();
    let t = std::thread::spawn(move || splice(a_in.into(), b_in.into()));
    let (mut a, mut b) = (a_out, b_out);
    a.write_all(b"to b").unwrap();
    a.shutdown(std::net::Shutdown::Write).unwrap();
    let mut got = String::new();
    b.read_to_string(&mut got).unwrap();
    assert_eq!(got, "to b");
    b.write_all(b"to a").unwrap();
    drop(b);
    got.clear();
    a.read_to_string(&mut got).unwrap();
    assert_eq!(got, "to a");
    t.join().unwrap().unwrap();
}

#[test]
fn forward_sends_a_guest_connection_to_the_socket() {
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("broker.sock");
    let host = UnixListener::bind(&path).unwrap();
    let (mut guest, guest_end) = UnixStream::pair().unwrap();
    forward(guest_end.into(), &path);
    let (mut conn, _) = host.accept().unwrap();
    guest.write_all(b"hello\n").unwrap();
    let mut line = String::new();
    std::io::BufReader::new(&mut conn)
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line, "hello\n");
    conn.write_all(b"back").unwrap();
    drop(conn);
    let mut got = String::new();
    guest.read_to_string(&mut got).unwrap();
    assert_eq!(got, "back");
}

/// A guest that echoes on port 1027 and has nothing on the other ports.
#[derive(Default)]
struct FakeGuest {
    stopped: AtomicBool,
    ports: Mutex<Vec<u32>>,
}

impl Guest for FakeGuest {
    fn connect(&self, port: u32) -> Result<OwnedFd> {
        self.ports.lock().unwrap().push(port);
        if port != ports::PTY {
            bail!("nothing listens on port {port}\nin the guest");
        }
        let (mine, theirs) = UnixStream::pair()?;
        std::thread::spawn(move || {
            let mut r = mine.try_clone().unwrap();
            let mut w = mine;
            let _ = std::io::copy(&mut r, &mut w);
        });
        Ok(theirs.into())
    }
    fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
    }
}

fn ask(path: &Path, line: &str) -> (String, UnixStream) {
    let mut c = UnixStream::connect(path).unwrap();
    c.write_all(line.as_bytes()).unwrap();
    let mut answer = Vec::new();
    let mut byte = [0u8; 1];
    // A refused request can end with a reset instead of a close.
    while matches!(c.read(&mut byte), Ok(1)) && byte[0] != b'\n' {
        answer.push(byte[0]);
    }
    (String::from_utf8(answer).unwrap(), c)
}

#[test]
fn control_socket_connects_stops_and_refuses() {
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("control.sock");
    // A stale socket from an earlier start goes away.
    drop(UnixListener::bind(&path).unwrap());
    let listener = bind_control(&path).unwrap();
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    let guest = Arc::new(FakeGuest::default());
    serve_control(listener, guest.clone());

    assert_eq!(ask(&path, "ping\n").0, "ok");

    let (answer, mut c) = ask(&path, "connect 1027\n");
    assert_eq!(answer, "ok");
    c.write_all(b"true\n").unwrap();
    c.shutdown(std::net::Shutdown::Write).unwrap();
    let mut echo = String::new();
    c.read_to_string(&mut echo).unwrap();
    assert_eq!(echo, "true\n");

    let (answer, _) = ask(&path, "connect control\n");
    assert_eq!(answer, "error nothing listens on port 1024 in the guest");
    let (answer, _) = ask(&path, "connect 80\n");
    assert_eq!(answer, "error unknown port 80");
    let (answer, _) = ask(&path, &format!("{}\n", "x".repeat(100)));
    assert!(
        "error the request is too long".starts_with(&answer),
        "{answer}"
    );
    assert_eq!(*guest.ports.lock().unwrap(), vec![1027, 1024]);

    assert!(!guest.stopped.load(Ordering::SeqCst));
    assert_eq!(ask(&path, "stop\n").0, "ok");
    // The stop runs after the answer.
    for _ in 0..100 {
        if guest.stopped.load(Ordering::SeqCst) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(guest.stopped.load(Ordering::SeqCst));
}

#[test]
fn bind_control_refuses_a_file() {
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("control.sock");
    std::fs::write(&path, "x").unwrap();
    assert!(bind_control(&path).is_err());
    assert!(path.exists());
}

/// A host socket that echoes one line back.
fn echo_server(path: &std::path::Path) {
    let l = UnixListener::bind(path).unwrap();
    std::thread::spawn(move || {
        for c in l.incoming() {
            let mut c = c.unwrap();
            let mut s = String::new();
            let _ = c.read_to_string(&mut s);
            let _ = c.write_all(format!("echo {s}").as_bytes());
        }
    });
}

#[test]
fn a_folder_routes_by_the_run_token() {
    // A short folder: the routed socket path must fit in a unix socket
    // address, which is 104 bytes on macOS, where `$TMPDIR` is long.
    let t = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
    let real = t.path().join("broker.sock");
    echo_server(&real);
    let routes = t.path().join("routes");
    std::fs::create_dir(&routes).unwrap();
    let token = "00112233445566778899aabbccddeeff";
    std::os::unix::fs::symlink(&real, routes.join(format!("{token}.sock"))).unwrap();

    let (mut guest, host_end) = UnixStream::pair().unwrap();
    forward(host_end.into(), &routes);
    guest
        .write_all(format!("{token}\nhello").as_bytes())
        .unwrap();
    guest.shutdown(std::net::Shutdown::Write).unwrap();
    let mut got = String::new();
    guest.read_to_string(&mut got).unwrap();
    assert_eq!(got, "echo hello");

    // An unknown token, a token of another form, and no token close the
    // connection without a byte.
    for first in [
        "ffffffffffffffffffffffffffffffff\nhello".to_string(),
        "../broker\nhello".to_string(),
        String::new(),
    ] {
        let (mut guest, host_end) = UnixStream::pair().unwrap();
        forward(host_end.into(), &routes);
        // The bridge can close first. macOS then fails the write with
        // EPIPE and the shutdown with ENOTCONN: both do not matter here.
        let _ = guest.write_all(first.as_bytes());
        let _ = guest.shutdown(std::net::Shutdown::Write);
        let mut got = String::new();
        let _ = guest.read_to_string(&mut got);
        assert_eq!(got, "", "{first:?}");
    }
}
