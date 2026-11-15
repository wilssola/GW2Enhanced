//! Data path: how the highlight boost and the ambient occlusion reach the picture.
//!
//! WHY THIS EXISTS. The first versions drew their own full-screen passes between the engine's passes (a
//! "resolve" before the UI, the AO multiplied into the scene colour). They ran, and the GPU read-back proved
//! the draws landed, but nothing showed on screen: the engine writes the target again before the final pass
//! (measured in the real game: right after our draw the target held our marker, just before the final pass
//! it held the scene again). Timing-based insertion cannot be made reliable without knowing the whole frame.
//!
//! A substituted engine shader cannot be bypassed, because it IS the data flow. So the effects live inside
//! the shaders that every later pass reads or that write the swapchain:
//! - `light_adjust_hdr.hlsl` (replaces `ps#f5cd4832`, the pass that produces the scene colour of the whole
//!   post chain): ambient occlusion factor (from the visibility texture) and the highlight boost;
//! - `final_scrgb.hlsl` (replaces `ps#afdc14cb`, the pass that writes the swapchain): the tone shoulder,
//!   paper white and the peak clamp.
//!
//! The light-adjust shader needs two extra inputs that the game does not bind: the AO visibility texture (t6)
//! and a small constant buffer (b6). They are bound here, at DRAW time (the game's own binding calls come
//! before the draw and could otherwise overwrite the slot), only while the substituted shader is the bound
//! pixel shader (`KIND`, set by the `PSSetShader` hook). Unbound, the shader reads zeros everywhere, which
//! means "no boost, no AO, no SDR clamp": the behaviour the equivalence tests pin.

use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering::Relaxed};

use crate::d3d::{IgnoreScope, Obj, Tex2dDesc, ignoring, release, vfn, vt};
use crate::output::{self, OutputMode};
use crate::{ao, halo, log, recorder, resolve, ssr};

/// `t6` and `b6` of the light-adjust substitute.
pub const SLOT: u32 = 6;
/// `t7` of the light-adjust substitute: the halo texture.
pub const HALO_SLOT: u32 = 7;

pub const KIND_NONE: u8 = 0;
pub const KIND_LIGHT_ADJUST: u8 = 1;
/// The combine pass (`ps#37c5610f`) is bound: its bloom colour is sampled (see `bloomfix.rs`).
pub const KIND_COMBINE: u8 = 2;
/// The bloom-threshold substitute is bound: it gets the tint compensation in `b6` (see `bloomfix.rs`).
pub const KIND_BLOOM: u8 = 3;

/// Which substituted shader is bound as the pixel shader on the game's context.
static KIND: AtomicU8 = AtomicU8::new(KIND_NONE);

pub fn set_kind(k: u8) {
    KIND.store(k, Relaxed);
}

pub fn kind() -> u8 {
    KIND.load(Relaxed)
}

/// What the light-adjust substitute is told (constant buffer b6, two float4).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Params {
    /// Highlight boost gain (1 = none).
    pub boost: f32,
    /// AO strength (0 = none; the visibility texture is only trusted when this is above 0).
    pub ao_strength: f32,
    /// SDR output: clamp like the original shader (no HDR range, no boost).
    pub sdr_clamp: bool,
    /// 0 off, 1 debug view (boosted pixels red, occluded pixels blue), 2 debug fill (left half red, top right black).
    pub debug: u8,
    /// Halo strength (0 = none; the halo texture is only trusted when this is above 0).
    pub halo: f32,
    /// Halo radius slider, informational for the shader (the reach is built into the halo texture).
    pub halo_radius: f32,
    /// Fur strength (0 = off; applies to the pixels of the fur mask `t20`, see `fur_look` in the shader and `furmask.rs`).
    pub fur: f32,
    /// Fur selector debug: the pixels of the fur mask (t20) are painted magenta (`b6[1].w`).
    pub fur_debug: bool,
}

/// Range, default and live value of the fur test slider (`fur_strength` in config.ini).
pub const FUR_RANGE: (f32, f32) = (0.0, 1.0);
pub const FUR_DEFAULT: f32 = 0.0;
static FUR: AtomicU32 = AtomicU32::new(0); // 0.0f32

/// Clamps to 0..=1; NaN gives the default.
pub fn clamp_fur(v: f32) -> f32 {
    if v.is_nan() { FUR_DEFAULT } else { v.clamp(FUR_RANGE.0, FUR_RANGE.1) }
}

pub fn set_fur(v: f32) -> f32 {
    let v = clamp_fur(v);
    FUR.store(v.to_bits(), Relaxed);
    v
}

/// The fur strength as set (config key `fur_strength`, overlay slider), 0..1.
pub fn fur_set() -> f32 {
    f32::from_bits(FUR.load(Relaxed))
}

/// The strength the fur look runs with: `fur_set`, except that a diagnostic fur view (`fur_view` 1..4) keeps it above 0 while the
/// strength is 0. `furmask` builds the fur mask only while this is above 0 (or `fur_debug` is on) and the views need the mask, which
/// is how they work with `fur_strength` 0. The value then is the smallest positive float: the look it switches on changes nothing.
pub fn fur() -> f32 {
    let s = crate::toggles::eff(crate::toggles::Fx::FurLook, fur_set());
    if s <= 0.0 && fur_view() != 0 { f32::MIN_POSITIVE } else { s }
}

// ---- the live fur look: b12 (docs/fur-look.md) ----------------------------------------------------------------------------

/// `b12` of the light-adjust substitute: the live parameters of the fur look.
pub const FUR_LOOK_SLOT_B: u32 = 12;
/// Ranges and defaults of the six parameters (`fur_amp` .. `fur_sss` in config.ini). The defaults are the values that were compiled into the
/// shader before; only `fur_rim` differs: the outline was effectively 1.0 and painted a visible ring around characters, 0.25 makes it subtle.
pub const FUR_AMP_RANGE: (f32, f32) = (0.0, 1.0);
pub const FUR_AMP_DEFAULT: f32 = 0.32;
pub const FUR_SCALE_RANGE: (f32, f32) = (0.25, 4.0);
pub const FUR_SCALE_DEFAULT: f32 = 1.0;
pub const FUR_LENGTH_RANGE: (f32, f32) = (1.0, 12.0);
pub const FUR_LENGTH_DEFAULT: f32 = 5.0;
pub const FUR_DETAIL_RANGE: (f32, f32) = (0.0, 2.0);
pub const FUR_DETAIL_DEFAULT: f32 = 1.0;
pub const FUR_RIM_RANGE: (f32, f32) = (0.0, 2.0);
pub const FUR_RIM_DEFAULT: f32 = 0.25;
pub const FUR_SSS_RANGE: (f32, f32) = (0.0, 2.0);
pub const FUR_SSS_DEFAULT: f32 = 1.0;
/// Highest `fur_view`: 0 off, 1 mesh uv, 2 strand pattern, 3 rim band, 4 strand fade.
pub const FUR_VIEW_MAX: u8 = 4;

