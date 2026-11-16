# Nexus Mods page: GW2 Remastered

Text and file list for the mod page. The HD pack files are made by `tools/package_nexus.py` (see its docstring); the
mod itself is the `Textures` zip of the GitHub release. The other pages: [translation](../../translated/docs/nexus-mod.md),
[graphics](../../graphics/docs/nexus-mod.md), [GW2 Enhanced, all three](../../enhanced/docs/nexus-mod.md).

## Files to upload

| File | Category | Description |
|---|---|---|
| `GW2Enhanced-Textures-<version>.zip` (from the GitHub release) | Main files | The mod (`msimg32.dll`) and its installer. **Required**, together with the HD pack. |
| `GW2Enhanced-HD-Textures-part1of4.zip` .. `part4of4.zip` | Main files | HD textures, main pack. **Required.** Download all four parts. |
| `GW2Enhanced-HD-Composite4x-part1of2.zip`, `part2of2.zip` | Optional files | 4x armor and skin for your character. Download both parts. |
| `GW2Enhanced-HD-Character4x.zip` | Optional files | 4x face and hair for your character. |

Upload `SHA256SUMS.txt` too (or paste it in the description), so players can check the downloads.

## Description (BBCode, paste as is)

```
[size=5][b]GW2 Remastered — HD textures for Guild Wars 2[/b][/size]

[size=4][b]Description[/b][/size]
GW2 Remastered replaces Guild Wars 2's textures with upscaled HD versions while you play: 2x for the world, armor, weapons and creatures, and 4x for your character with the optional packs. The game's files are never modified. The HD textures are served from a separate pack, and removing the mod gives you back the original game.

The art style is kept: no recolouring, no restyle, no new materials. Every HD texture is the original texture, enlarged with a super-resolution upscaler (4x-UltraSharp, run locally, no generative AI). Normal maps and masks are resized without any model. Guild Wars 2 still looks like Guild Wars 2, only sharper up close.

[size=4][b]Installation instructions[/b][/size]
[list=1]
[*]Download the mod, [b]GW2Enhanced-Textures[/b], from the Files tab (or from [url=https://github.com/wilssola/GW2Enhanced/releases]GitHub Releases[/url]) and extract it into the game folder (the one with Gw2-64.exe), or run the installer inside it, which finds the folder for you.
[*]Download [b]every part[/b] of the main HD pack from the Files tab and extract them all into the [b]game folder[/b]. You get GW2Remastered\pack\ next to Gw2-64.exe.
[*]Optional: extract the 4x packs the same way (Composite4x: both parts; Character4x).
[*]Start the game and press [b]Shift+Home[/b]. The Textures tab shows how many HD textures were found.
[/list]
To keep the pack on another drive, extract it anywhere and pick that folder in the Textures tab (or set pack_dir= in GW2Remastered\config.ini).
To uninstall, delete msimg32.dll and the GW2Remastered folder from the game folder.

[size=4][b]Main features[/b][/size]
[list]
[*]HD textures for the world, armor, weapons, creatures and NPCs (2x)
[*]Optional 4x armor, skin, face and hair for your character
[*]Live on/off switch in the overlay (Shift+Home): turn it off to see the original textures and compare
[*]Pack on any drive, read from a few large archive files (fast, no thousands of small files)
[*]RAM cache and prefetch of the textures used last session, with a memory guard that frees the cache when the system runs low
[*]Nothing is written to the game's files; the game updates normally
[*]Client-side only: no gameplay change, nothing sent to the game's servers
[/list]

[size=4][b]Requirements[/b][/size]
[list]
[*]Guild Wars 2 (64-bit, DirectX 11)
[*]GW2 Remastered / GW2 Enhanced msimg32.dll ("Textures" or "All" package) from [url=https://github.com/wilssola/GW2Enhanced/releases]GitHub Releases[/url]
[*]About 65 GB free for the main pack (+29 GB and +2 GB for the optional 4x packs). An SSD is recommended.
[*]16 GB of RAM or more is recommended
[/list]

[size=4][b]Links[/b][/size]
[list]
[*]Website: [url=https://gw2enhanced.wilssola.dev]gw2enhanced.wilssola.dev[/url]
[*]Source code and every release: [url=https://github.com/wilssola/GW2Enhanced]GitHub[/url]
[/list]

[size=4][b]Shout outs[/b][/size]
[list]
[*][b]Kim2091[/b], for the 4x-UltraSharp upscaling model (CC BY-NC-SA 4.0). This pack is free and non-commercial because of it.
[*][b]hecomi[/b], whose UnityFurURP inspired the shell fur of the GW2 Enhanced graphics feature
[*]The authors of [b]hudhook[/b] and [b]Dear ImGui[/b], used for the in-game overlay
[*]ArenaNet, for Guild Wars 2
[/list]

[size=2]Unofficial fan project. Not affiliated with or endorsed by ArenaNet or NCSOFT. Third-party programs that hook the game client are used at your own risk; read ArenaNet's policy on third-party programs before using it. Guild Wars 2 © ArenaNet, LLC. All rights reserved. The textures are derived from the game's own textures and are distributed only as a mod for owners of the game.[/size]
```

## Generative AI usage

Tags: **Mod content: AI Assisted** (AI texture upscaling); **Media: AI-Generated** (logo and page text).

Describe how AI assisted you:

> The HD textures are the game's original textures upscaled with the 4x-UltraSharp super-resolution model (by Kim2091),
> run locally. It only enlarges existing textures: nothing new is painted or generated, and the art style is unchanged.
> Normal maps and masks are resized without any model.

## Images

`images/remastered-card.png` (16:9, page header and thumbnail) and `images/remastered-banner.png` (1280x400,
transparent background).
