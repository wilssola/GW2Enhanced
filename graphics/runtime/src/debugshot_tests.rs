use super::*;

// ---- PNG ------------------------------------------------------------------------------------------------------

/// A tiny decoder for what `encode_png_rgb8` writes: checks the signature, every chunk CRC, the zlib header, the
/// stored blocks and the adler32, and un-filters (filter 0 only).
fn decode_png(png: &[u8]) -> (u32, u32, Vec<u8>) {
    assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
    let mut pos = 8;
    let (mut w, mut h) = (0u32, 0u32);
    let mut idat = Vec::new();
    let mut seen_end = false;
    let mut order = Vec::new();
    while pos < png.len() {
        let len = u32::from_be_bytes(png[pos..pos + 4].try_into().unwrap()) as usize;
        let ty: [u8; 4] = png[pos + 4..pos + 8].try_into().unwrap();
        let data = &png[pos + 8..pos + 8 + len];
        let crc = u32::from_be_bytes(png[pos + 8 + len..pos + 12 + len].try_into().unwrap());
        assert_eq!(crc, crc32(&[&ty, data]), "crc of {:?}", std::str::from_utf8(&ty));
        order.push(ty);
        match &ty {
            b"IHDR" => {
                w = u32::from_be_bytes(data[0..4].try_into().unwrap());
                h = u32::from_be_bytes(data[4..8].try_into().unwrap());
                assert_eq!(&data[8..], &[8, 2, 0, 0, 0]);
            }
            b"IDAT" => idat.extend_from_slice(data),
            b"IEND" => seen_end = true,
            b"tEXt" => {}
            _ => panic!("unexpected chunk"),
        }
        pos += 12 + len;
    }
    assert!(seen_end);
    assert_eq!(order.first(), Some(b"IHDR"));
    assert_eq!(&idat[..2], &[0x78, 0x01]);
    assert_eq!((idat[0] as u32 * 256 + idat[1] as u32) % 31, 0, "zlib header check");
    let mut raw = Vec::new();
    let mut p = 2;
    loop {
        let fin = idat[p] & 1;
        assert_eq!(idat[p] >> 1, 0, "stored block");
        let len = u16::from_le_bytes([idat[p + 1], idat[p + 2]]);
        let nlen = u16::from_le_bytes([idat[p + 3], idat[p + 4]]);
        assert_eq!(len, !nlen);
        raw.extend_from_slice(&idat[p + 5..p + 5 + len as usize]);
        p += 5 + len as usize;
        if fin == 1 {
            break;
        }
    }
    assert_eq!(u32::from_be_bytes(idat[p..p + 4].try_into().unwrap()), adler32(&raw));
    assert_eq!(p + 4, idat.len());
    let stride = w as usize * 3;
    assert_eq!(raw.len(), h as usize * (stride + 1));
    let mut px = Vec::new();
    for row in raw.chunks(stride + 1) {
        assert_eq!(row[0], 0);
        px.extend_from_slice(&row[1..]);
    }
    (w, h, px)
}

#[test]
fn crc_and_adler_match_known_values() {
    assert_eq!(crc32(&[b"123456789"]), 0xcbf4_3926);
    assert_eq!(crc32(&[b"1234", b"56789"]), 0xcbf4_3926);
    assert_eq!(adler32(b"Wikipedia"), 0x11e6_0398);
    assert_eq!(adler32(b""), 1);
    // Long input: the deferred modulo must agree with the plain definition.
    let data: Vec<u8> = (0..200_000u32).map(|i| (i * 31 + 7) as u8).collect();
    let (mut a, mut b) = (1u32, 0u32);
    for &x in &data {
        a = (a + x as u32) % 65521;
        b = (b + a) % 65521;
    }
    assert_eq!(adler32(&data), (b << 16) | a);
}

#[test]
fn png_round_trips() {
    let (w, h) = (7u32, 5u32);
    let rgb: Vec<u8> = (0..w * h * 3).map(|i| (i * 13 + 5) as u8).collect();
    let png = encode_png_rgb8(w, h, &rgb);
    assert_eq!(decode_png(&png), (w, h, rgb));
}

#[test]
fn png_with_several_stored_blocks_round_trips() {
    // 200 * 150 * 3 + 150 > 65535: more than one stored block.
    let (w, h) = (200u32, 150u32);
    let rgb: Vec<u8> = (0..w * h * 3).map(|i| (i.wrapping_mul(i) >> 3) as u8).collect();
    let png = encode_png_rgb8(w, h, &rgb);
    assert!(png.len() > 90_000);
    assert_eq!(decode_png(&png), (w, h, rgb));
}

#[test]
fn a_half_resolution_ultrawide_shot_stays_under_4_mb() {
    let (w, h) = (1720u32, 720u32);
    let png = encode_png_rgb8(w, h, &vec![0x55u8; (w * h * 3) as usize]);
    assert!(png.len() < 4_000_000, "{}", png.len());
}

// ---- conversion and statistics ------------------------------------------------------------------------------------

fn f16(v: f32) -> u16 {
    if v == 0.0 {
        return 0;
    }
    let b = v.to_bits();
    let sign = ((b >> 16) & 0x8000) as u16;
    let exp = ((b >> 23) & 0xff) as i32 - 127 + 15;
    sign | ((exp as u16) << 10) | ((b >> 13) & 0x3ff) as u16
}

fn f16_image(px: &[[f32; 3]]) -> Vec<u8> {
    let mut v = Vec::new();
    for p in px {
        for c in [p[0], p[1], p[2], 1.0] {
            v.extend_from_slice(&f16(c).to_le_bytes());
        }
    }
    v
}

#[test]
fn half_floats_decode() {
    for v in [0.0f32, 0.5, 1.0, 2.0, 4.0, 8.0, 0.25, 1024.0, -2.0] {
        assert_eq!(f16_to_f32(f16(v)), v);
    }
    assert!(f16_to_f32(0x7c00).is_infinite());
    assert!(f16_to_f32(0x7e00).is_nan());
    assert!(f16_to_f32(1) > 0.0 && f16_to_f32(1) < 1e-6); // subnormal
}

#[test]
fn srgb_encode_decode_are_inverse_and_clamp() {
    for v in [0.0f32, 0.001, 0.2, 0.5, 0.9, 1.0] {
        assert!((srgb_decode(srgb_encode(v)) - v).abs() < 1e-5, "{v}");
    }
    assert!((srgb_encode(5.0) - 1.0).abs() < 1e-6);
    assert_eq!(srgb_encode(-1.0), 0.0);
    assert!((srgb_encode(0.5) - 0.7354).abs() < 1e-3);
}

#[test]
fn float_data_is_scaled_by_paper_white_and_encoded() {
    let img = f16_image(&[[0.5, 0.5, 0.5]; 4]);
    let c = convert(&img, 2, 2, PixFmt::F16, 1.0).unwrap();
    assert_eq!((c.w, c.h), (1, 1));
    assert_eq!(c.rgb, vec![188, 188, 188]);
    // Paper white 160 nits: scRGB 2.0 is 160 nits = 1.0 = white.
    let img = f16_image(&[[2.0, 2.0, 2.0]; 4]);
    let c = convert(&img, 2, 2, PixFmt::F16, 80.0 / 160.0).unwrap();
    assert_eq!(c.rgb, vec![255, 255, 255]);
    // Above paper white clamps; below keeps the tone.
    let img = f16_image(&[[8.0, 0.0, 0.5 * 4.0]; 4]);
    let c = convert(&img, 2, 2, PixFmt::F16, 1.0).unwrap();
    assert_eq!(c.rgb, vec![255, 0, 255]);
}

#[test]
fn box_filter_averages_in_linear_light() {
    // One white and three black pixels: linear average 0.25 -> sRGB 137.
    let img = f16_image(&[[1.0; 3], [0.0; 3], [0.0; 3], [0.0; 3]]);
    let c = convert(&img, 2, 2, PixFmt::F16, 1.0).unwrap();
    assert_eq!(c.rgb[0], (srgb_encode(0.25) * 255.0 + 0.5) as u8);
    assert_eq!(c.rgb[0], 137);
}

#[test]
fn row_order_and_odd_sizes() {
    // 3x2: left block red-ish, right column dropped; rows are top to bottom.
    let px = [[1.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]];
    let c = convert(&f16_image(&px), 3, 2, PixFmt::F16, 1.0).unwrap();
    assert_eq!((c.w, c.h), (1, 1));
    assert_eq!(c.rgb, vec![255, 0, 0]);
    assert_eq!(c.stats.pixels, 4);
    assert!(convert(&f16_image(&[[0.0; 3]]), 1, 1, PixFmt::F16, 1.0).is_none());
    assert!(convert(&[0u8; 8], 2, 2, PixFmt::F16, 1.0).is_none(), "short data");
}

