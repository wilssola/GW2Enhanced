//! WARP tests of the exposure passes (`Exposure::run`) and of the exposure / local tone code of the light-adjust substitute.
//! They need no game exe: the scene colour and the depth are synthetic, the light-adjust substitute is the real HLSL.
//!
//! The state texture is read back from the GPU (`Exposure::read_state`): x = EV applied, y = measured log2 luminance,
//! z = EV the scene asks for, w = initialised. The light-adjust renders use a Y-identity remap (t1 = 0,0,0,1) so the
//! output is `sqrt(linear light) + dither`, and the dither is the same with and without the effect (same pixel position):
//! differences against a reference render are exact.

use std::ffi::c_void;

use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::{Interface, PCSTR};

use super::*;
use crate::hdr_shaders::{LIGHT_ADJUST_ENTRY, LIGHT_ADJUST_HLSL};

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
    static B: std::sync::OnceLock<Blobs> = std::sync::OnceLock::new();
    B.get_or_init(|| compile_blobs().unwrap_or_else(|e| panic!("exposure.hlsl does not compile: {e}")))
}

/// Sets the settings of a test (and returns the previous ones so that they can be put back).
struct Settings([f32; 6]);

impl Settings {
    fn set(strength: f32, target: f32, range: f32, up: f32, down: f32, local: f32) -> Settings {
        let keep = Settings([self::strength(), self::target(), self::range(), adapt_up(), adapt_down(), local_strength()]);
        set_strength(strength);
        set_target(target);
        set_range(range);
        set_adapt_up(up);
        set_adapt_down(down);
        set_local_strength(local);
        keep
    }
}

impl Drop for Settings {
    fn drop(&mut self) {
        set_strength(self.0[0]);
        set_target(self.0[1]);
        set_range(self.0[2]);
        set_adapt_up(self.0[3]);
        set_adapt_down(self.0[4]);
        set_local_strength(self.0[5]);
    }
}

fn tex(g: &Gpu, n: usize, fmt: DXGI_FORMAT, data: &[u8], pitch: usize) -> ID3D11Texture2D {
    let desc = D3D11_TEXTURE2D_DESC { Width: n as u32, Height: n as u32, MipLevels: 1, ArraySize: 1, Format: fmt, SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, Usage: D3D11_USAGE_IMMUTABLE, BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32, CPUAccessFlags: 0, MiscFlags: 0 };
    let sd = D3D11_SUBRESOURCE_DATA { pSysMem: data.as_ptr() as *const c_void, SysMemPitch: pitch as u32, SysMemSlicePitch: 0 };
    let mut t = None;
    unsafe { g.dev.CreateTexture2D(&desc, Some(&sd), Some(&mut t)).expect("CreateTexture2D") };
    t.unwrap()
}

fn srv_of(g: &Gpu, t: &ID3D11Texture2D) -> ID3D11ShaderResourceView {
    let mut s = None;
    unsafe { g.dev.CreateShaderResourceView(t, None, Some(&mut s)).unwrap() };
    s.unwrap()
}

/// Scene colour (gamma ~2.0 encoded) n x n from a function of the pixel.
fn scene(g: &Gpu, n: usize, f: impl Fn(usize, usize) -> [f32; 3]) -> (Vec<[f32; 4]>, ID3D11ShaderResourceView) {
    let px: Vec<[f32; 4]> = (0..n * n).map(|i| {
        let c = f(i % n, i / n);
        [c[0], c[1], c[2], 1.0]
    }).collect();
    let bytes: Vec<u8> = px.iter().flat_map(|p| p.iter().flat_map(|c| c.to_le_bytes())).collect();
    let t = tex(g, n, DXGI_FORMAT_R32G32B32A32_FLOAT, &bytes, n * 16);
    (px, srv_of(g, &t))
}

fn depth_tex(g: &Gpu, n: usize, f: impl Fn(usize, usize) -> f32) -> ID3D11Texture2D {
    let bytes: Vec<u8> = (0..n * n).flat_map(|i| f(i % n, i / n).to_le_bytes()).collect();
    tex(g, n, DXGI_FORMAT_R32_FLOAT, &bytes, n * 4)
}

fn grey(l: f32) -> [f32; 3] {
    let c = l.sqrt();
    [c, c, c]
}

fn params(autoexp: bool, local: bool, dt: f32, reset: bool, sdr: bool) -> RunParams {
    RunParams { autoexp, local, dt, reset, sdr }
}

/// The state (x = EV applied, y = measured log2 luminance, z = EV asked for, w = initialised).
fn read(g: &Gpu, ex: &Exposure) -> [f32; 4] {
    unsafe { ex.read_state(g.dev.as_raw(), g.ctx.as_raw()).expect("read_state") }
}

fn make(g: &Gpu) -> Exposure {
    unsafe { Exposure::new(g.dev.as_raw(), blobs()).expect("Exposure::new") }
}

fn run(g: &Gpu, ex: &mut Exposure, srv: &ID3D11ShaderResourceView, n: usize, depth: Option<&ID3D11Texture2D>, p: &RunParams) {
    let ok = unsafe { ex.run(g.dev.as_raw(), g.ctx.as_raw(), srv.as_raw(), (n as u32, n as u32), depth.map(|d| d.as_raw()), p) };
    assert!(ok, "Exposure::run failed");
}

#[test]
fn exposure_hlsl_compiles_and_the_objects_are_created() {
    let _l = crate::testlock::lock();
    let b = blobs();
    assert!(b.vs.len() > 16 && b.state.len() > 16 && b.hist.len() > 16);
    let Some(g) = gpu() else { return };
    let _ex = make(&g);
}

