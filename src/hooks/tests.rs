use super::command::{parse, Parsed};
use super::*;
use std::collections::BTreeMap;
use std::os::unix::fs::{MetadataExt, PermissionsExt};

#[test]
fn the_program_of_a_command_line() {
    let p = |prog: &str, script: Option<&str>| Parsed::Program {
        program: prog.into(),
        script: script.map(str::to_string),
    };
    assert_eq!(parse("my-hook stop"), p("my-hook", None));
    assert_eq!(parse("A=1 B='x y' env -i C=2 my-hook"), p("my-hook", None));
    assert_eq!(
        parse("exec ~/.local/bin/my-hook a"),
        p("~/.local/bin/my-hook", None)
    );
    assert_eq!(parse("'/opt/my hook' --x || true"), p("/opt/my hook", None));
    assert_eq!(
        parse("bash ~/hooks/x.sh arg"),
        p("bash", Some("~/hooks/x.sh"))
    );
    assert_eq!(parse("python3 -I script.py"), p("python3", None));
    assert_eq!(
        parse("python3 ./h/script.py"),
        p("python3", Some("./h/script.py"))
    );
    assert_eq!(parse("sh -c 'anything here'"), Parsed::ShellC);
    assert_eq!(parse("bash -lc x"), Parsed::ShellC);
    assert_eq!(parse("$(which x) y"), Parsed::ShellC);
    assert_eq!(parse("(cd /x && y)"), Parsed::ShellC);
    assert_eq!(parse("true"), Parsed::Builtin);
    assert_eq!(parse("echo hi > /tmp/x"), Parsed::Builtin);
    assert_eq!(parse("A=1"), Parsed::Builtin);
    assert_eq!(parse("\"$HOME\"/bin/x"), p("$HOME/bin/x", None));
    assert_eq!(parse("if my-hook; then"), p("my-hook", None));
    assert_eq!(parse("fi"), Parsed::Builtin);
    assert_eq!(
        command::split("[ ! -f '/x/a;b' ] || '/x/a;b' run 2>&1 | tee -a log; echo \"a|b\" $(c; d)"),
        [
            "[ ! -f '/x/a;b' ] ",
            " '/x/a;b' run 2>&1 ",
            " tee -a log",
            " echo \"a|b\" $(c; d)"
        ]
    );
    assert_eq!(
        command::first_program("[ -n \"$TOOL_HOME\" ] && \"$TOOL_HOME/bin/tool\" stop"),
        "$TOOL_HOME/bin/tool"
    );
}

#[test]
fn an_ignore_rule_matches_by_name_or_path() {
    let home = Path::new("/home/demo");
    assert!(command::matches(home, "my-hook", "my-hook"));
    assert!(command::matches(home, "my-hook", "/usr/local/bin/my-hook"));
    assert!(!command::matches(home, "my-hook", "my-hook2"));
    assert!(command::matches(
        home,
        "~/.local/bin/my-hook",
        "/home/demo/.local/bin/my-hook"
    ));
    assert!(command::matches(
        home,
        "~/.local/bin/my-hook",
        "$HOME/.local/bin/my-hook"
    ));
    assert!(command::matches(
        home,
        "~/.local/bin/my-hook",
        "~/.local/bin/my-hook"
    ));
    assert!(!command::matches(
        home,
        "~/.local/bin/my-hook",
        "/usr/bin/my-hook"
    ));
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

fn put(w: &World, rel: &str, text: &str, mode: u32) -> PathBuf {
    let p = w.env.home.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, text).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
    p
}

fn tree(dir: &Path) -> BTreeMap<PathBuf, (Vec<u8>, u32)> {
    let mut out = BTreeMap::new();
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            let m = std::fs::symlink_metadata(&p).unwrap();
            if m.is_dir() {
                todo.push(p);
            } else {
                out.insert(p.clone(), (std::fs::read(&p).unwrap_or_default(), m.mode()));
            }
        }
    }
    out
}

