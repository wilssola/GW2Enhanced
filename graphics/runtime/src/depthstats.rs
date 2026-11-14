//! Depth diagnostic: how does the engine's linear depth encode the sky? See `docs/shafts.md`.
//!
//! The overlay button "Log depth stats" arms it. At the next light-adjust draw (`datapath::bind_current` calls
//! `on_light_adjust`, once) the linear depth texture and the scene colour (pixel shader SRV in slot 0) are copied
//! to staging textures, mapped (a one-off stall of the render thread is fine) and summarised in the log, as one
//! block of lines starting with `depthstats:`:
//!
//! * size, and the counts of depth <= 0, NaN / infinite and > 1e30 (the "invalid" classes),
//! * min, max and the percentiles 1 / 5 / 25 / 50 / 75 / 95 / 99 of the finite positive depths,
//! * a 64-bin histogram of log2(depth) as one line of text,
//! * the fraction of pixels with depth >= 0.99 * max and >= 0.9 * max, and for the 0.99 ones (and for the
//!   invalid ones) the average scene colour and alpha,
//! * a 9 x 5 grid of (depth, scene rgb, alpha) samples.
//!
//! The summary maths (`analyze`, `percentile`, `histogram`, `hist_line`, `decode_pixel`, `format_report`) is pure
//! and host tested; the read-back (`capture`) is tested on WARP.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};

use crate::ao;
use crate::d3d::{IgnoreScope, Obj, Tex2dDesc, release, vfn, vt};
use crate::log;

/// Depth consumer bit of this module (`ao::set_depth_consumer`): the depth is captured while the button is armed.
/// Defined next to the other consumers in `ao` (bit 64) so the bits cannot collide.
pub const DEPTH_FOR_STATS: u32 = ao::DEPTH_FOR_STATS;

/// Depths above this are "huge" (invalid) like in the shaders.
pub const HUGE: f32 = 1e30;
pub const PERCENTS: [f32; 7] = [1.0, 5.0, 25.0, 50.0, 75.0, 95.0, 99.0];
pub const BINS: usize = 64;
pub const GRID: (usize, usize) = (9, 5);

static ARMED: AtomicBool = AtomicBool::new(false);
static TRIES: AtomicU32 = AtomicU32::new(0);
static DONE_COUNT: AtomicU32 = AtomicU32::new(0);

/// The button: capture at the next light-adjust draw that has a depth.
pub fn arm() {
    TRIES.store(0, Relaxed);
    ARMED.store(true, Relaxed);
    ao::set_depth_consumer(DEPTH_FOR_STATS, true);
    log!("depthstats: armed, the next light-adjust draw will be captured");
}

pub fn armed() -> bool {
    ARMED.load(Relaxed)
}

/// Overlay line.
pub fn status_text() -> String {
    let n = DONE_COUNT.load(Relaxed);
    if armed() {
        "armed, waiting for the next scene frame".to_string()
    } else if n > 0 {
        format!("{n} block(s) written to the log")
    } else {
        "not captured yet".to_string()
    }
}

// ---- pure parts ---------------------------------------------------------------------------------------

/// The scene colour as read back: raw bytes of a texture of format `fmt` (DXGI number) and row pitch.
pub struct SceneView<'a> {
    pub bytes: &'a [u8],
    pub pitch: usize,
    pub w: usize,
    pub h: usize,
    pub fmt: u32,
}

/// Bytes per pixel of the formats `decode_pixel` understands, None for others.
pub fn pixel_size(fmt: u32) -> Option<usize> {
    match fmt {
        2 => Some(16),                    // R32G32B32A32_FLOAT
        10 => Some(8),                    // R16G16B16A16_FLOAT
        24 | 28 | 29 | 87 | 91 => Some(4), // R10G10B10A2_UNORM, R8G8B8A8_UNORM(_SRGB), B8G8R8A8_UNORM(_SRGB)
        _ => None,
    }
}

pub fn f16_to_f32(h: u16) -> f32 {
    let (s, e, m) = ((h >> 15) as u32, ((h >> 10) & 0x1f) as i32, (h & 0x3ff) as f32);
    let v = match e {
        0 => m * 2f32.powi(-24),
        31 => if m == 0.0 { f32::INFINITY } else { f32::NAN },
        _ => (1.0 + m / 1024.0) * 2f32.powi(e - 15),
    };
    if s == 1 { -v } else { v }
}

