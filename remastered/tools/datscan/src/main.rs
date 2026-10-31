//! datscan <Gw2.dat> <out.jsonl> — one line per entry whose 4-byte magic is in
//! the watch list (videos etc.), plus a magic histogram on stderr.
#[path = "../../gw2tex/src/compression.rs"]
mod compression;
#[path = "../../gw2tex/src/dat.rs"]
mod dat;
use std::collections::BTreeMap;
use std::io::Write;

fn main() -> anyhow::Result<()> {
    let a: Vec<String> = std::env::args().collect();
    let mut d = dat::Dat::open(std::path::Path::new(&a[1]))?;
    if a[2] == "raw" {
        // datscan <dat> raw <file_id> <out>: dump decompressed file
        let atoc = d.atoc()?;
        let id: u32 = a[3].parse()?;
        std::fs::write(&a[4], d.read(atoc[&id])?)?;
        return Ok(());
    }
    let mut out = std::io::BufWriter::new(std::fs::File::create(&a[2])?);
    let atoc = d.atoc()?;
    let mut fid = std::collections::HashMap::new();
    for (&id, &m) in &atoc {
        fid.entry(m).and_modify(|v: &mut u32| *v = (*v).min(id)).or_insert(id);
    }
    let mut order: Vec<usize> = (0..d.entries.len()).filter(|&i| d.entries[i].offset > 0).collect();
    order.sort_by_key(|&i| d.entries[i].offset);
    let mut hist: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    for i in order {
        let Ok(h) = d.peek(i, 12) else { continue };
        if h.len() < 4 { continue; }
        let key = if &h[0..2] == b"PF" && h.len() >= 12 { format!("PF:{}", String::from_utf8_lossy(&h[8..12])) } else {
            h[0..4].iter().map(|&c| if c.is_ascii_graphic() { c as char } else { '.' }).collect()
        };
        let e = hist.entry(key.clone()).or_default();
        e.0 += 1;
        e.1 += d.entries[i].size as u64;
        if h.starts_with(b"DDS ") {
            let b = d.peek(i, 148).unwrap_or_default();
            if b.len() >= 148 {
                let u = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
                let fcc: String = b[84..88].iter().map(|&c| if c.is_ascii_graphic() { c as char } else { '.' }).collect();
                writeln!(out, "{{\"mft\":{i},\"file_id\":{},\"magic\":\"DDS\",\"width\":{},\"height\":{},\"mips\":{},\"pf_flags\":{},\"fourcc\":\"{}\",\"dxgi\":{},\"rgb_bits\":{}}}",
                    fid.get(&i).copied().unwrap_or(0), u(16), u(12), u(28), u(80), fcc.escape_default(), if &b[84..88] == b"DX10" { u(128) } else { 0 }, u(88))?;
            }
        }
        let video = h.starts_with(b"KB2") || h.starts_with(b"BIK") || h.starts_with(b"RIFF") || &h[4..8] == b"ftyp" || h.starts_with(b"\x1aE\xdf\xa3");
        if video {
            let b = d.peek(i, 44).unwrap_or_default();
            let u = |o: usize| if b.len() >= o + 4 { u32::from_le_bytes(b[o..o + 4].try_into().unwrap()) } else { 0 };
            // Bink header: frames@8, width@20, height@24, fps num@28, den@32
            writeln!(out, "{{\"mft\":{i},\"file_id\":{},\"magic\":\"{}\",\"dat_bytes\":{},\"frames\":{},\"width\":{},\"height\":{},\"fps_num\":{},\"fps_den\":{}}}",
                fid.get(&i).copied().unwrap_or(0), key.escape_default(), d.entries[i].size, u(8), u(20), u(24), u(28), u(32))?;
        }
    }
    let mut v: Vec<_> = hist.into_iter().collect();
    v.sort_by(|a, b| b.1.1.cmp(&a.1.1));
    for (k, (n, b)) in v.iter().take(40) { eprintln!("{k:12} {n:8} {:8.2} GB", *b as f64 / 1073741824.0); }
    Ok(())
}
