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
