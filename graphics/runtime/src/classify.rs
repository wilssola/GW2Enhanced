//! Classifies a frame's passes into the known scene stages.

use crate::passes::{Pass, Target, FMT_B8G8R8A8_UNORM, FMT_R16G16B16A16_FLOAT, FMT_R32_FLOAT};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SceneMap {
    pub opaque: usize,
    pub depth_linear: usize,
    pub effects: usize,
    pub luminance: usize,
    pub light_adjust: usize,
    pub bloom: Vec<usize>,
    pub combine: usize,
    pub ui: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClassifyError {
    pub reason: String,
}

impl std::fmt::Display for ClassifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reason)
    }
}

impl std::error::Error for ClassifyError {}

fn err<T>(r: &str) -> Result<T, ClassifyError> {
    Err(ClassifyError { reason: r.to_string() })
}

fn is_size(t: &Target, (w, h): (u32, u32)) -> bool {
    t.w == w && t.h == h
}

/// Scene colour targets are 8-bit normally and float once the float scene upgrade is active.
fn is_scene_colour(fmt: u32) -> bool {
    fmt == FMT_B8G8R8A8_UNORM || fmt == FMT_R16G16B16A16_FLOAT
}

fn rt0_scene(p: &Pass, size: (u32, u32)) -> bool {
    p.rt.first().is_some_and(|t| is_scene_colour(t.fmt) && is_size(t, size))
}

fn rt0_is(p: &Pass, fmt: u32, size: (u32, u32)) -> bool {
    p.rt.first().is_some_and(|t| t.fmt == fmt && is_size(t, size))
}

fn full_depth(p: &Pass, screen: (u32, u32)) -> bool {
    p.depth.as_ref().is_some_and(|d| is_size(d, screen))
}

/// Minimum draws of the opaque pass (the reference frame has 932; quiet scenes still have dozens).
const MIN_OPAQUE_DRAWS: u32 = 20;
/// Minimum draws of the UI pass. The final `afdc14cb` pass has the same targets with exactly one
/// draw, so a frame whose UI is hidden does not match.
const MIN_UI_DRAWS: u32 = 2;

/// Everything up to and including the combine pass (the UI pass is found after it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stages {
    pub opaque: usize,
    pub depth_linear: usize,
    pub effects: usize,
    pub luminance: usize,
    pub light_adjust: usize,
    pub bloom: Vec<usize>,
    pub combine: usize,
}

/// Full-size scene colour, no depth, one draw: the shape of the combine pass and of the copy that
/// follows the SMAA pass right before the UI.
pub fn is_copy_shape(p: &Pass, screen: (u32, u32)) -> bool {
    rt0_scene(p, screen) && p.depth.is_none() && p.draws == 1
}

/// Finds the stages through `combine` in `frame` (which may be a prefix of a frame).
pub fn stages_through_combine(frame: &[Pass], screen: (u32, u32)) -> Result<Stages, ClassifyError> {
    let find = |from: usize, f: &dyn Fn(&Pass) -> bool| (from..frame.len()).find(|&i| f(&frame[i]));

    let Some(opaque) = find(0, &|p| rt0_scene(p, screen) && full_depth(p, screen) && p.draws >= MIN_OPAQUE_DRAWS)
    else {
        return err("no opaque pass");
    };
    let Some(depth_linear) = find(opaque + 1, &|p| rt0_is(p, FMT_R32_FLOAT, screen)) else {
        return err("no linear depth pass");
    };
    // Reading: "same RT and depth as opaque" compares format and size of RT0 and of depth;
    // "next pass" is the first such pass after depth_linear. It is identified structurally, not by
    // its draw count.
    let (o_rt, o_ds) = (frame[opaque].rt[0], frame[opaque].depth);
    let Some(effects) = find(depth_linear + 1, &|p| p.rt.first() == Some(&o_rt) && p.depth == o_ds && p.draws >= 1)
    else {
        return err("no effects pass");
    };
    let Some(luminance) = find(0, &|p| rt0_is(p, FMT_R16G16B16A16_FLOAT, (1, 1))) else {
        return err("no luminance pass");
    };
    let no_depth_full = |p: &Pass| rt0_scene(p, screen) && p.depth.is_none();
    let Some(light_adjust) = find(luminance + 1, &no_depth_full) else {
        return err("no light adjust pass");
    };
    let quarter = (screen.0 / 4, screen.1 / 4);
    let bloom: Vec<usize> = (luminance + 1..frame.len())
        .filter(|&i| frame[i].rt.first().is_some_and(|t| is_size(t, quarter)))
        .collect();
    let Some(&last_bloom) = bloom.last() else {
        return err("no bloom passes");
    };
    // Reading: combine is searched after the last bloom pass (end of the bloom chain).
    let Some(combine) = find(last_bloom + 1, &|p| is_copy_shape(p, screen)) else {
        return err("no combine pass");
    };
    Ok(Stages { opaque, depth_linear, effects, luminance, light_adjust, bloom, combine })
}

