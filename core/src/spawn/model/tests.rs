use super::*;
use crate::profile::{Profile, BUILTIN};

fn oneshot(name: &str) -> OneShot {
    let text = BUILTIN.iter().find(|(n, _)| *n == name).unwrap().1;
    let p: Profile = toml::from_str(text).unwrap();
    p.oneshot.unwrap()
}

fn bare() -> OneShot {
    toml::from_str("args = [\"run\"]\nprompt = \"stdin\"\n").unwrap()
}

fn pick(model: Option<&str>, effort: Option<Effort>) -> Pick {
    Pick {
        model: model.map(str::to_string),
        effort,
    }
}

#[test]
fn codex_gets_config_overrides_for_model_and_effort() {
    let o = oneshot("codex");
    let p = pick(Some("gpt-5.5"), Some(Effort::High));
    assert_eq!(
        p.args(&o),
        [
            "-c",
            "model=\"gpt-5.5\"",
            "-c",
            "model_reasoning_effort=\"high\""
        ]
    );
    // Only what was chosen.
    assert_eq!(
        pick(None, Some(Effort::Xhigh)).args(&o),
        ["-c", "model_reasoning_effort=\"xhigh\""]
    );
    assert!(Pick::default().args(&o).is_empty());
}

#[test]
fn claude_gets_its_model_and_effort_flags() {
    let o = oneshot("claude");
    assert_eq!(
        pick(Some("opus"), Some(Effort::Medium)).args(&o),
        ["--model", "opus", "--effort", "medium"]
    );
}

#[test]
fn a_harness_without_the_entries_refuses_the_flags() {
    let cfg = SpawnCfg::default();
    let e = choose(
        &cfg,
        "crush",
        &bare(),
        &pick(Some("m1"), None),
        &Pick::default(),
    )
    .unwrap_err()
    .to_string();
    assert!(e.contains("crush has no [oneshot] model_args"), "{e}");
    let e = choose(
        &cfg,
        "crush",
        &bare(),
        &pick(None, Some(Effort::High)),
        &Pick::default(),
    )
    .unwrap_err()
    .to_string();
    assert!(e.contains("crush has no [oneshot] effort_args"), "{e}");
}

#[test]
fn a_hint_or_the_default_is_left_out_where_the_harness_cannot_take_it() {
    let cfg = SpawnCfg {
        effort: Some(Effort::High),
        ..Default::default()
    };
    let c = choose(
        &cfg,
        "crush",
        &bare(),
        &Pick::default(),
        &pick(Some("m1"), None),
    )
    .unwrap();
    assert_eq!(c.pick, Pick::default());
    assert!(c.card.contains("Model: the default of crush"), "{}", c.card);
    assert!(c.card.contains("The task asks for model m1"), "{}", c.card);
    assert!(
        c.card.contains("Effort high ([spawn] effort) is left out"),
        "{}",
        c.card
    );
}

#[test]
fn the_flag_wins_over_the_hint_and_the_hint_over_the_configuration() {
    let cfg = SpawnCfg {
        effort: Some(Effort::Low),
        ..Default::default()
    };
    let o = oneshot("codex");
    let hint = pick(Some("gpt-5.4"), Some(Effort::High));
    let c = choose(&cfg, "codex", &o, &Pick::default(), &hint).unwrap();
    assert_eq!(c.pick, hint);
    let c = choose(
        &cfg,
        "codex",
        &o,
        &pick(Some("gpt-5.5"), Some(Effort::Xhigh)),
        &hint,
    )
    .unwrap();
    assert_eq!(c.pick, pick(Some("gpt-5.5"), Some(Effort::Xhigh)));
    let c = choose(&cfg, "codex", &o, &Pick::default(), &Pick::default()).unwrap();
    assert_eq!(c.pick, pick(None, Some(Effort::Low)));
}

#[test]
fn the_configured_list_refuses_another_model() {
    let doc: toml::Value = toml::from_str(
        "[spawn]\nharnesses = [\"codex\"]\nmodels = [\"gpt-5.5\"]\neffort = \"high\"\n",
    )
    .unwrap();
    let cfg = SpawnCfg::from_doc(&doc).unwrap();
    assert_eq!(cfg.models, ["gpt-5.5"]);
    assert_eq!(cfg.effort, Some(Effort::High));
    let o = oneshot("codex");
    for (asked, hint) in [
        (pick(Some("gpt-4o"), None), Pick::default()),
        (Pick::default(), pick(Some("gpt-4o"), None)),
    ] {
        let e = choose(&cfg, "codex", &o, &asked, &hint)
            .unwrap_err()
            .to_string();
        assert!(e.contains("gpt-4o"), "{e}");
        assert!(e.contains("[spawn] models"), "{e}");
    }
    assert!(choose(
        &cfg,
        "codex",
        &o,
        &pick(Some("gpt-5.5"), None),
        &Pick::default()
    )
    .is_ok());
    // Bad entries in the configuration.
    for bad in ["models = \"gpt\"", "models = [\"a b\"]", "effort = \"max\""] {
        let doc: toml::Value = toml::from_str(&format!("[spawn]\n{bad}\n")).unwrap();
        assert!(SpawnCfg::from_doc(&doc).is_err(), "{bad}");
    }
}

#[test]
fn a_model_name_cannot_carry_toml_or_a_flag() {
    for good in [
        "gpt-5.5",
        "claude-opus-5-5",
        "openai/gpt-oss:20b",
        "o3@2025",
    ] {
        assert!(valid_model(good), "{good}");
    }
    for bad in [
        "",
        "-x",
        "a\"b",
        "a=b",
        "a b",
        "a\nb",
        "a\\b",
        &"m".repeat(101),
    ] {
        assert!(!valid_model(bad), "{bad}");
    }
    let e = choose(
        &SpawnCfg::default(),
        "codex",
        &oneshot("codex"),
        &Pick::default(),
        &pick(Some("x\"y"), None),
    );
    assert!(e.is_err());
}

#[test]
fn the_card_names_the_model_the_effort_and_where_they_come_from() {
    let c = choose(
        &SpawnCfg::default(),
        "claude",
        &oneshot("claude"),
        &pick(Some("opus"), None),
        &pick(None, Some(Effort::High)),
    )
    .unwrap();
    assert_eq!(
        c.card,
        "Model: opus (asked with --model)\nEffort: high (the hint of the task)"
    );
    let c = choose(
        &SpawnCfg::default(),
        "codex",
        &oneshot("codex"),
        &Pick::default(),
        &Pick::default(),
    )
    .unwrap();
    assert_eq!(
        c.card,
        "Model: the default of codex\nEffort: the default of codex"
    );
}

#[test]
fn a_running_child_shows_its_pick_from_the_job() {
    let tmp = tempfile::tempdir().unwrap();
    let mut vars = std::collections::BTreeMap::new();
    vars.insert(
        "XDG_RUNTIME_DIR".into(),
        tmp.path().join("run").display().to_string(),
    );
    let env = HostEnv::from_parts(tmp.path().join("home"), "t".into(), 1000, 1000, vars);
    let dir = env.paddock_run().join("p-1/spawn/0011aabb");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("job.json"),
        r#"{"spawn_id":"0011aabb","harness":"codex","pick":{"model":"gpt-5.5","effort":"high"}}"#,
    )
    .unwrap();
    assert_eq!(
        of_child(&env, "p-1", "0011aabb"),
        Some(pick(Some("gpt-5.5"), Some(Effort::High)))
    );
    assert_eq!(of_child(&env, "p-1", "0011aabc"), None);
    assert_eq!(of_child(&env, "..", "0011aabb"), None);
}
