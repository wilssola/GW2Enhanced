//! Debug mode: periodic screenshots of the final picture (plus a vanilla comparison shot), image statistics,
//! and live re-reading of `config.ini`. Everything is OFF by default (`debug_capture_seconds=0`, `debug_hot_reload=0`).
//! See `docs/debug-mode.md`.
//!
//! # Capture
//! `on_frame` runs once per present from the overlay's render loop (hudhook calls it BEFORE it draws the overlay, so
//! the copy of back buffer 0 does not contain the overlay). It copies the back buffer into a ring of three staging
//! textures with `CopyResource` (GPU side, never waits), and reads copies that are at least two frames old with
//! `Map(DO_NOT_WAIT)`; a copy that is not finished is retried at the next present. The mapped rows are copied into a
//! `Vec` (one memcpy of the frame, a few ms, only when a capture is due) and handed to a background thread that converts
//! (box filter 2x, sRGB), encodes the PNG, computes the statistics and writes the files.
//!
//! # Pair (mod + vanilla)
//! A capture pair takes shot A with the mod on, then forces the user's `Enabled` switch off (the same path as the
//! overlay checkbox: `gate::set_user_enabled(false)` + `output::apply_user_levels(false, ..)`), waits until the pipeline
//! has been rebuilt for that state (`pair_step`), takes shot B and restores the user's values. The restore happens in
//! `Saved::restore`, reached from: the capture step, the abort step, any error exit, a panic in `on_frame`, the
//! `ForceGuard` drop, and a watchdog thread that restores after `PAIR_WATCHDOG_MS` even if the render loop stopped.

use std::ffi::c_void;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use crate::config::Config;
use crate::d3d::{Guid, IgnoreScope, Obj, Tex2dDesc, release, vfn, vt};
use crate::{ao, datapath, gate, halo, hdr10, hdr_shaders, log, output, photo, resolve, shafts, shaders, ssr};

// ================================================================================================================
// PNG writer (stored zlib blocks: no compression crate is vendored)
// ================================================================================================================

const fn make_crc_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut n = 0;
    while n < 256 {
        let mut c = n as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        t[n] = c;
        n += 1;
    }
    t
}

static CRC_TABLE: [u32; 256] = make_crc_table();

/// CRC-32 (IEEE) as PNG uses it.
pub fn crc32(parts: &[&[u8]]) -> u32 {
    let mut c = 0xffff_ffffu32;
    for p in parts {
        for &b in *p {
            c = CRC_TABLE[((c ^ b as u32) & 0xff) as usize] ^ (c >> 8);
        }
    }
    c ^ 0xffff_ffff
}

pub fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for chunk in data.chunks(5552) {
        for &x in chunk {
            a += x as u32;
            b += a;
        }
        a %= 65521;
        b %= 65521;
    }
    (b << 16) | a
}

fn push_chunk(out: &mut Vec<u8>, ty: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(ty);
    out.extend_from_slice(data);
    out.extend_from_slice(&crc32(&[ty, data]).to_be_bytes());
}

/// An 8-bit RGB PNG (`rgb.len() == w * h * 3`), zlib stored blocks (about the size of the raw picture).
pub fn encode_png_rgb8(w: u32, h: u32, rgb: &[u8]) -> Vec<u8> {
    encode_png_rgb8_text(w, h, rgb, None)
}

/// Same, plus an optional `tEXt` chunk (keyword "Settings") carrying the mod settings the picture was taken with, so
/// the image keeps them when it is copied away from its `.txt` (ASCII only: anything else becomes '?').
pub fn encode_png_rgb8_text(w: u32, h: u32, rgb: &[u8], settings: Option<&str>) -> Vec<u8> {
    assert_eq!(rgb.len(), w as usize * h as usize * 3);
    let stride = w as usize * 3;
    let mut raw = Vec::with_capacity(h as usize * (stride + 1));
    for row in rgb.chunks(stride.max(1)).take(h as usize) {
        raw.push(0); // filter: none
        raw.extend_from_slice(row);
    }
    let mut z = Vec::with_capacity(raw.len() + raw.len() / 65535 * 5 + 16);
    z.extend_from_slice(&[0x78, 0x01]);
    let mut blocks = raw.chunks(65535).peekable();
    if blocks.peek().is_none() {
        z.extend_from_slice(&[1, 0, 0, 0xff, 0xff]);
    }
    while let Some(b) = blocks.next() {
        z.push(if blocks.peek().is_none() { 1 } else { 0 });
        z.extend_from_slice(&(b.len() as u16).to_le_bytes());
        z.extend_from_slice(&(!(b.len() as u16)).to_le_bytes());
        z.extend_from_slice(b);
    }
    z.extend_from_slice(&adler32(&raw).to_be_bytes());
    let mut out = Vec::with_capacity(z.len() + 64);
    out.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // 8 bit, truecolour, deflate, filter 0, no interlace
    push_chunk(&mut out, b"IHDR", &ihdr);
    if let Some(t) = settings {
        let mut d = b"Settings\0".to_vec();
        d.extend(t.chars().map(|c| if c.is_ascii() && (c == '\n' || !c.is_ascii_control()) { c as u8 } else { b'?' }));
        push_chunk(&mut out, b"tEXt", &d);
    }
    push_chunk(&mut out, b"IDAT", &z);
    push_chunk(&mut out, b"IEND", &[]);
    out
}

// ================================================================================================================
// Pixel conversion and statistics
// ================================================================================================================

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixFmt {
    F16,
    Bgra8,
    Rgba8,
    /// `R10G10B10A2_UNORM` holding HDR10 (PQ, BT.2020): decoded to scRGB-unit BT.709 like the float data.
    Rgb10A2,
}

/// `DXGI_FORMAT` of a back buffer the conversion understands.
pub fn pixfmt(dxgi: u32) -> Option<PixFmt> {
    match dxgi {
        10 => Some(PixFmt::F16),
        87 | 91 => Some(PixFmt::Bgra8),
        28 | 29 => Some(PixFmt::Rgba8),
        24 => Some(PixFmt::Rgb10A2),
        _ => None,
    }
}

pub fn bytes_per_px(f: PixFmt) -> usize {
    if f == PixFmt::F16 { 8 } else { 4 }
}

pub fn f16_to_f32(h: u16) -> f32 {
    let sign = if h & 0x8000 != 0 { -1.0f32 } else { 1.0 };
    let exp = ((h >> 10) & 0x1f) as i32;
    let man = (h & 0x3ff) as f32;
    match exp {
        0 => sign * man * 2f32.powi(-24),
        31 => if man == 0.0 { sign * f32::INFINITY } else { f32::NAN },
        e => sign * (1.0 + man / 1024.0) * 2f32.powi(e - 15),
    }
}

pub fn srgb_encode(l: f32) -> f32 {
    let l = l.clamp(0.0, 1.0);
    if l <= 0.003_130_8 { l * 12.92 } else { 1.055 * l.powf(1.0 / 2.4) - 0.055 }
}

pub fn srgb_decode(e: f32) -> f32 {
    if e <= 0.040_45 { e / 12.92 } else { ((e + 0.055) / 1.055).powf(2.4) }
}

/// Image statistics on the linear data, 1.0 = paper white, before any clipping.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Stats {
    pub pixels: u64,
    pub frac_gt1: f32,
    pub frac_gt2: f32,
    pub frac_gt4: f32,
    pub p99_lum: f32,
    pub mean_lum: f32,
    /// Mean HSV saturation of the pixels with a channel above 1.0 (0 when there are none).
    pub mean_sat_above1: f32,
}

const HIST_BINS: usize = 640;
const HIST_LO: f32 = -12.0; // log2 of the lowest bin
const HIST_PER_OCTAVE: f32 = 32.0;

fn hist_bin(lum: f32) -> usize {
    ((lum.log2() - HIST_LO) * HIST_PER_OCTAVE).clamp(0.0, HIST_BINS as f32 - 1.0) as usize
}

fn hist_value(bin: usize) -> f32 {
    2f32.powf((bin as f32 + 0.5) / HIST_PER_OCTAVE + HIST_LO)
}

pub struct Converted {
    pub w: u32,
    pub h: u32,
    pub rgb: Vec<u8>,
    /// Display-encoded luminance (0..1) of every output pixel, for the difference of two shots.
    pub luma: Vec<f32>,
    pub stats: Stats,
}

fn luminance(c: [f32; 3]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

/// Converts tightly packed rows (`w * bytes_per_px` per row) to an 8-bit sRGB picture at half size (2x2 box filter on
/// the linear values) and computes the statistics. `scale` multiplies the linear value (scRGB: `80 / paper_white`, so
/// paper white becomes 1.0; 8-bit formats: 1). None for sizes below 2x2 or short data.
pub fn convert(data: &[u8], w: u32, h: u32, fmt: PixFmt, scale: f32) -> Option<Converted> {
    let (w, h) = (w as usize, h as usize);
    let bpp = bytes_per_px(fmt);
    if w < 2 || h < 2 || data.len() < w * h * bpp || !scale.is_finite() {
        return None;
    }
    let lut: Vec<f32> = (0..256).map(|i| srgb_decode(i as f32 / 255.0)).collect();
    let lut10: Vec<f32> = if fmt == PixFmt::Rgb10A2 { (0..1024).map(|i| hdr10::pq_decode(i as f32 / 1023.0) / 80.0).collect() } else { Vec::new() };
    let px = |x: usize, y: usize| -> [f32; 3] {
        let o = (y * w + x) * bpp;
        let c = match fmt {
            PixFmt::F16 => {
                let g = |i: usize| f16_to_f32(u16::from_le_bytes([data[o + i * 2], data[o + i * 2 + 1]]));
                [g(0), g(1), g(2)]
            }
            PixFmt::Bgra8 => [lut[data[o + 2] as usize], lut[data[o + 1] as usize], lut[data[o] as usize]],
            PixFmt::Rgba8 => [lut[data[o] as usize], lut[data[o + 1] as usize], lut[data[o + 2] as usize]],
            PixFmt::Rgb10A2 => {
                let v = u32::from_le_bytes([data[o], data[o + 1], data[o + 2], data[o + 3]]);
                hdr10::bt2020_to_709([lut10[(v & 1023) as usize], lut10[((v >> 10) & 1023) as usize], lut10[((v >> 20) & 1023) as usize]])
            }
        };
        c.map(|v| if v.is_nan() { 0.0 } else { (v * scale).min(65504.0) })
    };
    let (w2, h2) = (w / 2, h / 2);
    let mut rgb = Vec::with_capacity(w2 * h2 * 3);
    let mut luma = Vec::with_capacity(w2 * h2);
    let mut hist = vec![0u32; HIST_BINS];
    let (mut n, mut g1, mut g2, mut g4, mut above) = (0u64, 0u64, 0u64, 0u64, 0u64);
    let (mut lum_sum, mut sat_sum) = (0f64, 0f64);
    for by in 0..h2 {
        for bx in 0..w2 {
            let mut acc = [0f32; 3];
            for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                let c = px(bx * 2 + dx, by * 2 + dy);
                let mx = c[0].max(c[1]).max(c[2]);
                let mn = c[0].min(c[1]).min(c[2]);
                let lum = luminance(c.map(|v| v.max(0.0)));
                n += 1;
                lum_sum += lum as f64;
                if lum > 0.0 {
                    hist[hist_bin(lum)] += 1;
                } else {
                    hist[0] += 1;
                }
                if mx > 1.0 {
                    g1 += 1;
                    above += 1;
                    sat_sum += ((mx - mn.max(0.0)) / mx) as f64;
                    if mx > 2.0 {
                        g2 += 1;
                    }
                    if mx > 4.0 {
                        g4 += 1;
                    }
                }
                for i in 0..3 {
                    acc[i] += c[i];
                }
            }
            let avg = acc.map(|v| (v * 0.25).clamp(0.0, 1.0));
            for v in avg {
                rgb.push((srgb_encode(v) * 255.0 + 0.5) as u8);
            }
            luma.push(srgb_encode(luminance(avg)));
        }
    }
    let target = (n as f64 * 0.99).ceil() as u64;
    let mut run = 0u64;
    let mut p99 = 0.0;
    for (i, &c) in hist.iter().enumerate() {
        run += c as u64;
        if run >= target {
            p99 = if i == 0 { 0.0 } else { hist_value(i) };
            break;
        }
    }
    let nf = n as f64;
    let stats = Stats {
        pixels: n,
        frac_gt1: (g1 as f64 / nf) as f32,
        frac_gt2: (g2 as f64 / nf) as f32,
        frac_gt4: (g4 as f64 / nf) as f32,
        p99_lum: p99,
        mean_lum: (lum_sum / nf) as f32,
        mean_sat_above1: if above > 0 { (sat_sum / above as f64) as f32 } else { 0.0 },
    };
    Some(Converted { w: w2 as u32, h: h2 as u32, rgb, luma, stats })
}

