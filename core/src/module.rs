//! A view of `Gw2-64.exe`: its code section, its data sections (for strings) and a fingerprint of the build.
//!
//! The same view is built from the running process (the mod) or from a file (the patch-day tool, the tests).

#[derive(Debug, Clone, Copy)]
struct Section {
    name: [u8; 8],
    vsize: u32,
    rva: u32,
    raw_size: u32,
    raw_ptr: u32,
    characteristics: u32,
}

const EXEC: u32 = 0x2000_0000;

pub struct Module<'a> {
    /// The `.text` section as mapped.
    pub text: &'a [u8],
    pub text_rva: u32,
    /// Every other non-executable section as `(rva, bytes)`: where strings live.
    pub data: Vec<(u32, &'a [u8])>,
    pub timestamp: u32,
    pub size_of_image: u32,
}

fn rd32(b: &[u8], o: usize) -> Option<u32> {
    b.get(o..o + 4).map(|s| u32::from_le_bytes(s.try_into().unwrap()))
}

fn rd16(b: &[u8], o: usize) -> Option<u16> {
    b.get(o..o + 2).map(|s| u16::from_le_bytes(s.try_into().unwrap()))
}

struct Headers {
    timestamp: u32,
    size_of_image: u32,
    sections: Vec<Section>,
}

fn parse_headers(h: &[u8]) -> Option<Headers> {
    if h.get(..2)? != b"MZ" {
        return None;
    }
    let pe = rd32(h, 0x3C)? as usize;
    if h.get(pe..pe + 4)? != b"PE\0\0" || rd16(h, pe + 24)? != 0x20B {
        return None; // 64-bit images only
    }
    let nsec = rd16(h, pe + 6)? as usize;
    let timestamp = rd32(h, pe + 8)?;
    let opt_size = rd16(h, pe + 20)? as usize;
    let size_of_image = rd32(h, pe + 24 + 56)?;
    let first = pe + 24 + opt_size;
    let mut sections = Vec::with_capacity(nsec);
    for i in 0..nsec {
        let s = first + 40 * i;
        let mut name = [0u8; 8];
        name.copy_from_slice(h.get(s..s + 8)?);
        sections.push(Section {
            name,
            vsize: rd32(h, s + 8)?,
            rva: rd32(h, s + 12)?,
            raw_size: rd32(h, s + 16)?,
            raw_ptr: rd32(h, s + 20)?,
            characteristics: rd32(h, s + 36)?,
        });
    }
    Some(Headers {
        timestamp,
        size_of_image,
        sections,
    })
}

impl<'a> Module<'a> {
    /// From the bytes of the `.exe` file on disk.
    pub fn from_file_bytes(data: &'a [u8]) -> Option<Self> {
        let h = parse_headers(data)?;
        let slice = |s: &Section| -> Option<&'a [u8]> {
            let len = s.vsize.min(s.raw_size) as usize;
            data.get(s.raw_ptr as usize..s.raw_ptr as usize + len)
        };
        Self::assemble(&h, slice)
    }

    /// From the executable already loaded in this process.
    ///
    /// # Safety
    /// `base` must be the base address of a mapped 64-bit PE image that stays mapped for `'a`.
    pub unsafe fn from_memory(base: usize) -> Option<Self> {
        let head = unsafe { std::slice::from_raw_parts(base as *const u8, 0x1000) };
        let h = parse_headers(head)?;
        let slice = |s: &Section| -> Option<&'a [u8]> {
            Some(unsafe { std::slice::from_raw_parts((base + s.rva as usize) as *const u8, s.vsize as usize) })
        };
        Self::assemble(&h, slice)
    }

    /// The main executable of this process: `(base address, view)`.
    #[cfg(windows)]
    pub fn current() -> Option<(usize, Module<'static>)> {
        use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
        let base = unsafe { GetModuleHandleW(std::ptr::null()) } as usize;
        if base == 0 {
            return None;
        }
        unsafe { Module::from_memory(base).map(|m| (base, m)) }
    }

    fn assemble(h: &Headers, slice: impl Fn(&Section) -> Option<&'a [u8]>) -> Option<Self> {
        let text_sec = h.sections.iter().find(|s| s.name.starts_with(b".text"))?;
        let text = slice(text_sec)?;
        let data = h
            .sections
            .iter()
            .filter(|s| s.characteristics & EXEC == 0)
            .filter_map(|s| slice(s).map(|b| (s.rva, b)))
            .collect();
        Some(Self {
            text,
            text_rva: text_sec.rva,
            data,
            timestamp: h.timestamp,
            size_of_image: h.size_of_image,
        })
    }

    /// Identifies the build: changes whenever the code does. Used to key the cache of resolved addresses.
    pub fn fingerprint(&self) -> u64 {
        let mut h: u64 = 0xcbf29ce484222325;
        for &b in self.text {
            h = (h ^ b as u64).wrapping_mul(0x100000001b3);
        }
        h ^ ((self.size_of_image as u64) << 32) ^ self.timestamp as u64
    }

    /// Bytes of the code at `rva` (up to `len`), if inside `.text`.
    pub fn code_at(&self, rva: u32, len: usize) -> Option<&'a [u8]> {
        let off = rva.checked_sub(self.text_rva)? as usize;
        let end = (off + len).min(self.text.len());
        (off < self.text.len()).then(|| &self.text[off..end])
    }

    pub fn contains_code(&self, rva: u32) -> bool {
        rva >= self.text_rva && ((rva - self.text_rva) as usize) < self.text.len()
    }
}

