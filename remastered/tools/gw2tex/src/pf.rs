//! PackFile ("PF") parsing, ported from GW2Viewer (Data/Pack/*).
//!
//! Chunk layouts are versioned and only exist inside the game client, so
//! they are recovered by scanning `.rdata` of Gw2-64.exe for the
//! `{fourcc, numVersions, PackFileVersion*}` tables (Manager.cpp).
//! Traversal follows the packed (#pragma pack 1) PF encoding: pointers are
//! self-relative signed offsets, arrays are `{count, ptr}`.

use crate::pe::Image;
use anyhow::{anyhow, bail, Result};
use std::collections::HashMap;

// UnderlyingTypes (PackFile-Layout.ixx)
pub const T_STRUCT: u16 = 0;
pub const T_INLINE_ARRAY: u16 = 1;
pub const T_DWORD_ARRAY: u16 = 2;
pub const T_DWORD_PTR_ARRAY: u16 = 3;
pub const T_DWORD_TYPED_ARRAY: u16 = 4;
pub const T_DWORD: u16 = 10;
pub const T_FILENAME: u16 = 11;
pub const T_PTR: u16 = 16;
pub const T_QWORD: u16 = 17;
pub const T_WSTRING: u16 = 18;
pub const T_STRING: u16 = 19;
pub const T_INLINE_STRUCT: u16 = 20;
pub const T_WORD: u16 = 21;
pub const T_FILENAME2: u16 = 27;
pub const T_VARIANT: u16 = 28;
pub const T_INLINE_STRUCT2: u16 = 29;
pub const T_WORD_ARRAY: u16 = 30;
pub const T_WORD_PTR_ARRAY: u16 = 31;
pub const T_WORD_TYPED_ARRAY: u16 = 32;
pub const T_BYTE_ARRAY: u16 = 33;
pub const T_BYTE_PTR_ARRAY: u16 = 34;
pub const T_BYTE_TYPED_ARRAY: u16 = 35;
pub const T_DWORD_ID: u16 = 36;
pub const T_QWORD_ID: u16 = 37;
const T_MAX: u16 = 38;

const PRIM_SIZES: [u32; T_MAX as usize] =
    [0, 0, 0, 0, 0, 1, 4, 8, 16, 24, 4, 0, 4, 8, 12, 16, 0, 8, 0, 0, 0, 2, 16, 3, 8, 16, 6, 0, 0, 0, 0, 0, 0, 0, 0, 0, 4, 8];

pub const TYPE_NAMES: [&str; T_MAX as usize] = [
    "StructDefinition", "InlineArray", "DwordArray", "DwordPtrArray", "DwordTypedArray", "Byte", "Byte4", "Double",
    "Double2", "Double3", "Dword", "FileName", "Float", "Float2", "Float3", "Float4", "Ptr", "Qword", "WString",
    "String", "InlineStruct", "Word", "Byte16", "Byte3", "Dword2", "Dword4", "Word3", "FileName2", "Variant",
    "InlineStruct2", "WordArray", "WordPtrArray", "WordTypedArray", "ByteArray", "BytePtrArray", "ByteTypedArray",
    "DwordID", "QwordID",
];

#[derive(Debug)]
pub struct Field {
    pub name: String,
    pub ut: u16,
    pub rt: u16,
    /// InlineArray length / variant count.
    pub array_size: u32,
    pub elem: Option<usize>,
    pub variants: Vec<Option<usize>>,
}

#[derive(Debug)]
pub struct Type {
    pub name: String,
    pub fields: Vec<Field>,
}

#[derive(Default)]
pub struct Layout {
    pub types: Vec<Type>,
    /// fourcc -> version -> candidate types (first = what GW2Viewer picks).
    /// Several pack formats share chunk fourccs (e.g. SKEL in MODL and CSCN).
    pub chunks: HashMap<[u8; 4], HashMap<u32, Vec<usize>>>,
    /// Precomputed [x86, x64] sizes per type (filled by `from_exe`).
    sizes: Vec<[u32; 2]>,
}