/// Difference of two shots (display-encoded luminance, so 0.1 is 10 % of the full scale).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DiffStats {
    pub mean_abs_luma: f32,
    pub frac_over_10pct: f32,
}

pub fn diff_stats(a: &[f32], b: &[f32]) -> Option<DiffStats> {
    if a.len() != b.len() || a.is_empty() {
        return None;
    }
    let (mut sum, mut over) = (0f64, 0u64);
    for (x, y) in a.iter().zip(b) {
        let d = (x - y).abs();
        sum += d as f64;
        if d > 0.10 {
            over += 1;
        }
    }
    Some(DiffStats { mean_abs_luma: (sum / a.len() as f64) as f32, frac_over_10pct: (over as f64 / a.len() as f64) as f32 })
}

// ================================================================================================================
// Pure logic: ring, cadence, retention, pair machine, hot-reload diff
// ================================================================================================================

pub const RING: usize = 3;
/// A copy is read at the earliest this many presents after it was issued.
pub const MIN_AGE: u64 = 2;
/// A copy that never became readable is dropped after this many presents.
pub const MAX_AGE: u64 = 240;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slot {
    Free,
    Pending(u64),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlotRing {
    pub slots: [Slot; RING],
}

impl SlotRing {
    pub const fn new() -> Self {
        SlotRing { slots: [Slot::Free; RING] }
    }

    /// Takes a free slot for a copy issued at `frame`.
    pub fn acquire(&mut self, frame: u64) -> Option<usize> {
        let i = self.slots.iter().position(|s| *s == Slot::Free)?;
        self.slots[i] = Slot::Pending(frame);
        Some(i)
    }

    pub fn free_count(&self) -> usize {
        self.slots.iter().filter(|s| **s == Slot::Free).count()
    }

    pub fn pending_count(&self) -> usize {
        RING - self.free_count()
    }

    /// Slots whose copy is old enough to try a read.
    pub fn ready(&self, frame: u64) -> Vec<usize> {
        (0..RING).filter(|&i| matches!(self.slots[i], Slot::Pending(f) if frame.saturating_sub(f) >= MIN_AGE)).collect()
    }

    pub fn expired(&self, frame: u64) -> Vec<usize> {
        (0..RING).filter(|&i| matches!(self.slots[i], Slot::Pending(f) if frame.saturating_sub(f) > MAX_AGE)).collect()
    }

    pub fn release(&mut self, i: usize) {
        self.slots[i] = Slot::Free;
    }
}

impl Default for SlotRing {
    fn default() -> Self {
        Self::new()
    }
}

/// Is a periodic capture due? `seconds == 0` = never; the first one is due at once.
pub fn capture_due(now_ms: u64, last_ms: Option<u64>, seconds: u32) -> bool {
    seconds > 0 && last_ms.is_none_or(|l| now_ms.saturating_sub(l) >= seconds as u64 * 1000)
}

pub const PAIR_MIN_FRAMES: u64 = 6;
pub const PAIR_MIN_MS: u64 = 250;
/// The vanilla shot is taken at the latest after this long even if some substitute is still being built.
pub const PAIR_FORCE_MS: u64 = 1500;
/// The user's values are restored after this long whatever happens.
pub const PAIR_WATCHDOG_MS: u64 = 2000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PairStep {
    /// No pair is running.
    Idle,
    Wait,
    /// Take the vanilla shot now; then restore.
    Capture,
    /// Give up; restore.
    Abort,
}

impl PairStep {
    /// Every step that ends a pair needs the user's values restored.
    pub fn restore(self) -> bool {
        matches!(self, PairStep::Capture | PairStep::Abort)
    }
}

/// `Enabled` was forced off at (`since_ms`, `since_frame`): what to do at present `frame`. The user's switch is read
/// at the frame boundary of the NEXT present, and the substitutes (bloom shaders, final pass) are rebuilt lazily, so
/// the shot waits for at least `PAIR_MIN_FRAMES` presents and `PAIR_MIN_MS`, and for `settled` (no re-installed
/// substitute still pending); at `PAIR_FORCE_MS` it takes the shot anyway; at `PAIR_WATCHDOG_MS` it gives up.
pub fn pair_step(since_ms: u64, since_frame: u64, now_ms: u64, frame: u64, settled: bool) -> PairStep {
    let elapsed = now_ms.saturating_sub(since_ms);
    let frames = frame.saturating_sub(since_frame);
    if elapsed >= PAIR_WATCHDOG_MS {
        PairStep::Abort
    } else if frames >= PAIR_MIN_FRAMES && elapsed >= PAIR_MIN_MS && (settled || elapsed >= PAIR_FORCE_MS) {
        PairStep::Capture
    } else {
        PairStep::Wait
    }
}

/// The state of a running pair.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PairMachine {
    forced: Option<(u64, u64)>,
}

impl PairMachine {
    pub const fn new() -> Self {
        PairMachine { forced: None }
    }

    pub fn is_forced(&self) -> bool {
        self.forced.is_some()
    }

    /// Starts a pair (false when one runs already).
    pub fn begin(&mut self, now_ms: u64, frame: u64) -> bool {
        if self.forced.is_some() {
            return false;
        }
        self.forced = Some((now_ms, frame));
        true
    }

    pub fn step(&mut self, now_ms: u64, frame: u64, settled: bool) -> PairStep {
        let Some((ms, fr)) = self.forced else { return PairStep::Idle };
        let s = pair_step(ms, fr, now_ms, frame, settled);
        if s.restore() {
            self.forced = None;
        }
        s
    }

    /// An error exit (no slot, no swapchain, ...): ends the pair; true when the user's values need restoring.
    pub fn cancel(&mut self) -> bool {
        self.forced.take().is_some()
    }
}

/// The user's values saved when `Enabled` is forced off.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Saved {
    pub enabled: bool,
    pub paper: f32,
    pub peak: f32,
    pub at_ms: u64,
}

impl Saved {
    /// Puts the user's values back through the two setters.
    pub fn restore(&self, mut set_enabled: impl FnMut(bool), mut set_levels: impl FnMut(f32, f32)) {
        set_enabled(self.enabled);
        set_levels(self.paper, self.peak);
    }

    pub fn watchdog_expired(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.at_ms) >= PAIR_WATCHDOG_MS
    }
}

/// The id (unix seconds) of a `prefix<id>...` file name.
fn id_of(name: &str, prefix: &str) -> Option<u64> {
    let rest = name.strip_prefix(prefix)?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() || digits.len() > 12 {
        return None;
    }
    digits.parse().ok()
}

/// The files of `names` that belong to a shot/pair older than the newest `keep` ones. Only `prefix<digits>...` files
/// ending in `.png`, `.txt` or `.tmp` are ever listed: nothing else is touched (`latest*` never matches).
pub fn stale_files(names: &[String], prefix: &str, keep: usize) -> Vec<String> {
    let mut ids: Vec<u64> = names.iter().filter(|n| is_ours(n)).filter_map(|n| id_of(n, prefix)).collect();
    ids.sort_unstable_by(|a, b| b.cmp(a));
    ids.dedup();
    let keep_ids: Vec<u64> = ids.into_iter().take(keep).collect();
    names.iter().filter(|n| is_ours(n)).filter(|n| id_of(n, prefix).is_some_and(|i| !keep_ids.contains(&i))).cloned().collect()
}

fn is_ours(n: &str) -> bool {
    n.ends_with(".png") || n.ends_with(".txt") || n.ends_with(".tmp")
}

/// Hot reload: which keys differ between two parses of the file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Diff {
    pub live: Vec<&'static str>,
    /// Keys that need a restart (the swapchain exists already) or are read once.
    pub ignored: Vec<&'static str>,
}

impl Diff {
    pub fn is_empty(&self) -> bool {
        self.live.is_empty() && self.ignored.is_empty()
    }
}

pub fn diff(old: &Config, new: &Config) -> Diff {
    let mut d = Diff::default();
    macro_rules! live {
        ($($f:ident),*) => { $( if old.$f != new.$f { d.live.push(stringify!($f)); } )* };
    }
    live!(enabled, paper_white_nits, peak_nits, sdr_gamma, bloom_strength, highlight_boost, ao_strength, ao_radius, halo_strength, halo_radius, ssr_strength, ssr_range, ssr_classes, ssr_smart, ssr_hue_filter, ssr_flatness, ssr_water_boost, water_depth_look, ssr_matte_cut, shafts_strength, shafts_length, contact_strength, contact_length, contact_sun, aerial_strength, aerial_distance, depth_unit, aniso_level, aniso_filters, mip_bias, smaa, smaa_threshold, sharpen_strength, autoexp_strength, autoexp_target, autoexp_range, adapt_up, adapt_down, localtone_strength, lens_strength, lens_threshold, lens_ghost, lens_streak_length, lens_max_size, lens_color_reject, grade_preset, grade_saturation, grade_contrast, grade_temperature, grade_tint, grade_vibrance, grade_shadows, grade_highlights, grade_lut, grade_lut_strength, fur_strength, fur_ps, fur_vs, fur_min_vcb, fur_debug, fur_uv, fur_occlusion, fur_amp, fur_scale, fur_length, fur_detail, fur_rim, fur_sss, fur_view, rim_strength, rim_width, subsurface_strength, rim_ps, rim_min_vcb, rim_debug, debug_capture_seconds, debug_compare, debug_hot_reload, debug_view, photo_hide_ui, calibration_pattern);
    d.live.extend(crate::shellfur::ShellCfg::changed(&old.shell, &new.shell));
    d.live.extend(crate::toggles::Toggles::changed(&old.toggles, &new.toggles));
    if old.hdr_output != new.hdr_output {
        d.ignored.push("hdr_output");
    }
    if old.hdr_format != new.hdr_format {
        d.ignored.push("hdr_format");
    }
    if old.overlay_key != new.overlay_key {
        d.ignored.push("overlay_key");
    }
    if old.photo_key != new.photo_key {
        d.ignored.push("photo_key");
    }
    d
}

