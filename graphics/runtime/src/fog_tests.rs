//! WARP tests of the fog estimator (`Fog::run`). They need no game exe: scene colour and depth are synthetic.
//!
//! World (N x N): sky (invalid depth) in the top rows with the fog colour, a strip at the bottom at depth 100 (the
//! farthest geometry, also fog coloured), and a middle band of other colours at depth 20..50. The state texture is
//! read back from the GPU: texel 0 = (F rgb, confidence), texel 1 = (zref, far fraction).

use std::ffi::c_void;

use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::Interface;

use super::*;

const N: usize = 128;
const SKY_ROWS: usize = 32;
const STRIP_ROWS: usize = 8;

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
    B.get_or_init(|| compile_blobs().unwrap_or_else(|e| panic!("fog.hlsl does not compile: {e}")))
}

fn tex(g: &Gpu, fmt: DXGI_FORMAT, data: &[u8], pitch: usize) -> ID3D11Texture2D {
    let desc = D3D11_TEXTURE2D_DESC { Width: N as u32, Height: N as u32, MipLevels: 1, ArraySize: 1, Format: fmt, SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, Usage: D3D11_USAGE_IMMUTABLE, BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32, CPUAccessFlags: 0, MiscFlags: 0 };
    let sd = D3D11_SUBRESOURCE_DATA { pSysMem: data.as_ptr() as *const c_void, SysMemPitch: pitch as u32, SysMemSlicePitch: 0 };
    let mut t = None;
    unsafe { g.dev.CreateTexture2D(&desc, Some(&sd), Some(&mut t)).expect("CreateTexture2D") };
    t.unwrap()
}

fn scene_srv(g: &Gpu, f: impl Fn(usize, usize) -> [f32; 3]) -> (ID3D11Texture2D, ID3D11ShaderResourceView) {
    let mut v = Vec::new();
    for y in 0..N {
        for x in 0..N {
            let c = f(x, y);
            for ch in [c[0], c[1], c[2], 1.0] {
                v.extend_from_slice(&ch.to_le_bytes());
            }
        }
    }
    let t = tex(g, DXGI_FORMAT_R32G32B32A32_FLOAT, &v, N * 16);
    let mut s = None;
    unsafe { g.dev.CreateShaderResourceView(&t, None, Some(&mut s)).unwrap() };
    (t, s.unwrap())
}

fn depth_tex(g: &Gpu, f: impl Fn(usize, usize) -> f32) -> ID3D11Texture2D {
    let mut v = Vec::new();
    for y in 0..N {
        for x in 0..N {
            v.extend_from_slice(&f(x, y).to_le_bytes());
        }
    }
    tex(g, DXGI_FORMAT_R32_FLOAT, &v, N * 4)
}

/// The current state: (texel 0, texel 1).
fn state(g: &Gpu, fog: &Fog) -> ([f32; 4], [f32; 4]) {
    let s = state3(g, fog);
    (s[0], s[1])
}

/// The whole 3x1 state: texel 0 (F, confidence), texel 1 (zref, far fraction, haze age, haze source), texel 2 (H, confidence).
fn state3(g: &Gpu, fog: &Fog) -> [[f32; 4]; 3] {
    unsafe {
        let src: ID3D11Texture2D = ID3D11Texture2D::from_raw_borrowed(&fog.state[fog.cur].tex).unwrap().clone();
        let desc = D3D11_TEXTURE2D_DESC { Width: STATE_W, Height: 1, MipLevels: 1, ArraySize: 1, Format: DXGI_FORMAT_R32G32B32A32_FLOAT, SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, Usage: D3D11_USAGE_STAGING, BindFlags: 0, CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32, MiscFlags: 0 };
        let mut st = None;
        g.dev.CreateTexture2D(&desc, None, Some(&mut st)).unwrap();
        let st = st.unwrap();
        g.ctx.CopyResource(&st, &src);
        let mut m = D3D11_MAPPED_SUBRESOURCE::default();
        g.ctx.Map(&st, 0, D3D11_MAP_READ, 0, Some(&mut m)).unwrap();
        let p = m.pData as *const [f32; 4];
        let out = [*p, *p.add(1), *p.add(2)];
        g.ctx.Unmap(&st, 0);
        out
    }
}

