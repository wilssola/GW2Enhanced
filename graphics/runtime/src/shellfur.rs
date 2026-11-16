//! Shell fur (docs/shell-fur.md): real strands that stick out of the silhouette, darker at the roots, lit like the body
//! underneath, with a fuzzy back-lit edge and a little wind / gravity.
//!
//! HOW. The fur mask (`furmask.rs`) already re-issues the selected fur draws of the opaque pass. A masked draw that the SHELL selector
//! also picks (`shell_ps` / `shell_vs` / `shell_min_vcb` / `shell_counts`: real fur, not the cloth armour that shares the pixel shader)
//! is RECORDED (`on_fur_draw`): its vertex shader, input layout, vertex / index buffers, rasterizer state, viewport, the draw call and a
//! GPU COPY of its vertex constant buffers (bgfx rewrites the same buffer for every draw, the copy keeps this draw's bone matrices).
//! At the start of the light-adjust draw (`on_light_adjust`, the data-path rule: the result only enters the picture through the
//! substituted light-adjust shader), once the frame's depth is final, every record is drawn again into the mod's own R32_FLOAT target
//! with the engine's vertex shader, a GEOMETRY shader of the mod generated per vertex-output signature (`gs_hlsl`; it extrudes each
//! triangle `fur_shells` times along its normal, instance 0 being the base layer) and the strand pixel shader (`shaders/shellfur.hlsl`).
//! The target keeps, per pixel, the packed fragment of the highest shell (MAX blend): its height and the offset to its root (the
//! anchor). `light_adjust_hdr.hlsl` reads it as `t24` (+ `b13`) and paints the strand with the body colour at the anchor, darker at the
//! root, with a back light against a bright background.
//!
//! Occlusion (v7, the transparency fix): no hardware depth test; a strand is drawn only when its ROOT is the surface the engine's depth
//! shows at the root's pixel (the composite takes the strand's colour from there), and none of its fragments lies over anything nearer
//! than its root (5 mm + 0.3 %): a cape, a bracer, an arm or another character in front hides the strands, the sky and the body behind
//! do not. Records whose vertex / index buffers are DYNAMIC (could be rewritten before the replay) are not recorded at all.
//! The fur discriminator (v7) runs per strand at its own root uv: the draw's coat statistics (`ps_feat` / `ps_stats`, a uv-space pass
//! over the draw's own atlas) decide whether the draw is fur at all (charr skin, not armour or human skin) and which texels are not fur
//! (a flat horn, glossy or tan claws, the fang, red lips) relative to that coat, for a coat of any colour.
//!
//! Cost: per recorded draw about 40 COM calls and one constant-buffer copy at the draw, the same again at the replay; the GPU draws the
//! fur meshes once more with up to 32 geometry-shader instances per triangle, only up to `shell_lod_distance` (fewer shells farther).
//! No per-frame allocation (fixed-capacity vectors), shaders compiled on background threads, never on the render thread. Fail open:
//! any failure leaves the old `fur_look` alone and logs once.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Mutex, OnceLock};

use crate::d3d::{IgnoreScope, Obj, StateGuard, Viewport, release, vfn, vt};
use crate::furmask::{SigElem, UvPick, VsSig};
use crate::shafts::{NO_TARGET, Target, make_target};
use crate::{log, shaders};

pub const HLSL: &str = include_str!("../shaders/shellfur.hlsl");
pub const PS_ENTRY: &str = "ps_shell";
pub const GS_ENTRY: &str = "gs_shell";
/// The unit measurement (one geometry shader per vertex layout like `gs_shell`, its pixel shader `ps_measure`).
pub const MEASURE_GS_ENTRY: &str = "gs_measure";
pub const MEASURE_PS_ENTRY: &str = "ps_measure";
/// v7, the per-draw coat statistics: the uv-space footprint (one geometry shader per vertex layout, `gs_uv`), its feature pixel shader,
/// and the reduction (a full-screen triangle into the statistics target).
pub const UV_GS_ENTRY: &str = "gs_uv";
pub const FEAT_PS_ENTRY: &str = "ps_feat";
pub const STATS_VS_ENTRY: &str = "vs_stats";
pub const STATS_PS_ENTRY: &str = "ps_stats";
/// v9: the stream-output capture of the triangles (one point per primitive: furisland.rs reads it back).
pub const TRI_GS_ENTRY: &str = "gs_tri";
/// v11: the skin pass (the recorded draws' base layer, `gs_shell` with `g5.x = 1`): per pixel how much the visible surface is proven fur
/// skin (draw weight x texel weight x shell_uv_exclude; excluded islands, hidden roots and pixels whose fur mask holds another draw's uv
/// give 0). The old fur look and the rim light are gated by it (`t25` of the light-adjust substitute).
pub const SKIN_PS_ENTRY: &str = "ps_skin";
/// `t25` of the light-adjust substitute: the skin target (R8_UNORM, screen size), or a 1x1 zero texture when the shells are on but no
/// record was drawn this frame (no proven fur: no old look, no rim). Unbound (the shells off / not ready) = the v10 behaviour (the mask
/// alone).
pub const SKIN_SRV_SLOT: u32 = 25;
/// DXGI_FORMAT_R8_UNORM.
const SKIN_FORMAT: u32 = 61;
/// The largest uv difference (wrapping) between the fur mask's uv and the skin pass's own uv at a pixel (`SKIN_UV_TOL` in shellfur.hlsl):
/// the same vertex output rasterised twice differs by float rounding (and the mask's 16-bit storage, 1.5e-5); another draw's uv does not.
#[allow(dead_code)] // the shader has its own copy; a test keeps the two equal
pub const SKIN_UV_TOL: f32 = 0.004;
/// Cells across a record's uv-space footprint (`FT_N` in shellfur.hlsl): the feature target is FEAT_N x (FEAT_N x MAX_RECS).
pub const FEAT_N: u32 = 32;
/// DXGI_FORMAT_R32G32B32A32_FLOAT (the features and the statistics).
const F32X4: u32 = 2;
/// `t24` / `b13` of the light-adjust substitute: the shell target (R32_FLOAT, packed keys) and the composite constants.
pub const SRV_SLOT: u32 = 24;
pub const CB_SLOT: u32 = 13;
/// DXGI_FORMAT_R32_FLOAT.
const SHELL_FORMAT: u32 = 41;
/// Records per frame (the charr is 3..6 draws).
/// v16: 256 (v14: 64, v13: 24). Black Citadel frames have 47..247 fur draws (one frame: 136, 72 of them
/// past v15's cap of 64, chosen by the engine's draw order: which character lost its fur changed with the camera angle, screenshots
/// 86 / 87). Every fur draw of those frames fits now; the LOD ends the far ones in the geometry shader at once.
/// v19: 96. With fur_auto every charr NPC of a crowd gets the passes (stats, shells, skin): 247 draws x 16 shells stalled the game.
/// The engine draws the characters near to far, so the cap keeps the near ones; the hysteresis keeps the choice stable.
pub const MAX_RECS: usize = 96;
/// v16: the triangles all records of a frame may hold together (v14: 400000; the crowd frames record 60k for 64 draws, ~250k for 247).
/// Past it, or past MAX_RECS, a draw that had shells in the previous frame takes the slot of one that did not (`Shell::victim`): the
/// choice stays stable across frames instead of following the draw order.
pub const MAX_FRAME_TRIS: u32 = 300_000; // v19 (v16: 800000)
/// v14: a DYNAMIC vertex / index buffer is copied at the draw (a mod-owned DEFAULT buffer, pooled per record) when it is at most this
/// big; the frame's copies together at most DYN_FRAME_BYTES. Larger: refused as before (fail open).
pub const DYN_MAX_BYTES: u32 = 4 << 20;
pub const DYN_FRAME_BYTES: u64 = 32 << 20;
/// The largest number of shells (geometry-shader instances).
pub const MAX_SHELLS: u32 = 32;
/// The largest extrusion of a strand on the screen, in pixels (a guard against a wrong unit: never a fur ball).
pub const MAX_PX: f32 = 64.0;
/// Smallest strand cell on the screen (px): denser strands would be sub-pixel noise, the lattice coarsens instead.
pub const MIN_CELL_PX: f32 = 2.0;
/// Narrowest strand on the screen (px): a thinner strand is drawn this wide and fainter by the same ratio (soft coverage instead of a
/// sub-pixel strand that blinks on and off when it moves by a fraction of a pixel).
pub const MIN_STRAND_PX: f32 = 1.0;
/// Share of a strand pulled towards its clump at `shell_clump` 0.5 (the default; 1 = twice that).
pub const CLUMP: f32 = 0.35;
/// The lean of a clump's strands at their tips (cells) at `shell_clump` 1 (directional clumping, "combed" fur).
pub const LEAN: f32 = 0.25;
/// v7, the strand fragment's occlusion test (the transparency fix): the fragment is hidden where the scene at its pixel is nearer than
/// the fragment ITSELF by more than FRAG_TOL_M (m) + FRAG_REL_TOL x its depth. The fragment's depth is measured from the root's VISIBLE
/// linear depth (only the short extrusion uses the unit), so the tolerance can be small: a bracer or a cape lying 1 cm on the fur hides
/// the strands under it.
pub const FRAG_TOL_M: f32 = 0.005;
pub const FRAG_REL_TOL: f32 = 0.003;
/// v7, the root's visibility at its anchor (the strand takes its colour from the screen there): the engine's device depth at the anchor
/// within ROOT_DEV_TOL (+ twice the local slope of the root's depth per pixel); a depth format no shader can read: the linear depth
/// within ROOT_LIN_REL x depth + ROOT_LIN_M (m), the measured unit's error.
pub const ROOT_DEV_TOL: f32 = 2e-5;
pub const ROOT_LIN_REL: f32 = 0.02;
pub const ROOT_LIN_M: f32 = 0.015;
/// v8: no shells on a triangle seen edge-on: |cos(face normal, view ray)| below FACE_LO, full from FACE_HI (shellfur.hlsl): a flat
/// card, strip or hem at a grazing angle drew a hard-edged comb / rectangle of parallel strands.
pub const FACE_LO: f32 = 0.08;
pub const FACE_HI: f32 = 0.20;
/// v8: the flat (horn) and glossy (claw) texel tests also read the 5x5 windows this many texels away diagonally (at the ~1024 level):
/// the fur of a root within ~3 texels of a horn or a claw goes too (the residue at the horn's edge and base, the paw claws' rim).
pub const FT_DIL: i64 = 3;
/// Gravity of the strand tips (share of the strand length).
pub const GRAVITY: f32 = 0.25;
/// tan(vertical half fov) the view space is rebuilt with (the AO's assumption).
pub const TAN_HALF_FOV: f32 = crate::ao::TAN_HALF_FOV;

// ---- config -----------------------------------------------------------------------------------------------

/// The shell-fur keys of config.ini (`Config::shell`).
#[derive(Clone, Debug, PartialEq)]
pub struct ShellCfg {
    /// `fur_shells` 0, 4..32 (16): layers, the base layer included; 0 = off.
    pub shells: u32,
    /// `shell_length` cm (0.5..20, 2.5; v9 and before 4): strand length.
    pub length: f32,
    /// `shell_density` 50..4000 (1200): strand cells per uv unit at the densest level.
    pub density: f32,
    /// `shell_thickness` 0.1..1 (0.6): strand diameter at the root, share of a cell.
    pub thickness: f32,
    /// `shell_root_dark` 0..1 (0.6): self-shadowing of the coat: deep fragments (roots, the skin between the strands) darker, the tips
    /// brighter, the average of the coat kept (shellfur composite in light_adjust_hdr.hlsl).
    pub root_dark: f32,
    /// `shell_wind` 0..1 (0.15): wind sway of the tips.
    pub wind: f32,
    /// `shell_lod_distance` m (2..100, 12.5; was 20 with 4 cm strands): no shells beyond, shorter and fainter strands from half of it
    /// (continuous).
    pub lod: f32,
    /// `shell_rim` 0..2 (1): back light of the strands against a bright background.
    pub rim: f32,
    /// `shell_ps`: pixel shaders of the shell draws (a subset of `fur_ps`; empty = every fur draw).
    pub ps: String,
    /// `shell_vs`: vertex shaders the shell draws must use (empty = any).
    pub vs: String,
    /// `shell_min_vcb` bytes (4096): smallest vertex constant buffer 0 (skinned meshes only; the rigid 208-byte draw is not fur).
    pub min_vcb: u32,
    /// `shell_counts`: index (vertex) counts per draw to take (`17694`) or to leave out (`-5214`); empty = all.
    pub counts: String,
    /// `shell_normal`: `face` (default, the triangle normal) or `TEXCOORDn` (a vertex output with a per-vertex normal).
    pub normal: String,
    /// `shell_unit` engine units per metre (0 = auto from the linear depth, default; 0.01..1000 fixed).
    pub unit: f32,
    /// `shell_debug` 0/1: paints the shell pixels (red root .. yellow tip).
    pub debug: u8,
    /// `shell_detail` 0..1 (0.7): over the body a strand only paints where the surface under it looks like its root, so the eyes, the
    /// nose, the mouth, markings and horns are not smeared over by the fur around them (0 = strands paint everywhere).
    pub detail: f32,
    /// `shell_clump` 0..1 (0.5): clumping of the strands (pulled towards a shared point and leaning the same way, "combed").
    pub clump: f32,
    /// `shell_fur_mask` (v3, the fur discriminator): which channel of which engine texture of the fur draw says "this texel is fur":
    /// `off`, or (v5) up to 4 tests `t<slot 0..3>.<r|g|b|a|l|d|s|f>` followed by `<` (fur where the value is BELOW the threshold) or
    /// `>` (above) and optionally the test's own threshold (else `shell_fur_threshold`), separated by commas; every test must say fur.
    /// Default `t0.a<, t0.f>0.025`: the gloss alpha (v3) and the colour flatness (v5, the horn of the real atlas is a flat colour; see
    /// `FUR_MASK_DEFAULT`). Applied in the fur mask (furmask.rs): the shells, the old fur look, the rim light and the magenta debug
    /// all follow it.
    pub fur_mask: String,
    /// `shell_fur_threshold` 0..1 (0.66): the value where the fur ends.
    pub fur_threshold: f32,
    /// `shell_fur_soft` 0.01..0.5 (0.05): half width of the soft transition around the threshold.
    pub fur_soft: f32,
    /// `shell_uv_exclude`: up to 16 rectangles of the mesh uv (the engine's uv of the fur draw) without fur, `u0 v0 u1 v1` each, `;`
    /// between them, an optional `@count` limits one to the draw with that index count. Soft edge `UV_FEATHER`.
    pub uv_exclude: String,
    /// `fur_auto` 0/1 (1, v18): the content rule of furselect.rs: skinned draws of OTHER pixel shaders than `fur_ps` that sample a
    /// character composite (BC3 colour + BC5 normal) also go to the fur mask and the shells, where the draw test decides; draws on a
    /// proven charr atlas that are only partly fur (skin + armour) get fur on their fur texels. 0 = only the `fur_ps` draws (v17).
    pub auto: u8,
}

pub const SHELLS_DEFAULT: u32 = 16;
pub const LENGTH_RANGE: (f32, f32) = (0.5, 20.0);
/// v10: 2.5 cm (was 4: the strands were too long).
pub const LENGTH_DEFAULT: f32 = 2.5;
pub const DENSITY_RANGE: (f32, f32) = (50.0, 4000.0);
pub const DENSITY_DEFAULT: f32 = 1200.0;
pub const THICKNESS_RANGE: (f32, f32) = (0.1, 1.0);
pub const THICKNESS_DEFAULT: f32 = 0.6;
pub const ROOT_DARK_DEFAULT: f32 = 0.6;
pub const WIND_DEFAULT: f32 = 0.15;
pub const DETAIL_DEFAULT: f32 = 0.7;
pub const CLUMP_DEFAULT: f32 = 0.5;
pub const LOD_RANGE: (f32, f32) = (2.0, 100.0);
/// v10: 12.5 m = the 20 m of the 4 cm strands scaled with the strand length (2.5 / 4): the strands start to shorten and fade, and
/// end, at the same size on the screen as before.
pub const LOD_DEFAULT: f32 = 20.0 * LENGTH_DEFAULT / 4.0;
pub const RIM_RANGE: (f32, f32) = (0.0, 2.0);
pub const RIM_DEFAULT: f32 = 1.0;
pub const UNIT_RANGE: (f32, f32) = (0.01, 1000.0);
/// v14: 0 (was 4096: skinned draws only). The rigid 208-byte draws (vs 59f8b692) are NPC charr bodies and heads too (count 1824 / 1140:
/// 28 % of a crowd frame's fur pixels); the hair cards among them are rejected by the per-draw test (gloss band) like every draw. A saved
/// 4096 (the old default) reads as 0 (`Selector::new`); any other value is honoured.
pub const MIN_VCB_DEFAULT: u32 = 0;
/// The pre-v14 default of `shell_min_vcb`, saved in every config of v1..v13: treated as "no limit" (v14).
pub const MIN_VCB_LEGACY: u32 = 4096;

impl Default for ShellCfg {
    fn default() -> Self {
        ShellCfg {
            shells: SHELLS_DEFAULT,
            length: LENGTH_DEFAULT,
            density: DENSITY_DEFAULT,
            thickness: THICKNESS_DEFAULT,
            root_dark: ROOT_DARK_DEFAULT,
            wind: WIND_DEFAULT,
            lod: LOD_DEFAULT,
            rim: RIM_DEFAULT,
            ps: String::new(),
            vs: String::new(),
            min_vcb: MIN_VCB_DEFAULT,
            counts: String::new(),
            normal: "face".to_string(),
            unit: 0.0,
            debug: 0,
            detail: DETAIL_DEFAULT,
            clump: CLUMP_DEFAULT,
            fur_mask: FUR_MASK_DEFAULT.to_string(),
            fur_threshold: FUR_THRESHOLD_DEFAULT,
            fur_soft: FUR_SOFT_DEFAULT,
            uv_exclude: String::new(),
            auto: 1,
        }
    }
}

/// Every key, in the order of config.ini.
pub const KEYS: [&str; 22] = [
    "fur_shells", "shell_length", "shell_density", "shell_thickness", "shell_root_dark", "shell_wind", "shell_lod_distance", "shell_rim", "shell_ps", "shell_vs", "shell_min_vcb",
    "shell_counts", "shell_normal", "shell_unit", "shell_debug", "shell_detail", "shell_clump", "shell_fur_mask", "shell_fur_threshold", "shell_fur_soft", "shell_uv_exclude", "fur_auto",
];

// ---- the fur discriminator (v3): which texels of a fur draw are fur (not horn, claw, nose, lips) -------------------------------

/// v5 default: the gloss test of v3 AND the colour flatness (docs/shell-fur.md "v5"): in a real charr atlas the horn is a flat dye
/// colour (relative rgb detail 0.000 at the 1024 mip over 5x5 texels) while the painted fur never is (head fur p1 0.025, body fur p1
/// 0.034; clipped white counts as fur), so `t0.f>0.025` removes the horn and keeps the coat, whatever its dye.
pub const FUR_MASK_DEFAULT: &str = "t0.a<, t0.f>0.025";
pub const FUR_THRESHOLD_DEFAULT: f32 = 0.66;
pub const FUR_SOFT_DEFAULT: f32 = 0.05;
pub const FUR_SOFT_RANGE: (f32, f32) = (0.01, 0.5);
/// Engine texture slots the fur mask's shader can read (it declares t0..t3).
pub const FUR_MASK_SLOTS: u32 = 4;
/// Rectangles of `shell_uv_exclude`.
pub const MAX_UV_RECTS: usize = 16;
/// Soft edge of an excluded rectangle (uv; 16 texels of a 4096 atlas).
pub const UV_FEATHER: f32 = 0.004;
/// Constant-buffer slot of the fur mask's pixel shader for the discriminator (`fd_cb_values`), bound during the mask draw only.
pub const FD_CB_SLOT: u32 = 13;
/// Tests of `shell_fur_mask` (v5: a list, every test must say "fur": the weights multiply).
pub const MAX_FUR_TESTS: usize = 4;
/// float4s of that constant buffer: [0] test 1, [1] its threshold / soft / rectangles, [2..18] the rectangles, [18] the draw tag
/// (`FD_TAG`), [19..22] tests 2..4 (like [0]), [22] their thresholds.
pub const FD_LEN: usize = 3 + MAX_UV_RECTS + MAX_FUR_TESTS;
/// Index of the draw tag: `.x` = what the mask writes into `b` for this draw (0 = 1.0, as before): `furprobe::tag_value` (the draw's
/// index in the frame, for `shell_debug=2` and the debug capture's per-draw probe).
pub const FD_TAG: usize = 2 + MAX_UV_RECTS;
/// First of the extra tests (2..4) and the float4 of their thresholds.
pub const FD_MORE: usize = FD_TAG + 1;
pub const FD_MORE_TH: usize = FD_MORE + MAX_FUR_TESTS - 1;
/// Channel ids past r g b a l: `d` = the local detail of the texel's .rg (a normal map's bumpiness: noisy fur strands vs smooth horn,
/// claw, nose), `s` = the colour saturation, `f` (v5) = the colour FLATNESS: rms of rgb over the 5x5 texels around the nearest texel
/// at the ~1024 mip, x2, divided by the rms of the window's mean colour (dye-invariant: a dark or saturated coat has the same relative
/// detail as a white one); 1 where its brightest mean channel is above `FLAT_CLIP` (clipped white has no detail to measure: counted
/// as fur). Mirror:
/// `furprobe::flat_at`.
pub const CH_DETAIL: u32 = 5;
pub const CH_SAT: u32 = 6;
pub const CH_FLAT: u32 = 7;
pub const CH_LAST: u32 = CH_FLAT;
pub const FLAT_CLIP: f32 = 0.97;
pub const CH_NAMES: [&str; 8] = ["r", "g", "b", "a", "l", "d", "s", "f"];
pub const CH_LONG: [&str; 8] = ["red", "green", "blue", "alpha", "luminance", "detail (rg 3x3 rms x2 at the ~1024 mip)", "saturation", "flatness (rgb 5x5 rms x2 / brightness at the ~1024 mip)"];

/// `shell_fur_mask` parsed: the slot (0..3), the channel (0 r, 1 g, 2 b, 3 a, 4 luminance), `above` = fur where the value is above.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FurMaskSel {
    pub slot: u32,
    pub ch: u32,
    pub above: bool,
}

/// `off` / empty = `None`; `t0.a<`, `t1.r>`, `t2.l<` ...; `Err` for anything else.
pub fn parse_fur_mask(v: &str) -> Result<Option<FurMaskSel>, ()> {
    let t: String = v.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_ascii_lowercase();
    if t.is_empty() || t == "off" || t == "0" || t == "none" {
        return Ok(None);
    }
    let b = t.as_bytes();
    if b.len() != 5 || b[0] != b't' || b[2] != b'.' {
        return Err(());
    }
    let slot = (b[1] as char).to_digit(10).ok_or(())?;
    if slot >= FUR_MASK_SLOTS {
        return Err(());
    }
    let ch = match b[3] {
        b'r' => 0,
        b'g' => 1,
        b'b' => 2,
        b'a' => 3,
        b'l' => 4,
        b'd' => CH_DETAIL,
        b's' => CH_SAT,
        b'f' => CH_FLAT,
        _ => return Err(()),
    };
    let above = match b[4] {
        b'<' => false,
        b'>' => true,
        _ => return Err(()),
    };
    Ok(Some(FurMaskSel { slot, ch, above }))
}

/// The normalised text of a selection (`off` or `t0.a<`).
pub fn fur_mask_text(s: Option<FurMaskSel>) -> String {
    match s {
        None => "off".to_string(),
        Some(s) => format!("t{}.{}{}", s.slot, CH_NAMES[s.ch.min(CH_LAST) as usize], if s.above { ">" } else { "<" }),
    }
}

/// One test of the v5 list: the selection and its own threshold (`None` = `shell_fur_threshold`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FurTest {
    pub sel: FurMaskSel,
    pub th: Option<f32>,
}

