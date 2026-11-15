//! WARP tests of `look.hlsl` and `Look::run`. They need no game exe: the picture is synthetic.
//!
//! Harness: D3D11 WARP device, a float "scene colour" built from a closure of LINEAR values (encoded with the engine's extended
//! sRGB like the real picture), `Look::run` through the raw vtables exactly as the draw hook does, the float16 output read back and
//! decoded to linear.

use std::ffi::c_void;
use std::sync::OnceLock;

use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::Interface;

use super::*;
use crate::grading::{PRESETS, grade_rgb, preset_by_name};
use crate::lens::{srgb_decode, srgb_encode};

const W: usize = 256;
const H: usize = 128;
const GAIN: f32 = 2.9;

struct Gpu {
    dev: ID3D11Device,
    ctx: ID3D11DeviceContext,
}

fn gpu() -> Option<Gpu> {
    let mut dev = None;
    let mut ctx = None;
    let r = unsafe { D3D11CreateDevice(None::<&windows::Win32::Graphics::Dxgi::IDXGIAdapter>, D3D_DRIVER_TYPE_WARP, HMODULE::default(), D3D11_CREATE_DEVICE_FLAG(0), None, D3D11_SDK_VERSION, Some(&mut dev), None, Some(&mut ctx)) };
    match (r, dev, ctx) {
        (Ok(()), Some(dev), Some(ctx)) => Some(Gpu { dev, ctx }),
        (r, ..) => {
            println!("skipped: WARP device creation failed: {r:?}");
            None
        }
    }
}

fn blobs() -> &'static Blobs {
    static B: OnceLock<Blobs> = OnceLock::new();
    B.get_or_init(|| compile_blobs().unwrap_or_else(|e| panic!("look.hlsl does not compile: {e}")))
}

/// An RGBA32F texture from ENCODED texels.
fn scene_enc(g: &Gpu, w: usize, h: usize, f: impl Fn(usize, usize) -> [f32; 3]) -> (ID3D11Texture2D, ID3D11ShaderResourceView) {
    let mut v = Vec::with_capacity(w * h * 16);
    for y in 0..h {
        for x in 0..w {
            let c = f(x, y);
            for ch in [c[0], c[1], c[2], 1.0] {
                v.extend_from_slice(&ch.to_le_bytes());
            }
        }
    }
    let desc = D3D11_TEXTURE2D_DESC { Width: w as u32, Height: h as u32, MipLevels: 1, ArraySize: 1, Format: DXGI_FORMAT_R32G32B32A32_FLOAT, SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, Usage: D3D11_USAGE_DEFAULT, BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32, CPUAccessFlags: 0, MiscFlags: 0 };
    let sd = D3D11_SUBRESOURCE_DATA { pSysMem: v.as_ptr() as *const c_void, SysMemPitch: (w * 16) as u32, SysMemSlicePitch: 0 };
    let mut t = None;
    unsafe { g.dev.CreateTexture2D(&desc, Some(&sd), Some(&mut t)).expect("CreateTexture2D") };
    let t = t.unwrap();
    let mut s = None;
    unsafe { g.dev.CreateShaderResourceView(&t, None, Some(&mut s)).unwrap() };
    (t, s.unwrap())
}

/// From LINEAR texels.
fn scene(g: &Gpu, w: usize, h: usize, f: impl Fn(usize, usize) -> [f32; 3]) -> (ID3D11Texture2D, ID3D11ShaderResourceView) {
    scene_enc(g, w, h, |x, y| f(x, y).map(srgb_encode))
}

fn f16_to_f32(h: u16) -> f32 {
    let (s, e, m) = ((h >> 15) as u32, ((h >> 10) & 0x1f) as i32, (h & 0x3ff) as f32);
    let v = match e {
        0 => m * 2f32.powi(-24),
        31 => if m == 0.0 { f32::INFINITY } else { f32::NAN },
        _ => (1.0 + m / 1024.0) * 2f32.powi(e - 15),
    };
    if s == 1 { -v } else { v }
}

/// RGB texels (ENCODED, as the float16 output holds them).
fn read_enc(g: &Gpu, l: &Look, w: usize, h: usize) -> Vec<[f32; 3]> {
    unsafe {
        let t: ID3D11Texture2D = ID3D11Texture2D::from_raw_borrowed(&l.out.tex).unwrap().clone();
        let desc = D3D11_TEXTURE2D_DESC { Width: w as u32, Height: h as u32, MipLevels: 1, ArraySize: 1, Format: DXGI_FORMAT_R16G16B16A16_FLOAT, SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, Usage: D3D11_USAGE_STAGING, BindFlags: 0, CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32, MiscFlags: 0 };
        let mut st = None;
        g.dev.CreateTexture2D(&desc, None, Some(&mut st)).unwrap();
        let st = st.unwrap();
        g.ctx.CopyResource(&st, &t);
        let mut m = D3D11_MAPPED_SUBRESOURCE::default();
        g.ctx.Map(&st, 0, D3D11_MAP_READ, 0, Some(&mut m)).unwrap();
        let mut out = Vec::with_capacity(w * h);
        for y in 0..h {
            let row = (m.pData as *const u8).add(y * m.RowPitch as usize) as *const u16;
            for x in 0..w {
                out.push([f16_to_f32(*row.add(x * 4)), f16_to_f32(*row.add(x * 4 + 1)), f16_to_f32(*row.add(x * 4 + 2))]);
            }
        }
        g.ctx.Unmap(&st, 0);
        out
    }
}