/// Decodes one pixel (rgba as the shader would see it, UNORM = 0..1, SRGB formats left encoded) or None for a format
/// or a slice that is not understood.
pub fn decode_pixel(fmt: u32, b: &[u8]) -> Option<[f32; 4]> {
    let n = pixel_size(fmt)?;
    if b.len() < n {
        return None;
    }
    let f32_at = |i: usize| f32::from_le_bytes([b[i * 4], b[i * 4 + 1], b[i * 4 + 2], b[i * 4 + 3]]);
    let f16_at = |i: usize| f16_to_f32(u16::from_le_bytes([b[i * 2], b[i * 2 + 1]]));
    Some(match fmt {
        2 => [f32_at(0), f32_at(1), f32_at(2), f32_at(3)],
        10 => [f16_at(0), f16_at(1), f16_at(2), f16_at(3)],
        28 | 29 => [b[0] as f32 / 255.0, b[1] as f32 / 255.0, b[2] as f32 / 255.0, b[3] as f32 / 255.0],
        87 | 91 => [b[2] as f32 / 255.0, b[1] as f32 / 255.0, b[0] as f32 / 255.0, b[3] as f32 / 255.0],
        24 => {
            let v = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
            [(v & 0x3ff) as f32 / 1023.0, ((v >> 10) & 0x3ff) as f32 / 1023.0, ((v >> 20) & 0x3ff) as f32 / 1023.0, ((v >> 30) & 3) as f32 / 3.0]
        }
        _ => return None,
    })
}

/// Nearest-rank-with-interpolation percentile (0..100) of an ascending slice; NaN for an empty slice.
pub fn percentile(sorted: &[f32], p: f32) -> f32 {
    if sorted.is_empty() {
        return f32::NAN;
    }
    let pos = (p.clamp(0.0, 100.0) / 100.0) * (sorted.len() - 1) as f32;
    let (i, f) = (pos.floor() as usize, pos.fract());
    let j = (i + 1).min(sorted.len() - 1);
    sorted[i] + (sorted[j] - sorted[i]) * f
}

/// Histogram of log2(value) over `lo..hi` (64 bins; values outside land in the first / last bin).
pub fn histogram(values: &[f32], lo: f32, hi: f32) -> [u32; BINS] {
    let mut h = [0u32; BINS];
    let span = (hi - lo).max(1e-6);
    for &v in values {
        if v > 0.0 && v.is_finite() {
            let b = ((v.log2() - lo) / span * BINS as f32).floor();
            h[(b.max(0.0) as usize).min(BINS - 1)] += 1;
        }
    }
    h
}

/// One line of text for a histogram: '.' = empty bin, '1'..'9' = log-scaled count (9 = the fullest bin).
pub fn hist_line(h: &[u32; BINS]) -> String {
    let max = h.iter().copied().max().unwrap_or(0);
    h.iter()
        .map(|&c| {
            if c == 0 {
                '.'
            } else {
                let l = ((9.0 * (1.0 + c as f32).ln() / (1.0 + max as f32).ln()).ceil() as u32).clamp(1, 9);
                char::from_digit(l, 10).unwrap_or('9')
            }
        })
        .collect()
}

/// Average colour of a group of pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Avg {
    pub n: usize,
    pub rgba: [f32; 4],
}

#[derive(Clone, Debug)]
pub struct Report {
    pub w: usize,
    pub h: usize,
    pub scene_w: usize,
    pub scene_h: usize,
    pub scene_fmt: u32,
    pub scene_ok: bool,
    pub total: usize,
    pub le0: usize,
    pub nonfinite: usize,
    pub huge: usize,
    pub finite: usize,
    pub min: f32,
    pub max: f32,
    pub pct: [f32; 7],
    pub hist_lo: f32,
    pub hist_hi: f32,
    pub hist: [u32; BINS],
    pub far99: usize,
    pub far90: usize,
    pub far99_avg: Avg,
    pub invalid_avg: Avg,
    /// Row-major 9 x 5: (depth, rgba or None when the scene format is not decodable).
    pub grid: Vec<(f32, Option<[f32; 4]>)>,
}

