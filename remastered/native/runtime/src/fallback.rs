//! Runtime 2× of composite sources that are not in the HD pack.
//!
//! With the armor atlas at 2048 every composite input (base, normal, dye/cut
//! masks) must be 2× its original size, including other players' armor that the
//! pack does not cover. This turns an original ATEX file into a DDS at 2×
//! (bilinear, no AI), same format, same level rule as the pack builder.

use crate::atex;
use intel_tex_2::{RSurface, RgSurface, RgbaSurface, bc1, bc3, bc4, bc5, bc7};

#[derive(Clone, Copy, PartialEq)]
enum Enc {
    Dxt1,
    Dxt5,
    DxtA,
    /// 3DCX normals: BC5 under the "ATI2" FourCC (engine format 0x1e, same as 3DCX).
    Ati2,
    /// DXTL: DXT5 blocks kept under their own FourCC (engine maps it to its own format).
    DxtL,
    /// BC7X model textures: real BC7 blocks under a "BC7X" FourCC; the mod hands them over as native ATEX.
    Bc7,
}

/// Formats the fallback can rebuild (decided from the first 8 bytes of the file).
pub fn supported(fourcc: &[u8]) -> bool {
    matches!(fourcc, b"DXT1" | b"DXT4" | b"DXT5" | b"DXTA" | b"3DCX" | b"BC7X" | b"DXTL")
}

fn atex_levels(b: &[u8]) -> u32 {
    let (mut o, mut n) = (12usize, 0);
    while o + 4 <= b.len() {
        let sz = u32::from_le_bytes(b[o..o + 4].try_into().unwrap()) as usize;
        if sz < 4 || o + sz > b.len() {
            break;
        }
        o += sz;
        n += 1;
    }
    n
}

/// RGBA8 from BCn blocks.
fn decode(d: &atex::Decoded) -> Option<Vec<u8>> {
    let (w, h) = (d.width as usize, d.height as usize);
    let mut px = vec![0u32; w * h];
    let r = match d.bc {
        atex::Bc::Bc1 => texture2ddecoder::decode_bc1(&d.blocks, w, h, &mut px),
        atex::Bc::Bc3 => texture2ddecoder::decode_bc3(&d.blocks, w, h, &mut px),
        atex::Bc::Bc4 => texture2ddecoder::decode_bc4(&d.blocks, w, h, &mut px),
        atex::Bc::Bc5 => texture2ddecoder::decode_bc5(&d.blocks, w, h, &mut px),
        // BC7X is re-encoded as DXT5: composite loads convert every source to the
        // composite format anyway, and the engine's DDS reader has no BC7 FourCC.
        atex::Bc::Bc7 => texture2ddecoder::decode_bc7(&d.blocks, w, h, &mut px),
        _ => return None,
    };
    r.ok()?;
    // texture2ddecoder packs pixels as 0xAARRGGBB
    Some(px.iter().flat_map(|&p| [(p >> 16) as u8, (p >> 8) as u8, p as u8, (p >> 24) as u8]).collect())
}

/// 2× bilinear (pixel-center aligned, edge clamped), rows in parallel.
fn up2(src: &[u8], w: usize, h: usize) -> Vec<u8> {
    use rayon::prelude::*;
    let (nw, nh) = (w * 2, h * 2);
    let mut out = vec![0u8; nw * nh * 4];
    out.par_chunks_mut(nw * 4).enumerate().for_each(|(y, row)| {
        let fy = ((y as f32 + 0.5) / 2.0 - 0.5).max(0.0);
        let y0 = (fy as usize).min(h - 1);
        let y1 = (y0 + 1).min(h - 1);
        let ty = fy - y0 as f32;
        let (r0, r1) = (&src[y0 * w * 4..][..w * 4], &src[y1 * w * 4..][..w * 4]);
        for x in 0..nw {
            let fx = ((x as f32 + 0.5) / 2.0 - 0.5).max(0.0);
            let x0 = (fx as usize).min(w - 1);
            let x1 = (x0 + 1).min(w - 1);
            let tx = fx - x0 as f32;
            for c in 0..4 {
                let a = r0[x0 * 4 + c] as f32 * (1.0 - tx) + r0[x1 * 4 + c] as f32 * tx;
                let b = r1[x0 * 4 + c] as f32 * (1.0 - tx) + r1[x1 * 4 + c] as f32 * tx;
                row[x * 4 + c] = (a * (1.0 - ty) + b * ty + 0.5) as u8;
            }
        }
    });
    out
}

