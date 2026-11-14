//! WARP tests of the shafts shaders and of `Shafts::run`. They need no game exe: scene and depth are synthetic.
//!
//! Harness: D3D11 WARP device, an N x N RGBA32F "scene colour" (gamma-2 encoded like the engine's) and an N x N
//! R32_FLOAT depth. The world: sky (depth 0) in the top rows, geometry below, and optionally vertical columns
//! of valid depth in front of the sky. `Shafts::run` goes through the raw vtables exactly as the hook does.

use std::ffi::c_void;
use std::sync::OnceLock;

use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::Interface;

use super::*;

const N: usize = 128;
const SKY_ROWS: usize = 40;

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
    B.get_or_init(|| compile_blobs().unwrap_or_else(|e| panic!("shafts.hlsl does not compile: {e}")))
}

fn tex(g: &Gpu, w: usize, h: usize, fmt: DXGI_FORMAT, bind: D3D11_BIND_FLAG, init: Option<(&[u8], usize)>) -> ID3D11Texture2D {
    let desc = D3D11_TEXTURE2D_DESC { Width: w as u32, Height: h as u32, MipLevels: 1, ArraySize: 1, Format: fmt, SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, Usage: D3D11_USAGE_DEFAULT, BindFlags: bind.0 as u32, CPUAccessFlags: 0, MiscFlags: 0 };
    let sd = init.map(|(d, pitch)| D3D11_SUBRESOURCE_DATA { pSysMem: d.as_ptr() as *const c_void, SysMemPitch: pitch as u32, SysMemSlicePitch: 0 });
    let mut t = None;
    unsafe { g.dev.CreateTexture2D(&desc, sd.as_ref().map(|s| s as *const _), Some(&mut t)).expect("CreateTexture2D") };
    t.unwrap()
}

fn srv_of(g: &Gpu, t: &ID3D11Texture2D) -> ID3D11ShaderResourceView {
    let mut v = None;
    unsafe { g.dev.CreateShaderResourceView(t, None, Some(&mut v)).unwrap() };
    v.unwrap()
}

/// RGBA32F scene colour from a closure (encoded colour, gamma about 2.0).
fn scene_tex(g: &Gpu, w: usize, h: usize, f: impl Fn(usize, usize) -> [f32; 3]) -> ID3D11Texture2D {
    let mut v = Vec::with_capacity(w * h * 16);
    for y in 0..h {
        for x in 0..w {
            let c = f(x, y);
            for ch in [c[0], c[1], c[2], 1.0] {
                v.extend_from_slice(&ch.to_le_bytes());
            }
        }
    }
    tex(g, w, h, DXGI_FORMAT_R32G32B32A32_FLOAT, D3D11_BIND_SHADER_RESOURCE, Some((&v, w * 16)))
}

fn depth_tex(g: &Gpu, w: usize, h: usize, f: impl Fn(usize, usize) -> f32) -> ID3D11Texture2D {
    let mut v = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        for x in 0..w {
            v.extend_from_slice(&f(x, y).to_le_bytes());
        }
    }
    tex(g, w, h, DXGI_FORMAT_R32_FLOAT, D3D11_BIND_SHADER_RESOURCE, Some((&v, w * 4)))
}

fn f16_to_f32(h: u16) -> f32 {
    let (s, e, m) = ((h >> 15) as u32, ((h >> 10) & 0x1f) as i32, (h & 0x3ff) as f32);
    let v = match e {
        0 => m * 2f32.powi(-24),
        31 => f32::INFINITY,
        _ => (1.0 + m / 1024.0) * 2f32.powi(e - 15),
    };
    if s == 1 { -v } else { v }
}

/// RGBA texels of a texture (float16 or float32 per channel).
fn read(g: &Gpu, t: &ID3D11Texture2D, w: usize, h: usize, fmt: DXGI_FORMAT, half: bool) -> Vec<[f32; 4]> {
    unsafe {
        let desc = D3D11_TEXTURE2D_DESC { Width: w as u32, Height: h as u32, MipLevels: 1, ArraySize: 1, Format: fmt, SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, Usage: D3D11_USAGE_STAGING, BindFlags: 0, CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32, MiscFlags: 0 };
        let mut st = None;
        g.dev.CreateTexture2D(&desc, None, Some(&mut st)).unwrap();
        let st = st.unwrap();
        g.ctx.CopyResource(&st, t);
        let mut m = D3D11_MAPPED_SUBRESOURCE::default();
        g.ctx.Map(&st, 0, D3D11_MAP_READ, 0, Some(&mut m)).unwrap();
        let mut out = Vec::with_capacity(w * h);
        for y in 0..h {
            let row = (m.pData as *const u8).add(y * m.RowPitch as usize);
            for x in 0..w {
                let mut px = [0.0f32; 4];
                for c in 0..4 {
                    px[c] = if half { f16_to_f32(*(row as *const u16).add(x * 4 + c)) } else { *(row as *const f32).add(x * 4 + c) };
                }
                out.push(px);
            }
        }
        g.ctx.Unmap(&st, 0);
        out
    }
}

/// The finished shaft light (RGBA16F, quarter size).
fn shaft_light(g: &Gpu, sh: &Shafts) -> (Vec<[f32; 4]>, usize, usize) {
    let (qw, qh) = quarter(sh.size.0, sh.size.1);
    let t: ID3D11Texture2D = unsafe { ID3D11Texture2D::from_raw_borrowed(&sh.out.tex).unwrap().clone() };
    (read(g, &t, qw as usize, qh as usize, DXGI_FORMAT_R16G16B16A16_FLOAT, true), qw as usize, qh as usize)
}

/// The 1x1 frame depth statistics: (max valid depth, invalid fraction, samples, 0).
fn depth_stats(g: &Gpu, sh: &Shafts) -> [f32; 4] {
    let t: ID3D11Texture2D = unsafe { ID3D11Texture2D::from_raw_borrowed(&sh.dstat.tex).unwrap().clone() };
    read(g, &t, 1, 1, DXGI_FORMAT_R32G32B32A32_FLOAT, false)[0]
}

