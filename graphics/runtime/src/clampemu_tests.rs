//! Tests of the v18 UNORM clamp emulation: the DXBC checksum against the compiler's, the patch on the host and the twin's
//! output on WARP against the original shader on a float target and on the engine's own 8-bit target.

use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use super::*;
use crate::shaders::{compile, compile_ps};

const ADDITIVE: &str = "float4 main(float4 p : SV_Position) : SV_Target {
  return float4(-0.5, 2.0, asfloat(0x7fc00000u), 1.5); }";

/// A branch with an early return, a sample-free computed output and a second target.
const BRANCHY: &str = "cbuffer C : register(b0) { float4 k; };
struct O { float4 a : SV_Target0; float4 b : SV_Target1; };
O main(float4 p : SV_Position) { O o; o.b = float4(-1, 3, -2, -0.5) * k.y;
  [branch] if (p.x < 4.0) { o.a = float4(-k.x, k.x * 4, 0, 2); return o; }
  o.a = float4(k.x * 0.5, -k.x, 1, 0.5); return o; }";

const VS: &str = "float4 main(uint id : SV_VertexID) : SV_Position { float2 p = float2((id << 1) & 2, id & 2);
  return float4(p * float2(2, -2) + float2(-1, 1), 0, 1); }";

#[test]
fn the_checksum_is_the_one_the_compiler_writes() {
    let Ok(_) = compile_ps(ADDITIVE, "main") else {
        println!("skipped: no compiler");
        return;
    };
    // Different sizes, so both tails of the hash (last block below / at or above 56 bytes) are covered.
    let mut tails = std::collections::BTreeSet::new();
    for (i, src) in [ADDITIVE, BRANCHY, "float4 main() : SV_Target { return 1; }", "float4 main(float4 p : SV_Position, float2 uv : TEXCOORD0) : SV_Target { return float4(uv, p.xy); }"]
        .iter()
        .enumerate()
    {
        let code = compile_ps(src, "main").unwrap();
        let want: Vec<u32> = (0..4).map(|k| u32::from_le_bytes(code[4 + 4 * k..8 + 4 * k].try_into().unwrap())).collect();
        assert_eq!(checksum(&code).to_vec(), want, "shader {i}");
        tails.insert((code.len() - 20) % 64 >= 56);
    }
    // Every blob of the game exe (when GW2_EXE is set): about one in eight ends in the other tail.
    if let Ok(path) = std::env::var("GW2_EXE") {
        let data = std::fs::read(&path).expect("read GW2_EXE");
        for b in crate::shaders::tests::dxbc_blobs(&data) {
            let want: Vec<u32> = (0..4).map(|k| u32::from_le_bytes(b[4 + 4 * k..8 + 4 * k].try_into().unwrap())).collect();
            assert_eq!(checksum(b).to_vec(), want, "blob {:08x} ({} bytes)", crate::shaders::fnv1a32(b), b.len());
            tails.insert((b.len() - 20) % 64 >= 56);
        }
        assert_eq!(tails.len(), 2, "both tails covered");
    }
    println!("tails covered: {tails:?}");
}

#[test]
fn the_patch_rewrites_the_colour_outputs_and_keeps_a_valid_container() {
    let Ok(code) = compile_ps(BRANCHY, "main") else {
        println!("skipped: no compiler");
        return;
    };
    let twin = patch(&code).expect("patch");
    assert_eq!(&twin[..4], b"DXBC");
    assert_eq!(u32::from_le_bytes(twin[24..28].try_into().unwrap()) as usize, twin.len());
    let sealed: Vec<u32> = (0..4).map(|k| u32::from_le_bytes(twin[4 + 4 * k..8 + 4 * k].try_into().unwrap())).collect();
    assert_eq!(checksum(&twin).to_vec(), sealed);
    let dis = crate::shaders::disassemble(&twin).expect("the twin disassembles");
    println!("{dis}");
    // Two returns, two targets: four clamps of each kind.
    assert_eq!(dis.matches("mov_sat o").count(), 4, "{dis}");
    assert_eq!(dis.matches("min o").count(), 4, "{dis}");
    // The original wrote the outputs directly; the twin never does outside the clamps.
    let orig = crate::shaders::disassemble(&code).unwrap();
    assert!(orig.lines().any(|l| l.trim_start().starts_with("mov o")), "{orig}");
    assert!(!dis.lines().any(|l| l.trim_start().starts_with("mov o")), "{dis}");
    // The signatures are untouched.
    assert_eq!(crate::furmask::parse_output_signature(&twin), crate::furmask::parse_output_signature(&code));
    assert_eq!(crate::furmask::parse_input_signature(&twin), crate::furmask::parse_input_signature(&code));
}

