//! Photo mode (`docs/photo.md`): a full-resolution capture of the current frame on demand (button or hotkey), with the mod's effects,
//! saved as sRGB PNG + a linear Radiance `.hdr` + the settings text into `GW2Graphics\photos`. Everything is visual-only; nothing
//! here changes the picture the player plays with.
//!
//! The capture machinery is `debugshot`'s (the same staging ring, background writer, pair state machine with its restore guarantees and
//! watchdog); this module holds the pure parts (Radiance codec, disk-space guard, names, the capture queue and its arbitration with the
//! debug capture timer) and the optional UI-free scene grab (`grab_scene`, called from the pre-UI copy draw).
//!
//! # Which picture
//! The normal photo is a copy of the swapchain back buffer taken just BEFORE the overlay is drawn, i.e. exactly the final picture
//! including the game's own UI, without the mod's overlay. With `photo_hide_ui=1` the mod instead copies the texture that the engine's
//! pre-UI copy pass (`ps#c0ad1686`) reads - the 3D scene with every mod effect (AA, lens, grading, AO, ...) but without the UI - and
//! applies the final pass' maths on the CPU (decode, energy bleach, tone shoulder, paper white) so the result matches the picture. It
//! needs that pass to be seen in the frame (the GPU copy is armed by the request and taken at the next pre-UI copy draw); if it does
//! not arrive within `SCENE_TIMEOUT_FRAMES` presents the normal photo (with UI) is taken and the status line says so. Pair captures
//! always include the UI.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};

use crate::d3d::{Obj, release, vfn, vt};
use crate::debugshot::{self, PixFmt};
use crate::hdr10;

// ================================================================================================================
// Radiance .hdr (RGBE)
// ================================================================================================================

/// One pixel as RGBE: shared 8-bit exponent. Negative, NaN and zero channels become 0; infinity and huge values are clamped.
pub fn rgbe_encode(rgb: [f32; 3]) -> [u8; 4] {
    let c = rgb.map(|v| if v.is_nan() || v <= 0.0 { 0.0 } else { v.min(1.0e30) });
    let m = c[0].max(c[1]).max(c[2]);
    if m < 1.0e-32 {
        return [0, 0, 0, 0];
    }
    // m = f * 2^e with f in [0.5, 1)
    let mut e = m.log2().floor() as i32 + 1;
    if m >= 2f32.powi(e) {
        e += 1;
    } else if m < 2f32.powi(e - 1) {
        e -= 1;
    }
    let scale = 256.0 / 2f32.powi(e);
    let q = |v: f32| ((v * scale) as u32).min(255) as u8;
    [q(c[0]), q(c[1]), q(c[2]), (e + 128).clamp(1, 255) as u8]
}

/// Inverse of `rgbe_encode` (the Radiance convention: half a step is added).
#[allow(dead_code)] // the reader side is used by the tests and by anyone opening the files
pub fn rgbe_decode(p: [u8; 4]) -> [f32; 3] {
    if p[3] == 0 {
        return [0.0; 3];
    }
    let f = 2f32.powi(p[3] as i32 - 136);
    [0, 1, 2].map(|i| (p[i] as f32 + 0.5) * f)
}

/// A flat (uncompressed) Radiance picture, top row first: `rgb` is `w * h * 3` linear floats. Every Radiance reader understands flat
/// scanlines; the file is `4 * w * h` bytes plus a 60 byte header.
pub fn encode_radiance(w: u32, h: u32, rgb: &[f32]) -> Vec<u8> {
    assert_eq!(rgb.len(), w as usize * h as usize * 3);
    let mut out = format!("#?RADIANCE\n# GW2 Enhanced Graphics photo: linear scene-referred, 1.0 = paper white\nFORMAT=32-bit_rle_rgbe\nEXPOSURE=1.0\n\n-Y {h} +X {w}\n").into_bytes();
    out.reserve(w as usize * h as usize * 4);
    for p in rgb.chunks_exact(3) {
        out.extend_from_slice(&rgbe_encode([p[0], p[1], p[2]]));
    }
    out
}

