//! Static checks of the bar plugin, which has no QML test runner: its
//! strings exist in both languages, the paddock kv commands that the panel
//! runs parse, and a value from paddock kv never reaches a command line.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

fn read(p: &str) -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(p)).unwrap()
}

fn keys(file: &str) -> BTreeSet<String> {
    let v: serde_json::Value = serde_json::from_str(&read(file)).unwrap();
    v.as_object().unwrap().keys().cloned().collect()
}

#[test]
fn both_languages_have_every_string_the_plugin_uses() {
    let en = keys("plugin/i18n/en.json");
    let zh = keys("plugin/i18n/zh-CN.json");
    assert_eq!(en, zh, "en.json and zh-CN.json differ");
    for f in [
        "plugin/Panel.qml",
        "plugin/Service.qml",
        "plugin/BarWidget.qml",
        "plugin/Badge.qml",
    ] {
        for k in string_keys(&read(f)) {
            // Keys built at run time ("recent." + outcome) are not literals.
            if k.ends_with('.') {
                continue;
            }
            assert!(en.contains(&k), "{f} uses the missing string {k}");
        }
    }
}

/// The literal keys of `strings.t("...")` and `strings.tn("...")`.
fn string_keys(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for start in ["strings.t(\"", "strings.tn(\""] {
        let mut rest = text;
        while let Some(i) = rest.find(start) {
            rest = &rest[i + start.len()..];
            if let Some(end) = rest.find('"') {
                out.push(rest[..end].to_string());
            }
        }
    }
    out
}

#[test]
fn the_kv_commands_of_the_panel_parse() {
    let t = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(t.path().join("home")).unwrap();
    std::fs::create_dir_all(t.path().join("run")).unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_paddock"))
            .args(args)
            .env_clear()
            .env("HOME", t.path().join("home"))
            .env("USER", "demo")
            .env("PATH", "/usr/bin:/bin")
            .env("XDG_RUNTIME_DIR", t.path().join("run"))
            .current_dir(t.path())
            .output()
            .unwrap()
    };
    let out = run(&["kv", "overview", "--json"]);
    assert!(out.status.success(), "{out:?}");
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["api_version"], "paddock.kv/v1");
    // The shapes that Panel.qml builds: scope flags, then "--" and the key.
    let p = t.path().to_str().unwrap();
    let scopes: [&[&str]; 3] = [
        &["--pasture", "team", "--project", p],
        &["--user"],
        &["--legacy", "--project", p],
    ];
    for scope in scopes {
        let list = [&["kv", "list", "--json"][..], scope].concat();
        let out = run(&list);
        assert!(out.status.success(), "{list:?}: {out:?}");
        for (verb, extra) in [("get", Some("--json")), ("delete", None), ("release", None)] {
            let mut args = vec!["kv", verb];
            args.extend(extra);
            args.extend(scope);
            args.extend(["--", "-looks-like-a-flag"]);
            // 1: no such key. 2 would be a usage error.
            assert_eq!(run(&args).status.code(), Some(1), "{args:?}");
        }
    }
}

#[test]
fn a_kv_value_never_reaches_a_command_line() {
    let qml = read("plugin/Panel.qml");
    for (n, line) in qml.lines().enumerate() {
        let runs = ["command", "runPaddock(", "execDetached("]
            .iter()
            .any(|c| line.contains(c));
        let value = line.contains("kvEntry.value") || line.contains("e.value");
        assert!(
            !(runs && value),
            "Panel.qml:{}: a value next to a command: {line}",
            n + 1
        );
    }
    // A copy goes to wl-copy on standard input.
    assert!(qml.contains("command: [\"wl-copy\"]"));
    // Every value shown passes through plain().
    assert!(qml.contains("shown: root.plain(value, 4096)"));
}

#[test]
fn the_worktree_commands_of_the_panel_parse() {
    let t = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(t.path().join("home")).unwrap();
    std::fs::create_dir_all(t.path().join("run")).unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_paddock"))
            .args(args)
            .env_clear()
            .env("HOME", t.path().join("home"))
            .env("USER", "demo")
            .env("PATH", "/usr/bin:/bin")
            .env("XDG_RUNTIME_DIR", t.path().join("run"))
            .current_dir(t.path())
            .output()
            .unwrap()
    };
    let out = run(&["worktree", "list", "--json"]);
    assert!(out.status.success(), "{out:?}");
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["api_version"], "paddock.worktrees/v1");
    assert_eq!(v["worktrees"], serde_json::json!([]));
    // The shapes that Panel.qml builds: the verb, then an absolute path.
    for verb in ["merge", "remove"] {
        let out = run(&["worktree", verb, "/no/such/worktree"]);
        // The arguments parse, and paddock looks for the worktree.
        assert!(!out.status.success(), "{verb}: {out:?}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("no paddock worktree"), "{verb}: {err}");
    }
}

