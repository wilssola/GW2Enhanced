//! WARP tests of the SSR shaders, of `Ssr::run` and of the light-adjust t8/b7 block. They need no game exe:
//! depth and scene colour are synthetic. The depth follows the shader's camera model (tan half fov 0.5, square
//! aspect, y up): the floor is the plane y = -1, so its depth is z = 2 / (-v) at screen height v (-1..1, up).

use std::ffi::c_void;
use std::sync::OnceLock;

use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::Interface;

use super::*;

const N: usize = 128;
const H: usize = N / 2; // the reflection targets are half size

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
    B.get_or_init(|| compile_blobs().unwrap_or_else(|e| panic!("ssr.hlsl does not compile: {e}")))
}

fn tex(g: &Gpu, w: usize, h: usize, fmt: DXGI_FORMAT, bind: D3D11_BIND_FLAG, init: Option<(&[u8], u32)>) -> ID3D11Texture2D {
    let desc = D3D11_TEXTURE2D_DESC { Width: w as u32, Height: h as u32, MipLevels: 1, ArraySize: 1, Format: fmt, SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, Usage: D3D11_USAGE_DEFAULT, BindFlags: bind.0 as u32, CPUAccessFlags: 0, MiscFlags: 0 };
    let sd = init.map(|(d, pitch)| D3D11_SUBRESOURCE_DATA { pSysMem: d.as_ptr() as *const c_void, SysMemPitch: pitch, SysMemSlicePitch: 0 });
    let mut t = None;
    unsafe { g.dev.CreateTexture2D(&desc, sd.as_ref().map(|s| s as *const _), Some(&mut t)).expect("CreateTexture2D") };
    t.unwrap()
}

fn depth_tex(g: &Gpu, f: &dyn Fn(usize, usize) -> f32) -> ID3D11Texture2D {
    let mut v = Vec::with_capacity(N * N * 4);
    for y in 0..N {
        for x in 0..N {
            v.extend_from_slice(&f(x, y).to_le_bytes());
        }
    }
    tex(g, N, N, DXGI_FORMAT_R32_FLOAT, D3D11_BIND_SHADER_RESOURCE, Some((&v, (N * 4) as u32)))
}

/// Scene colour (gamma encoded) as R32G32B32A32_FLOAT, and a view of it.
fn scene_srv(g: &Gpu, f: &dyn Fn(usize, usize) -> [f32; 3]) -> (ID3D11Texture2D, ID3D11ShaderResourceView) {
    scene_srv_a(g, &|x, y| {
        let c = f(x, y);
        [c[0], c[1], c[2], 1.0]
    })
}

/// The same with an alpha channel (the engine's raw material alpha).
fn scene_srv_a(g: &Gpu, f: &dyn Fn(usize, usize) -> [f32; 4]) -> (ID3D11Texture2D, ID3D11ShaderResourceView) {
    let mut v = Vec::with_capacity(N * N * 16);
    for y in 0..N {
        for x in 0..N {
            let c = f(x, y);
            for k in c {
                v.extend_from_slice(&k.to_le_bytes());
            }
        }
    }
    let t = tex(g, N, N, DXGI_FORMAT_R32G32B32A32_FLOAT, D3D11_BIND_SHADER_RESOURCE, Some((&v, (N * 16) as u32)));
    let mut s = None;
    unsafe { g.dev.CreateShaderResourceView(&t, None, Some(&mut s)).unwrap() };
    (t, s.unwrap())
}

