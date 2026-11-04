//! The escape probe of a terminal run, for Seatbelt. `src/probe.sh` is for
//! bwrap: it checks mounts, namespaces and /proc, which macOS does not
//! have. These checks run the rendered profile, the rendered command and
//! the rendered environment with `sandbox-exec`. Temporary folders only:
//! the "real home" is a temporary folder too.

use super::*;
use crate::spec::Mount;
use std::process::{Command, Stdio};

struct World {
    _t: tempfile::TempDir,
    env: HostEnv,
    spec: SandboxSpec,
    /// The stand-in for the real home: nothing in it is allowed.
    home: PathBuf,
}

fn world(network: Network) -> World {
    let t = tempfile::tempdir().unwrap();
    let root = real(t.path());
    let home = root.join("home");
    let project = root.join("project");
    let home_src = root.join("sandbox-home");
    for d in [&home, &project, &home_src] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(home.join("secret.txt"), "secret").unwrap();
    std::fs::write(project.join("file.txt"), "x").unwrap();
    std::fs::write(project.join(".env"), "TOKEN=1").unwrap();
    // The real ids: the proxy socket goes into /tmp/paddock-<uid>.
    // SAFETY: getuid and getgid cannot fail.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let env = HostEnv::from_parts(home.clone(), "u".into(), uid, gid, BTreeMap::new());
    let mut spec = SandboxSpec::new(
        "sbtest".into(),
        "fake",
        &project,
        home_src,
        &home,
        uid,
        gid,
        "u",
    );
    spec.add_mount(Mount::same(&project, true, "project"));
    spec.add_mount(Mount::mask(project.join(".env"), false, "secrets"));
    spec.network = network;
    std::fs::create_dir_all(env.paddock_run().join(&spec.id)).unwrap();
    World {
        _t: t,
        env,
        spec,
        home,
    }
}

/// Run `script` in the sandbox. Its exit code says if it succeeded.
fn sandboxed(w: &World, script: &str) -> bool {
    let mut spec = w.spec.clone();
    spec.command = vec!["/bin/sh".into(), "-c".into(), script.into()];
    SeatbeltRun.prepare(&w.env, &spec, false).unwrap();
    let l = SeatbeltRun.render(&w.env, &spec, false).unwrap();
    for (path, text, _) in &l.files {
        std::fs::write(path, text).unwrap();
    }
    Command::new(&l.program)
        .args(&l.args)
        .env_clear()
        .envs(&l.env)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success()
}

#[test]
fn it_reads_and_writes_the_project_in_its_work_folder() {
    let w = world(Network::None);
    assert!(sandboxed(&w, "test \"$(cat file.txt)\" = x"));
    assert!(sandboxed(&w, "echo y > new.txt && test -f new.txt"));
    assert!(w.spec.project.join("new.txt").is_file());
    assert!(!sandboxed(&w, "cat .env"));
}

#[test]
fn it_cannot_read_or_write_the_real_home_or_shared_temp() {
    let w = world(Network::None);
    let secret = w.home.join("secret.txt").display().to_string();
    assert!(!sandboxed(&w, &format!("cat '{secret}'")));
    assert!(!sandboxed(&w, &format!("ls '{}'", w.home.display())));
    assert!(!sandboxed(
        &w,
        &format!("touch '{}'", w.home.join("planted").display())
    ));
    assert!(!w.home.join("planted").exists());
    // The host /tmp holds the sockets of other runs.
    assert!(!sandboxed(&w, "touch /private/tmp/paddock-probe-$$"));
    // Its own home and temporary folder work.
    assert!(sandboxed(
        &w,
        "echo x > \"$HOME/h\" && echo x > \"$TMPDIR/t\""
    ));
    assert!(sandboxed(
        &w,
        "test \"$HOME\" = \"$CFFIXED_USER_HOME\" && test \"$HOME\" != /"
    ));
}

#[test]
fn it_reaches_the_network_only_through_the_proxy() {
    let other = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let other_port = other.local_addr().unwrap().port();
    std::thread::spawn(move || for _ in other.incoming() {});
    let mut w = world(Network::Egress);
    start_proxy(&w.env, &mut w.spec).unwrap();
    let relay = w
        .spec
        .host_ports
        .iter()
        .find(|(_, o)| o == RELAY_ORIGIN)
        .unwrap()
        .0;
    let nc = |port: u16| format!("/usr/bin/nc -z -G 2 127.0.0.1 {port}");
    assert!(sandboxed(&w, &nc(relay)));
    assert!(!sandboxed(&w, &nc(other_port)));
    assert!(sandboxed(
        &w,
        &format!("test \"$HTTPS_PROXY\" = http://127.0.0.1:{relay}")
    ));
    // A host loopback port through the proxy: the proxy refuses it (403),
    // which shows that the proxy answers.
    assert!(sandboxed(
        &w,
        &format!(
            "test \"$(/usr/bin/curl -s --noproxy '' -o /dev/null -w '%{{http_code}}' -x \"$HTTP_PROXY\" http://127.0.0.1:{other_port}/)\" = 403"
        )
    ));
    // Without network, the relay is out of reach too.
    let none = world(Network::None);
    assert!(!sandboxed(&none, &nc(relay)));
}

