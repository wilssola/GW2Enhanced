//! Pixel-shader registry and substitution mechanism.
//!
//! The registry maps the `ID3D11PixelShader*` (any device's: keyed by the shader pointer only) to the FNV-1a 32 of the bytecode it was
//! created from (the `ps#xxxxxxxx` hash of the frame log). `Substitutes` holds replacement HLSL per
//! hash, compiled and created lazily the first time the hashed shader is bound while substitution
//! is on. Nothing here turns substitution on; `d3d.rs` applies the decisions.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};

use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW};

use crate::log;

/// FNV-1a 32 (offset basis 2166136261, prime 16777619).
pub fn fnv1a32(bytes: &[u8]) -> u32 {
    bytes.iter().fold(2166136261u32, |h, &b| (h ^ b as u32).wrapping_mul(16777619))
}

/// The game has about 1000 pixel shaders; this bounds a runaway producer.
const MAX_REGISTRY: usize = 8192;

#[derive(Default)]
struct RegistryInner {
    map: HashMap<usize, u32>,
}

pub struct Registry {
    inner: Mutex<RegistryInner>,
}

impl Registry {
    pub fn new() -> Self {
        Registry { inner: Mutex::new(RegistryInner::default()) }
    }

    /// Remembers the hash of `bytecode` for `shader_ptr`; a reused pointer is overwritten.
    pub fn record(&self, shader_ptr: usize, bytecode: &[u8]) {
        let h = fnv1a32(bytecode);
        let Ok(mut m) = self.inner.lock() else { return };
        if m.map.len() >= MAX_REGISTRY && !m.map.contains_key(&shader_ptr) {
            return;
        }
        m.map.insert(shader_ptr, h);
    }

    pub fn hash_of(&self, shader_ptr: usize) -> Option<u32> {
        self.inner.lock().ok()?.map.get(&shader_ptr).copied()
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

static REGISTRY: OnceLock<Registry> = OnceLock::new();

pub fn registry() -> &'static Registry {
    REGISTRY.get_or_init(Registry::new)
}

static VS_REGISTRY: OnceLock<Registry> = OnceLock::new();

/// Vertex shader pointer -> bytecode hash (draw diagnostic only; filled by the `CreateVertexShader` hook).
pub fn vs_registry() -> &'static Registry {
    VS_REGISTRY.get_or_init(Registry::new)
}

// ---- substitution switch ---------------------------------------------------------------------

static ENABLED: AtomicBool = AtomicBool::new(false);
static UNAVAILABLE: AtomicBool = AtomicBool::new(false);

/// Turns substitution on or off (default off). Has no effect once the mechanism is unavailable.
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Relaxed);
}

pub fn substitution_enabled() -> bool {
    ENABLED.load(Relaxed) && !UNAVAILABLE.load(Relaxed)
}

/// The `PSSetShader` hook could not be installed: substitution stays off for good.
pub fn mark_unavailable() {
    UNAVAILABLE.store(true, Relaxed);
}

/// Substitution is only possible when the `PSSetShader` patch took.
pub fn mechanism_usable(ps_set_shader_patched: bool) -> bool {
    ps_set_shader_patched
}

/// The shader `PSSetShader` should bind: the substitute when enabled and one exists.
pub fn choose(enabled: bool, substitute: Option<usize>, original: usize) -> usize {
    if enabled { substitute.unwrap_or(original) } else { original }
}

// ---- substitutes -----------------------------------------------------------------------------

enum State {
    Pending,
    Creating,
    /// Created on `device` as `ptr`.
    Created(usize, usize),
    /// Creation failed on `device`.
    Failed(usize),
}

struct Entry {
    hlsl: String,
    entry: String,
    state: State,
    /// The previously created shader (device, pointer) of an entry that was re-installed: served
    /// until the new source is created, kept if that fails, handed back for release when replaced.
    old: Option<(usize, usize)>,
    /// Bumped by every install; a creation that finishes after a newer install is stale.
    generation: u64,
}

/// First sighting of `hash` in `seen`: records it and returns true; false when already there.
pub fn first_time(seen: &mut Vec<u32>, hash: u32) -> bool {
    if seen.contains(&hash) {
        return false;
    }
    seen.push(hash);
    true
}

