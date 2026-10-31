//! Addresses and field offsets read from the game's own code.
//!
//! Nothing here is hardcoded: each value comes from the operand of an instruction
//! found by opcode inside the function located by signature. If a patch changes
//! the layout, the derivation fails with the reason and only that feature is disabled.
//! See `docs/reverse-engineering.md`.

use crate::game::Code;

/// Decoded text: `F_materialize` and the `TArray<u16>` at `ctx+arr`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Text {
    pub reserve: usize,
    /// Offset of the array in the decoding context.
    pub arr: usize,
    /// Fields inside the array.
    pub ptr: usize,
    pub cap: usize,
    pub count: usize,
}

/// Server message with `{u32 textId, u64 keySeed}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bulk {
    pub count: usize,
    pub records: usize,
}
pub const BULK_STRIDE: usize = 12;
pub const BULK_SEED: usize = 4;

/// Text cache walked by `cachetick`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cache {
    pub evict: usize,
    pub head: usize,
    pub link: usize,
    pub size: usize,
    pub busy: usize,
}

/// Globals used by `frame_rebuild`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rebuild {
    pub lang: usize,
    pub ready: usize,
}

fn field(v: Option<usize>, max: usize) -> Option<usize> {
    v.filter(|&v| v < max)
}

pub fn text(fmat: usize) -> Result<Text, &'static str> {
    let c = Code { at: fmat };
    // lea rcx,[r12+arr] ; mov rcx,[r12+arr+ptr] ; mov [rdx+arr+count],r15d
    let arr = field(c.disp32(&[0x49, 0x8D, 0x8C, 0x24], 0x80, 0), 0x10000).ok_or("text array lea")?;
    let ptr = c
        .disp32(&[0x49, 0x8B, 0x8C, 0x24], 0x100, 0)
        .ok_or("array pointer read")?;
    let count = c.disp32(&[0x44, 0x89, 0xBA], 0x80, 0).ok_or("array count write")?;
    let (ptr, count) = (
        ptr.checked_sub(arr).ok_or("ptr < arr")?,
        count.checked_sub(arr).ok_or("count < arr")?,
    );
    // mov edx,r14d ; call Reserve
    let reserve = c.rip(&[0x41, 0x8B, 0xD6, 0xE8], 0, 0x80, 0).ok_or("call Reserve")?;
    // mov r9d,[rcx+cap] right at the start of Reserve
    let cap = field(Code { at: reserve }.disp8(&[0x44, 0x8B, 0x49], 0x20, 0), 0x40).ok_or("capacity in Reserve")?;
    if ptr >= 0x40 || count >= 0x40 {
        return Err("array fields outside the expected range");
    }
    Ok(Text {
        reserve,
        arr,
        ptr,
        cap,
        count,
    })
}

pub fn bulk(addr: usize) -> Result<Bulk, &'static str> {
    let c = Code { at: addr };
    let count = field(c.disp8(&[0x0F, 0xB6, 0x55], 0x30, 0), 0x40).ok_or("message count")?;
    let records = field(c.disp8(&[0x48, 0x8B, 0x45], 0x100, 0), 0x40).ok_or("records pointer")?;
    // lea rcx,[rdi+rdi*2] ; mov r14,[rax+rcx*4+4]  =>  12-byte records, seed at +4
    c.find(&[0x48, 0x8D, 0x0C, 0x7F, 0x4C, 0x8B, 0x74, 0x88, 0x04], 0x100, 0)
        .ok_or("records format")?;
    Ok(Bulk { count, records })
}

pub fn cache(tick: usize) -> Result<Cache, &'static str> {
    let c = Code { at: tick };
    let head = field(c.disp8(&[0x48, 0x8B, 0x41], 0x40, 0), 0x100).ok_or("list head")?;
    let size = field(c.disp8(&[0x83, 0x7B], 0x80, 0), 0x100).ok_or("node size")?;
    let busy = field(c.disp8(&[0x83, 0x7B], 0x80, 1), 0x100).ok_or("node in use")?;
    let link = field(c.disp8(&[0x48, 0x63, 0x77], 0x100, 0), 0x100).ok_or("list link")?;
    // mov edx,1 ; mov rcx,rbx ; call evict
    let evict = c
        .rip(&[0xBA, 0x01, 0x00, 0x00, 0x00, 0x48, 0x8B, 0xCB, 0xE8], 0, 0x100, 0)
        .ok_or("call evict")?;
    Ok(Cache {
        evict,
        head,
        link,
        size,
        busy,
    })
}

pub fn rebuild(addr: usize) -> Result<Rebuild, &'static str> {
    let c = Code { at: addr };
    let ready = c.rip(&[0xF6, 0x05], 1, 0x20, 0).ok_or("UI ready flag")?;
    let lang = c.rip(&[0x89, 0x3D], 0, 0x40, 0).ok_or("language global")?;
    Ok(Rebuild { lang, ready })
}

/// Result of one [`check_exe`] item.
pub struct Check {
    pub name: &'static str,
    pub result: Result<String, String>,
}

