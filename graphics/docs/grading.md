# Colour grading (sliders, presets, optional 3D LUT)

Status: implemented, unit and WARP tested (`src/grading.rs`, `src/look.rs`, `shaders/look.hlsl`; tests in `grading_tests.rs` and
`look_tests.rs`). NOT yet verified in the real game: see "In-game checks".

All sliders neutral and no LUT = the feature does not exist for the GPU: no pass, nothing rebound, bit-identical. The UI is never graded.

## Where it runs

Same place as the lens effects (`lens.md`): the mod's composite pass at the engine's pre-UI copy (`ps#c0ad1686`, after the AA), on the
LINEAR scene light (extended sRGB decode, 1 = paper white, values above 1 are legal) and re-encoded for the engine. The final shader's UI
path is untouched. When the lens is also on, the grade is applied after the lens light is added (the glare is graded with the scene).

## Sliders (`Grade`, hot-reloadable, overlay section "Color grading")

Order of the maths (host mirror `grading::grade_rgb`, shader `grade()`):

1. White balance gains (`temp_tint_gains`): red `1 + 0.2 t`, blue `1 - 0.2 t`, green `1 - 0.15 tint`, divided by their luminance so a grey keeps
   its luminance. (0, 0) is exactly (1, 1, 1).
2. Contrast: a scale on the luminance `Y`: `0.18 (Y / 0.18)^c` up to Y = 1, then the tangent line (C1, monotonic); mid-grey 0.18 is a fixed point and
   HDR values never go through a power law. The scale is applied to all channels (hue kept).
3. Shadows / highlights: `x (1 + shadows 0.6 w_s(Y)) (1 + highlights 0.6 w_h(Y))`, `w_s = 1 - smoothstep(0.02, 0.35, Y)`, `w_h = smoothstep(0.3, 1, Y)`. At
   +-0.5 that is at most 30 %; black stays black (nothing is made visible that was not).
4. Saturation: `Y + (c - Y) s`, negatives clamped to 0. s = 0 gives exactly the Rec.709 luminance.
5. Vibrance: the same mix with `1 + vibrance (1 - chroma) 0.75`, `chroma = (max - min) / max`: dull colours get up to +75 %, saturated ones almost nothing, greys nothing.

| key | default | range |
|---|---|---|
| `grade_saturation` | 1.0 | 0..2 |
| `grade_contrast` | 1.0 | 0.5..1.5 |
| `grade_temperature` | 0 | -1..1 (warm +) |
| `grade_tint` | 0 | -1..1 (magenta +) |
| `grade_vibrance` | 0 | 0..1 |
| `grade_shadows` | 0 | -0.5..0.5 |
| `grade_highlights` | 0 | -0.5..0.5 |

## Presets

`grade_preset` (name; case, spaces, `_` and `-` do not matter) is applied at load only when set, and the individual keys override it wherever they
appear in the file. In the overlay the buttons set the sliders (and `grade_preset`); Save writes the preset name AND every slider, so the file always
reproduces what you saw. Numbers (pinned by `presets_match_the_documented_numeric_table`):

| preset | sat | contrast | temp | tint | vibrance | shadows | highlights | idea |
|---|---|---|---|---|---|---|---|---|
| Original | 1 | 1 | 0 | 0 | 0 | 0 | 0 | everything neutral |
| Warm cinematic | 0.92 | 1.12 | 0.30 | 0.05 | 0.10 | -0.10 | -0.08 | warm light, deeper shadows, a little more contrast |
| Cool | 0.95 | 1.05 | -0.25 | 0 | 0 | 0 | 0.05 | cooler white balance, slightly brighter highlights |
| Vivid | 1.25 | 1.10 | 0.05 | 0 | 0.35 | 0 | 0 | richer colours (dull ones most) |
| Soft | 0.90 | 0.88 | 0.05 | 0 | 0 | 0.15 | -0.15 | low contrast, lifted shadows, calm highlights |

## 3D LUT

* `grade_lut` = a `.cube` file INSIDE the GW2Graphics folder (relative path such as `luts/mine.cube`; absolute paths, drives, `..` and other
  extensions are refused). Size 17..65 (`LUT_3D_SIZE`), `DOMAIN_MIN 0 0 0` / `DOMAIN_MAX 1 1 1` only, red varies fastest. Parsed on a background
  thread at start and whenever `grade_lut` changes (hot reload); uploaded by the render thread as an immutable RGBA16F 3D texture.
* The cube is read as a **log2 shaper LUT**: the input axis is `(log2(linear) + 8) / 14` (linear light between 2^-8 and 2^6, so values above 1.0
  are legal), the output uses the same encoding (`linear = 2^(y 14 - 8)`). Values outside the range keep their ratio, so an identity cube is the
  identity everywhere, black and super-white included. A display-referred cube (a film emulation made for 0..1 sRGB) will NOT look right here: make
  or convert it for this shaper. `grade_lut_strength` 0..1 blends in linear light (0 = off, no pass).