fn scene_at(s: &SceneView, u: f32, v: f32) -> Option<[f32; 4]> {
    let n = pixel_size(s.fmt)?;
    let x = ((u * s.w as f32) as usize).min(s.w.saturating_sub(1));
    let y = ((v * s.h as f32) as usize).min(s.h.saturating_sub(1));
    let off = y * s.pitch + x * n;
    s.bytes.get(off..off + n).and_then(|b| decode_pixel(s.fmt, b))
}

fn add(avg: &mut Avg, px: Option<[f32; 4]>) {
    if let Some(p) = px {
        if p.iter().all(|c| c.is_finite()) {
            avg.n += 1;
            for c in 0..4 {
                avg.rgba[c] += p[c];
            }
        }
    }
}

fn finish(avg: &mut Avg) {
    if avg.n > 0 {
        for c in 0..4 {
            avg.rgba[c] /= avg.n as f32;
        }
    }
}

/// Summarises `depth` (w x h, row-major, tightly packed) and the scene colour.
pub fn analyze(depth: &[f32], w: usize, h: usize, scene: &SceneView) -> Report {
    let total = w * h;
    let (mut le0, mut nonfinite, mut huge) = (0, 0, 0);
    let mut good: Vec<f32> = Vec::with_capacity(total);
    for &d in depth.iter().take(total) {
        if !d.is_finite() {
            nonfinite += 1;
        } else if d <= 0.0 {
            le0 += 1;
        } else if d > HUGE {
            huge += 1;
        } else {
            good.push(d);
        }
    }
    good.sort_unstable_by(|a, b| a.total_cmp(b));
    let finite = good.len();
    let (min, max) = (good.first().copied().unwrap_or(f32::NAN), good.last().copied().unwrap_or(f32::NAN));
    let mut pct = [f32::NAN; 7];
    for (i, p) in PERCENTS.iter().enumerate() {
        pct[i] = percentile(&good, *p);
    }
    let (mut hist_lo, mut hist_hi) = (0.0, 1.0);
    let mut hist = [0u32; BINS];
    if finite > 0 {
        hist_lo = min.log2().floor();
        hist_hi = max.log2().ceil();
        if hist_hi <= hist_lo {
            hist_hi = hist_lo + 1.0;
        }
        hist = histogram(&good, hist_lo, hist_hi);
    }
    let (t99, t90) = (max * 0.99, max * 0.9);
    let (mut far99, mut far90) = (0, 0);
    let mut far99_avg = Avg::default();
    let mut invalid_avg = Avg::default();
    let scene_ok = pixel_size(scene.fmt).is_some() && scene.w > 0 && scene.h > 0;
    for y in 0..h {
        for x in 0..w {
            let Some(&d) = depth.get(y * w + x) else { continue };
            let valid = d.is_finite() && d > 0.0 && d <= HUGE;
            if valid && finite > 0 {
                if d >= t90 {
                    far90 += 1;
                }
                if d >= t99 {
                    far99 += 1;
                    if scene_ok {
                        add(&mut far99_avg, scene_at(scene, (x as f32 + 0.5) / w as f32, (y as f32 + 0.5) / h as f32));
                    }
                }
            } else if !valid && scene_ok {
                add(&mut invalid_avg, scene_at(scene, (x as f32 + 0.5) / w as f32, (y as f32 + 0.5) / h as f32));
            }
        }
    }
    finish(&mut far99_avg);
    finish(&mut invalid_avg);
    let mut grid = Vec::with_capacity(GRID.0 * GRID.1);
    for j in 0..GRID.1 {
        for i in 0..GRID.0 {
            let (u, v) = ((i as f32 + 0.5) / GRID.0 as f32, (j as f32 + 0.5) / GRID.1 as f32);
            let x = ((u * w as f32) as usize).min(w.saturating_sub(1));
            let y = ((v * h as f32) as usize).min(h.saturating_sub(1));
            let d = depth.get(y * w + x).copied().unwrap_or(f32::NAN);
            grid.push((d, scene_at(scene, u, v)));
        }
    }
    Report { w, h, scene_w: scene.w, scene_h: scene.h, scene_fmt: scene.fmt, scene_ok, total, le0, nonfinite, huge, finite, min, max, pct, hist_lo, hist_hi, hist, far99, far90, far99_avg, invalid_avg, grid }
}

