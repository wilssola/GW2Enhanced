# GW2 Enhanced Graphics: design

The graphics feature of GW2 Enhanced (`graphics/runtime`, crate `gw2graphics`, compiled into `msimg32.dll` with the
cargo feature `graphics`; see [the architecture of the mod](../../../docs/specs/unified-mod-design.md)). It makes
Guild Wars 2 look modern without changing its art style. The textures are covered by the HD textures feature; what
is left is the lighting: the engine is old, everything is rendered into 8-bit buffers, and magic, fire and other
effects cannot be brighter than white, so they look flat.

## Goals

1. **Real HDR**, mostly visible in spell effects, fire and other glowing effects. Real means the monitor's HDR
   output, not only a wider-looking SDR image.
2. **Better reflections** on materials that need them (water, metal, ice).
3. **Better shading in general**, as far as the engine data allows.
4. **Volumetric lighting** (light shafts, fog lit by the sun).

Success: a player with an HDR monitor sees effects that glow past white, with bloom that follows the energy of the
light. A player on SDR gets the same scene tone-mapped. Both look like the same game, only more polished. Every
effect toggles on and off in game, and nothing makes anything easier to see or do in PvP/WvW than the normal game
allows (visual only; no cheats, no reading or sending anything about other players).

Ray tracing and path tracing are out: the game is Direct3D 11, and DXR needs Direct3D 12.

## Phases

- **Phase 1:** float scene, HDR bloom and tone mapping, HDR output, then screen-space AO, volumetric light and
  screen-space reflections.
- **Phase 2:** material shading. It starts with a **spike**: a visible fur shader on the charr, to learn whether
  replacing character pixel shaders is practical. Phase 2 needs per-pixel normals and roughness that the engine does
  not store, so it starts with another diagnostic (which textures and buffers each draw uses,
  [draw-diagnostic.md](../draw-diagnostic.md)). Real layered ("shell") fur is a stretch goal
  ([shell-fur.md](../shell-fur.md)).

Phase 1 ships in milestones, each usable alone: **M1** float scene targets + bloom/tone mapping + HDR/SDR output
([specs/graphics-m1-design.md](graphics-m1-design.md)); **M2** screen-space AO ([m2-ao.md](../m2-ao.md)); **M3** volumetric light
([shafts.md](../shafts.md)); **M4** screen-space reflections ([ssr.md](../ssr.md)). M1 is the foundation and the only one
that must work for phase 1 to be useful.

## Scope

Phase 1 does:
- Upgrade the scene's colour buffers from 8 bits to 16-bit float so lighting can exceed 1.0.
- Replace the engine pixel shaders that clamp colour (tone/exposure remap, bright-pass downsample, bloom threshold)
  with HDR versions that let values above 1.0 through, and add a resolve with its own tone curve, keeping the game's
  look at default settings. The rest of the post chain (light adaptation, combine) runs as the game wrote it.
- HDR output (scRGB float16 swapchain, HDR10 as an option) with a paper-white and a peak-brightness setting, and an
  SDR fallback.
- Screen-space ambient occlusion, screen-space reflections and volumetric light, each with its own on/off and
  quality setting.
- An in-game panel (the **Graphics** tab of the GW2 Enhanced overlay) with on/off and settings, and a config file.

Phase 1 does not:
- Use ray tracing, path tracing or any Direct3D 12 device.
- Change material shaders, per-pixel normals/roughness or fur (phase 2).
- Change game data files or textures. The game's files are never modified.

Failures even if it works:
- Crashes, black screens or flicker after a game patch. A changed frame or shader must turn the feature off, not
  crash.
- Any visual change that gives a gameplay advantage (for example hiding fog or making telegraphs easier to see than
  normal), or reading depth/colour to feed external tools.
- Breaking the other features or a ReShade setup.
- A loading-time cost (no heavy work while a map loads).

## Constraints

- Direct3D 11 only. The engine renders through an embedded bgfx (`renderer_d3d11.cpp`).
- **Inside the mod's own DLL, not a ReShade add-on.** The feature runs in `msimg32.dll` with the other features;
  `dxgi.dll`/`d3d11.dll` are left to ReShade (which may also run RenoDX/DLSS add-ons).
- Must coexist with ReShade loaded as `dxgi.dll`, including when ReShade also changes formats or the swapchain.
- Rust, like the other features (`windows-sys`, `retour`, `hudhook` for the overlay).
- Must survive game patches: find everything by render-pass structure, with shader hashes as the identification of
  the few shaders that are replaced, never by fixed addresses. Pixel-shader hashes change with patches.
- Developed on a high-end GPU at 3440x1440. Should still run on mid-range GPUs; heavy effects are optional.

## What we know about the engine (from the diagnostic)

Captured with a diagnostic ReShade add-on that only logs; the frame is in [frame-structure.md](../frame-structure.md).

- The scene is **forward rendered into a single 8-bit target** (`B8G8R8A8_UNORM`, with `D24S8` depth). The
  deferred PBR/GBuffer/float-radiance tonemap shader that exists inside the exe is not used. There is no normal
  or roughness buffer. Swapchain: `R8G8B8A8_UNORM`, 3440x1440.
- A stable 21-pass frame, identical across 9 captures ([frame-structure.md](../frame-structure.md)):
  1. shadow maps (2048² depth, 6144x2048 cascade atlas, 512² `R16G16_FLOAT`);
  2. opaque scene, about 900 draws, 8-bit;
  3. linear depth `R32_FLOAT` (full and half resolution), SSAO half-res `R16G16_UNORM`, AO blur/upsample `R16_FLOAT`;
  4. a full-screen pass, then transparents/effects (about 720 draws, **still 8-bit and before bloom**);
  5. downsample to 860x360 and 215x215, a 1x1 `R16G16B16A16_FLOAT` luminance target (light adaptation);
  6. light adjustment, bloom blur at 860x360, combine, anti-aliasing;
  7. UI (about 300 draws, with depth), a 512² render (icons), and a final overlay.
