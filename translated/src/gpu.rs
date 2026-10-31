//! Graphics cards on this PC (DXGI), for choosing where the local translator
//! runs: name, vendor, dedicated memory and how much of it is free now.

use hudhook::windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, DXGI_QUERY_VIDEO_MEMORY_INFO,
    IDXGIAdapter1, IDXGIAdapter3, IDXGIFactory1,
};
use hudhook::windows::core::Interface;

pub const VENDOR_NVIDIA: u32 = 0x10DE;
pub const VENDOR_AMD: u32 = 0x1002;
pub const VENDOR_INTEL: u32 = 0x8086;

/// Free video memory the translator needs on the GPU (model + working space).
pub const MIN_FREE_MB: u64 = 1536;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gpu {
    /// Adapter id (the same card is listed once per output).
    pub luid: (u32, i32),
    pub name: String,
    pub vendor: u32,
    pub dedicated_mb: u64,
    /// Memory the OS lets this process use now, minus what it already uses
    /// (at most the dedicated memory: integrated GPUs report shared RAM).
    pub free_mb: Option<u64>,
}

impl Gpu {
    pub fn vendor_name(&self) -> &'static str {
        match self.vendor {
            VENDOR_NVIDIA => "NVIDIA",
            VENDOR_AMD => "AMD",
            VENDOR_INTEL => "Intel",
            _ => "?",
        }
    }

    /// Enough free memory for the translator (unknown counts as enough when
    /// the card has plenty of dedicated memory).
    pub fn has_room(&self) -> bool {
        self.free_mb.map_or(self.dedicated_mb >= 4096, |f| f >= MIN_FREE_MB)
    }
}

/// Hardware adapters, largest dedicated memory first.
pub fn list() -> Vec<Gpu> {
    let mut out = Vec::new();
    let Ok(factory) = (unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }) else {
        return out;
    };
    let mut i = 0;
    while let Ok(adapter) = unsafe { factory.EnumAdapters1(i) } {
        i += 1;
        if let Some(gpu) = describe(&adapter) {
            // The same card can be listed more than once (per output or per
            // virtual adapter of the driver).
            let same = |g: &Gpu| {
                g.luid == gpu.luid
                    || (g.name == gpu.name && g.vendor == gpu.vendor && g.dedicated_mb == gpu.dedicated_mb)
            };
            if !out.iter().any(same) {
                out.push(gpu);
            }
        }
    }
    out.sort_by_key(|g| std::cmp::Reverse(g.dedicated_mb));
    out
}

fn describe(adapter: &IDXGIAdapter1) -> Option<Gpu> {
    let desc = unsafe { adapter.GetDesc1() }.ok()?;
    if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
        return None;
    }
    let len = desc
        .Description
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(desc.Description.len());
    let name = String::from_utf16_lossy(&desc.Description[..len]).trim().to_owned();
    let dedicated_mb = desc.DedicatedVideoMemory as u64 / (1024 * 1024);
    if dedicated_mb == 0 {
        return None;
    }
    let free_mb = adapter.cast::<IDXGIAdapter3>().ok().and_then(|a3| {
        let mut info = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
        unsafe { a3.QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut info) }.ok()?;
        Some((info.Budget.saturating_sub(info.CurrentUsage) / (1024 * 1024)).min(dedicated_mb))
    });
    let luid = (desc.AdapterLuid.LowPart, desc.AdapterLuid.HighPart);
    Some(Gpu {
        luid,
        name,
        vendor: desc.VendorId,
        dedicated_mb,
        free_mb,
    })
}