/// The current 1x1 sun state: (u, v, confidence, mean luminance) and the tint.
fn sun_state(g: &Gpu, sh: &Shafts) -> ([f32; 4], [f32; 4]) {
    let a: ID3D11Texture2D = unsafe { ID3D11Texture2D::from_raw_borrowed(&sh.sun[sh.cur][0].tex).unwrap().clone() };
    let b: ID3D11Texture2D = unsafe { ID3D11Texture2D::from_raw_borrowed(&sh.sun[sh.cur][1].tex).unwrap().clone() };
    (read(g, &a, 1, 1, DXGI_FORMAT_R32G32B32A32_FLOAT, false)[0], read(g, &b, 1, 1, DXGI_FORMAT_R32G32B32A32_FLOAT, false)[0])
}

// ---- the synthetic world -----------------------------------------------------------------------

#[derive(Clone, Copy)]
struct World {
    /// Centre of a very bright blob in the sky (pixels), if any.
    blob: Option<(f32, f32)>,
    /// Encoded colour of the sky around the blob.
    sky: f32,
    /// Vertical columns of valid depth in front of the sky and the ground (x % 32 in 4..28, gaps of 8 px at x = 60..67, 92..99, ...).
    columns: bool,
    /// All depth valid (no sky at all).
    no_sky: bool,
    /// The sky has a finite depth (5000) and the geometry spreads over 10..200: no invalid depth anywhere.
    finite_sky: bool,
}

/// A dim sky with a hot sun blob: the estimate lands on the blob.
const SUN: World = World { blob: Some((64.0, 6.0)), sky: 0.45, columns: false, no_sky: false, finite_sky: false };
/// A bright sky band at the top with the sun in it.
const BAND: World = World { blob: Some((64.0, 6.0)), sky: 0.9, columns: false, no_sky: false, finite_sky: false };

impl World {
    fn column(&self, x: usize) -> bool {
        self.columns && (4..28).contains(&(x % 32))
    }
    fn is_sky(&self, x: usize, y: usize) -> bool {
        !self.no_sky && y < SKY_ROWS && !self.column(x)
    }
    fn depth(&self, x: usize, y: usize) -> f32 {
        if self.is_sky(x, y) {
            if self.finite_sky { 5000.0 } else { 0.0 } // invalid = sky, or a finite far plane
        } else if self.column(x) {
            if self.finite_sky { 10.0 + x as f32 * 0.2 } else { 5.0 }
        } else if self.finite_sky {
            10.0 + y as f32 * 1.5 // 10..200
        } else {
            10.0
        }
    }
    fn colour(&self, x: usize, y: usize) -> [f32; 3] {
        if self.is_sky(x, y) {
            // A sky that is dim (`sky`) with a glow around the sun blob that rises to encoded 1.0, and a hot core.
            let Some((bx, by)) = self.blob else { return [self.sky, self.sky, self.sky * 0.95] };
            let d = ((x as f32 - bx).powi(2) + (y as f32 - by).powi(2)).sqrt();
            if d < 6.0 {
                return [1.6, 1.6, 1.5];
            }
            let t = 1.0 - ((d - 6.0) / 24.0).clamp(0.0, 1.0);
            let e = self.sky + (1.0 - self.sky) * t * t * (3.0 - 2.0 * t);
            [e, e, e * 0.95]
        } else if self.column(x) {
            [0.05, 0.05, 0.05]
        } else {
            [0.1, 0.1, 0.1]
        }
    }
}

struct Rig {
    g: Gpu,
    sh: Shafts,
}

impl Rig {
    fn new() -> Option<Rig> {
        let g = gpu()?;
        let sh = unsafe { Shafts::new(g.dev.as_raw(), blobs()) }.expect("Shafts::new");
        Some(Rig { g, sh })
    }

    fn run_sized(&mut self, w: usize, h: usize, world: World, length: f32, debug: bool) {
        let scene = scene_tex(&self.g, w, h, |x, y| world.colour(x, y));
        let depth = depth_tex(&self.g, w, h, |x, y| world.depth(x, y));
        let srv = srv_of(&self.g, &scene);
        let p = Params { width: w as u32, height: h as u32, length, debug };
        assert!(unsafe { self.sh.run(self.g.dev.as_raw(), self.g.ctx.as_raw(), srv.as_raw(), depth.as_raw(), &p) }, "run failed");
    }

    fn run(&mut self, world: World, length: f32) {
        self.run_sized(N, N, world, length, false);
    }

    /// Runs `n` frames of the same world (the state converges).
    fn settle(&mut self, world: World, length: f32, n: usize) {
        for _ in 0..n {
            self.run(world, length);
        }
    }
}

/// Red of the shaft light at the quarter pixel that covers the full-resolution pixel (x, y).
fn light_at(px: &[[f32; 4]], qw: usize, x: usize, y: usize) -> f32 {
    px[(y / 4) * qw + x / 4][0]
}

// ---- compile ----------------------------------------------------------------------------------------

#[test]
fn shafts_hlsl_compiles_for_all_entries() {
    let b = blobs();
    for (name, code) in [("vs", &b.vs), ("dtile", &b.dtile), ("dreduce", &b.dreduce), ("mask", &b.mask), ("reduce_a", &b.reduce_a), ("reduce_b", &b.reduce_b), ("shafts", &b.shafts)] {
        assert_eq!(&code[..4], b"DXBC", "{name}");
    }
}

// ---- behaviour --------------------------------------------------------------------------------------

