use super::*;

// ---- Radiance RGBE ------------------------------------------------------------------------------------------------------------------

fn close(a: [f32; 3], b: [f32; 3]) -> bool {
    // 8-bit mantissa shared by the three channels: error is relative to the LARGEST channel (about 0.4 %).
    let m = b[0].max(b[1]).max(b[2]);
    (0..3).all(|i| (a[i] - b[i]).abs() <= m * 0.005 + 1e-30)
}

#[test]
fn rgbe_round_trips_hdr_values() {
    for c in [[1.0f32, 1.0, 1.0], [0.5, 0.25, 0.125], [2.5375, 1.0, 0.1], [12.5, 3.0, 0.5], [1000.0, 10.0, 0.01], [65504.0, 1.0, 0.0], [0.18, 0.18, 0.18]] {
        let back = rgbe_decode(rgbe_encode(c));
        assert!(close(back, c), "{c:?} -> {back:?}");
    }
    // exact powers of two land on the mantissa boundary: no off-by-one exponent
    for e in -20..20 {
        let v = 2f32.powi(e);
        let back = rgbe_decode(rgbe_encode([v, v * 0.5, 0.0]));
        assert!(close(back, [v, v * 0.5, 0.0]), "2^{e}: {back:?}");
    }
}

#[test]
fn rgbe_handles_tiny_zero_nan_negative_and_infinite() {
    assert_eq!(rgbe_encode([0.0, 0.0, 0.0]), [0, 0, 0, 0]);
    assert_eq!(rgbe_decode([0, 0, 0, 0]), [0.0; 3]);
    // tiny but representable
    let t = rgbe_decode(rgbe_encode([1.0e-6, 5.0e-7, 1.0e-7]));
    assert!(close(t, [1.0e-6, 5.0e-7, 1.0e-7]) && t[0] > 0.0, "{t:?}");
    // below the representable range: black, not garbage
    assert_eq!(rgbe_encode([1.0e-40, 0.0, 0.0]), [0, 0, 0, 0]);
    // NaN and negative count as 0, the other channels survive
    let n = rgbe_decode(rgbe_encode([f32::NAN, -3.0, 2.0]));
    assert_eq!(n[0], n[1]);
    assert!(n[0] <= 0.01 && (n[2] - 2.0).abs() < 0.02, "a zero byte decodes to half a step at most: {n:?}");
    assert_eq!(rgbe_encode([f32::NAN, f32::NAN, -1.0]), [0, 0, 0, 0]);
    // infinity is clamped, never wraps to a small value
    let i = rgbe_decode(rgbe_encode([f32::INFINITY, 1.0, 1.0]));
    assert!(i[0] > 1.0e20 && i[0].is_finite(), "{i:?}");
    // every output byte pattern decodes to finite non-negative numbers
    for e in [1u8, 100, 128, 200, 255] {
        for m in [0u8, 1, 127, 255] {
            assert!(rgbe_decode([m, 255 - m, m / 2, e]).iter().all(|v| v.is_finite() && *v >= 0.0));
        }
    }
}

#[test]
fn a_radiance_file_round_trips_and_has_a_valid_header() {
    let (w, h) = (5u32, 3u32);
    let mut rgb = Vec::new();
    for i in 0..(w * h) as usize {
        let t = i as f32 + 1.0;
        rgb.extend_from_slice(&[t * 0.7, t * 0.1, t * 3.0]);
    }
    rgb[4] = f32::NAN; // poison one channel
    let f = encode_radiance(w, h, &rgb);
    assert!(f.starts_with(b"#?RADIANCE\n"));
    let text = String::from_utf8_lossy(&f[..f.len().min(200)]).to_string();
    assert!(text.contains("FORMAT=32-bit_rle_rgbe"));
    let needle: &[u8] = b"\n-Y 3 +X 5\n";
    assert!(f.windows(needle.len()).any(|x| x == needle), "resolution line");
    let (w2, h2, back) = decode_radiance(&f).expect("decodes");
    assert_eq!((w2, h2, back.len()), (w, h, rgb.len()));
    for i in 0..(w * h) as usize {
        let a = [rgb[i * 3], if rgb[i * 3 + 1].is_nan() { 0.0 } else { rgb[i * 3 + 1] }, rgb[i * 3 + 2]];
        assert!(close([back[i * 3], back[i * 3 + 1], back[i * 3 + 2]], a), "pixel {i}");
    }
    // truncated and wrong files are refused
    assert!(decode_radiance(&f[..f.len() - 1]).is_none());
    assert!(decode_radiance(b"nope").is_none());
}

