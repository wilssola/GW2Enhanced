# Guild Wars 2 textures — reverse engineering

> **Engineering log.** Sections are appended in order as the work went on, so later ones can supersede
> earlier ones (for example the single-file pack became archive volumes, and config switches mentioned
> along the way may no longer exist). The addresses are RVAs of one executable build; the runtime does
> not use them, it finds everything by signature. For the current behavior read the README and the code.

Analysis on `Gw2-64.exe` sha256 `99342078…0d925d8621`
(kept in the analysis folder, outside the repo), with REA 6.8 / Ghidra 12.1.4
(`decompile-all 0x140b12000–0x140b88000`, output in `<GW2REM_DATA>\re\dec_tex`)
and our own xref scripts (string → `lea rip` → function via `.pdata`).
RVAs change with every patch: the runtime must find everything by signature.

## Engine pipeline

- Rendering through an embedded **bgfx** (`Arena\Engine\Gr\Bgfx\External\bgfx`,
  backend `renderer_d3d11.cpp`). ArenaNet wrapper: `Gr\Bgfx\BgfxTexture.cpp`.
- Decoding of the textures in `Gw2.dat`: `Gr\Img\ImgAtex.cpp`
  (`0xb84310`, "invalid ATEX file signature", "byte count for AtexLevel is too
  small"), mips in `Gr\Img\ImgMip.cpp`.

## `BgfxTexture` (ArenaNet object)

| Function | RVA | What it does |
|---|---|---|
| Create | `0xb14410` | `bgfx::createTexture2D/3D/Cube(w, h, hasMips, 1, fmt, flags, mem=NULL)` — creates it **empty** |
| Lock2D | `0xb14a70` | locks one level (`level < m_levels`), returns pointer + pitch |
| Unlock2D | `0xb15b80` | uploads the level (`bgfx::updateTexture2D`, via `0xb6da00`) |
| Upload levels | `0xb13140` / `0xb13420` / `0xb14130` | `srcFormat`, `startLevel..endLevel` |
| **UpRes** | `0xb15110` | promotes the texture to larger dimensions ("Texture UpRes failed…", "Unexpected upres dims") |
| DownRes | `0xb14df0` | shrinks it ("Unexpected downres dims") |

Fields (byte offsets): `+0x00` bgfx handle (u16), `+0x04` type (0=2D,
1=3D, 2=Cube), `+0x10` format, `+0x14` width, `+0x18` height, `+0x1C`
depth, `+0x20` levels, `+0x24` flags (`0x2000000` = failed).

The engine already does **resolution streaming** (UpRes/DownRes). This is the
mechanism to reuse: create/promote the texture with the HD dimensions and send
the pack's levels in place of the originals.

## bgfx / bimg

- `createTexture` with format validation: `0xb6d483` ("Texture format is not supported!").
- "Mismatch in texture size.": `0xb6cbd0`.
- bimg DDS parser: `0xc1c520` ("DDS: Unknown texture format.").

## ATEX format (Gw2.dat)

`magic(ATEX/ATEU/ATEP…) fourcc u16 w u16 h` + custom block compression
(port in `tools/gw2tex/src/atex.rs`, from gw2dattools). Mapping:
DXT1→BC1, DXT2/3→BC2, DXT4/5/DXTL/DXTN→BC3, DXTA→BC4, 3DCX→BC5, BC7X→BC7.

## `ImgDecode.cpp` — the engine accepts DDS

`0xb160d0` is the streaming decoder (`decoder->in`, `inAvail`,
`fileSize`, `platform`). It reads the first 4 bytes of the file and picks the type:

- `0xb86a30`: ATEX/ATTX/ATEC/ATEP/ATEU/ATET → `fileType 0` → `0xb84310`.
- `0xb8c330`: compares with the constant `"DDS "` (`.rdata 0x1c95310`) →
  `fileType 1` → `0xb893f0` / `0xb89460`.
- anything else → `fileType 2`.

Consequence: handing the engine a **DDS in place of the ATEX** (same file id)
should make it load the texture at the DDS dimensions, without touching bgfx.
The swap point is the file layer (`Archive3\Archive.cpp`,
`File3\FileArchive.cpp`, `ArchiveAsync\ArchiveAsyncStreaming.cpp`,
`0xd82000–0xdbe000`). To be confirmed: which DXGI formats the DDS path accepts
and whether the file's mips are used.

## In-game swap — test 1

`msimg32.dll` DLL with a hook on the block callback of the asynchronous read
(`ArchiveAsyncStreaming.cpp`, `0xd83710`, `cb(data, size, offset, fileSize,
flags, job)`; `job+0x00` = file id, `job+0x28` = decodeFunc; the read goes
through the file vtable `+200` at `0xd843e0`). 40-minute diagnostic: 5,101
different ATEX files pass through there; no crash.

Swap of 232 color textures (DXT1/DXT5 DDS with mips): the decoder
received the HD DDS (size confirmed), but the game aborted in
`GrTex2d.cpp(1734)`, function `0xa6f310`:

- `dims.x * dims.y <= m_dataDims.x * m_dataDims.y` — `dims` = decoded
  image (`[this+0x40]+0x38/+0x3C`), `m_dataDims` = texture record
  (`[this+0x28]+0x48/+0x4C`), set **before** the read.
- next line (`0x6c7`): the decoded format (`[this+0x40]+0x44`) must equal
  the record's (`+0x64`) → the DDS has to keep the original format.

Texture record (`GrTex2d`): `+0x18` device texture (`BgfxTexture`),
`+0x48/+0x4C` dims, `+0x50` file, `+0x58` `m_fileHighRes` (high-resolution
variant, 2× — the "+3" seen in the .dat), `+0x64` format, `+0x90` state
(2 = low, 4 = high). `0xa76210` does the UpRes (`0xb15110`) when loading the
high file; `0xa76b20` does the DownRes (dims >> 1). Creation of the device
texture at `0xa6ed70` uses the record's dims.

Next: find where the record receives the dims (probably: header read from the
file before decoding) and supply the HD dims there, or, in `0xa6f310`,
promote the record with the engine's own UpRes when the image is larger.

## Armor composition (`CpsComposite.cpp`, `CpsTex.cpp`, `CpsOverlap.cpp`)

Tests: with 2× bases and 1× masks → assert "Composite mask
smaller than source RGB texture" (`GrTex2d.cpp(2169)`, `0xa747c0`). With 2× bases
and masks → no crash, but misaligned armor (swapped colors, cropped face).

Cause: the armor is composed **on the CPU** into a fixed-size atlas and each piece
goes to a fixed rectangle in pixels.

- Atlas size: `0x12d4130` → 1024×1024 (512×512 in low
  quality mode). Color/normal working buffers: `0x12dd540` (same
  constants `0x400`/`0x200`), created at `0x12d21a0` and `0x12d8fc0`.
- Rectangles per region: `0x12dbe90` copies `{x0,y0,x1,y1}` rectangles (u32,
  1024 space) from a table (`[regionData+0x20]`, per-region indices in
  `[+0x14]`); in 512 mode it divides by 2 (`>> 1`). `0x12dc050` picks the regions
  (`CpsOverlap.cpp`); `0xdc660` does the blit (`0xa70ae0`) of each rectangle.
- Probable reading: 512 mode uses the low variants and 1024 mode the "+3"
  variants (2×). For 2048: atlas 2048, rectangles × 2 and **all** the
  composition sources (high variant + masks) at 2× — including those of other
  players, or a fallback that upscales at runtime the ones that are missing.

## Test 7 (2048 atlas) — device textures

Atlas patch applied (`patched=true`). Two problems came from **non**-composition
textures (device textures) that were in the pack:

- Fixed-size record (pre-allocated, e.g. 1024×1024): the larger version
  goes through the header hook (new buffer), but the engine never considers the
  texture ready and requests it again (~1,950×/s) → black scenery. Mitigation in
  the DLL: `DENY` list by file id (the `decodeFunc` runs inside our call
  to the callback, so `CURRENT` identifies the file in the header hook).
- `UpRes` (`BgfxTexture.cpp(726)`, "bad dimensions") requires high = 2 × low;
  doubling only one variant of the pair breaks when entering the map.

Decision: for now the DLL only swaps **composition pieces** (CPU-only
textures: no `UpRes`, no pre-allocated record). HD for device textures requires
handling the low/high pairs together (`m_file` / `m_fileHighRes`).

## Diagnosis 9 — fixed piece slots

Full load log (no swaps): the composition pieces (2,287
loads) always come in with loader `flags 0x19`, record `flags 0x3036`,
**pre-allocated 1024×1024**, internal formats 26 (color), 27 (mask) and 30
(normal). Origin: `0x12d8950` loads each piece with `0xa72530`
(`CreateFromFile` with fixed `textureDims`, flag `0x1000`), and the size comes from
`0x12da170` (`CpsData.cpp`, `blitRectSet`): a 0x28-byte entry with
dims at `+0x08/+0x0C` (>> 1 in low quality).

DDS reader (`0xb89460`): FourCC → internal format: `DXT1` 0x16, `DXT5` 0x1a,
`DXTA` 0x1b, `DXTN` 0x1d, `ATI2` 0x1e (= 3DCX/composition normals), `BC5 `
0x1f.

`UpRes`/`DownRes` in normal play: 962/186 calls, all exactly 2×; the DLL's
guard refused none.

Full 2× composition in the DLL: atlas (`0x12d4130`), buffers (`0x12dd540`),
rectangles (`0x12dbe90` × 2), **piece slots** (`0x12da170` → copy with dims
× 2), 2× sources (pack + fallback DXT1/DXT5/DXTA/3DCX→ATI2), guard on
`UpRes`/`DownRes`.

## Test 10 (full 2× composition)

No crash in the first minutes: 2048 slots (`blitRectSet` doubled), 2×
pieces (525 from the pack + 1,312 from the fallback, ~35 ms each). Two problems:

- `UpRes` refused ~30 thousand times, 4× jumps (512→2048): composition pieces
  also loaded as a **model texture** (low/high pair), where only the high one
  was doubled. Fix: double only composition loads —
  read context `+0x28` decodeFunc, `+0x30` userdata = `GrTex2d` loader; loader
  flags `+0x10` = `0x19` in composition. The (decodeFunc, loader) pair is
  confirmed in the header hook before being used.
- "Composite mask smaller than source RGB" crash after ~2 min: a BC7X piece (553
  in the game) was not converted by the fallback. Fix: BC7X → DXT5 2×.
  (DXT3: 2 pieces in the game, not handled yet.)

Note: the white face in the test screenshot was the monitor's HDR, not the texture.

## Pending

- Function that links **file id → texture** (loader above `ImgAtex`), to
  identify the texture without a hash.
- Confirm how many levels the `.dat` carries per texture (the rest is generated in `ImgMip`).

## Reloading textures in game (overlay toggle) — analysis

Goal: the "Remaster active" button should take effect immediately. Today nothing changes because
(a) the `GrTex2d` records keep the already-decoded file and (b) each
`CpsComposite` keeps the source pieces in its slots and only recomposes when some
bit of `+0xae4` is set. Static analysis (`dec_grtex`/`dec_cps` decompilation +
disassembly); nothing was tested in game.

### What the game does in Options > Graphics > Texture Detail

Chain (high confidence):

1. `0x1376200`: pref setter (virtual method of the options object; pointer
   in `.rdata 0x224c768`). Writes `[this+0x118]` and calls
   `GrDevSetMetric(mode, 0x15, value)` (`0xa69af0`) for modes 2 and 0;
   then `[vtbl+0xc8]` (save/notify). On weak cards it forces value ≥ 1.
2. `0xa69af0` / `0xa69a10` (`GrDev.cpp`) → `0xa6b3c0`: writes the metric and, if the
   mode is the active one (`+0x230`), dispatches: `0x14` → `0xa73430` (limit of
   simultaneous loads `DAT_142587cb0`), **`0x15` → `0xa73440`**, `0x16` → `0xa735c0`.
3. **`0xa73440` = `GrTex2dSetQuality(q)`** (`GrTex2d.cpp`, "No valid case for
   switch variable 'quality'", q ∈ {0,1,2}, 0 = maximum). If it changed: `xchg`
   on `s_textureQuality` (`DAT_14268b5b4`); walks the list of **all texture
   files** of `s_fileManager` (`DAT_14268b840`: head at
   `+0x10`, node offset at `+0x00`); for each file, `rec =
   file+0x28`; if `(rec+0x60 & 0x10) == 0` and the file is not the record's
   `m_fileHighRes`, it stores `rec` in a temporary array; **then**
   calls `0xa75280(rec, 0)` for each one. It gathers everything first because Reload
   destroys objects of the list itself.
4. **`0xa75280` = `CIGrTexture2dData::Reload(rec, force)`** (log "Forcing
   reload of texture data from file '%s'"): requires `IsLoadedFromDisk()`
   (`m_file`/`m_fileHighRes` ≠ 0; **fatal assert** if not). With `force=0` and
   some file still loading (`file+0x34 != 7`), it only puts the record in the
   deferred list `mgr+0x88`, and Advance tries again later. Otherwise: destroys
   `m_fileHighRes` (the next UpRes request, `0xa73890`, recreates it from disk),
   calls `0xa75490(m_file, rec+0x60)`, sets `rec+0x90 = 5` and frees
   `rec+0x40`. With `force=1` it waits in a loop (`0x9cff60`) and advances the load
   immediately (`0xa6e9d0`). This is what the UI material cache uses (`0x322380` →
   `0xa72a90 GrTextureReload(handle, 1)`).
5. `0xa75490` (file load request): recomputes the size key in
   `0xa70670` (quality → `0x400 >> q`; ignored with flag `0x10`). If
   `file+0x7c` changed, `0xa6f880` resets the file (assert `!m_asset ||
   m_asset->IsIdle()`, frees decoder and bits, state 0). In both cases it relinks
   the file in the pending load list `mgr+0x48`. In the state machine
   `0xa6db40` (table `0xa6df10`), **state 7 falls into the same case as 0** →
   state 1 → `0xa73d20` → new read of the `.dat` (goes through our hook
   `0xd83710`).

Texture Detail does **not** rebuild compositions: the pieces have flag `0x10`
(they stay out of the loop) and are `GR_TEXTURE_POOLED` (`0x2000`). For
pooled records, the 6→7 state of `0xa6db40` **destroys `m_file`/`m_fileHighRes`** right
after the upload, and they stop being `IsLoadedFromDisk()`: calling `Reload`
on them gives a fatal assert. The good side: there is no per-file piece
cache. `CreateFromFile` (`0xa72530`) only reuses (`0xa70530`) while the file
is still loading, so recreating the piece always reads from disk.

### Per-frame advance and thread

`0x94d688` (View advance, game main thread):
`lea rcx,"ViewAdvanceTexture"` → `call 0xa70a90` (thunk `jmp 0xab8000`;
`0xab8000` calls `GrTex2dAdvance 0xa72d90`, `0xc08500`, `0xabedf0`). Right
after comes `lea rcx,"ViewAdvanceComposite"` → `call 0x12d8310` (thunk of
`CpsAdvance 0x12d2410`). `GrTex2dAdvance` processes the ops list
(`mgr+0x78`), releases loads by budget (`DAT_14268b7a0`, 3 queues), advances
the pending list (`mgr+0x58`, only files with `+0x38 != 0`) and the deferred
Reload list (`mgr+0x98`).

None of these lists has a lock: `SetQuality`, `Reload`, Advance and
`CpsAdvance` touch them directly. The critical section `DAT_14268b5e0` protects
**only** the decoder pool `DAT_14268b5c0` (allocation/release in
`0xa6ef60`/`0xa6f490`/`0xa6f880`), not the lists. Therefore (high confidence):
**calling from another thread (e.g. the overlay's `Present`) is not safe**. The
reload has to run on the thread that calls `0xa70a90`, where it runs
**before** the `CpsAdvance` of the same frame.

### Composition (`CpsComposite`)

- List of **all** compositions: base `0x1428a9710` (`+0x00` node
  offset = `0xb78`; head at `+0x10` = `0x1428a9720`; next =
  `*(obj+0xb78+8)`; bit 0 = end). Active list: base `0x1428a9730` (node
  `0xb88`, head `0x1428a9740`), walked by `CpsAdvance`. The ctor
  `0x12d16b0` links the object via `0x12d49a0` (`lea rcx,[0x1428a9710]`).
  Vtable `0x2185c58`.
- Fields: `+0x18 + i*0x48` (i < 15) piece slot `{token, rgb, masks…}`;
  `+0x6e8 + i*0x10` slot token/data; `+0xa48` variant token (compositions
  shared through the dictionary `DAT_1428a9750..58`, `0x12d3400`); `+0xae4`
  invalid-component bits (full mask `DAT_1425e68ec` = `0x7fff`);
  `+0xb1c` flags (`0x2000` invalid geometry, `0x10000` apply pending);
  `+0xb44` state (7 = ready, 4/5 composing); `+0xb48/+0xb50` final textures
  (color/normal); `+0xb58/+0xb60` scratch (max 4, `DAT_1428a970c`).
- **`0x12d6880` = schedule recomposition** (table `0x12d6970`). With
  `b44 ∈ {1,7}`: `b44 = 4` (or 3 if `0x2000`), `b1c |= 0x10000`, links into the
  active list (assert if `ae4 == 0` without `0x2000`). In the other states it
  does almost nothing.
- Flag `0x10000` makes `0x12d71d0` (start of `0x12d7f20`) call
  **`0x12d8ee0(slot, token)` on the 15 slots, which releases the cached pieces**
  (`0x12d8db0` → `0x140dc2720` on each texture). With the slots empty,
  `0x12d7f20` recreates each piece via `0x12d8950` → `0xa72530` → disk.
  `0x12d21a0` sees `ae4 == 0x7fff` and composes from scratch. The final textures
  `b48/b50` stay alive, so the character shows the old composition
  until the new one is ready.
- `0x12d52e0` (vtable `+0x88`) does the same for the dye bits
  (`a10[0..6]` → `0x100..0x4000`) and calls `0x12d6880`. `0x12d6320` (vtable
  `+0xb0`) is the "full rebuild": releases the slots, **recreates the model**
  (`0x12d58d0`, notifies observers) and schedules. It is heavier and has no
  direct calls in the binary.
- Compositions with `+0xa48 != 0` (variants/NPC) are shared through the
  dictionary, and `0x12d52e0`/`0x12d4f10` assert on them: skip.

### Discarded paths

- "Device lost": `bgfx::Fatal::Enum::DeviceLost … Queueing
  WM_APP_DEVICE_ERROR` only ends or restarts the device; not usable.
- The texture debug panel (`0xb527e5`: "Hide generated textures",
  "Reset All Debug Fills") has no reload option.
- Switching quality to another and back (`0xa73440(q')` + `0xa73440(q)`)
  uses the game's path, but reads everything twice, changes the dims in the middle and does not
  touch the compositions.

### Signatures (unique in this build)

| Target | RVA | Signature |
|---|---|---|
| `ViewAdvanceTexture` site (lea str; call; **call GrTexAdvance**; call; lea str2) | `0x94d8f9` | `48 8D 0D ?? ?? ?? ?? E8 ?? ?? ?? ?? E8 ?? ?? ?? ?? E8 ?? ?? ?? ?? 48 8D 0D ?? ?? ?? ?? E8 ?? ?? ?? ?? 41 0F` (target = rel32 of the 2nd `call`, at `+0x0D`; more robust: find the `lea` of the string "ViewAdvanceTexture") |
| `GrTex2dSetQuality` | `0xa73440` | `48 83 EC 48 8B D1 85 C9 74 23 83 EA` (`s_textureQuality`: `mov eax,[rip]` at `+0x2D`, disp at `+0x2F`; `s_fileManager`: `mov rbp,[rip]` at `+0x47`, disp at `+0x4A`) |
| `CIGrTexture2dData::Reload` | `0xa75280` | `40 53 57 41 56 48 83 EC 20 48 83 79` |
| file load request | `0xa75490` | `48 89 5C 24 10 57 48 83 EC 30 48 8B D9 48 8D 44` |
| `GrTextureReload(handle, force)` | `0xa72a90` | `48 89 5C 24 08 57 48 83 EC 20 8B FA 48 8B D9 48 85 C9 75 19 41 B8 A9 1A` |
| `CpsComposite` schedule | `0x12d6880` | `40 53 48 83 EC 20 8B 81 44 0B 00 00` |
| `CpsComposite` full rebuild | `0x12d6320` | `40 56 48 83 EC 20 48 83 B9 48 0A 00` |
| `CpsAdvance` | `0x12d2410` | `48 89 5C 24 10 48 89 74 24 18 57 48 83 EC 30 33 DB C7 05 5D` |
| link into the global list (`lea rcx,[rip]` of the list at `+0x26`, disp at `+0x29`) | `0x12d49a0` | `48 89 5C 24 08 48 89 6C 24 10 48 89 74 24 18 57 48 83 EC 20 48 8B DA 49 8B F1 48 8B` |
| Texture Detail pref setter | `0x1376200` | `48 89 5C 24 08 57 48 83 EC 20 8B DA 48 8B F9 33` |

The 12-byte ones are unique but short: prefer resolving through the string site
and cross-check with them.

### Plan (from simplest to most complete)

1. **Trigger on the right thread**: the overlay only does `RELOAD.store(true)`. A
   detour (retour) on `0xab8000` (target of thunk `0xa70a90`, found through the
   "ViewAdvanceTexture" site) calls the original and, if `RELOAD` is set, runs
   step 2 (and 3, for device textures). This lands before the `CpsAdvance`
   of the same frame.
2. **Compositions (what the DLL swaps today)**: walk the list
   `0x1428a9720`. For each `c` with `c+0xa48 == 0` and `c+0xb44 ∈ {1,7}`:
   `c+0xae4 |= *DAT_1425e68ec` and call `0x12d6880(c)`. The ones in
   another state go to a queue and are tried in the following frames (until they
   reach 7). This releases the pieces, rereads from disk (the hook delivers HD or the
   plain 2×, depending on `ENABLED`) and recomposes into the current atlas. Medium-high
   confidence. Risk: many reads at the same time (the game limits itself to 4
   scratch and to the load budget).
3. **Pack device textures (`DEVICE` pairs)**: imitate the loop of
   `0xa73440` without changing the quality. Walk `mgr+0x10` and gather `rec` with
   the same high/low filter, plus these conditions: `file+0x30` (file id,
   from `0xdbcba0(name)`) is in the pack; `(rec+0x60 & 0x2006) == 0`;
   `rec+0x94 == 0`; ops list `mgr+0x78` empty. Then call
   `0xa75280(rec, 0)` for each one. To follow exactly the Texture
   Detail path, first zero `file+0x7c` of the files in state 7 (forces
   the reset in `0xa75490`). **High risk; test with logging first**: the record
   keeps `m_dataDims` and the device texture with the old dims. HD →
   original should pass (`dims ≤ m_dataDims`), but original → HD tends to the
   assert `GrTex2d.cpp(1734)` (`0xa6f310`), and the low/high pair has to
   stay 2× for the `UpRes`. Until this is validated, device textures keep
   taking effect only on the next map load.
4. Do not use `0xa75280` on pooled records (pieces): `IsLoadedFromDisk()` is
   false and it asserts. Do not call any of this from the `Present` thread.

To check in game: whether the id in `file+0x30` is the same as `job+0x00` of the file hook; whether
`file+0x38` is still 1 after the load (without the reset of step 3 the
file may sit idle in the pending list); the frame time when recomposing many
armors at once.

## Per-character scale (4× only on the local player's character, 2× on the others): static analysis

Goal: compose the local player's character at 4× and all the others at 2×.
Static analysis only (Ghidra `dec_cps`/`dec_grtex` + disassembly).
Confidence: **A** = read in the code, **M** = strong inference, **B** = hypothesis.

### 1. Where a job starts and which object goes through each point

Object: `CpsComposite`, 0xB98 bytes, with refcount, vtable `0x2185c58`. Fields
used: `+0x18 + i*0x48` are the layers (15; each layer = `{+0 piece, +8..+0x40 8 textures}`),
`+0xa40/+0xa48` data (race/appearance; `+0xa48 != 0` = "data" composition),
`+0xaf0` `m_model`, `+0xb1c` flags, `+0xb40` current layer, `+0xb44`
`m_updateState`, `+0xb48/+0xb50` atlas (color/normal), `+0xb58/+0xb60` scratch.

Creation (A):
- `0x12d84b0` (allocates 0xB98) → ctor `0x12d16b0(this, appearance, flags, …)`
  (player characters, `+0xa48 = 0`). `0x12d8570` → ctor
  `0x12d1b20(this, a40, a48, flags, …)` (data composition).
- ctor → `0x12d58d0` → `0x12d32b0` → `0x12d3400` → **`0x12d8fc0(composite, low)`**
  → `0x12d4130(&dims, low)` (atlas) → `0x12dd750` ×2 (`+0xb48`, `+0xb50`).
  `0x12d4130` has only that caller. `0x12d8fc0` keeps `rcx` in `rbx` and passes
  `edx` along intact, so in the `0x12d8fc0` hook the composite is in `rcx`. (A)
- Rebuild (equipment change): `0x12d6320` → `0x12d58d0` on the **same object**
  → new atlas through `0x12d8fc0`. (A)

Update (A): everything is synchronous and nested in `0x12d25e0(composite, …)`
(`CpsComposite::Update`, called from `0x12d2410`/`0x12d7db0`):
- `0x12d21a0(composite)` → `0x12dd540(low, type, 0)` ×2 (scratch). It is the only caller.
- `0x12d7f20(composite)` → `0x12d8950(layer, i, &composite->a40, …, low=b1c&8, …)`.
  At the call site (`0x12d81ce`): `rcx = composite+0x18+i*0x48`, **`r8 = composite+0xa40`**.
  So, in the `0x12d8950` hook, `composite = r8 - 0xa40`.
- Inside `0x12d8950`: `0x12da170(piece+0x66)` (blitRectSet, only caller);
  `0xa72530` (`CreateFromFile`, only called from here, 2 sites) with `createFunc =
  0x12d8930`; and `0x12dd5e0` (switch) → `0x12dd1d0`/`0x12dcc70`/`0x12dc660` →
  `0x12dc050` → **`0x12dbe90`** (all callers of `0x12dbe90` are in this
  tree).

Per-composite context: there is no "current composite" global. But **all** the
points are reached synchronously from `0x12d8fc0(rcx=composite)` or
`0x12d25e0(rcx=composite)`. A thread-local `CUR_SCALE` set in those
two hooks is enough (saving and restoring the previous value). (A)

What is asynchronous: the **piece reads/decodes**. When the source
is not ready yet, `0xa70ae0` (blit) creates a deferred job (`0xa6b940`, pool
`0x268b6c0`) that holds the textures, and the file read runs later, on another
thread. That is why the read hook can **not** use a composite thread-local.
(A for the deferred job; M for "another thread")

### 2. How to tell it is the local character

- `0x9b1fb0` = GetContextCollection (TLS: `gs:[0x58][_tls_index] + 0x10`).
  `ctx+0x98` = CharClient context, and **vtable `+0x68` = GetControlledCharacter**:
  `PvpCliContext.cpp` stores the return value in `m_controlledCharacter` and tests whether it is ≠0.
  The pattern `call 0x9b1fb0; mov rcx,[rax+0x98]; mov rax,[rcx]; call [rax+0x68]`
  appears 254 times. (A/M)
- Creation in the world: `0x1166580` (`AvCharModelHelpers.cpp`, assert
  `character->IsPlayer() || character->IsMonsterClone()`). Args: `ecx` flags,
  **`rdx` = character**, `r8d` composite flags, `r9`, `[rsp+0x28]` modelOut,
  `[rsp+0x30]` compositeOut, `[rsp+0x38]` data (≠0 → `0x12d8570`, otherwise
  `0x12d84b0`). Callers: `0x1168d30`, `0x1169d0b` (`AvCharModel.cpp`;
  `AvCharModel+0xc0` agent, `+0x40` model, `+0xe0` composite) and `0x1152930`.
  The ctor runs **inside** that call, so it is possible to test
  `rdx == GetControlledCharacter()` on entry and store it in a thread-local
  `PENDING_OWN`. (A; M for the pointers being the same type: `0x13a3870`
  compares the return value directly with a character pointer.)
- Alternative signal (M/B): `AvCharModel` builds the flags with `X->vt[0x678]()`
  (`0x1169340`, `0x11692d0`). If true, the composite gets `0x13 | 0x200`
  (high quality). If false, it gets `0x12` or `0x1a` (bit 3 = **half resolution**,
  the "high character textures" option of the others). The local player's UI
  passes bit 0: paper doll `0x205` (`0x40aac0`), creation `7` (`0x129cd6a`),
  `CharAppearanceDollButtons` `0xb`, `CompositeUtils` `3`. That is,
  `b1c & 0x201 == 0x201` probably means "local player", but the
  meaning of `vt[0x678]` was not confirmed. Use only as a cross-check
  in the log.
- Character selection: not confirmed whether it goes through `0x1166580` (B).

### 3. Reuse / cache (answer: yes, at three levels)

- **Composite**: one per model (`AvCharModel+0xe0`). It is rebuilt in place
  when the appearance changes and recreated when the model is recreated. Addresses
  are reused after free, so the composite→scale map has to be
  rewritten in the ctor. (A)
- **Atlas** (`0x12d3400`/`0x12d25e0`): global dictionary `0x28a9758`, key
  `(a40, a48, b1c bit3, bit6)`, **only when `a48 != 0`** (data compositions).
  Players (`0x12d16b0`, `a48 = 0`) never share an atlas. If a 4× data composite
  enters the dictionary, another identical one gets the ready 4× atlas (state
  7, no composing). It does not crash, it just spends memory. (A)
- **Pieces** (real danger): `CreateFromFile` looks for an existing loader in
  `0xa70530(0x268b840, file, internalFlags)`. The key is the name hash ^ internal
  flags (`0x19` in composition), **without the dims**. If it finds one, it reuses
  the record (`loader+0x28`) with the dims of whoever created it. Pieces stay alive
  for as long as the deferred job lasts, so two compositions in flight with the same piece
  (e.g. the race's base skin) share the record. With different scales, the 4×
  piece lands in a 2× composite (or the reverse), which gives misalignment or the assert
  "mask smaller". The insert `0xa6d760` accepts duplicate keys (it only counts
  collisions). (A)
- The slot/scratch pool `0x12dd930(dims, fmt, flags, levels, …)` searches by
  dims+format, so different scales do not collide. (A)
- In the DLL there is no per-file-id cache (only `KEEP` and `DENY`). With 4×, the 2× pack fails
  in `scale_ok` and the local player's pieces go to the 4× fallback. (A)

### 4. Hook plan

All or nothing: if a signature is missing or a hook fails, the feature stays
off and everything comes out at 2× (as today). Unknown scale at any point
becomes 2×. Use 4× only if `(flags & 8) == 0` (no half resolution). That way, a
record with width ≥ 4096 ⇔ 4×, with no ambiguity.

| Function (RVA) | Hook | Signature (`mksig.py`) |
|---|---|---|
| `0x1166580` AvChar creation | on entry: `PENDING_OWN = rdx != 0 && rdx == ctrl()`; restore on exit | `48 89 5C 24 08 55 56 57 41 54 41 55 41 56 41 57 48 8D 6C 24 80 48 81 EC` |
| `0x9b1fb0` ContextCollection | call only (`cc = *(ctx+0x98)`; `ctrl = (*cc)[0x68/8](cc)`, checking nulls) | `8B 0D ?? ?? ?? ?? 65 48 8B 04 25 58 00 00 00 BA 10 00 00 00` |
| `0x12d16b0` player ctor | `MAP[rcx] = (PENDING_OWN && !(r8d & 8)) ? 4 : 2` | `48 89 5C 24 08 48 89 6C 24 10 48 89 74 24 18 57 41 54 41 55 41 56 41 57 48 83 EC 40 48 8D 05 ??` |
| `0x12d1b20` data ctor | `MAP[rcx] = 2` (or the same rule, optional) | `48 89 5C 24 08 48 89 6C 24 10 48 89 74 24 18 57 41 54 41 55 41 56 41 57 48 83 EC 30 48 8D 05 ?? ?? ?? ?? 45 33 ED 48 89` |
| `0x12d8fc0` creates atlas | `CUR = MAP[rcx]` | `40 53 48 83 EC 20 48 8B D9 48 8D 4C 24 30 E8 ?? ?? ?? ?? 45 33 C0 48 8D` |
| `0x12d4130` atlas dims | after the original: `out *= CUR` (no constant patch) | already exists (`ATLAS_DIMS`) |
| `0x12d25e0` Update | `CUR = MAP[rcx]` | `40 53 56 48 83 EC 68 8B 81 44 0B 00` |
| `0x12dd540` scratch | `IN_SCRATCH = 1` | already exists (`SCRATCH`) |
| `0x12dd930` CpsTex pool | if `IN_SCRATCH`: dims (`rcx`, 2×u32) × `CUR` | `48 8B C4 48 89 48 08 55 56 41 56 48` |
| `0x12d8950` layer | optional: check `MAP[r8-0xa40] == CUR`; `LOW = arg7 != 0` | `48 89 5C 24 20 55 56 57 41 54 41 55 41 56 41 57 48 81 EC 30 01 00 00 48 8B 05 ?? ?? ?? ?? 48 33 C4 48 89 84 24 20 01 00 00 48 8B 84` |
| `0x12da170` blitRectSet | copy with dims × `CUR`; cache of copies per `(set, CUR)`; stores `EXPECT = dims>>LOW` | already exists (`BLIT_SET`) |
| `0x12dbe90` rectangles | × `CUR` | already exists (`REGION_RECTS`) |
| `0xa70530` loader lookup | if inside `0x12d8950` and the loader found has `rec(+0x28)` with dims `+0x48/+0x4C` ≠ `EXPECT`, return 0 (forced miss) | `48 89 6C 24 20 56 41 56 41 57 48 83 EC 30 4C 8B` |

Read (`diag.rs`, `block_cb`): on a composition load (`ctx+0x30` = loader,
flags `0x19`), `rec = loader+0x28` and `factor = rec[+0x48] >= 4096 ? 4 : 2`. The dims
of the pre-allocated record are the slot of the already-scaled blitRectSet entry (see
Diagnosis 9). Use `factor` in `scale_ok` and in `upscale_file_by`. No state
needed: the record already tells the scale of the composite that created it.

Remove the constant patches of `0x12d4130`/`0x12dd540` (they become hooks).

### Risks / to confirm in game

- Log in the `0x1166580` hook the values `character`, `ctrl`, `ecx` and `r8d`, and
  in the ctor the `flags`. Expected for the local player's character: `ecx = 4`, `r8d = 0x13` (`b1c = 0x213`).
- `0x12d4130` is 34 bytes (the 14-byte retour jmp fits). `0x12dd930` has a
  `mov [rax+8], rcx` right after `mov rax, rsp`; the retour relocates it normally.
- Memory for the local player's character: atlas and scratch 4096² (scratch probably uncompressed,
  2 buffers) and 4096² slots per piece. The CPU blit becomes 4× slower, but only for it.
- A piece that the fallback does not convert (DXT3) still breaks the composition, same as
  at 2×.
- The forced miss creates duplicate loaders (the insert accepts them). Confirm in the log
  how many times it happens.

### Implemented

In-game log confirmed the local character: ctor `0x12d16b0` with flags `0x213` in the
world and `0x3` in character selection (bit 0); other players come with
`0x12`. `GetControlledCharacter` is still 0 when the local player's character is created, so the
rule used is `flags & 1 || character == controlled`, and never 4× with `flags & 8`.

DLL (`composite.rs`, `player_diag.rs`, `diag.rs`):
- `MAP[composite]` written in both ctors **before** the original (the first atlas is
  created inside the ctor). Data ctor = always 2×.
- `CUR` thread-local in `0x12d8fc0` and `0x12d25e0`; atlas `0x12d4130` and pool
  `0x12dd930` (only the call coming from scratch `0x12dd540`) multiply by `CUR`;
  rectangles and `blitRectSet` too (copies per `(set, scale)`). No constant patch;
  if any hook is missing, it falls back to the fixed 2× patch.
- `0xa70530`: inside `0x12d8950`, piece loader (flags `0x19`) with a record of
  another scale → forced miss (counter in the overlay).
- Read: factor by the record's width (≥ 4096 = 4×); 4× pieces come from
  `<pack>\composite_4x` (`index.jsonl` same as the pack's) or from the 4× fallback.
- Config `composite_4x_all` (overlay: "4x for all players on screen"): applies
  to compositions created afterwards (map change), because an atlas's scale cannot
  change after it is created.

Pending: generate `composite_4x` (all composition pieces, all races and
genders: skin, face and armor need the same scale in the atlas). Composition loads
with record `[0,0]` (no slot) stay at 2×; check in the log whether any
belongs to a 4× composite.

## Model textures at 4× only on the local character (2× on the others): static analysis

Goal: head/face, hair/mane, horns, eyes and skin/fur (**model** textures,
not the armor composition) at 4K only on the local player's character and at
2K on the other players, even when the file is the same. Static analysis only
(disassembly with capstone + `dec_grtex`/`dec_cps`). Confidence: **A** = read in the
code, **M** = strong inference, **B** = hypothesis.

### Short answer

The engine **already separates** a character's model textures. The local
player's composite (`b1c & 1`) creates the model with flag `0x200000`. That flag reaches
`ModelFile` as `flags 0x10`, and the material textures are created with
`GR_TEXTURE 0x10` (ignore Texture Detail). That flag changes the loader key (bit `8`),
so the local player's character gets **another loader, another `GrTex2d` record and
another low/high pair**, even with the same file. (A)

No forced miss in `0xa70530` and no thread-local needed: the read hook
already knows the scale from the record (`rec+0x60 & 0x10`). The only sharing
left is that of **merged** `ModelFile`s (see Risks), which can be cut
with two small hooks.

### 1. Creation chain (A, unless indicated)

Model creation (synchronous, inside `0x1166580`):

- `0x1166580` (AvCharModelHelpers) → `0x12d84b0` → ctor `0x12d16b0` →
  `0x12d58d0` (recreates the model) → `0x12d32b0(composite, skeletonFile, flags, …)`.
  In `0x12d58d0`: `b1c & 1` → model flags `|= 0x200040`.
  The local player's `b1c` = `0x213` (world), `0x3` (selection), `0x205` (paper doll). The
  others get `0x12`/`0x1a` (`0x11692d0`: agent `vt[0x678]` → `0x13`;
  otherwise pref 5 chooses `0x12` or `0x1a`). Bit 0 only comes from the local player's character.
- `0x12d32b0` → **`0xcf7000` ModelCreate(file, flags, …)** (ORed with the global
  `0x27f8f2c`, written by `0xcf7110`) → **`0xcf5130` Model::Load**: translates the
  flags into `model+0x90`. **`0x200000` → bit 51 (`0x8000000000000`)**.
- In the same `0xcf5130`, the root file: `0xd02a20(mgr, name, …, key=model+0x150,
  flags = (model+0x90 >> 47) & 0x10, …)`. That is: **ModelFile flags = 0x10 ⇔
  local player's model**.
- Geometry of the pieces (head, hair etc.), in the composite update
  (`0x12d7770`): `model->vt[0x218]` = **`0xcea1d0` Model::AddGeometry** (with the
  global `0xcf7110(flags)` set during the call) or `0x12d3090` →
  `0xcf7000` with the same flags `0x200040`. `0xcea1d0` calls `0xd02a20` per
  file with the same flags `(model+0x90 >> 47) & 0x10`.
- `0xd02a20`: looks up the pack by name (`0xd035f0`, cache by name only) and then the
  `ModelFile` in table `mgr+0x60` with key **(`+0x08` key5, `+0x10` pack,
  `+0x18` flags)** (`0xd02820`). If not found, it creates one with ctor `0xcfec10` (0xC8
  bytes, `+0x70` = stage 1) and puts it in the pending list `mgr+0x110`.
  **The local player's character and the others have different `ModelFile`s for the same file.**

Texture creation (**deferred**, main thread):

- Frame: `ViewAdvance` (`0x94cde3`, string "ViewAdvanceModel") →
  `0xcf63a0`/`0xcf6c40` ("ModelAdvanceFiles") → `0xd01c30` (advances the
  manager's list) → **`0xd00d80` ModelFile::Advance(mf, wait)**. At stage 1, with the
  pack ready (`0xd048a0`), it calls **`0xd05170` ModelFile::LoadMaterials(mf)** and
  moves to stage 2 (`LOAD_STAGE_MATERIAL_LOADING`). It also runs through
  `0xd00570` (synchronous flush with wait, via `0xd00810`/`0xd069c0`) and through
  `MapLoader` (`0x951eab`). The main thread is the same as `GrTex2dAdvance`'s. (M)
- `0xd05170`: for each material texture (0x21-byte entry):
  `flags = mf+0x18`, plus `| 0x30` if the entry has bit 2. Bit 0 = cube
  (`0xabe7a0`, GrTexCube). The rest goes to
  `0xa72110(name, flags, mf+0xc5, mf+0xc0, mf+0x98, 0)` → **`0xa72160`
  GrTex2d CreateFromFile** (generic; `0xa72530` is composition-only) →
  `0xa70670` (key) → `0xa70530` (lookup). Then `0xaa6f10` creates the material and
  attaches it to `mf+0x78/+0x80/+0x8c` (`m_materials`).

That is: the texture is **not** born inside `0x1166580`. It is born frames later,
inside `0xd05170`, where there is no character nor `AvCharModel` at hand, only the
`ModelFile` (shared by everyone with the same flags). But the `ModelFile` already
says whether it is the local player's character (`+0x18 & 0x10`).

### 2. Why it stays separate (A)

In `0xa72160`, with `0x10` in the flags: `flags |= 0x40000`. In `0xa70670`, the
quality becomes 0 and the key gets **`| 8`**:

| Texture | flags (`rec+0x60`) | loader key (`loader+0x10`) | high-res at creation |
|---|---|---|---|
| local player's model | `0x10 \| 0x40000` | `0xe` (or `0xd` if `0xd90480(id)==0`) | yes, if `DAT_142587cac` (metric 0x16, texture LOD) ≠ 0 |
| other players' model | `0` | `6` | no (only later, through streaming `0xa73a53`) |
| entry with bit 2 (no mips) | `0x30` in both | same in both | — |
| composition piece | `0x3036` | `0x19` | — |

`0xa70530` compares hash(name)^key **and** the exact key (`lVar1+0x10 ==
param_3`), so `0xe` never finds the others' loader `6` (nor the reverse).

### 3. Low/high pair (A)

On a miss, `0xa72160` creates: record `0xa6be30(flags)` (`rec+0x60` = raw
flags), low loader **`0xa6c120(mem, name, key, dimsKey, 0, rec, …)`** (which
inserts with `0xa6d760`), `0xab8020` (handle), `0xa6db40` (start) and, if
`local_26c` (= `DAT_142587cac` in the `0x40000` case), **`0xa73890(lowLoader)`**.
That one creates the high loader directly with `0xa6c120(…, key 0x11, 0x420/0x440, 0, rec)`
and stores it in `rec+0x58` (`m_fileHighRes`). `0xa73890` does **not** call
`0xa70530`: the high loader always belongs to the record that requested it. `0xa6e3f0` links
the low one into `rec+0x50`. Both loaders point to the same `rec` (`+0x28`),
so the scale taken from the record is always the same within the pair. That way
`UpRes`/`DownRes` (2×) stays consistent: local player's 2048/4096, others 1024/2048.

Note: the high loader has key `0x11` both on the local player's character and on the others. It does not
matter, because nobody does a lookup with `0x11` for model textures.

### 4. Recognizing it in the read hook (A for the fields, M for the rule)

In `block_cb` (`0xd83710`), with the (`decodeFunc`, loader) pair already validated:

```
loader = job+0x30;  rec = *(loader+0x28);  f = *(u32*)(rec+0x60)
own     = (f & 0x10) && !(f & 0x20) && !(f & 0x2000) && !(f & 0x1000)
          && *(u32*)(loader+0x10) != 0x19        // not a composition piece
high    = *(rec+0x58) == loader;  low = *(rec+0x50) == loader
factor  = own ? 4 : 2                             // the same for low and high
```

No need to capture anything at creation. To check in the log, optional: hook on
`0xd05170` (`rcx` = ModelFile; log the pack name `[[rcx+0x10]+8]` and
`[rcx+0x18]`) and/or on `0xa6c120` (`rcx` = loader, `r8d` key, `[rsp+0x30]` rec,
on entry) to build a set of the local player's loaders. `0x10` without `0x20`
also shows up in a few UI textures (sites `mov edx,0x10`/`add
edx,0x10` of `0xa72110`), but the pack's file id list already filters that out. The
DLL cannot store HD by file id alone: the cache key has to include the
factor.

### 5. Risks / cache

- **Merged `ModelFile` (M for existence, B for frequency)**. The model
  with several pieces is merged (`0xcea1d0` → `0xd029c0`; `0xcf6150` →
  `0xd02bd0`). The merged pack is found by the **combined name of the pieces'
  packs**. The merged `ModelFile` uses the key `(key5 = model+0x150 = token composite+0xa50, pack, flags 0)`: **without flag 0x10**. It is born at stage 3
  and **copies the pieces' `m_materials` references** (loop in `0xd03460`).
  If the local player's character and another player have the same geometry set and the same
  token, whoever arrives later reuses the first one's merged file: the local player's character appears with
  the 2× records or the other with the 4× ones. Sharing only does not happen with
  bit 0 of the flags (`(model+0x90 bit22) == 0 && model+0x3e4 != 0`). The
  `+0x3e4` is only written by `0xcfa680` (vt `+0x210`, data compositions), so
  on a player it stays 0 and sharing happens. Suggested fix: hooks
  `0xcea1d0`/`0xcf6150` (`rcx` = model) → TLS `OWN = (*(u64*)(rcx+0x90) >> 51) & 1`
  (save/restore); `0xd029c0`: if `OWN`, `r9d |= 1` (becomes a miss, no lookup);
  `0xd02bd0`: if `OWN`, 8th arg (`[rsp+0x40]` on entry) `|= 1` (neither looks up nor
  inserts into tables `+0x18`/`+0x60`). It is the same path the game uses for
  "non-shareable" models. Confirm in the log first: count returns ≠ 0 from
  `0xd029c0` with `OWN`.
- **Textures without mips (entry bit 2 → `0x30`)**: the local player's character and the others use the
  same key and the same record. They stay at 2× (the rule of item 4 already excludes them).
  Separating them would require a forced miss in `0xa70530` with TLS coming from `0xd05170`. The
  lookup returns the first in the chain, and the insert accepts duplicates, so the
  others could pick up the local player's loader later. Not recommended (B).
- **Other sources of `0x200000`** (would also become 4× if they are in the pack):
  character creation `ScnCharCreate` (`0x29c510`, `0x200040`); paper doll and
  selection (bit 0); possibly equipment (`AvCharEquipment` `0x113f4c2`,
  variable flags), mount (`AvCharMountModel` `0x116d37b`) and the species
  model `0x1166a80` (`cmove edx, r14d`). All of them belong to the local player (M). The UI
  models `CtlModel` (`0x880040`) do not have the flag.
- **Texture Detail / Reload**: `0xa73440` skips records with `rec+0x60 & 0x10`.
  The local player's character always stays at maximum quality and does **not** reload through the loop of
  item 3 of "Reloading textures in game". For the overlay toggle, include the local player's
  records by hand (`0xa75280(rec, 0)` works: they are loaded from disk, not
  pooled).
- **Other caches**: the material (`0xaa6f10`) receives handles per record
  (`0xab8020`, cache in `rec+0x78`), so it does not merge different records (M).
  Streaming (`0xa73a53` UpRes / DownRes) is also per record. No
  cache that merges records by file id was found. The archive read is one job
  per loader (`job+0x30`). It was not verified whether Archive3 merges simultaneous
  reads of the same id into a single job (B; the `block_cb` log shows this).
- **Rebuild**: a look/equipment change (`0x12d6320` → `0x12d58d0`)
  destroys the model and recreates it with the same `b1c` → the same flags `0x10` →
  `0xd02a20` finds again (or recreates) the `ModelFile 0x10` → `0xa70530` finds again the live
  `0xe` records. The scale does not change and nothing needs to be rewritten (A). On a
  map change everything is destroyed and recreated by `0x1166580` with `0x213`. The
  record holds the scale itself; there is no pointer→scale map to invalidate.
- `DAT_142587cac == 0` (texture LOD off): the local player's character does not request the high one at
  creation, but the rule still holds when it is requested.

### 6. Signatures (unique in the whole exe in this build; checked with a regex on the file)

| Function (RVA) | Use | Args | Signature |
|---|---|---|---|
| `0xd05170` ModelFile::LoadMaterials | optional log/tag | `rcx` ModelFile (`+0x10` pack, `+0x18` flags, `+0x70` stage) | `40 55 41 54 41 55 41 56 41 57 48 81 EC B0 00 00 00 48 8D 6C 24 40 48 89 9D A0 00 00 00 48 89 B5` |
| `0xa72160` GrTex2d CreateFromFile | optional log | `rcx` name, `edx` flags, `r8d`, `r9d`, `[rsp+28]` dims, `[rsp+30]` levels, `[rsp+38..48]` | `40 53 55 56 57 41 54 41 55 41 56 41 57 48 81 EC 98 02 00 00 48 8B 05 ?? ?? ?? ?? 48 33 C4 48 89 84 24 80 02 00 00 8B` |
| `0xa72110` wrapper (96 callers) | — | `rcx` name, `edx` flags | `48 83 EC 68 8B 84 24 98 00 00 00 45 33 D2 89 44 24 40 8B 84 24 90 00 00 00 89 44 24 38 41 8B C2` |
| `0xa6c120` loader ctor | optional capture | `rcx` loader, `rdx` name, `r8d` key, `r9d` dimsKey, `[rsp+28]` p5, `[rsp+30]` rec | `48 89 5C 24 08 48 89 6C 24 10 48 89 74 24 18 57 48 83 EC 20 48 8D 05 ?? ?? ?? ?? 48 8B F9 48 89 01 41` |
| `0xa73890` creates `m_fileHighRes` | — | `rcx` low loader | `48 89 5C 24 08 48 89 74 24 10 57 48 83 EC 50 48 8B 41 28 48 8B F9 48 8B 58 58 48 85 DB 0F 84` |
| `0xa70670` loader key | — | `rcx` name, `edx` flags, `r8` &key, `r9` &dims, `[rsp+28]` &requestsHigh | `41 54 41 57 48 83 EC 38 8B 05 ?? ?? ?? ?? 4D 8B F9 48 89 6C 24 58 4D 8B E0 48 89 74 24 60 48 8B` |
| `0xcea1d0` Model::AddGeometry (vt `+0x218`) | TLS `OWN` | `rcx` model, `edx` count, `r8` names | `40 55 41 54 41 55 41 56 41 57 48 81 EC 90 00 00 00 48 8D 6C 24 50 48 89 5D 70 48 89 75 78 48 89 BD 80 00 00 00 48 8B 05 ?? ?? ?? ?? 48 33 C5 48 89 45 30` |
| `0xcf6150` Model merge | TLS `OWN` | `rcx` model, `edx` | `40 55 41 54 41 55 41 56 41 57 48 81 EC B0 00 00 00 48 8D 6C 24 70 48 89 5D 70 48 89 75 78 48 89` |
| `0xd029c0` merged lookup | `r9d \|= 1` if `OWN` | `rcx` mgr, `rdx` combined name, `r8` key5, `r9d` flags | `48 89 5C 24 08 57 48 83 EC 40 49 8B D8 48 8B F9 41 F6 C1 01 75 0A E8 ?? ?? ?? ?? 48 85 C0 75 0D` |
| `0xd02bd0` creates the merged one | 8th arg `\|= 1` if `OWN` | `rcx` mgr, `rdx` root ModelFile, `r8d` count, `r9` pieces, `[rsp+28]` model+0x3e4, `[rsp+40]` flags | `40 55 41 54 41 55 41 56 41 57 48 81 EC C0 00 00 00 48 8D 6C 24 40 48 89 9D B0 00 00 00 48 89 B5 B8 00 00 00 48 89 BD C0 00 00 00 48 8B 05 ?? ?? ?? ?? 48 33 C5 48 89 45 70` |
| `0xd02a20` ModelFile get/create | optional log | `rcx` mgr, `rdx` name, `[rsp+28]` key5, `[rsp+30]` flags | `40 55 56 41 54 41 55 41 57 48 83 EC 60 44 8B BC 24 D0 00 00 00 48 8B F1 4C 8B A4 24 C8 00 00 00` |
| `0xd00d80` ModelFile::Advance | — | `rcx` ModelFile, `edx` wait | `48 89 5C 24 10 57 48 83 EC 20 48 8B D9 8B FA 8B 49 70 8D 41 FC 83 F8 01 0F 86 ?? ?? ?? ?? 85 C9` |
| `0xcf7000` ModelCreate | optional log | `rcx` file, `edx` flags (`0x200000` = own) | `48 89 5C 24 08 48 89 74 24 18 48 89 7C 24 20 41 54 41 56 41 57 48 83 EC 60 48 8B B4 24 B8 00 00` |
| `0xcf7110` flags global | — | `ecx` flags | `89 0D ?? ?? ?? ?? C3 CC CC CC CC CC CC CC CC CC 48 83 EC 38 8B 05 ?? ?? ?? ?? 85 C0 75 0D 48 89` |

### 7. Plan

1. Read only (no new hook): in `block_cb`, use `factor` from item 4 for the pack's
   device textures (both members of the pair through the same `rec`). Pack:
   `<pack>\model_4x` (or the 4× fallback) for the local player's character and the current 2× pack for
   the others. Log `(file id, key, f, low/high, factor)` to confirm
   `0xe`/`6` in game with another player of the same race nearby.
2. If the log shows the same file id with the local player's character at `6` (the others' record),
   it is the merged `ModelFile`: enable the hooks `0xcea1d0`/`0xcf6150` +
   `0xd029c0`/`0xd02bd0` of item 5.
3. All or nothing, as in composition: if a signature is missing, everything comes out at 2×.

## Crash on non-square device textures served as DDS

- Symptom: `c0000005` write at 0x0 in `Gw2-64+0x18e9c11` (memcpy) called from `+0xb8b122`,
  inside the DDS decoder (`0xb89460`, file type 1 in `img_decode` 0xb160d0).
  File: 49577 (wagon/sign props, 512x256 DXT5 → pack 2048x1024, loader 6, dec_fmt 26 → rec_fmt 38).
- In the logs, all device textures (loader 6/14) successfully served as DDS were square;
  the first non-square one always crashed (3 dumps). Cutting the levels down to the original count did not fix it.
- The original game never uses the DDS path for device textures; the DDS format-conversion path
  fails on non-square images (writes to a nonexistent level of the destination image).
- Fix (`diag.rs::native_format`, `atex.rs::from_dds`): non-square device textures are delivered as
  native ATEX with uncompressed level chunks (`[u32 size incl. field][u32 cflags=0][raw words]`,
  in the order of `atex::decode`). Round-trip test with real files from the pack.
- Also: `composite_load` requires a pre-allocated record (+0x50, width ≠ 0). UI (ATEU) and terrain (ATEP)
  use the same flags 0x19 and were going to the 2× fallback (they lost transparency).

## Streaming: repeated reloads and RAM cache

2-hour session with the mod (diag_files.jsonl): 93,165 HD serves for only 11,895 distinct files
(~8 loads per file, 146 GB decompressed for 19 GB unique). 79 thousand serves were composite pieces (the game rereads all pieces on every
character rebuild), the rest comes from streaming eviction (16,938 downres / 32,875 upres). Peaks of 5,300 serves/min. Each serve opened the
`.dds.zst` and decompressed it on the game's read thread.

Changes in `diag.rs`:
- RAM LRU cache of the final buffer (decompressed, already in ATEX when non-square, and the
  4× generated from the 2× for pieces without a 4× entry), key (origin, file id);
- buffers shared via `Arc` between the cache and the KEEP ring; sampled checksum detects
  whether the engine writes into the served buffer (the entry is discarded and logged);
- `read_pack_file` reads the file in one go and decompresses with the exact frame size;
- `slow` event (>= 4 ms) with preparation time, time inside the engine callback, size,
  cache hit and thread, to measure where the remaining cost is.
- dynamic cache budget (`cache_budget`, reread every 5 s via GlobalMemoryStatusEx):
  min(1/16 of total RAM with a fixed 4 GB cap, free RAM + current cache − reserve), reserve =
  max(2 GB, 15% of total RAM), floor 256 MB. If free RAM drops, the cache shrinks on the next
  insertion. Budget changes > 256 MB go to `gw2remastered_log.txt`.

## Batch pipeline without idling

`tools/batch/run_batch.py`: the GPU stalled on every batch waiting for the CPU (end of the previous batch and packing).
Now the queues are bounded and independent: read-ahead decoding (32 textures), read-ahead
extraction (2 batches), up to 3 batches packing in the background. The CPU tools run at
below-normal priority and use all free cores; the thread that feeds the GPU stays at normal.
Chunks of the same size go in batches of up to 0.11 MPix. Measurement (with the game open, so noisy):
batching helps only on small chunks (64 px: 13 -> 60 chunks/s); from 256 px up, a larger batch only loses
speed. Tile 512 kept. `fsync` on the files before they enter the index (power loss).
Validation: 120 textures against the existing pack, 116 byte-for-byte identical; 4 small ones (<= 256 px)
differ by 46-50 dB. Automatic pause on low disk (< 20 GB one batch at a time, < 5 GB stops).

Batch robustness: once a `ProcessPoolExecutor` worker died (`BrokenProcessPool`, cause not
identified; there was no crash event or trace), the script aborted and `queue.sh` moved on to the next list without
warning. Now the pool is recreated and the texture is redone (tested by killing a worker at two moments: 200/200 done, 196
identical to the pack), native worker crashes go to `tmp/worker_faults.txt` and `queue.sh` retries each step
up to 5 times and does a second pass.

## Real-time reload of NPCs and model textures

Test with `debug_tint`: when turning the remaster off, only the local player's character changed (cyan -> yellow); NPCs and scenery stayed.
`reload.rs` now covers, always after a click and spread across frames:
- **NPC/variant**: same `0x12d6880` (without the `!m_variantToken` asserts, which are in `0x12d4f10`, `0x12d52e0`,
  `0x12d5610`, `0x12d5750`, `0x12d6990`, outside the `0x12d25e0` -> `0x12d7f20`/`0x12d21a0` path). Dedupe by final texture `+0xb48`.
- **Model textures**: loop of `0xa73440` without changing the quality: list of files of `s_fileManager`,
  `file+0x30` = id (confirmed in `0xa73d20`), zeroes `file+0x7c`, calls `0xa75280(rec, 0)`. Filter: record's base file,
  flags without `0x2006`, `+0x94 == 0`, state 7, id in the pack. Rewalks the list every frame, so it does not keep a file pointer.
- After a reload, a larger image no longer goes into `DENY`.
Static analysis; **no in-game validation**. Main risk: `DownRes`/`UpRes` require exactly 2x, so with factor 4 the
`resize.rs` guard may refuse (the current texture stays). Config: `live_reload_npc`, `live_reload_models`.

Reload crash (`m_updateState == COMPOSITE_STATE_DONE`, CpsComposite.cpp(1455)), in `0x12d3e90` (release, called from dtor
`0x12d1e60`): a shared composition has an owner (`0x12d3400` sets `b1c & 0x8000` on whoever creates the dictionary entry and loads the
pieces); the followers only point to the player's `b48/b50` (`ae4 = 0`, state 7, no piece data). Destroying the player outside state 7 with refcount > 1 asserts.
Fix: rebuild only the player (before, the dedupe by `b48` could pick a follower) and a detour on release sets state 7 on the player being rebuilt, since the final texture
is complete and survives through the refcount. Owner already destroyed with followers alive: the appearance stays as it is until recreated. Requests less than 1 s apart are merged.

Memory without duplicates, `diag.rs`: one buffer per cache key, even with simultaneous threads (`BUILDING`/`BUILT`); runtime upscale goes into the cache
(keys 7/8) and, if already done, does not even gather the original file; KEEP ring limited to 1 GB of buffers outside the cache (minimum 64 entries, the decoders may still be reading).
`reload.rs`: a `mem:` line every 30 s counts records per file id, compositions vs. final textures and the mod's RAM usage, to measure engine duplicates in game.

HD off without upscale: with the remaster off, composition pieces go as the original file; the atlas stays enlarged (a piece smaller than the slot is normal: logs have decoded/record 0.5 and 0.25).
No in-game validation. The "mask smaller" assert (`0xa747c0`) only requires mask >= color, so the risk is mixing modes in a composition in flight: pieces change mode 3 s after the click and the rebuild runs after that.
(The `original_when_off` switch was removed: HD off always means the original textures.) Defender: opening a pack file for reading went from ~138 ms to ~0.15 ms with the data folder excluded (same queue running).

Single-file pack: opening one file per texture is what triggers the antivirus; requiring an exclusion on every user is not acceptable. `tools/batch/pack_archive.py` generates `pack.bin` + `pack.idx`
(`GW2RPAK1`, 24-byte records), the DLL opens it once and reads by position (`seek_read`), and loose files still apply while the batch produces. Test in a folder without exclusion with 3,000 textures (300 MB): loose 3.22 ms on first read,
single file 0.035 ms. Not measured: the first opening of a file of tens of GB by the antivirus on a PC without an exclusion.

Pack volumes: `GW2RPAK2` format, `pack_NNNNN.bin` of up to 1.9 GB (common per-file limit on hosts) + `pack.idx`. The DLL only queries the size at startup and opens each volume on first use.
`pack_archive.py add|redo|compact`: `add --new-volume` isolates an update; a loose file of an already-archived texture replaces it, if different; `redo` removes the id from `index.jsonl` so the batch redoes it; `compact` frees the space of replaced textures, volume by volume. Tests: `tools/batch/test_pack_archive.py`.

## HD off = unmodified game

In-game finding: with HD off and the "original pieces" path (pieces left at 1x while the composite atlas stayed 2x/4x) the character select stayed black and the engine
loaded ~10,000 pieces in 55 s without finishing the character. The atlas scale is fixed when the composite is created, so pieces must match it.
Fix: a composite created while HD is off is built at scale 1 (player ctor and data ctor), which is the vanilla layout (1024 atlas), and its pieces stay the original files.
A piece belongs to a vanilla composite when its slot (`rec+0x48`) is narrower than 2048; every other piece goes to a scaled atlas and is HD or runtime-upscaled, HD on or off.
`lookup` refuses cached piece loaders made for another scale (1x/2x/4x). Limit: characters already on screen keep their atlas until the next map load, so toggling HD affects
them then; model textures (scenery, weapons) still switch live. Not validated in game yet.

Live rescale of players: a composite keeps two scales, `MAP` (current) and `HD_MAP` (what it gets with HD on: 4x own / 2x others). When HD is toggled, the reload frame hook calls the
engine's rebuild-all (`0x12d6320`, signature unique in build 209026; vtable +0xb0) on non-variant composites whose current scale differs from the wanted one (HD on -> `HD_MAP`, off -> 1),
after setting `MAP`, so the atlas is recreated at the new scale by the same path the constructor uses (`0x12d58d0` -> `0x12d32b0` -> `0x12d3400` -> `0x12d8fc0`, atlas hook reads `MAP`).
Only players: NPC appearances are shared and keep the scale they were created with. Config `live_rescale=false` disables it. Static analysis only, not validated in game.

HD off for model textures: in game, 16 `UpRes` requests were refused after turning HD off (`2048 -> 1024`, `512 -> 256`): the engine reloads the original (smaller) file, asks `UpRes` for it,
but the GPU texture still has the HD size, and only grow (`UpRes`) or halve (`DownRes`) exist. The guard refused them, so the HD stayed on screen (the local player's hair, scenery).
Fix: model textures served HD are remembered (`SERVED_HD`, file id -> HD width). With HD off, the next read of such a file is answered with the plain upscale of the original at the HD width,
so dims stay consistent and the resize passes; once per file, so after the texture is destroyed (map change) it loads vanilla again. Non-square results go out as ATEX (`native_format`).
Textures never served HD in the session are untouched. Not validated in game yet.

Crash of the live rescale (uptime 1:41): `CModel::UnregisterNotifier failed - notifier not found.` (ModelNotifiers.cpp:32). The rebuild-all (`0x12d6320` -> `0x12d58d0`) unregisters
the composite's observers from the model and recreates it; calling it from the frame hook, repeatedly and while a previous reload was still pending, left an observer not registered with the model.
`live_rescale` is now off by default (opt-in): characters change scale on the next map load; HD off still shows the original look (runtime upscale into the scaled atlas) and model textures switch live.
A cheaper candidate to try later: free and recreate only the atlas (`0x12d3e90` + `0x12d8fc0`), but the model keeps references to the old final textures, so it needs checking before use.
The mod log is now kept as `gw2remastered_log.prev.txt` across launches (a crashed session's log used to be overwritten).

Live toggle without rescale: characters are built at HD size (2x/4x) whether HD is on or off, so the engine never has to change an atlas. HD off: pieces are the original files enlarged
(runtime upscale, no AI); HD on: pieces from the pack. The atlas size is the only thing the engine cannot change, and rebuild-all (`0x12d6320`) crashed, so that is avoided.
Option `vanilla_when_off=true` (overlay checkbox) builds characters made while HD is off with the vanilla atlas; turning HD on then reaches them only after a map load.

Cleanup: the `vanilla_when_off` option and the live rescale (`live_rescale`, rebuild-all `0x12d6320`) were removed. Characters are always built at HD size; HD off sends the original pieces
enlarged (no AI), model textures the original enlarged to the size the engine already holds; everything switches live. Characters of a vanilla-sized atlas (1x) no longer exist.

Memory when HD is turned off: nothing is unloaded from VRAM (the engine only grows or halves a loaded texture; the original enlarged to the held size has the same size), it is freed when the engine destroys the
textures (map load). The mod's RAM cache (up to ~3.7 GB of pack buffers) used to stay: it is now emptied of pack entries (keys 0..=6) when HD goes off, again once the switch delay has passed.
Left-alone textures in a session (12,546 model textures): effects/UI by policy, 1,548 small members of pairs whose big member is already >= 1024 px, BC7X (1,061, mostly scenery), no job for the rest.

## Overlay memory, warm start, reason colors, half-served groups

- **Overlay memory** (`mem.rs`): game RAM (working set, private bytes), game VRAM (DXGI `QueryVideoMemoryInfo` on the largest adapter, loaded from the system `dxgi.dll`: the one in the game folder is ReShade's),
  mod RAM (a counting global allocator: bytes the mod holds on its heap, mostly the RAM cache) and mod VRAM (an estimate: what the served HD files add over the originals for textures the engine still holds, plus the larger atlases).
- **Warm start**: files served are noted (`hot_textures.bin`, saved every minute); the next launch reads them back with 4 below-normal threads into the RAM cache (up to 60% of its budget).
  Measured before: a 27-minute session paid 58.6 s of first-read cost on the engine's loader threads (p50 5.6 ms).
- **Reason colors** (Debug colors): `tools/batch/build_reasons.py` writes `pack/reasons.bin` (one byte per file id). Textures the pack leaves alone, in DXT1/DXT5, are handed back as the original with its colour blocks tinted:
  effect, small member of a pair whose big member is at the target, already at the target, under 128 px, no job, character-piece source. Marked ids are reloaded when the colours or HD go off.
  Of 71,137 such textures: 21,005 effect, 18,117 at the target, 16,840 piece sources, 12,458 small pair members, 2,577 under 128 px, 140 unexplained.
- **Half-served groups**: of 133,930 groups with 2+ members, 25,481 are fully served at one factor (never two factors), 71,657 are in the pack but vanilla together, 34,630 untouched, and **2,162 half served**: their other
  members are mostly BC7X (2,137), which is not packed yet. The engine pairs sizes exactly 2x, so those groups could stay smaller than the original. `tools/batch/check_groups.py` lists their 2,335 served textures in
  `pack/pair_skip.txt`; the mod treats them as denied (original). Rerun it after changing the pack.

RAM cache and half-served pairs, round 2: with HD off the originals enlarged to the held size were cached under keys 7/8 (1,520 files, 1.5 GB in a session) although each is used once; model textures are no longer cached
(composite pieces still are: they are re-read on every rebuild). Refused resizes reached 341,886 in 343 s (the engine retries a refused grow every frame; ~17 textures): 179 distinct requests, mostly `256 -> 1024` and `64 -> 512`
(the big file served, the small one vanilla). The survey misses pairs the engine makes, so `pair_is_consistent` reads the record at serve time (`rec+0x50` small file, `rec+0x58` big file, `file+0x30` id) and
does not serve a file whose other half is not servable. Not validated in game yet.
