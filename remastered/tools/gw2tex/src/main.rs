//! gw2tex — read-only texture tooling for Gw2.dat (GW2 Remastered).

mod atex;
mod compression;
mod dat;
mod models;
mod pe;
mod pf;
mod refs;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::PathBuf;

#[derive(Parser)]
struct Cli {
    /// Path to Gw2.dat (use a copy: the game locks its own while running)
    #[arg(long)]
    dat: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Lists every texture (ATEX/ATTX/ATEC/ATEP/ATEU/ATET) with format and size (JSONL).
    Survey {
        #[arg(long)]
        out: PathBuf,
    },
    /// Extracts textures (by file id) to DDS (mip 0, original BC blocks).
    Extract {
        #[arg(long)]
        out: PathBuf,
        /// File ids; empty = every id listed in --list (survey JSONL).
        #[arg(long, value_delimiter = ',')]
        ids: Vec<u32>,
        #[arg(long)]
        list: Option<PathBuf>,
    },
    /// Dumps PackFile chunk layouts recovered from the client exe.
    Layout {
        #[arg(long)]
        exe: PathBuf,
        /// Chunk FourCCs to dump (e.g. MODL,GEOM,SKEL); empty = list all.
        #[arg(long, value_delimiter = ',')]
        fcc: Vec<String>,
    },
    /// Lists texture file ids referenced by every MODL pack file (JSONL).
    Models {
        #[arg(long)]
        out: PathBuf,
        #[arg(long)]
        exe: PathBuf,
        /// Only parse these model file ids (debugging).
        #[arg(long, value_delimiter = ',')]
        ids: Vec<u32>,
        /// Cache of MODL mft indices (skips the full header scan when present).
        #[arg(long)]
        index: Option<PathBuf>,
        #[arg(long, default_value_t = 6)]
        threads: usize,
    },
    /// Lists every file reference in every pack file (JSONL).
    Refs {
        #[arg(long)]
        out: PathBuf,
        #[arg(long)]
        exe: PathBuf,
        /// Cache of mft index -> file kind (created on first run).
        #[arg(long)]
        kinds: PathBuf,
        #[arg(long, default_value_t = 6)]
        threads: usize,
        /// Only walk these source file ids (debugging).
        #[arg(long, value_delimiter = ',')]
        ids: Vec<u32>,
    },
}

const TEX_MAGICS: [&[u8; 4]; 6] = [b"ATEX", b"ATTX", b"ATEC", b"ATEP", b"ATEU", b"ATET"];

fn fourcc(b: &[u8]) -> String {
    b.iter().map(|&c| if c.is_ascii_graphic() { c as char } else { '.' }).collect()
}

