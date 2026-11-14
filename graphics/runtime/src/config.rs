//! `GW2Graphics\config.ini`: simple `key=value` lines. Unknown keys are ignored and bad
//! values fall back to the defaults; numeric values are clamped to their valid range.

use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HdrOutput {
    /// Scene HDR output only when nothing else could be hurt by it (no ReShade).
    Auto,
    /// Never.
    Off,
    /// The player opts in: ignores ReShade's presence (Windows HDR and an unchanged swapchain are still required).
    On,
}

/// Format of the HDR swapchain (`docs/hdr10.md`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HdrFormat {
    /// float16 scRGB (linear, 1.0 = 80 nits): the default, the long-tested path.
    Scrgb,
    /// EXPERIMENTAL: R10G10B10A2 with the HDR10 (PQ, BT.2020) colour space.
    Hdr10,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub enabled: bool,
    /// v18 (diagnostic, read at start): 0 = no float render-target upgrade at all: the engine's own formats, the gate never opens,
    /// no substitute runs (the engine-identical path for an A/B; needs a game restart).
    pub hdr_upgrade: bool,
    /// v18: the UNORM clamp emulation of the upgraded targets (`clampemu.rs`): engine shaders drawing into an upgraded target get
    /// the 8-bit target's floor, NaN removal and alpha range (values above 1 kept). 0 = off (A/B, live from the overlay).
    pub hdr_clamp: bool,
    pub hdr_output: HdrOutput,
    /// `scrgb` (default) or `hdr10` (experimental, restart needed, ignored with ReShade; `docs/hdr10.md`).
    pub hdr_format: HdrFormat,
    pub paper_white_nits: f32,
    pub peak_nits: f32,
    /// HDR output: the power the SDR range of the picture is decoded with (2.2 = default, like an SDR monitor; 0 / `srgb` = the
    /// piecewise sRGB curve Windows uses for SDR content, brighter blacks). 1.8..2.8. Live. `docs/hdr10.md` "Black level".
    pub sdr_gamma: f32,
    pub bloom_strength: f32,
    pub highlight_boost: f32,
    pub ao_strength: f32,
    pub ao_radius: f32,
    pub halo_strength: f32,
    pub halo_radius: f32,
    pub ssr_strength: f32,
    pub ssr_range: f32,
    /// Bitmask of the scene-alpha material classes that may reflect: 1 low, 2 mid, 4 high.
    pub ssr_classes: u32,
    /// Built-in material heuristics (vegetation, rough texture): 0 off .. 1 full.
    pub ssr_smart: f32,
    /// Hue gate of the reflectivity score (green / warm-earthy receivers do not reflect): 0 off .. 1 full.
    pub ssr_hue_filter: f32,
    /// Large-scale flatness requirement of the reflectivity score (varying normals do not reflect): 0 off .. 1 full.
    pub ssr_flatness: f32,
    /// Water / glass reflections (Fresnel, tint, sky at the screen edge, calm vs rippled blur) on top of the reflectivity score: 0 = the old trace, bit for bit (`docs/ssr.md`).
    pub ssr_water_boost: f32,
    /// Depth look of water: the receiver is darkened by up to 15 % x this where the reflection is strong (needs `ssr_water_boost` > 0). 0 = off.
    pub water_depth_look: f32,
    /// Matte gate of the reflectivity score (`docs/ssr.md`, "Matte gate"): 0 = the score as before .. 1 = only water / glass / ice under the frame's own light keeps it. Default 0.8.
    pub ssr_matte_cut: f32,
    pub shafts_strength: f32,
    pub shafts_length: f32,
    /// Screen-space contact shadows: largest darkening of the diffuse light, 0 = off (bit-identical) .. 0.8 (`docs/contact-shadows.md`).
    pub contact_strength: f32,
    /// Length of the contact shadow ray (1 = about 0.35 m).
    pub contact_length: f32,
    /// Where the light comes from: `auto` (the sun shafts' estimate when found, else a fixed direction from above) or `fixed`.
    pub contact_sun: String,
    /// Aerial perspective (distance haze on far pixels): 0 = off (bit-identical) .. 1; capped at 35 % of the haze colour (`docs/aerial.md`).
    pub aerial_strength: f32,
    /// Metres beyond 15 m at which the haze reaches half of its amount.
    pub aerial_distance: f32,
    /// Metres per 1.0 of the engine's NORMALISED linear depth (1.0 = far plane / sky), default 1000 (`aerial::DEFAULT_UNIT`).
    /// Old inch-based values (<= 2, the old default 0.0254) are read as "unset" and replaced by the default.
    pub depth_unit: f32,
    /// Forced anisotropic filtering of world texture samplers: 0 or 1 = off (the engine's own samplers), 2..16 = max anisotropy (`docs/aa-aniso.md`).
    pub aniso_level: u32,
    /// Mip LOD bias added to the same samplers (-1 sharper .. 0.5 softer; 0 = none).
    pub mip_bias: f32,
    /// Mod-owned SMAA 1x (medium, HDR-aware edges) on the 3D scene before the UI.
    pub smaa: bool,
    /// SMAA luma edge threshold (lower = more edges).
    pub smaa_threshold: f32,
    /// CAS-style contrast adaptive sharpening after the AA: 0 = off .. 1.
    pub sharpen_strength: f32,
    /// Auto exposure (eye adaptation) strength: 0 = off (bit-identical) .. 1; slope of the compensation (`docs/exposure.md`).
    pub autoexp_strength: f32,
    /// Mid-grey the auto exposure aims the log-average scene luminance at (linear, 1 = SDR white).
    pub autoexp_target: f32,
    /// Largest compensation in EV (brightening is also capped at +1 EV).
    pub autoexp_range: f32,
    /// Seconds to adapt when the scene gets brighter (time constant).
    pub adapt_up: f32,
    /// Seconds to adapt when the scene gets darker (time constant; the eye is slower there).
    pub adapt_down: f32,
    /// Local tone mapping strength: 0 = off (bit-identical) .. 1; compresses big bright areas, keeps their detail.
    pub localtone_strength: f32,
    /// Lens effects (docs/lens.md): streak / glare strength, 0 = off (bit-identical, no passes) .. 1.
    pub lens_strength: f32,
    /// Source energy above which the lens triggers, in units of paper white of the scene BEFORE the highlight boost (2.5 = default).
    pub lens_threshold: f32,
    /// Ghost flares of VERY bright point sources only: 0 = off .. 0.5.
    pub lens_ghost: f32,
    /// Length of the horizontal streak (1 = a sixth of the picture width each side).
    pub lens_streak_length: f32,
    /// Colour grading (docs/grading.md): a preset name applied at load before the individual keys (empty = none).
    pub grade_preset: String,
    pub grade_saturation: f32,
    pub grade_contrast: f32,
    pub grade_temperature: f32,
    pub grade_tint: f32,
    pub grade_vibrance: f32,
    pub grade_shadows: f32,
    pub grade_highlights: f32,
    /// Optional 3D LUT: a `.cube` path inside the GW2Graphics folder (empty = none).
    pub grade_lut: String,
    pub grade_lut_strength: f32,
    /// Fur look strength on the pixels of the fur mask: 0 off (bit-identical) .. 1 (full, visible). Strands anchored to the mesh uv (when the vertex shader has one), texture detail unsharp mask, fuzzy warm rim / feathered outline, small warm subsurface.
    pub fur_strength: f32,
    /// Fur selector (docs/draw-diagnostic.md, "Fur: the character mask"): comma-separated 8-hex pixel-shader hashes of the draws that
    /// count as fur (empty = no mask, the fur is off). Hot-reloadable.
    pub fur_ps: String,
    /// Optional vertex-shader hashes the fur draws must also use (empty = any).
    pub fur_vs: String,
    /// Only draws whose vertex constant buffer 0 is at least this many bytes (skinned meshes) count as fur.
    pub fur_min_vcb: u32,
    /// Paint the selected pixels magenta in the light-adjust output (to see what the selector picked).
    pub fur_debug: bool,
    /// Which vertex-shader output carries the mesh uv for the fur strands: `auto` (the lowest TEXCOORD with two float components)
    /// or `TEXCOORDn` / `TEXCOORDn.xy` / `TEXCOORDn.zw`. Hot-reloadable. If none is usable the fur has no strands (rim / detail only).
    pub fur_uv: String,
    /// Occlusion check of the fur and rim masks (`docs/rim.md`): 1 = on (default; mask pixels covered by geometry drawn later, e.g. a
    /// hood over the head, are erased), 0 = bypassed (the masks stay as drawn, the A/B switch). Hot-reloadable.
    pub fur_occlusion: u8,
    /// Live fur look (`docs/fur-look.md`), acts on the pixels of the fur mask: brightness modulation of the strands, 0..1 (0.32 = the strands the fur always had).
    pub fur_amp: f32,
    /// Multiplies the strand cell sizes, 0.25..4 (1 = default).
    pub fur_scale: f32,
    /// Strand length relative to its width, 1..12 (5 = default).
    pub fur_length: f32,
    /// Gain of the texture detail (unsharp mask), 0..2 (1 = default).
    pub fur_detail: f32,
    /// Rim glow and feather of the outline, 0..2 (0.25 = default, subtle; 1 = the outline the fur look drew before this key existed; 0 = none).
    pub fur_rim: f32,
    /// Warm subsurface term of the mid / dark fur, 0..2 (1 = default).
    pub fur_sss: f32,
    /// Fur diagnostics painted on the pixels of the fur mask only: 0 off, 1 mesh uv as colour, 2 strand pattern, 3 rim band, 4 strand fade.
    /// RUNTIME ONLY like `debug_view`: parsed (hot reload applies it) but `to_ini` always writes 0 and the start ignores it.
    pub fur_view: u8,
    /// Shell fur (`docs/shell-fur.md`, `shellfur.rs`): the keys `fur_shell`, `fur_shells`, `shell_*`.
    pub shell: crate::shellfur::ShellCfg,
    /// v17: the per-effect switches (`<effect>_enabled`, toggles.rs).
    pub toggles: crate::toggles::Toggles,
    /// Rim light on characters (`docs/rim.md`): largest extra luminance share 0..1 (0 = off, bit-identical, no extra masking cost).
    pub rim_strength: f32,
    /// Width of the rim from the silhouette, in pixels (1..8).
    pub rim_width: f32,
    /// Subsurface glow of the dark-lit, thin parts of the rim mask (0..1, at most +8 % luminance); needs `rim_strength` > 0.
    pub subsurface_strength: f32,
    /// Pixel-shader hashes of the character draws that get the rim light (default: the verified charr skin shaders). Empty = off.
    pub rim_ps: String,
    /// Only draws whose vertex constant buffer 0 is at least this many bytes (skinned meshes) count for the rim.
    pub rim_min_vcb: u32,
    /// Paint the rim selection cyan in the light-adjust output.
    pub rim_debug: bool,
    pub overlay_key: String,
    /// Dev feedback flow (all off by default): a screenshot (pair) every N seconds, 0 = off.
    pub dev_capture_seconds: u32,
    /// A periodic capture is a pair: the mod shot and a vanilla one (Enabled forced off for a moment).
    pub dev_compare: bool,
    /// Re-read config.ini when it changes and apply the live keys.
    pub dev_hot_reload: bool,
    /// Debug view 0..5 (0 off, 1 boost / occlusion, 2 fill, 3 material classes, 4 sun, 5 reflectivity heat map): one more source of
    /// `resolve::debug_level()`, so the heat maps can be switched from outside. RUNTIME ONLY: parsed (hot reload applies it) but
    /// `to_ini` always writes 0 (a debug tint must never persist by accident) and the start ignores it.
    pub debug_view: u8,
    /// Photo mode hotkey (`docs/photo.md`); same syntax as `overlay_key`. Read at start.
    pub photo_key: String,
    /// Photo mode: save the scene without the UI when the pre-UI copy is available (single captures only).
    pub photo_hide_ui: bool,
    /// HDR calibration test pattern: 0 = off, 1 = peak, 2 = paper white, 3 = black level. RUNTIME ONLY: parsed (so hot reload can
    /// switch it) but never written to config.ini and never applied at start; a 30 s watchdog turns it off.
    pub calibration_pattern: u8,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: true,
            hdr_upgrade: true,
            hdr_clamp: true,
            hdr_output: HdrOutput::Auto,
            hdr_format: HdrFormat::Scrgb,
            paper_white_nits: 203.0,
            peak_nits: 1000.0,
            sdr_gamma: crate::output::SDR_GAMMA_DEFAULT,
            bloom_strength: 1.0,
            highlight_boost: 2.5,
            ao_strength: 1.0,
            ao_radius: 1.0,
            halo_strength: 1.0,
            halo_radius: 1.0,
            ssr_strength: 0.5,
            ssr_range: 1.0,
            ssr_classes: 7,
            ssr_smart: 1.0,
            ssr_hue_filter: 1.0,
            ssr_flatness: 1.0,
            ssr_water_boost: 0.6,
            water_depth_look: 0.3,
            ssr_matte_cut: 0.8,
            shafts_strength: 0.6,
            shafts_length: 0.7,
            contact_strength: 0.5,
            contact_length: 1.0,
            contact_sun: "auto".to_string(),
            aerial_strength: 0.35,
            aerial_distance: 400.0,
            depth_unit: crate::aerial::DEFAULT_UNIT,
            aniso_level: 16,
            mip_bias: 0.0,
            smaa: true,
            smaa_threshold: 0.1,
            sharpen_strength: 0.3,
            autoexp_strength: 0.5,
            autoexp_target: 0.20,
            autoexp_range: 0.7,
            adapt_up: 0.6,
            adapt_down: 1.8,
            localtone_strength: 0.2,
            lens_strength: 0.35,
            lens_threshold: 2.5,
            lens_ghost: 0.15,
            lens_streak_length: 1.0,
            grade_preset: String::new(),
            grade_saturation: 1.0,
            grade_contrast: 1.0,
            grade_temperature: 0.0,
            grade_tint: 0.0,
            grade_vibrance: 0.0,
            grade_shadows: 0.0,
            grade_highlights: 0.0,
            grade_lut: String::new(),
            grade_lut_strength: 1.0,
            fur_strength: 0.0,
            fur_ps: String::new(),
            fur_vs: String::new(),
            fur_min_vcb: 4096,
            fur_debug: false,
            fur_uv: "auto".to_string(),
            fur_occlusion: 1,
            fur_amp: crate::datapath::FUR_AMP_DEFAULT,
            fur_scale: crate::datapath::FUR_SCALE_DEFAULT,
            fur_length: crate::datapath::FUR_LENGTH_DEFAULT,
            fur_detail: crate::datapath::FUR_DETAIL_DEFAULT,
            fur_rim: crate::datapath::FUR_RIM_DEFAULT,
            fur_sss: crate::datapath::FUR_SSS_DEFAULT,
            fur_view: 0,
            shell: crate::shellfur::ShellCfg::default(),
            toggles: crate::toggles::Toggles::default(),
            rim_strength: 0.4,
            rim_width: 3.0,
            subsurface_strength: 0.3,
            rim_ps: crate::furpick::CHARR_PRESET.to_string(),
            rim_min_vcb: 4096,
            rim_debug: false,
            overlay_key: "Shift+End".to_string(),
            dev_capture_seconds: 0,
            dev_compare: true,
            dev_hot_reload: false,
            debug_view: 0,
            photo_key: "Ctrl+Shift+End".to_string(),
            photo_hide_ui: false,
            calibration_pattern: 0,
        }
    }
}

