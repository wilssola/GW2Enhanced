//! WARP tests of the contact shadows (`Contact::run`). They need no game exe: the depth is synthetic, ray traced from a
//! camera at the origin looking along +z (vertical half fov tan 0.5, the AO's assumption), so the depth of a pixel is the
//! z of the surface point. The visibility textures are read back from the GPU.
//!
//! World (metres): the ground plane y = -2, boxes standing on it or floating above it; sky above the horizon. The depth
//! texture is NORMALISED like the engine's (metres / RANGE_M, `depth_unit` = RANGE_M); the tests used to feed metres with
//! a depth unit of 1, which the new unit rules (metres per 1.0 of a normalised depth, old values <= 2 = default) refuse.

use std::ffi::c_void;

use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::Interface;

use super::*;

const W: usize = 512;
const H: usize = 512;
const HW: usize = W / 2;
const HH: usize = H / 2;
const TAN: f32 = TAN_HALF_FOV;
/// Metres per 1.0 of the depth texture (the far plane).
const RANGE_M: f32 = crate::aerial::DEFAULT_UNIT;

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
    B.get_or_init(|| compile_blobs().unwrap_or_else(|e| panic!("contact.hlsl does not compile: {e}")))
}

fn f16_to_f32(h: u16) -> f32 {
    let (s, e, m) = ((h >> 15) & 1, (h >> 10) & 0x1f, (h & 0x3ff) as f32);
    let v = match e {
        0 => m * 2f32.powi(-24),
        31 => if m == 0.0 { f32::INFINITY } else { f32::NAN },
        _ => (1.0 + m / 1024.0) * 2f32.powi(e as i32 - 15),
    };
    if s == 1 { -v } else { v }
}

// ---- geometry -------------------------------------------------------------------------------------

/// Direction (z = 1) of the ray through pixel (x, y) of the full-size image.
fn ray(x: usize, y: usize) -> [f32; 3] {
    let ndc_x = (x as f32 + 0.5) / W as f32 * 2.0 - 1.0;
    let ndc_y = 1.0 - (y as f32 + 0.5) / H as f32 * 2.0;
    [ndc_x * (W as f32 / H as f32) * TAN, ndc_y * TAN, 1.0]
}

/// z of the ray at its intersection with the plane n . p = c, or None.
fn plane_t(d: [f32; 3], n: [f32; 3], c: f32) -> Option<f32> {
    let nd = n[0] * d[0] + n[1] * d[1] + n[2] * d[2];
    if nd.abs() < 1e-6 {
        return None;
    }
    let t = c / nd;
    (t > 0.0).then_some(t)
}

#[derive(Clone, Copy)]
struct Aabb {
    lo: [f32; 3],
    hi: [f32; 3],
}

/// Entry z of the ray into the box (the camera is outside of it), or None.
fn box_t(d: [f32; 3], b: &Aabb) -> Option<f32> {
    let (mut t0, mut t1) = (0.0f32, f32::MAX);
    for a in 0..3 {
        let (lo, hi) = (b.lo[a], b.hi[a]);
        if d[a].abs() < 1e-9 {
            if 0.0 < lo || 0.0 > hi {
                return None;
            }
        } else {
            let (mut a0, mut a1) = (lo / d[a], hi / d[a]);
            if a0 > a1 {
                std::mem::swap(&mut a0, &mut a1);
            }
            t0 = t0.max(a0);
            t1 = t1.min(a1);
            if t0 > t1 {
                return None;
            }
        }
    }
    Some(t0)
}

struct Scene {
    boxes: Vec<Aabb>,
    planes: Vec<([f32; 3], f32)>,
}

impl Scene {
    fn depth(&self, x: usize, y: usize) -> f32 {
        let d = ray(x, y);
        let mut best = f32::MAX;
        for (n, c) in &self.planes {
            if let Some(t) = plane_t(d, *n, *c) {
                best = best.min(t);
            }
        }
        for b in &self.boxes {
            if let Some(t) = box_t(d, b) {
                best = best.min(t);
            }
        }
        if best == f32::MAX || best > 5000.0 { 0.0 } else { best } // 0 = invalid = sky
    }

    fn depths(&self) -> Vec<f32> {
        (0..W * H).map(|i| self.depth(i % W, i / W)).collect()
    }
}

