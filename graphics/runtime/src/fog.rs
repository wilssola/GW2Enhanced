//! Fog colour and reference depth, estimated on the GPU every frame, so the AO can be released on water and fog of
//! ANY colour on any map (no per-map data). See `docs/m2-ao.md`, "Fog and water release".
//!
//! At the light-adjust draw (`datapath::bind_current` calls `on_light_adjust`), when the AO ran this frame, four tiny
//! draws run in this module's own targets (the shafts pattern: nothing is drawn into the engine's targets, no CPU
//! read-back, no stall):
//!
//! 1. depth -> 16x16 tiles of the largest valid depth, 2. tiles -> the frame's largest valid depth `zref`,
//! 3. scene colour of the FAR pixels (invalid depth = sky, or depth >= 0.9 * zref) -> 16x16 tile sums,
//! 3b. scene colour of the SKY pixels (invalid depth or the far plane) -> 16x16 tile sums,
//! 4. tile sums + previous state -> the new 3x1 state (ping-pong pair): texel 0 = (fog colour F, confidence),
//!    texel 1 = (zref, far fraction, haze age, haze source), texel 2 = (haze colour H, confidence) for the aerial
//!    perspective. With fewer than 0.5 % far pixels the confidence of F falls to 0 = F unknown. H is the horizon band
//!    of the sky, HELD when no sky is in view (F alone would be the far geometry's own colour then: no haze when the
//!    camera looks down), a neutral fallback from F before any sky was seen.
//!
//! The state is bound as `t3` and the depth as `t4` of the light-adjust substitute, which turns them into a per-pixel
//! fog amount and releases the occlusion by it. Unbound (feature off, AO did not run, any failure) both read zero,
//! which means "F unknown": the substitute then keeps only its blue-water rule.
//!
//! Shaders are compiled once on a background thread (never `D3DCompile` on the render thread).

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};

use crate::d3d::{IgnoreScope, Obj, StateGuard, Tex2dDesc, Viewport, release, vfn, vt};
use crate::log;
use crate::{ao, shaders};

pub const HLSL: &str = include_str!("../shaders/fog.hlsl");
pub const VS_ENTRY: &str = "vs_main";
pub const PS_ZTILE: &str = "ps_ztile";
pub const PS_ZREDUCE: &str = "ps_zreduce";
pub const PS_FTILE: &str = "ps_ftile";
pub const PS_FSTATE: &str = "ps_fstate";
pub const PS_STILE: &str = "ps_stile";

/// A pixel is far when its depth is at least this fraction of the frame's largest valid depth (or invalid).
pub const FAR_RATIO: f32 = 0.9;
/// F is known only when at least this share of the pixels is far.
pub const MIN_FAR_FRACTION: f32 = 0.005;
/// Per-frame blend of the confidence towards 1 / 0.
pub const CONF_RISE: f32 = 0.5;
pub const CONF_FALL: f32 = 0.15;
/// Per-frame blend of F and of zref towards the measurement.
pub const FOLLOW_F: f32 = 0.25;
pub const FOLLOW_Z: f32 = 0.35;
/// Size of the tile grid (the shader hard-codes 16).
pub const TILES: u32 = 16;

/// Haze colour of the aerial perspective (texel 2 of the state, `docs/aerial.md` "Haze colour"): the horizon band of the sky,
/// held when no sky is in view. Hysteresis on the sky share of the picture: the sky becomes the source at this share...
pub const SKY_ENTER: f32 = 0.004;
/// ...and stays the source down to this one.
pub const SKY_STAY: f32 = 0.001;
/// Per-frame blend of the haze colour (and its confidence) toward a new sky measurement or the fallback.
pub const FOLLOW_H: f32 = 0.1;
/// Per-frame drift of a HELD haze colour toward the fallback (about a minute at 60 fps to go half way): only so a new map
/// first seen without sky eventually adapts; slow enough to be invisible while the camera looks down.
pub const HOLD_DRIFT: f32 = 0.0002;
/// Normalised depth from which a pixel is sky for the haze colour (the engine writes 1.0 there; `aerial::SKY_DEPTH`).
pub const SKY_DEPTH: f32 = crate::aerial::SKY_DEPTH;
/// Confidence of the fallback haze colour (no sky seen yet): the haze is weaker while it is only a guess.
pub const FALLBACK_CONF: f32 = 0.6;
/// A tile is part of the horizon band when at least this share of its samples is sky.
pub const BAND_MIN: f32 = 0.02;

