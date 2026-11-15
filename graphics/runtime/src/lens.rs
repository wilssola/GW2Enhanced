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

static STRENGTH: AtomicU32 = AtomicU32::new(0x3e19_999a); // 0.15
static THRESHOLD: AtomicU32 = AtomicU32::new(0x4080_0000); // 4.0
static GHOST: AtomicU32 = AtomicU32::new(0x3d4c_cccd); // 0.05
static LENGTH: AtomicU32 = AtomicU32::new(0x3f19_999a); // 0.6

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

#[cfg(test)]
#[path = "lens_tests.rs"]
mod tests;
