//! Per-draw diagnostic of the scene's opaque pass (no visual change, measurement only).
//!
//! WHY: the engine has no material buffer and the world pixel shaders are not in the exe, so nothing says
//! which draws are, say, charr bodies. To find them offline (`tools/draw-diff/draw_diff.py`) this module
//! records, for ONE armed frame, what each draw of the opaque pass is made of: draw kind and counts, the
//! bound vertex and pixel shader (bytecode hashes), the pixel-shader textures (format, size, mips), the sizes
//! of the bound vertex and pixel constant buffers (a skinned mesh has a big vertex constant buffer with the
//! bone matrices) and the input layout / vertex stride. The draws are aggregated by (VS hash, PS hash,
//! texture set) and written to `draws-<unix>.log` next to the pass dump.
//!
//! COST: reading the bound state means about 25 COM `Get` calls per draw, so it only happens while a capture is
//! armed (the overlay's "Dump frame" arms it for the NEXT frame, see `recorder::frame_end`). In every other
//! frame the draw hooks pay ONE atomic load (`on_draw`). Nothing here changes a call: the capture runs
//! before the original draw, only reads state (every reference a `Get` returns is released) and never unwinds.
//!
//! The same armed frame also feeds `draws-fx-<unix>.log` (see "Effects passes capture" in docs/draw-diagnostic.md):
//! the groups of EVERY other full-size-RT0 pass (additive effect quads, post), with the RT0 blend state and a
//! quad-like flag per group; the opaque pass log above is unchanged.
//!
//! Pure parts (`aggregate`, `format_log`, `to_record`) are tested on the host; `read_bound` is tested on WARP.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::sync::Mutex;

use crate::classify::classify;
use crate::d3d::{self, Obj, release, vfn, vt};
use crate::passes::{Pass, Target, format_name};
use crate::shaders::{self, Registry};
use crate::{log, recorder};

/// Pixel-shader resource slots read.
pub const SRV_SLOTS: usize = 8;
/// Constant buffer slots read (vertex and pixel).
pub const CB_SLOTS: usize = 4;
/// Draws kept in one capture (a runaway guard; the reference frame has about 930 opaque draws).
pub const MAX_DRAWS: usize = 60_000;
/// Draws kept per pass by the effects capture (`draws-fx`), and in total over all passes (memory bound).
pub const FX_MAX_DRAWS: usize = 4_000;
pub const FX_MAX_TOTAL: usize = 16_000;
/// A draw is "quad-like" (a screen-aligned quad or a small mesh) at or below these counts.
pub const QUAD_MAX_INDICES: u32 = 12;
pub const QUAD_MAX_VERTICES: u32 = 6;

// ---- plain data ---------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DrawKind {
    Draw,
    Indexed,
    Instanced,
    IndexedInstanced,
}

pub const KINDS: [DrawKind; 4] = [DrawKind::Draw, DrawKind::Indexed, DrawKind::Instanced, DrawKind::IndexedInstanced];

impl DrawKind {
    pub fn name(self) -> &'static str {
        match self {
            DrawKind::Draw => "Draw",
            DrawKind::Indexed => "DrawIndexed",
            DrawKind::Instanced => "DrawInstanced",
            DrawKind::IndexedInstanced => "DrawIndexedInstanced",
        }
    }

    /// `count` counts indices (otherwise vertices).
    pub fn is_indexed(self) -> bool {
        matches!(self, DrawKind::Indexed | DrawKind::IndexedInstanced)
    }
}

/// What a draw call asked for: the kind, the vertex or index count and the instance count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DrawArgs {
    pub kind: DrawKind,
    pub count: u32,
    pub instances: u32,
}

impl DrawArgs {
    pub fn draw(vertices: u32) -> Self {
        DrawArgs { kind: DrawKind::Draw, count: vertices, instances: 1 }
    }
    pub fn indexed(indices: u32) -> Self {
        DrawArgs { kind: DrawKind::Indexed, count: indices, instances: 1 }
    }
    pub fn instanced(vertices: u32, instances: u32) -> Self {
        DrawArgs { kind: DrawKind::Instanced, count: vertices, instances }
    }
    pub fn indexed_instanced(indices: u32, instances: u32) -> Self {
        DrawArgs { kind: DrawKind::IndexedInstanced, count: indices, instances }
    }
}

/// A pixel-shader texture reduced to what tells materials apart (not the pointer: that is per object).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TexSig {
    pub slot: u8,
    pub fmt: u32,
    pub w: u32,
    pub h: u32,
    pub mips: u32,
}

/// The state bound at a draw, as read from the context (raw pointers are identities only).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Bound {
    pub vs: usize,
    pub ps: usize,
    /// (signature, resource pointer) of the SRVs at slots 0..8.
    pub srvs: Vec<(TexSig, usize)>,
    /// Byte width of the vertex / pixel constant buffers at slots 0..4 (0 = none bound).
    pub vcb: [u32; CB_SLOTS],
    pub pcb: [u32; CB_SLOTS],
    pub layout: usize,
    /// Stride of the vertex buffer at slot 0 (0 = none).
    pub stride: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DrawRecord {
    pub ctx: usize,
    /// Index of the pass in the frame (same numbering as the pass dump).
    pub pass: usize,
    pub args: DrawArgs,
    pub vs: Option<u32>,
    pub ps: Option<u32>,
    pub bound: Bound,
}

/// Turns the raw bound state into a record, looking the shader hashes up in the two registries.
pub fn to_record(ctx: usize, pass: usize, args: DrawArgs, b: Bound, vs_reg: &Registry, ps_reg: &Registry) -> DrawRecord {
    let vs = if b.vs == 0 { None } else { vs_reg.hash_of(b.vs) };
    let ps = if b.ps == 0 { None } else { ps_reg.hash_of(b.ps) };
    DrawRecord { ctx, pass, args, vs, ps, bound: b }
}

// ---- aggregation ----------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct GroupKey {
    pub vs: Option<u32>,
    pub ps: Option<u32>,
    pub tex: Vec<TexSig>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    pub key: GroupKey,
    pub draws: u32,
    /// Draws per kind, in `KINDS` order.
    pub kinds: [u32; 4],
    pub indices: u64,
    pub max_indices: u32,
    pub vertices: u64,
    pub max_vertices: u32,
    pub instances: u64,
    /// Smallest and largest "biggest vertex constant buffer" over the draws.
    pub vcb_min: u32,
    pub vcb_max: u32,
    /// Largest size seen per slot.
    pub vcb_slots: [u32; CB_SLOTS],
    pub pcb_slots: [u32; CB_SLOTS],
    pub layouts: BTreeSet<usize>,
    pub strides: BTreeSet<u32>,
    pub tex_ptrs: BTreeSet<usize>,
}

fn max_slot(a: &[u32; CB_SLOTS]) -> u32 {
    a.iter().copied().max().unwrap_or(0)
}

/// Groups the records by (VS hash, PS hash, texture set), biggest group first (ties by key).
pub fn aggregate(records: &[DrawRecord]) -> Vec<Group> {
    let mut map: BTreeMap<GroupKey, Group> = BTreeMap::new();
    for r in records {
        let key = GroupKey { vs: r.vs, ps: r.ps, tex: r.bound.srvs.iter().map(|(t, _)| *t).collect() };
        let vmax = max_slot(&r.bound.vcb);
        let g = map.entry(key.clone()).or_insert_with(|| Group {
            key,
            draws: 0,
            kinds: [0; 4],
            indices: 0,
            max_indices: 0,
            vertices: 0,
            max_vertices: 0,
            instances: 0,
            vcb_min: u32::MAX,
            vcb_max: 0,
            vcb_slots: [0; CB_SLOTS],
            pcb_slots: [0; CB_SLOTS],
            layouts: BTreeSet::new(),
            strides: BTreeSet::new(),
            tex_ptrs: BTreeSet::new(),
        });
        g.draws += 1;
        if let Some(i) = KINDS.iter().position(|k| *k == r.args.kind) {
            g.kinds[i] += 1;
        }
        if r.args.kind.is_indexed() {
            g.indices += u64::from(r.args.count);
            g.max_indices = g.max_indices.max(r.args.count);
        } else {
            g.vertices += u64::from(r.args.count);
            g.max_vertices = g.max_vertices.max(r.args.count);
        }
        g.instances += u64::from(r.args.instances);
        g.vcb_min = g.vcb_min.min(vmax);
        g.vcb_max = g.vcb_max.max(vmax);
        for i in 0..CB_SLOTS {
            g.vcb_slots[i] = g.vcb_slots[i].max(r.bound.vcb[i]);
            g.pcb_slots[i] = g.pcb_slots[i].max(r.bound.pcb[i]);
        }
        g.layouts.insert(r.bound.layout);
        g.strides.insert(r.bound.stride);
        for (_, p) in &r.bound.srvs {
            g.tex_ptrs.insert(*p);
        }
    }
    let mut v: Vec<Group> = map.into_values().collect();
    v.sort_by(|a, b| b.draws.cmp(&a.draws).then_with(|| a.key.cmp(&b.key)));
    v
}

