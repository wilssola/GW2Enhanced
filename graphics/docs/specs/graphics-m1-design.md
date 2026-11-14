# GW2 Enhanced Graphics, milestone M1: how it is built and verified

M1 upgrades the game's 8-bit scene buffers to float16, lets the post chain carry values above 1.0, resolves the
scene with its own tone mapping, and outputs HDR (scRGB) or SDR, with an in-game on/off. Design:
[graphics-design.md](graphics-design.md). The frame: [frame-structure.md](../frame-structure.md). AO (M2), volumetric light (M3) and SSR (M4) are
built on top of M1 and documented on their own ([../m2-ao.md](../m2-ao.md), [../shafts.md](../shafts.md),
[../ssr.md](../ssr.md)).

**Architecture.** Rust code in `graphics/runtime` (crate `gw2graphics`). It intercepts Direct3D 11 (device, context,
swapchain). A pure-Rust *pass classifier* identifies the engine's passes by structure (formats, sizes, order, draw
counts), and a gate disables every feature when the frame does not match. Shaders are HLSL compiled at start with
`d3dcompiler_47`.

**Stack.** Rust 2024, `windows-sys`, `retour`, `hudhook =0.9.3` (dx11), HLSL. Same conventions as the texture runtime
(init thread, `log!`, `config.ini`).

**How it is loaded.** M1 was first built and tested as a standalone DLL named `wtsapi32.dll` (the exe imports
`WTSRegisterSessionNotification` and `WTSUnRegisterSessionNotification` from it; the embedded browser `libcef.dll`
imports two more, which is why that build ended up forwarding all 78 exports). Today the crate is an rlib compiled into the
GW2 Enhanced `msimg32.dll` with the cargo feature `graphics`; the proxy exports live in `enhanced/src/proxy.rs` and
`gw2graphics::init` is started by `enhanced` on its own thread.

## Constraints

