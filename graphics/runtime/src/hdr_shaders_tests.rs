//! Tests of the HDR replacement shaders. `hlsl_files_compile` always runs; the equivalence tests run
//! only when `GW2_EXE` names the game exe (they need the original DXBC blobs) and WARP is available.
//!
//! Harness: D3D11 WARP device, 64x64 `R16G16B16A16_FLOAT` target, one full-screen triangle through a
//! trivial vertex shader, the original blob and our replacement run on identical inputs.

use std::ffi::c_void;
use std::sync::OnceLock;

use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::Fxc::{D3DCompile, D3DDisassemble};
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::{Interface, PCSTR};

use super::*;
use crate::resolve;
use crate::shaders::{compile, compile_ps, fnv1a32};
use crate::tonemap::{self, ToneParams};

const N: usize = 64;
const TOL: f32 = 2.0 / 255.0;

// ---- constants documented ---------------------------------------------------------------------
// f5cd4832 cb0: cb0[0].x = 0.9 (dark-curve scale), cb0[1].x = 0.7 (dark-curve exponent).
// f5cd4832 t1 (1x1 float16): (x=0.5 adapted luminance, unused by this shader; y=0.3 dark, z=0.2 bright, w=0.5 mid).
// 2ed4a75f / 14b1e90e cb0[0].xyz = (0.10, 0.05, 0.08) threshold.
const LIGHT_CB: [[f32; 4]; 2] = [[0.9, 0.0, 0.0, 0.0], [0.7, 0.0, 0.0, 0.0]];
const LIGHT_T1: [f32; 4] = [0.5, 0.3, 0.2, 0.5];
const BRIGHT_CB: [[f32; 4]; 1] = [[0.10, 0.05, 0.08, 0.0]];

// ---- compile / signature test (always runs) ----------------------------------------------------

fn blob_bytes(b: &ID3DBlob) -> Vec<u8> {
    unsafe { std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize()).to_vec() }
}

fn disasm(dxbc: &[u8]) -> String {
    let out = unsafe { D3DDisassemble(dxbc.as_ptr() as *const c_void, dxbc.len(), 0, PCSTR::null()).expect("D3DDisassemble") };
    String::from_utf8_lossy(&blob_bytes(&out)).to_string()
}

fn norm(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Input-signature rows and `dcl_` declarations (resources, samplers, cbuffer, inputs, outputs) of a
/// disassembly, whitespace-normalised. Temps/flags are compiler-dependent and left out.
fn interface_of(dis: &str) -> (Vec<String>, Vec<String>) {
    let mut sig = Vec::new();
    let mut in_sig = false;
    let mut dcls = Vec::new();
    for l in dis.lines() {
        if l.starts_with("// Input signature:") {
            in_sig = true;
            continue;
        }
        if l.starts_with("// Output signature:") {
            in_sig = false;
        }
        if in_sig {
            let t = norm(l.trim_start_matches('/'));
            if !t.is_empty() && !t.starts_with("Name ") && !t.starts_with("---") {
                sig.push(t);
            }
        }
        let t = l.trim();
        if t.starts_with("dcl_") && !t.starts_with("dcl_globalFlags") && !t.starts_with("dcl_temps") {
            dcls.push(norm(t));
        }
    }
    (sig, dcls)
}

fn strs(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

/// (input signature rows, dcl lines) of the originals, copied from the dxbc_dump disassembly.
fn expected_interface(hash: u32) -> (Vec<String>, Vec<String>) {
    match hash {
        LIGHT_ADJUST_HASH => (
            strs(&["SV_POSITION 0 xyzw 0 POS float xy", "TEXCOORD 0 xyzw 1 NONE float xy"]),
            strs(&[
                "dcl_constantbuffer CB0[2], immediateIndexed",
                "dcl_sampler s0, mode_default",
                "dcl_sampler s1, mode_default",
                "dcl_resource_texture2d (float,float,float,float) t0",
                "dcl_resource_texture2d (float,float,float,float) t1",
                "dcl_input_ps_siv linear noperspective v0.xy, position",
                "dcl_input_ps linear v1.xy",
                "dcl_output o0.xyzw",
            ]),
        ),
        _ => (
            strs(&[
                "TEXCOORD 0 xy 0 NONE float xy",
                "TEXCOORD 1 zw 0 NONE float zw",
                "TEXCOORD 2 xy 1 NONE float xy",
                "TEXCOORD 3 zw 1 NONE float zw",
            ]),
            strs(&[
                "dcl_constantbuffer CB0[1], immediateIndexed",
                "dcl_sampler s0, mode_default",
                "dcl_resource_texture2d (float,float,float,float) t0",
                "dcl_input_ps linear v0.xy",
                "dcl_input_ps linear v0.zw",
                "dcl_input_ps linear v1.xy",
                "dcl_input_ps linear v1.zw",
                "dcl_output o0.xyzw",
            ]),
        ),
    }
}

const ENTRIES: [(u32, &str, &str); 3] = [
    (LIGHT_ADJUST_HASH, LIGHT_ADJUST_HLSL, LIGHT_ADJUST_ENTRY),
    (BRIGHT_PASS_HASH, BRIGHT_PASS_HLSL, BRIGHT_PASS_ENTRY),
    (BLOOM_THRESHOLD_HASH, BRIGHT_PASS_HLSL, BLOOM_THRESHOLD_ENTRY),
];

#[test]
fn hlsl_files_compile() {
    for (hash, hlsl, entry) in ENTRIES {
        let dxbc = compile_ps(hlsl, entry).unwrap_or_else(|e| panic!("{entry}: {e}"));
        assert_eq!(&dxbc[..4], b"DXBC");
        let dis = disasm(&dxbc);
        let mut got = interface_of(&dis);
        if hash == LIGHT_ADJUST_HASH {
            // The data path adds exactly three inputs that the game never binds: b6 (parameters, two float4),
            // t3 / t4 (fog state and linear depth for the AO release), t6 (AO visibility), t7 (the light halo), b7 and t8 (reflections), b8 and t9 (sun shafts), t20 (the fur mask), b10 and t22 (contact shadows and aerial perspective), b11 and t23 (the rim mask and its constants), b12 (the live fur look), b13 and t24 (the shell fur; b13 is 2 float4 again since v7: the exclusion picker and its preview are removed), t25 (v11: the skin gate of the old fur look and the rim). Everything else must be the original's interface.
            for extra in ["dcl_constantbuffer CB6[2], immediateIndexed", "dcl_resource_texture2d (float,float,float,float) t3", "dcl_resource_texture2d (float,float,float,float) t4", "dcl_resource_texture2d (float,float,float,float) t6", "dcl_resource_texture2d (float,float,float,float) t7", "dcl_constantbuffer CB7[1], immediateIndexed", "dcl_resource_texture2d (float,float,float,float) t8", "dcl_constantbuffer CB8[1], immediateIndexed", "dcl_resource_texture2d (float,float,float,float) t9", "dcl_resource_texture2d (float,float,float,float) t20", "dcl_constantbuffer CB9[1], immediateIndexed", "dcl_resource_texture2d (float,float,float,float) t5", "dcl_resource_texture2d (float,float,float,float) t21", "dcl_constantbuffer CB10[1], immediateIndexed", "dcl_resource_texture2d (float,float,float,float) t22", "dcl_constantbuffer CB11[2], immediateIndexed", "dcl_resource_texture2d (float,float,float,float) t23", "dcl_constantbuffer CB12[2], immediateIndexed", "dcl_constantbuffer CB13[2], immediateIndexed", "dcl_resource_texture2d (float,float,float,float) t24", "dcl_resource_texture2d (float,float,float,float) t25"] {
                let n = got.1.len();
                got.1.retain(|l| l != extra);
                assert_eq!(got.1.len() + 1, n, "{entry}: the substitute must declare `{extra}`\n{dis}");
            }
        }
        if hash == BLOOM_THRESHOLD_HASH {
            // The bloom substitute reads one input the game never binds: b6 (the bloom tint compensation).
            got.1.retain(|l| l != "dcl_constantbuffer CB6[1], immediateIndexed");
        }
        let want = expected_interface(hash);
        assert_eq!(got, want, "{entry}: interface differs from the original\n{dis}");
        // When the exe is available also compare against the real blob.
        if let Some(blobs) = original_blobs() {
            let orig = &blobs.iter().find(|(h, _)| *h == hash).unwrap().1;
            assert_eq!(interface_of(&disasm(orig)), got, "{entry}: differs from the blob in the exe");
        }
    }
}

#[test]
fn install_all_registers_three_substitutes_without_enabling() {
    install_all();
    assert!(!crate::shaders::substitution_enabled());
    for (hash, _, _) in ENTRIES {
        assert!(crate::shaders::substitutes().get_or_create(1, hash, |_, _| Ok(0x10)).is_some(), "{hash:08x} not installed");
    }
}

// ---- original blobs ----------------------------------------------------------------------------

fn original_blobs() -> Option<&'static Vec<(u32, Vec<u8>)>> {
    static B: OnceLock<Option<Vec<(u32, Vec<u8>)>>> = OnceLock::new();
    B.get_or_init(|| {
        let path = std::env::var("GW2_EXE").ok()?;
        let data = std::fs::read(&path).expect("read GW2_EXE");
        let mut out = Vec::new();
        for b in crate::shaders::tests::dxbc_blobs(&data) {
            let h = fnv1a32(b);
            if ENTRIES.iter().any(|(eh, _, _)| *eh == h) && !out.iter().any(|(oh, _)| *oh == h) {
                out.push((h, b.to_vec()));
            }
        }
        assert_eq!(out.len(), 3, "original blobs not all found in {path}");
        Some(out)
    })
    .as_ref()
}

// ---- half floats -------------------------------------------------------------------------------

fn f32_to_f16(v: f32) -> u16 {
    let b = v.to_bits();
    let s = ((b >> 16) & 0x8000) as u16;
    let e = ((b >> 23) & 0xff) as i32;
    let m = b & 0x7f_ffff;
    if e == 0xff {
        return s | 0x7c00 | if m != 0 { 0x200 } else { 0 };
    }
    let ne = e - 127 + 15;
    if ne >= 31 {
        return s | 0x7c00;
    }
    if ne <= 0 {
        if ne < -10 {
            return s;
        }
        let m2 = m | 0x80_0000;
        let sh = (14 - ne) as u32;
        let mut h = (m2 >> sh) as u16;
        if (m2 >> (sh - 1)) & 1 == 1 {
            h += 1;
        }
        return s | h;
    }
    let mut h = s | ((ne as u16) << 10) | ((m >> 13) as u16);
    if m & 0x1000 != 0 {
        h += 1;
    }
    h
}

fn f16_to_f32(h: u16) -> f32 {
    let s = ((h as u32) & 0x8000) << 16;
    let e = ((h >> 10) & 0x1f) as u32;
    let m = (h & 0x3ff) as u32;
    let bits = if e == 0 {
        if m == 0 {
            s
        } else {
            let (mut m, mut e) = (m, 113u32);
            while m & 0x400 == 0 {
                m <<= 1;
                e -= 1;
            }
            s | (e << 23) | ((m & 0x3ff) << 13)
        }
    } else if e == 31 {
        s | 0x7f80_0000 | (m << 13)
    } else {
        s | ((e + 112) << 23) | (m << 13)
    };
    f32::from_bits(bits)
}

#[test]
fn half_conversion_roundtrips() {
    for v in [0.0f32, 1.0, 0.5, 3.0, 1000.0, 0.1, -1.0, 65504.0] {
        let r = f16_to_f32(f32_to_f16(v));
        assert!((r - v).abs() <= v.abs() * 1e-3, "{v} -> {r}");
    }
    assert!(f16_to_f32(f32_to_f16(f32::NAN)).is_nan());
}

// ---- D3D11 harness -----------------------------------------------------------------------------

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

/// `Some` when GW2_EXE is set and WARP works; otherwise prints why and returns `None`.
fn setup() -> Option<Gpu> {
    if original_blobs().is_none() {
        println!("skipped: GW2_EXE is not set (the original DXBC blobs are read from the game exe)");
        return None;
    }
    gpu()
}

fn compile_vs(src: &str) -> Vec<u8> {
    let mut code: Option<ID3DBlob> = None;
    let mut errs: Option<ID3DBlob> = None;
    let r = unsafe { D3DCompile(src.as_ptr() as *const c_void, src.len(), PCSTR::null(), None, None, PCSTR(c"main".as_ptr() as *const u8), PCSTR(c"vs_5_0".as_ptr() as *const u8), 0, 0, &mut code, Some(&mut errs)) };
    if r.is_err() {
        panic!("vs compile: {}", errs.map(|e| String::from_utf8_lossy(&blob_bytes(&e)).to_string()).unwrap_or_default());
    }
    blob_bytes(&code.unwrap())
}

/// Full-screen triangle; TEXCOORD0 = uv in xy (the light-adjust PS reads a float4).
const VS_LIGHT: &str = "struct O { float4 pos : SV_POSITION; float4 uv : TEXCOORD0; };
O main(uint id : SV_VertexID) { float2 p = float2((id << 1) & 2, id & 2); O o;
  o.pos = float4(p * float2(2, -2) + float2(-1, 1), 0, 1); o.uv = float4(p, 0, 0); return o; }";

/// Same triangle; four distinct tap positions around the pixel (offsets in uv units).
const VS_TAPS: &str = "struct O { float4 pos : SV_POSITION; float2 a : TEXCOORD0; float2 b : TEXCOORD1; float2 c : TEXCOORD2; float2 d : TEXCOORD3; };
O main(uint id : SV_VertexID) { float2 p = float2((id << 1) & 2, id & 2); O o; float e = 0.75 / 64.0;
  o.pos = float4(p * float2(2, -2) + float2(-1, 1), 0, 1);
  o.a = p + float2(-e, -e); o.b = p + float2(e, -e); o.c = p + float2(-e, e); o.d = p + float2(e, e); return o; }";

thread_local! {
    /// b6[1].z (the fur test strength) for the renders of this test thread; 0 = off (every other test).
    static FUR_TEST: std::cell::Cell<f32> = const { std::cell::Cell::new(0.0) };
    /// The fur mask (t20, R16G16B16A16_UNORM, N x N: a = value 0..=255 / 255) for the renders of this test thread; None = t20 unbound.
    static FUR_MASK: std::cell::RefCell<Option<Vec<u8>>> = const { std::cell::RefCell::new(None) };
    /// The mesh uv in the fur mask (rg = frac(uv), b = 1), N x N; None = no uv (b = 0, the fallback of a draw without uv).
    static FUR_UV: std::cell::RefCell<Option<Vec<[f32; 2]>>> = const { std::cell::RefCell::new(None) };
    /// b6[1].w (the fur debug switch) for the renders of this test thread.
    static FUR_DEBUG: std::cell::Cell<f32> = const { std::cell::Cell::new(0.0) };
}

struct Inputs<'a> {
    t0: &'a [[f32; 4]], // N*N texels, R32G32B32A32_FLOAT
    t1: Option<[f32; 4]>, // 1x1 R16G16B16A16_FLOAT
    cb: &'a [[f32; 4]],
}

fn tex2d(g: &Gpu, w: u32, h: u32, fmt: DXGI_FORMAT, bind: D3D11_BIND_FLAG, usage: D3D11_USAGE, cpu: u32, init: Option<(&[u8], u32)>) -> ID3D11Texture2D {
    let desc = D3D11_TEXTURE2D_DESC { Width: w, Height: h, MipLevels: 1, ArraySize: 1, Format: fmt, SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, Usage: usage, BindFlags: bind.0 as u32, CPUAccessFlags: cpu, MiscFlags: 0 };
    let sd = init.map(|(d, pitch)| D3D11_SUBRESOURCE_DATA { pSysMem: d.as_ptr() as *const c_void, SysMemPitch: pitch, SysMemSlicePitch: 0 });
    let mut t = None;
    unsafe { g.dev.CreateTexture2D(&desc, sd.as_ref().map(|s| s as *const _), Some(&mut t)).expect("CreateTexture2D") };
    t.unwrap()
}

/// Renders `ps` over the 64x64 target with the given inputs; returns RGBA per pixel.
fn render(g: &Gpu, vs_src: &str, ps_dxbc: &[u8], inp: &Inputs) -> Vec<[f32; 4]> {
    render_extra(g, vs_src, ps_dxbc, inp, None)
}

/// `extra` = (value of a uniform 1x1 R16_FLOAT bound as t6, the float4 bound as b6): the inputs of the light-adjust
/// substitute that the mod binds at draw time (datapath.rs).
fn render_extra(g: &Gpu, vs_src: &str, ps_dxbc: &[u8], inp: &Inputs, extra: Option<(f32, [f32; 4])>) -> Vec<[f32; 4]> {
    render_full(g, vs_src, ps_dxbc, inp, extra, None)
}

/// `glow` = (value of a uniform 1x1 RGBA16F bound as t7 or None to leave t7 unbound, halo strength written to b6[1].x); needs `extra`.
fn render_full(g: &Gpu, vs_src: &str, ps_dxbc: &[u8], inp: &Inputs, extra: Option<(f32, [f32; 4])>, glow: Option<(Option<[f32; 4]>, f32)>) -> Vec<[f32; 4]> {
    render_hook(g, vs_src, ps_dxbc, inp, extra, glow, &|_| {})
}

/// `render_full` with a hook that runs on the context right before the draw (extra bindings, e.g. the fog t3 / t4).
fn render_hook(g: &Gpu, vs_src: &str, ps_dxbc: &[u8], inp: &Inputs, extra: Option<(f32, [f32; 4])>, glow: Option<(Option<[f32; 4]>, f32)>, hook: &dyn Fn(&ID3D11DeviceContext)) -> Vec<[f32; 4]> {
    unsafe {
        let mut vs = None;
        g.dev.CreateVertexShader(&compile_vs(vs_src), None, Some(&mut vs)).expect("CreateVertexShader");
        let mut ps = None;
        g.dev.CreatePixelShader(ps_dxbc, None, Some(&mut ps)).expect("CreatePixelShader");

        let bytes: Vec<u8> = inp.t0.iter().flat_map(|p| p.iter().flat_map(|c| c.to_le_bytes())).collect();
        assert_eq!(inp.t0.len(), N * N);
        let t0 = tex2d(g, N as u32, N as u32, DXGI_FORMAT_R32G32B32A32_FLOAT, D3D11_BIND_SHADER_RESOURCE, D3D11_USAGE_IMMUTABLE, 0, Some((&bytes, (N * 16) as u32)));
        let mut srv0 = None;
        g.dev.CreateShaderResourceView(&t0, None, Some(&mut srv0)).unwrap();
        let mut srvs = vec![srv0];
        if let Some(v) = inp.t1 {
            let b: Vec<u8> = v.iter().flat_map(|c| f32_to_f16(*c).to_le_bytes()).collect();
            let t1 = tex2d(g, 1, 1, DXGI_FORMAT_R16G16B16A16_FLOAT, D3D11_BIND_SHADER_RESOURCE, D3D11_USAGE_IMMUTABLE, 0, Some((&b, 8)));
            let mut srv1 = None;
            g.dev.CreateShaderResourceView(&t1, None, Some(&mut srv1)).unwrap();
            srvs.push(srv1);
        }

        let sdesc = D3D11_SAMPLER_DESC { Filter: D3D11_FILTER_MIN_MAG_MIP_POINT, AddressU: D3D11_TEXTURE_ADDRESS_CLAMP, AddressV: D3D11_TEXTURE_ADDRESS_CLAMP, AddressW: D3D11_TEXTURE_ADDRESS_CLAMP, MaxAnisotropy: 1, ComparisonFunc: D3D11_COMPARISON_NEVER, MaxLOD: f32::MAX, ..Default::default() };
        let mut smp = None;
        g.dev.CreateSamplerState(&sdesc, Some(&mut smp)).unwrap();
        let samplers = vec![smp.clone(), smp];

        let cbytes: Vec<u8> = inp.cb.iter().flat_map(|p| p.iter().flat_map(|c| c.to_le_bytes())).collect();
        let cbd = D3D11_BUFFER_DESC { ByteWidth: cbytes.len() as u32, Usage: D3D11_USAGE_IMMUTABLE, BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32, ..Default::default() };
        let csd = D3D11_SUBRESOURCE_DATA { pSysMem: cbytes.as_ptr() as *const c_void, SysMemPitch: 0, SysMemSlicePitch: 0 };
        let mut cb = None;
        g.dev.CreateBuffer(&cbd, Some(&csd), Some(&mut cb)).unwrap();

        let rt = tex2d(g, N as u32, N as u32, DXGI_FORMAT_R16G16B16A16_FLOAT, D3D11_BIND_RENDER_TARGET, D3D11_USAGE_DEFAULT, 0, None);
        let mut rtv = None;
        g.dev.CreateRenderTargetView(&rt, None, Some(&mut rtv)).unwrap();

        let c = &g.ctx;
        c.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
        c.VSSetShader(vs.as_ref(), None);
        c.PSSetShader(ps.as_ref(), None);
        c.PSSetShaderResources(0, Some(&srvs));
        c.PSSetSamplers(0, Some(&samplers));
        c.PSSetConstantBuffers(0, Some(&[cb]));
        c.OMSetRenderTargets(Some(&[rtv]), None);
        c.RSSetViewports(Some(&[D3D11_VIEWPORT { TopLeftX: 0.0, TopLeftY: 0.0, Width: N as f32, Height: N as f32, MinDepth: 0.0, MaxDepth: 1.0 }]));
        if let Some((vis, mp)) = extra {
            let vb = f32_to_f16(vis).to_le_bytes();
            let vt = tex2d(g, 1, 1, DXGI_FORMAT_R16_FLOAT, D3D11_BIND_SHADER_RESOURCE, D3D11_USAGE_IMMUTABLE, 0, Some((&vb, 2)));
            let mut vsrv = None;
            g.dev.CreateShaderResourceView(&vt, None, Some(&mut vsrv)).unwrap();
            c.PSSetShaderResources(6, Some(&[vsrv]));
            let mut mb: Vec<u8> = mp.iter().flat_map(|x| x.to_le_bytes()).collect();
            mb.extend([glow.map_or(0.0, |g| g.1), 1.0, FUR_TEST.with(|f| f.get()), FUR_DEBUG.with(|f| f.get())].iter().flat_map(|x| x.to_le_bytes()));
            if let Some((Some(v), _)) = glow {
                let gb: Vec<u8> = v.iter().flat_map(|c| f32_to_f16(*c).to_le_bytes()).collect();
                let gt = tex2d(g, 1, 1, DXGI_FORMAT_R16G16B16A16_FLOAT, D3D11_BIND_SHADER_RESOURCE, D3D11_USAGE_IMMUTABLE, 0, Some((&gb, 8)));
                let mut gsrv = None;
                g.dev.CreateShaderResourceView(&gt, None, Some(&mut gsrv)).unwrap();
                c.PSSetShaderResources(7, Some(&[gsrv]));
            }
            if let Some(mask) = FUR_MASK.with(|m| m.borrow().clone()) {
                assert_eq!(mask.len(), N * N);
                let uvs = FUR_UV.with(|u| u.borrow().clone());
                let mut mbytes: Vec<u8> = Vec::with_capacity(N * N * 8);
                for (i, m) in mask.iter().enumerate() {
                    let q = |v: f32| ((v.rem_euclid(1.0) * 65535.0).round() as u32).min(65535) as u16;
                    let (u, v, b) = uvs.as_ref().map_or((0, 0, 0u16), |uv| (q(uv[i][0]), q(uv[i][1]), 65535));
                    for c in [u, v, b, *m as u16 * 257] {
                        mbytes.extend(c.to_le_bytes());
                    }
                }
                let mt = tex2d(g, N as u32, N as u32, DXGI_FORMAT_R16G16B16A16_UNORM, D3D11_BIND_SHADER_RESOURCE, D3D11_USAGE_IMMUTABLE, 0, Some((&mbytes, (N * 8) as u32)));
                let mut msrv = None;
                g.dev.CreateShaderResourceView(&mt, None, Some(&mut msrv)).unwrap();
                c.PSSetShaderResources(20, Some(&[msrv]));
            }
            let cbd6 = D3D11_BUFFER_DESC { ByteWidth: 32, Usage: D3D11_USAGE_IMMUTABLE, BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32, ..Default::default() };
            let csd6 = D3D11_SUBRESOURCE_DATA { pSysMem: mb.as_ptr() as *const c_void, SysMemPitch: 0, SysMemSlicePitch: 0 };
            let mut cb6 = None;
            g.dev.CreateBuffer(&cbd6, Some(&csd6), Some(&mut cb6)).unwrap();
            c.PSSetConstantBuffers(6, Some(&[cb6]));
        }
        hook(c);
        c.Draw(3, 0);

        let st = tex2d(g, N as u32, N as u32, DXGI_FORMAT_R16G16B16A16_FLOAT, D3D11_BIND_FLAG(0), D3D11_USAGE_STAGING, D3D11_CPU_ACCESS_READ.0 as u32, None);
        c.CopyResource(&st, &rt);
        let mut m = D3D11_MAPPED_SUBRESOURCE::default();
        c.Map(&st, 0, D3D11_MAP_READ, 0, Some(&mut m)).unwrap();
        let mut out = Vec::with_capacity(N * N);
        for y in 0..N {
            let row = std::slice::from_raw_parts((m.pData as *const u8).add(y * m.RowPitch as usize) as *const u16, N * 4);
            for x in 0..N {
                out.push([f16_to_f32(row[x * 4]), f16_to_f32(row[x * 4 + 1]), f16_to_f32(row[x * 4 + 2]), f16_to_f32(row[x * 4 + 3])]);
            }
        }
        c.Unmap(&st, 0);
        // Unbind so the next render starts clean.
        c.OMSetRenderTargets(None, None);
        c.PSSetShaderResources(3, Some(&[None, None]));
        if extra.is_some() {
            c.PSSetShaderResources(6, Some(&[None, None]));
            c.PSSetShaderResources(20, Some(&[None]));
            c.PSSetConstantBuffers(6, Some(&[None]));
        }
        out
    }
}

// ---- which shader ------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
struct Kind {
    hash: u32,
    hlsl: &'static str,
    entry: &'static str,
    vs: &'static str,
    cb: &'static [[f32; 4]],
    t1: Option<[f32; 4]>,
}

const LIGHT: Kind = Kind { hash: LIGHT_ADJUST_HASH, hlsl: LIGHT_ADJUST_HLSL, entry: LIGHT_ADJUST_ENTRY, vs: VS_LIGHT, cb: &LIGHT_CB, t1: Some(LIGHT_T1) };
const BRIGHT: Kind = Kind { hash: BRIGHT_PASS_HASH, hlsl: BRIGHT_PASS_HLSL, entry: BRIGHT_PASS_ENTRY, vs: VS_TAPS, cb: &BRIGHT_CB, t1: None };
const BLOOM: Kind = Kind { hash: BLOOM_THRESHOLD_HASH, hlsl: BRIGHT_PASS_HLSL, entry: BLOOM_THRESHOLD_ENTRY, vs: VS_TAPS, cb: &BRIGHT_CB, t1: None };

/// (original output, replacement output) for the same inputs.
fn run_both(g: &Gpu, k: Kind, t0: &[[f32; 4]]) -> (Vec<[f32; 4]>, Vec<[f32; 4]>) {
    let orig = &original_blobs().unwrap().iter().find(|(h, _)| *h == k.hash).unwrap().1;
    let ours = compile_ps(k.hlsl, k.entry).unwrap_or_else(|e| panic!("{}: {e}", k.entry));
    let inp = Inputs { t0, t1: k.t1, cb: k.cb };
    (render(g, k.vs, orig, &inp), render(g, k.vs, &ours, &inp))
}

fn lcg(seed: &mut u32) -> f32 {
    *seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
    (*seed >> 8) as f32 / (1u32 << 24) as f32
}

fn random_texels(seed: u32, lo: f32, hi: f32) -> Vec<[f32; 4]> {
    let mut s = seed;
    (0..N * N).map(|_| [lo + (hi - lo) * lcg(&mut s), lo + (hi - lo) * lcg(&mut s), lo + (hi - lo) * lcg(&mut s), lcg(&mut s)]).collect()
}

fn uniform(v: [f32; 4]) -> Vec<[f32; 4]> {
    vec![v; N * N]
}

fn max_diff(a: &[[f32; 4]], b: &[[f32; 4]]) -> f32 {
    a.iter().zip(b).flat_map(|(p, q)| (0..4).map(move |i| (p[i] - q[i]).abs())).fold(0.0, f32::max)
}

// ---- tests -------------------------------------------------------------------------------------

#[test]
fn light_adjust_matches_original_for_in_range_input() {
    let Some(g) = setup() else { return };
    let t0 = random_texels(1, 0.0, 1.0);
    let (orig, ours) = run_both(&g, LIGHT, &t0);
    // The original saturates the final colour; the replacement does not. For in-range INPUT the tone
    // remap can still push a saturated colour above 1.0 (Y' is lifted), which is exactly the part that
    // is meant to differ. So: (1) where the replacement stays <= 1 the two must agree, and
    // (2) everywhere the original must equal the replacement clamped to 1.
    let mut inside = 0;
    let mut above = 0;
    let mut max_inside = 0.0f32;
    let mut max_clamped = 0.0f32;
    for (o, n) in orig.iter().zip(&ours) {
        for i in 0..4 {
            max_clamped = max_clamped.max((o[i] - n[i].clamp(0.0, 1.0)).abs());
            if n[i] <= 1.0 {
                inside += 1;
                max_inside = max_inside.max((o[i] - n[i]).abs());
            } else {
                above += 1;
            }
        }
    }
    println!("light_adjust in-range: {inside} channels <=1 (max diff {max_inside}), {above} channels >1 from the lifted tone (max diff after clamp {max_clamped}); tolerance {TOL}");
    assert!(inside > 1000, "test is not exercising the in-range case");
    assert!(max_inside <= TOL, "max diff {max_inside} > {TOL}");
    assert!(max_clamped <= TOL, "max diff after clamp {max_clamped} > {TOL}");
}

