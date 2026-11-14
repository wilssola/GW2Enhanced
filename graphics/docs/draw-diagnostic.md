# Per-draw diagnostic (first step of the charr fur test)

Status: implemented, unit and WARP tested (13 Rust tests, 12 Python tests). Measurement only: no visual change, no change to
any call. Not yet run in the real game.

## Why

Phase 2 starts with a fur shader on the charr as a test of better material shading. The engine has no material buffer and the
world pixel shaders are not in the exe, so nothing tells us which draws of the scene's opaque pass are charr bodies. This
diagnostic records what each opaque draw is made of, for ONE frame, so that two dumps (a charr on screen, no charr on
screen) can be compared offline and the charr draws identified by their shaders and texture sets.

## How to use it

1. In game, with the overlay's Graphics tab open (Shift+Home; Shift+End in a graphics-only build), put a charr on screen and keep the camera still. Click **Dump frame**.
   The click writes the usual `frame-passes-<unix>.log` and ALSO arms the draw capture for the NEXT frame. The overlay
   line `Draw capture:` says `capturing this frame (N draws so far)` and then `wrote draws-<unix>.log (N draws in pass #k, G groups)`.
   The two files are adjacent frames (the pass dump is the frame that was finishing, the draw log the one after it).
2. Move so that no charr is in view (same place, same camera angle, same graphics settings, ideally the same
   other characters), click **Dump frame** again.
