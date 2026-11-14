//! Auto exposure (eye adaptation) and local tone mapping. See `docs/exposure.md`.
//!
//! Where: once per frame, right before the light-adjust draw (`datapath::bind_current` -> `on_light_adjust`), from the
//! scene colour bound at PS slot 0 of that draw (the same input the halo reads, so the measurement is taken BEFORE the
//! exposure is applied: no feedback loop) and, when the AO's depth capture exists, the engine's linear depth (the sky is
//! ignored). Our passes write only OUR OWN targets (`exposure.hlsl`):
//!
//! 1. scene -> L0 (1/4), L1 (1/8), L2 (1/16) luminance pyramid (valid-weighted, sky excluded);
//! 2. autoexp: L2 -> 64-bin log histogram (centre weighted) -> 1x1 state (EV, smoothed with separate up / down time
//!    constants, ping-pong, frame time from the CPU). No read-back: the state texture is bound as `t5`;
//! 3. local tone mapping: L2 -> edge-aware (bilateral) blur H + V -> the BASE layer, bound as `t21`.
//!
//! The light-adjust substitute applies both (`exposure_tone` in `light_adjust_hdr.hlsl`) to the linear scene light before
//! its remap, so the UI is never touched. `b9` carries the switches. Unbound / strength 0 reads zero = bit-identical no-op.
//!
//! Plain decisions (`soft_limit`, `target_ev`, `step_ev`, `gain_scale`, `local_scale`, ...) are host-testable Rust mirrors of
//! the HLSL; `Exposure` talks to D3D through raw vtables like `fog.rs` / `halo.rs` and is exercised on WARP by
//! `exposure_tests.rs`.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};

use crate::d3d::{IgnoreScope, Obj, StateGuard, Tex2dDesc, Viewport, release, vfn, vt};
use crate::passes::FMT_R16G16B16A16_FLOAT;
use crate::{ao, log, shaders};

pub const HLSL: &str = include_str!("../shaders/exposure.hlsl");
pub const VS_ENTRY: &str = "vs_main";
pub const PS_LUM: &str = "ps_lum";
pub const PS_DOWN_LIN: &str = "ps_down_lin";
pub const PS_DOWN_LOG: &str = "ps_down_log";
pub const PS_BLUR: &str = "ps_blur";
pub const PS_HIST: &str = "ps_hist";
pub const PS_STATE: &str = "ps_state";

/// Slots of the light-adjust substitute (see docs/data-path.md): the 1x1 state, the base layer, the switches.
pub const STATE_SLOT: u32 = 5;
pub const BASE_SLOT: u32 = 21;
pub const CB_SLOT: u32 = 9;

// ---- settings ------------------------------------------------------------------------------------------

pub const STRENGTH_RANGE: (f32, f32) = (0.0, 1.0);
pub const TARGET_RANGE: (f32, f32) = (0.05, 0.6);
pub const RANGE_RANGE: (f32, f32) = (0.0, 2.0);
pub const ADAPT_RANGE: (f32, f32) = (0.05, 20.0);
pub const LOCAL_RANGE: (f32, f32) = (0.0, 1.0);
pub const DEFAULT_STRENGTH: f32 = 0.5;
pub const DEFAULT_TARGET: f32 = 0.20;
pub const DEFAULT_RANGE: f32 = 0.7;
/// Time constant (s) when the scene gets BRIGHTER (the exposure goes down): the eye is fast that way.
pub const DEFAULT_ADAPT_UP: f32 = 0.6;
/// Time constant (s) when the scene gets DARKER (the exposure goes up): the eye is slow that way.
pub const DEFAULT_ADAPT_DOWN: f32 = 1.8;
pub const DEFAULT_LOCAL: f32 = 0.2; // was 0.4 until the HDR audit (it dimmed white fur / armour by ~11 %, docs/exposure.md)
/// Brightening is never more than this (EV), whatever `autoexp_range` says: no brighter dark areas for a gameplay advantage.
pub const BRIGHTEN_CAP_EV: f32 = 1.0;
/// Share of the luminance weight (the brightest) that is left out of the measurement.
pub const TOP_REJECT: f32 = 0.001;
/// Weight of the centre of the picture in the measurement (the border counts 1 - this).
pub const CENTER_WEIGHT: f32 = 0.65;
/// Range sigma (EV) of the edge-aware blur: a luminance step of 2 EV or more is not averaged across.
pub const BLUR_SIGMA_EV: f32 = 0.5;
/// Base luminance (linear, 1 = SDR white, after the exposure) above which the local tone mapping compresses.
pub const LOCAL_KNEE: f32 = 0.6;
/// Compression exponent at `localtone_strength` 1: the base layer above the knee is scaled by (Lbase / knee)^-k.
pub const LOCAL_GAIN: f32 = 0.6;
/// Passes: frames longer than this (s) reset the smoothing (a loading screen or a pause), the others are clamped.
pub const RESET_DT: f32 = 1.0;
pub const MAX_DT: f32 = 0.25;

static STRENGTH: AtomicU32 = AtomicU32::new(0x3f00_0000); // 0.5
static TARGET: AtomicU32 = AtomicU32::new(0x3e4c_cccd); // 0.2
static RANGE: AtomicU32 = AtomicU32::new(0x3f33_3333); // 0.7
static ADAPT_UP: AtomicU32 = AtomicU32::new(0x3f19_999a); // 0.6
static ADAPT_DOWN: AtomicU32 = AtomicU32::new(0x3fe6_6666); // 1.8
static LOCAL: AtomicU32 = AtomicU32::new(0x3e4c_cccd); // 0.2

fn clamp_to(v: f32, r: (f32, f32), default: f32) -> f32 {
    if v.is_nan() { default } else { v.clamp(r.0, r.1) }
}

pub fn clamp_strength(v: f32) -> f32 {
    clamp_to(v, STRENGTH_RANGE, DEFAULT_STRENGTH)
}
pub fn clamp_target(v: f32) -> f32 {
    clamp_to(v, TARGET_RANGE, DEFAULT_TARGET)
}
pub fn clamp_range(v: f32) -> f32 {
    clamp_to(v, RANGE_RANGE, DEFAULT_RANGE)
}
pub fn clamp_adapt(v: f32, default: f32) -> f32 {
    clamp_to(v, ADAPT_RANGE, default)
}
pub fn clamp_local(v: f32) -> f32 {
    clamp_to(v, LOCAL_RANGE, DEFAULT_LOCAL)
}

fn load(a: &AtomicU32) -> f32 {
    f32::from_bits(a.load(Relaxed))
}

pub fn strength() -> f32 {
    crate::toggles::eff(crate::toggles::Fx::Exposure, load(&STRENGTH))
}
pub fn target() -> f32 {
    load(&TARGET)
}
pub fn range() -> f32 {
    load(&RANGE)
}
pub fn adapt_up() -> f32 {
    load(&ADAPT_UP)
}
pub fn adapt_down() -> f32 {
    load(&ADAPT_DOWN)
}
pub fn local_strength() -> f32 {
    crate::toggles::eff(crate::toggles::Fx::LocalTone, load(&LOCAL))
}

/// Sets the strength (clamped; returns the applied value) and tells the AO's depth capture whether the sky exclusion needs it.
pub fn set_strength(v: f32) -> f32 {
    let v = clamp_strength(v);
    STRENGTH.store(v.to_bits(), Relaxed);
    ao::set_depth_consumer(ao::DEPTH_FOR_EXPOSURE, v > 0.0);
    v
}
pub fn set_target(v: f32) -> f32 {
    let v = clamp_target(v);
    TARGET.store(v.to_bits(), Relaxed);
    v
}
pub fn set_range(v: f32) -> f32 {
    let v = clamp_range(v);
    RANGE.store(v.to_bits(), Relaxed);
    v
}
pub fn set_adapt_up(v: f32) -> f32 {
    let v = clamp_adapt(v, DEFAULT_ADAPT_UP);
    ADAPT_UP.store(v.to_bits(), Relaxed);
    v
}
pub fn set_adapt_down(v: f32) -> f32 {
    let v = clamp_adapt(v, DEFAULT_ADAPT_DOWN);
    ADAPT_DOWN.store(v.to_bits(), Relaxed);
    v
}
pub fn set_local_strength(v: f32) -> f32 {
    let v = clamp_local(v);
    LOCAL.store(v.to_bits(), Relaxed);
    v
}