/// Reads a flat Radiance picture written by `encode_radiance` (used by the tests; the new-RLE scanline form is not supported).
#[allow(dead_code)]
pub fn decode_radiance(bytes: &[u8]) -> Option<(u32, u32, Vec<f32>)> {
    let mut pos = 0;
    let line = |pos: &mut usize| -> Option<String> {
        let rest = bytes.get(*pos..)?;
        let n = rest.iter().position(|b| *b == b'\n')?;
        let s = String::from_utf8_lossy(&rest[..n]).to_string();
        *pos += n + 1;
        Some(s)
    };
    if !line(&mut pos)?.starts_with("#?") {
        return None;
    }
    loop {
        if line(&mut pos)?.is_empty() {
            break;
        }
    }
    let res = line(&mut pos)?;
    let t: Vec<&str> = res.split_whitespace().collect();
    if t.len() != 4 || t[0] != "-Y" || t[2] != "+X" {
        return None;
    }
    let (h, w): (u32, u32) = (t[1].parse().ok()?, t[3].parse().ok()?);
    let body = bytes.get(pos..)?;
    if body.len() != w as usize * h as usize * 4 {
        return None;
    }
    let mut v = Vec::with_capacity(w as usize * h as usize * 3);
    for p in body.chunks_exact(4) {
        v.extend_from_slice(&rgbe_decode([p[0], p[1], p[2], p[3]]));
    }
    Some((w, h, v))
}

/// float32 -> IEEE half, round to nearest even; overflow becomes infinity (callers clamp to 65504 first).
pub fn f32_to_f16(v: f32) -> u16 {
    let b = v.to_bits();
    let sign = ((b >> 16) & 0x8000) as u16;
    let e = ((b >> 23) & 0xff) as i32;
    let m = b & 0x7f_ffff;
    if e == 255 {
        return sign | 0x7c00 | if m != 0 { 0x200 } else { 0 };
    }
    let e = e - 127 + 15;
    if e >= 31 {
        return sign | 0x7c00;
    }
    if e <= 0 {
        if e < -10 {
            return sign;
        }
        let m = m | 0x80_0000;
        let shift = (14 - e) as u32;
        let mut half = m >> shift;
        let rem = m & ((1 << shift) - 1);
        let mid = 1 << (shift - 1);
        if rem > mid || (rem == mid && half & 1 == 1) {
            half += 1;
        }
        return sign | half as u16;
    }
    let mut h = ((e as u32) << 10) | (m >> 13);
    let rem = m & 0x1fff;
    if rem > 0x1000 || (rem == 0x1000 && h & 1 == 1) {
        h += 1;
    }
    sign | h as u16
}

// ================================================================================================================
// Disk space, names
// ================================================================================================================

/// Nothing is written when less than this is free on the drive (plus the size of the files).
pub const MIN_FREE_BYTES: u64 = 200 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Space {
    Ok,
    /// Free bytes, bytes needed.
    Low(u64, u64),
    /// The free space could not be read: the photo is written anyway (a write error is reported if the disk is really full).
    Unknown,
}

/// Rough size of the files of one photo (PNG stored blocks + flat Radiance + text).
pub fn photo_bytes(w: u32, h: u32, hdr: bool) -> u64 {
    let px = w as u64 * h as u64;
    px * 3 + if hdr { px * 4 } else { 0 } + 32 * 1024
}

/// The free-space guard: free space must exceed `MIN_FREE_BYTES` plus the files.
pub fn space_check(free: Option<u64>, need_files: u64) -> Space {
    match free {
        None => Space::Unknown,
        Some(f) if f > MIN_FREE_BYTES.saturating_add(need_files) => Space::Ok,
        Some(f) => Space::Low(f, MIN_FREE_BYTES.saturating_add(need_files)),
    }
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetDiskFreeSpaceExW(dir: *const u16, free_to_caller: *mut u64, total: *mut u64, total_free: *mut u64) -> i32;
}

/// Bytes available to this user on the drive of `dir` (which must exist), None when the query fails.
pub fn free_bytes(dir: &Path) -> Option<u64> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let w: Vec<u16> = dir.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        let (mut free, mut total, mut total_free) = (0u64, 0u64, 0u64);
        // SAFETY: valid NUL-terminated path and three valid out pointers.
        let ok = unsafe { GetDiskFreeSpaceExW(w.as_ptr(), &mut free, &mut total, &mut total_free) };
        (ok != 0).then_some(free)
    }
    #[cfg(not(windows))]
    {
        let _ = dir;
        None
    }
}

