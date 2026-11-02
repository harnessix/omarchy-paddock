//! Read the files of a Go module zip: stored and deflated entries, no zip64.
//! Only for the hash check of promote.rs, so it reads into memory with a
//! size limit.

use anyhow::{bail, Context, Result};

/// Go refuses module zips larger than this when unpacked.
pub const MAX_UNPACKED: usize = 500 << 20;

fn u16le(d: &[u8], at: usize) -> Result<usize> {
    let b = d.get(at..at + 2).context("zip: short")?;
    Ok(u16::from_le_bytes([b[0], b[1]]) as usize)
}

fn u32le(d: &[u8], at: usize) -> Result<usize> {
    let b = d.get(at..at + 4).context("zip: short")?;
    Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize)
}

fn u32raw(d: &[u8], at: usize) -> Result<u32> {
    let b = d.get(at..at + 4).context("zip: short")?;
    Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

const CRC_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut j = 0;
        while j < 8 {
            if c & 1 != 0 {
                c = 0xedb8_8320 ^ (c >> 1);
            } else {
                c >>= 1;
            }
            j += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
};

/// CRC-32 (IEEE 802.3), as used in zip files.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &b in data {
        let idx = ((crc ^ (b as u32)) & 0xff) as usize;
        crc = CRC_TABLE[idx] ^ (crc >> 8);
    }
    !crc
}

/// The (name, content) of each entry, in the order of the central directory.
pub fn files(d: &[u8]) -> Result<Vec<(String, Vec<u8>)>> {
    let min = d.len().saturating_sub(22 + 65535);
    let eocd = (min..d.len().saturating_sub(21))
        .rev()
        .find(|&i| d[i..].starts_with(&[0x50, 0x4b, 0x05, 0x06]))
        .context("zip: no end record")?;
    let count = u16le(d, eocd + 10)?;
    let mut at = u32le(d, eocd + 16)?;
    let mut out = Vec::with_capacity(count);
    let mut total = 0usize;
    for _ in 0..count {
        if u32le(d, at)? != 0x0201_4b50 {
            bail!("zip: bad central record");
        }
        let method = u16le(d, at + 10)?;
        let crc = u32raw(d, at + 16)?;
        let csize = u32le(d, at + 20)?;
        let usize_ = u32le(d, at + 24)?;
        let (nlen, xlen, clen) = (u16le(d, at + 28)?, u16le(d, at + 30)?, u16le(d, at + 32)?);
        let local = u32le(d, at + 42)?;
        let name = d.get(at + 46..at + 46 + nlen).context("zip: short")?;
        let name = String::from_utf8(name.to_vec()).context("zip: a name is not UTF-8")?;
        at += 46 + nlen + xlen + clen;
        if csize == 0xffff_ffff || usize_ == 0xffff_ffff {
            bail!("zip: zip64 is not supported");
        }
        total += usize_;
        if total > MAX_UNPACKED {
            bail!("zip: larger than {MAX_UNPACKED} bytes unpacked");
        }
        if u32le(d, local)? != 0x0403_4b50 {
            bail!("zip: bad local record");
        }
        let start = local + 30 + u16le(d, local + 26)? + u16le(d, local + 28)?;
        let raw = d.get(start..start + csize).context("zip: short data")?;
        let data = match method {
            0 => raw.to_vec(),
            8 => inflate(raw, usize_)?,
            m => bail!("zip: method {m} is not supported"),
        };
        if data.len() != usize_ {
            bail!("zip: {name} has the wrong size");
        }
        if crc32(&data) != crc {
            bail!("zip: {name} has a bad CRC-32");
        }
        out.push((name, data));
    }
    Ok(out)
}

struct Bits<'a> {
    d: &'a [u8],
    pos: usize,
    buf: u32,
    cnt: u32,
}

impl Bits<'_> {
    fn bits(&mut self, n: u32) -> Result<u32> {
        let mut v = self.buf;
        while self.cnt < n {
            let b = *self.d.get(self.pos).context("deflate: short")? as u32;
            self.pos += 1;
            v |= b << self.cnt;
            self.cnt += 8;
        }
        self.buf = if n == 32 { 0 } else { v >> n };
        self.cnt -= n;
        Ok(v & ((1u32 << n) - 1))
    }
}

struct Huff {
    count: [u16; 16],
    symbol: Vec<u16>,
}

impl Huff {
    fn new(lengths: &[u8]) -> Result<Self> {
        let mut count = [0u16; 16];
        for &l in lengths {
            count[l as usize] += 1;
        }
        let mut offs = [0u16; 16];
        for l in 1..15 {
            offs[l + 1] = offs[l] + count[l];
        }
        let mut symbol = vec![0u16; lengths.len()];
        for (s, &l) in lengths.iter().enumerate() {
            if l != 0 {
                symbol[offs[l as usize] as usize] = s as u16;
                offs[l as usize] += 1;
            }
        }
        Ok(Self { count, symbol })
    }

