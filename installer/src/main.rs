//! GW2 Enhanced installer.
//!
//! Ships at the root of every release zip, next to `msimg32.dll` and the data folders of the mods in that package
//! (`GW2Translated\`, `GW2Graphics\`, ...):
//!
//! ```text
//! GW2Enhanced-Installer.exe                installs (finds the game or lets the player pick it)
//! GW2Enhanced-Installer.exe "<folder>"     installs into the given folder
//! GW2Enhanced-Installer.exe --uninstall    uninstalls and restores the previous msimg32.dll
//! ```
//!
//! Never deletes the player's collected strings, live translations, logs or `config.ini`: the package never has
//! them, and installing only adds or overwrites the files the package has.
//!
//! Upgrades from the old separate mods: GW2 Translated's `version.dll` is removed (the translation now lives in
//! `msimg32.dll`; both at once would hook the game twice) and the `version.dll` it had backed up is put back; GW2
//! Remastered's `msimg32.dll` is replaced without a backup, as it is ours.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use windows_sys::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoTaskMemFree};
use windows_sys::Win32::System::Registry::{HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};
use windows_sys::Win32::UI::Shell::{
    BIF_NEWDIALOGSTYLE, BIF_RETURNONLYFSDIRS, BROWSEINFOW, SHBrowseForFolderW, SHGetPathFromIDListW,
};

/// The mod's proxy DLL, next to `Gw2-64.exe`.
const DLL: &str = "msimg32.dll";
/// Markers of a `msimg32.dll` that is ours: GW2 Enhanced, and the old GW2 Remastered (same file name).
const DLL_MARKERS: &[&[u8]] = &[b"GW2ENHANCED-DLL-MARKER!!", b"GW2REMASTERED-DLL-MARKER"];
/// Copy of another mod's `msimg32.dll` found during installation.
const BACKUP: &str = "msimg32.dll.antes-do-gw2enhanced";
/// The old GW2 Translated proxy, its marker and the backup it made of another mod's `version.dll`.
const LEGACY_DLL: &str = "version.dll";
const LEGACY_MARKER: &[u8] = b"GW2TRANSLATED-DLL-MARKER";
const LEGACY_BACKUP: &str = "version.dll.antes-do-gw2translated";
/// Data folders a package may carry next to `msimg32.dll` (one per mod), copied as they are.
const DATA_DIRS: &[&str] = &["GW2Translated", "GW2Remastered", "GW2Graphics"];
/// Translation files from older versions, removed on install (per language).
const OBSOLETE: &[&str] = &["strings.jsonl", "ids.jsonl"];
const STEAM_APP_DIR: &str = r"steamapps\common\Guild Wars 2";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

fn reg_string(root: HKEY, key: &str, value: &str) -> Option<String> {
    let (k, v) = (wide(key), wide(value));
    let mut buf = vec![0u16; 1024];
    let mut len = (buf.len() * 2) as u32;
    let rc = unsafe {
        RegGetValueW(
            root,
            k.as_ptr(),
            v.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buf.as_mut_ptr().cast(),
            &mut len,
        )
    };
    (rc == 0).then(|| String::from_utf16_lossy(&buf[..(len as usize / 2).saturating_sub(1)]))
}

