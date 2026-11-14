//! HDR output audit (in the game, whites looked washed out, not like real HDR, with hdr_format=hdr10, paper white 400, peak 1405
//! effective). WARP tests that DECODE the final pass output back to absolute nits (inverse PQ, BT.2020 -> BT.709 for HDR10; x 80 for
//! scRGB) and check what the display is really asked to show: paper white, mid grey, black, a 4x highlight, NaN safety, both encodings
//! agreeing, the SDR decode gamma (black level) and the highlight share of big bright surfaces. Child module of the shader tests (it
//! uses their WARP harness). The `println!` lines are the before / after numbers of the audit (`cargo test -- --nocapture`).

use super::*;
use crate::hdr10::{bt2020_to_709, pq_decode, quantize10};
use crate::output::{FinalOpts, final_hlsl_with};

const CB1: [[f32; 4]; 1] = [[1.0, 0.0, 0.0, 0.0]];

fn final_ps(paper: f32, peak: f32, hdr10: bool, gamma_centi: u16) -> Vec<u8> {
    compile_ps(&final_hlsl_with(paper, peak, FinalOpts { hdr10, no_dither: true, pattern: 0, gamma_centi }), crate::output::FINAL_ENTRY).unwrap_or_else(|e| panic!("{e}"))
}

/// Absolute nits (linear BT.709 primaries) of one output pixel: HDR10 = PQ decode + BT.2020 -> BT.709; scRGB = x 80.
fn nits_of(p: [f32; 4], hdr10: bool) -> [f32; 3] {
    if hdr10 { bt2020_to_709([p[0], p[1], p[2]].map(pq_decode)) } else { [p[0] * 80.0, p[1] * 80.0, p[2] * 80.0] }
}

/// Renders one uniform encoded colour through the final pass and returns the nits of the centre pixel.
fn final_nits(g: &Gpu, ps: &[u8], enc: [f32; 3], hdr10: bool) -> [f32; 3] {
    let out = render(g, VS_LIGHT, ps, &Inputs { t0: &uniform([enc[0], enc[1], enc[2], 1.0]), t1: None, cb: &CB1 });
    let p = out[(N / 2) * N + N / 2];
    assert!(p.iter().all(|c| c.is_finite()), "NaN / inf out of the final pass for {enc:?}: {p:?}");
    nits_of(p, hdr10)
}

