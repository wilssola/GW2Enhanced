//! Bloom tint compensation: the bloom must not recolour the picture.
//!
//! WHY. The engine's combine pass (`ps#37c5610f`) adds the bloom as `rgb += bloom.rgb * cb0[0].xyz * 2.0`, where
//! `cb0[0].xyz` is the environment's bloom colour (usually cool/blue). In the 8-bit original every bright pixel was
//! clipped at 1.0 afterwards, so that tint was only visible in the dim tails. The scene is float now and nothing
//! clips: a big bright area such as the sun receives a large amount of tinted light and turns blue as soon as the
//! bloom strength is raised (seen in the game: white at bloom 0, blue otherwise).
//!
//! HOW. The tint is a constant of the combine pass, so the bloom shader (`bloom_threshold`, our substitute of
//! `ps#14b1e90e`) divides it out beforehand: it multiplies the bloom by `mean(t) / t` per channel, so after the
//! combine's multiplication by `t` the added light is grey with the same average energy. The tint is read from the
//! combine's own constant buffer: at the combine draw the first 16 bytes of the bound CB0 are copied into a ring of
//! three staging buffers and the copy of two frames ago is read without waiting (`DO_NOT_WAIT`), so the render
//! thread never stalls. The bloom shader runs earlier in the frame than the combine, so it uses the value of the
//! previous frames; the tint is an environment constant that changes only when the area does.
//!
//! The compensation reaches the bloom shader through `b6` (one float4: xyz factors, w = valid), bound at its draw
//! (`datapath::on_draw`, `KIND_BLOOM`). Unbound it reads zero = no compensation (the equivalence tests).

use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};

use crate::d3d::{IgnoreScope, Obj, ignoring, release, vfn, vt};
use crate::log;

/// `ps#37c5610f`, the combine pass.
pub const COMBINE_HASH: u32 = 0x37c5610f;
/// `b6` of the bloom-threshold substitute.
pub const SLOT: u32 = 6;

static TINT: [AtomicU32; 3] = [AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0)];
static TINT_KNOWN: AtomicBool = AtomicBool::new(false);

/// The bloom colour of the combine pass as last read, if any.
pub fn tint() -> Option<[f32; 3]> {
    TINT_KNOWN.load(Relaxed).then(|| [f32::from_bits(TINT[0].load(Relaxed)), f32::from_bits(TINT[1].load(Relaxed)), f32::from_bits(TINT[2].load(Relaxed))])
}

/// Accepts a value read from the game's buffer: finite, non-negative and plausible (a bloom colour is about 0..2).
pub fn plausible(t: [f32; 3]) -> bool {
    t.iter().all(|v| v.is_finite() && (0.0..=64.0).contains(v)) && t.iter().sum::<f32>() > 0.03
}

/// Per-channel factors `mean(t) / t` (clamped to 0.25..4): after the combine multiplies by `t` the added light is
/// grey. None for an implausible tint.
pub fn compensation_for(t: [f32; 3]) -> Option<[f32; 3]> {
    if !plausible(t) {
        return None;
    }
    let mean = (t[0] + t[1] + t[2]) / 3.0;
    Some(t.map(|c| (mean / c.max(0.05)).clamp(0.25, 4.0)))
}

/// The factors for the bloom shader right now: None while the tint is unknown or the user switched the mod off
/// (the vanilla emulation reproduces the original bloom, tint included).
pub fn compensation() -> Option<[f32; 3]> {
    if !crate::gate::user_enabled() {
        return None;
    }
    compensation_for(tint()?)
}

/// Overlay line.
pub fn status_text() -> String {
    match (tint(), compensation()) {
        (Some(t), Some(c)) => format!("bloom tint ({:.2}, {:.2}, {:.2}) neutralised by x({:.2}, {:.2}, {:.2})", t[0], t[1], t[2], c[0], c[1], c[2]),
        (Some(t), None) => format!("bloom tint ({:.2}, {:.2}, {:.2}), compensation off", t[0], t[1], t[2]),
        _ => "bloom tint not read yet".to_string(),
    }
}

// ---- reading the tint from the combine's constant buffer ------------------------------------------------

const RING: usize = 3;

struct Ring {
    device: usize,
    staging: [Obj; RING],
    write: usize,
    primed: usize,
    /// The 16-byte constant buffer of the bloom shader.
    cb: Obj,
}

unsafe impl Send for Ring {}

