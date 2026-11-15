//! The mod's own "look" passes of the 3D scene: lens effects (`lens.rs`) and colour grading (`grading.rs`). See `docs/lens.md`
//! and `docs/grading.md`.
//!
//! WHERE. Exactly where the AA runs (`aa.rs`, `data-path.md`): at the draw of the engine's pre-UI copy (`ps#c0ad1686`), the last pass
//! that draws the 3D scene alone. `draw_guard` (called by the `Draw*` hooks right after the AA's guard) reads the copy's current
//! input t0 (the AA's result when the AA is on), runs the mod's passes in mod-owned float16 targets and binds the result AS t0 for the
//! engine's own draw; the guard puts the engine's SRV back afterwards (RAII). The UI is drawn after the copy and is never touched; no
//! engine target is written. This was chosen over the light-adjust substitute because (a) the lens needs a blurred, downsampled copy
//! of the whole finished picture, which the substitute (a per-pixel shader) cannot read, (b) it must not feed the auto exposure
//! measurement (taken before the light-adjust draw) or the bloom, and (c) the LUT needs a 3D texture slot and constants that stay
//! out of the 37 KB substitute. The cost is that the exposure and the highlight boost are already applied: the lens threshold is
//! therefore expressed in units of the scene BEFORE the boost (`LensParams::gain`).
//!
//! With the lens off and the grading neutral (and no LUT) nothing runs and nothing is rebound: bit-identical to the engine.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};

use crate::d3d::{IgnoreScope, Obj, StateGuard, Viewport, ignoring, release, vfn, vt};
use crate::grading::{self, Cube, Grade, GradeParams};
use crate::lens::{self, LensParams};
use crate::shafts::{NO_TARGET, Target, make_target};
use crate::{ao, log, recorder, shaders};

pub const HLSL: &str = include_str!("../shaders/look.hlsl");
pub const VS_ENTRY: &str = "vs_main";

/// `ps#c0ad1686`: the engine's final copy before the UI.
pub const COPY_HASH: u32 = 0xc0ad1686;

const SLOT_FIRST: u32 = 10;
const SLOT_COUNT: u32 = 9;
const SLOT_COLOR: u32 = 10;
const SLOT_D4: u32 = 11;
const SLOT_SRC8: u32 = 12;
const SLOT_PING: u32 = 13;
const SLOT_GLARE: u32 = 14;
const SLOT_PONG: u32 = 15;
const SLOT_LUT: u32 = 16;
const SLOT_COV: u32 = 17;
const SLOT_TMP: u32 = 18;

const FMT_RGBA16F: u32 = 10;

const FLAG_LENS: f32 = 1.0;
const FLAG_GRADE: f32 = 2.0;
const FLAG_LUT: f32 = 4.0;

// ---- what a run does -----------------------------------------------------------------------------------------------------

/// One run: the lens settings (None = no lens passes), the slider grade (None or neutral = none), a LUT (generation, cube, strength).
pub struct LookInput<'a> {
    pub lens: Option<LensParams>,
    pub grade: Option<Grade>,
    pub lut: Option<(u64, &'a Cube, f32)>,
}

impl LookInput<'_> {
    pub fn do_grade(&self) -> bool {
        self.grade.is_some_and(|g| !g.is_neutral())
    }

    pub fn do_lut(&self) -> bool {
        self.lut.is_some_and(|(_, _, s)| s > 0.0)
    }

    /// Nothing to do: no pass runs and nothing is rebound.
    pub fn is_noop(&self) -> bool {
        self.lens.is_none() && !self.do_grade() && !self.do_lut()
    }
}

/// Constant vectors of `look.hlsl` (see its header).
pub const CB_VECTORS: usize = 9;

