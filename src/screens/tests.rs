use super::live::Watcher;
use super::*;
use std::time::{Duration, Instant};

/// The Codex 0.166.0 screen of a detached run on 2026-11-03.
const CODEX_HOOKS: &str = "\
  Hooks need review
  1 hook is new or changed.
  Hooks can run outside the sandbox after you trust them.
› 1. Review hooks
  2. Trust all and continue
  3. Continue without trusting (hooks won't run)
  enter confirm · esc skip
";

/// The same screen with the choices in another order: no rule matches.
const CODEX_HOOKS_CHANGED: &str = "\
  Hooks need review
  1 hook is new or changed.
  Hooks can run outside the sandbox after you trust them.
› 1. Review hooks
  2. Continue without trusting (hooks won't run)
  3. Trust all and continue
  enter confirm · esc skip
";

/// Two hooks.
const CODEX_HOOKS_TWO: &str = "\
  Hooks need review
  2 hooks are new or changed.
  Hooks can run outside the sandbox after you trust them.
› 1. Review hooks
  2. Trust all and continue
  3. Continue without trusting (hooks won't run)
  enter confirm · esc skip
";

/// An agy run that stopped after its start.
const AGY_QUOTA: &str = "\
 ▸ Thinking

  Individual quota reached. Your quota resets in 3h 12m.


 > Type a message
";

fn rows(screen: &str) -> Vec<String> {
    let mut r: Vec<String> = screen.lines().map(str::to_string).collect();
    r.insert(0, "codex 0.166.0".into());
    r.push(String::new());
    r
}

fn codex() -> crate::profile::Profile {
    toml::from_str(include_str!("../../profiles/codex.toml")).unwrap()
}

fn agy() -> crate::profile::Profile {
    toml::from_str(include_str!("../../profiles/agy.toml")).unwrap()
}

fn rule(answer: &str, send: &[&str]) -> Rule {
    Rule {
        id: "t".into(),
        name: "Test".into(),
        rows: vec![
            "Pick one".into(),
            "› 1. Trust all and continue".into(),
            "2. Continue without trusting".into(),
            "3. Yes, I accept the terms".into(),
            "4) Skip".into(),
        ],
        answer: answer.into(),
        send: send.iter().map(|s| s.to_string()).collect(),
        note: "skipped".into(),
        always: false,
    }
}

#[test]
fn the_codex_rules_load() {
    let p = codex();
    assert_eq!(p.screens.rule.len(), 2);
    for r in &p.screens.rule {
        r.check().unwrap();
        assert!(!r.always, "{}", r.id);
        assert_eq!(r.send, ["3"]);
    }
}

#[test]
fn the_codex_hooks_screen_matches_with_its_count() {
    let p = codex();
    let rules = valid("codex", &p.screens.rule);
    let found = find(&rules, &rows(CODEX_HOOKS));
    let Found::Rule(i, n) = found else {
        panic!("{found:?}")
    };
    assert_eq!(rules[i].id, "codex-hooks-review");
    assert_eq!(n, Some(1));
    assert_eq!(
        rules[i].note_for(n),
        "answered Codex hooks review: continued without trusting 1 hook"
    );
    let found = find(&rules, &rows(CODEX_HOOKS_TWO));
    let Found::Rule(i, n) = found else {
        panic!("{found:?}")
    };
    assert_eq!(
        rules[i].note_for(n),
        "answered Codex hooks review: continued without trusting 2 hooks"
    );
}

/// The Codex 0.166.0 screen as a real terminal shows it: empty rows between
/// the parts.
const CODEX_HOOKS_REAL: &str = "  Hooks need review\n  1 hook is new or changed.\n  Hooks can run outside the sandbox after you trust them.\n\n\u{203a} 1. Review hooks\n  2. Trust all and continue\n  3. Continue without trusting (hooks won't run)\n\n  enter confirm \u{b7} esc skip\n";

#[test]
fn the_codex_hooks_screen_matches_with_empty_rows_between_its_parts() {
    let rules = valid("codex", &codex().screens.rule);
    let found = find(&rules, &rows(CODEX_HOOKS_REAL));
    let Found::Rule(i, n) = found else {
        panic!("{found:?}")
    };
    assert_eq!(rules[i].id, "codex-hooks-review");
    assert_eq!(n, Some(1));
}

#[test]
fn a_changed_screen_does_not_match() {
    let rules = valid("codex", &codex().screens.rule);
    assert_eq!(find(&rules, &rows(CODEX_HOOKS_CHANGED)), Found::Nothing);
    // A part of the block is not enough.
    let half: String = CODEX_HOOKS.lines().take(4).collect::<Vec<_>>().join("\n");
    assert_eq!(find(&rules, &rows(&half)), Found::Nothing);
    // Twice on the screen: paddock sends nothing.
    let twice = format!("{CODEX_HOOKS}{CODEX_HOOKS}");
    assert_eq!(find(&rules, &rows(&twice)), Found::Unclear);
    // The count must be a number.
    let words = CODEX_HOOKS.replace("1 hook is", "one hook is");
    assert_eq!(find(&rules, &rows(&words)), Found::Nothing);
}

#[test]
fn the_guard_refuses_an_answer_that_grants() {
    assert!(grants("2. Trust all and continue"));
    assert!(grants("Yes, I accept"));
    assert!(grants("Approve for this session"));
    assert!(grants("Log in with ChatGPT"));
    assert!(grants("Allow always"));
    assert!(!grants("3. Continue without trusting (hooks won't run)"));
    assert!(!grants("4) Skip"));

    let e = rule("› 1. Trust all and continue", &["enter"])
        .check()
        .unwrap_err();
    assert!(format!("{e}").contains("grants trust"), "{e}");
    assert!(rule("3. Yes, I accept the terms", &["3"]).check().is_err());
    // The key must choose the answer row.
    let e = rule("2. Continue without trusting", &["1"])
        .check()
        .unwrap_err();
    assert!(format!("{e}").contains("does not choose"), "{e}");
    // Enter alone needs the focus on the answer.
    assert!(rule("2. Continue without trusting", &["enter"])
        .check()
        .is_err());
    // The answer must be on the screen, and the keys must be keys.
    assert!(rule("Skip", &["4"]).check().is_err());
    assert!(rule("4) Skip", &["x"]).check().is_err());
    assert!(rule("4) Skip", &["4"]).check().is_ok());
    assert!(rule("2. Continue without trusting", &["2"]).check().is_ok());
    assert!(rule("2. Continue without trusting", &["down", "enter"])
        .check()
        .is_ok());
}

#[test]
fn a_profile_rule_that_grants_does_not_load() {
    let text = r#"
name = "x"
[[screens.rule]]
id = "bad"
name = "Bad"
send = ["2"]
answer = "2. Trust all and continue"
note = "trusted"
rows = ["Hooks need review", "2. Trust all and continue"]
"#;
    let p: crate::profile::Profile = toml::from_str(text).unwrap();
    assert!(valid("x", &p.screens.rule).is_empty());
    assert!(
        explain(&p)[0].contains("does not load"),
        "{:?}",
        explain(&p)
    );
}

#[test]
fn a_quota_screen_has_clear_words() {
    let agy = agy();
    let e = known_error(&agy.screens.error, AGY_QUOTA).unwrap();
    assert!(
        e.starts_with("agy says the individual quota is used up: Individual quota reached"),
        "{e}"
    );
    // Without the profile entry, the general words still match.
    let e = known_error(&[], AGY_QUOTA).unwrap();
    assert!(
        e.starts_with("the harness says its quota is used up"),
        "{e}"
    );
    let e = known_error(&[], "Error: rate limit exceeded, retry in 20s").unwrap();
    assert!(e.contains("rate limit"), "{e}");
    assert_eq!(known_error(&[], CODEX_HOOKS), None);
}

#[test]
fn the_last_screen_is_trimmed() {
    let s = last_screen(AGY_QUOTA);
    assert_eq!(
        s,
        "▸ Thinking\n\nIndividual quota reached. Your quota resets in 3h 12m.\n\n> Type a message"
    );
    let long: String = (0..40).map(|i| format!("line {i}\n")).collect();
    let s = last_screen(&format!("\x1b[1m{long}\x1b[0m\n\n\n"));
    assert_eq!(s.lines().count(), SCREEN_LINES);
    assert!(s.ends_with("line 39"), "{s}");
}

#[test]
fn explain_lists_the_rules() {
    let lines = explain(&codex());
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(lines[0].contains("codex-hooks-review"), "{lines:?}");
    assert!(lines[0].contains("keys 3"), "{lines:?}");
    assert!(lines[0].contains("when nobody watches"), "{lines:?}");
    assert!(explain(&agy())
        .iter()
        .any(|l| l.contains("Individual quota reached")));
}

fn screen_bytes(screen: &str) -> Vec<u8> {
    format!("\x1b[2J\x1b[H{}", screen.replace('\n', "\r\n")).into_bytes()
}

#[test]
fn the_watcher_answers_and_reports() {
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let s2 = seen.clone();
    let w = Watcher::new(
        valid("codex", &codex().screens.rule),
        || false,
        move |r: &live::Report| s2.lock().unwrap().push(r.clone()),
    );
    let t0 = Instant::now();
    assert_eq!(w.decide(b"starting\r\n", t0), None);
    assert_eq!(
        w.decide(&screen_bytes(CODEX_HOOKS), t0).as_deref(),
        Some(&b"3"[..])
    );
    // The screen is still there: no second key while the answer is open.
    assert_eq!(w.decide(b"", t0 + Duration::from_secs(1)), None);
    assert!(seen.lock().unwrap().is_empty());
    assert_eq!(w.decide(&screen_bytes("> working\n"), t0), None);
    let r = seen.lock().unwrap().clone();
    assert_eq!(r.len(), 1, "{r:?}");
    assert!(r[0].ok);
    assert_eq!(r[0].rule, "codex-hooks-review");
    assert_eq!(r[0].keys, "3");
    assert_eq!(
        r[0].text,
        "answered Codex hooks review: continued without trusting 1 hook"
    );
    assert_eq!(w.reports(), r);
}

#[test]
fn the_watcher_stops_when_the_screen_stays() {
    let w = Watcher::new(valid("codex", &codex().screens.rule), || false, |_| {});
    let t0 = Instant::now();
    assert!(w.decide(&screen_bytes(CODEX_HOOKS), t0).is_some());
    assert_eq!(w.decide(b"", t0 + Duration::from_secs(6)), None);
    let r = w.reports();
    assert_eq!(r.len(), 1);
    assert!(!r[0].ok);
    assert!(r[0].text.contains("stayed on the screen"), "{r:?}");
    // No more tries for that rule.
    assert_eq!(w.decide(&screen_bytes(CODEX_HOOKS), t0), None);
}

#[test]
fn a_watched_run_gets_no_answer_unless_the_rule_is_always() {
    let mut rules = valid("codex", &codex().screens.rule);
    let w = Watcher::new(rules.clone(), || true, |_| {});
    assert_eq!(w.decide(&screen_bytes(CODEX_HOOKS), Instant::now()), None);
    rules[0].always = true;
    let w = Watcher::new(rules, || true, |_| {});
    assert!(w
        .decide(&screen_bytes(CODEX_HOOKS), Instant::now())
        .is_some());
}

#[test]
fn a_user_key_ends_the_watcher() {
    use crate::relay::Tap;
    let w = Watcher::new(valid("codex", &codex().screens.rule), || false, |_| {});
    assert!(w.active());
    w.user_key();
    assert!(!w.active());
}