#[test]
fn halves_convert_both_ways() {
    for v in [0.0f32, 1.0, -1.0, 0.5, 2.5375, 1000.0, 65504.0, 6.1e-5, 1.0e-6, 0.333_333, 12.5] {
        let h = f32_to_f16(v);
        let back = debugshot::f16_to_f32(h);
        assert!((back - v).abs() <= v.abs() * 1.0e-3 + 6.0e-8, "{v} -> {back}");
    }
    assert_eq!(f32_to_f16(1.0), 0x3c00);
    assert_eq!(f32_to_f16(-2.0), 0xc000);
    assert_eq!(f32_to_f16(1.0e9), 0x7c00, "overflow is infinity");
    assert_eq!(debugshot::f16_to_f32(f32_to_f16(f32::INFINITY)), f32::INFINITY);
    assert!(debugshot::f16_to_f32(f32_to_f16(f32::NAN)).is_nan());
    // all finite halves survive a round trip
    for h in 0u16..0x7c00 {
        assert_eq!(f32_to_f16(debugshot::f16_to_f32(h)), h, "{h:#x}");
    }
}

// ---- disk space ---------------------------------------------------------------------------------------------------------------------

#[test]
fn the_free_space_guard() {
    let need = photo_bytes(3440, 1440, true);
    assert!(need > 30_000_000 && need < 40_000_000, "{need}");
    assert_eq!(space_check(Some(10 * MIN_FREE_BYTES), need), Space::Ok);
    assert!(matches!(space_check(Some(MIN_FREE_BYTES), need), Space::Low(..)), "exactly the minimum is not enough");
    assert!(matches!(space_check(Some(MIN_FREE_BYTES + need), need), Space::Low(..)), "the files must fit on top of the minimum");
    assert_eq!(space_check(Some(MIN_FREE_BYTES + need + 1), need), Space::Ok);
    assert!(matches!(space_check(Some(0), need), Space::Low(0, _)));
    assert_eq!(space_check(None, need), Space::Unknown);
    assert_eq!(MIN_FREE_BYTES, 200 * 1024 * 1024);
    // overflow-safe
    assert!(matches!(space_check(Some(5), u64::MAX), Space::Low(..)));
    // the real query works on this machine's temp drive
    assert!(free_bytes(&std::env::temp_dir()).is_some());
}

#[test]
fn names_carry_a_utc_timestamp_and_never_repeat() {
    assert_eq!(format_utc(0), "19700101-000000");
    assert_eq!(format_utc(1_000_000_000), "20010909-014640");
    assert_eq!(format_utc(1_700_000_000), "20231114-221320");
    assert_eq!(format_utc(951_782_400), "20000229-000000", "leap day");
    assert_eq!(unique_stem(1_700_000_000, |_| false), "gw2-20231114-221320");
    assert_eq!(unique_stem(1_700_000_000, |s| s == "gw2-20231114-221320"), "gw2-20231114-221320-2");
    assert_eq!(unique_stem(1_700_000_000, |s| s == "gw2-20231114-221320" || s == "gw2-20231114-221320-2"), "gw2-20231114-221320-3");
}

// ---- hotkey -------------------------------------------------------------------------------------------------------------------------

#[test]
fn the_photo_hotkey_parses_and_does_not_clash_with_the_overlay_key() {
    let cfg = crate::config::Config::default();
    let (pk, pm) = crate::overlay::parse_key(&cfg.photo_key).expect("default photo key parses");
    let (ok, om) = crate::overlay::parse_key(&cfg.overlay_key).expect("overlay key parses");
    assert_eq!((pk, pm), (0x23, 1 | 2), "Ctrl+Shift+End");
    assert_eq!((ok, om), (0x23, 1), "Shift+End");
    let down = |keys: &'static [i32]| move |vk: i32| keys.contains(&vk);
    // Ctrl+Shift+End triggers the photo key only
    assert!(combo_down(down(&[0x23, 0x10, 0x11]), pk, pm));
    assert!(!combo_down(down(&[0x23, 0x10, 0x11]), ok, om));
    // Shift+End triggers the overlay only
    assert!(combo_down(down(&[0x23, 0x10]), ok, om));
    assert!(!combo_down(down(&[0x23, 0x10]), pk, pm));
    // the bare key and a missing key trigger nothing; an extra Alt spoils the match
    assert!(!combo_down(down(&[0x23]), pk, pm));
    assert!(!combo_down(down(&[0x10, 0x11]), pk, pm));
    assert!(!combo_down(down(&[0x23, 0x10, 0x11, 0x12]), pk, pm));
    // other spellings
    assert_eq!(crate::overlay::parse_key("ctrl+alt+f12"), Some((0x7b, 2 | 4)));
    assert_eq!(crate::overlay::parse_key("nonsense+"), None);
}

