//! Every test that runs the built paddock gives it temporary folders. A
//! build that runs with the user's own folders can rewrite the user's
//! launchers, shell commands and state with the path of the build, and
//! they break when the build is gone.
//!
//! A test file passes when it sets both XDG_DATA_HOME and XDG_STATE_HOME,
//! or clears the environment and sets HOME.

#[test]
fn tests_that_run_paddock_never_use_the_users_folders() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests");
    let mut bad = Vec::new();
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.extension().is_none_or(|x| x != "rs") {
            continue;
        }
        let text = std::fs::read_to_string(&p).unwrap();
        if !text.contains("CARGO_BIN_EXE_paddock") {
            continue;
        }
        let xdg = text.contains("XDG_DATA_HOME") && text.contains("XDG_STATE_HOME");
        let cleared = text.contains(".env_clear()") && text.contains("\"HOME\"");
        if !xdg && !cleared {
            bad.push(p.file_name().unwrap().to_string_lossy().into_owned());
        }
    }
    assert!(
        bad.is_empty(),
        "these tests run paddock with the user's own folders: {bad:?}"
    );
}
