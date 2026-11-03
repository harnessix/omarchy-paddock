//! The token expiry of a login file, in Unix seconds.

fn as_epoch_seconds(v: &serde_json::Value) -> Option<u64> {
    let raw = if let Some(n) = v.as_u64() {
        n
    } else if let Some(n) = v.as_i64() {
        u64::try_from(n).ok()?
    } else if let Some(f) = v.as_f64().filter(|f| f.is_finite() && *f > 0.0) {
        f as u64
    } else {
        let s = v.as_str().map(str::trim)?;
        if let Ok(n) = s.parse::<u64>() {
            n
        } else if let Ok(f) = s
            .parse::<f64>()
            .ok()
            .filter(|f| f.is_finite() && *f > 0.0)
            .ok_or(())
        {
            f as u64
        } else {
            return crate::keeper::parse_rfc3339_utc(s);
        }
    };
    (raw > 0).then_some(if raw >= 100_000_000_000 {
        raw / 1000
    } else {
        raw
    })
}

fn jwt_exp(token: &str) -> Option<u64> {
    use base64::Engine as _;
    let payload = token.split('.').nth(1)?;
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    as_epoch_seconds(&serde_json::from_slice::<serde_json::Value>(&raw).ok()?["exp"])
}

fn object_expiry(obj: &serde_json::Map<String, serde_json::Value>) -> Option<u64> {
    for k in ["expiresAt", "expires_at", "expiry_date", "expiry", "exp"] {
        if let Some(exp) = obj.get(k).and_then(as_epoch_seconds) {
            return Some(exp);
        }
    }
    for k in ["access_token", "id_token"] {
        if let Some(exp) = obj.get(k).and_then(|v| v.as_str()).and_then(jwt_exp) {
            return Some(exp);
        }
    }
    None
}

/// Extract the token expiry (in Unix seconds) from a login file when the
/// file names one.
pub fn token_expiry(bytes: &[u8], configured_path: Option<&str>) -> Option<u64> {
    if let Some(exp) = crate::keeper::access_expiry(bytes) {
        return Some(exp);
    }
    let doc: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    if let Some(path) = configured_path {
        let mut cur = Some(&doc);
        for part in path.split('.') {
            cur = cur.and_then(|c| c.get(part));
        }
        if let Some(exp) = cur.and_then(as_epoch_seconds) {
            return Some(exp);
        }
    }
    let obj = doc.as_object()?;
    if let Some(exp) = object_expiry(obj) {
        return Some(exp);
    }
    for sub in ["claudeAiOauth", "tokens", "oauth"] {
        if let Some(exp) = obj
            .get(sub)
            .and_then(|v| v.as_object())
            .and_then(object_expiry)
        {
            return Some(exp);
        }
    }
    if obj.len() == 1 {
        if let Some(exp) = obj
            .values()
            .next()
            .and_then(|v| v.as_object())
            .and_then(object_expiry)
        {
            return Some(exp);
        }
    }
    None
}
