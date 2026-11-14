# Fur look: live parameters and diagnostic views

Status: implemented, unit and WARP tested. Not yet calibrated in the real game: that is what the parameters and the views below are for.

## Why

The fur look (`fur_look()` in `light_adjust_hdr.hlsl`, see "Fur strands anchored to the mesh uv" in `draw-diagnostic.md`) had its constants compiled
into the shader, so every try meant a rebuild. In the game the fur never showed and the outline it added looked wrong. Now the constants are live
parameters in `config.ini` (hot reload with `debug_hot_reload=1`, overlay sliders), and a set of diagnostic views shows what the look is built from.
Everything applies only on the pixels of the fur mask (`fur_ps` / `fur_vs` / `fur_min_vcb`, `draw-diagnostic.md`); `fur_strength` 0 is still a bit-identical
no-op.

**v11 (`shell-fur.md` "v11"): only on proven fur skin while the shells are on.** The look's gate is multiplied by `t25`, the
shell pass's skin target (a fur draw, a fur island, a fur texel, the visible surface's own uv in the mask). An in-game check showed why: the
shells replace the look only where a strand lies, and v7..v10 took the strands away from the hair cards ("not fur") and the horn (an
excluded island), so the look's strands (value noise on a 3 x 15 px lattice of the MESH uv, +-32 %) came back there in full: a plaid on
the hair cards, stripes on the horn, plus its feather towards the background at every card edge and its strands in the cut-out holes
(the mask draws alpha-tested cards solid): the "transparent" hair. Now those pixels are left exactly as without the look. With
`fur_shell=0` `t25` is unbound and the look covers the whole mask as before (no classification exists then). The diagnostic views still
paint the whole mask. `status_furlook=` ends with the gate (`gate: proven fur skin only (v11 skin pass, N draws; ...)`).

## The keys

| key | range | default | what it replaces / does |
|---|---|---|---|
| `fur_amp` | 0..1 | `0.32` | `FUR_AMP`: strength of the strand brightness modulation (luminance factor `1 + fur_amp * strand`, never below 0.7). The warm/cool tint of the strands follows it (5 % at the default, at most twice that) |
| `fur_scale` | 0.25..4 | `1` | multiplies the strand cell sizes `FUR_CELL_A` (3 px) and `FUR_CELL_B` (1.5 px). Larger = coarser strands |
| `fur_length` | 1..12 | `5` | `FUR_LONG`: a strand cell is this many times longer along the mesh v than wide along u |
| `fur_detail` | 0..2 | `1` | gain of the texture detail (unsharp mask): the `0.7` in the ratio, still clamped to 0.85..1.15 |
| `fur_rim` | 0..2 | `0.25` | share of the rim glow (`0.14`) and the feather of the outline (`0.4`): `1` is exactly the outline the look drew before this key existed, `0` removes the glow and the feather |
| `fur_sss` | 0..2 | `1` | gain of the warm subsurface term (`0.12`) |
| `fur_view` | 0..4 | `0` | diagnostic view, see below. **Not persisted** |

`fur_strength` (0..1, default 0) is unchanged: it switches the look on. The five defaults other than `fur_rim` are the values that were compiled in, so
at the defaults the picture is the old fur look; `fur_rim` is the only deliberate change: it was effectively 1.0, which paints a visible outline around
characters, and 0.25 makes it subtle. The overlay (section "Fur (charr)") has a slider for each of the six, a "Defaults" button and the view combo; **Save**
writes the six, never `fur_view` (the line is always `fur_view=0`). The values are read at start; `fur_view` is never applied at start.

## The diagnostic views (`fur_view`)

They are painted on the pixels of the fur mask only (mask > 0.5) as the final colour of the pixel, so nothing after the fur look (exposure, tone, boost,
dither) changes them, and they do not need `fur_strength` (the mask is built while a view is on, even with strength 0, as long as `fur_ps` is not empty), nor do they care about the release
on dark and almost clipped pixels. Outside the mask the picture is untouched. Turn `fur_debug` (the magenta selection) off to read them: it is painted on
top with its usual 80 %.

