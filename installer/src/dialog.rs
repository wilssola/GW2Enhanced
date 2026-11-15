//! The Windows "open file" dialog, to point at `Gw2-64.exe` by hand.

use std::path::PathBuf;

use windows_sys::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize};
use windows_sys::Win32::UI::Controls::Dialogs::{
    GetOpenFileNameW, OFN_EXPLORER, OFN_FILEMUSTEXIST, OFN_NOCHANGEDIR, OFN_PATHMUSTEXIST, OPENFILENAMEW,
};

/// Asks for `Gw2-64.exe`; returns its folder, or `None` when cancelled. `owner` is the parent window (or 0).
/// Runs on its own thread (the dialog needs a single-threaded COM apartment).
pub fn pick_game_exe(owner: isize) -> Option<PathBuf> {
    std::thread::spawn(move || unsafe {
        CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32);
        let w = |s: &str| s.encode_utf16().collect::<Vec<u16>>();
        // Filter pairs separated by NULs, ended by two.
        let mut filter = w("Guild Wars 2 (Gw2-64.exe)");
        filter.push(0);
        filter.extend(w("Gw2-64.exe"));
        filter.push(0);
        filter.extend(w("Programas (*.exe)"));
        filter.push(0);
        filter.extend(w("*.exe"));
        filter.extend([0, 0]);
        let mut title = w("Selecione o Gw2-64.exe na pasta do Guild Wars 2");
        title.push(0);
        let mut file = vec![0u16; 1024];
        let mut ofn: OPENFILENAMEW = std::mem::zeroed();
        ofn.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
        ofn.hwndOwner = owner as _;
        ofn.lpstrFilter = filter.as_ptr();
        ofn.lpstrFile = file.as_mut_ptr();
        ofn.nMaxFile = file.len() as u32;
        ofn.lpstrTitle = title.as_ptr();
        ofn.Flags = OFN_EXPLORER | OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR;
        let ok = GetOpenFileNameW(&mut ofn) != 0;
        CoUninitialize();
        if !ok {
            return None;
        }
        let n = file.iter().position(|&c| c == 0).unwrap_or(0);
        let exe = PathBuf::from(String::from_utf16_lossy(&file[..n]));
        exe.parent().map(|p| p.to_path_buf())
    })
    .join()
    .ok()
    .flatten()
}
