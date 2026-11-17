//! Tests of the content rule (furselect.rs): which draws of other pixel shaders than `fur_ps` are charr candidates, the per-atlas proof
//! and the mixed (skin + armour) draws of the strand weight.

use super::*;
use crate::shellfur::{GLOSS_BAND, MIXED_BAND, draw_weight, mixed_weight, strand_weight};

const BC3: u32 = 77;
const BC5: u32 = 83;

fn t(fmt: u32, s: u32) -> Option<Tex> {
    Some(Tex { fmt, w: s, h: s })
}

#[test]
fn fur_auto_admits_a_skinned_character_composite_of_any_pixel_shader() {
    // The verified charr shaders' draws (1d50763f / 5d00827f): skinned, t0 BC3 4096 / 2048, t1 BC5.
    assert_eq!(admit(16464, t(BC3, 4096), t(BC5, 4096), false), Ok(()));
    assert_eq!(admit(16464, t(BC3, 2048), t(BC5, 2048), false), Ok(()));
    // Separately textured parts (1024 colour, 256..512 normal) and the srgb / typeless views.
    assert_eq!(admit(16464, t(BC3, 1024), t(BC5, 256), false), Ok(()));
    assert_eq!(admit(16496, t(78, 1024), t(82, 512), false), Ok(()));
    // Not a composite: no texture, the colour without alpha (BC1) or BC7, the normal in another slot, tiny textures.
    assert!(admit(16464, None, t(BC5, 2048), false).is_err());
    assert!(admit(16464, t(71, 2048), t(BC5, 2048), false).is_err(), "BC1 has no gloss alpha");
    assert!(admit(16464, t(98, 512), t(BC5, 128), false).is_err(), "BC7 face / eye material");
    assert!(admit(16464, t(BC3, 2048), None, false).is_err());
    assert!(admit(16464, t(BC3, 2048), t(BC3, 2048), false).is_err());
    assert!(admit(16464, t(BC3, 256), t(BC5, 256), false).is_err());
    assert!(admit(16464, t(BC3, 1024), t(BC5, 64), false).is_err());
    assert!(admit(16464, Some(Tex { fmt: BC3, w: 0, h: 0 }), t(BC5, 1024), false).is_err(), "a buffer view");
}

#[test]
fn fur_auto_takes_rigid_draws_only_on_a_proven_charr_atlas() {
    // The rigid 208-byte body / head draws (vs 59f8b692) of NPC charr: their shape alone is every static prop's.
    assert!(admit(208, t(BC3, 2048), t(BC5, 2048), false).is_err());
    assert_eq!(admit(208, t(BC3, 2048), t(BC5, 2048), true), Ok(()));
    assert!(admit(4095, t(BC3, 2048), t(BC5, 2048), false).is_err());
    assert_eq!(admit(MIN_SKINNED_VCB, t(BC3, 2048), t(BC5, 2048), false), Ok(()));
}

/// Row 0 of a draw's coat statistics: (band share, median flatness, cells, strict weight).
fn s0(band: f32, flat: f32, cells: f32) -> [f32; 4] {
    let mut s = [band, flat, cells, 0.0];
    s[3] = draw_weight(s);
    s
}

#[test]
fn fur_atlas_is_proven_by_one_fur_draw_and_covers_its_mixed_draws() {
    let mut b = Book::new();
    b.tick();
    let (charr, human) = (0x1000usize, 0x2000usize);
    // A partly armoured charr: the head all fur (100 % in the band), the body with its armour 25 %, hair cards 2 %. A human composite:
    // skin 30 % in the band at most (the measured human atlases 1..30 %).
    let ch = b.note(&[(charr, s0(1.0, 0.11, 74.0)), (charr, s0(0.25, 0.12, 140.0)), (charr, s0(0.02, 0.18, 51.0)), (human, s0(0.30, 0.05, 90.0))]);
    assert!(b.proven(charr));
    assert!(!b.proven(human));
    assert_eq!(ch.len(), 2, "both atlases measured for the first time");
    assert!(ch.iter().any(|c| c.0 == charr && c.2 && c.3));
    // The content rule: the charr atlas admitted, the human one skipped until the recheck.
    assert_eq!(b.verdict(charr), Ok(()));
    assert!(b.verdict(human).is_err());
    for _ in 0..=RECHECK_FRAMES {
        b.tick();
    }
    assert_eq!(b.verdict(human), Ok(()), "measured again after RECHECK_FRAMES");
    // An unknown atlas: admitted (to be measured).
    assert_eq!(b.verdict(0x3000), Ok(()));
    assert!(!b.proven(0x3000));
}