#[test]
fn rays_appear_between_the_columns_and_fan_out_from_the_sun() {
    let Some(mut r) = Rig::new() else { return };
    let w = World { columns: true, ..BAND };
    r.settle(w, 1.0, 14);
    let (px, qw, _) = shaft_light(&r.g, &r.sh);
    // The sun blob is at (64, 6); the columns are x % 32 in 4..28, so x = 64 is the middle of an 8 px slit through which
    // the bright sky is seen, and x = 48 is the middle of a column.
    let gap = light_at(&px, qw, 64, 80);
    let col = light_at(&px, qw, 36, 80).max(light_at(&px, qw, 92, 80));
    let far = light_at(&px, qw, 64, 120);
    let near = light_at(&px, qw, 64, 56);
    println!("shafts columns: gap {gap:.4} column {col:.4}; below the sun in the gap: near {near:.4} far {far:.4}");
    assert!(gap > 0.01, "no rays in the gap: {gap}");
    assert!(gap > col * 1.5, "gap {gap} not brighter than the column {col}");
    assert!(near > far, "the rays must fade away from the sun: near {near}, far {far}");
    // Fan: across the picture the light peaks under the sun and falls off to both sides.
    let row: Vec<f32> = [0usize, 20, 40, 64, 88, 104, 124].iter().map(|x| light_at(&px, qw, *x + 0, 80)).collect();
    println!("shafts fan at y = 80: {row:?}");
    let at = |x| light_at(&px, qw, x, 80);
    assert!(at(64) > at(44) && at(44) > at(24), "left side does not fall off: {row:?}");
    assert!(at(64) > at(84) && at(84) > at(104), "right side does not fall off: {row:?}");
    // Never negative, never NaN.
    assert!(px.iter().all(|p| p.iter().all(|c| c.is_finite() && *c >= 0.0)));
    // The light is tinted by the sun (slightly yellow sky: red >= blue).
    let p = px[(80 / 4) * qw + 64 / 4];
    assert!(p[0] >= p[2] * 0.99, "tint {p:?}");
}

#[test]
fn ray_length_is_controlled_by_the_parameter() {
    let Some(mut r) = Rig::new() else { return };
    let w = World { columns: true, ..BAND };
    r.settle(w, 1.0, 14);
    let (long, qw, _) = shaft_light(&r.g, &r.sh);
    let mut r2 = Rig::new().unwrap();
    r2.settle(w, 0.3, 14);
    let (short, _, _) = shaft_light(&r2.g, &r2.sh);
    let (l, s) = (light_at(&long, qw, 64, 100), light_at(&short, qw, 64, 100));
    println!("shafts length 1.0: {l:.4}, 0.3: {s:.4} at 94 px below the sun");
    assert!(l > s * 2.0 + 0.001, "a longer ray must reach farther: {l} vs {s}");
}

#[test]
fn no_sky_gives_no_shafts() {
    let Some(mut r) = Rig::new() else { return };
    let w = World { no_sky: true, ..BAND };
    r.settle(w, 0.7, 12);
    let (px, _, _) = shaft_light(&r.g, &r.sh);
    let max = px.iter().flat_map(|p| p[..3].iter()).fold(0.0f32, |a, b| a.max(b.abs()));
    let (s0, _) = sun_state(&r.g, &r.sh);
    println!("shafts no sky: max {max}, confidence {}", s0[2]);
    assert!(max < 1e-4, "shafts without sky: {max}");
    assert!(s0[2] < 0.01, "confidence without sky: {}", s0[2]);
}

#[test]
fn a_sky_that_is_not_bright_gives_nothing() {
    let Some(mut r) = Rig::new() else { return };
    // Blue-grey sky, encoded 0.35 (linear 0.12): well below the emitter threshold.
    let w = World { blob: None, sky: 0.35, ..BAND };
    r.settle(w, 0.7, 12);
    let (px, _, _) = shaft_light(&r.g, &r.sh);
    let max = px.iter().flat_map(|p| p[..3].iter()).fold(0.0f32, |a, b| a.max(b.abs()));
    let (s0, _) = sun_state(&r.g, &r.sh);
    println!("shafts dim sky: max {max}, confidence {}", s0[2]);
    assert!(max < 1e-4 && s0[2] < 0.01, "dim sky made shafts: {max}, confidence {}", s0[2]);
}

#[test]
fn a_sky_that_is_everywhere_bright_has_low_confidence() {
    // Depth unusable (everything reads as sky) or a uniform overcast glow: the centroid means nothing.
    let Some(mut r) = Rig::new() else { return };
    let scene = scene_tex(&r.g, N, N, |_, _| [0.95, 0.95, 0.95]);
    let depth = depth_tex(&r.g, N, N, |_, _| 0.0);
    let srv = srv_of(&r.g, &scene);
    let p = Params { width: N as u32, height: N as u32, length: 0.7, debug: false };
    for _ in 0..14 {
        assert!(unsafe { r.sh.run(r.g.dev.as_raw(), r.g.ctx.as_raw(), srv.as_raw(), depth.as_raw(), &p) });
    }
    let (s0, _) = sun_state(&r.g, &r.sh);
    println!("shafts all-sky confidence {}", s0[2]);
    assert!(s0[2] < 0.05, "{}", s0[2]);
}

#[test]
fn the_sun_estimate_lands_on_the_blob_and_moves_smoothly() {
    let Some(mut r) = Rig::new() else { return };
    let a = World { blob: Some((96.0, 14.0)), ..SUN };
    r.settle(a, 0.7, 14);
    let (s0, s1) = sun_state(&r.g, &r.sh);
    println!("shafts sun at ({:.3}, {:.3}) conf {:.3}, tint {:?}", s0[0], s0[1], s0[2], s1);
    let (tu, tv) = (96.0 / N as f32, 14.0 / N as f32);
    assert!((s0[0] - tu).abs() < 0.04 && (s0[1] - tv).abs() < 0.04, "estimate ({}, {}) vs blob ({tu}, {tv})", s0[0], s0[1]);
    assert!(s0[2] > 0.8, "confidence {}", s0[2]);
    // The tint has unit luminance and is slightly warm (blue < red).
    let lum = 0.2126 * s1[0] + 0.7152 * s1[1] + 0.0722 * s1[2];
    assert!((lum - 1.0).abs() < 0.05 && s1[2] <= s1[0] + 1e-3, "tint {s1:?}");

    // The blob jumps to the left: the estimate glides there instead of jumping.
    let b = World { blob: Some((30.0, 14.0)), ..SUN };
    let (u0, u1) = (s0[0], 30.0 / N as f32);
    let mut prev = u0;
    let mut path = vec![u0];
    for _ in 0..30 {
        r.run(b, 0.7);
        let (s, _) = sun_state(&r.g, &r.sh);
        path.push(s[0]);
        assert!(s[0] <= prev + 1e-4, "moved the wrong way: {prev} -> {}", s[0]);
        assert!((prev - s[0]) <= 0.3 * (u0 - u1) + 1e-3, "jumped: {prev} -> {}", s[0]);
        prev = s[0];
    }
    println!("shafts glide: {:?}", path.iter().step_by(5).collect::<Vec<_>>());
    assert!((prev - u1).abs() < 0.04, "did not arrive: {prev} vs {u1}");
    assert!(path[1] - path[0] != 0.0 && path.windows(2).filter(|w| w[1] < w[0]).count() >= 8, "not a glide: {path:?}");
}