/// Source of the haze colour (texel 1 .w of the state).
#[allow(dead_code)] // documents the value 0 (the status' default arm)
pub const HAZE_UNKNOWN: u32 = 0;
pub const HAZE_SKY: u32 = 1;
pub const HAZE_HELD: u32 = 2;
pub const HAZE_FALLBACK: u32 = 3;

/// The last state read back (lazily, never waiting): haze source, age in frames, confidence, sky share, colour.
static HAZE_SRC: AtomicU32 = AtomicU32::new(u32::MAX);
static HAZE_AGE: AtomicU32 = AtomicU32::new(0);
static HAZE_CONF: AtomicU32 = AtomicU32::new(0);
static HAZE_RGB: [AtomicU32; 3] = [AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0)];

/// What the haze colour is made from, as last read back from the GPU (None before the first read-back).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HazeInfo {
    pub source: u32,
    pub age_frames: f32,
    pub confidence: f32,
    pub colour: [f32; 3],
}

pub fn haze_info() -> Option<HazeInfo> {
    let s = HAZE_SRC.load(Relaxed);
    if s == u32::MAX {
        return None;
    }
    let f = |a: &AtomicU32| f32::from_bits(a.load(Relaxed));
    Some(HazeInfo { source: s, age_frames: f(&HAZE_AGE), confidence: f(&HAZE_CONF), colour: [f(&HAZE_RGB[0]), f(&HAZE_RGB[1]), f(&HAZE_RGB[2])] })
}

/// Stores a read-back state (texel 1, texel 2).
fn note_haze(t1: [f32; 4], t2: [f32; 4]) {
    if !(t1.iter().chain(t2.iter()).all(|v| v.is_finite())) {
        return;
    }
    HAZE_AGE.store(t1[2].to_bits(), Relaxed);
    HAZE_CONF.store(t2[3].to_bits(), Relaxed);
    for c in 0..3 {
        HAZE_RGB[c].store(t2[c].to_bits(), Relaxed);
    }
    HAZE_SRC.store(t1[3].round().clamp(0.0, 3.0) as u32, Relaxed);
}

/// "sky (horizon band)" / "held 120 frames" / "fallback (no sky seen yet)" for the status line.
pub fn haze_text(h: Option<HazeInfo>) -> String {
    let Some(h) = h else { return "haze colour not read back yet".to_string() };
    let rgb = format!("({:.2}, {:.2}, {:.2})", h.colour[0], h.colour[1], h.colour[2]);
    match h.source {
        HAZE_SKY => format!("haze colour from the sky (horizon band) {rgb}, confidence {:.2}", h.confidence),
        HAZE_HELD => format!("haze colour held from the sky seen {:.0} frames ago {rgb}, confidence {:.2}", h.age_frames, h.confidence),
        HAZE_FALLBACK => format!("haze colour fallback (no sky seen yet: neutral from the fog colour) {rgb}, confidence {:.2}", h.confidence),
        _ => "haze colour unknown (no sky seen and no fog colour): no haze".to_string(),
    }
}

/// Slots of the light-adjust substitute: the 2x1 state and the linear depth.
pub const STATE_SLOT: u32 = 3;
pub const DEPTH_SLOT: u32 = 4;

const SLOT_FIRST: u32 = 10;
const SLOT_COUNT: u32 = 6; // t10..t15
/// Width of the state texture: texel 0 fog colour, texel 1 zref + haze age / source, texel 2 haze colour.
pub const STATE_W: u32 = 3;

const FMT_RGBA32F: u32 = 2;
const FMT_R32_FLOAT: u32 = 41;