#[test]
fn fur_atlas_proof_holds_while_the_head_is_off_screen_then_expires() {
    let mut b = Book::new();
    b.tick();
    let a = 0x4000usize;
    b.note(&[(a, s0(1.0, 0.11, 74.0)), (a, s0(0.2, 0.1, 120.0))]);
    assert!(b.proven(a));
    // Only the body in view (the camera close on the arms): still proven.
    for _ in 0..100 {
        b.tick();
        assert!(b.note(&[(a, s0(0.2, 0.1, 120.0))]).is_empty(), "no change while the proof holds");
    }
    assert!(b.proven(a));
    for _ in 0..PROOF_FRAMES {
        b.tick();
        b.note(&[(a, s0(0.2, 0.1, 120.0))]);
    }
    assert!(!b.proven(a), "no fur draw on it for PROOF_FRAMES: another character may own the pointer now");
    // Draws without a footprint never prove or disprove anything.
    let c = 0x5000usize;
    assert!(b.note(&[(c, [1.0, 0.1, 3.0, 1.0])]).is_empty());
    assert!(!b.proven(c));
    assert!(b.note(&[(0, s0(1.0, 0.1, 80.0))]).is_empty(), "no atlas bound");
}

#[test]
fn fur_atlas_book_drops_stale_entries_and_stays_bounded() {
    let mut b = Book::new();
    b.tick();
    let v: Vec<(usize, [f32; 4])> = (1..=(MAX_ATLASES + 50)).map(|k| (k * 16, s0(1.0, 0.1, 80.0))).collect();
    b.note(&v);
    assert_eq!(b.map.len(), MAX_ATLASES);
    for _ in 0..(STALE_FRAMES + 128) {
        b.tick();
    }
    assert!(b.map.is_empty());
}

/// Texel features as `ft_feat_dil` gives them: (gloss, saturation, value, flatness).
const FUR: [f32; 4] = [0.42, 0.10, 0.70, 0.08];
const ARMOUR_GLOSSY: [f32; 4] = [0.62, 0.20, 0.50, 0.06];
const ARMOUR_MATTE: [f32; 4] = [0.20, 0.30, 0.40, 0.05];
const HORN_FLAT: [f32; 4] = [0.42, 0.10, 0.70, 0.0];

#[test]
fn fur_mixed_draw_of_a_proven_atlas_gets_fur_on_its_fur_texels_only() {
    // A body with its armour: 25 % of the footprint in the band, the median gloss the armour's.
    let s = s0(0.25, 0.06, 140.0);
    let s1 = [0.60, 0.2, 0.5, 30.0];
    assert_eq!(draw_weight(s), 0.0, "the draw test alone refuses it (v17: the body bare)");
    assert_eq!(strand_weight(FUR, 30.0, s, s1, true, false), 0.0, "not proven: no fur (a human body stays bare)");
    let fur = strand_weight(FUR, 30.0, s, s1, true, true);
    assert!(fur > 0.95, "proven atlas: fur on the fur texels ({fur})");
    assert!(strand_weight(ARMOUR_GLOSSY, 30.0, s, s1, true, true) < 0.01);
    assert!(strand_weight(ARMOUR_MATTE, 30.0, s, s1, true, true) < 0.01);
    assert!(strand_weight(HORN_FLAT, 30.0, s, s1, true, true) < 0.01, "the flat test still runs");
    // Texel tests off (shell_fur_mask=off): the band alone.
    assert!(strand_weight(FUR, 30.0, s, s1, false, true) > 0.95);
    assert!(strand_weight(ARMOUR_GLOSSY, 30.0, s, s1, false, true) < 0.01);
    // A draw of the same atlas with (almost) nothing in the band: hair cards, an armour piece.
    assert_eq!(mixed_weight(s0(0.02, 0.18, 51.0)), 0.0);
    assert_eq!(strand_weight(FUR, 30.0, s0(0.02, 0.18, 51.0), s1, true, true), 0.0);
    assert_eq!(mixed_weight(s0(MIXED_BAND.1, 0.1, 40.0)), 1.0);
    assert_eq!(mixed_weight([1.0, 0.1, 8.0, 0.0]), 0.0, "no footprint");
}

