//! Rust reference of the tone-map curve used by `shaders/resolve.hlsl` (the HLSL mirrors it).
//!
//! Colour space: the engine chain is gamma-encoded (about 2.0) and the display reads it as sRGB, so
//! values are decoded with the sRGB EOTF extended above 1.0 (the power segment simply continues),
//! tone-mapped per channel in linear light and encoded again. The output is display-referred and
//! gamma-encoded; its linear-light ceiling is the headroom `h = peak / paper_white`.

/// `shoulder_start` is a FRACTION of the headroom `h` (default 0.8): below `max(shoulder_start * h, 1)`
/// (linear light, see `knee`) the curve is the identity. `peak / paper_white` is the headroom (SDR: equal).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ToneParams {
    pub shoulder_start: f32,
    pub peak: f32,
    pub paper_white: f32,
}

impl ToneParams {
    /// Linear-light knee of the shoulder (HDR only): `shoulder_start * h`, but never below 1.0 so
    /// that SDR-range whites 0..1 stay untouched in every HDR configuration.
    pub fn knee(&self) -> f32 {
        (self.shoulder_start.clamp(0.0, 1.0) * self.headroom()).max(1.0)
    }

    /// Output headroom in linear light, relative to SDR white.
    pub fn headroom(&self) -> f32 {
        if self.paper_white > 0.0 && self.peak >= self.paper_white { self.peak / self.paper_white } else { 1.0 }
    }
}

/// sRGB EOTF; above 1.0 the power segment continues. NaN and negatives give 0.
#[allow(dead_code)] // Rust reference of the HLSL, used by the tests
pub fn srgb_decode(v: f32) -> f32 {
    let v = if v > 0.0 { v } else { 0.0 };
    if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
}

/// Inverse of `srgb_decode`.
#[allow(dead_code)]
pub fn srgb_encode(v: f32) -> f32 {
    let v = if v > 0.0 { v } else { 0.0 };
    if v <= 0.003_130_8 { v * 12.92 } else { 1.055 * v.powf(1.0 / 2.4) - 0.055 }
}

/// Smooth bounded shoulder in linear light: identity up to `s`, then C1 roll-off towards `h`.
#[allow(dead_code)]
pub fn shoulder(x: f32, s: f32, h: f32) -> f32 {
    let x = if x > 0.0 { x } else { 0.0 };
    if x <= s {
        return x;
    }
    let r = h - s;
    if r <= 1e-6 {
        return x.min(h);
    }
    s + r * (1.0 - (-(x - s) / r).exp())
}

/// Start of the highlight expansion (linear light, fraction of SDR white). It was 0.5 with the full gain only at 1.0: the game's
/// own tone curve keeps almost every pixel below 0.5..0.8 (in-game capture at highlight_boost 4: p99 luminance 0.495, no pixel
/// above 1.0), so the slider changed nothing visible. Now 0.4 .. 0.9.
#[allow(dead_code)]
pub const EXPAND_START: f32 = 0.4;
/// Largest channel from which the full gain applies.
#[allow(dead_code)]
pub const EXPAND_FULL: f32 = 0.9;

/// Highlight expansion (HDR only), hue-preserving: the largest channel `m` is multiplied by a gain
/// that rises smoothly from 1 at `EXPAND_START` to `gain` at `EXPAND_FULL` and stays there, so lights
/// the 8-bit engine clipped at 1.0 come out `gain` times brighter. Monotonic and continuous.
/// Mirrors `expand` in `resolve.hlsl` and `light_adjust_hdr.hlsl`.
#[allow(dead_code)]
pub fn expand(rgb: [f32; 3], gain: f32) -> [f32; 3] {
    // The trigger is capped by the luminance (1.25x): saturated blue/cyan/red is not a light source.
    let y = 0.2126 * rgb[0] + 0.7152 * rgb[1] + 0.0722 * rgb[2];
    let m = rgb[0].max(rgb[1]).max(rgb[2]).min(1.25 * y);
    if m <= EXPAND_START || gain <= 1.0 {
        return rgb;
    }
    let t = ((m - EXPAND_START) / (EXPAND_FULL - EXPAND_START)).clamp(0.0, 1.0);
    let s = t * t * (3.0 - 2.0 * t);
    // Bounded: the largest channel never ends above max(itself, gain) (the gain is at most the display headroom), so an
    // input already above SDR white is not pushed past the headroom.
    let mx = rgb[0].max(rgb[1]).max(rgb[2]);
    let f = (1.0 + (gain - 1.0) * s).min((gain / mx).max(1.0));
    rgb.map(|c| c * f)
}

