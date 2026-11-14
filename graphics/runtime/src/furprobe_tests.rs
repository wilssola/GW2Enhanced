//! Tests of the per-draw fur probe (furprobe.rs): the tag, the texture decoders, the shader mirrors, the statistics and the report.

use super::*;

#[test]
fn the_tag_round_trips_through_16_bit_unorm_and_untagged_stays_untagged() {
    for k in 0..=MAX_TAGGED {
        let b = (tag_value(k) * 65535.0).round() as u16;
        assert!(tag_value(k) > 0.5 && tag_value(k) < 0.99, "{k}");
        assert_eq!(tag_of(b), Some(k), "{k}");
    }
    assert_eq!(tag_value(99), tag_value(MAX_TAGGED), "later draws share the last tag");
    assert_eq!(tag_of(65535), None, "b = 1: untagged (the mask as before)");
    assert_eq!(tag_of(0), None, "b = 0: no uv");
    assert_eq!(tag_of(32767), None);
    assert_eq!(colour_name(0), "red");
    assert_eq!(colour_name(9), "green");
}

#[test]
fn bc_blocks_decode_like_the_hardware() {
    // BC1: c0 = pure red 565, c1 = pure blue; indices 0, 1, 2, 3 in the first row.
    let mut b1 = vec![0x00, 0xF8, 0x1F, 0x00];
    b1.extend_from_slice(&[0b1110_0100, 0, 0, 0]);
    let px = decode(71, 4, 4, &b1).unwrap();
    assert_eq!(px[0], [255, 0, 0, 255]);
    assert_eq!(px[1], [0, 0, 255, 255]);
    assert_eq!(px[2], [170, 0, 85, 255]);
    assert_eq!(px[3], [85, 0, 170, 255]);
    assert_eq!(px[4], [255, 0, 0, 255]);
    // BC1 3-colour mode (c0 <= c1): index 3 = transparent black.
    let b1t = vec![0x1F, 0x00, 0x00, 0xF8, 0b1100_0000, 0, 0, 0];
    assert_eq!(decode(71, 4, 4, &b1t).unwrap()[3], [0, 0, 0, 0]);
    // BC4: a0 = 255, a1 = 0 (8 values); indices 0, 1, 2 in the first texels.
    let mut b4 = vec![255u8, 0];
    let bits: u64 = 1 << 3 | 2 << 6;
    b4.extend_from_slice(&bits.to_le_bytes()[..6]);
    let px = decode(80, 4, 4, &b4).unwrap();
    assert_eq!((px[0][0], px[1][0], px[2][0]), (255, 0, 219));
    // BC3 = BC4 alpha + BC1 colour (always 4 colours).
    let mut b3 = b4.clone();
    b3.extend_from_slice(&b1t);
    let px = decode(77, 4, 4, &b3).unwrap();
    assert_eq!(px[0], [0, 0, 255, 255]);
    assert_eq!(px[1][3], 0);
    assert_eq!(px[3][3], 255, "BC3 colour index 3 is opaque (4-colour mode)");
    // BC5 = two BC4.
    let mut b5 = b4.clone();
    b5.extend_from_slice(&[0, 255, 0, 0, 0, 0, 0, 0]);
    let px = decode(83, 4, 4, &b5).unwrap();
    assert_eq!((px[0][0], px[0][1], px[2][0]), (255, 0, 219));
    // Uncompressed BGRA swaps.
    assert_eq!(decode(87, 1, 1, &[1, 2, 3, 4]).unwrap()[0], [3, 2, 1, 4]);
    assert!(decode(2, 4, 4, &[0; 64]).is_none(), "an unknown format");
    assert!(decode(77, 8, 8, &[0; 16]).is_none(), "too little data");
}

fn img(w: u32, h: u32, f: impl Fn(u32, u32) -> [u8; 4]) -> Img {
    Img { w, h, px: (0..h).flat_map(|y| (0..w).map(move |x| (x, y))).map(|(x, y)| f(x, y)).collect() }
}

