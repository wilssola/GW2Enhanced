//! Finds out which graphics card the translator can use and how much memory it
//! has free, without linking anything GPU-specific:
//!
//! - NVML (`nvml.dll`, installed with every NVIDIA driver): real free memory,
//!   system-wide (other programs, the game included).
//! - Fallback, DXGI: the adapter's dedicated memory and this process' budget.
//!   The budget barely reflects other programs, so it is only a heuristic.

use std::ffi::c_void;

use windows_sys::Win32::Foundation::HMODULE;
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW, SetDllDirectoryW};

/// A graphics card and its free memory, in MB.
#[derive(Debug, Clone)]
pub struct Gpu {
    pub name: String,
    pub free_mb: u64,
    pub total_mb: u64,
    /// `nvml` (exact free memory) or `dxgi` (estimate).
    pub source: &'static str,
    /// DXGI adapter number (what DirectML asks for); only filled by DXGI.
    pub index: u32,
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Adds `dir` (holding `cublas64_12.dll` and `cublasLt64_12.dll`) to the DLL
/// search path, so CTranslate2 finds cuBLAS when it loads it.
pub fn add_dll_dir(dir: &std::path::Path) -> bool {
    let w = wide(&dir.to_string_lossy());
    unsafe { SetDllDirectoryW(w.as_ptr()) != 0 }
}

/// Loads a DLL by its full path, so that later requests by name get this one.
pub fn load_library(path: &std::path::Path) -> bool {
    let w = wide(&path.to_string_lossy());
    unsafe { !LoadLibraryW(w.as_ptr()).is_null() }
}

/// Can cuBLAS be loaded (from the search path, `--gpu-runtime` included)?
pub fn cublas_available() -> bool {
    let w = wide("cublas64_12.dll");
    unsafe { !LoadLibraryW(w.as_ptr()).is_null() }
}

/// The NVIDIA GPU with the most free memory, if any.
pub fn best_nvidia() -> Option<Gpu> {
    nvml().or_else(|| dxgi(true))
}

/// The graphics card of any brand with the most free memory, if any.
pub fn best_adapter() -> Option<Gpu> {
    dxgi(false)
}

unsafe fn sym<T>(lib: HMODULE, name: &str) -> Option<T> {
    let c = std::ffi::CString::new(name).ok()?;
    let p = unsafe { GetProcAddress(lib, c.as_ptr() as *const u8) }?;
    Some(unsafe { std::mem::transmute_copy(&p) })
}

#[repr(C)]
#[derive(Default)]
struct NvmlMemory {
    total: u64,
    free: u64,
    used: u64,
}

fn nvml() -> Option<Gpu> {
    let lib = unsafe { LoadLibraryW(wide("nvml.dll").as_ptr()) };
    if lib.is_null() {
        return None;
    }
    unsafe {
        let init: extern "C" fn() -> i32 = sym(lib, "nvmlInit_v2")?;
        let count: extern "C" fn(*mut u32) -> i32 = sym(lib, "nvmlDeviceGetCount_v2")?;
        let handle: extern "C" fn(u32, *mut *mut c_void) -> i32 = sym(lib, "nvmlDeviceGetHandleByIndex_v2")?;
        let memory: extern "C" fn(*mut c_void, *mut NvmlMemory) -> i32 = sym(lib, "nvmlDeviceGetMemoryInfo")?;
        let get_name: extern "C" fn(*mut c_void, *mut u8, u32) -> i32 = sym(lib, "nvmlDeviceGetName")?;
        let shutdown: extern "C" fn() -> i32 = sym(lib, "nvmlShutdown")?;
        if init() != 0 {
            return None;
        }
        let mut n = 0u32;
        let mut best: Option<Gpu> = None;
        if count(&mut n) == 0 {
            for i in 0..n {
                let mut h = std::ptr::null_mut();
                let mut m = NvmlMemory::default();
                if handle(i, &mut h) != 0 || memory(h, &mut m) != 0 {
                    continue;
                }
                let mut buf = [0u8; 96];
                get_name(h, buf.as_mut_ptr(), buf.len() as u32);
                let name = String::from_utf8_lossy(&buf[..buf.iter().position(|&c| c == 0).unwrap_or(0)]).into_owned();
                let g = Gpu {
                    name,
                    free_mb: m.free >> 20,
                    total_mb: m.total >> 20,
                    source: "nvml",
                    index: 0,
                };
                if best.as_ref().is_none_or(|b| g.free_mb > b.free_mb) {
                    best = Some(g);
                }
            }
        }
        shutdown();
        best
    }
}

// --- DXGI (raw COM vtables: windows-sys 0.61 no longer has the DXGI bindings) ---

#[repr(C)]
struct Luid {
    low: u32,
    high: i32,
}

#[repr(C)]
struct AdapterDesc1 {
    description: [u16; 128],
    vendor_id: u32,
    device_id: u32,
    sub_sys_id: u32,
    revision: u32,
    dedicated_video_memory: usize,
    dedicated_system_memory: usize,
    shared_system_memory: usize,
    luid: Luid,
    flags: u32,
}

#[repr(C)]
#[derive(Default)]
struct VideoMemoryInfo {
    budget: u64,
    current_usage: u64,
    available_for_reservation: u64,
    current_reservation: u64,
}

#[repr(C)]
struct Guid(u32, u16, u16, [u8; 8]);
const IID_FACTORY1: Guid = Guid(
    0x770aae78,
    0xf26f,
    0x4dba,
    [0xa8, 0x29, 0x25, 0x3c, 0x83, 0xd1, 0xb3, 0x87],
);
const IID_ADAPTER3: Guid = Guid(
    0x645967a4,
    0x1392,
    0x4310,
    [0xa7, 0x98, 0x80, 0x53, 0xce, 0x3e, 0x93, 0xfd],
);

unsafe fn slot(obj: *mut c_void, i: usize) -> *const c_void {
    unsafe { *(*(obj as *const *const *const c_void)).add(i) }
}

unsafe fn release(obj: *mut c_void) {
    unsafe {
        let f: extern "system" fn(*mut c_void) -> u32 = std::mem::transmute(slot(obj, 2));
        f(obj);
    }
}

fn dxgi(nvidia_only: bool) -> Option<Gpu> {
    unsafe {
        let lib = LoadLibraryW(wide("dxgi.dll").as_ptr());
        if lib.is_null() {
            return None;
        }
        let create: extern "system" fn(*const Guid, *mut *mut c_void) -> i32 = sym(lib, "CreateDXGIFactory1")?;
        let mut factory = std::ptr::null_mut();
        if create(&IID_FACTORY1, &mut factory) < 0 {
            return None;
        }
        // IDXGIFactory1::EnumAdapters1 = 12; IDXGIAdapter1::GetDesc1 = 10;
        // IUnknown::QueryInterface = 0; IDXGIAdapter3::QueryVideoMemoryInfo = 14.
        let enum1: extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> i32 =
            std::mem::transmute(slot(factory, 12));
        let mut best: Option<Gpu> = None;
        let mut i = 0;
        loop {
            let mut ad = std::ptr::null_mut();
            if enum1(factory, i, &mut ad) < 0 {
                break;
            }
            let index = i;
            i += 1;
            let desc1: extern "system" fn(*mut c_void, *mut AdapterDesc1) -> i32 = std::mem::transmute(slot(ad, 10));
            let mut d: AdapterDesc1 = std::mem::zeroed();
            // Flag 2: software adapter (Microsoft Basic Render).
            let ok = desc1(ad, &mut d) >= 0 && (!nvidia_only || d.vendor_id == 0x10de) && d.flags & 2 == 0;
            let mut m = VideoMemoryInfo::default();
            if ok {
                let qi: extern "system" fn(*mut c_void, *const Guid, *mut *mut c_void) -> i32 =
                    std::mem::transmute(slot(ad, 0));
                let mut a3 = std::ptr::null_mut();
                if qi(ad, &IID_ADAPTER3, &mut a3) >= 0 {
                    let q: extern "system" fn(*mut c_void, u32, i32, *mut VideoMemoryInfo) -> i32 =
                        std::mem::transmute(slot(a3, 14));
                    q(a3, 0, 0, &mut m);
                    release(a3);
                }
            }
            release(ad);
            if !ok {
                continue;
            }
            let total = (d.dedicated_video_memory as u64) >> 20;
            // Budget is per process; capped by the dedicated memory (UMA adapters report system RAM).
            let free = (m.budget.saturating_sub(m.current_usage) >> 20).min(total);
            let name = String::from_utf16_lossy(&d.description)
                .trim_end_matches('\0')
                .to_string();
            let g = Gpu {
                name,
                free_mb: free,
                total_mb: total,
                source: "dxgi",
                index,
            };
            if best.as_ref().is_none_or(|b| g.free_mb > b.free_mb) {
                best = Some(g);
            }
        }
        release(factory);
        best
    }
}