static ENABLED: AtomicBool = AtomicBool::new(false);
static FAILED: AtomicBool = AtomicBool::new(false);
static DONE: AtomicBool = AtomicBool::new(false);
static CUR: AtomicU8 = AtomicU8::new(0);
static LAST: AtomicU8 = AtomicU8::new(0);
const SEEN_DRAW: u8 = 1;
const SEEN_APPLIED: u8 = 2;
const SEEN_BAD_SCENE: u8 = 4;
const SEEN_NO_DEPTH: u8 = 8;
const SEEN_SDR: u8 = 16;

/// Turned on and off at the frame boundary with the HDR resolve (the gate); the user's Enabled switch is checked too.
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Relaxed) && !FAILED.load(Relaxed) && crate::gate::user_enabled()
}

pub fn wants_autoexp() -> bool {
    enabled() && strength() > 0.0
}

pub fn wants_local() -> bool {
    enabled() && local_strength() > 0.0
}

/// A new frame starts: remembers what the last one did (overlay) and allows the next run.
pub fn frame_begin() {
    DONE.store(false, Relaxed);
    LAST.store(CUR.swap(0, Relaxed), Relaxed);
}

// ---- Rust mirrors of the HLSL (reference of the tests) -----------------------------------------------------

/// Smooth limiter: identity up to 60 % of the range `r`, then a tanh roll-off reaching `r` at infinity (mirrors `soft_limit`).
#[allow(dead_code)] // Rust mirror of the HLSL, the reference of the tests
pub fn soft_limit(x: f32, r: f32) -> f32 {
    if r <= 1e-4 || x.is_nan() {
        return 0.0;
    }
    let a = 0.6 * r;
    let ax = x.abs();
    if ax <= a {
        return x;
    }
    x.signum() * (a + (r - a) * ((ax - a) / (r - a)).tanh())
}

/// The ranges the shader gets: (darkening, brightening) in EV. Brightening is capped (`BRIGHTEN_CAP_EV`).
pub fn ev_limits(range: f32) -> (f32, f32) {
    let r = clamp_range(range);
    (r, r.min(BRIGHTEN_CAP_EV))
}

/// The exposure compensation (EV) a scene with log2 luminance `avg_log2` asks for (mirrors `ps_state`, before smoothing).
#[allow(dead_code)] // Rust mirror of the HLSL, the reference of the tests
pub fn target_ev(avg_log2: f32, target: f32, strength: f32, range: f32) -> f32 {
    let (down, up) = ev_limits(range);
    let raw = target.max(1e-4).log2() - avg_log2;
    let x = raw * strength;
    if x > 0.0 { soft_limit(x, up) } else { soft_limit(x, down) }
}

/// One smoothing step (mirrors `ps_state`): exponential, with `up_tau` when the scene gets brighter (the EV goes down)
/// and `down_tau` when it gets darker.
#[allow(dead_code)] // Rust mirror of the HLSL, the reference of the tests
pub fn step_ev(prev: f32, ev: f32, dt: f32, up_tau: f32, down_tau: f32) -> f32 {
    let tau = if ev < prev { up_tau } else { down_tau };
    let alpha = 1.0 - (-(dt.max(0.0)) / tau.max(1e-3)).exp();
    prev + (ev - prev) * alpha
}

/// The frame time to use and whether the smoothing resets, from the seconds since the last run (None = first run).
pub fn frame_dt(elapsed: Option<f32>) -> (f32, bool) {
    match elapsed {
        Some(e) if e.is_finite() && e >= 0.0 && e <= RESET_DT => (e.min(MAX_DT), false),
        _ => (MAX_DT, true),
    }
}

/// Result of the exposure on the largest linear channel `m`: darkening scales everything, brightening adds
/// `(E - 1) * h(m)` with `h(m) = m / (1 + (m/k)^4)^(1/4)` (about m below k, saturating at k): monotonic, the highlights
/// are not pushed further up, and the largest channel never gets more than `(E - 1) * k` brighter (mirrors the shader).
#[allow(dead_code)] // Rust mirror of the HLSL, the reference of the tests
pub const BRIGHTEN_KNEE: f32 = 0.5;

#[allow(dead_code)] // Rust mirror of the HLSL, the reference of the tests
pub fn exposed_max(m: f32, ev: f32, sdr: bool) -> f32 {
    let m = if m.is_finite() && m > 0.0 { m } else { 0.0 };
    let e = ev.exp2();
    let out = if e <= 1.0 {
        m * e
    } else {
        let r = m / BRIGHTEN_KNEE;
        m + (e - 1.0) * m / (1.0 + r * r * r * r).powf(0.25)
    };
    if sdr { out.min(m.max(1.0)) } else { out }
}

/// The local tone mapping scale of a pixel whose base layer has linear luminance `base` (after the exposure): 1 below
/// the knee, `(base / knee)^-k` above it, with a C1 quadratic blend over +-0.5 EV around the knee (mirrors the shader).
#[allow(dead_code)] // Rust mirror of the HLSL, the reference of the tests
pub fn local_scale(base: f32, knee: f32, k: f32) -> f32 {
    if !(base > 0.0) || !(k > 0.0) {
        return 1.0;
    }
    let x = (base / knee).log2();
    let w = 0.5;
    let g = if x <= -w {
        0.0
    } else if x >= w {
        x
    } else {
        (x + w) * (x + w) / (4.0 * w)
    };
    (-k * g).exp2()
}

/// What the light-adjust substitute gets in `b9` (two float4; all zero = off).
/// [0] = (autoexp on, local tone exponent k, local knee, 0), [1] = (0, 0, 0, 0) reserved.
pub fn light_cb(autoexp: bool, local: bool) -> [[f32; 4]; 2] {
    [[if autoexp { 1.0 } else { 0.0 }, if local { local_strength() * LOCAL_GAIN } else { 0.0 }, LOCAL_KNEE, 0.0], [0.0; 4]]
}

/// The four constant vectors of `exposure.hlsl` (see its header).
pub fn cb_values(dt: f32, reset: bool, sdr: bool) -> [[f32; 4]; 4] {
    let (down, up) = ev_limits(range());
    [
        [target(), down, up, strength()],
        [adapt_up(), adapt_down(), dt, if reset { 1.0 } else { 0.0 }],
        [CENTER_WEIGHT, if sdr { 1.0 } else { 0.0 }, TOP_REJECT, 0.0],
        [BLUR_SIGMA_EV, 0.0, 0.0, 0.0],
    ]
}

/// (size of L0 at 1/4, L1 at 1/8, L2 at 1/16) for a scene size, never below 1.
pub fn level_sizes(w: u32, h: u32) -> [(u32, u32); 3] {
    let up = |a: u32, b: u32| a.div_ceil(b).max(1);
    let l0 = (up(w.max(1), 4), up(h.max(1), 4));
    let l1 = (up(l0.0, 2), up(l0.1, 2));
    let l2 = (up(l1.0, 2), up(l1.1, 2));
    [l0, l1, l2]
}

pub const HIST_BINS: u32 = 64;
/// Rows of partial histograms (the state pass sums them).
pub const HIST_PARTS: u32 = 8;

// ---- shaders (compiled once, in the background) --------------------------------------------------------------

pub struct Blobs {
    pub vs: Vec<u8>,
    pub lum: Vec<u8>,
    pub down_lin: Vec<u8>,
    pub down_log: Vec<u8>,
    pub blur: Vec<u8>,
    pub hist: Vec<u8>,
    pub state: Vec<u8>,
}

