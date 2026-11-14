//! Host tests of the grading maths, the presets, the `.cube` parser and the LUT loader.

use super::*;

fn p_of(g: Grade) -> GradeParams {
    GradeParams::from_grade(&g)
}

const SAMPLES: [[f32; 3]; 8] = [[0.0, 0.0, 0.0], [0.02, 0.03, 0.01], [0.18, 0.18, 0.18], [0.5, 0.3, 0.1], [0.9, 0.95, 0.3], [1.0, 1.0, 1.0], [8.0, 4.0, 2.0], [40.0, 38.0, 20.0]];

fn close(a: [f32; 3], b: [f32; 3], tol: f32) -> bool {
    (0..3).all(|i| (a[i] - b[i]).abs() <= tol * b[i].abs().max(1.0))
}

#[test]
fn neutral_is_detected_exactly_and_the_neutral_grade_is_the_identity() {
    assert!(NEUTRAL.is_neutral() && Grade::default().is_neutral());
    assert!(!Grade { temperature: 0.01, ..NEUTRAL }.is_neutral());
    assert!(!Grade { contrast: 1.0001, ..NEUTRAL }.is_neutral());
    assert_eq!(temp_tint_gains(0.0, 0.0), [1.0; 3], "no float noise at the neutral value");
    let p = p_of(NEUTRAL);
    for c in SAMPLES {
        assert!(close(grade_rgb(c, &p), c, 1e-5), "{c:?} -> {:?}", grade_rgb(c, &p));
    }
}

#[test]
fn ranges_clamp_and_nan_falls_back_to_neutral() {
    let g = Grade { saturation: 9.0, contrast: 0.0, temperature: -9.0, tint: 9.0, vibrance: -1.0, shadows: 9.0, highlights: -9.0 }.clamped();
    assert_eq!(g, Grade { saturation: 2.0, contrast: 0.5, temperature: -1.0, tint: 1.0, vibrance: 0.0, shadows: 0.5, highlights: -0.5 });
    let n = Grade { saturation: f32::NAN, contrast: f32::NAN, temperature: f32::NAN, tint: f32::NAN, vibrance: f32::NAN, shadows: f32::NAN, highlights: f32::NAN }.clamped();
    assert_eq!(n, NEUTRAL);
    assert_eq!(clamp_lut_strength(f32::NAN), 1.0);
    assert_eq!(clamp_lut_strength(5.0), 1.0);
}

#[test]
fn saturation_zero_gives_the_luminance_and_one_changes_nothing() {
    let p = p_of(Grade { saturation: 0.0, ..NEUTRAL });
    for c in SAMPLES {
        let y = luma(c);
        let o = grade_rgb(c, &p);
        assert!(o.iter().all(|v| (v - y).abs() <= 1e-5 * y.max(1.0)), "{c:?} -> {o:?}, luminance {y}");
    }
    let q = p_of(Grade { saturation: 1.0, ..NEUTRAL });
    for c in SAMPLES {
        assert!(close(grade_rgb(c, &q), c, 1e-5));
    }
    // More saturation moves the channels away from the luminance, never below zero.
    let hi = grade_rgb([0.5, 0.3, 0.1], &p_of(Grade { saturation: 2.0, ..NEUTRAL }));
    assert!(hi[0] > 0.5 && hi[2] < 0.1 && hi.iter().all(|v| *v >= 0.0), "{hi:?}");
}

