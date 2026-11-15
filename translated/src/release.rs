//! Dictionary releases from the project's server (`backend/docs/api.md`, "Dictionary downloads").
//!
//! The server builds a release every day from the reviewed translations: a manifest signed with ed25519 and the
//! dictionary as gzip files, each listed in the manifest with its SHA-256 and size. In the background (never on a
//! game thread), at most once every [`CHECK_EVERY`] and only while `update_dictionary` is on:
//!
//! 1. `GET /v1/releases/{lang}/latest`; the signature is checked against [`PUBLIC_KEY`], built into the DLL. An
//!    unsigned, tampered or foreign manifest is ignored.
//! 2. Same version as the one in use: nothing to do. Any other version (newer, or older after the server rolled a
//!    bad release back; both signed) is downloaded. Files already on disk with the same SHA-256 are copied instead.
//! 3. Each file is checked (size, SHA-256, gunzipped size, records) and written into `v<N>.part\`. Only when every
//!    file passed is the folder renamed to `v<N>\` and `current.txt` switched to it, so a failure at any point
//!    leaves the version in use untouched.
//! 4. The new dictionary is built on this thread and swapped in ([`crate::dict::Shared`]), then the text on screen
//!    is refreshed.
//!
//! ```text
//! languages\<language>\release\
//! ├─ current.txt          version in use
//! ├─ state.json           time of the last check
//! ├─ v<N>\                manifest.json (as received, signature included), dictionary-NNN.jsonl.gz
//! └─ v<M>\                the version before it, kept for rollback
//! ```
//!
//! At startup the version in `current.txt` is checked again (signature and every SHA-256); if it fails, the newest
//! other version that passes is used and becomes current (rollback), and with none the shipped dictionary is used
//! alone. Offline, nothing changes and the check is retried a little later.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::records::{self, Translation};
use crate::runtime::{self, Flag};
use crate::{log, net};

/// Public half of the server's `RELEASE_SIGNING_KEY` (base64 of the raw 32 bytes, as `GET /v1/public-key` shows
/// it). Rotating the key means a new DLL; releases signed with another key are refused.
pub const PUBLIC_KEY: &str = "aOr/xdAzXI67aSac2o0YefbhMcl+vprKe2Z/+jBqfP8=";

/// Least time between two checks (seconds); the server builds one release a day.
pub const CHECK_EVERY: u64 = 6 * 3600;
/// After the server could not be reached.
const RETRY_OFFLINE: u64 = 15 * 60;
/// The game loads first.
const FIRST_DELAY: Duration = Duration::from_secs(30);
/// How often the thread looks at the setting and the clock.
const POLL: Duration = Duration::from_secs(60);

/// Limits: a release is about 15 files of under 1 MB each, 2 MiB of JSON Lines inside each.
const MAX_MANIFEST: usize = 64 * 1024;
const MAX_FILES: usize = 64;
const MAX_FILE: u64 = 8 * 1024 * 1024;
const MAX_TOTAL: u64 = 64 * 1024 * 1024;
const MAX_RAW: usize = 16 * 1024 * 1024;

const FILE_PREFIX: &str = "dictionary-";
const FILE_SUFFIX: &str = ".jsonl.gz";

/// Version in use (0 = none, the shipped dictionary alone), for the overlay.
static IN_USE: AtomicU64 = AtomicU64::new(0);

pub fn in_use() -> Option<u64> {
    Some(IN_USE.load(Ordering::Relaxed)).filter(|&v| v > 0)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    pub name: String,
    /// Lowercase hex SHA-256 of the `.gz` as served.
    pub sha256: String,
    pub size: u64,
}

/// `GET /v1/releases/{lang}/latest`, after its signature was checked.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Manifest {
    pub lang: String,
    pub version: u64,
    #[serde(default)]
    pub created_at: String,
    pub files: Vec<FileEntry>,
}

/// JSON with object keys sorted by their bytes at every level and no whitespace: the bytes the server signs
/// (`backend/src/manifest.rs`). Written by hand so it does not depend on how `serde_json` orders its maps.
pub fn canonical_json(v: &Value) -> String {
    let mut out = String::new();
    write_canonical(v, &mut out);
    out
}

