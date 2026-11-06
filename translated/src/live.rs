//! Live translation of what is missing from the dictionary, on the player's PC.
//!
//! The hook only enqueues; a dedicated thread starts `gw2translated-translator`
//! (outside the game process, low priority, no window) once the game is in
//! English, sends batches over HTTP on 127.0.0.1, writes the
//! result to `local.jsonl` and requests a refresh for the text on screen.
//!
//! Two priorities: text that is already on screen (urgent) and text the game
//! has just loaded but not shown yet, such as map dialogues
//! (ahead of time). Ahead-of-time work only runs with no urgent items queued and does not
//! request a refresh: the idea is for the translation to exist before the speech bubble
//! or the chat appears, because those keep the text they received and don't ask again.
//!
//! Web page texts (see `web.rs`) have a queue of their own: their translations
//! go to `web_local.jsonl`, which is never uploaded, and back to the page.
//!
//! Only translates with the client in English: the model is English → Portuguese.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

use windows_sys::Win32::System::Threading::{BELOW_NORMAL_PRIORITY_CLASS, CREATE_NO_WINDOW};

use crate::{log, refresh};

/// Texts per request for on-screen texts (they are needed together).
const BATCH: usize = 16;
/// Texts per request for background work: kept small so a text that just
/// showed up on screen never waits long behind it.
const BACKGROUND_BATCH: usize = 4;
/// `src` of the translations made on this PC.
const LOCAL_SOURCE: &str = "argos";
/// Texts longer than this wait for the dictionary (the model's quality drops).
const MAX_CHARS: usize = 2000;

static LIVE: OnceLock<RwLock<HashMap<String, String>>> = OnceLock::new();
/// Bumped every time live translations are added.
static VERSION: AtomicUsize = AtomicUsize::new(0);
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
enum Kind {
    /// Game text on screen.
    Urgent,
    /// Text on a web page.
    Web,
    /// Game text about to be shown: the server just sent its decryption key.
    Soon,
    /// Game text loaded but not displayed yet.
    Ahead,
}

/// Order in which the queues are served.
const PRIORITY: [Kind; 4] = [Kind::Urgent, Kind::Web, Kind::Soon, Kind::Ahead];

static ON_SCREEN: AtomicUsize = AtomicUsize::new(0);
static AHEAD: AtomicUsize = AtomicUsize::new(0);
static QUEUED: AtomicUsize = AtomicUsize::new(0);
static RUNNING: AtomicBool = AtomicBool::new(false);

/// For the overlay: translated on screen, translated ahead of time, queued.
/// `None` when local translation is not running.
pub fn stats() -> Option<(usize, usize, usize)> {
    RUNNING.load(Ordering::Relaxed).then(|| {
        (
            ON_SCREEN.load(Ordering::Relaxed),
            AHEAD.load(Ordering::Relaxed),
            QUEUED.load(Ordering::Relaxed),
        )
    })
}

static QUEUE: OnceLock<Mutex<Sender<(String, Kind)>>> = OnceLock::new();
/// Text IDs seen for texts waiting for a translation (saved with it).
static IDS: Mutex<Option<HashMap<String, Vec<u32>>>> = Mutex::new(None);

/// Changes whenever new live translations arrive.
pub fn version() -> u64 {
    VERSION.load(Ordering::Relaxed) as u64
}

/// Translation made live in this session (earlier ones already come in the dictionary).
pub fn get(english: &str) -> Option<String> {
    LIVE.get()?.read().ok()?.get(english).cloned()
}

fn enqueue(english: &str, kind: Kind, id: Option<u32>) {
    if english.chars().count() > MAX_CHARS || !english.chars().any(char::is_alphabetic) {
        return;
    }
    if let (Some(id), Ok(mut ids)) = (id, IDS.lock()) {
        let list = ids
            .get_or_insert_with(HashMap::new)
            .entry(english.to_owned())
            .or_default();
        if !list.contains(&id) {
            list.push(id);
        }
    }
    if let Some(q) = QUEUE.get()
        && let Ok(q) = q.lock()
    {
        let _ = q.send((english.to_owned(), kind));
    }
}

