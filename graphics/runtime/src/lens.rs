//! Lens effects from HDR highlights: anamorphic-style horizontal streaks, a soft glare and (for very bright point sources only)
//! subtle ghosts. See `docs/lens.md`.
//!
//! This module is the settings and the host mirrors of the maths in `shaders/look.hlsl`; the passes run in `look.rs` at the pre-UI
//! copy (the same hook point as the AA), on mod-owned targets. `lens_strength` 0 runs nothing.

use std::sync::atomic::{AtomicU32, Ordering::Relaxed};

pub const STRENGTH_RANGE: (f32, f32) = (0.0, 1.0);
pub const THRESHOLD_RANGE: (f32, f32) = (1.2, 8.0);
pub const GHOST_RANGE: (f32, f32) = (0.0, 0.5);
pub const LENGTH_RANGE: (f32, f32) = (0.25, 2.0);
pub const DEFAULT_STRENGTH: f32 = 0.15;
pub const DEFAULT_THRESHOLD: f32 = 4.0;
pub const DEFAULT_GHOST: f32 = 0.05;
pub const DEFAULT_LENGTH: f32 = 0.6;
/// `lens_max_size`: 0 = no size test (every bright pixel is a source).
pub const MAX_SIZE_RANGE: (f32, f32) = (0.0, 0.2);
pub const COLOR_REJECT_RANGE: (f32, f32) = (0.0, 1.0);
pub const DEFAULT_MAX_SIZE: f32 = 0.04;
pub const DEFAULT_COLOR_REJECT: f32 = 0.5;

/// The most energy (largest linear channel, 1 = paper white) the whole lens effect adds to any pixel (asymptotically).
pub const MAX_ADD: f32 = 0.6;
/// Ghost sources must be this many times above `lens_threshold`.
pub const GHOST_THRESHOLD_MULT: f32 = 2.0;
/// Overall gain of the streak (before `lens_strength` and the bound).
pub const LENS_GAIN: f32 = 2.0;
/// Share of the isotropic glare added to the streak.
pub const GLARE_WEIGHT: f32 = 0.35;
/// The streak: half length at `lens_streak_length` 1, as a share of the picture width (the full width is twice this).
pub const STREAK_HALF_WIDTH: f32 = 1.0 / 6.0;
/// Sum of the step multipliers of the three streak passes (1 + 3 + 9) times the taps per side (3).
pub const STREAK_REACH_UNITS: f32 = 39.0;
pub const WARM_WHITE: [f32; 3] = [1.0, 0.92, 0.80];
/// How far the colour of the source is pulled to warm white.
pub const TINT_TO_WARM: f32 = 0.65;
/// A blue channel is never above this share of the largest of the other two.
pub const BLUE_CAP: f32 = 0.85;

// ---- which bright pixels are light sources (docs/lens.md, "Which pixels are sources") ---------------------------------------

/// A saturated cool source (blue, cyan, violet, green) needs `1 + COOL_GAIN * lens_color_reject` times the threshold.
pub const COOL_GAIN: f32 = 4.0;
/// Chroma `(max - min) / max` below the first value is "white" (no penalty), above the second fully saturated.
pub const CHROMA_RAMP: (f32, f32) = (0.25, 0.75);
/// A pixel counts towards the bright area of its neighbourhood when its largest channel is above this share of the threshold.
/// 0.25 (was 0.5): a glowing creature's body (about 1x paper white) must count as area, else only the hover highlight's thin
/// outline around it was bright enough, read as a small area, and the creature flared when the mouse passed over it.
pub const COVER_FRACTION: f32 = 0.25;
/// Side of the square neighbourhood whose bright area is measured, in units of `lens_max_size`.
pub const WINDOW_MULT: f32 = 4.0;
/// Bright area of the neighbourhood in units of `lens_max_size` squared: up to the first value the pixel is a full source,
/// from the second on it is not a source at all (smooth in between).
pub const AREA_RAMP: (f32, f32) = (1.0, 2.5);
/// Taps of each of the two (separable) box passes of the area measure.
pub const AREA_TAPS: u32 = 16;
/// The coverage is counted on a grid of `COVER_GRID` x `COVER_GRID` full-resolution pixels per 1/16 texel (every second pixel).
pub const COVER_GRID: u32 = 8;

static STRENGTH: AtomicU32 = AtomicU32::new(0x3e19_999a); // 0.15
static THRESHOLD: AtomicU32 = AtomicU32::new(0x4080_0000); // 4.0
static GHOST: AtomicU32 = AtomicU32::new(0x3d4c_cccd); // 0.05
static LENGTH: AtomicU32 = AtomicU32::new(0x3f19_999a); // 0.6
static MAX_SIZE: AtomicU32 = AtomicU32::new(0x3d23_d70a); // 0.04
static COLOR_REJECT: AtomicU32 = AtomicU32::new(0x3f00_0000); // 0.5

fn clamp_to(v: f32, r: (f32, f32), default: f32) -> f32 {
    if v.is_nan() { default } else { v.clamp(r.0, r.1) }
}