/// The constant vectors of `look.hlsl` (see its header).
pub fn cb_values(size: (u32, u32), inp: &LookInput) -> [[f32; 4]; CB_VECTORS] {
    let (w, h) = (size.0.max(1) as f32, size.1.max(1) as f32);
    let lv = crate::exposure::level_sizes(size.0, size.1);
    let (l4, l8) = (lv[0], lv[1]);
    let mut flags = 0.0;
    let (mut lens_a, mut lens_b, mut lens_c, mut sz16) = ([0.0; 4], [0.0; 4], [0.0; 4], [0.0; 4]);
    if let Some(p) = &inp.lens {
        flags += FLAG_LENS;
        (lens_a, lens_b) = lens::cb_lens(p, l8.0);
        (lens_c, sz16) = lens::cb_sources(p, size.1, lv[2]);
    }
    let g = inp.grade.unwrap_or_default().clamped();
    let gp = GradeParams::from_grade(&g);
    let mut lut_size = 0.0;
    let mut lut_strength = 0.0;
    if inp.do_grade() {
        flags += FLAG_GRADE;
    }
    let (sat, con, vib) = if inp.do_grade() { (gp.saturation, gp.contrast, gp.vibrance) } else { (1.0, 1.0, 0.0) };
    let (sh, hi, gains) = if inp.do_grade() { (gp.shadows, gp.highlights, gp.gains) } else { (0.0, 0.0, [1.0; 3]) };
    if let Some((_, cube, s)) = inp.lut {
        if s > 0.0 {
            flags += FLAG_LUT;
            lut_size = cube.size as f32;
            lut_strength = grading::clamp_lut_strength(s);
        }
    }
    [
        [w, h, 1.0 / w, 1.0 / h],
        [l4.0 as f32, l4.1 as f32, l8.0 as f32, l8.1 as f32],
        lens_a,
        lens_b,
        [sat, con, vib, lut_strength],
        [gains[0], gains[1], gains[2], lut_size],
        [sh, hi, flags, 0.0],
        lens_c,
        sz16,
    ]
}

// ---- shaders ------------------------------------------------------------------------------------------------------------

pub struct Blobs {
    pub vs: Vec<u8>,
    pub cover: Vec<u8>,
    pub area_h: Vec<u8>,
    pub area_v: Vec<u8>,
    pub down4: Vec<u8>,
    pub down8: Vec<u8>,
    pub glare_h: Vec<u8>,
    pub glare_v: Vec<u8>,
    pub streak: [Vec<u8>; 3],
    pub composite: Vec<u8>,
}

/// Compiles the shaders (calls D3DCompile: never on the render thread).
pub fn compile_blobs() -> Result<Blobs, String> {
    let c = |entry: &str, profile: &str| shaders::compile(HLSL, entry, profile).map_err(|e| format!("{entry}: {e}"));
    Ok(Blobs {
        vs: c(VS_ENTRY, "vs_5_0")?,
        cover: c("ps_cover", "ps_5_0")?,
        area_h: c("ps_area_h", "ps_5_0")?,
        area_v: c("ps_area_v", "ps_5_0")?,
        down4: c("ps_down4", "ps_5_0")?,
        down8: c("ps_down8", "ps_5_0")?,
        glare_h: c("ps_glare_h", "ps_5_0")?,
        glare_v: c("ps_glare_v", "ps_5_0")?,
        streak: [c("ps_streak0", "ps_5_0")?, c("ps_streak1", "ps_5_0")?, c("ps_streak2", "ps_5_0")?],
        composite: c("ps_composite", "ps_5_0")?,
    })
}

static BLOBS: OnceLock<Result<Blobs, String>> = OnceLock::new();

/// Starts the background compile (the settings are applied by `lens` / `grading` init).
pub fn init() {
    let _ = std::thread::Builder::new().name("gw2g-look-compile".into()).spawn(|| {
        let r = compile_blobs();
        match &r {
            Ok(_) => log!("look: shaders compiled"),
            Err(e) => log!("look: shader compile failed, lens effects and colour grading stay off: {e}"),
        }
        let _ = BLOBS.set(r);
    });
}

// ---- device objects ------------------------------------------------------------------------------------------------------

type CreateShaderFn = unsafe extern "system" fn(Obj, *const u8, usize, Obj, *mut Obj) -> i32;
type CreateViewFn = unsafe extern "system" fn(Obj, Obj, *const [u32; 6], *mut Obj) -> i32;

#[repr(C)]
struct SubresourceData {
    mem: *const c_void,
    pitch: u32,
    slice: u32,
}

/// Device objects of one device: shaders, the constant buffer, targets for one picture size and the LUT texture.
pub struct Look {
    device: usize,
    vs: Obj,
    ps_cover: Obj,
    ps_area_h: Obj,
    ps_area_v: Obj,
    ps_down4: Obj,
    ps_down8: Obj,
    ps_glare_h: Obj,
    ps_glare_v: Obj,
    ps_streak: [Obj; 3],
    ps_composite: Obj,
    cb: Obj,
    size: (u32, u32),
    l4: Target,
    l8: Target,
    ping: Target,
    pong: Target,
    glare: Target,
    cov16: Target,
    tmp16: Target,
    out: Target,
    lut_tex: Obj,
    lut_srv: Obj,
    lut_gen: u64,
}

