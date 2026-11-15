//! Aerial perspective: a subtle distance haze on far scene pixels. See `docs/aerial.md`.
//!
//! Nothing is drawn here. The effect lives in the light-adjust substitute (`light_adjust_hdr.hlsl`, the data path):
//! it needs the linear depth (`t4`), the fog colour estimate (`t3`, `fog.rs`) and four numbers in `b10`, which
//! `contact.rs` binds together with the contact shadows (they share the constant buffer). This module only owns
//! the live settings, the rule in plain Rust (the reference the WARP tests compare the shader with) and the
//! status text.
//!
//! The haze of a pixel is `min(strength * (1 - 2^(-(d - 15 m) / aerial_distance)), 35 %)` (d = RADIAL distance) toward the
//! haze colour H (`fog.rs` texel 2: the horizon band of the sky, held while no sky is in view, a neutral fallback before
//! any sky was seen), scaled by the confidence of H, released where the pixel already has the colour H (the game's own
//! fog is never doubled), released on bluish pixels (water) and
//! divided by `1 + max(0, m - 1)` for a pixel whose largest channel `m` is above SDR white (lights, the sun and
//! emissive stay as they are). Sky (invalid depth, or the far plane: normalised depth >= 0.98) and everything nearer
//! than 15 m are never touched. The engine's depth is normalised (1.0 = far plane); `depth_unit` = metres per 1.0.

use std::sync::atomic::{AtomicU32, Ordering::Relaxed};

use crate::ao;

pub const STRENGTH_RANGE: (f32, f32) = (0.0, 1.0);
pub const DEFAULT_STRENGTH: f32 = 0.35;
/// Distance (metres beyond `NEAR_M`) at which the haze reaches half of its full amount.
pub const DISTANCE_RANGE: (f32, f32) = (50.0, 5000.0);
pub const DEFAULT_DISTANCE: f32 = 400.0;
/// Metres per 1.0 of the engine's linear depth (`depth_unit`). The engine's full-size R32_FLOAT "linear depth" is
/// NORMALISED, not in world units: the debug capture's `status_depth=` measured valid depths from about 0.0005 to 1 with
/// the sky / far plane at exactly 1.0. So `metres = depth * depth_unit`, and `depth_unit` is the distance of the far
/// plane. The default comes from real captures (docs/aerial.md "Depth unit"): a crouching charr (about 2 m
/// visible height) covering 23 % of the screen height at depth 0.0089 is 8.7 m away (tan half fov 0.5, the mod's
/// assumption), i.e. 980 m per unit; the ground plane of the same frame (1/depth linear in the screen row, horizon at
/// y = 0.29) with the camera about 3.8 m above the ground gives 1060 m per unit. Hence 1000 m.
/// The old meaning (metres per depth unit assuming inches, default 0.0254, range 0.001..2) left every pixel nearer
/// than 0.03 m: both effects did nothing. A value up to `LEGACY_UNIT_MAX` is such an old setting and is replaced by
/// the default (`clamp_unit`).
pub const UNIT_RANGE: (f32, f32) = (100.0, 20000.0);
pub const DEFAULT_UNIT: f32 = 1000.0;
/// `depth_unit` values up to this are the old inch-based key (0.001..2) and mean "unset": the default is used.
pub const LEGACY_UNIT_MAX: f32 = 2.0;
/// Normalised depth from which a pixel is the sky / far plane (the engine writes exactly 1.0 there): never hazed.
pub const SKY_DEPTH: f32 = 0.98;
/// Nothing nearer than this is ever touched (character, nearby NPCs and enemies).
pub const NEAR_M: f32 = 15.0;
/// The largest share of the haze colour that can replace the scene colour.
#[allow(dead_code)] // used by the tests
pub const MAX_BLEND: f32 = 0.35;

static STRENGTH: AtomicU32 = AtomicU32::new(0x3eb3_3333); // 0.35f32
static DISTANCE: AtomicU32 = AtomicU32::new(0x43c8_0000); // 400.0f32
static UNIT: AtomicU32 = AtomicU32::new(0x447a_0000); // 1000.0f32

pub fn clamp_strength(v: f32) -> f32 {
    if v.is_nan() { DEFAULT_STRENGTH } else { v.clamp(STRENGTH_RANGE.0, STRENGTH_RANGE.1) }
}

pub fn clamp_distance(v: f32) -> f32 {
    if v.is_nan() { DEFAULT_DISTANCE } else { v.clamp(DISTANCE_RANGE.0, DISTANCE_RANGE.1) }
}