/// Build a sandbox of a built-in profile and write its home as a start
/// does: the seed copies, the settings, then the hooks.
fn start(w: &World, name: &str) -> anyhow::Result<crate::spec::SandboxSpec> {
    let h = crate::profile::Harness {
        profile: crate::profile::builtin(name).unwrap(),
        exe: "/usr/bin/true".into(),
        install_mounts: vec![],
        path_dirs: vec![],
        source: "test".into(),
    };
    let opts = crate::build::Options {
        harness: name.into(),
        project: Some(w.project.clone()),
        assume_yes: true,
        ..Default::default()
    };
    let b = crate::build::build_with(&w.env, &opts, h)?;
    for rel in &crate::profile::builtin(name).unwrap().state.seed {
        if let Ok(bytes) = std::fs::read(w.env.home.join(rel)) {
            let dst = b.spec.home_src.join(rel);
            std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
            std::fs::write(dst, bytes).unwrap();
        }
    }
    crate::privacy::write_settings(&b.spec.home_src, &b.spec.home_settings)?;
    assert!(write(&b.spec)?.is_empty());
    Ok(b.spec)
}

const HOOK: &str = "#!/bin/sh\nmkdir -p \"$HOME/.local/state/my-hook\"\necho \"$1\" >> \"$HOME/.local/state/my-hook/log\"\n";

/// The hook file of a harness, and its user entries: a missing program, a
/// builtin, a shell line, a listed program and an ignored program.
fn user_file(name: &str) -> (&'static str, String) {
    let cmds = [
        "missing-hook stop",
        "true",
        "sh -c 'echo x'",
        "~/.local/bin/listed-hook stop",
        "ign-hook stop",
    ];
    let handler = |c: &str| serde_json::json!({"type": "command", "command": c});
    if name == "agy" {
        let list: Vec<Value> = cmds.iter().map(|c| handler(c)).collect();
        let doc = serde_json::json!({"mine": {"Stop": list}});
        return (".gemini/config/hooks.json", doc.to_string());
    }
    let groups: Vec<Value> = cmds
        .iter()
        .map(|c| serde_json::json!({"hooks": [handler(c)]}))
        .collect();
    let doc = serde_json::json!({"model": "m", "hooks": {"Stop": groups}});
    let rel = match name {
        "claude" => ".claude/settings.json",
        "qwen" => ".qwen/settings.json",
        "gemini" => ".gemini/settings.json",
        "codex" => ".codex/hooks.json",
        _ => ".grok/hooks/mine.json",
    };
    (rel, doc.to_string())
}

/// The commands of the sandbox copy, in order: (event or name, command).
fn sandbox_commands(spec: &crate::spec::SandboxSpec) -> Vec<(String, String)> {
    let t = &spec.hooks[0];
    let doc: Value =
        serde_json::from_slice(&std::fs::read(spec.home_src.join(&t.into)).unwrap()).unwrap();
    let section = crate::state::json_get(&doc, &t.path).unwrap();
    let mut out = Vec::new();
    for (k, v) in section.as_object().unwrap() {
        match t.style {
            Style::Groups => {
                for g in v.as_array().unwrap() {
                    for h in g["hooks"].as_array().unwrap() {
                        out.push((k.clone(), h["command"].as_str().unwrap().into()));
                    }
                }
            }
            Style::Named => {
                for list in v.as_object().unwrap().values() {
                    for h in list.as_array().unwrap() {
                        out.push((k.clone(), h["command"].as_str().unwrap().into()));
                    }
                }
            }
        }
    }
    out
}

const HARNESSES: &[&str] = &["claude", "codex", "grok", "agy", "qwen", "gemini"];

