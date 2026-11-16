# Nexus Mods page: GW2 Graphics

Text and file list for the mod page of the graphics on their own. The file is the `Graphics` zip of the GitHub release
(built by `scripts/package.ps1`). The other pages: [translation](../../translated/docs/nexus-mod.md),
[HD textures](../../remastered/docs/nexus-mod.md), [GW2 Enhanced, all three](../../enhanced/docs/nexus-mod.md).

## Files to upload

| File | Category | Description |
|---|---|---|
| `GW2Enhanced-Graphics-<version>.zip` | Main files | The mod (`msimg32.dll`) and its installer. |

Upload `SHA256SUMS.txt` too (or paste it in the description), so players can check the download.

## Description (BBCode, paste as is)

```
[size=5][b]GW2 Graphics — real HDR and lighting for Guild Wars 2[/b][/size]

[size=4][b]Description[/b][/size]
GW2 Graphics works [b]inside the game's own renderer[/b]. It recognises the engine's render passes and adds its effects between them, using the engine's real depth buffer and [b]before the UI is drawn[/b], so nameplates, the HUD and text are never blurred, graded or bloomed. The game's files are never modified, and removing the mod gives you back the original game.

The scene is rendered in 16-bit floating point instead of 8 bits, so fire, spells and the sun can be brighter than white all the way to an [b]HDR monitor[/b] (scRGB or HDR10). On an SDR monitor you get the same effects with the game's usual range.

No neural network and no vendor-specific API: plain Direct3D 11 shaders, on any graphics card the game runs on. Every effect has its own switch and slider in the overlay, applied live, and an effect switched off costs nothing.

[size=4][b]Installation instructions[/b][/size]
[list=1]
[*]Close the game.
[*]Download the Graphics package from the Files tab.
[*]Run [b]GW2Enhanced-Installer.exe[/b] from the zip: it finds the game folder (Steam, Epic Games or the ArenaNet client) and installs the mod. Or extract the zip into the game folder by hand (the one with Gw2-64.exe).
[*]Start the game and press [b]Shift+Home[/b] to open the overlay and its Graphics tab.
[/list]
If another mod already has a msimg32.dll in the game folder, the installer asks before replacing it and puts it back when you uninstall.
To uninstall, run the installer with --uninstall, or delete msimg32.dll (and the GW2Graphics folder) from the game folder.

[size=4][b]Main features[/b][/size]
[list]
[*][b]Real HDR output[/b] (scRGB, or HDR10 as an option), with paper white, peak brightness and calibration patterns
[*]HDR bloom from the scene's true brightness
[*]Ambient occlusion, contact shadows and sun shafts from the engine's depth
[*]Screen-space reflections on water and wet or polished surfaces
[*]Light halo, aerial perspective and local tone mapping
[*]Auto exposure that adapts like an eye going from a cave into daylight
[*]Lens streaks and flares only on compact light sources
[*]Colour grading with presets and LUTs, sharpening, anti-aliasing and anisotropic filtering
[*]Shell fur: real strands of fur on furred characters and creatures, with rim light and soft subsurface light on skin
[*]Photo mode: full-resolution PNG and linear HDR (.hdr) captures, optionally without the UI
[*]Every effect on its own switch, live, plus one switch to compare with the original game
[*]Runs next to ReShade
[*]Client-side only: no gameplay change, nothing sent to the game's servers
[/list]

[size=4][b]Requirements[/b][/size]
[list]
[*]Guild Wars 2 (64-bit, DirectX 11), on Windows 10 or 11. On Linux (Steam Proton, Wine, Lutris) use the Linux installer from GitHub; the game needs the launch option WINEDLLOVERRIDES="msimg32=n,b" %command%, which the installer can add.
[*]For HDR output: an HDR monitor with Windows HDR turned on
[/list]

[size=4][b]Testers wanted[/b][/size]
The graphics need players who can try them on different graphics cards, monitors (SDR and HDR) and maps, and say what looks wrong or which settings look best. Reports are welcome in the Posts and Bugs tabs or as a "Graphics calibration report" on GitHub.

[size=4][b]Links[/b][/size]
[list]
[*]Website: [url=https://gw2enhanced.wilssola.dev]gw2enhanced.wilssola.dev[/url]
[*]Source code, documentation of every effect and every release: [url=https://github.com/wilssola/GW2Enhanced]GitHub[/url]
[*]GW2 Enhanced, the same graphics together with the translation and HD textures in one mod: see its own page
[/list]

[size=4][b]Shout outs[/b][/size]
[list]
[*][b]hecomi[/b], whose UnityFurURP inspired the shell fur
[*]The authors of [b]hudhook[/b] and [b]Dear ImGui[/b], used for the in-game overlay
[*]ArenaNet, for Guild Wars 2
[/list]

[size=2]Unofficial fan project. Not affiliated with or endorsed by ArenaNet or NCSOFT. Third-party programs that hook the game client are used at your own risk; read ArenaNet's policy on third-party programs before using it. Guild Wars 2 © ArenaNet, LLC. All rights reserved.[/size]
```

## Generative AI usage

Tags: **Media: AI-Generated** (page text). The mod's content (shaders, effects) uses no AI.

## Images

`images/graphics-card.png` (16:9, page header and thumbnail) and `images/graphics-banner.png` (1280x400, transparent
background). Before / after screenshots made with the photo mode's with/without-mod pair show the effects best.