#[test]
fn it_cannot_open_another_terminal() {
    let w = world(Network::None);
    // Opening for reading only: nothing reaches the terminal.
    if Path::new("/dev/ttys000").exists() {
        assert!(!sandboxed(&w, "exec 3</dev/ttys000"));
    }
    assert!(sandboxed(&w, "echo x > /dev/null"));
}

/// `security` on the host with the stand-in home, so the real search list
/// of the user never changes.
fn host_security(home: &Path, args: &[&str]) -> std::process::Output {
    Command::new("/usr/bin/security")
        .args(args)
        .env("HOME", home)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

#[test]
fn the_keychain_of_the_login_opens_and_the_users_keychain_stays_closed() {
    let mut w = world(Network::None);
    // A stand-in for the user's login keychain: a temporary keychain file
    // in the stand-in home, unlocked, with an item.
    // Named `user`: the security service treats a keychain named `login`
    // as the login keychain and refuses our test password for it.
    let user = w.home.join("Library/Keychains/user.keychain-db");
    std::fs::create_dir_all(user.parent().unwrap()).unwrap();
    let u = user.display().to_string();
    for args in [
        vec!["create-keychain", "-p", "pw", &u],
        vec!["unlock-keychain", "-p", "pw", &u],
        vec![
            "add-generic-password",
            "-a",
            "a",
            "-s",
            "paddock-user-item",
            "-w",
            "user-secret",
            &u,
        ],
    ] {
        assert!(host_security(&w.home, &args).status.success(), "{args:?}");
    }
    let mode = crate::state::AuthMode::Isolated("t".into());
    super::super::keychain::start(&w.env, &mut w.spec, &mode).unwrap();
    let kc = w.spec.env[super::super::keychain::VAR].value.clone();
    use std::os::unix::fs::PermissionsExt;
    let kc_mode = std::fs::metadata(&kc).unwrap().permissions().mode() & 0o777;
    assert_eq!(kc_mode, 0o600, "the keychain file is private");
    let sec = "/usr/bin/security";
    // The search list of the sandbox names its own keychain.
    assert!(sandboxed(
        &w,
        &format!("{sec} list-keychains -d user | grep -qF '{kc}'")
    ));
    assert!(sandboxed(
        &w,
        &format!("{sec} default-keychain -d user | grep -qF '{kc}'")
    ));
    assert!(sandboxed(&w, &format!("{sec} show-keychain-info")));
    // An item that the sandbox writes, as Claude Code does, reads back in a
    // new sandbox.
    assert!(sandboxed(
        &w,
        &format!("{sec} add-generic-password -U -a a -s paddock-test -w s3cret")
    ));
    assert!(sandboxed(
        &w,
        &format!("test \"$({sec} find-generic-password -a a -s paddock-test -w)\" = s3cret")
    ));
    // The stand-in user keychain stays closed: by path, by search and as
    // a file.
    assert!(!sandboxed(
        &w,
        &format!("{sec} find-generic-password -s paddock-user-item -w '{u}'")
    ));
    assert!(!sandboxed(
        &w,
        &format!("{sec} find-generic-password -s paddock-user-item -w")
    ));
    assert!(!sandboxed(&w, &format!("cat '{u}' >/dev/null")));
    assert!(!sandboxed(
        &w,
        &format!(
            "{sec} list-keychains -s '{u}' && {sec} find-generic-password -s paddock-user-item -w"
        )
    ));
    // The system keychains stay closed too.
    assert!(!sandboxed(
        &w,
        "cat /Library/Keychains/System.keychain >/dev/null"
    ));
    // The test is sound: the host reads both items.
    let out = host_security(
        &w.home,
        &["find-generic-password", "-s", "paddock-user-item", "-w", &u],
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "user-secret");
    let out = host_security(
        &w.home,
        &["find-generic-password", "-s", "paddock-test", "-w", &kc],
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "s3cret");
    // A second start keeps the keychain and its item.
    super::super::keychain::start(&w.env, &mut w.spec, &mode).unwrap();
    assert!(sandboxed(
        &w,
        &format!("{sec} find-generic-password -a a -s paddock-test -w >/dev/null")
    ));
    for k in [&u, &kc] {
        host_security(&w.home, &["delete-keychain", k]);
    }
}

/// A probe for ptys: it makes a pty with openpty, opens its terminal side
/// by name and sends a byte through it. Then it opens the terminal in
/// argv[1], which it did not make. Exit 0 when all of it works and the
/// open of argv[1] fails with EPERM.
const PTY_PROBE: &str = r#"
#include <errno.h>
#include <fcntl.h>
#include <string.h>
#include <unistd.h>
#include <util.h>
int main(int argc, char **argv) {
    int m, s;
    char name[128], c;
    if (argc != 2) return 2;
    if (openpty(&m, &s, name, NULL, NULL) != 0) return 10;
    if (strncmp(name, "/dev/ttys", 9) != 0) return 11;
    int again = open(name, O_RDWR | O_NOCTTY);
    if (again < 0) return 12;
    if (write(m, "x\n", 2) != 2) return 13;
    if (read(again, &c, 1) != 1 || c != 'x') return 14;
    if (open(argv[1], O_RDWR | O_NOCTTY) >= 0) return 20;
    return errno == EPERM ? 0 : 21;
}
"#;

/// The probe, built with `cc` and signed with the hardened runtime, as
/// released agy, Codex and Grok builds are. None without `cc` or
/// `codesign`.
fn hardened_probe(dir: &Path) -> Option<PathBuf> {
    let codesign = Path::new("/usr/bin/codesign");
    let cc = Command::new("cc").arg("--version").output();
    if !codesign.is_file() || !cc.map(|o| o.status.success()).unwrap_or(false) {
        eprintln!("skipped: no cc or codesign");
        return None;
    }
    let src = dir.join("ptyprobe.c");
    let bin = dir.join("ptyprobe");
    std::fs::write(&src, PTY_PROBE).unwrap();
    let ok = |c: &mut Command| c.status().unwrap().success();
    assert!(ok(Command::new("cc").arg("-o").arg(&bin).arg(&src)));
    assert!(ok(Command::new(codesign)
        .args(["-f", "-s", "-", "-o", "runtime"])
        .arg(&bin)));
    Some(bin)
}

/// The exit code of `cmd` in the sandbox, with the policy of the run
/// changed by `edit`, and the profile text.
fn sandboxed_code(w: &World, cmd: Vec<String>, edit: fn(&mut Policy)) -> (i32, String) {
    let mut spec = w.spec.clone();
    spec.command = cmd;
    SeatbeltRun.prepare(&w.env, &spec, false).unwrap();
    let l = SeatbeltRun.render(&w.env, &spec, false).unwrap();
    let (mut pol, _) = policy(&w.env, &spec, &private_tmp(&spec)).unwrap();
    edit(&mut pol);
    let text = crate::seatbelt::render(&pol).unwrap();
    std::fs::write(&l.files[0].0, &text).unwrap();
    let code = Command::new(&l.program)
        .args(&l.args)
        .env_clear()
        .envs(&l.env)
        .stdin(Stdio::null())
        .status()
        .unwrap()
        .code()
        .unwrap_or(-1);
    (code, text)
}

#[test]
fn a_hardened_harness_makes_its_own_ptys_and_cannot_open_others() {
    let w = world(Network::None);
    let Some(probe) = hardened_probe(&w.spec.project) else {
        return;
    };
    // A pty of the host, outside the sandbox: the probe must not open it.
    let (mut m, mut s) = (0, 0);
    let mut name = [0 as libc::c_char; 128];
    // SAFETY: the out pointers are valid, the name buffer is 128 bytes.
    let rc = unsafe {
        libc::openpty(
            &mut m,
            &mut s,
            name.as_mut_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(rc, 0);
    // SAFETY: openpty wrote a terminated name.
    let other = unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    let cmd = vec![probe.display().to_string(), other.clone()];
    // The host itself opens the other pty: the probe's EPERM comes from
    // the sandbox.
    assert!(std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&other)
        .is_ok());
    let (code, text) = sandboxed_code(&w, cmd.clone(), |_| {});
    assert!(
        text.contains("(allow pseudo-tty)"),
        "pty_refusal found no reason for the hardened probe"
    );
    assert_eq!(code, 0, "probe exit code {code} (see PTY_PROBE)");
    // The test is sound: without the rules, openpty fails.
    let (code, _) = sandboxed_code(&w, cmd, |p| p.own_ptys = false);
    assert_eq!(code, 10);
    // SAFETY: the test owns both descriptors.
    unsafe {
        libc::close(m);
        libc::close(s);
    }
}
