use super::*;

/// A ChatGPT login as Codex writes it, with an access token that
/// expires at `exp`.
pub fn login(exp: u64, refresh: &str) -> Vec<u8> {
    use base64::Engine as _;
    let b64 = |v: serde_json::Value| {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v.to_string())
    };
    let at = format!(
        "{}.{}.sig",
        b64(serde_json::json!({"alg": "none"})),
        b64(serde_json::json!({"exp": exp}))
    );
    serde_json::to_vec(&serde_json::json!({
        "auth_mode": "chatgpt",
        "OPENAI_API_KEY": null,
        "tokens": {"id_token": "e30.e30.sig", "access_token": at, "refresh_token": refresh, "account_id": "acct-1"},
        "last_refresh": "2026-11-01T00:00:00Z"
    }))
    .unwrap()
}

#[test]
fn a_login_loses_only_its_refresh_token() {
    let l = login(2_000_000_000, "rt-1");
    assert!(keepable(&l));
    let s = strip(&l).unwrap();
    let v: serde_json::Value = serde_json::from_slice(&s).unwrap();
    // Codex needs the field; it is there, empty.
    assert_eq!(v["tokens"]["refresh_token"], "");
    assert_eq!(v["tokens"]["account_id"], "acct-1");
    assert_eq!(v["last_refresh"], "2026-11-01T00:00:00Z");
    assert!(!keepable(&s));
    assert!(strip(&s).is_none(), "nothing to take out twice");
    // An API-key login and other files are left alone.
    assert!(strip(br#"{"auth_mode":"apikey","OPENAI_API_KEY":"sk-x"}"#).is_none());
    assert!(strip(b"not json").is_none());
}

#[test]
fn a_refresh_is_due_before_codex_would_refresh_itself() {
    let cfg = Cfg::default();
    let now = 1_900_000_000;
    assert_eq!(access_expiry(&login(now + 3600, "rt")), Some(now + 3600));
    assert!(!due(&login(now + 3600, "rt"), now, &cfg));
    assert!(due(&login(now + cfg.margin, "rt"), now, &cfg));
    assert!(due(&login(now - 10, "rt"), now, &cfg));
    // No expiry: the age of the last refresh decides.
    let no_exp = br#"{"tokens":{"access_token":"opaque","refresh_token":"rt"},"last_refresh":"2026-11-01T00:00:00Z"}"#;
    let t = parse_rfc3339("2026-11-01T00:00:00Z").unwrap();
    assert!(!due(no_exp, t + 6 * 86400, &cfg));
    assert!(due(no_exp, t + 7 * 86400, &cfg));
}

#[test]
fn dates_and_settings() {
    assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
    assert_eq!(parse_rfc3339("2000-03-01T12:30:15.123Z"), Some(951_913_815));
    assert!(parse_rfc3339("yesterday").is_none());
    // An offset moves the time to UTC. agy writes this form.
    assert_eq!(
        parse_rfc3339("2026-11-03T13:33:52.237172342+08:00"),
        parse_rfc3339("2026-11-03T05:33:52Z")
    );
    assert_eq!(
        parse_rfc3339("2026-11-02T21:00:00-03:30"),
        parse_rfc3339("2026-11-03T00:30:00Z")
    );
    assert_eq!(Cfg::parse("").unwrap(), Cfg::default());
    let c = Cfg::parse("[logins]\nrefresh_margin = 3600\ncheck_interval = 5\n").unwrap();
    assert_eq!((c.margin, c.interval, c.enabled), (3600, 5, true));
    assert!(!Cfg::parse("[logins]\nkeep = false\n").unwrap().enabled);
    // Not later than Codex's own window of 5 minutes.
    assert!(Cfg::parse("[logins]\nrefresh_margin = 300\n").is_err());
    assert!(Cfg::parse("[logins]\nother = 1\n").is_err());
}
