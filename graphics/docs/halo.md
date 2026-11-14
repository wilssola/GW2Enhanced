# Light halo

Status: implemented, unit and WARP tested. NOT yet seen in the real game (see "What only the game can show").

## Problem

Street lamps, lanterns and other light sources had no glow. The game's own bloom (ps f5cd4832 chain, 860x360,
about 20 px radius) is too small and weak for it, and the bloom slider recolours everything. The halo is a separate,
controllable HDR glow that keeps the art style.

## Where it lives (the architecture rule)

Per `data-path.md`, an effect must live inside a substituted engine shader. The halo is computed into OUR OWN
textures (safe: the engine only overwrites its own targets) and added by `light_adjust_hdr.hlsl`:

```
datapath::on_draw (KIND_LIGHT_ADJUST, game context)           once per frame, right before the light-adjust draw
  halo::prepare:  scene colour = PS SRV at slot 0  --8 passes-->  G (RGBA16F, 1/4 res)
  datapath::Resources::bind:  t6 AO, t7 G (or a 1x1 black), b6 (two float4)
light_adjust_hdr.hlsl:  lin = scene^2 ; lin += G * strength ; bleach ; remap ; boost ; encode
```

Added after the AO and the gamma decode and BEFORE the bleach and the exposure remap, so the remap, the bloom,
the combine and the tone shoulder all see the halo. Unbound t7 reads zero, strength 0 skips the read: the
original-equivalence tests bind nothing and still pass.

## What it detects (no knowledge of which pixels are lights)

In linear light, per full-resolution pixel: `m` = largest channel, `chroma = (max - min) / max`.

```
e = max(m - 0.6, 0) * saturate(2.5 * (chroma - 0.25)) saturated bright things: fire, lamps, magic
  + max(m - max(1.6, 1.6 * local_mean), 0)            neutral spots above 1.6x paper white; big bright areas (sky) do not
e = min(e, 8)                                         one hot pixel cannot flood the screen
source = linear * e / m                               keeps the hue of the pixel
```

A lit surface is not a light: a white coat or armour in the sun reaches about 1.0 .. 1.4x paper white (slightly warm, chroma
0.1 .. 0.2) and gets no halo, so a white character in the light has no aura. Lamps, fire and magic are far brighter or clearly
coloured (test `a_sunlit_white_character_on_a_dark_floor_makes_no_glow`).

`local_mean` is the mean of `min(m, 4)` over a 16x16 block (1/16 resolution), bilinearly interpolated. Scene values
are tamed first (negative and NaN become 0, the encoded value is capped at 8).

## The passes (`shaders/halo.hlsl`, `src/halo.rs`)

