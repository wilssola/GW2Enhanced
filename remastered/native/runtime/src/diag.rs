//! Diagnostic hooks (read-only): record which files the game streams and which
//! of them reach the image decoder, to confirm where textures can be swapped.
//! Writes `GW2Remastered\diag_files.jsonl`. Nothing in the game is changed:
//! every hook calls the original with the original arguments.

use std::collections::HashSet;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::sync::mpsc::{Sender, channel};
use std::time::Instant;

use retour::GenericDetour;

use crate::game::Image;
use crate::log;
use crate::sig::Site;

unsafe extern "system" {
    fn GetCurrentThreadId() -> u32;
}

/// Async archive read: per-block callback `cb(data, size, offset, fileSize, flags, ctx)`;
/// `ctx` is the read job whose first u32 is the file id (ArchiveAsyncStreaming.cpp).
pub const BLOCK_CB: Site = Site { name: "archive_block_cb", sig: "48 89 5C 24 08 57 48 83 EC 30 4C 8B 54 24 68 48" };
/// Streaming image decoder (ImgDecode.cpp): `decode(decoder)`.
pub const IMG_DECODE: Site = Site { name: "img_decode", sig: "40 53 48 81 EC 90 00 00 00 48 8B D9 48 85 C9 75" };

/// GrTex2d loader: decoded image header ready (`GrTex2d.cpp`, asserts
/// `dims.x * dims.y <= m_dataDims...` when reusing a pre-allocated image).
pub const TEX_HEADER: Site = Site { name: "grtex2d_header", sig: "48 89 5C 24 10 48 89 6C 24 18 48 89 74 24 20 57 48 83 EC 40 F6 41 10 10" };
type TexHeader = unsafe extern "system" fn(*mut u8);
static HEADER: OnceLock<GenericDetour<TexHeader>> = OnceLock::new();

type BlockCb = unsafe extern "system" fn(*const u8, u64, u32, u64, u32, *const u32) -> u64;
type ImgDecode = unsafe extern "system" fn(*mut u8) -> u64;

static BLOCK: OnceLock<GenericDetour<BlockCb>> = OnceLock::new();
static DECODE: OnceLock<GenericDetour<ImgDecode>> = OnceLock::new();
static TX: OnceLock<std::sync::Mutex<Sender<Event>>> = OnceLock::new();

/// HD pack: file id -> (dds path, original file size). Loaded from `pack\index.jsonl`.
static PACK: OnceLock<std::collections::HashMap<u32, (PackLoc, u64, u32)>> = OnceLock::new();
/// 4K model textures (face, head, hair/mane, eyes) for the local player: `<pack>\character_4k`.
static PACK_CHAR4K: OnceLock<std::collections::HashMap<u32, (PackLoc, u64, u32)>> = OnceLock::new();
/// Model textures served at 4K so far (overlay counter).
pub static N_CHAR4K: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Model texture of the local player's character? The game already gives the own
/// character separate texture records (ModelFile flag 0x10 -> record flag 0x10,
/// different loader key), see docs "Texturas de modelo em 4x só no personagem local".
unsafe fn own_model_texture(ctx: *const u32) -> bool {
    unsafe {
        let ud = ctx.cast::<u8>().add(0x30).cast::<usize>().read();
        if ud == 0 || (*(ud as *const u8).add(0x10).cast::<u32>() & 0xFF) == COMPOSITE_LOADER_FLAGS {
            return false;
        }
        let rec = (ud as *const u8).add(0x28).cast::<usize>().read();
        if rec == 0 {
            return false;
        }
        let f = (rec as *const u8).add(0x60).cast::<u32>().read();
        f & 0x10 != 0 && f & (0x20 | 0x1000 | 0x2000) == 0
    }
}

/// 4x composite pieces (skin, face and armor of a 4x character): `<pack>\composite_4x`.
static PACK4: OnceLock<std::collections::HashMap<u32, (PackLoc, u64, u32)>> = OnceLock::new();
/// Read jobs whose data we already replaced: later blocks of the original are dropped.
static SWALLOW: std::sync::Mutex<Vec<usize>> = std::sync::Mutex::new(Vec::new());
/// Recently served DDS buffers. The decoder may keep reading from the pointer after
/// the callback returns, so buffers outlive the call (bounded ring).
static KEEP: std::sync::Mutex<std::collections::VecDeque<Arc<Vec<u8>>>> = std::sync::Mutex::new(std::collections::VecDeque::new());

/// Decompressed HD files kept in RAM. The engine reloads the same textures over and
/// over (composite pieces on every character rebuild, streaming evictions: ~8 loads
/// per file in a 2 h session), and each reload used to reopen and unzstd the file on
/// the loader thread. Key: (source, file id); LRU by a byte budget that follows the machine's RAM.
struct Cache {
    map: std::collections::HashMap<(u8, u32), (Arc<Vec<u8>>, u64, u64)>,
    bytes: usize,
    tick: u64,
}
static CACHE: std::sync::Mutex<Option<Cache>> = std::sync::Mutex::new(None);

#[repr(C)]
struct MemoryStatusEx {
    len: u32,
    load: u32,
    total_phys: u64,
    avail_phys: u64,
    total_page: u64,
    avail_page: u64,
    total_virt: u64,
    avail_virt: u64,
    avail_ext: u64,
}
unsafe extern "system" {
    fn GlobalMemoryStatusEx(m: *mut MemoryStatusEx) -> i32;
}

/// Cache budget from the machine's RAM, re-read every few seconds:
/// at most 1/16 of total RAM (hard cap 4 GB, even on big machines), and never more than what keeps a reserve of
/// free RAM (max(2 GB, 15% of total)) for the game and the system, counting what the
/// cache already holds as reclaimable. Floor 256 MB.
fn cache_budget(held: usize) -> usize {
    static LAST: std::sync::Mutex<Option<(Instant, usize)>> = std::sync::Mutex::new(None);
    let mut g = LAST.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((t, b)) = *g {
        if t.elapsed().as_secs() < 5 {
            return b;
        }
    }
    let mut m = MemoryStatusEx { len: std::mem::size_of::<MemoryStatusEx>() as u32, load: 0, total_phys: 0, avail_phys: 0, total_page: 0, avail_page: 0, total_virt: 0, avail_virt: 0, avail_ext: 0 };
    let b = if unsafe { GlobalMemoryStatusEx(&mut m) } != 0 {
        let total = m.total_phys as usize;
        let reserve = (2usize << 30).max(total / 100 * 15);
        let by_total = (total / 16).min(4 << 30);
        let by_free = (m.avail_phys as usize + held).saturating_sub(reserve);
        by_total.min(by_free).max(256 << 20)
    } else {
        1 << 30
    };
    let changed = g.is_none_or(|(_, old)| old.abs_diff(b) > 256 << 20);
    *g = Some((Instant::now(), b));
    drop(g);
    if changed {
        log!("texture RAM cache budget: {} MB (free RAM {} MB, total {} MB)", b >> 20, m.avail_phys >> 20, m.total_phys >> 20);
    }
    b
}
/// HD serves answered from the RAM cache (overlay/diag counter).
pub static N_CACHE_HIT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

fn cache_get(k: (u8, u32)) -> Option<Arc<Vec<u8>>> {
    let mut g = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let c = g.as_mut()?;
    c.tick += 1;
    let t = c.tick;
    let e = c.map.get_mut(&k)?;
    if sample_sum(&e.0) != e.2 {
        // The engine wrote into a served buffer: never hand it out again.
        let old = c.map.remove(&k).unwrap();
        c.bytes -= old.0.len();
        send(Event::Skipped { file_id: k.1, reason: "cache entry changed after serve; dropped" });
        return None;
    }
    e.1 = t;
    N_CACHE_HIT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Some(e.0.clone())
}

fn cache_put(k: (u8, u32), b: Arc<Vec<u8>>) {
    let mut g = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let c = g.get_or_insert_with(|| Cache { map: Default::default(), bytes: 0, tick: 0 });
    let budget = cache_budget(c.bytes);
    if b.len() > budget / 16 {
        return;
    }
    c.tick += 1;
    let len = b.len();
    let sum = sample_sum(&b);
    if let Some(old) = c.map.insert(k, (b, c.tick, sum)) {
        c.bytes -= old.0.len();
    }
    c.bytes += len;
    if c.bytes > budget {
        let mut v: Vec<((u8, u32), u64)> = c.map.iter().map(|(k, e)| (*k, e.1)).collect();
        v.sort_unstable_by_key(|e| e.1);
        for (k, _) in v {
            if c.bytes <= budget / 10 * 9 {
                break;
            }
            if let Some(e) = c.map.remove(&k) {
                c.bytes -= e.0.len();
            }
        }
    }
}

/// Checksum of 16 spread 256-byte samples (cheap check that a buffer is untouched).
fn sample_sum(b: &[u8]) -> u64 {
    let mut h = 0xcbf29ce484222325u64 ^ b.len() as u64;
    for i in 0..16 {
        let o = (b.len().saturating_sub(256)) / 15 * i;
        for &x in &b[o..(o + 256).min(b.len())] {
            h = (h ^ x as u64).wrapping_mul(0x100000001b3);
        }
    }
    h
}

