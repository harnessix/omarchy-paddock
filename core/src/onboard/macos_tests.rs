//! The first-run decisions on a real Mac, in temporary folders.

use super::*;
use std::collections::BTreeMap;

fn env_in(t: &Path, path: &Path) -> HostEnv {
    let mut vars = BTreeMap::new();
    for (k, d) in [
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_CACHE_HOME", "cache"),
        ("XDG_STATE_HOME", "state"),
        ("XDG_RUNTIME_DIR", "run"),
    ] {
        vars.insert(k.to_string(), t.join(d).display().to_string());
    }
    vars.insert("PATH".into(), path.display().to_string());
    vars.insert("PADDOCK_NO_UPDATE_CHECK".into(), "1".into());
    let home = t.join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(path).unwrap();
    HostEnv::from_parts(home, "demo".into(), 501, 20, vars)
}

fn exec(p: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(p, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn this_mac_reports_its_version_and_sandbox_exec() {
    assert_eq!(Os::HOST, Os::Macos);
    let p = MacProbe::host();
    assert!(p.sandbox_exec, "{p:?}");
    let v = p.version.clone().expect("sw_vers prints a version");
    assert!(macos_major(&v).is_some(), "{v}");
    let (lines, _) = mac_system_lines(&p);
    assert!(lines[0].contains(&format!("macOS {v}")), "{lines:?}");
}

#[test]
fn doctor_on_a_mac_names_the_mac_parts_and_runs_no_linux_check() {
    let t = tempfile::tempdir().unwrap();
    let bin = t.path().join("bin");
    let env = env_in(t.path(), &bin);
    exec(&bin.join("git"));
    let (lines, req, opt) = mac_dep_lines(|c| env.which(c));
    assert!(req.is_empty(), "{lines:?}");
    assert!(opt.contains(&"brew install tmux"));
    // The whole report runs: the PATH holds only the fake git, and the
    // update check is off.
    crate::doctor::report(&env).unwrap();
}

#[test]
fn shims_reach_zsh_through_a_new_zshrc_and_go_away_again() {
    let t = tempfile::tempdir().unwrap();
    let bin = t.path().join("bin");
    let env = env_in(t.path(), &bin);
    exec(&bin.join("codex"));
    let r = crate::shims::install(&env).unwrap();
    let zshrc = env.home.join(".zshrc");
    assert_eq!(r.rc_files, vec![zshrc.clone()]);
    assert!(crate::shims::installed(&env));
    assert_eq!(crate::shims::active(&env), vec!["codex".to_string()]);

    // zsh reads the line and gets the function.
    let out = std::process::Command::new("/bin/zsh")
        .args(["-f", "-c", ". \"$1\" && whence -w codex", "zsh"])
        .arg(&zshrc)
        .env_clear()
        .env("HOME", &env.home)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("codex: function"), "{out:?}");

    let removed = crate::shims::remove(&env).unwrap();
    assert!(removed.contains(&zshrc), "{removed:?}");
    assert!(!zshrc.exists());
    assert!(!crate::shims::script_file(&env).exists());
    assert!(!crate::shims::installed(&env));
}
