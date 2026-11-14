# Playtest

What to check in the game, and what to send back. You need GW2 Enhanced with the graphics feature
(`msimg32.dll`, the `All` or `Graphics` package or a self-built one) and about half an hour. Automated tests cannot drive the game, so these are the checks that decide whether the mod is good.

## Setup

1. Close the game. Install a release package with the installer, or build the DLL
   ([README, Build](../README.md#build): Rust stable with the MSVC toolchain and the Windows SDK) and copy
   `msimg32.dll` (`target\release`, or `%CARGO_TARGET_DIR%\release` when that variable is set) next to
   `Gw2-64.exe`. Delete any `wtsapi32.dll` left there from the earlier standalone graphics build.
2. Start the game once, then close it and set in `GW2Graphics\config.ini`:
   - `hdr_output=on` if ReShade is present (with it, `auto` would stay SDR),
   - leave the rest at the defaults. There is no `dump_shaders` option.
3. Start the game. `GW2Graphics\gw2graphics_log.txt` starts with a `GW2 Enhanced Graphics v...` line when the
   graphics feature started. `Shift+Home` opens the overlay (`Shift+End` in a graphics-only build) with the
   **Graphics** tab, but the overlay only appears about 8 seconds after the game starts (it is installed late on
   purpose). The graphics feature runs next to the translation and HD textures features and ReShade; all of them
   must keep working, also with ReShade's menu open.

## Reference scenes

Look at each scene with `Enabled` on and off (toggle in the overlay) and, for HDR, with Windows HDR on.

| Scene | What to look for |
|---|---|
| City (e.g. Lion's Arch) | Same look as the original with `Enabled` off; sun and lamps slightly brighter in HDR; UI and nameplates unchanged |
| Open field, daytime | Sky and sun glints go past white in HDR; no banding; ground telegraphs still clearly visible |
| Water (lake or sea, sun reflection) | Reflections glint in HDR; nothing turns black or white |
| Combat with effects | Spells and fire glow past white in HDR, bloom grows with the `Bloom` slider; effects never hide enemies or telegraphs |
| Night | Not brighter or darker than the original; bright lights bloom; no raised black level |

Suggested order: SDR with the defaults (must look like the original); then `Bloom` at 0, 1 and 2;
then HDR output.

## Verify

(a) **Classifier and coexistence.** In `GW2Graphics\gw2graphics_log.txt` the line `frame matches`
appears in the world and `frame does not match` on loading screens and at character select, and
flips back without flicker when you return to the world. ReShade (effects, its menu) and the other
features keep working. Interception coexistence is the main risk: report anything odd. While the classifier says `does not match`, the
log gets a `hooks: ...` line every 5 s (30 at most): per method how it is hooked (inline/vtable), calls, the distinct
`this` values with `GetType`, where the vtable pointer and the slot of the last `this` point (module name), the
immediate contexts with their draws of the last frame, and the devices with the most CreateTexture2D /
CreatePixelShader calls. Send it as it is.

(b) **Shader hashes.** After a minute in the world the log has these lines:
- the first 8 `CreatePixelShader ... len=... dxbc_size=... hash=...` lines, where `len` equals
  `dxbc_size` in each;
- four `seen engine shader` lines, for `f5cd4832`, `2ed4a75f`, `14b1e90e` and `afdc14cb`. If one is
  missing, a game patch changed that shader (see [after-a-game-patch.md](after-a-game-patch.md));
- `substitute applied` lines for `f5cd4832`, `2ed4a75f` and `14b1e90e` once the log says `frame matches`
  (and `afdc14cb` as well with HDR output on);
- the interception lines: `d3d: inline hook <method> at 0x... ok` for each of the 17 hooked methods (context
  draw/render-target/copy/PSSetShader, device Create*, factory `CreateSwapChain`, swapchain `ResizeBuffers`). A
  `failed: ...` line followed by `vtable slot N patched instead` is the fallback and works too, but report it. One
  `d3d: first <method> call this=0x... type=0` line per method that is called (type 0 = immediate context). A second
  class of objects (a wrapper layer) shows up as a second `detour slot 1` line;
- `output: game window 0x...` (the window whose class starts with `ArenaNet`), and one
  `d3d: <creation call> returned hr=0x... swapchain=... device=... context=...` line per device/swapchain creation
  (40 at most). `objects created while ignoring` lines are the overlays' dummy devices (normal).
Send the log if any of them is missing or wrong.

(c) **SDR.** With Windows HDR off (or `hdr_output=off`), scenes without bright effects look identical
to the original, and the log has no `copy format mismatch` lines. The overlay shows `Output: Sdr`.

(d) **HDR.** With `hdr_output=on` and Windows HDR on (restart the game after changing either): spells
and fire glow past white, the UI sits at paper white (`Paper white` slider), no black or white screen.
`Output: ScRgb` in the overlay. Known: other overlays and ReShade's UI look dim, because they draw
gamma-encoded colours onto the float swapchain.

(e) **Screens that skip the engine's final pass (HDR).** With HDR output on, look at the loading screen,
login, character select, a cinematic, and the world with the UI hidden (screenshot mode). Screens that
write the back buffer without `afdc14cb` are drawn gamma-encoded into a linear buffer, so DIM is expected
there. Report which screens are dim (and whether the world is correct again afterwards). The overlay's
`Final pass:` line must say `ok`; a `WARNING` in the log about `afdc14cb` means the picture is wrong:
set `hdr_output=off` and send the log.

(f) **Resolution and window changes.** In the world, change the resolution or switch between windowed
and borderless. The log shows `screen size changed WxH`, the picture stays correct (no black, no 8-bit
banding in the sky) and the classifier keeps saying `frame matches`.

(g) **Other tabs and overlays open.** Switch between the Translation, Textures and Graphics tabs of the
overlay, and open ReShade's menu while the overlay is open. All must keep working; the game must not crash
or flicker.

(h) **Screenshot key and copy mismatches.** Take a screenshot with the game's key. The picture is saved
and the game does not crash. `copy format mismatch` lines in the log are only logged (nothing is
changed); send them if there are some.

(i) **Settings.** `HDR output` in the overlay only takes effect after `Save` and a restart of the game.
Paper white and Peak apply when you release the slider.

(j) **`Enabled` off** restores the original look at the next frame (HDR output keeps its conversion
until a restart, so the picture stays correct).

(k) **Crash.** If the game crashes, delete `msimg32.dll` (or run the installer with `--uninstall`), start the game to confirm it is fine
without the mod, and send the log.

## Send back

- `GW2Graphics\gw2graphics_log.txt` (and `gw2graphics_log.prev.txt` after a crash),
- the overlay status block (screenshot) for one world scene,
- a `frame-passes-*.log` from **Dump frame** if the classifier says `does not match` in the world
  (close the overlay window first, or accept that the dump may start with the overlay's own pass),
- a short note per scene: better, same, or worse, and what.