#[test]
fn uniform_scene_converges_to_the_target_and_stays_stable() {
    let _l = crate::testlock::lock();
    let Some(g) = gpu() else { return };
    let _s = Settings::set(1.0, 0.2, 2.0, 0.6, 1.8, 0.0);
    let n = 128;
    let mut ex = make(&g);
    // A uniform scene with luminance 0.15: the scene asks for log2(0.2 / 0.15) = +0.415 EV (inside the linear part of the limiter).
    let (_, dim) = scene(&g, n, |_, _| grey(0.15));
    let want = target_ev(0.15f32.log2(), 0.2, 1.0, 2.0);
    assert!((want - 0.415).abs() < 1e-3, "{want}");
    // Start from a much brighter scene so the state has to travel.
    let (_, bright) = scene(&g, n, |_, _| grey(0.8));
    for _ in 0..30 {
        run(&g, &mut ex, &bright, n, None, &params(true, false, 1.0 / 60.0, false, false));
    }
    let s0 = read(&g, &ex);
    assert!(s0[0] < -0.3 && s0[3] > 0.5, "starts darkened: {s0:?}");
    // The state follows the reference smoothing frame by frame and ends on the target.
    let mut reference = s0[0];
    for frame in 0..900 {
        run(&g, &mut ex, &dim, n, None, &params(true, false, 1.0 / 60.0, false, false));
        reference = step_ev(reference, want, 1.0 / 60.0, 0.6, 1.8);
        if frame % 100 == 99 {
            let s = read(&g, &ex);
            assert!((s[0] - reference).abs() < 2e-3, "frame {frame}: gpu {} reference {reference}", s[0]);
        }
    }
    let end = read(&g, &ex);
    assert!((end[0] - want).abs() < 0.01, "converged EV {} want {want}", end[0]);
    assert!((end[1] - 0.15f32.log2()).abs() < 0.01, "measured log2 luminance {} vs {}", end[1], 0.15f32.log2());
    assert!((end[2] - want).abs() < 1e-3, "asked EV {}", end[2]);
    // Stable: identical input, further frames change nothing (the last creep of the exponential is gone after a while).
    for _ in 0..900 {
        run(&g, &mut ex, &dim, n, None, &params(true, false, 1.0 / 60.0, false, false));
    }
    let settled = read(&g, &ex);
    for _ in 0..120 {
        run(&g, &mut ex, &dim, n, None, &params(true, false, 1.0 / 60.0, false, false));
    }
    let later = read(&g, &ex);
    assert!((later[0] - settled[0]).abs() < 1e-5 && (later[1] - settled[1]).abs() < 1e-6, "{settled:?} -> {later:?}");
    assert!((later[0] - want).abs() < 2e-3, "{later:?}"); // the luminance pyramid is float16: log2 values to about 1e-3
    // Exposure x luminance reaches the target: the corrected log-average is the mid-grey (within the strength 1 slope).
    assert!((later[1] + later[0] - 0.2f32.log2()).abs() < 0.02);
}

#[test]
fn a_brightness_step_is_smoothed_with_the_configured_times() {
    let _l = crate::testlock::lock();
    let Some(g) = gpu() else { return };
    let n = 64;
    let (_, mid) = scene(&g, n, |_, _| grey(0.2));
    let (_, dark) = scene(&g, n, |_, _| grey(0.1));
    let (_, bright) = scene(&g, n, |_, _| grey(0.4));
    for (up, down) in [(0.6f32, 1.8f32), (0.2, 3.0)] {
        let _s = Settings::set(1.0, 0.2, 2.0, up, down, 0.0);
        let mut ex = make(&g);
        run(&g, &mut ex, &mid, n, None, &params(true, false, 1.0 / 60.0, true, false));
        assert!(read(&g, &ex)[0].abs() < 1e-3, "on target: no compensation");
        // Darker scene: the EV rises with the 'down' time constant.
        let ask_dark = target_ev(0.1f32.log2(), 0.2, 1.0, 2.0);
        let mut v = 0.0f32;
        let mut t_63 = None;
        for f in 1..=600 {
            run(&g, &mut ex, &dark, n, None, &params(true, false, 1.0 / 60.0, false, false));
            v = step_ev(v, ask_dark, 1.0 / 60.0, up, down);
            if t_63.is_none() && v >= 0.632 * ask_dark {
                t_63 = Some(f as f32 / 60.0);
            }
            if f % 60 == 0 {
                assert!((read(&g, &ex)[0] - v).abs() < 2e-3, "dark step, frame {f}");
            }
        }
        let t63 = t_63.unwrap();
        assert!((t63 - down).abs() < 0.1 * down + 0.03, "63 % of the way after {t63} s, time constant {down}");
        // Brighter scene: the EV falls with the 'up' time constant.
        let ask_bright = target_ev(0.4f32.log2(), 0.2, 1.0, 2.0);
        let (mut v, mut t_63) = (read(&g, &ex)[0], None);
        let start = v;
        for f in 1..=600 {
            run(&g, &mut ex, &bright, n, None, &params(true, false, 1.0 / 60.0, false, false));
            v = step_ev(v, ask_bright, 1.0 / 60.0, up, down);
            if t_63.is_none() && (start - v) >= 0.632 * (start - ask_bright) {
                t_63 = Some(f as f32 / 60.0);
            }
            if f % 60 == 0 {
                assert!((read(&g, &ex)[0] - v).abs() < 2e-3, "bright step, frame {f}");
            }
        }
        let t63 = t_63.unwrap();
        assert!((t63 - up).abs() < 0.1 * up + 0.03, "63 % of the way after {t63} s, time constant {up}");
    }
}