/// v5 `shell_fur_mask`: `off` / empty = no test, else up to `MAX_FUR_TESTS` tests separated by `,` (or `&`), each `t<slot>.<ch><op>`
/// optionally followed by its own threshold 0..1 (`t0.f>0.025`; without one the test uses `shell_fur_threshold`). A texel is fur when
/// EVERY test says so (the weights multiply). Extra tests past the fourth are dropped; anything malformed is `Err`.
pub fn parse_fur_tests(v: &str) -> Result<Vec<FurTest>, ()> {
    let t: String = v.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_ascii_lowercase();
    if t.is_empty() || t == "off" || t == "0" || t == "none" {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for part in t.split([',', '&']) {
        if part.len() < 5 || !part.is_char_boundary(5) {
            return Err(());
        }
        let sel = parse_fur_mask(&part[..5])?.ok_or(())?;
        let rest = &part[5..];
        let th = if rest.is_empty() {
            None
        } else {
            match rest.parse::<f32>() {
                Ok(x) if x.is_finite() => Some(x.clamp(0.0, 1.0)),
                _ => return Err(()),
            }
        };
        if out.len() < MAX_FUR_TESTS {
            out.push(FurTest { sel, th });
        }
    }
    Ok(out)
}

fn short_num(x: f32) -> String {
    let s = format!("{:.4}", x);
    let s = s.trim_end_matches('0').trim_end_matches('.').to_string();
    if s.is_empty() || s == "-0" { "0".to_string() } else { s }
}

/// The normalised text of a test list (`off`, `t0.a<`, `t0.a<, t0.f>0.025`): a round trip is stable.
pub fn fur_tests_text(v: &[FurTest]) -> String {
    if v.is_empty() {
        return "off".to_string();
    }
    v.iter().map(|t| format!("{}{}", fur_mask_text(Some(t.sel)), t.th.map_or(String::new(), short_num))).collect::<Vec<_>>().join(", ")
}

/// `shell_fur_mask` cleaned: a valid list normalised, garbage = the default.
pub fn clean_fur_mask(v: &str) -> String {
    match parse_fur_tests(v) {
        Ok(s) => fur_tests_text(&s),
        Err(()) => FUR_MASK_DEFAULT.to_string(),
    }
}

/// The soft half width of a test at `th`: `shell_fur_soft`, but never more than half the distance to 0 or 1 (so `t0.f>0.025` has a
/// 0.0125..0.0375 edge, not a -0.025..0.075 one). Mirror of the shader's `fd_soft`.
pub fn test_soft(soft: f32, th: f32) -> f32 {
    let s = if soft.is_finite() { soft.clamp(FUR_SOFT_RANGE.0, FUR_SOFT_RANGE.1) } else { FUR_SOFT_DEFAULT };
    s.min(0.5 * th.min(1.0 - th)).max(0.002)
}

/// One excluded uv rectangle: `[u0, v0, u1, v1]` (u0 < u1, v0 < v1, inside 0..1) and the index count it is limited to (0 = every draw).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct UvRect {
    pub r: [f32; 4],
    pub count: u32,
}

/// `shell_uv_exclude`: `u0 v0 u1 v1[@count]` separated by `;` (numbers separated by spaces or commas). Bad or empty rectangles are
/// skipped, the corners sorted and clamped to 0..1, at most `MAX_UV_RECTS`.
pub fn parse_uv_rects(text: &str) -> Vec<UvRect> {
    let mut out = Vec::new();
    for part in text.split(';') {
        if out.len() >= MAX_UV_RECTS {
            break;
        }
        let (nums, count) = match part.split_once('@') {
            Some((a, c)) => match c.trim().parse::<u32>() {
                Ok(n) => (a, n),
                Err(_) => continue,
            },
            None => (part, 0),
        };
        let v: Vec<f32> = nums.split([',', ' ', '\t']).filter(|s| !s.trim().is_empty()).filter_map(|s| s.trim().parse::<f32>().ok()).collect();
        if v.len() != 4 || v.iter().any(|x| !x.is_finite()) {
            continue;
        }
        let c = |x: f32| x.clamp(0.0, 1.0);
        let r = [c(v[0].min(v[2])), c(v[1].min(v[3])), c(v[0].max(v[2])), c(v[1].max(v[3]))];
        if r[2] - r[0] <= 0.0 || r[3] - r[1] <= 0.0 {
            continue;
        }
        out.push(UvRect { r, count });
    }
    out
}

/// The normalised text of a rectangle list (what config.ini gets back: a round trip is stable).
pub fn uv_rects_text(v: &[UvRect]) -> String {
    let f = |x: f32| {
        let s = format!("{:.4}", x);
        let s = s.trim_end_matches('0').trim_end_matches('.').to_string();
        if s.is_empty() || s == "-0" { "0".to_string() } else { s }
    };
    v.iter()
        .map(|u| {
            let base = format!("{} {} {} {}", f(u.r[0]), f(u.r[1]), f(u.r[2]), f(u.r[3]));
            if u.count > 0 { format!("{base}@{}", u.count) } else { base }
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// The discriminator as the fur mask uses it per draw (no allocation: copied per draw).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Discrim {
    /// The first test and its threshold (its own or `shell_fur_threshold`).
    pub sel: Option<FurMaskSel>,
    pub threshold: f32,
    pub soft: f32,
    pub n: usize,
    pub rects: [UvRect; MAX_UV_RECTS],
    /// v5: tests 2..4 (selection, resolved threshold) and how many.
    pub more: [(FurMaskSel, f32); MAX_FUR_TESTS - 1],
    pub nmore: usize,
}

const NO_SEL: FurMaskSel = FurMaskSel { slot: 0, ch: 0, above: false };

impl Default for Discrim {
    fn default() -> Self {
        Discrim {
            sel: None,
            threshold: FUR_THRESHOLD_DEFAULT,
            soft: FUR_SOFT_DEFAULT,
            n: 0,
            rects: [UvRect { r: [0.0; 4], count: 0 }; MAX_UV_RECTS],
            more: [(NO_SEL, 0.0); MAX_FUR_TESTS - 1],
            nmore: 0,
        }
    }
}

impl Discrim {
    pub fn new(c: &ShellCfg) -> Discrim {
        let c = c.clamped();
        let tests = parse_fur_tests(&c.fur_mask).unwrap_or_default();
        let mut d = Discrim { soft: c.fur_soft, threshold: c.fur_threshold, ..Discrim::default() };
        for (k, t) in tests.iter().enumerate() {
            let th = t.th.unwrap_or(c.fur_threshold);
            if k == 0 {
                d.sel = Some(t.sel);
                d.threshold = th;
            } else {
                d.more[k - 1] = (t.sel, th);
                d.nmore = k;
            }
        }
        for (i, r) in parse_uv_rects(&c.uv_exclude).into_iter().enumerate().take(MAX_UV_RECTS) {
            d.rects[i] = r;
            d.n = i + 1;
        }
        d
    }

    /// Every texture test: (selection, threshold), the first one first.
    pub fn tests(&self) -> impl Iterator<Item = (FurMaskSel, f32)> + '_ {
        self.sel.map(|s| (s, self.threshold)).into_iter().chain(self.more[..self.nmore.min(MAX_FUR_TESTS - 1)].iter().copied())
    }

    /// Bit k set = some test reads engine slot `t<k>`.
    pub fn slots(&self) -> u32 {
        self.tests().fold(0, |m, (s, _)| m | 1 << s.slot.min(FUR_MASK_SLOTS - 1))
    }

    /// Anything to do (a texture test or at least one rectangle).
    pub fn active(&self) -> bool {
        self.sel.is_some() || self.n > 0
    }

    /// The rectangles that apply to a draw with `count` indices.
    pub fn rects_for(&self, count: u32) -> usize {
        self.rects[..self.n].iter().filter(|r| r.count == 0 || r.count == count).count()
    }
}

/// `b13` of the fur mask's pixel shader for one draw (`count` = its index count): [0] = (texture test on, slot, channel, above),
/// [1] = (threshold, soft, rectangles, feather), [2..] = the rectangles that apply to this draw. Inactive = all zero: the shader writes
/// the mask exactly as before (a = 1).
pub fn fd_cb_values(d: &Discrim, count: u32) -> [[f32; 4]; FD_LEN] {
    let mut o = [[0.0f32; 4]; FD_LEN];
    if !d.active() {
        return o;
    }
    let sel4 = |s: FurMaskSel| [1.0, s.slot.min(FUR_MASK_SLOTS - 1) as f32, s.ch.min(CH_LAST) as f32, if s.above { 1.0 } else { 0.0 }];
    let th_of = |t: f32| if t.is_finite() { t.clamp(0.0, 1.0) } else { FUR_THRESHOLD_DEFAULT };
    if let Some(s) = d.sel {
        o[0] = sel4(s);
    }
    for (k, (s, t)) in d.more[..d.nmore.min(MAX_FUR_TESTS - 1)].iter().enumerate() {
        o[FD_MORE + k] = sel4(*s);
        o[FD_MORE_TH][k] = th_of(*t);
    }
    let th = th_of(d.threshold);
    let soft = if d.soft.is_finite() { d.soft.clamp(FUR_SOFT_RANGE.0, FUR_SOFT_RANGE.1) } else { FUR_SOFT_DEFAULT };
    let mut n = 0usize;
    for r in d.rects[..d.n.min(MAX_UV_RECTS)].iter().filter(|r| r.count == 0 || r.count == count) {
        o[2 + n] = r.r;
        n += 1;
    }
    o[1] = [th, soft, n as f32, UV_FEATHER];
    o
}

/// The constants of one fur draw, or `None` when the discriminator cannot change anything for it (off; the selected texture slot is
/// unbound for this draw and no rectangle applies): then nothing is bound and the mask is written exactly as before.
#[allow(dead_code)] // tests (every slot bound or none); the game path uses `fd_for_draw_slots`
pub fn fd_for_draw(d: &Discrim, count: u32, tex_bound: bool) -> Option<[[f32; 4]; FD_LEN]> {
    fd_for_draw_slots(d, count, if tex_bound { (1 << FUR_MASK_SLOTS) - 1 } else { 0 })
}

/// `fd_for_draw` with the engine slots bound for this draw as a bit mask (bit k = `t<k>`): a test on an unbound slot is switched off.
pub fn fd_for_draw_slots(d: &Discrim, count: u32, bound: u32) -> Option<[[f32; 4]; FD_LEN]> {
    let mut v = fd_cb_values(d, count);
    let live = |e: &[f32; 4]| e[0] > 0.5 && bound & (1 << (e[1] as u32).min(FUR_MASK_SLOTS - 1)) != 0;
    let mut any = false;
    for i in std::iter::once(0).chain(FD_MORE..FD_MORE + MAX_FUR_TESTS - 1) {
        if live(&v[i]) {
            any = true;
        } else {
            v[i] = [0.0; 4];
        }
    }
    if !any && d.rects_for(count) == 0 {
        return None;
    }
    Some(v)
}

fn smooth(a: f32, b: f32, x: f32) -> f32 {
    let t = ((x - a) / (b - a)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// The weight of one test for value `v` (mirror of the shader's `fd_test`).
pub fn test_weight(above: bool, th: f32, soft: f32, v: f32) -> f32 {
    let s = test_soft(soft, th);
    let up = smooth(th - s, th + s, v);
    if above { up } else { 1.0 - up }
}

#[allow(dead_code)] // tests: the CPU mirror the WARP test compares with
/// CPU mirror of the shader's weight for a texel value `v` (`None` = no texture bound) at the frac uv `uv`: 1 = fur, 0 = not fur.
/// Only the FIRST test; `fur_weight_all` takes a value per test.
pub fn fur_weight(d: &Discrim, count: u32, v: Option<f32>, uv: [f32; 2]) -> f32 {
    fur_weight_all(d, count, &[v], uv)
}

/// CPU mirror for every test: `vals[k]` = the value of test k (`None` = its slot unbound / not measured: that test says fur).
pub fn fur_weight_all(d: &Discrim, count: u32, vals: &[Option<f32>], uv: [f32; 2]) -> f32 {
    let cb = fd_cb_values(d, count);
    let mut w = 1.0f32;
    for (k, (s, _)) in d.tests().enumerate() {
        if let Some(Some(v)) = vals.get(k) {
            w *= test_weight(s.above, if k == 0 { cb[1][0] } else { cb[FD_MORE_TH][k - 1] }, cb[1][1], *v);
        }
    }
    let ss = smooth;
    let fe = cb[1][3].max(1e-5);
    for r in cb[2..2 + cb[1][2] as usize].iter() {
        let dd = (uv[0] - r[0]).min(r[2] - uv[0]).min((uv[1] - r[1]).min(r[3] - uv[1]));
        w *= 1.0 - ss(-fe, fe, dd);
    }
    w.clamp(0.0, 1.0)
}

static DISCRIM: Mutex<Discrim> = Mutex::new(Discrim {
    sel: None,
    threshold: FUR_THRESHOLD_DEFAULT,
    soft: FUR_SOFT_DEFAULT,
    n: 0,
    rects: [UvRect { r: [0.0; 4], count: 0 }; MAX_UV_RECTS],
    more: [(NO_SEL, 0.0); MAX_FUR_TESTS - 1],
    nmore: 0,
});

/// The discriminator of the current config (a copy; the lock is only held for a copy, like the fur mask's selector).
pub fn discrim() -> Discrim {
    *DISCRIM.lock().unwrap_or_else(|e| e.into_inner())
}

/// The `status_shells=` part of the discriminator.
pub fn discrim_text(c: &ShellCfg) -> String {
    let c = c.clamped();
    let d = Discrim::new(&c);
    let tests: Vec<String> = d
        .tests()
        .map(|(s, th)| format!("t{}.{} {} {:.3} (soft +-{:.3})", s.slot, CH_LONG[s.ch.min(CH_LAST) as usize], if s.above { ">" } else { "<" }, th, test_soft(d.soft, th)))
        .collect();
    let tex = if tests.is_empty() { "texture test off".to_string() } else { format!("fur where {}", tests.join(" and ")) };
    format!("fur discriminator: {tex}, {} excluded uv rects{}", d.n, if d.n > 0 { format!(" [{}]", uv_rects_text(&d.rects[..d.n])) } else { String::new() })
}

fn num(v: &str, lo: f32, hi: f32, default: f32) -> f32 {
    match v.trim().parse::<f32>() {
        Ok(x) if x.is_finite() => x.clamp(lo, hi),
        _ => default,
    }
}

fn clamp_or(v: f32, r: (f32, f32), d: f32) -> f32 {
    if v.is_finite() { v.clamp(r.0, r.1) } else { d }
}

/// `fur_shells`: 4..32 (0..3 become 4).
pub fn clamp_shells(v: f32) -> u32 {
    if !v.is_finite() {
        return SHELLS_DEFAULT;
    }
    let n = v.round().clamp(0.0, MAX_SHELLS as f32) as u32;
    n.max(4)
}

/// A list value: no control characters, lower case, at most 400 characters.
fn clean(v: &str) -> String {
    v.chars().filter(|c| !c.is_control()).take(400).collect::<String>().trim().to_ascii_lowercase()
}

/// `face` or `texcoordN` (lower case); anything else is `face`.
pub fn clean_normal(v: &str) -> String {
    let t = clean(v);
    match t.strip_prefix("texcoord") {
        Some(d) if !d.is_empty() && d.len() <= 2 && d.bytes().all(|b| b.is_ascii_digit()) => t,
        _ => "face".to_string(),
    }
}

impl ShellCfg {
    /// Applies one config.ini key; false when `key` is not a shell key.
    pub fn set(&mut self, key: &str, v: &str) -> bool {
        let d = ShellCfg::default();
        match key {
            "fur_shells" => self.shells = clamp_shells(v.trim().parse::<f32>().unwrap_or(f32::NAN)),
            "shell_length" => self.length = num(v, LENGTH_RANGE.0, LENGTH_RANGE.1, d.length),
            "shell_density" => self.density = num(v, DENSITY_RANGE.0, DENSITY_RANGE.1, d.density),
            "shell_thickness" => self.thickness = num(v, THICKNESS_RANGE.0, THICKNESS_RANGE.1, d.thickness),
            "shell_root_dark" => self.root_dark = num(v, 0.0, 1.0, d.root_dark),
            "shell_wind" => self.wind = num(v, 0.0, 1.0, d.wind),
            "shell_lod_distance" => self.lod = num(v, LOD_RANGE.0, LOD_RANGE.1, d.lod),
            "shell_rim" => self.rim = num(v, RIM_RANGE.0, RIM_RANGE.1, d.rim),
            "shell_ps" => self.ps = clean(v),
            "shell_vs" => self.vs = clean(v),
            "shell_min_vcb" => self.min_vcb = num(v, 0.0, 1_048_576.0, d.min_vcb as f32).round() as u32,
            "shell_counts" => self.counts = clean(v),
            "shell_normal" => self.normal = clean_normal(v),
            "shell_unit" => {
                let x = v.trim().parse::<f32>().unwrap_or(f32::NAN);
                self.unit = if !x.is_finite() || x <= 0.0 { 0.0 } else { x.clamp(UNIT_RANGE.0, UNIT_RANGE.1) };
            }
            "shell_debug" => self.debug = num(v, 0.0, 2.0, 0.0).round() as u8,
            "shell_detail" => self.detail = num(v, 0.0, 1.0, d.detail),
            "shell_clump" => self.clump = num(v, 0.0, 1.0, d.clump),
            "shell_fur_mask" => self.fur_mask = clean_fur_mask(v),
            "shell_fur_threshold" => self.fur_threshold = num(v, 0.0, 1.0, d.fur_threshold),
            "shell_fur_soft" => self.fur_soft = num(v, FUR_SOFT_RANGE.0, FUR_SOFT_RANGE.1, d.fur_soft),
            "shell_uv_exclude" => self.uv_exclude = uv_rects_text(&parse_uv_rects(&clean(v))),
            "fur_auto" => self.auto = num(v, 0.0, 1.0, d.auto as f32).round() as u8,
            _ => return false,
        }
        true
    }

    /// Every value inside its range (the overlay sliders, NaN-safe).
    pub fn clamped(&self) -> ShellCfg {
        let d = ShellCfg::default();
        ShellCfg {
            shells: clamp_shells(self.shells as f32),
            length: clamp_or(self.length, LENGTH_RANGE, d.length),
            density: clamp_or(self.density, DENSITY_RANGE, d.density),
            thickness: clamp_or(self.thickness, THICKNESS_RANGE, d.thickness),
            root_dark: clamp_or(self.root_dark, (0.0, 1.0), d.root_dark),
            wind: clamp_or(self.wind, (0.0, 1.0), d.wind),
            lod: clamp_or(self.lod, LOD_RANGE, d.lod),
            rim: clamp_or(self.rim, RIM_RANGE, d.rim),
            ps: clean(&self.ps),
            vs: clean(&self.vs),
            min_vcb: self.min_vcb.min(1_048_576),
            counts: clean(&self.counts),
            normal: clean_normal(&self.normal),
            unit: if self.unit.is_finite() && self.unit > 0.0 { self.unit.clamp(UNIT_RANGE.0, UNIT_RANGE.1) } else { 0.0 },
            debug: self.debug.min(2),
            detail: clamp_or(self.detail, (0.0, 1.0), d.detail),
            clump: clamp_or(self.clump, (0.0, 1.0), d.clump),
            fur_mask: clean_fur_mask(&self.fur_mask),
            fur_threshold: clamp_or(self.fur_threshold, (0.0, 1.0), d.fur_threshold),
            fur_soft: clamp_or(self.fur_soft, FUR_SOFT_RANGE, d.fur_soft),
            uv_exclude: uv_rects_text(&parse_uv_rects(&self.uv_exclude)),
            auto: self.auto.min(1),
        }
    }

    /// The config.ini block (comments + every key).
    pub fn ini(&self) -> String {
        format!(
            concat!(
                "# Shell fur (docs/shell-fur.md): real strands on the fur draws (the fur mask's draws that the shell selector picks), drawn with the engine's mesh\n",
                "# extruded fur_shells times; needs fur_strength > 0 and a fur_ps selection. fur_shells 4..32\n",
                "# (16 = default; layers incl. the base, 0 = off), shell_length 0.5..20 cm (2.5), shell_density 50..4000 strand cells per uv unit (1200), shell_thickness\n",
                "# 0.1..1 root diameter of a strand (0.6), shell_root_dark 0..1 (0.6: the roots and the skin between the strands darker, the tips brighter, the coat's average kept), shell_wind 0..1 (0.15), shell_lod_distance\n",
                "# 2..100 m (12.5: none beyond, shorter and fainter from half of it), shell_rim 0..2 (1) back light against a bright background (never in the dark),\n",
                "# shell_detail 0..1 (0.7: the eyes, nose, markings and horns stay clear of the strands around them; 0 = strands paint everywhere), shell_clump 0..1 (0.5).\n",
                "# Which draws: shell_ps (pixel shaders, a subset of fur_ps; empty = all fur draws), shell_vs (vertex shaders; empty = any), shell_min_vcb (0 = any draw;\n",
                "# v14: an old saved 4096 reads as 0), shell_counts = index counts to take (17694) or leave out (-5214), comma-separated (empty = all; status_shells= lists the draws).\n",
                "# shell_normal face (default) or TEXCOORDn (a vertex output with the normal), shell_unit 0 = auto (engine units per metre from the depth) or 0.01..1000,\n",
                "# shell_debug 1 paints the shell pixels (red root .. yellow tip), 2 = a colour per fur draw (the legend and the per-draw texture probe are in\n",
                "# status_shells=; the fur texels the discriminator removed turn black). All live with debug_hot_reload=1.\n",
                "# Fur discriminator (only the texels that really are fur get fur: not the horns, claws, nose, lips; also the old fur look, the rim light and fur_debug):\n",
                "# shell_fur_mask = off, or up to 4 tests separated by commas, ALL must say fur: t<slot 0..3>.<r|g|b|a|l|d|s|f> then < (fur below the\n",
                "# threshold) or > (above), optionally the test's own threshold (t0.f>0.025; else shell_fur_threshold). d = local detail of .rg (a normal\n",
                "# map's bumpiness), s = saturation, f = colour flatness (a flat dye-coloured horn ~0, painted fur 0.03+, dye-independent). Default\n",
                "# t0.a<, t0.f>0.025 (gloss alpha below the threshold and not flat) for the mask (old fur look, rim, fur_debug). v7: the shells test every\n",
                "# strand at its own root uv against the draw's own coat (flat horn, glossy / tan claws, fang, red lips; any coat colour; no extra line is\n",
                "# needed); off = only the draw test (charr skin draws, not armour / human skin). shell_fur_threshold 0..1 (0.66), shell_fur_soft 0.01..0.5\n",
                "# (0.05). shell_uv_exclude (normally empty) = mesh-uv rectangles without fur, \"u0 v0 u1 v1\" separated by ; (@count = one draw only).\n",
                "# fur_auto 1 (default) = every charr, not only the fur_ps shaders: skinned draws of any pixel shader that sample a character composite\n",
                "# (BC3 colour + BC5 normal) are tested too, and the draws of an atlas proven charr fur (head, body, arms, tail of one character) get fur on\n",
                "# their fur texels even when the draw is half armour; the log says per draw group and per atlas why (furselect: lines). 0 = fur_ps only.\n",
                "fur_shells={}\n",
                "shell_length={}\n",
                "shell_density={}\n",
                "shell_thickness={}\n",
                "shell_root_dark={}\n",
                "shell_wind={}\n",
                "shell_lod_distance={}\n",
                "shell_rim={}\n",
                "shell_ps={}\n",
                "shell_vs={}\n",
                "shell_min_vcb={}\n",
                "shell_counts={}\n",
                "shell_normal={}\n",
                "shell_unit={}\n",
                "shell_debug={}\n",
                "shell_detail={}\n",
                "shell_clump={}\n",
                "shell_fur_mask={}\n",
                "shell_fur_threshold={}\n",
                "shell_fur_soft={}\n",
                "shell_uv_exclude={}\n",
                "fur_auto={}\n",
            ),
            self.shells, self.length, self.density, self.thickness, self.root_dark, self.wind, self.lod, self.rim, self.ps, self.vs, self.min_vcb, self.counts, self.normal, self.unit, self.debug, self.detail, self.clump,
            self.fur_mask, self.fur_threshold, self.fur_soft, self.uv_exclude, self.auto
        )
    }

    /// The keys whose value differs between `a` and `b` (the hot reload's live list).
    pub fn changed(a: &ShellCfg, b: &ShellCfg) -> Vec<&'static str> {
        let mut v = Vec::new();
        macro_rules! c {
            ($($f:ident => $k:expr),*) => { $( if a.$f != b.$f { v.push($k); } )* };
        }
        c!(shells => "fur_shells", length => "shell_length", density => "shell_density", thickness => "shell_thickness", root_dark => "shell_root_dark", wind => "shell_wind", lod => "shell_lod_distance", rim => "shell_rim", ps => "shell_ps", vs => "shell_vs", min_vcb => "shell_min_vcb", counts => "shell_counts", normal => "shell_normal", unit => "shell_unit", debug => "shell_debug", detail => "shell_detail", clump => "shell_clump", fur_mask => "shell_fur_mask", fur_threshold => "shell_fur_threshold", fur_soft => "shell_fur_soft", uv_exclude => "shell_uv_exclude", auto => "fur_auto");
        v
    }

    /// Takes the value of `key` from `new` (the hot reload's merge: the other keys keep the slider values).
    pub fn take(&mut self, key: &str, new: &ShellCfg) {
        match key {
            "fur_shells" => self.shells = new.shells,
            "shell_length" => self.length = new.length,
            "shell_density" => self.density = new.density,
            "shell_thickness" => self.thickness = new.thickness,
            "shell_root_dark" => self.root_dark = new.root_dark,
            "shell_wind" => self.wind = new.wind,
            "shell_lod_distance" => self.lod = new.lod,
            "shell_rim" => self.rim = new.rim,
            "shell_ps" => self.ps = new.ps.clone(),
            "shell_vs" => self.vs = new.vs.clone(),
            "shell_min_vcb" => self.min_vcb = new.min_vcb,
            "shell_counts" => self.counts = new.counts.clone(),
            "shell_normal" => self.normal = new.normal.clone(),
            "shell_unit" => self.unit = new.unit,
            "shell_debug" => self.debug = new.debug,
            "shell_detail" => self.detail = new.detail,
            "shell_clump" => self.clump = new.clump,
            "shell_fur_mask" => self.fur_mask = new.fur_mask.clone(),
            "shell_fur_threshold" => self.fur_threshold = new.fur_threshold,
            "shell_fur_soft" => self.fur_soft = new.fur_soft,
            "shell_uv_exclude" => self.uv_exclude = new.uv_exclude.clone(),
            "fur_auto" => self.auto = new.auto,
            _ => {}
        }
    }

    pub fn is_key(k: &str) -> bool {
        KEYS.contains(&k)
    }
}

// ---- the selector (pure) ----------------------------------------------------------------------------------------

/// `shell_counts`: (take, leave out). Bad entries are skipped.
pub fn parse_counts(text: &str) -> (Vec<u32>, Vec<u32>) {
    let (mut take, mut leave) = (Vec::new(), Vec::new());
    for tok in text.split([',', ';', ' ', '\t']) {
        let t = tok.trim();
        let (neg, d) = match t.strip_prefix('-') {
            Some(d) => (true, d),
            None => (false, t.strip_prefix('+').unwrap_or(t)),
        };
        let Ok(n) = d.parse::<u32>() else { continue };
        let v = if neg { &mut leave } else { &mut take };
        if !v.contains(&n) && v.len() < 32 {
            v.push(n);
        }
    }
    (take, leave)
}

/// Which fur draws get shells.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Selector {
    pub ps: Vec<u32>,
    pub vs: Vec<u32>,
    pub min_vcb: u32,
    pub take: Vec<u32>,
    pub leave: Vec<u32>,
}

impl Selector {
    pub fn new(c: &ShellCfg) -> Selector {
        let (take, leave) = parse_counts(&c.counts);
        let min_vcb = if c.min_vcb == MIN_VCB_LEGACY { MIN_VCB_DEFAULT } else { c.min_vcb };
        Selector { ps: crate::furmask::parse_hashes(&c.ps), vs: crate::furmask::parse_hashes(&c.vs), min_vcb, take, leave }
    }

    /// `Ok` = shells; `Err(why)` for the per-draw list of the status line.
    pub fn verdict(&self, ps: u32, vs: Option<u32>, vcb0: u32, count: u32) -> Result<(), &'static str> {
        if !self.ps.is_empty() && !self.ps.contains(&ps) {
            return Err("ps not in shell_ps");
        }
        if !self.vs.is_empty() && !vs.is_some_and(|v| self.vs.contains(&v)) {
            return Err("vs not in shell_vs");
        }
        if vcb0 < self.min_vcb {
            return Err("vcb < shell_min_vcb");
        }
        if self.leave.contains(&count) {
            return Err("count left out by shell_counts");
        }
        if !self.take.is_empty() && !self.take.contains(&count) {
            return Err("count not in shell_counts");
        }
        Ok(())
    }
}