/// `cur` with the live keys that changed between `old` and `new` (the file) taken from `new`; keys the file did not
/// change keep their current (slider) value.
pub fn merge(cur: &Config, old: &Config, new: &Config) -> (Config, Diff) {
    let d = diff(old, new);
    let mut c = cur.clone();
    for k in &d.live {
        match *k {
            "enabled" => c.enabled = new.enabled,
            "paper_white_nits" => c.paper_white_nits = new.paper_white_nits,
            "peak_nits" => c.peak_nits = new.peak_nits,
            "sdr_gamma" => c.sdr_gamma = new.sdr_gamma,
            "bloom_strength" => c.bloom_strength = new.bloom_strength,
            "highlight_boost" => c.highlight_boost = new.highlight_boost,
            "ao_strength" => c.ao_strength = new.ao_strength,
            "ao_radius" => c.ao_radius = new.ao_radius,
            "halo_strength" => c.halo_strength = new.halo_strength,
            "halo_radius" => c.halo_radius = new.halo_radius,
            "ssr_strength" => c.ssr_strength = new.ssr_strength,
            "ssr_range" => c.ssr_range = new.ssr_range,
            "ssr_classes" => c.ssr_classes = new.ssr_classes,
            "ssr_smart" => c.ssr_smart = new.ssr_smart,
            "ssr_hue_filter" => c.ssr_hue_filter = new.ssr_hue_filter,
            "ssr_flatness" => c.ssr_flatness = new.ssr_flatness,
            "ssr_water_boost" => c.ssr_water_boost = new.ssr_water_boost,
            "water_depth_look" => c.water_depth_look = new.water_depth_look,
            "ssr_matte_cut" => c.ssr_matte_cut = new.ssr_matte_cut,
            "shafts_strength" => c.shafts_strength = new.shafts_strength,
            "shafts_length" => c.shafts_length = new.shafts_length,
            "contact_strength" => c.contact_strength = new.contact_strength,
            "contact_length" => c.contact_length = new.contact_length,
            "contact_sun" => c.contact_sun = new.contact_sun.clone(),
            "aerial_strength" => c.aerial_strength = new.aerial_strength,
            "aerial_distance" => c.aerial_distance = new.aerial_distance,
            "depth_unit" => c.depth_unit = new.depth_unit,
            "aniso_level" => c.aniso_level = new.aniso_level,
            "aniso_filters" => c.aniso_filters = new.aniso_filters,
            "mip_bias" => c.mip_bias = new.mip_bias,
            "smaa" => c.smaa = new.smaa,
            "smaa_threshold" => c.smaa_threshold = new.smaa_threshold,
            "sharpen_strength" => c.sharpen_strength = new.sharpen_strength,
            "autoexp_strength" => c.autoexp_strength = new.autoexp_strength,
            "autoexp_target" => c.autoexp_target = new.autoexp_target,
            "autoexp_range" => c.autoexp_range = new.autoexp_range,
            "adapt_up" => c.adapt_up = new.adapt_up,
            "adapt_down" => c.adapt_down = new.adapt_down,
            "localtone_strength" => c.localtone_strength = new.localtone_strength,
            "lens_strength" => c.lens_strength = new.lens_strength,
            "lens_threshold" => c.lens_threshold = new.lens_threshold,
            "lens_ghost" => c.lens_ghost = new.lens_ghost,
            "lens_streak_length" => c.lens_streak_length = new.lens_streak_length,
            "lens_max_size" => c.lens_max_size = new.lens_max_size,
            "lens_color_reject" => c.lens_color_reject = new.lens_color_reject,
            "grade_preset" => c.grade_preset = new.grade_preset.clone(),
            "grade_saturation" => c.grade_saturation = new.grade_saturation,
            "grade_contrast" => c.grade_contrast = new.grade_contrast,
            "grade_temperature" => c.grade_temperature = new.grade_temperature,
            "grade_tint" => c.grade_tint = new.grade_tint,
            "grade_vibrance" => c.grade_vibrance = new.grade_vibrance,
            "grade_shadows" => c.grade_shadows = new.grade_shadows,
            "grade_highlights" => c.grade_highlights = new.grade_highlights,
            "grade_lut" => c.grade_lut = new.grade_lut.clone(),
            "grade_lut_strength" => c.grade_lut_strength = new.grade_lut_strength,
            "fur_strength" => c.fur_strength = new.fur_strength,
            "fur_ps" => c.fur_ps = new.fur_ps.clone(),
            "fur_vs" => c.fur_vs = new.fur_vs.clone(),
            "fur_min_vcb" => c.fur_min_vcb = new.fur_min_vcb,
            "fur_debug" => c.fur_debug = new.fur_debug,
            "fur_uv" => c.fur_uv = new.fur_uv.clone(),
            "fur_occlusion" => c.fur_occlusion = new.fur_occlusion,
            "fur_amp" => c.fur_amp = new.fur_amp,
            "fur_scale" => c.fur_scale = new.fur_scale,
            "fur_length" => c.fur_length = new.fur_length,
            "fur_detail" => c.fur_detail = new.fur_detail,
            "fur_rim" => c.fur_rim = new.fur_rim,
            "fur_sss" => c.fur_sss = new.fur_sss,
            "fur_view" => c.fur_view = new.fur_view,
            "rim_strength" => c.rim_strength = new.rim_strength,
            "rim_width" => c.rim_width = new.rim_width,
            "subsurface_strength" => c.subsurface_strength = new.subsurface_strength,
            "rim_ps" => c.rim_ps = new.rim_ps.clone(),
            "rim_min_vcb" => c.rim_min_vcb = new.rim_min_vcb,
            "rim_debug" => c.rim_debug = new.rim_debug,
            "debug_capture_seconds" => c.debug_capture_seconds = new.debug_capture_seconds,
            "debug_compare" => c.debug_compare = new.debug_compare,
            "debug_hot_reload" => c.debug_hot_reload = new.debug_hot_reload,
            "debug_view" => c.debug_view = new.debug_view,
            "photo_hide_ui" => c.photo_hide_ui = new.photo_hide_ui,
            "calibration_pattern" => c.calibration_pattern = new.calibration_pattern,
            k if crate::shellfur::ShellCfg::is_key(k) => c.shell.take(k, &new.shell),
            k if crate::toggles::Toggles::is_key(k) => c.toggles.take(k, &new.toggles),
            _ => {}
        }
    }
    (c, d)
}

/// Applies the changed live keys through the existing setters (atomics; the shader re-installs they trigger are the
/// lazy ones the overlay sliders use). `cfg` is the merged config.
pub fn apply_live(keys: &[&str], cfg: &Config) {
    let has = |k: &str| keys.contains(&k);
    if keys.iter().any(|k| crate::toggles::Toggles::is_key(k)) && crate::toggles::apply(&cfg.toggles) {
        hdr_shaders::set_bloom_strength(cfg.bloom_strength); // the bloom shader re-installed with its effective strength
    }
    if has("enabled") {
        gate::set_user_enabled(cfg.enabled);
    }
    if has("paper_white_nits") || has("peak_nits") || has("enabled") {
        output::apply_user_levels(cfg.enabled, cfg.paper_white_nits, cfg.peak_nits);
    }
    if has("sdr_gamma") {
        output::set_sdr_gamma(cfg.sdr_gamma);
    }
    if has("bloom_strength") {
        hdr_shaders::set_bloom_strength(cfg.bloom_strength);
    }
    if has("highlight_boost") {
        resolve::set_highlight(cfg.highlight_boost);
    }
    if has("ao_strength") {
        ao::set_strength(cfg.ao_strength);
    }
    if has("ao_radius") {
        ao::set_radius(cfg.ao_radius);
    }
    if has("halo_strength") {
        halo::set_strength(cfg.halo_strength);
    }
    if has("halo_radius") {
        halo::set_radius(cfg.halo_radius);
    }
    if has("ssr_strength") {
        ssr::set_strength(cfg.ssr_strength);
    }
    if has("ssr_range") {
        ssr::set_range(cfg.ssr_range);
    }
    if has("ssr_classes") {
        ssr::set_classes(cfg.ssr_classes);
    }
    if has("ssr_smart") {
        ssr::set_smart(cfg.ssr_smart);
    }
    if has("ssr_hue_filter") {
        ssr::set_hue_filter(cfg.ssr_hue_filter);
    }
    if has("ssr_flatness") {
        ssr::set_flatness(cfg.ssr_flatness);
    }
    if has("ssr_water_boost") {
        ssr::set_water_boost(cfg.ssr_water_boost);
    }
    if has("water_depth_look") {
        ssr::set_depth_look(cfg.water_depth_look);
    }
    if has("ssr_matte_cut") {
        ssr::set_matte_cut(cfg.ssr_matte_cut);
    }
    if has("shafts_strength") {
        shafts::set_strength(cfg.shafts_strength);
    }
    if has("shafts_length") {
        shafts::set_length(cfg.shafts_length);
    }
    if has("contact_strength") {
        crate::contact::set_strength(cfg.contact_strength);
    }
    if has("contact_length") {
        crate::contact::set_length(cfg.contact_length);
    }
    if has("contact_sun") {
        crate::contact::set_mode(&cfg.contact_sun);
    }
    if has("aerial_strength") {
        crate::aerial::set_strength(cfg.aerial_strength);
    }
    if has("aerial_distance") {
        crate::aerial::set_distance(cfg.aerial_distance);
    }
    if has("depth_unit") {
        crate::aerial::set_unit(cfg.depth_unit);
    }
    if has("aniso_level") {
        crate::aniso::set_level(cfg.aniso_level);
    }
    if has("aniso_filters") {
        crate::aniso::set_filters(cfg.aniso_filters);
    }
    if has("mip_bias") {
        crate::aniso::set_bias(cfg.mip_bias);
    }
    if has("smaa") {
        crate::aa::set_smaa(cfg.smaa);
    }
    if has("smaa_threshold") {
        crate::aa::set_threshold(cfg.smaa_threshold);
    }
    if has("sharpen_strength") {
        crate::aa::set_sharpen(cfg.sharpen_strength);
    }
    if has("autoexp_strength") {
        crate::exposure::set_strength(cfg.autoexp_strength);
    }
    if has("autoexp_target") {
        crate::exposure::set_target(cfg.autoexp_target);
    }
    if has("autoexp_range") {
        crate::exposure::set_range(cfg.autoexp_range);
    }
    if has("adapt_up") {
        crate::exposure::set_adapt_up(cfg.adapt_up);
    }
    if has("adapt_down") {
        crate::exposure::set_adapt_down(cfg.adapt_down);
    }
    if has("localtone_strength") {
        crate::exposure::set_local_strength(cfg.localtone_strength);
    }
    if has("lens_strength") {
        crate::lens::set_strength(cfg.lens_strength);
    }
    if has("lens_threshold") {
        crate::lens::set_threshold(cfg.lens_threshold);
    }
    if has("lens_ghost") {
        crate::lens::set_ghost(cfg.lens_ghost);
    }
    if has("lens_streak_length") {
        crate::lens::set_streak_length(cfg.lens_streak_length);
    }
    if has("lens_max_size") {
        crate::lens::set_max_size(cfg.lens_max_size);
    }
    if has("lens_color_reject") {
        crate::lens::set_color_reject(cfg.lens_color_reject);
    }
    if ["grade_saturation", "grade_contrast", "grade_temperature", "grade_tint", "grade_vibrance", "grade_shadows", "grade_highlights"].iter().any(|k| has(k)) {
        crate::grading::set(cfg.grade());
    }
    if has("grade_lut") {
        crate::grading::set_lut(&cfg.grade_lut);
    }
    if has("grade_lut_strength") {
        crate::grading::set_lut_strength(cfg.grade_lut_strength);
    }
    if has("fur_strength") {
        datapath::set_fur(cfg.fur_strength);
    }
    if has("fur_ps") || has("fur_vs") || has("fur_min_vcb") || has("fur_debug") {
        crate::furmask::configure(&cfg.fur_ps, &cfg.fur_vs, cfg.fur_min_vcb, cfg.fur_debug);
    }
    if has("fur_uv") {
        crate::furmask::set_uv_pref(&cfg.fur_uv);
    }
    if has("fur_occlusion") {
        crate::furmask::set_occlusion(cfg.fur_occlusion != 0);
    }
    if ["fur_amp", "fur_scale", "fur_length", "fur_detail", "fur_rim", "fur_sss", "fur_view"].iter().any(|k| has(k)) {
        datapath::set_fur_look(&cfg.fur_look());
    }
    if has("rim_strength") || has("rim_width") || has("subsurface_strength") {
        datapath::set_rim(cfg.rim_strength);
        datapath::set_rim_width(cfg.rim_width);
        datapath::set_subsurface(cfg.subsurface_strength);
    }
    if has("rim_ps") || has("rim_min_vcb") || has("rim_debug") {
        crate::furmask::configure_rim(&cfg.rim_ps, cfg.rim_min_vcb, cfg.rim_debug);
    }
    if has("debug_capture_seconds") || has("debug_compare") || has("debug_hot_reload") {
        set_capture_seconds(cfg.debug_capture_seconds);
        set_compare(cfg.debug_compare);
        set_hot_reload(cfg.debug_hot_reload);
    }
    if has("debug_view") {
        resolve::set_debug_view(cfg.debug_view);
    }
    if has("photo_hide_ui") {
        photo::set_hide_ui(cfg.photo_hide_ui);
    }
    if has("calibration_pattern") {
        output::set_calibration(cfg.calibration_pattern);
    }
    if keys.iter().any(|k| crate::shellfur::ShellCfg::is_key(k)) {
        crate::shellfur::configure(&cfg.shell);
    }
}

// ================================================================================================================
// Settings (atomics), status, requests
// ================================================================================================================

static CAPTURE_SECONDS: AtomicU32 = AtomicU32::new(0);
static COMPARE: AtomicBool = AtomicBool::new(true);
static HOT_RELOAD: AtomicBool = AtomicBool::new(false);
static REQ_SINGLE: AtomicBool = AtomicBool::new(false);
static REQ_PAIR: AtomicBool = AtomicBool::new(false);
static FRAME: AtomicU64 = AtomicU64::new(0);
static LAST_ID: AtomicU64 = AtomicU64::new(0);

pub fn capture_seconds() -> u32 {
    CAPTURE_SECONDS.load(Relaxed)
}
pub fn set_capture_seconds(s: u32) {
    CAPTURE_SECONDS.store(s.min(3600), Relaxed);
}
pub fn compare() -> bool {
    COMPARE.load(Relaxed)
}
pub fn set_compare(on: bool) {
    COMPARE.store(on, Relaxed);
}
pub fn hot_reload() -> bool {
    HOT_RELOAD.load(Relaxed)
}
pub fn set_hot_reload(on: bool) {
    HOT_RELOAD.store(on, Relaxed);
}
/// Overlay button: one mod shot.
pub fn request_single() {
    REQ_SINGLE.store(true, Relaxed);
}
/// Overlay button: a mod + vanilla pair.
pub fn request_pair() {
    REQ_PAIR.store(true, Relaxed);
}

static STATUS: Mutex<String> = Mutex::new(String::new());

fn set_status(s: impl Into<String>) {
    if let Ok(mut g) = STATUS.lock() {
        *g = s.into();
    }
}

