# Shell fur

Real strands of fur for furred creatures, drawn by the mod on top of the game's own meshes. Development and calibration
started with the charr (every measurement below comes from charr texture atlases and
the charr skin shaders); other furred creatures (NPC animals, pets, other furred races) are next, through the same selector and
discriminator. The sections v2..v17 record how each problem was found and solved, in order; the current behaviour is the sum
of them, summed up in "What it does", "How" and "Keys".

Status: unit, WARP, synthetic- and real-atlas-tested; strands out of the silhouette, the flicker fix and the per-island
discriminator are verified in the game; "What to check in the game" lists what still needs a look.

## What it does

Real fur in the style of [UnityFurURP](https://github.com/hecomi/UnityFurURP) by hecomi (credit: the look and the shell technique follow it): soft, dense, slightly clumped strands that stick OUT of the silhouette, darker at the roots,
lit like the body underneath (they take its colour), with a back-lit fuzzy edge against a bright background and a little wind and gravity
at the tips. The old `fur_look` (strand modulation on the mask) stays as the fallback: it runs where there are no shells (shells off, too far,
a draw without shells); where the shells left a strand fragment they REPLACE it (v2: before, both ran on the same pixels).

## How (`src/shellfur.rs`, `shaders/shellfur.hlsl`, composite in `shaders/light_adjust_hdr.hlsl`)

1. **Record.** The fur mask (`furmask.rs`) already re-issues the selected fur draws of the opaque pass. A masked draw that the SHELL selector
   also picks is recorded at that moment (still in the `Draw*` hook, before the engine's draw): vertex shader, input layout, topology, vertex /
   index buffers, vertex-shader views and samplers, rasterizer state, viewport, the depth buffer, the draw call and a GPU COPY of its vertex
   constant buffers (bgfx rewrites the same buffer for the next draw: the copy keeps this draw's bone matrices). At most 24 per frame.
2. **Replay as shells.** At the start of the light-adjust draw (`datapath::bind_current`, right after the fur mask's occlusion check), once
   the depth of the frame is final, every record is drawn again into the mod's own R32_FLOAT target with the engine's vertex shader, a
   GEOMETRY shader of the mod and the strand pixel shader. This is a mod-owned pass whose result only enters the picture through the
   substituted light-adjust shader (data-path rule); the engine's pipeline state is restored (`StateGuard` + the IA buffers, vertex-shader
   constant buffers / views / samplers and the geometry shader's b0 / t0 / t1).
   * The geometry shader is GENERATED per vertex-shader output signature (like the fur mask's uv shader: its inputs must sit on the vertex
     shader's registers; `gs_hlsl`, compiled on a background thread, link-checked with `verify_link`). With `[instance(32)]` it emits each
     triangle once per shell: instance 0 is the base layer (on the surface), the others are pushed out along the normal by `h x length`
     (h = 0..1). The view space is rebuilt from the clip position with the AO's projection model (tan half fov 0.5, the viewport's aspect);
     the root's device depth is kept (the shells do not use the hardware depth test).
   * Normal: `shell_normal=face` (default): the triangle normal, oriented by the rasterizer's front-face winding. `TEXCOORDn`: that vertex
     output is taken as a per-vertex normal and turned into the rebuilt view space by the rotation that maps the triangle's mean vertex
     normal onto its face normal (works for a world- or a view-space normal; smoother shells; falls back to the face normal per vertex when
     the value does not look like a unit normal).
   * Units: the strand length is in cm. The engine's view units per metre are MEASURED (v2): a second geometry shader of the same layout
     (`gs_measure`) compares the clip w with the frame's linear depth (metres = normalised depth x `depth_unit`) at each triangle's own visible
     pixels (a vertex counts only where the engine's final device depth shows that very vertex) and adds log2 of every agreeing triangle's
     ratio into a 1x1 RGBA32F target (ADD blend); the CPU reads that sum back two frames later from a staging ring (`DO_NOT_WAIT`, never a
     stall) and keeps ONE unit for every triangle: the first good frame sets it, then a running mean (15 % per frame, at most 1.5 % step) that
     is only published when it moved by more than 1.5 % (hysteresis). `shell_unit` > 0 fixes it. Until the first read-back (two frames after
     a load) the geometry shader falls back to the per-triangle guess of v1.
   * Gravity pulls the tips down (view space; a level camera), wind sways them (`shell_wind`). Both grow with h squared. The wind phase is
     computed on the CPU in double precision (sin / cos of two slow phases): continuous, never wraps.
   * Guard: a strand is never longer than 64 px at 1440p (scaled with the height) on the screen: a wrong unit cannot make a fur ball.
   * LOD (v2): every shell up to half of `shell_lod_distance`, then the strands get continuously shorter (to 35 %) and fainter (the fade goes
     into the key's coverage), none past it. The distance is the triangle's centroid with the frame's unit, so neighbours agree. (v1 dropped
     shells per triangle in whole steps.)
3. **Strands** (`ps_shell`). The mesh uv is cut into cells (`shell_density` per uv unit at the densest level, coarsened by powers of two where
   a cell would be smaller than 2 px; the two levels around the right size are blended BY COVERAGE, a level change is a fade). Each cell holds
   one strand: a cone with a random root position pulled towards its 4x4 clump, the clump's strands leaning the same way towards their tips
   (`shell_clump`: directional clumping), a random length (60..100 % of the shell height), root diameter `shell_thickness` cells, tapering to
   40 %, the last quarter fading out (soft tips). The coverage is soft (v2): the edge is box-filtered over one pixel and a strand thinner than
   1 px is drawn 1 px wide and fainter by the same ratio, so a strand that moves by a fraction of a pixel changes a little, it does not blink.
   **Occlusion:** a fragment is dropped where the final linear depth at the pixel is nearer than the strand's ROOT (its own depth, from the
   geometry shader) by more than the strand can reach (`shell_length` + 5 mm + 0.6 % of the depth): cloth, a hood or an arm in front hides it,
   including strands whose root is hidden; the sky and the background do not.
   **Output:** the packed key `hq << 24 | a << 20 | dx << 10 | dy` (hq = 1 + round(h x 125); a = coverage x LOD fade in 1/15 steps; dx / dy =
   offset from the pixel to the root in quarter pixels + 512, +-128 px) as the bits of a float. Which fragment a pixel keeps (v2): the best
   SCORE through the shells' own D32 depth buffer (`SV_Depth = 1 - score / 2`, test LESS): score = a (0.75 + 0.25 h) for a strand, 0.2 x fade
   for the base layer. A covering strand beats the faint edge or tip of a higher one, among equally covering ones the higher shell wins, the
   base layer only where no strand covers. Deterministic (equal scores keep the first fragment in draw order): identical input = identical
   output. (v1 kept the highest shell with a MAX blend: with soft tips that would have been the faint tips everywhere.)
4. **Composite** (`shell_look`, light-adjust substitute, `t24` + `b13`). Where a key is set and its root lies on the (validated) fur mask (read
   bilinearly at the anchor: a root at the mask's edge fades, it does not toggle): the colour of the scene AT THE ROOT (read bilinearly at the
   quarter-pixel anchor) times the AO / contact factor of this pixel when the strand lies over the body, times the coat's self-shadowing
   `1 + rd (D0 - (1 - h)^2)` (rd = `shell_root_dark`, D0 = 0.82 the mean depth of the visible fragments, calibrated in the WARP scene): deep
   fragments (roots, the base layer = the skin between the strands) darker, the tips brighter, the AVERAGE of the coat = the body's (WARP:
   1.003). What shows through a partly covering strand over the body is the base layer's colour (more fur), not the bare surface.
   **Detail** (`shell_detail`, v2): over the body a strand only paints where the surface under it looks like its root (luminance ratio and
   chroma within a tolerance, then a soft falloff): the eyes, the nose, the mouth, dark markings and the horns' outline stay clear of the
   strands of the fur around them, and their own strands do not spill onto the coat (the smeared face of v1). Strands outside the body get a
   back light from a bright background (`shell_rim`, strongest at the tips, nothing against a dark background, released in fog and water).
   Coverage: the key's a x 0.95 (root) .. 0.7 (tip) x detail x `fur_strength`. No channel above max(its source peak, 1.0). `shell_debug=1`
   paints the shell pixels red (root) .. yellow (tip). A second light-adjust draw in the same frame binds the same shells again.

## v2: the flicker and the calibration

Verified in the game on v1: the strands work, but the fur flickers and the coat looks washed out with a grey-white smear over the
face (eyes / muzzle), the mane strands were good. Causes found in v1 (code + captures) and what v2 does:

1. **Sub-pixel strands with a 0/1 coverage** (main cause). v1 cells were 1.5 px with a root diameter of 0.9 cell (1.35 px) tapering to 0.54 px,
   each fragment in or out of a disk: under the charr's constant idle animation every strand pixel toggles. v2: soft, box-filtered edges, a 1 px
   minimum width (fainter instead of thinner), 2 px cells, soft tips. WARP: a third-of-a-pixel camera move changes 0.2 % of the fur's pixels by
   more than 0.1 (the scene without shells: 1.5 %).
2. **The density levels were switched per cell by a hash vs the level fraction**: the fraction follows the uv derivative, which the animation and
   the camera change every frame, so cells popped between the fine and the coarse lattice. v2 blends the two levels by coverage.
3. **The unit was guessed per triangle per frame**: a triangle with no two agreeing samples (silhouettes, vertices on another surface's pixels,
   thin parts) got NO shells at all (strands and base layer) that frame, and an agreeing one could still be 1.5 % off, which moves the root depth
   used by the strands' occlusion test by up to 7.5 cm at 5 m, more than its 4.5 cm tolerance: strands of whole triangles came and went. v2: one
   measured, smoothed unit with hysteresis for every triangle (`status_shells=` shows it) and a relative depth tolerance. WARP (ideal depth, so
   the effect is small there): at 4.5 m the per-triangle guess lost 70..115 strand pixels per frame, the frame unit none.
4. **LOD in whole shells per triangle** (from 10 m at the default 20 m): continuous now (length and fade).
5. **Highest-shell MAX blend + nearest-pixel root colour + a 0/1 root mask test**: v2 picks by coverage score, reads the root colour and the mask
   bilinearly at the quarter-pixel anchor.
6. Also: the wind time wrapped every hour (now exact sin / cos from the CPU); a second light-adjust draw in a frame got no shells (now bound again);
   the status text no longer touches the render thread's lock.
Ruled out from the captures: the records (3..4 draws, never near the cap of 24; the order of the draws does not matter, the selection is
order-independent), the bone matrices (copied per draw), the fur mask's occlusion check (it erases what the robe and the hood cover; the large
`would erase` shares are the body under the robe, drawn later; v2 reads the mask bilinearly anyway). The `e0189185` draw (count 1854) appears in
2 of 7 captures only (another mesh in view), not a per-frame alternation.

**Look.** The washed-out coat: v1 ran BOTH the old fur look and the shells on the same pixels, took the root colour before the AO, and its
darkening / sheen did not keep the average (0.83 of the old). v2 replaces the fur look where the shells are, gives the strands the pixel's AO and
keeps the average of the body (1.003 in WARP). The smeared face: `shell_detail` (WARP: a dark eye at a grazing angle keeps its brightness, 0.064
with the rule vs 0.216 without, the engine's 0.065). Finer: density 1200 (was 800), thickness 0.6 (was 0.9), soft tips; wind 0.15 (was 0.3);
root darkening 0.6 with the average kept (was 0.65 with a net darkening).

**Selector.** The captures list per frame `[vs ba0709f4 ps 1d50763f vcb 16464 count 3354 / 17694 / 2712 -> shells]` (all three on the
charr's own 4096 skin textures), sometimes `[vs a5c9d651 ps e0189185 count 1854 -> shells]`, and the rigid 208-byte draw left out. In the
captures the robe, hood and armour show no strands (they are other pixel shaders or covered), so nothing showed cloth with shells: the default
selector stays (confidence: medium-high for the 3 skin draws; the 1854 draw is unidentified, `shell_counts=-1854` removes it if it is not fur).
The horns are part of the head mesh: they get strands rooted on themselves (grey on grey); `shell_detail` keeps their outline clean.

## v3: only the texels that are fur (horns, claws, nose)

In-game screenshots of v2: grey fuzz on the HORNS (they are part of the head mesh), white feather-like strands on the CLAWS / fingers of the
hand, strands on the nose / muzzle. Those parts are the same draw (same mesh, same textures) as the fur, so no per-draw selector can separate them:
the decision must be per texel.

**Evidence for the channel.** The fur draws (`ps 1d50763f`) bind exactly two engine textures: `t0` 4096 BC3 (colour + alpha) and `t1` 4096 BC5
(a two-channel normal map); `ps cb0` is 48 bytes. There is no dedicated mask texture, so the only per-texel candidates are the colour, the normal detail
and `t0.a`. The game's charr textures were decoded from a copy of Gw2.dat (`gw2tex extract`, 149 charr colour textures from the Remastered
project's `charr_device_roles.json`, plus armour composite pieces) and the alpha measured per material: on the hair / fur / mane strands the alpha is a
tight 0.50..0.61 (median 0.50..0.61 in 45 textures, 90th percentile at most 0.65); on hard, glossy elements of the same textures (bone beads,
metal rings, grey hard parts) 0.62..0.95 (medians 0.62, 0.72, 0.87); the strand cut-outs are below 0.3. So the colour alpha is a gloss map with
matte fur near 0.5. Keratin (horns, claws) and the wet nose are expected to be glossier than fur, so `t0.a < 0.66` = fur. Confidence: MEDIUM. The
charr's own body texture (the composited 4096 atlas with the horns, claws and nose) could not be found among the extracted files, so its alpha on the
horns is not measured; the threshold is tunable live and the debug capture says what share of the coat it removes (below). The colour alone is NOT a
discriminator on a charr: the claws are ivory like the coat (the WARP claw has the coat's colour on purpose and only the gloss finds it).
Fallback when the gloss does not separate them: `shell_uv_exclude` rectangles (hand-placed with the uv printed by the debug capture).

**How.** In the fur mask (`furmask.rs`): the uv mask shader (`uv_ps_hlsl` + `FD_HLSL`) reads the selected channel of the engine's texture that is
still bound at the re-issued draw (`t0..t3`), with `Load` at a FIXED mip (the level with at most 512 texels across) and a hand-made bilinear filter
(wrapping): no sampler, no derivatives, so the value at a uv never depends on the camera (no flicker). `a` of the mask = the fur weight
`1 - smoothstep(th - soft, th + soft, v)` (`>`: the other way) x `1 - smoothstep(-f, f, d)` per excluded rectangle (`d` = signed uv distance to its
border, `f` = 0.004). Its constants are the shader's `b13` for that draw only (`shellfur::fd_cb_values`; the engine's b13 is put back). Because the
fur mask carries it, everything that reads the mask follows: the shells' composite (a strand whose ROOT is not fur is dropped), the old fur look,
the magenta `fur_debug`. The shell replay also binds the mask as `t1` of the strand pixel shader and drops strands rooted on non-fur texels
before they take a pixel (else such a fragment could hide a fur strand and leave a hole). Strands of the fur around a horn may still lie OVER
the horn (they grow from the fur). The rim light (`rim.md`, its own R8 mask) is not discriminated: a rim on a horn is right.
Off / the selected slot unbound for a draw and no rectangle for it: nothing is bound, the mask and the shells are bit-identical to v2 (tested).

**Debug capture.** `status_shells=` ends with the discriminator (`fur where t0.alpha < 0.660 (soft +-0.050), N excluded uv rects [...]`) and the probe of
the last capture's frame: the share of the fur draws' pixels it removed and the mesh uv + fur weight at the screen centre and under the mouse (the
nearest fur-draw pixel within 48 px). See `debug-mode.md`.

**WARP (`shellfur_tests.rs`).** The coat sphere with a 1024 material texture (with mips) whose alpha is the fur's 0.47..0.59 and 0.86 on a horn
(grey, at the upper left silhouette), a nose (dark pink, mid face) and a claw (the coat's colour, lower right silhouette), ramping over 0.02 uv:
without the discriminator 631 / 3281 / 1563 strand pixels are rooted inside them, with it 0; the fur away from them keeps its shells (29464 of 29467
px) and its picture (all 9596 far pixels within 0.02); the weight goes 1.000 -> 0.995 -> 0.609 -> 0.014 -> 0 across the ramp (soft); the rectangles
alone do the same (0 roots inside, shader weight = CPU mirror within 0.0014); repeated frames bit-identical, a third-of-a-pixel move changes the fur
by a mean 0.009; `shell_fur_mask=off` and a selected slot that is unbound give the same output bit for bit; the v2 scenes (no material texture)
are unchanged with the default config. Renders (suffix `-v3`): `shells-regions` / `shells-regions-off` (with / without), `shells-regions-mask`
(the fur weight, white = fur), the 4..8x zooms `shells-regions-{horn,nose,claw}-zoom` / `-off-zoom`, `shells-rects` / `shells-rects-mask`.

**Keys** (all saved, live, in the overlay under "Shell fur", clamped, NaN-safe):

| key | range | default | meaning |
|---|---|---|---|
| `shell_fur_mask` | `off` / `t0..t3` `.r .g .b .a .l` + `<` or `>` | `t0.a<` | which texture channel says "fur": `<` = fur below the threshold |
| `shell_fur_threshold` | 0..1 | 0.66 | where the fur ends |
| `shell_fur_soft` | 0.01..0.5 | 0.05 | half width of the soft edge |
| `shell_uv_exclude` | up to 16 `u0 v0 u1 v1[@count]`, `;` between | empty | uv rectangles without fur (by hand; v7: not needed, the per-strand discriminator finds horn / claws) |

**Check in the game / tune.** `fur_debug=1` + a capture: the horns, claws, nose and lips must NOT be magenta, the coat and mane must be. In
`status_shells=`: removed share a few % (about 100 % = wrong channel: `shell_fur_mask=off` restores v2, then try `t0.a>` or `t0.l<`; 0 % = nothing
found: raise / lower the threshold by 0.05 steps, or use rectangles). Horn still fuzzy: lower `shell_fur_threshold` (0.60); fur missing on the
coat: raise it (0.72). Rectangles: centre the horn on the screen (or point the mouse at it), capture, add a rectangle around the printed uv.

## v4: the evidence per draw

**What the game showed on v3.** With v3 installed (the capture confirms `fur where t0.alpha < 0.660`) nothing changed: grey fuzz
on the horn, a ring of strands around the muzzle (screenshot of a close-up). The probe of the capture: `233585 px drawn by the fur draws,
1182 not fur (0.5 %), 1212 in the soft edge (0.5 %)`. So in the texture that is really BOUND (the engine's composited 4096 BC3 atlas of
the character, `t0`, + a 4096 BC5 normal map, `t1`, shared by all fur draws: draws logs) the horn has the gloss alpha of
the coat. The v3 numbers came from the dat textures (Gw2.dat), not from that runtime atlas; nothing on disk holds the atlas (the charr's
composite is built by the game; the earlier dumps and the earlier scratch extracts are armour / dye-mask /
other textures). What else is known from the captures: every frame has the same three skinned draws `ba0709f4` (counts 3354, 17694,
2712: they never come and go with what is on screen, so they are not "hands" or "head" switched by the camera) plus the rigid
`59f8b692` draw (count 5214, one matrix, the same atlas, no shells). Which mesh carries the horns cannot be told from counts alone.

**The probe (`src/furprobe.rs`).** A frame that starts with a debug capture probe pending COLLECTS its fur draws (shaders, vcb, the draw
call: count, start index, base vertex, instances; shells yes / no; the engine's `t0` / `t1` resources with their view's mip range and
format) and TAGS the fur mask: `b` (1.0 before; every reader only tests `> 0.5`) becomes `0.5 + k / 64`, k = the draw's index in the
frame + 1 (the uv mask shader's constant `fd_tag`, with a marker so a foreign `b13` never tags). At the probe (the light-adjust draw)
the mips of t0 / t1 the mask shader reads (at most 512 texels across, and 1024 for the detail) are copied back (one wait, debug captures
only); a background thread decodes them (BC1 / BC3 / BC4 / BC5 / RGBA8, `decode`), samples them at every visible fur pixel's mesh uv
exactly like the shader (`sample_bilinear` = `fd_value`, `detail_at` = `fd_detail`) and writes:

* in `status_shells=` (after the discriminator probe): `per-draw probe (...)` with, per draw, `#k <colour> [vs ps vcb count start base
  inst -> shells | no shells; t0 tex <crc> 4096x4096 BC3_UNORM m11 (view ... mips <first>+<n>; sampled at level L = WxH), t1 tex ...;
  N px (x % of the fur px), not fur y %; t0.a p10/50/90 .. % per 1/16 [16 bins]; t0 luminance ...; t0 saturation ...; t1 detail ...;
  t0 detail ...]`, then for the screen centre and the mouse (the nearest fur pixel within 48 px): the draw, the uv, `t0 rgba`, luminance,
  saturation, `t1 rg`, `t1 detail`, the fur weight;
* in `GW2Graphics\debug\`: `furtex-<crc>-rgb.png`, `-a.png` (alpha as grey), `-detail.png` (the detail metric) of each bound texture at
  its ~1024 level (once per texture: the crc of the raw level is stable across sessions), and `furprobe-uv-<crc>.png`: the atlas dimmed,
  every visible fur pixel's uv plotted in its draw's colour (white = removed by the discriminator), a white cross at the mouse uv and a
  yellow one at the centre uv. With that picture the horn / claw / nose islands of the atlas can be SEEN and a rectangle or a channel
  chosen from real values.

**`shell_debug=2`.** Tags every frame; the composite paints each strand with the colour of the draw its ROOT belongs to (red, green,
blue, yellow, cyan, magenta, orange, purple by the draw's index; brighter towards the tip; white = untagged), the bare body of each draw
gets a light tint of its colour and the texels the discriminator removed turn black. The legend is the per-draw probe's list.

**New channels.** `shell_fur_mask=t<slot>.d<|>`: the detail of `.rg` (a normal map's xy): the rms deviation over the 3x3 texels around
the nearest texel at the ~1024 level, x2, saturated (noisy fur strands high, a smooth horn / claw / nose low: `t1.d>` with a threshold
from the probe's histograms). `t<slot>.s<|>`: the colour saturation. WARP: a coat with a bumpy normal map and smooth horn / nose / claw
regions: `t0.d> 0.3` roots none in the regions, the fur keeps its shells (29464 of 29467 px), the shader's weight = the CPU mirror on
51372 of 51379 px.

**Real-path checks (code).** The mask shader reads the SRVs the engine bound for that very draw (it runs inside the re-issued draw; only
its pixel shader and `b13` are swapped), at the level counted from the VIEW (`GetDimensions` of the view: a streamed view that starts at
a lower mip is handled; the probe prints the view's first mip and count), with the uv of the vertex shader's `TEXCOORD0` (which
`ba0709f4` computes as the mesh uv times `cb0[5..6]`: the atlas uv the engine's pixel shader samples with; the overlay PNG shows whether
the uvs land on the body's islands). An sRGB view would change the colour channels, not the alpha (the probe prints the view format).
Everything that reads the mask follows the discriminator (shell roots, the old fur look, `fur_debug`); the rim does not (by design).

**Default.** Unchanged (`t0.a<` 0.66): it removes almost nothing in the game, but no other channel is measured on the real atlas yet;
switching blind could strip the coat. Confidence that the next probe shows the separating channel: high for the evidence, unknown for
which channel. If no channel separates, `shell_uv_exclude` rectangles from the overlay picture (per draw with `@count`).

| key | change |
|---|---|
| `shell_debug` | 0 / 1 / 2 (2 = a colour per fur draw + black removed texels) |
| `shell_fur_mask` | also `.d` (detail of rg) and `.s` (saturation) |

## v5: what the real captures showed, and the rule

**The captures.** `GW2Graphics\debug\` (`pair-*`, `latest.*`, `furtex-*`, `furprobe-uv-<crc>.png`).
Two facts about the files first:
* `shell_debug` was **0** in every capture (the config dump in each `pair-*.txt` says so): the `-mod.png` carry no draw tint, so which draw owns
  a strand had to come from the probe and the atlas, not from the picture.
* The per-draw report in `pair-N.txt` is measured in the frame right AFTER the PREVIOUS capture (the probe is requested by a capture, runs in
  the next frame, is analysed on a thread and printed by the next capture). Proven by the mouse position: the mouse pixel each report names matches the picture of the
  capture before it, not its own. The `furprobe-uv-*.png` is written at once, so it belongs to the capture of the same time (its colours match
  that frame's draw count). v5 says so in the report and writes the
  report next to the PNGs (`dev\furprobe-last.txt`), so picture and numbers of one frame sit together.

**(a) Separate draws? No.** Every frame has the same skinned draws, all on ONE atlas (`t0` 323868e4 4096 BC3, `t1` 3c23d29e BC5):

| draw (index count) | what (uv plot + atlas) | t0.a | t0 saturation | share of fur px |
|---|---|---|---|---|
| 3354 | head: face island uv 0.375..0.69 x 0.50..0.75 (+ pieces at 0.375..0.5 x 0..0.25), INCLUDING the horn and the fang | 0.40..0.44 in 96..98 % | p90 0.12 | 28..58 % |
| 2712 | body, arms, hands, feet (uv 0.69..1 x 0.5..1), INCLUDING the claws | mode 0.44, p10 0.19..0.31, p90 0.50..0.56 | p90 0.44..0.50 | 29..63 % |
| 17694 | mane / hair strands (top-left hair cards) | 0.56 | 0.06 | 3..8 % |
| 5214 (rigid, vcb 208) | hair cards, no shells | 0.56 | 0.06 | 4..10 % |

The horn lives in the head draw, the claws in the body draw: no `shell_counts` exclusion can take them out (it would take the head or the
whole body). The decision must stay per texel.

**(b) Where they are in the atlas, and what separates them** (the atlas at its 1024 mip, `furtex-<crc>-rgb/-a.png`, measured per class over
the texels of the uv plot; flatness = the new channel `f`, 5x5, see below):

| class | uv (1024 atlas) | rgb / note | alpha p10/50/90 | saturation p50 | flatness p1/p10/p50 |
|---|---|---|---|---|---|
| horn (head draw) | flat grey crescent ~0.597..0.683 x 0.569..0.678 (diagonal: a rectangle would also cut the fur below it) | 0.51 neutral, a dye colour without any painting | 0.40/0.40/0.41 | 0.008 | 0.000/0.000/0.000 (p90 0.018) |
| head fur (visible texels of 3354) | | | 0.39/0.40/0.42 | 0.03 | 0.026/0.048/0.125 |
| body fur (visible texels of 2712) | | clipped white in places (lum 0.99..1.0) | 0.18/0.41/0.44 | 0.012 | 0.035/0.063/0.219 |
| claws (2712) | brown islands 0.69..0.75 x 0.88..1.0, 0.70..0.77 x 0.76..0.82, nails 0.82..0.87 x 0.78..0.83 | brown | 0.42/0.56/0.65 | 0.45 (all > 0.27) | 0.045/0.078/0.251 |
| fang (3354) | tan triangle 0.505..0.56 x 0.70..0.75 | tan | 0.50 | 0.36 | |
| nose / nostril | dark diamond 0.50..0.52 x 0.60..0.64 | lum 0.26 | 0.40 | 0.03 | |
| mouth inside, gums, lip line | 0.656..0.69 x 0.56..0.61, 0.53..0.56 x 0.67..0.69 | dark red / tan | 0.40 | > 0.2 | |

Why the crescent is the horn (no mouse sample landed ON the horn; confidence medium-high): it is the only large non-fur island of the head
draw; its shape is an unrolled cone (a fan); it is a flat dye colour while its normal map has ridges (the horn's rings in the game); it was
not visible in the frame from below (chin and claws in view) while all fur around it was; the probe under the horn base
read uv (0.6304, 0.6656), the fur right at the crescent's lower edge. The claws are certain: the only texels the old `t0.a< 0.66` removed in
the uv plot (the white points) lie in the brown islands, and every brown texel is saturated.

What separates, per channel: **alpha** (gloss) no: the horn has the coat's 0.40, the claws overlap the fur (0.42..0.73). **Normal detail**
(`t1.d`) no (horn 0.07 like fur 0.04..0.09). **Saturation**: perfectly for the claws, fang, gums, lips on the WHITE coat (claws > 0.27, fur
p99 < 0.2 outside the mouth), but a dyed coat (brown, red) is saturated itself: not generic. **Colour flatness**: the horn is a flat dye colour,
painted fur never is: horn 0.000, head fur p1 0.026, body fur p1 0.035 (clipped white excepted, see below). It is relative to the brightness,
so a dark coat with the same painting gives the same value (dye-invariant; tested). Generic as far as the evidence goes (one atlas): the
horn colour of every charr is a dye channel.

**The new channel `f` (colour flatness).** `t<slot>.f`: the rms deviation of rgb over the 5x5 texels around the nearest texel at the mip with at
most 1024 texels across, x2, divided by the rms of the window's mean colour (a paint that multiplies the colour gives the same value
on any dye); 1 where the brightest mean channel is above 0.97 (clipped white has no detail to
measure: counted as fur). In `fd_flat` (mask shader, `Load` at a fixed mip: no flicker) and `furprobe::flat_at` (the CPU mirror, the probe).

**Test lists.** `shell_fur_mask` is a list of up to 4 tests separated by commas, each optionally with its own threshold (`t0.f>0.025`; without
one, `shell_fur_threshold`); a texel is fur when EVERY test says so (the weights multiply). The soft edge of a test is `shell_fur_soft`, at most
half the distance of its threshold to 0 / 1 (`t0.f>0.025`: 0.0125..0.0375). A test whose slot is unbound for a draw is switched off for that
draw (`fd_for_draw_slots`). Constants: tests 2..4 in `b13[19..21]`, their thresholds in `[22]` (the tag stays at `[18]`).

**Default (code): `t0.a<, t0.f>0.025`.** Removes the horn (94 % of its texels below 0.025: median weight 0, p90 0.12) and keeps the coat (head
fur p1 0.56, p2 0.74, p10 and above 1; body fur p1 0.97; 1.0 % of the head fur and 0.1 % of the body fur below the threshold); the gloss part still removes the glossiest claw texels. The claws, fang and lips need the colour, which is
only safe on a white / pale coat: that is the pale-coat line below, not a default.

**(c) The muzzle ring.** Strands at the snout's silhouette are rooted on the texels whose normal is perpendicular to the view: in a side view
that is the face's CENTRE LINE. The head island is a mirrored half face whose centre line is its straight right edge, u = 0.5635 from
v 0.496 down to the lips (the nostril sits 0.04 inside it, the nose pad and the lip line touch it). A narrow band along it removes the ring
from the nose bridge to the lips; the gums / lip line themselves go with `t0.s<0.2`, the nostril is protected by `shell_detail`. Confidence
medium (the seam is read from the atlas layout, no capture had the mouse on the nose).

**`config.ini` lines for a charr:**

```
shell_fur_mask=t0.a<, t0.f>0.025, t0.s<0.2
shell_uv_exclude=0.546 0.585 0.566 0.715@3354
```

(`shell_fur_threshold=0.66`, `shell_fur_soft=0.05` stay.) The config.ini saves every key, so the new code default does NOT reach an existing
config.ini: the `shell_fur_mask` line must be written there in any case. For a dyed (non-white) charr use the default line without `t0.s<0.2`.

**The rim light follows the mask (v5).** `rim_look` multiplies its gate by the fur mask's weight where a fur draw with uv lies (`b > 0.5`):
no rim / subsurface on the horn, claws, nose; armour, unmasked and erased pixels (`b = 0`) unchanged, bit for bit (WARP). The old fur look
already followed (it reads the same mask).

**Probe (v5).** Per draw a `t0 flat` histogram, at the centre / mouse the `t0 flat` value; the report says which frame it measured, and the
background thread writes it to `dev\furprobe-last.txt` with the PNGs of the same frame.

**WARP / host tests (v5).** The coat's material texture now has painted strands (+-8 % per texel, relative flatness ~0.09) and flat regions:
`t0.f>0.025` alone roots no strand in the horn / nose / claw, the coat keeps its shells (> 95 %, min weight > 0.97), the shader's weight =
`flat_at` on > 97 % of the pixels; the default and the pale-coat line do the same; the measured real classes (table above) are classified like
the game (`the_real_atlas_classes_are_classified_like_the_game`); flatness dye-invariance and clipped white; list parsing, round trip,
constants, unbound slots; the rim test above.

**What to check in the game (v5).** With the two lines: (1) horn: no fuzz on the horn's top edge (the fringe gone); (2) claws: no
white strands on the fingers / toes; (3) the muzzle: the ring around the snout front and lips gone, the cheeks and the forehead keep their fur;
(4) the coat elsewhere unchanged. Then `shell_debug=2` (the mask's removed texels turn black) and `fur_debug=1` (magenta = fur) with a dev
capture: the horn, claws, fang, lips must be black / not magenta, the coat coloured. In `status_shells=`: `not fur` a few % (the horn and
claws), and per draw `t0 flat`. If the ring stays under the CHIN: a capture with the mouse on the chin (the report of the NEXT capture
gives its uv) and add a second band there. If the horn's base still fuzzes: `t0.f>0.03`. If fur is missing next to the horn: `t0.f>0.02`.

## v6: the silhouette strands

v6 also shipped an exclusion picker in the overlay (point at a part in the game, click, the rectangle goes to `shell_uv_exclude`). A
hand-placed tool goes against a mod that adapts by itself and works out of the box: v7 removed it entirely (`furexcl.rs`, its overlay controls,
F9 / left click, the green / blue preview paint and its `b13[2..7]`, its mask tags and read-backs; the files are kept beside the source
as `src/furexcl.rs.removed` / `src/furexcl_tests.rs.removed`, outside the build). What stays from v6:

**The silhouette strands.** The fringe on the horn's edge and the ring around the muzzle are strands
of triangles at the silhouette: their shells are pushed out along the normal, so the strands stand over the background while the
ROOT is on the horn / the snout. Up to v5 a strand was dropped when the fur mask AT THE ROOT'S SCREEN PIXEL said "not fur". At a
silhouette that pixel is the edge itself; and where the root is HIDDEN (the horn's far side, a horn base behind the mane, the far side
of the muzzle, the underside of the chin) that pixel shows whatever is in front, usually fur (weight 1): the strands of a hidden horn
root survived and stuck out at the outline. v6: the strand pixel shader tests the `shell_uv_exclude` rectangles of its draw on the
fragment's OWN mesh uv (`ps_rect_values` in `b0[2..19]`, `rect_keep`; the shells share the base triangle's uv, so it is the root's):
every layer of an excluded root goes, the base layer included, wherever the root projects. WARP, with the screen root test switched
off: 633 / 4854 / 1583 keys (base layer 1595) rooted in the horn / nose / claw without rectangles, 0 in every layer with them, the coat
keeps all its shells (29467 of 29467 px). The texture tests (`shell_fur_mask`) still go through the screen mask (the strand pixel
shader does not see the engine's textures): a hidden root on a texel the texture test removes can still leak; rectangles do not.
The old fur look and the rim light follow the rectangles through the mask as before.

**Test (v6, kept).** `shellfur_tests.rs`: excluded roots lose every layer by their own uv (above). The picker's tests went with it.

## v7: the transparency fix and the per-strand fur discriminator

**Seen in the game.** The fur sometimes made NPCs transparent (a close-up of a charr's arm):
the brown leather bracers and the dark cape look semi-transparent, with white-blue speckles of fur over them and a ghosted cape edge.
And, with `t0.s<0.2` removed from the config (it stripped coloured coats), the claws had fur again.

**The cause (evidence: code + captures + WARP).**

1. *Strands painted over a nearer object, with a colour from somewhere else.* The composite paints a strand with the scene's colour AT ITS
   ROOT's pixel (the anchor). Up to v6 the strand pixel shader dropped a fragment only where the scene at its pixel was nearer than the
   root by more than the whole strand reach (`shell_length` + 5 mm + 0.6 % of the depth: 4.5 cm + 1.8 cm at 3 m). Anything lying on the fur
   closer than that (a bracer, a cape, a belt, another character in contact) was painted over by the fur next to / under it: the
   speckled fur over the bracers. And no strand checked that its root was VISIBLE: the composite's root test is the fur
   mask at the anchor, and the cape, the armour and every other character share the fur pixel shader (`1d50763f`), so a root hidden
   behind any of them still read "fur" at its anchor and its strand took THAT surface's colour (the cape's, an NPC's) and dragged it up to
   64 px across whatever lay there: the ghosted cape edge, NPCs that look transparent (the colour of what is behind or in front shows through).
   WARP, the v6 tests emulated in the v7 shader (no root test, the root depth + 4.5 cm + 0.6 %): a cloth 2 cm in front of the fur: 500
   cloth pixels get a strand key, 68 visibly change, 291 strands are rooted behind the cloth; two overlapping characters: 59 strands of the
   one behind are rooted where the front one hides it (their colour would be the FRONT character's: both are fur draws). v7: 0 / 0 / 0
   and 0, the cloth and the front character bit-identical to the output without shells (`v7_a_nearer_object_lying_on_the_fur_stays_pixel_
   identical`, also at 1.2 cm and 12 cm; `v7_two_characters_overlapping_never_show_each_others_hidden_strands`).
2. *Shells on every NPC's armour and on human skin.* A capture in the Black Citadel (`latest.txt`): `24 draws drew
   shells (20 picked of 212 fur draws, 188 not listed)`. The per-draw probe (`furprobe-last.txt`) lists 32 draws on 9 different atlases;
   the atlases written to `dev\` show the composite of each character: armour pieces AND skin in one texture, and four of them are HUMAN
   (atlases `1ff2e7a8`, `3ce54b1d`, `a1f66505`, `fdcbd489`: a human face in the skin region). The uv plots of atlases `71695927`,
   `ab4a374a`, `d4a5f575` show fur draws covering the ARMOUR regions; draw #30 (`count 1086`, atlas `e884ef71`: dark leather and a skull,
   saturation p50 0.44) was 50 % of all fur pixels in that frame. Every skinned draw with that pixel shader got shells: capes, leather,
   metal, human skin, each with strands of their own displaced colours and a fuzzy outline over the background.
3. Ruled out: (b) the coverage blend: the composite is `lerp(under, strand, a x ...)`, a proper blend (`v7_strands_over_the_background_
   are_a_coverage_blend`: a strand edge of 1..4/15 coverage changes the sky by 0.035 on average, a covering one by 0.21; no key = no
   change); (c) the mask's tag channel: every reader tests `b > 0.5` (tested since v4) and only the probe / `shell_debug=2` tag now (the
   picker's tagging is gone); (d) the 64 px cap: anchors are +-128 px, the clamp holds (tested since v1); (f) the discriminator only
   lowers a strand's coverage, it never cuts a hole into the body.
4. (e) Recorded buffers rewritten before the replay: not seen in the captures, but possible for NPC meshes that come from shared pools
   (`start 118476`, `start 124464` in the probe). A record with a DYNAMIC vertex or index buffer is now refused (`dynamic vertex / index
   buffer` in `status_shells=`); a DEFAULT buffer rewritten in place is caught by the root test (its roots do not lie on the surface the
   screen shows): WARP, the mesh moved 25 cm after the draw: 2122 keys instead of 76453, none over the background in bulk
   (`v7_buffers_changed_between_record_and_replay_never_ghost`).

**The fix (`shaders/shellfur.hlsl` `ps_shell`, `root_visible`).** Fail open, no shells rather than ghosts:
* **The root must be visible.** The engine's DEVICE depth at the anchor (the best of the 3x3 pixels around it) must equal the root's own
  device depth within 2e-5 + twice the local slope (unit-free; a few D24 steps, the characters' depth bias included); the root's visible
  linear depth is taken at the pixel that matched. Otherwise the strand goes. Without a readable device depth: the linear depth within
  2 % + 1.5 cm (the unit's error).
* **Nothing nearer than the root under a strand.** A fragment is dropped where the scene at its pixel is nearer than the root's visible
  depth (or the fragment's own depth for a strand leaning away) by more than 5 mm + 0.3 % (`FRAG_TOL_M`, `FRAG_REL_TOL`; was the strand
  reach 4.5 cm + 0.6 %). A convex coat behind its own strands is farther: unaffected (the v2 scenes keep their numbers, mean brightness
  1.003 of the body).
* **The composite** gates a strand with the fur mask's PRESENCE at the anchor (a fur draw's surface: `b > 0.5` or `a`), no longer with the
  mask's texture tests (those now run per strand, below); the old fur look, the rim and `fur_debug` still use the mask's `a`.
* WARP: two overlapping characters (the front one without shells): 0 keys on it, 0 strands rooted behind it; both with shells: 14163
  keys on the front one, 0 rooted on the one behind (`v7_two_characters_overlapping_never_show_each_others_hidden_strands`).

**The fur discriminator per strand.** v6 left strands on the horns because the texture test ran through the SCREEN mask: a strand
whose root is hidden (the far side of a horn, a horn base behind the mane) read the fur in front of it. v7 binds the recorded draw's
own atlas (its pixel shader's `t0`) to the strand pixel shader and evaluates every strand AT ITS OWN ROOT UV (the fragment's mesh uv):
every layer of a non-fur root goes, wherever it projects. The rule is RELATIVE to the draw's own coat, measured on the GPU every frame:

* **The coat statistics** (`gs_uv` + `ps_feat` + `ps_stats`): each recorded draw is drawn once more in UV space into its own 32 x 32 cells
  (the triangles at their mesh uv); each covered cell gets the atlas features at its uv: the 5x5 texels around the nearest texel at the
  ~1024 mip (Load, fixed mip, no flicker): gloss (alpha), saturation, value, colour flatness, hue. One full-screen draw reduces each
  draw's cells to two texels: the share of cells in the fur gloss band 0.33..0.50, the median flatness, the cell count, and the
  medians of gloss / saturation / value plus the saturation-weighted mean hue. No CPU read-back on the path (`status_shells=` reads a
  copy a few frames late, never waiting). GPU = CPU mirror (`coat_stats`) within 0.0004 (`v7_the_gpu_coat_statistics_match_the_cpu_mirror`).
* **Draw test** (`ft_draw_weight`): a draw is fur when 40..60 % (soft) of its footprint has the fur gloss and its median flatness is
  0.03..0.05 or more. Measured: the fur texels of every charr atlas have alpha 0.40..0.44 (the skin region of 10 charr atlases: 84..91 %
  of its texels in 0.35..0.45); human skin 0.45..0.55 (`1ff2e7a8` 30 % in the band, `3ce54b1d` 1 %), human skin flatness median
  0.013..0.069; hair cards 0.56; the armour regions mostly above 0.45 (a charr's armour 37 %, `e884ef71` 49 %, `5566458d` 60 %:
  some leather pieces stay in, see Limits). Without a footprint (no atlas, uv shader still compiling): no shells (fail closed).
* **Texel tests** (`ft_texel_weight`, every one relative to the draw's coat; all soft):
  * flat: flatness below min(0.025, 0.4 x the coat's median) = the horn (a flat dye colour; a uniformly smooth coat lowers the threshold);
  * glossy: alpha above the coat's + 0.03..0.07 = the claws (their alpha 0.5..0.7 in EVERY atlas, the coat 0.40..0.44), the fang, nails;
  * keratin colour: the claws and the fang are one UNDYED tan on every charr (hue 15..55, saturation 0.22..0.65, value 0.25..0.9); such a
    texel that differs from the coat (saturation by 0.1..0.2 or 1.3..1.6x brighter) is not fur, where the coat is not itself that colour
    (coat saturation below 0.28..0.38, or its hue 18..28 degrees away from the tan): white, grey, blue, black coats; an orange / tan coat
    relies on the gloss (its light fur has the claws' colour);
  * red: lips, gums, mouth, eyes: red hue (within 15..25 degrees), saturated, more saturated than the coat or 10..18 degrees off its hue.
  `shell_fur_mask=off` keeps only the draw test. `shell_uv_exclude` still works (default empty, not needed any more).

**Evidence per coat (real atlases, `GW2Graphics\debug\furtex-*-rgb.png` / `-a.png`, the ~1024 level, 5x5 features; the coat statistics
of the body region 0.69..1 x 0.75..1 as the body draw's footprint, the head island for the head draw; regions read from the atlas
layout, which is the same in every charr composite).** Share of texels kept (weight > 0.5); the claw rectangles include some fur at
their edges (the floor is ~13 %: on the light-coat atlas 34 of the 77 kept "claw" texels look like the coat):

| atlas | coat (median sat / value / hue) | body fur | cheek | claws | fang | horn (light coats only; elsewhere fur there) |
|---|---|---|---|---|---|---|
| 323868e4 light coat | 0.05 / 0.54 / - | 99 % | 99 % | 15 % | 11 % | 24 % (rectangle edge fur; the flat texels 0) |
| dfe7c25c grey | 0.10 / 0.28 / 37 | 100 % | 100 % | 13 % | 10 % | |
| c91ca4a3 dark blue-grey, spotted | 0.27 / 0.19 / 226 | 100 % | 100 % | 15 % | 9 % | |
| 5566458d grey-brown, scaled | 0.23 / 0.21 / 34 | 92 % | 100 % | 13 % | 12 % | |
| e884ef71 brown | 0.45 / 0.24 / 29 | 98 % | 98 % | 13 % | 10 % | |
| 5100819f brown | 0.45 / 0.27 / 27 | 97 % | 97 % | 14 % | 38 % | |
| 21a45427 dark brown-black | 0.79 / 0.08 / 27 | 98 % | 99 % | 14 % | 9 % | |
| 3c71e3f2 orange tiger | 0.61 / 0.27 / 21 | 98 % | 100 % | 15 % | 9 % | |
| 05a0d288 orange tiger | 0.66 / 0.29 / 34 | 100 % | 100 % | 43 % | 62 % | |
| 71695927 orange leopard | 0.73 / 0.29 / 33 | 100 % | 100 % | 43 % | 45 % | |
| c984dc99 orange, light pattern | 0.63 / 0.24 / 30 | 99 % | 100 % | 40 % | 42 % | |

Confidence: high for the horn (flat) and the claws on white / grey / blue / brown / dark coats (gloss + colour); medium on the three
orange atlases where only the glossy half of the claw goes (the claws' matte base has the coat's colour and gloss: no texture channel
separates it there); lips and gums: the red rule (the mouth regions were read from the light-coat atlas' layout only: medium). The nose is
not removed by a rule (dark, coat-coloured gloss); `shell_detail` keeps it clear, and the root test removes the silhouette ring.
WARP (`v7_the_per_strand_discriminator_keeps_any_coat_and_drops_horn_claw_and_lips`): a synthetic atlas with a flat horn, a glossy tan
claw, a matte tan pad and red lips on a white, grey, dark brown and orange coat: horn / claw / lips 0..1 strands each (texel tests off:
632 / 1564 / 3323), the pad 0 on white and grey (kept on brown / orange, as designed), the coat 35153 of 35153 px keep their shells on
every coat. A horn root hidden behind a "mane" (a second fur draw in front): 0 strands rooted behind it, also with `shell_fur_mask=off`
(`v7_a_horn_root_hidden_behind_the_mane_grows_no_strands`). The real atlas numbers above as a host test
(`v7_real_atlas_texels_are_classified_by_the_coat_relative_rule`: coat median texel 1.000, claw median and glossy claw 0.000 on 7 atlases).

**`status_shells=` (v7)** begins its discriminator part with `per-strand fur discriminator (v7, at each strand's own root uv; texel tests
on: ...): [count N atlas <ptr>: C cells, gloss band B %, flat F, coat gloss G sat S value V hue H -> fur draw | not fur (armour / skin /
hair), no shells | no footprint, no shells | fur x w] ...` per replayed draw (the GPU's own statistics, a few frames late). A record
refused for a dynamic buffer shows `dynamic vertex / index buffer (could be rewritten before the replay)` in the per-draw list.

**Cost (v7).** Per recorded draw one more pass of its mesh without instancing into a 32 x 32 viewport (the uv footprint, 25 texel loads
per covered cell), one 24 x 2 reduction (each pixel loops its 1024 cells), per covering strand fragment 25 texel loads (only after its
coverage test) and 9 depth loads. Targets: 32 x 768 + 24 x 2 RGBA32F, three 24 x 2 staging textures.

## v8: the arm in front, the comb / rectangle, the horn's edge, the paw claws

**Seen in the game on v7** (debug captures): horns much better, a little fuzz
left at a horn's edge and base; strands on the PAW claws (the hand claws fine); a hard-edged pale yellow band with a straight
comb of strands at the top of a tail; a straight comb fringe along the lower edge of a red banner hanging over a charr; and
the remaining transparency: streaks of fur crossing over a charr's glove / arm, a dark-blue band with vertical streaks and floor-coloured
streaks under a tail seen edge-on (the spotted white NPC, atlas `a3e217eb`).

**(1) The transparency through the arm: the root test was too loose at grazing triangles.** v7 accepted a root as visible when the
engine's device depth at the 3x3 pixels around its anchor was within `2e-5 + 2 x slope` of the root's depth, slope = the derivative of the
root depth across the SHELL triangle, capped at 5e-4. At a grazing triangle (a tail strip seen edge-on, the silhouette of a limb) that cap
is reached, and 1e-3 of device depth is several centimetres at 1..3 m (standard Z, `LESS_EQUAL`, an in-game capture). So a root right
behind an arm, a glove, a cape, the floor or a staff "matched" THAT surface; the composite then painted the strand with the colour at the
anchor = the arm's / the floor's / the staff's: fur of the colour of what is in front, i.e. the arm looks see-through, the floor-coloured
and blue streaks under a tail, and very probably a pale yellow band (the character's staff lies along the back right there). WARP never showed it: its triangles are small and never hit the cap.
**Fix (`root_visible`):** the geometry shader computes the BASE triangle's device-depth plane over the screen (exact: z / w is affine in
screen space for a planar triangle; `GOut.d.xy`, `nointerpolation`). The expected depth at a pixel p is `z_root + grad . (p + 0.5 -
anchor)` (the anchor and z_root are interpolated with the same barycentrics, so they lie on that plane). The root is visible where one of
the 3x3 pixels holds that value within `2e-5` (D24 rounding) + the draw's own depth bias (`q4`, from its rasterizer state: DepthBias in
depth units, SlopeScaledDepthBias x the plane's slope, the clamp) + 5 % of a pixel of slope; a triangle under 0.001 px^2 on the screen uses
its depth range instead of a plane. No slope cap any more.

**(2) The banner fringe and the "late" geometry: the fragment test used the linear depth only.** The engine writes its linear depth in a
pass right after the opaque pass (`ao: depth source found ... first such pass after the opaque pass`); geometry drawn later still writes
the DEVICE depth (decorations, banners, alpha-tested cloth). v7 tested a strand fragment against the linear depth alone, so the strands
rooted on the fur just below the banner's edge painted up over the banner: the comb along its edge. **Fix (`hidden_by_device_depth`):** the
fragment is also dropped where the engine's final device depth at its pixel is nearer than the root (or the fragment itself) by more than
5 mm + 0.3 %. Device depths become view depths with the standard projection's 1 / w form anchored at the root: `w = w_root (1 - z_root) /
(1 - d)` (w_root (1 - z_root) is the near plane's distance; the geometry shader checks per triangle that `C.w - C.z` agrees within 5 % at
its three vertices, else this test is off for it and only the linear one runs; the sky / far plane never hides).

**(3) The comb / rectangle of an edge-on strip.** A flat strip, card or hem seen at a grazing angle grows parallel strands all in one
plane: in screen space a hard-edged band whose outer edge is the strip's straight polygon edge (a tail, a banner's hem). The
GW2 charr tail is such a strip (a charr's atlas: a long feather strip u 0.67..0.98 x v 0.70..0.75 whose alpha cuts the feathers out
of it). **Fix (geometry shader):** no shells where |cos(face normal, view ray)| < `FACE_LO` 0.08, full from `FACE_HI` 0.20 (smooth, into the
coverage fade). A curved coat's outline keeps the strands of its less grazing triangles. The two causes above also drew such bands (the
anchor latched on another surface).

**(4) The horn's edge and the paw claws: the texel tests look 3 texels around the root.** The flat test (horn) needs the whole 5x5 window
of a root flat, so the 2..3 texel border of a horn whose window still reaches into the fur stayed fur; the claws' matte rim (dark brown,
alpha 0.36..0.41, value 0.20..0.25: below the keratin box's value 0.25) and the claw base stayed fur. The paw claws are a strip at the
left edge of the foot island (light-coat atlas u 0.684..0.704 x v 0.945..0.997, and 0.688..0.716 x 0.874..0.906), the same keratin as the hand
claws, more of it matte. **Fix (`ft_feat_dil`, mirror `texel_features_dilated`):** the strand shader also reads the four 5x5 windows 3
texels away diagonally (one 11 x 11 block, 109 loads instead of 25): the flat test takes the LOWEST flatness and the glossy test the HIGHEST
gloss of the five windows (both tests are monotonic: the minimum of their weights). The colour tests stay at the centre (dilating them
cost up to 15 points of coat on marked / spotted coats); the coat statistics (`ps_feat`) stay the centre's. Measured on the debug atlases (the
~1024 level, an offline harness with the same rule as the shader; coat = body fur and cheek regions, claws = the texels of the
claw regions whose own colour is keratin, horn edge = head texels the v7 rule keeps within 3 texels of a dead-flat texel; v7 -> v8, %
kept, horn edge % removed):

| atlas | body fur | cheek | claws | fang | horn rect (white: horn) | horn edge removed |
|---|---|---|---|---|---|---|
| 323868e4 light coat | 99 -> 99 | 99 -> 95 | 3 -> 2 | 11 -> 10 | 24 -> 15 | 0 -> 92 |
| dfe7c25c grey | 100 -> 99 | 100 -> 94 | 2 -> 0 | 10 -> 8 | 86 -> 86 | 0 -> 89 |
| c91ca4a3 dark blue-grey | 100 -> 99 | 100 -> 93 | 3 -> 1 | 9 -> 8 | 100 -> 99 | 0 -> 94 |
| 5566458d grey-brown | 92 -> 91 | 100 -> 94 | 5 -> 4 | 12 -> 10 | 94 -> 93 | 0 -> 95 |
| e884ef71 brown | 72 -> 71 | 98 -> 97 | 4 -> 1 | 10 -> 5 | 45 -> 44 | 0 -> 85 |
| 5100819f brown | 98 -> 97 | 97 -> 97 | 8 -> 1 | 38 -> 20 | 45 -> 45 | 0 -> 93 |
| 21a45427 dark | 98 -> 96 | 99 -> 97 | 7 -> 1 | 9 -> 0 | 100 -> 98 | 0 -> 79 |
| 3c71e3f2 orange tiger | 98 -> 96 | 100 -> 100 | 8 -> 1 | 9 -> 0 | 100 -> 98 | 0 -> 88 |
| 05a0d288 orange tiger | 100 -> 100 | 100 -> 95 | 39 -> 16 | 62 -> 34 | 100 -> 100 | 0 -> 94 |
| 71695927 orange leopard | 100 -> 100 | 100 -> 96 | 39 -> 16 | 44 -> 26 | 95 -> 93 | 0 -> 92 |
| c984dc99 orange | 99 -> 99 | 100 -> 94 | 37 -> 16 | 42 -> 24 | 100 -> 99 | 0 -> 92 |
| a3e217eb spotted white | 99 -> 97 | 78 -> 76 | 2 -> 1 | 26 -> 12 | 25 -> 24 | 0 -> 96 |
| 700c4377 orange (NPC) | 100 -> 99 | 100 -> 93 | 36 -> 15 | 89 -> 81 | 100 -> 97 | 0 -> 89 |

(The e884ef71 body rectangle and the a3e217eb cheek rectangle hold non-fur / dark spots already below 90 % in v7; their loss is 1..2
points. On the non-white atlases the "horn edge" texels are fur next to other flat islands: that column is the cost there, the coat
columns are what the fur keeps.) The paw claws: light-coat atlas, keratin texels of the paw strip kept 4 of 748 -> 0, its rim (texels within 2 of
a keratin texel) 37 of 49 -> 0; the spotted white NPC (`a3e217eb`) paw strip 88 of 903 -> 0, rim 95 of 128 -> 1; its hand claws 67 of 1282 ->
6, its foot-claw island 82 of 522 -> 58 (white: 76 of 536 -> 18; the rest are matte claw-base texels of the coat's own colour and gloss). Rejected: lowering the keratin value bound to 0.15 (paw rim) cost the grey coat 29 points in the horn rectangle and spotted coats 8
points; a 3x3 corner window (cheaper) cost 10..30 points of cheek.

**Claw geometry (vertex attributes).** The character vertex shader `ba0709f4` outputs uv, a packed TEXCOORD1 / TEXCOORD5 and three frame
vectors (its disassembly): no vertex colour or material id reaches the pixel stage that could flag claw triangles; the claws are found by
their texels only.

**WARP (`shellfur_tests.rs`, v8).**
* `v8_an_arm_in_front_in_the_same_draw_never_shows_the_fur_behind_it`: an arm sphere 10..30 cm in front of the body IN THE SAME DRAW, the
  engine's depth bias none / 65 (measured) / 100 + slope-scaled 1.0: 9732..9734 arm px, every key on the arm rooted on the arm (0 off
  it), the arm's pixels without a key bit-identical, the body keeps 75106..75658 keys.
* `v8_another_character_in_front_with_a_depth_bias_never_shows_hidden_strands`: the v7 two-character scene with bias 65 and 100 + slope 1:
  0 keys / 0 changed px on the front character, 0 rooted behind it; both with shells 0 of 14161..14163 keys rooted elsewhere.
* `v8_a_banner_drawn_after_the_linear_depth_hides_the_strands_behind_it`: the cloth written to the colour and the device depth only, 2 and
  12 cm in front: 0 keys and 0 changed pixels on its ~47000 px (the v8 shader without the device-depth test: 503 keys, 245 changed px), the fur keeps ~57000 keys.
* `v8_a_card_seen_edge_on_grows_no_comb_of_strands`: a 1 x 1 m card at |cos| 0.03 / 0.06: 0 keys (without the edge-on fade: 4145 / 7819 keys, 1520 / 1817 of them off the card: the comb); at 0.5: 48262 keys.
* `v8_the_dilated_texel_tests_remove_the_border_of_a_horn_and_a_claw`: the CPU mirror next to a flat horn / a glossy claw: v7 weight 1.000,
  v8 0.000; 6 texels away fur again; the constants q3.w / q4.
* Changed (pinned to the replaced behaviour): `PsCb` is 336 bytes (+ q4), `ps_ext_values` w = `FRAG_TOL_M`;
  `v7_strands_over_the_background_are_a_coverage_blend` asks for 30 instead of 100 covering strand pixels over the sky (the edge-on fade
  thins the outline's most grazing strands: covering strand px over the sky 298 -> 64, faint ones 3847 -> 4552; a covering one still changes the sky by 0.205, a faint one by 0.034).

**`status_shells=` (v8)** begins its discriminator part with `per-strand fur discriminator (v8, ...; roots: the engine's depth = the base
triangle's depth plane + the draw's bias; fragments: hidden by the linear AND the device depth; no shells on triangles seen edge-on,
|cos| < 0.08..0.20)` and `flat (horn) and glossier than the coat (claws, fang) also 3 texels around the root`.

**Cost (v8).** Per covering strand fragment 109 texel loads (was 25) for the discriminator, one more device-depth load; per triangle in
the geometry shader a few more operations. Nothing new on the CPU.

**What to check in the game (v8).** (1) The arm / glove in front of the tail or the body (the same view, the spotted NPC): no fur
streaks over the glove or the arm, no floor-coloured or blue streaks under the tail. (2) A banner hanging over a charr: no comb along its lower edge.
(3) A tail seen from the side / edge-on: no yellow band, no straight comb; the tail's outline seen from behind keeps soft strands.
(4) Horns: no fringe at the edge / base on charr of several coat colours. (5) Paw claws: no strands on them; the paw's fur
next to them may be a little shorter at the claw base. (6) The coat elsewhere and the silhouettes of head / arms still have strands
(slightly fewer at the most grazing outline). (7) `status_shells=` shows the v8 line; `status_perf=` with `fur_shell=1` vs `0`.

## v9: which PARTS are fur, per UV island of the mesh

**Seen in the game on v8**: claw tips and claw bases with fine white strands on one paw, a clean paw on another
(the rule works on some atlases and not on others), a comb of strands along the upper edge of a charr's horn. **Why the texel
rules leak:** every v3..v8 rule is a per-texel statistic (gloss, flatness, keratin colour, red, relative to the coat). The atlas shows
what each part's uv covers (`furtex-<crc>-rgb.png` of the light-coat atlas): the claw geometry samples brown blobs surrounded by WHITE coat texels inside
the same uv island, the claw bases are matte and coat-coloured, the horn crescent is packed against fur and its 2..3 texel border reads
fur, and on orange coats the claws' matte base has the coat's colour and gloss (v8 keeps 15..16 % of their texels). A horn, a claw, a
fang is a PART of the mesh, its own UV island: a verdict per island is all-or-nothing for the part.

**The algorithm (`src/furisland.rs`, `gs_tri` / `tri_bits` / `ft_near` in `shaders/shellfur.hlsl`).**
1. *Capture, once per mesh (render thread, no stall).* The mesh key is (index buffer, its offset and format, vertex buffers 0 / 1 and
   their offsets / stride, index count, start index, base vertex, the atlas view, the vertex shader). A mesh not yet known is drawn once
   more (one mesh per frame) through the engine's own vertex shader (recorded state, copied bone matrices) and a generated stream-output
   geometry shader `gs_tri` (one point per triangle, `SV_PrimitiveID`): its three clip positions (x, y, w) and its three ATLAS uv,
   80 bytes per triangle, `CreateGeometryShaderWithStreamOutput`, no rasterized stream. The stream-output buffer is copied into a
   staging buffer and mapped with DO_NOT_WAIT in a later frame. **How the mesh is read:** never through the engine's vertex / index
   buffers: they may be IMMUTABLE or DEFAULT without CPU access, a D3D11 input layout cannot be queried for the POSITION / uv formats,
   the positions are skinned and the atlas uv is the vertex shader's `mesh uv x cb0[5..6]`. The vertex shader's OUTPUT is exactly what
   the shells see. The draw atlas's level with at most 1024 texels across (the level of the strand shader's texel tests) is copied back
   the same way, once per atlas. Only non-instanced triangle lists of at most 65536 triangles (a character draw: up to ~20k).
2. *Analysis (background thread).* Positions rebuilt like the shell geometry shader (x / P00, y / P11, w). **Islands:** union-find over
   the vertices welded on (position, uv) (quantised: 2e-6 of the mesh diagonal, 1/16384 uv): a uv seam splits, a hard normal edge does
   not, mirrored halves sharing one uv region stay apart. **Votes:** every triangle's uv footprint rasterised on the atlas level, each
   texel judged by the CPU mirror of the strand shader's v8 rule (`texel_features_dilated` + `texel_weight`) against the draw's coat
   statistics computed like the GPU does (`coat_of`: the 32 x 32 uv cells); non-fur = weight < 0.5. **Shape** per island (area-weighted
   surface points): area share of the mesh, PCA elongation sqrt(l1 / l2), taper (mean distance from the main axis in the thinner end
   quarter / the thickest quarter). hard-shape = smooth(1.4..2.4 elongation) x (1 - smooth(0.35..0.65 taper)) x (1 - smooth(0.08..0.20
   area share)). **Verdict:** excluded when the island's non-fur share > 0.40 - 0.10 x hard-shape + 0.25 x smooth(0.15..0.35 area
   share) (an ordinary island 0.40, a horn / claw shaped part 0.30, the body skin up to 0.65). Shape alone never excludes (an ear or a
   tail is tapered too). **Sub-regions:** inside a fur island, a connected group of triangles each >= 70 % non-fur whose mean is
   >= 85 % (or >= 70 % with a hard shape), >= 6 texels, at most half of its island (a claw strip fused into a foot island) is excluded
   too. **Near:** fur triangles that share a vertex POSITION with an excluded triangle (the seam / base of a horn or claw, also across a
   uv seam).
3. *Output.* 2 bits per triangle (16 per word: bit 0 excluded, bit 1 near) in an IMMUTABLE R32_UINT buffer, created once per mesh, bound
   to the shell geometry shader as **`t2`** (`Buffer<uint> tri_bits`; the shell GS used t0 / t1 only). `gs_shell` reads it by
   `SV_PrimitiveID`: an excluded triangle emits NO shell (every layer, the base layer included, wherever it projects: the comb at a
   horn's silhouette is gone by construction, a hidden root does not matter); a near triangle passes `GOut.c.w = 1` and its covering
   strand fragments also read the four 5x5 windows `FT_NEAR` = 6 texels away diagonally (`ft_near`: the flat and the glossy test take
   their min / max; mirror `shellfur::texel_features_near`). **The v8 texel rule still runs on every strand** (a strand needs a fur
   island AND a fur texel), unchanged.
4. *Cache / stability.* The bits never change for a mesh once ready (no frame-to-frame flip). Every 900 replays a Ready mesh is captured
   again (one per frame) and its uv hash compared: a pooled buffer rewritten in place is dropped and analysed again (meanwhile the v8
   rule alone). At most 256 meshes per device (least recently used go), 2 analyses at once, a capture / atlas read-back not readable
   within 240 replays fails.
5. *Fail open.* Pending (capture, read-back, analysis), failed, not analysable, the capture shader not created, or a device without
   stream output: `t2` stays unbound, `tri_bits.GetDimensions` is 0 and nothing changes: the v8 output bit for bit (tested).

**Why not a uv-space mask (the first plan).** A per-strand uv mask (an R8 texture tested at the root uv) would also need the island
verdicts, but in UV space: mirrored halves and claws sharing one brown blob overlap there (the claw geometry of every finger samples
the same blob), and a dilated mask would bleed into the coat packed next to an island. The per-triangle bit is exact, free (one load
per triangle in the geometry shader), and removes every layer of the part's shells, also where its root is hidden. The fur mask (old
fur look, rim, `fur_debug`) is NOT fed: the v7 discriminator was never wired into it (it keeps its own `shell_fur_mask` texture tests).

**Evidence.**
* WARP (`v9_islands_drop_every_layer_of_the_horn_and_claws_and_keep_the_coat_and_the_ear`): one draw = the coat sphere + a horn cone
  (its island: flat grey with fur-like coat bands of 24 rows and a coat border) + three claw cones (glossy tan tip, coat-coloured matte
  base) + an ear cone (tapered like a horn, coat texels). The real pipeline captures it, analyses it in the background and binds the bits
  after a few frames (`ready: 10496 triangles, 6 islands, 4 excluded`). Keys anchored on the part beyond its junction band with the
  sphere, all layers: horn v8 2421 -> v9 0, claws 340 / 358 / 401 -> 0 / 0 / 0; the ear 4898 -> 4933 (kept); the coat 65555 of 65555
  px keep their keys (v8: the same). At the junction ring (the base fifth of a part, where the embedded cone and the sphere have the same
  depth) the COAT's strands remain (504 / 350 / 338 / 303 keys: one key per pixel, so where the part's own base strands are gone the
  coat's win those pixels). Renders `v9-islands-off` / `v9-islands-on` (`GW2G_SHELL_PNG`): the v8 horn has the comb along its edge.
* WARP (`v9_a_pending_or_failed_island_analysis_is_the_v8_output_bit_for_bit`): pending (captures done, analysis held) and failed: keys
  and picture identical to the analysis off, three frames.
* Host (`furisland_tests.rs`): islands (a seam column = one island, mirrored copies = two, per-triangle vertex copies = one, a uv seam =
  two, a NaN triangle alone); shape (a 5:1 horn / claw cone: elongation 2.55, taper 0.21 / 0.28, hard-shape 1.0; a cylinder: taper 1.0,
  hard-shape 0; the skin: elongation 1.02, hard-shape 0); voting on a synthetic atlas (horn 46 % non-fur, the texel rule alone keeps 54
  % of its texels -> excluded; claw 62 % -> excluded; skin and a horn-shaped ear 0 % -> fur); a claw strip fused into a paw island ->
  an excluded sub-region, its neighbours near, the rest fur; the read-back checks (short / reordered capture rejected), the bit packing,
  the uv hash (ignores animated positions); the debug dump.
* **The 13 real atlases of the v8 table** (`the_real_atlases_are_classified_per_island`; the meshes are not on disk, so each part of the
  atlas layout is given a synthetic island with its real uv rectangle, cones for the hard parts and patches for the skin, analysed by
  the same code). Non-fur share by the v8 rule -> verdict:

| atlas | body fur A / B | cheek | hand claw | foot claw | paw claw strip | fang | horn rect (white: the crescent) |
|---|---|---|---|---|---|---|---|
| 323868e4 light coat | 1 / 0 % fur | 5 % fur | 91 % EXCL | 89 % EXCL | 98 % EXCL | 91 % EXCL | 85 % EXCL |
| dfe7c25c grey | 6 / 0 fur | 27 fur | 91 EXCL | 88 EXCL | 98 EXCL | 93 EXCL | 20 fur |
| c91ca4a3 blue-grey | 6 / 0 fur | 27 fur | 91 EXCL | 89 EXCL | 98 EXCL | 93 EXCL | 5 fur |
| 5566458d grey-brown | 18 / 28 fur | 40 fur | 92 EXCL | 88 EXCL | 98 EXCL | 91 EXCL | 19 fur |
| e884ef71 brown | 28 / 7 fur | 22 fur | 100 EXCL | 91 EXCL | 98 EXCL | 97 EXCL | 64 EXCL |
| 5100819f brown | 3 / 0 fur | 3 fur | 100 EXCL | 91 EXCL | 98 EXCL | 90 EXCL | 55 EXCL |
| 21a45427 dark | 6 / 0 fur | 5 fur | 100 EXCL | 90 EXCL | 99 EXCL | 100 EXCL | 8 fur |
| 3c71e3f2 orange tiger | 5 / 0 fur | 0 fur | 100 EXCL | 91 EXCL | 99 EXCL | 100 EXCL | 8 fur |
| 05a0d288 orange tiger | 1 / 0 fur | 25 fur | 70 EXCL | 88 EXCL | 98 EXCL | 60 EXCL | 4 fur |
| 71695927 orange leopard | 1 / 0 fur | 23 fur | 68 EXCL | 88 EXCL | 98 EXCL | 76 EXCL | 9 fur |
| c984dc99 orange | 7 / 0 fur | 28 fur | 75 EXCL | 88 EXCL | 98 EXCL | 81 EXCL | 4 fur |
| a3e217eb spotted white | 3 / 1 fur | 29 fur | 100 EXCL | 93 EXCL | 98 EXCL | 91 EXCL | 78 EXCL |
| 700c4377 orange NPC | 1 / 0 fur | 25 fur | 78 EXCL | 90 EXCL | 98 EXCL | 14 fur | 3 fur |

  The claws are 68..100 % non-fur on EVERY coat, also the orange ones where v8 kept 15..16 % of their texels: as islands they go whole.
  The hard-part threshold is 0.30 (a first 0.15 excluded the "horn rect" of the grey atlases at 19..20 %, which is head fur there; 0.30
  keeps a 2x margin under the lowest claw). The fang of `700c4377` is coat-coloured in its atlas (v8 kept 81 % of it): it stays fur
  (report). The cheek numbers are lower than in the v8 table because the coat statistics here come from the synthetic mesh's footprint.
* **The combination (keep v8, combine).** The shells use island AND texel: v8 alone is the leaks above; the island verdict
  alone was not adopted (by design, not measured: it would give up v8's removal of non-fur texels inside fur islands, the lips, the
  fang base, markings), so v8 stays on every strand, unchanged. The island verdict also feeds the texel rule (NEAR), measured on the 13
  atlases: of the head texels the v8 rule keeps as fur within 7 texels of a dead-flat
  (horn) texel, they remove 48..68 %; applied to the whole cheek they would cost 0..12 points of coat (`5566458d` 61 -> 49 %), so they
  run only on triangles touching an excluded part.

**`status_shells=` (v9)** has a part `v9 UV-island classification (...): [count N: ready: T triangles, I islands, E excluded, S
sub-regions (X triangles without shells), N near -> islands AND texel rule]` per replayed draw, or `pending: capture / read-back /
analysis -> texel rule (v8)`, `failed (why) -> texel rule (v8)`, `not analysable (...)`.

**Debug files.** At a debug capture (and at once with `shell_debug=2`) the newest 6 analyses are written to `GW2Graphics\debug\`:
`furmesh-<uvcrc>.txt` (index count, triangles, islands, excluded, the coat statistics, the rule, then per island: triangles, uv box,
3D area share, elongation, taper, tip share, hard-shape, texels, non-fur %, threshold %, verdict) and `furisland-<uvcrc>.png` (the atlas
dimmed, every triangle painted: green fur, red excluded island, orange excluded sub-region, yellow near). `<uvcrc>` = the crc of the
mesh's atlas uv (stable across sessions).

**Parameters (code, no config line).** T_BASE 0.40, T_HARD 0.30, T_LARGE 0.25 (smooth over area share 0.15..0.35), vote cut 0.5,
hard-shape ramps above, sub-region 0.70 / 0.85 / 0.70 shaped / 6 texels / half the island, FT_NEAR 6, re-verify 900 replays, timeout
240, 256 meshes, 2 analyses, 65536 triangles.

**Cost.** Once per mesh: one extra draw of the mesh through its vertex shader into a stream-output buffer (80 bytes per triangle, a
character ~0.5..1.6 MB, released after the read-back), one atlas level copy per atlas (1..4 MB staging, released), a background analysis
(off the render thread; its time is not measured in the game), one 4-byte-per-16-triangles immutable buffer. Per frame: one map attempt per pending capture,
a hash-map lookup per replayed draw, one buffer load per triangle in the shell geometry shader, 100 more texel loads per covering strand
fragment on near triangles only. Every 900 replays a re-capture per mesh (one per frame).

**What to check in the game (v9).** (1) The claws of every charr (NPCs and orange coats too): no strand at the tips nor at the base.
(2) The horn: no comb along its edge, no fuzz on the horn; the head fur right at the horn base may still lean over it (it grows
from the head). (3) The coat, ears, tail, mane: unchanged from v8 (`status_shells=` per draw: excluded islands a handful, never the
big ones). (4) A debug capture right after: `furmesh-*.txt` and `furisland-*.png` in `GW2Graphics\debug\`: in the PNG the horn crescent,
the claw blobs and the fang red, the coat green; in the txt the excluded islands' non-fur share and shape. If a claw or a horn stays
fuzzy: its island in the txt (verdict fur, its non-fur share) says why. If fur is missing on a part: a red island that should be green.

## v10: horns by their 3D shape, mirror copies, shorter strands

**Seen in the game on v9:** the claws are clean (keep), the HORNS still have fur; the strands are too long.

**What the real island tables showed** (`GW2Graphics\debug\furmesh-*.txt` + `furisland-*.png`, written by v9 in the game, 20 meshes).
The charr head mesh (index count 3102..3174, 45 islands; the same layout in `17c6559b`, `9cfde15c`, `87c3941a`, `8cc07fea`) has
its horns in their OWN atlas tile (u 0.378..0.498, v 0.003..0.246; the head fur is at v 0.50..0.75): 24 islands, 12 mirrored pairs
(identical triangle count and uv box: left / right horn), 3..45 triangles each, 27..28 % of the head's area together. Their texels are
0..2 % non-fur by the v8 rule (a few small ones 20..32 %): the horn texture is grey-brown with ring ridges, which the texel tests read
as strands of the coat's colour and gloss, so NO texel threshold excludes them (hypothesis 4) and the per-island shape does not help
either: a horn is cut into lengthwise strips and base pieces, each one only mildly elongated (1.1..4.4) and tapered (0.3..0.9),
like many head fur islands (hypothesis 1 holds only as "shape alone never excluded"). Mirrored pairs CAN split (hypothesis 2):
`87c3941a` island 39 excluded (30.5 % > 30.5 %), its mirror 42 kept (30.5 % < 30.6 %). Horn islands are not fused into head fur
islands (hypothesis 3 no): they touch the head only through one-triangle islands of the head tile (NEAR flags). Other horn styles:
`d3b786e3` (dark horn, 12 islands, 93 % non-fur) is excluded whole by v9, `e98a1eac` half (7 of 18 islands of its tile; the
small base pieces 0..14 % non-fur kept).

**The rules (`src/furisland.rs`, v9 kept for everything else).**
1. *Texels (v9).* Unchanged.
2. *Shape (new).* **Tiles:** islands whose uv boxes (moved into 0..1 by whole uv units: a mirror copy one atlas width over) come
   within 0.03 of each other form an atlas tile. **Parts:** islands of ONE tile that share a vertex position form a 3D part (a
   horn's strips join along their seams; the horn does not join the head across tiles; the two horns stay apart). A part is judged
   by its shape only when its tile does NOT hold the mesh's largest island (the coat's tile: a tail, a limb, a finger there is
   never judged), the tile is <= 45 % and the part <= 30 % of the mesh, and it has >= 16 triangles. **Shape of the part:** the
   largest boundary loop is its attachment (a horn's base ring; a closed part starts at an end of its diameter); geodesic distance
   (Dijkstra on the position-welded edges) cuts it into 8 slices. closure = |sum area x normal| / area (horn ~0.2, bowl 0.5, sheet 1);
   solid = |volume about its centroid| / area^1.5 (cone ~0.05, card 0); round = mean over slices of sqrt(nu2 / nu1) of the slice's
   normal moments (ring 1, flat card 0); taper = radius of the last quarter / the largest slice radius, radius measured from the
   slice's own axis (cone 0.2, cylinder 1); slender = geodesic length / largest radius; branches = pieces per slice (horn 1, a paw's
   fingers 3..4). **cone** = (1 - smooth 0.35..0.55 closure) x smooth 0.015..0.035 solid x smooth 0.35..0.60 round x (1 - smooth
   0.30..0.55 taper) x smooth 1.8..2.8 slender x (1 - smooth 1.3..1.8 branches). Excluded (every island of the part) when cone >=
   0.5 - 0.25 x smooth(0.10..0.30 of its texels non-fur): weak texel evidence lowers the cut, none is required (the real horns have
   none).
3. *Copies (new).* An island with the triangle count and uv box (modulo whole uv units, 5e-4) of an island excluded by rule 1 or 2
   is excluded too (`87c3941a` 42).
Then sub-regions and NEAR as in v9 (an excluded part's neighbours across the tile seam are NEAR).

**Evidence.**
* Host (`furisland_tests.rs`): a curved horn (92 deg arc, 10 rings x 16 segments to a closed point) cut into 4 strips x 2 pieces
  (8 islands) in its own tile, welded to a head collar of the head tile, mirrored (16 islands), on COAT texels (0 % non-fur, v9 keeps
  it): closure 0.19, solid 0.056, round 0.94, taper 0.20, slender 4.6, branches 1 -> cone 1.0, all 16 islands excluded by shape
  (one part per horn), the head fur, the collar near; the same horn with its uv in the coat's tile: not judged, 0 excluded. Kept as fur
  (cone 0): a one-sided mane strip (closure 0.99), a two-sided mane card and a tapered two-sided tail feather card (solid 0, round
  0.02), a tapered one-sided strip, a thick ear plate (round 0.26) and the ear of the v9 mesh (now a plate: v9's cone ear would be a
  horn), a cylinder in its own tile (taper 1.0), a 4-finger paw part (branches 3.3, taper 0.93), a tapered tail tube in the coat's tile
  (not judged). The copy rule (mirror, a copy one uv unit over; another triangle count or box: not), the tiles.
* WARP (`v9_islands_drop_every_layer_...`): the ear is a flat plate now (it keeps its fur), horn and claws as before.
* **The real tables replayed** (`v10_the_real_island_tables_copies_and_shape_candidates`; the triangles are not on disk, so the copy
  rule is replayed exactly and the shape rule as its TILE GUARD: the islands it may judge, an upper bound; the cone test decides in
  the game). Excluded islands / triangles / area share:

| table | mesh | v9 | + copies | + shape at most (tiles it may judge) |
|---|---|---|---|---|
| 17c6559b | head 3162 | 1 / 60 / 2.1 % | same | 25 / 342 / 29.8 % (horn tile: 24 islands, max island 4.1 %) |
| 9cfde15c | head 3162 | 7 / 122 / 3.8 % | same | 31 / 404 / 31.8 % (horn tile, 24) |
| 87c3941a | head 3102 | 6 / 107 / 4.3 % | 7 / 114 / 5.6 % (42 = mirror of 39) | 29 / 382 / 31.3 % (horn tile, 22 more) |
| 8cc07fea | head 3174 | 7 / 122 / 4.7 % | same | 31 / 404 / 31.5 % (horn tile, 24) |
| e98a1eac | head 4050 | 21 / 518 / 24.0 % | same | 41 / 696 / 39.1 % (horn tile + its copy one uv unit over: 20 base pieces) |
| d3b786e3 | head 2328 | 28 / 394 / 31.9 % | same | same (its horn tile is excluded by v9) |
| 38c9862b | arm / paw 2811 | 20 / 338 / 7.7 % | same | 28 / 530 / 10.4 % (paw tile: 8 finger islands around the claws; a paw is several prongs: branches) |
| fcb47033 | body 3333 | 20 / 338 / 6.9 % | same | 22 / 360 / 9.6 % (a 2-island 22-triangle tile, untapered islands) |
| 5fb74726 | other 1212 | 4 / 52 / 24.3 % | same | 8 / 180 / 36.5 % (a small striped box piece, not fur) |
| 01334ad4 | body 6402 | 26 / 572 / 15.7 % | same | 28 / 588 / 16.5 % (two 8-triangle islands: parts < 16 triangles, never judged) |
| ada1a7b6, be504206, 48298348, 91d08a24 | | | same | same (their small tiles are excluded by v9 already) |
| 5c273ac3, f61d4079, f9564300, 35a30a7c, d737cc78, e86c0685 | | | same | same (no tile outside the coat's) |

  No island above 4.1 % of its mesh sits in a tile the shape rule may judge (asserted <= 10 %): no head, cheek, mane or body
  island can be excluded by shape. The claws stay excluded (rule 1 unchanged; as parts they are cones too).

**Strand length (v10).** `shell_length` default 2.5 cm (was 4) and `shell_lod_distance`
default 12.5 m (was 20): the LOD shortens and fades the strands from half of it and ends them at it; scaled with the length, it
starts and ends at the same strand size on the screen as before. The 64 px guard is unchanged.

**Debug output (v10).** `furmesh-*.txt`: the header counts the excluded islands by rule (texels / part shape / copies) and prints the
three rules; per island two more columns (part, its cone score) and the REASON in the verdict (`EXCLUDED (no shells): texels 62.2 %
non-fur > 40.0 %`, `... part 3 is cone-shaped (0.93 >= 0.50) in its own tile`, `... copy of excluded island 39`, `fur [part 3 cone
0.12 < 0.50]`); then a parts table (islands, triangles, tile and its share, area share, closure, solid, round, taper, slender,
branches, open, non-fur, cone, cut, verdict or why not judged). `status_shells=`: `ready: T triangles, I islands, E excluded (a by
texels, b by part shape, c copies), ...`.

**What to check in the game (v10).** (1) The horns of the four NPC charr and a playable charr: no strands at all on the horn (also
its base pieces); a debug capture: in `furmesh-<head>.txt` the horn tile's islands `EXCLUDED ... part N is cone-shaped`, in the parts
table the two horn parts with cone >= 0.5 (if a horn stays fuzzy: its part's row says which measure failed: closure, solid, round,
taper, slender, branches, or "not judged" and why). (2) Claws still clean. (3) Ears, mane, cheeks, tail, paws, fingers keep their
fur (in the parts table: the paw tile's part `judged by shape: fur`, never `excluded by shape` for a fur part). (4) The strands
shorter (2.5 cm) and still visible at mid range; fur ends at 12.5 m (raise `shell_lod_distance` if it ends too early).

## v11: the old fur look and the rim only on proven fur skin

**Seen in the game on v10** (a charr's head in profile in a bright hazy scene): the shader looked transparent, the fur on the horn was gone, and the hair was checkered. The mane's
hair cards (the locks in front of the ear, the strands over the forehead) show a hard light / dark plaid, the horn diagonal zebra
stripes, and the head looks milky.

**Cause of the plaid and the stripes (evidence: code + capture).** The OLD fur look (`fur_look`, `fur_strength=1` in the config of the capture:
`fur_amp 0.32`, `fur_detail 1`, `fur_rim 0.25`) runs on every pixel of the fur MASK, and the mask is every draw with a fur pixel shader
(`status_fur=199 draws masked`: skin, hair cards, horns, armour, other races). The composite has always replaced the old look by the shells
only where a strand key lies (`shell_key == 0` -> `fur_look`). v7..v10 took the shells away from exactly the pixels that are not fur: the
hair cards (`status_shells=` of the capture: `[count 17694 atlas ee5874e8: 51 cells, gloss band 2 %, ... coat gloss 0.54 -> not fur
(armour / skin / hair), no shells]`) and the horn (v10: an excluded island, `EXCLUDED ... part N is cone-shaped`). On those pixels the old
look came back in full: its strands are value noise on a lattice ~3 px across and 15 px along the MESH uv, +-32 % luminance; on a hair card's
smooth uv it is a coarse rectangular plaid, on the horn's long strip uv diagonal stripes. The mod / vanilla crops of the head
(`latest-mod.png` vs `latest-vanilla.png`, x 960..1300, y 150..420) show both patterns only in the mod. Two more parts of the old look made
the cards look see-through: (1) the mask draws alpha-tested cards SOLID and the occlusion check keeps the cut-out pixels (the final depth is
FARTHER than the stamp, by design: fail open), so the strands, the detail and the warm term were painted onto whatever is seen through the
holes between the locks (the head behind, the sky), with the card's uv; (2) the look's rim feather lerps the mask's edge pixels towards the
colour OUTSIDE the mask, i.e. towards the background, at every card edge. The rim light (`rim_ps` = the same shaders, `rim_min_vcb 4096`:
161 draws) had the same reach (the hair, the horn, armour). The v9 report already said it: "the fur mask (old fur look, rim, `fur_debug`) is
NOT fed" by the classification. Not the cause (checked): the 2x2 / bilinear root reads, the v8 dilation windows, the MAX blend, MSAA (none),
a dither (the light-adjust's 1/255 dither is +-0.002).

**The fix: the skin pass (`ps_skin` in `shaders/shellfur.hlsl`, `src/shellfur.rs` replay, `t25` of `light_adjust_hdr.hlsl`).** After the
shells, every recorded draw is drawn once more, base layer only (`gs_shell` with `g5.x = 1`: no LOD end, no edge-on fade, the island bits
applied, so an excluded triangle emits nothing), into an R8 target with a MAX blend and no depth buffer. A fragment writes the draw weight
x the texel weight (`strand_fur` at its own uv, the strands' verdict) x `shell_uv_exclude`, only where
* the base triangle IS the visible surface at this very pixel (`surface_here`: the engine's device depth equals the triangle's depth plane +
  its bias; root_visible's test without its 3x3 search, so nothing bleeds a pixel into a cut-out or past a silhouette), and
* the fur mask at the pixel holds THIS surface's uv (within `SKIN_UV_TOL` 0.004, wrapping; the same vertex output rasterised twice): a hair
  card masked solid over the head no longer lends the head its uv (its strand pattern at the card's scale).
`fur_look` and `rim_look` multiply their gate by it. So: not-fur draws (hair cards, armour, human skin), excluded islands (horns, claws,
fang), non-fur texels, cut-out holes and hidden roots get neither the old look nor the rim; the coat keeps both as in v10 (bit for bit
where the skin is 1). t25 is the skin target when the replay drew it, a 1x1 ZERO texture when the shells are on but nothing was recorded this
frame (no proven fur: off, fail closed), and unbound only when the shells are off (`fur_shell=0`) or not ready: then the old look and the
rim run on the whole mask as before (there is no classification; documented, not a regression). Fur draws not recorded (refused as
`dynamic vertex / index buffer`, past the 24 records of a crowd, `vcb < shell_min_vcb`) get no old look / rim either. The diagnostic views
(`fur_view`, `fur_debug`, `rim_debug`) still paint the whole mask. Limit: the mask is not depth-tested within one draw (its rg is the LAST
fragment of the draw at the pixel), so where a draw overlaps itself (an arm in front of the body in the same mesh) the uv check can fail and
that pixel gets no old look / rim (fail closed; the shells are unaffected).

**`status_shells=` (v11)** has after the frame summary `old fur look + rim gate (v11): only on proven fur skin, the skin pass of N recorded
draws (...)` (or `no draw recorded this frame ...: off on the whole mask`, or `not bound this frame (shells not ready) ...`); a not-fur draw's
verdict ends `no shells, no old fur look / rim`. `status_furlook=` and `status_rim=` end with `gate: proven fur skin only (v11 skin pass, N
draws; hair cards, armour, horns, claws skipped)` / `gate: none (fur_shell off ...)`.

**Evidence (WARP).**
* `v11_a_hair_card_draw_gets_no_old_fur_look_no_shells_and_is_left_as_vanilla`: the sphere with a hair atlas (gloss 0.56): 67676 body px,
  skin 0 on all, no key; the v10 old look changed 67676 of them, v11 0; the whole output equals the picture without the old look.
* `v11_the_cut_out_holes_of_an_alpha_tested_draw_get_no_old_fur_look_and_the_coat_keeps_it`: the engine's pixel shader discards stripes
  (an alpha-tested card), the mask draws it solid: 30487 hole px inside the mask, skin 0 on all (the 3x3 root test leaked 2399 edge px: hence
  `surface_here`); of the 20377 without a key v10 changed 20377, v11 0; the visible coat: skin > 0.9 on 36739 of 37189 px.
* `v11_excluded_islands_get_no_old_fur_look_and_no_rim` (the v9 island mesh, analysis ready): horn 1826 px, claws 589 / 583 / 571: skin 0 on
  all, the v10 old look changed 1364 / 497 / 508 / 499 of their key-less px, v11 0; the ear 2235 px and the coat 65376 px: proven fur on all.
* `v11_the_skin_gate_switches_the_old_fur_look_and_the_rim_off_where_no_fur_skin_is_proven` (`hdr_shaders_tests.rs`, the shader alone): a
  gate of 1 = unbound = v10 bit for bit; a 1x1 zero = the picture without the look and the rim; half gated: the ungated half as v10 bit for
  bit, the gated half exactly the plain picture (v10 changed its 384 px by the look, 150 by the rim).
* `v11_the_skin_pass_constants_and_a_frame_without_records`: g5.x, slot 25, the shader's SKIN_UV_TOL = the Rust one, the status texts.
* Changed test harness (no assertion of an older test changed): `Out::old` (t24 / b13 unbound) now also unbinds t25, so it stays "the
  output without shells" (v10's).

**The washed-out, see-through look of the whole picture (numbers from a debug capture).**
* Ours and fixed: the see-through hair (the old look's feather towards the background at every card edge and its strands in the holes,
  above).
* Not the aerial perspective: the character stands at 1.5..5 m (`status_depth=` min 0.0015, p5 0.0018 of the 1000 m unit); the haze starts
  at 15 m RADIAL (`max(z x ray x 1000 - 15, 0)`, ray <= 1.6 at the picture's corner): 0 on the character. On the far left building
  (~200..300 m) the haze share is 0.35 x (1 - 2^(-(d - 15) / 400)) = 0.10..0.14 before the fog-colour release, ~0.04 after it (its colour is
  within 0.15 of the haze colour (0.59, 0.55, 0.47)): its luminance change matches the exposure alone (R x1.35, G x1.43, B x1.50; the
  exposure's curve gives x1.45 at that level).
* The global brightening is the auto exposure: `status_exposure= EV +0.55 (the scene asks +0.56), log-average 0.091, target 0.20, strength
  0.50`; mean luminance 0.265 vs vanilla 0.192 (+38 %), the dark floor x1.6..1.7, black level unchanged (2nd percentile luma 15 vs 16).
* The desaturation in the PNGs / an SDR screenshot is highlight CLIPPING of the HDR picture: 4.98 % of the mod's pixels have a channel above
  paper white (vanilla 2.14 %), the warm sky's red clips at 1.0 in the PNG while green and blue keep rising (sky: R 0.81 -> 0.90 clipped, G
  x1.5, B x2.0), mean saturation of those pixels 0.474 vs 0.732. On the HDR display the red is above paper white, not clipped (the HDR
  bleach of over-range colour is at most 13 % at 1.5x paper white). Settings, not a bug: `autoexp_target` / `autoexp_strength` lower it.
* No double composite: the light-adjust draw reads the scene colour once (t0); the fur / rim masks are separate targets.

**Cost (v11).** One more draw of each recorded mesh (base layer only: the geometry shader's 31 other instances return at once) into an R8
target of the screen size, its fragments 109..209 texel loads (the strand rule) + 1 depth load + 1 mask load; per light-adjust pixel one
more load (t25). Memory: one R8 screen target + a 1x1.

**What to check in the game (v11).** (1) The mane / hair locks of the same view: no checker / plaid, the locks as in vanilla
(`fur_strength=1` kept). (2) The horn: no stripes, like vanilla. (3) The coat still has the old look where no strand lies (beyond 12.5 m) and
the rim on the fur only (no rim glow on armour any more: by design, it follows the fur classification now). (4) `status_shells=` shows the
`old fur look + rim gate (v11)` part; `status_furlook=` / `status_rim=` end with `gate: proven fur skin only`. (5) `status_perf=` with
`fur_shell=1` vs `0`. (6) The overall brightness: if the scene still looks too bright / milky, try `autoexp_strength=0.3` or
`autoexp_target=0.16` (the exposure, not the fur).

## v12: the fingers keep their fur, the claws stay bare

**Seen in the game on v11** (a charr's hand with its claws): correct everywhere except the hands (front paws), which had no
fur. The hand and fingers show a smooth coat, no strands, no old fur look (v11 gates it the same way), the arm above has fur.

**Cause (evidence: the real tables of `GW2Graphics\debug\`).** A character's arm / hand draw is `count 2712` (atlas `323868e4`, 45 % of
the fur pixels): `ready: 904 triangles, 45 islands, 28 excluded (20 by texels, 8 by part shape, 0 copies)`. Its dumped twin of the
same layout is the island table `38c9862b` (and `1db2dfaf`, `24c4e630`, `662bbef0`, `ffb9d184`, the low LOD `9f8cd160`). In the paw tile
(u 0.69..0.86, v 0.93..1.0) each finger is a 3D part of TWO islands: the finger skin (24 triangles, elongation 1.2..1.3, 0..5 %
non-fur: in the light-coat atlas the white fur with the grey pads) welded to its claw tip (14 triangles, 99 % non-fur: the tan strip). As
one part they are a slender cone (slender 5.6..6.5, round 0.67, taper 0.26..0.46: cone 0.28..1.00) and the part's non-fur share
(51..58 %, all of it the claw tip's) lowers the cut to 0.25: v10's shape rule excluded the part whole, the finger skin with it
(`EXCLUDED (no shells): part 4 is cone-shaped (1.00 >= 0.25) in its own tile`): 8 finger islands, 192 triangles, 2.6 % of the mesh.
v10 had expected the paw to be one part of several prongs (branches); in the real mesh the hand back is in the ARM's tile, so each
finger + claw is its own single-prong part. The other candidates checked and ruled out: the draw verdict (the 2712 draw is `-> shells`,
not fur 0.8 %), dynamic / instancing / size limits (the refused `count 4170 / 408` draws are another character's), the texel rule on
the finger skin (98 % of the finger texels fur by the v8 windows), and the NEAR windows (hand back: all 34 triangles near, 96 -> 93 %
fur with them; fingers 98 -> 96 %; measured on the light-coat atlas by `v12_the_real_paw_near_windows_report`): they cost a few points,
they do not bare the hand.

**The fix (`classify_parts` in `src/furisland.rs`; shader unchanged).** When a part is cone-shaped AND holds islands the texels already
exclude (rule 1), its other islands (the rest) are judged again alone: the rest keeps its fur when (a) its own cone score is under its
own cut (`cone_cut` of the rest's own non-fur share, so the claw's texels no longer lower it) and (b) none of its islands is elongated
more than `REST_MAX_ELONG` = 2.0. The texel-excluded islands (the claw tips) stay excluded; NEAR flags the finger triangles that share a
position with a claw tip (their strands also read the FT_NEAR windows, unchanged). A part without texel-excluded islands (every real
horn of the tables: 87c3941a, 8cc07fea, c3d69b31, f8f1796d) is judged exactly as in v10/v11. Guard (b) comes from the data: the
finger islands of every paw table are 1.03..1.66 long, the only horn parts with texel-excluded islands (`da0abef0`, a dark head whose
horn strips are 45 / 15 triangles) have rest strips of 2.63..3.32: they stay excluded. The copy rule is unchanged (a spared island is
not excluded, so nothing copies it). The skin pass (old fur look + rim, v11) uses the same bits: the fingers get them back too.

**Per table (the real `furmesh-*.txt`, 49 tables; replayed by `v10_the_real_island_tables_copies_and_shape_candidates`: the islands of
a shape-excluded part that also holds texel-excluded islands and whose rest islands are all <= 2.0 long; the rest's cone needs the
triangles, so this is the upper bound the game can give back).** Excluded islands / triangles / area share:

| table | mesh | v11 | v12 (at most) | given back |
|---|---|---|---|---|
| 38c9862b | arm + hand 2811 | 28 / 530 / 10.2 % | 20 / 338 / 7.6 % | 8 finger islands (24 tris each) |
| 662bbef0 | arm + hand | 28 / 514 / 10.1 % | 20 / 322 / 7.9 % | 8 finger islands |
| 1db2dfaf | arm + hand | 28 / 498 / 10.2 % | 20 / 338 / 7.5 % | 8 finger islands (20 tris) |
| 24c4e630 | arm + hand | 33 / 606 / 22.9 % | 25 / 446 / 21.0 % | 8 finger islands (20 tris) |
| ffb9d184 | arm + hand | 28 / 498 / 8.4 % | 20 / 338 / 6.8 % | 8 finger islands (20 tris) |
| 9f8cd160 | arm + hand, low LOD 2901 | 45 / 881 / 79.9 % | 39 / 833 / 79.4 % | 6 finger islands (8 tris) |
| 87c3941a, 8cc07fea, c3d69b31, f8f1796d | heads | horns: 2 parts each by shape | same | 0 (no texel-excluded island in the horn parts) |
| da0abef0 | dark head 3804 | 44 / 1268 / 100 % | same | 0 (horn strips 2.6..3.3 long: guard b) |
| the other 37 | | | same | 0 |

The claws (tips 14 triangles, 99 % non-fur; the knuckle claws of the hand tile 66.5 %; the dewclaw 67 %) stay excluded by rule 1 in
every table; no horn island comes back. A character's 2712 mesh is not on disk as a table (only the 6 newest are written); its status
line has the same structure as 38c9862b (45 islands, 20 + 8 excluded, 88 near): expect `8 kept as fur skin under a texel-excluded claw`.

**Evidence (host).**
* `v12_a_hand_keeps_fur_on_the_palm_and_fingers_and_none_on_the_claws`: a palm plate (its own tile) + 4 finger tubes, each welded to a
  claw cone (glossy tan tip, coat-coloured base, 62 % non-fur) in the claws' tile. Each finger + claw part: cone 0.73 >= cut 0.25 (the
  v11 case: excluded whole); v12: the claw excluded by its texels, the finger alone cone 0.00 < 0.50, elongation 1.4: kept. Strands
  (the CPU mirror of the shells: triangle not excluded AND the strand texel rule, NEAR windows on near triangles) on 100 % of the
  palm's and every finger's texels, 0 % of every claw's; the dump says `4 kept as fur skin under a texel-excluded claw`.
* `v12_a_mitten_hand_with_claws_keeps_its_fur`: a closed mitten (flattened tube, one island) with 4 claws on its end ring, all one part
  in one tile: the part is not a cone (round 0.48, taper 0.96), the mitten fur on 100 %, the claws 0 %.
* `v12_a_horn_whose_rest_is_a_cone_too_stays_excluded`: a long cone whose tip island is texel-excluded and whose coat-textured base is
  itself tapered: the rest's cone 0.92 >= 0.50: excluded whole (the guard).
* `v12_the_real_paw_near_windows_report`: the NEAR windows on the real paw (above), asserted >= 85 % on the light-coat atlas.
* Changed test (pinned to data, not to replaced behaviour): `v10_the_real_island_tables_...` asserted that no island of a tile the
  shape rule may judge is > 10 % of its mesh; the small 8-island mesh `100a58c5` captured after v11 has a 16.9 % island there that its
  texels already exclude: the check now covers the islands the shape rule could still change. The same test prints the v12 replay.

**Debug output (v12).** `furmesh-*.txt` header `shell fur v12`, the count `N kept as fur skin under a texel-excluded claw`, rule 2's
v12 sentence; a kept finger island reads `fur (k near an excluded part) [part P cone 0.98 >= 0.25 only with its texel-excluded claw;
this skin alone cone 0.00 < 0.50: kept, v12]`, the parts table `cone-shaped only with its texel-excluded islands (claw): the rest (24
triangles) alone cone .. < ..: keeps its fur (v12)` or, kept excluded, `... the rest without its texel-excluded islands: cone / cut,
longest island elongation (> 2 = a horn strip)`. `status_shells=`: `ready: ... E excluded (a by texels, b by part shape, c copies; s
kept as fur skin under a texel-excluded claw, v12) ...`.

**Parameters.** `REST_MAX_ELONG` 2.0 (code). Everything else as v11.

**What to check in the game (v12).** (1) The hands of a playable charr: strands on the fingers and the hand
back, the claws still bare (no strands at the claw tips nor on the knuckle claws). (2) `status_shells=`: the `count 2712` draw reads
`28 excluded` -> `20 excluded (20 by texels, 0 by part shape, 0 copies; 8 kept as fur skin under a texel-excluded claw, v12)`.
(3) Horns, hair cards, everything else exactly as v11 (`4 by part shape` on the head draws unchanged). (4) A debug capture: in
`furmesh-<arm>.txt` the paw-tile parts read `keeps its fur (v12)`; in `furisland-<arm>.png` the finger islands green / yellow, the
claw tips red. If a finger stays bare: its part's row says which test kept it excluded (rest cone or elongation).

## v13: the thumb, and the fur in front of the horn

**Seen in the game on v12**: fur on all fingers except the thumb (four fluffy fingers,
the thumb with its big claw and smooth grey skin around its base); where the fur passes in front of the horn, it
disappears (a light-coated head in profile: where the head / neck fur lies over the horn on the screen its strands vanish, a smooth bare
edge; away from the horn the fringe shows).

**The thumb: cause.** v12 in the game: `count 2712: 20 excluded (20 by texels, 0 by part shape, 0 copies; 8 kept as fur skin under a
texel-excluded claw, v12)`: nothing is excluded by shape any more (an earlier reading of `1db2dfaf` was of the pre-v12 table). The
thumb is ONE island, skin and claw together (`1db2dfaf` islands 20 / 29, 18 triangles, uv 0.698..0.729 x 0.878..0.923, 66.7 % non-fur;
in the light-coat atlas the tan claw blob with a strip of white fur beside it), excluded whole by its TEXELS (rule 1, threshold 36.5 %): v12's
rest rule needs the claw in an island of its own. Every arm table has exactly these two islands (`38c9862b` 26 / 17, `662bbef0`,
`24c4e630`, `ffb9d184`, `920d3f4d`, `ba83e59d`: 18 triangles, 67..74 % non-fur).

**The thumb: fix (`analyze` in `src/furisland.rs`).** In a DIGIT tile (a tile holding a part whose finger skin v12 kept: proven
fingers with texel-excluded claw tips), a texel-excluded island keeps the triangles of its skin: a connected group of triangles each
texel-proven fur (own non-fur share <= `SKIN_TRI` 0.30, the group's <= `SKIN_MEAN` 0.15, >= 6 texels) that reaches the digit's
attachment: it shares a vertex position with a fur triangle of another island that is NOT itself a finger's skin (the hand). The claw
triangles stay excluded; the kept triangles next to them are NEAR. Why this is safe: a claw tip, even one with a coat-coloured base,
touches only its finger (a v12-kept island), never the hand; tiles without proven fingers (every head: horns, fangs; the knuckle claws
in the arm's tile) are untouched. Real tables (replay, `v10_the_real_island_tables_...`): digit tiles exist only in the 7 arm tables,
and in each the candidates are exactly the two thumb islands (18 triangles); the claw tips (99.7 % non-fur) have no fur triangle; no
head table has a digit tile. The kept triangle count needs the triangles (not on disk): `furmesh-<arm>.txt` reports it per island
(`EXCLUDED ... except its skin: k of 18 triangles at the attachment are fur (a digit with its claw in one island, v13)`), the header and
`status_shells=` count `N thumb-skin triangles kept, v13`.

**The fur in front of the horn: cause (code).** Not the island bits, the NEAR windows, the skin pass nor the depth tests: a coat strand
rooted on the neck whose fragment lies over the horn passes `root_visible` (its root is the visible coat) and the fragment test (the horn
is FARTHER than the root). It was removed in the COMPOSITE (`shell_look` in `light_adjust_hdr.hlsl`): the detail test (`shp2.x`, since
v2) keeps a strand over a fur-draw pixel only where the surface under it has the root's colour, made for the eyes, the nose, the
markings, and documented as "the horns stay clear of the strands of the white fur around them". The horn is part of the head draw
(the fur mask covers it), grey against the white root: `keep` ~ 0, whatever lies in front of what.

**The fur in front of the horn: fix.** `ps_shell` sets bit 31 of the key (`KEY_FRONT`, `shellfur.rs`) when the surface under the strand
fragment lies clearly BEHIND its root: `zp > zref + FRONT_DEPTH_ABS x FRAG_TOL_M + FRONT_DEPTH_REL x FRAG_REL_TOL x zref` (2 cm +
0.6 %). The composite strips the bit and, for such a strand, skips the detail test and does not apply that farther surface's occlusion
to it (it does not sit in that crease); the rest (coverage, shading, the under-darkening, no back light over a fur draw) is unchanged.
On the face of the coat (eyes, markings: the surface under the strand is the root's own surface) nothing changes. Strands of an
excluded triangle (the horn's own) still do not exist; `unpack_key` and `shell_key_at` mask the bit (keys stay below 0x7F000000).

**Evidence.**
* Host `v13_the_thumb_keeps_its_skin_and_its_claw_stays_bare`: the v12 hand + a thumb of ONE island (skin rings on coat, claw rings
  continuing the same uv strip onto glossy tan: excluded by texels) welded to the palm: 48 of 64 skin triangles kept, strands on 67 %
  of the skin's texels (the ring next to the claw reads the tan through the dilated windows), 0 % of the claw; fingers, palm, claws as
  in v12; the same thumb without proven fingers in its tile (a horn piece on a head): excluded whole.
* WARP `v13_coat_strands_in_front_of_the_horn_keep_their_colour_and_the_horn_grows_none`: the v9 island mesh with a coat "arm" of the
  same draw 24 cm in front of the horn's middle: 365 px of arm strands over the horn, KEY_FRONT on 365; on the horn's grey bands (247
  px) the pixel moved 9 % of the way to the root's colour without the flag (v12) and 33 % with it (v13; the soft strands' own coverage);
  no strand rooted on the horn; the body's strands over its own coat: flagged only towards the limb (3 of 40263 px more than 12 px
  inside the silhouette).
* Pinned test adjusted (data, not replaced behaviour): `v12_the_real_paw_near_windows_report` selects the finger islands by any painted
  class (a dump written by v12+ paints them fur / near, no longer red).

**What to check in the game (v13).** (1) The thumb: fur on its skin at the hand, the claw bare; `status_shells=` for `count 2712`:
`... ; N thumb-skin triangles kept, v13`; if the thumb stays bare and N = 0, a debug capture: the thumb island's line in
`furmesh-<arm>.txt` (its triangles' votes decide). (2) the view: the neck / head fur in front of the horn keeps its strands;
the horn itself grows none. (3) Eyes, nose, markings: strands still stop at them (the detail test on the coat's face). (4) Everything
else as v12.

## v14: the thumb's skin anywhere in a hand mesh, and the dumps no longer stale

**Seen in the game on v13**: the four fingers with fur, the thumb (upper left: a big tan claw with a
smooth grey / white skin patch around its base) still bare; `status_shells=`: `count 2712: ... 0 thumb-skin triangles kept, v13`.

**Cause.** v13 re-admitted skin only in the fingers' tile (the paw tile, v 0.93..1.0); the character's thumb skin is not there. The only
`furmesh` of the 904-triangle arm (`1db2dfaf`) was written by v10 and never rewritten (a debug capture wrote only the newest 6
analyses, and the arm is analysed first, so it was never among them): v12 / v13 verdicts of a character's arm were never on disk. The
texel-excluded islands of that arm mesh besides the claw tips: the four 18-triangle "cross" islands per hand in the hand tile (uv
0.695..0.731 x 0.751..0.804, 62.9 % non-fur, in the coat's tile next to the back of the hand, welded to it), island 14 / 8 (23 triangles,
84.7 %), and 20 / 29 (18 triangles, 66.7 %, paw tile). Which of them carries the thumb skin cannot be told without the triangles; v14
does not need to know.

**Fix (`analyze`).** The tile condition is replaced by a MESH condition: in a hand mesh (one where v12 proved finger skin under
texel-excluded claw tips), EVERY texel-excluded island re-admits the connected group of its own skin triangles (each <= `SKIN_TRI` 0.30
non-fur, the group <= `SKIN_MEAN` 0.20, >= 6 texels) that touches a fur triangle of another island which is not a finger's skin and is
at least as large as the excluded island (the hand / the arm; a claw tip touches only its finger; a one-triangle sliver is no
attachment). The claw triangles stay excluded; the re-admitted ones next to them are NEAR. **Guard for horns, fangs, lips:** a mesh
without proven fingers changes nothing, by construction; a head has none (no head table on disk has a v12 finger).

**Per table (all 80 `furmesh-*.txt` on disk, `v10_the_real_island_tables_...`).** 72 tables: no proven fingers, unchanged (every head
among them). 8 hand meshes may change; their candidates (texel-excluded islands under 90 % non-fur; upper bounds, the per-triangle votes
need the triangles): `1db2dfaf` (a character's arm) 12 islands / 226 triangles (the 8 cross islands, 20 / 29, 14 / 8); `38c9862b`,
`662bbef0`, `920d3f4d`, `ffb9d184` 10 / 180 each (8 cross islands + the two 18-triangle digit islands); `24c4e630` 15 / 288;
`ba83e59d` 14 / 220; `9f8cd160` (low LOD) 7 / 162. The claw tips (99.7 % non-fur) are never candidates.

**Dumps.** Every debug capture now rewrites the `furmesh-*.txt` of EVERY analysis of the session (the PNG still for the newest 6: it needs
the atlas), header `shell fur v14`; `Analysis::count` carries the draw's index count for it. An island line reads `EXCLUDED (no
shells): texels .. % non-fur > .. %, except its skin: k of N triangles re-admitted: its skin at the hand (v14)`; the header counts the
re-admitted triangles; `status_shells=` says `N skin triangles of texel-excluded islands re-admitted, v14`.

**Evidence (host).** `v14_a_thumb_in_the_coat_tile_keeps_its_skin_and_a_head_horn_stays_whole`: the thumb (one island, skin + claw,
86 % non-fur) lies in the COAT's tile (a tan stripe painted inside it), not the fingers' tile (asserted: v13 would keep nothing): 48 of
64 skin triangles re-admitted, strands on 65 % of the skin's texels, 0 % of the claw; the finger claws (coat-coloured bases touching
only their fingers) stay excluded; the same island in a mesh without proven fingers (a horn on a head) stays excluded whole. The v13
thumb test and the v12 hand / mitten tests unchanged and passing.

**What to check in the game (v14).** (1) The thumb skin with fur, its claw bare. (2) `status_shells=` for `count 2712`: `N skin triangles
of texel-excluded islands re-admitted, v14` with N > 0. (3) A debug capture writes the arm's `furmesh-<uvcrc>.txt` (`1db2dfaf`) again (header `shell fur v14`):
the island lines say which islands re-admitted how many triangles; if the thumb stays bare, that file tells which island it is and
why (its triangles' votes or no attachment). (4) Horns, claws, hair: as v13 (no head mesh can change).

### v14 (2): the charr of a crowd without fur

**Seen in the game** (the Black Citadel: a white spotted leopard NPC, a brown NPC and another beside him, 3..6 m
away): smooth skin, hardly a strand. Not the LOD (12.5 m). **Evidence** (debug captures):
`last frame: 24 draws drew shells (11 picked of 82 fur draws, 58 not listed)`, `(14 picked of 130 ...)`, `(12 picked of 47 ...)`. Three
causes, all ours:
1. **The record cap.** 24 records a frame, taken in the engine's draw order: in a frame of 47..130 fur draws (each charr is 3..8 draws:
   body, head, arms, legs, hair) only the first 24 got shells, whoever they were. The listed draws show the rest only as "not listed".
2. **Dynamic buffers refused** (v7): `dynamic vertex / index buffer (could be rewritten before the replay)` for counts 3456, 3480, 372,
   408, 4170 (atlases `dc7971f3`, `4d20f934`: NPC bodies), 8 of the 24 listed draws of that frame.
3. **Rigid draws refused by `shell_min_vcb=4096`** (v2): the `vs 59f8b692` draws (vertex cb 208 bytes) were hair cards in
   v2's evidence, but in a crowd they are NPC bodies and heads too: `count 1824` (atlas `c1e23e47`: 13823 px = 28 % of the frame's fur
   pixels, not fur 2.1 %), `count 1140` (`341f40d6`): 5 of the 24 listed draws.
The draw test (gloss band 85 % on these atlases) and the island analysis were not the cause.

**Fixes (`src/shellfur.rs`).**
1. `MAX_RECS` 64 (was 24) plus a triangle budget `MAX_FRAME_TRIS` 400000 a frame (`admit`): a frame of 82 / 130 fur draws gets 64
   records (was 24), one of 47 all of them. Far records cost little: the geometry shader ends every triangle beyond the LOD at once. The
   engine's draw order stays the priority (no CPU-side screen size exists at the draw; the LOD culls the far ones on the GPU).
2. **Dynamic buffers are copied at the draw** (`Shell::copy_dyn`): each DYNAMIC vertex buffer (slots 0..3) / index buffer, whole, into a
   mod-owned DEFAULT buffer of the record (pooled per record index and slot, re-created only when its size / binding changes;
   `CopyResource` on the GPU timeline, so it holds what this draw read even if the engine Map-DISCARDs the buffer later in the frame). Up
   to `DYN_MAX_BYTES` 4 MB a buffer and `DYN_FRAME_BYTES` 32 MB a frame; larger: refused as before (`... too big to copy`). The island
   analysis does not run on a copied record (its pooled buffer holds another mesh each frame): those draws keep the texel rule (v8).
3. **Rigid draws**: `shell_min_vcb` default 0; a SAVED 4096 (the old default, in every config written by v1..v13) reads as 0;
   any other value is honoured. The hair cards among the rigid draws fail the per-draw test (gloss 0.56) like the skinned hair (v11):
   no shells, no old look.

**Status** (`status_shells=`): `v14 records: all N fur draws by verdict [shells: a, too many shell draws this frame (cap): b, dynamic ...
too big to copy: c, ...]; cap 64 records / 400000 triangles a frame (T recorded); D with a dynamic buffer copied (X MB)`, every draw
counted (also the ones past the 24 listed).

**Tests.** Host `v14_the_record_cap_and_the_triangle_budget` (`admit`; the captured frames: 82, 130 and 47 fur draws -> 64 / 64 / 47
records, v13 24 / 24 / 24). WARP `v7_buffers_changed_between_record_and_replay_never_ghost` (changed: v7..v13 asserted the refusal of a
dynamic buffer, the replaced behaviour): a DYNAMIC vertex buffer is replayed with keys identical to the DEFAULT buffer's, and when the
engine rewrites it (Map WRITE_DISCARD, the mesh moved 25 cm) after the draw the replay still draws the recorded mesh (same keys); a
RIGID draw (vcb 208) is replayed like the skinned one (default and saved 4096), `shell_min_vcb=8192` leaves it out. Changed:
`selector_takes_the_skinned_fur_draws_and_the_counts` (rigid now Ok), `shell_keys_default_...` (min_vcb default 0).

**Cost.** CPU ~0.012 ms a record (0.35 ms for 24 in the crowd frame): up to ~0.8 ms for 64. GPU: the geometry shader's 32 instances per
triangle of every record (the far ones return at the LOD test); the copies: at most 32 MB a frame of buffer-to-buffer copies (the
measured dynamic draws are small meshes: 124..1390 triangles). Memory: the copy pool (per record and slot, the largest size seen).

**What to check in the game (v14 crowd).** (1) The NPC charr of the Black Citadel crowd have strands. (2) `status_shells=`: `v14 records: ...` shows how many fur draws
the crowd had and why any was left out (cap / budget / dynamic too big). (3) `status_perf=` in a crowd with `fur_shell=1` vs `0`.

## v15: two-tone arms, the dynamic RANGE copy, the gloved thumb

**Seen in the game on v14**: an NPC in a vest: fur on the chest, the arms / shoulders / flanks (tan,
pink) smooth; a GLOVED hand: the thumb skin the glove leaves bare has no fur (a bare hand: fine). `latest.txt`: `v14
records: all 90 fur draws by verdict [shells: 54, dynamic vertex / index buffer too big to copy ...: 36]; ... 0 with a dynamic buffer
copied`, the refused ones being the rigid `vs 59f8b692` NPC bodies (counts 1824, 5214, 768, 1086); `shell_min_vcb 4096` printed.

**1. The rigid / dynamic NPC bodies: the RANGE copy.** v14 copied a dynamic buffer whole (<= 4 MB): the rigid draws read shared RING
buffers far bigger than that, so every one was refused (36 of 90 fur draws). v15 copies only the bytes the draw reads
(`dyn_windows`): the index range [start, start + count) of a dynamic index buffer and, per vertex stream, the window [offset + base x
stride, + count x stride) (a draw has at most as many vertices as indices; a window is clipped at the buffer's end), into pooled
DEFAULT buffers that only grow (64 KB steps), with `CopySubresourceRegion` at the draw; the call is rebased onto the copies (base 0,
start 0 when the index buffer was copied). Instanced draws stay refused. The island analysis now runs on these records too, keyed on
the mesh (atlas, index count, vertex shader, strides) instead of the pooled buffers; the uv-hash re-check catches a change.
`shell_min_vcb 4096 (the pre-v14 default: in effect 0)` in the status (the saved value is honoured as documented in v14; the line
now says so).

**2. The two-tone coat.** `e8bc2bb8` (count 6198, atlas `a01d2766`: a grey coat with RUST bands on the arms): island 66, the upper-arm
strip (90 triangles, 7 % of the mesh, elongation 3.2), `EXCLUDED: texels 53.6 % non-fur`; the forearm island beside it all NEAR. The
measurement (`v15_report_keratin_and_red_vs_two_tone_fur`): the rust fur of that atlas is non-fur by the COLOUR tests (45 % of the arm
strip, 54 % of the hand arc; the keratin test alone 14..17 %, the rest the red test), exactly like the claws (70..93 %) and fangs:
per texel the colours of rust fur and claw keratin do not separate, and neither does the flatness (claw flat / coat median 0.5..3.0,
rust fur 0.6..3.3). The same pattern in the other arm meshes: the arm strip (u 0.70..1.0, v 0.67..0.75) excluded in `24c4e630`
(40.3 %), `bfb92527` (44.7 %), `9f8cd160` (69.6 %), the forearm in `cdf300b8` (63.1 %), `9f8cd160` (97.6 %). **Fix (`analyze`):** a
LIMB island is given back: in the coat's tile (the tile of the mesh's largest island, itself >= 15 % of the mesh: never a horn mesh's or a
claw's tile), >= 3 % of a mesh of >= 500 triangles, elongation >= 2 (an arm / leg strip; a face patch or the lips 1.05..1.2), not
hard-shaped, excluded by its texels ONLY because of the colour tests (its share without them, `texel_weight_opt(.., false)`, under
its threshold). Its triangles carry the bit value 3 (`tri_bits`): shells, and the strand shader skips the keratin / red tests there
(`ft_texel_weight(.., colour)`, `strand_fur` near_part 2); the flat and glossy tests stay. Its sub-regions are judged without the
colours too. Candidates in the 96 tables on disk (`v10_the_real_...` replay; the colour-free share decides in the game):
`e8bc2bb8` 66, `24c4e630` 50, `bfb92527` 41, `cdf300b8` 48, `9f8cd160` 38 / 48, `09fff74c` 52 (100 % non-fur: armour paint, most likely
kept out by its flat / gloss texels); no head table.

**3. The gloved thumb: the digit boxes.** A gloved arm mesh (`f6066c23`, count 3798) has no finger of its own (its digit islands are
the glove-cut tips, each excluded by its texels), so v14's hand-mesh condition never held there. v15 remembers, for the session, the
uv boxes a HAND mesh proved (its kept finger skins and its texel-excluded islands that re-admitted their skin: the charr hand layout is
one template, `DIGITS`, at most 256); a mesh without proven fingers re-admits the skin triangles of a texel-excluded island whose box
overlaps a learned one (intersection over the smaller box >= 0.7: the real bare / gloved finger pair 0.55 IoU, the thumb pair 1.0),
without the attachment condition (a gloved digit may touch only the glove's cut). An analysis that had such candidate islands is run
again when the memory grows (`digit_wait` / `DIGIT_GEN`). Horns, fangs, lips lie elsewhere in the atlas: no learned box overlaps
them.

**Dumps / status.** `shell fur v15`; the header and `status_shells=` count `two-tone limb islands given back` and `digit islands by the
learned digit boxes`; island lines `fur (v15: a limb of a two-tone coat: x % non-fur with the colour tests, y % without; ...)` and
`... re-admitted: its skin on a digit box learned from a bare hand (v15 ...)`.

**Tests.** Host: `v15_a_two_tone_limb_keeps_its_fur_and_the_lips_and_claws_stay_out` (a rust arm strip: excluded by its colours,
given back, bit 3 on every triangle, strands on 100 % of its texels; a short red patch and a claw-sized strip stay out; a mesh under 500
triangles gives nothing back); `v15_a_gloved_hand_learns_the_digit_boxes_of_a_bare_hand` (the bare hand proves 5 boxes; the gloved
mesh: whole and waiting without them, 48 skin triangles with them, 65 % strands on the skin, 0 on the claw; a horn elsewhere
unchanged); `v15_the_dynamic_copy_windows`. WARP `v7_buffers_...`: dynamic RINGS (base vertex 6000, start index 9000, both rewritten
with garbage after the draw): the replay's keys equal the plain mesh's. Report: `v15_report_keratin_and_red_vs_two_tone_fur`. Changed
(pinned to the debug folder's live data, which every session rewrites): two v10 / v12 report checks now print instead of assert (a new
266-triangle table and a 13 % island in `353853b0`; the near-window share of a re-painted dump).

**What to check in the game (v15).** (1) The NPC in the vest: fur on the arms and shoulders. (2) `status_shells=`: `v14 records: ... dynamic
... copied` > 0 and no `too big` in a crowd; the rigid NPC bodies with shells. (3) The gloved thumb: fur on its bare skin (after
a bare-hand charr has been seen in the session: `digit islands by the learned digit boxes` > 0 for its arm draw). (4) Claws, horns,
lips, fangs, hair cards as before.

## v16: the fur no longer depends on the camera angle

**Seen in the game on v15** (one charr, two close camera angles in the Black
Citadel): head and tail fur visible in one, mostly gone in the other. **Cause (`latest.txt`):** `v14 records: all 136 fur draws
by verdict [too many shell draws this frame (cap): 72, shells: 64]` (other captures: 101 draws / 37 refused; the older
pairs 47..247 fur draws a frame). The 64 records went to the first 64 fur draws in the engine's DRAW ORDER; the order and the culled
set change with the camera, so which character (or which of its parts: head, tail, arms are separate draws) lost its shells changed
with the angle. Checked and not the cause: the LOD (the character is ~2..4 m away, the LOD is 12.5 m), the edge-on fade (per
triangle, |cos| of its own face normal with its own view ray 0.08..0.20: only grazing triangles, never the body's orientation), the
island bits (cached per mesh; v15's copied records are keyed on atlas, vertex shader and index count, stable across frames), the dynamic
windows (an invalid window refuses the record, it does not thin it), `status_occlusion=` (its "would erase 39 %" is the fur mask's
occlusion check reporting; it does not gate the shells).

**Fix (`src/shellfur.rs`).** `MAX_RECS` 256 (was 64) and `MAX_FRAME_TRIS` 800000 (was 400000): every fur draw of every frame on disk
fits (the largest seen: 247). Past either limit the choice is STABLE: each record carries the draw's identity (atlas, vertex shader,
index count, instances, topology); a draw that had shells in the previous frame takes the slot of the last record that did not
(`victim_of`, within the triangle budget), so the set of shelled draws holds frame to frame instead of following the draw order.
Shader: `strand_fur` reads the coat statistics of record 0..255. Status: `v14/v16 records: ... cap 256 records / 800000 triangles a frame
(T recorded; E displaced by a draw shelled last frame)`.

**Cost.** CPU: 0.496 ms for 64 records in that frame (0.008 ms a record): ~1.1 ms for 136, ~2.0 ms for 247 records. GPU: the
geometry shader's 32 instances per triangle of every record (the far ones end at the LOD test); that frame recorded 60387
triangles for 64 draws, so ~130k for 136. Memory: the feature target 32 x 8192 RGBA32F (4 MB) and the per-record copy pools. To be
measured in the game (`status_perf=` in the Black Citadel with `fur_shell=1` vs `0`).

**Tests.** `v14_the_record_cap_and_the_triangle_budget` (changed: pinned 64 / 400000): the frames of 47 / 82 / 130 / 136 / 247 fur
draws take every draw. `v16_past_the_cap_the_draws_of_the_last_frame_keep_their_shells_whatever_the_draw_order`: 300 draws, 20 frames
in changing orders: after the first frame the same 256 keep their shells in every frame (by the draw order alone 503 draws gain or lose them over the 19 changes).

**What to check in the game (v16).** (1) Turn the camera around the charr in the crowd: head and tail fur stay. (2)
`status_shells=`: `v14/v16 records: all N fur draws by verdict [shells: N]` (no cap) in the Black Citadel. (3) `status_perf=` with
`fur_shell=1` vs `0` there; if the cost is too high, report it (a distance priority would need the draws' positions).

## v17: first person and the close camera

**Seen in the game** (on v15/v16): the fur vanished with the view angle, mostly in first person. **Cause (code + WARP).**
The shell geometry shader dropped a triangle WHOLE when one of its vertices had clip w <= 0 (`load_tri`: behind the camera). With
the camera at or inside the character (first person, a close camera), its large triangles (arms, chest, the head around the eye)
reach behind the camera: their visible parts lost every strand, and which ones did changed with every turn of the head. The WARP
measurement (`v17_triangles_reaching_behind_the_camera_keep_their_strands`: a 1 x 1 m card nearly along the view, from behind the
camera to 0.78 m): 32 x 32 quads (few crossing triangles) 0.95 strand keys per card pixel, 2 x 2 quads (the near ones all crossing)
0.218 with the v16 rule. Checked and not a cause (`v17_close_camera_and_angles_keep_the_strands`: a sphere at 1.4 / 0.9 / 0.7 / 0.62
m, cards at |cos| 0.9 / 0.6 / 0.4 / 0.25 and 1.4 / 0.4 / 0.2 m): 0.72..0.98 keys per px everywhere, the edge-on fade, the strand-length
clamp (64 px), the root / fragment depth tests and the LOD all hold at 0.12 m.

**Fix (`gs_shell`, `shaders/shellfur.hlsl`).** The triangle is clipped at the NEAR PLANE (clip z >= 0, and w >= SHELL_WCLIP) into one
or two sub-triangles (rotated with selects, no dynamic indexing; uv and vertex normals interpolated; `maxvertexcount(6)`), and each
visible part grows its shells exactly like a whole triangle (the island bits of the triangle apply to both). Measured: the coarse card
0.921 keys per px (v16 0.218), the fine card 0.951. A clip at the eye itself (w > 0) was tried first: the cut vertices project far off
screen, the root's depth plane loses its precision and the root test fails (0.22): hence the near plane.

**Not in v17 (data needed).**
* A charr's thigh smooth (the same crowd, its tail with fur): no capture so far holds that charr (the newest
  `latest.txt` has no draw with a yellow coat; no `furmesh` with a coat hue of 50..70). Candidates: the leg is part of an
  armour draw (trousers + skin in one atlas) whose per-draw test says "not fur" (gloss band share < 40 %), or its texels fail the
  colour tests relative to that draw's coat. A debug capture standing next to that charr (its draws' `-> ...` verdicts and gloss band
  share in `status_shells=`, `furmesh-*.txt`) decides; a per-texel gloss-band rule for mixed draws would risk fur on armour.
* Cutscenes / dialogue: the frame classifier closes the enable gate in those frames (`gw2graphics_log.txt`: `frame does
  not match: no opaque pass (passes=11 / 23)`, `no linear depth pass (passes=13)`, `no ui pass`, `no luminance pass`), and with it the
  substitutes (the light-adjust draw that composites the shells), the fur mask and the linear depth the shells use: every effect of the
  mod is off there, the fur included. Making the shells independent of the classified frame (their own composite target and the
  device depth only) is a larger change; recognising the cutscene layout needs a frame-pass dump taken during a cutscene (overlay:
  "dump frame passes").

**Far tail.** The composite read a strand's root colour bilinearly at its anchor; at a silhouette (a far tail's
lower edge) the four pixels can include the background, which then tints the strand. v17 weighs the four by the fur mask's presence
(`shell_root_colour` in `light_adjust_hdr.hlsl`; no fur pixel among them: as before). The WARP scene (`v17_far_strands_take_the_coat_
colour_not_the_background`, a white coat at 6 m over a near-black background) changes only a little (mean luminance of the strand
pixels over the background 0.067 -> 0.074, nearly black 23 -> 22 of 108): it has no dark underside like the game's tail, so the cause
seen in the game is not proven; a debug capture at that distance would show it.

**Effect switches (overlay, all effects).** `toggles.rs`: a checkbox at the top of every effect section (bloom, highlight boost, AO,
halo, SSR, sun shafts, contact shadows, aerial, auto exposure, local tone, lens, grading / LUT, fur look, shell fur, rim light), saved
as `<effect>_enabled` (1 = default, a missing key = on), hot reloaded, applied through each module's own strength getter (off = strength
0, or the neutral value: bloom 1 = the engine's, highlight boost 1): the same path as the slider at 0, the slider values kept. A runtime
master "All effects off (A/B vs the engine)" in General (not saved; the per-effect switches come back). `status_toggles=` in the dev
capture.

**What to check in the game (v17).** First person and a close camera turning around: the arms' / chest's fur stays. Third person as
v16. The overlay switches: each effect off / on live; "All effects off" for an A/B.

## Which draws: the selector and the evidence

The fur selector `fur_ps=1d50763f,e0189185,5d00827f` (verified magenta) picks the charr's skin AND its cloth armour (same pixel shader). What the
per-draw logs from the game (`GW2Graphics\draws-*.log`, 10 dumps) show for those shaders:

| vs | ps | vcb0 | draws | index counts | stride | textures |
|---|---|---|---|---|---|---|
| ba0709f4 | 1d50763f | 16464 (skinned) | 3 | 17694 + 2 others (23760 total) | 40 | 4096 BC3 + BC5 (the same 2 textures for all 3) |
| 59f8b692 | 1d50763f | 208 (rigid, one matrix) | 1 | 5214 | 56 | the same 4096 BC3 + BC5 |
| a5c9d651 | e0189185 | 16464 | 2 | 13788 total (max 8670) | 36 | 1024 BC5 + BC3 (slots 1, 2) |
| 3c3a91ca | 5d00827f | 16464 | 1..17 | many (other characters too: 16 texture sets) | 40 | 2048 / 4096 |

Nothing in these logs separates fur from cloth inside the skinned charr group (same shaders, same textures, same constant-buffer sizes); only the
index count differs per mesh, and the rigid 208-byte draw is certainly not a fur body. So the **default** is `shell_min_vcb=4096` (skinned only:
drops the rigid 59f8b692 draw) and every other fur draw gets shells (`shell_ps` empty = `fur_ps`, `shell_vs` empty, `shell_counts` empty).
Confidence: high that the rigid draw is excluded, LOW that the remaining skinned draws are only fur (the cloth armour pieces are probably among
them). The correction is meant to be done live from the next capture: `status_shells=` lists every fur draw of the last frame as
`[vs ps vcb count inst srv -> shells | why not]`; with `shell_debug=1` on, take or leave meshes by their index count (`shell_counts=17694` takes
only that mesh, `-5214` leaves one out) or narrow by `shell_vs` / `shell_ps`.

## Keys (`config.ini`, all saved, all hot reloaded with `debug_hot_reload=1`, all in the overlay section "Fur" > "Shell fur")

| key | range | default | meaning |
|---|---|---|---|
| `fur_shell` | 0 / 1 | 1 | shells on; 0 = only the old fur look |
| `fur_shells` | 0, 4..32 | 16 | layers (the base included); 0 = off; 1..3 become 4 |
| `shell_length` | 0.5..20 cm | 2.5 (v10; was 4) | strand length |
| `shell_density` | 50..4000 | 1200 (v1 800) | strand cells per uv unit at the densest level |
| `shell_thickness` | 0.1..1 | 0.6 (v1 0.9) | root diameter of a strand (share of a cell) |
| `shell_root_dark` | 0..1 | 0.6 (v1 0.65) | self-shadowing of the coat: roots / skin darker, tips brighter, the average kept |
| `shell_wind` | 0..1 | 0.15 (v1 0.3) | wind sway of the tips |
| `shell_lod_distance` | 2..100 m | 12.5 (v10; was 20) | no shells beyond, shorter and fainter from half of it (continuous); the default scales with the default strand length (20 m x 2.5 / 4) |
| `shell_rim` | 0..2 | 1 | back light against a bright background (never in the dark) |
| `shell_ps` | hashes | empty | pixel shaders of the shell draws (a subset of `fur_ps`); empty = all fur draws |
| `shell_vs` | hashes | empty | vertex shaders they must use; empty = any |
| `shell_min_vcb` | bytes | 4096 | smallest vertex cb 0 (skinned only) |
| `shell_counts` | list | empty | index counts to take (`17694`) or leave out (`-5214`); empty = all |
| `shell_normal` | face / TEXCOORDn | face | the extrusion direction |
| `shell_unit` | 0 / 0.01..1000 | 0 | engine units per metre; 0 = auto (measured, smoothed, one per frame) |
| `shell_debug` | 0 / 1 / 2 | 0 | 1 paints the shell pixels red (root) .. yellow (tip); 2 = a colour per fur draw (v4) |
| `shell_detail` | 0..1 | 0.7 (new) | over the body a strand only paints where the surface looks like its root: eyes, nose, markings, horns stay clear; 0 = v1 |
| `shell_clump` | 0..1 | 0.5 (new) | clumping: strands pulled towards a shared point and leaning the same way (0.5 = the v1 pull + a slight lean) |
| `shell_fur_mask` / `shell_fur_threshold` / `shell_fur_soft` / `shell_uv_exclude` | see v3, v5, v7 | `t0.a<, t0.f>0.025` (v5; was `t0.a<`) / 0.66 / 0.05 / empty | the mask's texture tests (old fur look, rim, `fur_debug`); for the shells (v7) `off` turns the per-strand texel tests off (the draw test stays) |

Shells need the fur mask: `fur_ps` non-empty and `fur_strength` > 0 (the strands scale with it). Mod disabled by the user (vanilla emulation),
the fur selector window's highlight, `fur_shell=0`, `fur_shells=0`: nothing recorded, `t24` / `b13` unbound: bit-identical to the old output (tested).

## Slots and constants

`t24` (R32_FLOAT, packed keys) and `b13` (`[0]` = on, root darkening, back light, debug; `[1].x` = detail) of the light-adjust substitute. The
shell pass itself uses its own `b0` (geometry shaders: shells, length, max px, unit of the frame; projection scale and viewport; front winding,
LOD; wind, gravity, depth unit; the wind phases) and `t0` (the frame's linear depth, the AO's capture: the shells register `ao::DEPTH_FOR_FUR`) /
`t1` (the engine's device depth, viewed through its typeless texture) of the geometry shaders, `b0` (v7: 20 float4, `q3` = the root tolerances) /
`t0` (linear depth) / `t1` (v7: the device depth) / `t2` (v7: the draw's atlas) / `t3` (v7: the coat statistics) of the strand pixel shader, its own
D32 score buffer, a 1x1 RGBA32F measurement target and three 1x1 staging textures; v7: the 32 x 768 feature and 24 x 2 statistics targets
(RGBA32F) and three 24 x 2 staging textures. v9: `t2` of the shell geometry shader = the per-triangle island bits (`Buffer<uint>`,
R32_UINT, one per mesh; unbound = v8), the stream-output target 0 during the capture draw only (saved / restored with the replay's
state), per capture a stream-output and a staging buffer, per atlas one staging texture (all released after the read-back). `b13` of the composite is 2 float4 again (v6's picker preview removed).
v11: `t25` of the light-adjust substitute = the skin target (R8_UNORM, the shell target's size) or a 1x1 zero texture; the skin pass uses the
shells' `b0` with `g5.x = 1`, its own pixel shader `ps_skin` with `t0..t3` as the strand shader and `t4` = the fur mask (only in that pass).

## Cost

CPU per recorded draw: about 40 COM calls + one constant-buffer copy at the draw, the same again at the replay plus about 15 for the unit
measurement (v2; one more pass of the fur meshes, without instancing, into a 1x1 target) (measured in WARP including
WARP's own work: 30..50 ms for one 9216-triangle sphere at 512x384 with 16 shells; on a GPU the CPU part is a few hundred microseconds).
GPU: the fur meshes once more with up to 32 geometry-shader instances per triangle, inside the LOD distance only. No allocation per frame
(fixed-capacity vectors, buffers reused), every shader compiled on a background thread, every hook under `catch_unwind`, `try_lock` only.
Fail open: a failure (objects, depth view, geometry shader) leaves the old look and logs once (`shellfur: ...`).

## The fur mask's occlusion check (fixed with this change)

In-game captures (`status_occlusion=`) showed the check erasing 90..98 % of a FULLY VISIBLE charr, with a constant gap of
3.87e-6 (65 D24 steps) between the stamp and the final depth: the character draws use a depth bias that the pixel shader's SV_Position.z
does not carry on that GPU (WARP does carry it). The tolerance `OCC_TOL` (2e-6) was smaller than the gap. It is 1e-5 now (168 steps; real
occluders such as a hood are 1e-3 and more away) plus the depth bias of the masked draws' rasterizer state (`bias_tol`, passed to the check
as `b0`). The earlier "stamped 0 px" (8181 mask px unstamped) was the probe hitting the first frames after a loading screen, while the
shaders generated per vertex signature were still compiling (the static fallback shader cannot stamp): the probe now says so
(`N drew with the static fallback shader`) and prints the tolerance and the rasterizer's depth bias. The hood-over-head fix depends on this
check; the shells do not (they test the final linear depth themselves). See `draw-diagnostic.md` and `debug-mode.md`.

## Tests (`shellfur_tests.rs`, `furmask_tests.rs`, `hdr_shaders_tests.rs`, `debugshot_tests.rs`, `config.rs`)

* Host: keys (defaults, parse, clamp, NaN / garbage, the ini block, round trip, one-by-one hot reload through `debugshot::merge`), the selector
  (the reference frame: the skinned draw yes, the rigid 208-byte draw no; shell_ps / shell_vs / shell_counts), the packed key (round trip with the
  coverage, never 0 / inf / NaN), the constants (clamped, zero when not ready), the status text, the unit tracker (first frame publishes, +-0.5 %
  noise never moves it, a wrong frame moves the running mean by at most one step, a lasting change is followed in small steps), the wind phase
  (continuous at 1 h, 1 day, 10^7 s; NaN-safe).
* The geometry shaders (shell + measurement) compile and link (`verify_link`) against the three real character vertex layouts of the game.
* WARP scene through the real pipeline: a lit 0.5 m sphere (the coat) with a vertex shader of the engine's output signature (16464-byte vertex
  cb), a dark cloth quad drawn later 12 cm in front of it, a bright sky; fur mask + occlusion check + record + replay + the real light-adjust
  substitute, several consecutive frames on one device like the game. Asserted: strands outside the base silhouette and never farther than the
  px guard; roots darker than tips; no strand on any cloth pixel and the cloth bit-identical; the body covered and its mean brightness within
  5 % of the body's own (1.003); no NaN / negative; one replayed draw; nothing beyond the LOD distance; `fur_shells=0`, `fur_shell=0`,
  `fur_strength=0` and half-bound inputs = the old output bit for bit; over a black sky no strand is brighter than its root; the replay restores
  the pipeline state; a second light-adjust draw of a frame binds the same shells, the next frame none until drawn.
* Temporal (v2): five identical frames: the measured unit settles on the scene's true 1.000 per metre within 1 % (1.0002, 1649 triangles) and
  the frames after it are bit-identical (keys and picture); a camera move of a third of a pixel (then a fifth) changes the fur by a mean 0.011
  and 0.2 % of its pixels by more than 0.1 (bounded: < 0.03 and < 6 %), no body pixel loses its shells; at 4.5 m the same, and never worse than
  the v1 per-triangle unit.
* Face (v2): a small dark "eye" at a grazing angle: with `shell_detail` it keeps its brightness (0.064 vs the engine's 0.065), with 0 the
  strands smear it (0.216), the coat around it still gets strands.

Renders (with `GW2G_SHELL_PNG=<folder>`, suffix `-v2`): `shells-close-v2.png` / `shells-close-old-v2.png` (1.4 m, with shells / the old fur look),
`shells-close-zoom-v2.png` (4x, the upper right silhouette), `shells-mid-v2.png` (4 m), `shells-debug-v2.png`, `shells-dark-v2.png` (black sky),
`shells-face-v2.png` and the 4x zooms `shells-face-old-zoom-v2.png` / `shells-face-nodetail-zoom-v2.png` / `shells-face-zoom-v2.png` (the
engine, the v1 smear, v2).

## What to check in the game

v7 first:

1. Transparency: the Black Citadel crowd and a close-up of the arm (the same view): bracers, belt and cape are solid, no white
   speckles over them, no ghosted cape edge; NPCs (charr and human) look like vanilla except the charr's fur. Human NPCs: no strands.
2. Horns, claws, fang, lips: no strands on a charr (no config line needed: a saved `shell_uv_exclude` can be emptied and the
   `shell_fur_mask` line set back to the default), and on an orange / grey / dark charr NPC (orange: the matte base of a claw may keep a
   few strands; report).
3. The coat keeps its fur on every colour (white, grey, orange tiger, dark); the outline of head, arms and tail still has strands against
   the sky. The MANE's hair cards (draw 17694) get no shells any more (gloss of hair, like human skin): say if the mane looks bare.
4. `status_shells=`: `per-strand fur discriminator (v7 ...)` lists every replayed draw: the charr's skin draws `-> fur draw` (gloss band
   65..95 %), armour / human draws `not fur ..., no shells`; a `dynamic vertex / index buffer` verdict in the draw list means such draws
   are skipped (if a character shows it, its fur is gone: report). In a crowd only the first 24 fur draws can get shells.

Then the v2 checks:

1. The flicker: stand still near a charr (5 m and a close-up), idle animation running, and look at the head, the mane and the outline
   against the sky for a few seconds: no blinking strands or patches. If something still blinks, set `shell_unit=` to the unit that
   `status_shells=` printed (A/B test of the measurement) and report.
2. `status_shells=`: `unit auto: <n> per m (measured over N triangles, spread x %, F frames ...)`; the same n in two captures of the same
   place (it may differ between maps); shells drawn > 0; the per-draw list (which counts got shells).
3. The face: eyes, nose and mouth as sharp as in vanilla, the fur around them and the mane with strands; the coat's brightness like vanilla
   (not greyer, not whiter). Too little fur on the face: lower `shell_detail` (0.4); still smeared: raise it (1.0).
4. `shell_debug=1` + `latest-mod.png`: shells on the fur (skin, head, limbs, tail), not on the cloth armour. If cloth has them, use
   `shell_counts` / `shell_vs` from the per-draw list. The `count 1854` draw (shader e0189185): what is it?
5. A hood / armour in front of fur: no strands over it. Strands at the silhouette against the sky: soft, lit at the tips; at night: none lit.
6. `status_perf=` with `fur_shell=1` vs `0` (the extra cost; v2 adds one measurement pass of the fur meshes).

## Limits

* The view space is rebuilt with an assumed field of view (tan half fov 0.5, the AO's); a different FOV slightly changes the strand direction
  and length on screen (not the occlusion).
* Gravity assumes a level camera ("down" in view space).
* The vertex buffers are referenced, not copied. v7: a draw with a DYNAMIC vertex / index buffer is not recorded (no shells for it); a
  DEFAULT buffer rewritten in place before the replay is not detected, but its strands fail the root test (no ghost, tested).
  `VSSetConstantBuffers1` offsets are not replayed.
* Face normals (the default) leave small gaps between the shells of strongly angled neighbouring triangles (hidden by the strands);
  `shell_normal=TEXCOORDn` removes them if the engine's vertex shader outputs a normal there (to be read from its disassembly).
* Only the four hooked `Draw*` calls; at most 24 shell draws per frame, the first 24 in draw order (in a crowded city the local player's
  draws can be past them: 212 fur draws in the Black Citadel capture). v7 drops the non-fur ones only at the replay, not at the record.
* v7 draw test: armour pieces whose gloss is the fur's (some leather: up to 60 % in the fur band in `5566458d`) can still pass as fur
  draws; human skin, metal and hair cards do not (hair cards: the charr's mane cards, count 17694 on a charr, get no shells any
  more; the head draw's own mane fur keeps them).
* v7 texel tests on an orange / tan coat: the claws' matte base keeps strands (only the glossy part goes); the nose is not a rule.
* The unit measurement needs the linear depth and the device depth of the frame; until its first read-back (two frames after a load or a
  device change) the per-triangle guess of v1 is used.
* `shell_detail` compares colours: a feature with the coat's own colour (a white horn tip on white fur) is not protected; strands rooted ON a
  feature of its own colour (the horns) still grow there. (v3: the fur discriminator removes them where the texture's gloss or a uv rectangle says "not fur".)
* One fragment per pixel (the best score): a pixel where two strands overlap shows the more covering one, not a blend of both.
* v9: the island analysis needs a non-instanced triangle list (the character draws are); a part fused into a fur island AND painted
  like fur (no texel votes) cannot be told (shape alone never excludes); the head fur right at a horn's base keeps its strands (they
  may lean over the horn); a mesh's verdict is fixed until its uv hash changes (re-checked every 900 replays). The old fur look, the rim
  light and `fur_debug` do not use the island verdict.