/// The first copy-shaped pass after `combine` (pass #17 of the reference frame): the UI pass starts
/// right after it.
pub fn copy_pass_after(frame: &[Pass], combine: usize, screen: (u32, u32)) -> Option<usize> {
    (combine + 1..frame.len()).find(|&i| is_copy_shape(&frame[i], screen))
}

/// Where a pass that starts now sits relative to the scene stages the AO needs (see `docs/m2-ao.md`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum AoStage {
    #[default]
    None,
    /// The first full-size `R32_FLOAT` pass after the opaque pass: the engine's linear depth (#4).
    DepthLinear,
    /// The effects pass (#9): same colour target and depth as the opaque pass, after the linear depth.
    /// The AO is applied to its RT0 just before it starts drawing.
    Effects,
}

/// Classifies the pass `cur` (just bound, not drawn yet) given the passes completed before it. Same
/// structural rules as `stages_through_combine` (opaque, then linear depth, then effects), evaluated on
/// a prefix of the frame; never uses pass indices.
pub fn ao_stage(frame_so_far: &[Pass], cur: &Pass, screen: (u32, u32)) -> AoStage {
    let cur_rt0 = cur.rt.first();
    let cand_depth = cur_rt0.is_some_and(|t| t.fmt == FMT_R32_FLOAT && is_size(t, screen));
    let cand_effects = cur_rt0.is_some_and(|t| is_scene_colour(t.fmt) && is_size(t, screen)) && full_depth(cur, screen);
    if !cand_depth && !cand_effects {
        return AoStage::None;
    }
    let find = |from: usize, f: &dyn Fn(&Pass) -> bool| (from..frame_so_far.len()).find(|&i| f(&frame_so_far[i]));
    let Some(opaque) = find(0, &|p| rt0_scene(p, screen) && full_depth(p, screen) && p.draws >= MIN_OPAQUE_DRAWS) else {
        return AoStage::None;
    };
    let depth_linear = find(opaque + 1, &|p| rt0_is(p, FMT_R32_FLOAT, screen));
    match depth_linear {
        None => {
            if cand_depth { AoStage::DepthLinear } else { AoStage::None }
        }
        Some(dl) => {
            let (o_rt, o_ds) = (frame_so_far[opaque].rt[0], frame_so_far[opaque].depth);
            let seen_effects = find(dl + 1, &|p| p.rt.first() == Some(&o_rt) && p.depth == o_ds && p.draws >= 1).is_some();
            if !seen_effects && cur.rt.first() == Some(&o_rt) && cur.depth == o_ds { AoStage::Effects } else { AoStage::None }
        }
    }
}