/// `YYYYMMDD-HHMMSS` (UTC) of a unix time in seconds.
pub fn format_utc(unix: u64) -> String {
    let days = (unix / 86_400) as i64;
    let secs = unix % 86_400;
    // civil from days (Howard Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}{m:02}{d:02}-{:02}{:02}{:02}", secs / 3600, secs % 3600 / 60, secs % 60)
}

/// The base name of a photo: `gw2-<timestamp>`; `-2`, `-3`... when the name is taken (nothing is ever overwritten).
pub fn unique_stem(unix: u64, taken: impl Fn(&str) -> bool) -> String {
    let base = format!("gw2-{}", format_utc(unix));
    if !taken(&base) {
        return base;
    }
    (2..1000).map(|n| format!("{base}-{n}")).find(|s| !taken(s)).unwrap_or_else(|| format!("{base}-{unix}"))
}

// ================================================================================================================
// Hotkey
// ================================================================================================================

/// Is the key combination held right now, with EXACTLY these modifiers (so the bare key, or the overlay key Shift+End while the photo key is
/// Ctrl+Shift+End, never triggers the other one)? `held(vk)` answers for one virtual key.
pub fn combo_down(held: impl Fn(i32) -> bool, key: u32, mods: u32) -> bool {
    const SHIFT: u32 = 1;
    const CTRL: u32 = 2;
    const ALT: u32 = 4;
    held(key as i32) && held(0x10) == (mods & SHIFT != 0) && held(0x11) == (mods & CTRL != 0) && held(0x12) == (mods & ALT != 0)
}

// ================================================================================================================
// The capture queue and its arbitration with the debug capture
// ================================================================================================================

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PhotoKind {
    /// One shot with the mod.
    Single,
    /// The mod's picture and the vanilla emulation (the debug pair machine).
    Pair,
}

/// Requests wait here, oldest first, at most `QUEUE_MAX`.
pub const QUEUE_MAX: usize = 3;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PhotoQueue {
    q: Vec<PhotoKind>,
}

impl PhotoQueue {
    pub const fn new() -> Self {
        PhotoQueue { q: Vec::new() }
    }

    /// Queues a request: Ok(position) or Err when the queue is full.
    pub fn push(&mut self, k: PhotoKind) -> Result<usize, &'static str> {
        if self.q.len() >= QUEUE_MAX {
            return Err("the photo queue is full");
        }
        self.q.push(k);
        Ok(self.q.len())
    }

    pub fn peek(&self) -> Option<PhotoKind> {
        self.q.first().copied()
    }

    pub fn pop(&mut self) -> Option<PhotoKind> {
        if self.q.is_empty() { None } else { Some(self.q.remove(0)) }
    }

    pub fn len(&self) -> usize {
        self.q.len()
    }
}

/// What the capture step does with the head of the photo queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Plan {
    /// No photo is queued: the debug capture logic goes on.
    Idle,
    /// A photo is queued but cannot start yet (a pair or a scene grab is running, or the ring is busy): NOTHING else starts either, so
    /// the photo is not starved and two captures never run at once.
    Wait,
    Single,
    Pair,
}

/// Single capture at a time. `pair_running`: a pair (dev or photo) holds `Enabled` forced off; `scene_waiting`: a UI-free scene grab is
/// armed; `free_slots`: free staging slots; a pair needs two, a single one. A pair request with the mod switched off degrades to a single
/// shot (there is no difference to show).
pub fn arbitrate(next: Option<PhotoKind>, pair_running: bool, scene_waiting: bool, free_slots: usize, user_enabled: bool) -> Plan {
    let Some(k) = next else { return Plan::Idle };
    if pair_running || scene_waiting {
        return Plan::Wait;
    }
    match k {
        PhotoKind::Single if free_slots >= 1 => Plan::Single,
        PhotoKind::Pair if user_enabled && free_slots >= 2 => Plan::Pair,
        PhotoKind::Pair if !user_enabled && free_slots >= 1 => Plan::Single,
        _ => Plan::Wait,
    }
}

/// A scene grab that has not arrived after this many presents is given up (the normal photo is taken).
pub const SCENE_TIMEOUT_FRAMES: u64 = 90;

/// What to do with an armed scene grab at `frame` (armed at `since`): ready data wins; else wait; else time out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SceneStep {
    Wait,
    Timeout,
}