fn alnum(c: u8) -> bool {
    c.is_ascii_alphanumeric()
}

impl Layout {
    /// Port of `Manager::Load` (scan .rdata of the client exe).
    pub fn from_exe(img: &Image) -> Layout {
        let mut lay = Layout::default();
        let mut cache: HashMap<u64, Option<usize>> = HashMap::new();
        let start = img.base + img.rdata.rva;
        let end = start + img.rdata.size;
        let mut p = start;
        while p + 16 <= end {
            let o = img.off(p).unwrap();
            let b = &img.mem[o..o + 16];
            if alnum(b[0]) && alnum(b[1]) && alnum(b[2]) && (b[3] == 0 || alnum(b[3])) {
                let nver = u32::from_le_bytes(b[4..8].try_into().unwrap());
                let vers = u64::from_le_bytes(b[8..16].try_into().unwrap());
                if nver != 0 && nver <= 100 && vers != 0 && img.rdata.contains(img.base, vers) {
                    let fcc0: [u8; 4] = b[0..4].try_into().unwrap();
                    for v in 0..nver as u64 {
                        let mut fcc = fcc0;
                        let va = vers + v * 24;
                        let (Some(fields), Some(post)) = (img.u64_at(va), img.u64_at(va + 8)) else { break };
                        if fields == 0 {
                            continue;
                        }
                        if !img.rdata.valid(img.base, fields) || !img.text.valid(img.base, post) {
                            break;
                        }
                        match lay.collect(img, fields, &mut cache, 0) {
                            Some(t) => {
                                // Ambiguous "Main" chunks are keyed by the file
                                // fourcc they belong to (Manager.cpp).
                                if &fcc == b"Main" {
                                    match lay.types[t].name.as_str() {
                                        "PackMapMetadata" => fcc = *b"mMet",
                                        "PackContent" => fcc = *b"cntc",
                                        _ => {}
                                    }
                                }
                                let c =lay.chunks.entry(fcc).or_default().entry(v as u32).or_default();
                                if !c.contains(&t) {
                                    c.push(t);
                                }
                            }
                            None => break,
                        }
                    }
                }
            }
            p += 8;
        }
        lay.sizes = (0..lay.types.len()).map(|t| [lay.calc_size(t, false, 0), lay.calc_size(t, true, 0)]).collect();
        lay
    }

    fn collect(&mut self, img: &Image, fields: u64, cache: &mut HashMap<u64, Option<usize>>, depth: u32) -> Option<usize> {
        if fields == 0 || depth > 64 {
            return None;
        }
        if let Some(&c) = cache.get(&fields) {
            return c;
        }
        let rd = &img.rdata;
        let dt = &img.data;
        let mut n = 0u64;
        loop {
            if n > 4096 {
                return None;
            }
            let f = fields + n * 32;
            let ut = img.u16_at(f)?;
            let name = img.u64_at(f + 8)?;
            let elem = img.u64_at(f + 16)?;
            if !rd.valid(img.base, name) {
                return None;
            }
            if ut != T_STRUCT && !rd.valid(img.base, elem) && !dt.valid(img.base, elem) {
                return None;
            }
            if ut == T_STRUCT {
                break;
            }
            n += 1;
        }
        // Reserve the slot first so self-referencing layouts terminate.
        let term = fields + n * 32;
        let idx = self.types.len();
        self.types.push(Type {
            name: img.cstr(img.u64_at(term + 8)?),
            fields: Vec::new(),
        });
        cache.insert(fields, Some(idx));
        let mut out = Vec::with_capacity(n as usize);
        for i in 0..n {
            let f = fields + i * 32;
            let ut = img.u16_at(f)?;
            let rt = img.u16_at(f + 2)?;
            let name = img.cstr(img.u64_at(f + 8)?);
            let elem = img.u64_at(f + 16)?;
            let size = img.u16_at(f + 24)? as u32;
            let mut fl = Field { name, ut, rt, array_size: size, elem: None, variants: Vec::new() };
            if ut == T_VARIANT {
                for k in 0..size as u64 {
                    let vp = img.u64_at(elem + k * 8).unwrap_or(0);
                    fl.variants.push(self.collect(img, vp, cache, depth + 1));
                }
            } else {
                fl.elem = self.collect(img, elem, cache, depth + 1);
            }
            out.push(fl);
        }
        self.types[idx].fields = out;
        Some(idx)
    }

