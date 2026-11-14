# Exposure and local tone mapping

Status: implemented, unit and WARP tested (26 tests, `src/exposure.rs` + `src/exposure_tests.rs`). NOT yet verified in the real
game: see "In-game checks".

Goal: the mod expands highlights (boost, bloom, bleach, shoulder) on an art style made for 8 bits, so sunny sand, snow and white
fur blow out and caves stay murky. Two gentle, optional effects on the 3D scene (the UI is never touched):

* **Auto exposure** (eye adaptation): the scene is measured every frame on the GPU and the exposure follows it slowly.
* **Local tone mapping**: big bright areas are compressed, edge-aware, without flattening their detail.

## Where it runs

Same place as the halo, the fog estimate and the AO inputs: right before the light-adjust draw (`datapath::bind_current` ->
`exposure::on_light_adjust`), from the scene colour bound at PS slot 0 of that draw. The measurement is taken BEFORE the exposure
is applied, so there is no feedback loop (the AA copy pass is after the light-adjust pass, so measuring there would see the
already exposed picture). The effect is applied inside the substituted light-adjust shader (`exposure_tone` in
`light_adjust_hdr.hlsl`), to the linear scene light before its remap, so the combine, the bloom, the AA, the UI composite and the
final shoulder all see it, and the UI is not scaled (the final pass shader also draws the UI, so it would have been the wrong
place).

Our passes (`shaders/exposure.hlsl`, one triangle each, only mod-owned targets, nothing read back):