3. `python tools\draw-diff\draw_diff.py draws-<with>.log draws-<without>.log` (the log folder is `GW2Graphics\`
   next to `Gw2-64.exe`). It lists the groups present only in the first log, best candidate first, with the reasons.
   `--shader-only` compares by (VS, PS) only; `--top N`, `--min-score S` shorten the list.

The log is also plain text: `G` lines of `key=value` tokens, one per group, readable without the tool.

## What is recorded

For every draw of a pass that has a full-size colour target AND a full-size depth target (the opaque pass; the effects
pass shares the shape, the final selection is made at the frame end with `classify::classify`, which names the opaque
pass index; if the classifier does not match, the pass with the most captured draws is used and the header says
`how=fallback_most_draws_...`):

| field | source |
|---|---|
| kind, vertex/index count, instance count | the `Draw*` hook arguments |
| vertex shader hash | `VSGetShader` -> `shaders::vs_registry()` (filled by the new `CreateVertexShader` hook, FNV-1a 32 of the DXBC like the pixel shaders) |
| pixel shader hash | `PSGetShader` -> `shaders::registry()` |
| pixel-shader textures, slots 0..7 | `PSGetShaderResources` -> view -> resource: format (the view's own format wins over a typeless texture), width x height, mip count. 2D textures only; other resource types keep format 0 and size 0 |
| vertex / pixel constant buffer sizes, slots 0..3 | `VSGetConstantBuffers` / `PSGetConstantBuffers` -> `ID3D11Buffer::GetDesc().ByteWidth` |
| input layout pointer, vertex buffer stride at slot 0 | `IAGetInputLayout`, `IAGetVertexBuffers(0, 1)` |

Aggregated by (VS hash, PS hash, texture set), where the texture set is the list of (slot, format, size, mips): NOT
the texture pointer, which differs per object. `texptrs` counts the distinct texture pointers inside a group.

### Log format (`draws-<unix>.log`, version 1)

```
# gw2g-draws v1
# unix=1700000000 frame=42 ctx=0x1f00 pass=3 screen=3440x1440
# pass_draws=932 captured=6 dropped=0 groups=3 how=classifier
G draws=2 vs=00001111 ps=00002222 kinds=DrawIndexed:2 indices=12600 max_idx=7200 vertices=0 max_vtx=0 instances=2 vcb=2048..4096 vcbs=4096,0,0,0 pcbs=64,0,0,0 layouts=1 strides=48 texptrs=6 tex=0:BC3_UNORM:1024x1024m11,1:BC1_UNORM:512x512m10,2:BC3_UNORM:1024x1024m11
```

`vcb=min..max` is the biggest vertex constant buffer per draw (smallest and largest over the group), `vcbs`/`pcbs` the
largest size seen per slot, `vs=unknown` / `ps=unknown` a shader created before the hooks were installed (or on a
device that was never hooked). Sorted by `draws`, biggest first. The same text (header and one G line per group) is
pinned by a Rust test against `tools/draw-diff/fixtures/sample_a.log`, which the Python tests also parse, so the writer and
the tool cannot drift apart.

## Effects passes capture (draws-fx)

The same armed frame also writes `draws-fx-<unix>.log`: the groups of EVERY other pass whose RT0 is full screen size (with
or without depth), the opaque pass already in `draws-<unix>.log` excluded. Purpose: floating hard-edged bright rectangles
around effects/characters are probably additive effect quads drawn after the opaque pass; this shows which shaders/textures
draw them, in which pass, with which blend state.

```
# gw2g-draws-fx v1
# unix=.. frame=.. ctx=0x.. screen=3440x1440
# opaque_pass=3 how=classifier passes=2 captured=121 dropped=0 groups=4
# fxpass=9 rt0=R16G16B16A16_FLOAT depth=none pass_draws=63 captured=63 groups=2
G pass=9 blend=ADD:ONE:ONE quad=1 draws=40 vs=... ps=... (the usual G fields) ...
```

* Each `G` line is the usual group line with `pass=`, `blend=` and `quad=` first. Groups are per (pass, blend state, VS, PS,
  texture set), ordered by pass then draw count.
* v18: `upg=` is the mask of the bound colour targets the mod upgraded to float (bit i = slot i) and `clamp=1` says the engine shader ran as its clamped twin (`src/clampemu.rs`, docs/data-path.md v18); groups are also split by these two.
* `quad=1`: every draw of the group is a screen-aligned quad / small mesh (indices <= 12 for indexed draws, vertices <= 6
  otherwise). `blend` is RT0 only: `off`, or `OP:SRC:DST` (colour), with `|OP:SRC:DST` appended when the alpha part
  differs (`ADD:ONE:ONE` is additive); `unknown` if it could not be read. Read with `OMGetBlendState` +
  `ID3D11BlendState::GetDesc` (new vtable constant `vt::BLEND_GET_DESC`), references released, WARP-tested.
* The `# fxpass=` header lines give the RT0 and depth format and the recorder's draw count of each covered pass.
* Caps: 4000 draws per pass (`FX_MAX_DRAWS`), 16000 in total (`FX_MAX_TOTAL`); the rest is counted in `dropped`. The opaque
  pass log keeps its own 60000 cap and its byte-identical format.
* Extra cost: draws of full-size no-depth passes are now read too (tens to a few hundred draws), plus 2 COM calls per draw
  for the blend state. Same guards as above (armed only, `catch_unwind`, read-only).
* Read it quickly: `python tools\draw-diff\fx_summary.py draws-fx-<unix>.log [--top N]` prints, per pass, the top groups,
  quad-like first then by draw count, flagging additive ones. Test: `tools/draw-diff/test_fx_summary.py`.

## Cost and safety

* Normal frames: every `Draw*` hook already runs `recorder::draw` inside the recorder gate; the diagnostic adds ONE
  `AtomicBool` load (`drawdiag::on_draw`). Nothing else, no lock, no D3D call.
* Armed frame (one frame per click): about 25 COM `Get` calls per opaque draw (about 1000 draws in the reference frame),
  so expect one visible hitch of some milliseconds in that frame. Draws of other passes are skipped from the recorder's
  own pass description without any D3D call.
* The capture runs BEFORE the original draw call, only reads, releases every reference a `Get` returns, sits inside
  `catch_unwind` and the hook's re-entrancy guard, and never touches the arguments. The `CreateVertexShader` hook records
  and returns the original's result, exactly like `CreatePixelShader`.
* Memory: at most 60000 records per capture (`MAX_DRAWS`), cleared at the next arming.
* If `CreateVertexShader` cannot be hooked (inline hook and vtable fallback both fail) the log says `vs=unknown`; nothing
  else is affected (log line `drawdiag: CreateVertexShader not hooked`).

## Tests

* Rust (`drawdiag::tests`, host and WARP): aggregation (grouping by shaders + texture set, pointers ignored, tie order,
  sums and maxima), formatting against the shared fixture, `to_record` registry lookup, pass choice (classifier on the
  reference frame, fallback, context filter, empty), status/arm/finish state machine and the not-armed no-op with a null
  context.
  **WARP test**: `read_bound` runs on a real D3D11 WARP context with a real vertex shader, pixel shader, input layout,
  vertex/index buffers, 256-byte vertex constant buffer, 16-byte pixel constant buffer and two textures (slots 0 and 3,
  one with mips); it checks every field, that no COM reference count changes, that three real draws aggregate into one
  group, and that an empty context reads as all zero. The hook functions themselves (`h_draw*`, `h_create_vertex_shader`)
  need the real game: they are inline detours on the live D3D11 module.
* Python (`tools/draw-diff/test_draw_diff.py`): parsing, key diff (with the synthetic pair `sample_a.log` /
  `sample_b.log`), the shader-only merge, ranking of a skinned-looking group over a rigid prop / instanced grass / a
  banner, monotonic score, report text, CLI.
  Run: `python -I -m unittest discover -s tools\draw-diff -p "test_*.py"`.

## Needs the real game

* That the hooks fire, the VS registry fills (`vs=` is not `unknown`), and the opaque pass is found.
* That the charr really stands out: the heuristic in `draw_diff.py` (vertex constant buffer 512/1024/2048/4096+ bytes,
  3-4 textures, 1000..60000 indices per draw, not instanced, stride >= 32) is a guess about bgfx skinning. bgfx creates
  one constant buffer per program sized to the program's uniforms, so a skinned program having a larger vertex buffer is
  expected, but it may be that bone matrices come through a texture or a structured buffer instead (then only the
  texture set and index counts help; the SRV list is for the PIXEL shader only, vertex-stage textures are not read).
* Constant buffers bound with `VSSetConstantBuffers1` (offset ranges of a big shared buffer) would report the size of the
  whole buffer; if every group shows the same vertex buffer size, that is the case and the next step is to read
  `VSGetConstantBuffers1` too.
* The hitch of the armed frame on a busy scene.

## Research: how a fur effect could be done later

Not implemented. What the data path gives, and the options.

### What is available

* The scene colour at the moment the light-adjust substitute runs (PS slot 0), full size, float16 or BGRA, gamma 2.0 encoded.
* Linear depth of the whole frame (`ao::depth_resource()`), full size R32_FLOAT, with the AO's view model (tan half fov 0.5).
  A normal can be reconstructed from it (cross of the depth derivatives, the AO already does the equivalent).
* The place and the means to draw into the mod's OWN targets and feed them to the light-adjust substitute through a new
  texture slot (the pattern of `ao.rs`: own target, own constant buffer, bound at draw time).
* After this diagnostic: a way to recognise the charr draws (VS hash, PS hash, texture set), at draw time, with the draw
  arguments, the bound vertex/index buffers, input layout and constant buffers at hand (they are the same `Get` calls).
* NOT available: normals, tangents, a material or object id buffer, light data in a usable form, the world pixel shader
  sources. Nothing can be put into the engine's own targets between passes (the data-path rule); only the opaque pass
  itself is a place where extra draws survive, because they are inside the pass that produces the picture.

### Option A: shells (the real thing)

Re-issue each recognised charr draw N times (8-24) inside the opaque pass, with a vertex shader that pushes every
vertex out along its normal by `i/N * length` and a pixel shader that alpha-tests a strand noise texture, so the layers
build volume. Needs a vertex shader of ours that skins like the engine's: the engine VS bytecode is captured by the new
hook, so it can be patched or re-written after reading its input signature and the skinning constants (use
`tools/dxbc-dump`), with the SAME bone constant buffer slot bound; plus the input layout elements (add a
`CreateInputLayout` hook to keep the semantics: normal, tangent, weights, indices) and the vertex stride (already
logged). The pixel shader has to do its own lighting: sample the body's albedo (texture slot known from the log),
darken towards the root, and use a simple sun term; the light constants live in pixel constant buffers of the engine's
shaders (their layout must be found by diffing two captures). Costs the charr geometry N times, so it must be limited to
the candidate groups and a distance. Risks: depth write/blend state of the shell draws (copy the draw's own states with
`OMGetBlendState`, `OMGetDepthStencilState`), the silhouette grows by the fur length (keep it a few centimetres:
visual only, never hide or reveal anything), transparency ordering of the shells against other opaque geometry (depth
test on, depth write off for the outer shells). Most work, best result.

### Option B: fins (silhouette quads)

Extruded quads along silhouette edges, to fix the shell look at grazing angles. Needs adjacency (the engine meshes have
none: derive it on the CPU once per index buffer from the index data, cache by buffer pointer, which also needs
mapping the index buffer) and a geometry shader stage the engine does not use. Only worthwhile on top of shells; skip
for the test.

### Option C: screen-space fake (cheapest, no geometry)

Two parts: (1) a mask of the charr pixels, (2) a fur-looking shading change inside the light-adjust substitute, driven by the
reconstructed normal and depth.

1. Mask: at draw time, for the recognised draws only, re-issue the same draw into an own full-size R8 target (same
   viewport, same depth buffer read-only for the test, our trivial pixel shader writing 1, the engine's vertex shader
   and layout and constant buffers left bound, so the geometry matches exactly). Needs: the engine VS + its bound state
   (nothing extra), an own render target and depth-test-only state, and a way to restore the engine's state (the
   `StateGuard`). One extra draw per charr draw, cheap. Alternative without re-drawing: compare the colour/depth before
   and after a recognised draw to find the changed pixels (more state juggling, more cost).