#[test]
fn shaders_without_float_colour_outputs_keep_the_original() {
    let Ok(depth) = compile_ps("float main(float4 p : SV_Position) : SV_Depth { return 0.5; }", "main") else {
        println!("skipped: no compiler");
        return;
    };
    assert_eq!(patch(&depth), Err(Skip::NoColourOutput));
    let uint = compile_ps("uint4 main() : SV_Target { return 3; }", "main").unwrap();
    assert_eq!(patch(&uint), Err(Skip::NoColourOutput));
    assert_eq!(patch(b"garbage garbage garbage garbage garbage"), Err(Skip::NotDxbc));
    let vs = compile(VS, "main", "vs_5_0").unwrap();
    assert!(patch(&vs).is_err());
}

#[test]
fn the_twin_is_bound_only_when_every_bound_target_was_upgraded() {
    assert!(wants(true, 0b1, 0b1));
    assert!(wants(true, 0b11, 0b11));
    assert!(!wants(false, 0b1, 0b1), "hdr_clamp=0");
    assert!(!wants(true, 0, 0b1), "a native target");
    assert!(!wants(true, 0b1, 0b11), "an upgraded target next to a native one");
    assert!(!wants(true, 0, 0), "no colour target");
}

#[test]
fn twins_are_paired_both_ways_and_a_reused_pointer_drops_the_old_pair() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(remember(0x1000, 0x2000), None);
    assert_eq!(original_of(0x2000), 0x1000);
    assert_eq!(original_of(0x1000), 0x1000);
    assert_eq!(remember(0x1000, 0x3000), Some(0x2000), "the old twin is handed back for release");
    assert_eq!(original_of(0x2000), 0x2000);
    assert_eq!(forget(0x1000), Some(0x3000));
    assert_eq!(original_of(0x3000), 0x3000);
    assert_eq!(forget(0x1000), None);
}

static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

// ---- WARP ----------------------------------------------------------------------------------------------

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
        _ => {
            println!("skipped: WARP device creation failed");
            None
        }
    }
}

fn f16_to_f32(h: u16) -> f32 {
    let (s, e, m) = ((h >> 15) as u32, ((h >> 10) & 0x1f) as i32, (h & 0x3ff) as f32);
    let v = match e {
        0 => m * 2f32.powi(-24),
        31 => {
            if m == 0.0 {
                f32::INFINITY
            } else {
                f32::NAN
            }
        }
        _ => (1.0 + m / 1024.0) * 2f32.powi(e - 15),
    };
    if s == 1 { -v } else { v }
}

const W: u32 = 8;

