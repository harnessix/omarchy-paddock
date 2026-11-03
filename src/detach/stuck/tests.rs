use super::*;

const QUOTA: &str = "\
 ▸ Thinking

  Individual quota reached. Your quota resets in 3h 12m.

 > Type a message
";

#[test]
fn the_same_screen_for_the_stuck_time_is_stuck_once() {
    let mut d = Detector::default();
    let t = 1_000;
    assert_eq!(d.observe(t, "a", 180), Event::Nothing);
    assert_eq!(d.observe(t + 100, "a", 180), Event::Nothing);
    assert_eq!(d.observe(t + 179, "a", 180), Event::Nothing);
    assert_eq!(
        d.observe(t + 180, "a", 180),
        Event::Stuck {
            since: t,
            screen: "a".into()
        }
    );
    // One report per stuck screen.
    assert_eq!(d.observe(t + 500, "a", 180), Event::Nothing);
    // The screen moves on.
    assert_eq!(d.observe(t + 501, "b", 180), Event::Moved);
    assert_eq!(d.observe(t + 600, "b", 180), Event::Nothing);
    assert!(matches!(
        d.observe(t + 681, "b", 180),
        Event::Stuck { since, .. } if since == t + 501
    ));
}

#[test]
fn a_changing_screen_is_never_stuck() {
    let mut d = Detector::default();
    for i in 0..100u64 {
        let screen = format!("Working ({i}s)");
        assert_eq!(d.observe(i * 10, &screen, 180), Event::Nothing);
    }
}

#[test]
fn stuck_after_zero_is_off() {
    let mut d = Detector::default();
    d.observe(0, "a", 0);
    assert_eq!(d.observe(100_000, "a", 0), Event::Nothing);
}

#[test]
fn the_stuck_record_holds_the_trimmed_screen() {
    let mut d = Detector::default();
    d.observe(0, &format!("{QUOTA}\n\n\n"), 60);
    let Event::Stuck { screen, .. } = d.observe(60, &format!("{QUOTA}\n\n\n"), 60) else {
        panic!()
    };
    assert!(screen.starts_with("▸ Thinking"), "{screen}");
    assert!(screen.ends_with("> Type a message"), "{screen}");
}

#[test]
fn the_idle_done_text_shows_the_last_screen() {
    let screen = crate::screens::last_screen(QUOTA);
    let t = idle_done_text(2400, Some(&screen), None);
    assert!(
        t.starts_with("ended after 40 minutes without output\nlast screen:\n▸ Thinking"),
        "{t}"
    );
    assert!(t.contains("Individual quota reached"), "{t}");
    let err = crate::screens::known_error(&[], &screen);
    let t = idle_done_text(2400, Some(&screen), err.as_deref());
    assert!(
        t.starts_with(
            "ended after 40 minutes without output. the harness says its quota is used up: Individual quota reached"
        ),
        "{t}"
    );
    assert_eq!(
        idle_done_text(2400, None, None),
        "ended after 40 minutes without output"
    );
    assert_eq!(
        idle_done_text(10, Some("  \n"), None),
        "ended after 10 seconds without output"
    );
}

#[test]
fn stuck_after_is_a_detach_key() {
    let c = crate::detach::IdleCfg::parse("[detach]\nstuck_after = \"2m\"\n").unwrap();
    assert_eq!(c.stuck, 120);
    let c = crate::detach::IdleCfg::parse("").unwrap();
    assert_eq!(c.stuck, STUCK_AFTER);
    assert_eq!(STUCK_AFTER, 180);
}

#[test]
fn a_note_goes_into_the_record() {
    let tmp = tempfile::tempdir().unwrap();
    let env = crate::detach::tests::env_in(tmp.path());
    let rec = Record {
        id: "codex-abc123".into(),
        harness: "codex".into(),
        ..Default::default()
    };
    rec.save(&env).unwrap();
    add_note(
        &env,
        "codex-abc123",
        "answered Codex hooks review: x\x1b[2J",
    )
    .unwrap();
    let r = Record::load(&env, "codex-abc123").unwrap();
    assert_eq!(r.notes, ["answered Codex hooks review: x [2J"]);
}

/// The stuck detector against a real pane, as in detach/tests.rs.
#[test]
fn a_pane_on_a_quota_screen_is_stuck_and_names_the_error() {
    use crate::detach::tests::{ctl, quiet_session, record, until, Server};
    use std::time::Duration;
    let t = tempfile::tempdir().unwrap();
    let env = crate::detach::tests::env_in(t.path());
    let Some(tmux) = crate::view::Tmux::find(&env) else {
        eprintln!("skipped: needs tmux");
        return;
    };
    let _server = Server(tmux.clone());
    let dir = env.paddock_config();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("config.toml"),
        "[detach]\nidle_after = \"1h\"\nstuck_after = \"2s\"\n",
    )
    .unwrap();
    let id = "stuck-pane-1";
    quiet_session(
        &tmux,
        id,
        "printf 'Thinking\\n\\nIndividual quota reached. Try later.\\n'; exec sleep 60",
    );
    record(&env, id);
    let (c, stops) = ctl(&env, id);
    let errors = vec![crate::screens::ErrorRule {
        text: "Individual quota reached".into(),
        says: "agy says the individual quota is used up".into(),
    }];
    c.watch(errors);
    until("the stuck record", Duration::from_secs(20), || {
        Record::load(&env, id).is_some_and(|r| r.stuck.is_some())
    });
    let s = Record::load(&env, id).unwrap().stuck.unwrap();
    assert_eq!(s.screen, "Thinking\n\nIndividual quota reached. Try later.");
    assert_eq!(
        s.error.as_deref(),
        Some("agy says the individual quota is used up: Individual quota reached. Try later.")
    );
    assert_eq!(stops.load(std::sync::atomic::Ordering::SeqCst), 0);
}