2. Look: in `light_adjust_hdr.hlsl` (new slot, the same pattern as t6/b6), inside the mask: break up the shading with
   an anisotropic strand noise (screen-space noise stretched along the projected normal gradient), add a soft rim
   term `(1 - N.V)^k` brightening grazing pixels (fur catches light at the edges), and optionally a slight darkening between
   strands. It changes shading only, never coverage. Limits: no silhouette, no parallax, strands swim with the camera
   unless the noise is anchored to the surface (anchor it to world-ish position rebuilt from depth), and the lighting
   is whatever the engine already baked into the colour.

### Extra data each option needs

| | shells | fins | screen-space fake |
|---|---|---|---|
| candidate draws identified (this diagnostic) | yes | yes | yes |
| engine VS bytecode and its skinning constants | yes | yes | only to re-issue the mask draw |
| input layout semantics (`CreateInputLayout` hook) | yes | yes | no |
| index/vertex buffer contents (adjacency) | no | yes | no |
| engine light constants / sun direction | yes | yes | optional (rim without a direction) |
| blend / depth-stencil / rasterizer state of the draw | yes | yes | depth only |
| mask target | no | no | yes |
| reconstructed normal from depth | no | no | yes |
| risk of the engine overwriting it | low (inside the pass) | low | low (inside the substitute) |
| work | high | highest | low |

