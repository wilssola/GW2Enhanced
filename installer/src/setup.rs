//! Installing and removing the mod in a game folder.
//!
//! A package is a folder with `msimg32.dll` and the data folders of the mods it has (`GW2Translated\`,
//! `GW2Graphics\`, ...): the release zip unpacked, or the folder the installer was extracted with.
//!
//! Never deletes the player's collected strings, live translations, logs or `config.ini`: the package never has
//! them, and installing only adds or overwrites the files the package has.
//!
//! Upgrades from the old separate mods: GW2 Translated's `version.dll` is removed (the translation now lives in
//! `msimg32.dll`; both at once would hook the game twice) and the `version.dll` it had backed up is put back; GW2
//! Remastered's `msimg32.dll` is replaced without a backup, as it is ours.

use std::path::{Path, PathBuf};

use serde::Serialize;

/// The mod's proxy DLL, next to `Gw2-64.exe`.
pub const DLL: &str = "msimg32.dll";
pub const GAME_EXE: &str = "Gw2-64.exe";
/// Markers of a `msimg32.dll` that is ours: GW2 Enhanced, and the old GW2 Remastered (same file name).
const DLL_MARKERS: &[&[u8]] = &[b"GW2ENHANCED-DLL-MARKER!!", b"GW2REMASTERED-DLL-MARKER"];
const ENHANCED_MARKER: &[u8] = b"GW2ENHANCED-DLL-MARKER!!";
/// Copy of another mod's `msimg32.dll` found during installation.
pub const BACKUP: &str = "msimg32.dll.antes-do-gw2enhanced";
/// The old GW2 Translated proxy, its marker and the backup it made of another mod's `version.dll`.
pub const LEGACY_DLL: &str = "version.dll";
const LEGACY_MARKER: &[u8] = b"GW2TRANSLATED-DLL-MARKER";
pub const LEGACY_BACKUP: &str = "version.dll.antes-do-gw2translated";
/// Data folders a package may carry next to `msimg32.dll` (one per mod), copied as they are.
pub const DATA_DIRS: &[&str] = &["GW2Translated", "GW2Remastered", "GW2Graphics"];
/// Translation files from older versions, removed on install (per language).
const OBSOLETE: &[&str] = &["strings.jsonl", "ids.jsonl"];

pub fn is_game_dir(p: &Path) -> bool {
    p.join(GAME_EXE).is_file()
}

pub fn has_marker(file: &Path, markers: &[&[u8]]) -> bool {
    std::fs::read(file).is_ok_and(|b| markers.iter().any(|m| b.windows(m.len()).any(|w| w == *m)))
}

pub fn is_ours(dll: &Path) -> bool {
    has_marker(dll, DLL_MARKERS)
}

/// What is in a game folder.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Status {
    /// Our `msimg32.dll` (GW2 Enhanced) is there.
    pub installed: bool,
    /// Version and package from the DLL (`None` for DLLs older than the build info, or the old GW2 Remastered).
    pub version: Option<String>,
    pub package: Option<String>,
    /// The old GW2 Remastered DLL (to be replaced by an install).
    pub legacy_remastered: bool,
    /// The old GW2 Translated `version.dll` (removed by an install).
    pub legacy_translated: bool,
    /// Another mod's `msimg32.dll` is in place (an install backs it up).
    pub foreign_dll: bool,
    /// A backup of another mod's `msimg32.dll` (put back on uninstall).
    pub backup: bool,
    /// Data folders present.
    pub data_dirs: Vec<String>,
}

/// Version and package of a DLL file of ours, when it says.
pub fn dll_info(dll: &Path) -> Option<(String, String)> {
    let bytes = std::fs::read(dll).ok()?;
    gw2enhanced_core::release::parse_build_info(&bytes)
}

