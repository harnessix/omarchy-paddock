//! Tests of the training gate with the screens of agy 1.4, recorded at 49
//! columns on a real login. The screens after the first key are the
//! recorded one with the box or the focus changed.

use super::*;

use super::screens::*;

/// The rows that vt100 shows for this text on a 41 x `cols` terminal.
pub(crate) fn screen(text: &str, cols: u16) -> Vec<String> {
    let mut p = vt100::Parser::new(
        std::num::NonZeroU16::new(41).unwrap(),
        std::num::NonZeroU16::new(cols).unwrap(),
        0,
    );
    p.process(b"\x1b[H\x1b[2J");
    p.process(text.replace('\n', "\r\n").as_bytes());
    p.screen().rows(0, cols).collect()
}

fn agy_rules() -> Vec<Rule> {
    let p = crate::profile::builtin("agy").unwrap();
    let rules = p.training_rules();
    assert_eq!(rules.len(), 1);
    rules
}

fn machine() -> Machine {
    Machine::new(agy_rules())
}

#[test]
fn every_builtin_rule_is_valid() {
    for (name, text) in crate::profile::BUILTIN {
        let p: crate::profile::Profile = toml::from_str(text).unwrap();
        for r in p.privacy.training.iter().flat_map(|t| &t.rule) {
            r.check().unwrap_or_else(|e| panic!("{name}: {e:#}"));
        }
    }
}

#[test]
fn the_recorded_dialog_is_unchecked_and_done() {
    let mut m = machine();
    assert_eq!(m.on_screen(&screen(AFTER_LOGIN, 49)), Step::Wait);
    assert_eq!(
        m.on_screen(&screen(AFTER_THEME, 49)),
        Step::Send(Key::Enter)
    );
    assert_eq!(
        m.on_screen(&screen(&dialog(false, "box"), 49)),
        Step::Send(Key::Down)
    );
    assert_eq!(
        m.on_screen(&screen(&dialog(false, "done"), 49)),
        Step::Send(Key::Enter)
    );
    assert_eq!(
        m.on_screen(&screen(AFTER_DONE, 49)),
        Step::End(Outcome::Declined)
    );
    assert_eq!(m.sent(), &[Key::Enter, Key::Down, Key::Enter]);
}

#[test]
fn the_focus_on_previous_moves_right_to_done() {
    let mut m = machine();
    m.on_screen(&screen(AFTER_THEME, 49));
    assert_eq!(
        m.on_screen(&screen(&dialog(false, "box"), 49)),
        Step::Send(Key::Down)
    );
    assert_eq!(
        m.on_screen(&screen(&dialog(false, "prev"), 49)),
        Step::Send(Key::Right)
    );
    assert_eq!(
        m.on_screen(&screen(&dialog(false, "done"), 49)),
        Step::Send(Key::Enter)
    );
    assert_eq!(
        m.on_screen(&screen(AFTER_DONE, 49)),
        Step::End(Outcome::Declined)
    );
}

#[test]
fn a_dialog_that_differs_in_one_row_gets_no_key() {
    let changed = AFTER_THEME.replace(
        "my Interactions data, subject to the",
        "my Interactions data and code, subject to the",
    );
    let mut m = machine();
    assert!(matches!(
        m.on_screen(&screen(&changed, 49)),
        Step::End(Outcome::Stopped(_))
    ));
    assert!(m.sent().is_empty());
    // Nothing after the stop.
    assert_eq!(m.on_screen(&screen(AFTER_THEME, 49)), Step::Wait);
    assert!(m.sent().is_empty());
}

#[test]
fn a_wider_terminal_wraps_the_dialog_and_gets_no_key() {
    // At 100 columns agy puts more words on a row. The rule holds at 49
    // columns only, which is why paddock gives agy that width.
    let wide = AFTER_THEME.replace(
        "AI coding agents are known to have certain\nsecurity risks",
        "AI coding agents are known to have certain security risks",
    );
    let mut m = machine();
    assert!(matches!(
        m.on_screen(&screen(&wide, 100)),
        Step::End(Outcome::Stopped(_))
    ));
    assert!(m.sent().is_empty());
}

#[test]
fn enter_on_done_needs_the_box_unchecked() {
    let mut m = machine();
    m.on_screen(&screen(AFTER_THEME, 49));
    m.on_screen(&screen(&dialog(false, "box"), 49));
    // The box shows checked again with the focus on [Done]: no Enter.
    assert!(matches!(
        m.on_screen(&screen(&dialog(true, "done"), 49)),
        Step::End(Outcome::Stopped(_))
    ));
    assert_eq!(m.sent(), &[Key::Enter, Key::Down]);
}