#[test]
fn contrast_keeps_mid_grey_is_monotonic_and_never_blows_up_hdr() {
    for c in [0.5, 0.8, 1.2, 1.5] {
        let p = p_of(Grade { contrast: c, ..NEUTRAL });
        let g = grade_rgb([0.18; 3], &p);
        assert!(g.iter().all(|v| (v - 0.18).abs() < 1e-5), "contrast {c}: mid-grey became {g:?}");
        // Darker than grey gets darker, brighter gets brighter (contrast > 1), the reverse below 1.
        let (lo, hi) = (grade_rgb([0.05; 3], &p)[0], grade_rgb([0.5; 3], &p)[0]);
        if c > 1.0 {
            assert!(lo < 0.05 && hi > 0.5, "contrast {c}: {lo} {hi}");
        } else {
            assert!(lo > 0.05 && hi < 0.5, "contrast {c}: {lo} {hi}");
        }
        // Monotonic over a dense luminance sweep that crosses the knee, and C1-ish at the knee (no jump).
        let mut prev = 0.0f32;
        for i in 0..=2000 {
            let y = i as f32 * 0.02;
            let o = grade_rgb([y; 3], &p)[0];
            assert!(o.is_finite() && o >= prev - 1e-5, "contrast {c}: not monotonic at {y}: {o} < {prev}");
            prev = o;
        }
        let (a, b) = (grade_rgb([0.9999; 3], &p)[0], grade_rgb([1.0001; 3], &p)[0]);
        assert!((a - b).abs() < 0.01, "contrast {c}: jump at the knee {a} {b}");
        // A 40x highlight stays a linear continuation: far below the power law.
        let big = grade_rgb([40.0; 3], &p_of(Grade { contrast: 1.5, ..NEUTRAL }))[0];
        assert!(big < 40.0 * 3.6 && big > 40.0, "{big}"); // slope c * f(1) at the knee, a straight line after it
    }
}

#[test]
fn temperature_is_monotonic_keeps_the_luminance_and_tint_moves_green() {
    let mut prev = 0.0;
    for i in 0..=20 {
        let t = -1.0 + i as f32 * 0.1;
        let o = grade_rgb([0.18; 3], &p_of(Grade { temperature: t, ..NEUTRAL }));
        let ratio = o[0] / o[2];
        assert!(ratio > prev, "temperature {t}: red/blue {ratio} not above {prev}");
        prev = ratio;
        assert!((luma(o) - 0.18).abs() < 1e-5, "temperature {t}: luminance {}", luma(o));
        assert!((o[1] / 0.18 - 1.0).abs() < 0.3);
    }
    let warm = grade_rgb([0.18; 3], &p_of(Grade { temperature: 0.5, ..NEUTRAL }));
    assert!(warm[0] > 0.18 && warm[2] < 0.18);
    let mag = grade_rgb([0.18; 3], &p_of(Grade { tint: 0.8, ..NEUTRAL }));
    let grn = grade_rgb([0.18; 3], &p_of(Grade { tint: -0.8, ..NEUTRAL }));
    assert!(mag[1] < mag[0] && grn[1] > grn[0]);
    assert!((luma(mag) - 0.18).abs() < 1e-5 && (luma(grn) - 0.18).abs() < 1e-5);
}

#[test]
fn vibrance_boosts_dull_colours_only() {
    let p = p_of(Grade { vibrance: 1.0, ..NEUTRAL });
    let grey = grade_rgb([0.3; 3], &p);
    assert!(grey.iter().all(|v| (v - 0.3).abs() < 1e-6), "grey must stay grey: {grey:?}");
    let sat = [0.8, 0.1, 0.05];
    let dull = [0.4, 0.3, 0.25];
    let chroma = |c: [f32; 3]| (c[0].max(c[1]).max(c[2]) - c[0].min(c[1]).min(c[2])) / c[0].max(c[1]).max(c[2]);
    let (gs, gd) = (chroma(grade_rgb(sat, &p)) / chroma(sat), chroma(grade_rgb(dull, &p)) / chroma(dull));
    assert!(gd > gs + 0.3, "dull colour gain {gd} vs saturated {gs}");
    assert!(gs < 1.35);
}

#[test]
fn shadows_and_highlights_are_bounded_and_leave_black_alone() {
    let lift = p_of(Grade { shadows: 0.5, ..NEUTRAL });
    assert_eq!(grade_rgb([0.0; 3], &lift), [0.0; 3]);
    let d = grade_rgb([0.03; 3], &lift)[0];
    assert!(d > 0.03 && d <= 0.03 * 1.31, "{d}");
    assert!(close(grade_rgb([1.0; 3], &lift), [1.0; 3], 1e-5), "shadows do not touch highlights");
    let hi = p_of(Grade { highlights: -0.5, ..NEUTRAL });
    assert!(close(grade_rgb([0.02; 3], &hi), [0.02; 3], 1e-5), "highlights do not touch shadows");
    let h = grade_rgb([5.0; 3], &hi)[0];
    assert!((h - 5.0 * 0.7).abs() < 1e-3, "{h}");
}

