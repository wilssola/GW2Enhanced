# GW2 engine post-processing chain (Task 4)

Source: offline disassembly of the pixel shaders embedded in `Gw2-64.exe`
with `graphics/tools/dxbc-dump/dxbc_dump.py` (`disasm <exe> <hash>...`). Pass numbers come from
[frame-structure.md](frame-structure.md) (fixture `runtime/src/frame_fixture.rs`, 3440x1440). Shader hashes are the FNV-1a 32 of the DXBC blob,
equal to the `ps#xxxxxxxx` hashes in the fixture (it was captured with a diagnostic that wrote
shader lines; the runtime's own `Dump frame` writes pass structure only, no `ps#` lines).

Limits that apply to everything below:
- The embedded blobs carry no RDEF chunk (only 2 of 2209 do), so constant-buffer variable NAMES
  (BloomAmount, HdrOffset...) are not recoverable. Constants are given as `cb0[i].c` slots, with the
  role inferred from the math. Texture/sampler slots (`t#`/`s#`) are exact.
- Which texture is bound to each slot is inferred from the pass structure and the math, not observed.
- The pixel shaders of passes #0, #2, #3 and #9 are NOT in the exe (0 of the 8 hashes the log lists for
  each; the log lists only the first 8 per pass). They are generated or loaded at run time. Effects
  pass #9 was therefore NOT disassembled.
- Blend state, write masks and the exact draw order inside a pass are not in the shaders and are not known.

## Current target formats (state today, NOT decision evidence)