* Lookup is trilinear (8 `Load`s in the shader, no sampler state), after the sliders.
* Failure is never fatal: missing file, wrong size, 1D LUT, NaN, truncated or too many rows, a file over 24 MB, an unreadable file: the LUT stays
  off and the overlay line "LUT status" and `status_grade=` say why ("LUT off: truncated: 100 of 4913 data rows").
* **No LUT = the strength does nothing.** `grade_lut` is empty by default; then `grade_lut_strength` has nothing to blend (seen in the game as
  a LUT strength that changed nothing: `status_grade=` said `LUT: no LUT`). The status now reads "no LUT loaded (grade_lut is empty): the LUT strength
  does nothing; ..." and the overlay shows it in orange above the LUT picker.

### Example LUTs and the picker

The mod ships four example cubes in its own log2 shaper space (17^3, all keep the luminance: only the colour moves):

| file | look |
|---|---|
| `identity.cube` | no change (a test that the LUT path is transparent) |
| `warm.cube` | red x1.07, blue x0.88, luminance restored |
| `cool.cube` | red x0.92, blue x1.10, luminance restored |
| `teal-orange.cube` | split toning: shadows toward teal, highlights toward orange (log luminance from 1.6 % to 100 % of white) |

* In the game: overlay, "Color grading", button **Create example LUTs** writes them into `GW2Graphics\luts` (creates the folder, never overwrites an
  existing file). The **LUT** combo lists every `.cube` in `GW2Graphics\luts` ("Refresh list" re-reads the folder); picking one sets `grade_lut`
  (loaded live on a background thread), "(none)" clears it. Then move **LUT strength** (0..1, blends in linear light). Save keeps `grade_lut`.
* In the repo: `graphics\presets\luts\*.cube`, byte-identical to what the button writes (a test pins it; regenerate with
  `cargo test write_repo_example_luts -- --ignored`). Copy one to `GW2Graphics\luts` and set `grade_lut=luts/warm.cube` by hand if preferred.
* Generator: `grading::example_cube_text` (lattice point -> decode the shaper axis -> look in linear light -> encode, unclamped).

## Debug capture and overlay

`status_grade=` (sliders applied / waiting / why not, LUT state and strength, frames). The debug capture also lists every `grade_*` and `lens_*` value.

## Tests

Host: neutral is exactly neutral and the identity; ranges / NaN; saturation 0 = luminance; contrast keeps mid-grey, is monotonic across the knee with no
jump; temperature monotonic in red/blue and luminance-preserving; vibrance leaves greys and saturated colours alone; shadows/highlights bounds; HDR
values (8, 4, 2) .. (40, 38, 20) keep their channel order and are not clipped for every preset and every extreme; the preset table; config preset +
override order and ini round trip; `.cube` parser (good, CRLF/BOM/comments, bad size, 1D, NaN, truncated, too many, bad domain, junk); path rules;
shaper round trip and identity everywhere; half-float conversion; the background loader (each outcome, newer request wins); the example LUTs parse,
keep the luminance and warm / cool / split-tone as named; they are written once (never overwritten) and listed; the repo copies are current.
WARP: the GPU grade equals the host mirror within 0.4 % for every preset and extreme slider combination over a 256x128 palette including a 24x
highlight; mid-grey unchanged by contrast; identity LUT (17, 33, 65) within 1e-3 of the input; a shifted LUT does what it says; strength blends;
an unusable cube is skipped without a crash while the sliders still run; the bundled warm example applied on the GPU warms by the documented amount,
strength 0.5 lands halfway in linear light and strength 0 is no pass.

## In-game checks

1. Overlay, "Color grading": press each preset; the picture changes but the UI (map, chat, bars) does not. "Original" gives back the exact picture.
2. `status_grade=` says "applied" with growing frame counts; the vanilla half of a compare shot is ungraded.
3. Press "Create example LUTs", pick `luts/warm.cube` in the LUT combo: `LUT status` says loaded; LUT strength 0 -> 1 warms the scene smoothly;
   `luts/identity.cube` must look unchanged. With "(none)" the orange line says the strength does nothing.
4. A preset on a bright HDR scene (sun, fire): highlights keep their colour, nothing clips to white earlier than before.

## Future work

* `grade_profile_<mapid>`: per-map profiles (a preset or a set of keys chosen by the current map id). Out of scope here; the map id is available from
  the game's mumble link, which this mod does not read yet.
* Three-way colour wheels (lift / gamma / gain), a display-referred LUT mode, a LUT in the light-adjust substitute (before the bloom), grading the glare separately.

## Known limits

* Saturation above 1 on very saturated bright colours clamps the weakest channel at 0 (hue order is kept, not strictly).
* Contrast 1.5 lifts highlights a lot (slope `c f(1)` above the knee); the presets stay at 1.12 or below.
