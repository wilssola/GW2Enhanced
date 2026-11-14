//! Colour grading of the 3D scene: sliders, presets and an optional 3D LUT (`.cube`). See `docs/grading.md`.
//!
//! This module is the settings, the maths (host mirrors of `shaders/look.hlsl`) and the `.cube` loader; the D3D side
//! (the composite pass at the pre-UI copy) is `look.rs`. Everything works on LINEAR scene light (1 = paper white, values above
//! 1 are legal highlights) before the UI is drawn. All sliders neutral and no LUT = the feature does not run at all
//! (bit-identical).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};
use std::sync::{Arc, Mutex, OnceLock};

use crate::log;

// ---- sliders ------------------------------------------------------------------------------------------------------------

pub const SATURATION_RANGE: (f32, f32) = (0.0, 2.0);
pub const CONTRAST_RANGE: (f32, f32) = (0.5, 1.5);
pub const TEMPERATURE_RANGE: (f32, f32) = (-1.0, 1.0);
pub const TINT_RANGE: (f32, f32) = (-1.0, 1.0);
pub const VIBRANCE_RANGE: (f32, f32) = (0.0, 1.0);
pub const SHADOWS_RANGE: (f32, f32) = (-0.5, 0.5);
pub const HIGHLIGHTS_RANGE: (f32, f32) = (-0.5, 0.5);
pub const LUT_STRENGTH_RANGE: (f32, f32) = (0.0, 1.0);
pub const DEFAULT_LUT_STRENGTH: f32 = 1.0;

/// Mid-grey the contrast pivots around (linear).
pub const CONTRAST_PIVOT: f32 = 0.18;
/// Above this luminance the contrast curve continues linearly (no power law on HDR values).
pub const CONTRAST_KNEE: f32 = 1.0;
/// Largest relative change of the shadows / highlights sliders at +-0.5 is 0.5 * this (30 %).
pub const SH_AMOUNT: f32 = 0.6;
pub const VIBRANCE_AMOUNT: f32 = 0.75;
/// Half-range of the temperature gain of the red and blue channel at temperature +-1.
pub const TEMP_GAIN: f32 = 0.20;
pub const TINT_GAIN: f32 = 0.15;

pub const LUMA: [f32; 3] = [0.2126, 0.7152, 0.0722];

/// The slider values. `Default` = neutral.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Grade {
    pub saturation: f32,
    pub contrast: f32,
    pub temperature: f32,
    pub tint: f32,
    pub vibrance: f32,
    pub shadows: f32,
    pub highlights: f32,
}

impl Default for Grade {
    fn default() -> Self {
        NEUTRAL
    }
}

pub const NEUTRAL: Grade = Grade { saturation: 1.0, contrast: 1.0, temperature: 0.0, tint: 0.0, vibrance: 0.0, shadows: 0.0, highlights: 0.0 };

fn clamp_to(v: f32, r: (f32, f32), default: f32) -> f32 {
    if v.is_nan() { default } else { v.clamp(r.0, r.1) }
}

impl Grade {
    pub fn clamped(self) -> Grade {
        Grade {
            saturation: clamp_to(self.saturation, SATURATION_RANGE, 1.0),
            contrast: clamp_to(self.contrast, CONTRAST_RANGE, 1.0),
            temperature: clamp_to(self.temperature, TEMPERATURE_RANGE, 0.0),
            tint: clamp_to(self.tint, TINT_RANGE, 0.0),
            vibrance: clamp_to(self.vibrance, VIBRANCE_RANGE, 0.0),
            shadows: clamp_to(self.shadows, SHADOWS_RANGE, 0.0),
            highlights: clamp_to(self.highlights, HIGHLIGHTS_RANGE, 0.0),
        }
    }

    /// All sliders at their neutral value: the grading pass does not run.
    pub fn is_neutral(&self) -> bool {
        *self == NEUTRAL
    }
}

pub fn clamp_lut_strength(v: f32) -> f32 {
    clamp_to(v, LUT_STRENGTH_RANGE, DEFAULT_LUT_STRENGTH)
}

// ---- presets -----------------------------------------------------------------------------------------------------------

pub struct Preset {
    pub name: &'static str,
    pub grade: Grade,
    pub what: &'static str,
}