// ---- the draw call --------------------------------------------------------------------------------------------

/// The arguments of one engine draw call (the four hooked `Draw*` methods).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Call {
    Draw(u32, u32),
    Indexed(u32, u32, i32),
    Instanced(u32, u32, u32, u32),
    IndexedInstanced(u32, u32, u32, i32, u32),
}

impl Call {
    /// Index count (indexed draws) or vertex count per instance: what `shell_counts` matches.
    pub fn count(&self) -> u32 {
        match *self {
            Call::Draw(n, _) | Call::Indexed(n, _, _) | Call::Instanced(n, _, _, _) | Call::IndexedInstanced(n, _, _, _, _) => n,
        }
    }

    pub fn instances(&self) -> u32 {
        match *self {
            Call::Draw(..) | Call::Indexed(..) => 1,
            Call::Instanced(_, i, _, _) | Call::IndexedInstanced(_, i, _, _, _) => i,
        }
    }

    /// (start index / vertex, base vertex): v9's mesh key.
    pub fn start_base(&self) -> (u32, i32) {
        match *self {
            Call::Draw(_, s) => (s, 0),
            Call::Indexed(_, s, b) => (s, b),
            Call::Instanced(_, _, s, _) => (s, 0),
            Call::IndexedInstanced(_, _, s, b, _) => (s, b),
        }
    }

    pub fn indexed(&self) -> bool {
        matches!(self, Call::Indexed(..) | Call::IndexedInstanced(..))
    }

    /// Issues the call on `ctx` (through the hooked functions: their re-entrancy guard passes our own call straight to the original).
    ///
    /// # Safety
    /// `ctx` must be a live device context with a complete pipeline bound.
    pub unsafe fn issue(&self, ctx: Obj) {
        unsafe {
            match *self {
                Call::Draw(n, s) => {
                    let f: unsafe extern "system" fn(Obj, u32, u32) = std::mem::transmute(vfn(ctx, vt::CTX_DRAW));
                    f(ctx, n, s)
                }
                Call::Indexed(n, s, b) => {
                    let f: unsafe extern "system" fn(Obj, u32, u32, i32) = std::mem::transmute(vfn(ctx, vt::CTX_DRAW_INDEXED));
                    f(ctx, n, s, b)
                }
                Call::Instanced(vc, ic, sv, si) => {
                    let f: unsafe extern "system" fn(Obj, u32, u32, u32, u32) = std::mem::transmute(vfn(ctx, vt::CTX_DRAW_INSTANCED));
                    f(ctx, vc, ic, sv, si)
                }
                Call::IndexedInstanced(ic, n, si, bv, sn) => {
                    let f: unsafe extern "system" fn(Obj, u32, u32, u32, i32, u32) = std::mem::transmute(vfn(ctx, vt::CTX_DRAW_INDEXED_INSTANCED));
                    f(ctx, ic, n, si, bv, sn)
                }
            }
        }
    }
}

// ---- the packed key (mirror of ps_shell / shell_look) ----------------------------------------------------------

/// The key `ps_shell` writes for shell height `h` (0..1), coverage `a` (0..1, quantised to 1/15; the shader drops a fragment below 1/30)
/// and the anchor offset `d` (anchor - pixel, px; +-128 px in quarter pixels): `hq << 24 | a << 20 | dx << 10 | dy`.
#[allow(dead_code)] // the shaders have their own copies; the tests keep them equal
pub fn pack_key(h: f32, a: f32, d: [f32; 2]) -> u32 {
    let hq = (1.0 + (h.clamp(0.0, 1.0) * 125.0 + 0.5).floor()).clamp(1.0, 126.0) as u32;
    let aq = (a.clamp(0.0, 1.0) * 15.0 + 0.5).floor().clamp(1.0, 15.0) as u32;
    let q = |v: f32| ((v * 4.0 + 0.5).floor() + 512.0).clamp(0.0, 1023.0) as u32;
    hq << 24 | aq << 20 | q(d[0]) << 10 | q(d[1])
}

/// v13: bit 31 of a key: the surface under the strand fragment lies clearly behind its root (more than FRONT_DEPTH_ABS x FRAG_TOL_M
/// + FRONT_DEPTH_REL x FRAG_REL_TOL x depth): the composite treats the strand like one over the background (no detail test against the
/// colour under it: the neck / head fur in front of the horn). `unpack_key` ignores it.
pub const KEY_FRONT: u32 = 0x8000_0000;
#[allow(dead_code)] // the shader's copy (tests keep them equal)
pub const FRONT_DEPTH_ABS: f32 = 4.0;
#[allow(dead_code)] // the shader's copy (tests keep them equal)
pub const FRONT_DEPTH_REL: f32 = 2.0;

/// (h, coverage, anchor offset) of a key; `None` for 0 (no strand) or a key outside the range the shader writes.
#[allow(dead_code)]
pub fn unpack_key(k: u32) -> Option<(f32, f32, [f32; 2])> {
    let k = k & !KEY_FRONT;
    if k == 0 || k >= 0x7F00_0000 || k >> 24 == 0 || (k >> 20) & 15 == 0 {
        return None;
    }
    let hq = (k >> 24) as f32;
    let d = |v: u32| (v as f32 - 512.0) * 0.25;
    Some((((hq - 1.0) / 125.0).clamp(0.0, 1.0), ((k >> 20) & 15) as f32 / 15.0, [d((k >> 10) & 0x3ff), d(k & 0x3ff)]))
}

// ---- the generated geometry shader -------------------------------------------------------------------------------

fn comps(mask: u8) -> u32 {
    8 - (mask & 15).leading_zeros().min(8)
}

/// The output element `shell_normal` names (a float output with at least 3 components), or `None` for `face` / not found.
pub fn normal_elem(sig: &VsSig, normal: &str) -> Option<usize> {
    let idx: u32 = normal.strip_prefix("texcoord")?.parse().ok()?;
    sig.elems.iter().position(|e: &SigElem| e.sys == 0 && e.ctype == 3 && e.name.eq_ignore_ascii_case("TEXCOORD") && e.index == idx && comps(e.mask) >= 3 && e.mask & 7 == 7)
}

/// HLSL of the geometry shader for the vertex outputs `sig`: the input struct replicates them in register order (so the inputs sit on the
/// vertex shader's registers, like the fur mask's uv shader), the uv is `uv`, the optional normal `normal`; then `shaders/shellfur.hlsl`.
pub fn gs_hlsl(sig: &VsSig, uv: UvPick, normal: Option<usize>) -> Result<String, String> {
    let (decl, sum, pos) = crate::furmask::ps_inputs(sig, Some(uv));
    let pos = pos.ok_or_else(|| "the vertex shader has no SV_Position output".to_string())?;
    let decl = decl.replace("nointerpolation ", "");
    let sum = sum.replace("i.e", "v[0].e");
    let e = &sig.elems[uv.elem];
    let sw = if uv.zw { "zw" } else { "xy" };
    let uv_expr = if comps(e.mask) == 1 { format!("float2(v[j].e{}, 0.0)", uv.elem) } else { format!("v[j].e{}.{sw}", uv.elem) };
    let (n_expr, has_n) = match normal {
        Some(n) => (format!("v[j].e{n}.xyz"), 1),
        None => ("float3(0.0, 0.0, 0.0)".to_string(), 0),
    };
    Ok(format!(
        "#define SHELL_GS 1\nstruct VIn\n{{\n{decl}}};\n#define V_POS(j) (v[j].e{pos})\n#define V_UV(j) ({uv_expr})\n#define V_N(j) ({n_expr})\n#define HAS_N {has_n}\nfloat input_sum(VIn v[3])\n{{\n    float k = 0.0;\n{sum}    return k;\n}}\n\n{HLSL}"
    ))
}

/// The two geometry shaders of one vertex layout: the shells and the unit measurement (`gs_measure`).
pub struct GsBlobs {
    pub shell: Vec<u8>,
    pub measure: Vec<u8>,
    /// v7: the uv-space footprint (the per-draw coat statistics).
    pub uvspace: Vec<u8>,
    /// v9: the stream-output capture of every triangle (clip positions + atlas uv per primitive id: the island analysis, furisland.rs).
    pub tri: Vec<u8>,
}

/// Generates, compiles and verifies (inputs on the vertex shader's registers) the geometry shaders. Slow: background threads and tests.
pub fn build_gs(sig: &VsSig, uv: UvPick, normal: Option<usize>) -> Result<GsBlobs, String> {
    let src = gs_hlsl(sig, uv, normal)?;
    let shell = shaders::compile(&src, GS_ENTRY, "gs_5_0")?;
    crate::furmask::verify_link(sig, &shell)?;
    let measure = shaders::compile(&src, MEASURE_GS_ENTRY, "gs_5_0")?;
    crate::furmask::verify_link(sig, &measure)?;
    let uvspace = shaders::compile(&src, UV_GS_ENTRY, "gs_5_0")?;
    crate::furmask::verify_link(sig, &uvspace)?;
    let tri = shaders::compile(&src, TRI_GS_ENTRY, "gs_5_0")?;
    crate::furmask::verify_link(sig, &tri)?;
    Ok(GsBlobs { shell, measure, uvspace, tri })
}

enum GsState {
    Pending,
    Ready(Arc<GsBlobs>),
    Failed(String),
}

static GS_REG: Mutex<Option<HashMap<String, GsState>>> = Mutex::new(None);

/// v19 (furwarm.rs): a geometry shader set compiled in an earlier session (shell, measure, uv-space, triangle capture).
pub fn preload_gs(key: &str, b: &[Vec<u8>]) {
    let blobs = GsBlobs { shell: b[0].clone(), measure: b[1].clone(), uvspace: b[2].clone(), tri: b[3].clone() };
    GS_REG.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).entry(key.to_string()).or_insert(GsState::Ready(Arc::new(blobs)));
}

/// v19 (furwarm.rs): on a frame without the scene, makes up to `max` geometry shader sets known to the registry; returns how many.
pub unsafe fn prewarm(ctx: Obj, max: usize) -> usize {
    let Some(Ok(blobs)) = PS_BLOB.get() else { return 0 };
    if FAILED.load(Relaxed) || !ON.load(Relaxed) {
        return 0;
    }
    let ready: Vec<(String, Arc<GsBlobs>)> = GS_REG
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|m| m.iter().filter_map(|(k, v)| if let GsState::Ready(b) = v { Some((k.clone(), b.clone())) } else { None }).collect())
        .unwrap_or_default();
    if ready.is_empty() {
        return 0;
    }
    unsafe {
        let dev = device_of(ctx);
        if dev.is_null() {
            return 0;
        }
        let _ig = IgnoreScope::new();
        let made = 'run: {
            let Ok(mut g) = STATE.try_lock() else { break 'run 0 };
            if g.as_ref().is_some_and(|s| s.device != dev as usize) {
                *g = None;
            }
            if g.is_none() {
                *g = Shell::new(dev, blobs);
            }
            let Some(s) = g.as_mut() else { break 'run 0 };
            let mut made = 0;
            for (k, b) in ready {
                if made >= max {
                    break;
                }
                if s.gs.iter().any(|e| e.0 == k) {
                    continue;
                }
                s.gs_object(dev, &k, &b, true);
                made += 1;
            }
            made
        };
        release(dev);
        made
    }
}
const GS_PENDING: &str = "compiling the geometry shader";
/// Geometry shaders kept (vertex layouts x normal choices).
const MAX_GS: usize = 64;

/// The geometry shader of the vertex shader at `vs_ptr`: `Ok((key, blob))` when built, `Err(why)` while it compiles on a background
/// thread (started by the first call) or when it cannot be built. Never compiles on the caller's thread.
fn gs_for(vs_ptr: usize) -> Result<(String, Arc<GsBlobs>), String> {
    let sig = crate::furmask::vs_sig(vs_ptr).ok_or_else(|| "vertex shader signature unknown".to_string())?;
    let uv = crate::furmask::uv_pick(&sig)?;
    let normal_cfg = cfg().normal;
    let normal = normal_elem(&sig, &normal_cfg);
    let key = format!("{}|{}{}|{}", sig.text(), uv.elem, if uv.zw { "zw" } else { "xy" }, normal.map_or("face".to_string(), |n| n.to_string()));
    let mut g = GS_REG.lock().unwrap_or_else(|e| e.into_inner());
    let m = g.get_or_insert_with(HashMap::new);
    match m.get(&key) {
        Some(GsState::Ready(b)) => Ok((key, b.clone())),
        Some(GsState::Pending) => Err(GS_PENDING.to_string()),
        Some(GsState::Failed(e)) => Err(e.clone()),
        None => {
            if m.len() >= MAX_GS {
                return Err("too many vertex layouts".to_string());
            }
            m.insert(key.clone(), GsState::Pending);
            let k2 = key.clone();
            let spawned = crate::warm::compile_job(move || {
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| build_gs(&sig, uv, normal))).unwrap_or_else(|_| Err("panic while building".to_string()));
                let st = match r {
                    Ok(b) => {
                        crate::warm::geometry(&b.shell);
                        crate::warm::geometry(&b.measure);
                        crate::warm::geometry(&b.uvspace);
                        crate::furwarm::remember(0, &k2, vec![b.shell.clone(), b.measure.clone(), b.uvspace.clone(), b.tri.clone()]);
                        log!("shellfur: geometry shader built (vertex outputs: {}, normal {})", sig.text(), normal.map_or("face".to_string(), |n| format!("{}{}", sig.elems[n].name, sig.elems[n].index)));
                        GsState::Ready(Arc::new(b))
                    }
                    Err(e) => {
                        log!("shellfur: geometry shader for [{}] failed, those draws get no shells: {e}", sig.text());
                        GsState::Failed(e)
                    }
                };
                GS_REG.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).insert(k2, st);
            });
            if spawned.is_err() {
                m.insert(key, GsState::Failed("could not start the compile thread".to_string()));
            }
            Err(GS_PENDING.to_string())
        }
    }
}

// ---- constants of the passes (pure) ---------------------------------------------------------------------------

/// What one replayed draw needs to know besides the config.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DrawGeom {
    pub vp: [f32; 4],
    pub ccw: bool,
}

/// The wind phases (sin a, cos a, sin b, cos b) at `time` seconds, computed in double precision and wrapped exactly: continuous in time,
/// no jump when a float time would wrap or lose precision.
pub fn wind_phases(time: f64) -> [f32; 4] {
    let t = if time.is_finite() { time.max(0.0) } else { 0.0 };
    let tau = std::f64::consts::TAU;
    let a = (t * 1.7).rem_euclid(tau);
    let b = (t * 1.19).rem_euclid(tau);
    [a.sin() as f32, a.cos() as f32, b.sin() as f32, b.cos() as f32]
}

/// `b0` of the geometry shaders (see shellfur.hlsl). `unit` = engine units per metre of this frame (`shell_unit`, else the smoothed
/// measurement; 0 = none yet: the geometry shader measures per triangle).
pub fn gs_cb_values(c: &ShellCfg, g: &DrawGeom, time: f64, depth_unit: f32, unit: f32) -> [[f32; 4]; 6] {
    let c = c.clamped();
    let (w, h) = (g.vp[2].max(1.0), g.vp[3].max(1.0));
    let p11 = 1.0 / TAN_HALF_FOV;
    let p00 = p11 * h / w;
    let n = c.shells as f32;
    let du = if depth_unit.is_finite() && depth_unit > 0.0 { depth_unit } else { crate::aerial::DEFAULT_UNIT };
    let u = if c.unit > 0.0 { c.unit } else if unit.is_finite() && unit > 0.0 { unit } else { 0.0 };
    [
        [n, c.length / 100.0, MAX_PX * (h / 1440.0).clamp(0.5, 2.0), u],
        [p00, p11, w, h],
        [g.vp[0], g.vp[1], if g.ccw { 1.0 } else { 0.0 }, c.lod],
        [c.wind, GRAVITY, 0.0, du],
        wind_phases(time),
        [0.0; 4],
    ]
}

/// v11: `b0` of the geometry shader for the skin pass: the shells' constants with `g5.x = 1` (the base layer only, on every triangle
/// whatever its distance or angle). It needs no shell count: the skin pass runs whenever the records are replayed.
pub fn skin_gs_cb_values(c: &ShellCfg, g: &DrawGeom, time: f64, depth_unit: f32, unit: f32) -> [[f32; 4]; 6] {
    let mut v = gs_cb_values(c, g, time, depth_unit, unit);
    v[5][0] = 1.0;
    v
}

/// `b0` of the strand pixel shader: density, root diameter, smallest cell px, clump pull; the fragment occlusion tolerance (v7:
/// `FRAG_TOL_M` in normalised linear depth + `FRAG_REL_TOL` x depth), the narrowest strand px, the lean of a clump's strands.
pub fn ps_cb_values(c: &ShellCfg, depth_unit: f32) -> [[f32; 4]; 2] {
    let c = c.clamped();
    let du = if depth_unit.is_finite() && depth_unit > 0.0 { depth_unit } else { crate::aerial::DEFAULT_UNIT };
    [[c.density, c.thickness, MIN_CELL_PX, CLUMP * 2.0 * c.clump], [FRAG_TOL_M / du, FRAG_REL_TOL, MIN_STRAND_PX, LEAN * c.clump]]
}

/// `q3` of the strand pixel shader (v7): the root visibility tolerances (device depth; the linear-depth fallback, relative and absolute
/// in normalised depth); v8: w = the device-depth fragment test's absolute tolerance in metres.
pub fn ps_ext_values(depth_unit: f32) -> [f32; 4] {
    let du = if depth_unit.is_finite() && depth_unit > 0.0 { depth_unit } else { crate::aerial::DEFAULT_UNIT };
    [ROOT_DEV_TOL, ROOT_LIN_REL, ROOT_LIN_M / du, FRAG_TOL_M]
}

/// `q4` of the strand pixel shader (v8): the recorded draw's depth bias from its rasterizer state `rs` (D3D11_RASTERIZER_DESC as
/// u32s: [3] DepthBias, [4] DepthBiasClamp, [5] SlopeScaledDepthBias) in depth units of `depth_fmt` (D16: 1 / 65536, else 2^-24):
/// (constant, slope scale, clamp, 0). The engine's depth where the root is visible is the root's plane + this bias (a test GPU:
/// DepthBias 65 = 3.87e-6), so the root test allows it. Garbage / NaN = 0, capped at 1e-3.
pub fn ps_bias_values(rs: &[u32; 10], depth_fmt: u32) -> [f32; 4] {
    let r = match depth_fmt {
        53 | 55 | 56 => 1.0 / 65536.0,
        _ => 1.0 / 16_777_216.0,
    };
    let ok = |v: f32| if v.is_finite() { v.abs().min(1e-3) } else { 0.0 };
    let slope = f32::from_bits(rs[5]);
    [ok((rs[3] as i32).unsigned_abs() as f32 * r), if slope.is_finite() { slope.abs().min(16.0) } else { 0.0 }, ok(f32::from_bits(rs[4])), 0.0]
}

/// `b0[2..19]` of the strand pixel shader for one replayed draw (`count` its index count): [0] = (rectangles, feather, 0, 0), [1..] = the
/// `shell_uv_exclude` rectangles that apply to it. The shader tests each fragment's OWN mesh uv (the root's uv: the shells share the
/// base triangle's uv), so every layer of a strand whose root is excluded goes, wherever its root projects on the screen (the fur
/// mask's root test reads the screen at the root's pixel, which may show other fur when the root is hidden). No rectangle = all zero.
pub fn ps_rect_values(d: &Discrim, count: u32) -> [[f32; 4]; 1 + MAX_UV_RECTS] {
    let mut o = [[0.0f32; 4]; 1 + MAX_UV_RECTS];
    let mut n = 0usize;
    for r in d.rects[..d.n.min(MAX_UV_RECTS)].iter().filter(|r| r.count == 0 || r.count == count) {
        o[1 + n] = r.r;
        n += 1;
    }
    if n > 0 {
        o[0] = [n as f32, UV_FEATHER, 0.0, 0.0];
    }
    o
}

/// v18: `q4.w` of the strand / skin pixel shader: 1 when the record's atlas is a proven charr atlas (furselect.rs), so a draw that is
/// only partly fur (skin + armour in one draw) gets fur on its fur texels (`ft_mixed_weight`); 0 = the draw test alone (v17).
/// v19: may a record's strands be drawn? A `fur_ps` draw always (its own draw test decides per texel); a content-rule draw only
/// once its atlas is proven fur (until then it is only measured).
/// v19: the shells a frame draws in all (records x shells): past it the shells of each record shrink (never under SHARE_MIN_SHELLS:
/// fewer show the layers as horizontal bands), so a crowd shares one budget. A record is one draw (a character has several).
pub const FRAME_SHELL_BUDGET: u32 = 768;
pub const SHARE_MIN_SHELLS: u32 = 12;

/// The shells each of `n` fur draws gets this frame (`want` = `fur_shells`).
pub fn shell_share(want: u32, n: usize) -> u32 {
    if n == 0 {
        return want;
    }
    let each = FRAME_SHELL_BUDGET / n as u32;
    want.min(each.max(SHARE_MIN_SHELLS))
}

pub fn shells_allowed(auto: bool, atlas: usize) -> bool {
    !auto || crate::furselect::proven(atlas)
}

