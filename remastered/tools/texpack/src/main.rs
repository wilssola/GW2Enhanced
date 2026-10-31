//! texpack <png_dir> <pack_dir> <texture_hashes.jsonl|csv>
//! For every `<file_id>.png` writes `<pack_dir>/<file_id>.dds` (legacy DDS header,
//! DXT1 if fully opaque else DXT5, full mip chain — the formats the game's own DDS
//! files use) and appends `<pack_dir>/index.jsonl`:
//! {file_id, width, height, format, mips, bytes, src_xxh3_128}
//! `src_xxh3_128` is the hash of the original texture this HD version replaces;
//! the runtime only serves the DDS while the game's file still has that hash.

use anyhow::Result;
use image::{RgbaImage, imageops::FilterType};
use intel_tex_2::{RSurface, RgSurface, RgbaSurface, bc1, bc3, bc4, bc5, bc7};
use rayon::prelude::*;
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

fn dds_header(w: u32, h: u32, mips: u32, fourcc: &[u8; 4], linear: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity(128);
    let p = |v: &mut Vec<u8>, x: u32| v.extend_from_slice(&x.to_le_bytes());
    v.extend_from_slice(b"DDS ");
    p(&mut v, 124);
    p(&mut v, 0x1 | 0x2 | 0x4 | 0x1000 | 0x20000 | 0x80000); // caps|height|width|pixfmt|mipcount|linearsize
    p(&mut v, h);
    p(&mut v, w);
    p(&mut v, linear);
    p(&mut v, 0);
    p(&mut v, mips);
    for _ in 0..11 {
        p(&mut v, 0);
    }
    p(&mut v, 32);
    p(&mut v, 0x4); // DDPF_FOURCC
    v.extend_from_slice(fourcc);
    for _ in 0..5 {
        p(&mut v, 0);
    }
    p(&mut v, 0x1000 | 0x8 | 0x400000); // texture|complex|mipmap
    for _ in 0..4 {
        p(&mut v, 0);
    }
    v
}

#[derive(Clone, Copy, PartialEq)]
enum Enc { Dxt1, Dxt5, DxtA, Ati2, DxtL, Bc7 }

fn encode(img: &RgbaImage, enc: Enc) -> Vec<u8> {
    // BC needs multiples of 4: pad by edge replication.
    let (w, h) = img.dimensions();
    let (pw, ph) = (w.div_ceil(4) * 4, h.div_ceil(4) * 4);
    let padded;
    let src = if (pw, ph) != (w, h) {
        let mut p = RgbaImage::new(pw, ph);
        for y in 0..ph {
            for x in 0..pw {
                p.put_pixel(x, y, *img.get_pixel(x.min(w - 1), y.min(h - 1)));
            }
        }
        padded = p;
        &padded
    } else {
        img
    };
    if enc == Enc::DxtA {
        // single-channel mask (decoded into R): BC4 blocks under the ANet "DXTA" FourCC
        let r: Vec<u8> = src.pixels().map(|p| p[0]).collect();
        return bc4::compress_blocks(&RSurface { data: &r, width: pw, height: ph, stride: pw });
    }
    if enc == Enc::Ati2 {
        let rg: Vec<u8> = src.pixels().flat_map(|p| [p[0], p[1]]).collect();
        return bc5::compress_blocks(&RgSurface { data: &rg, width: pw, height: ph, stride: pw * 2 });
    }
    let s = RgbaSurface { data: src.as_raw(), width: pw, height: ph, stride: pw * 4 };
    match enc {
        Enc::Dxt1 => bc1::compress_blocks(&s),
        // BC7X originals: the engine needs the record format back, so real BC7 blocks.
        Enc::Bc7 => bc7::compress_blocks(&bc7::alpha_basic_settings(), &s),
        _ => bc3::compress_blocks(&s),
    }
}

fn load_hashes(p: &Path) -> HashMap<u32, (String, u64, u32)> {
    let mut m = HashMap::new();
    let Ok(s) = std::fs::read_to_string(p) else { return m };
    for l in s.lines() {
        if l.starts_with('{') {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(l) {
                if let (Some(id), Some(h)) = (v["file_id"].as_u64(), v["xxh3_128"].as_str()) {
                    m.insert(id as u32, (h.to_string(), v["size"].as_u64().unwrap_or(0), v["levels"].as_u64().unwrap_or(0) as u32));
                }
            }
        } else {
            let f: Vec<&str> = l.split(',').collect();
            if f.len() >= 4 {
                if let Ok(id) = f[1].parse() {
                    m.insert(id, (f[3].to_string(), f[2].parse().unwrap_or(0), 0));
                }
            }
        }
    }
    m
}