static APPLIED: Mutex<Vec<u32>> = Mutex::new(Vec::new());

/// True the first time a substitute for `hash` was bound (log once).
pub fn note_applied(hash: u32) -> bool {
    APPLIED.lock().map_or(false, |mut s| first_time(&mut s, hash))
}

pub struct Substitutes {
    map: Mutex<HashMap<u32, Entry>>,
}

impl Substitutes {
    pub fn new() -> Self {
        Substitutes { map: Mutex::new(HashMap::new()) }
    }

    /// Installs (or re-installs) the replacement for `hash`. A re-install keeps the previously
    /// created shader as the one served until the new source has been created.
    pub fn install(&self, hash: u32, hlsl: &str, entry: &str) {
        if let Ok(mut m) = self.map.lock() {
            let (old, generation) = match m.get(&hash) {
                Some(e) => (
                    match e.state {
                        State::Created(d, p) => Some((d, p)),
                        _ => e.old,
                    },
                    e.generation + 1,
                ),
                None => (None, 0),
            };
            m.insert(hash, Entry { hlsl: hlsl.to_string(), entry: entry.to_string(), state: State::Pending, old, generation });
        }
    }

    /// Entries whose (re-)installed source is not created yet (still served by the previous shader or the original).
    pub fn pending_count(&self) -> usize {
        self.map.lock().map_or(0, |m| m.values().filter(|e| matches!(e.state, State::Pending | State::Creating)).count())
    }

    /// Forgets the replacement for `hash` (the original runs again). False when none was installed.
    pub fn remove(&self, hash: u32) -> bool {
        self.map.lock().map(|mut m| m.remove(&hash).is_some()).unwrap_or(false)
    }

    /// Creation of the replacement for `hash` failed (and no earlier version is being served).
    pub fn failed(&self, hash: u32) -> bool {
        self.map.lock().ok().is_some_and(|m| m.get(&hash).is_some_and(|e| matches!(e.state, State::Failed(_)) && e.old.is_none()))
    }

    /// The created replacement shader pointer, if any (never compiles).
    #[allow(dead_code)] // interface; the hook uses get_or_create
    pub fn get(&self, hash: u32) -> Option<usize> {
        let m = self.map.lock().ok()?;
        let e = m.get(&hash)?;
        match e.state {
            State::Created(_, p) => Some(p),
            _ => e.old.map(|(_, p)| p),
        }
    }

    /// Like `get`, but for `device`: creates the shader from the stored source with
    /// `create(hlsl, entry)` when none exists for that device. An entry made on another device is
    /// treated as absent and forgotten without being touched (that device may be gone). The lock is
    /// not held while `create` runs; a failure is logged once per device and never retried.
    #[allow(dead_code)] // the hook uses get_or_create_with; kept for callers that release nothing
    pub fn get_or_create(&self, device: usize, hash: u32, create: impl FnOnce(&str, &str) -> Result<usize, String>) -> Option<usize> {
        self.get_or_create_with(device, hash, create, |_| {})
    }