Recommendation: first identify the charr draws with this diagnostic; then do option C as the test (it proves the
identification and the mask, is cheap and cannot break the geometry); then shells for real volume.

## Screen-space fur (test)

> Superseded: the alpha-class mask did not work, and the screen-space hash-noise strands described below were REMOVED (they showed as a grid / screen door over light fur and swam with the camera). `fur_look` now uses the draw-based fur mask (last section of this file) and a pattern-free look: (1) unsharp mask of the luminance at 1 px inside the mask, as a hue-preserving ratio clamped to 0.85..1.15, so the strands painted in the HD texture gain contrast; (2) a thin rim from the mask distance (3 px) and depth steps: slightly warm desaturated glow plus a 20 % feather of the outline towards the neighbouring background; (3) a tiny warm subsurface term `0.10*(1-y)*y`. All scaled by `fur_strength`; 0 = exact no-op; no noise, nothing in screen coordinates; a flat area stays flat and the output is identical frame to frame. The strand / rim maths below is historical.

Status: implemented as a slider test, deliberately NOT charr-specific. A fur look was needed before any draw is
identified, so it is the "option C" look without the mask: every pixel whose raw scene alpha is in the character class
(mid / high, soft mask `smoothstep(0.20, 0.28, alpha)`) gets it, charr or not (other races, NPCs, mounts, effects with a
character-class alpha). Terrain (low class) never does.