- Static analysis (Ghidra): the live path is the classic `PostProcManager` chain (`PostProcLightAdjustment`,
  `DepthBlur`, `DepthOfField`, `LightRays`, `Selection`, `DownAndBlur`, `Combine`). Decompiled code is in
  `<GW2REM_DATA>/re/dec_post`. Parameters (bloom amount, saturation, tint, exposure) are read from game globals.
- Pixel shaders carry no names; they are identified by bytecode hash. The mapping of pass to hash is in the log.
- Wide aspect (21:9) works with the same structure.

The three that matter most for design: the effects are blended in 8 bits **before** bloom (so float targets let
additive effects exceed white and feed bloom); depth is already linearised by the game (pass 3); shadow maps and
their matrices are available as textures/constants (so volumetric light is possible).

The pass-by-pass analysis of the post chain (what each shader does, which ones clamp) is in
[engine-post-chain.md](../engine-post-chain.md).

## Design decisions

- **Own code in the mod's DLL**, not a ReShade add-on.
- **Original art style kept**; the feature is visual only, everything optional and toggleable in game.
- **Output format:** scRGB float16 swapchain when Windows HDR is on, SDR otherwise. HDR10 is an experimental option
  ([hdr10.md](../hdr10.md)).
- **Config and data:** `GW2Graphics\config.ini` and `GW2Graphics\gw2graphics_log.txt` next to the game. The window
  hotkey is the one of the mod's overlay (`Shift+Home` in the full build); a build with only the graphics feature
  uses `overlay_key` from `GW2Graphics\config.ini` (default `Shift+End`).
- **Normals** for SSAO/SSR are reconstructed from the linear depth buffer (the engine has no normal buffer), which
  loses normal-map detail.
- **Phase 1 does not touch material shaders.**
- **Shadow cascades for M3.** The diagnostic only saw the shadow-map targets; the cascade constants and the sun
  direction are not known (the world shaders are not in the exe). M3 is therefore built as screen-space sun shafts
  estimated from the picture ([shafts.md](../shafts.md)).
- **AO** replaces the engine's AO result only while enabled; with it off the engine's own AO runs unchanged. It never
  stacks on top of the engine's AO ([m2-ao.md](../m2-ao.md)).
- **"Same look" acceptance.** Judged by eye against reference screenshots taken without the mod (about 5 scenes:
  city, open field, water, combat with effects, night). At default settings, scenes without bright effects look like
  the original; bright effects are allowed, and meant, to change. No numeric golden-frame check is required, but the
  pass-structure recording (below) is.
- **Visibility rule.** No effect may reduce the visibility of something the original shows (ground telegraphs,
  nameplates, enemies in fog). Volumetric light and AO are additive/neutral and clamped so they cannot darken or
  hide gameplay-relevant pixels beyond the original; bloom has a cap; the overlay's sliders are capped at these
  limits. Each effect is judged against this rule; any doubt is flagged.
- **Verification.** Automated tests cannot drive the game, so verification is: unit tests for the math, shader tests
  on the D3D11 WARP software device, a recorded pass-structure replay (the diagnostic's frame log is a test
  fixture), and short playtests at the end of each milestone ([playtest.md](../playtest.md)). Visual tuning (bloom,
  curve, SSR/volumetric quality) is done from in-game screenshots ([debug-mode.md](../debug-mode.md)).
- **ReShade/HDR ownership.** The diagnostic shows the swapchain is `R8G8B8A8_UNORM` with ReShade and the DLSS add-ons
  running, so no one owns HDR there. The feature owns the float scene targets and the HDR swapchain. When ReShade is
  loaded, `hdr_output=auto` stays SDR (ReShade's effects on the swapchain are not HDR-safe) but keeps the float scene,
  bloom and tone mapping; `hdr_output=on` opts in anyway. The feature never forces HDR output over another tool that
  already changed the swapchain (float or 10-bit).

## Where things are

- Interception: Direct3D 11 is hooked through `D3D11CreateDevice` / `D3D11CreateDeviceAndSwapChain`, with inline
  detours on the native method bodies and a vtable fallback (`runtime/src/d3d.rs`, `intercept.rs`; README, "How the
  game is hooked"; [interception.md](../interception.md)).
- Pass identification by structure (RT formats, sizes, order, draw counts), with a gate that turns every effect off
  when the frame does not match (`classify.rs`, `gate.rs`; [specs/graphics-m1-design.md](graphics-m1-design.md)).
- Float upgrade, substituted shaders, resolve and tone curve, HDR output: [specs/graphics-m1-design.md](graphics-m1-design.md),
  [engine-post-chain.md](../engine-post-chain.md), [data-path.md](../data-path.md).
- Effects: [m2-ao.md](../m2-ao.md), [shafts.md](../shafts.md), [ssr.md](../ssr.md), [contact-shadows.md](../contact-shadows.md),
  [halo.md](../halo.md), [aerial.md](../aerial.md), [exposure.md](../exposure.md), [lens.md](../lens.md),
  [grading.md](../grading.md), [aa-aniso.md](../aa-aniso.md), [rim.md](../rim.md).
- Phase 2: [draw-diagnostic.md](../draw-diagnostic.md), [fur-look.md](../fur-look.md), [shell-fur.md](../shell-fur.md).
- Tools for patch day: [after-a-game-patch.md](../after-a-game-patch.md).