#[test]
fn a_long_gap_resets_the_smoothing_and_the_first_frame_jumps() {
    let _l = crate::testlock::lock();
    let Some(g) = gpu() else { return };
    let _s = Settings::set(1.0, 0.2, 2.0, 0.6, 1.8, 0.0);
    let n = 64;
    let (_, dark) = scene(&g, n, |_, _| grey(0.12));
    let (_, bright) = scene(&g, n, |_, _| grey(0.4));
    let mut ex = make(&g);
    // First frame: no history, the state is the target at once (not a crawl from zero).
    run(&g, &mut ex, &dark, n, None, &params(true, false, 1.0 / 60.0, false, false));
    let want = target_ev(0.12f32.log2(), 0.2, 1.0, 2.0);
    assert!((read(&g, &ex)[0] - want).abs() < 2e-3);
    // A long gap (a loading screen): reset = jump to the new target.
    let (dt, reset) = frame_dt(Some(5.0));
    run(&g, &mut ex, &bright, n, None, &params(true, false, dt, reset, false));
    assert!((read(&g, &ex)[0] - target_ev(0.4f32.log2(), 0.2, 1.0, 2.0)).abs() < 2e-3);
}

#[test]
fn the_sky_and_the_brightest_outlier_are_left_out_of_the_measurement() {
    let _l = crate::testlock::lock();
    let Some(g) = gpu() else { return };
    let _s = Settings::set(1.0, 0.2, 2.0, 0.6, 1.8, 0.0);
    let n = 128;
    // Top quarter: bright sky (luminance 4, invalid depth); the rest: ground (luminance 0.1, depth 50).
    let (_, sc) = scene(&g, n, |_, y| if y < n / 4 { grey(4.0) } else { grey(0.1) });
    let depth = depth_tex(&g, n, |_, y| if y < n / 4 { 0.0 } else { 50.0 });
    let mut ex = make(&g);
    run(&g, &mut ex, &sc, n, Some(&depth), &params(true, false, 1.0 / 60.0, true, false));
    let with = read(&g, &ex);
    assert!((with[1] - 0.1f32.log2()).abs() < 0.01, "with depth the sky is ignored: {}", with[1]);
    // Depth NaN / infinite / huge also count as sky.
    let depth_bad = depth_tex(&g, n, |x, y| if y >= n / 4 { 50.0 } else { [f32::NAN, f32::INFINITY, 1.0e35, -1.0][x % 4] });
    run(&g, &mut ex, &sc, n, Some(&depth_bad), &params(true, false, 1.0 / 60.0, true, false));
    assert!((read(&g, &ex)[1] - 0.1f32.log2()).abs() < 0.01, "invalid depth = sky");
    // Without depth the sky counts (the mean moves towards it), so the exclusion is really what made the difference.
    run(&g, &mut ex, &sc, n, None, &params(true, false, 1.0 / 60.0, true, false));
    let without = read(&g, &ex);
    assert!(without[1] > with[1] + 0.5, "without depth: {} with: {}", without[1], with[1]);
    // The brightest 0.1 %: a lamp that fills less of the picture than that does not move the measurement.
    let n2 = 512;
    let (_, lamp) = scene(&g, n2, |x, y| if (500..512).contains(&x) && (500..512).contains(&y) { grey(50.0) } else { grey(0.1) });
    let mut ex2 = make(&g);
    run(&g, &mut ex2, &lamp, n2, None, &params(true, false, 1.0 / 60.0, true, false));
    let m = read(&g, &ex2);
    assert!((m[1] - 0.1f32.log2()).abs() < 0.003, "the lamp is rejected: {} vs {}", m[1], 0.1f32.log2());
    // A big bright area (a quarter of the picture) is NOT an outlier.
    let (_, wall) = scene(&g, n, |x, _| if x < n / 4 { grey(1.0) } else { grey(0.1) });
    run(&g, &mut ex, &wall, n, None, &params(true, false, 1.0 / 60.0, true, false));
    assert!(read(&g, &ex)[1] > 0.1f32.log2() + 0.5);
    // All sky: nothing to measure, the state holds (first frame: 0 EV).
    let all_sky = depth_tex(&g, n, |_, _| 0.0);
    let mut ex3 = make(&g);
    run(&g, &mut ex3, &sc, n, Some(&all_sky), &params(true, false, 1.0 / 60.0, true, false));
    let s = read(&g, &ex3);
    assert!(s[0] == 0.0 && s[3] > 0.5, "{s:?}");
}

#[test]
fn the_centre_of_the_picture_weighs_more_than_the_border() {
    let _l = crate::testlock::lock();
    let Some(g) = gpu() else { return };
    let _s = Settings::set(1.0, 0.2, 2.0, 0.6, 1.8, 0.0);
    let n = 128;
    // The central square with half the area of the picture.
    let half = (0.3535 * n as f32) as isize;
    let inside = |x: usize, y: usize| (x as isize - (n / 2) as isize).abs() < half && (y as isize - (n / 2) as isize).abs() < half;
    let (_, bright_centre) = scene(&g, n, |x, y| if inside(x, y) { grey(0.4) } else { grey(0.05) });
    let (_, bright_border) = scene(&g, n, |x, y| if inside(x, y) { grey(0.05) } else { grey(0.4) });
    let mut ex = make(&g);
    run(&g, &mut ex, &bright_centre, n, None, &params(true, false, 1.0 / 60.0, true, false));
    let a = read(&g, &ex)[1];
    run(&g, &mut ex, &bright_border, n, None, &params(true, false, 1.0 / 60.0, true, false));
    let b = read(&g, &ex)[1];
    assert!(a > b + 0.2, "the same areas, the bright one in the centre measures brighter: {a} vs {b}");
}