/// A number for the log: plain with 3 decimals in a normal range, exponent otherwise; NaN / inf spelled out.
pub fn fmt_num(v: f32) -> String {
    if v.is_nan() {
        "nan".to_string()
    } else if v.is_infinite() {
        if v > 0.0 { "inf".to_string() } else { "-inf".to_string() }
    } else if v == 0.0 {
        "0".to_string()
    } else if v.abs() >= 1e6 || v.abs() < 1e-3 {
        format!("{v:.3e}")
    } else {
        format!("{v:.3}")
    }
}

fn fmt_avg(a: &Avg) -> String {
    if a.n == 0 {
        "none".to_string()
    } else {
        format!("n {} avg rgb ({}, {}, {}) alpha {}", a.n, fmt_num(a.rgba[0]), fmt_num(a.rgba[1]), fmt_num(a.rgba[2]), fmt_num(a.rgba[3]))
    }
}

fn pct_of(n: usize, total: usize) -> String {
    format!("{:.2}%", 100.0 * n as f32 / total.max(1) as f32)
}

/// The log block, one string per line (without the `depthstats:` prefix).
pub fn format_report(r: &Report) -> Vec<String> {
    let mut l = Vec::new();
    l.push(format!("depth {}x{}, scene {}x{} format {}{}", r.w, r.h, r.scene_w, r.scene_h, r.scene_fmt, if r.scene_ok { "" } else { " (not decodable: no colours)" }));
    l.push(format!("invalid: depth<=0 {} ({}), nan/inf {} ({}), >1e30 {} ({}); finite positive {} ({})", r.le0, pct_of(r.le0, r.total), r.nonfinite, pct_of(r.nonfinite, r.total), r.huge, pct_of(r.huge, r.total), r.finite, pct_of(r.finite, r.total)));
    l.push(format!("finite positive: min {} max {}", fmt_num(r.min), fmt_num(r.max)));
    let ps: Vec<String> = PERCENTS.iter().zip(r.pct.iter()).map(|(p, v)| format!("p{p}={}", fmt_num(*v))).collect();
    l.push(format!("percentiles: {}", ps.join(" ")));
    l.push(format!("hist log2(depth) {}..{} in {} bins ('.' empty, 1-9 log count): {}", fmt_num(r.hist_lo), fmt_num(r.hist_hi), BINS, hist_line(&r.hist)));
    l.push(format!("far: depth >= 0.99*max {} ({} of all pixels), >= 0.9*max {} ({})", r.far99, pct_of(r.far99, r.total), r.far90, pct_of(r.far90, r.total)));
    l.push(format!("far (>= 0.99*max) scene colour: {}", fmt_avg(&r.far99_avg)));
    l.push(format!("invalid-depth pixels scene colour: {}", fmt_avg(&r.invalid_avg)));
    for j in 0..GRID.1 {
        let row: Vec<String> = (0..GRID.0)
            .map(|i| {
                let (d, c) = &r.grid[j * GRID.0 + i];
                match c {
                    Some(c) => format!("[{} | {} {} {} | {}]", fmt_num(*d), fmt_num(c[0]), fmt_num(c[1]), fmt_num(c[2]), fmt_num(c[3])),
                    None => format!("[{}]", fmt_num(*d)),
                }
            })
            .collect();
        l.push(format!("grid row {j}: {}", row.join(" ")));
    }
    l
}

// ---- read-back --------------------------------------------------------------------------------------

type MapFn = unsafe extern "system" fn(Obj, Obj, u32, u32, u32, *mut [usize; 3]) -> i32;
type UnmapFn = unsafe extern "system" fn(Obj, Obj, u32);