/// Compiles the shaders (calls D3DCompile: never on the render thread).
pub fn compile_blobs() -> Result<Blobs, String> {
    let c = |entry: &str, profile: &str| shaders::compile(HLSL, entry, profile).map_err(|e| format!("{entry}: {e}"));
    Ok(Blobs { vs: c(VS_ENTRY, "vs_5_0")?, lum: c(PS_LUM, "ps_5_0")?, down_lin: c(PS_DOWN_LIN, "ps_5_0")?, down_log: c(PS_DOWN_LOG, "ps_5_0")?, blur: c(PS_BLUR, "ps_5_0")?, hist: c(PS_HIST, "ps_5_0")?, state: c(PS_STATE, "ps_5_0")? })
}

static BLOBS: OnceLock<Result<Blobs, String>> = OnceLock::new();

/// Sets the live values from the config and starts the background compile.
pub fn init(strength: f32, target: f32, range: f32, adapt_up: f32, adapt_down: f32, local: f32) {
    set_strength(strength);
    set_target(target);
    set_range(range);
    set_adapt_up(adapt_up);
    set_adapt_down(adapt_down);
    set_local_strength(local);
    log!("exposure: configured (autoexp strength {}, target {}, range {} EV, adapt up {} s / down {} s, local tone {})", self::strength(), self::target(), self::range(), self::adapt_up(), self::adapt_down(), self::local_strength());
    let _ = std::thread::Builder::new().name("gw2g-exposure-compile".into()).spawn(|| {
        let r = compile_blobs();
        match &r {
            Ok(_) => log!("exposure: shaders compiled"),
            Err(e) => log!("exposure: shader compile failed, auto exposure and local tone mapping stay off: {e}"),
        }
        let _ = BLOBS.set(r);
    });
}

// ---- device objects ---------------------------------------------------------------------------------------------

const FMT_RGBA32F: u32 = 2;
const FMT_R32_FLOAT: u32 = 41;

#[repr(C)]
struct SubresourceData {
    mem: *const c_void,
    pitch: u32,
    slice: u32,
}

type CreateShaderFn = unsafe extern "system" fn(Obj, *const u8, usize, Obj, *mut Obj) -> i32;
type CreateViewFn = unsafe extern "system" fn(Obj, Obj, *const [u32; 6], *mut Obj) -> i32;

/// A render-target + shader-resource texture with its views.
#[derive(Clone, Copy)]
struct Target {
    tex: Obj,
    srv: Obj,
    rtv: Obj,
    size: (u32, u32),
}

const NO_TARGET: Target = Target { tex: std::ptr::null_mut(), srv: std::ptr::null_mut(), rtv: std::ptr::null_mut(), size: (0, 0) };

impl Target {
    unsafe fn free(&mut self) {
        unsafe {
            release(self.rtv);
            release(self.srv);
            release(self.tex);
        }
        *self = NO_TARGET;
    }

    /// `bpp` bytes per texel (8 for float16 RGBA, 16 for float32 RGBA); zero-filled when `zero`.
    unsafe fn make(dev: Obj, fmt: u32, bpp: u32, size: (u32, u32), zero: bool) -> Option<Target> {
        let mut t = NO_TARGET;
        unsafe {
            let desc = Tex2dDesc { width: size.0, height: size.1, mips: 1, array: 1, format: fmt, samples: 1, quality: 0, usage: 0, bind: 0x28, cpu: 0, misc: 0 };
            let create_tex: unsafe extern "system" fn(Obj, *const Tex2dDesc, *const c_void, *mut Obj) -> i32 = std::mem::transmute(vfn(dev, vt::DEV_CREATE_TEXTURE2D));
            let create_srv: CreateViewFn = std::mem::transmute(vfn(dev, vt::DEV_CREATE_SRV));
            let create_rtv: CreateViewFn = std::mem::transmute(vfn(dev, vt::DEV_CREATE_RTV));
            let zeros = vec![0u8; (size.0 * size.1 * bpp) as usize];
            let sd = SubresourceData { mem: zeros.as_ptr().cast(), pitch: size.0 * bpp, slice: 0 };
            let init: *const c_void = if zero { (&sd as *const SubresourceData).cast() } else { std::ptr::null() };
            let sdesc: [u32; 6] = [fmt, 4, 0, 1, 0, 0];
            let rdesc: [u32; 6] = [fmt, 4, 0, 0, 0, 0];
            if create_tex(dev, &desc, init, &mut t.tex) < 0 || t.tex.is_null() || create_srv(dev, t.tex, &sdesc, &mut t.srv) < 0 || create_rtv(dev, t.tex, &rdesc, &mut t.rtv) < 0 {
                t.free();
                return None;
            }
        }
        t.size = size;
        Some(t)
    }
}

/// What a run does this frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RunParams {
    pub autoexp: bool,
    pub local: bool,
    pub dt: f32,
    pub reset: bool,
    pub sdr: bool,
}

/// Device objects of the exposure passes, valid for one device only.
pub struct Exposure {
    device: usize,
    vs: Obj,
    ps_lum: Obj,
    ps_down_lin: Obj,
    ps_down_log: Obj,
    ps_blur: Obj,
    ps_hist: Obj,
    ps_state: Obj,
    cb: Obj,
    /// L0, L1, L2, B1 (blur H), B2 (blur V = the base layer).
    lv: [Target; 5],
    hist: Target,
    /// The 1x1 state, two sets (ping-pong).
    state: [Target; 2],
    cur: usize,
    size: (u32, u32),
    depth_srv: Obj,
    depth_key: usize,
}

// The pointers are only used under the mutex, from the render thread.
unsafe impl Send for Exposure {}

impl Exposure {
    /// Creates the shaders, the constant buffer and the state on `device`.
    ///
    /// # Safety
    /// `device` must be a live `ID3D11Device`.
    pub unsafe fn new(device: Obj, b: &Blobs) -> Option<Exposure> {
        let _ig = IgnoreScope::new();
        let nul = std::ptr::null_mut();
        let mut e = Exposure { device: device as usize, vs: nul, ps_lum: nul, ps_down_lin: nul, ps_down_log: nul, ps_blur: nul, ps_hist: nul, ps_state: nul, cb: nul, lv: [NO_TARGET; 5], hist: NO_TARGET, state: [NO_TARGET; 2], cur: 0, size: (0, 0), depth_srv: nul, depth_key: 0 };
        unsafe {
            let create_vs: CreateShaderFn = std::mem::transmute(vfn(device, vt::DEV_CREATE_VERTEX_SHADER));
            let create_ps: CreateShaderFn = std::mem::transmute(vfn(device, vt::DEV_CREATE_PIXEL_SHADER));
            let ok = create_vs(device, b.vs.as_ptr(), b.vs.len(), nul, &mut e.vs) >= 0
                && create_ps(device, b.lum.as_ptr(), b.lum.len(), nul, &mut e.ps_lum) >= 0
                && create_ps(device, b.down_lin.as_ptr(), b.down_lin.len(), nul, &mut e.ps_down_lin) >= 0
                && create_ps(device, b.down_log.as_ptr(), b.down_log.len(), nul, &mut e.ps_down_log) >= 0
                && create_ps(device, b.blur.as_ptr(), b.blur.len(), nul, &mut e.ps_blur) >= 0
                && create_ps(device, b.hist.as_ptr(), b.hist.len(), nul, &mut e.ps_hist) >= 0
                && create_ps(device, b.state.as_ptr(), b.state.len(), nul, &mut e.ps_state) >= 0;
            if !ok {
                log!("exposure: could not create the shaders");
                return None; // Drop releases what exists
            }
            let create_buffer: unsafe extern "system" fn(Obj, *const [u32; 6], *const c_void, *mut Obj) -> i32 = std::mem::transmute(vfn(device, vt::DEV_CREATE_BUFFER));
            // D3D11_BUFFER_DESC { ByteWidth, Usage = DYNAMIC, BindFlags = CONSTANT_BUFFER, CPUAccessFlags = WRITE, MiscFlags, StructureByteStride }
            if create_buffer(device, &[64, 2, 4, 0x10000, 0, 0], std::ptr::null(), &mut e.cb) < 0 || e.cb.is_null() {
                log!("exposure: could not create the constant buffer");
                return None;
            }
            let h = Target::make(device, FMT_RGBA32F, 16, (HIST_BINS, HIST_PARTS), false);
            let s0 = Target::make(device, FMT_RGBA32F, 16, (1, 1), true);
            let s1 = Target::make(device, FMT_RGBA32F, 16, (1, 1), true);
            let (Some(h), Some(s0), Some(s1)) = (h, s0, s1) else {
                log!("exposure: could not create the state targets");
                return None;
            };
            e.hist = h;
            e.state = [s0, s1];
        }
        Some(e)
    }

