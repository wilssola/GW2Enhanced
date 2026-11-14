# HDR10 output (experimental) and HDR calibration

`hdr_format=scrgb` (default) is the long-tested path: a float16 scRGB swapchain (linear, 1.0 = 80 nits). `hdr_format=hdr10`
creates an `R10G10B10A2_UNORM` swapchain in the colour space `RGB_FULL_G2084_NONE_P2020` (HDR10: PQ, BT.2020) instead.
**It is off by default, experimental, and only used when it is safe.** Nothing else changes: the scene, bloom, tone shoulder
and the peak clamp are identical; only the last conversion differs.

## The rule

HDR10 is used only when all of these hold (`hdr10::decide_format`, pure and tested; the reason is appended to the rule line, i.e.
`status_rule=` and the overlay's "Rule" / "Swapchain format" lines):

| condition | otherwise |
|---|---|
| `hdr_format=hdr10` | scRGB, `hdr_format=scrgb` |
| HDR output is active (the existing rule: Windows HDR on, `hdr_output` not off, swapchain unchanged...) | `hdr_format=hdr10 ignored: HDR output is not active` |
| **ReShade is NOT present** - not even with `hdr_output=on` | `hdr_format=hdr10 ignored: ReShade is present (HDR10 is never forced with it)` |
| the previous start with HDR10 reached a stable picture (no `hdr10.pending` marker) | `...ignored: the previous start with HDR10 did not reach a stable picture; delete GW2Graphics\hdr10.pending to try again` |
| the HDR10 final shader compiles and the swapchain can be created / the colour space is supported | one retry as scRGB (`hdr_format=hdr10 failed in this session, scRGB used instead`), then the old SDR fallback |

Read at start (the swapchain is created once). A change in `config.ini` while the game runs is ignored with one log line.

## How it is made, and the risk

- **The format is chosen when the game creates its swapchain, never changed afterwards.** `output::create_swapchain` and the
  device-and-swapchain path create a flip-discard copy of the game's desc as R10G10B10A2, check `CheckColorSpaceSupport(12)`,
  select the colour space and hook `ResizeBuffers`, all before the game sees the swapchain. The same retry chain as scRGB applies
  (HDR10 -> scRGB -> the game's own swapchain), each step releasing everything of the failed attempt first.
- **Why not switch an existing swapchain with `ResizeBuffers`?** The game keeps the swapchain object and holds views of its
  buffers; changing the buffers' format and colour space under it is not something that can be made safe from a hook, and a
  failure would be a black screen. So there is no live switch (`hdr_format` needs a restart). While the game runs, `ResizeBuffers`
  on the mod's swapchain keeps its own format whatever the game passes (`hdr10::resize_args_for`: UNKNOWN stays, anything else
  becomes the swapchain's format).
- The game's back-buffer views are created as `R8G8B8A8_UNORM`; for an HDR10 swapchain they are rewritten to R10G10B10A2
  (`upgrade::view_format`), the same mechanism as the float16 rewrite.
- **Black screen / crash guard.** Whether a present "fails" is not visible from the hook (the overlay library owns the `Present`
  hook), so the guard works across starts: when an HDR10 swapchain is created, `GW2Graphics\hdr10.pending` is written; it is
  removed after 600 presents and 15 s. If the game dies, hangs or the driver resets before that, the file is still there at the
  next start and HDR10 stays off (scRGB is used, the rule line says why) until you delete the file. A swapchain that presents
  fine but looks wrong (black, wrong colours) is NOT detected: set `hdr_format=scrgb` in `config.ini` (or delete the key). Quitting
  within 15 s of the swapchain's creation also leaves the file (delete it to try again).
- **Overlay under HDR10.** hudhook draws ImGui after the final pass straight into the back buffer: its colours would be PQ code
  values (white = 10000 nits). The overlay maps its style colours and labels with `hdr10::ui_color` (white UI = paper white) and
  does not draw ImGui's own white mouse cursor. Colours the overlay code sets explicitly go through the same mapping; any ImGui
  widget colour not covered would be too bright - report it. The scRGB overlay is unchanged (dim: 80 nits at most).
- Tested on WARP: the shader against the host reference over a log range, the PQ round trip, the matrices, the rules, the
  fallback chain, the resize arguments, the view rewrite. **Not testable without a real HDR10 swapchain on real hardware**: that
  `CheckColorSpaceSupport(12)` and `SetColorSpace1` succeed on the game's output, flip-model presentation of R10G10B10A2, and the
  look. This needs the in-game check below.

## The encoding (`shaders/final_scrgb.hlsl`, `HDR10_OUTPUT 1`)

`final_hlsl_with(.., FinalOpts { hdr10: true, .. })` prepends `#define HDR10_OUTPUT 1`. After the usual path (decode, energy
bleach, tone shoulder, x paper white / 80, clamp at the peak) the result v80 (scRGB unit, linear BT.709) is converted:
`nits = max(M709to2020 * v80, 0) * 80`, `code = PQ(nits / 10000)`, plus a **1-LSB (1/1023) triangular dither** from a per-pixel
integer hash (static, deterministic; `DITHER 0` removes it for the tests). Alpha is 1. Host reference: `hdr10::pq_encode`,
`hdr10::BT709_TO_2020`, `hdr10::scrgb_to_pq`; the test `final_hdr10_matches_the_host_pq_reference_over_a_log_range` holds the shader to
1e-3 (the WARP target is float16, whose own rounding is about 5e-4 near 1.0).

Captures (debug and photo) decode R10G10B10A2 back to scRGB units with a LUT of the PQ decode and the inverse matrix.

## Audit: washed-out whites and unused headroom

Test setup: `hdr_format=hdr10`, paper white 400, peak 3546 (effective 1405 = the display's `MaxLuminance` from
`IDXGIOutput6::GetDesc1`; the setting is capped to it, which is right: above its own peak the display clips or tone-maps each
channel itself). Tests: `src/hdr10_audit_tests.rs` (WARP, the output is decoded back to nits: inverse PQ + BT.2020 -> BT.709).

What was checked and is CORRECT: PQ constants (ST 2084 m1 m2 c1 c2 c3) and the input in absolute nits / 10000 (scRGB 1.0 = 80
nits, `nits = v * 80`); paper white identical in both encodings (encoded 1.0 = paper white nits, decoded); BT.709 -> BT.2020
(BT.2087) applied in LINEAR light before PQ, negatives clamped, no second gamma; swapchain R10G10B10A2_UNORM + `SetColorSpace1(12)`
after `CheckColorSpaceSupport`, back-buffer views rewritten to R10G10B10A2 (no sRGB view); 1-LSB triangular dither before the 10-bit
quantisation; scRGB and HDR10 agree within 1.2 % (also after a real 10-bit quantisation); black = 0 nits; a 4x highlight = 4x paper
white below the knee, rolled off under the peak above it; no NaN with junk input.

What made it look washed (a debug capture of a forest at day):

1. **Lifted blacks.** The final pass decoded the SDR range with the piecewise sRGB curve (what Windows does with SDR content). The game
   was made for SDR monitors that decode about gamma 2.2; the sRGB linear toe is 3-20x brighter near black (encoded 0.05: 1.58 nits
   instead of 0.55 at paper white 400; 0.02: 0.62 instead of 0.07). Fix: `sdr_gamma` (default 2.2, `srgb` = the old curve), see below.
2. **No headroom used by white surfaces.** `frac_any_channel_gt_2.0 = 0`, p99 luminance 0.54 of paper white, white fur in the
   capture peaked at 389 nits = 0.97 x paper white (from the capture PNG): the scene never went above SDR white except small
   saturated effects. Big bright surfaces got only 30 % of the highlight boost and the local tone mapping (0.4) compressed them by about
   11 % first. Fix: `datapath::SURFACE_SHARE` 0.3 -> 0.6, `localtone_strength` default 0.4 -> 0.2 (exposure.md). WARP: a sunlit white
   surface now reaches about 610 nits (1.5x paper white) at a slider of 2, about 725 at the default 2.5.
3. **Auto exposure pinned at its cap.** The capture measures a log-average of 0.015 against the target 0.20: the exposure sits at
   +0.70 EV in every frame of that session (it is multiplicative: black stays black, mid tones x1.62, whites +0.31 at most), i.e. a
   constant flattening. Not changed by default (no other scenes measured); see the tuning hints.
4. **Paper white 400 with a 1405-nit display** leaves only 1.8 EV of headroom: the whole picture lives near the top. 200-250 nits gives
   2.5-2.8 EV for highlights (more "pop") with darker UI.
5. At capture time `aerial_strength=1` (35 % cap of haze colour at about 200 m) was set from the overlay: a visible veil on far terrain.

### Black level: `sdr_gamma`

`final_scrgb.hlsl` (`SDR_GAMMA`, prepended by `output::final_hlsl_with` as `gamma_centi`): channels <= 1.0 are decoded with
`pow(v, sdr_gamma)`, channels above 1.0 with the extended sRGB curve as before (C0 at 1.0; slopes 2.2 vs 2.28), so white, the
highlights, the shoulder and the peak do not change and mid tones move by under 4 %. Applies to scRGB and HDR10 alike, live (hot
reload, overlay slider next to the peak). While the mod is switched off (vanilla comparison) the piecewise sRGB curve is used, like
Windows. Range 1.8..2.8, `srgb` (or 0) = the old curve. The photo CPU mirror (`photo::final_pass_cpu_g`) follows it.

## Calibration (overlay section "HDR calibration")

Why the patterns are made by the mod's own last pass and not by the overlay: the overlay (hudhook) is drawn AFTER the final pass
with ImGui colours that stop at 1.0 (80 nits in scRGB; PQ code values in HDR10), so a 1000 or 4000 nit block cannot be drawn
there. Instead the final-pass substitute is rebuilt with `CALIB_PATTERN n` (a lazy re-install, like a paper white change), which
makes it output a test pattern in absolute nits in the picture's own encoding (scRGB or PQ); the overlay only adds the labels on
top (`calib::label_pos`). The flag `calibration_pattern` is runtime only (never saved, never applied at start, hot reload may set it).

| pattern | what | what you do |
|---|---|---|
| 1 peak | eight blocks, 400 / 600 / 800 / 1000 / 1400 / 2000 / 3000 / 4000 nits on black, NOT clamped by the configured peak | raise the **Peak** slider until the last block you can still tell apart from its neighbour matches the peak; the display's reported MaxLuminance (DXGI) is shown, with **Use display value** |
| 2 paper white | three swatches: white UI = exactly paper white, an 18 % grey card, 5 % dark grey | set **Paper white** so the white swatch is comfortable and the grey card is a mid grey |
| 3 black level | eight patches 0.005 .. 1 nit on black | say which is the dimmest you can see: read-only guidance, there is no setting (the mod never lifts blacks) |

Safety (`calib::step`, tested): the pattern ends when set to 0, when the overlay closes (only a pattern the overlay started),
when HDR output is not active, and after **30 s**. It needs HDR output (scRGB or HDR10); in SDR it is unavailable and says so.
Because the patterns replace the picture, a screenshot taken meanwhile shows the pattern.

## Check in the game (HDR10, risky: keep a way to edit config.ini)

1. With Windows HDR on and no ReShade: `hdr_format=hdr10`, restart. The log has `output: HDR10 [experimental] ...`; the overlay's
   "Swapchain format" says HDR10 active. The picture should match scRGB closely (slightly different clipping at the gamut edge).
2. If the picture is black or wrong: set `hdr_format=scrgb`; if the game crashed, delete `GW2Graphics\hdr10.pending` before retrying.
3. Overlay readable (white = paper white), no white cursor; Photo mode saves a normal-looking PNG and a `.hdr`.
4. Run the three calibration patterns in scRGB first (the default), then in HDR10.
