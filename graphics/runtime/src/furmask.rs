//! Fur mask: which pixels of the picture belong to the draws the fur selector calls "fur" (the charr).
//!
//! Nothing in the engine's data says which draw is a charr, and the scene alpha turned out unusable as a mask. So the
//! selection is a live-tunable guess on what a draw IS (`fur_ps`, `fur_vs`, `fur_min_vcb` in config.ini, hot
//! reloadable), and the mask is built from the draws themselves:
//!
//! * the `PSSetShader` hook tells `on_ps_set` the hash of the pixel shader that is bound now (one atomic per call when
//!   the feature is off);
//! * the `Draw*` hooks call `before_draw` BEFORE issuing the engine's draw. When the bound pixel shader is in `fur_ps`,
//!   the vertex shader is in `fur_vs` (if given) and vertex constant buffer 0 is at least `fur_min_vcb` bytes (skinned
//!   meshes), the SAME draw is issued once more into the mod's own R8 target with a pixel shader that writes 1.0
//!   (`shaders/furmask.hlsl`): same vertex shader, input layout, vertex / index / constant buffers, viewport,
//!   rasteriser state and the engine's own depth buffer, bound as a read-only depth test (our depth-stencil state
//!   copies the engine's depth function and enable flag and turns the depth WRITE off, stencil off). The engine's
//!   render targets are not touched: only our R8 target is bound as RTV0 for that one draw and `StateGuard`
//!   restores everything. Because it runs before the engine's draw, the depth buffer holds exactly the geometry
//!   drawn earlier in the pass, so the character is occluded correctly by what was drawn before it;
//! * `light_adjust_hdr.hlsl` reads the mask as `t20` (`on_light_adjust` binds it) and applies `fur_look` only there.
//!
//! THE MESH UV. The mask target is R16G16B16A16_UNORM: a = the mask, rg = frac of the mesh uv, b = 1 when rg is valid. The strands
//! of `fur_look` are procedural noise in that uv space, so they stay glued to the mesh. The uv comes from the vertex shader of the
//! draw itself: the `CreateVertexShader` hook parses its output signature (`note_vs`), the draw's mask pixel shader is GENERATED
//! per signature (`uv_ps_hlsl`: inputs replicate the vertex outputs so the registers match; the TEXCOORD with two float
//! components, or `fur_uv`, is written out), compiled on a background thread, checked against the vertex signature (`verify_link`)
//! and created on the render thread when ready. Until then, or for a vertex shader with no usable uv, the draw writes the
//! mask only (`shaders/furmask.hlsl`, b = 0) and `fur_look` skips the strands there. `status_fur=` says `uv: ok / partial / none`.
//!
//! The mask is cleared lazily by the first masked draw of a frame (equivalent to a clear at frame begin, but a frame
//! without a masked draw costs nothing and `on_light_adjust` then binds nothing, so a stale mask is never read).
//!
//! OCCLUSION BY LATER GEOMETRY (z-stamp + validate, fail open). The depth test above only knows what was drawn BEFORE the
//! character; a hood drawn after the head covers it in the picture but used to leave the head's mask pixels set (the rim band and
//! the strands then painted the head's outline onto the hood). So each mask has an R32_FLOAT z-stamp of the same size, bound as
//! RTV1 beside it: the generated mask shaders write `SV_Target1 = SV_Position.z` (the device depth the test used), the stamp is
//! cleared to -1 (nothing stamped). At the start of the light-adjust draw (`on_light_adjust`, once per frame and channel)
//! `Mask::validate` draws a full-screen pass into the mask that reads the stamp and the engine's FINAL depth (a shader view of the
//! depth buffer the masked draws used; typeless formats only, `depth_srv_format`) and writes 0 where a later surface covers the
//! stamped fragment, by the depth test the character used (`occ_mode`, tolerance `OCC_TOL`). Whenever the check cannot run (typed
//! depth, no SHADER_RESOURCE bind, another depth test, shaders missing) the mask is kept as it is; `status_fur=` / `status_rim=`
//! say `occlusion: checked / unavailable (why) / skipped (...) / off (fur_occlusion=0)`. THE STAMP IS THE NEAREST FRAGMENT: our
//! mask draw writes no depth, so the fragments of ONE draw do not occlude each other (a farther layer of the same mesh rasterised
//! later passes the read-only test too); the stamp is therefore blended with MIN (MAX for reversed Z, `stamp_blend_desc`; the mask
//! itself is not blended), cleared to 2 (-1 for reversed Z) = nothing. Without that the stamp was the LAST fragment and the check
//! erased the inside of every mesh that overlaps itself (the charr tail: only a band along the silhouette was left). `fur_occlusion=0`
//! bypasses the check (A/B), and the debug capture's `status_occlusion=` reads the masks back once and emulates the check on the CPU
//! (`probe_counts`). The static `ps_mask` / `ps_rim` cannot read SV_Position (a pixel
//! shader input must sit on the vertex shader's register), so they stamp -1 and their pixels are never erased; the shaders
//! generated per vertex signature (`uv_ps_hlsl`, `z_ps_hlsl`) stamp.
//!
//! LIMITS (on purpose): (1) the occlusion check only sees later geometry that WROTE depth (transparent surfaces do not erase,
//! depth-writing particles do); (2) the mask is the union of all selected draws: a
//! selector that also matches other races or armour paints them too (that is what `fur_debug` is for); (3) pixel
//! shaders that `discard` (alpha-tested cards) are drawn solid in the mask; (4) stencil is ignored.
//!
//! A SECOND, independent channel uses the same machinery for the RIM LIGHT (`docs/rim.md`): its own selector (`rim_ps`, `rim_min_vcb`; the
//! default is the verified charr skin), its own R8 target bound as `t23` and its own debug view. A draw can be in both lists: it is then
//! issued once for each mask. The rim channel skips draws whose blend state is an alpha blend (stealth / transparent characters render that
//! way) and is switched off by the picker highlight. `rim_strength` 0 (and `rim_debug` 0) = the channel is not wanted: no draws, no target.
//!
//! The mask shader is compiled once on a background thread (never `D3DCompile` on the render thread); until then
//! nothing is masked. Every hook entry is `catch_unwind`-guarded and never blocks (`try_lock` on the device state).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};
use std::sync::{Arc, Mutex, OnceLock};

use crate::d3d::{self, IgnoreScope, Obj, StateGuard, Tex2dDesc, Viewport, ignoring, release, vfn, vt};
use crate::log;
use crate::shafts::{NO_TARGET, Target, make_target};
use crate::{gate, recorder, shaders};

pub const HLSL: &str = include_str!("../shaders/furmask.hlsl");
pub const PS_ENTRY: &str = "ps_mask";
/// `t20` of the light-adjust substitute (t3, t4, t6..t9 are taken; the shafts use t10..t18 for their own passes).
pub const SRV_SLOT: u32 = 20;
/// `t23` of the light-adjust substitute: the rim mask (R8_UNORM, screen size, 1 where a `rim_ps` draw is visible).
pub const RIM_SRV_SLOT: u32 = 23;
/// Entry of the rim mask shader (writes 1.0 to every channel).
pub const PS_RIM_ENTRY: &str = "ps_rim";
/// DXGI_FORMAT_R8_UNORM.
const RIM_FORMAT: u32 = 61;
/// `ID3D11BlendState::GetDesc` (IUnknown 3 + DeviceChild 4).
const BS_GET_DESC: usize = 7;
/// Hashes kept per list.
pub const MAX_LIST: usize = 32;
pub const DEFAULT_MIN_VCB: u32 = 4096;
pub const MAX_VCB: u32 = 1_048_576;

/// DXGI_FORMAT_R16G16B16A16_UNORM: 16 bit uv precision (the strand scale is taken from uv differences between neighbouring pixels).
const MASK_FORMAT: u32 = 11;
/// `ID3D11Device::CreateDepthStencilState`.
const DEV_CREATE_DEPTH_STENCIL_STATE: usize = 21;
/// `ID3D11DepthStencilState::GetDesc` (IUnknown 3 + DeviceChild 4).
const DSS_GET_DESC: usize = 7;
/// `ID3D11RasterizerState::GetDesc` (IUnknown 3 + DeviceChild 4).
pub(crate) const RS_GET_DESC: usize = 7;
/// `ID3D11DeviceContext::ClearRenderTargetView`.
const CTX_CLEAR_RTV: usize = 50;

// ---- the occlusion check (z-stamp + validate, fail open) -------------------------------------------------

/// DXGI_FORMAT_R32_FLOAT: the z-stamp beside each mask (the device depth of the fragment that wrote the mask pixel; -1 = nothing).
const STAMP_FORMAT: u32 = 41;
/// Smallest absolute tolerance in device depth of the occlusion check (`OCC_TOL` in `shaders/furmask.hlsl`, the same value). The stamp
/// is the float depth of the fragment as the pixel shader sees it, the engine stores it rounded to D24 (one step = 6e-8). It was 2e-6;
/// the game showed a CONSTANT gap of 3.87e-6 (65 D24 steps, the final depth nearer than the stamp) on 90..98 % of the visible
/// mask, so the check erased almost the whole character: the engine draws characters with a depth bias that the pixel shader's
/// SV_Position.z does not carry on that GPU (WARP does include it). 1e-5 (168 steps) absorbs such a bias; the bias of the masked draws'
/// rasterizer state is added on top (`bias_tol`). Real occluders (a hood over the head) are 1e-3 and more away (the probe of the same
/// captures: the erased pixels with a real gap are all above 1e-3).
#[allow(dead_code)] // the shader has its own copy; a test keeps the two equal
pub const OCC_TOL: f32 = 1e-5;
/// The largest tolerance the depth bias of the masked draws can add (a slope-scaled bias without a clamp has no bound of its own).
pub const OCC_TOL_MAX: f32 = 2e-4;

/// The device-depth tolerance one masked draw needs on top of `OCC_TOL` for its rasterizer state's depth bias (`rs` = the words of a
/// D3D11_RASTERIZER_DESC: FillMode, CullMode, FrontCounterClockwise, DepthBias (int), DepthBiasClamp, SlopeScaledDepthBias, ...) on a
/// depth buffer of `depth_fmt`: |DepthBias| x r (r = 2^-24 for D24 and float depth below 1, 2^-16 for D16) plus, for a slope-scaled
/// bias, its clamp (or 1e-4 without one); capped at `OCC_TOL_MAX`. NaN / garbage give 0.
pub fn bias_tol(rs: &[u32; 10], depth_fmt: u32) -> f32 {
    let bias = (rs[3] as i32).unsigned_abs() as f32;
    let clamp = f32::from_bits(rs[4]).abs();
    let slope = f32::from_bits(rs[5]).abs();
    let r = match depth_fmt {
        53 | 55 | 56 => 1.0 / 65536.0,
        _ => 1.0 / 16_777_216.0,
    };
    let mut t = bias * r;
    if slope.is_finite() && slope > 0.0 {
        t += if clamp.is_finite() && clamp > 0.0 { clamp } else { 1e-4 };
    }
    if t.is_finite() { (t * 1.5).min(OCC_TOL_MAX) } else { 0.0 }
}

/// The depth bias of the last masked draw's rasterizer state (DepthBias, DepthBiasClamp bits, SlopeScaledDepthBias bits), for the probe text.
static LAST_BIAS: [AtomicU32; 3] = [AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0)];
pub const VS_OCC_ENTRY: &str = "vs_main";
pub const PS_OCC_LESS: &str = "ps_occ_less";
pub const PS_OCC_GREATER: &str = "ps_occ_greater";
/// The per-channel depth test of a frame: the engine's DepthFunc (D3D11_COMPARISON_*), 0 = depth test off, `TEST_MIXED` = the
/// selected draws of that frame did not all use the same test / the same depth buffer (the check is skipped then).
const TEST_MIXED: u32 = 0xff;
/// D3D11_BIND_SHADER_RESOURCE.
const BIND_SRV: u32 = 0x8;
/// The z-stamp's clear value = "nothing stamped" (any stamp outside 0..1 is): 2 for LESS / LESS_EQUAL (the stamp is blended with
/// MIN, so the first real fragment replaces it), -1 for GREATER / GREATER_EQUAL (blended with MAX). The static fallback shaders
/// write -1 without blending: under MIN that marks the pixel unverifiable for the rest of the frame (kept, fail open), under MAX a
/// later nearer selected draw replaces it (correct: it passed the depth test, so it is the visible one).
pub const STAMP_CLEAR_LESS: f32 = 2.0;
pub const STAMP_CLEAR_GREATER: f32 = -1.0;
/// `ID3D11Device::CheckFormatSupport`, and D3D11_FORMAT_SUPPORT_BLENDABLE.
const DEV_CHECK_FORMAT_SUPPORT: usize = 29;
const FORMAT_SUPPORT_BLENDABLE: u32 = 0x8000;
/// D3D11_BLEND_OP_MIN / MAX.
const BLEND_OP_MIN: u32 = 4;
const BLEND_OP_MAX: u32 = 5;

/// The words of the D3D11_BLEND_DESC of the stamp draws: IndependentBlendEnable, render target 0 (the mask) NOT blended, render
/// target 1 (the z-stamp) blended with `op` (MIN / MAX keep the nearest fragment of all the selected draws; the factors are ignored).
pub fn stamp_blend_desc(op: u32) -> [u32; 66] {
    let mut d = [0u32; 66];
    d[1] = 1; // IndependentBlendEnable
    for rt in 0..8 {
        // BlendEnable 0, SrcBlend ONE (2), DestBlend ZERO (1), BlendOp ADD (1), SrcBlendAlpha ONE, DestBlendAlpha ZERO, BlendOpAlpha ADD, write mask all.
        d[2 + rt * 8..2 + rt * 8 + 8].copy_from_slice(&[0, 2, 1, 1, 2, 1, 1, 0xf]);
    }
    d[2 + 8..2 + 16].copy_from_slice(&[1, 2, 2, op, 2, 2, op, 0xf]);
    d
}

/// The shader-resource format that reads the depth of a typeless depth texture, or `None` (a typed depth format cannot be viewed
/// by a shader at all): R24G8_TYPELESS -> R24_UNORM_X8_TYPELESS, R32_TYPELESS -> R32_FLOAT, R16_TYPELESS -> R16_UNORM,
/// R32G8X24_TYPELESS -> R32_FLOAT_X8X24_TYPELESS (21; 20 is D32_FLOAT_S8X24_UINT, a depth-stencil format).
pub fn depth_srv_format(tex_format: u32) -> Option<u32> {
    match tex_format {
        44 => Some(46),
        39 => Some(41),
        53 => Some(56),
        19 => Some(21),
        _ => None,
    }
}

/// Which comparison the check uses for a frame's depth test: `Some(false)` = LESS / LESS_EQUAL (occluded iff final < stamp - tol),
/// `Some(true)` = GREATER / GREATER_EQUAL (reversed Z: occluded iff final > stamp + tol), `None` = anything else (no check).
pub fn occ_mode(test: u32) -> Option<bool> {
    match test {
        2 | 4 => Some(false),
        5 | 7 => Some(true),
        _ => None,
    }
}

fn test_name(test: u32) -> &'static str {
    match test {
        0 => "off",
        1 => "NEVER",
        2 => "LESS",
        3 => "EQUAL",
        4 => "LESS_EQUAL",
        5 => "GREATER",
        6 => "NOT_EQUAL",
        7 => "GREATER_EQUAL",
        8 => "ALWAYS",
        TEST_MIXED => "mixed within the frame",
        _ => "unknown",
    }
}

/// Why the occlusion check could not run (static, or cached per depth resource: no allocation per frame).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reason {
    Static(&'static str),
    Depth(Arc<str>),
}

impl Reason {
    pub fn as_str(&self) -> &str {
        match self {
            Reason::Static(s) => s,
            Reason::Depth(s) => s,
        }
    }
}

/// Result of one occlusion check of a channel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Occ {
    /// The full-screen pass ran: covered mask pixels are 0 now.
    Checked,
    /// The frame's depth test is not one the check understands (`test_name`): the mask is kept as it is.
    Skipped(u32),
    /// Fail open: the mask is kept as it is.
    Unavailable(Reason),
}

/// The compiled shaders of the check (the worker thread compiles them at init).
pub struct OccBlobs {
    pub vs: Vec<u8>,
    pub less: Vec<u8>,
    pub greater: Vec<u8>,
}

/// Compiles the check shaders (D3DCompile: never on the render thread).
pub fn compile_occ_blobs() -> Result<OccBlobs, String> {
    let c = |entry: &str, profile: &str| shaders::compile(HLSL, entry, profile).map_err(|e| format!("{entry}: {e}"));
    Ok(OccBlobs { vs: c(VS_OCC_ENTRY, "vs_5_0")?, less: c(PS_OCC_LESS, "ps_5_0")?, greater: c(PS_OCC_GREATER, "ps_5_0")? })
}

static OCC_BLOBS: OnceLock<Result<OccBlobs, String>> = OnceLock::new();
/// The check of this frame ran (or was attempted) for the fur / the rim channel; reset with `FRESH` / `RIM_FRESH`.
static OCC_DONE: AtomicBool = AtomicBool::new(false);
static RIM_OCC_DONE: AtomicBool = AtomicBool::new(false);
/// Occlusion state of the current / the last frame, per channel (`occ_code`).
static OCC_CUR: AtomicU32 = AtomicU32::new(0);
static OCC_LAST: AtomicU32 = AtomicU32::new(0);
static RIM_OCC_CUR: AtomicU32 = AtomicU32::new(0);
static RIM_OCC_LAST: AtomicU32 = AtomicU32::new(0);
/// The reason behind an `unavailable` state, per channel (rewritten only when it changes).
static OCC_WHY: Mutex<String> = Mutex::new(String::new());
static RIM_OCC_WHY: Mutex<String> = Mutex::new(String::new());
static OCC_LOGGED: AtomicBool = AtomicBool::new(false);

