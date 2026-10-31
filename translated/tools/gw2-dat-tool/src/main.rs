//! `gw2-dat-tool`: `Gw2.dat` reader for GW2 Translated.
//!
//! Lists and extracts entries, reads the `TextPackManifest` and extracts the
//! text files (`strs`) of each language. Never writes to `Gw2.dat`.

mod compression;
mod dat;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// Known file_id of the `TextPackManifest`; if a patch changes it, use `find-type txtm`.
const MANIFEST: u32 = 3821681;

#[derive(Parser)]
#[command(name = "gw2-dat-tool", about = "Gw2.dat reader (read-only)")]
struct Cli {
    /// Path to Gw2.dat (the game must be closed)
    #[arg(short, long)]
    dat: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// File header
    Info,
    /// MFT entries with the type reported by the MFT
    List {
        #[arg(short, long)]
        filter: Option<String>,
    },
    /// Hexdump of the first (still compressed) bytes of an entry
    Dump {
        index: usize,
        #[arg(short, long, default_value = "64")]
        bytes: usize,
    },
    /// Decompresses an entry by MFT index
    Extract {
        index: usize,
        #[arg(short, long)]
        out: PathBuf,
    },
    /// Extracts the listed file_ids (one per line) to <out>/<fid>.bin
    ExtractIds {
        #[arg(long)]
        ids: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
    /// Decompresses entries to find their real type (slow)
    Scan {
        #[arg(short, long)]
        filter: Option<String>,
        #[arg(short, long)]
        limit: Option<usize>,
    },
    /// First entry whose real type is <fourcc> (skips those larger than --max-size)
    FindType {
        fourcc: String,
        #[arg(long, default_value_t = 1_000_000)]
        max_size: u32,
    },
    /// file_ids of a language's `strs` according to the `TextPackManifest`, one per line;
    /// line i covers text IDs i*1024 .. i*1024+1023
    TextFiles {
        #[arg(long, default_value_t = 0)]
        lang: usize,
        #[arg(long, default_value_t = MANIFEST)]
        manifest: u32,
        #[arg(long)]
        out: PathBuf,
    },
    /// Extracts all `strs` of a language to <out>/<fid>.bin
    ExtractText {
        #[arg(long, default_value_t = 0)]
        lang: usize,
        #[arg(long, default_value_t = MANIFEST)]
        manifest: u32,
        #[arg(long)]
        out: PathBuf,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut reader = dat::DatReader::open(&cli.dat)?;

    match cli.command {
        Command::Info => {
            let hdr = reader.header();
            println!("version     : {}", hdr.version);
            println!("chunk       : {}", hdr.chunk_size);
            println!("MFT offset  : {:#x}", hdr.mft_offset);
            println!("MFT size    : {} bytes", hdr.mft_size);
            println!("MFT entries : {}", reader.entry_count());
        }
        Command::List { filter } => {
            println!("{:>8}  {:>10}  {:>4}  {:>6}  type", "index", "offset", "cmp", "size");
            for (i, entry) in reader.entries().iter().enumerate() {
                let t = entry.file_type.as_deref().unwrap_or("????");
                if filter.as_ref().is_some_and(|f| !t.contains(f.as_str())) {
                    continue;
                }
                let cmp = if entry.compressed { "yes" } else { "no" };
                println!("{i:>8}  {:#010x}  {cmp:>4}  {:>6}  {t}", entry.offset, entry.size);
            }
        }
        Command::Dump { index, bytes } => {
            let entry = &reader.entries()[index];
            println!(
                "entry {index}: offset={:#x} size={} compressed={}",
                entry.offset, entry.size, entry.compressed
            );
            let data = reader.read_raw(index)?;
            for (i, chunk) in data[..bytes.min(data.len())].chunks(16).enumerate() {
                let hex: String = chunk.iter().map(|b| format!("{b:02x} ")).collect();
                let txt: String = chunk
                    .iter()
                    .map(|&b| if b.is_ascii_graphic() { b as char } else { '.' })
                    .collect();
                println!("  {:04x}  {hex:<48} |{txt}|", i * 16);
            }
        }
        Command::Extract { index, out } => {
            let data = reader.read_entry(index)?;
            std::fs::write(&out, &data)?;
            println!("{} bytes -> {}", data.len(), out.display());
        }
        Command::ExtractIds { ids, out } => {
            let fids: Vec<u32> = std::fs::read_to_string(&ids)?
                .lines()
                .filter_map(|l| l.trim().parse().ok())
                .collect();
            let (ok, miss) = extract_fids(&mut reader, &fids, &out)?;
            println!("extracted={ok} missing={miss} -> {}", out.display());
        }
        Command::Scan { filter, limit } => {
            for i in 0..limit.unwrap_or(usize::MAX).min(reader.entry_count()) {
                if let Some(t) = reader.read_entry_type(i)
                    && filter.as_ref().is_none_or(|f| t.contains(f.as_str()))
                {
                    println!("{i:>8}  {:>8}  {t}", reader.entries()[i].size);
                }
            }
        }
        Command::FindType { fourcc, max_size } => {
            let atoc = reader.read_atoc()?;
            let total = reader.entry_count();
            for i in 0..total {
                if i % 50_000 == 0 {
                    eprintln!("  {i}/{total}");
                }
                if reader.entries()[i].size <= max_size && reader.read_entry_type(i).as_deref() == Some(fourcc.as_str())
                {
                    let ids: Vec<u32> = atoc
                        .iter()
                        .filter(|(_, m)| **m as usize == i)
                        .map(|(f, _)| *f)
                        .collect();
                    println!(
                        "{fourcc}: index {i}, size {}, file_id(s) {ids:?}",
                        reader.entries()[i].size
                    );
                    return Ok(());
                }
            }
            println!("{fourcc} not found");
        }
        Command::TextFiles { lang, manifest, out } => {
            let files = text_files(&mut reader, manifest, lang)?;
            let body: String = files.iter().map(|f| format!("{f}\n")).collect();
            std::fs::write(&out, body)?;
            println!("language {lang}: {} files -> {}", files.len(), out.display());
        }
        Command::ExtractText { lang, manifest, out } => {
            let files = text_files(&mut reader, manifest, lang)?;
            let (ok, miss) = extract_fids(&mut reader, &files, &out)?;
            println!("language {lang}: extracted={ok} missing={miss} -> {}", out.display());
        }
    }
    Ok(())
}

/// Extracts each file_id to `<out>/<fid>.bin`; returns (extracted, missing).
fn extract_fids(reader: &mut dat::DatReader, fids: &[u32], out: &std::path::Path) -> Result<(usize, usize)> {
    std::fs::create_dir_all(out)?;
    let atoc = reader.read_atoc()?;
    let (mut ok, mut miss) = (0, 0);
    for fid in fids {
        match atoc.get(fid).map(|&m| reader.read_entry(m as usize)) {
            Some(Ok(d)) => {
                std::fs::write(out.join(format!("{fid}.bin")), &d)?;
                ok += 1;
            }
            Some(Err(e)) => {
                eprintln!("{fid}: read error: {e}");
                miss += 1;
            }
            None => {
                eprintln!("{fid}: not in the ATOC");
                miss += 1;
            }
        }
    }
    Ok((ok, miss))
}

/// Reads the `TextPackManifest` (PF/`txtm`) and returns the file_ids of a language.
/// Layout: `u32 stringsPerFile` at 0x1C; at 0x20 `{u32 count, u32 rel}` of the
/// languages (12 bytes each: `u32 count, u64 rel`); each file is an 8-byte
/// relative pointer to a 3×u16 `ANetFileReference`.
fn parse_text_manifest(d: &[u8], lang: usize) -> Result<Vec<u32>> {
    let u16_at = |o: usize| -> Result<u16> {
        Ok(u16::from_le_bytes(
            d.get(o..o + 2)
                .ok_or_else(|| anyhow::anyhow!("truncated manifest"))?
                .try_into()?,
        ))
    };
    let u32_at = |o: usize| -> Result<u32> {
        Ok(u32::from_le_bytes(
            d.get(o..o + 4)
                .ok_or_else(|| anyhow::anyhow!("truncated manifest"))?
                .try_into()?,
        ))
    };
    let i64_at = |o: usize| -> Result<i64> {
        Ok(i64::from_le_bytes(
            d.get(o..o + 8)
                .ok_or_else(|| anyhow::anyhow!("truncated manifest"))?
                .try_into()?,
        ))
    };
    if d.get(..2) != Some(b"PF") || d.get(8..12) != Some(b"txtm") {
        anyhow::bail!("not a TextPackManifest (PF/txtm)");
    }
    let nlang = u32_at(0x20)? as usize;
    let arr = 0x24 + u32_at(0x24)? as usize;
    if lang >= nlang {
        anyhow::bail!("language {lang} does not exist (there are {nlang})");
    }
    let e = arr + 12 * lang;
    let count = u32_at(e)? as usize;
    let base = (e as i64 + 4 + i64_at(e + 4)?) as usize;
    let mut out = Vec::with_capacity(count);
    for k in 0..count {
        let el = base + 8 * k;
        let rel = i64_at(el)?;
        if rel == 0 {
            out.push(0);
            continue;
        }
        let r = (el as i64 + rel) as usize;
        let (a, b) = (u16_at(r)? as u32, u16_at(r + 2)? as u32);
        out.push((a - 0x100) + 0xFF00 * (b - 0x100) + 1);
    }
    Ok(out)
}

fn text_files(reader: &mut dat::DatReader, manifest: u32, lang: usize) -> Result<Vec<u32>> {
    let atoc = reader.read_atoc()?;
    let idx = match atoc.get(&manifest) {
        Some(&m) => m as usize,
        None => anyhow::bail!("manifest {manifest} is not in the ATOC; run `find-type txtm` to find the new file_id"),
    };
    parse_text_manifest(&reader.read_entry(idx)?, lang)
}

#[cfg(test)]
mod manifest_tests {
    /// Checks against the manifest extracted to `re/txtm/` (outside git), when present.
    #[test]
    fn parses_extracted_manifest() {
        let p = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../re/txtm/3821681.bin");
        let Ok(d) = std::fs::read(p) else { return };
        let en = super::parse_text_manifest(&d, 0).unwrap();
        assert_eq!(en.len(), 1176);
        assert_eq!(&en[..2], &[2440724, 2440725]);
        assert!(super::parse_text_manifest(&d, 1).unwrap().is_empty());
        assert_eq!(super::parse_text_manifest(&d, 4).unwrap()[0], 2443700);
    }
}