pub fn mixed_flag(atlas: usize) -> f32 {
    if auto_configured() && crate::furselect::proven(atlas) { 1.0 } else { 0.0 }
}

/// The share of a mixed draw's footprint in the gloss band from which it gets fur (`ft_mixed_weight`: 0 below MIXED_BAND.0, 1 above
/// MIXED_BAND.1): a body draw of a partly armoured charr (bare arms, legs, tail beside the armour) is 10..40 % in the band, a pure
/// armour or hair-card draw of the same atlas 0..2 % (the measured player draws: 17694 hair cards 2 %, armour 0..2 %).
pub const MIXED_BAND: (f32, f32) = (0.04, 0.08);
/// The coat gloss a mixed draw's texels are compared with (the glossy test): its own median is the armour's, so the fur's measured
/// gloss (0.40..0.44 in every charr atlas) is used instead.
pub const MIXED_COAT_GLOSS: f32 = 0.44;

/// Mirror of `ft_mixed_weight`: the draw weight of a draw of a proven charr atlas.
#[allow(dead_code)] // the CPU mirror (tests)
pub fn mixed_weight(s0: [f32; 4]) -> f32 {
    if !(s0[2] >= 16.0) {
        return 0.0;
    }
    smooth(MIXED_BAND.0, MIXED_BAND.1, s0[0])
}

/// Mirror of `strand_fur` (without the island bits: `near` and the limb flag off): the weight of a strand rooted at a texel with the
/// features `ft` / `hue`, of a draw with the coat statistics `s0` / `s1`; `mixed` = the record's `q4.w` (its atlas is proven).
#[allow(dead_code)] // the CPU mirror (tests)
pub fn strand_weight(ft: [f32; 4], hue: f32, s0: [f32; 4], s1: [f32; 4], texel_tests: bool, mixed: bool) -> f32 {
    let w = draw_weight(s0);
    let wm = if mixed { mixed_weight(s0) } else { 0.0 };
    if wm > w {
        // the fur texels of a mixed draw: in the gloss band, the gloss test against the fur's own gloss, no colour tests
        let band = bx(GLOSS_BAND.0, GLOSS_BAND.1, 0.02, ft[0]);
        if !texel_tests {
            return wm * band;
        }
        let s1m = [MIXED_COAT_GLOSS, s1[1], s1[2], s1[3]];
        // The colour tests (keratin tan, red) stay on: they keep horns, claws and fangs bare.
        return wm * band * texel_weight_opt(ft, hue, s0, s1m, true);
    }
    if !(w > 0.0) || !texel_tests {
        return w;
    }
    w * texel_weight(ft, hue, s0, s1)
}

/// The strand pixel shader's whole `b0`: `rects[0]` = q2 (rectangles, feather, the record index, the texel tests on), `ext` = q3.

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PsCb {
    pub base: [[f32; 4]; 2],
    pub rects: [[f32; 4]; 1 + MAX_UV_RECTS],
    pub ext: [f32; 4],
    /// v8: `q4`, the record's depth bias (`ps_bias_values`).
    pub bias: [f32; 4],
}

/// `b13` of the light-adjust substitute: [0] = (on, root darkening, back light, debug), [1] = (detail, 0, 0, 0). `ready` = shells were
/// drawn this frame and the mod is on; otherwise all zero (the composite is skipped: bit-identical).
pub fn composite_cb_values(ready: bool, c: &ShellCfg) -> [[f32; 4]; 2] {
    let c = c.clamped();
    if !ready || c.shells == 0 {
        return [[0.0; 4]; 2];
    }
    [[1.0, c.root_dark, c.rim, c.debug as f32], [c.detail, 0.0, 0.0, 0.0]]
}

// ---- v7: the fur discriminator per strand, CPU mirror of shellfur.hlsl (ft_feat, ft_texel_weight, ps_stats) -------------------------

/// Features of an atlas level (the mip with at most 1024 texels across) at `uv`: (mean gloss alpha, saturation, value, flatness) of the
/// 5x5 texels around the nearest texel, and the hue (degrees). Mirror of `ft_feat`.
#[allow(dead_code)] // the CPU mirror (tests, the docs' atlas measurements)
pub fn texel_features(img: &crate::furprobe::Img, uv: [f32; 2]) -> ([f32; 4], f32) {
    if img.w == 0 || img.h == 0 || img.px.len() < (img.w * img.h) as usize {
        return ([0.0; 4], 0.0);
    }
    let fr = |x: f32| x - x.floor();
    let c = [(fr(uv[0]) * img.w as f32).floor() as i64, (fr(uv[1]) * img.h as f32).floor() as i64];
    let (mut s, mut s2, mut sa) = ([0.0f32; 3], 0.0f32, 0.0f32);
    for y in -2..=2 {
        for x in -2..=2 {
            let t = img.at(c[0] + x, c[1] + y);
            for k in 0..3 {
                s[k] += t[k];
                s2 += t[k] * t[k];
            }
            sa += t[3];
        }
    }
    let m = [s[0] / 25.0, s[1] / 25.0, s[2] / 25.0];
    let mx = m[0].max(m[1]).max(m[2]);
    let mn = m[0].min(m[1]).min(m[2]);
    let mm = m[0] * m[0] + m[1] * m[1] + m[2] * m[2];
    let var = ((s2 / 25.0 - mm).max(0.0)) / 3.0;
    let fl = if mx > FLAT_CLIP { 1.0 } else { (2.0 * var.sqrt() / (mm / 3.0).sqrt().max(0.05)).clamp(0.0, 1.0) };
    let d = mx - mn;
    let mut hue = 0.0;
    if d > 1e-5 {
        let hh = if mx == m[0] { (m[1] - m[2]) / d } else if mx == m[1] { 2.0 + (m[2] - m[0]) / d } else { 4.0 + (m[0] - m[1]) / d };
        hue = fr(hh / 6.0 + 1.0) * 360.0;
    }
    ([(sa / 25.0).clamp(0.0, 1.0), d / mx.max(1e-3), mx, fl], hue)
}

/// v8: `texel_features` at `uv` with the flat and glossy tests dilated: gloss = the HIGHEST and flatness = the LOWEST of the centre 5x5
/// window and the four 5x5 windows `FT_DIL` texels away diagonally (saturation, value and hue stay the centre's). Mirror of
/// `ft_feat_dil` (what the strand pixel shader tests at every root).
#[allow(dead_code)] // the CPU mirror (tests, the docs' atlas measurements)
pub fn texel_features_dilated(img: &crate::furprobe::Img, uv: [f32; 2]) -> ([f32; 4], f32) {
    let (mut ft, hue) = texel_features(img, uv);
    if img.w == 0 || img.h == 0 || img.px.len() < (img.w * img.h) as usize {
        return (ft, hue);
    }
    let fr = |x: f32| x - x.floor();
    let c = [(fr(uv[0]) * img.w as f32).floor() as i64, (fr(uv[1]) * img.h as f32).floor() as i64];
    for (dx, dy) in [(FT_DIL, FT_DIL), (-FT_DIL, FT_DIL), (FT_DIL, -FT_DIL), (-FT_DIL, -FT_DIL)] {
        let (mut s, mut s2, mut sa) = ([0.0f32; 3], 0.0f32, 0.0f32);
        for y in -2..=2 {
            for x in -2..=2 {
                let t = img.at(c[0] + dx + x, c[1] + dy + y);
                for k in 0..3 {
                    s[k] += t[k];
                    s2 += t[k] * t[k];
                }
                sa += t[3];
            }
        }
        let m = [s[0] / 25.0, s[1] / 25.0, s[2] / 25.0];
        let mx = m[0].max(m[1]).max(m[2]);
        let mm = m[0] * m[0] + m[1] * m[1] + m[2] * m[2];
        let var = ((s2 / 25.0 - mm).max(0.0)) / 3.0;
        let fl = if mx > FLAT_CLIP { 1.0 } else { (2.0 * var.sqrt() / (mm / 3.0).sqrt().max(0.05)).clamp(0.0, 1.0) };
        ft[0] = ft[0].max((sa / 25.0).clamp(0.0, 1.0));
        ft[3] = ft[3].min(fl);
    }
    (ft, hue)
}

/// v9: the root of a strand on a fur triangle NEAR an excluded part (the island analysis, furisland.rs: it shares a vertex position
/// with a horn / claw island) also reads the four 5x5 windows FT_NEAR texels away diagonally (flat and glossy tests, like FT_DIL).
pub const FT_NEAR: i64 = 6;

/// v9: `texel_features_dilated` plus the FT_NEAR windows (gloss max, flatness min). Mirror of `ft_feat_dil` + `ft_near`.
#[allow(dead_code)] // the CPU mirror (tests)
pub fn texel_features_near(img: &crate::furprobe::Img, uv: [f32; 2]) -> ([f32; 4], f32) {
    let (mut ft, hue) = texel_features_dilated(img, uv);
    if img.w == 0 || img.h == 0 || img.px.len() < (img.w * img.h) as usize {
        return (ft, hue);
    }
    let fr = |x: f32| x - x.floor();
    let c = [(fr(uv[0]) * img.w as f32).floor() as i64, (fr(uv[1]) * img.h as f32).floor() as i64];
    for (dx, dy) in [(-FT_NEAR, -FT_NEAR), (FT_NEAR, -FT_NEAR), (-FT_NEAR, FT_NEAR), (FT_NEAR, FT_NEAR)] {
        let (mut s, mut s2, mut sa) = ([0.0f32; 3], 0.0f32, 0.0f32);
        for y in -2..=2 {
            for x in -2..=2 {
                let t = img.at(c[0] + dx + x, c[1] + dy + y);
                for k in 0..3 {
                    s[k] += t[k];
                    s2 += t[k] * t[k];
                }
                sa += t[3];
            }
        }
        let m = [s[0] / 25.0, s[1] / 25.0, s[2] / 25.0];
        let mx = m[0].max(m[1]).max(m[2]);
        let mm = m[0] * m[0] + m[1] * m[1] + m[2] * m[2];
        let var = ((s2 / 25.0 - mm).max(0.0)) / 3.0;
        let fl = if mx > FLAT_CLIP { 1.0 } else { (2.0 * var.sqrt() / (mm / 3.0).sqrt().max(0.05)).clamp(0.0, 1.0) };
        ft[0] = ft[0].max((sa / 25.0).clamp(0.0, 1.0));
        ft[3] = ft[3].min(fl);
    }
    (ft, hue)
}

#[allow(dead_code)] // the CPU mirror (tests, the docs' atlas measurements)
fn bx(lo: f32, hi: f32, soft: f32, x: f32) -> f32 {
    smooth(lo - soft, lo + soft, x) * (1.0 - smooth(hi - soft, hi + soft, x))
}

/// Distance of two hues in degrees (0..180).
#[allow(dead_code)] // the CPU mirror (tests, the docs' atlas measurements)
pub fn hue_dist(a: f32, b: f32) -> f32 {
    let d = (a - b).abs() % 360.0;
    d.min(360.0 - d)
}

/// The fur gloss band of `draw_weight` (alpha of the fur texels of every measured charr atlas: 0.40..0.44).
#[allow(dead_code)] // the CPU mirror (tests, the docs' atlas measurements)
pub const GLOSS_BAND: (f32, f32) = (0.33, 0.50);

/// The draw is fur at all: `s0` = (share of its footprint in the gloss band, median flatness, cells, -). Mirror of `ft_draw_weight`.
#[allow(dead_code)] // the CPU mirror (tests, the docs' atlas measurements)
pub fn draw_weight(s0: [f32; 4]) -> f32 {
    if !(s0[2] >= 16.0) {
        return 0.0;
    }
    smooth(0.40, 0.60, s0[0]) * smooth(0.03, 0.05, s0[1])
}

/// One root texel: 1 = fur, 0 = horn / claw / fang / lips (relative to the draw's coat `s0` / `s1`). Mirror of `ft_texel_weight`.
#[allow(dead_code)] // the CPU mirror (tests, the docs' atlas measurements)
pub fn texel_weight(ft: [f32; 4], hue: f32, s0: [f32; 4], s1: [f32; 4]) -> f32 {
    texel_weight_opt(ft, hue, s0, s1, true)
}

/// v15: `texel_weight` with the two COLOUR tests (keratin tan, red) on or off (`colour` false: a texel of a limb island that only its
/// colours excluded, furisland.rs `LIMB_*`: the tan / rust band of a two-tone coat is fur there; the flat and glossy tests still run).
/// Mirror of `ft_texel_weight` with its `colour`.
#[allow(dead_code)] // the CPU mirror (tests, the island votes)
pub fn texel_weight_opt(ft: [f32; 4], hue: f32, s0: [f32; 4], s1: [f32; 4], colour: bool) -> f32 {
    let mut w = 1.0f32;
    let th = 0.025f32.min(0.4 * s0[1]);
    w *= smooth(0.5 * th, 1.5 * th, ft[3]);
    w *= 1.0 - smooth(s1[0] + 0.03, s1[0] + 0.07, ft[0]);
    let differs = smooth(0.10, 0.20, (ft[1] - s1[1]).abs()).max(smooth(0.35, 0.65, ((ft[2] + 0.02) / (s1[2] + 0.02)).log2()));
    let gate = (1.0 - smooth(0.28, 0.38, s1[1])).max(smooth(18.0, 28.0, hue_dist(s1[3], 32.0)));
    let ker = bx(15.0, 55.0, 5.0, hue) * bx(0.22, 0.65, 0.04, ft[1]) * bx(0.25, 0.90, 0.04, ft[2]) * differs * gate;
    let red_differs = smooth(0.15, 0.25, ft[1] - s1[1]).max(smooth(10.0, 18.0, hue_dist(hue, s1[3])) * smooth(0.35, 0.45, ft[1]));
    let red = (1.0 - smooth(15.0, 25.0, hue_dist(hue, 0.0))) * smooth(0.30, 0.40, ft[1]) * red_differs;
    // v19, grey keratin (mirror of `ft_texel_weight`): a colourless horn or claw a stop or more off the coat's value.
    let grey = (1.0 - smooth(0.16, 0.24, ft[1])) * smooth(0.6, 1.0, ((ft[2] + 0.02) / (s1[2] + 0.02)).log2().abs());
    if colour {
        w *= 1.0 - ker;
        w *= 1.0 - red;
        w *= 1.0 - grey;
    }
    w.clamp(0.0, 1.0)
}

/// The coat statistics of a footprint (the features of its cells, the hue as `ps_feat` packs it: whole degrees): (row 0, row 1) of
/// `ps_stats`. Mirror.
#[allow(dead_code)] // the CPU mirror (tests, the docs' atlas measurements)
pub fn coat_stats(cells: &[([f32; 4], f32)]) -> ([f32; 4], [f32; 4]) {
    let n = cells.len() as f32;
    if cells.is_empty() {
        return ([0.0; 4], [0.0; 4]);
    }
    let mut hist = [[0.0f32; 32]; 4];
    let (mut band, mut hs) = (0.0f32, [0.0f32; 2]);
    for (f, hue) in cells {
        let hue = hue.floor();
        let fl = f[3].min(0.999);
        if f[0] >= GLOSS_BAND.0 && f[0] <= GLOSS_BAND.1 {
            band += 1.0;
        }
        for (k, v) in [f[0], f[1], f[2], fl].into_iter().enumerate() {
            hist[k][((v.clamp(0.0, 1.0) * 32.0) as usize).min(31)] += 1.0;
        }
        let r = hue.to_radians();
        hs[0] += f[1] * r.cos();
        hs[1] += f[1] * r.sin();
    }
    let med = |h: &[f32; 32]| {
        let mut acc = 0.0;
        for (b, &c) in h.iter().enumerate() {
            if acc + c >= 0.5 * n && c > 0.0 {
                return (b as f32 + ((0.5 * n - acc) / c).clamp(0.0, 1.0)) / 32.0;
            }
            acc += c;
        }
        1.0
    };
    let mut s0 = [band / n, med(&hist[3]), n, 0.0];
    s0[3] = draw_weight(s0);
    let hue_ref = if (hs[0] * hs[0] + hs[1] * hs[1]).sqrt() > 1e-4 { (hs[1].atan2(hs[0]) / std::f32::consts::TAU + 1.0).fract() * 360.0 } else { 0.0 };
    (s0, [med(&hist[0]), med(&hist[1]), med(&hist[2]), hue_ref])
}

// ---- the frame's unit (engine units per metre), measured and smoothed ------------------------------------------------------

/// Fewest measured triangles a frame needs before its measurement counts.
pub const UNIT_MIN_TRIS: f32 = 12.0;
/// Weight of a new frame in the running mean (log2 domain).
pub const UNIT_EMA: f32 = 0.15;
/// The largest step one frame may move the running mean (log2): a wrong frame cannot throw the unit.
pub const UNIT_MAX_STEP: f32 = 0.1;
/// Hysteresis (log2, about 1 %): the unit the shells use only changes when the running mean moved farther than this from it, so the
/// strands never "breathe" with the measurement noise.
pub const UNIT_HYST: f32 = 0.015;

/// The measured unit: the sum the GPU added up (`gs_measure`: log2 upm, its square, the count) of one frame goes in, the published
/// unit (the one the shells use) changes only by more than the hysteresis.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct UnitTrack {
    /// The unit the shells use (engine units per metre; 0 = none yet).
    pub published: f32,
    /// The running mean (log2).
    pub mean: f32,
    /// The last measurement: triangles and spread (standard deviation, log2).
    pub tris: f32,
    pub spread: f32,
    /// Frames measured so far.
    pub frames: u32,
}

impl UnitTrack {
    /// One frame's sum; true when the published unit changed.
    pub fn feed(&mut self, s: [f32; 4]) -> bool {
        let n = s[2];
        if !(n >= UNIT_MIN_TRIS) || !s[0].is_finite() || !s[1].is_finite() {
            return false;
        }
        let m = s[0] / n;
        if !m.is_finite() || m.abs() > 40.0 {
            return false;
        }
        self.tris = n;
        self.spread = (s[1] / n - m * m).max(0.0).sqrt();
        self.frames = self.frames.saturating_add(1);
        if !(self.published > 0.0) {
            self.mean = m;
            self.published = m.exp2();
            return true;
        }
        self.mean += ((m - self.mean) * UNIT_EMA).clamp(-UNIT_MAX_STEP * UNIT_EMA, UNIT_MAX_STEP * UNIT_EMA);
        if (self.mean - self.published.log2()).abs() > UNIT_HYST {
            self.published = self.mean.exp2();
            return true;
        }
        false
    }
}

// ---- live state --------------------------------------------------------------------------------------------------

static CFG: Mutex<Option<ShellCfg>> = Mutex::new(None);
static SEL: Mutex<Option<Selector>> = Mutex::new(None);
/// The shells are wanted (fur_shells > 0): one atomic per fur draw when off.
static ON: AtomicBool = AtomicBool::new(false);
/// `fur_auto` (v18, furselect.rs).
static AUTO: AtomicBool = AtomicBool::new(true);
/// The strand pixel shader and the unit measurement's pixel shader.
static PS_BLOB: OnceLock<Result<PsBlobs, String>> = OnceLock::new();

/// The static shaders of the shell pass (compiled once on a background thread).
pub(crate) struct PsBlobs {
    pub shell: Vec<u8>,
    pub measure: Vec<u8>,
    /// v7: the per-draw coat statistics.
    pub feat: Vec<u8>,
    pub stats_vs: Vec<u8>,
    pub stats: Vec<u8>,
    /// v11: the skin pass.
    pub skin: Vec<u8>,
}
static FAILED: AtomicBool = AtomicBool::new(false);
static FAIL_LOGGED: AtomicBool = AtomicBool::new(false);
static START: OnceLock<std::time::Instant> = OnceLock::new();
/// Tests only: the shells use the per-triangle unit guess (the first version) instead of the frame's measured unit.
pub(crate) static FORCE_TRI: AtomicBool = AtomicBool::new(false);

/// The content rule of furselect.rs is wanted: `fur_auto` 1 and the shells can run (the draw test that decides lives in their statistics
/// pass: without the shells a candidate of another shader would get the old fur look untested).
pub fn auto_on() -> bool {
    AUTO.load(Relaxed) && ON.load(Relaxed) && !FAILED.load(Relaxed) && !crate::toggles::master_off() && matches!(PS_BLOB.get(), Some(Ok(_)))
}

pub fn auto_configured() -> bool {
    AUTO.load(Relaxed)
}

pub fn cfg() -> ShellCfg {

    CFG.lock().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or_default()
}

/// Applies the config (start, hot reload, overlay).
pub fn configure(c: &ShellCfg) {
    let c = c.clamped();
    ON.store(c.shells > 0, Relaxed);
    AUTO.store(c.auto != 0, Relaxed);
    *SEL.lock().unwrap_or_else(|e| e.into_inner()) = Some(Selector::new(&c));
    *DISCRIM.lock().unwrap_or_else(|e| e.into_inner()) = Discrim::new(&c);
    crate::furprobe::set_debug2(c.debug == 2);
    crate::furisland::set_debug2(c.debug == 2);
    *CFG.lock().unwrap_or_else(|e| e.into_inner()) = Some(c);
    crate::ao::set_depth_consumer(crate::ao::DEPTH_FOR_FUR, ON.load(Relaxed));
}

/// Start: the config, and the strand pixel shader compiled on a background thread.
pub fn init(c: &ShellCfg) {
    configure(c);
    let _ = START.set(std::time::Instant::now());
    let _ = std::thread::Builder::new().name("gw2g-shell-ps".into()).spawn(|| {
        let r = compile_pixel_shaders();
        if let Err(e) = &r {
            log!("shellfur: strand shader compile failed, the shells stay off: {e}");
        }
        let _ = PS_BLOB.set(r);
    });
}

fn compile_pixel_shaders() -> Result<PsBlobs, String> {
    Ok(PsBlobs {
        shell: shaders::compile(HLSL, PS_ENTRY, "ps_5_0")?,
        measure: shaders::compile(HLSL, MEASURE_PS_ENTRY, "ps_5_0")?,
        feat: shaders::compile(HLSL, FEAT_PS_ENTRY, "ps_5_0")?,
        stats_vs: shaders::compile(HLSL, STATS_VS_ENTRY, "vs_5_0")?,
        stats: shaders::compile(HLSL, STATS_PS_ENTRY, "ps_5_0")?,
        skin: shaders::compile(HLSL, SKIN_PS_ENTRY, "ps_5_0")?,
    })
}

fn time_s() -> f64 {
    START.get().map_or(0.0, |s| s.elapsed().as_secs_f64())
}

fn fail_once(why: &str) {
    if !FAIL_LOGGED.swap(true, Relaxed) {
        log!("shellfur: {why}; the shells stay off this frame (the old fur look is unchanged)");
    }
}

// ---- per-frame status: the fur draws and what the selector said ---------------------------------------------------

/// One fur draw of the last frame (for `status_shells=`).
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct DrawInfo {
    pub vs: Option<u32>,
    pub ps: u32,
    pub vcb0: u32,
    pub count: u32,
    pub instances: u32,
    pub srvs: u32,
    pub verdict: Option<&'static str>,
    pub shells: bool,
}

const MAX_INFO: usize = 24;