/// The six live parameters of the fur look and the diagnostic view.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FurLook {
    /// Brightness modulation of the strands (0..1).
    pub amp: f32,
    /// Multiplies the strand cell sizes (0.25..4).
    pub scale: f32,
    /// Strand length relative to its width (1..12).
    pub length: f32,
    /// Gain of the texture detail (0..2).
    pub detail: f32,
    /// Rim glow and feather of the outline (0..2).
    pub rim: f32,
    /// Warm subsurface term (0..2).
    pub sss: f32,
    /// Diagnostic view painted on the fur mask pixels (0 off .. 4).
    pub view: u8,
}

impl Default for FurLook {
    fn default() -> Self {
        FurLook { amp: FUR_AMP_DEFAULT, scale: FUR_SCALE_DEFAULT, length: FUR_LENGTH_DEFAULT, detail: FUR_DETAIL_DEFAULT, rim: FUR_RIM_DEFAULT, sss: FUR_SSS_DEFAULT, view: 0 }
    }
}

/// Clamps to the range; NaN gives the default (an infinity ends on the nearest end of the range).
fn clamp_or(v: f32, r: (f32, f32), default: f32) -> f32 {
    if v.is_nan() { default } else { v.clamp(r.0, r.1) }
}

impl FurLook {
    /// Every parameter inside its range; NaN gives the default, the view is clamped to 0..=4.
    pub fn clamped(self) -> FurLook {
        FurLook {
            amp: clamp_or(self.amp, FUR_AMP_RANGE, FUR_AMP_DEFAULT),
            scale: clamp_or(self.scale, FUR_SCALE_RANGE, FUR_SCALE_DEFAULT),
            length: clamp_or(self.length, FUR_LENGTH_RANGE, FUR_LENGTH_DEFAULT),
            detail: clamp_or(self.detail, FUR_DETAIL_RANGE, FUR_DETAIL_DEFAULT),
            rim: clamp_or(self.rim, FUR_RIM_RANGE, FUR_RIM_DEFAULT),
            sss: clamp_or(self.sss, FUR_SSS_RANGE, FUR_SSS_DEFAULT),
            view: self.view.min(FUR_VIEW_MAX),
        }
    }
}

static FL_AMP: AtomicU32 = AtomicU32::new(FUR_AMP_DEFAULT.to_bits());
static FL_SCALE: AtomicU32 = AtomicU32::new(FUR_SCALE_DEFAULT.to_bits());
static FL_LENGTH: AtomicU32 = AtomicU32::new(FUR_LENGTH_DEFAULT.to_bits());
static FL_DETAIL: AtomicU32 = AtomicU32::new(FUR_DETAIL_DEFAULT.to_bits());
static FL_RIM: AtomicU32 = AtomicU32::new(FUR_RIM_DEFAULT.to_bits());
static FL_SSS: AtomicU32 = AtomicU32::new(FUR_SSS_DEFAULT.to_bits());
static FL_VIEW: AtomicU8 = AtomicU8::new(0);

fn load(a: &AtomicU32) -> f32 {
    f32::from_bits(a.load(Relaxed))
}

fn store(a: &AtomicU32, v: f32) -> f32 {
    a.store(v.to_bits(), Relaxed);
    v
}

pub fn set_fur_amp(v: f32) -> f32 {
    store(&FL_AMP, clamp_or(v, FUR_AMP_RANGE, FUR_AMP_DEFAULT))
}

pub fn set_fur_scale(v: f32) -> f32 {
    store(&FL_SCALE, clamp_or(v, FUR_SCALE_RANGE, FUR_SCALE_DEFAULT))
}

pub fn set_fur_length(v: f32) -> f32 {
    store(&FL_LENGTH, clamp_or(v, FUR_LENGTH_RANGE, FUR_LENGTH_DEFAULT))
}

pub fn set_fur_detail(v: f32) -> f32 {
    store(&FL_DETAIL, clamp_or(v, FUR_DETAIL_RANGE, FUR_DETAIL_DEFAULT))
}

pub fn set_fur_rim(v: f32) -> f32 {
    store(&FL_RIM, clamp_or(v, FUR_RIM_RANGE, FUR_RIM_DEFAULT))
}

pub fn set_fur_sss(v: f32) -> f32 {
    store(&FL_SSS, clamp_or(v, FUR_SSS_RANGE, FUR_SSS_DEFAULT))
}

/// Sets the diagnostic view (0..=4, clamped). Runtime only: never saved, not applied at start.
pub fn set_fur_view(v: u8) -> u8 {
    let v = v.min(FUR_VIEW_MAX);
    FL_VIEW.store(v, Relaxed);
    v
}

pub fn fur_view() -> u8 {
    FL_VIEW.load(Relaxed)
}

/// Sets the six parameters and the view at once (hot reload, start); returns what was applied.
pub fn set_fur_look(l: &FurLook) -> FurLook {
    let c = l.clamped();
    set_fur_amp(c.amp);
    set_fur_scale(c.scale);
    set_fur_length(c.length);
    set_fur_detail(c.detail);
    set_fur_rim(c.rim);
    set_fur_sss(c.sss);
    set_fur_view(c.view);
    c
}

/// The live fur look.
pub fn fur_look() -> FurLook {
    FurLook { amp: load(&FL_AMP), scale: load(&FL_SCALE), length: load(&FL_LENGTH), detail: load(&FL_DETAIL), rim: load(&FL_RIM), sss: load(&FL_SSS), view: fur_view() }
}

/// `b12`: [0] = (amp, scale, length, detail), [1] = (rim, sss, view, 1). The last component says "these values are live"; the shader reads a
/// zero there (an unbound buffer) as "use the defaults", so an unbound `b12` is the default look. `view_ok` is false while the user has the
/// mod switched off or the fur picker's highlight owns the mask: the diagnostic view is then sent as 0 (the vanilla emulation stays vanilla).
pub fn fur_look_cb_values(l: &FurLook, view_ok: bool) -> [[f32; 4]; 2] {
    let c = l.clamped();
    [[c.amp, c.scale, c.length, c.detail], [c.rim, c.sss, if view_ok { f32::from(c.view) } else { 0.0 }, 1.0]]
}

/// One line for the overlay and the debug capture (`status_furlook=`): the six values and the view.
pub fn fur_look_text() -> String {
    let l = fur_look();
    let view = match l.view {
        0 => "off",
        1 => "mesh uv (red = no uv)",
        2 => "strand pattern",
        3 => "rim band",
        _ => "strand fade",
    };
    format!("fur_amp {}, fur_scale {}, fur_length {}, fur_detail {}, fur_rim {}, fur_sss {}, fur_view {} ({view})", l.amp, l.scale, l.length, l.detail, l.rim, l.sss, l.view)
}

