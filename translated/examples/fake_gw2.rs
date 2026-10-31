//! Simulates the game to test the DLL without launching GW2.
//!
//! Copy `fake_gw2.exe` as `Gw2-64.exe` into a folder together with
//! `version.dll` and run it: it loads the DLL from its own folder, like the game
//! does, and waits for initialization to finish.

use windows_sys::Win32::System::LibraryLoader::LoadLibraryW;

fn main() {
    let dll = std::env::current_exe().unwrap().with_file_name("version.dll");
    let wide: Vec<u16> = dll.as_os_str().to_string_lossy().encode_utf16().chain([0]).collect();
    let h = unsafe { LoadLibraryW(wide.as_ptr()) };
    assert!(!h.is_null(), "failed to load {}", dll.display());
    let secs = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(3);
    std::thread::sleep(std::time::Duration::from_secs(secs));
}