- Direct3D 11 only; no Direct3D 12 device; no ray or path tracing.
- Data folder `GW2Graphics\` next to the exe: `config.ini`, `gw2graphics_log.txt`.
- Nothing runs under the loader lock: `init` runs on a thread of its own.
- Never crash or visibly break the game: an unknown frame structure, a missing shader or a failed hook turns that
  feature off and logs why.
- Coexists with ReShade loaded as `dxgi.dll` (with RenoDX/DLSS add-ons). Never forces HDR output over a swapchain
  another tool already changed.
- Visual only. No effect may reduce the visibility of anything the original shows (ground telegraphs, nameplates,
  enemies in fog). Bloom is capped; overlay sliders are capped at these limits.
- No heavy work while a map loads. The default look matches the original in scenes without bright effects.
- No fixed addresses or bare hashes: pass structure first, hashes only to identify the shaders that are replaced.
  The patch procedure is in [../after-a-game-patch.md](../after-a-game-patch.md).
- Playtests judge the visuals; automated tests cannot drive the game.

## Failure cases it is built for

- A game patch changes the frame structure: the classifier returns an error, every feature stays off, the game
  renders as before.
- Loading screens, character select and cinematics have frames that do not match the world frame: they pass
  through untouched, with no flicker when the world frame returns.
- Resolution change, fullscreen toggle or window resize: the upgrade rule follows the new size and old float targets
  are not reused with wrong sizes.
- Windows HDR switched on/off, or ReShade/another tool changing the swapchain: the output mode is decided again,
  never forced.
- Alt-tab, device removed or reset: no use of released objects; state resets cleanly.

## 1. Crate skeleton, log, config

Files: `graphics/runtime/Cargo.toml`, `src/lib.rs`, `src/log.rs`, `src/config.rs` (tests inline in `config.rs`).

- `log!` and `log::init(&Path)` behave like the texture runtime's log: the previous session's log is kept as
  `.prev.txt`.
- `config::Config` holds `enabled`, `hdr_output` (`HdrOutput`), `paper_white_nits`, `peak_nits`, `bloom_strength`,
  `overlay_key` (and, since, many more keys); `Config::parse(&str)` ignores unknown keys and falls back to the
  default on a bad value; `Config::load(&Path)` reads the file.
- Defaults: `enabled=true`, `hdr_output=auto`, `paper_white_nits=203`, `peak_nits=1000`, `bloom_strength=1.0`,
  `overlay_key=Shift+End`. Ranges: paper white 80 to 400, peak 400 to 4000, bloom 0 to 2.
- `init()` returns unless the exe is `Gw2-64.exe`, creates `GW2Graphics\`, opens the log, loads the config and logs
  the version.

Tests: `defaults_when_empty` (empty text gives the defaults), `parses_values` (`paper_white_nits=300` gives 300.0),
`clamps_out_of_range` (`bloom_strength=9` gives 2.0, `peak_nits=10` gives 400.0), `ignores_garbage`
(`paper_white_nits=abc` keeps 203.0).

In the standalone build this step also had `proxy.rs` forwarding the WTS exports to `System32\wtsapi32.dll` and a
`cdylib` named `wtsapi32`; that is gone now that the crate is part of `msimg32.dll`.

## 2. Pass model and classifier (pure logic)

Files: `src/passes.rs`, `src/classify.rs`; fixture `runtime/src/frame_fixture.rs`, read with
`include_str!`.

- `passes::Target { w, h, fmt }` (`fmt` is the DXGI_FORMAT number), `passes::Pass { rt: Vec<Target>, depth:
  Option<Target>, draws, dispatches }`, and `passes::parse_frame_log(&str) -> Vec<Pass>`, which parses the
  diagnostic's text format.
- `classify::classify(frame: &[Pass], screen: (u32, u32)) -> Result<SceneMap, ClassifyError>` with
  `SceneMap { opaque, depth_linear, effects, luminance, light_adjust, bloom: Vec<usize>, combine, ui }` and
  `ClassifyError { reason }`.

Rules, verified against the fixture:
- `opaque`: the first pass with RT0 `B8G8R8A8_UNORM` (87) at screen size, full-size depth and 100+ draws;
- `depth_linear`: the first later pass with RT0 `R32_FLOAT` (41) at screen size;
- `effects`: the next pass after `depth_linear` with the same RT and depth as `opaque` and 100+ draws;
- `luminance`: the pass with RT0 1x1 `R16G16B16A16_FLOAT` (10);
- `light_adjust`: the first full-size no-depth BGRA pass after `luminance`;
- `bloom`: the passes after `luminance` whose RT0 is exactly screen/4 in both dimensions;
- `combine`: the first full-size no-depth BGRA pass after `bloom` with 1 draw;
- `ui`: the first pass after `combine` with full-size depth and 100+ draws.

Tests: `fixture_frame_has_21_passes`; `classifies_fixture` (3440x1440 gives opaque 3, depth_linear 4, effects 9,
luminance 12, light_adjust 13, bloom `[14]`, combine 15, ui 18); `wrong_screen_size_fails`;
`missing_luminance_fails` (pass 12 removed); `loading_like_frame_fails` (3 passes with small RTs);
`other_aspect_ratio_works` (every fixture size scaled to 1920x1080 and classified with that screen size).

## 3. Direct3D 11 interception and frame recorder

Files: `src/d3d.rs`, `src/recorder.rs`, `src/overlay.rs`.

- `d3d::install()` detours `D3D11CreateDevice` and `D3D11CreateDeviceAndSwapChain` in `d3d11.dll` (via `retour`).
  From the objects the game receives it hooks the methods it needs, at these vtable indices (from
  `d3d11.h`/`dxgi.h`): device `CreateTexture2D` 5, `CreateShaderResourceView` 7, `CreateRenderTargetView` 9,
  `CreatePixelShader` 15; context `PSSetShader` 9, `DrawIndexed` 12, `Draw` 13, `DrawIndexedInstanced` 20,
  `DrawInstanced` 21, `OMSetRenderTargets` 33; factory `CreateSwapChain` 10.
- `recorder::Recorder` collects one `Vec<Pass>` per frame (a new pass when the bound targets change, draw counts
  added); `recorder::frame_end` runs `classify` and logs `frame matches: opaque=.. ui=..` or
  `frame does not match: <reason>` once per change.
- The frame tick comes from the overlay's render loop, not from a second `Present` hook.
- At this step the hooks only record; they change nothing.

Test: a synthetic call sequence (`om_set_render_targets`, `draw`, `frame_end`) produces the expected `Vec<Pass>`.

Playtest: a minute in the world with the DLL installed, then the log. Expected: `frame matches` in the world,
`does not match` on the loading screen and at character select, ReShade and the other features still working.
Coexistence with ReShade's hooks was the main risk of the milestone. The first real runs led to the current
interception: the native method bodies are resolved from the vtable once and detoured inline (`RawDetour`), so a hook
fires for every object of the class whatever its pointer or wrapper, with a vtable-slot patch as fallback, and the
game's swapchain is the one on the game's window (class `ArenaNet...`), whichever device creates it
([interception.md](../interception.md)).

## 4. The engine's tone-mapping chain (investigation)

Tool and result: `graphics/tools/dxbc-dump/dxbc_dump.py` (lists the DXBC blobs embedded in the exe by hash and
disassembles them with `d3dcompiler_47!D3DDisassemble`) and [../engine-post-chain.md](../engine-post-chain.md).

The shaders of the passes `light_adjust`, `combine` and `bloom` were taken from the exe offline, by hash, rather
than dumped from the running game (there is no `dump_shaders` option). For each of `effects`, `light_adjust`,
`bloom` and `combine`, `engine-post-chain.md` records the colour space of input and output (gamma or linear), whether
it saturates/clamps, which texture and constant-buffer slots it reads, and where bloom amount, exposure,
saturation and tint come from. It ends with the choice between **(A)** the chain passes values above 1.0 through
to the end, so only a resolve pass is added, and **(B)** it clamps or compresses, so the clamping stages are
replaced. The result is **B**: the tone remap and bloom threshold shaders clamp colour, so they are substituted
(steps 6 and 7).

## 5. Float scene upgrade

File: `src/upgrade.rs`, wired in `d3d.rs`.

- `upgrade::target_format(desc: &TexDesc, screen: (u32, u32)) -> Option<u32>` with
  `TexDesc { w, h, fmt, bind_flags, mips, samples, usage, cpu_access, ... }`. It returns `Some(10)`
  (`R16G16B16A16_FLOAT`) only for `B8G8R8A8_UNORM` 2D textures with render-target and shader-resource bind flags,
  1 mip, 1 sample, default usage and no CPU access, whose size is the screen size or exactly screen/4; otherwise
  `None`. The `CreateTexture2D` hook applies it; the `CreateRenderTargetView` / `CreateShaderResourceView` hooks
  rewrite a non-null `B8G8R8A8_UNORM` view format to the float format for upgraded textures.
- The rule takes the current screen size at each call, so after a resize only targets of the new size are upgraded.
- If any upgrade call fails, it is logged, the upgrade is disabled and the game continues with 8-bit targets.
- The feature is gated by `enabled` and by a frame that classified successfully at least once (the hooks are inert
  before that).

Tests: `upgrades_full_size_scene_target`; `upgrades_quarter_size_bloom_target` (860x360 at 3440x1440);
`leaves_sixteenth_size_exposure_target` (215x215); `leaves_swapchain_format` (`R8G8B8A8_UNORM`);
`leaves_depth_and_shadow_maps`; `leaves_msaa_and_staging`; `follows_new_screen_size` (the same 1920x1080 target is
upgraded only when the screen is 1920x1080).

Playtest (SDR look unchanged): with the upgrade on and no HDR, the five reference scenes show no artifacts, no black
or white screen and no visible change except that bright effects no longer clip early. A final copy to the
swapchain between formats (for example `CopyResource`) is the place where a fix-up would be needed; the log reports
any `copy format mismatch`.

## 6. Pixel-shader registry and substitution

Files: `src/shaders.rs`, wired in `d3d.rs`. Needed because of the decision of step 4 (B).

- `shaders::fnv1a32(bytes) -> u32` (offset basis 2166136261, prime 16777619; equal to the `ps#` hashes of the frame
  log).