#[derive(Default)]
struct Frame {
    draws: Vec<DrawInfo>,
    dropped: u32,
    replayed: u32,
    pending: u32,
    gs_failed: u32,
    micros: u32,
    why: &'static str,
    /// v11: records drawn into the skin target, and what the light-adjust draw got at t25 (SKIN_*).
    skin: u32,
    skin_bound: u8,
    /// v14: every fur draw of the frame by verdict (listed or not): (verdict, draws).
    reasons: Vec<(&'static str, u32)>,
    /// v14: records whose dynamic buffers were copied, the bytes copied, the triangles recorded.
    dyn_copied: u32,
    dyn_bytes: u64,
    tris: u32,
    /// v16: records displaced by a draw that had shells in the previous frame.
    evicted: u32,
}

/// v11, `Frame::skin_bound`: t25 unbound (the shells off / not ready: the old look and the rim on the whole mask, as in v10).
const SKIN_UNBOUND: u8 = 0;
/// t25 = the skin target of this frame.
const SKIN_TARGET: u8 = 1;
/// t25 = the 1x1 zero texture (the shells on, nothing drawn: no old look, no rim).
const SKIN_NONE: u8 = 2;

static CUR: Mutex<Option<Frame>> = Mutex::new(None);
static LAST: Mutex<Option<Frame>> = Mutex::new(None);

fn with_cur(f: impl FnOnce(&mut Frame)) {
    if let Ok(mut g) = CUR.try_lock() {
        f(g.get_or_insert_with(|| Frame { draws: Vec::with_capacity(MAX_INFO), ..Default::default() }));
    }
}

/// A new frame starts (frame tick): the status rolls over, records left from a frame without a light-adjust draw are released.
pub fn frame_begin() {
    if let (Ok(mut c), Ok(mut l)) = (CUR.try_lock(), LAST.try_lock()) {
        let mut next = l.take().unwrap_or_default();
        next.draws.clear();
        (next.dropped, next.replayed, next.pending, next.gs_failed, next.micros, next.why) = (0, 0, 0, 0, 0, "");
        (next.skin, next.skin_bound) = (0, SKIN_UNBOUND);
        next.reasons.clear();
        (next.dyn_copied, next.dyn_bytes, next.tris, next.evicted) = (0, 0, 0, 0);
        *l = c.take();
        *c = Some(next);
    }
    if let Ok(mut s) = STATE.try_lock() {
        if let Some(st) = s.as_mut() {
            st.clear_recs();
            st.drawn_frame = false;
        }
    }
}

/// v14: every fur draw of the frame by verdict (also the ones past the listed MAX_INFO), the records' cap / budgets and the copies.
fn reasons_text(f: &Frame) -> String {
    let mut r = f.reasons.clone();
    r.sort_by(|a, b| b.1.cmp(&a.1));
    format!(
        "v14/v16 records: all {} fur draws by verdict [{}]; cap {MAX_RECS} records / {MAX_FRAME_TRIS} triangles a frame ({} recorded; {} displaced by a draw shelled last frame); {} with a dynamic buffer copied ({:.1} MB)",
        r.iter().map(|x| x.1).sum::<u32>(),
        r.iter().map(|(w, n)| format!("{w}: {n}")).collect::<Vec<_>>().join(", "),
        f.tris,
        f.evicted,
        f.dyn_copied,
        f.dyn_bytes as f64 / 1_048_576.0
    )
}

/// `status_shells=`: settings, what the last frame did and every fur draw with the selector's verdict.
pub fn status_text() -> String {
    let c = cfg();
    let stats = STATS_SHOW.lock().map(|g| g.clone()).unwrap_or_default();
    // The island dumps are written by the debug capture itself (debugshot), not by every look at this text (the overlay
    // draws it each frame).
    let disc = format!(
        "{}; {}; {}; {}; {}",
        stats_text(&stats, Discrim::new(&c).sel.is_some()),
        crate::furisland::status_text(),
        discrim_text(&c),
        crate::furmask::discrim_probe_text(),
        crate::furprobe::report_text()
    );
    let t = status_core(&c);
    format!("{t}; {disc}")
}

fn status_core(c: &ShellCfg) -> String {
    let c = c.clone();
    if c.shells == 0 {
        return format!("off (fur_shells {})", c.shells);
    }
    if FAILED.load(Relaxed) {
        return "failed (see log), off".to_string();
    }
    match PS_BLOB.get() {
        None => return "compiling the strand shader".to_string(),
        Some(Err(_)) => return "failed (strand shader compile, see log), off".to_string(),
        Some(Ok(_)) => {}
    }
    let head = format!(
        "{} shells, length {} cm, density {}, thickness {}, root dark {}, wind {}, LOD {} m, rim {}, detail {}, clump {}, normal {}, unit {}",
        c.shells,
        c.length,
        c.density,
        c.thickness,
        c.root_dark,
        c.wind,
        c.lod,
        c.rim,
        c.detail,
        c.clump,
        c.normal,
        if c.unit > 0.0 { format!("{} per m (fixed)", c.unit) } else { unit_text(&unit_track()) }
    );
    let sel = format!(
        "selector: shell_ps [{}], shell_vs [{}], shell_min_vcb {}, shell_counts [{}]",
        if c.ps.is_empty() { "= fur_ps" } else { c.ps.as_str() },
        if c.vs.is_empty() { "any" } else { c.vs.as_str() },
        if c.min_vcb == MIN_VCB_LEGACY { format!("{} (the pre-v14 default: in effect 0)", c.min_vcb) } else { c.min_vcb.to_string() },
        if c.counts.is_empty() { "all" } else { c.counts.as_str() }
    );
    let l = LAST.lock().unwrap_or_else(|e| e.into_inner());
    let Some(f) = l.as_ref() else { return format!("{head}; no frame yet; {sel}") };
    let picked = f.draws.iter().filter(|d| d.shells).count();
    let mut t = format!(
        "{head}; last frame: {} draws drew shells ({picked} picked of {} fur draws{}), CPU {:.3} ms{}{}{}",
        f.replayed,
        f.draws.len() as u32 + f.dropped,
        if f.dropped > 0 { format!(", {} not listed", f.dropped) } else { String::new() },
        f.micros as f32 / 1000.0,
        if f.pending > 0 { format!(", {} waiting for the geometry shader", f.pending) } else { String::new() },
        if f.gs_failed > 0 { format!(", {} without a usable geometry shader (see log)", f.gs_failed) } else { String::new() },
        if f.why.is_empty() { String::new() } else { format!(", {}", f.why) }
    );
    t.push_str(&format!("; {}", gate_of(f)));
    t.push_str(&format!("; {}", reasons_text(f)));
    t.push_str(&format!("; {sel}; draws:"));
    for d in &f.draws {
        t.push_str(&format!(
            " [vs {} ps {:08x} vcb {} count {} inst {} srv {} -> {}]",
            d.vs.map_or("unknown".to_string(), |v| format!("{v:08x}")),
            d.ps,
            d.vcb0,
            d.count,
            d.instances,
            d.srvs,
            if d.shells { "shells" } else { d.verdict.unwrap_or("skipped") }
        ));
    }
    if f.draws.is_empty() {
        t.push_str(" none (no fur draw was masked: fur_ps, fur_strength, the fur mask)");
    }
    t
}

/// v11: what gated the old fur look and the rim light in the frame `f` (the skin pass, `t25`).
fn gate_of(f: &Frame) -> String {
    match f.skin_bound {
        SKIN_TARGET => format!(
            "old fur look + rim gate (v11): only on proven fur skin, the skin pass of {} recorded draws (their draw test, island bits and texel tests, the same verdict as the strands; not fur draws = hair cards / armour / other skin, excluded islands = horns / claws / fang, cut-out card pixels and pixels whose fur mask holds another draw's uv get none; fur draws not recorded get none)",
            f.skin
        ),
        SKIN_NONE => "old fur look + rim gate (v11): no draw recorded this frame, so no proven fur: the old look and the rim are off on the whole mask".to_string(),
        _ => "old fur look + rim gate (v11): not bound this frame (shells not ready): the old look and the rim on the whole mask as before v11".to_string(),
    }
}

/// `status_furlook=` / `status_rim=` suffix (v11): the gate of the last frame in one line.
pub fn gate_text() -> String {
    let c = cfg();
    if c.shells == 0 {
        return "gate: none (shells off: the old look / rim on the whole mask, no classification)".to_string();
    }
    let l = LAST.lock().unwrap_or_else(|e| e.into_inner());
    match l.as_ref() {
        Some(f) => match f.skin_bound {
            SKIN_TARGET => format!("gate: proven fur skin only (v11 skin pass, {} draws; hair cards, armour, horns, claws skipped)", f.skin),
            SKIN_NONE => "gate: no proven fur this frame (no draw recorded): off on the whole mask".to_string(),
            _ => "gate: not bound last frame (shells not ready): the whole mask".to_string(),
        },
        None => "gate: no frame yet".to_string(),
    }
}

/// A copy of the measured unit for the status text (its own lock: a status reader on another thread never makes the render thread's
/// `try_lock` of the shell state fail, which would drop that frame's shells).
static UNIT_SHOW: Mutex<UnitTrack> = Mutex::new(UnitTrack { published: 0.0, mean: 0.0, tris: 0.0, spread: 0.0, frames: 0 });

/// v7: the coat statistics of one replayed draw as the GPU computed them (`ps_stats`), read back a few frames later.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct DrawStats {
    pub count: u32,
    pub atlas: usize,
    pub s0: [f32; 4],
    pub s1: [f32; 4],
}

static STATS_SHOW: Mutex<Vec<DrawStats>> = Mutex::new(Vec::new());

/// The `status_shells=` part of the per-strand discriminator: per replayed draw its coat statistics and verdict.
pub fn stats_text(v: &[DrawStats], texel_tests: bool) -> String {
    let mut t = format!(
        "per-strand fur discriminator (v8, at each strand's own root uv; texel tests {}; roots: the engine's depth = the base triangle's depth plane + the draw's bias; fragments: hidden by the linear AND the device depth; no shells on triangles seen edge-on, |cos| < {FACE_LO:.2}..{FACE_HI:.2}): ",
        if texel_tests {
            format!("on: flat (horn) and glossier than the coat (claws, fang) also {FT_DIL} texels around the root, keratin tan / red unlike the coat (claws, lips)")
        } else {
            "off (shell_fur_mask=off): draw test only".to_string()
        }
    );
    if v.is_empty() {
        t.push_str("no statistics yet (two frames after the first shells)");
        return t;
    }
    for d in v {
        let w = d.s0[3];
        t.push_str(&format!(
            "[count {} atlas {:08x}: {} cells, gloss band {:.0} %, flat {:.3}, coat gloss {:.2} sat {:.2} value {:.2} hue {:.0} -> {}] ",
            d.count,
            d.atlas as u32,
            d.s0[2] as u32,
            d.s0[0] * 100.0,
            d.s0[1],
            d.s1[0],
            d.s1[1],
            d.s1[2],
            d.s1[3],
            if d.s0[2] < 16.0 { "no footprint, no shells".to_string() } else if w >= 0.99 { "fur draw".to_string() } else if w <= 0.01 { "not fur (armour / skin / hair), no shells, no old fur look / rim".to_string() } else { format!("fur x {w:.2}") }
        ));
    }
    t
}

/// The measured unit of the current device (a copy).
pub(crate) fn unit_track() -> UnitTrack {
    *UNIT_SHOW.lock().unwrap_or_else(|e| e.into_inner())
}

/// `auto: 39.4 per m (measured: 5321 triangles, spread 0.4 %, 120 frames)` / `auto: measuring (...)`.
pub fn unit_text(u: &UnitTrack) -> String {
    if u.published > 0.0 {
        format!(
            "auto: {:.4} per m (measured over {} triangles, spread {:.2} %, {} frames; smoothed, changes only by more than {:.1} %)",
            u.published,
            u.tris as u32,
            (u.spread.exp2() - 1.0) * 100.0,
            u.frames,
            (UNIT_HYST.exp2() - 1.0) * 100.0
        )
    } else {
        "auto: measuring (the first frames use a per-triangle guess)".to_string()
    }
}

// ---- recording -----------------------------------------------------------------------------------------------

/// One recorded draw: every object holds a counted reference (released after the replay or at the next frame).
struct Rec {
    call: Call,
    vs_ptr: usize,
    vs: Obj,
    layout: Obj,
    topology: u32,
    vb: [Obj; 4],
    strides: [u32; 4],
    offsets: [u32; 4],
    ib: Obj,
    ib_fmt: u32,
    ib_off: u32,
    /// Our copies of the vertex constant buffers (owned by the pool, not released with the record).
    vcb: [Obj; 4],
    vsrv: [Obj; 4],
    vsmp: [Obj; 2],
    rs: Obj,
    vp: Viewport,
    /// Scissor rectangle 0 (used when the rasterizer state enables the scissor test).
    scissor: [i32; 4],
    ccw: bool,
    /// The depth buffer the draw used (its final contents tell the geometry shader which vertices are visible).
    depth: Obj,
    /// v7: the draw's own colour atlas (its pixel shader's t0): the per-strand fur discriminator and the coat statistics read it.
    tex0: Obj,
    /// v8: its rasterizer state's description (D3D11_RASTERIZER_DESC as u32s; zero = the default): the depth bias for the root test.
    rsd: [u32; 10],
    /// v14: its dynamic vertex / index buffers are our copies (`Shell::copy_dyn`).
    copied: bool,
    /// v16: the draw's identity across frames (atlas, vertex shader, index count, topology): the hysteresis of the record budget.
    id: u64,
    /// v19: admitted by the content rule (`fur_auto`), not by `fur_ps`: its coat statistics are measured, but no strand is drawn
    /// (shells, skin pass) until its atlas is proven fur, so a scaly or leathery creature never shows fur, not even for a frame.
    auto: bool,
}

impl Rec {
    unsafe fn release(&mut self) {
        unsafe {
            for o in [self.vs, self.layout, self.ib, self.rs, self.depth, self.tex0].into_iter().chain(self.vb).chain(self.vsrv).chain(self.vsmp) {
                release(o);
            }
        }
        let nul = std::ptr::null_mut();
        (self.vs, self.layout, self.ib, self.rs, self.depth, self.tex0) = (nul, nul, nul, nul, nul, nul);
        self.vb = [nul; 4];
        self.vsrv = [nul; 4];
        self.vsmp = [nul; 2];
    }
}

/// Device objects and the records of one device.
struct Shell {
    device: usize,
    recs: Vec<Rec>,
    /// Copies of the vertex constant buffers: per record index and slot, (byte width, buffer).
    pool: Vec<[(u32, Obj); 4]>,
    /// v16: the identities that had shells in the previous frame, and how many records were displaced this frame.
    prev_ids: std::collections::HashSet<u64>,
    evicted_frame: u32,
    /// v14: the frame's dynamic-buffer copies (records, bytes).
    dyn_copied_frame: u32,
    dyn_bytes_frame: u64,
    /// v14: copies of DYNAMIC vertex buffers 0..3 and the index buffer (slot 4): per record index, (byte width, bind flags, buffer).
    dyn_pool: Vec<[(u32, u32, Obj); 5]>,
    target: Target,
    size: (u32, u32),
    ps: Obj,
    /// Per vertex layout: (key, shell geometry shader, measurement geometry shader, uv-space geometry shader, v9 stream-output capture).
    gs: Vec<(String, Obj, Obj, Obj, Obj)>,
    /// v9: the per-mesh island analyses of this device (furisland.rs).
    islands: crate::furisland::IslandGpu,
    /// v7, the per-draw coat statistics: the uv-space features (FEAT_N x FEAT_N cells per record, RGBA32F), the statistics (MAX_RECS x 2,
    /// RGBA32F), their shaders, and a staging ring that brings the statistics back to the CPU for `status_shells=` (never waiting).
    feat: Target,
    stats: Target,
    ps_feat: Obj,
    vs_stats: Obj,
    ps_stats: Obj,
    stat_stage: [Obj; 3],
    stat_staged: [bool; 3],
    /// Per staged copy: the records it holds (index count, atlas pointer) in order.
    stat_recs: [[(u32, usize); MAX_RECS]; 3],
    stat_n: [usize; 3],
    stat_runs: u32,
    blend: Obj,
    dss: Obj,
    rs_fill: Obj,
    cb_gs: Obj,
    cb_ps: Obj,
    cb_comp: Obj,
    /// The unit measurement: its pixel shader, ADD blend, a rasterizer state without culling, the 1x1 RGBA32F sum, the staging ring
    /// the CPU reads (never waiting) and the smoothed result.
    ps_meas: Obj,
    blend_add: Obj,
    /// The shells' own depth buffer (D32_FLOAT, the target's size): it keeps the best-scoring fragment per pixel (see ps_shell).
    zbuf: Obj,
    zdsv: Obj,
    rs_meas: Obj,
    meas: Target,
    stage: [Obj; 3],
    staged: [bool; 3],
    runs: u32,
    unit: UnitTrack,
    /// The shells were drawn this frame (`t24` holds them): a second light-adjust draw of the same frame binds them again.
    drawn_frame: bool,
    /// v11: the skin pass's pixel shader, its target (R8_UNORM, the shell target's size; MAX blend) and the 1x1 zero texture bound
    /// instead when the shells are on but nothing was drawn (no proven fur: the old look and the rim stay off).
    ps_skin: Obj,
    skin: Target,
    skin_none: Target,
    /// Records drawn into `skin` by the last replay.
    skin_n: u32,
    depth_srv: Option<(usize, Obj)>,
    /// A shader view of the engine's depth buffer (by resource pointer; null = no shader can read that format).
    dev_srv: Option<(usize, Obj)>,
}

unsafe impl Send for Shell {}

static STATE: Mutex<Option<Shell>> = Mutex::new(None);

type CreateBufferFn = unsafe extern "system" fn(Obj, *const [u32; 6], *const std::ffi::c_void, *mut Obj) -> i32;

impl Shell {
    unsafe fn new(dev: Obj, blobs: &PsBlobs) -> Option<Shell> {
        let _ig = IgnoreScope::new();
        let nul = std::ptr::null_mut();
        let mut s = Shell {
            device: dev as usize,
            recs: Vec::with_capacity(MAX_RECS),
            pool: Vec::with_capacity(MAX_RECS),
            dyn_pool: Vec::new(),
            dyn_copied_frame: 0,
            prev_ids: std::collections::HashSet::new(),
            evicted_frame: 0,
            dyn_bytes_frame: 0,
            target: NO_TARGET,
            size: (0, 0),
            ps: nul,
            gs: Vec::new(),
            islands: crate::furisland::IslandGpu::new(),
            feat: NO_TARGET,
            stats: NO_TARGET,
            ps_feat: nul,
            vs_stats: nul,
            ps_stats: nul,
            stat_stage: [nul; 3],
            stat_staged: [false; 3],
            stat_recs: [[(0, 0); MAX_RECS]; 3],
            stat_n: [0; 3],
            stat_runs: 0,
            blend: nul,
            dss: nul,
            rs_fill: nul,
            cb_gs: nul,
            cb_ps: nul,
            cb_comp: nul,
            ps_meas: nul,
            blend_add: nul,
            zbuf: nul,
            zdsv: nul,
            rs_meas: nul,
            meas: NO_TARGET,
            stage: [nul; 3],
            staged: [false; 3],
            runs: 0,
            unit: UnitTrack::default(),
            drawn_frame: false,
            ps_skin: nul,
            skin: NO_TARGET,
            skin_none: NO_TARGET,
            skin_n: 0,
            depth_srv: None,
            dev_srv: None,
        };
        unsafe {
            let create_ps: unsafe extern "system" fn(Obj, *const u8, usize, Obj, *mut Obj) -> i32 = std::mem::transmute(vfn(dev, vt::DEV_CREATE_PIXEL_SHADER));
            if create_ps(dev, blobs.shell.as_ptr(), blobs.shell.len(), nul, &mut s.ps) < 0 || s.ps.is_null() {
                return None;
            }
            if create_ps(dev, blobs.measure.as_ptr(), blobs.measure.len(), nul, &mut s.ps_meas) < 0 || s.ps_meas.is_null() {
                return None;
            }
            // v7: the coat statistics (without them no draw gets shells: fail closed).
            let create_vs: unsafe extern "system" fn(Obj, *const u8, usize, Obj, *mut Obj) -> i32 = std::mem::transmute(vfn(dev, vt::DEV_CREATE_VERTEX_SHADER));
            if create_ps(dev, blobs.feat.as_ptr(), blobs.feat.len(), nul, &mut s.ps_feat) < 0
                || create_ps(dev, blobs.stats.as_ptr(), blobs.stats.len(), nul, &mut s.ps_stats) < 0
                || create_vs(dev, blobs.stats_vs.as_ptr(), blobs.stats_vs.len(), nul, &mut s.vs_stats) < 0
            {
                return None;
            }
            // v11: the skin pass (without it no old fur look / rim on any fur pixel while the shells are on: fail closed).
            if create_ps(dev, blobs.skin.as_ptr(), blobs.skin.len(), nul, &mut s.ps_skin) < 0 || s.ps_skin.is_null() {
                return None;
            }
            s.skin_none = make_target(dev, 1, 1, SKIN_FORMAT, true)?;
            s.feat = make_target(dev, FEAT_N, FEAT_N * MAX_RECS as u32, F32X4, false)?;
            s.stats = make_target(dev, MAX_RECS as u32, 2, F32X4, false)?;
            // MAX blend on render target 0 (the packed keys order like floats), the rest unused.
            let mut bd = [0u32; 66];
            for rt in 0..8 {
                bd[2 + rt * 8..2 + rt * 8 + 8].copy_from_slice(&[0, 2, 1, 1, 2, 1, 1, 0xf]);
            }
            bd[2..10].copy_from_slice(&[1, 2, 2, 5 /* MAX */, 2, 2, 5, 0xf]);
            let create_bs: unsafe extern "system" fn(Obj, *const [u32; 66], *mut Obj) -> i32 = std::mem::transmute(vfn(dev, vt::DEV_CREATE_BLEND_STATE));
            // Our own depth buffer only orders the fragments by score (LESS, write); the strand shader tests the engine's linear depth
            // itself. Stencil off.
            let dd: [u32; 13] = [1, 1, 2, 0, 0xffff, 1, 1, 1, 8, 1, 1, 1, 8];
            let create_dss: unsafe extern "system" fn(Obj, *const [u32; 13], *mut Obj) -> i32 = std::mem::transmute(vfn(dev, 21));
            let create_buffer: CreateBufferFn = std::mem::transmute(vfn(dev, vt::DEV_CREATE_BUFFER));
            let cbd = |n: u32| [n, 2, 4, 0x10000, 0, 0];
            // ADD blend (ONE, ONE) for the unit measurement's sum.
            let mut ba = [0u32; 66];
            for rt in 0..8 {
                ba[2 + rt * 8..2 + rt * 8 + 8].copy_from_slice(&[0, 2, 1, 1, 2, 1, 1, 0xf]);
            }
            ba[2..10].copy_from_slice(&[1, 2, 2, 1 /* ADD */, 2, 2, 1, 0xf]);
            if create_bs(dev, &bd, &mut s.blend) < 0
                || create_bs(dev, &ba, &mut s.blend_add) < 0
                || create_dss(dev, &dd, &mut s.dss) < 0
                || create_buffer(dev, &cbd(96), std::ptr::null(), &mut s.cb_gs) < 0
                || create_buffer(dev, &cbd(std::mem::size_of::<PsCb>() as u32), std::ptr::null(), &mut s.cb_ps) < 0
                || create_buffer(dev, &cbd(std::mem::size_of::<[[f32; 4]; 2]>() as u32), std::ptr::null(), &mut s.cb_comp) < 0
            {
                return None;
            }
            // A rasterizer state for records without one (the default: solid, cull back, clockwise front, depth clip).
            let rd: [u32; 10] = [3, 3, 0, 0, 0, 0, 1, 0, 0, 0];
            let create_rs: unsafe extern "system" fn(Obj, *const [u32; 10], *mut Obj) -> i32 = std::mem::transmute(vfn(dev, 22));
            if create_rs(dev, &rd, &mut s.rs_fill) < 0 {
                return None;
            }
            // The measurement's full-viewport triangles: no culling.
            let rn: [u32; 10] = [3, 1, 0, 0, 0, 0, 1, 0, 0, 0];
            if create_rs(dev, &rn, &mut s.rs_meas) < 0 {
                return None;
            }
            // The 1x1 sum (RGBA32F) and three staging copies the CPU reads with DO_NOT_WAIT. Without them the unit stays per triangle.
            match make_target(dev, 1, 1, 2 /* R32G32B32A32_FLOAT */, false) {
                Some(t) => s.meas = t,
                None => return None,
            }
            let create_tex: unsafe extern "system" fn(Obj, *const crate::d3d::Tex2dDesc, *const std::ffi::c_void, *mut Obj) -> i32 = std::mem::transmute(vfn(dev, vt::DEV_CREATE_TEXTURE2D));
            let sd = crate::d3d::Tex2dDesc { width: 1, height: 1, mips: 1, array: 1, format: 2, samples: 1, quality: 0, usage: 3 /* STAGING */, bind: 0, cpu: 0x20000 /* READ */, misc: 0 };
            for i in 0..3 {
                if create_tex(dev, &sd, std::ptr::null(), &mut s.stage[i]) < 0 {
                    s.stage[i] = nul;
                }
            }
            let ss = crate::d3d::Tex2dDesc { width: MAX_RECS as u32, height: 2, ..sd };
            for i in 0..3 {
                if create_tex(dev, &ss, std::ptr::null(), &mut s.stat_stage[i]) < 0 {
                    s.stat_stage[i] = nul;
                }
            }
        }
        Some(s)
    }

    fn clear_recs(&mut self) {
        // v16: the identities that had shells this frame (the next frame's priority)
        self.prev_ids = self.recs.iter().map(|r| r.id).collect();
        self.evicted_frame = 0;
        for r in &mut self.recs {
            unsafe { r.release() };
        }
        self.recs.clear();
        (self.dyn_bytes_frame, self.dyn_copied_frame) = (0, 0);
    }

    /// v16: the record a draw of identity `id` (`tris` triangles) may replace when the frame is full: only a draw that had shells in
    /// the previous frame displaces one that did not (the last such record, so the replacement keeps the triangle budget), else none.
    fn victim(&self, id: u64, tris: u32) -> Option<usize> {
        let recs: Vec<(u64, u32)> = self.recs.iter().map(|r| (r.id, (r.call.count() / 3).saturating_mul(r.call.instances().max(1)))).collect();
        victim_of(&self.prev_ids, &recs, id, tris)
    }

