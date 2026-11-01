//! Guards + diagnostics for the engine's device-texture resize (`BgfxTexture.cpp`).
//!
//! `UpRes(tex, format, data, dims)` (0xb15110) requires current == dims / 2 and
//! `DownRes(tex, dims)` (0xb14df0) requires current == dims * 2; otherwise the
//! engine aborts with "bad dimensions". With HD textures those relations can
//! break, so an impossible request is refused (return 0 = the engine's own
//! "resize failed, keep current texture" path) instead of crashing. In the
//! unmodified game the guard never triggers. Each (texture, request) is logged once.

use std::sync::OnceLock;

use retour::GenericDetour;

use crate::diag::{Event, send};
use crate::game::Image;
use crate::log;
use crate::sig::Site;

pub const DOWNRES: Site = Site { name: "bgfx_downres", sig: "40 55 56 48 81 EC 88 00 00 00 83 79 04 00 48 8B EA 48 8B F1 74 19 41 B8" };
pub const UPRES: Site = Site { name: "bgfx_upres", sig: "4C 89 44 24 18 89 54 24 10 55 56 57 41 56 48 81 EC A8 00 00 00 83 79 04" };

type DownRes = unsafe extern "system" fn(*mut u8, *const u32) -> u64;
type UpRes = unsafe extern "system" fn(*mut u8, u32, u64, *const u32) -> u64;
static DOWN: OnceLock<GenericDetour<DownRes>> = OnceLock::new();
static UP: OnceLock<GenericDetour<UpRes>> = OnceLock::new();

/// BgfxTexture: +0x04 type (0 = 2D), +0x14/+0x18 current dims, +0x48 flags.
/// The engine retries a refused resize every frame; log each (texture, request) once.
fn log_resize(tex: *mut u8, up: bool, cur: (u32, u32), req: (u32, u32), refused: bool) {
    static SEEN: std::sync::Mutex<Option<std::collections::HashSet<(usize, bool, u32, u32)>>> = std::sync::Mutex::new(None);
    let mut g = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    let set = g.get_or_insert_with(Default::default);
    if set.len() > 200_000 {
        set.clear();
    }
    if set.insert((tex as usize, up, req.0, req.1)) {
        send(Event::Resize { up, cur, req, refused });
    }
}

unsafe fn cur(tex: *mut u8) -> (u32, u32, u32) {
    unsafe { (tex.add(0x14).cast::<u32>().read(), tex.add(0x18).cast::<u32>().read(), tex.add(0x04).cast::<u32>().read()) }
}

unsafe extern "system" fn downres(tex: *mut u8, dims: *const u32) -> u64 {
    if !tex.is_null() && !dims.is_null() {
        let (w, h, ty) = unsafe { cur(tex) };
        let (rw, rh) = unsafe { (dims.read(), dims.add(1).read()) };
        // the engine only acts when the texture is a 2D, non-failed one and exactly 2× the request
        let ok = ty != 0 || (w == rw * 2 && h == rh * 2);
        log_resize(tex, false, (w, h), (rw, rh), !ok);
        if !ok {
            crate::overlay::N_REFUSED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return 0;
        }
    }
    unsafe { DOWN.get().unwrap().call(tex, dims) }
}

unsafe extern "system" fn upres(tex: *mut u8, format: u32, data: u64, dims: *const u32) -> u64 {
    if !tex.is_null() && !dims.is_null() {
        let (w, h, ty) = unsafe { cur(tex) };
        let (rw, rh) = unsafe { (dims.read(), dims.add(1).read()) };
        let ok = ty != 0 || (w * 2 == rw && h * 2 == rh);
        log_resize(tex, true, (w, h), (rw, rh), !ok);
        if !ok {
            crate::overlay::N_REFUSED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return 0;
        }
    }
    unsafe { UP.get().unwrap().call(tex, format, data, dims) }
}

fn hook<F: retour::Function + Copy>(img: &Image, site: &Site, cell: &OnceLock<GenericDetour<F>>, f: F) {
    let Some(addr) = img.find(site) else {
        log!("{}: signature not found; guard disabled", site.name);
        return;
    };
    let target: F = unsafe { F::from_ptr(addr as *const ()) };
    match unsafe { GenericDetour::new(target, f) } {
        Ok(d) => {
            let _ = cell.set(d);
            match unsafe { cell.get().unwrap().enable() } {
                Ok(()) => log!("{}: guarded at rva {:#x}", site.name, img.rva(addr)),
                Err(e) => log!("{}: enable failed: {e}", site.name),
            }
        }
        Err(e) => log!("{}: detour failed: {e}", site.name),
    }
}

pub fn install(img: &Image) {
    hook(img, &DOWNRES, &DOWN, downres as DownRes);
    hook(img, &UPRES, &UP, upres as UpRes);
}
