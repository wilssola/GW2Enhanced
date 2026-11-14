//! HDR10 output support (EXPERIMENTAL, `hdr_format=hdr10`): the pure parts. See `docs/hdr10.md`.
//!
//! The swapchain is created ONCE, by the mod, when the game creates its own (`output::create_swapchain` / the device-and-swapchain
//! path): as `R10G10B10A2_UNORM` flip-discard in the colour space `RGB_FULL_G2084_NONE_P2020` instead of float16 scRGB. The final pass
//! (`final_scrgb.hlsl`, built with `HDR10_OUTPUT`) then encodes: BT.709 -> BT.2020, x 80 (scRGB unit -> nits), PQ (ST.2084), plus a
//! 1-LSB triangular dither at 10 bit. Everything before the final pass (scene, bloom, shoulder, peak clamp) is unchanged.
//!
//! This module holds what is decided WITHOUT touching DXGI: the PQ maths (host reference of the shader), the matrices, the decision
//! whether HDR10 may be used, the fallback after a failed attempt, the `ResizeBuffers` arguments and the crash guard (a marker file).

use std::path::{Path, PathBuf};

use crate::config::HdrFormat;

/// `DXGI_FORMAT_R10G10B10A2_UNORM`.
pub const FMT_R10G10B10A2_UNORM: u32 = 24;
/// `DXGI_FORMAT_R16G16B16A16_FLOAT`.
pub const FMT_FLOAT16: u32 = 10;
/// `DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709` (scRGB).
pub const CS_SCRGB: u32 = 1;
/// `DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020` (HDR10).
pub const CS_HDR10: u32 = 12;

/// The format of the HDR swapchain the mod creates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SwapFormat {
    Float16,
    Rgb10A2,
}

impl SwapFormat {
    pub fn dxgi(self) -> u32 {
        match self {
            SwapFormat::Float16 => FMT_FLOAT16,
            SwapFormat::Rgb10A2 => FMT_R10G10B10A2_UNORM,
        }
    }

    pub fn colour_space(self) -> u32 {
        match self {
            SwapFormat::Float16 => CS_SCRGB,
            SwapFormat::Rgb10A2 => CS_HDR10,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            SwapFormat::Float16 => "scRGB float16",
            SwapFormat::Rgb10A2 => "HDR10 R10G10B10A2 (PQ, BT.2020)",
        }
    }
}

// ---- PQ (SMPTE ST 2084) ---------------------------------------------------------------------------------------------------------

const M1: f64 = 2610.0 / 16384.0;
const M2: f64 = 2523.0 / 4096.0 * 128.0;
const C1: f64 = 3424.0 / 4096.0;
const C2: f64 = 2413.0 / 4096.0 * 32.0;
const C3: f64 = 2392.0 / 4096.0 * 32.0;

/// PQ inverse EOTF: absolute luminance in nits (0..10000, clamped) -> code value 0..1. Host reference of `pq_encode` in the shader.
pub fn pq_encode(nits: f32) -> f32 {
    let y = (nits.max(0.0) as f64 / 10000.0).min(1.0);
    let yp = y.powf(M1);
    (((C1 + C2 * yp) / (1.0 + C3 * yp)).powf(M2)) as f32
}

/// PQ EOTF: code value 0..1 -> nits.
pub fn pq_decode(code: f32) -> f32 {
    let e = (code.clamp(0.0, 1.0) as f64).powf(1.0 / M2);
    let num = (e - C1).max(0.0);
    let den = C2 - C3 * e;
    (10000.0 * (num / den).powf(1.0 / M1)) as f32
}

// ---- gamut -----------------------------------------------------------------------------------------------------------------------

/// Linear BT.709 -> linear BT.2020 (ITU-R BT.2087, rows sum to 1 so white stays white).
pub const BT709_TO_2020: [[f32; 3]; 3] = [[0.627_403_9, 0.329_283_0, 0.043_313_1], [0.069_097_3, 0.919_540_4, 0.011_362_3], [0.016_391_4, 0.088_013_3, 0.895_595_3]];
/// The inverse (used to decode HDR10 captures back to the scRGB primaries).
pub const BT2020_TO_709: [[f32; 3]; 3] = [[1.660_491, -0.587_641_1, -0.072_849_9], [-0.124_550_5, 1.132_899_9, -0.008_349_4], [-0.018_150_8, -0.100_578_9, 1.118_729_7]];