#[test]
fn the_passes_never_produce_nan_or_huge_values_and_restore_the_pipeline() {
    let _l = crate::testlock::lock();
    let Some(g) = gpu() else { return };
    let _s = Settings::set(1.0, 0.2, 2.0, 0.6, 1.8, 1.0);
    let n = 64;
    let junk = [f32::NAN, f32::INFINITY, -5.0, 1.0e30, 0.0, 0.3, f32::NEG_INFINITY, 7.0];
    let (_, sc) = scene(&g, n, |x, y| [junk[(x + y) % 8], junk[(x * 3 + y) % 8], junk[(x + 5 * y) % 8]]);
    let mut ex = make(&g);
    // A marker pipeline state that must come back unchanged.
    let (_, marker) = scene(&g, n, |_, _| [0.5, 0.5, 0.5]);
    unsafe {
        g.ctx.PSSetShaderResources(0, Some(&[Some(marker.clone())]));
        g.ctx.RSSetViewports(Some(&[D3D11_VIEWPORT { TopLeftX: 0.0, TopLeftY: 0.0, Width: 32.0, Height: 32.0, MinDepth: 0.0, MaxDepth: 1.0 }]));
    }
    for _ in 0..10 {
        run(&g, &mut ex, &sc, n, None, &params(true, true, 1.0 / 60.0, false, false));
        let s = read(&g, &ex);
        assert!(s.iter().all(|v| v.is_finite()), "{s:?}");
        assert!(s[0].abs() <= 2.0, "EV bounded by the range: {s:?}");
    }
    unsafe {
        let mut t0 = [None];
        g.ctx.PSGetShaderResources(0, Some(&mut t0));
        assert_eq!(t0[0].as_ref().map(|s| s.as_raw()), Some(marker.as_raw()), "t0 restored");
        let mut extra = [None, None, None];
        g.ctx.PSGetShaderResources(10, Some(&mut extra));
        assert!(extra.iter().all(|s| s.is_none()), "t10..t12 left unbound");
        let mut vp = [D3D11_VIEWPORT::default()];
        let mut nvp = 1u32;
        g.ctx.RSGetViewports(&mut nvp, Some(vp.as_mut_ptr()));
        assert_eq!((vp[0].Width, vp[0].Height), (32.0, 32.0), "viewport restored");
        g.ctx.PSSetShaderResources(0, Some(&[None]));
    }
    // The base layer is finite too (read through a copy of its texture).
    let base = base_values(&g, &ex);
    assert!(base.iter().all(|v| v.iter().all(|c| c.is_finite())), "base layer finite");
}

/// The base layer (B2) as RGBA, read back through a staging copy.
fn base_values(g: &Gpu, ex: &Exposure) -> Vec<[f32; 4]> {
    unsafe {
        let tex = ID3D11Texture2D::from_raw_borrowed(&ex.lv[4].tex).unwrap().clone();
        let mut d = D3D11_TEXTURE2D_DESC::default();
        tex.GetDesc(&mut d);
        let sd = D3D11_TEXTURE2D_DESC { Usage: D3D11_USAGE_STAGING, BindFlags: 0, CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32, MiscFlags: 0, ..d };
        let mut st = None;
        g.dev.CreateTexture2D(&sd, None, Some(&mut st)).unwrap();
        let st = st.unwrap();
        g.ctx.CopyResource(&st, &tex);
        let mut m = D3D11_MAPPED_SUBRESOURCE::default();
        g.ctx.Map(&st, 0, D3D11_MAP_READ, 0, Some(&mut m)).unwrap();
        let mut out = Vec::new();
        for y in 0..d.Height as usize {
            let row = std::slice::from_raw_parts((m.pData as *const u8).add(y * m.RowPitch as usize) as *const u16, d.Width as usize * 4);
            for x in 0..d.Width as usize {
                out.push([crate::debugshot::f16_to_f32(row[x * 4]), crate::debugshot::f16_to_f32(row[x * 4 + 1]), crate::debugshot::f16_to_f32(row[x * 4 + 2]), crate::debugshot::f16_to_f32(row[x * 4 + 3])]);
            }
        }
        g.ctx.Unmap(&st, 0);
        out
    }
}

// ---- the light-adjust substitute with the exposure inputs -------------------------------------------------------------

fn compile_vs(src: &str) -> Vec<u8> {
    let mut code = None;
    let mut errs = None;
    let r = unsafe { D3DCompile(src.as_ptr() as *const c_void, src.len(), PCSTR::null(), None, None, PCSTR(c"main".as_ptr() as *const u8), PCSTR(c"vs_5_0".as_ptr() as *const u8), 0, 0, &mut code, Some(&mut errs)) };
    assert!(r.is_ok(), "vs compile failed");
    let b: ID3DBlob = code.unwrap();
    unsafe { std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize()).to_vec() }
}

const VS_LIGHT: &str = "struct O { float4 pos : SV_POSITION; float4 uv : TEXCOORD0; };
O main(uint id : SV_VertexID) { float2 p = float2((id << 1) & 2, id & 2); O o;
  o.pos = float4(p * float2(2, -2) + float2(-1, 1), 0, 1); o.uv = float4(p, 0, 0); return o; }";

/// What the exposure inputs of a light-adjust render are.
#[derive(Clone)]
struct Inputs {
    /// The EV of a 1x1 state texture bound as t5 (None = unbound).
    ev: Option<f32>,
    /// The base layer bound as t21 (None = unbound).
    base: Option<ID3D11ShaderResourceView>,
    /// b9 (None = unbound).
    b9: Option<[f32; 4]>,
    sdr: bool,
}

impl Inputs {
    fn off() -> Inputs {
        Inputs { ev: None, base: None, b9: None, sdr: false }
    }
}

fn f4_bytes(v: &[[f32; 4]]) -> Vec<u8> {
    v.iter().flat_map(|p| p.iter().flat_map(|c| c.to_le_bytes())).collect()
}

