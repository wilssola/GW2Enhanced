# Sun shafts (M3): volumetric light as screen-space god rays

Status: implemented, WARP tested (21 tests). Not yet seen in the real game.

## What it does

When the sun is on the screen (seen through gaps in trees, structures or other geometry), light rays radiate from
it across the scene and are added to the picture. Only the sun emits: the rest of the sky, however bright, and bright
clouds away from the sun give no rays, and an evenly bright sky (overcast, haze, no distinct sun) gives none at all. Everything is estimated
from the picture: the engine's shadow cascades and sun direction are not known (the world shaders are not in the
exe). It is a visual effect only; nothing is hidden or revealed.

## Passes (`src/shafts.rs`, `shaders/shafts.hlsl`), all in the mod's own targets

Run once per frame at the light-adjust draw (`datapath::bind_current` calls `shafts::on_light_adjust`). Scene
colour = pixel shader SRV in slot 0, depth = `ao::depth_resource()`. State is saved and restored with `StateGuard`;
the module's own SRVs (t10..t19) and sampler (s15) are unbound afterwards.

0. Depth statistics. `ps_dtile` (16x16 RGBA32F): per tile the maximum VALID depth, the count of invalid samples and the
   sample count (a 2-pixel stride is enough). `ps_dreduce` (1x1): the frame's max valid depth and the invalid fraction
   (bound as t17 in the following passes).
1. `ps_mask` (1/4 size, RGBA16F): 4x4 taps per quarter pixel. A tap is sky when its linear depth is invalid
   (<= 0, NaN, huge), OR, when fewer than 2 % of the depth samples of the frame are invalid, when `depth >= 0.98 *
   max depth` (the sky is a finite far plane, not invalid depth). The far-plane rule is off for a frame with many
   invalid samples: a flat scene with an invalid-depth sky would otherwise turn its farthest ground into sky.
   Constants `FAR_RATIO`, `MAX_INVALID_FOR_FAR` (`shafts.rs`, cb c3). Emitter = sky colour (linear = encoded^2, capped at 8) times `smoothstep(0.25, 0.7, luminance)`:
   only bright sky emits, geometry is the occluder (black). Alpha = sky fraction.
2. `ps_reduce_a` (16x16, two RGBA32F targets): per tile `w*rgb` and `w` with `w = lum^3` (for the tint), the sum
   of the emitter luminance, the sky count and the tile's peak luminance.
3. `ps_core` (16x16, RGBA32F): the sun core. Each tile first reduces the 256 tiles of pass 2 to the frame's peak
   luminance `P` and the mean emitter luminance of the sky `M`. A pixel belongs to the core with the weight
   `smoothstep(lerp(M, P, 0.5), P, lum)`: only the pixels in the upper half between the mean sky and the peak.
   Sums of `c*q`, `c`, `c*|q|^2`, with `q` the position in screen-height units (x times the aspect ratio).
4. `ps_reduce_b` (1x1, three RGBA32F targets): the sun state and this frame's measurement. Position = centroid of
   the core; spread = RMS radius of the core (screen heights); tint = `lum^3`-weighted colour at unit luminance.
   Confidence = bright-sky amount x contrast `smoothstep(1.3, 2.0, P / M)` (the peak must stand out from the mean
   sky; an evenly bright sky has a ratio near 1) x compactness `1 - smoothstep(0.08, 0.16, spread)` (bright clouds
   spread over the sky are not a sun) x believable sky share (all "sky" means the depth is unusable) x a small edge
   penalty. The state texture also keeps the sky fraction (s0.w) and the frame's max depth (s1.w). The measurement
   (t19 of the next pass, not blended) holds `P`, `M`, the emitter radius `R = clamp(3 x spread, 0.08, 0.16)` screen
   heights and the spread. The state is blended with the previous
   one (ping-pong pair of 1x1 textures): confidence rises 20 % per frame, falls 10 % per frame, position and tint
   follow at 25 % per frame, and they snap to the measurement only when there was no sun before. All on the GPU,
   no CPU read-back in the frame path.
5. `ps_shafts` (1/4 size, RGBA16F): radial blur from each pixel towards the sun (56 samples, decay 0.965 per
   sample, per-pixel interleaved-gradient jitter, linear clamp sampler), length = `shafts_length` x distance to the
   sun. A sample emits only near the sun: its emitter luminance times `1 - smoothstep(R, 2R, distance to the sun)`
   (distance in screen heights). The sun disc and its glow emit; the rest of the sky and clouds elsewhere do not, so
   the shadows come from the geometry in front of the sun's glow. Output rgb = summed luminance x sun tint x
   confidence x 1.5, in linear units. Alpha = the sky share of the pixel (used by debug level 4 only).

`light_adjust_hdr.hlsl` (one block, after `lin = c.rgb*c.rgb`, i.e. after the AO factor and before the exposure remap):
`lin += shafts(t9) * hp.x(b8) * (1 - smoothstep(0.35, 1.0, max channel of lin))`. Pixels that are already bright
get less and nothing at linear 1.0, so lit surfaces do not wash out. Guarded by `hp.x > 0`: unbound t9/b8 read zero
= off, identical to the original (pinned by a test, also with NaN in t9).

The overlay status line reads the 1x1 state from a ring of three staging textures with `Map(DO_NOT_WAIT)`; it never
waits (the value is 2 to 3 frames old) and is only for display.

## Parameters (config.ini, overlay sliders, applied live)

| key | range | default | meaning |
|---|---|---|---|
| `shafts_strength` | 0..2 | 0.6 | `b8.x`. 0 turns the feature off completely: no passes, the depth consumer (`ao::DEPTH_FOR_SHAFTS`) is released, t9/b8 are unbound |
| `shafts_length` | 0.3..1.0 | 0.7 | fraction of the way to the sun that each ray samples |

