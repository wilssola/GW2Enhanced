//! Host tests of the lens settings and the maths mirrored from `look.hlsl`.

use super::*;

fn params() -> LensParams {
    LensParams { strength: 0.35, threshold: 2.5, ghost: 0.15, length: 1.0, gain: 2.9, max_size: DEFAULT_MAX_SIZE, color_reject: DEFAULT_COLOR_REJECT }
}

#[test]
fn defaults_ranges_and_nan() {
    assert_eq!((DEFAULT_STRENGTH, DEFAULT_THRESHOLD, DEFAULT_GHOST, DEFAULT_LENGTH), (0.15, 4.0, 0.05, 0.6));
    assert_eq!((clamp_strength(-1.0), clamp_strength(9.0), clamp_strength(f32::NAN)), (0.0, 1.0, 0.15));
    assert_eq!((clamp_threshold(0.0), clamp_threshold(99.0), clamp_threshold(f32::NAN)), (1.2, 8.0, 4.0));
    assert_eq!((clamp_ghost(-1.0), clamp_ghost(9.0), clamp_ghost(f32::NAN)), (0.0, 0.5, 0.05));
    assert_eq!((clamp_length(0.0), clamp_length(9.0), clamp_length(f32::NAN)), (0.25, 2.0, 0.6));
    assert_eq!(f32::from_bits(0x3e19_999a), DEFAULT_STRENGTH);
    assert_eq!(f32::from_bits(0x4080_0000), DEFAULT_THRESHOLD);
    assert_eq!(f32::from_bits(0x3d4c_cccd), DEFAULT_GHOST);
}

#[test]
fn ordinary_bright_things_never_reach_the_threshold() {
    let p = params();
    // The highlight boost lifts SDR white (1.0) to `gain` in the picture the lens sees; fur, sand and the white UI top out there.
    let white_after_boost = 1.0 * p.gain;
    assert_eq!(excess(white_after_boost, abs_threshold(&p)), 0.0);
    assert_eq!(excess(white_after_boost * 1.5, abs_threshold(&p)), 0.0);
    // A light 4x above white (pre-boost) is a source; a very bright one (8x) is also a ghost source.
    assert!(excess(4.0 * p.gain, abs_threshold(&p)) > 0.0);
    assert_eq!(excess(4.0 * p.gain, abs_ghost_threshold(&p)), 0.0, "4x is bright, not 'very bright'");
    assert!(excess(8.0 * p.gain, abs_ghost_threshold(&p)) > 0.0);
    // Without the boost (gain 1) the same rule holds.
    let q = LensParams { gain: 1.0, ..p };
    assert_eq!(excess(1.0, abs_threshold(&q)), 0.0);
    assert!(abs_threshold(&LensParams { gain: 0.3, ..p }) >= p.threshold, "a gain below 1 never lowers the threshold");
}

#[test]
fn the_streak_colour_is_warm_white_never_blue_and_keeps_the_energy() {
    for src in [[0.1, 0.3, 1.0], [0.0, 0.0, 5.0], [1.0, 1.0, 1.0], [3.0, 1.0, 0.2], [0.2, 2.0, 0.2], [0.0, 1.0, 1.0], [1.0, 0.0, 1.0]] {
        let t = streak_tint(src);
        let (m_in, m_out) = (src.iter().cloned().fold(0.0, f32::max), t.iter().cloned().fold(0.0, f32::max));
        assert!((m_in - m_out).abs() <= 1e-5 * m_in, "{src:?} -> {t:?}: energy");
        assert!(t[2] <= BLUE_CAP * t[0].max(t[1]) + 1e-6, "{src:?} -> {t:?}: blue dominates");
        assert!(t.iter().all(|v| v.is_finite() && *v >= 0.0));
    }
    // A white source comes out warm (red above blue), a pure blue one is pulled most of the way to warm white.
    let w = streak_tint([5.0; 3]);
    assert!(w[0] > w[1] && w[1] > w[2]);
    let b = streak_tint([0.0, 0.0, 5.0]);
    assert!(b[0] > b[2] * 1.1 && b[0] > 1.0, "blue source {b:?}");
    assert_eq!(streak_tint([0.0; 3]), [0.0; 3]);
    assert_eq!(streak_tint([f32::NAN, 0.0, 0.0]), [0.0; 3]);
}