/// Not finite, or an old inch-based value (<= `LEGACY_UNIT_MAX`, including 0 and negatives): the default. Otherwise clamped.
pub fn clamp_unit(v: f32) -> f32 {
    if !v.is_finite() || v <= LEGACY_UNIT_MAX { DEFAULT_UNIT } else { v.clamp(UNIT_RANGE.0, UNIT_RANGE.1) }
}

/// Sets the strength (live). Above 0 the linear depth capture and the fog estimate are kept running for it.
pub fn set_strength(v: f32) -> f32 {
    let v = clamp_strength(v);
    STRENGTH.store(v.to_bits(), Relaxed);
    ao::set_depth_consumer(ao::DEPTH_FOR_AERIAL, v > 0.0);
    v
}

pub fn strength() -> f32 {
    crate::toggles::eff(crate::toggles::Fx::Aerial, f32::from_bits(STRENGTH.load(Relaxed)))
}

pub fn set_distance(v: f32) -> f32 {
    let v = clamp_distance(v);
    DISTANCE.store(v.to_bits(), Relaxed);
    v
}

pub fn distance() -> f32 {
    f32::from_bits(DISTANCE.load(Relaxed))
}

pub fn set_unit(v: f32) -> f32 {
    let v = clamp_unit(v);
    UNIT.store(v.to_bits(), Relaxed);
    v
}

pub fn unit() -> f32 {
    f32::from_bits(UNIT.load(Relaxed))
}

/// Applies the config values at start.
pub fn init(strength: f32, distance: f32, unit: f32) {
    set_distance(distance);
    set_unit(unit);
    set_strength(strength);
    crate::log!("aerial: configured (strength {}, distance {} m, depth unit {} m)", self::strength(), self::distance(), self::unit());
}

/// The share of the haze colour that replaces a pixel (0..=MAX_BLEND): the Rust twin of `aerial()` in the shader.
/// `z` is the normalised linear depth (1.0 = far plane / sky), `unit` metres per 1.0, `fog_k` the confidence of the fog
/// colour (0 = unknown), `fog_amount` the game's own fog share of the pixel (0..1), `bluish` the water release (0..1),
/// `m` the largest linear channel.
/// Pixels at the screen centre (ray 1): see `blend_at`.
#[allow(dead_code)] // the reference of the shader rule, used by the tests
pub fn blend(strength: f32, distance_m: f32, unit: f32, z: f32, fog_k: f32, fog_amount: f32, bluish: f32, m: f32) -> f32 {
    blend_at(strength, distance_m, unit, z, 1.0, fog_k, fog_amount, bluish, m)
}

/// Radial distance over view depth for the pixel at `uv` (0..1, y down) of a picture `aspect` = width / height wide, with the
/// mod's view assumption tan(half vertical fov) = 0.5 (the AO and the contact shadows use the same): `sqrt(1 + x^2 + y^2)` of the
/// view ray scaled to z = 1. 1 at the centre, about 1.43 in a 16:9 corner.
#[allow(dead_code)] // the reference of the shader rule, used by the tests
pub fn ray_factor(u: f32, v: f32, aspect: f32) -> f32 {
    let rx = (u * 2.0 - 1.0) * 0.5 * aspect;
    let ry = (v * 2.0 - 1.0) * 0.5;
    (1.0 + rx * rx + ry * ry).sqrt()
}

/// `blend` for a pixel whose radial distance is `ray` times its view depth (`ray_factor`): the haze follows the TRUE distance,
/// so a far point at the edge of a tilted-down view is not under-hazed. The sky test stays on the view depth `z`.
#[allow(dead_code)] // the reference of the shader rule, used by the tests
#[allow(clippy::too_many_arguments)]
pub fn blend_at(strength: f32, distance_m: f32, unit: f32, z: f32, ray: f32, fog_k: f32, fog_amount: f32, bluish: f32, m: f32) -> f32 {
    if !(z > 1e-5 && z < SKY_DEPTH) || !(strength > 0.0) {
        return 0.0;
    }
    let d = (z * ray.max(1.0) * unit - NEAR_M).max(0.0);
    let h = 1.0 - (-d / distance_m.max(1.0)).exp2();
    let b = (strength * h).min(MAX_BLEND);
    let b = b * fog_k.clamp(0.0, 1.0) * (1.0 - fog_amount.clamp(0.0, 1.0)) * (1.0 - bluish.clamp(0.0, 1.0));
    b / (1.0 + (m - 1.0).max(0.0))
}

