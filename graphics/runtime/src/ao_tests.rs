//! WARP tests of the AO shaders and of `Ao::run`. They need no game exe: the depth is synthetic.
//!
//! Harness: D3D11 WARP device, an N x N `R32_FLOAT` depth texture filled from a closure, an N x N scene
//! colour target cleared to a constant colour, `Ao::run` through the raw vtables exactly as the hook does.
//! The synthetic depth follows the shader's own camera model (tan half fov 0.5, square aspect): a plane has
//! `1/z` linear in the screen coordinates.

use std::ffi::c_void;
use std::sync::OnceLock;

use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::Interface;

use super::*;

const N: usize = 128;
const COLOUR: [f32; 4] = [0.5, 0.375, 0.25, 0.25]; // exact in float16

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
    B.get_or_init(|| compile_blobs().unwrap_or_else(|e| panic!("ao.hlsl does not compile: {e}")))
}

fn tex(g: &Gpu, fmt: DXGI_FORMAT, bind: D3D11_BIND_FLAG, init: Option<(&[u8], u32)>) -> ID3D11Texture2D {
    let desc = D3D11_TEXTURE2D_DESC { Width: N as u32, Height: N as u32, MipLevels: 1, ArraySize: 1, Format: fmt, SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, Usage: D3D11_USAGE_DEFAULT, BindFlags: bind.0 as u32, CPUAccessFlags: 0, MiscFlags: 0 };
    let sd = init.map(|(d, pitch)| D3D11_SUBRESOURCE_DATA { pSysMem: d.as_ptr() as *const c_void, SysMemPitch: pitch, SysMemSlicePitch: 0 });
    let mut t = None;
    unsafe { g.dev.CreateTexture2D(&desc, sd.as_ref().map(|s| s as *const _), Some(&mut t)).expect("CreateTexture2D") };
    t.unwrap()
}

fn depth_tex(g: &Gpu, f: impl Fn(usize, usize) -> f32) -> ID3D11Texture2D {
    let mut v = Vec::with_capacity(N * N * 4);
    for y in 0..N {
        for x in 0..N {
            v.extend_from_slice(&f(x, y).to_le_bytes());
        }
    }
    tex(g, DXGI_FORMAT_R32_FLOAT, D3D11_BIND_SHADER_RESOURCE, Some((&v, (N * 4) as u32)))
}

/// Screen coordinates in -1..1 (y up) of the pixel centre.
fn uv(x: usize, y: usize) -> (f32, f32) {
    ((x as f32 + 0.5) / N as f32 * 2.0 - 1.0, 1.0 - (y as f32 + 0.5) / N as f32 * 2.0)
}

fn flat(_: usize, _: usize) -> f32 {
    10.0
}

/// A tilted plane: 1/z linear in u and v.
fn tilted(x: usize, y: usize) -> f32 {
    let (u, v) = uv(x, y);
    1.0 / (0.1 + 0.03 * u - 0.02 * v)
}

/// The inside of a corner seen from the front: two planes meeting in the column x = N/2, farthest there.
fn corner(x: usize, y: usize) -> f32 {
    let (u, _) = uv(x, y);
    1.0 / (1.0 / 16.0 + 0.0375 * u.abs())
}

struct Scene {
    tex: ID3D11Texture2D,
    rtv: ID3D11RenderTargetView,
}

fn scene(g: &Gpu, fmt: DXGI_FORMAT) -> Scene {
    let t = tex(g, fmt, D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE, None);
    let mut rtv = None;
    unsafe { g.dev.CreateRenderTargetView(&t, None, Some(&mut rtv)).unwrap() };
    let rtv = rtv.unwrap();
    unsafe { g.ctx.ClearRenderTargetView(&rtv, &COLOUR) };
    Scene { tex: t, rtv }
}

