//! Probe the workspace options of a flatpak app in a real flatpak sandbox.
//! A Freedesktop runtime stands in for the app. Skipped where flatpak or the
//! runtime is not installed.

use paddock::app::{flatpak_args, flatpak_workspace};
use paddock::env::HostEnv;
use paddock::manifest::Manifest;
use paddock::spec::SandboxSpec;
use std::path::Path;
use std::process::{Command, Stdio};

fn runtime() -> Option<String> {
    let out = Command::new("flatpak")
        .args(["list", "--runtime", "--columns=ref"])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find(|l| l.starts_with("org.freedesktop.Platform/"))
        .map(|l| l.trim().to_string())
}

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@localhost"])
        .args(args)
        .stdout(Stdio::null())
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?}");
}

const PROBE: &str = r#"
P=$1
fail() { echo "FAIL $1"; }
touch "$P/file.txt" && echo "ok workspace-writable" || fail workspace-writable
for f in config config.worktree commondir objects/info/alternates; do
  if (echo evil >> "$P/.git/$f") 2>/dev/null; then fail "git-$f"; else echo "ok git-$f"; fi
done
if touch "$P/.git/hooks/pre-commit" 2>/dev/null; then fail git-hooks; else echo "ok git-hooks"; fi
if mv "$P/.git" "$P/.git-moved" 2>/dev/null; then fail git-rename; else echo "ok git-rename"; fi
if cat "$P/private/key.txt" 2>/dev/null; then fail hidden-folder; else echo "ok hidden-folder"; fi
if [ -e "$HOME/.ssh" ] || [ -e "$HOME/.gitconfig" ]; then fail home; else echo "ok home"; fi
"#;

#[test]
fn flatpak_workspace_escape_probe() {
    let Some(rt) = runtime() else {
        eprintln!("skipped: flatpak or org.freedesktop.Platform is not installed");
        return;
    };
    let t = tempfile::tempdir().unwrap();
    let project = t.path().join("ws");
    std::fs::create_dir_all(project.join("private")).unwrap();
    git(&project, &["init", "-q", "-b", "main"]);
    git(&project, &["commit", "-q", "--allow-empty", "-m", "one"]);
    std::fs::write(project.join("private/key.txt"), "paddock-secret-canary").unwrap();
    let env = HostEnv::from_parts(
        t.path().join("home"),
        "demo".into(),
        1000,
        1000,
        Default::default(),
    );
    std::fs::create_dir_all(&env.home).unwrap();
    let mut spec = SandboxSpec::new(
        "t".into(),
        "f",
        &project,
        env.home.join("sandbox"),
        &env.home,
        1000,
        1000,
        "demo",
    );
    let mut m = Manifest::default();
    m.secrets.hide.push("private".into());
    let ws = flatpak_workspace(&env, &mut spec, &m, std::slice::from_ref(&project)).unwrap();
    assert!(ws.exposed.is_empty());
    let mut args = flatpak_args(&rt, Some("sh"), &ws.args, false);
    // A runtime has no app bus to filter; the folders are under test here.
    args.retain(|a| a != "--session-bus" && !a.starts_with("--talk-name="));
    args.extend(["-c".into(), PROBE.into(), "probe".into()]);
    args.push(project.display().to_string());
    let out = Command::new("flatpak").args(&args).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    if !text.contains("ok workspace-writable") {
        eprintln!(
            "skipped: flatpak cannot run here: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        return;
    }
    assert!(!text.contains("FAIL"), "{text}");
    for check in [
        "git-config",
        "git-config.worktree",
        "git-commondir",
        "git-objects/info/alternates",
        "git-hooks",
        "git-rename",
        "hidden-folder",
        "home",
    ] {
        assert!(text.contains(&format!("ok {check}")), "{check}: {text}");
    }
    assert!(project.join(".git").is_dir());
}