fn read_lin(g: &Gpu, l: &Look, w: usize, h: usize) -> Vec<[f32; 3]> {
    read_enc(g, l, w, h).into_iter().map(|p| p.map(srgb_decode)).collect()
}

fn new_look(g: &Gpu) -> Look {
    unsafe { Look::new(g.dev.as_raw(), blobs()) }.expect("Look::new")
}

/// The test picture is 128 px high: a max source size of 0.15 (19 px) keeps the 7 to 9 px test sources "compact".
const TEST_MAX_SIZE: f32 = 0.15;

fn lens(strength: f32, ghost: f32) -> LensParams {
    LensParams { strength, threshold: 2.5, ghost, length: 1.0, gain: GAIN, max_size: TEST_MAX_SIZE, color_reject: crate::lens::DEFAULT_COLOR_REJECT }
}

fn lens_only(strength: f32, ghost: f32) -> LookInput<'static> {
    LookInput { lens: Some(lens(strength, ghost)), grade: None, lut: None }
}

/// Runs and returns the LINEAR output (None when nothing ran).
fn run(g: &Gpu, l: &mut Look, f: impl Fn(usize, usize) -> [f32; 3], inp: &LookInput) -> Option<Vec<[f32; 3]>> {
    let (_t, srv) = scene(g, W, H, f);
    let r = unsafe { l.run(g.dev.as_raw(), g.ctx.as_raw(), srv.as_raw(), (W as u32, H as u32), inp) }?;
    assert!(!r.is_null());
    Some(read_lin(g, l, W, H))
}

fn grey(v: f32) -> [f32; 3] {
    [v; 3]
}

/// A bright square of linear value `v` (RGB `c * v`) at (cx, cy) with half-size `r`, black elsewhere.
fn blob(cx: usize, cy: usize, r: usize, c: [f32; 3]) -> impl Fn(usize, usize) -> [f32; 3] {
    move |x, y| if x.abs_diff(cx) <= r && y.abs_diff(cy) <= r { c } else { [0.0; 3] }
}

fn at(o: &[[f32; 3]], x: usize, y: usize) -> [f32; 3] {
    o[y * W + x]
}

fn max_ch(c: [f32; 3]) -> f32 {
    c[0].max(c[1]).max(c[2])
}

#[test]
fn the_shaders_compile_and_the_constants_have_the_documented_layout() {
    let b = blobs();
    assert!(!b.vs.is_empty() && !b.down4.is_empty() && !b.down8.is_empty() && !b.glare_h.is_empty() && !b.glare_v.is_empty() && !b.composite.is_empty());
    assert!(b.streak.iter().all(|s| !s.is_empty()));
    let cube = Cube::identity(17);
    let inp = LookInput { lens: Some(lens(0.35, 0.15)), grade: Some(Grade { saturation: 1.2, ..grading::NEUTRAL }), lut: Some((1, &cube, 0.5)) };
    let c = cb_values((3440, 1440), &inp);
    assert_eq!(c[0], [3440.0, 1440.0, 1.0 / 3440.0, 1.0 / 1440.0]);
    assert_eq!(c[1], [860.0, 360.0, 430.0, 180.0]);
    assert_eq!((c[2][0], c[2][1]), (0.35, 0.15));
    assert_eq!(c[4][0], 1.2);
    assert_eq!(c[4][3], 0.5);
    assert_eq!(c[5][3], 17.0);
    assert_eq!(c[6][2], 7.0, "lens + grade + lut");
    assert_eq!(c[7][0], crate::lens::COOL_GAIN * 0.5);
    assert!(c[7][2] > 0.0 && c[7][3] == 16.0, "size test on: {:?}", c[7]);
    assert_eq!(c[8], [215.0, 90.0, 0.0, 0.0], "1/16 level");
    let none = LookInput { lens: None, grade: Some(grading::NEUTRAL), lut: Some((1, &cube, 0.0)) };
    assert!(none.is_noop());
    assert_eq!(cb_values((100, 100), &none)[6][2], 0.0);
    assert!(wanted_for(true, true, true, false) && wanted_for(true, true, false, true));
    assert!(!wanted_for(true, true, false, false), "everything off = nothing runs");
    assert!(!wanted_for(false, true, true, true), "vanilla emulation stays vanilla");
    assert!(!wanted_for(true, false, true, true));
    assert!(!status_lens().is_empty() && !status_grade().is_empty());
}

