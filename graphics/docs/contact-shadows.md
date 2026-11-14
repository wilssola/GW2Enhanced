# Contact shadows

Status: built, unit and WARP tested. Not yet seen in the real game: see "In-game checks".

Small, soft shadows right next to surfaces (feet, rocks, eaves, props on the ground) that the engine's own shadow maps
miss. A short ray march in the engine's linear depth towards the light. Purely cosmetic, soft, capped, and released
wherever the AO is released.

## Where it lives (data path)

Nothing is drawn into the engine's targets. `contact.rs` runs once per frame at the light-adjust draw
(`datapath::bind_current` -> `contact::on_light_adjust`), in its own half-size targets, and binds the result as **`t22`**
of the substituted light-adjust shader (`ps#f5cd4832`), together with **`b10`** (see below). The shader upsamples it and
multiplies the diffuse light right after the AO factor. Slots: `t22` = contact visibility (R16_FLOAT, half scene size),
`b10` = `float4(x contact strength, y aerial strength, z aerial half distance in metres, w depth_unit = metres per 1.0 of the normalised depth)`;
`t10`/`t11` are the module's own scratch inputs while its passes run (cleared afterwards). Unbound, `t22` / `b10` read zero
= both features off, bit-identical (pinned by a test).

## Algorithm (`shaders/contact.hlsl`, two draws at half resolution)

1. `ps_contact`: view-space position from the depth (the AO's field of view assumption, tan half fov 0.5) and a normal
   from the smaller one-sided depth differences. The light direction `L` (view space, towards the light) comes from the
   CPU. A surface facing away from the light (`N.L < 0.08`, smooth ramp to 0.33) is not shadowed (it is in its own shade).
   20 steps along `L` over a world length of 0.35 m x `contact_length`, clamped to 1.2..5 % of the pixel depth, step
   spacing denser near the surface, the start of the steps jittered per pixel by **interleaved gradient noise** (a fixed
   screen-space pattern: identical for identical input, stable from frame to frame). At each step the ray point is
   projected and the depth there is read. It occludes when the scene surface is nearer than the ray point by more than a
   bias and by at most 2.5 ray lengths (**thickness limit**: a thin pole or a big near object in front of a far receiver
   casts nothing). The bias is slope scaled: `0.4 % of depth + 0.1 * length * (1 - N.L) + 0.5 * (depth change per pixel)`.
   The depth is stored per pixel but the ray point sits anywhere inside its pixel, which on a slanted surface is a depth
   step per pixel by itself, so the sampled depth is moved along the local tangent plane from the centre of the sampled
   pixel to the exact position of the ray point. That is what keeps a **flat plane at a grazing angle** from shadowing
   itself, at any light direction (tested on the ground, a wall and a tilted plane).
2. `ps_blur`: 4x4 depth-aware blur (weights fall to 0 over 3 % of depth difference). It removes the noise pattern without
   crossing depth edges. In a WARP test with the longest ray (a shadow band beside a box) the mean absolute second
   difference of the visibility falls from 0.077 raw to 0.012, and the strongest grid frequency (period 2 and 4,
   axes and diagonals) stays below 0.001 amplitude.

In `light_adjust_hdr.hlsl`: the half-size texture is upsampled **depth-aware** (2x2 taps weighted by the bilinear weight
and by how close the depth of the tap's source pixel is to this pixel's depth, 2 %), so a shadow never leaks over a depth
edge. Then, in the encoded colour: `cocc = saturate(strength * (1 - vis)) * (1 - fog)`, `factor = sqrt(1 - cocc)` (a
linear factor of `1 - cocc`), released on bright pixels (`lerp(f, 1, saturate((lum - 0.7) * 3))`, lights are not
ambient light) and on bluish water pixels, exactly like the AO.

## Light direction

`contact_sun = auto` (default):
- the **sun shafts' estimate** when it is found (`shafts::last_sun()`, confidence >= 0.05, only while `shafts_strength` > 0):
  the view ray through the sun's screen position, weight rising from 0 (confidence 0.05) to 1 (0.35);
- otherwise a **fixed direction from above** (view space `(0.30, 0.85, -0.43)`: up, a little right, behind the camera) at weight
  0.6; when the shafts report almost no sky in the picture (under 8 % of the pixels: indoors, caves, but also the common
  outdoor view looking down at the ground under trees) the weight falls to half of it, 0.3 (`NO_SKY_KEEP`). It used to fall
  to 0: in-game captures showed `weight 0.00` in exactly that view and the strength slider changed nothing.
  With the shafts off nothing is known about the sky and the weight stays 0.6.

`contact_sun = fixed` always uses the fixed direction at weight 0.6. The weight multiplies the shadow (visibility
`1 - weight * shadow`).

Limits: night is not detectable (a moonlit scene still gets the guess at 0.6, softly); the fixed direction is "up" in VIEW space,
so with the camera pitched far up or down it is not the world's up, only a plausible light; the sun estimate is the centre of the
bright sky, and the sun behind the camera (the common case) is never found, so most views use the fixed direction.

## Not applied

Under water and inside the game's own fog (the AO's release: `fog_amount` from `fog.rs` plus the bluish-water rule), the
sky (no depth), lights and bright effects (released above luminance 0.7), the UI (drawn after this shader). Alpha is not
touched. The strength can never exceed 0.8 (the darkening of the diffuse light is at most `contact_strength`).