const OCC_NONE: u32 = 0;
const OCC_CHECKED: u32 = 1;
const OCC_UNAVAILABLE: u32 = 2;
const OCC_SKIPPED: u32 = 3;
const OCC_PENDING: u32 = 4;
const OCC_OFF: u32 = 5;

/// `fur_occlusion` (config.ini, live, default 1): 0 bypasses the occlusion check of BOTH masks (fur and rim), which then stay as
/// drawn (the behaviour before the check existed; the A/B switch for the game).
static OCC_ON: AtomicBool = AtomicBool::new(true);

pub fn set_occlusion(on: bool) {
    OCC_ON.store(on, Relaxed);
}

pub fn occlusion() -> bool {
    OCC_ON.load(Relaxed)
}

/// Records the result of a check for the status line (and logs an unavailable check once).
fn note_occ(rim: bool, r: &Occ) {
    let (cur, why) = if rim { (&RIM_OCC_CUR, &RIM_OCC_WHY) } else { (&OCC_CUR, &OCC_WHY) };
    let code = match r {
        Occ::Checked => OCC_CHECKED,
        Occ::Skipped(t) => OCC_SKIPPED | (t & 0xff) << 8,
        Occ::Unavailable(reason) => {
            let mut w = why.lock().unwrap_or_else(|e| e.into_inner());
            if w.as_str() != reason.as_str() {
                *w = reason.as_str().to_string();
            }
            if !OCC_LOGGED.swap(true, Relaxed) {
                log!("furmask: occlusion check unavailable: {}", reason.as_str());
            }
            OCC_UNAVAILABLE
        }
    };
    cur.store(code, Relaxed);
}

/// `occlusion: checked` / `occlusion: unavailable (why)` / `occlusion: skipped (depth test X)` for the status lines.
fn occ_text(code: u32, why: &Mutex<String>) -> String {
    match code & 0xff {
        OCC_CHECKED => "occlusion: checked".to_string(),
        OCC_UNAVAILABLE => format!("occlusion: unavailable ({})", why.lock().unwrap_or_else(|e| e.into_inner())),
        OCC_SKIPPED => format!("occlusion: skipped (depth test {})", test_name(code >> 8)),
        OCC_PENDING => "occlusion: pending (compiling the check shaders)".to_string(),
        OCC_OFF => "occlusion: off (fur_occlusion=0)".to_string(),
        _ => "occlusion: n/a (no check ran)".to_string(),
    }
}

// ---- the selector (pure) -------------------------------------------------------------------------------

/// Parses a comma-separated list of shader hashes: 1 to 8 hex digits each (an optional `0x`), spaces ignored, bad entries
/// skipped, duplicates dropped (first occurrence wins), at most `MAX_LIST`.
pub fn parse_hashes(text: &str) -> Vec<u32> {
    let mut out: Vec<u32> = Vec::new();
    for tok in text.split([',', ';', ' ', '\t']) {
        let t = tok.trim();
        let t = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")).unwrap_or(t);
        if t.is_empty() || t.len() > 8 || !t.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        let Ok(h) = u32::from_str_radix(t, 16) else { continue };
        if !out.contains(&h) && out.len() < MAX_LIST {
            out.push(h);
        }
    }
    out
}

pub(crate) fn join_hashes(v: &[u32]) -> String {
    v.iter().map(|h| format!("{h:08x}")).collect::<Vec<_>>().join(",")
}

/// What counts as a fur draw.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Selector {
    pub ps: Vec<u32>,
    /// Empty = any vertex shader.
    pub vs: Vec<u32>,
    pub min_vcb: u32,
}

impl Selector {
    pub const fn empty() -> Selector {
        Selector { ps: Vec::new(), vs: Vec::new(), min_vcb: DEFAULT_MIN_VCB }
    }

    pub fn new(ps: &str, vs: &str, min_vcb: u32) -> Selector {
        Selector { ps: parse_hashes(ps), vs: parse_hashes(vs), min_vcb: min_vcb.min(MAX_VCB) }
    }

    /// The pixel-shader list is the master switch: empty = nothing is a fur draw.
    pub fn is_empty(&self) -> bool {
        self.ps.is_empty()
    }

    pub fn has_ps(&self, ps: u32) -> bool {
        self.ps.contains(&ps)
    }

    /// Is a draw with this pixel shader, vertex shader (`None` = unknown) and vertex constant buffer 0 size (bytes) a fur draw?
    pub fn matches(&self, ps: u32, vs: Option<u32>, vcb0: u32) -> bool {
        self.has_ps(ps) && (self.vs.is_empty() || vs.is_some_and(|v| self.vs.contains(&v))) && vcb0 >= self.min_vcb
    }
}

/// Is the blend state of a draw an alpha blend (a transparent / stealth character)? `desc` = the words of a `D3D11_BLEND_DESC`:
/// AlphaToCoverageEnable, IndependentBlendEnable, then per render target BlendEnable, SrcBlend, DestBlend, BlendOp, SrcBlendAlpha,
/// DestBlendAlpha, BlendOpAlpha, RenderTargetWriteMask (8 words). Only render target 0 counts. D3D11_BLEND: 5 SRC_ALPHA, 6 INV_SRC_ALPHA,
/// 14 BLEND_FACTOR, 15 INV_BLEND_FACTOR. Additive (ONE, ONE) and opaque draws are not transparent.
pub fn blend_is_transparent(desc: &[u32]) -> bool {
    let w = |i: usize| desc.get(i).copied().unwrap_or(0);
    w(2) != 0 && (matches!(w(3), 5 | 14 | 15) || matches!(w(4), 6 | 15))
}

// ---- live state ----------------------------------------------------------------------------------------

static SELECTOR: Mutex<Selector> = Mutex::new(Selector::empty());
/// The selector has a pixel-shader list (so the hooks have anything to do).
static NONEMPTY: AtomicBool = AtomicBool::new(false);
static DEBUG: AtomicBool = AtomicBool::new(false);
static MIN_VCB: AtomicU32 = AtomicU32::new(DEFAULT_MIN_VCB);
/// The pixel shader bound on the game's context is in `fur_ps` (and the feature is on).
static MATCH: AtomicBool = AtomicBool::new(false);
/// Hash of that pixel shader (valid while `MATCH`).
static CUR_PS: AtomicU32 = AtomicU32::new(0);
/// The mask must be cleared by the next masked draw (a new frame started).
static FRESH: AtomicBool = AtomicBool::new(true);
/// At least one draw went into the mask this frame.
static DRAWN: AtomicBool = AtomicBool::new(false);
static CUR_DRAWS: AtomicU32 = AtomicU32::new(0);
static CUR_CAND: AtomicU32 = AtomicU32::new(0);
static CUR_REJ: AtomicU32 = AtomicU32::new(0);
static CUR_UV: AtomicU32 = AtomicU32::new(0);
static LAST_DRAWS: AtomicU32 = AtomicU32::new(0);
static LAST_CAND: AtomicU32 = AtomicU32::new(0);
static LAST_REJ: AtomicU32 = AtomicU32::new(0);
static LAST_UV: AtomicU32 = AtomicU32::new(0);
static FIRST_DRAW_LOGGED: AtomicBool = AtomicBool::new(false);
static FAILED: AtomicBool = AtomicBool::new(false);
/// The picker singles out one group (`furpick::HighlightKey`): only its draws go into the mask, painted magenta, no fur look.
static HL_ACTIVE: AtomicBool = AtomicBool::new(false);
/// Pixel-shader hash of the highlight (valid while `HL_ACTIVE`): the cheap pre-filter of `on_ps_set`.
static HL_PS: AtomicU32 = AtomicU32::new(0);
static HL: Mutex<Option<crate::furpick::HighlightKey>> = Mutex::new(None);

/// Result of the background compile of the mask pixel shader.
static BLOB: OnceLock<Result<Vec<u8>, String>> = OnceLock::new();
/// The same for the rim mask pixel shader (`ps_rim`).
static BLOB_RIM: OnceLock<Result<Vec<u8>, String>> = OnceLock::new();

// The rim channel (docs/rim.md): the same state as above under its own names.
static RIM_SELECTOR: Mutex<Selector> = Mutex::new(Selector::empty());
static RIM_NONEMPTY: AtomicBool = AtomicBool::new(false);
static RIM_DEBUG: AtomicBool = AtomicBool::new(false);
static RIM_MIN_VCB: AtomicU32 = AtomicU32::new(DEFAULT_MIN_VCB);
static RIM_MATCH: AtomicBool = AtomicBool::new(false);
static RIM_CUR_PS: AtomicU32 = AtomicU32::new(0);
static RIM_FRESH: AtomicBool = AtomicBool::new(true);
static RIM_DRAWN: AtomicBool = AtomicBool::new(false);
static RIM_H: AtomicU32 = AtomicU32::new(0);
static RIM_CUR_DRAWS: AtomicU32 = AtomicU32::new(0);
static RIM_CUR_CAND: AtomicU32 = AtomicU32::new(0);
static RIM_CUR_REJ: AtomicU32 = AtomicU32::new(0);
static RIM_CUR_BLEND: AtomicU32 = AtomicU32::new(0);
static RIM_LAST_DRAWS: AtomicU32 = AtomicU32::new(0);
static RIM_LAST_CAND: AtomicU32 = AtomicU32::new(0);
static RIM_LAST_REJ: AtomicU32 = AtomicU32::new(0);
static RIM_LAST_BLEND: AtomicU32 = AtomicU32::new(0);
static RIM_FIRST_LOGGED: AtomicBool = AtomicBool::new(false);

/// Sets the selector and the debug switch (live; hot reload and startup).
pub fn configure(ps: &str, vs: &str, min_vcb: u32, debug: bool) {
    let sel = Selector::new(ps, vs, min_vcb);
    NONEMPTY.store(!sel.is_empty(), Relaxed);
    MIN_VCB.store(sel.min_vcb, Relaxed);
    DEBUG.store(debug, Relaxed);
    if sel.is_empty() && !HL_ACTIVE.load(Relaxed) {
        MATCH.store(false, Relaxed);
    }
    *SELECTOR.lock().unwrap_or_else(|e| e.into_inner()) = sel;
}

/// Startup: applies the config and starts the background compile of the mask shader.
pub fn init(ps: &str, vs: &str, min_vcb: u32, debug: bool) {
    configure(ps, vs, min_vcb, debug);
    let (p, v) = selector_text();
    log!("furmask: configured (fur_ps [{p}], fur_vs [{v}], fur_min_vcb {}, fur_debug {})", self::min_vcb(), debug as u8);
    let _ = std::thread::Builder::new().name("gw2g-furmask-compile".into()).spawn(|| {
        let rim = shaders::compile(HLSL, PS_RIM_ENTRY, "ps_5_0");
        if let Err(e) = &rim {
            log!("furmask: rim shader compile failed, the rim mask stays off: {e}");
        }
        let _ = BLOB_RIM.set(rim);
        let r = shaders::compile(HLSL, PS_ENTRY, "ps_5_0");
        match &r {
            Ok(_) => log!("furmask: shader compiled"),
            Err(e) => log!("furmask: shader compile failed, the fur mask stays off: {e}"),
        }
        let _ = BLOB.set(r);
        let o = compile_occ_blobs();
        if let Err(e) = &o {
            log!("furmask: occlusion check shaders failed to compile, the masks stay unchecked: {e}");
        }
        let _ = OCC_BLOBS.set(o);
    });
}

/// Sets the rim selector and the debug switch (live; hot reload and startup).
pub fn configure_rim(ps: &str, min_vcb: u32, debug: bool) {
    let sel = Selector::new(ps, "", min_vcb);
    RIM_NONEMPTY.store(!sel.is_empty(), Relaxed);
    RIM_MIN_VCB.store(sel.min_vcb, Relaxed);
    RIM_DEBUG.store(debug, Relaxed);
    if sel.is_empty() {
        RIM_MATCH.store(false, Relaxed);
    }
    *RIM_SELECTOR.lock().unwrap_or_else(|e| e.into_inner()) = sel;
    refresh_rim_consumer();
}

/// The rim selector as written to the log / the dev text.
pub fn rim_selector_text() -> String {
    join_hashes(&RIM_SELECTOR.lock().unwrap_or_else(|e| e.into_inner()).ps)
}

pub fn rim_min_vcb() -> u32 {
    RIM_MIN_VCB.load(Relaxed)
}

pub fn rim_debug() -> bool {
    RIM_DEBUG.load(Relaxed)
}

/// The rim channel is wanted: a selector exists, the picker highlight is not on, and either the rim has strength or the debug view is on.
pub fn wanted_rim() -> bool {
    !HL_ACTIVE.load(Relaxed) && RIM_NONEMPTY.load(Relaxed) && (crate::datapath::rim() > 0.0 || RIM_DEBUG.load(Relaxed))
}

/// Registers (or releases) the depth consumer of the rim light (the shader reads the engine's linear depth for the grazing term and the
/// release in fog); called when the strength or the selector changes.
pub fn refresh_rim_consumer() {
    crate::ao::set_depth_consumer(crate::ao::DEPTH_FOR_RIM, RIM_NONEMPTY.load(Relaxed) && (crate::datapath::rim() > 0.0 || RIM_DEBUG.load(Relaxed)));
}

/// Some rim draw went into the mask this frame and the channel is wanted: the shader gets `t23` and a non-zero `b11`.
pub fn rim_ready() -> bool {
    RIM_DRAWN.load(Relaxed) && wanted_rim()
}

/// Height of the rim mask in pixels (0 before the first masked draw).
pub fn rim_height() -> u32 {
    RIM_H.load(Relaxed)
}

/// One line for the overlay / the dev text (`status_rim=`): what the rim selector did last frame.
pub fn rim_status_text() -> String {
    if !RIM_NONEMPTY.load(Relaxed) {
        return "off (rim_ps is empty)".to_string();
    }
    if !wanted_rim() {
        return if HL_ACTIVE.load(Relaxed) { "paused (picker highlight on)".to_string() } else { "off (rim_strength 0 and rim_debug 0)".to_string() };
    }
    if FAILED.load(Relaxed) {
        return "failed (see log), off".to_string();
    }
    match (BLOB_RIM.get(), BLOB.get()) {
        (Some(Err(_)), _) | (_, Some(Err(_))) => return "failed (shader compile, see log), off".to_string(),
        (Some(Ok(_)), Some(Ok(_))) => {}
        _ => return "compiling the mask shader".to_string(),
    }
    format!(
        "{} draws masked last frame ({} had a rim_ps shader, {} rejected by rim_min_vcb, {} skipped as alpha-blended = stealth / transparent), strength {:.2}, width {:.0} px, subsurface {:.2}, {}",
        RIM_LAST_DRAWS.load(Relaxed),
        RIM_LAST_CAND.load(Relaxed),
        RIM_LAST_REJ.load(Relaxed),
        RIM_LAST_BLEND.load(Relaxed),
        crate::datapath::rim(),
        crate::datapath::rim_width(),
        crate::datapath::subsurface(),
        occ_text(RIM_OCC_LAST.load(Relaxed), &RIM_OCC_WHY)
    )
}

/// The normalised lists as written to the log / the dev text: (`fur_ps`, `fur_vs`).
pub fn selector_text() -> (String, String) {
    let s = SELECTOR.lock().unwrap_or_else(|e| e.into_inner());
    (join_hashes(&s.ps), join_hashes(&s.vs))
}

pub fn min_vcb() -> u32 {
    MIN_VCB.load(Relaxed)
}

/// `fur_debug` as configured.
pub fn debug() -> bool {
    DEBUG.load(Relaxed)
}

/// Singles out one group of draws (the picker's hover / selection); `None` clears. The mask then holds only that group, the
/// shader paints it magenta and applies no fur look. Live, no config change.
pub fn set_highlight(k: Option<crate::furpick::HighlightKey>) {
    let mut g = HL.lock().unwrap_or_else(|e| e.into_inner());
    if *g == k {
        return;
    }
    *g = k;
    match k {
        Some(k) => {
            HL_PS.store(k.ps, Relaxed);
            HL_ACTIVE.store(true, Relaxed);
        }
        None => {
            HL_ACTIVE.store(false, Relaxed);
            MATCH.store(false, Relaxed);
        }
    }
}

#[allow(dead_code)]
pub fn highlight() -> Option<crate::furpick::HighlightKey> {
    *HL.lock().unwrap_or_else(|e| e.into_inner())
}

/// A picker highlight is on.
pub fn highlighting() -> bool {
    HL_ACTIVE.load(Relaxed)
}

/// What the light-adjust shader paints magenta: the configured debug view, or the picker highlight.
pub fn shader_debug() -> bool {
    DEBUG.load(Relaxed) || HL_ACTIVE.load(Relaxed)
}

/// The mask is wanted: a selector exists, and either the fur has strength or the debug view is on.
fn wanted() -> bool {
    HL_ACTIVE.load(Relaxed) || (NONEMPTY.load(Relaxed) && (crate::datapath::fur() > 0.0 || DEBUG.load(Relaxed)))
}