#[test]
fn the_sampling_mirrors_wrap_and_measure_detail() {
    let t = img(2, 2, |x, y| [if x == 0 { 0 } else { 255 }, if y == 0 { 0 } else { 255 }, 0, 255]);
    let c = sample_bilinear(&t, [0.25, 0.25]);
    assert!(c[0].abs() < 1e-6 && c[1].abs() < 1e-6, "texel centre {c:?}");
    let c = sample_bilinear(&t, [0.5, 0.25]);
    assert!((c[0] - 0.5).abs() < 1e-6);
    let c = sample_bilinear(&t, [1.0, 0.25]); // halfway between texel 1 and texel 0 again (wrap)
    assert!((c[0] - 0.5).abs() < 1e-6, "{c:?}");
    assert_eq!(sample_bilinear(&t, [1.25, -0.75]), sample_bilinear(&t, [0.25, 0.25]), "frac(uv)");
    let flat = img(16, 16, |_, _| [128, 128, 0, 255]);
    assert!(detail_at(&flat, [0.3, 0.7]) < 1e-6);
    let checker = img(16, 16, |x, y| if (x + y) % 2 == 0 { [255, 255, 0, 255] } else { [0, 0, 0, 255] });
    assert!(detail_at(&checker, [0.5, 0.5]) > 0.99, "{}", detail_at(&checker, [0.5, 0.5]));
    assert_eq!(level_for(4096, 4096, 11, 512), 3);
    assert_eq!(level_for(4096, 4096, 11, 1024), 2);
    assert_eq!(level_for(4096, 4096, 2, 512), 1, "a view with fewer mips stops at its last");
    assert_eq!(level_for(256, 256, 9, 512), 0);
    assert!((saturation([1.0, 0.5, 0.5, 1.0]) - 0.5).abs() < 1e-6 && saturation([0.0; 4]) == 0.0);
}

#[test]
fn the_histogram_counts_and_finds_quantiles() {
    let mut h = Hist::default();
    for k in 0..100 {
        h.add(k as f32 / 100.0);
    }
    h.add(f32::NAN);
    h.add(7.0);
    assert_eq!(h.n, 101);
    assert_eq!(h.bins[BINS - 1], 6 + 1, "0.94..0.99 and the clamped 7.0");
    assert!((h.quantile(0.5) - 0.5).abs() <= 1.0 / BINS as f32 + 1e-6);
    assert!(h.text().starts_with("p10/50/90"));
    assert_eq!(Hist::default().text(), "no samples");
}

#[test]
fn the_pixels_are_split_per_draw_and_the_points_report_the_texture_values() {
    // Two draws on one atlas: draw #0 left half (alpha 0.5), draw #1 right half (alpha 0.9, smooth). 8 x 2 screen.
    let atlas = img(8, 8, |x, _| if x < 4 { [200, 200, 200, 128] } else { [100, 100, 100, 230] });
    let tex = vec![TexData { img512: atlas.clone(), img1024: atlas, crc: 1, ..TexData::default() }];
    let draws = vec![DrawMeta { idx: 0, count: 3354, shells: true, tex: [Some(0), None], ..DrawMeta::default() }, DrawMeta { idx: 1, count: 2712, shells: true, tex: [Some(0), None], ..DrawMeta::default() }];
    let tg = |k: u32| (tag_value(k) * 65535.0).round() as u16;
    let a = [(0.3 * 65535.0) as u16, 32768, tg(0), 65535];
    let b = [(0.8 * 65535.0) as u16, 32768, tg(1), 0];
    let none = [0u16; 4];
    let untag = [100u16, 100, 65535, 65535];
    let tx = vec![a, a, a, b, b, untag, none, none, a, a, b, b, b, b, none, none];
    let st: Vec<f32> = tx.iter().map(|t| if t[2] > 0 { 0.5 } else { 2.0 }).collect();
    let (s, untagged, pts) = analyze_pixels(&tx, &st, 8, 2, &draws, &tex, &[("centre".into(), 3, 0), ("mouse".into(), 7, 1)]);
    assert_eq!((s[0].px, s[1].px, untagged), (5, 6, 1));
    assert_eq!((s[0].not_fur, s[1].not_fur), (0, 6));
    assert!((s[0].a.quantile(0.5) - 0.5625).abs() < 1e-6, "alpha 0.50 lands in bin 8");
    assert!((s[1].a.quantile(0.5) - 0.9375).abs() < 1e-6);
    let (_, _, _, p) = &pts[0];
    assert_eq!(p.draw, Some(1));
    assert!((p.t0.unwrap()[3] - 230.0 / 255.0).abs() < 0.01 && p.t1.is_none() && p.dist == 0.0);
    let (_, _, _, m) = &pts[1];
    assert_eq!(m.draw, Some(1));
    assert!(m.dist > 0.0, "the nearest drawn pixel");
    let t = report(&draws, &tex, &s, untagged, &pts, &["furtex-00000001-rgb/-a/-detail.png".into()]);
    for want in ["#0 red [", "#1 green [", "count 3354", "count 2712", "not fur 100.0 %", "untagged fur px 1", "centre px (3, 0): draw #1 green", "t0 rgba (", "files in dev\\: furtex-00000001"] {
        assert!(t.contains(want), "{want}: {t}");
    }
    // The overlay plots the draws' uvs in their colours on the dimmed atlas, not-fur texels white.
    let o = overlay(&tex[0].img512, &tx, &st, &draws, 0, &[]);
    assert_eq!(o.len(), 8 * 8 * 3);
    let at = |x: usize, y: usize| [o[(y * 8 + x) * 3], o[(y * 8 + x) * 3 + 1], o[(y * 8 + x) * 3 + 2]];
    assert_eq!(at(2, 4), [255, 0, 0], "draw #0 at uv (0.3, 0.5)");
    assert_eq!(at(6, 4), [255, 255, 255], "draw #1 at uv (0.8, 0.5): not fur, white");
    assert_eq!(at(0, 0), [80, 80, 80], "the dimmed atlas");
}