// ---- the capture queue --------------------------------------------------------------------------------------------------------------

#[test]
fn the_queue_is_fifo_and_bounded() {
    let mut q = PhotoQueue::new();
    assert_eq!(q.peek(), None);
    assert_eq!(q.push(PhotoKind::Single), Ok(1));
    assert_eq!(q.push(PhotoKind::Pair), Ok(2));
    assert_eq!(q.push(PhotoKind::Single), Ok(3));
    assert!(q.push(PhotoKind::Single).is_err(), "full");
    assert_eq!(q.len(), QUEUE_MAX);
    assert_eq!(q.pop(), Some(PhotoKind::Single));
    assert_eq!(q.pop(), Some(PhotoKind::Pair));
    assert_eq!(q.peek(), Some(PhotoKind::Single));
    assert_eq!(q.pop(), Some(PhotoKind::Single));
    assert_eq!(q.pop(), None);
}

#[test]
fn only_one_capture_at_a_time() {
    use PhotoKind::*;
    // nothing queued: the dev logic goes on
    assert_eq!(arbitrate(None, false, false, 3, true), Plan::Idle);
    assert_eq!(arbitrate(None, true, true, 0, true), Plan::Idle);
    // a pair or a scene grab is running: the photo waits, and nothing else may start either (Wait, not Idle)
    assert_eq!(arbitrate(Some(Single), true, false, 3, true), Plan::Wait);
    assert_eq!(arbitrate(Some(Pair), true, false, 3, true), Plan::Wait);
    assert_eq!(arbitrate(Some(Single), false, true, 3, true), Plan::Wait);
    // free slots decide
    assert_eq!(arbitrate(Some(Single), false, false, 1, true), Plan::Single);
    assert_eq!(arbitrate(Some(Single), false, false, 0, true), Plan::Wait);
    assert_eq!(arbitrate(Some(Pair), false, false, 2, true), Plan::Pair);
    assert_eq!(arbitrate(Some(Pair), false, false, 1, true), Plan::Wait, "a pair needs two slots");
    // mod already off: no vanilla to show, one shot is taken
    assert_eq!(arbitrate(Some(Pair), false, false, 1, false), Plan::Single);
    assert_eq!(arbitrate(Some(Pair), false, false, 3, false), Plan::Single);
    assert_eq!(scene_step(100, 100 + SCENE_TIMEOUT_FRAMES), SceneStep::Wait);
    assert_eq!(scene_step(100, 101 + SCENE_TIMEOUT_FRAMES), SceneStep::Timeout);
    assert_eq!(scene_step(100, 50), SceneStep::Wait, "counter going back");
}