#[test]
fn confidence_fades_smoothly_when_the_sky_goes_away() {
    let Some(mut r) = Rig::new() else { return };
    r.settle(BAND, 0.7, 20);
    let (s, _) = sun_state(&r.g, &r.sh);
    let mut prev = s[2];
    assert!(prev > 0.9, "start {prev}");
    let gone = World { no_sky: true, ..BAND };
    let mut seen = vec![prev];
    for i in 0..70 {
        r.run(gone, 0.7);
        let (s, _) = sun_state(&r.g, &r.sh);
        assert!(s[2] <= prev + 1e-6, "confidence rose: {prev} -> {}", s[2]);
        assert!(prev - s[2] <= 0.12, "pop at frame {i}: {prev} -> {}", s[2]);
        prev = s[2];
        seen.push(prev);
    }
    println!("shafts fade: {:?}", seen.iter().step_by(10).collect::<Vec<_>>());
    assert!(prev < 0.005, "never faded out: {prev}");
    let (px, _, _) = shaft_light(&r.g, &r.sh);
    assert!(px.iter().flat_map(|p| p[..3].iter()).fold(0.0f32, |a, b| a.max(b.abs())) < 1e-4, "rays left after the fade");
}

#[test]
fn debug_view_draws_a_cross_at_the_sun() {
    let Some(mut r) = Rig::new() else { return };
    for _ in 0..14 {
        r.run_sized(N, N, BAND, 0.7, true);
    }
    let (px, qw, _) = shaft_light(&r.g, &r.sh);
    let (s0, _) = sun_state(&r.g, &r.sh);
    let (cx, cy) = ((s0[0] * qw as f32) as usize, (s0[1] * qw as f32) as usize);
    let p = px[cy * qw + cx];
    println!("shafts debug: cross at q ({cx}, {cy}) = {p:?}");
    assert!(p[0] >= 3.9 && p[1] < 0.1 && p[2] >= 3.9, "{p:?}");
}

