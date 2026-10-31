# Reverse engineering `Gw2-64.exe`

How the Guild Wars 2 text system works internally and where the translation feature
hooks into it. Analyzed with REA/Ghidra (`Gw2-64.exe`
`sha256 99342078…0D925D8621`), starting from a reference DLL
(`sha256 0a883316…0840678`) and the pseudocode of the 136 functions of the
text subsystem.

RVAs belong to this game version and change with every patch; the DLL never
uses a hardcoded RVA. It finds each function by signature (`src/sig.rs`) and
reads field offsets from the instructions themselves (`src/layout.rs`).
`check_exe` verifies everything against a new exe (see
`after-a-game-patch.md`).

## Points used by the DLL

| Point | RVA | Purpose |
|---|---|---|
| `F_materialize` | `0x10e0840` | Translation: replaces the decoded text. |
| `Reserve` | `0x257360` | Grows the text array with the game's allocator. |
| `reg-insert` | `0x10ddb20` | `seed_decode`: encrypted entries of each loaded file. |
| `bulk_insert` | `0x10e7600` | `seed_decode`: keys sent by the server. |
| `cachetick` | `0x10dd520` | Refresh: runs every frame, on the game thread. |
| `evict` | `0x10dd0c0` | Refresh: discards a string from the cache. |
| `frame_rebuild` | `0x1073200` | Refresh: rebuilds the UI. |

### `F_materialize` — `TextDecode.cpp`

`fn(rcx, ctx, textId: u32, entry: *strs, keySeed: u64) -> *u16`

Decodes a `strs` entry into the `TArray<u16>` at `ctx+0x268` and returns the
pointer. Array fields: `+0x08` pointer, `+0x10` capacity, `+0x14` count,
`+0x18` growth step. **The count includes the trailing `NUL`.**

- Null entry or `size < 6` → empty string.
- Plain (`baseChar == 0`, `rangeBits == 16`): copies the UTF-16,
  `Reserve(n+1)`, writes `0`, `count = n + 1`.
- Encrypted with a seed: RC4 and symbol unpacking (see `src/rc4.rs`).
- Encrypted **without** a seed: empty string.

The only caller (`0x10e0b50`, `fn(cparser, textId, entry)`) looks up the seed
in the `ctx+0x200` map by `textId`, calls `F_materialize`, and hands the
result to the `CParser` (`0x10e5c20`), which resolves plurals, `%str1%`, etc.
With `[ctx+0x224] != 0` the game enters a debug mode that shows
`[ID: text]` for every string.

**The DLL** calls the original, reads the text, looks up the translation and,
if there is one, calls the game's `Reserve` and writes the pt-BR text into the
same array: any length, in the game's memory.

### `strs` entry

```text
u16 size         // includes the 6 header bytes
u16 baseChar     // 0 with rangeBits 16 = plain UTF-16LE
u8  rangeBits    // bits per symbol
u8  -
u8  payload[size - 6]
```

Encrypted: `keySeed` (8 bytes) → SHA-1-like mixing → 20-byte RC4 key → RC4 →
`rangeBits`-bit symbols (`0` terminates; `1..31` map through the table
`0123456strnum()[]<>%#/:-'" ,.!\n`; anything else is
`symbol + baseChar - 32`). Keys are random (37–45 bits), one per string, and
only the server has them.

### Text files and `TextPackManifest`

The manifest is a `PF` file of type `txtm` in `Gw2.dat` (file_id `3821681` in
this version). At `0x1C`, `u32 stringsPerFile` (1024); at `0x20`, the
language list (`u32 count, u32 rel`), 12 bytes each (`u32 count, u64 rel`);
each file is an 8-byte relative pointer to an `ANetFileReference`
(3×`u16`, file_id = `(a-0x100) + 0xFF00*(b-0x100) + 1`).

`textId = i * 1024 + entry index`, where `i` = the file's position in the
language's list. Languages: 0 English, 1 empty, 2 French, 3 German,
4 Spanish, 5 Chinese. The ID is the same in every language, which is what
makes translation by ID possible.

The `[null]` string is literal text in the files (ArenaNet's marker for a
removed string); the DLL never uses it in place of real text.

### `reg-insert` and `bulk_insert` (`seed_decode`)

- `strs` loading (`0x10dd778`): walks **all** entries in the file and calls
  `reg-insert(cache, language, textId, entry)`, which copies the entry into
  the text cache.
- `bulk_insert(rcx, msg)` is the handler of a server message
  (`{descriptor, handler}` table at `0x213c4f8`): `msg+2` = count (`u8`),
  `msg+3` = pointer to 12-byte records `{u32 textId, u64 keySeed}`, stored in
  the `ctx+0x200` map. `0x10e7730` clears that map.

