//! Links against the official CTranslate2 DLL in `vendor/ctranslate2-<version>/`
//! (fetched by `scripts/fetch-ctranslate2.ps1`) and copies the runtime DLLs next
//! to the built exe, so `target/<profile>/gw2translated-translator.exe` runs as is.
//! The DirectML runtime (`vendor/directml`, `scripts/fetch-directml.ps1`) is only
//! loaded at run time and goes to `target/<profile>/directml/`.

use std::path::PathBuf;

const CT2_VERSION: &str = "4.8.4";
const RUNTIME: &[&str] = &["ctranslate2.dll", "libiomp5md.dll"];
const DIRECTML: &[&str] = &[
    "onnxruntime.dll",
    "DirectML.dll",
    "onnxruntime-LICENSE.txt",
    "DirectML-LICENSE.txt",
];

fn copy_if_changed(src: &std::path::Path, dst: &std::path::Path) {
    let stale = std::fs::metadata(dst).map(|m| m.len()).ok() != std::fs::metadata(src).map(|m| m.len()).ok();
    if stale {
        std::fs::copy(src, dst).unwrap_or_else(|e| panic!("copy {} -> {}: {e}", src.display(), dst.display()));
    }
}

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let vendor = manifest.join("vendor").join(format!("ctranslate2-{CT2_VERSION}"));
    let directml = manifest.join("vendor").join("directml");
    println!("cargo:rerun-if-changed={}", vendor.display());
    println!("cargo:rerun-if-changed={}", directml.display());
    println!("cargo:rerun-if-changed=build.rs");
    if !vendor.join("ctranslate2.lib").is_file() {
        panic!(
            "{} is missing: run scripts/fetch-ctranslate2.ps1 first (downloads the official CTranslate2 {CT2_VERSION} build)",
            vendor.display()
        );
    }
    // ct2rs (`system` feature) asks for ctranslate2.lib, cpu_features.lib and cudart_static.lib.
    println!("cargo:rustc-link-search=native={}", vendor.display());

    // OUT_DIR = target/<profile>/build/<pkg>-<hash>/out → the exe goes in target/<profile>/.
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    if let Some(profile_dir) = out.ancestors().nth(3) {
        for dll in RUNTIME {
            copy_if_changed(&vendor.join(dll), &profile_dir.join(dll));
        }
        let dst = profile_dir.join("directml");
        std::fs::create_dir_all(&dst).unwrap();
        for file in DIRECTML {
            let src = directml.join(file);
            if !src.is_file() {
                panic!("{} is missing: run scripts/fetch-directml.ps1 first", src.display());
            }
            copy_if_changed(&src, &dst.join(file));
        }
    }
}
