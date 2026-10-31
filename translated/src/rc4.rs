//! Decrypts protected `strs` entries with the `keySeed` the server sends.
//!
//! Verified against the game. Entry header:
//!
//! ```text
//! u16 size       // entry bytes, including the 6 header bytes
//! u16 base_char  // 0 with range_bits 16 = plain UTF-16LE
//! u8  range_bits // bits per symbol
//! u8  -
//! u8  payload[size - 6]
//! ```
//!
//! Pipeline: `keySeed` (8 bytes) → SHA-1-like mixing → 20-byte RC4
//! key → RC4 → unpack `range_bits`-bit symbols.

pub const HEADER: usize = 6;

/// Symbols 1..31 (table at `0x1420f3100` in the exe).
const TABLE: &[u8; 31] = b"0123456strnum()[]<>%#/:-'\" ,.!\n";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub size: u16,
    pub base_char: u16,
    pub range_bits: u8,
}

impl Header {
    pub fn parse(entry: &[u8]) -> Option<Self> {
        if entry.len() < HEADER {
            return None;
        }
        let h = Self {
            size: u16::from_le_bytes([entry[0], entry[1]]),
            base_char: u16::from_le_bytes([entry[2], entry[3]]),
            range_bits: entry[4],
        };
        (h.size as usize >= HEADER && h.size as usize <= entry.len()).then_some(h)
    }

    /// Only the 6 header bytes, without checking the size against the data.
    pub fn peek(h: &[u8; HEADER]) -> Self {
        Self {
            size: u16::from_le_bytes([h[0], h[1]]),
            base_char: u16::from_le_bytes([h[2], h[3]]),
            range_bits: h[4],
        }
    }

    /// Text that can only be read with the server's `keySeed`.
    pub fn is_protected(&self) -> bool {
        !(self.base_char == 0 && self.range_bits == 16)
    }
}

