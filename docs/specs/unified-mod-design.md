# GW2 Enhanced: architecture of the single mod

GW2 Enhanced is one mod: one proxy DLL, `msimg32.dll`, placed next to `Gw2-64.exe`, that hosts three features
(translation, HD textures, graphics), one overlay with a tab per feature and a Health tab, one installer and one
updater. This document describes how the pieces fit together and how the mod behaves when a game update changes
what a feature depends on.

## Requirements

1. One DLL in the game folder for every feature; the player installs and removes one file.
2. After a game update the worst case is "some features are off and the overlay says which and why". The game
   itself never crashes because of the mod, and the original behaviour (original textures, untranslated text,
   vanilla look) is the fallback.
3. The features never interfere with each other: no two hooks on `Present`, no feature waiting for another to start,
   one failing feature never stops the others.
4. Upgrading from the earlier separate mods keeps the player's settings, collected translation data and HD pack
   location.

Out of scope: guaranteeing that every patch keeps every feature working (some patches move or rewrite what a
feature hooks); other games, other platforms or the 32-bit client; ray tracing (the game is Direct3D 11, DXR needs
Direct3D 12).

## Layout

One Cargo workspace at the repo root.

```
enhanced/                  the cdylib: lib name `msimg32` (DllMain, proxy forwarding, overlay host, Health tab, updater)
core/                      shared plumbing: game image, byte patterns, site resolution, build cache, health registry, signed checksums
translated/                translation feature (rlib `gw2translated`), local translator, tools
remastered/native/runtime/ HD textures feature (rlib `gw2remastered`)
graphics/runtime/          graphics feature (rlib `gw2graphics`)
installer/                 the installer (`GW2Enhanced-Installer.exe`)
```

The feature crates own neither `DllMain` nor any export. Each exposes an `init()` that `enhanced` calls on a thread of
its own, a `MODULE` handle that `enhanced` fills in, and an overlay panel that it publishes for the host to draw.

Which features go into the DLL is chosen at build time by the cargo features of `enhanced` (`translation`, `textures`,
`graphics`, all on by default). The aliases in `.cargo/config.toml` build the same `target/release/msimg32.dll` with
fewer features:

```sh
cargo mod-all            # translation + textures + graphics (same as `cargo build --release`)
cargo mod-translation    # only the translation
cargo mod-textures       # only the HD textures
cargo mod-graphics       # only the graphics
```

The release packages (`GW2Enhanced-All`, `-Translation[-CPU|-CUDA|-DirectML]`, `-Textures`, `-Graphics`) are these
builds plus the data folders each feature needs.

## Why `msimg32.dll`

Windows looks for a DLL that `Gw2-64.exe` imports in the exe's folder first, unless it is a KnownDLL. Among the DLLs
the exe imports that are not KnownDLLs, `msimg32.dll` has the fewest exports:

| DLL | Exports |
|---|---|
| `msimg32.dll` | 5 |
| `version.dll` | 17 |
| `msacm32.dll` | 42 |
| `usp10.dll` | 44 |
| `wtsapi32.dll` | 78 |

Fewer exports means less to forward and less that a Windows update can add. Every module of the game process that
imports a DLL by that name gets ours, not only the exe; with a small, stable export list the proxy stays a superset
of the real DLL. Other common GW2 add-ons do not use the name: ReShade, arcdps and Nexus load as `d3d11.dll` /
`dxgi.dll`.

`enhanced/src/proxy.rs` exports the five functions (`AlphaBlend`, `TransparentBlt`, `GradientFill`, `DllInitialize`,
`vSetDdrawflag`). On first use the real DLL is loaded by full path (`GetSystemDirectoryW` + `\msimg32.dll`), each
function's address is looked up once and cached, and the call is forwarded. If the real DLL or a function is missing,
the forwarder returns a failure value instead of crashing (`0`, `1` for `DllInitialize`).

The DLL carries the marker `GW2ENHANCED-DLL-MARKER!!`, which the installer uses to recognise its own `msimg32.dll`.

## Start-up

