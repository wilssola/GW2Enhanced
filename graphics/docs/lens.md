# Lens effects (streaks, glare, ghosts)

Status: implemented, unit and WARP tested (`src/lens.rs`, `src/look.rs`, `shaders/look.hlsl`; tests in `lens_tests.rs` and `look_tests.rs`).
NOT yet verified in the real game: see "In-game checks".

Goal: anamorphic-style horizontal streaks and a soft glare from HDR highlights, and subtle ghost flares for VERY bright point
sources only (sun disk, fire, big skill lights). The UI is never touched. HDR output only (in SDR nothing is above paper white).
Only compact light sources (fire, torches, lamps, lanterns, candles, the sun) count: big glowing areas such as an emissive or
translucent creature get no flare (see "Which pixels are sources").

## Where it runs (and why there)

At the draw of the engine's pre-UI copy (`ps#c0ad1686`), exactly where the AA runs (`aa.rs`, `data-path.md`): `look::draw_guard` is
created right after the AA's guard, reads the copy's input t0 (the AA result when the AA is on), runs the mod's passes in mod-owned
float16 targets and binds the result as t0 for the engine's own draw; the guard restores the engine's SRV afterwards (RAII). No engine
target is written, the UI is drawn after the copy. Chosen over the light-adjust substitute because the lens needs blurred, downsampled
copies of the whole finished picture (a per-pixel substitute cannot read them), it must not feed the auto exposure measurement or the
bloom, and it keeps the 37 KB substitute free of new inputs. Cost of the choice: the exposure and the highlight boost are already
applied at that point, so the threshold is expressed in units of the scene BEFORE the boost (see below).

## Which pixels are sources

The first version took every pixel above the threshold. In the game that put streaks and ghosts on glowing creatures (a translucent
spectral blue pet, a glowing orange spirit wolf): their bodies are drawn emissive / additive and easily reach 4x paper white over a large
area. What the runtime knows about a pixel at the pre-UI copy, and what was usable:

* **Which pass drew it: not usable.** The picture is the finished scene; no per-pixel record of the drawing pass exists. Fire, torch
  flames, skill lights and the glowing creatures are all translucent effects drawn after the linear depth (`m2-ao.md`: pass #9
  "effects / transparents", same targets as the opaque pass); the #9 shaders are not in the exe and their blend states are not known
  (`engine-post-chain.md`). Telling fire particles from a glowing pet by pass or blend state would need per-draw classification of
  creature shaders that does not exist.
* **Depth: not usable.** The linear depth excludes transparents (`m2-ao.md`, Known limits), so a translucent pet reads the depth of
  whatever is behind it, and a pet against the sky would read as sky (i.e. as the sun).
* **Temporal stability: not used.** Fire and candles flicker more than a glowing body, so stability does not separate them; it would
  also need history targets and motion handling.
* **Shape and size: used (the main test).** A light source is a small, compact bright core; a glowing body is a large bright area.
* **Colour: used (a second, softer test).** Fire, lamps, candles and the low sun are warm; the sun and some lamps are white or bluish
  white; spectral and magic glows are mostly saturated blue, cyan, violet or green.

**Size test** (`lens_max_size`, share of the picture height, default 0.04 = 58 px at 1440 p). For every 1/8 source texel the share of
bright pixels in the square of side `4 x lens_max_size` around it is measured; "bright" = largest channel above `0.5 T` (half the
threshold, so the dimmer interior of a body whose rim alone exceeds `T` still counts as bright area). The bright area, in units of
`lens_max_size` squared, gives the weight `1 - smoothstep(1, 2.5, area)` that multiplies the source (streak, glare and ghost). A lamp or
flame up to `lens_max_size` across keeps its full weight (bit-identical to no test), one about 1.8x as wide gets nothing; a glowing
body, its edges, its corners and its limbs (all within half a window of the body) get nothing. `0` = no size test.