// Only touched under the mutex from the render thread.
unsafe impl Send for Look {}

impl Look {
    /// # Safety
    /// `device` must be a live `ID3D11Device`.
    pub unsafe fn new(device: Obj, b: &Blobs) -> Option<Look> {
        let _ig = IgnoreScope::new();
        let nul = std::ptr::null_mut();
        let mut l = Look {
            device: device as usize,
            vs: nul,
            ps_cover: nul,
            ps_area_h: nul,
            ps_area_v: nul,
            ps_down4: nul,
            ps_down8: nul,
            ps_glare_h: nul,
            ps_glare_v: nul,
            ps_streak: [nul; 3],
            ps_composite: nul,
            cb: nul,
            size: (0, 0),
            l4: NO_TARGET,
            l8: NO_TARGET,
            ping: NO_TARGET,
            pong: NO_TARGET,
            glare: NO_TARGET,
            cov16: NO_TARGET,
            tmp16: NO_TARGET,
            out: NO_TARGET,
            lut_tex: nul,
            lut_srv: nul,
            lut_gen: 0,
        };
        unsafe {
            let create_vs: CreateShaderFn = std::mem::transmute(vfn(device, vt::DEV_CREATE_VERTEX_SHADER));
            let create_ps: CreateShaderFn = std::mem::transmute(vfn(device, vt::DEV_CREATE_PIXEL_SHADER));
            let ok = create_vs(device, b.vs.as_ptr(), b.vs.len(), nul, &mut l.vs) >= 0
                && create_ps(device, b.cover.as_ptr(), b.cover.len(), nul, &mut l.ps_cover) >= 0
                && create_ps(device, b.area_h.as_ptr(), b.area_h.len(), nul, &mut l.ps_area_h) >= 0
                && create_ps(device, b.area_v.as_ptr(), b.area_v.len(), nul, &mut l.ps_area_v) >= 0
                && create_ps(device, b.down4.as_ptr(), b.down4.len(), nul, &mut l.ps_down4) >= 0
                && create_ps(device, b.down8.as_ptr(), b.down8.len(), nul, &mut l.ps_down8) >= 0
                && create_ps(device, b.glare_h.as_ptr(), b.glare_h.len(), nul, &mut l.ps_glare_h) >= 0
                && create_ps(device, b.glare_v.as_ptr(), b.glare_v.len(), nul, &mut l.ps_glare_v) >= 0
                && (0..3).all(|i| create_ps(device, b.streak[i].as_ptr(), b.streak[i].len(), nul, &mut l.ps_streak[i]) >= 0)
                && create_ps(device, b.composite.as_ptr(), b.composite.len(), nul, &mut l.ps_composite) >= 0;
            if !ok {
                log!("look: could not create the shaders");
                return None; // Drop releases what exists
            }
            // D3D11_BUFFER_DESC { ByteWidth, Usage = DYNAMIC, BindFlags = CONSTANT_BUFFER, CPUAccessFlags = WRITE, MiscFlags, StructureByteStride }
            let create_buffer: unsafe extern "system" fn(Obj, *const [u32; 6], *const c_void, *mut Obj) -> i32 = std::mem::transmute(vfn(device, vt::DEV_CREATE_BUFFER));
            if create_buffer(device, &[(CB_VECTORS * 16) as u32, 2, 4, 0x10000, 0, 0], std::ptr::null(), &mut l.cb) < 0 || l.cb.is_null() {
                log!("look: could not create the constant buffer");
                return None;
            }
        }
        Some(l)
    }

    unsafe fn free_lens_targets(&mut self) {
        unsafe {
            self.l4.free();
            self.l8.free();
            self.ping.free();
            self.pong.free();
            self.glare.free();
            self.cov16.free();
            self.tmp16.free();
        }
    }

    unsafe fn free_targets(&mut self) {
        unsafe {
            self.free_lens_targets();
            self.out.free();
        }
        self.size = (0, 0);
    }