    fn decode(&self, b: &mut Bits) -> Result<u16> {
        let (mut code, mut first, mut index) = (0i32, 0i32, 0i32);
        for len in 1..16 {
            code |= b.bits(1)? as i32;
            let count = self.count[len] as i32;
            if code - count < first {
                return self
                    .symbol
                    .get((index + (code - first)) as usize)
                    .copied()
                    .context("deflate: bad code");
            }
            index += count;
            first += count;
            first <<= 1;
            code <<= 1;
        }
        bail!("deflate: bad code")
    }
}

const LBASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LEXT: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DBASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DEXT: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

/// Raw deflate (RFC 1951). Errors when the output passes `max` bytes.
pub fn inflate(d: &[u8], max: usize) -> Result<Vec<u8>> {
    let mut b = Bits {
        d,
        pos: 0,
        buf: 0,
        cnt: 0,
    };
    let mut out = Vec::new();
    loop {
        let last = b.bits(1)?;
        match b.bits(2)? {
            0 => {
                b.buf = 0;
                b.cnt = 0;
                let len = u16le(d, b.pos)?;
                let nlen = u16le(d, b.pos + 2)?;
                if len != !nlen & 0xffff {
                    bail!("deflate: bad stored block");
                }
                let s = d
                    .get(b.pos + 4..b.pos + 4 + len)
                    .context("deflate: short")?;
                out.extend_from_slice(s);
                b.pos += 4 + len;
            }
            1 => {
                let mut l = [0u8; 288];
                l[..144].fill(8);
                l[144..256].fill(9);
                l[256..280].fill(7);
                l[280..].fill(8);
                codes(
                    &mut b,
                    &mut out,
                    &Huff::new(&l)?,
                    &Huff::new(&[5; 30])?,
                    max,
                )?;
            }
            2 => {
                let (lc, dc) = dynamic(&mut b)?;
                codes(&mut b, &mut out, &lc, &dc, max)?;
            }
            _ => bail!("deflate: bad block type"),
        }
        if out.len() > max {
            bail!("deflate: too large");
        }
        if last == 1 {
            return Ok(out);
        }
    }
}

fn dynamic(b: &mut Bits) -> Result<(Huff, Huff)> {
    const ORDER: [usize; 19] = [
        16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
    ];
    let nlen = b.bits(5)? as usize + 257;
    let ndist = b.bits(5)? as usize + 1;
    let ncode = b.bits(4)? as usize + 4;
    if nlen > 286 || ndist > 30 {
        bail!("deflate: bad counts");
    }
    let mut cl = [0u8; 19];
    for &o in ORDER.iter().take(ncode) {
        cl[o] = b.bits(3)? as u8;
    }
    let h = Huff::new(&cl)?;
    let mut lengths = Vec::with_capacity(nlen + ndist);
    while lengths.len() < nlen + ndist {
        let sym = h.decode(b)?;
        let (val, rep) = match sym {
            0..=15 => (sym as u8, 1),
            16 => (
                *lengths.last().context("deflate: repeat without a length")?,
                3 + b.bits(2)?,
            ),
            17 => (0, 3 + b.bits(3)?),
            _ => (0, 11 + b.bits(7)?),
        };
        for _ in 0..rep {
            lengths.push(val);
        }
    }
    if lengths.len() != nlen + ndist {
        bail!("deflate: too many lengths");
    }
    Ok((Huff::new(&lengths[..nlen])?, Huff::new(&lengths[nlen..])?))
}

fn codes(b: &mut Bits, out: &mut Vec<u8>, lc: &Huff, dc: &Huff, max: usize) -> Result<()> {
    loop {
        let sym = lc.decode(b)? as usize;
        if sym < 256 {
            out.push(sym as u8);
        } else if sym == 256 {
            return Ok(());
        } else {
            let i = sym - 257;
            if i >= 29 {
                bail!("deflate: bad length");
            }
            let len = LBASE[i] as usize + b.bits(LEXT[i] as u32)? as usize;
            let ds = dc.decode(b)? as usize;
            if ds >= 30 {
                bail!("deflate: bad distance");
            }
            let dist = DBASE[ds] as usize + b.bits(DEXT[ds] as u32)? as usize;
            if dist > out.len() {
                bail!("deflate: distance too far");
            }
            let from = out.len() - dist;
            for k in 0..len {
                out.push(out[from + k]);
            }
        }
        if out.len() > max {
            bail!("deflate: too large");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    /// Made with Python: `zlib.compressobj(9, zlib.DEFLATED, -15)` over the
    /// 300 lines `b"line %d of the paddock layered cache test, value %d\n" %
    /// (i, (i * 7919) % 1000)`. It is one dynamic Huffman block.
    #[test]
    fn inflate_reads_a_dynamic_block() {
        let d = include_bytes!("dynamic.deflate");
        let out = inflate(d, 1 << 20).unwrap();
        assert_eq!(out.len(), 16055);
        assert_eq!(
            crate::manifest::hex(&Sha256::digest(&out)),
            "e325f22cd8c07490bae60c334e7d9ee18a733fbb8403c3ab9d2a09c1c95b3015"
        );
        assert!(inflate(d, 1000).is_err());
        assert!(inflate(&d[..d.len() / 2], 1 << 20).is_err());
    }

    #[test]
    fn crc32_matches_standard_test_vector() {
        assert_eq!(crc32(b"123456789"), 0xcbf43926);
    }
}