fn flag(v: &str, default: bool) -> bool {
    match v.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => true,
        "false" | "0" | "no" | "off" => false,
        _ => default,
    }
}

fn num(v: &str, lo: f32, hi: f32, default: f32) -> f32 {
    match v.parse::<f32>() {
        Ok(x) if x.is_finite() => x.clamp(lo, hi),
        _ => default,
    }
}

/// A hash list value: lower case, at most 400 characters, no control characters (it is written back to the ini as one line).
fn clean_list(v: &str) -> String {
    v.chars().filter(|c| !c.is_control()).take(400).collect::<String>().trim().to_ascii_lowercase()
}

/// A free text value (preset name, LUT path): no control characters, at most 200 characters, trimmed; case is kept.
fn clean_text(v: &str) -> String {
    v.chars().filter(|c| !c.is_control()).take(200).collect::<String>().trim().to_string()
}

impl Config {
    /// The grading sliders of this config.
    pub fn grade(&self) -> crate::grading::Grade {
        crate::grading::Grade { saturation: self.grade_saturation, contrast: self.grade_contrast, temperature: self.grade_temperature, tint: self.grade_tint, vibrance: self.grade_vibrance, shadows: self.grade_shadows, highlights: self.grade_highlights }
    }

    /// The live fur look of this config (the six parameters and the runtime-only view).
    pub fn fur_look(&self) -> crate::datapath::FurLook {
        crate::datapath::FurLook { amp: self.fur_amp, scale: self.fur_scale, length: self.fur_length, detail: self.fur_detail, rim: self.fur_rim, sss: self.fur_sss, view: self.fur_view }
    }

    pub fn set_grade(&mut self, g: &crate::grading::Grade) {
        self.grade_saturation = g.saturation;
        self.grade_contrast = g.contrast;
        self.grade_temperature = g.temperature;
        self.grade_tint = g.tint;
        self.grade_vibrance = g.vibrance;
        self.grade_shadows = g.shadows;
        self.grade_highlights = g.highlights;
    }