pub fn status(game: &Path) -> Status {
    let dll = game.join(DLL);
    let mut s = Status::default();
    if dll.is_file() {
        let bytes = std::fs::read(&dll).unwrap_or_default();
        let find = |m: &[u8]| bytes.windows(m.len()).any(|w| w == m);
        if find(ENHANCED_MARKER) {
            s.installed = true;
            if let Some((v, p)) = gw2enhanced_core::release::parse_build_info(&bytes) {
                s.version = Some(v);
                s.package = Some(p).filter(|p| !p.is_empty());
            }
        } else if find(b"GW2REMASTERED-DLL-MARKER") {
            s.legacy_remastered = true;
        } else {
            s.foreign_dll = true;
        }
    }
    let legacy = game.join(LEGACY_DLL);
    s.legacy_translated = legacy.is_file() && has_marker(&legacy, &[LEGACY_MARKER]);
    s.backup = game.join(BACKUP).is_file();
    s.data_dirs = DATA_DIRS.iter().filter(|d| game.join(d).is_dir()).map(|d| d.to_string()).collect();
    s
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

fn same_dir(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Removes the old GW2 Translated `version.dll` (and puts back the one it had backed up).
fn remove_legacy(game: &Path, log: &mut Vec<String>) -> Result<(), String> {
    let dll = game.join(LEGACY_DLL);
    if dll.is_file() && has_marker(&dll, &[LEGACY_MARKER]) {
        std::fs::remove_file(&dll)
            .map_err(|e| format!("não consegui remover o version.dll antigo do GW2 Translated: {e}"))?;
        log.push(format!("version.dll antigo do GW2 Translated removido (a tradução agora fica no {DLL})"));
        let backup = game.join(LEGACY_BACKUP);
        if backup.is_file() {
            std::fs::rename(&backup, &dll).map_err(|e| format!("não consegui restaurar o version.dll anterior: {e}"))?;
            log.push("version.dll de outro mod restaurado".into());
        }
    }
    Ok(())
}

/// Installs the package in `pkg` into `game`; returns what was done, one line per step.
pub fn install(pkg: &Path, game: &Path) -> Result<Vec<String>, String> {
    let mut log = Vec::new();
    let new_dll = pkg.join(DLL);
    if !new_dll.is_file() {
        return Err(format!("pacote incompleto: falta {DLL} em {}", pkg.display()));
    }
    remove_legacy(game, &mut log)?;
    if same_dir(pkg, game) {
        // The zip was extracted straight into the game folder: the files are already in place.
        log.push("o pacote já está na pasta do jogo".into());
        return Ok(log);
    }
    let dll = game.join(DLL);
    if dll.is_file() && !is_ours(&dll) {
        let backup = game.join(BACKUP);
        if !backup.exists() {
            std::fs::copy(&dll, &backup).map_err(|e| format!("não consegui guardar o {DLL} atual: {e}"))?;
            log.push(format!("{DLL} de outro mod guardado como {BACKUP}"));
        }
    }
    std::fs::copy(&new_dll, &dll).map_err(|e| format!("não consegui copiar o {DLL}: {e}"))?;
    log.push(format!("{DLL} instalado"));

    for dir in DATA_DIRS {
        let from = pkg.join(dir);
        if !from.is_dir() {
            continue;
        }
        let to = game.join(dir);
        let n = copy_dir(&from, &to).map_err(|e| format!("{dir}: {e}"))?;
        log.push(format!("{dir}{} atualizado ({n} arquivos)", std::path::MAIN_SEPARATOR));
        if let Ok(langs) = std::fs::read_dir(to.join("languages")) {
            for lang in langs.flatten() {
                for f in OBSOLETE {
                    let _ = std::fs::remove_file(lang.path().join(f));
                }
            }
        }
    }
    Ok(log)
}

/// Removes the mod from `game` and puts back another mod's DLL; the data folders stay.
pub fn uninstall(game: &Path) -> Result<Vec<String>, String> {
    let mut log = Vec::new();
    remove_legacy(game, &mut log)?;
    let dll = game.join(DLL);
    if dll.is_file() && is_ours(&dll) {
        std::fs::remove_file(&dll).map_err(|e| format!("não consegui remover o {DLL}: {e}"))?;
        log.push(format!("{DLL} do GW2 Enhanced removido"));
    }
    let backup = game.join(BACKUP);
    if backup.is_file() && !dll.exists() {
        std::fs::rename(&backup, &dll).map_err(|e| format!("não consegui restaurar o {DLL} anterior: {e}"))?;
        log.push(format!("{DLL} anterior restaurado"));
    }
    let dirs: Vec<String> = DATA_DIRS.iter().map(|d| format!("{d}{}", std::path::MAIN_SEPARATOR)).collect();
    log.push(format!(
        "as pastas {} (configuração, coleta, logs) foram mantidas; apague à mão se quiser",
        dirs.join(", ")
    ));
    Ok(log)
}

/// The package folder the installer itself sits in (an extracted release zip), when it has our DLL.
pub fn local_package() -> Option<PathBuf> {
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    let dll = dir.join(DLL);
    (dll.is_file() && has_marker(&dll, &[ENHANCED_MARKER])).then_some(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_keeps_player_files_and_backs_up_foreign_dll() {
        let (pkg, game) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        std::fs::write(pkg.path().join(DLL), b"xx GW2ENHANCED-DLL-MARKER!! xx").unwrap();
        let lang = pkg.path().join("GW2Translated/languages/pt-BR");
        std::fs::create_dir_all(&lang).unwrap();
        std::fs::create_dir_all(pkg.path().join("GW2Translated/translator/models/pt-BR")).unwrap();
        std::fs::create_dir_all(pkg.path().join("GW2Graphics/luts")).unwrap();
        std::fs::write(lang.join("dictionary.jsonl"), "{}\n").unwrap();
        std::fs::write(lang.join("web.jsonl"), "").unwrap();
        std::fs::write(pkg.path().join("GW2Translated/translator/t.exe"), b"t").unwrap();
        std::fs::write(pkg.path().join("GW2Translated/translator/models/pt-BR/m.bin"), b"m").unwrap();
        std::fs::write(pkg.path().join("GW2Graphics/luts/warm.cube"), b"lut").unwrap();
        std::fs::write(game.path().join("Gw2-64.exe"), b"").unwrap();
        std::fs::write(game.path().join(DLL), b"outro mod").unwrap();
        let data = game.path().join("GW2Translated");
        std::fs::create_dir_all(data.join("languages/pt-BR")).unwrap();
        std::fs::write(data.join("config.ini"), "translate=false").unwrap();
        std::fs::write(data.join("languages/pt-BR/local.jsonl"), "{\"en\":\"Hi\",\"tr\":\"Oi\"}\n").unwrap();
        std::fs::write(data.join("languages/pt-BR/ids.jsonl"), "").unwrap();

        assert!(status(game.path()).foreign_dll);
        install(pkg.path(), game.path()).unwrap();
        assert!(is_ours(&game.path().join(DLL)));
        assert_eq!(std::fs::read(game.path().join(BACKUP)).unwrap(), b"outro mod");
        assert_eq!(std::fs::read_to_string(data.join("config.ini")).unwrap(), "translate=false");
        assert!(data.join("languages/pt-BR/local.jsonl").is_file());
        assert!(data.join("languages/pt-BR/dictionary.jsonl").is_file());
        assert!(!data.join("languages/pt-BR/ids.jsonl").exists());
        assert!(data.join("translator/t.exe").is_file());
        assert!(data.join("translator/models/pt-BR/m.bin").is_file());
        assert!(game.path().join("GW2Graphics/luts/warm.cube").is_file());
        assert!(!game.path().join("GW2Remastered").exists());
        let s = status(game.path());
        assert!(s.installed && s.backup && !s.foreign_dll);
        assert_eq!(s.data_dirs, vec!["GW2Translated".to_string(), "GW2Graphics".to_string()]);

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
        let s = status(game.path());
        assert!(s.legacy_remastered && s.legacy_translated && !s.installed);

        install(pkg.path(), game.path()).unwrap();
        assert!(has_marker(&game.path().join(DLL), &[b"GW2ENHANCED-DLL-MARKER!!"]));
        assert!(!game.path().join(BACKUP).exists());
        assert_eq!(std::fs::read(game.path().join(LEGACY_DLL)).unwrap(), b"outro version.dll");
        assert!(!game.path().join(LEGACY_BACKUP).exists());

        // A version.dll that is not ours is left alone.
        install(pkg.path(), game.path()).unwrap();
        assert_eq!(std::fs::read(game.path().join(LEGACY_DLL)).unwrap(), b"outro version.dll");

        uninstall(game.path()).unwrap();
        assert!(!game.path().join(DLL).exists());
    }

    #[test]
    fn install_refuses_a_package_without_the_dll() {
        let (pkg, game) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        std::fs::write(game.path().join("Gw2-64.exe"), b"").unwrap();
        assert!(install(pkg.path(), game.path()).unwrap_err().contains(DLL));
    }

    #[test]
    fn a_package_extracted_into_the_game_folder_is_left_in_place() {
        let game = tempfile::tempdir().unwrap();
        std::fs::write(game.path().join("Gw2-64.exe"), b"").unwrap();
        std::fs::write(game.path().join(DLL), b"xx GW2ENHANCED-DLL-MARKER!! xx").unwrap();
        std::fs::create_dir_all(game.path().join("GW2Graphics/luts")).unwrap();
        std::fs::write(game.path().join("GW2Graphics/luts/warm.cube"), b"lut").unwrap();
        install(game.path(), game.path()).unwrap();
        assert!(is_ours(&game.path().join(DLL)));
        assert!(game.path().join("GW2Graphics/luts/warm.cube").is_file());
        assert!(!game.path().join(BACKUP).exists());
    }

    #[test]
    fn status_reads_version_and_package_from_the_dll() {
        let game = tempfile::tempdir().unwrap();
        let mut dll = b"MZ xx GW2ENHANCED-DLL-MARKER!! xx ".to_vec();
        dll.extend_from_slice(&gw2enhanced_core::release::build_info("1.3.0", "Graphics"));
        std::fs::write(game.path().join(DLL), dll).unwrap();
        let s = status(game.path());
        assert!(s.installed);
        assert_eq!((s.version.as_deref(), s.package.as_deref()), (Some("1.3.0"), Some("Graphics")));
        assert_eq!(status(&game.path().join("missing")), Status::default());
    }
}
