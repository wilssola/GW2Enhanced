# Screen-space reflections (M4)

Status: implemented, WARP tested. Not yet seen in the real game. Water and glass boost: see "Water and glass" below.

## What it does

Wet or shiny floors (the Iron Legion deck, ground, water-like flat surfaces) reflect what is above them on the
screen. The engine has no material or roughness buffer, so the decision is geometric and deliberately
conservative:

* **Floor test.** The view-space normal is rebuilt from the linear depth like `ao.hlsl` (smaller one-sided
  difference per axis). Only pixels whose normal has a dominant up component count: weight
  `smoothstep(0.80, 0.94, N.y)` (an unrolled camera is assumed). Walls (`N.y` about 0) and ceilings
  (`N.y` < 0) get nothing.
* **Fresnel.** `F = 0.04 + (0.6 - 0.04) * (1 - n.v)^4`: a low floor, growing at grazing angles, capped at 0.6.
* **Fades.** The weight is multiplied by: a fade for rays heading back to the camera, a fade of the last 45 % of
  the ray length, a fade over the first 4 % (no self-hit at the start), a fade of the hit position over the 10 %
  of the screen next to each border, and a mild fade of the origin near the border.
* **Sky.** A ray that reaches a pixel with invalid depth (sky, cleared area) reflects THAT pixel's scene colour
  (the sky colour above) at 35 % of a normal hit (`SKY_WEIGHT`). A ray that leaves the screen without a hit
  reflects nothing. A floor with nothing above it therefore shows the sky faintly, never garbage.
* The colour is the scene colour at the hit, gamma 2.0 decoded to linear and clamped at 3.0 (no fireflies from
  lights). The texture holds `rgb = linear colour * every weight`, `a = the weight`.

## Where it runs (architecture rule of `data-path.md`)

Nothing is drawn into the engine's targets. At the light-adjust draw `datapath::bind_current` calls
`ssr::on_light_adjust` once per frame (a second draw in the same frame only rebinds). It reads the depth
(`ao::depth_resource()`) and the scene colour (PS SRV slot 0 right then), draws three passes into the module's own
half-size `RGBA16F` targets (`StateGuard` restores the pipeline; only `Load()` is used, so no sampler state is
touched), and binds the result as `t8` and the strength as `b7` of the light-adjust substitute:

```
lin = c.rgb * c.rgb                      // after the AO factor
lin += refl.SampleLevel(s0, uv, 0).rgb * sp.x   // before the exposure remap
```

so the AO never darkens a reflection, and the remap/boost treat it like any other light. Unbound `t8`/`b7` read
zero = off (pinned by a test; the original-equivalence tests are unchanged apart from allowing the two extra
declarations).

Passes (`shaders/ssr.hlsl`):

1. `ps_trace`: normal, floor test, Fresnel, then a screen-space ray march. The segment is clipped to the image and
   marched with 32 coarse steps (perspective-correct: 1/z is linear in screen space), a per-pixel dithered start
   offset, a thickness test (a hit needs the ray 0.2 % to 4 % of the depth behind the surface, plus one step of
   depth change), and 6 binary refinement steps.
2. `ps_blur_h`, 3. `ps_blur_v`: a depth-aware 9-tap blur of rgba (the "roughness" blur; it also removes the
   dither noise).

## Parameters

| key / overlay slider | range | default | meaning |
|---|---|---|---|
| `ssr_strength` / Reflection strength | 0..2 | 0.5 | scale of the added light; **0 = whole feature off**: no passes, depth consumer (`ao::DEPTH_FOR_SSR`) released |
| `ssr_range` / Reflection range | 0.25..2 | 1.0 | ray length = 3 x range x the depth of the reflecting pixel |

Both apply live (atomics copied to constant buffers; nothing recompiles). Fixed constants in `ssr.rs`:
`FRESNEL_FLOOR` 0.04, `RAY_LENGTH` 3, `SKY_WEIGHT` 0.35, `MAX_LINEAR` 3.