    pub fn parse(text: &str) -> Config {
        let mut c = Config::default();
        let d = Config::default();
        // `grade_preset` is applied first, the individual grade_* keys override it wherever they appear in the file.
        for line in text.lines() {
            if let Some((k, v)) = line.trim().split_once('=') {
                if k.trim().eq_ignore_ascii_case("grade_preset") {
                    c.grade_preset = clean_text(v);
                    if let Some(p) = crate::grading::preset_by_name(&c.grade_preset) {
                        c.set_grade(&p.grade);
                    }
                }
            }
        }
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') || line.starts_with('[') {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else { continue };
            let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
            match k.as_str() {
                "hdr_upgrade" => c.hdr_upgrade = !matches!(v.to_ascii_lowercase().as_str(), "0" | "false" | "no" | "off"),
                "hdr_clamp" => c.hdr_clamp = !matches!(v.to_ascii_lowercase().as_str(), "0" | "false" | "no" | "off"),
                "enabled" => {
                    c.enabled = match v.to_ascii_lowercase().as_str() {
                        "true" | "1" | "yes" | "on" => true,
                        "false" | "0" | "no" | "off" => false,
                        _ => d.enabled,
                    }
                }
                "hdr_output" => {
                    c.hdr_output = match v.to_ascii_lowercase().as_str() {
                        "auto" => HdrOutput::Auto,
                        "off" => HdrOutput::Off,
                        "on" => HdrOutput::On,
                        _ => d.hdr_output,
                    }
                }
                "hdr_format" => {
                    c.hdr_format = match v.to_ascii_lowercase().as_str() {
                        "scrgb" => HdrFormat::Scrgb,
                        "hdr10" => HdrFormat::Hdr10,
                        _ => d.hdr_format,
                    }
                }
                "paper_white_nits" => c.paper_white_nits = num(v, 80.0, 400.0, d.paper_white_nits),
                "peak_nits" => c.peak_nits = num(v, 400.0, 4000.0, d.peak_nits),
                "sdr_gamma" => {
                    c.sdr_gamma = if v.eq_ignore_ascii_case("srgb") { 0.0 } else { v.parse::<f32>().map_or(d.sdr_gamma, crate::output::clamp_sdr_gamma) };
                }
                "bloom_strength" => c.bloom_strength = num(v, 0.0, 2.0, d.bloom_strength),
                "highlight_boost" => c.highlight_boost = num(v, 1.0, 4.0, d.highlight_boost),
                "ao_strength" => c.ao_strength = num(v, 0.0, 2.0, d.ao_strength),
                "ao_radius" => c.ao_radius = num(v, 0.25, 4.0, d.ao_radius),
                "halo_strength" => c.halo_strength = num(v, 0.0, 2.0, d.halo_strength),
                "halo_radius" => c.halo_radius = num(v, 0.5, 2.0, d.halo_radius),
                "ssr_strength" => c.ssr_strength = num(v, 0.0, 2.0, d.ssr_strength),
                "ssr_range" => c.ssr_range = num(v, 0.25, 2.0, d.ssr_range),
                "ssr_classes" => c.ssr_classes = num(v, 0.0, 7.0, d.ssr_classes as f32).round() as u32,
                "ssr_smart" => c.ssr_smart = num(v, 0.0, 1.0, d.ssr_smart),
                "ssr_hue_filter" => c.ssr_hue_filter = num(v, 0.0, 1.0, d.ssr_hue_filter),
                "ssr_flatness" => c.ssr_flatness = num(v, 0.0, 1.0, d.ssr_flatness),
                "ssr_water_boost" => c.ssr_water_boost = num(v, 0.0, 1.0, d.ssr_water_boost),
                "water_depth_look" => c.water_depth_look = num(v, 0.0, 1.0, d.water_depth_look),
                "ssr_matte_cut" => c.ssr_matte_cut = num(v, 0.0, 1.0, d.ssr_matte_cut),
                "shafts_strength" => c.shafts_strength = num(v, 0.0, 2.0, d.shafts_strength),
                "shafts_length" => c.shafts_length = num(v, 0.3, 1.0, d.shafts_length),
                "contact_strength" => c.contact_strength = num(v, 0.0, 0.8, d.contact_strength),
                "contact_length" => c.contact_length = num(v, 0.25, 2.0, d.contact_length),
                "contact_sun" => c.contact_sun = if v.trim().eq_ignore_ascii_case("fixed") { "fixed".to_string() } else { "auto".to_string() },
                "aerial_strength" => c.aerial_strength = num(v, 0.0, 1.0, d.aerial_strength),
                "aerial_distance" => c.aerial_distance = num(v, 50.0, 5000.0, d.aerial_distance),
                "depth_unit" => c.depth_unit = crate::aerial::clamp_unit(v.parse::<f32>().unwrap_or(f32::NAN)),
                "aniso_level" => c.aniso_level = num(v, 0.0, 16.0, d.aniso_level as f32).round() as u32,
                "mip_bias" => c.mip_bias = num(v, -1.0, 0.5, d.mip_bias),
                "smaa" => c.smaa = flag(v, d.smaa),
                "smaa_threshold" => c.smaa_threshold = num(v, 0.02, 0.5, d.smaa_threshold),
                "sharpen_strength" => c.sharpen_strength = num(v, 0.0, 1.0, d.sharpen_strength),
                "autoexp_strength" => c.autoexp_strength = num(v, 0.0, 1.0, d.autoexp_strength),
                "autoexp_target" => c.autoexp_target = num(v, 0.05, 0.6, d.autoexp_target),
                "autoexp_range" => c.autoexp_range = num(v, 0.0, 2.0, d.autoexp_range),
                "adapt_up" => c.adapt_up = num(v, 0.05, 20.0, d.adapt_up),
                "adapt_down" => c.adapt_down = num(v, 0.05, 20.0, d.adapt_down),
                "localtone_strength" => c.localtone_strength = num(v, 0.0, 1.0, d.localtone_strength),
                "lens_strength" => c.lens_strength = num(v, 0.0, 1.0, d.lens_strength),
                "lens_threshold" => c.lens_threshold = num(v, 1.2, 8.0, d.lens_threshold),
                "lens_ghost" => c.lens_ghost = num(v, 0.0, 0.5, d.lens_ghost),
                "lens_streak_length" => c.lens_streak_length = num(v, 0.25, 2.0, d.lens_streak_length),
                "grade_saturation" => c.grade_saturation = num(v, 0.0, 2.0, d.grade_saturation),
                "grade_contrast" => c.grade_contrast = num(v, 0.5, 1.5, d.grade_contrast),
                "grade_temperature" => c.grade_temperature = num(v, -1.0, 1.0, d.grade_temperature),
                "grade_tint" => c.grade_tint = num(v, -1.0, 1.0, d.grade_tint),
                "grade_vibrance" => c.grade_vibrance = num(v, 0.0, 1.0, d.grade_vibrance),
                "grade_shadows" => c.grade_shadows = num(v, -0.5, 0.5, d.grade_shadows),
                "grade_highlights" => c.grade_highlights = num(v, -0.5, 0.5, d.grade_highlights),
                "grade_lut" => c.grade_lut = clean_text(v),
                "grade_lut_strength" => c.grade_lut_strength = num(v, 0.0, 1.0, d.grade_lut_strength),
                "grade_preset" => {} // applied before this loop
                "fur_strength" => c.fur_strength = num(v, 0.0, 1.0, d.fur_strength),
                "fur_ps" => c.fur_ps = clean_list(v),
                "fur_vs" => c.fur_vs = clean_list(v),
                "fur_min_vcb" => c.fur_min_vcb = num(v, 0.0, 1_048_576.0, d.fur_min_vcb as f32).round() as u32,
                "fur_debug" => c.fur_debug = flag(v, d.fur_debug),
                "fur_uv" => {
                    let u = clean_list(v);
                    c.fur_uv = if u.is_empty() { d.fur_uv.clone() } else { u };
                }
                "fur_occlusion" => c.fur_occlusion = num(v, 0.0, 1.0, d.fur_occlusion as f32).round() as u8,
                "fur_amp" => c.fur_amp = num(v, crate::datapath::FUR_AMP_RANGE.0, crate::datapath::FUR_AMP_RANGE.1, d.fur_amp),
                "fur_scale" => c.fur_scale = num(v, crate::datapath::FUR_SCALE_RANGE.0, crate::datapath::FUR_SCALE_RANGE.1, d.fur_scale),
                "fur_length" => c.fur_length = num(v, crate::datapath::FUR_LENGTH_RANGE.0, crate::datapath::FUR_LENGTH_RANGE.1, d.fur_length),
                "fur_detail" => c.fur_detail = num(v, crate::datapath::FUR_DETAIL_RANGE.0, crate::datapath::FUR_DETAIL_RANGE.1, d.fur_detail),
                "fur_rim" => c.fur_rim = num(v, crate::datapath::FUR_RIM_RANGE.0, crate::datapath::FUR_RIM_RANGE.1, d.fur_rim),
                "fur_sss" => c.fur_sss = num(v, crate::datapath::FUR_SSS_RANGE.0, crate::datapath::FUR_SSS_RANGE.1, d.fur_sss),
                "fur_view" => c.fur_view = num(v, 0.0, crate::datapath::FUR_VIEW_MAX as f32, 0.0).round() as u8,
                "rim_strength" => c.rim_strength = num(v, 0.0, 1.0, d.rim_strength),
                "rim_width" => c.rim_width = num(v, 1.0, 8.0, d.rim_width),
                "subsurface_strength" => c.subsurface_strength = num(v, 0.0, 1.0, d.subsurface_strength),
                "rim_ps" => c.rim_ps = clean_list(v),
                "rim_min_vcb" => c.rim_min_vcb = num(v, 0.0, 1_048_576.0, d.rim_min_vcb as f32).round() as u32,
                "rim_debug" => c.rim_debug = flag(v, d.rim_debug),
                "overlay_key" if !v.is_empty() => c.overlay_key = v.to_string(),
                "dev_capture_seconds" => c.dev_capture_seconds = num(v, 0.0, 3600.0, d.dev_capture_seconds as f32).round() as u32,
                "dev_compare" => c.dev_compare = flag(v, d.dev_compare),
                "dev_hot_reload" => c.dev_hot_reload = flag(v, d.dev_hot_reload),
                "debug_view" => c.debug_view = num(v, 0.0, 5.0, 0.0).round() as u8,
                "photo_key" if !v.is_empty() => c.photo_key = v.to_string(),
                "photo_hide_ui" => c.photo_hide_ui = flag(v, d.photo_hide_ui),
                "calibration_pattern" => c.calibration_pattern = num(v, 0.0, 3.0, 0.0).round() as u8,
                k if crate::shellfur::ShellCfg::is_key(k) => {
                    c.shell.set(k, v);
                }
                k if crate::toggles::Toggles::is_key(k) => {
                    c.toggles.set(k, v);
                }
                _ => {}
            }
        }
        c
    }

    /// The whole config as `config.ini` text: every key, with a short comment header.
    pub fn to_ini(&self) -> String {
        let mut t = self.to_ini_base();
        // The shell fur block follows the fur look (its keys are `ShellCfg::ini`).
        let at = t.find("fur_view=0\n").map_or(t.len(), |i| i + "fur_view=0\n".len());
        t.insert_str(at, &format!("\n{}", self.shell.ini()));
        // v18: the diagnostic upgrade switch
        t.push_str(&format!("\n# Diagnostic (v18): hdr_upgrade=0 turns the float render-target upgrade off at the next game start (the engine's own\n# formats; no substitute, no effect runs: the engine-identical path for an A/B). 1 = default.\nhdr_upgrade={}\n", self.hdr_upgrade as u8));
        t.push_str(&format!("# v18: hdr_clamp=1 gives engine shaders drawing into an upgraded target the 8-bit target's clamp (no negative colour,\n# no NaN, alpha 0..1; values above 1 kept): the fix of the black volumes. 0 = off (A/B).\nhdr_clamp={}\n", self.hdr_clamp as u8));
        // v17: the effect switches at the end
        t.push_str(&format!("\n{}", self.toggles.ini()));
        // The SDR gamma follows the paper white / peak block.
        let pk = format!("peak_nits={}\n", self.peak_nits);
        let at = t.find(&pk).map_or(t.len(), |i| i + pk.len());
        let g = if self.sdr_gamma >= 1.0 { format!("{}", self.sdr_gamma) } else { "srgb".to_string() };
        t.insert_str(
            at,
            &format!(
                "\n# Black level in HDR (docs/hdr10.md): the power the SDR range of the game's picture is decoded with. 2.2 = default, like an SDR\n\
                 # monitor (deep shadows); 2.4 = deeper; srgb = the piecewise sRGB curve Windows uses for SDR content (brighter, greyer blacks). 1.8..2.8.\n\
                 sdr_gamma={g}\n"
            ),
        );
        t
    }