#[test]
fn eight_bit_formats_keep_their_channel_order() {
    // BGRA: b=0, g=0, r=255 is red; RGBA: the same bytes mean blue.
    let bgra = [0u8, 0, 255, 255].repeat(4);
    let c = convert(&bgra, 2, 2, PixFmt::Bgra8, 1.0).unwrap();
    assert_eq!(c.rgb, vec![255, 0, 0]);
    let c = convert(&bgra, 2, 2, PixFmt::Rgba8, 1.0).unwrap();
    assert_eq!(c.rgb, vec![0, 0, 255]);
    assert_eq!(c.stats.frac_gt1, 0.0);
    // 8-bit sRGB values survive the round trip.
    let g = [100u8, 100, 100, 255].repeat(4);
    assert_eq!(convert(&g, 2, 2, PixFmt::Bgra8, 1.0).unwrap().rgb, vec![100, 100, 100]);
}

#[test]
fn pixel_formats() {
    assert_eq!(pixfmt(10), Some(PixFmt::F16));
    assert_eq!(pixfmt(87), Some(PixFmt::Bgra8));
    assert_eq!(pixfmt(28), Some(PixFmt::Rgba8));
    assert_eq!(pixfmt(2), None);
    assert_eq!((bytes_per_px(PixFmt::F16), bytes_per_px(PixFmt::Bgra8)), (8, 4));
}

#[test]
fn statistics_of_a_known_image() {
    // 8 pixels: 2 x (8,8,8), 2 x (3,0,0), 2 x (1.5,1.5,1.5), 2 x (0.5,0.5,0.5).
    let px = [[8.0; 3], [8.0; 3], [3.0, 0.0, 0.0], [3.0, 0.0, 0.0], [1.5; 3], [1.5; 3], [0.5; 3], [0.5; 3]];
    let c = convert(&f16_image(&px), 4, 2, PixFmt::F16, 1.0).unwrap();
    let s = c.stats;
    assert_eq!(s.pixels, 8);
    assert!((s.frac_gt1 - 0.75).abs() < 1e-6);
    assert!((s.frac_gt2 - 0.5).abs() < 1e-6);
    assert!((s.frac_gt4 - 0.25).abs() < 1e-6);
    let mean = (2.0 * 8.0 + 2.0 * 0.2126 * 3.0 + 2.0 * 1.5 + 2.0 * 0.5) / 8.0;
    assert!((s.mean_lum - mean).abs() < 1e-3, "{} vs {mean}", s.mean_lum);
    // Saturation above 1.0: grey 0, pure red 1, grey 0 -> (0*2 + 1*2 + 0*2) / 6.
    assert!((s.mean_sat_above1 - 1.0 / 3.0).abs() < 1e-5, "{}", s.mean_sat_above1);
    // 99th percentile is the brightest value (8) within the histogram resolution.
    assert!((s.p99_lum / 8.0 - 1.0).abs() < 0.03, "{}", s.p99_lum);
}

#[test]
fn p99_ignores_a_few_hot_pixels_and_mean_of_black_is_zero() {
    let mut px = vec![[0.25f32; 3]; 200];
    px[0] = [50.0; 3]; // 0.5 % of the pixels
    let c = convert(&f16_image(&px), 20, 10, PixFmt::F16, 1.0).unwrap();
    assert!((c.stats.p99_lum / 0.25 - 1.0).abs() < 0.03, "{}", c.stats.p99_lum);
    assert!(c.stats.frac_gt4 > 0.004 && c.stats.frac_gt4 < 0.006);
    let c = convert(&f16_image(&[[0.0; 3]; 4]), 2, 2, PixFmt::F16, 1.0).unwrap();
    assert_eq!((c.stats.mean_lum, c.stats.p99_lum, c.stats.mean_sat_above1), (0.0, 0.0, 0.0));
}

#[test]
fn nan_and_negative_pixels_do_not_poison_the_result() {
    let mut img = f16_image(&[[0.5; 3]; 4]);
    img[0..2].copy_from_slice(&0x7e00u16.to_le_bytes()); // NaN red
    img[8..10].copy_from_slice(&f16(-2.0).to_le_bytes());
    let c = convert(&img, 2, 2, PixFmt::F16, 1.0).unwrap();
    assert!(c.stats.mean_lum.is_finite() && c.stats.p99_lum.is_finite());
}

#[test]
fn difference_of_two_shots() {
    let d = diff_stats(&[0.0, 0.5, 1.0, 1.0], &[0.0, 0.5, 0.8, 0.95]).unwrap();
    assert!((d.mean_abs_luma - 0.0625).abs() < 1e-6);
    assert!((d.frac_over_10pct - 0.25).abs() < 1e-6);
    assert_eq!(diff_stats(&[0.0], &[0.0, 1.0]), None);
    assert_eq!(diff_stats(&[], &[]), None);
    let same = diff_stats(&[0.3; 10], &[0.3; 10]).unwrap();
    assert_eq!((same.mean_abs_luma, same.frac_over_10pct), (0.0, 0.0));
}

// ---- ring, cadence, retention --------------------------------------------------------------------------------------

#[test]
fn the_ring_hands_out_three_slots_and_reads_after_two_presents() {
    let mut r = SlotRing::new();
    assert_eq!(r.acquire(10), Some(0));
    assert_eq!(r.acquire(11), Some(1));
    assert_eq!(r.acquire(12), Some(2));
    assert_eq!(r.acquire(13), None);
    assert_eq!((r.free_count(), r.pending_count()), (0, 3));
    assert!(r.ready(11).is_empty());
    assert_eq!(r.ready(12), vec![0]);
    assert_eq!(r.ready(14), vec![0, 1, 2]);
    r.release(1);
    assert_eq!(r.acquire(20), Some(1));
    assert_eq!(r.ready(21), vec![0, 2]);
    assert!(r.expired(10 + MAX_AGE).is_empty());
    assert_eq!(r.expired(10 + MAX_AGE + 1), vec![0]);
}

#[test]
fn periodic_capture_cadence() {
    assert!(!capture_due(1_000_000, None, 0), "0 = off");
    assert!(capture_due(5, None, 20), "the first is due at once");
    assert!(!capture_due(19_999, Some(0), 20));
    assert!(capture_due(20_000, Some(0), 20));
    assert!(!capture_due(25_000, Some(10_000), 20));
    assert!(capture_due(30_000, Some(10_000), 20));
    assert!(!capture_due(100, Some(500), 20), "a clock that went back never fires");
}