    /// Like `get`, but for `device`: creates the shader from the stored source with
    /// `create(hlsl, entry)` when none exists for that device. An entry made on another device is
    /// treated as absent and forgotten without being touched (that device may be gone). The lock is
    /// not held while `create` or `release` run; a failure is logged once per device and never retried.
    ///
    /// A re-installed entry keeps serving its previous shader (same device) while the new one is
    /// being created and when that fails (it is never left `Failed` while a good one exists). When
    /// the new shader is in place the previous one is passed to `release` exactly once, after the
    /// lock is dropped; a stale creation (a newer install happened meanwhile) is released instead.
    pub fn get_or_create_with(&self, device: usize, hash: u32, create: impl FnOnce(&str, &str) -> Result<usize, String>, release: impl Fn(usize)) -> Option<usize> {
        let (hlsl, entry, generation, old) = {
            let mut m = self.map.lock().ok()?;
            let e = m.get_mut(&hash)?;
            let old = e.old.filter(|(d, _)| *d == device).map(|(_, p)| p);
            match e.state {
                State::Created(d, p) if d == device => return Some(p),
                State::Failed(d) if d == device => return old,
                State::Creating => return old,
                _ => {}
            }
            e.state = State::Creating;
            (e.hlsl.clone(), e.entry.clone(), e.generation, old)
        };
        let result = create(&hlsl, &entry);
        let (ret, to_release) = {
            let mut m = self.map.lock().ok()?;
            let e = m.get_mut(&hash)?;
            if e.generation != generation {
                // Re-installed while compiling: this result is for the old source.
                if matches!(e.state, State::Creating) {
                    e.state = State::Pending;
                }
                (old, result.ok())
            } else {
                match result {
                    Ok(p) => {
                        e.state = State::Created(device, p);
                        e.old = None;
                        log!("shaders: substitute for ps#{hash:08x} created ({p:#x})");
                        (Some(p), old)
                    }
                    Err(msg) => {
                        match old {
                            Some(o) => {
                                e.state = State::Created(device, o);
                                e.old = None;
                                log!("shaders: new substitute for ps#{hash:08x} failed, previous one kept: {msg}");
                            }
                            None => {
                                e.state = State::Failed(device);
                                log!("shaders: substitute for ps#{hash:08x} failed, original kept: {msg}");
                            }
                        }
                        (old, None)
                    }
                }
            }
        };
        if let Some(p) = to_release {
            release(p);
        }
        ret
    }
}

impl Default for Substitutes {
    fn default() -> Self {
        Self::new()
    }
}

static SUBSTITUTES: OnceLock<Substitutes> = OnceLock::new();

pub fn substitutes() -> &'static Substitutes {
    SUBSTITUTES.get_or_init(Substitutes::new)
}

/// Stores replacement HLSL for the pixel shader with bytecode hash `hash`. Compiled lazily.
pub fn install_substitute(hash: u32, hlsl: &str, entry: &str) {
    substitutes().install(hash, hlsl, entry);
    precompile(hlsl, entry);
}

type CompileKey = (String, String);
static PRECOMPILED: Mutex<Vec<(CompileKey, Result<Vec<u8>, String>)>> = Mutex::new(Vec::new());

/// Compiles `hlsl` on a background thread so the render thread does not stall on `D3DCompile` when
/// the shader is first bound (a slider release re-installs substitutes while the game is drawing).
fn precompile(hlsl: &str, entry: &str) {
    let key: CompileKey = (hlsl.to_string(), entry.to_string());
    if PRECOMPILED.lock().is_ok_and(|c| c.iter().any(|(k, _)| *k == key)) {
        return;
    }
    let _ = std::thread::Builder::new().name("gw2g-compile".into()).spawn(move || {
        let r = compile_ps(&key.0, &key.1);
        if let Ok(mut c) = PRECOMPILED.lock() {
            if c.len() >= 8 {
                let _ = c.remove(0);
            }
            c.push((key, r));
        }
    });
}

/// Like `compile_ps`, but takes the result of the background precompile when it is ready.
pub fn compile_ps_cached(hlsl: &str, entry: &str) -> Result<Vec<u8>, String> {
    if let Ok(c) = PRECOMPILED.lock() {
        if let Some((_, r)) = c.iter().find(|((h, e), _)| h == hlsl && e == entry) {
            return r.clone();
        }
    }
    compile_ps(hlsl, entry)
}

/// Removes the replacement for `hash`; the game's own shader is bound again from the next `PSSetShader`.
pub fn remove_substitute(hash: u32) {
    substitutes().remove(hash);
}

/// `PSSetShader` could not be hooked (substitution is impossible).
pub fn unavailable() -> bool {
    UNAVAILABLE.load(Relaxed)
}

// ---- D3DCompile ------------------------------------------------------------------------------

type D3DCompileFn = unsafe extern "system" fn(*const c_void, usize, *const u8, *const c_void, *const c_void, *const u8, *const u8, u32, u32, *mut *mut c_void, *mut *mut c_void) -> i32;