#[test]
fn each_harness_keeps_resolvable_user_hooks_and_leaves_out_the_others() {
    for name in HARNESSES {
        let w =
            world("[hooks]\nprograms = [\"~/.local/bin/listed-hook\"]\nignore = [\"ign-hook\"]\n");
        let prog = put(&w, ".local/bin/listed-hook", HOOK, 0o755);
        let (rel, text) = user_file(name);
        put(&w, rel, &text, 0o644);
        let before = tree(&w.env.home);
        let spec = start(&w, name).unwrap();
        let cmds: Vec<String> = sandbox_commands(&spec).into_iter().map(|c| c.1).collect();
        let user: Vec<&str> = cmds
            .iter()
            .filter(|c| !c.starts_with("paddock-host"))
            .map(String::as_str)
            .collect();
        // Codex and Grok Build trust only a listed program.
        let trusts = matches!(*name, "codex" | "grok");
        let want: &[&str] = if trusts {
            &["~/.local/bin/listed-hook stop"]
        } else {
            &["true", "sh -c 'echo x'", "~/.local/bin/listed-hook stop"]
        };
        assert_eq!(user, want, "{name}: {cmds:?}");
        // paddock's own entry stays, and comes first in its event.
        if *name != "gemini" {
            assert!(cmds.iter().any(|c| c.starts_with("paddock-host")), "{name}");
        }
        if *name == "agy" {
            let keys: Vec<String> = sandbox_commands(&spec).into_iter().map(|c| c.0).collect();
            assert_eq!(keys[0], "paddock-compaction", "{keys:?}");
            assert!(keys[1..].iter().all(|k| k == "user-mine"), "{keys:?}");
        }
        // One line about the missing program, none about the ignored one.
        let notes: Vec<&String> = spec
            .notices
            .iter()
            .filter(|n| n.contains("hooks left out"))
            .collect();
        assert_eq!(notes.len(), 1, "{name}: {:?}", spec.notices);
        assert!(notes[0].contains("missing-hook") && notes[0].contains("[hooks] programs"));
        assert!(!notes[0].contains("ign-hook"));
        assert_eq!(
            notes[0].contains(&format!(
                "paddock does not trust them for {name} in the sandbox: sh -c 'echo x', true."
            )),
            trusts,
            "{name}: {}",
            notes[0]
        );
        // The listed program is bound read-only: the file, never its folder.
        let tool = spec
            .host_tools
            .iter()
            .find(|t| t.name == "listed-hook")
            .unwrap();
        assert_eq!(tool.src, std::fs::canonicalize(&prog).unwrap());
        let m = spec.mounts.iter().find(|m| m.dst == prog).unwrap();
        assert_eq!(m.kind, crate::spec::MountKind::Ro);
        assert!(!spec.mounts.iter().any(|m| m.dst == prog.parent().unwrap()));
        // The explanation names each origin.
        let ex = explain(&spec).join("\n");
        assert!(ex.contains("ignored by [hooks] ignore"), "{ex}");
        assert!(ex.contains("missing-hook is not in the sandbox"), "{ex}");
        assert!(ex.contains("[hooks] programs listed-hook"), "{ex}");
        if *name != "gemini" {
            assert!(ex.contains("paddock (profile"), "{ex}");
        }
        // The host files stay byte for byte the same.
        assert_eq!(tree(&w.env.home), before, "{name}");
    }
}

#[test]
fn an_ignored_entry_is_gone_silently_and_paddock_entries_stay() {
    for name in HARNESSES {
        let w = world("[hooks]\nignore = [\"~/.local/bin/my-hook\", \"other-hook\"]\nprograms = [\"~/.local/bin/my-hook\"]\n");
        put(&w, ".local/bin/my-hook", HOOK, 0o755);
        let (rel, _) = user_file(name);
        let doc = if *name == "agy" {
            r#"{"a":{"Stop":[{"type":"command","command":"~/.local/bin/my-hook stop"},{"type":"command","command":"other-hook"}]}}"#
        } else {
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"$HOME/.local/bin/my-hook stop"}]},{"hooks":[{"type":"command","command":"/usr/bin/other-hook"}]}]}}"#
        };
        put(&w, rel, doc, 0o644);
        let before = tree(&w.env.home);
        let spec = start(&w, name).unwrap();
        let cmds = sandbox_commands(&spec);
        assert!(
            cmds.iter().all(|c| c.1.starts_with("paddock-host")),
            "{name}: {cmds:?}"
        );
        if *name != "gemini" {
            assert!(!cmds.is_empty(), "{name}");
        }
        assert!(
            !spec.notices.iter().any(|n| n.contains("hooks left out")),
            "{name}"
        );
        // ignore wins over programs: the program is not bound either.
        assert!(!spec.host_tools.iter().any(|t| t.name == "my-hook"));
        assert_eq!(
            explain(&spec)
                .join("\n")
                .matches("ignored by [hooks] ignore")
                .count(),
            2
        );
        assert_eq!(tree(&w.env.home), before, "{name}");
    }
}