#[test]
fn hdr_values_keep_their_hue_order_and_are_not_clipped_for_every_preset_and_extreme() {
    let mut grades: Vec<Grade> = PRESETS.iter().map(|p| p.grade).collect();
    for i in 0..7 {
        for sign in [-1.0, 1.0] {
            let mut g = NEUTRAL;
            match i {
                0 => g.saturation = if sign > 0.0 { 2.0 } else { 0.0 },
                1 => g.contrast = if sign > 0.0 { 1.5 } else { 0.5 },
                2 => g.temperature = sign,
                3 => g.tint = sign,
                4 => g.vibrance = (sign + 1.0) / 2.0,
                5 => g.shadows = sign * 0.5,
                _ => g.highlights = sign * 0.5,
            }
            grades.push(g);
        }
    }
    for g in grades {
        let p = p_of(g);
        for c in [[8.0, 4.0, 2.0], [40.0, 38.0, 20.0], [30.0, 6.0, 5.0], [3.0, 2.5, 2.0]] {
            let o = grade_rgb(c, &p);
            assert!(o.iter().all(|v| v.is_finite() && *v >= 0.0), "{g:?} {c:?} -> {o:?}");
            // Order of the channels is kept (white balance sliders move red and blue by at most 20 %, so only check without them).
            if g.temperature == 0.0 && g.tint == 0.0 {
                assert!(o[0] >= o[1] - 1e-4 && o[1] >= o[2] - 1e-4, "{g:?} {c:?} -> {o:?}");
            }
            // Not clipped: the largest channel is still a highlight (no ceiling at 1.0).
            let (mi, mo) = (c[0].max(c[1]).max(c[2]), o[0].max(o[1]).max(o[2]));
            if g.saturation > 0.0 && g.contrast >= 1.0 {
                assert!(mo > 1.0 && mo > mi * 0.5, "{g:?} {c:?} -> {o:?}");
            }
            assert!(mo > 0.2 * mi.min(40.0), "{g:?} {c:?} -> {o:?}");
        }
    }
}