/// The built-in presets (documented in `docs/grading.md`; pinned by a numeric test).
pub const PRESETS: [Preset; 5] = [
    Preset { name: "Original", grade: NEUTRAL, what: "everything neutral: the picture as the game / the rest of the mod makes it" },
    Preset { name: "Warm cinematic", grade: Grade { saturation: 0.92, contrast: 1.12, temperature: 0.30, tint: 0.05, vibrance: 0.10, shadows: -0.10, highlights: -0.08 }, what: "warm light, slightly deeper shadows, a little more contrast, calmer colours" },
    Preset { name: "Cool", grade: Grade { saturation: 0.95, contrast: 1.05, temperature: -0.25, tint: 0.0, vibrance: 0.0, shadows: 0.0, highlights: 0.05 }, what: "cooler white balance, a touch more contrast, slightly brighter highlights" },
    Preset { name: "Vivid", grade: Grade { saturation: 1.25, contrast: 1.10, temperature: 0.05, tint: 0.0, vibrance: 0.35, shadows: 0.0, highlights: 0.0 }, what: "richer colours (the dull ones most), a little more contrast" },
    Preset { name: "Soft", grade: Grade { saturation: 0.90, contrast: 0.88, temperature: 0.05, tint: 0.0, vibrance: 0.0, shadows: 0.15, highlights: -0.15 }, what: "low contrast, lifted shadows, calmer highlights, slightly muted" },
];

fn norm_name(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_ascii_lowercase()
}

/// A preset by name; case, spaces, `_` and `-` do not matter (`warm_cinematic`, `Warm cinematic`).
pub fn preset_by_name(name: &str) -> Option<&'static Preset> {
    let n = norm_name(name);
    if n.is_empty() {
        return None;
    }
    PRESETS.iter().find(|p| norm_name(p.name) == n)
}

// ---- live settings ------------------------------------------------------------------------------------------------------

static GRADE: Mutex<Grade> = Mutex::new(NEUTRAL);
/// The sliders are not all neutral (read by the hook for every PSSetShader: one atomic load).
static BASIC_ON: AtomicBool = AtomicBool::new(false);
static LUT_STRENGTH: AtomicU32 = AtomicU32::new(0x3f80_0000); // 1.0

/// Sets (clamped) and returns the sliders.
pub fn set(g: Grade) -> Grade {
    let g = g.clamped();
    *GRADE.lock().unwrap_or_else(|e| e.into_inner()) = g;
    BASIC_ON.store(!g.is_neutral(), Relaxed);
    g
}

pub fn get() -> Grade {
    *GRADE.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn basic_on() -> bool {
    BASIC_ON.load(Relaxed) && crate::toggles::on(crate::toggles::Fx::Grading)
}

pub fn set_lut_strength(v: f32) -> f32 {
    let v = clamp_lut_strength(v);
    LUT_STRENGTH.store(v.to_bits(), Relaxed);
    v
}

pub fn lut_strength() -> f32 {
    crate::toggles::eff(crate::toggles::Fx::Grading, f32::from_bits(LUT_STRENGTH.load(Relaxed)))
}

/// Does anything of the grading want to run (sliders not neutral, or a LUT with a strength).
pub fn any_on() -> bool {
    basic_on() || (lut_strength() > 0.0 && lut_wanted())
}

// ---- the maths (mirrors of look.hlsl) ------------------------------------------------------------------------------------

/// What the shader gets: the temperature / tint gains (luminance preserving) and the other sliders.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GradeParams {
    pub gains: [f32; 3],
    pub saturation: f32,
    pub contrast: f32,
    pub vibrance: f32,
    pub shadows: f32,
    pub highlights: f32,
}

/// Per-channel gains of the white balance sliders, normalised so the luminance of a grey is unchanged. (0, 0) is exactly 1, 1, 1.
/// Warmer = more red and less blue; tint > 0 = magenta (less green).
pub fn temp_tint_gains(temperature: f32, tint: f32) -> [f32; 3] {
    let (t, n) = (clamp_to(temperature, TEMPERATURE_RANGE, 0.0), clamp_to(tint, TINT_RANGE, 0.0));
    if t == 0.0 && n == 0.0 {
        return [1.0; 3];
    }
    let g = [1.0 + TEMP_GAIN * t, 1.0 - TINT_GAIN * n, 1.0 - TEMP_GAIN * t];
    let l = LUMA[0] * g[0] + LUMA[1] * g[1] + LUMA[2] * g[2];
    [g[0] / l, g[1] / l, g[2] / l]
}

