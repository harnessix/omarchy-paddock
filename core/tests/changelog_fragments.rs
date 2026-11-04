//! Changelog fragments: the check and the assemble script on a temporary git repository.

use std::path::{Path, PathBuf};
use std::process::Command;

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Changelog Test")
        .env("GIT_AUTHOR_EMAIL", "noreply@example.com")
        .env("GIT_COMMITTER_NAME", "Changelog Test")
        .env("GIT_COMMITTER_EMAIL", "noreply@example.com")
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
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, body).unwrap();
}

fn rev(dir: &Path) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("rev-parse");
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn commit(dir: &Path, msg: &str) {
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-m", msg]);
}

fn init_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-b", "main"]);
    git(dir.path(), &["config", "user.name", "Changelog Test"]);
    git(dir.path(), &["config", "user.email", "noreply@example.com"]);
    dir
}

fn run_script(dir: &Path, script: &str, args: &[&str], env: &[(&str, &str)]) -> (i32, String) {
    let bin = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(script);
    let mut cmd = Command::new("bash");
    cmd.arg(&bin).args(args).current_dir(dir);
    cmd.env_remove("CHANGELOG_LABELS");
    cmd.env_remove("CHANGELOG_HEAD_REF");
    for (key, value) in env {
        cmd.env(key, value);
    }
    let out = cmd.output().expect("script");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.code().unwrap_or(1), text)
}

const SAMPLE: &str = "\
# Changelog

## Unreleased

- Fix: already here

## 0.1.0

- New: old release
";

fn check(dir: &Path, base: &str, head: &str, env: &[(&str, &str)]) -> (i32, String) {
    run_script(
        dir,
        ".github/scripts/changelog-fragment.sh",
        &[base, head],
        env,
    )
}

