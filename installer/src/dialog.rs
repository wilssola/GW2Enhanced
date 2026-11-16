//! The "open file" dialog, to point at `Gw2-64.exe` by hand: Windows' own, or zenity / kdialog on Linux.

use std::path::PathBuf;

/// The dialog's title.
fn title() -> String {
    tr!("Selecione o Gw2-64.exe na pasta do Guild Wars 2", "Select Gw2-64.exe in the Guild Wars 2 folder")
}

/// Asks for `Gw2-64.exe`; returns its folder, or `None` when cancelled. `owner` is the parent window (or 0).
/// Runs on its own thread (the dialog needs a single-threaded COM apartment).
#[cfg(windows)]
pub fn pick_game_exe(owner: isize) -> Result<Option<PathBuf>, String> {
    use windows_sys::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize};
    use windows_sys::Win32::UI::Controls::Dialogs::{
        GetOpenFileNameW, OFN_EXPLORER, OFN_FILEMUSTEXIST, OFN_NOCHANGEDIR, OFN_PATHMUSTEXIST, OPENFILENAMEW,
    };
    let picked = std::thread::spawn(move || unsafe {
        CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32);
        let w = |s: &str| s.encode_utf16().collect::<Vec<u16>>();
        // Filter pairs separated by NULs, ended by two.
        let mut filter = w("Guild Wars 2 (Gw2-64.exe)");
        filter.push(0);
        filter.extend(w("Gw2-64.exe"));
        filter.push(0);
        filter.extend(w(crate::i18n::pick("Programas (*.exe)", "Programs (*.exe)")));
        filter.push(0);
        filter.extend(w("*.exe"));
        filter.extend([0, 0]);
        let mut title = w(&title());
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
    .flatten();
    Ok(picked)
}

/// Asks for `Gw2-64.exe` with zenity (GNOME and most desktops) or kdialog (KDE); returns its folder. `Err` when
/// neither is installed.
#[cfg(not(windows))]
pub fn pick_game_exe(_owner: isize) -> Result<Option<PathBuf>, String> {
    use std::process::Command;
    let title = title();
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    let tries: [(&str, Vec<String>); 2] = [
        (
            "zenity",
            vec![
                "--file-selection".into(),
                format!("--title={title}"),
                "--file-filter=Gw2-64.exe | Gw2-64.exe".into(),
                "--file-filter=*.exe | *.exe".into(),
            ],
        ),
        ("kdialog", vec!["--title".into(), title.clone(), "--getopenfilename".into(), home, "Gw2-64.exe *.exe".into()]),
    ];
    for (prog, args) in tries {
        let Some(exe) = crate::launch::which(prog) else { continue };
        let out = Command::new(exe).args(&args).output().map_err(|e| format!("{prog}: {e}"))?;
        // Cancelled: non-zero exit and no output.
        let chosen = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !out.status.success() || chosen.is_empty() {
            return Ok(None);
        }
        return Ok(PathBuf::from(chosen).parent().map(|p| p.to_path_buf()));
    }
    Err(tr!("não há um seletor de arquivos (instale o zenity ou o kdialog)", "there is no file picker (install zenity or kdialog)"))
}