fn mul(m: &[[f32; 3]; 3], v: [f32; 3]) -> [f32; 3] {
    [0, 1, 2].map(|i| m[i][0] * v[0] + m[i][1] * v[1] + m[i][2] * v[2])
}

pub fn bt709_to_2020(v: [f32; 3]) -> [f32; 3] {
    mul(&BT709_TO_2020, v)
}

pub fn bt2020_to_709(v: [f32; 3]) -> [f32; 3] {
    mul(&BT2020_TO_709, v)
}

/// What the final shader writes for an scRGB-unit colour (linear BT.709, 1.0 = 80 nits), without the dither: PQ code values.
pub fn scrgb_to_pq(v: [f32; 3]) -> [f32; 3] {
    bt709_to_2020(v).map(|c| pq_encode(c.max(0.0) * 80.0))
}

#[allow(dead_code)] // host reference for the tests (and the capture decode uses a LUT of the same maths)
/// Inverse of `scrgb_to_pq`: PQ code values -> scRGB-unit linear BT.709 (can be slightly negative outside the 709 gamut).
pub fn pq_to_scrgb(code: [f32; 3]) -> [f32; 3] {
    bt2020_to_709(code.map(|c| pq_decode(c) / 80.0))
}

#[allow(dead_code)]
/// The value of a 10-bit code stored in the swapchain (UNORM rounding).
pub fn quantize10(code: f32) -> u32 {
    (code.clamp(0.0, 1.0) * 1023.0 + 0.5) as u32
}

// ---- the overlay under HDR10 -------------------------------------------------------------------------------------------------------

/// hudhook draws ImGui AFTER the final pass, straight into the back buffer, so under HDR10 its colours would be taken as PQ code values
/// (ImGui white = 10000 nits). This maps an ImGui style colour (sRGB-ish, 0..1) to the PQ code that shows it at the right brightness:
/// white UI = `paper_white` nits. Alpha is kept. Used for the style colours and the labels of the overlay.
pub fn ui_color(c: [f32; 4], paper_white: f32) -> [f32; 4] {
    let lin = [c[0], c[1], c[2]].map(|v| srgb_to_linear(v) * paper_white / 80.0);
    let p = scrgb_to_pq(lin);
    [p[0], p[1], p[2], c[3]]
}

fn srgb_to_linear(v: f32) -> f32 {
    let v = v.clamp(0.0, 1.0);
    if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
}

// ---- decision --------------------------------------------------------------------------------------------------------------------

/// Why the swapchain format is what it is (appended to the rule line).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormatRule {
    /// `hdr_format=scrgb`.
    Requested,
    /// HDR output is not active at all (SDR), so there is no HDR format to choose.
    NoHdrOutput,
    /// ReShade is present: HDR10 is never forced (its effects are not HDR-safe and it may change the swapchain itself).
    ReshadePresent,
    /// The previous start with HDR10 did not reach a stable frame (crash, driver reset...): HDR10 stays off until the marker is deleted.
    PreviousStartFailed,
    /// An HDR10 attempt failed in this session (creation, colour space): scRGB was used instead.
    FailedThisSession,
    /// HDR10 is used.
    Hdr10,
}

impl FormatRule {
    pub fn as_str(self) -> &'static str {
        match self {
            FormatRule::Requested => "hdr_format=scrgb",
            FormatRule::NoHdrOutput => "hdr_format=hdr10 ignored: HDR output is not active (see the rule above)",
            FormatRule::ReshadePresent => "hdr_format=hdr10 ignored: ReShade is present (HDR10 is never forced with it)",
            FormatRule::PreviousStartFailed => "hdr_format=hdr10 ignored: the previous start with HDR10 did not reach a stable picture; delete GW2Graphics\\hdr10.pending to try again",
            FormatRule::FailedThisSession => "hdr_format=hdr10 failed in this session, scRGB used instead",
            FormatRule::Hdr10 => "hdr_format=hdr10 (experimental): R10G10B10A2, colour space G2084/P2020",
        }
    }
}

