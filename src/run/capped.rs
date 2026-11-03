//! The captured output of a sandbox, cut to its end.

use std::io::Read;

/// The captured output of a sandbox. Everything is read, so the sandbox
/// never blocks on a full pipe, but only the last 4 MiB are kept.
pub(super) fn read_capped(mut r: impl Read) -> String {
    const KEEP: usize = 4 << 20;
    let mut kept: Vec<u8> = Vec::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        match r.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                kept.extend_from_slice(&buf[..n]);
                if kept.len() > 2 * KEEP {
                    kept.drain(..kept.len() - KEEP);
                }
            }
        }
    }
    if kept.len() > KEEP {
        kept.drain(..kept.len() - KEEP);
    }
    String::from_utf8_lossy(&kept).into_owned()
}
