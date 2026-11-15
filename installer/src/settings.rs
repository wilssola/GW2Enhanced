//! The installer's own settings, in `%APPDATA%\GW2Enhanced\installer.json`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::launch::LaunchOptions;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// The chosen game folder (one of the detected installs, or picked by hand).
    pub game_dir: Option<PathBuf>,
    /// Folders the player picked by hand, kept in the list of installs.
    pub manual_dirs: Vec<PathBuf>,
    pub check_updates_on_start: bool,
    /// The package to install or update to (one of `gw2enhanced_core::release::PACKAGES`).
    pub package: String,
    /// Lists pre-releases too.
    pub show_prereleases: bool,
    pub launch: LaunchOptions,
    /// Closes the installer once the game has been started.
    pub close_on_launch: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            game_dir: None,
            manual_dirs: Vec::new(),
            check_updates_on_start: true,
            package: "All".into(),
            show_prereleases: false,
            launch: LaunchOptions::default(),
            close_on_launch: false,
        }
    }
}

fn path() -> Option<PathBuf> {
    let base = std::env::var_os("APPDATA")?;
    Some(PathBuf::from(base).join("GW2Enhanced").join("installer.json"))
}

impl Settings {
    pub fn load() -> Self {
        path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<(), String> {
        let p = path().ok_or("APPDATA não definido")?;
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("não consegui salvar as configurações: {e}"))?;
        }
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(&p, text).map_err(|e| format!("não consegui salvar as configurações: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_fields_take_their_defaults() {
        let s: Settings = serde_json::from_str(r#"{"package":"Graphics","launch":{"autologin":true}}"#).unwrap();
        assert_eq!(s.package, "Graphics");
        assert!(s.check_updates_on_start);
        assert!(s.launch.autologin && !s.launch.share_archive);
        let back: Settings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
    }
}