/// The swapchain format for a swapchain that is going to be HDR (`hdr_active`: the mode decision said scRGB-class HDR output).
/// HDR10 only when requested, HDR output is active, ReShade is NOT present (whatever `hdr_output` says) and the crash guard is clear.
pub fn decide_format(request: HdrFormat, hdr_active: bool, reshade: bool, marker_present: bool) -> (SwapFormat, FormatRule) {
    if request == HdrFormat::Scrgb {
        (SwapFormat::Float16, FormatRule::Requested)
    } else if !hdr_active {
        (SwapFormat::Float16, FormatRule::NoHdrOutput)
    } else if reshade {
        (SwapFormat::Float16, FormatRule::ReshadePresent)
    } else if marker_present {
        (SwapFormat::Float16, FormatRule::PreviousStartFailed)
    } else {
        (SwapFormat::Rgb10A2, FormatRule::Hdr10)
    }
}

/// After a failed attempt with `tried`: the format for the ONE retry (None = no HDR retry, the caller falls back to the game's own
/// swapchain). HDR10 gets exactly one scRGB retry; scRGB gets none (that is the old behaviour).
pub fn fallback_after_failure(tried: SwapFormat) -> Option<SwapFormat> {
    match tried {
        SwapFormat::Rgb10A2 => Some(SwapFormat::Float16),
        SwapFormat::Float16 => None,
    }
}

/// `ResizeBuffers` arguments for a swapchain the mod created in `fmt` (`is_hdr`): the game passes its old format (R8G8B8A8_UNORM) or
/// UNKNOWN (keep). UNKNOWN stays UNKNOWN; ANY explicit format becomes the swapchain's own, so the buffers can never change format under
/// the colour space that was set (the format of a swapchain is never changed by a resize). The count is at least 2 (flip model), flags are
/// the ones valid for flip. For a swapchain that is not ours the arguments are untouched.
pub fn resize_args_for(is_hdr: bool, fmt: SwapFormat, count: u32, fmt_arg: u32, flags: u32) -> (u32, u32, u32) {
    if !is_hdr {
        return (count, fmt_arg, flags);
    }
    let f = if fmt_arg == 0 { 0 } else { fmt.dxgi() };
    let c = if count == 0 { 0 } else { count.max(2) };
    (c, f, flags & (0x2 | 0x800))
}

// ---- crash guard -----------------------------------------------------------------------------------------------------------------

pub const MARKER_NAME: &str = "hdr10.pending";
/// The marker is removed after this many presents AND this long after the swapchain was created.
pub const MARKER_CLEAR_FRAMES: u64 = 600;
pub const MARKER_CLEAR_MS: u64 = 15_000;

pub fn marker_path(dir: &Path) -> PathBuf {
    dir.join(MARKER_NAME)
}