pub fn clamp_strength(v: f32) -> f32 {
    clamp_to(v, STRENGTH_RANGE, DEFAULT_STRENGTH)
}
pub fn clamp_threshold(v: f32) -> f32 {
    clamp_to(v, THRESHOLD_RANGE, DEFAULT_THRESHOLD)
}
pub fn clamp_ghost(v: f32) -> f32 {
    clamp_to(v, GHOST_RANGE, DEFAULT_GHOST)
}
pub fn clamp_length(v: f32) -> f32 {
    clamp_to(v, LENGTH_RANGE, DEFAULT_LENGTH)
}
pub fn clamp_max_size(v: f32) -> f32 {
    clamp_to(v, MAX_SIZE_RANGE, DEFAULT_MAX_SIZE)
}
pub fn clamp_color_reject(v: f32) -> f32 {
    clamp_to(v, COLOR_REJECT_RANGE, DEFAULT_COLOR_REJECT)
}

fn load(a: &AtomicU32) -> f32 {
    f32::from_bits(a.load(Relaxed))
}

pub fn strength() -> f32 {
    crate::toggles::eff(crate::toggles::Fx::Lens, load(&STRENGTH))
}
pub fn threshold() -> f32 {
    load(&THRESHOLD)
}
pub fn ghost() -> f32 {
    load(&GHOST)
}
pub fn streak_length() -> f32 {
    load(&LENGTH)
}
pub fn max_size() -> f32 {
    load(&MAX_SIZE)
}
pub fn color_reject() -> f32 {
    load(&COLOR_REJECT)
}

pub fn set_strength(v: f32) -> f32 {
    let v = clamp_strength(v);
    STRENGTH.store(v.to_bits(), Relaxed);
    v
}
pub fn set_threshold(v: f32) -> f32 {
    let v = clamp_threshold(v);
    THRESHOLD.store(v.to_bits(), Relaxed);
    v
}
pub fn set_ghost(v: f32) -> f32 {
    let v = clamp_ghost(v);
    GHOST.store(v.to_bits(), Relaxed);
    v
}
pub fn set_streak_length(v: f32) -> f32 {
    let v = clamp_length(v);
    LENGTH.store(v.to_bits(), Relaxed);
    v
}
pub fn set_max_size(v: f32) -> f32 {
    let v = clamp_max_size(v);
    MAX_SIZE.store(v.to_bits(), Relaxed);
    v
}
pub fn set_color_reject(v: f32) -> f32 {
    let v = clamp_color_reject(v);
    COLOR_REJECT.store(v.to_bits(), Relaxed);
    v
}

/// The lens passes run (before the HDR / frame conditions): the strength is above 0.
pub fn on() -> bool {
    strength() > 0.0
}

/// The settings of one run.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LensParams {
    pub strength: f32,
    pub threshold: f32,
    pub ghost: f32,
    pub length: f32,
    /// Gain the light-adjust shader applied to a source at SDR white (the highlight boost): the threshold is in units of the
    /// scene BEFORE that boost, so ordinary white things (which the boost lifts to `gain`) never reach it.
    pub gain: f32,
    /// Largest diameter of a source that gets the full effect, as a share of the picture height (0 = no size test).
    pub max_size: f32,
    /// How much brighter a saturated cool (blue, cyan, violet, green) source must be: 0 = colour ignored .. 1.
    pub color_reject: f32,
}

// ---- sRGB (the encoding of the scene at the copy pass; same as `final_scrgb.hlsl`) -----------------------------------------

pub fn srgb_decode(v: f32) -> f32 {
    let v = v.max(0.0);
    if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
}

pub fn srgb_encode(v: f32) -> f32 {
    let v = v.max(0.0);
    if v <= 0.003_130_8 { v * 12.92 } else { 1.055 * v.powf(1.0 / 2.4) - 0.055 }
}

// ---- mirrors of the shader maths --------------------------------------------------------------------------------------------

/// Absolute linear threshold of the streak source (largest channel, 1 = paper white).
pub fn abs_threshold(p: &LensParams) -> f32 {
    p.threshold * p.gain.max(1.0)
}

/// Absolute linear threshold of the ghost source.
pub fn abs_ghost_threshold(p: &LensParams) -> f32 {
    abs_threshold(p) * GHOST_THRESHOLD_MULT
}

/// Energy a pixel with linear largest channel `m` gives the streak source: only what is above the threshold.
pub fn excess(m: f32, thresh: f32) -> f32 {
    (m - thresh).max(0.0)
}

/// Karis weight of a sample for the firefly-safe downsample.
pub fn karis_weight(rgb: [f32; 3]) -> f32 {
    1.0 / (1.0 + 0.2126 * rgb[0] + 0.7152 * rgb[1] + 0.0722 * rgb[2])
}

/// Streak colour: the source colour pulled towards warm white, never dominated by blue, same energy (largest channel).
pub fn streak_tint(c: [f32; 3]) -> [f32; 3] {
    let m = c[0].max(c[1]).max(c[2]);
    if !(m > 0.0) {
        return [0.0; 3];
    }
    let mut n = [c[0] / m, c[1] / m, c[2] / m];
    for i in 0..3 {
        n[i] += (WARM_WHITE[i] - n[i]) * TINT_TO_WARM;
    }
    n[2] = n[2].min(BLUE_CAP * n[0].max(n[1]));
    let k = n[0].max(n[1]).max(n[2]);
    [n[0] / k * m, n[1] / k * m, n[2] / k * m]
}

