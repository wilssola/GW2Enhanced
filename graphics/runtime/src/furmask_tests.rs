//! Tests of the fur selector (pure) and of the mask draw on a WARP device: a synthetic "opaque pass" (colour + depth, an
//! occluder drawn first) and a fur draw re-issued into the mask through `Mask::draw`, exactly as the Draw* hooks do.

use super::*;

use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::Interface;

// ---- selector --------------------------------------------------------------------------------------------

#[test]
fn parse_hashes_accepts_hex_lists_and_skips_garbage() {
    assert_eq!(parse_hashes(""), Vec::<u32>::new());
    assert_eq!(parse_hashes("535b203b"), vec![0x535b203b]);
    assert_eq!(parse_hashes("535B203B, 1c8a2482 ,cf595318"), vec![0x535b203b, 0x1c8a2482, 0xcf595318]);
    assert_eq!(parse_hashes("0x1c8a2482;ad00c56f"), vec![0x1c8a2482, 0xad00c56f]);
    // Short hashes are numbers too; too long, non-hex and empty entries are skipped; duplicates dropped.
    assert_eq!(parse_hashes("ff,,zzzz,123456789,535b203b,535b203b,0x"), vec![0xff, 0x535b203b]);
    // Capped.
    let many: Vec<String> = (0..100u32).map(|i| format!("{:08x}", i + 1)).collect();
    assert_eq!(parse_hashes(&many.join(",")).len(), MAX_LIST);
}

#[test]
fn selector_needs_ps_in_the_list_vs_if_given_and_a_big_enough_vertex_cb() {
    let s = Selector::new("535b203b,1c8a2482", "", 4096);
    assert!(s.matches(0x535b203b, Some(0x130e17de), 16464));
    assert!(s.matches(0x1c8a2482, None, 4096), "any VS when the list is empty, also an unknown one; the limit is inclusive");
    assert!(!s.matches(0x1c8a2482, Some(1), 4095), "vertex cb 0 smaller than fur_min_vcb: not skinned");
    assert!(!s.matches(0xdeadbeef, Some(0x130e17de), 16464), "pixel shader not in the list");
    // A vertex-shader list restricts further; an unknown VS then fails.
    let s = Selector::new("535b203b", "130e17de, 0d12c4bb", 4096);
    assert!(s.matches(0x535b203b, Some(0x0d12c4bb), 16496));
    assert!(!s.matches(0x535b203b, Some(0xedcb8734), 16496));
    assert!(!s.matches(0x535b203b, None, 16496));
    // Empty ps list = nothing, whatever the rest says; min 0 accepts any cb.
    let s = Selector::new("", "", 0);
    assert!(s.is_empty() && !s.matches(0x535b203b, None, 99999));
    assert!(Selector::new("1", "", 0).matches(1, None, 0));
    // The limit is clamped.
    assert_eq!(Selector::new("1", "", u32::MAX).min_vcb, MAX_VCB);
}

#[test]
fn configure_sets_the_live_state_and_the_text() {    let _g = crate::testlock::lock();

    // Touches the globals (the draw tests below use Mask directly); holds the global test lock.
    configure("535B203B,xx,1c8a2482", "130e17de", 8192, true);
    assert_eq!(selector_text(), ("535b203b,1c8a2482".to_string(), "130e17de".to_string()));
    assert_eq!(min_vcb(), 8192);
    assert!(debug());
    assert!(NONEMPTY.load(Relaxed));
    assert!(status_text().contains("draws masked") || status_text().contains("compiling"), "{}", status_text());
    configure("", "", 4096, false);
    assert!(!NONEMPTY.load(Relaxed) && !MATCH.load(Relaxed));
    assert_eq!(status_text(), "off (fur_ps is empty)");
    assert_eq!(min_vcb(), 4096);
}

// ---- WARP -----------------------------------------------------------------------------------------------

const N: u32 = 64;

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

fn mask_blob() -> Vec<u8> {
    shaders::compile(HLSL, PS_ENTRY, "ps_5_0").unwrap_or_else(|e| panic!("furmask.hlsl does not compile: {e}"))
}

/// Quad from a rect in clip space (x0, y0, x1, y1) at depth z (a triangle strip of 4 vertices from SV_VertexID).
const VS_QUAD: &str = "cbuffer c : register(b0) { float4 r; float4 zz; };
float4 main(uint id : SV_VertexID) : SV_Position { float2 u = float2(id & 1, id >> 1); float2 p = lerp(r.xy, r.zw, u); return float4(p, zz.x, 1); }";
/// The "engine" pixel shader: opaque red.
const PS_RED: &str = "float4 main() : SV_Target { return float4(1, 0, 0, 1); }";

fn cb_rect(g: &Gpu, rect: [f32; 4], z: f32) -> ID3D11Buffer {
    let data = [rect[0], rect[1], rect[2], rect[3], z, 0.0, 0.0, 0.0];
    let bytes: Vec<u8> = data.iter().flat_map(|c| c.to_le_bytes()).collect();
    let d = D3D11_BUFFER_DESC { ByteWidth: 32, Usage: D3D11_USAGE_IMMUTABLE, BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32, ..Default::default() };
    let sd = D3D11_SUBRESOURCE_DATA { pSysMem: bytes.as_ptr() as *const _, SysMemPitch: 0, SysMemSlicePitch: 0 };
    let mut b = None;
    unsafe { g.dev.CreateBuffer(&d, Some(&sd), Some(&mut b)).unwrap() };
    b.unwrap()
}

fn tex2d(g: &Gpu, fmt: DXGI_FORMAT, bind: D3D11_BIND_FLAG, usage: D3D11_USAGE, cpu: u32) -> ID3D11Texture2D {
    let desc = D3D11_TEXTURE2D_DESC { Width: N, Height: N, MipLevels: 1, ArraySize: 1, Format: fmt, SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, Usage: usage, BindFlags: bind.0 as u32, CPUAccessFlags: cpu, MiscFlags: 0 };
    let mut t = None;
    unsafe { g.dev.CreateTexture2D(&desc, None, Some(&mut t)).unwrap() };
    t.unwrap()
}

fn read_back(g: &Gpu, src: &ID3D11Texture2D, fmt: DXGI_FORMAT, bpp: usize) -> Vec<u8> {
    let st = tex2d(g, fmt, D3D11_BIND_FLAG(0), D3D11_USAGE_STAGING, D3D11_CPU_ACCESS_READ.0 as u32);
    unsafe {
        g.ctx.CopyResource(&st, src);
        let mut m = D3D11_MAPPED_SUBRESOURCE::default();
        g.ctx.Map(&st, 0, D3D11_MAP_READ, 0, Some(&mut m)).unwrap();
        let mut out = Vec::new();
        for y in 0..N as usize {
            out.extend_from_slice(std::slice::from_raw_parts((m.pData as *const u8).add(y * m.RowPitch as usize), N as usize * bpp));
        }
        g.ctx.Unmap(&st, 0);
        out
    }
}

struct Scene {
    g: Gpu,
    color: ID3D11Texture2D,
    rtv: ID3D11RenderTargetView,
    dsv: ID3D11DepthStencilView,
    dss: ID3D11DepthStencilState,
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
}

/// The depth buffer of a test scene.
#[derive(Clone, Copy, PartialEq)]
enum Depth {
    /// Like the engine's: R24G8_TYPELESS with DEPTH_STENCIL | SHADER_RESOURCE, viewed as D24_UNORM_S8_UINT.
    Engine,
    /// Typed D24_UNORM_S8_UINT, depth-stencil bind only: no shader can read it.
    Typed,
    /// R24G8_TYPELESS without the SHADER_RESOURCE bind flag.
    NoSrvFlag,
}

impl Scene {
    fn new(g: Gpu) -> Scene {
        Scene::with(g, Depth::Engine, D3D11_COMPARISON_LESS)
    }

    /// `func` = the engine's depth test (GREATER / GREATER_EQUAL = reversed Z: the depth is cleared to 0 instead of 1).
    fn with(g: Gpu, kind: Depth, func: D3D11_COMPARISON_FUNC) -> Scene {
        unsafe {
            let color = tex2d(&g, DXGI_FORMAT_R8G8B8A8_UNORM, D3D11_BIND_RENDER_TARGET, D3D11_USAGE_DEFAULT, 0);
            let depth = match kind {
                Depth::Engine => tex2d(&g, DXGI_FORMAT_R24G8_TYPELESS, D3D11_BIND_FLAG(D3D11_BIND_DEPTH_STENCIL.0 | D3D11_BIND_SHADER_RESOURCE.0), D3D11_USAGE_DEFAULT, 0),
                Depth::Typed => tex2d(&g, DXGI_FORMAT_D24_UNORM_S8_UINT, D3D11_BIND_DEPTH_STENCIL, D3D11_USAGE_DEFAULT, 0),
                Depth::NoSrvFlag => tex2d(&g, DXGI_FORMAT_R24G8_TYPELESS, D3D11_BIND_DEPTH_STENCIL, D3D11_USAGE_DEFAULT, 0),
            };
            let mut rtv = None;
            g.dev.CreateRenderTargetView(&color, None, Some(&mut rtv)).unwrap();
            let mut dsv = None;
            let dvd = D3D11_DEPTH_STENCIL_VIEW_DESC { Format: DXGI_FORMAT_D24_UNORM_S8_UINT, ViewDimension: D3D11_DSV_DIMENSION_TEXTURE2D, Flags: 0, ..Default::default() };
            g.dev.CreateDepthStencilView(&depth, Some(&dvd), Some(&mut dsv)).unwrap();
            // The engine's state: depth test LESS (or `func`) with depth writes (a normal opaque pass).
            let dd = D3D11_DEPTH_STENCIL_DESC { DepthEnable: true.into(), DepthWriteMask: D3D11_DEPTH_WRITE_MASK_ALL, DepthFunc: func, ..Default::default() };
            let reversed = func == D3D11_COMPARISON_GREATER || func == D3D11_COMPARISON_GREATER_EQUAL;
            let mut dss = None;
            g.dev.CreateDepthStencilState(&dd, Some(&mut dss)).unwrap();
            let rd = D3D11_RASTERIZER_DESC { FillMode: D3D11_FILL_SOLID, CullMode: D3D11_CULL_NONE, DepthClipEnable: true.into(), ..Default::default() };
            let mut rs = None;
            g.dev.CreateRasterizerState(&rd, Some(&mut rs)).unwrap();
            g.ctx.RSSetState(rs.as_ref());
            let vsb = shaders::compile(VS_QUAD, "main", "vs_5_0").unwrap();
            let psb = shaders::compile(PS_RED, "main", "ps_5_0").unwrap();
            let mut vs = None;
            g.dev.CreateVertexShader(&vsb, None, Some(&mut vs)).unwrap();
            let mut ps = None;
            g.dev.CreatePixelShader(&psb, None, Some(&mut ps)).unwrap();
            let s = Scene { g, color, rtv: rtv.unwrap(), dsv: dsv.unwrap(), dss: dss.unwrap(), vs: vs.unwrap(), ps: ps.unwrap() };
            let c = &s.g.ctx;
            c.ClearRenderTargetView(&s.rtv, &[0.0, 0.0, 0.0, 0.0]);
            c.ClearDepthStencilView(&s.dsv, D3D11_CLEAR_DEPTH.0, if reversed { 0.0 } else { 1.0 }, 0);
            c.OMSetRenderTargets(Some(&[Some(s.rtv.clone())]), &s.dsv);
            c.OMSetDepthStencilState(&s.dss, 0);
            c.RSSetViewports(Some(&[D3D11_VIEWPORT { TopLeftX: 0.0, TopLeftY: 0.0, Width: N as f32, Height: N as f32, MinDepth: 0.0, MaxDepth: 1.0 }]));
            c.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
            c.VSSetShader(&s.vs, None);
            c.PSSetShader(&s.ps, None);
            s
        }
    }