    unsafe fn free_levels(&mut self) {
        unsafe {
            for l in &mut self.lv {
                l.free();
            }
        }
        self.size = (0, 0);
    }

    /// (Re)creates the pyramid when the scene size changed.
    unsafe fn ensure_levels(&mut self, dev: Obj, size: (u32, u32)) -> bool {
        if self.size == size && !self.lv[0].tex.is_null() {
            return true;
        }
        unsafe {
            self.free_levels();
            let [l0, l1, l2] = level_sizes(size.0, size.1);
            for (i, s) in [l0, l1, l2, l2, l2].into_iter().enumerate() {
                match Target::make(dev, FMT_R16G16B16A16_FLOAT, 8, s, false) {
                    Some(t) => self.lv[i] = t,
                    None => {
                        self.free_levels();
                        return false;
                    }
                }
            }
        }
        self.size = size;
        log!("exposure: targets for scene {}x{}: levels {:?}", size.0, size.1, self.lv.iter().take(3).map(|l| l.size).collect::<Vec<_>>());
        true
    }

    /// SRV of the depth resource, cached while the resource is the same (None = no depth, the sky is not excluded).
    unsafe fn ensure_depth_srv(&mut self, dev: Obj, depth: Option<Obj>) -> bool {
        let Some(depth) = depth else {
            unsafe { release(self.depth_srv) };
            self.depth_srv = std::ptr::null_mut();
            self.depth_key = 0;
            return true;
        };
        if !self.depth_srv.is_null() && self.depth_key == depth as usize {
            return true;
        }
        unsafe {
            release(self.depth_srv);
            self.depth_srv = std::ptr::null_mut();
            self.depth_key = 0;
            let create_srv: CreateViewFn = std::mem::transmute(vfn(dev, vt::DEV_CREATE_SRV));
            let sdesc: [u32; 6] = [FMT_R32_FLOAT, 4, 0, 1, 0, 0];
            if create_srv(dev, depth, &sdesc, &mut self.depth_srv) < 0 || self.depth_srv.is_null() {
                self.depth_srv = std::ptr::null_mut();
                return false;
            }
        }
        self.depth_key = depth as usize;
        true
    }

    /// SRV of the current 1x1 state (valid after a `run` with `autoexp`).
    pub fn state_srv(&self) -> Obj {
        self.state[self.cur].srv
    }

    /// SRV of the base layer (valid after a `run` with `local`).
    pub fn base_srv(&self) -> Obj {
        self.lv[4].srv
    }

    /// Size of the analysis level / base layer (1/16 of the scene).
    #[allow(dead_code)]
    pub fn base_size(&self) -> (u32, u32) {
        self.lv[4].size
    }

    /// The texture of the current state (tests, the dev read-back).
    pub fn state_tex(&self) -> Obj {
        self.state[self.cur].tex
    }

    /// Runs the draws from `scene_srv` (gamma ~2.0 colour of size `scene` = (w, h)) and the optional linear `depth`
    /// (R32_FLOAT). All pipeline state touched is restored. False when something failed before the first draw.
    ///
    /// # Safety
    /// All pointers must be live objects of the device this was created on.
    pub unsafe fn run(&mut self, dev: Obj, ctx: Obj, scene_srv: Obj, scene: (u32, u32), depth: Option<Obj>, p: &RunParams) -> bool {
        let _ig = IgnoreScope::new();
        unsafe {
            if !self.ensure_levels(dev, scene) || !self.ensure_depth_srv(dev, depth) {
                return false;
            }
            let _state = StateGuard::save(ctx);
            let factor = [1.0f32; 4];
            let set_rt: unsafe extern "system" fn(Obj, u32, *const Obj, Obj) = std::mem::transmute(vfn(ctx, vt::CTX_OM_SET_RENDER_TARGETS));
            let set_vp: unsafe extern "system" fn(Obj, u32, *const Viewport) = std::mem::transmute(vfn(ctx, vt::CTX_RS_SET_VIEWPORTS));
            let set_rs: unsafe extern "system" fn(Obj, Obj) = std::mem::transmute(vfn(ctx, vt::CTX_RS_SET_STATE));
            let set_blend: unsafe extern "system" fn(Obj, Obj, *const f32, u32) = std::mem::transmute(vfn(ctx, vt::CTX_OM_SET_BLEND));
            let set_depth: unsafe extern "system" fn(Obj, Obj, u32) = std::mem::transmute(vfn(ctx, vt::CTX_OM_SET_DEPTH));
            let set_layout: unsafe extern "system" fn(Obj, Obj) = std::mem::transmute(vfn(ctx, vt::CTX_IA_SET_LAYOUT));
            let set_topology: unsafe extern "system" fn(Obj, u32) = std::mem::transmute(vfn(ctx, vt::CTX_IA_SET_TOPOLOGY));
            type SetShader = unsafe extern "system" fn(Obj, Obj, *const Obj, u32);
            let set_vs: SetShader = std::mem::transmute(vfn(ctx, vt::CTX_VS_SET_SHADER));
            let set_gs: SetShader = std::mem::transmute(vfn(ctx, vt::CTX_GS_SET_SHADER));
            let set_ps: SetShader = std::mem::transmute(vfn(ctx, vt::CTX_PS_SET_SHADER));
            type SetSlots = unsafe extern "system" fn(Obj, u32, u32, *const Obj);
            let set_srv: SetSlots = std::mem::transmute(vfn(ctx, vt::CTX_PS_SET_SRVS));
            let set_cb: SetSlots = std::mem::transmute(vfn(ctx, vt::CTX_PS_SET_CBS));
            let draw: unsafe extern "system" fn(Obj, u32, u32) = std::mem::transmute(vfn(ctx, vt::CTX_DRAW));
            let map: unsafe extern "system" fn(Obj, Obj, u32, u32, u32, *mut [usize; 3]) -> i32 = std::mem::transmute(vfn(ctx, vt::CTX_MAP));
            let unmap: unsafe extern "system" fn(Obj, Obj, u32) = std::mem::transmute(vfn(ctx, vt::CTX_UNMAP));
            let nul: Obj = std::ptr::null_mut();
            let vp = |size: (u32, u32)| Viewport { x: 0.0, y: 0.0, w: size.0 as f32, h: size.1 as f32, min_depth: 0.0, max_depth: 1.0 };
            // Slots used besides t0: t10..t12 (the module's own range); the previous frame's results are bound on the game's
            // context as t5 / t21 for the light-adjust draw and must be free before they become targets.
            let clear = || set_srv(ctx, 10, 3, [nul; 3].as_ptr());
            set_srv(ctx, STATE_SLOT, 1, &nul);
            set_srv(ctx, BASE_SLOT, 1, &nul);

            let (cbuf, depth_srv, vs) = (self.cb, self.depth_srv, self.vs);
            let base_cb = cb_values(p.dt, p.reset, p.sdr);
            let write_cb = |blur: (f32, f32, f32)| -> bool {
                let mut mapped: [usize; 3] = [0; 3]; // D3D11_MAPPED_SUBRESOURCE { pData, RowPitch, DepthPitch }
                if map(ctx, cbuf, 0, 4 /* WRITE_DISCARD */, 0, &mut mapped) < 0 || mapped[0] == 0 {
                    return false;
                }
                let mut v = base_cb;
                v[3] = [BLUR_SIGMA_EV, blur.0, blur.1, blur.2];
                *(mapped[0] as *mut [[f32; 4]; 4]) = v;
                unmap(ctx, cbuf, 0);
                true
            };
            if !write_cb((0.0, 0.0, 0.0)) {
                return false;
            }

            set_rs(ctx, nul);
            set_depth(ctx, nul, 0);
            set_blend(ctx, nul, factor.as_ptr(), 0xffff_ffff);
            set_layout(ctx, nul);
            set_topology(ctx, 4 /* TRIANGLELIST */);
            set_vs(ctx, vs, std::ptr::null(), 0);
            set_gs(ctx, nul, std::ptr::null(), 0);
            set_cb(ctx, 0, 1, &cbuf);

            // One pass: clear the inputs, the target, the shader, then the inputs.
            let pass = |t: &Target, ps: Obj, a: Obj, b: Obj, with_scene: bool, with_depth: bool| {
                clear();
                set_rt(ctx, 1, &t.rtv, nul);
                set_vp(ctx, 1, &vp(t.size));
                set_ps(ctx, ps, std::ptr::null(), 0);
                if with_scene {
                    set_srv(ctx, 0, 1, &scene_srv);
                }
                if with_depth && !depth_srv.is_null() {
                    set_srv(ctx, 10, 1, &depth_srv);
                }
                if !a.is_null() || !b.is_null() {
                    set_srv(ctx, 11, 2, [a, b].as_ptr());
                }
                draw(ctx, 3, 0);
            };

            // 1: scene (+ depth) -> L0 (1/4).
            pass(&self.lv[0], self.ps_lum, nul, nul, true, true);
            // 2: L0 -> L1 (1/8), 3: L1 -> L2 (1/16, the analysis level).
            pass(&self.lv[1], self.ps_down_lin, self.lv[0].srv, nul, false, false);
            pass(&self.lv[2], self.ps_down_log, self.lv[1].srv, nul, false, false);
            if p.local {
                // 4: edge-aware blur, horizontal then vertical (step 2 texels, 13 taps: reach 12 texels = 192 px of the scene).
                if !write_cb((2.0, 0.0, 0.0)) {
                    return false;
                }
                pass(&self.lv[3], self.ps_blur, self.lv[2].srv, nul, false, false);
                if !write_cb((0.0, 2.0, 1.0)) {
                    return false;
                }
                pass(&self.lv[4], self.ps_blur, self.lv[3].srv, nul, false, false);
            }
            if p.autoexp {
                if !write_cb((0.0, 0.0, 0.0)) {
                    return false;
                }
                // 5: histogram of the analysis level, 6: histogram + previous state -> new state.
                pass(&self.hist, self.ps_hist, self.lv[2].srv, nul, false, false);
                let new = 1 - self.cur;
                pass(&self.state[new], self.ps_state, self.hist.srv, self.state[self.cur].srv, false, false);
                self.cur = new;
            }
            // Leave nothing of ours bound (the guard restores t0, t1 and b0).
            clear();
        }
        true
    }

