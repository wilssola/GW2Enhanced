//! The in-memory image of `Gw2-64.exe`: base, code section and signature search.

use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;

use crate::sig::{Pattern, Site};

pub struct Image {
    pub base: usize,
    text: &'static [u8],
    text_rva: usize,
}

impl Image {
    /// Reads the PE headers of the already-loaded main executable.
    pub fn current() -> Option<Self> {
        let base = unsafe { GetModuleHandleW(std::ptr::null()) } as usize;
        if base == 0 {
            return None;
        }
        unsafe {
            let rd32 = |o: usize| (base as *const u8).add(o).cast::<u32>().read_unaligned() as usize;
            let rd16 = |o: usize| (base as *const u8).add(o).cast::<u16>().read_unaligned() as usize;
            let pe = rd32(0x3C);
            let nsec = rd16(pe + 6);
            let opt_size = rd16(pe + 20);
            let first = pe + 24 + opt_size;
            for i in 0..nsec {
                let s = first + 40 * i;
                let name = std::slice::from_raw_parts((base + s) as *const u8, 8);
                if name.starts_with(b".text") {
                    let (vsize, rva) = (rd32(s + 8), rd32(s + 12));
                    let text = std::slice::from_raw_parts((base + rva) as *const u8, vsize);
                    return Some(Self { base, text, text_rva: rva });
                }
            }
        }
        None
    }

    /// Absolute address of the site, if the signature matches exactly once.
    pub fn find(&self, site: &Site) -> Option<usize> {
        let p = Pattern::parse(site.sig)?;
        p.find_unique(self.text).map(|o| self.base + self.text_rva + o)
    }

    pub fn rva(&self, addr: usize) -> usize {
        addr - self.base
    }
}