/// A new frame starts (frame tick): the mask is cleared by the first masked draw, the counters roll over.
pub fn frame_begin() {
    FRESH.store(true, Relaxed);
    DRAWN.store(false, Relaxed);
    FD_FRAME.store(false, Relaxed);
    LAST_DRAWS.store(CUR_DRAWS.swap(0, Relaxed), Relaxed);
    LAST_CAND.store(CUR_CAND.swap(0, Relaxed), Relaxed);
    LAST_REJ.store(CUR_REJ.swap(0, Relaxed), Relaxed);
    LAST_UV.store(CUR_UV.swap(0, Relaxed), Relaxed);
    RIM_FRESH.store(true, Relaxed);
    RIM_DRAWN.store(false, Relaxed);
    RIM_LAST_DRAWS.store(RIM_CUR_DRAWS.swap(0, Relaxed), Relaxed);
    RIM_LAST_CAND.store(RIM_CUR_CAND.swap(0, Relaxed), Relaxed);
    RIM_LAST_REJ.store(RIM_CUR_REJ.swap(0, Relaxed), Relaxed);
    RIM_LAST_BLEND.store(RIM_CUR_BLEND.swap(0, Relaxed), Relaxed);
    OCC_DONE.store(false, Relaxed);
    RIM_OCC_DONE.store(false, Relaxed);
    OCC_LAST.store(OCC_CUR.swap(OCC_NONE, Relaxed), Relaxed);
    RIM_OCC_LAST.store(RIM_OCC_CUR.swap(OCC_NONE, Relaxed), Relaxed);
    // The per-draw probe (furprobe.rs): a frame that starts with a probe pending collects its fur draws' textures and tags the mask.
    crate::furprobe::frame_begin(PROBE_WANT.load(Relaxed));
}

/// One line for the overlay / the dev text: what the selector did last frame.
pub fn status_text() -> String {
    if HL_ACTIVE.load(Relaxed) && !FAILED.load(Relaxed) && BLOB.get().is_some() {
        return format!(
            "picker highlight: {} draws of that group painted last frame ({} had its pixel shader), {}, {}",
            LAST_DRAWS.load(Relaxed),
            LAST_CAND.load(Relaxed),
            uv_text(LAST_DRAWS.load(Relaxed), LAST_UV.load(Relaxed)),
            occ_text(OCC_LAST.load(Relaxed), &OCC_WHY)
        );
    }
    if !NONEMPTY.load(Relaxed) {
        return "off (fur_ps is empty)".to_string();
    }
    if !wanted() {
        return "off (fur_strength 0 and fur_debug 0)".to_string();
    }
    if FAILED.load(Relaxed) {
        return "failed (see log), off".to_string();
    }
    if BLOB.get().is_none() {
        return "compiling the mask shader".to_string();
    }
    format!(
        "{} draws masked last frame ({} had a fur_ps shader, {} of those rejected by fur_vs / fur_min_vcb), {}, {}",
        LAST_DRAWS.load(Relaxed),
        LAST_CAND.load(Relaxed),
        LAST_REJ.load(Relaxed),
        uv_text(LAST_DRAWS.load(Relaxed), LAST_UV.load(Relaxed)),
        occ_text(OCC_LAST.load(Relaxed), &OCC_WHY)
    )
}

/// `uv: ok (TEXCOORD0.xy)` when every masked draw carried the mesh uv, `uv: partial a/b (why)`, `uv: none (why)`.
fn uv_text(draws: u32, with_uv: u32) -> String {
    let why = UV_WHY.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if draws == 0 {
        "uv: n/a (nothing masked)".to_string()
    } else if with_uv >= draws {
        format!("uv: ok ({})", UV_DESC.lock().unwrap_or_else(|e| e.into_inner()))
    } else if with_uv == 0 {
        format!("uv: none ({why})")
    } else {
        format!("uv: partial {with_uv}/{draws} ({why})")
    }
}

// ---- the mesh uv: the output signature of the vertex shader and the mask pixel shader that matches it ----------

/// One element of a DXBC input / output signature.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SigElem {
    pub name: String,
    pub index: u32,
    /// 0 = a user semantic; 1 = SV_Position, 2 = SV_ClipDistance, ...
    pub sys: u32,
    /// 1 uint, 2 int, 3 float.
    pub ctype: u32,
    pub reg: u32,
    /// Bit 0 = x .. bit 3 = w.
    pub mask: u8,
}

fn rd32(b: &[u8], o: usize) -> Option<u32> {
    b.get(o..o.checked_add(4)?).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// The elements of the signature chunk `fourcc` (`OSGN`, `OSG5`, `OSG1`, `ISGN`, `ISG1`) of a DXBC container; `None` if the chunk is
/// missing or the data is malformed (all reads are bounds-checked).
pub fn parse_signature(code: &[u8], fourcc: &[u8; 4]) -> Option<Vec<SigElem>> {
    if code.get(0..4)? != b"DXBC" {
        return None;
    }
    let n = rd32(code, 28)? as usize;
    if n > 64 {
        return None;
    }
    for i in 0..n {
        let off = rd32(code, 32 + 4 * i)? as usize;
        if code.get(off..off.checked_add(4)?)? != fourcc {
            continue;
        }
        let size = rd32(code, off + 4)? as usize;
        let data = code.get(off + 8..off.checked_add(8)?.checked_add(size)?)?;
        let count = rd32(data, 0)? as usize;
        if count > 64 {
            return None;
        }
        // OSG5 / OSG1 start every element with the stream number; ISG1 / OSG1 end it with the minimum precision.
        let (stride, stream) = match fourcc {
            b"OSG5" => (28, true),
            b"OSG1" => (32, true),
            b"ISG1" => (28, false),
            _ => (24, false),
        };
        let mut out = Vec::with_capacity(count);
        for e in 0..count {
            let o = 8 + e * stride + if stream { 4 } else { 0 };
            let name_off = rd32(data, o)? as usize;
            let nb = data.get(name_off..)?;
            let end = nb.iter().position(|&c| c == 0)?;
            out.push(SigElem {
                name: String::from_utf8_lossy(&nb[..end.min(64)]).into_owned(),
                index: rd32(data, o + 4)?,
                sys: rd32(data, o + 8)?,
                ctype: rd32(data, o + 12)?,
                reg: rd32(data, o + 16)?,
                mask: *data.get(o + 20)?,
            });
        }
        return Some(out);
    }
    None
}

/// Output signature of a vertex shader (any of the chunk flavours).
pub fn parse_output_signature(code: &[u8]) -> Option<Vec<SigElem>> {
    parse_signature(code, b"OSGN").or_else(|| parse_signature(code, b"OSG5")).or_else(|| parse_signature(code, b"OSG1"))
}

/// Input signature of a pixel shader.
pub fn parse_input_signature(code: &[u8]) -> Option<Vec<SigElem>> {
    parse_signature(code, b"ISGN").or_else(|| parse_signature(code, b"ISG1"))
}

/// Which vertex-shader output carries the uv (`fur_uv` in config.ini).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UvPref {
    /// The lowest TEXCOORD with two float components (.xy).
    Auto,
    Sem { name: String, index: u32, zw: bool },
}

impl UvPref {
    /// `auto` / empty / garbage = Auto; `TEXCOORD1`, `texcoord0.zw`, `TEXCOORD2.xy`.
    pub fn parse(text: &str) -> UvPref {
        let t = text.trim().to_ascii_lowercase();
        if t.is_empty() || t == "auto" {
            return UvPref::Auto;
        }
        let (head, sw) = t.split_once('.').unwrap_or((t.as_str(), "xy"));
        let zw = match sw {
            "xy" => false,
            "zw" => true,
            _ => return UvPref::Auto,
        };
        let digits = head.bytes().rev().take_while(|b| b.is_ascii_digit()).count();
        let (name, idx) = head.split_at(head.len() - digits);
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphabetic() || b == b'_') {
            return UvPref::Auto;
        }
        UvPref::Sem { name: name.to_ascii_uppercase(), index: idx.parse().unwrap_or(0), zw }
    }

    pub fn text(&self) -> String {
        match self {
            UvPref::Auto => "auto".to_string(),
            UvPref::Sem { name, index, zw } => format!("{name}{index}.{}", if *zw { "zw" } else { "xy" }),
        }
    }
}

/// The chosen uv element of a vertex shader's output signature.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UvPick {
    pub elem: usize,
    pub zw: bool,
}

/// Output signature of one vertex shader (the system values included).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VsSig {
    pub elems: Vec<SigElem>,
}

fn comps(mask: u8) -> u32 {
    8 - (mask & 15).leading_zeros().min(8) // highest set bit + 1: the PS declares a vector of that width
}

impl VsSig {
    pub fn pick(&self, pref: &UvPref) -> Result<UvPick, String> {
        let float = |e: &SigElem| e.sys == 0 && e.ctype == 3;
        match pref {
            UvPref::Auto => self
                .elems
                .iter()
                .enumerate()
                .filter(|(_, e)| float(e) && e.name.eq_ignore_ascii_case("TEXCOORD") && e.mask & 3 == 3)
                .min_by_key(|(_, e)| e.index)
                .map(|(elem, _)| UvPick { elem, zw: false })
                .ok_or_else(|| "no TEXCOORD output with two float components".to_string()),
            UvPref::Sem { name, index, zw } => {
                let (elem, e) = self
                    .elems
                    .iter()
                    .enumerate()
                    .find(|(_, e)| e.name.eq_ignore_ascii_case(name) && e.index == *index)
                    .ok_or_else(|| format!("no {name}{index} output"))?;
                let need = if *zw { 0xC } else { 0x3 };
                if !float(e) || e.mask & need != need {
                    return Err(format!("{name}{index} has no float {} components (mask {:x})", if *zw { "zw" } else { "xy" }, e.mask));
                }
                Ok(UvPick { elem, zw: *zw })
            }
        }
    }

    /// Human readable, for the log: `SV_Position@o0:f TEXCOORD0@o1:3`.
    pub fn text(&self) -> String {
        self.elems.iter().map(|e| format!("{}{}@o{}:{:x}{}", e.name, e.index, e.reg, e.mask, ["", "u", "i", "f"][e.ctype.min(3) as usize])).collect::<Vec<_>>().join(" ")
    }

    /// The key of the pixel shader generated for this signature and pick (equal keys = interchangeable shaders).
    pub fn key(&self, pick: UvPick) -> String {
        format!("{}|{}{}", self.text(), pick.elem, if pick.zw { "zw" } else { "xy" })
    }
}

/// The input struct body (declarations), the "read every input" sum and the SV_Position element of a pixel shader that links to
/// the outputs of `sig` (`pick` = the uv element, whose zw pick needs a full-width declaration).
pub(crate) fn ps_inputs(sig: &VsSig, pick: Option<UvPick>) -> (String, String, Option<usize>) {
    // SV_Position takes a register of the pixel shader's input signature too (it is a user-like slot there), so it is declared
    // where the vertex shader has it: before the user outputs when its register is lower (o0), after them when it is last (o4).
    let mut order: Vec<usize> = (0..sig.elems.len()).filter(|&i| sig.elems[i].sys <= 1).collect();
    order.sort_by_key(|&i| (sig.elems[i].reg, sig.elems[i].mask.trailing_zeros()));
    let mut decl = String::new();
    let mut sum = String::new();
    let mut pos = None;
    for &i in &order {
        let e = &sig.elems[i];
        if e.sys == 1 {
            decl.push_str(&format!("    float4 e{i} : SV_Position;\n"));
            sum.push_str(&format!("    k += i.e{i}.x;\n"));
            pos = Some(i);
            continue;
        }
        // Exact width: the compiler packs the inputs first-fit, so float2 + float1 + float1 land in x y / z / w of one register
        // like the vertex shader's outputs (a wider declaration would push the next input to another register).
        let (tz, pc) = (e.mask.trailing_zeros(), (e.mask & 15).count_ones());
        let exact = pc > 0 && (e.mask & 15) >> tz == (1u8 << pc) - 1 && !pick.is_some_and(|p| p.zw && i == p.elem);
        let n = if exact { pc } else { comps(e.mask) }.max(1);
        let (base, flat) = match e.ctype {
            1 => ("uint", "nointerpolation "),
            2 => ("int", "nointerpolation "),
            _ => ("float", ""),
        };
        let ty = if n == 1 { base.to_string() } else { format!("{base}{n}") };
        decl.push_str(&format!("    {flat}{ty} e{i} : {}{};\n", e.name, e.index));
        if n == 1 {
            sum.push_str(&format!("    k += (float)i.e{i};\n"));
        } else {
            for c in 0..n as usize {
                sum.push_str(&format!("    k += (float)i.e{i}.{};\n", ["x", "y", "z", "w"][c]));
            }
        }
    }
    (decl, sum, pos)
}

/// The generated shader's body around `value` (the float4 written to the mask, may use `one`): the input struct, the two
/// outputs (SV_Target0 = the mask, SV_Target1 = the z-stamp = SV_Position.z, the device depth the depth test used; -1 when the
/// vertex shader has no SV_Position output, which the occlusion check reads as "nothing stamped").
fn gen_ps(decl: &str, sum: &str, pos: Option<usize>, pre: &str, value: &str) -> String {
    let z = pos.map_or("-1.0".to_string(), |p| format!("i.e{p}.z"));
    format!(
        "struct VIn {{\n{decl}}};\n\nstruct POut {{\n    float4 m : SV_Target0;\n    float z : SV_Target1;\n}};\n\nPOut {PS_UV_ENTRY}(VIn i)\n{{\n    float k = 0.0;\n{sum}    float one = 1.0 - 1e-6 * saturate(abs(k) * 1e-30);\n{pre}    POut o;\n    o.m = {value};\n    o.z = {z};\n    return o;\n}}\n"
    )
}

/// HLSL of a pixel shader whose inputs replicate the user semantics of `sig` (same order by register, same widths), so the
/// compiler gives them the vertex shader's registers, and that writes the mask target: rg = frac(uv), b = 1, a = 1, and the
/// z-stamp. Every input is read (by a term that cannot change the result) so none is dropped from the signature.
pub fn uv_ps_hlsl(sig: &VsSig, pick: UvPick) -> String {
    let (decl, sum, pos) = ps_inputs(sig, Some(pick));
    let e = &sig.elems[pick.elem];
    let sw = if pick.zw { "zw" } else { "xy" };
    let uv = if comps(e.mask) == 1 { format!("float2(i.e{}, 0.0)", pick.elem) } else { format!("i.e{}.{sw}", pick.elem) };
    format!("{FD_HLSL}\n{}", gen_ps(&decl, &sum, pos, &format!("    float2 uv = {uv};\n"), "float4(frac(uv), fd_b(), one * fd_weight(uv))"))
}

/// The fur discriminator of the uv mask shader (v3, docs/shell-fur.md): `a` = how much the texel at the mesh uv is FUR, from a channel
/// of one of the engine's own textures still bound at the re-issued draw (`t0..t3`, `shell_fur_mask`) and the excluded uv rectangles
/// (`shell_uv_exclude`), with soft edges. The constants are `b13` (`shellfur::fd_cb_values`), bound for this draw only; unbound or
/// all zero = a = 1 (the mask as before). The texture is read with Load at a FIXED mip (about 512 texels across) and filtered bilinearly
/// by hand (wrapping): no sampler, no derivative, so the value at a uv never depends on the camera or the frame (no flicker).
pub const FD_HLSL: &str = r#"Texture2D<float4> fd_t0 : register(t0);
Texture2D<float4> fd_t1 : register(t1);
Texture2D<float4> fd_t2 : register(t2);
Texture2D<float4> fd_t3 : register(t3);
cbuffer fur_discrim : register(b13)
{
    float4 fd0; // x texture test on, y slot 0..3, z channel (0 r, 1 g, 2 b, 3 a, 4 luminance, 5 detail, 6 saturation, 7 flatness), w 1 = fur ABOVE
    float4 fd1; // x threshold, y soft half width, z excluded rectangles, w their feather (uv)
    float4 fd_rect[16];
    float4 fd_tag; // x = the value written to b for this draw (0.5 + k / 64, k = the draw's index in the frame + 1; 0 = 1.0 as before)
    float4 fd_more[3]; // v5: tests 2..4, like fd0 (x = 0: unused)
    float4 fd_more_th; // their thresholds (x y z)
};

float fd_pick(float4 c, uint ch)
{
    if (ch == 6u)
    {
        float mx = max(max(c.r, c.g), c.b);
        return (mx - min(min(c.r, c.g), c.b)) / max(mx, 1e-3);
    }
    return ch == 0u ? c.r : ch == 1u ? c.g : ch == 2u ? c.b : ch == 3u ? c.a : dot(c.rgb, float3(0.2126, 0.7152, 0.0722));
}

// The mip of a w x h view (n levels) with at most `lim` texels across.
uint fd_level(uint w, uint h, uint n, uint lim)
{
    uint lv = 0u;
    [loop] for (uint k = 0u; k < 15u; k++)
    {
        if (lv + 1u < n && (max(w, h) >> lv) > lim)
            lv++;
    }
    return lv;
}

// Detail (channel 5): the rms deviation of .rg (x2: a normal map's xy) over the 3x3 texels around the NEAREST texel at the mip with at
// most 1024 texels across, x2, saturated. Noisy fur strands are high, a smooth horn / claw / nose low. Mirror: furprobe::detail_at.
float fd_detail(Texture2D<float4> t, float2 uv, uint w, uint h, uint n)
{
    uint lv = fd_level(w, h, n, 1024u);
    int2 m = int2(max(w >> lv, 1u), max(h >> lv, 1u));
    int2 c = int2(floor(frac(uv) * float2(m)));
    float2 s = float2(0.0, 0.0);
    float s2 = 0.0;
    [unroll] for (int y = -1; y <= 1; y++)
    {
        [unroll] for (int x = -1; x <= 1; x++)
        {
            int2 p = ((c + int2(x, y)) % m + m) % m;
            float2 v = t.Load(int3(p, lv)).rg * 2.0;
            s += v;
            s2 += dot(v, v);
        }
    }
    float2 mean = s / 9.0;
    float var = max(s2 / 9.0 - dot(mean, mean), 0.0);
    return saturate(2.0 * sqrt(var));
}