**Colour test** (`lens_color_reject`, 0..1, default 0.5). Per pixel `penalty = smoothstep(0.25, 0.75, chroma) x (1 - warm)` with
`chroma = (max - min) / max` and `warm = saturate(2 (r - b) / max)`; the pixel's threshold becomes `T x (1 + 4 x lens_color_reject x
penalty)`. White, cool white (the sun), warm white, yellow, orange and red have no penalty; a saturated blue / cyan / violet / green
glow needs 3x the threshold at the default (5x at 1). Blue is not banned: a very bright blue light is still a source. `0` = colour
ignored.

## Algorithm (all `Load`-based, no sampler state is touched)

0. The size test, when `lens_max_size` > 0: `ps_cover` (full -> 1/16): the share of bright pixels (encoded largest channel above the
   encoded `0.5 T`; the encoding is monotonic, so no decode) on a staggered grid of 8x8 of the 16x16 pixels. `ps_area_h`, `ps_area_v`
   (1/16): a box mean of that share over the window, 16 bilinear taps each.
1. `ps_down4`: full -> 1/4. Per pixel: decode the extended sRGB, take the largest channel `m`; the source is `rgb * max(m - T', 0) / m`
   (hue kept), alpha = `max(m - 2T', 0)` (the ghost source), `T'` = `T` raised by the colour test. `T = lens_threshold * gain`, `gain` =
   the highlight boost the light-adjust shader applied to SDR white (`datapath::current_params().boost`, 1 in SDR). So SDR white things
   (sand, snow, white fur, the white UI) sit at `gain` after the boost and are far below `T`; a light has to be `lens_threshold` times
   brighter than white BEFORE the boost. 2x2 groups are Karis weighted (`1 / (1 + luma)`): a single hot pixel does not flicker the
   picture. Pixels whose encoded largest channel is under the encoded plain threshold exit early (no `pow`).
2. `ps_down8`: 1/4 -> 1/8, Karis weighted 2x2, times the size-test weight (bilinear from the 1/16 area).
3. Glare: `ps_glare_h`, `ps_glare_v` on the 1/8 source: 9 taps per direction, gaussian (sigma 1.5 taps), step `max(1, width8/160)` texels.
4. Streak: `ps_streak0/1/2`, three horizontal 7-tap passes at 1/8 with the step growing x3 (1, 3, 9 units; a dual-Kawase style stretch),
   weights `exp(-0.45 |i|)`, normalised (energy kept). The total reach of the three passes is 39 units = `lens_streak_length * width8 / 6`
   texels, i.e. a sixth of the picture width on each side at length 1.
5. `ps_composite` (full resolution): `lin += bound(tint(streak + 0.35 * glare) * lens_strength * 2 + ghosts * lens_ghost)`.
   * `tint`: the streak colour (normalised by its largest channel) is pulled 65 % to warm white (1, 0.92, 0.80) and the blue channel is
     capped at 85 % of the larger of red / green: a blue light gives a white-warm streak, never a blue paint. The largest channel
     (the energy) is kept. Same discipline as the bloom whitening.
   * ghosts: two mirrored, shrunken copies of the very bright sources about the screen centre (sample at `0.5 + (uv-0.5)/s`, `s = -0.3`
     and `-0.7`, weights 0.6 and 0.35), a radial chromatic split (the three channels sample at scales differing by 7 %) and warm tints
     (1, .82, .58) / (.95, .88, .70). Only energy above 2T feeds them.
   * `bound(a) = a / (1 + max(a) / 0.6)`: about the identity for small values, the added largest channel never reaches +0.6 of paper
     white however bright the source.
   A pixel the lens does not change keeps the engine's value exactly (the shader returns the input texel).

Targets: 1/4 (RGBA16F), 1/8 source, ping, pong, glare (RGBA16F), two 1/16 (RGBA16F, the size test), plus the full-size float16
output (shared with the grading).
Cost, 3440x1440, estimated (not measured): `ps_down4` 16 loads per 1/4-res pixel with an early out, about 0.1 to 0.2 ms; the size test
reads a quarter of the picture once (`ps_cover`, 64 loads per 1/16 texel, compares only) plus two 16-tap passes at 1/16, about 0.05 ms;
the colour test is a few ALU only on pixels already above the threshold; everything at 1/8 is under 0.05 ms; the composite is one full-screen pass with about 12 loads and no `pow` unless the lens adds light (about 0.1 ms; it
is shared with the grading). With `lens_strength` 0 (and the grading neutral) nothing runs and nothing is rebound.

## Config

| key | default | range | meaning |
|---|---|---|---|
| `lens_strength` | 0.15 | 0..1 | streak / glare amount. 0 = off: no passes, bit-identical |
| `lens_threshold` | 4 | 1.2..8 | times paper white of the scene BEFORE the highlight boost; ghosts need 2x this |
| `lens_ghost` | 0.05 | 0..0.5 | ghost flares of very bright sources; 0 = no ghosts (the streak is unaffected) |
| `lens_streak_length` | 0.6 | 0.25..2 | streak length |
| `lens_max_size` | 0.04 | 0..0.2 | largest source diameter (share of the picture height) with the full effect; bigger bright areas get none. 0 = no size test |
| `lens_color_reject` | 0.5 | 0..1 | saturated cool glows (blue, cyan, violet, green) need up to `1 + 4 x this` times the threshold. 0 = colour ignored |

All six are hot-reloadable (`debug_hot_reload`) and live in the overlay section "Lens effects". `status_lens=` in the debug capture says
"applied at the pre-UI copy ... N frames", "waiting ...", "off, SDR output" or why it does not run.

## Rules kept

* Visual only; adds light only around compact light sources, never reveals anything, bounded to +0.6 of paper white.
* Mod disabled / vanilla emulation (`gate::user_enabled` false, substitution off): nothing runs.
* Under water or in fog the lens is not special-cased: it only reacts to very bright sources.

## Tests

`lens_tests.rs` (host): defaults and clamps, "ordinary bright things never reach the threshold" (white after the boost, 1.5x it), tint never
blue and energy kept, the bound, Karis weights, geometry, shader constants. `look_tests.rs` (WARP): sand / snow / white at paper white
and at `gain` leave every pixel unchanged with the lens running; a bright blue-white source gives a horizontal streak (row vs column,
symmetric, decaying, at most +0.6, warm, blue never dominant, black far away stays black); strength scales the streak, longer length
reaches farther; ghosts appear at the mirrored place for a 20x source, none for a 4x source; NaN / negative / huge texels do not
spread; identical input gives identical output; the pipeline state is restored and our slots left empty; target rebuild on a size change.
Source selection: host tests of the colour penalty (white, cool white, warm lamp, candle, fire, yellow: none; blue, cyan, violet,
green: full), the raised threshold, the area weight (a source of the max size keeps 1, twice as wide and a body / edge get 0,
monotonic), the window and step, the constants and that the shader mirrors them. WARP: a 61 px glowing orange body streaks without
the size test and adds nothing with it, also with a hot rim around a dimmer interior; a lamp next to it keeps its streak; a torch-sized
source and a sun disc are bit-identical with and without the test; saturated blue and cyan at 2.8x the threshold add nothing while
white, cool white and orange do, blue does with the colour test off and when 3x brighter.

## In-game checks

1. Look at the sun / a bright skill with `lens_strength` 0 and 0.35 (hot reload): a soft horizontal streak, warm white, no blue.
2. Glowing creatures (a spectral blue pet, a glowing spirit wolf) at normal and close camera distance: no streak, glare or ghost. If
   one still flares far away (small on screen), raise `lens_color_reject` (blue) or lower `lens_max_size`; `lens_max_size` 0 shows
   the old behaviour for comparison.
3. Torches, lamps, lanterns, candles, campfires and the sun must keep their flare. If the sun disc or a big bonfire lost it, raise
   `lens_max_size` (0.06 to 0.08); if a bluish lamp lost it, lower `lens_color_reject`.
4. Sand, snow, white fur, a white armour in sunlight: no streak at all. If a bright desert still glows, raise `lens_threshold`.
5. `status_lens=` must say "applied" and grow; the mod-off comparison shot must show no lens.
6. The streak amplitude (`LENS_GAIN` 2, `GLARE_WEIGHT` 0.35 in `lens.rs` / `look.hlsl`) was chosen on paper, not tuned in the game.

## Known limits

* HDR (scRGB) output only. The threshold ignores the auto exposure (up to +-1 EV): a scene the exposure brightens by 2x makes lights
  trigger at half the nominal threshold.
* Sources off screen produce no streak (the picture is all there is). The streak reach scales with the picture width.
* Ghosts use the blurred 1/8 source, so they are soft blobs, not a lens-shaped flare.
* The size test sees bright AREA, not objects: a glowing creature far away (smaller on screen than `lens_max_size`) is compact and can
  still flare when it is warm (the colour test catches the cool ones); a lamp right next to a big bright area (a bright wall, another
  glowing body) loses its flare; a close bonfire bigger than `lens_max_size` gets none. A thin bright line (rim) with nothing bright
  inside it counts as a small area.