fn run_ao(g: &Gpu, s: &Scene, depth: &ID3D11Texture2D, strength: f32, radius: f32) {
    unsafe {
        let mut ao = Ao::new(g.dev.as_raw(), blobs()).expect("Ao::new");
        let p = AoParams { strength, radius, width: N as u32, height: N as u32 };
        assert!(ao.run(g.dev.as_raw(), g.ctx.as_raw(), s.rtv.as_raw(), depth.as_raw(), &p), "run failed");
        drop(ao);
    }
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

fn staging(g: &Gpu, t: &ID3D11Texture2D, fmt: DXGI_FORMAT) -> ID3D11Texture2D {
    let desc = D3D11_TEXTURE2D_DESC { Width: N as u32, Height: N as u32, MipLevels: 1, ArraySize: 1, Format: fmt, SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, Usage: D3D11_USAGE_STAGING, BindFlags: 0, CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32, MiscFlags: 0 };
    let mut st = None;
    unsafe {
        g.dev.CreateTexture2D(&desc, None, Some(&mut st)).unwrap();
        let st = st.unwrap();
        g.ctx.CopyResource(&st, t);
        st
    }
}

/// RGBA per pixel of a float16 scene.
fn read_f16(g: &Gpu, t: &ID3D11Texture2D) -> Vec<[f32; 4]> {
    unsafe {
        let st = staging(g, t, DXGI_FORMAT_R16G16B16A16_FLOAT);
        let mut m = D3D11_MAPPED_SUBRESOURCE::default();
        g.ctx.Map(&st, 0, D3D11_MAP_READ, 0, Some(&mut m)).unwrap();
        let mut out = Vec::with_capacity(N * N);
        for y in 0..N {
            let row = std::slice::from_raw_parts((m.pData as *const u8).add(y * m.RowPitch as usize) as *const u16, N * 4);
            for x in 0..N {
                out.push([f16_to_f32(row[x * 4]), f16_to_f32(row[x * 4 + 1]), f16_to_f32(row[x * 4 + 2]), f16_to_f32(row[x * 4 + 3])]);
            }
        }
        g.ctx.Unmap(&st, 0);
        out
    }
}

/// Red channel / 255 of a B8G8R8A8 scene (index 2 of each pixel is red).
fn read_bgra_red(g: &Gpu, t: &ID3D11Texture2D) -> Vec<f32> {
    unsafe {
        let st = staging(g, t, DXGI_FORMAT_B8G8R8A8_UNORM);
        let mut m = D3D11_MAPPED_SUBRESOURCE::default();
        g.ctx.Map(&st, 0, D3D11_MAP_READ, 0, Some(&mut m)).unwrap();
        let mut out = Vec::with_capacity(N * N);
        for y in 0..N {
            let row = std::slice::from_raw_parts((m.pData as *const u8).add(y * m.RowPitch as usize), N * 4);
            for x in 0..N {
                out.push(row[x * 4 + 2] as f32 / 255.0);
            }
        }
        g.ctx.Unmap(&st, 0);
        out
    }
}

const F16: DXGI_FORMAT = DXGI_FORMAT_R16G16B16A16_FLOAT;

fn at(px: &[[f32; 4]], x: usize, y: usize) -> [f32; 4] {
    px[y * N + x]
}

/// Lowest red value in the column band around `x` (rows 20..N-20, away from the image border).
fn min_red_near(px: &[[f32; 4]], x: usize) -> f32 {
    (20..N - 20).flat_map(|y| (x - 1..=x + 1).map(move |xx| (xx, y))).map(|(xx, y)| at(px, xx, y)[0]).fold(f32::MAX, f32::min)
}

fn max_deviation(px: &[[f32; 4]], region: impl Fn(usize, usize) -> bool) -> f32 {
    let mut d = 0.0f32;
    for y in 0..N {
        for x in 0..N {
            if region(x, y) {
                let p = at(px, x, y);
                d = d.max((0..3).map(|c| (p[c] - COLOUR[c]).abs()).fold(0.0, f32::max));
            }
        }
    }
    d
}

// ---- compile -----------------------------------------------------------------------------------------

#[test]
fn ao_hlsl_compiles_for_all_entries() {
    let b = blobs();
    for (name, code) in [("vs", &b.vs), ("ao", &b.ao), ("blur", &b.blur), ("apply", &b.apply)] {
        assert_eq!(&code[..4], b"DXBC", "{name}");
    }
}

// ---- behaviour ---------------------------------------------------------------------------------------

#[test]
fn flat_plane_is_not_darkened() {
    let Some(g) = gpu() else { return };
    for (name, f) in [("flat", flat as fn(usize, usize) -> f32), ("tilted", tilted)] {
        let s = scene(&g, F16);
        run_ao(&g, &s, &depth_tex(&g, f), 2.0, 4.0);
        let px = read_f16(&g, &s.tex);
        let d = max_deviation(&px, |_, _| true);
        println!("ao {name} plane (strength 2, radius 4): max deviation {d}");
        assert!(d < 2e-3, "{name} plane darkened by {d}");
        assert_eq!(at(&px, 10, 10)[3], COLOUR[3], "alpha is kept");
    }
}

#[test]
fn corner_is_darkened_in_the_crease_only() {
    let Some(g) = gpu() else { return };
    let s = scene(&g, F16);
    run_ao(&g, &s, &depth_tex(&g, corner), 1.0, 2.0);
    let px = read_f16(&g, &s.tex);
    let crease = min_red_near(&px, N / 2);
    let far = max_deviation(&px, |x, _| x < 24 || x >= N - 24);
    println!("ao corner (strength 1, radius 2): crease red {crease} of {}, deviation far from the crease {far}", COLOUR[0]);
    assert!(crease < COLOUR[0] * 0.93, "crease not darkened: {crease}");
    assert!(crease >= COLOUR[0] * (1.0 - MAX_DARKEN) - 1e-3, "darker than the cap: {crease}");
    assert!(far < 2e-3, "darkened away from the crease by {far}");
    // Never brighter, never below the cap, alpha untouched, rgb scaled by one factor.
    for p in &px {
        for c in 0..3 {
            assert!(p[c] <= COLOUR[c] + 1e-3 && p[c] >= COLOUR[c] * (1.0 - MAX_DARKEN) - 1e-3, "{p:?}");
        }
        assert_eq!(p[3], COLOUR[3]);
        let k = p[0] / COLOUR[0];
        assert!((p[1] / COLOUR[1] - k).abs() < 3e-3 && (p[2] / COLOUR[2] - k).abs() < 3e-3, "hue changed: {p:?}");
    }
}

#[test]
fn depth_discontinuities_do_not_halo() {
    let Some(g) = gpu() else { return };
    // A box 5 units from the camera in front of a wall 20 units away: no contact, so no darkening on
    // either side of the silhouette (the range check removes samples far in depth).
    let d = depth_tex(&g, |x, y| if (40..88).contains(&x) && (40..88).contains(&y) { 5.0 } else { 20.0 });
    let s = scene(&g, F16);
    run_ao(&g, &s, &d, 2.0, 4.0);
    let px = read_f16(&g, &s.tex);
    let dev = max_deviation(&px, |_, _| true);
    println!("ao silhouette (strength 2, radius 4): max deviation {dev}");
    assert!(dev < 2e-3, "halo of {dev}");
}

#[test]
fn strength_zero_is_the_identity() {
    let Some(g) = gpu() else { return };
    let s = scene(&g, F16);
    run_ao(&g, &s, &depth_tex(&g, corner), 0.0, 1.0);
    let px = read_f16(&g, &s.tex);
    let d = max_deviation(&px, |_, _| true);
    assert!(d < 1e-6, "strength 0 changed the scene by {d}");
}

#[test]
fn strength_and_radius_sliders_act_through_constants() {
    let Some(g) = gpu() else { return };
    let depth = depth_tex(&g, corner);
    let dark = |strength: f32, radius: f32| {
        let s = scene(&g, F16);
        run_ao(&g, &s, &depth, strength, radius);
        let px = read_f16(&g, &s.tex);
        let crease = min_red_near(&px, N / 2);
        let wide = px.iter().filter(|p| p[0] < COLOUR[0] - 0.004).count();
        (crease, wide)
    };
    let (c_half, _) = dark(0.5, 1.0);
    let (c_one, w_one) = dark(1.0, 1.0);
    let (c_two, _) = dark(2.0, 1.0);
    println!("ao strength 0.5/1/2 crease red: {c_half} {c_one} {c_two}");
    assert!(c_half > c_one && c_one > c_two, "strength must darken monotonically");
    let (_, w_small) = dark(1.0, 0.5);
    let (_, w_big) = dark(1.0, 3.0);
    println!("ao darkened pixels at radius 0.5/1/3: {w_small} {w_one} {w_big}");
    assert!(w_small < w_one && w_one < w_big, "radius must widen the darkened area");
}

#[test]
fn eight_bit_scene_is_darkened_too() {
    let Some(g) = gpu() else { return };
    let s = scene(&g, DXGI_FORMAT_B8G8R8A8_UNORM);
    run_ao(&g, &s, &depth_tex(&g, corner), 1.0, 2.0);
    let red = read_bgra_red(&g, &s.tex);
    let crease = (20..N - 20).map(|y| red[y * N + N / 2]).fold(f32::MAX, f32::min);
    let far = red[10 * N + 5];
    println!("ao 8-bit: crease red {crease}, far red {far}");
    assert!(crease < COLOUR[0] * 0.95, "{crease}");
    assert!((far - COLOUR[0]).abs() < 2.0 / 255.0, "{far}");
}

#[test]
fn invalid_depth_is_left_alone() {
    let Some(g) = gpu() else { return };
    // Left third: sky (0), middle: NaN, right: the corner. Only the right part may change, and not next to the invalid area.
    let d = depth_tex(&g, |x, y| if x < N / 3 { 0.0 } else if x < 2 * N / 3 { f32::NAN } else { corner(x, y) });
    let s = scene(&g, F16);
    run_ao(&g, &s, &d, 2.0, 2.0);
    let px = read_f16(&g, &s.tex);
    let dev = max_deviation(&px, |x, _| x < 2 * N / 3);
    assert!(dev < 1e-6, "invalid depth was darkened by {dev}");
    assert!(px.iter().all(|p| p.iter().all(|v| v.is_finite())), "NaN or inf reached the scene");
    // Infinite depth (far plane) too.
    let d = depth_tex(&g, |x, y| if x < N / 2 { f32::INFINITY } else { corner(x, y) });
    let s = scene(&g, F16);
    run_ao(&g, &s, &d, 2.0, 2.0);
    let px = read_f16(&g, &s.tex);
    assert!(max_deviation(&px, |x, _| x < N / 2) < 1e-6);
    assert!(px.iter().all(|p| p.iter().all(|v| v.is_finite())));
}

#[test]
fn depth_unit_does_not_matter() {
    let Some(g) = gpu() else { return };
    // The same corner in "metres" and scaled to 0..1: same picture (scale invariance).
    let a = scene(&g, F16);
    run_ao(&g, &a, &depth_tex(&g, corner), 1.0, 1.0);
    let b = scene(&g, F16);
    run_ao(&g, &b, &depth_tex(&g, |x, y| corner(x, y) / 1000.0), 1.0, 1.0);
    let (pa, pb) = (read_f16(&g, &a.tex), read_f16(&g, &b.tex));
    let d = pa.iter().zip(&pb).map(|(p, q)| (p[0] - q[0]).abs()).fold(0.0, f32::max);
    println!("ao depth scale 1 vs 1/1000: max diff {d}");
    assert!(d < 5e-3, "{d}");
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
        let s = scene(&g, F16);
        let depth = depth_tex(&g, corner);
        // Sentinel state the AO must restore.
        let other = tex(&g, F16, D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE, None);
        let mut other_rtv = None;
        g.dev.CreateRenderTargetView(&other, None, Some(&mut other_rtv)).unwrap();
        let other_rtv = other_rtv.unwrap();
        let sampled: Vec<ID3D11ShaderResourceView> = (0..2)
            .map(|_| {
                let t = tex(&g, F16, D3D11_BIND_SHADER_RESOURCE, None);
                let mut v = None;
                g.dev.CreateShaderResourceView(&t, None, Some(&mut v)).unwrap();
                v.unwrap()
            })
            .collect();
        let c = &g.ctx;
        let vp = D3D11_VIEWPORT { TopLeftX: 1.0, TopLeftY: 2.0, Width: 30.0, Height: 20.0, MinDepth: 0.0, MaxDepth: 1.0 };
        c.OMSetRenderTargets(Some(&[Some(other_rtv.clone())]), None);
        c.RSSetViewports(Some(&[vp]));
        c.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_POINTLIST);
        c.PSSetShaderResources(0, Some(&[Some(sampled[0].clone()), Some(sampled[1].clone())]));
        let mut bd = D3D11_BLEND_DESC::default();
        bd.RenderTarget[0].RenderTargetWriteMask = 0xf;
        let mut bs = None;
        g.dev.CreateBlendState(&bd, Some(&mut bs)).unwrap();
        let bs = bs.unwrap();
        c.OMSetBlendState(&bs, Some(&[0.1, 0.2, 0.3, 0.4]), 0x0f0f);
        let before = (refs(&other_rtv), refs(&sampled[0]), refs(&sampled[1]), refs(&other), refs(&s.rtv), refs(&depth), refs(&bs));

        let p = AoParams { strength: 1.0, radius: 1.0, width: N as u32, height: N as u32 };
        let mut ao = Ao::new(g.dev.as_raw(), blobs()).unwrap();
        for _ in 0..2 {
            assert!(ao.run(g.dev.as_raw(), g.ctx.as_raw(), s.rtv.as_raw(), depth.as_raw(), &p));
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
        assert_eq!(srv[0].as_ref().unwrap().as_raw(), sampled[0].as_raw(), "SRV 0 not restored");
        assert_eq!(srv[1].as_ref().unwrap().as_raw(), sampled[1].as_raw(), "SRV 1 not restored");
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

        drop(ao);
        let after = (refs(&other_rtv), refs(&sampled[0]), refs(&sampled[1]), refs(&other), refs(&s.rtv), refs(&depth), refs(&bs));
        assert_eq!(after, before, "reference counts changed");
        assert!(!crate::d3d::ignoring(), "ignore flag left on");
    }
}

