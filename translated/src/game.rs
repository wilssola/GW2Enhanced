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
                    return Some(Self {
                        base,
                        text,
                        text_rva: rva,
                    });
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

/// Reads operands directly from the game's code, so as not to hardcode offsets
/// that a patch may change. Each search looks for the expected opcode in a window
/// starting at the beginning of the function; if not found, it returns `None` and
/// the feature that depends on it is disabled.
#[derive(Clone, Copy)]
pub struct Code {
    pub at: usize,
}

impl Code {
    /// Address of the `nth` occurrence (0-based) of `op` in `[at, at+window)`.
    pub fn find(&self, op: &[u8], window: usize, nth: usize) -> Option<usize> {
        let bytes = unsafe { std::slice::from_raw_parts(self.at as *const u8, window + op.len()) };
        bytes
            .windows(op.len())
            .enumerate()
            .filter(|(_, w)| *w == op)
            .nth(nth)
            .map(|(i, _)| self.at + i)
    }

    /// `disp8` right after the opcode.
    pub fn disp8(&self, op: &[u8], window: usize, nth: usize) -> Option<usize> {
        self.find(op, window, nth)
            .map(|a| unsafe { ((a + op.len()) as *const u8).read() } as usize)
    }

    /// `disp32` right after the opcode, as a field offset.
    pub fn disp32(&self, op: &[u8], window: usize, nth: usize) -> Option<usize> {
        let a = self.find(op, window, nth)?;
        let v = unsafe { ((a + op.len()) as *const i32).read_unaligned() };
        usize::try_from(v).ok()
    }

    /// RIP-relative target of the found `op disp32 [imm; tail]` instruction.
    pub fn rip(&self, op: &[u8], tail: usize, window: usize, nth: usize) -> Option<usize> {
        let a = self.find(op, window, nth)?;
        let v = unsafe { ((a + op.len()) as *const i32).read_unaligned() };
        Some((a as isize + (op.len() + 4 + tail) as isize + v as isize) as usize)
    }
}
