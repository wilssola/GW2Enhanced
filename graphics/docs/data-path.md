# Data path: where the effects enter the picture

Status: implemented, unit and WARP tested (244 tests). Verified in the real game by the diagnostics below, see
"How to verify in the game".

## The rule

**Do not insert your own full-screen passes between the engine's passes and expect them to show. Put the effect
inside a substituted engine shader that the data flows through.**

## Why (measured in the real game)

M1 applied the highlight boost and the tone curve with an extra "resolve" pass drawn in place on the UI pass's
render target, and M2 multiplied the AO into the scene colour. Neither showed on screen, for any slider value,
while the shader substitutions (paper white in the final pass, bloom) did.

The mod's own GPU read-backs separated the causes (the `probe` lines of the log):

| step | result |
|---|---|
| our draw lands in the target | yes: the pixel read back right after the draw was the marker value (2, 0, 0) |
| the final pass samples that same texture | yes: same resource pointer (`0x...7e0`) |
| that texture, read just before the final pass draws | the scene again: `[0.274, 0.300, 0.148]`, not the marker |
| copies / render-target switches in between | none on the game's context |

So the engine draws the whole target again between the UI start and the final pass. A pass inserted into that
stretch is overwritten; where exactly the engine redraws it was not needed to know once the design changed.

## The design

```
 scene colour --> [ps f5cd4832 "light adjust"] --> combine --> SMAA --> copy --> UI --> [ps afdc14cb final] --> swapchain
                    ^ AO factor (t6) + boost                                                ^ tone shoulder,
                      (inputs bound at draw time)                                              paper white, peak
```

* `shaders/light_adjust_hdr.hlsl` replaces `ps#f5cd4832`. Every later pass reads its output, so anything done here
  cannot be bypassed.
  * AO: multiplies the encoded colour by `1 - 0.5 * saturate(strength * (1 - visibility))`, released on bright
    pixels (lights and effects are not ambient light).
  * Highlight boost (HDR output only): hue-preserving gain on the largest linear channel, rising smoothly from 1
    at 0.4 to the gain at 0.9 of SDR white and staying there, bounded so the largest channel never ends above
    max(itself, gain) (`tonemap::expand`, mirrored in the HLSL). The slider (1..4) is a FRACTION OF THE DISPLAY
    HEADROOM: gain = 1 + (peak/paper white - 1) * (slider - 1) / 3 (`datapath::effective_gain`). Slider 4 puts
    lights at SDR white on the display peak, and the peak setting matters because a higher peak (or lower paper
    white) gives the lights more room. With the halo on, big uniformly bright areas (not light sources) get
    `SURFACE_SHARE` = 60 % of the gain (30 % until the HDR audit, docs/hdr10.md). Until this change the range was 0.5 .. 1.0 and the surface share 20 %: the
    game's tone curve keeps nearly every pixel below that range (in-game capture at slider 4: p99 luminance 0.495, no
    pixel above 1.0), so the slider looked dead. `status_highlight=` (debug capture, overlay) shows slider, gain,
    surface gain, headroom, peak and paper white. WARP test: `light_adjust_highlight_slider_has_a_visible_calibrated_effect`
    (measured headroom 3.51: pixels at 0.73 linear go x1.9 at slider 2.5 and x2.8 at 4; surfaces x1.5 at 4; dark pixels unchanged).
  * SDR output: clamps like the original (`saturate`), no boost.
* `shaders/final_scrgb.hlsl` replaces `ps#afdc14cb` (scRGB output only): after the extended sRGB decode, a
  hue-preserving shoulder above the knee (`max(0.8 * peak/paper, 1)`) rolls the largest channel towards the
  headroom `peak/paper`, then paper white and the clamp at the peak. UI and ordinary scenes stay below the knee.
* The old in-place resolve pass is retired (`resolve::PASS_ENABLED = false`, pinned by a test). Its code and
  tests stay as the reference of the curve.

### Inputs the light-adjust shader gets (`src/datapath.rs`)

Two inputs the game never binds: `t6` (AO visibility, R16_FLOAT, scene size) and `b6` (one float4:
x boost gain, y AO strength, z SDR clamp, w debug). They are bound **at draw time**, in the `Draw*` hooks, only
while the substituted shader is the bound pixel shader (`datapath::kind`, set by the `PSSetShader` hook). Binding
earlier would not survive: the engine's own binding calls come before the draw. When the AO did not run this
frame, `t6` is a 1x1 "fully open" texture. **Unbound, everything reads zero, which means no boost, no AO, no SDR
clamp**: the exact behaviour the original-equivalence tests pin.