/// On-screen text without a translation: goes to the front of the queue.
pub fn request(english: &str, id: Option<u32>) {
    enqueue(english, Kind::Urgent, id);
}

/// Text loaded but not displayed yet: translated when there are no urgent items.
pub fn prefetch(english: &str, id: Option<u32>) {
    enqueue(english, Kind::Ahead, id);
}

/// Text shown on a web page without a translation.
pub fn request_web(english: &str) {
    enqueue(english, Kind::Web, None);
}

/// Text whose key just arrived from the server: it is about to be shown.
pub fn soon(english: &str, id: Option<u32>) {
    enqueue(english, Kind::Soon, id);
}

pub struct Setup {
    pub exe: PathBuf,
    pub model: PathBuf,
    /// Larger model used on the GPU, when downloaded.
    pub model_big: PathBuf,
    /// Folder with the CUDA (cuBLAS) DLLs, when downloaded.
    pub gpu_runtime: PathBuf,
    pub port_file: PathBuf,
    pub local_file: PathBuf,
    /// Where translations of web page texts go (`web_local.jsonl`).
    pub web_file: PathBuf,
    /// Translations already present in `local.jsonl`.
    pub known: HashMap<String, String>,
    /// Web page texts already in `web_local.jsonl`.
    pub known_web: HashSet<String>,
}

pub fn start(setup: Setup) {
    if !setup.exe.is_file() || !setup.model.join("model").is_dir() {
        log!(
            "live translation: translator not found at {} — disabled",
            setup.exe.display()
        );
        return;
    }
    let _ = LIVE.set(RwLock::new(setup.known.clone()));
    RUNNING.store(true, Ordering::Relaxed);
    let (tx, rx) = mpsc::channel();
    let _ = QUEUE.set(Mutex::new(tx));
    let _ = std::thread::Builder::new()
        .name("gw2translated-live".into())
        .spawn(move || worker(setup, rx));
}

struct Sidecar {
    child: Child,
    port: u16,
}

/// Device the translator reported (`cpu`, `cuda`, `directml`), for the overlay.
static ACTIVE_DEVICE: Mutex<String> = Mutex::new(String::new());

pub fn active_device() -> String {
    ACTIVE_DEVICE.lock().map(|d| d.clone()).unwrap_or_default()
}

/// Threads for the translator: the setting, or a quarter of the CPU's
/// threads (2 to 6), leaving the rest to the game.
fn translator_threads() -> u32 {
    match crate::runtime::threads() {
        0 => std::thread::available_parallelism().map_or(2, |n| (n.get() as u32 / 4).clamp(2, 6)),
        n => n,
    }
}