fn write_canonical(v: &Value, out: &mut String) {
    match v {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
            out.push('{');
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(k).unwrap_or_default());
                out.push(':');
                write_canonical(&map[k.as_str()], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        other => out.push_str(&serde_json::to_string(other).unwrap_or_default()),
    }
}

/// `dictionary-NNN.jsonl.gz`: the only names a release may list (never a path).
fn valid_file_name(name: &str) -> bool {
    name.strip_prefix(FILE_PREFIX)
        .and_then(|r| r.strip_suffix(FILE_SUFFIX))
        .is_some_and(|n| n.len() == 3 && n.bytes().all(|b| b.is_ascii_digit()))
}

fn valid_sha256(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Checks the signature of a manifest body (as received) and what it lists.
pub fn verify_manifest(body: &[u8], lang: &str, public_key: &str) -> Result<Manifest, String> {
    if body.len() > MAX_MANIFEST {
        return Err("manifest too large".into());
    }
    let mut v: Value = serde_json::from_slice(body).map_err(|e| format!("manifest is not JSON: {e}"))?;
    let signature = v
        .as_object_mut()
        .and_then(|m| m.remove("signature"))
        .and_then(|s| s.as_str().map(str::to_owned))
        .ok_or("manifest is not signed")?;
    gw2enhanced_core::signed::verify_with(canonical_json(&v).as_bytes(), &signature, public_key)
        .map_err(|_| "manifest signature does not match".to_string())?;
    let m: Manifest = serde_json::from_value(v).map_err(|e| format!("manifest: {e}"))?;
    if m.lang != lang {
        return Err(format!("manifest is for {}, not {lang}", m.lang));
    }
    if m.version == 0 {
        return Err("manifest version 0".into());
    }
    if m.files.is_empty() || m.files.len() > MAX_FILES {
        return Err(format!("manifest lists {} files", m.files.len()));
    }
    let mut names = HashSet::new();
    let mut total = 0u64;
    for f in &m.files {
        if !valid_file_name(&f.name) || !names.insert(f.name.as_str()) {
            return Err(format!("bad file name {:?}", f.name));
        }
        if !valid_sha256(&f.sha256) {
            return Err(format!("{}: bad sha256", f.name));
        }
        if f.size == 0 || f.size > MAX_FILE {
            return Err(format!("{}: size {} out of bounds", f.name, f.size));
        }
        total += f.size;
    }
    if total > MAX_TOTAL {
        return Err(format!("release of {total} bytes is too large"));
    }
    Ok(m)
}

/// Whether the server's version must be downloaded. Any difference counts: an older version means the server
/// rolled a bad release back, and it is signed like any other.
pub fn needs_update(in_use: Option<u64>, latest: u64) -> bool {
    in_use != Some(latest)
}

/// Whether a check is due, `CHECK_EVERY` after the last one. A last check in the future (clock moved back) is due.
pub fn due(last_check: Option<u64>, now: u64) -> bool {
    last_check.is_none_or(|last| last > now || now - last >= CHECK_EVERY)
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// The bytes are the file the manifest lists.
fn check_file(entry: &FileEntry, bytes: &[u8]) -> Result<(), String> {
    if bytes.len() as u64 != entry.size {
        return Err(format!("{}: {} bytes, expected {}", entry.name, bytes.len(), entry.size));
    }
    if sha256_hex(bytes) != entry.sha256 {
        return Err(format!("{}: sha256 does not match", entry.name));
    }
    Ok(())
}

/// Gunzips (at most `limit` bytes) and reads the records of one file.
fn decode(bytes: &[u8], limit: usize) -> Result<Vec<Translation>, String> {
    let mut raw = Vec::new();
    flate2::read::GzDecoder::new(bytes)
        .take(limit as u64 + 1)
        .read_to_end(&mut raw)
        .map_err(|e| format!("gzip: {e}"))?;
    if raw.len() > limit {
        return Err(format!("more than {limit} bytes once gunzipped"));
    }
    let text = String::from_utf8(raw).map_err(|_| "not UTF-8".to_string())?;
    Ok(records::parse::<Translation>(&text).records)
}

/// Where the server is reached (injected in tests). `path` is the request path; a body over `max` bytes is an
/// error. `Err` means the server could not be reached.
pub trait Fetch {
    fn get(&self, path: &str, max: usize) -> Result<(u32, Vec<u8>), String>;
}

impl<F: Fn(&str, usize) -> Result<(u32, Vec<u8>), String>> Fetch for F {
    fn get(&self, path: &str, max: usize) -> Result<(u32, Vec<u8>), String> {
        self(path, max)
    }
}

/// The project's server, over HTTPS (`net`).
struct Server;

impl Fetch for Server {
    fn get(&self, path: &str, max: usize) -> Result<(u32, Vec<u8>), String> {
        net::get(net::HOST, path, max).map_err(|e| e.to_string())
    }
}

#[derive(Debug)]
pub enum Error {
    /// The server could not be reached: try again a little later.
    Offline(String),
    /// Bad answer, bad file, disk error: wait for the next regular check.
    Failed(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Offline(e) => write!(f, "server unreachable ({e})"),
            Error::Failed(e) => f.write_str(e),
        }
    }
}

fn failed(e: impl std::fmt::Display) -> Error {
    Error::Failed(e.to_string())
}

#[derive(Debug)]
pub enum Outcome {
    /// The server has no release for this language yet.
    NoRelease,
    UpToDate(u64),
    Installed {
        version: u64,
        records: Vec<Translation>,
        /// Bytes downloaded, and files copied from a version already on disk.
        downloaded: u64,
        reused: usize,
    },
}

/// A release read back from disk.
#[derive(Debug)]
pub struct Loaded {
    pub version: u64,
    pub records: Vec<Translation>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    /// Unix time (seconds) the server last answered a check.
    last_check: Option<u64>,
}

/// The `release\` folder of one language.
pub struct Store {
    dir: PathBuf,
    lang: String,
    public_key: String,
}

impl Store {
    pub fn new(dir: PathBuf, lang: &str, public_key: &str) -> Self {
        Self {
            dir,
            lang: lang.to_owned(),
            public_key: public_key.to_owned(),
        }
    }

    fn version_dir(&self, version: u64) -> PathBuf {
        self.dir.join(format!("v{version}"))
    }

    fn current_file(&self) -> PathBuf {
        self.dir.join("current.txt")
    }

    fn state_file(&self) -> PathBuf {
        self.dir.join("state.json")
    }

    /// Version named in `current.txt`.
    pub fn current(&self) -> Option<u64> {
        fs::read_to_string(self.current_file()).ok()?.trim().parse().ok()
    }

    /// Rewrites `current.txt` through a temporary file (a crash leaves the old one or the new one).
    fn set_current(&self, version: u64) -> io::Result<()> {
        let tmp = self.dir.join("current.txt.tmp");
        fs::write(&tmp, format!("{version}\n"))?;
        fs::rename(&tmp, self.current_file())
    }

    /// Versions on disk (`v<N>` folders), newest first.
    fn versions(&self) -> Vec<u64> {
        let mut out: Vec<u64> = fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().is_dir())
            .filter_map(|e| e.file_name().to_str()?.strip_prefix('v')?.parse().ok())
            .collect();
        out.sort_unstable_by(|a, b| b.cmp(a));
        out
    }

    /// Reads and checks one version on disk: signature, every file's size and SHA-256, records.
    fn read_version(&self, version: u64) -> Result<Vec<Translation>, String> {
        let dir = self.version_dir(version);
        let body = fs::read(dir.join("manifest.json")).map_err(|e| format!("manifest.json: {e}"))?;
        let m = verify_manifest(&body, &self.lang, &self.public_key)?;
        if m.version != version {
            return Err(format!("folder v{version} holds version {}", m.version));
        }
        let mut records = Vec::new();
        for f in &m.files {
            let path = dir.join(&f.name);
            if fs::metadata(&path).map_err(|e| format!("{}: {e}", f.name))?.len() != f.size {
                return Err(format!("{}: wrong size", f.name));
            }
            let bytes = fs::read(&path).map_err(|e| format!("{}: {e}", f.name))?;
            check_file(f, &bytes)?;
            records.extend(decode(&bytes, MAX_RAW).map_err(|e| format!("{}: {e}", f.name))?);
        }
        Ok(records)
    }

    /// The version in use, checked again; if it fails, the newest other version that passes (which becomes
    /// current). `None`: use the shipped dictionary alone.
    pub fn load(&self) -> Option<Loaded> {
        let current = self.current();
        let candidates = current
            .into_iter()
            .chain(self.versions().into_iter().filter(|v| Some(*v) != current));
        for version in candidates {
            match self.read_version(version) {
                Ok(records) => {
                    if current != Some(version) {
                        log!("dictionary release: rolling back to version {version}");
                        if let Err(e) = self.set_current(version) {
                            log!("dictionary release: could not save current.txt: {e}");
                        }
                    }
                    return Some(Loaded { version, records });
                }
                Err(e) => log!("dictionary release: version {version} on disk is unusable: {e}"),
            }
        }
        None
    }

    /// Files of the versions on disk, by SHA-256 (as their manifest lists them; the bytes are checked again
    /// before use).
    fn files_on_disk(&self) -> HashMap<String, PathBuf> {
        let mut out = HashMap::new();
        for version in self.versions() {
            let dir = self.version_dir(version);
            let Some(v) = fs::read(dir.join("manifest.json"))
                .ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            else {
                continue;
            };
            let files: Vec<FileEntry> = v
                .get("files")
                .and_then(|f| serde_json::from_value(f.clone()).ok())
                .unwrap_or_default();
            for f in files.into_iter().filter(|f| valid_file_name(&f.name)) {
                out.entry(f.sha256).or_insert_with(|| dir.join(f.name));
            }
        }
        out
    }

    /// Asks the server for its latest release and installs it when it differs from `in_use`.
    pub fn update(&self, fetch: &dyn Fetch, in_use: Option<u64>) -> Result<Outcome, Error> {
        let (status, body) = fetch
            .get(&format!("/v1/releases/{}/latest", self.lang), MAX_MANIFEST)
            .map_err(Error::Offline)?;
        match status {
            200 => {}
            404 => return Ok(Outcome::NoRelease),
            s => return Err(failed(format!("server answered {s}"))),
        }
        let m = verify_manifest(&body, &self.lang, &self.public_key).map_err(failed)?;
        if !needs_update(in_use, m.version) {
            return Ok(Outcome::UpToDate(m.version));
        }
        fs::create_dir_all(&self.dir).map_err(failed)?;
        let part = self.dir.join(format!("v{}.part", m.version));
        let _ = fs::remove_dir_all(&part);
        let fetched = self.fetch_into(fetch, &m, &body, &part);
        let (records, downloaded, reused) = match fetched {
            Ok(r) => r,
            Err(e) => {
                let _ = fs::remove_dir_all(&part);
                return Err(e);
            }
        };
        let target = self.version_dir(m.version);
        let _ = fs::remove_dir_all(&target);
        if let Err(e) = fs::rename(&part, &target).and_then(|()| self.set_current(m.version)) {
            let _ = fs::remove_dir_all(&part);
            return Err(failed(format!("could not install version {}: {e}", m.version)));
        }
        self.prune(&[Some(m.version), in_use]);
        Ok(Outcome::Installed {
            version: m.version,
            records,
            downloaded,
            reused,
        })
    }

    /// Fills `part` with every file of `m`, each checked; returns the records, bytes downloaded and files reused.
    fn fetch_into(
        &self,
        fetch: &dyn Fetch,
        m: &Manifest,
        body: &[u8],
        part: &Path,
    ) -> Result<(Vec<Translation>, u64, usize), Error> {
        fs::create_dir_all(part).map_err(failed)?;
        let on_disk = self.files_on_disk();
        let (mut records, mut downloaded, mut reused) = (Vec::new(), 0u64, 0usize);
        for f in &m.files {
            let local = on_disk
                .get(&f.sha256)
                .and_then(|p| fs::read(p).ok())
                .filter(|b| check_file(f, b).is_ok());
            let bytes = match local {
                Some(b) => {
                    reused += 1;
                    b
                }
                None => {
                    let path = format!("/v1/releases/{}/{}/{}", self.lang, m.version, f.name);
                    let (status, b) = fetch.get(&path, f.size as usize).map_err(Error::Offline)?;
                    if status != 200 {
                        return Err(failed(format!("{}: server answered {status}", f.name)));
                    }
                    check_file(f, &b).map_err(failed)?;
                    downloaded += b.len() as u64;
                    b
                }
            };
            records.extend(decode(&bytes, MAX_RAW).map_err(|e| failed(format!("{}: {e}", f.name)))?);
            fs::write(part.join(&f.name), &bytes).map_err(failed)?;
        }
        fs::write(part.join("manifest.json"), body).map_err(failed)?;
        Ok((records, downloaded, reused))
    }

    /// Deletes every version folder (and leftover `.part`) except `keep`.
    fn prune(&self, keep: &[Option<u64>]) {
        for e in fs::read_dir(&self.dir).into_iter().flatten().flatten() {
            let name = e.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(rest) = name.strip_prefix('v') else { continue };
            let version = rest.strip_suffix(".part").unwrap_or(rest);
            let Ok(version) = version.parse::<u64>() else { continue };
            if rest.ends_with(".part") || !keep.contains(&Some(version)) {
                let _ = fs::remove_dir_all(e.path());
            }
        }
    }

    fn last_check(&self) -> Option<u64> {
        fs::read_to_string(self.state_file())
            .ok()
            .and_then(|s| serde_json::from_str::<State>(&s).ok())
            .and_then(|s| s.last_check)
    }

    fn save_last_check(&self, now: u64) {
        let state = State { last_check: Some(now) };
        let tmp = self.dir.join("state.json.tmp");
        if fs::create_dir_all(&self.dir).is_ok()
            && let Ok(json) = serde_json::to_string(&state)
            && fs::write(&tmp, json).is_ok()
        {
            let _ = fs::rename(&tmp, self.state_file());
        }
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

pub struct Setup {
    pub store: Store,
    /// Version loaded at startup.
    pub in_use: Option<u64>,
    /// Puts a newly installed release in use (builds the merged dictionary and swaps it in).
    pub apply: Box<dyn Fn(u64, Vec<Translation>) + Send>,
}

/// Records the version loaded at startup (for the overlay).
pub fn set_in_use(version: Option<u64>) {
    IN_USE.store(version.unwrap_or(0), Ordering::Relaxed);
}

fn run(setup: Setup) {
    std::thread::sleep(FIRST_DELAY);
    let mut in_use = setup.in_use;
    let mut retry_at = 0u64;
    let mut told_off = false;
    loop {
        let now = now();
        if !runtime::get(Flag::UpdateDictionary) {
            if !told_off {
                log!("dictionary release: updates are off (update_dictionary=false)");
                told_off = true;
            }
        } else if now >= retry_at && due(setup.store.last_check(), now) {
            told_off = false;
            match setup.store.update(&Server, in_use) {
                Ok(Outcome::NoRelease) => log!("dictionary release: the server has none yet"),
                Ok(Outcome::UpToDate(v)) => log!("dictionary release: version {v} is the latest"),
                Ok(Outcome::Installed {
                    version,
                    records,
                    downloaded,
                    reused,
                }) => {
                    log!(
                        "dictionary release: version {version} installed ({} texts, {} KB downloaded, {reused} files reused)",
                        records.len(),
                        downloaded / 1024
                    );
                    in_use = Some(version);
                    (setup.apply)(version, records);
                    IN_USE.store(version, Ordering::Relaxed);
                }
                Err(e @ Error::Offline(_)) => {
                    log!("dictionary release: {e}; trying again later");
                    retry_at = now + RETRY_OFFLINE;
                    std::thread::sleep(POLL);
                    continue;
                }
                Err(e) => log!(
                    "dictionary release: {e}; keeping {}",
                    in_use.map_or("the shipped dictionary".into(), |v| format!("version {v}"))
                ),
            }
            setup.store.save_last_check(now);
        }
        std::thread::sleep(POLL);
    }
}

pub fn start(setup: Setup) {
    let _ = std::thread::Builder::new()
        .name("gw2translated-release".into())
        .spawn(move || run(setup));
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::io::Write as _;

    use super::*;

    const SEED: &str = "AQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyA="; // 1..=32
    const OTHER_SEED: &str = "ICEiIyQlJicoKSorLC0uLzAxMjM0NTY3ODk6Ozw9Pj8=";

    fn public() -> String {
        gw2enhanced_core::signed::public_of(SEED).unwrap()
    }

    fn gz(text: &str) -> Vec<u8> {
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(text.as_bytes()).unwrap();
        e.finish().unwrap()
    }

    fn lines(pairs: &[(&str, &str)]) -> String {
        pairs
            .iter()
            .map(|(en, tr)| format!("{{\"en\":\"{en}\",\"tr\":\"{tr}\",\"src\":\"nemotron\"}}\n"))
            .collect()
    }

    /// Signs a manifest the way the server does (pretty-printed, so the check cannot rely on the layout).
    fn signed(lang: &str, version: u64, files: &[(String, Vec<u8>)], seed: &str) -> Vec<u8> {
        let files: Vec<Value> = files
            .iter()
            .map(|(name, b)| serde_json::json!({"name": name, "sha256": sha256_hex(b), "size": b.len()}))
            .collect();
        let mut v = serde_json::json!({"lang": lang, "version": version, "created_at": "2026-11-15T03:00:00Z", "files": files});
        let sig = gw2enhanced_core::signed::sign(canonical_json(&v).as_bytes(), seed).unwrap();
        v["signature"] = Value::String(sig);
        serde_json::to_vec_pretty(&v).unwrap()
    }

    /// A fake server: the manifest plus its files, counting the requests.
    struct Fake {
        manifest: Vec<u8>,
        version: u64,
        files: Vec<(String, Vec<u8>)>,
        calls: RefCell<Vec<String>>,
        offline: bool,
    }

    impl Fake {
        fn new(version: u64, files: Vec<(String, Vec<u8>)>) -> Self {
            Self {
                manifest: signed("pt-BR", version, &files, SEED),
                version,
                files,
                calls: RefCell::new(Vec::new()),
                offline: false,
            }
        }
    }

    impl Fetch for Fake {
        fn get(&self, path: &str, max: usize) -> Result<(u32, Vec<u8>), String> {
            self.calls.borrow_mut().push(path.to_owned());
            if self.offline {
                return Err("no network".into());
            }
            let body = if path == "/v1/releases/pt-BR/latest" {
                Some(self.manifest.clone())
            } else {
                self.files
                    .iter()
                    .find(|(n, _)| path == format!("/v1/releases/pt-BR/{}/{n}", self.version))
                    .map(|(_, b)| b.clone())
            };
            match body {
                Some(b) if b.len() > max => Err("too large".into()),
                Some(b) => Ok((200, b)),
                None => Ok((404, br#"{"error":"not_found"}"#.to_vec())),
            }
        }
    }

    fn file(n: u32, pairs: &[(&str, &str)]) -> (String, Vec<u8>) {
        (format!("dictionary-{n:03}.jsonl.gz"), gz(&lines(pairs)))
    }

    fn store(dir: &Path) -> Store {
        Store::new(dir.join("release"), "pt-BR", &public())
    }

    fn texts(records: &[Translation]) -> Vec<(&str, &str)> {
        records.iter().map(|t| (t.en.as_str(), t.tr.as_str())).collect()
    }

    #[test]
    fn canonical_form_is_the_one_the_server_signs() {
        // backend/src/manifest.rs, `exact_signing_input`.
        let v: Value = serde_json::from_str(&format!(
            r#"{{"version":42,"lang":"pt-BR","files":[{{"size":123456,"sha256":"{}","name":"strings-000.jsonl.gz"}}],"created_at":"2026-10-31T19:00:00Z"}}"#,
            "ab".repeat(32)
        ))
        .unwrap();
        assert_eq!(
            canonical_json(&v),
            format!(
                r#"{{"created_at":"2026-10-31T19:00:00Z","files":[{{"name":"strings-000.jsonl.gz","sha256":"{}","size":123456}}],"lang":"pt-BR","version":42}}"#,
                "ab".repeat(32)
            )
        );
        let v: Value = serde_json::from_str(r#"{ "b": 1, "a": {"y": "é\n", "x": [true, null]} }"#).unwrap();
        assert_eq!(canonical_json(&v), r#"{"a":{"x":[true,null],"y":"é\n"},"b":1}"#);
    }

    #[test]
    fn signature_good_and_bad() {
        let files = vec![file(0, &[("Sword", "Espada")])];
        let body = signed("pt-BR", 3, &files, SEED);
        let m = verify_manifest(&body, "pt-BR", &public()).unwrap();
        assert_eq!((m.version, m.files.len()), (3, 1));

        let text = String::from_utf8(body.clone()).unwrap();
        let tampered = text.replace("\"version\": 3", "\"version\": 4");
        assert_ne!(tampered, text);
        assert!(verify_manifest(tampered.as_bytes(), "pt-BR", &public()).is_err());
        let other_key = signed("pt-BR", 3, &files, OTHER_SEED);
        assert!(verify_manifest(&other_key, "pt-BR", &public()).is_err());
        let mut unsigned: Value = serde_json::from_slice(&body).unwrap();
        unsigned.as_object_mut().unwrap().remove("signature");
        let unsigned = serde_json::to_vec(&unsigned).unwrap();
        assert_eq!(
            verify_manifest(&unsigned, "pt-BR", &public()).unwrap_err(),
            "manifest is not signed"
        );
        assert!(verify_manifest(&body, "es", &public()).unwrap_err().contains("not es"));
        assert!(verify_manifest(b"<html>captive portal</html>", "pt-BR", &public()).is_err());
    }

    #[test]
    fn manifest_cannot_name_paths_or_huge_files() {
        let bad_name = vec![("../dictionary-000.jsonl.gz".to_string(), gz("x"))];
        assert!(
            verify_manifest(&signed("pt-BR", 1, &bad_name, SEED), "pt-BR", &public())
                .unwrap_err()
                .contains("bad file name")
        );
        for name in ["dictionary-0001.jsonl.gz", "dictionary-abc.jsonl.gz", "x.dll"] {
            assert!(!valid_file_name(name), "{name}");
        }
        assert!(valid_file_name("dictionary-042.jsonl.gz"));
        let huge = vec![("dictionary-000.jsonl.gz".to_string(), vec![0u8; (MAX_FILE + 1) as usize])];
        assert!(
            verify_manifest(&signed("pt-BR", 1, &huge, SEED), "pt-BR", &public())
                .unwrap_err()
                .contains("out of bounds")
        );
        assert!(verify_manifest(&signed("pt-BR", 1, &[], SEED), "pt-BR", &public()).is_err());
    }

    #[test]
    fn version_compare_and_check_interval() {
        assert!(needs_update(None, 1));
        assert!(needs_update(Some(4), 5));
        assert!(!needs_update(Some(5), 5));
        assert!(needs_update(Some(5), 4), "a signed rollback on the server is followed");
        assert!(due(None, 1000));
        assert!(!due(Some(1000), 1000 + CHECK_EVERY - 1));
        assert!(due(Some(1000), 1000 + CHECK_EVERY));
        assert!(due(Some(5000), 1000), "clock moved back");
    }

    #[test]
    fn gunzip_is_bounded() {
        let big = gz(&"a".repeat(2000));
        assert!(decode(&big, 1000).unwrap_err().contains("more than 1000"));
        assert_eq!(decode(&gz(&lines(&[("A", "B")])), 1000).unwrap().len(), 1);
        assert!(decode(b"not gzip", 1000).is_err());
    }

    #[test]
    fn installs_then_skips_the_same_version() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        let fake = Fake::new(1, vec![file(0, &[("Sword", "Espada")]), file(1, &[("Shield", "Escudo")])]);
        let Outcome::Installed { version, records, .. } = s.update(&fake, None).unwrap() else {
            panic!("not installed")
        };
        assert_eq!(version, 1);
        assert_eq!(texts(&records), [("Sword", "Espada"), ("Shield", "Escudo")]);
        assert_eq!(s.current(), Some(1));
        assert!(matches!(s.update(&fake, Some(1)).unwrap(), Outcome::UpToDate(1)));
        let loaded = s.load().unwrap();
        assert_eq!((loaded.version, loaded.records.len()), (1, 2));
    }

    #[test]
    fn hash_mismatch_keeps_the_version_in_use() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.update(&Fake::new(1, vec![file(0, &[("Sword", "Espada")])]), None)
            .unwrap();
        // The manifest is signed for one file, the server hands out another of the same size.
        let mut bad = Fake::new(2, vec![file(0, &[("Sword", "Espadx")])]);
        bad.files[0].1 = file(0, &[("Sword", "Espady")]).1;
        assert_eq!(bad.files[0].1.len(), file(0, &[("Sword", "Espadx")]).1.len());
        let err = s.update(&bad, Some(1)).unwrap_err().to_string();
        assert!(err.contains("sha256 does not match"), "{err}");
        assert_eq!(s.current(), Some(1));
        assert!(!s.version_dir(2).exists() && !s.dir.join("v2.part").exists());
        assert_eq!(texts(&s.load().unwrap().records), [("Sword", "Espada")]);
    }

    #[test]
    fn offline_and_missing_release() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        let mut fake = Fake::new(1, vec![file(0, &[("Sword", "Espada")])]);
        fake.offline = true;
        assert!(matches!(s.update(&fake, None), Err(Error::Offline(_))));
        let none = |_: &str, _: usize| Ok((404, br#"{"error":"no_release"}"#.to_vec()));
        assert!(matches!(s.update(&none, None).unwrap(), Outcome::NoRelease));
        let broken = |_: &str, _: usize| Ok((502, b"Bad gateway".to_vec()));
        assert!(matches!(s.update(&broken, None), Err(Error::Failed(_))));
        assert!(s.load().is_none());
    }

    #[test]
    fn reuses_unchanged_files_and_prunes_old_versions() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        let a = file(0, &[("Sword", "Espada")]);
        s.update(&Fake::new(1, vec![a.clone()]), None).unwrap();
        let v2 = Fake::new(2, vec![a.clone(), file(1, &[("Shield", "Escudo")])]);
        let Outcome::Installed { reused, .. } = s.update(&v2, Some(1)).unwrap() else { panic!() };
        assert_eq!(reused, 1);
        assert!(!v2.calls.borrow().iter().any(|c| c.ends_with("dictionary-000.jsonl.gz")));
        assert!(s.version_dir(1).exists(), "the previous version is kept for rollback");
        s.update(&Fake::new(3, vec![file(0, &[("Bow", "Arco")])]), Some(2)).unwrap();
        assert_eq!(s.versions(), [3, 2]);
    }

    #[test]
    fn a_damaged_version_on_disk_rolls_back_to_the_previous_one() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.update(&Fake::new(1, vec![file(0, &[("Sword", "Espada")])]), None)
            .unwrap();
        s.update(&Fake::new(2, vec![file(0, &[("Sword", "Espada nova")])]), Some(1))
            .unwrap();
        assert_eq!(s.load().unwrap().version, 2);
        let f = s.version_dir(2).join("dictionary-000.jsonl.gz");
        let mut bytes = fs::read(&f).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        fs::write(&f, bytes).unwrap();
        let loaded = s.load().unwrap();
        assert_eq!(loaded.version, 1);
        assert_eq!(texts(&loaded.records), [("Sword", "Espada")]);
        assert_eq!(s.current(), Some(1));
        // A manifest edited on disk is refused too.
        let m = s.version_dir(1).join("manifest.json");
        let text = fs::read_to_string(&m).unwrap().replace("pt-BR", "pt-PT");
        fs::write(&m, text).unwrap();
        assert!(s.load().is_none());
    }

    #[test]
    fn last_check_is_remembered() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        assert_eq!(s.last_check(), None);
        s.save_last_check(1234);
        assert_eq!(s.last_check(), Some(1234));
    }
}