What it does, inside the light-adjust substitute only (the one place effects survive, see `data-path.md`), in
`fur_look()` of `light_adjust_hdr.hlsl`, on the encoded colour right after the AO factor:
- strands: a hash noise in cells 1 px wide x 5 px tall plus a lower-frequency octave (3 x 11 px), as a multiplicative
  luminance factor `1 + fur * 0.22 * (n - 0.5) * 2` on all three channels (hue kept, at most +-22 % at strength 1);
- fuzzy silhouette: the alpha mask is sampled at +-2 px; where it drops (the character's edge) the pixel is lerped towards
  the mean of the 4 neighbours by `0.25 * fur` and gets `0.10 * fur * lum` of light, like backlit fur. 4 extra taps;
- released on bright pixels (smoothly from 0.60 to 0.85 of the max channel: lights, effects, skill glows get no noise, and
  nothing can clip) and on very dark ones (below 0.03 to 0.06).

Control: `fur_strength` in config.ini (0..1, default 0.35, 0 = off: the branch is skipped and nothing is read), the overlay
slider "Fur test (screen-space)" with the warning line, and `b6[1].z` (a free component of the existing constants, no new
slot). With the mod disabled by the user the value sent is 0 (vanilla emulation). Visual only: alpha and coverage unchanged.

Limits: the noise is anchored to the screen, so the strands swim when the camera or the character moves; no parallax, no
real silhouette growth; the lighting is whatever the engine baked into the colour.

Next step: identify the charr draws. Take two draw dumps (see "How to use it") in different scenes with a charr in view
(for example a city and an open-world map, different armour), and keep the draws whose shader pair / vertex layout / constants
are common to both and absent from non-charr characters. Then the mask (option C part 1) replaces the alpha class and the
fur stops affecting everything else.

## Fur: the character mask and the live selector

The alpha class was not a usable mask, and nothing in the engine's data says "this draw is a charr". So the mod now builds
the mask from the DRAWS and lets the player (or the developer) tune which draws count, live, with the debug screenshots.

### The keys (config.ini, hot reloaded with `debug_hot_reload=1`)

| key | default | meaning |
|---|---|---|
| `fur_strength` | `0` | intensity of the look, 0..1 (as before). 0 = no fur |
| `fur_ps` | empty | comma-separated 8-hex PIXEL shader hashes (the `ps=` of a `draws-*.log` group) whose draws count as fur. Empty = the mask is off |
| `fur_vs` | empty | optional VERTEX shader hashes the draw must also use (empty = any) |
| `fur_min_vcb` | `4096` | only draws whose vertex constant buffer 0 is at least this many bytes (skinned meshes have 16464 / 16496) |
| `fur_debug` | `0` | 1 = the selected pixels are painted magenta (80 %) in the light-adjust output, so a screenshot shows exactly what was picked. Works with `fur_strength=0` |
| `fur_uv` | `auto` | which vertex-shader output carries the mesh uv for the strands: `auto` (the lowest `TEXCOORD` with two float components) or `TEXCOORDn` / `TEXCOORDn.xy` / `TEXCOORDn.zw`. Hot reloaded |

A draw is a fur draw when `ps` is in `fur_ps`, `vs` is in `fur_vs` (if given) and `vcb0 >= fur_min_vcb`.

### How the mask is built (`src/furmask.rs`, `shaders/furmask.hlsl`)

* The `PSSetShader` hook keeps the hash of the bound pixel shader. The `Draw*` hooks, before the engine's own call, check
  one atomic; for a fur draw in the opaque pass (full-size colour + full-size depth) the SAME draw is issued once more into
  the mod's own R16G16B16A16_UNORM target (a = mask, rg = frac of the mesh uv, b = uv valid) with a pixel shader of the mod's: same vertex shader, input layout, vertex / index / constant
  buffers, viewport and rasteriser state, the engine's own depth buffer bound as DSV, our own depth-stencil state with the
  engine's depth test and depth WRITES OFF (the engine's targets and depth are not modified), `StateGuard` restores everything.
  It runs before the engine's draw, so the depth buffer holds what was drawn earlier in the pass: a character behind
  something drawn earlier is occluded correctly.