fn lum(c: [f32; 3]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

/// The encoded value the final pass decodes to `lin` (relative to paper white) with the given gamma (0 = piecewise sRGB).
fn enc_for(lin: f32, gamma: f32) -> f32 {
    if gamma >= 1.0 && lin <= 1.0 { lin.powf(1.0 / gamma) } else { tonemap::srgb_encode(lin) }
}

#[test]
fn the_final_shader_with_the_sdr_gamma_keeps_the_original_interface() {
    let want = interface_of(&disasm(&compile_ps(&crate::output::final_hlsl(203.0, 1000.0), crate::output::FINAL_ENTRY).unwrap()));
    assert_eq!(interface_of(&disasm(&final_ps(400.0, 1405.0, false, 220))), want, "scRGB + gamma");
    assert_eq!(interface_of(&disasm(&final_ps(400.0, 1405.0, true, 220))), want, "HDR10 (no dither) + gamma");
    let h = final_hlsl_with(400.0, 1405.0, FinalOpts { gamma_centi: 220, ..FinalOpts::default() });
    assert!(h.contains("#define SDR_GAMMA 2.2\n"), "{}", &h[..200]);
    assert!(!final_hlsl_with(400.0, 1405.0, FinalOpts::default()).contains("SDR_GAMMA 2"), "gamma 0 = no define = the piecewise sRGB shader");
}

/// Paper white, mid grey, black and a 4x highlight land on the right absolute nits in BOTH encodings, for typical levels and others.
#[test]
fn decoded_output_puts_white_grey_black_and_highlights_at_the_right_nits() {
    let Some(g) = gpu() else { return };
    for (paper, peak) in [(400.0f32, 1405.0f32), (203.0, 1000.0), (200.0, 4000.0), (80.0, 400.0)] {
        for hdr10 in [false, true] {
            for gamma_centi in [0u16, 220] {
                let gm = gamma_centi as f32 / 100.0;
                let ps = final_ps(paper, peak, hdr10, gamma_centi);
                let tag = format!("paper {paper} peak {peak} hdr10 {hdr10} gamma {gm}");
                // SDR white (encoded 1.0) = paper white on every channel.
                let w = final_nits(&g, &ps, [1.0; 3], hdr10);
                for c in w {
                    assert!((c - paper).abs() <= paper * 0.01, "{tag}: white = {c} nits, want {paper}");
                }
                // Mid grey (18 % of white, whatever decode curve is in use) = 0.18 x paper white.
                let e = enc_for(0.18, gm);
                let m = lum(final_nits(&g, &ps, [e; 3], hdr10));
                assert!((m - 0.18 * paper).abs() <= 0.18 * paper * 0.015, "{tag}: mid grey = {m} nits, want {}", 0.18 * paper);
                // Black stays black (PQ code 0 = 0 nits; no lift anywhere in the chain).
                let b = final_nits(&g, &ps, [0.0; 3], hdr10);
                assert!(b.iter().all(|c| c.abs() < 1e-3), "{tag}: black = {b:?} nits");
                // A highlight 4x paper white: exactly 4x when the knee (0.8 x headroom) is above it, else rolled off, never above the peak.
                let four = lum(final_nits(&g, &ps, [tonemap::srgb_encode(4.0); 3], hdr10));
                let want = crate::photo::final_pass_cpu_g([tonemap::srgb_encode(4.0); 3], paper, peak, gm)[1] * 80.0;
                assert!((four - want).abs() <= want * 0.015, "{tag}: 4x white = {four} nits, host reference {want}");
                assert!(four <= peak * 1.01, "{tag}: {four} above the peak {peak}");
                let (_, knee) = crate::output::shoulder_constants(paper, peak);
                if knee > 4.0 {
                    assert!((four - 4.0 * paper).abs() <= 4.0 * paper * 0.015, "{tag}: 4x white must be 4x paper white below the knee: {four}");
                } else {
                    assert!(four >= 0.8 * peak.min(4.0 * paper), "{tag}: the shoulder keeps a 4x highlight near the top ({four} vs peak {peak})");
                }
            }
        }
    }
}

/// HDR10 and scRGB describe the same picture: decoded to nits they agree within the float16 / PQ precision, also after a real
/// 10-bit quantisation of the HDR10 code values; junk input gives finite, non-negative output in both.
#[test]
fn hdr10_and_scrgb_agree_after_decoding_and_survive_junk() {
    let Some(g) = gpu() else { return };
    let colours: [[f32; 3]; 12] = [
        [0.0, 0.0, 0.0],
        [0.02, 0.02, 0.02],
        [0.1, 0.08, 0.05],
        [0.5, 0.5, 0.5],
        [0.95, 0.80, 0.60],
        [1.0, 1.0, 1.0],
        [0.9, 0.1, 0.1],
        [0.1, 0.8, 0.2],
        [0.15, 0.3, 0.95],
        [1.4, 1.3, 1.1],
        [2.2, 1.5, 0.6],
        [6.0, 6.0, 6.0],
    ];
    let mut worst = 0.0f32;
    for (paper, peak) in [(400.0f32, 1405.0f32), (203.0, 1000.0)] {
        for gamma_centi in [0u16, 220] {
            let (s, h) = (final_ps(paper, peak, false, gamma_centi), final_ps(paper, peak, true, gamma_centi));
            for c in colours {
                let a = final_nits(&g, &s, c, false);
                let b = final_nits(&g, &h, c, true);
                let out = render(&g, VS_LIGHT, &h, &Inputs { t0: &uniform([c[0], c[1], c[2], 1.0]), t1: None, cb: &CB1 })[0];
                let q = bt2020_to_709([out[0], out[1], out[2]].map(|v| pq_decode(quantize10(v) as f32 / 1023.0)));
                // float16 storage of the PQ code (about 5e-4 near 0.6) is about 0.6 % of each BT.2020 channel; the inverse matrix (entries
                // up to 1.66) spreads the error of the LARGEST channel onto the small ones of a saturated colour: the tolerance follows
                // the largest channel of the pixel.
                let big = a[0].max(a[1]).max(a[2]);
                for i in 0..3 {
                    let tol = big * 0.012 + 0.05;
                    worst = worst.max((a[i] - b[i]).abs() / tol);
                    assert!((a[i] - b[i]).abs() <= tol, "paper {paper} gamma {gamma_centi} {c:?} ch {i}: scRGB {} nits vs HDR10 {} nits", a[i], b[i]);
                    // one 10-bit step of PQ is under 1.2 % above 1 nit (half a step after rounding)
                    assert!((a[i] - q[i]).abs() <= big * 0.015 + 0.08, "10-bit: {c:?} ch {i}: {} vs {}", a[i], q[i]);
                }
            }
            for junk in [[f32::NAN, 0.5, 0.5], [f32::INFINITY, 1.0, 1.0], [-3.0, -3.0, -3.0], [1e30, 0.0, 1e30]] {
                for (ps, hdr10) in [(&s, false), (&h, true)] {
                    let out = render(&g, VS_LIGHT, ps, &Inputs { t0: &uniform([junk[0], junk[1], junk[2], 1.0]), t1: None, cb: &CB1 })[0];
                    assert!(out.iter().all(|v| v.is_finite() && *v >= 0.0), "junk {junk:?} hdr10 {hdr10}: {out:?}");
                    assert!(nits_of(out, hdr10).iter().all(|v| *v <= peak * 1.02), "junk {junk:?} hdr10 {hdr10}: above the peak");
                }
            }
        }
    }
    println!("hdr10 vs scRGB: worst difference {worst:.2} of the tolerance");
}

/// The black level fix: the SDR range decoded with gamma 2.2 instead of the piecewise sRGB curve. Shadows are 2-10x darker near black,
/// white, mid tones (within 2 %) and everything above SDR white are unchanged; monotonic across 1.0. Prints the before / after table.
#[test]
fn sdr_gamma_deepens_the_shadows_and_keeps_white_mids_and_highlights() {
    let Some(g) = gpu() else { return };
    let (paper, peak) = (400.0f32, 1405.0f32);
    for hdr10 in [false, true] {
        let (old, new) = (final_ps(paper, peak, hdr10, 0), final_ps(paper, peak, hdr10, 220));
        println!("paper white {paper}, peak {peak}, hdr10 {hdr10}: encoded value -> nits (piecewise sRGB, before | gamma 2.2, after)");
        let mut prev = -1.0f32;
        for v in [0.01f32, 0.02, 0.05, 0.1, 0.2, 0.3, 0.5, 0.7, 0.9, 0.99, 1.0, 1.01, 1.2, 1.6] {
            let a = lum(final_nits(&g, &old, [v; 3], hdr10));
            let b = lum(final_nits(&g, &new, [v; 3], hdr10));
            println!("  {v:5.2}: {a:9.3} | {b:9.3} nits");
            assert!(b >= prev - 1e-3 * prev.abs() - 1e-4, "not monotonic at {v}: {prev} -> {b}");
            prev = b;
            let want_new = paper * if v <= 1.0 { v.powf(2.2) } else { tonemap::srgb_decode(v) };
            assert!((b - want_new).abs() <= want_new * 0.015 + 0.02, "gamma 2.2 at {v}: {b} vs {want_new}");
            if v <= 0.05 {
                assert!(b < a * 0.5, "shadow {v} must be clearly darker: {a} -> {b}");
            } else if (0.3..=0.9).contains(&v) {
                assert!((b - a).abs() <= a * 0.12, "mid tone {v} changes little: {a} -> {b}");
            } else if v >= 1.0 {
                assert!((b - a).abs() <= a * 0.01, "white and highlights unchanged at {v}: {a} -> {b}");
            }
        }
    }
}

/// The CPU mirror of the final pass (photos of the UI-free scene) follows the gamma like the shader.
#[test]
fn the_cpu_final_pass_follows_the_sdr_gamma() {
    let Some(g) = gpu() else { return };
    for gamma_centi in [0u16, 220, 240] {
        let gm = gamma_centi as f32 / 100.0;
        let ps = final_ps(400.0, 1405.0, false, gamma_centi);
        for v in [0.0f32, 0.03, 0.2, 0.5, 0.99, 1.0, 1.3, 3.0, 20.0] {
            let t = [v, v * 0.7, v * 0.4];
            let got = final_nits(&g, &ps, t, false);
            let want = crate::photo::final_pass_cpu_g(t, 400.0, 1405.0, gm).map(|c| c * 80.0);
            for i in 0..3 {
                assert!((got[i] - want[i]).abs() <= want[i] * 2e-3 + 0.05, "gamma {gm} v {v} ch {i}: shader {} vs cpu {}", got[i], want[i]);
            }
        }
    }
    assert_eq!(crate::photo::sdr_decode(f32::NAN, 2.2), 0.0);
    assert_eq!(crate::photo::sdr_decode(-1.0, 2.2), 0.0);
    assert_eq!(crate::photo::sdr_decode(0.5, 0.0), tonemap::srgb_decode(0.5));
}

#[test]
fn sdr_gamma_setting_is_clamped_parsed_and_saved() {
    use crate::config::Config;
    use crate::output::clamp_sdr_gamma;
    assert_eq!(Config::default().sdr_gamma, 2.2);
    assert_eq!(clamp_sdr_gamma(f32::NAN), 2.2);
    assert_eq!(clamp_sdr_gamma(0.0), 0.0);
    assert_eq!(clamp_sdr_gamma(0.7), 0.0);
    assert_eq!(clamp_sdr_gamma(1.0), 1.8);
    assert_eq!(clamp_sdr_gamma(2.4), 2.4);
    assert_eq!(clamp_sdr_gamma(9.0), 2.8);
    assert_eq!(Config::parse("sdr_gamma=srgb").sdr_gamma, 0.0);
    assert_eq!(Config::parse("sdr_gamma=SRGB").sdr_gamma, 0.0);
    assert_eq!(Config::parse("sdr_gamma=2.4").sdr_gamma, 2.4);
    assert_eq!(Config::parse("sdr_gamma=abc").sdr_gamma, 2.2);
    assert_eq!(Config::parse("sdr_gamma=0").sdr_gamma, 0.0);
    for g in [0.0f32, 2.2, 2.6] {
        let t = Config { sdr_gamma: g, ..Config::default() }.to_ini();
        assert_eq!(t.matches("sdr_gamma=").count(), 1, "{t}");
        assert_eq!(Config::parse(&t).sdr_gamma, g, "round trip of {g}");
    }
    assert!(Config { sdr_gamma: 0.0, ..Config::default() }.to_ini().contains("sdr_gamma=srgb\n"));
}

/// The highlight share of big bright surfaces: a sunlit white surface at SDR white (white fur, white armour, the halo says "not a light
/// source") must now come out clearly above paper white at typical levels; a light keeps the full gain; dark pixels untouched.
/// Then the whole chain (light adjust -> final pass, HDR10, decoded) for a synthetic scene, before (share 0.3) and after (0.6).
#[test]
fn a_white_surface_gets_real_headroom_through_the_whole_chain() {
    let Some(g) = gpu() else { return };
    let headroom = 1405.0f32 / 400.0;
    let ident_t1 = [0.0f32, 0.0, 0.0, 1.0]; // identity remap (mid weight 1)
    let light_ps = compile_ps(LIGHT_ADJUST_HLSL, LIGHT_ADJUST_ENTRY).unwrap_or_else(|e| panic!("{e}"));
    let fin = final_ps(400.0, 1405.0, true, 220);
    let la = |enc: f32, slider: f32, surface: bool| -> Vec<[f32; 4]> {
        let gain = crate::datapath::effective_gain(slider, headroom);
        // halo local mean = the pixel's own light: a big uniform area (surface); 0 = halo off (every pixel a source).
        let glow = surface.then_some((Some([0.0, 0.0, 0.0, enc * enc]), 1.0f32));
        render_full(&g, VS_LIGHT, &light_ps, &Inputs { t0: &uniform([enc, enc, enc, 1.0]), t1: Some(ident_t1), cb: &LIGHT_CB }, Some((1.0, [gain, 0.0, 0.0, 0.0])), glow)
    };
    let chain_nits = |enc: f32, slider: f32, surface: bool| -> f32 {
        let l = la(enc, slider, surface);
        let out = render(&g, VS_LIGHT, &fin, &Inputs { t0: &l, t1: None, cb: &CB1 });
        lum(nits_of(out[(N / 2) * N + N / 2], true))
    };
    // the share the shader applies (0.6) vs the old one, analytically, for the record
    for slider in [2.0f32, 2.5] {
        let gn = crate::datapath::effective_gain(slider, headroom);
        println!("slider {slider}: light gain x{gn:.2}; surface gain x{:.2} (share 0.3, before) -> x{:.2} (share {}, after)", 1.0 + (gn - 1.0) * 0.3, 1.0 + (gn - 1.0) * crate::datapath::SURFACE_SHARE, crate::datapath::SURFACE_SHARE);
    }
    assert_eq!(crate::datapath::SURFACE_SHARE, 0.6);
    let white = 0.98f32; // encoded (gamma 2.0) white fur in the sun: linear 0.96
    for slider in [2.0f32, 2.5] {
        let gn = crate::datapath::effective_gain(slider, headroom);
        let surf = la(white, slider, true)[(N / 2) * N + N / 2];
        let src = la(white, slider, false)[(N / 2) * N + N / 2];
        let plain = la(white, 1.0, true)[(N / 2) * N + N / 2];
        let (ls, lsrc, lp) = (surf[1] * surf[1], src[1] * src[1], plain[1] * plain[1]);
        let want = 1.0 + (gn - 1.0) * crate::datapath::SURFACE_SHARE;
        assert!((ls / lp - want).abs() <= want * 0.04, "slider {slider}: surface x{} want x{want}", ls / lp);
        assert!(lsrc > ls, "a light gets more than a surface");
        let n = chain_nits(white, slider, true);
        println!("slider {slider}: sunlit white surface (encoded {white}) -> {n:.0} nits on screen (paper white 400, peak 1405, HDR10 decoded)");
        assert!(n >= 400.0 * 1.4 && n <= 1405.0, "slider {slider}: the white surface must be well above paper white: {n} nits");
    }
    // Dark and mid pixels are not touched by the boost (the expansion starts at 0.4 of SDR white).
    for enc in [0.1f32, 0.3, 0.6] {
        let a = la(enc, 2.0, true)[(N / 2) * N + N / 2][1];
        let b = la(enc, 1.0, true)[(N / 2) * N + N / 2][1];
        assert!((a - b).abs() <= 0.006, "encoded {enc}: {b} -> {a}");
    }
    // Synthetic scene through the whole chain at typical levels (slider 2), after the fixes: black .. white surface .. lamp.
    println!("synthetic scene, in-game levels (paper 400, peak 1405, slider 2, HDR10 decoded), after: ");
    for (name, enc, surface) in [("black 0.02", 0.02f32, true), ("shadow 0.15", 0.15, true), ("mid grey 0.42", 0.42, true), ("bright 0.8", 0.8, true), ("white surface 0.98", 0.98, true), ("lamp 0.98", 0.98, false), ("lamp 1.3", 1.3, false)] {
        println!("  {name:20} {:8.2} nits", chain_nits(enc, 2.0, surface));
    }
}