    unsafe fn ensure_targets(&mut self, dev: Obj, size: (u32, u32), lens: bool) -> bool {
        if self.size != size {
            unsafe { self.free_targets() };
        }
        let lv = crate::exposure::level_sizes(size.0, size.1);
        unsafe {
            let mut ok = true;
            let mut need = |t: &mut Target, sz: (u32, u32)| {
                if t.tex.is_null() {
                    match make_target(dev, sz.0, sz.1, FMT_RGBA16F, false) {
                        Some(n) => *t = n,
                        None => ok = false,
                    }
                }
            };
            need(&mut self.out, size);
            if lens {
                need(&mut self.l4, lv[0]);
                need(&mut self.l8, lv[1]);
                need(&mut self.ping, lv[1]);
                need(&mut self.pong, lv[1]);
                need(&mut self.glare, lv[1]);
                need(&mut self.cov16, lv[2]);
                need(&mut self.tmp16, lv[2]);
            }
            if !ok {
                self.free_targets();
                return false;
            }
        }
        if self.size != size {
            log!("look: targets {}x{}", size.0, size.1);
        }
        self.size = size;
        true
    }

    /// Uploads the LUT as an RGBA16F 3D texture when the generation changed. false = no usable texture.
    unsafe fn ensure_lut(&mut self, dev: Obj, generation: u64, cube: &Cube) -> bool {
        if generation == self.lut_gen && !self.lut_srv.is_null() {
            return true;
        }
        unsafe {
            release(self.lut_srv);
            release(self.lut_tex);
        }
        self.lut_srv = std::ptr::null_mut();
        self.lut_tex = std::ptr::null_mut();
        self.lut_gen = generation;
        let n = cube.size as u32;
        if cube.data.len() != (n * n * n) as usize || n < 2 {
            return false;
        }
        let texels = grading::cube_texels(cube);
        unsafe {
            // D3D11_TEXTURE3D_DESC { Width, Height, Depth, MipLevels, Format, Usage = IMMUTABLE, BindFlags = SRV, CPUAccessFlags, MiscFlags }
            let desc: [u32; 9] = [n, n, n, 1, FMT_RGBA16F, 1, 0x8, 0, 0];
            let sd = SubresourceData { mem: texels.as_ptr().cast(), pitch: n * 8, slice: n * n * 8 };
            let create_tex3d: unsafe extern "system" fn(Obj, *const [u32; 9], *const SubresourceData, *mut Obj) -> i32 = std::mem::transmute(vfn(dev, vt::DEV_CREATE_TEXTURE3D));
            let create_srv: CreateViewFn = std::mem::transmute(vfn(dev, vt::DEV_CREATE_SRV));
            let mut tex: Obj = std::ptr::null_mut();
            if create_tex3d(dev, &desc, &sd, &mut tex) < 0 || tex.is_null() {
                return false;
            }
            let mut srv: Obj = std::ptr::null_mut();
            if create_srv(dev, tex, std::ptr::null(), &mut srv) < 0 || srv.is_null() {
                release(tex);
                return false;
            }
            self.lut_tex = tex;
            self.lut_srv = srv;
        }
        true
    }

    /// SRV of the finished picture (valid after `run`, until the targets are recreated).
    #[allow(dead_code)] // used by the tests
    pub fn out_srv(&self) -> Obj {
        self.out.srv
    }