* The mask is cleared by the first masked draw of a frame; a frame without one binds no mask (nothing stale is read).
* **Occlusion by geometry drawn LATER (z-stamp + check, fail open).** The depth test above only knows what was drawn BEFORE the
  character. A hood drawn after the head covered it in the picture but left the head's mask pixels set, so the rim band, feather and
  strands painted the head's outline onto the hood (seen in game: a charr with a hood, from behind). Now every mask (fur and rim)
  has an R32_FLOAT **z-stamp** of the same size bound as RTV1 beside it: the mask pixel shaders also write `SV_Target1 =
  SV_Position.z` (the device depth the depth test used). The stamp keeps the NEAREST fragment: it is blended with MIN (MAX for
  reversed Z; the mask itself is not blended) and cleared to 2 (-1 for reversed Z); any value outside 0..1 = nothing stamped. Our
  mask draw writes no depth, so the fragments of ONE draw do not occlude each other: before this blend a farther layer of the same
  mesh rasterised later overwrote the stamp, the engine's depth kept the nearer layer, and the check erased the inside of every mesh
  that overlaps itself (seen in game: with `fur_debug=1` only a thin band along the silhouette of the charr tail stayed magenta;
  WARP regression `regression_a_mesh_that_overlaps_itself_in_one_draw_is_not_erased_by_the_check`). At the start of the
  light-adjust draw, once per frame and channel, a full-screen pass (`Mask::validate`, `ps_occ_less` / `ps_occ_greater` in
  `furmask.hlsl`, blend off, no depth buffer) reads the stamp (`t0`) and the engine's FINAL depth (`t1`, a shader view of the depth
  buffer the masked draws used, kept by a counted reference) and writes 0 into every channel of a mask pixel whose fragment was
  covered: depth test LESS / LESS_EQUAL -> covered iff `final < stamp - tol`, GREATER / GREATER_EQUAL (reversed Z) -> covered iff
  `final > stamp + tol`, `tol` = `OCC_TOL` (1e-5, 168 D24 steps; it was 2e-6) plus the depth bias of the masked draws' rasterizer state (`bias_tol`, bound as
  `b0` of the check). The game showed why: the final depth of a fully visible charr was a constant 3.87e-6 (65 D24 steps) nearer than the stamp
  (a depth bias the pixel shader's SV_Position.z does not carry on that GPU; WARP does carry it), so the check with 2e-6 erased 90..98 % of the mask. Any other test, or a frame whose masked draws used different tests /
  depth buffers, skips the check. Everything else is discarded (bit-identical). The depth view exists only for a typeless depth
  texture with the SHADER_RESOURCE bind flag: R24G8_TYPELESS (read as R24_UNORM_X8_TYPELESS), R32_TYPELESS (R32_FLOAT), R16_TYPELESS
  (R16_UNORM), R32G8X24_TYPELESS (R32_FLOAT_X8X24_TYPELESS). Otherwise the check is unavailable and the mask stays as it was (the
  old behaviour, logged once: `furmask: occlusion check unavailable: <why>`). `status_fur=` / `status_rim=` end with
  `occlusion: checked`, `occlusion: unavailable (why)`, `occlusion: skipped (depth test X)`, `occlusion: pending (...)` or
  `occlusion: off (fur_occlusion=0)`. `fur_occlusion=0` (config.ini, live) bypasses the check for both masks (the A/B switch);
  the debug capture's `status_occlusion=` reads the masks, stamps and depth back once and counts what the check erases
  (`docs/debug-mode.md`).
  To read SV_Position the pixel shader must sit on the vertex shader's registers (D3D11 links PS inputs by register), so the stamp
  comes from the shaders GENERATED per vertex signature: the uv shader, and (new) a fur-without-uv shader and a rim shader
  (`z_ps_hlsl`, same register rules and link check). The static `ps_mask` / `ps_rim` (signature unknown or still compiling) stamp -1
  (unblended), so their pixels are never erased (for LESS that -1 sticks for the frame; for reversed Z a later nearer selected draw
  replaces it).
* `light_adjust_hdr.hlsl` reads it as `t20` (see `data-path.md`) and `fur_look` acts only where it is set. Mask 0 or unbound:
  the picture is bit-identical to no fur. Bright pixels only darken, almost-clipped and very dark ones are untouched.
* Never when the mod is disabled by the user or the gate is closed, never for the mod's own draws, never blocking (the device
  state is behind a `try_lock`), the mask shader is compiled on a background thread, every hook entry is `catch_unwind`-guarded.

