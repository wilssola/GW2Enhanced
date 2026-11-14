//! v18: the UNORM clamp emulation of the float scene upgrade.
//!
//! WHY: the mod creates the scene's 8-bit colour targets (`B8G8R8A8_UNORM`) as `R16G16B16A16_FLOAT` (`upgrade.rs`). A UNORM target
//! clamps the pixel shader's output to 0..1 BEFORE the blend (and turns NaN into 0); a float target stores it as it is. Engine
//! shaders written for the 8-bit target may output negative colour, NaN, infinity or an alpha above 1 and rely on that clamp:
//! - an additive (`ONE:ONE`) volume whose output goes negative ADDS 0 in the engine's format but SUBTRACTS in float;
//! - a NaN / infinity stays in the float target and every later blur (bloom, fog, the light composite) spreads it;
//! - an alpha above 1 makes `INV_SRC_ALPHA` negative (the destination is subtracted).
//!
//! That is the black, hard-edged volumes and slabs seen in the game (pass #10 of the frame-pass log:
//! 12 instanced box volumes and two other volume draws, `ADD:ONE:ONE`, into an upgraded target), and it persists with every
//! effect off because the upgrade stays.
//!
//! WHAT: every engine pixel shader gets a twin (created once, next to the original, on the same device) whose colour outputs
//! (`SV_Target0..7`) are written as `min(max(c.rgb, 0), 65504)` and `saturate(c.a)`: the floor, the NaN removal and the alpha
//! range of the UNORM target, while values ABOVE 1 (the point of the upgrade) are kept. Before an engine draw on the game's
//! context the twin is bound when every bound colour target is one the mod upgraded (marked with private data at creation),
//! and the original otherwise (native float targets, shadow maps, the back buffer: their shaders already expect float).
//! The mod's own shaders and substitutes are never touched. `hdr_clamp=0` (config / overlay) is the A/B.
//!
//! HOW (pure, host-tested): `patch` rewrites the DXBC token stream (`SHDR` / `SHEX`): writes to the colour outputs go to new
//! temporaries, and before every `ret` / `retc` the clamped values are copied to the outputs; `checksum` recomputes the
//! container hash the runtime checks. A shader it cannot parse safely (interfaces, relative output indexing, unknown layout)
//! keeps the original: never a broken shader.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering::Relaxed};

// ---- DXBC container ----------------------------------------------------------------------------------------

fn rd32(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(off..off.checked_add(4)?)?.try_into().ok()?))
}

/// The four words of the MD5 variant the D3D runtime checks (bytes 4..20 of the container), over bytes 20.. .
pub fn checksum(code: &[u8]) -> [u32; 4] {
    let d = code.get(20..).unwrap_or(&[]);
    let n = d.len();
    let full = n / 64 * 64;
    let mut st = [0x6745_2301u32, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];
    for c in d[..full].chunks_exact(64) {
        md5_block(&mut st, c);
    }
    let last = &d[full..];
    let bits = (n as u32).wrapping_mul(8);
    let tail = (bits >> 2) | 1;
    let mut block = [0u8; 64];
    if last.len() >= 56 {
        block[..last.len()].copy_from_slice(last);
        block[last.len()] = 0x80;
        md5_block(&mut st, &block);
        let mut b2 = [0u8; 64];
        b2[0..4].copy_from_slice(&bits.to_le_bytes());
        b2[60..64].copy_from_slice(&tail.to_le_bytes());
        md5_block(&mut st, &b2);
    } else {
        block[0..4].copy_from_slice(&bits.to_le_bytes());
        block[4..4 + last.len()].copy_from_slice(last);
        block[4 + last.len()] = 0x80;
        block[60..64].copy_from_slice(&tail.to_le_bytes());
        md5_block(&mut st, &block);
    }
    st
}

