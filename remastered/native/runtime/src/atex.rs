//! ATEX/ATTX/ATEC/ATEP/ATEU/ATET texture decoding: ArenaNet block-level
//! compression -> raw BCn blocks of mip 0.
//! Faithful port of gw2dattools `inflateTextureFileBuffer.cpp` and
//! `huffmanTreeUtils` (GW2Viewer/dep/gw2dattools).

use anyhow::{bail, Result};
use std::sync::OnceLock;

const MAX_BITS: usize = 32;
const MAX_SYM: usize = 285;
const HASH_BITS: u32 = 8;

const FF_COLOR: u16 = 0x10;
const FF_ALPHA: u16 = 0x20;
const FF_DEDUCEDALPHACOMP: u16 = 0x40;
const FF_PLAINCOMP: u16 = 0x80;
const FF_BICOLORCOMP: u16 = 0x200;

const CF_WHITE: u32 = 1;
const CF_ALPHA4: u32 = 2;
const CF_ALPHA8: u32 = 4;
const CF_PLAIN: u32 = 8;

struct Tree {
    comp: [u32; MAX_BITS],
    off: [u16; MAX_BITS],
    sym: [u16; MAX_SYM],
    bits: [u8; MAX_BITS],
    hsym: [i16; 1 << HASH_BITS],
    hbits: [u8; 1 << HASH_BITS],
}

fn build_tree(wb: &[i16; MAX_BITS], wc: &[i16; MAX_SYM]) -> Tree {
    let mut t = Tree {
        comp: [0; MAX_BITS],
        off: [0; MAX_BITS],
        sym: [0; MAX_SYM],
        bits: [0; MAX_BITS],
        hsym: [-1; 1 << HASH_BITS],
        hbits: [0; 1 << HASH_BITS],
    };
    let mut code: u32 = 0;
    let mut nb: u32 = 0;
    while nb <= HASH_BITS {
        let mut s = wb[nb as usize];
        while s != -1 {
            let mut h = (code << (HASH_BITS - nb)) as u16;
            let nh = ((code.wrapping_add(1)) << (HASH_BITS - nb)) as u16;
            while h < nh {
                t.hsym[h as usize] = s;
                t.hbits[h as usize] = nb as u8;
                h += 1;
            }
            s = wc[s as usize];
            code = code.wrapping_sub(1);
        }
        code = (code << 1).wrapping_add(1);
        nb += 1;
    }
    let (mut ci, mut so) = (0usize, 0u16);
    while (nb as usize) < MAX_BITS {
        if wb[nb as usize] != -1 {
            let mut s = wb[nb as usize];
            while s != -1 {
                t.sym[so as usize] = s as u16;
                so += 1;
                s = wc[s as usize];
                code = code.wrapping_sub(1);
            }
            t.comp[ci] = code.wrapping_add(1) << (32 - nb);
            t.bits[ci] = nb as u8;
            t.off[ci] = so - 1;
            ci += 1;
        }
        code = (code << 1).wrapping_add(1);
        nb += 1;
    }
    t
}

fn dict() -> &'static Tree {
    static T: OnceLock<Tree> = OnceLock::new();
    T.get_or_init(|| {
        let mut wb = [-1i16; MAX_BITS];
        let mut wc = [-1i16; MAX_SYM];
        let mut add = |bits: usize, sym: i16| {
            if wb[bits] == -1 {
                wb[bits] = sym;
            } else {
                wc[sym as usize] = wb[bits];
                wb[bits] = sym;
            }
        };
        add(1, 0x01);
        add(2, 0x12);
        for s in (0x02..=0x11).rev() {
            add(6, s);
        }
        build_tree(&wb, &wc)
    })
}

struct State<'a> {
    input: &'a [u8],
    size: u32, // in words
    pos: u32,
    head: u32,
    buffer: u32,
    bits: u32,
    empty: bool,
}