/// Overlay status line (last file or error).
pub fn status() -> String {
    let s = STATUS.lock().map(|g| g.clone()).unwrap_or_default();
    if s.is_empty() { "idle".to_string() } else { s }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// A file id: the unix time, bumped when two captures fall into the same second.
fn next_id() -> u64 {
    let now = unix_now();
    let mut last = LAST_ID.load(Relaxed);
    loop {
        let id = now.max(last + 1);
        match LAST_ID.compare_exchange(last, id, Relaxed, Relaxed) {
            Ok(_) => return id,
            Err(l) => last = l,
        }
    }
}

/// Takes the debug settings of the config (start-up).
pub fn configure(cfg: &Config) {
    set_capture_seconds(cfg.debug_capture_seconds);
    set_compare(cfg.debug_compare);
    set_hot_reload(cfg.debug_hot_reload);
    photo::configure(cfg);
}

// ================================================================================================================
// Forcing Enabled off, and restoring
// ================================================================================================================

static SAVED: Mutex<Option<Saved>> = Mutex::new(None);

fn lock_saved() -> std::sync::MutexGuard<'static, Option<Saved>> {
    SAVED.lock().unwrap_or_else(|e| e.into_inner())
}

/// Forces the mod off like the overlay checkbox does (the final pass then shows plain SDR at the Windows SDR level).
fn force_off(now_ms: u64) -> bool {
    let mut g = lock_saved();
    if g.is_some() {
        return false;
    }
    let (enabled, paper, peak) = (gate::user_enabled(), output::paper_white(), output::user_peak());
    *g = Some(Saved { enabled, paper, peak, at_ms: now_ms });
    gate::set_user_enabled(false);
    output::apply_user_levels(false, paper, peak);
    true
}

/// Restores the user's values if they are forced; true when something was restored. Safe to call from any thread, any
/// number of times.
fn restore_now(why: &str) -> bool {
    let Some(s) = lock_saved().take() else { return false };
    s.restore(gate::set_user_enabled, output::set_levels);
    log!("debug: user values restored ({why}): enabled {}, paper white {:.0}, peak {:.0}", s.enabled, s.paper, s.peak);
    true
}

/// Restores the user's values when dropped.
struct ForceGuard;

impl Drop for ForceGuard {
    fn drop(&mut self) {
        restore_now("guard dropped");
    }
}

fn forced() -> bool {
    lock_saved().is_some()
}

// ================================================================================================================
// Frame time telemetry (status_perf=)
// ================================================================================================================

/// Presents the frame-time statistics cover.
pub const PERF_WINDOW: usize = 300;

/// Frame-time statistics over the last presents (CPU timestamps between presents, ms).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PerfStats {
    /// Presents the numbers cover (at most `PERF_WINDOW`).
    pub presents: usize,
    pub avg_ms: f32,
    /// Mean of the slowest 1 % of the frames (at least the slowest one): a stall shows here even among hundreds of smooth frames.
    pub worst1_ms: f32,
    pub fps: f32,
}

/// Pure: the statistics of a list of frame intervals in milliseconds. Non-finite and non-positive entries are ignored; None for no data.
pub fn perf_stats(intervals_ms: &[f32]) -> Option<PerfStats> {
    let mut v: Vec<f32> = intervals_ms.iter().copied().filter(|x| x.is_finite() && *x > 0.0).collect();
    if v.is_empty() {
        return None;
    }
    let n = v.len();
    let avg = (v.iter().map(|x| *x as f64).sum::<f64>() / n as f64) as f32;
    v.sort_by(|a, b| b.total_cmp(a)); // slowest first
    let k = n.div_ceil(100).max(1); // the slowest 1 % of the frames, at least one
    let worst1 = (v[..k].iter().map(|x| *x as f64).sum::<f64>() / k as f64) as f32;
    Some(PerfStats { presents: n, avg_ms: avg, worst1_ms: worst1, fps: 1000.0 / avg })
}

/// A ring of the last `PERF_WINDOW` frame intervals.
#[derive(Clone, Debug)]
pub struct PerfRing {
    buf: [f32; PERF_WINDOW],
    len: usize,
    next: usize,
}

impl PerfRing {
    pub const fn new() -> Self {
        PerfRing { buf: [0.0; PERF_WINDOW], len: 0, next: 0 }
    }

    /// Adds an interval in ms (a non-finite or non-positive one is ignored); the oldest one falls out when the ring is full.
    pub fn push(&mut self, ms: f32) {
        if !ms.is_finite() || ms <= 0.0 {
            return;
        }
        self.buf[self.next] = ms;
        self.next = (self.next + 1) % PERF_WINDOW;
        self.len = (self.len + 1).min(PERF_WINDOW);
    }

    #[allow(dead_code)] // the tests count the entries
    pub fn len(&self) -> usize {
        self.len
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn stats(&self) -> Option<PerfStats> {
        perf_stats(&self.buf[..self.len])
    }
}

impl Default for PerfRing {
    fn default() -> Self {
        Self::new()
    }
}

/// The text of `status_perf=` and of the overlay line.
pub fn perf_text(s: Option<PerfStats>) -> String {
    match s {
        None => "no presents measured yet".to_string(),
        Some(s) => format!(
            "avg {:.2} ms ({:.1} fps), worst 1 % {:.2} ms, over {} presents{} (CPU time between presents)",
            s.avg_ms,
            s.fps,
            s.worst1_ms,
            s.presents,
            if s.presents < PERF_WINDOW { ", still filling" } else { "" }
        ),
    }
}

static PERF: Mutex<PerfRing> = Mutex::new(PerfRing::new());
static PERF_LAST: Mutex<Option<Instant>> = Mutex::new(None);

/// Once per present (first thing in `on_frame`): the CPU time since the previous present goes into the ring. `Instant` is the
/// QueryPerformanceCounter on Windows. Two uncontended locks and one clock read; never blocks.
fn perf_tick() {
    let now = Instant::now();
    let Ok(mut last) = PERF_LAST.try_lock() else { return };
    let prev = last.replace(now);
    drop(last);
    if let (Some(prev), Ok(mut ring)) = (prev, PERF.try_lock()) {
        ring.push(now.saturating_duration_since(prev).as_secs_f64() as f32 * 1000.0);
    }
}

/// The frame-time line for the overlay and `status_perf=`.
pub fn perf_status() -> String {
    perf_text(PERF.lock().unwrap_or_else(|e| e.into_inner()).stats())
}

// ================================================================================================================
// Depth telemetry (status_depth=)
// ================================================================================================================

/// Depth consumer bit of the debug capture (`ao::set_depth_consumer`): keeps the AO's depth capture running while a shot waits for the depth.
/// Far above the bits of `ao.rs` (1..=128), see the test that keeps them apart.
pub const DEPTH_FOR_DEV: u32 = 1 << 20;
/// A shot waits at most this many presents for the depth of the light-adjust draw, then it goes ahead without it.
pub const DEPTH_WAIT_FRAMES: u64 = 12;
/// A depth read-back older than this many presents does not belong to the picture of a shot any more.
pub const DEPTH_MAX_AGE: u64 = 3;

/// The engine's linear depth as read back (row-major, `w * h` values).
pub struct DepthData {
    pub w: u32,
    pub h: u32,
    pub data: Vec<f32>,
}

/// A number with 6 significant digits: plain notation for 1e-5 <= |v| < 1e6, scientific outside, trailing zeros dropped.
pub fn sig6(v: f32) -> String {
    if v.is_nan() {
        return "nan".to_string();
    }
    if v.is_infinite() {
        return if v > 0.0 { "inf" } else { "-inf" }.to_string();
    }
    if v == 0.0 {
        return "0".to_string();
    }
    fn trim(mut t: String) -> String {
        if t.contains('.') {
            while t.ends_with('0') {
                t.pop();
            }
            if t.ends_with('.') {
                t.pop();
            }
        }
        t
    }
    let sci = format!("{v:.5e}"); // d.ddddde[-]x: the exponent after rounding
    let (mant, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    if (-5..6).contains(&exp) {
        trim(format!("{:.*}", (5 - exp).max(0) as usize, v))
    } else {
        format!("{}e{exp}", trim(mant.to_string()))
    }
}

/// Grid of `depth_status` (columns x rows) and the y positions of its centre-column samples.
pub const DEPTH_GRID: (usize, usize) = (5, 3);
pub const DEPTH_COLUMN_Y: [f32; 5] = [0.9, 0.7, 0.5, 0.3, 0.1];

/// Pure: the text of `status_depth=` from the engine's linear depth (`w * h` values, row-major, row 0 on top).
///
/// `valid` = finite and > 0; the percentiles (nearest rank: the value at index round(p / 100 * (n - 1)) of the sorted valid depths) and the
/// minimum / maximum are over the valid depths only. `sky/invalid` = the share of ALL pixels that are invalid or at least 0.98 x the largest
/// valid depth. The grid is the raw depth at the centres of 5 x 3 cells, row-major, top-left first; the centre column is the raw depth at
/// x = 0.5 and y = 0.9, 0.7, 0.5, 0.3, 0.1 (y from the top: 0.9 is near the bottom of the screen). Raw = invalid values are shown as they are.
pub fn depth_status(data: &[f32], w: usize, h: usize) -> String {
    let n = w.saturating_mul(h);
    if w == 0 || h == 0 || data.len() < n {
        return format!("unavailable (bad depth data: {} values for {w}x{h})", data.len());
    }
    let data = &data[..n];
    let mut valid: Vec<f32> = data.iter().copied().filter(|d| d.is_finite() && *d > 0.0).collect();
    let invalid = n - valid.len();
    let at = |fx: f32, fy: f32| -> f32 {
        let (x, y) = (((fx * w as f32) as usize).min(w - 1), ((fy * h as f32) as usize).min(h - 1));
        data[y * w + x]
    };
    let mut grid_rows = Vec::new();
    for j in 0..DEPTH_GRID.1 {
        let row: Vec<String> = (0..DEPTH_GRID.0).map(|i| sig6(at((i as f32 + 0.5) / DEPTH_GRID.0 as f32, (j as f32 + 0.5) / DEPTH_GRID.1 as f32))).collect();
        grid_rows.push(row.join(" "));
    }
    let column: Vec<String> = DEPTH_COLUMN_Y.iter().map(|y| sig6(at(0.5, *y))).collect();
    let tail = format!("grid {}x{} (row-major, top-left first): {}; centre column x=0.5 at y=0.9,0.7,0.5,0.3,0.1: {}", DEPTH_GRID.0, DEPTH_GRID.1, grid_rows.join(" / "), column.join(" "));
    if valid.is_empty() {
        return format!("valid none; sky/invalid 100 %; {tail}");
    }
    let (min, max) = valid.iter().fold((f32::INFINITY, 0.0f32), |(lo, hi), &d| (lo.min(d), hi.max(d)));
    let cut = 0.98 * max;
    let sky = invalid + valid.iter().filter(|&&d| d >= cut).count();
    let mut pct = |p: f64| -> f32 {
        let idx = (p / 100.0 * (valid.len() - 1) as f64).round() as usize;
        *valid.select_nth_unstable_by(idx, |a, b| a.total_cmp(b)).1
    };
    let (p5, p50, p95) = (pct(5.0), pct(50.0), pct(95.0));
    format!("valid min {}, p5 {}, p50 {}, p95 {}, max {}; sky/invalid {:.2} %; {tail}", sig6(min), sig6(p5), sig6(p50), sig6(p95), sig6(max), 100.0 * sky as f64 / n as f64)
}

/// What the debug capture does about the depth when a shot is due (pure: `depth_gate`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DepthGate {
    /// A fresh read-back is there: take it.
    Take,
    /// Ask the light-adjust draw of the next frame for it; the shot waits.
    Arm,
    /// Asked, not there yet: the shot waits.
    Wait,
    /// Asked long ago, never came: the shot goes ahead without a depth.
    GiveUp,
}

/// `frame` = this present, `armed_at` = the present at which the read-back was asked for, `shot_stamp` = the present whose frame the
/// waiting read-back was taken in.
pub fn depth_gate(frame: u64, armed_at: Option<u64>, shot_stamp: Option<u64>) -> DepthGate {
    if shot_stamp.is_some_and(|s| frame.saturating_sub(s) <= DEPTH_MAX_AGE) {
        return DepthGate::Take;
    }
    match armed_at {
        None => DepthGate::Arm,
        Some(a) if frame.saturating_sub(a) > DEPTH_WAIT_FRAMES => DepthGate::GiveUp,
        Some(_) => DepthGate::Wait,
    }
}

struct DepthSlot {
    /// The present at which the capture was armed (None = not armed).
    armed_at: Option<u64>,
    /// The read-back and the present whose frame it belongs to.
    shot: Option<(u64, DepthData)>,
    /// Why the last attempt did not produce a depth.
    why: String,
}

static DEPTH: Mutex<DepthSlot> = Mutex::new(DepthSlot { armed_at: None, shot: None, why: String::new() });
/// The light-adjust draw reads this (one atomic load) to know that a shot waits for the depth.
static DEPTH_ARMED: AtomicBool = AtomicBool::new(false);

fn lock_depth() -> std::sync::MutexGuard<'static, DepthSlot> {
    DEPTH.lock().unwrap_or_else(|e| e.into_inner())
}