- `shaders::Registry` with `record(shader_ptr, bytecode)` and `hash_of(shader_ptr) -> Option<u32>`;
  `shaders::Substitutes::get(hash)` returns the replacement `ID3D11PixelShader`; `shaders::install_substitute(hash,
  hlsl, entry)` compiles lazily with `D3DCompile` (`d3dcompiler_47.dll`, target `ps_5_0`).
- Device slot 15 `CreatePixelShader` records `fnv1a32(bytecode)` for each returned shader (overwritten when a
  pointer is reused; shaders created before the hook are unknown). Context slot 9 `PSSetShader` binds the substitute
  when one exists for the bound shader's hash and substitution is enabled, and passes the argument through
  otherwise. Substitution stays off until the frame classifies.
- A substitute that fails to compile is logged once and the original shader stays in place.

Tests: `fnv1a32_matches_known_blob_hashes` (with `GW2_EXE` set: the blob with hash `4340ca88` has 504 bytes; skipped
otherwise); `fnv1a32_empty_is_offset_basis`; `registry_overwrites_reused_pointer`; `registry_unknown_pointer_is_none`;
`substitution_disabled_returns_original`; `hlsl_compile_error_is_reported_not_panicked` (a broken HLSL string returns
`Err` with the compiler message).

## 7. HDR replacement shaders for the clamping stages

