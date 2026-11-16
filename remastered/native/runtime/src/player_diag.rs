//! Diagnostics for the per-character composite scale (log only, changes nothing).
//!
//! Confirms, in game, how the DLL can tell the local player's composite apart
//! (docs/texture-reverse-engineering.md, "Escala por personagem"):
//!   * `AvCharModel` composite creation (0x1166580): rdx = character, r8d = flags.
//!   * controlled character = `ctx = 0x9b1fb0(); obj = *(ctx+0x98); obj->vt[0x68/8](obj)`.
//!   * player composite ctor (0x12d16b0): rcx = composite, r8d = flags; after it
//!     `+0xb1c` holds the composite flags (expected 0x213 for the local player).
//! Every hook forwards up to 10 integer arguments unchanged (extra ones are just
//! the caller's stack slots), so functions with stack arguments still work.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};

use retour::GenericDetour;

use crate::game::Image;
use crate::log;
use crate::sig::Site;

const CHAR_MODEL_COMPOSITE: Site = Site {
    name: "avcharmodel_composite",
    sig: "48 89 5C 24 08 55 56 57 41 54 41 55 41 56 41 57 48 8D 6C 24 80 48 81 EC",
};
const CONTEXT: Site = Site { name: "context_tls", sig: "8B 0D ?? ?? ?? ?? 65 48 8B 04 25 58 00 00 00 BA 10 00 00 00" };
const PLAYER_CTOR: Site = Site {
    name: "composite_player_ctor",
    sig: "48 89 5C 24 08 48 89 6C 24 10 48 89 74 24 18 57 41 54 41 55 41 56 41 57 48 83 EC 40 48 8D 05",
};

type F10 = unsafe extern "system" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64;
type Ctx = unsafe extern "system" fn() -> usize;

static MODEL: OnceLock<GenericDetour<F10>> = OnceLock::new();
static CTOR: OnceLock<GenericDetour<F10>> = OnceLock::new();
static CTX: OnceLock<usize> = OnceLock::new();
static BUDGET: AtomicU32 = AtomicU32::new(400); // log lines, to keep the log small

thread_local! {
    static PENDING: std::cell::Cell<(u64, u64, u32)> = const { std::cell::Cell::new((0, 0, 0)) };
}

/// Controlled character pointer, or 0 (every step null-checked).
fn controlled() -> u64 {
    let Some(&f) = CTX.get() else { return 0 };
    let r = std::panic::catch_unwind(|| unsafe {
        let ctx = (std::mem::transmute::<usize, Ctx>(f))();
        if ctx == 0 {
            return 0;
        }
        let obj = ((ctx + 0x98) as *const usize).read();
        if obj == 0 {
            return 0;
        }
        let vt = (obj as *const usize).read();
        if vt == 0 {
            return 0;
        }
        let get = ((vt + 0x68) as *const usize).read();
        if get == 0 {
            return 0;
        }
        let g: unsafe extern "system" fn(usize) -> u64 = std::mem::transmute(get);
        g(obj)
    });
    r.unwrap_or(0)
}

fn budget() -> bool {
    BUDGET.try_update(Ordering::Relaxed, Ordering::Relaxed, |b| b.checked_sub(1)).is_ok()
}

unsafe extern "system" fn model_hook(a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64, h: u64, i: u64, j: u64) -> u64 {
    let ctrl = controlled();
    PENDING.with(|p| p.set((b, ctrl, c as u32)));
    let r = unsafe { MODEL.get().unwrap().call(a, b, c, d, e, f, g, h, i, j) };
    PENDING.with(|p| p.set((0, 0, 0)));
    r
}

unsafe extern "system" fn ctor_hook(a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64, h: u64, i: u64, j: u64) -> u64 {
    let (ch, ctrl, mflags) = PENDING.with(|p| p.get());
    // Local player: flag bit 0 (set for the controlled character, 0x213 in the
    // world, and for its UI previews: character select 0x3, paper doll 0x205);
    // other players get 0x12/0x1a. Confirmed in game. The
    // AvCharModel character == controlled check covers the case where it is set.
    let own = (c as u32 & 1) != 0 || (ch != 0 && ch == ctrl);
    // No character in the world yet (neither the model's nor the controlled one): the character select's previews.
    let select = ch == 0 && ctrl == 0;
    let (r, scale) = crate::composite::player_ctor(a as usize, c as u32, own, select, || unsafe { CTOR.get().unwrap().call(a, b, c, d, e, f, g, h, i, j) });
    if budget() {
        log!("player-diag: composite {:#x} scale {scale}x", a);
        let b1c = if a != 0 { unsafe { ((a as usize + 0xb1c) as *const u32).read_unaligned() } } else { 0 };
        log!(
            "player-diag: composite {:#x} ctor_flags {:#x} b1c {:#x} | from AvCharModel: character {:#x} controlled {:#x} own={} model_flags {:#x}",
            a,
            c as u32,
            b1c,
            ch,
            ctrl,
            own,
            mflags
        );
    }
    r
}

fn hook(img: &Image, site: &Site, cell: &OnceLock<GenericDetour<F10>>, f: F10) -> bool {
    let Some(addr) = img.find(site) else {
        log!("player-diag: {} not found", site.name);
        return false;
    };
    let t: F10 = unsafe { std::mem::transmute(addr) };
    match unsafe { GenericDetour::new(t, f) } {
        Ok(d) => {
            let _ = cell.set(d);
            match unsafe { cell.get().unwrap().enable() } {
                Ok(()) => {
                    log!("player-diag: {} hooked at rva {:#x}", site.name, img.rva(addr));
                    true
                }
                Err(e) => {
                    log!("player-diag: {} enable failed: {e}", site.name);
                    false
                }
            }
        }
        Err(e) => {
            log!("player-diag: {} detour failed: {e}", site.name);
            false
        }
    }
}

pub fn install(img: &Image) {
    // CONTEXT signature matches inside the function body; the accessor starts there
    // only if it is the function entry — verify against .pdata-free heuristic: the
    // byte before must be an int3/ret padding.
    match img.find(&CONTEXT) {
        Some(a) if matches!(unsafe { ((a - 1) as *const u8).read() }, 0xCC | 0xC3 | 0x90) => {
            let _ = CTX.set(a);
            log!("player-diag: context accessor at rva {:#x}", img.rva(a));
        }
        Some(a) => log!("player-diag: context signature at rva {:#x} is not a function start; controlled() disabled", img.rva(a)),
        None => log!("player-diag: context accessor not found"),
    }
    if hook(img, &PLAYER_CTOR, &CTOR, ctor_hook as F10) {
        hook(img, &CHAR_MODEL_COMPOSITE, &MODEL, model_hook as F10);
    }
}
