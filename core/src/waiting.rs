//! Blocking reads: an agent waits for an event without `sleep`.
//!
//! A read of `paddock-host kv watch`, `paddock-host wait` or a waiting
//! device in `/dev/paddock` blocks until the event happens. The kernel or
//! the host process holds the reader, so it uses no CPU and no model tokens.
//! A tool call of a harness has a time limit, so every blocking read has a
//! longest wait. When that time passes before the event, the read ends with
//! one status that means "still waiting":
//!
//! - `paddock-host` exits with `EXIT_STILL_WAITING` (75, `EX_TEMPFAIL` of
//!   sysexits.h: "try again later"), and JSON responses have the status
//!   `waiting`.
//! - A device read fails with `ERRNO_STILL_WAITING` (`ETIME`, "timer
//!   expired"). No device uses that errno for anything else. `ETIMEDOUT`
//!   stays the error of a device that did not answer within its limit.
//!
//! The agent then starts the same read again. This module also holds the
//! time format of the `wait` device: RFC 3339 in UTC, or a duration.

use anyhow::{bail, Context as _, Result};

/// The exit code of `paddock-host` when the time limit of a blocking read
/// passed before the event: run the same command again.
pub const EXIT_STILL_WAITING: i32 = 75;

/// The errno of a device read whose time limit passed before the event.
pub const ERRNO_STILL_WAITING: i32 = libc::ETIME;

/// The default longest wait of one device read, in seconds. It is below the
/// default time limit of a shell tool call in the common harnesses (two
/// minutes), so a read with no `timeout` does not hit that limit.
pub const DEFAULT_READ_TIMEOUT: u64 = 100;

/// The longest wait of one device read, in seconds.
pub const MAX_READ_TIMEOUT: u64 = 600;

/// How far ahead the `wait` device accepts a time: seven days.
pub const MAX_WAIT_AHEAD: u64 = 7 * 24 * 3600;

/// Days since 1970-01-01 for a date of the proleptic Gregorian calendar.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// A Unix time as RFC 3339 in UTC, for example `2026-10-30T15:00:00Z`.
pub fn format_rfc3339(secs: u64) -> String {
    let days = (secs / 86400) as i64;
    let rest = secs % 86400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest / 60 % 60,
        rest % 60
    )
}

/// Parse an RFC 3339 time: `YYYY-MM-DDTHH:MM:SS`, optional fractions of a
/// second, and `Z` or an offset such as `+02:00`. A space may replace `T`.
pub fn parse_rfc3339(s: &str) -> Result<u64> {
    let s = s.trim();
    let bad = || anyhow::anyhow!("{s:?} is not an RFC 3339 time such as 2026-10-30T15:00:00Z");
    // Every slice below is by byte: a multibyte character would split one.
    if !s.is_ascii() {
        return Err(bad());
    }
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't' | b' ') {
        return Err(bad());
    }
    if b[13] != b':' || b[16] != b':' {
        return Err(bad());
    }
    let num = |r: std::ops::Range<usize>| -> Result<i64> {
        let t = s.get(r).ok_or_else(bad)?;
        if !t.bytes().all(|c| c.is_ascii_digit()) {
            return Err(bad());
        }
        t.parse::<i64>().map_err(|_| bad())
    };
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return Err(bad());
    }
    let mut rest = &s[19..];
    if let Some(r) = rest.strip_prefix('.') {
        let n = r.bytes().take_while(u8::is_ascii_digit).count();
        if n == 0 {
            return Err(bad());
        }
        rest = &r[n..];
    }
    let offset: i64 = match rest {
        "Z" | "z" => 0,
        o if o.len() == 6 && matches!(&o[..1], "+" | "-") && &o[3..4] == ":" => {
            let oh: i64 = o[1..3].parse().map_err(|_| bad())?;
            let om: i64 = o[4..6].parse().map_err(|_| bad())?;
            if oh > 23 || om > 59 {
                return Err(bad());
            }
            let v = oh * 3600 + om * 60;
            if o.starts_with('-') {
                -v
            } else {
                v
            }
        }
        _ => return Err(bad()),
    };
    let days = days_from_civil(y, mo as u32, d as u32);
    // Reject dates such as 2026-02-31, which would move to March.
    if civil_from_days(days) != (y, mo as u32, d as u32) {
        return Err(bad());
    }
    let t = days * 86400 + h * 3600 + mi * 60 + sec.min(59) - offset;
    u64::try_from(t).map_err(|_| bad())
}