#[test]
fn a_key_without_effect_stops_the_gate() {
    let mut m = machine();
    m.on_screen(&screen(AFTER_THEME, 49));
    // The box is still checked after Enter.
    assert!(matches!(
        m.on_screen(&screen(AFTER_THEME, 49)),
        Step::End(Outcome::Stopped(_))
    ));
    assert_eq!(m.sent(), &[Key::Enter]);
}

#[test]
fn the_dialog_twice_on_screen_gets_no_key() {
    let twice = format!("{AFTER_THEME}{AFTER_THEME}");
    let mut p = vt100::Parser::new(
        std::num::NonZeroU16::new(90).unwrap(),
        std::num::NonZeroU16::new(49).unwrap(),
        0,
    );
    p.process(twice.replace('\n', "\r\n").as_bytes());
    let rows: Vec<String> = p.screen().rows(0, 49).collect();
    let mut m = machine();
    assert!(matches!(m.on_screen(&rows), Step::End(Outcome::Stopped(_))));
    assert!(m.sent().is_empty());
}

#[test]
fn a_user_key_cancels_the_gate_under_way() {
    let mut m = machine();
    m.on_screen(&screen(AFTER_THEME, 49));
    let now = screen(&dialog(false, "box"), 49);
    assert_eq!(m.on_user_key(&now), UserKey::End(Outcome::Cancelled));
    assert_eq!(m.on_screen(&now), Step::Wait);
    assert_eq!(m.sent(), &[Key::Enter]);
}

#[test]
fn a_user_key_on_the_dialog_cancels_the_gate_before_any_key() {
    let mut m = machine();
    m.on_screen(&screen(AFTER_LOGIN, 49));
    // The dialog is drawn but not settled yet when the key comes.
    assert_eq!(
        m.on_user_key(&screen(AFTER_THEME, 49)),
        UserKey::End(Outcome::Cancelled)
    );
    assert_eq!(m.on_screen(&screen(AFTER_THEME, 49)), Step::Wait);
    assert!(m.sent().is_empty());
}

#[test]
fn user_keys_on_the_login_and_theme_screens_keep_the_gate() {
    let mut m = machine();
    m.on_screen(&screen(LOGIN, 49));
    assert_eq!(m.on_user_key(&screen(LOGIN, 49)), UserKey::Keep);
    m.on_screen(&screen(AFTER_LOGIN, 49));
    assert_eq!(m.on_user_key(&screen(AFTER_LOGIN, 49)), UserKey::Keep);
    assert_eq!(
        m.on_screen(&screen(AFTER_THEME, 49)),
        Step::Send(Key::Enter)
    );
}

#[test]
fn a_user_key_on_another_screen_ends_the_gate() {
    let mut m = machine();
    m.on_screen(&screen(AFTER_DONE, 49));
    assert!(matches!(
        m.on_user_key(&screen(AFTER_DONE, 49)),
        UserKey::End(Outcome::NotShown(_))
    ));
    // The dialog cannot come after that, for example drawn by the model.
    assert_eq!(m.on_screen(&screen(AFTER_THEME, 49)), Step::Wait);
    assert!(m.sent().is_empty());
    assert_eq!(m.audit_detail(), None);
}

#[test]
fn a_resize_stops_the_gate() {
    let mut m = machine();
    m.on_screen(&screen(AFTER_THEME, 49));
    assert!(matches!(m.on_resize(), Some(Outcome::Stopped(_))));
    assert_eq!(m.on_screen(&screen(&dialog(false, "box"), 49)), Step::Wait);
    assert_eq!(m.sent(), &[Key::Enter]);
}

#[test]
fn the_audit_line_holds_no_screen_text() {
    let mut m = machine();
    m.on_screen(&screen(AFTER_THEME, 49));
    m.on_screen(&screen(&dialog(false, "box"), 49));
    m.on_screen(&screen(&dialog(false, "done"), 49));
    m.on_screen(&screen(AFTER_DONE, 49));
    let line = m.audit_detail().unwrap();
    assert_eq!(
        line,
        "training rule agy-1.4-data-use: keys enter,down,enter: declined"
    );
    for text in [AFTER_THEME, AFTER_DONE] {
        for row in text.lines().map(str::trim).filter(|r| r.len() > 3) {
            assert!(!line.contains(row), "{row}");
        }
    }
}