#[test]
fn fragment_rules_on_a_temporary_repository() {
    let dir = init_repo();
    let root = dir.path();
    write(root, "CHANGELOG.md", SAMPLE);
    write(root, "src/lib.rs", "fn base() {}\n");
    commit(root, "base");
    let base = rev(root);

    write(root, "src/lib.rs", "fn changed() {}\n");
    commit(root, "touch src");
    let head_src = rev(root);

    let (code, text) = check(root, &base, &head_src, &[]);
    assert_ne!(code, 0, "{text}");
    assert!(text.contains("changelog: label exemption: no"), "{text}");
    assert!(text.contains("changelog: release exemption: no"), "{text}");
    assert!(
        text.contains("no fragment for a change under src/, profiles/ or skills/"),
        "{text}"
    );
    assert!(!text.contains("changelog: ok"), "{text}");

    let (code, text) = check(
        root,
        &base,
        &head_src,
        &[("CHANGELOG_LABELS", "no-changelog")],
    );
    assert_eq!(code, 0, "{text}");
    assert!(
        text.contains("changelog: label exemption: yes (no-changelog)"),
        "{text}"
    );
    assert!(
        text.contains("fragment not required because label no-changelog is set"),
        "{text}"
    );
    assert!(text.contains("changelog: release exemption: no"), "{text}");
    assert!(text.contains("changelog: ok"), "{text}");

    write(root, "changes/good.md", "- New: a feature\n");
    commit(root, "add fragment");
    let head_good = rev(root);
    let (code, text) = check(root, &base, &head_good, &[]);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("changelog: ok"), "{text}");

    write(root, "changes/empty.md", "\n");
    commit(root, "empty fragment");
    let head_empty = rev(root);
    let (code, text) = check(root, &base, &head_empty, &[]);
    assert_ne!(code, 0, "{text}");
    assert!(text.contains("changes/empty.md is empty"), "{text}");

    std::fs::remove_file(root.join("changes/empty.md")).unwrap();
    write(root, "changes/bad.md", "not a list\n");
    commit(root, "bad fragment");
    let head_bad = rev(root);
    let (code, text) = check(root, &base, &head_bad, &[]);
    assert_ne!(code, 0, "{text}");
    assert!(
        text.contains("changes/bad.md does not start with '- '"),
        "{text}"
    );

    std::fs::remove_file(root.join("changes/bad.md")).unwrap();
    commit(root, "drop bad fragment");

    let old_only = SAMPLE.replace("- New: old release", "- New: older release");
    write(root, "CHANGELOG.md", &old_only);
    commit(root, "edit a released section");
    let head_old = rev(root);
    let (code, text) = check(root, &base, &head_old, &[]);
    assert_eq!(code, 0, "{text}");
    assert!(!text.contains("edits ## Unreleased"), "{text}");

    let unreleased = SAMPLE.replace(
        "- Fix: already here",
        "- Fix: already here\n- Changed: sneaked into unreleased",
    );
    write(root, "CHANGELOG.md", &unreleased);
    commit(root, "edit unreleased");
    let head_unreleased = rev(root);

    let (code, text) = check(root, &base, &head_unreleased, &[]);
    assert_ne!(code, 0, "{text}");
    assert!(
        text.contains("the diff edits ## Unreleased in CHANGELOG.md"),
        "{text}"
    );
    assert!(text.contains("changelog: release exemption: no"), "{text}");

    let (code, text) = check(
        root,
        &base,
        &head_unreleased,
        &[("CHANGELOG_LABELS", "no-changelog")],
    );
    assert_ne!(code, 0, "{text}");
    assert!(
        text.contains("the diff edits ## Unreleased in CHANGELOG.md"),
        "{text}"
    );

    let (code, text) = check(
        root,
        &base,
        &head_unreleased,
        &[("CHANGELOG_HEAD_REF", "release/0.1.8")],
    );
    assert_eq!(code, 0, "{text}");
    assert!(
        text.contains("changelog: release exemption: yes (head ref release/0.1.8)"),
        "{text}"
    );
    assert!(
        text.contains("Unreleased edit allowed (head ref release/0.1.8)"),
        "{text}"
    );

    let (code, text) = check(
        root,
        &base,
        &head_unreleased,
        &[("CHANGELOG_LABELS", "release")],
    );
    assert_eq!(code, 0, "{text}");
    assert!(
        text.contains("changelog: release exemption: yes (label release)"),
        "{text}"
    );
    assert!(
        text.contains("Unreleased edit allowed (label release)"),
        "{text}"
    );
}

#[test]
fn assemble_sorts_prefixes_and_deletes_fragments() {
    let dir = init_repo();
    let root = dir.path();
    write(root, "CHANGELOG.md", SAMPLE);
    write(root, "changes/README.md", "Rules stay here.\n");
    write(
        root,
        "changes/beta.md",
        "- Fix: beta fix\n- Security: beta hole\n",
    );
    write(
        root,
        "changes/alpha.md",
        "- Removed: alpha gone\n- New: alpha feature\n- a plain note\n- Changed: alpha change\n",
    );
    commit(root, "sample fragments");

    let (code, text) = run_script(root, ".github/scripts/changelog-assemble.sh", &[], &[]);
    assert_eq!(code, 0, "{text}");
    assert!(
        text.contains("changelog: removed changes/alpha.md"),
        "{text}"
    );
    assert!(
        text.contains("changelog: removed changes/beta.md"),
        "{text}"
    );
    assert!(!root.join("changes/alpha.md").exists());
    assert!(!root.join("changes/beta.md").exists());
    assert!(root.join("changes/README.md").exists());
    assert_eq!(
        std::fs::read_to_string(root.join("changes/README.md")).unwrap(),
        "Rules stay here.\n"
    );

    let changelog = std::fs::read_to_string(root.join("CHANGELOG.md")).unwrap();
    assert_eq!(
        changelog,
        "\
# Changelog

## Unreleased

- Security: beta hole
- New: alpha feature
- Changed: alpha change
- Fix: beta fix
- Removed: alpha gone
- a plain note

- Fix: already here

## 0.1.0

- New: old release
"
    );
}
