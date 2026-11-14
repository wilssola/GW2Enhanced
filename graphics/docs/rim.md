# Rim light and skin glow on characters

Status: implemented, unit and WARP tested. Not yet seen in the real game (see "In-game checks").

## What it does

A soft rim along the silhouette of characters, and a gentle warm glow of their dim-lit parts. Both are additive light in the
light-adjust substitute (`light_adjust_hdr.hlsl`, the data path of `data-path.md`), only on the pixels of a character mask.

* **Rim.** A pixel of the mask within `rim_width` px of the silhouette gets a relative luminance gain of at most
  `0.25 x rim_strength`. "Silhouette" = the nearest pixel (8 directions, rings 1..width) that is outside the mask, is sky, or is
  more than 6 % farther in the engine's linear depth (an arm in front of the torso is a silhouette too). The gain falls off
  smoothly with the distance, is weighted by a grazing term from the depth derivatives (`0.4 + 0.6 x saturate(2 x grazing)`; a facing
  surface still gets 40 %) and by how bright the background is: the background colour is the brightness-weighted mean of the
  linear scene colour at 8 directions x 2 radii (6 and 10 px) that lie OUTSIDE the mask, and the rim fades to nothing against a dark
  background (`smoothstep(0.02, 0.25, background luminance)`). The added light has the pixel's own chromaticity tinted 35 % towards
  that background colour, so the pixel's hue is kept (chromaticity moves by about 0.02, tested < 0.03).
* **Subsurface glow.** A warm (1.0, 0.45, 0.2 at unit luminance) lift of at most `0.08 x subsurface_strength` of the pixel's
  luminance, strongest in the dim part of the mask (`1 - smoothstep(0.08, 0.45, linear luminance)`) and stronger near the edge
  (`0.3 + 0.7 x edge proximity`: thin parts). Nothing on bright pixels.
* **Limits that make it safe.**
  * the total relative gain is capped at +25 % (rim and glow together);
  * no channel is pushed above `max(its own pixel maximum, 1.0)` (never above SDR white or the pixel's own peak; the display peak is
    applied later by the final pass as for everything else);
  * released on dark pixels: `smoothstep(0.004, 0.03, linear luminance)`, so black and almost black get exactly nothing and a dim
    pixel an absolute lift of < 0.001. A night scene (dark background) gets no rim at all. **It never makes a character easier to
    see in the dark.**
  * released in the game's own fog and in water by the same rule as the AO (`(1 - fog amount) x (1 - bluish water rule)`, from `fog.rs`
    / the shader's own `haze`); a blue pixel is released like water (blue armour loses the rim, on purpose, cosmetic only);
  * stealth / invisible characters: they are drawn with an alpha blend, and a draw whose blend state on render target 0 is an alpha blend
    (`SRC_ALPHA`, `BLEND_FACTOR` source, or `INV_SRC_ALPHA` / `INV_BLEND_FACTOR` destination, premultiplied alpha included) is NOT put
    in the mask (`blend_is_transparent`, counted in `status_rim=` as "skipped as alpha-blended"). Additive and opaque draws pass.
    In addition the usual stealth shaders are not in the default list. Visual only, nothing is revealed.
* **Independent of the fur.** The fur mask (`t20`) and the rim mask (`t23`) are separate; both can run together on the charr, a draw
  that is in both lists is issued once for each mask. Deep inside the character only the fur acts; at the silhouette both do.

* **v11: only on proven fur skin while the shells are on** (`shell-fur.md` "v11"). The rim's gate is multiplied by `t25`, the shell
  pass's skin target: how much the visible surface is a fur draw, a fur island and a fur texel (the strands' own verdict). Hair cards,
  armour, other races' skin, horns, claws, the cut-out pixels of alpha-tested cards and pixels whose fur mask holds another draw's uv get
  no rim (the rim feather / glow on the hair cards and the horn). With `fur_shell=0` (no classification) `t25` is unbound
  and the rim covers the whole rim mask as before. `status_rim=` ends with the gate (`gate: proven fur skin only ...`). Test:
  `v11_the_skin_gate_switches_the_old_fur_look_and_the_rim_off_where_no_fur_skin_is_proven` (the gated half bit-identical to no rim, the
  rest bit-identical to v10).

## The rim mask (generalised from the fur mask, `furmask.rs`)