fn disarm(d: &mut DepthSlot) {
    d.armed_at = None;
    DEPTH_ARMED.store(false, Relaxed);
    crate::ao::set_depth_consumer(DEPTH_FOR_DEV, false);
}

/// A debug shot of the mod is about to be issued at present `frame`: gets its depth ready (`st.depth`), or says why there is none
/// (`st.depth_note`). False = the shot waits one more present (the depth of the next frame's light-adjust draw is being asked for).
fn prepare_depth(st: &mut State, frame: u64) -> bool {
    prepare_depth_with(st, frame, gate::user_enabled())
}

/// `prepare_depth` with the user's `Enabled` switch given (with the mod off there is no depth capture to wait for).
fn prepare_depth_with(st: &mut State, frame: u64, enabled: bool) -> bool {
    st.depth = None;
    st.depth_note = None;
    if !enabled {
        st.depth_note = Some("the mod is switched off".to_string());
        return true;
    }
    let mut d = lock_depth();
    match depth_gate(frame, d.armed_at, d.shot.as_ref().map(|s| s.0)) {
        DepthGate::Take => {
            st.depth = d.shot.take().map(|s| s.1);
            disarm(&mut d);
            true
        }
        DepthGate::Arm => {
            d.shot = None;
            d.why.clear();
            d.armed_at = Some(frame);
            DEPTH_ARMED.store(true, Relaxed);
            crate::ao::set_depth_consumer(DEPTH_FOR_DEV, true);
            false
        }
        DepthGate::Wait => false,
        DepthGate::GiveUp => {
            let why = if d.why.is_empty() {
                format!("no light-adjust draw with a linear depth within {DEPTH_WAIT_FRAMES} presents: the AO and the other depth users are off, the gate is closed, or no scene is on screen")
            } else {
                d.why.clone()
            };
            disarm(&mut d);
            st.depth_note = Some(why);
            true
        }
    }
}

/// The depth part of a shot's text: a debug shot of the mod (`wanted`) keeps its read-back for the writer thread (`Some`), or gets the line
/// `status_depth=unavailable (reason)`; any other shot (vanilla emulation, photo) gets nothing.
fn attach_depth(text: &mut String, wanted: bool, depth: Option<DepthData>, note: Option<String>) -> Option<DepthData> {
    if !wanted {
        return None;
    }
    match (depth, note) {
        (Some(d), _) => Some(d),
        (None, Some(why)) => {
            text.push_str(&format!("status_depth=unavailable ({why})\n"));
            None
        }
        _ => None,
    }
}

/// Every present: an arm nobody answered (a pair that was cancelled, the debug mode switched off) is dropped after a while, so the depth
/// capture of the AO does not stay on for good.
fn depth_housekeeping(frame: u64) {
    if !DEPTH_ARMED.load(Relaxed) {
        return;
    }
    let mut d = lock_depth();
    if d.armed_at.is_none_or(|a| frame.saturating_sub(a) > DEPTH_WAIT_FRAMES * 4) {
        disarm(&mut d);
    }
}

/// The light-adjust draw (`datapath::bind_current`, on the game's context): when a debug shot waits for the depth, reads the engine's linear depth
/// of THIS frame back. A GPU stall of the render thread, once per capture; one atomic load otherwise. Never unwinds.
pub fn on_light_adjust(ctx: Obj, dev: Obj) {
    if !DEPTH_ARMED.load(Relaxed) {
        return;
    }
    let _ = std::panic::catch_unwind(|| unsafe { depth_capture(ctx, dev) });
}

unsafe fn depth_capture(ctx: Obj, dev: Obj) {
    let _ig = IgnoreScope::new();
    let Some(res) = crate::ao::depth_resource() else {
        lock_depth().why = "no linear depth at the light-adjust draw (the depth pass was not found: the gate is closed or the AO has nothing to track)".to_string();
        return;
    };
    let r = unsafe { read_depth(dev, ctx, res) };
    let mut d = lock_depth();
    match r {
        Ok(data) => {
            d.shot = Some((FRAME.load(Relaxed) + 1, data));
            disarm(&mut d);
        }
        Err(e) => {
            d.why = e;
            DEPTH_ARMED.store(false, Relaxed);
            crate::ao::set_depth_consumer(DEPTH_FOR_DEV, false);
        }
    }
}

/// Copies the depth texture (single-sample R32_FLOAT, or R32_TYPELESS) into a staging texture, waits for it and returns the values. The wait is the
/// one-off stall.
///
/// # Safety
/// `dev` / `ctx` live device and immediate context; `depth` a live 2D texture of that device.
pub(crate) unsafe fn read_depth(dev: Obj, ctx: Obj, depth: Obj) -> Result<DepthData, String> {
    let _ig = IgnoreScope::new();
    unsafe {
        let mut d = Tex2dDesc::default();
        let get_desc: unsafe extern "system" fn(Obj, *mut Tex2dDesc) = std::mem::transmute(vfn(depth, vt::TEX2D_GET_DESC));
        get_desc(depth, &mut d);
        if d.samples != 1 || d.array != 1 || !(d.format == 41 || d.format == 39) || d.width == 0 || d.height == 0 {
            return Err(format!("the depth texture is {}x{} format {} samples {} (expected a single-sample R32_FLOAT)", d.width, d.height, d.format, d.samples));
        }
        let st = create_staging(dev, d.width, d.height, d.format);
        if st.is_null() {
            return Err("could not create the staging texture for the depth".to_string());
        }
        copy_resource(ctx, st, depth);
        let bytes = read_texture(ctx, st, 0, d.width, d.height, 4);
        release(st);
        let bytes = bytes.ok_or("could not map the depth copy")?;
        let data = bytes.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
        Ok(DepthData { w: d.width, h: d.height, data })
    }
}

// ================================================================================================================
// Text of a shot
// ================================================================================================================

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Single,
    PairMod,
    PairVanilla,
}

/// A shot that belongs to photo mode (`photo.rs`): written to `GW2Graphics\photos` at full resolution.
#[derive(Clone, Copy, Debug)]
struct PhotoTag {
    /// The picture is the grabbed pre-UI scene (display-referred): converted with the final pass' maths first.
    scene: Option<photo::SceneMap>,
}

struct Meta {
    kind: Kind,
    id: u64,
    /// Paper white (nits) the shot was taken with.
    paper: f32,
    text: String,
    photo: Option<PhotoTag>,
    /// The engine's linear depth read back for this shot (debug shots of the mod only); the writer thread turns it into `status_depth=`.
    depth: Option<DepthData>,
}

fn fmt_name(f: u32) -> String {
    match f {
        10 => "R16G16B16A16_FLOAT (scRGB)".to_string(),
        87 => "B8G8R8A8_UNORM".to_string(),
        91 => "B8G8R8A8_UNORM_SRGB".to_string(),
        28 => "R8G8B8A8_UNORM".to_string(),
        29 => "R8G8B8A8_UNORM_SRGB".to_string(),
        o => format!("DXGI format {o}"),
    }
}

/// Every effective setting and the overlay's status lines at this moment.
pub fn settings_text(label: &str, frame: u64, screen: (u32, u32), fmt: u32) -> String {
    let mut s = String::new();
    let mut kv = |k: &str, v: String| {
        s.push_str(k);
        s.push('=');
        s.push_str(&v);
        s.push('\n');
    };
    kv("capture", label.to_string());
    kv("frame", format!("{frame} (presents since the debug mode started; resolved frames {})", resolve::frames_resolved()));
    kv("screen", format!("{}x{}", screen.0, screen.1));
    kv("backbuffer_format", fmt_name(fmt));
    kv("output_mode", format!("{:?}", output::mode()));
    kv("enabled", gate::user_enabled().to_string());
    kv("hdr_upgrade", (!crate::upgrade::forced_off()).to_string());
    kv("hdr_clamp", crate::clampemu::status_text());
    kv("debug_view", format!("{}", resolve::debug_view()));
    kv("debug_level", format!("{}", resolve::debug_level()));
    kv("vanilla_emulation", hdr_shaders::vanilla().to_string());
    kv("paper_white_nits", format!("{}", output::paper_white()));
    kv("peak_nits", format!("{}", output::user_peak()));
    kv("effective_peak_nits", format!("{}", output::peak()));
    kv("display_max_nits", output::display_max_luminance().map_or("unknown".to_string(), |v| format!("{v:.0}")));
    kv("sdr_gamma", if output::active_sdr_gamma() >= 1.0 { format!("{} (in use)", output::active_sdr_gamma()) } else { format!("srgb in use (setting {})", output::sdr_gamma()) });
    kv("bloom_strength", format!("{}", hdr_shaders::bloom_strength()));
    kv("highlight_boost", format!("{}", resolve::highlight()));
    kv("ao_strength", format!("{}", ao::strength()));
    kv("ao_radius", format!("{}", ao::radius()));
    kv("halo_strength", format!("{}", halo::strength()));
    kv("halo_radius", format!("{}", halo::radius()));
    kv("ssr_strength", format!("{}", ssr::strength()));
    kv("ssr_range", format!("{}", ssr::range()));
    kv("ssr_classes", format!("{}", ssr::classes()));
    kv("ssr_smart", format!("{}", ssr::smart()));
    kv("ssr_hue_filter", format!("{}", ssr::hue_filter()));
    kv("ssr_flatness", format!("{}", ssr::flatness()));
    kv("ssr_water_boost", format!("{}", ssr::water_boost()));
    kv("water_depth_look", format!("{}", ssr::depth_look()));
    kv("ssr_matte_cut", format!("{}", ssr::matte_cut()));
    kv("shafts_strength", format!("{}", shafts::strength()));
    kv("shafts_length", format!("{}", shafts::length()));
    kv("contact_strength", format!("{}", crate::contact::strength()));
    kv("contact_length", format!("{}", crate::contact::length()));
    kv("contact_sun", crate::contact::mode().to_string());
    kv("aerial_strength", format!("{}", crate::aerial::strength()));
    kv("aerial_distance", format!("{}", crate::aerial::distance()));
    kv("depth_unit", format!("{}", crate::aerial::unit()));
    kv("aniso_level", format!("{}", crate::aniso::level()));
    kv("aniso_filters", format!("{}", crate::aniso::filters()));
    kv("mip_bias", format!("{}", crate::aniso::bias()));
    kv("smaa", format!("{}", crate::aa::smaa() as u8));
    kv("smaa_threshold", format!("{}", crate::aa::threshold()));
    kv("sharpen_strength", format!("{}", crate::aa::sharpen()));
    kv("autoexp_strength", format!("{}", crate::exposure::strength()));
    kv("autoexp_target", format!("{}", crate::exposure::target()));
    kv("autoexp_range", format!("{}", crate::exposure::range()));
    kv("adapt_up", format!("{}", crate::exposure::adapt_up()));
    kv("adapt_down", format!("{}", crate::exposure::adapt_down()));
    kv("localtone_strength", format!("{}", crate::exposure::local_strength()));
    kv("lens_strength", format!("{}", crate::lens::strength()));
    kv("lens_threshold", format!("{}", crate::lens::threshold()));
    kv("lens_ghost", format!("{}", crate::lens::ghost()));
    kv("lens_streak_length", format!("{}", crate::lens::streak_length()));
    kv("lens_max_size", format!("{}", crate::lens::max_size()));
    kv("lens_color_reject", format!("{}", crate::lens::color_reject()));
    let g = crate::grading::get();
    kv("grade_saturation", format!("{}", g.saturation));
    kv("grade_contrast", format!("{}", g.contrast));
    kv("grade_temperature", format!("{}", g.temperature));
    kv("grade_tint", format!("{}", g.tint));
    kv("grade_vibrance", format!("{}", g.vibrance));
    kv("grade_shadows", format!("{}", g.shadows));
    kv("grade_highlights", format!("{}", g.highlights));
    kv("grade_lut_strength", format!("{}", crate::grading::lut_strength()));
    kv("fur_strength", format!("{}", datapath::fur_set()));
    let fl = datapath::fur_look();
    kv("fur_amp", format!("{}", fl.amp));
    kv("fur_scale", format!("{}", fl.scale));
    kv("fur_length", format!("{}", fl.length));
    kv("fur_detail", format!("{}", fl.detail));
    kv("fur_rim", format!("{}", fl.rim));
    kv("fur_sss", format!("{}", fl.sss));
    kv("fur_view", format!("{}", fl.view));
    kv("status_furlook", format!("{}; {}", datapath::fur_look_text(), crate::shellfur::gate_text()));
    kv("fur_ps", crate::furmask::selector_text().0);
    kv("fur_vs", crate::furmask::selector_text().1);
    kv("fur_min_vcb", format!("{}", crate::furmask::min_vcb()));
    kv("fur_debug", format!("{}", crate::furmask::debug() as u8));
    kv("fur_uv", crate::furmask::uv_pref_text());
    kv("status_fur", crate::furmask::status_text());
    kv("rim_strength", format!("{}", datapath::rim()));
    kv("rim_width", format!("{}", datapath::rim_width()));
    kv("subsurface_strength", format!("{}", datapath::subsurface()));
    kv("rim_ps", crate::furmask::rim_selector_text());
    kv("rim_min_vcb", format!("{}", crate::furmask::rim_min_vcb()));
    kv("rim_debug", format!("{}", crate::furmask::rim_debug() as u8));
    kv("status_rim", format!("{}; {}", crate::furmask::rim_status_text(), crate::shellfur::gate_text()));
    kv("fur_occlusion", format!("{}", crate::furmask::occlusion() as u8));
    kv("status_occlusion", crate::furmask::probe_text());
    let sc = crate::shellfur::cfg();
    for line in sc.ini().lines().filter(|l| !l.starts_with('#')) {
        if let Some((k, v)) = line.split_once('=') {
            kv(k, v.to_string());
        }
    }
    // A debug capture also writes the island dumps of the newest analyses (furmesh-*.txt, furisland-*.png).
    crate::furisland::dump_on_capture();
    kv("status_shells", crate::shellfur::status_text());
    kv("status_bloom", crate::bloomfix::status_text());
    kv("status_ao", ao::status_text());
    kv("status_halo", halo::status_text());
    kv("status_highlight", crate::datapath::highlight_status_text());
    kv("status_reflections", ssr::status_text());
    kv("status_shafts", shafts::status_text());
    kv("status_contact", crate::contact::status_text());
    kv("status_aerial", crate::aerial::status_text());
    kv("status_aa", crate::aa::status_text());
    kv("status_lens", crate::look::status_lens());
    kv("status_toggles", crate::toggles::status_text());
    kv("status_grade", crate::look::status_grade());
    crate::exposure::probe_state(); // the debug capture may stall the GPU a moment for the 1x1 exposure state
    kv("status_exposure", crate::exposure::status_text());
    kv("status_localtone", crate::exposure::status_local_text());
    kv("status_aniso", crate::aniso::status_text());
    kv("status_final_pass", output::final_pass_status().text().to_string());
    kv("status_rule", output::reason());
    kv("status_hdr_format", output::format_status());
    kv("status_calibration", crate::calib::status_text(output::calibration_available(), crate::log::uptime_ms()));
    kv("status_photo", photo::status_text());
    kv("status_perf", perf_status());
    s
}