pub fn scene_step(since: u64, frame: u64) -> SceneStep {
    if frame.saturating_sub(since) > SCENE_TIMEOUT_FRAMES { SceneStep::Timeout } else { SceneStep::Wait }
}

// ================================================================================================================
// Settings, queue state, status
// ================================================================================================================

static HIDE_UI: AtomicBool = AtomicBool::new(false);
static QUEUE: Mutex<PhotoQueue> = Mutex::new(PhotoQueue::new());
static STATUS: Mutex<String> = Mutex::new(String::new());
static SAVED: AtomicU64 = AtomicU64::new(0);

pub fn hide_ui() -> bool {
    HIDE_UI.load(Relaxed)
}

pub fn set_hide_ui(on: bool) {
    HIDE_UI.store(on, Relaxed);
}

pub fn configure(cfg: &crate::config::Config) {
    set_hide_ui(cfg.photo_hide_ui);
}

pub(crate) fn set_status(s: impl Into<String>) {
    *STATUS.lock().unwrap_or_else(|e| e.into_inner()) = s.into();
}

pub(crate) fn note_saved() {
    SAVED.fetch_add(1, Relaxed);
}

/// Requests a photo (overlay button or hotkey). Returns what to tell the player.
pub fn request(k: PhotoKind) -> String {
    let r = QUEUE.lock().unwrap_or_else(|e| e.into_inner()).push(k);
    let s = match r {
        Ok(1) => "photo requested".to_string(),
        Ok(n) => format!("photo queued ({n} waiting)"),
        Err(e) => e.to_string(),
    };
    set_status(s.clone());
    s
}

pub(crate) fn queued() -> Option<PhotoKind> {
    QUEUE.try_lock().ok().and_then(|q| q.peek())
}

pub(crate) fn pop_queued() -> Option<PhotoKind> {
    QUEUE.lock().unwrap_or_else(|e| e.into_inner()).pop()
}

pub fn queue_len() -> usize {
    QUEUE.lock().unwrap_or_else(|e| e.into_inner()).len()
}

static HOTKEY: Mutex<String> = Mutex::new(String::new());

/// The hotkey as the config spells it (for the status line).
pub fn set_hotkey_text(s: &str) {
    *HOTKEY.lock().unwrap_or_else(|e| e.into_inner()) = s.to_string();
}

/// The `status_photo=` line.
pub fn status_text() -> String {
    let s = STATUS.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let last = if s.is_empty() { "idle".to_string() } else { s };
    let hotkey = HOTKEY.lock().unwrap_or_else(|e| e.into_inner()).clone();
    format!("{last}; saved this session {}; queue {}; photo_hide_ui={} ({}); hotkey {hotkey}", SAVED.load(Relaxed), queue_len(), hide_ui() as u8, scene_state_text())
}

// ================================================================================================================
// Pixel conversion of a full-resolution capture
// ================================================================================================================

/// Rows of linear BT.709 values relative to paper white (1.0 = paper white), `w * h * 3` floats, NaN -> 0, negative -> 0, capped at
/// 65504. `scale` is the same multiplier as `debugshot::convert` (scRGB: `80 / paper_white`; HDR10: the same, after the PQ decode).
pub fn linear_rows(data: &[u8], w: u32, h: u32, fmt: PixFmt, scale: f32) -> Option<Vec<f32>> {
    let (w, h) = (w as usize, h as usize);
    let bpp = debugshot::bytes_per_px(fmt);
    if w == 0 || h == 0 || data.len() < w * h * bpp || !scale.is_finite() {
        return None;
    }
    let lut8: Vec<f32> = (0..256).map(|i| debugshot::srgb_decode(i as f32 / 255.0)).collect();
    let lut10: Vec<f32> = if fmt == PixFmt::Rgb10A2 { (0..1024).map(|i| hdr10::pq_decode(i as f32 / 1023.0) / 80.0).collect() } else { Vec::new() };
    let mut out = Vec::with_capacity(w * h * 3);
    for i in 0..w * h {
        let o = i * bpp;
        let c = match fmt {
            PixFmt::F16 => [0, 1, 2].map(|k| debugshot::f16_to_f32(u16::from_le_bytes([data[o + k * 2], data[o + k * 2 + 1]]))),
            PixFmt::Bgra8 => [lut8[data[o + 2] as usize], lut8[data[o + 1] as usize], lut8[data[o] as usize]],
            PixFmt::Rgba8 => [lut8[data[o] as usize], lut8[data[o + 1] as usize], lut8[data[o + 2] as usize]],
            PixFmt::Rgb10A2 => {
                let v = u32::from_le_bytes([data[o], data[o + 1], data[o + 2], data[o + 3]]);
                hdr10::bt2020_to_709([lut10[(v & 1023) as usize], lut10[((v >> 10) & 1023) as usize], lut10[((v >> 20) & 1023) as usize]])
            }
        };
        out.extend(c.map(|v| if v.is_nan() { 0.0 } else { (v * scale).clamp(0.0, 65504.0) }));
    }
    Some(out)
}