static FAILED: AtomicBool = AtomicBool::new(false);
static DONE: AtomicBool = AtomicBool::new(false);
/// t3 / t4 are currently bound on the game's context (so they can be unbound when the feature stops).
static BOUND: AtomicBool = AtomicBool::new(false);

/// A new frame starts: allows the passes to run again.
pub fn frame_begin() {
    DONE.store(false, Relaxed);
}

/// The four constant-buffer vectors of `fog.hlsl` (see its header).
pub fn cb_values() -> [[f32; 4]; 4] {
    [[FAR_RATIO, MIN_FAR_FRACTION, CONF_RISE, CONF_FALL], [FOLLOW_F, FOLLOW_Z, 0.0, 0.0], [SKY_ENTER, SKY_STAY, FOLLOW_H, HOLD_DRIFT], [SKY_DEPTH, FALLBACK_CONF, BAND_MIN, 0.0]]
}

// ---- shaders (compiled once, in the background) -----------------------------------------------------

pub struct Blobs {
    pub vs: Vec<u8>,
    pub ztile: Vec<u8>,
    pub zreduce: Vec<u8>,
    pub ftile: Vec<u8>,
    pub fstate: Vec<u8>,
    pub stile: Vec<u8>,
}

/// Compiles the shaders (calls D3DCompile: never on the render thread).
pub fn compile_blobs() -> Result<Blobs, String> {
    let c = |entry: &str, profile: &str| shaders::compile(HLSL, entry, profile).map_err(|e| format!("{entry}: {e}"));
    Ok(Blobs { vs: c(VS_ENTRY, "vs_5_0")?, ztile: c(PS_ZTILE, "ps_5_0")?, zreduce: c(PS_ZREDUCE, "ps_5_0")?, ftile: c(PS_FTILE, "ps_5_0")?, fstate: c(PS_FSTATE, "ps_5_0")?, stile: c(PS_STILE, "ps_5_0")? })
}

static BLOBS: OnceLock<Result<Blobs, String>> = OnceLock::new();

/// Starts the background compile.
pub fn init() {
    let _ = std::thread::Builder::new().name("gw2g-fog-compile".into()).spawn(|| {
        let r = compile_blobs();
        match &r {
            Ok(_) => log!("fog: shaders compiled"),
            Err(e) => log!("fog: shader compile failed, the AO keeps only the blue-water rule: {e}"),
        }
        let _ = BLOBS.set(r);
    });
}

// ---- device objects ---------------------------------------------------------------------------------

/// A texture with its shader-resource and render-target views.
#[derive(Clone, Copy)]
struct Target {
    tex: Obj,
    srv: Obj,
    rtv: Obj,
}

const NO_TARGET: Target = Target { tex: std::ptr::null_mut(), srv: std::ptr::null_mut(), rtv: std::ptr::null_mut() };

impl Target {
    unsafe fn free(&mut self) {
        unsafe {
            release(self.rtv);
            release(self.srv);
            release(self.tex);
        }
        *self = NO_TARGET;
    }
}

#[repr(C)]
struct SubresourceData {
    mem: *const c_void,
    pitch: u32,
    slice: u32,
}

type CreateShaderFn = unsafe extern "system" fn(Obj, *const u8, usize, Obj, *mut Obj) -> i32;
type CreateViewFn = unsafe extern "system" fn(Obj, Obj, *const [u32; 6], *mut Obj) -> i32;