// ---- text format ----------------------------------------------------------------------------------------

/// What the header of the log says about the capture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    pub unix: u64,
    pub frame: u64,
    pub ctx: usize,
    pub pass: usize,
    pub screen: (u32, u32),
    /// Draws the recorder counted in that pass.
    pub pass_draws: u64,
    /// Records dropped by the `MAX_DRAWS` cap.
    pub dropped: u64,
    /// How the pass was chosen ("classifier" or a fallback text).
    pub how: String,
}

fn hash_text(h: Option<u32>) -> String {
    h.map_or_else(|| "unknown".to_string(), |h| format!("{h:08x}"))
}

/// `slot:FORMAT:WxHmM` joined by commas; `-` when there is no texture.
pub fn tex_text(tex: &[TexSig]) -> String {
    if tex.is_empty() {
        return "-".to_string();
    }
    tex.iter().map(|t| format!("{}:{}:{}x{}m{}", t.slot, format_name(t.fmt), t.w, t.h, t.mips)).collect::<Vec<_>>().join(",")
}

fn nums<T: std::fmt::Display>(v: &[T]) -> String {
    v.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(",")
}

/// One group as a `G ` line of `key=value` tokens (no spaces inside a value).
pub fn format_group(g: &Group) -> String {
    let kinds: Vec<String> = KINDS.iter().zip(g.kinds).filter(|(_, n)| *n > 0).map(|(k, n)| format!("{}:{n}", k.name())).collect();
    let strides: Vec<u32> = g.strides.iter().copied().collect();
    format!(
        "G draws={} vs={} ps={} kinds={} indices={} max_idx={} vertices={} max_vtx={} instances={} vcb={}..{} vcbs={} pcbs={} layouts={} strides={} texptrs={} tex={}",
        g.draws,
        hash_text(g.key.vs),
        hash_text(g.key.ps),
        kinds.join(","),
        g.indices,
        g.max_indices,
        g.vertices,
        g.max_vertices,
        g.instances,
        if g.vcb_min == u32::MAX { 0 } else { g.vcb_min },
        g.vcb_max,
        nums(&g.vcb_slots),
        nums(&g.pcb_slots),
        g.layouts.len(),
        nums(&strides),
        g.tex_ptrs.len(),
        tex_text(&g.key.tex),
    )
}

/// The whole log: `#` header lines, then one `G` line per group (biggest first).
pub fn format_log(h: &Header, groups: &[Group]) -> String {
    let captured: u64 = groups.iter().map(|g| u64::from(g.draws)).sum();
    let mut s = String::new();
    let _ = writeln!(s, "# gw2g-draws v1");
    let _ = writeln!(s, "# unix={} frame={} ctx={:#x} pass={} screen={}x{}", h.unix, h.frame, h.ctx, h.pass, h.screen.0, h.screen.1);
    let _ = writeln!(s, "# pass_draws={} captured={} dropped={} groups={} how={}", h.pass_draws, captured, h.dropped, groups.len(), h.how.replace(' ', "_"));
    for g in groups {
        let _ = writeln!(s, "{}", format_group(g));
    }
    s
}

// ---- effects passes (draws-fx) ------------------------------------------------------------------------------

/// Blend state of render target 0 (`D3D11_RENDER_TARGET_BLEND_DESC`; the numbers are the D3D11 enum values).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Blend {
    pub enable: bool,
    pub src: u32,
    pub dst: u32,
    pub op: u32,
    pub src_a: u32,
    pub dst_a: u32,
    pub op_a: u32,
}

fn blend_factor_name(n: u32) -> String {
    match n {
        1 => "ZERO",
        2 => "ONE",
        3 => "SRC_COLOR",
        4 => "INV_SRC_COLOR",
        5 => "SRC_ALPHA",
        6 => "INV_SRC_ALPHA",
        7 => "DEST_ALPHA",
        8 => "INV_DEST_ALPHA",
        9 => "DEST_COLOR",
        10 => "INV_DEST_COLOR",
        11 => "SRC_ALPHA_SAT",
        14 => "BLEND_FACTOR",
        15 => "INV_BLEND_FACTOR",
        _ => return format!("B{n}"),
    }
    .to_string()
}

fn blend_op_name(n: u32) -> String {
    match n {
        1 => "ADD",
        2 => "SUBTRACT",
        3 => "REV_SUBTRACT",
        4 => "MIN",
        5 => "MAX",
        _ => return format!("OP{n}"),
    }
    .to_string()
}

/// `off`, `OP:SRC:DST` (colour) with `|OP:SRC:DST` appended when the alpha part differs, `unknown` when not read.
pub fn blend_text(b: Option<Blend>) -> String {
    let Some(b) = b else { return "unknown".to_string() };
    if !b.enable {
        return "off".to_string();
    }
    let mut s = format!("{}:{}:{}", blend_op_name(b.op), blend_factor_name(b.src), blend_factor_name(b.dst));
    if (b.op_a, b.src_a, b.dst_a) != (b.op, b.src, b.dst) {
        let _ = write!(s, "|{}:{}:{}", blend_op_name(b.op_a), blend_factor_name(b.src_a), blend_factor_name(b.dst_a));
    }
    s
}

/// A draw captured for the effects log: the usual record plus the blend state (kept outside `DrawRecord`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FxRecord {
    pub rec: DrawRecord,
    pub blend: Option<Blend>,
    /// v18 trace: bit i = colour target i is a texture the mod upgraded to float; `clamp` = the clamped twin was bound.
    pub upg: u8,
    pub clamp: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FxGroup {
    pub pass: usize,
    pub blend: Option<Blend>,
    pub upg: u8,
    pub clamp: bool,
    pub group: Group,
}

/// Every draw of the group is a screen-aligned quad or a small mesh (indices <= 12 / vertices <= 6 per draw).
pub fn is_quad_like(g: &Group) -> bool {
    g.max_indices <= QUAD_MAX_INDICES && g.max_vertices <= QUAD_MAX_VERTICES
}

/// The passes the effects log covers: RT0 is full screen size (with or without depth), the opaque pass excluded.
pub fn fx_passes(frame: &[Pass], screen: (u32, u32), opaque: Option<usize>) -> Vec<usize> {
    frame.iter().enumerate().filter(|(i, p)| Some(*i) != opaque && p.rt.first().is_some_and(|t| is_screen(t, screen))).map(|(i, _)| i).collect()
}

/// Groups per (pass, blend state) by the usual key; ordered by pass, then biggest group first.
pub fn aggregate_fx(records: &[FxRecord]) -> Vec<FxGroup> {
    let mut parts: BTreeMap<(usize, Option<Blend>, u8, bool), Vec<DrawRecord>> = BTreeMap::new();
    for r in records {
        parts.entry((r.rec.pass, r.blend, r.upg, r.clamp)).or_default().push(r.rec.clone());
    }
    let mut out = Vec::new();
    for ((pass, blend, upg, clamp), recs) in parts {
        for group in aggregate(&recs) {
            out.push(FxGroup { pass, blend, upg, clamp, group });
        }
    }
    out.sort_by(|a, b| a.pass.cmp(&b.pass).then_with(|| b.group.draws.cmp(&a.group.draws)).then_with(|| a.group.key.cmp(&b.group.key)));
    out
}