fn stat_rows(s: &Stats) -> Vec<(&'static str, String)> {
    vec![
        ("pixels", s.pixels.to_string()),
        ("frac_any_channel_gt_1.0", format!("{:.4}", s.frac_gt1)),
        ("frac_any_channel_gt_2.0", format!("{:.4}", s.frac_gt2)),
        ("frac_any_channel_gt_4.0", format!("{:.4}", s.frac_gt4)),
        ("p99_luminance", format!("{:.3}", s.p99_lum)),
        ("mean_luminance", format!("{:.4}", s.mean_lum)),
        ("mean_saturation_above_1.0", format!("{:.3}", s.mean_sat_above1)),
    ]
}

const STATS_NOTE: &str = "statistics: linear light, 1.0 = paper white, computed on the full-resolution data before clipping (luminance = Rec.709 weights; p99 from a log histogram, about 2 % resolution)";

pub fn single_txt(id: u64, text: &str, st: &Stats) -> String {
    let mut s = format!("GW2 Enhanced Graphics debug shot {id}\nfiles: shot-{id}.png (also latest.png)\nPNG: sRGB, half resolution; scRGB value / (paper_white/80), clamped (what an SDR monitor would show)\n{STATS_NOTE}\n\n== statistics ==\n");
    for (k, v) in stat_rows(st) {
        s.push_str(&format!("{k}={v}\n"));
    }
    s.push_str("\n== settings ==\n");
    s.push_str(text);
    s
}

pub fn pair_txt(id: u64, mod_text: &str, mod_stats: &Stats, van: Option<(&str, &Stats, Option<DiffStats>)>, note: &str) -> String {
    let mut s = format!(
        "GW2 Enhanced Graphics debug pair {id}\nfiles: pair-{id}-mod.png pair-{id}-vanilla.png (also latest-mod.png, latest-vanilla.png)\nvanilla = the mod's own vanilla emulation (what you see with Enabled unchecked), not a pixel-identical original\nPNG: sRGB, half resolution; scRGB value / (paper_white/80), clamped; each shot uses the paper white it was taken with\n{STATS_NOTE}\n"
    );
    if !note.is_empty() {
        s.push_str(&format!("note: {note}\n"));
    }
    s.push_str("\n== statistics (mod | vanilla) ==\n");
    let a = stat_rows(mod_stats);
    let b = van.map(|v| stat_rows(v.1));
    for (i, (k, v)) in a.iter().enumerate() {
        let vv = b.as_ref().map_or("-".to_string(), |b| b[i].1.clone());
        s.push_str(&format!("{k:<28} {v:>12} | {vv}\n"));
    }
    if let Some((_, _, Some(d))) = van {
        s.push_str(&format!(
            "\n== difference (half resolution, display-encoded luminance 0..1) ==\nmean_abs_luminance_difference={:.4}\nfraction_pixels_differing_more_than_10pct={:.4}\n",
            d.mean_abs_luma, d.frac_over_10pct
        ));
    }
    s.push_str("\n== MOD settings ==\n");
    s.push_str(mod_text);
    if let Some((t, _, _)) = van {
        s.push_str("\n== VANILLA settings ==\n");
        s.push_str(t);
    }
    s
}

// ================================================================================================================
// Background writer
// ================================================================================================================

struct Job {
    meta: Meta,
    w: u32,
    h: u32,
    fmt: PixFmt,
    scale: f32,
    data: Vec<u8>,
}

enum Msg {
    Image(Job),
    /// The pair did not complete: write the text of what exists.
    Abort(u64, String),
}

static TX: OnceLock<SyncSender<Msg>> = OnceLock::new();
static DIR: OnceLock<PathBuf> = OnceLock::new();
static PHOTO_DIR: OnceLock<PathBuf> = OnceLock::new();

pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let mut f = std::fs::File::create(&tmp)?;
    f.write_all(bytes)?;
    drop(f);
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

fn retention(dir: &Path, prefix: &str, keep: usize) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let names: Vec<String> = rd.filter_map(|e| e.ok()).filter_map(|e| e.file_name().into_string().ok()).collect();
    for n in stale_files(&names, prefix, keep) {
        let _ = std::fs::remove_file(dir.join(n));
    }
}

struct PendingMod {
    id: u64,
    text: String,
    stats: Stats,
    luma: Vec<f32>,
    /// Photo pair: the stem of the files (None = a debug pair).
    photo: Option<String>,
}

fn worker(rx: std::sync::mpsc::Receiver<Msg>, dir: PathBuf) {
    let mut pending: Option<PendingMod> = None;
    while let Ok(msg) = rx.recv() {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| process(msg, &dir, &mut pending)));
        match r {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                log!("debug: {e}");
                set_status(format!("error: {e}"));
            }
            Err(_) => set_status("error: the writer panicked"),
        }
    }
}

fn process(msg: Msg, dir: &Path, pending: &mut Option<PendingMod>) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let io = |what: &str, r: std::io::Result<()>| r.map_err(|e| format!("{what}: {e}"));
    match msg {
        Msg::Abort(id, why) => {
            if pending.as_ref().is_some_and(|p| p.id == id && p.photo.is_some()) {
                let Some(p) = pending.take() else { return Ok(()) };
                // A photo pair that did not complete: the mod shot exists, say so next to it.
                let stem = p.photo.clone().unwrap_or_default();
                let t = format!("photo: {stem}\nthe pair is incomplete: {why}\n\n{}", pair_txt(id, &p.text, &p.stats, None, &format!("pair incomplete: {why}")));
                if let Some(pd) = PHOTO_DIR.get() {
                    let _ = write_atomic(&pd.join(format!("{stem}.txt")), t.as_bytes());
                }
                photo::set_status(format!("photo pair {stem}: only the mod shot ({why})"));
                return Ok(());
            }
            if let Some(p) = pending.take().filter(|p| p.id == id) {
                let t = pair_txt(id, &p.text, &p.stats, None, &format!("pair incomplete: {why}"));
                io("pair txt", write_atomic(&dir.join(format!("pair-{id}.txt")), t.as_bytes()))?;
                io("latest.txt", write_atomic(&dir.join("latest.txt"), t.as_bytes()))?;
                set_status(format!("pair {id}: only the mod shot ({why})"));
            }
            Ok(())
        }
        Msg::Image(job) if job.meta.photo.is_some() => process_photo(job, pending),
        Msg::Image(mut job) => {
            if let Some(d) = job.meta.depth.take() {
                let line = depth_status(&d.data, d.w as usize, d.h as usize);
                drop(d);
                job.meta.text.push_str(&format!("status_depth={line}\n"));
            }
            let c = convert(&job.data, job.w, job.h, job.fmt, job.scale).ok_or("could not convert the back buffer")?;
            drop(job.data);
            let png = encode_png_rgb8_text(c.w, c.h, &c.rgb, Some(&job.meta.text));
            let id = job.meta.id;
            match job.meta.kind {
                Kind::Single => {
                    io("png", write_atomic(&dir.join(format!("shot-{id}.png")), &png))?;
                    let t = single_txt(id, &job.meta.text, &c.stats);
                    io("txt", write_atomic(&dir.join(format!("shot-{id}.txt")), t.as_bytes()))?;
                    io("latest.png", write_atomic(&dir.join("latest.png"), &png))?;
                    io("latest.txt", write_atomic(&dir.join("latest.txt"), t.as_bytes()))?;
                    retention(dir, "shot-", 12);
                    set_status(format!("saved shot-{id}.png ({}x{})", c.w, c.h));
                }
                Kind::PairMod => {
                    io("png", write_atomic(&dir.join(format!("pair-{id}-mod.png")), &png))?;
                    io("latest-mod.png", write_atomic(&dir.join("latest-mod.png"), &png))?;
                    io("latest.png", write_atomic(&dir.join("latest.png"), &png))?;
                    *pending = Some(PendingMod { id, text: job.meta.text, stats: c.stats, luma: c.luma, photo: None });
                    set_status(format!("pair {id}: mod shot saved, waiting for the vanilla one"));
                }
                Kind::PairVanilla => {
                    io("png", write_atomic(&dir.join(format!("pair-{id}-vanilla.png")), &png))?;
                    io("latest-vanilla.png", write_atomic(&dir.join("latest-vanilla.png"), &png))?;
                    let Some(m) = pending.take().filter(|p| p.id == id) else {
                        return Err(format!("vanilla shot {id} without its mod shot"));
                    };
                    let d = diff_stats(&m.luma, &c.luma);
                    let t = pair_txt(id, &m.text, &m.stats, Some((&job.meta.text, &c.stats, d)), "");
                    io("pair txt", write_atomic(&dir.join(format!("pair-{id}.txt")), t.as_bytes()))?;
                    io("latest.txt", write_atomic(&dir.join("latest.txt"), t.as_bytes()))?;
                    retention(dir, "pair-", 8);
                    set_status(format!("saved pair-{id} (mod + vanilla, {}x{})", c.w, c.h));
                }
            }
            Ok(())
        }
    }
}

/// Photo mode: full resolution PNG + Radiance .hdr + settings text into `GW2Graphics\photos`. A problem (low disk space, write error)
/// is reported in `status_photo=` and the log; the debug status is left alone.
fn process_photo(job: Job, pending: &mut Option<PendingMod>) -> Result<(), String> {
    let r = photo_inner(job, pending);
    if let Err(e) = &r {
        log!("photo: {e}");
        photo::set_status(format!("not saved: {e}"));
    }
    Ok(())
}