#[test]
fn a_listed_program_runs_and_keeps_its_state_in_the_sandbox_home() {
    let w = world("[hooks]\nprograms = [\"~/.local/bin/listed-hook\"]\n");
    put(&w, ".local/bin/listed-hook", HOOK, 0o755);
    let spec = start(&w, "claude").unwrap();
    let tool = spec
        .host_tools
        .iter()
        .find(|t| t.name == "listed-hook")
        .unwrap();
    let out = std::process::Command::new(&tool.src)
        .arg("ran")
        .env("HOME", &spec.home_src)
        .output()
        .unwrap();
    assert!(out.status.success());
    let log = spec.home_src.join(".local/state/my-hook/log");
    assert_eq!(std::fs::read_to_string(log).unwrap(), "ran\n");
    assert!(!w.env.home.join(".local/state").exists());
}

#[test]
fn a_symlink_a_writable_file_or_a_project_file_is_refused() {
    let w = world("");
    let real = put(&w, "bin/real-hook", HOOK, 0o755);
    std::os::unix::fs::symlink(&real, w.env.home.join("bin/link-hook")).unwrap();
    put(&w, "bin/open-hook", HOOK, 0o757);
    put(&w, "bin/group-hook", HOOK, 0o775);
    put(&w, "bin/plain-file", HOOK, 0o644);
    let inside = w.project.join("proj-hook");
    std::fs::write(&inside, HOOK).unwrap();
    std::fs::set_permissions(&inside, std::fs::Permissions::from_mode(0o755)).unwrap();
    let check = |raw: &str| check_program(&w.env, &w.project, raw);
    assert!(check("~/bin/real-hook").is_ok());
    let e = |raw: &str| format!("{:#}", check(raw).unwrap_err());
    assert!(e("~/bin/link-hook").contains("symlink"));
    assert!(e("~/bin/open-hook").contains("writable"));
    assert!(e("~/bin/group-hook").contains("writable"));
    assert!(e("~/bin/plain-file").contains("not executable"));
    assert!(e(&inside.display().to_string()).contains("inside the project"));
    assert!(e("bin/real-hook").contains("absolute"));
    // A refused program is a warning, and the sandbox does not get it.
    let w2 = world("[hooks]\nprograms = [\"~/bin/open-hook\"]\n");
    put(&w2, "bin/open-hook", HOOK, 0o757);
    let spec = start(&w2, "claude").unwrap();
    assert!(!spec.host_tools.iter().any(|t| t.name == "open-hook"));
    assert!(spec
        .warnings
        .iter()
        .any(|x| x.contains("open-hook is writable")));
}

#[test]
fn a_project_file_cannot_set_hooks() {
    let w = world("");
    std::fs::write(
        w.project.join("paddock.toml"),
        "[hooks]\nprograms = [\"/usr/bin/env\"]\n",
    )
    .unwrap();
    let e = start(&w, "claude").unwrap_err();
    assert!(format!("{e:#}").contains("paddock.toml"), "{e:#}");
}