    fn to_ini_base(&self) -> String {
        let hdr = match self.hdr_output {
            HdrOutput::Auto => "auto",
            HdrOutput::On => "on",
            HdrOutput::Off => "off",
        };
        let fmt = match self.hdr_format {
            HdrFormat::Scrgb => "scrgb",
            HdrFormat::Hdr10 => "hdr10",
        };
        format!(
            concat!(
                "# GW2 Enhanced Graphics - configuration (also saved from the overlay)\n",
                "\n",
                "# false = original behaviour: no new float targets, no HDR shaders, no resolve.\n",
                "# (With HDR output active the final-pass conversion stays until the next restart.)\n",
                "enabled={}\n",
                "\n",
                "# HDR output: auto = only when Windows HDR is on and ReShade is absent, on = also with ReShade, off = never.\n",
                "# Read at start (the swapchain is created once).\n",
                "hdr_output={}\n",
                "\n",
                "# HDR format when HDR output is active: scrgb = float16 scRGB (default, tested), hdr10 = EXPERIMENTAL 10-bit PQ / BT.2020 (docs/hdr10.md).\n",
                "# Ignored (with the reason in the overlay status) when ReShade is present or Windows HDR is off. Read at start (restart needed).\n",
                "hdr_format={}\n",
                "\n",
                "# Brightness of UI and paper white in HDR (80..400) and peak brightness of highlights (400..4000), in nits.\n",
                "paper_white_nits={}\n",
                "peak_nits={}\n",
                "\n",
                "# Bloom strength, 1 = as the game (0..2).\n",
                "bloom_strength={}\n",
                "\n",
                "# HDR highlight boost: extra intensity for light sources and bright effects above SDR white, HDR output only (1 = none, 2.5 = default, 4 = max).\n",
                "highlight_boost={}\n",
                "\n",
                "# Ambient occlusion: strength 0..2 (1 = default, subtle; 0 turns the pass off) and radius 0.25..4 (1 = default; relative to depth).
",
                "ao_strength={}
",
                "ao_radius={}
",
                "
",
                "# Light halo: a glow around light sources. Strength 0..2 (1 = default; 0 turns the whole feature off) and radius 0.5..2 (1 = default).
",
                "halo_strength={}
",
                "halo_radius={}
",
                "
",
                "# Reflections: strength 0..2 (0.5 = default, subtle; 0 turns the passes off) and range 0.25..2 (1 = default; how far rays travel).
",
                "ssr_strength={}
",
                "ssr_range={}
",
                "# Reflection materials: ssr_classes is a bitmask of the scene-alpha classes that reflect (1 = low, 2 = mid, 4 = high; 7 = all, the default; use the overlay material debug view to find the class of metal / water). ssr_smart 0..1 (1 = default) scales the built-in vegetation / rough-texture rejection, 0 = off.
",
                "ssr_classes={}
",
                "ssr_smart={}
",
                "# Reflectivity score (the material is detected from the picture; ssr_smart is its master amount): ssr_hue_filter 0..1 (1 = default) is the colour-class requirement: only cool (water, glass, ice) and colourless (polished stone, metal) smooth surfaces reflect, warm / green / unclassified ones (dirt, sand, wood, grass) score 0, and 0 lets every colour be a candidate; ssr_flatness 0..1 (1 = default) rejects surfaces whose normals vary over a wider area.
",
                "ssr_hue_filter={}
",
                "ssr_flatness={}
",
                "# Water and glass (docs/ssr.md): ssr_water_boost 0..1 (0.6 = default; 0 = the old reflections, bit for bit) adds a Schlick Fresnel (F0 0.02 water / 0.04 glass, at most 0.85), a water-colour tint, the sky instead of a black band at the screen edge and a sharper mirror on calm water / a softer one on ripples, only on receivers the score calls water or glass; water_depth_look 0..1 (0.3 = default, 0 = off) darkens such water by up to 15 % where the reflection is strong.
",
                "ssr_water_boost={}
",
                "water_depth_look={}
",
                "# Matte gate (docs/ssr.md): ssr_matte_cut 0..1 (0.8 = default; 0 = the reflectivity score as before). The game has no gloss buffer, so the score judges colour, and moonlight, fog or shade make dry dirt / rock look like water (cool) or polished stone (grey). The gate white-balances the receiver by the frame's mean colour and keeps the score only where it still looks like water / glass / ice; grey \"polished\" floors are scaled by (1 - cut). Raise towards 1 if dry ground still reflects, lower (0.3..0.5) to get polished stone / metal floors back. Needs ssr_smart > 0.
",
                "ssr_matte_cut={}
",
                "
",
                "# Sun shafts (god rays from bright sky seen through gaps): strength 0..2 (0.6 = default; 0 turns the feature off) and length 0.3..1 (0.7 = default).
",
                "shafts_strength={}
",
                "shafts_length={}
",
                "
",
                "# Contact shadows (docs/contact-shadows.md): a short ray march in the depth buffer towards the light darkens the diffuse light right next to\n",
                "# surfaces (feet, rocks, eaves). contact_strength 0..0.8 (0.5 = default, the largest darkening; 0 = off, bit-identical), contact_length 0.25..2\n",
                "# (1 = default, about 0.35 m), contact_sun = auto (the sun shafts' estimate when found, else a fixed direction from above) or fixed.\n",
                "# Never under water or inside the game's own fog, never on lights, the sky or the UI.\n",
                "contact_strength={}\n",
                "contact_length={}\n",
                "contact_sun={}\n",
                "\n",
                "# Aerial perspective (docs/aerial.md): a subtle haze toward the game's own fog / horizon colour on pixels farther than 15 m, never on the sky, lights or\n",
                "# the game's own dense fog, at most 35 % of the haze colour. aerial_strength 0..1 (0.35 = default, 0 = off, bit-identical), aerial_distance 50..5000 m\n",
                "# (400 = default: the distance beyond 15 m where it reaches half of its amount), depth_unit 100..20000 = metres per 1.0 of the engine's NORMALISED\n",
                "# linear depth (1.0 = far plane = sky; 1000 = default, measured on a charr; also the contact shadow length). Old values <= 2 (the inch, 0.0254) = default.\n",
                "aerial_strength={}\n",
                "aerial_distance={}\n",
                "depth_unit={}\n",
                "\n",
                "# Fur look on the CHARR: fur_strength 0..1 (0 = default, off). Which draws are charr is a live-tunable guess, see docs/draw-diagnostic.md:
# fur_ps = comma-separated pixel-shader hashes (8 hex, from a draws-*.log) of the draws that get fur (empty = nothing),
# fur_vs = optional vertex-shader hashes they must also use (empty = any), fur_min_vcb = smallest vertex constant buffer 0 in bytes
# (4096 = skinned meshes only), fur_debug = 1 paints the selected pixels magenta in the screenshots.
# The strands are anchored to the mesh uv: fur_uv = auto (the lowest TEXCOORD with 2 float components) or TEXCOORDn[.xy|.zw] to pick the
# vertex-shader output that carries the base-texture uv (the line status_fur= of the dev capture says uv: ok / none).
",
                "fur_strength={}
",
                "fur_ps={}
",
                "fur_vs={}
",
                "fur_min_vcb={}
",
                "fur_debug={}
",
                "fur_uv={}
",
                "# fur_occlusion 1 (default) = the occlusion check of the fur and rim masks: pixels covered by geometry drawn later (a hood over the head) are erased;\n",
                "# 0 = bypassed, the masks stay as drawn (A/B switch, live). The line status_occlusion= of the dev capture counts what the check erases.\n",
                "fur_occlusion={}\n",
                "\n",
                "# Fur look (docs/fur-look.md), how the fur looks where fur_strength switches it on (the pixels of the fur mask only; all live):\n",
                "# fur_amp 0..1 (0.32 = default) brightness of the strands, fur_scale 0.25..4 (1 = default) multiplies the strand cell size, fur_length 1..12 (5 = default) strand length\n",
                "# relative to its width, fur_detail 0..2 (1 = default) gain of the texture detail, fur_rim 0..2 (0.25 = default, subtle; 1 = the strong outline the fur had before; 0 = no rim\n",
                "# glow and no feather of the outline), fur_sss 0..2 (1 = default) warm glow of the mid and dark fur.\n",
                "# fur_view 0..4 is a diagnostic painted on the fur mask pixels only (0 = off, 1 = the mesh uv as colour and solid red where a pixel has no uv, 2 = the strand pattern as grey,\n",
                "# 3 = the rim band, 4 = the strand fade); it works with fur_strength 0, hot reload applies it, and it is NEVER saved: always written as 0, ignored at start.\n",
                "fur_amp={}\n",
                "fur_scale={}\n",
                "fur_length={}\n",
                "fur_detail={}\n",
                "fur_rim={}\n",
                "fur_sss={}\n",
                "fur_view=0\n",
                "\n",
                "# Rim light and skin glow on CHARACTERS (docs/rim.md): rim_strength 0..1 (0.4 = default; 0 = off, bit-identical, no extra masking) is the largest extra luminance share of a soft rim along the silhouette (never above +25 %), tinted a little towards the bright background; rim_width 1..8 pixels; subsurface_strength 0..1 (0.3) a gentle warm glow of the thin, dim-lit parts (at most +8 %).
",
                "# rim_ps = pixel-shader hashes of the character draws (default: the verified charr skin shaders; empty = off), rim_min_vcb = smallest vertex constant buffer 0 in bytes (4096 = skinned meshes), rim_debug = 1 paints the selected pixels cyan. Never in the dark, under water or in the game's own fog.
",
                "rim_strength={}
",
                "rim_width={}
",
                "subsurface_strength={}
",
                "rim_ps={}
",
                "rim_min_vcb={}
",
                "rim_debug={}
",
                "
",
                "# Texture filtering (docs/aa-aniso.md): aniso_level 0..16 forces anisotropic filtering on the game's trilinear world samplers (16 = default, 0 or 1 = off, the game's own); mip_bias -1..0.5 is added to their mip level (0 = none, -0.3 = a little sharper). UI and post-process samplers are never touched.\n",
                "aniso_level={}\n",
                "mip_bias={}\n",
                "\n",
                "# Anti-aliasing (docs/aa-aniso.md), on the 3D scene before the UI: smaa=1 adds an HDR-aware SMAA 1x pass (medium quality, no temporal), smaa_threshold 0.02..0.5 (0.1 = default, lower = more edges), sharpen_strength 0..1 (0.3 = default, 0 = off) is a contrast adaptive sharpen after it.\n",
                "smaa={}\n",
                "smaa_threshold={}\n",
                "sharpen_strength={}\n",
                "\n",
                "# Exposure (docs/exposure.md), on the 3D scene before the UI: autoexp_strength 0..1 (0.5 = default, 0 = off) is eye adaptation towards a mid-grey,\n",
                "# autoexp_target 0.05..0.6 (0.2 = default; the linear log-average luminance it aims at, 1 = SDR white), autoexp_range 0..2 EV (0.7 = default; the biggest\n",
                "# change either way, brightening is also capped at +1 EV), adapt_up / adapt_down 0.05..20 s (0.6 / 1.8 = default; time constants when the scene gets\n",
                "# brighter / darker). localtone_strength 0..1 (0.2 = default, 0 = off) compresses big bright areas (sand, snow, white fur) and keeps their detail.\n",
                "autoexp_strength={}\n",
                "autoexp_target={}\n",
                "autoexp_range={}\n",
                "adapt_up={}\n",
                "adapt_down={}\n",
                "localtone_strength={}\n",
                "\n",
                "# Lens effects (docs/lens.md), on the 3D scene before the UI, HDR output only: horizontal streaks and a soft glare from HDR highlights, and\n",
                "# subtle ghost flares for VERY bright point sources only. lens_strength 0..1 (0.35 = default, 0 = off, bit-identical, no passes),\n",
                "# lens_threshold 1.2..8 (2.5 = default: energy above this many times paper white of the scene before the highlight boost; white sand never\n",
                "# triggers), lens_ghost 0..0.5 (0.15 = default, 0 = no ghosts), lens_streak_length 0.25..2 (1 = default). Never saturated blue, adds at most 0.6 of paper white.\n",
                "lens_strength={}\n",
                "lens_threshold={}\n",
                "lens_ghost={}\n",
                "lens_streak_length={}\n",
                "\n",
                "# Colour grading (docs/grading.md), on the 3D scene before the UI (never the UI), all neutral by default (= off, bit-identical).\n",
                "# grade_preset = Original | Warm cinematic | Cool | Vivid | Soft (applied at load when set; the keys below override it).\n",
                "# grade_saturation 0..2, grade_contrast 0.5..1.5 (pivot = mid-grey 0.18), grade_temperature -1..1 (warm +), grade_tint -1..1 (magenta +),\n",
                "# grade_vibrance 0..1 (boosts the dull colours only), grade_shadows / grade_highlights -0.5..0.5.\n",
                "# grade_lut = a .cube 3D LUT (size 17..65) inside the GW2Graphics folder, e.g. luts/mine.cube (empty = none); it is read as a log2 shaper LUT between\n",
                "# 2^-8 and 2^6 of linear light (see the doc); grade_lut_strength 0..1 (1 = default). A missing or bad file leaves the LUT off.\n",
                "grade_preset={}\n",
                "grade_saturation={}\n",
                "grade_contrast={}\n",
                "grade_temperature={}\n",
                "grade_tint={}\n",
                "grade_vibrance={}\n",
                "grade_shadows={}\n",
                "grade_highlights={}\n",
                "grade_lut={}\n",
                "grade_lut_strength={}\n",
                "\n",
                "# Overlay hotkey: modifiers (Shift, Ctrl, Alt) + key (End, Insert, F1..F24, A..Z, 0..9).\n",
                "overlay_key={}\n",
                "\n",
                "# Dev feedback flow (docs/dev-flow.md), all off by default: a screenshot into GW2Graphics\\dev every N seconds (0 = off, max 3600),\n",
                "# as a pair (mod + vanilla comparison) when dev_compare=1, and re-reading this file live when it changes.\n",
                "dev_capture_seconds={}\n",
                "dev_compare={}\n",
                "dev_hot_reload={}\n",
                "\n",
                "# Debug view 0..5 (docs/dev-flow.md): 0 = off, 1 = boost / occlusion, 2 = fill, 3 = material classes, 4 = sun, 5 = reflectivity heat map. The same as the debug switches of the\n",
                "# overlay, so editing this line with dev_hot_reload=1 switches the view in the running game. NEVER saved: always written as 0, ignored at start.\n",
                "debug_view=0\n",
                "\n",
                "# Photo mode (docs/photo.md): the hotkey (same syntax as overlay_key) saves the current frame, full resolution, with the mod's effects, as PNG +\n",
                "# Radiance .hdr + settings .txt into GW2Graphics\\photos. photo_hide_ui=1 saves the 3D scene without the UI when the pre-UI copy is available.\n",
                "photo_key={}\n",
                "photo_hide_ui={}\n",
            ),
            self.enabled, hdr, fmt, self.paper_white_nits, self.peak_nits, self.bloom_strength, self.highlight_boost, self.ao_strength, self.ao_radius, self.halo_strength, self.halo_radius, self.ssr_strength, self.ssr_range, self.ssr_classes, self.ssr_smart, self.ssr_hue_filter, self.ssr_flatness, self.ssr_water_boost, self.water_depth_look, self.ssr_matte_cut, self.shafts_strength, self.shafts_length, self.contact_strength, self.contact_length, self.contact_sun, self.aerial_strength, self.aerial_distance, self.depth_unit, self.fur_strength, self.fur_ps, self.fur_vs, self.fur_min_vcb, self.fur_debug as u8, self.fur_uv, self.fur_occlusion, self.fur_amp, self.fur_scale, self.fur_length, self.fur_detail, self.fur_rim, self.fur_sss, self.rim_strength, self.rim_width, self.subsurface_strength, self.rim_ps, self.rim_min_vcb, self.rim_debug as u8, self.aniso_level, self.mip_bias, self.smaa as u8, self.smaa_threshold, self.sharpen_strength, self.autoexp_strength, self.autoexp_target, self.autoexp_range, self.adapt_up, self.adapt_down, self.localtone_strength, self.lens_strength, self.lens_threshold, self.lens_ghost, self.lens_streak_length, self.grade_preset, self.grade_saturation, self.grade_contrast, self.grade_temperature, self.grade_tint, self.grade_vibrance, self.grade_shadows, self.grade_highlights, self.grade_lut, self.grade_lut_strength, self.overlay_key, self.dev_capture_seconds, self.dev_compare as u8, self.dev_hot_reload as u8, self.photo_key, self.photo_hide_ui as u8
        )
    }

