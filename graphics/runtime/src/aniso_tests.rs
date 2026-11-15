//! Tests of the sampler decision (host) and of `translate` with real sampler states (WARP).

use std::sync::Mutex;

use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::core::Interface;

use super::*;

/// The settings are process-wide: tests that change them take turns.
static SERIAL: Mutex<()> = Mutex::new(());

fn world() -> Desc {
    Desc { filter: 0x15, u: 1, v: 1, w: 1, bias: 0.0, max_aniso: 1, cmp: 1, border: [0.0; 4], min_lod: 0.0, max_lod: f32::MAX }
}

#[test]
fn a_trilinear_wrapping_sampler_becomes_anisotropic() {
    let Verdict::Change(n) = plan(&world(), 16, 0.0) else { panic!("not changed") };
    assert_eq!((n.filter, n.max_aniso, n.bias), (0x55, 16, 0.0));
    // Everything else is the engine's.
    assert_eq!((n.u, n.v, n.w, n.cmp, n.min_lod, n.max_lod), (1, 1, 1, 1, 0.0, f32::MAX));
    // Bilinear with point mips counts as linear too; mirror counts as tiling.
    let d = Desc { filter: 0x14, u: 2, v: 3, w: 3, ..world() };
    assert!(matches!(plan(&d, 8, 0.0), Verdict::Change(Desc { filter: 0x55, max_aniso: 8, .. })));
}

#[test]
fn the_level_is_clamped_and_one_means_off() {
    assert!(matches!(plan(&world(), 99, 0.0), Verdict::Change(Desc { max_aniso: 16, .. })));
    assert!(matches!(plan(&world(), 2, 0.0), Verdict::Change(Desc { max_aniso: 2, .. })));
    assert_eq!(plan(&world(), 1, 0.0), Verdict::Same);
    assert_eq!(plan(&world(), 0, 0.0), Verdict::Same);
}

#[test]
fn ui_post_and_comparison_samplers_are_never_touched() {
    // Post chain / fullscreen: linear, clamp on every axis.
    assert_eq!(plan(&Desc { u: 3, v: 3, w: 3, ..world() }, 16, -0.5), Verdict::ClampOnly);
    assert_eq!(plan(&Desc { u: 4, v: 4, w: 4, ..world() }, 16, -0.5), Verdict::ClampOnly);
    // Point filters (the post chain's other sampler) and every non-linear mix.
    for f in [0x0, 0x1, 0x4, 0x5, 0x10, 0x11, 0x41] {
        assert_eq!(plan(&Desc { filter: f, ..world() }, 16, -0.5), Verdict::OtherFilter, "filter {f:#x}");
    }
    // Comparison filters (shadow maps).
    for f in [0x80, 0x94, 0x95, 0xd5] {
        assert_eq!(plan(&Desc { filter: f, ..world() }, 16, -0.5), Verdict::Comparison, "filter {f:#x}");
    }
    // A sampler that cannot leave mip 0 (UI atlases, the overlay font sampler).
    assert_eq!(plan(&Desc { max_lod: 0.0, ..world() }, 16, -0.5), Verdict::NoMips);
    assert_eq!(plan(&Desc { min_lod: 3.0, max_lod: 3.0, ..world() }, 16, -0.5), Verdict::NoMips);
    assert_eq!(plan(&Desc { max_lod: f32::NAN, ..world() }, 16, -0.5), Verdict::NoMips);
}

#[test]
fn an_engine_anisotropic_sampler_is_raised_never_lowered() {
    let d = Desc { filter: 0x55, max_aniso: 4, ..world() };
    assert!(matches!(plan(&d, 16, 0.0), Verdict::Change(Desc { max_aniso: 16, .. })));
    let d = Desc { filter: 0x55, max_aniso: 16, ..world() };
    assert_eq!(plan(&d, 8, 0.0), Verdict::Same, "an engine level above ours stays");
    assert_eq!(plan(&Desc { max_aniso: 16, ..d }, 16, 0.0), Verdict::Same);
}

#[test]
fn the_mip_bias_is_added_and_clamped_and_can_work_alone() {
    let Verdict::Change(n) = plan(&Desc { bias: 0.25, ..world() }, 16, -0.5) else { panic!() };
    assert!((n.bias - -0.25).abs() < 1e-6);
    // Bias only: the filter is the engine's.
    let Verdict::Change(n) = plan(&world(), 0, -0.3) else { panic!() };
    assert_eq!((n.filter, n.max_aniso), (0x15, 1));
    assert!((n.bias - -0.3).abs() < 1e-6);
    // D3D's limits.
    let Verdict::Change(n) = plan(&Desc { bias: -15.9, ..world() }, 0, -1.0) else { panic!() };
    assert_eq!(n.bias, -16.0);
    // A broken bias from the engine is left alone.
    assert!(matches!(plan(&Desc { bias: f32::NAN, ..world() }, 16, -0.3), Verdict::Change(Desc { filter: 0x55, .. })));
    // Out of range settings are clamped.
    let Verdict::Change(n) = plan(&world(), 0, -9.0) else { panic!() };
    assert_eq!(n.bias, -1.0);
}