#[test]
fn codex_trusts_a_listed_program_entry_and_no_other_user_entry() {
    let w = world("[hooks]\nprograms = [\"~/.local/bin/listed-hook\"]\n");
    put(&w, ".local/bin/listed-hook", HOOK, 0o755);
    let src = format!("{}/.codex/hooks.json", w.env.home.display());
    put(
        &w,
        ".codex/hooks.json",
        r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"true"},{"type":"command","command":"listed-hook stop","timeout":5}]}],"SessionStart":[{"matcher":"compact","hooks":[{"type":"command","command":"~/.local/bin/listed-hook compact"}]}]}}"#,
        0o644,
    );
    // A trust in the host copy does not reach the sandbox.
    put(
        &w,
        ".codex/config.toml",
        &format!("model = \"m\"\n[hooks.state.\"{src}:stop:0:0\"]\ntrusted_hash = \"sha256:aa\"\n"),
        0o600,
    );
    let before = tree(&w.env.home);
    let spec = start(&w, "codex").unwrap();
    let home = &spec.home_src;
    let hooks: Value =
        serde_json::from_slice(&std::fs::read(home.join(".codex/hooks.json")).unwrap()).unwrap();
    let cfg: toml::Value =
        toml::from_str(&std::fs::read_to_string(home.join(".codex/config.toml")).unwrap()).unwrap();
    let state = cfg["hooks"]["state"].as_table().unwrap();
    let mut keys: Vec<&String> = state.keys().collect();
    keys.sort();
    let want = |k: &str| format!("{src}:{k}");
    assert_eq!(
        keys,
        [
            &want("session_start:0:0"),
            &want("session_start:1:0"),
            &want("stop:0:0")
        ],
        "{state:?}"
    );
    let ss = &hooks["hooks"]["SessionStart"];
    assert_eq!(
        ss[0]["hooks"][0]["command"],
        "paddock-host compacted --harness codex"
    );
    let hash = |ev: &str, g: &Value, h: usize| {
        crate::compaction::codex::hook_hash(ev, g["matcher"].as_str(), &g["hooks"][h]).unwrap()
    };
    assert_eq!(
        state[&want("session_start:1:0")]["trusted_hash"]
            .as_str()
            .unwrap(),
        hash("SessionStart", &ss[1], 0)
    );
    // `true` is not trusted, so it is not in the copy.
    let stop = &hooks["hooks"]["Stop"][0];
    assert_eq!(stop["hooks"].as_array().unwrap().len(), 1, "{stop}");
    assert_eq!(
        state[&want("stop:0:0")]["trusted_hash"].as_str().unwrap(),
        hash("Stop", stop, 0)
    );
    assert_eq!(cfg["model"].as_str(), Some("m"));
    assert_eq!(tree(&w.env.home), before);
}

#[test]
fn grok_gets_the_user_files_in_one_copy_and_a_stale_copy_loses_its_hooks() {
    let w = world("[hooks]\nprograms = [\"~/.local/bin/listed-hook\"]\n");
    put(&w, ".local/bin/listed-hook", HOOK, 0o755);
    put(
        &w,
        ".grok/hooks/b.json",
        r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"listed-hook b"}]}]}}"#,
        0o644,
    );
    put(
        &w,
        ".grok/hooks/a.json",
        r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"listed-hook a"}]}]}}"#,
        0o644,
    );
    let h = crate::profile::Harness {
        profile: crate::profile::builtin("grok").unwrap(),
        exe: "/usr/bin/true".into(),
        install_mounts: vec![],
        path_dirs: vec![],
        source: "test".into(),
    };
    let opts = crate::build::Options {
        harness: "grok".into(),
        project: Some(w.project.clone()),
        assume_yes: true,
        ..Default::default()
    };
    let spec = crate::build::build_with(&w.env, &opts, h).unwrap().spec;
    // A snapshot of the host data has the user's file in the sandbox home.
    let stale = spec.home_src.join(".grok/hooks/a.json");
    std::fs::create_dir_all(stale.parent().unwrap()).unwrap();
    std::fs::copy(w.env.home.join(".grok/hooks/a.json"), &stale).unwrap();
    crate::privacy::write_settings(&spec.home_src, &spec.home_settings).unwrap();
    write(&spec).unwrap();
    let stop: Vec<String> = sandbox_commands(&spec)
        .into_iter()
        .filter(|c| c.0 == "Stop")
        .map(|c| c.1)
        .collect();
    assert_eq!(stop, ["listed-hook a", "listed-hook b"]);
    let s: Value = serde_json::from_slice(&std::fs::read(&stale).unwrap()).unwrap();
    assert_eq!(s["hooks"], serde_json::json!({}));
}