/// 2×2 box downsample (for the mip chain), rows in parallel.
fn down2(src: &[u8], w: usize, h: usize) -> (Vec<u8>, usize, usize) {
    use rayon::prelude::*;
    let (nw, nh) = ((w / 2).max(1), (h / 2).max(1));
    let mut out = vec![0u8; nw * nh * 4];
    out.par_chunks_mut(nw * 4).enumerate().for_each(|(y, row)| {
        for x in 0..nw {
            for c in 0..4 {
                let mut s = 0u32;
                for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                    let xx = (x * 2 + dx).min(w - 1);
                    let yy = (y * 2 + dy).min(h - 1);
                    s += src[(yy * w + xx) * 4 + c] as u32;
                }
                row[x * 4 + c] = ((s + 2) / 4) as u8;
            }
        }
    });
    (out, nw, nh)
}

/// BCn encode, split into horizontal bands of block rows compressed in parallel
/// (BC blocks are stored row-major by block row, so bands concatenate exactly).
fn encode(rgba: &[u8], w: usize, h: usize, enc: Enc) -> Vec<u8> {
    use rayon::prelude::*;
    let (pw, ph) = (w.div_ceil(4) * 4, h.div_ceil(4) * 4);
    let mut pad = vec![0u8; pw * ph * 4];
    pad.par_chunks_mut(pw * 4).enumerate().for_each(|(y, row)| {
        let sy = y.min(h - 1);
        for x in 0..pw {
            let s = (sy * w + x.min(w - 1)) * 4;
            row[x * 4..][..4].copy_from_slice(&rgba[s..s + 4]);
        }
    });
    const BAND: usize = 64; // pixel rows per band (multiple of 4)
    let bands: Vec<Vec<u8>> = pad
        .par_chunks(pw * 4 * BAND)
        .map(|band| {
            let bh = (band.len() / (pw * 4)) as u32;
            match enc {
                Enc::DxtA => {
                    let r: Vec<u8> = band.chunks_exact(4).map(|p| p[0]).collect();
                    bc4::compress_blocks(&RSurface { data: &r, width: pw as u32, height: bh, stride: pw as u32 })
                }
                Enc::Ati2 => {
                    let rg: Vec<u8> = band.chunks_exact(4).flat_map(|p| [p[0], p[1]]).collect();
                    bc5::compress_blocks(&RgSurface { data: &rg, width: pw as u32, height: bh, stride: pw as u32 * 2 })
                }
                Enc::Dxt1 => bc1::compress_blocks(&RgbaSurface { data: band, width: pw as u32, height: bh, stride: pw as u32 * 4 }),
                Enc::Dxt5 | Enc::DxtL => bc3::compress_blocks(&RgbaSurface { data: band, width: pw as u32, height: bh, stride: pw as u32 * 4 }),
                Enc::Bc7 => bc7::compress_blocks(&bc7::alpha_ultra_fast_settings(), &RgbaSurface { data: band, width: pw as u32, height: bh, stride: pw as u32 * 4 }),
            }
        })
        .collect();
    bands.concat()
}