/// Copies `src` (a 2D texture, description `d`) to a staging texture and returns its bytes and pitch.
unsafe fn read_texture(dev: Obj, ctx: Obj, src: Obj, d: &Tex2dDesc, bytes_per_px: usize) -> Result<(Vec<u8>, usize), String> {
    unsafe {
        let sd = Tex2dDesc { width: d.width, height: d.height, mips: 1, array: 1, format: d.format, samples: 1, quality: 0, usage: 3 /* STAGING */, bind: 0, cpu: 0x20000 /* READ */, misc: 0 };
        let create_tex: unsafe extern "system" fn(Obj, *const Tex2dDesc, *const std::ffi::c_void, *mut Obj) -> i32 = std::mem::transmute(vfn(dev, vt::DEV_CREATE_TEXTURE2D));
        let mut st: Obj = std::ptr::null_mut();
        if create_tex(dev, &sd, std::ptr::null(), &mut st) < 0 || st.is_null() {
            return Err(format!("could not create a staging texture {}x{} format {}", d.width, d.height, d.format));
        }
        let copy: unsafe extern "system" fn(Obj, Obj, Obj) = std::mem::transmute(vfn(ctx, vt::CTX_COPY_RESOURCE));
        copy(ctx, st, src);
        let map: MapFn = std::mem::transmute(vfn(ctx, vt::CTX_MAP));
        let unmap: UnmapFn = std::mem::transmute(vfn(ctx, vt::CTX_UNMAP));
        let mut m: [usize; 3] = [0; 3];
        // READ, no flags: waits for the GPU (the one-off stall).
        if map(ctx, st, 0, 1, 0, &mut m) < 0 || m[0] == 0 {
            release(st);
            return Err("could not map the staging texture".to_string());
        }
        let pitch = m[1] & 0xffff_ffff; // RowPitch (u32); DepthPitch shares the usize
        let rows = d.height as usize;
        let row_bytes = d.width as usize * bytes_per_px;
        let mut out = vec![0u8; pitch.max(row_bytes) * rows];
        let p = pitch.max(row_bytes);
        for y in 0..rows {
            std::ptr::copy_nonoverlapping((m[0] + y * pitch) as *const u8, out.as_mut_ptr().add(y * p), row_bytes);
        }
        unmap(ctx, st, 0);
        release(st);
        Ok((out, p))
    }
}

unsafe fn desc_of(res: Obj) -> Tex2dDesc {
    unsafe {
        let mut d = Tex2dDesc::default();
        let get_desc: unsafe extern "system" fn(Obj, *mut Tex2dDesc) = std::mem::transmute(vfn(res, vt::TEX2D_GET_DESC));
        get_desc(res, &mut d);
        d
    }
}

/// Reads `depth` (R32_FLOAT texture) and `scene` (any texture) back and summarises them.
///
/// # Safety
/// `dev` / `ctx` live device and immediate context; `depth` and `scene` live 2D textures of that device.
pub unsafe fn capture(dev: Obj, ctx: Obj, depth: Obj, scene: Obj) -> Result<Report, String> {
    let _ig = IgnoreScope::new();
    unsafe {
        let dd = desc_of(depth);
        if dd.samples != 1 || dd.array != 1 || !(dd.format == 41 || dd.format == 39) || dd.width == 0 || dd.height == 0 {
            return Err(format!("the depth texture is {}x{} format {} samples {} (expected R32_FLOAT)", dd.width, dd.height, dd.format, dd.samples));
        }
        let sd = desc_of(scene);
        if sd.samples != 1 || sd.array != 1 {
            return Err(format!("the scene colour has samples {} array {}", sd.samples, sd.array));
        }
        let (db, dp) = read_texture(dev, ctx, depth, &dd, 4)?;
        let depths: Vec<f32> = (0..dd.height as usize).flat_map(|y| (0..dd.width as usize).map(move |x| (x, y))).map(|(x, y)| {
            let o = y * dp + x * 4;
            f32::from_le_bytes([db[o], db[o + 1], db[o + 2], db[o + 3]])
        }).collect();
        let scene_bytes = pixel_size(sd.format);
        let (sb, sp) = match scene_bytes {
            Some(n) => read_texture(dev, ctx, scene, &sd, n)?,
            None => (Vec::new(), 0),
        };
        let view = SceneView { bytes: &sb, pitch: sp, w: sd.width as usize, h: sd.height as usize, fmt: sd.format };
        Ok(analyze(&depths, dd.width as usize, dd.height as usize, &view))
    }
}

/// Writes the block to the log.
pub fn log_report(r: &Report) {
    log!("depthstats: ---- begin ----");
    for line in format_report(r) {
        log!("depthstats: {line}");
    }
    log!("depthstats: ---- end ----");
}

/// The light-adjust draw (called from `datapath::bind_current`, ctx = the game's context, scene colour in PS slot 0):
/// captures once when armed. Never unwinds.
pub fn on_light_adjust(ctx: Obj, dev: Obj) {
    if !ARMED.load(Relaxed) {
        return;
    }
    let _ = std::panic::catch_unwind(|| unsafe { on_light_adjust_inner(ctx, dev) });
}

