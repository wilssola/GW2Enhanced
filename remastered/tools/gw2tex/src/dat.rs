//! Read-only Gw2.dat access. Opened with shared read so it never blocks
//! other readers; keep sessions short because the game opens the file
//! exclusively and fails to start while another process holds it.

use anyhow::{anyhow, bail, Result};
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::compression::{decompress_gw2, decompress_gw2_limit};

const RAW_BLOCK_DATA: usize = 65532;
const RAW_BLOCK_TOTAL: usize = 65536;

#[derive(Clone, Copy, Debug)]
pub struct MftEntry {
    pub offset: u64,
    pub size: u32,
    pub compressed: bool,
}

pub struct Dat {
    file: File,
    pub entries: Vec<MftEntry>,
}

fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn u64le(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

impl Dat {
    pub fn open(path: &Path) -> Result<Self> {
        let mut file = File::open(path)?;
        let mut h = [0u8; 40];
        file.read_exact(&mut h)?;
        if &h[1..4] != b"AN\x1a" {
            bail!("not a Gw2.dat (bad magic)");
        }
        let mft_offset = u64le(&h, 24);
        let mft_size = u32le(&h, 32) as usize;
        file.seek(SeekFrom::Start(mft_offset))?;
        let mut mft = vec![0u8; mft_size];
        file.read_exact(&mut mft)?;
        if &mft[0..4] != b"Mft\x1a" {
            bail!("bad MFT magic");
        }
        let n = u32le(&mft, 12) as usize;
        let mut entries = Vec::with_capacity(n);
        // Entry 0 is the MFT header itself (24 bytes, same stride).
        for i in 1..n {
            let o = i * 24;
            if o + 24 > mft.len() {
                break;
            }
            entries.push(MftEntry {
                offset: u64le(&mft, o),
                size: u32le(&mft, o + 8),
                compressed: u16::from_le_bytes([mft[o + 12], mft[o + 13]]) == 8,
            });
        }
        Ok(Self { file, entries })
    }

    fn raw(&mut self, idx: usize, max: usize) -> Result<Vec<u8>> {
        let e = *self.entries.get(idx).ok_or_else(|| anyhow!("mft index {idx} out of range"))?;
        let n = (e.size as usize).min(max);
        self.file.seek(SeekFrom::Start(e.offset))?;
        let mut v = vec![0u8; n];
        self.file.read_exact(&mut v)?;
        Ok(v)
    }

    /// Full decompressed content of an MFT entry.
    pub fn read(&mut self, idx: usize) -> Result<Vec<u8>> {
        let e = self.entries[idx];
        let raw = self.raw(idx, usize::MAX)?;
        if e.compressed { decompress_gw2(&raw) } else { Ok(strip_crc(&raw)) }
    }

    /// First `n` decompressed bytes (cheap header peek).
    // Used by gw2tex; texhash shares this module and does not call it.
    #[allow(dead_code)]
    pub fn peek(&mut self, idx: usize, n: usize) -> Result<Vec<u8>> {
        let e = self.entries[idx];
        if e.offset == 0 || e.size < 8 {
            bail!("empty entry");
        }
        if !e.compressed {
            let mut v = self.raw(idx, n)?;
            v.truncate(n);
            return Ok(v);
        }
        // Huffman tables live at the start of each block; 32 KiB covers them.
        let raw = self.raw(idx, 32 * 1024)?;
        match decompress_gw2_limit(&raw, n) {
            Ok(v) => Ok(v),
            Err(_) => {
                let raw = self.raw(idx, usize::MAX)?;
                decompress_gw2_limit(&raw, n)
            }
        }
    }

    /// file id -> mft index (from the ATOC, entry 1). base ids only.
    #[allow(dead_code)]
    pub fn atoc(&mut self) -> Result<HashMap<u32, usize>> {
        let raw = self.raw(1, usize::MAX)?;
        let mut m = HashMap::new();
        for c in raw.chunks_exact(8) {
            let id = u32le(c, 0);
            let mi = u32le(c, 4);
            if id != 0 && mi > 0 {
                m.insert(id, mi as usize - 1);
            }
        }
        Ok(m)
    }
}

fn strip_crc(data: &[u8]) -> Vec<u8> {
    if data.len() <= RAW_BLOCK_DATA {
        return data.to_vec();
    }
    let mut out = Vec::with_capacity(data.len());
    let mut pos = 0;
    while pos < data.len() {
        let end = (pos + RAW_BLOCK_DATA).min(data.len());
        out.extend_from_slice(&data[pos..end]);
        pos += RAW_BLOCK_TOTAL;
    }
    out
}