Today every colour render target in the chain except the 1x1 luminance is `B8G8R8A8_UNORM`
(#3, #8, #9, #10/#11, #13, #14, #15, #16, #17, #18). That clamps to [0,1] on write, but the next task
upgrades the scene colour targets to float16, so this describes only the current 8-bit state and is
not used as evidence for the decision. The decision rests on what the shaders do (see Decision).

## Pass by pass

### #10/#11 bright-pass downsample, ps `2ed4a75f` (860x360, then 215x215)
- t0/s0, CB0[1]. 4 taps averaged (0.25 each), `add_sat o0.xyz = avg - cb0[0].xyz`, `mov_sat o0.w`.
- cb0[0].xyz is a subtracted threshold/offset (likely the "HdrOffset"-like value). Saturated.
- The input is the LDR scene, so the threshold cuts within 0..1 today.

### #12 luminance / adaptation, ps `fbea4f58` (1x1 R16G16B16A16_FLOAT)
- t0/s0 (the 215x215 downsample), t1/s1 (previous 1x1 result, sampled at 0,0), CB0[6].
- 11x11 = 121 fixed taps over t0 (x0[] = 0.0..1.0 step 0.1), sum * 0.008264 (=1/121).
- `dp3_sat` with (0.2126, 0.7152, 0.0722): average luminance, saturated to 0..1.
- Temporal adaptation: if prev < cur, `min(cur, prev + cb0[1].x)`; else `max(cur, prev - cb0[0].x)`
  (speed up / speed down).
- Outputs: x = adapted luminance; y = `div_sat((cb0[3].x - L) / (cb0[3].x - cb0[2].x))` (dark weight);
  z = `div_sat((L - max(cb0[5].x,0.2)) / (max(cb0[5].x,0.2) - cb0[4].x))` (bright weight);
  w = `max(1 - y - z, 0)` (mid weight). cb0[2..5].x are the dark/bright range limits.
- Consumed by `f5cd4832` (t1). In the ORIGINAL chain the luminance can never exceed 1.0 (saturated, fed by LDR).
  That no longer holds with the mod: the unclamped bright pass (`2ed4a75f` replacement) feeds #12 an average that
  includes energy above 1.0 (the `dp3_sat` still caps the stored value at 1.0 but the average it sees is brighter),
  so exposure weights can differ from the original in bright scenes. A look interaction to check in bright scenes.

### #13 light adjustment (3 draws, order unknown)
- `4340ca88` (CB0[2], t0/s0): reads the ALPHA of t0. `a > 0.491961` -> out.a = 1.0; `0.240980 < a <= 0.491961`
  -> out.a = 0.0; `a <= 0.24098` -> out.a = 0.5; out.rgb = 0. This re-encodes a 3-class material mask into
  alpha (0.5 = neutral; 0 and 1 = the two other classes), later read by `37c5610f` as `a*2-1`. Whether it only
  writes alpha (colour write mask) is not determinable from the shader.
- `56875aa2` (CB0[3], t0 colour/s0, t1 one channel/s1): DOF-like blur. t1 is a circle-of-confusion value:
  `sat((coc*cb0[2].w - cb0[1].x) / cb0[1].y)` scales the radius (cb0[2].xy = texel size); 8 taps averaged
  (`* 0.125`). No clamp in the shader, no colour-space conversion (averages stored values as they are).
- `f5cd4832` (CB0[2], t0 colour/s0, t1 = the 1x1 luminance/s1): tone/exposure remap driven by the #12 weights.
  - Decodes colour with `r*r` (gamma 2.0 approximation, not the real sRGB curve), `Y = dp3(0.2126,0.7152,0.0722)`.
  - New luminance `Y' = sat(Y*w_mid + pow(Y, max(cb0[1].x,1e-5))*cb0[0].x*w_dark + min(Y, (4Y)^2/6)*w_bright)`
    (pow via `log/exp`). cb0[0].x scales the dark curve, cb0[1].x is its exponent.
  - Keeps chromaticity: goes through XYZ (Rec709 matrices 0.4124/0.3576/0.1805... and 3.2405/-1.5371/-0.4985...),
    rescales to Y', converts back to linear RGB, then `sqrt` (gamma 2.0 encode).
  - The XYZ-to-RGB step can give a negative value before `sqrt`; the NaN is mapped to 0 by `mad_sat`.
  - Adds a 1/255 dither (`frc` hash of the pixel position) and ends with `mad_sat o0.xyz`, `mov_sat o0.w`.
  - Summary: input gamma-encoded (~2.0), math in linear light, output gamma-encoded, HARD saturated.
    The only luminance compression is the `min(Y, (4Y)^2/6)` toe on the bright weight, which only lowers values.

### #14 bloom blur (860x360)
- `14b1e90e` x1 (CB0[1], t0/s0, 4 taps from TEXCOORD pairs): average of 4 taps (0.25 each);
  `Y = dp3(0.2125,0.7154,0.0721)`; `mad_sat o0.xyz = avg*Y - cb0[0].xyz`; `mov_sat o0.w`.
  This is the bloom threshold: colour weighted by its own luminance (quadratic), minus an offset
  cb0[0].xyz, saturated. Works on gamma-encoded LDR values.
- `b73c3be3` x4 (CB0[1], t0/s0): 4-tap Gaussian-like blur, weights 0.487427 / 0.147914 / 0.328026 /
  0.036633 (sum 1.0), `o0.xyz = sum * cb0[0].w` (per-draw bloom gain), `o0.w = sum.w`. No `_sat`, but the
  target is UNORM. Presumably 2 horizontal + 2 vertical draws ping-ponging; not verified.

### #15 combine, ps `37c5610f` (CB0[9], t0 = scene after #13, t1 = bloom from #14, s0/s1)
- v0.xy = scene UV, v0.zw = bloom UV (bloom is bilinear-upsampled through s1).
- Saturation: `rgb' = lerp(Y, rgb, cb0[5].w)` (PostProcSaturation-like; Y = 0.2125/0.7154/0.0721 on encoded values).
- Hue-targeted tint: `d = mean(|rgb/mean(rgb) - cb0[6].xyz|)`, `m = pow(1 - min(d,1), cb0[6].w) * cb0[7].w`,
  `rgb' = lerp(rgb', cb0[7].xyz * Y, m)`.
- Bloom: `rgb' += bloom.rgb * cb0[0].xyz * 2.0` (cb0[0].xyz = bloom colour/amount).
- Per-class grade driven by alpha `a*2-1` (the mask from `4340ca88`): `< -0.01` -> `rgb' * cb0[4].xyz`;
  `> 0.01` -> `rgb' * cb0[2].xyz`; otherwise a 4-tap alpha edge test (offset cb0[8].xy * cb0[8].z * 1.2) and
  lerp towards `cb0[3].xyz` (weight cb0[3].w) when the sum of 4 alphas is below 1.96, or towards `cb0[1].xyz`
  (weight cb0[1].w) when above 2.04. `o0.w = 0`.
- No `_sat`, no min/max on the colour, no pow/gamma conversion on the colour, no Reinhard-like curve.
  The only `min` is on the tint distance. The shader is additive and passes floats through unclamped (only today's UNORM target clips it).
  Colour space: gamma-encoded in and out.

### #16 anti-aliasing (SMAA, 3 draws; in-pass order unknown)
- `833ba6e3` (t0/s0): colour edge detection (max abs RGB delta to 4 neighbours, local-contrast factor 0.5,
  threshold 0.1, `discard` when no edge). Output = edges in xy.
- `342c9cb4` (CB0[1], t0 edges, t1 area table 160x560, t2 search table): SMAA blend-weight calculation.
- `dbd1a9c1` (CB0[1], t0 colour, t1 weights): neighbourhood blending, texture reads only, no clamp.
- Edges are detected on the gamma-encoded LDR colour; no colour-space math.

### #17 final copy, ps `c0ad1686` (t0/s0)
- `sample` then `mov o0 = tex`. No math. The last engine colour pass before the UI.

### #18 UI
- 301 draws, shaders such as `8546e280` (x179), `7949b6ba`, `36e6e263`. Not analysed.

### #8 light accumulation (context), ps `625d33af` (CB0[22], t0 normals, t1 depth)
- Rebuilds world position from depth, point-light attenuation with a cone/shadow test, output
  `light colour (cb0[21].xyz) * atten * cb0[0].x`, `a = spec * 0.5`. No `_sat`. Target is `B8G8R8A8_UNORM`.
  Whether it is blended additively is not in the shader.

### #9 effects / transparents (NOT analysed)
- The 8 listed hashes are not embedded (0 of 8), so no shader could be read. What can be said without them:
  nothing about shader clamping or blend factors (premultiplied or not, additive or alpha) is known.
  Today's UNORM target clips results, which is a format effect, not a shader fact.

## Final output stage before UI

Order: `#15 37c5610f` combine -> `#16 833ba6e3, 342c9cb4, dbd1a9c1` SMAA -> `#17 c0ad1686` copy -> `#18` UI.
The last engine colour pass before the UI is **#17, ps `c0ad1686`** (a pure copy of the SMAA result; hash
verified in the exe). The resolve runs at the start of the UI pass, i.e. right after the #17 copy has finished
(found structurally at bind time, not by pass index; see `recorder::is_ui_start`). The tone
stages proper are `f5cd4832` (#13) and `37c5610f` (#15); #16 and #17 are anti-aliasing and a copy.

## Decision

Decision: B

Scope: B for the post chain (#13 tone remap and the bloom threshold feed). Scene and effects stages
(#0, #2, #3, #9) are unknown.

Per-pass colour clamping (shader facts only):

| Pass | Shader | Clamps colour | Instruction evidence |
|---|---|---|---|
| #10/#11 | `2ed4a75f` | yes | `add_sat o0.xyz, avg, -cb0[0].xyz` |
| #12 | `fbea4f58` | stat only | `dp3_sat r0.x` caps the average luminance used for exposure weights; no scene colour written |
| #13 | `f5cd4832` | yes | `mad_sat r2.y` on remapped luminance Y', then `mad_sat o0.xyz` after `mul r1.xyz,r1,r1` decode / `sqrt` encode |
| #13 | `56875aa2`, `4340ca88` | no | plain averaging / alpha re-encode |
| #14 | `14b1e90e` | yes | `mad_sat o0.xyz = avg*Y - cb0[0].xyz` |
| #14 | `b73c3be3` | no | weighted sum times `cb0[0].w`, no `_sat` |
| #15 | `37c5610f` | no | no `_sat`/min/max on colour; additive bloom, saturation, tint |
| #16 | `833ba6e3`, `342c9cb4`, `dbd1a9c1` | no | edge/weights/blend texture reads |
| #17 | `c0ad1686` | no | `mov o0, tex` |

Justification: #13 `f5cd4832` clamps the colour (and tone-compresses via the gamma-2.0 remap), and the
bloom threshold stages #10/#11 and #14 clamp the bloom feed. Highlights above 1.0 would be lost there
even with float16 targets.

What the resolve (M1) replaces or bypasses:
- the #13 light-adjustment tone remap (`f5cd4832`), so colour is not saturated/re-encoded there;
- the bloom threshold/feed (#10/#11 `2ed4a75f`, #14 `14b1e90e`), with our own HDR bright-pass and blur.

What can stay: #15 combine `37c5610f` (additive, unclamped), the SMAA passes, the #17 copy. Note #15 reads
the bloom texture as t1, so its input changes meaning if the bloom feed is replaced (scale/units must be
matched). Also #15 and SMAA assume gamma-encoded values; with linear HDR data the colour space needs handling.

Unresolved risk: the pixel shaders of passes #0, #2, #3 and #9 (shadow, opaque scene, effects) are NOT
embedded in the exe (0 of 8 hashes each), so they may clip colour themselves, and the HDR gain on effects
is unproven until measured in game. Also unknown: constant-buffer names/values, blend states, draw order
within #13 and #14, texture bindings (inferred).