// Flatness (channel 7, v5): the rms deviation of .rgb over the 5x5 texels around the NEAREST texel at the mip with at most 1024 texels
// across, x2, divided by the rms of the window's mean colour (a paint that multiplies the colour gives the same value on any dye); 1
// where the brightest mean channel is above 0.97 (clipped white: no
// detail to measure, counted as fur). A flat dye-coloured horn is ~0, painted fur 0.03 and more. Mirror: furprobe::flat_at.
float fd_flat(Texture2D<float4> t, float2 uv, uint w, uint h, uint n)
{
    uint lv = fd_level(w, h, n, 1024u);
    int2 m = int2(max(w >> lv, 1u), max(h >> lv, 1u));
    int2 c = int2(floor(frac(uv) * float2(m)));
    float3 s = float3(0.0, 0.0, 0.0);
    float s2 = 0.0;
    [unroll] for (int y = -2; y <= 2; y++)
    {
        [unroll] for (int x = -2; x <= 2; x++)
        {
            int2 p = ((c + int2(x, y)) % m + m) % m;
            float3 v = t.Load(int3(p, lv)).rgb;
            s += v;
            s2 += dot(v, v);
        }
    }
    float3 mean = s / 25.0;
    float mx = max(max(mean.r, mean.g), mean.b);
    if (mx > 0.97)
        return 1.0;
    float var = max(s2 / 25.0 - dot(mean, mean), 0.0) / 3.0;
    return saturate(2.0 * sqrt(var) / max(sqrt(dot(mean, mean) / 3.0), 0.05));
}

float fd_b()
{
    return (fd_tag.y == 1234.5 && fd_tag.x > 0.5) ? fd_tag.x : 1.0; // y = a marker: a foreign b13 never tags
}

// The channel at uv (bilinear, wrapping, at the mip with at most 512 texels across); -1 = nothing bound.
float fd_value(Texture2D<float4> t, float2 uv, uint ch)
{
    uint w, h, n;
    t.GetDimensions(0, w, h, n);
    if (w == 0u || h == 0u || n == 0u)
        return -1.0;
    if (ch == 5u)
        return fd_detail(t, uv, w, h, n);
    if (ch == 7u)
        return fd_flat(t, uv, w, h, n);
    uint lv = 0u;
    [loop] for (uint k = 0u; k < 15u; k++)
    {
        if (lv + 1u < n && (max(w, h) >> lv) > 512u)
            lv++;
    }
    int2 m = int2(max(w >> lv, 1u), max(h >> lv, 1u));
    float2 q = frac(uv) * float2(m) - 0.5;
    int2 b = int2(floor(q));
    float2 f = q - float2(b);
    int2 p0 = ((b % m) + m) % m;
    int2 p1 = ((b + 1) % m + m) % m;
    float4 c00 = t.Load(int3(p0.x, p0.y, lv)), c10 = t.Load(int3(p1.x, p0.y, lv));
    float4 c01 = t.Load(int3(p0.x, p1.y, lv)), c11 = t.Load(int3(p1.x, p1.y, lv));
    float v = fd_pick(lerp(lerp(c00, c10, f.x), lerp(c01, c11, f.x), f.y), ch);
    return v == v ? saturate(v) : -1.0; // NaN = no answer
}

// The soft half width of a test at threshold th: shell_fur_soft, at most half the distance to 0 / 1. Mirror: shellfur::test_soft.
float fd_soft(float th)
{
    return max(min(clamp(fd1.y, 0.01, 0.5), 0.5 * min(th, 1.0 - th)), 0.002);
}

float fd_weight(float2 uv)
{
    float wgt = 1.0;
    if (fd0.x > 0.5)
    {
        uint slot = (uint)clamp(fd0.y, 0.0, 3.0);
        uint ch = (uint)clamp(fd0.z, 0.0, 7.0);
        float v = slot == 0u ? fd_value(fd_t0, uv, ch) : slot == 1u ? fd_value(fd_t1, uv, ch) : slot == 2u ? fd_value(fd_t2, uv, ch) : fd_value(fd_t3, uv, ch);
        if (v >= 0.0)
        {
            float s = fd_soft(fd1.x);
            float above = smoothstep(fd1.x - s, fd1.x + s, v);
            wgt = fd0.w > 0.5 ? above : 1.0 - above;
        }
    }
    [unroll] for (int k = 0; k < 3; k++)
    {
        float4 sel = fd_more[k];
        if (sel.x > 0.5)
        {
            float th = k == 0 ? fd_more_th.x : k == 1 ? fd_more_th.y : fd_more_th.z;
            uint slot = (uint)clamp(sel.y, 0.0, 3.0);
            uint ch = (uint)clamp(sel.z, 0.0, 7.0);
            float v = slot == 0u ? fd_value(fd_t0, uv, ch) : slot == 1u ? fd_value(fd_t1, uv, ch) : slot == 2u ? fd_value(fd_t2, uv, ch) : fd_value(fd_t3, uv, ch);
            if (v >= 0.0)
            {
                float s = fd_soft(th);
                float above = smoothstep(th - s, th + s, v);
                wgt *= sel.w > 0.5 ? above : 1.0 - above;
            }
        }
    }
    int nr = (int)clamp(fd1.z, 0.0, 16.0);
    float fe = max(fd1.w, 1e-5);
    float2 u = frac(uv);
    [loop] for (int k2 = 0; k2 < nr; k2++)
    {
        float4 r = fd_rect[k2];
        float d = min(min(u.x - r.x, r.z - u.x), min(u.y - r.y, r.w - u.y));
        wgt *= 1.0 - smoothstep(-fe, fe, d);
    }
    return saturate(wgt);
}
"#;

/// HLSL of the pixel shader for a draw without uv, linked to `sig` like `uv_ps_hlsl` so it can read SV_Position: `rim` = the rim
/// mask (1 in every channel), else the fur mask without uv (a = 1, b = 0). Writes the z-stamp.
pub fn z_ps_hlsl(sig: &VsSig, rim: bool) -> String {
    let (decl, sum, pos) = ps_inputs(sig, None);
    gen_ps(&decl, &sum, pos, "", if rim { "float4(one, one, one, one)" } else { "float4(0.0, 0.0, 0.0, one)" })
}

/// Generates, compiles and verifies `z_ps_hlsl`. Slow (the compiler): background threads and tests only.
pub fn build_z_ps(sig: &VsSig, rim: bool) -> Result<Vec<u8>, String> {
    let blob = shaders::compile(&z_ps_hlsl(sig, rim), PS_UV_ENTRY, "ps_5_0")?;
    verify_link(sig, &blob)?;
    Ok(blob)
}

pub const PS_UV_ENTRY: &str = "ps_mask_uv";

/// The compiled pixel shader's user inputs must sit at the registers of the vertex shader's outputs (same name / index, same
/// register, components a subset). Returns the first mismatch.
pub fn verify_link(vs: &VsSig, ps_code: &[u8]) -> Result<(), String> {
    let isg = parse_input_signature(ps_code).ok_or_else(|| "no input signature in the compiled pixel shader".to_string())?;
    for p in isg.iter().filter(|e| e.sys == 0) {
        let Some(v) = vs.elems.iter().find(|v| v.sys == 0 && v.name.eq_ignore_ascii_case(&p.name) && v.index == p.index) else {
            return Err(format!("pixel input {}{} is not a vertex output", p.name, p.index));
        };
        if v.reg != p.reg || p.mask & !v.mask != 0 {
            return Err(format!("{}{}: vertex output o{}:{:x}, pixel input v{}:{:x}", p.name, p.index, v.reg, v.mask, p.reg, p.mask));
        }
    }
    Ok(())
}

/// Generates, compiles and verifies the uv mask pixel shader of `sig`. Slow (the compiler): background threads and tests only.
pub fn build_uv_ps(sig: &VsSig, pick: UvPick) -> Result<Vec<u8>, String> {
    let hlsl = uv_ps_hlsl(sig, pick);
    let blob = shaders::compile(&hlsl, PS_UV_ENTRY, "ps_5_0")?;
    verify_link(sig, &blob)?;
    Ok(blob)
}

// ---- registries: signatures of the vertex shaders, pixel shaders built from them ---------------------------------

static VS_SIGS: Mutex<Option<HashMap<usize, Arc<VsSig>>>> = Mutex::new(None);
static UV_PREF: Mutex<Option<UvPref>> = Mutex::new(None);

enum UvPsState {
    Pending,
    Ready(Arc<Vec<u8>>),
    Failed(String),
}
static UV_PS: Mutex<Option<HashMap<String, UvPsState>>> = Mutex::new(None);
/// Why the last draw had no uv (for the status line) and which element the last built shader reads.
static UV_WHY: Mutex<String> = Mutex::new(String::new());
static UV_DESC: Mutex<String> = Mutex::new(String::new());
static UV_LOGGED: Mutex<Vec<usize>> = Mutex::new(Vec::new());

/// The bytecode of the vertex shaders whose outputs look like the game's character layouts (a TEXCOORD with xy, at least two float3+
/// TEXCOORDs and SV_Position): the shell fur's dev dump disassembles the ones that get shells (`shellfur::dump_vs`). At most 256.
static VS_CODE: Mutex<Option<HashMap<usize, Arc<Vec<u8>>>>> = Mutex::new(None);

fn character_like(elems: &[SigElem]) -> bool {
    let tex3 = elems.iter().filter(|e| e.sys == 0 && e.name.eq_ignore_ascii_case("TEXCOORD") && comps(e.mask) >= 3).count();
    let uv = elems.iter().any(|e| e.sys == 0 && e.name.eq_ignore_ascii_case("TEXCOORD") && e.mask & 3 == 3);
    tex3 >= 2 && uv && elems.iter().any(|e| e.sys == 1)
}

/// The bytecode of the vertex shader at `ptr` (only for character-like output layouts).
pub fn vs_code(ptr: usize) -> Option<Arc<Vec<u8>>> {
    VS_CODE.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|m| m.get(&ptr).cloned())
}

/// Remembers the output signature of a vertex shader just created (`CreateVertexShader` hook). Never unwinds.
pub fn note_vs(ptr: usize, code: &[u8]) {
    let sig = parse_output_signature(code);
    {
        let mut g = VS_CODE.lock().unwrap_or_else(|e| e.into_inner());
        let m = g.get_or_insert_with(HashMap::new);
        match &sig {
            Some(e) if character_like(e) && (m.len() < 256 || m.contains_key(&ptr)) => {
                m.insert(ptr, Arc::new(code.to_vec()));
            }
            _ => {
                m.remove(&ptr);
            }
        }
    }
    let mut g = VS_SIGS.lock().unwrap_or_else(|e| e.into_inner());
    let m = g.get_or_insert_with(HashMap::new);
    match sig {
        Some(elems) if m.len() < 8192 || m.contains_key(&ptr) => {
            m.insert(ptr, Arc::new(VsSig { elems }));
        }
        _ => {
            m.remove(&ptr);
        }
    }
}

/// The output signature of the vertex shader at `ptr` (recorded by the `CreateVertexShader` hook), if known.
pub fn vs_sig(ptr: usize) -> Option<Arc<VsSig>> {
    VS_SIGS.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|m| m.get(&ptr).cloned())
}

/// The uv element `fur_uv` picks in `sig` (the shells read the same uv as the fur mask).
pub fn uv_pick(sig: &VsSig) -> Result<UvPick, String> {
    let pref = UV_PREF.lock().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or(UvPref::Auto);
    sig.pick(&pref)
}

/// `fur_uv` (live).
pub fn set_uv_pref(text: &str) {
    *UV_PREF.lock().unwrap_or_else(|e| e.into_inner()) = Some(UvPref::parse(text));
}

pub fn uv_pref_text() -> String {
    UV_PREF.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map_or_else(|| "auto".to_string(), UvPref::text)
}

fn why(reason: &str) {
    let mut w = UV_WHY.lock().unwrap_or_else(|e| e.into_inner());
    if *w != reason {
        *w = reason.to_string();
    }
}

/// The uv pixel shader for the vertex shader at `vs_ptr`: `Ok((key, blob))` when it is built, `Err(reason)` while it is
/// compiling (a background thread does it) or when this shader has no usable uv. Never compiles on the caller's thread.
fn uv_shader_for(vs_ptr: usize) -> Result<(String, Arc<Vec<u8>>), String> {
    let sig = VS_SIGS.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|m| m.get(&vs_ptr).cloned());
    let Some(sig) = sig else { return Err("vertex shader signature unknown".to_string()) };
    let pref = UV_PREF.lock().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or(UvPref::Auto);
    let pick = match sig.pick(&pref) {
        Ok(p) => p,
        Err(e) => {
            let mut l = UV_LOGGED.lock().unwrap_or_else(|e| e.into_inner());
            if !l.contains(&vs_ptr) && l.len() < 32 {
                l.push(vs_ptr);
                log!("furmask: no uv for this vertex shader ({e}); outputs: {}", sig.text());
            }
            return Err(e);
        }
    };
    let key = sig.key(pick);
    let desc = format!("{}{}.{}", sig.elems[pick.elem].name, sig.elems[pick.elem].index, if pick.zw { "zw" } else { "xy" });
    cached_shader(key, UV_PENDING, move || {
        let r = build_uv_ps(&sig, pick);
        match &r {
            Ok(_) => {
                log!("furmask: uv shader built, reads {desc} (vertex outputs: {})", sig.text());
                *UV_DESC.lock().unwrap_or_else(|e| e.into_inner()) = desc;
            }
            Err(e) => log!("furmask: uv shader for [{}] failed, those draws stay mask-only: {e}", sig.text()),
        }
        r
    })
}

const UV_PENDING: &str = "compiling the uv shader";

/// The shader `key` from the registry: `Ok` when built, `Err(pending)` while `build` runs on a background thread (started by
/// the first call), `Err(why)` when it failed. Never compiles on the caller's thread.
fn cached_shader(key: String, pending: &'static str, build: impl FnOnce() -> Result<Vec<u8>, String> + Send + 'static) -> Result<(String, Arc<Vec<u8>>), String> {
    let mut g = UV_PS.lock().unwrap_or_else(|e| e.into_inner());
    let m = g.get_or_insert_with(HashMap::new);
    match m.get(&key) {
        Some(UvPsState::Ready(b)) => Ok((key, b.clone())),
        Some(UvPsState::Pending) => Err(pending.to_string()),
        Some(UvPsState::Failed(e)) => Err(e.clone()),
        None => {
            if m.len() >= MAX_GEN_PS {
                return Err("too many vertex shader layouts".to_string());
            }
            m.insert(key.clone(), UvPsState::Pending);
            let k2 = key.clone();
            let spawned = std::thread::Builder::new().name("gw2g-furuv-compile".into()).spawn(move || {
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(build)).unwrap_or_else(|_| Err("panic while building".to_string()));
                let st = match r {
                    Ok(b) => UvPsState::Ready(Arc::new(b)),
                    Err(e) => UvPsState::Failed(e),
                };
                UV_PS.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).insert(k2, st);
            });
            if spawned.is_err() {
                m.insert(key, UvPsState::Failed("could not start the compile thread".to_string()));
            }
            Err(pending.to_string())
        }
    }
}

/// Generated pixel shaders kept (registry and device objects): up to three per vertex layout (uv, fur without uv, rim).
const MAX_GEN_PS: usize = 192;

/// The z-stamping pixel shader without uv for the vertex shader at `vs_ptr` (`rim` = the rim mask, else the fur mask without uv),
/// as `uv_shader_for`. `Err` = use the static fallback (`ps_mask` / `ps_rim`, which stamps "nothing": the check keeps those pixels).
fn z_shader_for(vs_ptr: usize, rim: bool) -> Result<(String, Arc<Vec<u8>>), String> {
    let sig = VS_SIGS.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|m| m.get(&vs_ptr).cloned());
    let Some(sig) = sig else { return Err("vertex shader signature unknown".to_string()) };
    let key = format!("{}|{}", sig.text(), if rim { "rim" } else { "mask" });
    cached_shader(key, "compiling the mask shader", move || {
        let r = build_z_ps(&sig, rim);
        if let Err(e) = &r {
            log!("furmask: {} shader for [{}] failed, those draws stay unchecked by the occlusion test: {e}", if rim { "rim" } else { "mask" }, sig.text());
        }
        r
    })
}

// ---- hooks ---------------------------------------------------------------------------------------------

/// `PSSetShader` on `ctx` (hook, before the original): remembers whether the pixel shader is one of `fur_ps`.
/// One atomic load when the feature is off; never unwinds.
pub fn on_ps_set(ctx: Obj, ps: Obj) {
    let hl = HL_ACTIVE.load(Relaxed);
    if !NONEMPTY.load(Relaxed) && !hl && !RIM_NONEMPTY.load(Relaxed) {
        if MATCH.load(Relaxed) {
            MATCH.store(false, Relaxed);
        }
        if RIM_MATCH.load(Relaxed) {
            RIM_MATCH.store(false, Relaxed);
        }
        return;
    }
    if ignoring() || recorder::game_context() != ctx as usize {
        return;
    }
    let _ = std::panic::catch_unwind(|| {
        let (want_fur, want_rim) = (wanted(), wanted_rim());
        let hash = if ps.is_null() || !(want_fur || want_rim) { None } else { shaders::registry().hash_of(ps as usize) };
        let m = want_fur && hash.is_some_and(|h| if hl { h == HL_PS.load(Relaxed) } else { SELECTOR.lock().unwrap_or_else(|e| e.into_inner()).has_ps(h) });
        let mr = want_rim && hash.is_some_and(|h| RIM_SELECTOR.lock().unwrap_or_else(|e| e.into_inner()).has_ps(h));
        if let (true, Some(h)) = (m, hash) {
            CUR_PS.store(h, Relaxed);
        }
        if let (true, Some(h)) = (mr, hash) {
            RIM_CUR_PS.store(h, Relaxed);
        }
        MATCH.store(m, Relaxed);
        RIM_MATCH.store(mr, Relaxed);
    });
}