Overlay: a "Reflections" status line (`applied`, or why it skips: no depth, no scene SRV, sizes differ). Debug view
(level 1) tints pixels that received a reflection magenta (50 %).

## Materials

### What the engine's alpha is (and is not)

The scene colour's alpha (docs/engine-post-chain.md, #13 / #15) is re-encoded by shader `4340ca88` into 3 classes
(raw a > 0.491961 "high", 0.240980 < a <= 0.491961 "mid", a <= 0.240980 "low"). **Measured in the game (in-game
screenshots of the "Debug materials" view): the whole picture was class "low" (forest grass, path, trees, the Iron
Legion deck); only the pet and glowing sprites differed.** So that mask marks characters and effects, not terrain
materials, and cannot tell grass from metal. It is kept as an optional gate (`ssr_classes`, default all on, nothing was
removed) but the detection no longer relies on it.

### The reflectivity score (`ssr_smart` > 0): positive evidence

There is no material buffer, so the trace pass computes, at the receiving pixel (after the floor test), a score in
[0,1] from signals that exist in the picture, and multiplies the reflection weight by `lerp(1, score, ssr_smart)`.
The design is POSITIVE evidence: a surface reflects only if it looks like a reflective material, and anything
unclassified (dry dirt, mud, sand, wood, grass, rock) scores exactly 0. (The old design started from 1 and only
rejected green / warm hues, so desaturated or dark brown dirt passed.)

`raw = class_prior * smoothness * flatness * specular_cue`, then `score = smoothstep(0.35, 0.70, raw)`: weak evidence
gives exactly 0, never a faint reflection. Every threshold is a named constant in the header of `shaders/ssr.hlsl`.

| term | signal | result |
|---|---|---|
| class prior (slider `ssr_hue_filter`; 0 = every colour is a candidate) | linear receiver colour, a 5-tap spatial mean (centre + 4 taps 2 px away) | **water / glass / ice**: cool (B/R 0.95..1.15) and coloured (saturation 0.08..0.22) = 1. **wet / polished stone, metal**: nearly colourless (saturation 0.06..0.16 ramps off) = 0.8. **Everything else = 0** (warm brown / ochre / sand / wood / rust, dark desaturated earth). Vegetation (G / max(R,B) 1.15..1.40 with saturation) and warm earth (R/B 1.3..2.0, R >= G >= B) also veto a value that looks cool (a green with B > R). |
| smoothness | std. deviation of the encoded luminance PLUS the chroma axes (R-B, G-(R+B)/2, weight 0.5) at two scales: narrow 5x5 taps 2 px apart, ramp 0.03..0.10; wide 9x9 taps 4 px apart (+-16 px, also needs a large coherent area), ramp 0.05..0.14 | rough = noisy at EITHER; smoothness = 1 - rough, down to 0 (micro detail kills the reflection) |
| flatness (slider `ssr_flatness`) | normals rebuilt from the depth at 4 taps on rings of 4, 8, 16 px (x height/1080 above 1080p) vs the centre normal (depth-normal variance), ramps 0.08..0.30, 0.07..0.26, 0.06..0.22 (about 5..17 degrees) | bumpy terrain, rocks, grass: up to 95 % off; one plane (polished floor, water): kept |
| specular cue | linear luminance Y | `1 - 0.5 * smoothstep(0.25, 0.80, Y)`: bright diffuse or bright metal keep less, dark and mid water keep all |

Reflection strength = Fresnel x score x floor / ray fades; the existing depth-aware blur passes (which also carry the
score in alpha) soften the mask and the reflection, and rough surfaces are already killed by the smoothness term
instead of being blurred (a roughness-dependent blur width was not added: the score reaches 0 before blur matters).

Stability: the score is a pure function of the picture at trace resolution (no time input, no per-frame dither in
it; the dither only offsets the ray start), the class and the specular cue use the 5-tap mean, and the blur smooths the
mask. A still camera gives an identical score every frame (a test pins bit-identical repeats) and +-0.2 % colour noise
moves it by < 0.08 (tested, in practice < 0.03 on plateaus).