fn d3d_compile_fn() -> Option<D3DCompileFn> {
    static F: OnceLock<usize> = OnceLock::new();
    let addr = *F.get_or_init(|| unsafe {
        let name: Vec<u16> = "d3dcompiler_47.dll\0".encode_utf16().collect();
        let m = LoadLibraryExW(name.as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32);
        if m.is_null() {
            return 0;
        }
        GetProcAddress(m, c"D3DCompile".as_ptr() as *const u8).map_or(0, |f| f as usize)
    });
    (addr != 0).then(|| unsafe { std::mem::transmute::<usize, D3DCompileFn>(addr) })
}

type D3DDisassembleFn = unsafe extern "system" fn(*const c_void, usize, u32, *const u8, *mut *mut c_void) -> i32;

/// The text disassembly of DXBC `code` (`D3DDisassemble` of d3dcompiler_47). Slow: background threads only (dev dumps).
pub fn disassemble(code: &[u8]) -> Result<String, String> {
    static F: OnceLock<usize> = OnceLock::new();
    let addr = *F.get_or_init(|| unsafe {
        let name: Vec<u16> = "d3dcompiler_47.dll\0".encode_utf16().collect();
        let m = LoadLibraryExW(name.as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32);
        if m.is_null() {
            return 0;
        }
        GetProcAddress(m, c"D3DDisassemble".as_ptr() as *const u8).map_or(0, |f| f as usize)
    });
    if addr == 0 {
        return Err("D3DDisassemble not available".to_string());
    }
    unsafe {
        let f: D3DDisassembleFn = std::mem::transmute::<usize, D3DDisassembleFn>(addr);
        let mut out: *mut c_void = std::ptr::null_mut();
        let hr = f(code.as_ptr().cast(), code.len(), 0, std::ptr::null(), &mut out);
        if hr < 0 || out.is_null() {
            return Err(format!("D3DDisassemble failed (0x{:08x})", hr as u32));
        }
        Ok(String::from_utf8_lossy(&take_blob(out)).trim_end_matches('\0').to_string())
    }
}

/// Reads and releases an `ID3DBlob` (Release is slot 2, GetBufferPointer 3, GetBufferSize 4).
unsafe fn take_blob(blob: *mut c_void) -> Vec<u8> {
    if blob.is_null() {
        return Vec::new();
    }
    unsafe {
        let vtbl = *(blob as *const *const usize);
        let ptr: unsafe extern "system" fn(*mut c_void) -> *const u8 = std::mem::transmute(*vtbl.add(3));
        let size: unsafe extern "system" fn(*mut c_void) -> usize = std::mem::transmute(*vtbl.add(4));
        let rel: unsafe extern "system" fn(*mut c_void) -> u32 = std::mem::transmute(*vtbl.add(2));
        let v = std::slice::from_raw_parts(ptr(blob), size(blob)).to_vec();
        rel(blob);
        v
    }
}

/// Compiles HLSL to `ps_5_0` bytecode. `Err` carries the compiler message.
pub fn compile_ps(hlsl: &str, entry: &str) -> Result<Vec<u8>, String> {
    compile(hlsl, entry, "ps_5_0")
}

/// Compiles HLSL for the given target profile (`ps_5_0`, `vs_5_0`, ...).
pub fn compile(hlsl: &str, entry: &str, profile: &str) -> Result<Vec<u8>, String> {
    #[cfg(test)]
    {
        test_cache::compile(hlsl, entry, profile)
    }
    #[cfg(not(test))]
    {
        compile_now(hlsl, entry, profile)
    }
}

/// Tests only: every compiled shader is kept in memory and on disk (the system temp folder), keyed by its source, entry and
/// profile, so a test run compiles each shader once and a second run compiles none. Failed compiles are not kept.
#[cfg(test)]
mod test_cache {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    fn key(hlsl: &str, entry: &str, profile: &str) -> String {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in hlsl.bytes().chain([0]).chain(entry.bytes()).chain([0]).chain(profile.bytes()) {
            h = (h ^ b as u64).wrapping_mul(0x0100_0000_01b3);
        }
        format!("{h:016x}-{}-{entry}-{profile}", hlsl.len())
    }

