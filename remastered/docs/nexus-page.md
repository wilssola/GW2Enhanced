# Nexus Mods page: GW2 Enhanced HD Textures

Text and file list for the mod page. The files are made by `tools/package_nexus.py` (see its docstring).

## Files to upload

| File | Category | Description |
|---|---|---|
| `GW2Enhanced-HD-Textures-part1of4.zip` .. `part4of4.zip` | Main files | HD textures, main pack. **Required.** Download all four parts. |
| `GW2Enhanced-HD-Composite4x-part1of2.zip`, `part2of2.zip` | Optional files | 4x armor and skin for your character. Download both parts. |
| `GW2Enhanced-HD-Character4x.zip` | Optional files | 4x face and hair for your character. |

Upload `SHA256SUMS.txt` too (or paste it in the description), so players can check the downloads.

## Description

**GW2 Enhanced HD Textures** replaces Guild Wars 2's textures with AI-upscaled versions (2x for the world, armor,
weapons and creatures; 4x for your character with the optional packs), while you play. The game's files are
never modified: the HD versions are served from this pack by the GW2 Enhanced mod, and removing the mod restores the
original game.

**The game's art style is kept**: no recolouring, no restyle, no new materials. Every HD texture is the original one,
enlarged with a super-resolution upscaler (4x-UltraSharp, run locally; no generative AI, no prompts). Normal maps and
masks are resized without any model. Guild Wars 2 looks like itself, only sharper up close.

4x-UltraSharp by Kim2091 (CC BY-NC-SA 4.0): this pack is free and non-commercial.

### Requirements

- The **GW2 Enhanced** mod (`msimg32.dll`), "All" or "Textures" package, from
  https://github.com/wilssola/GW2Enhanced/releases
- About 65 GB free for the main pack (+29 GB and +2 GB for the optional 4x packs). An SSD is recommended.

### Installation

1. Install GW2 Enhanced (extract it into the game folder, the one with `Gw2-64.exe`).
2. Extract **every part** of the pack into the **game folder**. You get `GW2Remastered\pack\` next to `Gw2-64.exe`.
   Extract the optional 4x packs the same way (they go into `GW2Remastered\pack\composite_4x` and `character_4x`).
3. Start the game. The Textures tab of the overlay (**Shift+Home**) shows how many HD textures were found.

To keep the pack on another drive, extract it anywhere and pick that folder in the Textures tab (or set
`pack_dir=` in `GW2Remastered\config.ini`).

### Notes

- The first minutes in a new map can stream textures a bit slower while the pack is read from disk.
- Turning the HD textures off in the overlay goes back to the original textures.
- Unofficial fan project, not affiliated with or endorsed by ArenaNet or NCSOFT. Client-side only; use at your own risk.
  Guild Wars 2 © ArenaNet, LLC. All rights reserved. The textures are derived from the game's own and are distributed
  only as a mod for owners of the game.