impl State<'_> {
    fn word(&self, i: u32) -> u32 {
        let o = i as usize * 4;
        u32::from_le_bytes(self.input[o..o + 4].try_into().unwrap())
    }
    fn pull(&mut self) -> Result<()> {
        if (self.pos + 1) % 0x4000 == 0 {
            self.pos += 1;
        }
        let v = if self.pos >= self.size {
            if self.empty {
                bail!("atex: end of input");
            }
            self.empty = true;
            0
        } else {
            self.word(self.pos)
        };
        if self.bits == 0 {
            self.head = v;
            self.buffer = 0;
        } else {
            self.head |= v >> self.bits;
            self.buffer = v << (32 - self.bits);
        }
        self.bits += 32;
        self.pos += 1;
        Ok(())
    }
    fn need(&mut self, n: u32) -> Result<()> {
        if self.bits < n { self.pull() } else { Ok(()) }
    }
    fn drop(&mut self, n: u32) -> Result<()> {
        if n > self.bits {
            bail!("atex: drop past end");
        }
        if n == 0 {
            return Ok(());
        }
        if n == 32 {
            self.head = self.buffer;
            self.buffer = 0;
        } else {
            self.head = (self.head << n) | (self.buffer >> (32 - n));
            self.buffer <<= n;
        }
        self.bits -= n;
        Ok(())
    }
    fn read(&self, n: u32) -> u32 {
        if n == 0 { 0 } else { self.head >> (32 - n) }
    }
    fn code(&mut self, t: &Tree) -> Result<u16> {
        self.need(32)?;
        let h = self.read(HASH_BITS) as usize;
        if t.hsym[h] != -1 {
            let c = t.hsym[h] as u16;
            self.drop(t.hbits[h] as u32)?;
            Ok(c)
        } else {
            let mut i = 0usize;
            while i < MAX_BITS && self.read(32) < t.comp[i] {
                i += 1;
            }
            if i >= MAX_BITS || t.bits[i] == 0 {
                bail!("atex: bad code");
            }
            let nb = t.bits[i] as u32;
            let idx = t.off[i] as i64 - ((self.read(32).wrapping_sub(t.comp[i])) >> (32 - nb)) as i64;
            if idx < 0 || idx as usize >= MAX_SYM {
                bail!("atex: bad symbol index");
            }
            let c = t.sym[idx as usize];
            self.drop(nb)?;
            Ok(c)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bc {
    Bc1,
    Bc2,
    Bc3,
    Bc4,
    Bc5,
    Bc7,
}

/// (flags, bits per pixel, output BC format) for an ANet format FourCC.
pub fn format_info(fcc: &[u8]) -> Option<(u16, u32, Bc)> {
    let dxt2 = (FF_COLOR | FF_ALPHA | FF_PLAINCOMP, 8);
    Some(match fcc {
        b"DXT1" => (FF_COLOR | FF_ALPHA | FF_DEDUCEDALPHACOMP, 4, Bc::Bc1),
        b"DXT2" | b"DXT3" => (dxt2.0, dxt2.1, Bc::Bc2),
        b"DXT4" | b"DXT5" => (dxt2.0, dxt2.1, Bc::Bc3),
        b"BC7X" => (dxt2.0, dxt2.1, Bc::Bc7),
        b"DXTA" => (FF_ALPHA | FF_PLAINCOMP, 4, Bc::Bc4),
        b"DXTL" => (FF_COLOR | FF_ALPHA | FF_PLAINCOMP, 8, Bc::Bc3),
        b"DXTN" => (FF_BICOLORCOMP, 8, Bc::Bc3),
        b"3DCX" => (FF_BICOLORCOMP, 8, Bc::Bc5),
        _ => return None,
    })
}

pub struct Decoded {
    pub fourcc: [u8; 4],
    pub width: u32,
    pub height: u32,
    pub bc: Bc,
    pub blocks: Vec<u8>,
}

/// Decodes mip 0 of an ATEX-family file (already dat-decompressed).
pub fn decode(file: &[u8]) -> Result<Decoded> {
    if file.len() < 16 {
        bail!("atex: too short");
    }
    let fourcc: [u8; 4] = file[4..8].try_into().unwrap();
    let mut width = u16::from_le_bytes([file[8], file[9]]) as u32;
    let height = u16::from_le_bytes([file[10], file[11]]) as u32;
    if width == 126 && height == 64 {
        width = 128; // same quirk as Gw2Browser
    }
    let Some((flags, bpp, bc)) = format_info(&fourcc) else {
        bail!("atex: unknown format {}", String::from_utf8_lossy(&fourcc));
    };
    let nblocks = ((width + 3) / 4) * ((height + 3) / 4);
    let bpb = bpp * 16 / 8;
    let two = (flags & (FF_PLAINCOMP | FF_COLOR | FF_ALPHA)) == (FF_PLAINCOMP | FF_COLOR | FF_ALPHA)
        || (flags & FF_BICOLORCOMP) != 0;
    let bpc = bpb / if two { 2 } else { 1 };
    let mut out = vec![0u8; (bpb * nblocks) as usize];

    let mut st = State { input: file, size: (file.len() / 4) as u32, pos: 3, head: 0, buffer: 0, bits: 0, empty: false };
    let t = dict();
    st.need(32)?;
    let _data_size = st.read(32);
    st.drop(32)?;
    st.need(32)?;
    let cflags = st.read(32);
    st.drop(32)?;

    let n = nblocks as usize;
    let mut cmap = vec![false; n];
    let mut amap = vec![false; n];
    let blk = |i: usize| bpb as usize * i;

    if cflags & CF_WHITE != 0 {
        let mut p = 0usize;
        while p < n {
            let mut c = st.code(t)?;
            st.need(1)?;
            let v = st.read(1);
            st.drop(1)?;
            while c > 0 {
                if p >= n {
                    bail!("atex: white overrun");
                }
                if !cmap[p] {
                    if v != 0 {
                        out[blk(p)..blk(p) + 8].copy_from_slice(&0xFFFF_FFFF_FFFF_FFFEu64.to_le_bytes());
                        amap[p] = true;
                        cmap[p] = true;
                    }
                    c -= 1;
                }
                p += 1;
            }
            while p < n && cmap[p] {
                p += 1;
            }
        }
    }

    for (flag, nbits) in [(CF_ALPHA4, 4u32), (CF_ALPHA8, 8u32)] {
        if cflags & flag == 0 {
            continue;
        }
        st.need(nbits)?;
        let b = st.read(nbits) as u64;
        st.drop(nbits)?;
        let alpha: u64 = if nbits == 4 {
            let x = b | (b << 4);
            let x = x | (x << 8);
            let x = x | (x << 16);
            x | (x << 32)
        } else {
            b | (b << 8)
        };
        let mut p = 0usize;
        while p < n {
            let mut c = st.code(t)?;
            st.need(2)?;
            let v = st.read(1);
            st.drop(1)?;
            let not_null = st.read(1);
            if v != 0 {
                st.drop(1)?;
            }
            while c > 0 {
                if p >= n {
                    bail!("atex: alpha overrun");
                }
                if !amap[p] {
                    if v != 0 {
                        let val = if not_null != 0 { alpha } else { 0 };
                        out[blk(p)..blk(p) + bpc as usize].copy_from_slice(&val.to_le_bytes()[..bpc as usize]);
                        amap[p] = true;
                    }
                    c -= 1;
                }
                p += 1;
            }
            while p < n && amap[p] {
                p += 1;
            }
        }
    }

    if cflags & CF_PLAIN != 0 {
        let fin = plain_color(&mut st, flags)?;
        let mut p = 0usize;
        while p < n {
            let mut c = st.code(t)?;
            st.need(1)?;
            let v = st.read(1);
            st.drop(1)?;
            while c > 0 {
                if p >= n {
                    bail!("atex: color overrun");
                }
                if !cmap[p] {
                    if v != 0 {
                        let o = blk(p) + if two { bpc as usize } else { 0 };
                        out[o..o + bpc as usize].copy_from_slice(&fin.to_le_bytes()[..bpc as usize]);
                        cmap[p] = true;
                    }
                    c -= 1;
                }
                p += 1;
            }
            while p < n && cmap[p] {
                p += 1;
            }
        }
    }

    if st.bits >= 32 {
        st.pos -= 1;
    }
    let put = |out: &mut [u8], o: usize, w: u32| out[o..o + 4].copy_from_slice(&w.to_le_bytes());
    if ((flags & FF_ALPHA) != 0 && (flags & FF_DEDUCEDALPHACOMP) == 0) || (flags & FF_BICOLORCOMP) != 0 {
        for i in 0..n {
            if st.pos >= st.size {
                break;
            }
            if !amap[i] {
                put(&mut out, blk(i), st.word(st.pos));
                st.pos += 1;
                if bpc > 4 && st.pos < st.size {
                    put(&mut out, blk(i) + 4, st.word(st.pos));
                    st.pos += 1;
                }
            }
        }
    }
    if (flags & FF_COLOR) != 0 || (flags & FF_BICOLORCOMP) != 0 {
        let base = if two { bpc as usize } else { 0 };
        for i in 0..n {
            if st.pos >= st.size {
                break;
            }
            if !cmap[i] {
                put(&mut out, blk(i) + base, st.word(st.pos));
                st.pos += 1;
            }
        }
        if bpc > 4 {
            for i in 0..n {
                if st.pos >= st.size {
                    break;
                }
                if !cmap[i] {
                    put(&mut out, blk(i) + base + 4, st.word(st.pos));
                    st.pos += 1;
                }
            }
        }
    }

    Ok(Decoded { fourcc, width, height, bc, blocks: out })
}

fn plain_color(st: &mut State, flags: u16) -> Result<u64> {
    st.need(24)?;
    let blue = st.read(8);
    st.drop(8)?;
    let green = st.read(8);
    st.drop(8)?;
    let red = st.read(8);
    st.drop(8)?;
    // u8 truncation exactly as the C++ (uint8 temporaries).
    let r1 = ((red - (red >> 5)) >> 3) as u8 as u32;
    let b1 = ((blue - (blue >> 5)) >> 3) as u8 as u32;
    let g1 = (green - (green >> 6)) >> 2;
    let r2 = ((r1 << 3) + (r1 >> 2)) as u8 as u32;
    let b2 = ((b1 << 3) + (b1 >> 2)) as u8 as u32;
    let g2 = ((g1 << 2) + (g1 >> 4)) & 0xFFFF;
    let comp = |v: u32, t2: u32, t1: u32, mask: u32| -> u32 {
        (12u32.wrapping_mul(v.wrapping_sub(t2))) / (8 - if (t1 & mask) == mask { 1 } else { 0 })
    };
    let cr = comp(red, r2, r1, 0x11);
    let cb = comp(blue, b2, b1, 0x11);
    let cg = comp(green, g2, g1, 0x1111);
    let pick = |c: u32, t: u32| -> (u32, u32) {
        if c < 2 { (t, t) } else if c < 6 { (t, t + 1) } else if c < 10 { (t + 1, t) } else { (t + 1, t + 1) }
    };
    let (vr1, vr2) = pick(cr, r1);
    let (vb1, vb2) = pick(cb, b1);
    let (vg1, vg2) = pick(cg, g1);
    let mut c1 = vr1 | ((vg1 | (vb1 << 6)) << 5);
    let mut c2 = vr2 | ((vg2 | (vb2 << 6)) << 5);
    let (mut t1, mut t2) = (0u32, 0u32);
    for (a, b, base, cc) in [(vr1, vr2, r1, cr), (vb1, vb2, b1, cb), (vg1, vg2, g1, cg)] {
        if a != b {
            t1 += if a == base { cc } else { 12u32.wrapping_sub(cc) };
            t2 += 1;
        }
    }
    if t2 > 0 {
        t1 = (t1 + t2 / 2) / t2;
    }
    let dxt1_special = (flags & FF_DEDUCEDALPHACOMP) != 0 && (t1 == 5 || t1 == 6 || t2 != 0);
    if t2 > 0 && !dxt1_special {
        if c2 == 0xFFFF {
            t1 = 12;
            c1 = c1.wrapping_sub(1);
        } else {
            t1 = 0;
            c2 += 1;
        }
    }
    if c2 >= c1 {
        std::mem::swap(&mut c1, &mut c2);
        t1 = 12u32.wrapping_sub(t1);
    }
    let chosen: u64 = if dxt1_special {
        2
    } else if t1 < 2 {
        0
    } else if t1 < 6 {
        2
    } else if t1 < 10 {
        3
    } else {
        1
    };
    let mut tv = chosen | (chosen << 2) | ((chosen | (chosen << 2)) << 4);
    tv |= tv << 8;
    tv |= tv << 16;
    Ok((c1 as u64) | ((c2 as u64) << 16) | (tv << 32))
}

/// DDS (BCn, any level count) -> ATEX-family file with uncompressed level chunks
/// (cflags = 0), so the engine decodes it on its native ATEX path instead of the
/// DDS path. Each level chunk: [u32 chunk size incl. this field][u32 cflags = 0]
/// then the raw words in the order `decode` consumes them:
///   alpha half (formats with explicit alpha / bicolor): per block, words 0 and 1;
///   colour half: all blocks' first colour word, then all blocks' second word.
/// `kind` / `fourcc`: the original file's magic and ANet format (e.g. 3DCX for an ATI2 DDS).
pub fn from_dds(dds: &[u8], kind: [u8; 4], fourcc: [u8; 4]) -> Option<Vec<u8>> {
    if dds.len() < 128 || &dds[0..4] != b"DDS " {
        return None;
    }
    let rd = |o: usize| u32::from_le_bytes(dds[o..o + 4].try_into().unwrap());
    let (h, w, mips) = (rd(12), rd(16), rd(28).max(1));
    if w == 0 || h == 0 || w > 0xFFFF || h > 0xFFFF {
        return None;
    }
    let (flags, bpp, _) = format_info(&fourcc)?;
    let bpb = (bpp * 16 / 8) as usize;
    let dds_bpb = match &dds[84..88] {
        b"DXT1" | b"DXTA" | b"ATI1" | b"BC4U" => 8,
        b"DXT5" | b"DXT4" | b"DXT3" | b"DXT2" | b"DXTL" | b"ATI2" | b"BC5U" | b"BC7X" => 16,
        _ => return None,
    };
    if dds_bpb != bpb {
        return None;
    }
    let two = (flags & (FF_PLAINCOMP | FF_COLOR | FF_ALPHA)) == (FF_PLAINCOMP | FF_COLOR | FF_ALPHA) || (flags & FF_BICOLORCOMP) != 0;
    let alpha_sec = ((flags & FF_ALPHA) != 0 && (flags & FF_DEDUCEDALPHACOMP) == 0) || (flags & FF_BICOLORCOMP) != 0;
    let color_sec = (flags & FF_COLOR) != 0 || (flags & FF_BICOLORCOMP) != 0;
    let bpc = bpb / if two { 2 } else { 1 };
    let mut out = Vec::with_capacity(dds.len() + 8 * mips as usize + 12);
    out.extend_from_slice(&kind);
    out.extend_from_slice(&fourcc);
    out.extend_from_slice(&(w as u16).to_le_bytes());
    out.extend_from_slice(&(h as u16).to_le_bytes());
    let mut off = 128usize;
    for l in 0..mips {
        let (lw, lh) = ((w >> l).max(1) as usize, (h >> l).max(1) as usize);
        let n = lw.div_ceil(4) * lh.div_ceil(4);
        let blocks = dds.get(off..off + n * bpb)?;
        off += n * bpb;
        let word = |i: usize, o: usize| &blocks[i * bpb + o..i * bpb + o + 4];
        let start = out.len();
        out.extend_from_slice(&[0u8; 8]); // size, cflags = 0
        if alpha_sec {
            for i in 0..n {
                out.extend_from_slice(word(i, 0));
                if bpc > 4 {
                    out.extend_from_slice(word(i, 4));
                }
            }
        }
        if color_sec {
            let base = if two { bpc } else { 0 };
            for i in 0..n {
                out.extend_from_slice(word(i, base));
            }
            if bpc > 4 {
                for i in 0..n {
                    out.extend_from_slice(word(i, base + 4));
                }
            }
        }
        let size = (out.len() - start) as u32;
        out[start..start + 4].copy_from_slice(&size.to_le_bytes());
    }
    Some(out)
}

#[cfg(test)]
mod from_dds_tests {
    use super::*;

    fn dds(w: u32, h: u32, mips: u32, fcc: &[u8; 4], bpb: usize) -> Vec<u8> {
        let mut v = vec![0u8; 128];
        v[0..4].copy_from_slice(b"DDS ");
        v[12..16].copy_from_slice(&h.to_le_bytes());
        v[16..20].copy_from_slice(&w.to_le_bytes());
        v[28..32].copy_from_slice(&mips.to_le_bytes());
        v[84..88].copy_from_slice(fcc);
        let mut x: u32 = 12345;
        for l in 0..mips {
            let n = ((w >> l).max(1) as usize).div_ceil(4) * ((h >> l).max(1) as usize).div_ceil(4);
            for _ in 0..n * bpb {
                x = x.wrapping_mul(1103515245).wrapping_add(12345);
                v.push((x >> 16) as u8);
            }
        }
        v
    }

    #[test]
    fn roundtrip_mip0_and_levels() {
        for (fcc_dds, fcc_anet, bpb) in [(b"DXT5", *b"DXT5", 16), (b"DXT1", *b"DXT1", 8), (b"DXTA", *b"DXTA", 8), (b"ATI2", *b"3DCX", 16)] {
            for (w, h, m) in [(2048u32, 1024u32, 10u32), (256, 512, 1), (64, 16, 5)] {
                let d = dds(w, h, m, fcc_dds, bpb);
                let a = from_dds(&d, *b"ATEX", fcc_anet).unwrap();
                let dec = decode(&a).unwrap();
                let n0 = (w as usize).div_ceil(4) * (h as usize).div_ceil(4) * bpb;
                assert_eq!((dec.width, dec.height), (w, h));
                assert!(dec.blocks == d[128..128 + n0], "{} {w}x{h}", String::from_utf8_lossy(fcc_dds));
                // level chunks walk exactly to the end
                let (mut o, mut k) = (12usize, 0);
                while o + 4 <= a.len() {
                    o += u32::from_le_bytes(a[o..o + 4].try_into().unwrap()) as usize;
                    k += 1;
                }
                assert_eq!((o, k), (a.len(), m));
            }
        }
    }
}

#[cfg(test)]
mod from_dds_real {
    /// GW2REM_TEST_DDS=<dir with *.dds.zst> + GW2REM_TEST_IDS=a,b: real pack files round-trip.
    #[test]
    fn real_pack_files() {
        let (Ok(dir), Ok(ids)) = (std::env::var("GW2REM_TEST_DDS"), std::env::var("GW2REM_TEST_IDS")) else { return };
        for id in ids.split(',') {
            let f = std::fs::File::open(format!("{dir}/{id}.dds.zst")).unwrap();
            let d = zstd::stream::decode_all(std::io::BufReader::new(f)).unwrap();
            let fcc: [u8; 4] = match &d[84..88] { b"ATI2" => *b"3DCX", x => x.try_into().unwrap() };
            let a = super::from_dds(&d, *b"ATEX", fcc).unwrap();
            let dec = super::decode(&a).unwrap();
            let n = dec.blocks.len();
            assert!(dec.blocks == d[128..128 + n], "{id}");
            eprintln!("{id}: {}x{} {} -> atex {} bytes ok", dec.width, dec.height, String::from_utf8_lossy(&fcc), a.len());
        }
    }
}