#[test]
fn run_restores_pipeline_state_and_leaves_no_references() {
    let Some(mut r) = Rig::new() else { return };
    let g = &r.g;
    unsafe {
        let scene = scene_tex(g, N, N, |x, y| BAND.colour(x, y));
        let depth = depth_tex(g, N, N, |x, y| BAND.depth(x, y));
        let scene_srv = srv_of(g, &scene);
        let other = tex(g, N, N, DXGI_FORMAT_R16G16B16A16_FLOAT, D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE, None);
        let mut other_rtv = None;
        g.dev.CreateRenderTargetView(&other, None, Some(&mut other_rtv)).unwrap();
        let other_rtv = other_rtv.unwrap();
        let sampled: Vec<ID3D11ShaderResourceView> = (0..2).map(|_| srv_of(g, &tex(g, N, N, DXGI_FORMAT_R16G16B16A16_FLOAT, D3D11_BIND_SHADER_RESOURCE, None))).collect();
        let c = &g.ctx;
        let vp = D3D11_VIEWPORT { TopLeftX: 1.0, TopLeftY: 2.0, Width: 30.0, Height: 20.0, MinDepth: 0.0, MaxDepth: 1.0 };
        c.OMSetRenderTargets(Some(&[Some(other_rtv.clone())]), None);
        c.RSSetViewports(Some(&[vp]));
        c.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_POINTLIST);
        // Slot 0 holds the scene (as in the light-adjust draw), slot 1 another view; slot 9 a sentinel that must stay.
        c.PSSetShaderResources(0, Some(&[Some(scene_srv.clone()), Some(sampled[1].clone())]));
        c.PSSetShaderResources(9, Some(&[Some(sampled[0].clone())]));
        let mut bd = D3D11_BLEND_DESC::default();
        bd.RenderTarget[0].RenderTargetWriteMask = 0xf;
        let mut bs = None;
        g.dev.CreateBlendState(&bd, Some(&mut bs)).unwrap();
        let bs = bs.unwrap();
        c.OMSetBlendState(&bs, Some(&[0.1, 0.2, 0.3, 0.4]), 0x0f0f);
        let before = (refs(&other_rtv), refs(&sampled[0]), refs(&sampled[1]), refs(&other), refs(&scene_srv), refs(&depth), refs(&bs));

        let p = Params { width: N as u32, height: N as u32, length: 0.7, debug: false };
        for _ in 0..4 {
            assert!(r.sh.run(g.dev.as_raw(), g.ctx.as_raw(), scene_srv.as_raw(), depth.as_raw(), &p));
        }

        let mut rtvs: [Option<ID3D11RenderTargetView>; 8] = Default::default();
        c.OMGetRenderTargets(Some(&mut rtvs), None);
        assert_eq!(rtvs[0].as_ref().unwrap().as_raw(), other_rtv.as_raw(), "render target not restored");
        assert!(rtvs[1..].iter().all(|x| x.is_none()), "extra render targets left bound");
        drop(rtvs);
        let mut n = 2u32;
        let mut vps = [D3D11_VIEWPORT::default(); 2];
        c.RSGetViewports(&mut n, Some(vps.as_mut_ptr()));
        assert_eq!((n, vps[0].TopLeftX, vps[0].Width, vps[0].Height), (1, 1.0, 30.0, 20.0));
        assert_eq!(c.IAGetPrimitiveTopology(), D3D_PRIMITIVE_TOPOLOGY_POINTLIST);
        let mut srv = [None, None];
        c.PSGetShaderResources(0, Some(&mut srv));
        assert_eq!(srv[0].as_ref().unwrap().as_raw(), scene_srv.as_raw(), "SRV 0 not restored");
        assert_eq!(srv[1].as_ref().unwrap().as_raw(), sampled[1].as_raw(), "SRV 1 not restored");
        drop(srv);
        let mut mine: [Option<ID3D11ShaderResourceView>; 10] = Default::default();
        c.PSGetShaderResources(9, Some(&mut mine[..1]));
        assert_eq!(mine[0].as_ref().unwrap().as_raw(), sampled[0].as_raw(), "t9 must be left alone");
        drop(mine);
        let mut leftovers: [Option<ID3D11ShaderResourceView>; 7] = Default::default();
        c.PSGetShaderResources(10, Some(&mut leftovers));
        assert!(leftovers.iter().all(|x| x.is_none()), "t10..t16 left bound");
        let mut smp = [None];
        c.PSGetSamplers(15, Some(&mut smp));
        assert!(smp[0].is_none(), "sampler s15 left bound");
        let (mut got_bs, mut factor, mut mask) = (None, [0.0f32; 4], 0u32);
        c.OMGetBlendState(Some(&mut got_bs), Some(&mut factor), Some(&mut mask));
        assert_eq!((got_bs.as_ref().unwrap().as_raw(), factor, mask), (bs.as_raw(), [0.1, 0.2, 0.3, 0.4], 0x0f0f), "blend state not restored");
        drop(got_bs);
        let mut ps = None;
        c.PSGetShader(&mut ps, None, None);
        assert!(ps.is_none(), "pixel shader not restored");
        let mut vs = None;
        c.VSGetShader(&mut vs, None, None);
        assert!(vs.is_none(), "vertex shader not restored");
        let mut cb = [None];
        c.PSGetConstantBuffers(0, Some(&mut cb));
        assert!(cb[0].is_none(), "constant buffer 0 not restored");

        // bind / unbind of the light-adjust inputs.
        assert!(r.sh.bind(g.ctx.as_raw(), 0.6));
        let mut t9 = [None];
        c.PSGetShaderResources(SRV_SLOT, Some(&mut t9));
        assert!(t9[0].is_some(), "t9 not bound");
        let mut b8 = [None];
        c.PSGetConstantBuffers(CB_SLOT, Some(&mut b8));
        assert!(b8[0].is_some(), "b8 not bound");
        drop((t9, b8));
        unbind(g.ctx.as_raw());
        let mut t9 = [None];
        c.PSGetShaderResources(SRV_SLOT, Some(&mut t9));
        assert!(t9[0].is_none(), "t9 not unbound");
        let mut b8 = [None];
        c.PSGetConstantBuffers(CB_SLOT, Some(&mut b8));
        assert!(b8[0].is_none(), "b8 not unbound");
        drop((t9, b8));

        // Dropping the module's objects gives every reference back.
        let _ = std::mem::replace(&mut r.sh, Shafts::new(g.dev.as_raw(), blobs()).unwrap());
        let after = (refs(&other_rtv), refs(&sampled[0]), refs(&sampled[1]), refs(&other), refs(&scene_srv), refs(&depth), refs(&bs));
        assert_eq!(after, before, "reference counts changed");
        assert!(!crate::d3d::ignoring(), "ignore flag left on");
    }
}

fn refs<T: Interface>(o: &T) -> u32 {
    unsafe {
        let raw = o.as_raw();
        let vt = *(raw as *const *const usize);
        let add: unsafe extern "system" fn(*mut c_void) -> u32 = std::mem::transmute(*vt.add(1));
        let rel: unsafe extern "system" fn(*mut c_void) -> u32 = std::mem::transmute(*vt.add(2));
        let n = add(raw);
        rel(raw);
        n - 1
    }
}

#[test]
fn resize_recreates_the_targets() {
    let Some(mut r) = Rig::new() else { return };
    r.run(BAND, 0.7);
    assert_eq!(r.sh.size, (N as u32, N as u32));
    let (_, qw, qh) = shaft_light(&r.g, &r.sh);
    assert_eq!((qw, qh), (32, 32));
    r.run_sized(64, 48, BAND, 0.7, false);
    assert_eq!(r.sh.size, (64, 48));
    let (px, qw, qh) = shaft_light(&r.g, &r.sh);
    assert_eq!((qw, qh, px.len()), (16, 12, 16 * 12));
    // The sun state starts again from zero after a resize (no stale position).
    let (s0, _) = sun_state(&r.g, &r.sh);
    assert!(s0[2] <= 0.25, "confidence carried over a resize: {}", s0[2]);
    // An odd size rounds the quarter size up.
    r.run_sized(66, 50, BAND, 0.7, false);
    let (_, qw, qh) = shaft_light(&r.g, &r.sh);
    assert_eq!((qw, qh), (17, 13));
}

#[test]
fn the_lazy_read_back_follows_the_gpu_without_waiting() {
    let Some(mut r) = Rig::new() else { return };
    // WARP may need a few frames before a staging copy is ready (the read never waits for it).
    let mut got = None;
    for _ in 0..200 {
        r.run(SUN, 0.7);
        unsafe { r.g.ctx.Flush() };
        if r.sh.last_read.is_some() && r.sh.runs > 40 {
            got = r.sh.last_read;
            break;
        }
    }
    let v = got.expect("no estimate was read back");
    println!("shafts read-back: ({:.3}, {:.3}) confidence {:.3}", v[0], v[1], v[2]);
    assert!((v[0] - 0.5).abs() < 0.06 && (v[1] - 6.0 / N as f32).abs() < 0.06 && v[2] > 0.5);
}