/// A draw is about to be issued on `ctx` (hook, before the engine's own call): when it is a fur draw, issue it once more
/// into the mask through `issue` (the original draw call with the same arguments). One atomic load for every other draw;
/// never unwinds; the engine's draw is NOT issued here.
pub fn before_draw(ctx: Obj, issue: &dyn Fn(), call: crate::shellfur::Call) {
    if !MATCH.load(Relaxed) && !RIM_MATCH.load(Relaxed) {
        return;
    }
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe { before_draw_inner(ctx, issue, call) }));
}

unsafe fn before_draw_inner(ctx: Obj, issue: &dyn Fn(), call: crate::shellfur::Call) {
    let fur = MATCH.load(Relaxed) && wanted();
    let rim = RIM_MATCH.load(Relaxed) && wanted_rim();
    if ignoring() || recorder::game_context() != ctx as usize || !(shaders::substitution_enabled() && gate::user_enabled()) || !(fur || rim) {
        return;
    }
    // Opaque-pass shape: a full-size colour target with a full-size depth buffer. Shadow maps and the like are skipped.
    let Some(site) = recorder::current_site(ctx as usize) else { return };
    let screen = d3d::hooks().screen();
    let full = |t: &crate::passes::Target| t.w == screen.0 && t.h == screen.1;
    if !site.rt0.as_ref().is_some_and(full) || !site.depth.as_ref().is_some_and(full) {
        return;
    }
    if FAILED.load(Relaxed) {
        return;
    }
    // Which draw is this: vertex shader, vertex constant buffer 0 (the pixel shader is known to be in a list).
    let (vs_hash, vcb0, vs_ptr) = unsafe { read_draw(ctx) };
    if fur {
        if let Some(Ok(blob)) = BLOB.get() {
            unsafe { fur_draw(ctx, issue, blob, vs_hash, vcb0, vs_ptr, call) };
        }
    }
    if rim {
        if let (Some(Ok(blob)), Some(Ok(rim_blob))) = (BLOB.get(), BLOB_RIM.get()) {
            unsafe { rim_draw(ctx, issue, blob, rim_blob, vs_hash, vcb0, vs_ptr) };
        }
    }
}

/// The rim channel of a draw about to be issued: when the draw is in `rim_ps` (and big enough, not alpha-blended), it is issued once
/// more into the R8 rim mask.
unsafe fn rim_draw(ctx: Obj, issue: &dyn Fn(), blob: &[u8], rim_blob: &[u8], vs_hash: Option<u32>, vcb0: u32, vs_ptr: usize) {
    let ps_hash = RIM_CUR_PS.load(Relaxed);
    RIM_CUR_CAND.fetch_add(1, Relaxed);
    if !RIM_SELECTOR.lock().unwrap_or_else(|e| e.into_inner()).matches(ps_hash, vs_hash, vcb0) {
        RIM_CUR_REJ.fetch_add(1, Relaxed);
        return;
    }
    // The rim shader linked to this vertex shader (it stamps the fragment depth); until it is built, the static `ps_rim`.
    let rim_ps = z_shader_for(vs_ptr, true).ok();
    let mut dev: Obj = std::ptr::null_mut();
    unsafe {
        let get_device: unsafe extern "system" fn(Obj, *mut Obj) = std::mem::transmute(vfn(ctx, vt::CHILD_GET_DEVICE));
        get_device(ctx, &mut dev);
    }
    if dev.is_null() {
        return;
    }
    let result = 'run: {
        let Ok(mut g) = STATE.try_lock() else { break 'run Ok(false) };
        if g.as_ref().is_some_and(|m| m.device != dev as usize) {
            *g = None;
        }
        if g.is_none() {
            *g = unsafe { Mask::new(dev, blob) };
            if g.is_none() {
                FAILED.store(true, Relaxed);
                log!("furmask: could not create the mask objects; the masks stay off");
                break 'run Ok(false);
            }
        }
        let Some(m) = g.as_mut() else { break 'run Ok(false) };
        if !unsafe { m.ensure_rim_ps(dev, rim_blob) } {
            break 'run Err("could not create the rim mask shader");
        }
        let clear = RIM_FRESH.swap(false, Relaxed);
        let gen_arg = rim_ps.as_ref().map(|(k, b)| (k.as_str(), b.as_slice()));
        match unsafe { m.draw_into(dev, ctx, true, clear, issue, gen_arg) } {
            Ok(_) => Ok(true),
            Err(why) => {
                if clear {
                    RIM_FRESH.store(true, Relaxed);
                }
                Err(why)
            }
        }
    };
    unsafe { release(dev) };
    match result {
        Ok(false) => {}
        Ok(true) => {
            RIM_DRAWN.store(true, Relaxed);
            RIM_CUR_DRAWS.fetch_add(1, Relaxed);
            if !RIM_FIRST_LOGGED.swap(true, Relaxed) {
                log!("furmask: first rim-masked draw (ps {}, vs {}, vertex cb0 {} bytes)", format!("{ps_hash:08x}"), hash_text(vs_hash), vcb0);
            }
        }
        Err("alpha-blended draw") => {
            RIM_CUR_BLEND.fetch_add(1, Relaxed);
        }
        Err(why) => {
            static WHY_LOGGED: AtomicBool = AtomicBool::new(false);
            if !WHY_LOGGED.swap(true, Relaxed) {
                log!("furmask: a draw could not be rim-masked: {why}");
            }
        }
    }
}

/// The fur channel of a draw about to be issued (the original body of `before_draw_inner`).
unsafe fn fur_draw(ctx: Obj, issue: &dyn Fn(), blob: &[u8], vs_hash: Option<u32>, vcb0: u32, vs_ptr: usize, call: crate::shellfur::Call) {
    let ps_hash = CUR_PS.load(Relaxed);
    CUR_CAND.fetch_add(1, Relaxed);
    let ok = if HL_ACTIVE.load(Relaxed) {
        // The picker's group: shaders and (when it has one) the size of the first bound texture, no vcb / fur_vs rule.
        let key = *HL.lock().unwrap_or_else(|e| e.into_inner());
        key.is_some_and(|k| {
            let tex = if k.needs_tex() { crate::furpick::tex0_of(&unsafe { crate::drawdiag::read_bound(ctx) }) } else { None };
            k.matches(ps_hash, vs_hash, tex)
        })
    } else {
        SELECTOR.lock().unwrap_or_else(|e| e.into_inner()).matches(ps_hash, vs_hash, vcb0)
    };
    if !ok {
        CUR_REJ.fetch_add(1, Relaxed);
        return;
    }
    let uv = uv_shader_for(vs_ptr);
    // Without uv the draw still gets a shader linked to its vertex shader, so the z-stamp is written (`Err` = the static `ps_mask`).
    let mut has_uv = true;
    let uv = match uv {
        Ok(x) => Ok(x),
        Err(e) => {
            why(&e);
            has_uv = false;
            if e == UV_PENDING { Err(e) } else { z_shader_for(vs_ptr, false) }
        }
    };
    let mut dev: Obj = std::ptr::null_mut();
    unsafe {
        let get_device: unsafe extern "system" fn(Obj, *mut Obj) = std::mem::transmute(vfn(ctx, vt::CHILD_GET_DEVICE));
        get_device(ctx, &mut dev);
    }
    if dev.is_null() {
        return;
    }
    let probing = !HL_ACTIVE.load(Relaxed) && crate::furprobe::tagging();
    let probe_idx = if probing { crate::furprobe::next_index() } else { 0 };
    let probe_tex = if probing && crate::furprobe::collecting() { unsafe { crate::furprobe::textures(ctx) } } else { Default::default() };
    let result = 'run: {
        let Ok(mut g) = STATE.try_lock() else { break 'run Ok(None) }; // never wait: this draw simply is not masked
        if g.as_ref().is_some_and(|m| m.device != dev as usize) {
            *g = None;
        }
        if g.is_none() {
            *g = unsafe { Mask::new(dev, blob) };
            if g.is_none() {
                FAILED.store(true, Relaxed);
                log!("furmask: could not create the mask objects; the fur mask stays off");
                break 'run Ok(None);
            }
        }
        let clear = FRESH.swap(false, Relaxed);
        let uv_arg = uv.as_ref().ok().map(|(k, b)| (k.as_str(), b.as_slice()));
        // v3 fur discriminator (shell_fur_mask / shell_uv_exclude): this draw's constants; the picker's highlight shows the whole draw.
        let d = crate::shellfur::discrim();
        let fd = if d.active() && !HL_ACTIVE.load(Relaxed) {
            // v5: the engine slots t0..t3 bound for this draw (a test on an unbound slot is switched off).
            let bound = if d.slots() == 0 {
                0
            } else {
                unsafe {
                    let mut v: [Obj; crate::shellfur::FUR_MASK_SLOTS as usize] = [std::ptr::null_mut(); crate::shellfur::FUR_MASK_SLOTS as usize];
                    let get_srv: unsafe extern "system" fn(Obj, u32, u32, *mut Obj) = std::mem::transmute(vfn(ctx, vt::CTX_PS_GET_SRVS));
                    get_srv(ctx, 0, crate::shellfur::FUR_MASK_SLOTS, v.as_mut_ptr());
                    let mut m = 0u32;
                    for (k, p) in v.into_iter().enumerate() {
                        if !p.is_null() {
                            m |= 1 << k;
                            release(p);
                        }
                    }
                    m
                }
            };
            crate::shellfur::fd_for_draw_slots(&d, call.count(), bound)
        } else {
            None
        };
        if fd.is_some() {
            FD_FRAME.store(true, Relaxed);
        }
        // Per-draw tag in the mask's b (shell_debug=2 / the debug capture's per-draw probe, furprobe.rs): the draw's index in the frame.
        let mut fd = fd;
        if !HL_ACTIVE.load(Relaxed) && crate::furprobe::tagging() {
            let v = fd.get_or_insert([[0.0f32; 4]; crate::shellfur::FD_LEN]);
            v[crate::shellfur::FD_TAG] = [crate::furprobe::tag_value(probe_idx), crate::furprobe::TAG_MARK, 0.0, 0.0];
        }
        if let Some(m) = g.as_mut() {
            m.fd_next = fd;
        }
        match g.as_mut().map(|m| unsafe { m.draw(dev, ctx, clear, issue, uv_arg) }) {
            Some(Ok(generated)) => Ok(Some(generated && has_uv)),
            Some(Err(why)) => {
                if clear {
                    FRESH.store(true, Relaxed); // the mask was not cleared: the next draw does it
                }
                Err(why)
            }
            None => Ok(None),
        }
    };
    unsafe { release(dev) };
    match result {
        Ok(None) => {}
        Ok(Some(with_uv)) => {
            DRAWN.store(true, Relaxed);
            CUR_DRAWS.fetch_add(1, Relaxed);
            // Shell fur (shellfur.rs): a masked fur draw may also be recorded for the shells, replayed at the light-adjust draw.
            let shells = !HL_ACTIVE.load(Relaxed) && unsafe { crate::shellfur::on_fur_draw(ctx, call, ps_hash, vs_hash, vcb0, vs_ptr) };
            if probing {
                crate::furprobe::push_draw(probe_idx, ps_hash, vs_hash, vcb0, call, shells, probe_tex);
            }
            if with_uv {
                CUR_UV.fetch_add(1, Relaxed);
            }
            if !FIRST_DRAW_LOGGED.swap(true, Relaxed) {
                log!("furmask: first masked draw (ps {}, vs {}, vertex cb0 {} bytes)", format!("{ps_hash:08x}"), hash_text(vs_hash), vcb0);
            }
        }
        Err(why) => {
            static WHY_LOGGED: AtomicBool = AtomicBool::new(false);
            if !WHY_LOGGED.swap(true, Relaxed) {
                log!("furmask: a draw could not be masked: {why}");
            }
        }
    }
}

fn hash_text(h: Option<u32>) -> String {
    h.map_or("unknown".to_string(), |h| format!("{h:08x}"))
}

/// (vertex shader hash, vertex constant buffer 0 size in bytes, vertex shader pointer) of the draw about to be issued.
///
/// # Safety
/// `ctx` must be a live `ID3D11DeviceContext`.
pub(crate) unsafe fn read_draw(ctx: Obj) -> (Option<u32>, u32, usize) {
    unsafe {
        type GetShader = unsafe extern "system" fn(Obj, *mut Obj, *mut Obj, *mut u32);
        let mut sh: Obj = std::ptr::null_mut();
        let get_vs: GetShader = std::mem::transmute(vfn(ctx, vt::CTX_VS_GET_SHADER));
        get_vs(ctx, &mut sh, std::ptr::null_mut(), std::ptr::null_mut());
        let vs = if sh.is_null() { None } else { shaders::vs_registry().hash_of(sh as usize) };
        let vs_ptr = sh as usize;
        release(sh);
        let mut cb: Obj = std::ptr::null_mut();
        let get_cb: unsafe extern "system" fn(Obj, u32, u32, *mut Obj) = std::mem::transmute(vfn(ctx, vt::CTX_VS_GET_CBS));
        get_cb(ctx, 0, 1, &mut cb);
        let w = crate::drawdiag::buffer_width(cb);
        release(cb);
        (vs, w, vs_ptr)
    }
}

/// `t20` / `t23` of the light-adjust draw: the masks when something was masked this frame, nothing otherwise (reads as zero).
/// First, once per frame and channel, the occlusion check (`Mask::validate`): mask pixels covered by geometry drawn later in the
/// pass are erased, so the shader only sees pixels whose character fragment is the visible one. Any failure keeps the mask as it is.
pub fn on_light_adjust(ctx: Obj) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        let mut srv: Obj = std::ptr::null_mut();
        let mut rim_srv: Obj = std::ptr::null_mut();
        let (fur_on, rim_on) = (DRAWN.load(Relaxed) && wanted(), RIM_DRAWN.load(Relaxed) && wanted_rim());
        if fur_on || rim_on {
            if let Ok(mut g) = STATE.try_lock() {
                let check_fur = fur_on && !OCC_DONE.swap(true, Relaxed);
                let check_rim = rim_on && !RIM_OCC_DONE.swap(true, Relaxed);
                if let (true, Some(m)) = (check_fur || check_rim, g.as_mut()) {
                    // The debug capture's diagnostic reads the masks BEFORE the check changes them (one stall, debug captures only).
                    // (only in a frame that collected its fur draws from the start: furprobe.rs; at most one frame later)
                    if crate::furprobe::collecting() && PROBE_WANT.swap(false, Relaxed) {
                        probe(m, ctx, check_fur, check_rim);
                    }
                    if OCC_ON.load(Relaxed) {
                        run_checks(m, ctx, check_fur, check_rim);
                    } else {
                        for (on, cur) in [(check_fur, &OCC_CUR), (check_rim, &RIM_OCC_CUR)] {
                            if on {
                                cur.store(OCC_OFF, Relaxed);
                            }
                        }
                    }
                }
                if let Some(m) = g.as_ref() {
                    if fur_on {
                        srv = m.srv();
                    }
                    if rim_on {
                        rim_srv = m.rim_srv();
                    }
                }
            }
        }
        let set_srv: unsafe extern "system" fn(Obj, u32, u32, *const Obj) = std::mem::transmute(vfn(ctx, vt::CTX_PS_SET_SRVS));
        set_srv(ctx, SRV_SLOT, 1, &srv);
        set_srv(ctx, RIM_SRV_SLOT, 1, &rim_srv);
    }));
}

/// A fur draw of this frame was masked with the discriminator's constants (v3; reset at the frame start).
static FD_FRAME: AtomicBool = AtomicBool::new(false);

/// The fur mask's view when this frame's mask went through the fur discriminator (borrowed: the mask lives in the state until the device
/// changes), for the shell replay's root test (v3); null otherwise (the shells then run exactly as v2) or when the lock is busy.
#[allow(dead_code)] // v7: the shells no longer read the mask in the strand shader (the per-strand discriminator)
pub fn fur_mask_srv() -> Obj {
    if !DRAWN.load(Relaxed) || !wanted() || !FD_FRAME.load(Relaxed) {
        return std::ptr::null_mut();
    }
    STATE.try_lock().ok().and_then(|g| g.as_ref().map(|m| m.srv())).unwrap_or(std::ptr::null_mut())
}

/// The occlusion checks of this frame on the game's context (`on_light_adjust`, the mask lock held).
unsafe fn run_checks(m: &mut Mask, ctx: Obj, fur: bool, rim: bool) {
    let blobs = match OCC_BLOBS.get() {
        None => {
            for (on, cur) in [(fur, &OCC_CUR), (rim, &RIM_OCC_CUR)] {
                if on {
                    cur.store(OCC_PENDING, Relaxed);
                }
            }
            return;
        }
        Some(Err(_)) => {
            for (on, ch) in [(fur, false), (rim, true)] {
                if on {
                    note_occ(ch, &Occ::Unavailable(Reason::Static("the check shaders failed to compile, see log")));
                }
            }
            return;
        }
        Some(Ok(b)) => b,
    };
    let mut dev: Obj = std::ptr::null_mut();
    unsafe {
        let get_device: unsafe extern "system" fn(Obj, *mut Obj) = std::mem::transmute(vfn(ctx, vt::CHILD_GET_DEVICE));
        get_device(ctx, &mut dev);
    }
    if dev.is_null() {
        return;
    }
    for (on, ch) in [(fur, false), (rim, true)] {
        if !on {
            continue;
        }
        let r = if m.device != dev as usize { Occ::Unavailable(Reason::Static("the masks belong to another device")) } else { unsafe { m.validate(dev, ctx, ch, blobs) } };
        note_occ(ch, &r);
    }
    unsafe { release(dev) };
}