fn md5_block(st: &mut [u32; 4], c: &[u8]) {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6,
        10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    let mut m = [0u32; 16];
    for (i, w) in m.iter_mut().enumerate() {
        *w = u32::from_le_bytes([c[4 * i], c[4 * i + 1], c[4 * i + 2], c[4 * i + 3]]);
    }
    let (mut a, mut b, mut cc, mut d) = (st[0], st[1], st[2], st[3]);
    for i in 0..64 {
        let (f, g) = match i / 16 {
            0 => ((b & cc) | (!b & d), i),
            1 => ((d & b) | (!d & cc), (5 * i + 1) % 16),
            2 => (b ^ cc ^ d, (3 * i + 5) % 16),
            _ => (cc ^ (b | !d), (7 * i) % 16),
        };
        let k = ((i as f64 + 1.0).sin().abs() * 4_294_967_296.0) as u32;
        let t = d;
        d = cc;
        cc = b;
        b = b.wrapping_add(a.wrapping_add(f).wrapping_add(k).wrapping_add(m[g]).rotate_left(S[i]));
        a = t;
    }
    st[0] = st[0].wrapping_add(a);
    st[1] = st[1].wrapping_add(b);
    st[2] = st[2].wrapping_add(cc);
    st[3] = st[3].wrapping_add(d);
}

/// Writes the checksum into the container.
pub fn seal(code: &mut [u8]) {
    let h = checksum(code);
    for (i, w) in h.iter().enumerate() {
        code[4 + 4 * i..8 + 4 * i].copy_from_slice(&w.to_le_bytes());
    }
}

/// Offset and size of the shader chunk (`SHDR` / `SHEX`) data.
fn shader_chunk(code: &[u8]) -> Option<(usize, usize, usize)> {
    if code.get(0..4)? != b"DXBC" {
        return None;
    }
    let n = rd32(code, 28)? as usize;
    if n > 64 {
        return None;
    }
    for i in 0..n {
        let off = rd32(code, 32 + 4 * i)? as usize;
        let cc = code.get(off..off.checked_add(4)?)?;
        if cc == b"SHDR" || cc == b"SHEX" {
            let size = rd32(code, off + 4)? as usize;
            code.get(off + 8..off.checked_add(8)?.checked_add(size)?)?;
            return Some((i, off, size));
        }
    }
    None
}

// ---- token stream -----------------------------------------------------------------------------------------

const OP_MAX: u32 = 0x34;
const OP_MIN: u32 = 0x33;
const OP_MOV: u32 = 0x36;
const OP_CUSTOMDATA: u32 = 0x35;
const OP_RET: u32 = 0x3E;
const OP_RETC: u32 = 0x3F;
const OP_DCL_OUTPUT: u32 = 0x65;
const OP_DCL_TEMPS: u32 = 0x68;
const OP_FCALL: u32 = 0x83;
const SAT: u32 = 1 << 13;
const T_TEMP: u32 = 0;
const T_OUTPUT: u32 = 2;
const T_IMM32: u32 = 4;
const T_IMM64: u32 = 5;

fn is_decl(op: u32) -> bool {
    (0x58..=0x6A).contains(&op) || (0x8F..=0xA2).contains(&op) || op == 0xCE
}

fn is_interface(op: u32) -> bool {
    (0x90..=0x92).contains(&op) || op == OP_FCALL
}

/// Walks one operand from `p`; rewrites a colour-output destination in `targets` to temp `base + index`.
/// Returns the position after the operand, or None on a layout this does not handle.
fn operand(t: &mut [u32], mut p: usize, end: usize, targets: u8, base: u32, hit: &mut bool) -> Option<usize> {
    let at = p;
    let tok = *t.get(p)?;
    p += 1;
    let ncomp = tok & 3;
    let ty = (tok >> 12) & 0xFF;
    let dim = (tok >> 20) & 3;
    let reps = [(tok >> 22) & 7, (tok >> 25) & 7, (tok >> 28) & 7];
    let mut ext = tok >> 31 != 0;
    while ext {
        let e = *t.get(p)?;
        p += 1;
        ext = e >> 31 != 0;
    }
    match ty {
        T_IMM32 => p += if ncomp == 2 { 4 } else { 1 },
        T_IMM64 => p += if ncomp == 2 { 8 } else { 2 },
        _ => {}
    }
    if ty == T_OUTPUT {
        if dim != 1 || reps[0] != 0 {
            return None; // relative / multi-dimensional output indexing: not handled
        }
        let idx = *t.get(p)?;
        if idx < 8 && targets & (1 << idx) != 0 {
            t[at] = (tok & !(0xFF << 12)) | (T_TEMP << 12);
            t[p] = base + idx;
            *hit = true;
        }
        p += 1;
    } else {
        for &rep in reps.iter().take(dim as usize) {
            match rep {
                0 => p += 1,
                1 => p += 2,
                2 => p = operand(t, p, end, 0, 0, hit)?,
                3 => p = operand(t, p + 1, end, 0, 0, hit)?,
                4 => p = operand(t, p + 2, end, 0, 0, hit)?,
                _ => return None,
            }
        }
    }
    (p <= end).then_some(p)
}