/// A w x h RGBA32F render-target + shader-resource texture, zero-filled when `zero`.
unsafe fn make_target(dev: Obj, w: u32, h: u32, zero: bool) -> Option<Target> {
    let mut t = NO_TARGET;
    unsafe {
        let desc = Tex2dDesc { width: w, height: h, mips: 1, array: 1, format: FMT_RGBA32F, samples: 1, quality: 0, usage: 0, bind: 0x28, cpu: 0, misc: 0 };
        let create_tex: unsafe extern "system" fn(Obj, *const Tex2dDesc, *const c_void, *mut Obj) -> i32 = std::mem::transmute(vfn(dev, vt::DEV_CREATE_TEXTURE2D));
        let create_srv: CreateViewFn = std::mem::transmute(vfn(dev, vt::DEV_CREATE_SRV));
        let create_rtv: CreateViewFn = std::mem::transmute(vfn(dev, vt::DEV_CREATE_RTV));
        let zeros = vec![0u8; (w * h * 16) as usize];
        let sd = SubresourceData { mem: zeros.as_ptr().cast(), pitch: w * 16, slice: 0 };
        let init: *const c_void = if zero { (&sd as *const SubresourceData).cast() } else { std::ptr::null() };
        let sdesc: [u32; 6] = [FMT_RGBA32F, 4, 0, 1, 0, 0];
        let rdesc: [u32; 6] = [FMT_RGBA32F, 4, 0, 0, 0, 0];
        if create_tex(dev, &desc, init, &mut t.tex) < 0 || t.tex.is_null() || create_srv(dev, t.tex, &sdesc, &mut t.srv) < 0 || create_rtv(dev, t.tex, &rdesc, &mut t.rtv) < 0 {
            t.free();
            return None;
        }
    }
    Some(t)
}

/// Device objects of the fog estimate, valid for one device only.
pub struct Fog {
    device: usize,
    vs: Obj,
    ps_ztile: Obj,
    ps_zreduce: Obj,
    ps_ftile: Obj,
    ps_fstate: Obj,
    ps_stile: Obj,
    cb: Obj,
    ztile: Target,
    zstat: Target,
    ftile: Target,
    stile: Target,
    /// The 3x1 state, two sets (ping-pong): texel 0 = (F rgb, confidence), texel 1 = (zref, far fraction, haze age, haze
    /// source), texel 2 = (haze colour H rgb, haze confidence).
    state: [Target; 2],
    cur: usize,
    /// Ring of 3x1 staging copies of the state for the status line (read with DO_NOT_WAIT, never stalls).
    stage: [Obj; 3],
    staged: [bool; 3],
    runs: u32,
    /// SRV of the depth resource it was made for.
    depth_srv: Obj,
    depth_key: usize,
}

// The pointers are only used under the mutex, from the render thread.
unsafe impl Send for Fog {}