`DllMain` does only two things on `DLL_PROCESS_ATTACH`: `DisableThreadLibraryCalls` and spawning the
`gw2enhanced-init` thread. Nothing runs under the loader lock.

The init thread starts, each on its own named thread:

- every built-in feature (`gw2enhanced-translated`, `gw2enhanced-textures`, `gw2enhanced-graphics`), each inside a
  panic guard. A feature that panics while starting is recorded as `Off("panicked while starting")` in the health
  registry; one that returns without reporting a state of its own is recorded as `Ok`. Each feature checks that the
  host process is `Gw2-64.exe` and does nothing otherwise;
- the overlay host (`gw2enhanced-overlay`, see below);
- with the textures feature, a memory monitor (`gw2enhanced-memory`) that every 3 s reports the free system commit
  (RAM + pagefile) as `memory / commit`: degraded when caches had to be trimmed or HD loading is paused;
- the update check (`gw2enhanced-update`, see Updates).

The features hook at different moments and none waits for another: graphics hooks Direct3D 11 almost immediately
(about 0.2 s), the textures feature hooks the file layer at about 0.8 s, and the overlay attaches after the features
have published their panels.

Only one `#[global_allocator]` can exist in the DLL. With the textures feature it is the texture runtime's counting
allocator (`gw2remastered::mem::CountingAlloc`), which measures the memory the mod holds for its panel.

## Overlay

One hudhook instance, one hook on `Present` (the game cannot have several). `install_overlay` polls every 250 ms for
the panels the built-in features publish, for at least 8 s and at most 40 s, then hooks `Present` once with a single
`ImguiRenderLoop` that draws them all. With the graphics feature the hook is installed inside
`gw2graphics::overlay::while_hooking_present`, so graphics does not adopt hudhook's dummy device and swapchain as the
game's. If no feature published a panel, or hooking `Present` fails, the overlay is recorded as off and the features
keep running without it.

The window is titled **GW2 Enhanced** and has a tab per built-in feature (**Translation**, **Textures**,
**Graphics**) plus **Version** and **Health**. The hotkeys belong to the host (`enhanced/src/hotkeys.rs`), saved in
`GW2Enhanced\config.ini` and changed in the Version tab; every one is `Shift+Home` by default:

| Key | Builds | What it does |
|---|---|---|
| `hotkey` | full build | opens / closes the window on the tab it was left on |
| `hotkey_translation`, `hotkey_textures`, `hotkey_graphics` | every build with that feature | single-feature build: opens / closes the window; full build: opens it on that feature's tab (again on that tab: closes); equal to `hotkey`: acts as `hotkey` |

The graphics feature does its per-frame work inside the render loop whether the window is open or not (it is the
frame tick of its passes). The translation's first-run setup and the update prompt are windows of their own, shown
with the main window closed. Every panel is drawn inside `catch_unwind`: a panel that panics is turned off and
recorded in the registry, and the other tabs keep working. The mouse cursor is drawn while any window is open, never
under HDR10 output (it would be drawn at 10000 nits).

### Health tab

The health registry (`core/src/registry.rs`) records a state per feature and per item: `Ok`, `Degraded(reason)` or
`Off(reason)`. The Health tab lists every entry, worst first, with a summary line ("Everything is working." or
"N off, M degraded") and the updater's state at the bottom. It is the first place to look after a game update.

## Configuration, data folders and logs

Each feature keeps its own folder next to `Gw2-64.exe`, with the same names the separate mods used, so an upgrade
needs no migration of data:

