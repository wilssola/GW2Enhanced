//! HDR resolve: just before the UI pass starts, the finished scene (float16, gamma-encoded, may hold
//! values above 1) is tone-mapped in place, so the UI is drawn on a display-referred buffer.
//!
//! In place = a scratch copy (`CopyResource`) of the scene texture is read by a full-screen triangle
//! that writes back into the scene's own render-target view. The output is still gamma-encoded and
//! lies in [0, h]; `h` is the headroom (`set_headroom`, 1.0 = SDR). No paper-white scaling here: a
//! later pass converts after the UI.
//!
//! The decisions (`decide`, `wants`) are plain code; `Resolver` talks to D3D through raw vtables like
//! `d3d.rs` and is exercised on WARP by the tests in `hdr_shaders_tests.rs`.

use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering::Relaxed};

use crate::d3d::{IgnoreScope, Obj, RESOURCE_TEXTURE2D, StateGuard, Tex2dDesc, Viewport, release, vfn, vt};
use crate::log;
use crate::passes::{FMT_B8G8R8A8_UNORM, FMT_R16G16B16A16_FLOAT, Target};
use crate::shaders;
use crate::tonemap::ToneParams;

pub const HLSL: &str = include_str!("../shaders/resolve.hlsl");
pub const VS_ENTRY: &str = "vs_main";
pub const PS_ENTRY: &str = "ps_main";
/// Default knee: this fraction of the headroom.
pub const SHOULDER_START: f32 = 0.8;

// ---- switches --------------------------------------------------------------------------------

static ENABLED: AtomicBool = AtomicBool::new(false);
static FAILED: AtomicBool = AtomicBool::new(false);
static DONE: AtomicBool = AtomicBool::new(false);
static HEADROOM: AtomicU32 = AtomicU32::new(0x3f80_0000); // 1.0f32
static SELF_CHECK_LOGGED: AtomicBool = AtomicBool::new(false);

/// Turned on and off at the frame boundary by the enable gate.
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Relaxed);
}

/// On, and no unrecoverable failure so far.
pub fn enabled() -> bool {
    ENABLED.load(Relaxed) && !FAILED.load(Relaxed)
}

/// Debug: reads back one pixel of `res` (GPU stall, only used while the debug fill is on) and logs it
/// together with the resource pointer, so a draw that did not land can be told from a texture that
/// is not the one displayed.
pub(crate) unsafe fn probe(dev: Obj, ctx: Obj, res: Obj, d: &Tex2dDesc, x: u32, y: u32, what: &str) {
    unsafe {
        let sd = Tex2dDesc { width: 1, height: 1, mips: 1, array: 1, format: d.format, samples: 1, quality: 0, usage: 3 /* STAGING */, bind: 0, cpu: 0x20000 /* READ */, misc: 0 };
        let create: unsafe extern "system" fn(Obj, *const Tex2dDesc, *const c_void, *mut Obj) -> i32 = std::mem::transmute(vfn(dev, vt::DEV_CREATE_TEXTURE2D));
        let mut st: Obj = std::ptr::null_mut();
        if create(dev, &sd, std::ptr::null(), &mut st) < 0 || st.is_null() {
            log!("probe {what}: could not create the staging texture");
            return;
        }
        let bx: [u32; 6] = [x, y, 0, x + 1, y + 1, 1];
        let copy: unsafe extern "system" fn(Obj, Obj, u32, u32, u32, u32, Obj, u32, *const [u32; 6]) = std::mem::transmute(vfn(ctx, vt::CTX_COPY_SUBRESOURCE_REGION));
        copy(ctx, st, 0, 0, 0, 0, res, 0, &bx);
        let mut mapped: [usize; 3] = [0; 3];
        let map: unsafe extern "system" fn(Obj, Obj, u32, u32, u32, *mut [usize; 3]) -> i32 = std::mem::transmute(vfn(ctx, vt::CTX_MAP));
        if map(ctx, st, 0, 1 /* READ */, 0, &mut mapped) >= 0 && mapped[0] != 0 {
            let p = mapped[0] as *const u16;
            let f = |h: u16| -> f32 {
                let (s, e, m) = ((h >> 15) as i32, ((h >> 10) & 31) as i32, (h & 1023) as f32);
                let v = if e == 0 { m * 2f32.powi(-24) } else { (1.0 + m / 1024.0) * 2f32.powi(e - 15) };
                if s == 1 { -v } else { v }
            };
            if d.format == FMT_R16G16B16A16_FLOAT {
                log!("probe {what}: resource {:p} fmt {} pixel({x},{y}) = [{:.3}, {:.3}, {:.3}, {:.3}]", res, d.format, f(*p), f(*p.add(1)), f(*p.add(2)), f(*p.add(3)));
            } else {
                log!("probe {what}: resource {:p} fmt {} (not float, raw {:#06x})", res, d.format, *p);
            }
            let unmap: unsafe extern "system" fn(Obj, Obj, u32) = std::mem::transmute(vfn(ctx, vt::CTX_UNMAP));
            unmap(ctx, st, 0);
        }
        release(st);
    }
}

