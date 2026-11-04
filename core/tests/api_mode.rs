//! Network mode api: `paddock explain` lists the allowed hosts and their
//! sources, and a bwrap sandbox has no network but the proxy variables. A
//! direct connection fails, and a CONNECT to a host not on the list gets 403.
//! Temporary folders only. No outside host is contacted: the refused names
//! are in reserved domains, and the direct target is a documentation address
//! that a sandbox without network cannot reach.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

fn setup(root: &Path, script: &str) {
    let cfg = root.join("cfg/paddock");
    std::fs::create_dir_all(cfg.join("harness.d")).unwrap();
    for d in ["project", "home"] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    let run = root.join("run");
    std::fs::create_dir_all(&run).unwrap();
    std::fs::set_permissions(&run, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    std::fs::write(
        cfg.join("harness.d/apicheck.toml"),
        format!(
            "name = \"apicheck\"\n[locate]\ncommands = [\"sh\"]\n[launch]\nargs = [\"-c\", {}]\n[network]\nendpoints = [\"model.example.invalid\"]\n",
            serde_json::to_string(script).unwrap()
        ),
    )
    .unwrap();
}

fn cmd(root: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_paddock"));
    c.env("HOME", root.join("home"))
        .env("XDG_CONFIG_HOME", root.join("cfg"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_RUNTIME_DIR", root.join("run"))
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
        .current_dir(root.join("project"));
    for v in paddock::proxy::PROXY_VARS {
        c.env_remove(v);
    }
    c
}

#[test]
fn explain_lists_the_allowed_hosts_and_their_sources() {
    let tmp = common::tempdir();
    let root: PathBuf = tmp.path().to_path_buf();
    setup(&root, "true");
    let out = cmd(&root)
        .args([
            "explain",
            "apicheck",
            "--network",
            "api",
            "--allow",
            "*.docs.example.org",
        ])
        .arg(root.join("project"))
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{text}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("network   Api"), "{text}");
    assert!(
        text.contains(
            "allow  model.example.invalid\n         profile apicheck [network] endpoints"
        ),
        "{text}"
    );
    assert!(
        text.contains("allow  *.docs.example.org\n         --allow"),
        "{text}"
    );
    assert!(text.contains("HTTPS_PROXY=http://127.0.0.1:3131"), "{text}");
}

#[test]
fn a_bwrap_sandbox_in_api_mode_has_only_the_proxy() {
    let curl = Command::new("curl").arg("--version").output().is_ok();
    if !common::bwrap_works() || !curl {
        eprintln!("skipped: needs bwrap and curl");
        return;
    }
    let tmp = common::tempdir();
    let root: PathBuf = tmp.path().to_path_buf();
    let script = concat!(
        "env > env.txt; ",
        "curl -s --noproxy '*' --max-time 5 -o /dev/null http://192.0.2.1/; echo $? > direct.txt; ",
        "curl -s --max-time 10 -o /dev/null -w '%{http_connect}' https://refused.example.invalid/ > refused.txt; ",
        "curl -s --max-time 10 -o /dev/null -w '%{http_code}' http://model.example.invalid/ > plain.txt",
    );
    setup(&root, script);
    let out = cmd(&root)
        .args([
            "run",
            "apicheck",
            "--backend",
            "bwrap",
            "--network",
            "api",
            "-y",
        ])
        .arg(root.join("project"))
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    let read = |f: &str| std::fs::read_to_string(root.join("project").join(f)).unwrap_or_default();
    let env = read("env.txt");
    assert!(
        env.contains("HTTPS_PROXY=http://127.0.0.1:3131"),
        "{err}\n{env}"
    );
    assert!(env.contains("NO_PROXY=127.0.0.1,localhost"), "{env}");
    let direct = read("direct.txt");
    assert_ne!(direct.trim(), "0", "a direct connection worked");
    assert!(!direct.trim().is_empty(), "{err}");
    assert_eq!(read("refused.txt"), "403", "{err}");
    assert_eq!(read("plain.txt"), "403", "{err}");
    let log = cmd(&root).args(["log", "--json"]).output().unwrap();
    let log = String::from_utf8_lossy(&log.stdout);
    assert!(
        log.contains("api: refused.example.invalid: not on the list"),
        "{log}"
    );
}