const GROUND: ([f32; 3], f32) = ([0.0, 1.0, 0.0], -2.0);

fn ground_only() -> Scene {
    Scene { boxes: vec![], planes: vec![GROUND] }
}

/// A box floating 0.2 m above the ground, 2 m wide, 1 m tall, 2 m deep, 12 m away.
fn floating_box() -> Scene {
    Scene { boxes: vec![Aabb { lo: [-1.0, -1.8, 12.0], hi: [1.0, -0.8, 14.0] }], planes: vec![GROUND] }
}

/// The depth texture of `d` (metres; 0 / garbage kept as they are) in the engine's normalised encoding.
fn depth_tex(g: &Gpu, d: &[f32]) -> ID3D11Texture2D {
    let bytes: Vec<u8> = d.iter().flat_map(|v| (v / RANGE_M).to_le_bytes()).collect();
    let desc = D3D11_TEXTURE2D_DESC { Width: W as u32, Height: H as u32, MipLevels: 1, ArraySize: 1, Format: DXGI_FORMAT_R32_FLOAT, SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, Usage: D3D11_USAGE_IMMUTABLE, BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32, CPUAccessFlags: 0, MiscFlags: 0 };
    let sd = D3D11_SUBRESOURCE_DATA { pSysMem: bytes.as_ptr() as *const c_void, SysMemPitch: (W * 4) as u32, SysMemSlicePitch: 0 };
    let mut t = None;
    unsafe { g.dev.CreateTexture2D(&desc, Some(&sd), Some(&mut t)).expect("CreateTexture2D") };
    t.unwrap()
}

/// Reads a half-size R16_FLOAT texture back (row-major HW x HH).
fn read_half(g: &Gpu, tex: Obj) -> Vec<f32> {
    unsafe {
        let src: ID3D11Texture2D = ID3D11Texture2D::from_raw_borrowed(&tex).unwrap().clone();
        let desc = D3D11_TEXTURE2D_DESC { Width: HW as u32, Height: HH as u32, MipLevels: 1, ArraySize: 1, Format: DXGI_FORMAT_R16_FLOAT, SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, Usage: D3D11_USAGE_STAGING, BindFlags: 0, CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32, MiscFlags: 0 };
        let mut st = None;
        g.dev.CreateTexture2D(&desc, None, Some(&mut st)).unwrap();
        let st = st.unwrap();
        g.ctx.CopyResource(&st, &src);
        let mut m = D3D11_MAPPED_SUBRESOURCE::default();
        g.ctx.Map(&st, 0, D3D11_MAP_READ, 0, Some(&mut m)).unwrap();
        let mut out = Vec::with_capacity(HW * HH);
        for y in 0..HH {
            let row = std::slice::from_raw_parts((m.pData as *const u8).add(y * m.RowPitch as usize) as *const u16, HW);
            out.extend(row.iter().map(|h| f16_to_f32(*h)));
        }
        g.ctx.Unmap(&st, 0);
        out
    }
}

struct Vis {
    out: Vec<f32>,
    raw: Vec<f32>,
}

fn params(dir: [f32; 3], fade: f32) -> Params {
    Params { width: W as u32, height: H as u32, dir, fade, length: 1.0, unit: RANGE_M }
}

fn run_with(g: &Gpu, c: &mut Contact, depth: &ID3D11Texture2D, p: &Params) -> Vis {
    assert!(unsafe { c.run(g.dev.as_raw(), g.ctx.as_raw(), depth.as_raw(), p) }, "run failed");
    let (o, r) = c.textures();
    Vis { out: read_half(g, o), raw: read_half(g, r) }
}

fn run(g: &Gpu, s: &Scene, p: &Params) -> Vis {
    let d = depth_tex(g, &s.depths());
    let mut c = unsafe { Contact::new(g.dev.as_raw(), Some(blobs())) }.expect("Contact::new");
    run_with(g, &mut c, &d, p)
}

/// The directions under test: the fixed one and a few suns seen on the screen (left, right, high, near the horizon).
fn directions() -> Vec<[f32; 3]> {
    let (d, _, _) = plan(true, None, None, 1.0);
    vec![d, sun_direction(0.8, 0.2, 1.0), sun_direction(0.2, 0.1, 1.0), sun_direction(0.5, 0.45, 1.0), normalize3([0.9, 0.4, 0.0]), normalize3([-0.5, 0.8, -0.3])]
}