    /// Reads the current state back (x = EV applied, y = measured log2 luminance, z = EV asked for, w = initialised).
    /// A GPU stall: only for the dev capture.
    ///
    /// # Safety
    /// `dev` / `ctx` must be the live device and immediate context these objects belong to.
    pub unsafe fn read_state(&self, dev: Obj, ctx: Obj) -> Option<[f32; 4]> {
        let _ig = IgnoreScope::new();
        unsafe {
            let create_tex: unsafe extern "system" fn(Obj, *const Tex2dDesc, *const c_void, *mut Obj) -> i32 = std::mem::transmute(vfn(dev, vt::DEV_CREATE_TEXTURE2D));
            // STAGING (3), no bind, CPU read (0x20000).
            let desc = Tex2dDesc { width: 1, height: 1, mips: 1, array: 1, format: FMT_RGBA32F, samples: 1, quality: 0, usage: 3, bind: 0, cpu: 0x20000, misc: 0 };
            let mut st: Obj = std::ptr::null_mut();
            if create_tex(dev, &desc, std::ptr::null(), &mut st) < 0 || st.is_null() {
                return None;
            }
            let copy: unsafe extern "system" fn(Obj, Obj, Obj) = std::mem::transmute(vfn(ctx, vt::CTX_COPY_RESOURCE));
            copy(ctx, st, self.state_tex());
            let map: unsafe extern "system" fn(Obj, Obj, u32, u32, u32, *mut [usize; 3]) -> i32 = std::mem::transmute(vfn(ctx, vt::CTX_MAP));
            let unmap: unsafe extern "system" fn(Obj, Obj, u32) = std::mem::transmute(vfn(ctx, vt::CTX_UNMAP));
            let mut mapped: [usize; 3] = [0; 3];
            let out = if map(ctx, st, 0, 1 /* READ */, 0, &mut mapped) >= 0 && mapped[0] != 0 {
                let v = *(mapped[0] as *const [f32; 4]);
                unmap(ctx, st, 0);
                Some(v)
            } else {
                None
            };
            release(st);
            out
        }
    }
}

impl Drop for Exposure {
    fn drop(&mut self) {
        // Objects of a device that is gone are children that keep it alive; releasing them is safe.
        unsafe {
            self.free_levels();
            self.hist.free();
            for t in self.state.iter_mut() {
                t.free();
            }
            for o in [self.depth_srv, self.cb, self.ps_state, self.ps_hist, self.ps_blur, self.ps_down_log, self.ps_down_lin, self.ps_lum, self.vs] {
                release(o);
            }
        }
    }
}

// ---- the light-adjust inputs: b9 and the unbound fallback ---------------------------------------------------

/// The constant buffer of the light-adjust inputs (b9), per device. Kept apart from `Exposure` so that it exists (and
/// reads "off") even while the shaders are compiling or the feature is off: the engine may leave something in b9.
struct ParamBuf {
    device: usize,
    cb: Obj,
}

unsafe impl Send for ParamBuf {}

impl Drop for ParamBuf {
    fn drop(&mut self) {
        unsafe { release(self.cb) };
    }
}

static PARAMS: Mutex<Option<ParamBuf>> = Mutex::new(None);

/// Writes `v` into `b9` and binds `b9`, `t5` and `t21` (null = unbound) on `ctx`. False when the buffer is not available.
///
/// # Safety
/// `ctx` must be the live immediate context of `dev`; `state` / `base` live SRVs of it or null.
unsafe fn bind_light_inputs(ctx: Obj, dev: Obj, v: [[f32; 4]; 2], state: Obj, base: Obj) -> bool {
    unsafe {
        let Ok(mut g) = PARAMS.try_lock() else { return false };
        if g.as_ref().is_some_and(|p| p.device != dev as usize) {
            *g = None;
        }
        if g.is_none() {
            let create_buffer: unsafe extern "system" fn(Obj, *const [u32; 6], *const c_void, *mut Obj) -> i32 = std::mem::transmute(vfn(dev, vt::DEV_CREATE_BUFFER));
            let mut cb: Obj = std::ptr::null_mut();
            if create_buffer(dev, &[32, 2, 4, 0x10000, 0, 0], std::ptr::null(), &mut cb) < 0 || cb.is_null() {
                return false;
            }
            *g = Some(ParamBuf { device: dev as usize, cb });
        }
        let cb = g.as_ref().map_or(std::ptr::null_mut(), |p| p.cb);
        let mut mapped: [usize; 3] = [0; 3];
        let map: unsafe extern "system" fn(Obj, Obj, u32, u32, u32, *mut [usize; 3]) -> i32 = std::mem::transmute(vfn(ctx, vt::CTX_MAP));
        if map(ctx, cb, 0, 4 /* WRITE_DISCARD */, 0, &mut mapped) < 0 || mapped[0] == 0 {
            return false;
        }
        *(mapped[0] as *mut [[f32; 4]; 2]) = v;
        let unmap: unsafe extern "system" fn(Obj, Obj, u32) = std::mem::transmute(vfn(ctx, vt::CTX_UNMAP));
        unmap(ctx, cb, 0);
        type SetSlots = unsafe extern "system" fn(Obj, u32, u32, *const Obj);
        let set_srv: SetSlots = std::mem::transmute(vfn(ctx, vt::CTX_PS_SET_SRVS));
        let set_cb: SetSlots = std::mem::transmute(vfn(ctx, vt::CTX_PS_SET_CBS));
        set_srv(ctx, STATE_SLOT, 1, &state);
        set_srv(ctx, BASE_SLOT, 1, &base);
        set_cb(ctx, CB_SLOT, 1, &cb);
        true
    }
}

