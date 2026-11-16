//! v19: the fur shaders made ahead of time. The game's D3D11 device is single-threaded, so the fur's per-vertex-layout shader objects
//! (the uv / z-stamp pixel shaders and the shell geometry shaders) can only be created on the render thread, and the driver takes
//! tens of ms for each: every new kind of character on screen was
//! a stutter. The compiled bytecode of every layout seen is kept on disk (`cache\fur-shaders.bin`, invalidated by a change of the
//! shader sources); at start it fills the compile registries, and the objects are created where a stall does not show: on frames the
//! scene is not drawn (loading screens, the character select), a few per frame.

use std::path::PathBuf;
use std::sync::Mutex;

use crate::d3d::Obj;
use crate::log;

const MAGIC: &[u8; 8] = b"GW2FURS1";
/// Objects made per frame on a loading screen.
pub const PER_FRAME: usize = 4;
/// Entries kept in the file.
const MAX_ENTRIES: usize = 512;

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    /// 0 = a geometry shader set (shell, measure, uv-space, triangle capture), 1 = a furmask pixel shader.
    pub kind: u8,
    pub key: String,
    pub blobs: Vec<Vec<u8>>,
}

static ENTRIES: Mutex<Vec<Entry>> = Mutex::new(Vec::new());
static LOADED: std::sync::Once = std::sync::Once::new();

fn fnv(h: u64, b: &[u8]) -> u64 {
    b.iter().fold(h, |h, &x| (h ^ x as u64).wrapping_mul(0x100_0000_01b3))
}

/// The version of the generated shaders: the sources they are built from.
fn version() -> u64 {
    fnv(fnv(0xcbf2_9ce4_8422_2325, crate::shellfur::HLSL.as_bytes()), crate::furmask::HLSL.as_bytes())
}

fn path() -> Option<PathBuf> {
    crate::grading::dir().map(|d| d.join("cache").join("fur-shaders.bin"))
}

pub fn encode(version: u64, entries: &[Entry]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(MAGIC);
    v.extend_from_slice(&version.to_le_bytes());
    v.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for e in entries {
        v.push(e.kind);
        v.extend_from_slice(&(e.key.len() as u32).to_le_bytes());
        v.extend_from_slice(e.key.as_bytes());
        v.extend_from_slice(&(e.blobs.len() as u32).to_le_bytes());
        for b in &e.blobs {
            v.extend_from_slice(&(b.len() as u32).to_le_bytes());
            v.extend_from_slice(b);
        }
    }
    v
}

/// The entries of a file of this `version` (empty: another version, or damaged).
pub fn decode(version: u64, b: &[u8]) -> Vec<Entry> {
    fn take<'a>(b: &mut &'a [u8], n: usize) -> Option<&'a [u8]> {
        if b.len() < n {
            return None;
        }
        let (h, t) = b.split_at(n);
        *b = t;
        Some(h)
    }
    fn u32_of(b: &mut &[u8]) -> Option<usize> {
        take(b, 4).map(|x| u32::from_le_bytes(x.try_into().unwrap()) as usize)
    }
    let mut b = b;
    let parse = |b: &mut &[u8]| -> Option<Vec<Entry>> {
        if take(b, 8)? != MAGIC || u64::from_le_bytes(take(b, 8)?.try_into().ok()?) != version {
            return None;
        }
        let n = u32_of(b)?.min(MAX_ENTRIES);
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            let kind = take(b, 1)?[0];
            let kl = u32_of(b)?;
            let key = String::from_utf8(take(b, kl)?.to_vec()).ok()?;
            let nb = u32_of(b)?;
            if nb > 8 {
                return None;
            }
            let mut blobs = Vec::new();
            for _ in 0..nb {
                let l = u32_of(b)?;
                blobs.push(take(b, l)?.to_vec());
            }
            out.push(Entry { kind, key, blobs });
        }
        Some(out)
    };
    parse(&mut b).unwrap_or_default()
}