    /// v14 / v15: our DEFAULT copy of `len` bytes from `lo` of the DYNAMIC buffer `src` (`bind` flags) for record `i`, slot `slot` (0..3
    /// vertex, 4 index): created on first use (or when it must grow / the binding changes), reused every frame; the window is copied at the draw, so
    /// the replay draws exactly what the engine drew even if the engine rewrites (Map DISCARD) the buffer before the end of the frame.
    /// Returns the copy with an extra reference (the record releases it like an engine buffer), or null.
    unsafe fn copy_dyn(&mut self, dev: Obj, ctx: Obj, i: usize, slot: usize, src: Obj, lo: u32, len: u32, bind: u32) -> Obj {
        while self.dyn_pool.len() <= i {
            self.dyn_pool.push([(0, 0, std::ptr::null_mut()); 5]);
        }
        let e = &mut self.dyn_pool[i][slot];
        // v15: a window of `len` bytes from `lo`; the pooled buffer only grows (64 KB steps: no re-creation every frame)
        let width = len.div_ceil(65536).max(1) * 65536;
        unsafe {
            if e.0 < width || e.1 != bind || e.2.is_null() {
                release(e.2);
                *e = (0, 0, std::ptr::null_mut());
                let create_buffer: CreateBufferFn = std::mem::transmute(vfn(dev, vt::DEV_CREATE_BUFFER));
                let d: [u32; 6] = [width, 0 /* DEFAULT */, bind, 0, 0, 0];
                let mut b: Obj = std::ptr::null_mut();
                if create_buffer(dev, &d, std::ptr::null(), &mut b) < 0 || b.is_null() {
                    return std::ptr::null_mut();
                }
                *e = (width, bind, b);
            }
            let copy: unsafe extern "system" fn(Obj, Obj, u32, u32, u32, u32, Obj, u32, *const [u32; 6]) = std::mem::transmute(vfn(ctx, vt::CTX_COPY_SUBRESOURCE_REGION));
            let bx: [u32; 6] = [lo, 0, 0, lo + len, 1, 1];
            copy(ctx, e.2, 0, 0, 0, 0, src, 0, &bx);
            let add: unsafe extern "system" fn(Obj) -> u32 = std::mem::transmute(vfn(e.2, 1));
            add(e.2);
            e.2
        }
    }

    /// Our copy of a vertex constant buffer `src` (`width` bytes) for record `i`, slot `slot`: created on first use, reused.
    unsafe fn copy_cb(&mut self, dev: Obj, ctx: Obj, i: usize, slot: usize, src: Obj, width: u32) -> Obj {
        while self.pool.len() <= i {
            self.pool.push([(0, std::ptr::null_mut()); 4]);
        }
        let e = &mut self.pool[i][slot];
        unsafe {
            if e.0 != width || e.1.is_null() {
                release(e.1);
                *e = (0, std::ptr::null_mut());
                let create_buffer: CreateBufferFn = std::mem::transmute(vfn(dev, vt::DEV_CREATE_BUFFER));
                let d: [u32; 6] = [width, 0 /* DEFAULT */, 4 /* CONSTANT_BUFFER */, 0, 0, 0];
                let mut b: Obj = std::ptr::null_mut();
                if create_buffer(dev, &d, std::ptr::null(), &mut b) < 0 || b.is_null() {
                    return std::ptr::null_mut();
                }
                *e = (width, b);
            }
            let copy: unsafe extern "system" fn(Obj, Obj, Obj) = std::mem::transmute(vfn(ctx, vt::CTX_COPY_RESOURCE));
            copy(ctx, e.1, src);
            e.1
        }
    }

    unsafe fn ensure_target(&mut self, dev: Obj, w: u32, h: u32) -> bool {
        if !self.target.rtv.is_null() && !self.zdsv.is_null() && !self.skin.rtv.is_null() && self.size == (w, h) {
            return true;
        }
        unsafe {
            self.target.free();
            self.skin.free();
            release(self.zdsv);
            release(self.zbuf);
            (self.zdsv, self.zbuf) = (std::ptr::null_mut(), std::ptr::null_mut());
            self.size = (0, 0);
            let Some(t) = make_target(dev, w, h, SHELL_FORMAT, false) else { return false };
            self.target = t;
            let Some(k) = make_target(dev, w, h, SKIN_FORMAT, false) else {
                self.target.free();
                return false;
            };
            self.skin = k;
            // D32_FLOAT, DEPTH_STENCIL bind.
            let desc = crate::d3d::Tex2dDesc { width: w, height: h, mips: 1, array: 1, format: 40, samples: 1, quality: 0, usage: 0, bind: 0x40, cpu: 0, misc: 0 };
            let create_tex: unsafe extern "system" fn(Obj, *const crate::d3d::Tex2dDesc, *const std::ffi::c_void, *mut Obj) -> i32 = std::mem::transmute(vfn(dev, vt::DEV_CREATE_TEXTURE2D));
            let create_dsv: unsafe extern "system" fn(Obj, Obj, *const [u32; 6], *mut Obj) -> i32 = std::mem::transmute(vfn(dev, DEV_CREATE_DSV));
            if create_tex(dev, &desc, std::ptr::null(), &mut self.zbuf) < 0 || self.zbuf.is_null() || create_dsv(dev, self.zbuf, &[40, 3, 0, 0, 0, 0], &mut self.zdsv) < 0 || self.zdsv.is_null() {
                self.target.free();
                self.skin.free();
                release(self.zbuf);
                (self.zdsv, self.zbuf) = (std::ptr::null_mut(), std::ptr::null_mut());
                return false;
            }
            self.size = (w, h);
            log!("shellfur: shell target {w}x{h} R32_FLOAT + D32 score buffer + R8 skin target (v11) created");
            true
        }
    }

    /// A shader view of the frame's linear depth (cached by resource pointer).
    unsafe fn depth_view(&mut self, dev: Obj, res: Obj) -> Obj {
        if let Some((p, v)) = self.depth_srv {
            if p == res as usize {
                return v;
            }
            unsafe { release(v) };
            self.depth_srv = None;
        }
        unsafe {
            let create_srv: unsafe extern "system" fn(Obj, Obj, *const [u32; 6], *mut Obj) -> i32 = std::mem::transmute(vfn(dev, vt::DEV_CREATE_SRV));
            let mut v: Obj = std::ptr::null_mut();
            if create_srv(dev, res, &[41, 4, 0, 1, 0, 0], &mut v) < 0 || v.is_null() {
                return std::ptr::null_mut();
            }
            self.depth_srv = Some((res as usize, v));
            v
        }
    }

    /// v8: the format of the engine's depth buffer `res` (0 = none): the unit of the depth bias.
    unsafe fn dev_depth_fmt(&self, res: Obj) -> u32 {
        if res.is_null() {
            return 0;
        }
        unsafe {
            let mut d = crate::d3d::Tex2dDesc::default();
            let get_desc: unsafe extern "system" fn(Obj, *mut crate::d3d::Tex2dDesc) = std::mem::transmute(vfn(res, vt::TEX2D_GET_DESC));
            get_desc(res, &mut d);
            d.format
        }
    }

    /// A shader view of the engine's depth buffer `res` (typeless with the SHADER_RESOURCE bind only; null otherwise), cached.
    unsafe fn dev_depth_view(&mut self, dev: Obj, res: Obj) -> Obj {
        if res.is_null() {
            return std::ptr::null_mut();
        }
        if let Some((p, v)) = self.dev_srv {
            if p == res as usize {
                return v;
            }
            unsafe { release(v) };
            self.dev_srv = None;
        }
        let mut v: Obj = std::ptr::null_mut();
        unsafe {
            let mut d = crate::d3d::Tex2dDesc::default();
            let get_desc: unsafe extern "system" fn(Obj, *mut crate::d3d::Tex2dDesc) = std::mem::transmute(vfn(res, vt::TEX2D_GET_DESC));
            get_desc(res, &mut d);
            if let Some(f) = crate::furmask::depth_srv_format(d.format) {
                if d.bind & 0x8 != 0 && d.samples == 1 && d.array == 1 {
                    let create_srv: unsafe extern "system" fn(Obj, Obj, *const [u32; 6], *mut Obj) -> i32 = std::mem::transmute(vfn(dev, vt::DEV_CREATE_SRV));
                    if create_srv(dev, res, &[f, 4, 0, 1, 0, 0], &mut v) < 0 {
                        v = std::ptr::null_mut();
                    }
                }
            }
        }
        self.dev_srv = Some((res as usize, v));
        v
    }

    /// (shell, measurement, uv-space, v9 stream-output capture) geometry shader objects of a vertex layout, created on first use; null
    /// on failure.
    unsafe fn gs_object(&mut self, dev: Obj, key: &str, blobs: &GsBlobs, now: bool) -> (Obj, Obj, Obj, Obj) {
        let nul: Obj = std::ptr::null_mut();
        if let Some((_, g, m, u, t)) = self.gs.iter().find(|(k, _, _, _, _)| k == key) {
            return (*g, *m, *u, *t);
        }
        if self.gs.len() >= MAX_GS || !(now || crate::warm::may_create()) {
            return (nul, nul, nul, nul); // v19: not this frame (no shells for that draw until its turn)
        }
        let t0 = std::time::Instant::now();
        unsafe {
            let create_gs: unsafe extern "system" fn(Obj, *const u8, usize, Obj, *mut Obj) -> i32 = std::mem::transmute(vfn(dev, 13));
            let mut g: Obj = nul;
            if create_gs(dev, blobs.shell.as_ptr(), blobs.shell.len(), nul, &mut g) < 0 || g.is_null() {
                return (nul, nul, nul, nul);
            }
            // v9: ID3D11Device::CreateGeometryShaderWithStreamOutput (slot 14): 5 float4 per triangle (TEXCOORD0..4), one buffer,
            // no rasterized stream.
            let names: [&[u8]; 1] = [b"TEXCOORD\0"];
            let decl: [SoDecl; 5] = std::array::from_fn(|k| SoDecl { stream: 0, name: names[0].as_ptr(), index: k as u32, start: 0, count: 4, slot: 0 });
            let strides = [crate::furisland::SO_STRIDE];
            let create_so: unsafe extern "system" fn(Obj, *const u8, usize, *const SoDecl, u32, *const u32, u32, u32, Obj, *mut Obj) -> i32 = std::mem::transmute(vfn(dev, 14));
            let mut t: Obj = nul;
            if create_so(dev, blobs.tri.as_ptr(), blobs.tri.len(), decl.as_ptr(), 5, strides.as_ptr(), 1, 0xffff_ffff /* NO_RASTERIZED_STREAM */, nul, &mut t) < 0 {
                t = nul;
            }
            let mut m: Obj = nul;
            if create_gs(dev, blobs.measure.as_ptr(), blobs.measure.len(), nul, &mut m) < 0 {
                m = nul;
            }
            let mut u: Obj = nul;
            if create_gs(dev, blobs.uvspace.as_ptr(), blobs.uvspace.len(), nul, &mut u) < 0 {
                u = nul;
            }
            crate::warm::note_render_create("geometry shader set", t0);
            self.gs.push((key.to_string(), g, m, u, t));
            (g, m, u, t)
        }
    }

    /// v7: copies this frame's coat statistics into the next free staging texture (read back by `read_stats` a few frames later).
    unsafe fn stage_stats(&mut self, ctx: Obj) {
        let slot = self.stat_runs as usize % 3;
        if self.stat_stage[slot].is_null() || self.stat_staged[slot] || self.stats.tex.is_null() {
            return;
        }
        unsafe {
            let copy: unsafe extern "system" fn(Obj, Obj, Obj) = std::mem::transmute(vfn(ctx, vt::CTX_COPY_RESOURCE));
            copy(ctx, self.stat_stage[slot], self.stats.tex);
        }
        let n = self.recs.len().min(MAX_RECS);
        for (i, r) in self.recs.iter().take(n).enumerate() {
            self.stat_recs[slot][i] = (r.call.count(), r.tex0 as usize);
        }
        self.stat_n[slot] = n;
        self.stat_staged[slot] = true;
        self.stat_runs = self.stat_runs.wrapping_add(1);
    }

    /// v7: the newest finished statistics copy (never waits) into `STATS_SHOW` for `status_shells=`.
    unsafe fn read_stats(&mut self, ctx: Obj) {
        unsafe {
            let map: unsafe extern "system" fn(Obj, Obj, u32, u32, u32, *mut [usize; 3]) -> i32 = std::mem::transmute(vfn(ctx, vt::CTX_MAP));
            let unmap: unsafe extern "system" fn(Obj, Obj, u32) = std::mem::transmute(vfn(ctx, vt::CTX_UNMAP));
            let next = self.stat_runs as usize % 3;
            for k in 0..3 {
                let j = (next + k) % 3;
                if !self.stat_staged[j] || self.stat_stage[j].is_null() {
                    continue;
                }
                let mut mapped: [usize; 3] = [0; 3];
                if map(ctx, self.stat_stage[j], 0, 1, 0x10_0000, &mut mapped) >= 0 && mapped[0] != 0 {
                    let pitch = mapped[1] & 0xffff_ffff; // D3D11_MAPPED_SUBRESOURCE: pData, then RowPitch and DepthPitch (u32 each)
                    let mut out = Vec::with_capacity(self.stat_n[j]);
                    for i in 0..self.stat_n[j] {
                        let r0 = *((mapped[0] + i * 16) as *const [f32; 4]);
                        let r1 = *((mapped[0] + pitch + i * 16) as *const [f32; 4]);
                        let (count, atlas) = self.stat_recs[j][i];
                        out.push(DrawStats { count, atlas, s0: r0, s1: r1 });
                    }
                    unmap(ctx, self.stat_stage[j], 0);
                    self.stat_staged[j] = false;
                    // v18: the per-atlas proof of the content rule (furselect.rs).
                    let book: Vec<(usize, u32, [f32; 4])> = out.iter().map(|d| (d.atlas, d.count, d.s0)).collect();
                    crate::furselect::note_stats(&book);
                    if let Ok(mut g) = STATS_SHOW.try_lock() {
                        *g = out;
                    }
                } else {
                    break;
                }
            }
        }
    }

    /// Reads the staged unit sums the GPU has finished (never waits), oldest first, into the tracker.
    unsafe fn read_units(&mut self, ctx: Obj) {
        unsafe {
            let map: unsafe extern "system" fn(Obj, Obj, u32, u32, u32, *mut [usize; 3]) -> i32 = std::mem::transmute(vfn(ctx, vt::CTX_MAP));
            let unmap: unsafe extern "system" fn(Obj, Obj, u32) = std::mem::transmute(vfn(ctx, vt::CTX_UNMAP));
            let next = self.runs as usize % 3; // the slot written next = the oldest one
            for k in 0..3 {
                let j = (next + k) % 3;
                if !self.staged[j] || self.stage[j].is_null() {
                    continue;
                }
                let mut mapped: [usize; 3] = [0; 3];
                // READ, DO_NOT_WAIT: fails with "still drawing" instead of stalling.
                if map(ctx, self.stage[j], 0, 1, 0x10_0000, &mut mapped) >= 0 && mapped[0] != 0 {
                    let v = *(mapped[0] as *const [f32; 4]);
                    unmap(ctx, self.stage[j], 0);
                    self.staged[j] = false;
                    self.unit.feed(v);
                } else {
                    break; // a newer copy cannot be done before an older one
                }
            }
        }
    }
}

impl Drop for Shell {
    fn drop(&mut self) {
        self.clear_recs();
        unsafe {
            self.target.free();
            for p in &self.pool {
                for (_, b) in p {
                    release(*b);
                }
            }
            for p in &self.dyn_pool {
                for (_, _, b) in p {
                    release(*b);
                }
            }
            for (_, g, m, u, t) in &self.gs {
                release(*g);
                release(*m);
                release(*u);
                release(*t);
            }
            self.meas.free();
            self.skin.free();
            self.skin_none.free();
            release(self.ps_skin);
            self.feat.free();
            self.stats.free();
            for st in self.stat_stage {
                release(st);
            }
            for o in [self.ps_feat, self.vs_stats, self.ps_stats] {
                release(o);
            }
            for st in self.stage {
                release(st);
            }
            release(self.zdsv);
            release(self.zbuf);
            if let Some((_, v)) = self.depth_srv {
                release(v);
            }
            if let Some((_, v)) = self.dev_srv {
                release(v);
            }
            for o in [self.ps, self.blend, self.dss, self.rs_fill, self.cb_gs, self.cb_ps, self.cb_comp, self.ps_meas, self.blend_add, self.rs_meas] {
                release(o);
            }
        }
    }
}

unsafe fn device_of(ctx: Obj) -> Obj {
    let mut dev: Obj = std::ptr::null_mut();
    unsafe {
        let get_device: unsafe extern "system" fn(Obj, *mut Obj) = std::mem::transmute(vfn(ctx, vt::CHILD_GET_DEVICE));
        get_device(ctx, &mut dev);
    }
    dev
}

/// Number of shader-resource views bound to the pixel shader (slots 0..7), for the per-draw list.
unsafe fn ps_srv_count(ctx: Obj) -> u32 {
    let mut v: [Obj; 8] = [std::ptr::null_mut(); 8];
    unsafe {
        let get: unsafe extern "system" fn(Obj, u32, u32, *mut Obj) = std::mem::transmute(vfn(ctx, vt::CTX_PS_GET_SRVS));
        get(ctx, 0, 8, v.as_mut_ptr());
        let n = v.iter().filter(|p| !p.is_null()).count() as u32;
        for p in v {
            release(p);
        }
        n
    }
}

/// A fur draw went into the fur mask (`furmask::fur_draw`, before the engine's own draw, the draw's state bound): lists it for the
/// status and records it when the shell selector picks it. One atomic load when the shells are off; never unwinds.
///
/// # Safety
/// `ctx` must be the game's live immediate context with the draw's state bound.
/// `auto`: admitted by the content rule (furselect.rs), not by `fur_ps`: measured, and drawn only once its atlas is proven fur.
pub unsafe fn on_fur_draw(ctx: Obj, call: Call, ps: u32, vs: Option<u32>, vcb0: u32, vs_ptr: usize, auto: bool) -> bool {
    if !ON.load(Relaxed) || !!crate::toggles::master_off() || FAILED.load(Relaxed) {
        return false;
    }
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        let _ig = IgnoreScope::new();
        let verdict = SEL.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map_or(Err("no selector"), |s| s.verdict(ps, vs, vcb0, call.count()));
        let mut info = DrawInfo { vs, ps, vcb0, count: call.count(), instances: call.instances(), srvs: ps_srv_count(ctx), verdict: verdict.err(), shells: false };
        if verdict.is_ok() {
            match record(ctx, call, vs_ptr, auto) {
                Ok(()) => {
                    info.shells = true;
                    dump_vs(vs_ptr, vs);
                }
                Err(why) => info.verdict = Some(why),
            }
        }
        let shells = info.shells;
        let reason = if shells { "shells" } else { info.verdict.unwrap_or("skipped") };
        with_cur(|f| {
            match f.reasons.iter_mut().find(|r| r.0 == reason) {
                Some(r) => r.1 += 1,
                None => f.reasons.push((reason, 1)),
            }
            if f.draws.len() < MAX_INFO {
                f.draws.push(info);
            } else {
                f.dropped += 1;
            }
        });
        shells
    }))
    .unwrap_or(false)
}

/// Vertex shaders already dumped (by hash).
static DUMPED: Mutex<Vec<u32>> = Mutex::new(Vec::new());

/// Debug aid: the first time a vertex shader gets shells, its output signature and disassembly go to `GW2Graphics\debug\vs-<hash>.txt` (a
/// background thread; D3DDisassemble is never run on the render thread). That is where the normal / tangent outputs (`shell_normal`)
/// and the position math can be read. At most 32 files per session.
fn dump_vs(vs_ptr: usize, hash: Option<u32>) {
    let Some(h) = hash else { return };
    // Only in debug mode (periodic captures or hot reload) or with the shell debug view: no files otherwise.
    if crate::debugshot::capture_seconds() == 0 && !crate::debugshot::hot_reload() && cfg().debug == 0 {
        return;
    }
    let Some(dir) = crate::grading::dir() else { return };
    {
        let mut d = DUMPED.lock().unwrap_or_else(|e| e.into_inner());
        if d.contains(&h) || d.len() >= 32 {
            return;
        }
        d.push(h);
    }
    let Some(code) = crate::furmask::vs_code(vs_ptr) else { return };
    let sig = crate::furmask::vs_sig(vs_ptr).map(|s| s.text()).unwrap_or_default();
    let _ = std::thread::Builder::new().name("gw2g-shell-vsdump".into()).spawn(move || {
        let dis = shaders::disassemble(&code).unwrap_or_else(|e| format!("// {e}"));
        let dir = dir.join("debug");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("vs-{h:08x}.txt"));
        let text = format!("// vertex shader {h:08x} ({} bytes), outputs: {sig}\n// written by the shell fur (docs/shell-fur.md) for shell_normal / the position math\n{dis}\n", code.len());
        match std::fs::write(&path, text) {
            Ok(()) => log!("shellfur: wrote {}", path.display()),
            Err(e) => log!("shellfur: could not write {}: {e}", path.display()),
        }
    });
}