/// `t23` / `b11` of the light-adjust substitute: the rim mask (R8_UNORM, screen size) and the rim constants.
pub const RIM_SLOT_T: u32 = 23;
pub const RIM_SLOT_B: u32 = 11;
/// Range, default and live value of the rim light (`rim_strength`), its width in pixels (`rim_width`) and the subsurface glow (`subsurface_strength`).
pub const RIM_RANGE: (f32, f32) = (0.0, 1.0);
pub const RIM_DEFAULT: f32 = 0.4;
pub const RIM_WIDTH_RANGE: (f32, f32) = (1.0, 8.0);
pub const RIM_WIDTH_DEFAULT: f32 = 3.0;
pub const SUBSURFACE_DEFAULT: f32 = 0.3;
static RIM: AtomicU32 = AtomicU32::new(0x3ecc_cccd); // 0.4f32
static RIM_WIDTH: AtomicU32 = AtomicU32::new(0x4040_0000); // 3.0f32
static SUBSURFACE: AtomicU32 = AtomicU32::new(0x3e99_999a); // 0.3f32

/// Clamps to 0..=1; NaN gives `default`.
fn clamp01(v: f32, default: f32) -> f32 {
    if v.is_nan() { default } else { v.clamp(0.0, 1.0) }
}

pub fn clamp_rim(v: f32) -> f32 {
    clamp01(v, RIM_DEFAULT)
}

/// Sets the rim strength (0 = off) and tells the rim mask whether it is wanted.
pub fn set_rim(v: f32) -> f32 {
    let v = clamp_rim(v);
    RIM.store(v.to_bits(), Relaxed);
    crate::furmask::refresh_rim_consumer();
    v
}

pub fn rim() -> f32 {
    crate::toggles::eff(crate::toggles::Fx::Rim, f32::from_bits(RIM.load(Relaxed)))
}

/// Clamps to 1..=8 pixels; NaN gives the default (3).
pub fn clamp_rim_width(v: f32) -> f32 {
    if v.is_nan() { RIM_WIDTH_DEFAULT } else { v.clamp(RIM_WIDTH_RANGE.0, RIM_WIDTH_RANGE.1) }
}

pub fn set_rim_width(v: f32) -> f32 {
    let v = clamp_rim_width(v);
    RIM_WIDTH.store(v.to_bits(), Relaxed);
    v
}

pub fn rim_width() -> f32 {
    f32::from_bits(RIM_WIDTH.load(Relaxed))
}

pub fn clamp_subsurface(v: f32) -> f32 {
    clamp01(v, SUBSURFACE_DEFAULT)
}

pub fn set_subsurface(v: f32) -> f32 {
    let v = clamp_subsurface(v);
    SUBSURFACE.store(v.to_bits(), Relaxed);
    v
}

pub fn subsurface() -> f32 {
    f32::from_bits(SUBSURFACE.load(Relaxed))
}

/// `b11` of the light-adjust substitute. [0]: x rim strength (0 = the branch is skipped), y width in pixels, z subsurface strength,
/// w debug (1 = the rim mask is painted cyan). [1]: x = view-space size of one pixel per unit of depth at the screen centre
/// (`2 tan(half fov) / height`, for the grazing term), yzw unused. `ready` = the rim mask has pixels this frame and the mod is on.
pub fn rim_cb_values(ready: bool, debug: bool, strength: f32, width: f32, subsurface: f32, screen_h: u32) -> [[f32; 4]; 2] {
    let s = if ready { clamp_rim(strength) } else { 0.0 };
    let k = if screen_h > 0 { 2.0 * ao::TAN_HALF_FOV / screen_h as f32 } else { 0.0 };
    [[s, clamp_rim_width(width), clamp_subsurface(subsurface), if ready && debug { 1.0 } else { 0.0 }], [k, 0.0, 0.0, 0.0]]
}

/// The constant buffer contents. b6[0]: x boost, y AO strength, z SDR clamp, w debug.
/// b6[1]: x halo strength, y halo radius, z fur strength, w fur debug (1 = paint the fur mask magenta).
pub fn cb_values(p: &Params) -> [[f32; 4]; 2] {
    let boost = if p.boost.is_finite() { p.boost.clamp(1.0, MAX_GAIN) } else { 1.0 };
    let ao = if p.ao_strength.is_finite() { p.ao_strength.clamp(0.0, 2.0) } else { 0.0 };
    let halo_s = if p.halo.is_finite() { p.halo.clamp(halo::STRENGTH_RANGE.0, halo::STRENGTH_RANGE.1) } else { 0.0 };
    let halo_r = if p.halo_radius.is_finite() { p.halo_radius.clamp(halo::RADIUS_RANGE.0, halo::RADIUS_RANGE.1) } else { halo::DEFAULT_RADIUS };
    [[boost, ao, if p.sdr_clamp { 1.0 } else { 0.0 }, f32::from(p.debug.min(5))], [halo_s, halo_r, if p.fur.is_finite() { p.fur.clamp(FUR_RANGE.0, FUR_RANGE.1) } else { 0.0 }, if p.fur_debug { 1.0 } else { 0.0 }]]
}

/// Upper limit of the gain the shader is given (the headroom `peak / paper white` can reach 50).
pub const MAX_GAIN: f32 = 64.0;

/// The gain the boost slider (1..4) really applies: the slider is a FRACTION OF THE DISPLAY HEADROOM
/// (`peak / paper white`, linear light, 1.0 = paper white). Slider 1 = no boost; slider 4 = lights at SDR white
/// come out at the display peak. In between the gain is linear in the slider. This is what makes the peak
/// setting matter: a higher peak (or a lower paper white) gives the lights more room, so they get brighter,
/// instead of the peak only capping content that rarely reaches it.
pub fn effective_gain(slider: f32, headroom: f32) -> f32 {
    if !slider.is_finite() || !headroom.is_finite() || headroom <= 1.0 {
        return 1.0;
    }
    let t = ((slider - 1.0) / 3.0).clamp(0.0, 1.0);
    (1.0 + (headroom - 1.0) * t).clamp(1.0, MAX_GAIN)
}

/// Share of the boost a big uniformly bright surface gets when the halo pass says it is not a light source (the shader's
/// `lerp(SURFACE_SHARE, 1, source_share)`; was 0.2, which together with the old 0.5 .. 1.0 expansion range made the slider
/// invisible in the game; 0.3 until the HDR audit: a debug capture had white fur at 0.97 of paper white and nothing
/// above 2x anywhere, "washed, not real HDR"; at 0.6 a sunlit white surface at SDR white reaches 1.5x paper white at slider 2 and
/// 1.75x at the default 2.5 with a headroom of 3.5, lights keep the full gain).
pub const SURFACE_SHARE: f32 = 0.6;

