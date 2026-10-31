//! texhash — XXH3-128 of the *decompressed* content of every texture in Gw2.dat.
//!
//! Usage: texhash <Gw2.dat> <survey.jsonl> <out.jsonl> [threads]
//! Output JSONL: {mft,file_id,size,xxh3_128,levels} (levels = mip levels stored in the ATEX file). The hash is of the exact bytes the game's
//! decoder receives for that file, so the runtime can verify a texture is still
//! the one the HD version was made from, and a re-run after a game patch shows
//! which textures changed (diff by file_id).

#[path = "../../gw2tex/src/compression.rs"]
mod compression;
#[path = "../../gw2tex/src/dat.rs"]
mod dat;

use anyhow::Result;
use rayon::prelude::*;
use std::io::Write;
use std::path::PathBuf;

/// Number of mip levels stored in an ATEX-family file: chain of `u32 size` blocks
/// starting at offset 12 (size includes the u32 itself).
fn atex_levels(b: &[u8]) -> u32 {
    let mut o = 12usize;
    let mut n = 0;
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

fn main() -> Result<()> {
    let a: Vec<String> = std::env::args().collect();
    let (datp, survey, out) = (PathBuf::from(&a[1]), &a[2], &a[3]);
    let threads: usize = a.get(4).and_then(|s| s.parse().ok()).unwrap_or(8);
    // survey.jsonl ({"mft":..,"file_id":..}) or legacy survey.csv (mft,file_id,...)
    let mut rows: Vec<(usize, u32)> = std::fs::read_to_string(survey)?
        .lines()
        .filter_map(|l| {
            if l.starts_with('{') {
                let v: serde_json::Value = serde_json::from_str(l).ok()?;
                Some((v["mft"].as_u64()? as usize, v["file_id"].as_u64()? as u32))
            } else {
                let mut it = l.split(',');
                Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
            }
        })
        .collect();
    let probe = dat::Dat::open(&datp)?;
    rows.sort_by_key(|&(m, _)| probe.entries[m].offset);
    drop(probe);
    let chunk = rows.len().div_ceil(threads);
    let t = std::time::Instant::now();
    let parts: Vec<Vec<String>> = rows
        .par_chunks(chunk)
        .map(|part| {
            let mut d = dat::Dat::open(&datp).expect("open dat");
            part.iter()
                .map(|&(m, fid)| match d.read(m) {
                    Ok(b) => format!(
                        r#"{{"mft":{m},"file_id":{fid},"size":{},"xxh3_128":"{:032x}","levels":{}}}"#,
                        b.len(),
                        xxhash_rust::xxh3::xxh3_128(&b),
                        atex_levels(&b)
                    ),
                    Err(_) => format!(r#"{{"mft":{m},"file_id":{fid},"size":0,"xxh3_128":null,"levels":0}}"#),
                })
                .collect()
        })
        .collect();
    let mut w = std::io::BufWriter::new(std::fs::File::create(out)?);
    let mut n = 0;
    for p in parts {
        for l in p {
            writeln!(w, "{l}")?;
            n += 1;
        }
    }
    eprintln!("{n} textures hashed in {:.0}s", t.elapsed().as_secs_f32());
    Ok(())
}