1. `ps_lum`: scene (+ the engine's linear depth) -> L0 at 1/4: mean luminance of the valid pixels of each 4x4 block. A pixel is
   invalid (the sky) when the depth is <= 0, NaN, infinite or huge, the same rule as the AO/fog. Without a depth resource every
   pixel counts (logged once; `status_exposure` says "sky NOT excluded").
2. `ps_down_lin`, `ps_down_log`: L1 (1/8), L2 (1/16, the analysis level) in the log2 domain, weighted by the valid share.
3. Auto exposure: `ps_hist` (L2 -> 64 bins of log2 luminance, -10..+6 EV, centre weighted: the border counts 0.35, the centre
   1.0; built in 8 partial rows) and `ps_state`: sums the bins, **leaves out the brightest 0.1 % of the weight** (lamps, sun,
   glints), takes the log-average, computes the EV the scene asks for and smooths it (see below). The result is a 1x1 RGBA32F
   ping-pong texture.
4. Local tone: `ps_blur` twice (horizontal, vertical; 13 taps, step 2 texels) on L2: an edge-aware (bilateral) blur of the
   log luminance. The weight of a tap is gaussian in distance times gaussian in the log-luminance difference to the centre
   (sigma 0.5 EV), so a step of 2 EV or more is never averaged across. Result: the base layer B2 (x = blurred, y = raw,
   z = valid share).

Cost, 3440x1440: `ps_lum` dominates (310k texels x 16 scene taps + 16 depth taps); the rest runs on 215x90 texels or less.
Estimated 0.2-0.5 ms on a mid-range GPU with both features on, about 0.15-0.3 ms with only auto exposure (no blur passes). Not
measured in the game. With both strengths at 0 nothing runs except one 32-byte constant-buffer write and three binds per frame.

## Auto exposure

```
raw   = log2(autoexp_target) - log2(measured log-average luminance)      (EV the scene asks for)
x     = raw * autoexp_strength
ev    = limit(x)   // identity up to 60 % of the range, then a tanh roll-off that reaches the range at infinity
                   // darkening: range = autoexp_range; brightening: min(autoexp_range, 1.0)   (hard cap +1 EV)
smoothed: ev += (ev_target - ev) * (1 - exp(-dt / tau)),  tau = adapt_up if the scene got brighter (the EV falls),
                                                           adapt_down if it got darker (the EV rises)
```

* `adapt_up` (default 0.6 s) and `adapt_down` (default 1.8 s) are time constants (about 95 % of a change after three times the
  value): the eye adapts to bright light quickly and to the dark slowly.
* `dt` is the CPU frame time between runs, clamped to 0.25 s. A gap longer than 1 s (loading screen, pause) or the first frame
  jumps straight to the target.
* Nothing to measure (all sky or black): the state holds.
* Defaults: strength 0.5, target 0.20 (linear, 1 = SDR white), range 0.7 EV. So a sunny-sand scene (log-average 0.5) asks for
  -1.3 EV, x = -0.65, limited to about -0.44 EV; a cave (0.06) asks +1.7, x = +0.85, limited to about +0.6 EV.

Applied in the shader (`exposed_max`, mirrored by `exposure::exposed_max`), hue-preserving (one factor on all channels, from the
largest channel `m`):

* darkening (EV < 0): `m * 2^ev`;
* brightening: `m + (E - 1) * m / (1 + (m/0.5)^4)^(1/4)`: about `m * E` for dark pixels, but the extra saturates at
  `(E - 1) * 0.5`, so highlights and lamps are NOT pushed further up (the shoulder and the boost then work as usual) and the curve
  is monotonic. Dark scenes get a lift in the mid and low tones only; the brightening is capped at +1 EV whatever the config says,
  and by default about +0.6 EV at most. This is a look, not a visibility aid.

## Local tone mapping

The pixel is scaled by one factor `s = 2^(-k * g(x))`, `x = log2(base * E / knee)` (the base layer of the neighbourhood after
the exposure), `g(x) = 0` below -0.5 EV, `x` above +0.5 EV and a C1 quadratic in between (so a flat area below the knee is exactly
unchanged), `k = localtone_strength * 0.6`, `knee = 0.6` (linear, 1 = SDR white). The detail layer (pixel / base) is untouched,
so texture contrast is kept, only the base layer is compressed. At strength 0.4 (k = 0.24; the default is 0.2 since the HDR audit, k = 0.12) white sand at luminance
1.0 loses about 0.18 EV; at strength 1 about 0.44 EV.

* **No halo**: the base layer is edge-aware (above), and the shader upsamples it with a joint-bilateral filter: the 4 taps are
  weighted by bilinear weight x valid share x similarity of the tap's RAW log luminance to this pixel's own luminance. The test
  with a step edge checks that the dark side 1..24 px from the edge is not touched and that the bright side is compressed by the
  same factor as far from the edge.
* **Small lights keep their energy**: a lamp smaller than a base cell (16 px at full resolution) does not appear in the base layer
  (it is averaged away and rejected by the range weights), so it is not compressed. Where the halo is on, its local mean also
  fades the compression out on light sources (`source_share`, same rule as the boost); with the halo off, large lamps (> 16 px)
  are compressed like any bright area.
* The sky (invalid depth) has no valid base and is not touched.
* Hue-preserving, only ever compresses (`s <= 1`), bounded.

## SDR output and the vanilla emulation

* SDR (`hdr_output` off / clamp): both effects work; the measurement caps the input at 1.0 and the shader never lets a pixel end
  above `max(input, 1)`, so there is no exceeding of the 1.0 headroom (brightening an already white pixel does nothing).
* Mod disabled (`enabled=0`, vanilla emulation) or the classifier gate closed: `exposure::enabled()` is false, no pass runs and
  `b9` is bound as zeros: bit-identical to the original. Strength 0 does the same per feature.

## Config keys (hot-reloadable with the debug mode, overlay section "Exposure & tone")

| key | default | range | meaning |
|---|---|---|---|
| `autoexp_strength` | 0.5 | 0..1 | slope of the compensation; 0 = off, bit-identical |
| `autoexp_target` | 0.20 | 0.05..0.6 | linear log-average luminance the exposure aims at (1 = SDR white) |
| `autoexp_range` | 0.7 | 0..2 EV | biggest change either way; brightening also capped at +1 EV |
| `adapt_up` | 0.6 | 0.05..20 s | time constant when the scene gets brighter |
| `adapt_down` | 1.8 | 0.05..20 s | time constant when the scene gets darker |
| `localtone_strength` | 0.2 (was 0.4: it dimmed white fur / armour by about 11 % before the highlight boost, docs/hdr10.md "Audit") | 0..1 | local compression; 0 = off, bit-identical |

## Slots (see data-path.md)

Light-adjust substitute: `t5` = the 1x1 state (x = EV applied), `t21` = the base layer (1/16 scene), `b9` = (x auto exposure on,
y local exponent k, z knee). The module's own passes use `t10..t12` (cleared afterwards) and `b0` (restored). Always bound at
draw time, zero/null when off, because the engine may leave something in those slots.

## Debug capture

`latest.txt` has the settings and `status_exposure=` (applied or why not; the EV applied, the EV the scene asks for, the measured
log-average luminance, the target) and `status_localtone=`. The EV is read back from the 1x1 state texture only while a dev
capture is built (a small GPU stall, once per capture); the overlay shows the last read value.

## In-game checks (not done)

1. Bright sand / snow scene: with `localtone_strength` 0.4 and `autoexp_strength` 0.5 the sand should show more texture and not
   clip flat; compare 0 / 0.4 / 1 with the debug capture; look at the edge of a bright wall against shade for halos.
2. Cave / dark interior: a gentle lift (about +0.4..0.6 EV at most) of the low and mid tones, lamps unchanged.
3. Walking from sun into shade and back: the exposure follows in about 2-5 s going dark and 1-2 s going bright, no pumping when a
   character or the camera crosses a bright patch (the top 0.1 % and the sky are ignored; if pumping appears, raise `adapt_*`).
4. `status_exposure=` should read "applied (HDR, sky excluded)" and the EV should move with the scene; if it says "sky NOT
   excluded (no depth)" the depth capture did not run.
5. SDR mode and `enabled=0` look as before / exactly vanilla.

## Known limits

* The measurement sees the scene colour before the engine's own remap (`light_adjust`'s dark/mid/bright weights) and before
  the bloom; very bloomy scenes get bloom added after the exposure.
* Large lamps (> 16 px) with the halo off are compressed like bright surfaces.
* The 1/16 base layer cannot follow detail below about 16 px; that detail is kept (it is the detail layer).
* WARP tests only; the cost estimate is by arithmetic, not measured.