/// Overlay line / debug capture `status_highlight=`: the slider, the gain the shader really gets and the headroom it comes from.
pub fn highlight_status_text() -> String {
    let slider = resolve::highlight();
    let (headroom, _) = output::shoulder_constants(output::paper_white(), output::peak());
    if !crate::gate::user_enabled() {
        return format!("off (mod disabled), slider {slider:.2}");
    }
    if output::mode() != OutputMode::ScRgb {
        return format!("off (SDR output: no headroom), slider {slider:.2}");
    }
    let g = effective_gain(slider, headroom);
    let surf = 1.0 + (g - 1.0) * SURFACE_SHARE;
    format!(
        "slider {slider:.2} -> gain x{g:.2} on lights, x{surf:.2} on big bright surfaces (with the halo on); headroom {headroom:.2} (peak {:.0} / paper white {:.0} nits); expansion from {:.2} to {:.2} of SDR white",
        output::peak(),
        output::paper_white(),
        crate::tonemap::EXPAND_START,
        crate::tonemap::EXPAND_FULL
    )
}

/// The parameters for the current frame. The AO strength counts only when the visibility exists this frame,
/// the halo strength only when the halo texture exists this frame.
pub fn current_params(ao_ready: bool, halo_ready: bool) -> Params {
    // The user switched the mod off: the light-adjust substitute only reproduces the original (clip the input and the
    // output like the 8-bit engine, no AO, no boost, no halo).
    if !crate::gate::user_enabled() {
        return Params { boost: 1.0, ao_strength: 0.0, sdr_clamp: true, debug: 0, halo: 0.0, halo_radius: halo::radius(), fur: 0.0, fur_debug: false };
    }
    let sdr = output::mode() != OutputMode::ScRgb;
    let (headroom, _) = output::shoulder_constants(output::paper_white(), output::peak());
    Params { boost: if sdr { 1.0 } else { effective_gain(resolve::highlight(), headroom) }, ao_strength: if ao_ready { ao::strength() } else { 0.0 }, sdr_clamp: sdr, debug: resolve::debug_level(), halo: if halo_ready { halo::strength() } else { 0.0 }, halo_radius: halo::radius(), fur: if crate::furmask::highlighting() { 0.0 } else { fur() }, fur_debug: crate::furmask::shader_debug() }
}

// ---- device objects ----------------------------------------------------------------------------------

#[repr(C)]
struct SubresourceData {
    mem: *const c_void,
    pitch: u32,
    slice: u32,
}

const R16_FLOAT: u32 = 54;
const R16G16B16A16_FLOAT: u32 = 10;

/// Objects of one device: the constant buffer, a 1x1 "fully open" visibility used when the AO did not run and
/// a 1x1 black halo used when the halo did not run.
pub struct Resources {
    device: usize,
    cb: Obj,
    /// `b11`: the rim constants.
    cb_rim: Obj,
    /// `b12`: the live fur look.
    cb_fur: Obj,
    white_tex: Obj,
    white_srv: Obj,
    black_tex: Obj,
    black_srv: Obj,
}

// Only touched under the mutex from the render thread.
unsafe impl Send for Resources {}

impl Resources {
    /// # Safety
    /// `dev` must be a live `ID3D11Device`.
    pub unsafe fn new(dev: Obj) -> Option<Resources> {
        let _ig = IgnoreScope::new();
        let nul = std::ptr::null_mut();
        let mut r = Resources { device: dev as usize, cb: nul, cb_rim: nul, cb_fur: nul, white_tex: nul, white_srv: nul, black_tex: nul, black_srv: nul };
        unsafe {
            // D3D11_BUFFER_DESC { ByteWidth, Usage = DYNAMIC, BindFlags = CONSTANT_BUFFER, CPUAccessFlags = WRITE, MiscFlags, StructureByteStride }
            let desc: [u32; 6] = [32, 2, 4, 0x10000, 0, 0];
            let create_buffer: unsafe extern "system" fn(Obj, *const [u32; 6], *const c_void, *mut Obj) -> i32 = std::mem::transmute(vfn(dev, vt::DEV_CREATE_BUFFER));
            if create_buffer(dev, &desc, std::ptr::null(), &mut r.cb) < 0
                || r.cb.is_null()
                || create_buffer(dev, &desc, std::ptr::null(), &mut r.cb_rim) < 0
                || r.cb_rim.is_null()
                || create_buffer(dev, &desc, std::ptr::null(), &mut r.cb_fur) < 0
                || r.cb_fur.is_null()
            {
                log!("datapath: could not create the constant buffer");
                return None;
            }
            let td = Tex2dDesc { width: 1, height: 1, mips: 1, array: 1, format: R16_FLOAT, samples: 1, quality: 0, usage: 1 /* IMMUTABLE */, bind: 0x8, cpu: 0, misc: 0 };
            let texel: u16 = 0x3c00; // 1.0 in float16
            let sd = SubresourceData { mem: (&texel as *const u16).cast(), pitch: 2, slice: 0 };
            let create_tex: unsafe extern "system" fn(Obj, *const Tex2dDesc, *const c_void, *mut Obj) -> i32 = std::mem::transmute(vfn(dev, vt::DEV_CREATE_TEXTURE2D));
            if create_tex(dev, &td, (&sd as *const SubresourceData).cast(), &mut r.white_tex) < 0 || r.white_tex.is_null() {
                log!("datapath: could not create the fallback visibility texture");
                return None;
            }
            let sdesc: [u32; 6] = [R16_FLOAT, 4, 0, 1, 0, 0];
            let create_srv: unsafe extern "system" fn(Obj, Obj, *const [u32; 6], *mut Obj) -> i32 = std::mem::transmute(vfn(dev, vt::DEV_CREATE_SRV));
            if create_srv(dev, r.white_tex, &sdesc, &mut r.white_srv) < 0 || r.white_srv.is_null() {
                log!("datapath: could not create the fallback visibility view");
                return None;
            }
            // 1x1 black float16 RGBA: "no halo".
            let td = Tex2dDesc { width: 1, height: 1, mips: 1, array: 1, format: R16G16B16A16_FLOAT, samples: 1, quality: 0, usage: 1 /* IMMUTABLE */, bind: 0x8, cpu: 0, misc: 0 };
            let zero = [0u16; 4];
            let sd = SubresourceData { mem: zero.as_ptr().cast(), pitch: 8, slice: 0 };
            if create_tex(dev, &td, (&sd as *const SubresourceData).cast(), &mut r.black_tex) < 0 || r.black_tex.is_null() {
                log!("datapath: could not create the fallback halo texture");
                return None;
            }
            let sdesc: [u32; 6] = [R16G16B16A16_FLOAT, 4, 0, 1, 0, 0];
            if create_srv(dev, r.black_tex, &sdesc, &mut r.black_srv) < 0 || r.black_srv.is_null() {
                log!("datapath: could not create the fallback halo view");
                return None;
            }
        }
        Some(r)
    }