    /// Binds the quad (clip-space rect, depth z) as the vertex constant buffer 0.
    fn quad(&self, rect: [f32; 4], z: f32) {
        let cb = cb_rect(&self.g, rect, z);
        unsafe { self.g.ctx.VSSetConstantBuffers(0, Some(&[Some(cb)])) };
    }

    fn draw(&self) {
        unsafe { self.g.ctx.Draw(4, 0) };
    }

    /// The mask channel (a) of the target, 0..=255.
    fn mask(&self, m: &Mask) -> Vec<u8> {
        self.texels(m).iter().map(|t| (t[3] >> 8) as u8).collect()
    }

    /// The R16G16B16A16_UNORM texels of the target: [u, v, valid, mask].
    fn texels(&self, m: &Mask) -> Vec<[u16; 4]> {
        let srv: ID3D11ShaderResourceView = unsafe { ID3D11ShaderResourceView::from_raw_borrowed(&m.srv()).unwrap().clone() };
        let res = unsafe { srv.GetResource().unwrap() };
        let tex: ID3D11Texture2D = res.cast().unwrap();
        let raw = read_back(&self.g, &tex, DXGI_FORMAT_R16G16B16A16_UNORM, 8);
        raw.chunks_exact(8).map(|c| [0, 2, 4, 6].map(|i| u16::from_le_bytes([c[i], c[i + 1]]))).collect()
    }
}

/// Mask value at pixel (x, y) of the N x N picture.
fn at(v: &[u8], x: u32, y: u32) -> u8 {
    v[(y * N + x) as usize]
}

#[test]
fn mask_draw_marks_visible_pixels_respects_the_depth_buffer_and_restores_the_state() {
    let Some(g) = gpu() else { return };
    let s = Scene::new(g);
    let blob = mask_blob();
    unsafe {
        let mut m = Mask::new(s.g.dev.as_raw(), &blob).expect("mask objects");
        // The "pass so far": an occluder in the lower right quadrant (clip y < 0 is the lower half), z = 0.2.
        s.quad([0.0, -1.0, 1.0, 0.0], 0.2);
        s.draw();
        // The fur draw: the whole screen at z = 0.5, behind the occluder.
        s.quad([-1.0, -1.0, 1.0, 1.0], 0.5);
        let ctx = s.g.ctx.clone();
        m.draw(s.g.dev.as_raw(), ctx.as_raw(), true, &|| ctx.Draw(4, 0), None).expect("mask draw");
        let v = s.mask(&m);
        assert_eq!(m.size(), (N, N), "the mask has the size of the depth buffer");
        assert_eq!(at(&v, 16, 32), 255, "visible left half is masked");
        assert_eq!(at(&v, 48, 8), 255, "visible top right is masked");
        assert_eq!(at(&v, 48, 48), 0, "behind the occluder (depth test) is not masked");
        assert_eq!(at(&v, 16, 56), 255, "lower left is visible");

        // The engine's state is back: targets, depth state, shader.
        let (mut rtvs, mut dsv) = ([None, None], None);
        ctx.OMGetRenderTargets(Some(&mut rtvs), Some(&mut dsv));
        assert_eq!(rtvs[0].as_ref().map(|r| r.as_raw()), Some(s.rtv.as_raw()), "RTV0 restored");
        assert_eq!(dsv.as_ref().map(|d| d.as_raw()), Some(s.dsv.as_raw()), "DSV restored");
        let (mut dss, mut sref) = (None, 0);
        ctx.OMGetDepthStencilState(Some(&mut dss), Some(&mut sref));
        assert_eq!(dss.as_ref().map(|d| d.as_raw()), Some(s.dss.as_raw()), "depth state restored");
        let mut ps = None;
        ctx.PSGetShader(&mut ps, None, None);
        assert_eq!(ps.as_ref().map(|p| p.as_raw()), Some(s.ps.as_raw()), "pixel shader restored");

        // The mask draw wrote neither colour nor depth: the real draw of the same quad (z = 0.5, LESS) still passes
        // everywhere the occluder is not, and paints the engine colour there.
        let before = read_back(&s.g, &s.color, DXGI_FORMAT_R8G8B8A8_UNORM, 4);
        assert_eq!(before[(((32 * N) + 16) * 4) as usize], 0, "the colour target is untouched by the mask draw");
        s.draw();
        let after = read_back(&s.g, &s.color, DXGI_FORMAT_R8G8B8A8_UNORM, 4);
        assert_eq!(after[(((32 * N) + 16) * 4) as usize], 255, "depth was not written by the mask draw (the real draw passes LESS)");
        assert_eq!(after[(((48 * N) + 48) * 4) as usize], 255, "the occluder's own red is there");
    }
}

#[test]
fn mask_draws_accumulate_within_a_frame_and_the_next_frame_clears() {
    let Some(g) = gpu() else { return };
    let s = Scene::new(g);
    let blob = mask_blob();
    unsafe {
        let mut m = Mask::new(s.g.dev.as_raw(), &blob).expect("mask objects");
        let ctx = s.g.ctx.clone();
        // Two fur draws: left strip, then right strip (the second without clearing).
        s.quad([-1.0, -1.0, -0.5, 1.0], 0.5);
        m.draw(s.g.dev.as_raw(), ctx.as_raw(), true, &|| ctx.Draw(4, 0), None).unwrap();
        s.quad([0.5, -1.0, 1.0, 1.0], 0.5);
        m.draw(s.g.dev.as_raw(), ctx.as_raw(), false, &|| ctx.Draw(4, 0), None).unwrap();
        let v = s.mask(&m);
        assert_eq!((at(&v, 4, 32), at(&v, 60, 32), at(&v, 32, 32)), (255, 255, 0), "union of the two draws, middle empty");
        // A new frame: the first draw clears what the last frame left.
        s.quad([-1.0, -1.0, -0.5, 1.0], 0.9);
        m.draw(s.g.dev.as_raw(), ctx.as_raw(), true, &|| ctx.Draw(4, 0), None).unwrap();
        let v = s.mask(&m);
        assert_eq!((at(&v, 4, 32), at(&v, 60, 32)), (255, 0), "cleared, only the new draw");
        // The depth-state cache: same engine state, one state object.
        assert_eq!(m.states.len(), 1);
    }
}

#[test]
fn mask_draw_without_a_depth_buffer_is_refused_and_leaves_the_state_alone() {
    let Some(g) = gpu() else { return };
    let s = Scene::new(g);
    let blob = mask_blob();
    unsafe {
        let mut m = Mask::new(s.g.dev.as_raw(), &blob).expect("mask objects");
        let ctx = s.g.ctx.clone();
        ctx.OMSetRenderTargets(Some(&[Some(s.rtv.clone())]), None::<&ID3D11DepthStencilView>);
        let called = std::cell::Cell::new(false);
        let r = m.draw(s.g.dev.as_raw(), ctx.as_raw(), true, &|| called.set(true), None);
        assert!(r.is_err() && !called.get(), "no depth view: nothing is issued");
        let (mut rtvs, mut dsv) = ([None, None], None);
        ctx.OMGetRenderTargets(Some(&mut rtvs), Some(&mut dsv));
        assert_eq!(rtvs[0].as_ref().map(|r| r.as_raw()), Some(s.rtv.as_raw()));
        assert!(dsv.is_none());
    }
}

// ---- the mesh uv: signature parsing, the generated pixel shader, the draw into the mask --------------------

/// A vertex shader with the kind of output the engine has: position, a float4 and a float2 TEXCOORD, a COLOR and a flat uint.
const VS_UV: &str = "cbuffer c : register(b0) { float4 r; float4 zz; };
struct O { float4 pos : SV_Position; float4 t0 : TEXCOORD0; float2 t1 : TEXCOORD1; float4 col : COLOR0; nointerpolation uint id : BLENDINDICES0; };
O main(uint vid : SV_VertexID) { float2 u = float2(vid & 1, vid >> 1); float2 p = lerp(r.xy, r.zw, u); O o;
  o.pos = float4(p, zz.x, 1); o.t0 = float4(0.25 + 0.5 * u.x, 0.125 + 0.5 * u.y, 7, 8); o.t1 = float2(u.y, 1.0 - u.x); o.col = float4(1, 2, 3, 4); o.id = 5; return o; }";

fn vs_blob() -> Vec<u8> {
    shaders::compile(VS_UV, "main", "vs_5_0").unwrap_or_else(|e| panic!("{e}"))
}

fn vs_sig() -> VsSig {
    VsSig { elems: parse_output_signature(&vs_blob()).expect("output signature") }
}

#[test]
fn output_signature_of_a_compiled_vertex_shader_is_parsed() {
    let sig = vs_sig();
    let find = |n: &str, i: u32| sig.elems.iter().find(|e| e.name == n && e.index == i).unwrap_or_else(|| panic!("{n}{i} missing in {}", sig.text())).clone();
    assert_eq!(find("SV_Position", 0).sys, 1);
    let t0 = find("TEXCOORD", 0);
    assert_eq!((t0.sys, t0.ctype, t0.mask), (0, 3, 0xf));
    assert_eq!(find("TEXCOORD", 1).mask & 3, 3);
    assert_eq!(find("BLENDINDICES", 0).ctype, 1);
    // Garbage and truncation are refused, never panic.
    assert!(parse_output_signature(&[]).is_none());
    assert!(parse_output_signature(b"DXBCgarbage").is_none());
    let blob = vs_blob();
    for cut in [0, 10, 30, 40, 60, blob.len() / 2, blob.len() - 1] {
        let _ = parse_output_signature(&blob[..cut]);
    }
    let mut bad = blob.clone();
    for b in bad.iter_mut().skip(32).take(200) {
        *b = 0xff;
    }
    let _ = parse_output_signature(&bad);
}