#[test]
fn presets_match_the_documented_numeric_table() {
    // name, saturation, contrast, temperature, tint, vibrance, shadows, highlights  (docs/grading.md, "Presets")
    let table: [(&str, [f32; 7]); 5] = [
        ("Original", [1.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
        ("Warm cinematic", [0.92, 1.12, 0.30, 0.05, 0.10, -0.10, -0.08]),
        ("Cool", [0.95, 1.05, -0.25, 0.0, 0.0, 0.0, 0.05]),
        ("Vivid", [1.25, 1.10, 0.05, 0.0, 0.35, 0.0, 0.0]),
        ("Soft", [0.90, 0.88, 0.05, 0.0, 0.0, 0.15, -0.15]),
    ];
    assert_eq!(PRESETS.len(), table.len());
    for (p, (name, v)) in PRESETS.iter().zip(table) {
        assert_eq!(p.name, name);
        let g = p.grade;
        assert_eq!([g.saturation, g.contrast, g.temperature, g.tint, g.vibrance, g.shadows, g.highlights], v, "{name}");
        assert_eq!(g, g.clamped(), "{name} is inside the slider ranges");
        assert!(!p.what.is_empty());
    }
    assert!(PRESETS[0].grade.is_neutral());
    assert!(PRESETS[1..].iter().all(|p| !p.grade.is_neutral()));
    // Lookup by name: case, spaces and separators do not matter; unknown names find nothing.
    for n in ["warm cinematic", "WARM_CINEMATIC", "Warm-Cinematic", " Warm cinematic "] {
        assert_eq!(preset_by_name(n).map(|p| p.name), Some("Warm cinematic"), "{n}");
    }
    assert_eq!(preset_by_name("vivid").map(|p| p.name), Some("Vivid"));
    assert!(preset_by_name("").is_none() && preset_by_name("nope").is_none());
    // A preset on a mid-grey scene: the warm one is warmer, the cool one cooler, soft has less contrast than vivid.
    let at = |name: &str, c: [f32; 3]| grade_rgb(c, &p_of(preset_by_name(name).unwrap().grade));
    let (w, k) = (at("Warm cinematic", [0.18; 3]), at("Cool", [0.18; 3]));
    assert!(w[0] / w[2] > 1.05 && k[0] / k[2] < 0.95, "{w:?} {k:?}");
    let spread = |n: &str| at(n, [0.5; 3])[0] / at(n, [0.05; 3])[0];
    assert!(spread("Soft") < spread("Original") && spread("Vivid") > spread("Original"));
}

#[test]
fn config_applies_the_preset_first_and_individual_keys_override() {
    use crate::config::Config;
    let c = Config::parse("grade_preset=Warm cinematic");
    assert_eq!(c.grade(), PRESETS[1].grade);
    assert_eq!(c.grade_preset, "Warm cinematic");
    // The individual key wins wherever it is in the file.
    let c = Config::parse("grade_saturation=1.5\ngrade_preset=vivid\ngrade_temperature=-0.4");
    assert_eq!((c.grade_saturation, c.grade_temperature, c.grade_contrast, c.grade_vibrance), (1.5, -0.4, 1.10, 0.35));
    // Unknown preset: neutral, the name is kept as typed.
    let c = Config::parse("grade_preset=Nope");
    assert!(c.grade().is_neutral());
    // Defaults are neutral and the ini round trip keeps everything.
    assert!(Config::default().grade().is_neutral());
    let t = Config { grade_preset: "Soft".into(), grade_saturation: 0.9, grade_lut: "luts/a.cube".into(), grade_lut_strength: 0.5, lens_strength: 0.2, lens_ghost: 0.1, ..Config::default() }.to_ini();
    for k in ["grade_preset=Soft", "grade_saturation=0.9", "grade_lut=luts/a.cube", "grade_lut_strength=0.5", "lens_strength=0.2", "lens_ghost=0.1", "lens_threshold=2.5", "lens_streak_length=1", "grade_contrast=1\n"] {
        assert!(t.contains(k), "missing {k} in\n{t}");
    }
    let back = Config::parse(&t);
    assert_eq!((back.grade_preset.as_str(), back.grade_saturation, back.grade_lut.as_str(), back.grade_lut_strength, back.lens_strength), ("Soft", 0.9, "luts/a.cube", 0.5, 0.2));
    // Ranges.
    let c = Config::parse("grade_saturation=9\ngrade_contrast=0\ngrade_temperature=-5\ngrade_tint=5\ngrade_vibrance=-1\ngrade_shadows=3\ngrade_highlights=-3\ngrade_lut_strength=2\nlens_strength=9\nlens_threshold=0\nlens_ghost=9\nlens_streak_length=0");
    assert_eq!((c.grade_saturation, c.grade_contrast, c.grade_temperature, c.grade_tint, c.grade_vibrance, c.grade_shadows, c.grade_highlights, c.grade_lut_strength), (2.0, 0.5, -1.0, 1.0, 0.0, 0.5, -0.5, 1.0));
    assert_eq!((c.lens_strength, c.lens_threshold, c.lens_ghost, c.lens_streak_length), (1.0, 1.2, 0.5, 0.25));
    let c = Config::parse("grade_saturation=nan\nlens_strength=abc\nlens_threshold=inf\ngrade_lut=a\u{7}b.cube");
    assert_eq!((c.grade_saturation, c.lens_strength, c.lens_threshold, c.grade_lut.as_str()), (1.0, 0.35, 2.5, "ab.cube"));
    // Defaults agree with the modules.
    let d = Config::default();
    assert_eq!((d.lens_strength, d.lens_threshold, d.lens_ghost, d.lens_streak_length), (crate::lens::DEFAULT_STRENGTH, crate::lens::DEFAULT_THRESHOLD, crate::lens::DEFAULT_GHOST, crate::lens::DEFAULT_LENGTH));
}

// ---- the .cube file -------------------------------------------------------------------------------------------------------------

fn cube_text(n: usize, f: impl Fn(usize, usize, usize) -> [f32; 3]) -> String {
    let mut s = format!("# test\nTITLE \"t\"\nDOMAIN_MIN 0 0 0\nDOMAIN_MAX 1 1 1\nLUT_3D_SIZE {n}\n");
    for b in 0..n {
        for g in 0..n {
            for r in 0..n {
                let v = f(r, g, b);
                s.push_str(&format!("{} {} {}\n", v[0], v[1], v[2]));
            }
        }
    }
    s
}

fn ident(n: usize) -> impl Fn(usize, usize, usize) -> [f32; 3] {
    move |r, g, b| [r as f32 / (n - 1) as f32, g as f32 / (n - 1) as f32, b as f32 / (n - 1) as f32]
}

#[test]
fn a_good_cube_parses_in_red_fastest_order() {
    let c = parse_cube(&cube_text(17, ident(17))).unwrap();
    assert_eq!(c.size, 17);
    assert_eq!(c.data.len(), 17 * 17 * 17);
    assert_eq!(c.data[1], [1.0 / 16.0, 0.0, 0.0], "red varies fastest");
    assert_eq!(c.data[17], [0.0, 1.0 / 16.0, 0.0]);
    assert_eq!(c.data[17 * 17], [0.0, 0.0, 1.0 / 16.0]);
    assert_eq!(c, Cube::identity(17));
    // CRLF, comments, blank lines, unknown keywords, exponent notation and a BOM are fine.
    let t = format!("\u{feff}{}", cube_text(17, ident(17)).replace('\n', "\r\n").replace("# test", "# a\r\n\r\nLUT_3D_INPUT_RANGE 0.0 1.0 # x"));
    assert!(parse_cube(&t).is_ok());
    assert!(parse_cube(&cube_text(33, ident(33))).is_ok() && parse_cube(&cube_text(65, ident(65))).is_ok());
    assert!(parse_cube(&cube_text(17, |r, _, _| [1e-3 * r as f32, 2.5e-1, 1e0])).is_ok());
}

#[test]
fn bad_cubes_say_why_and_never_panic() {
    let err = |t: &str| parse_cube(t).unwrap_err();
    assert!(err("").contains("LUT_3D_SIZE"));
    assert!(err("LUT_1D_SIZE 32\n0 0 0\n").contains("1D"));
    for n in [2, 16, 66, 100] {
        assert!(err(&format!("LUT_3D_SIZE {n}\n")).contains("not supported"), "size {n}");
    }
    assert!(err("LUT_3D_SIZE x\n").contains("bad LUT_3D_SIZE"));
    assert!(err("LUT_3D_SIZE 17\nLUT_3D_SIZE 17\n").contains("twice"));
    assert!(err("0 0 0\nLUT_3D_SIZE 17\n").contains("before LUT_3D_SIZE"));
    // Truncated.
    let full = cube_text(17, ident(17));
    let cut: String = full.lines().take(5 + 100).collect::<Vec<_>>().join("\n");
    assert!(err(&cut).contains("truncated: 100 of 4913"), "{}", err(&cut));
    // Too many rows.
    assert!(err(&format!("{full}0 0 0\n")).contains("too many"));
    // NaN / inf / garbage.
    for bad in ["nan 0 0", "0 inf 0", "0 0 -inf", "0.5 abc 0", "0.5 0.5", "0 0 0 0"] {
        let t = full.replacen("\n0 0 0\n", &format!("\n{bad}\n"), 1);
        assert_ne!(t, full);
        assert!(parse_cube(&t).is_err(), "{bad}");
    }
    assert!(err(&full.replace("DOMAIN_MAX 1 1 1", "DOMAIN_MAX 2 2 2")).contains("DOMAIN_MAX"));
    assert!(err(&full.replace("DOMAIN_MIN 0 0 0", "DOMAIN_MIN -1 0 0")).contains("DOMAIN_MIN"));
    // Binary garbage and a huge line do not panic.
    let junk: String = (0..5000u32).map(|i| char::from_u32(i % 2000 + 1).unwrap_or('x')).collect();
    assert!(parse_cube(&junk).is_err());
}

#[test]
fn the_file_loader_reports_missing_and_oversized_and_bad_files() {
    let dir = std::env::temp_dir().join(format!("gw2g_grading_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    assert_eq!(load_cube_file(&dir.join("missing.cube")).unwrap_err(), "file not found");
    assert_eq!(load_cube_file(&dir).unwrap_err(), "not a file");
    std::fs::write(dir.join("ok.cube"), cube_text(17, ident(17))).unwrap();
    assert_eq!(load_cube_file(&dir.join("ok.cube")).unwrap().size, 17);
    std::fs::write(dir.join("bad.cube"), "LUT_3D_SIZE 17\n0 0 0\n").unwrap();
    assert!(load_cube_file(&dir.join("bad.cube")).unwrap_err().contains("truncated"));
    std::fs::write(dir.join("bin.cube"), [0xffu8, 0xfe, 0, 1, 2, 3]).unwrap();
    assert!(load_cube_file(&dir.join("bin.cube")).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn lut_paths_stay_inside_the_folder() {
    let dir = Path::new("C:\\Games\\GW2\\GW2Graphics");
    assert_eq!(resolve_lut_path(dir, "luts/a.cube").unwrap(), dir.join("luts/a.cube"));
    assert_eq!(resolve_lut_path(dir, " a.CUBE ").unwrap(), dir.join("a.CUBE"));
    for bad in ["", "   ", "../a.cube", "luts/../../a.cube", "C:\\a.cube", "C:a.cube", "/etc/a.cube", "\\\\srv\\share\\a.cube", "a.txt", "a", "luts/", "a.cube.exe"] {
        assert!(resolve_lut_path(dir, bad).is_err(), "{bad:?} must be refused");
    }
}

#[test]
fn the_shaper_round_trips_and_an_identity_lut_is_the_identity_everywhere() {
    for lin in [2f32.powi(-8), 0.005, 0.0187, 0.18, 1.0, 7.5, 63.0, 2f32.powi(6)] {
        let x = shaper_encode(lin);
        assert!((0.0..=1.0).contains(&x));
        assert!((shaper_decode(x) / lin - 1.0).abs() < 1e-5, "{lin}");
    }
    assert_eq!((shaper_encode(0.0), shaper_encode(-3.0), shaper_encode(f32::NAN), shaper_encode(1e9)), (0.0, 0.0, 0.0, 1.0));
    // An identity LUT returns y = x; the channel output equals the input, also in the clamped ranges (black, super-white).
    for lin in [0.0, 1e-6, 1e-4, 0.0039, 0.02, 0.18, 1.0, 20.0, 64.0, 65.0, 200.0, 5000.0] {
        let out = lut_channel_out(lin, shaper_encode(lin));
        assert!((out - lin).abs() <= 1e-4 * lin.max(1e-3), "{lin} -> {out}");
    }
    // A LUT that darkens by 1.4 axis units divides by 2^1.4.
    let out = lut_channel_out(0.5, shaper_encode(0.5) - 0.1);
    assert!((out / 0.5 - 2f32.powf(-1.4)).abs() < 1e-4);
}

#[test]
fn half_floats_convert_correctly() {
    assert_eq!(f32_to_f16(0.0), 0);
    assert_eq!(f32_to_f16(1.0), 0x3c00);
    assert_eq!(f32_to_f16(-2.0), 0xc000);
    assert_eq!(f32_to_f16(0.5), 0x3800);
    assert_eq!(f32_to_f16(65504.0), 0x7bff);
    assert_eq!(f32_to_f16(1e9), 0x7bff, "overflow clamps to the largest finite half");
    assert_eq!(f32_to_f16(-1e9), 0xfbff);
    assert_eq!(f32_to_f16(f32::NAN), 0);
    assert_eq!(f32_to_f16(f32::INFINITY), 0x7bff);
    assert_eq!(f32_to_f16(5.960_464_5e-8), 1, "smallest subnormal");
    assert_eq!(f32_to_f16(1.0e-9), 0);
    assert_eq!(f32_to_f16(1.0 + 1.0 / 2048.0), 0x3c00, "ties to even");
    assert_eq!(f32_to_f16(1.0 + 3.0 / 2048.0), 0x3c02);
    // Round trip of a sweep: within half a unit of the 10-bit mantissa.
    for i in 1..2000 {
        let v = i as f32 * 0.0137;
        let h = f32_to_f16(v);
        let (e, m) = (((h >> 10) & 0x1f) as i32, (h & 0x3ff) as f32);
        let back = (1.0 + m / 1024.0) * 2f32.powi(e - 15);
        assert!((back - v).abs() <= v * 5e-4 + 1e-7, "{v} -> {back}");
    }
    let t = cube_texels(&Cube::identity(17));
    assert_eq!(t.len(), 17 * 17 * 17);
    assert_eq!(t[1][3], 0x3c00);
}

/// The LUT loader and its status, in one test (it owns the global LUT state among the host tests).
#[test]
fn the_background_loader_reports_each_outcome_and_a_newer_request_wins() {
    let dir = std::env::temp_dir().join(format!("gw2g_grading_loader_{}", std::process::id()));
    let _ = std::fs::create_dir_all(dir.join("luts"));
    std::fs::write(dir.join("luts/good.cube"), cube_text(17, ident(17))).unwrap();
    std::fs::write(dir.join("luts/bad.cube"), "LUT_3D_SIZE 17\nnan 0 0\n").unwrap();
    let wait = |pred: &dyn Fn(&LutState) -> bool| {
        for _ in 0..400 {
            if pred(&lut_state()) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("timeout, state {:?}", lut_state());
    };
    set_lut_in(&dir, "");
    assert_eq!(lut_state(), LutState::Off);
    // (Was "no LUT": the overlay now says plainly that the strength slider does nothing without a LUT.)
    assert_eq!(lut_status_text(), NO_LUT_TEXT);
    assert!(lut_status_text().contains("does nothing"));
    assert!(!lut_wanted());
    set_lut_in(&dir, "luts/missing.cube");
    wait(&|s| matches!(s, LutState::Failed(_)));
    assert!(lut_status_text().contains("file not found"), "{}", lut_status_text());
    assert!(lut_for_gpu().1.is_none() && !lut_wanted());
    set_lut_in(&dir, "luts/bad.cube");
    wait(&|s| matches!(s, LutState::Failed(_)));
    assert!(lut_status_text().starts_with("LUT off: "), "{}", lut_status_text());
    set_lut_in(&dir, "..\\x.cube");
    assert!(matches!(lut_state(), LutState::Failed(_)), "refused immediately");
    set_lut_in(&dir, "luts/good.cube");
    wait(&|s| matches!(s, LutState::Ready { .. }));
    assert!(lut_status_text().contains("17x17x17"));
    let (g1, c1) = lut_for_gpu();
    assert!(c1.is_some() && lut_wanted());
    set_lut_strength(0.0);
    assert!(!any_on() || basic_on());
    set_lut_strength(1.0);
    assert!(any_on() || !lut_wanted());
    // Switching off bumps the generation and drops the cube.
    set_lut_in(&dir, "");
    let (g2, c2) = lut_for_gpu();
    assert!(g2 > g1 && c2.is_none());
    // Sliders drive `basic_on`.
    set(NEUTRAL);
    assert!(!basic_on() && !any_on());
    set(Grade { saturation: 1.2, ..NEUTRAL });
    assert!(basic_on() && any_on());
    assert_eq!(get().saturation, 1.2);
    set(Grade { saturation: 99.0, ..NEUTRAL });
    assert_eq!(get().saturation, 2.0);
    set(NEUTRAL);
    assert!(!any_on());
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- the example LUTs ------------------------------------------------------------------------------------------------------

/// What a cube does to one linear colour (nearest lattice point is enough for lattice-aligned inputs).
fn apply_at_lattice(c: &Cube, idx: [usize; 3], lin: [f32; 3]) -> [f32; 3] {
    let n = c.size;
    let y = c.data[idx[2] * n * n + idx[1] * n + idx[0]];
    [lut_channel_out(lin[0], y[0]), lut_channel_out(lin[1], y[1]), lut_channel_out(lin[2], y[2])]
}

#[test]
fn the_example_luts_parse_keep_the_luminance_and_do_what_their_name_says() {
    let ex = example_luts();
    assert_eq!(ex.iter().map(|e| e.0).collect::<Vec<_>>(), ["identity.cube", "warm.cube", "cool.cube", "teal-orange.cube"]);
    let n = EXAMPLE_SIZE;
    let ax = |i: usize| shaper_decode(i as f32 / (n - 1) as f32);
    for (name, text) in &ex {
        let c = parse_cube(text).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(c.size, n);
        assert!(c.data.iter().flatten().all(|v| v.is_finite()), "{name}");
        // Sample lattice points: a mid grey, a warm and a cool colour, a dark and a bright one.
        for idx in [[8usize, 8, 8], [10, 8, 6], [6, 8, 10], [3, 3, 2], [14, 13, 12]] {
            let lin = idx.map(ax);
            let o = apply_at_lattice(&c, idx, lin);
            let (yi, yo) = (luma(lin), luma(o));
            assert!((yo - yi).abs() <= yi * 1e-3, "{name} {idx:?}: luminance {yi} -> {yo}");
            let ratio = |v: [f32; 3]| v[0] / v[2];
            match *name {
                "identity.cube" => assert!(close(o, lin, 1e-4), "{name} {idx:?}: {o:?} vs {lin:?}"),
                "warm.cube" => assert!(ratio(o) > ratio(lin) * 1.1, "{name} must warm: {o:?} vs {lin:?}"),
                "cool.cube" => assert!(ratio(o) < ratio(lin) / 1.1, "{name} must cool: {o:?} vs {lin:?}"),
                _ => {}
            }
        }
        // Teal-orange: the shadows go toward teal (blue up), the highlights toward orange (red up).
        if *name == "teal-orange.cube" {
            let dark = [2usize, 2, 2];
            let bright = [14usize, 14, 14];
            let od = apply_at_lattice(&c, dark, dark.map(ax));
            let ob = apply_at_lattice(&c, bright, bright.map(ax));
            assert!(od[2] > od[0], "dark greys go teal: {od:?}");
            assert!(ob[0] > ob[2], "bright greys go orange: {ob:?}");
        }
    }
}

#[test]
fn the_example_luts_are_written_once_and_listed() {
    let dir = std::env::temp_dir().join(format!("gw2g_grading_examples_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    assert!(list_luts_in(&dir).is_empty(), "no folder: no LUTs");
    assert_eq!(write_example_luts_in(&dir), Ok(4));
    // Never overwrites: an edited file stays.
    std::fs::write(dir.join("luts/warm.cube"), "mine").unwrap();
    std::fs::write(dir.join("luts/Notes.txt"), "not a lut").unwrap();
    assert_eq!(write_example_luts_in(&dir), Ok(0));
    assert_eq!(std::fs::read_to_string(dir.join("luts/warm.cube")).unwrap(), "mine");
    assert_eq!(list_luts_in(&dir), ["luts/cool.cube", "luts/identity.cube", "luts/teal-orange.cube", "luts/warm.cube"]);
    // Each listed value is a legal grade_lut path.
    for l in list_luts_in(&dir) {
        assert!(resolve_lut_path(&dir, &l).is_ok(), "{l}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// The copies in the repo (graphics/presets/luts) are exactly what the mod writes.
#[test]
fn the_repo_copies_of_the_example_luts_are_current() {
    let folder = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../presets/luts");
    for (name, text) in example_luts() {
        let p = folder.join(name);
        let Ok(have) = std::fs::read_to_string(&p) else { panic!("missing {} (run `cargo test write_repo_example_luts -- --ignored`)", p.display()) };
        assert_eq!(have.replace("\r\n", "\n"), text, "{name} is stale (run `cargo test write_repo_example_luts -- --ignored`)");
    }
}

/// Regenerates graphics/presets/luts (not part of the normal run).
#[test]
#[ignore]
fn write_repo_example_luts() {
    let folder = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../presets/luts");
    std::fs::create_dir_all(&folder).unwrap();
    for (name, text) in example_luts() {
        std::fs::write(folder.join(name), text).unwrap();
    }
}