impl GradeParams {
    pub fn from_grade(g: &Grade) -> GradeParams {
        let g = g.clamped();
        GradeParams { gains: temp_tint_gains(g.temperature, g.tint), saturation: g.saturation, contrast: g.contrast, vibrance: g.vibrance, shadows: g.shadows, highlights: g.highlights }
    }
}

fn smoothstep(a: f32, b: f32, x: f32) -> f32 {
    let t = ((x - a) / (b - a)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

pub fn luma(c: [f32; 3]) -> f32 {
    LUMA[0] * c[0] + LUMA[1] * c[1] + LUMA[2] * c[2]
}

/// The factor the contrast slider multiplies a pixel of luminance `y` by: power law around mid-grey up to the knee, then the
/// tangent line (C1, monotonic, no blow-up on HDR values). 1 at contrast 1; `y` = pivot maps to itself.
pub fn contrast_scale(y: f32, c: f32) -> f32 {
    if (c - 1.0).abs() < 1e-6 || y <= 1e-6 {
        return 1.0;
    }
    let (p, k) = (CONTRAST_PIVOT, CONTRAST_KNEE);
    let fk = p * (k / p).powf(c);
    let f = if y <= k { p * (y / p).powf(c) } else { fk + c * fk / k * (y - k) };
    f / y
}

/// The whole slider grade on one linear pixel (mirrors `grade()` in `look.hlsl`).
pub fn grade_rgb(c: [f32; 3], p: &GradeParams) -> [f32; 3] {
    let mut c = [c[0] * p.gains[0], c[1] * p.gains[1], c[2] * p.gains[2]];
    let s = contrast_scale(luma(c), p.contrast);
    c = [c[0] * s, c[1] * s, c[2] * s];
    let y = luma(c);
    let w_s = 1.0 - smoothstep(0.02, 0.35, y);
    let w_h = smoothstep(0.3, 1.0, y);
    let f = (1.0 + p.shadows * SH_AMOUNT * w_s) * (1.0 + p.highlights * SH_AMOUNT * w_h);
    c = [c[0] * f, c[1] * f, c[2] * f];
    let y = luma(c);
    let sat = |c: [f32; 3], y: f32, k: f32| [(y + (c[0] - y) * k).max(0.0), (y + (c[1] - y) * k).max(0.0), (y + (c[2] - y) * k).max(0.0)];
    c = sat(c, y, p.saturation);
    let mx = c[0].max(c[1]).max(c[2]);
    let mn = c[0].min(c[1]).min(c[2]);
    let s = (mx - mn) / mx.max(1e-6);
    let y = luma(c);
    sat(c, y, 1.0 + p.vibrance * (1.0 - s) * VIBRANCE_AMOUNT)
}

// ---- log shaper of the LUT -----------------------------------------------------------------------------------------------

pub const SHAPER_MIN_LOG2: f32 = -8.0;
pub const SHAPER_MAX_LOG2: f32 = 6.0;
const SHAPER_RANGE: f32 = SHAPER_MAX_LOG2 - SHAPER_MIN_LOG2;

/// Linear light -> LUT axis 0..1: log2 between 2^-8 and 2^6 (below / above, the axis is clamped).
pub fn shaper_encode(lin: f32) -> f32 {
    let x = if lin.is_finite() { lin } else { 0.0 };
    ((x.max(2f32.powf(SHAPER_MIN_LOG2)).log2() - SHAPER_MIN_LOG2) / SHAPER_RANGE).clamp(0.0, 1.0)
}

/// LUT axis value -> linear light (the output decoding; also legal outside 0..1).
pub fn shaper_decode(y: f32) -> f32 {
    (y * SHAPER_RANGE + SHAPER_MIN_LOG2).exp2()
}

/// What the LUT does to one channel value given the looked-up output axis value `y`: values outside the shaper range keep their
/// ratio (so an identity LUT is the identity everywhere, black and super-white included).
pub fn lut_channel_out(lin: f32, y: f32) -> f32 {
    let (lo, hi) = (2f32.powf(SHAPER_MIN_LOG2), 2f32.powf(SHAPER_MAX_LOG2));
    let r = lin.clamp(lo, hi);
    shaper_decode(y) * (lin / r)
}

// ---- the .cube file -----------------------------------------------------------------------------------------------------

pub const LUT_SIZE_RANGE: (usize, usize) = (17, 65);
const MAX_CUBE_BYTES: u64 = 24 * 1024 * 1024;

/// A parsed 3D LUT: `size`^3 rows, red varies fastest (the `.cube` order, also the texture order).
#[derive(Clone, Debug, PartialEq)]
pub struct Cube {
    pub size: usize,
    pub data: Vec<[f32; 3]>,
}

impl Cube {
    /// The identity LUT in the shaper space of this mod (output axis = input axis).
    pub fn identity(size: usize) -> Cube {
        let n = size.max(2);
        let mut data = Vec::with_capacity(n * n * n);
        for b in 0..n {
            for g in 0..n {
                for r in 0..n {
                    data.push([r as f32 / (n - 1) as f32, g as f32 / (n - 1) as f32, b as f32 / (n - 1) as f32]);
                }
            }
        }
        Cube { size: n, data }
    }
}

fn three_floats(rest: &str) -> Option<[f32; 3]> {
    let mut it = rest.split_whitespace();
    let v = [it.next()?.parse::<f32>().ok()?, it.next()?.parse::<f32>().ok()?, it.next()?.parse::<f32>().ok()?];
    if it.next().is_some() { None } else { Some(v) }
}

/// Parses the text of an Adobe / Resolve `.cube` 3D LUT. Errors say what is wrong; never panics.
pub fn parse_cube(text: &str) -> Result<Cube, String> {
    let mut size: Option<usize> = None;
    let mut data: Vec<[f32; 3]> = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim().trim_start_matches('\u{feff}');
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let ln = i + 1;
        let first = line.split_whitespace().next().unwrap_or("");
        if first.chars().next().is_some_and(|c| c.is_ascii_digit() || c == '-' || c == '+' || c == '.') {
            let Some(n) = size else { return Err(format!("line {ln}: data before LUT_3D_SIZE")) };
            let Some(v) = three_floats(line) else { return Err(format!("line {ln}: expected three numbers")) };
            if !v.iter().all(|x| x.is_finite()) {
                return Err(format!("line {ln}: not a finite number"));
            }
            if data.len() >= n * n * n {
                return Err(format!("too many data rows (expected {})", n * n * n));
            }
            data.push(v);
            continue;
        }
        let (key, rest) = line.split_once(char::is_whitespace).map_or((line, ""), |(k, r)| (k, r.trim()));
        match key.to_ascii_uppercase().as_str() {
            "TITLE" => {}
            "LUT_3D_SIZE" => {
                let n: usize = rest.parse().map_err(|_| format!("line {ln}: bad LUT_3D_SIZE"))?;
                if n < LUT_SIZE_RANGE.0 || n > LUT_SIZE_RANGE.1 {
                    return Err(format!("LUT size {n} is not supported (use {} to {})", LUT_SIZE_RANGE.0, LUT_SIZE_RANGE.1));
                }
                if size.is_some() {
                    return Err(format!("line {ln}: LUT_3D_SIZE twice"));
                }
                size = Some(n);
            }
            "LUT_1D_SIZE" => return Err("1D LUTs are not supported (need LUT_3D_SIZE)".to_string()),
            "DOMAIN_MIN" => {
                if three_floats(rest) != Some([0.0; 3]) {
                    return Err("DOMAIN_MIN other than 0 0 0 is not supported".to_string());
                }
            }
            "DOMAIN_MAX" => {
                if three_floats(rest) != Some([1.0; 3]) {
                    return Err("DOMAIN_MAX other than 1 1 1 is not supported".to_string());
                }
            }
            _ => {} // unknown keywords (LUT_3D_INPUT_RANGE, comments of other tools) are ignored
        }
    }
    let Some(n) = size else { return Err("no LUT_3D_SIZE found".to_string()) };
    if data.len() != n * n * n {
        return Err(format!("truncated: {} of {} data rows", data.len(), n * n * n));
    }
    Ok(Cube { size: n, data })
}

/// Reads and parses a `.cube` file (size-limited).
pub fn load_cube_file(path: &Path) -> Result<Cube, String> {
    let meta = std::fs::metadata(path).map_err(|e| if e.kind() == std::io::ErrorKind::NotFound { "file not found".to_string() } else { format!("cannot read the file: {e}") })?;
    if !meta.is_file() {
        return Err("not a file".to_string());
    }
    if meta.len() > MAX_CUBE_BYTES {
        return Err("file too large (over 24 MB)".to_string());
    }
    let bytes = std::fs::read(path).map_err(|e| format!("cannot read the file: {e}"))?;
    parse_cube(&String::from_utf8_lossy(&bytes))
}

/// The configured `grade_lut` value as a path inside the `GW2Graphics` folder: relative, no `..`, no drive or root, `.cube` only.
pub fn resolve_lut_path(dir: &Path, rel: &str) -> Result<PathBuf, String> {
    let rel = rel.trim();
    if rel.is_empty() {
        return Err("empty path".to_string());
    }
    let p = Path::new(rel);
    if p.is_absolute() || rel.starts_with('/') || rel.starts_with('\\') || rel.contains(':') {
        return Err("the LUT must be inside the GW2Graphics folder (use a relative path)".to_string());
    }
    if p.components().any(|c| !matches!(c, std::path::Component::Normal(_) | std::path::Component::CurDir)) {
        return Err("the LUT path must not contain '..'".to_string());
    }
    if !p.extension().is_some_and(|e| e.eq_ignore_ascii_case("cube")) {
        return Err("the LUT file must be a .cube file".to_string());
    }
    Ok(dir.join(p))
}

/// f32 -> IEEE half (round to nearest even; overflow clamps to the largest finite half, NaN gives 0).
pub fn f32_to_f16(v: f32) -> u16 {
    if v.is_nan() {
        return 0;
    }
    let b = v.to_bits();
    let sign = ((b >> 16) & 0x8000) as u16;
    let a = v.abs();
    if a >= 65520.0 {
        return sign | 0x7bff;
    }
    if a < 6.103_515_6e-5 {
        // Subnormal half: a * 2^24, rounded to even.
        let m = (a * 16_777_216.0).round_ties_even() as u32;
        return sign | m as u16;
    }
    let e = ((b >> 23) & 0xff) as i32 - 127 + 15;
    let m = b & 0x7f_ffff;
    let mut h = ((e as u32) << 10) | (m >> 13);
    let rest = m & 0x1fff;
    if rest > 0x1000 || (rest == 0x1000 && h & 1 == 1) {
        h += 1; // may carry into the exponent, which is right
    }
    sign | h as u16
}

/// The RGBA16F texels of a cube for the 3D texture (alpha 1).
pub fn cube_texels(c: &Cube) -> Vec<[u16; 4]> {
    c.data.iter().map(|v| [f32_to_f16(v[0]), f32_to_f16(v[1]), f32_to_f16(v[2]), 0x3c00]).collect()
}

// ---- LUT loading (background thread) ---------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub enum LutState {
    /// `grade_lut` is empty.
    Off,
    Loading(String),
    Ready { name: String, size: usize },
    Failed(String),
}