/// Checks a `Gw2-64.exe` on disk: each signature must match exactly
/// once and each layout must be derivable. Use after every game patch.
pub fn check_exe(exe: &[u8]) -> Vec<Check> {
    use crate::sig::{self, Pattern, Site};
    let mut out = Vec::new();
    let Some((text_off, text_rva, text_len)) = text_section(exe) else {
        out.push(Check {
            name: "PE",
            result: Err(".text section not found".into()),
        });
        return out;
    };
    let code = &exe[text_off..text_off + text_len];
    let base = code.as_ptr() as usize;
    // Addresses inside the buffer; RIP-relative targets are just arithmetic.
    let rva = |a: usize| a.wrapping_sub(base).wrapping_add(text_rva);
    let mut found = |s: &Site| -> Option<usize> {
        let hits = Pattern::parse(s.sig).map(|p| p.find_all(code)).unwrap_or_default();
        let r = match hits.as_slice() {
            [one] => Ok(format!("rva {:#x}", text_rva + one)),
            [] => Err("signature not found".into()),
            many => Err(format!("ambiguous signature ({} occurrences)", many.len())),
        };
        let ok = r.is_ok();
        out.push(Check {
            name: s.name,
            result: r,
        });
        ok.then(|| base + hits[0])
    };
    let mut sites = Vec::new();
    for s in sig::ALL {
        sites.push((s.name, found(s)));
    }
    let at = |n: &str| sites.iter().find(|(name, _)| *name == n).and_then(|(_, a)| *a);
    // Slack so the search windows do not run past the end of the buffer.
    let safe = |a: Option<usize>| a.filter(|&a| a + 0x200 < base + code.len());
    let mut derived = |name: &'static str, r: Result<String, &'static str>| {
        out.push(Check {
            name,
            result: r.map_err(|e| e.to_string()),
        });
    };
    if let Some(a) = safe(at(sig::MATERIALIZE.name)) {
        derived(
            "layout text",
            text(a).map(|t| format!("{t:x?}, Reserve rva {:#x}", rva(t.reserve))),
        );
    }
    if let Some(a) = safe(at(sig::BULK_INSERT.name)) {
        derived("layout bulk_insert", bulk(a).map(|b| format!("{b:x?}")));
    }
    if let Some(a) = safe(at(sig::CACHETICK.name)) {
        derived(
            "layout cache",
            cache(a).map(|c| format!("{c:x?}, evict rva {:#x}", rva(c.evict))),
        );
    }
    if let Some(a) = safe(at(sig::FRAME_REBUILD.name)) {
        derived(
            "layout frame_rebuild",
            rebuild(a).map(|r| format!("language rva {:#x}, ready rva {:#x}", rva(r.lang), rva(r.ready))),
        );
    }
    out
}

/// `(file offset, RVA, size)` of the `.text` section.
fn text_section(exe: &[u8]) -> Option<(usize, usize, usize)> {
    let rd16 = |o: usize| Some(u16::from_le_bytes(exe.get(o..o + 2)?.try_into().ok()?) as usize);
    let rd32 = |o: usize| Some(u32::from_le_bytes(exe.get(o..o + 4)?.try_into().ok()?) as usize);
    let pe = rd32(0x3C)?;
    let (nsec, opt) = (rd16(pe + 6)?, rd16(pe + 20)?);
    (0..nsec).map(|i| pe + 24 + opt + 40 * i).find_map(|s| {
        (exe.get(s..s + 5)? == b".text").then_some(())?;
        let (rva, raw_len, raw_off) = (rd32(s + 12)?, rd32(s + 16)?, rd32(s + 20)?);
        (raw_off + raw_len <= exe.len()).then_some((raw_off, rva, raw_len))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sig::{self, Pattern, Site};

    /// Derives everything from the copy of the exe in `re/` and checks it against the manual analysis.
    #[test]
    fn derives_layout_from_game_exe() {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../re/Gw2-64.exe");
        let Ok(exe) = std::fs::read(&p) else { return };
        let base = exe.as_ptr() as usize;
        // .text: file offset 0x400, RVA 0x1000; RIP-relative targets are just arithmetic.
        let rva = |a: usize| a - base - 0x400 + 0x1000;
        let at = |s: &Site| base + Pattern::parse(s.sig).unwrap().find_unique(&exe).unwrap();

        let t = text(at(&sig::MATERIALIZE)).unwrap();
        assert_eq!(
            (rva(t.reserve), t.arr, t.ptr, t.cap, t.count),
            (0x257360, 0x268, 8, 0x10, 0x14)
        );
        assert_eq!(bulk(at(&sig::BULK_INSERT)).unwrap(), Bulk { count: 2, records: 3 });
        let c = cache(at(&sig::CACHETICK)).unwrap();
        assert_eq!(
            (rva(c.evict), c.head, c.link, c.size, c.busy),
            (0x10dd0c0, 0x48, 0x38, 0x30, 0x1C)
        );
        let r = rebuild(at(&sig::FRAME_REBUILD)).unwrap();
        assert_eq!((rva(r.lang), rva(r.ready)), (0x25b9858, 0x25b985c));

        let checks = check_exe(&exe);
        assert_eq!(checks.len(), sig::ALL.len() + 4);
        for c in &checks {
            assert!(c.result.is_ok(), "{}: {:?}", c.name, c.result);
        }
    }
}
