use super::*;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

fn env(t: &Path) -> HostEnv {
    let mut vars = BTreeMap::new();
    vars.insert("PATH".into(), "/usr/bin".into());
    HostEnv::from_parts(t.join("home"), "demo".into(), 1, 1, vars)
}

fn set_mtime(p: &Path, secs: u64) {
    let t = filetime_timespec(secs);
    let c_path = std::ffi::CString::new(p.as_os_str().as_encoded_bytes()).unwrap();
    let times = [t, t];
    unsafe {
        libc::utimensat(libc::AT_FDCWD, c_path.as_ptr(), times.as_ptr(), 0);
    }
}

fn filetime_timespec(secs: u64) -> libc::timespec {
    libc::timespec {
        tv_sec: secs as libc::time_t,
        tv_nsec: 0,
    }
}

#[test]
fn data_roots_skip_shared_xdg_folders() {
    assert_eq!(data_root(".codex/auth.json").as_deref(), Some(".codex"));
    assert_eq!(data_root(".claude.json").as_deref(), Some(".claude.json"));
    assert_eq!(
        data_root(".local/share/opencode/auth.json").as_deref(),
        Some(".local/share/opencode")
    );
    assert_eq!(
        data_root(".config/cursor/auth.json").as_deref(),
        Some(".config/cursor")
    );
    assert_eq!(
        data_root(".kimi-code/credentials/").as_deref(),
        Some(".kimi-code")
    );
    assert_eq!(
        data_root(".gemini/antigravity-cli/x").as_deref(),
        Some(".gemini")
    );
    for bad in [
        "",
        "/etc/passwd",
        "../x",
        ".local/bin/x",
        ".cache/x",
        ".config",
    ] {
        assert_eq!(data_root(bad), None, "{bad}");
    }
}

#[test]
fn the_view_merges_host_and_sandbox_homes_and_nothing_else() {
    let t = tempfile::tempdir().unwrap();
    let e = env(t.path());
    let home = &e.home;
    std::fs::create_dir_all(home.join(".codex/sessions")).unwrap();
    std::fs::create_dir_all(home.join(".ssh")).unwrap();
    std::fs::write(home.join(".claude.json"), "{}").unwrap();
    let homes = e.paddock_data().join("homes/codex");
    let a = homes.join("shared/proj-a/.codex");
    let b = homes.join("shared/proj-b/.codex");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    // A link that a sandbox planted is never a layer.
    let c = homes.join("shared/proj-c");
    std::fs::create_dir_all(&c).unwrap();
    std::os::unix::fs::symlink(home.join(".ssh"), c.join(".codex")).unwrap();
    let v = view(&e);
    let codex = v
        .mounts
        .iter()
        .find_map(|m| match m {
            Mount::Overlay { dest, lowers } if dest.ends_with(".codex") => Some(lowers),
            _ => None,
        })
        .expect("no overlay for .codex");
    assert_eq!(codex.len(), 3, "{codex:?}");
    assert_eq!(
        codex.last(),
        Some(&home.join(".codex")),
        "the host is on top"
    );
    assert!(codex.contains(&a) && codex.contains(&b));
    assert!(v.mounts.contains(&Mount::Ro(home.join(".claude.json"))));
    let args = bwrap_args(&e, &v, Path::new("/x/update"), &["--force".into()]);
    let joined = args.join(" ");
    assert!(!joined.contains(".ssh"), "{joined}");
    assert!(joined.contains(&format!("--tmpfs {}", home.display())));
    assert!(joined.contains(&format!("--bind {0} {0}", state_dir(&e).display())));
    assert!(joined.ends_with("-- /x/update --force"));
    // Only the bind of the record and cache folders is writable.
    let rw: Vec<_> = args
        .windows(2)
        .filter(|w| w[0] == "--bind")
        .map(|w| w[1].clone())
        .collect();
    assert_eq!(
        rw,
        vec![
            state_dir(&e).display().to_string(),
            cache_dir(&e).display().to_string()
        ]
    );
}