// ---- per-frame glue (called from datapath::bind_current) ----------------------------------------------------

static EXPO: Mutex<Option<Exposure>> = Mutex::new(None);
static LAST_RUN: Mutex<Option<std::time::Instant>> = Mutex::new(None);
/// The game's immediate context of the last run (for the dev read-back, same thread).
static LAST_CTX: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
/// Last read-back (EV applied, measured log2 luminance, EV asked for); NaN until the first one.
static READ_EV: AtomicU32 = AtomicU32::new(0x7fc0_0000);
static READ_AVG: AtomicU32 = AtomicU32::new(0x7fc0_0000);
static READ_ASK: AtomicU32 = AtomicU32::new(0x7fc0_0000);
/// Size of the base layer of the last run (w << 16 | h).
static BASE_DIM: AtomicU32 = AtomicU32::new(0);

fn log_once(flag: &'static AtomicBool, msg: impl FnOnce() -> String) {
    if !flag.swap(true, Relaxed) {
        log!("{}", msg());
    }
}

/// The light-adjust substitute is about to draw on the game's context: runs the passes (once per frame) when a feature is
/// on and binds the results (or "off") as `t5` / `t21` / `b9`. Never unwinds; every failure is logged once and leaves the
/// frame untouched (the shader reads zero = no exposure, no local tone).
pub fn on_light_adjust(ctx: Obj, dev: Obj) {
    let _ = std::panic::catch_unwind(|| unsafe { on_light_adjust_inner(ctx, dev) });
}

unsafe fn on_light_adjust_inner(ctx: Obj, dev: Obj) {
    static BAD_SCENE: AtomicBool = AtomicBool::new(false);
    static NO_RUN: AtomicBool = AtomicBool::new(false);
    static NO_DEPTH: AtomicBool = AtomicBool::new(false);
    static FIRST: AtomicBool = AtomicBool::new(false);
    let _ig = IgnoreScope::new();
    let nul: Obj = std::ptr::null_mut();
    let (want_ae, want_lt) = (wants_autoexp(), wants_local());
    let mut result: Option<(Obj, Obj, bool, bool)> = None; // (state, base, autoexp applied, local applied)
    if (want_ae || want_lt) && !FAILED.load(Relaxed) {
        CUR.fetch_or(SEEN_DRAW, Relaxed);
        result = unsafe { run_frame(ctx, dev, want_ae, want_lt, &BAD_SCENE, &NO_RUN, &NO_DEPTH, &FIRST) };
    }
    let (state, base, ae, lt) = result.unwrap_or((nul, nul, false, false));
    let ok = unsafe { bind_light_inputs(ctx, dev, light_cb(ae, lt), if ae { state } else { nul }, if lt { base } else { nul }) };
    if !ok {
        log_once(&NO_RUN, || "exposure: could not bind the light-adjust inputs (b9); auto exposure and local tone mapping are skipped".to_string());
    }
}

/// Runs (or reuses this frame's) passes and returns the SRVs to bind.
unsafe fn run_frame(ctx: Obj, dev: Obj, want_ae: bool, want_lt: bool, bad_scene: &'static AtomicBool, no_run: &'static AtomicBool, no_depth: &'static AtomicBool, first: &'static AtomicBool) -> Option<(Obj, Obj, bool, bool)> {
    let blobs = match BLOBS.get() {
        None => return None, // still compiling
        Some(Err(_)) => {
            FAILED.store(true, Relaxed);
            return None;
        }
        Some(Ok(b)) => b,
    };
    let mut guard = EXPO.try_lock().ok()?;
    if guard.as_ref().is_some_and(|x| x.device != dev as usize) {
        *guard = None;
    }
    if guard.is_none() {
        match unsafe { Exposure::new(dev, blobs) } {
            Some(x) => *guard = Some(x),
            None => {
                FAILED.store(true, Relaxed);
                log!("exposure: could not be created; auto exposure and local tone mapping stay off");
                return None;
            }
        }
    }
    let ex = guard.as_mut()?;
    unsafe {
        if !DONE.load(Relaxed) {
            // The scene colour: the pixel shader SRV in slot 0 of the light-adjust draw.
            let mut scene_srv: Obj = std::ptr::null_mut();
            let get: unsafe extern "system" fn(Obj, u32, u32, *mut Obj) = std::mem::transmute(vfn(ctx, vt::CTX_PS_GET_SRVS));
            get(ctx, 0, 1, &mut scene_srv);
            if scene_srv.is_null() {
                CUR.fetch_or(SEEN_BAD_SCENE, Relaxed);
                log_once(bad_scene, || "exposure: skipped, no scene colour is bound in slot 0".to_string());
                return None;
            }
            let Some((res, d)) = ao::texture_of(scene_srv) else {
                release(scene_srv);
                CUR.fetch_or(SEEN_BAD_SCENE, Relaxed);
                log_once(bad_scene, || "exposure: skipped, the scene colour in slot 0 is not a 2D texture".to_string());
                return None;
            };
            release(res);
            if d.samples != 1 || d.array != 1 || d.width < 16 || d.height < 16 {
                release(scene_srv);
                CUR.fetch_or(SEEN_BAD_SCENE, Relaxed);
                log_once(bad_scene, || format!("exposure: skipped, the scene colour is {}x{} format {} samples {}", d.width, d.height, d.format, d.samples));
                return None;
            }
            let depth = ao::depth_resource();
            if depth.is_none() && want_ae {
                CUR.fetch_or(SEEN_NO_DEPTH, Relaxed);
                log_once(no_depth, || "exposure: the linear depth of this frame is not available; the sky is not excluded from the measurement".to_string());
            }
            let elapsed = LAST_RUN.lock().ok().and_then(|l| l.map(|t| t.elapsed().as_secs_f32()));
            let (dt, reset) = frame_dt(elapsed);
            let sdr = crate::output::mode() != crate::output::OutputMode::ScRgb;
            if sdr {
                CUR.fetch_or(SEEN_SDR, Relaxed);
            }
            let p = RunParams { autoexp: want_ae, local: want_lt, dt, reset, sdr };
            let ok = ex.run(dev, ctx, scene_srv, (d.width, d.height), depth, &p);
            release(scene_srv);
            if !ok {
                log_once(no_run, || "exposure: the targets, the depth view or the constants could not be set up; frame left untouched".to_string());
                return None;
            }
            DONE.store(true, Relaxed);
            CUR.fetch_or(SEEN_APPLIED, Relaxed);
            if want_ae {
                if let Ok(mut l) = LAST_RUN.lock() {
                    *l = Some(std::time::Instant::now());
                }
            }
            LAST_CTX.store(ctx as usize, Relaxed);
            let s = ex.base_size();
            BASE_DIM.store((s.0 << 16) | s.1.min(0xffff), Relaxed);
            log_once(first, || format!("exposure: first frame applied (scene {}x{}, autoexp {}, local tone {}, {})", d.width, d.height, want_ae, want_lt, if sdr { "SDR clamp" } else { "HDR" }));
        }
        // The autoexp state exists once a state pass has run (this frame or an earlier one with the same settings).
        Some((ex.state_srv(), ex.base_srv(), want_ae, want_lt && DONE.load(Relaxed)))
    }
}