#[test]
fn the_added_energy_is_bounded_and_small_values_pass() {
    for v in [0.0, 0.001, 0.01, 0.1, 0.6, 1.0, 5.0, 100.0, 1e6] {
        let b = bound_add([v, v * 0.5, v * 0.2]);
        assert!(b[0] <= MAX_ADD && b[0] >= 0.0, "{v} -> {b:?}");
        assert!(b[0] <= v + 1e-9);
    }
    let s = bound_add([0.01, 0.0, 0.0]);
    assert!((s[0] - 0.01).abs() < 2e-4, "small values pass nearly unchanged: {s:?}");
    // Monotonic.
    let mut prev = 0.0;
    for i in 0..1000 {
        let o = bound_add([i as f32 * 0.05, 0.0, 0.0])[0];
        assert!(o >= prev);
        prev = o;
    }
    assert!(prev > 0.55, "the bound approaches {MAX_ADD}: {prev}");
}

#[test]
fn karis_weight_downweights_bright_samples() {
    assert_eq!(karis_weight([0.0; 3]), 1.0);
    assert!((karis_weight([1.0; 3]) - 0.5).abs() < 1e-6);
    assert!(karis_weight([100.0; 3]) < 0.01);
    // One hot pixel among three dark ones is pulled well below the plain average.
    let hot = 400.0;
    let ws = [karis_weight([hot; 3]), 1.0, 1.0, 1.0];
    let karis = hot * ws[0] / ws.iter().sum::<f32>();
    assert!(karis < hot / 4.0 / 20.0, "{karis}");
}

#[test]
fn geometry_follows_the_length_and_the_width() {
    // Total reach = unit x 39 = length x width8 / 6 texels at 1/8 resolution.
    for (w8, l) in [(430u32, 1.0f32), (430, 2.0), (215, 0.5)] {
        let reach = streak_unit(w8, l) * STREAK_REACH_UNITS;
        assert!((reach - l * w8 as f32 / 6.0).abs() < 1e-3, "{w8} {l}: {reach}");
    }
    assert!(streak_unit(430, 9.0) == streak_unit(430, 2.0), "the length is clamped");
    assert!(glare_step(100) == 1.0 && glare_step(430) > 2.0 && glare_step(0) == 1.0);
    // The full-resolution streak at the default length is a sixth of the width on each side (8 px per texel).
    assert!((streak_unit(430, 1.0) * STREAK_REACH_UNITS * 8.0 - 3440.0 / 6.0).abs() < 8.0);
}

#[test]
fn the_constants_for_the_shader() {
    let (a, b) = cb_lens(&params(), 430);
    assert_eq!(a[0], 0.35);
    assert_eq!(a[1], 0.15);
    assert!((a[2] - 7.25).abs() < 1e-5, "absolute threshold {}", a[2]);
    assert!(a[3] > 0.0);
    assert!(b[0] >= 1.0 && b[3] == LENS_GAIN);
    // The encoded thresholds are what the shader's early out compares the encoded picture with.
    assert!((srgb_decode(b[2]) - a[2]).abs() < 1e-3 * a[2]);
    assert!((srgb_decode(b[1]) - 2.0 * a[2]).abs() < 1e-3 * a[2]);
    // Out-of-range values are clamped on the way.
    let (c, _) = cb_lens(&LensParams { strength: 9.0, ghost: -1.0, ..params() }, 430);
    assert_eq!((c[0], c[1]), (1.0, 0.0));
}

#[test]
fn srgb_round_trips_and_matches_the_final_pass() {
    for v in [0.0, 0.001, 0.0031308, 0.04, 0.18, 0.5, 1.0, 2.9, 8.0, 100.0] {
        assert!((srgb_decode(srgb_encode(v)) - v).abs() <= 1e-4 * v.max(1e-2), "{v}");
    }
    assert!((srgb_encode(0.18) - 0.4614).abs() < 1e-3);
    assert_eq!((srgb_decode(-1.0), srgb_encode(-1.0)), (0.0, 0.0));
}