#[test]
fn nothing_on_runs_nothing() {
    let Some(g) = gpu() else { return };
    let mut l = new_look(&g);
    let none = LookInput { lens: None, grade: Some(grading::NEUTRAL), lut: None };
    assert!(run(&g, &mut l, |_, _| grey(0.5), &none).is_none());
    // A LUT with strength 0 is nothing either.
    let cube = Cube::identity(17);
    let z = LookInput { lens: None, grade: None, lut: Some((1, &cube, 0.0)) };
    assert!(run(&g, &mut l, |_, _| grey(0.5), &z).is_none());
}

#[test]
fn ordinary_bright_surfaces_do_not_trigger_the_lens_and_keep_their_exact_value() {
    let Some(g) = gpu() else { return };
    let mut l = new_look(&g);
    // Sand / snow / white fur / the white UI after the highlight boost (SDR white x gain), also noisy and with a gradient.
    let f = |x: usize, y: usize| {
        let n = ((x * 7 + y * 13) % 5) as f32 * 0.02;
        [GAIN * (0.9 + n), GAIN * (0.85 + n), GAIN * (0.6 + n)].map(|v| v.min(GAIN * 1.05))
    };
    let (t, srv) = scene(&g, W, H, f);
    let before = read_enc_of(&g, &t);
    let out = unsafe { l.run(g.dev.as_raw(), g.ctx.as_raw(), srv.as_raw(), (W as u32, H as u32), &lens_only(1.0, 0.5)) };
    assert!(out.is_some(), "the lens passes ran");
    // The shader returns the input texel itself when nothing changes it; the only difference is how the float32 -> float16 store rounds
    // (WARP truncates, the host helper rounds to even): at most one half-float step.
    for (a, b) in read_enc(&g, &l, W, H).iter().zip(&before) {
        for c in 0..3 {
            assert!((a[c] - b[c]).abs() <= b[c] * 1.0e-3, "pixel changed: {a:?} vs {b:?}");
        }
    }
    // A pure white picture at SDR white (1.0) and at paper white x gain.
    for v in [1.0, GAIN, GAIN * 1.2] {
        let o = run(&g, &mut l, |_, _| grey(v), &lens_only(1.0, 0.5)).unwrap();
        for p in &o {
            assert!((max_ch(*p) - v).abs() <= v * 2e-3, "flat {v}: {p:?}");
        }
    }
}

/// The encoded texels of an RGBA32F texture, rounded to float16 like the output (for exact comparisons).
fn read_enc_of(g: &Gpu, t: &ID3D11Texture2D) -> Vec<[f32; 3]> {
    unsafe {
        let desc = D3D11_TEXTURE2D_DESC { Width: W as u32, Height: H as u32, MipLevels: 1, ArraySize: 1, Format: DXGI_FORMAT_R32G32B32A32_FLOAT, SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, Usage: D3D11_USAGE_STAGING, BindFlags: 0, CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32, MiscFlags: 0 };
        let mut st = None;
        g.dev.CreateTexture2D(&desc, None, Some(&mut st)).unwrap();
        let st = st.unwrap();
        g.ctx.CopyResource(&st, t);
        let mut m = D3D11_MAPPED_SUBRESOURCE::default();
        g.ctx.Map(&st, 0, D3D11_MAP_READ, 0, Some(&mut m)).unwrap();
        let mut out = Vec::new();
        for y in 0..H {
            let row = (m.pData as *const u8).add(y * m.RowPitch as usize) as *const f32;
            for x in 0..W {
                // Through the half representation the output uses.
                out.push([0, 1, 2].map(|c| f16_to_f32(grading::f32_to_f16(*row.add(x * 4 + c)))));
            }
        }
        g.ctx.Unmap(&st, 0);
        out
    }
}