fn spawn(setup: &Setup) -> Option<Sidecar> {
    let _ = std::fs::remove_file(&setup.port_file);
    let child = Command::new(&setup.exe)
        .arg("serve")
        .arg("--model")
        .arg(&setup.model)
        .arg("--port-file")
        .arg(&setup.port_file)
        .arg("--parent-pid")
        .arg(std::process::id().to_string())
        .arg("--threads")
        .arg(translator_threads().to_string())
        .arg("--device")
        .arg(crate::runtime::device().key())
        .arg("--model-big")
        .arg(&setup.model_big)
        .arg("--gpu-runtime")
        .arg(&setup.gpu_runtime)
        .creation_flags(CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS)
        .spawn()
        .inspect_err(|e| log!("live translation: failed to start the translator: {e}"))
        .ok()?;
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(60) {
        // Lines: port, device used, model (small/big), compute type.
        let status = std::fs::read_to_string(&setup.port_file).unwrap_or_default();
        let mut lines = status.lines().map(str::trim);
        if let Some(port) = lines.next().and_then(|s| s.parse::<u16>().ok()) {
            let device = lines.next().unwrap_or("cpu").to_owned();
            let model = lines.next().unwrap_or("small");
            let compute = lines.next().unwrap_or("?");
            log!(
                "live translation: translator ready on port {port} in {:.1}s (device {device}, model {model}, {compute}, {} threads)",
                t.elapsed().as_secs_f64(),
                translator_threads()
            );
            crate::procstats::watch(child.id());
            if let Ok(mut d) = ACTIVE_DEVICE.lock() {
                *d = format!("{device} / {model} / {compute}");
            }
            return Some(Sidecar { child, port });
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    log!("live translation: the translator did not respond within 60 s");
    None
}

/// `POST /translate` with a list; returns the translations in the same order.
fn translate(port: u16, texts: &[String]) -> std::io::Result<Vec<String>> {
    // No `beam`: the translator's default (beam 2 on the CPU, which costs about
    // the same as greedy, ~23 ms per text; beam 4 on the GPU) for every text.
    let body = serde_json::json!({ "q": texts, "source": "en", "target": "pb" });
    let body = body.to_string();
    let mut s = TcpStream::connect(("127.0.0.1", port))?;
    s.set_read_timeout(Some(Duration::from_secs(300)))?;
    write!(
        s,
        "POST /translate HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    let mut resp = Vec::new();
    s.read_to_end(&mut resp)?;
    let start = resp.windows(4).position(|w| w == b"\r\n\r\n").map_or(0, |i| i + 4);
    let v: serde_json::Value = serde_json::from_slice(&resp[start..]).map_err(std::io::Error::other)?;
    let out: Vec<String> = v["translatedText"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|x| x.as_str().map(str::to_owned))
        .collect();
    if out.len() == texts.len() {
        Ok(out)
    } else {
        Err(std::io::Error::other(format!("unexpected response: {v}")))
    }
}

/// Appends translations of web page texts to `web_local.jsonl`.
fn append_web(path: &Path, pairs: &[(String, String)]) -> std::io::Result<()> {
    let mut w = crate::records::Appender::open(path)?;
    for (en, tr) in pairs {
        w.write(&crate::records::Translation {
            en: en.clone(),
            tr: tr.clone(),
            src: Some(LOCAL_SOURCE.into()),
            ids: Vec::new(),
        })?;
    }
    w.flush()
}

/// Appends translations made on this PC to `local.jsonl`, with the text IDs
/// seen for each one.
fn append_local(path: &Path, pairs: &[(String, String)]) -> std::io::Result<()> {
    let mut ids = IDS.lock().ok();
    let mut w = crate::records::Appender::open(path)?;
    for (en, tr) in pairs {
        let mut ids = ids.as_mut().and_then(|m| m.as_mut()?.remove(en)).unwrap_or_default();
        ids.sort_unstable();
        w.write(&crate::records::Translation {
            en: en.clone(),
            tr: tr.clone(),
            src: Some(LOCAL_SOURCE.into()),
            ids,
        })?;
    }
    w.flush()
}

fn worker(setup: Setup, rx: Receiver<(String, Kind)>) {
    let mut seen: HashSet<String> = setup.known.keys().cloned().collect();
    let mut q = Queues::new(&setup.known_web);
    let mut sidecar: Option<Sidecar> = None;
    let mut failures = 0;
    let (mut total, mut total_ahead) = (0usize, 0usize);
    loop {
        // Device or threads changed: stop the old translator at once (it holds
        // video memory) and start the new one, so the overlay shows the change.
        if crate::runtime::take_restart() {
            if let Some(mut s) = sidecar.take() {
                log!("live translation: restarting the translator with the new settings");
                let _ = s.child.kill();
                let _ = s.child.wait();
                sidecar = spawn(&setup);
            }
            failures = 0;
        }
        // Only wait when there is nothing to do (waking up now and then to
        // apply setting changes); otherwise just collect what has arrived.
        if q.is_empty() {
            match rx.recv_timeout(std::time::Duration::from_millis(500)) {
                Ok(item) => q.push(item, &mut seen),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    // Start it before it is needed (loading takes a second or
                    // two), so the first line on screen does not wait for it.
                    let wanted =
                        crate::runtime::get(crate::runtime::Flag::LiveTranslate) && refresh::current_lang() == Some(0);
                    if wanted && sidecar.is_none() && failures < 3 {
                        sidecar = spawn(&setup);
                        if sidecar.is_none() {
                            failures += 1;
                        }
                    }
                    continue;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }
        for item in rx.try_iter() {
            q.push(item, &mut seen);
        }
        if refresh::current_lang() != Some(0) {
            q = Queues::new(&setup.known_web);
            continue;
        }
        let Some((kind, chunk)) = q.next_batch() else { continue };

        if sidecar
            .as_mut()
            .is_none_or(|s| s.child.try_wait().ok().flatten().is_some())
        {
            if failures >= 3 {
                // Translator is broken: drop the queue and just go back to waiting.
                q = Queues::new(&setup.known_web);
                continue;
            }
            sidecar = spawn(&setup);
            if sidecar.is_none() {
                failures += 1;
                continue;
            }
        }
        let port = sidecar.as_ref().map_or(0, |s| s.port);
        match translate(port, &chunk) {
            Ok(out) => {
                let pairs: Vec<(String, String)> = chunk
                    .iter()
                    .cloned()
                    .zip(out)
                    .filter(|(en, pt)| !pt.trim().is_empty() && !crate::dict::unusable(en, pt))
                    .collect();
                if kind == Kind::Web {
                    if let Err(e) = append_web(&setup.web_file, &pairs) {
                        log!("live translation: error writing {}: {e}", setup.web_file.display());
                    }
                    crate::web::deliver(&pairs);
                    QUEUED.store(q.len(), Ordering::Relaxed);
                    continue;
                }
                if let Err(e) = append_local(&setup.local_file, &pairs) {
                    log!("live translation: error writing {}: {e}", setup.local_file.display());
                }
                if let Some(map) = LIVE.get().and_then(|m| m.write().ok()).as_mut() {
                    map.extend(pairs.iter().cloned());
                }
                VERSION.fetch_add(1, Ordering::Relaxed);
                // Anything that was already on screen needs the screen refreshed,
                // whichever queue translated it.
                if q.shown(&pairs) || kind == Kind::Urgent {
                    total += pairs.len();
                    // The chat is cheap to rebuild and shows the line right away;
                    // the full refresh (heavy) stays throttled.
                    crate::chat::request_rebuild();
                    refresh::request();
                    log!(
                        "live translation: {total} on screen, {total_ahead} ahead of time ({} queued)",
                        q.len()
                    );
                } else {
                    total_ahead += pairs.len();
                    // About to be shown (next dialogue lines, keys just sent), or
                    // already decoded by the game (in its text cache, maybe in the
                    // chat): refresh (throttled) for the translation to be used.
                    if kind == Kind::Soon || pairs.iter().any(|(en, _)| crate::hooks::decoded_recently(en)) {
                        refresh::request();
                    }
                    if total_ahead % 200 < pairs.len() {
                        log!(
                            "live translation: {total} on screen, {total_ahead} ahead of time ({} queued)",
                            q.len()
                        );
                    }
                }
                ON_SCREEN.store(total, Ordering::Relaxed);
                AHEAD.store(total_ahead, Ordering::Relaxed);
                QUEUED.store(q.len(), Ordering::Relaxed);
            }
            Err(e) => {
                log!("live translation: batch failed: {e}");
                // Accepted again the next time it shows up.
                for t in &chunk {
                    if kind == Kind::Web {
                        q.web_seen.remove(t);
                    } else {
                        seen.remove(t);
                    }
                }
                failures += 1;
                if let Some(mut s) = sidecar.take() {
                    let _ = s.child.kill();
                }
            }
        }
    }
}

/// Queues by priority: text on screen (speech bubbles, cutscenes, story),
/// then web pages, then text about to be shown, then text translated ahead of time.
#[derive(Default)]
struct Queues {
    urgent: VecDeque<String>,
    web: VecDeque<String>,
    /// Web page texts translated or queued (apart from the game's: a page has its own file).
    web_seen: HashSet<String>,
    soon: VecDeque<String>,
    ahead: VecDeque<String>,
    /// Texts already shown on screen without a translation: whichever queue
    /// translates them, the screen must be refreshed.
    on_screen: HashSet<String>,
}

impl Queues {
    fn new(known_web: &HashSet<String>) -> Self {
        Queues {
            web_seen: known_web.clone(),
            ..Default::default()
        }
    }

    /// Whether any of `texts` was shown on screen (forgets them).
    fn shown(&mut self, texts: &[(String, String)]) -> bool {
        let mut any = false;
        for (en, _) in texts {
            any |= self.on_screen.remove(en);
        }
        any
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn len(&self) -> usize {
        self.urgent.len() + self.web.len() + self.soon.len() + self.ahead.len()
    }

    fn queue(&mut self, kind: Kind) -> &mut VecDeque<String> {
        match kind {
            Kind::Urgent => &mut self.urgent,
            Kind::Web => &mut self.web,
            Kind::Soon => &mut self.soon,
            Kind::Ahead => &mut self.ahead,
        }
    }

    fn push(&mut self, (text, kind): (String, Kind), seen: &mut HashSet<String>) {
        if kind == Kind::Web {
            if self.web_seen.insert(text.clone()) {
                self.web.push_back(text);
            }
            return;
        }
        if kind == Kind::Urgent {
            self.on_screen.insert(text.clone());
        }
        if seen.insert(text.clone()) {
            self.queue(kind).push_back(text);
            return;
        }
        // Already queued with a lower priority: move it up.
        let rank = |k: Kind| PRIORITY.iter().position(|&p| p == k).unwrap_or(usize::MAX);
        for lower in [Kind::Soon, Kind::Ahead] {
            if rank(lower) <= rank(kind) {
                continue;
            }
            if let Some(i) = self.queue(lower).iter().position(|t| *t == text) {
                self.queue(lower).remove(i);
                self.queue(kind).push_back(text);
                return;
            }
        }
    }

    fn next_batch(&mut self) -> Option<(Kind, Vec<String>)> {
        let kind = PRIORITY.into_iter().find(|&k| !self.queue(k).is_empty())?;
        let q = self.queue(kind);
        let size = if matches!(kind, Kind::Urgent | Kind::Web | Kind::Soon) {
            BATCH
        } else {
            BACKGROUND_BATCH
        };
        let chunk: Vec<String> = q.drain(..q.len().min(size)).collect();
        Some((kind, chunk))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Starts the real translator, when the binary and the model exist on the machine.
    #[test]
    fn sidecar_roundtrip_preserves_markup() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let exe = root.join("target/release/gw2translated-translator.exe");
        let model = root.join("data/models/pt-BR");
        if !exe.is_file() || !model.is_dir() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let setup = Setup {
            exe,
            model,
            port_file: dir.path().join("port.txt"),
            local_file: dir.path().join("local.jsonl"),
            web_file: dir.path().join("web_local.jsonl"),
            model_big: dir.path().join("big"),
            gpu_runtime: dir.path().join("gpu"),
            known: HashMap::new(),
            known_web: HashSet::new(),
        };
        let mut sc = spawn(&setup).expect("translator started");
        let out = translate(
            sc.port,
            &["<c=@flavor>Hello, friend.</c>".into(), "+15%% Magic Find".into()],
        )
        .unwrap();
        let _ = sc.child.kill();
        assert!(out[0].starts_with("<c=@flavor>") && out[0].ends_with("</c>"), "{out:?}");
        assert!(out[1].starts_with("+15%%"), "{out:?}");
        enqueue("a", Kind::Urgent, Some(9));
        enqueue("a", Kind::Urgent, Some(3));
        append_local(&setup.local_file, &[("a".into(), "b".into())]).unwrap();
        assert_eq!(
            std::fs::read_to_string(&setup.local_file).unwrap(),
            "{\"en\":\"a\",\"tr\":\"b\",\"src\":\"argos\",\"ids\":[3,9]}\n"
        );
    }
}

#[cfg(test)]
mod queue_tests {
    use super::*;

    #[test]
    fn web_texts_have_their_own_queue() {
        let mut seen = HashSet::from(["Armor".to_owned()]);
        let mut q = Queues::new(&HashSet::from(["Old".to_owned()]));
        q.push(("Armor".into(), Kind::Web), &mut seen);
        q.push(("Old".into(), Kind::Web), &mut seen);
        q.push(("Armor".into(), Kind::Web), &mut seen);
        q.push(("Later".into(), Kind::Ahead), &mut seen);
        assert_eq!(q.len(), 2);
        assert_eq!(q.next_batch(), Some((Kind::Web, vec!["Armor".to_owned()])));
        assert_eq!(q.next_batch(), Some((Kind::Ahead, vec!["Later".to_owned()])));
    }
}