/// 8-bit sRGB rows from `linear_rows` (paper white = 1.0, clipped).
pub fn srgb8_rows(lin: &[f32]) -> Vec<u8> {
    lin.iter().map(|v| (debugshot::srgb_encode(*v) * 255.0 + 0.5) as u8).collect()
}

// ---- the UI-free scene ------------------------------------------------------------------------------------------------------------

/// The final pass' maths on the CPU for one pixel of the pre-UI scene (display-referred, gamma encoded): extended sRGB decode, energy
/// bleach, tone shoulder, paper white, clamp at the peak. Returns scRGB units (1.0 = 80 nits). Mirror of `final_scrgb.hlsl` (the test
/// compares it with the host reference of the shader).
#[allow(dead_code)] // the piecewise sRGB form (gamma 0), used by the tests
pub fn final_pass_cpu(v: [f32; 3], paper_white: f32, peak: f32) -> [f32; 3] {
    final_pass_cpu_g(v, paper_white, peak, 0.0)
}

/// `final_pass_cpu` with the SDR decode gamma (`sdr_gamma`, 0 = the piecewise sRGB curve): the SDR range 0..1 is decoded with the
/// pure power `gamma`, above 1 the extended sRGB curve (mirror of `decode_in` in `final_scrgb.hlsl`).
pub fn final_pass_cpu_g(v: [f32; 3], paper_white: f32, peak: f32, gamma: f32) -> [f32; 3] {
    let mut lin = v.map(|c| sdr_decode(c, gamma));
    let m0 = lin[0].max(lin[1]).max(lin[2]);
    let yl = 0.2126 * lin[0] + 0.7152 * lin[1] + 0.0722 * lin[2];
    let kb = 0.4 * ((m0 - 1.5) / (m0 + 1.5)).clamp(0.0, 1.0);
    lin = lin.map(|c| c + (yl - c) * kb);
    let m = lin[0].max(lin[1]).max(lin[2]);
    let (head, knee) = crate::output::shoulder_constants(paper_white, peak);
    let k = if m > knee {
        let r = (head - knee).max(1e-4);
        (knee + r * (1.0 - (-(m - knee) / r).exp())) / m
    } else {
        1.0
    };
    lin.map(|c| (c * k * (paper_white / 80.0)).min(peak / 80.0))
}

/// The display decode of one encoded channel: `gamma` >= 1 = pure power on 0..1 (NaN / negative -> 0) and the extended sRGB curve
/// above 1; otherwise the (extended) piecewise sRGB curve.
pub fn sdr_decode(v: f32, gamma: f32) -> f32 {
    if gamma >= 1.0 && !(v > 1.0) {
        if v > 0.0 { v.powf(gamma) } else { 0.0 }
    } else {
        crate::tonemap::srgb_decode(v)
    }
}

/// How the grabbed pre-UI scene is turned into a picture.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SceneMap {
    /// HDR output is active: the final pass' maths apply with these levels; otherwise plain sRGB decode.
    pub hdr: bool,
    pub paper: f32,
    pub peak: f32,
    /// The SDR decode gamma of the final pass (`output::active_sdr_gamma`, 0 = piecewise sRGB); used only with `hdr`.
    pub gamma: f32,
}