/// Has the HDR10 swapchain proven itself (presented for a while without the process dying)?
pub fn marker_may_be_cleared(frames: u64, elapsed_ms: u64) -> bool {
    frames >= MARKER_CLEAR_FRAMES && elapsed_ms >= MARKER_CLEAR_MS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pq_matches_the_published_anchor_values() {
        // ST 2084 anchors: 0 -> 0, 10000 nits -> 1, 100 nits -> 0.5081 (the classic value), 203 nits -> about 0.5807.
        assert!(pq_encode(0.0) < 1e-5, "black is below one 10-bit step: {}", pq_encode(0.0));
        assert_eq!(quantize10(pq_encode(0.0)), 0);
        assert!((pq_encode(10000.0) - 1.0).abs() < 1e-6);
        assert!((pq_encode(100.0) - 0.508_078).abs() < 1e-4, "{}", pq_encode(100.0));
        assert!((pq_encode(203.0) - 0.580_7).abs() < 1e-3, "{}", pq_encode(203.0));
        assert!((pq_encode(1000.0) - 0.751_827).abs() < 1e-4, "{}", pq_encode(1000.0));
        // above 10000 clamps, negative is 0
        assert_eq!(pq_encode(1.0e6), pq_encode(10000.0));
        assert_eq!(pq_encode(-5.0), pq_encode(0.0));
    }

    #[test]
    fn pq_round_trips_over_a_log_range() {
        let mut n = 0.0005f32;
        while n < 10000.0 {
            let back = pq_decode(pq_encode(n));
            assert!((back - n).abs() <= n * 2e-4 + 1e-6, "{n} -> {back}");
            n *= 1.07;
        }
        // decode of every code is monotonic
        let mut prev = -1.0;
        for i in 0..=1023 {
            let v = pq_decode(i as f32 / 1023.0);
            assert!(v >= prev && v.is_finite());
            prev = v;
        }
        assert!(pq_decode(0.0) < 1e-6);
        assert!((pq_decode(1.0) - 10000.0).abs() < 1.0);
    }

    #[test]
    fn pq_is_monotonic_and_one_lsb_is_a_fraction_of_a_percent_above_black() {
        let mut prev = 0.0;
        let mut n = 0.001f32;
        while n < 10000.0 {
            let c = pq_encode(n);
            assert!(c > prev);
            prev = c;
            n *= 1.01;
        }
        // 1 LSB (1/1023) near 203 nits is well under 1 % of the luminance: no visible banding with the dither.
        let a = pq_decode(0.58);
        let b = pq_decode(0.58 + 1.0 / 1023.0);
        assert!((b - a) / a < 0.012, "{}", (b - a) / a);
    }

    #[test]
    fn the_709_to_2020_matrix_keeps_white_and_inverts() {
        for row in BT709_TO_2020 {
            assert!((row.iter().sum::<f32>() - 1.0).abs() < 1e-5, "{row:?}");
        }
        for row in BT2020_TO_709 {
            assert!((row.iter().sum::<f32>() - 1.0).abs() < 1e-5, "{row:?}");
        }
        let w = bt709_to_2020([1.0, 1.0, 1.0]);
        assert!(w.iter().all(|c| (c - 1.0).abs() < 1e-5), "{w:?}");
        // The published BT.2087 values.
        let r = bt709_to_2020([1.0, 0.0, 0.0]);
        assert!((r[0] - 0.6274).abs() < 1e-3 && (r[1] - 0.0691).abs() < 1e-3 && (r[2] - 0.0164).abs() < 1e-3, "{r:?}");
        // round trip, including a saturated and a bright colour
        for c in [[0.2, 0.5, 0.9], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0], [12.0, 3.0, 0.1], [0.0, 0.0, 0.0]] {
            let back = bt2020_to_709(bt709_to_2020(c));
            for i in 0..3 {
                assert!((back[i] - c[i]).abs() < 2e-4 * (1.0 + c[i].abs()), "{c:?} -> {back:?}");
            }
        }
        // 709 colours are inside 2020: never negative after the matrix
        for c in [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0], [0.3, 0.0, 0.7]] {
            assert!(bt709_to_2020(c).iter().all(|v| *v >= 0.0));
        }
    }

    #[test]
    fn scrgb_to_pq_and_back_round_trips() {
        for c in [[1.0f32, 1.0, 1.0], [2.5375, 2.5375, 2.5375], [0.01, 0.02, 0.03], [12.5, 4.0, 0.5], [0.0, 0.0, 0.0]] {
            let back = pq_to_scrgb(scrgb_to_pq(c));
            for i in 0..3 {
                assert!((back[i] - c[i]).abs() <= c[i].abs() * 6e-4 + 2e-5, "{c:?} -> {back:?}");
            }
        }
        // paper white 203 nits = 2.5375 scRGB = about the PQ value of 203 nits on all channels
        let w = scrgb_to_pq([203.0 / 80.0; 3]);
        assert!((w[0] - pq_encode(203.0)).abs() < 1e-5);
        // 10-bit quantisation
        assert_eq!(quantize10(0.0), 0);
        assert_eq!(quantize10(1.0), 1023);
        assert_eq!(quantize10(2.0), 1023);
        assert_eq!(quantize10(-1.0), 0);
    }

    #[test]
    fn hdr10_is_off_by_default_and_never_with_reshade_or_without_hdr() {
        // The default request keeps the current scRGB behaviour in every situation.
        for hdr in [false, true] {
            for rs in [false, true] {
                for mk in [false, true] {
                    assert_eq!(decide_format(HdrFormat::Scrgb, hdr, rs, mk), (SwapFormat::Float16, FormatRule::Requested));
                }
            }
        }
        assert_eq!(decide_format(HdrFormat::Hdr10, true, false, false), (SwapFormat::Rgb10A2, FormatRule::Hdr10));
        // Windows HDR off (no HDR output at all)
        assert_eq!(decide_format(HdrFormat::Hdr10, false, false, false), (SwapFormat::Float16, FormatRule::NoHdrOutput));
        // ReShade present: never HDR10 (also when hdr_output=on made the HDR output active)
        assert_eq!(decide_format(HdrFormat::Hdr10, true, true, false), (SwapFormat::Float16, FormatRule::ReshadePresent));
        // a failed previous start
        assert_eq!(decide_format(HdrFormat::Hdr10, true, false, true), (SwapFormat::Float16, FormatRule::PreviousStartFailed));
        // every non-HDR10 outcome has a reason line that says why
        for r in [FormatRule::NoHdrOutput, FormatRule::ReshadePresent, FormatRule::PreviousStartFailed, FormatRule::FailedThisSession] {
            assert!(r.as_str().contains("hdr_format=hdr10"), "{r:?}");
        }
        assert!(FormatRule::ReshadePresent.as_str().contains("ReShade"));
        assert!(FormatRule::PreviousStartFailed.as_str().contains("hdr10.pending"));
    }

    #[test]
    fn overlay_colours_are_mapped_to_paper_white_not_10000_nits() {
        let w = ui_color([1.0, 1.0, 1.0, 0.9], 203.0);
        assert!((w[0] - pq_encode(203.0)).abs() < 2e-4 && (w[1] - w[0]).abs() < 1e-4 && (w[2] - w[0]).abs() < 1e-4, "{w:?}");
        assert_eq!(w[3], 0.9);
        assert!(w[0] < 0.62, "white UI is about 0.58 in PQ, not 1.0");
        let b = ui_color([0.0, 0.0, 0.0, 1.0], 203.0);
        assert!(b[0] < 1e-5);
        // monotonic in the input
        assert!(ui_color([0.5, 0.5, 0.5, 1.0], 203.0)[0] < w[0]);
        // follows paper white
        assert!(ui_color([1.0; 4], 300.0)[0] > w[0]);
    }

    #[test]
    fn exactly_one_retry_after_a_failed_hdr10_attempt() {
        assert_eq!(fallback_after_failure(SwapFormat::Rgb10A2), Some(SwapFormat::Float16));
        assert_eq!(fallback_after_failure(SwapFormat::Float16), None);
        // the chain ends: hdr10 -> scRGB -> (the caller's) game desc
        let mut f = SwapFormat::Rgb10A2;
        let mut steps = 0;
        while let Some(n) = fallback_after_failure(f) {
            f = n;
            steps += 1;
        }
        assert_eq!((f, steps), (SwapFormat::Float16, 1));
    }

    #[test]
    fn formats_and_colour_spaces_belong_together() {
        assert_eq!((SwapFormat::Float16.dxgi(), SwapFormat::Float16.colour_space()), (10, 1));
        assert_eq!((SwapFormat::Rgb10A2.dxgi(), SwapFormat::Rgb10A2.colour_space()), (24, 12));
    }

    #[test]
    fn resize_keeps_the_swapchains_own_format() {
        // not ours: untouched
        assert_eq!(resize_args_for(false, SwapFormat::Rgb10A2, 1, 28, 0x1234), (1, 28, 0x1234));
        // ours, hdr10: the game's old format becomes R10G10B10A2, UNKNOWN stays UNKNOWN, count >= 2, flip flags only
        assert_eq!(resize_args_for(true, SwapFormat::Rgb10A2, 1, 28, 0x1002), (2, 24, 0x2));
        assert_eq!(resize_args_for(true, SwapFormat::Rgb10A2, 0, 0, 0x800), (0, 0, 0x800));
        assert_eq!(resize_args_for(true, SwapFormat::Float16, 1, 28, 0), (2, 10, 0));
        assert_eq!(resize_args_for(true, SwapFormat::Float16, 3, 28, 0), (3, 10, 0));
        // an explicit other format can never change the swapchain's format under its colour space
        assert_eq!(resize_args_for(true, SwapFormat::Rgb10A2, 2, 10, 0), (2, 24, 0));
        assert_eq!(resize_args_for(true, SwapFormat::Float16, 2, 24, 0), (2, 10, 0));
        assert_eq!(resize_args_for(true, SwapFormat::Float16, 2, 87, 0), (2, 10, 0));
    }

    #[test]
    fn the_crash_guard_clears_only_after_a_stable_stretch() {
        assert!(!marker_may_be_cleared(0, 0));
        assert!(!marker_may_be_cleared(MARKER_CLEAR_FRAMES, MARKER_CLEAR_MS - 1));
        assert!(!marker_may_be_cleared(MARKER_CLEAR_FRAMES - 1, MARKER_CLEAR_MS));
        assert!(marker_may_be_cleared(MARKER_CLEAR_FRAMES, MARKER_CLEAR_MS));
        assert_eq!(marker_path(Path::new("a")).file_name().unwrap(), MARKER_NAME);
    }
}