unsafe fn record(ctx: Obj, call: Call, vs_ptr: usize, auto: bool) -> Result<(), &'static str> {
    let Some(Ok(blobs)) = PS_BLOB.get() else { return Err("strand shader not compiled yet") };
    unsafe {
        let dev = device_of(ctx);
        if dev.is_null() {
            return Err("no device");
        }
        let r = 'run: {
            let Ok(mut g) = STATE.try_lock() else { break 'run Err("busy") };
            if g.as_ref().is_some_and(|s| s.device != dev as usize) {
                *g = None;
            }
            if g.is_none() {
                *g = Shell::new(dev, blobs);
                if g.is_none() {
                    FAILED.store(true, Relaxed);
                    log!("shellfur: could not create the shell objects; the shells stay off");
                    break 'run Err("could not create the shell objects");
                }
            }
            let Some(s) = g.as_mut() else { break 'run Err("no state") };
            let tris_now: u32 = s.recs.iter().map(|r| (r.call.count() / 3).saturating_mul(r.call.instances().max(1))).sum();
            let tris_new = (call.count() / 3).saturating_mul(call.instances().max(1));
            // v16: over the budget, decided below once the draw's identity is known (a draw shelled in the last frame keeps its shells)
            let over = admit(s.recs.len(), tris_now, tris_new).err();
            let nul: Obj = std::ptr::null_mut();
            let mut rec = Rec {
                call,
                vs_ptr,
                vs: nul,
                layout: nul,
                topology: 0,
                vb: [nul; 4],
                strides: [0; 4],
                offsets: [0; 4],
                ib: nul,
                ib_fmt: 0,
                ib_off: 0,
                vcb: [nul; 4],
                vsrv: [nul; 4],
                vsmp: [nul; 2],
                rs: nul,
                vp: Viewport::default(),
                scissor: [0; 4],
                ccw: false,
                depth: nul,
                tex0: nul,
                rsd: [0; 10],
                copied: false,
                id: 0,
                auto,
            };
            type GetShader = unsafe extern "system" fn(Obj, *mut Obj, *mut Obj, *mut u32);
            let get_vs: GetShader = std::mem::transmute(vfn(ctx, vt::CTX_VS_GET_SHADER));
            get_vs(ctx, &mut rec.vs, std::ptr::null_mut(), std::ptr::null_mut());
            let get_layout: unsafe extern "system" fn(Obj, *mut Obj) = std::mem::transmute(vfn(ctx, vt::CTX_IA_GET_LAYOUT));
            get_layout(ctx, &mut rec.layout);
            let get_topo: unsafe extern "system" fn(Obj, *mut u32) = std::mem::transmute(vfn(ctx, vt::CTX_IA_GET_TOPOLOGY));
            get_topo(ctx, &mut rec.topology);
            let get_vbs: unsafe extern "system" fn(Obj, u32, u32, *mut Obj, *mut u32, *mut u32) = std::mem::transmute(vfn(ctx, vt::CTX_IA_GET_VBS));
            get_vbs(ctx, 0, 4, rec.vb.as_mut_ptr(), rec.strides.as_mut_ptr(), rec.offsets.as_mut_ptr());
            let get_ib: unsafe extern "system" fn(Obj, *mut Obj, *mut u32, *mut u32) = std::mem::transmute(vfn(ctx, CTX_IA_GET_IB));
            get_ib(ctx, &mut rec.ib, &mut rec.ib_fmt, &mut rec.ib_off);
            let get_vsrv: unsafe extern "system" fn(Obj, u32, u32, *mut Obj) = std::mem::transmute(vfn(ctx, CTX_VS_GET_SRVS));
            get_vsrv(ctx, 0, 4, rec.vsrv.as_mut_ptr());
            let get_vsmp: unsafe extern "system" fn(Obj, u32, u32, *mut Obj) = std::mem::transmute(vfn(ctx, CTX_VS_GET_SAMPLERS));
            get_vsmp(ctx, 0, 2, rec.vsmp.as_mut_ptr());
            let get_psrv: unsafe extern "system" fn(Obj, u32, u32, *mut Obj) = std::mem::transmute(vfn(ctx, vt::CTX_PS_GET_SRVS));
            get_psrv(ctx, 0, 1, &mut rec.tex0);
            let get_rs: unsafe extern "system" fn(Obj, *mut Obj) = std::mem::transmute(vfn(ctx, vt::CTX_RS_GET_STATE));
            get_rs(ctx, &mut rec.rs);
            let mut n_vp = 1u32;
            let get_vp: unsafe extern "system" fn(Obj, *mut u32, *mut Viewport) = std::mem::transmute(vfn(ctx, vt::CTX_RS_GET_VIEWPORTS));
            get_vp(ctx, &mut n_vp, &mut rec.vp);
            let mut n_sc = 1u32;
            let get_sc: unsafe extern "system" fn(Obj, *mut u32, *mut [i32; 4]) = std::mem::transmute(vfn(ctx, vt::CTX_RS_GET_SCISSORS));
            get_sc(ctx, &mut n_sc, &mut rec.scissor);
            if n_sc == 0 {
                rec.scissor = [rec.vp.x as i32, rec.vp.y as i32, (rec.vp.x + rec.vp.w) as i32, (rec.vp.y + rec.vp.h) as i32];
            }
            if !rec.rs.is_null() {
                let mut rd = [0u32; 10];
                let get: unsafe extern "system" fn(Obj, *mut [u32; 10]) = std::mem::transmute(vfn(rec.rs, crate::furmask::RS_GET_DESC));
                get(rec.rs, &mut rd);
                rec.ccw = rd[2] != 0;
                rec.rsd = rd;
            }
            // The depth buffer (a counted reference to its texture).
            let mut rtvs: [Obj; 8] = [nul; 8];
            let mut dsv: Obj = nul;
            let get_rts: unsafe extern "system" fn(Obj, u32, *mut Obj, *mut Obj) = std::mem::transmute(vfn(ctx, vt::CTX_OM_GET_RTS));
            get_rts(ctx, 8, rtvs.as_mut_ptr(), &mut dsv);
            for r in rtvs {
                release(r);
            }
            if !dsv.is_null() {
                let get_res: unsafe extern "system" fn(Obj, *mut Obj) = std::mem::transmute(vfn(dsv, vt::VIEW_GET_RESOURCE));
                get_res(dsv, &mut rec.depth);
                release(dsv);
            }
            if call.indexed() && rec.ib.is_null() {
                rec.release();
                break 'run Err("indexed draw without an index buffer");
            }
            // v7 (the transparency check): the replay draws these buffers again at the end of the frame. A DYNAMIC buffer can be
            // rewritten (Map DISCARD) after this draw: the replay would draw another mesh's data as strands. v14: such a buffer is
            // COPIED now (whole, into a pooled DEFAULT buffer of this record) when it is small enough, so the replay draws exactly
            // this draw's data; too big: no shells (fail open). The island analysis does not run on a copy (its pooled buffer holds
            // another mesh each frame): those records keep the texel rule alone.
            rec.id = crate::furisland::mesh_key(&[rec.tex0 as u64, vs_ptr as u64, call.count() as u64, call.instances() as u64, rec.topology as u64]);
            let mut evict: Option<usize> = None;
            if let Some(why) = over {
                match s.victim(rec.id, tris_new) {
                    Some(v) => evict = Some(v),
                    None => {
                        rec.release();
                        break 'run Err(why);
                    }
                }
            }
            let idx = evict.unwrap_or(s.recs.len());
            let any_dyn = rec.vb.iter().copied().chain(std::iter::once(rec.ib)).any(|b| buffer_dynamic(b));
            if any_dyn {
                // v15: only the RANGE this draw reads (v14 copied the whole buffer and refused every shared ring buffer as "too
                // big": the rigid NPC bodies): the index range [start, start + count) of a dynamic index buffer, and per vertex
                // stream the vertex window [base, base + count) (a draw's vertices number at most its indices: every vertex is
                // referenced; an index outside reads zeros = a degenerate triangle the root test drops). The call is rebased onto
                // the copies (base 0; start 0 when the index buffer was copied).
                let (start, base) = call.start_base();
                let count = call.count();
                let windows = dyn_windows(call, &rec.strides, &rec.offsets, rec.ib_fmt, rec.ib_off, buffer_dynamic(rec.ib));
                let Some(mut w) = windows else {
                    rec.release();
                    break 'run Err("dynamic vertex / index buffer of an instanced draw (could be rewritten before the replay)");
                };
                let mut bytes = 0u64;
                let mut fits = true;
                for (k, win) in w.iter_mut().enumerate() {
                    if k < 4 && rec.vb[k].is_null() {
                        *win = (0, 0);
                    }
                    if win.1 == 0 {
                        continue;
                    }
                    let src = if k < 4 { rec.vb[k] } else { rec.ib };
                    let (width, _) = buffer_width_bind(src);
                    if k < 4 {
                        // a vertex window past the buffer's end holds no vertex the draw can reference
                        win.1 = win.1.min(width.saturating_sub(win.0));
                    }
                    fits &= win.1 > 0 && win.1 <= DYN_MAX_BYTES && win.0 as u64 + win.1 as u64 <= width as u64;
                    bytes += win.1 as u64;
                }
                if !fits || s.dyn_bytes_frame + bytes > DYN_FRAME_BYTES {
                    rec.release();
                    break 'run Err("dynamic vertex / index buffer range too big to copy (could be rewritten before the replay)");
                }
                let mut ok = true;
                for (k, &(lo, len)) in w.iter().enumerate() {
                    if len == 0 {
                        continue;
                    }
                    let src = if k < 4 { rec.vb[k] } else { rec.ib };
                    let bind = if k < 4 { 1 } else { 2 };
                    let c = s.copy_dyn(dev, ctx, idx, k, src, lo, len, bind);
                    if c.is_null() {
                        ok = false;
                        break;
                    }
                    release(src);
                    if k < 4 {
                        rec.vb[k] = c;
                        rec.offsets[k] = 0;
                    } else {
                        rec.ib = c;
                        rec.ib_off = 0;
                    }
                }
                if !ok {
                    rec.release();
                    break 'run Err("could not copy a dynamic vertex / index buffer");
                }
                let ib_copied = w[4].1 > 0;
                rec.call = if call.indexed() { Call::Indexed(count, if ib_copied { 0 } else { start }, 0) } else { Call::Draw(count, 0) };
                let _ = base;
                rec.copied = true;
                s.dyn_bytes_frame += bytes;
                s.dyn_copied_frame += 1;
            }
            if n_vp == 0 || rec.vp.w < 1.0 || rec.vp.h < 1.0 {
                rec.release();
                break 'run Err("no viewport");
            }
            // The vertex constant buffers: a GPU copy of this draw's contents (the engine rewrites them for the next draw).
            let mut cbs: [Obj; 4] = [nul; 4];
            let get_cb: unsafe extern "system" fn(Obj, u32, u32, *mut Obj) = std::mem::transmute(vfn(ctx, vt::CTX_VS_GET_CBS));
            get_cb(ctx, 0, 4, cbs.as_mut_ptr());
            let mut ok = true;
            for (slot, cb) in cbs.iter().enumerate() {
                if cb.is_null() {
                    continue;
                }
                let w = crate::drawdiag::buffer_width(*cb);
                let copy = if w > 0 && w % 16 == 0 { s.copy_cb(dev, ctx, idx, slot, *cb, w) } else { nul };
                if copy.is_null() {
                    ok = false;
                }
                rec.vcb[slot] = copy;
                release(*cb);
            }
            if !ok {
                rec.release();
                break 'run Err("could not copy the vertex constant buffers");
            }
            match evict {
                Some(v) => {
                    s.recs[v].release();
                    s.recs[v] = rec;
                    s.evicted_frame += 1;
                }
                None => s.recs.push(rec),
            }
            Ok(())
        };
        release(dev);
        r
    }
}

/// v15: the byte windows (offset, length) a draw reads from its vertex streams 0..3 and (slot 4) its index buffer when one of them is
/// DYNAMIC: per stream with a stride, [offset + (base + first) x stride, + n x stride) where n = the call's count (indexed: the vertex
/// window from the base vertex, at most `count` vertices; not indexed: [start, start + count)); the index buffer's window
/// [ib_off + start x size, + count x size) only when `ib_dynamic`. None for an instanced draw (its per-instance streams are not
/// bounded by the count).
pub fn dyn_windows(call: Call, strides: &[u32; 4], offsets: &[u32; 4], ib_fmt: u32, ib_off: u32, ib_dynamic: bool) -> Option<[(u32, u32); 5]> {
    let (n, first, base) = match call {
        Call::Draw(n, s) => (n as u64, s as i64, 0i64),
        Call::Indexed(n, _, b) => (n as u64, 0, b as i64),
        _ => return None,
    };
    let mut w = [(0u32, 0u32); 5];
    for k in 0..4 {
        if strides[k] == 0 {
            continue;
        }
        let lo = offsets[k] as i64 + (base + first) * strides[k] as i64;
        let len = n * strides[k] as u64;
        if lo < 0 || lo > u32::MAX as i64 || len > u32::MAX as u64 {
            return None;
        }
        w[k] = (lo as u32, len as u32);
    }
    if let Call::Indexed(n, start, _) = call {
        if ib_dynamic {
            let size: u64 = if ib_fmt == 57 /* R16_UINT */ { 2 } else { 4 };
            let lo = ib_off as u64 + start as u64 * size;
            let len = n as u64 * size;
            if lo > u32::MAX as u64 || len > u32::MAX as u64 {
                return None;
            }
            w[4] = (lo as u32, len as u32);
        }
    }
    Some(w)
}

/// v16: the pure rule of `Shell::victim`: `recs` = (identity, triangles) of this frame's records so far.
pub fn victim_of(prev: &std::collections::HashSet<u64>, recs: &[(u64, u32)], id: u64, tris: u32) -> Option<usize> {
    if !prev.contains(&id) {
        return None;
    }
    let total: u32 = recs.iter().map(|r| r.1).sum();
    (0..recs.len()).rev().find(|&k| !prev.contains(&recs[k].0) && total - recs[k].1 + tris <= MAX_FRAME_TRIS)
}

/// v14: whether one more record fits the frame: `n` records holding `tris_now` triangles, the new draw `tris_new` (MAX_RECS records,
/// MAX_FRAME_TRIS triangles).
pub fn admit(n: usize, tris_now: u32, tris_new: u32) -> Result<(), &'static str> {
    if n >= MAX_RECS {
        return Err("too many shell draws this frame (cap)");
    }
    if tris_now.saturating_add(tris_new) > MAX_FRAME_TRIS {
        return Err("shell triangle budget of the frame");
    }
    Ok(())
}

/// v14: (byte width, bind flags) of a buffer.
unsafe fn buffer_width_bind(buf: Obj) -> (u32, u32) {
    if buf.is_null() {
        return (0, 0);
    }
    let mut d = [0u32; 6];
    unsafe {
        let get_desc: unsafe extern "system" fn(Obj, *mut [u32; 6]) = std::mem::transmute(vfn(buf, vt::BUFFER_GET_DESC));
        get_desc(buf, &mut d);
    }
    (d[0], d[2])
}

/// The buffer is D3D11_USAGE_DYNAMIC (null: false).
pub(crate) unsafe fn buffer_dynamic(buf: Obj) -> bool {
    if buf.is_null() {
        return false;
    }
    let mut d = [0u32; 6]; // D3D11_BUFFER_DESC: width, usage, bind, cpu, misc, stride
    unsafe {
        let get_desc: unsafe extern "system" fn(Obj, *mut [u32; 6]) = std::mem::transmute(vfn(buf, vt::BUFFER_GET_DESC));
        get_desc(buf, &mut d);
    }
    d[1] == 2
}

/// D3D11_SO_DECLARATION_ENTRY.
#[repr(C)]
struct SoDecl {
    stream: u32,
    name: *const u8,
    index: u32,
    start: u8,
    count: u8,
    slot: u8,
}

/// v9: `SOSetTargets` / `SOGetTargets`.
const CTX_SO_SET_TARGETS: usize = 37;
const CTX_SO_GET_TARGETS: usize = 93;

/// `IAGetIndexBuffer`, `VSGetShaderResources`, `VSGetSamplers`, `GSGetConstantBuffers`, `GSGetShaderResources` and the setters.
const CTX_IA_GET_IB: usize = 80;
/// `ID3D11Device::CreateDepthStencilView`, `ID3D11DeviceContext::ClearDepthStencilView`.
const DEV_CREATE_DSV: usize = 10;
const CTX_CLEAR_DSV: usize = 53;
const CTX_VS_GET_SRVS: usize = 84;
const CTX_VS_GET_SAMPLERS: usize = 85;
const CTX_GS_GET_CBS: usize = 81;
const CTX_GS_GET_SRVS: usize = 87;
const CTX_VS_SET_CBS: usize = 7;
const CTX_IA_SET_VBS: usize = 18;
const CTX_IA_SET_IB: usize = 19;
const CTX_GS_SET_CBS: usize = 22;
const CTX_VS_SET_SRVS: usize = 25;
const CTX_VS_SET_SAMPLERS: usize = 26;
const CTX_GS_SET_SRVS: usize = 31;

/// The pipeline state the replay touches beyond `StateGuard`: vertex / index buffers, the vertex shader's constant buffers, views and
/// samplers, the geometry shader's b0 / t0. Restored (and every reference released) on drop.
struct ExtraGuard {
    ctx: Obj,
    vb: [Obj; 4],
    strides: [u32; 4],
    offsets: [u32; 4],
    ib: Obj,
    ib_fmt: u32,
    ib_off: u32,
    vcb: [Obj; 4],
    vsrv: [Obj; 4],
    vsmp: [Obj; 2],
    gcb: Obj,
    gsrv: [Obj; 3],
    /// v9: the stream-output target 0 (the island capture binds its own).
    so: Obj,
}

impl ExtraGuard {
    unsafe fn save(ctx: Obj) -> ExtraGuard {
        let nul: Obj = std::ptr::null_mut();
        let mut g = ExtraGuard { ctx, vb: [nul; 4], strides: [0; 4], offsets: [0; 4], ib: nul, ib_fmt: 0, ib_off: 0, vcb: [nul; 4], vsrv: [nul; 4], vsmp: [nul; 2], gcb: nul, gsrv: [nul; 3], so: nul };
        unsafe {
            let get_vbs: unsafe extern "system" fn(Obj, u32, u32, *mut Obj, *mut u32, *mut u32) = std::mem::transmute(vfn(ctx, vt::CTX_IA_GET_VBS));
            get_vbs(ctx, 0, 4, g.vb.as_mut_ptr(), g.strides.as_mut_ptr(), g.offsets.as_mut_ptr());
            let get_ib: unsafe extern "system" fn(Obj, *mut Obj, *mut u32, *mut u32) = std::mem::transmute(vfn(ctx, CTX_IA_GET_IB));
            get_ib(ctx, &mut g.ib, &mut g.ib_fmt, &mut g.ib_off);
            type GetSlots = unsafe extern "system" fn(Obj, u32, u32, *mut Obj);
            let f: GetSlots = std::mem::transmute(vfn(ctx, vt::CTX_VS_GET_CBS));
            f(ctx, 0, 4, g.vcb.as_mut_ptr());
            let f: GetSlots = std::mem::transmute(vfn(ctx, CTX_VS_GET_SRVS));
            f(ctx, 0, 4, g.vsrv.as_mut_ptr());
            let f: GetSlots = std::mem::transmute(vfn(ctx, CTX_VS_GET_SAMPLERS));
            f(ctx, 0, 2, g.vsmp.as_mut_ptr());
            let f: GetSlots = std::mem::transmute(vfn(ctx, CTX_GS_GET_CBS));
            f(ctx, 0, 1, &mut g.gcb);
            let f: GetSlots = std::mem::transmute(vfn(ctx, CTX_GS_GET_SRVS));
            f(ctx, 0, 3, g.gsrv.as_mut_ptr());
            let so_get: unsafe extern "system" fn(Obj, u32, *mut Obj) = std::mem::transmute(vfn(ctx, CTX_SO_GET_TARGETS));
            so_get(ctx, 1, &mut g.so);
        }
        g
    }
}

impl Drop for ExtraGuard {
    fn drop(&mut self) {
        let ctx = self.ctx;
        unsafe {
            let set_vbs: unsafe extern "system" fn(Obj, u32, u32, *const Obj, *const u32, *const u32) = std::mem::transmute(vfn(ctx, CTX_IA_SET_VBS));
            set_vbs(ctx, 0, 4, self.vb.as_ptr(), self.strides.as_ptr(), self.offsets.as_ptr());
            let set_ib: unsafe extern "system" fn(Obj, Obj, u32, u32) = std::mem::transmute(vfn(ctx, CTX_IA_SET_IB));
            set_ib(ctx, self.ib, self.ib_fmt, self.ib_off);
            type SetSlots = unsafe extern "system" fn(Obj, u32, u32, *const Obj);
            let f: SetSlots = std::mem::transmute(vfn(ctx, CTX_VS_SET_CBS));
            f(ctx, 0, 4, self.vcb.as_ptr());
            let f: SetSlots = std::mem::transmute(vfn(ctx, CTX_VS_SET_SRVS));
            f(ctx, 0, 4, self.vsrv.as_ptr());
            let f: SetSlots = std::mem::transmute(vfn(ctx, CTX_VS_SET_SAMPLERS));
            f(ctx, 0, 2, self.vsmp.as_ptr());
            let f: SetSlots = std::mem::transmute(vfn(ctx, CTX_GS_SET_CBS));
            f(ctx, 0, 1, &self.gcb);
            let f: SetSlots = std::mem::transmute(vfn(ctx, CTX_GS_SET_SRVS));
            f(ctx, 0, 3, self.gsrv.as_ptr());
            // SOSetTargets: the saved target, appending where it was (offset -1).
            let so_set: unsafe extern "system" fn(Obj, u32, *const Obj, *const u32) = std::mem::transmute(vfn(ctx, CTX_SO_SET_TARGETS));
            so_set(ctx, 1, &self.so, &u32::MAX);
            for o in self.vb.into_iter().chain(self.vcb).chain(self.vsrv).chain(self.vsmp).chain(self.gsrv).chain([self.ib, self.gcb, self.so]) {
                release(o);
            }
        }
    }
}

pub(crate) unsafe fn write_cb<T: Copy>(ctx: Obj, cb: Obj, v: &T) -> bool {
    unsafe {
        let mut mapped: [usize; 3] = [0; 3];
        let map: unsafe extern "system" fn(Obj, Obj, u32, u32, u32, *mut [usize; 3]) -> i32 = std::mem::transmute(vfn(ctx, vt::CTX_MAP));
        if cb.is_null() || map(ctx, cb, 0, 4 /* WRITE_DISCARD */, 0, &mut mapped) < 0 || mapped[0] == 0 {
            return false;
        }
        *(mapped[0] as *mut T) = *v;
        let unmap: unsafe extern "system" fn(Obj, Obj, u32) = std::mem::transmute(vfn(ctx, vt::CTX_UNMAP));
        unmap(ctx, cb, 0);
        true
    }
}

/// The light-adjust draw is about to run (`datapath::bind_current`, after the fur mask's occlusion check): replays the recorded draws
/// into the shell target and binds it as `t24` with `b13`; when nothing was recorded (or anything fails) binds nothing (the composite is
/// skipped: bit-identical). Never unwinds; the engine's pipeline state is restored.
pub fn on_light_adjust(ctx: Obj) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe { light_adjust_inner(ctx, crate::ao::depth_resource(), crate::aerial::unit()) }));
}

