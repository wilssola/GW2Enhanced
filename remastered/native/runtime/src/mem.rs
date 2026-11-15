//! Memory figures for the overlay: RAM and VRAM of the game and of the mod.
//!
//! * Game RAM: working set and private bytes of the process (Windows).
//! * Game VRAM: what the process uses on the GPU, from DXGI (`QueryVideoMemoryInfo`), plus the
//!   budget the OS gives it. The system `dxgi.dll` is loaded by full path: the one in the game
//!   folder is ReShade's wrapper.
//! * Mod RAM: bytes the mod holds on its Rust heap (counted by the allocator), mostly the RAM cache.
//! * Mod VRAM: an estimate. The textures live in the engine, so the mod cannot read their size; it
//!   adds up what the HD files add over the originals for the textures still resident, and the larger
//!   composite atlases of the characters on screen.

use std::alloc::{GlobalAlloc, Layout, System};
use std::ffi::c_void;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::time::Instant;

use hudhook::windows::Win32::Graphics::Dxgi::{
    DXGI_MEMORY_SEGMENT_GROUP_LOCAL, DXGI_QUERY_VIDEO_MEMORY_INFO, IDXGIAdapter3, IDXGIFactory1,
};
use hudhook::windows::core::Interface;
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX};
use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows_sys::Win32::System::Threading::GetCurrentProcess;

/// Counts the bytes the mod has allocated and not yet freed.
pub struct CountingAlloc;

static LIVE: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            LIVE.fetch_add(l.size(), Relaxed);
        }
        p
    }

    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(l) };
        if !p.is_null() {
            LIVE.fetch_add(l.size(), Relaxed);
        }
        p
    }

    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) };
        LIVE.fetch_sub(l.size(), Relaxed);
    }

    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, l, new) };
        if !q.is_null() {
            if new >= l.size() {
                LIVE.fetch_add(new - l.size(), Relaxed);
            } else {
                LIVE.fetch_sub(l.size() - new, Relaxed);
            }
        }
        q
    }
}

/// Bytes the mod holds on its heap right now.
pub fn mod_heap() -> u64 {
    LIVE.load(Relaxed) as u64
}

static RAM_WORKING: AtomicU64 = AtomicU64::new(0);
static RAM_PRIVATE: AtomicU64 = AtomicU64::new(0);
static VRAM_USED: AtomicU64 = AtomicU64::new(0);
static VRAM_BUDGET: AtomicU64 = AtomicU64::new(0);
static MOD_VRAM: AtomicU64 = AtomicU64::new(0);

pub struct Os {
    pub ram_working: u64,
    pub ram_private: u64,
    pub vram_used: u64,
    pub vram_budget: u64,
}

pub fn os() -> Os {
    Os {
        ram_working: RAM_WORKING.load(Relaxed),
        ram_private: RAM_PRIVATE.load(Relaxed),
        vram_used: VRAM_USED.load(Relaxed),
        vram_budget: VRAM_BUDGET.load(Relaxed),
    }
}

pub fn mod_vram() -> u64 {
    MOD_VRAM.load(Relaxed)
}

pub fn set_mod_vram(bytes: u64) {
    MOD_VRAM.store(bytes, Relaxed);
}

struct Adapter(IDXGIAdapter3);
// Only `QueryVideoMemoryInfo` is called on it, which DXGI allows from any thread.
unsafe impl Send for Adapter {}
unsafe impl Sync for Adapter {}

static ADAPTER: OnceLock<Option<Adapter>> = OnceLock::new();