    /// Writes the parameters and binds `t6` (the visibility, or the open fallback), `t7` (the halo, or the
    /// black fallback) and `b6` on `ctx`.
    ///
    /// # Safety
    /// `ctx` must be a live immediate context of the device these objects belong to; `vis` and `glow` live SRVs of it.
    pub unsafe fn bind(&self, ctx: Obj, vis: Option<Obj>, glow: Option<Obj>, p: &Params) -> bool {
        unsafe {
            let mut mapped: [usize; 3] = [0; 3]; // D3D11_MAPPED_SUBRESOURCE { pData, RowPitch, DepthPitch }
            let map: unsafe extern "system" fn(Obj, Obj, u32, u32, u32, *mut [usize; 3]) -> i32 = std::mem::transmute(vfn(ctx, vt::CTX_MAP));
            if map(ctx, self.cb, 0, 4 /* WRITE_DISCARD */, 0, &mut mapped) < 0 || mapped[0] == 0 {
                return false;
            }
            *(mapped[0] as *mut [[f32; 4]; 2]) = cb_values(p);
            let unmap: unsafe extern "system" fn(Obj, Obj, u32) = std::mem::transmute(vfn(ctx, vt::CTX_UNMAP));
            unmap(ctx, self.cb, 0);
            // b11: the rim constants (all zero = the rim branch is skipped).
            if map(ctx, self.cb_rim, 0, 4, 0, &mut mapped) >= 0 && mapped[0] != 0 {
                *(mapped[0] as *mut [[f32; 4]; 2]) = rim_cb_values(
                    crate::furmask::rim_ready() && crate::gate::user_enabled(),
                    crate::furmask::rim_debug(),
                    rim(),
                    rim_width(),
                    subsurface(),
                    crate::furmask::rim_height(),
                );
                unmap(ctx, self.cb_rim, 0);
                let set_cb11: unsafe extern "system" fn(Obj, u32, u32, *const Obj) = std::mem::transmute(vfn(ctx, vt::CTX_PS_SET_CBS));
                set_cb11(ctx, RIM_SLOT_B, 1, &self.cb_rim);
            }
            // b12: the live fur look (the last component 1 = live; unbound the shader uses its defaults). The diagnostic view is sent as 0 while
            // the user has the mod off or the fur picker's highlight owns the mask.
            if map(ctx, self.cb_fur, 0, 4, 0, &mut mapped) >= 0 && mapped[0] != 0 {
                let view_ok = crate::gate::user_enabled() && !crate::furmask::highlighting();
                *(mapped[0] as *mut [[f32; 4]; 2]) = fur_look_cb_values(&fur_look(), view_ok);
                unmap(ctx, self.cb_fur, 0);
                let set_cb12: unsafe extern "system" fn(Obj, u32, u32, *const Obj) = std::mem::transmute(vfn(ctx, vt::CTX_PS_SET_CBS));
                set_cb12(ctx, FUR_LOOK_SLOT_B, 1, &self.cb_fur);
            }
            type SetSlots = unsafe extern "system" fn(Obj, u32, u32, *const Obj);
            let set_srv: SetSlots = std::mem::transmute(vfn(ctx, vt::CTX_PS_SET_SRVS));
            let set_cb: SetSlots = std::mem::transmute(vfn(ctx, vt::CTX_PS_SET_CBS));
            let srv = vis.unwrap_or(self.white_srv);
            set_srv(ctx, SLOT, 1, &srv);
            let glow_srv = glow.unwrap_or(self.black_srv);
            set_srv(ctx, HALO_SLOT, 1, &glow_srv);
            set_cb(ctx, SLOT, 1, &self.cb);
        }
        true
    }
}

impl Drop for Resources {
    fn drop(&mut self) {
        unsafe {
            for o in [self.black_srv, self.black_tex, self.white_srv, self.white_tex, self.cb_fur, self.cb_rim, self.cb] {
                release(o);
            }
        }
    }
}

static RES: Mutex<Option<Resources>> = Mutex::new(None);
static BINDS: AtomicU32 = AtomicU32::new(0);

/// How many times the light-adjust inputs were bound (overlay: proves the substitute really runs with them).
pub fn bind_count() -> u32 {
    BINDS.load(Relaxed)
}
static FAILED_LOGGED: AtomicBool = AtomicBool::new(false);

/// A draw is about to be issued on `ctx`: when the substituted light-adjust shader is bound on the game's
/// context, give it its inputs. One atomic load for every other draw; never unwinds.
pub fn on_draw(ctx: Obj) {
    let kind = KIND.load(Relaxed);
    if kind == KIND_NONE || ignoring() || recorder::game_context() != ctx as usize {
        return;
    }
    match kind {
        KIND_LIGHT_ADJUST => {
            let _ = std::panic::catch_unwind(|| unsafe { bind_current(ctx) });
        }
        KIND_COMBINE => crate::bloomfix::on_combine_draw(ctx),
        KIND_BLOOM => crate::bloomfix::on_bloom_draw(ctx),
        _ => {}
    }
}