### Fur strands anchored to the mesh uv

The first look (unsharp detail + soft rim + warmth) was too subtle, and the screen-space strands before it swam with the camera.
The strands are now procedural noise in the MESH UV space, so they are glued to the surface.

* The uv comes from the draw's own vertex shader. The `CreateVertexShader` hook parses the DXBC output signature (`OSGN` / `OSG5` /
  `OSG1`, bounds-checked; `furmask::note_vs`). For a masked draw the uv pixel shader is GENERATED from that signature
  (`uv_ps_hlsl`: the user inputs of the vertex shader are replicated in register order and all read, so the compiler gives them the
  vertex shader's registers), compiled on a background thread, then CHECKED against the vertex signature (`verify_link`: same
  semantic, same register, component subset) and created on the render thread when ready. The element is the lowest `TEXCOORD`
  with two float components (`fur_uv=auto`) or the one named by `fur_uv`. Shaders are cached per signature + pick.
* Fallback, never a crash: while the shader compiles, for a vertex shader whose signature is unknown, has no usable uv or whose
  generated shader fails the link check, that draw writes the mask only (`b = 0`) and `fur_look` skips the strands there (detail,
  rim and warmth remain). `status_fur=` ends with `uv: ok (TEXCOORD0.xy)`, `uv: partial a/b (reason)` or `uv: none (reason)`; the
  log has the vertex outputs of every shader that failed. If `uv: none (no TEXCOORD output ...)` or the strands look like noise
  without structure, read the outputs in `GW2Graphics\log` and set `fur_uv` (for example `TEXCOORD1` or `TEXCOORD0.zw`).
* In `fur_look` (`light_adjust_hdr.hlsl`) the uv derivative per screen pixel is taken by finite differences of the 4 neighbouring mask
  texels (only neighbours that belong to the mask and have a uv; wrapped at the uv seam). The lattice level is chosen from it, so a
  strand cell is about 3 px (coarse layer) and 1.5 px (fine layer) across and 5 times longer along v whatever the uv density; two
  power-of-two levels are crossfaded (no popping when the camera zooms); the pattern wraps with the uv. Two layers, luminance
  modulation up to +-`FUR_AMP` (0.32) at strength 1, a luminance-neutral warm (valleys, roots) to cool (crests, tips) tint of 5 %,
  hue kept, never pushed into clipping (a pixel above 1.0 is not brightened, below 0.7x not darkened), released on very dark and
  almost-clipped pixels like the rest of the fur.
* LOD fade: where the uv changes faster than 0.03..0.08 uv per pixel the strands would be sub-pixel (aliasing, moire), the
  modulation fades to nothing; where the uv does not change at all (uv = const) there is nothing to anchor to and no pattern.
* The rim is stronger: a 4 px band from 16 mask taps (full strength in the middle of the band), a 40 % feather of the outline towards
  the background colour, a warm desaturated glow (0.14), and the strands break the fringe up.
* Stability: a pure function of the mask texel, its neighbours and the picture (no time, no random, nothing in screen
  coordinates): a still camera gives the same pixels every frame, and when the mesh animates the strands move with its uv. Strength 0 = the
  branch is skipped (bit-identical), mask 0 or unbound = untouched. The tuning constants are at the top of `fur_look` (`FUR_AMP`,
  `FUR_TINT`, `FUR_CELL_A / B`, `FUR_LONG`).
* Tested in WARP only (`hdr_shaders_tests.rs`, `furmask_tests.rs`): a uv ramp gives bounded, deterministic strands that travel with
  the uv, a flat uv / no uv gives none, a fast uv fades them out, mask 0 and strength 0 are the identity, NaN / negative / clipping
  inputs stay finite and bounded; the signature parser, the generated shader (compiled, link-checked and drawn) and the fallback.
  The real charr vertex outputs and the look itself need the game.