/// Keys being built right now: a second thread asking for one waits for the first
/// instead of decompressing its own copy.
static BUILDING: std::sync::Mutex<Vec<(u8, u32)>> = std::sync::Mutex::new(Vec::new());
static BUILT: std::sync::Condvar = std::sync::Condvar::new();
/// Copies avoided that way.
pub static N_SHARED_BUILD: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

struct BuildGuard((u8, u32));
impl Drop for BuildGuard {
    fn drop(&mut self) {
        BUILDING.lock().unwrap_or_else(|e| e.into_inner()).retain(|k| *k != self.0);
        BUILT.notify_all();
    }
}

/// Cached HD file; `make` builds it on a miss (read + unzstd + convert). Returns the
/// buffer and whether it came from the cache. One buffer per key, even when several
/// threads ask at once.
fn cached(k: (u8, u32), make: impl FnOnce() -> Option<Vec<u8>>) -> Option<(Arc<Vec<u8>>, bool)> {
    loop {
        if let Some(b) = cache_get(k) {
            return Some((b, true));
        }
        let mut building = BUILDING.lock().unwrap_or_else(|e| e.into_inner());
        if !building.contains(&k) {
            building.push(k);
            break;
        }
        N_SHARED_BUILD.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // A file too large to cache would loop rebuilding one thread at a time, which is
        // still no worse than before; the timeout only guards against a lost wake-up.
        drop(BUILT.wait_timeout(building, std::time::Duration::from_millis(200)).unwrap_or_else(|e| e.into_inner()));
    }
    let _guard = BuildGuard(k);
    let b = Arc::new(make()?);
    cache_put(k, b.clone());
    Some((b, false))
}

/// (bytes, files) in the RAM cache; bytes held only by the KEEP ring.
pub fn memory_stats() -> (usize, usize, usize) {
    let (b, n) = CACHE.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map_or((0, 0), |c| (c.bytes, c.map.len()));
    let k = KEEP.lock().unwrap_or_else(|e| e.into_inner()).iter().filter(|a| Arc::strong_count(a) == 1).map(|a| a.len()).sum();
    (b, n, k)
}