    /// Layout type for a chunk; prefers a candidate whose type name starts
    /// with `prefix` (e.g. "ModelFile"), else the first one found.
    pub fn chunk_type(&self, fcc: &[u8; 4], version: u32, prefix: &str) -> Option<usize> {
        let c = self.chunks.get(fcc)?.get(&version)?;
        c.iter().copied().find(|&t| self.types[t].name.starts_with(prefix)).or_else(|| c.first().copied())
    }

    pub fn type_size(&self, t: usize, x64: bool) -> u32 {
        match self.sizes.get(t) {
            Some(s) => s[x64 as usize],
            None => self.calc_size(t, x64, 0),
        }
    }

    fn calc_size(&self, t: usize, x64: bool, depth: u32) -> u32 {
        if depth > 32 {
            return 0;
        }
        self.types[t].fields.iter().map(|f| self.field_size_d(f, x64, depth + 1)).sum()
    }

    pub fn field_size(&self, f: &Field, x64: bool) -> u32 {
        self.field_size_d(f, x64, 0)
    }

    fn field_size_d(&self, f: &Field, x64: bool, depth: u32) -> u32 {
        let p = if x64 { 8 } else { 4 };
        if let Some(&s) = PRIM_SIZES.get(f.ut as usize) {
            if s != 0 {
                return s;
            }
        }
        let el = |f: &Field| f.elem.map_or(0, |e| if self.sizes.is_empty() { self.calc_size(e, x64, depth) } else { self.type_size(e, x64) });
        match f.ut {
            T_INLINE_ARRAY => el(f) * f.array_size,
            T_DWORD_ARRAY | T_DWORD_PTR_ARRAY => 4 + p,
            T_WORD_ARRAY | T_WORD_PTR_ARRAY => 2 + p,
            T_BYTE_ARRAY | T_BYTE_PTR_ARRAY => 1 + p,
            T_DWORD_TYPED_ARRAY => 4 + 2 * p,
            T_WORD_TYPED_ARRAY => 2 + 2 * p,
            T_BYTE_TYPED_ARRAY => 1 + 2 * p,
            T_FILENAME | T_FILENAME2 | T_PTR | T_WSTRING | T_STRING => p,
            T_INLINE_STRUCT | T_INLINE_STRUCT2 => el(f),
            T_VARIANT => 4 + p,
            _ => 0,
        }
    }

    /// Human-readable dump of a type tree (debugging aid).
    pub fn dump(&self, t: usize, indent: usize, seen: &mut Vec<usize>, out: &mut String) {
        use std::fmt::Write;
        let ty = &self.types[t];
        if seen.contains(&t) || indent > 12 {
            let _ = writeln!(out, "{:w$}<{} ...>", "", ty.name, w = indent * 2);
            return;
        }
        seen.push(t);
        for f in &ty.fields {
            let tn = TYPE_NAMES.get(f.ut as usize).copied().unwrap_or("?");
            let en = f.elem.map(|e| self.types[e].name.as_str()).unwrap_or("");
            let _ = writeln!(out, "{:w$}{} : {}{} {} (rt={})", "", f.name, tn,
                if f.ut == T_INLINE_ARRAY { format!("[{}]", f.array_size) } else { String::new() }, en, f.rt, w = indent * 2);
            if let Some(e) = f.elem {
                if !matches!(self.types[e].fields.first().map(|x| x.ut), Some(u) if self.types[e].fields.len() == 1 && PRIM_SIZES.get(u as usize).copied().unwrap_or(0) != 0) {
                    self.dump(e, indent + 1, seen, out);
                }
            }
            for v in f.variants.iter().flatten() {
                self.dump(*v, indent + 1, seen, out);
            }
        }
        seen.pop();
    }
}

