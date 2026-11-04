//! File size limits: the check script on a temporary git repository.

use std::path::{Path, PathBuf};
use std::process::Command;

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "File Size Test")
        .env("GIT_AUTHOR_EMAIL", "noreply@example.com")
        .env("GIT_COMMITTER_NAME", "File Size Test")
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

fn commit(dir: &Path, msg: &str) {
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-m", msg]);
}

fn init_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-b", "main"]);
    git(dir.path(), &["config", "user.name", "File Size Test"]);
    git(dir.path(), &["config", "user.email", "noreply@example.com"]);
    dir
}

fn run_script(dir: &Path) -> (i32, String) {
    let bin = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".github/scripts/file-size.sh");
    let out = Command::new("bash")
        .arg(&bin)
        .current_dir(dir)
        .output()
        .expect("script");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.code().unwrap_or(1), text)
}

fn n_lines(n: usize) -> String {
    "x\n".repeat(n)
}

#[test]
fn file_size_rules_on_a_temporary_repository() {
    let dir = init_repo();
    let root = dir.path();

    write(root, "src/ok.rs", &n_lines(800));
    write(root, "plugin/ok.qml", &n_lines(10));
    commit(root, "base");
    let (code, text) = run_script(root);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("file-size: ok"), "{text}");

    write(root, "src/too_long.rs", &n_lines(801));
    commit(root, "unlisted over 800");
    let (code, text) = run_script(root);
    assert_ne!(code, 0, "{text}");
    assert!(
        text.contains("src/too_long.rs has 801 lines; the limit is 800"),
        "{text}"
    );
    assert!(
        text.contains("split the file, or change the baseline in a separate PR with a reason"),
        "{text}"
    );
    assert!(!text.contains("file-size: ok"), "{text}");

    git(root, &["rm", "-f", "src/too_long.rs"]);
    write(root, ".github/file-size-baseline", "1000 src/big.rs\n");
    write(root, "src/big.rs", &n_lines(1000));
    commit(root, "listed at limit");
    let (code, text) = run_script(root);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("file-size: ok"), "{text}");

    write(root, "src/big.rs", &n_lines(1001));
    commit(root, "listed grew");
    let (code, text) = run_script(root);
    assert_ne!(code, 0, "{text}");
    assert!(
        text.contains("src/big.rs has 1001 lines; the limit is 1000"),
        "{text}"
    );
    assert!(
        text.contains("split the file, or change the baseline in a separate PR with a reason"),
        "{text}"
    );

    write(root, "src/big.rs", &n_lines(900));
    commit(root, "listed shrank");
    let (code, text) = run_script(root);
    assert_eq!(code, 0, "{text}");
    assert!(
        text.contains(
            "src/big.rs shrank to 900 lines (baseline 1000). Put 900 in .github/file-size-baseline"
        ),
        "{text}"
    );
    assert!(text.contains("file-size: ok"), "{text}");

    write(root, "plugin/wide.qml", &n_lines(801));
    commit(root, "qml over 800");
    let (code, text) = run_script(root);
    assert_ne!(code, 0, "{text}");
    assert!(
        text.contains("plugin/wide.qml has 801 lines; the limit is 800"),
        "{text}"
    );

    git(root, &["rm", "-f", "plugin/wide.qml"]);
    commit(root, "drop qml");
    write(root, "src/untracked.rs", &n_lines(900));
    let (code, text) = run_script(root);
    assert_eq!(code, 0, "{text}");
    assert!(!text.contains("untracked.rs"), "{text}");
}