#[test]
fn settings_setters_clamp_and_report() {
    // Only this test writes the lens atomics among the host tests.
    let saved = (strength(), threshold(), ghost(), streak_length());
    assert_eq!(set_strength(5.0), 1.0);
    assert!(on());
    assert_eq!(set_strength(0.0), 0.0);
    assert!(!on(), "strength 0 = the lens is off");
    assert_eq!(set_threshold(0.1), 1.2);
    assert_eq!(set_ghost(0.2), 0.2);
    assert_eq!(set_streak_length(f32::NAN), 0.6);
    set_strength(saved.0);
    set_threshold(saved.1);
    set_ghost(saved.2);
    set_streak_length(saved.3);
}

#[test]
fn source_selection_defaults_ranges_and_setters() {
    assert_eq!((DEFAULT_MAX_SIZE, DEFAULT_COLOR_REJECT), (0.04, 0.5));
    assert_eq!((clamp_max_size(-1.0), clamp_max_size(9.0), clamp_max_size(f32::NAN)), (0.0, 0.2, 0.04));
    assert_eq!((clamp_color_reject(-1.0), clamp_color_reject(9.0), clamp_color_reject(f32::NAN)), (0.0, 1.0, 0.5));
    assert_eq!(f32::from_bits(0x3d23_d70a), DEFAULT_MAX_SIZE);
    assert_eq!(f32::from_bits(0x3f00_0000), DEFAULT_COLOR_REJECT);
    // Only this test writes these two atomics among the host tests.
    let saved = (max_size(), color_reject());
    assert_eq!(set_max_size(0.5), 0.2);
    assert_eq!(set_max_size(0.0), 0.0);
    assert_eq!(set_color_reject(f32::NAN), 0.5);
    assert_eq!(set_color_reject(2.0), 1.0);
    set_max_size(saved.0);
    set_color_reject(saved.1);
}

#[test]
fn white_and_warm_lights_have_no_colour_penalty_saturated_cool_glows_do() {
    // Real light sources: white, the sun (white to slightly cool white), warm lamps, candles, fire (orange to red), yellow.
    for (what, c) in [
        ("white", [1.0, 1.0, 1.0]),
        ("cool white sun", [0.85, 0.92, 1.0]),
        ("bluish white lamp", [0.8, 0.88, 1.0]),
        ("warm lamp", [1.0, 0.85, 0.6]),
        ("candle", [1.0, 0.7, 0.3]),
        ("fire", [1.0, 0.5, 0.1]),
        ("deep red fire", [1.0, 0.25, 0.05]),
        ("yellow", [1.0, 1.0, 0.2]),
    ] {
        assert!(cool_penalty(c) < 0.05, "{what} {c:?}: {}", cool_penalty(c));
    }
    // Spectral / magic glows: saturated blue, cyan, violet, green.
    for (what, c) in [("blue", [0.1, 0.3, 1.0]), ("cyan", [0.1, 1.0, 1.0]), ("violet", [0.6, 0.1, 1.0]), ("green", [0.2, 1.0, 0.3]), ("pure blue", [0.0, 0.0, 1.0])] {
        assert!(cool_penalty(c) > 0.9, "{what} {c:?}: {}", cool_penalty(c));
    }
    // A pale blue is in between, the scale does not matter, nonsense is no penalty.
    let pale = cool_penalty([0.5, 0.7, 1.0]);
    assert!(pale > 0.2 && pale < 0.8, "{pale}");
    assert!((cool_penalty([5.0, 15.0, 50.0]) - cool_penalty([0.1, 0.3, 1.0])).abs() < 1e-6);
    assert_eq!(cool_penalty([0.0; 3]), 0.0);
    assert!(cool_penalty([f32::NAN; 3]) == 0.0);
}

#[test]
fn the_colour_raises_the_threshold_only_for_cool_glows() {
    let p = params();
    let t = abs_threshold(&p);
    assert_eq!(source_threshold(&p, [1.0; 3]), t, "white keeps the plain threshold");
    assert_eq!(source_threshold(&p, [1.0, 0.5, 0.1]), t, "fire keeps the plain threshold");
    let blue = source_threshold(&p, [0.0, 0.0, 1.0]);
    assert!((blue - t * (1.0 + COOL_GAIN * 0.5)).abs() < 1e-4, "a saturated blue needs 3x at the default: {blue} vs {t}");
    // 0 = the colour is ignored (the old behaviour); 1 = 5x.
    assert_eq!(source_threshold(&LensParams { color_reject: 0.0, ..p }, [0.0, 0.0, 1.0]), t);
    assert!((source_threshold(&LensParams { color_reject: 1.0, ..p }, [0.0, 0.0, 1.0]) - 5.0 * t).abs() < 1e-4);
    // Never below the plain threshold.
    for c in [[0.3, 0.9, 0.2], [1.0, 0.0, 1.0], [0.2, 0.2, 0.2], [9.0, 0.1, 9.0]] {
        assert!(source_threshold(&p, c) >= t);
    }
}