/// Screen coordinates in -1..1 (y up) of the full-resolution pixel centre.
fn uv(x: usize, y: usize) -> (f32, f32) {
    ((x as f32 + 0.5) / N as f32 * 2.0 - 1.0, 1.0 - (y as f32 + 0.5) / N as f32 * 2.0)
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

/// RGBA of a half-size float16 target.
fn read_half(g: &Gpu, t: &ID3D11Texture2D, w: usize, h: usize) -> Vec<[f32; 4]> {
    unsafe {
        let desc = D3D11_TEXTURE2D_DESC { Width: w as u32, Height: h as u32, MipLevels: 1, ArraySize: 1, Format: DXGI_FORMAT_R16G16B16A16_FLOAT, SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, Usage: D3D11_USAGE_STAGING, BindFlags: 0, CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32, MiscFlags: 0 };
        let mut st = None;
        g.dev.CreateTexture2D(&desc, None, Some(&mut st)).unwrap();
        let st = st.unwrap();
        g.ctx.CopyResource(&st, t);
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

/// Runs the SSR on a synthetic frame and returns the finished half-size reflection texture.
/// The old behaviour: all classes allowed, the smart heuristics off (alpha 1).
fn trace(g: &Gpu, depth: &dyn Fn(usize, usize) -> f32, scene: &dyn Fn(usize, usize) -> [f32; 3], range: f32) -> Vec<[f32; 4]> {
    trace_ex(g, depth, &|x, y| {
        let c = scene(x, y);
        [c[0], c[1], c[2], 1.0]
    }, range, CLASSES_ALL, 0.0)
}

fn trace_ex(g: &Gpu, depth: &dyn Fn(usize, usize) -> f32, scene: &dyn Fn(usize, usize) -> [f32; 4], range: f32, classes: u32, smart: f32) -> Vec<[f32; 4]> {
    trace_full(g, depth, scene, range, classes, smart, 1.0, 1.0, false)
}

#[allow(clippy::too_many_arguments)]
fn trace_full(g: &Gpu, depth: &dyn Fn(usize, usize) -> f32, scene: &dyn Fn(usize, usize) -> [f32; 4], range: f32, classes: u32, smart: f32, hue_filter: f32, flatness: f32, debug_reflectivity: bool) -> Vec<[f32; 4]> {
    unsafe {
        let d = depth_tex(g, depth);
        let (_t, s) = scene_srv_a(g, scene);
        let mut ssr = Ssr::new(g.dev.as_raw(), blobs()).expect("Ssr::new");
        let p = SsrParams { range, width: N as u32, height: N as u32, classes, smart, hue_filter, flatness, debug_reflectivity, water_boost: 0.0, water_depth_look: 0.0, matte_cut: 0.0, stats: false };
        assert!(ssr.run(g.dev.as_raw(), g.ctx.as_raw(), d.as_raw(), s.as_raw(), &p, 1.0), "run failed");
        let out = read_half(g, ID3D11Texture2D::from_raw_borrowed(&ssr.tex[0]).expect("target"), H, H);
        drop(ssr);
        out
    }
}

// ---- the synthetic world -----------------------------------------------------------------------------

const OBJ_Z: f32 = 8.0;
/// Floor depth at screen height v (negative = below the horizon).
fn floor_z(v: f32) -> f32 {
    2.0 / -v
}
const FAR_WALL: f32 = 40.0;

fn in_object(u: f32, v: f32) -> bool {
    u.abs() < 0.3 && (-0.25..=0.5).contains(&v)
}

/// A flat floor with a vertical object standing on it (its base at v = -0.25, face at z = 8) in front of a far wall.
fn world_depth(x: usize, y: usize) -> f32 {
    let (u, v) = uv(x, y);
    if in_object(u, v) {
        OBJ_Z
    } else if v < -0.05 {
        floor_z(v)
    } else {
        FAR_WALL
    }
}

/// The object face is a vertical gradient (so the mirrored position can be read back); the floor is dim.
fn obj_colour(v: f32) -> f32 {
    0.4 + 0.5 * (v + 0.25) / 0.75
}

fn world_scene(x: usize, y: usize) -> [f32; 3] {
    let (u, v) = uv(x, y);
    if in_object(u, v) {
        let c = obj_colour(v);
        [c, c, c]
    } else if v < -0.05 {
        [0.2, 0.2, 0.2]
    } else {
        [0.3, 0.3, 0.3]
    }
}

/// The half-size pixel that covers the full-resolution pixel centre at screen position (u, v).
fn half_px(u: f32, v: f32) -> (usize, usize) {
    let x = ((u + 1.0) * 0.5 * N as f32 - 0.5).round() as usize;
    let y = ((1.0 - v) * 0.5 * N as f32 - 0.5).round() as usize;
    (x / 2, y / 2)
}

/// (weight, recovered encoded colour) of the reflection at the half pixel.
fn refl_at(px: &[[f32; 4]], hx: usize, hy: usize) -> (f32, f32) {
    let p = px[hy * H + hx];
    let a = p[3];
    (a, if a > 1e-5 { (p[0] / a).max(0.0).sqrt() } else { 0.0 })
}

// ---- compile -----------------------------------------------------------------------------------------

#[test]
fn ssr_hlsl_compiles_for_all_entries() {
    let b = blobs();
    for (name, code) in [("vs", &b.vs), ("trace", &b.trace), ("blur_h", &b.blur_h), ("blur_v", &b.blur_v), ("mean", &b.mean), ("stats", &b.stats)] {
        assert_eq!(&code[..4], b"DXBC", "{name}");
    }
}

// ---- behaviour ---------------------------------------------------------------------------------------

#[test]
fn a_floor_reflects_the_object_at_the_mirrored_screen_position() {
    let Some(g) = gpu() else { return };
    let px = trace(&g, &world_depth, &world_scene, 2.0);
    // The floor point at screen height v_m shows the object point at v_o = -0.5 - v_m (the plane y = -1 mirrors
    // the object's base at v = -0.25 onto itself). The gradient colour tells which object height was reflected.
    for v_m in [-0.45f32, -0.6, -0.75] {
        let (hx, hy) = half_px(0.0, v_m);
        let (a, c) = refl_at(&px, hx, hy);
        let v_o = -0.5 - v_m;
        let v_est = (c - 0.4) / 0.5 * 0.75 - 0.25;
        println!("ssr mirror: floor v {v_m}: weight {a:.4}, reflected object height v {v_est:.3} (expected {v_o:.3})");
        assert!(a > 0.01, "no reflection at v {v_m}: weight {a}");
        assert!((v_est - v_o).abs() < 0.08, "reflected the wrong place: v {v_est} instead of {v_o}");
    }
    // Floor outside the object's columns reflects the far wall (dim, 0.3), not the object's gradient.
    let (hx, hy) = half_px(0.8, -0.6);
    let (a, c) = refl_at(&px, hx, hy);
    println!("ssr beside the object: weight {a:.4}, colour {c:.3}");
    assert!(a < 0.01 || (c - 0.3).abs() < 0.05, "beside the object the floor must not show the object: weight {a}, colour {c}");
}

#[test]
fn a_vertical_wall_gets_no_reflection() {
    let Some(g) = gpu() else { return };
    let px = trace(&g, &world_depth, &world_scene, 2.0);
    // The object's face (interior) and the far wall (interior).
    for (u, v) in [(0.0, 0.1), (0.15, 0.3), (-0.15, 0.0), (0.0, 0.4), (0.7, 0.3), (-0.7, 0.8), (0.0, 0.9)] {
        let (hx, hy) = half_px(u, v);
        let a = px[hy * H + hx][3];
        assert!(a < 1e-3, "wall at ({u}, {v}) got a reflection: weight {a}");
    }
}

#[test]
fn a_floor_with_nothing_above_reflects_the_sky_only_and_faintly() {
    let Some(g) = gpu() else { return };
    // Floor below the horizon, invalid depth (sky) above.
    let depth = |x: usize, y: usize| {
        let (_, v) = uv(x, y);
        if v < -0.05 { floor_z(v) } else { 0.0 }
    };
    let bright_sky = |x: usize, y: usize| {
        let (_, v) = uv(x, y);
        if v < -0.05 { [0.2; 3] } else { [0.8, 0.9, 1.0] }
    };
    let px = trace(&g, &depth, &bright_sky, 1.0);
    let (hx, hy) = half_px(0.0, -0.4);
    let (a, c) = refl_at(&px, hx, hy);
    println!("ssr sky: weight {a:.4}, colour {c:.3}");
    assert!(a > 0.002 && a <= SKY_WEIGHT + 1e-3, "sky reflection weight {a}");
    assert!((c - 0.9).abs() < 0.15, "the sky colour is reflected, got {c}");
    // The sky itself is not a surface.
    let (hx, hy) = half_px(0.0, 0.5);
    assert!(px[hy * H + hx][3] < 1e-4);
    // A black sky reflects nothing.
    let dark = |x: usize, y: usize| {
        let (_, v) = uv(x, y);
        if v < -0.05 { [0.2; 3] } else { [0.0; 3] }
    };
    let px = trace(&g, &depth, &dark, 1.0);
    let (hx, hy) = half_px(0.0, -0.4);
    assert!(px[hy * H + hx][..3].iter().all(|v| v.abs() < 1e-4), "black sky reflected light: {:?}", px[hy * H + hx]);
}

#[test]
fn a_ceiling_and_a_tilted_wall_get_nothing() {
    let Some(g) = gpu() else { return };
    // A ceiling (plane y = +1) over the whole upper screen, bright; a wall tilted 60 degrees from vertical is
    // not horizontal enough either. Only the up component of the normal counts.
    let ceiling = |x: usize, y: usize| {
        let (_, v) = uv(x, y);
        if v > 0.05 { 2.0 / v } else { floor_z(v.min(-0.05)) }
    };
    let px = trace(&g, &ceiling, &|_, _| [0.9; 3], 2.0);
    let (hx, hy) = half_px(0.0, 0.5);
    assert!(px[hy * H + hx][3] < 1e-3, "a ceiling must not reflect");
    // Tilted wall: z depends on u (a plane x = z*k): 1/z linear in u, v-independent: normal in the x-z plane.
    let tilted = |x: usize, y: usize| {
        let (u, _) = uv(x, y);
        1.0 / (0.1 + 0.03 * u + 0.0 * y as f32)
    };
    let px = trace(&g, &tilted, &|_, _| [0.9; 3], 2.0);
    assert!(px.iter().all(|p| p[3] < 1e-3), "a wall must not reflect");
}

#[test]
fn rays_that_leave_the_screen_fade_out_instead_of_snapping() {
    let Some(g) = gpu() else { return };
    // A far wall covers everything above the floor, bright. Floor rows near the camera send their rays out of the
    // top of the screen (no hit); rows further away hit the wall below the top edge. The weight must go down
    // smoothly towards the rows whose rays leave the screen.
    let depth = |x: usize, y: usize| {
        let (_, v) = uv(x, y);
        if v < -0.05 { floor_z(v) } else { FAR_WALL }
    };
    let px = trace(&g, &depth, &|_, _| [0.8; 3], 2.0);
    let col = H / 2;
    let a: Vec<f32> = (0..H).map(|y| px[y * H + col][3]).collect();
    // Floor rows: from v = -0.05 (half row ~ 33) down to the bottom (63).
    let rows: Vec<f32> = (34..H - 1).map(|y| a[y]).collect();
    println!("ssr edge fade, weight per half row 34..: {rows:.3?}");
    let peak = rows.iter().cloned().fold(0.0, f32::max);
    assert!(peak > 0.01, "the test scene reflected nothing at all");
    let mut max_step = 0.0f32;
    for w in rows.windows(2) {
        max_step = max_step.max((w[1] - w[0]).abs());
    }
    assert!(max_step < 0.35 * peak, "the weight snaps: biggest step between rows {max_step} of a peak {peak}");
    let nearest = rows[rows.len() - 3..].iter().cloned().fold(0.0, f32::max);
    assert!(nearest < 0.1 * peak, "rays that leave the screen must have faded out, nearest rows weigh {nearest} of a peak {peak}");
}

#[test]
fn range_slider_lengthens_the_rays() {
    let Some(g) = gpu() else { return };
    // The object is far from the floor row at v = -0.75 (depth 2.67): a short ray does not reach it, a long one does.
    let (hx, hy) = half_px(0.0, -0.75);
    let short = trace(&g, &world_depth, &world_scene, 0.25)[hy * H + hx][3];
    let long = trace(&g, &world_depth, &world_scene, 2.0)[hy * H + hx][3];
    println!("ssr range 0.25: weight {short:.4}, range 2: weight {long:.4}");
    assert!(short < 1e-3 && long > 0.01, "range must decide whether the far object is reached ({short} vs {long})");
}

// ---- pipeline state, references, resize ---------------------------------------------------------------

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
        let d = depth_tex(&g, &world_depth);
        let (scene_t, scene_s) = scene_srv(&g, &world_scene);
        let other = tex(&g, N, N, DXGI_FORMAT_R16G16B16A16_FLOAT, D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE, None);
        let mut other_rtv = None;
        g.dev.CreateRenderTargetView(&other, None, Some(&mut other_rtv)).unwrap();
        let other_rtv = other_rtv.unwrap();
        let sampled: Vec<ID3D11ShaderResourceView> = (0..2)
            .map(|_| {
                let t = tex(&g, N, N, DXGI_FORMAT_R16G16B16A16_FLOAT, D3D11_BIND_SHADER_RESOURCE, None);
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
        let before = (refs(&other_rtv), refs(&sampled[0]), refs(&sampled[1]), refs(&other), refs(&d), refs(&scene_s), refs(&scene_t), refs(&bs));

        let p = SsrParams { range: 1.0, width: N as u32, height: N as u32, classes: CLASSES_ALL, smart: 0.0, hue_filter: 1.0, flatness: 1.0, debug_reflectivity: false, water_boost: 0.0, water_depth_look: 0.0, matte_cut: 0.0, stats: false };
        let mut ssr = Ssr::new(g.dev.as_raw(), blobs()).unwrap();
        for _ in 0..2 {
            assert!(ssr.run(g.dev.as_raw(), g.ctx.as_raw(), d.as_raw(), scene_s.as_raw(), &p, 1.0));
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
        // t8 / b7 belong to the glue, not to run(): untouched here.
        let mut t8 = [None];
        c.PSGetShaderResources(8, Some(&mut t8));
        assert!(t8[0].is_none(), "run() must not bind t8");

        drop(ssr);
        let after = (refs(&other_rtv), refs(&sampled[0]), refs(&sampled[1]), refs(&other), refs(&d), refs(&scene_s), refs(&scene_t), refs(&bs));
        assert_eq!(after, before, "reference counts changed");
        assert!(!crate::d3d::ignoring(), "ignore flag left on");
    }
}

#[test]
fn resize_recreates_the_targets() {
    let Some(g) = gpu() else { return };
    unsafe {
        let d = depth_tex(&g, &world_depth);
        let (_t, s) = scene_srv(&g, &world_scene);
        let mut ssr = Ssr::new(g.dev.as_raw(), blobs()).unwrap();
        let mut p = SsrParams { range: 1.0, width: N as u32, height: N as u32, classes: CLASSES_ALL, smart: 0.0, hue_filter: 1.0, flatness: 1.0, debug_reflectivity: false, water_boost: 0.0, water_depth_look: 0.0, matte_cut: 0.0, stats: false };
        assert!(ssr.run(g.dev.as_raw(), g.ctx.as_raw(), d.as_raw(), s.as_raw(), &p, 1.0));
        assert_eq!(ssr.size, (64, 64));
        let first = ssr.tex[0];
        // Same size: the targets are kept.
        assert!(ssr.run(g.dev.as_raw(), g.ctx.as_raw(), d.as_raw(), s.as_raw(), &p, 1.0));
        assert_eq!(ssr.tex[0], first);
        // Another window size re-creates them (odd sizes round up).
        p.width = 61;
        p.height = 37;
        assert!(ssr.run(g.dev.as_raw(), g.ctx.as_raw(), d.as_raw(), s.as_raw(), &p, 1.0));
        assert_eq!(ssr.size, (31, 19));
        assert!(!ssr.result_srv().is_null());
    }
}

// ---- the light-adjust block (t8 / b7) -------------------------------------------------------------------

const VS_LIGHT: &str = "struct O { float4 pos : SV_POSITION; float4 uv : TEXCOORD0; };
O main(uint id : SV_VertexID) { float2 p = float2((id << 1) & 2, id & 2); O o;
  o.pos = float4(p * float2(2, -2) + float2(-1, 1), 0, 1); o.uv = float4(p, 0, 0); return o; }";

fn f32_tex(g: &Gpu, w: usize, h: usize, v: [f32; 4]) -> ID3D11ShaderResourceView {
    let bytes: Vec<u8> = (0..w * h).flat_map(|_| v.iter().flat_map(|c| c.to_le_bytes())).collect();
    let t = tex(g, w, h, DXGI_FORMAT_R32G32B32A32_FLOAT, D3D11_BIND_SHADER_RESOURCE, Some((&bytes, (w * 16) as u32)));
    let mut s = None;
    unsafe { g.dev.CreateShaderResourceView(&t, None, Some(&mut s)).unwrap() };
    s.unwrap()
}

fn cbuf(g: &Gpu, v: [f32; 4]) -> ID3D11Buffer {
    let b: Vec<u8> = v.iter().flat_map(|x| x.to_le_bytes()).collect();
    let d = D3D11_BUFFER_DESC { ByteWidth: 16, Usage: D3D11_USAGE_IMMUTABLE, BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32, ..Default::default() };
    let sd = D3D11_SUBRESOURCE_DATA { pSysMem: b.as_ptr() as *const c_void, SysMemPitch: 0, SysMemSlicePitch: 0 };
    let mut o = None;
    unsafe { g.dev.CreateBuffer(&d, Some(&sd), Some(&mut o)).unwrap() };
    o.unwrap()
}

/// Runs the light-adjust substitute over a uniform colour (the pixel at the centre is returned). `refl` = the
/// colour of a 1x1 texture bound as t8 and the float4 bound as b7; `b6` = the AO / boost block.
fn run_light(g: &Gpu, scene: [f32; 3], refl: Option<([f32; 4], [f32; 4])>, b6: Option<[f32; 4]>) -> [f32; 4] {
    run_light_a(g, scene, 1.0, refl, b6)
}

/// The same with the alpha of the scene colour (the raw material alpha).
fn run_light_a(g: &Gpu, scene: [f32; 3], alpha: f32, refl: Option<([f32; 4], [f32; 4])>, b6: Option<[f32; 4]>) -> [f32; 4] {
    unsafe {
        let ps = crate::shaders::compile_ps(crate::hdr_shaders::LIGHT_ADJUST_HLSL, crate::hdr_shaders::LIGHT_ADJUST_ENTRY).unwrap_or_else(|e| panic!("{e}"));
        let vs = crate::shaders::compile(VS_LIGHT, "main", "vs_5_0").unwrap();
        let (mut v, mut p) = (None, None);
        g.dev.CreateVertexShader(&vs, None, Some(&mut v)).unwrap();
        g.dev.CreatePixelShader(&ps, None, Some(&mut p)).unwrap();
        let t0 = f32_tex(g, 8, 8, [scene[0], scene[1], scene[2], alpha]);
        let t1 = f32_tex(g, 1, 1, [0.5, 0.3, 0.2, 0.5]);
        let sd = D3D11_SAMPLER_DESC { Filter: D3D11_FILTER_MIN_MAG_MIP_POINT, AddressU: D3D11_TEXTURE_ADDRESS_CLAMP, AddressV: D3D11_TEXTURE_ADDRESS_CLAMP, AddressW: D3D11_TEXTURE_ADDRESS_CLAMP, MaxAnisotropy: 1, ComparisonFunc: D3D11_COMPARISON_NEVER, MaxLOD: f32::MAX, ..Default::default() };
        let mut smp = None;
        g.dev.CreateSamplerState(&sd, Some(&mut smp)).unwrap();
        let cb0 = {
            let b: Vec<u8> = [0.9f32, 0.0, 0.0, 0.0, 0.7, 0.0, 0.0, 0.0].iter().flat_map(|x| x.to_le_bytes()).collect();
            let d = D3D11_BUFFER_DESC { ByteWidth: 32, Usage: D3D11_USAGE_IMMUTABLE, BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32, ..Default::default() };
            let s = D3D11_SUBRESOURCE_DATA { pSysMem: b.as_ptr() as *const c_void, SysMemPitch: 0, SysMemSlicePitch: 0 };
            let mut o = None;
            g.dev.CreateBuffer(&d, Some(&s), Some(&mut o)).unwrap();
            o.unwrap()
        };
        let rt = tex(g, 8, 8, DXGI_FORMAT_R16G16B16A16_FLOAT, D3D11_BIND_RENDER_TARGET, None);
        let mut rtv = None;
        g.dev.CreateRenderTargetView(&rt, None, Some(&mut rtv)).unwrap();
        let c = &g.ctx;
        c.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
        c.VSSetShader(v.as_ref(), None);
        c.PSSetShader(p.as_ref(), None);
        c.PSSetShaderResources(0, Some(&[Some(t0), Some(t1)]));
        c.PSSetSamplers(0, Some(&[smp.clone(), smp]));
        c.PSSetConstantBuffers(0, Some(&[Some(cb0)]));
        c.OMSetRenderTargets(Some(&[rtv]), None);
        c.RSSetViewports(Some(&[D3D11_VIEWPORT { TopLeftX: 0.0, TopLeftY: 0.0, Width: 8.0, Height: 8.0, MinDepth: 0.0, MaxDepth: 1.0 }]));
        if let Some(b) = b6 {
            c.PSSetConstantBuffers(6, Some(&[Some(cbuf(g, b))]));
        }
        if let Some((col, sp)) = refl {
            c.PSSetShaderResources(8, Some(&[Some(f32_tex(g, 1, 1, col))]));
            c.PSSetConstantBuffers(7, Some(&[Some(cbuf(g, sp))]));
        }
        c.Draw(3, 0);
        let desc = D3D11_TEXTURE2D_DESC { Width: 8, Height: 8, MipLevels: 1, ArraySize: 1, Format: DXGI_FORMAT_R16G16B16A16_FLOAT, SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, Usage: D3D11_USAGE_STAGING, BindFlags: 0, CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32, MiscFlags: 0 };
        let mut st = None;
        g.dev.CreateTexture2D(&desc, None, Some(&mut st)).unwrap();
        let st = st.unwrap();
        c.CopyResource(&st, &rt);
        let mut m = D3D11_MAPPED_SUBRESOURCE::default();
        c.Map(&st, 0, D3D11_MAP_READ, 0, Some(&mut m)).unwrap();
        let row = std::slice::from_raw_parts((m.pData as *const u8).add(4 * m.RowPitch as usize) as *const u16, 8 * 4);
        let out = [f16_to_f32(row[16]), f16_to_f32(row[17]), f16_to_f32(row[18]), f16_to_f32(row[19])];
        c.Unmap(&st, 0);
        c.OMSetRenderTargets(None, None);
        c.PSSetShaderResources(8, Some(&[None]));
        c.PSSetConstantBuffers(7, Some(&[None]));
        c.PSSetConstantBuffers(6, Some(&[None]));
        out
    }
}

#[test]
fn light_adjust_strength_zero_and_unbound_t8_are_identity() {
    let Some(g) = gpu() else { return };
    let scene = [0.5, 0.4, 0.3];
    let plain = run_light(&g, scene, None, None);
    let zero = run_light(&g, scene, Some(([0.3, 0.2, 0.1, 1.0], [0.0, 0.0, 0.0, 0.0])), None);
    let unbound_cb = run_light(&g, scene, None, Some([1.0, 0.0, 0.0, 0.0]));
    assert_eq!(plain, zero, "strength 0 must be the identity");
    assert_eq!(plain, unbound_cb, "unbound t8 / b7 must be the identity");
    let on = run_light(&g, scene, Some(([0.3, 0.2, 0.1, 1.0], [1.0, 0.0, 0.0, 0.0])), None);
    println!("light-adjust: plain {plain:?}, with reflection {on:?}");
    for i in 0..3 {
        assert!(on[i] > plain[i] + 0.01, "the reflection must add light (ch {i}: {} vs {})", on[i], plain[i]);
    }
    // Linear in the strength: half strength adds less than full strength.
    let half = run_light(&g, scene, Some(([0.3, 0.2, 0.1, 1.0], [0.5, 0.0, 0.0, 0.0])), None);
    assert!(half[0] > plain[0] && half[0] < on[0]);
}

#[test]
fn light_adjust_debug_view_tints_reflected_pixels_magenta() {
    let Some(g) = gpu() else { return };
    let scene = [0.5, 0.4, 0.3];
    let dbg = run_light(&g, scene, None, Some([1.0, 0.0, 0.0, 1.0]));
    let tinted = run_light(&g, scene, Some(([0.3, 0.2, 0.1, 1.0], [1.0, 0.0, 0.0, 0.0])), Some([1.0, 0.0, 0.0, 1.0]));
    assert!(tinted[2] > tinted[1] && tinted[0] > tinted[1], "reflected pixel is not magenta: {tinted:?}");
    assert!(dbg[1] > 0.0 && (dbg[0] - dbg[1]).abs() < 0.3, "a pixel without reflection keeps its colour in the debug view: {dbg:?}");
}

// ---- materials: class gating, vegetation and roughness rejection, debug level 3 -----------------------------------

/// The synthetic world with a chosen floor colour / alpha (the object and the far wall keep their colours).
fn world_with_floor(floor: &'static dyn Fn(usize, usize) -> [f32; 3], alpha: f32) -> impl Fn(usize, usize) -> [f32; 4] {
    move |x, y| {
        let (u, v) = uv(x, y);
        let c = if in_object(u, v) || v >= -0.05 { world_scene(x, y) } else { floor(x, y) };
        [c[0], c[1], c[2], alpha]
    }
}

fn mean_weight(px: &[[f32; 4]]) -> f32 {
    // Floor pixels beside and in front of the object (where a reflection exists in the plain scene).
    let mut s = 0.0;
    let mut n = 0.0;
    for v_m in [-0.6f32, -0.65, -0.7, -0.75, -0.8] {
        for u in [-0.2f32, -0.1, 0.0, 0.1, 0.2] {
            let (hx, hy) = half_px(u, v_m);
            s += px[hy * H + hx][3];
            n += 1.0;
        }
    }
    s / n
}

fn grey_floor(_: usize, _: usize) -> [f32; 3] {
    [0.45, 0.45, 0.45]
}
fn green_floor(_: usize, _: usize) -> [f32; 3] {
    [0.20, 0.50, 0.15] // grass
}
fn teal_floor(_: usize, _: usize) -> [f32; 3] {
    [0.35, 0.47, 0.50] // teal-grey deck: G close to B
}
fn noisy_floor(x: usize, y: usize) -> [f32; 3] {
    let h = ((x as f32 * 12.9898 + y as f32 * 78.233).sin() * 43758.547).fract().abs();
    let c = 0.40 + (h - 0.5) * 0.4; // +-0.2 around 0.4, neutral grey
    [c, c, c]
}

#[test]
fn smart_rejects_a_green_floor_but_keeps_a_teal_one() {
    let Some(g) = gpu() else { return };
    let run = |floor: &'static dyn Fn(usize, usize) -> [f32; 3], smart: f32| mean_weight(&trace_ex(&g, &world_depth, &world_with_floor(floor, 1.0), 2.0, CLASSES_ALL, smart));
    let base = run(&grey_floor, 0.0);
    let green = run(&green_floor, 1.0);
    let teal = run(&teal_floor, 1.0);
    let grey = run(&grey_floor, 1.0);
    let green_off = run(&green_floor, 0.0);
    println!("ssr smart: plain {base:.4}, grey {grey:.4}, teal {teal:.4}, green {green:.4}, green with smart 0 {green_off:.4}");
    assert!(base > 0.01, "the test scene reflected nothing");
    assert!(green < 0.1 * base, "grass must (almost) not reflect: {green} of {base}");
    assert!(teal > 0.9 * base, "a teal deck must keep its reflection: {teal} of {base}");
    assert!(grey > 0.9 * base, "a neutral smooth floor keeps its reflection: {grey} of {base}");
    // Half smart is half way.
    let half = mean_weight(&trace_ex(&g, &world_depth, &world_with_floor(&green_floor, 1.0), 2.0, CLASSES_ALL, 0.5));
    assert!(half > green && half < green_off, "smart 0.5 must lie between ({green} < {half} < {green_off})");
}

#[test]
fn smart_rejects_a_noisy_floor_and_keeps_a_smooth_one() {
    let Some(g) = gpu() else { return };
    let run = |floor: &'static dyn Fn(usize, usize) -> [f32; 3], smart: f32| mean_weight(&trace_ex(&g, &world_depth, &world_with_floor(floor, 1.0), 2.0, CLASSES_ALL, smart));
    let smooth = run(&grey_floor, 1.0);
    let noisy = run(&noisy_floor, 1.0);
    let noisy_off = run(&noisy_floor, 0.0);
    println!("ssr roughness: smooth {smooth:.4}, noisy {noisy:.4}, noisy with smart 0 {noisy_off:.4}");
    assert!(smooth > 0.01);
    assert!(noisy < 0.4 * smooth, "a noisy texture must reflect clearly less: {noisy} vs {smooth}");
    assert!(noisy_off > 0.8 * smooth, "smart 0 ignores the texture: {noisy_off} vs {smooth}");
}

#[test]
fn smart_zero_ignores_the_receiver_colour_completely() {
    let Some(g) = gpu() else { return };
    // With smart 0 the floor colour cannot change a single output value (the floor is never hit by its own rays).
    let a = trace_ex(&g, &world_depth, &world_with_floor(&green_floor, 1.0), 2.0, CLASSES_ALL, 0.0);
    let b = trace_ex(&g, &world_depth, &world_with_floor(&grey_floor, 1.0), 2.0, CLASSES_ALL, 0.0);
    let c = trace_ex(&g, &world_depth, &world_with_floor(&noisy_floor, 1.0), 2.0, CLASSES_ALL, 0.0);
    let same = |x: &[[f32; 4]], y: &[[f32; 4]]| x.iter().zip(y).all(|(p, q)| p.iter().zip(q).all(|(s, t)| s.to_bits() == t.to_bits()));
    assert!(same(&a, &b) && same(&a, &c), "smart 0 must not depend on the receiver colour");
}

#[test]
fn class_gating_follows_the_alpha_of_the_receiver() {
    let Some(g) = gpu() else { return };
    // (alpha, class bit): low <= 0.24098, mid <= 0.491961, high above.
    for (alpha, bit) in [(0.0f32, CLASS_LOW), (0.1, CLASS_LOW), (0.24, CLASS_LOW), (0.25, CLASS_MID), (0.35, CLASS_MID), (0.49, CLASS_MID), (0.5, CLASS_HIGH), (1.0, CLASS_HIGH)] {
        let scene = world_with_floor(&grey_floor, alpha);
        let open = trace_ex(&g, &world_depth, &scene, 2.0, CLASSES_ALL, 0.0);
        let allowed = trace_ex(&g, &world_depth, &scene, 2.0, bit, 0.0);
        let blocked = trace_ex(&g, &world_depth, &scene, 2.0, CLASSES_ALL & !bit, 0.0);
        let (wo, wa, wb) = (mean_weight(&open), mean_weight(&allowed), mean_weight(&blocked));
        println!("ssr class gating: alpha {alpha}: all {wo:.4}, only its class {wa:.4}, without its class {wb:.4}");
        assert!(wo > 0.01, "the test scene reflected nothing");
        assert_eq!(wa.to_bits(), wo.to_bits(), "alpha {alpha}: with its class bit set nothing changes");
        assert_eq!(wb, 0.0, "alpha {alpha}: with its class bit cleared the reflection is 0");
        assert!(blocked.iter().all(|p| p[3] < 1e-6), "alpha {alpha}: no reflection anywhere");
    }
    // No class at all: nothing reflects.
    let none = trace_ex(&g, &world_depth, &world_with_floor(&grey_floor, 1.0), 2.0, 0, 0.0);
    assert!(none.iter().all(|p| p[3] < 1e-6));
}

#[test]
fn debug_level_3_tints_by_the_alpha_class() {
    let Some(g) = gpu() else { return };
    let scene = [0.5, 0.4, 0.3];
    let b6 = Some([1.0, 0.0, 0.0, 3.0]);
    // The left half of the target is where the debug fill (level 2) would paint red: level 3 must not.
    let low = run_light_a(&g, scene, 0.1, None, b6);
    let mid = run_light_a(&g, scene, 0.35, None, b6);
    let high = run_light_a(&g, scene, 0.8, None, b6);
    let plain = run_light_a(&g, scene, 0.35, None, None);
    println!("debug materials: low {low:?}, mid {mid:?}, high {high:?}, untinted {plain:?}");
    assert!(low[2] > low[0] + 0.2 && low[2] > low[1] + 0.2, "low alpha must be blue: {low:?}");
    assert!(mid[1] > mid[0] + 0.2 && mid[1] > mid[2] + 0.2, "mid alpha must be green: {mid:?}");
    assert!(high[0] > high[1] + 0.2 && high[0] > high[2] + 0.2, "high alpha must be red: {high:?}");
    // The scene is still visible under the tint (55 % tint, 45 % scene).
    assert!(high[1] > 0.05 && low[0] > 0.05, "the scene must show through: {high:?} {low:?}");
    // The class boundaries are the engine's (0.240980, 0.491961).
    let b = |a: f32| run_light_a(&g, scene, a, None, b6);
    assert!(b(0.24)[2] > b(0.24)[1] && b(0.245)[1] > b(0.245)[2]);
    assert!(b(0.49)[1] > b(0.49)[0] && b(0.495)[0] > b(0.495)[1]);
    // The alpha passes through unchanged.
    assert!((mid[3] - 0.35).abs() < 2e-3);
    // Levels 0 / 1 are untouched by it: no tint at level 0.
    assert!((plain[0] - mid[0]).abs() > 0.05, "level 3 changes the picture, level 0 does not");
}

// ---- reflectivity score: hue gate, two-scale roughness, flatness, debug level 5 ----------------------------------

fn brown_floor(_: usize, _: usize) -> [f32; 3] {
    [0.45, 0.33, 0.20] // dirt / wood
}
fn cyan_floor(_: usize, _: usize) -> [f32; 3] {
    [0.20, 0.50, 0.55] // ice / water
}
fn rust_teal_floor(x: usize, y: usize) -> [f32; 3] {
    // The right half (u > 0) is a rust-red patch of about the same luminance as the teal deck on the left.
    if uv(x, y).0 > 0.0 { [0.80, 0.33, 0.20] } else { teal_floor(x, y) }
}
/// Coarse noise: random two-level 12 px blocks (narrow window sees mostly one block, the wide one many).
fn blocky_floor(x: usize, y: usize) -> [f32; 3] {
    let (bx, by) = ((x / 12) as f32, (y / 12) as f32);
    let h = ((bx * 12.9898 + by * 78.233).sin() * 43758.547).fract().abs();
    let c = if h > 0.5 { 0.62 } else { 0.28 };
    [c, c, c]
}
/// A floor whose depth carries a regular bump pattern (the normals vary on every ring).
fn bumpy_depth(x: usize, y: usize) -> f32 {
    let d = world_depth(x, y);
    let (u, v) = uv(x, y);
    if in_object(u, v) || v >= -0.05 {
        return d;
    }
    d * (1.0 + 0.015 * (x as f32 * 0.5).sin() * (y as f32 * 0.5).sin())
}

/// Mean weight (alpha) over floor pixels at the given u columns (rows as in `mean_weight`).
fn cols_weight(px: &[[f32; 4]], us: &[f32]) -> f32 {
    let mut s = 0.0;
    let mut n = 0.0;
    for v_m in [-0.6f32, -0.65, -0.7, -0.75, -0.8] {
        for &u in us {
            let (hx, hy) = half_px(u, v_m);
            s += px[hy * H + hx][3];
            n += 1.0;
        }
    }
    s / n
}

fn floor_weight(g: &Gpu, floor: &'static dyn Fn(usize, usize) -> [f32; 3], smart: f32) -> f32 {
    mean_weight(&trace_ex(g, &world_depth, &world_with_floor(floor, 1.0), 2.0, CLASSES_ALL, smart))
}

#[test]
fn hue_gate_rejects_green_and_brown_and_keeps_grey_teal_cyan() {
    let Some(g) = gpu() else { return };
    let base = floor_weight(&g, &grey_floor, 0.0);
    let (grey, teal, cyan) = (floor_weight(&g, &grey_floor, 1.0), floor_weight(&g, &teal_floor, 1.0), floor_weight(&g, &cyan_floor, 1.0));
    let (green, brown) = (floor_weight(&g, &green_floor, 1.0), floor_weight(&g, &brown_floor, 1.0));
    println!("ssr hue gate: plain {base:.4}, grey {grey:.4}, teal {teal:.4}, cyan {cyan:.4}, green {green:.4}, brown {brown:.4}");
    assert!(base > 0.01);
    assert!(green < 0.1 * base, "green must not reflect: {green} of {base}");
    assert!(brown < 0.1 * base, "a warm brown floor must not reflect: {brown} of {base}");
    for (n, w) in [("grey", grey), ("teal", teal), ("cyan", cyan)] {
        assert!(w > 0.9 * base, "{n} must keep its reflection: {w} of {base}");
    }
    // The hue slider at 0 switches the gate off (smart on, flatness irrelevant on a flat floor).
    let off = |floor: &'static dyn Fn(usize, usize) -> [f32; 3]| mean_weight(&trace_full(&g, &world_depth, &world_with_floor(floor, 1.0), 2.0, CLASSES_ALL, 1.0, 0.0, 1.0, false));
    assert!(off(&green_floor) > 0.9 * base && off(&brown_floor) > 0.9 * base, "hue_filter 0 must let green / brown through");
}

#[test]
fn a_rust_patch_on_a_teal_floor_loses_the_reflection_and_the_teal_keeps_it() {
    let Some(g) = gpu() else { return };
    let scene = world_with_floor(&rust_teal_floor, 1.0);
    let plain = trace_ex(&g, &world_depth, &scene, 2.0, CLASSES_ALL, 0.0);
    let smart = trace_ex(&g, &world_depth, &scene, 2.0, CLASSES_ALL, 1.0);
    let (teal_p, rust_p) = (cols_weight(&plain, &[-0.45, -0.4]), cols_weight(&plain, &[0.15, 0.2]));
    let (teal_s, rust_s) = (cols_weight(&smart, &[-0.45, -0.4]), cols_weight(&smart, &[0.15, 0.2]));
    println!("ssr rust patch: plain teal {teal_p:.4} rust {rust_p:.4}; smart teal {teal_s:.4} rust {rust_s:.4}");
    // The teal columns are > 16 px (the wide window) from the patch edge: the boundary itself is not coherent.
    assert!(teal_p > 0.005 && rust_p > 0.01);
    assert!(teal_s > 0.9 * teal_p, "the teal side keeps (almost) everything: {teal_s} of {teal_p}");
    assert!(rust_s < 0.1 * rust_p, "the rust patch loses it: {rust_s} of {rust_p}");
}

#[test]
fn noise_at_the_wide_scale_loses_the_reflection() {
    let Some(g) = gpu() else { return };
    let smooth = floor_weight(&g, &grey_floor, 1.0);
    let blocky = floor_weight(&g, &blocky_floor, 1.0);
    let blocky_off = floor_weight(&g, &blocky_floor, 0.0);
    println!("ssr wide roughness: smooth {smooth:.4}, blocky {blocky:.4}, blocky with smart 0 {blocky_off:.4}");
    assert!(smooth > 0.01);
    assert!(blocky < 0.4 * smooth, "coarse noise must reflect clearly less: {blocky} vs {smooth}");
    assert!(blocky_off > 0.8 * smooth, "smart 0 ignores the texture: {blocky_off} vs {smooth}");
}

#[test]
fn varying_normals_lose_the_reflection_and_the_flatness_slider_controls_it() {
    let Some(g) = gpu() else { return };
    let scene = world_with_floor(&grey_floor, 1.0);
    let w = |smart: f32, flat: f32| mean_weight(&trace_full(&g, &bumpy_depth, &scene, 2.0, CLASSES_ALL, smart, 1.0, flat, false));
    let flat_floor = floor_weight(&g, &grey_floor, 1.0);
    let (off, full, half, no_flat) = (w(0.0, 1.0), w(1.0, 1.0), w(1.0, 0.5), w(1.0, 0.0));
    println!("ssr flatness: flat floor {flat_floor:.4}; bumpy: smart 0 {off:.4}, full {full:.4}, flatness 0.5 {half:.4}, flatness 0 {no_flat:.4}");
    assert!(off > 0.01, "the bumpy floor must still reflect without the score");
    assert!(flat_floor > 0.01);
    assert!(full < 0.4 * off, "a floor with varying normals must lose it: {full} of {off}");
    assert!(no_flat > full && half > full && half < no_flat, "the slider is monotonic ({full} < {half} < {no_flat})");
    assert!(no_flat > 0.9 * off, "flatness 0 turns the requirement off: {no_flat} of {off}");
}

#[test]
fn smart_zero_is_bit_identical_whatever_the_new_keys_say() {
    let Some(g) = gpu() else { return };
    let scene = world_with_floor(&brown_floor, 1.0);
    let a = trace_full(&g, &bumpy_depth, &scene, 2.0, CLASSES_ALL, 0.0, 1.0, 1.0, false);
    let b = trace_full(&g, &bumpy_depth, &scene, 2.0, CLASSES_ALL, 0.0, 0.0, 0.0, false);
    let c = trace_full(&g, &bumpy_depth, &world_with_floor(&grey_floor, 1.0), 2.0, CLASSES_ALL, 0.0, 0.3, 0.9, false);
    let same = |x: &[[f32; 4]], y: &[[f32; 4]]| x.iter().zip(y).all(|(p, q)| p.iter().zip(q).all(|(s, t)| s.to_bits() == t.to_bits()));
    assert!(same(&a, &b) && same(&a, &c), "smart 0 is the plain floor reflection regardless of hue, flatness or colour");
    assert!(a.iter().any(|p| p[3] > 0.01));
}

#[test]
fn debug_level_5_stores_the_reflectivity_in_alpha() {
    let Some(g) = gpu() else { return };
    let rp = |floor: &'static dyn Fn(usize, usize) -> [f32; 3]| trace_full(&g, &world_depth, &world_with_floor(floor, 1.0), 2.0, CLASSES_ALL, 1.0, 1.0, 1.0, true);
    let rows = |px: &[[f32; 4]]| {
        // Floor pixels at the same places as mean_weight, whether or not a ray hit anything.
        let mut s = 0.0;
        let mut n = 0.0;
        for v_m in [-0.6f32, -0.65, -0.7, -0.75, -0.8] {
            for u in [-0.2f32, -0.1, 0.0, 0.1, 0.2] {
                let (hx, hy) = half_px(u, v_m);
                s += px[hy * H + hx][3];
                n += 1.0;
            }
        }
        s / n
    };
    let (teal, green, brown) = (rows(&rp(&teal_floor)), rows(&rp(&green_floor)), rows(&rp(&brown_floor)));
    println!("ssr reflectivity alpha: teal {teal:.3}, green {green:.3}, brown {brown:.3}");
    assert!(teal > 0.9, "a smooth teal deck is fully reflective: {teal}");
    assert!(green < 0.1 && brown < 0.1, "green / brown score about 0: {green} {brown}");
    // A wall is not a floor: no heat there.
    let px = rp(&teal_floor);
    let (hx, hy) = half_px(0.0, 0.2);
    assert!(px[hy * H + hx][3] < 1e-3, "the object face is no floor: {}", px[hy * H + hx][3]);
    // Without the debug flag the alpha is the weight (<= 0.6), unchanged.
    let w = trace_ex(&g, &world_depth, &world_with_floor(&teal_floor, 1.0), 2.0, CLASSES_ALL, 1.0);
    assert!(w.iter().all(|p| p[3] <= 0.61));
}

#[test]
fn debug_level_5_shows_a_heat_map_over_a_dimmed_scene() {
    let Some(g) = gpu() else { return };
    let scene = [0.5, 0.4, 0.3];
    let b6 = Some([1.0, 0.0, 0.0, 5.0]);
    let heat = |a: f32| run_light(&g, scene, Some(([0.0, 0.0, 0.0, a], [1.0, 0.0, 0.0, 0.0])), b6);
    let (c0, c25, c5, c75, c1) = (heat(0.0), heat(0.25), heat(0.5), heat(0.75), heat(1.0));
    println!("heat map: 0 {c0:?} .25 {c25:?} .5 {c5:?} .75 {c75:?} 1 {c1:?}");
    // Alpha 0 = not a floor: only the dimmed scene (no tint), darker than level 0.
    let plain = run_light(&g, scene, None, None);
    assert!(c0[0] < plain[0] * 0.6 && c0[1] < plain[1] * 0.6, "dimmed scene: {c0:?} vs {plain:?}");
    assert!((c0[0] - c0[2]).abs() < 0.2 * plain[0].max(0.01) + 0.05, "no tint on non-floor pixels: {c0:?}");
    let low = heat(0.01);
    assert!(low[2] > low[0] + 0.3 && low[2] > low[1] + 0.3, "near 0 is blue: {low:?}");
    assert!(c5[1] > c5[0] + 0.3 && c5[1] > c5[2] + 0.3, "0.5 is green: {c5:?}");
    assert!(c1[0] > c1[1] + 0.3 && c1[0] > c1[2] + 0.3, "1 is red: {c1:?}");
    assert!(c75[0] > 0.3 && c75[1] > 0.3 && c75[2] < 0.2, "0.75 is yellow: {c75:?}");
    assert!(c25[1] > 0.3 && c25[2] > 0.3 && c25[0] < 0.2, "0.25 is cyan: {c25:?}");
}


// ---- positive-evidence material score: dirt / sand / grass ~ 0, water high, polished medium / high, metal medium ----

fn dark_dirt(_: usize, _: usize) -> [f32; 3] {
    [0.12, 0.09, 0.06]
}
fn desat_dark_dirt(_: usize, _: usize) -> [f32; 3] {
    [0.22, 0.19, 0.16] // dark desaturated brown, flat and smooth
}
fn desat_mid_dirt(_: usize, _: usize) -> [f32; 3] {
    [0.35, 0.32, 0.29] // grey-brown mud
}
fn charr_path(_: usize, _: usize) -> [f32; 3] {
    [0.40, 0.30, 0.22]
}
fn noisy_dirt(x: usize, y: usize) -> [f32; 3] {
    let h = ((x as f32 * 12.9898 + y as f32 * 78.233).sin() * 43758.547).fract().abs();
    let k = 0.8 + (h - 0.5) * 0.5;
    [0.42 * k, 0.31 * k, 0.21 * k]
}
fn sand_floor(_: usize, _: usize) -> [f32; 3] {
    [0.75, 0.65, 0.45]
}
fn blue_water(_: usize, _: usize) -> [f32; 3] {
    [0.15, 0.30, 0.55]
}
fn metal_floor(_: usize, _: usize) -> [f32; 3] {
    [0.75, 0.75, 0.78] // bright smooth grey
}

/// Mean reflectivity score (debug alpha) over the floor sample rows, whether or not a ray hit anything.
fn score_of(g: &Gpu, depth: &dyn Fn(usize, usize) -> f32, scene: &dyn Fn(usize, usize) -> [f32; 4]) -> f32 {
    let px = trace_full(g, depth, scene, 2.0, CLASSES_ALL, 1.0, 1.0, 1.0, true);
    cols_weight(&px, &[-0.2, -0.1, 0.0, 0.1, 0.2])
}

fn score_floor(g: &Gpu, floor: &'static dyn Fn(usize, usize) -> [f32; 3]) -> f32 {
    score_of(g, &world_depth, &world_with_floor(floor, 1.0))
}

#[test]
fn dry_dirt_in_every_variant_scores_zero() {
    let Some(g) = gpu() else { return };
    for (n, f) in [
        ("brown", &brown_floor as &'static dyn Fn(usize, usize) -> [f32; 3]),
        ("dark brown", &dark_dirt),
        ("dark desaturated", &desat_dark_dirt),
        ("grey-brown mud", &desat_mid_dirt),
        ("charr path", &charr_path),
        ("noisy dirt", &noisy_dirt),
        ("sand", &sand_floor),
        ("grass", &green_floor),
    ] {
        let s = score_floor(&g, f);
        println!("ssr score {n}: {s:.4}");
        assert!(s < 0.03, "{n} must score about 0: {s}");
    }
    // A noisy variant on a tilted-differently (bumpy) ground too.
    let s = score_of(&g, &bumpy_depth, &world_with_floor(&brown_floor, 1.0));
    assert!(s < 0.03, "bumpy brown: {s}");
    // And the actual reflection weight of dry dirt is exactly nothing.
    for f in [&charr_path as &'static dyn Fn(usize, usize) -> [f32; 3], &desat_dark_dirt, &desat_mid_dirt] {
        let w = mean_weight(&trace_ex(&g, &world_depth, &world_with_floor(f, 1.0), 2.0, CLASSES_ALL, 1.0));
        assert!(w < 1e-3, "dirt reflection weight {w}");
    }
}

#[test]
fn water_scores_high_polished_floor_medium_high_and_metal_medium() {
    let Some(g) = gpu() else { return };
    let (teal, cyan, blue) = (score_floor(&g, &teal_floor), score_floor(&g, &cyan_floor), score_floor(&g, &blue_water));
    let (polished, metal) = (score_floor(&g, &grey_floor), score_floor(&g, &metal_floor));
    println!("ssr score: teal {teal:.3} cyan {cyan:.3} blue {blue:.3} polished {polished:.3} metal {metal:.3}");
    for (n, s) in [("teal", teal), ("cyan", cyan), ("blue", blue)] {
        assert!(s > 0.9, "{n} water must be high: {s}");
    }
    assert!(polished > 0.6, "a smooth neutral polished floor is medium / high: {polished}");
    assert!(metal > 0.25 && metal < 0.9, "bright smooth grey metal is medium: {metal}");
    assert!(metal < polished, "metal ({metal}) below the darker polished floor ({polished})");
}

#[test]
fn the_score_is_stable_under_tiny_input_noise_and_repeats_exactly() {
    let Some(g) = gpu() else { return };
    for (n, f) in [("teal", &teal_floor as &'static dyn Fn(usize, usize) -> [f32; 3]), ("polished", &grey_floor), ("metal", &metal_floor), ("dirt", &charr_path)] {
        let clean = trace_full(&g, &world_depth, &world_with_floor(f, 1.0), 2.0, CLASSES_ALL, 1.0, 1.0, 1.0, true);
        let again = trace_full(&g, &world_depth, &world_with_floor(f, 1.0), 2.0, CLASSES_ALL, 1.0, 1.0, 1.0, true);
        assert!(clean.iter().zip(&again).all(|(a, b)| a[3].to_bits() == b[3].to_bits()), "{n}: the same frame must give the same score");
        let noisy = trace_full(&g, &world_depth, &|x, y| {
            let c = world_with_floor(f, 1.0)(x, y);
            let h = ((x as f32 * 12.9898 + y as f32 * 78.233).sin() * 43758.547).fract().abs() - 0.5;
            let (u, v) = uv(x, y);
            if in_object(u, v) || v >= -0.05 { c } else { [c[0] + h * 0.004, c[1] - h * 0.004, c[2] + h * 0.003, 1.0] }
        }, 2.0, CLASSES_ALL, 1.0, 1.0, 1.0, true);
        let mut worst = 0.0f32;
        for v_m in [-0.6f32, -0.65, -0.7, -0.75, -0.8] {
            for u in [-0.2f32, -0.1, 0.0, 0.1, 0.2] {
                let (hx, hy) = half_px(u, v_m);
                worst = worst.max((clean[hy * H + hx][3] - noisy[hy * H + hx][3]).abs());
            }
        }
        println!("ssr score stability {n}: worst delta {worst:.4}");
        assert!(worst < 0.08, "{n}: +-0.2 % input noise moved the score by {worst}");
    }
}

// ---- water and glass (ssr_water_boost, water_depth_look) -----------------------------------------------------------------

/// The whole SSR (trace + both blurs) with the water keys; smart 1 (the score is what finds the water), hue and flatness 1.
fn trace_w(g: &Gpu, depth: &dyn Fn(usize, usize) -> f32, scene: &dyn Fn(usize, usize) -> [f32; 4], range: f32, boost: f32, look: f32) -> Vec<[f32; 4]> {
    unsafe {
        let d = depth_tex(g, depth);
        let (_t, s) = scene_srv_a(g, scene);
        let mut ssr = Ssr::new(g.dev.as_raw(), blobs()).expect("Ssr::new");
        let p = SsrParams { range, width: N as u32, height: N as u32, classes: CLASSES_ALL, smart: 1.0, hue_filter: 1.0, flatness: 1.0, debug_reflectivity: false, water_boost: boost, water_depth_look: look, matte_cut: 0.0, stats: false };
        assert!(ssr.run(g.dev.as_raw(), g.ctx.as_raw(), d.as_raw(), s.as_raw(), &p, 1.0), "run failed");
        let out = read_half(g, ID3D11Texture2D::from_raw_borrowed(&ssr.tex[0]).expect("target"), H, H);
        drop(ssr);
        out
    }
}

fn water_floor_world(floor: &'static dyn Fn(usize, usize) -> [f32; 3]) -> impl Fn(usize, usize) -> [f32; 4] {
    world_with_floor(floor, 1.0)
}

fn bit_equal(a: &[[f32; 4]], b: &[[f32; 4]]) -> bool {
    a.iter().zip(b).all(|(p, q)| p.iter().zip(q).all(|(x, y)| x.to_bits() == y.to_bits()))
}

#[test]
fn water_boost_leaves_every_non_water_pixel_bit_identical() {
    let Some(g) = gpu() else { return };
    // Dry earth, grass, sand, dark dirt, a polished neutral floor and bright metal: none of them is water or glass.
    let floors: [(&str, &'static dyn Fn(usize, usize) -> [f32; 3]); 7] = [("brown", &brown_floor), ("green", &green_floor), ("charr path", &charr_path), ("sand", &sand_floor), ("grey polished", &grey_floor), ("metal", &metal_floor), ("noisy", &noisy_floor)];
    for (name, f) in floors {
        let scene = water_floor_world(f);
        let old = trace_w(&g, &world_depth, &scene, 2.0, 0.0, 0.0);
        for boost in [0.3f32, 0.6, 1.0] {
            let new = trace_w(&g, &world_depth, &scene, 2.0, boost, 1.0);
            assert!(bit_equal(&old, &new), "{name}: boost {boost} changed a pixel of a non-water scene");
        }
    }
    // And boost 0 is the old trace for water too (depth look alone does nothing without the boost).
    let teal = water_floor_world(&teal_floor);
    let old = trace_w(&g, &world_depth, &teal, 2.0, 0.0, 0.0);
    let off = trace_w(&g, &world_depth, &teal, 2.0, 0.0, 1.0);
    assert!(bit_equal(&old, &off), "boost 0 must be the old behaviour whatever the depth look says");
}

#[test]
fn water_reflection_is_stronger_and_never_above_085() {
    let Some(g) = gpu() else { return };
    let teal = water_floor_world(&teal_floor);
    let old = trace_w(&g, &world_depth, &teal, 2.0, 0.0, 0.0);
    let new = trace_w(&g, &world_depth, &teal, 2.0, 1.0, 0.0);
    let half = trace_w(&g, &world_depth, &teal, 2.0, 0.5, 0.0);
    let (mo, mn) = (mean_weight(&old), mean_weight(&new));
    let mh = mean_weight(&half);
    println!("water weight: old {mo:.4}, boost 0.5 {mh:.4}, boost 1 {mn:.4}");
    assert!(mo > 0.01, "the test scene reflected nothing");
    assert!(mn > 1.3 * mo, "water must reflect clearly more with the boost ({mn} vs {mo})");
    assert!(mh > mo && mh < mn, "the boost is monotonic ({mo} < {mh} < {mn})");
    let max_new = new.iter().map(|p| p[3]).fold(0.0, f32::max);
    let max_old = old.iter().map(|p| p[3]).fold(0.0, f32::max);
    println!("largest weight: old {max_old:.3}, new {max_new:.3}");
    assert!(max_new <= 0.85 + 1e-3, "weight {max_new} above the 0.85 clamp");
    assert!(max_old <= 0.6 + 1e-3);
    // The colour of a reflected object stays plausible (the tint is mild): the recovered colour moves by less than 0.1.
    let (hx, hy) = half_px(0.0, -0.6);
    let (_, co) = refl_at(&old, hx, hy);
    let (_, cn) = refl_at(&new, hx, hy);
    assert!((co - cn).abs() < 0.12, "the in-medium tint must be mild: {co} -> {cn}");
}

#[test]
fn water_fresnel_grows_toward_grazing_angles() {
    let Some(g) = gpu() else { return };
    // A flat teal floor under a wall 8 deep and bright: every row reflects the wall.
    let depth = |x: usize, y: usize| {
        let (_, v) = uv(x, y);
        if v < -0.05 { floor_z(v) } else { 8.0 }
    };
    let scene = |x: usize, y: usize| {
        let (_, v) = uv(x, y);
        if v < -0.05 { [0.35, 0.47, 0.50, 1.0] } else { [0.8, 0.8, 0.8, 1.0] }
    };
    let old = trace_w(&g, &depth, &scene, 2.0, 0.0, 0.0);
    let new = trace_w(&g, &depth, &scene, 2.0, 1.0, 0.0);
    // The same ray, the same fades: the ratio new / old is the Fresnel gain. Rows from steep to grazing.
    let mut ratios = Vec::new();
    for v in [-0.9f32, -0.8, -0.7, -0.6, -0.5] {
        let (hx, hy) = half_px(0.0, v);
        let (a_old, a_new) = (old[hy * H + hx][3], new[hy * H + hx][3]);
        println!("row {v}: old {a_old:.4}, new {a_new:.4}");
        if a_old > 1e-3 {
            ratios.push(a_new / a_old);
        }
    }
    assert!(ratios.len() >= 3, "too few rows with a reflection to compare: {ratios:?}");
    println!("water fresnel gain new/old, steep to grazing: {ratios:.3?}");
    for w in ratios.windows(2) {
        assert!(w[1] > w[0] + 0.05, "the Fresnel gain must grow toward grazing: {ratios:?}");
    }
    assert!(ratios[0] > 1.1, "even a steep view gets some boost");
}

/// The reflection weight along a half-pixel row, across the right edge of the reflected object (a step from the weight to none).
fn edge_profile(px: &[[f32; 4]], v: f32) -> Vec<f32> {
    let (_, hy) = half_px(0.0, v);
    let (hx0, _) = half_px(0.3, v);
    (hx0.saturating_sub(10)..hx0 + 10).map(|hx| px[hy * H + hx][3]).collect()
}

/// Width in half pixels of the falling edge of a profile: from 80 % to 20 % of its plateau (the mean of the first 6 samples), with
/// linear interpolation between samples.
fn transition_width(p: &[f32]) -> f32 {
    let plateau = p[..6].iter().sum::<f32>() / 6.0;
    let cross = |level: f32| -> f32 {
        for i in 0..p.len() - 1 {
            if p[i] >= level && p[i + 1] < level {
                return i as f32 + (p[i] - level) / (p[i] - p[i + 1]).max(1e-9);
            }
        }
        p.len() as f32
    };
    cross(0.2 * plateau) - cross(0.8 * plateau)
}

fn ripple_floor(x: usize, y: usize) -> [f32; 3] {
    // Teal with small cell noise (3 px cells): enough luminance / chroma variance to lower the smoothness without killing the score.
    let (cx, cy) = (x / 3, y / 3);
    let h = ((cx as f32 * 12.9898 + cy as f32 * 78.233).sin() * 43758.547).fract().abs();
    let k = 1.0 + (h - 0.5) * 0.45;
    [0.35 * k, 0.47 * k, 0.50 * k]
}

#[test]
fn calm_water_gets_a_sharper_mirror_than_rippled_water() {
    let Some(g) = gpu() else { return };
    let calm = water_floor_world(&teal_floor);
    let rippled = water_floor_world(&ripple_floor);
    let v = -0.6;
    let c_old = edge_profile(&trace_w(&g, &world_depth, &calm, 2.0, 0.0, 0.0), v);
    let c_new = edge_profile(&trace_w(&g, &world_depth, &calm, 2.0, 1.0, 0.0), v);
    let r_old = edge_profile(&trace_w(&g, &world_depth, &rippled, 2.0, 0.0, 0.0), v);
    let r_new = edge_profile(&trace_w(&g, &world_depth, &rippled, 2.0, 1.0, 0.0), v);
    let (wc_old, wc_new, wr_old, wr_new) = (transition_width(&c_old), transition_width(&c_new), transition_width(&r_old), transition_width(&r_new));
    println!("edge profiles: calm old {c_old:.2?}
 calm new {c_new:.2?}
 ripple old {r_old:.2?}
 ripple new {r_new:.2?}");
    println!("edge width (half px, 80 % to 20 %): calm old {wc_old:.2}, calm new {wc_new:.2}, ripple old {wr_old:.2}, ripple new {wr_new:.2}");
    assert!(r_new.iter().cloned().fold(0.0, f32::max) > 0.05, "the rippled floor must still be water (it reflects)");
    assert!(wc_new < wc_old - 0.3, "calm water must get a sharper edge ({wc_new} vs {wc_old})");
    assert!(wr_new > wr_old + 0.3, "rippled water must get a softer edge ({wr_new} vs {wr_old})");
    assert!(wc_new < wr_new - 0.5, "calm is sharper than rippled ({wc_new} vs {wr_new})");
}

/// A teal floor under a wall 8 deep up to v = 0.94 and, when `sky`, sky (invalid depth, bright) above it; otherwise the wall goes on
/// to the top of the picture. In this camera model a flat floor never sends a ray out of the picture (its screen path only
/// approaches the mirrored row), so the screen edge is reached by rays that hit things next to the top border.
fn sky_world_depth(sky: bool) -> impl Fn(usize, usize) -> f32 {
    move |x, y| {
        let (_, v) = uv(x, y);
        if v < -0.05 {
            floor_z(v)
        } else if v > 0.94 && sky {
            0.0
        } else {
            8.0
        }
    }
}

fn sky_world_scene(sky: bool) -> impl Fn(usize, usize) -> [f32; 4] {
    move |x, y| {
        let (_, v) = uv(x, y);
        if v < -0.05 {
            [0.35, 0.47, 0.50, 1.0]
        } else if v > 0.94 && sky {
            [0.8, 0.9, 1.0, 1.0]
        } else {
            [0.3, 0.3, 0.3, 1.0]
        }
    }
}

#[test]
fn a_reflection_next_to_the_screen_edge_fades_into_the_sky_instead_of_a_black_band() {
    let Some(g) = gpu() else { return };
    let (d_sky, s_sky) = (sky_world_depth(true), sky_world_scene(true));
    let (d_wall, s_wall) = (sky_world_depth(false), sky_world_scene(false));
    let old = trace_w(&g, &d_sky, &s_sky, 2.0, 0.0, 0.0);
    let with_sky = trace_w(&g, &d_sky, &s_sky, 2.0, 1.0, 0.0);
    let no_sky = trace_w(&g, &d_wall, &s_wall, 2.0, 1.0, 0.0);
    let no_sky_old = trace_w(&g, &d_wall, &s_wall, 2.0, 0.0, 0.0);
    // Where the screen edge swallowed the reflection (the weight is clearly lower than the same trace without the edge fade could
    // give), the sky must now be there: heavier and brighter than the same boosted trace with no sky on the screen.
    let mut better = Vec::new();
    for y in H / 2 + 2..H {
        for x in 0..H {
            let (a, b) = (with_sky[y * H + x], no_sky[y * H + x]);
            if a[3] > b[3] + 0.01 {
                better.push((x, y, b[3], a[3], (a[0] / a[3]).max(0.0).sqrt(), (b[0] / b[3].max(1e-6)).max(0.0).sqrt()));
            }
        }
    }
    println!("{} pixels gained sky, e.g. {:?}", better.len(), better.iter().take(4).collect::<Vec<_>>());
    assert!(better.len() >= 6, "the black band at the screen edge must be filled ({} pixels)", better.len());
    let mut brighter = 0;
    for (_, _, _, wn, cn, co) in &better {
        assert!(*wn <= 0.85 + 1e-3, "weight {wn} above the clamp");
        if cn > co {
            brighter += 1;
        }
    }
    assert!(brighter * 4 >= better.len() * 3, "the filled pixels must be brighter (sky) than the wall they replace: {brighter} of {}", better.len());
    // Nothing is invented: with no sky on the screen the boosted trace reflects only where the old one did.
    let invented = no_sky.iter().zip(&no_sky_old).filter(|(n, o)| o[3] < 1e-5 && n[3] > 0.01).count();
    assert_eq!(invented, 0, "with no sky on the screen nothing is invented");
    // And the sky never makes the old trace (boost 0) different from itself: bit-identical repeat.
    assert!(bit_equal(&old, &trace_w(&g, &d_sky, &s_sky, 2.0, 0.0, 0.0)));
}

#[test]
fn water_depth_look_darkens_by_at_most_15_percent_and_only_with_the_boost() {
    let Some(g) = gpu() else { return };
    let scene = water_floor_world(&teal_floor);
    let off = trace_w(&g, &world_depth, &scene, 2.0, 1.0, 0.0);
    let on = trace_w(&g, &world_depth, &scene, 2.0, 1.0, 1.0);
    let half = trace_w(&g, &world_depth, &scene, 2.0, 1.0, 0.5);
    // Without the look no channel is ever negative; with it the receiver is darkened.
    assert!(off.iter().all(|p| p[..3].iter().all(|v| *v >= -1e-4)), "negative light without the depth look");
    let recv = [0.35f32 * 0.35, 0.47 * 0.47, 0.50 * 0.50];
    let mut darkest = 0.0f32;
    let mut deepest = [0.0f32; 3];
    for (a, b) in off.iter().zip(&on) {
        for i in 0..3 {
            let d = a[i] - b[i]; // what the look removed
            assert!(d >= -1e-3, "the look must only darken ({d})");
            assert!(d <= 0.15 * recv[i] * 1.02 + 2e-3, "channel {i}: darkened by {d}, more than 15 % of the receiver {}", recv[i]);
            if d > deepest[i] {
                deepest[i] = d;
            }
            darkest = darkest.max(d / recv[i]);
        }
    }
    println!("depth look: largest darkening share {darkest:.3} of the receiver (limit 0.15), per channel {deepest:.4?}");
    assert!(darkest > 0.03, "the depth look must be visible somewhere ({darkest})");
    // Half the look removes less than the full look at the strongest pixel.
    let (hx, hy) = half_px(0.0, -0.7);
    let d_full = off[hy * H + hx][1] - on[hy * H + hx][1];
    let d_half = off[hy * H + hx][1] - half[hy * H + hx][1];
    assert!(d_half < d_full && d_half > 0.0, "{d_half} vs {d_full}");
    // No water, no look: a brown floor is untouched even with the look at 1.
    let brown = water_floor_world(&brown_floor);
    let b_off = trace_w(&g, &world_depth, &brown, 2.0, 1.0, 0.0);
    let b_on = trace_w(&g, &world_depth, &brown, 2.0, 1.0, 1.0);
    assert!(bit_equal(&b_off, &b_on));
}

#[test]
fn water_is_nan_safe_and_bounded_with_bad_inputs() {
    let Some(g) = gpu() else { return };
    let weird_scene = |x: usize, y: usize| {
        let (_, v) = uv(x, y);
        let base = if v < -0.05 { [0.35, 0.47, 0.50] } else { [0.3, 0.3, 0.3] };
        match (x * 7 + y * 3) % 23 {
            0 => [f32::NAN, 0.5, 0.5, 1.0],
            1 => [f32::INFINITY, 0.4, 0.4, 1.0],
            2 => [-2.0, 0.4, 0.5, 1.0],
            _ => [base[0], base[1], base[2], 1.0],
        }
    };
    let weird_depth = |x: usize, y: usize| match (x + 5 * y) % 31 {
        0 => 0.0,
        1 => f32::NAN,
        2 => -3.0,
        3 => 1e37,
        _ => world_depth(x, y),
    };
    let out = trace_w(&g, &weird_depth, &weird_scene, 2.0, 1.0, 1.0);
    for p in &out {
        assert!(p.iter().all(|v| v.is_finite()), "non-finite output {p:?}");
        assert!(p[3] >= 0.0 && p[3] <= 0.85 + 1e-3, "weight {} out of range", p[3]);
        assert!(p[..3].iter().all(|v| *v >= -0.2), "a darkening beyond its bound: {p:?}");
    }
    // The light-adjust block with a NEGATIVE reflection texel (the depth look): finite, never negative, never brighter than the pixel.
    let scene = [0.3f32, 0.4, 0.45];
    let plain = run_light(&g, scene, None, None);
    for neg in [-0.001f32, -0.02, -5.0] {
        let on = run_light(&g, scene, Some(([neg, neg, neg, 0.5], [1.0, 0.0, 0.0, 0.0])), None);
        assert!(on.iter().all(|v| v.is_finite() && *v >= 0.0), "negative reflection gave {on:?}");
        assert!(on[1] <= plain[1] + 1e-3, "a negative reflection darkens: {on:?} vs {plain:?}");
    }
}

// ---- matte gate (ssr_matte_cut): dry ground lit cool / grey no longer reflects, water keeps it ----------------------------

/// The whole SSR with every material key at its default except the ones given (classes 7, smart, hue 1, flatness 1,
/// water boost and depth look at the defaults 0.6 / 0.3).
fn trace_m(g: &Gpu, depth: &dyn Fn(usize, usize) -> f32, scene: &dyn Fn(usize, usize) -> [f32; 4], smart: f32, cut: f32, debug: bool) -> Vec<[f32; 4]> {
    unsafe {
        let d = depth_tex(g, depth);
        let (_t, s) = scene_srv_a(g, scene);
        let mut ssr = Ssr::new(g.dev.as_raw(), blobs()).expect("Ssr::new");
        let p = SsrParams { range: 2.0, width: N as u32, height: N as u32, classes: CLASSES_ALL, smart, hue_filter: DEFAULT_HUE, flatness: DEFAULT_FLAT, debug_reflectivity: debug, water_boost: DEFAULT_WATER, water_depth_look: DEFAULT_DEPTH_LOOK, matte_cut: cut, stats: false };
        assert!(ssr.run(g.dev.as_raw(), g.ctx.as_raw(), d.as_raw(), s.as_raw(), &p, 1.0), "run failed");
        let out = read_half(g, ID3D11Texture2D::from_raw_borrowed(&ssr.tex[0]).expect("target"), H, H);
        drop(ssr);
        out
    }
}

/// The telemetry of one frame (the stats pass, read back waiting).
fn stats_m(g: &Gpu, depth: &dyn Fn(usize, usize) -> f32, scene: &dyn Fn(usize, usize) -> [f32; 4], cut: f32) -> MatteStats {
    unsafe {
        let d = depth_tex(g, depth);
        let (_t, s) = scene_srv_a(g, scene);
        let mut ssr = Ssr::new(g.dev.as_raw(), blobs()).expect("Ssr::new");
        let p = SsrParams { range: 2.0, width: N as u32, height: N as u32, classes: CLASSES_ALL, smart: 1.0, hue_filter: DEFAULT_HUE, flatness: DEFAULT_FLAT, debug_reflectivity: false, water_boost: DEFAULT_WATER, water_depth_look: DEFAULT_DEPTH_LOOK, matte_cut: cut, stats: true };
        assert!(ssr.run(g.dev.as_raw(), g.ctx.as_raw(), d.as_raw(), s.as_raw(), &p, 1.0), "run failed");
        let st = ssr.poll_stats(g.ctx.as_raw(), true).expect("the stats copy must be readable");
        assert!(ssr.poll_stats(g.ctx.as_raw(), true).is_none(), "one read per queued copy");
        st
    }
}

/// A whole frame under a light tint (encoded factor per channel), the floor being `floor`: moonlight / blue haze.
fn tinted_world(floor: [f32; 3], tint: [f32; 3]) -> impl Fn(usize, usize) -> [f32; 4] {
    move |x, y| {
        let (u, v) = uv(x, y);
        let c = if in_object(u, v) || v >= -0.05 { world_scene(x, y) } else { floor };
        [c[0] * tint[0], c[1] * tint[1], c[2] * tint[2], 1.0]
    }
}

const MOON: [f32; 3] = [0.75, 0.85, 1.2];
const DAY: [f32; 3] = [1.0, 1.0, 1.0];
/// Grey-brown dry earth (as `desat_mid_dirt`); under MOON it reads (0.26, 0.27, 0.35): cool and pale, like a night dirt path.
const DRY_EARTH: [f32; 3] = [0.35, 0.32, 0.29];
/// Dry ground in grey haze / shade: colourless.
const HAZE_EARTH: [f32; 3] = [0.42, 0.42, 0.43];
const SAMPLE_COLS: [f32; 5] = [-0.2, -0.1, 0.0, 0.1, 0.2];

#[test]
fn matte_ground_lit_cool_or_grey_reflected_before_and_no_longer_does_at_the_defaults() {
    let Some(g) = gpu() else { return };
    let plain = mean_weight(&trace_m(&g, &world_depth, &tinted_world(DRY_EARTH, MOON), 0.0, DEFAULT_MATTE, false));
    assert!(plain > 0.01, "the test scene reflected nothing");
    for (name, floor, tint) in [("moonlit dry earth", DRY_EARTH, MOON), ("grey haze earth", HAZE_EARTH, DAY), ("moonlit haze earth", HAZE_EARTH, MOON)] {
        let scene = tinted_world(floor, tint);
        let old_w = mean_weight(&trace_m(&g, &world_depth, &scene, 1.0, 0.0, false));
        let old_s = cols_weight(&trace_m(&g, &world_depth, &scene, 1.0, 0.0, true), &SAMPLE_COLS);
        let new_w = mean_weight(&trace_m(&g, &world_depth, &scene, 1.0, DEFAULT_MATTE, false));
        let new_s = cols_weight(&trace_m(&g, &world_depth, &scene, 1.0, DEFAULT_MATTE, true), &SAMPLE_COLS);
        let full = trace_m(&g, &world_depth, &scene, 1.0, 1.0, false);
        println!("ssr matte gate {name}: cut 0 score {old_s:.3} weight {old_w:.4}; cut {DEFAULT_MATTE} score {new_s:.3} weight {new_w:.5}; cut 1 weight {:.5}", mean_weight(&full));
        assert!(old_s > 0.5 && old_w > 0.3 * plain, "{name}: the old score let it reflect (the bug seen in the game): {old_s} {old_w}");
        assert!(new_s < 0.01, "{name}: matte ground must score 0 at the default cut: {new_s}");
        assert!(new_w < 1e-3, "{name}: no reflection at the default cut: {new_w}");
        assert!(full.iter().all(|p| p[3] < 1e-3), "{name}: cut 1 leaves nothing anywhere on it");
    }
}

#[test]
fn water_keeps_its_reflection_and_its_boost_at_the_default_cut() {
    let Some(g) = gpu() else { return };
    for (name, f) in [("blue water", &blue_water as &'static dyn Fn(usize, usize) -> [f32; 3]), ("cyan ice / water", &cyan_floor)] {
        let scene = world_with_floor(f, 1.0);
        let old = mean_weight(&trace_m(&g, &world_depth, &scene, 1.0, 0.0, false));
        let new = mean_weight(&trace_m(&g, &world_depth, &scene, 1.0, DEFAULT_MATTE, false));
        let full = mean_weight(&trace_m(&g, &world_depth, &scene, 1.0, 1.0, false));
        let score = cols_weight(&trace_m(&g, &world_depth, &scene, 1.0, DEFAULT_MATTE, true), &SAMPLE_COLS);
        println!("ssr matte gate {name}: weight cut 0 {old:.4}, cut {DEFAULT_MATTE} {new:.4}, cut 1 {full:.4}; score {score:.3}");
        assert!(old > 0.01);
        assert!(score > 0.9, "{name}: the score stays high: {score}");
        assert!(new > 0.95 * old && full > 0.95 * old, "{name}: water keeps its (boosted) reflection: {new} / {full} of {old}");
    }
    // Moonlit water stays water: the frame's light is removed from both the water and the ground around it.
    let moon_water = tinted_world([0.15, 0.30, 0.55], MOON);
    let w = mean_weight(&trace_m(&g, &world_depth, &moon_water, 1.0, DEFAULT_MATTE, false));
    let w0 = mean_weight(&trace_m(&g, &world_depth, &moon_water, 1.0, 0.0, false));
    println!("ssr matte gate moonlit water: cut 0 {w0:.4}, default {w:.4}");
    assert!(w > 0.9 * w0, "moonlit blue water keeps it: {w} of {w0}");
}

#[test]
fn the_sky_and_the_horizon_are_untouched_by_the_cut() {
    let Some(g) = gpu() else { return };
    // Blue water under a sky band: the sky pixels get nothing either way, the water's reflection stays.
    let d = sky_world_depth(true);
    let s = |x: usize, y: usize| {
        let (_, v) = uv(x, y);
        if v < -0.05 { [0.15, 0.30, 0.55, 1.0] } else { sky_world_scene(true)(x, y) }
    };
    let old = trace_m(&g, &d, &s, 1.0, 0.0, false);
    let new = trace_m(&g, &d, &s, 1.0, DEFAULT_MATTE, false);
    let mut sky_px = 0;
    for y in 0..H {
        for x in 0..H {
            let (_, v) = uv(2 * x + 1, 2 * y + 1);
            if v > 0.95 {
                sky_px += 1;
                assert_eq!(new[y * H + x], [0.0; 4], "a sky pixel got something");
            }
        }
    }
    assert!(sky_px > 0);
    let (wo, wn) = (mean_weight(&old), mean_weight(&new));
    // The rows nearer the horizon (grazing view, v = -0.3: the sky band is in the mirror there).
    // Every floor row from the horizon down (v = -0.08 .. -0.56): the row nearest the horizon that reflects must keep it,
    // and no row may lose more than 5 %.
    let row = |px: &[[f32; 4]], v: f32| {
        let mut s = 0.0;
        for u in [-0.6f32, -0.3, 0.0, 0.3, 0.6] {
            let (hx, hy) = half_px(u, v);
            s += px[hy * H + hx][3];
        }
        s / 5.0
    };
    let rows: Vec<(f32, f32, f32)> = (0..13).map(|k| -0.08 - 0.04 * k as f32).map(|v| (v, row(&old, v), row(&new, v))).collect();
    println!("ssr matte gate under a sky: floor weight cut 0 {wo:.4}, default {wn:.4}; rows (v, cut 0, default) {rows:.4?}");
    let first = rows.iter().find(|r| r.1 > 0.005).expect("some row near the horizon reflects at cut 0");
    assert!(first.0 > -0.5, "the reflecting rows reach towards the horizon: {first:?}");
    assert!(wn > 0.95 * wo, "the water under the sky keeps its reflection");
    for r in &rows {
        assert!(r.2 >= 0.95 * r.1 - 1e-5, "row {r:?} lost its reflection");
    }
    assert!(new.iter().all(|p| p.iter().all(|v| v.is_finite())));
}

#[test]
fn smart_zero_ignores_the_matte_cut_bit_for_bit() {
    let Some(g) = gpu() else { return };
    let scene = tinted_world(DRY_EARTH, MOON);
    let a = trace_m(&g, &world_depth, &scene, 0.0, 0.0, false);
    let b = trace_m(&g, &world_depth, &scene, 0.0, 1.0, false);
    let c = trace_m(&g, &world_depth, &scene, 0.0, DEFAULT_MATTE, false);
    assert!(bit_equal(&a, &b) && bit_equal(&a, &c), "smart 0 = no material logic, whatever the cut");
    assert!(a.iter().any(|p| p[3] > 0.01));
}

#[test]
fn the_cut_is_monotonic_and_nan_safe() {
    let Some(g) = gpu() else { return };
    let scene = tinted_world(DRY_EARTH, MOON);
    let w: Vec<f32> = [0.0f32, 0.2, 0.4, 0.6, 0.8, 1.0].iter().map(|&c| mean_weight(&trace_m(&g, &world_depth, &scene, 1.0, c, false))).collect();
    println!("ssr matte gate moonlit earth weight by cut 0..1: {w:.4?}");
    for k in 1..w.len() {
        assert!(w[k] <= w[k - 1] + 1e-4, "the cut only removes: {w:?}");
    }
    // NaN / inf / negative colours and depths: finite and bounded.
    let weird_scene = |x: usize, y: usize| match (x * 7 + y * 3) % 23 {
        0 => [f32::NAN, 0.5, 0.5, 1.0],
        1 => [f32::INFINITY, 0.4, 0.4, 1.0],
        2 => [-2.0, 0.4, 0.5, 1.0],
        _ => tinted_world(DRY_EARTH, MOON)(x, y),
    };
    let weird_depth = |x: usize, y: usize| match (x + 5 * y) % 31 {
        0 => 0.0,
        1 => f32::NAN,
        2 => -3.0,
        3 => 1e37,
        _ => world_depth(x, y),
    };
    for cut in [DEFAULT_MATTE, 1.0, f32::NAN] {
        let out = trace_m(&g, &weird_depth, &weird_scene, 1.0, cut, false);
        assert!(out.iter().all(|p| p.iter().all(|v| v.is_finite()) && p[3] >= 0.0 && p[3] <= 0.85 + 1e-3), "cut {cut}: bad output");
    }
}

#[test]
fn telemetry_counts_the_rejected_matte_ground_and_the_kept_water() {
    let Some(g) = gpu() else { return };
    let dirt = stats_m(&g, &world_depth, &tinted_world(DRY_EARTH, MOON), DEFAULT_MATTE);
    println!("ssr telemetry moonlit earth: {}", stats_text(&dirt));
    assert_eq!(dirt.samples, STATS_W * STATS_H);
    assert!(dirt.floors > dirt.samples / 4, "the floor half of the picture: {dirt:?}");
    assert!(dirt.would_reflect > dirt.floors / 2, "the score alone would reflect it: {dirt:?}");
    assert_eq!((dirt.reflective_water + dirt.reflective_other, dirt.rejected), (0, dirt.would_reflect), "all rejected by the gate: {dirt:?}");
    let water = stats_m(&g, &world_depth, &world_with_floor(&blue_water, 1.0), DEFAULT_MATTE);
    println!("ssr telemetry blue water: {}", stats_text(&water));
    assert!(water.reflective_water > water.floors / 2 && water.rejected == 0 && water.reflective_other == 0, "{water:?}");
    // Cut 0: nothing is rejected, the class is still reported.
    let off = stats_m(&g, &world_depth, &world_with_floor(&blue_water, 1.0), 0.0);
    assert!(off.rejected == 0 && off.reflective_water > off.floors / 2, "{off:?}");
}