#[test]
fn resize_recreates_the_targets() {
    let Some(g) = gpu() else { return };
    unsafe {
        let s = scene(&g, F16);
        let depth = depth_tex(&g, corner);
        let mut ao = Ao::new(g.dev.as_raw(), blobs()).unwrap();
        let mut p = AoParams { strength: 1.0, radius: 1.0, width: N as u32, height: N as u32 };
        assert!(ao.run(g.dev.as_raw(), g.ctx.as_raw(), s.rtv.as_raw(), depth.as_raw(), &p));
        assert_eq!(ao.size, (N as u32, N as u32));
        // A smaller viewport (another window size) re-creates the targets; the draw covers the top-left part.
        p.width = 64;
        p.height = 64;
        assert!(ao.run(g.dev.as_raw(), g.ctx.as_raw(), s.rtv.as_raw(), depth.as_raw(), &p));
        assert_eq!(ao.size, (64, 64));
    }
}

/// R16_FLOAT texture of N x N as floats.
fn read_r16(g: &Gpu, t: &ID3D11Texture2D) -> Vec<f32> {
    unsafe {
        let st = staging(g, t, DXGI_FORMAT_R16_FLOAT);
        let mut m = D3D11_MAPPED_SUBRESOURCE::default();
        g.ctx.Map(&st, 0, D3D11_MAP_READ, 0, Some(&mut m)).unwrap();
        let mut out = Vec::with_capacity(N * N);
        for y in 0..N {
            let row = std::slice::from_raw_parts((m.pData as *const u8).add(y * m.RowPitch as usize) as *const u16, N);
            for x in 0..N {
                out.push(f16_to_f32(row[x]));
            }
        }
        g.ctx.Unmap(&st, 0);
        out
    }
}