/// Dev capture: reads the state back (a small GPU stall) and caches it for `status_text`. Call on the render thread.
pub fn probe_state() {
    let _ = std::panic::catch_unwind(|| {
        let ctx = LAST_CTX.load(Relaxed) as Obj;
        if ctx.is_null() {
            return;
        }
        let Ok(g) = EXPO.try_lock() else { return };
        let Some(ex) = g.as_ref() else { return };
        if let Some(v) = unsafe { ex.read_state(ex.device as Obj, ctx) } {
            if v[3] > 0.5 {
                READ_EV.store(v[0].to_bits(), Relaxed);
                READ_AVG.store(v[1].to_bits(), Relaxed);
                READ_ASK.store(v[2].to_bits(), Relaxed);
            }
        }
    });
}

fn status_prefix(strength_now: f32) -> Option<String> {
    if FAILED.load(Relaxed) {
        return Some("failed (see log), off".to_string());
    }
    if strength_now <= 0.0 {
        return Some("off (strength 0)".to_string());
    }
    if !ENABLED.load(Relaxed) || !crate::gate::user_enabled() {
        return Some("waiting (classifier gate closed or mod disabled)".to_string());
    }
    if BLOBS.get().is_none() {
        return Some("compiling shaders".to_string());
    }
    let l = LAST.load(Relaxed);
    if l & SEEN_APPLIED == 0 {
        return Some(if l & SEEN_BAD_SCENE != 0 { "skipped: scene colour not usable (see log)".to_string() } else if l & SEEN_DRAW != 0 { "skipped: could not run (see log)".to_string() } else { "waiting for a scene frame".to_string() });
    }
    None
}

/// Overlay / dev capture line for the auto exposure: the current EV and the measured log-average (after `probe_state`).
pub fn status_text() -> String {
    if let Some(s) = status_prefix(strength()) {
        return s;
    }
    let l = LAST.load(Relaxed);
    let ev = f32::from_bits(READ_EV.load(Relaxed));
    let avg = f32::from_bits(READ_AVG.load(Relaxed));
    let ask = f32::from_bits(READ_ASK.load(Relaxed));
    let mode = if l & SEEN_SDR != 0 { "SDR clamp" } else { "HDR" };
    let depth = if l & SEEN_NO_DEPTH != 0 { ", sky NOT excluded (no depth)" } else { ", sky excluded" };
    if ev.is_nan() {
        format!("applied ({mode}{depth}); EV not read yet (dev capture reads it)")
    } else {
        format!("applied ({mode}{depth}): EV {ev:+.2} (the scene asks {ask:+.2}), measured log-average luminance {:.3} (log2 {avg:.2}), target {:.2}, strength {:.2}", avg.exp2(), target(), strength())
    }
}

