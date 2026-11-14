# Aerial perspective

Status: built, unit and WARP tested; depth unit corrected from real-game depth captures (normalised depth, 1000 m per 1.0).
Seen working in the real game across a valley; it vanished with the camera tilted down: fixed with a held
horizon haze colour and radial distance (see "Haze colour"), not yet re-checked in game: see "In-game checks" 5.

A subtle atmospheric tint and contrast reduction on far scene pixels, toward the game's own fog / horizon colour. Purely
cosmetic and far-distance only: nothing nearer than 15 m is ever touched, at most 35 % of the haze colour replaces a
pixel, so distant enemies keep their visibility beyond a mild tint.

## Where it lives (data path)

Everything is in the light-adjust substitute (`light_adjust_hdr.hlsl`), applied to the linear colour right after the gamma
decode (so the bloom, the combine and the tone shoulder all see it). Inputs: the linear depth `t4` and the fog state
`t3` (3x1; the aerial reads texel 2, the haze colour; both from `fog.rs`, which now also runs when only the contact shadows or the aerial perspective are on, not only with
the AO), and `b10` (`float4(contact strength, aerial strength, half distance in m, depth_unit = metres per 1.0 of the normalised depth)`, bound by
`contact.rs`, which owns the constant buffer for both features). `aerial.rs` holds the settings, the rule in plain Rust
(`aerial::blend`, the reference the WARP tests compare the shader with) and the status text. Unbound or strength 0 = bit-identical.

## The rule

For a pixel at normalised view depth `z` (1.0 = far plane; `z >= 0.98` is the sky and untouched) at screen position `uv`, with the
RADIAL distance `r = z * ray(uv) * depth_unit` and `d = max(r - 15 m, 0)`:

```
ray    = sqrt(1 + (x * 0.5 * aspect)^2 + (y * 0.5)^2)   (x, y = uv in -1..1; tan(half vertical fov) = 0.5, the AO / contact assumption)
h      = 1 - 2^(-d / aerial_distance)             (0 at 15 m, 0.5 at aerial_distance beyond it)
blend  = min(aerial_strength * h, 0.35)
blend *= k * (1 - close(pixel, H)) * (1 - bluish)  (k = confidence of the haze colour H; bluish = the AO's water rule)
blend /= 1 + max(0, m - 1)                         (m = largest linear channel: lights, the sun, emissive keep their energy)
lin    = lerp(lin, H^2, blend)                     (H = an ENCODED colour, so H^2 is linear)
```

- **Distance** is radial, not the view depth: 1.0 at the centre of the picture, 1.12 at the top / bottom edge, about 1.43 in a
  16:9 corner. The rule is `aerial::blend_at` (`aerial::ray_factor`); `aerial::blend` is the same with ray 1.
- **Haze colour H** (texel 2 of the fog state, see "Haze colour" below). Confidence 0 = no haze.
- **Sky** (invalid depth, or the far plane: the engine writes 1.0 for the sky; everything `>= 0.98` counts, `aerial::SKY_DEPTH`)
  and anything nearer than 15 m (radial): untouched, bit for bit.