#[test]
fn the_view_commands_of_the_panel_parse() {
    let qml = read("plugin/Panel.qml");
    assert!(qml.contains(r#"["view", s.id, "--write"]"#));
    assert!(qml.contains(r#"["view", s.id]"#));
    let t = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(t.path().join("home")).unwrap();
    std::fs::create_dir_all(t.path().join("run")).unwrap();
    for args in [&["view", "abc123"][..], &["view", "abc123", "--write"][..]] {
        // An empty PATH: no tmux, no terminal, so no window opens.
        let out = Command::new(env!("CARGO_BIN_EXE_paddock"))
            .args(args)
            .env_clear()
            .env("HOME", t.path().join("home"))
            .env("USER", "demo")
            .env("PATH", t.path().join("none"))
            .env("XDG_RUNTIME_DIR", t.path().join("run"))
            .current_dir(t.path())
            .output()
            .unwrap();
        // The arguments parse, and paddock says why it cannot open a view.
        assert!(!out.status.success(), "{out:?}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("has no run log"), "{err}");
    }
}

/// A QML object with two functions of one name does not load: Qt reports
/// "Duplicate method name" and the whole panel stays away. Checks the
/// functions of each file's root object (two-space indent).
#[test]
fn no_qml_root_object_has_two_functions_of_one_name() {
    for file in [
        "plugin/Panel.qml",
        "plugin/Service.qml",
        "plugin/BarWidget.qml",
        "plugin/Badge.qml",
    ] {
        let mut seen = BTreeSet::new();
        for line in read(file).lines() {
            let Some(rest) = line.strip_prefix("  function ") else {
                continue;
            };
            let name: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            assert!(
                seen.insert(name.clone()),
                "{file}: two functions named {name}"
            );
        }
    }
}

/// The protected windows commands that the plugin runs parse, and the
/// plugin draws the same level colors as the border (windows.rs).
#[test]
fn the_window_commands_of_the_plugin_parse_and_colors_match() {
    let t = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(t.path().join("bin")).unwrap();
    // A hyprctl that cannot reach a compositor: no real window changes.
    let hyprctl = t.path().join("bin/hyprctl");
    std::fs::write(&hyprctl, "#!/bin/sh\nexit 1\n").unwrap();
    std::fs::set_permissions(
        &hyprctl,
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_paddock"))
            .args(args)
            .env_clear()
            .env("HOME", t.path())
            .env("USER", "demo")
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", t.path().join("bin").display()),
            )
            .env("XDG_RUNTIME_DIR", t.path())
            .output()
            .unwrap()
    };
    assert!(read("plugin/Service.qml").contains(r#""paddock", "windows", "--mark", "--json""#));
    assert!(read("plugin/Panel.qml").contains(r#""paddock", "windows", "--json""#));
    assert!(read("plugin/Panel.qml").contains(r#"["windows", "--focus", w.address]"#));
    for args in [&["windows", "--mark", "--json"][..], &["windows", "--json"]] {
        let out = run(args);
        assert!(out.status.success(), "{args:?}: {out:?}");
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(v["api_version"], "paddock.windows/v1");
    }
    let out = run(&["windows", "--focus", "0x1"]);
    assert!(!out.status.success(), "no protected window 0x1");

    use paddock::windows::Level;
    for file in ["plugin/Badge.qml", "plugin/Panel.qml"] {
        let text = read(file);
        for (level, light) in [
            (Level::Proxy, false),
            (Level::Proxy, true),
            (Level::Wide, false),
            (Level::Wide, true),
        ] {
            let border = level.border(light).0;
            let hex = &border[4..10];
            assert!(text.contains(&format!("\"#{hex}\"")), "{file} lacks #{hex}");
        }
    }
}

/// The badge never takes the keyboard, takes the pointer only on the pill,
/// and puts only a checked sandbox id on the command line.
#[test]
fn the_badge_stays_out_of_the_way() {
    let badge = read("plugin/Badge.qml");
    assert!(badge.contains("WlrLayershell.keyboardFocus: WlrKeyboardFocus.None"));
    assert!(badge.contains("mask: Region { item: surface.here ? pill : null }"));
    assert!(badge.contains("holdTimer"));
    // Text from profiles and window titles is never markup.
    assert_eq!(
        badge.matches("Text {").count(),
        badge.matches("textFormat: Text.PlainText").count()
    );
    assert!(badge.contains(r#"if (/^[A-Za-z0-9._-]{1,96}$/.test(id))"#));
    assert!(badge.contains(r#"["omarchy-shell", "harnessix.paddock", "show", id]"#));
    let bar = read("plugin/BarWidget.qml");
    assert!(bar.contains("function show(id: string): void"));
    assert!(bar.contains("function windows(): void"));
    let panel = read("plugin/Panel.qml");
    assert!(panel.contains(r#"if (!/^[A-Za-z0-9._-]{1,96}$/.test(id))"#));
    assert!(read("plugin/Service.qml").contains("Badge {"));
}