/// Draws a full-screen triangle with `ps` into targets of `fmt` cleared to `clear`, with `blend` on every target; returns
/// pixel (x, 0) of each target as RGBA floats.
fn render(g: &Gpu, ps_code: &[u8], fmt: DXGI_FORMAT, targets: usize, clear: [f32; 4], blend: (D3D11_BLEND, D3D11_BLEND), k: [f32; 4], x: usize) -> Vec<[f32; 4]> {
    unsafe {
        let mut texs = Vec::new();
        let mut rtvs = Vec::new();
        for _ in 0..targets {
            let d = D3D11_TEXTURE2D_DESC { Width: W, Height: 1, MipLevels: 1, ArraySize: 1, Format: fmt, SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, Usage: D3D11_USAGE_DEFAULT, BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32, CPUAccessFlags: 0, MiscFlags: 0 };
            let mut t = None;
            g.dev.CreateTexture2D(&d, None, Some(&mut t)).unwrap();
            let t = t.unwrap();
            let mut v = None;
            g.dev.CreateRenderTargetView(&t, None, Some(&mut v)).unwrap();
            let v = v.unwrap();
            g.ctx.ClearRenderTargetView(&v, &clear);
            texs.push(t);
            rtvs.push(Some(v));
        }
        let vs_code = compile(VS, "main", "vs_5_0").unwrap();
        let mut vs = None;
        g.dev.CreateVertexShader(&vs_code, None, Some(&mut vs)).unwrap();
        let mut ps = None;
        g.dev.CreatePixelShader(ps_code, None, Some(&mut ps)).expect("CreatePixelShader");
        let mut bd = D3D11_BLEND_DESC { AlphaToCoverageEnable: false.into(), IndependentBlendEnable: false.into(), RenderTarget: Default::default() };
        bd.RenderTarget[0] = D3D11_RENDER_TARGET_BLEND_DESC { BlendEnable: true.into(), SrcBlend: blend.0, DestBlend: blend.1, BlendOp: D3D11_BLEND_OP_ADD, SrcBlendAlpha: D3D11_BLEND_ONE, DestBlendAlpha: D3D11_BLEND_ONE, BlendOpAlpha: D3D11_BLEND_OP_ADD, RenderTargetWriteMask: 0xF };
        let mut bs = None;
        g.dev.CreateBlendState(&bd, Some(&mut bs)).unwrap();
        let cbd = D3D11_BUFFER_DESC { ByteWidth: 16, Usage: D3D11_USAGE_DEFAULT, BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32, CPUAccessFlags: 0, MiscFlags: 0, StructureByteStride: 0 };
        let init = D3D11_SUBRESOURCE_DATA { pSysMem: k.as_ptr() as *const _, SysMemPitch: 0, SysMemSlicePitch: 0 };
        let mut cb = None;
        g.dev.CreateBuffer(&cbd, Some(&init), Some(&mut cb)).unwrap();
        g.ctx.OMSetRenderTargets(Some(&rtvs), None);
        g.ctx.OMSetBlendState(bs.as_ref(), None, 0xffff_ffff);
        g.ctx.RSSetViewports(Some(&[D3D11_VIEWPORT { TopLeftX: 0.0, TopLeftY: 0.0, Width: W as f32, Height: 1.0, MinDepth: 0.0, MaxDepth: 1.0 }]));
        g.ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
        g.ctx.IASetInputLayout(None);
        g.ctx.VSSetShader(vs.as_ref(), None);
        g.ctx.PSSetShader(ps.as_ref(), None);
        g.ctx.PSSetConstantBuffers(0, Some(&[cb]));
        g.ctx.Draw(3, 0);
        g.ctx.OMSetRenderTargets(None, None);
        let mut out = Vec::new();
        for t in &texs {
            let d = D3D11_TEXTURE2D_DESC { Width: W, Height: 1, MipLevels: 1, ArraySize: 1, Format: fmt, SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, Usage: D3D11_USAGE_STAGING, BindFlags: 0, CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32, MiscFlags: 0 };
            let mut st = None;
            g.dev.CreateTexture2D(&d, None, Some(&mut st)).unwrap();
            let st = st.unwrap();
            g.ctx.CopyResource(&st, t);
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            g.ctx.Map(&st, 0, D3D11_MAP_READ, 0, Some(&mut m)).unwrap();
            let px = if fmt == DXGI_FORMAT_R16G16B16A16_FLOAT {
                let p = (m.pData as *const u16).add(x * 4);
                [f16_to_f32(*p), f16_to_f32(*p.add(1)), f16_to_f32(*p.add(2)), f16_to_f32(*p.add(3))]
            } else {
                let p = (m.pData as *const u8).add(x * 4);
                [*p as f32 / 255.0, *p.add(1) as f32 / 255.0, *p.add(2) as f32 / 255.0, *p.add(3) as f32 / 255.0]
            };
            g.ctx.Unmap(&st, 0);
            out.push(px);
        }
        out
    }
}

fn near(a: f32, b: f32) -> bool {
    (a - b).abs() <= 1.5 / 255.0
}