#[test]
fn fur_a_full_fur_draw_keeps_its_v17_weight_with_or_without_the_mixed_flag() {
    // The head (100 % in the band, the coat's own medians): the mixed flag changes nothing.
    let s = s0(1.0, 0.11, 74.0);
    let s1 = [0.40, 0.02, 0.70, 21.0];
    for ft in [FUR, ARMOUR_GLOSSY, HORN_FLAT, [0.45, 0.4, 0.6, 0.05]] {
        let a = strand_weight(ft, 30.0, s, s1, true, false);
        let b = strand_weight(ft, 30.0, s, s1, true, true);
        assert_eq!(a, b, "{ft:?}");
        assert_eq!(a, draw_weight(s) * crate::shellfur::texel_weight(ft, 30.0, s, s1));
    }
    assert!(GLOSS_BAND.0 < FUR[0] && FUR[0] < GLOSS_BAND.1);
}

#[test]
fn fur_the_shader_carries_the_mixed_rule_of_its_mirror() {
    let h = crate::shellfur::HLSL;
    assert!(h.contains("float ft_mixed_weight(float4 s0)"));
    assert!(h.contains(&format!("smoothstep({:.2}, {:.2}, s0.x)", MIXED_BAND.0, MIXED_BAND.1)));
    assert!(h.contains(&format!("MIXED_COAT_GLOSS = {:.2};", crate::shellfur::MIXED_COAT_GLOSS)));
    assert!(h.contains("q4.w > 0.5 ? ft_mixed_weight(s0)"));
}

#[test]
fn fur_auto_key_parses_saves_and_defaults_on() {
    let d = crate::config::Config::default();
    assert_eq!(d.shell.auto, 1);
    assert_eq!(crate::config::Config::parse("fur_auto=0").shell.auto, 0);
    assert_eq!(crate::config::Config::parse("fur_auto=7").shell.auto, 1);
    assert_eq!(crate::config::Config::parse("fur_auto=x").shell.auto, 1);
    let t = d.to_ini();
    assert_eq!(t.lines().filter(|l| *l == "fur_auto=1").count(), 1);
    assert!(t.contains("# fur_auto 1 (default) = every charr"));
}

#[test]
fn fur_select_log_lines_are_once_per_group() {
    let k = GroupKey { ps: 0xabad_cafe, vs: Some(1), t0: Tex { fmt: BC3, w: 2048, h: 2048 }, t1_fmt: BC5, verdict: "x" };
    assert!(first_time(k));
    assert!(!first_time(k));
    assert!(first_time(GroupKey { verdict: "y", ..k }));
}

#[test]
fn fur_trials_are_limited_per_frame_so_a_crowd_does_not_get_shells_at_once() {
    let mut b = Book::new();
    b.tick();
    // A crowd of unmeasured atlases: only MAX_TRIALS_PER_FRAME get the shells this frame, the same ones may draw again.
    let crowd: Vec<usize> = (1..=MAX_TRIALS_PER_FRAME + 2).map(|i| i * 0x1000).collect();
    let ok: Vec<bool> = crowd.iter().map(|&a| b.verdict(a).is_ok()).collect();
    assert_eq!(ok.iter().filter(|o| **o).count(), MAX_TRIALS_PER_FRAME);
    assert_eq!(b.verdict(crowd[0]), Ok(()), "a second draw of an atlas on trial");
    assert_eq!(b.verdict(crowd[MAX_TRIALS_PER_FRAME + 1]), Err("waiting for its turn to be measured"));
    // The next frame, the waiting ones get their turn.
    b.tick();
    assert_eq!(b.verdict(crowd[MAX_TRIALS_PER_FRAME + 1]), Ok(()));
}

#[test]
fn fur_a_trial_without_a_measurement_is_tried_again_later_and_never_counts_as_not_fur() {
    let mut b = Book::new();
    b.tick();
    let a = 0x4000usize;
    assert_eq!(b.verdict(a), Ok(()));
    for _ in 0..=TRIAL_FRAMES {
        b.tick();
        let _ = b.verdict(a);
    }
    assert_eq!(b.verdict(a), Err("waiting for its turn to be measured"), "no readback measured it during its trial: it waits");
    for _ in 0..RETRY_FRAMES {
        b.tick();
    }
    assert_eq!(b.verdict(a), Ok(()), "on trial again (its shaders were still compiling the first time)");
    b.note(&[(a, s0(1.0, 0.11, 74.0))]);
    assert!(b.proven(a), "and proven by the measurement of the second trial");
    // A fur readback during a later trial proves it.
    let mut b = Book::new();
    b.tick();
    assert_eq!(b.verdict(a), Ok(()));
    b.note(&[(a, s0(1.0, 0.11, 74.0))]);
    assert!(b.proven(a));
    for _ in 0..TRIAL_FRAMES * 2 {
        b.tick();
        assert_eq!(b.verdict(a), Ok(()), "a proven atlas is not on trial");
    }
}