/// A scheduler that mixes photo requests with the dev timer and the dev pair machine, in the order `capture_step` uses: the pair machine
/// first, then `arbitrate`, then the dev logic. At no step do two captures start, a queued photo is never starved by the dev timer and the
/// dev timer is never starved for good.
#[test]
fn photos_and_the_dev_timer_never_run_together_and_both_get_their_turn() {
    use crate::debugshot::{PairMachine, PairStep, capture_due};
    let mut q = PhotoQueue::new();
    let mut m = PairMachine::new();
    let (mut now, mut frame) = (0u64, 0u64);
    let (mut photos, mut devs, mut last_dev) = (0, 0, None);
    let mut busy_until = 0u64; // a started capture occupies the writer for a while
    let mut pair_is_photo = false;
    for step in 0..4000u64 {
        now += 16;
        frame += 1;
        if step % 300 == 5 {
            let _ = q.push(if step % 600 == 5 { PhotoKind::Pair } else { PhotoKind::Single });
        }
        let started_before = photos + devs;
        match m.step(now, frame, true) {
            PairStep::Wait => continue, // a pair runs: nothing else starts
            PairStep::Capture | PairStep::Abort => {
                pair_is_photo = false;
                continue;
            }
            PairStep::Idle => {}
        }
        let _ = pair_is_photo;
        if now < busy_until {
            continue;
        }
        match arbitrate(q.peek(), m.is_forced(), false, 3, true) {
            Plan::Idle => {
                if capture_due(now, last_dev, 2) {
                    last_dev = Some(now);
                    devs += 1;
                    assert!(m.begin(now, frame), "no pair was running");
                    busy_until = now + 100;
                }
            }
            Plan::Wait => {}
            Plan::Single => {
                q.pop();
                photos += 1;
                busy_until = now + 100;
            }
            Plan::Pair => {
                q.pop();
                photos += 1;
                assert!(m.begin(now, frame), "no pair was running");
                pair_is_photo = true;
                busy_until = now + 100;
            }
        }
        assert!(photos + devs - started_before <= 1, "two captures started in one step");
    }
    assert!(photos >= 10, "photos starved: {photos}");
    assert!(devs >= 10, "the dev timer starved: {devs}");
    assert_eq!(q.len(), 0, "every queued photo was taken");
}

// ---- pixels ------------------------------------------------------------------------------------------------------------------------

fn f16_px(c: [f32; 3]) -> Vec<u8> {
    let mut v = Vec::new();
    for x in [c[0], c[1], c[2], 1.0] {
        v.extend_from_slice(&f32_to_f16(x).to_le_bytes());
    }
    v
}

#[test]
fn linear_rows_are_paper_relative_and_sanitised() {
    let mut data = f16_px([2.5375, 5.075, 0.0]);
    data.extend(f16_px([f32::NAN, -1.0, 1.0e6]));
    let lin = linear_rows(&data, 2, 1, PixFmt::F16, 80.0 / 203.0).unwrap();
    assert!((lin[0] - 203.0 / 80.0 * 80.0 / 203.0).abs() < 2e-3, "paper white 203 nits = 1.0: {}", lin[0]);
    assert!((lin[1] - 2.0).abs() < 4e-3);
    assert_eq!(lin[2], 0.0);
    assert_eq!((lin[3], lin[4]), (0.0, 0.0), "NaN and negative are 0");
    assert!(lin[5].is_finite() && lin[5] <= 65504.0, "{}", lin[5]);
    assert!(linear_rows(&data[..4], 2, 1, PixFmt::F16, 1.0).is_none(), "short data");
    assert!(linear_rows(&data, 2, 1, PixFmt::F16, f32::NAN).is_none());
    // 8-bit: sRGB decode
    let lin = linear_rows(&[255, 255, 255, 255, 0, 0, 0, 255], 2, 1, PixFmt::Bgra8, 1.0).unwrap();
    assert!((lin[0] - 1.0).abs() < 1e-5 && lin[3] == 0.0);
    let srgb = srgb8_rows(&[1.0, 0.0, 4.0, 0.2140]);
    assert_eq!(srgb, vec![255, 0, 255, 127]);
}

#[test]
fn hdr10_pixels_decode_to_the_scrgb_primaries() {
    // 203 nits white as 10-bit PQ on all channels, and a saturated 709 red
    let code = |c: f32| hdr10::quantize10(c) as u32;
    let white = code(hdr10::pq_encode(203.0));
    let px = |r: u32, g: u32, b: u32| (r | (g << 10) | (b << 20) | (3 << 30)).to_le_bytes().to_vec();
    let lin = linear_rows(&px(white, white, white), 1, 1, PixFmt::Rgb10A2, 80.0 / 203.0).unwrap();
    for c in lin {
        assert!((c - 1.0).abs() < 0.01, "paper white decodes to 1.0, got {c}");
    }
    let red = hdr10::scrgb_to_pq([203.0 / 80.0, 0.0, 0.0]);
    let lin = linear_rows(&px(code(red[0]), code(red[1]), code(red[2])), 1, 1, PixFmt::Rgb10A2, 80.0 / 203.0).unwrap();
    assert!((lin[0] - 1.0).abs() < 0.02 && lin[1].abs() < 0.02 && lin[2].abs() < 0.02, "{lin:?}");
    // the debug capture's statistics read the same format
    let c = debugshot::convert(&px(white, white, white).repeat(4), 2, 2, PixFmt::Rgb10A2, 80.0 / 203.0).unwrap();
    assert!((c.stats.mean_lum - 1.0).abs() < 0.02, "{}", c.stats.mean_lum);
    assert_eq!(debugshot::pixfmt(24), Some(PixFmt::Rgb10A2));
    assert_eq!(debugshot::bytes_per_px(PixFmt::Rgb10A2), 4);
}