/// The bound on the added energy: about the input for small values, never above `MAX_ADD` for the largest channel.
pub fn bound_add(a: [f32; 3]) -> [f32; 3] {
    let mx = a[0].max(a[1]).max(a[2]).max(0.0);
    let k = 1.0 / (1.0 + mx / MAX_ADD);
    [a[0] * k, a[1] * k, a[2] * k]
}

fn smoothstep(a: f32, b: f32, x: f32) -> f32 {
    let t = ((x - a) / (b - a)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// 0 for white, warm and unsaturated colours, up to 1 for a saturated cool colour (blue, cyan, violet, green). Warmth is red
/// above blue: fire, lamps, candles and the low sun are warm; spectral and magic glows are mostly cool. A white or bluish-white
/// light (the sun, some lamps) has a low chroma and no penalty.
pub fn cool_penalty(c: [f32; 3]) -> f32 {
    let mx = c[0].max(c[1]).max(c[2]);
    if !(mx > 0.0) {
        return 0.0;
    }
    let mn = c[0].min(c[1]).min(c[2]).max(0.0);
    let chroma = smoothstep(CHROMA_RAMP.0, CHROMA_RAMP.1, (mx - mn) / mx);
    let warm = (2.0 * (c[0] - c[2]) / mx).clamp(0.0, 1.0);
    chroma * (1.0 - warm)
}

/// Absolute threshold of one pixel of linear colour `c`: the plain threshold, raised for saturated cool colours.
pub fn source_threshold(p: &LensParams, c: [f32; 3]) -> f32 {
    abs_threshold(p) * (1.0 + COOL_GAIN * clamp_color_reject(p.color_reject) * cool_penalty(c))
}

/// Side, in full-resolution pixels, of the neighbourhood whose bright area decides whether a pixel is a point source.
pub fn window_px(max_size: f32, height: u32) -> f32 {
    WINDOW_MULT * clamp_max_size(max_size) * height as f32
}

/// Source weight of a pixel whose neighbourhood (a `window_px` square) has the share `fill` (0..1) of bright pixels: 1 for a
/// compact source up to `lens_max_size` across, 0 for big bright areas (a glowing body, a bright wall) and their edges.
pub fn area_weight(fill: f32) -> f32 {
    let area = fill.clamp(0.0, 1.0) * WINDOW_MULT * WINDOW_MULT;
    1.0 - smoothstep(AREA_RAMP.0, AREA_RAMP.1, area)
}

/// Step, in 1/16 texels, between the taps of the area passes (0 = the size test is off).
pub fn area_step(max_size: f32, height: u32) -> f32 {
    if clamp_max_size(max_size) <= 0.0 {
        return 0.0;
    }
    window_px(max_size, height) / 16.0 / AREA_TAPS as f32
}

/// Texel step of the first streak pass at 1/8 resolution: the three passes use 1x, 3x and 9x this, 3 taps each side, so the
/// total reach is `STREAK_REACH_UNITS` steps = half of `lens_streak_length` x width / 6.
pub fn streak_unit(width8: u32, length: f32) -> f32 {
    clamp_length(length) * STREAK_HALF_WIDTH * width8.max(1) as f32 / STREAK_REACH_UNITS
}

/// Texel step of the glare blur at 1/8 resolution (4 taps each side per pass).
pub fn glare_step(width8: u32) -> f32 {
    (width8.max(1) as f32 / 160.0).max(1.0)
}

/// What the lens passes are given besides the picture: see the `look.hlsl` header for the layout.
pub fn cb_lens(p: &LensParams, width8: u32) -> ([f32; 4], [f32; 4]) {
    let strength = clamp_strength(p.strength);
    let ghost = clamp_ghost(p.ghost);
    (
        [strength, ghost, abs_threshold(p), streak_unit(width8, p.length)],
        // glare step, ENCODED ghost threshold (sRGB, for the cheap early out), encoded streak threshold, gain
        [glare_step(width8), srgb_encode(abs_ghost_threshold(p)), srgb_encode(abs_threshold(p)), LENS_GAIN],
    )
}

/// The source-selection constants: (cool gain x colour reject, ENCODED coverage threshold, area tap step in 1/16 texels
/// (0 = no size test), area units per fill) and the 1/16 level size (w16, h16, 0, 0).
pub fn cb_sources(p: &LensParams, height: u32, l16: (u32, u32)) -> ([f32; 4], [f32; 4]) {
    (
        [COOL_GAIN * clamp_color_reject(p.color_reject), srgb_encode(abs_threshold(p) * COVER_FRACTION), area_step(p.max_size, height), WINDOW_MULT * WINDOW_MULT],
        [l16.0 as f32, l16.1 as f32, 0.0, 0.0],
    )
}

#[cfg(test)]
#[path = "lens_tests.rs"]
mod tests;