#[test]
fn compact_sources_keep_their_weight_big_bright_areas_and_their_edges_lose_it() {
    // The fill of the window around a bright area of `px2` pixels.
    let fill = |px2: f32, max_px: f32| px2 / (WINDOW_MULT * max_px).powi(2);
    let d = 57.6; // the default max size at 1440 px
    // A lamp or torch flame as wide as the max size (disc, square) and a small candle: full weight.
    assert_eq!(area_weight(fill(std::f32::consts::FRAC_PI_4 * d * d, d)), 1.0);
    assert_eq!(area_weight(fill(d * d, d)), 1.0);
    assert_eq!(area_weight(fill(10.0 * 10.0, d)), 1.0);
    // Twice as wide: nothing. A glowing creature filling the window, its edge (half the window), a limb near the body: nothing.
    assert_eq!(area_weight(fill(std::f32::consts::FRAC_PI_4 * (2.0 * d).powi(2), d)), 0.0);
    assert_eq!(area_weight(1.0), 0.0);
    assert_eq!(area_weight(0.5), 0.0);
    assert_eq!(area_weight(0.2), 0.0);
    // In between it falls smoothly and never rises with the area.
    let mid = area_weight(fill(1.6 * d * d, d));
    assert!(mid > 0.2 && mid < 0.9, "{mid}");
    let mut prev = 1.0;
    for i in 0..=100 {
        let w = area_weight(i as f32 / 100.0);
        assert!(w <= prev && (0.0..=1.0).contains(&w));
        prev = w;
    }
    assert_eq!((area_weight(-1.0), area_weight(9.0)), (1.0, 0.0));
}

#[test]
fn the_neighbourhood_follows_the_picture_height_and_zero_turns_the_test_off() {
    assert!((window_px(0.04, 1440) - 230.4).abs() < 1e-3);
    assert!((window_px(0.04, 1080) - 172.8).abs() < 1e-3);
    // AREA_TAPS taps over the window, in 1/16 texels.
    assert!((area_step(0.04, 1440) * AREA_TAPS as f32 * 16.0 - 230.4).abs() < 1e-3);
    assert_eq!(area_step(0.0, 1440), 0.0, "lens_max_size 0 = no size test");
    assert_eq!(area_step(9.0, 1440), area_step(0.2, 1440), "clamped");
    let p = params();
    let (a, b) = cb_sources(&p, 1440, (215, 90));
    assert_eq!(a[0], COOL_GAIN * 0.5);
    assert!((srgb_decode(a[1]) - COVER_FRACTION * abs_threshold(&p)).abs() < 1e-3 * abs_threshold(&p), "encoded coverage threshold");
    assert_eq!((a[2], a[3]), (area_step(p.max_size, 1440), WINDOW_MULT * WINDOW_MULT));
    assert_eq!(b, [215.0, 90.0, 0.0, 0.0]);
    let (off, _) = cb_sources(&LensParams { max_size: 0.0, color_reject: 0.0, ..p }, 1440, (215, 90));
    assert_eq!((off[0], off[2]), (0.0, 0.0), "both tests off = the old behaviour");
}

#[test]
fn the_shader_mirrors_the_source_selection_constants() {
    let h = crate::look::HLSL;
    for (name, v) in [("CHROMA_LO", CHROMA_RAMP.0), ("CHROMA_HI", CHROMA_RAMP.1), ("AREA_LO", AREA_RAMP.0), ("AREA_HI", AREA_RAMP.1)] {
        assert!(h.contains(&format!("static const float {name} = {v:?};")), "{name} = {v:?} missing in look.hlsl");
    }
    assert!(h.contains(&format!("static const int AREA_TAPS = {AREA_TAPS};")));
    assert!(h.contains(&format!("static const int COVER_GRID = {COVER_GRID};")));
}