impl Fog {
    /// Creates the shaders, the constant buffer and the targets on `device`.
    ///
    /// # Safety
    /// `device` must be a live `ID3D11Device`.
    pub unsafe fn new(device: Obj, b: &Blobs) -> Option<Fog> {
        let _ig = IgnoreScope::new();
        let nul = std::ptr::null_mut();
        let mut f = Fog { device: device as usize, vs: nul, ps_ztile: nul, ps_zreduce: nul, ps_ftile: nul, ps_fstate: nul, ps_stile: nul, cb: nul, ztile: NO_TARGET, zstat: NO_TARGET, ftile: NO_TARGET, stile: NO_TARGET, state: [NO_TARGET; 2], cur: 0, stage: [nul; 3], staged: [false; 3], runs: 0, depth_srv: nul, depth_key: 0 };
        unsafe {
            let create_vs: CreateShaderFn = std::mem::transmute(vfn(device, vt::DEV_CREATE_VERTEX_SHADER));
            let create_ps: CreateShaderFn = std::mem::transmute(vfn(device, vt::DEV_CREATE_PIXEL_SHADER));
            let ok = create_vs(device, b.vs.as_ptr(), b.vs.len(), nul, &mut f.vs) >= 0
                && create_ps(device, b.ztile.as_ptr(), b.ztile.len(), nul, &mut f.ps_ztile) >= 0
                && create_ps(device, b.zreduce.as_ptr(), b.zreduce.len(), nul, &mut f.ps_zreduce) >= 0
                && create_ps(device, b.ftile.as_ptr(), b.ftile.len(), nul, &mut f.ps_ftile) >= 0
                && create_ps(device, b.fstate.as_ptr(), b.fstate.len(), nul, &mut f.ps_fstate) >= 0
                && create_ps(device, b.stile.as_ptr(), b.stile.len(), nul, &mut f.ps_stile) >= 0;
            if !ok {
                log!("fog: could not create the shaders");
                return None; // Drop releases what exists
            }
            let create_buffer: unsafe extern "system" fn(Obj, *const [u32; 6], *const c_void, *mut Obj) -> i32 = std::mem::transmute(vfn(device, vt::DEV_CREATE_BUFFER));
            if create_buffer(device, &[64, 2, 4, 0x10000, 0, 0], std::ptr::null(), &mut f.cb) < 0 {
                log!("fog: could not create the constant buffer");
                return None;
            }
            let mut ok = true;
            for (slot, w, h, zero) in [(0usize, TILES, TILES, false), (1, 1, 1, false), (2, TILES, TILES, false), (3, STATE_W, 1, true), (4, STATE_W, 1, true), (5, TILES, TILES, false)] {
                match make_target(device, w, h, zero) {
                    Some(t) => match slot {
                        0 => f.ztile = t,
                        1 => f.zstat = t,
                        2 => f.ftile = t,
                        3 => f.state[0] = t,
                        4 => f.state[1] = t,
                        _ => f.stile = t,
                    },
                    None => ok = false,
                }
            }
            if !ok {
                log!("fog: could not create the targets");
                return None;
            }
            // Staging copies for the status read-back: optional (without them the status says "not read back yet").
            let create_tex: unsafe extern "system" fn(Obj, *const Tex2dDesc, *const c_void, *mut Obj) -> i32 = std::mem::transmute(vfn(device, vt::DEV_CREATE_TEXTURE2D));
            let sdesc = Tex2dDesc { width: STATE_W, height: 1, mips: 1, array: 1, format: FMT_RGBA32F, samples: 1, quality: 0, usage: 3 /* STAGING */, bind: 0, cpu: 0x20000 /* READ */, misc: 0 };
            for s in f.stage.iter_mut() {
                if create_tex(device, &sdesc, std::ptr::null(), s) < 0 {
                    *s = nul;
                }
            }
        }
        Some(f)
    }

