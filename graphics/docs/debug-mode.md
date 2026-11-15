# Debug mode

A way to watch the real game and tune the mod without describing the picture in words: the game periodically saves
what the player sees (with the mod, and a vanilla comparison), plus numbers about it; `config.ini` edits are applied
live. Everything is OFF by default; normal users never see it.

## Enable (in `GW2Graphics\config.ini`)

```
debug_capture_seconds=20   # a capture every 20 s (0 = off, max 3600)
debug_compare=1            # each capture is a PAIR: mod + vanilla (0 = only the mod shot)
debug_hot_reload=1         # re-read config.ini when it changes and apply it live
```

The overlay's Graphics tab (Shift+Home) has collapsing sections
(General / HDR, Bloom + highlight, AO, Halo, Reflections, Sun shafts, Anti-aliasing & filtering, Fur, Debug); labels have their
own room and long texts wrap. The Fur section has the full selector (list editor, preset, magenta view) and **Pick fur shaders**,
a live table of the skinned draws with a per-group magenta highlight (see "The overlay's fur section and shader picker" in
`draw-diagnostic.md`). **Save** writes config.ini; hot reload re-reads it without a feedback loop (same values).

The same switches, plus **Save screenshot now**, **Save screenshot pair now** and a status line, are in the overlay
under "Dev". They are not saved unless you press Save.

Photo mode (`photo.md`) reuses this capture machinery (staging ring, writer thread, the pair state machine with its restore
guarantees and watchdog) for full-resolution captures on demand. Only one capture runs at a time: a queued photo waits for a
running pair, and the periodic capture and the debug buttons wait for a queued photo. `latest.txt` also carries `status_photo=`,
`status_hdr_format=` and `status_calibration=`; nothing in the `photos` folder is ever deleted by the mod.

## Files (`GW2Graphics\debug\`)

| file | what |
|---|---|
| `latest-mod.png`, `latest-vanilla.png`, `latest.txt` | the newest pair, stable paths (also `latest.png` = the mod shot) |
| `pair-<unix>-mod.png`, `pair-<unix>-vanilla.png`, `pair-<unix>.txt` | a pair; the newest 8 are kept |
| `shot-<unix>.png`, `shot-<unix>.txt` | a single mod shot (button, or `debug_compare=0`); the newest 12 are kept |

Only `shot-*` and `pair-*` files of this folder are ever deleted. Files are written to a temp name and renamed.

The PNG is 8-bit sRGB at HALF the back buffer size (about 1720x720 on a 3440x1440 screen, about 3.7 MB, uncompressed
zlib blocks). With HDR output (float16 scRGB back buffer) the value is `v / (paper_white / 80)` so paper white = 1.0,
clamped: what an SDR monitor would show, highlights clipped. Otherwise the 8-bit back buffer is used as it is.