/// Fog colour `fogc` in the sky and in the bottom strip (depth 100); two bands of other colours at depth 50 and 20.
fn colour(fogc: [f32; 3]) -> impl Fn(usize, usize) -> [f32; 3] {
    move |x, y| if y < SKY_ROWS || y >= N - STRIP_ROWS { fogc } else if x < N / 2 { [0.2, 0.3, 0.1] } else { [0.7, 0.5, 0.6] }
}

fn depth(x: usize, y: usize) -> f32 {
    if y < SKY_ROWS { 0.0 } else if y >= N - STRIP_ROWS { 100.0 } else if x < N / 2 { 50.0 } else { 20.0 }
}

/// Only one sampled pixel is far: no sky, every depth valid.
fn depth_no_far(x: usize, y: usize) -> f32 {
    if (x, y) == (40, 40) { 100.0 } else if x < N / 2 { 50.0 } else { 20.0 }
}

fn run(g: &Gpu, fog: &mut Fog, sc: &ID3D11ShaderResourceView, d: &ID3D11Texture2D) {
    assert!(unsafe { fog.run(g.dev.as_raw(), g.ctx.as_raw(), sc.as_raw(), d.as_raw()) }, "run failed");
}

#[test]
fn fog_hlsl_compiles_for_all_entries() {
    let b = blobs();
    for (name, code) in [("vs", &b.vs), ("ztile", &b.ztile), ("zreduce", &b.zreduce), ("ftile", &b.ftile), ("fstate", &b.fstate)] {
        assert_eq!(&code[..4], b"DXBC", "{name}");
    }
}

#[test]
fn the_fog_colour_is_the_average_of_the_farthest_pixels_for_any_colour() {
    let Some(g) = gpu() else { return };
    for (name, f) in [("gray", [0.45f32, 0.45, 0.45]), ("green", [0.30, 0.50, 0.28]), ("orange", [0.60, 0.40, 0.20])] {
        let (_t, sc) = scene_srv(&g, colour(f));
        let d = depth_tex(&g, depth);
        let mut fog = unsafe { Fog::new(g.dev.as_raw(), blobs()) }.expect("Fog::new");
        run(&g, &mut fog, &sc, &d);
        let (s0, s1) = state(&g, &fog);
        println!("fog {name}: F {:?} confidence {} zref {} far fraction {}", &s0[..3], s0[3], s1[0], s1[1]);
        for c in 0..3 {
            assert!((s0[c] - f[c]).abs() < 1e-3, "{name}: F channel {c} is {} want {}", s0[c], f[c]);
        }
        assert_eq!(s0[3], 1.0, "{name}: F must be known");
        assert!((s1[0] - 100.0).abs() < 1e-3, "{name}: zref {}", s1[0]);
        // sky 32 rows + strip 8 rows of 128 = 31 %
        assert!((s1[1] - 40.0 / 128.0).abs() < 0.02, "{name}: far fraction {}", s1[1]);
    }
}

#[test]
fn too_few_far_pixels_leave_the_fog_colour_unknown() {
    let Some(g) = gpu() else { return };
    let (_t, sc) = scene_srv(&g, colour([0.45; 3]));
    let d = depth_tex(&g, depth_no_far);
    let mut fog = unsafe { Fog::new(g.dev.as_raw(), blobs()) }.expect("Fog::new");
    run(&g, &mut fog, &sc, &d);
    let (s0, s1) = state(&g, &fog);
    assert_eq!(s0[3], 0.0, "confidence must stay 0 with a single far pixel, state {s0:?}");
    assert!((s1[0] - 100.0).abs() < 1e-3, "zref is still measured: {}", s1[0]);
}