impl Drop for Ring {
    fn drop(&mut self) {
        unsafe {
            for o in self.staging {
                release(o);
            }
            release(self.cb);
        }
    }
}

static RING_STATE: Mutex<Option<Ring>> = Mutex::new(None);

unsafe fn create_buffer(dev: Obj, usage: u32, bind: u32, cpu: u32, bytes: u32) -> Obj {
    unsafe {
        // D3D11_BUFFER_DESC { ByteWidth, Usage, BindFlags, CPUAccessFlags, MiscFlags, StructureByteStride }
        let desc: [u32; 6] = [bytes, usage, bind, cpu, 0, 0];
        let create: unsafe extern "system" fn(Obj, *const [u32; 6], *const c_void, *mut Obj) -> i32 = std::mem::transmute(vfn(dev, vt::DEV_CREATE_BUFFER));
        let mut out: Obj = std::ptr::null_mut();
        if create(dev, &desc, std::ptr::null(), &mut out) < 0 {
            return std::ptr::null_mut();
        }
        out
    }
}

impl Ring {
    unsafe fn new(dev: Obj) -> Option<Ring> {
        let _ig = IgnoreScope::new();
        let nul = std::ptr::null_mut();
        let mut r = Ring { device: dev as usize, staging: [nul; RING], write: 0, primed: 0, cb: nul };
        unsafe {
            for i in 0..RING {
                r.staging[i] = create_buffer(dev, 3 /* STAGING */, 0, 0x20000 /* CPU READ */, 16);
                if r.staging[i].is_null() {
                    return None;
                }
            }
            r.cb = create_buffer(dev, 2 /* DYNAMIC */, 4 /* CONSTANT_BUFFER */, 0x10000 /* CPU WRITE */, 16);
            if r.cb.is_null() {
                return None;
            }
        }
        Some(r)
    }
}

/// Copies the first 16 bytes of `src` (a buffer) into the staging buffer `dst`.
pub(crate) unsafe fn copy_head(ctx: Obj, dst: Obj, src: Obj) {
    unsafe {
        // D3D11_BOX { left, top, front, right, bottom, back }: for a buffer the range is in bytes.
        let bx: [u32; 6] = [0, 0, 0, 16, 1, 1];
        let copy: unsafe extern "system" fn(Obj, Obj, u32, u32, u32, u32, Obj, u32, *const [u32; 6]) = std::mem::transmute(vfn(ctx, vt::CTX_COPY_SUBRESOURCE_REGION));
        copy(ctx, dst, 0, 0, 0, 0, src, 0, &bx);
    }
}

/// Maps a staging buffer for reading and returns its first three floats. `flags` 0 waits, `0x100000` is
/// `DO_NOT_WAIT` (None when the copy is not finished yet).
pub(crate) unsafe fn read_head(ctx: Obj, staging: Obj, flags: u32) -> Option<[f32; 3]> {
    unsafe {
        let mut mapped: [usize; 3] = [0; 3]; // D3D11_MAPPED_SUBRESOURCE { pData, RowPitch, DepthPitch }
        let map: unsafe extern "system" fn(Obj, Obj, u32, u32, u32, *mut [usize; 3]) -> i32 = std::mem::transmute(vfn(ctx, vt::CTX_MAP));
        if map(ctx, staging, 0, 1 /* READ */, flags, &mut mapped) < 0 || mapped[0] == 0 {
            return None;
        }
        let p = mapped[0] as *const f32;
        let v = [*p, *p.add(1), *p.add(2)];
        let unmap: unsafe extern "system" fn(Obj, Obj, u32) = std::mem::transmute(vfn(ctx, vt::CTX_UNMAP));
        unmap(ctx, staging, 0);
        Some(v)
    }
}

static COMBINE_DRAWS: AtomicU32 = AtomicU32::new(0);
static RAW_LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// A draw of the combine pass is about to be issued on the game's context: samples its bloom colour (about twice a
/// second). Never waits for the GPU; never unwinds.
pub fn on_combine_draw(ctx: Obj) {
    let n = COMBINE_DRAWS.fetch_add(1, Relaxed);
    if n == 0 {
        log!("bloom: first combine draw seen");
    }
    if ignoring() || !crate::gate::user_enabled() || n % 30 != 0 {
        return;
    }
    let _ = std::panic::catch_unwind(|| unsafe { sample(ctx) });
}

