//! Terminal replies on the input side of the relay.
//!
//! A harness asks its terminal for the cursor position, its colors, its
//! modes or its version when it starts. The terminal (in a detached run,
//! the tmux server of the pane) writes the answer to the input, the same
//! way as a key. An answer is not a user key: it must not end the start
//! screen watcher or the training gate.
//!
//! A cursor position report can look like Shift+F3 (`ESC [ 1 ; 2 R`). A
//! wrong guess there only keeps the watcher on, and a run that a user
//! watches gets only the `always` rules.

/// True when `data` holds one or more whole terminal replies and nothing
/// else.
pub fn only_replies(data: &[u8]) -> bool {
    let mut rest = data;
    while !rest.is_empty() {
        match reply_len(rest) {
            Some(n) => rest = &rest[n..],
            None => return false,
        }
    }
    !data.is_empty()
}

/// The length of the reply at the start of `d`.
fn reply_len(d: &[u8]) -> Option<usize> {
    if d.first() != Some(&0x1b) {
        return None;
    }
    match d.get(1)? {
        b'[' => csi(d),
        b']' | b'P' | b'_' => string(d),
        _ => None,
    }
}

/// OSC (ends with BEL or ST), DCS and APC (end with ST).
fn string(d: &[u8]) -> Option<usize> {
    let osc = d[1] == b']';
    let mut i = 2;
    while i < d.len() {
        match d[i] {
            0x07 if osc => return Some(i + 1),
            0x1b if d.get(i + 1) == Some(&b'\\') => return Some(i + 2),
            0x1b => return None,
            _ => i += 1,
        }
    }
    None
}

/// The CSI forms that only a terminal sends.
fn csi(d: &[u8]) -> Option<usize> {
    let mut i = 2;
    let prefix = d
        .get(i)
        .copied()
        .filter(|c| matches!(c, b'?' | b'>' | b'='));
    if prefix.is_some() {
        i += 1;
    }
    let start = i;
    while d
        .get(i)
        .is_some_and(|c| c.is_ascii_digit() || matches!(c, b';' | b':'))
    {
        i += 1;
    }
    let params = &d[start..i];
    let inter = d.get(i).copied().filter(|c| (0x20..=0x2f).contains(c));
    if inter.is_some() {
        i += 1;
    }
    let ok = match (prefix, inter, *d.get(i)?) {
        // The cursor position.
        (None | Some(b'?'), None, b'R') => params.contains(&b';'),
        // The device attributes, first and second.
        (Some(b'?' | b'>'), None, b'c') => true,
        // The device status, the window size.
        (None, None, b'n' | b't') => !params.is_empty(),
        // A mode report.
        (_, Some(b'$'), b'y') => true,
        // The keyboard flags of the kitty protocol.
        (Some(b'?'), None, b'u') => true,
        // Focus in and out.
        (None, None, b'I' | b'O') => params.is_empty(),
        _ => false,
    };
    ok.then_some(i + 1)
}

#[cfg(test)]
mod tests {
    use super::only_replies;

    #[test]
    fn terminal_replies_are_not_keys() {
        for r in [
            &b"\x1b[12;1R"[..],
            b"\x1b[?64;1;2c",
            b"\x1b[>84;0;0c",
            b"\x1b[0n",
            b"\x1b[?2026;2$y",
            b"\x1b[?0u",
            b"\x1b]11;rgb:1e1e/1e1e/2e2e\x1b\\",
            b"\x1b]10;rgb:ffff/ffff/ffff\x07",
            b"\x1bP>|tmux 3.5a\x1b\\",
            b"\x1b[I",
            b"\x1b[O",
            // Several in one read.
            b"\x1b[?0u\x1b[5;1R\x1b[?62;22c",
        ] {
            assert!(only_replies(r), "{:?}", String::from_utf8_lossy(r));
        }
        for k in [
            &b""[..],
            b"3",
            b"\r",
            b"\x1b",
            b"\x1b[A",
            b"\x1b[1;5A",
            b"\x1b[15~",
            b"\x1b[13u",
            b"\x1b[5;1Rx",
            b"x\x1b[5;1R",
            b"\x1b]11;rgb:0/0/0",
        ] {
            assert!(!only_replies(k), "{:?}", String::from_utf8_lossy(k));
        }
    }
}
