//! Sizes, CPU counts and durations of the spawn limits.

use anyhow::{bail, Context as _, Result};

/// Parse a size such as 512M, 4G or 1073741824.
pub fn parse_bytes(s: &str) -> Result<u64> {
    let s = s.trim();
    let (num, mul) = match s.char_indices().last() {
        Some((i, c)) if c.is_ascii_alphabetic() => {
            let mul: u64 = match c.to_ascii_uppercase() {
                'K' => 1 << 10,
                'M' => 1 << 20,
                'G' => 1 << 30,
                'T' => 1 << 40,
                _ => bail!("{s}: use a size such as 512M or 4G"),
            };
            (&s[..i], mul)
        }
        _ => (s, 1),
    };
    let n: u64 = num
        .parse()
        .with_context(|| format!("{s}: use a size such as 512M or 4G"))?;
    let b = n
        .checked_mul(mul)
        .with_context(|| format!("{s} is too large"))?;
    if b < 64 << 20 {
        bail!("{s} is too small: a harness needs at least 64M");
    }
    Ok(b)
}

pub fn format_bytes(b: u64) -> String {
    for (unit, size) in [("T", 1u64 << 40), ("G", 1 << 30), ("M", 1 << 20)] {
        if b >= size && b.is_multiple_of(size) {
            return format!("{}{unit}", b / size);
        }
    }
    format!("{}M", b >> 20)
}

/// Parse a CPU count such as 2 or 1.5 into percent of one CPU.
pub fn parse_cpus(s: &str) -> Result<u32> {
    let f: f64 = s
        .trim()
        .parse()
        .with_context(|| format!("{s}: use a number of CPUs such as 2 or 0.5"))?;
    if !(0.1..=256.0).contains(&f) {
        bail!("{s}: use a number of CPUs from 0.1 to 256");
    }
    Ok((f * 100.0).round() as u32)
}

pub fn format_cpus(p: u32) -> String {
    if p.is_multiple_of(100) {
        format!("{}", p / 100)
    } else {
        format!("{:.2}", p as f64 / 100.0)
            .trim_end_matches('0')
            .to_string()
    }
}

/// Parse a duration such as 90, 90s, 30m or 2h into seconds.
pub fn parse_duration(s: &str) -> Result<u64> {
    let s = s.trim();
    let (num, mul) = match s.chars().last() {
        Some('s') => (&s[..s.len() - 1], 1),
        Some('m') => (&s[..s.len() - 1], 60),
        Some('h') => (&s[..s.len() - 1], 3600),
        _ => (s, 1),
    };
    let n: u64 = num
        .parse()
        .with_context(|| format!("{s}: use a time such as 600, 30m or 2h"))?;
    n.checked_mul(mul)
        .with_context(|| format!("{s} is too long"))
}