/// One group of the effects log: a `G` line as in `draws-<unix>.log` with `pass=`, `blend=` and `quad=` first, and (v18) `upg=`
/// (mask of the colour targets the mod upgraded to float) and `clamp=` (the engine shader ran as its clamped twin).
pub fn format_fx_group(fg: &FxGroup) -> String {
    let base = format_group(&fg.group);
    format!("G pass={} blend={} quad={} upg={:#04x} clamp={} {}", fg.pass, blend_text(fg.blend), u8::from(is_quad_like(&fg.group)), fg.upg, u8::from(fg.clamp), &base[2..])
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FxHeader {
    pub unix: u64,
    pub frame: u64,
    pub ctx: usize,
    pub screen: (u32, u32),
    /// The opaque pass written to `draws-<unix>.log` (excluded here), if it is known.
    pub opaque: Option<usize>,
    pub how: String,
    /// Records dropped by the effects caps.
    pub dropped: u64,
    /// The covered passes, in order (see `fx_passes`).
    pub passes: Vec<usize>,
}

/// The effects log: `#` header lines (one `fxpass` line per covered pass with its colour/depth format), then one `G`
/// line per group.
pub fn format_fx_log(h: &FxHeader, frame: &[Pass], groups: &[FxGroup]) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "# gw2g-draws-fx v1");
    let _ = writeln!(s, "# unix={} frame={} ctx={:#x} screen={}x{}", h.unix, h.frame, h.ctx, h.screen.0, h.screen.1);
    let opaque = h.opaque.map_or_else(|| "none".to_string(), |p| p.to_string());
    let captured: u64 = groups.iter().map(|g| u64::from(g.group.draws)).sum();
    let _ = writeln!(s, "# opaque_pass={} how={} passes={} captured={} dropped={} groups={}", opaque, h.how.replace(' ', "_"), h.passes.len(), captured, h.dropped, groups.len());
    for &p in &h.passes {
        let (rt0, depth, draws) = match frame.get(p) {
            Some(pass) => (
                pass.rt.first().map_or_else(|| "none".to_string(), |t| format_name(t.fmt)),
                pass.depth.map_or_else(|| "none".to_string(), |t| format_name(t.fmt)),
                pass.draws,
            ),
            None => ("none".to_string(), "none".to_string(), 0),
        };
        let mine = groups.iter().filter(|g| g.pass == p);
        let (n, c) = mine.fold((0u32, 0u64), |(n, c), g| (n + 1, c + u64::from(g.group.draws)));
        let _ = writeln!(s, "# fxpass={p} rt0={rt0} depth={depth} pass_draws={draws} captured={c} groups={n}");
    }
    for g in groups {
        let _ = writeln!(s, "{}", format_fx_group(g));
    }
    s
}

/// Builds the effects log from the captured records; Ok((text, summary)) or the reason nothing can be written.
#[allow(clippy::too_many_arguments)]
pub fn build_fx_log(unix: u64, frame_no: u64, ctx: usize, frame: &[Pass], screen: (u32, u32), records: Vec<FxRecord>, dropped: u64, opaque: Option<(usize, String)>) -> Result<(String, String), String> {
    let passes = fx_passes(frame, screen, opaque.as_ref().map(|o| o.0));
    let mine: Vec<FxRecord> = records.into_iter().filter(|r| r.rec.ctx == ctx && passes.contains(&r.rec.pass)).collect();
    if mine.is_empty() {
        return Err("no effects draws captured (no other full-size pass drew on the game's context)".to_string());
    }
    let groups = aggregate_fx(&mine);
    let (opaque_pass, how) = match opaque {
        Some((p, how)) => (Some(p), how),
        None => (None, "unknown".to_string()),
    };
    let covered: Vec<usize> = passes.iter().copied().filter(|p| groups.iter().any(|g| g.pass == *p) || frame.get(*p).is_some_and(|x| x.draws > 0)).collect();
    let summary = format!("{} draws in {} passes, {} groups", mine.len(), covered.len(), groups.len());
    let header = FxHeader { unix, frame: frame_no, ctx, screen, opaque: opaque_pass, how, dropped, passes: covered };
    Ok((format_fx_log(&header, frame, &groups), summary))
}

// ---- capture state --------------------------------------------------------------------------------------

/// A capture is running (set by `arm`, cleared by `finish_frame`). The ONLY thing the hooks read in normal frames.
static ARMED: AtomicBool = AtomicBool::new(false);
static CAPTURED: AtomicUsize = AtomicUsize::new(0);

#[derive(Default)]
struct Capture {
    records: Vec<DrawRecord>,
    dropped: u64,
    /// Effects capture: draws of every full-size pass (any depth), with the blend state.
    fx: Vec<FxRecord>,
    fx_dropped: u64,
    fx_per_pass: BTreeMap<usize, usize>,
}

static CAPTURE: Mutex<Capture> = Mutex::new(Capture { records: Vec::new(), dropped: 0, fx: Vec::new(), fx_dropped: 0, fx_per_pass: BTreeMap::new() });
static STATUS: Mutex<String> = Mutex::new(String::new());

fn set_status(s: String) {
    *STATUS.lock().unwrap_or_else(|e| e.into_inner()) = s;
}

/// The capture is running.
pub fn is_armed() -> bool {
    ARMED.load(Relaxed)
}

/// Overlay line.
pub fn status_text() -> String {
    if is_armed() {
        return format!("capturing this frame ({} draws so far)", CAPTURED.load(Relaxed));
    }
    let s = STATUS.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if s.is_empty() { "idle (Dump frame arms it for the next frame)".to_string() } else { s }
}

/// Starts capturing from the next draw (called at a frame boundary, right after a dump request).
pub fn arm() {
    {
        let mut c = CAPTURE.lock().unwrap_or_else(|e| e.into_inner());
        c.records.clear();
        c.dropped = 0;
        c.fx.clear();
        c.fx_dropped = 0;
        c.fx_per_pass.clear();
    }
    CAPTURED.store(0, Relaxed);
    ARMED.store(true, Relaxed);
    set_status("armed: capturing the next frame".to_string());
    log!("drawdiag: capture armed, the next frame is recorded");
}

/// A draw is about to be issued (called inside the recorder gate: an immediate context, not our own call).
/// ONE atomic load when no capture is armed; never unwinds.
pub fn on_draw(ctx: Obj, args: DrawArgs) {
    let armed = ARMED.load(Relaxed);
    let pick = crate::furpick::is_open();
    if !armed && !pick {
        return;
    }
    if armed {
        let _ = std::panic::catch_unwind(|| capture(ctx, args));
    }
    if pick {
        crate::furpick::capture(ctx, args); // the overlay's fur picker window (skinned draws only, see furpick.rs)
    }
}

fn is_screen(t: &Target, screen: (u32, u32)) -> bool {
    t.w == screen.0 && t.h == screen.1
}

fn capture(ctx: Obj, args: DrawArgs) {
    // Only passes with a full-size colour target are read (opaque pass shape: colour AND depth; the effects passes:
    // colour with or without depth). Shadow maps, half-size post and the rest are skipped without a D3D call.
    let Some(site) = recorder::current_site(ctx as usize) else { return };
    let screen = d3d::hooks().screen();
    if !site.rt0.is_some_and(|t| is_screen(&t, screen)) {
        return;
    }
    let with_depth = site.depth.is_some_and(|t| is_screen(&t, screen));
    let bound = unsafe { read_bound(ctx) };
    let blend = unsafe { read_blend(ctx) };
    let rec = to_record(ctx as usize, site.index, args, bound, shaders::vs_registry(), shaders::registry());
    let mut c = CAPTURE.lock().unwrap_or_else(|e| e.into_inner());
    // Effects capture: per-pass and total caps.
    let in_pass = c.fx_per_pass.get(&rec.pass).copied().unwrap_or(0);
    if in_pass >= FX_MAX_DRAWS || c.fx.len() >= FX_MAX_TOTAL {
        c.fx_dropped += 1;
    } else {
        *c.fx_per_pass.entry(rec.pass).or_default() += 1;
        let (upg, clamp) = crate::clampemu::trace_state();
        c.fx.push(FxRecord { rec: rec.clone(), blend, upg, clamp });
    }
    if with_depth {
        if c.records.len() >= MAX_DRAWS {
            c.dropped += 1;
        } else {
            c.records.push(rec);
        }
    }
    CAPTURED.store(c.fx.len().max(c.records.len()), Relaxed);
}

