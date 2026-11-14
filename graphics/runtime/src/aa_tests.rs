//! WARP tests of `aa.hlsl` and `Aa::run`. They need no game exe: the picture is synthetic.
//!
//! Harness: D3D11 WARP device and an RGBA32F "scene colour" built from a closure; `Aa::run` goes through the raw vtables
//! exactly as the draw hook does; the result is read back from the float16 output target.

use std::ffi::c_void;
use std::sync::OnceLock;

use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::Interface;

use super::*;

const N: usize = 64;

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
    B.get_or_init(|| compile_blobs().unwrap_or_else(|e| panic!("aa.hlsl does not compile: {e}")))
}

fn scene(g: &Gpu, w: usize, h: usize, f: impl Fn(usize, usize) -> [f32; 3]) -> (ID3D11Texture2D, ID3D11ShaderResourceView) {
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

fn f16_to_f32(h: u16) -> f32 {
    let (s, e, m) = ((h >> 15) as u32, ((h >> 10) & 0x1f) as i32, (h & 0x3ff) as f32);
    let v = match e {
        0 => m * 2f32.powi(-24),
        31 => if m == 0.0 { f32::INFINITY } else { f32::NAN },
        _ => (1.0 + m / 1024.0) * 2f32.powi(e - 15),
    };
    if s == 1 { -v } else { v }
}

/// RGB texels of the float16 output.
fn read_out(g: &Gpu, aa: &Aa, w: usize, h: usize) -> Vec<[f32; 3]> {
    unsafe {
        let t: ID3D11Texture2D = ID3D11Texture2D::from_raw_borrowed(&aa.out.tex).unwrap().clone();
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

/// Runs the passes on `f` and returns (the output texels, the bound state afterwards is checked by the caller).
fn run(g: &Gpu, aa: &mut Aa, f: impl Fn(usize, usize) -> [f32; 3], smaa: bool, thr: f32, sharpen: f32) -> Option<Vec<[f32; 3]>> {
    let (_t, srv) = scene(g, N, N, f);
    let r = unsafe { aa.run(g.dev.as_raw(), g.ctx.as_raw(), srv.as_raw(), (N as u32, N as u32), smaa, thr, sharpen) }?;
    assert!(!r.is_null());
    Some(read_out(g, aa, N, N))
}

fn new_aa(g: &Gpu) -> Aa {
    unsafe { Aa::new(g.dev.as_raw(), blobs()) }.expect("Aa::new")
}

fn grey(v: f32) -> [f32; 3] {
    [v, v, v]
}

fn step_v(x: usize, y: usize, lo: f32, hi: f32) -> [f32; 3] {
    let _ = y;
    grey(if x >= N / 2 { hi } else { lo })
}

#[test]
fn the_shaders_compile() {
    let b = blobs();
    assert!(!b.vs.is_empty() && !b.edges.is_empty() && !b.weights.is_empty() && !b.blend.is_empty() && !b.cas.is_empty());
}

#[test]
fn settings_clamp_and_the_pass_selection_is_exact() {
    assert_eq!((clamp_threshold(0.0), clamp_threshold(9.0), clamp_threshold(f32::NAN)), (0.02, 0.5, 0.1));
    assert_eq!((clamp_sharpen(-1.0), clamp_sharpen(9.0), clamp_sharpen(f32::NAN)), (0.0, 1.0, 0.3));
    assert_eq!(passes_for(false, 0.0), (false, false));
    assert_eq!(passes_for(true, 0.0), (true, false));
    assert_eq!(passes_for(false, 0.3), (false, true));
    assert_eq!(passes_for(true, 0.3), (true, true));
    // Mod switched off (vanilla emulation), unrecognised frames, both passes off: nothing runs.
    assert!(wanted_for(true, true, true, 0.0));
    assert!(wanted_for(true, true, false, 0.3));
    assert!(!wanted_for(false, true, true, 0.3), "vanilla emulation stays vanilla");
    assert!(!wanted_for(true, false, true, 0.3));
    assert!(!wanted_for(true, true, false, 0.0), "both off = bit-identical no-op");
    let c = cb_values(3440, 1440, 0.1, 0.3);
    assert_eq!(c[0], [3440.0, 1440.0, 1.0 / 3440.0, 1.0 / 1440.0]);
    assert_eq!(c[1], [0.1, 0.3, LOCAL_CONTRAST, 0.0]);
    assert_eq!(cb_values(0, 0, f32::NAN, 9.0)[1][..2], [0.1, 1.0]);
    assert!(!status_text().is_empty());
}

#[test]
fn both_passes_off_run_nothing() {
    let Some(g) = gpu() else { return };
    let mut aa = new_aa(&g);
    assert!(run(&g, &mut aa, |_, _| grey(0.5), false, 0.1, 0.0).is_none());
}

#[test]
fn a_flat_picture_is_returned_exactly_by_every_mode() {
    let Some(g) = gpu() else { return };
    let mut aa = new_aa(&g);
    for v in [0.0, 0.25, 1.0, 8.0] {
        let out = run(&g, &mut aa, |_, _| grey(v), true, 0.1, 0.0).unwrap();
        assert!(out.iter().all(|p| *p == grey(v)), "smaa on a flat {v} picture");
    }
    // Sharpen on a flat picture changes nothing beyond float16 rounding.
    for v in [0.0, 0.25, 0.8, 8.0] {
        let out = run(&g, &mut aa, |_, _| grey(v), false, 0.1, 1.0).unwrap();
        for p in &out {
            for c in p {
                assert!((c - v).abs() <= v * 2e-3 + 1e-6, "sharpen on flat {v}: {c}");
            }
        }
    }
}

#[test]
fn hdr_highlights_next_to_each_other_make_no_false_edges() {
    let Some(g) = gpu() else { return };
    let mut aa = new_aa(&g);
    // 8 against 16: a huge difference in linear terms, a small one on the compressed scale.
    let out = run(&g, &mut aa, |x, y| step_v(x, y, 8.0, 16.0), true, 0.1, 0.0).unwrap();
    for (i, p) in out.iter().enumerate() {
        assert_eq!(*p, step_v(i % N, i / N, 8.0, 16.0), "pixel {i} changed");
    }
}

#[test]
fn a_staircase_gets_intermediate_values_and_keeps_the_interior_and_the_energy() {
    let Some(g) = gpu() else { return };
    let mut aa = new_aa(&g);
    // A shallow slope: 1 pixel up for every 8 across (long steps, the case SMAA exists for).
    let f = |x: usize, y: usize| grey(if (y as f32) * 8.0 > x as f32 + 4.0 { 1.0 } else { 0.0 });
    let out = run(&g, &mut aa, f, true, 0.1, 0.0).unwrap();
    let mid = out.iter().filter(|p| p[0] > 0.05 && p[0] < 0.95).count();
    assert!(mid >= 20, "only {mid} blended pixels");
    for (i, p) in out.iter().enumerate() {
        let (x, y) = (i % N, i / N);
        assert!(p[0].is_finite() && p[0] >= 0.0 && p[0] <= 1.0001, "pixel {i}: {p:?}");
        assert!(p[0] == p[1] && p[1] == p[2], "grey stays grey");
        // Far from the edge nothing changes.
        let line = (x as f32 + 4.0) / 8.0;
        if (y as f32 - line).abs() > 3.0 {
            assert_eq!(*p, f(x, y), "pixel ({x},{y}) far from the edge changed");
        }
    }
    let (a, b): (f32, f32) = (out.iter().map(|p| p[0]).sum(), (0..N * N).map(|i| f(i % N, i / N)[0]).sum());
    assert!((a - b).abs() < 0.02 * b.max(1.0), "energy {a} vs {b}");
    // A vertical staircase (the same picture transposed) is handled the same way.
    let ft = |x: usize, y: usize| f(y, x);
    let outt = run(&g, &mut aa, ft, true, 0.1, 0.0).unwrap();
    let midt = outt.iter().filter(|p| p[0] > 0.05 && p[0] < 0.95).count();
    assert!(midt >= 20, "only {midt} blended pixels (vertical)");
}

#[test]
fn a_bright_edge_is_blended_without_a_bright_rim_or_nan() {
    let Some(g) = gpu() else { return };
    let mut aa = new_aa(&g);
    // A 20x highlight against black on a staircase: the blend is done in the compressed domain.
    let f = |x: usize, y: usize| grey(if (y as f32) * 8.0 > x as f32 + 4.0 { 20.0 } else { 0.0 });
    let out = run(&g, &mut aa, f, true, 0.1, 0.0).unwrap();
    let mut mids = 0;
    for p in &out {
        assert!(p[0].is_finite() && p[0] >= 0.0 && p[0] <= 20.01, "{p:?}");
        if p[0] > 0.2 && p[0] < 19.0 {
            mids += 1;
        }
    }
    assert!(mids >= 10, "{mids} blended HDR pixels");
}

#[test]
fn garbage_input_does_not_spread() {
    let Some(g) = gpu() else { return };
    let mut aa = new_aa(&g);
    // Negative, NaN and huge texels inside a stepped picture: the neighbours of a bad texel stay finite and non-negative.
    let f = |x: usize, y: usize| {
        if (x, y) == (10, 10) {
            grey(f32::NAN)
        } else if (x, y) == (20, 20) {
            grey(-5.0)
        } else if (x, y) == (30, 30) {
            grey(1.0e9)
        } else {
            grey(if (y as f32) * 8.0 > x as f32 + 4.0 { 1.0 } else { 0.0 })
        }
    };
    for (smaa, sharpen) in [(true, 0.0), (false, 0.8), (true, 0.8)] {
        let out = run(&g, &mut aa, f, smaa, 0.1, sharpen).unwrap();
        for (i, p) in out.iter().enumerate() {
            let (x, y) = (i % N, i / N);
            if (x, y) == (10, 10) {
                continue; // the bad texel itself
            }
            for c in p {
                assert!(c.is_finite() && *c >= 0.0, "({x},{y}) smaa={smaa} sharpen={sharpen}: {c}");
            }
        }
    }
}

#[test]
fn sharpen_raises_edge_contrast_by_the_strength_and_stays_hdr_safe() {
    let Some(g) = gpu() else { return };
    let mut aa = new_aa(&g);
    let at = |out: &Vec<[f32; 3]>, x: usize| out[(N / 2) * N + x][0];
    let (lo, hi) = (0.2, 0.6);
    let weak = run(&g, &mut aa, |x, y| step_v(x, y, lo, hi), false, 0.1, 0.3).unwrap();
    let strong = run(&g, &mut aa, |x, y| step_v(x, y, lo, hi), false, 0.1, 1.0).unwrap();
    let c = N / 2;
    // Next to the step the dark side gets darker and the bright side brighter; far away nothing changes.
    assert!(at(&weak, c - 1) < lo - 0.003, "dark side {}", at(&weak, c - 1));
    assert!(at(&weak, c) > hi + 0.003, "bright side {}", at(&weak, c));
    assert!(at(&strong, c - 1) < at(&weak, c - 1) && at(&strong, c) > at(&weak, c), "strength must matter");
    for x in [2, 10, c - 4, c + 3, N - 3] {
        let want = if x < c { lo } else { hi };
        assert!((at(&weak, x) - want).abs() < 1e-3, "x {x}: {}", at(&weak, x));
    }
    // HDR edge: finite, non-negative, bounded by a small overshoot.
    let hdr = run(&g, &mut aa, |x, y| step_v(x, y, 0.1, 20.0), false, 0.1, 1.0).unwrap();
    for p in &hdr {
        assert!(p[0].is_finite() && p[0] >= 0.0 && p[0] <= 20.0 * 1.4, "{p:?}");
    }
    // Strength 0 does not run the pass at all (no-op).
    assert!(run(&g, &mut aa, |x, y| step_v(x, y, lo, hi), false, 0.1, 0.0).is_none());
}

#[test]
fn smaa_and_sharpen_together_are_deterministic_and_restore_the_pipeline() {
    let Some(g) = gpu() else { return };
    let f = |x: usize, y: usize| grey(if (y as f32) * 8.0 > x as f32 + 4.0 { 0.9 } else { 0.1 });
    let mut a1 = new_aa(&g);
    let mut a2 = new_aa(&g);
    // A marker in t0 / b0 / t10 before: they must be what they were after the run.
    let (_t, mark) = scene(&g, 4, 4, |_, _| grey(0.5));
    unsafe {
        g.ctx.PSSetShaderResources(0, Some(&[Some(mark.clone())]));
        g.ctx.PSSetShaderResources(SLOT_COLOR, Some(&[None]));
    }
    let r1 = run(&g, &mut a1, f, true, 0.1, 0.3).unwrap();
    let r2 = run(&g, &mut a1, f, true, 0.1, 0.3).unwrap();
    let r3 = run(&g, &mut a2, f, true, 0.1, 0.3).unwrap();
    assert_eq!(r1, r2, "same input, same output (same instance)");
    assert_eq!(r1, r3, "same input, same output (new instance)");
    assert!(r1.iter().all(|p| p.iter().all(|c| c.is_finite() && *c >= 0.0)));
    unsafe {
        let mut got = [None];
        g.ctx.PSGetShaderResources(0, Some(&mut got));
        assert_eq!(got[0].as_ref().map(|s| s.as_raw()), Some(mark.as_raw()), "t0 restored");
        let mut mine = [None, None, None, None];
        g.ctx.PSGetShaderResources(SLOT_COLOR, Some(&mut mine));
        assert!(mine.iter().all(|s| s.is_none()), "our slots are left empty");
    }
    // The size changes: the targets are rebuilt and the result is still right.
    let (_t, srv) = scene(&g, 48, 40, |_, _| grey(0.25));
    let out = unsafe { a1.run(g.dev.as_raw(), g.ctx.as_raw(), srv.as_raw(), (48, 40), true, 0.1, 0.3) };
    assert!(out.is_some());
    assert_eq!(a1.size, (48, 40));
}