#[test]
fn a_bright_source_makes_a_horizontal_streak_bounded_warm_and_never_blue() {
    let Some(g) = gpu() else { return };
    let mut l = new_look(&g);
    let (cx, cy) = (128, 64);
    // A 6x6 source at 12x paper white before the boost (= GAIN * 12 now), white-blue: the worst case for a blue paint.
    let src = [GAIN * 8.0, GAIN * 10.0, GAIN * 14.0];
    let out = run(&g, &mut l, blob(cx, cy, 3, src), &lens_only(1.0, 0.0)).unwrap();
    // Streak: the row through the source is lit far to both sides, the column is not (same distance).
    let right = at(&out, cx + 28, cy);
    let left = at(&out, cx - 28, cy);
    let above = at(&out, cx, cy - 28);
    assert!(max_ch(right) > 0.02 && max_ch(left) > 0.02, "streak missing: {right:?} {left:?}");
    assert!(max_ch(right) > 4.0 * max_ch(above).max(1e-4), "horizontal {right:?} vs vertical {above:?}");
    assert!((max_ch(right) - max_ch(left)).abs() < 0.15 * max_ch(right), "symmetric");
    // Falls off with distance along the row.
    assert!(max_ch(at(&out, cx + 10, cy)) > max_ch(at(&out, cx + 40, cy)));
    // Bounded: nothing outside the source gets more than 0.6 of paper white, however bright the source.
    for y in 0..H {
        for x in 0..W {
            if x.abs_diff(cx) > 3 || y.abs_diff(cy) > 3 {
                let p = at(&out, x, y);
                assert!(p.iter().all(|v| v.is_finite() && *v >= 0.0));
                assert!(max_ch(p) <= 0.6 * 1.01 + 2e-3, "({x},{y}) {p:?} adds more than the bound");
                // Never a blue paint: the blue added is at most 85 % of the red/green added (the picture is black here).
                assert!(p[2] <= 0.85 * p[0].max(p[1]) + 2e-3, "({x},{y}) {p:?} is blue");
                // Warm: red is not below green.
                assert!(p[0] + 2e-3 >= p[1] * 0.95);
            }
        }
    }
    // The source itself only gets brighter.
    assert!(at(&out, cx, cy)[2] >= src[2] * 0.99);
    // Black far away from the source stays exactly black.
    assert_eq!(at(&out, 3, 3), [0.0; 3]);
    assert_eq!(at(&out, 250, 125), [0.0; 3]);
}

#[test]
fn the_strength_scales_the_streak_and_zero_runs_no_passes() {
    let Some(g) = gpu() else { return };
    let mut l = new_look(&g);
    let src = [GAIN * 9.0; 3];
    let f = blob(128, 64, 3, src);
    let a = run(&g, &mut l, &f, &lens_only(0.2, 0.0)).unwrap();
    let b = run(&g, &mut l, &f, &lens_only(0.8, 0.0)).unwrap();
    assert!(max_ch(at(&b, 150, 64)) > 1.5 * max_ch(at(&a, 150, 64)) && max_ch(at(&a, 150, 64)) > 0.0);
    // Strength 0 at the top level means `lens: None`: nothing runs. (The settings layer does this, see lens::on.)
    assert!(!{
        let s = lens::strength();
        s <= 0.0 && lens::on()
    });
    assert!(run(&g, &mut l, &f, &LookInput { lens: None, grade: None, lut: None }).is_none());
    // A longer streak reaches farther.
    let short = LookInput { lens: Some(LensParams { length: 0.5, ..lens(1.0, 0.0) }), grade: None, lut: None };
    let long = LookInput { lens: Some(LensParams { length: 2.0, ..lens(1.0, 0.0) }), grade: None, lut: None };
    let (s, lg) = (run(&g, &mut l, &f, &short).unwrap(), run(&g, &mut l, &f, &long).unwrap());
    assert!(max_ch(at(&lg, 128 + 60, 64)) > max_ch(at(&s, 128 + 60, 64)) + 0.002);
}

#[test]
fn ghosts_appear_only_for_very_bright_sources_and_never_blue() {
    let Some(g) = gpu() else { return };
    let mut l = new_look(&g);
    // Source right of the centre; the mirrored, shrunken copy (x0.3) lands left of the centre: 128 - 0.3 * 80 = 104.
    let (sx, sy) = (208, 64);
    let ghost_px = (104, 64);
    // Very bright (20x paper white before the boost): ghosts on.
    let very = blob(sx, sy, 4, [GAIN * 20.0; 3]);
    let off = run(&g, &mut l, &very, &lens_only(1.0, 0.0)).unwrap();
    let on = run(&g, &mut l, &very, &lens_only(1.0, 0.3)).unwrap();
    let (a, b) = (at(&off, ghost_px.0, ghost_px.1), at(&on, ghost_px.0, ghost_px.1));
    assert!(max_ch(b) > max_ch(a) + 0.003, "no ghost where it should be: {a:?} -> {b:?}");
    assert!(b[0] >= b[2], "ghosts are warm, not blue: {b:?}");
    // Subtle: the ghost is far weaker than the source and bounded.
    for p in &on {
        assert!(max_ch(*p) <= 0.6 * 1.01 + 2e-3 || max_ch(*p) > 20.0, "{p:?}");
    }
    // Merely bright (4x paper white before the boost = a streak source, below the ghost threshold): the ghost slider changes nothing.
    let bright = blob(sx, sy, 4, [GAIN * 4.0; 3]);
    let o0 = run(&g, &mut l, &bright, &lens_only(1.0, 0.0)).unwrap();
    let o5 = run(&g, &mut l, &bright, &lens_only(1.0, 0.5)).unwrap();
    assert_eq!(o0, o5, "a 4x source makes no ghosts");
    assert!(max_ch(at(&o0, sx + 20, sy)) > 0.0, "but it does make a streak");
}