#[test]
fn fur_mixed_draw_keeps_horns_and_claws_bare() {
    // A body drawn with its armour on a proven atlas; a tan keratin texel (horn, claw) in the gloss band, saturated against a greyish
    // coat: the colour tests keep it bare, as on a full fur draw.
    let s = s0(0.25, 0.06, 140.0);
    let s1 = [0.60, 0.2, 0.5, 30.0];
    let keratin = [0.40, 0.45, 0.70, 0.10];
    assert!(strand_weight(keratin, 35.0, s, s1, true, true) < 0.01, "horn / claw keratin on a mixed draw");
    assert!(strand_weight(FUR, 30.0, s, s1, true, true) > 0.95, "the coat keeps its fur");
}

#[test]
fn fur_leather_or_scales_never_prove_an_atlas_and_unproven_draws_get_no_strands() {
    let mut b = Book::new();
    b.tick();
    let (leather, charr) = (0x5000usize, 0x6000usize);
    // A leathery / scaly body: 60 % of its footprint in the fur gloss band (the most measured on leather), textured.
    b.note(&[(leather, s0(0.60, 0.08, 120.0))]);
    assert!(!b.proven(leather), "60 % in the band is leather, not fur");
    // Two draws, each passing one bar only, do not add up to a proof.
    b.note(&[(leather, [0.74, 0.08, 120.0, 0.9]), (leather, [0.90, 0.08, 120.0, 0.1])]);
    assert!(!b.proven(leather));
    // A charr's fur: 84 % and more.
    b.note(&[(charr, s0(0.84, 0.08, 120.0))]);
    assert!(b.proven(charr));
    // Strands: a fur_ps draw always (its draw test decides); a content-rule draw only on a proven atlas.
    assert!(crate::shellfur::shells_allowed(false, leather));
}

#[test]
fn fur_grey_horn_on_a_light_coat_stays_bare_and_the_coat_keeps_its_fur() {
    use crate::shellfur::texel_weight_opt;
    // A light coat: gloss 0.42, saturation 0.08, value 0.85, hue 30.
    let s0 = s0(0.9, 0.08, 200.0);
    let s1 = [0.42, 0.08, 0.85, 30.0];
    // Its grey horn: same gloss band, streaked (not flat), no colour, value 0.38.
    let horn = [0.42, 0.06, 0.38, 0.06];
    assert!(texel_weight_opt(horn, 30.0, s0, s1, true) < 0.05, "a grey horn gets no strands");
    // The coat itself, with its usual shading (0.70..0.95), keeps them.
    for v in [0.70f32, 0.85, 0.95] {
        assert!(texel_weight_opt([0.42, 0.08, v, 0.06], 30.0, s0, s1, true) > 0.9, "coat value {v}");
    }
}

#[test]
fn fur_a_crowd_shares_one_strand_budget() {
    use crate::shellfur::{FRAME_SHELL_BUDGET, SHARE_MIN_SHELLS, shell_share};
    assert_eq!(shell_share(16, 1, 0), 16, "one character: the full count");
    assert!((0..96).all(|k| shell_share(16, 96, k) == 16), "96 draws fit the budget");
    // A crowd: the nearest draws (first in the engine's near-to-far order) keep the full count, the far ones drop to the minimum.
    assert_eq!(shell_share(32, 100, 0), 32, "the nearest: full");
    assert_eq!(shell_share(32, 100, 99), SHARE_MIN_SHELLS, "the farthest: the minimum");
    assert!((1..100).all(|k| shell_share(32, 100, k) <= shell_share(32, 100, k - 1)), "never more for a farther draw");
    assert!((0..100).map(|k| shell_share(32, 100, k)).sum::<u32>() <= FRAME_SHELL_BUDGET, "the frame stays in the budget");
    assert_eq!(shell_share(16, 200, 0), SHARE_MIN_SHELLS, "past the budget everyone keeps the minimum (fewer show bands)");
    assert_eq!(shell_share(8, 3, 2), 8, "fewer shells wanted than the minimum: what is wanted");
}
