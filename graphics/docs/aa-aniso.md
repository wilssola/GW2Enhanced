# Anisotropic filtering, mip bias, SMAA and sharpen

Status: implemented, unit and WARP tested. NOT yet verified in the real game (see "In-game checks").

Config (all live with `debug_hot_reload`, written to `config.ini`):

| key | default | range | meaning |
|---|---|---|---|
| `aniso_level` | 16 | 0..16 | forced anisotropy on world samplers; 0 or 1 = off (the game's own samplers) |
| `mip_bias` | 0.0 | -1..0.5 | added to the mip LOD bias of the same samplers (-0.3 = a bit sharper) |
| `aniso_filters` | 4 | 0..15 | which engine samplers get the anisotropy, by filter: 1 trilinear tiling, 2 bilinear + point mips, 4 already anisotropic, 8 trilinear clamped on U and V |
| `smaa` | 1 | 0/1 | mod-owned SMAA 1x on the 3D scene |
| `smaa_threshold` | 0.1 | 0.02..0.5 | luma edge threshold (on the HDR-compressed colour) |
| `sharpen_strength` | 0.3 | 0..1 | CAS-style sharpen after the AA; 0 = pass not run |

Debug capture lines: `status_aniso=`, `status_aa=` (also in the overlay).
`enabled=0` (vanilla emulation): nothing of this runs, every sampler and the copy pass are the game's own.

## Forced anisotropic filtering and mip bias (`src/aniso.rs`)

**dashed lines again, fixed.** With the default `6` the same dashed lines showed on wooden floors and walls.
A/B in the game, live: SMAA and sharpening off changed nothing; `aniso_level=0` removed the lines and `16` brought them
back; the only samplers changed in that scene were the two point-mip ones (filter `0x14`, wrap and clamped U/V). Default
since: `4` (only samplers the engine already made anisotropic get a higher level); a config file holding the old default
`6` is read as `4`.

**dashed lines.** With the anisotropy forced on the engine's trilinear samplers (`MIN_MAG_MIP_LINEAR`), dark dashed
lines followed the mesh edges of floors, walls and characters. A/B in the game with `aniso_filters` (live): the lines appear with
`1` (trilinear only) and not with `2` (point-mip only); the captures' difference shows them as a grid along the geometry edges.
The engine had 4 world samplers then (log line `aniso: sampler ...`): trilinear wrap, trilinear clamped U/V, point-mip wrap,
point-mip clamped U/V. Default since: `6` (the trilinear ones stay as the engine made them). Bit 8 (trilinear clamped U/V) exists
to test whether only the clamped one is at fault: if `aniso_filters=14` shows no lines in the game, the wrap one is, and 14 can
become the default.

**Route.** `ID3D11DeviceContext::PSSetSamplers` is hooked (inline detour, `Method::PsSetSamplers`, same machinery as
`PSSetShader`). Each sampler the engine binds is looked up by pointer in a cache of "what to bind instead". A miss reads the
desc (`GetDesc`), decides (`aniso::plan`) and creates a modified copy once (`CreateSamplerState` on the sampler's own device);
a hit costs a hash probe (open addressing, no allocation). The engine's objects are never changed; only the pointer array passed
to the original call is swapped. The cache holds one reference on each engine sampler (so an address cannot be reused by a
different sampler) and on its copy; when `aniso_level` or `mip_bias` changes the entries are rebuilt lazily.

**Why not hook `CreateSamplerState`.** Device-method hooks are installed lazily (first hooked call reaching
`discover_device`), and the engine may create its samplers before that, which would leave them unmodified for good. Binding
time does not depend on the order, handles live changes, and leaves the vanilla emulation untouched. Cost: one atomic check when
the settings are neutral, a few ns per bound sampler otherwise.

**World sampler or UI/post sampler.** From the desc alone (`plan`), in this order:

1. comparison filters (`Filter & 0x80`, shadow maps): never touched;
2. `MaxLOD <= 0.5` or `MinLOD >= MaxLOD` (cannot leave mip 0: UI atlases, the overlay's font sampler, 1:1 passes): never touched;
3. clamp/border on every axis (the bloom chain, SMAA, every fullscreen pass use clamp; world textures wrap or mirror): never touched;
4. only MIN_MAG_MIP_LINEAR (trilinear) and MIN_MAG_LINEAR_MIP_POINT become ANISOTROPIC with `MaxAnisotropy = aniso_level`;
   point filters and every other mix stay as they are; an engine sampler that is ANISOTROPIC already is raised to the level
   when lower (never lowered);
5. the bias is added to the engine's own `MipLODBias` and clamped to D3D's -16..15.99 (also when aniso is off).

The mod's own passes are not touched either: they bind through the same vtable but inside a hook (re-entrancy guard) or
under `ignoring()`. Known limit: a world texture that clamps on every axis (decals, some atlases) keeps the engine's filter.

**Coexistence with the Remastered HD texture mod.** That mod (msimg32 proxy) replaces texture DATA at load time; it does not
create or bind samplers. Nothing here depends on texture content, so both work together; HD textures benefit from the
anisotropy like any other.

## SMAA 1x + sharpen (`src/aa.rs`, `shaders/aa.hlsl`)

**Where.** The engine's chain ends `#15 combine -> #16 SMAA -> #17 copy (ps#c0ad1686) -> #18 UI -> final`
(`engine-post-chain.md`). The copy is the last pass that draws only the 3D scene; the UI is drawn after it. Because of the
data-path rule an extra pass inserted between engine passes would be overwritten, so nothing is inserted:

1. `PSSetShader` hook: `aa::on_ps_set` remembers when the bound pixel shader is `c0ad1686` (hash from the shader registry).
2. In every `Draw*` hook, `aa::draw_guard(ctx)` runs first. If the copy is bound, the feature is on, the shape is right
   (a full-size colour target without depth fed by a texture of the same size) and this frame was not processed yet, it runs the
   mod's own passes (below) in its own targets, reading the copy's input t0 and writing a mod-owned RGBA16F texture, and binds
   that texture AS t0.
3. The engine's draw then copies the processed picture into the UI target. The guard (RAII) puts the engine's SRV back after the
   draw, also on a panic. Pipeline state touched by the passes is saved/restored by `StateGuard`; the slots used besides t0 are
   t10..t13 (cleared) and b0 (restored).

The picture is scene-referred and float16 there (HDR highlights above 1.0 survive, the light-adjust substitute runs earlier), the
UI is not yet drawn, and the final pass (afdc14cb) runs later: the three conditions this stage needs.

**Passes** (all full resolution, `Load()` only, no sampler; SMAA "medium" = 16 pixel search, no diagonals, no corner rounding):

1. `ps_edges` (RG8): luma edges of the HDR-compressed colour, SMAA's local contrast adaptation (factor 2).
2. `ps_weights` (RGBA8): follows the edge line left/right (up/down) up to 16 pixels, finds the end crossings and computes the
   area under the line **analytically** (trapezoid / L / Z / U shapes) instead of SMAA's AreaTex/SearchTex: no 100 KB tables to
   embed, no bilinear search trick. Same geometry as SMAA's orthogonal patterns; the difference is only in the small details
   SMAA's tables smooth.
3. `ps_blend` (RGBA16F): neighbourhood blending on the dominant axis; pixels without weights keep their exact value.
4. `ps_cas` (RGBA16F, only with `sharpen_strength > 0`): CAS (contrast adaptive sharpening, weight
   `-0.2 * sqrt(strength) * sqrt(min(mn, 2 - mx) / mx)`); the dark side may undershoot, the bright side overshoot is capped.

**HDR handling.** Edge test, blend and sharpen weights work on `comp(c) = c * s(max(c))` with `s = 1` up to 1.0 and
`(2 - 1/m)/m` above (hue preserving, exact inverse, asymptote 2): an SDR picture is processed exactly like plain SMAA, a 20x
highlight against a 40x one is no edge, and mixing a highlight with a dark pixel leaves no bright rim. Negative and NaN texels are
treated as 0, huge ones are capped at 60000.

**Settings combinations.** `smaa=1`: passes 1-3 (+4 if sharpen); `smaa=0` and sharpen: only pass 4 on the engine's picture;
both off: nothing runs, nothing is rebound (bit-identical to the engine).

**Cost.** Memory: edges 2 B, weights 4 B, output 8 B (+8 B blended with sharpen) per pixel, about 110 MB at 3440x1440.
GPU time was NOT measured: three full-screen passes with a handful of texture loads, edge-free pixels exit early in the weights
pass; estimate 0.4-0.8 ms at 3440x1440 on a mid-range GPU plus about 0.2 ms for the sharpen. Must be measured in the game.

**The engine's own SMAA (#16) still runs first** and is not switched off (its shaders are not mod-owned and replacing them
needs an in-game check): the mod pass then works on an already smoothed picture, so the visible gain is mostly the HDR-aware
second pass and the sharpen. If the double AA looks too soft, `smaa=0` plus `sharpen_strength` keeps only the sharpen. A
substitute for the edge pass `833ba6e3` that discards everything would remove the engine's AA (follow-up, not done).

## In-game checks

* `status_aniso=` shows how many samplers changed and the skip counters; A/B `aniso_level=0` on an oblique ground texture;
  no change in the UI, bloom or shadows.
* `status_aa=` says "applied ... frames" with the screen size; "skipped, the copy pass does not have the expected shape"
  means the copy pass differs from the model (read the pass log).
* A/B `smaa=0/1` and `sharpen_strength=0/0.3`; look at thin geometry, HDR lights against the sky, UI crispness (must be unchanged).
* GPU time of the passes (a GPU profiler) against the < 1 ms budget.

## Tests

`aniso_tests.rs` (decision table, cache, WARP `translate` with real sampler states), `aa_tests.rs` (WARP: flat picture exact,
HDR highlights make no edges, staircase gets blended values and keeps interior and energy, bright edge without NaN or rim,
garbage input, sharpen contrast and strength, determinism, pipeline restored, size change), config tests.