Files: `runtime/shaders/light_adjust_hdr.hlsl` (replaces `f5cd4832`), `runtime/shaders/bright_pass_hdr.hlsl`
(replaces `2ed4a75f` and `14b1e90e`), `src/hdr_shaders.rs`; tests in `src/hdr_shaders_tests.rs`.

- `hdr_shaders::install_all()` registers the substitutes. Each replacement keeps the same input semantics,
  texture/sampler slots and constant-buffer layout as the original (taken from its disassembly,
  `dxbc_dump.py disasm <exe> <hash>`), reproduces its math, and differs only in removing the upper colour clamp:
  `f5cd4832` keeps the gamma-2.0 decode, the XYZ luminance remap and the 1/255 dither but drops the `sat` on Y' and on
  the final colour (a lower bound of 0 stays, with `max(x, 0)`, so negative or NaN values still become 0); the two
  bright-pass shaders keep `max(x, 0)` and drop the upper clamp.
- With HDR output a fourth substitute replaces the engine's final pass `afdc14cb` (`runtime/shaders/final_scrgb.hlsl`,
  see step 9).

Tests (on the D3D11 WARP device; the comparisons with the originals run only with `GW2_EXE` set and print why they
skip otherwise): a small full-screen quad (for example a 64x64 float16 target) is rendered through the original blob,
extracted from the exe by hash, and through the replacement, with the same inputs and constants.
`light_adjust_matches_original_for_in_range_input` (random inputs in [0,1], at most 2/255 apart including dither),
`light_adjust_passes_values_above_one` (an input of 3.0 gives an output above 1.0 where the original gives at most
1.0), `bright_pass_matches_original_in_range`, `bright_pass_passes_values_above_one`, `no_nan_or_negative_output`
(negative and extreme inputs give finite outputs of at least 0), and `hlsl_files_compile`.

## 8. HDR resolve and tone mapping

Files: `src/tonemap.rs`, `src/resolve.rs`, `runtime/shaders/resolve.hlsl`, wired in `d3d.rs`.

- Input: the float targets of step 5 and the substitutes of step 7. The last engine colour pass before the UI is #17
  (`c0ad1686`, a pure copy); the resolve runs right after it, immediately before the UI pass starts, so the UI is not
  tone-mapped.
- Colour space (from `engine-post-chain.md`): the whole chain is gamma-encoded (about 2.0). The resolve decodes with
  the sRGB EOTF extended above 1.0 (the power segment continues for `v > 1`), tone-maps per channel in linear light
  and encodes back to display.
- `tonemap::map(rgb, &ToneParams) -> [f32; 3]` is the Rust reference of the curve; the HLSL mirrors it.
  `ToneParams { shoulder_start, peak, paper_white }`: below the knee (default `shoulder_start` 0.8 of the headroom
  `peak / paper_white`, never below 1.0) the output equals the input, so scenes without bright effects look like
  the original; above it highlights roll off smoothly; SDR compresses to 1.0, HDR to `peak / paper_white`.
- `resolve::Resolver` compiles `resolve.hlsl` and draws a full-screen triangle that turns the scene-referred float
  buffer into the display-referred one, in place through a scratch copy. A state guard saves and restores all bound
  state around it (render targets, viewport, scissor, shaders, blend, rasterizer, depth-stencil, input layout,
  topology, SRVs, samplers, constant buffers).