#[test]
fn the_scene_conversion_matches_the_final_pass_maths() {
    // SDR: clamped and decoded, paper 80 -> scale 1
    let map = SceneMap { hdr: false, paper: 203.0, peak: 1000.0, gamma: 0.0 };
    let mut d = f16_px([1.0, 0.5, 2.0]);
    scene_to_scrgb_f16(&mut d, &map);
    let lin = linear_rows(&d, 1, 1, PixFmt::F16, 1.0).unwrap();
    assert!((lin[0] - 1.0).abs() < 1e-3 && (lin[1] - crate::tonemap::srgb_decode(0.5)).abs() < 1e-3 && (lin[2] - 1.0).abs() < 1e-3, "over-range clips in SDR: {lin:?}");
    // HDR: values above 1 survive (shoulder under the peak), paper white maps to 1.0 after the scale
    let map = SceneMap { hdr: true, paper: 203.0, peak: 1000.0, gamma: 2.2 };
    let mut d = f16_px([1.0, 1.0, 1.0]);
    d.extend(f16_px([4.0, 4.0, 4.0]));
    scene_to_scrgb_f16(&mut d, &map);
    let lin = linear_rows(&d, 2, 1, PixFmt::F16, 80.0 / 203.0).unwrap();
    assert!((lin[0] - 1.0).abs() < 2e-3, "{}", lin[0]);
    assert!(lin[3] > 1.5 && lin[3] <= 1000.0 / 203.0 + 1e-3, "bright stays above paper white, below the peak: {}", lin[3]);
    // direct maths
    let c = final_pass_cpu([1.0, 0.5, 0.25], 203.0, 1000.0);
    assert!((c[0] - 203.0 / 80.0).abs() < 1e-5);
    assert!(final_pass_cpu([50.0, 50.0, 50.0], 203.0, 1000.0).iter().all(|v| *v <= 1000.0 / 80.0 + 1e-4));
    assert_eq!(final_pass_cpu([0.0; 3], 203.0, 1000.0), [0.0; 3]);
    assert_eq!(final_pass_cpu([-1.0, f32::NAN.max(0.0), 0.0], 203.0, 1000.0), [0.0; 3]);
}

// ---- writing ------------------------------------------------------------------------------------------------------------------------