#[test]
fn uv_preference_parses_and_picks_the_element() {
    assert_eq!(UvPref::parse(""), UvPref::Auto);
    assert_eq!(UvPref::parse("AUTO"), UvPref::Auto);
    assert_eq!(UvPref::parse("texcoord1"), UvPref::Sem { name: "TEXCOORD".into(), index: 1, zw: false });
    assert_eq!(UvPref::parse(" TexCoord0.zw "), UvPref::Sem { name: "TEXCOORD".into(), index: 0, zw: true });
    assert_eq!(UvPref::parse("texcoord2.xy").text(), "TEXCOORD2.xy");
    assert_eq!(UvPref::parse("texcoord2.xx"), UvPref::Auto, "bad swizzle = auto");
    assert_eq!(UvPref::parse("123"), UvPref::Auto);
    let sig = vs_sig();
    let p = sig.pick(&UvPref::Auto).expect("auto");
    assert_eq!((sig.elems[p.elem].name.as_str(), sig.elems[p.elem].index, p.zw), ("TEXCOORD", 0, false), "the lowest TEXCOORD");
    let p = sig.pick(&UvPref::parse("texcoord0.zw")).expect("zw");
    assert!(p.zw);
    assert!(sig.pick(&UvPref::parse("texcoord1.zw")).is_err(), "TEXCOORD1 is a float2: no zw");
    assert!(sig.pick(&UvPref::parse("texcoord5")).is_err(), "missing");
    assert!(sig.pick(&UvPref::parse("color0")).is_ok(), "any float element can be named");
    assert!(sig.pick(&UvPref::parse("blendindices0")).is_err(), "not a float");
    // No TEXCOORD at all: auto fails with a reason.
    let none = VsSig { elems: sig.elems.iter().filter(|e| e.name != "TEXCOORD").cloned().collect() };
    assert!(none.pick(&UvPref::Auto).unwrap_err().contains("TEXCOORD"));
}

#[test]
fn generated_uv_pixel_shader_compiles_and_links_to_the_vertex_outputs() {
    let sig = vs_sig();
    for pref in ["auto", "texcoord1", "texcoord0.zw"] {
        let pick = sig.pick(&UvPref::parse(pref)).unwrap();
        let hlsl = uv_ps_hlsl(&sig, pick);
        let blob = build_uv_ps(&sig, pick).unwrap_or_else(|e| panic!("{pref}: {e}\n{hlsl}"));
        verify_link(&sig, &blob).unwrap();
        assert_eq!(sig.key(pick), sig.key(pick));
    }
    assert_ne!(sig.key(sig.pick(&UvPref::Auto).unwrap()), sig.key(sig.pick(&UvPref::parse("texcoord1")).unwrap()), "keys differ per pick");
    // The check catches a pixel shader that reads the wrong register: a ps with only TEXCOORD1 declared first gets register 1.
    let wrong = shaders::compile("struct I { float4 p : SV_Position; float2 b : TEXCOORD1; }; float4 main(I i) : SV_Target { return float4(i.b, 0, 1); }", "main", "ps_5_0").unwrap();
    assert!(verify_link(&sig, &wrong).is_err(), "register mismatch must be reported");
}

/// The three vertex-output layouts the game's character vertex shaders really have (copied from the log of a play session: the
/// uv shader used to be refused for ALL of them, "TEXCOORD0: vertex output o0:3, pixel input v1:3", so the fur never had a uv).
/// Several semantics share one register (TEXCOORD0.xy + TEXCOORD1.z + TEXCOORD5.w) and SV_Position is the LAST output (o4).
#[test]
fn the_uv_pixel_shader_links_to_the_real_character_vertex_layouts() {
    fn el(name: &str, index: u32, reg: u32, mask: u8, sys: u32) -> SigElem {
        SigElem { name: name.to_string(), index, sys, ctype: 3, reg, mask }
    }
    let layouts: [(&str, Vec<SigElem>); 3] = [
        (
            "four registers",
            vec![el("TEXCOORD", 0, 0, 0x3, 0), el("TEXCOORD", 1, 1, 0xf, 0), el("TEXCOORD", 2, 2, 0xf, 0), el("TEXCOORD", 3, 3, 0xf, 0), el("SV_POSITION", 0, 4, 0xf, 1)],
        ),
        (
            "z packed beside the uv",
            vec![el("TEXCOORD", 0, 0, 0x3, 0), el("TEXCOORD", 1, 0, 0x4, 0), el("TEXCOORD", 2, 1, 0xf, 0), el("TEXCOORD", 3, 2, 0xf, 0), el("TEXCOORD", 4, 3, 0xf, 0), el("SV_POSITION", 0, 4, 0xf, 1)],
        ),
        (
            "z and w packed beside the uv",
            vec![
                el("TEXCOORD", 0, 0, 0x3, 0),
                el("TEXCOORD", 1, 0, 0x4, 0),
                el("TEXCOORD", 5, 0, 0x8, 0),
                el("TEXCOORD", 2, 1, 0x7, 0),
                el("TEXCOORD", 3, 2, 0x7, 0),
                el("TEXCOORD", 4, 3, 0x7, 0),
                el("SV_POSITION", 0, 4, 0xf, 1),
            ],
        ),
    ];
    for (what, elems) in layouts {
        let sig = VsSig { elems };
        let pick = sig.pick(&UvPref::Auto).unwrap();
        assert_eq!((sig.elems[pick.elem].index, pick.zw), (0, false), "{what}: the uv is TEXCOORD0.xy");
        let hlsl = uv_ps_hlsl(&sig, pick);
        build_uv_ps(&sig, pick).unwrap_or_else(|e| panic!("{what}: {e}\n{}\n{hlsl}", sig.text()));
        // The z-stamping shaders without uv (fur without uv, rim) link to the same layouts and read SV_Position.
        for rim in [false, true] {
            let hlsl = z_ps_hlsl(&sig, rim);
            assert!(hlsl.contains("SV_Target1") && hlsl.contains(".z;"), "{what}: the stamp is written\n{hlsl}");
            build_z_ps(&sig, rim).unwrap_or_else(|e| panic!("{what} rim {rim}: {e}\n{hlsl}"));
        }
    }
}

/// Draws the "engine" quad into the mask with the vertex shader `VS_UV` and the pixel shader `uv` (None = the plain mask shader).
fn draw_uv(s: &Scene, m: &mut Mask, uv: Option<(&str, &[u8])>) {
    unsafe {
        let ctx = s.g.ctx.clone();
        let vs = {
            let mut v = None;
            s.g.dev.CreateVertexShader(&vs_blob(), None, Some(&mut v)).unwrap();
            v.unwrap()
        };
        ctx.VSSetShader(&vs, None);
        s.quad([-1.0, -1.0, 1.0, 1.0], 0.5);
        let used = m.draw(s.g.dev.as_raw(), ctx.as_raw(), true, &|| ctx.Draw(4, 0), uv).expect("mask draw");
        assert_eq!(used, uv.is_some(), "the draw reports whether the uv shader was used");
    }
}

#[test]
fn mask_draw_with_the_uv_shader_writes_uv_valid_and_mask() {
    let Some(g) = gpu() else { return };
    let s = Scene::new(g);
    let sig = vs_sig();
    let pick = sig.pick(&UvPref::Auto).unwrap();
    let blob = build_uv_ps(&sig, pick).unwrap();
    let key = sig.key(pick);
    let mask_blob = mask_blob();
    unsafe {
        let mut m = Mask::new(s.g.dev.as_raw(), &mask_blob).expect("mask objects");
        draw_uv(&s, &mut m, Some((&key, &blob)));
        let t = s.texels(&m);
        let q = |v: u16| v as f32 / 65535.0;
        // TEXCOORD0.xy = (0.25 + 0.5 u, 0.125 + 0.5 v): pixel (x, y) is at u = (x + 0.5) / N, v = 1 - (y + 0.5) / N (clip y up).
        for &(x, y) in &[(8u32, 8u32), (32, 32), (50, 20), (60, 60)] {
            let px = t[(y * N + x) as usize];
            let (u, v) = ((x as f32 + 0.5) / N as f32, 1.0 - (y as f32 + 0.5) / N as f32);
            assert!((q(px[0]) - (0.25 + 0.5 * u)).abs() < 0.004, "u at ({x},{y}): {} vs {}", q(px[0]), 0.25 + 0.5 * u);
            assert!((q(px[1]) - (0.125 + 0.5 * v)).abs() < 0.004, "v at ({x},{y})");
            assert_eq!((px[2], px[3]), (65535, 65535), "uv valid and masked");
        }
        // The pixel shader object is cached per key.
        assert_eq!(m.uv_ps.len(), 1);
        draw_uv(&s, &mut m, Some((&key, &blob)));
        assert_eq!(m.uv_ps.len(), 1);
        // Engine state restored (the plain shader is bound again).
        let mut ps = None;
        s.g.ctx.PSGetShader(&mut ps, None, None);
        assert_eq!(ps.as_ref().map(|p| p.as_raw()), Some(s.ps.as_raw()));
    }
}

#[test]
fn mask_draw_without_a_uv_shader_falls_back_to_mask_only() {
    let Some(g) = gpu() else { return };
    let s = Scene::new(g);
    let mask_blob = mask_blob();
    unsafe {
        let mut m = Mask::new(s.g.dev.as_raw(), &mask_blob).expect("mask objects");
        draw_uv(&s, &mut m, None);
        let t = s.texels(&m);
        let px = t[(32 * N + 32) as usize];
        assert_eq!((px[2], px[3]), (0, 65535), "masked, uv not valid");
        // A broken blob (not DXBC) is refused by the device and the draw falls back instead of failing.
        let ctx = s.g.ctx.clone();
        s.quad([-1.0, -1.0, 1.0, 1.0], 0.5);
        let used = m.draw(s.g.dev.as_raw(), ctx.as_raw(), true, &|| ctx.Draw(4, 0), Some(("bad", &[1, 2, 3]))).expect("falls back");
        assert!(!used);
    }
}

#[test]
fn uv_status_text_says_ok_partial_or_none() {
    UV_WHY.lock().unwrap().clear();
    why("no TEXCOORD output with two float components");
    assert!(uv_text(0, 0).starts_with("uv: n/a"));
    assert_eq!(uv_text(3, 0), "uv: none (no TEXCOORD output with two float components)");
    assert!(uv_text(3, 1).starts_with("uv: partial 1/3"));
    assert!(uv_text(3, 3).starts_with("uv: ok"));
}

// ---- the rim channel -------------------------------------------------------------------------------------