/// Half-size pixel -> the scene's full-size pixel whose depth it reads.
fn full_of(hx: usize, hy: usize) -> (usize, usize) {
    ((hx * 2).min(W - 1), (hy * 2).min(H - 1))
}

// ---- tests ------------------------------------------------------------------------------------------

#[test]
fn contact_hlsl_compiles_for_all_entries() {
    let b = blobs();
    for (name, code) in [("vs", &b.vs), ("contact", &b.contact), ("blur", &b.blur)] {
        assert_eq!(&code[..4], b"DXBC", "{name}");
    }
}

#[test]
fn a_flat_plane_gets_no_shadow_for_any_light_direction() {
    let Some(g) = gpu() else { return };
    let planes: [(&str, Scene); 3] = [
        ("ground", ground_only()),
        ("wall", Scene { boxes: vec![], planes: vec![([0.0, 0.0, 1.0], 15.0)] }),
        ("tilted plane", Scene { boxes: vec![], planes: vec![(normalize3([0.2, 1.0, -0.3]), -2.5)] }),
    ];
    for (name, s) in &planes {
        for dir in directions() {
            let r = run(&g, s, &params(dir, 1.0));
            let mut worst = 1.0f32;
            for hy in 0..HH {
                for hx in 0..HW {
                    let (x, y) = full_of(hx, hy);
                    // Stay clear of the horizon band, where a one-pixel step changes the depth by metres.
                    if s.depth(x, y) > 0.0 && s.depth(x, y) < 120.0 {
                        worst = worst.min(r.raw[hy * HW + hx]).min(r.out[hy * HW + hx]);
                    }
                }
            }
            println!("{name} dir {dir:?}: darkest visibility {worst}");
            assert!(worst > 0.995, "{name}: a flat plane must not self-shadow (dir {dir:?}, darkest {worst})");
        }
    }
}

/// The ground pixels (half-size coordinates) under and next to the floating box, visible from the camera.
fn shadow_stats(s: &Scene, r: &Vis) -> (usize, f32, f32) {
    let (mut n, mut sum_x, mut darkest) = (0usize, 0.0f32, 1.0f32);
    for hy in 0..HH {
        for hx in 0..HW {
            let (x, y) = full_of(hx, hy);
            let d = s.depth(x, y);
            let v = r.out[hy * HW + hx];
            if d > 0.0 && v < 0.7 {
                n += 1;
                sum_x += hx as f32;
                darkest = darkest.min(v);
            }
        }
    }
    (n, if n > 0 { sum_x / n as f32 } else { 0.0 }, darkest)
}

#[test]
fn a_floating_box_shadows_the_ground_beneath_its_edge_towards_the_sun() {
    let Some(g) = gpu() else { return };
    let s = floating_box();
    let toward_right = run(&g, &s, &params(normalize3([0.4, 0.9, 0.15]), 1.0));
    let toward_left = run(&g, &s, &params(normalize3([-0.4, 0.9, 0.15]), 1.0));
    let (nr, mx_r, dark_r) = shadow_stats(&s, &toward_right);
    let (nl, mx_l, dark_l) = shadow_stats(&s, &toward_left);
    println!("sun right: {nr} shadowed pixels, mean x {mx_r}, darkest {dark_r}; sun left: {nl}, mean x {mx_l}, darkest {dark_l}");
    assert!(nr >= 20 && nl >= 20, "the box must shadow the ground ({nr}, {nl})");
    assert!(dark_r < 0.3 && dark_l < 0.3, "the shadow must be dark enough to see ({dark_r}, {dark_l})");
    // (the receiver is on the side AWAY from the light: with the sun on the right the shadowed ground lies further left)
    assert!(mx_r < mx_l - 0.3, "the shadow must move with the light (mean x {mx_r} vs {mx_l})");
    // Far from the box the ground is lit, and the sky is untouched.
    for hy in 0..HH {
        for hx in 0..HW {
            let (x, y) = full_of(hx, hy);
            let d = s.depth(x, y);
            let v = toward_right.out[hy * HW + hx];
            let ray = ray(x, y);
            if d == 0.0 {
                assert_eq!(v, 1.0, "sky at ({x}, {y}) was darkened");
            } else if (ray[0] * d).abs() > 3.5 && d < 120.0 {
                assert!(v > 0.995, "ground at ({x}, {y}) far from the box is shadowed: {v}");
            }
        }
    }
}

