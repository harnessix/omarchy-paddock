//! The `paddock-vm` helper refuses a bad start before it boots anything.
//! These checks run on every system with fake guest files.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_paddock-vm");

fn sha256(path: &Path) -> String {
    use sha2::Digest;
    let data = std::fs::read(path).unwrap();
    sha2::Sha256::digest(&data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A fake home with a project and fake guest files.
fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
    let t = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
    let home = t.path().canonicalize().unwrap();
    let project = home.join("app");
    let assets = home.join("vm");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&assets).unwrap();
    let mut toml = String::new();
    for name in ["kernel", "rootfs.img"] {
        std::fs::write(assets.join(name), format!("fake {name}")).unwrap();
        toml.push_str(&format!(
            "[[file]]\nname = \"{name}\"\nurl = \"https://example.invalid/{name}\"\nsha256 = \"{}\"\nsize = {}\nversion = \"1\"\n",
            sha256(&assets.join(name)),
            std::fs::metadata(assets.join(name)).unwrap().len()
        ));
    }
    std::fs::write(assets.join("assets.toml"), toml).unwrap();
    (t, home, project, assets)
}

fn start(home: &Path, project: &Path, assets: &Path, extra: &[&str]) -> Output {
    Command::new(BIN)
        .env_clear()
        .env("HOME", home)
        .env("PATH", "/usr/bin:/bin")
        .arg("start")
        .arg("--assets")
        .arg(assets)
        .arg("--project")
        .arg(project)
        .arg("--state")
        .arg(home.join("state"))
        .args(extra)
        .output()
        .unwrap()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn refuses_the_home_as_the_project() {
    let (_t, home, _project, assets) = fixture();
    let o = start(&home, &home, &assets, &[]);
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    assert!(stderr(&o).contains("contains it"), "{}", stderr(&o));
    assert!(!home.join("state").exists());
}

#[test]
fn refuses_missing_or_changed_guest_files() {
    let (_t, home, project, assets) = fixture();
    std::fs::write(assets.join("kernel"), "fake kernal").unwrap();
    let o = start(&home, &project, &assets, &[]);
    assert_eq!(o.status.code(), Some(1));
    assert!(stderr(&o).contains("does not match"), "{}", stderr(&o));
    let o = start(&home, &project, &home.join("none"), &[]);
    assert_eq!(o.status.code(), Some(1));
    assert!(stderr(&o).contains("not set up"), "{}", stderr(&o));
}

#[test]
fn refuses_an_unknown_port() {
    let (_t, home, project, assets) = fixture();
    let o = start(&home, &project, &assets, &["--forward", "8080=/tmp/x.sock"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(
        stderr(&o).contains("not a paddock VM port"),
        "{}",
        stderr(&o)
    );
}

#[cfg(not(target_os = "macos"))]
#[test]
fn says_that_it_needs_macos() {
    let (_t, home, project, assets) = fixture();
    let o = start(&home, &project, &assets, &[]);
    assert_eq!(o.status.code(), Some(1));
    assert!(stderr(&o).contains("only on macOS"), "{}", stderr(&o));
}