#[test]
fn blend_state_words_tell_alpha_blends_from_opaque_and_additive_draws() {
    // [AlphaToCoverage, Independent, then RT0: BlendEnable, SrcBlend, DestBlend, ...]
    let d = |en: u32, src: u32, dst: u32| {
        let mut v = [0u32; 66];
        v[2] = en;
        v[3] = src;
        v[4] = dst;
        v
    };
    assert!(!blend_is_transparent(&d(0, 5, 6)), "blending off: opaque, whatever the factors say");
    assert!(blend_is_transparent(&d(1, 5, 6)), "SRC_ALPHA / INV_SRC_ALPHA = a transparent (stealth) draw");
    assert!(blend_is_transparent(&d(1, 2, 6)), "premultiplied alpha (ONE, INV_SRC_ALPHA)");
    assert!(blend_is_transparent(&d(1, 14, 15)), "blend factor");
    assert!(!blend_is_transparent(&d(1, 2, 2)), "additive (ONE, ONE) is not a transparent character");
    assert!(!blend_is_transparent(&d(1, 1, 1)));
    assert!(!blend_is_transparent(&[]), "a short / empty description is not transparent");
}

#[test]
fn configure_rim_sets_the_live_state_the_text_and_the_depth_consumer() {    let _g = crate::testlock::lock();

    // The only test that touches the rim globals (the draw tests below use Mask directly).
    let keep = crate::datapath::rim();
    crate::datapath::set_rim(0.4);
    configure_rim("1D50763F, xx, e0189185", 8192, false);
    assert_eq!(rim_selector_text(), "1d50763f,e0189185");
    assert_eq!((rim_min_vcb(), rim_debug()), (8192, false));
    assert!(wanted_rim() && RIM_NONEMPTY.load(Relaxed));
    assert_ne!(crate::ao::depth_consumers() & crate::ao::DEPTH_FOR_RIM, 0, "a wanted rim registers the depth consumer");
    assert!(rim_status_text().contains("draws masked") || rim_status_text().contains("compiling"), "{}", rim_status_text());
    // Strength 0 and no debug: not wanted, no consumer, no draws.
    crate::datapath::set_rim(0.0);
    assert!(!wanted_rim());
    assert_eq!(crate::ao::depth_consumers() & crate::ao::DEPTH_FOR_RIM, 0);
    assert_eq!(rim_status_text(), "off (rim_strength 0 and rim_debug 0)");
    assert!(!rim_ready());
    // The debug view wants the mask even at strength 0.
    configure_rim("1d50763f", 4096, true);
    assert!(wanted_rim());
    // Empty list: off, and the match flag is dropped.
    configure_rim("", 4096, false);
    assert!(!RIM_NONEMPTY.load(Relaxed) && !RIM_MATCH.load(Relaxed));
    assert_eq!(rim_status_text(), "off (rim_ps is empty)");
    assert_eq!(crate::ao::depth_consumers() & crate::ao::DEPTH_FOR_RIM, 0);
    // The fur channel does not care about the rim list.
    assert_eq!(selector_text().0.is_empty(), !NONEMPTY.load(Relaxed));
    crate::datapath::set_rim(keep);
    configure_rim("", 4096, false);
}

fn rim_blob() -> Vec<u8> {
    shaders::compile(HLSL, PS_RIM_ENTRY, "ps_5_0").unwrap_or_else(|e| panic!("furmask.hlsl ps_rim does not compile: {e}"))
}

/// R8 rim mask of `m` (0..=255 per pixel).
fn rim_mask(s: &Scene, m: &Mask) -> Vec<u8> {
    let srv: ID3D11ShaderResourceView = unsafe { ID3D11ShaderResourceView::from_raw_borrowed(&m.rim_srv()).unwrap().clone() };
    let res = unsafe { srv.GetResource().unwrap() };
    let tex: ID3D11Texture2D = res.cast().unwrap();
    read_back(&s.g, &tex, DXGI_FORMAT_R8_UNORM, 1)
}

#[test]
fn rim_draw_marks_visible_pixels_in_its_own_r8_target_and_leaves_the_fur_mask_and_the_state_alone() {
    let Some(g) = gpu() else { return };
    let s = Scene::new(g);
    let (blob, rb) = (mask_blob(), rim_blob());
    unsafe {
        let mut m = Mask::new(s.g.dev.as_raw(), &blob).expect("mask objects");
        assert!(m.ensure_rim_ps(s.g.dev.as_raw(), &rb), "rim pixel shader");
        // Occluder lower right (z 0.2), the character behind it (z 0.5): same set-up as the fur draw test.
        s.quad([0.0, -1.0, 1.0, 0.0], 0.2);
        s.draw();
        s.quad([-1.0, -1.0, 1.0, 1.0], 0.5);
        let ctx = s.g.ctx.clone();
        let r = m.draw_into(s.g.dev.as_raw(), ctx.as_raw(), true, true, &|| ctx.Draw(4, 0), None);
        assert_eq!(r, Ok(false), "no uv for the rim channel");
        let v = rim_mask(&s, &m);
        assert_eq!(at(&v, 16, 32), 255, "visible left half is in the rim mask");
        assert_eq!(at(&v, 48, 8), 255);
        assert_eq!(at(&v, 48, 48), 0, "behind the occluder: not masked (depth test)");
        assert!(m.srv().is_null(), "the fur target was not created by a rim draw");
        assert_eq!(RIM_H.load(Relaxed), N, "the height of the rim mask is published for the shader");
        // State restored, colour and depth untouched.
        let (mut rtvs, mut dsv) = ([None, None], None);
        ctx.OMGetRenderTargets(Some(&mut rtvs), Some(&mut dsv));
        assert_eq!(rtvs[0].as_ref().map(|r| r.as_raw()), Some(s.rtv.as_raw()));
        assert_eq!(dsv.as_ref().map(|d| d.as_raw()), Some(s.dsv.as_raw()));
        let mut ps = None;
        ctx.PSGetShader(&mut ps, None, None);
        assert_eq!(ps.as_ref().map(|p| p.as_raw()), Some(s.ps.as_raw()));
        let before = read_back(&s.g, &s.color, DXGI_FORMAT_R8G8B8A8_UNORM, 4);
        assert_eq!(before[(((32 * N) + 16) * 4) as usize], 0, "the colour target is untouched");
        s.draw();
        let after = read_back(&s.g, &s.color, DXGI_FORMAT_R8G8B8A8_UNORM, 4);
        assert_eq!(after[(((32 * N) + 16) * 4) as usize], 255, "depth was not written by the rim draw");
        // Both channels from one engine draw, each in its own target (the real draw above left depth 0.5: z 0.3 passes).
        s.quad([-1.0, -1.0, -0.5, 1.0], 0.3);
        m.draw(s.g.dev.as_raw(), ctx.as_raw(), true, &|| ctx.Draw(4, 0), None).unwrap();
        m.draw_into(s.g.dev.as_raw(), ctx.as_raw(), true, true, &|| ctx.Draw(4, 0), None).unwrap();
        let (f, v) = (s.mask(&m), rim_mask(&s, &m));
        assert_eq!((at(&f, 4, 32), at(&f, 60, 32)), (255, 0));
        assert_eq!((at(&v, 4, 32), at(&v, 60, 32)), (255, 0), "cleared with the first draw of the new frame");
    }
}

#[test]
fn rim_draw_skips_alpha_blended_draws_but_accepts_additive_and_opaque_ones() {
    let Some(g) = gpu() else { return };
    let s = Scene::new(g);
    let (blob, rb) = (mask_blob(), rim_blob());
    unsafe {
        let mut m = Mask::new(s.g.dev.as_raw(), &blob).expect("mask objects");
        assert!(m.ensure_rim_ps(s.g.dev.as_raw(), &rb));
        let ctx = s.g.ctx.clone();
        let blend = |src: D3D11_BLEND, dst: D3D11_BLEND| {
            let mut bd = D3D11_BLEND_DESC::default();
            bd.RenderTarget[0] = D3D11_RENDER_TARGET_BLEND_DESC { BlendEnable: true.into(), SrcBlend: src, DestBlend: dst, BlendOp: D3D11_BLEND_OP_ADD, SrcBlendAlpha: D3D11_BLEND_ONE, DestBlendAlpha: D3D11_BLEND_ZERO, BlendOpAlpha: D3D11_BLEND_OP_ADD, RenderTargetWriteMask: 15 };
            let mut b = None;
            s.g.dev.CreateBlendState(&bd, Some(&mut b)).unwrap();
            b.unwrap()
        };
        s.quad([-1.0, -1.0, 1.0, 1.0], 0.5);
        // Stealth-like: alpha blend. Nothing is issued, the error says why, the state (blend included) is back.
        let alpha = blend(D3D11_BLEND_SRC_ALPHA, D3D11_BLEND_INV_SRC_ALPHA);
        ctx.OMSetBlendState(&alpha, Some(&[1.0; 4]), 0xffff_ffff);
        let called = std::cell::Cell::new(false);
        let r = m.draw_into(s.g.dev.as_raw(), ctx.as_raw(), true, true, &|| called.set(true), None);
        assert_eq!(r, Err("alpha-blended draw"));
        assert!(!called.get(), "a refused draw is never issued");
        let mut cur = None;
        ctx.OMGetBlendState(Some(&mut cur), None, None);
        assert_eq!(cur.as_ref().map(|b| b.as_raw()), Some(alpha.as_raw()), "blend state restored");
        // Additive is not transparent: accepted.
        let add = blend(D3D11_BLEND_ONE, D3D11_BLEND_ONE);
        ctx.OMSetBlendState(&add, Some(&[1.0; 4]), 0xffff_ffff);
        assert!(m.draw_into(s.g.dev.as_raw(), ctx.as_raw(), true, true, &|| ctx.Draw(4, 0), None).is_ok());
        assert_eq!(at(&rim_mask(&s, &m), 32, 32), 255);
        // No blend state bound (opaque): accepted.
        ctx.OMSetBlendState(None::<&ID3D11BlendState>, Some(&[1.0; 4]), 0xffff_ffff);
        assert!(m.draw_into(s.g.dev.as_raw(), ctx.as_raw(), true, true, &|| ctx.Draw(4, 0), None).is_ok());
        // The fur channel is not affected by the blend state at all (it never looked at it).
        ctx.OMSetBlendState(&alpha, Some(&[1.0; 4]), 0xffff_ffff);
        assert!(m.draw(s.g.dev.as_raw(), ctx.as_raw(), true, &|| ctx.Draw(4, 0), None).is_ok());
    }
}