- Substitution is turned on only after a frame has classified successfully.

Tests: `identity_below_shoulder` (0.5 stays 0.5), `monotonic` (increasing inputs never decrease the output over 0 to
20), `sdr_never_exceeds_one`, `hdr_never_exceeds_peak_ratio`, `continuous_at_shoulder`, `zero_stays_zero`,
`srgb_roundtrip_extended` (encode(decode(v)) is v for v from 0 to 4); on WARP, `resolve_hlsl_compiles`,
`resolve_shader_matches_rust_reference` and `resolve_applies_in_place_and_restores_pipeline_state`.

Playtest: SDR shows no change in the reference scenes; bright spells and fire roll off smoothly instead of turning
flat white. The later move of the effects into the engine's own passes (the resolve draw alone was overwritten by
the engine in the real game) is described in [../data-path.md](../data-path.md).

## 9. HDR output and the ownership rule

File: `src/output.rs`, wired in `d3d.rs` and `resolve.rs`.

- `output::decide(request, windows_hdr, swapchain_fmt, other_tool_changed_swapchain, other_tool_present) ->
  OutputMode` with `OutputMode { Sdr, ScRgb }`. `ScRgb` only when Windows HDR is on, the game's swapchain format was
  still `R8G8B8A8_UNORM` at creation, no other tool changed the swapchain, and the request allows it: `off` never,
  `auto` only when ReShade is not loaded, `on` regardless of ReShade. `Sdr` otherwise.
- The `CreateSwapChain` / `ResizeBuffers` path creates `R16G16B16A16_FLOAT`, calls
  `SetColorSpace1(DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709)`, and the output is scaled by `paper_white / 80`. The
  Windows HDR state is read from the output's `DXGI_OUTPUT_DESC1` colour space.
- If any swapchain or colour-space call fails, the output falls back to `Sdr` and the log says why.
- In scRGB the engine's last pass `afdc14cb`, which writes gamma-encoded values to the back buffer, is replaced so
  that the picture is linear (see README, "Known limitations").

Tests: `sdr_when_windows_hdr_off`, `scrgb_when_all_conditions_hold`, `sdr_when_request_off`,
`sdr_when_another_tool_changed_swapchain`, `sdr_when_swapchain_already_float`, `redecides_when_windows_hdr_toggles`.

Playtest: with Windows HDR on, spells are brighter than white up to the configured peak; toggling Windows HDR does
not crash (the mode is decided when the swapchain is created, so a change needs a restart); with ReShade effects that
change the swapchain, the output stays SDR and the log says why.

## 10. Overlay, config persistence and docs

Files: `src/overlay.rs`, `src/lib.rs`; `graphics/README.md`, `graphics/docs/after-a-game-patch.md`,
`graphics/docs/playtest.md`.

- The panel (today the **Graphics** tab of the GW2 Enhanced overlay; in a graphics-only build its window opens with
  `overlay_key`, default `Shift+End`): checkboxes `Enabled`, `HDR output`; sliders `Paper white (nits)` 80 to 400,
  `Peak (nits)` 400 to 4000, `Bloom` 0 to 2 (the caps of the visibility rule); a line with `frame matches` or the
  failure reason; `Save` writes `config.ini` through `Config::save(&Path)`.
- `Enabled` off restores the original behaviour at the next frame (no new float targets, no resolve); targets
  already upgraded keep working.
- The docs cover installing, the patch procedure (re-capturing the frame log, running the classifier test on it,
  re-checking the shader hashes) and the playtest (the five reference scenes and what to look for).

Test: `save_then_load_roundtrips`.

## Coverage

- Design coverage: float targets (step 5), bloom/tone mapping (step 4 decides, steps 6 to 8 implement), HDR output with
  SDR fallback and the ReShade rule (step 9), overlay/config/toggle (step 10), pass-structure detection and
  self-disable (steps 2 and 3), patch procedure (step 10), failure cases (Constraints, Failure cases). AO, SSR and
  volumetric light are outside M1.
- Names shared across steps: `SceneMap`, `Pass`, `Target`, `Config`, `HdrOutput`, `OutputMode`, `Resolver`,
  `upgrade::target_format`.