#[test]
fn run_visibility_makes_the_finished_visibility_and_leaves_the_scene_alone() {
    let Some(g) = gpu() else { return };
    unsafe {
        let s = scene(&g, F16);
        let mut ao = Ao::new(g.dev.as_raw(), blobs()).expect("Ao::new");
        let p = AoParams { strength: 1.0, radius: 2.0, width: N as u32, height: N as u32 };

        // A flat wall: nothing occludes anything.
        let flat_depth = depth_tex(&g, flat);
        assert!(ao.run_visibility(g.dev.as_raw(), g.ctx.as_raw(), flat_depth.as_raw(), &p), "run_visibility failed");
        assert!(!ao.vis_srv().is_null(), "no visibility view");
        let vis = read_r16(&g, ID3D11Texture2D::from_raw_borrowed(&ao.tex[2]).expect("visibility texture"));
        let lowest = vis.iter().cloned().fold(f32::MAX, f32::min);
        assert!(lowest > 0.99, "a flat wall must stay open, lowest visibility {lowest}");

        // A corner: the crease is occluded, the walls away from it are not.
        let corner_depth = depth_tex(&g, corner);
        assert!(ao.run_visibility(g.dev.as_raw(), g.ctx.as_raw(), corner_depth.as_raw(), &p));
        let vis = read_r16(&g, ID3D11Texture2D::from_raw_borrowed(&ao.tex[2]).expect("visibility texture"));
        let crease = (20..N - 20).map(|y| vis[y * N + N / 2]).fold(f32::MAX, f32::min);
        let away = (20..N - 20).map(|y| vis[y * N + 8]).fold(f32::MAX, f32::min);
        assert!(crease < 0.9, "the crease must be occluded, visibility {crease}");
        assert!(away > 0.99, "the wall far from the crease must stay open, visibility {away}");

        // Nothing was drawn into the scene.
        let px = read_f16(&g, &s.tex);
        assert!(px.iter().all(|p| *p == COLOUR), "run_visibility must not touch the scene colour");
        drop(ao);
    }
}
