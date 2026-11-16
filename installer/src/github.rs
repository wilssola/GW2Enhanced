//! The GitHub releases of GW2 Enhanced: the list with notes, and downloading a package the way the in-game updater
//! does (enhanced/src/update.rs).
//!
//! Every file comes through this repository's release asset API (`gw2enhanced_core::release::asset_url_ok`).
//! `SHA256SUMS.txt` must carry a valid ed25519 signature (`SHA256SUMS.txt.sig`, key built in) before anything in it
//! is trusted, and the zip must match its sum; then it is unpacked with the updater's path rules (nothing outside
//! the folder, never the player's files) and installed like a local package.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use gw2enhanced_core::release::{
    OWNER, PACKAGES, REPO, SIG_ASSET, SUMS_ASSET, asset_name, asset_url_ok, expected_sum, is_protected, safe_relative,
};
use serde::Serialize;

/// Size caps: the largest package (CUDA translator runtime) is well under this.
const MAX_DOWNLOAD: u64 = 6 * 1024 * 1024 * 1024;
const MAX_UNPACKED: u64 = 12 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Asset {
    pub name: String,
    /// The API url (works for private repositories too, with a token).
    #[serde(skip)]
    pub url: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Release {
    pub tag: String,
    /// Without the `v`.
    pub version: String,
    pub title: String,
    /// ISO 8601 (`published_at`).
    pub published_at: String,
    pub prerelease: bool,
    /// The notes, Markdown rendered to HTML with raw HTML escaped.
    pub notes_html: String,
    pub page: String,
    /// Packages this release has a zip of.
    pub packages: Vec<String>,
    /// Zip size of each of those packages (bytes).
    pub package_sizes: std::collections::BTreeMap<String, u64>,
    /// Signed checksums present.
    pub signed: bool,
    #[serde(skip)]
    pub assets: Vec<Asset>,
}

impl Release {
    pub fn asset(&self, name: &str) -> Option<&Asset> {
        self.assets.iter().find(|a| a.name == name)
    }

    pub fn package_size(&self, package: &str) -> Option<u64> {
        self.asset(&asset_name(package, &self.version)).map(|a| a.size)
    }
}

/// While the repository is private, the API needs a token (`GW2E_GH_TOKEN`, as for the in-game updater).
fn token() -> Option<String> {
    std::env::var("GW2E_GH_TOKEN").ok().map(|t| t.trim().to_string()).filter(|t| !t.is_empty())
}

fn get(url: &str, accept: &str, api: bool) -> Result<ureq::http::Response<ureq::Body>, String> {
    let mut req = ureq::get(url)
        .header("User-Agent", "GW2Enhanced-installer")
        .header("Accept", accept)
        .header("X-GitHub-Api-Version", "2022-11-28");
    if let Some(t) = token() {
        // ureq drops it on redirects to another host (the asset storage).
        req = req.header("Authorization", &format!("Bearer {t}"));
    }
    req.config()
        .timeout_connect(Some(Duration::from_secs(20)))
        .timeout_per_read(Some(Duration::from_secs(60)))
        .timeout_global(api.then(|| Duration::from_secs(30)))
        .build()
        .call()
        .map_err(|e| match e {
            ureq::Error::StatusCode(404) if token().is_none() => tr!(
                "o GitHub respondeu 404: o repositório ainda é privado \
                 ou não existe nenhuma versão publicada (com o repositório privado, defina a variável de ambiente \
                 GW2E_GH_TOKEN com um token de acesso)",
                "GitHub answered 404: the repository is still private \
                 or no version has been published (with a private repository, set the environment variable \
                 GW2E_GH_TOKEN to an access token)"
            ),
            ureq::Error::StatusCode(404) => tr!(
                "o GitHub respondeu 404: nenhuma versão publicada, ou o token em \
                 GW2E_GH_TOKEN não tem acesso ao repositório",
                "GitHub answered 404: no version published, or the token in \
                 GW2E_GH_TOKEN has no access to the repository"
            ),
            ureq::Error::StatusCode(c @ (401 | 403)) => tr!(
                "o GitHub recusou o acesso (HTTP {c}): token inválido ou limite de consultas atingido; tente mais tarde",
                "GitHub refused access (HTTP {c}): invalid token or rate limit reached; try again later"
            ),
            ureq::Error::StatusCode(c) => tr!("o GitHub respondeu HTTP {c}", "GitHub answered HTTP {c}"),
            e => tr!("sem conexão com o GitHub: {e}", "no connection to GitHub: {e}"),
        })
}

/// Markdown to HTML; raw HTML in the notes is shown as text and links other than http(s) are dropped.
pub fn render_markdown(md: &str) -> String {
    use pulldown_cmark::{CowStr, Event, Options, Parser, Tag};
    let safe = |url: &CowStr| {
        let u = url.trim().to_ascii_lowercase();
        u.starts_with("https://") || u.starts_with("http://")
    };
    let parser = Parser::new_ext(md, Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS)
        .map(|ev| match ev {
            Event::Html(h) | Event::InlineHtml(h) => Event::Text(h),
            Event::Start(Tag::Link { link_type, dest_url, title, id }) => {
                let dest_url = if safe(&dest_url) { dest_url } else { CowStr::Borrowed("") };
                Event::Start(Tag::Link { link_type, dest_url, title, id })
            }
            // Images would be fetched from anywhere: shown as their link instead.
            Event::Start(Tag::Image { link_type, dest_url, title, id }) => {
                let dest_url = if safe(&dest_url) { dest_url } else { CowStr::Borrowed("") };
                Event::Start(Tag::Link { link_type, dest_url, title, id })
            }
            Event::End(pulldown_cmark::TagEnd::Image) => Event::End(pulldown_cmark::TagEnd::Link),
            e => e,
        });
    let mut out = String::new();
    pulldown_cmark::html::push_html(&mut out, parser);
    out
}

fn parse_release(v: &serde_json::Value) -> Option<Release> {
    if v.get("draft").and_then(|d| d.as_bool()).unwrap_or(false) {
        return None;
    }
    let tag = v.get("tag_name")?.as_str()?.trim().to_string();
    let version = tag.trim_start_matches('v').to_string();
    let assets: Vec<Asset> = v
        .get("assets")
        .and_then(|a| a.as_array())
        .map(|assets| {
            assets
                .iter()
                .filter_map(|a| {
                    Some(Asset {
                        name: a.get("name")?.as_str()?.to_string(),
                        url: a.get("url")?.as_str()?.to_string(),
                        size: a.get("size").and_then(|s| s.as_u64()).unwrap_or(0),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let package_sizes: std::collections::BTreeMap<String, u64> = PACKAGES
        .iter()
        .filter_map(|p| assets.iter().find(|a| a.name == asset_name(p, &version)).map(|a| (p.to_string(), a.size)))
        .collect();
    let packages = PACKAGES.iter().filter(|p| package_sizes.contains_key(**p)).map(|p| p.to_string()).collect();
    let signed = assets.iter().any(|a| a.name == SUMS_ASSET) && assets.iter().any(|a| a.name == SIG_ASSET);
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    let title = Some(s("name")).filter(|n| !n.trim().is_empty()).unwrap_or_else(|| format!("GW2 Enhanced {tag}"));
    let page = s("html_url");
    let page = if page.starts_with(&format!("https://github.com/{OWNER}/{REPO}/")) { page } else { String::new() };
    Some(Release {
        tag,
        version,
        title,
        published_at: s("published_at"),
        prerelease: v.get("prerelease").and_then(|p| p.as_bool()).unwrap_or(false),
        notes_html: render_markdown(&s("body")),
        page,
        packages,
        package_sizes,
        signed,
        assets,
    })
}

/// Releases as the API lists them (newest first), drafts left out.
pub fn parse_releases(text: &str) -> Result<Vec<Release>, String> {
    let json: serde_json::Value = serde_json::from_str(text).map_err(|e| tr!("resposta inválida do GitHub: {e}", "invalid response from GitHub: {e}"))?;
    let list = json
        .as_array()
        .ok_or_else(|| tr!("resposta inválida do GitHub: não é uma lista", "invalid response from GitHub: not a list"))?;
    Ok(list.iter().filter_map(parse_release).collect())
}

pub fn list_releases() -> Result<Vec<Release>, String> {
    let url = format!("https://api.github.com/repos/{OWNER}/{REPO}/releases?per_page=30");
    let text = get(&url, "application/vnd.github+json", true)?
        .body_mut()
        .with_config()
        .limit(8 * 1024 * 1024)
        .read_to_string()
        .map_err(|e| e.to_string())?;
    parse_releases(&text)
}

fn semver(v: &str) -> Option<semver::Version> {
    // Build metadata (`+...`) never makes a release newer.
    let v = v.trim().trim_start_matches('v');
    semver::Version::parse(v.split('+').next().unwrap_or(v)).ok()
}

/// `candidate` is newer than `current` (semver; a pre-release is older than its release).
pub fn is_newer(current: &str, candidate: &str) -> bool {
    match (semver(current), semver(candidate)) {
        (Some(c), Some(n)) => n > c,
        _ => false,
    }
}

/// The newest stable release (the one the in-game updater calls "latest").
pub fn latest_stable(releases: &[Release]) -> Option<&Release> {
    releases.iter().filter(|r| !r.prerelease && semver(&r.version).is_some()).max_by(|a, b| {
        semver(&a.version).cmp(&semver(&b.version))
    })
}

/// Download progress: (what, bytes so far, total bytes or 0).
pub type Progress<'a> = &'a mut dyn FnMut(&str, u64, u64);

fn read_small(asset: &Asset, limit: u64) -> Result<Vec<u8>, String> {
    let mut resp = get(&asset.url, "application/octet-stream", false)?;
    resp.body_mut().with_config().limit(limit).read_to_vec().map_err(|e| tr!("falha ao baixar {}: {e}", "download of {} failed: {e}", asset.name))
}

/// Streams `asset` into `path`; returns its SHA-256 (lowercase hex).
fn download_to(asset: &Asset, path: &Path, progress: Progress) -> Result<String, String> {
    use sha2::Digest as _;
    let mut resp = get(&asset.url, "application/octet-stream", false)?;
    let mut reader = resp.body_mut().with_config().limit(MAX_DOWNLOAD).reader();
    let file = fs::File::create(path).map_err(|e| tr!("não consegui gravar {}: {e}", "could not write {}: {e}", path.display()))?;
    let mut out = std::io::BufWriter::with_capacity(1 << 20, file);
    let mut hasher = sha2::Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut total = 0u64;
    loop {
        let n = reader.read(&mut buf).map_err(|e| tr!("o download foi interrompido: {e}", "the download was interrupted: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        out.write_all(&buf[..n]).map_err(disk_full)?;
        total += n as u64;
        progress("download", total, asset.size);
    }
    out.flush().map_err(disk_full)?;
    if asset.size != 0 && total != asset.size {
        return Err(tr!("o download veio incompleto ({total} de {} bytes)", "the download is incomplete ({total} of {} bytes)", asset.size));
    }
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

fn disk_full(e: std::io::Error) -> String {
    tr!("não consegui gravar o download (disco cheio?): {e}", "could not write the download (disk full?): {e}")
}

fn bad_zip(e: impl std::fmt::Display) -> String {
    tr!("zip inválido: {e}", "invalid zip: {e}")
}

fn unpack_failed(e: std::io::Error) -> String {
    tr!("não consegui descompactar (disco cheio?): {e}", "could not unpack (disk full?): {e}")
}

/// Unpacks `zip` into `into`, refusing unsafe names; returns the files (relative), protected ones left out.
pub fn unpack(zip: &Path, into: &Path) -> Result<Vec<PathBuf>, String> {
    let file = fs::File::open(zip).map_err(|e| e.to_string())?;
    let mut archive = zip::ZipArchive::new(std::io::BufReader::new(file)).map_err(bad_zip)?;
    let mut files: Vec<PathBuf> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut unpacked = 0u64;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(bad_zip)?;
        let name = entry.name().map_err(bad_zip)?.into_owned();
        let rel = safe_relative(&name)
            .filter(|_| entry.enclosed_name().is_some())
            .ok_or_else(|| tr!("o zip tem um caminho inseguro: {name:?}", "the zip has an unsafe path: {name:?}"))?;
        if entry.is_dir() {
            continue;
        }
        if !entry.is_file() || entry.is_symlink() {
            return Err(tr!("o zip tem um link ou arquivo especial: {name:?}", "the zip has a link or special file: {name:?}"));
        }
        if is_protected(&rel) {
            continue;
        }
        if !seen.insert(rel.to_string_lossy().to_ascii_lowercase()) {
            return Err(tr!("o zip tem {name:?} duas vezes", "the zip has {name:?} twice"));
        }
        let size = entry.size();
        unpacked += size;
        if unpacked > MAX_UNPACKED {
            return Err(tr!("o zip descompacta para mais do que o esperado", "the zip unpacks to more than expected"));
        }
        let dest = into.join(&rel);
        if let Some(p) = dest.parent() {
            fs::create_dir_all(p).map_err(unpack_failed)?;
        }
        let mut out = fs::File::create(&dest).map_err(unpack_failed)?;
        let written = std::io::copy(&mut (&mut entry).take(size + 1), &mut out)
            .map_err(|e| tr!("não consegui descompactar {name}: {e}", "could not unpack {name}: {e}"))?;
        if written != size {
            return Err(tr!("não consegui descompactar {name}: tamanho diferente", "could not unpack {name}: wrong size"));
        }
        files.push(rel);
    }
    if files.is_empty() {
        return Err(tr!("o zip está vazio", "the zip is empty"));
    }
    Ok(files)
}

/// The signed checksum of `zip_name` in a release, after checking the signature over the exact bytes.
pub fn verified_sum(sums: &[u8], sig: &str, zip_name: &str) -> Result<String, String> {
    gw2enhanced_core::signed::verify(sums, sig).map_err(|e| tr!("assinatura inválida: {e}", "invalid signature: {e}"))?;
    let text = std::str::from_utf8(sums).map_err(|_| tr!("{SUMS_ASSET} não é texto", "{SUMS_ASSET} is not text"))?;
    expected_sum(text, zip_name).ok_or_else(|| tr!("{zip_name} não está no {SUMS_ASSET}", "{zip_name} is not in {SUMS_ASSET}"))
}

/// Downloads, verifies and unpacks `package` of `release` into `stage` (emptied first); returns the folder with
/// the package's files, ready for [`crate::setup::install`].
pub fn fetch_package(release: &Release, package: &str, stage: &Path, progress: Progress) -> Result<PathBuf, String> {
    if !PACKAGES.contains(&package) {
        return Err(tr!("pacote desconhecido: {package}", "unknown package: {package}"));
    }
    let v = &release.version;
    let zip_name = asset_name(package, v);
    let asset = release
        .asset(&zip_name)
        .ok_or_else(|| tr!("a versão {v} não tem o pacote {package}", "version {v} has no {package} package"))?;
    let sums = release.asset(SUMS_ASSET).ok_or_else(|| {
        tr!(
            "a versão {v} não tem {SUMS_ASSET}: download sem verificação recusado",
            "version {v} has no {SUMS_ASSET}: unverified download refused"
        )
    })?;
    let sig = release.asset(SIG_ASSET).ok_or_else(|| {
        tr!(
            "a versão {v} não tem {SIG_ASSET}: download sem assinatura recusado",
            "version {v} has no {SIG_ASSET}: unsigned download refused"
        )
    })?;
    for a in [asset, sums, sig] {
        if !asset_url_ok(&a.url) {
            return Err(tr!(
                "{} não vem das versões deste repositório: recusado",
                "{} does not come from this repository's releases: refused",
                a.name
            ));
        }
    }
    let _ = fs::remove_dir_all(stage);
    fs::create_dir_all(stage).map_err(|e| tr!("não consegui gravar na pasta do jogo: {e}", "could not write to the game folder: {e}"))?;

    progress("verify", 0, 0);
    let sums_bytes = read_small(sums, 1024 * 1024)?;
    let sig_text = String::from_utf8(read_small(sig, 4096)?).map_err(|_| tr!("{SIG_ASSET} não é texto", "{SIG_ASSET} is not text"))?;
    let expected = verified_sum(&sums_bytes, &sig_text, &zip_name)?;

    let zip_path = stage.join(&zip_name);
    let actual = download_to(asset, &zip_path, progress)?;
    if actual != expected {
        return Err(tr!(
            "o arquivo baixado não confere com a soma assinada: recusado",
            "the downloaded file does not match the signed checksum: refused"
        ));
    }
    progress("unpack", 0, 0);
    let files_dir = stage.join("files");
    unpack(&zip_path, &files_dir)?;
    let _ = fs::remove_file(&zip_path);
    Ok(files_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r###"[
      {"tag_name":"v1.3.0-beta.1","name":"","draft":false,"prerelease":true,"published_at":"2026-11-10T12:00:00Z",
       "html_url":"https://github.com/wilssola/GW2Enhanced/releases/tag/v1.3.0-beta.1","body":"beta","assets":[]},
      {"tag_name":"v1.2.0","name":"GW2 Enhanced v1.2.0","draft":false,"prerelease":false,
       "published_at":"2026-11-01T12:00:00Z","html_url":"https://github.com/wilssola/GW2Enhanced/releases/tag/v1.2.0",
       "body":"## Novidades\n\n- **HDR** melhor <script>alert(1)</script>\n- [link](javascript:alert(1)) e [ok](https://example.com)\n",
       "assets":[
         {"name":"GW2Enhanced-All-1.2.0.zip","url":"https://api.github.com/repos/wilssola/GW2Enhanced/releases/assets/11","size":42},
         {"name":"GW2Enhanced-Graphics-1.2.0.zip","url":"https://api.github.com/repos/wilssola/GW2Enhanced/releases/assets/12","size":7},
         {"name":"SHA256SUMS.txt","url":"https://api.github.com/repos/wilssola/GW2Enhanced/releases/assets/13","size":1},
         {"name":"SHA256SUMS.txt.sig","url":"https://api.github.com/repos/wilssola/GW2Enhanced/releases/assets/14","size":1}]},
      {"tag_name":"v1.1.0","draft":true,"assets":[]},
      {"tag_name":"v1.0.0","name":"Old","prerelease":false,"html_url":"https://evil.example/x","body":"","assets":[]}
    ]"###;

    #[test]
    fn releases_are_parsed_with_packages_and_safe_notes() {
        let r = parse_releases(SAMPLE).unwrap();
        assert_eq!(r.len(), 3, "the draft is left out");
        assert_eq!(r[0].title, "GW2 Enhanced v1.3.0-beta.1");
        let stable = &r[1];
        assert_eq!(stable.version, "1.2.0");
        assert_eq!(stable.packages, vec!["All".to_string(), "Graphics".to_string()]);
        assert!(stable.signed);
        assert_eq!(stable.package_size("All"), Some(42));
        assert_eq!(stable.package_sizes.get("Graphics"), Some(&7));
        assert!(stable.notes_html.contains("<h2>Novidades</h2>"));
        assert!(stable.notes_html.contains("<strong>HDR</strong>"));
        assert!(!stable.notes_html.contains("<script"), "{}", stable.notes_html);
        assert!(!stable.notes_html.contains("javascript:"), "{}", stable.notes_html);
        assert!(stable.notes_html.contains(r#"href="https://example.com""#));
        assert_eq!(r[2].page, "", "a page outside the repository is not linked");
        assert!(!r[2].signed);
        assert_eq!(latest_stable(&r).unwrap().version, "1.2.0");
        assert!(parse_releases("{}").is_err());
    }

    #[test]
    fn versions_use_semver_ordering() {
        for tag in ["1.2.4", "v1.3.0", "v1.10.0", "2.0.0"] {
            assert!(is_newer("1.2.3", tag), "{tag}");
        }
        for tag in ["1.2.3", "v1.2.3", "1.2.2", "v1.1.10", "1.2.3-beta.1", "1.2.3+build.9", "", "latest", "1.2"] {
            assert!(!is_newer("1.2.3", tag), "{tag}");
        }
        assert!(is_newer("1.2.3-beta.1", "v1.2.3"));
        assert!(!is_newer("desconhecida", "1.0.0"));
    }

    #[test]
    fn checksums_are_trusted_only_with_a_valid_signature() {
        let sums = format!("{}  GW2Enhanced-All-1.2.0.zip\n", "a".repeat(64));
        // Not signed by the built-in key.
        let seed = "AQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyA=";
        let sig = gw2enhanced_core::signed::sign(sums.as_bytes(), seed).unwrap();
        assert!(verified_sum(sums.as_bytes(), &sig, "GW2Enhanced-All-1.2.0.zip").unwrap_err().contains("assinatura"));
        assert!(verified_sum(sums.as_bytes(), "", "GW2Enhanced-All-1.2.0.zip").is_err());
    }

    #[test]
    fn downloads_are_refused_without_signed_sums_or_from_other_urls() {
        let mut r = parse_releases(SAMPLE).unwrap().remove(1);
        let stage = tempfile::tempdir().unwrap();
        let mut nop = |_: &str, _: u64, _: u64| {};
        assert!(fetch_package(&r, "Textures", stage.path(), &mut nop).unwrap_err().contains("não tem o pacote"));
        assert!(fetch_package(&r, "Evil", stage.path(), &mut nop).unwrap_err().contains("desconhecido"));
        r.assets[0].url = "https://evil.example/repos/wilssola/GW2Enhanced/releases/assets/11".into();
        assert!(fetch_package(&r, "All", stage.path(), &mut nop).unwrap_err().contains("recusado"));
        r.assets.retain(|a| a.name != SIG_ASSET);
        assert!(fetch_package(&r, "Graphics", stage.path(), &mut nop).unwrap_err().contains("sem assinatura"));
    }

    #[test]
    fn unpack_refuses_unsafe_zips_and_skips_protected_files() {
        use zip::write::SimpleFileOptions;
        let dir = tempfile::tempdir().unwrap();
        let make = |name: &str, entries: &[(&str, &[u8])]| {
            let p = dir.path().join(name);
            let mut w = zip::ZipWriter::new(fs::File::create(&p).unwrap());
            let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
            for (n, d) in entries {
                w.start_file(*n, opts).unwrap();
                w.write_all(d).unwrap();
            }
            w.finish().unwrap();
            p
        };
        let good = make(
            "good.zip",
            &[
                ("msimg32.dll", b"dll"),
                ("GW2Enhanced-Installer.exe", b"exe"),
                ("GW2Translated/config.ini", b"user"),
                ("GW2Translated/translator/t.exe", b"exe"),
            ],
        );
        let out = dir.path().join("out");
        let files = unpack(&good, &out).unwrap();
        assert_eq!(files.len(), 3);
        assert!(!out.join("GW2Translated/config.ini").exists());
        assert!(out.join("GW2Translated/translator/t.exe").is_file());
        let evil = make("evil.zip", &[("ok.txt", b"x"), ("../evil.dll", b"x")]);
        assert!(unpack(&evil, &dir.path().join("out2")).unwrap_err().contains("inseguro"));
        assert!(!dir.path().join("evil.dll").exists());
    }
}