/// Overlay line / debug capture `status_aerial=`.
pub fn status_text() -> String {
    if strength() <= 0.0 {
        return "off (strength 0)".to_string();
    }
    let (_, md) = crate::shafts::last_depth_info();
    let state = crate::contact::aerial_state();
    format!("{state}, {}, strength {:.2}, half distance {:.0} m (radial) beyond {:.0} m, depth unit {} m per 1.0 of the normalised depth, largest depth of the frame {}", crate::fog::haze_text(crate::fog::haze_info()), strength(), distance(), NEAR_M, unit(), depth_text(md, unit()))
}

/// "0.214 = 214 m" for a normalised depth `z` with `unit` metres per 1.0; the sky / far plane and unknown depths say so.
pub fn depth_text(z: f32, unit: f32) -> String {
    if !(z.is_finite() && z > 0.0) {
        "n/a (measured by the sun shafts while they run)".to_string()
    } else if z >= SKY_DEPTH {
        format!("{z:.3} = {:.0} m (the sky / far plane, never hazed)", z * unit)
    } else {
        format!("{z:.3} = {:.0} m", z * unit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamps_and_defaults() {
        assert_eq!(clamp_strength(9.0), 1.0);
        assert_eq!(clamp_strength(-1.0), 0.0);
        assert_eq!(clamp_strength(f32::NAN), DEFAULT_STRENGTH);
        assert_eq!(clamp_distance(1.0), 50.0);
        assert_eq!(clamp_distance(1e9), 5000.0);
        assert_eq!(clamp_distance(f32::NAN), DEFAULT_DISTANCE);
        // Old inch-based values (0.001..2, the old default 0.0254) and garbage mean "unset": the default.
        for old in [0.0254f32, 0.001, 1.0, 2.0, 0.0, -5.0, f32::NAN, f32::INFINITY] {
            assert_eq!(clamp_unit(old), DEFAULT_UNIT, "{old}");
        }
        assert_eq!(clamp_unit(3.0), UNIT_RANGE.0);
        assert_eq!(clamp_unit(2500.0), 2500.0);
        assert_eq!(clamp_unit(1e9), UNIT_RANGE.1);
        assert_eq!(f32::from_bits(0x3eb3_3333), DEFAULT_STRENGTH);
        assert_eq!(f32::from_bits(0x43c8_0000), DEFAULT_DISTANCE);
        assert_eq!(f32::from_bits(0x447a_0000), DEFAULT_UNIT);
    }

    #[test]
    fn a_real_capture_gives_metres_at_the_default_unit() {
        // status_depth= of a real capture: ground in front of the charr 0.006, the charr 0.0089, far trees 0.143, sky 1.0.
        let m = |z: f32| z * DEFAULT_UNIT;
        assert!((5.0..8.0).contains(&m(0.006)) && (7.0..11.0).contains(&m(0.0089)) && (100.0..200.0).contains(&m(0.143)));
        // Near ground and the character are below 15 m: untouched; far trees get a little haze; the sky none.
        let at = |z: f32| blend(DEFAULT_STRENGTH, DEFAULT_DISTANCE, DEFAULT_UNIT, z, 1.0, 0.0, 0.0, 0.5);
        assert_eq!(at(0.0089), 0.0);
        assert!(at(0.143) > 0.02 && at(0.143) < 0.1, "{}", at(0.143));
        assert!(at(0.5) > at(0.143));
        assert_eq!(at(1.0), 0.0);
        assert_eq!(at(SKY_DEPTH), 0.0);
        assert!(depth_text(1.0, 1000.0).contains("1000 m") && depth_text(1.0, 1000.0).contains("sky"));
        assert_eq!(depth_text(0.214, 1000.0), "0.214 = 214 m");
        assert!(depth_text(0.0, 1000.0).starts_with("n/a"));
    }

    #[test]
    fn radial_distance_grows_toward_the_edges_and_never_shrinks_the_haze() {
        assert_eq!(ray_factor(0.5, 0.5, 16.0 / 9.0), 1.0);
        let corner = ray_factor(0.0, 0.0, 16.0 / 9.0);
        assert!((corner - (1.0f32 + 0.889 * 0.889 + 0.25).sqrt()).abs() < 1e-3, "{corner}");
        // Top edge of a tilted-down view (far trees): radial distance 1.118 x the view depth, so more haze than at the centre.
        let top = ray_factor(0.5, 0.0, 16.0 / 9.0);
        assert!((top - 1.25f32.sqrt()).abs() < 1e-5);
        let z = 0.19; // a no-sky capture: largest depth 0.193
        let centre = blend_at(1.0, 400.0, DEFAULT_UNIT, z, 1.0, 1.0, 0.0, 0.0, 0.5);
        let edge = blend_at(1.0, 400.0, DEFAULT_UNIT, z, top, 1.0, 0.0, 0.0, 0.5);
        assert!(edge > centre && centre > 0.2, "{centre} {edge}");
        // Proportional to the radial distance: the same radial metres give the same haze wherever on the screen.
        let same = blend_at(1.0, 400.0, DEFAULT_UNIT, z * top, 1.0, 1.0, 0.0, 0.0, 0.5);
        assert!((same - edge).abs() < 1e-6);
        // The sky test stays on the view depth: the far plane is never hazed, even in a corner.
        assert_eq!(blend_at(1.0, 400.0, DEFAULT_UNIT, 1.0, corner, 1.0, 0.0, 0.0, 0.5), 0.0);
        assert_eq!(blend(0.35, 400.0, DEFAULT_UNIT, 0.3, 1.0, 0.0, 0.0, 0.5), blend_at(0.35, 400.0, DEFAULT_UNIT, 0.3, 1.0, 1.0, 0.0, 0.0, 0.5));
    }

    #[test]
    fn strength_registers_the_depth_consumer() {
        let k = strength();
        set_strength(0.0);
        assert_eq!(ao::depth_consumers() & ao::DEPTH_FOR_AERIAL, 0);
        assert_eq!(set_strength(0.5), 0.5);
        assert_ne!(ao::depth_consumers() & ao::DEPTH_FOR_AERIAL, 0);
        set_strength(k);
        if k <= 0.0 {
            ao::set_depth_consumer(ao::DEPTH_FOR_AERIAL, false);
        }
    }

    #[test]
    fn blend_rule_near_far_cap_and_releases() {
        let u = DEFAULT_UNIT;
        let at = |metres: f32| blend(0.35, 400.0, u, metres / u, 1.0, 0.0, 0.0, 0.5);
        // Nothing within 15 m (and exactly at it); strictly growing beyond it; never above the cap.
        assert_eq!(at(1.0), 0.0);
        assert_eq!(at(15.0), 0.0);
        // (Depths are normalised now: 1.0 = the far plane = DEFAULT_UNIT metres; the old test used inches up to 1e6.)
        assert!(at(100.0) > 0.0 && at(500.0) > at(100.0) && at(950.0) > at(500.0));
        assert!(at(970.0) <= MAX_BLEND + 1e-6);
        // Half distance: the haze share is half of the strength cap at 415 m.
        assert!((at(15.0 + 400.0) - 0.175).abs() < 1e-4);
        // Even the strongest setting never goes over 35 %.
        assert!(blend(1.0, 50.0, u, 0.97, 1.0, 0.0, 0.0, 0.0) <= MAX_BLEND + 1e-6);
        // Releases: dense fog, water, unknown fog colour, invalid depth, the sky (1.0), strength 0.
        assert_eq!(blend(0.35, 400.0, u, 0.5, 1.0, 1.0, 0.0, 0.5), 0.0);
        assert_eq!(blend(0.35, 400.0, u, 0.5, 1.0, 0.0, 1.0, 0.5), 0.0);
        assert_eq!(blend(0.35, 400.0, u, 0.5, 0.0, 0.0, 0.0, 0.5), 0.0);
        assert_eq!(blend(0.35, 400.0, u, 0.0, 1.0, 0.0, 0.0, 0.5), 0.0);
        assert_eq!(blend(0.35, 400.0, u, 1.0, 1.0, 0.0, 0.0, 0.5), 0.0);
        assert_eq!(blend(0.0, 400.0, u, 0.5, 1.0, 0.0, 0.0, 0.5), 0.0);
        // Energy: a pixel at 3x SDR white gets 1/3 of the blend.
        let lit = blend(0.35, 400.0, u, 0.5, 1.0, 0.0, 0.0, 1.0);
        let hot = blend(0.35, 400.0, u, 0.5, 1.0, 0.0, 0.0, 3.0);
        assert!((hot - lit / 3.0).abs() < 1e-6);
        assert!(blend(0.35, 400.0, u, f32::NAN, 1.0, 0.0, 0.0, 0.5) == 0.0);
    }
}