    /// Runs the passes on `color_srv` (a `size` picture) and returns the SRV of the result, None when nothing is to be done or
    /// something failed before the first draw. All pipeline state touched is restored.
    ///
    /// # Safety
    /// All pointers must be live objects of the device this was created on; `color_srv` a view of a `size` colour texture.
    pub unsafe fn run(&mut self, dev: Obj, ctx: Obj, color_srv: Obj, size: (u32, u32), inp: &LookInput) -> Option<Obj> {
        let _ig = IgnoreScope::new();
        if inp.is_noop() {
            return None;
        }
        let do_lens = inp.lens.is_some();
        unsafe {
            if !self.ensure_targets(dev, size, do_lens) {
                return None;
            }
            let mut inp_lut = inp.lut;
            if let Some((generation, cube, s)) = inp.lut {
                if s > 0.0 && !self.ensure_lut(dev, generation, cube) {
                    inp_lut = None; // the LUT cannot be used this frame: the rest still runs
                }
            }
            let inp2 = LookInput { lens: inp.lens, grade: inp.grade, lut: inp_lut };
            if inp2.is_noop() {
                return None;
            }
            let mut mapped: [usize; 3] = [0; 3]; // D3D11_MAPPED_SUBRESOURCE { pData, RowPitch, DepthPitch }
            let map: unsafe extern "system" fn(Obj, Obj, u32, u32, u32, *mut [usize; 3]) -> i32 = std::mem::transmute(vfn(ctx, vt::CTX_MAP));
            let unmap: unsafe extern "system" fn(Obj, Obj, u32) = std::mem::transmute(vfn(ctx, vt::CTX_UNMAP));
            if map(ctx, self.cb, 0, 4 /* WRITE_DISCARD */, 0, &mut mapped) < 0 || mapped[0] == 0 {
                return None;
            }
            *(mapped[0] as *mut [[f32; 4]; CB_VECTORS]) = cb_values(size, &inp2);
            unmap(ctx, self.cb, 0);

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
            let nul: Obj = std::ptr::null_mut();
            let clear = || set_srv(ctx, SLOT_FIRST, SLOT_COUNT, [nul; SLOT_COUNT as usize].as_ptr());

            set_rs(ctx, nul);
            set_depth(ctx, nul, 0);
            set_blend(ctx, nul, factor.as_ptr(), 0xffff_ffff);
            set_layout(ctx, nul);
            set_topology(ctx, 4 /* TRIANGLELIST */);
            set_vs(ctx, self.vs, std::ptr::null(), 0);
            set_gs(ctx, nul, std::ptr::null(), 0);
            set_cb(ctx, 0, 1, &self.cb);

            // One full-screen pass: `inputs` are (slot, srv) pairs, everything else is unbound.
            let pass = |ps: Obj, dst: &Target, sz: (u32, u32), inputs: &[(u32, Obj)]| {
                clear();
                let vp = Viewport { x: 0.0, y: 0.0, w: sz.0 as f32, h: sz.1 as f32, min_depth: 0.0, max_depth: 1.0 };
                set_vp(ctx, 1, &vp);
                set_rt(ctx, 1, &dst.rtv, nul);
                set_ps(ctx, ps, std::ptr::null(), 0);
                for (slot, srv) in inputs {
                    set_srv(ctx, *slot, 1, srv);
                }
                draw(ctx, 3, 0);
            };

            let lv = crate::exposure::level_sizes(size.0, size.1);
            if do_lens {
                // The size test (lens_max_size 0 = off): the bright share of the neighbourhood, cov16 -> tmp16 -> cov16.
                let sized = inp2.lens.is_some_and(|p| lens::area_step(p.max_size, size.1) > 0.0);
                if sized {
                    pass(self.ps_cover, &self.cov16, lv[2], &[(SLOT_COLOR, color_srv)]);
                    pass(self.ps_area_h, &self.tmp16, lv[2], &[(SLOT_COV, self.cov16.srv)]);
                    pass(self.ps_area_v, &self.cov16, lv[2], &[(SLOT_TMP, self.tmp16.srv)]);
                }
                pass(self.ps_down4, &self.l4, lv[0], &[(SLOT_COLOR, color_srv)]);
                let d8_inputs: &[(u32, Obj)] = if sized { &[(SLOT_D4, self.l4.srv), (SLOT_COV, self.cov16.srv)] } else { &[(SLOT_D4, self.l4.srv)] };
                pass(self.ps_down8, &self.l8, lv[1], d8_inputs);
                pass(self.ps_glare_h, &self.ping, lv[1], &[(SLOT_SRC8, self.l8.srv)]);
                pass(self.ps_glare_v, &self.glare, lv[1], &[(SLOT_PING, self.ping.srv)]);
                pass(self.ps_streak[0], &self.ping, lv[1], &[(SLOT_SRC8, self.l8.srv)]);
                pass(self.ps_streak[1], &self.pong, lv[1], &[(SLOT_PING, self.ping.srv)]);
                pass(self.ps_streak[2], &self.ping, lv[1], &[(SLOT_PONG, self.pong.srv)]);
            }
            let mut inputs: Vec<(u32, Obj)> = vec![(SLOT_COLOR, color_srv)];
            if do_lens {
                inputs.push((SLOT_PING, self.ping.srv));
                inputs.push((SLOT_GLARE, self.glare.srv));
            }
            if inp2.do_lut() {
                inputs.push((SLOT_LUT, self.lut_srv));
            }
            pass(self.ps_composite, &self.out, size, &inputs);
            // Leave nothing of ours bound (the guard restores t0, t1, b0, the targets and the shaders).
            clear();
        }
        Some(self.out.srv)
    }
}

