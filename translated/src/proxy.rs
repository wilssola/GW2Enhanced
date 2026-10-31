//! Forwarding of the 17 `version.dll` exports to the real copy in System32.
//!
//! The real DLL is only loaded on the first call; none of this runs in `DllMain`.

use std::ffi::c_void;
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::HMODULE;
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;

struct RealDll(HMODULE);
// HMODULE is just a base address that is immutable once loaded.
unsafe impl Send for RealDll {}
unsafe impl Sync for RealDll {}

static REAL: OnceLock<RealDll> = OnceLock::new();

fn real_module() -> HMODULE {
    REAL.get_or_init(|| unsafe {
        let mut buf = [0u16; 260];
        let n = GetSystemDirectoryW(buf.as_mut_ptr(), buf.len() as u32) as usize;
        let mut path: Vec<u16> = buf[..n].to_vec();
        path.extend("\\version.dll".encode_utf16());
        path.push(0);
        RealDll(LoadLibraryW(path.as_ptr()))
    })
    .0
}

fn real_proc(name: &'static [u8]) -> *const c_void {
    let m = real_module();
    if m.is_null() {
        return std::ptr::null();
    }
    unsafe {
        GetProcAddress(m, name.as_ptr())
            .map(|f| f as *const c_void)
            .unwrap_or(std::ptr::null())
    }
}

/// Generates an export that forwards the call to the real function with the same signature.
/// If the real DLL cannot be loaded, returns `$fail` (FALSE/0), as the API would.
macro_rules! forward {
    ($name:ident ( $($arg:ident : $ty:ty),* ) -> $ret:ty, $fail:expr) => {
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn $name($($arg: $ty),*) -> $ret {
            static PTR: OnceLock<usize> = OnceLock::new();
            let p = *PTR.get_or_init(|| real_proc(concat!(stringify!($name), "\0").as_bytes()) as usize);
            if p == 0 {
                return $fail;
            }
            let f: unsafe extern "system" fn($($ty),*) -> $ret = unsafe { std::mem::transmute(p) };
            unsafe { f($($arg),*) }
        }
    };
}

type P = *const c_void;
type M = *mut c_void;

forward!(GetFileVersionInfoA(a: P, b: u32, c: u32, d: M) -> i32, 0);
forward!(GetFileVersionInfoW(a: P, b: u32, c: u32, d: M) -> i32, 0);
forward!(GetFileVersionInfoExA(f: u32, a: P, b: u32, c: u32, d: M) -> i32, 0);
forward!(GetFileVersionInfoExW(f: u32, a: P, b: u32, c: u32, d: M) -> i32, 0);
forward!(GetFileVersionInfoByHandle(f: u32, h: P, a: M, b: u32) -> i32, 0);
forward!(GetFileVersionInfoSizeA(a: P, b: M) -> u32, 0);
forward!(GetFileVersionInfoSizeW(a: P, b: M) -> u32, 0);
forward!(GetFileVersionInfoSizeExA(f: u32, a: P, b: M) -> u32, 0);
forward!(GetFileVersionInfoSizeExW(f: u32, a: P, b: M) -> u32, 0);
forward!(VerFindFileA(f: u32, a: P, b: P, c: P, d: M, e: M, g: M, h: M) -> u32, 0);
forward!(VerFindFileW(f: u32, a: P, b: P, c: P, d: M, e: M, g: M, h: M) -> u32, 0);
forward!(
    VerInstallFileA(f: u32, a: P, b: P, c: P, d: P, e: P, g: M, h: M) -> u32,
    0
);
forward!(
    VerInstallFileW(f: u32, a: P, b: P, c: P, d: P, e: P, g: M, h: M) -> u32,
    0
);
forward!(VerLanguageNameA(l: u32, a: M, n: u32) -> u32, 0);
forward!(VerLanguageNameW(l: u32, a: M, n: u32) -> u32, 0);
forward!(VerQueryValueA(a: P, b: P, c: M, d: M) -> i32, 0);
forward!(VerQueryValueW(a: P, b: P, c: M, d: M) -> i32, 0);