/// 20-byte RC4 key derived from the `keySeed` (`sub_140D9F630`).
fn expand_key(seed: u64) -> [u8; 20] {
    let kb = seed.to_le_bytes();
    let mut b = [0u8; 20];
    for (i, x) in b.iter_mut().enumerate() {
        *x = kb[i % 8];
    }
    let w = |i: usize| u32::from_le_bytes(b[i * 4..i * 4 + 4].try_into().unwrap());
    let (a, bb, c, d, e) = (w(0), w(1), w(2), w(3), w(4));
    let t = a.wrapping_sub(1615554381);
    let v15 = t.rotate_left(5).wrapping_add(bb).wrapping_add(1722862861);
    let v16 = t.rotate_left(30);
    let v17 = (!(t & 0x2222_2222) & 0x7BF3_6AE2)
        .wrapping_sub(214083945)
        .wrapping_add(c)
        .wrapping_add(v15.rotate_left(5));
    let v18 = (v15 & (v16 ^ 0x59D1_48C0)) ^ 0x59D1_48C0;
    let v19 = v15.rotate_left(30);
    let v20 = v18
        .wrapping_sub(696916869)
        .wrapping_add(d)
        .wrapping_add(v17.rotate_left(5));
    let out = [
        a.wrapping_add(v20.rotate_left(5))
            .wrapping_add(e)
            .wrapping_add(v16 ^ (v17 & (v19 ^ v16)))
            .wrapping_sub(1269579175),
        bb.wrapping_add(v20),
        c.wrapping_add(v17.rotate_left(30)),
        d.wrapping_add(v19),
        e.wrapping_add(v16),
    ];
    let mut k = [0u8; 20];
    for (i, v) in out.iter().enumerate() {
        k[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
    }
    k
}

fn rc4(key: &[u8], data: &mut [u8]) {
    let mut s: [u8; 256] = std::array::from_fn(|i| i as u8);
    let mut j = 0u8;
    for i in 0..256 {
        j = j.wrapping_add(s[i]).wrapping_add(key[i % key.len()]);
        s.swap(i, j as usize);
    }
    let (mut i, mut j) = (0u8, 0u8);
    for b in data {
        i = i.wrapping_add(1);
        j = j.wrapping_add(s[i as usize]);
        s.swap(i as usize, j as usize);
        *b ^= s[s[i as usize].wrapping_add(s[j as usize]) as usize];
    }
}

/// Unpacks `range_bits`-bit symbols (`sub_1410CFC50`). Symbol 0 terminates.
fn unpack(payload: &[u8], base_char: u16, range_bits: u8) -> Option<String> {
    let bits = range_bits as u32;
    if bits == 0 || bits > 24 {
        return None;
    }
    let (mut acc, mut n, mut p) = (0u64, 0u32, 0usize);
    let mut out = String::new();
    for _ in 0..(8 * payload.len() as u32 / bits + 1) {
        while n <= 24 {
            if p < payload.len() {
                acc |= (payload[p] as u64) << n;
                p += 1;
            }
            n += 8;
        }
        let s = (acc & ((1 << bits) - 1)) as u32;
        acc >>= bits;
        n -= bits;
        match s {
            0 => break,
            1..0x20 => out.push(TABLE[s as usize - 1] as char),
            _ => out.push(char::from_u32(s + base_char as u32 - 32).unwrap_or('\u{FFFD}')),
        }
    }
    Some(out)
}

/// Text of a complete `strs` entry. Plain entries don't need a seed.
pub fn decode(entry: &[u8], seed: Option<u64>) -> Option<String> {
    let h = Header::parse(entry)?;
    let payload = &entry[HEADER..h.size as usize];
    if !h.is_protected() {
        let u: Vec<u16> = payload
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        return Some(String::from_utf16_lossy(&u).trim_end_matches('\0').to_owned());
    }
    let mut buf = payload.to_vec();
    if let Some(seed) = seed {
        if h.base_char != 0 && seed == 0 {
            return None; // the game shows "[null]"
        }
        rc4(&expand_key(seed), &mut buf);
    }
    unpack(&buf, h.base_char, h.range_bits)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// Real entries from `Gw2.dat` with keys captured from the game.
    #[test]
    fn decrypts_known_entries() {
        let cases = [
            (
                "180042000700d2a6247de2f9cf3bb5e66a650413128c05e1",
                0x00001adfb96a5a5f,
                "Second Birthday Gift",
            ),
            (
                "17004300070046a2fc15d32af9d0a59a9e4481080ebc30",
                0x1f2333d8237f,
                "The Cost of Victory",
            ),
            (
                "1700420007004edfa4cc79515d53e7dd969dec2c9e1d89",
                0x16c61dae416d,
                "Birthday Booster[s]",
            ),
            (
                "3a00420007007d62f7515858184624efa01272d89f209e38fc041223cf405489218bab2fd067db68dd7fc880afa9f99c8c09872cabcd69aeda48",
                0x1e3a7cc3f0a6,
                "Bind %num2% unique legendary Perfected Envoy armor skin[s].",
            ),
        ];
        for (e, seed, want) in cases {
            assert_eq!(decode(&hex(e), Some(seed)).as_deref(), Some(want));
        }
    }

    #[test]
    fn plain_entries_and_bad_input() {
        let mut e = vec![0, 0, 0, 0, 16, 0];
        e.extend("Oi".encode_utf16().flat_map(|u| u.to_le_bytes()));
        e[0] = e.len() as u8;
        let h = Header::parse(&e).unwrap();
        assert!(!h.is_protected());
        assert_eq!(decode(&e, None).as_deref(), Some("Oi"));
        assert!(decode(&[1, 2, 3], None).is_none());
        let zero_seed = hex("180042000700d2a6247de2f9cf3bb5e66a650413128c05e1");
        assert!(decode(&zero_seed, Some(0)).is_none());
    }
}