fn names(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

#[test]
fn retention_keeps_the_newest_shots_and_nothing_else_is_touched() {
    let mut files = Vec::new();
    for id in 100..114u64 {
        files.push(format!("shot-{id}.png"));
        files.push(format!("shot-{id}.txt"));
    }
    files.extend(names(&["latest.png", "latest.txt", "latest-mod.png", "notes.txt", "shot-abc.png", "shot-.png", "shot-105.log", "pair-50.png", "config.ini"]));
    let stale = stale_files(&files, "shot-", 12);
    let mut expect = names(&["shot-100.png", "shot-100.txt", "shot-101.png", "shot-101.txt"]);
    expect.sort();
    let mut got = stale.clone();
    got.sort();
    assert_eq!(got, expect);
    assert!(stale_files(&files, "shot-", 14).is_empty());
}

#[test]
fn retention_of_pairs_groups_the_three_files_of_an_id() {
    let mut files = Vec::new();
    for id in [10u64, 20, 30] {
        files.extend(names(&[&format!("pair-{id}-mod.png"), &format!("pair-{id}-vanilla.png"), &format!("pair-{id}.txt")]));
    }
    files.extend(names(&["latest-vanilla.png", "latest.txt", "shot-5.png"]));
    let mut stale = stale_files(&files, "pair-", 2);
    stale.sort();
    assert_eq!(stale, names(&["pair-10-mod.png", "pair-10-vanilla.png", "pair-10.txt"]));
    assert!(stale_files(&[], "pair-", 8).is_empty());
}

// ---- pair state machine ---------------------------------------------------------------------------------------------

#[test]
fn pair_step_waits_for_frames_time_and_settling() {
    // Nothing before the minimum number of presents, however long.
    assert_eq!(pair_step(0, 0, 100, 3, true), PairStep::Wait);
    assert_eq!(pair_step(0, 0, 1_000, 5, true), PairStep::Wait);
    // Enough presents but too early.
    assert_eq!(pair_step(0, 0, 100, 10, true), PairStep::Wait);
    // Enough of both: settled = shot, not settled = wait.
    assert_eq!(pair_step(0, 0, 300, 6, true), PairStep::Capture);
    assert_eq!(pair_step(0, 0, 300, 6, false), PairStep::Wait);
    assert_eq!(pair_step(0, 0, 1_499, 60, false), PairStep::Wait);
    // Not settled for too long: take the shot anyway.
    assert_eq!(pair_step(0, 0, PAIR_FORCE_MS, 60, false), PairStep::Capture);
    // A stalled render loop (no presents): give up at the watchdog time.
    assert_eq!(pair_step(0, 0, PAIR_FORCE_MS + 100, 2, false), PairStep::Wait);
    assert_eq!(pair_step(0, 0, PAIR_WATCHDOG_MS, 2, false), PairStep::Abort);
    assert_eq!(pair_step(0, 0, PAIR_WATCHDOG_MS, 100, true), PairStep::Abort);
    // Relative to the start.
    assert_eq!(pair_step(5_000, 100, 5_300, 106, true), PairStep::Capture);
    assert_eq!(pair_step(5_000, 100, 4_000, 90, true), PairStep::Wait, "clock going back");
}

#[test]
fn only_the_ending_steps_ask_for_a_restore() {
    assert!(PairStep::Capture.restore());
    assert!(PairStep::Abort.restore());
    assert!(!PairStep::Wait.restore());
    assert!(!PairStep::Idle.restore());
}

#[test]
fn the_machine_runs_one_pair_and_always_ends_with_a_restore() {
    let mut m = PairMachine::new();
    assert_eq!(m.step(0, 0, true), PairStep::Idle);
    assert!(!m.cancel(), "nothing to restore when nothing is forced");
    assert!(m.begin(0, 0));
    assert!(!m.begin(10, 1), "no second pair while one runs");
    assert!(m.is_forced());
    assert_eq!(m.step(100, 2, true), PairStep::Wait);
    assert_eq!(m.step(300, 6, true), PairStep::Capture);
    assert!(!m.is_forced());
    assert_eq!(m.step(400, 7, true), PairStep::Idle, "ended pairs end once");
    // Error exit path.
    assert!(m.begin(1_000, 10));
    assert!(m.cancel(), "an error exit restores");
    assert!(!m.cancel());
    // Timeout path.
    assert!(m.begin(2_000, 20));
    assert_eq!(m.step(4_000, 21, false), PairStep::Abort);
    assert!(!m.is_forced());
}

/// Drives the machine like the render loop would, with every kind of stall, and checks the invariant "forced until
/// restored, and restored within the watchdog time".
#[test]
fn every_schedule_ends_restored_within_the_watchdog_time() {
    // (ms per present, settles after n ms, presents stop after n presents)
    for &(dt, settle_ms, stop_after) in &[(16u64, 0u64, u64::MAX), (16, 600, u64::MAX), (16, 99_999, u64::MAX), (200, 0, u64::MAX), (16, 0, 3), (500, 0, u64::MAX), (1_000, 0, u64::MAX)] {
        let mut m = PairMachine::new();
        let mut forced = false;
        let saved = Saved { enabled: true, paper: 203.0, peak: 1000.0, at_ms: 0 };
        let mut restored = 0;
        assert!(m.begin(0, 0));
        forced = !forced;
        let (mut now, mut frame) = (0u64, 0u64);
        let mut finished_at = None;
        while now < 10_000 {
            now += dt;
            if frame < stop_after {
                frame += 1;
                let st = m.step(now, frame, now >= settle_ms);
                if st.restore() {
                    restored += 1;
                    forced = false;
                    finished_at = Some(now);
                    break;
                }
            } else if saved.watchdog_expired(now) {
                // The render loop stopped: the watchdog thread restores by itself.
                restored += 1;
                forced = false;
                finished_at = Some(now);
                break;
            }
        }
        assert!(!forced && restored == 1, "schedule {dt} {settle_ms} {stop_after}");
        assert!(finished_at.unwrap() <= PAIR_WATCHDOG_MS + dt, "schedule {dt} {settle_ms} {stop_after}: {finished_at:?}");
    }
}

#[test]
fn saved_values_are_restored_exactly_through_the_setters() {
    let s = Saved { enabled: true, paper: 203.5, peak: 1234.0, at_ms: 100 };
    let (mut en, mut lv) = (None, None);
    s.restore(|e| en = Some(e), |p, k| lv = Some((p, k)));
    assert_eq!(en, Some(true));
    assert_eq!(lv, Some((203.5, 1234.0)));
    assert!(!s.watchdog_expired(100 + PAIR_WATCHDOG_MS - 1));
    assert!(s.watchdog_expired(100 + PAIR_WATCHDOG_MS));
    assert!(!s.watchdog_expired(50), "clock before the start");
}

// ---- hot reload -------------------------------------------------------------------------------------------------------

#[test]
fn diff_lists_the_changed_keys_and_ignores_the_restart_ones() {
    let a = Config::parse("bloom_strength=1\nao_strength=1\nhdr_output=auto");
    let b = Config::parse("bloom_strength=1.5\nao_strength=1\nhdr_output=on\noverlay_key=F9\nssr_classes=3\npeak_nits=1500");
    let d = diff(&a, &b);
    assert_eq!(d.live, vec!["peak_nits", "bloom_strength", "ssr_classes"]);
    assert_eq!(d.ignored, vec!["hdr_output", "overlay_key"]);
    assert!(diff(&a, &a).is_empty());
    let d = diff(&Config::default(), &Config::parse("debug_capture_seconds=20\nenabled=0\ndebug_hot_reload=1"));
    assert_eq!(d.live, vec!["enabled", "debug_capture_seconds", "debug_hot_reload"]);
}

#[test]
fn every_live_key_of_the_config_is_covered_by_the_diff() {
    let d0 = Config::default();
    let all = Config {
        enabled: false, hdr_output: crate::config::HdrOutput::On, paper_white_nits: 300.0, peak_nits: 2000.0, sdr_gamma: 2.4, bloom_strength: 0.1, highlight_boost: 1.1, ao_strength: 0.1, ao_radius: 3.0, halo_strength: 0.1, halo_radius: 1.9, ssr_strength: 0.1, ssr_range: 1.9, ssr_classes: 1, ssr_smart: 0.1, ssr_hue_filter: 0.1, ssr_flatness: 0.1, ssr_water_boost: 0.1, water_depth_look: 0.1, ssr_matte_cut: 0.1, shafts_strength: 0.1, shafts_length: 0.4, contact_strength: 0.2, contact_length: 1.6, contact_sun: "fixed".into(), aerial_strength: 0.7, aerial_distance: 800.0, depth_unit: 1.0, aniso_level: 4, aniso_filters: 3, mip_bias: -0.5, smaa: false, smaa_threshold: 0.3, sharpen_strength: 0.8, autoexp_strength: 0.9, autoexp_target: 0.4, autoexp_range: 1.5, adapt_up: 0.3, adapt_down: 4.0, localtone_strength: 0.7, lens_strength: 0.6, lens_threshold: 4.0, lens_ghost: 0.3, lens_streak_length: 1.5, lens_max_size: 0.1, lens_color_reject: 0.9, grade_preset: "Vivid".into(), grade_saturation: 1.3, grade_contrast: 1.1, grade_temperature: 0.2, grade_tint: 0.1, grade_vibrance: 0.4, grade_shadows: 0.2, grade_highlights: -0.2, grade_lut: "luts/a.cube".into(), grade_lut_strength: 0.5, fur_strength: 0.9, fur_ps: "535b203b".into(), fur_vs: "130e17de".into(), fur_min_vcb: 9000, fur_debug: true, fur_uv: "texcoord1.zw".into(), fur_occlusion: 0, fur_amp: 0.5, fur_scale: 2.0, fur_length: 8.0, fur_detail: 1.5, fur_rim: 1.0, fur_sss: 0.5, fur_view: 3, rim_strength: 0.9, rim_width: 7.0, subsurface_strength: 0.8, rim_ps: "feedface".into(), rim_min_vcb: 777, rim_debug: true, overlay_key: "F1".into(), debug_capture_seconds: 7, debug_compare: false, debug_hot_reload: true, debug_view: 4,
        hdr_format: crate::config::HdrFormat::Hdr10, photo_key: "F5".into(), photo_hide_ui: true, calibration_pattern: 2,
        shell: crate::shellfur::ShellCfg { on: 0, shells: 8, length: 2.0, density: 100.0, thickness: 0.2, root_dark: 0.1, wind: 0.9, lod: 9.0, rim: 0.2, ps: "aa".into(), vs: "bb".into(), min_vcb: 1, counts: "3".into(), normal: "texcoord2".into(), unit: 2.0, debug: 1, detail: 0.2, clump: 0.9, fur_mask: "off".into(), fur_threshold: 0.3, fur_soft: 0.2, uv_exclude: "0 0 0.5 0.5".into() },
        toggles: { let mut t = crate::toggles::Toggles::default(); t.0 = [false; 15]; t },
        hdr_upgrade: true,
        hdr_clamp: true,
    };
    let d = diff(&d0, &all);
    // 80 keys + the 18 shell-fur keys (shellfur::KEYS).
    assert_eq!(d.live.len(), 80 + crate::shellfur::KEYS.len() + crate::toggles::ALL.len(), "{:?}", d.live); // v17: + the 15 effect switches
    assert!(d.live.contains(&"photo_hide_ui") && d.live.contains(&"calibration_pattern"));
    for k in ["fur_amp", "fur_scale", "fur_length", "fur_detail", "fur_rim", "fur_sss", "fur_view", "debug_view"] {
        assert!(d.live.contains(&k), "{k} must hot reload");
    }
    assert_eq!(d.ignored, vec!["hdr_output", "hdr_format", "overlay_key", "photo_key"]);
    // Merging all of them onto a config that differs everywhere reproduces the new live values and keeps ignored ones.
    let (m, _) = merge(&d0, &d0, &all);
    assert_eq!(Config { hdr_output: d0.hdr_output, hdr_format: d0.hdr_format, overlay_key: d0.overlay_key.clone(), photo_key: d0.photo_key.clone(), ..all.clone() }, m);
}

#[test]
fn photo_and_hdr_keys_hot_reload_as_documented() {
    let a = Config::default();
    let b = Config::parse("photo_hide_ui=1\ncalibration_pattern=3\nphoto_key=F9\nhdr_format=hdr10");
    let d = diff(&a, &b);
    assert_eq!(d.live, vec!["photo_hide_ui", "calibration_pattern"]);
    assert_eq!(d.ignored, vec!["hdr_format", "photo_key"], "the swapchain format and the hotkey are read at start");
    let (m, _) = merge(&a, &a, &b);
    assert!(m.photo_hide_ui && m.calibration_pattern == 3);
    assert_eq!((m.hdr_format, m.photo_key.as_str()), (a.hdr_format, "Ctrl+Shift+End"));
}

#[test]
fn photo_files_are_never_touched_by_the_debug_retention() {
    let n = names(&["gw2-20261105-143012.png", "gw2-20261105-143012.hdr", "gw2-20261105-143012.txt", "gw2-20261105-143012-mod.png", "shot-100.png", "shot-200.png", "pair-5-mod.png"]);
    for prefix in ["shot-", "pair-"] {
        for keep in 0..3 {
            let stale = stale_files(&n, prefix, keep);
            assert!(stale.iter().all(|s| s.starts_with(prefix)), "{prefix} {keep}: {stale:?}");
        }
    }
}

#[test]
fn a_photo_pair_has_the_same_restore_guarantees_as_a_debug_pair() {
    // The photo pair IS the debug pair machine: forced -> exactly one ending step -> restored, within the watchdog time, for every schedule.
    for &(dt, settle_ms) in &[(16u64, 0u64), (33, 700), (200, 99_999), (1_000, 0)] {
        let mut m = PairMachine::new();
        let saved = Saved { enabled: true, paper: 203.0, peak: 1000.0, at_ms: 0 };
        assert!(m.begin(0, 0));
        // a photo request while the pair runs must wait (arbitrate is told the pair runs)
        assert_eq!(crate::photo::arbitrate(Some(crate::photo::PhotoKind::Single), m.is_forced(), false, 3, true), crate::photo::Plan::Wait);
        let (mut now, mut frame, mut ended) = (0u64, 0u64, None);
        while now < 10_000 {
            now += dt;
            frame += 1;
            let st = m.step(now, frame, now >= settle_ms);
            if st.restore() {
                ended = Some(now);
                break;
            }
            if saved.watchdog_expired(now) {
                ended = Some(now);
                break;
            }
        }
        assert!(ended.is_some_and(|t| t <= PAIR_WATCHDOG_MS + dt), "{dt} {settle_ms}: {ended:?}");
        assert!(!m.is_forced() || saved.watchdog_expired(now), "the machine is free again");
        // after the pair the photo may start
        let freed = PairMachine::new();
        assert_eq!(crate::photo::arbitrate(Some(crate::photo::PhotoKind::Pair), freed.is_forced(), false, 3, true), crate::photo::Plan::Pair);
    }
    // an error exit of a photo pair (no slot, no swapchain...) also restores
    let mut m = PairMachine::new();
    assert!(m.begin(5, 1));
    assert!(m.cancel());
    assert!(!m.cancel());
}

#[test]
fn merge_keeps_slider_values_the_file_did_not_change() {
    let old = Config::parse("bloom_strength=1\nao_strength=1");
    let new = Config::parse("bloom_strength=1.4\nao_strength=1\nhdr_output=on");
    let cur = Config { ao_strength: 0.3, bloom_strength: 1.0, ..Config::default() }; // the slider moved ao in the game
    let (m, d) = merge(&cur, &old, &new);
    assert_eq!(m.bloom_strength, 1.4);
    assert_eq!(m.ao_strength, 0.3, "not changed in the file: the slider value stays");
    assert_eq!(m.hdr_output, cur.hdr_output, "restart keys are never applied");
    assert_eq!(d.ignored, vec!["hdr_output"]);
}

#[test]
fn fur_selector_keys_hot_reload_through_diff_and_merge() {    let _g = crate::testlock::lock();

    let old = Config::parse("fur_strength=0.5");
    let new = Config::parse("fur_strength=0.5
fur_ps=535b203b,1c8a2482
fur_vs=130e17de
fur_min_vcb=16000
fur_debug=1");
    let d = diff(&old, &new);
    assert_eq!(d.live, vec!["fur_ps", "fur_vs", "fur_min_vcb", "fur_debug"]);
    // The in-game state has a different fur_ps (set before): merged from the file because the file changed it.
    let cur = Config { fur_ps: "deadbeef".into(), fur_strength: 0.9, ..Config::default() };
    let (m, _) = merge(&cur, &old, &new);
    assert_eq!((m.fur_ps.as_str(), m.fur_vs.as_str(), m.fur_min_vcb, m.fur_debug), ("535b203b,1c8a2482", "130e17de", 16000, true));
    assert_eq!(m.fur_strength, 0.9, "unchanged keys keep the current value");
    // Applying them reaches the selector.
    apply_live(&["fur_ps", "fur_vs", "fur_min_vcb", "fur_debug"], &m);
    assert_eq!(crate::furmask::selector_text(), ("535b203b,1c8a2482".to_string(), "130e17de".to_string()));
    assert_eq!(crate::furmask::min_vcb(), 16000);
    assert!(crate::furmask::debug());
    crate::furmask::configure("", "", 4096, false);
}

// ---- text and misc ------------------------------------------------------------------------------------------------------

#[test]
fn settings_text_lists_the_settings_and_the_status_lines() {
    let t = settings_text("test", 42, (3440, 1440), 10);
    for k in ["capture=test", "frame=42", "screen=3440x1440", "R16G16B16A16_FLOAT", "paper_white_nits=", "peak_nits=", "effective_peak_nits=", "ao_strength=", "ssr_classes=", "shafts_length=", "fur_strength=", "fur_ps=", "fur_min_vcb=", "fur_debug=", "status_fur=", "status_bloom=", "status_ao=", "status_halo=", "status_reflections=", "status_shafts=", "status_contact=", "status_aerial=", "contact_strength=", "aerial_strength=", "depth_unit=", "status_aa=", "status_aniso=", "aniso_level=", "mip_bias=", "smaa=", "smaa_threshold=", "sharpen_strength=", "autoexp_strength=", "localtone_strength=", "adapt_up=", "status_exposure=", "status_localtone=", "status_lens=", "status_grade=", "lens_strength=", "grade_saturation=", "grade_lut_strength=", "enabled=", "debug_view=", "debug_level=", "fur_amp=", "fur_scale=", "fur_length=", "fur_detail=", "fur_rim=", "fur_sss=", "fur_view=", "status_furlook=", "status_perf="] {
        assert!(t.contains(k), "missing {k} in\n{t}");
    }
}

#[test]
fn single_and_pair_texts_carry_both_sets_of_numbers() {
    let a = Stats { pixels: 10, frac_gt1: 0.1, frac_gt2: 0.05, frac_gt4: 0.01, p99_lum: 3.0, mean_lum: 0.4, mean_sat_above1: 0.2 };
    let b = Stats { mean_lum: 0.3, ..a };
    let t = single_txt(7, "enabled=true\n", &a);
    assert!(t.contains("shot-7.png") && t.contains("p99_luminance=3.000") && t.contains("enabled=true"));
    let d = DiffStats { mean_abs_luma: 0.05, frac_over_10pct: 0.2 };
    let t = pair_txt(9, "MODSET\n", &a, Some(("VANSET\n", &b, Some(d))), "");
    for k in ["pair-9-mod.png", "pair-9-vanilla.png", "not a pixel-identical original", "0.4000 | 0.3000", "mean_abs_luminance_difference=0.0500", "fraction_pixels_differing_more_than_10pct=0.2000", "== MOD settings ==\nMODSET", "== VANILLA settings ==\nVANSET"] {
        assert!(t.contains(k), "missing {k} in\n{t}");
    }
    let t = pair_txt(9, "MODSET\n", &a, None, "pair incomplete: timeout");
    assert!(t.contains("pair incomplete: timeout") && !t.contains("VANILLA settings"));
}

#[test]
fn file_ids_never_repeat() {
    let a = next_id();
    let b = next_id();
    let c = next_id();
    assert!(a < b && b < c);
}

#[test]
fn write_atomic_replaces_and_leaves_no_temp() {
    let dir = std::env::temp_dir().join(format!("gw2g_dev_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("latest.txt");
    write_atomic(&p, b"one").unwrap();
    write_atomic(&p, b"two").unwrap();
    assert_eq!(std::fs::read(&p).unwrap(), b"two");
    assert!(!dir.join("latest.txt.tmp").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_writer_makes_a_pair_with_stable_paths_and_prunes() {
    let dir = std::env::temp_dir().join(format!("gw2g_debug_pair_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut pending = None;
    let img = |v: f32| f16_image(&[[v; 3]; 16]);
    let job = |kind, id, v: f32| Msg::Image(Job { meta: Meta { kind, id, paper: 80.0, text: format!("enabled={}\n", kind == Kind::PairMod), photo: None, depth: None }, w: 4, h: 4, fmt: PixFmt::F16, scale: 1.0, data: img(v) });
    for id in 1000..1010u64 {
        process(job(Kind::PairMod, id, 2.0), &dir, &mut pending).unwrap();
        process(job(Kind::PairVanilla, id, 0.5), &dir, &mut pending).unwrap();
    }
    for f in ["latest-mod.png", "latest-vanilla.png", "latest.txt", "latest.png", "pair-1009-mod.png", "pair-1009-vanilla.png", "pair-1009.txt"] {
        assert!(dir.join(f).exists(), "missing {f}");
    }
    assert!(!dir.join("pair-1001.txt").exists() && dir.join("pair-1002.txt").exists(), "newest 8 pairs kept");
    let t = std::fs::read_to_string(dir.join("latest.txt")).unwrap();
    assert!(t.contains("mean_abs_luminance_difference") && t.contains("enabled=true") && t.contains("enabled=false"), "{t}");
    // A vanilla shot without its mod shot is an error, an abort writes the mod-only text.
    assert!(process(job(Kind::PairVanilla, 2000, 0.5), &dir, &mut pending).is_err());
    process(job(Kind::PairMod, 2001, 2.0), &dir, &mut pending).unwrap();
    process(Msg::Abort(2001, "timeout".into()), &dir, &mut pending).unwrap();
    assert!(std::fs::read_to_string(dir.join("pair-2001.txt")).unwrap().contains("pair incomplete: timeout"));
    // Single shots: 14 of them, 12 kept.
    for id in 3000..3014u64 {
        process(job(Kind::Single, id, 1.0), &dir, &mut pending).unwrap();
    }
    assert!(!dir.join("shot-3001.png").exists() && dir.join("shot-3002.png").exists() && dir.join("shot-3013.txt").exists());
    let (w, h, px) = decode_png(&std::fs::read(dir.join("latest.png")).unwrap());
    assert_eq!((w, h, px.len()), (2, 2, 12));
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- GPU copy + read-back on WARP ------------------------------------------------------------------------------------------

use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::Interface;

#[test]
fn a_texture_is_copied_and_read_back_on_warp() {
    let mut dev = None;
    let mut ctx = None;
    let r = unsafe { D3D11CreateDevice(None::<&windows::Win32::Graphics::Dxgi::IDXGIAdapter>, D3D_DRIVER_TYPE_WARP, HMODULE::default(), D3D11_CREATE_DEVICE_FLAG(0), None, D3D11_SDK_VERSION, Some(&mut dev), None, Some(&mut ctx)) };
    let (Ok(()), Some(dev), Some(ctx)) = (r, dev, ctx) else {
        println!("skipped: WARP device creation failed");
        return;
    };
    unsafe {
        for (fmt, pf) in [(DXGI_FORMAT_R16G16B16A16_FLOAT, PixFmt::F16), (DXGI_FORMAT_B8G8R8A8_UNORM, PixFmt::Bgra8), (DXGI_FORMAT_R10G10B10A2_UNORM, PixFmt::Rgb10A2)] {
            let (w, h) = (6u32, 4u32);
            let bpp = bytes_per_px(pf);
            let src: Vec<u8> = if pf == PixFmt::F16 {
                f16_image(&(0..w * h).map(|i| [i as f32 * 0.25, 0.5, 2.0]).collect::<Vec<_>>())
            } else {
                (0..w * h).flat_map(|i| [i as u8 * 5, 100, 200, 255]).collect()
            };
            let desc = D3D11_TEXTURE2D_DESC { Width: w, Height: h, MipLevels: 1, ArraySize: 1, Format: fmt, SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, Usage: D3D11_USAGE_DEFAULT, BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32, ..Default::default() };
            let sd = D3D11_SUBRESOURCE_DATA { pSysMem: src.as_ptr() as *const c_void, SysMemPitch: w * bpp as u32, SysMemSlicePitch: 0 };
            let mut tex = None;
            dev.CreateTexture2D(&desc, Some(&sd), Some(&mut tex)).unwrap();
            let tex = tex.unwrap();
            let staging = create_staging(dev.as_raw(), w, h, fmt.0 as u32);
            assert!(!staging.is_null());
            copy_resource(ctx.as_raw(), staging, tex.as_raw());
            // DO_NOT_WAIT may or may not be finished on WARP; a blocking read must give the bytes.
            let _ = read_texture(ctx.as_raw(), staging, 0x10_0000, w, h, bpp);
            let got = read_texture(ctx.as_raw(), staging, 0, w, h, bpp).expect("read back");
            assert_eq!(got, src, "{pf:?}");
            let c = convert(&got, w, h, pf, 1.0).unwrap();
            assert_eq!((c.w, c.h), (3, 2));
            if pf == PixFmt::F16 {
                assert!(c.stats.frac_gt1 > 0.9, "blue is 2.0 everywhere");
            }
            release(staging);
        }
    }
}

#[test]
fn png_can_carry_the_settings_in_a_text_chunk_and_stays_valid() {
    let (w, h) = (3u32, 2u32);
    let rgb = vec![0x40u8; (w * h * 3) as usize];
    let png = encode_png_rgb8_text(w, h, &rgb, Some("bloom=2\npeak=3500 \u{e9}"));
    let plain = encode_png_rgb8(w, h, &rgb);
    assert!(png.len() > plain.len());
    let needle = b"Settings\0bloom=2\npeak=3500 ?";
    assert!(png.windows(needle.len()).any(|x| x == needle), "the text chunk is in the file, non-ASCII replaced");
    // The decoder of this file still accepts it (every chunk CRC, adler, order).
    assert_eq!(decode_png(&png), decode_png(&plain));
}

// ---- the workbench: debug_view, the live fur look, frame time and depth telemetry ------------------------------------------------

#[test]
fn workbench_keys_hot_reload_through_diff_merge_and_apply_live() {
    let _g = crate::testlock::lock();
    let (keep_look, keep_view, keep_fur) = (crate::datapath::fur_look(), crate::resolve::debug_view(), crate::datapath::fur_set());
    // debug_view 3 (not 1 or 2: the AO's constants read those two in tests running in parallel).
    let new = Config::parse("debug_view=3\nfur_amp=0.6\nfur_scale=2\nfur_length=9\nfur_detail=1.5\nfur_rim=1\nfur_view=3");
    let d = diff(&Config::default(), &new);
    assert_eq!(d.live, vec!["fur_amp", "fur_scale", "fur_length", "fur_detail", "fur_rim", "fur_view", "debug_view"]);
    assert!(d.ignored.is_empty());
    // A slider moved in the game (fur_sss) is not reverted by the edit: only the keys the file changed are taken.
    let cur = Config { fur_sss: 0.9, ..Config::default() };
    let (m, _) = merge(&cur, &Config::default(), &new);
    assert_eq!((m.fur_amp, m.fur_scale, m.fur_length, m.fur_detail, m.fur_rim, m.fur_sss, m.fur_view, m.debug_view), (0.6, 2.0, 9.0, 1.5, 1.0, 0.9, 3, 3));
    apply_live(&d.live, &m);
    let l = crate::datapath::fur_look();
    assert_eq!((l.amp, l.scale, l.length, l.detail, l.rim, l.sss, l.view), (0.6, 2.0, 9.0, 1.5, 1.0, 0.9, 3));
    assert_eq!(crate::resolve::debug_view(), 3, "the key is one more source of the debug level");
    // Editing the file back to off switches both off again.
    let off = Config::parse("debug_view=0\nfur_amp=0.6\nfur_scale=2\nfur_length=9\nfur_detail=1.5\nfur_rim=1\nfur_view=0");
    let d2 = diff(&new, &off);
    assert_eq!(d2.live, vec!["fur_view", "debug_view"]);
    let (m2, _) = merge(&m, &new, &off);
    apply_live(&d2.live, &m2);
    assert_eq!((crate::datapath::fur_look().view, crate::resolve::debug_view()), (0, 0));
    assert_eq!(crate::datapath::fur_look().amp, 0.6, "the other look values stay");
    crate::datapath::set_fur_look(&keep_look);
    crate::resolve::set_debug_view(keep_view);
    crate::datapath::set_fur(keep_fur);
}

#[test]
fn fur_occlusion_hot_reloads_like_the_fur_look_and_the_debug_text_reports_it() {
    let _g = crate::testlock::lock();
    let keep = crate::furmask::occlusion();
    crate::furmask::set_occlusion(true);
    let off = Config::parse("fur_occlusion=0");
    let d = diff(&Config::default(), &off);
    assert_eq!(d.live, vec!["fur_occlusion"]);
    let (m, _) = merge(&Config::default(), &Config::default(), &off);
    assert_eq!(m.fur_occlusion, 0);
    apply_live(&d.live, &m);
    assert!(!crate::furmask::occlusion(), "applied live");
    let t = settings_text("test", 7, (1920, 1080), 10);
    assert!(t.contains("fur_occlusion=0
"), "{t}");
    let line = t.lines().find(|l| l.starts_with("status_occlusion=")).expect("status_occlusion=");
    assert!(line.contains("fur_occlusion 0"), "{line}");
    let on = Config::parse("fur_occlusion=1");
    let d2 = diff(&off, &on);
    let (m2, _) = merge(&m, &off, &on);
    apply_live(&d2.live, &m2);
    assert!(crate::furmask::occlusion());
    crate::furmask::set_occlusion(keep);
}

#[test]
fn the_debug_text_echoes_the_workbench_values() {
    let _g = crate::testlock::lock();
    let keep = crate::datapath::fur_look();
    crate::datapath::set_fur_look(&crate::datapath::FurLook { amp: 0.41, scale: 1.5, length: 7.0, detail: 0.5, rim: 0.75, sss: 1.25, view: 0 });
    let t = settings_text("test", 7, (1920, 1080), 10);
    for k in ["fur_amp=0.41\n", "fur_scale=1.5\n", "fur_length=7\n", "fur_detail=0.5\n", "fur_rim=0.75\n", "fur_sss=1.25\n", "fur_view=0\n", "debug_view=", "debug_level=", "status_perf="] {
        assert!(t.contains(k), "{k} missing in\n{t}");
    }
    let line = t.lines().find(|l| l.starts_with("status_furlook=")).expect("status_furlook=");
    for k in ["fur_amp 0.41", "fur_scale 1.5", "fur_length 7", "fur_detail 0.5", "fur_rim 0.75", "fur_sss 1.25", "fur_view 0"] {
        assert!(line.contains(k), "{k} missing in {line}");
    }
    // status_depth= is added to a shot's own text (it needs the read-back), not to the generic settings.
    assert!(!t.contains("status_depth="));
    crate::datapath::set_fur_look(&keep);
}

// ---- frame time telemetry --------------------------------------------------------------------------------------------------------

#[test]
fn perf_stats_of_a_steady_60_fps() {
    let s = perf_stats(&vec![1000.0 / 60.0; PERF_WINDOW]).unwrap();
    assert_eq!(s.presents, 300);
    assert!((s.avg_ms - 16.6667).abs() < 1e-3 && (s.worst1_ms - 16.6667).abs() < 1e-3, "{s:?}");
    assert!((s.fps - 60.0).abs() < 0.01, "{s:?}");
}

#[test]
fn perf_stats_show_a_two_second_stall_in_the_worst_one_percent() {
    let mut v = vec![1000.0f32 / 60.0; 299];
    v.insert(120, 2000.0); // the stall in the middle of the window
    let s = perf_stats(&v).unwrap();
    // average: (299 x 16.6667 + 2000) / 300; worst 1 % of 300 presents = the 3 slowest: (2000 + 2 x 16.6667) / 3.
    let avg = (299.0 * 1000.0 / 60.0 + 2000.0) / 300.0;
    assert!((s.avg_ms - avg).abs() < 1e-2, "{} vs {avg}", s.avg_ms);
    assert!((s.worst1_ms - (2000.0 + 2.0 * 1000.0 / 60.0) / 3.0).abs() < 1e-2, "the stall shows in the worst 1 %: {}", s.worst1_ms);
    assert!((s.fps - 1000.0 / avg).abs() < 1e-2);
    assert!(s.worst1_ms > 600.0 && s.avg_ms < 30.0, "a stall barely moves the average but dominates the worst 1 %: {s:?}");
    // The same stall without the other 299 smooth frames around: one present is the whole picture.
    let one = perf_stats(&[2000.0]).unwrap();
    assert_eq!((one.presents, one.avg_ms, one.worst1_ms, one.fps), (1, 2000.0, 2000.0, 0.5));
}

#[test]
fn perf_worst_one_percent_is_at_least_one_frame_and_rounds_up() {
    let up = |n: usize| -> Vec<f32> { (1..=n).map(|i| i as f32).collect() };
    assert_eq!(perf_stats(&up(99)).unwrap().worst1_ms, 99.0, "under 100 presents: the slowest one");
    assert_eq!(perf_stats(&up(100)).unwrap().worst1_ms, 100.0);
    assert_eq!(perf_stats(&up(101)).unwrap().worst1_ms, 100.5, "101 presents: the slowest two");
    assert_eq!(perf_stats(&up(300)).unwrap().worst1_ms, 299.0, "300 presents: the slowest three (300, 299, 298)");
}

#[test]
fn perf_ignores_garbage_and_has_no_stats_without_data() {
    assert_eq!(perf_stats(&[]), None);
    assert_eq!(perf_stats(&[f32::NAN, f32::INFINITY, -4.0, 0.0]), None);
    let s = perf_stats(&[f32::NAN, 10.0, f32::NEG_INFINITY, 0.0, 20.0, -1.0]).unwrap();
    assert_eq!((s.presents, s.avg_ms, s.worst1_ms), (2, 15.0, 20.0));
}

#[test]
fn the_perf_ring_keeps_the_last_300_intervals() {
    let mut r = PerfRing::new();
    assert!(r.is_empty() && r.stats().is_none());
    for _ in 0..PERF_WINDOW {
        r.push(100.0);
    }
    assert_eq!((r.len(), r.stats().unwrap().avg_ms), (300, 100.0));
    // 300 faster presents push every slow one out.
    for _ in 0..PERF_WINDOW {
        r.push(10.0);
    }
    let s = r.stats().unwrap();
    assert_eq!((r.len(), s.presents, s.avg_ms, s.worst1_ms), (300, 300, 10.0, 10.0));
    // Half way: 150 slow among 150 fast.
    for _ in 0..150 {
        r.push(100.0);
    }
    let s = r.stats().unwrap();
    assert_eq!(s.presents, 300);
    assert!((s.avg_ms - 55.0).abs() < 1e-4, "{}", s.avg_ms);
    // Garbage does not enter the ring.
    r.push(f32::NAN);
    r.push(-1.0);
    r.push(0.0);
    assert_eq!(r.len(), 300);
    assert_eq!(r.stats().unwrap().avg_ms, s.avg_ms);
    // A stall of 2 s among the last 300.
    r.push(2000.0);
    let st = r.stats().unwrap();
    assert!(st.worst1_ms > 600.0, "{st:?}");
    // Fewer presents than the window: the numbers cover only those.
    let mut few = PerfRing::new();
    for _ in 0..12 {
        few.push(16.0);
    }
    assert_eq!((few.len(), few.stats().unwrap().presents), (12, 12));
}

#[test]
fn the_perf_text_says_what_it_measures() {
    let t = perf_text(perf_stats(&vec![1000.0 / 60.0; PERF_WINDOW]));
    for k in ["avg 16.67 ms (60.0 fps)", "worst 1 % 16.67 ms", "over 300 presents", "CPU time between presents"] {
        assert!(t.contains(k), "{k} missing in {t}");
    }
    assert!(!t.contains("still filling"));
    assert!(perf_text(perf_stats(&[16.0; 12])).contains("over 12 presents, still filling"));
    assert_eq!(perf_text(None), "no presents measured yet");
}

#[test]
fn the_present_tick_measures_the_time_between_presents() {
    let _g = crate::testlock::lock();
    *PERF_LAST.lock().unwrap() = None;
    *PERF.lock().unwrap() = PerfRing::new();
    perf_tick(); // the first present has no predecessor
    assert!(PERF.lock().unwrap().is_empty());
    std::thread::sleep(std::time::Duration::from_millis(30));
    perf_tick();
    let s = PERF.lock().unwrap().stats().expect("one interval");
    assert_eq!(s.presents, 1);
    assert!(s.avg_ms >= 25.0 && s.avg_ms < 2000.0, "{s:?}");
    assert!(perf_status().contains("over 1 presents"));
    *PERF_LAST.lock().unwrap() = None;
    *PERF.lock().unwrap() = PerfRing::new();
}

// ---- depth telemetry --------------------------------------------------------------------------------------------------------------

#[test]
fn sig6_prints_six_significant_digits() {
    for (v, want) in [
        (0.0f32, "0"),
        (1.0, "1"),
        (0.5, "0.5"),
        (0.1, "0.1"),
        (25.4, "25.4"),
        (0.0254, "0.0254"),
        (12345.6789, "12345.7"),
        (123456.0, "123456"),
        (1234567.0, "1.23457e6"),
        (999999.9, "1e6"),
        (0.000123456, "0.000123456"),
        (1e-7, "1e-7"),
        (-2.5, "-2.5"),
        (3.4028235e38, "3.40282e38"),
        (1.0 / 3.0, "0.333333"),
        (2.0 / 3.0, "0.666667"),
        (100.0, "100"),
        (1000.0, "1000"),
        (f32::NAN, "nan"),
        (f32::INFINITY, "inf"),
        (f32::NEG_INFINITY, "-inf"),
    ] {
        assert_eq!(sig6(v), want, "{v:e}");
    }
}

/// A 105 x 63 depth whose value names its position: y * 1000 + x + 1.
fn position_depth() -> (Vec<f32>, usize, usize) {
    let (w, h) = (105usize, 63usize);
    ((0..w * h).map(|i| ((i / w) * 1000 + i % w + 1) as f32).collect(), w, h)
}

#[test]
fn depth_status_has_the_documented_shape_and_samples_the_documented_places() {
    let (d, w, h) = position_depth();
    let t = depth_status(&d, w, h);
    // Cell centres of the 5 x 3 grid: x = 10, 31, 52, 73, 94 and y = 10, 31, 52. The centre column: x = 52 at y = 56, 44, 31, 18, 6.
    let want_tail = "grid 5x3 (row-major, top-left first): 10011 10032 10053 10074 10095 / 31011 31032 31053 31074 31095 / 52011 52032 52053 52074 52095; centre column x=0.5 at y=0.9,0.7,0.5,0.3,0.1: 56053 44053 31053 18053 6053";
    assert!(t.ends_with(want_tail), "{t}");
    assert!(t.starts_with("valid min 1, p5 "), "{t}");
    // min, p5, p50, p95, max and the sky share are printed in this order.
    let head = t.split("; grid").next().unwrap();
    let parts: Vec<&str> = head.split("; ").collect();
    assert_eq!(parts.len(), 2, "{head}");
    assert!(parts[0].starts_with("valid min 1, p5 ") && parts[0].contains(", p50 ") && parts[0].contains(", p95 ") && parts[0].ends_with(", max 62105"), "{head}");
    assert!(parts[1].starts_with("sky/invalid ") && parts[1].ends_with(" %"), "{head}");
    // Nothing is invalid; sky = at least 0.98 x 62105 = 60862.9: rows 61 and 62 (row 60 tops out at 60105).
    let sky = d.iter().filter(|&&v| v >= 0.98 * 62105.0).count();
    assert_eq!(parts[1], format!("sky/invalid {:.2} %", 100.0 * sky as f64 / (w * h) as f64));
}

#[test]
fn depth_status_percentiles_are_nearest_rank_over_the_valid_depths() {
    // 100 x 60 depths, every value 1..=100 appears 60 times (a left to right ramp).
    let (w, h) = (100usize, 60usize);
    let d: Vec<f32> = (0..w * h).map(|i| (i % w + 1) as f32).collect();
    let t = depth_status(&d, w, h);
    // sorted index round(p / 100 x 5999): 300 -> value 6, 3000 -> 51, 5699 -> 95.
    assert!(t.starts_with("valid min 1, p5 6, p50 51, p95 95, max 100; sky/invalid 3.00 %;"), "{t}");
}

#[test]
fn depth_status_counts_invalid_and_sky_and_ignores_them_in_the_numbers() {
    let mut d = vec![50.0f32; 100];
    d[..5].copy_from_slice(&[0.0, f32::NAN, -1.0, f32::INFINITY, f32::NEG_INFINITY]); // invalid
    d[5..25].fill(1000.0); // the largest valid depth: sky
    d[25..30].fill(990.0); // >= 0.98 x max: sky
    d[30..35].fill(970.0); // below the cut
    let t = depth_status(&d, 10, 10);
    // 95 valid depths: 65 x 50, 5 x 970, 5 x 990, 20 x 1000. Sky/invalid = 5 + 20 + 5 = 30 of 100.
    assert!(t.starts_with("valid min 50, p5 50, p50 50, p95 1000, max 1000; sky/invalid 30.00 %; grid 5x3"), "{t}");
    // The raw values of the invalid pixels are reported as they are in the grid and the column.
    let (mut d, w, h) = position_depth();
    for (y, v) in [(56usize, f32::NAN), (44, 0.0), (31, -3.0), (18, f32::INFINITY), (6, f32::NEG_INFINITY)] {
        d[y * w + 52] = v;
    }
    let t = depth_status(&d, w, h);
    assert!(t.ends_with("centre column x=0.5 at y=0.9,0.7,0.5,0.3,0.1: nan 0 -3 inf -inf"), "{t}");
    assert!(t.starts_with("valid min 1, "), "{t}");
    assert!(t.contains(", max 62105;"), "the infinities are not the maximum: {t}");
}

#[test]
fn depth_status_survives_empty_all_invalid_and_short_data() {
    let t = depth_status(&[0.0; 50], 10, 5);
    assert!(t.starts_with("valid none; sky/invalid 100 %; grid 5x3"), "{t}");
    assert!(depth_status(&[f32::NAN; 6], 3, 2).starts_with("valid none"));
    assert!(depth_status(&[], 0, 0).starts_with("unavailable"));
    assert!(depth_status(&[1.0; 10], 10, 5).starts_with("unavailable"), "short data");
    assert!(depth_status(&[1.0], 0, 1).starts_with("unavailable"));
    // One pixel, one value: every sample is that value.
    let t = depth_status(&[0.25], 1, 1);
    assert!(t.starts_with("valid min 0.25, p5 0.25, p50 0.25, p95 0.25, max 0.25; sky/invalid 100.00 %; grid 5x3"), "{t}");
    // Normalised depths 0..1 are readable with six digits.
    let d: Vec<f32> = (1..=1000).map(|i| i as f32 / 1000.0).collect();
    let t = depth_status(&d, 100, 10);
    assert!(t.starts_with("valid min 0.001, p5 0.051, p50 0.501, p95 0.95, max 1;"), "{t}");
}

#[test]
fn the_depth_gate_arms_waits_takes_and_gives_up() {
    use DepthGate::*;
    assert_eq!(depth_gate(100, None, None), Arm);
    assert_eq!(depth_gate(101, Some(100), None), Wait);
    assert_eq!(depth_gate(101, Some(100), Some(101)), Take);
    assert_eq!(depth_gate(101 + DEPTH_MAX_AGE, None, Some(101)), Take, "a read-back of the last few presents is still the picture of the shot");
    assert_eq!(depth_gate(101 + DEPTH_MAX_AGE + 1, None, Some(101)), Arm, "a stale one is not used: ask again");
    assert_eq!(depth_gate(100 + DEPTH_WAIT_FRAMES, Some(100), None), Wait);
    assert_eq!(depth_gate(100 + DEPTH_WAIT_FRAMES + 1, Some(100), None), GiveUp, "never answered: the shot goes ahead without a depth");
    assert_eq!(depth_gate(5, Some(100), None), Wait, "a frame counter that went back never panics");
}

#[test]
fn prepare_depth_arms_waits_takes_and_gives_up_and_always_disarms() {
    let _g = crate::testlock::lock();
    let reset = || {
        let mut d = lock_depth();
        disarm(&mut d);
        d.shot = None;
        d.why.clear();
    };
    reset();
    let mut st = State { machine: PairMachine::new(), guard: None, last_ms: None, gpu: None, pair_id: 0, pair_photo: false, scene_wait: None, depth: None, depth_note: None };
    // The first call asks the light-adjust draw of the next frame for the depth and tells the AO to capture it.
    assert!(!prepare_depth_with(&mut st, 100, true), "the shot waits");
    assert!(DEPTH_ARMED.load(Relaxed));
    assert_ne!(crate::ao::depth_consumers() & DEPTH_FOR_DEBUG, 0, "the AO's depth capture runs while the shot waits");
    assert!(!prepare_depth_with(&mut st, 101, true), "still waiting");
    // The light-adjust draw answers (simulated): a read-back of the frame the shot is taken in.
    lock_depth().shot = Some((102, DepthData { w: 2, h: 1, data: vec![1.0, 2.0] }));
    assert!(prepare_depth_with(&mut st, 102, true));
    let d = st.depth.take().expect("the depth is handed to the shot");
    assert_eq!((d.w, d.h, d.data), (2, 1, vec![1.0, 2.0]));
    assert!(st.depth_note.is_none());
    assert!(!DEPTH_ARMED.load(Relaxed), "disarmed after the hand-over");
    assert_eq!(crate::ao::depth_consumers() & DEPTH_FOR_DEBUG, 0);
    assert!(lock_depth().shot.is_none(), "taken once");
    // Nobody answers: it waits, then gives up with the reason the light-adjust draw left.
    assert!(!prepare_depth_with(&mut st, 200, true));
    lock_depth().why = "no linear depth at the light-adjust draw (test)".to_string();
    for f in 201..=200 + DEPTH_WAIT_FRAMES {
        assert!(!prepare_depth_with(&mut st, f, true), "present {f}");
    }
    assert!(prepare_depth_with(&mut st, 200 + DEPTH_WAIT_FRAMES + 1, true), "gives up");
    assert!(st.depth.is_none());
    assert!(st.depth_note.as_deref().is_some_and(|n| n.contains("no linear depth at the light-adjust draw")), "{:?}", st.depth_note);
    assert!(!DEPTH_ARMED.load(Relaxed) && crate::ao::depth_consumers() & DEPTH_FOR_DEBUG == 0, "the AO is released on every path");
    // Without a reason from the draw the note still says why.
    assert!(!prepare_depth_with(&mut st, 300, true));
    assert!(prepare_depth_with(&mut st, 300 + DEPTH_WAIT_FRAMES + 1, true));
    assert!(st.depth_note.as_deref().is_some_and(|n| n.contains("no light-adjust draw")), "{:?}", st.depth_note);
    // With the mod switched off there is nothing to wait for.
    assert!(prepare_depth_with(&mut st, 400, false));
    assert!(st.depth.is_none() && st.depth_note.as_deref().is_some_and(|n| n.contains("switched off")));
    assert!(!DEPTH_ARMED.load(Relaxed));
    // An arm nobody uses is dropped by the housekeeping after a while.
    assert!(!prepare_depth_with(&mut st, 500, true));
    depth_housekeeping(500 + DEPTH_WAIT_FRAMES * 4);
    assert!(DEPTH_ARMED.load(Relaxed), "not yet");
    depth_housekeeping(500 + DEPTH_WAIT_FRAMES * 4 + 1);
    assert!(!DEPTH_ARMED.load(Relaxed) && crate::ao::depth_consumers() & DEPTH_FOR_DEBUG == 0);
    reset();
}

#[test]
fn the_depth_consumer_bit_is_its_own() {
    assert_eq!(DEPTH_FOR_DEBUG.count_ones(), 1);
    assert!(!crate::ao::ALL_DEPTH_CONSUMERS.contains(&DEPTH_FOR_DEBUG), "no other depth user may share the bit");
    assert!(crate::ao::ALL_DEPTH_CONSUMERS.iter().all(|b| b & DEPTH_FOR_DEBUG == 0));
}

#[test]
fn a_debug_shot_of_the_mod_gets_the_depth_and_everything_else_does_not() {
    let depth = || Some(DepthData { w: 1, h: 1, data: vec![2.0] });
    let mut t = String::from("a=1\n");
    assert!(attach_depth(&mut t, true, depth(), None).is_some() && t == "a=1\n", "the numbers are made by the writer");
    let mut t = String::from("a=1\n");
    assert!(attach_depth(&mut t, true, None, Some("the mod is switched off".to_string())).is_none());
    assert_eq!(t, "a=1\nstatus_depth=unavailable (the mod is switched off)\n");
    for (d, n) in [(depth(), None), (None, Some("x".to_string())), (depth(), Some("x".to_string()))] {
        let mut t = String::from("a=1\n");
        assert!(attach_depth(&mut t, false, d, n).is_none() && t == "a=1\n", "vanilla shots and photos carry no depth line");
    }
    let mut t = String::new();
    assert!(attach_depth(&mut t, true, None, None).is_none() && t.is_empty());
}

#[test]
fn the_writer_adds_status_depth_to_the_text_the_png_and_the_latest_file() {
    let dir = std::env::temp_dir().join(format!("gw2g_dev_depth_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut pending = None;
    let (d, w, h) = position_depth();
    let job = |kind, id, depth| Msg::Image(Job { meta: Meta { kind, id, paper: 80.0, text: "enabled=true\n".to_string(), photo: None, depth }, w: 4, h: 4, fmt: PixFmt::F16, scale: 1.0, data: f16_image(&[[0.5; 3]; 16]) });
    process(job(Kind::Single, 777, Some(DepthData { w: w as u32, h: h as u32, data: d.clone() })), &dir, &mut pending).unwrap();
    for f in ["shot-777.txt", "latest.txt"] {
        let t = std::fs::read_to_string(dir.join(f)).unwrap();
        assert_eq!(t.lines().filter(|l| l.starts_with("status_depth=")).count(), 1, "exactly one line in {f}");
        assert!(t.contains(&format!("status_depth={}\n", depth_status(&d, w, h))), "{f}: {t}");
        assert!(t.contains("enabled=true"));
    }
    let png = std::fs::read(dir.join("shot-777.png")).unwrap();
    let needle = b"status_depth=valid min 1, ";
    assert!(png.windows(needle.len()).any(|x| x == needle), "the settings kept in the PNG carry it too");
    // A pair: the mod shot has it, the vanilla one does not (it has no depth).
    process(job(Kind::PairMod, 778, Some(DepthData { w: w as u32, h: h as u32, data: d })), &dir, &mut pending).unwrap();
    process(job(Kind::PairVanilla, 778, None), &dir, &mut pending).unwrap();
    let t = std::fs::read_to_string(dir.join("latest.txt")).unwrap();
    assert_eq!(t.matches("status_depth=").count(), 1, "{t}");
    let mod_part = t.split("== VANILLA settings ==").next().unwrap();
    assert!(mod_part.contains("status_depth=valid min"), "{t}");
    // A shot without a depth has no such line from the writer.
    process(job(Kind::Single, 779, None), &dir, &mut pending).unwrap();
    assert!(!std::fs::read_to_string(dir.join("shot-779.txt")).unwrap().contains("status_depth="));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_engine_depth_is_read_back_from_an_r32_float_texture_on_warp() {
    let mut dev = None;
    let mut ctx = None;
    let r = unsafe { D3D11CreateDevice(None::<&windows::Win32::Graphics::Dxgi::IDXGIAdapter>, D3D_DRIVER_TYPE_WARP, HMODULE::default(), D3D11_CREATE_DEVICE_FLAG(0), None, D3D11_SDK_VERSION, Some(&mut dev), None, Some(&mut ctx)) };
    let (Ok(()), Some(dev), Some(ctx)) = (r, dev, ctx) else {
        println!("skipped: WARP device creation failed");
        return;
    };
    unsafe {
        let (w, h) = (7u32, 5u32);
        let vals: Vec<f32> = (0..w * h).map(|i| i as f32 * 0.5 + 0.25).collect();
        let bytes: Vec<u8> = vals.iter().flat_map(|v| v.to_le_bytes()).collect();
        let make = |fmt: DXGI_FORMAT| {
            let desc = D3D11_TEXTURE2D_DESC { Width: w, Height: h, MipLevels: 1, ArraySize: 1, Format: fmt, SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, Usage: D3D11_USAGE_DEFAULT, BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32, ..Default::default() };
            let sd = D3D11_SUBRESOURCE_DATA { pSysMem: bytes.as_ptr() as *const c_void, SysMemPitch: w * 4, SysMemSlicePitch: 0 };
            let mut tex = None;
            let r = dev.CreateTexture2D(&desc, Some(&sd), Some(&mut tex));
            (r, tex)
        };
        for fmt in [DXGI_FORMAT_R32_FLOAT, DXGI_FORMAT_R32_TYPELESS] {
            let (r, tex) = make(fmt);
            r.unwrap();
            let tex = tex.unwrap();
            let d = read_depth(dev.as_raw(), ctx.as_raw(), tex.as_raw()).expect("read back");
            assert_eq!((d.w, d.h), (w, h));
            assert_eq!(d.data, vals, "{fmt:?}");
            // The pure function on real read-back data.
            let t = depth_status(&d.data, d.w as usize, d.h as usize);
            assert!(t.starts_with("valid min 0.25, p5 "), "{t}");
            assert!(t.contains(", max 17.25; "), "{t}");
        }
        // Not a depth: refused with the reason, nothing copied.
        let (r, tex) = make(DXGI_FORMAT_R32_UINT);
        r.unwrap();
        let e = read_depth(dev.as_raw(), ctx.as_raw(), tex.unwrap().as_raw()).err().expect("an R32_UINT texture is not a depth");
        assert!(e.contains("expected a single-sample R32_FLOAT"), "{e}");
    }
}