#[test]
fn the_state_follows_the_measurement_smoothly_and_forgets_it_when_it_disappears() {
    let Some(g) = gpu() else { return };
    let (_t1, sc1) = scene_srv(&g, colour([0.2, 0.2, 0.2]));
    let (_t2, sc2) = scene_srv(&g, colour([0.6, 0.6, 0.6]));
    let d = depth_tex(&g, depth);
    let none = depth_tex(&g, depth_no_far);
    let mut fog = unsafe { Fog::new(g.dev.as_raw(), blobs()) }.expect("Fog::new");
    run(&g, &mut fog, &sc1, &d);
    run(&g, &mut fog, &sc2, &d);
    let (s0, _) = state(&g, &fog);
    let want = 0.2 + (0.6 - 0.2) * FOLLOW_F;
    assert!((s0[0] - want).abs() < 1e-3, "F must move {FOLLOW_F} of the way: {} want {want}", s0[0]);
    for _ in 0..40 {
        run(&g, &mut fog, &sc2, &d);
    }
    assert!((state(&g, &fog).0[0] - 0.6).abs() < 0.01, "F must converge");
    // The far pixels vanish: the confidence falls to 0 over a few frames (never a sudden jump), F is not invented.
    run(&g, &mut fog, &sc2, &none);
    let c1 = state(&g, &fog).0[3];
    assert!(c1 > 0.5 && c1 < 1.0, "confidence falls gradually: {c1}");
    for _ in 0..60 {
        run(&g, &mut fog, &sc2, &none);
    }
    assert_eq!(state(&g, &fog).0[3], 0.0, "confidence reaches 0");
}

// ---- the haze colour of the aerial perspective (texel 2) ---------------------------------------------------------------

const HIGH_SKY: [f32; 3] = [0.20, 0.30, 0.70];
const HORIZON: [f32; 3] = [0.60, 0.65, 0.72];
const TREES: [f32; 3] = [0.30, 0.42, 0.22];
const GROUND: [f32; 3] = [0.45, 0.38, 0.25];

/// Looking across the valley: deep blue high sky (rows 0..16), a light horizon band (16..32, both the far plane 1.0), far
/// trees at 0.19 (32..64), the ground near the camera below.
fn valley_colour(_x: usize, y: usize) -> [f32; 3] {
    if y < 16 { HIGH_SKY } else if y < SKY_ROWS { HORIZON } else if y < 64 { TREES } else { GROUND }
}
fn valley_depth(_x: usize, y: usize) -> f32 {
    if y < SKY_ROWS { 1.0 } else if y < 64 { 0.19 } else { 0.01 + (N - y) as f32 * 0.001 }
}
/// Tilted down at the same place: no sky, the far trees at the top, the slope below.
fn slope_colour(_x: usize, y: usize) -> [f32; 3] {
    if y < 40 { TREES } else { GROUND }
}
fn slope_depth(_x: usize, y: usize) -> f32 {
    if y < 40 { 0.19 } else { 0.01 + (N - y) as f32 * 0.001 }
}
/// The slope with a sliver of sky in the top-left corner: 8 of the 4096 samples (0.2 %), between SKY_STAY and SKY_ENTER.
fn sliver_depth(x: usize, y: usize) -> f32 {
    if x < 8 && y < 4 { 1.0 } else { slope_depth(x, y) }
}
fn sliver_colour(x: usize, y: usize) -> [f32; 3] {
    if x < 8 && y < 4 { HORIZON } else { slope_colour(x, y) }
}

fn close(a: [f32; 4], b: [f32; 3], tol: f32) -> bool {
    (0..3).all(|c| (a[c] - b[c]).abs() <= tol)
}

#[test]
fn the_haze_colour_is_the_horizon_band_of_the_sky() {
    let Some(g) = gpu() else { return };
    let (_t, sc) = scene_srv(&g, valley_colour);
    let d = depth_tex(&g, valley_depth);
    let mut fog = unsafe { Fog::new(g.dev.as_raw(), blobs()) }.expect("Fog::new");
    run(&g, &mut fog, &sc, &d);
    let s = state3(&g, &fog);
    println!("valley: F {:?} H {:?} age/source {:?}", s[0], s[2], &s[1][2..]);
    assert!(close(s[2], HORIZON, 1e-3), "H must be the lowest sky band, not the deep blue above it: {:?}", s[2]);
    assert_eq!(s[2][3], 1.0, "H known at once");
    assert_eq!((s[1][2], s[1][3]), (0.0, HAZE_SKY as f32), "source sky, age 0");
    // The sky also given as invalid depth (0) counts the same.
    let d0 = depth_tex(&g, |x, y| if y < SKY_ROWS { 0.0 } else { valley_depth(x, y) });
    let mut fog0 = unsafe { Fog::new(g.dev.as_raw(), blobs()) }.expect("Fog::new");
    run(&g, &mut fog0, &sc, &d0);
    assert!(close(state3(&g, &fog0)[2], HORIZON, 1e-3));
}