## Settings (config.ini, overlay section "Contact shadows", applied live)

| key | range | default | |
|---|---|---|---|
| `contact_strength` | 0..0.8 | 0.5 | largest darkening of the diffuse light; **0 = off: no passes, no depth consumer, bit-identical** |
| `contact_length` | 0.25..2 | 1.0 | ray length (1 = about 0.35 m) |
| `contact_sun` | auto / fixed | auto | light direction source |
| `depth_unit` | 100..20000 | 1000 | metres per 1.0 of the engine's NORMALISED linear depth (shared with the aerial perspective; derivation and migration in aerial.md "Depth unit") |

The engine's depth is normalised (1.0 = far plane, written for the sky). The ray is `0.35 m * contact_length / depth_unit` depth
units, then clamped to 1.2..5 % of the pixel depth (at 9 m, the charr of the in-game capture, 0.35 m is 3.9 %: inside the clamps,
so the length is really metric now; with the old inch unit it was always the 5 % clamp). The far plane (`>= 0.98`) is the sky:
never shadowed, never an occluder. Thickness (2.5 ray lengths), bias and blur are relative to the ray or the pixel depth.

## Debug capture and overlay

`status_contact=` / overlay line: `applied, light from the sun estimate | fixed direction (a guess), weight W, mode M, strength S, length L (X m at depth unit U m)`
or the reason it is not running (`skipped: depth not found`, `compiling shaders`, ...). The settings are echoed
(`contact_strength=`, `contact_length=`, `contact_sun=`, `depth_unit=`).

## Cost (estimate, not measured in the game)

Two half-resolution draws (1/4 of the pixels): 20 dependent depth loads per pixel plus 8 for the normal, about 25 M loads at
3440x1440 (roughly 0.3 to 0.6 ms on a mid-range GPU), the 4x4 blur 16 loads (0.1 ms), and 4 more loads per pixel in the
light-adjust shader, which already runs full screen. Memory: two 1720x720 R16F textures (5 MB total).

## In-game checks

1. Debug capture `status_contact=` says `applied`; with `contact_strength=0` it says `off` and the picture is identical to before.
2. Standing next to a wall or a rock in the sun: a soft dark rim on the ground at the base on the side away from the light;
   no dark outline around the character against distant ground (halo), no dirt on flat ground, none in water or fog.
3. Rotating the camera: the shadows do not shimmer or show a dotted pattern (the noise is a fixed screen pattern, blurred 4x4).
4. `contact_sun=fixed` versus `auto` in a scene where the shafts find the sun (`status_shafts=` says `sun found`).
5. In the forest view of the in-game capture (no sky, sun not found) `status_contact=` must now say `weight 0.30`, not `0.00`, and
   moving `contact_strength` 0 -> 0.8 must visibly darken the base of trees and rocks and under the character's feet.
6. Check `depth_unit`: `status_aerial=` prints the largest depth of the frame as `Z = X m` (1.0 = sky = `depth_unit` metres).