The same redraw-into-mask machinery as the fur: the `PSSetShader` hook remembers the bound pixel shader, the `Draw*` hooks call
`before_draw` before the engine's draw, and a draw whose pixel shader is in `rim_ps` (vertex constant buffer 0 at least
`rim_min_vcb` bytes, opaque pass = full-size colour and depth) is issued once more into the mod's own **R8_UNORM** target (screen
size) with a rim pixel shader (writes 1.0; the one generated for the draw's vertex signature, `z_ps_hlsl`, also writes the fragment
depth into the rim's R32_FLOAT z-stamp; the static `ps_rim` until it is built), same vertex shader / layout / buffers / viewport /
rasteriser and the engine's depth buffer as a read-only depth test; `StateGuard` restores everything. At the light-adjust draw the
occlusion check erases the rim pixels that geometry drawn later covers (as for the fur, see `draw-diagnostic.md`). The target is cleared by the first rim draw of a frame; a frame without a rim
draw binds nothing and sets `b11` to zero, so a stale mask is never read and the shader skips the whole branch.

| | fur | rim |
|---|---|---|
| selector | `fur_ps` / `fur_vs` / `fur_min_vcb` | `rim_ps` / `rim_min_vcb` (no vertex-shader filter) |
| target | R16G16B16A16_UNORM (mask + mesh uv) | R8_UNORM |
| shader slot | `t20` | `t23` |
| constants | `b6[1].zw` | `b11` |
| picker highlight | yes (it takes over the fur channel) | rim is paused while the highlight is on |
| blend-state check | no | yes (alpha blends skipped) |

`rim_strength` 0 (and `rim_debug` 0) = the channel is not wanted: `on_ps_set` / `before_draw` do nothing for it, no target is
created, no depth consumer is registered, `b11` is zero and the substitute is bit-identical (pinned by a test).

## Slots and constants (data-path additions)

* `t23` = the rim mask (null when no rim draw was made this frame; unbound reads zero).
* `b11` (`datapath::rim_cb_values`): `[0] = (rim strength or 0 when not ready, width px, subsurface strength, debug)`,
  `[1].x = 2 tan(half fov) / screen height` (the grazing term). `rim_ready()` = a rim draw was masked this frame and the mod is on.
* While the rim is wanted the AO depth capture runs (`ao::DEPTH_FOR_RIM`) and `fog.rs` runs, so the shader has the linear depth
  `t4` and the fog state `t3` for the grazing term, the depth silhouette and the fog release.

## Which draws are "characters"? (honest status)

* **Verified: the charr skin.** `rim_ps` defaults to the charr shaders verified in the game `1d50763f,e0189185,5d00827f`
  (`furpick::CHARR_PRESET`, the same as the fur's "Charr (verified)" button), `rim_min_vcb` 4096 (skinned meshes have vertex
  constant buffer 0 of 16464 / 16496 bytes).
* **Other races and armour: not decided.** No `draws-*.log` dumps of other races and armour have been analysed yet, so no data-driven classification of the other skinned
  shader groups was made, and **"all skinned characters with vcb >= 16000" is deliberately NOT the default**: a vertex cb size alone
  also matches NPCs, animals and some props, and nothing proves that the pixel shader groups are consistent across races. A wrong
  guess would only put a cosmetic rim on NPCs / animals, but it would also make the default depend on an unverified claim.
  To extend it from data: in game, open the overlay > "Fur (charr)" > **Pick fur shaders**, hover the rows (head, hands, armour
  light / medium / heavy, other races) and add the pixel shaders you want to the **Rim light & skin** list (type the hash, or
  **Same as fur list**), with **Show selection in cyan** on. `status_rim=` says how many draws were masked and how many were rejected
  by `rim_min_vcb`.

## Config (`config.ini`, hot reloadable, comments are written by Save)

| key | range | default | meaning |
|---|---|---|---|
| `rim_strength` | 0..1 | 0.4 | largest extra luminance share of the rim = `0.25 x` this (default about +10 %); **0 = off, bit-identical, no masking cost** |
| `rim_width` | 1..8 px | 3 | width of the rim from the silhouette |
| `subsurface_strength` | 0..1 | 0.3 | warm glow of the dim, thin parts, at most +8 % x this (needs `rim_strength` > 0) |
| `rim_ps` | hashes | `1d50763f,e0189185,5d00827f` | pixel shaders of the character draws; empty = off |
| `rim_min_vcb` | bytes | 4096 | smallest vertex cb 0 |
| `rim_debug` | 0/1 | 0 | paints the selected pixels cyan (80 %); works with strength 0 |

Overlay: section **Rim light & skin** (sliders, cyan view, min vertex cb, the `rim_ps` list with Remove / Add / Charr (verified) /
Same as fur list / Clear, and the status line). Debug capture: `rim_strength=`, `rim_width=`, `subsurface_strength=`, `rim_ps=`,
`rim_min_vcb=`, `rim_debug=` and `status_rim=` (draws masked last frame, candidates, rejected, skipped as alpha-blended).

## Tuning constants (top of the rim block in `light_adjust_hdr.hlsl`)

`RIM_MAX_GAIN` 0.25, `SSS_MAX_GAIN` 0.08, `RIM_Y0/Y1` 0.004 / 0.03 (dark release), `RIM_BG_Y0/Y1` 0.02 / 0.25 (background brightness),
`RIM_BG_NEAR/FAR` 6 / 10 px, `RIM_DEPTH_STEP` 1.06 (depth silhouette), tint share 0.35 in `rim_look`.

## Tests (WARP, `hdr_shaders_tests.rs`, `furmask_tests.rs`, `datapath.rs`, `config.rs`)

No-op at 0 (bit-identical: strength 0 with full mask, mask 0, unbound `t23`, and every pixel outside the mask for any strength);
the rim acts only within the width of the mask edge (exactly unchanged one pixel beyond, falls off inwards, widths 1 / 3 / 6);
bounded at +25 % (5 colours, both effects at full) and the subsurface alone at +8 %; no channel above `max(pixel, 1)`; hue kept
(chromaticity < 0.03, channel order kept); no lift on black / almost black, < 0.001 on a dim pixel, nothing against a dark background;
released in bluish water and in fog (and applies without a fog estimate or with zero confidence); the tint follows the background
sampled outside the mask (red side redder, blue side bluer, a bar inside the mask does not tint); the grazing term (a slanted
surface gets > 1.25x the rim of a facing one); a depth step inside the mask is a silhouette; the subsurface is warm, only in dim pixels
and zero at strength 0; NaN / inf / negative / garbage depth give finite non-negative output; the debug view paints cyan even at
strength 0; fur and rim together stay independent. Host: blend-state classifier, selector / depth-consumer / status state, constants,
config keys. WARP draw tests: the rim draw marks visible pixels in its own R8 target, respects the depth buffer, never touches the
fur target, colour or depth and restores the pipeline state; alpha-blended draws are refused (nothing issued), additive and opaque ones accepted.
Occlusion check (WARP, engine-like R24G8_TYPELESS depth with a shader view): a nearer surface drawn later erases exactly the covered
pixels (fur with uv: survivors bit-identical; rim R8 the same), nothing in front = bit-identical, reversed Z (GREATER), typed D24 /
no SRV flag = untouched + `unavailable`, nothing stamped = untouched, other / mixed depth tests = skipped, a mesh that overlaps
itself within ONE draw (two layers, the farther rasterised last; LESS, LESS_EQUAL, GREATER, GREATER_EQUAL) keeps the nearest stamp
and is left bit-identical (the bug that left only the silhouette band of the charr tail), `fur_occlusion=0` bypasses the check
through `on_light_adjust` and the probe counts what it would erase, pipeline state
(RTVs, DSV, shaders, t0 / t1, blend, depth state, viewport, topology) restored after the two-target mask draw and after the check,
and the hood-over-head regression for both channels.

## In-game checks (what WARP cannot say)

1. `rim_debug=1` (or the overlay cyan view): only the charr (and nothing else) cyan. `status_rim=` shows draws masked > 0; if it
   says "skipped as alpha-blended" for the visible charr, the skin draws use a blend state and the blend rule must be relaxed.
2. The look at the default 0.4 (about +10 % at the silhouette): visible but subtle; `rim_strength` 1 is the +25 % maximum.
3. Dark areas / night: no rim, characters are not easier to see. Under water and in thick fog: no rim.
4. Stealth: no rim on the stealthed character.
5. Whether the engine's linear depth `t4` is right during the light-adjust draw (the grazing term and depth silhouettes need it;
   without it the rim falls back to the mask edge and a constant grazing of 0.5).
6. Geometry drawn LATER that covers the character (a hood over the head) is now erased from the mask by the occlusion check (z-stamp +
   final depth, `draw-diagnostic.md`, "Fur: the character mask"). `status_rim=` must end with `occlusion: checked`; with
   `rim_debug=1` the hood must not be cyan where it covers the head, and the whole visible character must stay cyan (not only a
   band along its silhouette). `fur_occlusion=0` bypasses the check (A/B); `status_occlusion=` in `latest.txt` counts what the
   check erases (`debug-mode.md`).

## Limits

* The mask is the union of all `rim_ps` draws (NPCs, animals and other characters that share those shaders get the rim too; cosmetic).
* Pixel shaders that `discard` are drawn solid in the mask; stencil is ignored; only the four `Draw*` calls the mod hooks.
* The occlusion check only sees what wrote DEPTH: later transparent geometry (no depth write) does not erase, depth-writing particles
  do; later alpha-tested cards erase only where they kept their pixels (correct). No check when the depth buffer is typed or has no
  shader view (then as before), or when the character's depth test is not LESS / LESS_EQUAL / GREATER / GREATER_EQUAL.
* Resolution: the rim width is in pixels at the screen size, so at 4K it is thinner on screen than at 1080p for the same value.