struct Shared {
    generation: u64,
    state: LutState,
    cube: Option<Arc<Cube>>,
}

static LUT: Mutex<Shared> = Mutex::new(Shared { generation: 0, state: LutState::Off, cube: None });
static DIR: OnceLock<PathBuf> = OnceLock::new();

fn lut_lock() -> std::sync::MutexGuard<'static, Shared> {
    LUT.lock().unwrap_or_else(|e| e.into_inner())
}

/// A LUT is configured and (still loading or) ready: the strength alone decides whether a frame uses it.
pub fn lut_wanted() -> bool {
    matches!(lut_lock().state, LutState::Ready { .. })
}

/// The generation and the cube for the render thread (it uploads when the generation changed).
pub fn lut_for_gpu() -> (u64, Option<Arc<Cube>>) {
    let g = lut_lock();
    (g.generation, g.cube.clone())
}

pub fn lut_state() -> LutState {
    lut_lock().state.clone()
}

/// Read-only status line of the LUT (overlay, `status_grade=`).
pub fn lut_status_text() -> String {
    match lut_state() {
        LutState::Off => NO_LUT_TEXT.to_string(),
        LutState::Loading(n) => format!("loading {n}"),
        LutState::Ready { name, size } => format!("{name} ({size}x{size}x{size}) loaded"),
        LutState::Failed(why) => format!("LUT off: {why}"),
    }
}