Typical scores: water / ice 1.0, smooth polished neutral floor 1.0, bright smooth grey metal about 0.5, brown / dark
desaturated / grey-brown / noisy dirt, sand and grass 0.

Costs (only on floor pixels, at half resolution): up to about 25 + 81 luminance taps and 12 normals; the wide roughness
and the flatness are skipped when the hue gate already killed the pixel. `ssr_smart` 0 skips all of it and is
bit-identical to a plain floor reflection (a test pins this regardless of colour, hue and flatness values).

| key / overlay | range | default | meaning |
|---|---|---|---|
| `ssr_smart` / "Reflection smart (material score amount)" | 0..1 | 1 | master amount of the score; 0 = no material logic |
| `ssr_hue_filter` / "Reflection hue filter (green / warm earth)" | 0..1 | 1 | scales the colour-class requirement (cool or colourless only); 0 lets every colour be a candidate |
| `ssr_flatness` / "Reflection flatness (varying normals)" | 0..1 | 1 | scales the flatness requirement; 0 ignores bumpy normals |
| `ssr_classes` / "Reflect on: low (blue) / mid (green) / high (red)" | bitmask 0..7 (1 low, 2 mid, 4 high) | 7 | optional alpha-class gate (see above) |

All live (constants c3.z/c3.w and c4 of the trace, no recompile). Save in the overlay writes them to config.ini. The matte gate below (`ssr_matte_cut`) multiplies this score.

### Matte gate (`ssr_matte_cut`): flat, smooth and pale is not glossy

**Seen in the game: reflections still appear on dry dirt / earth / rock ground.** Root cause: the game renders forward into one
8-bit target. There is **no gloss, roughness or specular buffer** ([graphics-design.md](specs/graphics-design.md): "There is no normal or roughness
buffer"), and the only per-pixel material data, the scene alpha, puts the whole terrain in one class (see above). The score therefore judges
the receiver's COLOUR, and colour is mostly the light: under moonlight, blue fog or a blue sky ambient dry earth is cool and pale exactly like
water (the "water / glass / ice" class, prior 1), and in grey haze or shade it is colourless exactly like polished stone (prior 0.8). A large,
smooth, flat dirt path then passes the smoothness and flatness terms too (smooth texture at half resolution, one plane), so flat ground plus
the light's tint was enough.

Evidence (colour replay of the class prior, the two-scale roughness and the specular cue of `ssr.hlsl` on the in-game PNGs; the captures hold
no depth, so the floor test and the flatness term could not be replayed and were assumed passed, i.e. an upper bound; lower 37 % of the
picture, central 65 %, every 6th pixel):

| picture | score > 0 before (cut 0) | water class / grey class | after (cut 0.8) |
|---|---|---|---|
| night dirt path, frame mean B/R 1.48 | 36.5 % | 35.8 % / 0.8 % | 0.9 % |
| foggy dirt slope, B/R 1.08 | 28.3 % | 16.8 % / 11.5 % | 0.2 % |
| sunny slope | 0.2 % | 0 / 0.2 % | 0.2 % |
| warm daylight | 0.0 % | 0 / 0 | 0.0 % |

The gate (all in `shaders/ssr.hlsl`, constants `MC_*`, `MATTE_K`; only when `ssr_smart` > 0):

1. `ps_mean` (a 1x1 pass before the trace, 32x18 taps): the mean linear colour of the pixels with valid depth (sky excluded).
2. `matte_gloss`: the receiver's 5-tap mean colour is white balanced by that mean (gain `Ymean / mean` per channel, clamped 0.5..2, blended in by
   the cut): a global tint of the light is removed, water stays bluer than the frame around it. On the balanced colour the water / glass / ice
   ramps are stricter (B/R 0.95..1.15 -> 1.25..1.60, saturation 0.08..0.22 -> 0.18..0.35, lerped by the cut), a very dark receiver needs
   linear luminance 0.01..0.04 (colour ratios of a few 8-bit codes are noise), the vegetation / warm-earth vetoes stay, and the colourless
   branch is scaled by `1 - cut`.