    pub fn compile(hlsl: &str, entry: &str, profile: &str) -> Result<Vec<u8>, String> {
        static MEM: OnceLock<Mutex<HashMap<String, Vec<u8>>>> = OnceLock::new();
        let k = key(hlsl, entry, profile);
        let mem = MEM.get_or_init(Default::default);
        if let Some(v) = mem.lock().unwrap_or_else(|e| e.into_inner()).get(&k) {
            return Ok(v.clone());
        }
        let dir = std::env::temp_dir().join("gw2graphics-test-shaders");
        let file = dir.join(format!("{k}.dxbc"));
        let r = match std::fs::read(&file) {
            Ok(v) if !v.is_empty() => Ok(v),
            _ => {
                let r = super::compile_now(hlsl, entry, profile);
                if let Ok(v) = &r {
                    let _ = std::fs::create_dir_all(&dir);
                    let tmp = dir.join(format!("{k}.{:?}.tmp", std::thread::current().id()));
                    if std::fs::write(&tmp, v).is_ok() {
                        let _ = std::fs::rename(&tmp, &file);
                    }
                }
                r
            }
        };
        if let Ok(v) = &r {
            mem.lock().unwrap_or_else(|e| e.into_inner()).insert(k, v.clone());
        }
        r
    }
}

/// `D3DCompile` itself.
fn compile_now(hlsl: &str, entry: &str, profile: &str) -> Result<Vec<u8>, String> {
    let f = d3d_compile_fn().ok_or_else(|| "d3dcompiler_47.dll or D3DCompile not available".to_string())?;
    let entry_c = format!("{entry}\0");
    let profile_c = format!("{profile}\0");
    let mut code: *mut c_void = std::ptr::null_mut();
    let mut errs: *mut c_void = std::ptr::null_mut();
    let hr = unsafe { f(hlsl.as_ptr() as *const c_void, hlsl.len(), std::ptr::null(), std::ptr::null(), std::ptr::null(), entry_c.as_ptr(), profile_c.as_ptr(), 0, 0, &mut code, &mut errs) };
    let code = unsafe { take_blob(code) };
    let msg = String::from_utf8_lossy(&unsafe { take_blob(errs) }).trim().to_string();
    finish_compile(hr, code, msg)
}