// ---- the light-adjust shader --------------------------------------------------------------------------

/// Renders the light-adjust substitute over a 4x4 target with a uniform encoded colour. `shafts` = Some((t9 value,
/// b8 value)) binds the shaft inputs; None leaves t9/b8 unbound. Returns pixel (1, 1).
fn light_adjust(g: &Gpu, enc: [f32; 3], shafts: Option<([f32; 4], [f32; 4])>, bind_cb_only: bool) -> [f32; 4] {
    light_adjust_dbg(g, enc, shafts, bind_cb_only, 0.0)
}

/// The same with the debug level `level` in b6[0].w.
fn light_adjust_dbg(g: &Gpu, enc: [f32; 3], shafts: Option<([f32; 4], [f32; 4])>, bind_cb_only: bool, level: f32) -> [f32; 4] {
    const VS: &str = "struct O { float4 pos : SV_POSITION; float4 uv : TEXCOORD0; };
O main(uint id : SV_VertexID) { float2 p = float2((id << 1) & 2, id & 2); O o;
  o.pos = float4(p * float2(2, -2) + float2(-1, 1), 0, 1); o.uv = float4(p, 0, 0); return o; }";
    let vs_dxbc = shaders::compile(VS, "main", "vs_5_0").unwrap();
    let ps_dxbc = shaders::compile(crate::hdr_shaders::LIGHT_ADJUST_HLSL, crate::hdr_shaders::LIGHT_ADJUST_ENTRY, "ps_5_0").unwrap();
    unsafe {
        let mut vs = None;
        g.dev.CreateVertexShader(&vs_dxbc, None, Some(&mut vs)).unwrap();
        let mut ps = None;
        g.dev.CreatePixelShader(&ps_dxbc, None, Some(&mut ps)).unwrap();
        let f32s = |v: &[[f32; 4]]| -> Vec<u8> { v.iter().flat_map(|p| p.iter().flat_map(|c| c.to_le_bytes())).collect() };
        let uniform = |w: usize, h: usize, px: [f32; 4]| {
            let bytes = f32s(&vec![px; w * h]);
            srv_of(g, &tex(g, w, h, DXGI_FORMAT_R32G32B32A32_FLOAT, D3D11_BIND_SHADER_RESOURCE, Some((&bytes, w * 16))))
        };
        let t0 = uniform(4, 4, [enc[0], enc[1], enc[2], 1.0]);
        let t1 = uniform(1, 1, [0.5, 0.3, 0.2, 0.5]);
        let sd = D3D11_SAMPLER_DESC { Filter: D3D11_FILTER_MIN_MAG_MIP_POINT, AddressU: D3D11_TEXTURE_ADDRESS_CLAMP, AddressV: D3D11_TEXTURE_ADDRESS_CLAMP, AddressW: D3D11_TEXTURE_ADDRESS_CLAMP, MaxAnisotropy: 1, ComparisonFunc: D3D11_COMPARISON_NEVER, MaxLOD: f32::MAX, ..Default::default() };
        let mut smp = None;
        g.dev.CreateSamplerState(&sd, Some(&mut smp)).unwrap();
        let cbytes = f32s(&[[0.9, 0.0, 0.0, level], [0.7, 0.0, 0.0, 0.0]]);
        let cbd = D3D11_BUFFER_DESC { ByteWidth: 32, Usage: D3D11_USAGE_IMMUTABLE, BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32, ..Default::default() };
        let csd = D3D11_SUBRESOURCE_DATA { pSysMem: cbytes.as_ptr() as *const c_void, SysMemPitch: 0, SysMemSlicePitch: 0 };
        let mut cb0 = None;
        g.dev.CreateBuffer(&cbd, Some(&csd), Some(&mut cb0)).unwrap();
        let rt = tex(g, 4, 4, DXGI_FORMAT_R32G32B32A32_FLOAT, D3D11_BIND_RENDER_TARGET, None);
        let mut rtv = None;
        g.dev.CreateRenderTargetView(&rt, None, Some(&mut rtv)).unwrap();
        let c = &g.ctx;
        c.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
        c.VSSetShader(vs.as_ref(), None);
        c.PSSetShader(ps.as_ref(), None);
        c.PSSetShaderResources(0, Some(&[Some(t0), Some(t1)]));
        c.PSSetSamplers(0, Some(&[smp.clone(), smp]));
        c.PSSetConstantBuffers(0, Some(&[cb0]));
        if level != 0.0 {
            // b6 (mod_params): the debug level in mp.w; everything else zero.
            let b6 = f32s(&[[0.0, 0.0, 0.0, level], [0.0; 4]]);
            let d6 = D3D11_BUFFER_DESC { ByteWidth: 32, Usage: D3D11_USAGE_IMMUTABLE, BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32, ..Default::default() };
            let s6 = D3D11_SUBRESOURCE_DATA { pSysMem: b6.as_ptr() as *const c_void, SysMemPitch: 0, SysMemSlicePitch: 0 };
            let mut cb6 = None;
            g.dev.CreateBuffer(&d6, Some(&s6), Some(&mut cb6)).unwrap();
            c.PSSetConstantBuffers(6, Some(&[cb6]));
        }
        c.OMSetRenderTargets(Some(&[rtv]), None);
        c.RSSetViewports(Some(&[D3D11_VIEWPORT { TopLeftX: 0.0, TopLeftY: 0.0, Width: 4.0, Height: 4.0, MinDepth: 0.0, MaxDepth: 1.0 }]));
        if let Some((t9, hp)) = shafts {
            if !bind_cb_only {
                c.PSSetShaderResources(SRV_SLOT, Some(&[Some(uniform(2, 2, t9))]));
            }
            let hb = f32s(&[hp]);
            let hd = D3D11_BUFFER_DESC { ByteWidth: 16, Usage: D3D11_USAGE_IMMUTABLE, BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32, ..Default::default() };
            let hsd = D3D11_SUBRESOURCE_DATA { pSysMem: hb.as_ptr() as *const c_void, SysMemPitch: 0, SysMemSlicePitch: 0 };
            let mut b8 = None;
            g.dev.CreateBuffer(&hd, Some(&hsd), Some(&mut b8)).unwrap();
            c.PSSetConstantBuffers(CB_SLOT, Some(&[b8]));
        }
        c.Draw(3, 0);
        c.OMSetRenderTargets(None, None);
        c.PSSetShaderResources(SRV_SLOT, Some(&[None]));
        c.PSSetConstantBuffers(CB_SLOT, Some(&[None]));
        read(g, &rt, 4, 4, DXGI_FORMAT_R32G32B32A32_FLOAT, false)[5]
    }
}