#[test]
fn v18_an_additive_volume_with_negative_nan_and_big_alpha_output_behaves_like_the_engines_8bit_target() {
    let Some(g) = gpu() else { return };
    let Ok(code) = compile_ps(ADDITIVE, "main") else { return };
    let twin = patch(&code).expect("patch");
    // The runtime checks the container hash: a twin with a wrong one would be refused (so `seal` matters).
    let mut bad = twin.clone();
    bad[5] ^= 0x40;
    let mut refused = None;
    assert!(unsafe { g.dev.CreatePixelShader(&bad, None, Some(&mut refused)) }.is_err(), "a wrong checksum is refused");
    let add = (D3D11_BLEND_ONE, D3D11_BLEND_ONE);
    let clear = [0.25; 4];
    let float = DXGI_FORMAT_R16G16B16A16_FLOAT;
    let unorm = DXGI_FORMAT_R8G8B8A8_UNORM;
    let orig_f = render(&g, &code, float, 1, clear, add, [0.0; 4], 0)[0];
    let twin_f = render(&g, &twin, float, 1, clear, add, [0.0; 4], 0)[0];
    let orig_u = render(&g, &code, unorm, 1, clear, add, [0.0; 4], 0)[0];
    println!("float original {orig_f:?}, float twin {twin_f:?}, 8-bit original {orig_u:?}");
    // The bug: on the upgraded target the volume SUBTRACTS (0.25 - 0.5) and leaves NaN behind.
    assert!(orig_f[0] < 0.0, "{orig_f:?}");
    assert!(orig_f[2].is_nan(), "{orig_f:?}");
    // The engine's own target: the output is clamped before the blend.
    assert!(near(orig_u[0], 0.25) && near(orig_u[1], 1.0) && near(orig_u[2], 0.25) && near(orig_u[3], 1.0), "{orig_u:?}");
    // The twin on the float target: the 8-bit target's floor, NaN removal and alpha, but the HDR value above 1 is kept.
    assert!(near(twin_f[0], orig_u[0]), "red {twin_f:?}");
    assert!(near(twin_f[2], orig_u[2]), "blue (NaN) {twin_f:?}");
    assert!(near(twin_f[3], 1.25), "alpha saturated before the blend {twin_f:?}");
    assert!(near(twin_f[1], 2.25), "values above 1 kept {twin_f:?}");
}

#[test]
fn v18_branches_early_returns_and_two_targets_are_all_clamped_and_unchanged_values_stay_exact() {
    let Some(g) = gpu() else { return };
    let Ok(code) = compile_ps(BRANCHY, "main") else { return };
    let twin = patch(&code).expect("patch");
    let opaque = (D3D11_BLEND_ONE, D3D11_BLEND_ZERO);
    let float = DXGI_FORMAT_R16G16B16A16_FLOAT;
    let k = [0.5, 0.25, 0.0, 0.0];
    for x in [1usize, 6] {
        let o = render(&g, &code, float, 2, [0.0; 4], opaque, k, x);
        let t = render(&g, &twin, float, 2, [0.0; 4], opaque, k, x);
        println!("x={x} original {o:?} twin {t:?}");
        for (a, b) in o.iter().zip(&t) {
            for c in 0..3 {
                assert_eq!(b[c], a[c].max(0.0), "x={x} colour {c}: {o:?} -> {t:?}");
            }
            assert_eq!(b[3], a[3].clamp(0.0, 1.0), "x={x} alpha: {o:?} -> {t:?}");
        }
    }
    // The early-return branch (x < 4) really produced out-of-range values that were clamped.
    let o = render(&g, &code, float, 2, [0.0; 4], opaque, k, 1);
    assert!(o[0][0] < 0.0 && o[0][1] > 1.0 && o[0][3] > 1.0, "{o:?}");
}

#[test]
fn v18_an_inv_src_alpha_blend_with_alpha_above_1_does_not_subtract_the_scene() {
    let Some(g) = gpu() else { return };
    let src = "float4 main() : SV_Target { return float4(0.1, 0.1, 0.1, 1.6); }";
    let Ok(code) = compile_ps(src, "main") else { return };
    let twin = patch(&code).unwrap();
    let over = (D3D11_BLEND_SRC_ALPHA, D3D11_BLEND_INV_SRC_ALPHA);
    let o = render(&g, &code, DXGI_FORMAT_R16G16B16A16_FLOAT, 1, [0.5; 4], over, [0.0; 4], 0)[0];
    let t = render(&g, &twin, DXGI_FORMAT_R16G16B16A16_FLOAT, 1, [0.5; 4], over, [0.0; 4], 0)[0];
    let u = render(&g, &code, DXGI_FORMAT_R8G8B8A8_UNORM, 1, [0.5; 4], over, [0.0; 4], 0)[0];
    println!("original {o:?} twin {t:?} 8-bit {u:?}");
    // 0.1 * 1.6 + 0.5 * (1 - 1.6) = -0.14 on float; 0.1 * 1 + 0.5 * 0 = 0.1 on 8-bit.
    assert!(o[0] < 0.0, "{o:?}");
    assert!(near(t[0], u[0]) && near(t[0], 0.1), "{t:?} vs {u:?}");
}

