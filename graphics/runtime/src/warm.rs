//! Shader warm-up off the render thread (v19). Creating a shader object makes the driver translate its bytecode, tens of ms or more
//! for the fur's generated shaders, and the game's device is created single-threaded (D3D11_CREATE_DEVICE_SINGLETHREADED), so its
//! objects may only be made on the render thread: each new kind of character on screen was a stutter. The warm-up makes every compiled
//! blob once, on a background thread, on a device of OUR OWN (free-threaded, on the game's adapter): the driver translates it there and
//! keeps the result in its shader cache, so the game device's own create of the same bytecode later is quick. A free-threaded game
//! device is used directly.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

use crate::d3d::{IgnoreScope, Obj, release, vfn, vt};

/// The game's device when free-threaded (1 = single-threaded: use our own).
static DEVICE: AtomicUsize = AtomicUsize::new(0);
/// The game's DXGI adapter (our device goes on the same GPU).
static ADAPTER: AtomicUsize = AtomicUsize::new(0);
/// Our own device: 0 = not tried, 1 = failed, else the device.
static OWN: Mutex<usize> = Mutex::new(0);

const DEV_CREATE_GEOMETRY_SHADER: usize = 13;
const DEV_GET_CREATION_FLAGS: usize = 38;
const CREATE_DEVICE_SINGLETHREADED: u32 = 0x1;
const ADDREF: usize = 1;
const QUERY_INTERFACE: usize = 0;
const DXGI_DEVICE_GET_ADAPTER: usize = 7;
/// IID_IDXGIDevice {54ec77fa-1377-44e6-8c32-88fd5f44c84c}
const IID_DXGI_DEVICE: [u8; 16] = [0xfa, 0x77, 0xec, 0x54, 0x77, 0x13, 0xe6, 0x44, 0x8c, 0x32, 0x88, 0xfd, 0x5f, 0x44, 0xc8, 0x4c];

/// The game's device, seen on the render thread: kept when free-threaded, else its adapter for our own device.
pub fn note_device(dev: Obj) {
    if dev.is_null() || DEVICE.load(Relaxed) != 0 {
        return;
    }
    unsafe {
        let flags: unsafe extern "system" fn(Obj) -> u32 = std::mem::transmute(vfn(dev, DEV_GET_CREATION_FLAGS));
        if flags(dev) & CREATE_DEVICE_SINGLETHREADED != 0 {
            if DEVICE.compare_exchange(0, 1, Relaxed, Relaxed).is_ok() {
                let qi: unsafe extern "system" fn(Obj, *const [u8; 16], *mut Obj) -> i32 = std::mem::transmute(vfn(dev, QUERY_INTERFACE));
                let mut dxgi: Obj = std::ptr::null_mut();
                if qi(dev, &IID_DXGI_DEVICE, &mut dxgi) >= 0 && !dxgi.is_null() {
                    let get_adapter: unsafe extern "system" fn(Obj, *mut Obj) -> i32 = std::mem::transmute(vfn(dxgi, DXGI_DEVICE_GET_ADAPTER));
                    let mut a: Obj = std::ptr::null_mut();
                    if get_adapter(dxgi, &mut a) >= 0 && !a.is_null() {
                        ADAPTER.store(a as usize, Relaxed);
                    }
                    release(dxgi);
                }
                crate::log!("warm: the game's device is single-threaded: the warm-up uses a device of its own on the same adapter");
            }
            return;
        }
        if DEVICE.compare_exchange(0, dev as usize, Relaxed, Relaxed).is_ok() {
            let add_ref: unsafe extern "system" fn(Obj) -> u32 = std::mem::transmute(vfn(dev, ADDREF));
            add_ref(dev);
        }
    }
}

type CreateDevice = unsafe extern "system" fn(Obj, u32, usize, u32, *const u32, u32, u32, *mut Obj, *mut u32, *mut Obj) -> i32;

/// Our own free-threaded device (made on first use, on a background thread).
fn own_device() -> Option<Obj> {
    let mut g = OWN.lock().unwrap_or_else(|e| e.into_inner());
    if *g == 0 {
        *g = 1;
        unsafe {
            use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};
            let m = LoadLibraryA(c"d3d11.dll".as_ptr().cast());
            if let Some(f) = (!m.is_null()).then(|| GetProcAddress(m, c"D3D11CreateDevice".as_ptr().cast())).flatten() {
                let create: CreateDevice = std::mem::transmute(f);
                let _ig = IgnoreScope::new(); // not hooked: our devices are not the game's
                let a = ADAPTER.load(Relaxed) as Obj;
                let levels: [u32; 1] = [0xb000]; // D3D_FEATURE_LEVEL_11_0
                let (mut dev, mut ctx, mut fl): (Obj, Obj, u32) = (std::ptr::null_mut(), std::ptr::null_mut(), 0);
                // D3D_DRIVER_TYPE_UNKNOWN with the adapter, HARDWARE without; flags 0 = free-threaded.
                let hr = create(a, if a.is_null() { 1 } else { 0 }, 0, 0, levels.as_ptr(), 1, 7, &mut dev, &mut fl, &mut ctx);
                if hr >= 0 && !dev.is_null() {
                    release(ctx);
                    *g = dev as usize;
                    crate::log!("warm: own device ready for the shader warm-up");
                } else {
                    crate::log!("warm: own device could not be created ({hr:#x}); no warm-up");
                }
            }
        }
    }
    (*g > 1).then_some(*g as Obj)
}