All use `Load` only (no sampler: the game's sampler state is never touched), one full-screen triangle each.

| # | pass | target | what |
|---|---|---|---|
| 1 | `ps_mean` | R16F 1/16 | block mean of the clamped largest channel |
| 2 | `ps_source` | RGBA16F 1/4 | source energy, 8x8 pixel tent per texel (energy conserving, no flicker; 4 loads per pixel) |
| 3-5 | `ps_down` x3 | RGBA16F 1/8, 1/16, 1/32 | 13-tap downsample |
| 6-7 | `ps_up` x2 | 1/16, 1/8 | tent upsample, blend ONE,ONE into the level, weights `a3`, `a2` |
| 8 | `ps_up_final` | 1/4 (= G) | same with weight `a1`; alpha = local mean (debug view only) |

Upsample weights `a = min(UP_BASE * halo_radius, 1.2)` with `UP_BASE = [0.9, 0.75, 0.6]` (level 1 into 0, 2 into 1,
3 into 2): the contribution of level k is the product of the weights up to k, so it decays; the radius slider scales
how far the large levels reach. No Karis weighting: a Karis average removes isolated bright pixels, which is exactly
a small lamp; the per-pixel clamp of 8 and the 8x8 tent do the firefly protection instead.

The pipeline state is saved and restored by `StateGuard` (the same guard as the AO), t7 is freed before the first
draw. Targets are recreated when the scene size changes. Shaders are compiled once on a background thread
(`gw2g-halo-compile`); until then the halo is skipped.

## Parameters

| key | range | default | |
|---|---|---|---|
| `halo_strength` | 0..2 | 1.0 | multiplies G in the shader; 0 disables the whole feature (no passes at all) |
| `halo_radius` | 0.5..2 | 1.0 | scales the upsample weights (reach and far tail) |

Both overlay sliders apply live (atomics -> constant buffers, no recompile). Overlay line `Halo: ...`; the debug
view checkbox now also paints the pixels that count as halo sources GREEN.

Log lines: `halo: configured`, `halo: shaders compiled`, `halo: targets for scene ...`,
`halo: first frame applied (scene WxH, strength, radius)`, and one `halo: skipped, ...` line per reason.

## Fail safe

The scene SRV (slot 0) must be a 2D texture of the same size as the pass's own render target, single sample, one
mip, one slice, R16G16B16A16_FLOAT or B8G8R8A8_UNORM, at least 16x16. Otherwise: log once, no halo, black t7. A
shader compile or object creation failure switches the feature off for the session (`failed (see log), off`).
Everything runs inside the `catch_unwind` of `datapath::on_draw`.

## How to tune

1. Turn the debug view on (green = counted as a source). If lamps are not green the detector misses them; if walls or
   sky are green the thresholds are too low (constants in `halo.hlsl` `source_of` AND the copy in
   `light_adjust_hdr.hlsl` `halo_source`: keep them identical).
2. Strength first (0.5..2), then radius. A flat saturated area gets about `1 + a1 (1 + a2 (1 + a3))` times its own
   excess added (about 3.0 at defaults): a big orange wall with m = 0.8 brightens. Lower the strength if so.
3. A small source is weak by nature: its energy is spread over the area of the halo. A 2x2 pixel orange spot
   (linear 3, 1.2, 0.2) peaks at 0.32 red at the source and is at 0.008 four texels (16 px) out; a lantern of
   8x8 pixels carries 16 times more energy. If the scene is clipped to 1.0 (8-bit scenes) the source energy is at
   most 0.4 for a saturated colour and the halo is faint: that is the reason to want float16 scene targets.

## Tested on WARP (`src/halo_tests.rs`: 10 GPU + 5 host tests; plus 3 in `hdr_shaders_tests.rs`, 2 in `config.rs`)

* single saturated spot: halo mirror symmetric (both axes and the diagonal), never rising with the distance, keeps
  the hue, fades out;
* a single hot pixel of 60 and of 60000 give the same halo, far corner dark;
* uniform neutral 0.8 / 1.0 / 1.5 / 3.0 areas: exactly no glow; a uniform saturated area glows (the detector is alive);
* neutral spot 3.0 on a 0.5 area glows (1.78), on a 1.5 sky glows less (0.48), full-screen white: none;
* radius 0.5 / 1 / 2 widens the halo (12 / 44 / 76 texels above 0.02);
* NaN, inf, negative scene values: halo finite and non-negative; a BGRA8 scene works;
* pipeline state, SRV 0/1, blend, RT, viewport, topology restored; reference counts equal; t7 free; ignore flag off;
* resize (and odd sizes such as 100x75) recreate the targets;
* light-adjust: strength 0, unbound t7 and black t7 are the identity; strength adds linear light monotonically; the
  debug view paints sources green, not grey, not uniform bright areas, not while the halo is off.

## What only the game can show

* That slot 0 of the real light-adjust draw really is the full-size scene colour (validated at run time, logged if not).
* How big the source energy really is in the game's scene (float16 values above 1 or clipped?), i.e. whether the
  default strength is visible on lamps. Use the green debug view and the strength slider.
* The cost: about 4 texture loads per scene pixel (source pass) plus 1 (mean), the rest at 1/4 resolution or less.
  Not measured anywhere yet.