/// Why a shader keeps its original (for the log and the tests).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Skip {
    NotDxbc,
    NotPixel,
    NoColourOutput,
    Interfaces,
    Layout,
    NoRet,
}

fn op_len(t: &[u32], p: usize) -> Option<usize> {
    let tok = *t.get(p)?;
    let op = tok & 0x7FF;
    let len = if op == OP_CUSTOMDATA { *t.get(p + 1)? as usize } else { ((tok >> 24) & 0x7F) as usize };
    (len > 0).then_some(len)
}

fn dst(mask: u32, ty: u32, idx: u32) -> [u32; 2] {
    [2 | (mask << 4) | (ty << 12) | (1 << 20), idx]
}

fn src_temp(swz: u32, idx: u32) -> [u32; 2] {
    [2 | (1 << 2) | (swz << 4) | (T_TEMP << 12) | (1 << 20), idx]
}

fn imm4(v: f32) -> [u32; 5] {
    let b = v.to_bits();
    [2 | (T_IMM32 << 12), b, b, b, b]
}

/// The clamp of output `o` (declared components `mask`) from temp `r`, appended to `out`.
fn emit_clamp(out: &mut Vec<u32>, o: u32, r: u32, mask: u32) {
    let rgb = mask & 7;
    if rgb != 0 {
        out.push(OP_MAX | (10 << 24));
        out.extend(dst(rgb, T_TEMP, r));
        out.extend(src_temp(0xE4, r));
        out.extend(imm4(0.0));
        out.push(OP_MIN | (10 << 24));
        out.extend(dst(rgb, T_OUTPUT, o));
        out.extend(src_temp(0xE4, r));
        out.extend(imm4(65504.0));
    }
    if mask & 8 != 0 {
        out.push(OP_MOV | SAT | (5 << 24));
        out.extend(dst(8, T_OUTPUT, o));
        out.extend(src_temp(0xFF, r));
    }
}

