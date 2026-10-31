//! Minimal PE64 image loader: maps sections at their RVAs so absolute
//! pointers stored in `.rdata` can be followed (VA - ImageBase = RVA).

use anyhow::{bail, Result};
use std::path::Path;

pub struct Section {
    pub rva: u64,
    pub size: u64,
}

pub struct Image {
    pub mem: Vec<u8>,
    pub base: u64,
    pub text: Section,
    pub rdata: Section,
    pub data: Section,
}

fn u16le(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

impl Section {
    /// `va` lies inside the section (or is null), like GW2Viewer's `Valid`.
    pub fn valid(&self, base: u64, va: u64) -> bool {
        va == 0 || self.contains(base, va)
    }
    pub fn contains(&self, base: u64, va: u64) -> bool {
        va >= base + self.rva && va <= base + self.rva + self.size
    }
}

impl Image {
    pub fn load(path: &Path) -> Result<Self> {
        let f = std::fs::read(path)?;
        if f.len() < 0x400 || &f[0..2] != b"MZ" {
            bail!("not a PE file");
        }
        let nt = u32le(&f, 0x3c) as usize;
        if &f[nt..nt + 4] != b"PE\0\0" {
            bail!("bad PE signature");
        }
        let nsec = u16le(&f, nt + 6) as usize;
        let opt_size = u16le(&f, nt + 20) as usize;
        let opt = nt + 24;
        if u16le(&f, opt) != 0x20b {
            bail!("not PE32+");
        }
        let base = u64::from_le_bytes(f[opt + 24..opt + 32].try_into().unwrap());
        let size_of_image = u32le(&f, opt + 56) as usize;
        let mut mem = vec![0u8; size_of_image];
        let mut text = Section { rva: 0, size: 0 };
        let mut rdata = Section { rva: 0, size: 0 };
        let mut data = Section { rva: 0, size: 0 };
        let sh = opt + opt_size;
        for i in 0..nsec {
            let s = sh + i * 40;
            let name: Vec<u8> = f[s..s + 8].iter().copied().take_while(|&c| c != 0).collect();
            let vsize = u32le(&f, s + 8) as usize;
            let rva = u32le(&f, s + 12) as usize;
            let raw_size = u32le(&f, s + 16) as usize;
            let raw_ptr = u32le(&f, s + 20) as usize;
            let n = raw_size.min(vsize).min(f.len().saturating_sub(raw_ptr)).min(mem.len().saturating_sub(rva));
            mem[rva..rva + n].copy_from_slice(&f[raw_ptr..raw_ptr + n]);
            let sec = Section { rva: rva as u64, size: raw_size as u64 };
            match name.as_slice() {
                b".text" => text = sec,
                b".rdata" => rdata = sec,
                b".data" => data = sec,
                _ => {}
            }
        }
        if rdata.size == 0 {
            bail!("no .rdata section");
        }
        Ok(Self { mem, base, text, rdata, data })
    }

    pub fn off(&self, va: u64) -> Option<usize> {
        let o = va.checked_sub(self.base)? as usize;
        (o < self.mem.len()).then_some(o)
    }
    pub fn u16_at(&self, va: u64) -> Option<u16> {
        let o = self.off(va)?;
        Some(u16::from_le_bytes(self.mem.get(o..o + 2)?.try_into().ok()?))
    }
    pub fn u64_at(&self, va: u64) -> Option<u64> {
        let o = self.off(va)?;
        Some(u64::from_le_bytes(self.mem.get(o..o + 8)?.try_into().ok()?))
    }
    pub fn cstr(&self, va: u64) -> String {
        let Some(o) = self.off(va) else { return String::new() };
        let end = self.mem[o..].iter().position(|&c| c == 0).map_or(self.mem.len(), |p| o + p);
        String::from_utf8_lossy(&self.mem[o..end.min(o + 256)]).into_owned()
    }
}