static FRAMES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Frames the resolve has been applied to so far (the watchdog's heartbeat).
pub fn frames_resolved() -> u64 {
    FRAMES.load(Relaxed)
}

static DEBUG: AtomicBool = AtomicBool::new(false);
static DEBUG_FILL: AtomicBool = AtomicBool::new(false);

/// Debug fill: the resolve paints the left half of the target solid red and the AO the top half
/// black, regardless of the scene, to tell "does not reach the screen" from "has no visible effect".
pub fn set_debug_fill(on: bool) {
    DEBUG_FILL.store(on, Relaxed);
}

pub fn debug_fill() -> bool {
    DEBUG_FILL.load(Relaxed)
}

static DEBUG_MATERIALS: AtomicBool = AtomicBool::new(false);

/// Debug materials: the light-adjust shader tints every pixel by the class of its raw scene alpha (low blue, mid
/// green, high red), so the player can find which class is metal / water / stone (see `docs/ssr.md`).
pub fn set_debug_materials(on: bool) {
    DEBUG_MATERIALS.store(on, Relaxed);
}

pub fn debug_materials() -> bool {
    DEBUG_MATERIALS.load(Relaxed)
}

static DEBUG_SUN: AtomicBool = AtomicBool::new(false);

/// Debug sun (level 4): the light-adjust shader tints the sky cyan and the sun-shaft light yellow, a magenta cross
/// marks the estimated sun and `shafts.rs` logs the sun state once per second (see `docs/shafts.md`).
pub fn set_debug_sun(on: bool) {
    DEBUG_SUN.store(on, Relaxed);
}

pub fn debug_sun() -> bool {
    DEBUG_SUN.load(Relaxed)
}

static DEBUG_REFLECTIVITY: AtomicBool = AtomicBool::new(false);

/// Debug reflectivity: the light-adjust shader shows the SSR reflectivity score as a heat map (blue 0 .. red 1)
/// over a dimmed scene (level 5; see `docs/ssr.md`).
pub fn set_debug_reflectivity(on: bool) {
    DEBUG_REFLECTIVITY.store(on, Relaxed);
}

pub fn debug_reflectivity() -> bool {
    DEBUG_REFLECTIVITY.load(Relaxed)
}

static DEBUG_VIEW: AtomicU8 = AtomicU8::new(0);

/// Highest debug level the config key `debug_view` (0..=5) can ask for.
pub const DEBUG_VIEW_MAX: u8 = 5;

/// The debug level asked for from OUTSIDE the overlay (`debug_view` in config.ini, applied by the hot reload, or the overlay's
/// "Debug view" combo): one more source of `debug_level`, so the developer can switch the heat maps without touching the
/// checkboxes. Clamped to 0..=5; never saved (`Config::to_ini` writes `debug_view=0`) and not applied at start.
pub fn set_debug_view(v: u8) -> u8 {
    let v = v.min(DEBUG_VIEW_MAX);
    DEBUG_VIEW.store(v, Relaxed);
    v
}

pub fn debug_view() -> u8 {
    DEBUG_VIEW.load(Relaxed)
}

/// The ONE place that defines the precedence of the debug modes: 0 off, 1 debug view, 2 debug fill, 3 debug
/// materials, 4 debug sun, 5 debug reflectivity. The highest active level wins (the checkboxes of the overlay and the
/// `debug_view` key are sources of the same level); everything that reacts to a debug mode must ask this function.
pub fn debug_level() -> u8 {
    merge_level(debug_reflectivity(), debug_sun(), debug_materials(), debug_fill(), debug(), debug_view())
}

/// The pure core of `debug_level`: the states of the five overlay checkboxes (precedence 5 reflectivity, 4 sun, 3 materials,
/// 2 fill, 1 view) and the `debug_view` key merged, the highest level wins.
pub fn merge_level(reflectivity: bool, sun: bool, materials: bool, fill: bool, view: bool, key: u8) -> u8 {
    let boxes = if reflectivity {
        5
    } else if sun {
        4
    } else if materials {
        3
    } else if fill {
        2
    } else if view {
        1
    } else {
        0
    };
    boxes.max(key.min(DEBUG_VIEW_MAX))
}

/// The in-place resolve PASS is retired. It was inserted between the engine's passes and, as measured in the
/// real game, the engine overwrote its target before the final pass (see `datapath.rs`). Its job is done
/// inside the substituted shaders now: the boost in `light_adjust_hdr.hlsl`, the tone shoulder, paper white
/// and peak in `final_scrgb.hlsl`. The code stays for its tests and as a reference of the curve.
pub const PASS_ENABLED: bool = false;

/// Debug view: the resolve paints the pixels the highlight boost changes red and the AO shows its
/// occlusion as black, so a feature that has no visible effect can be told apart from one that does
/// not run. Not saved in the config.
pub fn set_debug(on: bool) {
    DEBUG.store(on, Relaxed);
}

pub fn debug() -> bool {
    DEBUG.load(Relaxed)
}

static HIGHLIGHT: AtomicU32 = AtomicU32::new(0x3f80_0000); // 1.0f32

/// Highlight boost for HDR output (1..4); 1 = none. Applied by the resolve shader to scene values
/// above SDR white.
pub fn set_highlight(g: f32) {
    let g = if g.is_finite() { g.clamp(1.0, 4.0) } else { 1.0 };
    HIGHLIGHT.store(g.to_bits(), Relaxed);
}

pub fn highlight() -> f32 {
    crate::toggles::eff_or(crate::toggles::Fx::Highlight, f32::from_bits(HIGHLIGHT.load(Relaxed)), 1.0)
}

/// Output headroom (linear, relative to SDR white). Values below 1 or not finite mean SDR.
pub fn set_headroom(h: f32) {
    let h = if h.is_finite() && h >= 1.0 { h } else { 1.0 };
    HEADROOM.store(h.to_bits(), Relaxed);
}

pub fn headroom() -> f32 {
    f32::from_bits(HEADROOM.load(Relaxed))
}

/// A new frame starts: the resolve may run once again.
pub fn frame_begin() {
    DONE.store(false, Relaxed);
}

/// Cheap test for the hook (called when the recorder says a UI pass starts): the resolve is on and
/// has not run this frame.
pub fn wants() -> bool {
    PASS_ENABLED && enabled() && !DONE.load(Relaxed)
}

fn params() -> ToneParams {
    ToneParams { shoulder_start: SHOULDER_START, peak: headroom(), paper_white: 1.0 }
}

// ---- decision --------------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    Run,
    /// The self-check failed; the string says why.
    SelfCheck(&'static str),
}

fn is_scene_colour(t: &Target, screen: (u32, u32)) -> bool {
    (t.fmt == FMT_R16G16B16A16_FLOAT || t.fmt == FMT_B8G8R8A8_UNORM) && (t.w, t.h) == screen
}

/// `rt0` is RT0 of the binding that starts the UI pass, `prev_rt0` RT0 of the pass before it.
pub fn decide(rt0: Target, prev_rt0: Option<Target>, screen: (u32, u32)) -> Decision {
    if rt0.fmt != FMT_R16G16B16A16_FLOAT || (rt0.w, rt0.h) != screen {
        return Decision::SelfCheck("the UI pass RT0 is not a full-size R16G16B16A16_FLOAT target");
    }
    if !prev_rt0.is_some_and(|t| is_scene_colour(&t, screen)) {
        return Decision::SelfCheck("the pass before the UI pass is not a full-size scene colour pass");
    }
    Decision::Run
}

pub fn log_self_check_once(why: &str) {
    if !SELF_CHECK_LOGGED.swap(true, Relaxed) {
        log!("resolve: skipped, {why}");
    }
}

// ---- the resolver ----------------------------------------------------------------------------

/// Device objects of the resolve, valid for one device only.
pub struct Resolver {
    device: usize,
    vs: Obj,
    ps: Obj,
    cb: Obj,
    scratch: Obj,
    scratch_srv: Obj,
    scratch_key: (u32, u32, u32),
}

// The pointers are only used under the RESOLVER mutex, from the render thread.
unsafe impl Send for Resolver {}

type CreateShaderFn = unsafe extern "system" fn(Obj, *const u8, usize, Obj, *mut Obj) -> i32;

impl Resolver {
    /// Compiles `resolve.hlsl` and creates the shaders and the constant buffer on `device`.
    ///
    /// # Safety
    /// `device` must be a live `ID3D11Device`.
    pub unsafe fn new(device: Obj) -> Option<Resolver> {
        let _ig = IgnoreScope::new();
        let vs_code = shaders::compile(HLSL, VS_ENTRY, "vs_5_0").map_err(|e| log!("resolve: vertex shader: {e}")).ok()?;
        let ps_code = shaders::compile(HLSL, PS_ENTRY, "ps_5_0").map_err(|e| log!("resolve: pixel shader: {e}")).ok()?;
        let mut r = Resolver { device: device as usize, vs: std::ptr::null_mut(), ps: std::ptr::null_mut(), cb: std::ptr::null_mut(), scratch: std::ptr::null_mut(), scratch_srv: std::ptr::null_mut(), scratch_key: (0, 0, 0) };
        unsafe {
            let create_vs: CreateShaderFn = std::mem::transmute(vfn(device, vt::DEV_CREATE_VERTEX_SHADER));
            let create_ps: CreateShaderFn = std::mem::transmute(vfn(device, vt::DEV_CREATE_PIXEL_SHADER));
            if create_vs(device, vs_code.as_ptr(), vs_code.len(), std::ptr::null_mut(), &mut r.vs) < 0 || create_ps(device, ps_code.as_ptr(), ps_code.len(), std::ptr::null_mut(), &mut r.ps) < 0 {
                log!("resolve: could not create the shaders");
                return None; // Drop releases what exists
            }
            // D3D11_BUFFER_DESC { ByteWidth, Usage = DYNAMIC, BindFlags = CONSTANT_BUFFER, CPUAccessFlags = WRITE, MiscFlags, StructureByteStride }
            let desc: [u32; 6] = [16, 2, 4, 0x10000, 0, 0];
            let create_buffer: unsafe extern "system" fn(Obj, *const [u32; 6], *const c_void, *mut Obj) -> i32 = std::mem::transmute(vfn(device, vt::DEV_CREATE_BUFFER));
            if create_buffer(device, &desc, std::ptr::null(), &mut r.cb) < 0 {
                log!("resolve: could not create the constant buffer");
                return None;
            }
        }
        Some(r)
    }

    /// (Re)creates the scratch copy when the size or format changed.
    unsafe fn ensure_scratch(&mut self, dev: Obj, d: &Tex2dDesc) -> bool {
        let key = (d.width, d.height, d.format);
        if !self.scratch.is_null() && self.scratch_key == key {
            return true;
        }
        unsafe {
            release(self.scratch_srv);
            release(self.scratch);
            self.scratch = std::ptr::null_mut();
            self.scratch_srv = std::ptr::null_mut();
            self.scratch_key = (0, 0, 0);
            let desc = Tex2dDesc { width: d.width, height: d.height, mips: 1, array: 1, format: d.format, samples: 1, quality: 0, usage: 0, bind: 0x8, cpu: 0, misc: 0 };
            let create_tex: unsafe extern "system" fn(Obj, *const Tex2dDesc, *const c_void, *mut Obj) -> i32 = std::mem::transmute(vfn(dev, vt::DEV_CREATE_TEXTURE2D));
            if create_tex(dev, &desc, std::ptr::null(), &mut self.scratch) < 0 || self.scratch.is_null() {
                self.scratch = std::ptr::null_mut();
                return false;
            }
            // D3D11_SHADER_RESOURCE_VIEW_DESC { Format, ViewDimension = TEXTURE2D, MostDetailedMip, MipLevels, (unused) }
            let sdesc: [u32; 6] = [FMT_R16G16B16A16_FLOAT, 4, 0, 1, 0, 0];
            let create_srv: unsafe extern "system" fn(Obj, Obj, *const [u32; 6], *mut Obj) -> i32 = std::mem::transmute(vfn(dev, vt::DEV_CREATE_SRV));
            if create_srv(dev, self.scratch, &sdesc, &mut self.scratch_srv) < 0 || self.scratch_srv.is_null() {
                release(self.scratch);
                self.scratch = std::ptr::null_mut();
                self.scratch_srv = std::ptr::null_mut();
                return false;
            }
        }
        self.scratch_key = key;
        log!("resolve: scratch copy {}x{} format {}", d.width, d.height, d.format);
        true
    }

    /// Copies `res` to the scratch texture and draws the tone-mapped result back through `rtv`.
    /// All pipeline state touched is restored. False when something failed before the draw.
    ///
    /// # Safety
    /// All pointers must be live objects of the device this resolver was created on.
    pub unsafe fn run(&mut self, dev: Obj, ctx: Obj, rtv: Obj, res: Obj, d: &Tex2dDesc, p: &ToneParams) -> bool {
        let _ig = IgnoreScope::new();
        unsafe {
            if !self.ensure_scratch(dev, d) {
                return false;
            }
            // constants: x = knee fraction, y = headroom
            let mut mapped: [usize; 3] = [0; 3]; // D3D11_MAPPED_SUBRESOURCE { pData, RowPitch, DepthPitch }
            let map: unsafe extern "system" fn(Obj, Obj, u32, u32, u32, *mut [usize; 3]) -> i32 = std::mem::transmute(vfn(ctx, vt::CTX_MAP));
            if map(ctx, self.cb, 0, 4 /* WRITE_DISCARD */, 0, &mut mapped) < 0 || mapped[0] == 0 {
                return false;
            }
            *(mapped[0] as *mut [f32; 4]) = [p.shoulder_start, p.headroom(), highlight(), if debug_fill() { 2.0 } else if debug() { 1.0 } else { 0.0 }];
            let unmap: unsafe extern "system" fn(Obj, Obj, u32) = std::mem::transmute(vfn(ctx, vt::CTX_UNMAP));
            unmap(ctx, self.cb, 0);

            let copy: unsafe extern "system" fn(Obj, Obj, Obj) = std::mem::transmute(vfn(ctx, crate::d3d::vt::CTX_COPY_RESOURCE));
            copy(ctx, self.scratch, res);

            let _state = StateGuard::save(ctx);
            let rtvs = [rtv];
            let vp = Viewport { x: 0.0, y: 0.0, w: d.width as f32, h: d.height as f32, min_depth: 0.0, max_depth: 1.0 };
            let factor = [1.0f32; 4];
            let set_rt: unsafe extern "system" fn(Obj, u32, *const Obj, Obj) = std::mem::transmute(vfn(ctx, vt::CTX_OM_SET_RENDER_TARGETS));
            set_rt(ctx, 1, rtvs.as_ptr(), std::ptr::null_mut());
            let set_vp: unsafe extern "system" fn(Obj, u32, *const Viewport) = std::mem::transmute(vfn(ctx, vt::CTX_RS_SET_VIEWPORTS));
            set_vp(ctx, 1, &vp);
            // Null states are the defaults: no culling for a clockwise triangle, scissor off, no blend, no depth.
            let set_rs: unsafe extern "system" fn(Obj, Obj) = std::mem::transmute(vfn(ctx, vt::CTX_RS_SET_STATE));
            set_rs(ctx, std::ptr::null_mut());
            let set_blend: unsafe extern "system" fn(Obj, Obj, *const f32, u32) = std::mem::transmute(vfn(ctx, vt::CTX_OM_SET_BLEND));
            set_blend(ctx, std::ptr::null_mut(), factor.as_ptr(), 0xffff_ffff);
            let set_depth: unsafe extern "system" fn(Obj, Obj, u32) = std::mem::transmute(vfn(ctx, vt::CTX_OM_SET_DEPTH));
            set_depth(ctx, std::ptr::null_mut(), 0);
            let set_layout: unsafe extern "system" fn(Obj, Obj) = std::mem::transmute(vfn(ctx, vt::CTX_IA_SET_LAYOUT));
            set_layout(ctx, std::ptr::null_mut());
            let set_topology: unsafe extern "system" fn(Obj, u32) = std::mem::transmute(vfn(ctx, vt::CTX_IA_SET_TOPOLOGY));
            set_topology(ctx, 4 /* TRIANGLELIST */);
            type SetShader = unsafe extern "system" fn(Obj, Obj, *const Obj, u32);
            let set_vs: SetShader = std::mem::transmute(vfn(ctx, vt::CTX_VS_SET_SHADER));
            set_vs(ctx, self.vs, std::ptr::null(), 0);
            let set_gs: SetShader = std::mem::transmute(vfn(ctx, vt::CTX_GS_SET_SHADER));
            set_gs(ctx, std::ptr::null_mut(), std::ptr::null(), 0);
            let set_ps: SetShader = std::mem::transmute(vfn(ctx, vt::CTX_PS_SET_SHADER));
            set_ps(ctx, self.ps, std::ptr::null(), 0);
            type SetSlot0 = unsafe extern "system" fn(Obj, u32, u32, *const Obj);
            let set_srv: SetSlot0 = std::mem::transmute(vfn(ctx, vt::CTX_PS_SET_SRVS));
            set_srv(ctx, 0, 1, &self.scratch_srv);
            let set_cb: SetSlot0 = std::mem::transmute(vfn(ctx, vt::CTX_PS_SET_CBS));
            set_cb(ctx, 0, 1, &self.cb);
            let draw: unsafe extern "system" fn(Obj, u32, u32) = std::mem::transmute(vfn(ctx, vt::CTX_DRAW));
            draw(ctx, 3, 0);
            drop(_state);
            if debug_fill() {
                static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                if N.fetch_add(1, Relaxed) % 120 == 0 {
                    probe(dev, ctx, res, d, d.width / 4, d.height / 2, "resolve (after draw, left half should be red 2,0,0)");
                }
            }
        }
        true
    }
}

impl Drop for Resolver {
    fn drop(&mut self) {
        // Objects of a device that is gone are children that keep it alive; releasing them is safe.
        unsafe {
            for o in [self.scratch_srv, self.scratch, self.cb, self.ps, self.vs] {
                release(o);
            }
        }
    }
}

static RESOLVER: Mutex<Option<Resolver>> = Mutex::new(None);

/// Tone-maps the scene texture behind `rtv` in place, on the context `ctx`. Returns true when the
/// draw was issued. Failures are logged once and turn the resolve off for good only when the
/// resolver itself cannot be built.
///
/// # Safety
/// `ctx` and `rtv` must be live D3D11 objects of the same device.
pub unsafe fn apply(ctx: Obj, rtv: Obj, screen: (u32, u32)) -> bool {
    let ok = unsafe { apply_with(ctx, rtv, screen, &params()) };
    if ok {
        let n = FRAMES.fetch_add(1, Relaxed) + 1;
        if n % 600 == 1 {
            log!("resolve: frame {n} (highlight {:.2}, headroom {:.2})", highlight(), headroom());
        }
        if !DONE.swap(true, Relaxed) {
            static FIRST: AtomicBool = AtomicBool::new(false);
            if !FIRST.swap(true, Relaxed) {
                log!("resolve: first frame resolved (headroom {})", headroom());
            }
        }
    }
    ok
}

/// `apply` with explicit parameters and without the per-frame bookkeeping.
///
/// # Safety
/// As `apply`.
pub unsafe fn apply_with(ctx: Obj, rtv: Obj, screen: (u32, u32), p: &ToneParams) -> bool {
    let _ig = IgnoreScope::new();
    let mut res: Obj = std::ptr::null_mut();
    let mut dev: Obj = std::ptr::null_mut();
    let ok = unsafe { apply_inner(ctx, rtv, screen, p, &mut res, &mut dev) };
    unsafe {
        release(res);
        release(dev);
    }
    ok
}

unsafe fn apply_inner(ctx: Obj, rtv: Obj, screen: (u32, u32), p: &ToneParams, res: &mut Obj, dev: &mut Obj) -> bool {
    unsafe {
        let get_resource: unsafe extern "system" fn(Obj, *mut Obj) = std::mem::transmute(vfn(rtv, vt::VIEW_GET_RESOURCE));
        get_resource(rtv, res);
        if res.is_null() {
            return false;
        }
        let mut dim = 0u32;
        let get_type: unsafe extern "system" fn(Obj, *mut u32) = std::mem::transmute(vfn(*res, vt::RES_GET_TYPE));
        get_type(*res, &mut dim);
        if dim != RESOURCE_TEXTURE2D {
            return false;
        }
        let mut d = Tex2dDesc::default();
        let get_desc: unsafe extern "system" fn(Obj, *mut Tex2dDesc) = std::mem::transmute(vfn(*res, vt::TEX2D_GET_DESC));
        get_desc(*res, &mut d);
        if (d.width, d.height) != screen || d.mips != 1 || d.array != 1 || d.samples != 1 {
            log_self_check_once("the UI pass RT0 texture is not a single-mip, single-sample full-size 2D texture");
            return false;
        }
        let get_device: unsafe extern "system" fn(Obj, *mut Obj) = std::mem::transmute(vfn(ctx, vt::CHILD_GET_DEVICE));
        get_device(ctx, dev);
        if dev.is_null() {
            return false;
        }
        let Ok(mut guard) = RESOLVER.try_lock() else { return false };
        // Objects of another device are never used: a device change drops (releases) them first.
        if guard.as_ref().is_some_and(|r| r.device != *dev as usize) {
            *guard = None;
        }
        if guard.is_none() {
            match Resolver::new(*dev) {
                Some(r) => *guard = Some(r),
                None => {
                    FAILED.store(true, Relaxed);
                    log!("resolve: could not be created; the resolve stays off");
                    return false;
                }
            }
        }
        let r = guard.as_mut().expect("just created");
        if !r.run(*dev, ctx, rtv, *res, &d, p) {
            static LOGGED: AtomicBool = AtomicBool::new(false);
            if !LOGGED.swap(true, Relaxed) {
                log!("resolve: the scratch copy or the constants could not be set up; frame left unresolved");
            }
            return false;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_level_precedence() {
        let _g = crate::testlock::lock();
        let keep = (debug(), debug_fill(), debug_materials(), debug_sun(), debug_reflectivity(), debug_view());
        set_debug(false); set_debug_fill(false); set_debug_materials(false); set_debug_sun(false); set_debug_reflectivity(false); set_debug_view(0);
        assert_eq!(debug_level(), 0);
        set_debug(true);
        assert_eq!(debug_level(), 1);
        set_debug_fill(true);
        assert_eq!(debug_level(), 2);
        set_debug_materials(true);
        assert_eq!(debug_level(), 3);
        set_debug_sun(true);
        assert_eq!(debug_level(), 4);
        set_debug_reflectivity(true);
        assert_eq!(debug_level(), 5);
        set_debug_reflectivity(false);
        assert_eq!(debug_level(), 4);
        set_debug_sun(false);
        assert_eq!(debug_level(), 3);
        set_debug(keep.0); set_debug_fill(keep.1); set_debug_materials(keep.2); set_debug_sun(keep.3); set_debug_reflectivity(keep.4); set_debug_view(keep.5);
    }

    #[test]
    fn the_debug_view_key_merges_with_the_checkboxes_and_the_highest_level_wins() {
        // Pure: the whole matrix, no shared state touched. (reflectivity, sun, materials, fill, view) -> the level of the checkboxes alone.
        let cases: [((bool, bool, bool, bool, bool), u8); 10] = [
            ((false, false, false, false, false), 0),
            ((false, false, false, false, true), 1),
            ((false, false, false, true, false), 2),
            ((false, false, true, false, false), 3),
            ((false, true, false, false, false), 4),
            ((true, false, false, false, false), 5),
            ((false, false, false, true, true), 2),
            ((false, false, true, true, true), 3),
            ((false, true, true, true, true), 4),
            ((true, true, true, true, true), 5),
        ];
        for key in 0..=5u8 {
            for ((r, su, m, f, v), boxes) in cases {
                assert_eq!(merge_level(r, su, m, f, v, key), boxes.max(key), "checkboxes {:?} key {key}", (r, su, m, f, v));
            }
        }
        // Out of range is clamped, never wraps.
        assert_eq!(merge_level(false, false, false, false, false, 9), 5);
        assert_eq!(merge_level(false, false, false, false, false, 255), 5);
        // The key against single checkboxes.
        assert_eq!(merge_level(false, false, false, false, true, 3), 3, "key 3 beats the view checkbox (1)");
        assert_eq!(merge_level(false, true, false, false, false, 3), 4, "the sun checkbox (4) beats key 3");
        assert_eq!(merge_level(false, true, false, false, false, 5), 5, "key 5 beats the sun checkbox");
        assert_eq!(merge_level(true, false, false, false, false, 2), 5, "the reflectivity checkbox beats key 2");
        assert_eq!(merge_level(false, false, false, false, true, 2), 2, "key 2 beats the view checkbox (1)");
        assert_eq!(merge_level(false, false, false, false, true, 0), 1, "the checkbox alone");
    }

    #[test]
    fn debug_view_feeds_debug_level_and_is_clamped() {
        let _g = crate::testlock::lock();
        let keep = (debug(), debug_fill(), debug_materials(), debug_sun(), debug_reflectivity(), debug_view());
        set_debug(false); set_debug_fill(false); set_debug_materials(false); set_debug_sun(false); set_debug_reflectivity(false); set_debug_view(0);
        // Levels 3..5 only: the AO's constants read levels 1 and 2 (their own tests run in parallel), so those two are covered by the pure test above.
        for v in 3..=5u8 {
            assert_eq!(set_debug_view(v), v);
            assert_eq!(debug_view(), v);
            assert_eq!(debug_level(), v, "debug_view {v} alone");
        }
        assert_eq!(set_debug_view(9), 5, "clamped");
        assert_eq!(set_debug_view(255), 5);
        assert_eq!(debug_level(), 5);
        set_debug_view(3);
        set_debug_sun(true);
        assert_eq!(debug_level(), 4, "the sun checkbox beats key 3");
        set_debug_sun(false);
        assert_eq!(debug_level(), 3);
        set_debug_view(0);
        assert_eq!(debug_level(), 0);
        set_debug(keep.0); set_debug_fill(keep.1); set_debug_materials(keep.2); set_debug_sun(keep.3); set_debug_reflectivity(keep.4); set_debug_view(keep.5);
    }

    const SCREEN: (u32, u32) = (3440, 1440);
    const F16: Target = Target { w: 3440, h: 1440, fmt: FMT_R16G16B16A16_FLOAT };
    const U8: Target = Target { w: 3440, h: 1440, fmt: FMT_B8G8R8A8_UNORM };

    #[test]
    fn decide_runs_on_float_ui_target_after_a_scene_pass() {
        assert_eq!(decide(F16, Some(F16), SCREEN), Decision::Run);
        assert_eq!(decide(F16, Some(U8), SCREEN), Decision::Run);
    }

    #[test]
    fn decide_skips_when_the_target_is_not_float_or_not_full_size() {
        assert!(matches!(decide(U8, Some(F16), SCREEN), Decision::SelfCheck(_)));
        assert!(matches!(decide(Target { w: 860, ..F16 }, Some(F16), SCREEN), Decision::SelfCheck(_)));
    }

    #[test]
    fn decide_skips_when_the_previous_pass_is_not_a_full_size_scene_pass() {
        assert!(matches!(decide(F16, None, SCREEN), Decision::SelfCheck(_)));
        assert!(matches!(decide(F16, Some(Target { w: 860, h: 360, fmt: 10 }), SCREEN), Decision::SelfCheck(_)));
        assert!(matches!(decide(F16, Some(Target { fmt: 45, ..F16 }), SCREEN), Decision::SelfCheck(_)));
    }

    #[test]
    fn headroom_defaults_to_one_and_ignores_bad_values() {
        // Only this test touches HEADROOM.
        assert_eq!(headroom(), 1.0);
        set_headroom(4.0);
        assert_eq!(headroom(), 4.0);
        set_headroom(0.5);
        assert_eq!(headroom(), 1.0);
        set_headroom(f32::NAN);
        assert_eq!(headroom(), 1.0);
    }

    #[test]
    fn wants_needs_enabled_and_not_done() {
        // Only this test touches ENABLED / DONE / FAILED. The pass itself is retired (PASS_ENABLED), so
        // `wants()` is false whatever the switches say; the switch logic is still pinned below.
        set_enabled(false);
        frame_begin();
        assert!(!wants());
        set_enabled(true);
        assert_eq!(wants(), PASS_ENABLED);
        DONE.store(true, Relaxed);
        assert!(!wants());
        frame_begin();
        assert_eq!(wants(), PASS_ENABLED);
        set_enabled(false);
    }

    #[test]
    fn the_resolve_pass_is_retired_in_favour_of_the_data_path() {
        assert!(!PASS_ENABLED, "the in-place pass was overwritten by the engine before the final pass; do not turn it back on");
    }
}