The same binding site also gives the shader the **fur mask** at `t20` (R16G16B16A16_UNORM: a = mask, rg = frac of the mesh uv, b = uv valid; screen size, `src/furmask.rs`; `t10`..`t18` are the
shafts' own passes, so `t20` is free) and `b6[1].z` / `b6[1].w` (fur strength / fur debug). `t20` is bound only when a draw was
masked this frame, otherwise it is set to null; unbound it reads zero = no fur, no debug paint. The mask is built from the draws
the selector picks (`fur_ps`, `fur_vs`, `fur_min_vcb`), see `draw-diagnostic.md`, "Fur: the character mask". Before binding,
`furmask::on_light_adjust` runs the masks' occlusion check once per frame (a full-screen pass into the mask with the z-stamp in `t0`
and the engine's depth in `t1`, both unbound again and the engine's `t0` / `t1` restored by `StateGuard`); the shader only ever reads
the cleaned mask.

### Auto exposure and local tone mapping (`t5`, `t21`, `b9`)

`exposure.rs` runs once per frame at the same binding site and gives the light-adjust shader `t5` (1x1 float4: x = EV applied),
`t21` (the base layer of the scene luminance at 1/16 resolution) and `b9` (x auto exposure on, y local tone exponent, z knee). They are
bound on every draw (null / zeros when off: the engine may leave something there). See `exposure.md`.

### Contact shadows and aerial perspective (`t22`, `b10`)

`contact.rs` computes a half-size contact visibility (R16_FLOAT) once per frame and binds it as `t22` with `b10` (x contact
strength, y aerial strength, z aerial half distance in m, w metres per depth unit) on every light-adjust draw (null / zeros when
both are off). The shader reads the linear depth `t4` and the fog state `t3` for the release and for the aerial perspective, so
`fog.rs` also runs when only these two are on. `t22` / `b10` unbound read zero = both off, bit-identical. See
`contact-shadows.md` and `aerial.md`. Used slots now: `t3`..`t9`, `t20`, `t21`, `t22`; `b6`..`b10`; `t10`..`t18` are the modules' scratch inputs. The rim light adds `t23` (the rim mask, R8_UNORM, screen size, `furmask.rs` second channel) and `b11` (rim strength, width, subsurface, debug; grazing constant): see `rim.md`. `t2` is used inside the SSR blur passes only (the water map, never visible to the engine).
The live fur look adds `b12` (two float4: fur_amp, fur_scale, fur_length, fur_detail / fur_rim, fur_sss, fur_view, and a last component 1 = "these values are live"; an unbound `b12` reads zero = the flag is 0 = the default look, no view): see `fur-look.md`.
The shell fur adds `t24` (R32_FLOAT holding packed keys: the highest strand fragment per pixel, its height and the offset to its root) and `b13` (on, root
darkening, back light, debug; unbound = zero = off, bit-identical). Both are bound by `shellfur::on_light_adjust` right after the fur mask's check, after it
drew the recorded fur draws once more as shells into the mod's own target (a mod-owned pass at the light-adjust draw, the pipeline state restored): see
`shell-fur.md`.

### Lens effects and colour grading (pre-UI copy, `t10`..`t16`)

`look.rs` runs at the draw of the pre-UI copy (`ps#c0ad1686`), right after the AA, with its own targets; its scratch inputs are `t10`..`t16`
(set and cleared inside its own passes, never visible to the engine). It is not a light-adjust input. See `lens.md` and `grading.md`.

### AO

`ao.rs` still computes the visibility at the start of the effects pass (depth from the engine's linear depth
target), but now only writes it to its own R16_FLOAT target (`Ao::run_visibility`: AO, horizontal blur, vertical
blur). Nothing is drawn into the scene any more.

## How to verify in the game

Overlay: `Data path: light-adjust inputs bound N times` must grow. Log: `datapath: light-adjust inputs bound (#N): boost ...`.

**Debug view** (overlay): pixels the boost changes turn red, occluded pixels turn blue.

**Debug fill** (overlay): the light-adjust shader paints the left half of the picture red and the top right black,
regardless of the scene. If the picture shows that, the shader is on the data path. The log also reads three
pixels of the final pass input (`probe final pass input, ...`): left half must be red, top right black, bottom
right the scene.