fn main() -> Result<()> {
    let a: Vec<String> = std::env::args().collect();
    let (src, dst) = (PathBuf::from(&a[1]), PathBuf::from(&a[2]));
    let hashes = load_hashes(Path::new(&a[3]));
    // Optional 4th arg: survey.jsonl -> keep the original ANet format (the engine
    // asserts decoded format == record format). Unsupported originals are skipped.
    let orig_w: HashMap<u32, u32> = a.get(4).and_then(|p| std::fs::read_to_string(p).ok()).map(|s| {
        s.lines().filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter_map(|v| Some((v["file_id"].as_u64()? as u32, v["width"].as_u64()? as u32))).collect()
    }).unwrap_or_default();
    let orig_fmt: HashMap<u32, String> = a.get(4).and_then(|p| std::fs::read_to_string(p).ok()).map(|s| {
        s.lines().filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter_map(|v| Some((v["file_id"].as_u64()? as u32, v["format"].as_str()?.to_string()))).collect()
    }).unwrap_or_default();
    std::fs::create_dir_all(&dst)?;
    let files: Vec<PathBuf> = std::fs::read_dir(&src)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "png"))
        .collect();
    let lines: Vec<String> = files
        .par_iter()
        .filter_map(|p| {
            let id: u32 = p.file_stem()?.to_str()?.parse().ok()?;
            let mut img = image::open(p).ok()?.to_rgba8();
            let enc = match orig_fmt.get(&id).map(String::as_str) {
                None if img.pixels().any(|px| px[3] < 255) => Enc::Dxt5,
                None => Enc::Dxt1,
                Some("DXT1") => Enc::Dxt1,
                Some("DXT4" | "DXT5") => Enc::Dxt5,
                Some("BC7X") => Enc::Bc7,
                Some("DXTA") => Enc::DxtA,
                Some("3DCX") => Enc::Ati2,
                Some("DXTL") => Enc::DxtL,
                Some(_) => return None,
            };
            let (fourcc, bpb): (&[u8; 4], u32) = match enc {
                Enc::Dxt1 => (b"DXT1", 8),
                Enc::Dxt5 => (b"DXT5", 16),
                Enc::Bc7 => (b"BC7X", 16),
                Enc::DxtA => (b"DXTA", 8),
                Enc::Ati2 => (b"ATI2", 16),
                Enc::DxtL => (b"DXTL", 16),
            };
            let (w, h) = img.dimensions();
            // Same number of levels as the original file: the engine pre-allocates
            // the decode buffer from it (more levels = heap overflow).
            let full = 32 - w.max(h).leading_zeros();
            // Upscaled by 2^k: one extra level per doubling when the original
            // stores a chain; single-level originals stay single-level.
            let src_w = orig_w.get(&id).copied().unwrap_or(w);
            let extra = (w / src_w.max(1)).max(1).trailing_zeros();
            let mips = match hashes.get(&id).map(|h| h.2) {
                Some(1) => 1,
                Some(n) if n > 0 => (n + extra).min(full),
                _ => full,
            };
            let mut body = Vec::new();
            for level in 0..mips {
                if level > 0 {
                    let (nw, nh) = ((img.width() / 2).max(1), (img.height() / 2).max(1));
                    img = image::imageops::resize(&img, nw, nh, FilterType::Triangle);
                }
                body.extend(encode(&img, enc));
            }
            let first = w.div_ceil(4) * h.div_ceil(4) * bpb;
            let mut out = dds_header(w, h, mips, fourcc, first);
            out.extend(body);
            std::fs::write(dst.join(format!("{id}.dds")), &out).ok()?;
            Some(format!(
                r#"{{"file_id":{id},"width":{w},"height":{h},"format":"{}","mips":{mips},"bytes":{},"src_xxh3_128":{},"src_bytes":{}}}"#,
                String::from_utf8_lossy(fourcc),
                out.len(),
                hashes.get(&id).map(|h| format!("\"{}\"", h.0)).unwrap_or("null".into()),
                hashes.get(&id).map(|h| h.1).unwrap_or(0)
            ))
        })
        .collect();
    let mut idx = std::fs::OpenOptions::new().create(true).append(true).open(dst.join("index.jsonl"))?;
    for l in &lines {
        writeln!(idx, "{l}")?;
    }
    eprintln!("packed {} textures", lines.len());
    Ok(())
}