/// Overlay / dev capture line for the local tone mapping.
pub fn status_local_text() -> String {
    if let Some(s) = status_prefix(local_strength()) {
        return s;
    }
    let l = LAST.load(Relaxed);
    let d = BASE_DIM.load(Relaxed);
    format!("applied ({}): strength {:.2} (exponent {:.2} above {:.2} of paper white), base layer {}x{}", if l & SEEN_SDR != 0 { "SDR clamp" } else { "HDR" }, local_strength(), local_strength() * LOCAL_GAIN, LOCAL_KNEE, d >> 16, d & 0xffff)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamps_defaults_and_nan() {
        assert_eq!((DEFAULT_STRENGTH, DEFAULT_TARGET, DEFAULT_RANGE, DEFAULT_ADAPT_UP, DEFAULT_ADAPT_DOWN, DEFAULT_LOCAL), (0.5, 0.2, 0.7, 0.6, 1.8, 0.2));
        assert_eq!(strength_defaults_match_atomics(), true);
        assert_eq!(clamp_strength(9.0), 1.0);
        assert_eq!(clamp_strength(-1.0), 0.0);
        assert_eq!(clamp_strength(f32::NAN), DEFAULT_STRENGTH);
        assert_eq!(clamp_target(0.0), 0.05);
        assert_eq!(clamp_target(5.0), 0.6);
        assert_eq!(clamp_range(f32::NAN), DEFAULT_RANGE);
        assert_eq!(clamp_range(9.0), 2.0);
        assert_eq!(clamp_adapt(0.0, 1.0), 0.05);
        assert_eq!(clamp_adapt(f32::NAN, 1.8), 1.8);
        assert_eq!(clamp_local(f32::INFINITY), 1.0);
    }

    fn strength_defaults_match_atomics() -> bool {
        // The static initialisers are bit patterns: pin them to the documented defaults.
        let near = |a: u32, v: f32| (f32::from_bits(a) - v).abs() < 1e-6;
        near(0x3f00_0000, DEFAULT_STRENGTH) && near(0x3e4c_cccd, DEFAULT_TARGET) && near(0x3f33_3333, DEFAULT_RANGE) && near(0x3f19_999a, DEFAULT_ADAPT_UP) && near(0x3fe6_6666, DEFAULT_ADAPT_DOWN) && near(0x3e4c_cccd, DEFAULT_LOCAL)
    }

    #[test]
    fn soft_limit_is_identity_inside_bounded_monotonic_and_symmetric() {
        assert_eq!(soft_limit(0.3, 0.7), 0.3);
        assert_eq!(soft_limit(-0.4, 0.7), -0.4);
        let mut prev = -10.0;
        for i in -200..=200 {
            let x = i as f32 * 0.05;
            let y = soft_limit(x, 0.7);
            assert!(y >= prev - 1e-6, "monotonic at {x}");
            assert!(y.abs() <= 0.7 + 1e-6);
            assert!((soft_limit(-x, 0.7) + y).abs() < 1e-6, "odd");
            prev = y;
        }
        assert!((soft_limit(100.0, 0.7) - 0.7).abs() < 1e-4);
        assert_eq!(soft_limit(f32::NAN, 0.7), 0.0);
        assert_eq!(soft_limit(1.0, 0.0), 0.0);
        // C1 at the start of the roll-off.
        let a = 0.6 * 0.7;
        let (l, r) = (soft_limit(a - 1e-3, 0.7), soft_limit(a + 1e-3, 0.7));
        assert!(((r - l) / 2e-3 - 1.0).abs() < 0.01);
    }

    #[test]
    fn target_ev_follows_the_scene_inside_the_range_and_is_limited_outside() {
        // log-average a half EV under the target: +0.5 EV wanted, strength 1, range 2 (brightening cap 1): exactly that (inside 60 % of the range).
        let dim = 0.2f32.log2() - 0.5;
        assert!((target_ev(dim, 0.2, 1.0, 2.0) - 0.5).abs() < 1e-5);
        // Strength scales it, strength 0 gives exactly 0.
        assert!((target_ev(dim, 0.2, 0.5, 2.0) - 0.25).abs() < 1e-5);
        assert_eq!(target_ev(0.01f32.log2(), 0.2, 0.0, 2.0), 0.0);
        // Dark cave: brightening is capped by the range and by the cap.
        assert!(target_ev(0.001f32.log2(), 0.2, 1.0, 0.7) <= 0.7 + 1e-5);
        assert!(target_ev(0.001f32.log2(), 0.2, 1.0, 2.0) <= BRIGHTEN_CAP_EV + 1e-5, "never brighter than the cap");
        assert!(target_ev(0.001f32.log2(), 0.2, 1.0, 2.0) > 0.9);
        // Bright sand: darkening is limited by the range.
        assert!(target_ev(8.0f32.log2(), 0.2, 1.0, 0.7) >= -0.7 - 1e-5);
        assert!(target_ev(8.0f32.log2(), 0.2, 1.0, 0.7) < -0.6);
        // On target: nothing.
        assert!(target_ev(0.2f32.log2(), 0.2, 1.0, 0.7).abs() < 1e-6);
        assert_eq!(ev_limits(2.0), (2.0, 1.0));
        assert_eq!(ev_limits(0.4), (0.4, 0.4));
    }

    #[test]
    fn smoothing_uses_separate_times_and_converges() {
        // Scene gets brighter: the EV goes down with the 'up' time (0.6 s); darker: up with the 'down' time (1.8 s).
        let dt = 1.0 / 60.0;
        let steps_to = |from: f32, to: f32| {
            let mut v = from;
            let mut n = 0;
            while (v - to).abs() > 0.05 * (from - to).abs() {
                v = step_ev(v, to, dt, 0.6, 1.8);
                n += 1;
                assert!(n < 10_000);
            }
            n as f32 * dt
        };
        let t_bright = steps_to(0.5, -0.5); // 95 % in about 3 tau
        let t_dark = steps_to(-0.5, 0.5);
        assert!((t_bright - 3.0 * 0.6).abs() < 0.15, "{t_bright}");
        assert!((t_dark - 3.0 * 1.8).abs() < 0.3, "{t_dark}");
        assert!(t_dark > 2.5 * t_bright, "adapting to the dark is slower");
        // No overshoot, exact at dt 0 and at a huge dt.
        assert_eq!(step_ev(0.3, -0.3, 0.0, 0.6, 1.8), 0.3);
        assert!((step_ev(0.3, -0.3, 1000.0, 0.6, 1.8) + 0.3).abs() < 1e-6);
    }

    #[test]
    fn frame_time_is_clamped_and_long_gaps_reset() {
        assert_eq!(frame_dt(Some(0.016)), (0.016, false));
        assert_eq!(frame_dt(Some(0.5)), (MAX_DT, false));
        assert_eq!(frame_dt(Some(3.0)), (MAX_DT, true));
        assert_eq!(frame_dt(None), (MAX_DT, true));
        assert_eq!(frame_dt(Some(f32::NAN)), (MAX_DT, true));
        assert_eq!(frame_dt(Some(-1.0)), (MAX_DT, true));
    }

    #[test]
    fn exposed_max_is_monotonic_bounded_and_respects_sdr() {
        for ev in [-0.7f32, -0.3, 0.0, 0.3, 0.7, 1.0] {
            let mut prev = -1.0;
            for i in 0..=400 {
                let m = i as f32 * 0.05;
                let o = exposed_max(m, ev, false);
                assert!(o >= prev - 1e-6, "monotonic ev {ev} m {m}");
                prev = o;
                let e = ev.exp2();
                if e > 1.0 {
                    assert!(o <= m + (e - 1.0) * BRIGHTEN_KNEE + 1e-4, "highlights get at most (E-1)*knee brighter");
                } else {
                    assert!((o - m * e).abs() < 1e-5);
                }
            }
        }
        assert_eq!(exposed_max(0.7, 0.0, false), 0.7);
        // SDR: never above max(m, 1).
        for i in 0..=100 {
            let m = i as f32 * 0.03;
            assert!(exposed_max(m, 1.0, true) <= m.max(1.0) + 1e-6);
        }
        assert_eq!(exposed_max(f32::NAN, 0.5, false), 0.0);
        assert_eq!(exposed_max(-3.0, 0.5, false), 0.0);
    }

    #[test]
    fn local_scale_is_one_below_the_knee_compresses_above_and_is_bounded() {
        let k = 0.6 * 0.4;
        assert_eq!(local_scale(0.3, 0.6, k), 1.0);
        assert_eq!(local_scale(0.6 * 0.7, 0.6, k), 1.0); // below -0.5 EV
        let mut prev = 1.0;
        for i in 1..=200 {
            let b = i as f32 * 0.05;
            let s = local_scale(b, 0.6, k);
            assert!(s <= prev + 1e-6, "the scale never rises with the base: {b}");
            assert!(s > 0.0 && s <= 1.0);
            // base * scale stays monotonic (a brighter base never comes out darker).
            assert!(b * s >= (b - 0.05) * local_scale(b - 0.05, 0.6, k) - 1e-6);
            prev = s;
        }
        assert!((local_scale(1.2, 0.6, k) - (-k).exp2()).abs() < 1e-5, "one EV above the knee: 2^-k");
        assert_eq!(local_scale(1.2, 0.6, 0.0), 1.0);
        assert_eq!(local_scale(f32::NAN, 0.6, k), 1.0);
        assert_eq!(local_scale(-1.0, 0.6, k), 1.0);
    }

    #[test]
    fn cb_values_and_light_cb_follow_the_settings() {
        let _l = crate::testlock::lock();
        let keep = (strength(), target(), range(), adapt_up(), adapt_down(), local_strength());
        set_strength(0.5);
        set_target(0.2);
        set_range(2.0);
        set_adapt_up(0.6);
        set_adapt_down(1.8);
        set_local_strength(0.4);
        let c = cb_values(0.016, false, true);
        assert_eq!(c[0], [0.2, 2.0, 1.0, 0.5]);
        assert_eq!(c[1], [0.6, 1.8, 0.016, 0.0]);
        assert_eq!(c[2], [CENTER_WEIGHT, 1.0, TOP_REJECT, 0.0]);
        assert_eq!(cb_values(0.1, true, false)[1][3], 1.0);
        assert!(c.iter().flatten().all(|v| v.is_finite()));
        let l = light_cb(true, true);
        assert_eq!(l[0][0], 1.0);
        assert!((l[0][1] - 0.4 * LOCAL_GAIN).abs() < 1e-6);
        assert_eq!(light_cb(false, false), [[0.0, 0.0, LOCAL_KNEE, 0.0], [0.0; 4]]);
        set_strength(keep.0);
        set_target(keep.1);
        set_range(keep.2);
        set_adapt_up(keep.3);
        set_adapt_down(keep.4);
        set_local_strength(keep.5);
    }

    #[test]
    fn level_sizes_divide_and_never_reach_zero() {
        assert_eq!(level_sizes(3440, 1440), [(860, 360), (430, 180), (215, 90)]);
        assert_eq!(level_sizes(256, 256), [(64, 64), (32, 32), (16, 16)]);
        assert_eq!(level_sizes(1, 1), [(1, 1); 3]);
        assert_eq!(level_sizes(0, 0), [(1, 1); 3]);
    }

    #[test]
    fn enabled_needs_gate_and_user_and_strength_decides_wants() {
        let _l = crate::testlock::lock();
        let keep = (strength(), local_strength());
        set_enabled(false);
        set_strength(0.5);
        set_local_strength(0.4);
        assert!(!wants_autoexp() && !wants_local());
        set_enabled(true);
        if crate::gate::user_enabled() {
            assert!(wants_autoexp() && wants_local());
            set_strength(0.0);
            assert!(!wants_autoexp(), "strength 0 disables the passes");
            set_local_strength(0.0);
            assert!(!wants_local());
        }
        set_enabled(false);
        set_strength(keep.0);
        set_local_strength(keep.1);
    }

    #[test]
    fn strength_drives_the_depth_consumer_bit() {
        let _l = crate::testlock::lock();
        let keep = strength();
        set_strength(0.0);
        assert_eq!(ao::depth_consumers() & ao::DEPTH_FOR_EXPOSURE, 0);
        set_strength(0.5);
        assert_ne!(ao::depth_consumers() & ao::DEPTH_FOR_EXPOSURE, 0);
        set_strength(keep);
    }
}

#[cfg(test)]
#[path = "exposure_tests.rs"]
mod gpu_tests;
