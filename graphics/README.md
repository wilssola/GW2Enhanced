# GW2 Enhanced Graphics

Float render targets, HDR bloom and optional HDR output for Guild Wars 2. The game's files are never
modified. Unofficial mod, not affiliated with ArenaNet. Direct3D 11 only.

It is the graphics feature of [GW2 Enhanced](../README.md): a library (`runtime/`, crate `gw2graphics`) compiled into
the mod's single proxy DLL, `msimg32.dll`, next to the translation and HD textures features. It runs alongside
ReShade (`dxgi.dll`). Design: [docs/specs/graphics-design.md](docs/specs/graphics-design.md).

## What it does

The game draws the scene into 8-bit targets and its post chain clamps colour to 0..1, so a fire spell
and a white wall end up the same white. The DLL:

1. Creates the scene targets as 16-bit float instead (full size and quarter-size bloom targets only).
2. Replaces the three post-chain pixel shaders that clamp (tone remap, bright-pass downsample, bloom
   threshold) with versions that let values above 1.0 through. Bloom strength is a setting. M1 replaces
   only these three clamping engine shaders plus the resolve step below, not the whole engine light
   adaptation or combine: everything else in the post chain runs as the game wrote it.
3. Adds one resolve step before the UI is drawn, which tone maps the scene: identity below the knee, a
   shoulder above it. In SDR the result is clipped to 0..1 exactly as before.
4. In HDR (Windows HDR on) it creates the swapchain as float16 scRGB and converts the final pass, so
   highlights go past paper white up to `peak_nits` (the final pass clamps to it); the UI stays at
   `paper_white_nits`. scRGB is the default; HDR10 exists as an experiment (`hdr_format=hdr10`, off by
   default, see [docs/hdr10.md](docs/hdr10.md)).

A frame classifier checks the pass structure every frame. If a frame does not look like the world
(loading screen, character select, cinematic, or a game patch that changed the chain), every feature
turns off for that frame (it takes 3 matching frames to turn on and 2 mismatching frames in a row to turn
off). In SDR the game then renders as before. In HDR it is different, see "Known limitations": a missing or
changed final-pass shader (`afdc14cb`) breaks the picture, and screens that skip it are dim. Reasons go to
the log.

Resolution and window changes are followed automatically (the game's `ResizeBuffers` is observed; the log
shows `screen size changed WxH`).

## Compared with ReShade, RenoDX and DLSS

A post-processing tool such as [ReShade](https://reshade.me) gets the finished frame: 8-bit, already clamped to
0..1 by the engine's post chain, with the UI drawn in. This feature works one level earlier, inside the frame:

- **Before the clamp.** The engine blends its effects in 8 bits *before* bloom and clamps in three post-chain shaders
  ([docs/engine-post-chain.md](docs/engine-post-chain.md)). With the scene targets created as float16 and those
  shaders replaced, a fire spell carries more energy than a white wall all the way to bloom and to the HDR output.
  Highlights that are already clipped in the presented frame cannot be recovered afterwards.
- **Before the UI.** The effects and the tone curve run before the UI pass, so nameplates, the HUD and text are never
  graded, bloomed or reflected.
- **With the engine's depth.** It reads the linear depth the engine itself writes (pass 3 of the
  [frame](docs/frame-structure.md)), the same frame the scene
  comes from.
