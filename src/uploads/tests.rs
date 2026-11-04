use super::*;
use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt;

const ROW: &str = "session-log-deepseek";

fn off_rows(text: &str) -> usize {
    text.matches("- id: session-log-deepseek\n  config:\n    enabled: false\n")
        .count()
}

#[test]
fn the_row_goes_at_the_end_of_each_form_of_patch_file() {
    // No file, an empty file, comments only and `[]`.
    for text in ["", "\n", "# mine\n", "[]\n", "# mine\n[]\n"] {
        let out = patch_text(text, ROW, true).unwrap().unwrap();
        assert_eq!(off_rows(&out), 1, "{text:?}: {out}");
        assert!(!out.contains("[]"), "{out}");
        assert_eq!(out.contains("# mine"), text.contains("# mine"));
    }
    // A block list keeps its rows, and paddock's row comes last.
    let user = "# my patch\n- id: session-log-deepseek\n  config:\n    enabled: true\n- id: llm\n  config: {}\n";
    let out = patch_text(user, ROW, true).unwrap().unwrap();
    assert!(out.starts_with(user), "{out}");
    assert!(out.ends_with("# paddock: end\n"), "{out}");
    // A list in JSON form.
    let out = patch_text(r#"[{"id": "llm", "config": {}}]"#, ROW, true)
        .unwrap()
        .unwrap();
    assert!(
        out.starts_with("- {\"config\":{},\"id\":\"llm\"}\n"),
        "{out}"
    );
    assert_eq!(off_rows(&out), 1);
    // A mapping is no patch list: the file stays.
    assert!(patch_text("id: x\n", ROW, true).is_err());
}

#[test]
fn a_later_start_replaces_or_takes_out_the_row() {
    let user = "- id: llm\n  config: {}\n";
    let once = patch_text(user, ROW, true).unwrap().unwrap();
    let twice = patch_text(&once, ROW, true).unwrap().unwrap();
    assert_eq!(once, twice);
    // The switch is on: paddock's row goes, the user's rows stay.
    assert_eq!(patch_text(&once, ROW, false).unwrap().unwrap(), user);
    // A file of paddock's row alone becomes an empty list, not a file of
    // comments, which stops dsh at boot.
    let alone = patch_text("", ROW, true).unwrap().unwrap();
    assert_eq!(patch_text(&alone, ROW, false).unwrap().unwrap(), "[]\n");
    // Without paddock's row, the switch changes nothing.
    assert_eq!(patch_text(user, ROW, false).unwrap(), None);
    assert_eq!(patch_text("", ROW, false).unwrap(), None);
}

#[test]
fn the_switch_is_read_from_the_profile_table() {
    let on = "[profiles.dsh]\nupload_session_logs = true\n";
    assert!(switch_in(on, "dsh", "upload_session_logs").unwrap());
    assert!(!switch_in(on, "grok", "upload_session_logs").unwrap());
    assert!(!switch_in("", "dsh", "upload_session_logs").unwrap());
    assert!(switch_in(
        "[profiles.dsh]\nupload_session_logs = \"yes\"\n",
        "dsh",
        "upload_session_logs"
    )
    .is_err());
}

struct World {
    _tmp: tempfile::TempDir,
    env: HostEnv,
    project: PathBuf,
}

fn world(config: &str) -> World {
    let tmp = tempfile::tempdir().unwrap();
    let uid = std::fs::metadata(tmp.path()).unwrap().uid();
    let home = tmp.path().join("home");
    let project = tmp.path().join("proj");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    let mut vars = BTreeMap::new();
    for (k, d) in [
        ("XDG_CONFIG_HOME", "cfg"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_STATE_HOME", "state"),
        ("XDG_CACHE_HOME", "cache"),
        ("XDG_RUNTIME_DIR", "run"),
    ] {
        vars.insert(k.to_string(), tmp.path().join(d).display().to_string());
    }
    let env = HostEnv::from_parts(home, "t".into(), uid, uid, vars);
    std::fs::create_dir_all(env.paddock_config()).unwrap();
    std::fs::write(env.paddock_config().join("config.toml"), config).unwrap();
    World {
        _tmp: tmp,
        env,
        project,
    }
}

/// Build a dsh sandbox and write its home as a start does.
fn start(w: &World) -> SandboxSpec {
    let h = crate::profile::Harness {
        profile: crate::profile::builtin("dsh").unwrap(),
        exe: "/usr/bin/true".into(),
        install_mounts: vec![],
        path_dirs: vec![],
        source: "test".into(),
    };
    let opts = crate::build::Options {
        harness: "dsh".into(),
        project: Some(w.project.clone()),
        assume_yes: true,
        ..Default::default()
    };
    let spec = crate::build::build_with(&w.env, &opts, h).unwrap().spec;
    assert!(crate::privacy::write_home(&spec).unwrap().is_empty());
    spec
}

fn notice(spec: &SandboxSpec) -> Vec<&String> {
    spec.notices
        .iter()
        .filter(|n| n.contains("session-log upload"))
        .collect()
}

#[test]
fn dsh_runs_without_the_session_log_upload_unless_the_switch_keeps_it() {
    let w = world("");
    // The host has a home patch of its own.
    let host = w.env.home.join(".dsh/cordis.patch.yml");
    std::fs::create_dir_all(host.parent().unwrap()).unwrap();
    std::fs::write(&host, "- id: llm\n  config: {}\n").unwrap();
    let spec = start(&w);
    let copy = spec.home_src.join(".dsh/cordis.patch.yml");
    let text = std::fs::read_to_string(&copy).unwrap();
    assert_eq!(off_rows(&text), 1, "{text}");
    let n = notice(&spec);
    assert_eq!(n.len(), 1, "{:?}", spec.notices);
    assert!(
        n[0].contains("[profiles.dsh] upload_session_logs = true"),
        "{}",
        n[0]
    );
    // A second start keeps one row.
    let spec = start(&w);
    assert_eq!(off_rows(&std::fs::read_to_string(&copy).unwrap()), 1);
    // The switch keeps the upload on: no row, no line.
    std::fs::write(
        w.env.paddock_config().join("config.toml"),
        "[profiles.dsh]\nupload_session_logs = true\n",
    )
    .unwrap();
    let on = start(&w);
    assert_eq!(on.home_src, spec.home_src);
    let text = std::fs::read_to_string(&copy).unwrap();
    assert_eq!(off_rows(&text), 0, "{text}");
    assert_eq!(text.trim(), "[]");
    assert!(notice(&on).is_empty(), "{:?}", on.notices);
    // The host file never changes.
    assert_eq!(
        std::fs::read_to_string(&host).unwrap(),
        "- id: llm\n  config: {}\n"
    );
    // Other harnesses have no such upload.
    assert!(crate::profile::builtin("claude")
        .unwrap()
        .privacy
        .uploads
        .is_empty());
}
