//! texrdo study <dir> : for every .dds (legacy header, DXT1/DXT5/DXTA/ATI2/DXTL, level 0 only)
//! tries block-reuse RDO at several thresholds and prints size after zstd-19 and PSNR.
//! RDO: in raster order, a block is replaced by an exact copy of its left or upper
//! neighbour (already final) when the decoded error stays under `max_mse`.
use rayon::prelude::*;
use std::path::PathBuf;

fn bpb(f: &[u8]) -> usize { if f == b"DXT1" || f == b"DXTA" { 8 } else { 16 } }

fn dec_block(f: &[u8], b: &[u8], out: &mut [u32; 16]) {
    match f {
        b"DXT1" => texture2ddecoder::decode_bc1_block(b, out),
        b"DXTA" => texture2ddecoder::decode_bc4_block(b, out),
        b"ATI2" => texture2ddecoder::decode_bc5_block(b, out),
        _ => texture2ddecoder::decode_bc3_block(b, out),
    }
}

/// Error per meaningful channel. The decoder packs pixels as BGRA (R in bits 16..23),
/// so a one-channel BC4 mask lives in byte 2 and BC5 in bytes 2 and 1: every byte is
/// compared (unused ones are constant) and the sum is divided by the real channel count.
/// Comparing only the low `ch` bytes made every BC4 block look identical (all masks
/// collapsed into one block) and ignored X of normal maps.
fn mse(a: &[u32; 16], b: &[u32; 16], ch: usize) -> f64 {
    let mut s = 0f64;
    for i in 0..16 {
        for c in 0..4 {
            let d = ((a[i] >> (8 * c)) & 255) as f64 - ((b[i] >> (8 * c)) & 255) as f64;
            s += d * d;
        }
    }
    s / (16 * ch) as f64
}

/// returns (rdo blocks, total mse vs original decode)
pub fn rdo(f: &[u8], blocks: &[u8], bw: usize, bh: usize, max_mse: f64) -> (Vec<u8>, f64) {
    let n = bpb(f);
    let ch = if f == b"DXTA" { 1 } else if f == b"ATI2" { 2 } else { 4 };
    let mut out = blocks.to_vec();
    let mut err = 0f64;
    let (mut a, mut b) = ([0u32; 16], [0u32; 16]);
    for y in 0..bh {
        for x in 0..bw {
            let i = (y * bw + x) * n;
            dec_block(f, &blocks[i..i + n], &mut a);
            let mut best: Option<(f64, usize)> = None;
            for j in [x.checked_sub(1).map(|xx| (y * bw + xx) * n), y.checked_sub(1).map(|yy| (yy * bw + x) * n)].into_iter().flatten() {
                dec_block(f, &out[j..j + n], &mut b);
                let e = mse(&a, &b, ch);
                if e <= max_mse && best.is_none_or(|(be, _)| e < be) { best = Some((e, j)); }
            }
            if let Some((e, j)) = best {
                let src = out[j..j + n].to_vec();
                out[i..i + n].copy_from_slice(&src);
                err += e;
            }
        }
    }
    (out, err / (bw * bh) as f64)
}

/// texrdo apply <in_dir> <out_dir> <max_mse>: RDO on level 0 of every .dds, writes
/// `<id>.dds.zst` (zstd-19) and copies index.jsonl / *.txt unchanged.
fn apply(inp: PathBuf, out: PathBuf, t: f64) {
    std::fs::create_dir_all(&out).unwrap();
    let files: Vec<_> = std::fs::read_dir(&inp).unwrap().filter_map(|e| e.ok().map(|e| e.path())).collect();
    let (raw, z): (usize, usize) = files.par_iter().map(|p| {
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        if !name.ends_with(".dds") {
            let _ = std::fs::copy(p, out.join(&name));
            return (0, 0);
        }
        let mut d = std::fs::read(p).unwrap();
        let h = u32::from_le_bytes(d[12..16].try_into().unwrap()) as usize;
        let w = u32::from_le_bytes(d[16..20].try_into().unwrap()) as usize;
        let f = d[84..88].to_vec();
        if t > 0.0 && matches!(&f[..], b"DXT1" | b"DXT5" | b"DXTL" | b"DXTA" | b"ATI2") {
            let (bw, bh) = (w.div_ceil(4), h.div_ceil(4));
            let l0 = bw * bh * bpb(&f);
            let (o, _) = rdo(&f, &d[128..128 + l0], bw, bh, t);
            d[128..128 + l0].copy_from_slice(&o);
        }
        let c = zstd::bulk::compress(&d, 19).unwrap();
        std::fs::write(out.join(format!("{name}.zst")), &c).unwrap();
        (d.len(), c.len())
    }).reduce(|| (0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
    eprintln!("raw {:.0} MB -> {:.0} MB ({:.1}x)", raw as f64 / 1e6, z as f64 / 1e6, raw as f64 / z.max(1) as f64);
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a[1] == "apply" {
        return apply(PathBuf::from(&a[2]), PathBuf::from(&a[3]), a[4].parse().unwrap());
    }
    let dir = PathBuf::from(std::env::args().nth(2).expect("dir"));
    let files: Vec<_> = std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().is_some_and(|e| e == "dds")).collect();
    let ts = [0.0f64, 1.0, 2.0, 4.0, 8.0, 16.0];
    let res: Vec<(String, usize, Vec<(usize, f64)>)> = files.par_iter().filter_map(|p| {
        let d = std::fs::read(p).ok()?;
        let h = u32::from_le_bytes(d[12..16].try_into().unwrap()) as usize;
        let w = u32::from_le_bytes(d[16..20].try_into().unwrap()) as usize;
        if w < 512 && h < 512 { return None; }
        let f = d[84..88].to_vec();
        let (bw, bh) = (w.div_ceil(4), h.div_ceil(4));
        let l0 = bw * bh * bpb(&f);
        let blocks = &d[128..128 + l0];
        let mut r = vec![];
        for &t in &ts {
            let (o, e) = if t == 0.0 { (blocks.to_vec(), 0.0) } else { rdo(&f, blocks, bw, bh, t) };
            r.push((zstd::bulk::compress(&o, 19).unwrap().len(), e));
        }
        Some((String::from_utf8_lossy(&f).into(), l0, r))
    }).collect();
    let mut by: std::collections::BTreeMap<String, (usize, Vec<(usize, f64)>, usize)> = Default::default();
    for (f, raw, r) in res {
        let e = by.entry(f).or_insert((0, vec![(0, 0.0); ts.len()], 0));
        e.0 += raw; e.2 += 1;
        for (k, (s, m)) in r.into_iter().enumerate() { e.1[k].0 += s; e.1[k].1 += m; }
    }
    for (f, (raw, r, n)) in by {
        print!("{f} n={n} raw={:.0}MB |", raw as f64 / 1e6);
        for (k, (s, m)) in r.iter().enumerate() {
            let mm = m / n as f64;
            let psnr = if mm > 0.0 { 10.0 * (255.0f64 * 255.0 / mm).log10() } else { 99.0 };
            print!(" T{}: {:.0}MB ({:.1}x, {:.0}dB)", ts[k], *s as f64 / 1e6, raw as f64 / *s as f64, psnr);
        }
        println!();
    }
}