### Limits (first version)

* Geometry drawn LATER in the opaque pass that covers the character does not erase its mask pixels (stale pixels there).
* The mask is the union of all selected draws. Shaders are shared with other races and armour, so a guess can paint them too.
* Pixel shaders that `discard` (alpha-tested hair cards) are drawn solid in the mask. Stencil is ignored.
* Only the four `Draw*` calls the mod already hooks; indirect draws are not masked.

### How to iterate (developer flow, see `debug-mode.md`)

1. Make sure `debug_capture_seconds=20`, `debug_compare=1`, `debug_hot_reload=1` and that a charr is on screen. Set `fur_debug=1`
   and `fur_strength=0.5` (or any value; debug works without it).
2. Pick one candidate from the dump diff (`python tools\draw-diff\draw_diff.py`): groups only present WITH the charr and
   skinned (`vcb0` 16464 / 16496), for example `vs=130e17de ps=535b203b`. Write `fur_ps=535b203b` into config.ini.
3. Within 2-4 s the setting is live. Open `GW2Graphics\debug\latest-mod.png`: magenta = what the selector picked. The
   `latest.txt` line `status_fur=` says how many draws were masked last frame, how many had a `fur_ps` shader, and how many of
   those `fur_vs` / `fur_min_vcb` rejected (0 masked with candidates > 0 = tighten/loosen those two).
4. Only the charr magenta: good. Armour of other races magenta too: add `fur_vs=` to narrow it, or drop that shader. Nothing
   magenta: try the next candidate (`fur_ps=1c8a2482`, `cf595318`, `ad00c56f`, `5b124fd7`, `a901762b`, `c1089de4`), or combine
   several with commas (the skin shader and the fur-bearing armour parts are usually different draws).
5. When the magenta covers exactly the charr, set `fur_debug=0` and `fur_strength` to taste and look at the fur.

### The overlay's fur section and shader picker (in game, no file editing)

Overlay (Shift+Home), Graphics tab > section **Fur (charr)**: `fur_strength` slider, **Show selection in magenta** (`fur_debug`), **Min vertex cb**
(`fur_min_vcb`), text inputs for `fur_vs` and `fur_uv`, the `fur_ps` list as an editable list (hash, **Remove**; an input and
**Add**; **Charr (verified)** = `1d50763f,e0189185,5d00827f`; **Clear list**) and the live status (`status_fur`: draws masked,
uv state, rejected counts). Everything applies live through the same `furmask::configure` call that hot reload uses; **Save**
writes config.ini (with `debug_hot_reload` on, the file watcher then re-reads the same values: idempotent, no loop).

**Pick fur shaders** opens a second window with a live table of the skinned draws of the last frame (opaque pass, vertex
constant buffer 0 of 1 KB or more), one row per group of (vertex shader, pixel shader, size of the first bound texture):
pixel shader, vertex shader, biggest vcb, draws, indices or vertices per draw, texture size.

* Tick **fur** on a row to add / remove that pixel shader in `fur_ps` (live).
* Hover a row, or click its pixel shader to keep it selected: ONLY that group is drawn into the fur mask and painted magenta
  (no fur look meanwhile), so head, hands and armour can be told apart; the same pixel shader with another texture size is
  another row. Click again or **Clear highlight** to release it. The highlight is a live state, never written to config.
* Closing the window (or the overlay) clears the groups and the highlight.

Implementation (`src/furpick.rs`): the picker is fed from `drawdiag::on_draw` (so a draw pays ONE more atomic load while the
window is closed and nothing else; open: the opaque-pass check, one vertex cb read, and `drawdiag::read_bound` for skinned
draws only). Groups are bounded (64 per frame, extra draws only counted), published at the frame boundary
(`recorder::frame_begin`). The highlight uses `furmask::set_highlight` (`HighlightKey` = pixel shader, optional vertex shader and
first texture size): `on_ps_set` / `before_draw` then select by that key instead of `fur_ps`, the mask holds only that group and
`datapath::current_params` sends `fur = 0`, `fur_debug = 1` to the shader. Tested on the host (list editing, layout numbers,
bounded groups, zero cost closed, highlight filter); the UI itself and the draw selection need the game.