#[test]
fn many_sandbox_homes_stay_within_the_mount_option_limit() {
    let t = tempfile::tempdir().unwrap();
    let e = env(t.path());
    let homes = e.paddock_data().join("homes/grok/shared");
    for n in 0..200 {
        std::fs::create_dir_all(homes.join(format!("project-{n:04}/.grok"))).unwrap();
    }
    let v = view(&e);
    let lowers = v
        .mounts
        .iter()
        .find_map(|m| match m {
            Mount::Overlay { dest, lowers } if dest.ends_with(".grok") => Some(lowers),
            _ => None,
        })
        .unwrap();
    let size: usize = lowers.iter().map(|p| p.as_os_str().len() + 10).sum();
    assert!(size <= LOWER_BUDGET);
    assert_eq!(lowers.len() + v.left_out, 200);
}

#[test]
fn only_the_listed_variables_pass_and_dbus_is_unset() {
    let t = tempfile::tempdir().unwrap();
    let mut vars = BTreeMap::new();
    for (k, v) in [
        ("PATH", "/usr/bin"),
        ("LANG", "C"),
        ("OMARCHY_PATH", "/o"),
        ("SSH_AUTH_SOCK", "/s"),
        ("OPENAI_API_KEY", "k"),
        ("DBUS_SESSION_BUS_ADDRESS", "unix:path=/run/user/1000/bus"),
    ] {
        vars.insert(k.to_string(), v.to_string());
    }
    let mut e = HostEnv::from_parts(t.path().join("h"), "demo".into(), 1, 1, vars);
    let rt = t.path().join("run-user-1");
    std::fs::create_dir_all(&rt).unwrap();
    e.runtime_dir = Some(rt.clone());
    let b = box_env(&e);
    assert!(!b.contains_key("DBUS_SESSION_BUS_ADDRESS"));
    assert!(!b.contains_key("SSH_AUTH_SOCK"));
    assert!(!b.contains_key("OPENAI_API_KEY"));
    assert_eq!(
        b.get("HTTPS_PROXY").map(String::as_str),
        Some("http://127.0.0.1:3131")
    );
    assert_eq!(
        b.get("HTTP_PROXY").map(String::as_str),
        Some("http://127.0.0.1:3131")
    );
    let v = view(&e);
    let args = bwrap_args(&e, &v, Path::new("/x/update"), &[]);
    assert!(args.contains(&"--unshare-net".to_string()));
}

#[test]
fn without_bwrap_the_box_fails_closed() {
    let t = tempfile::tempdir().unwrap();
    let mut e = env(t.path());
    e.path = vec![t.path().join("nothing")];
    let err = probe(&e).unwrap_err();
    let line = fail_line(&err);
    assert!(line.contains("bwrap is not on PATH"), "{line}");
    assert!(line.contains("paddock shims remove"));
    assert!(!line.contains('\n'));
}

#[test]
fn every_shipped_profile_with_a_collector_is_in_the_table() {
    let t = tempfile::tempdir().unwrap();
    let e = env(t.path());
    let names = crate::profile::HarnessFactory::new(&e).unwrap().names();
    for name in &names {
        let p = crate::profile::builtin(name).unwrap();
        let stem = name
            .strip_suffix("-agent")
            .or_else(|| name.strip_suffix("-cli"))
            .unwrap_or(name.as_str());
        if let Some(c) = COLLECTORS
            .iter()
            .find(|c| c.id == name || c.id == stem || p.locate.commands.iter().any(|b| b == c.id))
        {
            assert_eq!(
                c.profile,
                Some(name.as_str()),
                "collector {} matches shipped profile {name} but does not name it",
                c.id
            );
        }
    }
    for c in COLLECTORS {
        if let Some(p) = c.profile {
            assert!(
                names.iter().any(|n| n == p),
                "collector {} names unknown profile {p}",
                c.id
            );
        }
    }
}