fn dds_header(w: u32, h: u32, mips: u32, fourcc: &[u8; 4], linear: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity(128);
    let p = |v: &mut Vec<u8>, x: u32| v.extend_from_slice(&x.to_le_bytes());
    v.extend_from_slice(b"DDS ");
    p(&mut v, 124);
    p(&mut v, 0x1 | 0x2 | 0x4 | 0x1000 | 0x20000 | 0x80000);
    p(&mut v, h);
    p(&mut v, w);
    p(&mut v, linear);
    p(&mut v, 0);
    p(&mut v, mips);
    for _ in 0..11 {
        p(&mut v, 0);
    }
    p(&mut v, 32);
    p(&mut v, 0x4);
    v.extend_from_slice(fourcc);
    for _ in 0..5 {
        p(&mut v, 0);
    }
    p(&mut v, 0x1000 | 0x8 | 0x400000);
    for _ in 0..4 {
        p(&mut v, 0);
    }
    v
}

/// Original ATEX file -> DDS at 2×, or None if unsupported/undecodable.
#[cfg(test)]
pub fn upscale_file(file: &[u8]) -> Option<Vec<u8>> {
    upscale_file_by(file, 2)
}

/// A pack DDS (already 2×, AI) -> the same DDS at 2× its size (bilinear, no AI).
/// Used for armor pieces of a 4× character: only skin/face/fur get a real 4× pack,
/// armor keeps its 2× detail but must match the 4× atlas layout.
pub fn upscale_dds_2x(dds: &[u8]) -> Option<Vec<u8>> {
    if dds.len() < 128 || &dds[0..4] != b"DDS " {
        return None;
    }
    let rd = |o: usize| u32::from_le_bytes(dds[o..o + 4].try_into().unwrap());
    let (h, w, mips) = (rd(12) as usize, rd(16) as usize, rd(28).max(1));
    let fourcc: [u8; 4] = dds[84..88].try_into().unwrap();
    let (enc, bpb, bc) = match &fourcc {
        b"DXT1" => (Enc::Dxt1, 8, atex::Bc::Bc1),
        b"DXTA" => (Enc::DxtA, 8, atex::Bc::Bc4),
        b"ATI2" => (Enc::Ati2, 16, atex::Bc::Bc5),
        b"DXTL" => (Enc::DxtL, 16, atex::Bc::Bc3),
        b"DXT5" => (Enc::Dxt5, 16, atex::Bc::Bc3),
        _ => return None,
    };
    let _ = Enc::Bc7;
    let n0 = w.div_ceil(4) * h.div_ceil(4) * bpb;
    let blocks = dds.get(128..128 + n0)?;
    let mut px = vec![0u32; w * h];
    match bc {
        atex::Bc::Bc1 => texture2ddecoder::decode_bc1(blocks, w, h, &mut px),
        atex::Bc::Bc4 => texture2ddecoder::decode_bc4(blocks, w, h, &mut px),
        atex::Bc::Bc5 => texture2ddecoder::decode_bc5(blocks, w, h, &mut px),
        _ => texture2ddecoder::decode_bc3(blocks, w, h, &mut px),
    }
    .ok()?;
    let rgba: Vec<u8> = px.iter().flat_map(|&p| [(p >> 16) as u8, (p >> 8) as u8, p as u8, (p >> 24) as u8]).collect();
    let mut img = up2(&rgba, w, h);
    let (mut w, mut h) = (w * 2, h * 2);
    let full = 32 - (w.max(h) as u32).leading_zeros();
    let levels = if mips <= 1 { 1 } else { (mips + 1).min(full) };
    let mut out = dds_header(w as u32, h as u32, levels, &fourcc, (w.div_ceil(4) * h.div_ceil(4) * bpb) as u32);
    for l in 0..levels {
        if l > 0 {
            let (n, nw, nh) = down2(&img, w, h);
            img = n;
            w = nw;
            h = nh;
        }
        out.extend(encode(&img, w, h, enc));
    }
    Some(out)
}

/// Original ATEX -> DDS at `factor` (2 or 4) × its size. BC7X becomes DXT5 (composite pieces).
#[cfg(test)]
pub fn upscale_file_by(file: &[u8], factor: u32) -> Option<Vec<u8>> {
    upscale_file_keep(file, factor, false)
}

