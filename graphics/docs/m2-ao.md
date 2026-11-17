# M2: screen-space ambient occlusion

Status: built, unit and WARP tested.

**Update (data path):** the AO is no longer multiplied into the scene colour by an extra draw. In the real game that
draw (like the old resolve pass) was overwritten by the engine before it could show. The visibility is now computed
exactly as described below, but written to its own texture, and applied INSIDE the substituted light-adjust shader
(`ps#f5cd4832`, input `t6`). See `data-path.md`. The "Where" and "Decision" sections below describe where the
visibility is computed (still the start of the effects pass), not where it is applied.

## Decision

The AO multiplies the scene colour **at the start of the effects pass** (#9 of the reference frame), reading the
engine's **full-size R32_FLOAT linear depth** (#4). That is after the opaque pass and the engine's light accumulation
and before transparents/effects, light adjustment (#13), bloom (#14), combine (#15), SMAA and the resolve, so the
engine exposure, our HDR bloom and the tone map all see the darkened scene. Effects and particles drawn afterwards
are not darkened.

Why there and not later (right before the resolve): after the combine the AO would also darken bloom, the
saturation/tint grade and transparents, and the depth no longer matches the colour (effects are not in the linear
depth). Why not earlier (inside the opaque pass): the linear depth does not exist yet.

## Engine facts relied on (from [frame-structure.md](frame-structure.md) and [engine-post-chain.md](engine-post-chain.md))

- #3 opaque: RT0 full-size scene colour, full-size depth, hundreds of draws.
- #4: one draw, RT0 3440x1440 `R32_FLOAT` = the linear depth the engine light pass (#8, `625d33af`, t1) reads.
  The engine shaders read it, so it is created with a shader-resource bind (the mod checks the bind flag).
- #5 is the half-size `R32_FLOAT` (1720x720); #6/#7 are the engine SSAO at half size and the R16_FLOAT AO upsample.
- #9 effects: same RT0 and depth as the opaque pass, after the linear depth.

## How the stages are found (no indices, no hashes)

`classify::ao_stage(frame_so_far, cur, screen)` runs when a pass starts (from the recorder, only while the AO is on):

- `DepthLinear`: the first full-size `R32_FLOAT` pass after a pass of the opaque shape (full-size scene colour +
  full-size depth + at least one draw); the opaque pass is the LAST pass of that shape before it (the classifier rule).
  The mod keeps a reference to that render target texture for the frame. There is no minimum draw count: it was 20, and
  a camera looking at the sky (the character, the sky, little else in the opaque pass) then took the effects or the UI
  pass for the opaque one, found no linear depth and turned every effect off in those frames (`frame does not match: no
  linear depth pass` in the log; the fur vanished at some camera angles).
- `Effects`: after the linear depth was seen, the first pass whose RT0 and depth equal the opaque pass. The AO runs
  just before the original `OMSetRenderTargets` call that starts it, on that binding RT0.

Both only fire on the game context and only while the classifier gate is open (`gate::switches().resolve`, the same
switch as the resolve) and `ao_strength > 0`. If no usable depth was seen in the frame (wrong size/format, no
shader-resource bind, no such pass), the frame is skipped and one line is logged. The scene colour must be a
single-sample full-size `R16G16B16A16_FLOAT` or `B8G8R8A8_UNORM`.

## Algorithm (`shaders/ao.hlsl`)

Three full-screen draws, all fed by `Load` (no samplers):

1. `ps_ao`: view-space position from depth (assumed vertical half fov tan 0.5), normal from the smaller one-sided depth
   difference per axis, 16 normal-oriented taps on a per-pixel rotated spiral; a tap occludes by
   `saturate(dot(N, v)/|v| - bias)` times a range falloff `1 - |v|/R`. Output: visibility in an R16_FLOAT target at scene size.
2. `ps_blur_h`: 9-tap horizontal blur weighted by depth similarity (3 % of depth).
3. `ps_apply`: the same blur vertically, then the factor `1 - 0.5 * saturate(strength * (1 - visibility))`, drawn with
   blend `dest * src` on RGB only (alpha, which carries the material mask the combine reads, is kept).

Scale invariance: the radius is a fraction of the pixel depth (`0.03 * ao_radius`), and only depth ratios are used, so
the unit of the engine linear depth is irrelevant (tested: depth/1000 gives the same image). The only assumption is the
field of view; a wrong value changes the look, not the validity. Invalid depth (<= 0, NaN, inf, outside the image) is
never darkened and never used as a tap.

Visual only: the factor is in [0.5, 1], so AO can only darken, never below half, and never touches alpha.

## Fog and water release

The AO sees the depth of the ground below the water and behind the haze, while the engine has already blended that
fog into the pixel colour, so the AO painted dirty streaks on water and fog. The first fix released the AO on bluish
pixels only; the release below works for fog of any colour on any map, with no per-map data.

`fog.rs` / `shaders/fog.hlsl` estimate, every frame, at the light-adjust draw and only when the AO ran (four tiny draws
into the module's own targets, a 2x1 ping-pong state blended with the previous frame, no CPU read-back, no stall):

- `zref`: the largest valid linear depth of the frame (16x16 tile maxima, then 1x1).
- `F`: the fog colour, the average encoded scene colour of the far pixels: invalid depth (sky) plus valid depth
  >= 0.9 * zref. `F` is known only when at least 0.5 % of the pixels are far. The confidence rises fast (the first
  measurement is taken as is) and falls by 15 % per frame, so `F` never flickers on and off.

`light_adjust_hdr.hlsl` (t3 = state, t4 = depth) turns them into a fog amount `a` per pixel and multiplies the occlusion
by `1 - a`: `a = max(closeness * smoothstep(0.15, 0.5, z/zref), smoothstep(0.55, 0.95, z/zref) * 0.6)` with
`closeness = 1 - smoothstep(0.06, 0.30, distance(colour, F))` in encoded space, all scaled by the confidence. Only depth
ratios are used, so the unit of the depth does not matter. Sky (invalid depth) keeps visibility 1.

Fallback: when `F` is unknown (too few far pixels, depth or scene colour not usable, the module failed or is still
compiling) t3/t4 read zero and `a` is 0: only the blue-water rule (`haze`, right after the lum release in the shader)
applies, exactly as before. That rule stays, also as a safety net for blue water when `F` is known but differs.

Limits: it is an estimate from the picture. A dark mountain range that is the farthest geometry becomes the "fog"
colour, so near-identical dark surfaces at mid distance lose their AO; the pure-distance term releases up to 60 % of the
AO on the farthest 40 % of the image regardless of colour. Underwater, where the whole view is fog, `F` is the water
colour and the release behaves the same way. The legacy multiply path (`ps_apply`, tests only) has no scene colour to
compare with and is unchanged.

## Settings

`ao_strength` 0..2 (default 1, 0 disables the pass entirely: no draws and no stage tracking) and `ao_radius` 0.25..4
(default 1). Both are atomics copied into a constant buffer each draw: the overlay sliders apply live and never compile.
The shaders are compiled once on a background thread started at init; until it finishes the AO is skipped.

## Known limits

- The engine own SSAO (#6/#7) still runs; whether #8/#9 use its output was not determined. They can stack in the
  creases the engine already darkens. If it is too dark, lower `ao_strength`; the follow-up is to find and
  neutralise the engine AO consumer.
- The linear depth excludes transparents, so AO does not account for them (and is not applied to them).
- Normals come from depth only (no normal buffer), so normal-map detail does not take part.
