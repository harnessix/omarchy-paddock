//! The login keeper end to end, with bwrap: the agent sandbox gets the
//! Codex login without its refresh token, the keeper refreshes it in a
//! sandbox of its own before the access token runs out, spends each refresh
//! token once, writes the new login back to the host, and the agent sandbox
//! sees the new access token while it runs. The `codex` here is a fake: a
//! script that plays the app-server's refresh and, as the harness, records
//! what it reads. A user profile replaces the built-in one, so the real
//! codex is never found. Temporary folders only; nothing leaves the machine.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

const FAKE_CODEX: &str = r#"#!/usr/bin/python3
import base64, json, os, sys, time

def b64(v):
    return base64.urlsafe_b64encode(json.dumps(v).encode()).decode().rstrip("=")

def exp_of(at):
    p = at.split(".")[1]
    return json.loads(base64.urlsafe_b64decode(p + "=" * (-len(p) % 4)))["exp"]

if "app-server" in sys.argv:
    home = os.environ["CODEX_HOME"]
    auth = os.path.join(home, "auth.json")
    used = os.path.join(home, "used.log")
    for line in sys.stdin:
        msg = json.loads(line)
        mid = msg.get("id")
        if msg.get("method") == "initialize":
            print(json.dumps({"id": mid, "result": {}}), flush=True)
        elif msg.get("method") == "account/read":
            a = json.load(open(auth))
            rt = a["tokens"]["refresh_token"]
            seen = open(used).read().split() if os.path.exists(used) else []
            with open(used, "a") as f:
                f.write(("REUSED-" if rt in seen or not rt else "") + rt + "\n")
            n = int(rt.split("-")[1]) + 1 if rt else 0
            a["tokens"]["refresh_token"] = "rt-%d" % n
            a["tokens"]["access_token"] = "%s.%s.sig" % (b64({"alg": "none"}), b64({"exp": int(time.time()) + 3600}))
            tmp = auth + ".tmp"
            json.dump(a, open(tmp, "w"))
            os.replace(tmp, auth)
            print(json.dumps({"id": mid, "result": {"account": {"type": "chatgpt"}}}), flush=True)
else:
    path = os.path.expanduser("~/.codex/auth.json")
    with open("seen.log", "w") as out:
        for _ in range(12):
            a = json.load(open(path))
            t = a["tokens"]
            out.write("%s %d\n" % (t["refresh_token"] or "-", exp_of(t["access_token"])))
            out.flush()
            time.sleep(0.5)
"#;

fn can_run() -> bool {
    let ok = |c: &str, a: &[&str]| {
        Command::new(c)
            .args(a)
            .output()
            .is_ok_and(|o| o.status.success())
    };
    ok("bwrap", &["--unshare-user", "--ro-bind", "/", "/", "true"])
        && Command::new("pasta").arg("--version").output().is_ok()
        && ok("python3", &["-c", "pass"])
}

fn login(exp: u64, rt: &str) -> String {
    use base64::Engine as _;
    let b64 = |v: serde_json::Value| {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v.to_string())
    };
    let at = format!(
        "{}.{}.sig",
        b64(serde_json::json!({"alg": "none"})),
        b64(serde_json::json!({"exp": exp}))
    );
    serde_json::json!({
        "auth_mode": "chatgpt",
        "tokens": {"id_token": "e30.e30.sig", "access_token": at, "refresh_token": rt, "account_id": "acct-1"},
        "last_refresh": "2026-11-01T00:00:00Z"
    })
    .to_string()
}

fn exp_of(login: &str) -> u64 {
    use base64::Engine as _;
    let v: serde_json::Value = serde_json::from_str(login).unwrap();
    let at = v["tokens"]["access_token"].as_str().unwrap().to_string();
    let p = at.split('.').nth(1).unwrap().to_string();
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(p)
        .unwrap();
    serde_json::from_slice::<serde_json::Value>(&raw).unwrap()["exp"]
        .as_u64()
        .unwrap()
}

#[test]
fn the_keeper_refreshes_once_and_the_sandbox_never_holds_the_refresh_token() {
    if !can_run() {
        eprintln!("skipped: needs bwrap, pasta and python3");
        return;
    }
    let tmp = common::tempdir();
    let root: PathBuf = tmp.path().to_path_buf();
    // The fake codex is installed outside the home, as a real one would be.
    let home = root.join("u/home");
    let bin = root.join("tools/bin");
    for d in [
        home.join(".codex"),
        bin.clone(),
        root.join("project"),
        root.join("cfg/paddock/harness.d"),
    ] {
        std::fs::create_dir_all(d).unwrap();
    }
    let run = root.join("run");
    std::fs::create_dir_all(&run).unwrap();
    std::fs::set_permissions(&run, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    let fake = bin.join("codex");
    std::fs::write(&fake, FAKE_CODEX).unwrap();
    std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    // Replaces the built-in profile: no mise lookup, only this PATH.
    std::fs::write(
        root.join("cfg/paddock/harness.d/codex.toml"),
        "name = \"codex\"\n[locate]\ncommands = [\"codex\"]\n[state]\nauth = [\".codex/auth.json\"]\n",
    )
    .unwrap();
    std::fs::write(
        root.join("cfg/paddock/config.toml"),
        "[logins]\ncheck_interval = 1\nrefresh_margin = 1800\n",
    )
    .unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    // Ten minutes left: due for the keeper, not yet for Codex itself.
    let store = home.join(".codex/auth.json");
    std::fs::write(&store, login(now + 600, "rt-1")).unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_paddock"))
        .args([
            "run",
            "codex",
            "--backend",
            "bwrap",
            "--network",
            "none",
            "--env",
            "clean",
            "-y",
        ])
        .arg(root.join("project"))
        .env_clear()
        .env("HOME", &home)
        .env("USER", "demo")
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("XDG_CONFIG_HOME", root.join("cfg"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_RUNTIME_DIR", &run)
        .current_dir(root.join("project"))
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");

    let seen = std::fs::read_to_string(root.join("project/seen.log")).unwrap();
    let lines: Vec<(&str, u64)> = seen
        .lines()
        .map(|l| {
            let (rt, exp) = l.split_once(' ').unwrap();
            (rt, exp.parse().unwrap())
        })
        .collect();
    assert!(!lines.is_empty(), "{seen}");
    assert!(
        lines.iter().all(|(rt, _)| *rt == "-"),
        "the sandbox held a refresh token: {seen}"
    );
    assert!(
        lines.iter().any(|(_, exp)| *exp >= now + 3000),
        "the sandbox never got the new access token: {seen}"
    );

    // The host has the refreshed login, refresh token and all.
    let after = std::fs::read_to_string(&store).unwrap();
    let v: serde_json::Value = serde_json::from_str(&after).unwrap();
    assert_eq!(v["tokens"]["refresh_token"], "rt-2", "{after}");
    assert!(exp_of(&after) >= now + 3000);
    // Each refresh token was spent once, and the empty one never.
    let keepers = root.join("state/paddock/keepers");
    let used: String = std::fs::read_dir(&keepers)
        .unwrap()
        .flatten()
        .map(|e| std::fs::read_to_string(e.path().join("home/.codex/used.log")).unwrap_or_default())
        .collect();
    assert_eq!(used, "rt-1\n", "{used}");
    assert!(Path::new(&store).is_file());
}