#[test]
fn light_adjust_passes_values_above_one() {
    let Some(g) = setup() else { return };
    let (orig, ours) = run_both(&g, LIGHT, &uniform([3.0, 3.0, 3.0, 1.0]));
    let (omax, nmin) = (orig.iter().map(|p| p[0].max(p[1]).max(p[2])).fold(0.0, f32::max), ours.iter().map(|p| p[0].min(p[1]).min(p[2])).fold(f32::MAX, f32::min));
    println!("light_adjust 3.0: original max {omax}, replacement min {nmin}");
    assert!(omax <= 1.0, "original exceeded 1: {omax}");
    assert!(nmin > 1.0, "replacement did not pass values above 1: {nmin}");
}

/// What bloom_threshold does after the threshold: white-out of strong bloom, then the soft knee (strength 1).
fn bloom_post(b: [f32; 3]) -> [f32; 3] {
    let ls = 0.2125 * b[0] + 0.7154 * b[1] + 0.0721 * b[2];
    let mb = b[0].max(b[1]).max(b[2]);
    let w = 0.85 * ((mb - 0.35) / 1.0).clamp(0.0, 1.0);
    let b = b.map(|c| c + (ls - c) * w);
    let mb = b[0].max(b[1]).max(b[2]);
    b.map(|c| c / (1.0 + mb / 0.55))
}

fn bright_in_range(k: Kind) {
    let Some(g) = setup() else { return };
    let (mut orig, ours) = run_both(&g, k, &random_texels(7, 0.0, 1.0));
    if k.entry == BLOOM.entry {
        // In range the original output is the plain threshold result; ours adds the white-out and the soft knee.
        for p in orig.iter_mut() {
            let b = bloom_post([p[0], p[1], p[2]]);
            p[..3].copy_from_slice(&b);
        }
    }
    let d = max_diff(&orig, &ours);
    println!("{}: in-range max diff {d} (tolerance {TOL})", k.entry);
    assert!(d <= TOL, "{}: max diff {d} > {TOL}", k.entry);
}

fn bright_above_one(k: Kind) {
    let Some(g) = setup() else { return };
    let (orig, ours) = run_both(&g, k, &uniform([3.0, 3.0, 3.0, 1.0]));
    let omax = orig.iter().flat_map(|p| p[..3].iter().copied()).fold(0.0, f32::max);
    let nmin = ours.iter().flat_map(|p| p[..3].iter().copied()).fold(f32::MAX, f32::min);
    println!("{} 3.0: original max {omax}, replacement min {nmin}", k.entry);
    assert!(omax <= 1.0, "original exceeded 1: {omax}");
    assert!(nmin > 1.0, "replacement did not pass values above 1: {nmin}");
}

#[test]
fn bright_pass_matches_original_in_range() {
    bright_in_range(BRIGHT);
    bright_in_range(BLOOM);
}

#[test]
fn bright_pass_passes_values_above_one() {
    bright_above_one(BRIGHT);
}

#[test]
fn bloom_threshold_is_capped_at_the_games_own_maximum_with_the_hue_kept() {
    let Some(g) = setup() else { return };
    // The original saturates every channel at 1.0; the substitute keeps the energy of the INPUT but never lets the
    // bloom it writes exceed the original's maximum (largest channel 1.0), scaling the three channels together.
    let (orig, ours) = run_both(&g, BLOOM, &uniform([3.0, 2.0, 1.0, 1.0]));
    let o = orig[0];
    let n = ours[0];
    assert!(o[0].max(o[1]).max(o[2]) <= 1.0);
    let top = n[0].max(n[1]).max(n[2]);
    assert!(top <= 0.55 + 2e-3, "the soft knee keeps the largest channel under 0.55, got {top}");
    assert!(n[0] > 0.0 && n[1] > 0.0 && n[2] > 0.0, "{n:?}");
    // The whitened bloom, then the second white-out and the knee.
    let y = 0.2125f32 * 3.0 + 0.7154 * 2.0 + 0.0721 * 1.0;
    let b = [3.0f32 * y - 0.10, 2.0 * y - 0.05, 1.0 * y - 0.08];
    let l = 0.2125 * b[0] + 0.7154 * b[1] + 0.0721 * b[2];
    let w = ((l - 1.0) * 0.5).clamp(0.0, 1.0);
    let wb = [b[0] + (l - b[0]) * w, b[1] + (l - b[1]) * w, b[2] + (l - b[2]) * w];
    let e = bloom_post(wb);
    for i in 0..3 {
        assert!((n[i] - e[i]).abs() < 3e-3, "ch {i}: {} vs {}", n[i], e[i]);
    }
}

#[test]
fn no_nan_or_negative_output() {
    let Some(g) = setup() else { return };
    let vals = [0.0f32, 1e-6, 100.0, 1000.0, -1.0, f32::NAN, 1.0, 0.5];
    // Every value appears in every channel position, over a shifting pattern.
    let t0: Vec<[f32; 4]> = (0..N * N).map(|i| [vals[i % 8], vals[(i / 8 + 3) % 8], vals[(i / 3 + 5) % 8], vals[(i / 5) % 8]]).collect();
    for k in [LIGHT, BRIGHT, BLOOM] {
        let (_, ours) = run_both(&g, k, &t0);
        for (i, p) in ours.iter().enumerate() {
            for c in 0..4 {
                assert!(p[c].is_finite() && p[c] >= 0.0, "{}: pixel {i} channel {c} = {} (input {:?})", k.entry, p[c], t0[i]);
            }
        }
        println!("{}: finite and >= 0 for {} pixels", k.entry, ours.len());
    }
}

// ---- resolve (Task 8): needs WARP only, not GW2_EXE ------------------------------------------------

const GAMMA_VALUES: [f32; 11] = [0.0, 0.25, 0.5, 0.79, 0.81, 1.0, 1.0001, 1.5, 3.0, 10.0, 0.999];

/// Every value in every channel position, over a shifting pattern; alpha 0.5.
fn gamma_texels() -> Vec<[f32; 4]> {
    (0..N * N).map(|i| [GAMMA_VALUES[i % 11], GAMMA_VALUES[(i / 11 + 2) % 11], GAMMA_VALUES[(i / 3 + 5) % 11], 0.5]).collect()
}

fn sdr_params() -> ToneParams {
    ToneParams { shoulder_start: 0.8, peak: 1.0, paper_white: 1.0 }
}

fn hdr_params(h: f32) -> ToneParams {
    ToneParams { shoulder_start: 0.8, peak: h * 200.0, paper_white: 200.0 }
}

fn expected(input: &[[f32; 4]], p: &ToneParams) -> Vec<[f32; 4]> {
    input
        .iter()
        .map(|t| {
            let o = tonemap::map([t[0], t[1], t[2]], p);
            [o[0], o[1], o[2], t[3]]
        })
        .collect()
}

fn through_f16(v: &[[f32; 4]]) -> Vec<[f32; 4]> {
    v.iter().map(|p| p.map(|c| f16_to_f32(f32_to_f16(c)))).collect()
}

#[test]
fn resolve_hlsl_compiles() {
    let vs = compile(resolve::HLSL, resolve::VS_ENTRY, "vs_5_0").unwrap_or_else(|e| panic!("vs: {e}"));
    let ps = compile_ps(resolve::HLSL, resolve::PS_ENTRY).unwrap_or_else(|e| panic!("ps: {e}"));
    assert_eq!(&vs[..4], b"DXBC");
    let dis = disasm(&ps);
    let (_, dcls) = interface_of(&dis);
    assert!(dcls.contains(&"dcl_constantbuffer CB0[1], immediateIndexed".to_string()), "{dis}");
    assert!(dcls.contains(&"dcl_resource_texture2d (float,float,float,float) t0".to_string()), "{dis}");
}

#[test]
fn resolve_shader_matches_rust_reference() {
    let Some(g) = gpu() else { return };
    let ps = compile_ps(resolve::HLSL, resolve::PS_ENTRY).expect("resolve ps");
    let t0 = gamma_texels();
    for (name, p) in [("h=1", sdr_params()), ("h=4", hdr_params(4.0))] {
        let cb = [[p.shoulder_start, p.headroom(), 0.0, 0.0]];
        let out = render(&g, VS_LIGHT, &ps, &Inputs { t0: &t0, t1: None, cb: &cb });
        let want = expected(&t0, &p);
        let d = max_diff(&out, &want);
        println!("resolve {name}: max diff vs tonemap::map {d} over {} pixels", out.len());
        assert!(d <= 2e-3, "{name}: max diff {d} > 2e-3");
        if p.headroom() <= 1.0 {
            // SDR: the output is exactly min(input, 1) in the encoded domain. Inputs are rounded to
            // float16 first so that storing the result in the float16 target adds no error of its own.
            let exact = through_f16(&t0);
            let out = render(&g, VS_LIGHT, &ps, &Inputs { t0: &exact, t1: None, cb: &cb });
            let clip: Vec<[f32; 4]> = exact.iter().map(|t| [t[0].min(1.0), t[1].min(1.0), t[2].min(1.0), t[3]]).collect();
            let dc = max_diff(&out, &clip);
            println!("resolve {name}: max diff vs min(input, 1) {dc}");
            assert!(dc <= 1e-4, "{name}: not a hard clip, diff {dc}");
        }
        // The curve is really active: the brightest input is compressed to the headroom.
        let top = out.iter().map(|p| p[0].max(p[1]).max(p[2])).fold(0.0, f32::max);
        assert!(top <= tonemap::srgb_encode(p.headroom()) + 2e-3, "{name}: output {top} above the headroom");
    }
}

#[test]
fn resolve_shader_highlight_boost_matches_reference() {
    let Some(g) = gpu() else { return };
    let ps = compile_ps(resolve::HLSL, resolve::PS_ENTRY).expect("resolve ps");
    let t0 = gamma_texels();
    let p = hdr_params(4.0);
    let gain = 3.0f32;
    let cb = [[p.shoulder_start, p.headroom(), gain, 0.0]];
    let out = render(&g, VS_LIGHT, &ps, &Inputs { t0: &t0, t1: None, cb: &cb });
    let plain = render(&g, VS_LIGHT, &ps, &Inputs { t0: &t0, t1: None, cb: &[[p.shoulder_start, p.headroom(), 1.0, 0.0]] });
    let want: Vec<[f32; 4]> = t0
        .iter()
        .map(|t| {
            let lin = [tonemap::srgb_decode(t[0]), tonemap::srgb_decode(t[1]), tonemap::srgb_decode(t[2])];
            let e = tonemap::expand(lin, gain);
            let (s, h) = (p.knee(), p.headroom());
            [tonemap::srgb_encode(tonemap::shoulder(e[0], s, h)), tonemap::srgb_encode(tonemap::shoulder(e[1], s, h)), tonemap::srgb_encode(tonemap::shoulder(e[2], s, h)), t[3]]
        })
        .collect();
    let d = max_diff(&out, &want);
    println!("resolve highlight x{gain}: max diff vs reference {d}");
    assert!(d <= 3e-3, "boost differs from the reference by {d}");
    // The boost is really visible: some pixel is clearly brighter than without it.
    let gain_seen = out.iter().zip(&plain).map(|(a, b)| a[0] - b[0]).fold(0.0, f32::max);
    assert!(gain_seen > 0.05, "boost changed nothing (max gain {gain_seen})");
}

/// Reference count of a COM object as seen from outside (AddRef then Release).
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

fn read_back(g: &Gpu, t: &ID3D11Texture2D) -> Vec<[f32; 4]> {
    unsafe {
        let st = tex2d(g, N as u32, N as u32, DXGI_FORMAT_R16G16B16A16_FLOAT, D3D11_BIND_FLAG(0), D3D11_USAGE_STAGING, D3D11_CPU_ACCESS_READ.0 as u32, None);
        g.ctx.CopyResource(&st, t);
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

#[test]
fn resolve_applies_in_place_and_restores_pipeline_state() {
    let Some(g) = gpu() else { return };
    unsafe {
        let src = gamma_texels();
        let bytes: Vec<u8> = src.iter().flat_map(|p| p.iter().flat_map(|c| f32_to_f16(*c).to_le_bytes())).collect();
        let scene = tex2d(&g, N as u32, N as u32, DXGI_FORMAT_R16G16B16A16_FLOAT, D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE, D3D11_USAGE_DEFAULT, 0, Some((&bytes, (N * 8) as u32)));
        let mut scene_rtv = None;
        g.dev.CreateRenderTargetView(&scene, None, Some(&mut scene_rtv)).unwrap();
        let scene_rtv = scene_rtv.unwrap();

        // Sentinel state the resolve must restore.
        let other = tex2d(&g, 32, 32, DXGI_FORMAT_R16G16B16A16_FLOAT, D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE, D3D11_USAGE_DEFAULT, 0, None);
        let mut other_rtv = None;
        g.dev.CreateRenderTargetView(&other, None, Some(&mut other_rtv)).unwrap();
        let other_rtv = other_rtv.unwrap();
        // A separate texture for the SRV: the runtime would unbind an SRV of a texture that is also a bound RT.
        let sampled = tex2d(&g, 8, 8, DXGI_FORMAT_R16G16B16A16_FLOAT, D3D11_BIND_SHADER_RESOURCE, D3D11_USAGE_DEFAULT, 0, None);
        let mut other_srv = None;
        g.dev.CreateShaderResourceView(&sampled, None, Some(&mut other_srv)).unwrap();
        let other_srv = other_srv.unwrap();
        let c = &g.ctx;
        let vp = D3D11_VIEWPORT { TopLeftX: 1.0, TopLeftY: 2.0, Width: 30.0, Height: 20.0, MinDepth: 0.0, MaxDepth: 1.0 };
        c.OMSetRenderTargets(Some(&[Some(other_rtv.clone())]), None);
        c.RSSetViewports(Some(&[vp]));
        c.RSSetScissorRects(Some(&[windows::Win32::Foundation::RECT { left: 3, top: 4, right: 20, bottom: 25 }]));
        c.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_POINTLIST);
        c.PSSetShaderResources(0, Some(&[Some(other_srv.clone())]));
        let rs_desc = D3D11_RASTERIZER_DESC { FillMode: D3D11_FILL_SOLID, CullMode: D3D11_CULL_NONE, DepthClipEnable: true.into(), ..Default::default() };
        let mut rs = None;
        g.dev.CreateRasterizerState(&rs_desc, Some(&mut rs)).unwrap();
        let rs = rs.unwrap();
        let mut bd = D3D11_BLEND_DESC::default();
        bd.RenderTarget[0].RenderTargetWriteMask = 0xf;
        let mut bs = None;
        g.dev.CreateBlendState(&bd, Some(&mut bs)).unwrap();
        let bs = bs.unwrap();
        let dd = D3D11_DEPTH_STENCIL_DESC { DepthEnable: true.into(), DepthWriteMask: D3D11_DEPTH_WRITE_MASK_ZERO, DepthFunc: D3D11_COMPARISON_ALWAYS, ..Default::default() };
        let mut ds = None;
        g.dev.CreateDepthStencilState(&dd, Some(&mut ds)).unwrap();
        let ds = ds.unwrap();
        c.RSSetState(&rs);
        c.OMSetBlendState(&bs, Some(&[0.1, 0.2, 0.3, 0.4]), 0x0f0f);
        c.OMSetDepthStencilState(&ds, 7);
        let before = (refs(&other_rtv), refs(&other_srv), refs(&other), refs(&sampled));

        let ctx_raw = c.as_raw();
        let params = hdr_params(4.0);
        assert!(resolve::apply_with(ctx_raw, scene_rtv.as_raw(), (N as u32, N as u32), &params), "apply failed");
        assert!(resolve::apply_with(ctx_raw, scene_rtv.as_raw(), (N as u32, N as u32), &params), "second apply failed");

        // The scene texture holds the curve applied twice (it is not idempotent above the knee).
        let once = through_f16(&expected(&through_f16(&src), &params));
        let twice = expected(&once, &params);
        let got = read_back(&g, &scene);
        let d = max_diff(&got, &twice);
        println!("resolve in place (two passes): max diff {d}");
        assert!(d <= 3e-3, "max diff {d}");

        // State restored.
        let mut rtvs: [Option<ID3D11RenderTargetView>; 8] = Default::default();
        c.OMGetRenderTargets(Some(&mut rtvs), None);
        assert_eq!(rtvs[0].as_ref().unwrap().as_raw(), other_rtv.as_raw(), "render target not restored");
        assert!(rtvs[1..].iter().all(|r| r.is_none()));
        drop(rtvs);
        let mut n = 2u32;
        let mut vps = [D3D11_VIEWPORT::default(); 2];
        c.RSGetViewports(&mut n, Some(vps.as_mut_ptr()));
        assert_eq!(n, 1);
        assert_eq!((vps[0].TopLeftX, vps[0].TopLeftY, vps[0].Width, vps[0].Height), (1.0, 2.0, 30.0, 20.0));
        let mut m = 2u32;
        let mut rects = [windows::Win32::Foundation::RECT::default(); 2];
        c.RSGetScissorRects(&mut m, Some(rects.as_mut_ptr()));
        assert_eq!((m, rects[0].left, rects[0].top, rects[0].right, rects[0].bottom), (1, 3, 4, 20, 25));
        assert_eq!(c.IAGetPrimitiveTopology(), D3D_PRIMITIVE_TOPOLOGY_POINTLIST);
        let mut srv = [None];
        c.PSGetShaderResources(0, Some(&mut srv));
        assert_eq!(srv[0].as_ref().unwrap().as_raw(), other_srv.as_raw(), "SRV 0 not restored");
        drop(srv);
        assert_eq!(c.RSGetState().unwrap().as_raw(), rs.as_raw(), "rasterizer state not restored");
        let (mut got_bs, mut factor, mut mask) = (None, [0.0f32; 4], 0u32);
        c.OMGetBlendState(Some(&mut got_bs), Some(&mut factor), Some(&mut mask));
        assert_eq!((got_bs.as_ref().unwrap().as_raw(), factor, mask), (bs.as_raw(), [0.1, 0.2, 0.3, 0.4], 0x0f0f), "blend state not restored");
        let (mut got_ds, mut sref) = (None, 0u32);
        c.OMGetDepthStencilState(Some(&mut got_ds), Some(&mut sref));
        assert_eq!((got_ds.as_ref().unwrap().as_raw(), sref), (ds.as_raw(), 7), "depth-stencil state not restored");
        drop((got_bs, got_ds));
        let mut ps = None;
        c.PSGetShader(&mut ps, None, None);
        assert!(ps.is_none(), "pixel shader not restored (none was bound)");
        let mut vs = None;
        c.VSGetShader(&mut vs, None, None);
        assert!(vs.is_none(), "vertex shader not restored (none was bound)");
        let mut cb = [None];
        c.PSGetConstantBuffers(0, Some(&mut cb));
        assert!(cb[0].is_none(), "constant buffer 0 not restored (none was bound)");

        // No COM reference taken by the guard is left behind.
        assert_eq!((refs(&other_rtv), refs(&other_srv), refs(&other), refs(&sampled)), before, "reference counts changed");
        assert!(!crate::d3d::ignoring(), "ignore flag left on");
    }
}

// ---- final scRGB conversion (Task 9): WARP only; the original is compared when GW2_EXE is set -----

fn final_texels(v: f32) -> Vec<[f32; 4]> {
    vec![[v, v * 0.5, v * 0.25, 0.75]; N * N]
}

/// min(decode(tex * gain) * paper_white / 80, peak / 80); alpha is tex.a * gain (the original multiplies all four).
fn final_reference(t: [f32; 4], gain: f32, paper_white: f32, peak: f32) -> [f32; 4] {
    // Linear light (1.0 = paper white), then the hue-preserving shoulder on the largest channel, paper
    // white scale and the clamp at the peak: the same order as final_scrgb.hlsl.
    let lin = [tonemap::srgb_decode(t[0] * gain), tonemap::srgb_decode(t[1] * gain), tonemap::srgb_decode(t[2] * gain)];
    // Energy bleaching towards white (nothing up to 1.5x paper white).
    let m0 = lin[0].max(lin[1]).max(lin[2]);
    let yl = 0.2126 * lin[0] + 0.7152 * lin[1] + 0.0722 * lin[2];
    let kb = 0.4 * ((m0 - 1.5) / (m0 + 1.5)).clamp(0.0, 1.0);
    let lin = [lin[0] + (yl - lin[0]) * kb, lin[1] + (yl - lin[1]) * kb, lin[2] + (yl - lin[2]) * kb];
    let m = lin[0].max(lin[1]).max(lin[2]);
    let (h, knee) = crate::output::shoulder_constants(paper_white, peak);
    let k = if m > knee {
        let r = (h - knee).max(1e-4);
        (knee + r * (1.0 - (-(m - knee) / r).exp())) / m
    } else {
        1.0
    };
    let c = |x: f32| (x * k * (paper_white / 80.0)).min(peak / 80.0);
    [c(lin[0]), c(lin[1]), c(lin[2]), t[3] * gain]
}

fn final_original_blob() -> Option<&'static Vec<u8>> {
    static B: OnceLock<Option<Vec<u8>>> = OnceLock::new();
    B.get_or_init(|| {
        let path = std::env::var("GW2_EXE").ok()?;
        let data = std::fs::read(&path).expect("read GW2_EXE");
        let b = crate::shaders::tests::dxbc_blobs(&data).into_iter().find(|b| fnv1a32(b) == crate::output::FINAL_HASH);
        Some(b.expect("afdc14cb not found in GW2_EXE").to_vec())
    })
    .as_ref()
}

#[test]
fn final_scrgb_hlsl_compiles_with_the_original_interface() {
    let dxbc = compile_ps(&crate::output::final_hlsl(203.0, 1000.0), crate::output::FINAL_ENTRY).unwrap_or_else(|e| panic!("{e}"));
    let got = interface_of(&disasm(&dxbc));
    let want = (
        strs(&["SV_POSITION 0 xyzw 0 POS float", "TEXCOORD 0 xy 1 NONE float xy"]),
        strs(&["dcl_constantbuffer CB0[1], immediateIndexed", "dcl_sampler s0, mode_default", "dcl_resource_texture2d (float,float,float,float) t0", "dcl_input_ps linear v1.xy", "dcl_output o0.xyzw"]),
    );
    assert_eq!(got, want, "interface differs from afdc14cb");
    if let Some(orig) = final_original_blob() {
        assert_eq!(interface_of(&disasm(orig)), got, "differs from the blob in the exe");
    }
}

#[test]
fn final_scrgb_matches_reference() {
    let Some(g) = gpu() else { return };
    let ps = compile_ps(&crate::output::final_hlsl(203.0, 1000.0), crate::output::FINAL_ENTRY).expect("final ps");
    let mut worst = 0.0f32;
    for v in [0.0f32, 0.25, 0.5, 1.0, 2.5, 20.0] {
        for gain in [1.0f32, 0.5] {
            let t0 = final_texels(v);
            let out = render(&g, VS_LIGHT, &ps, &Inputs { t0: &t0, t1: None, cb: &[[gain, 0.0, 0.0, 0.0]] });
            let want = final_reference(t0[0], gain, 203.0, 1000.0);
            for p in &out {
                for i in 0..3 {
                    // The output is stored as float16 (relative precision 2^-11): allow that on top of 2e-3.
                    let tol = 2e-3 + want[i].abs() * 1e-3;
                    let d = (p[i] - want[i]).abs();
                    worst = worst.max(d / tol);
                    assert!(d <= tol, "v={v} gain={gain} ch={i}: got {} want {}", p[i], want[i]);
                }
                assert!((p[3] - want[3]).abs() <= 2e-3, "alpha is {} for gain {gain}", p[3]);
            }
        }
    }
    println!("final_scrgb: worst error is {worst:.3} of the tolerance");
}

#[test]
fn final_scrgb_never_exceeds_the_peak() {
    let Some(g) = gpu() else { return };
    for peak in [400.0f32, 1000.0, 4000.0] {
        let ps = compile_ps(&crate::output::final_hlsl(203.0, peak), crate::output::FINAL_ENTRY).expect("final ps");
        for v in [1.0f32, 20.0, 1000.0] {
            let t0 = final_texels(v);
            let out = render(&g, VS_LIGHT, &ps, &Inputs { t0: &t0, t1: None, cb: &[[1.0, 0.0, 0.0, 0.0]] });
            for p in &out {
                for c in &p[..3] {
                    assert!(*c <= peak / 80.0 * 1.002, "v={v} peak={peak}: {c} exceeds {}", peak / 80.0);
                }
            }
        }
        // 20.0 is far above any peak: the first channel sits exactly on it.
        let out = render(&g, VS_LIGHT, &ps, &Inputs { t0: &final_texels(20.0), t1: None, cb: &[[1.0, 0.0, 0.0, 0.0]] });
        assert!((out[0][0] - peak / 80.0).abs() <= peak / 80.0 * 1e-3, "{} vs {}", out[0][0], peak / 80.0);
    }
}

#[test]
fn original_final_pass_is_unchanged_sdr_passthrough() {
    let Some(orig) = final_original_blob() else {
        println!("skipped: GW2_EXE is not set");
        return;
    };
    let Some(g) = gpu() else { return };
    let t0 = uniform([0.5, 0.5, 0.5, 1.0]);
    let out = render(&g, VS_LIGHT, orig, &Inputs { t0: &t0, t1: None, cb: &[[1.0, 0.0, 0.0, 0.0]] });
    assert!(out.iter().all(|p| (p[0] - 0.5).abs() < 1e-3 && (p[1] - 0.5).abs() < 1e-3), "the original does not pass 0.5 through");
    // The substitute in the same harness gives decode(0.5) * 203/80 instead: the two differ on purpose.
    let ps = compile_ps(&crate::output::final_hlsl(203.0, 1000.0), crate::output::FINAL_ENTRY).unwrap();
    let ours = render(&g, VS_LIGHT, &ps, &Inputs { t0: &t0, t1: None, cb: &[[1.0, 0.0, 0.0, 0.0]] });
    let want = tonemap::srgb_decode(0.5) * 203.0 / 80.0;
    assert!((ours[0][0] - want).abs() < 2e-3, "{} vs {want}", ours[0][0]);
}

// ---- bloom strength ------------------------------------------------------------------------------

#[test]
fn bloom_strength_is_clamped() {
    assert_eq!(clamp_bloom(-1.0), 0.0);
    assert_eq!(clamp_bloom(0.0), 0.0);
    assert_eq!(clamp_bloom(1.25), 1.25);
    assert_eq!(clamp_bloom(9.0), 2.0);
    assert_eq!(clamp_bloom(f32::INFINITY), 2.0);
    assert_eq!(clamp_bloom(f32::NAN), 1.0);
    assert!(bloom_hlsl(0.5).starts_with("#define BLOOM_STRENGTH 0.5\n"));
    assert!(bloom_hlsl(7.0).starts_with("#define BLOOM_STRENGTH 2.0\n"));
}

#[test]
fn set_bloom_strength_reinstalls_the_substitute() {
    install_all();
    let dev = 0x5eed;
    assert!(crate::shaders::substitutes().get_or_create(dev, BLOOM_THRESHOLD_HASH, |_, _| Ok(0x10)).is_some());
    assert_eq!(set_bloom_strength(0.5), 0.5);
    let got = crate::shaders::substitutes().get_or_create(dev, BLOOM_THRESHOLD_HASH, |h, e| {
        assert!(h.starts_with("#define BLOOM_STRENGTH 0.5\n"), "{h}");
        assert_eq!(e, BLOOM_THRESHOLD_ENTRY);
        Ok(0x20)
    });
    assert_eq!(got, Some(0x20), "the cached shader was not invalidated");
    assert_eq!(set_bloom_strength(40.0), 2.0);
    assert_eq!(bloom_strength(), 2.0);
    set_bloom_strength(1.0);
}

#[test]
fn bloom_strength_scales_bright_pass_output() {
    let Some(g) = gpu() else { return };
    // Inputs within the game's range: the bloom stays under the cap at strength 1, so the scaling is exactly linear.
    let t0 = random_texels(11, 0.0, 0.6);
    let run = |s: f32| {
        let ps = compile_ps(&bloom_hlsl(s), BLOOM_THRESHOLD_ENTRY).unwrap_or_else(|e| panic!("{e}"));
        render(&g, VS_TAPS, &ps, &Inputs { t0: &t0, t1: None, cb: &BRIGHT_CB })
    };
    let (full, half, zero) = (run(1.0), run(0.5), run(0.0));
    let mut checked = 0;
    for (f, h) in full.iter().zip(&half) {
        for i in 0..3 {
            // f16 output: allow its rounding of the two results
            // The soft knee compresses the larger value more: half the strength gives MORE than half the bloom, never more than all of it.
            assert!(h[i] >= f[i] * 0.5 - 1e-4 && h[i] <= f[i] + 1e-4, "{} vs {}", f[i], h[i]);
            if f[i] > 0.1 {
                checked += 1;
            }
        }
        assert_eq!(f[3], h[3], "alpha must not scale");
    }
    assert!(checked > 20, "test is not exercising bright pixels ({checked})");
    assert!(zero.iter().all(|p| p[..3].iter().all(|c| *c == 0.0)));
    // At 1.0 the output is the file compiled without any define (what the equivalence tests cover).
    let plain = compile_ps(BRIGHT_PASS_HLSL, BLOOM_THRESHOLD_ENTRY).unwrap();
    let base = render(&g, VS_TAPS, &plain, &Inputs { t0: &t0, t1: None, cb: &BRIGHT_CB });
    assert_eq!(base, full);
}

#[test]
fn known_engine_hashes_are_named_and_noted_once() {
    assert_eq!(known_engine_hash(0xf5cd4832), Some("f5cd4832"));
    assert_eq!(known_engine_hash(0x2ed4a75f), Some("2ed4a75f"));
    assert_eq!(known_engine_hash(0x14b1e90e), Some("14b1e90e"));
    assert_eq!(known_engine_hash(0xafdc14cb), Some("afdc14cb"));
    assert_eq!(known_engine_hash(0x12345678), None);
    assert_eq!(note_engine_hash(0xf5cd4832), Some("f5cd4832"));
    assert_eq!(note_engine_hash(0xf5cd4832), None);
    assert_eq!(note_engine_hash(0x12345678), None);
}

// ---- the data path: the light-adjust substitute's boost / AO / SDR clamp / debug inputs -----------------

fn run_light(g: &Gpu, t0: &[[f32; 4]], extra: Option<(f32, [f32; 4])>) -> Vec<[f32; 4]> {
    let ps = compile_ps(LIGHT_ADJUST_HLSL, LIGHT_ADJUST_ENTRY).unwrap_or_else(|e| panic!("{e}"));
    render_extra(g, VS_LIGHT, &ps, &Inputs { t0, t1: Some(LIGHT_T1), cb: &LIGHT_CB }, extra)
}