fn survey(dat: &mut dat::Dat, out: &PathBuf) -> Result<()> {
    let atoc = dat.atoc()?;
    let mut fid_of: HashMap<usize, u32> = HashMap::new();
    for (&id, &mi) in &atoc {
        fid_of.entry(mi).and_modify(|v| *v = (*v).min(id)).or_insert(id);
    }
    // Visit in disk order for sequential reads.
    let mut order: Vec<usize> = (0..dat.entries.len()).filter(|&i| dat.entries[i].offset > 0).collect();
    order.sort_by_key(|&i| dat.entries[i].offset);

    let mut w = std::io::BufWriter::new(std::fs::File::create(out)?);
    let mut hist: BTreeMap<(String, u32), (u64, u64)> = BTreeMap::new();
    let (mut n, mut errs) = (0u64, 0u64);
    let t = std::time::Instant::now();
    for (k, &i) in order.iter().enumerate() {
        if k % 50000 == 0 {
            eprintln!("{k}/{} textures={n} errs={errs} {:.0}s", order.len(), t.elapsed().as_secs_f32());
        }
        let h = match dat.peek(i, 12) {
            Ok(h) if h.len() >= 12 => h,
            Ok(_) => continue,
            Err(_) => {
                errs += 1;
                continue;
            }
        };
        if !TEX_MAGICS.iter().any(|m| &h[0..4] == *m) {
            continue;
        }
        let kind = fourcc(&h[0..4]);
        let fmt = fourcc(&h[4..8]);
        let wdt = u16::from_le_bytes([h[8], h[9]]) as u32;
        let hgt = u16::from_le_bytes([h[10], h[11]]) as u32;
        let fid = fid_of.get(&i).copied().unwrap_or(0);
        let sz = dat.entries[i].size as u64;
        let rec = serde_json::json!({
            "mft": i, "file_id": fid, "kind": kind, "format": fmt,
            "width": wdt, "height": hgt, "dat_bytes": sz,
        });
        writeln!(w, "{rec}")?;
        let e = hist.entry((kind, wdt.max(hgt))).or_default();
        e.0 += 1;
        e.1 += sz;
        n += 1;
    }
    w.flush()?;
    eprintln!("done: {n} textures, {errs} unreadable entries, {:.0}s", t.elapsed().as_secs_f32());
    let histogram: Vec<_> = hist
        .into_iter()
        .map(|((kind, side), (c, b))| serde_json::json!({"kind": kind, "max_side": side, "count": c, "dat_bytes": b}))
        .collect();
    let summary = serde_json::json!({
        "textures": n, "unreadable_entries": errs, "seconds": t.elapsed().as_secs_f64(), "histogram": histogram,
    });
    write_summary(out, &summary)
}

/// Writes `<stem>_summary.json` next to a JSONL output.
pub fn write_summary(out: &std::path::Path, v: &serde_json::Value) -> Result<()> {
    let stem = out.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "out".into());
    let p = out.with_file_name(format!("{stem}_summary.json"));
    std::fs::write(&p, serde_json::to_string_pretty(v)?)?;
    eprintln!("summary: {}", p.display());
    Ok(())
}

fn extract(dat: &mut dat::Dat, out: &PathBuf, mut ids: Vec<u32>, list: Option<PathBuf>) -> Result<()> {
    if let Some(l) = list {
        // JSONL (`file_id` per line); legacy survey CSV (column 2) still accepted.
        for line in std::fs::read_to_string(l)?.lines() {
            let line = line.trim();
            let id = if line.starts_with('{') {
                serde_json::from_str::<serde_json::Value>(line).ok().and_then(|v| v["file_id"].as_u64()).map(|v| v as u32)
            } else {
                line.split(',').nth(1).and_then(|v| v.parse().ok())
            };
            if let Some(id) = id {
                ids.push(id);
            }
        }
    }
    std::fs::create_dir_all(out)?;
    let atoc = dat.atoc()?;
    let (mut ok, mut bad) = (0u32, 0u32);
    let mut log = std::io::BufWriter::new(std::fs::File::create(out.join("extract_errors.txt"))?);
    for id in ids {
        let Some(&mi) = atoc.get(&id) else {
            writeln!(log, "{id}: not in ATOC")?;
            bad += 1;
            continue;
        };
        let r = dat.read(mi).and_then(|f| atex::decode(&f));
        match r {
            Ok(d) => {
                std::fs::write(out.join(format!("{id}.dds")), atex::to_dds(&d))?;
                ok += 1;
            }
            Err(e) => {
                writeln!(log, "{id}: {e}")?;
                bad += 1;
            }
        }
    }
    eprintln!("extracted {ok}, failed {bad}");
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Survey { out } => survey(&mut dat::Dat::open(&cli.dat)?, &out),
        Cmd::Extract { out, ids, list } => extract(&mut dat::Dat::open(&cli.dat)?, &out, ids, list),
        Cmd::Layout { exe, fcc } => models::dump_layout(&exe, &fcc),
        Cmd::Refs { out, exe, kinds, threads, ids } => refs::run(&cli.dat, &exe, &out, &kinds, threads, ids),
        Cmd::Models { out, exe, ids, index, threads } => {
            models::run(&mut dat::Dat::open(&cli.dat)?, &cli.dat, &exe, &out, ids, index, threads)
        }
    }
}
