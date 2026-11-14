//! Forwarding of the 5 `msimg32.dll` exports to the real copy in System32.
//!
//! `msimg32.dll` was chosen because `Gw2-64.exe` imports it, it is not a
//! KnownDLL (so Windows loads ours from the game folder first) and no other
//! common GW2 mod uses it: `version.dll` is GW2 Translated, `d3d11.dll` /
//! `dxgi.dll` are ReShade, arcdps and Nexus.

use std::ffi::c_void;
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::HMODULE;
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;

struct RealDll(HMODULE);
unsafe impl Send for RealDll {}
unsafe impl Sync for RealDll {}

static REAL: OnceLock<RealDll> = OnceLock::new();

fn real_module() -> HMODULE {
    REAL.get_or_init(|| unsafe {
        let mut buf = [0u16; 260];
        let n = GetSystemDirectoryW(buf.as_mut_ptr(), buf.len() as u32) as usize;
        let mut path: Vec<u16> = buf[..n].to_vec();
        path.extend("\\msimg32.dll".encode_utf16());
        path.push(0);
        RealDll(LoadLibraryW(path.as_ptr()))
    })
    .0
}

fn real_proc(name: &'static [u8]) -> usize {
    let m = real_module();
    if m.is_null() {
        return 0;
    }
    unsafe { GetProcAddress(m, name.as_ptr()).map(|f| f as usize).unwrap_or(0) }
}

macro_rules! forward {
    ($name:ident ( $($arg:ident : $ty:ty),* ) -> $ret:ty, $fail:expr) => {
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn $name($($arg: $ty),*) -> $ret {
            static PTR: OnceLock<usize> = OnceLock::new();
            let p = *PTR.get_or_init(|| real_proc(concat!(stringify!($name), "\0").as_bytes()));
            if p == 0 {
                return $fail;
            }
            let f: unsafe extern "system" fn($($ty),*) -> $ret = unsafe { std::mem::transmute(p) };
            unsafe { f($($arg),*) }
        }
    };
}

type P = *mut c_void;

forward!(AlphaBlend(a: P, b: i32, c: i32, d: i32, e: i32, f: P, g: i32, h: i32, i: i32, j: i32, k: u32) -> i32, 0);
forward!(TransparentBlt(a: P, b: i32, c: i32, d: i32, e: i32, f: P, g: i32, h: i32, i: i32, j: i32, k: u32) -> i32, 0);
forward!(GradientFill(a: P, b: P, c: u32, d: P, e: u32, f: u32) -> i32, 0);
forward!(DllInitialize(a: P, b: u32, c: P) -> i32, 1);
forward!(vSetDdrawflag() -> (), ());