#[test]
fn the_haze_colour_is_held_when_the_camera_tilts_down_and_never_flickers() {
    let Some(g) = gpu() else { return };
    let (_t1, sky) = scene_srv(&g, valley_colour);
    let dsky = depth_tex(&g, valley_depth);
    let (_t2, down) = scene_srv(&g, slope_colour);
    let ddown = depth_tex(&g, slope_depth);
    let mut fog = unsafe { Fog::new(g.dev.as_raw(), blobs()) }.expect("Fog::new");
    for _ in 0..5 {
        run(&g, &mut fog, &sky, &dsky);
    }
    let h0 = state3(&g, &fog)[2];
    let mut prev = h0;
    for i in 1..=300 {
        run(&g, &mut fog, &down, &ddown);
        let s = state3(&g, &fog);
        // F (the AO's estimate) becomes the far trees, as before; H must not.
        assert_eq!(s[1][3], HAZE_HELD as f32, "frame {i}: source must be 'held'");
        assert_eq!(s[1][2], i as f32, "frame {i}: age in frames");
        assert_eq!(s[2][3], 1.0, "frame {i}: a held colour never loses confidence (no fade, no flicker)");
        for c in 0..3 {
            assert!((s[2][c] - prev[c]).abs() < 1e-3, "frame {i}: H jumped {:?} -> {:?}", prev, s[2]);
        }
        prev = s[2];
    }
    let s = state3(&g, &fog);
    assert!(close(s[0], TREES, 1e-2), "F follows the farthest pixels (the trees): {:?}", s[0]);
    assert!(close(s[2], HORIZON, 0.03), "after 5 s looking down H is still the horizon colour: {:?}", s[2]);
    // Looking up again: back to the sky source, no jump.
    run(&g, &mut fog, &sky, &dsky);
    let s = state3(&g, &fog);
    assert_eq!((s[1][2], s[1][3]), (0.0, HAZE_SKY as f32));
    assert!(close(s[2], [prev[0], prev[1], prev[2]], 0.01));
}

#[test]
fn without_any_sky_the_haze_colour_is_a_weaker_neutral_fallback_until_the_sky_shows() {
    let Some(g) = gpu() else { return };
    let (_t1, down) = scene_srv(&g, slope_colour);
    let ddown = depth_tex(&g, slope_depth);
    let (_t2, sky) = scene_srv(&g, valley_colour);
    let dsky = depth_tex(&g, valley_depth);
    let mut fog = unsafe { Fog::new(g.dev.as_raw(), blobs()) }.expect("Fog::new");
    run(&g, &mut fog, &down, &ddown);
    let s = state3(&g, &fog);
    println!("no sky ever: H {:?} source {}", s[2], s[1][3]);
    assert_eq!(s[1][3], HAZE_FALLBACK as f32);
    assert_eq!(s[2][3], FALLBACK_CONF);
    // Neutral (a grey a touch cooler), not the trees' green.
    assert!(s[2][2] > s[2][0] && (s[2][1] - s[2][0]).abs() < 0.05, "{:?}", s[2]);
    let fb = s[2];
    // The sky appears: source sky, the colour and the confidence move smoothly (FOLLOW_H per frame).
    run(&g, &mut fog, &sky, &dsky);
    let s = state3(&g, &fog);
    assert_eq!(s[1][3], HAZE_SKY as f32);
    let want = fb[0] + (HORIZON[0] - fb[0]) * FOLLOW_H;
    assert!((s[2][0] - want).abs() < 1e-3, "{} want {want}", s[2][0]);
    assert!(s[2][3] > FALLBACK_CONF && s[2][3] < 1.0);
    for _ in 0..80 {
        run(&g, &mut fog, &sky, &dsky);
    }
    assert!(close(state3(&g, &fog)[2], HORIZON, 1e-2));
}