#[test]
fn rim_target_follows_the_depth_buffer_size_and_the_channels_do_not_share_state() {
    let Some(g) = gpu() else { return };
    let s = Scene::new(g);
    let (blob, rb) = (mask_blob(), rim_blob());
    unsafe {
        let mut m = Mask::new(s.g.dev.as_raw(), &blob).expect("mask objects");
        // The rim draw before ensure_rim_ps is a caller error the hook guards against; with the shader it works and a second device
        // size recreates the target (here: same size twice keeps one target).
        assert!(m.ensure_rim_ps(s.g.dev.as_raw(), &rb));
        assert!(m.ensure_rim_ps(s.g.dev.as_raw(), &rb), "idempotent");
        let ctx = s.g.ctx.clone();
        s.quad([-1.0, -1.0, 0.0, 1.0], 0.5);
        m.draw_into(s.g.dev.as_raw(), ctx.as_raw(), true, true, &|| ctx.Draw(4, 0), None).unwrap();
        let first = m.rim_srv();
        m.draw_into(s.g.dev.as_raw(), ctx.as_raw(), true, false, &|| ctx.Draw(4, 0), None).unwrap();
        assert_eq!(first, m.rim_srv(), "the same size keeps the same target");
        assert!(m.srv().is_null(), "the fur channel has no target until it is drawn");
    }
}

// ---- the occlusion check (z-stamp + validate) -------------------------------------------------------------

#[test]
fn occlusion_constants_match_the_shader_and_the_dxgi_formats() {
    assert!(HLSL.contains(&format!("static const float OCC_TOL = {:e};", OCC_TOL)), "OCC_TOL in furmask.hlsl must equal the Rust constant");
    let f = |x: DXGI_FORMAT| x.0 as u32;
    assert_eq!(depth_srv_format(f(DXGI_FORMAT_R24G8_TYPELESS)), Some(f(DXGI_FORMAT_R24_UNORM_X8_TYPELESS)));
    assert_eq!(depth_srv_format(f(DXGI_FORMAT_R32_TYPELESS)), Some(f(DXGI_FORMAT_R32_FLOAT)));
    assert_eq!(depth_srv_format(f(DXGI_FORMAT_R16_TYPELESS)), Some(f(DXGI_FORMAT_R16_UNORM)));
    assert_eq!(depth_srv_format(f(DXGI_FORMAT_R32G8X24_TYPELESS)), Some(f(DXGI_FORMAT_R32_FLOAT_X8X24_TYPELESS)));
    for typed in [DXGI_FORMAT_D24_UNORM_S8_UINT, DXGI_FORMAT_D32_FLOAT, DXGI_FORMAT_D16_UNORM, DXGI_FORMAT_D32_FLOAT_S8X24_UINT, DXGI_FORMAT_R8G8B8A8_UNORM] {
        assert_eq!(depth_srv_format(f(typed)), None, "{typed:?} cannot be read by a shader");
    }
    assert_eq!((occ_mode(2), occ_mode(4), occ_mode(5), occ_mode(7)), (Some(false), Some(false), Some(true), Some(true)));
    for t in [0, 1, 3, 6, 8, TEST_MIXED] {
        assert_eq!(occ_mode(t), None, "test {t}: no check");
    }
}

#[test]
fn occlusion_status_text_says_checked_unavailable_or_skipped() {
    let why = Mutex::new("depth format D24_UNORM_S8_UINT (45) is not a typeless depth format".to_string());
    assert_eq!(occ_text(OCC_CHECKED, &why), "occlusion: checked");
    assert_eq!(occ_text(OCC_UNAVAILABLE, &why), "occlusion: unavailable (depth format D24_UNORM_S8_UINT (45) is not a typeless depth format)");
    assert_eq!(occ_text(OCC_SKIPPED | 3 << 8, &why), "occlusion: skipped (depth test EQUAL)");
    assert_eq!(occ_text(OCC_SKIPPED | TEST_MIXED << 8, &why), "occlusion: skipped (depth test mixed within the frame)");
    assert!(occ_text(OCC_NONE, &why).starts_with("occlusion: n/a"));
    assert!(occ_text(OCC_PENDING, &why).starts_with("occlusion: pending"));
    assert_eq!(occ_text(OCC_OFF, &why), "occlusion: off (fur_occlusion=0)");
}

fn occ_blobs() -> OccBlobs {
    compile_occ_blobs().unwrap_or_else(|e| panic!("occlusion shaders: {e}"))
}

/// The output signature of `VS_QUAD` (SV_Position only) and its z-stamping mask shader (`rim` = the rim variant).
fn quad_z_blob(rim: bool) -> (String, Vec<u8>) {
    let vs = shaders::compile(VS_QUAD, "main", "vs_5_0").unwrap();
    let sig = VsSig { elems: parse_output_signature(&vs).unwrap() };
    (format!("{}|{}", sig.text(), rim), build_z_ps(&sig, rim).unwrap_or_else(|e| panic!("{e}\n{}", z_ps_hlsl(&sig, rim))))
}

/// The R32_FLOAT z-stamp of a channel.
fn stamp(s: &Scene, m: &Mask, rim: bool) -> Vec<f32> {
    let srv: ID3D11ShaderResourceView = unsafe { ID3D11ShaderResourceView::from_raw_borrowed(&m.stamp_srv(rim)).unwrap().clone() };
    let tex: ID3D11Texture2D = unsafe { srv.GetResource().unwrap() }.cast().unwrap();
    read_back(&s.g, &tex, DXGI_FORMAT_R32_FLOAT, 4).chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
}

/// The character: the mask draw (through `Mask::draw_into`) of the quad bound now, then the engine's own draw of it (depth write).
unsafe fn character(s: &Scene, m: &mut Mask, rim: bool, clear: bool, shader: Option<(&str, &[u8])>) {
    let ctx = s.g.ctx.clone();
    unsafe {
        m.draw_into(s.g.dev.as_raw(), ctx.as_raw(), rim, clear, &|| ctx.Draw(4, 0), shader).expect("mask draw");
        ctx.Draw(4, 0);
    }
}

unsafe fn validate(s: &Scene, m: &mut Mask, rim: bool) -> Occ {
    unsafe { m.validate(s.g.dev.as_raw(), s.g.ctx.as_raw(), rim, &occ_blobs()) }
}

#[test]
fn occlusion_check_erases_the_mask_under_a_nearer_surface_drawn_later_and_keeps_the_rest_bit_identical() {
    let Some(g) = gpu() else { return };
    let s = Scene::new(g);
    let sig = vs_sig();
    let pick = sig.pick(&UvPref::Auto).unwrap();
    let (key, blob) = (sig.key(pick), build_uv_ps(&sig, pick).unwrap());
    unsafe {
        let mut m = Mask::new(s.g.dev.as_raw(), &mask_blob()).expect("mask objects");
        // The character over the whole screen at z 0.5 (uv shader, VS_UV), then a nearer surface (z 0.2) over the right half.
        draw_uv(&s, &mut m, Some((&key, &blob)));
        s.draw();
        let z = stamp(&s, &m, false);
        assert!((z[(32 * N + 10) as usize] - 0.5).abs() < 1e-6, "the stamp is the fragment depth: {}", z[(32 * N + 10) as usize]);
        s.quad([0.0, -1.0, 1.0, 1.0], 0.2);
        s.draw();
        let before = s.texels(&m);
        assert_eq!(validate(&s, &mut m, false), Occ::Checked);
        let after = s.texels(&m);
        for y in 0..N {
            for x in 0..N {
                let i = (y * N + x) as usize;
                if x >= N / 2 {
                    assert_eq!(after[i], [0; 4], "({x},{y}) is under the nearer surface: erased, all channels");
                } else {
                    assert_eq!(after[i], before[i], "({x},{y}) is visible: untouched (uv included)");
                    assert_eq!(after[i][3], 65535);
                }
            }
        }
        assert_eq!(before[(32 * N + 10) as usize][2], 65535, "the uv was valid before");
        // Stable: a second check changes nothing more.
        assert_eq!(validate(&s, &mut m, false), Occ::Checked);
        assert!(s.texels(&m) == after);
    }
}

#[test]
fn occlusion_check_with_nothing_in_front_leaves_the_mask_bit_identical() {
    let Some(g) = gpu() else { return };
    let s = Scene::new(g);
    let (key, zb) = quad_z_blob(false);
    unsafe {
        let mut m = Mask::new(s.g.dev.as_raw(), &mask_blob()).expect("mask objects");
        // An occluder drawn BEFORE (the mask draw respects it), the character half-covered by it, nothing after.
        s.quad([0.0, -1.0, 1.0, 0.0], 0.2);
        s.draw();
        s.quad([-0.75, -0.75, 0.75, 0.75], 0.5);
        character(&s, &mut m, false, true, Some((&key, &zb)));
        let before = s.texels(&m);
        assert_eq!(at(&s.mask(&m), 48, 48), 0, "behind the earlier occluder: not masked at all");
        assert_eq!(at(&s.mask(&m), 20, 20), 255);
        assert_eq!(validate(&s, &mut m, false), Occ::Checked);
        assert!(s.texels(&m) == before, "bit-identical");
    }
}

#[test]
fn occlusion_check_handles_reversed_z() {
    let Some(g) = gpu() else { return };
    let s = Scene::with(g, Depth::Engine, D3D11_COMPARISON_GREATER);
    let (key, zb) = quad_z_blob(false);
    unsafe {
        let mut m = Mask::new(s.g.dev.as_raw(), &mask_blob()).expect("mask objects");
        s.quad([-1.0, -1.0, 1.0, 1.0], 0.5);
        character(&s, &mut m, false, true, Some((&key, &zb)));
        assert_eq!(at(&s.mask(&m), 10, 10), 255, "masked with GREATER against a depth cleared to 0");
        // Nearer in reversed Z = larger: 0.8 over the top half (clip y > 0).
        s.quad([-1.0, 0.0, 1.0, 1.0], 0.8);
        s.draw();
        // A farther surface (0.3) drawn later over the bottom left fails the engine's test: nothing changes there.
        s.quad([-1.0, -1.0, 0.0, 0.0], 0.3);
        s.draw();
        assert_eq!(validate(&s, &mut m, false), Occ::Checked);
        let v = s.mask(&m);
        assert_eq!((at(&v, 10, 10), at(&v, 50, 20)), (0, 0), "under the nearer surface");
        assert_eq!((at(&v, 10, 50), at(&v, 50, 50)), (255, 255), "the farther surface does not cover the character");
    }
}

#[test]
fn occlusion_check_fails_open_when_the_depth_buffer_cannot_be_read() {
    let _g = crate::testlock::lock();
    for (kind, want) in [(Depth::Typed, "not a typeless depth format"), (Depth::NoSrvFlag, "no shader-resource bind flag")] {
        let Some(g) = gpu() else { return };
        let s = Scene::with(g, kind, D3D11_COMPARISON_LESS);
        let (key, zb) = quad_z_blob(false);
        unsafe {
            let mut m = Mask::new(s.g.dev.as_raw(), &mask_blob()).expect("mask objects");
            s.quad([-1.0, -1.0, 1.0, 1.0], 0.5);
            character(&s, &mut m, false, true, Some((&key, &zb)));
            s.quad([0.0, -1.0, 1.0, 1.0], 0.2);
            s.draw();
            let before = s.texels(&m);
            let r = validate(&s, &mut m, false);
            match &r {
                Occ::Unavailable(why) => assert!(why.as_str().contains(want), "{}", why.as_str()),
                other => panic!("{other:?}"),
            }
            assert!(s.texels(&m) == before, "fail open: the mask is untouched");
            // Cached: the second attempt gives the same answer without a new view.
            assert_eq!(validate(&s, &mut m, false), r);
            assert_eq!(m.depth_srvs.len(), 1);
            // The status line says so.
            note_occ(false, &r);
            let t = occ_text(OCC_CUR.load(Relaxed), &OCC_WHY);
            assert!(t.starts_with("occlusion: unavailable (") && t.contains(want), "{t}");
        }
    }
    OCC_CUR.store(OCC_NONE, Relaxed);
}