/// Converts the grabbed float16 scene (gamma encoded, may exceed 1) in place to float16 scRGB-unit linear light, so the ordinary conversion
/// (`scale = 80 / paper`) applies. Without HDR output the values are clamped and decoded like the SDR final pass; paper then is 80 (scale 1).
pub fn scene_to_scrgb_f16(data: &mut [u8], map: &SceneMap) {
    let (paper, peak) = if map.hdr { (map.paper, map.peak) } else { (80.0, 80.0) };
    for px in data.chunks_exact_mut(8) {
        let v = [0, 1, 2].map(|k| {
            let x = debugshot::f16_to_f32(u16::from_le_bytes([px[k * 2], px[k * 2 + 1]]));
            let x = if x.is_nan() { 0.0 } else { x.clamp(0.0, 65504.0) };
            if map.hdr { x } else { x.min(1.0) }
        });
        let c = if map.hdr { final_pass_cpu_g(v, paper, peak, map.gamma) } else { v.map(crate::tonemap::srgb_decode) };
        for k in 0..3 {
            px[k * 2..k * 2 + 2].copy_from_slice(&f32_to_f16(c[k].clamp(0.0, 65504.0)).to_le_bytes());
        }
    }
}

// ================================================================================================================
// Writing
// ================================================================================================================

/// What `save_files` wrote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Saved {
    pub png: PathBuf,
    pub hdr: Option<PathBuf>,
}

/// Writes `<stem><suffix>.png` (full resolution sRGB, with the settings in a tEXt chunk) and, when `hdr`, `<stem><suffix>.hdr`.
/// The free-space guard runs first; nothing is deleted, nothing is overwritten (`stem` is unique). `lin` comes from `linear_rows`.
pub fn save_files(dir: &Path, stem: &str, suffix: &str, w: u32, h: u32, lin: &[f32], settings: &str, hdr: bool) -> Result<Saved, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    match space_check(free_bytes(dir), photo_bytes(w, h, hdr)) {
        Space::Low(free, need) => return Err(format!("skipped: only {} MB free on the drive, need more than {} MB", free / 1_048_576, need / 1_048_576)),
        Space::Unknown | Space::Ok => {}
    }
    let png = debugshot::encode_png_rgb8_text(w, h, &srgb8_rows(lin), Some(settings));
    let png_path = dir.join(format!("{stem}{suffix}.png"));
    if png_path.exists() {
        return Err(format!("{} exists already, not overwritten", png_path.display()));
    }
    debugshot::write_atomic(&png_path, &png).map_err(|e| format!("png: {e}"))?;
    let hdr_path = if hdr {
        let p = dir.join(format!("{stem}{suffix}.hdr"));
        debugshot::write_atomic(&p, &encode_radiance(w, h, lin)).map_err(|e| format!("hdr: {e}"))?;
        Some(p)
    } else {
        None
    };
    Ok(Saved { png: png_path, hdr: hdr_path })
}

// ================================================================================================================
// GPU: the UI-free scene copy
// ================================================================================================================

struct Stage {
    device: usize,
    w: u32,
    h: u32,
    fmt: u32,
    tex: Obj,
}

unsafe impl Send for Stage {}

impl Drop for Stage {
    fn drop(&mut self) {
        unsafe { release(self.tex) };
    }
}

struct SceneState {
    stage: Option<Stage>,
    /// (present number at the copy, w, h, format)
    pending: Option<(u64, u32, u32, u32)>,
}

static SCENE_WANT: AtomicBool = AtomicBool::new(false);
static SCENE: Mutex<SceneState> = Mutex::new(SceneState { stage: None, pending: None });
static NOW: AtomicU64 = AtomicU64::new(0);
static LAST_SCENE: Mutex<String> = Mutex::new(String::new());

fn scene_state_text() -> String {
    let s = LAST_SCENE.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if s.is_empty() { "UI-free copy not tried yet".to_string() } else { s }
}

/// The UI-free copy is wanted at the next pre-UI copy draw (cheap atomic, read by `look::on_ps_set` / `draw_guard`).
pub fn scene_wanted() -> bool {
    SCENE_WANT.load(Relaxed)
}

/// Arms the scene grab (the frame tick calls this when a photo with `photo_hide_ui` starts).
pub(crate) fn arm_scene(frame: u64) {
    NOW.store(frame, Relaxed);
    if let Ok(mut s) = SCENE.lock() {
        s.pending = None;
    }
    SCENE_WANT.store(true, Relaxed);
}

pub(crate) fn disarm_scene(note: &str) {
    SCENE_WANT.store(false, Relaxed);
    if let Ok(mut s) = SCENE.lock() {
        s.pending = None;
    }
    *LAST_SCENE.lock().unwrap_or_else(|e| e.into_inner()) = note.to_string();
}