3. The raw score is multiplied by `lerp(1, gloss^2, cut)` BEFORE the score's own gate (`smoothstep(0.35, 0.70)`), so matte ground lands below
   it and is exactly 0, while water (gloss 1) is unchanged and keeps `ssr_water_boost` (the water receiver needs a score above 0, so the boost
   follows the gate automatically).

| key / overlay | range | default | meaning |
|---|---|---|---|
| `ssr_matte_cut` / "Matte cut (dry ground)" | 0..1 | 0.8 | 0 = the score as before, bit for bit; 0.56 and above remove the grey "polished" class entirely; 1 = strictest |

Live (c5.x of the trace), hot reload, overlay slider (Save writes it), NaN gives the default, clamped. `ssr_smart` 0 ignores it (bit-identical).

Trade-offs (no gloss signal exists, so these are deliberate): grey polished stone, metal decks and teal-grey decks no longer reflect at the
default (set 0.3..0.5 for them, at the cost of grey dry ground in shade); water that fills nearly the whole frame is balanced towards
neutral and can lose its reflection (shore and sky in the frame keep it blue relative to the mean); very dark night water below linear
luminance 0.01 loses it.

Telemetry: every 60 frames `ps_stats` scores a 64x36 grid with and without the cut and copies it to a staging texture that is read a few
frames later without waiting (`D3D11_MAP_FLAG_DO_NOT_WAIT`, no stall). `status_reflections=` (overlay line and debug capture) then reads
`applied (...); matte cut 0.80; sample 2304 pts: floors F %, reflective N (water/glass W, other O) = R % of floors; the score alone would
reflect M (P % of floors), the matte gate rejected K = Q % of those`. Before the first read-back: `no sample read back yet`.

WARP tests (`ssr_tests.rs`, "matte gate"): grey-brown earth under a moonlight tint (0.26, 0.27, 0.35), grey haze earth and moonlit haze earth:
score above 0.5 and a reflection at cut 0 (the reported bug reproduced), score < 0.01 and weight < 0.001 at 0.8, nothing at 1; blue water and
cyan ice keep >= 95 % of their (boosted) weight and a score > 0.9, moonlit water keeps > 90 %; water under a sky band: sky pixels get
exactly 0 and the floor and the horizon row keep >= 95 %; the weight never grows with the cut; NaN / inf / negative inputs stay finite and
bounded; smart 0 is bit-identical for any cut; the telemetry counts all moonlit-earth floors as rejected and blue water as water with no
rejection. Every older SSR test runs with cut 0 (the code path they pinned is unchanged and bit-identical).

### Reading the heat map (debug level 5)

Tick "Debug reflectivity (heat map)". The scene is dimmed to 30 % and every floor pixel (the only ones the detector
scores) is coloured by its score: **blue 0, cyan 0.25, green 0.5, yellow 0.75, red 1**. Dim, untinted pixels are not
floors (walls, ceilings, sky, characters). Expect water, ice and polished floors red, grass, paths, sand and dirt blue (score exactly 0). If a surface you
want to reflect is blue, look at which term kills it: toggle `ssr_hue_filter` / `ssr_flatness` to 0 (live) and watch
the colour come back; if neither does it is the smoothness (texture or colour noise) or the specular cue. Reflections themselves
are still added underneath (the alpha of the reflection texture carries the score only while level 5 is active, so
normal rendering is unchanged).

Debug levels (`resolve::debug_level()`, the highest active wins): 0 off, 1 view (boosted red, occluded blue, halo green,
reflections magenta), 2 fill, 3 materials (alpha class: low blue, mid green, high red), 4 sun (shafts), 5 reflectivity.

Tuning the numbers needs the real game: depth-derived normals may be noisier on real floors than in the synthetic test,
in which case raise the flatness ramps; if textured but flat decks lose reflection raise the roughness ramps.

## Water and glass (`ssr_water_boost`, `water_depth_look`)