/// Renders the real light-adjust substitute over an n x n target and returns the RGBA of every pixel.
fn light(g: &Gpu, n: usize, t0: &[[f32; 4]], inp: &Inputs) -> Vec<[f32; 4]> {
    unsafe {
        let mut vs = None;
        g.dev.CreateVertexShader(&compile_vs(VS_LIGHT), None, Some(&mut vs)).unwrap();
        let mut ps = None;
        g.dev.CreatePixelShader(&crate::shaders::compile_ps(LIGHT_ADJUST_HLSL, LIGHT_ADJUST_ENTRY).unwrap_or_else(|e| panic!("{e}")), None, Some(&mut ps)).unwrap();
        let t0t = tex(g, n, DXGI_FORMAT_R32G32B32A32_FLOAT, &f4_bytes(t0), n * 16);
        let t1 = {
            let b = f4_bytes(&[[0.0, 0.0, 0.0, 1.0]]);
            let desc = D3D11_TEXTURE2D_DESC { Width: 1, Height: 1, MipLevels: 1, ArraySize: 1, Format: DXGI_FORMAT_R32G32B32A32_FLOAT, SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, Usage: D3D11_USAGE_IMMUTABLE, BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32, CPUAccessFlags: 0, MiscFlags: 0 };
            let sd = D3D11_SUBRESOURCE_DATA { pSysMem: b.as_ptr() as *const c_void, SysMemPitch: 16, SysMemSlicePitch: 0 };
            let mut t = None;
            g.dev.CreateTexture2D(&desc, Some(&sd), Some(&mut t)).unwrap();
            t.unwrap()
        };
        let sdesc = D3D11_SAMPLER_DESC { Filter: D3D11_FILTER_MIN_MAG_MIP_POINT, AddressU: D3D11_TEXTURE_ADDRESS_CLAMP, AddressV: D3D11_TEXTURE_ADDRESS_CLAMP, AddressW: D3D11_TEXTURE_ADDRESS_CLAMP, MaxAnisotropy: 1, ComparisonFunc: D3D11_COMPARISON_NEVER, MaxLOD: f32::MAX, ..Default::default() };
        let mut smp = None;
        g.dev.CreateSamplerState(&sdesc, Some(&mut smp)).unwrap();
        let cbuf = |v: &[[f32; 4]]| {
            let bytes = f4_bytes(v);
            let bd = D3D11_BUFFER_DESC { ByteWidth: bytes.len() as u32, Usage: D3D11_USAGE_IMMUTABLE, BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32, ..Default::default() };
            let sd = D3D11_SUBRESOURCE_DATA { pSysMem: bytes.as_ptr() as *const c_void, SysMemPitch: 0, SysMemSlicePitch: 0 };
            let mut b = None;
            g.dev.CreateBuffer(&bd, Some(&sd), Some(&mut b)).unwrap();
            b.unwrap()
        };
        let rt_desc = D3D11_TEXTURE2D_DESC { Width: n as u32, Height: n as u32, MipLevels: 1, ArraySize: 1, Format: DXGI_FORMAT_R16G16B16A16_FLOAT, SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, Usage: D3D11_USAGE_DEFAULT, BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32, CPUAccessFlags: 0, MiscFlags: 0 };
        let mut rt = None;
        g.dev.CreateTexture2D(&rt_desc, None, Some(&mut rt)).unwrap();
        let rt = rt.unwrap();
        let mut rtv = None;
        g.dev.CreateRenderTargetView(&rt, None, Some(&mut rtv)).unwrap();

        let c = &g.ctx;
        c.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
        c.VSSetShader(vs.as_ref(), None);
        c.PSSetShader(ps.as_ref(), None);
        c.PSSetShaderResources(0, Some(&[Some(srv_of(g, &t0t)), Some(srv_of(g, &t1))]));
        c.PSSetSamplers(0, Some(&[smp.clone(), smp]));
        // cb0: dark curve constants (unused with the identity remap), b6: boost 1, AO 0, SDR flag, no debug.
        c.PSSetConstantBuffers(0, Some(&[Some(cbuf(&[[0.9, 0.0, 0.0, 0.0], [0.7, 0.0, 0.0, 0.0]]))]));
        c.PSSetConstantBuffers(6, Some(&[Some(cbuf(&[[1.0, 0.0, if inp.sdr { 1.0 } else { 0.0 }, 0.0], [0.0; 4]]))]));
        if let Some(ev) = inp.ev {
            let b = f4_bytes(&[[ev, 0.0, ev, 1.0]]);
            let desc = D3D11_TEXTURE2D_DESC { Width: 1, Height: 1, MipLevels: 1, ArraySize: 1, Format: DXGI_FORMAT_R32G32B32A32_FLOAT, SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, Usage: D3D11_USAGE_IMMUTABLE, BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32, CPUAccessFlags: 0, MiscFlags: 0 };
            let sd = D3D11_SUBRESOURCE_DATA { pSysMem: b.as_ptr() as *const c_void, SysMemPitch: 16, SysMemSlicePitch: 0 };
            let mut t = None;
            g.dev.CreateTexture2D(&desc, Some(&sd), Some(&mut t)).unwrap();
            c.PSSetShaderResources(STATE_SLOT, Some(&[Some(srv_of(g, &t.unwrap()))]));
        }
        if let Some(b) = &inp.base {
            c.PSSetShaderResources(BASE_SLOT, Some(&[Some(b.clone())]));
        }
        if let Some(v) = inp.b9 {
            c.PSSetConstantBuffers(CB_SLOT, Some(&[Some(cbuf(&[v, [0.0; 4]]))]));
        }
        c.OMSetRenderTargets(Some(&[rtv]), None);
        c.RSSetViewports(Some(&[D3D11_VIEWPORT { TopLeftX: 0.0, TopLeftY: 0.0, Width: n as f32, Height: n as f32, MinDepth: 0.0, MaxDepth: 1.0 }]));
        c.Draw(3, 0);
        let sd = D3D11_TEXTURE2D_DESC { Usage: D3D11_USAGE_STAGING, BindFlags: 0, CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32, ..rt_desc };
        let mut st = None;
        g.dev.CreateTexture2D(&sd, None, Some(&mut st)).unwrap();
        let st = st.unwrap();
        c.CopyResource(&st, &rt);
        let mut m = D3D11_MAPPED_SUBRESOURCE::default();
        c.Map(&st, 0, D3D11_MAP_READ, 0, Some(&mut m)).unwrap();
        let mut out = Vec::with_capacity(n * n);
        for y in 0..n {
            let row = std::slice::from_raw_parts((m.pData as *const u8).add(y * m.RowPitch as usize) as *const u16, n * 4);
            for x in 0..n {
                out.push([crate::debugshot::f16_to_f32(row[x * 4]), crate::debugshot::f16_to_f32(row[x * 4 + 1]), crate::debugshot::f16_to_f32(row[x * 4 + 2]), crate::debugshot::f16_to_f32(row[x * 4 + 3])]);
            }
        }
        c.Unmap(&st, 0);
        c.OMSetRenderTargets(None, None);
        c.PSSetShaderResources(0, Some(&[None, None]));
        c.PSSetShaderResources(STATE_SLOT, Some(&[None]));
        c.PSSetShaderResources(BASE_SLOT, Some(&[None]));
        c.PSSetConstantBuffers(CB_SLOT, Some(&[None]));
        c.PSSetConstantBuffers(6, Some(&[None]));
        out
    }
}