## Bloom stays the colour of the picture

`bloom_threshold` (`ps#14b1e90e` substitute) whitens bloom energy above the game's own range (1.0): the bloom the
combine adds is multiplied by a tint, and a strong coloured light would otherwise paint its surroundings in its own
colour (seen at bloom 1.7: white armour turned blue next to a cyan effect). The whitening depends on the energy
before the strength, so the output stays linear in `BLOOM_STRENGTH`; in-range inputs are bit-identical to the original.

## What this does not change

The float16 upgrade of the scene targets, the bloom substitutes, the gate and the swapchain handling are as in M1.

## v18: the black volumes of the jungle instance and the `hdr_upgrade` A/B

**Evidence.** The frame of that instance (the frame-pass log and the draw log of that frame): pass #10 draws into the UPGRADED
scene target (3440x1440, B8G8R8A8_UNORM -> R16G16B16A16_FLOAT by the mod) with ADDITIVE blending (`ADD:ONE:ONE`): 12 instanced box
volumes (`vs bbb96fb8 / ps 625d33af`, 384 indices each), and `ps 266402d5`, `ps 77418b51`, reading the scene colour. In D3D11 a UNORM
target clamps the pixel shader's output to 0..1 BEFORE the blend; a float target does not. A volume shader whose output goes negative
(a light / shadow term outside its intended range) adds 0 in the engine's own format but SUBTRACTS in the float target: a box-shaped
dark region with the volume's straight edges and dark, hard-edged regions where such volumes overlap the wreck; the
fog drawn afterwards cannot lift a negative value back. This is the leading hypothesis, consistent with every observation (it persists with
every effect off: the upgrade stays), not yet proven in the game.

**The A/B (v18).** `hdr_upgrade=0` in config.ini (or the overlay checkbox "Float scene upgrade", General; both apply at the next game
start) turns the upgrade off entirely: the engine's own formats, the classifier's gate never opens, no substitute and no effect runs:
the engine-identical path. The debug capture prints `hdr_upgrade=`. If the volumes are gone with it, the fix is to clamp the output of
those pass-#10 pixel shaders (a substitute of the three hashes with `max(output, 0)`; the substitution needs their bytecode / source).

**The fix (v18): the UNORM clamp emulation (`src/clampemu.rs`, `hdr_clamp=1`, default).** Generic, no shader list: every engine pixel
shader gets a TWIN, created next to it at `CreatePixelShader` (while the float upgrade is possible), whose float colour outputs
(`SV_Target0..7`) are written as `min(max(rgb, 0), 65504)` and `saturate(a)`: exactly what the engine's 8-bit target did to the
output before the blend (floor at 0, NaN -> 0, alpha 0..1) except that values ABOVE 1 are kept (the point of the upgrade). The
twin is a DXBC rewrite (writes to `o#` go to new temps; before every `ret` / `retc` the clamped temps are copied to the outputs;
the container hash is recomputed) and is registered under the original's hash, so every lookup by pointer sees the same shader.
Textures the mod upgrades are marked with private data; before an engine draw on the game context the twin is bound when every
bound colour target is marked, the original otherwise (native float targets, shadow maps, the back buffer, every mod shader and
substitute: untouched). One atomic swap per draw unless the shader or the targets changed.

Proven on WARP (`clampemu_tests.rs`): an additive volume outputting (-0.5, 2, NaN, 1.5) over 0.25 gives (-0.25, 2.25, NaN, 1.75) on
the float target (the bug: subtraction and a NaN), (0.25, 1, 0.25, 1) on the engine's 8-bit target, and (0.25, 2.25, 0.25, 1.25)
with the twin (the 8-bit result, HDR kept); an `INV_SRC_ALPHA` blend with alpha 1.6 subtracts the scene on float and not with the
twin; branches / early returns / two targets are all clamped and in-range values stay bit-exact; the checksum equals the
compiler's for every DXBC blob of the game exe (both hash tails); a wrong checksum is refused by the runtime; all 1011 pixel shaders
of the exe get a twin the runtime accepts. NOT proven in the game: that the pass-#10 shaders are the ones that output negative /
NaN / big alpha (the world shaders are not in the exe). The debug capture's `draws-fx-<unix>.log` now marks each group with `upg=`
(the upgraded colour targets) and `clamp=` (the twin was bound); `hdr_clamp=` in the debug capture prints the twin counters. A/B:
the overlay checkbox "8-bit clamp for engine shaders" (General, live).