- **No neural network, any GPU.** Plain Direct3D 11 shaders. [DLSS 5](https://www.nvidia.com/en-us/geforce/news/dlss-5-3d-guided-neural-rendering/)
  is an AI rendering stage that game developers integrate and that needs a GeForce RTX 50 series card; GW2 does not
  integrate it, and this feature neither upscales nor uses AI.
- **Next to ReShade.** ReShade loads as `d3d11.dll`/`dxgi.dll` and keeps working; with it loaded, `hdr_output=auto`
  stays SDR (see [HDR output and ReShade](#hdr-output-and-reshade)). [RenoDX](https://github.com/clshortfuse/renodx)
  is a ReShade add-on that rewrites a game's shaders for HDR; its [mod list](https://github.com/clshortfuse/renodx/wiki/mods)
  has a Guild Wars 2 entry marked in progress.

The comparison with GW2Hook, d912pxy and the usual ReShade presets is in the
[root README](../README.md#the-graphics-compared-with-other-graphics-mods).

## Install

1. Close the game. Install GW2 Enhanced with its installer, or extract an `All` or `Graphics` release package into
   the game folder (next to `Gw2-64.exe`); see the [root README](../README.md#installing). A self-built
   `target\release\msimg32.dll` (see [Build](#build)) is installed by copying it there.
2. Start the game. `GW2Graphics\config.ini` is created, and `GW2Graphics\gw2graphics_log.txt` when the log is on
   (`log_graphics=1` in `GW2Enhanced\config.ini`, or **Logs** in the overlay's Health tab; off by default). The
   graphics feature started if the log starts with a `GW2 Enhanced Graphics ...` line.
3. Open the overlay and its **Graphics** tab: `Shift+Home` (`hotkey_graphics` in `GW2Enhanced\config.ini`, also changed
   in the Version tab). The overlay only appears about 8 seconds after the game starts.

For what to check in the game see [docs/playtest.md](docs/playtest.md).

To uninstall, run the installer with `--uninstall` or delete `msimg32.dll` (and the `GW2Graphics` folder if you
like). A `wtsapi32.dll` left over from the earlier standalone graphics build must be deleted too.

## Settings (`GW2Graphics\config.ini`)

Also editable in the overlay (the `Save` button writes the file). Out-of-range values are clamped.

| Key | Default | Range | Meaning |
|---|---|---|---|
| `enabled` | `true` | | `false` = original behaviour (see Limitations for HDR output) |
| `hdr_output` | `auto` | `auto`, `on`, `off` | HDR output. Read at start |
| `paper_white_nits` | `203` | 80 to 400 | Brightness of the UI and of "white" in HDR |
| `peak_nits` | `1000` | 400 to 4000 | Brightest highlight in HDR |
| `bloom_strength` | `1.0` | 0 to 2 | `1` = as the game; applies live |
| `hdr_format` | `scrgb` | `scrgb`, `hdr10` | EXPERIMENTAL HDR10 swapchain, never with ReShade ([docs/hdr10.md](docs/hdr10.md)). Read at start |
| `photo_key` | `Ctrl+Shift+End` | | Photo mode hotkey, same syntax ([docs/photo.md](docs/photo.md)). Read at start |
| `photo_hide_ui` | `0` | `0`, `1` | Photo mode: save the 3D scene without the UI when possible |
| `overlay_key` | `Shift+End` | | No longer used: the hotkeys are `hotkey` / `hotkey_graphics` in `GW2Enhanced\config.ini` (Version tab), `Shift+Home` by default |
| `lens_strength` | `0.15` | 0 to 1 | Lens streak / glare ([docs/lens.md](docs/lens.md)); `0` = off |
| `lens_threshold` | `4` | 1.2 to 8 | Light source brightness, times paper white before the highlight boost |
| `lens_ghost` | `0.05` | 0 to 0.5 | Ghost flares of very bright sources; `0` = none |
| `lens_streak_length` | `0.6` | 0.25 to 2 | Streak length |
| `lens_max_size` | `0.04` | 0 to 0.2 | Largest lens source, share of the picture height: bigger glowing areas (creatures, walls) get no flare; `0` = no size test |
| `lens_color_reject` | `0.5` | 0 to 1 | Saturated blue / cyan / violet / green glows need more brightness to flare; `0` = colour ignored |

The caps are also a visibility rule: no setting can hide anything the original shows.

### HDR output and ReShade

`hdr_output=auto` switches the swapchain to HDR only when Windows HDR is on **and ReShade is not
loaded**: ReShade draws its effects on the swapchain and they are not HDR-safe. With ReShade present,
`auto` stays SDR (the float targets, bloom and resolve still work). Set `hdr_output=on` to opt in
anyway. The swapchain is never touched if another tool already changed its format.

### Photo mode and HDR calibration

The overlay has a **Photo mode** section (and the hotkey `photo_key`): a full-resolution PNG, a linear Radiance
`.hdr` and a settings text into `GW2Graphics\photos`, or a with/without-mod pair ([docs/photo.md](docs/photo.md)).
The **HDR calibration** section shows peak, paper-white and black-level test patterns drawn by the mod's own
last pass ([docs/hdr10.md](docs/hdr10.md)).

### Overlay

The **Graphics** tab: `Enabled`, `HDR output` (needs `Save` and a restart of the game), `Paper white`, `Peak`, `Bloom`, a status block
(classifier verdict, gate, output mode and the rule that decided it, float upgrade, substitution,
resolve, copy-format mismatches), `Save`, and `Dump frame`, which writes the pass structure of one frame
to `GW2Graphics\frame-passes-<unix seconds>.log` (pass structure only; it may start with the overlay's
own pass; used after a game patch, see
[docs/after-a-game-patch.md](docs/after-a-game-patch.md)).
Without a recorded frame it still writes a file (`no frame recorded yet ...` plus the hook counters),
and the overlay shows the full classifier reason and the hook counters; while the classifier does not
match, the log gets a `hooks: ...` counter line every 5 s (30 at most).

### How the game is hooked

The native function bodies are detoured inline (the address is read once from the vtable of the object the
creation call returned), so a hook fires for every object of the class whatever its pointer or wrapper; the log
says `inline hook <method> at <addr> ok|failed` and falls back to patching the vtable slot. Nothing is decided by
device or COM identity: every device is seen (only the screen-size textures are upgraded), every immediate
context is recorded and the frame of the one with the most draws is classified, and the game's swapchain is the
one on the game's window (class `ArenaNet...`), whichever device creates it, by `CreateSwapChain` or by
`D3D11CreateDeviceAndSwapChain`. The `hooks:` line shows per method how it is hooked, its calls, the distinct
`this` values and where the vtable and the slot of the last `this` point.

## Known limitations

- **HDR depends on the final pass.** In scRGB the picture goes through the replacement for the engine's
  last pass, `ps#afdc14cb`. If the exe no longer contains that hash (a game patch), or its replacement
  cannot be created, the picture is dark or wrong; the log gets a `WARNING` and the overlay `Final pass:`
  line says so. Any screen that writes the back buffer WITHOUT `afdc14cb` (loading screens, login,
  character select, cinematics) is shown gamma-encoded in a linear buffer, so it looks dim. The player must
  check these screens in HDR. `hdr_output=off` is the escape hatch. In SDR a missing hash is harmless: the
  game renders as before.

- Other overlays and ReShade's UI look dim in HDR: they draw gamma-encoded colours onto a linear
  float swapchain.
- Switching Windows HDR on or off while the game runs needs a restart of the game; the mode is decided when the
  swapchain is created.
- `Enabled` off stops new float targets, the HDR shaders and the resolve at the next frame. Targets
  already float keep working, and with HDR output the final conversion stays on; a full revert needs a
  restart (or `hdr_output=off`).
- Effect shaders that the exe does not contain, or that ReShade adds, may still clip.
- Paper white changed in the overlay recompiles the final-pass shader on its next use (instant, but it
  is a recompile); peak and bloom do not.
- A game patch can change the frame structure. In SDR nothing breaks: features switch off and the log says why.

## Build

Prerequisites: Rust stable with the MSVC toolchain (`x86_64-pc-windows-msvc`) and the Windows SDK (the
MSVC build tools install both). From the repository root:

```powershell
cargo build --release              # target\release\msimg32.dll with every feature
cargo mod-graphics                 # target\release\msimg32.dll with only the graphics feature
cargo test --release -p gw2graphics                 # unit tests; the shader tests use the D3D11 WARP software device (about 30 min)
$env:GW2_EXE = "G:\path\to\Gw2-64.exe"; cargo test --release -p gw2graphics   # also compares the replacement shaders with the game's originals
```

Without `GW2_EXE` the equivalence tests print `skipped` and pass. With `CARGO_TARGET_DIR` set, the DLL is in
`%CARGO_TARGET_DIR%\release\`.

## Proxy exports

The feature has no exports of its own. The DLL is loaded under the name `msimg32.dll`, and every module of the game
process that imports from that name gets ours, so it must export everything the real one does:
`enhanced/src/proxy.rs` exports and forwards all 5 functions of `System32\msimg32.dll` (`AlphaBlend`,
`TransparentBlt`, `GradientFill`, `DllInitialize`, `vSetDdrawflag`); see
[the architecture of the mod](../docs/specs/unified-mod-design.md#why-msimg32dll).

`tools/check-exports` is the guard written for the earlier standalone build, which loaded as `wtsapi32.dll`: the
embedded browser `bin64\cef\libcef.dll` imports `WTSFreeMemory` and `WTSQuerySessionInformationW`, and when only the
exe's two functions were exported the game aborted with `Failed to load CEF DLL. Error code: 0x7f`, so that build
forwarded all 78 exports. The tool is fixed
to `wtsapi32.dll` (module name and smoke calls) and does not apply to the `msimg32.dll` build as it stands.

## Layout

| Path | What |
|---|---|
| `runtime/src` | The feature: `d3d` (hooks), `passes`/`classify`/`recorder`/`gate` (frame detection), `upgrade` (float targets), `shaders`/`hdr_shaders`/`resolve`/`tonemap`/`output` (shaders, resolve, HDR swapchain), `overlay`, `config`, `log` |
| `runtime/shaders` | HLSL of the replacement shaders, resolve and final pass |
| `tools/check-exports` | Python export guard of the earlier standalone `wtsapi32.dll` build (see Proxy exports) |
| `tools/dxbc-dump` | Python tool to list and disassemble the game's shaders by hash |
| `docs` | [Design](docs/specs/graphics-design.md), [M1](docs/specs/graphics-m1-design.md), the engine post chain, [frame structure](docs/frame-structure.md), [interception](docs/interception.md), [after a patch](docs/after-a-game-patch.md), [playtest](docs/playtest.md) |

## Credits

The shell fur follows the look and the shell technique of [UnityFurURP](https://github.com/hecomi/UnityFurURP) by hecomi;
the shaders here are a rewrite for the game's Direct3D 11 renderer.