    /// Writes `to_ini` to `<path>.tmp` and renames it over `path`, so a crash never leaves half a file.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = std::path::PathBuf::from(tmp);
        std::fs::write(&tmp, self.to_ini())?;
        std::fs::rename(&tmp, path).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })
    }

    pub fn load(path: &Path) -> Config {
        std::fs::read_to_string(path).map(|t| Config::parse(&t)).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_when_empty() {
        let c = Config::parse("");
        assert!(c.enabled);
        assert_eq!(c.hdr_output, HdrOutput::Auto);
        assert_eq!(c.paper_white_nits, 203.0);
        assert_eq!(c.peak_nits, 1000.0);
        assert_eq!(c.bloom_strength, 1.0);
        assert_eq!(c.overlay_key, "Shift+End");
    }

    #[test]
    fn photo_and_hdr10_keys() {
        let d = Config::default();
        assert_eq!(d.hdr_format, HdrFormat::Scrgb, "HDR10 is off by default");
        assert_eq!((d.photo_key.as_str(), d.photo_hide_ui, d.calibration_pattern), ("Ctrl+Shift+End", false, 0));
        let c = Config::parse("hdr_format=HDR10\nphoto_key=Ctrl+F10\nphoto_hide_ui=1\ncalibration_pattern=2");
        assert_eq!((c.hdr_format, c.photo_key.as_str(), c.photo_hide_ui, c.calibration_pattern), (HdrFormat::Hdr10, "Ctrl+F10", true, 2));
        assert_eq!(Config::parse("hdr_format=whatever").hdr_format, HdrFormat::Scrgb);
        assert_eq!(Config::parse("hdr_format=SCRGB").hdr_format, HdrFormat::Scrgb);
        assert_eq!(Config::parse("calibration_pattern=9").calibration_pattern, 3);
        assert_eq!(Config::parse("calibration_pattern=abc").calibration_pattern, 0);
        assert_eq!(Config::parse("photo_key=").photo_key, "Ctrl+Shift+End", "an empty value keeps the default");
        // written to config.ini with comments, except the runtime-only calibration flag
        let t = Config { hdr_format: HdrFormat::Hdr10, photo_hide_ui: true, calibration_pattern: 3, ..Config::default() }.to_ini();
        assert!(t.contains("hdr_format=hdr10") && t.contains("photo_key=Ctrl+Shift+End") && t.contains("photo_hide_ui=1"), "{t}");
        assert!(!t.contains("calibration_pattern"), "runtime only, never saved");
        assert!(t.contains("docs/photo.md") && t.contains("docs/hdr10.md"));
        assert_eq!(Config::parse(&t).calibration_pattern, 0);
        assert_eq!(Config::parse(&Config::default().to_ini()), Config::default());
    }

    #[test]
    fn parses_hdr_output_on() {
        assert_eq!(Config::parse("hdr_output=on").hdr_output, HdrOutput::On);
        assert_eq!(Config::parse("hdr_output = ON").hdr_output, HdrOutput::On);
        assert_eq!(Config::parse("hdr_output=Auto").hdr_output, HdrOutput::Auto);
        assert_eq!(Config::parse("hdr_output=OFF").hdr_output, HdrOutput::Off);
        assert_eq!(Config::parse("hdr_output=maybe").hdr_output, HdrOutput::Auto);
    }

    #[test]
    fn parses_values() {
        assert_eq!(Config::parse("paper_white_nits=300").paper_white_nits, 300.0);
    }

    #[test]
    fn clamps_out_of_range() {
        let c = Config::parse("bloom_strength=9\npeak_nits=10");
        assert_eq!(c.bloom_strength, 2.0);
        assert_eq!(c.peak_nits, 400.0);
    }

    #[test]
    fn highlight_boost_defaults_and_clamps() {
        assert_eq!(Config::default().highlight_boost, 2.5);
        assert_eq!(Config::parse("highlight_boost=9").highlight_boost, 4.0);
        assert_eq!(Config::parse("highlight_boost=0").highlight_boost, 1.0);
    }

    #[test]
    fn ao_defaults_parse_and_clamp() {
        let d = Config::default();
        assert_eq!((d.ao_strength, d.ao_radius), (1.0, 1.0));
        assert_eq!(Config::parse("").ao_strength, 1.0);
        let c = Config::parse("ao_strength=1.5
ao_radius=3");
        assert_eq!((c.ao_strength, c.ao_radius), (1.5, 3.0));
        let c = Config::parse("ao_strength=9
ao_radius=0.01");
        assert_eq!((c.ao_strength, c.ao_radius), (2.0, 0.25));
        let c = Config::parse("ao_strength=-3
ao_radius=99");
        assert_eq!((c.ao_strength, c.ao_radius), (0.0, 4.0));
        let c = Config::parse("ao_strength=abc
ao_radius=nan");
        assert_eq!((c.ao_strength, c.ao_radius), (1.0, 1.0));
        assert_eq!(Config::parse("AO_Strength = 0").ao_strength, 0.0);
    }

    #[test]
    fn ao_keys_are_written_to_the_ini() {
        let t = Config { ao_strength: 0.5, ao_radius: 2.0, ..Config::default() }.to_ini();
        assert!(t.contains("ao_strength=0.5
") && t.contains("ao_radius=2
"), "{t}");
    }

    #[test]
    fn shafts_defaults_parse_and_clamp() {
        let d = Config::default();
        assert_eq!((d.shafts_strength, d.shafts_length), (0.6, 0.7));
        let c = Config::parse("shafts_strength=1.5\nshafts_length=0.4");
        assert_eq!((c.shafts_strength, c.shafts_length), (1.5, 0.4));
        let c = Config::parse("shafts_strength=9\nshafts_length=0.01");
        assert_eq!((c.shafts_strength, c.shafts_length), (2.0, 0.3));
        let c = Config::parse("shafts_strength=-3\nshafts_length=99");
        assert_eq!((c.shafts_strength, c.shafts_length), (0.0, 1.0));
        let c = Config::parse("shafts_strength=abc\nshafts_length=nan");
        assert_eq!((c.shafts_strength, c.shafts_length), (0.6, 0.7));
        assert_eq!(Config::parse("Shafts_Strength = 0").shafts_strength, 0.0);
        let t = Config { shafts_strength: 0.25, shafts_length: 0.9, ..Config::default() }.to_ini();
        assert!(t.contains("shafts_strength=0.25") && t.contains("shafts_length=0.9"), "{t}");
    }

    #[test]
    fn contact_and_aerial_defaults_parse_clamp_and_write() {
        let d = Config::default();
        assert_eq!((d.contact_strength, d.contact_length, d.contact_sun.as_str()), (0.5, 1.0, "auto"));
        // depth_unit is metres per 1.0 of the normalised depth now (it was metres per inch-unit, 0.001..2, default 0.0254).
        assert_eq!((d.aerial_strength, d.aerial_distance, d.depth_unit), (0.35, 400.0, 1000.0));
        let c = Config::parse("contact_strength=0.8\ncontact_length=1.5\ncontact_sun=FIXED\naerial_strength=0.6\naerial_distance=900\ndepth_unit=2500");
        assert_eq!((c.contact_strength, c.contact_length, c.contact_sun.as_str()), (0.8, 1.5, "fixed"));
        assert_eq!((c.aerial_strength, c.aerial_distance, c.depth_unit), (0.6, 900.0, 2500.0));
        let c = Config::parse("contact_strength=9\ncontact_length=0.01\ncontact_sun=moon\naerial_strength=9\naerial_distance=1\ndepth_unit=50");
        assert_eq!((c.contact_strength, c.contact_length, c.contact_sun.as_str()), (0.8, 0.25, "auto"));
        assert_eq!((c.aerial_strength, c.aerial_distance, c.depth_unit), (1.0, 50.0, 100.0));
        let c = Config::parse("contact_strength=-3\ncontact_length=99\naerial_strength=-1\naerial_distance=1e9\ndepth_unit=1e9");
        assert_eq!((c.contact_strength, c.contact_length, c.aerial_strength, c.aerial_distance, c.depth_unit), (0.0, 2.0, 0.0, 5000.0, 20000.0));
        let c = Config::parse("contact_strength=abc\ncontact_length=nan\naerial_strength=nan\naerial_distance=x\ndepth_unit=inf");
        assert_eq!((c.contact_strength, c.contact_length, c.aerial_strength, c.aerial_distance, c.depth_unit), (0.5, 1.0, 0.35, 400.0, 1000.0));
        // Migration: an old config (the inch default 0.0254, or any old value up to 2) reads as unset = the new default.
        for old in ["0.0254", "1", "2", "0", "0.001", "-1", "x"] {
            assert_eq!(Config::parse(&format!("depth_unit={old}")).depth_unit, 1000.0, "{old}");
        }
        assert!(Config::parse("depth_unit=0.0254").to_ini().contains("depth_unit=1000\n"));
        assert_eq!(Config::parse("Contact_Strength = 0").contact_strength, 0.0);
        let t = Config { contact_strength: 0.25, contact_sun: "fixed".into(), aerial_strength: 0.2, ..Config::default() }.to_ini();
        assert!(t.contains("contact_strength=0.25") && t.contains("contact_sun=fixed") && t.contains("aerial_strength=0.2"), "{t}");
        // The setters' ranges and defaults are the config's.
        assert_eq!(Config::parse("contact_strength=9").contact_strength, crate::contact::STRENGTH_RANGE.1);
        assert_eq!(Config::parse("contact_length=0").contact_length, crate::contact::LENGTH_RANGE.0);
        assert_eq!((Config::parse("aerial_strength=9").aerial_strength, Config::parse("aerial_distance=1").aerial_distance), (crate::aerial::STRENGTH_RANGE.1, crate::aerial::DISTANCE_RANGE.0));
        assert_eq!((d.contact_strength, d.contact_length), (crate::contact::DEFAULT_STRENGTH, crate::contact::DEFAULT_LENGTH));
        assert_eq!((d.aerial_strength, d.aerial_distance, d.depth_unit), (crate::aerial::DEFAULT_STRENGTH, crate::aerial::DEFAULT_DISTANCE, crate::aerial::DEFAULT_UNIT));
    }

    #[test]
    fn fur_strength_defaults_parse_and_clamp() {
        assert_eq!(Config::default().fur_strength, 0.0);
        assert_eq!(Config::parse("").fur_strength, 0.0);
        assert_eq!(Config::parse("fur_strength=0.8").fur_strength, 0.8);
        assert_eq!(Config::parse("fur_strength=9").fur_strength, 1.0);
        assert_eq!(Config::parse("fur_strength=-3").fur_strength, 0.0);
        assert_eq!(Config::parse("fur_strength=abc").fur_strength, 0.0);
        assert_eq!(Config::parse("fur_strength=nan").fur_strength, 0.0);
        assert_eq!(Config::parse("Fur_Strength = 0").fur_strength, 0.0);
        let t = Config { fur_strength: 0.5, ..Config::default() }.to_ini();
        assert!(t.contains("fur_strength=0.5
"), "{t}");
        // The overlay slider range and default are the config's.
        assert_eq!(Config::parse("fur_strength=9").fur_strength, crate::datapath::FUR_RANGE.1);
        assert_eq!(Config::parse("fur_strength=-9").fur_strength, crate::datapath::FUR_RANGE.0);
        assert_eq!(Config::default().fur_strength, crate::datapath::FUR_DEFAULT);
    }

    #[test]
    fn ssr_defaults_parse_clamp_and_write() {
        let d = Config::default();
        assert_eq!((d.ssr_strength, d.ssr_range), (0.5, 1.0));
        assert_eq!(Config::parse("").ssr_strength, 0.5);
        let c = Config::parse("ssr_strength=1.5
ssr_range=1.75");
        assert_eq!((c.ssr_strength, c.ssr_range), (1.5, 1.75));
        let c = Config::parse("ssr_strength=9
ssr_range=0.01");
        assert_eq!((c.ssr_strength, c.ssr_range), (2.0, 0.25));
        let c = Config::parse("ssr_strength=-3
ssr_range=99");
        assert_eq!((c.ssr_strength, c.ssr_range), (0.0, 2.0));
        let c = Config::parse("ssr_strength=abc
ssr_range=nan");
        assert_eq!((c.ssr_strength, c.ssr_range), (0.5, 1.0));
        assert_eq!(Config::parse("SSR_Strength = 0").ssr_strength, 0.0);
        let t = Config { ssr_strength: 0.25, ssr_range: 2.0, ..Config::default() }.to_ini();
        assert!(t.contains("ssr_strength=0.25") && t.contains("ssr_range=2"), "{t}");
    }

    #[test]
    fn halo_defaults_parse_and_clamp() {
        let d = Config::default();
        assert_eq!((d.halo_strength, d.halo_radius), (1.0, 1.0));
        assert_eq!(Config::parse("").halo_strength, 1.0);
        let c = Config::parse("halo_strength=1.5
halo_radius=1.75");
        assert_eq!((c.halo_strength, c.halo_radius), (1.5, 1.75));
        let c = Config::parse("halo_strength=9
halo_radius=0.01");
        assert_eq!((c.halo_strength, c.halo_radius), (2.0, 0.5));
        let c = Config::parse("halo_strength=-3
halo_radius=99");
        assert_eq!((c.halo_strength, c.halo_radius), (0.0, 2.0));
        let c = Config::parse("halo_strength=abc
halo_radius=nan");
        assert_eq!((c.halo_strength, c.halo_radius), (1.0, 1.0));
        assert_eq!(Config::parse("Halo_Strength = 0").halo_strength, 0.0);
    }

    #[test]
    fn halo_keys_are_written_to_the_ini() {
        let t = Config { halo_strength: 0.5, halo_radius: 2.0, ..Config::default() }.to_ini();
        assert!(t.contains("halo_strength=0.5
") && t.contains("halo_radius=2
"), "{t}");
    }

    #[test]
    fn ssr_material_keys_parse_clamp_and_write() {
        let d = Config::default();
        assert_eq!((d.ssr_classes, d.ssr_smart), (7, 1.0));
        assert_eq!(Config::parse("").ssr_classes, 7);
        let c = Config::parse("ssr_classes=5\nssr_smart=0.4");
        assert_eq!((c.ssr_classes, c.ssr_smart), (5, 0.4));
        let c = Config::parse("ssr_classes=99\nssr_smart=7");
        assert_eq!((c.ssr_classes, c.ssr_smart), (7, 1.0));
        let c = Config::parse("ssr_classes=-4\nssr_smart=-1");
        assert_eq!((c.ssr_classes, c.ssr_smart), (0, 0.0));
        let c = Config::parse("ssr_classes=abc\nssr_smart=nan");
        assert_eq!((c.ssr_classes, c.ssr_smart), (7, 1.0));
        assert_eq!(Config::parse("ssr_classes=2.6").ssr_classes, 3);
        let t = Config { ssr_classes: 6, ssr_smart: 0.5, ..Config::default() }.to_ini();
        assert!(t.contains("ssr_classes=6") && t.contains("ssr_smart=0.5"), "{t}");
    }

    #[test]
    fn ssr_reflectivity_keys_parse_clamp_and_write() {
        let d = Config::default();
        assert_eq!((d.ssr_hue_filter, d.ssr_flatness), (1.0, 1.0));
        assert_eq!(Config::parse("").ssr_hue_filter, 1.0);
        let c = Config::parse("ssr_hue_filter=0.4
ssr_flatness=0.7");
        assert_eq!((c.ssr_hue_filter, c.ssr_flatness), (0.4, 0.7));
        let c = Config::parse("ssr_hue_filter=9
ssr_flatness=-2");
        assert_eq!((c.ssr_hue_filter, c.ssr_flatness), (1.0, 0.0));
        let c = Config::parse("ssr_hue_filter=abc
ssr_flatness=nan");
        assert_eq!((c.ssr_hue_filter, c.ssr_flatness), (1.0, 1.0));
        let t = Config { ssr_hue_filter: 0.5, ssr_flatness: 0.25, ..Config::default() }.to_ini();
        assert!(t.contains("ssr_hue_filter=0.5") && t.contains("ssr_flatness=0.25"), "{t}");
        assert_eq!(Config::parse(&t).ssr_flatness, 0.25);
    }

    #[test]
    fn fur_selector_keys_default_parse_and_roundtrip() {
        let d = Config::default();
        assert_eq!((d.fur_ps.as_str(), d.fur_vs.as_str(), d.fur_min_vcb, d.fur_debug), ("", "", 4096, false));
        let c = Config::parse("fur_ps = 535B203B, 1c8a2482 
fur_vs=130e17de
fur_min_vcb=16000
fur_debug=1");
        assert_eq!((c.fur_ps.as_str(), c.fur_vs.as_str(), c.fur_min_vcb, c.fur_debug), ("535b203b, 1c8a2482", "130e17de", 16000, true));
        // Garbage falls back to the defaults, the range is clamped.
        let c = Config::parse("fur_min_vcb=abc
fur_debug=maybe");
        assert_eq!((c.fur_min_vcb, c.fur_debug), (4096, false));
        assert_eq!(Config::parse("fur_min_vcb=99999999").fur_min_vcb, 1_048_576);
        assert_eq!(Config::parse("fur_min_vcb=-5").fur_min_vcb, 0);
        assert!(Config::parse(&format!("fur_ps={}", "a".repeat(2000))).fur_ps.len() <= 400);
        let t = Config { fur_ps: "535b203b".into(), fur_vs: "0d12c4bb".into(), fur_min_vcb: 8192, fur_debug: true, ..Config::default() }.to_ini();
        for k in ["fur_ps=535b203b", "fur_vs=0d12c4bb", "fur_min_vcb=8192", "fur_debug=1"] {
            assert!(t.contains(k), "{k} missing in {t}");
        }
        let back = Config::parse(&t);
        assert_eq!((back.fur_ps.as_str(), back.fur_vs.as_str(), back.fur_min_vcb, back.fur_debug), ("535b203b", "0d12c4bb", 8192, true));
    }

    #[test]
    fn ssr_matte_cut_default_parse_clamp_and_roundtrip() {
        assert_eq!(Config::default().ssr_matte_cut, 0.8);
        assert_eq!(Config::parse("").ssr_matte_cut, 0.8);
        assert_eq!(Config::parse("ssr_matte_cut=0.35").ssr_matte_cut, 0.35);
        assert_eq!(Config::parse("ssr_matte_cut=7").ssr_matte_cut, 1.0);
        assert_eq!(Config::parse("ssr_matte_cut=-1").ssr_matte_cut, 0.0);
        assert_eq!(Config::parse("ssr_matte_cut=nan").ssr_matte_cut, 0.8);
        assert_eq!(Config::parse("ssr_matte_cut=abc").ssr_matte_cut, 0.8);
        let t = Config::default().to_ini();
        assert!(t.contains("ssr_matte_cut=0.8") && t.contains("# Matte gate"), "{t}");
        for v in [0.0f32, 0.25, 1.0] {
            let c = Config { ssr_matte_cut: v, ..Config::default() };
            assert_eq!(Config::parse(&c.to_ini()).ssr_matte_cut, v);
        }
    }

    #[test]
    fn water_and_rim_keys_default_parse_clamp_and_roundtrip() {
        let d = Config::default();
        assert_eq!((d.ssr_water_boost, d.water_depth_look), (0.6, 0.3));
        assert_eq!((d.rim_strength, d.rim_width, d.subsurface_strength, d.rim_min_vcb, d.rim_debug), (0.4, 3.0, 0.3, 4096, false));
        assert_eq!(d.rim_ps, crate::furpick::CHARR_PRESET, "the default rim list is the verified charr skin");
        let c = Config::parse("ssr_water_boost=9
water_depth_look=-1
rim_strength=7
rim_width=99
subsurface_strength=nan");
        assert_eq!((c.ssr_water_boost, c.water_depth_look, c.rim_strength, c.rim_width, c.subsurface_strength), (1.0, 0.0, 1.0, 8.0, 0.3));
        assert_eq!(Config::parse("rim_width=0").rim_width, 1.0);
        assert_eq!(Config::parse("rim_ps=").rim_ps, "", "an explicit empty list switches the rim off");
        assert_eq!(Config::parse("rim_ps = AABBCCDD , 1d50763f").rim_ps, "aabbccdd , 1d50763f");
        assert_eq!(Config::parse("rim_min_vcb=-4").rim_min_vcb, 0);
        let t = d.to_ini();
        for k in ["ssr_water_boost=0.6", "water_depth_look=0.3", "rim_strength=0.4", "rim_width=3", "subsurface_strength=0.3", "rim_ps=1d50763f,e0189185,5d00827f", "rim_min_vcb=4096", "rim_debug=0"] {
            assert!(t.contains(k), "{k} missing: {t}");
        }
        let c2 = Config { rim_strength: 0.0, rim_width: 6.0, rim_ps: String::new(), rim_debug: true, ssr_water_boost: 0.0, ..Config::default() };
        let back = Config::parse(&c2.to_ini());
        assert_eq!((back.rim_strength, back.rim_width, back.rim_ps.as_str(), back.rim_debug, back.ssr_water_boost), (0.0, 6.0, "", true, 0.0));
    }

    #[test]
    fn fur_uv_defaults_to_auto_parses_and_roundtrips() {
        assert_eq!(Config::default().fur_uv, "auto");
        assert_eq!(Config::parse("").fur_uv, "auto");
        assert_eq!(Config::parse("fur_uv=").fur_uv, "auto");
        assert_eq!(Config::parse("fur_uv = TEXCOORD1.zw").fur_uv, "texcoord1.zw");
        let t = Config { fur_uv: "texcoord2".into(), ..Config::default() }.to_ini();
        assert!(t.contains("fur_uv=texcoord2"), "{t}");
        assert_eq!(Config::parse(&t).fur_uv, "texcoord2");
        assert!(Config::default().to_ini().contains("fur_uv=auto"));
    }

    #[test]
    fn fur_look_keys_default_parse_clamp_and_roundtrip() {
        use crate::datapath as dp;
        let d = Config::default();
        assert_eq!((d.fur_amp, d.fur_scale, d.fur_length, d.fur_detail, d.fur_rim, d.fur_sss, d.fur_view), (0.32, 1.0, 5.0, 1.0, 0.25, 1.0, 0));
        assert_eq!(Config::parse(""), d);
        // The values the shader used to have compiled in, and the documented defaults, are the module's constants.
        assert_eq!((dp::FUR_AMP_DEFAULT, dp::FUR_SCALE_DEFAULT, dp::FUR_LENGTH_DEFAULT, dp::FUR_DETAIL_DEFAULT, dp::FUR_RIM_DEFAULT, dp::FUR_SSS_DEFAULT), (0.32, 1.0, 5.0, 1.0, 0.25, 1.0));
        let c = Config::parse("fur_amp=0.5\nfur_scale=2\nfur_length=8\nfur_detail=1.5\nfur_rim=1\nfur_sss=0.5\nfur_view=3");
        assert_eq!((c.fur_amp, c.fur_scale, c.fur_length, c.fur_detail, c.fur_rim, c.fur_sss, c.fur_view), (0.5, 2.0, 8.0, 1.5, 1.0, 0.5, 3));
        // Out of range: clamped to the ends.
        let c = Config::parse("fur_amp=9\nfur_scale=99\nfur_length=99\nfur_detail=9\nfur_rim=9\nfur_sss=9\nfur_view=9");
        assert_eq!((c.fur_amp, c.fur_scale, c.fur_length, c.fur_detail, c.fur_rim, c.fur_sss, c.fur_view), (1.0, 4.0, 12.0, 2.0, 2.0, 2.0, 4));
        let c = Config::parse("fur_amp=-3\nfur_scale=0\nfur_length=0\nfur_detail=-1\nfur_rim=-1\nfur_sss=-1\nfur_view=-5");
        assert_eq!((c.fur_amp, c.fur_scale, c.fur_length, c.fur_detail, c.fur_rim, c.fur_sss, c.fur_view), (0.0, 0.25, 1.0, 0.0, 0.0, 0.0, 0));
        // Garbage, NaN and infinity fall back to the defaults.
        let c = Config::parse("fur_amp=abc\nfur_scale=nan\nfur_length=inf\nfur_detail=-inf\nfur_rim=\nfur_sss=1,5\nfur_view=x");
        assert_eq!(c, d);
        let c = Config::parse("fur_amp=NaN\nfur_rim=NaN\nfur_view=NaN");
        assert_eq!((c.fur_amp, c.fur_rim, c.fur_view), (0.32, 0.25, 0));
        // The view is an integer: rounded.
        assert_eq!(Config::parse("fur_view=2.6").fur_view, 3);
        assert_eq!(Config::parse("Fur_Rim = 0").fur_rim, 0.0);
        // Written with a comment, and read back exactly.
        let c = Config { fur_amp: 0.45, fur_scale: 1.5, fur_length: 8.0, fur_detail: 1.25, fur_rim: 0.75, fur_sss: 0.5, ..Config::default() };
        let t = c.to_ini();
        for k in ["fur_amp=0.45\n", "fur_scale=1.5\n", "fur_length=8\n", "fur_detail=1.25\n", "fur_rim=0.75\n", "fur_sss=0.5\n", "docs/fur-look.md"] {
            assert!(t.contains(k), "{k} missing in {t}");
        }
        assert_eq!(Config::parse(&t), c);
        let t = Config::default().to_ini();
        for k in ["fur_amp=0.32\n", "fur_scale=1\n", "fur_length=5\n", "fur_detail=1\n", "fur_rim=0.25\n", "fur_sss=1\n"] {
            assert!(t.contains(k), "{k} missing in {t}");
        }
        // The overlay slider ranges are the parser's.
        for (key, r) in [("fur_amp", dp::FUR_AMP_RANGE), ("fur_scale", dp::FUR_SCALE_RANGE), ("fur_length", dp::FUR_LENGTH_RANGE), ("fur_detail", dp::FUR_DETAIL_RANGE), ("fur_rim", dp::FUR_RIM_RANGE), ("fur_sss", dp::FUR_SSS_RANGE)] {
            let lo = Config::parse(&format!("{key}=-1000"));
            let hi = Config::parse(&format!("{key}=1000"));
            let get = |c: &Config| match key {
                "fur_amp" => c.fur_amp,
                "fur_scale" => c.fur_scale,
                "fur_length" => c.fur_length,
                "fur_detail" => c.fur_detail,
                "fur_rim" => c.fur_rim,
                _ => c.fur_sss,
            };
            assert_eq!((get(&lo), get(&hi)), r, "{key}");
        }
        // The helper hands the same numbers to the data path.
        let l = c.fur_look();
        assert_eq!((l.amp, l.scale, l.length, l.detail, l.rim, l.sss, l.view), (0.45, 1.5, 8.0, 1.25, 0.75, 0.5, 0));
        assert_eq!(Config::default().fur_look(), dp::FurLook::default());
    }

    #[test]
    fn fur_occlusion_parses_clamps_and_round_trips() {
        assert_eq!(Config::default().fur_occlusion, 1, "the check is on by default");
        assert_eq!(Config::parse("fur_occlusion=0").fur_occlusion, 0);
        assert_eq!(Config::parse("fur_occlusion=1").fur_occlusion, 1);
        assert_eq!(Config::parse("fur_occlusion=7").fur_occlusion, 1, "clamped");
        assert_eq!(Config::parse("fur_occlusion=-3").fur_occlusion, 0, "clamped");
        assert_eq!(Config::parse("fur_occlusion=0.4").fur_occlusion, 0, "an integer");
        for bad in ["abc", "nan", "NaN", "inf", "", "1,0"] {
            assert_eq!(Config::parse(&format!("fur_occlusion={bad}")).fur_occlusion, 1, "{bad}: the default");
        }
        // Unlike the debug views, the current value is written (0 too), with its comment, and reads back.
        for v in [0u8, 1] {
            let t = Config { fur_occlusion: v, ..Config::default() }.to_ini();
            let lines: Vec<&str> = t.lines().filter(|l| l.starts_with("fur_occlusion=")).collect();
            assert_eq!(lines, vec![format!("fur_occlusion={v}").as_str()], "{t}");
            assert!(t.contains("# fur_occlusion 1 (default)"));
            assert_eq!(Config::parse(&t).fur_occlusion, v);
        }
    }

    #[test]
    fn debug_view_and_fur_view_are_read_but_never_written() {
        let d = Config::default();
        assert_eq!((d.debug_view, d.fur_view), (0, 0));
        assert_eq!(Config::parse("debug_view=5").debug_view, 5);
        assert_eq!(Config::parse("debug_view=3").debug_view, 3);
        assert_eq!(Config::parse("debug_view=9").debug_view, 5, "clamped");
        assert_eq!(Config::parse("debug_view=-2").debug_view, 0);
        assert_eq!(Config::parse("debug_view=2.6").debug_view, 3, "an integer");
        for bad in ["abc", "nan", "inf", ""] {
            assert_eq!(Config::parse(&format!("debug_view={bad}")).debug_view, 0, "{bad}");
        }
        assert_eq!(Config::parse("fur_view=4").fur_view, 4);
        // Whatever the config holds, the file says 0: a debug tint must never persist by accident.
        let t = Config { debug_view: 5, fur_view: 4, ..Config::default() }.to_ini();
        let lines: Vec<&str> = t.lines().collect();
        assert_eq!(lines.iter().filter(|l| l.starts_with("debug_view=")).collect::<Vec<_>>(), vec![&"debug_view=0"], "{t}");
        assert_eq!(lines.iter().filter(|l| l.starts_with("fur_view=")).collect::<Vec<_>>(), vec![&"fur_view=0"], "{t}");
        let back = Config::parse(&t);
        assert_eq!((back.debug_view, back.fur_view), (0, 0));
        // ... and saving to a file and loading it back gives 0 too.
        let dir = std::env::temp_dir().join(format!("gw2g_cfg_view_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.ini");
        Config { debug_view: 3, fur_view: 2, fur_amp: 0.6, ..Config::default() }.save(&p).unwrap();
        let loaded = Config::load(&p);
        assert_eq!((loaded.debug_view, loaded.fur_view, loaded.fur_amp), (0, 0, 0.6));
        let _ = std::fs::remove_dir_all(&dir);
        // The comments explain it.
        assert!(t.contains("NEVER saved"));
    }

    #[test]
    fn save_then_load_roundtrips() {
        let dir = std::env::temp_dir().join(format!("gw2g_cfg_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.ini");
        let c = Config {
            enabled: false,
            hdr_output: HdrOutput::On,
            hdr_format: HdrFormat::Hdr10,
            paper_white_nits: 250.5,
            peak_nits: 1800.0,
            sdr_gamma: 2.4,
            bloom_strength: 0.75,
            highlight_boost: 3.5,
            ao_strength: 0.6,
            ao_radius: 2.5,
            halo_strength: 1.25,
            halo_radius: 1.75,
            ssr_strength: 0.8,
            ssr_range: 1.5,
            ssr_classes: 5,
            ssr_smart: 0.25,
            ssr_hue_filter: 0.6,
            ssr_flatness: 0.35,
            ssr_water_boost: 0.45,
            water_depth_look: 0.2,
            ssr_matte_cut: 0.55,
            shafts_strength: 1.25,
            shafts_length: 0.45,
            contact_strength: 0.25,
            contact_length: 1.5,
            contact_sun: "fixed".to_string(),
            aerial_strength: 0.6,
            aerial_distance: 900.0,
            depth_unit: 2500.0, // (was 1.0: an old inch-based value now reads back as the default)
            aniso_level: 8,
            mip_bias: -0.3,
            smaa: false,
            smaa_threshold: 0.2,
            sharpen_strength: 0.6,
            autoexp_strength: 0.75,
            autoexp_target: 0.3,
            autoexp_range: 1.25,
            adapt_up: 0.9,
            adapt_down: 2.5,
            localtone_strength: 0.65,
            lens_strength: 0.6,
            lens_threshold: 4.0,
            lens_ghost: 0.3,
            lens_streak_length: 1.5,
            grade_preset: "Cool".to_string(),
            grade_saturation: 1.3,
            grade_contrast: 1.1,
            grade_temperature: -0.2,
            grade_tint: 0.1,
            grade_vibrance: 0.4,
            grade_shadows: 0.2,
            grade_highlights: -0.2,
            grade_lut: "luts/x.cube".to_string(),
            grade_lut_strength: 0.5,
            fur_strength: 0.8,
            fur_ps: "535b203b,1c8a2482".to_string(),
            fur_vs: "130e17de".to_string(),
            fur_min_vcb: 16464,
            fur_debug: true,
            fur_uv: "texcoord1.zw".to_string(),
            fur_occlusion: 0,
            fur_amp: 0.45,
            fur_scale: 1.5,
            fur_length: 8.0,
            fur_detail: 1.25,
            fur_rim: 0.75,
            fur_sss: 0.5,
            fur_view: 0, // runtime only: never written, so it cannot round-trip
            shell: crate::shellfur::ShellCfg {
                on: 0,
                shells: 24,
                length: 6.5,
                density: 1200.0,
                thickness: 0.5,
                root_dark: 0.4,
                wind: 0.7,
                lod: 35.0,
                rim: 1.5,
                ps: "1d50763f".to_string(),
                vs: "ba0709f4".to_string(),
                min_vcb: 16000,
                counts: "17694,-5214".to_string(),
                normal: "texcoord4".to_string(),
                unit: 39.37,
                debug: 1,
                detail: 0.3,
                clump: 0.8,
                fur_mask: "t1.g>".to_string(),
                fur_threshold: 0.4,
                fur_soft: 0.1,
                uv_exclude: "0.1 0.2 0.3 0.4; 0.5 0.5 0.6 0.7@17694".to_string(),
            },
            toggles: { let mut t = crate::toggles::Toggles::default(); t.0[3] = false; t },
            hdr_upgrade: false,
            hdr_clamp: false,
            rim_strength: 0.7,
            rim_width: 5.0,
            subsurface_strength: 0.6,
            rim_ps: "aabbccdd,11223344".to_string(),
            rim_min_vcb: 16000,
            rim_debug: true,
            overlay_key: "Ctrl+F9".to_string(),
            dev_capture_seconds: 20,
            dev_compare: false,
            dev_hot_reload: true,
            debug_view: 0, // runtime only: never written
            photo_key: "Ctrl+F10".to_string(),
            photo_hide_ui: true,
            calibration_pattern: 0,
        };
        c.save(&p).unwrap();
        assert_eq!(Config::load(&p), c);
        for m in [HdrOutput::Auto, HdrOutput::Off] {
            let c = Config { hdr_output: m, ..c.clone() };
            c.save(&p).unwrap();
            assert_eq!(Config::load(&p), c);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_leaves_no_tmp_file_and_replaces_an_existing_one() {
        let dir = std::env::temp_dir().join(format!("gw2g_cfg_tmp_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.ini");
        std::fs::write(&p, "enabled=false\n").unwrap();
        let c = Config { bloom_strength: 0.5, ..Config::default() };
        c.save(&p).unwrap();
        assert!(!dir.join("config.ini.tmp").exists());
        assert_eq!(Config::load(&p), c);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_creates_a_loadable_file_for_defaults() {
        let dir = std::env::temp_dir().join(format!("gw2g_cfg_def_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.ini");
        Config::default().save(&p).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        for k in ["enabled=", "hdr_output=auto", "paper_white_nits=", "peak_nits=", "bloom_strength=", "ao_strength=1", "ao_radius=1", "halo_strength=1", "halo_radius=1", "ssr_strength=0.5", "ssr_range=1", "ssr_classes=7", "ssr_smart=1", "ssr_hue_filter=1", "ssr_flatness=1", "shafts_strength=0.6", "shafts_length=0.7", "contact_strength=0.5", "contact_length=1", "contact_sun=auto", "aerial_strength=0.35", "aerial_distance=400", "depth_unit=1000", "fur_strength=0", "ssr_water_boost=0.6", "rim_strength=0.4", "overlay_key=Shift+End"] {
            assert!(text.contains(k), "missing {k} in:\n{text}");
        }
        assert!(text.starts_with('#'));
        assert_eq!(Config::load(&p), Config::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dev_keys_default_off_parse_and_clamp() {
        let d = Config::default();
        assert_eq!((d.dev_capture_seconds, d.dev_compare, d.dev_hot_reload), (0, true, false));
        let c = Config::parse("");
        assert_eq!((c.dev_capture_seconds, c.dev_compare, c.dev_hot_reload), (0, true, false));
        let c = Config::parse("dev_capture_seconds=20\ndev_compare=0\ndev_hot_reload=1");
        assert_eq!((c.dev_capture_seconds, c.dev_compare, c.dev_hot_reload), (20, false, true));
        assert_eq!(Config::parse("dev_capture_seconds=99999").dev_capture_seconds, 3600);
        assert_eq!(Config::parse("dev_capture_seconds=-5").dev_capture_seconds, 0);
        assert_eq!(Config::parse("dev_capture_seconds=abc").dev_capture_seconds, 0);
        assert_eq!(Config::parse("dev_capture_seconds=2.6").dev_capture_seconds, 3);
        assert!(Config::parse("dev_compare=maybe").dev_compare);
        assert!(!Config::parse("dev_hot_reload=maybe").dev_hot_reload);
        let t = Config { dev_capture_seconds: 30, dev_hot_reload: true, ..Config::default() }.to_ini();
        assert!(t.contains("dev_capture_seconds=30") && t.contains("dev_hot_reload=1") && t.contains("dev_compare=1"), "{t}");
    }

    #[test]
    fn aniso_and_aa_keys_default_parse_clamp_and_write() {
        let d = Config::default();
        assert_eq!((d.aniso_level, d.mip_bias, d.smaa, d.smaa_threshold, d.sharpen_strength), (16, 0.0, true, 0.1, 0.3));
        assert_eq!(Config::parse("").aniso_level, 16);
        let c = Config::parse("aniso_level=8\nmip_bias=-0.3\nsmaa=0\nsmaa_threshold=0.2\nsharpen_strength=0.7");
        assert_eq!((c.aniso_level, c.mip_bias, c.smaa, c.smaa_threshold, c.sharpen_strength), (8, -0.3, false, 0.2, 0.7));
        let c = Config::parse("aniso_level=99\nmip_bias=-9\nsmaa_threshold=9\nsharpen_strength=5");
        assert_eq!((c.aniso_level, c.mip_bias, c.smaa_threshold, c.sharpen_strength), (16, -1.0, 0.5, 1.0));
        let c = Config::parse("aniso_level=-4\nmip_bias=9\nsmaa_threshold=0\nsharpen_strength=-1");
        assert_eq!((c.aniso_level, c.mip_bias, c.smaa_threshold, c.sharpen_strength), (0, 0.5, 0.02, 0.0));
        let c = Config::parse("aniso_level=abc\nmip_bias=nan\nsmaa=maybe\nsmaa_threshold=x\nsharpen_strength=nan");
        assert_eq!((c.aniso_level, c.mip_bias, c.smaa, c.smaa_threshold, c.sharpen_strength), (16, 0.0, true, 0.1, 0.3));
        assert_eq!(Config::parse("Aniso_Level = 4").aniso_level, 4);
        let t = Config { aniso_level: 4, mip_bias: -0.5, smaa: false, smaa_threshold: 0.25, sharpen_strength: 0.5, ..Config::default() }.to_ini();
        for k in ["aniso_level=4\n", "mip_bias=-0.5\n", "smaa=0\n", "smaa_threshold=0.25\n", "sharpen_strength=0.5\n"] {
            assert!(t.contains(k), "{k} missing in {t}");
        }
        let back = Config::parse(&t);
        assert_eq!((back.aniso_level, back.mip_bias, back.smaa, back.smaa_threshold, back.sharpen_strength), (4, -0.5, false, 0.25, 0.5));
    }

    #[test]
    fn exposure_keys_default_parse_clamp_and_write() {
        let d = Config::default();
        assert_eq!((d.autoexp_strength, d.autoexp_target, d.autoexp_range, d.adapt_up, d.adapt_down, d.localtone_strength), (0.5, 0.2, 0.7, 0.6, 1.8, 0.2));
        assert_eq!(Config::parse("").autoexp_strength, 0.5);
        let c = Config::parse("autoexp_strength=0.8\nautoexp_target=0.3\nautoexp_range=1.2\nadapt_up=1\nadapt_down=3\nlocaltone_strength=0.9");
        assert_eq!((c.autoexp_strength, c.autoexp_target, c.autoexp_range, c.adapt_up, c.adapt_down, c.localtone_strength), (0.8, 0.3, 1.2, 1.0, 3.0, 0.9));
        let c = Config::parse("autoexp_strength=9\nautoexp_target=9\nautoexp_range=9\nadapt_up=999\nadapt_down=0\nlocaltone_strength=-4");
        assert_eq!((c.autoexp_strength, c.autoexp_target, c.autoexp_range, c.adapt_up, c.adapt_down, c.localtone_strength), (1.0, 0.6, 2.0, 20.0, 0.05, 0.0));
        let c = Config::parse("autoexp_strength=abc\nautoexp_target=nan\nautoexp_range=inf\nadapt_up=x\nadapt_down=\nlocaltone_strength=nan");
        assert_eq!(c, Config::default());
        assert_eq!(Config::parse("AutoExp_Strength = 0").autoexp_strength, 0.0);
        let t = Config { autoexp_strength: 0.25, adapt_down: 4.0, localtone_strength: 0.0, ..Config::default() }.to_ini();
        for k in ["autoexp_strength=0.25\n", "autoexp_target=0.2\n", "autoexp_range=0.7\n", "adapt_up=0.6\n", "adapt_down=4\n", "localtone_strength=0\n"] {
            assert!(t.contains(k), "{k} missing in {t}");
        }
    }

    #[test]
    fn ignores_garbage() {
        assert_eq!(Config::parse("paper_white_nits=abc").paper_white_nits, 203.0);
    }
}