- **Dense fog**: `close(pixel, H) = 1 - smoothstep(0.06, 0.30, |scene colour - H|)` (encoded): a pixel that already has the haze
  colour (the game's own fog / mist) gets nothing, so the game's fog is never doubled.

## Haze colour (why the haze vanished when the camera looked down)

Seen in the game: haze across the valley, none at all with the camera tilted down at the same place (no sky in view).
Cause: H used to be the AO's fog colour F, the average colour of the pixels at >= 90 % of the frame's LARGEST depth. With sky in
view that is the sky (depth 1.0); with the camera tilted down it is the far geometry itself (debug captures:
`sky/invalid 0.00 %`, `largest depth 0.193`), so the far trees were lerped toward their own colour, and the colour-match release
(`close(pixel, F)` with `z / zref` near 1) set the blend to 0 on them: no haze at all.

Now `fog.rs` keeps a separate haze state (texel 2 of the 3x1 state; texel 1 `.z` = age in frames, `.w` = source):

| source | when | H | confidence |
|---|---|---|---|
| `sky` (1) | sky pixels (depth invalid or >= 0.98) are at least 0.4 % of the picture (once in this state: down to 0.1 %, hysteresis) | the horizon band: per tile column (16 x 16 tiles) the lowest tile row holding sky (>= 2 % of its samples), averaged; followed at 10 % per frame | 1 |
| `held` (2) | the sky left the picture after being seen | the last sky colour, unchanged; it drifts toward the fallback at 0.02 % per frame (about a minute to go half way at 60 fps) only so a new map seen without sky eventually adapts | kept (1): never fades, no flicker |
| `fallback` (3) | no sky seen since start (or since the device was recreated), F known | a neutral, slightly cool grey from F's luminance: `saturate(1.1 L + 0.05) * (0.92, 0.97, 1.06)` | 0.6 (the haze is weaker while it is a guess) |
| unknown (0) | neither | - | 0: no haze |

Constants (`fog.rs`, not config keys): `SKY_ENTER 0.004`, `SKY_STAY 0.001`, `FOLLOW_H 0.1`, `HOLD_DRIFT 0.0002`, `FALLBACK_CONF 0.6`,
`BAND_MIN 0.02`. The game's own fog parameters are not read (no engine constant is known to hold them). The AO's fog release still uses
F (texel 0), unchanged.
- **Water**: the bluish rule, and in a fully fogged underwater view the colour match releases it too.
- **Cap**: 35 % even at strength 1, so contrast is reduced and a tint added, never a wash-out.
- **Height fog**: not done. The world height of a pixel needs the camera pitch and height, which the mod does not know (the
  projection is not read, the depth is view-space). Distance only. If a camera pose ever becomes available, a height falloff
  would be a factor on `h`.

## Settings (config.ini, overlay section "Aerial perspective", applied live)

| key | range | default | |
|---|---|---|---|
| `aerial_strength` | 0..1 | 0.35 | haze share at full distance, capped at 0.35; **0 = off: bit-identical** |
| `aerial_distance` | 50..5000 m | 400 | metres beyond 15 m where it reaches half of its amount (at the default strength: 100 m 5 %, 415 m 17.5 %, 1 km 29 %, 2 km 34 %) |
| `depth_unit` | 100..20000 m | 1000 | metres per 1.0 of the engine's NORMALISED linear depth (= distance of the far plane); also the contact shadows' metres. Overlay slider "Depth unit (m at the far plane)", live |

## Depth unit

The engine's full-size R32_FLOAT "linear depth" is **normalised**, not in world units. The debug capture's `status_depth=`
(debug-mode.md) measured in the real game: valid depths from about 0.0005 (grass right at the camera) to 1, the sky exactly 1.0,
p50 0.016..0.05. So `metres = depth * depth_unit`, and `depth_unit` is the far plane distance.

The old meaning (metres per unit assuming inches, default 0.0254) made every pixel nearer than 0.03 m: the haze never started
(`largest depth of the frame 0 m`) and the slider changed nothing.

How 1000 m was derived (an in-game capture, a charr standing on a forest path, camera behind it; the mod's own
view assumption tan(half vertical fov) = 0.5, the same as the AO and contact shadows, so the result is consistent with them):

* The charr's feet / legs at the screen centre column, y = 0.7: depth 0.0089. The hunched charr (about 2.0 m visible height)
  covers 165 of 720 rows = 23 % of the screen height; the screen height at distance D is 2 D tan = D, so D = 2.0 / 0.23 = 8.7 m,
  i.e. 8.7 / 0.0089 = **980 m per 1.0**.
* The ground plane of the same frame: 1 / depth is linear in the screen row (y = 0.9, 0.7, 0.5: 165, 112, 56; slope 272 per
  screen height, horizon at y = 0.29, camera pitch 12 degrees). For a camera h metres above the ground the slope gives
  depth_unit = 278 h; a third-person camera about 3.8 m up (charr pivot about 2 m + 8.7 m * sin 12 degrees) gives **1060 m**.
* A second, indoor-ish capture (horizon at y = 0.07, pitch 23 degrees) gives 130 h, about 500 m for a 4 m camera height: the
  same order. The default is 1000. An estimate of about 2600 (charr at 5 m with depth 0.0019) would put the near ground
  of these captures at 15..26 m in front of the camera, which does not match the pictures.

Migration: `depth_unit` values up to 2 (the old key, its default 0.0254, or any old manual value) and garbage read as unset and
become the default 1000; the next Save writes `depth_unit=1000`. Values from 3 to 100 clamp to 100.

If the haze starts visibly too near (the character's surroundings at 20..30 m already tinted) lower `depth_unit`; if a far
mountain range shows no haze at all, raise it. `status_aerial=` shows the largest depth of the frame as `0.214 = 214 m`.

## Debug capture and overlay

`status_aerial=` / overlay line: `on (inputs bound; ...), haze colour from the sky (horizon band) (r, g, b), confidence 1.00` (or
`haze colour held from the sky seen N frames ago (...)`, `haze colour fallback (no sky seen yet: ...)`, `haze colour unknown ...`,
`haze colour not read back yet`; read back from the GPU through a 3-deep staging ring, never waiting), `strength S, half distance D m
(radial) beyond 15 m, depth unit U m per 1.0 of the normalised depth, largest depth of the frame Z = X m` (with `(the sky / far plane, never hazed)` when it is 1.0; the largest depth is
measured by the sun shafts, `n/a` while they are off), or `off`, or `waiting for a scene frame`. The echoed keys are `aerial_strength=`,
`aerial_distance=`, `depth_unit=`.

## Cost (estimate)

No extra pass in the light-adjust shader: one depth load (shared with the contact shadows) and a few arithmetic operations, which
already runs full screen. The fog estimate (now 5 tiny draws: the sky tiles were added, plus a 48-byte staging copy for the status)
also runs when the AO is off but the contact shadows or the aerial perspective are on.

## Tests

`fog_tests.rs`: the haze colour is the horizon band (not the deep blue above it); held for 300 frames looking down with age counting,
confidence 1 and no per-frame jump > 0.001; the fallback before any sky and the smooth move to the sky colour; the 0.1 / 0.4 %
hysteresis. `hdr_shaders_tests.rs` (real estimator + light-adjust shader on WARP): the same far trees get the same haze (within 0.01)
with and without sky in view; a tilted-down ground plane matches `blend_at` with the radial distance at four pixels, more haze farther
up; twelve identical frames differ by <= 0.002. `aerial.rs`: `ray_factor`, radial proportionality.

## In-game checks

1. `status_aerial=` shows the largest depth: with sky in view `1.000 = 1000 m (the sky / far plane, never hazed)`. Sky colour must be
   identical with `aerial_strength=1` versus `0` (the far plane is excluded). A distant mountain (depth 0.2..0.5 in `status_depth=`) is
   200..500 m: the haze there is 10..25 % at strength 0.35. If the near forest (20..40 m) is already tinted, lower `depth_unit`.
2. A far mountain or a distant NPC at 300+ m is slightly hazier and lower in contrast; the character, nearby NPCs and enemies within
   about 15 m look exactly as before. The sky, the sun and light sources look as before.
3. In heavy fog (a foggy map) and under water: no change with `aerial_strength=1` versus `0`.
4. Distant enemies stay recognisable (the share never exceeds 35 %).
5. Pitch: look across a valley (status `haze colour from the sky`), then tilt the camera down until no sky is visible: the far trees /
   far hill keep the same haze (status `held from the sky seen N frames ago`, N counting up), no pop, no flicker when the horizon
   touches the top edge. Right after loading into a map looking down with no sky, the status says `fallback` and the haze is weaker
   until the sky is seen once.