/// The LUT status while `grade_lut` is empty: the strength slider alone does nothing (the symptom "LUT strength
/// changes nothing" was exactly this: no LUT was configured).
pub const NO_LUT_TEXT: &str = "no LUT loaded (grade_lut is empty): the LUT strength does nothing; pick a .cube from GW2Graphics\\luts";

/// Folder the LUT files are looked up in (set once at start).
pub fn set_dir(dir: &Path) {
    let _ = DIR.set(dir.to_path_buf());
}

/// The GW2Graphics folder, once known.
pub fn dir() -> Option<PathBuf> {
    DIR.get().cloned()
}

/// Sub-folder of the GW2Graphics folder the overlay lists LUTs from.
pub const LUT_FOLDER: &str = "luts";

/// The `.cube` files directly inside `<dir>/luts`, as `grade_lut` values (`luts/name.cube`), sorted, at most 200.
pub fn list_luts_in(dir: &Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(dir.join(LUT_FOLDER)) else { return Vec::new() };
    let mut v: Vec<String> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .filter(|n| Path::new(n).extension().is_some_and(|x| x.eq_ignore_ascii_case("cube")))
        .map(|n| format!("{LUT_FOLDER}/{n}"))
        .collect();
    v.sort_by_key(|s| s.to_ascii_lowercase());
    v.truncate(200);
    v
}