// ---- debug capture diagnostic: status_occlusion= ---------------------------------------------------------------

/// The debug capture asked for the occlusion diagnostic: the next light-adjust draw that checks reads the masks, the z-stamps and the
/// engine's depth back (BEFORE the check), emulates the check on the CPU and leaves the result for `probe_text`.
static PROBE_WANT: AtomicBool = AtomicBool::new(false);
static PROBE_TEXT: Mutex<String> = Mutex::new(String::new());

/// The debug capture's `status_occlusion=` line: the result of the last probe (a frame after the previous capture) and a request for
/// the next one. Costs nothing outside the debug captures.
pub fn probe_text() -> String {
    PROBE_WANT.store(true, Relaxed);
    let t = PROBE_TEXT.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let state = format!("fur_occlusion {} ({})", OCC_ON.load(Relaxed) as u8, if OCC_ON.load(Relaxed) { "check on" } else { "check bypassed, the numbers say what it WOULD erase" });
    if t.is_empty() { format!("{state}; no probe yet (requested now: the next debug capture reports the frame after this one)") } else { format!("{state}; {t}") }
}

/// One channel of a probe: the CPU emulation of `ps_occ_less` / `ps_occ_greater` on the read-back textures.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Probe {
    /// Mask pixels set (a > 0.5).
    pub masked: u64,
    /// Pixels with a stamp in 0..1 (anywhere), and mask pixels without one (static fallback shader, never checked).
    pub stamped: u64,
    pub masked_unstamped: u64,
    /// Mask pixels the check erases (a later surface nearer than the stamp by more than OCC_TOL), and of those by more than 1e-3.
    pub erase: u64,
    pub erase_big: u64,
    /// Mask pixels whose final depth is FARTHER than the stamp by more than OCC_TOL (the engine's draw discarded the stamped
    /// fragment, e.g. alpha test; kept by the check).
    pub farther: u64,
    /// Median and largest depth gap (stamp - final, toward the camera) of the erased pixels.
    pub gap_median: f32,
    pub gap_max: f32,
}

/// The engine's depth as floats from the raw texels of a typeless depth texture (`fmt`), or `None` for a format the probe cannot read.
pub fn decode_depth(fmt: u32, raw: &[u8]) -> Option<Vec<f32>> {
    Some(match fmt {
        44 | 45 | 46 => raw.chunks_exact(4).map(|c| (u32::from_le_bytes([c[0], c[1], c[2], c[3]]) & 0x00ff_ffff) as f32 / 16_777_215.0).collect(),
        39 | 40 | 41 => raw.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect(),
        53 | 55 | 56 => raw.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]]) as f32 / 65_535.0).collect(),
        19 | 20 | 21 => raw.chunks_exact(8).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect(),
        _ => return None,
    })
}

/// Bytes per texel of the depth formats `decode_depth` reads.
fn depth_bpp(fmt: u32) -> Option<usize> {
    match fmt {
        44 | 45 | 46 | 39 | 40 | 41 => Some(4),
        53 | 55 | 56 => Some(2),
        19 | 20 | 21 => Some(8),
        _ => None,
    }
}

/// Pure: the CPU emulation of the check (`mode` = `occ_mode` of the frame's test; `None` = the check is skipped, nothing erased) with
/// the smallest tolerance `OCC_TOL`.
#[allow(dead_code)] // the tests use it; the game uses probe_counts_tol
pub fn probe_counts(mask: &[bool], stamp: &[f32], depth: &[f32], mode: Option<bool>) -> Probe {
    probe_counts_tol(mask, stamp, depth, mode, OCC_TOL)
}

/// `probe_counts` with the tolerance the check of that frame used (`Mask::tolerance`).
pub fn probe_counts_tol(mask: &[bool], stamp: &[f32], depth: &[f32], mode: Option<bool>, tol: f32) -> Probe {
    let tol = if tol.is_finite() { tol.max(OCC_TOL) } else { OCC_TOL };
    let mut p = Probe::default();
    let mut gaps: Vec<f32> = Vec::new();
    for ((&m, &s), &z) in mask.iter().zip(stamp).zip(depth) {
        let stamped = (0.0..=1.0).contains(&s);
        p.stamped += stamped as u64;
        if !m {
            continue;
        }
        p.masked += 1;
        if !stamped {
            p.masked_unstamped += 1;
            continue;
        }
        // Gap toward the camera: positive = the final surface is nearer than the stamped fragment.
        let gap = match mode {
            Some(true) => z - s,
            _ => s - z,
        };
        if !gap.is_finite() {
            continue;
        }
        if mode.is_some() && gap > tol {
            p.erase += 1;
            p.erase_big += (gap > 1e-3) as u64;
            gaps.push(gap);
        } else if gap < -tol {
            p.farther += 1;
        }
    }
    if !gaps.is_empty() {
        gaps.sort_by(|a, b| a.total_cmp(b));
        p.gap_median = gaps[gaps.len() / 2];
        p.gap_max = gaps[gaps.len() - 1];
    }
    p
}

/// Pure: the text of one channel of `status_occlusion=`.
pub fn probe_line(name: &str, test: u32, depth: &str, p: &Probe) -> String {
    let pct = |n: u64| if p.masked == 0 { 0.0 } else { n as f64 * 100.0 / p.masked as f64 };
    let check = match occ_mode(test) {
        Some(_) => format!("would erase {} ({:.1} % of the mask; {} by more than 1e-3; gap median {:.2e}, max {:.2e})", p.erase, pct(p.erase), p.erase_big, p.gap_median, p.gap_max),
        None => format!("check skipped (depth test {})", test_name(test)),
    };
    format!(
        "{name}: depth test {}, depth {depth}: mask a>0.5 {} px, stamped 0..1 {} px ({} mask px unstamped = never checked), {check}, {} mask px with the final depth farther than the stamp",
        test_name(test),
        p.masked,
        p.stamped,
        p.masked_unstamped,
        p.farther
    )
}

/// Pure: the tail of a probe line: the tolerance the check used, the depth bias of the last masked draw's rasterizer state (`bias` =
/// DepthBias, DepthBiasClamp bits, SlopeScaledDepthBias bits) and how many of the frame's masked draws stamped. A draw drawn with the
/// static fallback shader (while the shaders generated per vertex signature are still compiling, e.g. the first frames after a
/// loading screen) stamps nothing: such a frame reads "stamped 0 px" and is never checked (this was the "0 stamped" of the first probe).
pub fn probe_extra(tol: f32, bias: [u32; 3], generated: u32, fallback: u32) -> String {
    let mut t = format!(
        "; tolerance {tol:.2e} (OCC_TOL {OCC_TOL:.0e} + depth bias: DepthBias {}, DepthBiasClamp {}, SlopeScaledDepthBias {} on the last masked draw); {generated} masked draws stamped",
        bias[0] as i32,
        f32::from_bits(bias[1]),
        f32::from_bits(bias[2])
    );
    if fallback > 0 {
        t.push_str(&format!(", {fallback} drew with the static fallback shader (the generated shaders were still compiling: those pixels have no stamp and are never checked)"));
    }
    t
}

/// Copies `tex` (`w` x `h`, `fmt`, `bpp` bytes per texel) to a staging texture and maps it (waits for the GPU: debug captures only).
unsafe fn read_tex(dev: Obj, ctx: Obj, tex: Obj, w: u32, h: u32, fmt: u32, bpp: usize) -> Option<Vec<u8>> {
    unsafe {
        let st = crate::debugshot::create_staging(dev, w, h, fmt);
        if st.is_null() {
            return None;
        }
        crate::debugshot::copy_resource(ctx, st, tex);
        let r = crate::debugshot::read_texture(ctx, st, 0, w, h, bpp);
        release(st);
        r
    }
}

/// The probe of one channel (the mask lock held, before the check of this frame).
unsafe fn probe_channel(m: &Mask, dev: Obj, ctx: Obj, rim: bool) -> String {
    let name = if rim { "rim" } else { "fur" };
    let (t, z, (w, h), test, res, ordered) = if rim { (m.rim, m.rim_zst, m.rim_size, m.rim_test, m.rim_depth, m.rim_ordered) } else { (m.mask, m.zst, m.size, m.fur_test, m.fur_depth, m.fur_ordered) };
    if t.tex.is_null() || z.tex.is_null() || res.is_null() {
        return format!("{name}: nothing masked");
    }
    let _ig = IgnoreScope::new();
    unsafe {
        let mut d = Tex2dDesc::default();
        let get_desc: unsafe extern "system" fn(Obj, *mut Tex2dDesc) = std::mem::transmute(vfn(res, vt::TEX2D_GET_DESC));
        get_desc(res, &mut d);
        let fmt_name = crate::passes::format_name(d.format);
        let Some(bpp) = depth_bpp(d.format) else { return format!("{name}: the depth format {fmt_name} cannot be read back") };
        if (d.width, d.height) != (w, h) || d.samples != 1 {
            return format!("{name}: the depth buffer ({}x{}) does not match the mask ({w}x{h})", d.width, d.height);
        }
        let (mfmt, mbpp) = if rim { (RIM_FORMAT, 1) } else { (MASK_FORMAT, 8) };
        let (Some(mr), Some(zr), Some(dr)) = (read_tex(dev, ctx, t.tex, w, h, mfmt, mbpp), read_tex(dev, ctx, z.tex, w, h, STAMP_FORMAT, 4), read_tex(dev, ctx, res, w, h, d.format, bpp)) else {
            return format!("{name}: read-back failed");
        };
        let mask: Vec<bool> = if rim { mr.iter().map(|&b| b > 127).collect() } else { mr.chunks_exact(8).map(|c| u16::from_le_bytes([c[6], c[7]]) > 32767).collect() };
        let stamp: Vec<f32> = zr.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
        let Some(depth) = decode_depth(d.format, &dr) else { return format!("{name}: the depth format {fmt_name} cannot be decoded") };
        let tol = m.tolerance(rim);
        let p = probe_counts_tol(&mask, &stamp, &depth, occ_mode(test), tol);
        let mut line = probe_line(name, test, &format!("{fmt_name} {w}x{h}"), &p);
        if !ordered {
            line.push_str(" (stamps unordered: R32_FLOAT blending unsupported, the check does not run)");
        }
        let (ng, ns) = if rim { (m.rim_gen, m.rim_static) } else { (m.fur_gen, m.fur_static) };
        line.push_str(&probe_extra(tol, [LAST_BIAS[0].load(Relaxed), LAST_BIAS[1].load(Relaxed), LAST_BIAS[2].load(Relaxed)], ng, ns));
        if !rim {
            // v3: what the fur discriminator did in this frame, and the mesh uv at the screen centre and under the mouse.
            let texels: Vec<[u16; 4]> = mr.chunks_exact(8).map(|c| [u16::from_le_bytes([c[0], c[1]]), u16::from_le_bytes([c[2], c[3]]), u16::from_le_bytes([c[4], c[5]]), u16::from_le_bytes([c[6], c[7]])]).collect();
            let mut pts = vec![("centre", w / 2, h / 2)];
            if let Some(p) = cursor_in(w, h) {
                pts.push(("mouse", p.0, p.1));
            }
            *DISC_PROBE.lock().unwrap_or_else(|e| e.into_inner()) = discrim_probe(&texels, &stamp, w, h, &pts);
            // The per-draw probe: the textures of the collected fur draws are read back here, the rest runs on a background thread.
            let pts: Vec<(String, u32, u32)> = pts.iter().map(|&(n, x, y)| (n.to_string(), x, y)).collect();
            crate::furprobe::analyze(dev, ctx, texels, stamp, w, h, pts);
        }
        line
    }
}

/// The fur discriminator part of the last probe (`status_shells=`).
static DISC_PROBE: Mutex<String> = Mutex::new(String::new());

/// `status_shells=`: what the discriminator did in the frame of the last debug-capture probe and the uv at the centre / the mouse.
/// It does not ask for a probe itself (the overlay shows this text every frame): the debug capture's `probe_text` does.
pub fn discrim_probe_text() -> String {
    let t = DISC_PROBE.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if t.is_empty() { "discriminator probe: none yet (the next debug capture reports it)".to_string() } else { t }
}

/// The mouse position in the pixels of a `w` x `h` mask (the game window's client area scaled), `None` outside it or without a window.
fn cursor_in(w: u32, h: u32) -> Option<(u32, u32)> {
    use windows_sys::Win32::Foundation::POINT;
    use windows_sys::Win32::Graphics::Gdi::ScreenToClient;
    use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;
    let hwnd = crate::output::game_hwnd();
    let (cw, ch) = crate::output::game_client_size()?;
    if hwnd == 0 || w == 0 || h == 0 {
        return None;
    }
    let mut p = POINT { x: 0, y: 0 };
    // SAFETY: plain queries; a stale handle just fails.
    if unsafe { GetCursorPos(&mut p) } == 0 || unsafe { ScreenToClient(hwnd as _, &mut p) } == 0 {
        return None;
    }
    if p.x < 0 || p.y < 0 || p.x as u32 >= cw || p.y as u32 >= ch {
        return None;
    }
    Some(((p.x as u64 * w as u64 / cw as u64) as u32, (p.y as u64 * h as u64 / ch as u64) as u32))
}

/// Pure: the discriminator's numbers from the read-back fur mask (`texels` = r g b a of R16G16B16A16_UNORM: rg = frac(uv), b = uv valid,
/// a = fur weight) and its z-stamps (a stamp in 0..1 = a pixel the generated uv shader drew), plus the uv / weight at `points` (the
/// nearest drawn pixel within 48 px when the point itself shows no fur draw).
pub fn discrim_probe(texels: &[[u16; 4]], stamp: &[f32], w: u32, h: u32, points: &[(&str, u32, u32)]) -> String {
    let n = (w as usize * h as usize).min(texels.len()).min(stamp.len());
    let drawn = |i: usize| (0.0..=1.0).contains(&stamp[i]) && texels[i][2] > 32767;
    let (mut d, mut not_fur, mut soft) = (0u64, 0u64, 0u64);
    for i in 0..n {
        if !drawn(i) {
            continue;
        }
        d += 1;
        let a = texels[i][3];
        not_fur += (a <= 32767) as u64;
        soft += (a > 3276 && a < 62258) as u64;
    }
    let pct = |x: u64| if d == 0 { 0.0 } else { x as f64 * 100.0 / d as f64 };
    let mut t = format!("discriminator probe: {d} px drawn by the fur draws, {not_fur} not fur ({:.1} %), {soft} in the soft edge ({:.1} %)", pct(not_fur), pct(soft));
    for &(name, x, y) in points {
        if w == 0 || h == 0 || n == 0 {
            break;
        }
        let (x, y) = (x.min(w - 1), y.min(h - 1));
        let mut best: Option<(u32, usize)> = None;
        let r = 48i64;
        for dy in -r..=r {
            for dx in -r..=r {
                let (px, py) = (x as i64 + dx, y as i64 + dy);
                if px < 0 || py < 0 || px >= w as i64 || py >= h as i64 {
                    continue;
                }
                let i = py as usize * w as usize + px as usize;
                if i >= n || !drawn(i) {
                    continue;
                }
                let dist = (dx * dx + dy * dy) as u32;
                if best.is_none_or(|(b, _)| dist < b) {
                    best = Some((dist, i));
                }
            }
        }
        match best {
            Some((dist, i)) => {
                let tx = texels[i];
                let (u, v, a) = (tx[0] as f32 / 65535.0, tx[1] as f32 / 65535.0, tx[3] as f32 / 65535.0);
                let at = if dist == 0 { String::new() } else { format!(" (nearest fur-draw pixel, {:.0} px away)", (dist as f32).sqrt()) };
                t.push_str(&format!("; {name} px ({x}, {y}): uv ({u:.4}, {v:.4}) fur weight {a:.2}{at}"));
            }
            None => t.push_str(&format!("; {name} px ({x}, {y}): no fur draw within 48 px")),
        }
    }
    t
}

unsafe fn probe(m: &Mask, ctx: Obj, fur: bool, rim: bool) {
    let mut dev: Obj = std::ptr::null_mut();
    unsafe {
        let get_device: unsafe extern "system" fn(Obj, *mut Obj) = std::mem::transmute(vfn(ctx, vt::CHILD_GET_DEVICE));
        get_device(ctx, &mut dev);
    }
    if dev.is_null() {
        return;
    }
    let mut parts = Vec::new();
    for (on, ch) in [(fur, false), (rim, true)] {
        if on && m.device == dev as usize {
            parts.push(unsafe { probe_channel(m, dev, ctx, ch) });
        }
    }
    unsafe { release(dev) };
    *PROBE_TEXT.lock().unwrap_or_else(|e| e.into_inner()) = if parts.is_empty() { "nothing masked in the probed frame".to_string() } else { parts.join("; ") };
}

// ---- device objects ------------------------------------------------------------------------------------

static STATE: Mutex<Option<Mask>> = Mutex::new(None);

