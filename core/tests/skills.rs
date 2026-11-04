//! The two agent skills: every reference file that a skill links exists,
//! every `paddock-host` subcommand that a skill names exists, and
//! `paddock skill install` copies the whole paddock-host folder.

use std::path::{Path, PathBuf};
use std::process::Command;

const SKILLS: [&str; 2] = ["paddock", "paddock-host"];

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("skills")
}

/// The Markdown files of one skill: SKILL.md and its reference files.
fn files(skill: &str) -> Vec<PathBuf> {
    let dir = root().join(skill);
    let mut out = vec![dir.join("SKILL.md")];
    if let Ok(rd) = std::fs::read_dir(dir.join("references")) {
        out.extend(rd.map(|e| e.unwrap().path()));
    }
    out.sort();
    out
}

/// The targets of the Markdown links `](target)` in a text.
fn links(text: &str) -> Vec<String> {
    text.split("](")
        .skip(1)
        .filter_map(|rest| rest.split(')').next())
        .map(str::to_string)
        .collect()
}

#[test]
fn every_linked_reference_file_exists_and_is_linked() {
    for skill in SKILLS {
        let dir = root().join(skill);
        let main = std::fs::read_to_string(dir.join("SKILL.md")).unwrap();
        assert!(main.starts_with("---\nname: "), "{skill}: no front matter");
        let lines = main.lines().count();
        assert!(lines < 120, "{skill}/SKILL.md has {lines} lines");
        for file in files(skill) {
            let text = std::fs::read_to_string(&file).unwrap();
            for target in links(&text) {
                if target.starts_with("http") {
                    continue;
                }
                let path = file.parent().unwrap().join(&target);
                assert!(path.is_file(), "{}: no file {target}", file.display());
            }
        }
        for file in files(skill).iter().filter(|f| !f.ends_with("SKILL.md")) {
            let rel = file.strip_prefix(&dir).unwrap().display().to_string();
            assert!(
                main.contains(&format!("]({rel})")),
                "{skill}/SKILL.md does not link {rel}"
            );
        }
    }
}

/// The help text of `paddock-host`, which the binary prints for `paddock host`.
fn host_help(args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_paddock"))
        .arg("host")
        .args(args)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn every_named_paddock_host_command_exists() {
    let help = host_help(&["--help"]);
    let commands: Vec<&str> = help
        .lines()
        .filter_map(|l| l.trim().strip_prefix("paddock-host "))
        .filter_map(|rest| rest.split([' ', '|']).next())
        .collect();
    assert!(commands.contains(&"spawn"), "unexpected help: {help}");
    let subhelp = |cmd: &str| host_help(&[cmd, "--help"]);
    for skill in SKILLS {
        for file in files(skill) {
            let text = std::fs::read_to_string(&file).unwrap();
            for rest in text.split("paddock-host ").skip(1) {
                let mut words = rest.split([' ', '`', '"', ')', ',', '\n', '.', ':']);
                let cmd = words.next().unwrap_or("");
                if cmd.is_empty() || !cmd.chars().all(|c| c.is_ascii_lowercase() || c == '-') {
                    continue;
                }
                if cmd.starts_with("--") {
                    continue;
                }
                assert!(
                    commands.contains(&cmd),
                    "{}: paddock-host {cmd} is not in `paddock-host --help`",
                    file.display()
                );
                // A subcommand of kv, timer, pasture or device must be in
                // that command's own help.
                if ["kv", "timer", "pasture"].contains(&cmd) {
                    let sub = words.next().unwrap_or("");
                    if !sub.is_empty() && sub.chars().all(|c| c.is_ascii_lowercase()) {
                        assert!(
                            subhelp(cmd).contains(&format!("paddock-host {cmd} {sub}")),
                            "{}: paddock-host {cmd} {sub} is not in its help",
                            file.display()
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn skill_install_copies_the_whole_folder() {
    let t = tempfile::tempdir().unwrap();
    let home = t.path().join("home");
    let xdg = t.path().join("xdg");
    std::fs::create_dir_all(&home).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_paddock"))
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", xdg.join("cfg"))
        .env("XDG_DATA_HOME", xdg.join("data"))
        .env("XDG_STATE_HOME", xdg.join("state"))
        .env("XDG_CACHE_HOME", xdg.join("cache"))
        .env_remove("PADDOCK")
        .args(["skill", "install"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let src = root().join("paddock-host");
    for base in [".claude/skills", ".agents/skills"] {
        let dir = home.join(base).join("paddock-host");
        for file in files("paddock-host") {
            let rel = file.strip_prefix(&src).unwrap();
            assert_eq!(
                std::fs::read_to_string(dir.join(rel)).unwrap(),
                std::fs::read_to_string(&file).unwrap(),
                "{} differs",
                rel.display()
            );
        }
    }
}