pub(crate) fn tick(frame: u64) {
    NOW.store(frame, Relaxed);
}

/// Called by the `Draw*` hooks at the engine's pre-UI copy draw, AFTER the mod's passes bound their result as t0: when armed, copies the
/// texture bound at t0 (the 3D scene with every mod effect, no UI) to a staging texture. GPU side only; never waits. Never unwinds.
pub fn grab_scene(ctx: Obj) {
    if !SCENE_WANT.load(Relaxed) {
        return;
    }
    let _ = std::panic::catch_unwind(|| unsafe { grab_inner(ctx) });
}

unsafe fn grab_inner(ctx: Obj) {
    unsafe {
        let get_srvs: unsafe extern "system" fn(Obj, u32, u32, *mut Obj) = std::mem::transmute(vfn(ctx, vt::CTX_PS_GET_SRVS));
        let mut srv: Obj = std::ptr::null_mut();
        get_srvs(ctx, 0, 1, &mut srv);
        if srv.is_null() {
            return;
        }
        let tex = crate::ao::texture_of(srv);
        release(srv);
        let Some((res, d)) = tex else { return };
        let ok = d.samples == 1 && d.array == 1 && d.mips == 1 && d.width >= 320 && d.height >= 200 && (d.format == 10 || d.format == 87);
        if !ok {
            release(res);
            *LAST_SCENE.lock().unwrap_or_else(|e| e.into_inner()) = format!("the pre-UI copy reads an unsupported texture (format {}, {}x{})", d.format, d.width, d.height);
            return;
        }
        let mut dev: Obj = std::ptr::null_mut();
        let get_device: unsafe extern "system" fn(Obj, *mut Obj) = std::mem::transmute(vfn(ctx, vt::CHILD_GET_DEVICE));
        get_device(ctx, &mut dev);
        if dev.is_null() {
            release(res);
            return;
        }
        if let Ok(mut s) = SCENE.try_lock() {
            if s.stage.as_ref().is_none_or(|g| g.device != dev as usize || g.w != d.width || g.h != d.height || g.fmt != d.format) {
                s.stage = None;
                let tex = debugshot::create_staging(dev, d.width, d.height, d.format);
                if !tex.is_null() {
                    s.stage = Some(Stage { device: dev as usize, w: d.width, h: d.height, fmt: d.format, tex });
                }
            }
            if let Some(g) = s.stage.as_ref() {
                debugshot::copy_resource(ctx, g.tex, res);
                s.pending = Some((NOW.load(Relaxed), d.width, d.height, d.format));
                SCENE_WANT.store(false, Relaxed);
            }
        }
        release(dev);
        release(res);
    }
}

/// A grabbed scene, read back.
pub struct SceneData {
    pub data: Vec<u8>,
    pub w: u32,
    pub h: u32,
    pub fmt: u32,
}

/// Reads the grabbed scene when its copy is at least `MIN_AGE` presents old and finished (never waits). `Err` = give up.
pub(crate) unsafe fn take_scene(ctx: Obj, frame: u64) -> Result<Option<SceneData>, String> {
    let Ok(mut s) = SCENE.try_lock() else { return Ok(None) };
    let Some((at, w, h, fmt)) = s.pending else { return Ok(None) };
    let age = frame.saturating_sub(at);
    if age < debugshot::MIN_AGE {
        return Ok(None);
    }
    let Some(g) = s.stage.as_ref() else {
        s.pending = None;
        return Err("the scene staging texture is gone".to_string());
    };
    let bpp = if fmt == 10 { 8 } else { 4 };
    match unsafe { debugshot::read_texture(ctx, g.tex, 0x10_0000, w, h, bpp) } {
        Some(data) => {
            s.pending = None;
            Ok(Some(SceneData { data, w, h, fmt }))
        }
        None if age > debugshot::MAX_AGE => {
            s.pending = None;
            Err("the GPU copy of the scene never finished".to_string())
        }
        None => Ok(None),
    }
}

#[allow(dead_code)]
pub(crate) fn scene_pending() -> bool {
    SCENE.try_lock().map(|s| s.pending.is_some()).unwrap_or(false)
}

pub(crate) fn note_scene(s: &str) {
    *LAST_SCENE.lock().unwrap_or_else(|e| e.into_inner()) = s.to_string();
}

#[cfg(test)]
#[path = "photo_tests.rs"]
mod tests;
