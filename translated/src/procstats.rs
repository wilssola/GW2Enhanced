//! Resources used by the local translator process, for the overlay: RAM
//! (working set), CPU, video memory and GPU use. Video memory and GPU use come
//! from Windows' performance counters (`GPU Process Memory`, `GPU Engine`),
//! which work per process even where NVIDIA's tools show "N/A" (WDDM).

use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HANDLE};
use windows_sys::Win32::System::Performance::{
    PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_HQUERY, PDH_MORE_DATA, PdhAddEnglishCounterW,
    PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterArrayW, PdhOpenQueryW,
};
use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows_sys::Win32::System::Threading::{GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

const INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Usage {
    pub ram_mb: u64,
    /// Share of the whole processor (all cores), in %.
    pub cpu_pct: f32,
    /// `None` when the process uses no GPU (or the counters are unavailable).
    pub vram_mb: Option<u64>,
    pub gpu_pct: Option<f32>,
}

static USAGE: Mutex<Option<Usage>> = Mutex::new(None);
/// Process being watched; a new translator replaces it.
static WATCHED: AtomicU32 = AtomicU32::new(0);

/// Latest measurement of the translator, if it is running.
pub fn usage() -> Option<Usage> {
    USAGE.lock().ok().and_then(|u| *u)
}

/// Starts measuring process `pid` (stops measuring the previous one).
pub fn watch(pid: u32) {
    WATCHED.store(pid, Ordering::Relaxed);
    let _ = std::thread::Builder::new()
        .name("gw2translated-stats".into())
        .spawn(move || run(pid));
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

fn filetime(t: FILETIME) -> u64 {
    (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime)
}

/// Kernel + user CPU time of the process, in 100 ns units.
fn cpu_time(process: HANDLE) -> Option<u64> {
    let z = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut c, mut e, mut k, mut u) = (z, z, z, z);
    (unsafe { GetProcessTimes(process, &mut c, &mut e, &mut k, &mut u) } != 0).then(|| filetime(k) + filetime(u))
}

fn ram_mb(process: HANDLE) -> Option<u64> {
    let mut m: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    (unsafe { K32GetProcessMemoryInfo(process, &mut m, size) } != 0).then_some(m.WorkingSetSize as u64 / (1024 * 1024))
}

/// Performance counters of one process: video memory and GPU engines.
struct GpuCounters {
    query: PDH_HQUERY,
    memory: PDH_HCOUNTER,
    engines: PDH_HCOUNTER,
}

impl GpuCounters {
    fn open(pid: u32) -> Option<Self> {
        let mut query: PDH_HQUERY = std::ptr::null_mut();
        if unsafe { PdhOpenQueryW(std::ptr::null(), 0, &mut query) } != 0 {
            return None;
        }
        let add = |path: &str| {
            let mut c: PDH_HCOUNTER = std::ptr::null_mut();
            (unsafe { PdhAddEnglishCounterW(query, wide(path).as_ptr(), 0, &mut c) } == 0).then_some(c)
        };
        let memory = add(&format!("\\GPU Process Memory(pid_{pid}_*)\\Dedicated Usage"));
        let engines = add(&format!("\\GPU Engine(pid_{pid}_*)\\Utilization Percentage"));
        match (memory, engines) {
            (Some(memory), Some(engines)) => Some(Self { query, memory, engines }),
            _ => {
                unsafe { PdhCloseQuery(query) };
                None
            }
        }
    }

    /// Values of every instance of a counter.
    fn values(counter: PDH_HCOUNTER) -> Vec<f64> {
        let (mut size, mut count) = (0u32, 0u32);
        let status = unsafe {
            PdhGetFormattedCounterArrayW(counter, PDH_FMT_DOUBLE, &mut size, &mut count, std::ptr::null_mut())
        };
        if status != PDH_MORE_DATA || size == 0 {
            return Vec::new();
        }
        let items = (size as usize).div_ceil(std::mem::size_of::<PDH_FMT_COUNTERVALUE_ITEM_W>());
        let mut buf: Vec<PDH_FMT_COUNTERVALUE_ITEM_W> = (0..items).map(|_| unsafe { std::mem::zeroed() }).collect();
        if unsafe { PdhGetFormattedCounterArrayW(counter, PDH_FMT_DOUBLE, &mut size, &mut count, buf.as_mut_ptr()) }
            != 0
        {
            return Vec::new();
        }
        buf[..count as usize]
            .iter()
            .filter(|i| i.FmtValue.CStatus == 0)
            .map(|i| unsafe { i.FmtValue.Anonymous.doubleValue })
            .collect()
    }

    /// (video memory in MB, busiest GPU engine in %).
    fn read(&self) -> Option<(u64, f32)> {
        if unsafe { PdhCollectQueryData(self.query) } != 0 {
            return None;
        }
        let memory = Self::values(self.memory);
        if memory.is_empty() {
            return None;
        }
        let vram = memory.iter().sum::<f64>() / (1024.0 * 1024.0);
        let gpu = Self::values(self.engines).into_iter().fold(0.0f64, f64::max);
        Some((vram as u64, gpu as f32))
    }
}

impl Drop for GpuCounters {
    fn drop(&mut self) {
        unsafe { PdhCloseQuery(self.query) };
    }
}

fn run(pid: u32) {
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return;
    }
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get()) as f64;
    let gpu = GpuCounters::open(pid);
    let mut last = (Instant::now(), cpu_time(process).unwrap_or(0));
    while WATCHED.load(Ordering::Relaxed) == pid {
        std::thread::sleep(INTERVAL);
        let (Some(ram), Some(cpu)) = (ram_mb(process), cpu_time(process)) else {
            break;
        };
        let elapsed = last.0.elapsed().as_secs_f64();
        let cpu_pct = ((cpu.saturating_sub(last.1)) as f64 / 1e7 / elapsed / cores * 100.0) as f32;
        last = (Instant::now(), cpu);
        let (vram_mb, gpu_pct) = match gpu.as_ref().and_then(GpuCounters::read) {
            Some((v, g)) if v > 0 => (Some(v), Some(g)),
            _ => (None, None),
        };
        if WATCHED.load(Ordering::Relaxed) == pid
            && let Ok(mut u) = USAGE.lock()
        {
            *u = Some(Usage {
                ram_mb: ram,
                cpu_pct,
                vram_mb,
                gpu_pct,
            });
        }
    }
    if WATCHED.load(Ordering::Relaxed) == pid
        && let Ok(mut u) = USAGE.lock()
    {
        *u = None;
    }
    unsafe { CloseHandle(process) };
}