/// The clamped twin of a pixel shader; `Err` says why it keeps the original.
pub fn patch(code: &[u8]) -> Result<Vec<u8>, Skip> {
    let (chunk_i, off, size) = shader_chunk(code).ok_or(Skip::NotDxbc)?;
    let colour: Vec<u32> = crate::furmask::parse_output_signature(code)
        .ok_or(Skip::NotDxbc)?
        .iter()
        // float colour targets only (an integer target has nothing to clamp)
        .filter(|e| e.name.eq_ignore_ascii_case("SV_Target") && e.reg < 8 && e.ctype == 3)
        .map(|e| e.reg)
        .collect();
    if colour.is_empty() {
        return Err(Skip::NoColourOutput);
    }
    let data = &code[off + 8..off + 8 + size];
    let mut t: Vec<u32> = data.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
    if t.len() < 2 || (t[0] >> 16) != 0 {
        return Err(Skip::NotPixel);
    }
    let total = (t[1] as usize).min(t.len());
    let mut targets = 0u8;
    for &r in &colour {
        targets |= 1 << r;
    }
    // Pass 1: declarations (temps count, declared output masks, where the code starts).
    let mut temps: Option<(usize, u32)> = None;
    let mut masks = [0u32; 8];
    let mut code_start = None;
    let mut p = 2;
    while p < total {
        let len = op_len(&t, p).ok_or(Skip::Layout)?;
        let op = t[p] & 0x7FF;
        if is_interface(op) {
            return Err(Skip::Interfaces);
        }
        if op == OP_CUSTOMDATA || is_decl(op) {
            if op == OP_DCL_TEMPS {
                temps = Some((p + 1, *t.get(p + 1).ok_or(Skip::Layout)?));
            } else if op == OP_DCL_OUTPUT {
                let tok = *t.get(p + 1).ok_or(Skip::Layout)?;
                if (tok >> 12) & 0xFF == T_OUTPUT {
                    let idx = *t.get(p + 2).ok_or(Skip::Layout)?;
                    if idx < 8 {
                        masks[idx as usize] = (tok >> 4) & 0xF;
                    }
                }
            }
        } else if code_start.is_none() {
            code_start = Some(p);
        }
        p += len;
    }
    if p != total {
        return Err(Skip::Layout);
    }
    let code_start = code_start.ok_or(Skip::NoRet)?;
    let base = temps.map_or(0, |(_, n)| n);
    let list: Vec<u32> = (0..8u32).filter(|i| targets & (1 << i) != 0 && masks[*i as usize] != 0).collect();
    if list.is_empty() {
        return Err(Skip::NoColourOutput);
    }
    let mut targets = 0u8;
    for &i in &list {
        targets |= 1 << i;
    }
    // Pass 2: rewrite the output writes, then rebuild with the init, the clamps and the new temps count.
    let mut out: Vec<u32> = Vec::with_capacity(t.len() + 32);
    out.extend_from_slice(&t[..2]);
    let mut rets = 0;
    let mut p = 2;
    while p < total {
        let len = op_len(&t, p).ok_or(Skip::Layout)?;
        let op = t[p] & 0x7FF;
        if p == code_start {
            if temps.is_none() {
                out.push(OP_DCL_TEMPS | (2 << 24));
                out.push(base + 8);
            }
            for &i in &list {
                out.push(OP_MOV | (8 << 24));
                out.extend(dst(0xF, T_TEMP, base + i));
                out.extend(imm4(0.0));
            }
        }
        let decl = op == OP_CUSTOMDATA || is_decl(op);
        if !decl && p >= code_start {
            if op == OP_RET || op == OP_RETC {
                for &i in &list {
                    emit_clamp(&mut out, i, base + i, masks[i as usize]);
                }
                rets += 1;
            } else {
                let mut q = p + 1;
                let mut ext = t[p] >> 31 != 0;
                while ext {
                    ext = *t.get(q).ok_or(Skip::Layout)? >> 31 != 0;
                    q += 1;
                }
                let end = p + len;
                let mut hit = false;
                while q < end {
                    q = operand(&mut t, q, end, targets, base, &mut hit).ok_or(Skip::Layout)?;
                }
                if q != end {
                    return Err(Skip::Layout);
                }
            }
        }
        if op == OP_DCL_TEMPS {
            out.push(t[p]);
            out.push(base + 8);
        } else {
            out.extend_from_slice(&t[p..p + len]);
        }
        p += len;
    }
    if rets == 0 {
        return Err(Skip::NoRet);
    }
    out[1] = out.len() as u32;
    // Rebuild the container: the new chunk, later chunk offsets moved, total size, checksum.
    let new_size = out.len() * 4;
    let delta = new_size as isize - size as isize;
    let mut v = Vec::with_capacity((code.len() as isize + delta) as usize);
    v.extend_from_slice(&code[..off + 4]);
    v.extend_from_slice(&(new_size as u32).to_le_bytes());
    for w in &out {
        v.extend_from_slice(&w.to_le_bytes());
    }
    v.extend_from_slice(&code[off + 8 + size..]);
    let n = rd32(code, 28).ok_or(Skip::NotDxbc)? as usize;
    for i in 0..n {
        let o = rd32(code, 32 + 4 * i).ok_or(Skip::NotDxbc)? as usize;
        if o > off && i != chunk_i {
            let moved = (o as isize + delta) as u32;
            v[32 + 4 * i..36 + 4 * i].copy_from_slice(&moved.to_le_bytes());
        }
    }
    let total_len = v.len() as u32;
    v[24..28].copy_from_slice(&total_len.to_le_bytes());
    seal(&mut v);
    Ok(v)
}

/// Bind the twin: the emulation is on and every bound colour target is an upgraded one.
pub fn wants(enabled: bool, upgraded: u8, bound: u8) -> bool {
    enabled && upgraded != 0 && upgraded == bound
}

// ---- runtime state ----------------------------------------------------------------------------------------

static ENABLED: AtomicBool = AtomicBool::new(true);
/// Upgraded / bound colour-target masks of the game context's current binding (bit = slot).
static UPGRADED_MASK: AtomicU32 = AtomicU32::new(0);
static BOUND_MASK: AtomicU32 = AtomicU32::new(0);
/// The game changed its pixel shader or render targets since the last check.
static DIRTY: AtomicBool = AtomicBool::new(true);
/// A twin is bound on the game context.
static TWIN_BOUND: AtomicBool = AtomicBool::new(false);
static CREATED: AtomicUsize = AtomicUsize::new(0);
static SKIPPED: AtomicUsize = AtomicUsize::new(0);
static FAILED: AtomicUsize = AtomicUsize::new(0);
static SWAPS: AtomicUsize = AtomicUsize::new(0);