/// WARP: the live path. A BC3 colour atlas (64 x 64, 7 mips, every level a constant alpha of its own) with a view that starts at mip 1
/// (a streamed texture) and a BC5 normal map are bound like the engine's fur draw; `textures` reads what is bound (vtable plumbing, view
/// mips), `push_draw` / `analyze` read the right resource level back, the thread decodes and samples it and the report says so.
#[test]
fn warp_the_bound_textures_are_read_back_at_the_shaders_level_and_reported() {
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::*;
    use windows::Win32::Graphics::Direct3D11::*;
    use windows::Win32::Graphics::Dxgi::Common::*;
    use windows::core::Interface;
    let _g = crate::testlock::lock();
    let (mut dev, mut ctx) = (None, None);
    let r = unsafe { D3D11CreateDevice(None::<&windows::Win32::Graphics::Dxgi::IDXGIAdapter>, D3D_DRIVER_TYPE_WARP, HMODULE::default(), D3D11_CREATE_DEVICE_FLAG(0), None, D3D11_SDK_VERSION, Some(&mut dev), None, Some(&mut ctx)) };
    let (Ok(()), Some(dev), Some(ctx)) = (r, dev, ctx) else {
        println!("skipped: no WARP device");
        return;
    };
    let alpha_of = |l: u32| 20 + 30 * l as u8;
    let make = |fmt: DXGI_FORMAT, block: &dyn Fn(u32) -> [u8; 16]| -> ID3D11Texture2D {
        let levels: Vec<Vec<u8>> = (0..7u32).map(|l| { let n = ((64 >> l) as usize / 4).max(1); (0..n * n).flat_map(|_| block(l)).collect() }).collect();
        let sd: Vec<D3D11_SUBRESOURCE_DATA> = levels.iter().enumerate().map(|(l, b)| D3D11_SUBRESOURCE_DATA { pSysMem: b.as_ptr() as *const std::ffi::c_void, SysMemPitch: ((64 >> l) as u32 / 4).max(1) * 16, SysMemSlicePitch: 0 }).collect();
        let desc = D3D11_TEXTURE2D_DESC { Width: 64, Height: 64, MipLevels: 7, ArraySize: 1, Format: fmt, SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, Usage: D3D11_USAGE_IMMUTABLE, BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32, CPUAccessFlags: 0, MiscFlags: 0 };
        let mut t = None;
        unsafe { dev.CreateTexture2D(&desc, Some(sd.as_ptr()), Some(&mut t)).unwrap() };
        t.unwrap()
    };
    // BC3: alpha constant per level (a0 = a1), colour pure green.
    let c0 = make(DXGI_FORMAT_BC3_UNORM, &|l| { let a = alpha_of(l); [a, a, 0, 0, 0, 0, 0, 0, 0xE0, 0x07, 0xE0, 0x07, 0, 0, 0, 0] });
    // BC5: r = 128, g = 64.
    let c1 = make(DXGI_FORMAT_BC5_UNORM, &|_| [128, 128, 0, 0, 0, 0, 0, 0, 64, 64, 0, 0, 0, 0, 0, 0]);
    let vd = D3D11_SHADER_RESOURCE_VIEW_DESC {
        Format: DXGI_FORMAT_BC3_UNORM,
        ViewDimension: D3D_SRV_DIMENSION_TEXTURE2D,
        Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_SRV { MostDetailedMip: 1, MipLevels: 3 } },
    };
    let (mut v0, mut v1) = (None, None);
    unsafe {
        dev.CreateShaderResourceView(&c0, Some(&vd), Some(&mut v0)).unwrap();
        dev.CreateShaderResourceView(&c1, None, Some(&mut v1)).unwrap();
        ctx.PSSetShaderResources(0, Some(&[v0, v1]));
    }
    frame_begin(true);
    assert!(collecting() && tagging());
    let k = next_index();
    let tx = unsafe { textures(ctx.as_raw()) };
    {
        let t0 = tx.0[0].as_ref().expect("t0");
        let t1 = tx.0[1].as_ref().expect("t1");
        assert_eq!((t0.d.width, t0.d.mips, t0.d.format, t0.view_fmt, t0.most, t0.view_mips), (64, 7, 77, 77, 1, 3));
        assert_eq!((t1.d.format, t1.most, t1.view_mips), (83, 0, 7));
    }
    push_draw(k, 0x1d50763f, Some(0xba0709f4), 16464, Call::Indexed(2712, 30, 4), true, tx);
    // A 4 x 1 mask: three pixels of this draw at uv (0.5, 0.5), one not drawn.
    let b = (tag_value(k) * 65535.0).round() as u16;
    let texels = vec![[32768, 32768, b, 65535], [32768, 32768, b, 65535], [32768, 32768, b, 0], [0, 0, 0, 0]];
    let stamp = vec![0.5, 0.5, 0.5, 2.0];
    *REPORT.lock().unwrap() = String::new();
    unsafe { analyze(dev.as_raw(), ctx.as_raw(), texels, stamp, 4, 1, vec![("centre".into(), 1, 0)]) };
    for _ in 0..500 {
        if !BUSY.load(Relaxed) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let t = report_text();
    println!("{t}");
    let a1 = alpha_of(1) as f32 / 255.0;
    for want in [
        "#0 red [vs ba0709f4 ps 1d50763f vcb 16464 count 2712 start 30 base 4 inst 1 -> shells".to_string(),
        "BC3_UNORM m7 (view BC3_UNORM mips 1+3; sampled at level 1 = 32x32)".to_string(),
        "BC5_UNORM m7 (view BC5_UNORM mips 0+7; sampled at level 0 = 64x64)".to_string(),
        "3 px (100.0 % of the fur px), not fur 33.3 %".to_string(),
        format!("t0 rgba (0.000, 1.000, 0.000, {a1:.3})"),
        "t1 rg (0.502, 0.251)".to_string(),
        "t1 detail 0.000".to_string(),
    ] {
        assert!(t.contains(&want), "{want}\n{t}");
    }
    frame_begin(false);
    assert!(!collecting() && !tagging());
    unsafe { ctx.PSSetShaderResources(0, Some(&[None, None])) };
}

#[test]
fn nothing_collected_reports_plainly() {
    let (s, u, p) = analyze_pixels(&[], &[], 0, 0, &[], &[], &[("centre".into(), 0, 0)]);
    assert!(s.is_empty() && u == 0);
    let t = report(&[], &[], &s, u, &p, &[]);
    assert!(t.starts_with("per-draw probe (0 fur draws collected, 0 visible"), "{t}");
}