pub enum Event {
    Archive { file_id: u32, file_size: u64, magic: [u8; 4] },
    Decode { file_size: u32, magic: [u8; 4] },
    Replaced { file_id: u32, original: u64, bytes: usize },
    Skipped { file_id: u32, reason: &'static str },
    Header { dec: (u32, u32), rec: (u32, u32), prealloc: bool, dropped: bool },
    Fallback { file_id: u32, original: usize, bytes: usize, ms: u32 },
    Resize { up: bool, cur: (u32, u32), req: (u32, u32), refused: bool },
    Load { file_id: u32, dec: (u32, u32), rec: (u32, u32), prealloc: bool, loader_flags: u32, rec_flags: u32, dec_fmt: u32, rec_fmt: u32 },
    /// Slow HD serve: time to get our buffer, time inside the engine's callback.
    Slow { file_id: u32, prep_us: u32, engine_us: u32, bytes: usize, cached: bool, tid: u32 },
}

pub fn send(e: Event) {
    if let Some(tx) = TX.get() {
        if let Ok(tx) = tx.lock() {
            let _ = tx.send(e);
        }
    }
}

fn magic_str(m: &[u8; 4]) -> String {
    m.iter().map(|&c| if c.is_ascii_alphanumeric() || c == b' ' { c as char } else { '.' }).collect()
}

/// Composite sources (armor base/normal/dye/cut masks, skin patterns and their
/// high-res variants): with the 2048 atlas every one must arrive at 2×.
static COMPOSITE: OnceLock<std::collections::HashSet<u32>> = OnceLock::new();
/// Jobs whose original file is being collected for the runtime 2× fallback.
/// Device (model) textures allowed to be swapped: complete low/high-res pairs,
/// both at 2×, so the engine's UpRes/DownRes relation (exactly 2×) still holds.
static DEVICE: OnceLock<std::collections::HashSet<u32>> = OnceLock::new();
/// True once the 2048 atlas patch is in place; without it no composite piece is touched.
static COMPOSITE_OK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Files whose texture record has a fixed, pre-allocated size: an HD version
/// makes the engine reload them forever, so they are always served original.
static DENY: std::sync::Mutex<Option<std::collections::HashSet<u32>>> = std::sync::Mutex::new(None);
/// Model textures served HD this session -> width of the HD file the engine saw. The engine can
/// only grow a loaded texture (UpRes) or halve it (DownRes), so once HD is off the original has to
/// come at that same size, or the resize is refused and the HD stays on screen.
static SERVED_HD: std::sync::Mutex<Option<std::collections::HashMap<u32, u32>>> = std::sync::Mutex::new(None);
/// Warm start. Reading and decompressing a pack file the first time costs ~5 ms on the engine's
/// loader thread; a session in a busy map pays that thousands of times. The files served are noted
/// (`HOT`), saved every minute, and read back on the next launch by a few low-priority threads, so
/// the engine finds them in the RAM cache. Kinds are the first part of the cache key: 0 model
/// texture, 1 composite piece (2x), 3 composite piece (4x), 4 local-player model texture.
static HOT: std::sync::Mutex<Vec<(u8, u32, [u8; 8])>> = std::sync::Mutex::new(Vec::new());
static HOT_SEEN: std::sync::Mutex<Option<HashSet<(u8, u32)>>> = std::sync::Mutex::new(None);
static HOT_DIRTY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
const HOT_MAX: usize = 30_000;
const HOT_MAGIC: &[u8; 8] = b"GW2HOT1\0";
const PREFETCH_THREADS: usize = 4;
/// Files warmed at launch.
pub static N_PREFETCHED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

fn hot_note(kind: u8, id: u32, head: &[u8]) {
    if !matches!(kind, 0 | 1 | 3 | 4) || head.len() < 8 {
        return;
    }
    if !HOT_SEEN.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(Default::default).insert((kind, id)) {
        return;
    }
    let mut h = [0u8; 8];
    h.copy_from_slice(&head[..8]);
    HOT.lock().unwrap_or_else(|e| e.into_inner()).push((kind, id, h));
    HOT_DIRTY.store(true, std::sync::atomic::Ordering::Relaxed);
}

fn hot_encode(list: &[(u8, u32, [u8; 8])]) -> Vec<u8> {
    let list = &list[list.len().saturating_sub(HOT_MAX)..];
    let mut v = Vec::with_capacity(12 + list.len() * 13);
    v.extend_from_slice(HOT_MAGIC);
    v.extend_from_slice(&(list.len() as u32).to_le_bytes());
    for (k, id, h) in list {
        v.push(*k);
        v.extend_from_slice(&id.to_le_bytes());
        v.extend_from_slice(h);
    }
    v
}

fn hot_decode(b: &[u8]) -> Vec<(u8, u32, [u8; 8])> {
    if b.len() < 12 || &b[..8] != HOT_MAGIC {
        return Vec::new();
    }
    let n = u32::from_le_bytes(b[8..12].try_into().unwrap()) as usize;
    b[12..]
        .chunks_exact(13)
        .take(n)
        .map(|r| (r[0], u32::from_le_bytes(r[1..5].try_into().unwrap()), r[5..13].try_into().unwrap()))
        .collect()
}

/// Saves the list when it grew (once a minute).
fn hot_saver(path: PathBuf) {
    loop {
        std::thread::sleep(std::time::Duration::from_secs(60));
        if !HOT_DIRTY.swap(false, std::sync::atomic::Ordering::Relaxed) {
            continue;
        }
        let bytes = hot_encode(&HOT.lock().unwrap_or_else(|e| e.into_inner()));
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, bytes).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

/// Fills the RAM cache with the files of the previous session, oldest request first, with a few
/// below-normal threads, up to 60% of the cache budget.
fn prefetch(list: Vec<(u8, u32, [u8; 8])>) {
    use rayon::prelude::*;
    use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL};
    let t0 = Instant::now();
    let Ok(pool) = rayon::ThreadPoolBuilder::new()
        .num_threads(PREFETCH_THREADS)
        .thread_name(|i| format!("gw2remastered-prefetch-{i}"))
        .start_handler(|_| unsafe {
            SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
        })
        .build()
    else {
        return;
    };
    pool.install(|| {
        list.par_iter().for_each(|&(kind, id, head)| {
            let (held, _, _) = memory_stats();
            if held >= cache_budget(held) / 10 * 6 || !crate::overlay::enabled() {
                return;
            }
            let (map, is_comp) = match kind {
                0 => (PACK.get(), false),
                1 => (PACK.get(), true),
                3 => (PACK4.get(), true),
                4 => (PACK_CHAR4K.get(), false),
                _ => return,
            };
            let Some(e) = map.and_then(|m| m.get(&id)) else { return };
            let built = cached((kind, id), || read_pack_file(&e.0).map(|b| if is_comp { b } else { native_format(b, &head) }));
            if built.is_some_and(|(_, from_cache)| !from_cache) {
                N_PREFETCHED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        });
    });
    let (held, files, _) = memory_stats();
    log!(
        "prefetch: {} of {} files in {} ms, cache {} MB / {files} files",
        N_PREFETCHED.load(std::sync::atomic::Ordering::Relaxed),
        list.len(),
        t0.elapsed().as_millis(),
        held >> 20
    );
}

#[cfg(test)]
mod hot_tests {
    #[test]
    fn list_round_trips_and_keeps_the_newest() {
        let l: Vec<(u8, u32, [u8; 8])> = (0..40_000u32).map(|i| ((i % 5) as u8, i, [i as u8; 8])).collect();
        let b = super::hot_encode(&l);
        let d = super::hot_decode(&b);
        assert_eq!(d.len(), super::HOT_MAX);
        assert_eq!(d.last(), l.last());
        assert_eq!(d[0], l[40_000 - super::HOT_MAX]);
        assert!(super::hot_decode(b"garbage").is_empty());
        assert!(super::hot_decode(&b[..20]).len() < 3);
    }
}

/// Served HD texture -> bytes it adds on the GPU over the original (estimate for the overlay).
static HD_EXTRA: std::sync::Mutex<Option<std::collections::HashMap<u32, u32>>> = std::sync::Mutex::new(None);

/// Sum of `HD_EXTRA` for the textures the engine still holds; the others are forgotten.
pub fn vram_extra_resident(resident: &std::collections::HashSet<u32>) -> u64 {
    let mut g = HD_EXTRA.lock().unwrap_or_else(|e| e.into_inner());
    let Some(m) = g.as_mut() else { return 0 };
    m.retain(|id, _| resident.contains(id));
    m.values().map(|&v| v as u64).sum()
}
/// Can the pack serve this model texture (HD on, listed, not in a half-served group)?
fn servable_id(id: u32) -> bool {
    if denied(id) {
        return false;
    }
    PACK_CHAR4K.get().is_some_and(|p| p.contains_key(&id))
        || (PACK.get().is_some_and(|p| p.contains_key(&id)) && DEVICE.get().is_some_and(|d| d.contains(&id)))
}

/// The engine pairs a texture's small and big files in one record and grows the small one to the big
/// one, exactly 2x. If the file being read is HD but the other half of its record is not, the sizes
/// cannot line up: the grow request is refused (every frame) and the texture can stay smaller than
/// the original. The survey cannot see every pair the engine makes, so the record is asked directly.
/// True when there is no other half yet (the small file is read first) or both halves are served.
unsafe fn pair_is_consistent(ctx: *const u32, file_id: u32) -> bool {
    unsafe {
        let ud = ctx.cast::<u8>().add(0x30).cast::<usize>().read();
        if ud == 0 {
            return true;
        }
        let rec = (ud as *const u8).add(0x28).cast::<usize>().read();
        if rec == 0 {
            return true;
        }
        let low = (rec as *const u8).add(0x50).cast::<usize>().read();
        let high = (rec as *const u8).add(0x58).cast::<usize>().read();
        let other = if ud == high { low } else if ud == low { high } else { return true };
        if other == 0 || other == ud {
            return true;
        }
        let id = (other as *const u8).add(0x30).cast::<u32>().read();
        id == 0 || id == file_id || servable_id(id)
    }
}

/// Set on the factor handed to `finish_fallback` for a model (non-composite) texture.
const FALLBACK_DEVICE: u32 = 0x100;
/// Debug colors: hand back the original tinted with the colour in the upper 16 bits.
const FALLBACK_FLAT: u32 = 0x200;

/// Why the pack leaves a texture alone (`pack/reasons.bin`, tools/batch/build_reasons.py); only
/// used by Debug colors. One byte per file id.
static REASONS: OnceLock<Vec<u8>> = OnceLock::new();
/// Served textures of groups that are only half in the pack (`pack/pair_skip.txt`,
/// tools/batch/check_groups.py): the engine could not pair their sizes, so they stay original.
static PAIR_SKIP: OnceLock<HashSet<u32>> = OnceLock::new();
/// Terrain tiles (ATEP) with an upscaled version in the pack (`pack/terrain_ids.txt`). They are CPU textures
/// of the terrain, not paired model textures, so they are served only when `terrain_hd` is on (experimental).
static TERRAIN: OnceLock<HashSet<u32>> = OnceLock::new();
pub static TERRAIN_HD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static N_TERRAIN: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
/// Textures currently shown in a reason colour: reloaded when Debug colors or HD goes off.
static MARKED: std::sync::Mutex<Option<HashSet<u32>>> = std::sync::Mutex::new(None);

fn reason_of(id: u32) -> u8 {
    REASONS.get().and_then(|r| r.get(id as usize).copied()).unwrap_or(0)
}

fn reason_color(code: u8) -> Option<u16> {
    match code {
        1 => Some(0xF800), // effect: red
        2 => Some(0x8010), // small member of a pair whose big member is at the target: purple
        3 => Some(0xFFFF), // already at the target size: white
        4 => Some(0x9B02), // under 128 px: brown
        5 => Some(0xFDB6), // no job, other: pink
        6 => Some(0x7BEF), // source of a character piece: gray
        _ => None,
    }
}

/// Should the texture-reload pass visit this file? Pack textures, and with Debug colors the ones
/// that get (or have) a reason colour.
pub fn wants_reload(id: u32) -> bool {
    if is_device_pack(id) {
        return true;
    }
    if MARKED.lock().unwrap_or_else(|e| e.into_inner()).as_ref().is_some_and(|m| m.contains(&id)) {
        return true;
    }
    DEBUG_TINT.load(std::sync::atomic::Ordering::Relaxed) && crate::overlay::enabled() && reason_color(reason_of(id)).is_some()
}

/// End of a reload pass: nothing stays marked once the colours are off.
pub fn clear_marked_if_idle() {
    if !DEBUG_TINT.load(std::sync::atomic::Ordering::Relaxed) || !crate::overlay::enabled() {
        *MARKED.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// The original of `file` as a DDS with its colour blocks set to `color` (alpha kept).
fn flat_original(file: &[u8], color: u16) -> Option<Vec<u8>> {
    let mut dds = crate::fallback::flat_original(file)?;
    tint_dds(&mut dds, color);
    Some(native_format(dds, &file[..file.len().min(8)]))
}
/// decodeFunc pointers confirmed to belong to the GrTex2d texture loader: for them
/// the read context's +0x30 userdata is the loader object (verified in the header
/// hook, where the engine hands us that same loader).
static LOADER_FUNCS: std::sync::Mutex<Vec<usize>> = std::sync::Mutex::new(Vec::new());
thread_local! {
    static CURRENT_UD: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static CURRENT_DF: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
/// GrTex2d stream-decode adapter (`GrTex2d.cpp`, jumps to the loader's decode
/// with the loader as 6th argument): the decodeFunc of every texture file read.
pub const TEX_DECODE_ADAPTER: Site = Site {
    name: "grtex2d_decode_adapter",
    sig: "48 89 6C 24 08 48 89 74 24 10 57 48 83 EC 30 48 83 7C 24 68 00 41 8B F9 8B F2 48 8B E9 75",
};
/// Loader flags of a composite-source load (fixed 2048 slot): `0x19`.
const COMPOSITE_LOADER_FLAGS: u32 = 0x19;
/// True when this read feeds the armor composite (not a model/device texture).
unsafe fn composite_load(ctx: *const u32) -> bool {
    unsafe {
        let df = ctx.cast::<u8>().add(0x28).cast::<usize>().read();
        let ud = ctx.cast::<u8>().add(0x30).cast::<usize>().read();
        if df == 0 || ud == 0 || !LOADER_FUNCS.lock().unwrap_or_else(|e| e.into_inner()).contains(&df) {
            return false;
        }
        let u = ud as *const u8;
        if (*u.add(0x10).cast::<u32>() & 0xFF) != COMPOSITE_LOADER_FLAGS {
            return false;
        }
        // UI (ATEU) and terrain (ATEP) CPU textures use the same loader flags (0x19),
        // but only composite pieces have a record pre-allocated from the blit slot
        // (+0x50 set, record width != 0). Without this check the runtime 2x fallback
        // re-encoded UI icons and they lost their transparency.
        if u.add(0x50).cast::<usize>().read() == 0 {
            return false;
        }
        let rec = u.add(0x28).cast::<usize>().read();
        rec != 0 && (rec as *const u8).add(0x48).cast::<u32>().read() != 0
    }
}
thread_local! {
    /// File id being decoded right now (the decodeFunc runs inside our call to
    /// the original callback, so the header hook sees it on the same thread).
    static CURRENT: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}
/// A model texture the HD pack serves (or would, with the remaster on).
pub fn is_device_pack(id: u32) -> bool {
    DEVICE.get().is_some_and(|d| d.contains(&id)) && (PACK.get().is_some_and(|p| p.contains_key(&id)) || PACK_CHAR4K.get().is_some_and(|p| p.contains_key(&id)))
}

fn denied(id: u32) -> bool {
    PAIR_SKIP.get().is_some_and(|s| s.contains(&id)) || DENY.lock().unwrap_or_else(|e| e.into_inner()).as_ref().is_some_and(|d| d.contains(&id))
}
static ACCUM: std::sync::Mutex<Vec<(usize, u32, u32, Vec<u8>)>> = std::sync::Mutex::new(Vec::new());

/// Scale of the composite a piece is loaded for, read from the pre-allocated
/// record the engine made from the (scaled) blit slot: 4096 wide = 4x, else 2x.
/// Stateless, so it holds on the reader thread too (see composite.rs).
unsafe fn comp_factor(ctx: *const u32) -> u32 {
    unsafe {
        let ud = ctx.cast::<u8>().add(0x30).cast::<usize>().read();
        if ud == 0 {
            return 2;
        }
        let rec = (ud as *const u8).add(0x28).cast::<usize>().read();
        if rec == 0 {
            return 2;
        }
        if (rec as *const u8).add(0x48).cast::<u32>().read() >= 4096 { 4 } else { 2 }
    }
}

const KEEP_MAX: usize = 256;
const KEEP_MIN: usize = 64;
const KEEP_BYTES: usize = 1 << 30;
/// Cache key of a runtime upscale.
fn runtime_key(file_id: u32, factor: u32) -> (u8, u32) {
    (if factor == 4 { 8 } else { 7 }, file_id)
}

fn keep(buf: Vec<u8>) -> (*const u8, u64) {
    keep_arc(Arc::new(buf))
}

fn keep_arc(buf: Arc<Vec<u8>>) -> (*const u8, u64) {
    let mut keep = KEEP.lock().unwrap_or_else(|e| e.into_inner());
    keep.push_back(buf);
    // Buffers also in the RAM cache cost nothing extra here; the rest are capped in bytes
    // (a 4x texture is tens of MB), but never below what the decoders may still be reading.
    let mut alone: usize = keep.iter().filter(|a| Arc::strong_count(a) == 1).map(|a| a.len()).sum();
    while keep.len() > KEEP_MAX || (keep.len() > KEEP_MIN && alone > KEEP_BYTES) {
        if let Some(a) = keep.pop_front() {
            if Arc::strong_count(&a) == 1 {
                alone -= a.len();
            }
        }
    }
    let b = keep.back().unwrap();
    (b.as_ptr(), b.len() as u64)
}

/// Whole original file collected: build the 2× DDS (or hand back the original).
fn finish_fallback(file_id: u32, file: Vec<u8>, factor_and_flags: u32) -> (*const u8, u64) {
    let device = factor_and_flags & FALLBACK_DEVICE != 0;
    let factor = factor_and_flags & 0xFF;
    if factor_and_flags & FALLBACK_FLAT != 0 {
        let color = (factor_and_flags >> 16) as u16;
        let built = cached((9, file_id), || std::panic::catch_unwind(|| flat_original(&file, color)).ok().flatten());
        return match built {
            Some((d, _)) => keep_arc(d),
            None => keep(file),
        };
    }
    let t = std::time::Instant::now();
    let make = || {
        std::panic::catch_unwind(|| crate::fallback::upscale_file_keep(&file, factor, device)).ok().flatten().map(|mut d| {
            debug_tint(&mut d, TINT_RUNTIME);
            // Model textures go back to the engine in its own container when non-square.
            if device { native_format(d, &file[..file.len().min(8)]) } else { d }
        })
    };
    // A model texture is enlarged once per toggle and the engine keeps it; character pieces are
    // read again and again on every rebuild, so only those are worth the RAM cache.
    let built = if device { make().map(|d| (Arc::new(d), false)) } else { cached(runtime_key(file_id, factor), make) };
    match built {
        Some((dds, from_cache)) => {
            crate::overlay::N_PLAIN2X.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if !from_cache {
                send(Event::Fallback { file_id, original: file.len(), bytes: dds.len(), ms: t.elapsed().as_millis() as u32 });
            }
            keep_arc(dds)
        }
        None => {
            send(Event::Skipped { file_id, reason: "fallback failed; original kept" });
            keep(file)
        }
    }
}

unsafe extern "system" fn block_cb(data: *const u8, size: u64, offset: u32, file_size: u64, flags: u32, ctx: *const u32) -> u64 {
    let orig = |d: *const u8, sz: u64, off: u32, fs: u64, fl: u32| unsafe { BLOCK.get().unwrap().call(d, sz, off, fs, fl, ctx) };
    let key = ctx as usize;
    // flags: 1 first block, 2 last block, 4/8 cancel/abort.
    if offset != 0 || (flags & 0xE) != 0 {
        // Rest of a file already replaced by a pack DDS: drop it.
        {
            let mut sw = SWALLOW.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(i) = sw.iter().position(|&k| k == key) {
                if (flags & 0xE) != 0 {
                    sw.swap_remove(i);
                }
                return 0;
            }
        }
        // Rest of a file being collected for the 2× fallback.
        let mut acc = ACCUM.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(i) = acc.iter().position(|e| e.0 == key) {
            if (flags & 0xC) != 0 {
                acc.swap_remove(i);
                return 0;
            }
            if !data.is_null() && size as u32 > 0 {
                let buf = &mut acc[i].3;
                let end = offset as usize + size as u32 as usize;
                if buf.len() < end {
                    buf.resize(end, 0);
                }
                unsafe { std::ptr::copy_nonoverlapping(data, buf.as_mut_ptr().add(offset as usize), size as u32 as usize) };
            }
            if (flags & 2) != 0 {
                let (_, file_id, factor, file) = acc.swap_remove(i);
                drop(acc);
                let (p, n) = finish_fallback(file_id, file, factor);
                CURRENT.with(|c| c.set(file_id));
                let r = orig(p, n, 0, n, 1 | 2);
                CURRENT.with(|c| c.set(0));
                return r;
            }
            return 0;
        }
    }
    let mut replacement: Option<(*const u8, u64)> = None;
    let mut collect = false;
    let t_prep = Instant::now();
    let mut hit = false;
    let mut factor = 2u32;
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        if offset == 0 && !ctx.is_null() && !data.is_null() && (size as u32) >= 8 && (flags & 4) == 0 {
            let head = std::slice::from_raw_parts(data, 8);
            let m: [u8; 4] = head[0..4].try_into().unwrap();
            let file_id = ctx.read();
            send(Event::Archive { file_id, file_size: file_size as u32 as u64, magic: m });
            // job layout: +0x28 decodeFunc (streaming). Never replace into fixed-size buffers.
            let streaming = ctx.cast::<u8>().add(0x28).cast::<usize>().read() != 0;
            // Every read that feeds the armor composite must arrive at 2× (the slots,
            // atlas and rectangles are doubled); decided from the live loader, so it
            // covers all composite sources, not only the ones in our id list.
            let is_comp = composite_load(ctx);
            let comp_ok = COMPOSITE_OK.load(std::sync::atomic::Ordering::Relaxed);
            // Remaster OFF (overlay): pack is ignored; composite pieces still get a
            // plain 2x below so the 2048 composite layout stays valid.
            // A composite piece from the pack must be exactly SCALE × the original
            // (ATEX width at bytes 8..10); otherwise use the runtime upscale.
            let orig_w = if (size as u32) >= 12 { u16::from_le_bytes([*data.add(8), *data.add(9)]) as u32 } else { 0 };
            if is_comp {
                factor = comp_factor(ctx);
            }
            let scale_ok = |pw: u32| !is_comp || (orig_w != 0 && pw == orig_w * factor);
            let is_terrain = !is_comp
                && TERRAIN_HD.load(std::sync::atomic::Ordering::Relaxed)
                && TERRAIN.get().is_some_and(|t| t.contains(&file_id));
            let mut served = false;
            // Model textures: 4K for the local player (or everyone with composite_4x_all)
            // when the 4K pack has the file; both members of a low/high pair share the
            // record, so both get 4K and the engine's exact-2x rule still holds.
            let char4k = !is_comp
                && PACK_CHAR4K.get().is_some_and(|p| p.contains_key(&file_id))
                && LOADER_FUNCS.lock().unwrap_or_else(|e| e.into_inner()).contains(&ctx.cast::<u8>().add(0x28).cast::<usize>().read())
                && (crate::composite::ALL_4X.load(std::sync::atomic::Ordering::Relaxed) || own_model_texture(ctx));
            if char4k {
                send(Event::Skipped { file_id, reason: "model texture at 4K (local player)" });
            }
            let src: u8 = if is_comp && factor == 4 { 1 } else if char4k { 2 } else { 0 };
            let ckey = (src * 2 + is_comp as u8, file_id);
            if let Some((path, src_bytes, hd_w)) = (if src == 1 { &PACK4 } else if src == 2 { &PACK_CHAR4K } else { &PACK })
                .get()
                .filter(|_| if is_comp { crate::overlay::pieces_on() } else { crate::overlay::enabled() })
                .and_then(|p| p.get(&file_id))
                .filter(|e| scale_ok(e.2))
            {
                if denied(file_id) {
                    send(Event::Skipped { file_id, reason: "fixed-size texture record; original kept" });
                } else if !is_comp && !is_terrain && !DEVICE.get().is_some_and(|d| d.contains(&file_id)) {
                    // Device textures only when listed as a complete low/high pair.
                    send(Event::Skipped { file_id, reason: "device texture not in paired list" });
                } else if !is_comp && !is_terrain && !pair_is_consistent(ctx, file_id) {
                    send(Event::Skipped { file_id, reason: "other half of the pair is not served" });
                } else if is_comp && !comp_ok {
                    send(Event::Skipped { file_id, reason: "composite piece but 2x atlas not active" });
                } else if !streaming {
                    send(Event::Skipped { file_id, reason: "fixed-size target buffer" });
                } else if *src_bytes != 0 && (file_size as u32 as u64).abs_diff(*src_bytes) > 4 {
                    // The archive reports some files 4 bytes shorter than their stored
                    // size (trailing checksum), so only a larger difference means a patch.
                    send(Event::Skipped { file_id, reason: "original changed (size mismatch)" });
                } else if let Some((buf, from_cache)) =
                    cached(ckey, || {
                        read_pack_file(path).map(|mut b| {
                            debug_tint(&mut b, match (is_comp, src) {
                                (true, 1) => TINT_COMP4X,
                                (true, _) => TINT_COMP2X,
                                (false, 2) => TINT_CHAR4K,
                                _ => TINT_DEVICE,
                            });
                            if is_comp { b } else { native_format(b, &head) }
                        })
                    })
                {
                    hit = from_cache;
                    hot_note(ckey.0, file_id, &head);
                    crate::overlay::N_PACK.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if char4k {
                        N_CHAR4K.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    send(Event::Replaced { file_id, original: file_size as u32 as u64, bytes: buf.len() });
                    replacement = Some(keep_arc(buf));
                    served = true;
                    if is_terrain {
                        let n = N_TERRAIN.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                        if n <= 5 || n.is_power_of_two() {
                            log!("terrain: HD tiles served so far: {n} (last id {file_id})");
                        }
                    }
                    if !is_comp {
                        SERVED_HD.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(Default::default).insert(file_id, *hd_w);
                        let ratio = orig_w.max(1) as f64 / (*hd_w).max(1) as f64;
                        let extra = (replacement.map_or(0, |r| r.1) as f64 * (1.0 - ratio * ratio).max(0.0)) as u32;
                        HD_EXTRA.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(Default::default).insert(file_id, extra);
                    }
                }
            }
            // 4× character, piece without a 4× pack entry (armor: only skin/face/fur get
            // one): take the 2× AI version from the main pack and double it, so armor
            // keeps its 2× detail while matching the 4× atlas.
            if !served && is_comp && factor == 4 && comp_ok && streaming && !denied(file_id) && crate::overlay::pieces_on() {
                if let Some((path, src_bytes, _)) = PACK.get().and_then(|p| p.get(&file_id)).filter(|e| orig_w != 0 && e.2 == orig_w * 2) {
                    if *src_bytes == 0 || (file_size as u32 as u64).abs_diff(*src_bytes) <= 4 {
                        let t = std::time::Instant::now();
                        if let Some((dds, from_cache)) = cached((6, file_id), || {
                            read_pack_file(path).and_then(|b| std::panic::catch_unwind(|| crate::fallback::upscale_dds_2x(&b)).ok().flatten()).map(|mut d| {
                                debug_tint(&mut d, TINT_DOUBLED);
                                d
                            })
                        }) {
                            hit = from_cache;
                            crate::overlay::N_PACK.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            if !from_cache {
                                send(Event::Fallback { file_id, original: file_size as u32 as usize, bytes: dds.len(), ms: t.elapsed().as_millis() as u32 });
                            }
                            replacement = Some(keep_arc(dds));
                            served = true;
                        }
                    }
                }
            }
            // A composite piece must never reach the doubled atlas at 1x ("Composite mask
            // smaller than source RGB" assert): whenever the pack did not serve it, use the
            // runtime 2x, whatever the reason it was skipped.
            // HD was turned off for a model texture the engine already holds at HD size: it cannot
            // shrink it, so hand over the original at that same size (plain upscale, no AI). Taken
            // once per file: after the texture is destroyed (map change) it loads vanilla again.
            // A texture never served HD this session is not touched: the unmodified game.
            if !served && !is_comp && streaming && !crate::overlay::enabled() && !denied(file_id) && orig_w != 0 && crate::fallback::supported(&head[4..8]) {
                let hd = SERVED_HD.lock().unwrap_or_else(|e| e.into_inner()).as_mut().and_then(|m| m.remove(&file_id));
                if let Some(w) = hd.filter(|w| *w > orig_w && w % orig_w == 0 && (w / orig_w).is_power_of_two() && w / orig_w <= 8) {
                    factor = (w / orig_w) | FALLBACK_DEVICE;
                    collect = true;
                }
            }
            // Debug colors: a texture the pack leaves alone is shown in the colour of the reason.
            if !served && !collect && !is_comp && streaming && DEBUG_TINT.load(std::sync::atomic::Ordering::Relaxed) && crate::overlay::enabled() && !denied(file_id) && matches!(&head[4..8], b"DXT1" | b"DXT5") {
                if let Some(color) = reason_color(reason_of(file_id)) {
                    MARKED.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(Default::default).insert(file_id);
                    factor = 1 | FALLBACK_FLAT | ((color as u32) << 16);
                    collect = true;
                }
            }
            if !served && streaming && is_comp && comp_ok && !denied(file_id) && crate::fallback::supported(&head[4..8])
            {
                // Already upscaled once: no need to collect and redo it.
                if let Some(b) = cache_get(runtime_key(file_id, factor)) {
                    crate::overlay::N_PLAIN2X.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    hit = true;
                    replacement = Some(keep_arc(b));
                } else {
                    collect = true;
                }
            }
        }
    }));
    if let Some((p, n)) = replacement {
        if (flags & 2) == 0 {
            SWALLOW.lock().unwrap_or_else(|e| e.into_inner()).push(key);
        }
        let file_id = unsafe { ctx.read() };
        let prep = t_prep.elapsed();
        CURRENT.with(|c| c.set(file_id));
        let t_eng = Instant::now();
        let r = orig(p, n, 0, n, flags | 1 | 2);
        CURRENT.with(|c| c.set(0));
        let eng = t_eng.elapsed();
        if (prep + eng).as_micros() >= 4000 {
            send(Event::Slow {
                file_id,
                prep_us: prep.as_micros() as u32,
                engine_us: eng.as_micros() as u32,
                bytes: n as usize,
                cached: hit,
                tid: unsafe { GetCurrentThreadId() },
            });
        }
        return r;
    }
    if collect {
        let file_id = unsafe { ctx.read() };
        let mut buf = Vec::with_capacity(file_size as u32 as usize);
        buf.extend_from_slice(unsafe { std::slice::from_raw_parts(data, size as u32 as usize) });
        if (flags & 2) != 0 {
            let (p, n) = finish_fallback(file_id, buf, factor);
            CURRENT.with(|c| c.set(file_id));
            let r = orig(p, n, 0, n, 1 | 2);
            CURRENT.with(|c| c.set(0));
            return r;
        }
        ACCUM.lock().unwrap_or_else(|e| e.into_inner()).push((key, file_id, factor, buf));
        return 0;
    }
    if offset == 0 && !ctx.is_null() {
        CURRENT.with(|c| c.set(unsafe { ctx.read() }));
        CURRENT_DF.with(|c| c.set(unsafe { ctx.cast::<u8>().add(0x28).cast::<usize>().read() }));
        CURRENT_UD.with(|c| c.set(unsafe { ctx.cast::<u8>().add(0x30).cast::<usize>().read() }));
        let r = orig(data, size, offset, file_size, flags);
        CURRENT.with(|c| c.set(0));
        CURRENT_UD.with(|c| c.set(0));
        return r;
    }
    orig(data, size, offset, file_size, flags)
}

/// loader: +0x28 record (dims +0x48/+0x4C), +0x40 decoder (dims +0x38/+0x3C),
/// +0x50 pre-allocated image. If our HD image is larger than the reused buffer,
/// drop the reuse so the engine allocates a new image with the decoded dims.
unsafe extern "system" fn tex_header(loader: *mut u8) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        let rec = loader.add(0x28).cast::<*mut u8>().read();
        let dec = loader.add(0x40).cast::<*mut u8>().read();
        if rec.is_null() || dec.is_null() {
            return;
        }
        let d = (dec.add(0x38).cast::<u32>().read(), dec.add(0x3C).cast::<u32>().read());
        let r = (rec.add(0x48).cast::<u32>().read(), rec.add(0x4C).cast::<u32>().read());
        let pre = loader.add(0x50).cast::<usize>().read() != 0;
        if CURRENT_UD.with(|c| c.get()) == loader as usize {
            let df = CURRENT_DF.with(|c| c.get());
            let mut lf = LOADER_FUNCS.lock().unwrap_or_else(|e| e.into_inner());
            if df != 0 && !lf.contains(&df) {
                lf.push(df);
                drop(lf);
                send(Event::Skipped { file_id: 0, reason: "learned GrTex2d loader decodeFunc" });
            }
        }
        send(Event::Load {
            file_id: CURRENT.with(|c| c.get()),
            dec: (dec.add(0x38).cast::<u32>().read(), dec.add(0x3C).cast::<u32>().read()),
            rec: (rec.add(0x48).cast::<u32>().read(), rec.add(0x4C).cast::<u32>().read()),
            prealloc: pre,
            loader_flags: loader.add(0x10).cast::<u32>().read(),
            rec_flags: rec.add(0x60).cast::<u32>().read(),
            dec_fmt: dec.add(0x44).cast::<u32>().read(),
            rec_fmt: rec.add(0x64).cast::<u32>().read(),
        });
        let bigger = (d.0 as u64 * d.1 as u64) > (r.0 as u64 * r.1 as u64);
        if pre && bigger {
            // Avoid the size assert for this load, and never send HD for this file again.
            loader.add(0x50).cast::<usize>().write(0);
            let id = CURRENT.with(|c| c.get());
            if id != 0 && !crate::reload::RELOADED.load(std::sync::atomic::Ordering::Relaxed) {
                let mut d = DENY.lock().unwrap_or_else(|e| e.into_inner());
                if d.get_or_insert_with(Default::default).insert(id) {
                    send(Event::Skipped { file_id: id, reason: "denied: fixed-size texture record" });
                }
            }
        }
        if bigger {
            send(Event::Header { dec: d, rec: r, prealloc: pre, dropped: pre && bigger });
        }
    }));
    unsafe { HEADER.get().unwrap().call(loader) }
}

