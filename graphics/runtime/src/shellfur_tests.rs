//! Tests of the shell fur: the config keys, the selector, the packed key, the generated geometry shader for the game's real vertex
//! layouts, and a WARP scene through the real pipeline (fur mask -> record -> replay -> the real light-adjust substitute): a lit sphere
//! (the charr's fur) drawn with a vertex shader that has the engine's output signature, a cloth quad in front of it, a bright sky.
//! With `GW2G_SHELL_PNG=<folder>` the scene renders are written there as PNG (to look at them).

use super::*;

use std::ffi::c_void;

use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::Interface;

// ---- config ---------------------------------------------------------------------------------------------------------

#[test]
fn shell_keys_default_parse_clamp_and_round_trip() {
    let d = ShellCfg::default();
    // v10: 2.5 cm strands (was 4) and the LOD scaled with them (12.5 m, was 20).
    assert_eq!((d.on, d.shells, d.length, d.density, d.thickness, d.root_dark, d.wind, d.lod, d.rim), (1, 16, 2.5, 1200.0, 0.6, 0.6, 0.15, 12.5, 1.0));
    assert_eq!((d.detail, d.clump), (0.7, 0.5));
    assert_eq!((d.ps.as_str(), d.vs.as_str(), d.min_vcb, d.counts.as_str(), d.normal.as_str(), d.unit, d.debug), ("", "", 0, "", "face", 0.0, 0)); // v14: shell_min_vcb 0 (was 4096)
    assert_eq!(crate::config::Config::default().shell, d);
    let c = crate::config::Config::parse(
        "fur_shell=0\nfur_shells=24\nshell_length=6\nshell_density=1500\nshell_thickness=0.5\nshell_root_dark=0.3\nshell_wind=0.8\nshell_lod_distance=40\nshell_rim=1.5\nshell_ps=1D50763F\nshell_vs=ba0709f4\nshell_min_vcb=16000\nshell_counts=17694, -5214\nshell_normal=TEXCOORD4\nshell_unit=39.37\nshell_debug=1\nshell_detail=0.25\nshell_clump=0.9",
    )
    .shell;
    assert_eq!((c.on, c.shells, c.length, c.density, c.thickness, c.root_dark, c.wind, c.lod, c.rim), (0, 24, 6.0, 1500.0, 0.5, 0.3, 0.8, 40.0, 1.5));
    assert_eq!((c.ps.as_str(), c.vs.as_str(), c.min_vcb, c.counts.as_str(), c.normal.as_str(), c.unit, c.debug), ("1d50763f", "ba0709f4", 16000, "17694, -5214", "texcoord4", 39.37, 1));
    assert_eq!((c.detail, c.clump), (0.25, 0.9));
    let c = crate::config::Config::parse("shell_detail=7\nshell_clump=-2").shell;
    assert_eq!((c.detail, c.clump), (1.0, 0.0), "clamped");
    let c = crate::config::Config::parse("shell_detail=nan\nshell_clump=x").shell;
    assert_eq!((c.detail, c.clump), (DETAIL_DEFAULT, CLUMP_DEFAULT), "NaN / garbage = the default");
    // Clamped.
    let c = crate::config::Config::parse("fur_shell=7\nfur_shells=99\nshell_length=99\nshell_density=1\nshell_thickness=5\nshell_root_dark=-1\nshell_wind=4\nshell_lod_distance=0\nshell_rim=9\nshell_min_vcb=-4\nshell_unit=99999\nshell_debug=3").shell;
    assert_eq!((c.on, c.shells, c.length, c.density, c.thickness, c.root_dark, c.wind, c.lod, c.rim, c.min_vcb, c.unit, c.debug), (1, 32, 20.0, 50.0, 1.0, 0.0, 1.0, 2.0, 2.0, 0, 1000.0, 2));
    assert_eq!(crate::config::Config::parse("fur_shells=2").shell.shells, 4, "1..3 become the smallest real count");
    assert_eq!(crate::config::Config::parse("fur_shells=0").shell.shells, 0, "0 = off");
    // NaN / garbage = the default (NaN-safe), shell_unit 0 / negative / garbage = auto.
    let c = crate::config::Config::parse("fur_shell=x\nfur_shells=nan\nshell_length=NaN\nshell_density=inf\nshell_thickness=\nshell_root_dark=abc\nshell_wind=nan\nshell_lod_distance=-inf\nshell_rim=x\nshell_unit=-3\nshell_normal=garbage7").shell;
    assert_eq!(c, ShellCfg { unit: 0.0, ..ShellCfg::default() });
    assert_eq!(crate::config::Config::parse("shell_unit=abc").shell.unit, 0.0);
    // The ini block: every key once, with its comment, and it parses back to the same values.
    let x = ShellCfg { on: 0, shells: 12, length: 2.5, density: 600.0, thickness: 0.4, root_dark: 0.5, wind: 0.1, lod: 12.0, rim: 0.5, ps: "aabbccdd".into(), vs: "11223344".into(), min_vcb: 208, counts: "-5214".into(), normal: "texcoord2".into(), unit: 1.0, debug: 1, detail: 0.4, clump: 0.2, fur_mask: "t2.l>".into(), fur_threshold: 0.3, fur_soft: 0.08, uv_exclude: "0.1 0.2 0.3 0.4; 0.5 0.6 0.7 0.8@17694".into(), auto: 0 };
    let t = crate::config::Config { shell: x.clone(), ..crate::config::Config::default() }.to_ini();
    for k in KEYS {
        assert_eq!(t.lines().filter(|l| l.starts_with(&format!("{k}="))).count(), 1, "{k} once in\n{t}");
    }
    assert!(t.contains("# Shell fur (docs/shell-fur.md)"));
    assert!(t.find("fur_view=0\n").unwrap() < t.find("fur_shell=0\n").unwrap(), "after the fur look");
    assert_eq!(crate::config::Config::parse(&t).shell, x);
    assert_eq!(crate::config::Config::parse(&crate::config::Config::default().to_ini()), crate::config::Config::default());
    // clamped() (the overlay) is NaN-safe.
    let n = ShellCfg { length: f32::NAN, density: f32::INFINITY, thickness: -1.0, lod: f32::NAN, unit: f32::NAN, detail: f32::NAN, clump: 9.0, ..ShellCfg::default() }.clamped();
    assert_eq!((n.length, n.density, n.thickness, n.lod, n.unit, n.detail, n.clump), (2.5, 1200.0, 0.1, 12.5, 0.0, 0.7, 1.0));
}

#[test]
fn shell_keys_hot_reload_one_by_one() {
    let a = ShellCfg::default();
    let b = ShellCfg { length: 6.0, counts: "17694".into(), ..ShellCfg::default() };
    assert_eq!(ShellCfg::changed(&a, &b), vec!["shell_length", "shell_counts"]);
    let mut cur = ShellCfg { wind: 0.9, ..ShellCfg::default() }; // a slider value the file did not change
    for k in ShellCfg::changed(&a, &b) {
        cur.take(k, &b);
    }
    assert_eq!((cur.length, cur.counts.as_str(), cur.wind), (6.0, "17694", 0.9));
    assert!(KEYS.iter().all(|k| ShellCfg::is_key(k)) && !ShellCfg::is_key("fur_strength"));
    // Through the debug mode's diff / merge.
    let old = crate::config::Config::default();
    let new = crate::config::Config::parse("shell_wind=0.6\nshell_debug=1");
    let (m, d) = crate::debugshot::merge(&old, &old, &new);
    assert_eq!(d.live, vec!["shell_wind", "shell_debug"]);
    assert_eq!((m.shell.wind, m.shell.debug), (0.6, 1));
}

#[test]
fn selector_takes_the_skinned_fur_draws_and_the_counts() {
    assert_eq!(parse_counts("17694, -5214;+30 x -y 17694"), (vec![17694, 30], vec![5214]));
    let s = Selector::new(&ShellCfg::default());
    // The reference frame: skinned ba0709f4 (vcb 16464) yes; v14: the rigid 59f8b692 (vcb 208) too (NPC bodies / heads; the hair cards
    // among them fail the per-draw test). Changed test: v1..v13 refused it by the vcb. A saved 4096 (the old default) reads as 0; any
    // other value is honoured.
    assert_eq!(s.verdict(0x1d50763f, Some(0xba0709f4), 16464, 17694), Ok(()));
    assert_eq!(s.verdict(0x1d50763f, Some(0x59f8b692), 208, 5214), Ok(()));
    assert_eq!(MIN_VCB_DEFAULT, 0);
    let legacy = Selector::new(&ShellCfg { min_vcb: MIN_VCB_LEGACY, ..ShellCfg::default() });
    assert_eq!(legacy.verdict(0x1d50763f, Some(0x59f8b692), 208, 1824), Ok(()));
    let strict = Selector::new(&ShellCfg { min_vcb: 8192, ..ShellCfg::default() });
    assert_eq!(strict.verdict(0x1d50763f, Some(0x59f8b692), 208, 1824), Err("vcb < shell_min_vcb"));
    let s = Selector::new(&ShellCfg { ps: "1d50763f".into(), vs: "ba0709f4".into(), counts: "-3033".into(), ..ShellCfg::default() });
    assert_eq!(s.verdict(0xe0189185, Some(0xba0709f4), 16464, 100), Err("ps not in shell_ps"));
    assert_eq!(s.verdict(0x1d50763f, Some(0x3c3a91ca), 16464, 100), Err("vs not in shell_vs"));
    assert_eq!(s.verdict(0x1d50763f, None, 16464, 100), Err("vs not in shell_vs"));
    assert_eq!(s.verdict(0x1d50763f, Some(0xba0709f4), 16464, 3033), Err("count left out by shell_counts"));
    let s = Selector::new(&ShellCfg { counts: "17694".into(), ..ShellCfg::default() });
    assert_eq!(s.verdict(1, None, 16464, 3033), Err("count not in shell_counts"));
    assert_eq!(s.verdict(1, None, 16464, 17694), Ok(()));
}

#[test]
fn v15_the_dynamic_copy_windows() {
    // Indexed: per stream [offset + base x stride, + count x stride); the index window only when the index buffer is dynamic.
    let w = dyn_windows(Call::Indexed(300, 9000, 6000), &[32, 16, 0, 0], &[64, 0, 0, 0], 42 /* R32_UINT */, 128, true).unwrap();
    assert_eq!(w, [(64 + 6000 * 32, 300 * 32), (6000 * 16, 300 * 16), (0, 0), (0, 0), (128 + 9000 * 4, 1200)]);
    let w = dyn_windows(Call::Indexed(300, 9000, 6000), &[32, 0, 0, 0], &[0; 4], 57 /* R16_UINT */, 0, false).unwrap();
    assert_eq!(w[4], (0, 0), "a static index buffer is not copied");
    assert_eq!(dyn_windows(Call::Indexed(300, 0, 0), &[32, 0, 0, 0], &[0; 4], 57, 0, true).unwrap()[4], (0, 600), "16-bit indices");
    // Not indexed: [start, start + count).
    assert_eq!(dyn_windows(Call::Draw(99, 33), &[20, 0, 0, 0], &[0; 4], 0, 0, false).unwrap()[0], (33 * 20, 99 * 20));
    // Instanced: not bounded by the count: refused.
    assert!(dyn_windows(Call::IndexedInstanced(300, 2, 0, 0, 0), &[32, 0, 0, 0], &[0; 4], 57, 0, true).is_none());
    // A negative base vertex before the buffer's start: refused.
    assert!(dyn_windows(Call::Indexed(30, 0, -5), &[32, 0, 0, 0], &[0; 4], 57, 0, false).is_none());
}

#[test]
fn v14_the_record_cap_and_the_triangle_budget() {
    // A crowd: draws arrive in the engine's order; each is admitted while fewer than MAX_RECS records and MAX_FRAME_TRIS triangles.
    // v16: 256 records / 800000 triangles (changed test: v14 pinned 64 / 400000, the replaced behaviour)
    assert_eq!((MAX_RECS, MAX_FRAME_TRIS), (256, 800_000));
    assert_eq!(admit(0, 0, 1000), Ok(()));
    assert_eq!(admit(MAX_RECS - 1, 0, 1000), Ok(()));
    assert_eq!(admit(MAX_RECS, 0, 1000), Err("too many shell draws this frame (cap)"));
    assert_eq!(admit(10, MAX_FRAME_TRIS - 1000, 1000), Ok(()));
    assert_eq!(admit(10, MAX_FRAME_TRIS - 999, 1000), Err("shell triangle budget of the frame"));
    // Black Citadel crowd frames: 82 / 130 / 47 fur draws, every one a
    // charr mesh of 120..5898 triangles (median ~1100). Admitted in draw order: v13 24 records, v14 up to 64 (the budget is never the
    // limit there: 64 x 5898 = 377472 < 400000).
    let crowd = |n: usize, tris: u32| {
        let (mut k, mut t) = (0usize, 0u32);
        for _ in 0..n {
            if admit(k, t, tris).is_ok() {
                k += 1;
                t += tris;
            }
        }
        k
    };
    // v16: also a frame with 136 and the largest seen (247) take every draw.
    for (n, want) in [(82usize, 82usize), (130, 130), (47, 47), (136, 136), (247, 247)] {
        assert_eq!(crowd(n, 1100), want);
        println!("a frame of {n} fur draws: v13 {} records, v14/v15 {}, v16 {}", n.min(24), n.min(64), crowd(n, 1100));
    }
    assert_eq!(crowd(130, 5898), 130);
    assert_eq!(crowd(300, 1100), MAX_RECS, "past 256 the cap holds (the hysteresis then keeps last frame's draws: WARP test)");
}

#[test]
fn v16_past_the_cap_the_draws_of_the_last_frame_keep_their_shells_whatever_the_draw_order() {
    // 300 fur draws (more than MAX_RECS), the engine's order changing every frame (the camera angle): v15 gave shells to whichever
    // came first; v16: after the first frame the same draws keep them (a draw shelled last frame displaces one that was not).
    let ids: Vec<u64> = (0..300u64).map(|k| 1000 + k).collect();
    let frame = |order: &[u64], prev: &std::collections::HashSet<u64>| -> std::collections::HashSet<u64> {
        let mut recs: Vec<(u64, u32)> = Vec::new();
        for &id in order {
            let t: u32 = recs.iter().map(|r| r.1).sum();
            if admit(recs.len(), t, 1000).is_ok() {
                recs.push((id, 1000));
            } else if let Some(v) = victim_of(prev, &recs, id, 1000) {
                recs[v] = (id, 1000);
            }
        }
        recs.iter().map(|r| r.0).collect()
    };
    let mut prev = std::collections::HashSet::new();
    let mut first = std::collections::HashSet::new();
    let mut flips_v15 = 0usize;
    let mut last_v15: std::collections::HashSet<u64> = std::collections::HashSet::new();
    for f in 0..20u64 {
        // a different draw order each frame (a rotation and a reversal: the angle changes what the engine draws first)
        let mut order = ids.clone();
        order.rotate_left(((f * 37) % 300) as usize);
        if f % 2 == 1 {
            order.reverse();
        }
        let set = frame(&order, &prev);
        assert_eq!(set.len(), MAX_RECS);
        if f == 0 {
            first = set.clone();
        } else {
            assert_eq!(set, first, "frame {f}: the same draws keep their shells");
        }
        prev = set;
        let v15: std::collections::HashSet<u64> = order.iter().take(MAX_RECS).copied().collect();
        if f > 0 {
            flips_v15 += v15.symmetric_difference(&last_v15).count() / 2;
        }
        last_v15 = v15;
    }
    println!("20 frames of 300 fur draws in changing orders: v16 0 draws lose or gain shells after the first frame; by the draw order alone {flips_v15}");
    assert!(flips_v15 > 100);
}

#[test]
fn the_packed_key_round_trips_and_orders_by_height_then_coverage() {
    for h in [0.0, 0.004, 0.25, 0.5, 0.99, 1.0] {
        for a in [1.0 / 15.0, 0.5, 1.0] {
            for d in [[0.0, 0.0], [12.25, -3.5], [-127.75, 127.75], [-128.0, 0.25]] {
                let k = pack_key(h, a, d);
                let (h2, a2, d2) = unpack_key(k).unwrap();
                assert!((h2 - h).abs() <= 0.5 / 125.0 + 1e-6, "{h} -> {h2}");
                assert!((a2 - a).abs() <= 0.5 / 15.0 + 1e-6, "{a} -> {a2}");
                assert_eq!(d2, d);
                let f = f32::from_bits(k);
                assert!(f.is_finite() && f > 0.0 && f.is_normal(), "a positive normal float: MAX blending orders it like its bits");
            }
        }
    }
    // A higher shell always wins the MAX blend, whatever the coverage and offsets; at the same shell the more covering fragment wins.
    assert!(f32::from_bits(pack_key(0.5, 1.0 / 15.0, [-120.0, -120.0])) > f32::from_bits(pack_key(0.49, 1.0, [120.0, 120.0])));
    assert!(f32::from_bits(pack_key(0.5, 0.8, [-120.0, -120.0])) > f32::from_bits(pack_key(0.5, 0.6, [120.0, 120.0])));
    assert!(f32::from_bits(pack_key(0.0, 1.0, [0.0, 0.0])) > 0.0, "the base layer is a strand fragment too (never 0)");
    assert_eq!(unpack_key(0), None);
    assert_eq!(unpack_key(0x7f80_0000), None, "infinity is not a key");
    assert_eq!(unpack_key(0x00ff_ffff), None);
    assert_eq!(unpack_key(0x0100_0000), None, "zero coverage is not a key (the shader drops such fragments)");
    // Offsets beyond the range clamp (the geometry shader never extrudes that far: MAX_PX x 2 at most).
    assert!(MAX_PX * 2.0 <= 128.0);
    assert_eq!(unpack_key(pack_key(0.5, 1.0, [9000.0, -9000.0])).unwrap().2, [127.75, -128.0]);
}

#[test]
fn constants_are_clamped_and_zero_when_not_ready() {
    let c = ShellCfg::default();
    let g = gs_cb_values(&c, &DrawGeom { vp: [0.0, 0.0, 3440.0, 1440.0], ccw: false }, 12.5, 1000.0, 0.0);
    assert_eq!(g[0][0], 16.0);
    assert!((g[0][1] - 0.025).abs() < 1e-7, "2.5 cm in metres");
    assert_eq!(g[0][3], 0.0, "auto unit, nothing measured yet: per triangle");
    assert!((g[1][1] - 2.0).abs() < 1e-6 && (g[1][0] - 2.0 * 1440.0 / 3440.0).abs() < 1e-6, "P11 = 1 / tan(half fov), P00 = P11 / aspect");
    assert_eq!((g[2][2], g[2][3], g[3][2], g[3][3]), (0.0, 12.5, 0.0, 1000.0));
    assert_eq!(g[4], wind_phases(12.5));
    // The frame's measured unit goes to the geometry shader; a fixed shell_unit wins over it.
    assert_eq!(gs_cb_values(&c, &DrawGeom { vp: [0.0, 0.0, 8.0, 8.0], ccw: false }, 0.0, 1000.0, 39.5)[0][3], 39.5);
    assert_eq!(gs_cb_values(&ShellCfg { unit: 2.0, ..c.clone() }, &DrawGeom { vp: [0.0, 0.0, 8.0, 8.0], ccw: false }, 0.0, 1000.0, 39.5)[0][3], 2.0);
    let off = gs_cb_values(&ShellCfg { on: 0, ..c.clone() }, &DrawGeom { vp: [0.0; 4], ccw: true }, f64::NAN, f32::NAN, f32::NAN);
    assert_eq!((off[0][0], off[0][3], off[2][2], off[3][3]), (0.0, 0.0, 1.0, crate::aerial::DEFAULT_UNIT));
    for v in off.iter().chain(g.iter()).flatten() {
        assert!(v.is_finite());
    }
    let p = ps_cb_values(&c, 1000.0);
    assert_eq!((p[0][0], p[0][1], p[0][2], p[0][3]), (1200.0, 0.6, MIN_CELL_PX, CLUMP));
    // v7: the fragment's own depth test (5 mm + 0.3 %), no longer the root's depth + the whole strand reach (4.5 cm + 0.6 %).
    assert!((p[1][0] - FRAG_TOL_M / 1000.0).abs() < 1e-9, "5 mm, normalised");
    assert_eq!((p[1][1], p[1][2], p[1][3]), (FRAG_REL_TOL, MIN_STRAND_PX, LEAN * 0.5));
    assert_eq!(ps_ext_values(1000.0), [ROOT_DEV_TOL, ROOT_LIN_REL, ROOT_LIN_M / 1000.0, FRAG_TOL_M]); // v8: w = the device-depth fragment tolerance (m)
    assert!(ps_ext_values(f32::NAN).iter().all(|v| v.is_finite()));
    assert_eq!(composite_cb_values(false, &c), [[0.0; 4]; 2]);
    assert_eq!(composite_cb_values(true, &ShellCfg { shells: 0, ..c.clone() }), [[0.0; 4]; 2]);
    assert_eq!(composite_cb_values(true, &c), [[1.0, 0.6, 1.0, 0.0], [0.7, 0.0, 0.0, 0.0]]);
    assert_eq!(Call::IndexedInstanced(30, 2, 0, 0, 0).count(), 30);
    assert_eq!((Call::Indexed(9, 0, 0).instances(), Call::Instanced(4, 3, 0, 0).instances()), (1, 3));
}

// ---- the generated geometry shader -------------------------------------------------------------------------------------

fn el(name: &str, index: u32, reg: u32, mask: u8, sys: u32) -> SigElem {
    SigElem { name: name.to_string(), index, sys, ctype: 3, reg, mask }
}

/// The three output layouts of the game's character vertex shaders (the log of a play session, see furmask_tests).
fn real_layouts() -> Vec<(&'static str, VsSig)> {
    vec![
        ("four registers", VsSig { elems: vec![el("TEXCOORD", 0, 0, 0x3, 0), el("TEXCOORD", 1, 1, 0xf, 0), el("TEXCOORD", 2, 2, 0xf, 0), el("TEXCOORD", 3, 3, 0xf, 0), el("SV_POSITION", 0, 4, 0xf, 1)] }),
        (
            "z packed beside the uv",
            VsSig { elems: vec![el("TEXCOORD", 0, 0, 0x3, 0), el("TEXCOORD", 1, 0, 0x4, 0), el("TEXCOORD", 2, 1, 0xf, 0), el("TEXCOORD", 3, 2, 0xf, 0), el("TEXCOORD", 4, 3, 0xf, 0), el("SV_POSITION", 0, 4, 0xf, 1)] },
        ),
        (
            "z and w packed beside the uv",
            VsSig {
                elems: vec![
                    el("TEXCOORD", 0, 0, 0x3, 0),
                    el("TEXCOORD", 1, 0, 0x4, 0),
                    el("TEXCOORD", 5, 0, 0x8, 0),
                    el("TEXCOORD", 2, 1, 0x7, 0),
                    el("TEXCOORD", 3, 2, 0x7, 0),
                    el("TEXCOORD", 4, 3, 0x7, 0),
                    el("SV_POSITION", 0, 4, 0xf, 1),
                ],
            },
        ),
    ]
}

#[test]
fn the_geometry_shader_compiles_and_links_to_the_real_character_vertex_layouts() {
    for (what, sig) in real_layouts() {
        let uv = sig.pick(&crate::furmask::UvPref::Auto).unwrap();
        for normal in ["face", "texcoord3", "texcoord4"] {
            let n = normal_elem(&sig, normal);
            if normal == "face" {
                assert_eq!(n, None);
            }
            let src = gs_hlsl(&sig, uv, n).unwrap();
            build_gs(&sig, uv, n).unwrap_or_else(|e| panic!("{what} / {normal}: {e}\n{}\n{}", sig.text(), &src[..src.find("// Shell fur").unwrap_or(400).min(src.len())]));
        }
    }
    // The pixel shaders compile alone (no SHELL_GS).
    shaders::compile(HLSL, PS_ENTRY, "ps_5_0").unwrap();
    shaders::compile(HLSL, MEASURE_PS_ENTRY, "ps_5_0").unwrap();
    // No SV_Position = no geometry shader (the draw gets no shells), never a panic.
    let sig = VsSig { elems: vec![el("TEXCOORD", 0, 0, 0x3, 0)] };
    let uv = sig.pick(&crate::furmask::UvPref::Auto).unwrap();
    assert!(gs_hlsl(&sig, uv, None).is_err());
    assert_eq!(normal_elem(&real_layouts()[2].1, "texcoord1"), None, "a 1-component output is not a normal");
    assert_eq!(normal_elem(&real_layouts()[2].1, "texcoord4"), Some(5));
    assert_eq!(clean_normal(" TEXCOORD4 "), "texcoord4");
    assert_eq!(clean_normal("texcoord"), "face");
}

// ---- the WARP scene --------------------------------------------------------------------------------------------------

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

fn tex(g: &Gpu, w: u32, h: u32, fmt: DXGI_FORMAT, bind: u32, usage: D3D11_USAGE, cpu: u32, init: Option<(&[u8], u32)>) -> ID3D11Texture2D {
    let desc = D3D11_TEXTURE2D_DESC { Width: w, Height: h, MipLevels: 1, ArraySize: 1, Format: fmt, SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, Usage: usage, BindFlags: bind, CPUAccessFlags: cpu, MiscFlags: 0 };
    let sd = init.map(|(d, pitch)| D3D11_SUBRESOURCE_DATA { pSysMem: d.as_ptr() as *const c_void, SysMemPitch: pitch, SysMemSlicePitch: 0 });
    let mut t = None;
    unsafe { g.dev.CreateTexture2D(&desc, sd.as_ref().map(|s| s as *const _), Some(&mut t)).unwrap() };
    t.unwrap()
}

fn read(g: &Gpu, src: &ID3D11Texture2D, w: u32, h: u32, fmt: DXGI_FORMAT, bpp: usize) -> Vec<u8> {
    let st = tex(g, w, h, fmt, 0, D3D11_USAGE_STAGING, D3D11_CPU_ACCESS_READ.0 as u32, None);
    unsafe {
        g.ctx.CopyResource(&st, src);
        let mut m = D3D11_MAPPED_SUBRESOURCE::default();
        g.ctx.Map(&st, 0, D3D11_MAP_READ, 0, Some(&mut m)).unwrap();
        let mut out = Vec::with_capacity((w * h) as usize * bpp);
        for y in 0..h as usize {
            out.extend_from_slice(std::slice::from_raw_parts((m.pData as *const u8).add(y * m.RowPitch as usize), w as usize * bpp));
        }
        g.ctx.Unmap(&st, 0);
        out
    }
}

fn f16(h: u16) -> f32 {
    let s = if h >> 15 != 0 { -1.0 } else { 1.0 };
    let e = ((h >> 10) & 0x1f) as i32;
    let m = (h & 0x3ff) as f32;
    s * match e {
        0 => m * 2f32.powi(-24),
        31 => f32::INFINITY,
        _ => (1.0 + m / 1024.0) * 2f32.powi(e - 15),
    }
}

fn buffer(g: &Gpu, bytes: &[u8], bind: D3D11_BIND_FLAG) -> ID3D11Buffer {
    let d = D3D11_BUFFER_DESC { ByteWidth: bytes.len() as u32, Usage: D3D11_USAGE_IMMUTABLE, BindFlags: bind.0 as u32, ..Default::default() };
    let sd = D3D11_SUBRESOURCE_DATA { pSysMem: bytes.as_ptr() as *const c_void, SysMemPitch: 0, SysMemSlicePitch: 0 };
    let mut b = None;
    unsafe { g.dev.CreateBuffer(&d, Some(&sd), Some(&mut b)).unwrap() };
    b.unwrap()
}