On top of the reflectivity score, receivers the score calls water or glass get better reflections. **The score itself is not changed**:
a pixel is a water receiver only if it already has a score above 0 (dry terrain, dirt, sand, grass score 0 and are never touched), and
only if its colour class is the cool one (`water_prior` = the cool-and-coloured branch of the class prior WITHOUT the hue-filter slider,
vetoed by vegetation and warm earth, 5-tap mean colour, smoothstep(0.5, 0.9) of the prior): `wat = gate(prior) x score`. Everything
below is blended in by `wk = wat x ssr_water_boost` and needs `ssr_smart` > 0 (it is the score that finds the water).

| part | what |
|---|---|
| Fresnel | Schlick `F0 + (1 - F0)(1 - n.v)^5` with F0 = 0.02 for saturated (water) .. 0.04 for pale (glass / ice) receivers (`smoothstep(0.12, 0.30, saturation)`), times `1 + boost`, never below the old term, clamped to **0.85** (`FRES_MAX`); the old term stays the floor, so a steep view is not weaker than before. Measured on the test floor: gain over the old weight 1.67 (steep) to 2.31 (grazing) at boost 1; the largest weight 0.47 vs 0.20 before |
| tint | the reflected colour is multiplied by `lerp(1, receiver hue / luminance (clamped 0.5..1.8), 0.35 x wk)`: reflections look in-medium; the weight (alpha) is untouched |
| screen edge | a hit within 10 % of the border, or a ray that leaves the picture upwards, blends towards the sky found in the same column (the encoded colour of the first pixel from the top whose depth is invalid, searched over the upper 30 % of the picture, 6 probes), with the weight of the existing sky reflection `c2.z` (0.35); no sky in the column = nothing is invented (tested). The fade of the origin near the border stays. Note: in this camera model a flat floor sends no ray out of the picture (its screen path only approaches the mirrored row), so the filled band is the "reflection near the border" case; sloped water / ripples do exit |
| depth look | `water_depth_look` x up to 15 % darkening of the receiver where the reflection is strong (`saturate(fresnel / 0.3)`), written as a NEGATIVE linear rgb in the reflection texture (`-receiver linear colour x 0.15 x look x wk x floorness x ...`) so it never goes below 85 % of the receiver; the light-adjust substitute adds the reflection and now clamps the sum at 0 (`lin = max(lin, 0)`, a no-op for non-negative input, NaN becomes 0). Needs the boost (it is part of it) |
| calm vs ripple | the trace writes a second target (half size R16G16F, `t2` of the blur passes: x = `wk`, y = ripple = `saturate((1 - smoothness) x 2)` from the rough term of the score). The blur passes read it: calm water (ripple 0) keeps 30 % of the outer taps (a sharper mirror), ripples spread the taps up to 2x (a softer one); where the map is 0 the taps and weights are exactly the old ones. Measured (80 % to 20 % edge width in half pixels): calm 3.9 -> 2.7, rippled 3.3 -> 4.2 |

**Bit-identical for dry pixels**: every new term is behind `wat > 0` / `wk > 0` (and `boost > 0`); the water map is zero there, the blur arithmetic
is unchanged, and the added darkening is `+ 0`. A test renders brown, green, charr-path, sand, grey polished, metal and noisy floors with boost 0.3 / 0.6 / 1 and
depth look 1 and compares every output texel to boost 0 **by bits**. `ssr_water_boost` 0 is the old behaviour exactly (also for water, whatever the
depth look says).

| key / overlay | range | default | meaning |
|---|---|---|---|
| `ssr_water_boost` / "Water / glass boost" | 0..1 | 0.6 | Fresnel, tint, sky at the edge, calm / ripple blur on water and glass receivers; 0 = off, old behaviour exactly |
| `water_depth_look` / "Water depth look" | 0..1 | 0.3 | darkening of such water by up to 15 % x this; 0 = off |

Both live (c4.z / c4.w of the trace). Debug capture: `ssr_water_boost=`, `water_depth_look=`, and `status_reflections=` now ends with
`applied (water boost B, water depth look D, smart S)`.