| Folder | Contents |
|---|---|
| `GW2Translated\` | `config.ini`, `gw2translated_log.txt`, collection (`discovered.jsonl`, `decrypted.jsonl`), `languages\<language>\`, `translator\` |
| `GW2Remastered\` | `config.ini` (HD on/off, 4x options, `overlay_key`, `pack_dir`), `gw2remastered_log.txt` (+ `.prev.txt`), `pack\` by default |
| `GW2Graphics\` | `config.ini`, `gw2graphics_log.txt` (+ `.prev.txt`), frame dumps, photos |
| `GW2Enhanced\` | `config.ini` (`check_updates`), `update_log.txt`, `update\` (staging), `update_cleanup.txt` |

A `.prev.txt` log is the previous session's, kept so the log of a session that crashed survives the next start.

## Resilience to game updates

Layered, from "never crash" to "repair fast"; each layer is useful alone.

1. **Isolation.** Each feature starts inside its own panic guard and on its own thread; each overlay panel is drawn
   inside its own guard. A failure turns off that feature or panel, the reason goes to the registry, and the rest
   runs.
2. **Located, not hard-coded.** No feature uses a fixed address. The translation (29 signatures,
   `translated/src/sig.rs`) and textures (26 signatures, `remastered/native/runtime/src/sig.rs`) features find
   their functions by byte signatures with wildcards over relative operands, each of which must match exactly once
   in `.text`; struct offsets are read from the instructions that use them. A signature that is not found turns off
   only the hook or feature that needs it, with the reason in the feature's log. Graphics uses no byte signatures:
   it recognises the frame by its pass structure and the engine's shaders by the hash of their bytecode, and turns
   its effects off for any frame that does not match (see `graphics/docs/after-a-game-patch.md`).
3. **Site resolution in `core`.** `core` describes a site by how to find it rather than where it is
   (`core::site::Site`): an ordered list of strategies, an expected entry kind and a risk level.
   - `Sig(pattern)`: a byte signature that must match exactly once in `.text`.
   - `StringXref(text)`: the function that references a unique string of the data sections (an assert message, a
     source file name, a format string). Strings rarely change when code is recompiled.
   - `CalleeOfAnchor(text, n)`: the n-th distinct function called (`call rel32`) by the function a string
     identifies, for helpers without a string of their own.

   `core::resolve` tries the strategies in order and validates each result before it is accepted: a site declared
   `FunctionStart` must follow padding (`int3`, `nop`, zero) or a `ret`. The result records which strategy won, so a
   match by anything but the first one is reported as degraded (`registry::report_resolved`). `core::cache` stores
   the resolved addresses keyed by a fingerprint of the build (FNV-1a of `.text`, `SizeOfImage`, timestamp): same
   build, the stored address is re-checked cheaply and used; new build, the cache is discarded and rebuilt.
   `Risk` (low, medium, high) records how much damage a wrong hook at the site could do. The same `Module` view is
   built from the running process or from a file on disk, so everything here also runs offline.
   `core/tests/real_exe.rs` checks that every signature the features use resolves through `core` to the same single
   address their own search finds.
4. **Graceful degradation.** Original behaviour is always the fallback: untranslated text, original textures,
   the vanilla frame. A feature or hook that cannot be placed stays off and says so; nothing is guessed.

**Evidence from the real exe (measured with `core/tests/string_anchors.rs`).** Of the 39 sites the
features use, 14 are the start of a function that a unique string identifies, and 10 more are called by such a
function: 24 of 39 (62%) can be found without their raw bytes. The other 15 have one to three callers, none of them
anchored directly (mostly the composite and chat clusters): they need a second hop (callers of anchored callers) or
a relocation-tolerant comparison against a stored copy of the function. Strings alone are therefore not enough,
which is why the signature stays the first strategy and `CalleeOfAnchor` covers the one-hop helpers.

**First real patch (exe 43,065,736 -> 43,109,256 bytes).** All 39 signatures still matched exactly
once in the new build, with no change to the mod. The functions had only moved (+752 to +5632 bytes) and their code
was 91-100% identical (`core/tests/compare_builds.rs`, which compares the sites of two builds given as `GW2_EXE_OLD`
and `GW2_EXE_NEW`). The mod started on the new build with every hook active. Signatures that wildcard relative
operands survive ordinary patches; the other layers matter for a patch that reorders or rewrites code. Both builds
are kept under `re/builds/` (git-ignored) as baselines.

What this does not do: recover a feature whose hooked function was removed or redesigned by a patch. Such a feature
stays off, and its log and the Health tab say which site failed. The per-feature patch guides are
`translated/docs/after-a-game-patch.md`, `remastered/docs/after-a-game-patch.md` and
`graphics/docs/after-a-game-patch.md`.

## Updates

`enhanced/src/update.rs`, built in whatever features are. Nothing in it blocks the game: the check, download and
install run on their own threads and the overlay only reads the shared state.

1. 5 s after the start, the `gw2enhanced-update` thread asks GitHub for the latest release of
   `wilssola/GW2Enhanced`. `check_updates=false` in `GW2Enhanced\config.ini` turns the check off.
2. When the release is newer, a small window asks the player (shown with the main window closed too).
3. "Update now" downloads the same package this DLL was built as (`GW2E_PACKAGE`, set by CI), only through the
   GitHub API asset URLs of this repository, with a size cap.
4. The zip is checked against the release's `SHA256SUMS.txt`, and `SHA256SUMS.txt` against its ed25519 signature
   `SHA256SUMS.txt.sig` (`core/src/signed.rs`; the public key is built into the DLL, the private key exists only as
   the CI secret `UPDATE_SIGNING_KEY`). Without a valid signature nothing is installed.
5. The zip is unpacked on the game's volume and every file is swapped in by renames: files in use (this
   `msimg32.dll`, the running translator) are renamed to `<name>.old` first; those that cannot be deleted at once
   are listed in `GW2Enhanced\update_cleanup.txt` and deleted at the next start. If any step fails, the renames
   already done are undone and the game keeps running on the old version.
6. The player's files are never written, even if a zip had them: `config.ini`, `local.jsonl`, `web_local.jsonl`,
   `discovered.jsonl`, `decrypted.jsonl`, and the game's `Gw2-64.exe`, `Gw2.dat`, `Local.dat`.
7. After a successful install the game is asked to close after 10 s (and ended if still alive 20 s later), so the
   next start runs the new version.

Every step is written to `GW2Enhanced\update_log.txt`, and the Health tab shows the updater's state.

## Installer and migration of old installs

`installer/` builds `GW2Enhanced-Installer.exe`, shipped at the root of every release zip next to `msimg32.dll` and
the data folders of that package.

```text
GW2Enhanced-Installer.exe                install (finds the game or lets the player pick the folder)
GW2Enhanced-Installer.exe "<folder>"     install into the given folder
GW2Enhanced-Installer.exe --uninstall    uninstall and restore the previous msimg32.dll
```

- It finds the game through the Steam libraries (`libraryfolders.vdf`), the ArenaNet registry keys and the default
  install folders, or shows a folder picker. It refuses to run while `Gw2-64.exe` is running.
- A `msimg32.dll` from another mod (no GW2 Enhanced or GW2 Remastered marker) is never replaced silently: the install
  stops with a warning, and the launcher asks the player. Only when they choose to replace it (CLI:
  `--replace-other-mod`) is it backed up as `msimg32.dll.antes-do-gw2enhanced` and restored on uninstall.
- The data folders in the package (`GW2Translated\`, `GW2Remastered\`, `GW2Graphics\`) are copied over the game's;
  only files the package carries are added or overwritten, so the player's settings, collection, translations and
  logs stay. Obsolete per-language files of older versions (`strings.jsonl`, `ids.jsonl`) are removed.
- Uninstall removes only our `msimg32.dll`, restores the backup and keeps the data folders.

**Migration from the earlier separate mods.** Before the features were merged, the translation shipped as its own
`version.dll` (GW2 Translated), the HD textures as `msimg32.dll` (GW2 Remastered) and the graphics as `wtsapi32.dll`.
On install and uninstall the installer:

- removes GW2 Translated's `version.dll` (recognised by `GW2TRANSLATED-DLL-MARKER`), since both at once would hook
  the game twice, and puts back the `version.dll` of another mod it had backed up
  (`version.dll.antes-do-gw2translated`);
- replaces GW2 Remastered's `msimg32.dll` (recognised by `GW2REMASTERED-DLL-MARKER`) without a backup, as it is ours.

The data folders kept their names, so settings, collected strings, translations and the HD pack location carry over
unchanged. The installer does not look for the earlier standalone graphics build: a `wtsapi32.dll` left in the game
folder from it must be deleted by hand, otherwise the graphics feature would be loaded twice.
