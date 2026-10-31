//! Opt-in login ports for app sandboxes: with `login_ports = true`, the port
//! that the app listens on is reachable on the host's 127.0.0.1, and other
//! ports are not; without it, nothing is. Needs a Wayland session with
//! wp_security_context_v1, so it is skipped elsewhere.

mod common;

use std::net::TcpStream;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

fn can_run() -> bool {
    std::env::var("WAYLAND_DISPLAY").is_ok()
        && paddock::wlsec::supported()
        && Command::new("python3").arg("-V").output().is_ok()
}

fn reachable(port: u16) -> bool {
    TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_millis(500)).is_ok()
}

fn start(cfg: &std::path::Path, ws: &std::path::Path, login_ports: bool, port: u16) -> Child {
    std::fs::create_dir_all(cfg.join("paddock/harness.d")).unwrap();
    std::fs::write(
        cfg.join("paddock/harness.d/porttest.toml"),
        format!("name = \"porttest\"\n[locate]\ncommands = [\"sh\"]\n[launch]\nargs = [\"-c\", \"exec python3 -m http.server {port} --bind 127.0.0.1\"]\n[app]\n"),
    )
    .unwrap();
    std::fs::write(
        cfg.join("paddock/config.toml"),
        format!("[apps.porttest]\nlogin_ports = {login_ports}\n"),
    )
    .unwrap();
    Command::new(env!("CARGO_BIN_EXE_paddock"))
        .args([
            "app",
            "run",
            "porttest",
            "--network",
            "none",
            "--env",
            "clean",
        ])
        .arg(ws)
        .env("XDG_CONFIG_HOME", cfg)
        // Never the user's own folders. The runtime folder stays: the app
        // needs the Wayland session.
        .env("XDG_DATA_HOME", cfg.join("../data"))
        .env("XDG_STATE_HOME", cfg.join("../state"))
        .env("XDG_CACHE_HOME", cfg.join("../cache"))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap()
}

#[test]
fn login_ports_open_only_listening_ports_and_only_when_enabled() {
    if !can_run() {
        eprintln!("skipped: needs a Wayland session with wp_security_context_v1 and python3");
        return;
    }
    let tmp = common::tempdir();
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let (port, other) = (47651, 47652);

    let mut on = start(&tmp.path().join("on"), &ws, true, port);
    let start_t = Instant::now();
    while !reachable(port) && start_t.elapsed() < Duration::from_secs(15) {
        std::thread::sleep(Duration::from_millis(200));
    }
    let open = reachable(port);
    let other_open = reachable(other);
    let _ = on.kill();
    let _ = on.wait();
    assert!(
        open,
        "the app port is not reachable with login_ports = true"
    );
    assert!(
        !other_open,
        "a port the app does not listen on is reachable"
    );
    std::thread::sleep(Duration::from_secs(1));
    assert!(
        !reachable(port),
        "the port stays open after the sandbox stopped"
    );

    let mut off = start(&tmp.path().join("off"), &ws, false, port);
    std::thread::sleep(Duration::from_secs(6));
    let open = reachable(port);
    let _ = off.kill();
    let _ = off.wait();
    assert!(!open, "the app port is reachable without login_ports");
}
