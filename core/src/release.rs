//! The release layout shared by the in-game updater and the installer.
//!
//! Every GitHub release of `wilssola/GW2Enhanced` carries one zip per package (`GW2Enhanced-<Package>-<ver>.zip`),
//! `SHA256SUMS.txt` and its ed25519 signature `SHA256SUMS.txt.sig` (see [`crate::signed`]). Both programs download
//! only through this repository's release asset API, check the signature first, then the zip's checksum, and unpack
//! with the same path rules.

use std::path::{Component, Path, PathBuf};

pub const OWNER: &str = "wilssola";
pub const REPO: &str = "GW2Enhanced";

/// The packages CI builds (one zip each per release, see README "Downloads").
pub const PACKAGES: &[&str] = &[
    "All",
    "Translation",
    "Translation-CPU",
    "Translation-CUDA",
    "Translation-DirectML",
    "Textures",
    "Graphics",
];

pub const SUMS_ASSET: &str = "SHA256SUMS.txt";
/// ed25519 signature of SHA256SUMS.txt (see [`crate::signed`]): without a valid one nothing is installed.
pub const SIG_ASSET: &str = "SHA256SUMS.txt.sig";
/// Every asset is fetched through the GitHub API of this repository (never a url some JSON points elsewhere).
pub const ASSET_URL_PREFIX: &str = "https://api.github.com/repos/wilssola/GW2Enhanced/releases/assets/";

/// The asset url is this repository's release asset API (`.../releases/assets/<numeric id>`).
pub fn asset_url_ok(url: &str) -> bool {
    url.strip_prefix(ASSET_URL_PREFIX).is_some_and(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
}

pub fn asset_name(package: &str, version: &str) -> String {
    format!("GW2Enhanced-{package}-{}.zip", version.trim_start_matches('v'))
}

/// The sum `SHA256SUMS.txt` (sha256sum format: `<hex>  <name>`, or `<hex> *<name>` in binary mode) lists for `name`.
pub fn expected_sum(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let (sum, file) = line.trim().split_once(char::is_whitespace)?;
        let file = file.trim().trim_start_matches('*');
        let file = file.strip_prefix("./").unwrap_or(file);
        (file == name && sum.len() == 64 && sum.bytes().all(|b| b.is_ascii_hexdigit())).then(|| sum.to_ascii_lowercase())
    })
}

/// The zip entry name as a path relative to the game folder, or `None` when it is unsafe: absolute, with a drive or
/// stream (`:`), with `.` / `..` parts, or with names Windows would change or treat as devices.
pub fn safe_relative(name: &str) -> Option<PathBuf> {
    if name.is_empty() || name.starts_with('/') || name.starts_with('\\') || name.contains(':') {
        return None;
    }
    let parts: Vec<&str> = name.split(['/', '\\']).collect();
    let mut out = PathBuf::new();
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            // Only a trailing separator (a directory entry) is fine.
            if i == parts.len() - 1 && i > 0 {
                continue;
            }
            return None;
        }
        if *part == "." || *part == ".." || part.ends_with('.') || part.ends_with(' ') {
            return None;
        }
        if part.chars().any(|c| c.is_control() || matches!(c, '<' | '>' | '"' | '|' | '?' | '*')) {
            return None;
        }
        let stem = part.split('.').next().unwrap_or("").to_ascii_uppercase();
        let device = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || ((stem.starts_with("COM") || stem.starts_with("LPT"))
                && stem.len() == 4
                && stem.as_bytes()[3].is_ascii_digit());
        if device {
            return None;
        }
        out.push(part);
    }
    // Belt and braces: what Path makes of it must be plain names only.
    if out.as_os_str().is_empty() || !out.components().all(|c| matches!(c, Component::Normal(_))) {
        return None;
    }
    Some(out)
}

/// Files an update never writes even if a zip had them: the player's settings and collected/own translations, and
/// the game's own files.
pub fn is_protected(rel: &Path) -> bool {
    let Some(name) = rel.file_name().and_then(|n| n.to_str()) else {
        return true;
    };
    const NAMES: &[&str] = &[
        "config.ini",
        "local.jsonl",
        "web_local.jsonl",
        "discovered.jsonl",
        "decrypted.jsonl",
        "Gw2-64.exe",
        "Gw2.dat",
        "Local.dat",
    ];
    NAMES.iter().any(|n| name.eq_ignore_ascii_case(n))
}

// ---------------------------------------------------------------------------------------------------------------
// Which release this build is: CI sets GW2E_VERSION (the tag) and GW2E_PACKAGE (the flavour) for every package.

/// The version shown for a local build (no `GW2E_VERSION`).
pub const DEV_VERSION: &str = "dev";

/// The release version this build is (without `v`), or [`DEV_VERSION`] for a local build.
pub fn version() -> &'static str {
    version_from(option_env!("GW2E_VERSION"))
}