#[test]
fn occlusion_check_keeps_pixels_nothing_stamped() {
    let Some(g) = gpu() else { return };
    let s = Scene::new(g);
    unsafe {
        let mut m = Mask::new(s.g.dev.as_raw(), &mask_blob()).expect("mask objects");
        // The static fallback shader (no signature known yet) stamps -1: the check must keep those pixels even under a nearer surface.
        s.quad([-1.0, -1.0, 1.0, 1.0], 0.5);
        character(&s, &mut m, false, true, None);
        assert!(stamp(&s, &m, false).iter().all(|&z| z == -1.0), "nothing stamped");
        s.quad([0.0, -1.0, 1.0, 1.0], 0.2);
        s.draw();
        let before = s.texels(&m);
        assert_eq!(validate(&s, &mut m, false), Occ::Checked);
        assert!(s.texels(&m) == before, "untouched");
        assert_eq!(at(&s.mask(&m), 50, 30), 255);
    }
}

#[test]
fn occlusion_check_works_for_the_rim_channel() {
    let Some(g) = gpu() else { return };
    let s = Scene::new(g);
    let (key, zb) = quad_z_blob(true);
    unsafe {
        let mut m = Mask::new(s.g.dev.as_raw(), &mask_blob()).expect("mask objects");
        assert!(m.ensure_rim_ps(s.g.dev.as_raw(), &rim_blob()));
        s.quad([-1.0, -1.0, 1.0, 1.0], 0.5);
        character(&s, &mut m, true, true, Some((&key, &zb)));
        assert!(m.srv().is_null(), "the fur channel is not involved");
        assert_eq!(at(&rim_mask(&s, &m), 10, 10), 255, "the generated rim shader writes 1");
        s.quad([-1.0, -1.0, 0.0, 1.0], 0.2); // left half, nearer
        s.draw();
        assert_eq!(validate(&s, &mut m, true), Occ::Checked);
        let v = rim_mask(&s, &m);
        for y in 0..N {
            for x in 0..N {
                assert_eq!(at(&v, x, y), if x < N / 2 { 0 } else { 255 }, "({x},{y})");
            }
        }
        // The fur channel has nothing to check.
        assert_eq!(validate(&s, &mut m, false), Occ::Unavailable(Reason::Static("nothing was masked")));
    }
}

#[test]
fn occlusion_check_is_skipped_for_other_depth_tests_and_mixed_frames() {
    let Some(g) = gpu() else { return };
    let s = Scene::with(g, Depth::Engine, D3D11_COMPARISON_LESS_EQUAL);
    let (key, zb) = quad_z_blob(false);
    unsafe {
        let mut m = Mask::new(s.g.dev.as_raw(), &mask_blob()).expect("mask objects");
        s.quad([-1.0, -1.0, 1.0, 1.0], 0.5);
        character(&s, &mut m, false, true, Some((&key, &zb)));
        assert_eq!(m.fur_test, 4, "LESS_EQUAL remembered");
        // A second draw of the frame with an EQUAL test: the frame is mixed, no check.
        let dd = D3D11_DEPTH_STENCIL_DESC { DepthEnable: true.into(), DepthWriteMask: D3D11_DEPTH_WRITE_MASK_ZERO, DepthFunc: D3D11_COMPARISON_EQUAL, ..Default::default() };
        let mut eq = None;
        s.g.dev.CreateDepthStencilState(&dd, Some(&mut eq)).unwrap();
        s.g.ctx.OMSetDepthStencilState(eq.as_ref(), 0);
        character(&s, &mut m, false, false, Some((&key, &zb)));
        s.g.ctx.OMSetDepthStencilState(&s.dss, 0);
        s.quad([0.0, -1.0, 1.0, 1.0], 0.2);
        s.draw();
        let before = s.texels(&m);
        assert_eq!(validate(&s, &mut m, false), Occ::Skipped(TEST_MIXED));
        assert!(s.texels(&m) == before);
        // A new frame with EQUAL only: skipped, named.
        s.g.ctx.OMSetDepthStencilState(eq.as_ref(), 0);
        s.quad([-1.0, -1.0, 1.0, 1.0], 0.2);
        character(&s, &mut m, false, true, Some((&key, &zb)));
        assert_eq!(validate(&s, &mut m, false), Occ::Skipped(3));
    }
}

#[test]
fn masked_draw_with_two_targets_and_the_occlusion_check_restore_the_pipeline_state() {
    let Some(g) = gpu() else { return };
    let s = Scene::new(g);
    let (key, zb) = quad_z_blob(false);
    unsafe {
        let ctx = s.g.ctx.clone();
        // The engine's extra state: SRVs in t0 / t1, a blend state, a second colour target.
        let mk_srv = || {
            let t = tex2d(&s.g, DXGI_FORMAT_R8G8B8A8_UNORM, D3D11_BIND_SHADER_RESOURCE, D3D11_USAGE_DEFAULT, 0);
            let mut v = None;
            s.g.dev.CreateShaderResourceView(&t, None, Some(&mut v)).unwrap();
            v.unwrap()
        };
        let (t0, t1) = (mk_srv(), mk_srv());
        ctx.PSSetShaderResources(0, Some(&[Some(t0.clone()), Some(t1.clone())]));
        let mut bd = D3D11_BLEND_DESC::default();
        bd.RenderTarget[0] = D3D11_RENDER_TARGET_BLEND_DESC { BlendEnable: true.into(), SrcBlend: D3D11_BLEND_ONE, DestBlend: D3D11_BLEND_ONE, BlendOp: D3D11_BLEND_OP_ADD, SrcBlendAlpha: D3D11_BLEND_ONE, DestBlendAlpha: D3D11_BLEND_ZERO, BlendOpAlpha: D3D11_BLEND_OP_ADD, RenderTargetWriteMask: 15 };
        let mut blend = None;
        s.g.dev.CreateBlendState(&bd, Some(&mut blend)).unwrap();
        let blend = blend.unwrap();
        ctx.OMSetBlendState(&blend, Some(&[0.5; 4]), 0xffff_ffff);
        let rt1 = tex2d(&s.g, DXGI_FORMAT_R8G8B8A8_UNORM, D3D11_BIND_RENDER_TARGET, D3D11_USAGE_DEFAULT, 0);
        let mut rtv1 = None;
        s.g.dev.CreateRenderTargetView(&rt1, None, Some(&mut rtv1)).unwrap();
        let rtv1 = rtv1.unwrap();
        ctx.OMSetRenderTargets(Some(&[Some(s.rtv.clone()), Some(rtv1.clone())]), &s.dsv);
        let check = |what: &str| {
            let (mut rtvs, mut dsv) = ([None, None, None], None);
            ctx.OMGetRenderTargets(Some(&mut rtvs), Some(&mut dsv));
            assert_eq!(rtvs[0].as_ref().map(|r| r.as_raw()), Some(s.rtv.as_raw()), "{what}: RTV0");
            assert_eq!(rtvs[1].as_ref().map(|r| r.as_raw()), Some(rtv1.as_raw()), "{what}: RTV1 (the z-stamp is not left bound)");
            assert!(rtvs[2].is_none(), "{what}: RTV2");
            assert_eq!(dsv.as_ref().map(|d| d.as_raw()), Some(s.dsv.as_raw()), "{what}: DSV");
            let mut ps = None;
            ctx.PSGetShader(&mut ps, None, None);
            assert_eq!(ps.as_ref().map(|p| p.as_raw()), Some(s.ps.as_raw()), "{what}: PS");
            let mut vs = None;
            ctx.VSGetShader(&mut vs, None, None);
            assert_eq!(vs.as_ref().map(|p| p.as_raw()), Some(s.vs.as_raw()), "{what}: VS");
            let mut srvs = [None, None];
            ctx.PSGetShaderResources(0, Some(&mut srvs));
            assert_eq!(srvs[0].as_ref().map(|v| v.as_raw()), Some(t0.as_raw()), "{what}: t0");
            assert_eq!(srvs[1].as_ref().map(|v| v.as_raw()), Some(t1.as_raw()), "{what}: t1 (the depth view is not left bound)");
            let (mut b, mut f, mut sm) = (None, [0.0f32; 4], 0u32);
            ctx.OMGetBlendState(Some(&mut b), Some(&mut f), Some(&mut sm));
            assert_eq!((b.as_ref().map(|b| b.as_raw()), f), (Some(blend.as_raw()), [0.5; 4]), "{what}: blend");
            let (mut dss, mut sref) = (None, 0);
            ctx.OMGetDepthStencilState(Some(&mut dss), Some(&mut sref));
            assert_eq!(dss.as_ref().map(|d| d.as_raw()), Some(s.dss.as_raw()), "{what}: depth state");
            let (mut n, mut vps) = (1u32, [D3D11_VIEWPORT::default()]);
            ctx.RSGetViewports(&mut n, Some(vps.as_mut_ptr()));
            assert_eq!((n, vps[0].Width, vps[0].Height), (1, N as f32, N as f32), "{what}: viewport");
            let topo = ctx.IAGetPrimitiveTopology();
            assert_eq!(topo, D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP, "{what}: topology");
        };
        let mut m = Mask::new(s.g.dev.as_raw(), &mask_blob()).expect("mask objects");
        s.quad([-1.0, -1.0, 1.0, 1.0], 0.5);
        m.draw_into(s.g.dev.as_raw(), ctx.as_raw(), false, true, &|| ctx.Draw(4, 0), Some((&key, &zb))).unwrap();
        check("after the masked draw");
        ctx.Draw(4, 0);
        s.quad([0.0, -1.0, 1.0, 1.0], 0.2);
        ctx.Draw(4, 0);
        assert_eq!(validate(&s, &mut m, false), Occ::Checked);
        check("after the check");
        assert_eq!(at(&s.mask(&m), 50, 30), 0, "and the check did its work");
    }
}