fn temp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("gw2g_photo_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

#[test]
fn a_photo_is_written_as_png_hdr_and_never_overwritten() {
    let dir = temp_dir("save");
    let (w, h) = (6u32, 4u32);
    let lin: Vec<f32> = (0..(w * h * 3) as usize).map(|i| i as f32 * 0.1).collect();
    let s = save_files(&dir, "gw2-test", "", w, h, &lin, "frame=1\nenabled=true", true).expect("saved");
    assert!(s.png.exists() && s.hdr.as_ref().is_some_and(|p| p.exists()));
    let png = std::fs::read(&s.png).unwrap();
    assert_eq!(&png[1..4], b"PNG");
    assert!(String::from_utf8_lossy(&png).contains("Settings"), "settings travel in the PNG");
    let (rw, rh, back) = decode_radiance(&std::fs::read(s.hdr.unwrap()).unwrap()).unwrap();
    assert_eq!((rw, rh), (w, h));
    assert!(back[(w * h * 3 - 1) as usize] > 2.0, "values above 1.0 are kept in the .hdr");
    // a second photo with the same name is refused, the first stays intact
    let before = std::fs::read(&s.png).unwrap();
    assert!(save_files(&dir, "gw2-test", "", w, h, &lin, "x", true).unwrap_err().contains("not overwritten"));
    assert_eq!(std::fs::read(&s.png).unwrap(), before);
    // suffixes (pair)
    assert!(save_files(&dir, "gw2-test", "-mod", w, h, &lin, "x", false).unwrap().hdr.is_none());
    let names: Vec<String> = std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().to_string()).collect();
    assert!(names.iter().all(|n| !n.ends_with(".tmp")), "{names:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn status_lines_report_the_state() {
    set_hotkey_text("Ctrl+Shift+End");
    let r = request(PhotoKind::Single);
    assert!(!r.is_empty());
    let t = status_text();
    assert!(t.contains("photo_hide_ui=") && t.contains("hotkey Ctrl+Shift+End") && t.contains("queue"), "{t}");
    while pop_queued().is_some() {}
}

// ---- the UI-free scene grab on WARP ------------------------------------------------------------------------------------------------

#[test]
fn the_scene_grab_copies_the_bound_texture_once_and_reads_it_back() {
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::*;
    use windows::Win32::Graphics::Direct3D11::*;
    use windows::Win32::Graphics::Dxgi::Common::*;
    use windows::core::Interface;
    let (mut dev, mut ctx) = (None, None);
    let r = unsafe { D3D11CreateDevice(None::<&windows::Win32::Graphics::Dxgi::IDXGIAdapter>, D3D_DRIVER_TYPE_WARP, HMODULE::default(), D3D11_CREATE_DEVICE_FLAG(0), None, D3D11_SDK_VERSION, Some(&mut dev), None, Some(&mut ctx)) };
    let (Ok(()), Some(dev), Some(ctx)) = (r, dev, ctx) else {
        println!("skipped: WARP device creation failed");
        return;
    };
    let (w, h) = (320u32, 200u32);
    let mut src = Vec::new();
    for i in 0..(w * h) {
        src.extend(f16_px([i as f32 * 0.001, 0.5, 3.0]));
    }
    unsafe {
        let desc = D3D11_TEXTURE2D_DESC { Width: w, Height: h, MipLevels: 1, ArraySize: 1, Format: DXGI_FORMAT_R16G16B16A16_FLOAT, SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, Usage: D3D11_USAGE_DEFAULT, BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32, ..Default::default() };
        let sd = D3D11_SUBRESOURCE_DATA { pSysMem: src.as_ptr() as *const std::ffi::c_void, SysMemPitch: w * 8, SysMemSlicePitch: 0 };
        let mut tex = None;
        dev.CreateTexture2D(&desc, Some(&sd), Some(&mut tex)).unwrap();
        let tex = tex.unwrap();
        let mut srv = None;
        dev.CreateShaderResourceView(&tex, None, Some(&mut srv)).unwrap();
        ctx.PSSetShaderResources(0, Some(&[srv]));
        let raw = ctx.as_raw();
        // not armed: nothing happens
        disarm_scene("test");
        grab_scene(raw);
        assert!(!scene_pending());
        // armed: one copy, the flag is consumed
        arm_scene(100);
        assert!(scene_wanted());
        grab_scene(raw);
        assert!(!scene_wanted(), "single shot");
        assert!(scene_pending());
        // too young to read
        assert!(matches!(take_scene(raw, 101), Ok(None)));
        // old enough: the bytes are the texture's
        ctx.Flush();
        let mut got = None;
        for frame in 103..203 {
            std::thread::sleep(std::time::Duration::from_millis(10));
            if let Ok(Some(s)) = take_scene(raw, frame) {
                got = Some(s);
                break;
            }
        }
        let s = got.expect("read back");
        assert_eq!((s.w, s.h, s.fmt), (w, h, 10));
        assert_eq!(s.data, src);
        assert!(!scene_pending());
        // a second grab without arming does nothing
        grab_scene(raw);
        assert!(!scene_pending());
        // a texture that is too small (a UI target) is refused and the request stays armed
        let small = D3D11_TEXTURE2D_DESC { Width: 64, Height: 64, ..desc };
        let mut t2 = None;
        dev.CreateTexture2D(&small, None, Some(&mut t2)).unwrap();
        let mut v2 = None;
        dev.CreateShaderResourceView(&t2.unwrap(), None, Some(&mut v2)).unwrap();
        ctx.PSSetShaderResources(0, Some(&[v2]));
        arm_scene(200);
        grab_scene(raw);
        assert!(scene_wanted() && !scene_pending(), "wrong-shaped texture: nothing copied, still waiting");
        disarm_scene("done");
        ctx.PSSetShaderResources(0, Some(&[None]));
    }
}