impl Drop for Look {
    fn drop(&mut self) {
        unsafe {
            self.free_targets();
            release(self.lut_srv);
            release(self.lut_tex);
            for o in [self.cb, self.ps_composite, self.ps_streak[2], self.ps_streak[1], self.ps_streak[0], self.ps_glare_v, self.ps_glare_h, self.ps_down8, self.ps_down4, self.ps_area_v, self.ps_area_h, self.ps_cover, self.vs] {
                release(o);
            }
        }
    }
}

// ---- per-frame glue (called from the Draw hooks) ---------------------------------------------------------------------------

static STATE: Mutex<Option<Look>> = Mutex::new(None);
static AT_COPY: AtomicBool = AtomicBool::new(false);
static DONE: AtomicBool = AtomicBool::new(false);
static FAILED: AtomicBool = AtomicBool::new(false);
static COPY_SEEN: AtomicU64 = AtomicU64::new(0);
static LENS_APPLIED: AtomicU64 = AtomicU64::new(0);
static GRADE_APPLIED: AtomicU64 = AtomicU64::new(0);
static LUT_APPLIED: AtomicU64 = AtomicU64::new(0);
static SKIPPED_SHAPE: AtomicU64 = AtomicU64::new(0);
static LAST_SKIP: AtomicU32 = AtomicU32::new(0);
/// Last frame: (lens, grade, lut) were part of the composite (bits 1, 2, 4).
static LAST_MODE: AtomicU32 = AtomicU32::new(0);
const SKIP_NONE: u32 = 0;
const SKIP_COMPILING: u32 = 1;
const SKIP_SHAPE: u32 = 2;
const SKIP_RUN: u32 = 3;
const SKIP_NOTHING: u32 = 4;

/// Does the feature run at all: the user did not switch the mod off, the frames are recognised and a part is on.
pub fn wanted_for(user_enabled: bool, substitution: bool, lens_on: bool, grade_on: bool) -> bool {
    user_enabled && substitution && (lens_on || grade_on)
}

fn wanted() -> bool {
    wanted_for(crate::gate::user_enabled(), shaders::substitution_enabled(), lens::on(), grading::any_on())
}

/// A new frame starts: the picture may be processed again.
pub fn frame_begin() {
    DONE.store(false, Relaxed);
}

/// `PSSetShader` on `ctx`: remembers whether the engine's pre-UI copy shader is now bound. One atomic load when the feature is off.
pub fn on_ps_set(ctx: Obj, ps: Obj) {
    if !wanted() && !crate::photo::scene_wanted() {
        if AT_COPY.load(Relaxed) {
            AT_COPY.store(false, Relaxed);
        }
        return;
    }
    if ignoring() || recorder::game_context() != ctx as usize {
        return;
    }
    let _ = std::panic::catch_unwind(|| {
        let is_copy = !ps.is_null() && shaders::registry().hash_of(ps as usize) == Some(COPY_HASH);
        AT_COPY.store(is_copy, Relaxed);
    });
}

/// Puts the engine's (or the AA's) SRV back into t0 when dropped (at the end of the draw hook, after the engine's draw).
pub struct LookGuard {
    restore: Option<(Obj, Obj)>,
}

impl Drop for LookGuard {
    fn drop(&mut self) {
        if let Some((ctx, srv)) = self.restore.take() {
            unsafe {
                let set_srv: unsafe extern "system" fn(Obj, u32, u32, *const Obj) = std::mem::transmute(vfn(ctx, vt::CTX_PS_SET_SRVS));
                set_srv(ctx, 0, 1, &srv);
                release(srv);
            }
        }
    }
}

/// A draw is about to be issued on `ctx`: when it is the pre-UI copy and a part is on, runs the passes and binds the result as t0.
/// Must be created AFTER the AA's guard (it reads the AA's result) so it drops first. One atomic load for every other draw; never unwinds.
pub fn draw_guard(ctx: Obj) -> LookGuard {
    if !AT_COPY.load(Relaxed) {
        return LookGuard { restore: None };
    }
    let restore = std::panic::catch_unwind(|| unsafe { apply(ctx) }).ok().flatten().map(|srv| (ctx, srv));
    // Photo mode (`photo_hide_ui`): the picture now bound as t0 is the 3D scene with every mod effect and no UI; copy it once when armed.
    crate::photo::grab_scene(ctx);
    LookGuard { restore }
}