#[test]
fn a_sliver_of_sky_between_the_thresholds_keeps_the_current_source() {
    let Some(g) = gpu() else { return };
    let (_t1, sl) = scene_srv(&g, sliver_colour);
    let dsl = depth_tex(&g, sliver_depth);
    let (_t2, sky) = scene_srv(&g, valley_colour);
    let dsky = depth_tex(&g, valley_depth);
    // From nothing: 0.2 % of sky is not enough to take the sky as the source (needs SKY_ENTER).
    let mut fresh = unsafe { Fog::new(g.dev.as_raw(), blobs()) }.expect("Fog::new");
    run(&g, &mut fresh, &sl, &dsl);
    assert_ne!(state3(&g, &fresh)[1][3], HAZE_SKY as f32);
    // From the sky: the same sliver keeps it (down to SKY_STAY): no source flip-flop while the horizon touches the top edge.
    let mut fog = unsafe { Fog::new(g.dev.as_raw(), blobs()) }.expect("Fog::new");
    run(&g, &mut fog, &sky, &dsky);
    for _ in 0..10 {
        run(&g, &mut fog, &sl, &dsl);
        assert_eq!(state3(&g, &fog)[1][3], HAZE_SKY as f32);
    }
}

#[test]
fn the_status_read_back_reports_the_haze_source() {
    let Some(g) = gpu() else { return };
    let (_t, sc) = scene_srv(&g, valley_colour);
    let d = depth_tex(&g, valley_depth);
    let mut fog = unsafe { Fog::new(g.dev.as_raw(), blobs()) }.expect("Fog::new");
    // The ring reads the copy of an earlier run once the GPU is done (never waits): a few runs are enough on WARP.
    let mut seen = None;
    for _ in 0..20 {
        run(&g, &mut fog, &sc, &d);
        unsafe { g.ctx.Flush() };
        if let Some(h) = haze_info() {
            seen = Some(h);
        }
    }
    // Other tests run in parallel and write the same globals: only check that something consistent was read.
    let h = seen.expect("a state was read back");
    assert!(h.source <= HAZE_FALLBACK && h.confidence.is_finite());
    assert!(haze_text(Some(HazeInfo { source: HAZE_HELD, age_frames: 120.0, confidence: 1.0, colour: HORIZON })).contains("held from the sky seen 120 frames ago"));
    assert!(haze_text(Some(HazeInfo { source: HAZE_SKY, age_frames: 0.0, confidence: 1.0, colour: HORIZON })).contains("from the sky"));
    assert!(haze_text(Some(HazeInfo { source: HAZE_FALLBACK, age_frames: 3.0, confidence: 0.6, colour: HORIZON })).contains("fallback"));
    assert!(haze_text(None).contains("not read back"));
}

#[test]
fn run_leaves_no_slot_of_ours_bound_and_restores_the_pipeline() {
    let Some(g) = gpu() else { return };
    let (_t, sc) = scene_srv(&g, colour([0.45; 3]));
    let d = depth_tex(&g, depth);
    let mut fog = unsafe { Fog::new(g.dev.as_raw(), blobs()) }.expect("Fog::new");
    unsafe {
        g.ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_POINTLIST);
        g.ctx.PSSetShaderResources(0, Some(&[Some(sc.clone())]));
    }
    run(&g, &mut fog, &sc, &d);
    unsafe {
        assert_eq!(g.ctx.IAGetPrimitiveTopology(), D3D_PRIMITIVE_TOPOLOGY_POINTLIST);
        let mut srv: [Option<ID3D11ShaderResourceView>; 6] = Default::default();
        g.ctx.PSGetShaderResources(10, Some(&mut srv));
        assert!(srv.iter().all(|s| s.is_none()), "t10..t15 must be left empty");
        let mut s0 = [None];
        g.ctx.PSGetShaderResources(0, Some(&mut s0));
        assert_eq!(s0[0].as_ref().unwrap().as_raw(), sc.as_raw(), "t0 not restored");
    }
    assert!(!crate::d3d::ignoring(), "ignore flag left on");
}