fn check_collector_entry(id: &str) {
    let c = collector(id).unwrap_or_else(|| panic!("missing collector {id}"));
    let t = tempfile::tempdir().unwrap();
    let e = env(t.path());
    let f_map = folders(&e);
    let l_map = login_files(&e);
    let hosts = allowed_hosts(&e);
    for h in c.hosts {
        assert!(hosts.iter().any(|x| x == h), "{id}: missing host {h}");
    }
    match c.profile {
        Some(prof) => {
            let p = crate::profile::builtin(prof).expect("builtin profile");
            for ep in &p.network.endpoints {
                assert!(hosts.contains(ep), "{id}: missing profile endpoint {ep}");
            }
            for folder in c.folders {
                let root = data_root(folder).unwrap();
                let covered = f_map
                    .iter()
                    .find(|(k, _)| Path::new(&root).starts_with(Path::new(k)))
                    .unwrap_or_else(|| panic!("{id}: folder {folder} not in folders()"));
                assert!(
                    covered.1.contains(prof),
                    "{id}: folder {folder} does not map to profile {prof}"
                );
            }
            for lf in c.login_files {
                let (profs, _) = l_map
                    .get(*lf)
                    .unwrap_or_else(|| panic!("{id}: login file {lf} not in login_files()"));
                assert!(profs.contains(prof), "{id}: {lf} missing profile {prof}");
                let host_f = e.home.join(lf);
                let sb1 = e
                    .paddock_data()
                    .join("homes")
                    .join(prof)
                    .join("shared/p1")
                    .join(lf);
                let sb2 = e
                    .paddock_data()
                    .join("homes")
                    .join(prof)
                    .join("shared/p2")
                    .join(lf);
                for (path, exp, mt) in [
                    (&host_f, 1000u64, 10u64),
                    (&sb1, 3000, 20),
                    (&sb2, 2000, 30),
                ] {
                    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                    std::fs::write(path, format!(r#"{{"expires_at":{exp}}}"#)).unwrap();
                    set_mtime(path, mt);
                }
                let v = view(&e);
                assert_eq!(
                    v.logins.get(&host_f),
                    Some(&sb1),
                    "{id}: newest login not chosen for {lf}"
                );
            }
        }
        None => {
            for folder in c.folders {
                let root = data_root(folder).unwrap();
                assert!(
                    f_map
                        .keys()
                        .any(|k| Path::new(&root).starts_with(Path::new(k))),
                    "{id}: folder {folder} not in folders()"
                );
                let host_d = e.home.join(&root);
                std::fs::create_dir_all(&host_d).unwrap();
                let v = view(&e);
                assert!(
                    v.mounts.contains(&Mount::Ro(host_d)),
                    "{id}: host folder {root} not mounted Ro"
                );
            }
        }
    }
}

#[test]
fn collector_entry_agy() {
    check_collector_entry("agy");
}
#[test]
fn collector_entry_claude() {
    check_collector_entry("claude");
}
#[test]
fn collector_entry_codex() {
    check_collector_entry("codex");
}
#[test]
fn collector_entry_copilot() {
    check_collector_entry("copilot");
}
#[test]
fn collector_entry_cursor() {
    check_collector_entry("cursor");
}
#[test]
fn collector_entry_deepseek() {
    check_collector_entry("deepseek");
}
#[test]
fn collector_entry_devin() {
    check_collector_entry("devin");
}
#[test]
fn collector_entry_fireworks() {
    check_collector_entry("fireworks");
}
#[test]
fn collector_entry_grok() {
    check_collector_entry("grok");
}
#[test]
fn collector_entry_grokbot() {
    check_collector_entry("grokbot");
}
#[test]
fn collector_entry_hermes() {
    check_collector_entry("hermes");
}
#[test]
fn collector_entry_kimchi() {
    check_collector_entry("kimchi");
}
#[test]
fn collector_entry_kimi() {
    check_collector_entry("kimi");
}
#[test]
fn collector_entry_local() {
    check_collector_entry("local");
}
#[test]
fn collector_entry_minimax() {
    check_collector_entry("minimax");
}
#[test]
fn collector_entry_muse() {
    check_collector_entry("muse");
}
#[test]
fn collector_entry_ollama() {
    check_collector_entry("ollama");
}
#[test]
fn collector_entry_opencode() {
    check_collector_entry("opencode");
}
#[test]
fn collector_entry_openrouter() {
    check_collector_entry("openrouter");
}
#[test]
fn collector_entry_qwen() {
    check_collector_entry("qwen");
}
#[test]
fn collector_entry_synthetic() {
    check_collector_entry("synthetic");
}
#[test]
fn collector_entry_vibe() {
    check_collector_entry("vibe");
}
#[test]
fn collector_entry_zai() {
    check_collector_entry("zai");
}

#[test]
fn freshest_login_prefers_latest_expiry_then_latest_mtime() {
    let t = tempfile::tempdir().unwrap();
    let e = env(t.path());
    let rel = ".claude/.credentials.json";
    let host_f = e.home.join(rel);
    let sb1 = e
        .paddock_data()
        .join("homes/claude/shared/proj-a")
        .join(rel);
    let sb2 = e
        .paddock_data()
        .join("homes/claude/shared/proj-b")
        .join(rel);
    for p in [&host_f, &sb1, &sb2] {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    }
    // sb1 has the latest token expiry (3_000_000_000 ms), even though sb2 has
    // a newer mtime.
    std::fs::write(&host_f, r#"{"claudeAiOauth":{"expiresAt":1000000000000}}"#).unwrap();
    std::fs::write(&sb1, r#"{"claudeAiOauth":{"expiresAt":3000000000000}}"#).unwrap();
    std::fs::write(&sb2, r#"{"claudeAiOauth":{"expiresAt":2000000000000}}"#).unwrap();
    set_mtime(&host_f, 100);
    set_mtime(&sb1, 200);
    set_mtime(&sb2, 300);
    let v = view(&e);
    assert_eq!(v.logins.get(&host_f), Some(&sb1));
    assert!(v.mounts.contains(&Mount::Login {
        src: sb1.clone(),
        dest: host_f.clone(),
    }));

    // Without an expiry field, the latest mtime wins.
    let grok_rel = ".grok/auth.json";
    let g_host = e.home.join(grok_rel);
    let g_sb1 = e
        .paddock_data()
        .join("homes/grok/shared/proj-a")
        .join(grok_rel);
    let g_sb2 = e
        .paddock_data()
        .join("homes/grok/shared/proj-b")
        .join(grok_rel);
    for p in [&g_host, &g_sb1, &g_sb2] {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    }
    std::fs::write(&g_host, r#"{"token":"host"}"#).unwrap();
    std::fs::write(&g_sb1, r#"{"token":"sb1"}"#).unwrap();
    std::fs::write(&g_sb2, r#"{"token":"sb2"}"#).unwrap();
    set_mtime(&g_host, 100);
    set_mtime(&g_sb1, 200);
    set_mtime(&g_sb2, 300);
    let v = view(&e);
    assert_eq!(v.logins.get(&g_host), Some(&g_sb2));
}

#[test]
fn codex_kept_login_comes_from_the_keeper_copy() {
    let t = tempfile::tempdir().unwrap();
    let e = env(t.path());
    let rel = ".codex/auth.json";
    let host_f = e.home.join(rel);
    let sb = e.paddock_data().join("homes/codex/shared/proj-a").join(rel);
    std::fs::create_dir_all(host_f.parent().unwrap()).unwrap();
    std::fs::create_dir_all(sb.parent().unwrap()).unwrap();
    let kept = br#"{"tokens":{"access_token":"opaque","refresh_token":"rt-live"}}"#;
    let stripped = crate::keeper::strip(kept).unwrap();
    std::fs::write(&host_f, kept).unwrap();
    std::fs::write(&sb, &stripped).unwrap();
    set_mtime(&host_f, 100);
    set_mtime(&sb, 500);
    let v = view(&e);
    assert_eq!(v.logins.get(&host_f), Some(&host_f));
}

#[test]
fn isolated_logins_and_homes_are_never_shown() {
    let t = tempfile::tempdir().unwrap();
    let e = env(t.path());
    let rel = ".claude/.credentials.json";
    let host_f = e.home.join(rel);
    let shared = e
        .paddock_data()
        .join("homes/claude/shared/proj-a")
        .join(rel);
    let iso1 = e
        .paddock_data()
        .join("homes/claude/isolated/proj-b")
        .join(rel);
    let iso2 = e
        .paddock_data()
        .join("homes/claude/isolated-custom/proj-c")
        .join(rel);
    let iso3_home = e.paddock_data().join("homes/claude/work/proj-d");
    let iso3 = iso3_home.join(rel);
    for p in [&host_f, &shared, &iso1, &iso2, &iso3] {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    }
    std::fs::write(iso3_home.join(".paddock-origin"), r#"{"auth":"isolated"}"#).unwrap();
    std::fs::write(&host_f, r#"{"claudeAiOauth":{"expiresAt":1000000000000}}"#).unwrap();
    std::fs::write(&shared, r#"{"claudeAiOauth":{"expiresAt":2000000000000}}"#).unwrap();
    std::fs::write(&iso1, r#"{"claudeAiOauth":{"expiresAt":9000000000000}}"#).unwrap();
    std::fs::write(&iso2, r#"{"claudeAiOauth":{"expiresAt":9000000000000}}"#).unwrap();
    std::fs::write(&iso3, r#"{"claudeAiOauth":{"expiresAt":9000000000000}}"#).unwrap();
    let homes = sandbox_homes(&e, "claude");
    assert_eq!(homes.len(), 1);
    let v = view(&e);
    assert_eq!(v.logins.get(&host_f), Some(&shared));
    let args = bwrap_args(&e, &v, Path::new("/x/update"), &[]).join(" ");
    assert!(!args.contains("isolated"));
    assert!(!args.contains("proj-d"));
}

#[test]
fn proxy_refuses_unlisted_host_and_names_it() {
    use crate::proxy::allow::{Allowlist, ApiPolicy, Refusals};
    let t = tempfile::tempdir().unwrap();
    let e = env(t.path());
    let sock = t.path().join("proxy.sock");
    let hosts = allowed_hosts(&e);
    let (list, _) = Allowlist::parse(&hosts);
    assert!(list.check("api.anthropic.com", 443).is_ok());
    assert!(list.check("daily-cloudcode-pa.googleapis.com", 443).is_ok());
    assert!(list.check("unlisted.example", 443).is_err());

    let log: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
    let log2 = log.clone();
    let refusals = Refusals::new(std::sync::Arc::new(move |host: &str, why: &str| {
        log2.lock().unwrap().push(format!(
            "paddock: the usage box refused a connection to {host} ({why})"
        ));
    }));
    let cfg = crate::proxy::HostProxy {
        api: Some(ApiPolicy {
            list,
            refusals: Some(refusals),
        }),
        ..Default::default()
    };
    crate::proxy::serve(&sock, cfg).unwrap();
    let mut s = UnixStream::connect(&sock).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(b"CONNECT unlisted.example:443 HTTP/1.1\r\nHost: unlisted.example:443\r\n\r\n")
        .unwrap();
    let mut resp = String::new();
    let _ = s.read_to_string(&mut resp);
    assert!(resp.starts_with("HTTP/1.1 403 Forbidden"), "{resp}");
    let lines = log.lock().unwrap().clone();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("unlisted.example"), "{lines:?}");
}