unsafe extern "system" fn img_decode(decoder: *mut u8) -> u64 {
    let _ = std::panic::catch_unwind(|| unsafe {
        // decoder: +0x00 in ptr, +0x08 inAvail, +0x0C fileSize, +0x60 fileType (-1 = not detected yet)
        if !decoder.is_null() && decoder.add(0x60).cast::<i32>().read() == -1 {
            let input = decoder.cast::<*const u8>().read();
            let avail = decoder.add(8).cast::<u32>().read();
            if !input.is_null() && avail >= 4 {
                let mut m = [0u8; 4];
                std::ptr::copy_nonoverlapping(input, m.as_mut_ptr(), 4);
                send(Event::Decode { file_size: decoder.add(0xC).cast::<u32>().read(), magic: m });
            }
        }
    });
    unsafe { DECODE.get().unwrap().call(decoder) }
}

fn writer(path: PathBuf, rx: std::sync::mpsc::Receiver<Event>) {
    let Ok(f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) else { return };
    let mut w = std::io::BufWriter::new(f);
    let t0 = Instant::now();
    let mut seen: HashSet<u32> = HashSet::new();
    let mut last_flush = Instant::now();
    loop {
        match rx.recv_timeout(std::time::Duration::from_secs(1)) {
            Ok(Event::Archive { file_id, file_size, magic }) => {
                if seen.insert(file_id) {
                    let _ = writeln!(w, r#"{{"t":{:.3},"src":"archive","file_id":{file_id},"file_size":{file_size},"magic":"{}"}}"#, t0.elapsed().as_secs_f64(), magic_str(&magic));
                }
            }
            Ok(Event::Decode { file_size, magic }) => {
                let _ = writeln!(w, r#"{{"t":{:.3},"src":"img_decode","file_size":{file_size},"magic":"{}"}}"#, t0.elapsed().as_secs_f64(), magic_str(&magic));
            }
            Ok(Event::Replaced { file_id, original, bytes }) => {
                let _ = writeln!(w, r#"{{"t":{:.3},"src":"replaced","file_id":{file_id},"original_size":{original},"hd_size":{bytes}}}"#, t0.elapsed().as_secs_f64());
            }
            Ok(Event::Skipped { file_id, reason }) => {
                let _ = writeln!(w, r#"{{"t":{:.3},"src":"skipped","file_id":{file_id},"reason":"{reason}"}}"#, t0.elapsed().as_secs_f64());
            }
            Ok(Event::Header { dec, rec, prealloc, dropped }) => {
                let _ = writeln!(w, r#"{{"t":{:.3},"src":"header","decoded":[{},{}],"record":[{},{}],"prealloc":{prealloc},"dropped":{dropped}}}"#, t0.elapsed().as_secs_f64(), dec.0, dec.1, rec.0, rec.1);
            }
            Ok(Event::Fallback { file_id, original, bytes, ms }) => {
                let _ = writeln!(w, r#"{{"t":{:.3},"src":"fallback_2x","file_id":{file_id},"original_size":{original},"hd_size":{bytes},"ms":{ms}}}"#, t0.elapsed().as_secs_f64());
            }
            Ok(Event::Resize { up, cur, req, refused }) => {
                let _ = writeln!(w, r#"{{"t":{:.3},"src":"{}","cur":[{},{}],"req":[{},{}],"refused":{refused}}}"#, t0.elapsed().as_secs_f64(), if up { "upres" } else { "downres" }, cur.0, cur.1, req.0, req.1);
            }
            Ok(Event::Load { file_id, dec, rec, prealloc, loader_flags, rec_flags, dec_fmt, rec_fmt }) => {
                let _ = writeln!(w, r#"{{"t":{:.3},"src":"load","file_id":{file_id},"decoded":[{},{}],"record":[{},{}],"prealloc":{prealloc},"loader_flags":{loader_flags},"rec_flags":{rec_flags},"dec_fmt":{dec_fmt},"rec_fmt":{rec_fmt}}}"#, t0.elapsed().as_secs_f64(), dec.0, dec.1, rec.0, rec.1);
            }
            Ok(Event::Slow { file_id, prep_us, engine_us, bytes, cached, tid }) => {
                let _ = writeln!(w, r#"{{"t":{:.3},"src":"slow","file_id":{file_id},"prep_us":{prep_us},"engine_us":{engine_us},"bytes":{bytes},"cached":{cached},"tid":{tid}}}"#, t0.elapsed().as_secs_f64());
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(_) => break,
        }
        if last_flush.elapsed().as_secs() >= 1 {
            let _ = w.flush();
            last_flush = Instant::now();
        }
    }
}

fn install<F: retour::Function>(img: &Image, site: &Site, cell: &OnceLock<GenericDetour<F>>, hook: F) -> bool
where
    F: Copy,
{
    let Some(addr) = img.find(site) else {
        log!("{}: signature not found; hook disabled", site.name);
        return false;
    };
    let target: F = unsafe { F::from_ptr(addr as *const ()) };
    match unsafe { GenericDetour::new(target, hook) } {
        Ok(d) => {
            let _ = cell.set(d);
            match unsafe { cell.get().unwrap().enable() } {
                Ok(()) => {
                    log!("{}: hooked at rva {:#x}", site.name, img.rva(addr));
                    true
                }
                Err(e) => {
                    log!("{}: enable failed: {e}", site.name);
                    false
                }
            }
        }
        Err(e) => {
            log!("{}: detour failed: {e}", site.name);
            false
        }
    }
}

/// Pack file: `<id>.dds.zst` (zstd) or plain `<id>.dds`.
/// Non-square device textures go back to their native ATEX container.
/// The engine's DDS path converts device textures (decoded format != record
/// format, loader 6) and that conversion writes past the destination's level
/// table for non-square images (null write, Gw2-64 +0xb8b122); vanilla files
/// never take it. Uncompressed ATEX level chunks decode on the native path.
/// Square textures keep the DDS (proven on thousands of loads).
fn native_format(dds: Vec<u8>, head: &[u8]) -> Vec<u8> {
    if dds.len() < 128 || &dds[0..4] != b"DDS " || head.len() < 8 || &head[0..2] != b"AT" {
        return dds;
    }
    let rd = |o: usize| u32::from_le_bytes(dds[o..o + 4].try_into().unwrap());
    // BC7X has no DDS FourCC the engine knows: it always goes out as native ATEX.
    if rd(12) == rd(16) && &dds[84..88] != b"BC7X" {
        return dds;
    }
    let kind: [u8; 4] = head[0..4].try_into().unwrap();
    let fourcc: [u8; 4] = head[4..8].try_into().unwrap();
    match crate::atex::from_dds(&dds, kind, fourcc) {
        Some(a) => {
            N_ATEX.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            a
        }
        None => dds,
    }
}
/// Device textures served as native ATEX (non-square).
pub static N_ATEX: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Diagnostic (config `debug_tint=true`): every texture the mod replaces gets a flat
/// colour by origin, so anything keeping its normal look was NOT replaced.
pub static DEBUG_TINT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
pub const TINT_DEVICE: u16 = 0x07E0; // green: model texture from the pack
pub const TINT_CHAR4K: u16 = 0x001F; // blue: 4K model texture (local player)
pub const TINT_COMP2X: u16 = 0xF81F; // magenta: composite piece, 2x pack
pub const TINT_COMP4X: u16 = 0x07FF; // cyan: composite piece, 4x pack
pub const TINT_DOUBLED: u16 = 0xFD20; // orange: 4x piece doubled from the 2x pack
pub const TINT_RUNTIME: u16 = 0xFFE0; // yellow: no AI, plain runtime upscale

/// HD off: the pack buffers kept in RAM for serving are of no use until HD is on again, so they go
/// (keys 0..=6 are pack files; 7 and 8 are the runtime enlargements of originals, which HD off needs).
pub fn drop_hd_cache() {
    let mut g = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(c) = g.as_mut() {
        let keys: Vec<(u8, u32)> = c.map.keys().filter(|k| k.0 < 7).copied().collect();
        let mut freed = 0usize;
        for k in keys {
            if let Some(e) = c.map.remove(&k) {
                freed += e.0.len();
                c.bytes -= e.0.len();
            }
        }
        log!("RAM cache: HD buffers released ({} MB), {} MB kept", freed >> 20, c.bytes >> 20);
    }
}

/// Overlay switch: cached buffers carry the previous colors, so they are dropped.
pub fn set_debug_tint(on: bool) {
    DEBUG_TINT.store(on, std::sync::atomic::Ordering::Relaxed);
    *CACHE.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

fn debug_tint(dds: &mut [u8], rgb565: u16) {
    if DEBUG_TINT.load(std::sync::atomic::Ordering::Relaxed) {
        tint_dds(dds, rgb565);
    }
}

/// Sets every DXT colour block of a DDS to one colour; alpha and non-colour formats
/// (masks, normals) stay as they are.
fn tint_dds(dds: &mut [u8], rgb565: u16) {
    if dds.len() < 128 || &dds[0..4] != b"DDS " {
        return;
    }
    if &dds[84..88] == b"BC7X" {
        let blk = bc7_solid_block(rgb565);
        for b in dds[128..].chunks_exact_mut(16) {
            b.copy_from_slice(&blk);
        }
        return;
    }
    let (block, colour_at) = match &dds[84..88] {
        b"DXT1" => (8, 0),
        b"DXT3" | b"DXT4" | b"DXT5" => (16, 8),
        _ => return,
    };
    let c = rgb565.to_le_bytes();
    for b in dds[128..].chunks_exact_mut(block) {
        b[colour_at..colour_at + 8].copy_from_slice(&[c[0], c[1], c[0], c[1], 0, 0, 0, 0]);
    }
}

/// One BC7 block (mode 6) that decodes to a single opaque colour in all 16 texels.
fn bc7_solid_block(rgb565: u16) -> [u8; 16] {
    let (r, g, b) = ((rgb565 >> 11) & 31, (rgb565 >> 5) & 63, rgb565 & 31);
    let c8 = [(r << 3 | r >> 2) as u128, (g << 2 | g >> 4) as u128, (b << 3 | b >> 2) as u128, 255u128];
    let mut v: u128 = 0x40; // mode 6: six zero bits, then a one
    let mut at = 7;
    for ch in c8 {
        for _ in 0..2 {
            v |= (ch >> 1) << at; // both endpoints equal
            at += 7;
        }
    }
    // p-bits (bits 63, 64) add the low bit of every channel of each endpoint: 1 keeps alpha at 255
    // (the colour moves by at most 1, which is fine for a debug tint).
    v |= 1u128 << 63 | 1u128 << 64;
    // indices stay 0: every texel is endpoint 0
    v.to_le_bytes()
}

#[cfg(test)]
mod tint_tests {
    #[test]
    fn bc7_solid_block_decodes_to_the_colour() {
        let blk = super::bc7_solid_block(0xF81F);
        let mut px = vec![0u32; 16];
        texture2ddecoder::decode_bc7(&blk, 4, 4, &mut px).unwrap();
        for p in px {
            let (r, g, b, a) = ((p >> 16) & 255, (p >> 8) & 255, p & 255, p >> 24);
            assert!(r >= 247 && g <= 1 && b >= 247 && a == 255, "{r} {g} {b} {a}");
        }
    }

    #[test]
    fn colours_blocks_and_keeps_alpha() {
        for (fcc, block, at) in [(b"DXT1", 8usize, 0usize), (b"DXT5", 16, 8)] {
            let mut d = vec![0xABu8; 128 + block * 3];
            d[0..4].copy_from_slice(b"DDS ");
            d[84..88].copy_from_slice(fcc);
            super::tint_dds(&mut d, 0xF81F);
            for b in d[128..].chunks_exact(block) {
                assert_eq!(&b[at..at + 8], &[0x1F, 0xF8, 0x1F, 0xF8, 0, 0, 0, 0]);
                assert!(b[..at].iter().all(|&x| x == 0xAB));
            }
        }
        // masks and normals untouched
        let mut d = vec![0xABu8; 128 + 16];
        d[0..4].copy_from_slice(b"DDS ");
        d[84..88].copy_from_slice(b"ATI2");
        let before = d.clone();
        super::tint_dds(&mut d, 0xF81F);
        assert_eq!(d, before);
    }
}

/// One archive file of a pack, opened on first use: an open can mean an antivirus scan,
/// so it never happens at startup or for a volume this session does not need.
pub struct Volume {
    path: PathBuf,
    file: OnceLock<Option<std::fs::File>>,
}

impl Volume {
    fn file(&self) -> Option<&std::fs::File> {
        self.file
            .get_or_init(|| {
                use std::os::windows::fs::OpenOptionsExt;
                const FILE_FLAG_RANDOM_ACCESS: u32 = 0x1000_0000;
                let f = std::fs::OpenOptions::new().read(true).custom_flags(FILE_FLAG_RANDOM_ACCESS).open(&self.path);
                if let Err(e) = &f {
                    log!("{}: {e}", self.path.display());
                }
                f.ok()
            })
            .as_ref()
    }
}

/// Where an HD file lives: inside one of the pack's archive volumes, or loose while the
/// batch is still producing it.
pub enum PackLoc {
    Archive { volume: Arc<Volume>, offset: u64, len: u32 },
    Loose(PathBuf),
}

fn unzstd(z: &[u8]) -> Option<Vec<u8>> {
    // One exact-size allocation (the frame header carries the size).
    match zstd::zstd_safe::get_frame_content_size(z) {
        Ok(Some(n)) if n <= 1 << 30 => zstd::bulk::decompress(z, n as usize).ok(),
        _ => zstd::stream::decode_all(z).ok(),
    }
}

/// Positional read: threads share the handle without a shared cursor.
fn read_at(file: &std::fs::File, offset: u64, len: u32) -> Option<Vec<u8>> {
    use std::os::windows::fs::FileExt;
    let mut buf = vec![0u8; len as usize];
    let mut done = 0usize;
    while done < buf.len() {
        match file.seek_read(&mut buf[done..], offset + done as u64) {
            Ok(0) | Err(_) => return None,
            Ok(n) => done += n,
        }
    }
    Some(buf)
}

fn read_pack_file(loc: &PackLoc) -> Option<Vec<u8>> {
    match loc {
        PackLoc::Archive { volume, offset, len } => unzstd(&read_at(volume.file()?, *offset, *len)?),
        // Packs built before zstd have a plain `<id>.dds` instead.
        PackLoc::Loose(path) => match std::fs::read(path) {
            Ok(z) => unzstd(&z),
            Err(_) => std::fs::read(path.with_extension("")).ok(),
        },
    }
}

/// `"key":N` / `"key": N` in one index line. No allocation: runs 165k times at startup.
fn index_num(line: &str, key: &str) -> Option<u64> {
    let rest = &line[line.find(key)? + key.len()..];
    let rest = rest.trim_start();
    let end = rest.bytes().position(|c| !c.is_ascii_digit()).unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// Reads only the index: it lists what the batch finished, so no pack file is touched
/// here. Opening each one (`Path::exists` does) cost minutes with the disk busy and an
/// antivirus scanning every open; a file that is really missing just fails when served.
fn load_pack(dir: &std::path::Path) -> std::collections::HashMap<u32, (PackLoc, u64, u32)> {
    let mut m = match std::fs::read_to_string(dir.join("index.jsonl")) {
        Ok(s) => parse_index(&s, dir),
        Err(_) => Default::default(),
    };
    load_archive(dir, &mut m);
    m
}

const ARCHIVE_MAGIC: &[u8; 8] = b"GW2RPAK2";
const ARCHIVE_RECORD: usize = 28;

/// `pack.idx` + `pack_NNNNN.bin` (tools/batch/pack_archive.py). An archived texture wins
/// over a loose one: a loose file for it means a redo the tool has not added yet.
fn load_archive(dir: &std::path::Path, m: &mut std::collections::HashMap<u32, (PackLoc, u64, u32)>) {
    let Ok(idx) = std::fs::read(dir.join("pack.idx")) else { return };
    if idx.len() < 8 || &idx[..8] != ARCHIVE_MAGIC || (idx.len() - 8) % ARCHIVE_RECORD != 0 {
        log!("{}: bad archive index, using loose files", dir.display());
        return;
    }
    let mut volumes: std::collections::HashMap<u32, Option<(Arc<Volume>, u64)>> = Default::default();
    let mut skipped = 0u32;
    for r in idx[8..].chunks_exact(ARCHIVE_RECORD) {
        let u32_at = |o: usize| u32::from_le_bytes(r[o..o + 4].try_into().unwrap());
        let (id, vol, len) = (u32_at(0), u32_at(4), u32_at(8));
        let offset = u64::from_le_bytes(r[20..28].try_into().unwrap());
        let entry = volumes.entry(vol).or_insert_with(|| {
            let path = dir.join(format!("pack_{vol:05}.bin"));
            let size = std::fs::metadata(&path).ok()?.len();
            Some((Arc::new(Volume { path, file: OnceLock::new() }), size))
        });
        match entry {
            Some((volume, size)) if offset + len as u64 <= *size => {
                m.insert(id, (PackLoc::Archive { volume: volume.clone(), offset, len }, u32_at(12) as u64, u32_at(16)));
            }
            _ => skipped += 1,
        }
    }
    log!("{}: {} archive volumes, {} textures outside them", dir.display(), volumes.len(), skipped);
}

fn parse_index(text: &str, dir: &std::path::Path) -> std::collections::HashMap<u32, (PackLoc, u64, u32)> {
    let mut m = std::collections::HashMap::with_capacity(text.len() / 150);
    for l in text.lines() {
        if let Some(id) = index_num(l, "\"file_id\":") {
            let path = PackLoc::Loose(dir.join(format!("{id}.dds.zst")));
            m.insert(id as u32, (path, index_num(l, "\"src_bytes\":").unwrap_or(0), index_num(l, "\"width\":").unwrap_or(0) as u32));
        }
    }
    m
}

#[cfg(test)]
mod index_tests {
    #[test]
    fn parses_lines_and_skips_bad_ones() {
        let t = concat!(
            "{\"file_id\":14199,\"width\":1024,\"height\":1024,\"src_bytes\":126380}\n",
            "{\"file_id\": 7, \"width\": 64}\n",
            "{\"file_i\n",
            "{\"file_id\":71179,\"width\":512"
        );
        let m = super::parse_index(t, std::path::Path::new("x"));
        assert_eq!(m.len(), 3);
        assert_eq!(m[&14199].1, 126380);
        assert_eq!(m[&14199].2, 1024);
        assert!(matches!(&m[&7].0, super::PackLoc::Loose(p) if p == &std::path::Path::new("x").join("7.dds.zst")));
        assert_eq!((m[&7].1, m[&7].2), (0, 64));
        assert_eq!(m[&71179].2, 512);
    }

    #[test]
    fn archive_overrides_loose_and_reads_back() {
        let dir = std::env::temp_dir().join(format!("gw2rem_arch_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
        let b: Vec<u8> = (0..900u32).map(|i| (i % 13) as u8).collect();
        let (za, zb) = (zstd::bulk::compress(&a, 3).unwrap(), zstd::bulk::compress(&b, 3).unwrap());
        // id 1 and 2 archived in two volumes; id 3 only loose (batch still running); a
        // record past the end of its volume and one in a missing volume are ignored.
        std::fs::write(dir.join("pack_00000.bin"), &za).unwrap();
        std::fs::write(dir.join("pack_00001.bin"), [b"xx".as_slice(), &zb].concat()).unwrap();
        let mut idx = super::ARCHIVE_MAGIC.to_vec();
        for (id, vol, len, src, w, off) in [
            (1u32, 0u32, za.len() as u32, 77u32, 512u32, 0u64),
            (2, 1, zb.len() as u32, 88, 256, 2),
            (9, 0, 10, 0, 0, 1 << 40),
            (10, 5, 10, 0, 0, 0),
        ] {
            for v in [id, vol, len, src, w] {
                idx.extend_from_slice(&v.to_le_bytes());
            }
            idx.extend_from_slice(&off.to_le_bytes());
        }
        std::fs::write(dir.join("pack.idx"), idx).unwrap();
        std::fs::write(dir.join("3.dds.zst"), &zb).unwrap();
        std::fs::write(dir.join("index.jsonl"), "{\"file_id\":1,\"width\":1}\n{\"file_id\":3,\"width\":64}\n").unwrap();
        let m = super::load_pack(&dir);
        assert_eq!(m.len(), 3);
        assert!(matches!(m[&1].0, super::PackLoc::Archive { .. }));
        assert_eq!((m[&1].1, m[&1].2), (77, 512));
        assert!(matches!(m[&3].0, super::PackLoc::Loose(_)));
        assert_eq!(super::read_pack_file(&m[&1].0).unwrap(), a);
        assert_eq!(super::read_pack_file(&m[&2].0).unwrap(), b);
        assert_eq!(super::read_pack_file(&m[&3].0).unwrap(), b);
        drop(m);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Real pack vs its archive (`GW2REM_TEST_ARCHIVE=<dir with both>`, `GW2REM_TEST_LOOSE=<pack>`).
    #[test]
    fn real_archive_matches_loose() {
        let (Ok(a), Ok(l)) = (std::env::var("GW2REM_TEST_ARCHIVE"), std::env::var("GW2REM_TEST_LOOSE")) else { return };
        let m = super::load_pack(std::path::Path::new(&a));
        let mut checked = 0;
        for (id, e) in &m {
            let loose = super::PackLoc::Loose(std::path::Path::new(&l).join(format!("{id}.dds.zst")));
            assert!(matches!(e.0, super::PackLoc::Archive { .. }), "{id} not archived");
            assert_eq!(super::read_pack_file(&e.0), super::read_pack_file(&loose), "{id}");
            checked += 1;
        }
        println!("{checked} textures identical");
        assert!(checked > 0);
    }

    /// `GW2REM_TEST_INDEX=<index.jsonl> cargo test --release -- --nocapture real_index`
    #[test]
    fn real_index_speed() {
        let Ok(p) = std::env::var("GW2REM_TEST_INDEX") else { return };
        let t0 = std::time::Instant::now();
        let s = std::fs::read_to_string(&p).unwrap();
        let t1 = t0.elapsed();
        let m = super::parse_index(&s, std::path::Path::new(&p).parent().unwrap());
        println!("{} textures: read {:?}, parsed {:?}", m.len(), t1, t0.elapsed() - t1);
    }
}

pub fn start(img: &Image, data_dir: &std::path::Path) {
    let pack_dir = crate::overlay::pack_dir(data_dir);
    log!("pack dir: {}", pack_dir.display());
    let t0 = Instant::now();
    let pack = load_pack(&pack_dir);
    log!("HD pack: {} textures ({} ms)", pack.len(), t0.elapsed().as_millis());
    crate::overlay::PACK_SIZE.store(pack.len() as u32, std::sync::atomic::Ordering::Relaxed);
    let _ = PACK.set(pack);
    let pack4 = load_pack(&pack_dir.join("composite_4x"));
    log!("HD pack 4x (composite): {} textures", pack4.len());
    let _ = PACK4.set(pack4);
    let char4k = load_pack(&pack_dir.join("character_4k"));
    log!("HD pack 4K (model textures, local player): {} textures", char4k.len());
    let _ = PACK_CHAR4K.set(char4k);
    // composite_ids.txt: one file id per line (written by the pack builder).
    let comp: std::collections::HashSet<u32> = std::fs::read_to_string(pack_dir.join("composite_ids.txt"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect();
    log!("composite sources known: {}", comp.len());
    // composite 2x needs the pack folder present (opt-in), the id list is informative only
    let composite_ok = pack_dir.is_dir() && crate::composite::install(img);
    if !composite_ok {
        log!("composite 2x disabled: composite pieces are NOT replaced (pack textures for them are skipped)");
    }
    COMPOSITE_OK.store(composite_ok, std::sync::atomic::Ordering::Relaxed);
    crate::overlay::COMPOSITE_2X.store(composite_ok, std::sync::atomic::Ordering::Relaxed);
    let _ = COMPOSITE.set(comp);
    let dev: std::collections::HashSet<u32> = std::fs::read_to_string(pack_dir.join("device_ids.txt"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect();
    log!("device textures allowed (paired): {}", dev.len());
    let _ = DEVICE.set(dev);
    if let Ok(b) = std::fs::read(pack_dir.join("reasons.bin")) {
        if b.len() >= 12 && &b[..8] == b"GW2RSN1\0" {
            let _ = REASONS.set(b[12..].to_vec());
        }
    }
    if let Ok(t) = std::fs::read_to_string(pack_dir.join("pair_skip.txt")) {
        let set: HashSet<u32> = t.lines().filter_map(|l| l.trim().parse().ok()).collect();
        log!("pair_skip: {} textures stay original (half-served groups)", set.len());
        let _ = PAIR_SKIP.set(set);
    }
    if let Ok(t) = std::fs::read_to_string(pack_dir.join("terrain_ids.txt")) {
        let set: HashSet<u32> = t.lines().filter_map(|l| l.trim().parse().ok()).collect();
        log!("terrain: {} upscaled tiles in the pack (served only with terrain_hd=true)", set.len());
        let _ = TERRAIN.set(set);
    }
    // Warm start from the previous session's list (HD on, no debug colors in the cache).
    let hot_path = data_dir.join("hot_textures.bin");
    if crate::overlay::enabled() && !DEBUG_TINT.load(std::sync::atomic::Ordering::Relaxed) {
        if let Ok(b) = std::fs::read(&hot_path) {
            let list = hot_decode(&b);
            log!("prefetch: {} files from the previous session", list.len());
            *HOT.lock().unwrap_or_else(|e| e.into_inner()) = list.clone();
            HOT_SEEN.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(Default::default).extend(list.iter().map(|e| (e.0, e.1)));
            let _ = std::thread::Builder::new().name("gw2remastered-prefetch".into()).spawn(move || prefetch(list));
        }
    }
    let _ = std::thread::Builder::new().name("gw2remastered-hot".into()).spawn(move || hot_saver(hot_path));
    let (tx, rx) = channel();
    let _ = TX.set(std::sync::Mutex::new(tx));
    let path = data_dir.join("diag_files.jsonl");
    let _ = std::thread::Builder::new().name("gw2remastered-diag".into()).spawn(move || writer(path, rx));
    install(img, &BLOCK_CB, &BLOCK, block_cb as BlockCb);
    install(img, &IMG_DECODE, &DECODE, img_decode as ImgDecode);
    // Seed the known texture-loader decodeFunc so composite reads are recognised
    // from the very first file (learning at runtime stays as a cross-check).
    match img.find(&TEX_DECODE_ADAPTER) {
        Some(a) => {
            LOADER_FUNCS.lock().unwrap_or_else(|e| e.into_inner()).push(a);
            log!("{}: at rva {:#x}", TEX_DECODE_ADAPTER.name, img.rva(a));
        }
        None => log!("{}: signature not found; relying on runtime learning", TEX_DECODE_ADAPTER.name),
    }
    install(img, &TEX_HEADER, &HEADER, tex_header as TexHeader);
    crate::resize::install(img);
}

#[cfg(test)]
mod pair_skip_tests {
    #[test]
    fn listed_textures_are_denied() {
        let _ = super::PAIR_SKIP.set([5u32, 9].into_iter().collect());
        assert!(super::denied(5) && super::denied(9));
        assert!(!super::denied(6));
    }
}