#[test]
fn settings_clamp_and_the_mod_off_switch_wins() {
    assert_eq!((clamp_level(0), clamp_level(16), clamp_level(99)), (0, 16, 16));
    assert_eq!((clamp_bias(f32::NAN), clamp_bias(-5.0), clamp_bias(5.0)), (0.0, -1.0, 0.5));
    assert!(active_for(16, 0.0, true));
    assert!(active_for(0, -0.3, true));
    assert!(!active_for(0, 0.0, true), "neutral settings substitute nothing");
    assert!(!active_for(1, 0.0, true));
    assert!(!active_for(16, -0.3, false), "the mod switched off: vanilla samplers");
}

#[test]
fn the_cache_table_finds_inserts_and_probes() {
    let mut c = Cache::new();
    let a = c.find(0x1000).unwrap();
    c.slots[a].key = 0x1000;
    assert_eq!(c.find(0x1000), Some(a));
    let b = c.find(0x2000).unwrap();
    assert_ne!(a, b);
    // Two keys with the same start position probe on.
    let mut keys = Vec::new();
    let target = Cache::index(0x3000);
    let mut k = 0x4000usize;
    while keys.len() < 3 {
        if Cache::index(k) == target {
            keys.push(k);
        }
        k += 16;
    }
    let mut seen = Vec::new();
    for k in keys {
        let p = c.find(k).unwrap();
        c.slots[p].key = k;
        assert!(!seen.contains(&p));
        seen.push(p);
    }
}

fn warp() -> Option<(ID3D11Device, ID3D11DeviceContext)> {
    let mut dev = None;
    let mut ctx = None;
    let r = unsafe { D3D11CreateDevice(None::<&windows::Win32::Graphics::Dxgi::IDXGIAdapter>, D3D_DRIVER_TYPE_WARP, HMODULE::default(), D3D11_CREATE_DEVICE_FLAG(0), None, D3D11_SDK_VERSION, Some(&mut dev), None, Some(&mut ctx)) };
    match (r, dev, ctx) {
        (Ok(()), Some(d), Some(c)) => Some((d, c)),
        _ => {
            println!("skipped: WARP device creation failed");
            None
        }
    }
}

fn sampler(dev: &ID3D11Device, filter: D3D11_FILTER, addr: D3D11_TEXTURE_ADDRESS_MODE, cmp: D3D11_COMPARISON_FUNC, max_lod: f32, aniso: u32) -> ID3D11SamplerState {
    let d = D3D11_SAMPLER_DESC { Filter: filter, AddressU: addr, AddressV: addr, AddressW: addr, MipLODBias: 0.0, MaxAnisotropy: aniso, ComparisonFunc: cmp, BorderColor: [0.0; 4], MinLOD: 0.0, MaxLOD: max_lod };
    let mut s = None;
    unsafe { dev.CreateSamplerState(&d, Some(&mut s)).expect("CreateSamplerState") };
    s.unwrap()
}

fn desc_of(s: Obj) -> D3D11_SAMPLER_DESC {
    let s: ID3D11SamplerState = unsafe { ID3D11SamplerState::from_raw_borrowed(&s).unwrap().clone() };
    let mut d = D3D11_SAMPLER_DESC::default();
    unsafe { s.GetDesc(&mut d) };
    d
}