fn px(v: &[[f32; 4]], x: usize, y: usize) -> [f32; 4] {
    v[y * N + x]
}

/// The light-adjust substitute with the halo inputs: t7 = uniform `glow` (None: unbound), b6[1].x = `strength`.
/// b6[0] is "no boost, no AO, HDR".
fn run_halo(g: &Gpu, t0: &[[f32; 4]], glow: Option<[f32; 4]>, strength: f32, debug: f32) -> Vec<[f32; 4]> {
    let ps = compile_ps(LIGHT_ADJUST_HLSL, LIGHT_ADJUST_ENTRY).unwrap_or_else(|e| panic!("{e}"));
    render_full(g, VS_LIGHT, &ps, &Inputs { t0, t1: Some(LIGHT_T1), cb: &LIGHT_CB }, Some((1.0, [1.0, 0.0, 0.0, debug])), Some((glow, strength)))
}

#[test]
fn light_adjust_boost_matches_the_reference_curve() {
    let Some(g) = gpu() else { return };
    let gain = 3.0f32;
    let mut boosted_any = false;
    for v in [0.05f32, 0.3, 0.6, 1.0, 2.0] {
        let t0 = uniform([v, v * 0.8, v * 0.6, 1.0]);
        let plain = px(&run_light(&g, &t0, None), 7, 7);
        let boost = px(&run_light(&g, &t0, Some((1.0, [gain, 0.0, 0.0, 0.0]))), 7, 7);
        // The shader boosts the linear light after the remap and encodes with gamma 2.0: the reference is
        // the same function applied to the unboosted result (the dither, +-0.002, is in both).
        let lin = [plain[0] * plain[0], plain[1] * plain[1], plain[2] * plain[2]];
        let ex = tonemap::expand(lin, gain);
        // After the boost a large energy bleaches towards white (light_adjust_hdr.hlsl).
        let mb = ex[0].max(ex[1]).max(ex[2]);
        let yb = 0.2126 * ex[0] + 0.7152 * ex[1] + 0.0722 * ex[2];
        let k = ((mb - 2.0) / (mb + 2.0)).clamp(0.0, 1.0).min(0.4);
        let want = [ex[0] + (yb - ex[0]) * k, ex[1] + (yb - ex[1]) * k, ex[2] + (yb - ex[2]) * k].map(f32::sqrt);
        for i in 0..3 {
            let tol = 0.02 + want[i].abs() * 3e-2; // dither, float16 storage and the squaring of the plain result
            assert!((boost[i] - want[i]).abs() <= tol, "v={v} ch={i}: boosted {} reference {} (plain {})", boost[i], want[i], plain[i]);
        }
        if boost[0] > plain[0] * 1.2 {
            boosted_any = true;
        }
        if lin[0].max(lin[1]).max(lin[2]) < tonemap::EXPAND_START {
            assert!((boost[0] - plain[0]).abs() <= 0.005, "v={v}: a dark pixel must not be boosted ({} vs {})", boost[0], plain[0]);
        }
    }
    assert!(boosted_any, "the boost changed nothing for any input");
}

#[test]
fn light_adjust_boost_is_off_when_the_gain_is_one_or_the_inputs_are_unbound() {
    let Some(g) = gpu() else { return };
    let t0 = uniform([1.5, 1.2, 0.9, 1.0]);
    let plain = run_light(&g, &t0, None);
    let one = run_light(&g, &t0, Some((1.0, [1.0, 0.0, 0.0, 0.0])));
    assert!(max_diff(&plain, &one) <= 0.006, "gain 1 must equal the unbound behaviour");
}

/// The symptom "highlight_boost changes nothing": the slider (1 .. 4) goes through `effective_gain` with the display
/// headroom of a typical setup (1405 / 400 nits = 3.51) into b6; pixels at several brightness levels must come out clearly
/// brighter at 2.5 than at 1 and again at 4, the dark ones untouched, nothing above the headroom, no NaN. With the halo on
/// (a big uniform bright area: not a light source) the surface share still gives a visible step.
#[test]
fn light_adjust_highlight_slider_has_a_visible_calibrated_effect() {
    let Some(g) = gpu() else { return };
    let headroom = 1405.0f32 / 400.0;
    let lin_max = |p: [f32; 4]| (p[0] * p[0]).max(p[1] * p[1]).max(p[2] * p[2]);
    let mut seen_mid = false;
    for v in [0.3f32, 0.55, 0.7, 0.85, 1.0, 1.2] {
        let t0 = uniform([v, v * 0.95, v * 0.88, 1.0]);
        let at = |slider: f32, halo: bool| {
            let gain = crate::datapath::effective_gain(slider, headroom);
            let ps = compile_ps(LIGHT_ADJUST_HLSL, LIGHT_ADJUST_ENTRY).unwrap_or_else(|e| panic!("{e}"));
            let glow = halo.then_some((Some([0.0, 0.0, 0.0, v * v]), 1.0f32));
            let out = render_full(&g, VS_LIGHT, &ps, &Inputs { t0: &t0, t1: Some(LIGHT_T1), cb: &LIGHT_CB }, Some((1.0, [gain, 0.0, 0.0, 0.0])), glow);
            assert!(finite_nonneg(&out), "NaN or negative at v {v} slider {slider}");
            lin_max(px(&out, 7, 7))
        };
        let (s1, s25, s4) = (at(1.0, false), at(2.5, false), at(4.0, false));
        let (h1, h4) = (at(1.0, true), at(4.0, true));
        println!("v {v}: plain lin max {s1:.3}; slider 2.5 x{:.2}, slider 4 x{:.2}; with the halo (surface) slider 4 x{:.2}", s25 / s1, s4 / s1, h4 / h1);
        assert!(s4 <= headroom * 1.03, "v {v}: {s4} above the headroom {headroom}");
        if s1 < tonemap::EXPAND_START * 0.95 {
            assert!((s4 - s1).abs() <= s1 * 0.02 + 1e-3, "v {v}: a dark pixel ({s1}) must not be boosted ({s4})");
            continue;
        }
        if s1 >= 0.6 {
            seen_mid = true;
            assert!(s25 > s1 * 1.25, "v {v}: slider 2.5 must be clearly brighter than 1 ({s1} -> {s25})");
            assert!(s4 > s25 * 1.15, "v {v}: slider 4 must be clearly brighter than 2.5 ({s25} -> {s4})");
            assert!(h4 > h1 * 1.1, "v {v}: even a big bright surface (halo on) must show the slider ({h1} -> {h4})");
            assert!(h4 < s4, "v {v}: a surface gets less than a light ({h4} vs {s4})");
        }
    }
    assert!(seen_mid, "the test needs pixels in the expansion range");
}

#[test]
fn light_adjust_ao_follows_the_visibility_and_spares_bright_pixels() {
    let Some(g) = gpu() else { return };
    let mid = uniform([0.5, 0.4, 0.3, 1.0]);
    let plain = px(&run_light(&g, &mid, None), 7, 7);
    let open = px(&run_light(&g, &mid, Some((1.0, [1.0, 1.0, 0.0, 0.0]))), 7, 7);
    let shut = px(&run_light(&g, &mid, Some((0.0, [1.0, 1.0, 0.0, 0.0]))), 7, 7);
    let off = px(&run_light(&g, &mid, Some((0.0, [1.0, 0.0, 0.0, 0.0]))), 7, 7);
    for i in 0..3 {
        assert!((open[i] - plain[i]).abs() <= 0.006, "visibility 1 must not darken (ch {i}: {} vs {})", open[i], plain[i]);
        assert!((off[i] - plain[i]).abs() <= 0.006, "AO strength 0 must not darken (ch {i})");
        assert!(shut[i] < plain[i] - 0.02, "visibility 0 must darken (ch {i}: {} vs {})", shut[i], plain[i]);
    }
    // A bright pixel (a light or an effect) is released from the occlusion.
    let bright = uniform([1.5, 1.3, 1.1, 1.0]);
    let b_plain = px(&run_light(&g, &bright, None), 7, 7);
    let b_shut = px(&run_light(&g, &bright, Some((0.0, [1.0, 1.0, 0.0, 0.0]))), 7, 7);
    for i in 0..3 {
        assert!((b_shut[i] - b_plain[i]).abs() <= 0.006, "a bright pixel must not be darkened (ch {i}: {} vs {})", b_shut[i], b_plain[i]);
    }
}

#[test]
fn light_adjust_sdr_clamp_saturates_like_the_original() {
    let Some(g) = gpu() else { return };
    let t0 = uniform([3.0, 3.0, 3.0, 1.0]);
    // SDR flag set, with a boost gain that must be ignored.
    let out = run_light(&g, &t0, Some((1.0, [4.0, 0.0, 1.0, 0.0])));
    for p in &out {
        for c in &p[..3] {
            assert!(*c <= 1.0, "SDR output above 1: {c}");
        }
    }
    let hdr = run_light(&g, &t0, Some((1.0, [1.0, 0.0, 0.0, 0.0])));
    assert!(hdr.iter().all(|p| p[0].min(p[1]).min(p[2]) > 1.0), "HDR output must pass values above 1");
}

#[test]
fn light_adjust_debug_fill_paints_the_halves() {
    let Some(g) = gpu() else { return };
    let t0 = uniform([0.5, 0.4, 0.3, 1.0]);
    let plain = run_light(&g, &t0, None);
    let out = run_light(&g, &t0, Some((1.0, [1.0, 0.0, 0.0, 2.0])));
    assert_eq!(px(&out, 10, 32)[..3], [1.0, 0.0, 0.0], "left half is red");
    assert_eq!(px(&out, 50, 10)[..3], [0.0, 0.0, 0.0], "top right is black");
    let (a, b) = (px(&out, 50, 50), px(&plain, 50, 50));
    for i in 0..3 {
        assert!((a[i] - b[i]).abs() <= 0.006, "bottom right is the scene (ch {i})");
    }
}

#[test]
fn final_scrgb_shoulder_keeps_the_hue_and_stays_under_the_headroom() {
    let Some(g) = gpu() else { return };
    let ps = compile_ps(&crate::output::final_hlsl(203.0, 1000.0), crate::output::FINAL_ENTRY).expect("final ps");
    let enc = tonemap::srgb_encode;
    // Linear (6, 3, 1.5): the first channel is above the knee (3.94), the others are below it.
    let t0 = uniform([enc(6.0), enc(3.0), enc(1.5), 1.0]);
    let out = px(&render(&g, VS_LIGHT, &ps, &Inputs { t0: &t0, t1: None, cb: &[[1.0, 0.0, 0.0, 0.0]] }), 5, 5);
    let scale = 203.0 / 80.0;
    assert!(out[0] < 6.0 * scale * 0.99, "the shoulder must compress: {} vs {}", out[0], 6.0 * scale);
    assert!(out[0] <= 1000.0 / 80.0, "never above the peak");
    // Bright saturated colour bleaches towards white: less saturated than the input (0.75), but still tinted.
    let sat = (out[0].max(out[1]).max(out[2]) - out[0].min(out[1]).min(out[2])) / out[0].max(out[1]).max(out[2]);
    assert!(sat < 0.85 && sat > 0.2, "saturation of the bleached colour is {sat}");
    // Up to 1.5x paper white nothing changes (the UI and ordinary scenes): linear 1.5 -> 1.5 * paper white scale.
    let low = uniform([enc(1.5), enc(0.75), enc(0.375), 1.0]);
    let o = px(&render(&g, VS_LIGHT, &ps, &Inputs { t0: &low, t1: None, cb: &[[1.0, 0.0, 0.0, 0.0]] }), 5, 5);
    assert!((o[0] - 1.5 * scale).abs() < 0.02, "below the bleaching the output is untouched: {} vs {}", o[0], 1.5 * scale);
    assert!((o[0] / o[1] - 2.0).abs() < 0.01, "ratio r/g is {}", o[0] / o[1]);
}

#[test]
fn shoulder_constants_follow_the_headroom() {
    let (h, k) = crate::output::shoulder_constants(203.0, 1000.0);
    assert!((h - 1000.0 / 203.0).abs() < 1e-5);
    assert!((k - 0.8 * h).abs() < 1e-5);
    // Little headroom: the knee never goes below SDR white.
    let (_, k) = crate::output::shoulder_constants(400.0, 450.0);
    assert!(k >= 1.0);
    // No headroom: no shoulder at all.
    let (h, k) = crate::output::shoulder_constants(400.0, 400.0);
    assert_eq!(h, 1.0);
    assert!(k > 1e8);
}

#[test]
fn bloom_threshold_whitens_strong_bloom_and_leaves_the_games_range_alone() {
    let Some(g) = gpu() else { return };
    let ps = compile_ps(BRIGHT_PASS_HLSL, BLOOM_THRESHOLD_ENTRY).unwrap_or_else(|e| panic!("{e}"));
    let sat = |p: [f32; 4]| {
        let m = p[0].max(p[1]).max(p[2]);
        if m > 0.0 { (m - p[0].min(p[1]).min(p[2])) / m } else { 0.0 }
    };
    let run = |c: [f32; 3]| {
        let t0 = uniform([c[0], c[1], c[2], 1.0]);
        render(&g, VS_TAPS, &ps, &Inputs { t0: &t0, t1: None, cb: &BRIGHT_CB })[0]
    };
    // Reference of the whole function for a uniform input.
    let want = |c: [f32; 3]| {
        let y = 0.2125 * c[0] + 0.7154 * c[1] + 0.0721 * c[2];
        let b = [(c[0] * y - 0.10).max(0.0), (c[1] * y - 0.05).max(0.0), (c[2] * y - 0.08).max(0.0)];
        let l = 0.2125 * b[0] + 0.7154 * b[1] + 0.0721 * b[2];
        let w = ((l - 1.0) * 0.5).clamp(0.0, 1.0);
        let wb = [b[0] + (l - b[0]) * w, b[1] + (l - b[1]) * w, b[2] + (l - b[2]) * w];
        // ...then the strong-bloom whitening (after the strength), and the cap at the game's own maximum.
        let l2 = 0.2125 * wb[0] + 0.7154 * wb[1] + 0.0721 * wb[2];
        let w2 = 0.85 * ((wb[0].max(wb[1]).max(wb[2]) - 0.35) / 1.0).clamp(0.0, 1.0);
        let wb = [wb[0] + (l2 - wb[0]) * w2, wb[1] + (l2 - wb[1]) * w2, wb[2] + (l2 - wb[2]) * w2];
        let m = wb[0].max(wb[1]).max(wb[2]);
        let k = 1.0 / (1.0 + m / 0.55);
        [wb[0] * k, wb[1] * k, wb[2] * k]
    };
    for c in [[0.2f32, 0.6, 0.9], [0.5, 0.5, 0.5], [0.9, 0.9, 1.0], [0.2, 1.6, 2.0], [0.4, 2.5, 3.0]] {
        let (got, w) = (run(c), want(c));
        for i in 0..3 {
            assert!((got[i] - w[i]).abs() <= 2e-3 + w[i].abs() * 2e-3, "{c:?} ch {i}: {} vs {}", got[i], w[i]);
        }
    }
    // A strong cyan light: the bloom is much less saturated than without the whitening.
    let strong = [0.4f32, 2.5, 3.0];
    let y = 0.2125 * strong[0] + 0.7154 * strong[1] + 0.0721 * strong[2];
    let raw = [(strong[0] * y - 0.10).max(0.0), (strong[1] * y - 0.05).max(0.0), (strong[2] * y - 0.08).max(0.0), 1.0];
    assert!(sat(run(strong)) < sat(raw) * 0.6, "strong bloom must be whitened: {} vs {}", sat(run(strong)), sat(raw));
    // Within the game's own range (energy <= 1) nothing changes: exactly the original's threshold.
    let mild = [0.2f32, 0.5, 0.7];
    let ym = 0.2125 * mild[0] + 0.7154 * mild[1] + 0.0721 * mild[2];
    let plain = [(mild[0] * ym - 0.10).max(0.0), (mild[1] * ym - 0.05).max(0.0), (mild[2] * ym - 0.08).max(0.0)];
    let got = run(mild);
    // Only the soft knee 1 / (1 + max / 0.55) applies to dim bloom (the same factor on every channel, so hue is untouched).
    let k = 1.0 / (1.0 + plain[0].max(plain[1]).max(plain[2]) / 0.55);
    for i in 0..3 {
        assert!((got[i] - plain[i] * k).abs() <= 1e-3, "dim bloom only gets the soft knee (ch {i}: {} vs {})", got[i], plain[i] * k);
    }
}

#[test]
fn light_adjust_bleaches_over_range_colour_in_hdr_and_clips_it_in_sdr() {
    let Some(g) = gpu() else { return };
    let sat = |p: [f32; 4]| {
        let m = p[0].max(p[1]).max(p[2]);
        if m > 0.0 { (m - p[0].min(p[1]).min(p[2])) / m } else { 0.0 }
    };
    // Encoded (0.5, 1.5, 2.0): linear (0.25, 2.25, 4.0), saturation 0.94 before the remap.
    let hot = uniform([0.5, 1.5, 2.0, 1.0]);
    let out = px(&run_light(&g, &hot, None), 7, 7);
    assert!(sat(out) < 0.7, "an over-range colour must bleach towards white in HDR (saturation {})", sat(out));
    assert!(out[0].max(out[1]).max(out[2]) > 1.0, "...but keep its energy: {out:?}");
    // The same colour within the game's range keeps its saturation (the in-range equivalence is pinned elsewhere).
    let mild = uniform([0.3, 0.6, 0.9, 1.0]);
    let m = px(&run_light(&g, &mild, None), 7, 7);
    assert!(sat(m) > 0.5, "an in-range colour must not be bleached (saturation {})", sat(m));
    // SDR: every channel is clipped at 1.0 first, exactly like the 8-bit engine: (0.5, 1.5, 2.0) == (0.5, 1, 1).
    let sdr_flag = Some((1.0, [1.0, 0.0, 1.0, 0.0]));
    let a = px(&run_light(&g, &hot, sdr_flag), 7, 7);
    let b = px(&run_light(&g, &uniform([0.5, 1.0, 1.0, 1.0]), sdr_flag), 7, 7);
    for i in 0..3 {
        assert!((a[i] - b[i]).abs() <= 0.006, "SDR must clip the input per channel (ch {i}: {} vs {})", a[i], b[i]);
        assert!(a[i] <= 1.0);
    }
}

#[test]
fn light_adjust_boosted_colour_bleaches_towards_white_as_the_gain_grows() {
    let Some(g) = gpu() else { return };
    let sat = |p: [f32; 4]| {
        let m = p[0].max(p[1]).max(p[2]);
        if m > 0.0 { (m - p[0].min(p[1]).min(p[2])) / m } else { 0.0 }
    };
    // Encoded (0.1, 0.5, 1.0): a saturated cyan-blue light inside the game's range.
    let cyan = uniform([0.6, 0.9, 1.0, 1.0]);
    let at = |gain: f32| px(&run_light(&g, &cyan, Some((1.0, [gain, 0.0, 0.0, 0.0]))), 7, 7);
    let (low, mid, high) = (at(1.0), at(2.0), at(8.0));
    // Up to 2x SDR white nothing is bleached (the boost only scales: same saturation).
    assert!((sat(low) - sat(mid)).abs() < 0.05, "a mild boost keeps the colour ({} vs {})", sat(low), sat(mid));
    // A large boost keeps the energy and loses saturation.
    assert!(high[2] > mid[2] * 1.3, "the energy grows with the gain ({} vs {})", high[2], mid[2]);
    assert!(sat(high) < sat(mid) - 0.1, "a large boost must bleach towards white ({} vs {})", sat(high), sat(mid));
}

#[test]
fn light_adjust_halo_is_the_identity_at_strength_zero_or_with_t7_unbound() {
    let Some(g) = gpu() else { return };
    let t0 = uniform([0.7, 0.55, 0.4, 1.0]);
    let plain = run_light(&g, &t0, Some((1.0, [1.0, 0.0, 0.0, 0.0])));
    // Strength 0 with a very bright halo texture bound: not read.
    let zero = run_halo(&g, &t0, Some([5.0, 5.0, 5.0, 0.0]), 0.0, 0.0);
    assert!(max_diff(&plain, &zero) <= 0.006, "strength 0 must be the identity: {}", max_diff(&plain, &zero));
    // Strength 1 with t7 unbound reads zero: the identity too.
    let unbound = run_halo(&g, &t0, None, 1.0, 0.0);
    assert!(max_diff(&plain, &unbound) <= 0.006, "an unbound t7 must be the identity: {}", max_diff(&plain, &unbound));
    // A black halo texture (the fallback the data path binds when the halo did not run).
    let black = run_halo(&g, &t0, Some([0.0; 4]), 1.0, 0.0);
    assert!(max_diff(&plain, &black) <= 0.006);
}

#[test]
fn light_adjust_halo_adds_linear_light_and_scales_with_the_strength() {
    let Some(g) = gpu() else { return };
    let t0 = uniform([0.5, 0.4, 0.3, 1.0]);
    let at = |s: f32| px(&run_halo(&g, &t0, Some([0.0, 0.2, 0.4, 0.0]), s, 0.0), 9, 9);
    let (p, h1, h2) = (at(0.0), at(1.0), at(2.0));
    // Blue gains the most (the halo is blue-green; the remap is luminance based, so red moves a little too), and 2 > 1 > 0.
    assert!(h1[2] > p[2] + 0.02 && h2[2] > h1[2] + 0.02, "blue {} {} {}", p[2], h1[2], h2[2]);
    assert!(h1[1] > p[1] && h2[1] > h1[1]);
    // It is ADDED before the exposure remap: the result is brighter than the plain colour in every glowing channel.
    println!("halo: plain {p:?} strength 1 {h1:?} strength 2 {h2:?}");
}

/// The light-adjust substitute with the fur strength in b6[1].z, the fur mask in t20 (`None` = unbound) and the fur debug
/// switch in b6[1].w (no boost, no AO, HDR, no halo).
fn run_fur_masked(g: &Gpu, t0: &[[f32; 4]], fur: f32, mask: Option<Vec<u8>>, debug: f32) -> Vec<[f32; 4]> {
    FUR_TEST.with(|f| f.set(fur));
    FUR_MASK.with(|m| *m.borrow_mut() = mask);
    FUR_DEBUG.with(|f| f.set(debug));
    let out = run_halo(g, t0, None, 0.0, 0.0);
    FUR_TEST.with(|f| f.set(0.0));
    FUR_MASK.with(|m| *m.borrow_mut() = None);
    FUR_UV.with(|u| *u.borrow_mut() = None);
    FUR_DEBUG.with(|f| f.set(0.0));
    out
}

fn mask_all(v: u8) -> Vec<u8> {
    vec![v; N * N]
}

/// Left half 255, right half 0.
fn mask_left() -> Vec<u8> {
    (0..N * N).map(|i| if i % N < N / 2 { 255 } else { 0 }).collect()
}

/// Mask fully set: the fur look is applied to every pixel of `t0`.
fn run_fur(g: &Gpu, t0: &[[f32; 4]], fur: f32) -> Vec<[f32; 4]> {
    run_fur_masked(g, t0, fur, Some(mask_all(255)), 0.0)
}

#[test]
fn light_adjust_fur_is_the_identity_at_strength_zero() {
    let Some(g) = gpu() else { return };
    // Mask fully set, mid colour: strength 0 reads nothing and changes nothing.
    let t0 = uniform([0.5, 0.4, 0.3, 0.6]);
    let plain = run_light(&g, &t0, Some((1.0, [1.0, 0.0, 0.0, 0.0])));
    let zero = run_fur(&g, &t0, 0.0);
    assert!(max_diff(&plain, &zero) <= 0.006, "fur 0 must be the identity: {}", max_diff(&plain, &zero));
}

#[test]
fn light_adjust_fur_mask_zero_or_unbound_leaves_the_picture_untouched() {
    let Some(g) = gpu() else { return };
    // Full strength, the scene alpha is in the old "character" class: without a mask nothing may change any more.
    let t0 = uniform([0.5, 0.4, 0.3, 0.6]);
    let plain = run_light(&g, &t0, Some((1.0, [1.0, 0.0, 0.0, 0.0])));
    let zero = run_fur_masked(&g, &t0, 1.0, Some(mask_all(0)), 0.0);
    assert!(max_diff(&plain, &zero) <= 0.006, "mask 0 must be the identity: {}", max_diff(&plain, &zero));
    let unbound = run_fur_masked(&g, &t0, 1.0, None, 0.0);
    assert!(max_diff(&plain, &unbound) <= 0.006, "an unbound mask must be the identity: {}", max_diff(&plain, &unbound));
    // The alpha is not a mask any more: a terrain-class alpha with a full mask is modulated like any other.
    let terrain = uniform([0.5, 0.4, 0.3, 0.1]);
    assert!(max_diff(&run_fur(&g, &terrain, 0.0), &run_fur(&g, &terrain, 1.0)) > 0.01, "the mask, not the alpha, decides");
}

/// Vertical 1 px stripes of two grey levels (a texture with painted strands).
fn stripes(a: f32, b: f32) -> Vec<[f32; 4]> {
    (0..N * N).map(|i| { let v = if i % N % 2 == 0 { a } else { b }; [v, v * 0.9, v * 0.8, 0.6] }).collect()
}

fn finite_nonneg(v: &[[f32; 4]]) -> bool {
    v.iter().all(|p| p.iter().all(|c| c.is_finite() && *c >= 0.0))
}

#[test]
fn light_adjust_fur_flat_masked_area_stays_flat() {
    let Some(g) = gpu() else { return };
    // No pattern of its own: a uniform masked patch has no variance (away from the picture edge nothing is a silhouette).
    for c in [[0.5, 0.4, 0.3, 0.6], [0.9, 0.9, 0.9, 0.6], [0.2, 0.2, 0.25, 0.6]] {
        let on = run_fur(&g, &uniform(c), 1.0);
        let first = px(&on, 10, 10);
        for y in 4..N - 4 {
            for x in 4..N - 4 {
                let p = px(&on, x, y);
                for k in 0..3 {
                    assert!((p[k] - first[k]).abs() <= 0.006, "flat patch must stay flat at ({x},{y}): {p:?} vs {first:?}");
                }
            }
        }
        assert!(finite_nonneg(&on));
    }
}

#[test]
fn light_adjust_fur_textured_patch_gets_bounded_contrast_gain_and_keeps_the_hue() {
    let Some(g) = gpu() else { return };
    let t0 = stripes(0.5, 0.4);
    let off = run_fur(&g, &t0, 0.0);
    let on = run_fur(&g, &t0, 1.0);
    let contrast = |v: &[[f32; 4]]| (4..N - 4).map(|x| (px(v, x, 20)[1] - px(v, x + 1, 20)[1]).abs()).sum::<f32>();
    let (c0, c1) = (contrast(&off), contrast(&on));
    println!("fur contrast: {c0:.3} -> {c1:.3}");
    assert!(c1 > c0 * 1.05, "the detail must gain contrast: {c0} -> {c1}");
    assert!(c1 < c0 * 1.6, "the gain is bounded: {c0} -> {c1}");
    for (a, b) in off.iter().zip(on.iter()) {
        // Hue kept (the small warm term aside): ratio of g to r moves little.
        assert!(((b[1] / b[0]) / (a[1] / a[0]) - 1.0).abs() < 0.08, "hue moved: {a:?} -> {b:?}");
        assert_eq!(b[3], a[3], "alpha untouched");
    }
    // Half the strength, less than the full effect.
    let half = run_fur(&g, &t0, 0.5);
    assert!(contrast(&half) < c1 && contrast(&half) > c0);
    // Stable: two identical runs give identical pixels.
    assert_eq!(on, run_fur(&g, &t0, 1.0));
}

#[test]
fn light_adjust_fur_is_bounded_and_never_nan_or_negative() {
    let Some(g) = gpu() else { return };
    for (a, b) in [(0.5, 0.4), (0.9, 0.2), (0.05, 0.9), (0.2, 0.02), (3.0, 0.5)] {
        let t0 = stripes(a, b);
        let off = run_fur(&g, &t0, 0.0);
        let on = run_fur_masked(&g, &t0, 1.0, Some(mask_left()), 0.0);
        let on_all = run_fur(&g, &t0, 1.0);
        for v in [&on, &on_all] {
            assert!(finite_nonneg(v), "nan / negative for {a},{b}");
            for (p, q) in v.iter().zip(off.iter()) {
                for k in 0..3 {
                    assert!(p[k] <= q[k] * 1.25 + 0.12, "bounded {a},{b}: {} vs {}", p[k], q[k]);
                }
            }
        }
    }
}

#[test]
fn light_adjust_fur_rim_acts_only_near_the_mask_edge_and_outside_is_untouched() {
    let Some(g) = gpu() else { return };
    let t0 = uniform([0.5, 0.4, 0.3, 0.6]);
    let off = run_fur_masked(&g, &t0, 0.0, Some(mask_left()), 0.0);
    let on = run_fur_masked(&g, &t0, 1.0, Some(mask_left()), 0.0);
    let col_mean = |v: &[[f32; 4]], x: usize| (0..N).map(|y| px(v, x, y)[1]).sum::<f32>() / N as f32;
    let edge = col_mean(&on, N / 2 - 1) / col_mean(&off, N / 2 - 1);
    let inner = col_mean(&on, 10) / col_mean(&off, 10);
    println!("fur silhouette: edge column x{edge:.3}, interior x{inner:.3}");
    assert!(edge > inner + 0.01, "the silhouette gets a soft rim: {edge} vs {inner}");
    // Deep inside (more than 4 px from the edge) the flat patch is flat: the rim is local.
    for x in 4..N / 2 - 5 {
        assert!((px(&on, x, 30)[1] - px(&on, 10, 30)[1]).abs() <= 0.006, "rim leaked inside at x{x}");
    }
    // Outside the mask nothing changes, not even next to the edge.
    for y in 0..N {
        for x in N / 2..N {
            let (a, b) = (px(&off, x, y), px(&on, x, y));
            assert!((a[1] - b[1]).abs() <= 0.006, "outside the mask must be untouched at ({x},{y})");
        }
    }
}

