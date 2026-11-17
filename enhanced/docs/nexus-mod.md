# Nexus Mods page: GW2 Enhanced

Text and file list for the mod page of GW2 Enhanced, the three features in one mod. The files come from the GitHub
release. Each feature also has its own page: [translation](../../translated/docs/nexus-mod.md),
[HD textures](../../remastered/docs/nexus-mod.md), [graphics](../../graphics/docs/nexus-mod.md).

## Files to upload

| File | Category | Description |
|---|---|---|
| `GW2Enhanced-All-<version>.zip` | Main files | The whole mod: translation (every translator mode), HD textures runtime and graphics, with the installer. |
| `GW2Enhanced-Installer-windows-x64.exe` | Main files | The installer and launcher alone: it downloads and installs the package you pick, checks its signature, and starts the game. |
| `GW2Enhanced-Installer-linux-amd64.AppImage` | Optional files | The installer and launcher for Linux (Steam Proton, Wine, Lutris). |

Upload `SHA256SUMS.txt` too (or paste it in the description), so players can check the downloads. The HD texture pack
itself stays on the GW2 Remastered page.

## Description (BBCode, paste as is)

```
[size=5][b]GW2 Enhanced — translation, HD textures and graphics for Guild Wars 2[/b][/size]

[size=4][b]Description[/b][/size]
GW2 Enhanced is one mod with three features that can be used together or alone, each with its own switch in one in-game overlay:
[list]
[*][b]Translation[/b]: Guild Wars 2 in Brazilian Portuguese while you play, with what is still missing translated on your own PC, offline.
[*][b]HD textures[/b]: upscaled versions of the game's own textures (2x, and 4x for your character), read from a separate HD pack, with the art style kept.
[*][b]Graphics[/b]: real HDR output, bloom, ambient occlusion, reflections, sun shafts, contact shadows, lens effects, colour grading, shell fur and a photo mode, inside the game's own renderer and before the UI.
[/list]
Everything is a single file, msimg32.dll, placed next to Gw2-64.exe. The game's files are never modified, and removing the DLL gives you back the original game.

[size=4][b]Installation instructions[/b][/size]
[list=1]
[*]Close the game.
[*]Run [b]GW2Enhanced-Installer.exe[/b] (from the Files tab, or inside the All zip). It finds the game on Steam, Epic Games or the ArenaNet client, lists the packages with their size, installs the one you pick and starts the game through the right store. Or extract the All zip into the game folder by hand (the one with Gw2-64.exe).
[*]For HD textures, download the HD pack from the GW2 Remastered page and extract every part into the game folder.
[*]Start the game and press [b]Shift+Home[/b]. The overlay has one tab per feature and a [b]Health[/b] tab that shows whether each part of the mod is working.
[/list]
If another mod already has a msimg32.dll in the game folder, the installer asks before replacing it and puts it back when you uninstall.
To uninstall, run the installer with --uninstall, or delete msimg32.dll from the game folder.

[size=4][b]Main features[/b][/size]
[list]
[*][b]Three features in one DLL[/b], each with its own on/off switch and hotkey (Shift+Home by default)
[*][b]Health tab[/b]: every part of the mod and whether it is working, with the reason when something is off
[*][b]Isolation[/b]: if a game patch breaks one feature, it turns itself off and says why; the others keep running
[*][b]Signed automatic updates[/b]: a new release is downloaded only after you accept it, checked against its signature, and your settings and translations are kept
[*]Logs off by default, one switch per feature when you need to report a problem
[*]Overlay in English or Portuguese
[*][b]Installer and launcher[/b] for Windows and Linux
[*][b]Client-side only[/b]: no gameplay change, nothing sent to the game's servers
[/list]

[size=4][b]Requirements[/b][/size]
[list]
[*]Guild Wars 2 (64-bit, DirectX 11), on Windows 10 or 11, or on Linux through Steam Proton, Wine or Lutris (launch option WINEDLLOVERRIDES="msimg32=n,b" %command%, which the installer can add)
[*]HD textures: the HD pack from the GW2 Remastered page (about 65 GB, up to 95 GB with the optional 4x packs); 16 GB of RAM and an SSD are recommended
[*]Local translation of missing text: the game in English. Graphics card translator: any DirectX 12 card (or NVIDIA for CUDA)
[*]HDR output: an HDR monitor with Windows HDR turned on
[/list]

[size=4][b]Links[/b][/size]
[list]
[*]Website: [url=https://gw2enhanced.wilssola.dev]gw2enhanced.wilssola.dev[/url]
[*]HD texture pack: [url=https://www.nexusmods.com/guildwars2/mods/10]GW2 Remastered on Nexus Mods[/url]
[*]Source code, documentation and every release: [url=https://github.com/wilssola/GW2Enhanced]GitHub[/url]
[/list]

[size=4][b]Shout outs[/b][/size]
[list]
[*][b]Kim2091[/b], for the 4x-UltraSharp upscaling model (CC BY-NC-SA 4.0)
[*][b]OPUS-MT[/b] (Helsinki-NLP) and [b]Argos Translate[/b], for the translation models (CC-BY 4.0); [b]CTranslate2[/b] and [b]ONNX Runtime[/b] / [b]DirectML[/b], which run them
[*][b]hecomi[/b], whose UnityFurURP inspired the shell fur
[*]The authors of [b]hudhook[/b] and [b]Dear ImGui[/b], used for the in-game overlay
[*]Everyone who votes and suggests translations on the website
[*]ArenaNet, for Guild Wars 2
[/list]

[size=2]Unofficial fan project. Not affiliated with or endorsed by ArenaNet or NCSOFT. Third-party programs that hook the game client are used at your own risk; read ArenaNet's policy on third-party programs before using it. Guild Wars 2 © ArenaNet, LLC. All rights reserved. The HD textures are derived from the game's own textures and are distributed only as a mod for owners of the game.[/size]
```

## Generative AI usage

Tags: **Mod content: AI Generated** (machine translation) and **AI Assisted** (texture upscaling); **Media:
AI-Generated** (page text).

Describe how AI was used:

> The pt-BR translation dictionary was machine translated from the game's English text with NVIDIA Nemotron and is
> improved by players' reviewed votes and suggestions; missing text is translated on the player's PC by OPUS-MT, an
> offline neural machine translation model. The HD textures are the game's own textures upscaled with the
> 4x-UltraSharp super-resolution model, run locally: nothing new is painted or generated. The graphics use no AI.

## Images

`images/enhanced-card.png` (16:9, page header and thumbnail) and [`../../docs/images/logo.png`](../../docs/images/logo.png)
(1280x400, transparent background). The installer screenshots in `docs/images/installer-*.webp` and the HD comparisons
in `docs/images/hd-*.jpg` fit the gallery.