/// A mask target of `fmt` and its R32_FLOAT z-stamp of the same size (both or neither).
unsafe fn make_pair(dev: Obj, w: u32, h: u32, fmt: u32) -> Option<(Target, Target)> {
    unsafe {
        let mut t = make_target(dev, w, h, fmt, false)?;
        match make_target(dev, w, h, STAMP_FORMAT, false) {
            Some(z) => Some((t, z)),
            None => {
                t.free();
                None
            }
        }
    }
}

/// The mask target, the pixel shader and the cache of read-only depth states of one device.
pub struct Mask {
    device: usize,
    ps: Obj,
    /// Pixel shaders built from vertex-shader signatures (`uv_ps_hlsl`), by key.
    uv_ps: Vec<(String, Obj)>,
    mask: Target,
    size: (u32, u32),
    /// The rim channel: its pixel shader and its R8 target (created on first use).
    ps_rim: Obj,
    rim: Target,
    rim_size: (u32, u32),
    /// (DepthEnable, DepthFunc) of the engine's state -> our state with the same test and no depth writes.
    states: Vec<(u32, u32, Obj)>,
    /// The z-stamps (R32_FLOAT, the size of their mask): bound as RTV1 beside the fur / the rim mask.
    zst: Target,
    rim_zst: Target,
    /// The engine's depth buffer of the last fur / rim draw (a counted reference: the occlusion check reads it).
    fur_depth: Obj,
    rim_depth: Obj,
    /// The depth test of this frame's fur / rim draws (DepthFunc, 0 = off, `TEST_MIXED`).
    fur_test: u32,
    rim_test: u32,
    /// Shader-resource views of depth buffers, by resource pointer (`Err` = why none can exist; cached so a failure costs nothing).
    depth_srvs: Vec<(usize, Result<Obj, Arc<str>>)>,
    /// The check's full-screen vertex shader and its two pixel shaders (created on first use; `occ_failed` = the device refused).
    occ_vs: Obj,
    occ_less: Obj,
    occ_greater: Obj,
    occ_failed: bool,
    /// The blend states of the stamp draws: [MIN (LESS), MAX (reversed Z)]; `stamp_blend_ok` = None until tried, Some(false) = the
    /// device cannot blend R32_FLOAT (the stamps are then unordered and the check does not run: fail open).
    stamp_blend: [Obj; 2],
    stamp_blend_ok: Option<bool>,
    /// Every stamping draw of this frame's fur / rim channel kept the nearest fragment (set to true again with the clear).
    fur_ordered: bool,
    rim_ordered: bool,
    /// The largest depth-bias tolerance (`bias_tol`) of this frame's fur / rim draws; the check adds it to `OCC_TOL`.
    fur_bias: f32,
    rim_bias: f32,
    /// Draws of this frame per channel that used a generated (stamping) shader / the static fallback (stamps nothing).
    fur_gen: u32,
    fur_static: u32,
    rim_gen: u32,
    rim_static: u32,
    /// `b0` of the check (the tolerance), created on first use.
    occ_cb: Obj,
    /// v3 fur discriminator: `b13` of the uv mask shader (`shellfur::fd_cb_values`, created on first use) and the values for the NEXT
    /// fur draw (`fur_draw` sets them, `draw_into` binds them for that draw only and takes them). `None` = nothing bound (a = 1).
    fd_cb: Obj,
    pub(crate) fd_next: Option<[[f32; 4]; crate::shellfur::FD_LEN]>,
}

// Only touched under the mutex from the render thread.
unsafe impl Send for Mask {}

impl Mask {
    /// # Safety
    /// `dev` must be a live `ID3D11Device`; `blob` the compiled `ps_mask`.
    pub unsafe fn new(dev: Obj, blob: &[u8]) -> Option<Mask> {
        let _ig = IgnoreScope::new();
        let nul = std::ptr::null_mut();
        let mut m = Mask {
            device: dev as usize,
            ps: nul,
            uv_ps: Vec::new(),
            mask: NO_TARGET,
            size: (0, 0),
            ps_rim: nul,
            rim: NO_TARGET,
            rim_size: (0, 0),
            states: Vec::new(),
            zst: NO_TARGET,
            rim_zst: NO_TARGET,
            fur_depth: nul,
            rim_depth: nul,
            fur_test: 0,
            rim_test: 0,
            depth_srvs: Vec::new(),
            occ_vs: nul,
            occ_less: nul,
            occ_greater: nul,
            occ_failed: false,
            stamp_blend: [nul, nul],
            stamp_blend_ok: None,
            fur_ordered: true,
            rim_ordered: true,
            fur_bias: 0.0,
            rim_bias: 0.0,
            fur_gen: 0,
            fur_static: 0,
            rim_gen: 0,
            rim_static: 0,
            occ_cb: nul,
            fd_cb: nul,
            fd_next: None,
        };
        unsafe {
            let create_ps: unsafe extern "system" fn(Obj, *const u8, usize, Obj, *mut Obj) -> i32 = std::mem::transmute(vfn(dev, vt::DEV_CREATE_PIXEL_SHADER));
            if create_ps(dev, blob.as_ptr(), blob.len(), std::ptr::null_mut(), &mut m.ps) < 0 || m.ps.is_null() {
                return None;
            }
        }
        Some(m)
    }

    /// The mask's shader-resource view (null before the first draw).
    pub fn srv(&self) -> Obj {
        self.mask.srv
    }

    #[allow(dead_code)]
    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    /// The rim mask's shader-resource view (null before the first rim draw).
    pub fn rim_srv(&self) -> Obj {
        self.rim.srv
    }

    /// Creates the rim mask pixel shader from its compiled blob on first use.
    ///
    /// # Safety
    /// `dev` must be the live `ID3D11Device` this was created on.
    pub unsafe fn ensure_rim_ps(&mut self, dev: Obj, blob: &[u8]) -> bool {
        if !self.ps_rim.is_null() {
            return true;
        }
        let _ig = IgnoreScope::new();
        unsafe {
            let create_ps: unsafe extern "system" fn(Obj, *const u8, usize, Obj, *mut Obj) -> i32 = std::mem::transmute(vfn(dev, vt::DEV_CREATE_PIXEL_SHADER));
            create_ps(dev, blob.as_ptr(), blob.len(), std::ptr::null_mut(), &mut self.ps_rim) >= 0 && !self.ps_rim.is_null()
        }
    }

    unsafe fn ensure_rim_target(&mut self, dev: Obj, w: u32, h: u32) -> bool {
        if !self.rim.rtv.is_null() && self.rim_size == (w, h) {
            return true;
        }
        unsafe {
            self.rim.free();
            self.rim_zst.free();
            match make_pair(dev, w, h, RIM_FORMAT) {
                Some((t, z)) => {
                    self.rim = t;
                    self.rim_zst = z;
                    self.rim_size = (w, h);
                    RIM_H.store(h, Relaxed);
                    log!("furmask: rim mask target {w}x{h} R8_UNORM + R32_FLOAT z-stamp created");
                    true
                }
                None => {
                    self.rim_size = (0, 0);
                    false
                }
            }
        }
    }

    unsafe fn ensure_target(&mut self, dev: Obj, w: u32, h: u32) -> bool {
        if !self.mask.rtv.is_null() && self.size == (w, h) {
            return true;
        }
        unsafe {
            self.mask.free();
            self.zst.free();
            match make_pair(dev, w, h, MASK_FORMAT) {
                Some((t, z)) => {
                    self.mask = t;
                    self.zst = z;
                    self.size = (w, h);
                    log!("furmask: mask target {w}x{h} R16G16B16A16_UNORM (uv + mask) + R32_FLOAT z-stamp created");
                    true
                }
                None => {
                    self.size = (0, 0);
                    false
                }
            }
        }
    }

    /// Our depth-stencil state for the engine's `(enable, func)`: the same depth test, depth writes off, stencil off.
    unsafe fn read_only_state(&mut self, dev: Obj, enable: u32, func: u32) -> Option<Obj> {
        if let Some(&(_, _, s)) = self.states.iter().find(|&&(e, f, _)| e == enable && f == func) {
            return Some(s);
        }
        if self.states.len() >= 16 {
            return None;
        }
        // D3D11_DEPTH_STENCIL_DESC: DepthEnable, DepthWriteMask (0 = ZERO), DepthFunc, StencilEnable, StencilReadMask | StencilWriteMask << 8,
        // then front and back face { StencilFailOp, StencilDepthFailOp, StencilPassOp (KEEP = 1), StencilFunc (ALWAYS = 8) }.
        let desc: [u32; 13] = [enable, 0, func, 0, 0xffff, 1, 1, 1, 8, 1, 1, 1, 8];
        let mut s: Obj = std::ptr::null_mut();
        unsafe {
            let create: unsafe extern "system" fn(Obj, *const [u32; 13], *mut Obj) -> i32 = std::mem::transmute(vfn(dev, DEV_CREATE_DEPTH_STENCIL_STATE));
            if create(dev, &desc, &mut s) < 0 || s.is_null() {
                return None;
            }
        }
        self.states.push((enable, func, s));
        Some(s)
    }