/// Turns the raw `D3DCompile` outcome into a result; success without code is an error.
fn finish_compile(hr: i32, code: Vec<u8>, msg: String) -> Result<Vec<u8>, String> {
    if hr < 0 {
        return Err(if msg.is_empty() { format!("D3DCompile failed (hr={hr:#x})") } else { msg });
    }
    if code.is_empty() {
        return Err("compiler returned no code".to_string());
    }
    Ok(code)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn fnv1a32_empty_is_offset_basis() {
        assert_eq!(fnv1a32(&[]), 2166136261);
    }

    #[test]
    fn fnv1a32_known_vector() {
        assert_eq!(fnv1a32(b"a"), 0xe40c292c);
    }

    #[test]
    fn registry_unknown_pointer_is_none() {
        assert_eq!(Registry::new().hash_of(0x1000), None);
    }

    #[test]
    fn registry_overwrites_reused_pointer() {
        let r = Registry::new();
        r.record(0x1000, b"first");
        assert_eq!(r.hash_of(0x1000), Some(fnv1a32(b"first")));
        r.record(0x1000, b"second");
        assert_eq!(r.hash_of(0x1000), Some(fnv1a32(b"second")));
    }

    #[test]
    fn substitution_disabled_returns_original() {
        assert!(!substitution_enabled());
        assert_eq!(choose(false, Some(0x2000), 0x1000), 0x1000);
        assert_eq!(choose(true, None, 0x1000), 0x1000);
        assert_eq!(choose(true, Some(0x2000), 0x1000), 0x2000);
    }

    #[test]
    fn failed_reports_a_substitute_that_could_not_be_created() {
        let s = Substitutes::new();
        assert!(!s.failed(1));
        s.install(1, "src", "main");
        assert!(!s.failed(1));
        assert_eq!(s.get_or_create(10, 1, |_, _| Err("boom".into())), None);
        assert!(s.failed(1));
        s.install(1, "src2", "main"); // re-installed: pending again
        assert!(!s.failed(1));
    }

    #[test]
    fn failed_ps_set_shader_patch_means_unusable() {
        assert!(!mechanism_usable(false));
        assert!(mechanism_usable(true));
    }

    #[test]
    fn substitute_is_created_once_and_failure_is_not_retried() {
        let s = Substitutes::new();
        assert_eq!(s.get_or_create(10, 1, |_, _| Ok(7)), None); // nothing installed
        s.install(1, "src", "main");
        assert_eq!(s.get(1), None);
        assert_eq!(
            s.get_or_create(10, 1, |h, e| {
                assert_eq!((h, e), ("src", "main"));
                Ok(0x2000)
            }),
            Some(0x2000)
        );
        assert_eq!(s.get_or_create(10, 1, |_, _| panic!("created twice")), Some(0x2000));
        assert_eq!(s.get(1), Some(0x2000));
        s.install(2, "bad", "main");
        assert_eq!(s.get_or_create(10, 2, |_, _| Err("boom".into())), None);
        assert_eq!(s.get_or_create(10, 2, |_, _| panic!("retried")), None);
    }

    #[test]
    fn removed_substitute_is_gone_and_can_be_installed_again() {
        let s = Substitutes::new();
        s.install(1, "src", "main");
        assert_eq!(s.get_or_create(10, 1, |_, _| Ok(0x2000)), Some(0x2000));
        assert!(s.remove(1));
        assert!(!s.remove(1));
        assert_eq!(s.get(1), None);
        assert_eq!(s.get_or_create(10, 1, |_, _| panic!("removed substitute created")), None);
        s.install(1, "src", "main");
        assert_eq!(s.get_or_create(10, 1, |_, _| Ok(0x3000)), Some(0x3000));
    }

    #[test]
    fn replacing_returns_old_pointer_once_to_release() {
        let s = Substitutes::new();
        s.install(1, "v1", "main");
        assert_eq!(s.get_or_create(10, 1, |_, _| Ok(0x2000)), Some(0x2000));
        s.install(1, "v2", "main");
        let released = std::cell::RefCell::new(Vec::new());
        let got = s.get_or_create_with(10, 1, |h, _| {
            assert_eq!(h, "v2");
            Ok(0x3000)
        }, |p| released.borrow_mut().push(p));
        assert_eq!(got, Some(0x3000));
        assert_eq!(*released.borrow(), vec![0x2000]);
        // Served from now on without another release.
        assert_eq!(s.get_or_create_with(10, 1, |_, _| panic!("again"), |p| released.borrow_mut().push(p)), Some(0x3000));
        assert_eq!(*released.borrow(), vec![0x2000]);
    }

    #[test]
    fn final_substitute_keeps_old_pointer_while_replacing() {
        let s = Substitutes::new();
        s.install(1, "v1", "main");
        assert_eq!(s.get_or_create(10, 1, |_, _| Ok(0x2000)), Some(0x2000));
        s.install(1, "v2", "main");
        assert_eq!(s.get(1), Some(0x2000)); // pending: the old one is still the active one
        // While the new source is being created, another bind still gets the old pointer.
        let got = s.get_or_create(10, 1, |_, _| {
            assert_eq!(s.get_or_create(10, 1, |_, _| panic!("nested create")), Some(0x2000));
            Ok(0x3000)
        });
        assert_eq!(got, Some(0x3000));
        // Two quick re-installs: still the first good pointer until a creation succeeds.
        s.install(1, "v3", "main");
        s.install(1, "v4", "main");
        assert_eq!(s.get(1), Some(0x3000));
    }

    #[test]
    fn failed_replacement_keeps_old_pointer() {
        let s = Substitutes::new();
        s.install(1, "v1", "main");
        assert_eq!(s.get_or_create(10, 1, |_, _| Ok(0x2000)), Some(0x2000));
        s.install(1, "bad", "main");
        let released = std::cell::RefCell::new(Vec::new());
        let got = s.get_or_create_with(10, 1, |_, _| Err("boom".into()), |p| released.borrow_mut().push(p));
        assert_eq!(got, Some(0x2000));
        assert!(released.borrow().is_empty());
        assert_eq!(s.get_or_create(10, 1, |_, _| panic!("retried")), Some(0x2000)); // never Failed
    }

    #[test]
    fn stale_creation_is_released_not_installed() {
        let s = Substitutes::new();
        s.install(1, "v1", "main");
        let released = std::cell::RefCell::new(Vec::new());
        let got = s.get_or_create_with(10, 1, |_, _| {
            s.install(1, "v2", "main"); // slider moved while compiling
            Ok(0x2000)
        }, |p| released.borrow_mut().push(p));
        assert_eq!(got, None);
        assert_eq!(*released.borrow(), vec![0x2000]);
        assert_eq!(s.get_or_create(10, 1, |h, _| { assert_eq!(h, "v2"); Ok(0x3000) }), Some(0x3000));
    }

    #[test]
    fn first_time_reports_each_hash_once() {
        let mut seen = Vec::new();
        assert!(first_time(&mut seen, 5));
        assert!(!first_time(&mut seen, 5));
        assert!(first_time(&mut seen, 6));
    }

    #[test]
    fn substitute_for_other_device_is_not_reused() {
        let s = Substitutes::new();
        s.install(1, "src", "main");
        assert_eq!(s.get_or_create(10, 1, |_, _| Ok(0x2000)), Some(0x2000));
        // New device: the old pointer must not be returned; a new one is created.
        assert_eq!(s.get_or_create(20, 1, |_, _| Ok(0x3000)), Some(0x3000));
        assert_eq!(s.get_or_create(20, 1, |_, _| panic!("created twice")), Some(0x3000));
        // A failure on one device does not block another.
        s.install(2, "bad", "main");
        assert_eq!(s.get_or_create(10, 2, |_, _| Err("boom".into())), None);
        assert_eq!(s.get_or_create(20, 2, |_, _| Ok(0x4000)), Some(0x4000));
    }

    #[test]
    fn compile_success_without_code_is_an_error() {
        assert_eq!(finish_compile(0, Vec::new(), String::new()), Err("compiler returned no code".to_string()));
        assert_eq!(finish_compile(0, vec![1], String::new()), Ok(vec![1]));
        assert!(finish_compile(-1, Vec::new(), "bad".into()).unwrap_err().contains("bad"));
    }

    #[test]
    fn hlsl_compile_error_is_reported_not_panicked() {
        let e = compile_ps("float4 main() : SV_Target { return nonsense; }", "main").unwrap_err();
        assert!(e.contains("nonsense"), "{e}");
    }

    #[test]
    fn hlsl_compile_ok_gives_dxbc() {
        let b = compile_ps("float4 main() : SV_Target { return float4(1,0,0,1); }", "main").unwrap();
        assert_eq!(&b[..4], b"DXBC");
    }

    /// DXBC blobs of `data`: `DXBC` magic, container size u32 at offset 24 (64..=2,000,000, fits).
    pub(crate) fn dxbc_blobs(data: &[u8]) -> Vec<&[u8]> {
        let mut out = Vec::new();
        let mut i = 0;
        while i + 28 <= data.len() {
            if data[i] == b'D' && &data[i..i + 4] == b"DXBC" {
                let size = u32::from_le_bytes(data[i + 24..i + 28].try_into().unwrap()) as usize;
                if (64..=2_000_000).contains(&size) && i + size <= data.len() {
                    out.push(&data[i..i + size]);
                    i += size;
                    continue;
                }
            }
            i += 1;
        }
        out
    }

    #[test]
    fn fnv1a32_matches_known_blob_hashes() {
        let Ok(path) = std::env::var("GW2_EXE") else {
            println!("skipped: GW2_EXE is not set");
            return;
        };
        let data = std::fs::read(&path).expect("read GW2_EXE");
        let blobs = dxbc_blobs(&data);
        println!("{} DXBC blobs in {path}", blobs.len());
        let b = blobs.iter().find(|b| fnv1a32(b) == 0x4340ca88).expect("blob with hash 4340ca88");
        assert_eq!(b.len(), 504);
    }
}