/// Same for a model texture (`keep_bc7`): BC7X stays BC7 (the engine checks the record format).
pub fn upscale_file_keep(file: &[u8], factor: u32, keep_bc7: bool) -> Option<Vec<u8>> {
    if file.len() < 16 || !supported(&file[4..8]) {
        return None;
    }
    let d = atex::decode(file).ok()?;
    let enc = match &d.fourcc {
        b"BC7X" if keep_bc7 => Enc::Bc7,
        b"DXT1" => Enc::Dxt1,
        b"DXTA" => Enc::DxtA,
        b"3DCX" => Enc::Ati2,
        b"DXTL" => Enc::DxtL,
        _ => Enc::Dxt5,
    };
    let (fourcc, bpb): (&[u8; 4], usize) = match enc {
        Enc::Dxt1 => (b"DXT1", 8),
        Enc::Dxt5 => (b"DXT5", 16),
        Enc::DxtA => (b"DXTA", 8),
        Enc::Ati2 => (b"ATI2", 16),
        Enc::DxtL => (b"DXTL", 16),
        Enc::Bc7 => (b"BC7X", 16),
    };
    let rgba = decode(&d)?;
    let steps = factor.max(2).trailing_zeros();
    let (mut w, mut h) = (d.width as usize, d.height as usize);
    let mut img = rgba;
    for _ in 0..steps {
        img = up2(&img, w, h);
        w *= 2;
        h *= 2;
    }
    let full = 32 - (w.max(h) as u32).leading_zeros();
    let levels = match atex_levels(file) {
        0 | 1 => 1,
        n => (n + steps).min(full),
    };
    let mut out = dds_header(w as u32, h as u32, levels, fourcc, (w.div_ceil(4) * h.div_ceil(4) * bpb) as u32);
    for l in 0..levels {
        if l > 0 {
            let (n, nw, nh) = down2(&img, w, h);
            img = n;
            w = nw;
            h = nh;
        }
        out.extend(encode(&img, w, h, enc));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dims(dds: &[u8]) -> (u32, u32, u32, [u8; 4]) {
        let u = |o: usize| u32::from_le_bytes(dds[o..o + 4].try_into().unwrap());
        (u(16), u(12), u(28), dds[84..88].try_into().unwrap())
    }

    /// Real ATEX file dumped from Gw2.dat (datscan raw), from `GW2REM_TEST_RAW`.
    /// These tests are skipped when the variable is not set.
    fn raw(name: &str) -> Option<Vec<u8>> {
        std::fs::read(std::path::Path::new(&std::env::var("GW2REM_TEST_RAW").ok()?).join(name)).ok()
    }

    #[test]
    fn upscales_4x() {
        let Some(file) = raw("raw_1009182.bin") else { return };
        let dds = upscale_file_by(&file, 4).expect("4x");
        let (dw, dh, dl, df) = dims(&dds);
        assert_eq!((dw, dh, dl, df), (2048, 2048, 1, *b"DXT5"));
        let dds = upscale_file_by(&raw("raw_2564483.bin").unwrap(), 4).unwrap();
        let (dw, _, dl, _) = dims(&dds);
        assert_eq!((dw, dl), (2048, 10));
    }

    #[test]
    fn bc7x_becomes_dxt5() {
        let Some(file) = raw("raw_bc7.bin") else { return };
        let w = u16::from_le_bytes([file[8], file[9]]) as u32;
        let dds = upscale_file(&file).expect("bc7x upscale");
        let (dw, _, _, df) = dims(&dds);
        assert_eq!((dw, df), (w * 2, *b"DXT5"));
    }

    #[test]
    fn upscales_real_files() {
        for (id, w, levels, fcc) in [(2564483u32, 1024, 9, *b"DXT1"), (2564482, 2048, 1, *b"DXT5"), (1009182, 1024, 1, *b"DXT5"), (124756, 512, 1, *b"ATI2")] {
            let Some(file) = raw(&format!("raw_{id}.bin")) else { continue };
            let dds = upscale_file(&file).expect("upscale");
            let (dw, dh, dl, df) = dims(&dds);
            assert_eq!((dw, dl, df), (w, levels, fcc), "{id}");
            // payload = sum of level sizes
            let bpb = if &df == b"DXT1" || &df == b"DXTA" { 8 } else { 16 };
            let mut total = 0;
            let (mut lw, mut lh) = (dw as usize, dh as usize);
            for _ in 0..dl {
                total += lw.div_ceil(4) * lh.div_ceil(4) * bpb;
                lw = (lw / 2).max(1);
                lh = (lh / 2).max(1);
            }
            assert_eq!(dds.len(), 128 + total, "{id}");
        }
    }
}

/// Mip 0 of an original ATEX file as a plain DDS (same size, same format), or None when it cannot be
/// read. Debug colors tint it and hand it back in place of the original.
pub fn flat_original(file: &[u8]) -> Option<Vec<u8>> {
    if file.len() < 16 || !supported(&file[4..8]) {
        return None;
    }
    let d = atex::decode(file).ok()?;
    let fourcc: &[u8; 4] = match &d.fourcc {
        b"DXT1" => b"DXT1",
        b"DXT5" => b"DXT5",
        _ => return None,
    };
    let mut v = dds_header(d.width, d.height, 1, fourcc, d.blocks.len() as u32);
    v.extend_from_slice(&d.blocks);
    Some(v)
}

#[cfg(test)]
mod flat_tests {
    use super::*;

    #[test]
    fn bc7x_pack_dds_round_trips_through_native_atex() {
        let (w, h) = (64u32, 32u32);
        let mut d = dds_header(w, h, 1, b"BC7X", w / 4 * h / 4 * 16);
        d.extend((0..(w / 4 * h / 4) as usize * 16).map(|i| (i * 13 + 5) as u8));
        let atex = crate::atex::from_dds(&d, *b"ATEX", *b"BC7X").expect("BC7X DDS accepted");
        assert_eq!(&atex[4..8], b"BC7X");
        let back = crate::atex::decode(&atex).unwrap();
        assert_eq!((back.width, back.height, back.bc), (w, h, crate::atex::Bc::Bc7));
        assert_eq!(back.blocks, &d[128..]);
    }

    #[test]
    fn flat_original_keeps_size_and_blocks() {
        for (fcc, bpb) in [(b"DXT1", 8usize), (b"DXT5", 16)] {
            let (w, h) = (64u32, 32u32);
            let mut d = vec![0u8; 128];
            d[0..4].copy_from_slice(b"DDS ");
            d[12..16].copy_from_slice(&h.to_le_bytes());
            d[16..20].copy_from_slice(&w.to_le_bytes());
            d[28..32].copy_from_slice(&1u32.to_le_bytes());
            d[84..88].copy_from_slice(fcc);
            d.extend((0..(w / 4 * h / 4) as usize * bpb).map(|i| (i * 7 + 3) as u8));
            let atex = crate::atex::from_dds(&d, *b"ATEX", *fcc).unwrap();
            let flat = flat_original(&atex).unwrap();
            assert_eq!(&flat[0..4], b"DDS ");
            assert_eq!(u32::from_le_bytes(flat[16..20].try_into().unwrap()), w);
            assert_eq!(u32::from_le_bytes(flat[12..16].try_into().unwrap()), h);
            assert_eq!(&flat[84..88], fcc);
            assert_eq!(&flat[128..], &d[128..]);
        }
        let mut other = vec![0u8; 64];
        other[0..4].copy_from_slice(b"ATEX");
        other[4..8].copy_from_slice(b"3DCX");
        assert!(flat_original(&other).is_none());
    }
}