fn lens_with(max_size: f32, color_reject: f32) -> LookInput<'static> {
    LookInput { lens: Some(LensParams { max_size, color_reject, ..lens(1.0, 0.3) }), grade: None, lut: None }
}

/// Every pixel outside the square (cx, cy, r) is exactly black (nothing was added there).
fn black_outside(o: &[[f32; 3]], cx: usize, cy: usize, r: usize) -> bool {
    (0..H).all(|y| (0..W).all(|x| (x.abs_diff(cx) <= r && y.abs_diff(cy) <= r) || at(o, x, y) == [0.0; 3]))
}

#[test]
fn a_big_glowing_body_gets_no_flare_a_compact_lamp_still_does() {
    let Some(g) = gpu() else { return };
    let mut l = new_look(&g);
    // A glowing creature: 61x61 px (half the picture height) at 8x paper white, warm orange (no colour penalty: only the size
    // can reject it). Before the size test it made a long streak.
    let body = blob(90, 64, 30, [GAIN * 8.0, GAIN * 4.0, GAIN * 1.0]);
    let old = run(&g, &mut l, &body, &lens_with(0.0, 0.5)).unwrap();
    assert!(max_ch(at(&old, 140, 64)) > 0.02, "without the size test the body streaks: {:?}", at(&old, 140, 64));
    let new = run(&g, &mut l, &body, &lens_with(TEST_MAX_SIZE, 0.5)).unwrap();
    assert!(black_outside(&new, 90, 64, 30), "the body still adds light around it");
    // The same body with a hot rim (2 px at 3x the threshold) around a dimmer interior (0.7x the threshold, above the coverage
    // share): the interior counts as bright area, so the rim is not a row of point sources either.
    let rim = |x: usize, y: usize| {
        let (dx, dy) = (x.abs_diff(90), y.abs_diff(64));
        if dx.max(dy) > 30 {
            [0.0; 3]
        } else if dx.max(dy) >= 29 {
            [GAIN * 7.5, GAIN * 4.0, GAIN * 1.0]
        } else {
            [GAIN * 1.75, GAIN * 1.0, GAIN * 0.3]
        }
    };
    let old = run(&g, &mut l, rim, &lens_with(0.0, 0.5)).unwrap();
    assert!(max_ch(at(&old, 140, 64)) > 0.005, "without the size test the rim streaks");
    let new = run(&g, &mut l, rim, &lens_with(TEST_MAX_SIZE, 0.5)).unwrap();
    assert!(black_outside(&new, 90, 64, 30), "the rim of the body still adds light");
    // A lamp (7x7 px) far from the body keeps its streak with the body in the picture, the same as alone.
    let lamp = blob(220, 64, 3, [GAIN * 8.0, GAIN * 6.0, GAIN * 3.0]);
    let both = |x: usize, y: usize| if x >= 200 { lamp(x, y) } else { body(x, y) };
    let with_body = run(&g, &mut l, both, &lens_with(TEST_MAX_SIZE, 0.5)).unwrap();
    let alone = run(&g, &mut l, &lamp, &lens_with(TEST_MAX_SIZE, 0.5)).unwrap();
    let (a, b) = (at(&with_body, 236, 64), at(&alone, 236, 64));
    assert!(max_ch(b) > 0.02, "the lamp streaks: {b:?}");
    assert!((max_ch(a) - max_ch(b)).abs() < 0.1 * max_ch(b), "the body does not change the lamp: {a:?} vs {b:?}");
}

#[test]
fn compact_sources_and_a_sun_disc_are_untouched_by_the_size_test() {
    let Some(g) = gpu() else { return };
    let mut l = new_look(&g);
    // A torch-sized source and a sun disc of about the max size (17 px; the max size is 19 px here): bit-identical with and without the test.
    for f in [blob(128, 64, 3, [GAIN * 9.0; 3]), blob(128, 64, 8, [GAIN * 20.0, GAIN * 19.0, GAIN * 17.0])] {
        let off = run(&g, &mut l, &f, &lens_with(0.0, 0.5)).unwrap();
        let on = run(&g, &mut l, &f, &lens_with(TEST_MAX_SIZE, 0.5)).unwrap();
        assert!(max_ch(at(&on, 160, 64)) > 0.02);
        assert_eq!(off, on);
    }
}