#[test]
fn the_weight_scales_the_shadow_and_zero_removes_it() {
    let Some(g) = gpu() else { return };
    let s = floating_box();
    let dir = normalize3([0.4, 0.9, 0.15]);
    let full = shadow_stats(&s, &run(&g, &s, &params(dir, 1.0))).2;
    let half = shadow_stats(&s, &run(&g, &s, &params(dir, 0.5))).2;
    let none = run(&g, &s, &params(dir, 0.0));
    assert!(none.out.iter().chain(none.raw.iter()).all(|v| *v == 1.0), "weight 0 must leave everything lit");
    assert!(half > full + 0.2 && half > 0.45, "weight 0.5 must halve the darkening (full {full}, half {half})");
}

#[test]
fn a_near_object_does_not_shadow_the_far_ground_behind_its_silhouette() {
    let Some(g) = gpu() else { return };
    // A tall box 5 m away in front of the ground that stretches to 60 m: the far ground right next to the box silhouette
    // is 10x deeper than the box, so it must not be darkened (thickness limit, depth-aware blur).
    let s = Scene { boxes: vec![Aabb { lo: [-0.5, -2.0, 5.0], hi: [0.5, 1.0, 5.5] }], planes: vec![GROUND] };
    for dir in directions() {
        let r = run(&g, &s, &params(dir, 1.0));
        let mut worst = 1.0f32;
        for hy in 0..HH {
            for hx in 0..HW {
                let (x, y) = full_of(hx, hy);
                let d = s.depth(x, y);
                if d > 25.0 && d < 100.0 {
                    worst = worst.min(r.out[hy * HW + hx]);
                }
            }
        }
        assert!(worst > 0.99, "far ground behind a near box got a halo (dir {dir:?}, darkest {worst})");
    }
}

#[test]
fn the_output_is_finite_in_range_and_identical_for_identical_input() {
    let Some(g) = gpu() else { return };
    let s = floating_box();
    let d = depth_tex(&g, &s.depths());
    let mut c = unsafe { Contact::new(g.dev.as_raw(), Some(blobs())) }.expect("Contact::new");
    let p = params(normalize3([0.4, 0.9, 0.15]), 1.0);
    let a = run_with(&g, &mut c, &d, &p);
    let b = run_with(&g, &mut c, &d, &p);
    assert_eq!(a.out, b.out, "identical input must give identical output");
    assert_eq!(a.raw, b.raw);
    for v in a.out.iter().chain(a.raw.iter()) {
        assert!(v.is_finite() && *v >= 0.0 && *v <= 1.0, "bad value {v}");
    }
    // Garbage depth (NaN, infinity, zero, negative, huge) never produces NaN or out-of-range visibility.
    let mut bad = s.depths();
    for (i, v) in bad.iter_mut().enumerate() {
        match i % 11 {
            0 => *v = f32::NAN,
            1 => *v = f32::INFINITY,
            2 => *v = -5.0,
            3 => *v = 1e38,
            _ => {}
        }
    }
    let bd = depth_tex(&g, &bad);
    let r = run_with(&g, &mut c, &bd, &p);
    for v in r.out.iter().chain(r.raw.iter()) {
        assert!(v.is_finite() && *v >= 0.0 && *v <= 1.0, "bad value {v} from garbage depth");
    }
}

/// Amplitude of the spatial frequency (fx, fy) in cycles per pixel in the region, relative to nothing: the DFT bin.
fn bin(img: &[f32], x0: usize, x1: usize, y0: usize, y1: usize, fx: f32, fy: f32) -> f32 {
    let (mut re, mut im, mut n) = (0.0f32, 0.0f32, 0.0f32);
    for y in y0..y1 {
        for x in x0..x1 {
            let ph = std::f32::consts::TAU * (fx * x as f32 + fy * y as f32);
            re += img[y * HW + x] * ph.cos();
            im += img[y * HW + x] * ph.sin();
            n += 1.0;
        }
    }
    2.0 * (re * re + im * im).sqrt() / n
}

