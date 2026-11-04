//! Layered package caches on bwrap: a crate file that project A plants in
//! its cache never reaches project B. Promotion refuses it because a local
//! fake crates.io index gives another `cksum`, and it promotes a good crate,
//! which B then reads through the shared lower layer. Temporary folders and
//! a local server only: no registry is contacted.

mod common;

use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Output};

const INDEX_DIR: &str = ".cargo/registry/cache/index.crates.io-1949cf8c6b5b557f";

/// bwrap starts, and it can mount an overlay in a user namespace.
fn overlay_works(dir: &Path) -> bool {
    let d: Vec<_> = ["low", "up", "work", "dst"]
        .iter()
        .map(|n| {
            let p = dir.join("probe").join(n);
            std::fs::create_dir_all(&p).unwrap();
            p
        })
        .collect();
    Command::new("bwrap")
        .args(["--unshare-user", "--ro-bind", "/", "/", "--overlay-src"])
        .arg(&d[0])
        .arg("--overlay")
        .args([&d[1], &d[2], &d[3]])
        .arg("true")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn sha(d: &[u8]) -> String {
    Sha256::digest(d)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A fake sparse index for the crates `good` and `evil`. It answers every
/// request until the test ends.
fn index(good: &[u8]) -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
    let good = format!(
        "{{\"name\":\"good\",\"vers\":\"1.0.0\",\"cksum\":\"{}\"}}\n",
        sha(good)
    );
    // The real crate has other content than the one that A plants.
    let evil = format!(
        "{{\"name\":\"evil\",\"vers\":\"2.0.0\",\"cksum\":\"{}\"}}\n",
        sha(b"the published crate")
    );
    std::thread::spawn(move || {
        for s in l.incoming() {
            let Ok(mut s) = s else { continue };
            let mut buf = Vec::new();
            let mut b = [0u8; 4096];
            while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                match s.read(&mut b) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buf.extend_from_slice(&b[..n]),
                }
            }
            let head = String::from_utf8_lossy(&buf).into_owned();
            let body = match head.split(' ').nth(1) {
                Some("/go/od/good") => Some(&good),
                Some("/ev/il/evil") => Some(&evil),
                _ => None,
            };
            let _ = match body {
                Some(b) => write!(
                    s,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{b}",
                    b.len()
                ),
                None => write!(
                    s,
                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                ),
            };
        }
    });
    base
}

fn cmd(root: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_paddock"));
    c.env("HOME", root.join("home"))
        .env("XDG_CONFIG_HOME", root.join("cfg"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_RUNTIME_DIR", root.join("run"))
        // The promotion after each run would ask the real crates.io.
        .env("PADDOCK_NO_PROMOTE", "1")
        .env("PADDOCK_NO_DEDUPE", "1")
        .env_remove("DBUS_SESSION_BUS_ADDRESS");
    for v in paddock::proxy::PROXY_VARS {
        c.env_remove(v);
    }
    c
}

/// Run the fake harness with `script` in `project` on bwrap.
fn run(root: &Path, project: &Path, script: &str) -> Output {
    std::fs::write(
        root.join("cfg/paddock/harness.d/cachetest.toml"),
        format!(
            "name = \"cachetest\"\n[locate]\ncommands = [\"sh\"]\n[launch]\nargs = [\"-c\", {}]\n",
            serde_json::to_string(script).unwrap()
        ),
    )
    .unwrap();
    let out = cmd(root)
        .args([
            "run",
            "cachetest",
            "--backend",
            "bwrap",
            "--network",
            "none",
            "-y",
        ])
        .arg(project)
        .current_dir(project)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

/// What B sees in its crate cache: the listing and the good crate.
fn look(root: &Path, b: &Path) -> (String, String) {
    let script = format!(
        "ls \"$HOME/{INDEX_DIR}\" > seen.txt 2>/dev/null; cat \"$HOME/{INDEX_DIR}/good-1.0.0.crate\" > good.txt 2>/dev/null; true"
    );
    run(root, b, &script);
    let read = |f: &str| std::fs::read_to_string(b.join(f)).unwrap_or_default();
    (read("seen.txt"), read("good.txt"))
}

#[test]
fn a_planted_crate_stays_in_its_project_and_a_verified_one_is_shared() {
    let tmp = common::tempdir();
    let root = tmp.path();
    if !common::bwrap_works() || !overlay_works(root) {
        eprintln!("skipped: bwrap cannot start with an overlay here");
        return;
    }
    for d in ["cfg/paddock/harness.d", "home", "a", "b"] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    let run_dir = root.join("run");
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::set_permissions(
        &run_dir,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    let (a, b) = (root.join("a"), root.join("b"));
    for p in [&a, &b] {
        std::fs::write(p.join("Cargo.toml"), "[package]\nname = \"t\"\n").unwrap();
    }
    let good = b"a good crate";

    // A writes a planted crate and a good one into its cache view.
    run(
        root,
        &a,
        &format!(
            "mkdir -p \"$HOME/{INDEX_DIR}\" && printf 'planted code' > \"$HOME/{INDEX_DIR}/evil-2.0.0.crate\" && printf 'a good crate' > \"$HOME/{INDEX_DIR}/good-1.0.0.crate\""
        ),
    );

    // B, before any promotion, sees neither.
    let (seen, _) = look(root, &b);
    assert!(!seen.contains("evil") && !seen.contains("good"), "{seen}");

    // Promotion checks A's new crates against the fake index.
    let out = cmd(root)
        .arg("promote-caches")
        .arg(&a)
        .arg("--crates-index")
        .arg(index(good))
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{text}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        text.contains(
            "cargo-registry evil 2.0.0 refused: the cksum of the crates.io index differs"
        ),
        "{text}"
    );
    assert!(
        text.contains("cargo-registry good 1.0.0 promoted"),
        "{text}"
    );

    // B now reads the good crate through its lower layer, and still not the
    // planted one.
    let (seen, content) = look(root, &b);
    assert!(seen.contains("good-1.0.0.crate"), "{seen}");
    assert!(!seen.contains("evil"), "{seen}");
    assert_eq!(content.as_bytes(), good);
    let log = cmd(root).args(["log", "--json"]).output().unwrap();
    let log = String::from_utf8_lossy(&log.stdout);
    assert!(!log.contains("planted code"), "{log}");
}