/// Steam library `path` folders (from `libraryfolders.vdf`).
fn steam_libraries(steam: &Path) -> Vec<PathBuf> {
    let mut out = vec![steam.to_path_buf()];
    if let Ok(vdf) = std::fs::read_to_string(steam.join(r"steamapps\libraryfolders.vdf")) {
        for line in vdf.lines() {
            let mut parts = line.split('"').filter(|p| !p.trim().is_empty());
            if parts.next() == Some("path")
                && let Some(p) = parts.next()
            {
                out.push(PathBuf::from(p.replace(r"\\", r"\")));
            }
        }
    }
    out
}

fn is_game_dir(p: &Path) -> bool {
    p.join("Gw2-64.exe").is_file()
}

fn find_game() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(steam) = reg_string(HKEY_CURRENT_USER, r"Software\Valve\Steam", "SteamPath") {
        for lib in steam_libraries(Path::new(&steam)) {
            candidates.push(lib.join(STEAM_APP_DIR));
        }
    }
    for (root, key) in [
        (HKEY_LOCAL_MACHINE, r"SOFTWARE\ArenaNet\Guild Wars 2"),
        (HKEY_LOCAL_MACHINE, r"SOFTWARE\WOW6432Node\ArenaNet\Guild Wars 2"),
        (HKEY_CURRENT_USER, r"Software\ArenaNet\Guild Wars 2"),
    ] {
        if let Some(p) = reg_string(root, key, "Path") {
            let p = PathBuf::from(p);
            candidates.push(if p.extension().is_some() {
                p.parent().map(Path::to_path_buf).unwrap_or(p)
            } else {
                p
            });
        }
    }
    for p in [r"C:\Program Files\Guild Wars 2", r"C:\Program Files (x86)\Guild Wars 2"] {
        candidates.push(PathBuf::from(p));
    }
    let mut found: Vec<PathBuf> = Vec::new();
    for c in candidates {
        if is_game_dir(&c) && !found.iter().any(|f| f.eq(&c)) {
            found.push(c);
        }
    }
    found
}

/// Windows folder picker; `None` if the user cancels.
fn pick_folder() -> Option<PathBuf> {
    let title = wide("Escolha a pasta do Guild Wars 2 (onde fica o Gw2-64.exe)");
    let mut name = [0u16; 260];
    let info = BROWSEINFOW {
        pszDisplayName: name.as_mut_ptr(),
        lpszTitle: title.as_ptr(),
        ulFlags: BIF_RETURNONLYFSDIRS | BIF_NEWDIALOGSTYLE,
        ..Default::default()
    };
    unsafe {
        CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32);
        let pidl = SHBrowseForFolderW(&info);
        if pidl.is_null() {
            return None;
        }
        let mut path = [0u16; 1024];
        let ok = SHGetPathFromIDListW(pidl, path.as_mut_ptr());
        CoTaskMemFree(pidl.cast());
        let n = path.iter().position(|&c| c == 0).unwrap_or(0);
        (ok != 0 && n > 0).then(|| PathBuf::from(String::from_utf16_lossy(&path[..n])))
    }
}

fn game_running() -> bool {
    Command::new("tasklist")
        .args(["/FI", "IMAGENAME eq Gw2-64.exe", "/NH"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("Gw2-64.exe"))
        .unwrap_or(false)
}

fn has_marker(file: &Path, markers: &[&[u8]]) -> bool {
    std::fs::read(file).is_ok_and(|b| markers.iter().any(|m| b.windows(m.len()).any(|w| w == *m)))
}

fn is_ours(dll: &Path) -> bool {
    has_marker(dll, DLL_MARKERS)
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<u64> {
    std::fs::create_dir_all(to)?;
    let mut n = 0;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let dst = to.join(e.file_name());
        if e.file_type()?.is_dir() {
            n += copy_dir(&e.path(), &dst)?;
        } else {
            std::fs::copy(e.path(), dst)?;
            n += 1;
        }
    }
    Ok(n)
}

/// Removes the old GW2 Translated `version.dll` (and puts back the one it had backed up).
fn remove_legacy(game: &Path) -> Result<(), String> {
    let dll = game.join(LEGACY_DLL);
    if dll.is_file() && has_marker(&dll, &[LEGACY_MARKER]) {
        std::fs::remove_file(&dll)
            .map_err(|e| format!("não consegui remover o version.dll antigo do GW2 Translated: {e}"))?;
        println!("  version.dll antigo do GW2 Translated removido (a tradução agora fica no {DLL})");
        let backup = game.join(LEGACY_BACKUP);
        if backup.is_file() {
            std::fs::rename(&backup, &dll)
                .map_err(|e| format!("não consegui restaurar o version.dll anterior: {e}"))?;
            println!("  version.dll de outro mod restaurado");
        }
    }
    Ok(())
}

fn install(pkg: &Path, game: &Path) -> Result<(), String> {
    let new_dll = pkg.join(DLL);
    if !new_dll.is_file() {
        return Err(format!("pacote incompleto: falta {DLL} em {}", pkg.display()));
    }
    remove_legacy(game)?;
    let dll = game.join(DLL);
    if dll.is_file() && !is_ours(&dll) {
        let backup = game.join(BACKUP);
        if !backup.exists() {
            std::fs::copy(&dll, &backup).map_err(|e| format!("não consegui guardar o {DLL} atual: {e}"))?;
            println!("  {DLL} de outro mod guardado como {BACKUP}");
        }
    }
    std::fs::copy(&new_dll, &dll).map_err(|e| format!("não consegui copiar o {DLL}: {e}"))?;
    println!("  {DLL} instalado");

    for dir in DATA_DIRS {
        let from = pkg.join(dir);
        if !from.is_dir() {
            continue;
        }
        let to = game.join(dir);
        let n = copy_dir(&from, &to).map_err(|e| format!("{dir}: {e}"))?;
        println!("  {dir}\\ atualizado ({n} arquivos)");
        if let Ok(langs) = std::fs::read_dir(to.join("languages")) {
            for lang in langs.flatten() {
                for f in OBSOLETE {
                    let _ = std::fs::remove_file(lang.path().join(f));
                }
            }
        }
    }
    Ok(())
}

fn uninstall(game: &Path) -> Result<(), String> {
    remove_legacy(game)?;
    let dll = game.join(DLL);
    if dll.is_file() && is_ours(&dll) {
        std::fs::remove_file(&dll).map_err(|e| format!("não consegui remover o {DLL}: {e}"))?;
        println!("  {DLL} do GW2 Enhanced removido");
    }
    let backup = game.join(BACKUP);
    if backup.is_file() && !dll.exists() {
        std::fs::rename(&backup, &dll).map_err(|e| format!("não consegui restaurar o {DLL} anterior: {e}"))?;
        println!("  {DLL} anterior restaurado");
    }
    let dirs: Vec<String> = DATA_DIRS.iter().map(|d| format!("{d}\\")).collect();
    println!(
        "  as pastas {} (configuração, coleta, logs) foram mantidas; apague à mão se quiser",
        dirs.join(", ")
    );
    Ok(())
}

fn ask(prompt: &str) -> String {
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    let mut s = String::new();
    let _ = std::io::stdin().lock().read_line(&mut s);
    s.trim().trim_matches('"').to_owned()
}

/// Game folder: the detected one (confirmed with Enter) or one picked by hand.
fn choose_game() -> Result<PathBuf, String> {
    let found = find_game();
    let mut options: Vec<PathBuf> = found.clone();
    if !found.is_empty() {
        println!("Guild Wars 2 encontrado:");
        for (i, p) in found.iter().enumerate() {
            println!("  {}) {}", i + 1, p.display());
        }
        let hint = if found.len() == 1 {
            "Enter = instalar aqui"
        } else {
            "número = escolher"
        };
        let a = ask(&format!("\n{hint}, P = escolher outra pasta: "));
        if !a.eq_ignore_ascii_case("p") {
            let i: usize = a.parse().unwrap_or(1);
            return found
                .get(i.saturating_sub(1))
                .cloned()
                .ok_or_else(|| "opção inválida".into());
        }
        options.clear();
    } else {
        println!("Não encontrei o Guild Wars 2 automaticamente. Escolha a pasta do jogo.");
    }
    loop {
        let Some(p) = pick_folder() else {
            return Err("nenhuma pasta escolhida".into());
        };
        if is_game_dir(&p) {
            return Ok(p);
        }
        println!("{} não tem o Gw2-64.exe. Tente de novo.", p.display());
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let remove = args.iter().any(|a| a == "--uninstall");
    let pkg = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_default();

    let game = match args.iter().find(|a| !a.starts_with("--")) {
        Some(p) => PathBuf::from(p),
        None => choose_game()?,
    };
    if !is_game_dir(&game) {
        return Err(format!("{} não tem o Gw2-64.exe", game.display()));
    }
    println!("\nPasta do jogo: {}", game.display());
    if game_running() {
        return Err("feche o Guild Wars 2 antes de continuar".into());
    }
    if remove { uninstall(&game) } else { install(&pkg, &game) }
}

fn main() {
    println!("GW2 Enhanced — tradução, texturas HD e gráficos para o Guild Wars 2\n");
    let code = match run() {
        Ok(()) => {
            println!("\nPronto! Abra o jogo normalmente.");
            0
        }
        Err(e) => {
            println!("\nERRO: {e}");
            1
        }
    };
    // When launched by double-click, the window would close before the message could be read.
    ask("\nPressione Enter para sair.");
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_steam_library_paths() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("steamapps")).unwrap();
        std::fs::write(
            dir.path().join(r"steamapps\libraryfolders.vdf"),
            "\"libraryfolders\"\n{\n\t\"0\"\n\t{\n\t\t\"path\"\t\t\"D:\\\\SteamLibrary\"\n\t}\n}\n",
        )
        .unwrap();
        let libs = steam_libraries(dir.path());
        assert_eq!(libs[1], PathBuf::from(r"D:\SteamLibrary"));
    }

    #[test]
    fn install_keeps_player_files_and_backs_up_foreign_dll() {
        let (pkg, game) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        std::fs::write(pkg.path().join(DLL), b"xx GW2ENHANCED-DLL-MARKER!! xx").unwrap();
        let lang = pkg.path().join(r"GW2Translated\languages\pt-BR");
        std::fs::create_dir_all(&lang).unwrap();
        std::fs::create_dir_all(pkg.path().join(r"GW2Translated\translator\models\pt-BR")).unwrap();
        std::fs::create_dir_all(pkg.path().join(r"GW2Graphics\luts")).unwrap();
        std::fs::write(lang.join("dictionary.jsonl"), "{}\n").unwrap();
        std::fs::write(lang.join("web.jsonl"), "").unwrap();
        std::fs::write(pkg.path().join(r"GW2Translated\translator\t.exe"), b"t").unwrap();
        std::fs::write(pkg.path().join(r"GW2Translated\translator\models\pt-BR\m.bin"), b"m").unwrap();
        std::fs::write(pkg.path().join(r"GW2Graphics\luts\warm.cube"), b"lut").unwrap();
        std::fs::write(game.path().join("Gw2-64.exe"), b"").unwrap();
        std::fs::write(game.path().join(DLL), b"outro mod").unwrap();
        let data = game.path().join("GW2Translated");
        std::fs::create_dir_all(data.join(r"languages\pt-BR")).unwrap();
        std::fs::write(data.join("config.ini"), "translate=false").unwrap();
        std::fs::write(
            data.join(r"languages\pt-BR\local.jsonl"),
            "{\"en\":\"Hi\",\"tr\":\"Oi\"}\n",
        )
        .unwrap();
        std::fs::write(data.join(r"languages\pt-BR\ids.jsonl"), "").unwrap();

        install(pkg.path(), game.path()).unwrap();
        assert!(is_ours(&game.path().join(DLL)));
        assert_eq!(std::fs::read(game.path().join(BACKUP)).unwrap(), b"outro mod");
        assert_eq!(
            std::fs::read_to_string(data.join("config.ini")).unwrap(),
            "translate=false"
        );
        assert!(data.join(r"languages\pt-BR\local.jsonl").is_file());
        assert!(data.join(r"languages\pt-BR\dictionary.jsonl").is_file());
        assert!(!data.join(r"languages\pt-BR\ids.jsonl").exists());
        assert!(data.join(r"translator\t.exe").is_file());
        assert!(data.join(r"translator\models\pt-BR\m.bin").is_file());
        assert!(game.path().join(r"GW2Graphics\luts\warm.cube").is_file());
        assert!(!game.path().join("GW2Remastered").exists());

        // Reinstalling does not overwrite the backup with our own DLL.
        install(pkg.path(), game.path()).unwrap();
        assert_eq!(std::fs::read(game.path().join(BACKUP)).unwrap(), b"outro mod");

        uninstall(game.path()).unwrap();
        assert_eq!(std::fs::read(game.path().join(DLL)).unwrap(), b"outro mod");
        assert!(data.join("config.ini").is_file());
    }

    #[test]
    fn install_replaces_the_old_separate_mods() {
        let (pkg, game) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        std::fs::write(pkg.path().join(DLL), b"xx GW2ENHANCED-DLL-MARKER!! xx").unwrap();
        std::fs::write(game.path().join("Gw2-64.exe"), b"").unwrap();
        // Old GW2 Remastered msimg32.dll: ours, so no backup.
        std::fs::write(game.path().join(DLL), b"xx GW2REMASTERED-DLL-MARKER xx").unwrap();
        // Old GW2 Translated version.dll, which had backed up another mod's version.dll.
        std::fs::write(game.path().join(LEGACY_DLL), b"xx GW2TRANSLATED-DLL-MARKER xx").unwrap();
        std::fs::write(game.path().join(LEGACY_BACKUP), b"outro version.dll").unwrap();

        install(pkg.path(), game.path()).unwrap();
        assert!(has_marker(&game.path().join(DLL), &[b"GW2ENHANCED-DLL-MARKER!!"]));
        assert!(!game.path().join(BACKUP).exists());
        assert_eq!(
            std::fs::read(game.path().join(LEGACY_DLL)).unwrap(),
            b"outro version.dll"
        );
        assert!(!game.path().join(LEGACY_BACKUP).exists());

        // A version.dll that is not ours is left alone.
        install(pkg.path(), game.path()).unwrap();
        assert_eq!(
            std::fs::read(game.path().join(LEGACY_DLL)).unwrap(),
            b"outro version.dll"
        );

        uninstall(game.path()).unwrap();
        assert!(!game.path().join(DLL).exists());
    }

    #[test]
    fn install_refuses_a_package_without_the_dll() {
        let (pkg, game) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        std::fs::write(game.path().join("Gw2-64.exe"), b"").unwrap();
        assert!(install(pkg.path(), game.path()).unwrap_err().contains(DLL));
    }
}