fn photo_inner(job: Job, pending: &mut Option<PendingMod>) -> Result<(), String> {
    let dir = PHOTO_DIR.get().ok_or("no photo folder")?.clone();
    let tag = job.meta.photo.ok_or("not a photo")?;
    let Job { meta, w, h, fmt, mut scale, mut data } = job;
    if let (Some(map), PixFmt::F16) = (tag.scene, fmt) {
        photo::scene_to_scrgb_f16(&mut data, &map);
        scale = 80.0 / if map.hdr { map.paper.max(1.0) } else { 80.0 };
    }
    let c = convert(&data, w, h, fmt, scale).ok_or("could not convert the capture")?;
    let lin = photo::linear_rows(&data, w, h, fmt, scale).ok_or("could not convert the capture")?;
    drop(data);
    let id = meta.id;
    let taken = |s: &str| ["", "-mod", "-vanilla"].iter().any(|x| dir.join(format!("{s}{x}.png")).exists()) || dir.join(format!("{s}.txt")).exists();
    let note = if tag.scene.is_some() { " (3D scene without the UI)" } else { "" };
    match meta.kind {
        Kind::Single => {
            let stem = photo::unique_stem(id, taken);
            let saved = photo::save_files(&dir, &stem, "", w, h, &lin, &meta.text, true)?;
            let t = format!(
                "GW2 Enhanced Graphics photo {stem}{note}\nfiles: {stem}.png (sRGB, full resolution, paper white = 1.0, highlights clipped), {stem}.hdr (Radiance RGBE, linear, 1.0 = paper white, values above 1.0 kept)\n{STATS_NOTE}\n\n== statistics ==\n{}\n== settings ==\n{}",
                stat_rows(&c.stats).iter().map(|(k, v)| format!("{k}={v}\n")).collect::<String>(),
                meta.text
            );
            write_atomic(&dir.join(format!("{stem}.txt")), t.as_bytes()).map_err(|e| format!("txt: {e}"))?;
            photo::note_saved();
            photo::set_status(format!("saved {} ({w}x{h}{note}) + .hdr", saved.png.file_name().and_then(|n| n.to_str()).unwrap_or(&stem)));
        }
        Kind::PairMod => {
            let stem = photo::unique_stem(id, taken);
            photo::save_files(&dir, &stem, "-mod", w, h, &lin, &meta.text, true)?;
            *pending = Some(PendingMod { id, text: meta.text, stats: c.stats, luma: c.luma, photo: Some(stem.clone()) });
            photo::set_status(format!("photo pair {stem}: mod shot saved, waiting for the vanilla one"));
        }
        Kind::PairVanilla => {
            if !pending.as_ref().is_some_and(|p| p.id == id && p.photo.is_some()) {
                return Err("the vanilla shot has no mod shot (it was skipped)".to_string());
            }
            let Some(m) = pending.take() else { return Ok(()) };
            let stem = m.photo.clone().unwrap_or_default();
            photo::save_files(&dir, &stem, "-vanilla", w, h, &lin, &meta.text, true)?;
            let d = diff_stats(&m.luma, &c.luma);
            let t = format!("photo pair {stem}: {stem}-mod.png/.hdr and {stem}-vanilla.png/.hdr (full resolution)\n\n{}", pair_txt(id, &m.text, &m.stats, Some((&meta.text, &c.stats, d)), ""));
            write_atomic(&dir.join(format!("{stem}.txt")), t.as_bytes()).map_err(|e| format!("txt: {e}"))?;
            photo::note_saved();
            photo::set_status(format!("saved pair {stem} (mod + vanilla, {w}x{h})"));
        }
    }
    Ok(())
}

fn send(msg: Msg) -> bool {
    TX.get().is_some_and(|tx| tx.try_send(msg).is_ok())
}

// ================================================================================================================
// GPU: copy at present time, read back without waiting
// ================================================================================================================

const SWAPCHAIN_GET_BUFFER: usize = 9;
const IID_TEXTURE2D: Guid = Guid(0x6f15aaf2, 0xd208, 0x4e89, [0x9a, 0xb4, 0x48, 0x95, 0x35, 0xd3, 0x4f, 0x9c]);

/// A CPU-readable staging texture of the back buffer's size and format.
pub(crate) unsafe fn create_staging(dev: Obj, w: u32, h: u32, fmt: u32) -> Obj {
    let _ig = IgnoreScope::new();
    unsafe {
        let desc = Tex2dDesc { width: w, height: h, mips: 1, array: 1, format: fmt, samples: 1, quality: 0, usage: 3 /* STAGING */, bind: 0, cpu: 0x20000 /* CPU READ */, misc: 0 };
        let create: unsafe extern "system" fn(Obj, *const Tex2dDesc, *const c_void, *mut Obj) -> i32 = std::mem::transmute(vfn(dev, vt::DEV_CREATE_TEXTURE2D));
        let mut out: Obj = std::ptr::null_mut();
        if create(dev, &desc, std::ptr::null(), &mut out) < 0 {
            return std::ptr::null_mut();
        }
        out
    }
}

/// `CopyResource(dst, src)`: GPU side, does not wait.
pub(crate) unsafe fn copy_resource(ctx: Obj, dst: Obj, src: Obj) {
    let _ig = IgnoreScope::new();
    unsafe {
        let copy: unsafe extern "system" fn(Obj, Obj, Obj) = std::mem::transmute(vfn(ctx, vt::CTX_COPY_RESOURCE));
        copy(ctx, dst, src);
    }
}

/// Maps a staging texture and copies its rows into a tight `Vec`. `flags` 0 waits, `0x100000` is `DO_NOT_WAIT` (None
/// while the copy is not finished).
pub(crate) unsafe fn read_texture(ctx: Obj, staging: Obj, flags: u32, w: u32, h: u32, bpp: usize) -> Option<Vec<u8>> {
    unsafe {
        let mut mapped: [usize; 3] = [0; 3]; // D3D11_MAPPED_SUBRESOURCE { pData, RowPitch u32, DepthPitch u32 }
        let map: unsafe extern "system" fn(Obj, Obj, u32, u32, u32, *mut [usize; 3]) -> i32 = std::mem::transmute(vfn(ctx, vt::CTX_MAP));
        if map(ctx, staging, 0, 1 /* READ */, flags, &mut mapped) < 0 || mapped[0] == 0 {
            return None;
        }
        let pitch = (mapped[1] & 0xffff_ffff) as usize;
        let row = w as usize * bpp;
        let mut v = Vec::with_capacity(row * h as usize);
        if pitch >= row {
            for y in 0..h as usize {
                v.extend_from_slice(std::slice::from_raw_parts((mapped[0] + y * pitch) as *const u8, row));
            }
        }
        let unmap: unsafe extern "system" fn(Obj, Obj, u32) = std::mem::transmute(vfn(ctx, vt::CTX_UNMAP));
        unmap(ctx, staging, 0);
        (v.len() == row * h as usize).then_some(v)
    }
}

struct Gpu {
    device: usize,
    w: u32,
    h: u32,
    fmt: u32,
    staging: [Obj; RING],
    slots: SlotRing,
    metas: [Option<Meta>; RING],
}

unsafe impl Send for Gpu {}

impl Drop for Gpu {
    fn drop(&mut self) {
        unsafe {
            for o in self.staging {
                release(o);
            }
        }
    }
}

impl Gpu {
    unsafe fn new(dev: Obj, w: u32, h: u32, fmt: u32) -> Option<Gpu> {
        let nul = std::ptr::null_mut();
        let mut g = Gpu { device: dev as usize, w, h, fmt, staging: [nul; RING], slots: SlotRing::new(), metas: [const { None }; RING] };
        for i in 0..RING {
            g.staging[i] = unsafe { create_staging(dev, w, h, fmt) };
            if g.staging[i].is_null() {
                return None;
            }
        }
        Some(g)
    }
}

/// A photo with `photo_hide_ui` is waiting for the pre-UI scene copy.
#[derive(Clone, Debug)]
struct SceneWait {
    since: u64,
    id: u64,
    text: String,
}

struct State {
    machine: PairMachine,
    guard: Option<ForceGuard>,
    last_ms: Option<u64>,
    gpu: Option<Gpu>,
    pair_id: u64,
    /// The running pair is a photo pair (its shots go to the photo folder).
    pair_photo: bool,
    scene_wait: Option<SceneWait>,
    /// The depth read-back `prepare_depth` made ready for the shot that is issued right after it.
    depth: Option<DepthData>,
    /// Why there is none.
    depth_note: Option<String>,
}

static STATE: Mutex<State> = Mutex::new(State { machine: PairMachine::new(), guard: None, last_ms: None, gpu: None, pair_id: 0, pair_photo: false, scene_wait: None, depth: None, depth_note: None });

/// Called once per present, BEFORE the overlay is drawn (so the copy has no overlay in it). Never unwinds, never waits
/// for the GPU. Cheap (a few atomics) while nothing is requested.
pub fn on_frame() {
    let _ = std::panic::catch_unwind(perf_tick);
    let r = std::panic::catch_unwind(|| unsafe { frame_inner() });
    if r.is_err() {
        restore_now("panic in on_frame");
        set_status("error: internal panic (user values restored)");
    }
}

unsafe fn frame_inner() {
    let frame = FRAME.fetch_add(1, Relaxed) + 1;
    depth_housekeeping(frame);
    let Ok(mut st) = STATE.try_lock() else { return };
    let now = crate::log::uptime_ms();
    let want_single = REQ_SINGLE.load(Relaxed);
    let want_pair = REQ_PAIR.load(Relaxed);
    let secs = capture_seconds();
    let running = st.machine.is_forced();
    photo::tick(frame);
    let photo_work = photo::queued().is_some() || st.scene_wait.is_some();
    let pending = st.gpu.as_ref().is_some_and(|g| g.slots.pending_count() > 0);
    // The watchdog thread may have restored already: the pair is over then.
    if running && !forced() {
        st.machine.cancel();
        st.guard = None;
        set_status("pair aborted (watchdog restored the user values)");
        st.pair_photo = false;
        send(Msg::Abort(st.pair_id, "watchdog".to_string()));
    }
    let due = !st.machine.is_forced() && capture_due(now, st.last_ms, secs);
    if !(want_single || want_pair || due || st.machine.is_forced() || pending || photo_work) {
        return;
    }
    let sc = output::game_swapchain() as Obj;
    if sc.is_null() || DIR.get().is_none() {
        end_pair(&mut st, "no swapchain");
        return;
    }
    unsafe {
        let dev = crate::d3d::device_from_swapchain(sc);
        if dev.is_null() {
            end_pair(&mut st, "no device");
            return;
        }
        let ctx = crate::d3d::immediate_context_of(dev);
        if !ctx.is_null() {
            frame_gpu(&mut st, frame, now, sc, dev, ctx, want_single, want_pair, due);
        }
        release(ctx);
        release(dev);
    }
}

/// Ends a running pair on an error path: restores and tells the writer.
fn end_pair(st: &mut State, why: &str) {
    if st.machine.cancel() {
        st.guard = None; // drop = restore
        restore_now(why);
        set_status(format!("pair aborted: {why}"));
        st.pair_photo = false;
        send(Msg::Abort(st.pair_id, why.to_string()));
    }
}

#[allow(clippy::too_many_arguments)]
unsafe fn frame_gpu(st: &mut State, frame: u64, now: u64, sc: Obj, dev: Obj, ctx: Obj, want_single: bool, want_pair: bool, due: bool) {
    unsafe {
        let mut bb: Obj = std::ptr::null_mut();
        let get: unsafe extern "system" fn(Obj, u32, *const Guid, *mut Obj) -> i32 = std::mem::transmute(vfn(sc, SWAPCHAIN_GET_BUFFER));
        if get(sc, 0, &IID_TEXTURE2D, &mut bb) < 0 || bb.is_null() {
            end_pair(st, "GetBuffer failed");
            set_status("error: GetBuffer(0) failed");
            return;
        }
        let mut d = Tex2dDesc::default();
        let get_desc: unsafe extern "system" fn(Obj, *mut Tex2dDesc) = std::mem::transmute(vfn(bb, vt::TEX2D_GET_DESC));
        get_desc(bb, &mut d);
        let res = capture_step(st, frame, now, bb, &d, dev, ctx, want_single, want_pair, due);
        release(bb);
        if let Err(e) = res {
            end_pair(st, &e);
            set_status(format!("error: {e}"));
        }
    }
}