fn at(v: &[[f32; 4]], n: usize, x: usize, y: usize) -> [f32; 4] {
    v[y * n + x]
}

#[test]
fn strength_zero_and_unbound_inputs_are_bit_identical() {
    let _l = crate::testlock::lock();
    let Some(g) = gpu() else { return };
    let n = 64;
    let mut seed = 12345u32;
    let mut rnd = || {
        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        (seed >> 8) as f32 / 16777216.0
    };
    let px: Vec<[f32; 4]> = (0..n * n).map(|_| [rnd() * 1.6, rnd() * 1.6, rnd() * 1.6, 1.0]).collect();
    let reference = light(&g, n, &px, &Inputs::off());
    // b9 = the 'off' values (what light_cb(false, false) writes), with a state and a base layer bound anyway: nothing changes, bit for bit.
    let (_, sc) = scene(&g, n, |x, y| grey(0.1 + 0.8 * ((x + y) % 16) as f32 / 16.0));
    let mut ex = make(&g);
    let _s = Settings::set(1.0, 0.2, 2.0, 0.6, 1.8, 1.0);
    run(&g, &mut ex, &sc, n, None, &params(true, true, 1.0 / 60.0, true, false));
    let off = light(&g, n, &px, &Inputs { ev: Some(0.9), base: Some(srv_from_raw(ex.base_srv())), b9: Some(light_cb(false, false)[0]), sdr: false });
    assert_eq!(reference, off, "strength 0 / b9 off must be bit-identical to the shader without the inputs");
    // The same with only an exact zero EV and the auto exposure 'on': E = 1 is skipped, bit-identical too.
    let zero = light(&g, n, &px, &Inputs { ev: Some(0.0), base: None, b9: Some(light_cb(true, false)[0]), sdr: false });
    assert_eq!(reference, zero, "EV 0 changes nothing");
    // And the exposure really does something when on.
    let on = light(&g, n, &px, &Inputs { ev: Some(0.5), base: None, b9: Some(light_cb(true, false)[0]), sdr: false });
    assert_ne!(reference, on);
}

fn srv_from_raw(o: Obj) -> ID3D11ShaderResourceView {
    unsafe { ID3D11ShaderResourceView::from_raw_borrowed(&o).unwrap().clone() }
}

#[test]
fn the_exposure_scales_the_scene_in_the_right_direction_and_highlights_are_not_pushed_up() {
    let _l = crate::testlock::lock();
    let Some(g) = gpu() else { return };
    let n = 32;
    let lum = |l: f32| grey(l);
    let px: Vec<[f32; 4]> = (0..n * n).map(|i| { let c = lum(if i % n < n / 2 { 0.1 } else { 1.2 }); [c[0], c[1], c[2], 1.0] }).collect();
    let b9 = light_cb(true, false)[0];
    let base = light(&g, n, &px, &Inputs::off());
    let lin = |v: [f32; 4]| v[1] * v[1];
    for ev in [-0.7f32, -0.3, 0.3, 0.7, 1.0] {
        let out = light(&g, n, &px, &Inputs { ev: Some(ev), base: None, b9: Some(b9), sdr: false });
        let (lo_ref, lo) = (lin(at(&base, n, 4, 8)), lin(at(&out, n, 4, 8)));
        let (hi_ref, hi) = (lin(at(&base, n, 28, 8)), lin(at(&out, n, 28, 8)));
        let e = ev.exp2();
        if ev < 0.0 {
            assert!((lo / lo_ref - e).abs() < 0.03, "dark pixel scales by 2^ev: {} vs {e}", lo / lo_ref);
            assert!((hi / hi_ref - e).abs() < 0.03, "bright pixel scales by 2^ev too when darkening");
        } else {
            assert!(lo / lo_ref > 1.0 + 0.8 * (e - 1.0) && lo / lo_ref <= e + 0.03, "dark pixel brightened by about 2^ev: {} vs {e}", lo / lo_ref);
            assert!(hi >= hi_ref - 0.02 && hi - hi_ref <= (e - 1.0) * BRIGHTEN_KNEE + 0.05, "a highlight gains at most (E-1)*0.5: {hi_ref} -> {hi}");
        }
    }
}

#[test]
fn sdr_output_never_exceeds_the_clamp_with_the_exposure() {
    let _l = crate::testlock::lock();
    let Some(g) = gpu() else { return };
    let n = 32;
    let px: Vec<[f32; 4]> = (0..n * n).map(|i| { let v = 0.05 + 0.95 * (i % n) as f32 / (n - 1) as f32; [v, v * 0.8, v * 0.6, 1.0] }).collect();
    let b9 = light_cb(true, false)[0];
    let hdr = light(&g, n, &px, &Inputs { ev: Some(1.0), base: None, b9: Some(b9), sdr: false });
    let sdr = light(&g, n, &px, &Inputs { ev: Some(1.0), base: None, b9: Some(b9), sdr: true });
    let sdr_ref = light(&g, n, &px, &Inputs { ev: None, base: None, b9: None, sdr: true });
    let max_hdr = hdr.iter().map(|p| p[0].max(p[1]).max(p[2])).fold(0.0f32, f32::max);
    let max_sdr = sdr.iter().map(|p| p[0].max(p[1]).max(p[2])).fold(0.0f32, f32::max);
    assert!(max_hdr > 1.05, "HDR keeps the headroom: {max_hdr}");
    assert!(max_sdr <= 1.0 + 1e-3, "SDR clamps at 1: {max_sdr}");
    for (a, b) in sdr.iter().zip(&sdr_ref) {
        assert!(a.iter().all(|c| c.is_finite() && *c >= 0.0));
        // Brightening in SDR never goes above the clamp and never darkens a pixel.
        assert!(a[1] >= b[1] - 0.01);
    }
    // Darkening works in SDR too.
    let dark = light(&g, n, &px, &Inputs { ev: Some(-0.7), base: None, b9: Some(b9), sdr: true });
    assert!(dark[n / 2 * n + n - 2][1] < sdr_ref[n / 2 * n + n - 2][1] - 0.05);
}