#[derive(Default)]
struct Twins {
    of: HashMap<usize, usize>,
    back: HashMap<usize, usize>,
}

static TWINS: Mutex<Option<Twins>> = Mutex::new(None);

pub fn set_enabled(on: bool) {
    if ENABLED.swap(on, Relaxed) != on {
        DIRTY.store(true, Relaxed);
    }
}

pub fn enabled() -> bool {
    ENABLED.load(Relaxed)
}

/// The original of a twin (or the pointer itself).
pub fn original_of(ps: usize) -> usize {
    TWINS.lock().ok().and_then(|g| g.as_ref().and_then(|t| t.back.get(&ps).copied())).unwrap_or(ps)
}

fn twin_of(ps: usize) -> Option<usize> {
    TWINS.lock().ok().and_then(|g| g.as_ref().and_then(|t| t.of.get(&ps).copied()))
}

/// Remembers the twin of `ps`; returns a twin it replaces (the pointer was reused), to be released by the caller.
pub fn remember(ps: usize, twin: usize) -> Option<usize> {
    let mut g = TWINS.lock().ok()?;
    let t = g.get_or_insert_with(Twins::default);
    // A pointer that was a twin before and is now a new game shader: forget the old pairing.
    if let Some(orig) = t.back.remove(&ps) {
        t.of.remove(&orig);
    }
    let old = t.of.insert(ps, twin);
    if let Some(o) = old {
        t.back.remove(&o);
    }
    t.back.insert(twin, ps);
    CREATED.fetch_add(1, Relaxed);
    old
}

/// A game shader at `ps` was created without a twin: an older twin of that pointer is stale.
pub fn forget(ps: usize) -> Option<usize> {
    let mut g = TWINS.lock().ok()?;
    let t = g.as_mut()?;
    let old = t.of.remove(&ps)?;
    t.back.remove(&old);
    Some(old)
}

pub fn note_skipped() {
    SKIPPED.fetch_add(1, Relaxed);
}

pub fn note_failed() {
    FAILED.fetch_add(1, Relaxed);
}

/// The game bound new render targets (masks of the upgraded and of all bound colour targets).
pub fn note_targets(upgraded: u8, bound: u8) {
    UPGRADED_MASK.store(u32::from(upgraded), Relaxed);
    BOUND_MASK.store(u32::from(bound), Relaxed);
    DIRTY.store(true, Relaxed);
}

/// The game bound a pixel shader.
pub fn note_ps_set() {
    TWIN_BOUND.store(false, Relaxed);
    DIRTY.store(true, Relaxed);
}

/// The current upgraded-target mask and whether a twin is bound (the draw capture's `upg=` / `clamp=`).
pub fn trace_state() -> (u8, bool) {
    (UPGRADED_MASK.load(Relaxed) as u8, TWIN_BOUND.load(Relaxed))
}

/// What the draw hook must bind: Some(new shader) when the current one `cur` must change.
pub fn decide(cur: usize) -> Option<usize> {
    let want = wants(enabled(), UPGRADED_MASK.load(Relaxed) as u8, BOUND_MASK.load(Relaxed) as u8);
    let orig = original_of(cur);
    let desired = if want { twin_of(orig).unwrap_or(orig) } else { orig };
    TWIN_BOUND.store(want && desired != orig, Relaxed);
    (desired != cur).then_some(desired)
}

/// Draw hook fast path: nothing to check (one or two atomic loads).
pub fn needs_check() -> bool {
    DIRTY.swap(false, Relaxed)
}

pub fn note_swap() {
    SWAPS.fetch_add(1, Relaxed);
}

/// Overlay / dev capture line.
pub fn status_text() -> String {
    format!(
        "{} (twins {} created, {} kept original, {} failed; {} swaps; bound targets upgraded={:#04x} of {:#04x}, twin bound={})",
        if enabled() { "on" } else { "off" },
        CREATED.load(Relaxed),
        SKIPPED.load(Relaxed),
        FAILED.load(Relaxed),
        SWAPS.load(Relaxed),
        UPGRADED_MASK.load(Relaxed),
        BOUND_MASK.load(Relaxed),
        TWIN_BOUND.load(Relaxed)
    )
}

#[cfg(test)]
#[path = "clampemu_tests.rs"]
mod tests;