#[test]
fn translate_swaps_only_world_samplers_on_warp() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let Some((dev, _ctx)) = warp() else { return };
    let world_s = sampler(&dev, D3D11_FILTER_MIN_MAG_MIP_LINEAR, D3D11_TEXTURE_ADDRESS_WRAP, D3D11_COMPARISON_NEVER, f32::MAX, 1);
    let post_s = sampler(&dev, D3D11_FILTER_MIN_MAG_MIP_LINEAR, D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_COMPARISON_NEVER, f32::MAX, 1);
    let point_s = sampler(&dev, D3D11_FILTER_MIN_MAG_MIP_POINT, D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_COMPARISON_NEVER, f32::MAX, 1);
    let shadow_s = sampler(&dev, D3D11_FILTER_COMPARISON_MIN_MAG_MIP_LINEAR, D3D11_TEXTURE_ADDRESS_WRAP, D3D11_COMPARISON_LESS_EQUAL, f32::MAX, 1);
    let ui_s = sampler(&dev, D3D11_FILTER_MIN_MAG_MIP_LINEAR, D3D11_TEXTURE_ADDRESS_WRAP, D3D11_COMPARISON_ALWAYS, 0.0, 1);

    let (keep_l, keep_b, keep_f) = (level(), bias(), filters());
    set_filters(DEFAULT_FILTERS | FILTER_TRILINEAR);
    set_level(16);
    set_bias(-0.3);
    let input = [world_s.as_raw(), post_s.as_raw(), point_s.as_raw(), shadow_s.as_raw(), ui_s.as_raw(), std::ptr::null_mut()];
    let mut out = [std::ptr::null_mut(); 16];
    let changed = unsafe { translate(input.as_ptr(), input.len() as u32, &mut out) };
    assert!(changed);
    // The world sampler is a new object with aniso and the bias; the engine's object is untouched.
    assert_ne!(out[0], input[0]);
    let d = desc_of(out[0]);
    assert_eq!((d.Filter, d.MaxAnisotropy), (D3D11_FILTER_ANISOTROPIC, 16));
    assert!((d.MipLODBias - -0.3).abs() < 1e-6);
    assert_eq!(desc_of(input[0]).Filter, D3D11_FILTER_MIN_MAG_MIP_LINEAR);
    // Bias applies to the world sampler only when the sampler qualifies: every other one is passed through as the same pointer.
    for i in 1..input.len() {
        assert_eq!(out[i], input[i], "sampler {i} must not be touched");
    }
    // Asked again: the same copy comes back (cached), nothing new is created.
    let seen = SEEN.load(Relaxed);
    let mut again = [std::ptr::null_mut(); 16];
    assert!(unsafe { translate(input.as_ptr(), input.len() as u32, &mut again) });
    assert_eq!(again[0], out[0]);
    assert_eq!(SEEN.load(Relaxed), seen);

    // Changing a setting rebuilds the copy with the new value.
    set_level(4);
    let mut lower = [std::ptr::null_mut(); 16];
    assert!(unsafe { translate(input.as_ptr(), 1, &mut lower) });
    assert_eq!(desc_of(lower[0]).MaxAnisotropy, 4);

    // Both settings neutral: nothing is substituted, the output equals the input.
    set_level(0);
    set_bias(0.0);
    let mut none = [std::ptr::null_mut(); 16];
    assert!(!unsafe { translate(input.as_ptr(), input.len() as u32, &mut none) });

    // Bad arguments pass through.
    set_level(16);
    assert!(!unsafe { translate(std::ptr::null(), 3, &mut none) });
    assert!(!unsafe { translate(input.as_ptr(), 0, &mut none) });
    assert!(!unsafe { translate(input.as_ptr(), 17, &mut none) });
    assert!(!status_text().is_empty());
    set_level(keep_l);
    set_filters(keep_f);
    set_bias(keep_b);
}

#[test]
fn translate_is_stable_and_bound_samplers_can_be_read_back_on_warp() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let Some((dev, ctx)) = warp() else { return };
    let w = sampler(&dev, D3D11_FILTER_MIN_MAG_MIP_LINEAR, D3D11_TEXTURE_ADDRESS_MIRROR, D3D11_COMPARISON_NEVER, f32::MAX, 1);
    let (keep_l, keep_b, keep_f) = (level(), bias(), filters());
    set_filters(DEFAULT_FILTERS | FILTER_TRILINEAR);
    set_level(8);
    set_bias(0.0);
    let input = [w.as_raw()];
    let mut a = [std::ptr::null_mut(); 16];
    let mut b = [std::ptr::null_mut(); 16];
    unsafe {
        assert!(translate(input.as_ptr(), 1, &mut a));
        assert!(translate(input.as_ptr(), 1, &mut b));
    }
    assert_eq!(a[0], b[0], "identical input gives the identical output");
    // What the real call would bind: PSSetSamplers with the translated pointer.
    unsafe {
        let s: ID3D11SamplerState = ID3D11SamplerState::from_raw_borrowed(&a[0]).unwrap().clone();
        ctx.PSSetSamplers(2, Some(&[Some(s)]));
        let mut got = [None];
        ctx.PSGetSamplers(2, Some(&mut got));
        let mut d = D3D11_SAMPLER_DESC::default();
        got[0].as_ref().unwrap().GetDesc(&mut d);
        assert_eq!((d.Filter, d.MaxAnisotropy), (D3D11_FILTER_ANISOTROPIC, 8));
    }
    set_level(keep_l);
    set_filters(keep_f);
    set_bias(keep_b);
}

#[test]
fn trilinear_samplers_are_left_alone_by_default() {
    // Forced anisotropy on the engine's trilinear samplers drew dashed lines along mesh edges in the game.
    let tri_wrap = Desc { filter: 0x15, u: 1, v: 1, w: 1, bias: 0.0, max_aniso: 0, cmp: 0, border: [0.0; 4], min_lod: 0.0, max_lod: f32::MAX };
    let tri_clamped = Desc { u: 3, v: 3, ..tri_wrap };
    let mip_point = Desc { filter: 0x14, ..tri_wrap };
    assert_eq!(filter_class(&tri_wrap), FILTER_TRILINEAR);
    assert_eq!(filter_class(&tri_clamped), FILTER_TRILINEAR_CLAMPED);
    assert_eq!(filter_class(&mip_point), FILTER_MIP_POINT);
    assert_eq!(DEFAULT_FILTERS & (FILTER_TRILINEAR | FILTER_TRILINEAR_CLAMPED), 0);
    assert_ne!(DEFAULT_FILTERS & FILTER_MIP_POINT, 0);
}