/// The frame boundary after a capture frame: picks the opaque pass of the game's context, aggregates, writes
/// `draws-<unix>.log`. `frame` is the finished frame of context `ctx` (as the pass dump numbers it).
/// Does nothing when no capture is armed.
pub fn finish_frame(n: u64, ctx: usize, frame: &[Pass], screen: (u32, u32)) {
    if !ARMED.swap(false, Relaxed) {
        return;
    }
    let (records, dropped, fx, fx_dropped) = {
        let mut c = CAPTURE.lock().unwrap_or_else(|e| e.into_inner());
        c.fx_per_pass.clear();
        (std::mem::take(&mut c.records), std::mem::take(&mut c.dropped), std::mem::take(&mut c.fx), std::mem::take(&mut c.fx_dropped))
    };
    let unix = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    // The pass `draws-<unix>.log` uses, which the effects log leaves out.
    let opaque = opaque_choice(frame, screen, records.iter().filter(|r| r.ctx == ctx).map(|r| r.pass));
    let mut msg = match build_log(unix, n, ctx, frame, screen, records, dropped) {
        Ok((text, summary)) => match recorder::write_dump_named("draws", &text) {
            Ok(name) => format!("wrote {name} ({summary})"),
            Err(e) => e,
        },
        Err(e) => e,
    };
    let fx_msg = match build_fx_log(unix, n, ctx, frame, screen, fx, fx_dropped, opaque) {
        Ok((text, summary)) => match recorder::write_dump_named("draws-fx", &text) {
            Ok(name) => format!("wrote {name} ({summary})"),
            Err(e) => e,
        },
        Err(e) => e,
    };
    msg = format!("{msg}; fx: {fx_msg}");
    log!("drawdiag: capture finished: {msg}");
    set_status(msg);
}

/// The opaque pass: the classifier's, else the pass with the most captured draws (`passes` = the pass of each
/// captured draw); None when the classifier fails and nothing was captured.
fn opaque_choice(frame: &[Pass], screen: (u32, u32), passes: impl Iterator<Item = usize>) -> Option<(usize, String)> {
    match classify(frame, screen) {
        Ok(m) => Some((m.opaque, "classifier".to_string())),
        Err(e) => {
            let mut per: BTreeMap<usize, usize> = BTreeMap::new();
            for p in passes {
                *per.entry(p).or_default() += 1;
            }
            let best = per.iter().max_by_key(|(p, n)| (**n, std::cmp::Reverse(**p))).map(|(p, _)| *p)?;
            Some((best, format!("fallback_most_draws_(classifier:_{e})")))
        }
    }
}

/// Picks the pass and builds the log text; Ok((text, summary)) or the reason nothing can be written.
pub fn build_log(unix: u64, frame_no: u64, ctx: usize, frame: &[Pass], screen: (u32, u32), records: Vec<DrawRecord>, dropped: u64) -> Result<(String, String), String> {
    let mine: Vec<DrawRecord> = records.into_iter().filter(|r| r.ctx == ctx).collect();
    if mine.is_empty() {
        return Err("no draws captured (no full-size colour+depth pass drew on the game's context)".to_string());
    }
    let (pass, how) = opaque_choice(frame, screen, mine.iter().map(|r| r.pass)).unwrap_or((0, "fallback_most_draws_(no_draws)".to_string()));
    let sel: Vec<DrawRecord> = mine.into_iter().filter(|r| r.pass == pass).collect();
    if sel.is_empty() {
        return Err(format!("no draws captured in the opaque pass #{pass}"));
    }
    let groups = aggregate(&sel);
    let header = Header { unix, frame: frame_no, ctx, pass, screen, pass_draws: frame.get(pass).map_or(0, |p| u64::from(p.draws)), dropped, how };
    let summary = format!("{} draws in pass #{pass}, {} groups", sel.len(), groups.len());
    Ok((format_log(&header, &groups), summary))
}

// ---- reading the bound state ------------------------------------------------------------------------------

/// `ByteWidth` of a buffer (0 for null).
pub(crate) unsafe fn buffer_width(buf: Obj) -> u32 {
    if buf.is_null() {
        return 0;
    }
    let mut d = [0u32; 6]; // D3D11_BUFFER_DESC
    unsafe {
        let get_desc: unsafe extern "system" fn(Obj, *mut [u32; 6]) = std::mem::transmute(vfn(buf, vt::BUFFER_GET_DESC));
        get_desc(buf, &mut d);
    }
    d[0]
}

/// Slot signature + resource pointer of the SRV `view` (a 2D texture; other resources keep fmt 0 and size 0).
pub(crate) unsafe fn srv_info(slot: usize, view: Obj) -> (TexSig, usize) {
    let mut sig = TexSig { slot: slot as u8, fmt: 0, w: 0, h: 0, mips: 0 };
    unsafe {
        let mut res: Obj = std::ptr::null_mut();
        let get_resource: unsafe extern "system" fn(Obj, *mut Obj) = std::mem::transmute(vfn(view, vt::VIEW_GET_RESOURCE));
        get_resource(view, &mut res);
        if res.is_null() {
            return (sig, 0);
        }
        let mut dim = 0u32;
        let get_type: unsafe extern "system" fn(Obj, *mut u32) = std::mem::transmute(vfn(res, vt::RES_GET_TYPE));
        get_type(res, &mut dim);
        if dim == d3d::RESOURCE_TEXTURE2D {
            let mut d = d3d::Tex2dDesc::default();
            let get_desc: unsafe extern "system" fn(Obj, *mut d3d::Tex2dDesc) = std::mem::transmute(vfn(res, vt::TEX2D_GET_DESC));
            get_desc(res, &mut d);
            sig.fmt = d.format;
            sig.w = d.width;
            sig.h = d.height;
            sig.mips = d.mips;
            // The view's own format wins (the texture may be typeless); word 0 of D3D11_SHADER_RESOURCE_VIEW_DESC.
            let mut v = [0u32; 8];
            let get_view_desc: unsafe extern "system" fn(Obj, *mut [u32; 8]) = std::mem::transmute(vfn(view, vt::VIEW_GET_DESC));
            get_view_desc(view, &mut v);
            if v[0] != 0 {
                sig.fmt = v[0];
            }
        }
        release(res);
        (sig, res as usize)
    }
}

/// Blend state of RT0 via `OMGetBlendState` + `ID3D11BlendState::GetDesc` (no state bound = the default: off).
/// Every reference is released.
///
/// # Safety
/// `ctx` must be a live `ID3D11DeviceContext`.
pub(crate) unsafe fn read_blend(ctx: Obj) -> Option<Blend> {
    unsafe {
        let mut state: Obj = std::ptr::null_mut();
        let mut factor = [0f32; 4];
        let mut mask = 0u32;
        let get: unsafe extern "system" fn(Obj, *mut Obj, *mut f32, *mut u32) = std::mem::transmute(vfn(ctx, vt::CTX_OM_GET_BLEND));
        get(ctx, &mut state, factor.as_mut_ptr(), &mut mask);
        if state.is_null() {
            return Some(Blend::default());
        }
        // D3D11_BLEND_DESC: AlphaToCoverage, Independent, then RT0 { Enable, Src, Dest, Op, SrcA, DestA, OpA, WriteMask }.
        let mut d = [0u32; 66];
        let get_desc: unsafe extern "system" fn(Obj, *mut [u32; 66]) = std::mem::transmute(vfn(state, vt::BLEND_GET_DESC));
        get_desc(state, &mut d);
        release(state);
        Some(Blend { enable: d[2] != 0, src: d[3], dst: d[4], op: d[5], src_a: d[6], dst_a: d[7], op_a: d[8] })
    }
}