/// Each simple command of a line counts: the test builtin `[` does not
/// make the line resolved.
#[test]
fn every_program_of_a_shell_line_must_be_in_the_sandbox() {
    let w = world("[hooks]\nprograms = [\"~/.local/bin/listed-hook\"]\n");
    let prog = put(&w, ".local/bin/listed-hook", HOOK, 0o755);
    let gone = w.env.home.join(".local/bin/gone-hook");
    let cmds = [
        format!("[ ! -f '{0}' ] || '{0}' stop", gone.display()),
        format!("[ ! -f '{0}' ] || '{0}' stop", prog.display()),
        "[ -n \"$TOOL_HOME\" ] && \"$TOOL_HOME/bin/tool\" stop".to_string(),
        "test -x ~/.local/bin/listed-hook && listed-hook stop".to_string(),
    ];
    let handlers: Vec<Value> = cmds
        .iter()
        .map(|c| serde_json::json!({"hooks": [{"type": "command", "command": c}]}))
        .collect();
    let doc = serde_json::json!({"hooks": {"Stop": handlers}}).to_string();
    for name in ["claude", "codex"] {
        let (rel, _) = user_file(name);
        put(&w, rel, &doc, 0o644);
        let spec = start(&w, name).unwrap();
        let user: Vec<String> = sandbox_commands(&spec)
            .into_iter()
            .map(|c| c.1)
            .filter(|c| !c.starts_with("paddock-host"))
            .collect();
        // The missing path is left out everywhere. Codex keeps only the
        // lines that run a listed program, and trusts them.
        let want: Vec<&String> = match name {
            "codex" => vec![&cmds[1], &cmds[3]],
            _ => vec![&cmds[1], &cmds[2], &cmds[3]],
        };
        assert_eq!(user.iter().collect::<Vec<_>>(), want, "{name}");
        let notes: Vec<&String> = spec
            .notices
            .iter()
            .filter(|n| n.contains("hooks left out"))
            .collect();
        assert_eq!(notes.len(), 1, "{name}: {:?}", spec.notices);
        assert!(notes[0].contains("gone-hook"), "{}", notes[0]);
        assert_eq!(
            notes[0].contains("$TOOL_HOME/bin/tool"),
            name == "codex",
            "{}",
            notes[0]
        );
        let ex = explain(&spec).join("\n");
        if name == "codex" {
            assert!(
                ex.contains("$TOOL_HOME/bin/tool is not in [hooks] programs"),
                "{ex}"
            );
            assert_eq!(
                ex.matches("[hooks] programs listed-hook, trusted").count(),
                2,
                "{ex}"
            );
            let cfg: toml::Value = toml::from_str(
                &std::fs::read_to_string(spec.home_src.join(".codex/config.toml")).unwrap(),
            )
            .unwrap();
            let state = cfg["hooks"]["state"].as_table().unwrap();
            assert_eq!(
                state.keys().filter(|k| k.contains(":stop:")).count(),
                2,
                "{state:?}"
            );
        }
    }
}