#[test]
fn light_adjust_fur_leaves_clipped_and_very_dark_pixels_alone() {
    let Some(g) = gpu() else { return };
    let bright = uniform([0.999, 0.999, 0.999, 0.6]);
    let d = max_diff(&run_fur(&g, &bright, 0.0), &run_fur(&g, &bright, 1.0));
    assert!(d <= 0.006, "a clipped pixel must not change: {d}");
    let dark = uniform([0.02, 0.02, 0.02, 0.6]);
    let d = max_diff(&run_fur(&g, &dark, 0.0), &run_fur(&g, &dark, 1.0));
    assert!(d <= 0.006, "a very dark pixel must not change: {d}");
}

#[test]
fn light_adjust_fur_debug_paints_the_mask_magenta_even_at_strength_zero() {
    let Some(g) = gpu() else { return };
    let t0 = uniform([0.5, 0.4, 0.3, 0.6]);
    let plain = run_fur_masked(&g, &t0, 0.0, Some(mask_left()), 0.0);
    let dbg = run_fur_masked(&g, &t0, 0.0, Some(mask_left()), 1.0);
    let m = px(&dbg, 10, 32);
    assert!(m[0] > 0.7 && m[2] > 0.7 && m[1] < 0.35, "masked pixel must be magenta: {m:?}");
    // Unmasked pixels are exactly the plain picture.
    assert!((px(&dbg, 50, 32)[1] - px(&plain, 50, 32)[1]).abs() <= 0.006);
    // No mask: nothing painted.
    let none = run_fur_masked(&g, &t0, 0.0, None, 1.0);
    assert!(max_diff(&none, &plain) <= 0.006, "debug without a mask paints nothing");
}

// ---- fur strands anchored to the mesh uv (the mask carries rg = frac(uv), b = 1) ----------------------------

/// The light-adjust substitute with the fur on `mask` and the mesh uv `uv` (None = a draw without uv).
fn run_fur_uv(g: &Gpu, t0: &[[f32; 4]], fur: f32, mask: Vec<u8>, uv: Option<Vec<[f32; 2]>>) -> Vec<[f32; 4]> {
    FUR_UV.with(|u| *u.borrow_mut() = uv);
    run_fur_masked(g, t0, fur, Some(mask), 0.0)
}

/// A uv ramp of `gu` / `gv` uv units per pixel, starting at (0.25 + ox * gu, 0.25) at x = 0 (ox shifts the mesh on screen).
fn uv_ramp(gu: f32, gv: f32, ox: f32) -> Vec<[f32; 2]> {
    (0..N * N).map(|i| [0.25 + gu * ((i % N) as f32 - ox), 0.25 + gv * (i / N) as f32]).collect()
}

/// Standard deviation of the green channel over the interior (6 px from every border).
fn interior_std(v: &[[f32; 4]]) -> f32 {
    let vals: Vec<f32> = (6..N - 6).flat_map(|y| (6..N - 6).map(move |x| (x, y))).map(|(x, y)| px(v, x, y)[1]).collect();
    let m = vals.iter().sum::<f32>() / vals.len() as f32;
    (vals.iter().map(|x| (x - m) * (x - m)).sum::<f32>() / vals.len() as f32).sqrt()
}

#[test]
fn light_adjust_fur_strands_appear_on_a_uv_gradient_bounded_and_deterministic() {
    let Some(g) = gpu() else { return };
    let t0 = uniform([0.5, 0.4, 0.3, 0.6]);
    let off = run_fur_uv(&g, &t0, 0.0, mask_all(255), Some(uv_ramp(0.004, 0.004, 0.0)));
    let on = run_fur_uv(&g, &t0, 1.0, mask_all(255), Some(uv_ramp(0.004, 0.004, 0.0)));
    let (s0, s1) = (interior_std(&off), interior_std(&on));
    println!("fur strands: std {s0:.4} -> {s1:.4}");
    assert!(s0 < 0.003, "the input is flat: {s0}");
    assert!(s1 > 0.02, "strands must be visible on a flat patch: {s1}");
    assert!(finite_nonneg(&on));
    for y in 6..N - 6 {
        for x in 6..N - 6 {
            let (a, b) = (px(&off, x, y), px(&on, x, y));
            let r = b[1] / a[1];
            assert!(r > 0.65 && r < 1.45, "amplitude out of bounds at ({x},{y}): x{r}");
            // hue kept: the tint is luminance neutral and a few percent
            assert!(((b[2] / b[0]) / (a[2] / a[0]) - 1.0).abs() < 0.16, "hue moved at ({x},{y}): {a:?} -> {b:?}");
            assert_eq!(b[3], a[3], "alpha untouched");
        }
    }
    // Half the strength, less modulation.
    let half = run_fur_uv(&g, &t0, 0.5, mask_all(255), Some(uv_ramp(0.004, 0.004, 0.0)));
    assert!(interior_std(&half) < s1 && interior_std(&half) > s0);
    // Deterministic: identical input, identical pixels.
    assert_eq!(on, run_fur_uv(&g, &t0, 1.0, mask_all(255), Some(uv_ramp(0.004, 0.004, 0.0))));
}

#[test]
fn light_adjust_fur_strands_are_anchored_to_the_uv_and_move_with_the_mesh() {
    let Some(g) = gpu() else { return };
    let t0 = uniform([0.5, 0.4, 0.3, 0.6]);
    let a = run_fur_uv(&g, &t0, 1.0, mask_all(255), Some(uv_ramp(0.004, 0.004, 0.0)));
    // The same mesh 3 px to the right: uv(x) = old uv(x - 3). The strands move with it.
    let b = run_fur_uv(&g, &t0, 1.0, mask_all(255), Some(uv_ramp(0.004, 0.004, 3.0)));
    for y in 8..N - 8 {
        for x in 10..N - 10 {
            assert!((px(&b, x, y)[1] - px(&a, x - 3, y)[1]).abs() < 0.004, "the pattern must travel with the uv at ({x},{y})");
        }
    }
    // A different place of the uv space has a different pattern (it is not a constant).
    let c = run_fur_uv(&g, &t0, 1.0, mask_all(255), Some((0..N * N).map(|i| [0.6 + 0.004 * (i % N) as f32, 0.7 + 0.004 * (i / N) as f32]).collect()));
    assert!(max_diff(&a, &c) > 0.02);
}

#[test]
fn light_adjust_fur_has_no_pattern_for_a_flat_uv_or_without_uv() {
    let Some(g) = gpu() else { return };
    let t0 = uniform([0.5, 0.4, 0.3, 0.6]);
    let flat_uv = run_fur_uv(&g, &t0, 1.0, mask_all(255), Some(vec![[0.37, 0.61]; N * N]));
    assert!(interior_std(&flat_uv) < 0.003, "uv = const: no pattern, {}", interior_std(&flat_uv));
    let no_uv = run_fur_uv(&g, &t0, 1.0, mask_all(255), None);
    assert!(interior_std(&no_uv) < 0.003, "no uv: the fallback has no strands, {}", interior_std(&no_uv));
    // the fallback is still the pattern-free look (detail + rim + warmth), finite and bounded
    assert!(finite_nonneg(&no_uv));
}

#[test]
fn light_adjust_fur_strands_fade_out_when_the_uv_changes_too_fast() {
    let Some(g) = gpu() else { return };
    let t0 = uniform([0.5, 0.4, 0.3, 0.6]);
    let std_at = |gu: f32| interior_std(&run_fur_uv(&g, &t0, 1.0, mask_all(255), Some(uv_ramp(gu, gu, 0.0))));
    let (near, mid, far) = (std_at(0.004), std_at(0.04), std_at(0.2));
    println!("fur LOD fade: std {near:.4} (0.004 uv/px), {mid:.4} (0.04), {far:.4} (0.2)");
    assert!(near > 0.02);
    assert!(mid < near && mid > far, "the fade is gradual");
    assert!(far < 0.004, "sub-pixel strands (aliasing / moire) must be faded to nothing: {far}");
}

#[test]
fn light_adjust_fur_with_uv_and_mask_zero_leaves_the_picture_untouched() {
    let Some(g) = gpu() else { return };
    let t0 = uniform([0.5, 0.4, 0.3, 0.6]);
    let plain = run_light(&g, &t0, Some((1.0, [1.0, 0.0, 0.0, 0.0])));
    let zero = run_fur_uv(&g, &t0, 1.0, mask_all(0), Some(uv_ramp(0.004, 0.004, 0.0)));
    assert!(max_diff(&plain, &zero) <= 0.006, "mask 0 must be the identity even with a uv: {}", max_diff(&plain, &zero));
    // strength 0 with a uv is the identity too
    let s0 = run_fur_uv(&g, &t0, 0.0, mask_all(255), Some(uv_ramp(0.004, 0.004, 0.0)));
    assert!(max_diff(&plain, &s0) <= 0.006);
}

#[test]
fn light_adjust_fur_strands_never_nan_negative_or_clip() {
    let Some(g) = gpu() else { return };
    for (a, b) in [(0.5, 0.4), (0.9, 0.2), (0.05, 0.9), (0.2, 0.02), (3.0, 0.5), (0.97, 0.97)] {
        let t0 = stripes(a, b);
        for uv in [uv_ramp(0.004, 0.004, 0.0), uv_ramp(0.0005, 0.02, 0.0), uv_ramp(-0.003, 0.0, 0.0), vec![[f32::NAN, 0.5]; N * N], uv_ramp(0.5, 0.5, 0.0)] {
            let off = run_fur_uv(&g, &t0, 0.0, mask_all(255), Some(uv.clone()));
            let on = run_fur_uv(&g, &t0, 1.0, mask_all(255), Some(uv));
            assert!(finite_nonneg(&on), "nan / negative for {a},{b}");
            for (p, q) in on.iter().zip(off.iter()) {
                for k in 0..3 {
                    assert!(p[k] <= q[k] * 1.4 + 0.14, "bounded {a},{b}: {} vs {}", p[k], q[k]);
                    // a pixel at or below 1.0 is not pushed into clipping by the strands (the detail / warmth terms aside)
                    if q[k] > 0.97 && q[k] <= 1.0 {
                        assert!(p[k] <= 1.0 + 0.12, "clip {a},{b}: {} vs {}", p[k], q[k]);
                    }
                }
            }
        }
    }
}

#[test]
fn light_adjust_debug_view_paints_halo_sources_green() {
    let Some(g) = gpu() else { return };
    // Left half: a saturated orange (a source); right half: a mid grey (not a source).
    let t0: Vec<[f32; 4]> = (0..N * N).map(|i| if i % N < N / 2 { [1.0, 0.7, 0.2, 1.0] } else { [0.6, 0.6, 0.6, 1.0] }).collect();
    let out = run_halo(&g, &t0, Some([0.0; 4]), 1.0, 1.0);
    assert_eq!(px(&out, 10, 32)[..3], [0.0, 1.0, 0.0], "a halo source is green");
    assert_ne!(px(&out, 100, 32)[..3], [0.0, 1.0, 0.0], "grey is not a source");
    // Strength 0: the halo is off, nothing is painted.
    let off = run_halo(&g, &t0, Some([0.0; 4]), 0.0, 1.0);
    assert_ne!(px(&off, 10, 32)[..3], [0.0, 1.0, 0.0], "no sources are painted while the halo is off");
    // A bright neutral spot on a bright area is only a source when it stands out of the local mean (G alpha).
    let white: Vec<[f32; 4]> = uniform([1.2, 1.2, 1.2, 1.0]);
    let sky = run_halo(&g, &white, Some([0.0, 0.0, 0.0, 1.5]), 1.0, 1.0);
    assert_ne!(px(&sky, 10, 10)[..3], [0.0, 1.0, 0.0], "a uniform bright area is not a source");
    let bare = run_halo(&g, &white, Some([0.0, 0.0, 0.0, 0.0]), 1.0, 1.0);
    assert_eq!(px(&bare, 10, 10)[..3], [0.0, 1.0, 0.0], "without a local mean the same area stands out (proves the mean is read)");
}

