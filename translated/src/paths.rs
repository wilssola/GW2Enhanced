//! `GW2Translated\` folder next to `Gw2-64.exe`.
//!
//! ```text
//! GW2Translated\
//! ├─ config.ini
//! ├─ gw2translated_log.txt
//! ├─ discovered.jsonl, decrypted.jsonl      collection (always the original English)
//! ├─ languages\<language>\                  dictionary.jsonl, local.jsonl
//! │  └─ release\                            downloaded dictionary releases (see `release.rs`)
//! └─ translator\
//!    ├─ gw2translated-translator.exe
//!    └─ models\<language>\                  local translation model
//! ```

use std::path::{Path, PathBuf};

pub const DIR_NAME: &str = "GW2Translated";

/// Language code that is safe to use as a folder name (`pt-BR`, `es`, `zh-Hans`).
pub fn valid_language(code: &str) -> bool {
    !code.is_empty() && code.len() <= 16 && code.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

#[derive(Debug, Clone)]
pub struct Paths {
    pub data: PathBuf,
    pub language: String,
}

impl Paths {
    pub fn new(game_dir: &Path, language: &str) -> Self {
        Self {
            data: game_dir.join(DIR_NAME),
            language: language.to_owned(),
        }
    }
    pub fn config(&self) -> PathBuf {
        self.data.join("config.ini")
    }
    pub fn log(&self) -> PathBuf {
        self.data.join("gw2translated_log.txt")
    }
    pub fn discovered(&self) -> PathBuf {
        self.data.join("discovered.jsonl")
    }
    pub fn decrypted(&self) -> PathBuf {
        self.data.join("decrypted.jsonl")
    }
    pub fn language_dir(&self) -> PathBuf {
        self.data.join("languages").join(&self.language)
    }
    pub fn dictionary(&self) -> PathBuf {
        self.language_dir().join("dictionary.jsonl")
    }
    pub fn local(&self) -> PathBuf {
        self.language_dir().join("local.jsonl")
    }
    /// Dictionary releases downloaded from the project's server.
    pub fn release_dir(&self) -> PathBuf {
        self.language_dir().join("release")
    }
    /// Translations of web page texts made on this PC (never uploaded).
    pub fn web_local(&self) -> PathBuf {
        self.language_dir().join("web_local.jsonl")
    }
    pub fn translator_dir(&self) -> PathBuf {
        self.data.join("translator")
    }
    pub fn translator_exe(&self) -> PathBuf {
        self.translator_dir().join("gw2translated-translator.exe")
    }
    pub fn model_dir(&self) -> PathBuf {
        self.translator_dir().join("models").join(&self.language)
    }
    /// Larger, better model used on the GPU (optional download).
    pub fn model_big_dir(&self) -> PathBuf {
        self.translator_dir()
            .join("models")
            .join(format!("{}-big", self.language))
    }
    /// NVIDIA's cuBLAS DLLs for CUDA (optional download).
    pub fn gpu_runtime_dir(&self) -> PathBuf {
        self.translator_dir().join("gpu")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_codes_cannot_escape_the_folder() {
        assert!(valid_language("pt-BR") && valid_language("zh-Hans") && valid_language("es"));
        assert!(!valid_language("..\\x") && !valid_language("") && !valid_language("a/b"));
    }
}