unsafe fn sample(ctx: Obj) {
    unsafe {
        let mut dev: Obj = std::ptr::null_mut();
        let get_device: unsafe extern "system" fn(Obj, *mut Obj) = std::mem::transmute(vfn(ctx, vt::CHILD_GET_DEVICE));
        get_device(ctx, &mut dev);
        if dev.is_null() {
            return;
        }
        let mut buf: Obj = std::ptr::null_mut();
        let get_cbs: unsafe extern "system" fn(Obj, u32, u32, *mut Obj) = std::mem::transmute(vfn(ctx, vt::CTX_PS_GET_CBS));
        get_cbs(ctx, 0, 1, &mut buf);
        if !buf.is_null() {
            if let Ok(mut g) = RING_STATE.try_lock() {
                if g.as_ref().is_some_and(|r| r.device != dev as usize) {
                    *g = None;
                }
                if g.is_none() {
                    *g = Ring::new(dev);
                }
                if let Some(r) = g.as_mut() {
                    // Read the copy made two draws-of-sampling ago, then issue a new copy into the oldest slot.
                    if r.primed >= RING - 1 {
                        let read = r.staging[(r.write + 1) % RING];
                        if let Some(t) = read_head(ctx, read, 0x10_0000) {
                            if !RAW_LOGGED.swap(true, Relaxed) {
                                log!("bloom: first raw read ({:.3}, {:.3}, {:.3}) plausible={}", t[0], t[1], t[2], plausible(t));
                            }
                            if plausible(t) {
                                let first = !TINT_KNOWN.swap(true, Relaxed);
                                let changed = first || (0..3).any(|i| (f32::from_bits(TINT[i].load(Relaxed)) - t[i]).abs() > 0.01);
                                for i in 0..3 {
                                    TINT[i].store(t[i].to_bits(), Relaxed);
                                }
                                if changed {
                                    if let Some(c) = compensation_for(t) {
                                        log!("bloom: combine tint ({:.3}, {:.3}, {:.3}), compensation x({:.3}, {:.3}, {:.3})", t[0], t[1], t[2], c[0], c[1], c[2]);
                                    }
                                }
                            }
                        }
                    }
                    copy_head(ctx, r.staging[r.write], buf);
                    r.write = (r.write + 1) % RING;
                    r.primed = (r.primed + 1).min(RING);
                }
            }
        }
        release(buf);
        release(dev);
    }
}

/// A draw of the bloom-threshold substitute is about to be issued: binds `b6` with the compensation. Never unwinds.
pub fn on_bloom_draw(ctx: Obj) {
    if ignoring() {
        return;
    }
    let _ = std::panic::catch_unwind(|| unsafe { bind(ctx) });
}