/// The Unix time that `when` names: an RFC 3339 time, or a duration from
/// `now` (`90`, `90s`, `15m`, `2h`). It must be at most `MAX_WAIT_AHEAD`
/// after `now`. A time in the past is fine: the wait ends at once.
pub fn parse_when(when: &str, now: u64) -> Result<u64> {
    let w = when.trim();
    if w.is_empty() {
        bail!("the request is empty: write a time such as 2026-10-30T15:00:00Z or a duration such as 15m");
    }
    let at = if w.contains('-') && w.len() >= 20 {
        parse_rfc3339(w)?
    } else {
        let secs = crate::spawn::parse_duration(w)
            .with_context(|| format!("{w:?} is neither an RFC 3339 time nor a duration"))?;
        now.saturating_add(secs)
    };
    if at > now.saturating_add(MAX_WAIT_AHEAD) {
        bail!(
            "{} is more than {} days ahead; the wait device accepts at most that",
            format_rfc3339(at),
            MAX_WAIT_AHEAD / 86400
        );
    }
    Ok(at)
}

/// The longest wait of one read: `asked` seconds, or the default, and at
/// most `MAX_READ_TIMEOUT`.
pub fn read_timeout(asked: Option<u64>) -> Result<u64> {
    match asked {
        None => Ok(DEFAULT_READ_TIMEOUT),
        Some(t) if (1..=MAX_READ_TIMEOUT).contains(&t) => Ok(t),
        Some(_) => bail!("timeout must be 1 to {MAX_READ_TIMEOUT} seconds"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_round_trips() {
        for t in [0u64, 951782400, 1_793_372_400, 4_102_444_799] {
            assert_eq!(parse_rfc3339(&format_rfc3339(t)).unwrap(), t);
        }
        assert_eq!(format_rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(
            parse_rfc3339("2026-10-30T17:00:00+02:00").unwrap(),
            parse_rfc3339("2026-10-30T15:00:00Z").unwrap()
        );
        assert_eq!(
            parse_rfc3339("2026-10-30T15:00:00.250Z").unwrap(),
            parse_rfc3339("2026-10-30T15:00:00Z").unwrap()
        );
        for bad in [
            "2026-02-31T00:00:00Z",
            "2026-10-30T25:00:00Z",
            "2026-10-30T15:00:00",
            "2026-10-30",
            "tomorrow",
        ] {
            assert!(parse_rfc3339(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn when_takes_a_time_or_a_duration_and_is_capped() {
        let now = parse_rfc3339("2026-10-30T15:00:00Z").unwrap();
        assert_eq!(parse_when("90", now).unwrap(), now + 90);
        assert_eq!(parse_when("15m", now).unwrap(), now + 900);
        assert_eq!(parse_when(" 2h\n", now).unwrap(), now + 7200);
        assert_eq!(parse_when("2026-10-30T16:00:00Z", now).unwrap(), now + 3600);
        // In the past: the wait ends at once.
        assert_eq!(parse_when("2026-10-30T14:00:00Z", now).unwrap(), now - 3600);
        assert!(parse_when("8d", now).is_err());
        assert!(parse_when("200h", now).is_err());
        assert!(parse_when("2026-12-30T16:00:00Z", now).is_err());
        assert!(parse_when("", now).is_err());
        assert!(parse_when("soon", now).is_err());
    }

    #[test]
    fn read_timeouts_have_a_default_and_a_cap() {
        assert_eq!(read_timeout(None).unwrap(), DEFAULT_READ_TIMEOUT);
        assert_eq!(read_timeout(Some(30)).unwrap(), 30);
        assert!(read_timeout(Some(0)).is_err());
        assert!(read_timeout(Some(MAX_READ_TIMEOUT + 1)).is_err());
    }

    #[test]
    fn the_still_waiting_codes_are_distinct() {
        assert_eq!(EXIT_STILL_WAITING, 75);
        assert_ne!(ERRNO_STILL_WAITING, libc::ETIMEDOUT);
        assert_ne!(ERRNO_STILL_WAITING, libc::EAGAIN);
    }

    #[test]
    fn a_time_with_a_multibyte_character_is_refused_not_a_panic() {
        for t in [
            "2026-10-30T15:00:00\u{e9}0000",
            "2026-10-30T15:00:00+0\u{e9}00",
            "2026-10-30T15:00:0\u{e9}Z",
        ] {
            assert!(parse_rfc3339(t).is_err(), "{t}");
        }
    }
}