/// The package this build is (`All`, `Translation-CUDA`...), or `None` for a local build.
pub fn package() -> Option<&'static str> {
    package_from(option_env!("GW2E_PACKAGE"))
}

/// The build is a release (CI set its version), so it can be compared with the published ones.
pub fn is_release() -> bool {
    version() != DEV_VERSION
}

/// "v1.4.0 (All)", "v1.4.0", or "dev" for a local build: what the overlay shows.
pub fn label() -> String {
    label_of(version(), package())
}

pub fn version_from(v: Option<&'static str>) -> &'static str {
    v.map(|v| v.trim().trim_start_matches('v')).filter(|v| !v.is_empty()).unwrap_or(DEV_VERSION)
}

pub fn package_from(p: Option<&'static str>) -> Option<&'static str> {
    p.map(str::trim).filter(|p| !p.is_empty())
}

pub fn label_of(version: &str, package: Option<&str>) -> String {
    match (version, package) {
        (DEV_VERSION, Some(p)) => format!("{DEV_VERSION} ({p})"),
        (DEV_VERSION, None) => DEV_VERSION.to_string(),
        (v, Some(p)) => format!("v{v} ({p})"),
        (v, None) => format!("v{v}"),
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Build info inside msimg32.dll: which version and package a DLL is, readable from the file without loading it.

/// Start of the build info record in the DLL.
pub const BUILD_INFO_PREFIX: &[u8] = b"GW2ENHANCED-BUILD-INFO:";
/// Fixed size of the record (prefix, `<version>;<package>`, NUL padding).
pub const BUILD_INFO_LEN: usize = 96;

/// The record `GW2ENHANCED-BUILD-INFO:<version>;<package>` padded with NULs (cut if too long).
pub const fn build_info(version: &str, package: &str) -> [u8; BUILD_INFO_LEN] {
    let mut out = [0u8; BUILD_INFO_LEN];
    let mut n = 0;
    n = put(&mut out, n, BUILD_INFO_PREFIX);
    n = put(&mut out, n, version.as_bytes());
    n = put(&mut out, n, b";");
    put(&mut out, n, package.as_bytes());
    out
}

const fn put(out: &mut [u8; BUILD_INFO_LEN], mut at: usize, bytes: &[u8]) -> usize {
    let mut i = 0;
    // The last byte stays NUL: the record always ends.
    while i < bytes.len() && at < BUILD_INFO_LEN - 1 {
        out[at] = bytes[i];
        at += 1;
        i += 1;
    }
    at
}

/// `(version without v, package)` from the bytes of a DLL built with [`build_info`]; `None` for older DLLs. The
/// package is empty for a local (development) build.
pub fn parse_build_info(dll: &[u8]) -> Option<(String, String)> {
    let start = dll.windows(BUILD_INFO_PREFIX.len()).position(|w| w == BUILD_INFO_PREFIX)? + BUILD_INFO_PREFIX.len();
    let rest = &dll[start..dll.len().min(start + BUILD_INFO_LEN)];
    let end = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
    let text = std::str::from_utf8(&rest[..end]).ok()?;
    let (version, package) = text.split_once(';')?;
    let version = version.trim().trim_start_matches('v');
    if version.is_empty() || !version.bytes().all(|b| b.is_ascii_alphanumeric() || b".-+".contains(&b)) {
        return None;
    }
    Some((version.to_string(), package.trim().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_build_is_named_by_its_release_version_and_package() {
        assert_eq!(version_from(Some("v1.4.0")), "1.4.0");
        assert_eq!(version_from(Some(" 2.0.1 ")), "2.0.1");
        assert_eq!(version_from(Some("")), DEV_VERSION);
        assert_eq!(version_from(None), DEV_VERSION);
        assert_eq!(package_from(Some("Translation-CUDA")), Some("Translation-CUDA"));
        assert_eq!(package_from(Some(" ")), None);
        assert_eq!(label_of("1.4.0", Some("All")), "v1.4.0 (All)");
        assert_eq!(label_of("1.4.0", None), "v1.4.0");
        assert_eq!(label_of(DEV_VERSION, None), "dev");
        assert_eq!(label_of(DEV_VERSION, Some("Graphics")), "dev (Graphics)");
        // This build: a local one is "dev", never the crate version.
        assert_eq!(is_release(), option_env!("GW2E_VERSION").is_some_and(|v| !v.trim().trim_start_matches('v').is_empty()));
        if !is_release() {
            assert_eq!(version(), "dev");
            assert!(label().starts_with("dev"));
        }
        if let Some(p) = package() {
            assert!(PACKAGES.contains(&p), "GW2E_PACKAGE={p} is not a known package");
        }
    }

    #[test]
    fn asset_urls_must_be_this_repositorys_release_assets() {
        assert!(asset_url_ok("https://api.github.com/repos/wilssola/GW2Enhanced/releases/assets/123456"));
        assert!(!asset_url_ok("https://api.github.com/repos/someone/else/releases/assets/123456"));
        assert!(!asset_url_ok("https://api.github.com/repos/wilssola/GW2Enhanced/releases/assets/12/../x"));
        assert!(!asset_url_ok("https://evil.example/repos/wilssola/GW2Enhanced/releases/assets/1"));
        assert!(!asset_url_ok("https://api.github.com/repos/wilssola/GW2Enhanced/releases/assets/"));
    }

    #[test]
    fn asset_names_follow_the_release_layout() {
        assert_eq!(asset_name("All", "1.4.0"), "GW2Enhanced-All-1.4.0.zip");
        assert_eq!(asset_name("Translation-DirectML", "v2.0.1"), "GW2Enhanced-Translation-DirectML-2.0.1.zip");
        assert!(PACKAGES.contains(&"Translation-CUDA"));
    }

    #[test]
    fn sha256sums_are_parsed_in_text_and_binary_mode() {
        let a = "a".repeat(64);
        let b = "B".repeat(64);
        let sums = format!(
            "{a}  GW2Enhanced-All-1.0.0.zip\n{b} *GW2Enhanced-Graphics-1.0.0.zip\n\
             {a}  ./GW2Enhanced-Textures-1.0.0.zip\r\nnot a line\nabc  GW2Enhanced-Translation-1.0.0.zip\n"
        );
        assert_eq!(expected_sum(&sums, "GW2Enhanced-All-1.0.0.zip"), Some(a.clone()));
        assert_eq!(expected_sum(&sums, "GW2Enhanced-Graphics-1.0.0.zip"), Some("b".repeat(64)));
        assert_eq!(expected_sum(&sums, "GW2Enhanced-Textures-1.0.0.zip"), Some(a));
        // A short (bad) sum is not a sum; a name that is only a prefix is not a match.
        assert_eq!(expected_sum(&sums, "GW2Enhanced-Translation-1.0.0.zip"), None);
        assert_eq!(expected_sum(&sums, "GW2Enhanced-All-1.0.0"), None);
        assert_eq!(expected_sum("", "x.zip"), None);
    }

    #[test]
    fn zip_paths_must_stay_inside_the_game_folder() {
        assert_eq!(safe_relative("msimg32.dll"), Some(PathBuf::from("msimg32.dll")));
        assert_eq!(
            safe_relative("GW2Translated/translator/gw2translated-translator.exe"),
            Some(PathBuf::from("GW2Translated").join("translator").join("gw2translated-translator.exe"))
        );
        assert_eq!(safe_relative("GW2Translated\\translator\\"), Some(PathBuf::from("GW2Translated").join("translator")));
        for bad in [
            "",
            "../evil.dll",
            "a/../../evil.dll",
            "a/./b",
            "/abs.dll",
            "\\abs.dll",
            "C:/Windows/evil.dll",
            "C:evil.dll",
            "file.txt:stream",
            "a//b",
            "trailing.",
            "trailing ",
            "NUL",
            "con.txt",
            "dir/COM1.dll",
            "a*b",
            "x\u{1}y",
        ] {
            assert_eq!(safe_relative(bad), None, "{bad:?}");
        }
        assert!(is_protected(Path::new("GW2Translated/config.ini")));
        assert!(is_protected(Path::new("CONFIG.INI")));
        assert!(is_protected(Path::new("GW2Translated/languages/pt-BR/local.jsonl")));
        assert!(is_protected(Path::new("Gw2-64.exe")));
        assert!(!is_protected(Path::new("GW2Translated/languages/pt-BR/dictionary.jsonl")));
        assert!(!is_protected(Path::new("msimg32.dll")));
    }

    #[test]
    fn build_info_round_trips_through_dll_bytes() {
        let info = build_info("1.4.0", "Translation-CUDA");
        let mut dll = b"MZ....some code....".to_vec();
        dll.extend_from_slice(&info);
        dll.extend_from_slice(b"more bytes");
        assert_eq!(parse_build_info(&dll), Some(("1.4.0".into(), "Translation-CUDA".into())));
        // A local build has no package.
        assert_eq!(parse_build_info(&build_info("0.1.0", "")), Some(("0.1.0".into(), String::new())));
        // Older DLLs have no record; garbage is not a version.
        assert_eq!(parse_build_info(b"GW2ENHANCED-DLL-MARKER!!"), None);
        assert_eq!(parse_build_info(b"GW2ENHANCED-BUILD-INFO:\x01\x02;All\0"), None);
        // Too long: cut, still terminated.
        let long = build_info(&"9".repeat(200), "All");
        assert_eq!(long[BUILD_INFO_LEN - 1], 0);
    }
}