#[test]
fn vanilla_bright_pass_and_bloom_equal_the_original_on_clipped_input() {
    let Some(g) = setup() else { return };
    // With the mod off the scene stays float, so a tap can be above 1. VANILLA clips every tap like the UNORM target did:
    // the substitute fed the raw float input must equal the ORIGINAL shader fed the clipped input.
    let t0 = random_texels(5, 0.0, 3.0);
    let clipped: Vec<[f32; 4]> = t0.iter().map(|p| [p[0].min(1.0), p[1].min(1.0), p[2].min(1.0), p[3].min(1.0)]).collect();
    for k in [BRIGHT, BLOOM] {
        let orig = &original_blobs().unwrap().iter().find(|(h, _)| *h == k.hash).unwrap().1;
        let src = format!("#define VANILLA 1
{}", k.hlsl);
        let ours = compile_ps(&src, k.entry).unwrap_or_else(|e| panic!("{}: {e}", k.entry));
        let a = render(&g, k.vs, orig, &Inputs { t0: &clipped, t1: k.t1, cb: k.cb });
        let b = render(&g, k.vs, &ours, &Inputs { t0: &t0, t1: k.t1, cb: k.cb });
        let d = max_diff(&a, &b);
        println!("{} vanilla vs original on clipped input: max diff {d}", k.entry);
        assert!(d <= TOL, "{}: vanilla differs from the original by {d}", k.entry);
    }
}

#[test]
fn bloom_threshold_divides_the_tint_out_when_the_compensation_is_bound() {
    let Some(g) = gpu() else { return };
    let ps = compile_ps(BRIGHT_PASS_HLSL, BLOOM_THRESHOLD_ENTRY).unwrap_or_else(|e| panic!("{e}"));
    let t0 = uniform([0.5, 0.5, 0.5, 1.0]);
    let run = |extra: Option<(f32, [f32; 4])>| px(&render_extra(&g, VS_TAPS, &ps, &Inputs { t0: &t0, t1: None, cb: &BRIGHT_CB }, extra), 7, 7);
    let plain = run(None);
    // Not valid (w = 0): identical to the unbound behaviour.
    let off = run(Some((1.0, [2.0, 1.0, 0.5, 0.0])));
    for i in 0..3 {
        assert!((off[i] - plain[i]).abs() < 1e-3, "an invalid compensation must change nothing (ch {i})");
    }
    // Valid: each channel is multiplied by its factor (the input keeps the result under the 1.0 cap).
    let f = [2.0f32, 1.0, 0.5];
    let on = run(Some((1.0, [f[0], f[1], f[2], 1.0])));
    for i in 0..3 {
        // The soft knee scales all channels equally: the CHANNEL RATIOS carry the factors.
        let want = plain[i] * f[i] / (plain[1] * f[1]);
        let got = on[i] / on[1];
        assert!((got - want).abs() <= 0.01 + want * 0.01, "ch {i}: ratio {got} vs {want}");
    }
}

// ---- the fog and water release of the AO (fog.rs estimates, light_adjust_hdr.hlsl applies) -------------------------

/// The synthetic world of the fog tests: sky on top, a mid-distance surface on the left, a near surface on the right,
/// and a strip at the bottom that is the farthest geometry. All the AO visibility is 0 (a crease everywhere), so what
/// differs between pixels is only their colour and depth.
const FOG_SKY_ROWS: usize = 16;
const FOG_STRIP_ROWS: usize = 8;

fn fog_depth_at(x: usize, y: usize, far_outlier: bool) -> f32 {
    if far_outlier {
        // Nothing near the largest depth: one single sampled pixel is at 100, everything else is at most 50.
        return if (x, y) == (20, 40) { 100.0 } else if x < N / 2 { 50.0 } else { 20.0 };
    }
    if y < FOG_SKY_ROWS {
        0.0 // invalid depth = sky
    } else if y >= N - FOG_STRIP_ROWS {
        100.0
    } else if x < N / 2 {
        50.0
    } else {
        20.0
    }
}

/// Runs the estimator on the scene and renders the light-adjust substitute with the AO fully closed (visibility 0) and
/// the fog state bound (`bind_fog`) or not. Returns the output.
fn fog_render(g: &Gpu, fog_colour: [f32; 3], mid: [f32; 3], near: [f32; 3], far_outlier: bool, ao_strength: f32, bind_fog: bool) -> Vec<[f32; 4]> {
    let scene = |x: usize, y: usize| -> [f32; 4] {
        let c = if far_outlier {
            if x < N / 2 { mid } else { near }
        } else if y < FOG_SKY_ROWS || y >= N - FOG_STRIP_ROWS {
            fog_colour
        } else if x < N / 2 {
            mid
        } else {
            near
        };
        [c[0], c[1], c[2], 1.0]
    };
    let t0: Vec<[f32; 4]> = (0..N * N).map(|i| scene(i % N, i / N)).collect();
    let bytes: Vec<u8> = t0.iter().flat_map(|p| p.iter().flat_map(|c| c.to_le_bytes())).collect();
    let dbytes: Vec<u8> = (0..N * N).flat_map(|i| fog_depth_at(i % N, i / N, far_outlier).to_le_bytes()).collect();
    unsafe {
        let st = tex2d(g, N as u32, N as u32, DXGI_FORMAT_R32G32B32A32_FLOAT, D3D11_BIND_SHADER_RESOURCE, D3D11_USAGE_IMMUTABLE, 0, Some((&bytes, (N * 16) as u32)));
        let dt = tex2d(g, N as u32, N as u32, DXGI_FORMAT_R32_FLOAT, D3D11_BIND_SHADER_RESOURCE, D3D11_USAGE_IMMUTABLE, 0, Some((&dbytes, (N * 4) as u32)));
        let mut ssrv = None;
        g.dev.CreateShaderResourceView(&st, None, Some(&mut ssrv)).unwrap();
        let blobs = crate::fog::compile_blobs().expect("fog.hlsl compiles");
        let mut fog = crate::fog::Fog::new(g.dev.as_raw(), &blobs).expect("Fog::new");
        assert!(fog.run(g.dev.as_raw(), g.ctx.as_raw(), ssrv.as_ref().unwrap().as_raw(), dt.as_raw()), "Fog::run failed");
        let state = ID3D11ShaderResourceView::from_raw_borrowed(&fog.state_srv()).unwrap().clone();
        let depth = ID3D11ShaderResourceView::from_raw_borrowed(&fog.depth_view()).unwrap().clone();
        let ps = compile_ps(LIGHT_ADJUST_HLSL, LIGHT_ADJUST_ENTRY).unwrap_or_else(|e| panic!("{e}"));
        let hook = |c: &ID3D11DeviceContext| {
            if bind_fog {
                c.PSSetShaderResources(3, Some(&[Some(state.clone()), Some(depth.clone())]));
            }
        };
        // vis 0 everywhere, AO strength `ao_strength` (b6[0].y), no boost, HDR.
        render_hook(g, VS_LIGHT, &ps, &Inputs { t0: &t0, t1: Some(LIGHT_T1), cb: &LIGHT_CB }, Some((0.0, [1.0, ao_strength, 0.0, 0.0])), None, &hook)
    }
}

/// The darkening of a pixel: output with the AO closed divided by the output with the AO off (same scene).
fn fog_darkening(g: &Gpu, fogc: [f32; 3], mid: [f32; 3], near: [f32; 3], outlier: bool, bind: bool) -> (f32, f32) {
    let on = fog_render(g, fogc, mid, near, outlier, 1.0, bind);
    let off = fog_render(g, fogc, mid, near, outlier, 0.0, bind);
    let k = |x: usize, y: usize| px(&on, x, y)[1] / px(&off, x, y)[1].max(1e-4);
    (k(10, 30), k(50, 30)) // (mid-distance surface near F in colour, near surface of another colour)
}

/// (fog colour, mid-distance surface close to it, near surface of a clearly different colour): none of them bluish,
/// so the blue-water fallback of the shader plays no part.
const FOGS: [(&str, [f32; 3], [f32; 3], [f32; 3]); 3] = [
    ("gray", [0.45, 0.45, 0.45], [0.48, 0.46, 0.47], [0.60, 0.30, 0.15]),
    ("green", [0.30, 0.50, 0.28], [0.32, 0.48, 0.30], [0.55, 0.30, 0.45]),
    ("orange", [0.60, 0.40, 0.20], [0.58, 0.41, 0.22], [0.20, 0.35, 0.30]),
];

#[test]
fn light_adjust_releases_the_ao_on_fog_of_any_colour_but_not_on_a_near_surface() {
    let Some(g) = gpu() else { return };
    for (name, f, mid, near) in FOGS {
        let (fogged, solid) = fog_darkening(&g, f, mid, near, false, true);
        println!("fog {name}: mid-distance surface near F x{fogged:.3}, near surface x{solid:.3}");
        assert!(fogged > 0.97, "{name}: a mid-distance surface the colour of the fog was darkened (x{fogged})");
        assert!(solid < 0.85, "{name}: a near surface of another colour must keep the AO (x{solid})");
    }
}

#[test]
fn light_adjust_without_a_fog_estimate_is_exactly_todays_behaviour() {
    let Some(g) = gpu() else { return };
    for (name, f, mid, near) in FOGS {
        // (b1) F unknown: only one single pixel is far, so the confidence stays 0 and the state is bound but inert.
        let (fogged, solid) = fog_darkening(&g, f, mid, near, true, true);
        println!("fog {name}, F unknown: mid x{fogged:.3}, near x{solid:.3}");
        assert!(fogged < 0.85 && solid < 0.85, "{name}: with F unknown nothing may be released ({fogged}, {solid})");
        let with = fog_render(&g, f, mid, near, true, 1.0, true);
        let without = fog_render(&g, f, mid, near, true, 1.0, false);
        assert!(max_diff(&with, &without) <= 1e-3, "{name}: an unknown F must equal the unbound (t3/t4 read zero) behaviour");
    }
    // (b2) t3 / t4 unbound (feature off) with a known F elsewhere: the mid surface is darkened like before.
    let (name, f, mid, near) = FOGS[0];
    let (fogged, _) = fog_darkening(&g, f, mid, near, false, false);
    assert!(fogged < 0.85, "{name}: unbound fog inputs must not release anything ({fogged})");
}

#[test]
fn light_adjust_keeps_the_blue_water_rule_as_the_fallback() {
    let Some(g) = gpu() else { return };
    // A bluish surface, F unknown: the old haze rule still releases it (blue above the mean of red and green by 0.2).
    let blue = [0.25, 0.35, 0.60];
    let on = fog_render(&g, [0.45; 3], blue, blue, true, 1.0, true);
    let off = fog_render(&g, [0.45; 3], blue, blue, true, 0.0, true);
    let k = px(&on, 10, 30)[2] / px(&off, 10, 30)[2];
    assert!(k > 0.97, "the blue-water fallback must stay (x{k})");
}

#[test]
fn light_adjust_treats_negative_scene_channels_as_zero() {
    let Some(g) = gpu() else { return };
    // A float scene can hold negative channels (subtractive effects); squaring them must not create energy.
    let neg = uniform([-1.5, 0.5, 0.5, 1.0]);
    let zero = uniform([0.0, 0.5, 0.5, 1.0]);
    let a = px(&run_light(&g, &neg, None), 7, 7);
    let b = px(&run_light(&g, &zero, None), 7, 7);
    for i in 0..3 {
        assert!((a[i] - b[i]).abs() <= 0.01, "negative input must equal zero (ch {i}: {} vs {})", a[i], b[i]);
    }
}

// ---- contact shadows and aerial perspective in the light-adjust substitute (b10, t22; contact.rs / aerial.rs) -----------

/// The inputs of one render: the fog state (t3: F rgb + confidence, zref), the depth (t4), the contact visibility (t22, half size,
/// None = unbound) and b10 (None = unbound).
struct Atmos {
    f: [f32; 3],
    k: f32,
    zref: f32,
    bind_fog: bool,
    depth: fn(usize, usize) -> f32,
    vis: Option<fn(usize, usize) -> f32>,
    b10: Option<[f32; 4]>,
}

// Depths are the engine's NORMALISED linear depth (1.0 = far plane = sky; metres = depth * depth_unit, 1000 here). These tests
// used to feed world units (50, 5000) with a unit of 0.0254 or 1 m, which the real engine never produces.
const ATMOS_ZREF: f32 = 0.9;
const UNIT_M: f32 = 1000.0;

fn atmos_render(g: &Gpu, t0: &[[f32; 4]], a: &Atmos) -> Vec<[f32; 4]> {
    unsafe {
        // 3x1 state: texel 0 the fog colour F, texel 1 zref, texel 2 the aerial haze colour H (here the same colour as F).
        let st: Vec<u8> = [a.f[0], a.f[1], a.f[2], a.k, a.zref, 0.0, 0.0, 1.0, a.f[0], a.f[1], a.f[2], a.k].iter().flat_map(|v| v.to_le_bytes()).collect();
        let state = tex2d(g, 3, 1, DXGI_FORMAT_R32G32B32A32_FLOAT, D3D11_BIND_SHADER_RESOURCE, D3D11_USAGE_IMMUTABLE, 0, Some((&st, 48)));
        let db: Vec<u8> = (0..N * N).flat_map(|i| (a.depth)(i % N, i / N).to_le_bytes()).collect();
        let dtex = tex2d(g, N as u32, N as u32, DXGI_FORMAT_R32_FLOAT, D3D11_BIND_SHADER_RESOURCE, D3D11_USAGE_IMMUTABLE, 0, Some((&db, (N * 4) as u32)));
        let (mut ssrv, mut dsrv) = (None, None);
        g.dev.CreateShaderResourceView(&state, None, Some(&mut ssrv)).unwrap();
        g.dev.CreateShaderResourceView(&dtex, None, Some(&mut dsrv)).unwrap();
        let mut vsrv = None;
        if let Some(f) = a.vis {
            let h = N / 2;
            let vb: Vec<u8> = (0..h * h).flat_map(|i| f32_to_f16(f(i % h, i / h)).to_le_bytes()).collect();
            let vt = tex2d(g, h as u32, h as u32, DXGI_FORMAT_R16_FLOAT, D3D11_BIND_SHADER_RESOURCE, D3D11_USAGE_IMMUTABLE, 0, Some((&vb, (h * 2) as u32)));
            g.dev.CreateShaderResourceView(&vt, None, Some(&mut vsrv)).unwrap();
        }
        let mut cb10 = None;
        if let Some(v) = a.b10 {
            let b: Vec<u8> = v.iter().flat_map(|x| x.to_le_bytes()).collect();
            let d = D3D11_BUFFER_DESC { ByteWidth: 16, Usage: D3D11_USAGE_IMMUTABLE, BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32, ..Default::default() };
            let sd = D3D11_SUBRESOURCE_DATA { pSysMem: b.as_ptr() as *const c_void, SysMemPitch: 0, SysMemSlicePitch: 0 };
            g.dev.CreateBuffer(&d, Some(&sd), Some(&mut cb10)).unwrap();
        }
        let ps = compile_ps(LIGHT_ADJUST_HLSL, LIGHT_ADJUST_ENTRY).unwrap_or_else(|e| panic!("{e}"));
        let hook = |c: &ID3D11DeviceContext| {
            if a.bind_fog {
                c.PSSetShaderResources(3, Some(&[ssrv.clone(), dsrv.clone()]));
            }
            if vsrv.is_some() {
                c.PSSetShaderResources(22, Some(&[vsrv.clone()]));
            }
            if cb10.is_some() {
                c.PSSetConstantBuffers(10, Some(&[cb10.clone()]));
            }
        };
        let out = render_hook(g, VS_LIGHT, &ps, &Inputs { t0, t1: Some(LIGHT_T1), cb: &LIGHT_CB }, Some((1.0, [1.0, 0.0, 0.0, 0.0])), None, &hook);
        g.ctx.PSSetShaderResources(22, Some(&[None]));
        g.ctx.PSSetConstantBuffers(10, Some(&[None]));
        out
    }
}

fn near_depth(_: usize, _: usize) -> f32 {
    0.05
}
fn far_depth(_: usize, _: usize) -> f32 {
    ATMOS_ZREF
}
fn sky_depth(_: usize, _: usize) -> f32 {
    0.0
}
fn no_light(_: usize, _: usize) -> f32 {
    0.0
}
fn lit(_: usize, _: usize) -> f32 {
    1.0
}

const MID: [f32; 4] = [0.55, 0.45, 0.35, 1.0];
const HAZE: [f32; 3] = [0.62, 0.70, 0.80];

fn atmos(depth: fn(usize, usize) -> f32, vis: Option<fn(usize, usize) -> f32>, b10: Option<[f32; 4]>) -> Atmos {
    Atmos { f: HAZE, k: 1.0, zref: ATMOS_ZREF, bind_fog: true, depth, vis, b10 }
}

fn lum_of(p: [f32; 4]) -> f32 {
    0.2126 * p[0] + 0.7152 * p[1] + 0.0722 * p[2]
}

/// The radial-over-view-depth factor of pixel (20, 20), where the aerial tests sample (square picture).
fn ray20() -> f32 {
    crate::aerial::ray_factor(20.5 / N as f32, 20.5 / N as f32, 1.0)
}

/// `lin` mixed toward the haze colour by `share`, as an encoded (gamma 2) texel.
fn haze_mixed(c: [f32; 4], share: f32) -> [f32; 4] {
    let ch = |i: usize| ((1.0 - share) * c[i] * c[i] + share * HAZE[i] * HAZE[i]).sqrt();
    [ch(0), ch(1), ch(2), 1.0]
}

#[test]
fn light_adjust_contact_and_aerial_at_zero_or_unbound_are_bit_identical() {
    let Some(g) = gpu() else { return };
    let t0 = random_texels(5, 0.0, 1.4);
    let base = atmos_render(&g, &t0, &atmos(near_depth, None, None));
    let plain = run_light(&g, &t0, Some((1.0, [1.0, 0.0, 0.0, 0.0])));
    assert_eq!(base, plain, "unbound b10 / t22 must not change a bit");
    // Bound everything, but both strengths 0 (unit and distance set): still bit-identical. Contact visibility 0 everywhere.
    let zero = atmos_render(&g, &t0, &atmos(far_depth, Some(no_light), Some([0.0, 0.0, 400.0, 1000.0])));
    assert_eq!(zero, plain, "strengths 0 must not change a bit");
    // t22 bound without b10: ignored.
    let fog_only = atmos_render(&g, &t0, &atmos(far_depth, Some(no_light), None));
    let fog_only_ref = atmos_render(&g, &t0, &atmos(far_depth, None, None));
    assert_eq!(fog_only, fog_only_ref, "t22 without b10 must be ignored");
}

#[test]
fn light_adjust_contact_darkens_softly_by_at_most_the_strength() {
    let Some(g) = gpu() else { return };
    let t0 = uniform(MID);
    let off = atmos_render(&g, &t0, &atmos(near_depth, Some(no_light), Some([0.0, 0.0, 400.0, 1000.0])));
    let mut prev = 1.0f32;
    for s in [0.1f32, 0.25, 0.5, 0.8] {
        let on = atmos_render(&g, &t0, &atmos(near_depth, Some(no_light), Some([s, 0.0, 400.0, 1000.0])));
        let k = lum_of(px(&on, 20, 20)) / lum_of(px(&off, 20, 20));
        println!("contact strength {s}: luminance x{k:.3}");
        assert!(k < prev && k > 0.0, "more strength must darken more (x{k} after x{prev})");
        // linear light: the factor is 1 - strength; the tone remap in between is not linear, so allow a margin.
        assert!(k > (1.0 - s) * 0.8 - 0.02, "strength {s} darkened by more than the cap allows (x{k})");
        prev = k;
    }
    // Fully lit (visibility 1): nothing changes at any strength.
    let lit_px = atmos_render(&g, &t0, &atmos(near_depth, Some(lit), Some([1.0, 0.0, 400.0, 1000.0])));
    assert!(max_diff(&lit_px, &off) <= 1e-3, "visibility 1 must not darken");
}

#[test]
fn light_adjust_contact_is_released_on_sky_fog_water_and_lights() {
    let Some(g) = gpu() else { return };
    let b10 = Some([1.0, 0.0, 400.0, 1000.0]);
    let off = Some([0.0, 0.0, 400.0, 1000.0]);
    let ratio = |t0: &[[f32; 4]], depth: fn(usize, usize) -> f32, state_k: f32| {
        let mut a = atmos(depth, Some(no_light), b10);
        a.k = state_k;
        let on = atmos_render(&g, t0, &a);
        let mut o = atmos(depth, Some(no_light), off);
        o.k = state_k;
        let base = atmos_render(&g, t0, &o);
        lum_of(px(&on, 20, 20)) / lum_of(px(&base, 20, 20))
    };
    // Sanity: a mid-distance surface of another colour is darkened.
    let solid = ratio(&uniform(MID), near_depth, 1.0);
    assert!(solid < 0.8, "the reference surface must be darkened (x{solid})");
    // Sky: invalid depth.
    assert!(ratio(&uniform(MID), sky_depth, 1.0) > 0.999, "the sky must not be darkened");
    // The game's own fog: a far pixel of the fog colour.
    let fog_px = uniform([HAZE[0], HAZE[1], HAZE[2], 1.0]);
    assert!(ratio(&fog_px, far_depth, 1.0) > 0.99, "fog must not be darkened");
    // A bluish pixel (water): released even without a fog estimate.
    assert!(ratio(&uniform([0.25, 0.35, 0.60, 1.0]), near_depth, 0.0) > 0.99, "water must not be darkened");
    // A bright pixel (a light source): released.
    assert!(ratio(&uniform([1.2, 1.2, 1.2, 1.0]), near_depth, 1.0) > 0.99, "lights must not be darkened");
    // Without a depth (t4 unbound) the visibility cannot be used: nothing happens.
    let mut a = atmos(near_depth, Some(no_light), b10);
    a.bind_fog = false;
    let unb = atmos_render(&g, &uniform(MID), &a);
    let base = atmos_render(&g, &uniform(MID), &atmos(near_depth, Some(no_light), off));
    assert_eq!(unb, base, "without the depth bound the contact shadow must be a no-op");
}

#[test]
fn light_adjust_contact_upsample_does_not_leak_across_a_depth_edge() {
    let Some(g) = gpu() else { return };
    // Left half of the picture is near (10), right half far (100); the half-size visibility is 0 (shadow) on the left half
    // and 1 on the right. Pixels at the boundary on the far side must stay lit; the near side is shadowed.
    fn two_depths(x: usize, _: usize) -> f32 {
        if x < N / 2 { 0.01 } else { 0.1 }
    }
    fn two_halves(x: usize, _: usize) -> f32 {
        if x < N / 4 { 0.0 } else { 1.0 }
    }
    let t0 = uniform(MID);
    let on = atmos_render(&g, &t0, &atmos(two_depths, Some(two_halves), Some([1.0, 0.0, 400.0, 1000.0])));
    let off = atmos_render(&g, &t0, &atmos(two_depths, Some(two_halves), Some([0.0, 0.0, 400.0, 1000.0])));
    let k = |x: usize| lum_of(px(&on, x, 20)) / lum_of(px(&off, x, 20));
    for x in [N / 2, N / 2 + 1] {
        assert!(k(x) > 0.99, "far side pixel {x} was darkened by the near shadow (x{})", k(x));
    }
    for x in [N / 2 - 1, N / 2 - 2, 10] {
        assert!(k(x) < 0.8, "near side pixel {x} must be shadowed (x{})", k(x));
    }
}

#[test]
fn light_adjust_aerial_leaves_the_near_and_the_sky_alone_and_tints_the_far_within_the_cap() {
    let Some(g) = gpu() else { return };
    let t0 = uniform(MID);
    // 1000 m per 1.0 of the normalised depth: depth 0.01 = 10 m
    let on = |depth: fn(usize, usize) -> f32, strength: f32| atmos_render(&g, &t0, &atmos(depth, None, Some([0.0, strength, 400.0, UNIT_M])));
    fn at10(_: usize, _: usize) -> f32 {
        0.01
    }
    fn at15(_: usize, _: usize) -> f32 {
        0.015 / 1.5f32.sqrt() // 15 m RADIAL in the corners of the square test picture (ray factor up to sqrt(1.5))
    }
    fn at300(_: usize, _: usize) -> f32 {
        0.3
    }
    fn far_plane(_: usize, _: usize) -> f32 {
        1.0
    }
    fn almost_far_plane(_: usize, _: usize) -> f32 {
        0.985
    }
    let base = on(near_depth, 0.0);
    // Near: nothing within 15 m, bit for bit; the sky too (invalid depth, and the engine's real sky: the far plane 1.0).
    assert_eq!(on(at10, 1.0), on(at10, 0.0), "10 m must be untouched");
    assert_eq!(on(at15, 1.0), on(at15, 0.0), "15 m must be untouched");
    assert_eq!(on(sky_depth, 1.0), on(sky_depth, 0.0), "the sky must be untouched");
    assert_eq!(on(far_plane, 1.0), on(far_plane, 0.0), "the far-plane sky (1.0) must be untouched");
    assert_eq!(on(almost_far_plane, 1.0), on(almost_far_plane, 0.0), "depth >= 0.98 is the sky");
    // Far: tinted towards the haze colour. Reference: the same pixel fed through the shader with the haze mixed in by hand
    // (the share is min(strength * (1 - 2^(-(d - 15) / 400)), 0.35)).
    for (depth, strength) in [(at300 as fn(usize, usize) -> f32, 0.35f32), (far_depth, 1.0), (far_depth, 0.2)] {
        let z = depth(0, 0);
        let share = crate::aerial::blend_at(strength, 400.0, UNIT_M, z, ray20(), 1.0, 0.0, 0.0, 0.0);
        assert!(share > 0.0 && share <= 0.35 + 1e-6, "share {share}");
        // fog_amount of this pixel is 0 here because the scene colour is far from the haze colour
        let want = atmos_render(&g, &uniform(haze_mixed(MID, share)), &atmos(near_depth, None, Some([0.0, 0.0, 400.0, UNIT_M])));
        let got = on(depth, strength);
        println!("depth {z} strength {strength}: share {share:.3}, got {:?}, want {:?}", px(&got, 20, 20), px(&want, 20, 20));
        // Compared at the pixel the share was computed for: the radial distance differs across the picture.
        let d20 = max_diff(&[px(&got, 20, 20)], &[px(&want, 20, 20)]);
        assert!(d20 <= 0.01, "the haze must be the rule's lerp toward the haze colour (diff {d20})");
        assert!(max_diff(&got, &base) > 0.003, "the far pixel must change");
    }
    // The cap: even at strength 1 and 900 m the haze share stays at 35 %.
    let share = crate::aerial::blend(1.0, 50.0, UNIT_M, ATMOS_ZREF, 1.0, 0.0, 0.0, 0.0);
    assert!(share <= 0.35 + 1e-6);
}

#[test]
fn light_adjust_aerial_is_released_in_dense_fog_water_and_without_a_fog_colour_and_keeps_bright_pixels() {
    let Some(g) = gpu() else { return };
    let b10 = Some([0.0, 1.0, 400.0, UNIT_M]);
    let off = Some([0.0, 0.0, 400.0, UNIT_M]);
    let diff_for = |t0: &[[f32; 4]], k: f32| {
        let mut a = atmos(far_depth, None, b10);
        a.k = k;
        let mut o = atmos(far_depth, None, off);
        o.k = k;
        max_diff(&atmos_render(&g, t0, &a), &atmos_render(&g, t0, &o))
    };
    assert!(diff_for(&uniform(MID), 1.0) > 0.003, "sanity: the far surface is tinted");
    // The game's own dense fog: the pixel is the fog colour already (and sits at the farthest depth): untouched.
    assert!(diff_for(&uniform([HAZE[0], HAZE[1], HAZE[2], 1.0]), 1.0) <= 1e-3, "dense fog must not be hazed again");
    // Fog colour unknown (confidence 0): nothing to take the colour from.
    assert_eq!(diff_for(&uniform(MID), 0.0), 0.0, "an unknown fog colour must leave the picture alone");
    // Water: bluish pixel.
    assert!(diff_for(&uniform([0.25, 0.35, 0.60, 1.0]), 1.0) <= 1e-3, "water must not be hazed");
    // t3 / t4 unbound: no-op.
    let mut a = atmos(far_depth, None, b10);
    a.bind_fog = false;
    let mut o = atmos(far_depth, None, off);
    o.bind_fog = false;
    assert_eq!(atmos_render(&g, &uniform(MID), &a), atmos_render(&g, &uniform(MID), &o));
    // A bright pixel (3x SDR white in linear light) gets a third of the share: it keeps its energy.
    let bright = [1.7f32, 1.7, 1.7, 1.0];
    let m = bright[0] * bright[0];
    let share = crate::aerial::blend_at(1.0, 400.0, UNIT_M, ATMOS_ZREF, ray20(), 1.0, 0.0, 0.0, m);
    let full = crate::aerial::blend_at(1.0, 400.0, UNIT_M, ATMOS_ZREF, ray20(), 1.0, 0.0, 0.0, 0.0);
    assert!((share - full / m).abs() < 1e-6 && m > 2.5);
    let t0 = uniform(bright);
    let want = atmos_render(&g, &uniform(haze_mixed(bright, share)), &atmos(near_depth, None, off));
    let got = atmos_render(&g, &t0, &atmos(far_depth, None, b10));
    let d20 = max_diff(&[px(&got, 20, 20)], &[px(&want, 20, 20)]);
    assert!(d20 <= 0.02 * px(&want, 20, 20)[1].max(1.0), "bright pixels must get the reduced share (diff {d20})");
    let hazed_lum = lum_of(px(&got, 20, 20));
    let plain_lum = lum_of(px(&atmos_render(&g, &t0, &atmos(far_depth, None, off)), 20, 20));
    println!("bright pixel: {plain_lum} -> {hazed_lum}");
    assert!(hazed_lum > 0.8 * plain_lum, "a light source must keep its energy ({plain_lum} -> {hazed_lum})");
}

#[test]
fn light_adjust_contact_and_aerial_never_make_nan_or_negative_values() {
    let Some(g) = gpu() else { return };
    fn mixed_depth(x: usize, y: usize) -> f32 {
        match (x + 3 * y) % 7 {
            0 => 0.0,
            1 => f32::NAN,
            2 => f32::INFINITY,
            3 => -4.0,
            4 => 1e38,
            5 => 1.0, // the far plane (the engine's sky)
            _ => 0.02 + (x * y) as f32 * 1e-3,
        }
    }
    fn weird_vis(x: usize, y: usize) -> f32 {
        match (x + y) % 3 {
            0 => 0.0,
            1 => -2.0,
            _ => 3.0,
        }
    }
    let t0 = random_texels(9, -1.0, 3.0);
    for k in [0.0f32, 1.0] {
        let mut a = atmos(mixed_depth, Some(weird_vis), Some([1.0, 1.0, 50.0, UNIT_M]));
        a.k = k;
        let out = atmos_render(&g, &t0, &a);
        assert!(finite_nonneg(&out), "NaN or negative output (k {k})");
    }
}

// ---- aerial perspective with the REAL fog estimate (fog.rs texel 2): the haze must not depend on the camera pitch ----------
// Seen in the game: haze across the valley, none at all with the camera tilted down at the same place. The haze colour was the
// colour of the farthest pixels of the frame: with no sky in view those are the far trees themselves.

const AER_SKY: [f32; 3] = [0.62, 0.70, 0.80];
const AER_TREES: [f32; 3] = [0.30, 0.42, 0.22];
const AER_GROUND: [f32; 3] = [0.45, 0.38, 0.25];
const AER_TREE_Z: f32 = 0.19; // a no-sky capture: largest depth 0.193

/// Looking across the valley: sky (the far plane) in rows 0..12, far trees rows 12..36, near ground below.
fn aer_valley(_x: usize, y: usize) -> ([f32; 3], f32) {
    if y < 12 { (AER_SKY, 1.0) } else if y < 36 { (AER_TREES, AER_TREE_Z) } else { (AER_GROUND, 0.004 + (N - y) as f32 * 0.0005) }
}
/// Tilted down at the same place: no sky; far trees rows 0..36 (same pixels as above from row 12), the slope below.
fn aer_slope(x: usize, y: usize) -> ([f32; 3], f32) {
    if y < 36 { (AER_TREES, AER_TREE_Z) } else { aer_valley(x, y) }
}
/// Tilted down onto a ground plane: depth grows toward the top of the picture (1 / depth linear in the row), no sky.
fn aer_plane(_x: usize, y: usize) -> ([f32; 3], f32) {
    (AER_GROUND, 1.0 / (2.0 + y as f32 * 0.6)) // 500 m at the top row, 25 m at the bottom
}

struct AerWorld {
    _scene: ID3D11Texture2D,
    srv: ID3D11ShaderResourceView,
    t0: Vec<[f32; 4]>,
    depth: ID3D11Texture2D,
}

fn aer_world(g: &Gpu, f: fn(usize, usize) -> ([f32; 3], f32)) -> AerWorld {
    let t0: Vec<[f32; 4]> = (0..N * N).map(|i| { let c = f(i % N, i / N).0; [c[0], c[1], c[2], 1.0] }).collect();
    let bytes: Vec<u8> = t0.iter().flat_map(|p| p.iter().flat_map(|c| c.to_le_bytes())).collect();
    let dbytes: Vec<u8> = (0..N * N).flat_map(|i| f(i % N, i / N).1.to_le_bytes()).collect();
    unsafe {
        let st = tex2d(g, N as u32, N as u32, DXGI_FORMAT_R32G32B32A32_FLOAT, D3D11_BIND_SHADER_RESOURCE, D3D11_USAGE_IMMUTABLE, 0, Some((&bytes, (N * 16) as u32)));
        let depth = tex2d(g, N as u32, N as u32, DXGI_FORMAT_R32_FLOAT, D3D11_BIND_SHADER_RESOURCE, D3D11_USAGE_IMMUTABLE, 0, Some((&dbytes, (N * 4) as u32)));
        let mut srv = None;
        g.dev.CreateShaderResourceView(&st, None, Some(&mut srv)).unwrap();
        AerWorld { _scene: st, srv: srv.unwrap(), t0, depth }
    }
}

/// One frame: the estimator runs on `w`, then the light-adjust substitute with the aerial perspective at `strength`.
fn aer_frame(g: &Gpu, fog: &mut crate::fog::Fog, w: &AerWorld, strength: f32) -> Vec<[f32; 4]> {
    unsafe {
        assert!(fog.run(g.dev.as_raw(), g.ctx.as_raw(), w.srv.as_raw(), w.depth.as_raw()), "Fog::run failed");
        aer_render(g, fog, w, strength)
    }
}

/// The light-adjust substitute on `w` with the CURRENT fog state (no estimator run).
fn aer_render(g: &Gpu, fog: &crate::fog::Fog, w: &AerWorld, strength: f32) -> Vec<[f32; 4]> {
    unsafe {
        let state = ID3D11ShaderResourceView::from_raw_borrowed(&fog.state_srv()).unwrap().clone();
        let depth = ID3D11ShaderResourceView::from_raw_borrowed(&fog.depth_view()).unwrap().clone();
        let b: Vec<u8> = [0.0f32, strength, 400.0, UNIT_M].iter().flat_map(|x| x.to_le_bytes()).collect();
        let d = D3D11_BUFFER_DESC { ByteWidth: 16, Usage: D3D11_USAGE_IMMUTABLE, BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32, ..Default::default() };
        let sd = D3D11_SUBRESOURCE_DATA { pSysMem: b.as_ptr() as *const c_void, SysMemPitch: 0, SysMemSlicePitch: 0 };
        let mut cb10 = None;
        g.dev.CreateBuffer(&d, Some(&sd), Some(&mut cb10)).unwrap();
        let hook = |c: &ID3D11DeviceContext| {
            c.PSSetShaderResources(3, Some(&[Some(state.clone()), Some(depth.clone())]));
            c.PSSetConstantBuffers(10, Some(&[cb10.clone()]));
        };
        let out = render_hook(g, VS_LIGHT, light_ps(), &Inputs { t0: &w.t0, t1: Some(LIGHT_T1), cb: &LIGHT_CB }, Some((1.0, [1.0, 0.0, 0.0, 0.0])), None, &hook);
        g.ctx.PSSetShaderResources(3, Some(&[None, None]));
        g.ctx.PSSetConstantBuffers(10, Some(&[None]));
        out
    }
}

#[test]
fn light_adjust_aerial_gives_the_same_haze_with_and_without_sky_in_view() {
    let Some(g) = gpu() else { return };
    let blobs = crate::fog::compile_blobs().expect("fog.hlsl compiles");
    let mut fog = unsafe { crate::fog::Fog::new(g.dev.as_raw(), &blobs) }.expect("Fog::new");
    let valley = aer_world(&g, aer_valley);
    let slope = aer_world(&g, aer_slope);
    for _ in 0..4 {
        aer_frame(&g, &mut fog, &valley, 1.0);
    }
    let v_on = aer_render(&g, &fog, &valley, 1.0);
    let v_off = aer_render(&g, &fog, &valley, 0.0);
    // The camera tilts down: the sky leaves the picture.
    let mut s_on = Vec::new();
    for _ in 0..30 {
        s_on = aer_frame(&g, &mut fog, &slope, 1.0);
    }
    let s_off = aer_render(&g, &fog, &slope, 0.0);
    for (x, y) in [(32usize, 20usize), (10, 30), (55, 14)] {
        let hv = px(&v_on, x, y);
        let hs = px(&s_on, x, y);
        let effect = max_diff(&[hv], &[px(&v_off, x, y)]);
        let effect_down = max_diff(&[hs], &[px(&s_off, x, y)]);
        println!("far trees ({x}, {y}): with sky {hv:?} (haze {effect:.3}), tilted down {hs:?} (haze {effect_down:.3})");
        assert!(effect > 0.03, "sanity: the far trees are hazed with the sky in view ({effect})");
        assert!(effect_down > 0.03, "the far trees must keep their haze with no sky in view ({effect_down})");
        assert!(max_diff(&[hv], &[hs]) <= 0.01, "the same far trees must get the same haze with and without the sky ({hv:?} vs {hs:?})");
    }
    // The near slope is untouched either way.
    assert!(max_diff(&[px(&s_on, 32, 60)], &[px(&s_off, 32, 60)]) <= 1e-6);
}

#[test]
fn light_adjust_aerial_tilted_down_is_proportional_to_the_radial_distance_and_stable() {
    let Some(g) = gpu() else { return };
    let blobs = crate::fog::compile_blobs().expect("fog.hlsl compiles");
    let mut fog = unsafe { crate::fog::Fog::new(g.dev.as_raw(), &blobs) }.expect("Fog::new");
    let valley = aer_world(&g, aer_valley);
    let plane = aer_world(&g, aer_plane);
    for _ in 0..4 {
        aer_frame(&g, &mut fog, &valley, 1.0);
    }
    let mut frames = Vec::new();
    for _ in 0..12 {
        frames.push(aer_frame(&g, &mut fog, &plane, 1.0));
    }
    // Frame to frame: the held haze colour does not move (no flicker, no fade).
    for w in frames.windows(2) {
        assert!(max_diff(&w[0], &w[1]) <= 2e-3, "the haze changed between two identical frames ({})", max_diff(&w[0], &w[1]));
    }
    let on = frames.last().unwrap();
    // At several rows of the plane the output must be the rule's share (radial distance) mixed toward the held sky colour.
    let mut prev = 0.0f32;
    for (x, y) in [(32usize, 50usize), (32, 30), (32, 10), (2, 10)] {
        let z = aer_plane(x, y).1;
        let ray = crate::aerial::ray_factor((x as f32 + 0.5) / N as f32, (y as f32 + 0.5) / N as f32, 1.0);
        let share = crate::aerial::blend_at(1.0, 400.0, UNIT_M, z, ray, 1.0, 0.0, 0.0, 0.0);
        let g0 = [AER_GROUND[0], AER_GROUND[1], AER_GROUND[2], 1.0];
        let ch = |i: usize| ((1.0 - share) * g0[i] * g0[i] + share * AER_SKY[i] * AER_SKY[i]).sqrt();
        let want_t0 = uniform([ch(0), ch(1), ch(2), 1.0]);
        let want = atmos_render(&g, &want_t0, &atmos(near_depth, None, Some([0.0, 0.0, 400.0, UNIT_M])));
        let d = max_diff(&[px(on, x, y)], &[px(&want, x, y)]);
        println!("plane ({x}, {y}): depth {z:.4} ray {ray:.3} = {:.0} m, share {share:.3}, diff {d:.4}", z * ray * UNIT_M);
        assert!(d <= 0.01, "({x}, {y}): the haze must follow the radial distance (diff {d})");
        if x == 32 {
            assert!(share > prev, "farther up the picture = farther away = more haze");
            prev = share;
        }
    }
    // The edge pixel of the same row is farther (radial) than the centre one: more haze.
    let r_edge = crate::aerial::ray_factor(2.5 / N as f32, 10.5 / N as f32, 1.0);
    assert!(r_edge > crate::aerial::ray_factor(0.5, 10.5 / N as f32, 1.0));
}


// ---- rim light and subsurface glow (t23 / b11, docs/rim.md) -------------------------------------------------------------

/// Identity remap (t1.yzw = dark 0, bright 0, mid 1): the output of the substitute is sqrt(linear light) + a +-0.002 dither.
const IDENT_T1: [f32; 4] = [0.0, 0.0, 0.0, 1.0];

fn light_ps() -> &'static Vec<u8> {
    static PS: OnceLock<Vec<u8>> = OnceLock::new();
    PS.get_or_init(|| compile_ps(LIGHT_ADJUST_HLSL, LIGHT_ADJUST_ENTRY).unwrap_or_else(|e| panic!("{e}")))
}

/// What one rim render is given. `mask` = the R8 rim mask at t23 (None = unbound), `b11` = the rim constants (None = unbound = zero),
/// `depth` = the linear depth at t4 (None = unbound), `fog` = (fog colour encoded, confidence, zref) at t3 (None = unbound).
struct RimIn<'a> {
    t0: &'a [[f32; 4]],
    mask: Option<&'a [u8]>,
    b11: Option<[[f32; 4]; 2]>,
    depth: Option<fn(usize, usize) -> f32>,
    fog: Option<([f32; 3], f32, f32)>,
}

fn rim_render(g: &Gpu, i: &RimIn) -> Vec<[f32; 4]> {
    rim_render_fur(g, i, None)
}

/// `rim_render` with the fur mask at t20 (R16G16B16A16_UNORM texels: r g = uv, b = a fur draw with uv, a = fur weight).
fn rim_render_fur(g: &Gpu, i: &RimIn, fur: Option<&[[u16; 4]]>) -> Vec<[f32; 4]> {
    unsafe {
        let fur_srv = fur.map(|f| {
            assert_eq!(f.len(), N * N);
            let b: Vec<u8> = f.iter().flat_map(|t| t.iter().flat_map(|v| v.to_le_bytes())).collect();
            let t = tex2d(g, N as u32, N as u32, DXGI_FORMAT_R16G16B16A16_UNORM, D3D11_BIND_SHADER_RESOURCE, D3D11_USAGE_IMMUTABLE, 0, Some((&b, (N * 8) as u32)));
            let mut s = None;
            g.dev.CreateShaderResourceView(&t, None, Some(&mut s)).unwrap();
            s.unwrap()
        });
        let mask_srv = i.mask.map(|m| {
            assert_eq!(m.len(), N * N);
            let t = tex2d(g, N as u32, N as u32, DXGI_FORMAT_R8_UNORM, D3D11_BIND_SHADER_RESOURCE, D3D11_USAGE_IMMUTABLE, 0, Some((m, N as u32)));
            let mut s = None;
            g.dev.CreateShaderResourceView(&t, None, Some(&mut s)).unwrap();
            s.unwrap()
        });
        let cb11 = i.b11.map(|v| {
            let b: Vec<u8> = v.iter().flat_map(|p| p.iter().flat_map(|x| x.to_le_bytes())).collect();
            let d = D3D11_BUFFER_DESC { ByteWidth: 32, Usage: D3D11_USAGE_IMMUTABLE, BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32, ..Default::default() };
            let sd = D3D11_SUBRESOURCE_DATA { pSysMem: b.as_ptr() as *const c_void, SysMemPitch: 0, SysMemSlicePitch: 0 };
            let mut cb = None;
            g.dev.CreateBuffer(&d, Some(&sd), Some(&mut cb)).unwrap();
            cb.unwrap()
        });
        let depth_srv = i.depth.map(|f| {
            let db: Vec<u8> = (0..N * N).flat_map(|k| f(k % N, k / N).to_le_bytes()).collect();
            let t = tex2d(g, N as u32, N as u32, DXGI_FORMAT_R32_FLOAT, D3D11_BIND_SHADER_RESOURCE, D3D11_USAGE_IMMUTABLE, 0, Some((&db, (N * 4) as u32)));
            let mut s = None;
            g.dev.CreateShaderResourceView(&t, None, Some(&mut s)).unwrap();
            s.unwrap()
        });
        let fog_srv = i.fog.map(|(f, k, zref)| {
            let st: Vec<u8> = [f[0], f[1], f[2], k, zref, 0.0, 0.0, 0.0].iter().flat_map(|v| v.to_le_bytes()).collect();
            let t = tex2d(g, 2, 1, DXGI_FORMAT_R32G32B32A32_FLOAT, D3D11_BIND_SHADER_RESOURCE, D3D11_USAGE_IMMUTABLE, 0, Some((&st, 32)));
            let mut s = None;
            g.dev.CreateShaderResourceView(&t, None, Some(&mut s)).unwrap();
            s.unwrap()
        });
        let hook = |c: &ID3D11DeviceContext| {
            if depth_srv.is_some() || fog_srv.is_some() {
                c.PSSetShaderResources(3, Some(&[fog_srv.clone(), depth_srv.clone()]));
            }
            if mask_srv.is_some() {
                c.PSSetShaderResources(23, Some(&[mask_srv.clone()]));
            }
            if fur_srv.is_some() {
                c.PSSetShaderResources(20, Some(&[fur_srv.clone()]));
            }
            if cb11.is_some() {
                c.PSSetConstantBuffers(11, Some(&[cb11.clone()]));
            }
        };
        let out = render_hook(g, VS_LIGHT, light_ps(), &Inputs { t0: i.t0, t1: Some(IDENT_T1), cb: &LIGHT_CB }, Some((1.0, [1.0, 0.0, 0.0, 0.0])), None, &hook);
        g.ctx.PSSetShaderResources(20, Some(&[None]));
        g.ctx.PSSetShaderResources(23, Some(&[None]));
        g.ctx.PSSetConstantBuffers(11, Some(&[None]));
        out
    }
}

/// The character block (the rim mask) and its distance to the silhouette.
const BX: std::ops::Range<usize> = 20..44;
const BY: std::ops::Range<usize> = 16..48;

fn in_block(x: usize, y: usize) -> bool {
    BX.contains(&x) && BY.contains(&y)
}

fn block_mask() -> Vec<u8> {
    (0..N * N).map(|k| if in_block(k % N, k / N) { 255 } else { 0 }).collect()
}

/// Distance in px from a block pixel to the nearest pixel outside it (1 = at the edge).
fn edge_d(x: usize, y: usize) -> usize {
    (x - BX.start).min(BX.end - 1 - x).min(y - BY.start).min(BY.end - 1 - y) + 1
}

/// A scene with the character colour `c` (encoded) in the block and `bg` (encoded) around it.
fn block_scene(c: [f32; 3], bg: [f32; 3]) -> Vec<[f32; 4]> {
    (0..N * N).map(|k| if in_block(k % N, k / N) { [c[0], c[1], c[2], 1.0] } else { [bg[0], bg[1], bg[2], 1.0] }).collect()
}

const SKY: [f32; 3] = [0.8, 0.85, 0.95];
const SKIN: [f32; 3] = [0.55, 0.45, 0.35];

fn b11(strength: f32, width: f32, sss: f32) -> Option<[[f32; 4]; 2]> {
    Some([[strength, width, sss, 0.0], [0.0, 0.0, 0.0, 0.0]])
}

fn rim_in<'a>(t0: &'a [[f32; 4]], mask: Option<&'a [u8]>, b: Option<[[f32; 4]; 2]>) -> RimIn<'a> {
    RimIn { t0, mask, b11: b, depth: None, fog: None }
}

fn lin3(p: [f32; 4]) -> [f32; 3] {
    [p[0] * p[0], p[1] * p[1], p[2] * p[2]]
}