/// Reads what is bound on `ctx` right now (Get calls only; every returned reference is released).
///
/// # Safety
/// `ctx` must be a live `ID3D11DeviceContext`.
pub(crate) unsafe fn read_bound(ctx: Obj) -> Bound {
    let mut b = Bound::default();
    let null = std::ptr::null_mut::<std::ffi::c_void>();
    unsafe {
        type GetShader = unsafe extern "system" fn(Obj, *mut Obj, *mut Obj, *mut u32);
        let mut sh: Obj = null;
        let get_vs: GetShader = std::mem::transmute(vfn(ctx, vt::CTX_VS_GET_SHADER));
        get_vs(ctx, &mut sh, std::ptr::null_mut(), std::ptr::null_mut());
        b.vs = sh as usize;
        release(sh);
        sh = null;
        let get_ps: GetShader = std::mem::transmute(vfn(ctx, vt::CTX_PS_GET_SHADER));
        get_ps(ctx, &mut sh, std::ptr::null_mut(), std::ptr::null_mut());
        b.ps = sh as usize;
        release(sh);

        let mut srvs = [null; SRV_SLOTS];
        let get_srvs: unsafe extern "system" fn(Obj, u32, u32, *mut Obj) = std::mem::transmute(vfn(ctx, vt::CTX_PS_GET_SRVS));
        get_srvs(ctx, 0, SRV_SLOTS as u32, srvs.as_mut_ptr());
        for (i, v) in srvs.iter().enumerate() {
            if !v.is_null() {
                b.srvs.push(srv_info(i, *v));
                release(*v);
            }
        }

        type GetCbs = unsafe extern "system" fn(Obj, u32, u32, *mut Obj);
        let mut cbs = [null; CB_SLOTS];
        let get_vcb: GetCbs = std::mem::transmute(vfn(ctx, vt::CTX_VS_GET_CBS));
        get_vcb(ctx, 0, CB_SLOTS as u32, cbs.as_mut_ptr());
        for (i, c) in cbs.iter().enumerate() {
            b.vcb[i] = buffer_width(*c);
            release(*c);
        }
        let mut cbs = [null; CB_SLOTS];
        let get_pcb: GetCbs = std::mem::transmute(vfn(ctx, vt::CTX_PS_GET_CBS));
        get_pcb(ctx, 0, CB_SLOTS as u32, cbs.as_mut_ptr());
        for (i, c) in cbs.iter().enumerate() {
            b.pcb[i] = buffer_width(*c);
            release(*c);
        }

        let mut layout: Obj = null;
        let get_layout: unsafe extern "system" fn(Obj, *mut Obj) = std::mem::transmute(vfn(ctx, vt::CTX_IA_GET_LAYOUT));
        get_layout(ctx, &mut layout);
        b.layout = layout as usize;
        release(layout);

        let (mut vb, mut stride, mut offset) = (null, 0u32, 0u32);
        let get_vb: unsafe extern "system" fn(Obj, u32, u32, *mut Obj, *mut u32, *mut u32) = std::mem::transmute(vfn(ctx, vt::CTX_IA_GET_VBS));
        get_vb(ctx, 0, 1, &mut vb, &mut stride, &mut offset);
        release(vb);
        b.stride = if vb.is_null() { 0 } else { stride };
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::passes::FMT_B8G8R8A8_UNORM;

    const BC3: u32 = 77;
    const BC1: u32 = 71;

    fn tex(slot: u8, fmt: u32, w: u32, h: u32, mips: u32) -> TexSig {
        TexSig { slot, fmt, w, h, mips }
    }

    fn rec(pass: usize, args: DrawArgs, vs: u32, ps: u32, srvs: &[(TexSig, usize)], vcb0: u32, layout: usize, stride: u32) -> DrawRecord {
        DrawRecord {
            ctx: 0x10,
            pass,
            args,
            vs: Some(vs),
            ps: Some(ps),
            bound: Bound { vs: 1, ps: 2, srvs: srvs.to_vec(), vcb: [vcb0, 0, 0, 0], pcb: [64, 0, 0, 0], layout, stride },
        }
    }

    fn sample_records() -> Vec<DrawRecord> {
        let body = [(tex(0, BC3, 1024, 1024, 11), 0xa0), (tex(1, BC1, 512, 512, 10), 0xa1), (tex(2, BC3, 1024, 1024, 11), 0xa2)];
        let body2 = [(tex(0, BC3, 1024, 1024, 11), 0xb0), (tex(1, BC1, 512, 512, 10), 0xb1), (tex(2, BC3, 1024, 1024, 11), 0xb2)];
        let rock = [(tex(0, BC1, 256, 256, 9), 0xc0)];
        vec![
            rec(3, DrawArgs::indexed(5400), 0x1111, 0x2222, &body, 2048, 7, 48),
            rec(3, DrawArgs::indexed(7200), 0x1111, 0x2222, &body2, 4096, 7, 48),
            rec(3, DrawArgs::indexed(300), 0x3333, 0x4444, &rock, 128, 8, 32),
            rec(3, DrawArgs::indexed(300), 0x3333, 0x4444, &rock, 128, 8, 32),
            rec(3, DrawArgs::indexed_instanced(96, 40), 0x3333, 0x4444, &rock, 128, 8, 32),
            rec(3, DrawArgs::draw(6), 0x5555, 0x6666, &[], 0, 0, 0),
        ]
    }

    #[test]
    fn aggregation_groups_by_shaders_and_texture_set_and_sorts_by_count() {
        let g = aggregate(&sample_records());
        assert_eq!(g.len(), 3);
        // rock: 3 draws, first.
        assert_eq!(g[0].draws, 3);
        assert_eq!((g[0].key.vs, g[0].key.ps), (Some(0x3333), Some(0x4444)));
        assert_eq!(g[0].kinds, [0, 2, 0, 1]);
        assert_eq!(g[0].indices, 300 + 300 + 96);
        assert_eq!(g[0].max_indices, 300);
        assert_eq!(g[0].instances, 1 + 1 + 40);
        assert_eq!((g[0].vcb_min, g[0].vcb_max), (128, 128));
        // body: two draws with different texture pointers but the same signature -> one group.
        assert_eq!(g[1].draws, 2);
        assert_eq!(g[1].indices, 5400 + 7200);
        assert_eq!(g[1].max_indices, 7200);
        assert_eq!((g[1].vcb_min, g[1].vcb_max), (2048, 4096));
        assert_eq!(g[1].tex_ptrs.len(), 6);
        assert_eq!(g[1].layouts.len(), 1);
        assert_eq!(g[1].key.tex.len(), 3);
        // non-indexed, no texture.
        assert_eq!(g[2].draws, 1);
        assert_eq!((g[2].vertices, g[2].max_vertices, g[2].indices), (6, 6, 0));
        assert!(g[2].key.tex.is_empty());
    }

    #[test]
    fn a_different_texture_set_or_shader_is_another_group() {
        let t0 = [(tex(0, BC3, 64, 64, 7), 1)];
        let t1 = [(tex(0, BC3, 128, 128, 8), 2)];
        let r = vec![
            rec(1, DrawArgs::indexed(3), 1, 2, &t0, 0, 0, 0),
            rec(1, DrawArgs::indexed(3), 1, 2, &t1, 0, 0, 0),
            rec(1, DrawArgs::indexed(3), 9, 2, &t0, 0, 0, 0),
        ];
        assert_eq!(aggregate(&r).len(), 3);
        assert!(aggregate(&[]).is_empty());
    }

    #[test]
    fn ties_are_ordered_by_key_so_the_output_is_deterministic() {
        let r = vec![rec(1, DrawArgs::indexed(3), 9, 2, &[], 0, 0, 0), rec(1, DrawArgs::indexed(3), 1, 2, &[], 0, 0, 0)];
        let g = aggregate(&r);
        assert_eq!(g[0].key.vs, Some(1));
        let mut rev = r.clone();
        rev.reverse();
        assert_eq!(aggregate(&rev), g);
    }

    #[test]
    fn unknown_shader_hashes_are_grouped_and_written_as_unknown() {
        let mut r = rec(1, DrawArgs::draw(3), 1, 2, &[], 0, 0, 0);
        r.vs = None;
        let g = aggregate(&[r]);
        assert!(format_group(&g[0]).contains(" vs=unknown ps=00000002 "), "{}", format_group(&g[0]));
    }

    #[test]
    fn to_record_looks_the_hashes_up_and_leaves_unknown_pointers_none() {
        let (vs, ps) = (Registry::new(), Registry::new());
        vs.record(0x100, b"vs-bytes");
        ps.record(0x200, b"ps-bytes");
        let b = Bound { vs: 0x100, ps: 0x200, ..Default::default() };
        let r = to_record(5, 2, DrawArgs::draw(3), b, &vs, &ps);
        assert_eq!((r.vs, r.ps), (Some(shaders::fnv1a32(b"vs-bytes")), Some(shaders::fnv1a32(b"ps-bytes"))));
        let r = to_record(5, 2, DrawArgs::draw(3), Bound { vs: 0x999, ps: 0, ..Default::default() }, &vs, &ps);
        assert_eq!((r.vs, r.ps), (None, None));
    }

    #[test]
    fn draw_args_constructors() {
        assert_eq!(DrawArgs::draw(9), DrawArgs { kind: DrawKind::Draw, count: 9, instances: 1 });
        assert_eq!(DrawArgs::indexed(9).kind, DrawKind::Indexed);
        assert_eq!(DrawArgs::instanced(9, 4).instances, 4);
        assert_eq!(DrawArgs::indexed_instanced(9, 4).kind, DrawKind::IndexedInstanced);
        assert!(DrawKind::Indexed.is_indexed() && !DrawKind::Instanced.is_indexed());
    }

    const GOLDEN: &str = include_str!("../../tools/draw-diff/fixtures/sample_a.log");

    fn header() -> Header {
        Header { unix: 1700000000, frame: 42, ctx: 0x1f00, pass: 3, screen: (3440, 1440), pass_draws: 932, dropped: 0, how: "classifier".to_string() }
    }

    #[test]
    fn the_log_text_matches_the_fixture_the_python_tool_is_tested_with() {
        let text = format_log(&header(), &aggregate(&sample_records()));
        // The checkout may turn the fixture's line ends into CRLF (core.autocrlf on Windows).
        assert_eq!(text, GOLDEN.replace("\r\n", "\n"), "regenerate tools/draw-diff/fixtures/sample_a.log from this output if the format changed on purpose:\n{text}");
    }

    #[test]
    fn group_line_has_every_field_once() {
        let g = aggregate(&sample_records());
        let line = format_group(&g[1]);
        for k in ["draws=", "vs=", "ps=", "kinds=", "indices=", "max_idx=", "vertices=", "max_vtx=", "instances=", "vcb=", "vcbs=", "pcbs=", "layouts=", "strides=", "texptrs=", "tex="] {
            assert_eq!(line.matches(&format!(" {k}")).count(), 1, "{k} in {line}");
        }
        assert!(line.contains("tex=0:BC3_UNORM:1024x1024m11,1:BC1_UNORM:512x512m10,2:BC3_UNORM:1024x1024m11"), "{line}");
        assert!(line.contains(" vcb=2048..4096 vcbs=4096,0,0,0 "), "{line}");
        assert!(!line.contains("  "), "no double spaces: {line}");
        assert_eq!(tex_text(&[]), "-");
        assert_eq!(tex_text(&[tex(3, 9999, 1, 2, 3)]), "3:FMT_9999:1x2m3");
    }

    fn pass(rt: Target, depth: Option<Target>, draws: u32) -> Pass {
        Pass { rt: vec![rt], depth, draws, dispatches: 0 }
    }

    #[test]
    fn build_log_uses_the_classifier_pass_and_filters_context_and_pass() {
        let f = crate::passes::parse_frame_log(crate::frame_fixture::FRAME_PASSES);
        let screen = (3440, 1440);
        let opaque = classify(&f, screen).unwrap().opaque;
        let mut recs = sample_records();
        for r in &mut recs {
            r.pass = opaque;
        }
        recs.push(DrawRecord { pass: opaque + 5, ..recs[0].clone() }); // another pass: dropped
        recs.push(DrawRecord { ctx: 0x99, ..recs[0].clone() }); // another context: dropped
        let (text, summary) = build_log(1, 2, 0x10, &f, screen, recs, 3).unwrap();
        assert_eq!(summary, format!("6 draws in pass #{opaque}, 3 groups"));
        assert!(text.contains(&format!("pass={opaque} ")), "{text}");
        assert!(text.contains(&format!("pass_draws={} captured=6 dropped=3 groups=3 how=classifier", f[opaque].draws)), "{text}");
    }

    #[test]
    fn build_log_falls_back_to_the_pass_with_most_draws_when_the_classifier_fails() {
        let frame = vec![pass(Target { w: 10, h: 10, fmt: FMT_B8G8R8A8_UNORM }, None, 5)];
        let mut recs = sample_records();
        recs.truncate(3); // pass 3
        recs.push(DrawRecord { pass: 4, ..recs[0].clone() });
        let (text, summary) = build_log(1, 2, 0x10, &frame, (10, 10), recs, 0).unwrap();
        assert_eq!(summary, "3 draws in pass #3, 2 groups");
        assert!(text.contains("how=fallback_most_draws_(classifier:_no_opaque_pass)"), "{text}");
    }

    #[test]
    fn build_log_says_why_when_nothing_was_captured() {
        assert!(build_log(1, 2, 0x10, &[], (10, 10), vec![], 0).unwrap_err().contains("no draws captured"));
        let other_ctx = vec![DrawRecord { ctx: 0x77, ..sample_records()[0].clone() }];
        assert!(build_log(1, 2, 0x10, &[], (10, 10), other_ctx, 0).is_err());
    }

    #[test]
    fn status_text_follows_the_state() {
        // The only test touching the globals of the capture (tests run in parallel).
        ARMED.store(false, Relaxed);
        // Not armed: a null context would crash if anything were read.
        on_draw(std::ptr::null_mut(), DrawArgs::draw(3));
        set_status(String::new());
        assert!(status_text().starts_with("idle"));
        arm();
        assert!(is_armed());
        assert!(status_text().starts_with("capturing"));
        // finish_frame with nothing captured: disarms and explains.
        finish_frame(1, 0x10, &[], (10, 10));
        assert!(!is_armed());
        assert!(status_text().contains("no draws captured"), "{}", status_text());
        // Not armed: finish_frame is a no-op that keeps the status.
        let before = status_text();
        finish_frame(2, 0x10, &[], (10, 10));
        assert_eq!(status_text(), before);
    }

    // ---- effects passes (draws-fx) ----

    const ADD: Blend = Blend { enable: true, src: 2, dst: 2, op: 1, src_a: 2, dst_a: 2, op_a: 1 };
    const ALPHA: Blend = Blend { enable: true, src: 5, dst: 6, op: 1, src_a: 2, dst_a: 6, op_a: 1 };

    fn fx(pass: usize, args: DrawArgs, vs: u32, blend: Option<Blend>) -> FxRecord {
        FxRecord { rec: rec(pass, args, vs, 0x99, &[], 0, 0, 0), blend, upg: 0, clamp: false }
    }

    fn fx_frame() -> Vec<Pass> {
        let full = Target { w: 100, h: 50, fmt: 10 };
        let half = Target { w: 50, h: 25, fmt: 10 };
        let depth = Some(Target { w: 100, h: 50, fmt: 20 });
        vec![
            pass(full, depth, 900),            // 0 opaque
            pass(half, None, 5),               // 1 half size: never
            pass(full, None, 63),              // 2 full, no depth
            pass(full, depth, 556),            // 3 full, depth
            Pass { rt: vec![], depth, draws: 4, dispatches: 0 }, // 4 depth only: never
        ]
    }

    #[test]
    fn fx_passes_are_the_full_size_rt0_passes_without_the_opaque_one() {
        let f = fx_frame();
        assert_eq!(fx_passes(&f, (100, 50), Some(0)), vec![2, 3]);
        assert_eq!(fx_passes(&f, (100, 50), None), vec![0, 2, 3]);
        assert!(fx_passes(&[], (100, 50), Some(0)).is_empty());
    }

    #[test]
    fn blend_text_names_the_state() {
        assert_eq!(blend_text(None), "unknown");
        assert_eq!(blend_text(Some(Blend::default())), "off");
        assert_eq!(blend_text(Some(ADD)), "ADD:ONE:ONE");
        assert_eq!(blend_text(Some(ALPHA)), "ADD:SRC_ALPHA:INV_SRC_ALPHA|ADD:ONE:INV_SRC_ALPHA");
        assert_eq!(blend_text(Some(Blend { enable: true, src: 99, dst: 1, op: 7, src_a: 99, dst_a: 1, op_a: 7 })), "OP7:B99:ZERO");
    }

    #[test]
    fn quad_like_means_every_draw_is_small() {
        let g = |r: &[FxRecord]| aggregate(&r.iter().map(|x| x.rec.clone()).collect::<Vec<_>>());
        assert!(is_quad_like(&g(&[fx(1, DrawArgs::indexed(6), 1, None), fx(1, DrawArgs::indexed(12), 1, None)])[0]));
        assert!(is_quad_like(&g(&[fx(1, DrawArgs::draw(4), 1, None), fx(1, DrawArgs::instanced(6, 90), 1, None)])[0]));
        assert!(!is_quad_like(&g(&[fx(1, DrawArgs::indexed(6), 1, None), fx(1, DrawArgs::indexed(13), 1, None)])[0]));
        assert!(!is_quad_like(&g(&[fx(1, DrawArgs::draw(7), 1, None)])[0]));
    }

    #[test]
    fn fx_aggregation_separates_passes_and_blend_states_and_sorts() {
        let recs = vec![
            fx(3, DrawArgs::indexed(6), 1, Some(ADD)),
            fx(3, DrawArgs::indexed(6), 1, Some(ADD)),
            fx(3, DrawArgs::indexed(6), 1, Some(ALPHA)),
            fx(2, DrawArgs::indexed(600), 2, Some(Blend::default())),
            fx(2, DrawArgs::draw(4), 3, None),
            fx(2, DrawArgs::draw(4), 3, None),
            fx(2, DrawArgs::draw(4), 3, None),
        ];
        let g = aggregate_fx(&recs);
        let key: Vec<(usize, u32)> = g.iter().map(|x| (x.pass, x.group.draws)).collect();
        assert_eq!(key, vec![(2, 3), (2, 1), (3, 2), (3, 1)]);
        assert_eq!(g[0].blend, None);
        assert_eq!(g[2].blend, Some(ADD));
        let mut rev = recs.clone();
        rev.reverse();
        assert_eq!(aggregate_fx(&rev), g);
        let line = format_fx_group(&g[2]);
        assert!(line.starts_with("G pass=3 blend=ADD:ONE:ONE quad=1 upg=0x00 clamp=0 draws=2 vs=00000001 "), "{line}");
        assert!(format_fx_group(&g[1]).starts_with("G pass=2 blend=off quad=0 upg=0x00 clamp=0 draws=1 "), "{}", format_fx_group(&g[1]));
        assert!(format_fx_group(&g[0]).contains(" blend=unknown "));
        assert!(!line.contains("  "));
    }

    #[test]
    fn fx_log_text_has_headers_per_pass_and_the_group_lines() {
        let f = fx_frame();
        let recs = vec![fx(3, DrawArgs::indexed(6), 1, Some(ADD)), fx(2, DrawArgs::draw(4), 3, Some(ADD)), fx(0, DrawArgs::draw(4), 3, None), fx(1, DrawArgs::draw(4), 3, None)];
        let (text, summary) = build_fx_log(7, 8, 0x10, &f, (100, 50), recs, 2, Some((0, "classifier".to_string()))).unwrap();
        assert_eq!(summary, "2 draws in 2 passes, 2 groups");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "# gw2g-draws-fx v1");
        assert_eq!(lines[1], "# unix=7 frame=8 ctx=0x10 screen=100x50");
        assert_eq!(lines[2], "# opaque_pass=0 how=classifier passes=2 captured=2 dropped=2 groups=2");
        assert_eq!(lines[3], format!("# fxpass=2 rt0={} depth=none pass_draws=63 captured=1 groups=1", format_name(10)));
        assert_eq!(lines[4], format!("# fxpass=3 rt0={} depth={} pass_draws=556 captured=1 groups=1", format_name(10), format_name(20)));
        assert!(lines[5].starts_with("G pass=2 blend=ADD:ONE:ONE quad=1 "), "{}", lines[5]);
        assert!(lines[6].starts_with("G pass=3 "), "{}", lines[6]);
        assert_eq!(lines.len(), 7);
    }

    #[test]
    fn fx_log_ignores_other_contexts_and_says_why_when_empty() {
        let f = fx_frame();
        let mut other = fx(3, DrawArgs::draw(4), 1, None);
        other.rec.ctx = 0x77;
        assert!(build_fx_log(1, 2, 0x10, &f, (100, 50), vec![other], 0, Some((0, "x".into()))).unwrap_err().contains("no effects draws"));
        // Only the opaque pass drew: nothing to write.
        assert!(build_fx_log(1, 2, 0x10, &f, (100, 50), vec![fx(0, DrawArgs::draw(4), 1, None)], 0, Some((0, "x".into()))).is_err());
        // Unknown opaque pass: header says none and every full-size pass is covered.
        let (text, _) = build_fx_log(1, 2, 0x10, &f, (100, 50), vec![fx(0, DrawArgs::draw(4), 1, None)], 0, None).unwrap();
        assert!(text.contains("# opaque_pass=none how=unknown passes=3 "), "{text}");
    }

    #[test]
    fn opaque_choice_is_the_classifier_or_the_busiest_captured_pass() {
        assert_eq!(opaque_choice(&[], (1, 1), std::iter::empty()), None);
        let (p, how) = opaque_choice(&[], (1, 1), [4usize, 4, 5].into_iter()).unwrap();
        assert_eq!(p, 4);
        assert!(how.starts_with("fallback_most_draws_"), "{how}");
        let f = crate::passes::parse_frame_log(crate::frame_fixture::FRAME_PASSES);
        let (p, how) = opaque_choice(&f, (3440, 1440), std::iter::empty()).unwrap();
        assert_eq!((p, how.as_str()), (classify(&f, (3440, 1440)).unwrap().opaque, "classifier"));
    }

    #[test]
    fn the_evidence_frame_has_full_size_effects_passes_besides_the_opaque_one() {
        let f = crate::passes::parse_frame_log(crate::frame_fixture::FRAME_PASSES);
        let screen = (3440, 1440);
        let opaque = classify(&f, screen).unwrap().opaque;
        let fxp = fx_passes(&f, screen, Some(opaque));
        assert!(!fxp.contains(&opaque));
        // Full-size passes other than the opaque one, with and without depth (this frame numbers the opaque pass #3).
        assert_eq!(opaque, 3);
        assert!(fxp.iter().all(|&p| f[p].rt[0].w == 3440 && f[p].rt[0].h == 1440), "{fxp:?}");
        assert!(fxp.iter().any(|&p| f[p].depth.is_none()), "{fxp:?}");
        assert!(fxp.contains(&4) && fxp.contains(&9), "{fxp:?}");
    }

    // ---- WARP: the real Get-call reader on a real context ----

    #[test]
    fn read_blend_reads_rt0_of_a_real_context_and_does_not_leak() {
        let mut dev = None;
        let mut ctx = None;
        let r = unsafe { D3D11CreateDevice(None::<&windows::Win32::Graphics::Dxgi::IDXGIAdapter>, D3D_DRIVER_TYPE_WARP, HMODULE::default(), D3D11_CREATE_DEVICE_FLAG(0), None, D3D11_SDK_VERSION, Some(&mut dev), None, Some(&mut ctx)) };
        let (Ok(()), Some(dev), Some(ctx)) = (r, dev, ctx) else {
            println!("skipped: WARP device creation failed");
            return;
        };
        unsafe {
            // Default (nothing bound): off.
            assert_eq!(read_blend(ctx.as_raw()), Some(Blend::default()));
            let mut bd = D3D11_BLEND_DESC::default();
            bd.RenderTarget[0] = D3D11_RENDER_TARGET_BLEND_DESC {
                BlendEnable: true.into(),
                SrcBlend: D3D11_BLEND_SRC_ALPHA,
                DestBlend: D3D11_BLEND_ONE,
                BlendOp: D3D11_BLEND_OP_ADD,
                SrcBlendAlpha: D3D11_BLEND_ZERO,
                DestBlendAlpha: D3D11_BLEND_ONE,
                BlendOpAlpha: D3D11_BLEND_OP_MAX,
                RenderTargetWriteMask: 15,
            };
            let mut bs = None;
            dev.CreateBlendState(&bd, Some(&mut bs)).unwrap();
            let bs = bs.unwrap();
            ctx.OMSetBlendState(&bs, None, 0xffff_ffff);
            let before = refs(&bs);
            let b = read_blend(ctx.as_raw()).unwrap();
            assert_eq!(before, refs(&bs), "read_blend leaked or dropped a reference");
            assert_eq!(b, Blend { enable: true, src: 5, dst: 2, op: 1, src_a: 1, dst_a: 2, op_a: 5 });
            assert_eq!(blend_text(Some(b)), "ADD:SRC_ALPHA:ONE|MAX:ZERO:ONE");
        }
    }

    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::*;
    use windows::Win32::Graphics::Direct3D11::*;
    use windows::Win32::Graphics::Dxgi::Common::*;
    use windows::core::{Interface, s};

    const VS: &str = "cbuffer Bones : register(b0) { float4 m[16]; };\nstruct O { float4 pos : SV_POSITION; };\nO main(float3 p : POSITION) { O o; o.pos = float4(p, 1) + m[3]; return o; }";
    const PS: &str = "Texture2D a : register(t0); Texture2D b : register(t3); SamplerState s : register(s0);\ncbuffer C : register(b0) { float4 k; };\nfloat4 main(float4 pos : SV_POSITION) : SV_Target { return a.Load(int3(0,0,0)) + b.Load(int3(0,0,0)) + k; }";

    #[test]
    fn read_bound_reads_the_state_of_a_real_context_and_does_not_leak_references() {
        let mut dev = None;
        let mut ctx = None;
        let r = unsafe { D3D11CreateDevice(None::<&windows::Win32::Graphics::Dxgi::IDXGIAdapter>, D3D_DRIVER_TYPE_WARP, HMODULE::default(), D3D11_CREATE_DEVICE_FLAG(0), None, D3D11_SDK_VERSION, Some(&mut dev), None, Some(&mut ctx)) };
        let (Ok(()), Some(dev), Some(ctx)) = (r, dev, ctx) else {
            println!("skipped: WARP device creation failed");
            return;
        };
        let (Ok(vs_code), Ok(ps_code)) = (shaders::compile(VS, "main", "vs_5_0"), shaders::compile(PS, "main", "ps_5_0")) else {
            println!("skipped: d3dcompiler not available");
            return;
        };
        unsafe {
            let mut vs = None;
            dev.CreateVertexShader(&vs_code, None, Some(&mut vs)).unwrap();
            let vs = vs.unwrap();
            let mut ps = None;
            dev.CreatePixelShader(&ps_code, None, Some(&mut ps)).unwrap();
            let ps = ps.unwrap();
            // The registries the hooks fill (private copies here, the hooks are not installed in a unit test).
            let (vs_reg, ps_reg) = (Registry::new(), Registry::new());
            vs_reg.record(vs.as_raw() as usize, &vs_code);
            ps_reg.record(ps.as_raw() as usize, &ps_code);

            let elem = [D3D11_INPUT_ELEMENT_DESC { SemanticName: s!("POSITION"), SemanticIndex: 0, Format: DXGI_FORMAT_R32G32B32_FLOAT, InputSlot: 0, AlignedByteOffset: 0, InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA, InstanceDataStepRate: 0 }];
            let mut layout = None;
            dev.CreateInputLayout(&elem, &vs_code, Some(&mut layout)).unwrap();
            let layout = layout.unwrap();

            let buf = |bytes: u32, bind: D3D11_BIND_FLAG| {
                let mut b = None;
                dev.CreateBuffer(&D3D11_BUFFER_DESC { ByteWidth: bytes, Usage: D3D11_USAGE_DEFAULT, BindFlags: bind.0 as u32, ..Default::default() }, None, Some(&mut b)).unwrap();
                b.unwrap()
            };
            let vb = buf(36, D3D11_BIND_VERTEX_BUFFER);
            let ib = buf(12, D3D11_BIND_INDEX_BUFFER);
            let vcb = buf(256, D3D11_BIND_CONSTANT_BUFFER);
            let pcb = buf(16, D3D11_BIND_CONSTANT_BUFFER);

            let tex = |w: u32, h: u32, mips: u32, fmt: DXGI_FORMAT| {
                let d = D3D11_TEXTURE2D_DESC { Width: w, Height: h, MipLevels: mips, ArraySize: 1, Format: fmt, SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, Usage: D3D11_USAGE_DEFAULT, BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32, CPUAccessFlags: 0, MiscFlags: 0 };
                let mut t = None;
                dev.CreateTexture2D(&d, None, Some(&mut t)).unwrap();
                let t = t.unwrap();
                let mut v = None;
                dev.CreateShaderResourceView(&t, None, Some(&mut v)).unwrap();
                (t, v.unwrap())
            };
            let (t0, v0) = tex(64, 32, 3, DXGI_FORMAT_R8G8B8A8_UNORM);
            let (t3, v3) = tex(16, 16, 1, DXGI_FORMAT_R16G16B16A16_FLOAT);

            ctx.IASetInputLayout(&layout);
            ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            ctx.IASetVertexBuffers(0, 1, Some(&Some(vb.clone())), Some(&12u32), Some(&0u32));
            ctx.IASetIndexBuffer(&ib, DXGI_FORMAT_R16_UINT, 0);
            ctx.VSSetShader(&vs, None);
            ctx.PSSetShader(&ps, None);
            ctx.VSSetConstantBuffers(0, Some(&[Some(vcb.clone())]));
            ctx.PSSetConstantBuffers(0, Some(&[Some(pcb.clone())]));
            ctx.PSSetShaderResources(0, Some(&[Some(v0.clone())]));
            ctx.PSSetShaderResources(3, Some(&[Some(v3.clone())]));

            let before = (refs(&layout), refs(&vb), refs(&vcb), refs(&pcb), refs(&v0), refs(&v3), refs(&t0), refs(&t3), refs(&vs), refs(&ps));
            let b = read_bound(ctx.as_raw());
            let after = (refs(&layout), refs(&vb), refs(&vcb), refs(&pcb), refs(&v0), refs(&v3), refs(&t0), refs(&t3), refs(&vs), refs(&ps));
            assert_eq!(before, after, "read_bound leaked or dropped a reference");

            assert_eq!(b.vs, vs.as_raw() as usize);
            assert_eq!(b.ps, ps.as_raw() as usize);
            assert_eq!(b.layout, layout.as_raw() as usize);
            assert_eq!(b.stride, 12);
            assert_eq!(b.vcb, [256, 0, 0, 0]);
            assert_eq!(b.pcb, [16, 0, 0, 0]);
            assert_eq!(b.srvs.len(), 2);
            assert_eq!(b.srvs[0], (TexSig { slot: 0, fmt: 28, w: 64, h: 32, mips: 3 }, t0.as_raw() as usize));
            assert_eq!(b.srvs[1], (TexSig { slot: 3, fmt: 10, w: 16, h: 16, mips: 1 }, t3.as_raw() as usize));

            // Through the same helpers the hook uses: record + aggregate + format, with a real draw in between.
            let mut recs = Vec::new();
            for count in [3u32, 3, 6] {
                let a = DrawArgs::indexed(count);
                recs.push(to_record(ctx.as_raw() as usize, 1, a, read_bound(ctx.as_raw()), &vs_reg, &ps_reg));
                ctx.DrawIndexed(count, 0, 0);
            }
            let g = aggregate(&recs);
            assert_eq!(g.len(), 1);
            assert_eq!((g[0].draws, g[0].indices, g[0].max_indices), (3, 12, 6));
            let line = format_group(&g[0]);
            assert!(line.contains(&format!("vs={:08x} ps={:08x}", shaders::fnv1a32(&vs_code), shaders::fnv1a32(&ps_code))), "{line}");
            assert!(line.contains("vcb=256..256 vcbs=256,0,0,0 pcbs=16,0,0,0 layouts=1 strides=12 texptrs=2 tex=0:R8G8B8A8_UNORM:64x32m3,3:R16G16B16A16_FLOAT:16x16m1"), "{line}");

            // Nothing bound: all zero, no crash.
            ctx.ClearState();
            let e = read_bound(ctx.as_raw());
            assert_eq!(e, Bound::default());
        }
    }

    /// Reference count of a COM object (AddRef then Release on the raw pointer).
    unsafe fn refs<T: Interface>(o: &T) -> u32 {
        unsafe {
            let p = o.as_raw();
            let add: unsafe extern "system" fn(*mut std::ffi::c_void) -> u32 = std::mem::transmute(vfn(p, 1));
            let rel: unsafe extern "system" fn(*mut std::ffi::c_void) -> u32 = std::mem::transmute(vfn(p, 2));
            add(p);
            rel(p)
        }
    }
}