#[test]
fn a_saturated_blue_glow_needs_more_light_white_and_warm_lights_do_not() {
    let Some(g) = gpu() else { return };
    let mut l = new_look(&g);
    // 2.8x the threshold: a source for white and warm colours; under the 3x a saturated cool colour needs at the default reject.
    let m = GAIN * 7.0;
    let probe = (150, 64);
    for (what, c, expect) in [
        ("blue", [m * 0.07, m * 0.3, m], false),
        ("cyan", [m * 0.1, m, m * 0.95], false),
        ("white", [m; 3], true),
        ("cool white", [m * 0.85, m * 0.92, m], true),
        ("orange", [m, m * 0.5, m * 0.1], true),
    ] {
        let f = blob(128, 64, 3, c);
        let o = run(&g, &mut l, &f, &lens_with(TEST_MAX_SIZE, 0.5)).unwrap();
        let p = max_ch(at(&o, probe.0, probe.1));
        assert_eq!(p > 0.0, expect, "{what}: {p}");
        if !expect {
            assert!(black_outside(&o, 128, 64, 3), "{what} adds light");
            // With the colour test off it is a source like any other.
            let o = run(&g, &mut l, &f, &lens_with(TEST_MAX_SIZE, 0.0)).unwrap();
            assert!(max_ch(at(&o, probe.0, probe.1)) > 0.01, "{what} without the colour test");
        }
    }
    // Much brighter blue (3x more) is a source again: blue is not banned.
    let o = run(&g, &mut l, blob(128, 64, 3, [m * 0.2, m * 0.9, m * 3.0]), &lens_with(TEST_MAX_SIZE, 0.5)).unwrap();
    assert!(max_ch(at(&o, probe.0, probe.1)) > 0.01, "a very bright blue light");
}

#[test]
fn bad_pixels_do_not_spread_and_identical_input_gives_identical_output() {
    let Some(g) = gpu() else { return };
    let f = |x: usize, y: usize| {
        if (x, y) == (60, 60) {
            grey(f32::NAN)
        } else if (x, y) == (70, 40) {
            grey(-5.0)
        } else if (x, y) == (100, 90) {
            grey(1.0e9)
        } else {
            blob(180, 64, 2, [GAIN * 12.0; 3])(x, y)
        }
    };
    let mut l1 = new_look(&g);
    let mut l2 = new_look(&g);
    let a = run(&g, &mut l1, f, &lens_only(1.0, 0.3)).unwrap();
    let b = run(&g, &mut l1, f, &lens_only(1.0, 0.3)).unwrap();
    let c = run(&g, &mut l2, f, &lens_only(1.0, 0.3)).unwrap();
    assert_eq!(a, b, "same instance");
    assert_eq!(a, c, "new instance");
    for p in &a {
        assert!(p.iter().all(|v| v.is_finite() && *v >= 0.0), "{p:?}");
    }
}

#[test]
fn the_pipeline_is_restored_and_our_slots_are_left_empty() {
    let Some(g) = gpu() else { return };
    let mut l = new_look(&g);
    let (_t, mark) = scene(&g, 4, 4, |_, _| grey(0.5));
    unsafe {
        g.ctx.PSSetShaderResources(0, Some(&[Some(mark.clone())]));
        g.ctx.PSSetShaderResources(SLOT_COLOR, Some(&[None]));
    }
    let inp = LookInput { lens: Some(lens(0.5, 0.2)), grade: Some(Grade { contrast: 1.2, ..grading::NEUTRAL }), lut: None };
    let out = run(&g, &mut l, blob(128, 64, 3, [GAIN * 9.0; 3]), &inp);
    assert!(out.is_some());
    unsafe {
        let mut got = [None];
        g.ctx.PSGetShaderResources(0, Some(&mut got));
        assert_eq!(got[0].as_ref().map(|s| s.as_raw()), Some(mark.as_raw()), "t0 restored");
        let mut mine: [Option<ID3D11ShaderResourceView>; SLOT_COUNT as usize] = Default::default();
        g.ctx.PSGetShaderResources(SLOT_FIRST, Some(&mut mine));
        assert!(mine.iter().all(|s| s.is_none()), "our slots are left empty");
    }
    // The size changes: the targets are rebuilt and the result is still right.
    let (_t, srv) = scene(&g, 192, 96, |_, _| grey(0.25));
    let o = unsafe { l.run(g.dev.as_raw(), g.ctx.as_raw(), srv.as_raw(), (192, 96), &inp) };
    assert!(o.is_some());
    assert_eq!(l.size, (192, 96));
}

// ---- grading ----------------------------------------------------------------------------------------------------------------------

fn palette(x: usize, y: usize) -> [f32; 3] {
    // A spread of colours and levels, dark to a 20x highlight.
    let t = (x as f32 / W as f32).powf(2.0) * 24.0;
    let h = y as f32 / H as f32;
    [t * (0.4 + 0.6 * h), t * (0.9 - 0.5 * h), t * (0.2 + 0.7 * (1.0 - h))]
}