/// `on_light_adjust` with the linear depth and the depth unit given (the tests pass their own).
///
/// # Safety
/// `ctx` must be a live immediate context; `depth` the frame's linear depth texture (R32_FLOAT) of its device, or `None`.
pub(crate) unsafe fn light_adjust_inner(ctx: Obj, depth: Option<Obj>, depth_unit: f32) {
    unsafe {
        let set_srv: unsafe extern "system" fn(Obj, u32, u32, *const Obj) = std::mem::transmute(vfn(ctx, vt::CTX_PS_SET_SRVS));
        let set_cb: unsafe extern "system" fn(Obj, u32, u32, *const Obj) = std::mem::transmute(vfn(ctx, vt::CTX_PS_SET_CBS));
        let nul: Obj = std::ptr::null_mut();
        let mut srv: Obj = nul;
        let mut cb: Obj = nul;
        // v11: t25, the gate of the old fur look and the rim (the skin target, or the 1x1 zero texture: no proven fur this frame).
        let mut skin: Obj = nul;
        let mut skin_kind = SKIN_UNBOUND;
        let mut skin_n = 0u32;
        let t0 = std::time::Instant::now();
        let c = cfg();
        'run: {
            let Ok(mut g) = STATE.try_lock() else { break 'run };
            let Some(s) = g.as_mut() else { break 'run };
            let ready = ON.load(Relaxed) && !crate::toggles::master_off() && crate::gate::user_enabled() && !crate::furmask::highlighting();
            if ready && !s.skin_none.srv.is_null() {
                skin = s.skin_none.srv;
                skin_kind = SKIN_NONE;
            }
            if s.recs.is_empty() {
                // A second light-adjust draw of the same frame: the shells drawn by the first one, bound again (the same picture
                // in every pass of the frame, never "shells in one, none in the other").
                if s.drawn_frame && ready && write_cb(ctx, s.cb_comp, &composite_cb_values(true, &c)) {
                    srv = s.target.srv;
                    cb = s.cb_comp;
                    if !s.skin.srv.is_null() {
                        skin = s.skin.srv;
                        skin_kind = SKIN_TARGET;
                        skin_n = s.skin_n;
                    }
                }
                break 'run;
            }
            if !ready {
                s.clear_recs();
                break 'run;
            }
            let dev = device_of(ctx);
            if dev.is_null() {
                s.clear_recs();
                break 'run;
            }
            let drawn = 'draw: {
                if s.device != dev as usize {
                    break 'draw Err("the records belong to another device");
                }
                let Some(depth) = depth else { break 'draw Err("waiting for the linear depth (none this frame)") };
                let dsrv = s.depth_view(dev, depth);
                if dsrv.is_null() {
                    break 'draw Err("could not view the linear depth");
                }
                let vp = s.recs[0].vp;
                if !s.ensure_target(dev, vp.w as u32, vp.h as u32) {
                    break 'draw Err("could not create the shell target");
                }
                Ok(replay(s, dev, ctx, dsrv, &c, depth_unit))
            };
            release(dev);
            if drawn.is_ok() && s.skin_n > 0 && !s.skin.srv.is_null() {
                skin = s.skin.srv;
                skin_kind = SKIN_TARGET;
                skin_n = s.skin_n;
            }
            match drawn {
                Ok(n) if n > 0 => {
                    if write_cb(ctx, s.cb_comp, &composite_cb_values(true, &c)) {
                        srv = s.target.srv;
                        cb = s.cb_comp;
                        s.drawn_frame = true;
                    }
                }
                Ok(_) => {}
                Err(why) => {
                    with_cur(|f| f.why = why);
                    if why != "waiting for the linear depth (none this frame)" {
                        fail_once(why);
                    }
                }
            }
            s.clear_recs();
        }
        set_srv(ctx, SRV_SLOT, 1, &srv);
        set_cb(ctx, CB_SLOT, 1, &cb);
        set_srv(ctx, SKIN_SRV_SLOT, 1, &skin);
        with_cur(|f| {
            if skin_kind != SKIN_UNBOUND || f.skin_bound == SKIN_UNBOUND {
                (f.skin, f.skin_bound) = (skin_n, skin_kind);
            }
        });
        let us = t0.elapsed().as_micros().min(u32::MAX as u128) as u32;
        with_cur(|f| f.micros = us);
    }
}

/// The pixel-shader views 2..4 the replay binds (the atlas, the coat statistics, the features): saved and restored around it.
struct PsViewGuard {
    ctx: Obj,
    v: [Obj; 3],
}

impl PsViewGuard {
    unsafe fn save(ctx: Obj) -> PsViewGuard {
        let mut g = PsViewGuard { ctx, v: [std::ptr::null_mut(); 3] };
        unsafe {
            let f: unsafe extern "system" fn(Obj, u32, u32, *mut Obj) = std::mem::transmute(vfn(ctx, vt::CTX_PS_GET_SRVS));
            f(ctx, 2, 3, g.v.as_mut_ptr());
        }
        g
    }
}

impl Drop for PsViewGuard {
    fn drop(&mut self) {
        unsafe {
            let f: unsafe extern "system" fn(Obj, u32, u32, *const Obj) = std::mem::transmute(vfn(self.ctx, vt::CTX_PS_SET_SRVS));
            f(self.ctx, 2, 3, self.v.as_ptr());
            for o in self.v {
                release(o);
            }
        }
    }
}

/// Draws every record into the shell target; returns how many drew.
unsafe fn replay(s: &mut Shell, dev: Obj, ctx: Obj, dsrv: Obj, c: &ShellCfg, depth_unit: f32) -> u32 {
    unsafe {
        let g = StateGuard::save(ctx);
        let x = ExtraGuard::save(ctx);
        let pv = PsViewGuard::save(ctx);
        let nul: Obj = std::ptr::null_mut();
        type SetSlots = unsafe extern "system" fn(Obj, u32, u32, *const Obj);
        type SetShader = unsafe extern "system" fn(Obj, Obj, *const Obj, u32);
        let set_ps_srv: SetSlots = std::mem::transmute(vfn(ctx, vt::CTX_PS_SET_SRVS));
        let set_ps_cb: SetSlots = std::mem::transmute(vfn(ctx, vt::CTX_PS_SET_CBS));
        let set_vs_cb: SetSlots = std::mem::transmute(vfn(ctx, CTX_VS_SET_CBS));
        let set_vs_srv: SetSlots = std::mem::transmute(vfn(ctx, CTX_VS_SET_SRVS));
        let set_vs_smp: SetSlots = std::mem::transmute(vfn(ctx, CTX_VS_SET_SAMPLERS));
        let set_gs_cb: SetSlots = std::mem::transmute(vfn(ctx, CTX_GS_SET_CBS));
        let set_gs_srv: SetSlots = std::mem::transmute(vfn(ctx, CTX_GS_SET_SRVS));
        let set_vs: SetShader = std::mem::transmute(vfn(ctx, vt::CTX_VS_SET_SHADER));
        let set_gs: SetShader = std::mem::transmute(vfn(ctx, vt::CTX_GS_SET_SHADER));
        let set_ps: SetShader = std::mem::transmute(vfn(ctx, vt::CTX_PS_SET_SHADER));
        let set_layout: unsafe extern "system" fn(Obj, Obj) = std::mem::transmute(vfn(ctx, vt::CTX_IA_SET_LAYOUT));
        let set_topo: unsafe extern "system" fn(Obj, u32) = std::mem::transmute(vfn(ctx, vt::CTX_IA_SET_TOPOLOGY));
        let set_vbs: unsafe extern "system" fn(Obj, u32, u32, *const Obj, *const u32, *const u32) = std::mem::transmute(vfn(ctx, CTX_IA_SET_VBS));
        let set_ib: unsafe extern "system" fn(Obj, Obj, u32, u32) = std::mem::transmute(vfn(ctx, CTX_IA_SET_IB));
        let set_rs: unsafe extern "system" fn(Obj, Obj) = std::mem::transmute(vfn(ctx, vt::CTX_RS_SET_STATE));
        let set_vp: unsafe extern "system" fn(Obj, u32, *const Viewport) = std::mem::transmute(vfn(ctx, vt::CTX_RS_SET_VIEWPORTS));
        let set_sc: unsafe extern "system" fn(Obj, u32, *const [i32; 4]) = std::mem::transmute(vfn(ctx, vt::CTX_RS_SET_SCISSORS));
        let set_rt: unsafe extern "system" fn(Obj, u32, *const Obj, Obj) = std::mem::transmute(vfn(ctx, vt::CTX_OM_SET_RENDER_TARGETS));
        let set_blend: unsafe extern "system" fn(Obj, Obj, *const f32, u32) = std::mem::transmute(vfn(ctx, vt::CTX_OM_SET_BLEND));
        let set_depth: unsafe extern "system" fn(Obj, Obj, u32) = std::mem::transmute(vfn(ctx, vt::CTX_OM_SET_DEPTH));
        let clear_rtv: unsafe extern "system" fn(Obj, Obj, *const f32) = std::mem::transmute(vfn(ctx, 50));
        let draw: unsafe extern "system" fn(Obj, u32, u32) = std::mem::transmute(vfn(ctx, vt::CTX_DRAW));
        // The target may still be bound as t24 (the previous frame's light-adjust draw): never while it is a render target.
        set_ps_srv(ctx, SRV_SLOT, 1, &nul);
        set_ps_srv(ctx, SKIN_SRV_SLOT, 1, &nul); // v11: the skin target too (the previous frame's t25)
        s.skin_n = 0;
        // v11: the fur mask the light-adjust draw reads (t20, bound by furmask.rs just before): the skin pass checks its uv.
        let mut fur_mask: Obj = nul;
        {
            let get_ps_srv: unsafe extern "system" fn(Obj, u32, u32, *mut Obj) = std::mem::transmute(vfn(ctx, vt::CTX_PS_GET_SRVS));
            get_ps_srv(ctx, crate::furmask::SRV_SLOT, 1, &mut fur_mask);
        }
        set_ps_srv(ctx, 0, 2, [nul, nul].as_ptr());
        set_ps_srv(ctx, 2, 3, [nul, nul, nul].as_ptr());
        clear_rtv(ctx, s.target.rtv, [0.0f32; 4].as_ptr());
        set_rt(ctx, 1, &s.target.rtv, nul);
        set_blend(ctx, s.blend, [1.0f32; 4].as_ptr(), 0xffff_ffff);
        set_depth(ctx, s.dss, 0);
        set_ps(ctx, s.ps, std::ptr::null(), 0);
        let disc = discrim();
        let texel_tests = disc.sel.is_some();
        let ps_base = ps_cb_values(c, depth_unit);
        let ps_ext = ps_ext_values(depth_unit);
        set_ps_cb(ctx, 0, 1, &s.cb_ps);
        set_gs_srv(ctx, 0, 1, &dsrv);
        set_gs_cb(ctx, 0, 1, &s.cb_gs);
        let time = time_s();
        // The engine's depth buffer of each record, viewed for the geometry shader and the strand pixel shader (t1).
        let mut devs: [Obj; MAX_RECS] = [nul; MAX_RECS];
        for (i, d) in devs.iter_mut().enumerate().take(s.recs.len()) {
            let res = s.recs[i].depth;
            *d = s.dev_depth_view(dev, res);
        }
        // The geometry shaders of every record (pending / failed ones are skipped).
        let mut gss: [(Obj, Obj, Obj, Obj); MAX_RECS] = [(nul, nul, nul, nul); MAX_RECS];
        let (mut pending, mut failed) = (0u32, 0u32);
        for (i, slot) in gss.iter_mut().enumerate().take(s.recs.len()) {
            match gs_for(s.recs[i].vs_ptr) {
                Ok((key, blobs)) => {
                    // v19: a content-rule draw (another creature) never makes its shader objects during play: the driver takes
                    // tens of ms each on the game's single-threaded device. They are made on the next frame without the scene
                    // (a loading screen, the character select: furwarm.rs); until then that draw has no shells.
                    *slot = if s.recs[i].auto && !crate::warm::active() && !s.gs.iter().any(|e| e.0 == key) {
                        crate::furwarm::defer();
                        (nul, nul, nul, nul)
                    } else {
                        s.gs_object(dev, &key, &blobs, false)
                    };
                    if slot.0.is_null() {
                        failed += 1;
                    }
                }
                Err(e) => {
                    if e == GS_PENDING {
                        pending += 1;
                    } else {
                        failed += 1;
                    }
                }
            }
        }
        // The engine's state of one record (its vertex shader, buffers, constants, rasterizer, viewport).
        let bind_rec = |r: &Rec, rs: Obj, devv: Obj| {
            set_vs(ctx, r.vs, std::ptr::null(), 0);
            set_layout(ctx, r.layout);
            set_topo(ctx, r.topology);
            set_vbs(ctx, 0, 4, r.vb.as_ptr(), r.strides.as_ptr(), r.offsets.as_ptr());
            set_ib(ctx, r.ib, r.ib_fmt, r.ib_off);
            set_vs_cb(ctx, 0, 4, r.vcb.as_ptr());
            set_vs_srv(ctx, 0, 4, r.vsrv.as_ptr());
            set_vs_smp(ctx, 0, 2, r.vsmp.as_ptr());
            set_rs(ctx, rs);
            set_gs_srv(ctx, 1, 1, &devv);
        };
        // The unit (engine units per metre): `shell_unit`, else ONE smoothed measurement for every triangle of the frame. First the
        // sums the GPU finished in earlier frames, then this frame's measurement (read back in a later frame, never waiting).
        let auto = c.clamped().unit <= 0.0;
        if auto {
            s.read_units(ctx);
            if let Ok(mut u) = UNIT_SHOW.try_lock() {
                *u = s.unit;
            }
            let slot = s.runs as usize % 3;
            if !s.meas.rtv.is_null() && !s.stage[slot].is_null() && !s.staged[slot] {
                clear_rtv(ctx, s.meas.rtv, [0.0f32; 4].as_ptr());
                set_rt(ctx, 1, &s.meas.rtv, nul);
                set_blend(ctx, s.blend_add, [1.0f32; 4].as_ptr(), 0xffff_ffff);
                set_ps(ctx, s.ps_meas, std::ptr::null(), 0);
                let one = Viewport { x: 0.0, y: 0.0, w: 1.0, h: 1.0, min_depth: 0.0, max_depth: 1.0 };
                let mut measured = 0u32;
                for i in 0..s.recs.len() {
                    let (_, mg, _, _) = gss[i];
                    if mg.is_null() {
                        continue;
                    }
                    let r = &s.recs[i];
                    let geom = DrawGeom { vp: [r.vp.x, r.vp.y, r.vp.w, r.vp.h], ccw: r.ccw };
                    if !write_cb(ctx, s.cb_gs, &gs_cb_values(c, &geom, time, depth_unit, 0.0)) {
                        continue;
                    }
                    bind_rec(r, s.rs_meas, devs[i]);
                    set_gs(ctx, mg, std::ptr::null(), 0);
                    set_vp(ctx, 1, &one);
                    set_sc(ctx, 1, &[0, 0, 1, 1]);
                    r.call.issue(ctx);
                    measured += 1;
                }
                if measured > 0 {
                    let copy: unsafe extern "system" fn(Obj, Obj, Obj) = std::mem::transmute(vfn(ctx, vt::CTX_COPY_RESOURCE));
                    set_rt(ctx, 0, std::ptr::null(), nul);
                    copy(ctx, s.stage[slot], s.meas.tex);
                    s.staged[slot] = true;
                    s.runs = s.runs.wrapping_add(1);
                }
            }
        }
        // v7, the per-draw coat statistics. (1) Every record's mesh drawn in UV space into its own FEAT_N x FEAT_N block of the feature
        // target (gs_uv + ps_feat with the draw's atlas at t2; cleared to -1 = not covered). (2) One full-screen draw reduces every block
        // to the record's two texels of the statistics target (ps_stats). A record without an atlas or a uv-space shader keeps -1 cells:
        // its statistics say "no footprint" and it gets no shells (fail closed).
        s.read_stats(ctx);
        set_blend(ctx, nul, [1.0f32; 4].as_ptr(), 0xffff_ffff);
        clear_rtv(ctx, s.feat.rtv, [-1.0f32; 4].as_ptr());
        set_rt(ctx, 1, &s.feat.rtv, nul);
        set_ps(ctx, s.ps_feat, std::ptr::null(), 0);
        for i in 0..s.recs.len() {
            let (_, _, ug, _) = gss[i];
            let r = &s.recs[i];
            if ug.is_null() || r.tex0.is_null() {
                continue;
            }
            bind_rec(r, s.rs_meas, devs[i]);
            set_gs(ctx, ug, std::ptr::null(), 0);
            set_ps_srv(ctx, 2, 1, &r.tex0);
            let cell = Viewport { x: 0.0, y: (i as u32 * FEAT_N) as f32, w: FEAT_N as f32, h: FEAT_N as f32, min_depth: 0.0, max_depth: 1.0 };
            set_vp(ctx, 1, &cell);
            set_sc(ctx, 1, &[0, (i as u32 * FEAT_N) as i32, FEAT_N as i32, ((i as u32 + 1) * FEAT_N) as i32]);
            r.call.issue(ctx);
        }
        set_gs(ctx, nul, std::ptr::null(), 0);
        set_ps_srv(ctx, 2, 1, &nul);
        set_rt(ctx, 1, &s.stats.rtv, nul);
        set_ps_srv(ctx, 4, 1, &s.feat.srv);
        set_vs(ctx, s.vs_stats, std::ptr::null(), 0);
        set_ps(ctx, s.ps_stats, std::ptr::null(), 0);
        set_layout(ctx, nul);
        set_topo(ctx, 4 /* TRIANGLELIST */);
        set_rs(ctx, s.rs_meas);
        let sv = Viewport { x: 0.0, y: 0.0, w: MAX_RECS as f32, h: 2.0, min_depth: 0.0, max_depth: 1.0 };
        set_vp(ctx, 1, &sv);
        set_sc(ctx, 1, &[0, 0, MAX_RECS as i32, 2]);
        draw(ctx, 3, 0);
        set_ps_srv(ctx, 4, 1, &nul);
        set_rt(ctx, 0, std::ptr::null(), nul);
        s.stage_stats(ctx);
        // v9, the UV-island classification (furisland.rs): per mesh, once (cached for the session), the triangles' clip positions and
        // atlas uv are captured through the engine's vertex shader with a stream-output geometry shader (one mesh per frame), read
        // back a frame or more later (never waiting) and analysed on a background thread; the per-triangle bits (excluded / near) are
        // then bound as the shell geometry shader's t2. Until then (and when the analysis fails) t2 stays unbound: the v8 texel rule
        // alone, bit for bit.
        let mut isl_srv: [Obj; MAX_RECS] = [nul; MAX_RECS];
        let mut isl_st = [crate::furisland::DrawIsl::default(); MAX_RECS];
        if crate::furisland::enabled() {
            s.islands.tick(dev, ctx);
            let so_set: unsafe extern "system" fn(Obj, u32, *const Obj, *const u32) = std::mem::transmute(vfn(ctx, CTX_SO_SET_TARGETS));
            let mut captured = false;
            for i in 0..s.recs.len() {
                let r = &s.recs[i];
                let count = r.call.count();
                isl_st[i].count = count;
                let analysable = r.topology == 4 /* TRIANGLELIST */ && r.call.instances() == 1 && count >= 3 && count % 3 == 0 && count / 3 <= crate::furisland::MAX_TRIS && !gss[i].3.is_null();
                let (start, base) = r.call.start_base();
                // v15: a record drawn from our copies of dynamic buffers (pooled: another mesh in another frame) is keyed on what the
                // mesh IS (its atlas, index count, vertex shader, strides); the uv hash re-check (REVERIFY_FRAMES) catches a change
                let key = if r.copied {
                    crate::furisland::mesh_key(&[0xD15C_0B1E, r.tex0 as u64, count as u64, r.vs_ptr as u64, r.strides[0] as u64, r.strides[1] as u64, r.ib_fmt as u64])
                } else {
                    crate::furisland::mesh_key(&[
                    r.ib as u64,
                    r.ib_off as u64,
                    r.ib_fmt as u64,
                    r.vb[0] as u64,
                    r.offsets[0] as u64,
                    r.strides[0] as u64,
                    r.vb[1] as u64,
                    r.offsets[1] as u64,
                    count as u64,
                    start as u64,
                    base as i64 as u64,
                    r.tex0 as u64,
                    r.vs_ptr as u64,
                ])
                };
                let want = match s.islands.look(key, analysable, &mut isl_st[i]) {
                    crate::furisland::Look::Use(v) => {
                        isl_srv[i] = v;
                        false
                    }
                    crate::furisland::Look::UseAndVerify(v) => {
                        isl_srv[i] = v;
                        true
                    }
                    crate::furisland::Look::Capture => true,
                    crate::furisland::Look::None => false,
                };
                if want && !captured {
                    captured = true;
                    let p11 = 1.0 / TAN_HALF_FOV;
                    let p00 = p11 * r.vp.h.max(1.0) / r.vp.w.max(1.0);
                    let so = s.islands.begin(dev, ctx, key, count, r.tex0, p00, p11);
                    if !so.is_null() {
                        bind_rec(r, s.rs_meas, devs[i]);
                        set_gs(ctx, gss[i].3, std::ptr::null(), 0);
                        set_ps(ctx, nul, std::ptr::null(), 0);
                        so_set(ctx, 1, &so, &0);
                        r.call.issue(ctx);
                        so_set(ctx, 1, &nul, &0);
                        s.islands.end(ctx, key);
                    }
                }
            }
            set_gs(ctx, nul, std::ptr::null(), 0);
        }
        crate::furisland::publish_status(&isl_st[..s.recs.len().min(MAX_RECS)]);
        // (tests: FORCE_TRI keeps the per-triangle guess of the first version, to measure what the single unit fixes)
        let unit = if auto && !FORCE_TRI.load(Relaxed) { s.unit.published } else { 0.0 };
        // The shells: the key target with our score depth buffer (cleared to 1 = nothing).
        let clear_dsv: unsafe extern "system" fn(Obj, Obj, u32, f32, u8) = std::mem::transmute(vfn(ctx, CTX_CLEAR_DSV));
        clear_dsv(ctx, s.zdsv, 1 /* DEPTH */, 1.0, 0);
        set_rt(ctx, 1, &s.target.rtv, s.zdsv);
        set_blend(ctx, nul, [1.0f32; 4].as_ptr(), 0xffff_ffff);
        set_depth(ctx, s.dss, 0);
        set_ps(ctx, s.ps, std::ptr::null(), 0);
        set_ps_srv(ctx, 0, 1, &dsrv);
        set_ps_srv(ctx, 3, 1, &s.stats.srv);
        set_gs_srv(ctx, 0, 1, &dsrv);
        set_gs_cb(ctx, 0, 1, &s.cb_gs);
        let mut drawn = 0u32;
        // v19: the frame's strands are shared out: many fur draws on screen get fewer shells each (shell_share).
        let n_fur = (0..s.recs.len()).filter(|&i| !gss[i].0.is_null() && shells_allowed(s.recs[i].auto, s.recs[i].tex0 as usize)).count();
        let shared = ShellCfg { shells: shell_share(c.shells, n_fur), ..c.clone() };
        for i in 0..s.recs.len() {
            let (gs, _, _, _) = gss[i];
            if gs.is_null() {
                continue;
            }
            let r = &s.recs[i];
            if !shells_allowed(r.auto, r.tex0 as usize) {
                continue;
            }
            let geom = DrawGeom { vp: [r.vp.x, r.vp.y, r.vp.w, r.vp.h], ccw: r.ccw };
            if !write_cb(ctx, s.cb_gs, &gs_cb_values(&shared, &geom, time, depth_unit, unit)) {
                continue;
            }
            let mut rects = ps_rect_values(&disc, r.call.count());
            rects[0][2] = i as f32;
            rects[0][3] = if texel_tests { 1.0 } else { 0.0 };
            let mut bias = ps_bias_values(&r.rsd, s.dev_depth_fmt(r.depth));
            bias[3] = mixed_flag(r.tex0 as usize);
            if !write_cb(ctx, s.cb_ps, &PsCb { base: ps_base, rects, ext: ps_ext, bias }) {
                continue; // never draw a record without its exclusions
            }
            bind_rec(r, if r.rs.is_null() { s.rs_fill } else { r.rs }, devs[i]);
            set_ps_srv(ctx, 1, 1, &devs[i]);
            set_ps_srv(ctx, 2, 1, &r.tex0);
            set_gs_srv(ctx, crate::furisland::GS_SLOT, 1, &isl_srv[i]); // v9: null = the texel rule alone
            set_gs(ctx, gs, std::ptr::null(), 0);
            set_vp(ctx, 1, &r.vp);
            set_sc(ctx, 1, &r.scissor);
            r.call.issue(ctx);
            drawn += 1;
        }
        // v11, the skin pass: every record's base layer once more (g5.x = 1: no LOD end, no edge-on fade; the island bits applied) into
        // the R8 skin target with a MAX blend and no depth buffer (ps_skin tests the root itself): how much the visible surface is proven
        // fur skin. The old fur look and the rim light are gated by it (t25).
        if !s.skin.rtv.is_null() && !s.ps_skin.is_null() {
            clear_rtv(ctx, s.skin.rtv, [0.0f32; 4].as_ptr());
            set_rt(ctx, 1, &s.skin.rtv, nul);
            set_blend(ctx, s.blend, [1.0f32; 4].as_ptr(), 0xffff_ffff);
            set_depth(ctx, nul, 0);
            set_ps(ctx, s.ps_skin, std::ptr::null(), 0);
            set_ps_srv(ctx, 0, 1, &dsrv);
            set_ps_srv(ctx, 3, 1, &s.stats.srv);
            set_ps_srv(ctx, 4, 1, &fur_mask);
            set_gs_srv(ctx, 0, 1, &dsrv);
            set_gs_cb(ctx, 0, 1, &s.cb_gs);
            for i in 0..s.recs.len() {
                let (gs, _, _, _) = gss[i];
                if gs.is_null() {
                    continue;
                }
                let r = &s.recs[i];
                if !shells_allowed(r.auto, r.tex0 as usize) {
                    continue;
                }
                let geom = DrawGeom { vp: [r.vp.x, r.vp.y, r.vp.w, r.vp.h], ccw: r.ccw };
                if !write_cb(ctx, s.cb_gs, &skin_gs_cb_values(c, &geom, time, depth_unit, unit)) {
                    continue;
                }
                let mut rects = ps_rect_values(&disc, r.call.count());
                rects[0][2] = i as f32;
                rects[0][3] = if texel_tests { 1.0 } else { 0.0 };
                let mut bias = ps_bias_values(&r.rsd, s.dev_depth_fmt(r.depth));
                bias[3] = mixed_flag(r.tex0 as usize);
                if !write_cb(ctx, s.cb_ps, &PsCb { base: ps_base, rects, ext: ps_ext, bias }) {
                    continue;
                }
                bind_rec(r, if r.rs.is_null() { s.rs_fill } else { r.rs }, devs[i]);
                set_ps_srv(ctx, 1, 1, &devs[i]);
                set_ps_srv(ctx, 2, 1, &r.tex0);
                set_gs_srv(ctx, crate::furisland::GS_SLOT, 1, &isl_srv[i]);
                set_gs(ctx, gs, std::ptr::null(), 0);
                set_vp(ctx, 1, &r.vp);
                set_sc(ctx, 1, &r.scissor);
                r.call.issue(ctx);
                s.skin_n += 1;
            }
            set_ps_srv(ctx, 4, 1, &nul);
        }
        release(fur_mask);
        set_gs(ctx, nul, std::ptr::null(), 0);
        set_ps_srv(ctx, 0, 2, [nul, nul].as_ptr());
        set_gs_srv(ctx, 0, 3, [nul, nul, nul].as_ptr());
        drop(pv);
        drop(x);
        drop(g); // restores targets, viewports, scissors, rasterizer, blend, depth state, shaders, layout, topology, PS t0..t4 / b0
        let (dc, db, ev) = (s.dyn_copied_frame, s.dyn_bytes_frame, s.evicted_frame);
        let tris: u32 = s.recs.iter().map(|r| (r.call.count() / 3).saturating_mul(r.call.instances().max(1))).sum();
        with_cur(|f| {
            f.replayed = drawn;
            f.pending = pending;
            f.gs_failed = failed;
            (f.dyn_copied, f.dyn_bytes, f.tris, f.evicted) = (dc, db, tris, ev);
        });
        drawn
    }
}

/// The shell target's view (tests read it back).
#[allow(dead_code)]
pub(crate) fn target_srv() -> Obj {
    STATE.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map_or(std::ptr::null_mut(), |s| s.target.srv)
}

/// Number of draws recorded so far this frame (tests).
#[allow(dead_code)]
pub(crate) fn recorded() -> usize {
    STATE.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map_or(0, |s| s.recs.len())
}

/// Drops the device objects (tests: a new WARP device).
#[allow(dead_code)]
pub(crate) fn reset_state() {
    *STATE.lock().unwrap_or_else(|e| e.into_inner()) = None;
    *UNIT_SHOW.lock().unwrap_or_else(|e| e.into_inner()) = UnitTrack::default();
    STATS_SHOW.lock().unwrap_or_else(|e| e.into_inner()).clear();
    crate::furisland::reset();
}

/// The strand pixel shader blob (tests compile it synchronously when the background thread has not run).
#[allow(dead_code)]
pub(crate) fn ensure_ps_blob() {
    let _ = PS_BLOB.set(compile_pixel_shaders());
}

#[cfg(test)]
#[path = "shellfur_tests.rs"]
mod tests;