| `fur_view` | paints | read it as |
|---|---|---|
| 1 | the mesh uv as colour: red = u, green = v, blue 0.35 | a smooth red/green gradient across the body = the uv is the real texture uv. Noise or stripes = the wrong `TEXCOORD` (`fur_uv`). **Solid red = the mask pixel has no valid uv** (the draw's vertex shader had none, or the generated shader is not ready): no strands there |
| 2 | the strand pattern alone, grey `0.5 + 0.5 * strand * fade`, no scene colour | what the strands look like with `fur_scale` / `fur_length`; mid grey = no pattern (flat uv, no uv, or faded out) |
| 3 | the rim band alone, white on black (the band before `fur_rim`) | the width and the shape of the outline: 0.4 at the edge pixel falling to 0 four pixels in (broken up by the strands where there is a uv) |
| 4 | the strand fade alone, white = full pattern, black = faded out | where the strands are too small for the screen (far away, steep angles) or have nothing to anchor to |

Test hints: an all-red body in view 1 means `status_fur=` says `uv: none`; use `fur_uv` (`draw-diagnostic.md`).

## How it gets to the shader

* New constant buffer **`b12`** (two float4), bound at the same place and moment as `b6`..`b11` (`datapath::Resources::bind`, every light-adjust draw):
  `[0] = (fur_amp, fur_scale, fur_length, fur_detail)`, `[1] = (fur_rim, fur_sss, fur_view, 1)`.
* The last component is the **live flag**. An unbound `b12` reads zero, the flag is 0 and `fur_live()` takes the defaults (`FUR_*_DEFAULT` at the top of
  `fur_look`), no view: an unbound buffer is the default look. Pinned by tests: unbound == bound to the defaults, bit for bit; a buffer with the flag 0 and other
  numbers (even a view) is treated as unbound. Values are clamped in the shader too, and a NaN becomes the lower end of its range (no NaN reaches the picture).
* The mod sends `fur_view` as 0 while the user has the mod switched off (the vanilla emulation stays vanilla) or while the fur picker's highlight owns the mask.
* `fur_strength` 0 stays a bit-identical no-op for any `b12`; only a view runs the fur code at strength 0, and then it only paints the view.
* A view needs the fur mask, which `furmask.rs` builds only while `datapath::fur()` is above 0 (or `fur_debug` is on). So `datapath::fur()` returns the smallest
  positive float while the strength is 0 and a view is on (`fur_set()` is the real value). The look it would switch on changes nothing.

## What the developer reads

`latest.txt` has `fur_amp=` .. `fur_sss=`, `fur_view=` and the line `status_furlook=fur_amp 0.32, fur_scale 1, fur_length 5, fur_detail 1, fur_rim 0.25,
fur_sss 1, fur_view 0 (off)`, next to `status_fur=` (the mask: how many draws, `uv: ok / partial / none`).

A calibration round, in the game with `debug_capture_seconds=20`, `debug_hot_reload=1`, `fur_ps` set (the "Verified shaders" preset) and `fur_strength` above 0:

1. `fur_view=1`: is the uv a clean gradient and nowhere red? If not, fix `fur_uv` first, nothing else matters.
2. `fur_view=2` / `4`: how big are the strands at the usual camera distance, where do they fade? Adjust `fur_scale` and `fur_length` until the pattern is a few
   pixels wide and several times longer, without moire.
3. `fur_view=0`: tune `fur_amp` (how strong), `fur_detail` (texture contrast), `fur_sss` (warmth). `fur_rim` last: `3` shows the band, 0.25 is a hint, 1 the old outline.

## Tests (WARP, `hdr_shaders_tests.rs`, `config.rs`, `datapath.rs`)

* the defaults reproduce the output of the old shader (golden values captured from it) on masks without a silhouette; `fur_rim` 1 reproduces its outline exactly
  and the default 0.25 only differs within 4 px of the mask edge;
* unbound `b12` == the defaults == a flag of 0; strength 0 is bit-identical with any `b12`;
* `fur_rim` 0: the edge pixels change like the interior ones (no glow, no feather); the outline grows linearly with `fur_rim`;
* `fur_amp` scales the strand modulation, 0 removes it; `fur_scale` / `fur_length` change the roughness across and along the strands; `fur_detail` and `fur_sss`
  scale their terms;
* each view paints what it says (uv colour and solid red without uv, the grey pattern which equals the modulation the look applies to within 0.01, the band
  0.4 / 0.3 / 0.2 / 0.1, the fade 1 / 0.9 / 0), only on mask pixels, at strength 0, on dark and clipped pixels;
* garbage in `b12` (NaN, infinity, huge, negative) never gives a NaN or a negative value.

## Limits

Only WARP so far. Whether the defaults look right on the real charr, and whether the real vertex outputs give a clean uv, needs the game (the views are the
tool for that). `fur_scale` below about 0.5 makes strands smaller than a pixel (aliasing) on purpose: the range is a calibration range, not a recommendation.