// ---- bundled example LUTs ------------------------------------------------------------------------------------------------

/// Size of the example LUTs.
pub const EXAMPLE_SIZE: usize = 17;

/// The example looks, applied to LINEAR light (rgb) at one lattice point. All keep the luminance (only the colour moves), so
/// none of them brightens or darkens the picture.
fn example_look(name: &str, c: [f32; 3]) -> [f32; 3] {
    let y = luma(c);
    let gains: [f32; 3] = match name {
        "warm" => [1.07, 1.0, 0.88],
        "cool" => [0.92, 1.0, 1.10],
        "teal-orange" => {
            // Split toning: shadows toward teal, highlights toward orange (log luminance -6 .. 0, i.e. 1.6 % .. 100 % white).
            let t = ((y.max(1e-6).log2() + 6.0) / 6.0).clamp(0.0, 1.0);
            let t = t * t * (3.0 - 2.0 * t);
            let teal = [0.90, 1.02, 1.08];
            let orange = [1.10, 1.0, 0.86];
            [teal[0] + (orange[0] - teal[0]) * t, teal[1] + (orange[1] - teal[1]) * t, teal[2] + (orange[2] - teal[2]) * t]
        }
        _ => [1.0, 1.0, 1.0],
    };
    let o = [c[0] * gains[0], c[1] * gains[1], c[2] * gains[2]];
    let yo = luma(o);
    if y > 0.0 && yo > 0.0 { o.map(|v| v * (y / yo)) } else { c }
}

/// The text of an example `.cube` in this mod's log2 shaper space (name: identity, warm, cool, teal-orange).
pub fn example_cube_text(name: &str) -> String {
    let n = EXAMPLE_SIZE;
    let mut s = format!("# GW2 Enhanced Graphics example LUT '{name}' (docs/grading.md). Log2 shaper axis: (log2(linear) + 8) / 14.\n# Keeps the luminance; only the colour moves.\nTITLE \"{name}\"\nLUT_3D_SIZE {n}\n");
    for b in 0..n {
        for g in 0..n {
            for r in 0..n {
                let ax = [r, g, b].map(|i| i as f32 / (n - 1) as f32);
                let lin = ax.map(shaper_decode);
                let o = example_look(name, lin);
                // Unclamped encode: an output a little above the axis range is legal (decoded as linear light above 2^6).
                let y = o.map(|v| (v.max(1e-12).log2() - SHAPER_MIN_LOG2) / SHAPER_RANGE);
                s.push_str(&format!("{:.6} {:.6} {:.6}\n", y[0], y[1], y[2]));
            }
        }
    }
    s
}