#[cfg(test)]
pub(crate) mod testutil {
    /// A tiny valid PE64 image: `.text` with `code` and `.rdata` with `rdata`. RVAs: text 0x1000, rdata 0x3000.
    pub fn fake_pe(code: &[u8], rdata: &[u8]) -> Vec<u8> {
        let mut f = vec![0u8; 0x400];
        f[0..2].copy_from_slice(b"MZ");
        f[0x3C..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        f[0x80..0x84].copy_from_slice(b"PE\0\0");
        f[0x80 + 4..0x80 + 6].copy_from_slice(&0x8664u16.to_le_bytes());
        f[0x80 + 6..0x80 + 8].copy_from_slice(&2u16.to_le_bytes()); // sections
        f[0x80 + 8..0x80 + 12].copy_from_slice(&0x1234u32.to_le_bytes()); // timestamp
        f[0x80 + 20..0x80 + 22].copy_from_slice(&0xF0u16.to_le_bytes()); // optional header size
        let opt = 0x80 + 24;
        f[opt..opt + 2].copy_from_slice(&0x20Bu16.to_le_bytes());
        f[opt + 56..opt + 60].copy_from_slice(&0x5000u32.to_le_bytes());
        let first = opt + 0xF0;
        let mut put = |i: usize, name: &[u8], rva: u32, len: usize, raw: u32, ch: u32| {
            let s = first + 40 * i;
            f[s..s + name.len()].copy_from_slice(name);
            f[s + 8..s + 12].copy_from_slice(&(len as u32).to_le_bytes());
            f[s + 12..s + 16].copy_from_slice(&rva.to_le_bytes());
            f[s + 16..s + 20].copy_from_slice(&(len as u32).to_le_bytes());
            f[s + 20..s + 24].copy_from_slice(&raw.to_le_bytes());
            f[s + 36..s + 40].copy_from_slice(&ch.to_le_bytes());
        };
        put(0, b".text", 0x1000, code.len(), 0x400, 0x6000_0020);
        put(
            1,
            b".rdata",
            0x3000,
            rdata.len(),
            0x400 + code.len() as u32,
            0x4000_0040,
        );
        f.extend_from_slice(code);
        f.extend_from_slice(rdata);
        f
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_sections_from_file_bytes() {
        let f = testutil::fake_pe(&[0xCC, 0x55, 0xC3], b"hello\0");
        let m = Module::from_file_bytes(&f).unwrap();
        assert_eq!((m.text, m.text_rva), (&[0xCC, 0x55, 0xC3][..], 0x1000));
        assert_eq!(m.data.len(), 1);
        assert_eq!(m.data[0], (0x3000, &b"hello\0"[..]));
        assert_eq!(m.timestamp, 0x1234);
        assert_eq!(m.code_at(0x1001, 10), Some(&[0x55, 0xC3][..]));
        assert!(m.contains_code(0x1002) && !m.contains_code(0x1003));
    }

    #[test]
    fn fingerprint_follows_the_code() {
        let a = testutil::fake_pe(&[1, 2, 3], b"x");
        let b = testutil::fake_pe(&[1, 2, 4], b"x");
        let (a, b) = (
            Module::from_file_bytes(&a).unwrap(),
            Module::from_file_bytes(&b).unwrap(),
        );
        assert_ne!(a.fingerprint(), b.fingerprint());
        assert_eq!(a.fingerprint(), a.fingerprint());
    }

    #[test]
    fn rejects_non_pe() {
        assert!(Module::from_file_bytes(b"not a pe").is_none());
        assert!(Module::from_file_bytes(&[0u8; 0x200]).is_none());
    }
}
