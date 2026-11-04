//! Leak guard: the document rule on a temporary git repository.

use std::path::{Path, PathBuf};
use std::process::Command;

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn write(dir: &Path, rel: &str, body: &str) {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

/// A repository with the guard script and the public documents.
fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    git(d, &["init", "-q", "-b", "main"]);
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".github/scripts/leak-guard.sh");
    write(
        d,
        ".github/scripts/leak-guard.sh",
        &std::fs::read_to_string(script).unwrap(),
    );
    for f in [
        "README.md",
        "CHANGELOG.md",
        "CONTRIBUTING.md",
        "SECURITY.md",
        ".github/pull_request_template.md",
        "changes/a-feature.md",
        "skills/paddock/SKILL.md",
        "skills/paddock/references/kv.md",
        "docs/assets/hero.png",
        "src/main.rs",
    ] {
        write(d, f, "x\n");
    }
    git(d, &["add", "-A"]);
    dir
}

fn guard(dir: &Path) -> (bool, String) {
    let out = Command::new("bash")
        .arg(".github/scripts/leak-guard.sh")
        .current_dir(dir)
        .output()
        .expect("bash");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

#[test]
fn the_public_documents_pass() {
    let r = repo();
    let (ok, out) = guard(r.path());
    assert!(ok, "{out}");
}

#[test]
fn design_notes_and_other_documents_fail() {
    for f in [
        "docs/design/protected-windows.md",
        "docs/guide.md",
        "docs/plan.txt",
        "notes.md",
        "src/NOTES.md",
        "PRODUCT.md",
        "skills/paddock/plan.md",
        "changes/sub/x.md",
    ] {
        let r = repo();
        write(r.path(), f, "internal\n");
        git(r.path(), &["add", "-A"]);
        let (ok, out) = guard(r.path());
        assert!(!ok, "{f} passed:\n{out}");
        assert!(out.contains(f), "{f} not named:\n{out}");
    }
}
