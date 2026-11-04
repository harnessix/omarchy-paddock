//! The zstd container of dsh session files: independent, checksummed
//! Zstandard frames, one for the header line and one per durable batch.
//! The frame scan follows `scanZstdFrames` in dsh's
//! `packages/session/session-persistence-jsonl/src/zstd.ts`.

use anyhow::{bail, Context as _, Result};

const MAGIC: u32 = 0xFD2F_B528;

/// The complete frames of a file, and whether an incomplete frame follows
/// them (a torn tail after a crash or during a write).
#[derive(Debug, PartialEq, Eq)]
pub struct Scan {
    pub frames: Vec<(usize, usize)>,
    pub torn: bool,
}

/// Find the complete frames without decoding them. A bad structure is an
/// error, an end of file inside the last frame is a torn tail.
pub fn scan(buf: &[u8]) -> Result<Scan> {
    let mut frames = Vec::new();
    let mut off = 0usize;
    let torn = |frames| Ok(Scan { frames, torn: true });
    while off < buf.len() {
        let start = off;
        if buf.len() - off < 4 {
            return torn(frames);
        }
        let magic = u32::from_le_bytes(buf[off..off + 4].try_into().expect("4 bytes"));
        if magic != MAGIC {
            bail!("corrupt dsh session file: no zstd frame at byte {off}");
        }
        off += 4;
        if off == buf.len() {
            return torn(frames);
        }
        let d = buf[off];
        off += 1;
        if d & 0x18 != 0 {
            bail!(
                "corrupt dsh session file: reserved frame header bit at byte {}",
                off - 1
            );
        }
        let fcs_flag = d >> 6;
        let single = d & 0x20 != 0;
        let checksum = d & 0x04 != 0;
        let dict = match d & 0x03 {
            3 => 4,
            n => n as usize,
        };
        let fcs = if fcs_flag == 0 {
            usize::from(single)
        } else {
            1 << fcs_flag
        };
        let rest = usize::from(!single) + dict + fcs;
        if buf.len() - off < rest {
            return torn(frames);
        }
        off += rest;
        loop {
            if buf.len() - off < 3 {
                return torn(frames);
            }
            let bh =
                u32::from(buf[off]) | u32::from(buf[off + 1]) << 8 | u32::from(buf[off + 2]) << 16;
            off += 3;
            let last = bh & 1 != 0;
            let kind = (bh >> 1) & 3;
            let size = (bh >> 3) as usize;
            if kind == 3 {
                bail!(
                    "corrupt dsh session file: reserved block type at byte {}",
                    off - 3
                );
            }
            let payload = if kind == 1 { 1 } else { size };
            if buf.len() - off < payload {
                return torn(frames);
            }
            off += payload;
            if last {
                break;
            }
        }
        if checksum {
            if buf.len() - off < 4 {
                return torn(frames);
            }
            off += 4;
        }
        frames.push((start, off));
    }
    Ok(Scan {
        frames,
        torn: false,
    })
}

/// Decode the complete frames, and check their checksums. Returns the
/// plain text and whether a torn tail was left out.
pub fn decode(buf: &[u8]) -> Result<(Vec<u8>, bool)> {
    let s = scan(buf)?;
    let mut out = Vec::new();
    for (a, b) in &s.frames {
        let plain = zstd::stream::decode_all(&buf[*a..*b])
            .with_context(|| format!("corrupt zstd frame at byte {a} of a dsh session file"))?;
        out.extend_from_slice(&plain);
    }
    Ok((out, s.torn))
}

/// One checksummed frame, as dsh writes it (`compressZstdFrame`).
pub fn encode(plain: &[u8]) -> Result<Vec<u8>> {
    let mut c = zstd::bulk::Compressor::new(3)?;
    c.include_checksum(true)?;
    Ok(c.compress(plain)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_and_a_torn_tail_is_left_out() {
        let mut file = encode(b"header\n").unwrap();
        let second = encode(b"row 0\nrow 1\n").unwrap();
        file.extend_from_slice(&second);
        let (plain, torn) = decode(&file).unwrap();
        assert_eq!(plain, b"header\nrow 0\nrow 1\n");
        assert!(!torn);
        // The checksum flag is set, as dsh sets it.
        assert_ne!(file[4] & 0x04, 0);
        for cut in 1..second.len() {
            let short = &file[..file.len() - cut];
            let (plain, torn) = decode(short).unwrap();
            assert_eq!(plain, b"header\n", "cut {cut}");
            assert!(torn, "cut {cut}");
        }
    }

    #[test]
    fn a_bad_checksum_or_magic_is_an_error() {
        let mut f = encode(b"header\n").unwrap();
        let n = f.len();
        f[n - 1] ^= 0xff;
        assert!(decode(&f).is_err());
        assert!(decode(b"not zstd at all").is_err());
    }
}