/// The settings of this frame (the lens needs HDR output: in SDR nothing is above paper white).
fn frame_input<'a>(cube: &'a Option<std::sync::Arc<Cube>>, generation: u64) -> LookInput<'a> {
    let p = crate::datapath::current_params(false, false);
    let lens = (lens::on() && !p.sdr_clamp).then(|| LensParams {
        strength: lens::strength(),
        threshold: lens::threshold(),
        ghost: lens::ghost(),
        length: lens::streak_length(),
        gain: p.boost,
        max_size: lens::max_size(),
        color_reject: lens::color_reject(),
    });
    let g = grading::get();
    let s = grading::lut_strength();
    LookInput { lens, grade: (!g.is_neutral()).then_some(g), lut: cube.as_deref().filter(|_| s > 0.0).map(|c| (generation, c, s)) }
}

unsafe fn copy_shape(ctx: Obj, srv: Obj) -> Option<(u32, u32)> {
    let site = recorder::current_site(ctx as usize);
    unsafe {
        match (ao::texture_of(srv), site) {
            (Some((res, d)), Some(site)) => {
                release(res);
                let rt_ok = site.rt0.as_ref().is_some_and(|t| t.w == d.width && t.h == d.height);
                (d.samples == 1 && d.array == 1 && d.mips == 1 && d.width >= 320 && d.height >= 200 && rt_ok && site.depth.is_none()).then_some((d.width, d.height))
            }
            (Some((res, _)), None) => {
                release(res);
                None
            }
            _ => None,
        }
    }
}

/// Returns the t0 SRV that was bound (an owned reference) after binding the processed picture, or None (nothing changed).
unsafe fn apply(ctx: Obj) -> Option<Obj> {
    if DONE.load(Relaxed) || ignoring() || recorder::game_context() != ctx as usize || !wanted() || FAILED.load(Relaxed) {
        return None;
    }
    COPY_SEEN.fetch_add(1, Relaxed);
    let blobs = match BLOBS.get() {
        None => {
            LAST_SKIP.store(SKIP_COMPILING, Relaxed);
            return None;
        }
        Some(Err(_)) => {
            FAILED.store(true, Relaxed);
            return None;
        }
        Some(Ok(b)) => b,
    };
    let (generation, cube) = grading::lut_for_gpu();
    let inp = frame_input(&cube, generation);
    if inp.is_noop() {
        LAST_SKIP.store(SKIP_NOTHING, Relaxed);
        return None;
    }
    unsafe {
        let get_srvs: unsafe extern "system" fn(Obj, u32, u32, *mut Obj) = std::mem::transmute(vfn(ctx, vt::CTX_PS_GET_SRVS));
        let mut srv: Obj = std::ptr::null_mut();
        get_srvs(ctx, 0, 1, &mut srv);
        if srv.is_null() {
            return None;
        }
        let Some((width, height)) = copy_shape(ctx, srv) else {
            release(srv);
            SKIPPED_SHAPE.fetch_add(1, Relaxed);
            LAST_SKIP.store(SKIP_SHAPE, Relaxed);
            return None;
        };
        let mut dev: Obj = std::ptr::null_mut();
        let get_device: unsafe extern "system" fn(Obj, *mut Obj) = std::mem::transmute(vfn(ctx, vt::CHILD_GET_DEVICE));
        get_device(ctx, &mut dev);
        if dev.is_null() {
            release(srv);
            return None;
        }
        let out = 'run: {
            let Ok(mut g) = STATE.try_lock() else { break 'run None };
            if g.as_ref().is_some_and(|a| a.device != dev as usize) {
                *g = None;
            }
            if g.is_none() {
                match Look::new(dev, blobs) {
                    Some(a) => *g = Some(a),
                    None => {
                        FAILED.store(true, Relaxed);
                        log!("look: could not be created; lens effects and colour grading stay off");
                        break 'run None;
                    }
                }
            }
            let a = g.as_mut().expect("just created");
            a.run(dev, ctx, srv, (width, height), &inp)
        };
        release(dev);
        let Some(out) = out else {
            release(srv);
            LAST_SKIP.store(SKIP_RUN, Relaxed);
            return None;
        };
        let set_srv: unsafe extern "system" fn(Obj, u32, u32, *const Obj) = std::mem::transmute(vfn(ctx, vt::CTX_PS_SET_SRVS));
        set_srv(ctx, 0, 1, &out);
        DONE.store(true, Relaxed);
        LAST_SKIP.store(SKIP_NONE, Relaxed);
        let mode = (inp.lens.is_some() as u32) | ((inp.do_grade() as u32) << 1) | ((inp.do_lut() as u32) << 2);
        LAST_MODE.store(mode, Relaxed);
        if inp.lens.is_some() && LENS_APPLIED.fetch_add(1, Relaxed) == 0 {
            log!("look: lens effects applied for the first time ({width}x{height})");
        }
        if inp.do_grade() && GRADE_APPLIED.fetch_add(1, Relaxed) == 0 {
            log!("look: colour grading applied for the first time ({:?})", grading::get());
        }
        if inp.do_lut() {
            LUT_APPLIED.fetch_add(1, Relaxed);
        }
        Some(srv)
    }
}