#[allow(clippy::too_many_arguments)]
unsafe fn capture_step(st: &mut State, frame: u64, now: u64, bb: Obj, d: &Tex2dDesc, dev: Obj, ctx: Obj, want_single: bool, want_pair: bool, due: bool) -> Result<(), String> {
    unsafe {
        let Some(pf) = pixfmt(d.format) else { return Err(format!("unsupported back buffer format {}", d.format)) };
        if d.samples > 1 {
            return Err("MSAA back buffer".to_string());
        }
        if st.gpu.as_ref().is_none_or(|g| g.device != dev as usize || g.w != d.width || g.h != d.height || g.fmt != d.format) {
            st.gpu = None;
            st.gpu = Gpu::new(dev, d.width, d.height, d.format);
            if st.gpu.is_none() {
                return Err("could not create the staging textures".to_string());
            }
        }
        let screen = (d.width, d.height);
        let bpp = bytes_per_px(pf);
        // 1. Read the copies that are old enough (never waits).
        {
            let g = st.gpu.as_mut().ok_or("no ring")?;
            for i in g.slots.ready(frame) {
                if let Some(data) = read_texture(ctx, g.staging[i], 0x10_0000, g.w, g.h, bpp) {
                    g.slots.release(i);
                    if let Some(meta) = g.metas[i].take() {
                        let scale = if pf == PixFmt::F16 || pf == PixFmt::Rgb10A2 { 80.0 / meta.paper.max(1.0) } else { 1.0 };
                        if !send(Msg::Image(Job { meta, w: g.w, h: g.h, fmt: pf, scale, data })) {
                            set_status("a capture was dropped: the writer is busy");
                        }
                    }
                }
            }
            for i in g.slots.expired(frame) {
                g.slots.release(i);
                g.metas[i] = None;
                set_status("a capture was dropped: the GPU copy never finished");
            }
        }
        // 2. A running pair: is the pipeline rebuilt for the disabled state?
        let settled = shaders::substitutes().pending_count() == 0;
        match st.machine.step(now, frame, settled) {
            PairStep::Capture => {
                let id = st.pair_id;
                let tag = st.pair_photo.then_some(PhotoTag { scene: None });
                let r = issue(st, frame, bb, ctx, (Kind::PairVanilla, id), screen, d.format, tag);
                st.guard = None; // drop = restore the user's values
                restore_now("vanilla shot taken");
                st.pair_photo = false;
                if r.is_err() {
                    send(Msg::Abort(id, "vanilla shot failed".to_string()));
                }
                return r;
            }
            PairStep::Abort => {
                st.guard = None;
                restore_now("pair timed out");
                set_status("pair aborted: the pipeline did not settle in time");
                photo::set_status("photo pair aborted: the pipeline did not settle in time");
                st.pair_photo = false;
                send(Msg::Abort(st.pair_id, "timeout".to_string()));
                return Ok(());
            }
            PairStep::Wait => return Ok(()),
            PairStep::Idle => {}
        }
        // 2b. A photo waiting for the UI-free scene copy.
        if let Some(sw) = st.scene_wait.clone() {
            match photo::take_scene(ctx, frame) {
                Ok(Some(sd)) => {
                    st.scene_wait = None;
                    let hdr = output::mode() == output::OutputMode::ScRgb;
                    let (paper, peak) = (output::paper_white(), output::peak());
                    let (pfmt, scale) = if sd.fmt == 10 { (PixFmt::F16, 80.0 / if hdr { paper.max(1.0) } else { 80.0 }) } else { (PixFmt::Bgra8, 1.0) };
                    let meta = Meta { kind: Kind::Single, id: sw.id, paper, text: sw.text, photo: Some(PhotoTag { scene: (sd.fmt == 10).then_some(photo::SceneMap { hdr, paper, peak, gamma: output::active_sdr_gamma() }) }), depth: None };
                    photo::note_scene("UI-free scene copy used (3D scene with the mod's effects, without the UI)");
                    photo::set_status(format!("photo {}: UI-free scene copied, writing", sw.id));
                    if !send(Msg::Image(Job { meta, w: sd.w, h: sd.h, fmt: pfmt, scale, data: sd.data })) {
                        photo::set_status("a photo was dropped: the writer is busy");
                    }
                    return Ok(());
                }
                Ok(None) if photo::scene_step(sw.since, frame) == photo::SceneStep::Wait => return Ok(()),
                other => {
                    // Timeout or error: the normal photo (with the UI) instead.
                    let why = match other {
                        Err(e) => e,
                        _ => "the pre-UI copy pass was not seen".to_string(),
                    };
                    st.scene_wait = None;
                    photo::disarm_scene(&format!("UI-free copy not available ({why}); photos include the UI"));
                    if st.gpu.as_ref().is_some_and(|g| g.slots.free_count() >= 1) {
                        issue(st, frame, bb, ctx, (Kind::Single, sw.id), screen, d.format, Some(PhotoTag { scene: None }))?;
                        photo::set_status(format!("photo {}: no UI-free copy ({why}), taken with the UI", sw.id));
                    } else {
                        photo::set_status("photo skipped: the ring is busy");
                    }
                    return Ok(());
                }
            }
        }
        // 2c. Photos first: one capture at a time, the debug timer and the debug buttons wait for them (and the other way round, a running
        // pair holds the photo back).
        let free = st.gpu.as_ref().map_or(0, |g| g.slots.free_count());
        match photo::arbitrate(photo::queued(), st.machine.is_forced(), st.scene_wait.is_some(), free, gate::user_enabled()) {
            photo::Plan::Idle => {}
            photo::Plan::Wait => return Ok(()),
            photo::Plan::Single => {
                photo::pop_queued();
                let id = next_id();
                if photo::hide_ui() {
                    photo::arm_scene(frame);
                    let text = settings_text("photo, single shot, mod on (3D scene without the UI)", frame, screen, d.format);
                    st.scene_wait = Some(SceneWait { since: frame, id, text });
                    photo::set_status(format!("photo {id}: waiting for the UI-free copy"));
                } else {
                    issue(st, frame, bb, ctx, (Kind::Single, id), screen, d.format, Some(PhotoTag { scene: None }))?;
                    photo::set_status(format!("photo {id} taken, writing"));
                }
                return Ok(());
            }
            photo::Plan::Pair => {
                photo::pop_queued();
                let id = next_id();
                st.pair_id = id;
                issue(st, frame, bb, ctx, (Kind::PairMod, id), screen, d.format, Some(PhotoTag { scene: None }))?;
                if force_off(now) {
                    st.guard = Some(ForceGuard);
                    st.machine.begin(now, frame);
                    st.pair_photo = true;
                    photo::set_status(format!("photo pair {id}: mod shot taken, Enabled forced off for the vanilla one"));
                }
                return Ok(());
            }
        }
        // 3. New captures.
        let pair = want_pair || (due && compare());
        let single = want_single || (due && !compare());
        if pair && gate::user_enabled() && st.gpu.as_ref().is_some_and(|g| g.slots.free_count() >= 2) {
            if !prepare_depth(st, frame) {
                return Ok(()); // the shot waits for the depth of the next frame's light-adjust draw (a request or the timer stays pending)
            }
            REQ_PAIR.store(false, Relaxed);
            REQ_SINGLE.store(false, Relaxed);
            let id = next_id();
            st.pair_id = id;
            st.last_ms = Some(now);
            issue(st, frame, bb, ctx, (Kind::PairMod, id), screen, d.format, None)?;
            if force_off(now) {
                st.guard = Some(ForceGuard);
                st.machine.begin(now, frame);
                set_status(format!("pair {id}: mod shot taken, Enabled forced off for the vanilla one"));
            }
        } else if pair || single {
            // Pair impossible (mod already off, or no free slots) or a plain shot: one mod shot.
            if st.gpu.as_ref().is_some_and(|g| g.slots.free_count() >= 1) {
                if !prepare_depth(st, frame) {
                    return Ok(());
                }
                REQ_PAIR.store(false, Relaxed);
                REQ_SINGLE.store(false, Relaxed);
                st.last_ms = Some(now);
                let id = next_id();
                issue(st, frame, bb, ctx, (Kind::Single, id), screen, d.format, None)?;
                set_status(if pair { format!("shot {id} taken (no pair: the mod is off or the ring is busy)") } else { format!("shot {id} taken") });
            } else if want_single || want_pair {
                set_status("capture skipped: the ring is busy, try again");
                REQ_PAIR.store(false, Relaxed);
                REQ_SINGLE.store(false, Relaxed);
            }
        }
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
unsafe fn issue(st: &mut State, frame: u64, bb: Obj, ctx: Obj, kind: (Kind, u64), screen: (u32, u32), fmt: u32, photo: Option<PhotoTag>) -> Result<(), String> {
    let (kind, id) = kind;
    let label = match (kind, photo.is_some()) {
        (Kind::Single, false) => "single shot, mod on",
        (Kind::Single, true) => "photo, single shot, mod on",
        (Kind::PairMod, false) => "pair, shot A: mod on",
        (Kind::PairMod, true) => "photo pair, shot A: mod on",
        (Kind::PairVanilla, false) => "pair, shot B: vanilla emulation (Enabled forced off)",
        (Kind::PairVanilla, true) => "photo pair, shot B: vanilla emulation (Enabled forced off)",
    };
    let mut text = settings_text(label, frame, screen, fmt);
    // Debug shots of the mod carry the depth telemetry (`status_depth=`): the numbers are made by the writer thread; if there is none, the reason.
    let depth_data = attach_depth(&mut text, photo.is_none() && matches!(kind, Kind::Single | Kind::PairMod), st.depth.take(), st.depth_note.take());
    let g = st.gpu.as_mut().ok_or("no ring")?;
    let i = g.slots.acquire(frame).ok_or("no free staging slot")?;
    unsafe { copy_resource(ctx, g.staging[i], bb) };
    g.metas[i] = Some(Meta { kind, id, paper: output::paper_white(), text, photo, depth: depth_data });
    Ok(())
}

// ================================================================================================================
// Debug thread: watchdog and hot reload
// ================================================================================================================

static RELOAD: Mutex<Option<(Config, Config)>> = Mutex::new(None);

/// The pending config change read from the file, if any (not while a pair is running: the user's values are forced).
pub fn take_reload() -> Option<(Config, Config)> {
    if forced() {
        return None;
    }
    RELOAD.try_lock().ok()?.take()
}

/// Starts the writer and the debug thread (watchdog + hot reload). `path` is `config.ini`, `initial` what it said at start.
pub fn start(path: PathBuf, initial: Config) {
    configure(&initial);
    let dir = path.parent().map(|p| p.join("debug")).unwrap_or_else(|| PathBuf::from("debug"));
    let _ = DIR.set(dir.clone());
    let _ = PHOTO_DIR.set(path.parent().map(|p| p.join("photos")).unwrap_or_else(|| PathBuf::from("photos")));
    let (tx, rx) = sync_channel::<Msg>(2);
    if TX.set(tx).is_ok() {
        let d = dir.clone();
        let _ = std::thread::Builder::new().name("gw2graphics-debugshot".into()).spawn(move || worker(rx, d));
    }
    let _ = std::thread::Builder::new().name("gw2graphics-debug".into()).spawn(move || {
        let mut last_file = initial;
        let mut last_mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        let mut candidate = None;
        let mut ticks = 0u32;
        loop {
            std::thread::sleep(std::time::Duration::from_millis(250));
            let _ = std::panic::catch_unwind(|| watchdog_tick());
            ticks += 1;
            if ticks % 8 != 0 || !hot_reload() {
                continue;
            }
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let m = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
                if m == last_mtime {
                    candidate = None;
                    return;
                }
                // The file is read at the second look with the same modified time (a half-written file is skipped).
                if candidate != m {
                    candidate = m;
                    return;
                }
                last_mtime = m;
                candidate = None;
                let Ok(text) = std::fs::read_to_string(&path) else { return };
                let new = Config::parse(&text);
                if new == last_file {
                    return;
                }
                if let Ok(mut g) = RELOAD.lock() {
                    let old = g.take().map_or_else(|| last_file.clone(), |p| p.0);
                    *g = Some((old, new.clone()));
                }
                last_file = new;
            }));
        }
    });
    log!("debug: started (capture every {} s, compare {}, hot reload {}, folder {})", capture_seconds(), compare(), hot_reload(), dir.display());
}

fn watchdog_tick() {
    let now = crate::log::uptime_ms();
    let expired = lock_saved().is_some_and(|s| s.watchdog_expired(now));
    if expired {
        restore_now("watchdog");
    }
}

/// Logs what a hot reload applied and ignored (called by the overlay).
pub fn log_reload(d: &Diff) {
    if d.is_empty() {
        return;
    }
    if !d.live.is_empty() {
        log!("debug: config.ini changed, applied live: {}", d.live.join(", "));
    }
    for k in &d.ignored {
        log!("debug: config.ini key {k} changed but needs a restart; ignored");
    }
}

#[cfg(test)]
#[path = "debugshot_tests.rs"]
mod tests;