#[test]
fn local_tone_mapping_a_step_edge_has_no_halo_a_flat_area_is_unchanged_and_the_hue_is_kept() {
    let _l = crate::testlock::lock();
    let Some(g) = gpu() else { return };
    let _s = Settings::set(0.0, 0.2, 0.7, 0.6, 1.8, 1.0);
    let n = 256;
    // Left: a dark area (linear 0.09). Right: a bright, warm area (encoded 0.95, 0.80, 0.60).
    let edge = n / 2;
    let (px, sc) = scene(&g, n, |x, _| if x < edge { [0.3, 0.3, 0.3] } else { [0.95, 0.80, 0.60] });
    let mut ex = make(&g);
    run(&g, &mut ex, &sc, n, None, &params(false, true, 1.0 / 60.0, true, false));
    let b9 = light_cb(false, true)[0];
    let inp = Inputs { ev: None, base: Some(srv_from_raw(ex.base_srv())), b9: Some(b9), sdr: false };
    let out = light(&g, n, &px, &inp);
    let refr = light(&g, n, &px, &Inputs::off());
    let k = local_strength() * LOCAL_GAIN;
    // Encoded difference (the dither cancels): the scale of the linear light is ((out - dither) / (ref - dither))^2; with the
    // dither unknown use the encoded reference value instead: ref = sqrt(lin) + d, out = sqrt(lin * s) + d  =>  out - ref = sqrt(lin) (sqrt(s) - 1).
    let scale_of = |x: usize, y: usize, ch: usize| -> f32 {
        let lin_enc = px[y * n + x][ch]; // sqrt(lin) of the input (the remap is the identity)
        let r = 1.0 + (at(&out, n, x, y)[ch] - at(&refr, n, x, y)[ch]) / lin_enc;
        r * r
    };
    // Flat dark area far from the edge: below the knee, exactly unchanged.
    for &(x, y) in &[(10usize, 10usize), (60, 200), (100, 128), (edge - 40, 30)] {
        assert!((scale_of(x, y, 1) - 1.0).abs() < 0.01, "dark flat area changed at {x},{y}: {}", scale_of(x, y, 1));
    }
    // Flat bright area far from the edge: compressed by the scale of its own luminance (the pixel's luminance is the base).
    let lum_bright = 0.2126 * 0.95f32.powi(2) + 0.7152 * 0.80f32.powi(2) + 0.0722 * 0.60f32.powi(2);
    let s_far = local_scale(lum_bright, LOCAL_KNEE, k);
    assert!(s_far < 0.95, "the bright area is really compressed: {s_far}");
    for &(x, y) in &[(edge + 80, 40), (n - 20, 128), (edge + 100, 200)] {
        let s = scale_of(x, y, 1);
        assert!((s - s_far).abs() < 0.03, "bright flat area at {x},{y}: scale {s} vs {s_far}");
    }
    // Hue: the three channels of a bright pixel are scaled by the same factor.
    let (sr, sg, sb) = (scale_of(edge + 80, 40, 0), scale_of(edge + 80, 40, 1), scale_of(edge + 80, 40, 2));
    assert!((sr - sg).abs() < 0.02 && (sg - sb).abs() < 0.03, "hue preserved: {sr} {sg} {sb}");
    // No halo: a column of pixels from 1 px to 24 px away from the edge on each side.
    for d in 1..=24usize {
        let dark = scale_of(edge - d, 100, 1);
        assert!((dark - 1.0).abs() < 0.02, "dark side {d} px from the edge is touched (halo): {dark}");
        let bright = scale_of(edge + d - 1, 100, 1);
        assert!(bright <= 1.01 && bright >= s_far - 0.05, "bright side {d} px from the edge: {bright} (far value {s_far})");
    }
    // Bounded and finite everywhere: only compression, never more than the largest scale.
    for (i, (o, r)) in out.iter().zip(&refr).enumerate() {
        assert!(o.iter().all(|c| c.is_finite() && *c >= 0.0), "pixel {i}");
        assert!(o[1] <= r[1] + 0.005, "local tone never brightens: pixel {i}");
        assert!(o[1] >= r[1] * 0.4 - 0.01, "and stays bounded: pixel {i}");
    }
    // Stable: the same input gives the same picture.
    let again = light(&g, n, &px, &inp);
    assert_eq!(out, again);
}