The DLL matches both sides on its own thread and decrypts with its own RC4,
without calling game code and without requesting anything from the server.

### Refresh

- `cachetick(cache, ms)` runs every frame and, with `evict(node, 1)`,
  discards the nodes in the `[cache+0x48]` list (link at `[cache+0x38]`) with
  `[node+0x30] != 0`, `[node+0x1C] == 0` and more than 60 s unused.
- `frame_rebuild(language, flags)` writes the language to a global
  (`0x25b9858`) and sends message `0x47`. It requires the `[0x25b985c] & 1`
  flag (UI ready); without it, it hits an assert.

The DLL's refresh runs inside the `cachetick` hook: it discards every free
node and calls `frame_rebuild` with the current language, only when the UI is
ready.

## Other known points (unused)

- **Decompressor** `0xdbc0d0` (`FileArchive.cpp`): delivers blocks of up to
  64 KiB to the `[this+0x50]` callback (default `0xdbc9c0`, copies to
  `[ctx+0x100]`). Translating at the file level limits the size and does not
  reach encrypted text (58% of the entries).
- **Pending text queue** `0x289eac0` (lock `0x289eae0`): records
  `{slot, textId, subscriber}`; `0x10dee20` enqueues and `0x10dead0` delivers
  when the key arrives.
- Game exit (`MainStartCleanup` `0xe357e0`), plurals (`0x10e5c20`) and the
  language setters (`0x2420a0`, `0x101d650`, `0x10dcc80`, `0x1411ae0`) were
  identified but are not needed.

## Tools

- REA with Ghidra. The 43 MB exe needs a high
  `REA_GHIDRA_STARTUP_TIMEOUT_MS` (analysis takes ~15 min).
- Analyses, decompilations and copies of the exe are kept in `re/`, at the repo root (outside
  git).

## Web pages (CEF)

`libcef.dll` (CEF 103.0.10, `bin64\cef\`) is delay-loaded by `Gw2-64.exe`, which is CEF's browser process; `CefHost.exe` runs the renderers. The pages come from `https://<page>-<env>.ncplatform.net/` and their static files from `2<page>.staticwars.com`. Each page's text is in a bundle like `gen/i18n/strings-en.<hash>.js` (`window.tp_strings`, `window.gemstore_strings`, `window.strings`). The bundles of a session are listed in the CEF cache, `%TEMP%\gw2cache-*\user\Cache\Code Cache\js`. Struct offsets in `src/web.rs` come from CEF's C headers (branch 5060, `include/capi/`).

## Chat (`ChatLog.cpp`, `ChatLogEntry.cpp`, `AsChatBubble.cpp`)

Analyzed with REA/Ghidra (`re/gw2dec_ui`, 1,187 functions).

| Point | RVA | What it is |
|---|---|---|
| control handler | `0x43b6b0` | `fn(msg, a, b)`: `msg+4` = type; 10 creates the `ChatLog` (0x78 bytes, vtable `0x1aed9f0`), 0xE destroys and frees it; `**(msg+8)` = object. |
| `add-entry` | `0x43b3e0` | `fn(chatlog, message)`: builds one line. |
| `rebuild` | `0x43bfc0` | Clears the lines and calls `add-entry` for each stored message. This is what the game uses when a chat option changes. |
| literal channel | `0x12a9870` | `fn(channel) -> 0/1`: 0 for 2, 3, 4, 13, 14 (text is a reference to a game string), 1 for the rest (typed text). |
| line | `0x43f930` | Builds the line text with the text API: `0x10dbdd0(textId)`, `0x10dbde0(id, 7, literal, 0)`, `0x10dc110(a, b)` (concatenates), `0x4404d0(color, t)`. |

Stored message (0x40 bytes): `+0x04` channel, `+0x10` `codedText`
(string reference for NPCs; typed text for players).

- `0x12a5a10` creates the default tabs from text IDs `0xbcdf` (Main) and `0xc127` (Combat), through `0x12ae680`: a `0x48`-byte tab (vtable `0x217fdf0`, name pointer at `+0x10`, 15 characters at most) whose name arrives later in the callback `0x12aede0`, which calls `set_name` (vtable `+0x90`).
- `0x12ae5f0` creates a saved tab with the name as plain text. Both paths go through `0x12a7e70`, which adds the tab to the manager.
- The line stores the reference, not the final text: rebuilding the tab after
  a refresh decodes the NPC line again, already translated.
- The speech bubble (`AsChatBubble.cpp`, ~`0x3bb000`) receives the text once
  and does not request it again; that is why dialogue is translated ahead of
  time (pre-translation queue).