#[test]
fn light_adjust_is_the_identity_with_strength_zero_or_unbound_inputs() {
    let Some(g) = gpu() else { return };
    for enc in [[0.5, 0.4, 0.3], [0.9, 0.9, 0.9], [0.1, 0.1, 0.1], [1.4, 1.2, 1.0]] {
        let plain = light_adjust(&g, enc, None, false);
        // t9 bound with plenty of light but strength 0 in b8: nothing.
        let zero = light_adjust(&g, enc, Some(([5.0, 5.0, 5.0, 1.0], [0.0; 4])), false);
        assert_eq!(plain, zero, "strength 0 changed the colour {enc:?}");
        // Garbage in t9 must not leak through at strength 0 either.
        let nan = light_adjust(&g, enc, Some(([f32::NAN, f32::INFINITY, -3.0, 1.0], [0.0; 4])), false);
        assert_eq!(plain, nan, "NaN in t9 leaked at strength 0 for {enc:?}");
        // b8 bound but t9 unbound: the texture reads zero, nothing is added.
        let no_tex = light_adjust(&g, enc, Some(([0.0; 4], [1.0, 0.0, 0.0, 0.0])), true);
        assert_eq!(plain, no_tex, "unbound t9 changed the colour {enc:?}");
    }
}

#[test]
fn light_adjust_adds_the_shafts_but_spares_bright_pixels() {
    let Some(g) = gpu() else { return };
    let light = [0.2, 0.2, 0.2, 1.0];
    let on = [1.0, 0.0, 0.0, 0.0];
    let mid = [0.5, 0.4, 0.3];
    let (p0, p1) = (light_adjust(&g, mid, None, false), light_adjust(&g, mid, Some((light, on)), false));
    println!("shafts light-adjust mid {p0:?} -> {p1:?}");
    for c in 0..3 {
        assert!(p1[c] > p0[c] + 0.02, "shafts did not brighten channel {c}: {} -> {}", p0[c], p1[c]);
    }
    // Twice the strength adds more.
    let p2 = light_adjust(&g, mid, Some((light, [2.0, 0.0, 0.0, 0.0])), false);
    assert!(p2[0] > p1[0] + 0.01);
    // A pixel that is already bright (linear >= 1) gets nothing: lit surfaces do not wash out.
    let hot = [1.0, 1.0, 1.0];
    let (h0, h1) = (light_adjust(&g, hot, None, false), light_adjust(&g, hot, Some((light, on)), false));
    println!("shafts light-adjust bright {h0:?} -> {h1:?}");
    assert!((h1[0] - h0[0]).abs() < 1e-5, "bright pixel changed: {} -> {}", h0[0], h1[0]);
    // The relative gain falls monotonically as the pixel brightens.
    let mut prev = f32::MAX;
    for v in [0.3f32, 0.5, 0.65, 0.8, 0.95] {
        let (a, b) = (light_adjust(&g, [v, v, v], None, false), light_adjust(&g, [v, v, v], Some((light, on)), false));
        let d = b[0] - a[0];
        assert!(d <= prev + 1e-4 && d >= -1e-5, "gain not decreasing at {v}: {d} after {prev}");
        prev = d;
    }
}

// ---- robust sky: depth statistics, finite far plane, debug level 4 ------------------------------------

#[test]
fn depth_statistics_reduce_to_the_max_and_the_invalid_fraction() {
    let Some(mut r) = Rig::new() else { return };
    r.run(BAND, 0.7);
    let d = depth_stats(&r.g, &r.sh);
    println!("shafts depth stats (invalid sky): {d:?}");
    assert_eq!(d[0], 10.0, "max valid depth");
    assert!((d[1] - 40.0 / 128.0).abs() < 0.01, "invalid fraction {}", d[1]);
    // A finite sky: the maximum is the sky depth, nothing is invalid.
    let fin = World { finite_sky: true, ..BAND };
    r.run(fin, 0.7);
    let d = depth_stats(&r.g, &r.sh);
    println!("shafts depth stats (finite sky): {d:?}");
    assert_eq!(d[0], 5000.0);
    assert_eq!(d[1], 0.0);
    // Garbage depth (NaN, negative, huge) counts as invalid and never as the maximum.
    let scene = scene_tex(&r.g, N, N, |_, _| [0.1, 0.1, 0.1]);
    let depth = depth_tex(&r.g, N, N, |x, _| match x % 8 { 0 => f32::NAN, 2 => -3.0, 4 => 1e31, _ => 7.0 });
    let srv = srv_of(&r.g, &scene);
    let p = Params { width: N as u32, height: N as u32, length: 0.7, debug: false };
    assert!(unsafe { r.sh.run(r.g.dev.as_raw(), r.g.ctx.as_raw(), srv.as_raw(), depth.as_raw(), &p) });
    let d = depth_stats(&r.g, &r.sh);
    assert_eq!(d[0], 7.0);
    assert!((d[1] - 0.75).abs() < 0.01, "{d:?}"); // the stride-2 sampling sees the even columns: NaN, negative, huge, valid
}