#[test]
fn local_tone_mapping_is_monotonic_along_a_ramp_and_keeps_local_detail() {
    let _l = crate::testlock::lock();
    let Some(g) = gpu() else { return };
    let _s = Settings::set(0.0, 0.2, 0.7, 0.6, 1.8, 1.0);
    let n = 256;
    // A smooth horizontal ramp of encoded value 0.70 .. 1.0 (linear 0.49 .. 1.0), and on top of it a fine stripe pattern of +-6 %.
    let ramp = |x: usize| 0.70 + 0.30 * x as f32 / (n - 1) as f32;
    let (px, sc) = scene(&g, n, |x, y| { let v = ramp(x) * if (y / 2) % 2 == 0 { 1.04 } else { 0.96 }; [v, v, v] });
    let mut ex = make(&g);
    run(&g, &mut ex, &sc, n, None, &params(false, true, 1.0 / 60.0, true, false));
    let inp = Inputs { ev: None, base: Some(srv_from_raw(ex.base_srv())), b9: Some(light_cb(false, true)[0]), sdr: false };
    let out = light(&g, n, &px, &inp);
    let refr = light(&g, n, &px, &Inputs::off());
    // Monotonic: the mean of a column group never decreases to the right (rows averaged: the dither and stripes cancel).
    let col_mean = |v: &[[f32; 4]], x: usize| (0..n).map(|y| v[y * n + x][1] as f64).sum::<f64>() / n as f64;
    let mut prev = 0.0;
    for x0 in (0..n - 16).step_by(16) {
        let m: f64 = (x0..x0 + 16).map(|x| col_mean(&out, x)).sum::<f64>() / 16.0;
        assert!(m >= prev - 1e-4, "ramp not monotonic at {x0}: {m} after {prev}");
        prev = m;
    }
    // Compressed overall (the bright end more than the dark end) ...
    assert!(col_mean(&out, n - 8) < col_mean(&refr, n - 8) - 0.01);
    // ... and the local detail survives: the stripe contrast (ratio of neighbouring rows) is kept.
    let ratio = |v: &[[f32; 4]], x: usize| (v[2 * n + x][1] / v[0 * n + x][1]) as f32;
    let (r_out, r_ref) = (ratio(&out, n - 30), ratio(&refr, n - 30));
    assert!(r_ref < 0.95, "the reference has the stripes: {r_ref}");
    assert!((r_out - r_ref).abs() < 0.03, "stripe contrast kept: {r_out} vs {r_ref}");
}

#[test]
fn local_tone_mapping_spares_small_lights_and_follows_the_ev() {
    let _l = crate::testlock::lock();
    let Some(g) = gpu() else { return };
    let _s = Settings::set(0.0, 0.2, 0.7, 0.6, 1.8, 1.0);
    let n = 256;
    // A dark scene with a 6 px lamp (encoded 1.0): the lamp is far smaller than a base-layer cell and is not compressed.
    let (px, sc) = scene(&g, n, |x, y| if (100..106).contains(&x) && (100..106).contains(&y) { [1.0, 1.0, 1.0] } else { [0.3, 0.3, 0.3] });
    let mut ex = make(&g);
    run(&g, &mut ex, &sc, n, None, &params(false, true, 1.0 / 60.0, true, false));
    let inp = Inputs { ev: None, base: Some(srv_from_raw(ex.base_srv())), b9: Some(light_cb(false, true)[0]), sdr: false };
    let out = light(&g, n, &px, &inp);
    let refr = light(&g, n, &px, &Inputs::off());
    let lamp = (at(&out, n, 103, 103)[1], at(&refr, n, 103, 103)[1]);
    assert!(lamp.0 > 0.97 * lamp.1, "the lamp keeps its energy: {} vs {}", lamp.0, lamp.1);
    // With a negative EV the base layer (which is measured before the exposure) is judged after it: a bright wall that the
    // exposure already darkened below the knee is not compressed further.
    let (px2, sc2) = scene(&g, n, |_, _| grey(0.8));
    run(&g, &mut ex, &sc2, n, None, &params(false, true, 1.0 / 60.0, true, false));
    let base = Some(srv_from_raw(ex.base_srv()));
    let b9 = light_cb(true, true)[0];
    let without = light(&g, n, &px2, &Inputs { ev: Some(0.0), base: base.clone(), b9: Some(light_cb(false, true)[0]), sdr: false });
    let darkened = light(&g, n, &px2, &Inputs { ev: Some(-0.7), base: base.clone(), b9: Some(b9), sdr: false });
    let exposure_only = light(&g, n, &px2, &Inputs { ev: Some(-0.7), base: None, b9: Some(b9), sdr: false });
    let (a, b, c) = (at(&without, n, 128, 128)[1], at(&darkened, n, 128, 128)[1], at(&exposure_only, n, 128, 128)[1]);
    assert!(a < 0.8f32.sqrt() - 0.02, "compressed without the exposure: {a}");
    assert!(b <= c + 0.002 && b >= c * 0.9, "exposure first, then a smaller compression: {b} vs exposure-only {c}");
}

#[test]
fn the_light_adjust_output_stays_finite_with_junk_input_and_extreme_state() {
    let _l = crate::testlock::lock();
    let Some(g) = gpu() else { return };
    let _s = Settings::set(1.0, 0.2, 2.0, 0.6, 1.8, 1.0);
    let n = 64;
    let junk = [f32::NAN, f32::INFINITY, -5.0, 1.0e30, 0.0, 0.3, f32::NEG_INFINITY, 7.0];
    let (px, sc) = scene(&g, n, |x, y| [junk[(x + y) % 8], junk[(x * 3 + y) % 8], junk[(x + 5 * y) % 8]]);
    let mut ex = make(&g);
    run(&g, &mut ex, &sc, n, None, &params(true, true, 1.0 / 60.0, true, false));
    let base = Some(srv_from_raw(ex.base_srv()));
    for ev in [f32::NAN, f32::INFINITY, -100.0, 100.0, 0.5] {
        for sdr in [false, true] {
            let out = light(&g, n, &px, &Inputs { ev: Some(ev), base: base.clone(), b9: Some(light_cb(true, true)[0]), sdr });
            for (i, o) in out.iter().enumerate() {
                assert!(o.iter().all(|c| c.is_finite() && *c >= 0.0), "ev {ev} sdr {sdr} pixel {i}: {o:?}");
                // The largest finite output stays within a sane multiple of the input (no 'huge').
                assert!(o[0].max(o[1]).max(o[2]) < 1.0e4, "huge output at pixel {i}");
            }
        }
    }
}