/// ONE draw with two layers, like a charr tail (fur shell / inner layer / a mesh that overlaps itself): the near layer (cb z) over
/// the whole rect `r`, then, LATER IN THE SAME DRAW, a farther layer (cb z + step, step = zz.y) over the inner half of it. A triangle list of 12.
const VS_TWO_LAYERS: &str = "cbuffer c : register(b0) { float4 r; float4 zz; };
float4 main(uint id : SV_VertexID) : SV_Position {
  uint q = id / 6; uint k = id % 6;
  uint c = k == 0 ? 0 : k == 1 ? 1 : k == 2 ? 2 : k == 3 ? 2 : k == 4 ? 1 : 3;
  float2 u = float2(c & 1, c >> 1);
  float4 rr = q == 0 ? r : lerp(r.xyxy, r.zwzw, float4(0.25, 0.25, 0.75, 0.75));
  return float4(lerp(rr.xy, rr.zw, u), zz.x + zz.y * q, 1); }";

/// The real-game bug of the occlusion check (in-game screenshot, fur_debug=1: magenta only on a thin band along the silhouette of the
/// tail, the inside gone): our mask draw writes no depth, so within ONE draw a farther layer rasterised after a nearer one passed the
/// read-only depth test too and overwrote the stamp; the engine's own draw keeps the nearer layer in the depth buffer, so the check saw
/// "final nearer than stamp" and erased the inside (where two layers overlap), keeping only the rim (one layer). The stamp must be the
/// NEAREST fragment of the draw (MIN blend; MAX for reversed Z).
#[test]
fn regression_a_mesh_that_overlaps_itself_in_one_draw_is_not_erased_by_the_check() {
    for func in [D3D11_COMPARISON_LESS, D3D11_COMPARISON_LESS_EQUAL, D3D11_COMPARISON_GREATER, D3D11_COMPARISON_GREATER_EQUAL] {
        let Some(g) = gpu() else { return };
        let s = Scene::with(g, Depth::Engine, func);
        let reversed = func == D3D11_COMPARISON_GREATER || func == D3D11_COMPARISON_GREATER_EQUAL;
        let vsb = shaders::compile(VS_TWO_LAYERS, "main", "vs_5_0").unwrap();
        let sig = VsSig { elems: parse_output_signature(&vsb).unwrap() };
        let (key, zb) = (format!("{}|two", sig.text()), build_z_ps(&sig, false).unwrap());
        unsafe {
            let ctx = s.g.ctx.clone();
            let mut vs = None;
            s.g.dev.CreateVertexShader(&vsb, None, Some(&mut vs)).unwrap();
            ctx.VSSetShader(vs.as_ref(), None);
            ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            // The far layer: z + 0.1 (LESS), z - 0.1 (reversed Z, nearer = larger).
            let (z0, step) = (0.5f32, if reversed { -0.1f32 } else { 0.1 });
            let rect = [-0.75, -0.75, 0.75, 0.75];
            let data = [rect[0], rect[1], rect[2], rect[3], z0, step, 0.0, 0.0];
            let bytes: Vec<u8> = data.iter().flat_map(|c| c.to_le_bytes()).collect();
            let d = D3D11_BUFFER_DESC { ByteWidth: 32, Usage: D3D11_USAGE_IMMUTABLE, BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32, ..Default::default() };
            let sd = D3D11_SUBRESOURCE_DATA { pSysMem: bytes.as_ptr() as *const _, SysMemPitch: 0, SysMemSlicePitch: 0 };
            let mut cb = None;
            s.g.dev.CreateBuffer(&d, Some(&sd), Some(&mut cb)).unwrap();
            ctx.VSSetConstantBuffers(0, Some(&[cb]));
            let mut m = Mask::new(s.g.dev.as_raw(), &mask_blob()).expect("mask objects");
            m.draw_into(s.g.dev.as_raw(), ctx.as_raw(), false, true, &|| ctx.Draw(12, 0), Some((&key, &zb))).expect("mask draw");
            ctx.Draw(12, 0);
            let st = stamp(&s, &m, false);
            // The pixel (32, 32) is under both layers; the stamp must be the near one.
            let c = st[(32 * N + 32) as usize];
            let near = z0;
            assert!((c - near).abs() < 1e-6, "{func:?}: the stamp is the nearest fragment of the draw ({near}), got {c}");
            let before = s.texels(&m);
            assert_eq!(validate(&s, &mut m, false), Occ::Checked, "{func:?}");
            let after = s.texels(&m);
            let v = s.mask(&m);
            assert!(after == before, "{func:?}: nothing covers the mesh, the check must leave the mask bit-identical");
            assert_eq!((at(&v, 32, 32), at(&v, 12, 32), at(&v, 2, 2)), (255, 255, 0), "{func:?}: inside and rim masked, outside not");
            // The hood fix still holds on such a mesh: a surface drawn later, nearer, over the left half erases exactly there.
            ctx.VSSetShader(&s.vs, None);
            ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
            s.quad([-1.0, -1.0, 0.0, 1.0], if reversed { 0.8 } else { 0.2 });
            s.draw();
            assert_eq!(validate(&s, &mut m, false), Occ::Checked);
            let v = s.mask(&m);
            assert_eq!((at(&v, 12, 32), at(&v, 28, 32), at(&v, 36, 32), at(&v, 52, 32)), (0, 0, 255, 255), "{func:?}: erased under the nearer surface only");
        }
    }
}

#[test]
fn the_stamp_blend_state_keeps_the_mask_unblended_and_the_stamp_min_or_max() {
    let d = stamp_blend_desc(BLEND_OP_MIN);
    assert_eq!((d[0], d[1]), (0, 1), "no alpha to coverage, independent blend");
    assert_eq!(&d[2..10], &[0, 2, 1, 1, 2, 1, 1, 0xf], "render target 0 (the mask): not blended, all channels written");
    assert_eq!(&d[10..18], &[1, 2, 2, 4, 2, 2, 4, 0xf], "render target 1 (the stamp): MIN");
    assert_eq!(&stamp_blend_desc(BLEND_OP_MAX)[10..18], &[1, 2, 2, 5, 2, 2, 5, 0xf]);
    // The same layout the windows crate has (D3D11_BLEND_DESC is 2 + 8 x 8 words).
    assert_eq!(std::mem::size_of::<D3D11_BLEND_DESC>(), 66 * 4);
    assert_eq!((D3D11_BLEND_OP_MIN.0 as u32, D3D11_BLEND_OP_MAX.0 as u32), (BLEND_OP_MIN, BLEND_OP_MAX));
    assert_eq!(D3D11_FORMAT_SUPPORT_BLENDABLE.0 as u32, FORMAT_SUPPORT_BLENDABLE);
    // "Nothing stamped" lies outside 0..1 for both encodings.
    assert!(!(0.0..=1.0).contains(&STAMP_CLEAR_LESS) && !(0.0..=1.0).contains(&STAMP_CLEAR_GREATER));
}

#[test]
fn the_occlusion_probe_emulates_the_check_on_the_cpu() {
    // 8 pixels: masked + stamped visible, masked + nearer surface (small and big gap), masked unstamped (both encodings), masked
    // farther (alpha test), unmasked but stamped, unmasked nothing.
    let mask = [true, true, true, true, true, true, false, false];
    let stamp = [0.5, 0.5, 0.5, -1.0, 2.0, 0.5, 0.5, 2.0];
    let depth = [0.5, 0.4999, 0.3, 0.1, 0.1, 0.7, 0.1, 1.0];
    let p = probe_counts(&mask, &stamp, &depth, Some(false));
    assert_eq!((p.masked, p.stamped, p.masked_unstamped, p.erase, p.erase_big, p.farther), (6, 5, 2, 2, 1, 1), "{p:?}");
    assert!((p.gap_max - 0.2).abs() < 1e-6 && p.gap_median > 1e-4, "{p:?}");
    // Reversed Z: nearer = larger; skipped = nothing erased.
    let p = probe_counts(&[true, true], &[0.5, 0.5], &[0.8, 0.2], Some(true));
    assert_eq!((p.erase, p.farther), (1, 1));
    assert_eq!(probe_counts(&[true, true], &[0.5, 0.5], &[0.2, 0.2], None).erase, 0);
    // Within the tolerance (D24 rounding of the stamp) nothing is erased; NaN never is.
    let p = probe_counts(&[true, true], &[0.5, f32::NAN], &[0.5 - OCC_TOL * 0.5, 0.1], Some(false));
    assert_eq!((p.erase, p.stamped), (0, 1));
    let line = probe_line("fur", 4, "R24G8_TYPELESS 64x64", &probe_counts(&mask, &stamp, &depth, Some(false)));
    for k in ["fur: depth test LESS_EQUAL", "R24G8_TYPELESS 64x64", "mask a>0.5 6 px", "stamped 0..1 5 px", "2 mask px unstamped", "would erase 2 (33.3 % of the mask; 1 by more than 1e-3", "1 mask px with the final depth farther"] {
        assert!(line.contains(k), "{k} missing in {line}");
    }
    assert!(probe_line("rim", 3, "x", &Probe::default()).contains("check skipped (depth test EQUAL)"));
    // The depth decoder: D24 in the low 24 bits (the stencil byte ignored), R32 floats, R16, R32 + X8X24.
    assert_eq!(decode_depth(44, &[0xff, 0xff, 0xff, 0x7f]).unwrap(), vec![1.0]);
    assert_eq!(decode_depth(44, &(0x0080_0000u32 | 0x5500_0000).to_le_bytes()).unwrap(), vec![8_388_608.0 / 16_777_215.0]);
    assert_eq!(decode_depth(39, &0.25f32.to_le_bytes()).unwrap(), vec![0.25]);
    assert_eq!(decode_depth(53, &[0xff, 0xff]).unwrap(), vec![1.0]);
    assert_eq!(decode_depth(19, &[0.75f32.to_le_bytes(), [9, 9, 9, 9]].concat()).unwrap(), vec![0.75]);
    assert_eq!(decode_depth(28, &[0; 4]), None);
}