The `.txt` holds the frame number, screen size, back buffer format, every effective setting (paper white, peak,
effective peak, bloom, highlight boost, AO, halo, reflections, shafts, fur, enabled) and the overlay's status lines
(Bloom, AO, Halo, Reflections, Shafts, `status_aa=`, `status_lens=` (see `lens.md`), `status_grade=` (see `grading.md`), `status_aniso=`, `status_exposure=`, `status_localtone=` (see `exposure.md`), `status_contact=` (see `contact-shadows.md`), `status_aerial=` (see `aerial.md`; it names the haze colour's source: `from the sky (horizon band)`, `held from the sky seen N frames ago` or `fallback`, with the colour and its confidence), `status_furlook=` (see `fur-look.md`), `status_shells=` (see `shell-fur.md`), `status_perf=`, `status_depth=` (see "The workbench" below), final pass, output rule), and the image statistics, computed on the float data
at full resolution BEFORE clipping, linear, 1.0 = paper white:

- `frac_any_channel_gt_1.0 / 2.0 / 4.0`: fraction of pixels with any channel above 1x, 2x, 4x paper white
- `p99_luminance`, `mean_luminance` (Rec.709 weights; p99 from a log histogram, about 2 % resolution)
- `mean_saturation_above_1.0`: mean HSV saturation of the pixels above 1.0 (near 1 = coloured neon, near 0 = white blow-out)

A pair `.txt` shows both sets side by side and the difference: mean absolute luminance difference and the fraction of
pixels differing by more than 10 % (display-encoded luminance, at half resolution).

## The vanilla shot

Shot A is taken with the mod on. Then the capture logic forces the user's `Enabled` switch off, exactly like the
overlay checkbox (`gate::set_user_enabled(false)` and `output::apply_user_levels(false, ..)`: substitutes reproduce
the original, neutral final pass, no AO/halo/reflections/shafts/fur/bloom fix, SDR white level), waits until the
pipeline is rebuilt (at least 6 presents and 250 ms, and until every re-installed substitute exists; at most 1.5 s),
takes shot B and restores the user's values.

- It is the mod's own **vanilla emulation** (what you see with Enabled unchecked), not a pixel-identical original.
- **The screen flickers** to the vanilla look for roughly 0.3 s during a pair (longer the first time, while the
  vanilla shaders compile; they are cached after that).
- Restore guarantees: the saved values (enabled, paper white, peak) are put back on every exit path: after shot B, on
  an abort/timeout, on any error, on a panic in the capture code (`catch_unwind` + `ForceGuard` drop), and by a
  watchdog thread 2 s after the force even if the render loop stopped. If the mod was already off, no pair is made.

## How the developer uses it

1. Set `debug_capture_seconds=20`, `debug_hot_reload=1` once and play.
2. The developer opens `GW2Graphics\debug\latest-mod.png` and `latest-vanilla.png` (and `latest.txt` for the numbers:
   blow-out fractions, p99, saturation, and the exact settings the shot was taken with).
3. The developer edits `config.ini` (bloom_strength, highlight_boost, ao_*, halo_*, ssr_*, shafts_*, aniso_level, mip_bias, smaa, smaa_threshold, sharpen_strength, fur_strength, fur_uv, fur_amp / fur_scale / fur_length / fur_detail / fur_rim / fur_sss / fur_view (see `fur-look.md`; status line `status_furlook=`), debug_view (see "The workbench" below), rim_strength / rim_width / subsurface_strength / rim_ps / rim_debug (see `rim.md`; status line `status_rim=`), fur_occlusion (see "`fur_occlusion` and `status_occlusion=`" below), fur_shell / fur_shells / shell_* (see `shell-fur.md`; status line `status_shells=`), ssr_water_boost, water_depth_look, ssr_matte_cut (`status_reflections=`: after `applied (...)` it reports the matte cut and a telemetry sample every 60 frames: floors, reflective pixels by class water/glass vs other, how many the score alone would reflect and the share the matte gate rejected; see `ssr.md` "Matte gate"),
   paper_white_nits, peak_nits, enabled). Within about 2-4 s the values are applied live; the next capture shows them,
   and `latest.txt` says which values were in effect.
4. `hdr_output` and `overlay_key` need a restart: a change is ignored with one log line. The log (`gw2graphics_log.txt`)
   says `debugshot: config.ini changed, applied live: ...`.

Hot reload applies only the keys that changed in the FILE since its previous version, so a slider moved in the game is
not reverted by an unrelated edit. The file is read at the second look with an unchanged modified time (a half-written
file is skipped), and not while a pair is running.

## The workbench: instruments for tuning in the running game

The keys and lines below exist so that the developer can calibrate an effect in the real game without a restart and without guessing. All of them are live
(hot reload) and none of the switches is ever saved.

### `debug_view` (0..5): the heat maps from outside

`debug_view=N` in `config.ini` is one more source of the debug level that the overlay's debug checkboxes feed (`resolve::debug_level()`; the highest active
source wins): 1 boost red / occlusion blue / halo green, 2 fill (left half red, top right black), 3 material classes (low blue, mid green, high red),
4 sun (sky cyan, shaft light yellow, magenta cross at the sun), 5 reflectivity heat map (blue 0 .. red 1). The overlay has a "Debug view" combo in the Debug
section for the same value. It is **never saved** (`Save` and `Config::to_ini` always write `debug_view=0`) and never applied at start: a stale value in the
file is inert until it is changed. (A `Save` in the overlay while the developer has a view on in the file writes 0 into the file, and the hot reload then applies
that: the view goes off. The same holds for `fur_view`.) `latest.txt` echoes `debug_view=` (the key's value) and `debug_level=` (the effective level, the highest of the sources).

### `fur_view` and the live fur look

Six fur parameters (`fur_amp`, `fur_scale`, `fur_length`, `fur_detail`, `fur_rim`, `fur_sss`, saved) and the diagnostic `fur_view` (0..4, never saved) are
documented in `fur-look.md`. `status_furlook=` has all six values and the view.

### `fur_occlusion` and `status_occlusion=`: the occlusion check of the fur / rim masks

`fur_occlusion` (0 / 1, default 1, saved, live) switches the occlusion check of BOTH masks (`draw-diagnostic.md`, "Fur: the character mask"): 1 = mask pixels covered
by geometry drawn later (a hood over the head) are erased, 0 = bypassed, the masks stay exactly as drawn (the A/B switch). `status_fur=` / `status_rim=` then end
with `occlusion: off (fur_occlusion=0)`. Every debug capture asks for ONE read-back of the masks, their z-stamps and the engine's depth, taken at the next
light-adjust draw BEFORE the check (one GPU stall, debug captures only); the CPU emulates the check and the NEXT capture's `latest.txt` has
(per channel that was drawn):

```
status_occlusion=fur_occlusion 1 (check on); fur: depth test LESS_EQUAL, depth R24G8_TYPELESS 3440x1440: mask a>0.5 N px, stamped 0..1 N px (N mask px unstamped = never checked), would erase N (x % of the mask; N by more than 1e-3; gap median g, max g), N mask px with the final depth farther than the stamp; tolerance t (OCC_TOL 1e-5 + depth bias: DepthBias d, DepthBiasClamp c, SlopeScaledDepthBias s on the last masked draw); N masked draws stamped[, N drew with the static fallback shader (...)]; rim: ...
```

Read it as: `would erase` a few % = the hood / later geometry case; tens of % on a fully visible character = the check is wrong for this scene (keep
`fur_occlusion=0` and report the line); `farther` = the engine's draw discarded the stamped fragment (alpha test), those pixels are kept.
`stamped 0..1 0 px` together with `N drew with the static fallback shader` = the probe hit the first frames after a loading screen, while the shaders
generated per vertex signature were still compiling (the fallback cannot stamp; nothing is wrong, the next capture shows the real numbers).
In-game captures showed `would erase 90..98 %` with `gap median 3.87e-6` (a constant 65 D24 steps) on a fully visible charr: the engine's
depth bias, not seen by the stamp on that GPU. `OCC_TOL` is 1e-5 since, plus the rasterizer's depth bias (`tolerance` in the line); check that
`would erase` is now a few % and that `DepthBias` reads non-zero (it confirms the cause).

### Shell fur keys and `status_shells=`

`fur_shell`, `fur_shells`, `shell_length`, `shell_density`, `shell_thickness`, `shell_root_dark`, `shell_wind`, `shell_lod_distance`, `shell_rim`, `shell_ps`,
`shell_vs`, `shell_min_vcb`, `shell_counts`, `shell_normal`, `shell_unit`, `shell_debug`, `shell_detail`, `shell_clump` (all saved, all live) are documented
in `shell-fur.md`; `latest.txt` lists their values and `status_shells=`: the settings, the unit (`unit auto: 39.4 per m (measured over N triangles, spread
x %, F frames ...)` = the single smoothed unit every triangle uses; `auto: measuring` only in the first frames after a load), how many draws drew shells last
frame, the CPU time of the replay, geometry shaders still compiling, the selector, and EVERY fur draw of the last frame with
`[vs ps vcb count inst srv -> shells | why not]`. Use those lines to set `shell_counts` (a count such as `17694` is taken, `-5214` is left out) or `shell_vs` so that only the real
fur (skin, head, limbs, tail) gets shells, never the cloth; `shell_debug=1` paints what got shells (red root .. yellow tip). Flicker check (v2): the unit in `status_shells=` must be the same number in consecutive captures of the same map (it only moves by more than 1.5 %); a fixed
`shell_unit=<that number>` is the A/B test if the fur still blinks.

Fur discriminator (v3: no fur on the horns, claws, nose, lips): `shell_fur_mask` (default `t0.a<`), `shell_fur_threshold` (0.66),
`shell_fur_soft` (0.05), `shell_uv_exclude` (empty), all saved and live. `status_shells=` ends with
`fur discriminator: fur where t0.alpha < 0.660 (soft +-0.050), N excluded uv rects [...]` and the probe of the capture before:
`discriminator probe: P px drawn by the fur draws, Q not fur (x %), S in the soft edge (y %); centre px (x, y): uv (u, v) fur weight w; mouse px ...: uv ...`.
Read it like this: x % should be small (horns, claws, nose: a few %); about 100 % = the channel is wrong for this texture (the whole coat
lost its fur: set `shell_fur_mask=off`, or try `t0.a>`, another channel or slot); 0 % = nothing removed (horns still fuzzy: lower the threshold, or
use rectangles). To place a rectangle: point the screen centre or the mouse at the horn / claw / nose, take a capture, read its uv, and add
`u0 v0 u1 v1` around it to `shell_uv_exclude` (`;` between rectangles, `@count` for one draw). `fur_debug=1` (magenta) now paints only what counts as
fur: the horns, claws and nose must stay unpainted.

Per-strand fur discriminator (v7; `shell-fur.md` "v7"): no tool, no config line. `status_shells=` (after the draw list) reads
`per-strand fur discriminator (v7, at each strand's own root uv; texel tests on: ...): [count N atlas <ptr>: C cells, gloss band B %, flat F,
coat gloss G sat S value V hue H -> fur draw | not fur (armour / skin / hair), no shells | no footprint, no shells | fur x w] ...`: one entry
per replayed draw, the GPU's coat statistics read back a few frames late (`no statistics yet` right after a load). Read it like this: the
charr's skin draws (head, body) must say `fur draw` with a gloss band of 65..95 % and a coat colour that matches the charr; armour, capes and
human NPCs `not fur`. A charr skin draw marked `not fur`: its band share (below 40 %) or flatness (below 0.03) is unusual: report the line
and the atlas PNG of the probe. The exclusion picker of v6 (overlay, F9) is removed; `shell_uv_exclude` still works by hand but is not needed.
A draw refused with `dynamic vertex / index buffer (could be rewritten before the replay)` gets no shells (the transparency guard).

### `status_depth=`: what the engine's depth looks like

The mod's effects read the engine's linear depth (`t4`). It turned out NORMALISED (1.0 = far plane, written for the sky): `depth_unit` is now metres per 1.0
(default 1000, derived from these lines; see aerial.md "Depth unit"). At every debug capture of the mod (single shot, and the mod half of a pair; not the vanilla half, not photos) the depth is read back and
ONE line is added to `latest.txt`:

```
status_depth=valid min a, p5 b, p50 c, p95 d, max e; sky/invalid f %; grid 5x3 (row-major, top-left first): v v v v v / v v v v v / v v v v v; centre column x=0.5 at y=0.9,0.7,0.5,0.3,0.1: v v v v v
```

* `valid` = finite and above 0; `min`, `p5`, `p50`, `p95` (nearest rank of the sorted valid depths) and `max` are over the valid depths only; numbers have 6 significant digits.
* `sky/invalid` = share of ALL pixels that are invalid or at least 0.98 x `max`.
* The grid is the RAW depth (invalid values as they are: `0`, `nan`, `inf`) at the centres of 5 x 3 equal cells; the centre column is the raw depth at x = 0.5 and
  y = 0.9 ... 0.1 measured from the TOP of the screen (0.9 is near the bottom: the ground in front of the character, 0.1 the sky or the far distance).
* Reading it: `max` about 1 and a sky cell of 1 means a normalised 0..1 depth, not inches; ground samples that grow smoothly from y = 0.9 to 0.5 give the scale
  (compare with the distance of the character's feet), then `depth_unit` can be derived (done: 1000 m per 1.0).

`status_highlight=` (also in the overlay, Bloom + highlight): the highlight slider, the gain the light-adjust shader really gets
(`datapath::effective_gain` of the display headroom), the gain of big bright surfaces (30 % share with the halo on), the headroom
with peak and paper white, and the expansion range (0.4 .. 0.9 of SDR white); `off (SDR output ...)` / `off (mod disabled)`.
* If there is no depth the line says why: `status_depth=unavailable (reason)`.

How it is taken: the capture asks the NEXT light-adjust draw for the depth (`ao::depth_resource()` is only valid during the frame; the overlay's frame tick has
released it by the time a shot is issued) and the shot waits one present for it. The read-back is a GPU stall of the render thread once per capture and the
percentiles are computed on the writer thread; none of it is in the normal frame path. A shot goes ahead without a depth after 12 presents. No effect's maths is
changed by it.

### `status_perf=`: frame times

`status_perf=avg 16.67 ms (60.0 fps), worst 1 % 24.10 ms, over 300 presents (CPU time between presents)`, also in the overlay (Debug > Status, "Frame time").
CPU timestamps (`Instant`, which is `QueryPerformanceCounter` on Windows) between presents over the last 300 presents: the average, the mean of the slowest 1 % of the
frames (at least one frame, so a 2 s stall shows up) and the average fps. It does not measure GPU time. Always recorded (two locks and a clock read per present).

## How it works (and limits)

- `debugshot::on_frame` runs in the overlay's render loop, which hudhook calls just BEFORE it draws the overlay, so
  `CopyResource` of swapchain buffer 0 into a ring of three staging textures never contains the overlay.
- Read-back is `Map(DO_NOT_WAIT)` on copies at least 2 presents old; a copy not finished is retried next present, one
  that never finishes is dropped after 240 presents. The render thread does one memcpy of the frame (about 40 MB at
  3440x1440 float16, a few ms) per capture; conversion, PNG and file writing run on a background thread.
- A capture is skipped, with a status message, for an MSAA back buffer or an unsupported format.
- The game's swapchain pointer is the one `output.rs` tracks; the back buffer is fetched per capture with `GetBuffer(0)`.