pub fn classify(frame: &[Pass], screen: (u32, u32)) -> Result<SceneMap, ClassifyError> {
    let s = stages_through_combine(frame, screen)?;
    // The UI pass is structural: the pass right after the copy that follows combine, full-size scene
    // colour with full-size depth. With the UI hidden nothing like that follows, and the frame does
    // not classify (the gate turns off).
    let ui = copy_pass_after(frame, s.combine, screen)
        .map(|c| c + 1)
        .filter(|&i| frame.get(i).is_some_and(|p| rt0_scene(p, screen) && full_depth(p, screen) && p.draws >= MIN_UI_DRAWS));
    let Some(ui) = ui else {
        return err("no ui pass");
    };
    Ok(SceneMap { opaque: s.opaque, depth_linear: s.depth_linear, effects: s.effects, luminance: s.luminance, light_adjust: s.light_adjust, bloom: s.bloom, combine: s.combine, ui })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::passes::parse_frame_log;

    const FIXTURE: &str = crate::frame_fixture::FRAME_PASSES;
    const SCREEN: (u32, u32) = (3440, 1440);

    #[test]
    fn fixture_frame_has_21_passes() {
        assert_eq!(parse_frame_log(FIXTURE).len(), 21);
    }

    #[test]
    fn classifies_fixture() {
        let m = classify(&parse_frame_log(FIXTURE), SCREEN).unwrap();
        assert_eq!(
            m,
            SceneMap { opaque: 3, depth_linear: 4, effects: 9, luminance: 12, light_adjust: 13, bloom: vec![14], combine: 15, ui: 18 }
        );
    }

    #[test]
    fn error_displays_its_reason() {
        let e = classify(&[], SCREEN).unwrap_err();
        assert_eq!(e.to_string(), "no opaque pass");
        let _: &dyn std::error::Error = &e;
    }

    #[test]
    fn wrong_screen_size_fails() {
        assert!(classify(&parse_frame_log(FIXTURE), (1920, 1080)).is_err());
    }

    #[test]
    fn missing_luminance_fails() {
        let mut f = parse_frame_log(FIXTURE);
        f.remove(12);
        assert!(classify(&f, SCREEN).is_err());
    }

    #[test]
    fn loading_like_frame_fails() {
        let t = Target { w: 64, h: 64, fmt: FMT_B8G8R8A8_UNORM };
        let f: Vec<Pass> = (0..3).map(|_| Pass { rt: vec![t], depth: None, draws: 5, dispatches: 0 }).collect();
        assert!(classify(&f, SCREEN).is_err());
    }

    fn to_float(p: Pass, screen: (u32, u32)) -> Pass {
        let quarter = (screen.0 / 4, screen.1 / 4);
        let cv = |t: Target| {
            let sized = (t.w == screen.0 && t.h == screen.1) || (t.w == quarter.0 && t.h == quarter.1);
            Target { fmt: if t.fmt == FMT_B8G8R8A8_UNORM && sized { FMT_R16G16B16A16_FLOAT } else { t.fmt }, ..t }
        };
        Pass { rt: p.rt.into_iter().map(cv).collect(), depth: p.depth, ..p }
    }

    #[test]
    fn classifies_fixture_after_float_upgrade() {
        let f: Vec<Pass> = parse_frame_log(FIXTURE).into_iter().map(|p| to_float(p, SCREEN)).collect();
        assert!(f.iter().any(|p| p.rt.first().is_some_and(|t| t.fmt == FMT_R16G16B16A16_FLOAT && t.w == SCREEN.0)));
        let expected = classify(&parse_frame_log(FIXTURE), SCREEN).unwrap();
        assert_eq!(classify(&f, SCREEN).unwrap(), expected);
    }

    #[test]
    fn luminance_still_found_when_scene_targets_are_float() {
        let f: Vec<Pass> = parse_frame_log(FIXTURE).into_iter().map(|p| to_float(p, SCREEN)).collect();
        assert_eq!(classify(&f, SCREEN).unwrap().luminance, 12);
    }

    fn overlay_pass() -> Pass {
        Pass { rt: vec![Target { w: SCREEN.0, h: SCREEN.1, fmt: FMT_B8G8R8A8_UNORM }], depth: None, draws: 5, dispatches: 0 }
    }

    #[test]
    fn quiet_scene_still_classifies() {
        let mut f = parse_frame_log(FIXTURE);
        f[3].draws = 30; // opaque
        f[9].draws = 3; // effects
        f[18].draws = 5; // ui
        let m = classify(&f, SCREEN).unwrap();
        assert_eq!(m, SceneMap { opaque: 3, depth_linear: 4, effects: 9, luminance: 12, light_adjust: 13, bloom: vec![14], combine: 15, ui: 18 });
    }

    #[test]
    fn hidden_ui_frame_does_not_classify() {
        // UI hidden: the UI pass (#18) and the passes of the small UI targets (#19) are gone, only
        // the final pass (#20, R8G8B8A8 with the full-size depth) follows the copy.
        let mut f = parse_frame_log(FIXTURE);
        f.drain(18..20);
        assert_eq!(classify(&f, SCREEN).unwrap_err().to_string(), "no ui pass");
        // Same in scRGB, where the final pass renders to the float back buffer (one draw).
        let f: Vec<Pass> = f.into_iter().map(|mut p| {
            if p.rt.first().is_some_and(|t| t.fmt == 28) { p.rt[0].fmt = FMT_R16G16B16A16_FLOAT; }
            p
        }).collect();
        assert!(classify(&f, SCREEN).is_err());
        // And with nothing at all after the copy pass.
        let mut f = parse_frame_log(FIXTURE);
        f.truncate(18);
        assert!(classify(&f, SCREEN).is_err());
    }

    #[test]
    fn classifies_with_leading_overlay_pass() {
        let mut f = vec![overlay_pass()];
        f.extend(parse_frame_log(FIXTURE));
        let m = classify(&f, SCREEN).unwrap();
        assert_eq!(m, SceneMap { opaque: 4, depth_linear: 5, effects: 10, luminance: 13, light_adjust: 14, bloom: vec![15], combine: 16, ui: 19 });
    }

    #[test]
    fn other_aspect_ratio_works() {
        let sc = |t: Target| Target { w: (t.w * 1920 / 3440).max(1), h: (t.h * 1080 / 1440).max(1), fmt: t.fmt };
        let f: Vec<Pass> = parse_frame_log(FIXTURE)
            .into_iter()
            .map(|p| Pass { rt: p.rt.into_iter().map(sc).collect(), depth: p.depth.map(sc), ..p })
            .collect();
        let m = classify(&f, (1920, 1080)).unwrap();
        assert_eq!((m.opaque, m.ui, m.bloom), (3, 18, vec![14]));
    }

    #[test]
    fn ao_stage_finds_linear_depth_and_effects_on_the_fixture() {
        let f = parse_frame_log(FIXTURE);
        for i in 0..f.len() {
            let s = ao_stage(&f[..i], &f[i], SCREEN);
            match i {
                4 => assert_eq!(s, AoStage::DepthLinear, "#4"),
                9 => assert_eq!(s, AoStage::Effects, "#9"),
                _ => assert_eq!(s, AoStage::None, "#{i}"),
            }
        }
    }

    #[test]
    fn ao_stage_survives_float_upgrade_and_leading_overlay_pass() {
        let mut f: Vec<Pass> = parse_frame_log(FIXTURE).into_iter().map(|p| to_float(p, SCREEN)).collect();
        f.insert(0, overlay_pass());
        let got: Vec<usize> = (0..f.len()).filter(|&i| ao_stage(&f[..i], &f[i], SCREEN) != AoStage::None).collect();
        assert_eq!(got, vec![5, 10]);
    }

    #[test]
    fn ao_stage_needs_the_structure_and_fires_once() {
        let f = parse_frame_log(FIXTURE);
        assert_eq!(ao_stage(&f[..5], &f[4], SCREEN), AoStage::None); // linear depth already seen
        assert_eq!(ao_stage(&f[..10], &f[3], SCREEN), AoStage::None); // opaque binding again after the effects pass
        assert_eq!(ao_stage(&f[..3], &f[4], SCREEN), AoStage::None); // no opaque pass before
        assert_eq!(ao_stage(&[], &f[4], SCREEN), AoStage::None);
        assert_eq!(ao_stage(&f[..5], &f[5], SCREEN), AoStage::None); // half-size R32_FLOAT
        assert_eq!(ao_stage(&f[..4], &f[4], (1920, 1080)), AoStage::None); // wrong screen size
        let mut g = f.clone();
        g.remove(4); // without the linear depth pass the effects pass is not recognised
        assert_eq!(ao_stage(&g[..8], &g[8], SCREEN), AoStage::None);
        let t = Target { w: 64, h: 64, fmt: FMT_B8G8R8A8_UNORM };
        let l = Pass { rt: vec![t], depth: None, draws: 5, dispatches: 0 };
        assert_eq!(ao_stage(&[l.clone(), l.clone()], &l, SCREEN), AoStage::None);
    }
}