/// An ignore rule silences an untrusted Codex entry, as any other.
#[test]
fn an_ignored_untrusted_codex_entry_has_no_notice() {
    let w = world("[hooks]\nignore = [\"tool\"]\n");
    put(
        &w,
        ".codex/hooks.json",
        r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"[ -n \"$TOOL_HOME\" ] && \"$TOOL_HOME/bin/tool\" stop"}]}]}}"#,
        0o644,
    );
    let spec = start(&w, "codex").unwrap();
    assert!(
        !spec.notices.iter().any(|n| n.contains("hooks left out")),
        "{:?}",
        spec.notices
    );
    assert!(sandbox_commands(&spec)
        .iter()
        .all(|c| c.1.starts_with("paddock-host")));
}

/// Grok Build 1.0.70 runs the hooks of ~/.grok/hooks only in a trusted
/// folder. The sandbox copy trusts the project, and holds no user hook that
/// the trust would turn on, other than a listed one.
#[test]
fn grok_trusts_the_project_for_paddock_and_listed_hooks_only() {
    let w = world("[hooks]\nprograms = [\"~/.local/bin/listed-hook\"]\n");
    put(&w, ".local/bin/listed-hook", HOOK, 0o755);
    put(
        &w,
        ".grok/hooks/mine.json",
        r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"true"},{"type":"command","command":"listed-hook stop"}]}]}}"#,
        0o644,
    );
    put(
        &w,
        ".grok/trusted_folders.toml",
        "[folders.\"/elsewhere\"]\ntrusted = true\n",
        0o600,
    );
    put(
        &w,
        ".grok/config.toml",
        "model = \"m\"\n[[hooks.Stop]]\nhooks = [{ type = \"command\", command = \"true\" }]\n",
        0o600,
    );
    let before = tree(&w.env.home);
    let spec = start(&w, "grok").unwrap();
    let home = &spec.home_src;
    let cmds: Vec<String> = sandbox_commands(&spec).into_iter().map(|c| c.1).collect();
    assert!(cmds.contains(&"paddock-host compacted --harness grok".to_string()));
    assert!(cmds.contains(&"listed-hook stop".to_string()), "{cmds:?}");
    assert!(!cmds.contains(&"true".to_string()), "{cmds:?}");
    // One trust: the project folder.
    let text = std::fs::read_to_string(home.join(".grok/trusted_folders.toml")).unwrap();
    let trust: toml::Value = toml::from_str(&text).unwrap();
    let folders = trust["folders"].as_table().unwrap();
    let project = std::fs::canonicalize(&w.project).unwrap();
    assert_eq!(folders.len(), 1, "{text}");
    let entry = &folders[&project.display().to_string()];
    assert_eq!(entry["trusted"].as_bool(), Some(true), "{text}");
    assert!(entry["decided_at"].as_integer().unwrap() > 0);
    // The copy of config.toml loses its hooks and the Claude and Cursor
    // hook scan, and keeps the rest.
    let cfg: toml::Value =
        toml::from_str(&std::fs::read_to_string(home.join(".grok/config.toml")).unwrap()).unwrap();
    assert!(cfg.get("hooks").is_none(), "{cfg:?}");
    assert_eq!(cfg["model"].as_str(), Some("m"));
    assert_eq!(cfg["compat"]["claude"]["hooks"].as_bool(), Some(false));
    assert_eq!(cfg["compat"]["cursor"]["hooks"].as_bool(), Some(false));
    let ex = explain(&spec).join("\n");
    assert!(ex.contains("[hooks] programs listed-hook, trusted"), "{ex}");
    // The host files stay byte for byte the same.
    assert_eq!(tree(&w.env.home), before);
}

/// Without paddock's hook and without a listed entry, nothing needs the
/// trust, and paddock writes none.
#[test]
fn grok_gets_no_trust_without_a_hook_to_trust() {
    let w = world("[compaction]\nenabled = false\n");
    put(
        &w,
        ".grok/hooks/mine.json",
        r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"true"}]}]}}"#,
        0o644,
    );
    let spec = start(&w, "grok").unwrap();
    assert!(!spec.home_src.join(".grok/trusted_folders.toml").exists());
    assert!(spec
        .notices
        .iter()
        .any(|n| n.contains("paddock does not trust them for grok in the sandbox: true.")));
}