    /// Issues `issue` (the engine's draw, same arguments) into the mask. `clear` zeroes the mask first. The engine's
    /// pipeline state is restored on every exit path.
    ///
    /// # Safety
    /// `dev` / `ctx` live objects of the device this was created on, `ctx` the game's immediate context with the
    /// draw's state bound (vertex shader, layout, buffers, a depth-stencil view).
    pub unsafe fn draw(&mut self, dev: Obj, ctx: Obj, clear: bool, issue: &dyn Fn(), uv: Option<(&str, &[u8])>) -> Result<bool, &'static str> {
        unsafe { self.draw_into(dev, ctx, false, clear, issue, uv) }
    }

    /// `draw` for either channel: `rim` = the R8 rim target and `ps_rim` (no uv, alpha-blended draws are refused), else the fur target.
    /// `uv` = the generated pixel shader linked to the draw's vertex shader (`uv_ps_hlsl` / `z_ps_hlsl`), `None` (or a blob the
    /// device refuses) = the static fallback, which stamps nothing. `Ok(true)` = the generated shader was used. Both the mask
    /// (RTV0) and its z-stamp (RTV1) are written; `clear` clears the mask to 0 and the stamp to -1.
    ///
    /// # Safety
    /// As `draw`; for `rim`, `ensure_rim_ps` must have succeeded.
    pub unsafe fn draw_into(&mut self, dev: Obj, ctx: Obj, rim: bool, clear: bool, issue: &dyn Fn(), uv: Option<(&str, &[u8])>) -> Result<bool, &'static str> {
        unsafe {
            let g = StateGuard::save(ctx); // also turns the ignore flag on: our calls are not recorded
            if rim {
                // Stealth / transparent characters are drawn with an alpha blend: no rim light on them.
                let bs = g.blend();
                if !bs.is_null() {
                    let mut desc = [0u32; 66];
                    let get: unsafe extern "system" fn(Obj, *mut [u32; 66]) = std::mem::transmute(vfn(bs, BS_GET_DESC));
                    get(bs, &mut desc);
                    if blend_is_transparent(&desc) {
                        return Err("alpha-blended draw");
                    }
                }
            }
            let dsv = g.dsv();
            if dsv.is_null() {
                return Err("no depth-stencil view bound");
            }
            // The depth buffer decides the size of the mask (a render target must match its depth buffer).
            let mut res: Obj = std::ptr::null_mut();
            let get_resource: unsafe extern "system" fn(Obj, *mut Obj) = std::mem::transmute(vfn(dsv, vt::VIEW_GET_RESOURCE));
            get_resource(dsv, &mut res);
            if res.is_null() {
                return Err("depth view without resource");
            }
            let mut d = Tex2dDesc::default();
            let get_desc: unsafe extern "system" fn(Obj, *mut Tex2dDesc) = std::mem::transmute(vfn(res, vt::TEX2D_GET_DESC));
            get_desc(res, &mut d);
            if d.samples != 1 || d.width == 0 || d.height == 0 {
                release(res);
                return Err("multisampled or empty depth buffer");
            }
            // Keep the depth buffer for the occlusion check (one counted reference per channel, replaced when it changes).
            let slot = if rim { &mut self.rim_depth } else { &mut self.fur_depth };
            let depth_changed = *slot != res;
            if depth_changed {
                release(*slot);
                *slot = res;
            } else {
                release(res);
            }
            if rim {
                if !self.ensure_rim_target(dev, d.width, d.height) {
                    return Err("could not create the rim mask target");
                }
            } else if !self.ensure_target(dev, d.width, d.height) {
                return Err("could not create the mask target");
            }
            let rtvs = if rim { [self.rim.rtv, self.rim_zst.rtv] } else { [self.mask.rtv, self.zst.rtv] };
            // The engine's depth test (no state bound = the default: enabled, LESS).
            let (mut enable, mut func) = (1u32, 2u32);
            let es = g.depth_stencil();
            if !es.is_null() {
                let mut desc = [0u32; 13];
                let get: unsafe extern "system" fn(Obj, *mut [u32; 13]) = std::mem::transmute(vfn(es, DSS_GET_DESC));
                get(es, &mut desc);
                (enable, func) = (desc[0], desc[2]);
            }
            let Some(dss) = self.read_only_state(dev, enable, func) else { return Err("could not create the depth state") };
            // The depth bias of the engine's rasterizer state: the depth buffer holds the biased depth, the stamp may not (see OCC_TOL).
            let mut rsd = [0u32; 10];
            let rs = g.rasterizer();
            if !rs.is_null() {
                let get: unsafe extern "system" fn(Obj, *mut [u32; 10]) = std::mem::transmute(vfn(rs, RS_GET_DESC));
                get(rs, &mut rsd);
            }
            for (a, v) in LAST_BIAS.iter().zip([rsd[3], rsd[4], rsd[5]]) {
                a.store(v, Relaxed);
            }
            let btol = bias_tol(&rsd, d.format);
            // The test the check will use decides how the stamp keeps the nearest fragment (MIN, or MAX for reversed Z).
            let t = if enable == 0 { 0 } else { func };
            let greater = occ_mode(t) == Some(true);
            let stamp_bs = self.stamp_blend(dev, greater);

            let nul: Obj = std::ptr::null_mut();
            let set_srv: unsafe extern "system" fn(Obj, u32, u32, *const Obj) = std::mem::transmute(vfn(ctx, vt::CTX_PS_SET_SRVS));
            set_srv(ctx, SRV_SLOT, 1, &nul); // the light-adjust draw of the previous frame may have left the mask bound as t20 / t23
            set_srv(ctx, RIM_SRV_SLOT, 1, &nul);
            if clear {
                let clear_rtv: unsafe extern "system" fn(Obj, Obj, *const f32) = std::mem::transmute(vfn(ctx, CTX_CLEAR_RTV));
                clear_rtv(ctx, rtvs[0], [0.0f32; 4].as_ptr());
                let none = if greater { STAMP_CLEAR_GREATER } else { STAMP_CLEAR_LESS };
                clear_rtv(ctx, rtvs[1], [none; 4].as_ptr()); // outside 0..1 = nothing stamped
            }
            let set_rt: unsafe extern "system" fn(Obj, u32, *const Obj, Obj) = std::mem::transmute(vfn(ctx, vt::CTX_OM_SET_RENDER_TARGETS));
            let set_blend: unsafe extern "system" fn(Obj, Obj, *const f32, u32) = std::mem::transmute(vfn(ctx, vt::CTX_OM_SET_BLEND));
            let set_depth: unsafe extern "system" fn(Obj, Obj, u32) = std::mem::transmute(vfn(ctx, vt::CTX_OM_SET_DEPTH));
            let set_ps: unsafe extern "system" fn(Obj, Obj, *const Obj, u32) = std::mem::transmute(vfn(ctx, vt::CTX_PS_SET_SHADER));
            // RTV0 = the mask, RTV1 = its z-stamp. Our draw writes no depth, so its own fragments do not occlude each other: a
            // farther layer of the same mesh rasterised later passes the read-only test too. The mask (a = 1) does not care, but the
            // stamp must be the NEAREST fragment (the one the engine's depth keeps): blended with MIN (MAX for reversed Z), the
            // mask itself not blended. The static fallbacks stamp -1 unblended (see STAMP_CLEAR_LESS).
            let gen_ps = uv.and_then(|(key, blob)| self.uv_shader(dev, key, blob));
            set_rt(ctx, 2, rtvs.as_ptr(), dsv);
            set_blend(ctx, if gen_ps.is_some() { stamp_bs.unwrap_or(nul) } else { nul }, [1.0f32; 4].as_ptr(), 0xffff_ffff);
            set_depth(ctx, dss, 0);
            set_ps(ctx, gen_ps.unwrap_or(if rim { self.ps_rim } else { self.ps }), std::ptr::null(), 0);
            // v3 fur discriminator: the uv shader's b13 for this draw only (the engine's own b13, if any, is put back right after).
            let fd = if rim { None } else { self.fd_next.take() };
            let mut old_b13: Obj = nul;
            let mut fd_bound = false;
            if let (Some(v), true) = (fd, gen_ps.is_some()) {
                if self.fd_cb.is_null() {
                    let create_buffer: unsafe extern "system" fn(Obj, *const [u32; 6], *const std::ffi::c_void, *mut Obj) -> i32 = std::mem::transmute(vfn(dev, vt::DEV_CREATE_BUFFER));
                    let size = std::mem::size_of::<[[f32; 4]; crate::shellfur::FD_LEN]>() as u32;
                    let mut b: Obj = nul;
                    if create_buffer(dev, &[size, 2 /* DYNAMIC */, 4 /* CONSTANT_BUFFER */, 0x10000 /* CPU write */, 0, 0], std::ptr::null(), &mut b) >= 0 {
                        self.fd_cb = b;
                    }
                }
                if !self.fd_cb.is_null() && crate::shellfur::write_cb(ctx, self.fd_cb, &v) {
                    let get_cb: unsafe extern "system" fn(Obj, u32, u32, *mut Obj) = std::mem::transmute(vfn(ctx, vt::CTX_PS_GET_CBS));
                    get_cb(ctx, crate::shellfur::FD_CB_SLOT, 1, &mut old_b13);
                    let set_cb: unsafe extern "system" fn(Obj, u32, u32, *const Obj) = std::mem::transmute(vfn(ctx, vt::CTX_PS_SET_CBS));
                    set_cb(ctx, crate::shellfur::FD_CB_SLOT, 1, &self.fd_cb);
                    fd_bound = true;
                }
            }
            issue();
            if fd_bound {
                let set_cb: unsafe extern "system" fn(Obj, u32, u32, *const Obj) = std::mem::transmute(vfn(ctx, vt::CTX_PS_SET_CBS));
                set_cb(ctx, crate::shellfur::FD_CB_SLOT, 1, &old_b13);
                release(old_b13);
            }
            drop(g); // restores render targets, depth / blend state, shaders (it also restores t0, t1 and b0)
            // The depth test the check must use: the same for every draw of the frame, on the same depth buffer, or no check.
            let (test, ordered) = if rim { (&mut self.rim_test, &mut self.rim_ordered) } else { (&mut self.fur_test, &mut self.fur_ordered) };
            if clear {
                *test = t;
                *ordered = true;
            } else if depth_changed || *test != t {
                *test = TEST_MIXED;
            }
            if gen_ps.is_some() && stamp_bs.is_none() {
                *ordered = false;
            }
            let (bias, ng, ns) = if rim { (&mut self.rim_bias, &mut self.rim_gen, &mut self.rim_static) } else { (&mut self.fur_bias, &mut self.fur_gen, &mut self.fur_static) };
            if clear {
                (*bias, *ng, *ns) = (0.0, 0, 0);
            }
            *bias = bias.max(btol);
            if gen_ps.is_some() {
                *ng += 1;
            } else {
                *ns += 1;
            }
            Ok(gen_ps.is_some())
        }
    }

    /// The blend state of the stamping draws (`greater` = MAX for reversed Z, else MIN), created on first use; `None` when the device
    /// cannot blend R32_FLOAT or refuses the state (the stamps are then unordered and the check fails open).
    unsafe fn stamp_blend(&mut self, dev: Obj, greater: bool) -> Option<Obj> {
        if self.stamp_blend_ok.is_none() {
            let _ig = IgnoreScope::new();
            let ok = unsafe {
                let check: unsafe extern "system" fn(Obj, u32, *mut u32) -> i32 = std::mem::transmute(vfn(dev, DEV_CHECK_FORMAT_SUPPORT));
                let mut sup = 0u32;
                let blendable = check(dev, STAMP_FORMAT, &mut sup) >= 0 && sup & FORMAT_SUPPORT_BLENDABLE != 0;
                let create: unsafe extern "system" fn(Obj, *const [u32; 66], *mut Obj) -> i32 = std::mem::transmute(vfn(dev, vt::DEV_CREATE_BLEND_STATE));
                let (mut a, mut b): (Obj, Obj) = (std::ptr::null_mut(), std::ptr::null_mut());
                let made = blendable && create(dev, &stamp_blend_desc(BLEND_OP_MIN), &mut a) >= 0 && create(dev, &stamp_blend_desc(BLEND_OP_MAX), &mut b) >= 0 && !a.is_null() && !b.is_null();
                if made {
                    self.stamp_blend = [a, b];
                } else {
                    release(a);
                    release(b);
                    log!("furmask: the z-stamp cannot keep the nearest fragment (R32_FLOAT blendable: {blendable}); the occlusion check stays off");
                }
                made
            };
            self.stamp_blend_ok = Some(ok);
        }
        if self.stamp_blend_ok == Some(true) { Some(self.stamp_blend[greater as usize]) } else { None }
    }

    /// The device object of the uv pixel shader `key` (created on first use; `None` when the device refuses it).
    unsafe fn uv_shader(&mut self, dev: Obj, key: &str, blob: &[u8]) -> Option<Obj> {
        if let Some((_, s)) = self.uv_ps.iter().find(|(k, _)| k == key) {
            return Some(*s);
        }
        if self.uv_ps.len() >= MAX_GEN_PS {
            return None;
        }
        let mut ps: Obj = std::ptr::null_mut();
        unsafe {
            let create_ps: unsafe extern "system" fn(Obj, *const u8, usize, Obj, *mut Obj) -> i32 = std::mem::transmute(vfn(dev, vt::DEV_CREATE_PIXEL_SHADER));
            if create_ps(dev, blob.as_ptr(), blob.len(), std::ptr::null_mut(), &mut ps) < 0 || ps.is_null() {
                return None;
            }
        }
        self.uv_ps.push((key.to_string(), ps));
        Some(ps)
    }

    /// The z-stamp of a channel (its SRV is what the check reads; null before the first draw).
    #[allow(dead_code)]
    pub fn stamp_srv(&self, rim: bool) -> Obj {
        if rim { self.rim_zst.srv } else { self.zst.srv }
    }

    /// A shader-resource view of the depth texture `res` (cached by pointer; the failure too). Only typeless depth formats can be
    /// viewed (`depth_srv_format`), and only with the SHADER_RESOURCE bind flag.
    unsafe fn depth_srv(&mut self, dev: Obj, res: Obj) -> Result<Obj, Arc<str>> {
        if let Some((_, r)) = self.depth_srvs.iter().find(|(k, _)| *k == res as usize) {
            return r.clone();
        }
        let _ig = IgnoreScope::new(); // our view is not the engine's: no recording, no format upgrade
        let r: Result<Obj, Arc<str>> = unsafe {
            let mut d = Tex2dDesc::default();
            let get_desc: unsafe extern "system" fn(Obj, *mut Tex2dDesc) = std::mem::transmute(vfn(res, vt::TEX2D_GET_DESC));
            get_desc(res, &mut d);
            let name = crate::passes::format_name(d.format);
            match depth_srv_format(d.format) {
                None => Err(format!("depth format {name} ({}) is not a typeless depth format, no shader can read it", d.format).into()),
                Some(_) if d.bind & BIND_SRV == 0 => Err(format!("the depth buffer ({name}) has no shader-resource bind flag").into()),
                Some(_) if d.array != 1 || d.samples != 1 => Err(format!("the depth buffer ({name}) is an array or multisampled").into()),
                Some(f) => {
                    let create_srv: unsafe extern "system" fn(Obj, Obj, *const [u32; 6], *mut Obj) -> i32 = std::mem::transmute(vfn(dev, vt::DEV_CREATE_SRV));
                    let mut srv: Obj = std::ptr::null_mut();
                    // D3D11_SHADER_RESOURCE_VIEW_DESC: Format, ViewDimension = TEXTURE2D (4), MostDetailedMip 0, MipLevels 1.
                    let hr = create_srv(dev, res, &[f, 4, 0, 1, 0, 0], &mut srv);
                    if hr < 0 || srv.is_null() {
                        Err(format!("CreateShaderResourceView on the depth buffer ({name}) failed (0x{:08x})", hr as u32).into())
                    } else {
                        Ok(srv)
                    }
                }
            }
        };
        // Keep only the views of the depth buffers the channels hold now (each view also holds its texture).
        let (a, b) = (self.fur_depth as usize, self.rim_depth as usize);
        self.depth_srvs.retain(|(k, v)| {
            let keep = *k == a || *k == b;
            if !keep {
                if let Ok(s) = v {
                    unsafe { release(*s) };
                }
            }
            keep
        });
        self.depth_srvs.push((res as usize, r.clone()));
        r
    }

    /// Creates the check's shaders on first use.
    unsafe fn ensure_occ(&mut self, dev: Obj, b: &OccBlobs) -> bool {
        if !self.occ_vs.is_null() {
            return true;
        }
        if self.occ_failed {
            return false;
        }
        let _ig = IgnoreScope::new();
        unsafe {
            type CreateShader = unsafe extern "system" fn(Obj, *const u8, usize, Obj, *mut Obj) -> i32;
            let create_vs: CreateShader = std::mem::transmute(vfn(dev, vt::DEV_CREATE_VERTEX_SHADER));
            let create_ps: CreateShader = std::mem::transmute(vfn(dev, vt::DEV_CREATE_PIXEL_SHADER));
            let nul = std::ptr::null_mut();
            let (mut vs, mut less, mut greater) = (nul, nul, nul);
            let ok = create_vs(dev, b.vs.as_ptr(), b.vs.len(), nul, &mut vs) >= 0
                && create_ps(dev, b.less.as_ptr(), b.less.len(), nul, &mut less) >= 0
                && create_ps(dev, b.greater.as_ptr(), b.greater.len(), nul, &mut greater) >= 0
                && !vs.is_null()
                && !less.is_null()
                && !greater.is_null();
            if !ok {
                for o in [vs, less, greater] {
                    release(o);
                }
                self.occ_failed = true;
                return false;
            }
            (self.occ_vs, self.occ_less, self.occ_greater) = (vs, less, greater);
            // b0 of the check: one float4 (the tolerance), written before every check.
            let desc: [u32; 6] = [16, 2 /* DYNAMIC */, 4 /* CONSTANT_BUFFER */, 0x10000 /* CPU WRITE */, 0, 0];
            let create_buffer: unsafe extern "system" fn(Obj, *const [u32; 6], *const std::ffi::c_void, *mut Obj) -> i32 = std::mem::transmute(vfn(dev, vt::DEV_CREATE_BUFFER));
            if create_buffer(dev, &desc, std::ptr::null(), &mut self.occ_cb) < 0 {
                self.occ_cb = nul; // the check then uses OCC_TOL alone (unbound b0 reads 0)
            }
        }
        true
    }

    /// The tolerance of the check of a channel: `OCC_TOL` plus the depth bias of this frame's masked draws (`bias_tol`).
    pub fn tolerance(&self, rim: bool) -> f32 {
        OCC_TOL + if rim { self.rim_bias } else { self.fur_bias }
    }

    /// The occlusion check of one channel: a full-screen draw into the mask (blend off, no depth buffer) that sets to 0 every mask
    /// pixel whose stamped fragment is covered in the engine's final depth (a surface drawn later in the pass), with the depth test
    /// the engine used for the character (`occ_mode`). Pixels with no stamp (-1) or still visible are left bit-identical. Fail
    /// open: whenever the check cannot run, the mask is not touched. The pipeline state is restored and t0 / t1 hold the engine's
    /// views again (the depth view is never left bound).
    ///
    /// # Safety
    /// `dev` / `ctx` live objects of the device this was created on; no mask of this `Mask` may be bound as a render target.
    pub unsafe fn validate(&mut self, dev: Obj, ctx: Obj, rim: bool, b: &OccBlobs) -> Occ {
        let (t, z, (w, h), test, res) = if rim { (self.rim, self.rim_zst, self.rim_size, self.rim_test, self.rim_depth) } else { (self.mask, self.zst, self.size, self.fur_test, self.fur_depth) };
        if t.rtv.is_null() || z.srv.is_null() || res.is_null() {
            return Occ::Unavailable(Reason::Static("nothing was masked"));
        }
        let Some(greater) = occ_mode(test) else { return Occ::Skipped(test) };
        if !(if rim { self.rim_ordered } else { self.fur_ordered }) {
            return Occ::Unavailable(Reason::Static("the z-stamp could not keep the nearest fragment (R32_FLOAT blending unsupported)"));
        }
        let dsrv = match unsafe { self.depth_srv(dev, res) } {
            Ok(s) => s,
            Err(why) => return Occ::Unavailable(Reason::Depth(why)),
        };
        if !unsafe { self.ensure_occ(dev, b) } {
            return Occ::Unavailable(Reason::Static("the device refused the check shaders"));
        }
        unsafe {
            let g = StateGuard::save(ctx);
            let nul: Obj = std::ptr::null_mut();
            type SetShader = unsafe extern "system" fn(Obj, Obj, *const Obj, u32);
            type SetSlots = unsafe extern "system" fn(Obj, u32, u32, *const Obj);
            let set_srv: SetSlots = std::mem::transmute(vfn(ctx, vt::CTX_PS_SET_SRVS));
            let set_rt: unsafe extern "system" fn(Obj, u32, *const Obj, Obj) = std::mem::transmute(vfn(ctx, vt::CTX_OM_SET_RENDER_TARGETS));
            let set_vp: unsafe extern "system" fn(Obj, u32, *const Viewport) = std::mem::transmute(vfn(ctx, vt::CTX_RS_SET_VIEWPORTS));
            let set_rs: unsafe extern "system" fn(Obj, Obj) = std::mem::transmute(vfn(ctx, vt::CTX_RS_SET_STATE));
            let set_blend: unsafe extern "system" fn(Obj, Obj, *const f32, u32) = std::mem::transmute(vfn(ctx, vt::CTX_OM_SET_BLEND));
            let set_depth: unsafe extern "system" fn(Obj, Obj, u32) = std::mem::transmute(vfn(ctx, vt::CTX_OM_SET_DEPTH));
            let set_layout: unsafe extern "system" fn(Obj, Obj) = std::mem::transmute(vfn(ctx, vt::CTX_IA_SET_LAYOUT));
            let set_topology: unsafe extern "system" fn(Obj, u32) = std::mem::transmute(vfn(ctx, vt::CTX_IA_SET_TOPOLOGY));
            let set_vs: SetShader = std::mem::transmute(vfn(ctx, vt::CTX_VS_SET_SHADER));
            let set_gs: SetShader = std::mem::transmute(vfn(ctx, vt::CTX_GS_SET_SHADER));
            let set_ps: SetShader = std::mem::transmute(vfn(ctx, vt::CTX_PS_SET_SHADER));
            let draw: unsafe extern "system" fn(Obj, u32, u32) = std::mem::transmute(vfn(ctx, vt::CTX_DRAW));
            // The masks may still be bound as t20 / t23 (the light-adjust draw of the previous frame): not while one is a target.
            set_srv(ctx, SRV_SLOT, 1, &nul);
            set_srv(ctx, RIM_SRV_SLOT, 1, &nul);
            // Target first, no depth-stencil view: the engine's depth buffer is free to be read.
            set_rt(ctx, 1, &t.rtv, nul);
            set_vp(ctx, 1, &Viewport { x: 0.0, y: 0.0, w: w as f32, h: h as f32, min_depth: 0.0, max_depth: 1.0 });
            set_rs(ctx, nul);
            set_depth(ctx, nul, 0);
            set_blend(ctx, nul, [1.0f32; 4].as_ptr(), 0xffff_ffff);
            set_layout(ctx, nul);
            set_topology(ctx, 4 /* TRIANGLELIST */);
            set_vs(ctx, self.occ_vs, std::ptr::null(), 0);
            set_gs(ctx, nul, std::ptr::null(), 0);
            set_ps(ctx, if greater { self.occ_greater } else { self.occ_less }, std::ptr::null(), 0);
            // b0: the tolerance = OCC_TOL + the depth bias of this frame's masked draws (the guard restores the engine's b0).
            let tol = self.tolerance(rim);
            let mut mapped: [usize; 3] = [0; 3];
            let map: unsafe extern "system" fn(Obj, Obj, u32, u32, u32, *mut [usize; 3]) -> i32 = std::mem::transmute(vfn(ctx, vt::CTX_MAP));
            if !self.occ_cb.is_null() && map(ctx, self.occ_cb, 0, 4 /* WRITE_DISCARD */, 0, &mut mapped) >= 0 && mapped[0] != 0 {
                *(mapped[0] as *mut [f32; 4]) = [tol, 0.0, 0.0, 0.0];
                let unmap: unsafe extern "system" fn(Obj, Obj, u32) = std::mem::transmute(vfn(ctx, vt::CTX_UNMAP));
                unmap(ctx, self.occ_cb, 0);
            }
            let set_cb: SetSlots = std::mem::transmute(vfn(ctx, vt::CTX_PS_SET_CBS));
            set_cb(ctx, 0, 1, &self.occ_cb);
            set_srv(ctx, 0, 2, [z.srv, dsrv].as_ptr());
            draw(ctx, 3, 0);
            set_srv(ctx, 0, 2, [nul, nul].as_ptr());
            drop(g); // restores targets, viewport, states, shaders, layout, topology and the engine's t0 / t1
        }
        Occ::Checked
    }
}

impl Drop for Mask {
    fn drop(&mut self) {
        unsafe {
            self.mask.free();
            self.rim.free();
            self.zst.free();
            self.rim_zst.free();
            for (_, r) in &self.depth_srvs {
                if let Ok(s) = r {
                    release(*s);
                }
            }
            for o in [self.fur_depth, self.rim_depth, self.occ_vs, self.occ_less, self.occ_greater, self.stamp_blend[0], self.stamp_blend[1], self.occ_cb, self.fd_cb] {
                release(o);
            }
            release(self.ps_rim);
            release(self.ps);
            for &(_, s) in &self.uv_ps {
                release(s);
            }
            for &(_, _, s) in &self.states {
                release(s);
            }
        }
    }
}

/// The compiled static mask shader (other modules' tests build a `Mask` with it).
#[cfg(test)]
pub(crate) fn mask_blob_for_tests() -> Vec<u8> {
    shaders::compile(HLSL, PS_ENTRY, "ps_5_0").expect("furmask.hlsl")
}

#[cfg(test)]
#[path = "furmask_tests.rs"]
mod tests;
