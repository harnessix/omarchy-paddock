use super::*;

/// Harnesses without a flag for no approvals: they ask none (pi, ori,
/// freebuff, mcode's terminal UI), take the mode from a variable (dsh),
/// or run on the host (openclaw).
const NO_APPROVAL_FLAG: &[&str] = &["pi", "ori", "freebuff", "mcode", "dsh", "openclaw"];

#[test]
fn builtin_profiles_parse_and_have_yolo_args() {
    for (name, text) in BUILTIN {
        let p: Profile = toml::from_str(text).unwrap();
        assert_eq!(&p.name, name);
        // Desktop apps keep their approval setting in their own settings.
        // Some harnesses ask for no approvals, or take the mode from a
        // variable that omarchy-agent sets.
        if p.app.is_none() && !NO_APPROVAL_FLAG.contains(name) {
            assert!(!p.launch.yolo_args.is_empty(), "{name} has no yolo_args");
        }
    }
}

#[test]
fn builtin_privacy_entries_are_well_formed() {
    for (name, text) in BUILTIN {
        let p: Profile = toml::from_str(text).unwrap();
        let t = p.privacy.training.as_ref();
        assert!(t.is_some(), "{name} has no privacy.training note");
        assert!(t.unwrap().url.starts_with("https://"), "{name}");
        for s in &p.privacy.settings {
            assert!(
                s.format == "json" || s.format == "toml",
                "{name}: {}",
                s.file
            );
            assert!(!s.path.is_empty(), "{name}: {}", s.file);
            let rel = Path::new(&s.file);
            assert!(
                rel.is_relative() && rel.components().all(|c| matches!(c, Component::Normal(_))),
                "{name}: {} must stay inside the sandbox home",
                s.file
            );
        }
    }
}

#[test]
fn one_shot_commands_never_carry_the_prompt_in_argv() {
    for (name, text) in BUILTIN {
        let p: Profile = toml::from_str(text).unwrap();
        let Some(o) = p.oneshot else { continue };
        let file_args = o.args.iter().filter(|a| a.contains(PROMPT_FILE)).count();
        match o.prompt {
            PromptVia::Stdin => assert_eq!(file_args, 0, "{name}"),
            PromptVia::File => assert_eq!(file_args, 1, "{name} must name the prompt file"),
        }
        let secret = "PROMPT-TEXT-THAT-MUST-NOT-LEAK";
        let argv = o.render(Path::new("/home/demo/.paddock-spawn/prompt.md"));
        assert!(argv.iter().all(|a| !a.contains(secret)), "{name}");
    }
}

#[test]
fn detach_args_name_the_prompt_file_once() {
    let mut with = vec![];
    for (name, text) in BUILTIN {
        let p: Profile = toml::from_str(text).unwrap();
        let Some(d) = p.detach else { continue };
        with.push(p.name.clone());
        let n: usize = d.args.iter().map(|a| a.matches(PROMPT_FILE).count()).sum();
        assert_eq!(n, 1, "{name} must name the prompt file once");
    }
    for h in ["claude", "codex", "grok", "agy"] {
        assert!(with.iter().any(|w| w == h), "{h} has no [detach]");
    }
}

#[test]
fn install_root_rules() {
    assert_eq!(
        install_root(Path::new(
            "/x/opencode/2.2.0/node_modules/opencode/bin/cli.js"
        )),
        PathBuf::from("/x/opencode/2.2.0/node_modules")
    );
    // Another repository's package: its node_modules, never the repository.
    assert_eq!(
        install_root(Path::new(
            "/home/demo/other/node_modules/pkg/node_modules/dep/cli.js"
        )),
        PathBuf::from("/home/demo/other/node_modules")
    );
    for single in ["/home/demo/.cargo/bin/tool", "/home/demo/go/bin/tool"] {
        assert_eq!(install_root(Path::new(single)), PathBuf::from(single));
    }
    assert_eq!(
        install_root(Path::new("/usr/bin/claude")),
        PathBuf::from("/usr/bin/claude")
    );
    assert_eq!(
        install_root(Path::new("/x/claude/2.1/claude")),
        PathBuf::from("/x/claude/2.1")
    );
    assert_eq!(
        install_root(Path::new("/x/node/26/bin/node")),
        PathBuf::from("/x/node/26")
    );
}