/// The GPU with the most dedicated memory, through the system DXGI.
fn find_adapter() -> Option<Adapter> {
    unsafe {
        let mut dir = [0u16; 260];
        let n = GetSystemDirectoryW(dir.as_mut_ptr(), 260) as usize;
        let mut path: Vec<u16> = dir[..n].to_vec();
        path.extend("\\dxgi.dll".encode_utf16());
        path.push(0);
        let module = LoadLibraryW(path.as_ptr());
        if module.is_null() {
            return None;
        }
        let create = GetProcAddress(module, c"CreateDXGIFactory1".as_ptr().cast())?;
        type Create = unsafe extern "system" fn(*const hudhook::windows::core::GUID, *mut *mut c_void) -> i32;
        let create: Create = std::mem::transmute(create);
        let mut raw: *mut c_void = std::ptr::null_mut();
        if create(&IDXGIFactory1::IID, &mut raw) < 0 || raw.is_null() {
            return None;
        }
        let factory = IDXGIFactory1::from_raw(raw);
        let (mut best, mut best_mem) = (None, 0usize);
        let mut i = 0;
        while let Ok(a) = factory.EnumAdapters1(i) {
            i += 1;
            if let Ok(d) = a.GetDesc1() {
                if d.DedicatedVideoMemory > best_mem {
                    best_mem = d.DedicatedVideoMemory;
                    best = Some(a);
                }
            }
        }
        best?.cast::<IDXGIAdapter3>().ok().map(Adapter)
    }
}

/// Reads the process's RAM and VRAM; at most once per second, cheap.
pub fn refresh_os() {
    static LAST: std::sync::Mutex<Option<Instant>> = std::sync::Mutex::new(None);
    {
        let mut g = LAST.lock().unwrap_or_else(|e| e.into_inner());
        if g.is_some_and(|t| t.elapsed().as_millis() < 1000) {
            return;
        }
        *g = Some(Instant::now());
    }
    unsafe {
        let mut c: PROCESS_MEMORY_COUNTERS_EX = std::mem::zeroed();
        c.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
        if K32GetProcessMemoryInfo(GetCurrentProcess(), (&mut c as *mut PROCESS_MEMORY_COUNTERS_EX).cast::<PROCESS_MEMORY_COUNTERS>(), c.cb) != 0 {
            RAM_WORKING.store(c.WorkingSetSize as u64, Relaxed);
            RAM_PRIVATE.store(c.PrivateUsage as u64, Relaxed);
        }
        if let Some(a) = ADAPTER.get_or_init(find_adapter) {
            let mut info = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
            if a.0.QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut info).is_ok() {
                VRAM_USED.store(info.CurrentUsage, Relaxed);
                VRAM_BUDGET.store(info.Budget, Relaxed);
            }
        }
    }
}

/// `1.4 GB` / `512 MB`.
pub fn fmt(bytes: u64) -> String {
    if bytes >= 1 << 30 {
        format!("{:.2} GB", bytes as f64 / (1u64 << 30) as f64)
    } else {
        format!("{} MB", bytes >> 20)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn formats_sizes() {
        assert_eq!(super::fmt(512 << 20), "512 MB");
        assert_eq!(super::fmt(3 << 29), "1.50 GB");
        assert_eq!(super::fmt(0), "0 MB");
    }

    /// Real hardware: needs a GPU, as on any machine that runs the game.
    #[test]
    fn reads_real_ram_and_vram() {
        super::refresh_os();
        let o = super::os();
        println!("RAM working {} private {} | VRAM used {} budget {}", super::fmt(o.ram_working), super::fmt(o.ram_private), super::fmt(o.vram_used), super::fmt(o.vram_budget));
        assert!(o.ram_working > 0 && o.ram_private > 0);
        assert!(o.vram_budget > 0, "DXGI adapter not found or query failed");
    }

    #[test]
    #[cfg(feature = "standalone")] // the counting allocator is the global one only in the standalone DLL
    fn heap_counter_moves() {
        // Not the global allocator in tests of this crate? It is: the counter must follow a big allocation.
        let before = super::mod_heap();
        let v = vec![1u8; 8 << 20];
        let during = super::mod_heap();
        drop(v);
        assert!(during >= before + (8 << 20) - (1 << 20));
    }
}