#[test]
fn the_blur_leaves_no_visible_grid_or_high_frequency_noise_at_strength_one() {
    let Some(g) = gpu() else { return };
    // A tall box standing on the ground with a low sun from the side: the ground beside it gets a shadow band with soft
    // edges (the step jitter makes the raw visibility noisy exactly there), at strength 1 and the longest ray.
    let s = Scene { boxes: vec![Aabb { lo: [-1.0, -2.0, 12.0], hi: [1.0, 0.0, 15.0] }], planes: vec![GROUND] };
    let mut p = params(normalize3([0.9, 0.25, 0.0]), 1.0);
    p.length = 2.0;
    let r = run(&g, &s, &p);
    // The pixels of the band: ground, shadowed in the blurred image, with ground neighbours two pixels away on both sides.
    let is_ground = |hx: usize, hy: usize| {
        let (x, y) = full_of(hx, hy);
        let d = s.depth(x, y);
        d > 0.0 && d < 120.0 && ray(x, y)[1] * d < -1.99
    };
    let mut band: Vec<(usize, usize)> = Vec::new();
    for hy in 2..HH - 2 {
        for hx in 3..HW - 3 {
            if r.out[hy * HW + hx] < 0.9 && (0..=6).all(|i| is_ground(hx + i - 3, hy)) {
                band.push((hx, hy));
            }
        }
    }
    // The second difference along the row: 0 for a smooth ramp, large for noise.
    let lap = |img: &[f32]| band.iter().map(|(x, y)| (img[y * HW + x - 1] - 2.0 * img[y * HW + x] + img[y * HW + x + 1]).abs()).sum::<f32>() / band.len().max(1) as f32;
    let (lap_raw, lap_out) = (lap(&r.raw), lap(&r.out));
    // Grid frequencies (period 2 and 4, axes and diagonals) over the area around the band.
    let freqs = [(0.5, 0.0), (0.0, 0.5), (0.5, 0.5), (0.25, 0.0), (0.0, 0.25), (0.25, 0.25), (0.25, 0.5), (0.5, 0.25)];
    let (x0, x1, y0, y1) = (24, HW - 24, 76, HH - 4);
    let peak = |img: &[f32]| freqs.iter().map(|(fx, fy)| bin(img, x0, x1, y0, y1, *fx, *fy)).fold(0.0f32, f32::max);
    let (pk_raw, pk_out) = (peak(&r.raw), peak(&r.out));
    let dark = band.iter().map(|(x, y)| r.out[y * HW + x]).fold(1.0f32, f32::min);
    println!("{} band pixels, darkest {dark:.3}; mean |second difference|: raw {lap_raw:.4} -> blurred {lap_out:.4}; strongest grid frequency amplitude: raw {pk_raw:.4} -> blurred {pk_out:.4}", band.len());
    assert!(band.len() >= 20 && dark < 0.8, "the test needs a real shadow band in view ({} pixels, darkest {dark})", band.len());
    assert!(lap_raw > 0.02, "the raw visibility is supposed to be noisy here ({lap_raw}); the test would prove nothing");
    assert!(lap_out < 0.4 * lap_raw, "the blur must cut the noise (raw {lap_raw}, blurred {lap_out})");
    assert!(lap_out < 0.03, "residual high-frequency energy too high: {lap_out}");
    assert!(pk_out < 0.02, "a grid frequency survives the blur: {pk_out}");
}

#[test]
fn run_leaves_no_slot_of_ours_bound_and_restores_the_pipeline() {
    let Some(g) = gpu() else { return };
    let s = ground_only();
    let d = depth_tex(&g, &s.depths());
    let mut c = unsafe { Contact::new(g.dev.as_raw(), Some(blobs())) }.expect("Contact::new");
    unsafe {
        g.ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_POINTLIST);
    }
    run_with(&g, &mut c, &d, &params(FIXED_DIR, 1.0));
    unsafe {
        assert_eq!(g.ctx.IAGetPrimitiveTopology(), D3D_PRIMITIVE_TOPOLOGY_POINTLIST);
        let mut srv: [Option<ID3D11ShaderResourceView>; 4] = Default::default();
        g.ctx.PSGetShaderResources(10, Some(&mut srv));
        assert!(srv.iter().all(|s| s.is_none()), "t10..t13 must be left empty");
    }
    assert!(!crate::d3d::ignoring(), "ignore flag left on");
}