#[test]
fn a_finite_sky_far_plane_gives_shafts() {
    let Some(mut r) = Rig::new() else { return };
    let w = World { columns: true, finite_sky: true, ..BAND };
    r.settle(w, 1.0, 14);
    let (s0, _) = sun_state(&r.g, &r.sh);
    let (px, qw, _) = shaft_light(&r.g, &r.sh);
    let gap = light_at(&px, qw, 64, 80);
    let col = light_at(&px, qw, 36, 80).max(light_at(&px, qw, 92, 80));
    println!("shafts finite sky: sun ({:.3}, {:.3}) conf {:.3} sky {:.3}; gap {gap:.4} column {col:.4}", s0[0], s0[1], s0[2], s0[3]);
    assert!(s0[2] > 0.8, "confidence {}", s0[2]);
    assert!((s0[0] - 0.5).abs() < 0.05 && (s0[1] - 6.0 / N as f32).abs() < 0.06, "sun at ({}, {})", s0[0], s0[1]);
    assert!(gap > 0.01 && gap > col * 1.5, "rays: gap {gap}, column {col}");
}

#[test]
fn the_sky_fraction_is_reported_in_the_sun_state_and_the_max_depth_next_to_it() {
    for (finite, depth) in [(false, 10.0), (true, 5000.0)] {
        let Some(mut r) = Rig::new() else { return };
        r.settle(World { finite_sky: finite, ..BAND }, 0.7, 6);
        let (s0, s1) = sun_state(&r.g, &r.sh);
        println!("shafts sky fraction (finite {finite}): {} max depth {}", s0[3], s1[3]);
        assert!((s0[3] - 40.0 / 128.0).abs() < 0.03, "sky fraction {}", s0[3]);
        assert_eq!(s1[3], depth, "max depth in the sun state");
    }
}

#[test]
fn an_invalid_depth_sky_does_not_turn_a_bright_flat_ground_into_sky() {
    // Invalid-depth sky (31 % of the samples) over a BRIGHT ground that is all at the same, farthest valid depth: the
    // far-plane rule must stay off, otherwise the whole ground would be sky.
    let Some(mut r) = Rig::new() else { return };
    let scene = scene_tex(&r.g, N, N, |x, y| if y < SKY_ROWS { BAND.colour(x, y) } else { [0.95, 0.95, 0.95] });
    let depth = depth_tex(&r.g, N, N, |_, y| if y < SKY_ROWS { 0.0 } else { 10.0 });
    let srv = srv_of(&r.g, &scene);
    let p = Params { width: N as u32, height: N as u32, length: 0.7, debug: false };
    for _ in 0..10 {
        assert!(unsafe { r.sh.run(r.g.dev.as_raw(), r.g.ctx.as_raw(), srv.as_raw(), depth.as_raw(), &p) });
    }
    let (s0, _) = sun_state(&r.g, &r.sh);
    println!("shafts bright flat ground: sky fraction {} confidence {} at ({}, {})", s0[3], s0[2], s0[0], s0[1]);
    assert!((s0[3] - 40.0 / 128.0).abs() < 0.03, "ground counted as sky: {}", s0[3]);
    assert!((s0[1] - 6.0 / N as f32).abs() < 0.08, "sun pulled into the ground: v = {}", s0[1]);
}

#[test]
fn no_far_pixels_and_no_sky_gives_no_shafts_even_with_spread_depths() {
    let Some(mut r) = Rig::new() else { return };
    let w = World { no_sky: true, finite_sky: true, ..BAND };
    r.settle(w, 0.7, 12);
    let (px, _, _) = shaft_light(&r.g, &r.sh);
    let max = px.iter().map(|p| p[0].abs().max(p[1].abs()).max(p[2].abs())).fold(0.0f32, f32::max);
    let (s0, _) = sun_state(&r.g, &r.sh);
    println!("shafts spread depths, no sky: max {max}, confidence {}", s0[2]);
    assert!(max < 1e-4 && s0[2] < 0.01, "{max} / {}", s0[2]);
}

#[test]
fn shaft_texture_alpha_carries_the_sky_share() {
    let Some(mut r) = Rig::new() else { return };
    r.settle(BAND, 0.7, 6);
    let (px, qw, _) = shaft_light(&r.g, &r.sh);
    let sky = px[(2 * qw) + 5][3]; // quarter row 2 = full rows 8..11: sky
    let ground = px[(20 * qw) + 5][3]; // full rows 80..: ground
    assert!(sky > 0.99, "sky alpha {sky}");
    assert!(ground < 0.01, "ground alpha {ground}");
}

#[test]
fn debug_level_4_tints_the_sky_cyan_and_the_shaft_light_yellow() {
    let Some(g) = gpu() else { return };
    let mid = [0.5, 0.4, 0.3];
    let on = [1.0, 0.0, 0.0, 0.0];
    let plain = light_adjust_dbg(&g, mid, Some(([0.0, 0.0, 0.0, 1.0], on)), false, 0.0);
    let sky = light_adjust_dbg(&g, mid, Some(([0.0, 0.0, 0.0, 1.0], on)), false, 4.0);
    println!("shafts level 4: plain {plain:?}, sky {sky:?}");
    assert!(sky[1] > plain[1] && sky[2] > plain[2] + 0.2 && sky[0] < plain[0] * 0.7, "not cyan: {plain:?} -> {sky:?}");
    // Without the level the sky share changes nothing.
    assert_eq!(plain, light_adjust_dbg(&g, mid, Some(([0.0, 0.0, 0.0, 0.0], on)), false, 0.0));
    // Shaft light (and no sky): yellow.
    let lit = light_adjust_dbg(&g, mid, Some(([0.3, 0.3, 0.3, 0.0], on)), false, 4.0);
    println!("shafts level 4 lit: {lit:?}");
    assert!(lit[0] > 0.9 && lit[1] > 0.9 && lit[2] < 0.15, "not yellow: {lit:?}");
    // Nothing bound: no tint at all.
    let none = light_adjust_dbg(&g, mid, None, false, 4.0);
    assert_eq!(none, light_adjust_dbg(&g, mid, None, false, 0.0));
}
