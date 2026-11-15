//! WARP tests of the halo shaders and of `Halo::run`. They need no game exe: the scene is synthetic.
//!
//! Harness: D3D11 WARP device, a scene texture (float16 RGBA, gamma 2.0 encoded like the engine's) filled from a
//! closure that returns LINEAR light, `Halo::run` through the raw vtables exactly as the hook does, then the
//! finished halo `G` (1/4 resolution) read back.

use std::ffi::c_void;
use std::sync::OnceLock;

use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::Interface;

use super::*;

const N: usize = 128;

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
    B.get_or_init(|| compile_blobs().unwrap_or_else(|e| panic!("halo.hlsl does not compile: {e}")))
}

fn f32_to_f16(v: f32) -> u16 {
    let b = v.to_bits();
    let sign = ((b >> 16) & 0x8000) as u16;
    let exp = ((b >> 23) & 0xff) as i32;
    let man = b & 0x7f_ffff;
    if exp == 0xff {
        return sign | 0x7c00 | if man != 0 { 0x200 } else { 0 };
    }
    let e = exp - 127 + 15;
    if e >= 31 {
        return sign | 0x7c00;
    }
    if e <= 0 {
        return sign;
    }
    let mut h = ((e as u32) << 10) | (man >> 13);
    if man & 0x1000 != 0 {
        h += 1; // round to nearest (ties up: good enough for test data)
    }
    sign | h as u16
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

struct Scene {
    tex: ID3D11Texture2D,
    srv: ID3D11ShaderResourceView,
    size: (usize, usize),
}

fn make_tex(g: &Gpu, w: usize, h: usize, fmt: DXGI_FORMAT, bind: D3D11_BIND_FLAG, init: Option<(&[u8], u32)>) -> ID3D11Texture2D {
    let desc = D3D11_TEXTURE2D_DESC { Width: w as u32, Height: h as u32, MipLevels: 1, ArraySize: 1, Format: fmt, SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, Usage: D3D11_USAGE_DEFAULT, BindFlags: bind.0 as u32, CPUAccessFlags: 0, MiscFlags: 0 };
    let sd = init.map(|(d, pitch)| D3D11_SUBRESOURCE_DATA { pSysMem: d.as_ptr() as *const c_void, SysMemPitch: pitch, SysMemSlicePitch: 0 });
    let mut t = None;
    unsafe { g.dev.CreateTexture2D(&desc, sd.as_ref().map(|s| s as *const _), Some(&mut t)).expect("CreateTexture2D") };
    t.unwrap()
}

/// A float16 scene of `w` x `h`; `f(x, y)` is the LINEAR colour, stored gamma-2.0 encoded (sqrt).
fn scene_sized(g: &Gpu, w: usize, h: usize, f: impl Fn(usize, usize) -> [f32; 3]) -> Scene {
    let mut v = Vec::with_capacity(w * h * 8);
    for y in 0..h {
        for x in 0..w {
            let c = f(x, y);
            for k in 0..3 {
                let e = if c[k].is_finite() { c[k].max(0.0).sqrt() } else { c[k] };
                v.extend_from_slice(&f32_to_f16(e).to_le_bytes());
            }
            v.extend_from_slice(&f32_to_f16(1.0).to_le_bytes());
        }
    }
    let tex = make_tex(g, w, h, DXGI_FORMAT_R16G16B16A16_FLOAT, D3D11_BIND_SHADER_RESOURCE, Some((&v, (w * 8) as u32)));
    let mut srv = None;
    unsafe { g.dev.CreateShaderResourceView(&tex, None, Some(&mut srv)).unwrap() };
    Scene { tex, srv: srv.unwrap(), size: (w, h) }
}

fn scene(g: &Gpu, f: impl Fn(usize, usize) -> [f32; 3]) -> Scene {
    scene_sized(g, N, N, f)
}

fn staging(g: &Gpu, t: &ID3D11Texture2D, w: usize, h: usize, fmt: DXGI_FORMAT) -> ID3D11Texture2D {
    let desc = D3D11_TEXTURE2D_DESC { Width: w as u32, Height: h as u32, MipLevels: 1, ArraySize: 1, Format: fmt, SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, Usage: D3D11_USAGE_STAGING, BindFlags: 0, CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32, MiscFlags: 0 };
    let mut st = None;
    unsafe {
        g.dev.CreateTexture2D(&desc, None, Some(&mut st)).unwrap();
        let st = st.unwrap();
        g.ctx.CopyResource(&st, t);
        st
    }
}

/// RGBA of a float16 texture of `w` x `h`, row-major.
fn read_f16(g: &Gpu, t: &ID3D11Texture2D, w: usize, h: usize) -> Vec<[f32; 4]> {
    unsafe {
        let st = staging(g, t, w, h, DXGI_FORMAT_R16G16B16A16_FLOAT);
        let mut m = D3D11_MAPPED_SUBRESOURCE::default();
        g.ctx.Map(&st, 0, D3D11_MAP_READ, 0, Some(&mut m)).unwrap();
        let mut out = Vec::with_capacity(w * h);
        for y in 0..h {
            let row = std::slice::from_raw_parts((m.pData as *const u8).add(y * m.RowPitch as usize) as *const u16, w * 4);
            for x in 0..w {
                out.push([f16_to_f32(row[x * 4]), f16_to_f32(row[x * 4 + 1]), f16_to_f32(row[x * 4 + 2]), f16_to_f32(row[x * 4 + 3])]);
            }
        }
        g.ctx.Unmap(&st, 0);
        out
    }
}

struct Glow {
    px: Vec<[f32; 4]>,
    w: usize,
    h: usize,
}

impl Glow {
    fn at(&self, x: usize, y: usize) -> [f32; 4] {
        self.px[y * self.w + x]
    }
    /// Sum of the colour channels.
    fn lum(&self, x: usize, y: usize) -> f32 {
        let p = self.at(x, y);
        p[0] + p[1] + p[2]
    }
    fn max_lum(&self) -> f32 {
        (0..self.h).flat_map(|y| (0..self.w).map(move |x| (x, y))).map(|(x, y)| self.lum(x, y)).fold(0.0, f32::max)
    }
}

fn run_halo(g: &Gpu, s: &Scene, radius: f32) -> Glow {
    unsafe {
        let mut h = Halo::new(g.dev.as_raw(), blobs()).expect("Halo::new");
        let p = HaloParams { radius, width: s.size.0 as u32, height: s.size.1 as u32 };
        assert!(h.run(g.dev.as_raw(), g.ctx.as_raw(), s.srv.as_raw(), &p), "run failed");
        let (w, hh) = h.glow_size();
        let tex = ID3D11Texture2D::from_raw_borrowed(&h.glow_tex()).expect("glow texture").clone();
        let px = read_f16(g, &tex, w as usize, hh as usize);
        drop(h);
        Glow { px, w: w as usize, h: hh as usize }
    }
}

const BLACK: [f32; 3] = [0.0; 3];
const ORANGE: [f32; 3] = [3.0, 1.2, 0.2];

/// A 2x2 block of `c` whose centre is the image centre (a texel boundary of every pyramid level), else black.
fn block2(c: [f32; 3]) -> impl Fn(usize, usize) -> [f32; 3] {
    move |x, y| if (N / 2 - 1..=N / 2).contains(&x) && (N / 2 - 1..=N / 2).contains(&y) { c } else { BLACK }
}

// ---- compile -----------------------------------------------------------------------------------------

#[test]
fn halo_hlsl_compiles_for_all_entries() {
    let b = blobs();
    for (name, code) in [("vs", &b.vs), ("mean", &b.mean), ("source", &b.source), ("down", &b.down), ("up", &b.up), ("up_final", &b.up_final)] {
        assert_eq!(&code[..4], b"DXBC", "{name}");
    }
}

// ---- behaviour ---------------------------------------------------------------------------------------

#[test]
fn a_saturated_spot_makes_a_symmetric_halo_that_decays_with_distance() {
    let Some(g) = gpu() else { return };
    let s = scene(&g, block2(ORANGE));
    let gl = run_halo(&g, &s, 1.0);
    assert_eq!((gl.w, gl.h), (32, 32));
    let c = gl.w / 2; // the spot sits on the boundary between texels c - 1 and c
    println!("halo spot: centre {:?}, 4 texels away {:?}, 12 away {:?}", gl.at(c, c), gl.at(c + 4, c), gl.at(c + 12, c));
    assert!(gl.lum(c, c) > 0.3, "no halo at the source: {}", gl.lum(c, c));
    // Mirror symmetry about the spot, both axes, and the diagonal.
    for d in 0..c {
        for k in [0usize, 3, 9] {
            let (a, b) = (gl.lum(c - 1 - d, c + k), gl.lum(c + d, c + k));
            assert!((a - b).abs() <= 2e-3 * a.max(b) + 1e-6, "x mirror d={d} k={k}: {a} vs {b}");
            let (a, b) = (gl.lum(c + k, c - 1 - d), gl.lum(c + k, c + d));
            assert!((a - b).abs() <= 2e-3 * a.max(b) + 1e-6, "y mirror d={d} k={k}: {a} vs {b}");
        }
        for k in [0usize, 5] {
            let (a, b) = (gl.lum(c + d, c + k), gl.lum(c + k, c + d));
            assert!((a - b).abs() <= 2e-3 * a.max(b) + 1e-6, "diagonal d={d} k={k}: {a} vs {b}");
        }
    }
    // Decay: along the row through the spot, never rising with the distance; reaches several texels out
    // (a 4-pixel texel: 8 texels = 32 pixels) and fades towards the edge.
    let mut prev = f32::MAX;
    for d in 0..c {
        let v = gl.lum(c + d, c);
        assert!(v <= prev * 1.002 + 1e-6, "halo rises at distance {d}: {v} after {prev}");
        prev = v;
    }
    assert!(gl.lum(c + 8, c) > 1e-3, "halo too short: {}", gl.lum(c + 8, c));
    assert!(gl.lum(c + 15, c) < gl.lum(c + 2, c) * 0.25, "halo does not fade out");
    // The halo keeps the source hue: red > green > blue like (3, 1.2, 0.2).
    let p = gl.at(c + 2, c);
    assert!(p[0] > p[1] && p[1] > p[2], "hue lost: {p:?}");
}

#[test]
fn a_single_hot_pixel_cannot_flood_the_screen() {
    let Some(g) = gpu() else { return };
    let one = |v: f32| move |x: usize, y: usize| if (x, y) == (60, 70) { [v, 0.0, 0.0] } else { BLACK };
    let a = run_halo(&g, &scene(&g, one(60.0)), 1.0);
    let b = run_halo(&g, &scene(&g, one(60000.0)), 1.0);
    // 60000 is clipped to the same per-pixel maximum as 60 (the scene value is tamed, then the energy is clamped).
    let (ma, mb) = (a.max_lum(), b.max_lum());
    println!("halo hot pixel: peak {ma} (value 60) vs {mb} (value 60000)");
    assert!(ma.is_finite() && mb.is_finite());
    assert!((ma - mb).abs() <= 0.01 * ma.max(1e-3), "hot pixels of different size differ: {ma} vs {mb}");
    assert!(ma < 8.0 * 3.0, "one pixel must stay far below the clamp times the pyramid gain: {ma}");
    // One pixel of a 128 x 128 image: the far corner stays dark.
    assert!(b.lum(0, 0) < 0.05, "the glow of one pixel reaches the corner: {}", b.lum(0, 0));
}

#[test]
fn a_uniformly_bright_neutral_area_makes_no_glow() {
    let Some(g) = gpu() else { return };
    for v in [0.8f32, 1.0, 1.5, 3.0] {
        let gl = run_halo(&g, &scene(&g, move |_, _| [v, v, v]), 2.0);
        println!("halo uniform neutral {v}: max {}", gl.max_lum());
        assert!(gl.max_lum() < 1e-3, "a uniform neutral {v} area glows: {}", gl.max_lum());
    }
    // Control: a uniform saturated area IS a source (lamps and fire are saturated), so the detector is not dead.
    let gl = run_halo(&g, &scene(&g, |_, _| [1.0, 0.5, 0.1]), 1.0);
    assert!(gl.lum(16, 16) > 0.1, "a uniform saturated bright area does not glow: {}", gl.lum(16, 16));
    // The alpha channel carries the local mean (debug view): a uniform 0.8 area has mean 0.8.
    let gl = run_halo(&g, &scene(&g, |_, _| [0.8, 0.8, 0.8]), 1.0);
    let a = gl.at(10, 10)[3];
    assert!((a - 0.8).abs() < 0.01, "alpha should hold the local mean 0.8, got {a}");
}

#[test]
fn a_bright_neutral_spot_on_a_mid_bright_area_glows_but_full_screen_white_does_not() {
    let Some(g) = gpu() else { return };
    let spot = |bg: f32| move |x: usize, y: usize| if (N / 2 - 2..N / 2 + 2).contains(&x) && (N / 2 - 2..N / 2 + 2).contains(&y) { [3.0, 3.0, 3.0] } else { [bg, bg, bg] };
    let mid = run_halo(&g, &scene(&g, spot(0.5)), 1.0);
    let sky = run_halo(&g, &scene(&g, spot(1.5)), 1.0);
    let white = run_halo(&g, &scene(&g, |_, _| [3.0, 3.0, 3.0]), 1.0);
    let white1 = run_halo(&g, &scene(&g, |_, _| [1.0, 1.0, 1.0]), 1.0);
    println!("halo neutral spot: on 0.5 -> {}, on 1.5 -> {}, full-screen 3.0 -> {}, full-screen 1.0 -> {}", mid.lum(16, 16), sky.lum(16, 16), white.max_lum(), white1.max_lum());
    assert!(mid.lum(16, 16) > 0.5, "the spot does not glow on a mid-bright area: {}", mid.lum(16, 16));
    assert!(mid.lum(16, 16) > mid.lum(0, 0) * 20.0, "the glow is not local");
    assert!(sky.lum(16, 16) < mid.lum(16, 16), "the same spot must stand out less against a brighter surrounding");
    assert!(white.max_lum() < 1e-3 && white1.max_lum() < 1e-3, "full-screen white glows");
}

#[test]
fn a_sunlit_white_character_on_a_dark_floor_makes_no_glow() {
    let Some(g) = gpu() else { return };
    // A white coat / armour in the sun (slightly warm, up to 1.4x paper white), character-sized, on a dark wooden floor:
    // a lit surface, not a light. It must not get an aura; a lamp of the same size and place still glows.
    let body = |c: [f32; 3]| move |x: usize, y: usize| if (50..80).contains(&x) && (30..100).contains(&y) { c } else { [0.2, 0.15, 0.1] };
    let lit = run_halo(&g, &scene(&g, body([1.4, 1.3, 1.15])), 2.0);
    let lamp = run_halo(&g, &scene(&g, body([4.0, 3.2, 2.0])), 2.0);
    println!("halo sunlit white body: max {}, lamp-bright body: max {}", lit.max_lum(), lamp.max_lum());
    assert!(lit.max_lum() < 1e-3, "a sunlit white character glows: {}", lit.max_lum());
    assert!(lamp.max_lum() > 0.1, "a lamp-bright area must still glow: {}", lamp.max_lum());
}

#[test]
fn the_radius_widens_the_halo() {
    let Some(g) = gpu() else { return };
    let s = scene(&g, block2(ORANGE));
    let reach = |r: f32| {
        let gl = run_halo(&g, &s, r);
        let n = gl.px.iter().filter(|p| p[0] + p[1] + p[2] > 0.02).count();
        (n, gl.lum(gl.w / 2 + 10, gl.w / 2))
    };
    let (n_small, far_small) = reach(0.5);
    let (n_one, far_one) = reach(1.0);
    let (n_big, far_big) = reach(2.0);
    println!("halo area above 0.02: radius 0.5 -> {n_small}, 1 -> {n_one}, 2 -> {n_big}; 10 texels out: {far_small} {far_one} {far_big}");
    assert!(n_small < n_one && n_one < n_big, "the radius must widen the halo");
    assert!(far_small < far_one && far_one < far_big, "the radius must lift the far tail");
}

#[test]
fn nan_and_inf_in_the_scene_do_not_poison_the_halo() {
    let Some(g) = gpu() else { return };
    let s = scene(&g, |x, y| match (x + y) % 5 {
        0 => [f32::NAN, 0.5, 0.5],
        1 => [f32::INFINITY, 0.1, 0.1],
        2 => [-4.0, 0.2, 0.2],
        _ => [0.3, 0.3, 0.3],
    });
    let gl = run_halo(&g, &s, 1.0);
    assert!(gl.px.iter().all(|p| p.iter().all(|v| v.is_finite() && *v >= 0.0)), "NaN, inf or negative values reached the halo");
}

#[test]
fn an_eight_bit_scene_works_too() {
    let Some(g) = gpu() else { return };
    // BGRA8, encoded: a block of saturated orange (encoded 1.0, 0.7, 0.2 -> linear 1, 0.49, 0.04).
    let mut v = vec![0u8; N * N * 4];
    for y in 0..N {
        for x in 0..N {
            if (N / 2 - 2..N / 2 + 2).contains(&x) && (N / 2 - 2..N / 2 + 2).contains(&y) {
                let o = (y * N + x) * 4;
                v[o] = (0.2f32 * 255.0) as u8; // B
                v[o + 1] = (0.7f32 * 255.0) as u8; // G
                v[o + 2] = 255; // R
                v[o + 3] = 255;
            }
        }
    }
    let tex = make_tex(&g, N, N, DXGI_FORMAT_B8G8R8A8_UNORM, D3D11_BIND_SHADER_RESOURCE, Some((&v, (N * 4) as u32)));
    let mut srv = None;
    unsafe { g.dev.CreateShaderResourceView(&tex, None, Some(&mut srv)).unwrap() };
    let s = Scene { tex, srv: srv.unwrap(), size: (N, N) };
    let gl = run_halo(&g, &s, 1.0);
    assert!(gl.lum(16, 16) > 0.02, "an 8-bit source does not glow: {}", gl.lum(16, 16));
    assert!(gl.lum(0, 0) < gl.lum(16, 16) * 0.1);
    let _ = &s.tex;
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
fn run_restores_pipeline_state_and_leaves_no_references() {
    let Some(g) = gpu() else { return };
    unsafe {
        let s = scene(&g, block2(ORANGE));
        // Sentinel state the halo must restore: render target, viewport, topology, SRVs 0 and 1 (the game's own
        // inputs of the pending light-adjust draw, slot 0 is the scene itself), blend state, constant buffer 0.
        let other = make_tex(&g, N, N, DXGI_FORMAT_R16G16B16A16_FLOAT, D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE, None);
        let mut other_rtv = None;
        g.dev.CreateRenderTargetView(&other, None, Some(&mut other_rtv)).unwrap();
        let other_rtv = other_rtv.unwrap();
        let weights = make_tex(&g, 1, 1, DXGI_FORMAT_R16G16B16A16_FLOAT, D3D11_BIND_SHADER_RESOURCE, None);
        let mut wsrv = None;
        g.dev.CreateShaderResourceView(&weights, None, Some(&mut wsrv)).unwrap();
        let wsrv = wsrv.unwrap();
        let c = &g.ctx;
        let vp = D3D11_VIEWPORT { TopLeftX: 1.0, TopLeftY: 2.0, Width: 30.0, Height: 20.0, MinDepth: 0.0, MaxDepth: 1.0 };
        c.OMSetRenderTargets(Some(&[Some(other_rtv.clone())]), None);
        c.RSSetViewports(Some(&[vp]));
        c.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_POINTLIST);
        c.PSSetShaderResources(0, Some(&[Some(s.srv.clone()), Some(wsrv.clone())]));
        let mut bd = D3D11_BLEND_DESC::default();
        bd.RenderTarget[0].RenderTargetWriteMask = 0xf;
        let mut bs = None;
        g.dev.CreateBlendState(&bd, Some(&mut bs)).unwrap();
        let bs = bs.unwrap();
        c.OMSetBlendState(&bs, Some(&[0.1, 0.2, 0.3, 0.4]), 0x0f0f);
        let before = (refs(&other_rtv), refs(&s.srv), refs(&wsrv), refs(&other), refs(&s.tex), refs(&bs));

        let p = HaloParams { radius: 1.0, width: N as u32, height: N as u32 };
        let mut h = Halo::new(g.dev.as_raw(), blobs()).unwrap();
        for _ in 0..2 {
            assert!(h.run(g.dev.as_raw(), g.ctx.as_raw(), s.srv.as_raw(), &p));
        }

        let mut rtvs: [Option<ID3D11RenderTargetView>; 8] = Default::default();
        c.OMGetRenderTargets(Some(&mut rtvs), None);
        assert_eq!(rtvs[0].as_ref().unwrap().as_raw(), other_rtv.as_raw(), "render target not restored");
        drop(rtvs);
        let mut n = 2u32;
        let mut vps = [D3D11_VIEWPORT::default(); 2];
        c.RSGetViewports(&mut n, Some(vps.as_mut_ptr()));
        assert_eq!((n, vps[0].TopLeftX, vps[0].Width, vps[0].Height), (1, 1.0, 30.0, 20.0));
        assert_eq!(c.IAGetPrimitiveTopology(), D3D_PRIMITIVE_TOPOLOGY_POINTLIST);
        let mut srv = [None, None];
        c.PSGetShaderResources(0, Some(&mut srv));
        assert_eq!(srv[0].as_ref().unwrap().as_raw(), s.srv.as_raw(), "SRV 0 (the scene) not restored");
        assert_eq!(srv[1].as_ref().unwrap().as_raw(), wsrv.as_raw(), "SRV 1 not restored");
        drop(srv);
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
        // None of our views is left bound where the game could trip over it.
        let mut t7 = [None];
        c.PSGetShaderResources(7, Some(&mut t7));
        assert!(t7[0].is_none(), "t7 must be free after the passes (the data path binds it right after)");

        drop(h);
        let after = (refs(&other_rtv), refs(&s.srv), refs(&wsrv), refs(&other), refs(&s.tex), refs(&bs));
        assert_eq!(after, before, "reference counts changed");
        assert!(!crate::d3d::ignoring(), "ignore flag left on");
    }
}

#[test]
fn resize_recreates_the_targets() {
    let Some(g) = gpu() else { return };
    unsafe {
        let big = scene(&g, block2(ORANGE));
        let small = scene_sized(&g, 64, 64, |x, y| if (31..=32).contains(&x) && (31..=32).contains(&y) { ORANGE } else { BLACK });
        let odd = scene_sized(&g, 100, 75, |x, y| if (x, y) == (50, 40) { ORANGE } else { BLACK });
        let mut h = Halo::new(g.dev.as_raw(), blobs()).unwrap();
        let mut p = HaloParams { radius: 1.0, width: N as u32, height: N as u32 };
        assert!(h.run(g.dev.as_raw(), g.ctx.as_raw(), big.srv.as_raw(), &p));
        assert_eq!(h.size, (N as u32, N as u32));
        assert_eq!(h.glow_size(), (32, 32));
        let first = h.glow_tex();
        // Same size again: the targets are reused.
        assert!(h.run(g.dev.as_raw(), g.ctx.as_raw(), big.srv.as_raw(), &p));
        assert_eq!(h.glow_tex(), first);
        // Another window size re-creates every level.
        p.width = 64;
        p.height = 64;
        assert!(h.run(g.dev.as_raw(), g.ctx.as_raw(), small.srv.as_raw(), &p));
        assert_eq!(h.size, (64, 64));
        assert_eq!(h.glow_size(), (16, 16));
        assert_eq!(h.mean.size, (4, 4));
        // A size that is not a multiple of anything: the levels round up and the run still works.
        p.width = 100;
        p.height = 75;
        assert!(h.run(g.dev.as_raw(), g.ctx.as_raw(), odd.srv.as_raw(), &p));
        assert_eq!(h.glow_size(), (25, 19));
        assert_eq!(h.lv.iter().map(|l| l.size).collect::<Vec<_>>(), level_sizes(100, 75).1.to_vec());
        let tex = ID3D11Texture2D::from_raw_borrowed(&h.glow_tex()).unwrap().clone();
        let px = read_f16(&g, &tex, 25, 19);
        assert!(px.iter().any(|p| p[0] > 0.01), "the odd-sized run produced no glow");
        assert!(px.iter().all(|p| p.iter().all(|v| v.is_finite())));
    }
}