/// Every pixel shader in the game exe gets a twin the runtime accepts (or a stated reason to keep the original).
#[test]
fn v18_every_pixel_shader_of_the_game_exe_patches_into_a_shader_the_runtime_accepts() {
    let Ok(path) = std::env::var("GW2_EXE") else {
        println!("skipped: GW2_EXE is not set");
        return;
    };
    let Some(g) = gpu() else { return };
    let data = std::fs::read(&path).expect("read GW2_EXE");
    let (mut ok, mut kept, mut ps_n) = (0, std::collections::BTreeMap::new(), 0);
    for b in crate::shaders::tests::dxbc_blobs(&data) {
        let mut ps = None;
        if unsafe { g.dev.CreatePixelShader(b, None, Some(&mut ps)) }.is_err() {
            continue; // not a pixel shader
        }
        ps_n += 1;
        match patch(b) {
            Ok(twin) => {
                let mut t = None;
                unsafe { g.dev.CreatePixelShader(&twin, None, Some(&mut t)) }.unwrap_or_else(|e| panic!("twin of {:08x} refused: {e:?}", crate::shaders::fnv1a32(b)));
                ok += 1;
            }
            Err(why) => *kept.entry(format!("{why:?}")).or_insert(0) += 1,
        }
    }
    println!("pixel shaders in the exe: {ps_n}; twins created: {ok}; kept original: {kept:?}");
    assert!(ps_n > 0);
    assert!(ok * 10 >= ps_n * 9, "at least 90 % get a twin: {ok} of {ps_n} ({kept:?})");
}

/// The upgraded mark (private data on the texture) is read back through a render-target view, only on marked textures,
/// without leaking a reference; and the decision binds the twin only for the marked binding.
#[test]
fn v18_the_upgraded_mark_is_found_through_the_view_and_drives_the_decision() {
    use windows::core::Interface;
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let Some(g) = gpu() else { return };
    unsafe {
        let d = D3D11_TEXTURE2D_DESC { Width: 4, Height: 4, MipLevels: 1, ArraySize: 1, Format: DXGI_FORMAT_R16G16B16A16_FLOAT, SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, Usage: D3D11_USAGE_DEFAULT, BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32, CPUAccessFlags: 0, MiscFlags: 0 };
        let (mut a, mut b) = (None, None);
        g.dev.CreateTexture2D(&d, None, Some(&mut a)).unwrap();
        g.dev.CreateTexture2D(&d, None, Some(&mut b)).unwrap();
        let (a, b) = (a.unwrap(), b.unwrap());
        crate::d3d::mark_upgraded(a.as_raw());
        let (mut va, mut vb) = (None, None);
        g.dev.CreateRenderTargetView(&a, None, Some(&mut va)).unwrap();
        g.dev.CreateRenderTargetView(&b, None, Some(&mut vb)).unwrap();
        let (va, vb) = (va.unwrap(), vb.unwrap());
        let refs = |o: &ID3D11Texture2D| {
            let raw = o.as_raw();
            let add: unsafe extern "system" fn(*mut core::ffi::c_void) -> u32 = std::mem::transmute(*(*(raw as *const *const usize)).add(1));
            let n = add(raw);
            crate::d3d::release(raw);
            n - 1
        };
        let before = refs(&a);
        assert!(crate::d3d::view_is_upgraded(va.as_raw()), "marked texture");
        assert!(!crate::d3d::view_is_upgraded(vb.as_raw()), "native float texture");
        let after = refs(&a);
        assert_eq!(before, after, "no reference leaked");
    }
    // The decision on top of it: a game shader with a twin.
    let _ = remember(0x5000, 0x6000);
    set_enabled(true);
    note_targets(0b1, 0b1);
    assert_eq!(decide(0x5000), Some(0x6000), "upgraded binding: the twin");
    assert_eq!(decide(0x6000), None, "already bound");
    assert!(trace_state().1);
    note_targets(0, 0b1);
    assert_eq!(decide(0x6000), Some(0x5000), "native binding: back to the original");
    note_targets(0b1, 0b1);
    set_enabled(false);
    assert_eq!(decide(0x6000), Some(0x5000), "hdr_clamp=0: the original");
    set_enabled(true);
    assert_eq!(decide(0x7777), None, "no twin (a substitute or an unparsed shader): left alone");
    let _ = forget(0x5000);
    note_targets(0, 0);
}