/// Scene-referred gamma-encoded colour in (SDR, h <= 1: hard clip to h; HDR, h > 1: soft shoulder), display-referred gamma-encoded colour out.
#[allow(dead_code)]
pub fn map(rgb: [f32; 3], p: &ToneParams) -> [f32; 3] {
    let h = p.headroom();
    if h <= 1.0 {
        // SDR: hard clip in the encoded domain, exactly what the original 8-bit chain did.
        return rgb.map(|c| if c > 0.0 { c.min(h) } else { 0.0 });
    }
    let s = p.knee();
    rgb.map(|c| srgb_encode(shoulder(srgb_decode(c), s, h)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sdr() -> ToneParams {
        ToneParams { shoulder_start: 0.8, peak: 1.0, paper_white: 1.0 }
    }

    fn hdr(h: f32) -> ToneParams {
        ToneParams { shoulder_start: 0.8, peak: h * 200.0, paper_white: 200.0 }
    }

    #[test]
    fn expand_is_identity_below_start_and_gain_at_white() {
        assert_eq!(expand([0.5, 0.2, 0.1], 3.0), [0.5, 0.2, 0.1]);
        let o = expand([1.0, 1.0, 1.0], 3.0);
        assert!((o[0] - 3.0).abs() < 1e-5);
        assert_eq!(expand([2.0, 2.0, 2.0], 1.0), [2.0, 2.0, 2.0]);
        // Bounded by max(input, gain): over-range input is never pushed past the gain (= at most the headroom).
        assert_eq!(expand([3.0, 3.0, 3.0], 2.0), [3.0, 3.0, 3.0]);
        let o = expand([1.5, 1.4, 1.2], 4.0);
        assert!((o[0] - 4.0).abs() < 1e-5 && o[0] > 1.5, "{o:?}");
        // The range starts at 0.4 now (was 0.5) and is full at 0.9 (was 1.0).
        assert!(expand([0.6, 0.6, 0.6], 3.0)[0] > 0.6 && expand([0.38, 0.38, 0.38], 3.0)[0] == 0.38);
        assert!((expand([0.9, 0.9, 0.9], 3.0)[0] - 2.7).abs() < 1e-5);
    }

    #[test]
    fn expand_is_monotonic_and_keeps_hue() {
        let mut prev = 0.0f32;
        for i in 0..=400 {
            let v = i as f32 * 0.01;
            let o = expand([v, v * 0.5, v * 0.25], 4.0);
            // (1e-5: on the headroom bound the result is v * (gain / v), which rounds by one ulp either way.)
            assert!(o[0] >= prev - 1e-5, "decreased at {v}");
            prev = o[0];
            if v > 0.0 {
                assert!((o[1] / o[0] - 0.5).abs() < 1e-4);
            }
        }
    }

    #[test]
    fn identity_below_shoulder() {
        for v in [0.0f32, 0.1, 0.5, 0.8, 0.9] {
            let o = map([v, v, v], &sdr());
            for c in o {
                assert!((c - v).abs() < 1e-4, "{v} -> {c}");
            }
        }
        assert!((map([0.5, 0.5, 0.5], &sdr())[0] - 0.5).abs() < 1e-4);
    }

    #[test]
    fn monotonic() {
        for p in [sdr(), hdr(4.0)] {
            let mut prev = -1.0f32;
            for i in 0..=2000 {
                let v = i as f32 * 0.01; // 0..20
                let o = map([v, v, v], &p)[0];
                assert!(o >= prev, "decreased at {v}: {prev} -> {o}");
                prev = o;
            }
        }
    }

    #[test]
    fn sdr_never_exceeds_one() {
        for i in 0..=2000 {
            let v = i as f32 * 0.01;
            for c in map([v, v * 0.5, v * 2.0], &sdr()) {
                assert!(c <= 1.0 + 1e-6, "{v} -> {c}");
            }
        }
        assert!(map([1e9, 1e9, 1e9], &sdr())[0] <= 1.0 + 1e-6);
    }

    #[test]
    fn hdr_never_exceeds_peak_ratio() {
        let p = hdr(4.0);
        assert_eq!(p.headroom(), 4.0);
        for i in 0..=2000 {
            let v = i as f32 * 0.01;
            for c in map([v, v * 0.5, v * 2.0], &p) {
                // Linear light is bounded by h, so the encoded value is bounded by encode(h) (< h).
                assert!(c <= srgb_encode(4.0) + 1e-5 && c <= 4.0, "{v} -> {c}");
            }
        }
        // A bright input actually uses the headroom (clearly above SDR white).
        assert!(map([10.0, 10.0, 10.0], &p)[0] > 1.5);
    }

    #[test]
    fn sdr_is_hard_clip() {
        let p = sdr();
        assert_eq!(map([0.5, 1.0, 3.0], &p), [0.5, 1.0, 1.0]);
        assert_eq!(map([0.999, 1.0001, 1e9], &p), [0.999, 1.0, 1.0]);
    }

    #[test]
    fn sdr_identity_up_to_one() {
        for i in 0..=1000 {
            let v = i as f32 / 1000.0;
            assert_eq!(map([v, v, v], &sdr()), [v, v, v], "{v}");
        }
    }

    #[test]
    fn hdr_identity_below_knee() {
        let p = hdr(4.0);
        // Knee at 0.8 * 4 = 3.2 linear: every encoded value whose linear light is below it is unchanged.
        for i in 0..=1000 {
            let v = i as f32 / 1000.0 * srgb_encode(3.19);
            let o = map([v, v, v], &p)[0];
            assert!((o - v).abs() < 1e-4, "{v} -> {o}");
        }
    }

    #[test]
    fn hdr_knee_never_below_one() {
        // h = 1.2: 0.8 * h = 0.96 would compress SDR whites; the knee is held at 1.0.
        let p = hdr(1.2);
        assert!((map([1.0, 1.0, 1.0], &p)[0] - 1.0).abs() < 1e-6);
        for i in 0..=1000 {
            let v = i as f32 / 1000.0;
            assert!((map([v, v, v], &p)[0] - v).abs() < 1e-5, "{v}");
        }
        // h = 4: unchanged, identity up to 0.8 * 4 = 3.2 linear.
        let v = srgb_encode(3.19);
        assert!((map([v, v, v], &hdr(4.0))[0] - v).abs() < 1e-4);
        assert_eq!(hdr(4.0).knee(), 3.2);
        assert_eq!(hdr(1.2).knee(), 1.0);
    }

    #[test]
    fn continuous_at_shoulder() {
        for p in [hdr(1.5), hdr(4.0)] {
            let knee = p.shoulder_start * p.headroom();
            let e = 1e-3;
            let below = shoulder(knee - e, knee, p.headroom());
            let at = shoulder(knee, knee, p.headroom());
            let above = shoulder(knee + e, knee, p.headroom());
            assert!((at - knee).abs() < 1e-6);
            assert!((above - at).abs() < 2.0 * e && (at - below).abs() < 2.0 * e);
            // C1: the slope on both sides is 1 at the knee.
            assert!(((above - at) / e - 1.0).abs() < 1e-2);
            assert!(((at - below) / e - 1.0).abs() < 1e-2);
        }
    }

    #[test]
    fn zero_stays_zero() {
        assert_eq!(map([0.0, 0.0, 0.0], &sdr()), [0.0, 0.0, 0.0]);
        assert_eq!(map([0.0, 0.0, 0.0], &hdr(4.0)), [0.0, 0.0, 0.0]);
    }

    #[test]
    fn srgb_roundtrip_extended() {
        for i in 0..=4000 {
            let v = i as f32 * 0.001;
            let r = srgb_encode(srgb_decode(v));
            assert!((r - v).abs() < 1e-4, "{v} -> {r}");
        }
    }

    #[test]
    fn degenerate_shoulder_clamps_to_headroom() {
        let p = ToneParams { shoulder_start: 1.0, peak: 1.0, paper_white: 1.0 };
        assert!(map([5.0, 5.0, 5.0], &p)[0] <= 1.0 + 1e-6);
        assert!((map([0.5, 0.5, 0.5], &p)[0] - 0.5).abs() < 1e-4);
    }

    #[test]
    fn nan_and_negative_input_give_finite_nonnegative_output() {
        let o = map([f32::NAN, -1.0, f32::INFINITY], &sdr());
        for c in o {
            assert!(c.is_finite() && c >= 0.0, "{o:?}");
        }
    }
}