unsafe fn bind_current(ctx: Obj) {
    unsafe {
        let mut dev: Obj = std::ptr::null_mut();
        let get_device: unsafe extern "system" fn(Obj, *mut Obj) = std::mem::transmute(vfn(ctx, vt::CHILD_GET_DEVICE));
        get_device(ctx, &mut dev);
        if dev.is_null() {
            return;
        }
        // The halo is computed here, once per frame, from the scene colour bound at slot 0 of this very draw
        // (its passes restore the pipeline state). Strength 0 or any failure gives the black fallback.
        let glow = halo::prepare(ctx, dev);
        let vis = ao::visibility_srv(dev as usize);
        let p = current_params(vis.is_some(), glow.is_some());
        let ok = 'bind: {
            let Ok(mut g) = RES.try_lock() else { break 'bind false };
            if g.as_ref().is_some_and(|r| r.device != dev as usize) {
                *g = None;
            }
            if g.is_none() {
                *g = Resources::new(dev);
            }
            match g.as_ref() {
                Some(r) => r.bind(ctx, vis, glow, &p),
                None => false,
            }
        };
        if ok {
            // Screen-space reflections: computes the texture once per frame and binds t8 / b7 (see ssr.rs).
            ssr::on_light_adjust(ctx, dev);
            // Sun shafts: computes the texture once per frame and binds t9 / b8 (see shafts.rs).
            crate::shafts::on_light_adjust(ctx, dev);
            // Fur mask (t20): the pixels of the draws the fur selector picked this frame (see furmask.rs).
            crate::furmask::on_light_adjust(ctx);
            // Shell fur (t24 / b13): the recorded fur draws drawn again as shells, now that the depth is final (see shellfur.rs).
            crate::shellfur::on_light_adjust(ctx);
            // Fog colour and reference depth for the AO release: binds t3 / t4 (see fog.rs). The rim light needs them too.
            crate::fog::on_light_adjust(ctx, dev, vis.is_some() || crate::contact::needs_fog() || crate::furmask::rim_ready());
            // Contact shadows (t22) and the shared b10 of the aerial perspective: see contact.rs / aerial.rs.
            crate::contact::on_light_adjust(ctx, dev);
            // Auto exposure and local tone mapping: computes the state / base layer once per frame and binds t5 / t21 / b9 (see exposure.rs).
            crate::exposure::on_light_adjust(ctx, dev);
            // One-off depth diagnostic (overlay button "Log depth stats"; see depthstats.rs).
            crate::depthstats::on_light_adjust(ctx, dev);
            // Debug capture: the engine's linear depth of this frame for `status_depth=` (only while a dev shot waits for it; see debugshot.rs).
            crate::debugshot::on_light_adjust(ctx, dev);
        }
        release(dev);
        if ok {
            let n = BINDS.fetch_add(1, Relaxed) + 1;
            if n == 1 || n % 600 == 0 {
                log!("datapath: light-adjust inputs bound (#{n}): boost {:.2}, ao strength {:.2} (visibility {}), halo {:.2} ({}), fur test {:.2}, sdr clamp {}, debug {}", p.boost, p.ao_strength, if vis.is_some() { "ready" } else { "not ready" }, p.halo, if glow.is_some() { "ready" } else { "not ready" }, p.fur, p.sdr_clamp, p.debug);
            }
        } else if !FAILED_LOGGED.swap(true, Relaxed) {
            log!("datapath: could not bind the light-adjust inputs; the shader runs with its defaults (no boost, no AO)");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants_are_clamped_and_ordered() {
        let c = cb_values(&Params { boost: 900.0, ao_strength: 7.0, sdr_clamp: true, debug: 9, halo: 9.0, halo_radius: 9.0, fur: 0.0, fur_debug: false });
        assert_eq!(c, [[MAX_GAIN, 2.0, 1.0, 5.0], [2.0, 2.0, 0.0, 0.0]]);
        let c = cb_values(&Params { boost: 0.1, ao_strength: -1.0, sdr_clamp: false, debug: 0, halo: -1.0, halo_radius: 0.0, fur: 0.0, fur_debug: false });
        assert_eq!(c, [[1.0, 0.0, 0.0, 0.0], [0.0, 0.5, 0.0, 0.0]]);
        // Levels 4 (sun shafts) and 5 (SSR reflectivity) pass through unchanged.
        assert_eq!(cb_values(&Params { boost: 1.0, ao_strength: 0.0, sdr_clamp: false, debug: 4, halo: 0.0, halo_radius: 1.0, fur: 0.0, fur_debug: false })[0][3], 4.0);
        assert_eq!(cb_values(&Params { boost: 1.0, ao_strength: 0.0, sdr_clamp: false, debug: 5, halo: 0.0, halo_radius: 1.0, fur: 0.0, fur_debug: false })[0][3], 5.0);
        let c = cb_values(&Params { boost: f32::NAN, ao_strength: f32::INFINITY, sdr_clamp: false, debug: 1, halo: f32::NAN, halo_radius: f32::NAN, fur: 0.0, fur_debug: false });
        assert_eq!(c, [[1.0, 0.0, 0.0, 1.0], [0.0, 1.0, 0.0, 0.0]]);
    }

    #[test]
    fn rim_constants_are_clamped_gated_by_ready_and_ordered() {
        let c = rim_cb_values(true, false, 0.4, 3.0, 0.3, 1080);
        assert_eq!(c[0], [0.4, 3.0, 0.3, 0.0]);
        assert!((c[1][0] - 2.0 * ao::TAN_HALF_FOV / 1080.0).abs() < 1e-9);
        // Not ready (no rim pixels this frame / mod off): strength and debug are zero, so the shader skips the branch.
        let c = rim_cb_values(false, true, 0.4, 3.0, 0.3, 1080);
        assert_eq!((c[0][0], c[0][3]), (0.0, 0.0));
        let c = rim_cb_values(true, true, 9.0, 99.0, -3.0, 0);
        assert_eq!(c[0], [1.0, 8.0, 0.0, 1.0]);
        assert_eq!(c[1][0], 0.0);
        let c = rim_cb_values(true, false, f32::NAN, f32::NAN, f32::NAN, 720);
        assert_eq!(c[0][..3], [RIM_DEFAULT, RIM_WIDTH_DEFAULT, SUBSURFACE_DEFAULT]);
        assert_eq!((f32::from_bits(0x3ecc_cccd), f32::from_bits(0x4040_0000), f32::from_bits(0x3e99_999a)), (RIM_DEFAULT, RIM_WIDTH_DEFAULT, SUBSURFACE_DEFAULT));
        assert_eq!((RIM_SLOT_T, RIM_SLOT_B), (23, 11));
        assert_eq!((clamp_rim(2.0), clamp_rim(-1.0), clamp_rim_width(0.0), clamp_rim_width(20.0), clamp_subsurface(f32::NAN)), (1.0, 0.0, 1.0, 8.0, SUBSURFACE_DEFAULT));
    }

    #[test]
    fn fur_constant_is_clamped_and_goes_to_b6_1_z() {
        let p = |fur: f32| Params { boost: 1.0, ao_strength: 0.0, sdr_clamp: false, debug: 0, halo: 0.0, halo_radius: 1.0, fur, fur_debug: false };
        assert_eq!(cb_values(&p(0.35))[1], [0.0, 1.0, 0.35, 0.0]);
        assert_eq!(cb_values(&p(9.0))[1][2], 1.0);
        assert_eq!(cb_values(&p(-2.0))[1][2], 0.0);
        assert_eq!(cb_values(&p(f32::NAN))[1][2], 0.0);
        assert_eq!(cb_values(&p(f32::INFINITY))[1][2], 0.0);
        // The debug switch goes to b6[1].w.
        assert_eq!(cb_values(&Params { fur_debug: true, ..p(0.0) })[1], [0.0, 1.0, 0.0, 1.0]);
        // Range, default and setter (the overlay slider uses them).
        assert_eq!((FUR_RANGE, FUR_DEFAULT), ((0.0, 1.0), 0.0));
        assert_eq!(f32::from_bits(0), FUR_DEFAULT);
        assert_eq!(clamp_fur(f32::NAN), FUR_DEFAULT);
        assert_eq!(clamp_fur(5.0), 1.0);
        assert_eq!(clamp_fur(-1.0), 0.0);
        let _g = crate::testlock::lock();
        let keep = fur_set();
        assert_eq!(set_fur(0.6), 0.6);
        assert_eq!(fur(), 0.6);
        assert_eq!(fur_set(), 0.6);
        set_fur(keep);
    }

    #[test]
    fn fur_look_constants_are_clamped_ordered_and_flagged_live() {
        // The defaults are what the shader had compiled in (rim: the new subtle default), the slot is b12, nothing else uses it.
        assert_eq!(FUR_LOOK_SLOT_B, 12);
        assert_eq!([SLOT, crate::datapath::RIM_SLOT_B, FUR_LOOK_SLOT_B].iter().collect::<std::collections::BTreeSet<_>>().len(), 3);
        let d = FurLook::default();
        assert_eq!((d.amp, d.scale, d.length, d.detail, d.rim, d.sss, d.view), (0.32, 1.0, 5.0, 1.0, 0.25, 1.0, 0));
        // [0] = amp, scale, length, detail; [1] = rim, sss, view, 1 (live).
        assert_eq!(fur_look_cb_values(&d, true), [[0.32, 1.0, 5.0, 1.0], [0.25, 1.0, 0.0, 1.0]]);
        let l = FurLook { amp: 0.5, scale: 2.0, length: 8.0, detail: 1.5, rim: 1.0, sss: 0.5, view: 3 };
        assert_eq!(fur_look_cb_values(&l, true), [[0.5, 2.0, 8.0, 1.5], [1.0, 0.5, 3.0, 1.0]]);
        // The view is sent as 0 while the mod is off / the picker highlight owns the mask; the values and the live flag are not touched.
        assert_eq!(fur_look_cb_values(&l, false), [[0.5, 2.0, 8.0, 1.5], [1.0, 0.5, 0.0, 1.0]]);
        // Out of range: clamped to the ends.
        let big = FurLook { amp: 9.0, scale: 99.0, length: 99.0, detail: 9.0, rim: 9.0, sss: 9.0, view: 200 };
        assert_eq!(fur_look_cb_values(&big, true), [[1.0, 4.0, 12.0, 2.0], [2.0, 2.0, 4.0, 1.0]]);
        let small = FurLook { amp: -1.0, scale: 0.0, length: 0.0, detail: -1.0, rim: -1.0, sss: -1.0, view: 0 };
        assert_eq!(fur_look_cb_values(&small, true), [[0.0, 0.25, 1.0, 0.0], [0.0, 0.0, 0.0, 1.0]]);
        // Infinity ends on the range end, NaN gives the default (never a NaN in the buffer).
        let inf = FurLook { amp: f32::INFINITY, scale: f32::NEG_INFINITY, ..d };
        assert_eq!(fur_look_cb_values(&inf, true)[0][..2], [1.0, 0.25]);
        let nan = FurLook { amp: f32::NAN, scale: f32::NAN, length: f32::NAN, detail: f32::NAN, rim: f32::NAN, sss: f32::NAN, view: 1 };
        assert_eq!(fur_look_cb_values(&nan, true), [[0.32, 1.0, 5.0, 1.0], [0.25, 1.0, 1.0, 1.0]]);
        assert!(fur_look_cb_values(&nan, true).iter().flatten().all(|v| v.is_finite()));
        assert_eq!(FUR_VIEW_MAX, 4);
    }

    #[test]
    fn fur_look_setters_clamp_and_the_view_keeps_the_mask_wanted() {
        let _g = crate::testlock::lock();
        let (keep, keep_fur) = (fur_look(), fur_set());
        assert_eq!(set_fur_amp(0.5), 0.5);
        assert_eq!(set_fur_amp(7.0), 1.0);
        assert_eq!(set_fur_amp(f32::NAN), FUR_AMP_DEFAULT);
        assert_eq!(set_fur_scale(0.0), 0.25);
        assert_eq!(set_fur_length(30.0), 12.0);
        assert_eq!(set_fur_detail(-3.0), 0.0);
        assert_eq!(set_fur_rim(5.0), 2.0);
        assert_eq!(set_fur_sss(f32::NAN), FUR_SSS_DEFAULT);
        assert_eq!(set_fur_view(9), 4);
        assert_eq!(fur_view(), 4);
        assert_eq!(set_fur_view(0), 0);
        assert_eq!(set_fur_look(&FurLook { amp: 0.7, scale: 3.0, length: 2.0, detail: 0.5, rim: 1.5, sss: 0.25, view: 2 }), FurLook { amp: 0.7, scale: 3.0, length: 2.0, detail: 0.5, rim: 1.5, sss: 0.25, view: 2 });
        assert_eq!(fur_look(), FurLook { amp: 0.7, scale: 3.0, length: 2.0, detail: 0.5, rim: 1.5, sss: 0.25, view: 2 });
        assert_eq!(set_fur_look(&FurLook { amp: 9.0, view: 77, ..FurLook::default() }).amp, 1.0);
        assert_eq!(fur_look().view, 4);
        // `fur()` is what furmask asks ("above 0 = build the mask"): a view needs the mask even with strength 0, and nothing else changes.
        set_fur(0.0);
        set_fur_view(0);
        assert_eq!(fur(), 0.0, "no view, no strength: no mask");
        set_fur_view(3);
        assert!(fur() > 0.0 && fur() < 1e-30, "a view keeps the mask wanted without a visible strength: {}", fur());
        assert_eq!(fur_set(), 0.0, "the strength as set is untouched");
        set_fur(0.4);
        assert_eq!(fur(), 0.4, "with a strength the view does not change it");
        set_fur_view(0);
        // The text names every value.
        set_fur_look(&FurLook::default());
        let t = fur_look_text();
        for k in ["fur_amp 0.32", "fur_scale 1", "fur_length 5", "fur_detail 1", "fur_rim 0.25", "fur_sss 1", "fur_view 0 (off)"] {
            assert!(t.contains(k), "{k} missing in {t}");
        }
        set_fur_look(&keep);
        set_fur(keep_fur);
    }

    #[test]
    fn effective_gain_is_a_fraction_of_the_headroom() {
        // Slider 1: nothing. Slider 4: SDR white reaches the display peak (the headroom).
        assert_eq!(effective_gain(1.0, 8.0), 1.0);
        assert!((effective_gain(4.0, 8.0) - 8.0).abs() < 1e-5);
        assert!((effective_gain(2.5, 5.0) - 3.0).abs() < 1e-5); // halfway: 1 + 4 * 0.5
        // The peak matters: the same slider gives a larger gain with more headroom.
        assert!(effective_gain(2.5, 8.0) > effective_gain(2.5, 4.0));
        // Monotonic in the slider.
        let mut prev = 0.0;
        for i in 0..=30 {
            let g = effective_gain(1.0 + i as f32 * 0.1, 6.0);
            assert!(g >= prev);
            prev = g;
        }
        // No headroom or garbage: no boost.
        assert_eq!(effective_gain(4.0, 1.0), 1.0);
        assert_eq!(effective_gain(f32::NAN, 8.0), 1.0);
        assert_eq!(effective_gain(4.0, f32::INFINITY), 1.0);
        // Out-of-range sliders are clamped.
        assert!((effective_gain(9.0, 8.0) - 8.0).abs() < 1e-5);
        assert_eq!(effective_gain(0.0, 8.0), 1.0);
    }

    #[test]
    fn kind_roundtrip() {
        let keep = kind();
        set_kind(KIND_LIGHT_ADJUST);
        assert_eq!(kind(), KIND_LIGHT_ADJUST);
        set_kind(KIND_NONE);
        assert_eq!(kind(), KIND_NONE);
        set_kind(keep);
    }

    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::*;
    use windows::Win32::Graphics::Direct3D11::*;
    use windows::core::Interface;

    #[test]
    fn bind_sets_t6_and_b6_and_writes_the_parameters() {
        let mut dev = None;
        let mut ctx = None;
        let r = unsafe { D3D11CreateDevice(None::<&windows::Win32::Graphics::Dxgi::IDXGIAdapter>, D3D_DRIVER_TYPE_WARP, HMODULE::default(), D3D11_CREATE_DEVICE_FLAG(0), None, D3D11_SDK_VERSION, Some(&mut dev), None, Some(&mut ctx)) };
        let (Ok(()), Some(dev), Some(ctx)) = (r, dev, ctx) else {
            println!("skipped: WARP device creation failed");
            return;
        };
        unsafe {
            let r = Resources::new(dev.as_raw()).expect("resources");
            let p = Params { boost: 2.5, ao_strength: 1.5, sdr_clamp: false, debug: 1, halo: 0.75, halo_radius: 1.5, fur: 0.0, fur_debug: false };
            assert!(r.bind(ctx.as_raw(), None, None, &p), "bind failed");
            let mut srv = [None];
            ctx.PSGetShaderResources(SLOT, Some(&mut srv));
            let srv = srv[0].take().expect("t6 is not bound");
            let mut d = D3D11_SHADER_RESOURCE_VIEW_DESC::default();
            srv.GetDesc(&mut d);
            assert_eq!(d.Format.0 as u32, R16_FLOAT, "the fallback visibility is R16_FLOAT");
            let mut cbs = [None];
            ctx.PSGetConstantBuffers(SLOT, Some(&mut cbs));
            let cb = cbs[0].take().expect("b6 is not bound");
            let mut bd = D3D11_BUFFER_DESC::default();
            cb.GetDesc(&mut bd);
            assert_eq!(bd.ByteWidth, 32);
            // t7 is the black halo fallback.
            let mut g = [None];
            ctx.PSGetShaderResources(HALO_SLOT, Some(&mut g));
            let g = g[0].take().expect("t7 is not bound");
            let mut gd = D3D11_SHADER_RESOURCE_VIEW_DESC::default();
            g.GetDesc(&mut gd);
            assert_eq!(gd.Format.0 as u32, R16G16B16A16_FLOAT, "the fallback halo is float16 RGBA");

            // Binding again with another visibility view replaces t6 and rewrites the values.
            assert!(r.bind(ctx.as_raw(), Some(srv.as_raw()), Some(g.as_raw()), &Params { boost: 1.0, ao_strength: 0.0, sdr_clamp: true, debug: 0, halo: 0.0, halo_radius: 1.0, fur: 0.0, fur_debug: false }));
            let mut again = [None];
            ctx.PSGetShaderResources(SLOT, Some(&mut again));
            assert!(again[0].is_some());
            // The values: read the dynamic buffer back through a staging copy.
            let sd = D3D11_BUFFER_DESC { ByteWidth: 32, Usage: D3D11_USAGE_STAGING, BindFlags: 0, CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32, ..Default::default() };
            let mut st = None;
            dev.CreateBuffer(&sd, None, Some(&mut st)).unwrap();
            let st = st.unwrap();
            ctx.CopyResource(&st, &cb);
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            ctx.Map(&st, 0, D3D11_MAP_READ, 0, Some(&mut m)).unwrap();
            let v = *(m.pData as *const [[f32; 4]; 2]);
            ctx.Unmap(&st, 0);
            assert_eq!(v, [[1.0, 0.0, 1.0, 0.0], [0.0, 1.0, 0.0, 0.0]]);
        }
    }

    #[test]
    fn bind_sets_b12_to_the_live_fur_look_flagged_live() {
        let _g = crate::testlock::lock();
        let mut dev = None;
        let mut ctx = None;
        let r = unsafe { D3D11CreateDevice(None::<&windows::Win32::Graphics::Dxgi::IDXGIAdapter>, D3D_DRIVER_TYPE_WARP, HMODULE::default(), D3D11_CREATE_DEVICE_FLAG(0), None, D3D11_SDK_VERSION, Some(&mut dev), None, Some(&mut ctx)) };
        let (Ok(()), Some(dev), Some(ctx)) = (r, dev, ctx) else {
            println!("skipped: WARP device creation failed");
            return;
        };
        let keep = fur_look();
        set_fur_look(&FurLook { amp: 0.41, scale: 2.5, length: 9.0, detail: 1.75, rim: 0.6, sss: 0.3, view: 0 });
        unsafe {
            let r = Resources::new(dev.as_raw()).expect("resources");
            let p = Params { boost: 1.0, ao_strength: 0.0, sdr_clamp: false, debug: 0, halo: 0.0, halo_radius: 1.0, fur: 0.0, fur_debug: false };
            assert!(r.bind(ctx.as_raw(), None, None, &p), "bind failed");
            let mut cbs = [None];
            ctx.PSGetConstantBuffers(FUR_LOOK_SLOT_B, Some(&mut cbs));
            let cb = cbs[0].take().expect("b12 is not bound");
            let mut bd = D3D11_BUFFER_DESC::default();
            cb.GetDesc(&mut bd);
            assert_eq!(bd.ByteWidth, 32);
            let sd = D3D11_BUFFER_DESC { ByteWidth: 32, Usage: D3D11_USAGE_STAGING, BindFlags: 0, CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32, ..Default::default() };
            let mut st = None;
            dev.CreateBuffer(&sd, None, Some(&mut st)).unwrap();
            let st = st.unwrap();
            ctx.CopyResource(&st, &cb);
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            ctx.Map(&st, 0, D3D11_MAP_READ, 0, Some(&mut m)).unwrap();
            let v = *(m.pData as *const [[f32; 4]; 2]);
            ctx.Unmap(&st, 0);
            assert_eq!(v[0], [0.41, 2.5, 9.0, 1.75]);
            assert_eq!((v[1][0], v[1][1], v[1][3]), (0.6, 0.3, 1.0), "rim, sss and the live flag");
            // The view goes through when the mod is on and no picker highlight is up; the other buffers are untouched by it.
            set_fur_view(2);
            assert!(r.bind(ctx.as_raw(), None, None, &p));
            ctx.CopyResource(&st, &cb);
            ctx.Map(&st, 0, D3D11_MAP_READ, 0, Some(&mut m)).unwrap();
            let v2 = *(m.pData as *const [[f32; 4]; 2]);
            ctx.Unmap(&st, 0);
            assert_eq!(v2[0], v[0]);
            assert!(v2[1][2] == 2.0 || v2[1][2] == 0.0, "the view is 2 (or 0 if another test holds the mod off / a highlight): {}", v2[1][2]);
        }
        set_fur_look(&keep);
    }
}