To check in game: the debug view 5 (reflectivity heat map) shows which pixels have a score; the water boost only acts on the red / yellow ones that are also
cool and coloured. Lakes: stronger, bluer reflections that get stronger towards the horizon, a sharper mirror on calm parts, no dark band at the picture
edge; with the look on, deep parts a little darker. Tuning constants are at the top of `ssr.hlsl` (`WATER_F0`, `GLASS_F0`, `FRES_MAX`, `WATER_GAIN`, `WATER_TINT`,
`WATER_DARK_MAX`, `RIPPLE_SPREAD`, `CALM_OUTER`).

## Log lines

`ssr: configured (...)`, `ssr: shaders compiled`, `ssr: reflection targets WxH (scene WxH)`,
`ssr: first frame applied (...)`, and once each when it skips: no depth, no scene colour at slot 0, scene/depth size
mismatch, setup failure.

## Tuning in the game

* Too faint: raise `ssr_strength` (the main lever). Too much haze on distant ground: lower it; the grazing
  Fresnel is what lights far floors, and the sky weight is `SKY_WEIGHT` in `ssr.rs`.
* Reflections of things far from the floor missing: raise `ssr_range`.
* Sloped ground may flicker in and out near the 0.80 normal threshold; widen/narrow the `smoothstep` in the trace.
* Verify with debug view 1 that the magenta covers floors and only floors.

## Tested on WARP vs needs the real game

WARP (synthetic depth following the shader's camera model): the reflected object appears at the mirrored screen
position (gradient colour read back at three floor heights, within 0.08 of the expected height); a vertical wall,
the object's face, a ceiling and a tilted wall get nothing; a floor under a sky reflects the sky colour
faintly (weight <= 0.35), a black sky reflects nothing; rays leaving the screen fade smoothly (no step larger than
35 % of the peak, nearest rows below 10 %); the range slider decides whether a far object is reached; strength 0
and unbound `t8`/`b7` are bit-identical to no reflection in light-adjust; the debug tint; pipeline state and
reference counts restored; resize recreates the targets (odd sizes round up). Materials (the floor sample rows are v = -0.6..-0.8, away from the object base, because the wide roughness and the flatness rings
legitimately react to a tall object touching the floor): green and warm brown floors get < 10 % of the plain reflection
(measured 3 %), grey, teal-grey and cyan floors keep it (100 %), a rust patch on a teal floor loses it (3 %) while the teal
side keeps 100 %, coarse 12 px block noise loses it (10 %) where the narrow window barely sees it, a bumpy depth (1.5 % relief)
loses it (3 %) with `ssr_flatness` monotonic 0 -> 1, smart 0 is bit-identical whatever the colour, hue and flatness values,
class gating gives exactly 0 with the class bit cleared, debug level 3 tints blue / green / red, debug level 5 stores the
score in alpha (teal 1.0, green / brown 0.03, no heat on the object face) and the light-adjust heat map goes blue / cyan /
green / yellow / red over a dimmed scene. The pre-existing SSR tests run with classes 7 and smart 0. Positive-evidence score (debug alpha): brown, dark brown, dark desaturated, grey-brown mud, charr path, noisy dirt, sand, grass and bumpy brown all 0.000 (reflection weight < 0.001); teal, cyan, blue water and a neutral polished floor 1.0; bright smooth grey metal 0.48; identical on repeated frames; +-0.2 % input noise moves teal / polished by 0. The rust-patch test now reads the teal columns beyond the wide window (16 px) from the patch edge, because a material boundary is not coherent.

Needs the real game: whether the engine's depth-derived normals are clean enough on real floors (the blur hides
most noise), the look and the default strength, the cost at 3440x1440 (traces at half resolution, 32 steps),
whether the depth is correct while the light-adjust draw runs, and that `t8`/`b7` survive until that draw (they
are bound inside the same hook, right before the engine's draw).

## Risks

* Unrolled-camera assumption: with a strongly rolled camera "up" in view space is not world up.
* Reflections cannot show things that are off-screen or hidden behind other geometry (usual SSR limit); they fade
  rather than pop.
* Any horizontal surface qualifies (tabletops, roofs seen from above), not only wet ones; the low Fresnel floor and
  the default strength keep it subtle.