/// The live switch and the dev-capture probe through the real entry point (`on_light_adjust`, the global mask state): with
/// `fur_occlusion=0` the mask stays as drawn and the status says off; switched back on, the check erases; the probe reads the
/// masks back BEFORE the check and counts what it erases.
#[test]
fn fur_occlusion_0_bypasses_the_check_and_the_probe_counts_what_it_would_erase() {
    let _g = crate::testlock::lock();
    let Some(g) = gpu() else { return };
    let s = Scene::new(g);
    let (key, zb) = quad_z_blob(false);
    let _ = OCC_BLOBS.set(compile_occ_blobs());
    unsafe {
        let mut m = Mask::new(s.g.dev.as_raw(), &mask_blob()).expect("mask objects");
        // The character over the whole screen at 0.5, a later nearer surface over the right half.
        s.quad([-1.0, -1.0, 1.0, 1.0], 0.5);
        character(&s, &mut m, false, true, Some((&key, &zb)));
        s.quad([0.0, -1.0, 1.0, 1.0], 0.2);
        s.draw();
        let before = s.texels(&m);
        *STATE.lock().unwrap() = Some(m);
        configure("1", "", 0, true); // a selector + fur_debug: the channel is wanted
        frame_begin();
        DRAWN.store(true, Relaxed);
        set_occlusion(false);
        PROBE_TEXT.lock().unwrap().clear();
        let first = probe_text(); // the debug capture asks for the probe
        assert!(first.contains("fur_occlusion 0") && first.contains("no probe yet"), "{first}");
        on_light_adjust(s.g.ctx.as_raw());
        let texels = |s: &Scene| s.texels(STATE.lock().unwrap().as_ref().unwrap());
        assert!(texels(&s) == before, "fur_occlusion=0: the mask stays as drawn");
        assert_eq!(OCC_CUR.load(Relaxed), OCC_OFF);
        frame_begin();
        assert_eq!(occ_text(OCC_LAST.load(Relaxed), &OCC_WHY), "occlusion: off (fur_occlusion=0)");
        let t = probe_text();
        let half = (N * N / 2) as u64;
        for k in ["fur_occlusion 0 (check bypassed", "fur: depth test LESS", &format!("mask a>0.5 {} px", N * N), &format!("would erase {half} (50.0 % of the mask"), "0 mask px unstamped"] {
            assert!(t.contains(k), "{k} missing in {t}");
        }
        // Back on: the next frame's check erases the covered half (the mask is drawn again, as each frame does).
        set_occlusion(true);
        {
            let mut st = STATE.lock().unwrap();
            let m = st.as_mut().unwrap();
            s.quad([-1.0, -1.0, 1.0, 1.0], 0.5);
            s.g.ctx.ClearDepthStencilView(&s.dsv, D3D11_CLEAR_DEPTH.0, 1.0, 0);
            character(&s, m, false, true, Some((&key, &zb)));
            s.quad([0.0, -1.0, 1.0, 1.0], 0.2);
            s.draw();
        }
        DRAWN.store(true, Relaxed);
        on_light_adjust(s.g.ctx.as_raw());
        let v: Vec<u8> = texels(&s).iter().map(|t| (t[3] >> 8) as u8).collect();
        assert_eq!((at(&v, 10, 30), at(&v, 50, 30)), (255, 0), "fur_occlusion=1: checked");
        assert_eq!(OCC_CUR.load(Relaxed), OCC_CHECKED);
        // Clean up the globals.
        s.g.ctx.PSSetShaderResources(SRV_SLOT, Some(&[None]));
        *STATE.lock().unwrap() = None;
        configure("", "", 4096, false);
        frame_begin();
        frame_begin();
        PROBE_WANT.store(false, Relaxed);
        PROBE_TEXT.lock().unwrap().clear();
    }
}

/// The bug seen in the game: a charr with a hood, seen from behind. The head is drawn (and masked) first, the hood after it, nearer,
/// writing depth. Before the check the head's mask showed through the opaque hood; after it no mask pixel is left under the hood.
#[test]
fn regression_the_head_mask_does_not_show_through_a_hood_drawn_after_it() {
    let Some(g) = gpu() else { return };
    let s = Scene::new(g);
    let (key, zb) = quad_z_blob(false);
    let (rkey, rzb) = quad_z_blob(true);
    unsafe {
        let mut m = Mask::new(s.g.dev.as_raw(), &mask_blob()).expect("mask objects");
        assert!(m.ensure_rim_ps(s.g.dev.as_raw(), &rim_blob()));
        let ctx = s.g.ctx.clone();
        // The head: clip [-0.5, 0.5]^2 at z 0.6, in both channels (one engine draw, re-issued once per mask).
        s.quad([-0.5, -0.5, 0.5, 0.5], 0.6);
        m.draw_into(s.g.dev.as_raw(), ctx.as_raw(), false, true, &|| ctx.Draw(4, 0), Some((&key, &zb))).unwrap();
        m.draw_into(s.g.dev.as_raw(), ctx.as_raw(), true, true, &|| ctx.Draw(4, 0), Some((&rkey, &rzb))).unwrap();
        ctx.Draw(4, 0);
        // The hood: wider than the head over its upper part (clip y 0..0.7), slightly nearer (a real hood sits a few cm out).
        s.quad([-0.625, 0.0, 0.625, 0.75], 0.58);
        ctx.Draw(4, 0);
        let (f0, r0) = (s.mask(&m), rim_mask(&s, &m));
        assert_eq!((at(&f0, 32, 20), at(&r0, 32, 20)), (255, 255), "the bug: the head's mask is set under the hood before the check");
        assert_eq!(validate(&s, &mut m, false), Occ::Checked);
        assert_eq!(validate(&s, &mut m, true), Occ::Checked);
        let (f, r) = (s.mask(&m), rim_mask(&s, &m));
        for y in 0..N {
            for x in 0..N {
                // Pixel centre in clip space (y up).
                let (cx, cy) = ((x as f32 + 0.5) / N as f32 * 2.0 - 1.0, 1.0 - (y as f32 + 0.5) / N as f32 * 2.0);
                let head = cx.abs() < 0.5 && cy.abs() < 0.5;
                let hood = cx.abs() < 0.625 && cy > 0.0 && cy < 0.75;
                let want = if head && !hood { 255 } else { 0 };
                assert_eq!((at(&f, x, y), at(&r, x, y)), (want, want), "({x},{y}) head {head} hood {hood}");
            }
        }
    }
}

/// The bug seen in the game (`status_occlusion=` of in-game captures): with the check on, 90..98 % of the visible mask
/// was "covered" by a CONSTANT gap of 3.87e-6 (65 D24 steps) between the stamp and the final depth, the final depth nearer. The engine
/// draws the character with a depth bias the stamp (SV_Position.z on that GPU) does not carry; with the old tolerance (2e-6) the check
/// erased almost the whole character. WARP puts the bias into SV_Position.z, so the game's situation is emulated here: the mask draw
/// without a bias, the engine's own draw with DepthBias -65 (its depth lands 65 steps nearer than the stamp). The visible part must be
/// kept, a real occluder (1e-2 nearer) must still erase.
#[test]
fn regression_a_constant_depth_bias_gap_does_not_make_the_check_erase_the_visible_mask() {
    let Some(g) = gpu() else { return };
    let s = Scene::with(g, Depth::Engine, D3D11_COMPARISON_LESS_EQUAL);
    let (key, zb) = quad_z_blob(false);
    unsafe {
        let rs = |bias: i32| {
            let rd = D3D11_RASTERIZER_DESC { FillMode: D3D11_FILL_SOLID, CullMode: D3D11_CULL_NONE, DepthClipEnable: true.into(), DepthBias: bias, ..Default::default() };
            let mut r = None;
            s.g.dev.CreateRasterizerState(&rd, Some(&mut r)).unwrap();
            r
        };
        let (rs0, rs65) = (rs(0), rs(-65));
        let mut m = Mask::new(s.g.dev.as_raw(), &mask_blob()).expect("mask objects");
        let ctx = s.g.ctx.clone();
        s.quad([-1.0, -1.0, 1.0, 1.0], 0.5);
        ctx.RSSetState(rs0.as_ref());
        m.draw_into(s.g.dev.as_raw(), ctx.as_raw(), false, true, &|| ctx.Draw(4, 0), Some((&key, &zb))).expect("mask draw");
        ctx.RSSetState(rs65.as_ref());
        ctx.Draw(4, 0); // the engine's draw: its depth 65 D24 steps nearer than the stamp
        ctx.RSSetState(rs0.as_ref());
        let gap = 0.5 - 65.0 / 16_777_215.0;
        assert!(OCC_TOL > 0.5 - gap && 2e-6 < 0.5 - gap, "the old tolerance was below the gap, the new one is above it");
        // A real occluder over the right half, 1e-2 nearer.
        s.quad([0.0, -1.0, 1.0, 1.0], 0.49);
        s.draw();
        let dtex: ID3D11Texture2D = s.dsv.GetResource().unwrap().cast().unwrap();
        let p = probe_counts(&s.mask(&m).iter().map(|&v| v > 127).collect::<Vec<_>>(), &stamp(&s, &m, false), &decode_depth(44, &read_back(&s.g, &dtex, DXGI_FORMAT_R24G8_TYPELESS, 4)).unwrap(), Some(false));
        assert_eq!((p.erase, p.erase_big), ((N * N / 2) as u64, (N * N / 2) as u64), "only the occluded half would be erased: {p:?}");
        assert_eq!(validate(&s, &mut m, false), Occ::Checked);
        let v = s.mask(&m);
        for y in 0..N {
            for x in 0..N {
                assert_eq!(at(&v, x, y), if x < N / 2 { 255 } else { 0 }, "({x},{y}): the biased visible half kept, the occluded half erased");
            }
        }
    }
}

#[test]
fn the_depth_bias_tolerance_follows_the_rasterizer_state() {
    let rs = |bias: i32, clamp: f32, slope: f32| [3, 3, 0, bias as u32, clamp.to_bits(), slope.to_bits(), 1, 0, 0, 0];
    assert_eq!(bias_tol(&rs(0, 0.0, 0.0), 44), 0.0);
    assert!((bias_tol(&rs(-65, 0.0, 0.0), 44) - 1.5 * 65.0 / 16_777_216.0).abs() < 1e-12, "D24: 65 steps (x 1.5)");
    assert!((bias_tol(&rs(5, 0.0, 0.0), 53) - 1.5 * 5.0 / 65536.0).abs() < 1e-9, "D16 steps are bigger");
    assert!((bias_tol(&rs(0, 0.001, 2.0), 44) - 0.0015).abs() < 1e-9 || bias_tol(&rs(0, 0.001, 2.0), 44) == OCC_TOL_MAX, "slope-scaled: the clamp");
    assert!((bias_tol(&rs(0, 0.0, 2.0), 44) - 1.5e-4).abs() < 1e-9, "slope-scaled without a clamp: 1e-4");
    assert_eq!(bias_tol(&rs(i32::MIN, f32::NAN, f32::INFINITY), 44), OCC_TOL_MAX, "garbage is capped");
    assert!(bias_tol(&rs(0, f32::NAN, f32::NAN), 44) == 0.0);
    // The probe text names the tolerance, the bias and the fallback draws (the "0 stamped" of the first probe after a loading screen).
    let t = probe_extra(1e-5 + 1e-6, [(-65i32) as u32, 0, 0], 3, 2);
    for k in ["tolerance 1.10e-5", "DepthBias -65", "3 masked draws stamped", "2 drew with the static fallback shader"] {
        assert!(t.contains(k), "{k} missing in {t}");
    }
    assert!(!probe_extra(1e-5, [0; 3], 4, 0).contains("fallback"));
    // probe_counts_tol: a wider tolerance keeps what the narrow one erases.
    let p = probe_counts_tol(&[true, true], &[0.5, 0.5], &[0.5 - 3e-5, 0.4], Some(false), 5e-5);
    assert_eq!(p.erase, 1);
}