#[test]
fn a_rule_cannot_send_a_letter() {
    let text = "id = \"s\"\nrows = [\"T\"]\nsend = \"y\"\nnext = [\"gone\"]\n";
    assert!(toml::from_str::<State>(text).is_err());
}

#[test]
fn a_rule_with_an_unknown_next_state_is_invalid() {
    let mut r = agy_rules().remove(0);
    r.state[0].next = vec!["nowhere".into()];
    assert!(r.check().is_err());
    let mut r = agy_rules().remove(0);
    r.state[0].rows[0] = " Terms of Service & Data Use".into();
    assert!(r.check().is_err());
}

fn env_spec(t: &std::path::Path, harness: &str) -> (crate::env::HostEnv, crate::spec::SandboxSpec) {
    let env = crate::env::HostEnv::from_parts(
        t.join("home"),
        "demo".into(),
        1000,
        1000,
        Default::default(),
    );
    let spec = crate::spec::SandboxSpec::new(
        "t".into(),
        harness,
        &t.join("project"),
        t.join("sandbox-home"),
        &env.home,
        1000,
        1000,
        "demo",
    );
    (env, spec)
}

#[test]
fn manual_and_vendor_type_nothing() {
    let t = tempfile::tempdir().unwrap();
    let (env, spec) = env_spec(t.path(), "agy");
    let p = crate::profile::builtin("agy").unwrap();
    assert!(prepare(&env, &p, Policy::Vendor, &spec, true)
        .unwrap()
        .is_none());
    assert!(prepare(&env, &p, Policy::Vendor, &spec, false)
        .unwrap()
        .is_none());
    assert!(prepare(&env, &p, Policy::Manual, &spec, true)
        .unwrap()
        .is_none());
    assert!(prepare(&env, &p, Policy::Deny, &spec, true)
        .unwrap()
        .is_some());
    // Without a terminal the harness runs on pipes, with nothing to answer.
    assert!(prepare(&env, &p, Policy::Deny, &spec, false)
        .unwrap()
        .is_none());
}

#[test]
fn manual_without_a_terminal_stops_while_the_onboarding_is_open() {
    let t = tempfile::tempdir().unwrap();
    let (env, spec) = env_spec(t.path(), "agy");
    let p = crate::profile::builtin("agy").unwrap();
    let Err(e) = prepare(&env, &p, Policy::Manual, &spec, false) else {
        panic!("manual without a terminal must stop");
    };
    assert!(format!("{e:#}").contains("in a terminal"), "{e:#}");
    // A finished onboarding: the dialog does not come, and nothing stops.
    let (rel, keys) = p.state.done_flags.iter().next().unwrap();
    let file = spec.home_src.join(rel);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    let done: serde_json::Map<String, serde_json::Value> = keys
        .iter()
        .map(|k| (k.clone(), serde_json::Value::Bool(true)))
        .collect();
    std::fs::write(&file, serde_json::to_vec(&done).unwrap()).unwrap();
    assert!(prepare(&env, &p, Policy::Manual, &spec, false)
        .unwrap()
        .is_none());
    assert!(prepare(&env, &p, Policy::Deny, &spec, true)
        .unwrap()
        .is_none());
}

#[test]
fn a_harness_without_rules_gets_no_gate() {
    let t = tempfile::tempdir().unwrap();
    let (env, spec) = env_spec(t.path(), "claude");
    let p = crate::profile::builtin("claude").unwrap();
    for policy in [Policy::Deny, Policy::Manual, Policy::Vendor] {
        assert!(prepare(&env, &p, policy, &spec, false).unwrap().is_none());
        assert!(prepare(&env, &p, policy, &spec, true).unwrap().is_none());
    }
}

#[test]
fn the_summary_names_the_rule_and_the_setting() {
    let ok = (Some("r1".to_string()), Outcome::Declined);
    assert_eq!(
        summary(&ok, Some("s")).unwrap(),
        "training rule r1: declined"
    );
    let stop = (Some("r1".to_string()), Outcome::Stopped("x".into()));
    assert_eq!(
        summary(&stop, Some("open /settings")).unwrap(),
        "training rule r1: stopped: x. Check the data-use choice: open /settings"
    );
    assert_eq!(summary(&(None, Outcome::NotShown("y".into())), None), None);
}
