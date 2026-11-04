//! The usage box end to end, for three cases from a real check: 60 sandbox
//! homes of one profile, and a lapsed login that a fake refresh command
//! renews. Fake harness profiles and a fake Omarchy command in a temporary
//! home. No real harness, no vendor endpoint.

mod common;

use paddock::env::HostEnv;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

static EXEC: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn write_exec(p: &Path, text: &str) {
    use std::os::unix::fs::PermissionsExt;
    let _g = EXEC.lock().unwrap_or_else(|e| e.into_inner());
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
}

struct World {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    vars: BTreeMap<String, String>,
}

fn world(update: &str) -> World {
    // Not in /tmp: the box has its own /tmp.
    let tmp = common::tempdir();
    let home = tmp.path().join("home");
    let omarchy = home.join("omarchy");
    write_exec(&omarchy.join("bin/omarchy-agent-usage-update"), update);
    let run = tmp.path().join("run");
    std::fs::create_dir_all(&run).unwrap();
    std::fs::set_permissions(&run, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    let mut vars = BTreeMap::new();
    vars.insert("HOME".into(), home.display().to_string());
    vars.insert("OMARCHY_PATH".into(), omarchy.display().to_string());
    vars.insert("XDG_RUNTIME_DIR".into(), run.display().to_string());
    vars.insert("PATH".into(), std::env::var("PATH").unwrap());
    World {
        _tmp: tmp,
        home,
        vars,
    }
}

impl World {
    fn env(&self) -> HostEnv {
        HostEnv::from_parts(self.home.clone(), "demo".into(), 1, 1, self.vars.clone())
    }

    fn box_works(&self) -> bool {
        let _g = EXEC.lock().unwrap_or_else(|e| e.into_inner());
        paddock::usage_box::probe(&self.env()).is_ok()
    }

    fn update(&self) -> std::process::Output {
        let child = {
            let _g = EXEC.lock().unwrap_or_else(|e| e.into_inner());
            Command::new(env!("CARGO_BIN_EXE_paddock"))
                .args(["omarchy-usage-update", "--"])
                .env_clear()
                .envs(&self.vars)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        };
        child.wait_with_output().unwrap()
    }

    fn seen(&self) -> String {
        std::fs::read_to_string(self.home.join(".local/state/omarchy/agents/seen.txt"))
            .unwrap_or_default()
    }
}

#[test]
fn sixty_sandbox_homes_of_one_profile_are_all_counted() {
    let w = world(
        "#!/bin/sh\ns=\"$HOME/.local/state/omarchy/agents\"\nmkdir -p \"$s\"\nls -1 \"$HOME/.grok/sessions\" | wc -l > \"$s/seen.txt\"\n",
    );
    let homes = w.home.join(".local/share/paddock/homes/grok/shared");
    for n in 0..60 {
        let d = homes.join(format!(
            "a-project-with-a-rather-long-folder-name-{n:04}/.grok/sessions"
        ));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(format!("s-{n}.jsonl")), "{}\n").unwrap();
    }
    std::fs::create_dir_all(w.home.join(".grok/sessions")).unwrap();
    std::fs::write(w.home.join(".grok/sessions/host.jsonl"), "{}\n").unwrap();
    // One mount cannot take all 61 layers.
    let mut v = paddock::usage_box::view(&w.env());
    let groups = paddock::usage_box::stage::split(&mut v, &w.home.join("l"));
    assert!(!groups.is_empty());
    assert_eq!(v.left_out, 0);
    if !w.box_works() {
        eprintln!("skipped: the usage box cannot start here (bwrap, user namespaces, overlayfs)");
        return;
    }
    let out = w.update();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{err}");
    assert!(!err.contains("left out"), "{err}");
    assert_eq!(w.seen().trim(), "61", "{err}");
    // The stage's mounts never reach the host: only empty folders were
    // there, and paddock removed them.
    let run = PathBuf::from(&w.vars["XDG_RUNTIME_DIR"]).join("paddock");
    let left: Vec<_> = std::fs::read_dir(&run)
        .map(|r| r.flatten().map(|e| e.file_name()).collect())
        .unwrap_or_default();
    assert!(
        !left
            .iter()
            .any(|n| n.to_string_lossy().starts_with("usage-box-")),
        "{left:?}"
    );
}

/// The fake harness: with the argument `renew` it writes a renewed login,
/// as a real harness does when it renews its token, and counts its runs in
/// the project.
const FAKE_HARNESS: &str = r#"echo run >> "$PWD/runs"
[ "$0" = renew ] || exit 2
printf '{"access_token":"a","expires_at":4000000000,"refresh_token":"r2","which":"renewed"}' > "$HOME/.fakeagent/login.json"
"#;

#[test]
fn a_lapsed_login_is_renewed_once_by_the_profile_s_refresh_command() {
    let w = world(
        "#!/bin/sh\ns=\"$HOME/.local/state/omarchy/agents\"\nmkdir -p \"$s\"\ncat \"$HOME/.fakeagent/login.json\" > \"$s/seen.txt\"\n",
    );
    let cfg = w.home.join(".config/paddock/harness.d");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(
        cfg.join("fakeagent.toml"),
        format!(
            "name = \"fakeagent\"\n[locate]\ncommands = [\"sh\"]\n[launch]\nargs = [\"-c\", {}]\n[state]\nauth = [\".fakeagent/login.json\"]\nrefresh = [\"fakeagent\", \"renew\"]\n",
            serde_json::to_string(FAKE_HARNESS).unwrap()
        ),
    )
    .unwrap();
    let login = w.home.join(".fakeagent/login.json");
    let lapsed =
        r#"{"access_token":"a","expires_at":1000000000,"refresh_token":"r1","which":"lapsed"}"#;
    std::fs::create_dir_all(login.parent().unwrap()).unwrap();
    std::fs::write(&login, lapsed).unwrap();
    let e = w.env();
    assert_eq!(
        paddock::usage_box::refresh::due(&e, paddock::state::now()).len(),
        1
    );
    if !w.box_works() || !common::bwrap_works() {
        eprintln!("skipped: needs bwrap with user namespaces and overlayfs");
        return;
    }
    let out = w.update();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{err}");
    // The sandbox wrote the renewed login back to the store, and the box
    // showed it to the panel.
    assert!(std::fs::read_to_string(&login).unwrap().contains("renewed"));
    assert!(w.seen().contains("renewed"), "{err}");
    let runs = paddock::usage_box::refresh::scratch(&e, "fakeagent").join("runs");
    assert_eq!(std::fs::read_to_string(&runs).unwrap().lines().count(), 1);
    // Lapsed again within 10 minutes: no second run.
    std::fs::write(&login, lapsed).unwrap();
    let out = w.update();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(std::fs::read_to_string(&runs).unwrap().lines().count(), 1);
    // The renewed copy in paddock's login store is still the newest, so the
    // panel keeps seeing it.
    assert!(w.seen().contains("renewed"));
}