/// The example LUTs: (file name, text).
pub fn example_luts() -> Vec<(&'static str, String)> {
    ["identity", "warm", "cool", "teal-orange"].iter().map(|n| (match *n {
        "identity" => "identity.cube",
        "warm" => "warm.cube",
        "cool" => "cool.cube",
        _ => "teal-orange.cube",
    }, example_cube_text(n))).collect()
}

/// Writes the example LUTs into `<dir>/luts` (creates the folder; never overwrites an existing file). Returns how many were
/// written. Runs on the overlay thread when the player presses the button; the files are small (about 140 KB each).
pub fn write_example_luts_in(dir: &Path) -> Result<usize, String> {
    let folder = dir.join(LUT_FOLDER);
    std::fs::create_dir_all(&folder).map_err(|e| format!("cannot create {}: {e}", folder.display()))?;
    let mut n = 0;
    for (name, text) in example_luts() {
        let p = folder.join(name);
        if p.exists() {
            continue;
        }
        std::fs::write(&p, text).map_err(|e| format!("cannot write {}: {e}", p.display()))?;
        n += 1;
    }
    Ok(n)
}

/// Starts loading `rel` (a path inside the GW2Graphics folder) on a background thread; an empty value switches the LUT off. A bad
/// file leaves the LUT off and says why in the status, never anything worse.
pub fn set_lut(rel: &str) {
    let Some(dir) = DIR.get().cloned() else {
        let mut g = lut_lock();
        g.generation += 1;
        g.cube = None;
        g.state = if rel.trim().is_empty() { LutState::Off } else { LutState::Failed("the GW2Graphics folder is not known yet".to_string()) };
        return;
    };
    set_lut_in(&dir, rel);
}

/// `set_lut` with an explicit folder (the tests use a temp folder).
pub fn set_lut_in(dir: &Path, rel: &str) {
    let rel = rel.trim().to_string();
    let generation = {
        let mut g = lut_lock();
        g.generation += 1;
        g.cube = None;
        g.state = LutState::Off;
        if rel.is_empty() {
            return;
        }
        match resolve_lut_path(dir, &rel) {
            Err(why) => {
                g.state = LutState::Failed(why);
                return;
            }
            Ok(_) => g.state = LutState::Loading(rel.clone()),
        }
        g.generation
    };
    let path = match resolve_lut_path(dir, &rel) {
        Ok(p) => p,
        Err(_) => return,
    };
    let spawned = std::thread::Builder::new().name("gw2g-lut-load".into()).spawn(move || {
        let r = std::panic::catch_unwind(|| load_cube_file(&path)).unwrap_or_else(|_| Err("internal error while reading".to_string()));
        let mut g = lut_lock();
        if g.generation != generation {
            return; // a newer request replaced this one
        }
        match r {
            Ok(c) => {
                log!("grading: LUT {rel} loaded ({0}x{0}x{0})", c.size);
                g.state = LutState::Ready { name: rel, size: c.size };
                g.cube = Some(Arc::new(c));
            }
            Err(why) => {
                log!("grading: LUT {rel} not used: {why}");
                g.state = LutState::Failed(why);
            }
        }
    });
    if spawned.is_err() {
        let mut g = lut_lock();
        if g.generation == generation {
            g.state = LutState::Failed("could not start the loader thread".to_string());
        }
    }
}

/// Applies the config at start.
pub fn init(dir: &Path, g: Grade, lut: &str, lut_strength: f32) {
    set_dir(dir);
    set(g);
    set_lut_strength(lut_strength);
    set_lut(lut);
    log!("grading: configured ({:?}, LUT {:?} strength {})", get(), lut, self::lut_strength());
}

#[cfg(test)]
#[path = "grading_tests.rs"]
mod tests;