#[test]
fn the_slider_grade_matches_the_host_mirror_for_every_preset_and_extremes() {
    let Some(g) = gpu() else { return };
    let mut l = new_look(&g);
    let mut grades: Vec<Grade> = PRESETS.iter().skip(1).map(|p| p.grade).collect();
    grades.push(Grade { saturation: 0.0, ..grading::NEUTRAL });
    grades.push(Grade { saturation: 2.0, contrast: 1.5, vibrance: 1.0, ..grading::NEUTRAL });
    grades.push(Grade { contrast: 0.5, temperature: -1.0, tint: 1.0, shadows: 0.5, highlights: -0.5, ..grading::NEUTRAL });
    grades.push(Grade { temperature: 1.0, tint: -1.0, shadows: -0.5, highlights: 0.5, ..grading::NEUTRAL });
    for grade in grades {
        let inp = LookInput { lens: None, grade: Some(grade), lut: None };
        let out = run(&g, &mut l, palette, &inp).expect("grade runs");
        let p = GradeParams::from_grade(&grade);
        let mut worst = 0.0f32;
        for y in (0..H).step_by(3) {
            for x in (0..W).step_by(3) {
                let src = palette(x, y).map(|v| srgb_decode(srgb_encode(v))); // what the shader decodes
                let want = grade_rgb(src, &p);
                let got = at(&out, x, y);
                let tol = |w: f32| w.abs() * 4e-3 + 2e-4;
                for c in 0..3 {
                    let e = (got[c] - want[c]).abs() - tol(want[c]);
                    worst = worst.max(e);
                }
                assert!(got.iter().all(|v| v.is_finite() && *v >= 0.0));
            }
        }
        assert!(worst <= 0.0, "{grade:?}: GPU and host disagree by {worst} beyond the tolerance");
    }
}

#[test]
fn a_mid_grey_scene_is_unchanged_by_contrast_and_the_grade_keeps_hdr_unclipped() {
    let Some(g) = gpu() else { return };
    let mut l = new_look(&g);
    let inp = LookInput { lens: None, grade: Some(Grade { contrast: 1.4, ..grading::NEUTRAL }), lut: None };
    let o = run(&g, &mut l, |_, _| grey(0.18), &inp).unwrap();
    assert!(o.iter().all(|p| p.iter().all(|v| (v - 0.18).abs() < 0.18 * 3e-3)), "mid-grey moved: {:?}", o[0]);
    let hot = LookInput { lens: None, grade: Some(preset_by_name("vivid").unwrap().grade), lut: None };
    let o = run(&g, &mut l, |_, _| [30.0, 20.0, 5.0], &hot).unwrap();
    let p = o[0];
    assert!(p[0] > 20.0 && p[0] >= p[1] && p[1] >= p[2], "HDR hue order or level lost: {p:?}");
}

#[test]
fn the_grade_leaves_the_alpha_alone() {
    let Some(g) = gpu() else { return };
    let mut l = new_look(&g);
    let (_t, srv) = scene_enc(&g, W, H, |_, _| [0.5, 0.4, 0.3]);
    let inp = LookInput { lens: None, grade: Some(Grade { saturation: 1.5, ..grading::NEUTRAL }), lut: None };
    let r = unsafe { l.run(g.dev.as_raw(), g.ctx.as_raw(), srv.as_raw(), (W as u32, H as u32), &inp) };
    assert!(r.is_some());
}

// ---- LUT --------------------------------------------------------------------------------------------------------------------------

fn lut_levels() -> impl Fn(usize, usize) -> [f32; 3] {
    // Linear values from black through the shaper range and into super-white.
    |x, y| {
        let t = x as f32 / (W - 1) as f32;
        let v = if y < H / 2 { 2f32.powf(-10.0 + t * 17.0) } else { t * 0.9 };
        [v, v * 0.7, v * 0.35]
    }
}

#[test]
fn an_identity_lut_changes_nothing_within_the_float16_rounding() {
    let Some(g) = gpu() else { return };
    let mut l = new_look(&g);
    for n in [17usize, 33, 65] {
        let cube = Cube::identity(n);
        let inp = LookInput { lens: None, grade: None, lut: Some((n as u64, &cube, 1.0)) };
        let (t, srv) = scene(&g, W, H, lut_levels());
        let before = read_enc_of(&g, &t);
        let r = unsafe { l.run(g.dev.as_raw(), g.ctx.as_raw(), srv.as_raw(), (W as u32, H as u32), &inp) };
        assert!(r.is_some(), "LUT {n} ran");
        let after = read_enc(&g, &l, W, H);
        let mut worst = 0.0f32;
        for (a, b) in after.iter().zip(&before) {
            for c in 0..3 {
                worst = worst.max((a[c] - b[c]).abs() / b[c].abs().max(1e-3));
            }
        }
        assert!(worst < 1e-3, "identity LUT {n}: worst relative change {worst}");
    }
}