Debug view (overlay, level 1) draws a magenta cross at the estimated sun position in the shaft texture.
Overlay line: `Sun shafts: sun found at (u, v), confidence c, sky p %, max depth D` / `sun not found (no bright sky) ...` /
`skipped: ...`.

### Debug sun (level 4) and depth diagnostics

* Overlay checkbox "Debug sun" (`resolve::set_debug_sun`; `resolve::debug_level()` returns 4; 5 = SSR reflectivity wins,
  4 beats 3/2/1). The light-adjust shader tints the sky cyan (alpha of t9), the shaft light yellow, and the shafts pass
  draws the magenta cross (levels 1 and 4). The shafts must be on (strength > 0): the tint reads t9. While on, the log
  gets `shafts: sun (u, v) confidence c, sky fraction f, max depth D` at most once per second. Not saved in the config.
* Overlay button "Log depth stats" (`src/depthstats.rs`): at the next light-adjust draw the linear depth and the scene
  colour are read back once (a one-off stall) and a `depthstats:` block is written to the log: counts of depth <= 0,
  NaN/inf and > 1e30, min/max, percentiles 1/5/25/50/75/95/99, a 64-bin log2 histogram, the share of pixels at
  >= 0.99 / 0.9 of the max, the average scene colour and alpha of the far and of the invalid pixels, and a 9x5 grid
  of (depth, rgb, alpha) samples. Use it outdoors with visible sky, indoors, and looking at the sun to see how the
  engine encodes the sky.

## Tuning (constants in `shafts.rs` / `shafts.hlsl`)

* Emitter threshold: `smoothstep(0.25, 0.7, lum)` in `ps_mask`. Lower it for sunsets, raise it if grey cloud glows.
* The sun rule (constants at the top of `shafts.hlsl`): `CORE_LEVEL` (0.5, where the core starts between the mean sky
  and the peak), `CONTRAST` (1.3..2.0, peak over mean sky), `CORE_SPREAD` (0.08..0.16 screen heights),
  `EMIT_SCALE` and `EMIT_RADIUS` (emitter radius = 3 x spread, clamped to 0.08..0.16 screen heights, fading out to
  twice that). Rays too short around a large sun: raise the radius; a bright cloud near the sun emitting: lower it.
* Rays too strong: lower `GAIN` (1.5) in the shader or the default strength. Too short/long: `shafts_length`, `DECAY`.
* Sun jumps or lags: `CONF_RISE`, `CONF_FALL`, `FOLLOW`.
* Confidence thresholds are in `ps_reduce_b` (`smoothstep(0.0005, 0.01, meanlum)`, contrast, core spread, sky share 0.85..0.97).

Logs: `shafts: configured`, `shafts: shaders compiled`, `shafts: first frame applied`, and one line for each reason it
skips (still compiling, depth not available, scene colour not usable, targets could not be set up).

## Limits

* The sun is the centroid of the brightest core of the sky that is ON screen. A sun just outside the picture is not
  seen (its glow alone is usually not compact or contrasted enough); a sun behind the camera gives nothing. There is no
  estimate of an off-screen position.
* The sun is told apart by brightness only. A single compact cloud that is much brighter than the rest of the sky is
  taken for the sun; when the sun is only slightly brighter than the sky around it (a clipped picture with a white
  sky, or a gap that shows little more than the sun), the contrast is low and there are no or weak rays.
* Two bright cores far apart (the sun and a bright cloud of about the same luminance) give a wide core and so a low
  confidence.
* Rays are added to everything, including the sky itself (sky is bright, so the bright-pixel falloff removes most of it).
* The quarter-size result is sampled with the game's `s0` sampler in the light-adjust shader.

## Tested on WARP (`src/shafts_tests.rs`, `src/shafts.rs`, `src/config.rs`)

Compile of all entries; rays between columns (at the same distance from the sun, the ray down the slit in which the
sun is seen is about 2x the rays crossing a column in front of the glow; the light falls off with distance from the
sun and to both sides); ray length parameter; no sky = nothing; dim sky = nothing; all-sky = no confidence; an
evenly bright sky band (gradient and ripple, no distinct sun) = no confidence and no light; a distinct sun in a
bright sky with a bright cloud elsewhere is found on the sun and rays come only from it (nothing through the cloud,
nothing from the bright sky away from the sun); the 1x1 estimate lands on the blob (within 0.04) and glides when it moves (no step above 30 % of the
jump); confidence fades without pops and reaches exactly 0; debug cross; state/refcounts/ignore flag restored and
t10..t19/s15 left unbound, t9 untouched; resize recreates the targets (odd sizes round up); lazy read-back works; depth statistics (max, invalid fraction, garbage depth); finite far-plane sky gives shafts;
flat bright ground with an invalid-depth sky is not sky; sky fraction and max depth in the state; alpha = sky share;
debug level 4 tints (cyan sky, yellow light, nothing when unbound); `resolve::debug_level` precedence; the depth
diagnostic maths and its read-back on WARP (`src/depthstats.rs`);
light-adjust identity with strength 0, with NaN in t9, and with t9 unbound; bright pixels spared; config
parse/clamp/save/roundtrip; depth consumer bit set/released.

## Only verifiable in the real game

* That the depth target still holds the frame's depth at the light-adjust draw (it is captured at the depth pass and
  read later; the AO reads it earlier, at the effects pass). If it does not, the log says
  `shafts: skipped, the linear depth ...` or the sky detection is wrong (the all-sky guard then keeps confidence low).
* Look, strength and thresholds on real skies (sunset, overcast, fog), and the cost at 3440x1440 (the passes are
  at quarter size, the reductions are small; expected well under 0.5 ms).
* That `s0` of the light-adjust draw filters the quarter-size texture acceptably.