/// Reads the file once and hands its shaders to the registries (no compile needed for them this session).
pub fn load() {
    LOADED.call_once(|| {
        let Some(p) = path() else { return };
        let Ok(b) = std::fs::read(&p) else { return };
        let list = decode(version(), &b);
        if list.is_empty() {
            return;
        }
        let (mut gs, mut ps) = (0, 0);
        for e in &list {
            match e.kind {
                0 if e.blobs.len() == 4 => {
                    crate::shellfur::preload_gs(&e.key, &e.blobs);
                    gs += 1;
                }
                1 if e.blobs.len() == 1 => {
                    crate::furmask::preload_ps(&e.key, &e.blobs[0]);
                    ps += 1;
                }
                _ => {}
            }
        }
        log!("furwarm: {gs} geometry shader sets and {ps} pixel shaders from the previous sessions");
        // Warm them all in the background (the driver's translation, on our own device: warm.rs) once the game's device is known.
        let warm = list.clone();
        let _ = std::thread::Builder::new().name("gw2g-fur-warm".into()).spawn(move || {
            for _ in 0..600 {
                if crate::warm::known() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
            let t = std::time::Instant::now();
            for e in &warm {
                match e.kind {
                    0 => e.blobs.iter().take(3).for_each(|b| crate::warm::geometry(b)),
                    _ => e.blobs.iter().for_each(|b| crate::warm::pixel(b)),
                }
            }
            log!("furwarm: {} shaders warmed in {} ms", warm.len(), t.elapsed().as_millis());
        });
        *ENTRIES.lock().unwrap_or_else(|e| e.into_inner()) = list;
    });
}

/// Remembers a freshly compiled shader (a background thread), and saves the file.
pub fn remember(kind: u8, key: &str, blobs: Vec<Vec<u8>>) {
    let bytes = {
        let mut g = ENTRIES.lock().unwrap_or_else(|e| e.into_inner());
        if g.iter().any(|e| e.kind == kind && e.key == key) || g.len() >= MAX_ENTRIES {
            return;
        }
        g.push(Entry { kind, key: key.to_string(), blobs });
        encode(version(), &g)
    };
    let Some(p) = path() else { return };
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let tmp = p.with_extension("tmp");
    if std::fs::write(&tmp, bytes).is_ok() {
        let _ = std::fs::rename(&tmp, &p);
    }
}

static DEFERRED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// A content-rule draw needed a shader object that is left for the next frame without the scene.
pub fn defer() {
    DEFERRED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// A frame the scene is not drawn on (a loading screen, the character select): makes a few of the known shaders' objects.
pub fn on_idle_frame(ctx: Obj) {
    if ctx.is_null() {
        return;
    }
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        // The device (and its adapter, for the warm-up's own device) is known from the first frame on.
        let mut dev: Obj = std::ptr::null_mut();
        let get_device: unsafe extern "system" fn(Obj, *mut Obj) = std::mem::transmute(crate::d3d::vfn(ctx, crate::d3d::vt::CHILD_GET_DEVICE));
        get_device(ctx, &mut dev);
        if !dev.is_null() {
            crate::warm::note_device(dev);
            crate::d3d::release(dev);
        }
    }));
    load();
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        let mut left = PER_FRAME;
        left -= crate::shellfur::prewarm(ctx, left);
        if left > 0 {
            crate::furmask::prewarm(ctx, left);
        }
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fur_shader_cache_round_trips_and_a_new_shader_version_discards_it() {
        let e = vec![
            Entry { kind: 0, key: "TEXCOORD0@o0:3f|0xy|face".into(), blobs: vec![vec![1, 2, 3], vec![4], vec![], vec![5, 6]] },
            Entry { kind: 1, key: "uv|a".into(), blobs: vec![vec![9; 300]] },
        ];
        let b = encode(7, &e);
        assert_eq!(decode(7, &b), e);
        assert!(decode(8, &b).is_empty(), "other shader sources: rebuilt");
        assert!(decode(7, &b[..b.len() - 1]).is_empty(), "a damaged file is ignored");
    }
}