fn device() -> Option<Obj> {
    match DEVICE.load(Relaxed) {
        0 => None, // the game's device not seen yet: the adapter is unknown
        1 => own_device(),
        d => Some(d as Obj),
    }
}

/// Whether the game's device was seen (the warm-up can run).
pub fn known() -> bool {
    DEVICE.load(Relaxed) != 0
}

/// Whether blobs are warmed (a device to warm them on exists).
pub fn active() -> bool {
    DEVICE.load(Relaxed) == 1 && *OWN.lock().unwrap_or_else(|e| e.into_inner()) > 1 || DEVICE.load(Relaxed) > 1
}

/// Creates and releases a pixel shader from `blob` on the game's device (a background thread only).
pub fn pixel(blob: &[u8]) {
    create(vt::DEV_CREATE_PIXEL_SHADER, blob);
}

/// As `pixel`, a geometry shader (without stream output).
pub fn geometry(blob: &[u8]) {
    create(DEV_CREATE_GEOMETRY_SHADER, blob);
}

fn create(slot: usize, blob: &[u8]) {
    let Some(dev) = device() else { return };
    if blob.is_empty() {
        return;
    }
    let _ = std::panic::catch_unwind(|| unsafe {
        let _ig = IgnoreScope::new();
        let f: unsafe extern "system" fn(Obj, *const u8, usize, Obj, *mut Obj) -> i32 = std::mem::transmute(vfn(dev, slot));
        let mut o: Obj = std::ptr::null_mut();
        if f(dev, blob.as_ptr(), blob.len(), std::ptr::null_mut(), &mut o) >= 0 {
            release(o);
        }
    });
}

/// v19: at most one new fur shader object per this many ms on the render thread. A crowd entering the screen brought a dozen vertex
/// layouts at once (a dozen geometry shader sets within half a second): created in the same frame, their driver work stacked into one long
/// stall. Spread out, a draw whose object waits simply has no fur for a frame or two.
pub const CREATE_SPACING_MS: u64 = 25;

static LAST_CREATE_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static LOGGED_SLOW: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

fn now_ms() -> u64 {
    static T0: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    T0.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64 + CREATE_SPACING_MS
}

/// Whether the render thread may create a new fur shader object now (takes the turn when it may).
pub fn may_create() -> bool {
    // The tests draw many characters' first frames back to back: their shaders are made at once.
    cfg!(test) || turn(&LAST_CREATE_MS, now_ms())
}

fn turn(last: &std::sync::atomic::AtomicU64, now: u64) -> bool {
    let l = last.load(Relaxed);
    now >= l + CREATE_SPACING_MS && last.compare_exchange(l, now, Relaxed, Relaxed).is_ok()
}

/// Logs a render-thread creation that took long (the first 32): what the stutters cost.
pub fn note_render_create(what: &str, t: std::time::Instant) {
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    if ms >= 4.0 && LOGGED_SLOW.fetch_add(1, Relaxed) < 32 {
        crate::log!("warm: {what} created on the render thread in {ms:.1} ms (warmed: {})", active());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fur_shader_objects_are_created_one_at_a_time() {
        let last = std::sync::atomic::AtomicU64::new(0);
        let t = 1000;
        assert!(turn(&last, t));
        assert!(!turn(&last, t), "the same frame: wait");
        assert!(!turn(&last, t + CREATE_SPACING_MS - 1));
        assert!(turn(&last, t + CREATE_SPACING_MS));
    }
}

/// v19: the fur's shader compiles run on a small pool (a quarter of the cores, 2..4 threads) below normal priority: a thread per
/// compile at normal priority took the CPU from the game in a crowd, and a single thread at the lowest priority starved while the
/// game ran (7 s a shader: the strands came minutes late on a first start).
pub fn compile_job(job: impl FnOnce() + Send + 'static) -> Result<(), ()> {
    use std::sync::mpsc::{Receiver, Sender, channel};
    use std::sync::Arc;
    type Job = Box<dyn FnOnce() + Send>;
    static QUEUE: Mutex<Option<Sender<Job>>> = Mutex::new(None);
    let mut g = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    if g.is_none() {
        let (tx, rx) = channel::<Job>();
        let rx: Arc<Mutex<Receiver<Job>>> = Arc::new(Mutex::new(rx));
        let n = std::thread::available_parallelism().map_or(2, |n| n.get() / 4).clamp(2, 4);
        let mut any = false;
        for k in 0..n {
            let rx = rx.clone();
            any |= std::thread::Builder::new()
                .name(format!("gw2g-fur-compile-{k}"))
                .spawn(move || {
                    unsafe {
                        use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL};
                        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
                    }
                    loop {
                        let j = rx.lock().unwrap_or_else(|e| e.into_inner()).recv();
                        let Ok(j) = j else { break };
                        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(j));
                    }
                })
                .is_ok();
        }
        if !any {
            return Err(());
        }
        *g = Some(tx);
    }
    g.as_ref().ok_or(())?.send(Box::new(job)).map_err(|_| ())
}