fn floats(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

const NEAR: f32 = 0.1;
const FAR: f32 = 1000.0;

/// The "engine's" character vertex shader: the output signature of the game's (TEXCOORD0.xy uv, TEXCOORD1 / TEXCOORD5 packed beside it,
/// three float3 frames, SV_POSITION last at o4). The mesh is a unit sphere scaled and moved by cb0; units are metres.
const VS_CHAR: &str = "cbuffer c : register(b0) { float4 ctr; float4 proj; float4 jit; };
struct VI { float3 p : POSITION; float3 n : NORMAL; float2 uv : TEXCOORD0; };
struct VO { float2 uv : TEXCOORD0; float vz : TEXCOORD1; float sh : TEXCOORD5; float3 t : TEXCOORD2; float3 b : TEXCOORD3; float3 n : TEXCOORD4; float4 pos : SV_POSITION; };
VO main(VI i) { float3 w = ctr.xyz + i.p * ctr.w; VO o; o.uv = i.uv; o.vz = w.z; o.sh = 0.0; o.n = i.n;
  o.t = normalize(cross(float3(0.0, 1.0, 0.0), i.n) + float3(1e-4, 0.0, 0.0)); o.b = cross(i.n, o.t);
  float zc = (w.z - proj.z) * proj.w / (proj.w - proj.z);
  o.pos = float4(w.x * proj.x + jit.x * w.z, w.y * proj.y + jit.y * w.z, zc, w.z); return o; }";

/// The "engine's" fur pixel shader: a white lion-like coat lit by a sun from the upper left, the linear depth in RT1.
const PS_CHAR: &str = "cbuffer c : register(b0) { float4 ctr; float4 proj; float4 jit; float4 ex; };
Texture2D<float4> mat : register(t0); SamplerState ms : register(s0);
struct VO { float2 uv : TEXCOORD0; float vz : TEXCOORD1; float sh : TEXCOORD5; float3 t : TEXCOORD2; float3 b : TEXCOORD3; float3 n : TEXCOORD4; float4 pos : SV_POSITION; };
struct PO { float4 c : SV_Target0; float z : SV_Target1; };
PO main(VO i) { if (ex.x > 0.5 && frac(i.uv.x * 24.0) > 0.55) discard; float3 L = normalize(float3(-0.55, 0.6, -0.55)); float nl = saturate(dot(normalize(i.n), L));
  float3 alb = float3(0.88, 0.82, 0.70) * (0.93 + 0.07 * sin(i.uv.x * 180.0) * sin(i.uv.y * 90.0));
  if (jit.z > 0.5 && length((i.uv - float2(0.615, 0.40)) * float2(2.0, 1.0)) < 0.025) alb = float3(0.05, 0.07, 0.10);
  if (jit.w > 0.5) alb = mat.Sample(ms, i.uv).rgb;
  float3 lin = alb * (0.18 + 0.82 * nl); PO o; o.c = float4(sqrt(lin), 0.5); o.z = i.vz / proj.w; return o; }";

/// Cloth: a quad (clip rect at view depth z) of dark red cloth, drawn after the character, in front of it.
const VS_QUAD: &str = "cbuffer c : register(b1) { float4 r; float4 zz; };
struct VO { float vz : TEXCOORD0; float4 pos : SV_POSITION; };
VO main(uint id : SV_VertexID) { float2 u = float2(id & 1, id >> 1); float2 p = lerp(r.xy, r.zw, u); VO o; o.vz = zz.x;
  float zc = (zz.x - zz.y) * zz.z / (zz.z - zz.y); o.pos = float4(p * zz.x, zc, zz.x); return o; }";
const PS_QUAD: &str = "cbuffer c : register(b1) { float4 r; float4 zz; };
struct VO { float vz : TEXCOORD0; float4 pos : SV_POSITION; };
struct PO { float4 c : SV_Target0; float z : SV_Target1; };
PO main(VO i) { PO o; o.c = float4(sqrt(float3(0.30, 0.05, 0.04)), 0.5); o.z = i.vz / zz.z; return o; }";

/// The background: a sky gradient (or black), the linear depth of the far plane.
const VS_FULL: &str = "struct O { float4 pos : SV_POSITION; float4 uv : TEXCOORD0; };
O main(uint id : SV_VertexID) { float2 p = float2((id << 1) & 2, id & 2); O o; o.pos = float4(p * float2(2, -2) + float2(-1, 1), 0.9999, 1); o.uv = float4(p, 0, 0); return o; }";
const PS_SKY: &str = "cbuffer c : register(b2) { float4 sky; };
struct O { float4 pos : SV_POSITION; float4 uv : TEXCOORD0; };
struct PO { float4 c : SV_Target0; float z : SV_Target1; };
PO main(O i) { PO o; float3 lin = sky.rgb * lerp(1.15, 0.75, i.uv.y); o.c = float4(sqrt(lin), 0.0); o.z = 1.0; return o; }";

fn compile(src: &str, profile: &str) -> Vec<u8> {
    shaders::compile(src, "main", profile).unwrap_or_else(|e| panic!("{e}\n{src}"))
}

/// A unit sphere: (positions + normals + uv, 8 floats per vertex; indices), wound CLOCKWISE seen from outside (the D3D default front).
fn sphere(rings: u32, segs: u32) -> (Vec<f32>, Vec<u32>) {
    let mut v = Vec::new();
    for r in 0..=rings {
        let th = std::f32::consts::PI * r as f32 / rings as f32;
        for s in 0..=segs {
            let ph = 2.0 * std::f32::consts::PI * s as f32 / segs as f32;
            let n = [th.sin() * ph.cos(), th.cos(), th.sin() * ph.sin()];
            v.extend_from_slice(&[n[0], n[1], n[2], n[0], n[1], n[2], s as f32 / segs as f32, r as f32 / rings as f32]);
        }
    }
    let mut i = Vec::new();
    let row = segs + 1;
    for r in 0..rings {
        for s in 0..segs {
            let (a, b, c, d) = (r * row + s, r * row + s + 1, (r + 1) * row + s, (r + 1) * row + s + 1);
            i.extend_from_slice(&[a, b, c, b, d, c]);
        }
    }
    // Orientation check: the outward normal of a triangle must equal cross(p1 - p0, p2 - p0) in this left-handed space (x right, y up,
    // z forward); flip every triangle otherwise.
    let p = |k: u32| [v[k as usize * 8], v[k as usize * 8 + 1], v[k as usize * 8 + 2]];
    let t = (rings / 2 * segs + segs / 4) as usize * 6;
    let (p0, p1, p2) = (p(i[t]), p(i[t + 1]), p(i[t + 2]));
    let e1 = [p1[0] - p0[0], p1[1] - p0[1], p1[2] - p0[2]];
    let e2 = [p2[0] - p0[0], p2[1] - p0[1], p2[2] - p0[2]];
    let cr = [e1[1] * e2[2] - e1[2] * e2[1], e1[2] * e2[0] - e1[0] * e2[2], e1[0] * e2[1] - e1[1] * e2[0]];
    let out = [p0[0] + p1[0] + p2[0], p0[1] + p1[1] + p2[1], p0[2] + p1[2] + p2[2]];
    if cr[0] * out[0] + cr[1] * out[1] + cr[2] * out[2] < 0.0 {
        for tri in i.chunks_exact_mut(3) {
            tri.swap(1, 2);
        }
    }
    (v, i)
}

/// v8: a flat card (1 x 1 m at the body's 0.5 scale: object space -1..1), through the centre, its normal (0, sqrt(1 - c^2), -c): seen
/// from the camera on +z at |cos| = c. `n` x `n` quads, uv 0..1, wound so the camera side is the front.
fn card(c: f32, n: u32) -> (Vec<f32>, Vec<u32>) {
    let nn = [0.0, (1.0 - c * c).max(0.0).sqrt(), -c];
    let t = [0.0, c, (1.0 - c * c).max(0.0).sqrt()]; // in the card's plane, perpendicular to x
    let mut v = Vec::new();
    for j in 0..=n {
        for i in 0..=n {
            let (u, w) = (i as f32 / n as f32 * 2.0 - 1.0, j as f32 / n as f32 * 2.0 - 1.0);
            v.extend_from_slice(&[u, w * t[1], w * t[2], nn[0], nn[1], nn[2], i as f32 / n as f32, 1.0 - j as f32 / n as f32]);
        }
    }
    let mut idx = Vec::new();
    let row = n + 1;
    for j in 0..n {
        for i in 0..n {
            let (a, b, cc, d) = (j * row + i, j * row + i + 1, (j + 1) * row + i, (j + 1) * row + i + 1);
            idx.extend_from_slice(&[a, cc, b, b, cc, d]);
        }
    }
    (v, idx)
}

struct Opts {
    w: u32,
    h: u32,
    /// Distance of the sphere's centre (m), its radius 0.5 m.
    dist: f32,
    /// Sky colour (linear); black = a dark scene.
    sky: [f32; 3],
    cloth: bool,
    /// The fur strength (b6[1].z).
    fur: f32,
    /// A dark "eye" on the white coat (the face-feature test of shell_detail).
    eye: bool,
    /// v3: the coat's material texture (t0 of the fur draw: albedo + gloss alpha) with a horn, a nose and a claw region (`REGIONS`), the
    /// fur discriminator's constants for the mask draw and the fur mask for the replay's root test (what furmask::fur_draw does).
    regions: bool,
    /// v4: the material's rg is a bumpy normal map on the coat, flat on the regions (the detail channel `t0.d`).
    bumpy: bool,
    /// v4: the fur draw's probe tag (its index in the frame; `furprobe::tag_value`).
    tag: Option<u32>,
    /// v7: a coat of this colour with the v7 regions (`V7_REGIONS`: a flat horn, a glossy keratin claw, a keratin pad of the coat's gloss,
    /// red lips) as the draw's atlas (the per-strand discriminator reads it; the picture shows it).
    coat: Option<[f32; 3]>,
    /// v7: the cloth quad's distance in front of the sphere's front (m; 0.12 in the v2 scene). 0.02 lies within the strand length.
    cloth_gap: f32,
    /// v7: a second fur sphere (centre x, y, z, radius; drawn after the first, in front of it: "another character"); its index count
    /// differs from the first (`second_count`), so `shell_counts=-<it>` can take it out of the shells.
    second: Option<[f32; 4]>,
    /// v7: the sphere's vertex buffer is DYNAMIC (the record must refuse it).
    dynamic_vb: bool,
    /// v7: the sphere's vertex buffer is rewritten (the sphere moved by this many metres to the right) after the draw and before the
    /// replay: the replay draws stale geometry.
    stale: f32,
    /// v8: the engine's rasterizer depth bias for the character draws (DepthBias, SlopeScaledDepthBias): a test GPU uses 65 / 0;
    /// the tests also use a slope-scaled one.
    bias: Option<(i32, f32)>,
    /// v8: an "arm" (centre x, y, z, radius, metres) IN THE SAME DRAW as the body (appended to its mesh), in front of it.
    arm: Option<[f32; 4]>,
    /// v8: the cloth is drawn AFTER the engine wrote its linear depth (colour and device depth only, like a banner or an alpha-tested
    /// decoration the linear depth target never receives).
    late_cloth: bool,
    /// v8: the body is a flat card (1 x 1 m, through the sphere's centre) whose normal makes this |cos| with the view ray (instead of the
    /// sphere).
    card: Option<f32>,
    /// v9, the island analysis (furisland::MODE): 0 = off (the v8 scenes: never captured, never bound), 1 = on, 2 = every mesh stays
    /// pending, 3 = every mesh failed.
    islands: u8,
    /// v9: the body is the island test mesh (`island_mesh`: the coat sphere on the left half of the atlas + a horn, three claws and an
    /// ear as separate cone islands) with its atlas (`island_level0`: fur-like texels inside the horn and the claw bases).
    island_mesh: bool,
    /// v11: the atlas is a HAIR card's (the coat with the hair's gloss 0.56: the draw test says "not fur").
    hair: bool,
    /// v11: the engine's pixel shader cuts stripes out of the mesh (alpha test: `discard`), like a hair card; the fur mask draws it solid.
    cutout: bool,
    /// v14: the draw is reported like the RIGID 59f8b692 draws of the game (vertex constant buffer 0 of 208 bytes).
    rigid: bool,
    /// v15: the vertex AND index buffers are one big DYNAMIC ring each (like the game's rigid NPC draws): 6000 other vertices / 9000
    /// other indices first, the mesh drawn with base vertex 6000 and start index 9000 (v14 copied whole buffers and refused them).
    ring: bool,
    /// v17: the card's subdivisions (None = 32): 2 = a few LARGE triangles, like a character's own body around a first-person camera.
    card_n: Option<u32>,
}

struct Out {
    /// The light-adjust output with the shells (encoded rgb).
    out: Vec<[f32; 3]>,
    /// The same frame with t24 / b13 unbound (the old output).
    old: Vec<[f32; 3]>,
    /// The shell target's keys.
    keys: Vec<u32>,
    /// The sphere's own pixels (the engine's draw) and the cloth's.
    body: Vec<bool>,
    cloth: Vec<bool>,
    /// The sphere's scene colour (encoded) before the light adjust.
    scene: Vec<[f32; 3]>,
    /// How many draws replayed and the CPU time of the replay.
    replayed: u32,
    micros: u32,
    /// The unit measurement after this frame.
    unit: UnitTrack,
    /// v3 (`regions` only): the validated fur mask (r, g = frac uv, b = uv valid, a = fur weight).
    mask: Vec<[f32; 4]>,
    /// v7: the second sphere's own pixels (`Opts::second`).
    front: Vec<bool>,
    /// v7: the frame's linear depth (normalised by the far plane).
    lin: Vec<f32>,
    /// v7: the coat statistics the GPU computed, as `status_shells=` reads them back (a few frames late).
    stats: Vec<DrawStats>,
    /// v8: the arm's own pixels (`Opts::arm`).
    arm: Vec<bool>,
    /// v9: the island part of `status_shells=` after this frame.
    isl: String,
    /// v11: the skin target (t25: proven fur skin 0..1; empty when it was not bound).
    skin: Vec<f32>,
    /// v11: the output with the shells but WITHOUT the skin gate (t25 unbound: the v10 old fur look on the whole mask).
    v10: Vec<[f32; 3]>,
    /// v11: the output without shells and with the fur strength 0 (no old look at all).
    plain: Vec<[f32; 3]>,
}

/// Renders the scene through the real pipeline and the real light-adjust substitute. Holds the global test lock (the shell, fur mask
/// and vertex-signature registries are process globals).
fn run(cfg: &ShellCfg, o: &Opts) -> Option<Out> {
    run_seq(cfg, o, &[[0.0, 0.0]]).map(|mut v| v.remove(0))
}

/// `run` for several consecutive frames on ONE device (the shell state, its unit measurement and the fur mask persist like in the
/// game); frame k moves the camera by `jitter[k]` pixels (x right, y down: the character's clip position is offset).
fn run_seq(cfg: &ShellCfg, o: &Opts, jitter: &[[f32; 2]]) -> Option<Vec<Out>> {
    let g = gpu()?;
    let (w, h) = (o.w, o.h);
    unsafe {
        reset_state();
        ensure_ps_blob();
        configure(cfg);
        crate::furisland::MODE.store(match o.islands {
            0 => 1,
            1 => 0,
            m => m,
        }, Relaxed);
        crate::furmask::set_uv_pref("auto");
        frame_begin();
        // Targets: the scene colour (float16), the linear depth (R32, normalised by the far plane), the engine-like depth buffer.
        let color = tex(&g, w, h, DXGI_FORMAT_R16G16B16A16_FLOAT, (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32, D3D11_USAGE_DEFAULT, 0, None);
        let lin = tex(&g, w, h, DXGI_FORMAT_R32_FLOAT, (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32, D3D11_USAGE_DEFAULT, 0, None);
        let depth = tex(&g, w, h, DXGI_FORMAT_R24G8_TYPELESS, (D3D11_BIND_DEPTH_STENCIL.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32, D3D11_USAGE_DEFAULT, 0, None);
        let (mut rtv, mut lrtv, mut dsv, mut csrv) = (None, None, None, None);
        g.dev.CreateRenderTargetView(&color, None, Some(&mut rtv)).unwrap();
        g.dev.CreateRenderTargetView(&lin, None, Some(&mut lrtv)).unwrap();
        g.dev.CreateShaderResourceView(&color, None, Some(&mut csrv)).unwrap();
        let dvd = D3D11_DEPTH_STENCIL_VIEW_DESC { Format: DXGI_FORMAT_D24_UNORM_S8_UINT, ViewDimension: D3D11_DSV_DIMENSION_TEXTURE2D, Flags: 0, ..Default::default() };
        g.dev.CreateDepthStencilView(&depth, Some(&dvd), Some(&mut dsv)).unwrap();
        let (rtv, lrtv, dsv) = (rtv.unwrap(), lrtv.unwrap(), dsv.unwrap());
        let c = &g.ctx;
        let dd = D3D11_DEPTH_STENCIL_DESC { DepthEnable: true.into(), DepthWriteMask: D3D11_DEPTH_WRITE_MASK_ALL, DepthFunc: D3D11_COMPARISON_LESS_EQUAL, ..Default::default() };
        let mut dss = None;
        g.dev.CreateDepthStencilState(&dd, Some(&mut dss)).unwrap();
        let rd = D3D11_RASTERIZER_DESC { FillMode: D3D11_FILL_SOLID, CullMode: D3D11_CULL_BACK, FrontCounterClockwise: false.into(), DepthClipEnable: true.into(), ..Default::default() };
        let mut rs = None;
        g.dev.CreateRasterizerState(&rd, Some(&mut rs)).unwrap();
        // v8: the character draws' rasterizer (the engine's depth bias; a card is seen from both sides: no culling).
        let rdc = D3D11_RASTERIZER_DESC { DepthBias: o.bias.map_or(0, |b| b.0), SlopeScaledDepthBias: o.bias.map_or(0.0, |b| b.1), CullMode: if o.card.is_some() { D3D11_CULL_NONE } else { D3D11_CULL_BACK }, ..rd };
        let mut rs_char = None;
        g.dev.CreateRasterizerState(&rdc, Some(&mut rs_char)).unwrap();
        let (mut vs_full, mut ps_sky) = (None, None);
        g.dev.CreateVertexShader(&compile(VS_FULL, "vs_5_0"), None, Some(&mut vs_full)).unwrap();
        g.dev.CreatePixelShader(&compile(PS_SKY, "ps_5_0"), None, Some(&mut ps_sky)).unwrap();
        let sky_cb = buffer(&g, &floats(&[o.sky[0], o.sky[1], o.sky[2], 0.0]), D3D11_BIND_CONSTANT_BUFFER);
        // The character (the fur draw): the vertex shader's signature is known to the mod like the CreateVertexShader hook records it.
        let vsb = compile(VS_CHAR, "vs_5_0");
        let (mut vs, mut ps) = (None, None);
        g.dev.CreateVertexShader(&vsb, None, Some(&mut vs)).unwrap();
        g.dev.CreatePixelShader(&compile(PS_CHAR, "ps_5_0"), None, Some(&mut ps)).unwrap();
        let vs = vs.unwrap();
        crate::furmask::note_vs(vs.as_raw() as usize, &vsb);
        let (mut verts, mut idx) = match o.card {
            Some(cs) => card(cs, o.card_n.unwrap_or(32)),
            None if o.island_mesh => island_mesh(),
            None => sphere(48, 96),
        };
        if let Some(a) = o.arm {
            // v8: the arm, in the body's object space (world = (0, 0, dist) + p x 0.5), appended to the same buffers: ONE draw.
            let (av, ai) = sphere(16, 32);
            let base = (verts.len() / 8) as u32;
            for v in av.chunks_exact(8) {
                let p = [(a[0] + v[0] * a[3]) / 0.5, (a[1] + v[1] * a[3]) / 0.5, (a[2] - o.dist + v[2] * a[3]) / 0.5];
                // v13: with the island mesh the arm is coat too (its uv on the atlas' coat half, like the body sphere's)
                let u = if o.island_mesh { v[6] * 0.5 } else { v[6] };
                verts.extend_from_slice(&[p[0], p[1], p[2], v[3], v[4], v[5], u, v[7]]);
            }
            idx.extend(ai.iter().map(|i| i + base));
        }
        // v15: the ring: other data before the mesh (garbage far away: a wrong window would draw it)
        let (ring_v, ring_i) = if o.ring { (6000u32, 9000u32) } else { (0, 0) };
        if o.ring {
            let mut rv: Vec<f32> = (0..ring_v as usize * 8).map(|k| if k % 8 < 3 { 50.0 + (k % 7) as f32 } else { 0.5 }).collect();
            rv.extend_from_slice(&verts);
            verts = rv;
            let mut ri: Vec<u32> = (0..ring_i).map(|k| k % ring_v).collect();
            ri.extend_from_slice(&idx);
            idx = ri;
        }
        let vb = if o.dynamic_vb || o.stale != 0.0 || o.ring {
            let dynamic = o.dynamic_vb || o.ring;
            let usage = if dynamic { D3D11_USAGE_DYNAMIC } else { D3D11_USAGE_DEFAULT };
            let cpu = if dynamic { D3D11_CPU_ACCESS_WRITE.0 as u32 } else { 0 };
            let bytes = floats(&verts);
            let d = D3D11_BUFFER_DESC { ByteWidth: bytes.len() as u32, Usage: usage, BindFlags: D3D11_BIND_VERTEX_BUFFER.0 as u32, CPUAccessFlags: cpu, ..Default::default() };
            let sd = D3D11_SUBRESOURCE_DATA { pSysMem: bytes.as_ptr() as *const c_void, SysMemPitch: 0, SysMemSlicePitch: 0 };
            let mut b = None;
            g.dev.CreateBuffer(&d, Some(&sd), Some(&mut b)).unwrap();
            b.unwrap()
        } else {
            buffer(&g, &floats(&verts), D3D11_BIND_VERTEX_BUFFER)
        };
        let ib = if o.ring {
            let bytes: Vec<u8> = idx.iter().flat_map(|i| i.to_le_bytes()).collect();
            let d = D3D11_BUFFER_DESC { ByteWidth: bytes.len() as u32, Usage: D3D11_USAGE_DYNAMIC, BindFlags: D3D11_BIND_INDEX_BUFFER.0 as u32, CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32, ..Default::default() };
            let sd = D3D11_SUBRESOURCE_DATA { pSysMem: bytes.as_ptr() as *const c_void, SysMemPitch: 0, SysMemSlicePitch: 0 };
            let mut b = None;
            g.dev.CreateBuffer(&d, Some(&sd), Some(&mut b)).unwrap();
            b.unwrap()
        } else {
            buffer(&g, &idx.iter().flat_map(|i| i.to_le_bytes()).collect::<Vec<u8>>(), D3D11_BIND_INDEX_BUFFER)
        };
        // v7: the second fur sphere ("another character"), its own mesh (another index count).
        let (verts2, idx2) = sphere(24, 48);
        let vb2 = buffer(&g, &floats(&verts2), D3D11_BIND_VERTEX_BUFFER);
        let ib2 = buffer(&g, &idx2.iter().flat_map(|i| i.to_le_bytes()).collect::<Vec<u8>>(), D3D11_BIND_INDEX_BUFFER);
        let n2 = idx2.len() as u32;
        let elems = [
            D3D11_INPUT_ELEMENT_DESC { SemanticName: windows::core::s!("POSITION"), SemanticIndex: 0, Format: DXGI_FORMAT_R32G32B32_FLOAT, InputSlot: 0, AlignedByteOffset: 0, InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA, InstanceDataStepRate: 0 },
            D3D11_INPUT_ELEMENT_DESC { SemanticName: windows::core::s!("NORMAL"), SemanticIndex: 0, Format: DXGI_FORMAT_R32G32B32_FLOAT, InputSlot: 0, AlignedByteOffset: 12, InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA, InstanceDataStepRate: 0 },
            D3D11_INPUT_ELEMENT_DESC { SemanticName: windows::core::s!("TEXCOORD"), SemanticIndex: 0, Format: DXGI_FORMAT_R32G32_FLOAT, InputSlot: 0, AlignedByteOffset: 24, InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA, InstanceDataStepRate: 0 },
        ];
        let mut layout = None;
        g.dev.CreateInputLayout(&elems, &vsb, Some(&mut layout)).unwrap();
        let p11 = 1.0 / TAN_HALF_FOV;
        let p00 = p11 * h as f32 / w as f32;
        let sig = crate::furmask::vs_sig(vs.as_raw() as usize).expect("signature noted");
        let pick = crate::furmask::uv_pick(&sig).unwrap();
        let (key, uvb) = (sig.key(pick), crate::furmask::build_uv_ps(&sig, pick).unwrap());
        let mut m = crate::furmask::Mask::new(g.dev.as_raw(), &crate::furmask::mask_blob_for_tests()).expect("mask");
        let occ_blobs = crate::furmask::compile_occ_blobs().unwrap();
        let n = idx.len() as u32 - ring_i; // v15: the mesh's own indices (after the ring's)
        // Cloth: drawn LATER and in front (0.12 m before the sphere's front): over the lower left of the sphere.
        let (mut qv, mut qp) = (None, None);
        g.dev.CreateVertexShader(&compile(VS_QUAD, "vs_5_0"), None, Some(&mut qv)).unwrap();
        g.dev.CreatePixelShader(&compile(PS_QUAD, "ps_5_0"), None, Some(&mut qp)).unwrap();
        let zq = o.dist - 0.5 - o.cloth_gap;
        let k = 1.0 / zq; // clip rect from view-space metres
        let qcb = buffer(&g, &floats(&[-0.6 * p00 * k, -0.6 * p11 * k, 0.05 * p00 * k, -0.05 * p11 * k, zq, NEAR, FAR, 0.0]), D3D11_BIND_CONSTANT_BUFFER);
        let cull_none = D3D11_RASTERIZER_DESC { FillMode: D3D11_FILL_SOLID, CullMode: D3D11_CULL_NONE, DepthClipEnable: true.into(), ..Default::default() };
        let mut rs2 = None;
        g.dev.CreateRasterizerState(&cull_none, Some(&mut rs2)).unwrap();
        // The light-adjust substitute's own inputs, set like the engine and datapath.rs do.
        let la_ps = compile_ps_light();
        let (mut lvs, mut lps) = (None, None);
        g.dev.CreateVertexShader(&compile(VS_FULL, "vs_5_0"), None, Some(&mut lvs)).unwrap();
        g.dev.CreatePixelShader(&la_ps, None, Some(&mut lps)).unwrap();
        let out_t = tex(&g, w, h, DXGI_FORMAT_R16G16B16A16_FLOAT, D3D11_BIND_RENDER_TARGET.0 as u32, D3D11_USAGE_DEFAULT, 0, None);
        let mut ortv = None;
        g.dev.CreateRenderTargetView(&out_t, None, Some(&mut ortv)).unwrap();
        let weights: Vec<u8> = [0u16, 0, 0, 0x3c00].iter().flat_map(|v| v.to_le_bytes()).collect(); // (x, dark 0, bright 0, mid 1) in float16
        let t1 = tex(&g, 1, 1, DXGI_FORMAT_R16G16B16A16_FLOAT, D3D11_BIND_SHADER_RESOURCE.0 as u32, D3D11_USAGE_IMMUTABLE, 0, Some((&weights, 8)));
        let mut t1s = None;
        g.dev.CreateShaderResourceView(&t1, None, Some(&mut t1s)).unwrap();
        let sdesc = D3D11_SAMPLER_DESC { Filter: D3D11_FILTER_MIN_MAG_MIP_POINT, AddressU: D3D11_TEXTURE_ADDRESS_CLAMP, AddressV: D3D11_TEXTURE_ADDRESS_CLAMP, AddressW: D3D11_TEXTURE_ADDRESS_CLAMP, MaxAnisotropy: 1, ComparisonFunc: D3D11_COMPARISON_NEVER, MaxLOD: f32::MAX, ..Default::default() };
        let mut smp = None;
        g.dev.CreateSamplerState(&sdesc, Some(&mut smp)).unwrap();
        let cb0 = buffer(&g, &floats(&[0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0]), D3D11_BIND_CONSTANT_BUFFER);
        let cb6 = buffer(&g, &floats(&[1.0, 0.0, 0.0, 0.0, 0.0, 1.0, o.fur, 0.0]), D3D11_BIND_CONSTANT_BUFFER);
        let cb6_off = buffer(&g, &floats(&[1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0]), D3D11_BIND_CONSTANT_BUFFER);
        // v3: the material texture (1024 with its mips, like the engine's 4096 skin textures: the discriminator reads a fixed mip) and a
        // linear wrap sampler for the "engine's" pixel shader.
        // v7: the draw's atlas is always bound (the per-strand discriminator and the coat statistics read it): the region material, a
        // coloured coat with the v7 regions, or a plain painted coat of the fur's gloss (the v2 scenes; their picture does not use it).
        let mat_tex = if o.island_mesh {
            Some(texture_from(&g, island_level0()))
        } else if o.hair {
            Some(texture_from(&g, coat_level0([0.94, 0.91, 0.84], false).into_iter().map(|t| [t[0], t[1], t[2], 0.56]).collect()))
        } else if o.regions {
            Some(material_texture(&g, o.bumpy))
        } else if let Some(cc) = o.coat {
            Some(texture_from(&g, coat_level0(cc, true)))
        } else {
            Some(texture_from(&g, coat_level0([0.94, 0.91, 0.84], false)))
        };
        let uses_mat = o.regions || o.coat.is_some() || o.island_mesh || o.hair;
        let msd = D3D11_SAMPLER_DESC { Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR, AddressU: D3D11_TEXTURE_ADDRESS_WRAP, AddressV: D3D11_TEXTURE_ADDRESS_WRAP, AddressW: D3D11_TEXTURE_ADDRESS_WRAP, MaxAnisotropy: 1, ComparisonFunc: D3D11_COMPARISON_NEVER, MaxLOD: f32::MAX, ..Default::default() };
        let mut msmp = None;
        g.dev.CreateSamplerState(&msd, Some(&mut msmp)).unwrap();
        let read_rgb = |t: &ID3D11Texture2D| -> Vec<[f32; 3]> { read(&g, t, w, h, DXGI_FORMAT_R16G16B16A16_FLOAT, 8).chunks_exact(8).map(|p| [0, 2, 4].map(|i| f16(u16::from_le_bytes([p[i], p[i + 1]])))).collect() };
        let mut outs = Vec::with_capacity(jitter.len());
        for (fi, j) in jitter.iter().enumerate() {
            if fi > 0 {
                frame_begin(); // the frame boundary (records released, the shells of the last frame no longer bound)
            }
            c.ClearRenderTargetView(&rtv, &[0.0; 4]);
            c.ClearRenderTargetView(&lrtv, &[1.0; 4]);
            c.ClearDepthStencilView(&dsv, D3D11_CLEAR_DEPTH.0, 1.0, 0);
            c.OMSetRenderTargets(Some(&[Some(rtv.clone()), Some(lrtv.clone())]), &dsv);
            c.RSSetViewports(Some(&[D3D11_VIEWPORT { TopLeftX: 0.0, TopLeftY: 0.0, Width: w as f32, Height: h as f32, MinDepth: 0.0, MaxDepth: 1.0 }]));
            c.OMSetDepthStencilState(dss.as_ref(), 0);
            c.RSSetState(rs.as_ref());
            // The sky.
            c.PSSetConstantBuffers(2, Some(&[Some(sky_cb.clone())]));
            c.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            c.IASetInputLayout(None);
            c.VSSetShader(vs_full.as_ref(), None);
            c.PSSetShader(ps_sky.as_ref(), None);
            c.Draw(3, 0);
            // The character's vertex constant buffer: big like a skinned mesh's (16464 bytes), the parameters at the start; the camera
            // jitter as a clip-space offset (pixels -> NDC).
            let mut cbv = vec![0.0f32; 16464 / 4];
            cbv[..12].copy_from_slice(&[0.0, 0.0, o.dist, 0.5, p00, p11, NEAR, FAR, j[0] * 2.0 / w as f32, -j[1] * 2.0 / h as f32, if o.eye { 1.0 } else { 0.0 }, if uses_mat { 1.0 } else { 0.0 }]);
            cbv[12] = if o.cutout { 1.0 } else { 0.0 };
            let cb = buffer(&g, &floats(&cbv), D3D11_BIND_CONSTANT_BUFFER);
            c.RSSetState(rs_char.as_ref());
            c.IASetInputLayout(layout.as_ref());
            c.IASetVertexBuffers(0, 1, Some(&Some(vb.clone())), Some(&32), Some(&0));
            c.IASetIndexBuffer(&ib, DXGI_FORMAT_R32_UINT, 0);
            c.VSSetShader(&vs, None);
            c.PSSetShader(ps.as_ref(), None);
            c.VSSetConstantBuffers(0, Some(&[Some(cb.clone())]));
            c.PSSetConstantBuffers(0, Some(&[Some(cb.clone())]));
            if let Some(t) = &mat_tex {
                c.PSSetShaderResources(0, Some(&[Some(t.clone())]));
                c.PSSetSamplers(0, Some(&[msmp.clone()]));
            }
            if uses_mat {
                // furmask::fur_draw: this draw's discriminator constants (bound by the mask draw, taken by it).
                let d = discrim();
                m.fd_next = fd_for_draw_slots(&d, n, 1); // the scene binds t0 only (as furmask::fur_draw reads the bound slots)
            }
            // v4: the per-draw tag (furmask::fur_draw with shell_debug=2 / a probe frame).
            if let Some(k) = o.tag {
                let v = m.fd_next.get_or_insert([[0.0f32; 4]; FD_LEN]);
                v[FD_TAG] = [crate::furprobe::tag_value(k), crate::furprobe::TAG_MARK, 0.0, 0.0];
            }
            // What the Draw hook does before the engine's draw: the fur mask, then the shell record (furmask::fur_draw).
            let ctx2 = c.clone();
            m.draw(g.dev.as_raw(), c.as_raw(), true, &|| ctx2.DrawIndexed(n, ring_i, ring_v as i32), Some((&key, &uvb))).expect("mask draw");
            let (vs_hash, vcb_bytes) = if o.rigid { (0x59f8b692u32, 208u32) } else { (0xba0709f4, 16464) };
            on_fur_draw(c.as_raw(), Call::Indexed(n, ring_i, ring_v as i32), 0x1d50763f, Some(vs_hash), vcb_bytes, vs.as_raw() as usize);
            c.DrawIndexed(n, ring_i, ring_v as i32);
            if o.ring {
                // the engine rewrites both rings for its next draws (Map WRITE_DISCARD) before the replay
                for (buf, len) in [(&vb, (verts.len() * 4) as usize), (&ib, idx.len() * 4)] {
                    let mut m = D3D11_MAPPED_SUBRESOURCE::default();
                    c.Map(buf, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut m)).unwrap();
                    std::ptr::write_bytes(m.pData as *mut u8, 0x41, len);
                    c.Unmap(buf, 0);
                }
            }
            // v7: the second fur character, drawn later (in front), recorded like the first.
            if let Some(s2) = o.second {
                let mut cbv2 = cbv.clone();
                cbv2[..4].copy_from_slice(&s2);
                let cb2 = buffer(&g, &floats(&cbv2), D3D11_BIND_CONSTANT_BUFFER);
                c.IASetVertexBuffers(0, 1, Some(&Some(vb2.clone())), Some(&32), Some(&0));
                c.IASetIndexBuffer(&ib2, DXGI_FORMAT_R32_UINT, 0);
                c.VSSetConstantBuffers(0, Some(&[Some(cb2.clone())]));
                c.PSSetConstantBuffers(0, Some(&[Some(cb2.clone())]));
                if uses_mat {
                    m.fd_next = fd_for_draw_slots(&discrim(), n2, 1);
                }
                let ctx3 = c.clone();
                m.draw(g.dev.as_raw(), c.as_raw(), false, &|| ctx3.DrawIndexed(n2, 0, 0), Some((&key, &uvb))).expect("mask draw 2");
                on_fur_draw(c.as_raw(), Call::Indexed(n2, 0, 0), 0x1d50763f, Some(0xba0709f4), 16464, vs.as_raw() as usize);
                c.DrawIndexed(n2, 0, 0);
            }
            // v7: the engine rewrites the vertex buffer after the draw (the recorded buffer now holds another mesh).
            if o.stale != 0.0 {
                let mut moved = verts.clone();
                for v in moved.chunks_exact_mut(8) {
                    v[0] += o.stale / 0.5;
                }
                if o.dynamic_vb {
                    // v14: a DYNAMIC buffer is rewritten the way the engine does it (Map WRITE_DISCARD)
                    let bytes = floats(&moved);
                    let mut m = D3D11_MAPPED_SUBRESOURCE::default();
                    c.Map(&vb, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut m)).unwrap();
                    std::ptr::copy_nonoverlapping(bytes.as_ptr(), m.pData as *mut u8, bytes.len());
                    c.Unmap(&vb, 0);
                } else {
                    c.UpdateSubresource(&vb, 0, None, floats(&moved).as_ptr() as *const c_void, 0, 0);
                }
            }
            c.RSSetState(rs.as_ref());
            if o.cloth {
                if o.late_cloth {
                    c.OMSetRenderTargets(Some(&[Some(rtv.clone())]), &dsv); // colour + device depth only
                }
                c.IASetInputLayout(None);
                c.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
                c.VSSetShader(qv.as_ref(), None);
                c.PSSetShader(qp.as_ref(), None);
                c.VSSetConstantBuffers(1, Some(&[Some(qcb.clone())]));
                c.PSSetConstantBuffers(1, Some(&[Some(qcb.clone())]));
                c.RSSetState(rs2.as_ref());
                c.Draw(4, 0);
                c.RSSetState(rs.as_ref());
                c.OMSetRenderTargets(Some(&[Some(rtv.clone()), Some(lrtv.clone())]), &dsv);
            }
            // End of the opaque pass. The light-adjust draw: the mask's occlusion check, then the shells, then the substitute.
            c.OMSetRenderTargets(None, None);
            let occ = crate::furmask::Mask::validate(&mut m, g.dev.as_raw(), c.as_raw(), false, &occ_blobs);
            assert_eq!(occ, crate::furmask::Occ::Checked);
            let srv_raw = m.srv();
            // Wait for the background compile of the geometry shader (the game skips the shells until it is ready).
            for _ in 0..1500 {
                match gs_for(vs.as_raw() as usize) {
                    Err(e) if e == GS_PENDING => std::thread::sleep(std::time::Duration::from_millis(20)),
                    Err(e) => panic!("geometry shader: {e}"),
                    Ok(_) => break,
                }
            }
            let mask_srv = ID3D11ShaderResourceView::from_raw_borrowed(&srv_raw).unwrap().clone();
            let draw_light_with = |shells: bool, gate: bool, fur_on: bool| -> Vec<[f32; 3]> {
                c.OMSetRenderTargets(Some(&[ortv.clone()]), None);
                c.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
                c.IASetInputLayout(None);
                c.VSSetShader(lvs.as_ref(), None);
                c.PSSetShader(lps.as_ref(), None);
                c.PSSetShaderResources(0, Some(&[csrv.clone(), t1s.clone()]));
                c.PSSetSamplers(0, Some(&[smp.clone(), smp.clone()]));
                c.PSSetConstantBuffers(0, Some(&[Some(cb0.clone())]));
                c.PSSetConstantBuffers(6, Some(&[Some(if fur_on { cb6.clone() } else { cb6_off.clone() })]));
                c.PSSetShaderResources(20, Some(&[Some(mask_srv.clone())]));
                // v11: t25 (the skin gate) as light_adjust_inner left it, or unbound (the v10 look); restored after.
                let mut kept: [Option<ID3D11ShaderResourceView>; 1] = [None];
                c.PSGetShaderResources(25, Some(&mut kept));
                if !shells {
                    c.PSSetShaderResources(24, Some(&[None]));
                    c.PSSetConstantBuffers(13, Some(&[None]));
                }
                if !shells || !gate {
                    c.PSSetShaderResources(25, Some(&[None]));
                }
                c.Draw(3, 0);
                c.OMSetRenderTargets(None, None);
                c.PSSetShaderResources(25, Some(&kept));
                read_rgb(&out_t)
            };
            let draw_light = |shells: bool| draw_light_with(shells, true, true);
            let old = draw_light(false);
            // The shell pass (what datapath::bind_current runs right after the fur mask's check).
            c.OMSetRenderTargets(Some(&[ortv.clone()]), None);
            light_adjust_inner(c.as_raw(), Some(lin.as_raw()), FAR);
            let (replayed, micros) = LAST_FRAME_FOR_TESTS();
            let st = target_srv();
            let keys: Vec<u32> = if st.is_null() {
                vec![0; (w * h) as usize]
            } else {
                let s = ID3D11ShaderResourceView::from_raw_borrowed(&st).unwrap();
                let t: ID3D11Texture2D = s.GetResource().unwrap().cast().unwrap();
                read(&g, &t, w, h, DXGI_FORMAT_R32_FLOAT, 4).chunks_exact(4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect()
            };
            let out = draw_light(true);
            let v10 = draw_light_with(true, false, true);
            // v11: the skin gate the light-adjust draw got (t25).
            let skin: Vec<f32> = {
                let mut k: [Option<ID3D11ShaderResourceView>; 1] = [None];
                c.PSGetShaderResources(25, Some(&mut k));
                match &k[0] {
                    Some(s) => {
                        let t: ID3D11Texture2D = s.GetResource().unwrap().cast().unwrap();
                        let mut d = D3D11_TEXTURE2D_DESC::default();
                        t.GetDesc(&mut d);
                        let v: Vec<f32> = read(&g, &t, d.Width, d.Height, DXGI_FORMAT_R8_UNORM, 1).iter().map(|&b| b as f32 / 255.0).collect();
                        if d.Width == w && d.Height == h { v } else { vec![v[0]; (w * h) as usize] }
                    }
                    None => Vec::new(),
                }
            };
            let zl: Vec<f32> = read(&g, &lin, w, h, DXGI_FORMAT_R32_FLOAT, 4).chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
            let zqn = zq / FAR;
            // v8: the engine's device depth (D24: the low 24 bits), for the cloth drawn without a linear depth.
            let dev: Vec<f32> = read(&g, &depth, w, h, DXGI_FORMAT_R24G8_TYPELESS, 4).chunks_exact(4).map(|b| (u32::from_le_bytes([b[0], b[1], b[2], b[3]]) & 0xff_ffff) as f32 / 16_777_215.0).collect();
            let dq = (zq - NEAR) * FAR / (FAR - NEAR) / zq;
            let cloth: Vec<bool> = if o.late_cloth { dev.iter().map(|&d| o.cloth && (d - dq).abs() <= 2e-6).collect() } else { zl.iter().map(|&z| o.cloth && (z - zqn).abs() <= 1e-6).collect() };
            let body: Vec<bool> = zl.iter().zip(&cloth).map(|(&z, &cl)| z < 0.99 && (z - zqn).abs() > 1e-6 && !cl).collect();
            let arm: Vec<bool> = zl.iter().zip(&cloth).map(|(&z, &cl)| !cl && o.arm.is_some_and(|a| z * FAR > a[2] - a[3] - 0.01 && z * FAR < a[2] + a[3] + 0.01 && z < 0.99)).collect();
            let front: Vec<bool> = zl
                .iter()
                .zip(&cloth)
                .map(|(&z, &cl)| !cl && o.second.is_some_and(|s2| z * FAR > s2[2] - s2[3] - 0.01 && z * FAR < s2[2] + s2[3] + 0.01 && z < 0.99))
                .collect();
            let scene = read_rgb(&color);
            let plain = draw_light_with(false, false, false);
            let mask = if o.regions || o.tag.is_some() || o.coat.is_some() || o.second.is_some() || o.island_mesh || o.hair || o.cutout {
                let t: ID3D11Texture2D = mask_srv.GetResource().unwrap().cast().unwrap();
                read(&g, &t, w, h, DXGI_FORMAT_R16G16B16A16_UNORM, 8).chunks_exact(8).map(|p| [0, 2, 4, 6].map(|i| u16::from_le_bytes([p[i], p[i + 1]]) as f32 / 65535.0)).collect()
            } else {
                Vec::new()
            };
            let stats = STATS_SHOW.lock().unwrap().clone();
            // v9: the capture / atlas read-back finish on the GPU, the analysis on its thread, before the next frame (the game
            // simply takes a few frames).
            c.Flush();
            crate::furisland::wait_jobs(60_000);
            let isl = crate::furisland::status_text();
            outs.push(Out { out, old, keys, body, cloth, scene, replayed, micros, unit: unit_track(), mask, front, lin: zl, stats, arm, isl, skin, v10, plain });
        }
        // Clean the globals for the next test.
        c.PSSetShaderResources(24, Some(&[None]));
        c.PSSetShaderResources(25, Some(&[None]));
        c.PSSetConstantBuffers(13, Some(&[None]));
        frame_begin();
        reset_state();
        FORCE_TRI.store(false, Relaxed);
        configure(&ShellCfg { on: 0, ..ShellCfg::default() });
        crate::furisland::MODE.store(1, Relaxed);
        Some(outs)
    }
}

#[allow(non_snake_case)]
fn LAST_FRAME_FOR_TESTS() -> (u32, u32) {
    let c = CUR.lock().unwrap();
    c.as_ref().map_or((0, 0), |f| (f.replayed, f.micros))
}

fn compile_ps_light() -> Vec<u8> {
    shaders::compile(crate::hdr_shaders::LIGHT_ADJUST_HLSL, crate::hdr_shaders::LIGHT_ADJUST_ENTRY, "ps_5_0").unwrap()
}

fn lum(c: [f32; 3]) -> f32 {
    let l = [c[0] * c[0], c[1] * c[1], c[2] * c[2]];
    0.2126 * l[0] + 0.7152 * l[1] + 0.0722 * l[2]
}

/// Writes `px` (encoded rgb, w x h) as `<GW2G_SHELL_PNG>/<name>-v2.png` (v2 = the flicker-free, calibrated fur) when the variable is set; `zoom` > 1 crops the window
/// (x0, y0, cw, ch) and scales it up (nearest) so single strands can be seen.
fn png(name: &str, px: &[[f32; 3]], w: u32, h: u32, crop: Option<(u32, u32, u32, u32, u32)>) {
    let Ok(dir) = std::env::var("GW2G_SHELL_PNG") else { return };
    let _ = std::fs::create_dir_all(&dir);
    let (x0, y0, cw, ch, z) = crop.unwrap_or((0, 0, w, h, 1));
    let mut rgb = Vec::with_capacity((cw * ch * z * z * 3) as usize);
    for y in 0..ch * z {
        for x in 0..cw * z {
            let p = px[((y0 + y / z) * w + x0 + x / z) as usize];
            // The encoded value is gamma 2.0; the PNG is sRGB: close enough to look at (linear -> sRGB would be p^2 ^ (1/2.2)).
            for c in p {
                rgb.push(((c * c).max(0.0).powf(1.0 / 2.2).min(1.0) * 255.0 + 0.5) as u8);
            }
        }
    }
    let _ = std::fs::write(std::path::Path::new(&dir).join(format!("{name}-v2.png")), crate::debugshot::encode_png_rgb8(cw * z, ch * z, &rgb));
}

const W: u32 = 512;
const H: u32 = 384;

fn close() -> Opts {
    Opts { w: W, h: H, dist: 1.4, sky: [0.55, 0.65, 0.85], cloth: true, fur: 1.0, eye: false, regions: false, bumpy: false, tag: None, coat: None, cloth_gap: 0.12, second: None, dynamic_vb: false, stale: 0.0, bias: None, arm: None, late_cloth: false, card: None, islands: 0, island_mesh: false, hair: false, cutout: false, rigid: false, ring: false, card_n: None }
}

/// The pixels within `r` px of a body pixel (the silhouette grown), to tell "outside the base silhouette".
fn grown(body: &[bool], w: u32, h: u32, r: i32) -> Vec<bool> {
    let mut g = vec![false; body.len()];
    for y in 0..h as i32 {
        for x in 0..w as i32 {
            if !body[(y * w as i32 + x) as usize] {
                continue;
            }
            for dy in -r..=r {
                for dx in -r..=r {
                    let (xx, yy) = (x + dx, y + dy);
                    if xx >= 0 && yy >= 0 && xx < w as i32 && yy < h as i32 {
                        g[(yy * w as i32 + xx) as usize] = true;
                    }
                }
            }
        }
    }
    g
}

#[test]
fn shells_stick_out_of_the_silhouette_darker_at_the_roots_and_hidden_behind_the_cloth() {
    let _g = crate::testlock::lock();
    let Some(r) = run(&ShellCfg::default(), &close()) else { return };
    let n = (W * H) as usize;
    println!("replayed {} draws, CPU {:.3} ms", r.replayed, r.micros as f32 / 1000.0);
    assert_eq!(r.replayed, 1, "the sphere drew its shells");
    png("shells-close", &r.out, W, H, None);
    png("shells-close-old", &r.old, W, H, None);
    png("shells-close-zoom", &r.out, W, H, Some((330, 40, 100, 75, 4)));
    png("shells-close-old-zoom", &r.old, W, H, Some((330, 40, 100, 75, 4)));
    // No NaN, nothing negative.
    for p in r.out.iter().flatten() {
        assert!(p.is_finite() && *p >= 0.0, "{p}");
    }
    let keys: Vec<Option<(f32, f32, [f32; 2])>> = r.keys.iter().map(|&k| unpack_key(k)).collect();
    // 1. Strands outside the base silhouette (beyond the body, over the sky), and they change the picture there.
    let outside: Vec<usize> = (0..n).filter(|&i| !r.body[i] && !r.cloth[i] && keys[i].is_some()).collect();
    let changed_out = outside.iter().filter(|&&i| (0..3).any(|k| (r.out[i][k] - r.old[i][k]).abs() > 0.02)).count();
    println!("strand pixels outside the silhouette: {} ({} visibly changed)", outside.len(), changed_out);
    assert!(outside.len() > 500 && changed_out > 300, "the fur sticks out of the silhouette");
    let reach = grown(&r.body, W, H, (MAX_PX + 2.0) as i32);
    assert!(outside.iter().all(|&i| reach[i]), "never farther than MAX_PX from the body");
    // 2. Roots darker than tips: the strand colour relative to the body colour at the root, by height.
    let mut lo = (0.0f64, 0u32);
    let mut hi = (0.0f64, 0u32);
    for i in 0..n {
        let Some((hh, a, d)) = keys[i] else { continue };
        if hh <= 0.0 || !r.body[i] || a < 0.5 {
            continue; // the solid part of the strands (a soft edge mixes with what is under it)
        }
        let (x, y) = ((i as u32 % W) as f32 + 0.5 + d[0], (i as u32 / W) as f32 + 0.5 + d[1]);
        let a = (y as u32).min(H - 1) * W + (x as u32).min(W - 1);
        let base = lum(r.scene[a as usize]);
        if base < 0.05 {
            continue;
        }
        let ratio = (lum(r.out[i]) / base) as f64;
        if hh < 0.25 {
            lo = (lo.0 + ratio, lo.1 + 1);
        } else if hh > 0.5 {
            hi = (hi.0 + ratio, hi.1 + 1);
        }
    }
    let (rl, rh) = (lo.0 / lo.1.max(1) as f64, hi.0 / hi.1.max(1) as f64);
    println!("strand / root colour: roots {rl:.3} ({} px), tips {rh:.3} ({} px)", lo.1, hi.1);
    assert!(lo.1 > 100 && hi.1 > 100 && rl < rh - 0.1, "roots darker than tips");
    // 3. Hidden behind the cloth: no strand on a cloth pixel (the cloth is in front of every root).
    let on_cloth = (0..n).filter(|&i| r.cloth[i] && keys[i].is_some()).count();
    let cloth_px = r.cloth.iter().filter(|&&c| c).count();
    println!("cloth pixels {cloth_px}, with strands {on_cloth}");
    assert!(cloth_px > 1000 && on_cloth == 0, "the shells are hidden behind the cloth");
    for i in 0..n {
        if r.cloth[i] {
            assert_eq!(r.out[i], r.old[i], "the cloth is untouched");
        }
    }
    // 4. The body is covered by the base layer + strands; the mean brightness of the fur stays close to the body's.
    let body_px: Vec<usize> = (0..n).filter(|&i| r.body[i]).collect();
    let with = body_px.iter().filter(|&&i| keys[i].is_some()).count();
    assert!(with as f64 > 0.95 * body_px.len() as f64, "{with} of {}", body_px.len());
    // The reference is the body's own colour (the engine's, before any fur effect): the shells replace the old fur look on their pixels.
    let (ms, mn, mo) = body_px.iter().fold((0.0f64, 0.0f64, 0.0f64), |a, &i| (a.0 + lum(r.scene[i]) as f64, a.1 + lum(r.out[i]) as f64, a.2 + lum(r.old[i]) as f64));
    println!("mean body luminance: the body {:.4}, the old fur look {:.4}, shells {:.4} ({:.3} of the body)", ms / body_px.len() as f64, mo / body_px.len() as f64, mn / body_px.len() as f64, mn / ms);
    assert!(mn / ms > 0.95 && mn / ms < 1.05, "the fur keeps the brightness of the body (within 5 %)");
    // 5. Bounded cost: one replayed draw, at most MAX_SHELLS instances.
    assert!(r.micros < 2_000_000);
}

#[test]
fn shells_at_mid_distance_and_the_debug_view() {
    let _g = crate::testlock::lock();
    let o = Opts { dist: 4.0, ..close() };
    let Some(r) = run(&ShellCfg::default(), &o) else { return };
    png("shells-mid", &r.out, W, H, None);
    png("shells-mid-old", &r.old, W, H, None);
    png("shells-mid-zoom", &r.out, W, H, Some((W / 2 - 80, H / 2 - 70, 80, 60, 5)));
    let outside = (0..(W * H) as usize).filter(|&i| !r.body[i] && !r.cloth[i] && r.keys[i] != 0).count();
    println!("mid distance: strands outside {outside}");
    assert!(outside > 100);
    let Some(d) = run(&ShellCfg { debug: 1, ..ShellCfg::default() }, &close()) else { return };
    png("shells-debug", &d.out, W, H, None);
    // The debug view paints the shell pixels (red .. yellow: red channel high, blue low).
    let painted = (0..(W * H) as usize).filter(|&i| d.keys[i] != 0 && d.out[i][0] > 0.7 && d.out[i][2] < 0.3).count();
    assert!(painted > 1000);
    // Beyond the LOD distance: nothing.
    let Some(far) = run(&ShellCfg { lod: 2.0, ..ShellCfg::default() }, &o) else { return };
    // v11: compared without the skin gate (`v10`): beyond the LOD the skin pass still proves the coat, the old look applies as gated.
    assert!(far.keys.iter().all(|&k| k == 0) && far.v10 == far.old, "no shells beyond the LOD distance");
}

#[test]
fn zero_shells_off_or_unbound_is_the_old_output_bit_for_bit_and_the_output_is_stable() {
    let _g = crate::testlock::lock();
    let Some(a) = run(&ShellCfg::default(), &close()) else { return };
    let Some(b) = run(&ShellCfg::default(), &close()) else { return };
    assert!(a.out == b.out && a.keys == b.keys, "identical input -> identical output (MAX blend: no draw-order dependence)");
    for cfg in [ShellCfg { shells: 0, ..ShellCfg::default() }, ShellCfg { on: 0, ..ShellCfg::default() }] {
        let Some(r) = run(&cfg, &close()) else { return };
        assert!(r.keys.iter().all(|&k| k == 0));
        assert!(r.out == r.old, "{cfg:?}: the old output, bit for bit");
        assert!(r.out == a.old, "and the same as the old output of the shell run");
    }
    // fur strength 0: the composite does nothing even with the shells bound.
    let Some(z) = run(&ShellCfg::default(), &Opts { fur: 0.0, ..close() }) else { return };
    assert!(z.out == z.old);
}

#[test]
fn no_back_light_in_the_dark() {
    let _g = crate::testlock::lock();
    let Some(r) = run(&ShellCfg { rim: 2.0, ..ShellCfg::default() }, &Opts { sky: [0.0, 0.0, 0.0], cloth: false, ..close() }) else { return };
    png("shells-dark", &r.out, W, H, None);
    // Over a black background a strand is never brighter than the body at its root: nothing gets easier to see in the dark.
    let n = (W * H) as usize;
    let mut checked = 0;
    for i in 0..n {
        let Some((_, _, d)) = unpack_key(r.keys[i]) else { continue };
        if r.body[i] {
            continue;
        }
        // The root colour is read bilinearly at the quarter-pixel anchor: never brighter than the brightest of its 2 x 2 texels.
        let (x, y) = ((i as u32 % W) as f32 + d[0], (i as u32 / W) as f32 + d[1]);
        let mut top = 0.0f32;
        for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
            let (xx, yy) = ((x.floor() as i32 + dx).clamp(0, W as i32 - 1) as u32, (y.floor() as i32 + dy).clamp(0, H as i32 - 1) as u32);
            top = top.max(lum(r.scene[(yy * W + xx) as usize]));
        }
        assert!(lum(r.out[i]) <= top * 1.001 + 1e-4, "pixel {i}");
        checked += 1;
    }
    assert!(checked > 300);
}

/// The light-adjust shader with only one of t24 / b13 bound (a failed or half-bound frame) is the old output.
#[test]
fn half_bound_inputs_are_the_old_output() {
    let _g = crate::testlock::lock();
    let Some(g) = gpu() else { return };
    unsafe {
        let ps = compile_ps_light();
        let w = 32u32;
        let mut vs = None;
        g.dev.CreateVertexShader(&compile(VS_FULL, "vs_5_0"), None, Some(&mut vs)).unwrap();
        let mut p = None;
        g.dev.CreatePixelShader(&ps, None, Some(&mut p)).unwrap();
        let scene: Vec<u8> = (0..w * w).flat_map(|i| [0.6f32, 0.5, 0.4, 0.5].map(|v| v * (0.5 + (i % 7) as f32 / 14.0)).into_iter().flat_map(|v| v.to_le_bytes())).collect();
        let t0 = tex(&g, w, w, DXGI_FORMAT_R32G32B32A32_FLOAT, D3D11_BIND_SHADER_RESOURCE.0 as u32, D3D11_USAGE_IMMUTABLE, 0, Some((&scene, w * 16)));
        let keys: Vec<u8> = (0..w * w).flat_map(|i| pack_key((i % 5) as f32 / 4.0, 1.0, [1.0, -2.0]).to_le_bytes()).collect();
        let t24 = tex(&g, w, w, DXGI_FORMAT_R32_FLOAT, D3D11_BIND_SHADER_RESOURCE.0 as u32, D3D11_USAGE_IMMUTABLE, 0, Some((&keys, w * 4)));
        let mask: Vec<u8> = (0..w * w).flat_map(|_| [0u16, 0, 0, 65535].into_iter().flat_map(|v| v.to_le_bytes())).collect();
        let t20 = tex(&g, w, w, DXGI_FORMAT_R16G16B16A16_UNORM, D3D11_BIND_SHADER_RESOURCE.0 as u32, D3D11_USAGE_IMMUTABLE, 0, Some((&mask, w * 8)));
        let weights: Vec<u8> = [0u16, 0, 0, 0x3c00].iter().flat_map(|v| v.to_le_bytes()).collect();
        let t1 = tex(&g, 1, 1, DXGI_FORMAT_R16G16B16A16_FLOAT, D3D11_BIND_SHADER_RESOURCE.0 as u32, D3D11_USAGE_IMMUTABLE, 0, Some((&weights, 8)));
        let (mut s0, mut s1, mut s24, mut s20) = (None, None, None, None);
        g.dev.CreateShaderResourceView(&t0, None, Some(&mut s0)).unwrap();
        g.dev.CreateShaderResourceView(&t1, None, Some(&mut s1)).unwrap();
        let cb0 = buffer(&g, &floats(&[0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0]), D3D11_BIND_CONSTANT_BUFFER);
        g.dev.CreateShaderResourceView(&t24, None, Some(&mut s24)).unwrap();
        g.dev.CreateShaderResourceView(&t20, None, Some(&mut s20)).unwrap();
        let cb6 = buffer(&g, &floats(&[1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 1.0, 0.0]), D3D11_BIND_CONSTANT_BUFFER);
        let cb13 = buffer(&g, &floats(&[1.0, 0.65, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0]), D3D11_BIND_CONSTANT_BUFFER);
        let out_t = tex(&g, w, w, DXGI_FORMAT_R16G16B16A16_FLOAT, D3D11_BIND_RENDER_TARGET.0 as u32, D3D11_USAGE_DEFAULT, 0, None);
        let mut rtv = None;
        g.dev.CreateRenderTargetView(&out_t, None, Some(&mut rtv)).unwrap();
        let c = &g.ctx;
        let draw = |t24b: bool, b13b: bool| -> Vec<u8> {
            c.OMSetRenderTargets(Some(&[rtv.clone()]), None);
            c.RSSetViewports(Some(&[D3D11_VIEWPORT { TopLeftX: 0.0, TopLeftY: 0.0, Width: w as f32, Height: w as f32, MinDepth: 0.0, MaxDepth: 1.0 }]));
            c.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            c.VSSetShader(vs.as_ref(), None);
            c.PSSetShader(p.as_ref(), None);
            c.PSSetShaderResources(0, Some(&[s0.clone(), s1.clone()]));
            c.PSSetConstantBuffers(0, Some(&[Some(cb0.clone())]));
            c.PSSetShaderResources(20, Some(&[s20.clone()]));
            c.PSSetShaderResources(24, Some(&[if t24b { s24.clone() } else { None }]));
            c.PSSetConstantBuffers(6, Some(&[Some(cb6.clone())]));
            c.PSSetConstantBuffers(13, Some(&[if b13b { Some(cb13.clone()) } else { None }]));
            c.Draw(3, 0);
            c.OMSetRenderTargets(None, None);
            read(&g, &out_t, w, w, DXGI_FORMAT_R16G16B16A16_FLOAT, 8)
        };
        let old = draw(false, false);
        assert!(old.chunks_exact(8).any(|p| p[0..6] != [0u8; 6]), "the scene is not black");
        assert!(draw(true, false) == old, "t24 bound, b13 unbound (zero): the old output");
        assert!(draw(false, true) == old, "b13 on, t24 unbound: the old output");
        assert!(draw(true, true) != old, "both bound: the shells show");
        c.PSSetShaderResources(24, Some(&[None]));
        c.PSSetConstantBuffers(13, Some(&[None]));
    }
}

/// The replay restores the engine's pipeline state (the light-adjust draw follows it).
#[test]
fn the_replay_restores_the_pipeline_state() {
    let _g = crate::testlock::lock();
    let Some(g) = gpu() else { return };
    unsafe {
        reset_state();
        ensure_ps_blob();
        configure(&ShellCfg::default());
        frame_begin();
        let c = &g.ctx;
        // An engine state that the replay must hand back: a VS / PS, a render target, viewport, VS cb0, an index buffer, t0, b0.
        let (mut vs, mut ps) = (None, None);
        g.dev.CreateVertexShader(&compile(VS_FULL, "vs_5_0"), None, Some(&mut vs)).unwrap();
        g.dev.CreatePixelShader(&compile(PS_SKY, "ps_5_0"), None, Some(&mut ps)).unwrap();
        let rt = tex(&g, 16, 16, DXGI_FORMAT_R16G16B16A16_FLOAT, D3D11_BIND_RENDER_TARGET.0 as u32, D3D11_USAGE_DEFAULT, 0, None);
        let mut rtv = None;
        g.dev.CreateRenderTargetView(&rt, None, Some(&mut rtv)).unwrap();
        let cb = buffer(&g, &floats(&[1.0; 4]), D3D11_BIND_CONSTANT_BUFFER);
        let ib = buffer(&g, &[0u8; 12], D3D11_BIND_INDEX_BUFFER);
        c.VSSetShader(vs.as_ref(), None);
        c.PSSetShader(ps.as_ref(), None);
        c.OMSetRenderTargets(Some(&[rtv.clone()]), None);
        c.RSSetViewports(Some(&[D3D11_VIEWPORT { TopLeftX: 1.0, TopLeftY: 2.0, Width: 13.0, Height: 11.0, MinDepth: 0.0, MaxDepth: 1.0 }]));
        c.VSSetConstantBuffers(0, Some(&[Some(cb.clone())]));
        c.PSSetConstantBuffers(0, Some(&[Some(cb.clone())]));
        c.IASetIndexBuffer(&ib, DXGI_FORMAT_R16_UINT, 4);
        // A recorded draw (the same state, a full-screen triangle; whether it gets a geometry shader does not matter here).
        on_fur_draw(c.as_raw(), Call::Draw(3, 0), 1, Some(2), 16464, vs.as_ref().unwrap().as_raw() as usize);
        assert_eq!(recorded(), 1);
        let lin = tex(&g, 16, 16, DXGI_FORMAT_R32_FLOAT, (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32, D3D11_USAGE_DEFAULT, 0, None);
        light_adjust_inner(c.as_raw(), Some(lin.as_raw()), 1000.0);
        assert_eq!(recorded(), 0, "the records are released after the replay");
        let mut v = None;
        c.VSGetShader(&mut v, None, None);
        assert_eq!(v.map(|x| x.as_raw()), vs.as_ref().map(|x| x.as_raw()));
        let mut p = None;
        c.PSGetShader(&mut p, None, None);
        assert_eq!(p.map(|x| x.as_raw()), ps.as_ref().map(|x| x.as_raw()));
        let mut rts = [None];
        c.OMGetRenderTargets(Some(&mut rts), None);
        assert_eq!(rts[0].as_ref().map(|x| x.as_raw()), rtv.as_ref().map(|x| x.as_raw()));
        let mut n = 1u32;
        let mut vp = [D3D11_VIEWPORT::default()];
        c.RSGetViewports(&mut n, Some(vp.as_mut_ptr()));
        assert_eq!((vp[0].TopLeftX, vp[0].Width, vp[0].Height), (1.0, 13.0, 11.0));
        let mut cbs = [None];
        c.VSGetConstantBuffers(0, Some(&mut cbs));
        assert_eq!(cbs[0].as_ref().map(|x| x.as_raw()), Some(cb.as_raw()));
        let (mut ib2, mut f, mut off) = (None, DXGI_FORMAT_UNKNOWN, 0u32);
        c.IAGetIndexBuffer(Some(&mut ib2), Some(&mut f), Some(&mut off));
        assert_eq!((ib2.map(|x| x.as_raw()), f, off), (Some(ib.as_raw()), DXGI_FORMAT_R16_UINT, 4));
        let mut gs = None;
        c.GSGetShader(&mut gs, None, None);
        assert!(gs.is_none(), "no geometry shader left bound");
        c.PSSetShaderResources(24, Some(&[None]));
        c.PSSetConstantBuffers(13, Some(&[None]));
        frame_begin();
        reset_state();
        configure(&ShellCfg { on: 0, ..ShellCfg::default() });
    }
}

#[test]
fn the_vertex_shader_dump_disassembles_character_layouts_only() {
    let _g = crate::testlock::lock();
    let vsb = compile(VS_CHAR, "vs_5_0");
    crate::furmask::note_vs(0x5150, &vsb);
    let code = crate::furmask::vs_code(0x5150).expect("a character-like layout keeps its bytecode");
    let dis = shaders::disassemble(&code).unwrap();
    assert!(dis.contains("vs_5_0") && dis.contains("SV_POSITION") && dis.contains("TEXCOORD"), "{dis}");
    // A plain full-screen vertex shader is not kept.
    crate::furmask::note_vs(0x5151, &compile(VS_FULL, "vs_5_0"));
    assert!(crate::furmask::vs_code(0x5151).is_none());
    assert!(shaders::disassemble(b"DXBCgarbage").is_err());
}

#[test]
fn shell_keys_apply_live_and_the_debug_capture_reports_them() {
    let _g = crate::testlock::lock();
    let old = crate::config::Config::default();
    let new = crate::config::Config::parse("fur_shells=12\nshell_length=3\nshell_counts=-5214");
    let d = crate::debugshot::diff(&old, &new);
    assert_eq!(d.live, vec!["fur_shells", "shell_length", "shell_counts"]);
    let (m, _) = crate::debugshot::merge(&old, &old, &new);
    crate::debugshot::apply_live(&d.live, &m);
    let c = cfg();
    assert_eq!((c.shells, c.length, c.counts.as_str()), (12, 3.0, "-5214"));
    assert!(crate::ao::depth_consumers() & crate::ao::DEPTH_FOR_FUR != 0, "the shells read the linear depth");
    let t = crate::debugshot::settings_text("test", 7, (1920, 1080), 10);
    for k in ["fur_shell=1\n", "fur_shells=12\n", "shell_length=3\n", "shell_counts=-5214\n", "shell_normal=face\n", "status_shells="] {
        assert!(t.contains(k), "{k} missing in\n{t}");
    }
    configure(&ShellCfg { on: 0, ..ShellCfg::default() });
    assert!(crate::ao::depth_consumers() & crate::ao::DEPTH_FOR_FUR == 0);
    let t = crate::debugshot::settings_text("test", 7, (1920, 1080), 10);
    assert!(t.lines().any(|l| l.starts_with("status_shells=off")), "{t}");
}

#[test]
fn status_lists_the_draws_and_the_verdicts() {
    let _g = crate::testlock::lock();
    configure(&ShellCfg::default());
    ensure_ps_blob();
    frame_begin();
    with_cur(|f| {
        f.draws.push(DrawInfo { vs: Some(0xba0709f4), ps: 0x1d50763f, vcb0: 16464, count: 17694, instances: 1, srvs: 2, verdict: None, shells: true });
        f.draws.push(DrawInfo { vs: Some(0x59f8b692), ps: 0x1d50763f, vcb0: 208, count: 5214, instances: 1, srvs: 2, verdict: Some("vcb < shell_min_vcb"), shells: false });
        f.replayed = 1;
    });
    frame_begin();
    let t = status_text();
    for k in ["16 shells", "length 2.5 cm","1 draws drew shells (1 picked of 2 fur draws)", "[vs ba0709f4 ps 1d50763f vcb 16464 count 17694 inst 1 srv 2 -> shells]", "[vs 59f8b692 ps 1d50763f vcb 208 count 5214 inst 1 srv 2 -> vcb < shell_min_vcb]", "shell_ps [= fur_ps]"] {
        assert!(t.contains(k), "{k} missing in {t}");
    }
    configure(&ShellCfg { on: 0, ..ShellCfg::default() });
    assert!(status_text().starts_with("off"));
    frame_begin();
    frame_begin();
}

// ---- temporal stability (the flicker of the first version) -------------------------------------------------------------

#[test]
fn the_unit_tracker_publishes_once_and_holds_with_hysteresis() {
    let mut u = UnitTrack::default();
    let sum = |l: f32, n: f32| [l * n, l * l * n, n, 0.0];
    assert!(!u.feed(sum(5.3, 3.0)), "too few triangles: ignored");
    assert!(!u.feed([f32::NAN, 0.0, 100.0, 0.0]) && !u.feed([f32::INFINITY, 0.0, 100.0, 0.0]) && !u.feed([0.0; 4]));
    assert_eq!(u.published, 0.0);
    assert!(u.feed(sum(39.37f32.log2(), 500.0)), "the first good frame publishes");
    assert!((u.published - 39.37).abs() < 1e-3);
    // Noise of +-0.5 % frame to frame never moves the published unit (no breathing strands).
    for k in 0..200 {
        let l = 39.37f32.log2() + if k % 2 == 0 { 0.007 } else { -0.007 };
        assert!(!u.feed(sum(l, 400.0 + k as f32)));
    }
    assert!((u.published - 39.37).abs() < 1e-3);
    // One absurd frame (a wrong measurement) moves the running mean by at most UNIT_MAX_STEP x UNIT_EMA and does not publish.
    let before = u.mean;
    assert!(!u.feed(sum(20.0, 300.0)));
    assert!((u.mean - before).abs() <= UNIT_MAX_STEP * UNIT_EMA + 1e-6);
    // A real, lasting change (another map: another far plane) is followed within a second or two of frames, in small steps.
    let target = 50.0f32.log2();
    let mut steps = 0;
    for _ in 0..400 {
        if u.feed(sum(target, 500.0)) {
            steps += 1;
        }
    }
    assert!((u.published.log2() - target).abs() <= UNIT_HYST + 1e-4, "{}", u.published);
    assert!(steps > 3, "followed in several small steps, not one jump");
    assert!(u.spread >= 0.0 && u.frames > 400);
    assert!(unit_text(&u).starts_with("auto: 4") || unit_text(&u).starts_with("auto: 5"), "{}", unit_text(&u));
    assert!(unit_text(&UnitTrack::default()).contains("measuring"));
}

#[test]
fn the_wind_phase_is_continuous_and_never_wraps() {
    for t in [0.0, 1.0, 3599.99, 3600.0, 3600.01, 86_400.5, 1.0e7] {
        let a = wind_phases(t);
        let b = wind_phases(t + 1.0 / 120.0);
        for k in 0..4 {
            assert!(a[k].is_finite() && (a[k] - b[k]).abs() < 0.02, "t {t}: {a:?} -> {b:?}");
        }
        assert!((a[0] * a[0] + a[1] * a[1] - 1.0).abs() < 1e-5);
    }
    assert_eq!(wind_phases(f64::NAN), wind_phases(0.0));
    assert_eq!(wind_phases(-5.0), wind_phases(0.0));
}

/// Mean absolute difference (encoded luminance) and the share of pixels changing by more than `thr`, over `region`.
fn change(a: &[[f32; 3]], b: &[[f32; 3]], region: &[bool], thr: f32) -> (f64, f64) {
    let mut s = 0.0f64;
    let mut big = 0u32;
    let mut n = 0u32;
    for i in 0..a.len() {
        if !region[i] {
            continue;
        }
        let d = (lum(a[i]).sqrt() - lum(b[i]).sqrt()).abs();
        s += d as f64;
        big += (d > thr) as u32;
        n += 1;
    }
    (s / n.max(1) as f64, big as f64 / n.max(1) as f64)
}

#[test]
fn identical_frames_are_bit_identical_and_the_unit_settles_on_the_true_value() {
    let _g = crate::testlock::lock();
    let Some(f) = run_seq(&ShellCfg::default(), &close(), &[[0.0, 0.0]; 5]) else { return };
    // The scene's engine units are metres: the measured unit must be 1 per metre (within the measurement's 1 %).
    let u = f.last().unwrap().unit;
    println!("unit after 5 frames: {} ({} triangles, spread {}, {} frames)", u.published, u.tris, u.spread, u.frames);
    assert!(u.published > 0.0 && (u.published - 1.0).abs() < 0.01, "{u:?}");
    assert!(f[0].unit.published == 0.0 || f[0].unit.frames <= 1, "the first frame has no read-back yet");
    // Once the unit is published, consecutive frames with identical input are identical bit for bit (keys and picture).
    let settled: Vec<&Out> = f.iter().filter(|o| o.unit.published > 0.0).collect();
    assert!(settled.len() >= 2, "the unit was read back within a few frames");
    for w in settled.windows(2) {
        assert!(w[0].keys == w[1].keys, "identical frames: identical shell keys");
        assert!(w[0].out == w[1].out, "identical frames: identical picture");
    }
    for o in &f {
        assert_eq!(o.replayed, 1);
    }
}

#[test]
fn a_sub_pixel_camera_move_changes_the_fur_smoothly() {
    let _g = crate::testlock::lock();
    // Three frames to settle the unit, then the camera moves by a third of a pixel (then back by a fifth).
    let jit = [[0.0, 0.0], [0.0, 0.0], [0.0, 0.0], [0.0, 0.0], [0.33, 0.21], [0.13, 0.41]];
    let o = Opts { cloth: false, ..close() };
    let Some(f) = run_seq(&ShellCfg::default(), &o, &jit) else { return };
    let n = (W * H) as usize;
    let (a, b, c) = (&f[3], &f[4], &f[5]);
    let region: Vec<bool> = grown(&a.body, W, H, 24).iter().zip(grown(&b.body, W, H, 24)).map(|(x, y)| *x || y).collect();
    let base_ab = change(&a.old, &b.old, &region, 0.1);
    let fur_ab = change(&a.out, &b.out, &region, 0.1);
    let fur_bc = change(&b.out, &c.out, &region, 0.1);
    println!("sub-pixel move: the scene without shells changes {:.4} (>{:.1} % px), with shells {:.4} ({:.2} % px), next move {:.4} ({:.2} % px)", base_ab.0, base_ab.1 * 100.0, fur_ab.0, fur_ab.1 * 100.0, fur_bc.0, fur_bc.1 * 100.0);
    // Bounded: a third of a pixel moves the fur picture by a small amount on average, and only a few pixels change a lot.
    assert!(fur_ab.0 < 0.03 && fur_bc.0 < 0.03, "mean change of the fur under a sub-pixel move");
    assert!(fur_ab.1 < 0.06 && fur_bc.1 < 0.06, "share of pixels that blink");
    // Coverage of the body does not drop out (every visible triangle keeps its shells: one unit for the frame).
    for fr in [a, b, c] {
        let body: Vec<usize> = (0..n).filter(|&i| fr.body[i]).collect();
        let with = body.iter().filter(|&&i| fr.keys[i] != 0).count();
        assert!(with as f64 > 0.97 * body.len() as f64, "{with} of {}", body.len());
    }
    // The first version's per-triangle unit, for comparison (the cause measured): the same move.
    FORCE_TRI.store(true, Relaxed);
    let Some(t) = run_seq(&ShellCfg::default(), &o, &jit) else { return };
    let tri_ab = change(&t[3].out, &t[4].out, &region, 0.1);
    let lost = |fr: &Out| (0..n).filter(|&i| fr.body[i] && fr.keys[i] == 0).count();
    println!("per-triangle unit (first version): change {:.4} ({:.2} % px), body px without shells {} / {}", tri_ab.0, tri_ab.1 * 100.0, lost(&t[3]), lost(&t[4]));
    println!("frame unit: body px without shells {} / {}", lost(a), lost(b));
    // Farther (4.5 m, a bigger picture): the per-triangle guess vs the frame's unit, the same sub-pixel move.
    let o2 = Opts { dist: 4.5, w: 1024, h: 768, ..o };
    let n2 = (1024 * 768) as usize;
    FORCE_TRI.store(true, Relaxed);
    let Some(t2) = run_seq(&ShellCfg::default(), &o2, &jit) else { return }; // (run_seq clears FORCE_TRI at its end)
    let Some(f2) = run_seq(&ShellCfg::default(), &o2, &jit) else { return };
    let reg2: Vec<bool> = grown(&f2[3].body, 1024, 768, 24);
    let lost2 = |fr: &Out| (0..n2).filter(|&i| fr.body[i] && fr.keys[i] == 0).count();
    let strands2 = |fr: &Out| (0..n2).filter(|&i| fr.keys[i] != 0 && unpack_key(fr.keys[i]).is_some_and(|k| k.0 > 0.0)).count();
    let ct = change(&t2[3].out, &t2[4].out, &reg2, 0.1);
    let cf = change(&f2[3].out, &f2[4].out, &reg2, 0.1);
    println!("4.5 m: per-triangle: change {:.4} ({:.2} % px), body px without shells {} / {}, strand px {} / {}", ct.0, ct.1 * 100.0, lost2(&t2[3]), lost2(&t2[4]), strands2(&t2[3]), strands2(&t2[4]));
    println!("4.5 m: frame unit:   change {:.4} ({:.2} % px), body px without shells {} / {}, strand px {} / {}", cf.0, cf.1 * 100.0, lost2(&f2[3]), lost2(&f2[4]), strands2(&f2[3]), strands2(&f2[4]));
    assert!(cf.0 < 0.03 && cf.1 < 0.06, "bounded at 4.5 m too");
    assert!(cf.0 <= ct.0 + 1e-3 && lost2(&f2[4]) <= lost2(&t2[4]), "never worse than the per-triangle guess");
}

#[test]
fn a_second_light_adjust_draw_of_the_frame_binds_the_same_shells() {
    let _g = crate::testlock::lock();
    let Some(g) = gpu() else { return };
    unsafe {
        reset_state();
        ensure_ps_blob();
        configure(&ShellCfg::default());
        frame_begin();
        let c = &g.ctx;
        let lin = tex(&g, 16, 16, DXGI_FORMAT_R32_FLOAT, (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32, D3D11_USAGE_DEFAULT, 0, None);
        // Nothing recorded: nothing bound.
        light_adjust_inner(c.as_raw(), Some(lin.as_raw()), 1000.0);
        let mut v = [None];
        c.PSGetShaderResources(SRV_SLOT, Some(&mut v));
        assert!(v[0].is_none());
        // A frame whose shells were drawn by its first light-adjust draw: a second one binds the same target and constants; the
        // next frame binds nothing until its own shells are drawn.
        let Some(Ok(blobs)) = PS_BLOB.get() else { panic!("pixel shaders") };
        let mut st = Shell::new(g.dev.as_raw(), blobs).expect("shell objects");
        assert!(st.ensure_target(g.dev.as_raw(), 16, 16));
        st.drawn_frame = true;
        let (tsrv, tcb) = (st.target.srv, st.cb_comp);
        *STATE.lock().unwrap() = Some(st);
        light_adjust_inner(c.as_raw(), Some(lin.as_raw()), 1000.0);
        let mut v = [None];
        c.PSGetShaderResources(SRV_SLOT, Some(&mut v));
        assert_eq!(v[0].as_ref().map(|x| x.as_raw()), Some(tsrv));
        let mut b = [None];
        c.PSGetConstantBuffers(CB_SLOT, Some(&mut b));
        assert_eq!(b[0].as_ref().map(|x| x.as_raw()), Some(tcb));
        frame_begin();
        light_adjust_inner(c.as_raw(), Some(lin.as_raw()), 1000.0);
        let mut v = [None];
        c.PSGetShaderResources(SRV_SLOT, Some(&mut v));
        assert!(v[0].is_none(), "the next frame: nothing until its own shells are drawn");
        c.PSSetShaderResources(24, Some(&[None]));
        c.PSSetConstantBuffers(13, Some(&[None]));
        frame_begin();
        reset_state();
        configure(&ShellCfg { on: 0, ..ShellCfg::default() });
    }
}

// ---- the face: strands keep the features clear ---------------------------------------------------------------------------

#[test]
fn the_strands_do_not_smear_the_face_features() {
    let _g = crate::testlock::lock();
    let o = Opts { cloth: false, eye: true, ..close() };
    let Some(keep) = run(&ShellCfg::default(), &o) else { return };
    let Some(smear) = run(&ShellCfg { detail: 0.0, ..ShellCfg::default() }, &o) else { return };
    png("shells-face", &keep.out, W, H, None);
    png("shells-face-zoom", &keep.out, W, H, Some((110, 110, 100, 75, 4)));
    png("shells-face-nodetail-zoom", &smear.out, W, H, Some((110, 110, 100, 75, 4)));
    png("shells-face-old-zoom", &keep.scene, W, H, Some((110, 110, 100, 75, 4)));
    // The eye: the dark pixels of the body (the engine's colour).
    let n = (W * H) as usize;
    let eye: Vec<usize> = (0..n).filter(|&i| keep.body[i] && lum(keep.scene[i]) < 0.08).collect();
    assert!(eye.len() > 100, "the eye is in view: {} px", eye.len());
    let mean = |r: &Out| eye.iter().map(|&i| lum(r.out[i]) as f64).sum::<f64>() / eye.len() as f64;
    let base = eye.iter().map(|&i| lum(keep.scene[i]) as f64).sum::<f64>() / eye.len() as f64;
    println!("eye: the engine {:.4}, shells with detail {:.4}, without {:.4}", base, mean(&keep), mean(&smear));
    // Without the detail rule the white strands around it paint over the eye; with it the eye stays dark.
    assert!(mean(&smear) > base * 1.5, "the smear seen in the game is reproduced without the rule");
    assert!(mean(&keep) < base + 0.25 * (mean(&smear) - base), "the eye stays clear");
    // The white coat around the eye still gets its strands.
    let coat: Vec<usize> = (0..n).filter(|&i| keep.body[i] && lum(keep.scene[i]) > 0.2).collect();
    let changed = coat.iter().filter(|&&i| (0..3).any(|k| (keep.out[i][k] - keep.scene[i][k]).abs() > 0.02)).count();
    assert!(changed as f64 > 0.3 * coat.len() as f64, "{changed} of {}", coat.len());
}

// ---- v3: the fur discriminator (no fur on the horns, the nose, the claws) ---------------------------------------------------

/// The scene's non-fur regions of the mesh uv (u0, v0, u1, v1): a horn at the upper left silhouette, the nose in the middle of the face, a
/// claw at the lower right. Their albedo differs (grey horn, dark pink nose) except the claw's, which has the coat's colour: only the gloss
/// tells it apart (like an ivory claw on a light coat).
const REGIONS: [(&str, [f32; 4]); 3] = [("horn", [0.55, 0.10, 0.66, 0.30]), ("nose", [0.72, 0.44, 0.80, 0.54]), ("claw", [0.84, 0.62, 0.92, 0.80])];
/// The material texture ramps from the fur's gloss to the hard gloss over this much uv OUTSIDE each region (a painted texture is not a
/// hard step either): the discriminator's transition is soft there.
const RAMP: f32 = 0.02;

/// Signed distance of `uv` to a region's border (positive inside).
fn region_d(r: [f32; 4], u: f32, v: f32) -> f32 {
    (u - r[0]).min(r[2] - u).min((v - r[1]).min(r[3] - v))
}

/// The largest signed distance to any region (positive = inside one) and that region's index.
fn region_of(u: f32, v: f32) -> (f32, usize) {
    REGIONS.iter().enumerate().map(|(k, (_, r))| (region_d(*r, u, v), k)).fold((f32::MIN, 0), |a, b| if b.0 > a.0 { b } else { a })
}

/// The material of the coat at uv: (albedo, gloss alpha). Fur: the white coat, gloss 0.40..0.46 (v7: the 0.40..0.44 of the fur texels in
/// every real charr atlas the debug captures wrote; v3 used 0.47..0.59 from the game's dat hair textures); horn grey, nose dark pink, claw
/// the coat's colour, all glossy (0.86).
fn material(u: f32, v: f32) -> ([f32; 3], f32) {
    let n = (((u * 157.0).sin() * (v * 113.0).cos()) * 0.5 + 0.5) * 0.12;
    let mut alb = [0.88, 0.82, 0.70];
    let mut a = 0.40 + 0.5 * n;
    for (k, (_, r)) in REGIONS.iter().enumerate() {
        let d = region_d(*r, u, v);
        let t = ((d + RAMP) / RAMP).clamp(0.0, 1.0);
        let t = t * t * (3.0 - 2.0 * t);
        if t <= 0.0 {
            continue;
        }
        let hard = [[0.50, 0.48, 0.45], [0.30, 0.16, 0.16], [0.86, 0.80, 0.69]][k];
        for c in 0..3 {
            alb[c] += (hard[c] - alb[c]) * t;
        }
        a += (0.86 - a) * t;
    }
    (alb, a)
}

/// Level 0 of the material texture (1024, RGBA). `bumpy` (v4, the detail channel): r g carry a normal map's xy instead of the albedo:
/// per-texel noise (+-0.25 around 0.5, like fur strands) on the coat, flat 0.5 on the horn / nose / claw (blended along the ramp).
fn material_level0(bumpy: bool) -> Vec<[f32; 4]> {
    let size = 1024u32;
    let mut l0 = Vec::with_capacity((size * size) as usize);
    for y in 0..size {
        for x in 0..size {
            let (u, v) = ((x as f32 + 0.5) / size as f32, (y as f32 + 0.5) / size as f32);
            let (alb, a) = material(u, v);
            let mut t = [alb[0].sqrt(), alb[1].sqrt(), alb[2].sqrt(), a]; // the albedo stored encoded like a colour texture
            let tr = ((region_of(u, v).0 + RAMP) / RAMP).clamp(0.0, 1.0);
            let hash = |k: u32| {
                let mut s = x.wrapping_mul(0x9E37_79B1) ^ y.wrapping_mul(0x85EB_CA77) ^ k.wrapping_mul(0xC2B2_AE3D);
                s ^= s >> 15;
                s = s.wrapping_mul(0x2C1B_3C6D);
                s ^= s >> 12;
                (s & 0xffff) as f32 / 65535.0 - 0.5
            };
            // v5: painted strands on the coat (+-8 % brightness per texel, like the game's fur: relative detail ~0.09 at 5x5), the horn,
            // nose and claw a flat colour (the colour-flatness channel `t0.f`); faded along the ramp like the gloss.
            let paint = 1.0 + 0.16 * hash(3) * (1.0 - tr);
            for c in t.iter_mut().take(3) {
                *c *= paint;
            }
            if bumpy {
                t[0] = 0.5 + hash(1) * 0.5 * (1.0 - tr);
                t[1] = 0.5 + hash(2) * 0.5 * (1.0 - tr);
            }
            l0.push(t);
        }
    }
    l0
}

/// The material as a 1024 RGBA8 texture with its full mip chain (box filtered on the CPU), like the engine's skin textures.
fn material_texture(g: &Gpu, bumpy: bool) -> ID3D11ShaderResourceView {
    texture_from(g, material_level0(bumpy))
}

/// v7 regions of the coloured coats (u0, v0, u1, v1): a horn (a FLAT dye colour with the coat's gloss, like the real horn), a claw (the
/// undyed keratin tan, painted, glossy 0.62, like the real claws' p50), a claw pad (the same tan with the coat's gloss: only its colour
/// tells it) and the lips (red, the coat's gloss).
const V7_REGIONS: [(&str, [f32; 4]); 4] = [("horn", [0.55, 0.10, 0.66, 0.30]), ("claw", [0.84, 0.62, 0.92, 0.80]), ("pad", [0.72, 0.62, 0.80, 0.80]), ("lips", [0.72, 0.42, 0.80, 0.52])];

/// Level 0 (1024, the stored texel values) of a coat of colour `coat` (stored, like the atlas: painted +-8 % per texel, gloss
/// 0.40..0.44) with the v7 regions when `regions`.
fn coat_level0(coat: [f32; 3], regions: bool) -> Vec<[f32; 4]> {
    let size = 1024u32;
    let mut l0 = Vec::with_capacity((size * size) as usize);
    for y in 0..size {
        for x in 0..size {
            let (u, v) = ((x as f32 + 0.5) / size as f32, (y as f32 + 0.5) / size as f32);
            let hash = |k: u32| {
                let mut s = x.wrapping_mul(0x9E37_79B1) ^ y.wrapping_mul(0x85EB_CA77) ^ k.wrapping_mul(0xC2B2_AE3D);
                s ^= s >> 15;
                s = s.wrapping_mul(0x2C1B_3C6D);
                s ^= s >> 12;
                (s & 0xffff) as f32 / 65535.0 - 0.5
            };
            let paint = 1.0 + 0.16 * hash(3);
            let mut c = coat.map(|k| k * paint);
            let mut a = 0.42 + 0.04 * hash(4);
            if regions {
                if let Some(k) = V7_REGIONS.iter().position(|(_, r)| region_d(*r, u, v) > 0.0) {
                    match k {
                        0 => {
                            c = [0.50, 0.48, 0.45];
                            a = 0.41;
                        }
                        1 => {
                            c = [0.62, 0.48, 0.34].map(|k| k * paint);
                            a = 0.62;
                        }
                        2 => c = [0.62, 0.48, 0.34].map(|k| k * paint),
                        _ => c = [0.52, 0.14, 0.14].map(|k| k * paint),
                    }
                }
            }
            l0.push([c[0], c[1], c[2], a]);
        }
    }
    l0
}

/// A 1024 RGBA8 texture with its full mip chain (box filtered on the CPU) from its level 0.
fn texture_from(g: &Gpu, level0: Vec<[f32; 4]>) -> ID3D11ShaderResourceView {
    let size = 1024u32;
    let mut levels: Vec<Vec<[f32; 4]>> = Vec::new();
    levels.push(level0);
    let mut s = size;
    while s > 1 {
        let p = levels.last().unwrap();
        let n = s / 2;
        let mut q = Vec::with_capacity((n * n) as usize);
        for y in 0..n {
            for x in 0..n {
                let mut acc = [0.0f32; 4];
                for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                    let t = p[((2 * y + dy) * s + 2 * x + dx) as usize];
                    for c in 0..4 {
                        acc[c] += 0.25 * t[c];
                    }
                }
                q.push(acc);
            }
        }
        levels.push(q);
        s = n;
    }
    let bytes: Vec<Vec<u8>> = levels.iter().map(|l| l.iter().flat_map(|t| t.map(|c| (c.clamp(0.0, 1.0) * 255.0 + 0.5) as u8)).collect()).collect();
    let sd: Vec<D3D11_SUBRESOURCE_DATA> = bytes.iter().enumerate().map(|(i, b)| D3D11_SUBRESOURCE_DATA { pSysMem: b.as_ptr() as *const c_void, SysMemPitch: (size >> i).max(1) * 4, SysMemSlicePitch: 0 }).collect();
    let desc = D3D11_TEXTURE2D_DESC { Width: size, Height: size, MipLevels: sd.len() as u32, ArraySize: 1, Format: DXGI_FORMAT_R8G8B8A8_UNORM, SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, Usage: D3D11_USAGE_IMMUTABLE, BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32, CPUAccessFlags: 0, MiscFlags: 0 };
    let mut t = None;
    let mut v = None;
    unsafe {
        g.dev.CreateTexture2D(&desc, Some(sd.as_ptr()), Some(&mut t)).unwrap();
        g.dev.CreateShaderResourceView(&t.unwrap(), None, Some(&mut v)).unwrap();
    }
    v.unwrap()
}

/// `png` with the v3 suffix (`<name>-v3.png`).
fn png3(name: &str, px: &[[f32; 3]], w: u32, h: u32, crop: Option<(u32, u32, u32, u32, u32)>) {
    let Ok(dir) = std::env::var("GW2G_SHELL_PNG") else { return };
    let _ = std::fs::create_dir_all(&dir);
    let (x0, y0, cw, ch, z) = crop.unwrap_or((0, 0, w, h, 1));
    let mut rgb = Vec::with_capacity((cw * ch * z * z * 3) as usize);
    for y in 0..ch * z {
        for x in 0..cw * z {
            let p = px[((y0 + y / z) * w + x0 + x / z) as usize];
            for c in p {
                rgb.push(((c * c).max(0.0).powf(1.0 / 2.2).min(1.0) * 255.0 + 0.5) as u8);
            }
        }
    }
    let _ = std::fs::write(std::path::Path::new(&dir).join(format!("{name}-v3.png")), crate::debugshot::encode_png_rgb8(cw * z, ch * z, &rgb));
}

/// What the discriminator did in one render: strands rooted inside a region (must be none), the body pixels away from every region that
/// carry shells, and the mask's fur weight inside / outside / along the ramp.
struct RegionStats {
    roots_inside: usize,
    roots_inside_by: [usize; 3],
    fur_px: usize,
    fur_with_keys: usize,
    core_px: usize,
    core_max_a: f32,
    fur_min_a: f32,
    soft_px: usize,
    /// Mean fur weight in bins of the distance to the nearest region (-RAMP - 0.01 .. +0.01, 0.004 wide).
    bins: Vec<(f32, f32, usize)>,
}

fn region_stats(r: &Out, w: u32, h: u32) -> RegionStats {
    let n = (w * h) as usize;
    let uv_at = |i: usize| -> Option<(f32, f32, f32)> {
        let m = r.mask[i];
        (m[2] > 0.5).then_some((m[0], m[1], m[3]))
    };
    let mut s = RegionStats { roots_inside: 0, roots_inside_by: [0; 3], fur_px: 0, fur_with_keys: 0, core_px: 0, core_max_a: 0.0, fur_min_a: 1.0, soft_px: 0, bins: Vec::new() };
    for i in 0..n {
        let Some((hh, _, d)) = unpack_key(r.keys[i]) else { continue };
        if hh <= 0.0 {
            continue;
        }
        let (x, y) = ((i as u32 % w) as f32 + 0.5 + d[0], (i as u32 / w) as f32 + 0.5 + d[1]);
        if x < 0.0 || y < 0.0 || x >= w as f32 || y >= h as f32 {
            continue;
        }
        let ri = (y as u32 * w + x as u32) as usize;
        if let Some((u, v, _)) = uv_at(ri) {
            let (dd, k) = region_of(u, v);
            if dd > 0.008 {
                s.roots_inside += 1;
                s.roots_inside_by[k] += 1;
            }
        }
    }
    let nb = ((RAMP + 0.02) / 0.004).round() as usize;
    let mut bins = vec![(0.0f64, 0usize); nb];
    for i in 0..n {
        if !r.body[i] || r.cloth[i] {
            continue;
        }
        let Some((u, v, a)) = uv_at(i) else { continue };
        let (dd, _) = region_of(u, v);
        if dd < -(RAMP + 0.012) {
            s.fur_px += 1;
            s.fur_with_keys += (r.keys[i] != 0) as usize;
            s.fur_min_a = s.fur_min_a.min(a);
        } else if dd > 0.006 {
            s.core_px += 1;
            s.core_max_a = s.core_max_a.max(a);
        }
        if dd > -(RAMP + 0.01) && dd < 0.01 {
            s.soft_px += (a > 0.1 && a < 0.9) as usize;
            let b = (((dd + RAMP + 0.01) / 0.004) as usize).min(nb - 1);
            bins[b] = (bins[b].0 + a as f64, bins[b].1 + 1);
        }
    }
    s.bins = bins.iter().enumerate().map(|(k, &(sum, c))| (-(RAMP + 0.01) + (k as f32 + 0.5) * 0.004, if c > 0 { (sum / c as f64) as f32 } else { f32::NAN }, c)).collect();
    s
}

fn region_opts() -> Opts {
    Opts { cloth: true, regions: true, ..close() }
}

/// The mask's fur weight as grey (the PNG of what counts as fur); the rest of the picture darkened.
fn mask_picture(r: &Out) -> Vec<[f32; 3]> {
    r.mask.iter().zip(&r.out).map(|(m, o)| if m[2] > 0.5 { [m[3], m[3], m[3]] } else { [o[0] * 0.3, o[1] * 0.3, o[2] * 0.3] }).collect()
}

/// The screen box of a region (its body pixels), grown by `pad` px: (x0, y0, w, h).
fn region_box(r: &Out, k: usize, w: u32, h: u32, pad: u32) -> Option<(u32, u32, u32, u32)> {
    let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0, 0);
    for i in 0..(w * h) as usize {
        let m = r.mask[i];
        if m[2] > 0.5 && region_d(REGIONS[k].1, m[0], m[1]) > 0.0 {
            let (x, y) = (i as u32 % w, i as u32 / w);
            (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
        }
    }
    (x1 >= x0 && y1 >= y0).then(|| {
        let (a, b) = (x0.saturating_sub(pad), y0.saturating_sub(pad));
        (a, b, (x1 + pad).min(w - 1) - a + 1, (y1 + pad).min(h - 1) - b + 1)
    })
}

#[test]
fn fur_discriminator_keys_parse_clamp_round_trip_and_the_constants() {
    let d = ShellCfg::default();
    // v5: the default is the gloss test AND the colour flatness (was "t0.a<" alone).
    assert_eq!((d.fur_mask.as_str(), d.fur_threshold, d.fur_soft, d.uv_exclude.as_str()), ("t0.a<, t0.f>0.025", 0.66, 0.05, ""));
    assert_eq!(parse_fur_mask("t0.a<"), Ok(Some(FurMaskSel { slot: 0, ch: 3, above: false })));
    assert_eq!(parse_fur_mask(" T2 . L > "), Ok(Some(FurMaskSel { slot: 2, ch: 4, above: true })));
    assert_eq!(parse_fur_mask("off"), Ok(None));
    assert_eq!(parse_fur_mask(""), Ok(None));
    for bad in ["t4.a<", "t0.x<", "t0.a", "t0a<", "x0.a<", "t0.a<<", "nan"] {
        assert_eq!(parse_fur_mask(bad), Err(()), "{bad}");
    }
    let c = crate::config::Config::parse("shell_fur_mask=T1.G>\nshell_fur_threshold=0.4\nshell_fur_soft=0.2\nshell_uv_exclude=0.9 0.3 0.8 0.1 ; 0.1,0.1,0.2,0.2@17694; bad; 0.5 0.5 0.5 0.9; 2 -1 0.5 0.5").shell;
    assert_eq!((c.fur_mask.as_str(), c.fur_threshold, c.fur_soft), ("t1.g>", 0.4, 0.2));
    assert_eq!(c.uv_exclude, "0.8 0.1 0.9 0.3; 0.1 0.1 0.2 0.2@17694; 0.5 0 1 0.5", "sorted, clamped, empty and bad ones dropped");
    // Clamped, NaN / garbage = the default.
    let c = crate::config::Config::parse("shell_fur_mask=garbage\nshell_fur_threshold=7\nshell_fur_soft=0").shell;
    assert_eq!((c.fur_mask.as_str(), c.fur_threshold, c.fur_soft), (FUR_MASK_DEFAULT, 1.0, FUR_SOFT_RANGE.0));
    let c = crate::config::Config::parse("shell_fur_threshold=nan\nshell_fur_soft=x").shell;
    assert_eq!((c.fur_threshold, c.fur_soft), (FUR_THRESHOLD_DEFAULT, FUR_SOFT_DEFAULT));
    let n = ShellCfg { fur_threshold: f32::NAN, fur_soft: f32::INFINITY, fur_mask: "zz".into(), uv_exclude: "nan nan 1 1".into(), ..ShellCfg::default() }.clamped();
    assert_eq!((n.fur_threshold, n.fur_soft, n.fur_mask.as_str(), n.uv_exclude.as_str()), (FUR_THRESHOLD_DEFAULT, FUR_SOFT_DEFAULT, FUR_MASK_DEFAULT, ""));
    // Round trip of the ini text and the normalised rectangles; at most 16.
    let many = (0..20).map(|k| format!("0.{k:02} 0 0.{:02} 1", k + 1)).collect::<Vec<_>>().join(";");
    assert_eq!(parse_uv_rects(&many).len(), MAX_UV_RECTS);
    let x = ShellCfg { fur_mask: "t3.r>".into(), fur_threshold: 0.25, fur_soft: 0.125, uv_exclude: uv_rects_text(&parse_uv_rects("0.81 0.02 0.97 0.2; 0.1 0.6 0.2 0.7@17694")), ..ShellCfg::default() };
    let t = crate::config::Config { shell: x.clone(), ..crate::config::Config::default() }.to_ini();
    assert!(t.contains("shell_uv_exclude=0.81 0.02 0.97 0.2; 0.1 0.6 0.2 0.7@17694\n"), "{t}");
    assert_eq!(crate::config::Config::parse(&t).shell, x);
    // Hot reload one by one.
    let (m, dd) = crate::debugshot::merge(&crate::config::Config::default(), &crate::config::Config::default(), &crate::config::Config::parse("shell_fur_threshold=0.5\nshell_uv_exclude=0 0 0.1 0.1"));
    assert_eq!(dd.live, vec!["shell_fur_threshold", "shell_uv_exclude"]);
    assert_eq!((m.shell.fur_threshold, m.shell.uv_exclude.as_str()), (0.5, "0 0 0.1 0.1"));
    // The constants: off = all zero; the per-draw rectangles; NaN-safe.
    let off = Discrim::new(&ShellCfg { fur_mask: "off".into(), ..ShellCfg::default() });
    assert!(!off.active() && fd_cb_values(&off, 5) == [[0.0; 4]; FD_LEN] && fd_for_draw(&off, 5, true).is_none());
    let dflt = Discrim::new(&ShellCfg::default());
    assert_eq!(fd_cb_values(&dflt, 9)[0], [1.0, 0.0, 3.0, 0.0]);
    assert_eq!(fd_cb_values(&dflt, 9)[1], [0.66, 0.05, 0.0, UV_FEATHER]);
    assert!(fd_for_draw(&dflt, 9, false).is_none(), "the texture unbound and no rectangle: nothing to do, nothing bound");
    let rc = Discrim::new(&ShellCfg { fur_mask: "off".into(), uv_exclude: "0.1 0.1 0.2 0.2; 0.3 0.3 0.4 0.4@17694".into(), ..ShellCfg::default() });
    assert_eq!((rc.rects_for(17694), rc.rects_for(3354)), (2, 1));
    let v = fd_for_draw(&rc, 3354, true).unwrap();
    assert_eq!((v[0], v[1][2], v[2]), ([0.0; 4], 1.0, [0.1, 0.1, 0.2, 0.2]));
    assert_eq!(fd_for_draw(&rc, 17694, true).unwrap()[1][2], 2.0);
    let nan = Discrim { threshold: f32::NAN, soft: f32::NAN, ..dflt };
    assert!(fd_cb_values(&nan, 1).iter().flatten().all(|x| x.is_finite()));
    // The CPU mirror: fur below the threshold, a soft edge, the rectangle's feather.
    assert_eq!(fur_weight(&dflt, 1, Some(0.5), [0.5, 0.5]), 1.0);
    assert_eq!(fur_weight(&dflt, 1, Some(0.9), [0.5, 0.5]), 0.0);
    assert!((fur_weight(&dflt, 1, Some(0.66), [0.5, 0.5]) - 0.5).abs() < 1e-6);
    assert_eq!(fur_weight(&dflt, 1, None, [0.5, 0.5]), 1.0, "no texture: fur");
    assert_eq!(fur_weight(&rc, 1, None, [0.15, 0.15]), 0.0);
    assert!((fur_weight(&rc, 1, None, [0.1, 0.15]) - 0.5).abs() < 1e-6, "half at the border");
    assert_eq!(fur_weight(&rc, 1, None, [0.35, 0.35]), 1.0, "the @17694 rectangle is not for this draw");
    // The status line.
    let t = discrim_text(&ShellCfg { fur_mask: "t0.a<".into(), uv_exclude: "0.1 0.1 0.2 0.2".into(), ..ShellCfg::default() });
    assert!(t.contains("fur where t0.alpha < 0.660 (soft +-0.050), 1 excluded uv rects [0.1 0.1 0.2 0.2]"), "{t}");
    let t = discrim_text(&ShellCfg::default());
    assert!(t.contains("fur where t0.alpha < 0.660 (soft +-0.050) and t0.flatness (rgb 5x5 rms x2 / brightness at the ~1024 mip) > 0.025 (soft +-0.01"), "{t}");
    assert!(discrim_text(&ShellCfg { fur_mask: "off".into(), ..ShellCfg::default() }).contains("texture test off, 0 excluded uv rects"));
}

#[test]
fn the_discriminator_probe_reports_the_share_and_the_uv_at_a_point() {
    // 4 x 2 mask: two fur pixels, one horn pixel, one soft, the rest not drawn.
    let fur = [13107u16, 26214, 65535, 65535];
    let horn = [32768u16, 6554, 65535, 0];
    let soft = [32768u16, 6000, 65535, 32000];
    let none = [0u16; 4];
    let tx = [fur, fur, horn, soft, none, none, none, none];
    let st = [0.5f32, 0.5, 0.5, 0.5, 2.0, 2.0, 2.0, 2.0];
    let t = crate::furmask::discrim_probe(&tx, &st, 4, 2, &[("centre", 2, 0), ("mouse", 0, 1)]);
    assert!(t.contains("4 px drawn by the fur draws, 2 not fur (50.0 %), 1 in the soft edge (25.0 %)"), "{t}");
    assert!(t.contains("centre px (2, 0): uv (0.5000, 0.1000) fur weight 0.00"), "{t}");
    assert!(t.contains("mouse px (0, 1): uv (0.2000, 0.4000) fur weight 1.00 (nearest fur-draw pixel, 1 px away)"), "{t}");
    assert!(crate::furmask::discrim_probe(&[], &[], 0, 0, &[("centre", 0, 0)]).starts_with("discriminator probe: 0 px"));
}

#[test]
fn no_shells_on_the_horn_nose_and_claw_by_the_texture_gloss_with_a_soft_edge() {
    let _g = crate::testlock::lock();
    let o = region_opts();
    let Some(t) = run(&ShellCfg::default(), &o) else { return };
    let Some(off) = run(&ShellCfg { fur_mask: "off".into(), ..ShellCfg::default() }, &o) else { return };
    png3("shells-regions", &t.out, W, H, None);
    png3("shells-regions-off", &off.out, W, H, None);
    png3("shells-regions-mask", &mask_picture(&t), W, H, None);
    for (k, (name, _)) in REGIONS.iter().enumerate() {
        if let Some((x, y, cw, ch)) = region_box(&t, k, W, H, 16) {
            let z = (480 / cw.max(ch)).clamp(3, 8);
            png3(&format!("shells-regions-{name}-zoom"), &t.out, W, H, Some((x, y, cw, ch, z)));
            png3(&format!("shells-regions-{name}-off-zoom"), &off.out, W, H, Some((x, y, cw, ch, z)));
        }
    }
    let s = region_stats(&t, W, H);
    let so = region_stats(&off, W, H);
    println!(
        "texture discriminator: roots inside a region {} (by region {:?}; without it {} {:?}), fur px {} with shells {}, core px {} max weight {:.3}, fur min weight {:.3}, soft px {}",
        s.roots_inside, s.roots_inside_by, so.roots_inside, so.roots_inside_by, s.fur_px, s.fur_with_keys, s.core_px, s.core_max_a, s.fur_min_a, s.soft_px
    );
    for b in &s.bins {
        println!("  distance {:+.3}: mean fur weight {:.3} ({} px)", b.0, b.1, b.2);
    }
    // The scene proves something: without the discriminator every region grows strands.
    assert!(so.roots_inside_by.iter().all(|&c| c > 50), "{:?}", so.roots_inside_by);
    // 1. No strand is rooted inside a horn, the nose or a claw.
    assert_eq!(s.roots_inside, 0, "{:?}", s.roots_inside_by);
    // 2. The fur away from the regions keeps its shells and its full weight; the regions have none.
    assert!(s.fur_px > 5000 && s.fur_with_keys as f64 > 0.95 * s.fur_px as f64, "{} of {}", s.fur_with_keys, s.fur_px);
    assert!(s.fur_min_a > 0.97, "{}", s.fur_min_a);
    assert!(s.core_px > 300 && s.core_max_a < 0.05, "{} px, max {}", s.core_px, s.core_max_a);
    // 3. A soft transition: intermediate weights along the ramp, the mean weight falling towards the region.
    assert!(s.soft_px > 30, "{}", s.soft_px);
    let valid: Vec<f32> = s.bins.iter().filter(|b| b.2 > 20).map(|b| b.1).collect();
    assert!(valid.windows(2).all(|p| p[1] <= p[0] + 0.05), "{valid:?}");
    assert!(valid.first().is_some_and(|&a| a > 0.9) && valid.last().is_some_and(|&a| a < 0.1), "{valid:?}");
    // 4. The fur away from the regions looks like before (same strands, the root test changes nothing there).
    let n = (W * H) as usize;
    let far: Vec<usize> = (0..n)
        .filter(|&i| t.body[i] && !t.cloth[i] && t.mask[i][2] > 0.5 && region_of(t.mask[i][0], t.mask[i][1]).0 < -(RAMP + 0.06))
        .collect();
    let same = far.iter().filter(|&&i| (0..3).all(|k| (t.out[i][k] - off.out[i][k]).abs() < 0.02)).count();
    println!("fur far from the regions: {} px, {} unchanged by the discriminator", far.len(), same);
    assert!(same as f64 > 0.97 * far.len() as f64);
    for p in t.out.iter().flatten() {
        assert!(p.is_finite() && *p >= 0.0);
    }
}

#[test]
fn uv_rectangles_exclude_by_hand_and_match_the_cpu_mirror() {
    let _g = crate::testlock::lock();
    let o = region_opts();
    let rects = REGIONS.iter().map(|(_, r)| format!("{} {} {} {}", r[0], r[1], r[2], r[3])).collect::<Vec<_>>().join("; ");
    let cfg = ShellCfg { fur_mask: "off".into(), uv_exclude: rects, ..ShellCfg::default() };
    let Some(r) = run(&cfg, &o) else { return };
    png3("shells-rects", &r.out, W, H, None);
    png3("shells-rects-mask", &mask_picture(&r), W, H, None);
    let s = region_stats(&r, W, H);
    println!("uv rects: roots inside {} {:?}, fur px {} with shells {}, core max weight {:.3}", s.roots_inside, s.roots_inside_by, s.fur_px, s.fur_with_keys, s.core_max_a);
    assert_eq!(s.roots_inside, 0, "{:?}", s.roots_inside_by);
    assert!(s.fur_with_keys as f64 > 0.95 * s.fur_px as f64);
    assert!(s.core_max_a < 0.01);
    // The shader's weight is the CPU mirror's (the uv read back at 16 bits; the feather is a few pixels wide).
    let d = Discrim::new(&cfg);
    let n = (W * H) as usize;
    let (mut worst, mut soft) = (0.0f32, 0usize);
    for i in 0..n {
        let m = r.mask[i];
        if !r.body[i] || r.cloth[i] || m[2] <= 0.5 {
            continue;
        }
        let e = fur_weight(&d, 0, None, [m[0], m[1]]);
        worst = worst.max((e - m[3]).abs());
        soft += (m[3] > 0.05 && m[3] < 0.95) as usize;
    }
    println!("rect weight vs the CPU mirror: worst {worst:.4}, {soft} px in the feather");
    assert!(worst < 0.03 && soft > 30);
}

#[test]
fn the_discriminator_is_stable_frame_to_frame_and_off_or_unbound_is_the_old_output() {
    let _g = crate::testlock::lock();
    let o = region_opts();
    // Settle the unit (4 frames), then a third of a pixel and back.
    let jit = [[0.0, 0.0], [0.0, 0.0], [0.0, 0.0], [0.0, 0.0], [0.33, 0.0], [0.0, 0.0]];
    let Some(v) = run_seq(&ShellCfg::default(), &o, &jit) else { return };
    for (k, f) in v.iter().enumerate() {
        assert_eq!(region_stats(f, W, H).roots_inside, 0, "frame {k}");
    }
    assert!(v[3].keys == v[5].keys && v[3].out == v[5].out && v[3].mask == v[5].mask, "the same camera: bit-identical");
    let n = (W * H) as usize;
    let fur: Vec<bool> = (0..n).map(|i| v[3].body[i] && !v[3].cloth[i]).collect();
    let (mean, share) = change(&v[3].out, &v[4].out, &fur, 0.1);
    println!("a third of a pixel: mean change {mean:.4}, {:.2} % of the pixels by more than 0.1", share * 100.0);
    assert!(mean < 0.03 && share < 0.06);
    // The fur weight at the same uv does not move with the camera: compare the weight of pixels whose uv is (almost) the same.
    let mut by_uv = std::collections::HashMap::new();
    for i in 0..n {
        let m = v[3].mask[i];
        if m[2] > 0.5 && fur[i] {
            by_uv.insert(((m[0] * 2000.0) as i32, (m[1] * 2000.0) as i32), m[3]);
        }
    }
    let (mut worst, mut pairs) = (0.0f32, 0usize);
    for i in 0..n {
        let m = v[4].mask[i];
        if m[2] > 0.5 && fur[i] {
            if let Some(&a) = by_uv.get(&((m[0] * 2000.0) as i32, (m[1] * 2000.0) as i32)) {
                worst = worst.max((a - m[3]).abs());
                pairs += 1;
            }
        }
    }
    println!("fur weight at the same uv after the move: worst difference {worst:.3} over {pairs} pixel pairs");
    assert!(pairs > 1000 && worst < 0.25, "{worst}");
    // Off, and on with the selected texture slot unbound: the MASK and the old output are those of off, bit for bit (nothing bound for
    // the mask). (v7: the shells' own per-strand tests follow `shell_fur_mask` on / off, not its slot, so the keys may differ: changed
    // from "keys identical", the screen mask no longer gates the strands.)
    let Some(off) = run(&ShellCfg { fur_mask: "off".into(), ..ShellCfg::default() }, &o) else { return };
    let Some(unb) = run(&ShellCfg { fur_mask: "t3.a<".into(), ..ShellCfg::default() }, &o) else { return };
    assert!(unb.mask == off.mask && unb.old == off.old, "an unbound slot = off for the mask");
    assert!(off.mask.iter().all(|m| m[3] == 0.0 || m[3] == 1.0), "off: the mask is 0 / 1 as before");
    // And without the material texture (the scenes of the v2 tests) the default config changes nothing.
    let Some(a) = run(&ShellCfg::default(), &close()) else { return };
    let Some(b) = run(&ShellCfg { fur_mask: "off".into(), ..ShellCfg::default() }, &close()) else { return };
    assert!(a.out == b.out && a.keys == b.keys);
}

// ---- v4: the per-draw probe (furprobe.rs) and the detail channel ----------------------------------------------------------

#[test]
fn shell_debug_2_paints_each_strand_with_its_draws_colour_and_the_tag_leaves_the_picture_alone() {
    let _g = crate::testlock::lock();
    let o = Opts { tag: Some(1), ..close() };
    let Some(t) = run(&ShellCfg::default(), &o) else { return };
    let Some(plain) = run(&ShellCfg::default(), &close()) else { return };
    // The tag lives in the mask's b, which every reader only tests against 0.5: the picture and the shells are bit-identical.
    assert!(t.out == plain.out && t.keys == plain.keys && t.old == plain.old, "the tag changes nothing in the picture");
    let tags: Vec<Option<u32>> = t.mask.iter().filter(|m| m[2] > 0.5).map(|m| crate::furprobe::tag_of((m[2] * 65535.0).round() as u16)).collect();
    assert!(tags.len() > 5000 && tags.iter().all(|k| *k == Some(1)), "{} px, {:?}", tags.len(), tags.iter().find(|k| **k != Some(1)));
    let Some(d) = run(&ShellCfg { debug: 2, ..ShellCfg::default() }, &o) else { return };
    png("shells-debug2-v4", &d.out, W, H, None);
    let n = (W * H) as usize;
    let (mut strands, mut green) = (0usize, 0usize);
    let (mut body, mut tinted) = (0usize, 0usize);
    for i in 0..n {
        let c = d.out[i];
        let is_green = c[1] > c[0] + 0.25 && c[1] > c[2] + 0.1;
        if d.keys[i] != 0 && !d.body[i] && !d.cloth[i] && d.out[i] != t.out[i] {
            strands += 1;
            green += is_green as usize;
        }
        if d.body[i] && !d.cloth[i] && t.mask[i][2] > 0.5 {
            body += 1;
            let p = plain.out[i];
            tinted += (c[1] - c[0] > p[1] - p[0] + 0.05) as usize;
        }
    }
    println!("shell_debug=2: {green} of {strands} strand px outside the body green (draw #1), {tinted} of {body} body px tinted green");
    assert!(strands > 100 && green as f64 > 0.9 * strands as f64, "{green} of {strands}");
    assert!(body > 5000 && tinted as f64 > 0.9 * body as f64, "{tinted} of {body}");
    for p in d.out.iter().flatten() {
        assert!(p.is_finite() && *p >= 0.0);
    }
}

#[test]
fn the_detail_channel_removes_smooth_texels_and_matches_the_cpu_mirror() {
    let _g = crate::testlock::lock();
    let o = Opts { bumpy: true, ..region_opts() };
    let cfg = ShellCfg { fur_mask: "t0.d>".into(), fur_threshold: 0.3, ..ShellCfg::default() };
    let Some(r) = run(&cfg, &o) else { return };
    png3("shells-detail-mask", &mask_picture(&r), W, H, None);
    let s = region_stats(&r, W, H);
    println!("detail channel: roots inside {} {:?}, fur px {} with shells {}, fur min weight {:.3}, core px {} max weight {:.3}", s.roots_inside, s.roots_inside_by, s.fur_px, s.fur_with_keys, s.fur_min_a, s.core_px, s.core_max_a);
    assert_eq!(s.roots_inside, 0, "{:?}", s.roots_inside_by);
    assert!(s.fur_px > 5000 && s.fur_with_keys as f64 > 0.95 * s.fur_px as f64);
    assert!(s.core_px > 300 && s.core_max_a < 0.05, "{}", s.core_max_a);
    // The shader's value is furprobe::detail_at on the same texels (the uv read back at 16 bits: a few texel-border pixels may differ).
    let img = crate::furprobe::Img { w: 1024, h: 1024, px: material_level0(true).iter().map(|t| t.map(|c| (c.clamp(0.0, 1.0) * 255.0 + 0.5) as u8)).collect() };
    let d = Discrim::new(&cfg);
    let (mut n, mut agree) = (0usize, 0usize);
    for i in 0..(W * H) as usize {
        let m = r.mask[i];
        if !r.body[i] || r.cloth[i] || m[2] <= 0.5 {
            continue;
        }
        n += 1;
        let e = fur_weight(&d, 0, Some(crate::furprobe::detail_at(&img, [m[0], m[1]])), [m[0], m[1]]);
        agree += ((e - m[3]).abs() < 0.05) as usize;
    }
    println!("detail weight vs the CPU mirror: {agree} of {n} px within 0.05");
    assert!(n > 5000 && agree as f64 > 0.97 * n as f64);
}

#[test]
fn the_new_channels_parse_and_print() {
    assert_eq!(parse_fur_mask("t1.d>"), Ok(Some(FurMaskSel { slot: 1, ch: CH_DETAIL, above: true })));
    assert_eq!(parse_fur_mask("t0.s<"), Ok(Some(FurMaskSel { slot: 0, ch: CH_SAT, above: false })));
    assert_eq!(clean_fur_mask("T1.D>"), "t1.d>");
    assert_eq!(fd_cb_values(&Discrim::new(&ShellCfg { fur_mask: "t1.d>".into(), ..ShellCfg::default() }), 1)[0], [1.0, 1.0, 5.0, 1.0]);
    assert!(discrim_text(&ShellCfg { fur_mask: "t1.d>".into(), fur_threshold: 0.3, ..ShellCfg::default() }).contains("fur where t1.detail"));
    assert_eq!(crate::config::Config::parse("shell_debug=2").shell.debug, 2);
    assert_eq!(composite_cb_values(true, &ShellCfg { debug: 2, ..ShellCfg::default() })[0][3], 2.0);
}

// ---- v5: test lists, the colour flatness, the real atlas ----------------------------------------------------------------------

/// The light-coat line (docs/shell-fur.md "v5"): gloss, flatness, saturation.
const TUNED_MASK: &str = "t0.a<, t0.f>0.025, t0.s<0.2";

#[test]
fn fur_test_lists_parse_print_round_trip_and_fill_the_constants() {
    let a = FurMaskSel { slot: 0, ch: 3, above: false };
    let f = FurMaskSel { slot: 0, ch: CH_FLAT, above: true };
    let s = FurMaskSel { slot: 0, ch: CH_SAT, above: false };
    assert_eq!(parse_fur_tests("t0.s<0.2"), Ok(vec![FurTest { sel: s, th: Some(0.2) }]));
    assert_eq!(parse_fur_tests(FUR_MASK_DEFAULT), Ok(vec![FurTest { sel: a, th: None }, FurTest { sel: f, th: Some(0.025) }]));
    assert_eq!(parse_fur_tests("off"), Ok(vec![]));
    assert_eq!(clean_fur_mask(" T0.A< & t0.F>0.0250 ,t0.s<0.2"), TUNED_MASK);
    assert_eq!(clean_fur_mask("t0.a<7"), "t0.a<1", "a threshold is clamped to 0..1");
    assert_eq!(clean_fur_mask("t0.a<, t0.b<, t0.g<, t0.r<, t0.l<").matches(',').count(), 3, "at most 4 tests");
    for bad in ["t0.a<,", "t0.a<x", "t0.f>nan", "t0.a<, zz", "t0.a<<", "t0.f>0.0.2"] {
        assert_eq!(parse_fur_tests(bad), Err(()), "{bad}");
        assert_eq!(clean_fur_mask(bad), FUR_MASK_DEFAULT, "{bad}");
    }
    assert_eq!(clean_fur_mask("t1.f<"), "t1.f<");
    // The ini round trip and one-by-one hot reload of a list.
    let x = ShellCfg { fur_mask: TUNED_MASK.into(), ..ShellCfg::default() };
    let t = crate::config::Config { shell: x.clone(), ..crate::config::Config::default() }.to_ini();
    assert!(t.contains(&format!("shell_fur_mask={TUNED_MASK}\n")), "{t}");
    assert_eq!(crate::config::Config::parse(&t).shell, x);
    // The constants: test 1 in [0] / [1] as before, tests 2..4 in FD_MORE.., their thresholds in FD_MORE_TH; the tag slot untouched.
    let d = Discrim::new(&x);
    assert_eq!(d.slots(), 1);
    let v = fd_cb_values(&d, 3354);
    assert_eq!((v[0], v[1]), ([1.0, 0.0, 3.0, 0.0], [0.66, 0.05, 0.0, UV_FEATHER]));
    assert_eq!((v[FD_MORE], v[FD_MORE + 1], v[FD_MORE + 2]), ([1.0, 0.0, 7.0, 1.0], [1.0, 0.0, 6.0, 0.0], [0.0; 4]));
    assert_eq!(v[FD_MORE_TH], [0.025, 0.2, 0.0, 0.0]);
    assert_eq!(v[FD_TAG], [0.0; 4]);
    // A test on an unbound slot is switched off; all unbound and no rectangle = nothing bound.
    let mixed = Discrim::new(&ShellCfg { fur_mask: "t1.d>0.3, t0.a<".into(), ..ShellCfg::default() });
    assert_eq!(mixed.slots(), 3);
    let only_t1 = fd_for_draw_slots(&mixed, 1, 0b10).unwrap();
    assert_eq!((only_t1[0], only_t1[FD_MORE]), ([1.0, 1.0, 5.0, 1.0], [0.0; 4]));
    assert_eq!(fd_for_draw_slots(&mixed, 1, 0b01).unwrap()[0], [0.0; 4]);
    assert!(fd_for_draw_slots(&mixed, 1, 0b100).is_none());
    assert!(fd_for_draw_slots(&Discrim::new(&ShellCfg::default()), 1, 0).is_none());
    // The soft edge: at most half the distance to 0 / 1.
    assert_eq!((test_soft(0.05, 0.66), test_soft(0.05, 0.02), test_soft(0.5, 0.5)), (0.05, 0.01, 0.25));
    assert!(test_soft(f32::NAN, 0.5).is_finite());
    // The CPU mirror with every test (values: alpha, flatness, saturation).
    let w = |a: f32, fl: f32, sa: f32| fur_weight_all(&d, 3354, &[Some(a), Some(fl), Some(sa)], [0.5, 0.5]);
    assert_eq!(w(0.41, 0.12, 0.03), 1.0, "white fur");
    assert_eq!(w(0.41, 1.0, 0.0), 1.0, "clipped white fur");
    assert_eq!(w(0.40, 0.0, 0.008), 0.0, "the horn: flat");
    assert_eq!(w(0.56, 0.2, 0.45), 0.0, "a claw: saturated");
    assert!((w(0.41, 0.025, 0.03) - 0.5).abs() < 1e-6, "half at the flatness threshold");
    assert_eq!(fur_weight_all(&d, 1, &[Some(0.41), None, None], [0.5, 0.5]), 1.0, "unmeasured tests say fur");
    // The status line lists every test.
    let t = discrim_text(&x);
    assert!(t.contains("and t0.flatness (rgb 5x5 rms x2 / brightness at the ~1024 mip) > 0.025 (soft +-0.01"), "{t}");
}

#[test]
fn the_flatness_is_dye_invariant_and_counts_clipped_white_as_fur() {
    use crate::furprobe::{Img, flat_at};
    let img = |f: &dyn Fn(u32, u32) -> [u8; 4]| Img { w: 64, h: 64, px: (0..64 * 64).map(|i| f(i % 64, i / 64)).collect() };
    let hash = |x: u32, y: u32| ((x.wrapping_mul(2_654_435_761) ^ y.wrapping_mul(40_503)).wrapping_mul(2_246_822_519) >> 24) as f32 / 255.0 - 0.5;
    let painted = |base: [f32; 3]| move |x: u32, y: u32| {
        let k = 1.0 + 0.16 * hash(x, y);
        [(base[0] * k * 255.0) as u8, (base[1] * k * 255.0) as u8, (base[2] * k * 255.0) as u8, 255]
    };
    let flat = img(&|_, _| [128, 128, 128, 255]);
    let white = img(&painted([0.9, 0.88, 0.85]));
    let brown = img(&painted([0.35, 0.22, 0.12]));
    let clipped = img(&|_, _| [255, 255, 255, 255]);
    let at = |i: &Img| flat_at(i, [0.5, 0.5]);
    println!("flatness: flat {:.3}, white coat {:.3}, brown coat {:.3}, clipped {:.3}", at(&flat), at(&white), at(&brown), at(&clipped));
    assert_eq!(at(&flat), 0.0);
    assert!(at(&white) > 0.05 && at(&brown) > 0.05, "a painted coat is never flat");
    assert!((at(&white) - at(&brown)).abs() < 0.25 * at(&white), "dye-invariant: the same paint on a white and a brown coat");
    assert_eq!(at(&clipped), 1.0, "clipped white: no detail to measure, counted as fur");
    assert_eq!(flat_at(&Img::default(), [0.5, 0.5]), 0.0);
}

#[test]
fn the_flatness_channel_removes_flat_texels_and_matches_the_cpu_mirror() {
    let _g = crate::testlock::lock();
    let o = region_opts();
    let cfg = ShellCfg { fur_mask: "t0.f>0.025".into(), ..ShellCfg::default() };
    let Some(r) = run(&cfg, &o) else { return };
    png3("shells-flat-mask", &mask_picture(&r), W, H, None);
    let s = region_stats(&r, W, H);
    println!("flatness channel: roots inside {} {:?}, fur px {} with shells {}, fur min weight {:.3}, core px {} max weight {:.3}", s.roots_inside, s.roots_inside_by, s.fur_px, s.fur_with_keys, s.fur_min_a, s.core_px, s.core_max_a);
    assert_eq!(s.roots_inside, 0, "{:?}", s.roots_inside_by);
    assert!(s.fur_px > 5000 && s.fur_with_keys as f64 > 0.95 * s.fur_px as f64);
    assert!(s.fur_min_a > 0.97, "{}", s.fur_min_a);
    assert!(s.core_px > 300 && s.core_max_a < 0.05, "{}", s.core_max_a);
    // The shader's value is furprobe::flat_at on the same texels (the uv read back at 16 bits: a few texel-border pixels may differ).
    let img = crate::furprobe::Img { w: 1024, h: 1024, px: material_level0(false).iter().map(|t| t.map(|c| (c.clamp(0.0, 1.0) * 255.0 + 0.5) as u8)).collect() };
    let d = Discrim::new(&cfg);
    let (mut n, mut agree) = (0usize, 0usize);
    for i in 0..(W * H) as usize {
        let m = r.mask[i];
        if !r.body[i] || r.cloth[i] || m[2] <= 0.5 {
            continue;
        }
        n += 1;
        let e = fur_weight(&d, 0, Some(crate::furprobe::flat_at(&img, [m[0], m[1]])), [m[0], m[1]]);
        agree += ((e - m[3]).abs() < 0.05) as usize;
    }
    println!("flatness weight vs the CPU mirror: {agree} of {n} px within 0.05");
    assert!(n > 5000 && agree as f64 > 0.97 * n as f64);
}

#[test]
fn the_owner_line_and_the_default_remove_every_region_and_keep_the_coat() {
    let _g = crate::testlock::lock();
    let o = region_opts();
    for mask in [FUR_MASK_DEFAULT, TUNED_MASK] {
        let Some(r) = run(&ShellCfg { fur_mask: mask.into(), ..ShellCfg::default() }, &o) else { return };
        let s = region_stats(&r, W, H);
        println!("{mask}: roots inside {} {:?}, fur px {} with shells {}, fur min weight {:.3}, core max {:.3}", s.roots_inside, s.roots_inside_by, s.fur_px, s.fur_with_keys, s.fur_min_a, s.core_max_a);
        assert_eq!(s.roots_inside, 0, "{mask}: {:?}", s.roots_inside_by);
        assert!(s.fur_px > 5000 && s.fur_with_keys as f64 > 0.95 * s.fur_px as f64, "{mask}");
        assert!(s.fur_min_a > 0.97 && s.core_max_a < 0.05, "{mask}: {} {}", s.fur_min_a, s.core_max_a);
    }
}

/// The real atlas (a charr, `GW2Graphics\debug\furtex-<crc>-rgb.png` + `-a.png` and the uv plot of the
/// frame with the claws in view) as numbers measured with `furprobe::flat_at` / saturation / alpha at the 1024 mip: (class, alpha,
/// flatness, saturation) at the 1st / 10th / 50th percentile of each class ("horn" = the flat grey crescent of the head draw 3354;
/// "head fur" / "body fur" = the texels of the draws 3354 / 2712 visible in that frame; "claw" = the brown, saturated texels of 2712).
/// The default and the configured line must classify them as the game showed: (class, [alpha, flatness, saturation], the configured line's
/// weight must lie in lo..=hi).
const REAL: [(&str, [f32; 3], f32, f32); 13] = [
    ("horn p50", [0.404, 0.000, 0.008], 0.0, 0.01),
    ("horn p90 (its edge)", [0.412, 0.018, 0.008], 0.0, 0.15),
    ("head fur p1", [0.39, 0.026, 0.0], 0.45, 1.0),
    ("head fur p2", [0.39, 0.029, 0.0], 0.6, 1.0),
    ("head fur p10", [0.392, 0.048, 0.005], 0.99, 1.0),
    ("head fur p50", [0.404, 0.125, 0.03], 0.99, 1.0),
    ("body fur p1", [0.0, 0.035, 0.0], 0.9, 1.0),
    ("body fur p50", [0.408, 0.219, 0.012], 0.99, 1.0),
    ("body fur clipped white", [0.41, 1.0, 0.0], 0.99, 1.0),
    ("claw p2", [0.404, 0.050, 0.275], 0.0, 0.01),
    ("claw p50", [0.561, 0.251, 0.45], 0.0, 0.01),
    ("claw removed by the gloss", [0.718, 0.112, 0.423], 0.0, 0.01),
    ("fang", [0.50, 0.16, 0.36], 0.0, 0.01),
];

#[test]
fn the_real_atlas_classes_are_classified_like_the_game() {
    let dflt = Discrim::new(&ShellCfg::default());
    let tuned = Discrim::new(&ShellCfg { fur_mask: TUNED_MASK.into(), ..ShellCfg::default() });
    for (name, [a, f, s], lo, hi) in REAL {
        let wd = fur_weight_all(&dflt, 3354, &[Some(a), Some(f)], [0.5, 0.5]);
        let wo = fur_weight_all(&tuned, 3354, &[Some(a), Some(f), Some(s)], [0.5, 0.5]);
        println!("{name}: default {wd:.3}, tuned {wo:.3}");
        assert!((lo..=hi).contains(&wo), "{name}: tuned weight {wo} outside {lo}..{hi}");
        if lo > 0.4 {
            assert!((wd - wo).abs() < 1e-6, "{name}: on fur the saturation test changes nothing ({wd} / {wo})");
        }
    }
    // The default (generic) removes the horn; the claws only where the gloss says so (the colour of a claw is no generic rule).
    assert!(fur_weight_all(&dflt, 3354, &[Some(0.404), Some(0.0)], [0.5, 0.5]) < 0.01);
    assert!(fur_weight_all(&dflt, 2712, &[Some(0.718), Some(0.112)], [0.5, 0.5]) < 0.01);
}

// ---- v6: excluded uv rectangles by the strand's own uv (shell_uv_exclude; the picker that filled them is removed in v7) ----------

/// Keys (any layer, the base layer included) whose ROOT (the anchor) lies deep inside a region, per region: (all, base layer only).
fn layers_rooted_inside(r: &Out, w: u32, h: u32) -> ([usize; 3], usize) {
    let (mut by, mut base) = ([0usize; 3], 0usize);
    for i in 0..(w * h) as usize {
        let Some((hh, _, d)) = unpack_key(r.keys[i]) else { continue };
        let (x, y) = ((i as u32 % w) as f32 + 0.5 + d[0], (i as u32 / w) as f32 + 0.5 + d[1]);
        if x < 0.0 || y < 0.0 || x >= w as f32 || y >= h as f32 {
            continue;
        }
        let m = r.mask[(y as u32 * w + x as u32) as usize];
        if m[2] <= 0.5 {
            continue;
        }
        let (dd, k) = region_of(m[0], m[1]);
        if dd > 0.008 {
            by[k] += 1;
            base += (hh <= 0.0) as usize;
        }
    }
    (by, base)
}

#[test]
fn excluded_roots_lose_every_layer_by_their_own_uv_even_without_the_screen_root_test() {
    let _g = crate::testlock::lock();
    // The strand pixel shader's test of its own uv (the root's), which also holds where the root is hidden on the screen (a horn behind
    // the mane, the far side of the muzzle): the silhouette strands. (v7: there is no screen-mask root test in the strand shader any
    // more, so the former `no_root_mask` switch is gone: this is the default path.)
    let o = region_opts();
    let rects = REGIONS.iter().map(|(_, r)| format!("{} {} {} {}", r[0], r[1], r[2], r[3])).collect::<Vec<_>>().join("; ");
    let Some(base) = run(&ShellCfg { fur_mask: "off".into(), ..ShellCfg::default() }, &o) else { return };
    let Some(r) = run(&ShellCfg { fur_mask: "off".into(), uv_exclude: rects.clone(), ..ShellCfg::default() }, &o) else { return };
    let (b_by, b_base) = layers_rooted_inside(&base, W, H);
    let (by, nbase) = layers_rooted_inside(&r, W, H);
    let s = region_stats(&r, W, H);
    println!("rooted inside the regions without exclusions {b_by:?} (base layer {b_base}), with the rectangles {by:?} (base layer {nbase}); fur px {} with shells {}", s.fur_px, s.fur_with_keys);
    assert!(b_by.iter().all(|&c| c > 50) && b_base > 50, "the scene proves something: {b_by:?} {b_base}");
    assert_eq!((by, nbase), ([0; 3], 0), "no layer of an excluded root is left, the base layer included");
    assert!(s.fur_with_keys as f64 > 0.95 * s.fur_px as f64, "the coat keeps its shells: {} of {}", s.fur_with_keys, s.fur_px);
    // Draw-specific rectangles: a rectangle for another index count changes nothing.
    let other = REGIONS.iter().map(|(_, r)| format!("{} {} {} {}@17694", r[0], r[1], r[2], r[3])).collect::<Vec<_>>().join("; ");
    let Some(x) = run(&ShellCfg { fur_mask: "off".into(), uv_exclude: other, ..ShellCfg::default() }, &o) else { return };
    assert!(x.keys == base.keys, "a rectangle for another draw leaves this one alone");
    // The constants: only the rectangles of this draw, with the mask's feather.
    let d = Discrim::new(&ShellCfg { fur_mask: "off".into(), uv_exclude: format!("{rects}; 0.9 0.9 1 1@17694"), ..ShellCfg::default() });
    let v = ps_rect_values(&d, 3354);
    assert_eq!(v[0], [3.0, UV_FEATHER, 0.0, 0.0]);
    assert_eq!(v[4], [0.0; 4]);
    assert_eq!(ps_rect_values(&d, 17694)[0][0], 4.0);
    assert_eq!(ps_rect_values(&Discrim::default(), 1), [[0.0; 4]; 1 + MAX_UV_RECTS]);
    assert_eq!(std::mem::size_of::<PsCb>(), 336); // v8: + q4 (the draw's depth bias)
}

// ---- v7: the transparency fix and the per-strand fur discriminator (docs/shell-fur.md "v7") ------------------------------------

/// Strand keys (h > 0) whose ROOT (the anchor) shows a mask uv deep inside rect k (inset 0.008), per rect.
fn roots_in(r: &Out, w: u32, h: u32, rects: &[[f32; 4]]) -> Vec<usize> {
    let mut by = vec![0usize; rects.len()];
    for i in 0..(w * h) as usize {
        let Some((hh, _, d)) = unpack_key(r.keys[i]) else { continue };
        if hh <= 0.0 {
            continue;
        }
        let (x, y) = ((i as u32 % w) as f32 + 0.5 + d[0], (i as u32 / w) as f32 + 0.5 + d[1]);
        if x < 0.0 || y < 0.0 || x >= w as f32 || y >= h as f32 {
            continue;
        }
        let m = r.mask[(y as u32 * w + x as u32) as usize];
        if m[2] <= 0.5 {
            continue;
        }
        for (k, rc) in rects.iter().enumerate() {
            if region_d(*rc, m[0], m[1]) > 0.008 {
                by[k] += 1;
            }
        }
    }
    by
}

/// Body pixels (not the cloth, not the second sphere) whose mask uv is farther than 0.03 from every rect, and how many carry a key.
fn coat_kept(r: &Out, w: u32, h: u32, rects: &[[f32; 4]]) -> (usize, usize) {
    let (mut n, mut k) = (0usize, 0usize);
    for i in 0..(w * h) as usize {
        let m = r.mask[i];
        if !r.body[i] || r.cloth[i] || r.front.get(i).copied().unwrap_or(false) || m[2] <= 0.5 {
            continue;
        }
        if rects.iter().all(|rc| region_d(*rc, m[0], m[1]) < -0.03) {
            n += 1;
            k += (r.keys[i] != 0) as usize;
        }
    }
    (n, k)
}

/// The key's anchor lies deep inside the pixel set `occ` (the anchor and its 24 neighbours all in it): a root hidden behind that
/// surface (an anchor within two pixels of the occluder's edge can be a visible root: the shader matches the root over the 3x3 pixels
/// around its anchor, and the key's quarter-pixel anchor can round to the next pixel).
fn anchored_inside(key: u32, i: usize, w: u32, h: u32, occ: &[bool]) -> bool {
    let Some(a) = anchor_of(key, i, w, h) else { return false };
    let (x, y) = ((a as u32 % w) as i32, (a as u32 / w) as i32);
    (-2..=2).all(|dy| (-2..=2).all(|dx| {
        let (xx, yy) = (x + dx, y + dy);
        xx >= 0 && yy >= 0 && xx < w as i32 && yy < h as i32 && occ[(yy as u32 * w + xx as u32) as usize]
    }))
}

/// The anchor pixel of a key (inside the picture), or None.
fn anchor_of(key: u32, i: usize, w: u32, h: u32) -> Option<usize> {
    let (_, _, d) = unpack_key(key)?;
    let (x, y) = ((i as u32 % w) as f32 + 0.5 + d[0], (i as u32 / w) as f32 + 0.5 + d[1]);
    (x >= 0.0 && y >= 0.0 && x < w as f32 && y < h as f32).then(|| (y as u32 * w + x as u32) as usize)
}

#[test]
fn v7_a_nearer_object_lying_on_the_fur_stays_pixel_identical() {
    let _g = crate::testlock::lock();
    // The cloth 2 cm in front of the sphere: inside the strand length (4 cm), where the v6 test (the root's depth + the strand's reach
    // + 0.6 %) let the strands paint over it with the colour of their roots (the ghosted cloth seen in the game).
    // (an object within 5 mm + 0.3 % of the root counts as the same surface: FRAG_TOL_M / FRAG_REL_TOL)
    for gap in [0.02f32, 0.012, 0.12] {
        let o = Opts { cloth_gap: gap, ..close() };
        let Some(r) = run(&ShellCfg::default(), &o) else { return };
        let n = (W * H) as usize;
        let cloth: Vec<usize> = (0..n).filter(|&i| r.cloth[i]).collect();
        // The cloth pixels a strand could reach: within 40 px of a body pixel (the exposure of this scene).
        let near = grown(&r.body.iter().zip(&r.cloth).map(|(b, c)| *b && !*c).collect::<Vec<_>>(), W, H, 40);
        let exposed = cloth.iter().filter(|&&i| near[i]).count();
        let keyed = cloth.iter().filter(|&&i| r.keys[i] != 0).count();
        let changed = cloth.iter().filter(|&&i| r.out[i] != r.old[i]).count();
        // Strands of roots hidden behind the cloth: their anchors on cloth pixels.
        let hidden = (0..n).filter(|&i| anchored_inside(r.keys[i], i, W, H, &r.cloth)).count();
        let keys = r.keys.iter().filter(|&&k| k != 0).count();
        println!("cloth {gap} m in front: {} cloth px ({exposed} within strand reach of the fur), {keyed} with a key, {changed} changed, {hidden} keys rooted behind the cloth, {keys} keys", cloth.len());
        assert!(cloth.len() > 2000 && exposed > 500, "the scene proves something");
        assert_eq!((keyed, changed, hidden), (0, 0, 0), "the cloth is the no-shell output, bit for bit");
        assert!(keys > 5000, "the fur keeps its shells");
    }
}

#[test]
fn v7_strands_over_the_background_are_a_coverage_blend() {
    let _g = crate::testlock::lock();
    let o = Opts { cloth: false, ..close() };
    // v10: the 4 cm strands of this scene's evidence (the default is 2.5 cm now: fewer outline pixels over the sky).
    let Some(r) = run(&ShellCfg { length: 4.0, ..ShellCfg::default() }, &o) else { return };
    let n = (W * H) as usize;
    // Coverage buckets of the strand pixels over the sky: the change from the no-shell picture grows with the coverage (a blend, not a
    // replacement by the root colour), and pixels without a key are untouched.
    let mut by = [(0.0f64, 0usize); 16];
    let mut untouched = true;
    for i in 0..n {
        if r.body[i] {
            continue;
        }
        match unpack_key(r.keys[i]) {
            None => untouched &= r.out[i] == r.old[i],
            Some((_, a, _)) => {
                let q = (a * 15.0).round() as usize;
                let d = (0..3).map(|k| (r.out[i][k] - r.old[i][k]).abs()).fold(0.0f32, f32::max) as f64;
                by[q] = (by[q].0 + d, by[q].1 + 1);
            }
        }
    }
    let mean = |lo: usize, hi: usize| {
        let (s, c) = by[lo..=hi].iter().fold((0.0, 0usize), |a, b| (a.0 + b.0, a.1 + b.1));
        (s / c.max(1) as f64, c)
    };
    let (low, nl) = mean(1, 4);
    let (high, nh) = mean(11, 15);
    println!("over the sky: coverage 1..4/15 mean change {low:.4} ({nl} px), 11..15/15 {high:.4} ({nh} px)");
    assert!(untouched, "no key, no change");
    // v8: 30 (was 100): the edge-on fade (FACE_LO..FACE_HI) thins the most grazing outline strands (v7 298 covering px, v8 64).
    assert!(nl > 100 && nh > 30, "both kinds of pixels exist");
    assert!(low < 0.5 * high, "a faint strand edge changes the background less than a covering strand");
    for p in r.out.iter().flatten() {
        assert!(p.is_finite() && *p >= 0.0);
    }
}

/// The second sphere's index count (`sphere(24, 48)`).
const SECOND_COUNT: u32 = 24 * 48 * 6;

#[test]
fn v7_two_characters_overlapping_never_show_each_others_hidden_strands() {
    let _g = crate::testlock::lock();
    // A smaller fur character 0.5 m in front of the first, overlapping it on the screen.
    let o = Opts { cloth: false, second: Some([0.10, -0.05, 0.70, 0.12]), ..close() };
    let n = (W * H) as usize;
    // (1) The front character has no shells (shell_counts leaves its mesh out): every key on its pixels would be a strand of the one
    // behind painted over it (the "transparent NPC").
    let Some(r) = run(&ShellCfg { counts: format!("-{SECOND_COUNT}"), ..ShellCfg::default() }, &o) else { return };
    let front: Vec<usize> = (0..n).filter(|&i| r.front[i]).collect();
    let keyed = front.iter().filter(|&&i| r.keys[i] != 0).count();
    // v11: the shells' effect without the skin gate (`v10`); the front character is not recorded, so it is not proven fur either:
    // with the gate it is the picture without the old look.
    let changed = front.iter().filter(|&&i| r.v10[i] != r.old[i]).count();
    assert!(front.iter().all(|&i| r.out[i] == r.plain[i]), "v11: an unrecorded character gets no old look");
    let behind = (0..n).filter(|&i| anchored_inside(r.keys[i], i, W, H, &r.front)).count();
    let keys = r.keys.iter().filter(|&&k| k != 0).count();
    println!("front character without shells: {} px, {keyed} with a key, {changed} changed; {behind} keys rooted where it hides the back one; {keys} keys", front.len());
    assert!(front.len() > 3000, "the front character is in view");
    assert_eq!((keyed, changed, behind), (0, 0, 0));
    assert!(keys > 5000, "the back character keeps its shells");
    // (2) Both with shells: a key on the front character is rooted on the front character (its anchor shows it), never on the back one.
    let Some(b) = run(&ShellCfg::default(), &o) else { return };
    let (mut on_front, mut foreign) = (0usize, 0usize);
    for &i in &front {
        if let Some(a) = anchor_of(b.keys[i], i, W, H) {
            on_front += 1;
            foreign += (!b.front[a]) as usize;
        }
    }
    println!("both with shells: {on_front} keys on the front character, {foreign} rooted elsewhere");
    assert!(on_front > 2000);
    assert!(foreign * 200 <= on_front, "{foreign} of {on_front}");
}

#[test]
fn v7_buffers_changed_between_record_and_replay_never_ghost() {
    let _g = crate::testlock::lock();
    let o = Opts { cloth: false, ..close() };
    let Some(base) = run(&ShellCfg::default(), &o) else { return };
    let base_keys = base.keys.iter().filter(|&&k| k != 0).count();
    // A DYNAMIC vertex buffer can be rewritten (Map DISCARD) before the replay. v7..v13 refused it (no shells: an NPC charr
    // bare in a crowd); v14 copies it at the draw: the replay draws exactly the recorded mesh. (Changed test: the refusal was the
    // replaced behaviour.) Unchanged buffer: the same keys as the DEFAULT buffer, bit for bit.
    let Some(d) = run(&ShellCfg::default(), &Opts { dynamic_vb: true, ..o }) else { return };
    println!("dynamic vertex buffer: {} draws replayed, {} keys", d.replayed, d.keys.iter().filter(|&&k| k != 0).count());
    assert_eq!(d.replayed, 1);
    assert!(d.keys == base.keys, "the copy draws the same mesh: the same keys");
    // Rewritten (the mesh moved 25 cm, Map WRITE_DISCARD) after the draw and before the replay: still the recorded mesh's keys.
    let Some(dr) = run(&ShellCfg::default(), &Opts { dynamic_vb: true, stale: 0.25, ..o }) else { return };
    println!("dynamic vertex buffer rewritten after the draw: {} replayed, {} keys", dr.replayed, dr.keys.iter().filter(|&&k| k != 0).count());
    assert!(dr.replayed == 1 && dr.keys == base.keys, "the replay draws the copy taken at the draw, not the rewritten buffer");
    // v14: a RIGID draw (vcb 208, the game's 59f8b692 NPC bodies) gets shells like the skinned one (the default and a saved 4096); a
    // shell_min_vcb other than the old default still leaves it out.
    let Some(r) = run(&ShellCfg::default(), &Opts { rigid: true, ..o }) else { return };
    let Some(r4) = run(&ShellCfg { min_vcb: MIN_VCB_LEGACY, ..ShellCfg::default() }, &Opts { rigid: true, ..o }) else { return };
    let Some(r8) = run(&ShellCfg { min_vcb: 8192, ..ShellCfg::default() }, &Opts { rigid: true, ..o }) else { return };
    println!("rigid draw: {} / {} / {} replayed (default, 4096, 8192)", r.replayed, r4.replayed, r8.replayed);
    assert!(r.replayed == 1 && r.keys == base.keys && r4.keys == base.keys, "the rigid draw is replayed like the skinned one");
    assert!(r8.replayed == 0 && r8.keys.iter().all(|&k| k == 0));
    // v15: the vertex and index buffers are DYNAMIC RINGS (other data before the mesh, base vertex 6000, start index 9000, both rewritten
    // with garbage after the draw): only the draw's windows are copied, the call rebased: the same keys as the plain mesh.
    let Some(rg) = run(&ShellCfg::default(), &Opts { ring: true, ..o }) else { return };
    println!("dynamic rings: {} replayed, {} keys (the plain mesh {base_keys})", rg.replayed, rg.keys.iter().filter(|&&k| k != 0).count());
    assert!(rg.replayed == 1 && rg.keys == base.keys, "the windows hold exactly the draw's mesh");
    // A DEFAULT buffer rewritten after the draw (here: the mesh moved 25 cm): the replay draws stale geometry. Its roots are not where
    // the screen shows a surface at their depth, so the root test drops almost all of them; none paints the background or the body
    // with a colour of the wrong place in bulk.
    let Some(s) = run(&ShellCfg::default(), &Opts { stale: 0.25, ..o }) else { return };
    let stale_keys = s.keys.iter().filter(|&&k| k != 0).count();
    println!("stale vertex buffer: {stale_keys} keys (the true mesh: {base_keys})");
    assert!(base_keys > 5000);
    assert!(stale_keys * 20 < base_keys, "{stale_keys} of {base_keys}");
}

/// The coats of the v7 tests (stored colours): white, grey, dark brown, orange.
const COATS: [(&str, [f32; 3]); 4] = [("white", [0.94, 0.91, 0.86]), ("grey", [0.45, 0.45, 0.47]), ("dark brown", [0.30, 0.20, 0.12]), ("orange", [0.80, 0.46, 0.16])];

#[test]
fn v7_the_per_strand_discriminator_keeps_any_coat_and_drops_horn_claw_and_lips() {
    let _g = crate::testlock::lock();
    let rects: Vec<[f32; 4]> = V7_REGIONS.iter().map(|(_, r)| *r).collect();
    for (name, col) in COATS {
        let o = Opts { cloth: false, coat: Some(col), ..close() };
        let Some(r) = run(&ShellCfg::default(), &o) else { return };
        let Some(off) = run(&ShellCfg { fur_mask: "off".into(), ..ShellCfg::default() }, &o) else { return };
        let by = roots_in(&r, W, H, &rects);
        let by_off = roots_in(&off, W, H, &rects);
        let (cn, ck) = coat_kept(&r, W, H, &rects);
        png(&format!("v7-coat-{}", name.replace(' ', "-")), &r.out, W, H, None);
        println!("{name} coat: strands rooted in horn / claw / pad / lips {by:?} (texel tests off: {by_off:?}), coat {ck} of {cn} px keep shells, statistics {:?}", r.stats.first());
        assert!(by_off.iter().all(|&c| c > 30), "{name}: the scene proves something: {by_off:?}");
        // (at most 2 keys of the ~600 a region grows without the tests: an anchor rounded onto the region's inset edge)
        assert!(by[0] <= 2 && by[1] <= 2 && by[3] <= 2, "{name}: no strand on the horn, the glossy claw, the lips: {by:?}");
        // The keratin pad (the claw's tan with the COAT's gloss) is told by its colour where the coat is not itself tan / orange; on a
        // saturated tan or orange coat it keeps strands (docs: those coats rely on the claws' gloss, as in the real atlases).
        if name == "white" || name == "grey" {
            assert!(by[2] <= 2, "{name}: the pad by its colour: {by:?}");
        }
        assert!(cn > 5000 && ck as f64 > 0.95 * cn as f64, "{name}: the coat keeps its shells: {ck} of {cn}");
    }
}

#[test]
fn v7_a_horn_root_hidden_behind_the_mane_grows_no_strands() {
    let _g = crate::testlock::lock();
    let white = COATS[0].1;
    let o = Opts { cloth: false, coat: Some(white), ..close() };
    let Some(first) = run(&ShellCfg::default(), &o) else { return };
    // The horn's screen box, then a "mane" (a second fur draw without shells) right in front of it, 25 cm nearer.
    let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0u32, 0u32);
    for i in 0..(W * H) as usize {
        let m = first.mask[i];
        if m[2] > 0.5 && region_d(V7_REGIONS[0].1, m[0], m[1]) > 0.0 {
            let (x, y) = (i as u32 % W, i as u32 / W);
            (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
        }
    }
    assert!(x1 > x0 && y1 > y0, "the horn is in view");
    let (cx, cy) = ((x0 + x1) as f32 * 0.5 / W as f32 * 2.0 - 1.0, 1.0 - (y0 + y1) as f32 * 0.5 / H as f32 * 2.0);
    let p11 = 1.0 / TAN_HALF_FOV;
    let p00 = p11 * H as f32 / W as f32;
    let z = 0.7;
    let o2 = Opts { second: Some([cx / p00 * z, cy / p11 * z, z, 0.07]), ..o };
    for mask in [FUR_MASK_DEFAULT, "off"] {
        let Some(r) = run(&ShellCfg { counts: format!("-{SECOND_COUNT}"), fur_mask: mask.into(), ..ShellCfg::default() }, &o2) else { return };
        let n = (W * H) as usize;
        let hidden = (0..n).filter(|&i| anchored_inside(r.keys[i], i, W, H, &r.front)).count();
        let on_mane = (0..n).filter(|&i| r.front[i] && r.keys[i] != 0).count();
        let horn = roots_in(&r, W, H, &[V7_REGIONS[0].1])[0];
        println!("mane in front of the horn ({mask}): {} mane px, {hidden} keys rooted behind it, {on_mane} keys on it, {horn} rooted on the visible horn", r.front.iter().filter(|&&f| f).count());
        assert!(r.front.iter().filter(|&&f| f).count() > 500);
        assert_eq!((hidden, on_mane), (0, 0), "{mask}: no strand of a hidden root, whatever the texture says");
        if mask != "off" {
            assert!(horn <= 2, "{horn}");
        }
    }
}

#[test]
fn v7_the_gpu_coat_statistics_match_the_cpu_mirror() {
    let _g = crate::testlock::lock();
    let white = COATS[2].1;
    let o = Opts { cloth: false, coat: Some(white), ..close() };
    let Some(v) = run_seq(&ShellCfg::default(), &o, &[[0.0, 0.0]; 4]) else { return };
    let st = v.iter().rev().find_map(|f| f.stats.first().copied()).expect("the statistics came back within 4 frames");
    // The CPU mirror over the same 32 x 32 cells (the sphere's uv covers the whole square).
    let img = crate::furprobe::Img { w: 1024, h: 1024, px: coat_level0(white, true).iter().map(|t| t.map(|c| (c.clamp(0.0, 1.0) * 255.0 + 0.5) as u8)).collect() };
    let mut cells = Vec::new();
    for y in 0..FEAT_N {
        for x in 0..FEAT_N {
            cells.push(texel_features(&img, [(x as f32 + 0.5) / FEAT_N as f32, (y as f32 + 0.5) / FEAT_N as f32]));
        }
    }
    let (s0, s1) = coat_stats(&cells);
    println!("GPU {:?} / {:?}; CPU {s0:?} / {s1:?}", st.s0, st.s1);
    assert!(st.s0[2] > 0.9 * 1024.0, "the footprint covers the cells: {}", st.s0[2]);
    for k in 0..2 {
        assert!((st.s0[k] - s0[k]).abs() < 0.04, "s0[{k}] {} vs {}", st.s0[k], s0[k]);
    }
    for k in 0..3 {
        assert!((st.s1[k] - s1[k]).abs() < 0.04, "s1[{k}] {} vs {}", st.s1[k], s1[k]);
    }
    assert!(hue_dist(st.s1[3], s1[3]) < 4.0, "hue {} vs {}", st.s1[3], s1[3]);
    assert!(st.s0[3] > 0.99 && draw_weight(s0) > 0.99, "a fur draw");
    assert_eq!(st.count, 48 * 96 * 6);
    let t = stats_text(&[st], true);
    assert!(t.contains("-> fur draw") && t.contains("gloss band"), "{t}");
}

/// Real texels measured on the debug captures' atlases (`GW2Graphics\debug\furtex-<crc>-rgb.png` / `-a.png`, the ~1024 level, 5x5 means):
/// the coat statistics of the body region (stand-in for the body draw's footprint: s0 = band share, median flatness; s1 = median
/// gloss, saturation, value, hue) and the body fur's median texel, the claws' median and 75th-percentile-gloss texel (features: gloss,
/// saturation, value, flatness; hue). docs/shell-fur.md "v7" has the whole table and how it was measured.
const REAL_V7: [(&str, [f32; 2], [f32; 4], [[f32; 5]; 3]); 7] = [
    ("323868e4 white", [0.668, 0.115], [0.398, 0.048, 0.544, 120.0], [[0.415, 0.008, 0.627, 0.245, 338.0], [0.48, 0.511, 0.432, 0.421, 27.8], [0.614, 0.469, 0.418, 0.276, 30.0]]),
    ("05a0d288 orange tiger", [0.728, 0.211], [0.412, 0.657, 0.293, 33.8], [[0.417, 0.739, 0.499, 0.078, 31.8], [0.483, 0.546, 0.443, 0.608, 34.5], [0.617, 0.294, 0.501, 0.131, 30.1]]),
    ("dfe7c25c grey", [0.859, 0.226], [0.413, 0.098, 0.278, 36.6], [[0.417, 0.071, 0.371, 0.116, 47.1], [0.483, 0.557, 0.437, 0.599, 33.1], [0.617, 0.285, 0.503, 0.12, 30.2]]),
    ("c91ca4a3 dark blue-grey", [0.928, 0.376], [0.409, 0.266, 0.189, 226.1], [[0.417, 0.222, 0.155, 0.392, 224.7], [0.483, 0.566, 0.44, 0.619, 32.8], [0.617, 0.301, 0.5, 0.113, 32.3]]),
    ("e884ef71 brown", [0.968, 0.206], [0.423, 0.45, 0.236, 29.3], [[0.424, 0.443, 0.289, 0.3, 32.2], [0.607, 0.464, 0.269, 0.185, 20.1], [0.672, 0.505, 0.55, 0.112, 28.6]]),
    ("3c71e3f2 orange", [0.967, 0.275], [0.428, 0.611, 0.269, 21.2], [[0.424, 0.653, 0.333, 0.239, 22.7], [0.607, 0.519, 0.273, 0.24, 25.9], [0.672, 0.473, 0.548, 0.102, 26.0]]),
    ("21a45427 dark", [0.766, 0.271], [0.431, 0.795, 0.084, 27.0], [[0.424, 0.723, 0.118, 0.245, 20.6], [0.607, 0.481, 0.245, 0.46, 17.6], [0.673, 0.45, 0.546, 0.211, 30.8]]),
];

#[test]
fn v7_real_atlas_texels_are_classified_by_the_coat_relative_rule() {
    for (name, s0, s1, t) in REAL_V7 {
        let s0 = [s0[0], s0[1], 1024.0, 0.0];
        assert!(draw_weight(s0) > 0.99, "{name}: the body draw is a fur draw");
        let w = |f: [f32; 5]| texel_weight([f[0], f[1], f[2], f[3]], f[4], s0, s1);
        let (coat, claw, claw_glossy) = (w(t[0]), w(t[1]), w(t[2]));
        println!("{name}: coat {coat:.3}, claw p50 {claw:.3}, glossy claw {claw_glossy:.3}");
        assert!(coat > 0.99, "{name}: the coat's median texel is fur");
        assert!(claw < 0.05 && claw_glossy < 0.05, "{name}: the claws are not");
    }
    // The horn of a charr (flat) and its fang (tan, a little glossier) on the head draw's statistics.
    let head = ([0.87, 0.114, 1024.0, 0.0], [0.41, 0.04, 0.6, 100.0]);
    assert!(texel_weight([0.404, 0.008, 0.5, 0.0], 30.0, head.0, head.1) < 0.01, "the flat horn");
    assert!(texel_weight([0.45, 0.38, 0.62, 0.255], 34.0, head.0, head.1) < 0.05, "the fang");
    assert!(texel_weight([0.40, 0.02, 0.72, 0.086], 40.0, head.0, head.1) > 0.99, "the cheek fur");
    // A uniformly flat coat (median flatness 0.02): its texels at 0.02 stay fur, the dead-flat horn (0.0) does not.
    let flat = ([0.9, 0.02, 1024.0, 0.0], [0.41, 0.1, 0.5, 30.0]);
    assert!(draw_weight(flat.0) < 0.01, "a coat flatter than every measured charr coat is not taken as a fur draw at all (fail closed)");
    let soft = ([0.9, 0.05, 1024.0, 0.0], [0.41, 0.1, 0.5, 30.0]);
    // A smooth coat (median flatness 0.05): the threshold follows it down to 0.02 (soft 0.01..0.03): its texels at 0.04 stay fur, the
    // dead-flat horn (0.0) does not.
    assert!(texel_weight([0.41, 0.1, 0.5, 0.04], 30.0, soft.0, soft.1) > 0.99 && texel_weight([0.41, 0.1, 0.5, 0.0], 30.0, soft.0, soft.1) < 0.01);
    // Human skin (gloss 0.53..0.55: band share 0.01..0.30), metal armour, hair cards (0.56): no fur draw.
    for (what, band, fl) in [("human skin 1ff2e7a8", 0.30, 0.03), ("human skin 3ce54b1d", 0.01, 0.05), ("hair cards", 0.05, 0.19), ("light coat armour", 0.37, 0.2)] {
        assert!(draw_weight([band, fl, 1024.0, 0.0]) < 0.01, "{what}");
    }
    assert_eq!(draw_weight([1.0, 0.3, 8.0, 0.0]), 0.0, "no footprint: no shells");
    // The coat statistics mirror: a footprint of fur cells and a few claw cells: the medians are the fur's.
    let mut cells = vec![([0.42, 0.05, 0.6, 0.2], 40.0); 900];
    cells.extend(vec![([0.62, 0.45, 0.45, 0.3], 30.0); 100]);
    let (a, b) = coat_stats(&cells);
    assert!((a[0] - 0.9).abs() < 1e-3 && a[2] == 1000.0 && a[3] > 0.99);
    assert!((b[0] - 0.42).abs() < 0.04 && (b[1] - 0.05).abs() < 0.04, "{b:?}");
    assert_eq!(coat_stats(&[]), ([0.0; 4], [0.0; 4]));
}

// ---- v8: the strands seen "through" a nearer arm, the banner fringe, the comb of an edge-on card, the dilated texel tests ---------

/// Keys on the pixels `on` whose anchor (root) is NOT one of those pixels: strands of something else painted over them.
fn foreign_keys(r: &Out, on: &[bool]) -> (usize, usize) {
    let (mut n, mut foreign) = (0usize, 0usize);
    for (i, &o) in on.iter().enumerate() {
        if !o {
            continue;
        }
        if let Some(a) = anchor_of(r.keys[i], i, W, H) {
            n += 1;
            foreign += (!on[a]) as usize;
        }
    }
    (n, foreign)
}

#[test]
fn v8_an_arm_in_front_in_the_same_draw_never_shows_the_fur_behind_it() {
    let _g = crate::testlock::lock();
    // Seen in the game: the tail fur showing through the arm in front. The arm is part of the SAME draw as the body (one mesh), 10..30 cm in
    // front of it, with the engine's depth bias: none, the measured one (65 steps), and a slope-scaled one.
    for bias in [None, Some((65, 0.0f32)), Some((100, 1.0f32))] {
        let o = Opts { cloth: false, arm: Some([0.12, -0.05, 0.70, 0.10]), bias, ..close() };
        let Some(r) = run(&ShellCfg::default(), &o) else { return };
        let arm_px = r.arm.iter().filter(|&&a| a).count();
        let (on_arm, foreign) = foreign_keys(&r, &r.arm);
        // Keys rooted on the body where the arm hides it (their colour would be the arm's).
        let hidden = (0..(W * H) as usize).filter(|&i| anchored_inside(r.keys[i], i, W, H, &r.arm)).count();
        let keys = r.keys.iter().filter(|&&k| k != 0).count();
        println!("arm in the same draw, bias {bias:?}: {arm_px} arm px, {on_arm} keys on it, {foreign} of them rooted off the arm, {hidden} keys anchored inside the arm, {keys} keys");
        assert!(arm_px > 2000, "the arm is in view");
        assert_eq!(foreign, 0, "{bias:?}: no strand of the body behind is painted over the arm");
        assert!(keys > 5000 && on_arm > 500, "the body and the arm keep their own shells");
        // The arm's pixels that carry no key of its own are the no-shell output, bit for bit.
        let touched = (0..(W * H) as usize).filter(|&i| r.arm[i] && r.keys[i] == 0 && r.out[i] != r.old[i]).count();
        assert_eq!(touched, 0);
    }
}

#[test]
fn v8_another_character_in_front_with_a_depth_bias_never_shows_hidden_strands() {
    let _g = crate::testlock::lock();
    for bias in [Some((65, 0.0f32)), Some((100, 1.0f32))] {
        let o = Opts { cloth: false, second: Some([0.10, -0.05, 0.70, 0.12]), bias, ..close() };
        let n = (W * H) as usize;
        let Some(r) = run(&ShellCfg { counts: format!("-{SECOND_COUNT}"), ..ShellCfg::default() }, &o) else { return };
        let keyed = (0..n).filter(|&i| r.front[i] && r.keys[i] != 0).count();
        let changed = (0..n).filter(|&i| r.front[i] && r.v10[i] != r.old[i]).count(); // v11: without the skin gate (see v7)
        let behind = (0..n).filter(|&i| anchored_inside(r.keys[i], i, W, H, &r.front)).count();
        let keys = r.keys.iter().filter(|&&k| k != 0).count();
        println!("front character (no shells), bias {bias:?}: {keyed} keys on it, {changed} px changed, {behind} rooted behind it, {keys} keys");
        assert_eq!((keyed, changed, behind), (0, 0, 0), "{bias:?}");
        assert!(keys > 5000, "{bias:?}: the back character keeps its shells under the bias: {keys}");
        let Some(b) = run(&ShellCfg::default(), &o) else { return };
        let (on_front, foreign) = foreign_keys(&b, &b.front);
        println!("  both with shells: {on_front} keys on the front one, {foreign} rooted elsewhere");
        assert!(on_front > 2000 && foreign * 200 <= on_front, "{bias:?}: {foreign} of {on_front}");
    }
}

#[test]
fn v8_a_banner_drawn_after_the_linear_depth_hides_the_strands_behind_it() {
    let _g = crate::testlock::lock();
    // Seen in the game: a comb of strands along the bottom edge of a banner hanging in front of a charr. Geometry the engine
    // draws after it wrote its linear depth (decorations, banners, alpha-tested cloth) is only in the DEVICE depth: v7 measured the
    // strands against the linear depth alone and painted the fur's strands over it. v8 also tests the device depth.
    for gap in [0.02f32, 0.12] {
        let o = Opts { late_cloth: true, cloth_gap: gap, ..close() };
        let Some(r) = run(&ShellCfg::default(), &o) else { return };
        let n = (W * H) as usize;
        let banner = (0..n).filter(|&i| r.cloth[i]).count();
        let keyed = (0..n).filter(|&i| r.cloth[i] && r.keys[i] != 0).count();
        let changed = (0..n).filter(|&i| r.cloth[i] && r.out[i] != r.old[i]).count();
        let keys = r.keys.iter().filter(|&&k| k != 0).count();
        println!("banner {gap} m in front, not in the linear depth: {banner} px, {keyed} with a key, {changed} changed; {keys} keys");
        assert!(banner > 2000, "the banner is in view");
        assert_eq!((keyed, changed), (0, 0), "the banner is the no-shell output, bit for bit");
        assert!(keys > 5000, "the fur keeps its shells");
    }
}

#[test]
fn v8_a_card_seen_edge_on_grows_no_comb_of_strands() {
    let _g = crate::testlock::lock();
    // A tail seen in the game: a flat strip seen at a grazing angle. Its strands all stand parallel in one plane: a hard-edged
    // comb / a rectangle of strands along its straight edge. v8: no shells below |cos| 0.08, full from 0.20.
    let mut counts = Vec::new();
    for c in [0.03f32, 0.06, 0.5] {
        let o = Opts { cloth: false, card: Some(c), ..close() };
        let Some(r) = run(&ShellCfg::default(), &o) else { return };
        let body = r.body.iter().filter(|&&b| b).count();
        let keys = r.keys.iter().filter(|&&k| k != 0).count();
        let near = grown(&r.body, W, H, 1);
        let outside = (0..(W * H) as usize).filter(|&i| r.keys[i] != 0 && !near[i]).count();
        png(&format!("v8-card-{c}"), &r.out, W, H, None);
        println!("card at |cos| {c}: {body} card px, {keys} keys, {outside} of them off the card");
        counts.push((c, body, keys, outside));
    }
    for &(c, body, keys, outside) in &counts {
        assert!(body > 200, "the card is in view at {c}");
        if c < FACE_LO {
            assert_eq!((keys, outside), (0, 0), "edge-on ({c}): no comb");
        } else {
            assert!(keys > 1000, "a card facing the camera ({c}) keeps its fur: {keys}");
        }
    }
}

#[test]
fn v8_the_dilated_texel_tests_remove_the_border_of_a_horn_and_a_claw() {
    // The root next to a horn / a claw has its own 5x5 window still partly in the fur (v7: fur), a window 3 texels away on the horn /
    // claw (v8: not fur). A 64 x 64 coat painted like fur (relative flatness ~0.09, gloss 0.41) with a flat grey "horn" and a glossy tan
    // "claw" (x >= 40); the CPU mirror (= ft_feat_dil) at the texel 3 columns outside them. The real atlases: docs v8.
    let mut px = vec![[0u8; 4]; 64 * 64];
    for y in 0..64usize {
        for x in 0..64usize {
            let n = ((x * 7 + y * 13) % 5) as f32 / 4.0 - 0.5; // painted strands
            let v = (0.62 + 0.10 * n).clamp(0.0, 1.0);
            px[y * 64 + x] = [(v * 255.0) as u8, (v * 250.0) as u8, (v * 240.0) as u8, 105];
            if x >= 40 && y < 32 {
                px[y * 64 + x] = [130, 130, 130, 105]; // the flat horn
            }
            if x >= 40 && y >= 32 {
                px[y * 64 + x] = [150, 110, 70, 170]; // the glossy claw
            }
        }
    }
    let img = crate::furprobe::Img { w: 64, h: 64, px };
    let coat = ([0.9, 0.09, 1024.0, 0.0], [0.41, 0.03, 0.62, 30.0]);
    let at = |x: f32, y: f32| [(x + 0.5) / 64.0, (y + 0.5) / 64.0];
    let w = |f: ([f32; 4], f32)| texel_weight(f.0, f.1, coat.0, coat.1);
    // The fur away from them: fur with and without the dilation.
    assert!(w(texel_features(&img, at(12.0, 16.0))) > 0.99 && w(texel_features_dilated(&img, at(12.0, 16.0))) > 0.99, "the coat");
    // Next to the horn / the claw: v7 fur (its own 5x5 window is not all horn / mostly fur), v8 not (a window 3 texels away is all
    // horn / mostly claw). The flat test needs a whole window on the horn, the gloss test a window's mean.
    for (what, x, y) in [("horn", 39.0f32, 16.0f32), ("claw", 37.0, 48.0)] {
        let (v7, v8) = (w(texel_features(&img, at(x, y))), w(texel_features_dilated(&img, at(x, y))));
        println!("{what} border: v7 {v7:.3}, v8 {v8:.3}");
        assert!(v7 > 0.5 && v8 < 0.05, "{what}: {v7} {v8}");
    }
    // A little farther: fur again (the dilation is 3 texels).
    assert!(w(texel_features_dilated(&img, at(35.0, 16.0))) > 0.99);
    assert!(w(texel_features_dilated(&img, at(34.0, 48.0))) > 0.99);
    // The constants of the strand shader's b0 (v8: q3.w = the fragment tolerance in metres, q4 = the draw's depth bias).
    assert_eq!(ps_ext_values(1000.0)[3], FRAG_TOL_M);
    let mut rs = [0u32; 10];
    rs[3] = (-65i32) as u32;
    rs[5] = 1.0f32.to_bits();
    let b = ps_bias_values(&rs, 44);
    assert!((b[0] - 65.0 / 16_777_216.0).abs() < 1e-12 && b[1] == 1.0 && b[2] == 0.0, "{b:?}");
    rs[4] = f32::NAN.to_bits();
    rs[5] = f32::INFINITY.to_bits();
    assert!(ps_bias_values(&rs, 55).iter().all(|v| v.is_finite()));
}

// ---- v9: the UV-island classification ------------------------------------------------------------------------------------------

/// The parts of the v9 island test mesh: (name, direction from the body's centre, length and base radius in object units (x 0.5 =
/// metres), uv rectangle of its island, excluded by design). The horn and the claws are cones with fur-like texels inside their
/// islands (`island_level0`): the texel rule alone leaks strands there; the ear is (v10) a flat pointed plate on plain coat texels
/// (`ear_plate`; v9 used a cone tapered like a horn, which the v10 shape rule now excludes like a horn): it must keep its fur.
const ISL_PARTS: [(&str, [f32; 3], f32, f32, [f32; 4], bool); 5] = [
    ("horn", [-0.75, 0.6, -0.25], 0.7, 0.2, [0.56, 0.05, 0.76, 0.45], true),
    ("claw 1", [0.70, -0.62, -0.30], 0.4, 0.08, [0.80, 0.05, 0.95, 0.30], true),
    ("claw 2", [0.86, -0.40, -0.30], 0.4, 0.08, [0.80, 0.35, 0.95, 0.60], true),
    ("claw 3", [0.50, -0.82, -0.28], 0.4, 0.08, [0.80, 0.65, 0.95, 0.90], true),
    ("ear", [0.65, 0.70, -0.25], 0.6, 0.25, [0.56, 0.55, 0.76, 0.95], false),
];

fn n3(a: [f32; 3]) -> [f32; 3] {
    let l = (a[0] * a[0] + a[1] * a[1] + a[2] * a[2]).sqrt().max(1e-9);
    [a[0] / l, a[1] / l, a[2] / l]
}

fn c3(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}

/// A tapered cone (8 rings x 16 segments, the tip at 8 % of the base radius) standing on the unit sphere along `d`, its uv unrolled
/// over `rect` (u around, v from the base at the bottom to the tip at the top; the seam column duplicated with another u, like a real
/// unwrap). 8 floats per vertex like `sphere`; every triangle wound so its outward side is the front.
fn cone(d: [f32; 3], len: f32, r: f32, rect: [f32; 4]) -> (Vec<f32>, Vec<u32>) {
    let n = n3(d);
    let a = if n[1].abs() < 0.9 { [0.0, 1.0, 0.0] } else { [1.0, 0.0, 0.0] };
    let u1 = n3(c3(n, a));
    let u2 = c3(n, u1);
    let b = [n[0] * 0.92, n[1] * 0.92, n[2] * 0.92];
    let (rings, segs) = (8u32, 16u32);
    let mut v = Vec::new();
    for j in 0..=rings {
        let t = j as f32 / rings as f32;
        let rr = r * (1.0 - 0.92 * t);
        for k in 0..=segs {
            let ph = std::f32::consts::TAU * (k % segs) as f32 / segs as f32;
            let rad = [u1[0] * ph.cos() + u2[0] * ph.sin(), u1[1] * ph.cos() + u2[1] * ph.sin(), u1[2] * ph.cos() + u2[2] * ph.sin()];
            let p = [0, 1, 2].map(|c| b[c] + n[c] * len * t + rad[c] * rr);
            let uv = [rect[0] + (rect[2] - rect[0]) * k as f32 / segs as f32, rect[3] - (rect[3] - rect[1]) * t];
            v.extend_from_slice(&[p[0], p[1], p[2], rad[0], rad[1], rad[2], uv[0], uv[1]]);
        }
    }
    let row = segs + 1;
    let pos = |k: u32| [v[k as usize * 8], v[k as usize * 8 + 1], v[k as usize * 8 + 2]];
    let mut idx = Vec::new();
    for j in 0..rings {
        for k in 0..segs {
            let (a0, b0, c0, d0) = (j * row + k, j * row + k + 1, (j + 1) * row + k, (j + 1) * row + k + 1);
            for mut tri in [[a0, b0, c0], [b0, d0, c0]] {
                let (p0, p1, p2) = (pos(tri[0]), pos(tri[1]), pos(tri[2]));
                let cr = c3([p1[0] - p0[0], p1[1] - p0[1], p1[2] - p0[2]], [p2[0] - p0[0], p2[1] - p0[1], p2[2] - p0[2]]);
                let cen = [(p0[0] + p1[0] + p2[0]) / 3.0, (p0[1] + p1[1] + p2[1]) / 3.0, (p0[2] + p1[2] + p2[2]) / 3.0];
                let along = (cen[0] - b[0]) * n[0] + (cen[1] - b[1]) * n[1] + (cen[2] - b[2]) * n[2];
                let out = [cen[0] - b[0] - n[0] * along, cen[1] - b[1] - n[1] * along, cen[2] - b[2] - n[2] * along];
                if cr[0] * out[0] + cr[1] * out[1] + cr[2] * out[2] < 0.0 {
                    tri.swap(1, 2);
                }
                idx.extend_from_slice(&tri);
            }
        }
    }
    (v, idx)
}

/// v10: a flat pointed EAR plate (what a real ear is; v9 used a cone, which the v10 shape rule now treats as a horn): two sheets, a
/// lens cross-section (`thick` in the middle, 0 at the rim, shared rim vertices), standing on the unit sphere along `d`, `len` long,
/// `width` wide at the base narrowing to a point, facing the camera (its plane holds `d` and the direction across the view); the front
/// sheet's uv on the left half of `rect`, the back sheet's on the right half. 8 floats per vertex like `sphere`, outward fronts.
fn ear_plate(d: [f32; 3], len: f32, width: f32, thick: f32, rect: [f32; 4]) -> (Vec<f32>, Vec<u32>) {
    let n = n3(d);
    let u1 = n3(c3(n, [0.0, 0.0, 1.0]));
    let u2 = c3(n, u1);
    let b = [n[0] * 0.92, n[1] * 0.92, n[2] * 0.92];
    let (rows, cols) = (8u32, 8u32);
    let (mut v, mut idx) = (Vec::new(), Vec::new());
    for side in [1.0f32, -1.0] {
        let half = if side > 0.0 { [rect[0], rect[1], (rect[0] + rect[2]) * 0.5 - 0.005, rect[3]] } else { [(rect[0] + rect[2]) * 0.5 + 0.005, rect[1], rect[2], rect[3]] };
        let first = (v.len() / 8) as u32;
        for j in 0..=rows {
            let t = j as f32 / rows as f32;
            for k in 0..=cols {
                let s = 2.0 * k as f32 / cols as f32 - 1.0;
                let off = side * 0.5 * thick * (1.0 - s * s) * (1.0 - t);
                let p = [0, 1, 2].map(|c| b[c] + n[c] * len * t + u1[c] * s * 0.5 * width * (1.0 - t) + u2[c] * off);
                let uv = [half[0] + (half[2] - half[0]) * k as f32 / cols as f32, half[3] - (half[3] - half[1]) * t];
                v.extend_from_slice(&[p[0], p[1], p[2], u2[0] * side, u2[1] * side, u2[2] * side, uv[0], uv[1]]);
            }
        }
        let row = cols + 1;
        let pos = |k: u32| [v[k as usize * 8], v[k as usize * 8 + 1], v[k as usize * 8 + 2]];
        for j in 0..rows {
            for k in 0..cols {
                let (a0, b0, c0, d0) = (first + j * row + k, first + j * row + k + 1, first + (j + 1) * row + k, first + (j + 1) * row + k + 1);
                for mut tri in [[a0, b0, c0], [b0, d0, c0]] {
                    let (p0, p1, p2) = (pos(tri[0]), pos(tri[1]), pos(tri[2]));
                    let cr = c3([p1[0] - p0[0], p1[1] - p0[1], p1[2] - p0[2]], [p2[0] - p0[0], p2[1] - p0[1], p2[2] - p0[2]]);
                    if (cr[0] * u2[0] + cr[1] * u2[1] + cr[2] * u2[2]) * side < 0.0 {
                        tri.swap(1, 2);
                    }
                    idx.extend_from_slice(&tri);
                }
            }
        }
    }
    (v, idx)
}

/// The v9 island test mesh: the coat sphere (its uv squeezed into the left half of the atlas) and the `ISL_PARTS` cones, ONE draw
/// (v10: the ear a flat plate, `ear_plate`).
fn island_mesh() -> (Vec<f32>, Vec<u32>) {
    let (mut v, mut idx) = sphere(48, 96);
    for q in v.chunks_exact_mut(8) {
        q[6] *= 0.5;
    }
    for (name, d, len, r, rect, _) in ISL_PARTS {
        let base = (v.len() / 8) as u32;
        let (cv, ci) = if name == "ear" { ear_plate(d, len, 2.0 * r, 0.06, rect) } else { cone(d, len, r, rect) };
        v.extend(cv);
        idx.extend(ci.iter().map(|i| i + base));
    }
    (v, idx)
}

/// The atlas of the island test (1024): a painted white coat everywhere; the horn island a FLAT grey with fur-like bands of coat
/// (24 texel rows of every 48) and a 0.015 coat border (what the texel rule alone leaks on); the claw islands glossy keratin tan on
/// their upper 60 % (the tip end) and coat-coloured, matte, painted on the lower 40 % (the base: like the real claws' matte base);
/// the ear island plain coat.
fn island_level0() -> Vec<[f32; 4]> {
    let coat = [0.94, 0.91, 0.86];
    let mut l0 = coat_level0(coat, false);
    let size = 1024u32;
    for y in 0..size {
        for x in 0..size {
            let (u, v) = ((x as f32 + 0.5) / size as f32, (y as f32 + 0.5) / size as f32);
            let i = (y * size + x) as usize;
            let paint = l0[i][0] / coat[0];
            for (k, (_, _, _, _, rect, _)) in ISL_PARTS.iter().enumerate() {
                let d = region_d(*rect, u, v);
                if d <= 0.0 {
                    continue;
                }
                match k {
                    0 => {
                        if d > 0.015 && (y / 24) % 2 == 0 {
                            l0[i] = [0.50, 0.48, 0.45, 0.41];
                        }
                    }
                    1..=3 => {
                        let t = (rect[3] - v) / (rect[3] - rect[1]); // 0 at the base, 1 at the tip
                        if t > 0.4 {
                            l0[i] = [0.62 * paint, 0.48 * paint, 0.34 * paint, 0.62];
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    l0
}

/// Keys (every layer, the base layer included) whose root's anchor shows the mask uv inside `rect` (0.008 inset): (in the junction
/// band = the base fifth of the part, where the coat sphere and the embedded cone have the same depth so a COAT root right at the
/// junction can show the part at its anchor; on the rest of the part).
fn isl_roots(r: &Out, w: u32, h: u32, rect: [f32; 4]) -> (usize, usize) {
    let (mut band, mut rest) = (0, 0);
    for i in 0..(w * h) as usize {
        let Some((_, _, d)) = unpack_key(r.keys[i]) else { continue };
        let (x, y) = ((i as u32 % w) as f32 + 0.5 + d[0], (i as u32 / w) as f32 + 0.5 + d[1]);
        if x < 0.0 || y < 0.0 || x >= w as f32 || y >= h as f32 {
            continue;
        }
        let m = r.mask[(y as u32 * w + x as u32) as usize];
        if m[2] > 0.5 && region_d(rect, m[0], m[1]) > 0.008 {
            if (rect[3] - m[1]) / (rect[3] - rect[1]) < 0.2 {
                band += 1;
            } else {
                rest += 1;
            }
        }
    }
    (band, rest)
}

/// Coat pixels (the sphere's own uv, away from its seam) and how many carry a key.
fn isl_coat(r: &Out, w: u32, h: u32) -> (usize, usize) {
    let (mut n, mut k) = (0usize, 0usize);
    for i in 0..(w * h) as usize {
        let m = r.mask[i];
        if r.body[i] && m[2] > 0.5 && m[0] > 0.02 && m[0] < 0.48 {
            n += 1;
            k += (r.keys[i] != 0) as usize;
        }
    }
    (n, k)
}

#[test]
fn v9_islands_drop_every_layer_of_the_horn_and_claws_and_keep_the_coat_and_the_ear() {
    let _g = crate::testlock::lock();
    let base = Opts { cloth: false, island_mesh: true, ..close() };
    let Some(off) = run(&ShellCfg::default(), &Opts { islands: 0, ..base }) else { return };
    let Some(mut seq) = run_seq(&ShellCfg::default(), &Opts { islands: 1, ..base }, &[[0.0, 0.0]; 5]) else { return };
    let on = seq.pop().unwrap();
    println!("v9 status: {}", on.isl);
    assert!(on.isl.contains("ready:"), "the analysis finished within the frames: {}", on.isl);
    png("v9-islands-off", &off.out, W, H, None);
    png("v9-islands-on", &on.out, W, H, None);
    for (name, _, _, _, rect, excluded) in ISL_PARTS {
        let ((ja, ra), (jb, rb)) = (isl_roots(&off, W, H, rect), isl_roots(&on, W, H, rect));
        let (a, b) = (ja + ra, jb + rb);
        println!("{name}: keys anchored on it (junction band + rest): texel rule alone (v8) {ja} + {ra}, islands AND texel rule (v9) {jb} + {rb}");
        if excluded {
            assert!(ra > 20, "{name}: the scene proves something (the texel rule alone leaks): {ra}");
            assert_eq!(rb, 0, "{name}: an excluded island grows nothing on any layer");
            // At the junction only the coat's own strands remain (the part's triangles have no shells at all; one key per pixel,
            // so where the part's own base strands are gone the coat's strands there win those pixels: the count is reported, not
            // bounded by v8's).
        } else {
            assert!(a > 100 && b as f64 >= 0.9 * a as f64, "{name}: the ear (tapered, fur texels) keeps its fur: {a} -> {b}");
        }
    }
    let (n0, k0) = isl_coat(&off, W, H);
    let (n1, k1) = isl_coat(&on, W, H);
    println!("coat: {k0} of {n0} px with keys (v8), {k1} of {n1} (v9)");
    assert!(n0 > 5000 && n1 == n0);
    assert!(k1 as f64 >= 0.98 * k0 as f64, "the coat keeps >= 98 %: {k1} of {k0}");
}

#[test]
fn v9_a_pending_or_failed_island_analysis_is_the_v8_output_bit_for_bit() {
    let _g = crate::testlock::lock();
    let base = Opts { cloth: false, island_mesh: true, ..close() };
    let frames = [[0.0, 0.0]; 3];
    let Some(off) = run_seq(&ShellCfg::default(), &Opts { islands: 0, ..base }, &frames) else { return };
    let Some(pending) = run_seq(&ShellCfg::default(), &Opts { islands: 2, ..base }, &frames) else { return };
    let Some(failed) = run_seq(&ShellCfg::default(), &Opts { islands: 3, ..base }, &frames) else { return };
    for k in 0..frames.len() {
        println!("frame {k}: pending: {} / failed: {}", pending[k].isl, failed[k].isl);
        assert!(off[k].keys.iter().any(|&x| x != 0));
        assert!(pending[k].keys == off[k].keys && pending[k].out == off[k].out, "pending = v8, frame {k}");
        assert!(failed[k].keys == off[k].keys && failed[k].out == off[k].out, "failed = v8, frame {k}");
    }
    assert!(pending[2].isl.contains("pending"), "{}", pending[2].isl);
    assert!(failed[2].isl.contains("failed"), "{}", failed[2].isl);
}

#[test]
fn v9_the_near_dilation_mirror_and_the_capture_shader_link() {
    let _g = crate::testlock::lock();
    // The capture geometry shader compiles and links against the game's real character layouts (like the shell one).
    for (name, sig) in real_layouts() {
        let uv = crate::furmask::uv_pick(&sig).unwrap();
        let b = build_gs(&sig, uv, None).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(!b.tri.is_empty(), "{name}");
    }
    // texel_features_near: a root 3 texels from a flat patch (x >= 40) is fur for v8 (its FT_DIL 3 windows are mixed, never all flat)
    // and not fur when near (the window 6 texels away lies wholly on the patch).
    let mut px = vec![[0u8; 4]; 64 * 64];
    for y in 0..64 {
        for x in 0..64 {
            let h = ((x * 7919 + y * 104_729) % 97) as f32 / 97.0;
            let c = (200.0 + 40.0 * (h - 0.5)) as u8;
            px[y * 64 + x] = if x >= 40 { [128, 122, 115, 105] } else { [c, c, (c as f32 * 0.95) as u8, 107] };
        }
    }
    let img = crate::furprobe::Img { w: 64, h: 64, px };
    let cells: Vec<([f32; 4], f32)> = (0..20).map(|k| texel_features(&img, [(4.0 + k as f32) / 64.0, 0.5])).collect();
    let coat = coat_stats(&cells);
    let at = [37.5 / 64.0, 32.5 / 64.0];
    let w8 = { let (f, hh) = texel_features_dilated(&img, at); texel_weight(f, hh, coat.0, coat.1) };
    let w9 = { let (f, hh) = texel_features_near(&img, at); texel_weight(f, hh, coat.0, coat.1) };
    println!("6 texels from a flat horn: v8 {w8:.3}, v9 near {w9:.3}");
    assert!(w8 > 0.9 && w9 < 0.05, "{w8} {w9}");
}

// ---- v11: the old fur look and the rim only on proven fur skin (the skin pass, t25) ----------------------------------------------

/// v11: (pixels of `sel`, how many have skin 0, how many have no key, how many of those the v10 look changed (> 0.004 from the picture
/// without the look), how many of those differ between the v11 output and that picture at all).
fn v11_count(r: &Out, sel: &dyn Fn(usize) -> bool) -> (usize, usize, usize, usize, usize) {
    let (mut n, mut s0, mut nk, mut v10, mut v11) = (0, 0, 0, 0, 0);
    let d = |a: [f32; 3], b: [f32; 3]| (a[0] - b[0]).abs().max((a[1] - b[1]).abs()).max((a[2] - b[2]).abs());
    for i in 0..r.out.len() {
        if !sel(i) {
            continue;
        }
        n += 1;
        s0 += (r.skin.get(i).copied().unwrap_or(1.0) == 0.0) as usize;
        if r.keys[i] == 0 {
            nk += 1;
            v10 += (d(r.v10[i], r.plain[i]) > 0.004) as usize;
            v11 += (r.out[i] != r.plain[i]) as usize;
        }
    }
    (n, s0, nk, v10, v11)
}

#[test]
fn v11_a_hair_card_draw_gets_no_old_fur_look_no_shells_and_is_left_as_vanilla() {
    // Seen in the game: the hair cards (count 17694, gloss 0.54..0.56: "not fur (armour / skin / hair), no shells") were left to the old
    // fur look's strand modulation, which drew a plaid at the cards' uv scale. v11: the skin pass proves nothing there (the draw test),
    // so the old look does not run: the picture is the one without the fur look, bit for bit.
    let _g = crate::testlock::lock();
    let Some(r) = run(&ShellCfg::default(), &Opts { cloth: false, hair: true, ..close() }) else { return };
    let body = |i: usize| r.body[i];
    let (n, s0, nk, v10, v11) = v11_count(&r, &body);
    println!("v11 hair card: {n} px, skin 0 on {s0}, without a key {nk}; the v10 old look changed {v10} of them, v11 {v11}");
    assert!(!r.skin.is_empty(), "t25 bound");
    assert!(r.keys.iter().all(|&k| k == 0), "a hair draw grows no shell");
    assert!(n > 20000 && s0 == n && nk == n);
    assert!(v10 > n / 4, "the scene proves the old look painted the hair (the plaid): {v10}");
    assert_eq!(v11, 0, "v11: the hair is the picture without the old look");
    assert!(r.out == r.plain, "and nothing else changed anywhere");
}

#[test]
fn v11_the_cut_out_holes_of_an_alpha_tested_draw_get_no_old_fur_look_and_the_coat_keeps_it() {
    // An alpha-tested fur draw (stripes discarded by the engine's pixel shader): the fur mask draws it SOLID, the occlusion check keeps
    // the holes (the final depth is farther than the stamp), so v10 painted the old strands onto the sky seen through the holes (and onto
    // whatever lies behind a hair card). v11: the skin pass needs the root to BE the visible surface: the holes get 0, the coat ~1.
    let _g = crate::testlock::lock();
    let Some(r) = run(&ShellCfg::default(), &Opts { cloth: false, cutout: true, ..close() }) else { return };
    let holes = |i: usize| r.mask[i][3] > 0.5 && r.lin[i] >= 0.99;
    let (n, s0, nk, v10, v11) = v11_count(&r, &holes);
    let coat = |i: usize| r.body[i];
    let (nc, c0, _, _, _) = v11_count(&r, &coat);
    let fur_skin = (0..r.out.len()).filter(|&i| r.body[i] && r.skin[i] > 0.9).count();
    println!("v11 cut-out: {n} hole px in the mask, skin 0 on {s0}; without a key {nk}: v10 changed {v10}, v11 {v11}; coat {nc} px, skin > 0.9 on {fur_skin}, 0 on {c0}");
    assert!(n > 2000, "the scene has holes inside the mask: {n}");
    assert_eq!(s0, n, "no hole is proven fur");
    assert!(v10 > nk / 4, "v10 painted the holes: {v10} of {nk}");
    assert_eq!(v11, 0, "v11 leaves every hole without a strand as the picture without the old look");
    assert!(fur_skin as f64 >= 0.95 * nc as f64, "the visible coat is proven fur: {fur_skin} of {nc}");
}

#[test]
fn v11_excluded_islands_get_no_old_fur_look_and_no_rim() {
    // The horn: v10 excluded it from the shells (an island), and the old fur look filled exactly those pixels with its
    // strands (the zebra stripes). v11: the skin pass applies the island bits: the horn and the claws are 0, the coat and the ear 1.
    let _g = crate::testlock::lock();
    let base = Opts { cloth: false, island_mesh: true, ..close() };
    let Some(mut seq) = run_seq(&ShellCfg::default(), &Opts { islands: 1, ..base }, &[[0.0, 0.0]; 5]) else { return };
    let on = seq.pop().unwrap();
    assert!(on.isl.contains("ready:"), "{}", on.isl);
    let part = |r: &Out, rect: [f32; 4], i: usize| {
        let m = r.mask[i];
        r.body[i] && m[2] > 0.5 && region_d(rect, m[0], m[1]) > 0.008 && (rect[3] - m[1]) / (rect[3] - rect[1]) >= 0.2
    };
    let mut changed_v10 = 0usize;
    for (name, _, _, _, rect, excluded) in ISL_PARTS {
        let (n, s0, nk, v10, v11) = v11_count(&on, &|i| part(&on, rect, i));
        let fur = (0..on.out.len()).filter(|&i| part(&on, rect, i) && on.skin[i] > 0.5).count();
        println!("v11 {name}: {n} px, skin 0 on {s0} (> 0.5 on {fur}), without a key {nk}: v10 changed {v10}, v11 {v11}");
        if excluded {
            assert!(n > 20);
            assert_eq!(s0, n, "{name}: an excluded island is never proven fur");
            assert_eq!(v11, 0, "{name}: no old look on it");
            changed_v10 += v10;
        } else {
            assert!(fur as f64 >= 0.9 * n as f64, "{name}: the ear keeps its fur skin: {fur} of {n}");
        }
    }
    assert!(changed_v10 > 50, "the scene proves the old look painted the excluded parts in v10: {changed_v10}");
    let (n, k) = isl_coat(&on, W, H);
    let fur = (0..on.out.len()).filter(|&i| {
        let m = on.mask[i];
        on.body[i] && m[2] > 0.5 && m[0] > 0.02 && m[0] < 0.48 && on.skin[i] > 0.5
    }).count();
    println!("v11 coat: {n} px ({k} with keys), proven fur {fur}");
    assert!(fur as f64 >= 0.95 * n as f64);
}

#[test]
fn v11_the_skin_pass_constants_and_a_frame_without_records() {
    // The skin pass's geometry constants are the shells' with g5.x = 1, whatever the shell count.
    let c = ShellCfg::default();
    let g = DrawGeom { vp: [0.0, 0.0, 3440.0, 1440.0], ccw: false };
    let a = gs_cb_values(&c, &g, 3.0, 1000.0, 46.0);
    let b = skin_gs_cb_values(&c, &g, 3.0, 1000.0, 46.0);
    assert_eq!(a[..5], b[..5]);
    assert_eq!(a[5], [0.0; 4]);
    assert_eq!(b[5], [1.0, 0.0, 0.0, 0.0]);
    assert_eq!(SKIN_SRV_SLOT, 25);
    assert!(crate::hdr_shaders::LIGHT_ADJUST_HLSL.contains("register(t25)"));
    assert!(HLSL.contains(&format!("SKIN_UV_TOL = {SKIN_UV_TOL}")), "the shader's uv tolerance is the Rust one");
    // The texts.
    let f = Frame { skin: 3, skin_bound: SKIN_TARGET, ..Frame::default() };
    assert!(gate_of(&f).contains("skin pass of 3 recorded draws"));
    assert!(gate_of(&Frame { skin_bound: SKIN_NONE, ..Frame::default() }).contains("no draw recorded"));
    assert!(gate_of(&Frame::default()).contains("not bound"));
    // With the shells off there is no classification: the gate line says so (the old look / rim on the whole mask).
    let _g = crate::testlock::lock();
    configure(&ShellCfg { on: 0, ..ShellCfg::default() });
    assert!(gate_text().contains("fur_shell off"), "{}", gate_text());
}

// ---- v13: coat strands in front of the horn ----------------------------------------------------------------------------------------

#[test]
fn v13_coat_strands_in_front_of_the_horn_keep_their_colour_and_the_horn_grows_none() {
    // Seen in the game: where the head / neck fur lies IN FRONT of the horn, its strands vanished. The composite's detail test held every
    // strand over a fur-draw pixel to the colour of the surface under it (made for the eyes, the nose, the markings: "the horns stay
    // clear of the white fur's strands"), also when that surface is the horn far BEHIND the strand. v13: ps_shell flags the fragment
    // (KEY_FRONT) when the surface under it lies clearly behind its root; the composite then treats it like a strand over the background.
    // Scene: the v9 island mesh (its horn an excluded island: grey bands) and a coat "arm" of the same draw in front of the horn's middle.
    let _g = crate::testlock::lock();
    let horn_rect = ISL_PARTS[0].4;
    // the horn's middle (object n x 1.27 at 0.5 m per unit, the body at 1.4 m), seen along its ray at 1.0 m
    let n = n3(ISL_PARTS[0].1);
    let mid = [n[0] * 1.27 * 0.5, n[1] * 1.27 * 0.5, 1.4 + n[2] * 1.27 * 0.5];
    let k = 1.0 / mid[2];
    let base = Opts { cloth: false, island_mesh: true, islands: 1, arm: Some([mid[0] * k, mid[1] * k, 1.0, 0.03]), ..close() };
    let Some(mut seq) = run_seq(&ShellCfg::default(), &base, &[[0.0, 0.0]; 5]) else { return };
    let r = seq.pop().unwrap();
    assert!(r.isl.contains("ready:"), "{}", r.isl);
    png("v13-horn-front", &r.out, W, H, None);
    let (w, h) = (W as i64, H as i64);
    let (mut over, mut flagged, mut painted, mut sky, mut sky_painted) = (0usize, 0usize, 0usize, 0usize, 0usize);
    // On the horn's GREY bands (the picture without shells far from the white root): how far the pixel moved towards the root's colour
    // (0 = the strand is invisible, 1 = the root colour), the mean over those pixels.
    let (mut grey, mut toward) = (0usize, 0.0f64);
    for i in 0..(W * H) as usize {
        let Some((hh, _, d)) = unpack_key(r.keys[i]) else { continue };
        if hh <= 0.0 {
            continue;
        }
        let (x, y) = ((i as i64 % w) as f32 + 0.5 + d[0], (i as i64 / w) as f32 + 0.5 + d[1]);
        if x < 0.0 || y < 0.0 || x >= w as f32 || y >= h as f32 {
            continue;
        }
        let a = (y as i64 * w + x as i64) as usize;
        if !r.arm[a] || r.arm[i] {
            continue; // a strand of the arm, outside the arm's own pixels
        }
        let changed = (0..3).any(|c| (r.out[i][c] - r.old[i][c]).abs() > 0.02);
        let m = r.mask[i];
        if r.body[i] && m[2] > 0.5 && region_d(horn_rect, m[0], m[1]) > 0.008 {
            over += 1;
            flagged += (r.keys[i] & KEY_FRONT != 0) as usize;
            painted += changed as usize;
            let root = r.scene[a];
            let gap: f32 = (0..3).map(|c| (root[c] - r.old[i][c]).powi(2)).sum();
            if gap > 0.1 * 0.1 * 3.0 {
                grey += 1;
                toward += ((0..3).map(|c| (r.out[i][c] - r.old[i][c]) * (root[c] - r.old[i][c])).sum::<f32>() / gap) as f64;
            }
        } else if !r.body[i] {
            sky += 1;
            sky_painted += changed as usize;
        }
    }
    let toward = toward / grey.max(1) as f64;
    println!("v13: arm strands over the horn behind it: {over} px, KEY_FRONT on {flagged}, painted (> 0.02 from the picture without shells) {painted}; on its grey bands {grey} px moved {:.0} % of the way to the root colour; over the sky {sky} px, painted {sky_painted}", toward * 100.0);
    assert!(over > 40, "the scene proves something: {over}");
    assert!(flagged as f64 >= 0.95 * over as f64, "the fragments over the horn behind are flagged: {flagged} of {over}");
    assert!(painted as f64 >= 0.6 * over as f64, "and painted like over the sky ({sky_painted} of {sky}): {painted} of {over}");
    assert!(grey > 30 && toward > 0.25, "the strands show over the grey horn: {grey} px, {toward:.2}");
    // The horn itself still grows nothing (its island is excluded).
    // (keys anchored on a horn pixel away from the arm's silhouette: an arm root at its edge can anchor on the horn pixel beside it)
    let near_arm = grown(&r.arm, W, H, 3);
    let mut horn_rooted = 0usize;
    for i in 0..(W * H) as usize {
        let Some((_, _, d)) = unpack_key(r.keys[i]) else { continue };
        let (x, y) = ((i as i64 % w) as f32 + 0.5 + d[0], (i as i64 / w) as f32 + 0.5 + d[1]);
        if x < 0.0 || y < 0.0 || x >= w as f32 || y >= h as f32 {
            continue;
        }
        let a = (y as i64 * w + x as i64) as usize;
        let m = r.mask[a];
        if !near_arm[a] && m[2] > 0.5 && region_d(horn_rect, m[0], m[1]) > 0.008 && (horn_rect[3] - m[1]) / (horn_rect[3] - horn_rect[1]) >= 0.2 {
            horn_rooted += 1;
        }
    }
    assert_eq!(horn_rooted, 0, "no strand rooted on the horn");
    // Strands of the body over the body's own coat: flagged only towards the limb (the sphere curves away more than 2 cm + 0.6 % under
    // the strand); on the face of the coat (where the eyes and markings are) the detail test still runs.
    let near_body_edge = {
        let not_body: Vec<bool> = r.body.iter().map(|&b| !b).collect();
        grown(&not_body, W, H, 12)
    };
    let (mut cc, mut cf, mut cf_face) = (0usize, 0usize, 0usize);
    for i in 0..(W * H) as usize {
        let Some((_, _, d)) = unpack_key(r.keys[i]) else { continue };
        let (x, y) = ((i as i64 % w) as f32 + 0.5 + d[0], (i as i64 / w) as f32 + 0.5 + d[1]);
        if x < 0.0 || y < 0.0 || x >= w as f32 || y >= h as f32 {
            continue;
        }
        let a = (y as i64 * w + x as i64) as usize;
        if r.arm[a] || r.arm[i] || !r.body[i] || !(r.mask[i][2] > 0.5 && r.mask[i][0] > 0.02 && r.mask[i][0] < 0.48) {
            continue;
        }
        cc += 1;
        let f = r.keys[i] & KEY_FRONT != 0;
        cf += f as usize;
        cf_face += (f && !near_body_edge[i]) as usize;
    }
    println!("v13: body strands over the body's coat: {cc} px, flagged {cf} (more than 12 px inside the silhouette: {cf_face})");
    assert!(cc > 5000 && cf_face * 1000 <= cc, "the face of the coat keeps the detail test: {cf_face}");
    assert!(HLSL.contains(&format!("FRONT_DEPTH_ABS = {FRONT_DEPTH_ABS:.1}")) && HLSL.contains(&format!("FRONT_DEPTH_REL = {FRONT_DEPTH_REL:.1}")));
    assert_eq!(unpack_key(pack_key(0.5, 1.0, [3.0, -2.0]) | KEY_FRONT), unpack_key(pack_key(0.5, 1.0, [3.0, -2.0])));
}

// ---- v17: the close camera (first person) and the view angle -------------------------------------------------------------------

/// v17: strand keys (h > 0) on the body's own pixels and over the body, per body pixel.
fn v17_density(r: &Out) -> (usize, usize, f64) {
    let body = r.body.iter().filter(|&&b| b).count();
    let keys = r.keys.iter().zip(&r.body).filter(|(k, b)| **b && unpack_key(**k).is_some_and(|(h, _, _)| h > 0.0)).count();
    (body, keys, keys as f64 / body.max(1) as f64)
}

#[test]
fn v17_triangles_reaching_behind_the_camera_keep_their_strands() {
    // First person (seen in the game: the fur vanished with the view angle, mostly in first person): with the camera at / inside the
    // character, its large triangles reach behind the camera. v1..v16 dropped such a triangle WHOLE (load_tri: a vertex with w <= 0),
    // so the fur nearest the eye came and went with the angle. v17 clips it at the near plane and grows shells on its visible part
    // (measured: the coarse card 0.218 strand keys per px with the v16 rule, 0.921 with v17; the fine card 0.95 both).
    // Scene: a 1 x 1 m card nearly along the view (|cos| 0.25), its centre 0.3 m away: it reaches from behind the camera to 0.78 m.
    // Fine (32 x 32 quads: few triangles cross) vs coarse (2 x 2 quads: the near ones all cross): the same strand density.
    let _g = crate::testlock::lock();
    let base = Opts { cloth: false, dist: 0.3, card: Some(0.25), ..close() };
    let Some(fine) = run(&ShellCfg::default(), &Opts { card_n: Some(32), ..base }) else { return };
    let Some(coarse) = run(&ShellCfg::default(), &Opts { card_n: Some(2), ..base }) else { return };
    let (bf, kf, df) = v17_density(&fine);
    let (bc, kc, dc) = v17_density(&coarse);
    png("v17-coarse-card-behind-camera", &coarse.out, W, H, None);
    println!("v17 card through the camera plane: fine {bf} px / {kf} keys ({df:.3}), coarse {bc} px / {kc} keys ({dc:.3})");
    assert!(bc > 20000 && bf > 20000, "the scene shows the card: {bf} {bc}");
    assert!(dc >= 0.75 * df, "the coarse card keeps its strands where its triangles reach behind the camera: {dc:.3} vs {df:.3}");
}

#[test]
fn v17_close_camera_and_angles_keep_the_strands() {
    let _g = crate::testlock::lock();
    for dist in [1.4f32, 0.9, 0.7, 0.62, 0.58] {
        let Some(r) = run(&ShellCfg::default(), &Opts { cloth: false, dist, ..close() }) else { return };
        let (b, k, d) = v17_density(&r);
        println!("v17 sphere at {dist} m (surface {:.2} m): body {b} px, strand keys {k} ({:.3} per px), replayed {}", dist - 0.5, d, r.replayed);
        assert!(b < 10000 || d >= 0.6, "sphere at {dist} m: the strands stay ({d:.3} per px)");
    }
    for c in [0.9f32, 0.6, 0.4, 0.25] {
        for dist in [1.4f32, 0.4, 0.2] {
            let Some(r) = run(&ShellCfg::default(), &Opts { cloth: false, dist, card: Some(c), ..close() }) else { return };
            let (b, k, d) = v17_density(&r);
            println!("v17 card |cos| {c} at {dist} m: body {b} px, strand keys {k} ({:.3} per px)", d);
            assert!(b < 10000 || d >= 0.6, "close camera / angle |cos| {c} at {dist} m: the strands stay ({d:.3} per px)");
        }
    }
}


#[test]
fn v17_far_strands_take_the_coat_colour_not_the_background() {
    // Seen in the game: a far (zoomed out) tail could show dark marks along its lower edge. The composite read the root colour
    // bilinearly at the strand's anchor; at a silhouette, and at a distance where an anchor rounds onto the next pixel, that mixed the
    // dark floor into the strands. v17: only the fur pixels among the four contribute. A white coat over a near-black background at
    // 4 / 6 m: strand pixels over the background must look like the coat, not like the background.
    let _g = crate::testlock::lock();
    for dist in [4.0f32, 6.0] /* (10 m: past the LOD's fade, no strand over the background at all) */ {
        let Some(r) = run(&ShellCfg::default(), &Opts { cloth: false, dist, sky: [0.01, 0.01, 0.01], ..close() }) else { return };
        let (mut n, mut dark) = (0usize, 0usize);
        let mut sum = 0.0f64;
        for i in 0..r.out.len() {
            if r.body[i] || r.keys[i] == 0 || r.out[i] == r.old[i] {
                continue;
            }
            // a painted strand pixel over the background: its brightness relative to the coat it should have
            n += 1;
            let l = lum(r.out[i]) as f64;
            sum += l;
            dark += (l < 0.02) as usize;
        }
        let body_l: f64 = r.scene.iter().zip(&r.body).filter(|(_, b)| **b).map(|(c, _)| lum(*c) as f64).sum::<f64>() / r.body.iter().filter(|&&b| b).count().max(1) as f64;
        println!("v17 far {dist} m: {n} strand px over the background, mean luminance {:.3} (the coat {:.3}), {dark} nearly black", sum / n.max(1) as f64, body_l);
        assert!(n > 20, "the scene proves something");
        // (measured at 6 m: the plain bilinear root 0.067 mean / 23 nearly black of 108, v17 0.074 / 22: the synthetic sphere has no dark
        // underside, so the change is small here; the bound keeps the soft tips over a black background from being counted as marks)
        assert!(dark * 4 <= n, "{dark} of {n} strand px over the background are background-dark");
    }
}