// ---------------------------------------------------------------- PackFile

pub struct Chunk {
    pub fcc: [u8; 4],
    pub version: u32,
    /// Offset of chunk data (after the 16-byte header) in the file buffer.
    pub data: usize,
    pub end: usize,
}

pub struct PackFile<'a> {
    pub buf: &'a [u8],
    pub x64: bool,
    pub fcc: [u8; 4],
    pub chunks: Vec<Chunk>,
}

impl<'a> PackFile<'a> {
    pub fn parse(buf: &'a [u8]) -> Result<Self> {
        if buf.len() < 12 || &buf[0..2] != b"PF" || u16::from_le_bytes([buf[6], buf[7]]) != 12 {
            bail!("not a PackFile");
        }
        let flags = u16::from_le_bytes([buf[2], buf[3]]);
        let x64 = flags & 4 != 0;
        let fcc: [u8; 4] = buf[8..12].try_into().unwrap();
        let mut chunks = Vec::new();
        let mut p = 12usize;
        while p + 16 <= buf.len() {
            let magic: [u8; 4] = buf[p..p + 4].try_into().unwrap();
            let next = u32::from_le_bytes(buf[p + 4..p + 8].try_into().unwrap()) as usize;
            let version = u16::from_le_bytes([buf[p + 8], buf[p + 9]]) as u32;
            let end = (p + 8).checked_add(next).ok_or_else(|| anyhow!("chunk overflow"))?;
            if end > buf.len() || end < p + 16 {
                bail!("bad chunk size at {p}");
            }
            chunks.push(Chunk { fcc: magic, version, data: p + 16, end });
            p = end;
        }
        Ok(Self { buf, x64, fcc, chunks })
    }

    pub fn chunk(&self, fcc: &[u8; 4]) -> Option<&Chunk> {
        self.chunks.iter().find(|c| &c.fcc == fcc)
    }
}

/// A struct instance: offset into the PackFile buffer + its layout type.
#[derive(Clone, Copy, Debug)]
pub struct Node {
    pub off: usize,
    pub ty: usize,
}

/// Read helpers over a PackFile buffer with a layout. All bounds-checked.
pub struct Reader<'a> {
    pub buf: &'a [u8],
    pub x64: bool,
    pub lay: &'a Layout,
}