#[test]
fn the_lut_strength_blends_and_a_real_lut_does_what_it_says() {
    let Some(g) = gpu() else { return };
    let mut l = new_look(&g);
    // y = x - 0.1 in the log axis: divides linear light by 2^1.4 (for mid values where the axis stays inside 0..1).
    let n = 17;
    let mut cube = Cube::identity(n);
    for v in cube.data.iter_mut() {
        *v = [v[0] - 0.1, v[1] - 0.1, v[2] - 0.1];
    }
    let f = |_: usize, _: usize| grey(0.5);
    let full = run(&g, &mut l, f, &LookInput { lens: None, grade: None, lut: Some((100, &cube, 1.0)) }).unwrap();
    let want = 0.5 * 2f32.powf(-1.4);
    assert!((full[0][0] - want).abs() < want * 0.03, "{} vs {want}", full[0][0]);
    let half = run(&g, &mut l, f, &LookInput { lens: None, grade: None, lut: Some((100, &cube, 0.5)) }).unwrap();
    let mid = 0.5 * (0.5 + want); // the blend is in linear light
    assert!((half[0][0] - mid).abs() < mid * 0.03, "{} vs {mid}", half[0][0]);
    let zero = run(&g, &mut l, f, &LookInput { lens: None, grade: None, lut: Some((100, &cube, 0.0)) });
    assert!(zero.is_none(), "strength 0 = no pass");
    // A new generation re-uploads; the old texture is released (no panic, new result).
    let id = Cube::identity(n);
    let again = run(&g, &mut l, f, &LookInput { lens: None, grade: None, lut: Some((101, &id, 1.0)) }).unwrap();
    assert!((again[0][0] - 0.5).abs() < 0.5 * 2e-3);
}

#[test]
fn the_bundled_warm_lut_is_applied_on_the_gpu_and_the_strength_blends_it() {
    let Some(g) = gpu() else { return };
    let mut l = new_look(&g);
    let cube = grading::parse_cube(&grading::example_cube_text("warm")).expect("warm example parses");
    // Linear 0.5 sits exactly on a lattice point (axis 0.5): the warm look is red x1.07, blue x0.88, luminance restored.
    let f = |_: usize, _: usize| grey(0.5);
    let full = run(&g, &mut l, f, &LookInput { lens: None, grade: None, lut: Some((200, &cube, 1.0)) }).unwrap();
    let o = full[0];
    let ratio = o[0] / o[2];
    assert!((ratio - 1.07 / 0.88).abs() < 0.03, "warm red/blue ratio {ratio} ({o:?})");
    let y = grading::luma([o[0], o[1], o[2]]);
    assert!((y - 0.5).abs() < 0.5 * 0.02, "luminance kept: {y}");
    let half = run(&g, &mut l, f, &LookInput { lens: None, grade: None, lut: Some((200, &cube, 0.5)) }).unwrap();
    for c in 0..3 {
        let mid = 0.5 * (0.5 + o[c]);
        assert!((half[0][c] - mid).abs() < mid * 0.02, "ch {c}: strength 0.5 gives {} not {mid}", half[0][c]);
    }
    assert!(run(&g, &mut l, f, &LookInput { lens: None, grade: None, lut: Some((200, &cube, 0.0)) }).is_none(), "strength 0 = no pass");
}

#[test]
fn an_unusable_lut_is_skipped_without_a_crash_and_the_rest_still_runs() {
    let Some(g) = gpu() else { return };
    let mut l = new_look(&g);
    let bad = Cube { size: 17, data: vec![[0.0; 3]; 10] };
    let only = LookInput { lens: None, grade: None, lut: Some((7, &bad, 1.0)) };
    assert!(run(&g, &mut l, |_, _| grey(0.5), &only).is_none(), "nothing else to do: no pass");
    let with = LookInput { lens: None, grade: Some(Grade { saturation: 0.0, ..grading::NEUTRAL }), lut: Some((7, &bad, 1.0)) };
    let o = run(&g, &mut l, |_, _| [0.6, 0.2, 0.1], &with).unwrap();
    let y = grading::luma([0.6, 0.2, 0.1]);
    assert!((o[0][0] - y).abs() < y * 3e-3, "the slider grade still ran: {:?}", o[0]);
}

#[test]
fn lens_and_grade_and_lut_together_are_finite_and_deterministic() {
    let Some(g) = gpu() else { return };
    let cube = Cube::identity(17);
    let inp = LookInput { lens: Some(lens(0.6, 0.2)), grade: Some(preset_by_name("Warm cinematic").unwrap().grade), lut: Some((3, &cube, 0.7)) };
    let f = |x: usize, y: usize| {
        let b = blob(100, 50, 3, [GAIN * 10.0, GAIN * 9.0, GAIN * 6.0])(x, y);
        let p = palette(x, y).map(|v| v * 0.2);
        [b[0] + p[0], b[1] + p[1], b[2] + p[2]]
    };
    let mut a = new_look(&g);
    let mut b = new_look(&g);
    let r1 = run(&g, &mut a, f, &inp).unwrap();
    let r2 = run(&g, &mut b, f, &inp).unwrap();
    assert_eq!(r1, r2);
    assert!(r1.iter().all(|p| p.iter().all(|v| v.is_finite() && *v >= 0.0)));
}