fn lum3(c: [f32; 3]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

/// Relative gain of the linear luminance of `out` over `plain` at a pixel.
fn lum_gain(out: &[[f32; 4]], plain: &[[f32; 4]], x: usize, y: usize) -> f32 {
    lum3(lin3(px(out, x, y))) / lum3(lin3(px(plain, x, y))).max(1e-9) - 1.0
}

#[test]
fn rim_is_a_bit_identical_no_op_at_strength_zero_with_mask_zero_and_unbound() {
    let Some(g) = gpu() else { return };
    let t0 = block_scene(SKIN, SKY);
    let m = block_mask();
    let plain = rim_render(&g, &rim_in(&t0, None, None));
    // Strength 0 with a full mask and a non-zero width / subsurface: the branch is skipped.
    let zero = rim_render(&g, &rim_in(&t0, Some(&m), b11(0.0, 4.0, 1.0)));
    assert_eq!(max_diff(&plain, &zero), 0.0, "rim 0 must be bit-identical");
    // Mask 0 everywhere, full strength: nothing is a character pixel.
    let none = vec![0u8; N * N];
    let empty = rim_render(&g, &rim_in(&t0, Some(&none), b11(1.0, 4.0, 1.0)));
    assert_eq!(max_diff(&plain, &empty), 0.0, "mask 0 must be bit-identical");
    // t23 unbound with full strength.
    let unbound = rim_render(&g, &rim_in(&t0, None, b11(1.0, 4.0, 1.0)));
    assert_eq!(max_diff(&plain, &unbound), 0.0, "an unbound rim mask must be bit-identical");
    // And the pixels outside the mask never change, whatever the strength.
    let on = rim_render(&g, &rim_in(&t0, Some(&m), b11(1.0, 4.0, 1.0)));
    for y in 0..N {
        for x in 0..N {
            if !in_block(x, y) {
                assert_eq!(px(&on, x, y), px(&plain, x, y), "pixel ({x},{y}) outside the mask changed");
            }
        }
    }
}

#[test]
fn rim_acts_only_within_the_width_of_the_mask_edge() {
    let Some(g) = gpu() else { return };
    let t0 = block_scene(SKIN, SKY);
    let m = block_mask();
    let plain = rim_render(&g, &rim_in(&t0, None, None));
    for w in [1.0f32, 3.0, 6.0] {
        // Subsurface 0: only the rim.
        let on = rim_render(&g, &rim_in(&t0, Some(&m), b11(1.0, w, 0.0)));
        let mut gains = Vec::new();
        for x in BX.start..BX.start + 12 {
            let d = edge_d(x, 32);
            let gain = lum_gain(&on, &plain, x, 32);
            gains.push((d, gain));
            if d as f32 > w {
                assert_eq!(px(&on, x, 32), px(&plain, x, 32), "width {w}: the pixel {d} px inside the edge must not change (gain {gain})");
            }
        }
        println!("rim width {w}: gain by distance {gains:.3?}");
        assert!(gains[0].1 > 0.03, "width {w}: the edge pixel must get a visible rim, gain {}", gains[0].1);
        for p in gains.windows(2) {
            assert!(p[1].1 <= p[0].1 + 0.006, "width {w}: the rim must fall off towards the inside: {gains:?}");
        }
        // The deep interior and the far corner of the block never change.
        assert_eq!(px(&on, 32, 32), px(&plain, 32, 32));
    }
}

#[test]
fn rim_gain_is_bounded_at_25_percent_and_the_subsurface_at_8() {
    let Some(g) = gpu() else { return };
    let m = block_mask();
    let colours: [[f32; 3]; 5] = [[0.55, 0.45, 0.35], [0.3, 0.3, 0.3], [0.9, 0.9, 0.9], [0.2, 0.5, 0.2], [0.55, 0.35, 0.45]];
    for c in colours {
        let t0 = block_scene(c, SKY);
        let plain = rim_render(&g, &rim_in(&t0, None, None));
        let both = rim_render(&g, &rim_in(&t0, Some(&m), b11(1.0, 8.0, 1.0)));
        let mut worst = 0.0f32;
        for y in 0..N {
            for x in 0..N {
                let gain = lum_gain(&both, &plain, x, y);
                worst = worst.max(gain);
                assert!(gain <= 0.25 + 0.02, "colour {c:?} pixel ({x},{y}): luminance gain {gain} above +25 %");
                assert!(gain >= -0.02, "the rim must never darken: {gain}");
            }
        }
        println!("rim colour {c:?}: largest luminance gain {worst:.3}");
        assert!(worst > 0.03, "colour {c:?}: no visible lift at all ({worst})");
        // The subsurface alone (a dark background: no rim) is at most +8 %.
        let dark_bg = block_scene(c, [0.05, 0.05, 0.05]);
        let plain_d = rim_render(&g, &rim_in(&dark_bg, None, None));
        let sss = rim_render(&g, &rim_in(&dark_bg, Some(&m), b11(1.0, 8.0, 1.0)));
        for y in BY {
            for x in BX {
                let gain = lum_gain(&sss, &plain_d, x, y);
                assert!(gain <= 0.08 + 0.02, "colour {c:?} pixel ({x},{y}): subsurface gain {gain} above +8 %");
            }
        }
    }
}

#[test]
fn rim_never_pushes_a_channel_above_the_peak_or_the_pixels_own_maximum() {
    let Some(g) = gpu() else { return };
    let m = block_mask();
    // Linear 0.81 (encoded 0.9): may rise at most to SDR white; linear 1.44 (encoded 1.2) is above it: untouched.
    for (c, name) in [([0.9f32, 0.9, 0.9], "near white"), ([1.2, 1.2, 1.2], "over white"), ([1.6, 0.9, 0.5], "hot")] {
        let t0 = block_scene(c, SKY);
        let plain = rim_render(&g, &rim_in(&t0, None, None));
        let on = rim_render(&g, &rim_in(&t0, Some(&m), b11(1.0, 8.0, 1.0)));
        for y in BY {
            for x in BX {
                let (a, b) = (lin3(px(&plain, x, y)), lin3(px(&on, x, y)));
                let (ma, mb) = (a[0].max(a[1]).max(a[2]), b[0].max(b[1]).max(b[2]));
                assert!(mb <= ma.max(1.0) + 0.01, "{name} ({x},{y}): the largest channel went from {ma} to {mb}");
            }
        }
    }
}

#[test]
fn rim_preserves_the_hue() {
    let Some(g) = gpu() else { return };
    let m = block_mask();
    // An orange character against a NEUTRAL bright background: the tint towards the background is only 35 % of an added light that is at
    // most 25 % of the pixel, so the chromaticity (channel / sum of channels) moves by a couple of hundredths at most and the order of
    // the channels never changes. (Ratios of the weakest channel are the wrong yardstick: the blue of an orange is tiny.)
    let t0 = block_scene([0.7, 0.45, 0.25], [0.85, 0.85, 0.85]);
    let plain = rim_render(&g, &rim_in(&t0, None, None));
    let on = rim_render(&g, &rim_in(&t0, Some(&m), b11(1.0, 4.0, 0.0)));
    let mut worst = 0.0f32;
    for y in BY {
        for x in BX {
            let (a, b) = (lin3(px(&plain, x, y)), lin3(px(&on, x, y)));
            let (sa, sb) = (a[0] + a[1] + a[2], b[0] + b[1] + b[2]);
            for i in 0..3 {
                worst = worst.max((b[i] / sb - a[i] / sa).abs());
            }
            assert!(b[0] > b[1] && b[1] > b[2], "the channel order changed at ({x},{y}): {a:?} -> {b:?}");
        }
    }
    println!("rim hue: largest chromaticity change {worst:.3}");
    assert!(worst < 0.03, "the rim changed the hue by {worst}");
    assert!(lum_gain(&on, &plain, BX.start, 32) > 0.03);
}

#[test]
fn rim_gives_no_lift_on_black_and_almost_none_in_the_dark() {
    let Some(g) = gpu() else { return };
    let m = block_mask();
    for c in [[0.0f32; 3], [0.03; 3], [0.05; 3]] {
        let t0 = block_scene(c, SKY);
        let plain = rim_render(&g, &rim_in(&t0, None, None));
        let on = rim_render(&g, &rim_in(&t0, Some(&m), b11(1.0, 8.0, 1.0)));
        assert_eq!(max_diff(&plain, &on), 0.0, "encoded {c:?} (linear {}): a very dark pixel must not be lifted at all", c[0] * c[0]);
    }
    // A dim pixel (linear 0.01): the absolute lift stays tiny.
    let t0 = block_scene([0.1; 3], SKY);
    let plain = rim_render(&g, &rim_in(&t0, None, None));
    let on = rim_render(&g, &rim_in(&t0, Some(&m), b11(1.0, 8.0, 1.0)));
    for y in BY {
        for x in BX {
            let d = lum3(lin3(px(&on, x, y))) - lum3(lin3(px(&plain, x, y)));
            assert!(d < 0.001, "a dim pixel gained {d} linear");
        }
    }
    // A dark BACKGROUND (a night scene): no rim even on a lit character; and no lift at all with a black one.
    let lit = block_scene(SKIN, [0.1, 0.1, 0.1]);
    let plain = rim_render(&g, &rim_in(&lit, None, None));
    let on = rim_render(&g, &rim_in(&lit, Some(&m), b11(1.0, 4.0, 0.0)));
    assert_eq!(max_diff(&plain, &on), 0.0, "against a dark background the rim must vanish");
}

#[test]
fn rim_is_released_in_water_and_in_fog_and_applies_without_them() {
    let Some(g) = gpu() else { return };
    let m = block_mask();
    // Water: a bluish pixel (blue clearly above the mean of red and green) is released like the AO.
    let water = block_scene([0.2, 0.4, 0.7], SKY);
    let plain = rim_render(&g, &rim_in(&water, None, None));
    let on = rim_render(&g, &rim_in(&water, Some(&m), b11(1.0, 4.0, 1.0)));
    assert_eq!(max_diff(&plain, &on), 0.0, "bluish (water) pixels must be released");
    // A neutral pixel of the same brightness is not.
    let grey = block_scene([0.45; 3], SKY);
    let plain = rim_render(&g, &rim_in(&grey, None, None));
    let on = rim_render(&g, &rim_in(&grey, Some(&m), b11(1.0, 4.0, 1.0)));
    assert!(lum_gain(&on, &plain, BX.start, 32) > 0.03, "a neutral pixel must get the rim");
    // Fog: the character is the colour of the fog and far away: released. Without the fog state (or when it is not confident) it is not.
    let fog_c = [0.62, 0.62, 0.62];
    let foggy = block_scene(fog_c, SKY);
    fn far(_: usize, _: usize) -> f32 {
        3000.0
    }
    let with_fog = |conf: f32, bound: bool, masked: bool| {
        let i = RimIn { t0: &foggy, mask: if masked { Some(&m) } else { None }, b11: if masked { b11(1.0, 4.0, 1.0) } else { None }, depth: Some(far), fog: if bound { Some((fog_c, conf, 5000.0)) } else { None } };
        rim_render(&g, &i)
    };
    let plain = with_fog(1.0, true, false);
    let released = with_fog(1.0, true, true);
    assert_eq!(max_diff(&plain, &released), 0.0, "inside the game's own fog the rim must be released");
    let no_state = with_fog(1.0, false, true);
    let plain_ns = with_fog(1.0, false, false);
    assert!(lum_gain(&no_state, &plain_ns, BX.start, 32) > 0.02, "without a fog estimate the rim applies");
    let unsure = with_fog(0.0, true, true);
    let plain_u = with_fog(0.0, true, false);
    assert!(lum_gain(&unsure, &plain_u, BX.start, 32) > 0.02, "a zero-confidence fog estimate releases nothing");
}

#[test]
fn rim_tints_towards_the_background_it_samples_outside_the_mask() {
    let Some(g) = gpu() else { return };
    let m = block_mask();
    // Red background on the left half of the picture, blue on the right; a grey character spanning both.
    let red = [0.9f32, 0.5, 0.4];
    let blue = [0.4f32, 0.5, 0.9];
    let t0: Vec<[f32; 4]> = (0..N * N)
        .map(|k| {
            let (x, y) = (k % N, k / N);
            if in_block(x, y) { [0.5, 0.5, 0.5, 1.0] } else if x < 32 { [red[0], red[1], red[2], 1.0] } else { [blue[0], blue[1], blue[2], 1.0] }
        })
        .collect();
    let plain = rim_render(&g, &rim_in(&t0, None, None));
    let on = rim_render(&g, &rim_in(&t0, Some(&m), b11(1.0, 3.0, 0.0)));
    let rb = |p: [f32; 4]| {
        let l = lin3(p);
        l[0] / l[2]
    };
    let left = rb(px(&on, BX.start, 32)) / rb(px(&plain, BX.start, 32));
    let right = rb(px(&on, BX.end - 1, 32)) / rb(px(&plain, BX.end - 1, 32));
    println!("rim tint: red/blue ratio change at the left edge {left:.3}, at the right edge {right:.3}");
    assert!(left > 1.01, "the edge next to the red background must turn redder ({left})");
    assert!(right < 0.99, "the edge next to the blue background must turn bluer ({right})");
    // The samples are taken OUTSIDE the mask only: a bright bar INSIDE the block (6 px from the edge) must not tint the rim.
    let mut t1 = block_scene([0.5; 3], [0.5, 0.5, 0.5]);
    for y in BY {
        for x in BX.start + 6..BX.start + 7 {
            t1[y * N + x] = [1.0, 0.1, 0.1, 1.0];
        }
    }
    for k in 0..N * N {
        if !in_block(k % N, k / N) {
            t1[k] = [0.8, 0.8, 0.8, 1.0];
        }
    }
    let p1 = rim_render(&g, &rim_in(&t1, None, None));
    let o1 = rim_render(&g, &rim_in(&t1, Some(&m), b11(1.0, 3.0, 0.0)));
    let ratio = rb(px(&o1, BX.start, 32)) / rb(px(&p1, BX.start, 32));
    assert!((ratio - 1.0).abs() < 0.02, "a bar inside the mask tinted the rim ({ratio})");
}

#[test]
fn rim_follows_the_grazing_term_from_the_depth() {
    let Some(g) = gpu() else { return };
    let m = block_mask();
    let t0 = block_scene(SKIN, SKY);
    let plain = rim_render(&g, &rim_in(&t0, None, None));
    let k = 2.0 * 0.5 / N as f32; // 2 tan(half fov) / height, tan = 0.5
    let bb = Some([[1.0, 3.0, 0.0, 0.0], [k, 0.0, 0.0, 0.0]]);
    fn flat(_: usize, _: usize) -> f32 {
        50.0
    }
    fn slanted(x: usize, _: usize) -> f32 {
        50.0 + 0.5 * x as f32
    }
    let run = |d: fn(usize, usize) -> f32| rim_render(&g, &RimIn { t0: &t0, mask: Some(&m), b11: bb, depth: Some(d), fog: None });
    let facing = lum_gain(&run(flat), &plain, BX.start, 32);
    let grazing = lum_gain(&run(slanted), &plain, BX.start, 32);
    println!("rim grazing: facing {facing:.3}, slanted {grazing:.3}");
    assert!(facing > 0.02, "a facing surface still gets a rim ({facing})");
    assert!(grazing > facing * 1.25, "a grazing surface must get a stronger rim ({grazing} vs {facing})");
}

#[test]
fn rim_treats_a_depth_step_as_a_silhouette_inside_the_mask() {
    let Some(g) = gpu() else { return };
    let m = block_mask();
    let t0 = block_scene(SKIN, SKY);
    let plain = rim_render(&g, &rim_in(&t0, None, None));
    // An arm in front of the torso: the depth jumps by 30 % at x = 32 inside the mask. The near side of the step gets the rim there.
    fn step(x: usize, _: usize) -> f32 {
        if x < 32 { 40.0 } else { 52.0 }
    }
    let i = RimIn { t0: &t0, mask: Some(&m), b11: b11(1.0, 3.0, 0.0), depth: Some(step), fog: None };
    let on = rim_render(&g, &i);
    assert!(lum_gain(&on, &plain, 31, 32) > 0.02, "the near side of a depth step is a silhouette");
    assert_eq!(px(&on, 28, 32), px(&plain, 28, 32), "4 px away from the step nothing happens");
}

#[test]
fn rim_alone_subsurface_warms_dim_pixels_and_spares_bright_ones() {
    let Some(g) = gpu() else { return };
    let m = block_mask();
    // A dark background: the rim has nothing to reflect, only the subsurface term acts.
    let dark = [0.05; 3];
    let dim = block_scene([0.35, 0.3, 0.28], dark);
    let plain = rim_render(&g, &rim_in(&dim, None, None));
    let on = rim_render(&g, &rim_in(&dim, Some(&m), b11(1.0, 3.0, 1.0)));
    let (a, b) = (lin3(px(&plain, 32, 32)), lin3(px(&on, 32, 32)));
    println!("subsurface: dim {a:?} -> {b:?}");
    assert!(b[0] / a[0] > b[2] / a[2] + 0.005, "the glow must be warm (red gains more than blue)");
    assert!(lum3(b) > lum3(a), "the glow lifts a dim pixel");
    // A bright pixel (linear luminance above 0.45) gets none.
    let bright = block_scene([0.85, 0.85, 0.85], dark);
    let p2 = rim_render(&g, &rim_in(&bright, None, None));
    let o2 = rim_render(&g, &rim_in(&bright, Some(&m), b11(1.0, 3.0, 1.0)));
    assert_eq!(px(&p2, 32, 32), px(&o2, 32, 32));
    // Subsurface 0 = nothing in the interior.
    let o3 = rim_render(&g, &rim_in(&dim, Some(&m), b11(1.0, 3.0, 0.0)));
    assert_eq!(px(&plain, 32, 32), px(&o3, 32, 32));
}

#[test]
fn rim_never_makes_nan_or_negative_output() {
    let Some(g) = gpu() else { return };
    let m = block_mask();
    let t0 = random_texels(21, -1.0, 3.0);
    let mut t1 = t0.clone();
    for (i, p) in t1.iter_mut().enumerate() {
        match i % 11 {
            0 => *p = [f32::NAN, 0.5, 0.5, 1.0],
            1 => *p = [f32::INFINITY, 1.0, 1.0, 1.0],
            2 => *p = [-3.0, -3.0, -3.0, 1.0],
            _ => {}
        }
    }
    fn mixed(x: usize, y: usize) -> f32 {
        match (x + 2 * y) % 5 {
            0 => 0.0,
            1 => f32::NAN,
            2 => -4.0,
            3 => 1e38,
            _ => 30.0 + x as f32,
        }
    }
    for t in [&t0, &t1] {
        for (w, depth) in [(8.0f32, None), (1.0, Some(mixed as fn(usize, usize) -> f32))] {
            let all = vec![255u8; N * N];
            for mk in [&m, &all] {
                let i = RimIn { t0: t, mask: Some(mk), b11: Some([[1.0, w, 1.0, 0.0], [0.01, 0.0, 0.0, 0.0]]), depth, fog: None };
                assert!(finite_nonneg(&rim_render(&g, &i)), "NaN or negative output (width {w})");
            }
        }
    }
}

#[test]
fn rim_debug_paints_the_mask_cyan_even_at_strength_zero() {
    let Some(g) = gpu() else { return };
    let m = block_mask();
    let t0 = block_scene(SKIN, SKY);
    let plain = rim_render(&g, &rim_in(&t0, None, None));
    let dbg = rim_render(&g, &rim_in(&t0, Some(&m), Some([[0.0, 3.0, 0.0, 1.0], [0.0; 4]])));
    let (p, d) = (px(&plain, 32, 32), px(&dbg, 32, 32));
    assert!(d[1] > p[1] + 0.2 && d[2] > p[2] + 0.2 && d[0] < p[0] * 0.5, "cyan expected: {p:?} -> {d:?}");
    assert_eq!(px(&dbg, 5, 5), px(&plain, 5, 5), "outside the mask nothing is painted");
}

#[test]
fn rim_and_fur_run_together_and_stay_independent() {
    let Some(g) = gpu() else { return };
    let m = block_mask();
    let t0 = block_scene(SKIN, SKY);
    let fur_only = {
        FUR_TEST.with(|f| f.set(1.0));
        FUR_MASK.with(|k| *k.borrow_mut() = Some(m.clone()));
        let o = rim_render(&g, &rim_in(&t0, None, None));
        FUR_MASK.with(|k| *k.borrow_mut() = None);
        FUR_TEST.with(|f| f.set(0.0));
        o
    };
    let rim_only = rim_render(&g, &rim_in(&t0, Some(&m), b11(1.0, 3.0, 0.0)));
    let both = {
        FUR_TEST.with(|f| f.set(1.0));
        FUR_MASK.with(|k| *k.borrow_mut() = Some(m.clone()));
        let o = rim_render(&g, &rim_in(&t0, Some(&m), b11(1.0, 3.0, 0.0)));
        FUR_MASK.with(|k| *k.borrow_mut() = None);
        FUR_TEST.with(|f| f.set(0.0));
        o
    };
    assert!(finite_nonneg(&both));
    // Deep inside the block only the fur acts; the rim leaves it alone.
    assert_eq!(px(&both, 32, 32), px(&fur_only, 32, 32));
    // At the silhouette both act: different from either alone.
    assert!(max_diff(&both[32 * N + BX.start..=32 * N + BX.start], &fur_only[32 * N + BX.start..=32 * N + BX.start]) > 0.004);
    assert!(max_diff(&both[32 * N + BX.start..=32 * N + BX.start], &rim_only[32 * N + BX.start..=32 * N + BX.start]) > 0.004);
}

#[test]
fn the_rim_follows_the_fur_discriminator_and_leaves_everything_else_alone() {
    // v5 (shell-fur.md): the fur mask's b > 0.5 = a fur draw with uv, a = the discriminator's weight. The upper half of the block is fur
    // (a = 1), the lower half a removed horn (a = 0); a b = 0 mask (armour, erased pixels) changes nothing.
    let Some(g) = gpu() else { return };
    let m = block_mask();
    let t0 = block_scene(SKIN, SKY);
    let horn = |y: usize| y >= 32;
    let fur: Vec<[u16; 4]> = (0..N * N).map(|k| if !in_block(k % N, k / N) { [0; 4] } else if horn(k / N) { [100, 200, 65535, 0] } else { [100, 200, 65535, 65535] }).collect();
    let plain = rim_render_fur(&g, &rim_in(&t0, Some(&m), None), Some(&fur));
    let rim = rim_render(&g, &rim_in(&t0, Some(&m), b11(1.0, 3.0, 0.5)));
    let rim_fur = rim_render_fur(&g, &rim_in(&t0, Some(&m), b11(1.0, 3.0, 0.5)), Some(&fur));
    let no_uv: Vec<[u16; 4]> = (0..N * N).map(|k| if in_block(k % N, k / N) { [0, 0, 0, 65535] } else { [0; 4] }).collect();
    let rim_armour = rim_render_fur(&g, &rim_in(&t0, Some(&m), b11(1.0, 3.0, 0.5)), Some(&no_uv));
    assert!(finite_nonneg(&rim_fur));
    assert_eq!(max_diff(&rim, &rim_armour), 0.0, "a fur mask without uv (b = 0) must leave the rim bit-identical");
    let (mut fur_edge, mut horn_edge) = (0usize, 0usize);
    for y in BY {
        for x in BX {
            if horn(y) {
                assert_eq!(px(&rim_fur, x, y), px(&plain, x, y), "removed texel ({x},{y}) got a rim");
                horn_edge += (edge_d(x, y) == 1) as usize;
            } else {
                assert_eq!(px(&rim_fur, x, y), px(&rim, x, y), "fur texel ({x},{y}): the rim must be as without the discriminator");
                fur_edge += (edge_d(x, y) == 1 && max_diff(&[px(&rim, x, y)], &[px(&plain, x, y)]) > 0.004) as usize;
            }
        }
    }
    println!("rim follows the discriminator: {fur_edge} fur edge px lit, {horn_edge} horn edge px left alone");
    assert!(fur_edge > 10 && horn_edge > 10);
}

/// v11: an R8_UNORM texture (w x h) for the skin gate t25.
fn skin_tex(g: &Gpu, w: usize, h: usize, v: &[u8]) -> ID3D11ShaderResourceView {
    assert_eq!(v.len(), w * h);
    let t = tex2d(g, w as u32, h as u32, DXGI_FORMAT_R8_UNORM, D3D11_BIND_SHADER_RESOURCE, D3D11_USAGE_IMMUTABLE, 0, Some((v, w as u32)));
    let mut s = None;
    unsafe { g.dev.CreateShaderResourceView(&t, None, Some(&mut s)).unwrap() };
    s.unwrap()
}

/// v11: the old fur look (strands on a uv gradient, detail, feather, warmth) and the rim light on one block, with the skin gate `skin`
/// at t25 (None = unbound); `fur` / `rim` switch each on.
fn gated_render(g: &Gpu, t0: &[[f32; 4]], fur: bool, rim: bool, skin: Option<ID3D11ShaderResourceView>) -> Vec<[f32; 4]> {
    let m = block_mask();
    let uv: Vec<[f32; 2]> = (0..N * N).map(|k| [(k % N) as f32 * 0.011, (k / N) as f32 * 0.004]).collect();
    FUR_TEST.with(|f| f.set(if fur { 1.0 } else { 0.0 }));
    FUR_MASK.with(|k| *k.borrow_mut() = Some(m.clone()));
    FUR_UV.with(|u| *u.borrow_mut() = Some(uv));
    let cb11 = b11(1.0, 3.0, 0.5).map(|v| {
        let b: Vec<u8> = v.iter().flat_map(|p| p.iter().flat_map(|x| x.to_le_bytes())).collect();
        let d = D3D11_BUFFER_DESC { ByteWidth: 32, Usage: D3D11_USAGE_IMMUTABLE, BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32, ..Default::default() };
        let sd = D3D11_SUBRESOURCE_DATA { pSysMem: b.as_ptr() as *const c_void, SysMemPitch: 0, SysMemSlicePitch: 0 };
        let mut cb = None;
        unsafe { g.dev.CreateBuffer(&d, Some(&sd), Some(&mut cb)).unwrap() };
        cb.unwrap()
    });
    let rim_srv = skin_tex(g, N, N, &m);
    let hook = |c: &ID3D11DeviceContext| unsafe {
        if rim {
            c.PSSetShaderResources(23, Some(&[Some(rim_srv.clone())]));
            c.PSSetConstantBuffers(11, Some(&[cb11.clone()]));
        }
        c.PSSetShaderResources(25, Some(&[skin.clone()]));
    };
    let out = render_hook(g, VS_LIGHT, light_ps(), &Inputs { t0, t1: Some(IDENT_T1), cb: &LIGHT_CB }, Some((1.0, [1.0, 0.0, 0.0, 0.0])), None, &hook);
    unsafe {
        g.ctx.PSSetShaderResources(23, Some(&[None]));
        g.ctx.PSSetShaderResources(25, Some(&[None]));
        g.ctx.PSSetConstantBuffers(11, Some(&[None]));
    }
    FUR_TEST.with(|f| f.set(0.0));
    FUR_MASK.with(|k| *k.borrow_mut() = None);
    FUR_UV.with(|u| *u.borrow_mut() = None);
    out
}

#[test]
fn v11_the_skin_gate_switches_the_old_fur_look_and_the_rim_off_where_no_fur_skin_is_proven() {
    // shell-fur.md v11: t25 = how much the visible surface is proven fur skin (the shell classification). The block is the fur / rim mask
    // with a mesh uv (the old strands); its left half is proven fur (255), its right half a hair card / horn / armour (0).
    let Some(g) = gpu() else { return };
    // A textured coat (the detail term and the strands both act).
    let t0: Vec<[f32; 4]> = (0..N * N)
        .map(|k| {
            let (x, y) = (k % N, k / N);
            if in_block(x, y) {
                let n = 0.04 * (((x * 7 + y * 13) % 5) as f32 - 2.0);
                [SKIN[0] + n, SKIN[1] + n, SKIN[2] + n, 1.0]
            } else {
                [SKY[0], SKY[1], SKY[2], 1.0]
            }
        })
        .collect();
    let left = |x: usize| x < (BX.start + BX.end) / 2;
    let half: Vec<u8> = (0..N * N).map(|k| if left(k % N) { 255 } else { 0 }).collect();
    let plain = gated_render(&g, &t0, false, false, None);
    let v10 = gated_render(&g, &t0, true, true, None);
    let ones = gated_render(&g, &t0, true, true, Some(skin_tex(&g, N, N, &vec![255u8; N * N])));
    let gated = gated_render(&g, &t0, true, true, Some(skin_tex(&g, N, N, &half)));
    let none = gated_render(&g, &t0, true, true, Some(skin_tex(&g, 1, 1, &[0u8])));
    let fur_v10 = gated_render(&g, &t0, true, false, None);
    let fur_gated = gated_render(&g, &t0, true, false, Some(skin_tex(&g, N, N, &half)));
    let rim_v10 = gated_render(&g, &t0, false, true, None);
    let rim_gated = gated_render(&g, &t0, false, true, Some(skin_tex(&g, N, N, &half)));
    assert!(finite_nonneg(&gated));
    // Skin 1 everywhere = unbound = v10, bit for bit.
    assert_eq!(max_diff(&ones, &v10), 0.0, "a gate of 1 is the v10 output");
    // A 1x1 zero (the shells on, nothing recorded): neither the old look nor the rim anywhere.
    assert_eq!(max_diff(&none, &plain), 0.0, "a zero gate leaves the picture as without the fur look and the rim");
    let (mut right_v10, mut right_fur, mut right_rim, mut left_kept) = (0usize, 0usize, 0usize, 0usize);
    for y in 0..N {
        for x in 0..N {
            if in_block(x, y) && !left(x) {
                // Not proven fur: exactly the picture without the old look and the rim (no strands, no detail, no feather, no glow).
                assert_eq!(px(&gated, x, y), px(&plain, x, y), "({x},{y}) not fur skin got the old look / rim");
                assert_eq!(px(&fur_gated, x, y), px(&plain, x, y));
                assert_eq!(px(&rim_gated, x, y), px(&plain, x, y));
                right_v10 += (max_diff(&[px(&v10, x, y)], &[px(&plain, x, y)]) > 0.004) as usize;
                right_fur += (max_diff(&[px(&fur_v10, x, y)], &[px(&plain, x, y)]) > 0.004) as usize;
                right_rim += (max_diff(&[px(&rim_v10, x, y)], &[px(&plain, x, y)]) > 0.004) as usize;
            } else if in_block(x, y) {
                // Proven fur: as v10 (the gate is 1 there; the look reads its neighbours the same way).
                assert_eq!(px(&gated, x, y), px(&v10, x, y), "({x},{y}) proven fur must be as before");
                left_kept += (max_diff(&[px(&gated, x, y)], &[px(&plain, x, y)]) > 0.004) as usize;
            } else {
                assert_eq!(px(&gated, x, y), px(&plain, x, y), "({x},{y}) outside the mask is never touched");
            }
        }
    }
    println!("v11 gate: right half (not fur) changed by v10: {right_v10} px (fur look {right_fur}, rim {right_rim}), by v11: 0; left half (fur) still changed: {left_kept} px");
    assert!(right_v10 > 100 && right_fur > 100 && right_rim > 10, "the scene proves the old look and the rim would act there");
    assert!(left_kept > 100, "proven fur keeps the old look");
}

// ---- HDR10 final pass, calibration patterns, photo scene maths (WARP) ------------------------------------------------------------------

use crate::output::{FinalOpts, final_hlsl_with};

fn hdr10_ps(paper: f32, peak: f32, no_dither: bool) -> Vec<u8> {
    compile_ps(&final_hlsl_with(paper, peak, FinalOpts { hdr10: true, no_dither, pattern: 0, ..FinalOpts::default() }), crate::output::FINAL_ENTRY).unwrap_or_else(|e| panic!("{e}"))
}

#[test]
fn the_default_final_shader_has_none_of_the_new_switches() {
    let s = crate::output::final_hlsl(203.0, 1000.0);
    // the prepended defines are the four of the scRGB build and nothing else
    let defs: Vec<&str> = s.lines().take_while(|l| l.starts_with("#define")).collect();
    assert_eq!(defs.len(), 4, "the plain scRGB build adds no define: {defs:?}");
    assert!(defs.iter().all(|l| l.contains("PAPER_WHITE_SCALE") || l.contains("PEAK_SCALE") || l.contains("HEADROOM") || l.contains("KNEE")));
    assert_eq!(s, final_hlsl_with(203.0, 1000.0, FinalOpts::default()));
    let h = final_hlsl_with(203.0, 1000.0, FinalOpts { hdr10: true, no_dither: true, pattern: 2, ..FinalOpts::default() });
    assert!(h.contains("#define HDR10_OUTPUT 1") && h.contains("#define DITHER 0") && h.contains("#define CALIB_PATTERN 2"));
    let has = |s: String, d: &str| s.lines().take_while(|l| l.starts_with("#define")).any(|l| l.contains(d));
    assert!(!has(final_hlsl_with(203.0, 1000.0, FinalOpts { hdr10: false, no_dither: true, pattern: 0, ..FinalOpts::default() }), "DITHER"), "no_dither only matters for HDR10");
    assert!(!has(final_hlsl_with(203.0, 1000.0, FinalOpts { pattern: 7, ..FinalOpts::default() }), "CALIB_PATTERN"), "only patterns 1..3 exist");
}

#[test]
fn final_hdr10_and_the_patterns_compile_with_the_original_interface() {
    let want = interface_of(&disasm(&compile_ps(&crate::output::final_hlsl(203.0, 1000.0), crate::output::FINAL_ENTRY).unwrap()));
    // The HDR10 picture reads the same inputs as the original final pass.
    assert_eq!(interface_of(&disasm(&hdr10_ps(203.0, 1000.0, true))), want);
    // The dithered one also reads SV_POSITION (the pixel's own position, which every vertex shader provides): nothing else differs.
    let (mut sig, mut res) = interface_of(&disasm(&hdr10_ps(203.0, 1000.0, false)));
    sig[0] = sig[0].trim_end_matches(" xy").to_string();
    res.retain(|l| !l.contains(", position"));
    assert_eq!((sig, res), want);
    // The patterns compile in both encodings (they do not read the picture).
    for p in 1..=3u8 {
        for hdr10 in [false, true] {
            compile_ps(&final_hlsl_with(203.0, 1000.0, FinalOpts { hdr10, no_dither: false, pattern: p, ..FinalOpts::default() }), crate::output::FINAL_ENTRY).unwrap_or_else(|e| panic!("pattern {p} hdr10 {hdr10}: {e}"));
        }
    }
}

/// The HDR10 shader against the host reference: `final_reference` (the scRGB maths of the shader) -> BT.709 to BT.2020 -> x 80 -> PQ.
#[test]
fn final_hdr10_matches_the_host_pq_reference_over_a_log_range() {
    let Some(g) = gpu() else { return };
    let mut worst = 0.0f32;
    for (paper, peak) in [(203.0f32, 1000.0f32), (120.0, 4000.0), (300.0, 600.0)] {
        let ps = hdr10_ps(paper, peak, true);
        // gamma-encoded scene values from almost black to far over range
        for v in [0.0f32, 0.0005, 0.002, 0.01, 0.05, 0.12, 0.25, 0.5, 0.75, 1.0, 1.5, 2.5, 8.0, 20.0, 200.0] {
            let t0 = final_texels(v);
            let out = render(&g, VS_LIGHT, &ps, &Inputs { t0: &t0, t1: None, cb: &[[1.0, 0.0, 0.0, 0.0]] });
            let sc = final_reference(t0[0], 1.0, paper, peak);
            let want = crate::hdr10::scrgb_to_pq([sc[0], sc[1], sc[2]]);
            for p in &out {
                for i in 0..3 {
                    let d = (p[i] - want[i]).abs();
                    worst = worst.max(d);
                    assert!(d <= 1e-3, "paper {paper} peak {peak} v={v} ch={i}: got {} want {} (scRGB {:?})", p[i], want[i], sc);
                }
                assert_eq!(p[3], 1.0, "alpha");
                assert!(p[..3].iter().all(|c| c.is_finite() && *c >= 0.0 && *c <= 1.0 + 1e-3), "{p:?}");
            }
        }
    }
    println!("final_hdr10: worst PQ code error {worst:.2e} (tolerance 1e-3)");
}

#[test]
fn final_hdr10_round_trips_through_the_pq_decode() {
    let Some(g) = gpu() else { return };
    let ps = hdr10_ps(203.0, 1000.0, true);
    // The scene value 1.0 (paper white): the PQ decode of the shader output is 203 nits on every channel.
    let out = render(&g, VS_LIGHT, &ps, &Inputs { t0: &uniform([1.0, 1.0, 1.0, 1.0]), t1: None, cb: &[[1.0, 0.0, 0.0, 0.0]] });
    for c in &out[0][..3] {
        let nits = crate::hdr10::pq_decode(*c);
        assert!((nits - 203.0).abs() < 203.0 * 0.01, "decoded {nits} nits");
    }
    // Far over range: the peak (1000 nits) is never exceeded after the decode.
    let out = render(&g, VS_LIGHT, &ps, &Inputs { t0: &uniform([200.0, 200.0, 200.0, 1.0]), t1: None, cb: &[[1.0, 0.0, 0.0, 0.0]] });
    for c in &out[0][..3] {
        assert!(crate::hdr10::pq_decode(*c) <= 1000.0 * 1.01, "{}", crate::hdr10::pq_decode(*c));
    }
}

#[test]
fn final_hdr10_dither_is_one_lsb_stable_and_has_no_bias() {
    let Some(g) = gpu() else { return };
    let plain = hdr10_ps(203.0, 1000.0, true);
    let dith = hdr10_ps(203.0, 1000.0, false);
    let t0 = uniform([0.5, 0.5, 0.5, 1.0]);
    let inp = Inputs { t0: &t0, t1: None, cb: &[[1.0, 0.0, 0.0, 0.0]] };
    let a = render(&g, VS_LIGHT, &plain, &inp);
    let b = render(&g, VS_LIGHT, &dith, &inp);
    let b2 = render(&g, VS_LIGHT, &dith, &inp);
    assert_eq!(b, b2, "identical input, identical output");
    let lsb = 1.0 / 1023.0;
    let (mut max, mut sum, mut distinct) = (0.0f32, 0.0f64, std::collections::BTreeSet::new());
    for (x, y) in a.iter().zip(&b) {
        let d = y[0] - x[0];
        max = max.max(d.abs());
        sum += d as f64;
        distinct.insert(d.to_bits());
    }
    // the stored half float has its own rounding (about 5e-4 at these codes): allow that on top of one 10-bit step
    assert!(max <= lsb + 6e-4, "dither amplitude {max} vs one LSB {lsb}");
    assert!(max >= lsb * 0.5, "the dither is there: {max}");
    assert!((sum / a.len() as f64).abs() < 2e-4, "no bias: {}", sum / a.len() as f64);
    assert!(distinct.len() >= 3, "a noise pattern, not a constant offset");
    // The three channels get different noise only through the same pixel hash, never NaN or out of range
    assert!(b.iter().all(|p| p[..3].iter().all(|c| c.is_finite() && (0.0..=1.0).contains(c))));
}

fn cell_pixel(p: u8, i: usize) -> (usize, usize) {
    let (x, _) = crate::calib::label_pos(p, i).unwrap();
    let row = if p == crate::calib::PAPER { crate::calib::PAPER_ROW } else { crate::calib::ROW };
    (((x * N as f32) as usize).min(N - 1), (((row.0 + row.1) * 0.5 * N as f32) as usize).min(N - 1))
}

#[test]
fn calibration_patterns_show_the_documented_nits_in_scrgb() {
    let Some(g) = gpu() else { return };
    let t0 = uniform([0.5, 0.5, 0.5, 1.0]);
    for (paper, peak) in [(203.0f32, 1000.0f32), (150.0, 600.0)] {
        for p in 1..=3u8 {
            let ps = compile_ps(&final_hlsl_with(paper, peak, FinalOpts { pattern: p, ..FinalOpts::default() }), crate::output::FINAL_ENTRY).unwrap();
            let out = render(&g, VS_LIGHT, &ps, &Inputs { t0: &t0, t1: None, cb: &[[1.0, 0.0, 0.0, 0.0]] });
            for i in 0..crate::calib::cells(p) {
                let (x, y) = cell_pixel(p, i);
                let mut want = crate::calib::nits_of(p, i, paper).unwrap();
                if p == crate::calib::PAPER {
                    want = want.min(peak);
                }
                let got = out[y * N + x];
                let tol = want / 80.0 * 2e-3 + 2e-6;
                for c in 0..3 {
                    assert!((got[c] - want / 80.0).abs() <= tol, "pattern {p} cell {i} ch {c}: got {} nits want {want}", got[c] * 80.0);
                }
                assert_eq!(got[3], 1.0);
            }
            // background (above the blocks) is black, whatever the picture was
            let bg = out[5 * N + 5];
            assert_eq!(bg[..3], [0.0, 0.0, 0.0], "pattern {p} background");
        }
    }
}

#[test]
fn calibration_patterns_in_hdr10_are_pq_of_the_same_nits() {
    let Some(g) = gpu() else { return };
    let t0 = uniform([0.5, 0.5, 0.5, 1.0]);
    for p in 1..=2u8 {
        let ps = compile_ps(&final_hlsl_with(203.0, 1000.0, FinalOpts { hdr10: true, no_dither: true, pattern: p, ..FinalOpts::default() }), crate::output::FINAL_ENTRY).unwrap();
        let out = render(&g, VS_LIGHT, &ps, &Inputs { t0: &t0, t1: None, cb: &[[1.0, 0.0, 0.0, 0.0]] });
        for i in 0..crate::calib::cells(p) {
            let (x, y) = cell_pixel(p, i);
            let want = crate::hdr10::pq_encode(crate::calib::nits_of(p, i, 203.0).unwrap().min(if p == 2 { 1000.0 } else { 10000.0 }));
            let got = out[y * N + x];
            for c in 0..3 {
                assert!((got[c] - want).abs() <= 1e-3, "pattern {p} cell {i}: got {} want {want}", got[c]);
            }
        }
    }
}

#[test]
fn the_cpu_final_pass_equals_the_shader_reference() {
    // photo.rs applies the final pass' maths to the grabbed pre-UI scene; it must equal the reference the shader is tested against.
    for (paper, peak) in [(203.0f32, 1000.0f32), (120.0, 4000.0), (300.0, 600.0)] {
        for v in [0.0f32, 0.01, 0.2, 0.5, 1.0, 1.7, 3.0, 12.0, 100.0] {
            let t = [v, v * 0.5, v * 0.25, 1.0];
            let want = final_reference(t, 1.0, paper, peak);
            let got = crate::photo::final_pass_cpu([t[0], t[1], t[2]], paper, peak);
            for i in 0..3 {
                assert!((got[i] - want[i]).abs() <= want[i].abs() * 1e-5 + 1e-6, "paper {paper} peak {peak} v {v}: {got:?} vs {want:?}");
            }
        }
    }
}

// ---- the live fur look (b12) and its diagnostic views (docs/fur-look.md) --------------------------------------------------------

/// One render of the light-adjust substitute with the fur on: the scene, b6[1].zw, the fur mask (t20) and its mesh uv, and b12.
#[derive(Clone)]
struct FurIn {
    t0: Vec<[f32; 4]>,
    /// b6[1].z: the fur strength.
    fur: f32,
    /// b6[1].w: the fur selector debug (magenta).
    debug: f32,
    /// t20 alpha (None = unbound).
    mask: Option<Vec<u8>>,
    /// The mesh uv of the mask pixels (None = a draw without uv: rg = 0, b = 0).
    uv: Option<Vec<[f32; 2]>>,
    /// b12 (None = unbound = zeros).
    b12: Option<[[f32; 4]; 2]>,
    /// The identity remap in t1: the output is the encoded colour itself (plus the +-0.002 dither), not the game's remap.
    ident: bool,
}

impl FurIn {
    /// Full strength, the whole picture is fur, no uv, b12 unbound, the game's remap.
    fn new(t0: Vec<[f32; 4]>) -> FurIn {
        FurIn { t0, fur: 1.0, debug: 0.0, mask: Some(mask_all(255)), uv: None, b12: None, ident: false }
    }
}

fn fur_render(g: &Gpu, i: &FurIn) -> Vec<[f32; 4]> {
    FUR_TEST.with(|f| f.set(i.fur));
    FUR_DEBUG.with(|f| f.set(i.debug));
    FUR_MASK.with(|m| *m.borrow_mut() = i.mask.clone());
    FUR_UV.with(|u| *u.borrow_mut() = i.uv.clone());
    let out = unsafe {
        let cb12 = i.b12.map(|v| {
            let b: Vec<u8> = v.iter().flat_map(|p| p.iter().flat_map(|x| x.to_le_bytes())).collect();
            let d = D3D11_BUFFER_DESC { ByteWidth: 32, Usage: D3D11_USAGE_IMMUTABLE, BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32, ..Default::default() };
            let sd = D3D11_SUBRESOURCE_DATA { pSysMem: b.as_ptr() as *const c_void, SysMemPitch: 0, SysMemSlicePitch: 0 };
            let mut cb = None;
            g.dev.CreateBuffer(&d, Some(&sd), Some(&mut cb)).unwrap();
            cb.unwrap()
        });
        let hook = |c: &ID3D11DeviceContext| {
            if cb12.is_some() {
                c.PSSetConstantBuffers(12, Some(&[cb12.clone()]));
            }
        };
        let inp = Inputs { t0: &i.t0, t1: Some(if i.ident { IDENT_T1 } else { LIGHT_T1 }), cb: &LIGHT_CB };
        let out = render_hook(g, VS_LIGHT, light_ps(), &inp, Some((1.0, [1.0, 0.0, 0.0, 0.0])), None, &hook);
        g.ctx.PSSetConstantBuffers(12, Some(&[None]));
        out
    };
    FUR_TEST.with(|f| f.set(0.0));
    FUR_DEBUG.with(|f| f.set(0.0));
    FUR_MASK.with(|m| *m.borrow_mut() = None);
    FUR_UV.with(|u| *u.borrow_mut() = None);
    out
}

/// b12 as the mod writes it: (amp, scale, length, detail), (rim, sss, view, live = 1).
fn b12v(amp: f32, scale: f32, len: f32, detail: f32, rim: f32, sss: f32, view: f32) -> Option<[[f32; 4]; 2]> {
    Some([[amp, scale, len, detail], [rim, sss, view, 1.0]])
}

/// The default look as the mod sends it.
const B12_DEFAULTS: [[f32; 4]; 2] = [[0.32, 1.0, 5.0, 1.0], [0.25, 1.0, 0.0, 1.0]];

/// Same, with some values replaced (amp, scale, length, detail, rim, sss, view).
fn b12_with(f: impl Fn(&mut [[f32; 4]; 2])) -> Option<[[f32; 4]; 2]> {
    let mut b = B12_DEFAULTS;
    f(&mut b);
    Some(b)
}

/// The sample pixels of `OLD_FUR_GOLDEN` (x, y).
const OLD_FUR_PTS: [(usize, usize); 20] = [(6, 6), (12, 20), (20, 32), (31, 31), (32, 32), (33, 10), (40, 45), (45, 20), (50, 50), (57, 57), (10, 50), (31, 32), (30, 32), (29, 32), (28, 32), (31, 10), (31, 50), (27, 40), (5, 32), (16, 16)];

/// What the fur look drew BEFORE its constants became live parameters: the output of the old shader on WARP for the scenes of `old_fur_scene`
/// (captured once from the previous `light_adjust_hdr.hlsl`): per scene the sums of the red / green / blue values over the 64 x 64 picture and the
/// RGB of the sample pixels. The new shader with its default parameters must reproduce them (the outline excepted, see `fur_rim`).
struct OldFur {
    scene: &'static str,
    sums: [f64; 3],
    px: [[f32; 3]; 20],
}

const OLD_FUR_GOLDEN: [OldFur; 6] = [
    OldFur {
        scene: "A",
        sums: [2224.104, 1743.881, 1281.484],
        px: [
            [0.62353516, 0.49951172, 0.37524414], // (6, 6)
            [0.40698242, 0.30297852, 0.21044922], // (12, 20)
            [0.6850586, 0.5605469, 0.4296875], // (20, 32)
            [0.6821289, 0.5576172, 0.42700195], // (31, 31)
            [0.6816406, 0.5571289, 0.42651367], // (32, 32)
            [0.5004883, 0.38500977, 0.2775879], // (33, 10)
            [0.5209961, 0.40356445, 0.2927246], // (40, 45)
            [0.5498047, 0.4296875, 0.31469727], // (45, 20)
            [0.6069336, 0.48388672, 0.36132813], // (50, 50)
            [0.42163086, 0.3166504, 0.22216797], // (57, 57)
            [0.5239258, 0.40625, 0.29492188], // (10, 50)
            [0.6845703, 0.5600586, 0.42944336], // (31, 32)
            [0.68310547, 0.55859375, 0.42797852], // (30, 32)
            [0.6850586, 0.5605469, 0.4296875], // (29, 32)
            [0.68652344, 0.5620117, 0.43139648], // (28, 32)
            [0.6123047, 0.48901367, 0.3659668], // (31, 10)
            [0.40625, 0.30249023, 0.20983887], // (31, 50)
            [0.5283203, 0.4104004, 0.29858398], // (27, 40)
            [0.57666016, 0.45483398, 0.3359375], // (5, 32)
            [0.48583984, 0.37182617, 0.26611328], // (16, 16)
        ],
    },
    OldFur {
        scene: "B",
        sums: [2017.474, 1769.579, 1539.225],
        px: [
            [0.66015625, 0.59375, 0.5292969], // (6, 6)
            [0.4248047, 0.35424805, 0.2915039], // (12, 20)
            [0.71875, 0.6611328, 0.6020508], // (20, 32)
            [0.5131836, 0.46728516, 0.42211914], // (31, 31)
            [0.7158203, 0.65771484, 0.5986328], // (32, 32)
            [0.38330078, 0.32861328, 0.27929688], // (33, 10)
            [0.54785156, 0.47558594, 0.40893555], // (40, 45)
            [0.4189453, 0.36499023, 0.31518555], // (45, 20)
            [0.64208984, 0.57373047, 0.50878906], // (50, 50)
            [0.32495117, 0.27197266, 0.22497559], // (57, 57)
            [0.55126953, 0.47875977, 0.41210938], // (10, 50)
            [0.5151367, 0.46972656, 0.42456055], // (31, 32)
            [0.7167969, 0.6591797, 0.60009766], // (30, 32)
            [0.5151367, 0.46972656, 0.4248047], // (29, 32)
            [0.72021484, 0.66259766, 0.6040039], // (28, 32)
            [0.46435547, 0.4128418, 0.3642578], // (31, 10)
            [0.31298828, 0.25976563, 0.21276855], // (31, 50)
            [0.4038086, 0.34936523, 0.29956055], // (27, 40)
            [0.43798828, 0.38500977, 0.33544922], // (5, 32)
            [0.5097656, 0.43701172, 0.37109375], // (16, 16)
        ],
    },
    OldFur {
        scene: "C",
        sums: [2364.250, 2098.178, 1841.388],
        px: [
            [0.94970703, 0.8432617, 0.7402344], // (6, 6)
            [0.9326172, 0.8251953, 0.7216797], // (12, 20)
            [0.93847656, 0.83154297, 0.72802734], // (20, 32)
            [0.20861816, 0.18127441, 0.15612793], // (31, 31)
            [0.95214844, 0.8461914, 0.74316406], // (32, 32)
            [0.20788574, 0.18054199, 0.15527344], // (33, 10)
            [0.99365234, 0.89697266, 0.7998047], // (40, 45)
            [0.2076416, 0.18029785, 0.15515137], // (45, 20)
            [0.85009766, 0.7397461, 0.6357422], // (50, 50)
            [0.21069336, 0.18347168, 0.15820313], // (57, 57)
            [0.98876953, 0.89697266, 0.8041992], // (10, 50)
            [0.21264648, 0.1854248, 0.16027832], // (31, 32)
            [0.95214844, 0.8457031, 0.7426758], // (30, 32)
            [0.21276855, 0.18566895, 0.16052246], // (29, 32)
            [0.95996094, 0.8544922, 0.75146484], // (28, 32)
            [0.2076416, 0.18029785, 0.15515137], // (31, 10)
            [0.22290039, 0.19616699, 0.17126465], // (31, 50)
            [0.22143555, 0.19458008, 0.1694336], // (27, 40)
            [0.19506836, 0.16760254, 0.14257813], // (5, 32)
            [0.9663086, 0.8613281, 0.75878906], // (16, 16)
        ],
    },
    OldFur {
        scene: "D",
        sums: [2148.105, 1699.600, 1260.559],
        px: [
            [0.62353516, 0.49951172, 0.37524414], // (6, 6)
            [0.40698242, 0.30297852, 0.21044922], // (12, 20)
            [0.6850586, 0.5605469, 0.4296875], // (20, 32)
            [0.7314453, 0.6020508, 0.46484375], // (31, 31)
            [0.5107422, 0.40844727, 0.3059082], // (32, 32)
            [0.5136719, 0.41137695, 0.3088379], // (33, 10)
            [0.5126953, 0.4104004, 0.30786133], // (40, 45)
            [0.5126953, 0.4104004, 0.30810547], // (45, 20)
            [0.5131836, 0.41064453, 0.3083496], // (50, 50)
            [0.5131836, 0.41088867, 0.30859375], // (57, 57)
            [0.5239258, 0.40625, 0.29492188], // (10, 50)
            [0.734375, 0.6044922, 0.46728516], // (31, 32)
            [0.7207031, 0.59228516, 0.45654297], // (30, 32)
            [0.7192383, 0.5917969, 0.45654297], // (29, 32)
            [0.7036133, 0.5776367, 0.44482422], // (28, 32)
            [0.65478516, 0.52685547, 0.3984375], // (31, 10)
            [0.4272461, 0.32128906, 0.22546387], // (31, 50)
            [0.5283203, 0.4104004, 0.29858398], // (27, 40)
            [0.57666016, 0.45483398, 0.3359375], // (5, 32)
            [0.48583984, 0.37182617, 0.26611328], // (16, 16)
        ],
    },
    OldFur {
        scene: "E",
        sums: [2650.048, 2373.189, 2100.847],
        px: [
            [0.75097656, 0.67041016, 0.5917969], // (6, 6)
            [0.7495117, 0.6689453, 0.5908203], // (12, 20)
            [0.75, 0.6694336, 0.5913086], // (20, 32)
            [0.5839844, 0.51953125, 0.45654297], // (31, 31)
            [0.7167969, 0.64501953, 0.5732422], // (32, 32)
            [0.5629883, 0.50683594, 0.45092773], // (33, 10)
            [0.71875, 0.64697266, 0.5751953], // (40, 45)
            [0.5625, 0.50634766, 0.4501953], // (45, 20)
            [0.7192383, 0.64746094, 0.5756836], // (50, 50)
            [0.5629883, 0.50683594, 0.4506836], // (57, 57)
            [0.75, 0.6699219, 0.5913086], // (10, 50)
            [0.58496094, 0.5205078, 0.45776367], // (31, 32)
            [0.7651367, 0.68359375, 0.60302734], // (30, 32)
            [0.5654297, 0.5029297, 0.4423828], // (29, 32)
            [0.7578125, 0.6767578, 0.59765625], // (28, 32)
            [0.5859375, 0.5214844, 0.4584961], // (31, 10)
            [0.5834961, 0.51904297, 0.4560547], // (31, 50)
            [0.5566406, 0.4951172, 0.43579102], // (27, 40)
            [0.55371094, 0.4921875, 0.43286133], // (5, 32)
            [0.7495117, 0.6689453, 0.5908203], // (16, 16)
        ],
    },
    OldFur {
        scene: "F",
        sums: [2102.079, 1849.232, 1612.635],
        px: [
            [0.2980957, 0.25927734, 0.22375488], // (6, 6)
            [0.296875, 0.2578125, 0.22241211], // (12, 20)
            [0.29760742, 0.25854492, 0.22302246], // (20, 32)
            [0.7294922, 0.64501953, 0.5649414], // (31, 31)
            [0.29638672, 0.25732422, 0.22180176], // (32, 32)
            [0.7319336, 0.64746094, 0.56689453], // (33, 10)
            [0.2980957, 0.25927734, 0.22375488], // (40, 45)
            [0.73095703, 0.6464844, 0.56640625], // (45, 20)
            [0.29858398, 0.25952148, 0.2241211], // (50, 50)
            [0.7314453, 0.64697266, 0.56689453], // (57, 57)
            [0.29760742, 0.25854492, 0.22314453], // (10, 50)
            [0.73046875, 0.6459961, 0.56591797], // (31, 32)
            [0.29589844, 0.25683594, 0.22143555], // (30, 32)
            [0.72998047, 0.6455078, 0.5654297], // (29, 32)
            [0.2993164, 0.2602539, 0.22485352], // (28, 32)
            [0.7314453, 0.64697266, 0.56689453], // (31, 10)
            [0.7290039, 0.64453125, 0.5644531], // (31, 50)
            [0.7314453, 0.64697266, 0.56689453], // (27, 40)
            [0.7285156, 0.64404297, 0.56396484], // (5, 32)
            [0.296875, 0.2578125, 0.22241211], // (16, 16)
        ],
    },
];

/// The scenes of `OLD_FUR_GOLDEN`: (scene, fur mask, mesh uv, fur strength).
fn old_fur_scene(name: &str) -> FurIn {
    let (t0, mask, uv, fur) = match name {
        "A" => (uniform([0.5, 0.4, 0.3, 0.6]), mask_all(255), Some(uv_ramp(0.004, 0.004, 0.0)), 1.0),
        "B" => (stripes(0.5, 0.4), mask_all(255), Some(uv_ramp(0.004, 0.004, 0.0)), 1.0),
        "C" => (stripes(0.9, 0.2), mask_all(255), Some(uv_ramp(0.0005, 0.02, 0.0)), 0.6),
        "D" => (uniform([0.5, 0.4, 0.3, 0.6]), mask_left(), Some(uv_ramp(0.004, 0.004, 0.0)), 1.0),
        "E" => (stripes(0.7, 0.55), mask_left(), None, 0.5),
        "F" => (stripes(0.3, 0.6), mask_all(255), None, 1.0),
        _ => unreachable!("{name}"),
    };
    FurIn { fur, mask: Some(mask), uv, ..FurIn::new(t0) }
}

fn sums_of(out: &[[f32; 4]]) -> [f64; 3] {
    let mut s = [0f64; 3];
    for p in out {
        for k in 0..3 {
            s[k] += p[k] as f64;
        }
    }
    s
}

/// The output equals what the old shader drew: the sums of the channels (within 0.6, about 4e-5 per pixel) and the sample pixels (within 0.0016, a few half floats).
fn assert_old_fur(gd: &OldFur, out: &[[f32; 4]], what: &str) {
    let s = sums_of(out);
    for k in 0..3 {
        assert!((s[k] - gd.sums[k]).abs() <= 0.6, "{what}: scene {} channel {k}: sum {} vs the old shader's {}", gd.scene, s[k], gd.sums[k]);
    }
    for (n, &(x, y)) in OLD_FUR_PTS.iter().enumerate() {
        let p = px(out, x, y);
        for k in 0..3 {
            assert!((p[k] - gd.px[n][k]).abs() <= 0.0016, "{what}: scene {} pixel ({x},{y}) channel {k}: {} vs the old shader's {}", gd.scene, p[k], gd.px[n][k]);
        }
    }
}

#[test]
fn fur_look_defaults_equal_the_old_fur_look_on_masks_without_a_silhouette() {
    let Some(g) = gpu() else { return };
    for gd in &OLD_FUR_GOLDEN {
        // Full masks: the picture edge is not a silhouette, so the (new, subtler) outline plays no part.
        if !matches!(gd.scene, "A" | "B" | "C" | "F") {
            continue;
        }
        let base = old_fur_scene(gd.scene);
        assert_old_fur(gd, &fur_render(&g, &base), "b12 unbound");
        assert_old_fur(gd, &fur_render(&g, &FurIn { b12: Some(B12_DEFAULTS), ..base.clone() }), "b12 = the defaults the mod sends");
        // fur_rim 1 would only matter at a silhouette: it changes nothing here either.
        assert_old_fur(gd, &fur_render(&g, &FurIn { b12: b12_with(|b| b[1][0] = 1.0), ..base }), "fur_rim 1");
    }
}

#[test]
fn fur_rim_1_is_the_old_outline_exactly_and_the_new_default_is_subtler() {
    let Some(g) = gpu() else { return };
    for gd in &OLD_FUR_GOLDEN {
        if !matches!(gd.scene, "D" | "E") {
            continue;
        }
        let base = old_fur_scene(gd.scene);
        // The mask ends at x = 32. With fur_rim 1 the outline is what the old shader drew.
        let one = fur_render(&g, &FurIn { b12: b12_with(|b| b[1][0] = 1.0), ..base.clone() });
        assert_old_fur(gd, &one, "fur_rim 1");
        // The default 0.25: everything away from the outline (more than 4 px inside, or outside the mask) is the old picture, the outline is not.
        let dflt = fur_render(&g, &base);
        let mut edge_diff = 0.0f32;
        for (n, &(x, y)) in OLD_FUR_PTS.iter().enumerate() {
            let p = px(&dflt, x, y);
            let near_edge = (28..32).contains(&x);
            for k in 0..3 {
                let d = (p[k] - gd.px[n][k]).abs();
                if near_edge {
                    edge_diff = edge_diff.max(d);
                } else {
                    assert!(d <= 0.0016, "scene {} pixel ({x},{y}) channel {k} is away from the outline and must equal the old one: {} vs {}", gd.scene, p[k], gd.px[n][k]);
                }
            }
        }
        println!("scene {}: the default outline differs from the old one by up to {edge_diff:.4}", gd.scene);
        assert!(edge_diff > 0.004, "scene {}: the default (0.25) outline must be visibly subtler than the old one: {edge_diff}", gd.scene);
        // The outline of 1 is stronger than the default one at every pixel of the band, and no outline at 0 is the weakest.
        let zero = fur_render(&g, &FurIn { b12: b12_with(|b| b[1][0] = 0.0), ..base.clone() });
        for y in [16usize, 32, 48] {
            let (o1, od, o0) = (px(&one, 31, y)[1], px(&dflt, 31, y)[1], px(&zero, 31, y)[1]);
            assert!((o1 - o0).abs() > (od - o0).abs(), "scene {} row {y}: rim 1 {o1} vs default {od} vs none {o0}", gd.scene);
        }
    }
}

#[test]
fn unbound_b12_is_the_default_look_bit_for_bit_and_the_live_flag_gates_everything() {
    let Some(g) = gpu() else { return };
    for name in ["A", "B", "D", "E", "F"] {
        let base = old_fur_scene(name);
        let unbound = fur_render(&g, &base);
        let bound = fur_render(&g, &FurIn { b12: Some(B12_DEFAULTS), ..base.clone() });
        assert_eq!(max_diff(&unbound, &bound), 0.0, "scene {name}: an unbound b12 must be bit-identical to the defaults bound");
        // The live flag (the last component) at 0 = unbound, whatever the other numbers and the view say.
        let flagged_off = fur_render(&g, &FurIn { b12: Some([[0.9, 3.0, 9.0, 2.0], [2.0, 2.0, 4.0, 0.0]]), ..base.clone() });
        assert_eq!(max_diff(&unbound, &flagged_off), 0.0, "scene {name}: live flag 0 = unbound (also no view)");
        // Different values do change the picture (so the equality above is not vacuous).
        let changed = fur_render(&g, &FurIn { b12: b12v(0.8, 2.0, 9.0, 2.0, 1.0, 2.0, 0.0), ..base });
        assert!(max_diff(&unbound, &changed) > 0.01, "scene {name}: the live values must reach the look");
    }
    // Strength 0 stays a bit-identical no-op with any b12 (non-default values, and a view that is not asked for).
    let t0 = random_texels(5, 0.0, 1.0);
    let plain = fur_render(&g, &FurIn { fur: 0.0, mask: None, ..FurIn::new(t0.clone()) });
    for b in [None, Some(B12_DEFAULTS), b12v(1.0, 4.0, 12.0, 2.0, 2.0, 2.0, 0.0)] {
        let zero = fur_render(&g, &FurIn { fur: 0.0, b12: b, ..FurIn::new(t0.clone()) });
        assert_eq!(max_diff(&plain, &zero), 0.0, "strength 0 with b12 {b:?} must be bit-identical to no fur");
    }
}

#[test]
fn fur_rim_zero_removes_the_glow_and_the_feather_and_scales_the_outline() {
    let Some(g) = gpu() else { return };
    // A flat character on a flat picture, the mask ends at x = 32, no uv (no strands): with fur_rim 0 every masked pixel, the edge too, looks the same.
    let base = FurIn { mask: Some(mask_left()), ident: true, ..FurIn::new(uniform([0.5, 0.4, 0.3, 0.6])) };
    let with_rim = |rim: f32| fur_render(&g, &FurIn { b12: b12_with(|b| b[1][0] = rim), ..base.clone() });
    // The identity remap adds a +-0.002 dither that depends on the pixel: look at the change the fur made at each pixel.
    let plain = fur_render(&g, &FurIn { fur: 0.0, ..base.clone() });
    let delta = |v: &[[f32; 4]], x: usize, y: usize| px(v, x, y)[1] - px(&plain, x, y)[1];
    let gain = |v: &[[f32; 4]]| -> f32 {
        // the edge column against a column deep inside, green channel, over the rows that are not at the picture border
        (8..N - 8).map(|y| (delta(v, 31, y) - delta(v, 10, y)).abs()).fold(0.0, f32::max)
    };
    let none = with_rim(0.0);
    let g0 = gain(&none);
    assert!(g0 <= 0.0006, "fur_rim 0: no glow and no feather, the edge pixel changes like the interior: {g0}");
    // 0 = the outline is gone also one to five pixels in.
    for x in 26..32 {
        assert!((delta(&none, x, 32) - delta(&none, 10, 32)).abs() <= 0.0006, "x = {x}");
    }
    let mut prev = g0;
    for rim in [0.25f32, 0.5, 1.0, 2.0] {
        let gr = gain(&with_rim(rim));
        println!("fur_rim {rim}: edge against interior {gr:.4}");
        assert!(gr > prev, "the outline grows with fur_rim: {prev} -> {gr} at {rim}");
        prev = gr;
    }
    assert!(gain(&with_rim(0.25)) > 0.003 && gain(&with_rim(0.25)) < 0.5 * gain(&with_rim(1.0)), "the default is a subtle outline: {} vs {}", gain(&with_rim(0.25)), gain(&with_rim(1.0)));
    // The same with strands (a uv): the pixels at the edge equal the render of a mask without an edge (within the uv quantisation of the derivative).
    let strands = FurIn { uv: Some(uv_ramp(0.004, 0.004, 0.0)), b12: b12_with(|b| b[1][0] = 0.0), ..base.clone() };
    let edge = fur_render(&g, &strands);
    let full = fur_render(&g, &FurIn { mask: Some(mask_all(255)), ..strands });
    let mut worst = 0.0f32;
    for y in 8..N - 8 {
        for x in 24..32 {
            worst = worst.max((px(&edge, x, y)[1] - px(&full, x, y)[1]).abs());
        }
    }
    println!("fur_rim 0 with strands: edge against no edge, worst {worst:.4}");
    assert!(worst <= 0.012, "without the rim the outline must not show: {worst}");
}

/// Standard deviation of the green channel over the interior.
fn std_of(v: &[[f32; 4]]) -> f32 {
    interior_std(v)
}

/// Mean absolute difference of the green channel between pixels `(dx, dy)` apart, over the interior.
fn roughness(v: &[[f32; 4]], dx: usize, dy: usize) -> f32 {
    let mut s = 0.0f32;
    let mut n = 0.0f32;
    for y in 8..N - 8 - dy {
        for x in 8..N - 8 - dx {
            s += (px(v, x, y)[1] - px(v, x + dx, y + dy)[1]).abs();
            n += 1.0;
        }
    }
    s / n
}

/// A flat character with strands only: no texture detail, no outline, no warm glow (those have their own tests).
fn strands_only(amp: f32, scale: f32, len: f32) -> FurIn {
    FurIn { uv: Some(uv_ramp(0.004, 0.004, 0.0)), ident: true, b12: b12v(amp, scale, len, 0.0, 0.0, 0.0, 0.0), ..FurIn::new(uniform([0.5, 0.4, 0.3, 0.6])) }
}

#[test]
fn fur_amp_scales_the_strands_and_zero_removes_them() {
    let Some(g) = gpu() else { return };
    let off = fur_render(&g, &FurIn { fur: 0.0, ..strands_only(0.32, 1.0, 5.0) });
    let sd = |amp: f32| std_of(&fur_render(&g, &strands_only(amp, 1.0, 5.0)));
    let (s0, s16, s32, s64) = (sd(0.0), sd(0.16), sd(0.32), sd(0.64));
    println!("fur_amp: std {s0:.4} (0), {s16:.4} (0.16), {s32:.4} (0.32), {s64:.4} (0.64); strength 0: {:.4}", std_of(&off));
    assert!(s0 <= 0.003, "amplitude 0 = no strands: {s0}");
    assert!(max_diff(&fur_render(&g, &strands_only(0.0, 1.0, 5.0)), &off) <= 0.002, "amplitude 0 is the picture without the fur");
    assert!(s32 > 0.02, "the default strands are visible: {s32}");
    assert!((s16 / s32 - 0.5).abs() < 0.08, "half the amplitude, half the modulation: {}", s16 / s32);
    // (a little less than double: the strands never darken a pixel below 0.7x, see k in fur_look)
    assert!(s64 / s32 > 1.45 && s64 / s32 < 2.1, "double the amplitude, about double the modulation: {}", s64 / s32);
}

#[test]
fn fur_scale_and_length_change_the_size_and_the_direction_of_the_strands() {
    let Some(g) = gpu() else { return };
    let r = |scale: f32, len: f32| {
        let o = fur_render(&g, &strands_only(0.32, scale, len));
        (roughness(&o, 1, 0), roughness(&o, 0, 1), std_of(&o))
    };
    let (small, normal, big) = (r(0.25, 5.0), r(1.0, 5.0), r(4.0, 5.0));
    println!("fur_scale: horizontal / vertical roughness, std: 0.25 -> {small:.4?}, 1 -> {normal:.4?}, 4 -> {big:.4?}");
    // Larger cells = a smoother pattern in both directions; smaller cells = a finer one.
    assert!(big.0 < normal.0 && normal.0 < small.0, "horizontal: {} < {} < {}", big.0, normal.0, small.0);
    assert!(big.1 < normal.1 && normal.1 < small.1, "vertical: {} < {} < {}", big.1, normal.1, small.1);
    // The length stretches the cells along v: smoother vertically, the horizontal grain stays.
    let (short, long) = (r(1.0, 1.0), r(1.0, 12.0));
    println!("fur_length: horizontal / vertical roughness, std: 1 -> {short:.4?}, 12 -> {long:.4?}");
    assert!(long.1 < short.1 * 0.5, "longer strands are much smoother along v: {} vs {}", long.1, short.1);
    assert!(long.0 > 0.01, "while the grain across them stays: {}", long.0);
    // Everything stays a bounded modulation of the picture.
    for o in [small, normal, big, short, long] {
        assert!(o.2 > 0.005 && o.2 < 0.2, "{o:?}");
    }
}

#[test]
fn fur_detail_scales_the_texture_contrast_and_zero_removes_it() {
    let Some(g) = gpu() else { return };
    // Painted strands in the texture (1 px stripes), no uv: only the detail term acts on the contrast (rim 0, sss 0 isolate it).
    let base = FurIn { ident: true, ..FurIn::new(stripes(0.5, 0.4)) };
    let contrast = |v: &[[f32; 4]]| (4..N - 4).map(|x| (px(v, x, 20)[1] - px(v, x + 1, 20)[1]).abs()).sum::<f32>();
    let off = contrast(&fur_render(&g, &FurIn { fur: 0.0, ..base.clone() }));
    let at = |detail: f32| contrast(&fur_render(&g, &FurIn { b12: b12v(0.32, 1.0, 5.0, detail, 0.0, 0.0, 0.0), ..base.clone() }));
    let (c0, c1, c2) = (at(0.0), at(1.0), at(2.0));
    println!("fur_detail: contrast {off:.3} (fur off), {c0:.3} (0), {c1:.3} (1), {c2:.3} (2)");
    assert!((c0 / off - 1.0).abs() < 0.03, "detail 0: no unsharp mask: {c0} vs {off}");
    assert!(c1 > c0 * 1.05, "{c0} -> {c1}");
    assert!(c2 > c1 * 1.03, "more detail, more contrast: {c1} -> {c2}");
    assert!(c2 < off * 2.5, "bounded by the +-15 % clamp of the ratio (at most 1.15 / 0.85 more contrast per step on top): {c2} vs {off}");
}

#[test]
fn fur_sss_scales_the_warm_glow_and_zero_removes_it() {
    let Some(g) = gpu() else { return };
    let base = FurIn { ident: true, ..FurIn::new(uniform([0.5, 0.4, 0.3, 0.6])) };
    let off = fur_render(&g, &FurIn { fur: 0.0, ..base.clone() });
    let at = |sss: f32| fur_render(&g, &FurIn { b12: b12v(0.32, 1.0, 5.0, 1.0, 0.0, sss, 0.0), ..base.clone() });
    let (s0, s1, s2) = (at(0.0), at(1.0), at(2.0));
    let dr = |v: &[[f32; 4]]| px(v, 30, 30)[0] - px(&off, 30, 30)[0];
    let db = |v: &[[f32; 4]]| px(v, 30, 30)[2] - px(&off, 30, 30)[2];
    println!("fur_sss: red gain {:.4} (0), {:.4} (1), {:.4} (2); blue gain {:.4} (1)", dr(&s0), dr(&s1), dr(&s2), db(&s1));
    assert!(dr(&s0).abs() <= 0.002, "sss 0: nothing is added: {}", dr(&s0));
    assert!(dr(&s1) > 0.01, "warm: red goes up: {}", dr(&s1));
    assert!(dr(&s1) > db(&s1) * 3.0, "and mostly red, not blue: {} vs {}", dr(&s1), db(&s1));
    assert!((dr(&s2) / dr(&s1) - 2.0).abs() < 0.2, "twice the value, about twice the glow: {}", dr(&s2) / dr(&s1));
}

#[test]
fn fur_view_1_paints_the_mesh_uv_and_solid_red_where_there_is_none() {
    let Some(g) = gpu() else { return };
    let ramp = uv_ramp(0.004, 0.004, 0.0);
    // The view works with the fur strength at 0 and needs no b12 values besides the view.
    let base = FurIn { fur: 0.0, uv: Some(ramp), ident: true, b12: b12v(0.32, 1.0, 5.0, 1.0, 0.25, 1.0, 1.0), ..FurIn::new(uniform([0.5, 0.4, 0.3, 0.6])) };
    let out = fur_render(&g, &base);
    for (x, y) in [(5usize, 5usize), (20, 40), (32, 32), (50, 10), (63, 63)] {
        let (u, v) = (0.25 + 0.004 * x as f32, 0.25 + 0.004 * y as f32);
        let p = px(&out, x, y);
        assert!((p[0] - u).abs() < 0.002 && (p[1] - v).abs() < 0.002 && (p[2] - 0.35).abs() < 0.002, "({x},{y}) uv ({u},{v}): {p:?}");
        assert!((p[3] - 0.6).abs() < 0.001, "the alpha of the scene is kept: {}", p[3]);
    }
    assert!(out.iter().all(|p| p[2] > 0.3), "every pixel with a valid uv has the blue tint, none is pure red");
    // The uv wraps like the mask stores it (frac).
    let wrapped = fur_render(&g, &FurIn { uv: Some((0..N * N).map(|i| [1.3 + 0.01 * (i % N) as f32, 2.7]).collect()), ..base.clone() });
    let p = px(&wrapped, 0, 0);
    assert!((p[0] - 0.3).abs() < 0.002 && (p[1] - 0.7).abs() < 0.002, "{p:?}");
    // A mask pixel WITHOUT a valid uv (the draw's vertex shader had none): solid red.
    let no_uv = fur_render(&g, &FurIn { uv: None, ..base.clone() });
    assert!(no_uv.iter().all(|p| p[..3] == [1.0, 0.0, 0.0]), "solid red where the mask has no uv");
    // Only the mask pixels: outside the mask the picture is the plain one.
    let half = fur_render(&g, &FurIn { mask: Some(mask_left()), ..base.clone() });
    let plain = fur_render(&g, &FurIn { fur: 0.0, mask: None, b12: None, ..base.clone() });
    for y in 0..N {
        for x in 0..N {
            if x >= N / 2 {
                assert_eq!(px(&half, x, y), px(&plain, x, y), "({x},{y}) is outside the mask");
            } else {
                assert!(px(&half, x, y)[2] > 0.3, "({x},{y}) is inside the mask");
            }
        }
    }
    // No mask bound at all: nothing is painted.
    let unmasked = fur_render(&g, &FurIn { mask: None, ..base.clone() });
    assert_eq!(max_diff(&unmasked, &plain), 0.0);
    // The view is the final colour: the exposure remap of the game (t1) and the dither do not touch it.
    let game = fur_render(&g, &FurIn { ident: false, ..base });
    assert_eq!(max_diff(&game, &out), 0.0, "the view is written as it is, whatever the remap");
}

#[test]
fn fur_view_2_paints_the_strand_pattern_as_grey_and_matches_the_real_modulation() {
    let Some(g) = gpu() else { return };
    // A grey picture: the strand tint is neutral in luminance only for a grey pixel, so the luminance factor is exactly k.
    let scene = uniform([0.4, 0.4, 0.4, 0.6]);
    let view = fur_render(&g, &FurIn { fur: 0.0, uv: Some(uv_ramp(0.004, 0.004, 0.0)), ident: true, b12: b12v(0.32, 1.0, 5.0, 1.0, 0.25, 1.0, 2.0), ..FurIn::new(scene.clone()) });
    // Grey: the three channels are the same value, 0.5 + 0.5 sv sfade, between 0 and 1 and varying.
    for p in &view {
        assert!(p[0] == p[1] && p[1] == p[2], "{p:?}");
        assert!((0.0..=1.0).contains(&p[0]));
    }
    let sd = interior_std(&view);
    assert!(sd > 0.05, "the pattern is visible: {sd}");
    // The strands the look really draws (amplitude 0.32, nothing else): luminance factor k = 1 + 0.32 sv sfade, i.e. sv sfade = 2 (grey - 0.5).
    let plain = fur_render(&g, &FurIn { fur: 0.0, uv: Some(uv_ramp(0.004, 0.004, 0.0)), ident: true, ..FurIn::new(scene.clone()) });
    let real = fur_render(&g, &FurIn { uv: Some(uv_ramp(0.004, 0.004, 0.0)), ident: true, b12: b12v(0.32, 1.0, 5.0, 0.0, 0.0, 0.0, 0.0), ..FurIn::new(scene.clone()) });
    let lum = |p: [f32; 4]| 0.299 * p[0] + 0.587 * p[1] + 0.114 * p[2];
    let mut worst = 0.0f32;
    for y in 8..N - 8 {
        for x in 8..N - 8 {
            let k = lum(px(&real, x, y)) / lum(px(&plain, x, y));
            let svf = (k - 1.0) / 0.32;
            let want = 2.0 * (px(&view, x, y)[0] - 0.5);
            if want < -0.9 {
                continue; // the look never darkens a pixel below 0.7x (k >= 0.7): the deepest valleys are cut there, the view shows the pattern itself
            }
            worst = worst.max((svf - want).abs());
        }
    }
    println!("fur_view 2 against the real modulation: worst {worst:.4}");
    assert!(worst < 0.02, "the grey view is the pattern the look applies: {worst}");
    // It follows fur_scale and fur_length: a different size is a different picture.
    let big = fur_render(&g, &FurIn { fur: 0.0, uv: Some(uv_ramp(0.004, 0.004, 0.0)), ident: true, b12: b12v(0.32, 4.0, 5.0, 1.0, 0.25, 1.0, 2.0), ..FurIn::new(scene.clone()) });
    assert!(roughness(&big, 1, 0) < roughness(&view, 1, 0));
    // Flat uv and no uv: 0.5 grey (no pattern).
    let flat = fur_render(&g, &FurIn { fur: 0.0, uv: Some(vec![[0.37, 0.61]; N * N]), ident: true, b12: b12v(0.32, 1.0, 5.0, 1.0, 0.25, 1.0, 2.0), ..FurIn::new(scene.clone()) });
    let none = fur_render(&g, &FurIn { fur: 0.0, uv: None, ident: true, b12: b12v(0.32, 1.0, 5.0, 1.0, 0.25, 1.0, 2.0), ..FurIn::new(scene) });
    for o in [&flat, &none] {
        assert!(o.iter().all(|p| p[..3] == [0.5, 0.5, 0.5]), "no pattern = mid grey");
    }
}

#[test]
fn fur_view_3_paints_the_rim_band_and_view_4_the_strand_fade() {
    let Some(g) = gpu() else { return };
    let scene = uniform([0.5, 0.4, 0.3, 0.6]);
    // View 3: the band along the mask edge (x = 32), without a uv: 0.4, 0.3, 0.2, 0.1 at 1..4 px, nothing deeper, white = strong.
    let band = fur_render(&g, &FurIn { fur: 0.0, mask: Some(mask_left()), ident: true, b12: b12v(0.32, 1.0, 5.0, 1.0, 0.25, 1.0, 3.0), ..FurIn::new(scene.clone()) });
    let want = [(31usize, 0.4f32), (30, 0.3), (29, 0.2), (28, 0.1), (27, 0.0), (20, 0.0), (10, 0.0)];
    for (x, w) in want {
        let p = px(&band, x, 32);
        assert!(p[0] == p[1] && p[1] == p[2], "grey: {p:?}");
        assert!((p[0] - w).abs() < 0.004, "x = {x}: band {} vs {w}", p[0]);
    }
    // The band does not depend on the strength of the outline: fur_rim 0 and 2 show the same band.
    for rim in [0.0f32, 2.0] {
        let o = fur_render(&g, &FurIn { fur: 0.0, mask: Some(mask_left()), ident: true, b12: b12v(0.32, 1.0, 5.0, 1.0, rim, 1.0, 3.0), ..FurIn::new(scene.clone()) });
        assert_eq!(max_diff(&o, &band), 0.0, "rim {rim}");
    }
    // With a uv the strands break the fringe up: between 0.55 and 1.45 of the plain band.
    let fringe = fur_render(&g, &FurIn { fur: 0.0, mask: Some(mask_left()), uv: Some(uv_ramp(0.004, 0.004, 0.0)), ident: true, b12: b12v(0.32, 1.0, 5.0, 1.0, 0.25, 1.0, 3.0), ..FurIn::new(scene.clone()) });
    let mut seen = std::collections::BTreeSet::new();
    for y in 8..N - 8 {
        let v = px(&fringe, 31, y)[0];
        assert!(v >= 0.4 * 0.55 - 0.01 && v <= 0.4 * 1.45 + 0.01, "y = {y}: {v}");
        seen.insert((v * 1000.0) as i32);
    }
    assert!(seen.len() > 8, "the fringe is irregular: {}", seen.len());
    // Outside the mask nothing is painted.
    let plain = fur_render(&g, &FurIn { fur: 0.0, mask: None, ident: true, ..FurIn::new(scene.clone()) });
    for y in 0..N {
        assert_eq!(px(&band, 40, y), px(&plain, 40, y));
    }
    // View 4: the fade. White where the strands are fully there, black where they are faded out or there is nothing to anchor to.
    let fade = |uv: Option<Vec<[f32; 2]>>| fur_render(&g, &FurIn { fur: 0.0, uv, ident: true, b12: b12v(0.32, 1.0, 5.0, 1.0, 0.25, 1.0, 4.0), ..FurIn::new(scene.clone()) });
    let at = |o: &[[f32; 4]]| px(o, 32, 32);
    assert!(at(&fade(Some(uv_ramp(0.004, 0.004, 0.0))))[..3].iter().all(|c| (*c - 1.0).abs() < 0.003), "close: full pattern");
    let mid = at(&fade(Some(uv_ramp(0.04, 0.04, 0.0))));
    assert!((mid[0] - 0.896).abs() < 0.02 && mid[0] == mid[1] && mid[1] == mid[2], "a fast uv fades: {mid:?}");
    assert!(at(&fade(Some(uv_ramp(0.2, 0.2, 0.0))))[..3].iter().all(|c| *c < 0.003), "far: faded out");
    assert!(at(&fade(Some(vec![[0.37, 0.61]; N * N])))[..3].iter().all(|c| *c < 0.003), "flat uv: nothing to anchor to");
    assert!(at(&fade(None))[..3].iter().all(|c| *c == 0.0), "no uv: black");
}

#[test]
fn fur_views_work_without_strength_on_dark_and_clipped_pixels_and_leave_the_rest_alone() {
    let Some(g) = gpu() else { return };
    for view in 1..=4 {
        let ramp = Some(uv_ramp(0.004, 0.004, 0.0));
        let b = b12v(0.32, 1.0, 5.0, 1.0, 0.25, 1.0, view as f32);
        // Dark and almost clipped pixels are released by the look (gate 0), the view paints them all the same.
        for c in [[0.02, 0.02, 0.02, 0.6], [0.999, 0.999, 0.999, 0.6], [0.5, 0.4, 0.3, 0.6]] {
            let on = fur_render(&g, &FurIn { fur: 0.0, uv: ramp.clone(), b12: b, mask: Some(mask_left()), ident: true, ..FurIn::new(uniform(c)) });
            let off = fur_render(&g, &FurIn { fur: 0.0, uv: ramp.clone(), mask: Some(mask_left()), ident: true, ..FurIn::new(uniform(c)) });
            assert_ne!(px(&on, 20, 32)[..3], px(&off, 20, 32)[..3], "view {view} on {c:?} must paint the mask");
            // the outside of the mask is the plain picture
            for y in [0usize, 20, 40, 63] {
                assert_eq!(px(&on, 50, y), px(&off, 50, y), "view {view}: outside the mask");
            }
            assert!(finite_nonneg(&on));
        }
        // b12 unbound = no view = no change, even with the strength at 1.
        let unbound = fur_render(&g, &FurIn { uv: ramp.clone(), mask: Some(mask_left()), ..FurIn::new(uniform([0.5, 0.4, 0.3, 0.6])) });
        let defaults = fur_render(&g, &FurIn { uv: ramp.clone(), mask: Some(mask_left()), b12: Some(B12_DEFAULTS), ..FurIn::new(uniform([0.5, 0.4, 0.3, 0.6])) });
        assert_eq!(max_diff(&unbound, &defaults), 0.0);
    }
    // The magenta selection stays what it was: painted over the view with the same 80 %.
    let ramp = Some(uv_ramp(0.004, 0.004, 0.0));
    let both = fur_render(&g, &FurIn { fur: 0.0, debug: 1.0, uv: ramp, mask: Some(mask_left()), ident: true, b12: b12v(0.32, 1.0, 5.0, 1.0, 0.25, 1.0, 1.0), ..FurIn::new(uniform([0.5, 0.4, 0.3, 0.6])) });
    let p = px(&both, 20, 32);
    let (u, v) = (0.25 + 0.004 * 20.0, 0.25 + 0.004 * 32.0);
    assert!((p[0] - (0.2 * u + 0.8)).abs() < 0.003 && (p[1] - 0.2 * v).abs() < 0.003 && (p[2] - (0.2 * 0.35 + 0.8)).abs() < 0.003, "{p:?}");
}

#[test]
fn fur_look_with_garbage_in_b12_never_gives_nan_or_negative_output() {
    let Some(g) = gpu() else { return };
    let nan = f32::NAN;
    let garbage: [[[f32; 4]; 2]; 6] = [
        [[nan, nan, nan, nan], [nan, nan, nan, 1.0]],
        [[f32::INFINITY, f32::NEG_INFINITY, 1e30, -1e30], [1e30, -5.0, 77.0, 1.0]],
        [[-1.0, -1.0, -1.0, -1.0], [-1.0, -1.0, -1.0, 1.0]],
        [[1e38; 4], [1e38, 1e38, 3.0, 1.0]],
        [[0.0; 4], [0.0, 0.0, 0.0, 1.0]],
        [[nan; 4], [nan, nan, nan, nan]],
    ];
    for (i, b) in garbage.into_iter().enumerate() {
        for (scene, mask, uv) in [(stripes(0.5, 0.4), mask_left(), Some(uv_ramp(0.004, 0.004, 0.0))), (stripes(3.0, 0.5), mask_all(255), None), (uniform([0.02, 0.02, 0.02, 0.6]), mask_all(255), Some(uv_ramp(0.01, 0.01, 0.0)))] {
            for fur in [0.0f32, 1.0] {
                let out = fur_render(&g, &FurIn { fur, mask: Some(mask.clone()), uv: uv.clone(), b12: Some(b), ..FurIn::new(scene.clone()) });
                assert!(finite_nonneg(&out), "garbage b12 #{i} (fur {fur}) gave NaN or a negative value");
            }
        }
    }
    // And the defaults stay inside the bounds of the old look for the extreme textures too.
    for (a, b) in [(0.5f32, 0.4f32), (0.9, 0.2), (0.05, 0.9), (3.0, 0.5)] {
        let off = fur_render(&g, &FurIn { fur: 0.0, ..FurIn::new(stripes(a, b)) });
        for extreme in [b12v(1.0, 4.0, 12.0, 2.0, 2.0, 2.0, 0.0), b12v(1.0, 0.25, 1.0, 2.0, 2.0, 2.0, 0.0)] {
            let on = fur_render(&g, &FurIn { uv: Some(uv_ramp(0.004, 0.004, 0.0)), b12: extreme, ..FurIn::new(stripes(a, b)) });
            assert!(finite_nonneg(&on), "{a},{b}");
            for (p, q) in on.iter().zip(off.iter()) {
                for k in 0..3 {
                    // the limits of the live parameters: the modulation is capped (k in 0.7..2), the detail ratio clamped, the glow bounded
                    assert!(p[k] <= q[k] * 2.2 + 0.6, "{a},{b}: {} vs {}", p[k], q[k]);
                }
            }
        }
    }
}

// HDR output audit: decode the final output back to nits; see the file.
#[path = "hdr10_audit_tests.rs"]
mod hdr10_audit;