unsafe fn on_light_adjust_inner(ctx: Obj, dev: Obj) {
    let _ig = IgnoreScope::new();
    let Some(depth) = ao::depth_resource() else {
        // The depth of this frame is not (yet) known: try again at the next draw, give up after a long while.
        if TRIES.fetch_add(1, Relaxed) == 3000 {
            ARMED.store(false, Relaxed);
            ao::set_depth_consumer(DEPTH_FOR_STATS, false);
            log!("depthstats: gave up, no linear depth was available at the light-adjust draw");
        }
        return;
    };
    ARMED.store(false, Relaxed);
    ao::set_depth_consumer(DEPTH_FOR_STATS, false);
    unsafe {
        let mut scene_srv: Obj = std::ptr::null_mut();
        let get: unsafe extern "system" fn(Obj, u32, u32, *mut Obj) = std::mem::transmute(vfn(ctx, vt::CTX_PS_GET_SRVS));
        get(ctx, 0, 1, &mut scene_srv);
        if scene_srv.is_null() {
            log!("depthstats: no scene colour is bound in slot 0");
            return;
        }
        let Some((scene, _)) = ao::texture_of(scene_srv) else {
            release(scene_srv);
            log!("depthstats: the scene colour in slot 0 is not a 2D texture");
            return;
        };
        let r = capture(dev, ctx, depth, scene);
        release(scene);
        release(scene_srv);
        match r {
            Ok(rep) => {
                log_report(&rep);
                DONE_COUNT.fetch_add(1, Relaxed);
            }
            Err(e) => log!("depthstats: capture failed: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::c_void;
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::*;
    use windows::Win32::Graphics::Direct3D11::*;
    use windows::Win32::Graphics::Dxgi::Common::*;
    use windows::core::Interface;

    #[test]
    fn percentiles_interpolate_and_handle_edges() {
        let v: Vec<f32> = (0..=100).map(|i| i as f32).collect();
        assert_eq!(percentile(&v, 0.0), 0.0);
        assert_eq!(percentile(&v, 50.0), 50.0);
        assert_eq!(percentile(&v, 99.0), 99.0);
        assert_eq!(percentile(&v, 100.0), 100.0);
        assert!((percentile(&[1.0, 2.0], 50.0) - 1.5).abs() < 1e-6);
        assert_eq!(percentile(&[7.0], 33.0), 7.0);
        assert!(percentile(&[], 50.0).is_nan());
        assert_eq!(percentile(&v, 250.0), 100.0);
    }

    #[test]
    fn histogram_bins_log2_and_clamps() {
        // log2 range 0..6 in 64 bins: depth 1 -> bin 0, 2 -> bin 10 or 11, 64 -> last bin, 0.5 -> clamped to bin 0.
        let h = histogram(&[1.0, 2.0, 64.0, 0.5, 0.0, f32::NAN, -3.0], 0.0, 6.0);
        assert_eq!(h.iter().sum::<u32>(), 4, "invalid values must not count");
        assert_eq!(h[0], 2);
        assert_eq!(h[63], 1);
        assert_eq!(h[10], 1);
        let line = hist_line(&h);
        assert_eq!(line.chars().count(), BINS);
        assert_eq!(line.chars().filter(|c| *c == '.').count(), BINS - 3);
        assert_eq!(line.chars().next(), Some('9'));
        assert_eq!(hist_line(&[0; BINS]), ".".repeat(BINS));
    }

    #[test]
    fn decode_pixel_formats() {
        let mut f = Vec::new();
        for v in [0.25f32, 1.5, -2.0, 0.5] {
            f.extend_from_slice(&v.to_le_bytes());
        }
        assert_eq!(decode_pixel(2, &f), Some([0.25, 1.5, -2.0, 0.5]));
        let mut h = Vec::new();
        for v in [0x3c00u16, 0x4000, 0xc000, 0x3800] {
            h.extend_from_slice(&v.to_le_bytes());
        }
        assert_eq!(decode_pixel(10, &h), Some([1.0, 2.0, -2.0, 0.5]));
        assert_eq!(decode_pixel(28, &[255, 0, 51, 255]), Some([1.0, 0.0, 0.2, 1.0]));
        assert_eq!(decode_pixel(87, &[51, 0, 255, 255]), Some([1.0, 0.0, 0.2, 1.0]), "BGRA swaps red and blue");
        assert_eq!(decode_pixel(24, &1023u32.to_le_bytes()).map(|p| p[0]), Some(1.0));
        assert_eq!(decode_pixel(999, &[0; 16]), None);
        assert_eq!(decode_pixel(2, &[0; 4]), None, "short slice");
        assert!(f16_to_f32(0x7c00).is_infinite() && f16_to_f32(0x7e00).is_nan());
    }

    /// 16 x 8 depth: top 3 rows sky, the rest geometry 10..40.
    fn world(sky: f32) -> (Vec<f32>, Vec<u8>) {
        let (w, h) = (16usize, 8usize);
        let mut d = Vec::new();
        let mut c = Vec::new();
        for y in 0..h {
            for x in 0..w {
                let is_sky = y < 3;
                d.push(if is_sky { sky } else { 10.0 + (x as f32) * 2.0 });
                let rgba: [f32; 4] = if is_sky { [0.9, 0.9, 1.0, 0.1] } else { [0.1, 0.2, 0.1, 0.9] };
                for v in rgba {
                    c.extend_from_slice(&v.to_le_bytes());
                }
            }
        }
        (d, c)
    }

    #[test]
    fn analyze_counts_classes_and_far_colours_for_a_finite_sky() {
        let (mut d, c) = world(5000.0);
        d[0] = 0.0;
        d[1] = f32::NAN;
        d[2] = f32::INFINITY;
        d[3] = 1e31;
        let s = SceneView { bytes: &c, pitch: 16 * 16, w: 16, h: 8, fmt: 2 };
        let r = analyze(&d, 16, 8, &s);
        assert_eq!((r.le0, r.nonfinite, r.huge, r.finite, r.total), (1, 2, 1, 124, 128));
        assert_eq!((r.min, r.max), (10.0, 5000.0));
        assert_eq!(r.far99, 44, "the sky pixels with a finite depth");
        assert_eq!(r.far90, r.far99);
        assert_eq!(r.far99_avg.n, 44);
        assert!((r.far99_avg.rgba[0] - 0.9).abs() < 1e-5 && (r.far99_avg.rgba[3] - 0.1).abs() < 1e-5, "{:?}", r.far99_avg);
        assert_eq!(r.invalid_avg.n, 4);
        assert_eq!(r.hist.iter().sum::<u32>() as usize, r.finite);
        assert_eq!(r.grid.len(), 45);
        // Grid sample (4, 0) is the middle top: sky.
        assert_eq!(r.grid[4].0, 5000.0);
        assert!((r.grid[4].1.unwrap()[2] - 1.0).abs() < 1e-6);
        assert!((r.pct[3] - r.pct[3].clamp(r.min, r.max)).abs() < 1e-6);
        assert!(r.pct.windows(2).all(|w| w[0] <= w[1]));
    }

    #[test]
    fn report_text_has_every_item() {
        let (d, c) = world(5000.0);
        let s = SceneView { bytes: &c, pitch: 16 * 16, w: 16, h: 8, fmt: 2 };
        let lines = format_report(&analyze(&d, 16, 8, &s));
        let text = lines.join("\n");
        assert_eq!(lines.len(), 8 + GRID.1, "{text}");
        for key in ["depth 16x8, scene 16x8 format 2", "depth<=0 0", "nan/inf 0", ">1e30 0", "min 10.000 max 5000.000", "p1=", "p50=", "p99=", "hist log2(depth)", ">= 0.99*max", ">= 0.9*max", "far (>= 0.99*max) scene colour: n 48", "alpha 0.100", "grid row 0:", "grid row 4:"] {
            assert!(text.contains(key), "missing '{key}' in:\n{text}");
        }
        assert!(lines.iter().all(|l| !l.contains('\n')));
        // A texture format that cannot be decoded still gives the depth part.
        let s2 = SceneView { bytes: &[], pitch: 0, w: 16, h: 8, fmt: 999 };
        let t2 = format_report(&analyze(&d, 16, 8, &s2)).join("\n");
        assert!(t2.contains("not decodable") && t2.contains("far (>= 0.99*max) scene colour: none"), "{t2}");
        // All depth invalid: no panic, "nan" min / max.
        let z = vec![0.0f32; 128];
        let t3 = format_report(&analyze(&z, 16, 8, &s)).join("\n");
        assert!(t3.contains("depth<=0 128") && t3.contains("min nan max nan"), "{t3}");
    }

    #[test]
    fn number_format_is_compact() {
        assert_eq!(fmt_num(0.0), "0");
        assert_eq!(fmt_num(12.3456), "12.346");
        assert_eq!(fmt_num(f32::NAN), "nan");
        assert_eq!(fmt_num(f32::NEG_INFINITY), "-inf");
        assert!(fmt_num(2.5e9).contains('e') && fmt_num(1e-5).contains('e'));
    }

    #[test]
    fn arm_and_status_text() {
        // Only this test touches ARMED / DONE_COUNT among the host tests; it must not leave the consumer bit on.
        arm();
        assert!(armed() && ao::depth_consumers() & DEPTH_FOR_STATS != 0);
        assert!(status_text().starts_with("armed"));
        ARMED.store(false, Relaxed);
        ao::set_depth_consumer(DEPTH_FOR_STATS, false);
        assert!(!status_text().starts_with("armed"));
    }

    fn gpu() -> Option<(ID3D11Device, ID3D11DeviceContext)> {
        let (mut dev, mut ctx) = (None, None);
        let r = unsafe { D3D11CreateDevice(None::<&windows::Win32::Graphics::Dxgi::IDXGIAdapter>, D3D_DRIVER_TYPE_WARP, HMODULE::default(), D3D11_CREATE_DEVICE_FLAG(0), None, D3D11_SDK_VERSION, Some(&mut dev), None, Some(&mut ctx)) };
        match (r, dev, ctx) {
            (Ok(()), Some(d), Some(c)) => Some((d, c)),
            _ => {
                println!("skipped: WARP device creation failed");
                None
            }
        }
    }

    fn make(dev: &ID3D11Device, w: usize, h: usize, fmt: DXGI_FORMAT, data: &[u8], pitch: usize) -> ID3D11Texture2D {
        let desc = D3D11_TEXTURE2D_DESC { Width: w as u32, Height: h as u32, MipLevels: 1, ArraySize: 1, Format: fmt, SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, Usage: D3D11_USAGE_DEFAULT, BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32, CPUAccessFlags: 0, MiscFlags: 0 };
        let sd = D3D11_SUBRESOURCE_DATA { pSysMem: data.as_ptr() as *const c_void, SysMemPitch: pitch as u32, SysMemSlicePitch: 0 };
        let mut t = None;
        unsafe { dev.CreateTexture2D(&desc, Some(&sd), Some(&mut t)).unwrap() };
        t.unwrap()
    }

    #[test]
    fn capture_reads_depth_and_scene_back_on_warp() {
        let Some((dev, ctx)) = gpu() else { return };
        // A 40 x 24 world (the row pitch of the staging texture is not the tight width) with a finite-depth sky.
        let (w, h) = (40usize, 24usize);
        let mut depth = Vec::new();
        let mut scene = Vec::new();
        for y in 0..h {
            for x in 0..w {
                let sky = y < 8;
                depth.extend_from_slice(&(if sky { 4000.0f32 } else { 20.0 + x as f32 }).to_le_bytes());
                let px: [u8; 4] = if sky { [200, 220, 255, 30] } else { [20, 40, 20, 220] };
                scene.extend_from_slice(&px);
            }
        }
        let dt = make(&dev, w, h, DXGI_FORMAT_R32_FLOAT, &depth, w * 4);
        let st = make(&dev, w, h, DXGI_FORMAT_R8G8B8A8_UNORM, &scene, w * 4);
        let r = unsafe { capture(dev.as_raw(), ctx.as_raw(), dt.as_raw(), st.as_raw()) }.expect("capture");
        println!("{}", format_report(&r).join("\n"));
        assert_eq!((r.w, r.h, r.finite, r.max), (w, h, w * h, 4000.0));
        assert_eq!(r.far99, w * 8);
        assert!((r.far99_avg.rgba[2] - 1.0).abs() < 0.01 && (r.far99_avg.rgba[3] - 30.0 / 255.0).abs() < 0.01, "{:?}", r.far99_avg);
        assert_eq!(r.scene_fmt, 28);
        assert!(!crate::d3d::ignoring(), "ignore flag left on");
        // A depth texture of another format is refused with a message.
        let bad = make(&dev, 4, 4, DXGI_FORMAT_R8G8B8A8_UNORM, &[0u8; 64], 16);
        let e = unsafe { capture(dev.as_raw(), ctx.as_raw(), bad.as_raw(), st.as_raw()) }.unwrap_err();
        assert!(e.contains("expected R32_FLOAT"), "{e}");
    }
}