unsafe fn bind(ctx: Obj) {
    unsafe {
        let Ok(g) = RING_STATE.try_lock() else { return };
        let Some(r) = g.as_ref() else { return };
        let mut dev: Obj = std::ptr::null_mut();
        let get_device: unsafe extern "system" fn(Obj, *mut Obj) = std::mem::transmute(vfn(ctx, vt::CHILD_GET_DEVICE));
        get_device(ctx, &mut dev);
        let same = dev as usize == r.device;
        release(dev);
        if !same {
            return;
        }
        let values: [f32; 4] = match compensation() {
            Some(c) => [c[0], c[1], c[2], 1.0],
            None => [1.0, 1.0, 1.0, 0.0],
        };
        let mut mapped: [usize; 3] = [0; 3];
        let map: unsafe extern "system" fn(Obj, Obj, u32, u32, u32, *mut [usize; 3]) -> i32 = std::mem::transmute(vfn(ctx, vt::CTX_MAP));
        if map(ctx, r.cb, 0, 4 /* WRITE_DISCARD */, 0, &mut mapped) < 0 || mapped[0] == 0 {
            return;
        }
        *(mapped[0] as *mut [f32; 4]) = values;
        let unmap: unsafe extern "system" fn(Obj, Obj, u32) = std::mem::transmute(vfn(ctx, vt::CTX_UNMAP));
        unmap(ctx, r.cb, 0);
        let set_cb: unsafe extern "system" fn(Obj, u32, u32, *const Obj) = std::mem::transmute(vfn(ctx, vt::CTX_PS_SET_CBS));
        set_cb(ctx, SLOT, 1, &r.cb);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_neutral_tint_needs_no_compensation() {
        let c = compensation_for([0.8, 0.8, 0.8]).unwrap();
        for v in c {
            assert!((v - 1.0).abs() < 1e-6);
        }
    }

    #[test]
    fn a_blue_tint_is_divided_out_keeping_the_average_energy() {
        let t = [0.5f32, 0.8, 1.2];
        let c = compensation_for(t).unwrap();
        assert!(c[0] > 1.0 && c[2] < 1.0, "{c:?}");
        // After the combine multiplies by t, the added light is grey with the tint's mean.
        let mean = (t[0] + t[1] + t[2]) / 3.0;
        for i in 0..3 {
            assert!((c[i] * t[i] - mean).abs() < 1e-5, "ch {i}: {} vs {mean}", c[i] * t[i]);
        }
    }

    #[test]
    fn the_factors_are_clamped_and_garbage_is_rejected() {
        let c = compensation_for([0.0, 1.0, 1.0]).unwrap();
        assert!(c.iter().all(|v| (0.25..=4.0).contains(v)), "{c:?}");
        assert!(compensation_for([f32::NAN, 1.0, 1.0]).is_none());
        assert!(compensation_for([-1.0, 1.0, 1.0]).is_none());
        assert!(compensation_for([0.0, 0.0, 0.0]).is_none());
        assert!(compensation_for([1000.0, 1.0, 1.0]).is_none());
    }

    #[test]
    fn status_text_runs_before_any_read() {
        assert!(!status_text().is_empty());
    }

    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::*;
    use windows::Win32::Graphics::Direct3D11::*;
    use windows::core::Interface;

    #[test]
    fn the_head_of_a_constant_buffer_is_read_back_on_warp() {
        let mut dev = None;
        let mut ctx = None;
        let r = unsafe { D3D11CreateDevice(None::<&windows::Win32::Graphics::Dxgi::IDXGIAdapter>, D3D_DRIVER_TYPE_WARP, HMODULE::default(), D3D11_CREATE_DEVICE_FLAG(0), None, D3D11_SDK_VERSION, Some(&mut dev), None, Some(&mut ctx)) };
        let (Ok(()), Some(dev), Some(ctx)) = (r, dev, ctx) else {
            println!("skipped: WARP device creation failed");
            return;
        };
        unsafe {
            // A default-usage constant buffer like the game's, 144 bytes (CB0[9] of the combine), first float4 = the tint.
            let mut data = [0f32; 36];
            data[..4].copy_from_slice(&[0.55, 0.75, 1.1, 0.0]);
            data[4..8].copy_from_slice(&[9.0, 9.0, 9.0, 9.0]);
            let bd = D3D11_BUFFER_DESC { ByteWidth: 144, Usage: D3D11_USAGE_DEFAULT, BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32, ..Default::default() };
            let sd = D3D11_SUBRESOURCE_DATA { pSysMem: data.as_ptr() as *const c_void, SysMemPitch: 0, SysMemSlicePitch: 0 };
            let mut cb = None;
            dev.CreateBuffer(&bd, Some(&sd), Some(&mut cb)).unwrap();
            let cb = cb.unwrap();
            let staging = create_buffer(dev.as_raw(), 3, 0, 0x20000, 16);
            assert!(!staging.is_null());
            copy_head(ctx.as_raw(), staging, cb.as_raw());
            let t = read_head(ctx.as_raw(), staging, 0).expect("read back");
            assert_eq!(t, [0.55, 0.75, 1.1]);
            assert!(plausible(t));
            release(staging);
        }
    }

    #[test]
    fn a_ring_and_the_constant_buffer_are_created_and_bound_on_warp() {
        let mut dev = None;
        let mut ctx = None;
        let r = unsafe { D3D11CreateDevice(None::<&windows::Win32::Graphics::Dxgi::IDXGIAdapter>, D3D_DRIVER_TYPE_WARP, HMODULE::default(), D3D11_CREATE_DEVICE_FLAG(0), None, D3D11_SDK_VERSION, Some(&mut dev), None, Some(&mut ctx)) };
        let (Ok(()), Some(dev), Some(ctx)) = (r, dev, ctx) else {
            println!("skipped: WARP device creation failed");
            return;
        };
        unsafe {
            let ring = Ring::new(dev.as_raw()).expect("ring");
            let set_cb: unsafe extern "system" fn(Obj, u32, u32, *const Obj) = std::mem::transmute(vfn(ctx.as_raw(), vt::CTX_PS_SET_CBS));
            set_cb(ctx.as_raw(), SLOT, 1, &ring.cb);
            let mut cbs = [None];
            ctx.PSGetConstantBuffers(SLOT, Some(&mut cbs));
            assert!(cbs[0].is_some());
        }
    }
}