    /// SRV of the depth resource, cached while the resource is the same.
    unsafe fn ensure_depth_srv(&mut self, dev: Obj, depth: Obj) -> bool {
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

    /// SRV of the current 2x1 state (valid after `run`).
    #[allow(dead_code)] // used by the tests; production binds it through `bind`
    pub fn state_srv(&self) -> Obj {
        self.state[self.cur].srv
    }

    /// SRV of the depth the last `run` used.
    #[allow(dead_code)] // used by the tests
    pub fn depth_view(&self) -> Obj {
        self.depth_srv
    }

    /// Runs the four draws from the scene colour `scene_srv` and the linear `depth`. All pipeline state touched is
    /// restored. False when something failed before the first draw.
    ///
    /// # Safety
    /// All pointers must be live objects of the device this was created on; `scene_srv` a view of a colour texture,
    /// `depth` an R32_FLOAT texture with a shader-resource bind.
    pub unsafe fn run(&mut self, dev: Obj, ctx: Obj, scene_srv: Obj, depth: Obj) -> bool {
        let _ig = IgnoreScope::new();
        unsafe {
            if !self.ensure_depth_srv(dev, depth) {
                return false;
            }
            let mut mapped: [usize; 3] = [0; 3]; // D3D11_MAPPED_SUBRESOURCE { pData, RowPitch, DepthPitch }
            let map: unsafe extern "system" fn(Obj, Obj, u32, u32, u32, *mut [usize; 3]) -> i32 = std::mem::transmute(vfn(ctx, vt::CTX_MAP));
            let unmap: unsafe extern "system" fn(Obj, Obj, u32) = std::mem::transmute(vfn(ctx, vt::CTX_UNMAP));
            if map(ctx, self.cb, 0, 4 /* WRITE_DISCARD */, 0, &mut mapped) < 0 || mapped[0] == 0 {
                return false;
            }
            *(mapped[0] as *mut [[f32; 4]; 4]) = cb_values();
            unmap(ctx, self.cb, 0);

            let _state = StateGuard::save(ctx);
            let vp = |w: u32, h: u32| Viewport { x: 0.0, y: 0.0, w: w as f32, h: h as f32, min_depth: 0.0, max_depth: 1.0 };
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
            let nul: Obj = std::ptr::null_mut();
            // The slots this module uses besides t0: t10..t14.
            let clear = || set_srv(ctx, SLOT_FIRST, SLOT_COUNT, [nul; SLOT_COUNT as usize].as_ptr());

            set_rs(ctx, nul);
            set_depth(ctx, nul, 0);
            set_blend(ctx, nul, factor.as_ptr(), 0xffff_ffff);
            set_layout(ctx, nul);
            set_topology(ctx, 4 /* TRIANGLELIST */);
            set_vs(ctx, self.vs, std::ptr::null(), 0);
            set_gs(ctx, nul, std::ptr::null(), 0);
            set_cb(ctx, 0, 1, &self.cb);

            let new = 1 - self.cur;
            // 1: depth -> tiles of the largest valid depth.
            clear();
            set_rt(ctx, 1, &self.ztile.rtv, nul);
            set_vp(ctx, 1, &vp(TILES, TILES));
            set_ps(ctx, self.ps_ztile, std::ptr::null(), 0);
            set_srv(ctx, 10, 1, &self.depth_srv);
            draw(ctx, 3, 0);
            // 2: tiles -> zref.
            clear();
            set_rt(ctx, 1, &self.zstat.rtv, nul);
            set_vp(ctx, 1, &vp(1, 1));
            set_ps(ctx, self.ps_zreduce, std::ptr::null(), 0);
            set_srv(ctx, 11, 1, &self.ztile.srv);
            draw(ctx, 3, 0);
            // 3: scene colour of the far pixels -> tile sums.
            clear();
            set_rt(ctx, 1, &self.ftile.rtv, nul);
            set_vp(ctx, 1, &vp(TILES, TILES));
            set_ps(ctx, self.ps_ftile, std::ptr::null(), 0);
            set_srv(ctx, 0, 1, &scene_srv);
            set_srv(ctx, 10, 1, &self.depth_srv);
            set_srv(ctx, 12, 1, &self.zstat.srv);
            draw(ctx, 3, 0);
            // 3b: scene colour of the sky pixels -> tile sums (the haze colour of the aerial perspective).
            clear();
            set_rt(ctx, 1, &self.stile.rtv, nul);
            set_ps(ctx, self.ps_stile, std::ptr::null(), 0);
            set_srv(ctx, 0, 1, &scene_srv);
            set_srv(ctx, 10, 1, &self.depth_srv);
            draw(ctx, 3, 0);
            // 4: tile sums + previous state -> new state.
            clear();
            set_rt(ctx, 1, &self.state[new].rtv, nul);
            set_vp(ctx, 1, &vp(STATE_W, 1));
            set_ps(ctx, self.ps_fstate, std::ptr::null(), 0);
            set_srv(ctx, 11, 5, [self.ztile.srv, self.zstat.srv, self.ftile.srv, self.state[self.cur].srv, self.stile.srv].as_ptr());
            draw(ctx, 3, 0);
            self.cur = new;
            // Leave nothing of ours bound (the guard restores t0, t1 and b0).
            clear();
            set_rt(ctx, 0, std::ptr::null(), nul);
            self.read_back(ctx);
        }
        true
    }

    /// Copies the state into the staging ring and reads the oldest copy if the GPU is done with it (never waits); the
    /// result feeds the status line only.
    unsafe fn read_back(&mut self, ctx: Obj) {
        unsafe {
            let i = self.runs as usize % 3;
            self.runs = self.runs.wrapping_add(1);
            if self.stage[i].is_null() {
                return;
            }
            let copy: unsafe extern "system" fn(Obj, Obj, Obj) = std::mem::transmute(vfn(ctx, vt::CTX_COPY_RESOURCE));
            copy(ctx, self.stage[i], self.state[self.cur].tex);
            self.staged[i] = true;
            let j = (i + 1) % 3;
            if !self.staged[j] || self.stage[j].is_null() {
                return;
            }
            let map: unsafe extern "system" fn(Obj, Obj, u32, u32, u32, *mut [usize; 3]) -> i32 = std::mem::transmute(vfn(ctx, vt::CTX_MAP));
            let unmap: unsafe extern "system" fn(Obj, Obj, u32) = std::mem::transmute(vfn(ctx, vt::CTX_UNMAP));
            let mut mapped: [usize; 3] = [0; 3];
            // READ, DO_NOT_WAIT: fails with "still drawing" instead of stalling.
            if map(ctx, self.stage[j], 0, 1, 0x10_0000, &mut mapped) >= 0 && mapped[0] != 0 {
                let p = mapped[0] as *const [f32; 4];
                let (t1, t2) = (*p.add(1), *p.add(2));
                unmap(ctx, self.stage[j], 0);
                note_haze(t1, t2);
            }
        }
    }

    /// Binds the state as `t3` and the depth as `t4` on `ctx`.
    ///
    /// # Safety
    /// `ctx` must be a live immediate context of this device.
    pub unsafe fn bind(&self, ctx: Obj) {
        unsafe {
            type SetSlots = unsafe extern "system" fn(Obj, u32, u32, *const Obj);
            let set_srv: SetSlots = std::mem::transmute(vfn(ctx, vt::CTX_PS_SET_SRVS));
            set_srv(ctx, STATE_SLOT, 1, &self.state[self.cur].srv);
            set_srv(ctx, DEPTH_SLOT, 1, &self.depth_srv);
        }
    }
}

/// Unbinds `t3` and `t4` (they then read zero: F unknown, no fog release).
///
/// # Safety
/// `ctx` must be a live device context.
pub unsafe fn unbind(ctx: Obj) {
    unsafe {
        type SetSlots = unsafe extern "system" fn(Obj, u32, u32, *const Obj);
        let set_srv: SetSlots = std::mem::transmute(vfn(ctx, vt::CTX_PS_SET_SRVS));
        let nul: Obj = std::ptr::null_mut();
        set_srv(ctx, STATE_SLOT, 1, &nul);
        set_srv(ctx, DEPTH_SLOT, 1, &nul);
    }
}

impl Drop for Fog {
    fn drop(&mut self) {
        // Objects of a device that is gone are children that keep it alive; releasing them is safe.
        unsafe {
            self.ztile.free();
            self.zstat.free();
            self.ftile.free();
            self.stile.free();
            for t in self.state.iter_mut() {
                t.free();
            }
            for o in self.stage {
                release(o);
            }
            for o in [self.depth_srv, self.cb, self.ps_stile, self.ps_fstate, self.ps_ftile, self.ps_zreduce, self.ps_ztile, self.vs] {
                release(o);
            }
        }
    }
}

// ---- per-frame glue (called from datapath::bind_current) ------------------------------------------------

static FOG: Mutex<Option<Fog>> = Mutex::new(None);

fn log_once(flag: &'static AtomicBool, msg: impl FnOnce() -> String) {
    if !flag.swap(true, Relaxed) {
        log!("{}", msg());
    }
}

/// The light-adjust substitute is about to draw on the game's context: when the AO ran this frame (`ao_ran`),
/// estimates the fog colour and the reference depth (once per frame) and binds them as `t3` / `t4`; otherwise takes
/// them away. Never unwinds; every failure is logged once and leaves the frame with the blue-water rule only.
pub fn on_light_adjust(ctx: Obj, dev: Obj, ao_ran: bool) {
    let _ = std::panic::catch_unwind(|| unsafe { on_light_adjust_inner(ctx, dev, ao_ran) });
}

unsafe fn on_light_adjust_inner(ctx: Obj, dev: Obj, ao_ran: bool) {
    static NO_DEPTH: AtomicBool = AtomicBool::new(false);
    static BAD_SCENE: AtomicBool = AtomicBool::new(false);
    static NO_RUN: AtomicBool = AtomicBool::new(false);
    static FIRST: AtomicBool = AtomicBool::new(false);
    let _ig = IgnoreScope::new();
    let off = || {
        if BOUND.swap(false, Relaxed) {
            unsafe { unbind(ctx) };
        }
    };
    if !ao_ran || FAILED.load(Relaxed) || !ao::enabled() {
        off();
        return;
    }
    let blobs = match BLOBS.get() {
        None => {
            off();
            return;
        }
        Some(Err(_)) => {
            FAILED.store(true, Relaxed);
            off();
            return;
        }
        Some(Ok(b)) => b,
    };
    let Ok(mut guard) = FOG.try_lock() else { return };
    if guard.as_ref().is_some_and(|x| x.device != dev as usize) {
        *guard = None;
    }
    if guard.is_none() {
        match unsafe { Fog::new(dev, blobs) } {
            Some(x) => *guard = Some(x),
            None => {
                FAILED.store(true, Relaxed);
                log!("fog: could not be created; the AO keeps only the blue-water rule");
                off();
                return;
            }
        }
    }
    let fog = guard.as_mut().expect("just created");
    if !DONE.load(Relaxed) {
        let Some(depth) = ao::depth_resource() else {
            log_once(&NO_DEPTH, || "fog: skipped, the linear depth of this frame is not available".to_string());
            off();
            return;
        };
        // The scene colour: the pixel shader SRV in slot 0 of the light-adjust draw.
        let mut scene_srv: Obj = std::ptr::null_mut();
        unsafe {
            let get: unsafe extern "system" fn(Obj, u32, u32, *mut Obj) = std::mem::transmute(vfn(ctx, vt::CTX_PS_GET_SRVS));
            get(ctx, 0, 1, &mut scene_srv);
        }
        if scene_srv.is_null() {
            log_once(&BAD_SCENE, || "fog: skipped, no scene colour is bound in slot 0".to_string());
            off();
            return;
        }
        let Some((res, d)) = (unsafe { ao::texture_of(scene_srv) }) else {
            unsafe { release(scene_srv) };
            log_once(&BAD_SCENE, || "fog: skipped, the scene colour in slot 0 is not a 2D texture".to_string());
            off();
            return;
        };
        unsafe { release(res) };
        if d.samples != 1 || d.array != 1 || d.width < 16 || d.height < 16 {
            unsafe { release(scene_srv) };
            log_once(&BAD_SCENE, || format!("fog: skipped, the scene colour is {}x{} format {} samples {}", d.width, d.height, d.format, d.samples));
            off();
            return;
        }
        let ok = unsafe { fog.run(dev, ctx, scene_srv, depth) };
        unsafe { release(scene_srv) };
        if !ok {
            log_once(&NO_RUN, || "fog: the depth view or the constants could not be set up; frame left with the blue-water rule only".to_string());
            off();
            return;
        }
        DONE.store(true, Relaxed);
        log_once(&FIRST, || format!("fog: first frame estimated (scene {}x{})", d.width, d.height));
    }
    unsafe { fog.bind(ctx) };
    BOUND.store(true, Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants_follow_the_tuning() {
        let c = cb_values();
        assert_eq!(c[0], [FAR_RATIO, MIN_FAR_FRACTION, CONF_RISE, CONF_FALL]);
        assert_eq!(c[1], [FOLLOW_F, FOLLOW_Z, 0.0, 0.0]);
        assert_eq!(c[2], [SKY_ENTER, SKY_STAY, FOLLOW_H, HOLD_DRIFT]);
        assert_eq!(c[3][..3], [SKY_DEPTH, FALLBACK_CONF, BAND_MIN]);
        assert!(SKY_STAY < SKY_ENTER, "hysteresis");
        assert!(c.iter().flatten().all(|v| v.is_finite()));
    }
}

#[cfg(test)]
#[path = "fog_tests.rs"]
mod gpu_tests;