#[test]
fn bind_sets_t22_and_b10_and_unbind_clears_them() {
    let Some(g) = gpu() else { return };
    let s = ground_only();
    let d = depth_tex(&g, &s.depths());
    let mut c = unsafe { Contact::new(g.dev.as_raw(), Some(blobs())) }.expect("Contact::new");
    run_with(&g, &mut c, &d, &params(FIXED_DIR, 1.0));
    unsafe {
        assert!(c.bind(g.ctx.as_raw(), Some(c.out_srv()), b10_values(0.5, 0.35, 400.0, 1000.0)));
        let mut srv = [None];
        g.ctx.PSGetShaderResources(SRV_SLOT, Some(&mut srv));
        let v = srv[0].take().expect("t22 is not bound");
        let mut vd = D3D11_SHADER_RESOURCE_VIEW_DESC::default();
        v.GetDesc(&mut vd);
        assert_eq!(vd.Format, DXGI_FORMAT_R16_FLOAT);
        let mut cbs = [None];
        g.ctx.PSGetConstantBuffers(CB_SLOT, Some(&mut cbs));
        let cb = cbs[0].take().expect("b10 is not bound");
        let sd = D3D11_BUFFER_DESC { ByteWidth: 16, Usage: D3D11_USAGE_STAGING, BindFlags: 0, CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32, ..Default::default() };
        let mut st = None;
        g.dev.CreateBuffer(&sd, None, Some(&mut st)).unwrap();
        let st = st.unwrap();
        g.ctx.CopyResource(&st, &cb);
        let mut m = D3D11_MAPPED_SUBRESOURCE::default();
        g.ctx.Map(&st, 0, D3D11_MAP_READ, 0, Some(&mut m)).unwrap();
        let vals = *(m.pData as *const [f32; 4]);
        g.ctx.Unmap(&st, 0);
        assert_eq!(vals, [0.5, 0.35, 400.0, 1000.0]);
        // Without a visibility (contact off) t22 is empty but b10 stays bound for the aerial perspective.
        assert!(c.bind(g.ctx.as_raw(), None, b10_values(0.0, 0.35, 400.0, 1000.0)));
        let mut srv = [None];
        g.ctx.PSGetShaderResources(SRV_SLOT, Some(&mut srv));
        assert!(srv[0].is_none());
        unbind(g.ctx.as_raw());
        let mut cbs = [None];
        g.ctx.PSGetConstantBuffers(CB_SLOT, Some(&mut cbs));
        assert!(cbs[0].is_none());
    }
}

#[test]
fn an_object_without_shaders_still_serves_the_aerial_constant_buffer() {
    let Some(g) = gpu() else { return };
    let c = unsafe { Contact::new(g.dev.as_raw(), None) }.expect("Contact::new without shaders");
    assert!(!c.has_shaders());
    unsafe {
        assert!(c.bind(g.ctx.as_raw(), None, b10_values(0.0, 0.35, 400.0, 1000.0)));
        unbind(g.ctx.as_raw());
    }
}

#[test]
fn the_far_plane_sky_is_never_shadowed_nor_an_occluder() {
    let Some(g) = gpu() else { return };
    // The engine writes 1.0 (the far plane) for the sky instead of an invalid depth: the same scene with the sky at 1.0
    // must give the ground exactly the visibility it gets with an invalid sky, and the sky itself stays lit.
    let s = floating_box();
    let mut far = s.depths();
    for v in far.iter_mut() {
        if *v == 0.0 {
            *v = RANGE_M; // 1.0 after normalisation
        }
    }
    let mut c = unsafe { Contact::new(g.dev.as_raw(), Some(blobs())) }.expect("Contact::new");
    for dir in directions() {
        let p = params(dir, 1.0);
        let a = run_with(&g, &mut c, &depth_tex(&g, &s.depths()), &p);
        let b = run_with(&g, &mut c, &depth_tex(&g, &far), &p);
        assert_eq!(a.out, b.out, "the far-plane sky changed the contact shadows ({dir:?})");
    }
}