impl<'a> Reader<'a> {
    fn bytes(&self, off: usize, n: usize) -> Result<&'a [u8]> {
        self.buf.get(off..off.checked_add(n).ok_or_else(|| anyhow!("overflow"))?).ok_or_else(|| anyhow!("read out of bounds at {off}+{n}"))
    }
    pub fn u8(&self, off: usize) -> Result<u8> {
        Ok(self.bytes(off, 1)?[0])
    }
    pub fn u16(&self, off: usize) -> Result<u16> {
        Ok(u16::from_le_bytes(self.bytes(off, 2)?.try_into().unwrap()))
    }
    pub fn u32(&self, off: usize) -> Result<u32> {
        Ok(u32::from_le_bytes(self.bytes(off, 4)?.try_into().unwrap()))
    }
    pub fn u64(&self, off: usize) -> Result<u64> {
        Ok(u64::from_le_bytes(self.bytes(off, 8)?.try_into().unwrap()))
    }
    fn psize(&self) -> usize {
        if self.x64 { 8 } else { 4 }
    }
    /// Self-relative pointer at `off`; None for null.
    pub fn ptr(&self, off: usize) -> Result<Option<usize>> {
        let d: i64 = if self.x64 { self.u64(off)? as i64 } else { self.u32(off)? as i32 as i64 };
        if d == 0 {
            return Ok(None);
        }
        let t = off as i64 + d;
        if t < 0 || t as usize >= self.buf.len() {
            bail!("pointer out of bounds at {off}");
        }
        Ok(Some(t as usize))
    }

    pub fn field(&self, n: Node, name: &str) -> Result<(usize, &'a Field)> {
        let ty = &self.lay.types[n.ty];
        let mut off = n.off;
        for f in &ty.fields {
            if f.name == name {
                return Ok((off, f));
            }
            off += self.lay.field_size(f, self.x64) as usize;
        }
        bail!("field '{name}' not in {}", ty.name)
    }
    pub fn has(&self, n: Node, name: &str) -> bool {
        self.lay.types[n.ty].fields.iter().any(|f| f.name == name)
    }

    /// Struct reached through an InlineStruct / Ptr / Variant field.
    pub fn sub(&self, n: Node, name: &str) -> Result<Option<Node>> {
        let (off, f) = self.field(n, name)?;
        match f.ut {
            T_INLINE_STRUCT | T_INLINE_STRUCT2 => Ok(f.elem.map(|ty| Node { off, ty })),
            T_PTR => Ok(match (self.ptr(off)?, f.elem) {
                (Some(t), Some(ty)) => Some(Node { off: t, ty }),
                _ => None,
            }),
            T_VARIANT => {
                let idx = self.u32(off)? as usize;
                let tgt = self.ptr(off + 4)?;
                Ok(match (tgt, f.variants.get(idx).copied().flatten()) {
                    (Some(t), Some(ty)) => Some(Node { off: t, ty }),
                    _ => None,
                })
            }
            _ => bail!("field '{name}' is not a struct ({})", f.ut),
        }
    }

    /// Elements of an array field (pointer arrays are dereferenced; null
    /// entries are None). Each element is a Node of the element type.
    pub fn array(&self, n: Node, name: &str) -> Result<Vec<Option<Node>>> {
        let (off, f) = self.field(n, name)?;
        let Some(ty) = f.elem else { bail!("array '{name}' has no element type") };
        let stride = self.lay.type_size(ty, self.x64) as usize;
        let (count, data, ptrs) = match f.ut {
            T_INLINE_ARRAY => (f.array_size as usize, Some(off), false),
            T_DWORD_ARRAY | T_DWORD_PTR_ARRAY => (self.u32(off)? as usize, self.ptr(off + 4)?, f.ut == T_DWORD_PTR_ARRAY),
            T_WORD_ARRAY | T_WORD_PTR_ARRAY => (self.u16(off)? as usize, self.ptr(off + 2)?, f.ut == T_WORD_PTR_ARRAY),
            T_BYTE_ARRAY | T_BYTE_PTR_ARRAY => (self.u8(off)? as usize, self.ptr(off + 1)?, f.ut == T_BYTE_PTR_ARRAY),
            _ => bail!("field '{name}' is not an array ({})", f.ut),
        };
        let Some(data) = data else { return Ok(Vec::new()) };
        if count > 1_000_000 {
            bail!("array '{name}' too large ({count})");
        }
        let es = if ptrs { self.psize() } else { stride };
        self.bytes(data, count * es)?;
        let mut v = Vec::with_capacity(count);
        for i in 0..count {
            let e = data + i * es;
            if ptrs {
                v.push(self.ptr(e)?.map(|t| Node { off: t, ty }));
            } else {
                v.push(Some(Node { off: e, ty }));
            }
        }
        Ok(v)
    }

    pub fn float(&self, n: Node, name: &str) -> Result<f32> {
        let (off, _) = self.field(n, name)?;
        Ok(f32::from_bits(self.u32(off)?))
    }
    /// Element count of an array field without materialising it.
    pub fn count(&self, n: Node, name: &str) -> Result<usize> {
        let (off, f) = self.field(n, name)?;
        Ok(match f.ut {
            T_INLINE_ARRAY => f.array_size as usize,
            T_DWORD_ARRAY | T_DWORD_PTR_ARRAY | T_DWORD_TYPED_ARRAY => self.u32(off)? as usize,
            T_WORD_ARRAY | T_WORD_PTR_ARRAY | T_WORD_TYPED_ARRAY => self.u16(off)? as usize,
            T_BYTE_ARRAY | T_BYTE_PTR_ARRAY | T_BYTE_TYPED_ARRAY => self.u8(off)? as usize,
            u => bail!("field '{name}' is not an array ({u})"),
        })
    }
    pub fn qword(&self, n: Node, name: &str) -> Result<u64> {
        let (off, _) = self.field(n, name)?;
        self.u64(off)
    }
    /// Integer field (Byte/Word/Dword/Qword) widened to i64.
    pub fn int(&self, n: Node, name: &str) -> Result<i64> {
        let (off, f) = self.field(n, name)?;
        Ok(match f.ut {
            5 => self.u8(off)? as i64,
            T_WORD => self.u16(off)? as i64,
            T_DWORD | T_DWORD_ID => self.u32(off)? as i32 as i64,
            T_QWORD | T_QWORD_ID => self.u64(off)? as i64,
            u => bail!("field '{name}' not integral ({u})"),
        })
    }
    pub fn string(&self, n: Node, name: &str) -> Result<String> {
        let (off, f) = self.field(n, name)?;
        let Some(p) = self.ptr(off)? else { return Ok(String::new()) };
        match f.ut {
            T_STRING => {
                let s = &self.buf[p..];
                let e = s.iter().position(|&c| c == 0).unwrap_or(s.len()).min(512);
                Ok(String::from_utf8_lossy(&s[..e]).into_owned())
            }
            T_WSTRING => {
                let mut out = Vec::new();
                let mut q = p;
                while out.len() < 512 {
                    let c = self.u16(q)?;
                    if c == 0 {
                        break;
                    }
                    out.push(c);
                    q += 2;
                }
                Ok(String::from_utf16_lossy(&out))
            }
            u => bail!("field '{name}' not a string ({u})"),
        }
    }
    /// FileName field -> file id (0 if null / invalid).
    pub fn file_id(&self, n: Node, name: &str) -> Result<u32> {
        let (off, f) = self.field(n, name)?;
        if f.ut != T_FILENAME && f.ut != T_FILENAME2 {
            bail!("field '{name}' not a FileName ({})", f.ut);
        }
        let Some(p) = self.ptr(off)? else { return Ok(0) };
        Ok(file_ref_id(self.u16(p)?, self.u16(p + 2)?, self.u16(p + 4)?))
    }
}

pub fn file_ref_id(low: u16, high: u16, term: u16) -> u32 {
    if term == 0 && high >= 0x100 && low >= 0x100 {
        (high as u32 - 0x100) * 0xff00 + (low as u32 - 0xff)
    } else {
        0
    }
}

/// Token64 -> string (Common/Token64.ixx). None if it doesn't look textual.
pub fn token64(t: u64) -> String {
    if t == 0 {
        return String::new();
    }
    let num = t >> 60;
    let mut v = t & 0x0FFF_FFFF_FFFF_FFFF;
    let mut s = String::new();
    while v != 0 {
        let c = v & 0x1f;
        s.push(if c != 0 { (b'`' + c as u8) as char } else { ' ' });
        v >>= 5;
    }
    if num != 0 {
        s.push((b'0' + (num / 10) as u8) as char);
        s.push((b'0' + (num % 10) as u8) as char);
    }
    // '`' (0x60) + 27..31 gives '{|}~DEL' — not a real token character.
    if s.chars().any(|c| !(c == ' ' || c.is_ascii_lowercase() || c.is_ascii_digit())) {
        return format!("0x{t:016x}");
    }
    s
}