fn common_wait_text() -> Option<String> {
    match LAST_SKIP.load(Relaxed) {
        SKIP_COMPILING => Some("waiting, the shaders are still compiling".to_string()),
        SKIP_SHAPE => Some(format!("skipped, the copy pass does not have the expected shape ({} draws)", SKIPPED_SHAPE.load(Relaxed))),
        SKIP_RUN => Some("skipped, the targets or constants could not be set up (see log)".to_string()),
        _ if BLOBS.get().is_none() => Some("compiling shaders".to_string()),
        _ => None,
    }
}

/// Debug capture / overlay line of the lens effects.
pub fn status_lens() -> String {
    if !lens::on() {
        return "off (lens_strength 0)".to_string();
    }
    if !crate::gate::user_enabled() {
        return "off (mod disabled)".to_string();
    }
    let what = format!(
        "strength {:.2}, threshold {:.1}x, ghost {:.2}, streak length {:.2}, max source size {:.3}, colour reject {:.2}",
        lens::strength(),
        lens::threshold(),
        lens::ghost(),
        lens::streak_length(),
        lens::max_size(),
        lens::color_reject()
    );
    if FAILED.load(Relaxed) {
        return format!("{what}: failed (see log), off");
    }
    if crate::datapath::current_params(false, false).sdr_clamp {
        return format!("{what}: off, SDR output (nothing above paper white)");
    }
    let n = LENS_APPLIED.load(Relaxed);
    if n > 0 {
        let live = LAST_MODE.load(Relaxed) & 1 != 0;
        return format!("{what}: applied at the pre-UI copy ps#{COPY_HASH:08x}, {n} frames{}", if live { "" } else { " (not in the last frame)" });
    }
    match common_wait_text() {
        Some(t) => format!("{what}: {t}"),
        None => format!("{what}: waiting for the pre-UI copy pass (ps#{COPY_HASH:08x} seen {} times)", COPY_SEEN.load(Relaxed)),
    }
}

/// Debug capture / overlay line of the colour grading (sliders and LUT).
pub fn status_grade() -> String {
    let g = grading::get();
    let lut = grading::lut_status_text();
    let lut_part = format!("LUT: {lut}, strength {:.2}", grading::lut_strength());
    if g.is_neutral() && !grading::any_on() {
        return format!("off (sliders neutral); {lut_part}");
    }
    if !crate::gate::user_enabled() {
        return "off (mod disabled)".to_string();
    }
    let sliders = if g.is_neutral() {
        "sliders neutral".to_string()
    } else {
        format!("sat {:.2} contrast {:.2} temp {:.2} tint {:.2} vibrance {:.2} shadows {:.2} highlights {:.2}", g.saturation, g.contrast, g.temperature, g.tint, g.vibrance, g.shadows, g.highlights)
    };
    if FAILED.load(Relaxed) {
        return format!("{sliders}; {lut_part}: failed (see log), off");
    }
    let n = GRADE_APPLIED.load(Relaxed);
    let m = LUT_APPLIED.load(Relaxed);
    if n > 0 || m > 0 {
        return format!("{sliders}; {lut_part}; applied at the pre-UI copy ps#{COPY_HASH:08x}: sliders {n} frames, LUT {m} frames");
    }
    match common_wait_text() {
        Some(t) => format!("{sliders}; {lut_part}: {t}"),
        None => format!("{sliders}; {lut_part}: waiting for the pre-UI copy pass (ps#{COPY_HASH:08x} seen {} times)", COPY_SEEN.load(Relaxed)),
    }
}

#[cfg(test)]
#[path = "look_tests.rs"]
mod tests;
