# Nexus Mods page: GW2 Translated

Text and file list for the mod page of the translation on its own. The files are the `Translation` zips of the GitHub
release (built by `scripts/package.ps1`). The other pages: [HD textures](../../remastered/docs/nexus-mod.md),
[graphics](../../graphics/docs/nexus-mod.md), [GW2 Enhanced, all three](../../enhanced/docs/nexus-mod.md).

## Files to upload

| File | Category | Description |
|---|---|---|
| `GW2Enhanced-Translation-CPU-<version>.zip` | Main files | Translator on the processor. Smallest download, works on every PC. **Pick one of the four.** |
| `GW2Enhanced-Translation-DirectML-<version>.zip` | Main files | Translator on any DirectX 12 graphics card (AMD, Intel, NVIDIA): faster, larger model. |
| `GW2Enhanced-Translation-CUDA-<version>.zip` | Main files | Translator on NVIDIA cards (CUDA): fastest, larger model. |
| `GW2Enhanced-Translation-<version>.zip` | Main files | Every translator mode in one download; the overlay picks the best one. |

Upload `SHA256SUMS.txt` too (or paste it in the description), so players can check the downloads.

## Description (BBCode, paste as is)

```
[size=5][b]GW2 Translated — Guild Wars 2 in Brazilian Portuguese[/b][/size]

[size=4][b]Description[/b][/size]
GW2 Translated puts Guild Wars 2 in [b]Brazilian Portuguese (pt-BR)[/b] while you play: quests, dialogue, items, skills, menus, the Trading Post, the Gem Store and the Wizard's Vault. The text is replaced in memory at the moment the game builds it. Gw2.dat is never modified, and removing the mod gives you back the original game.

It ships with a dictionary of about 205 thousand strings, kept up to date in the background with reviewed translations from the project's website. What the dictionary does not have yet is translated on your own PC, offline, by a small local translator that runs at low priority on the processor or on the graphics card. Map dialogue is translated before it appears, and a speech bubble stays up a little longer while its translation is being made.

[i]Em português:[/i] o GW2 Translated traduz o Guild Wars 2 para o português do Brasil enquanto você joga, sem alterar os arquivos do jogo. O que ainda não está no dicionário é traduzido no seu próprio PC, offline. Você pode votar e sugerir traduções melhores no site do projeto, sem criar conta.

[size=4][b]Installation instructions[/b][/size]
[list=1]
[*]Close the game.
[*]Download [b]one[/b] of the Translation packages from the Files tab ([b]CPU[/b] if unsure).
[*]Run [b]GW2Enhanced-Installer.exe[/b] from the zip: it finds the game folder (Steam, Epic Games or the ArenaNet client) and installs the mod. Or extract the zip into the game folder by hand (the one with Gw2-64.exe).
[*]Start the game. The first-run setup appears in the overlay ([b]Shift+Home[/b]).
[/list]
If another mod already has a msimg32.dll in the game folder, the installer asks before replacing it and puts it back when you uninstall.
To uninstall, run the installer with --uninstall, or delete msimg32.dll from the game folder.

[size=4][b]Main features[/b][/size]
[list]
[*]About [b]205 thousand translated strings[/b], updated in the background (signed releases, checked before use)
[*]Translations of [b]any length[/b]: no text cut to fit the English space
[*]Works with [b]any client language[/b]: everything that has a translation shows in pt-BR, the rest stays in the client's language
[*]Missing text translated [b]on your PC, offline[/b] (processor or graphics card), at low priority
[*][b]Trading Post, Gem Store and Wizard's Vault[/b] pages translated in place
[*][b]F10[/b] reapplies the translation to the text already on screen
[*]In-game overlay with the options, the first-run setup and the translator's metrics
[*][b]Patch resilient[/b]: if a game patch changes something, only that part turns itself off and says why; the game keeps working
[*][b]Client-side only[/b]: no gameplay change, nothing sent to the game's servers
[/list]

[size=4][b]Privacy[/b][/size]
Player chat is never translated, saved or sent. Helping the project is optional and asked on first use: if you agree, the mod sends the game texts that have no translation yet and the translations made on your PC, never personal data. It can be turned off at any time in the overlay ("Help improve the translation").

[size=4][b]Requirements[/b][/size]
[list]
[*]Guild Wars 2 (64-bit, DirectX 11), on Windows 10 or 11. On Linux (Steam Proton, Wine, Lutris) use the Linux installer from GitHub; the game needs the launch option WINEDLLOVERRIDES="msimg32=n,b" %command%, which the installer can add.
[*]The local translation of missing text needs the game in English (the dictionary works with any client language)
[*]DirectML package: any DirectX 12 graphics card. CUDA package: an NVIDIA graphics card.
[/list]

[size=4][b]Links[/b][/size]
[list]
[*]Website, where you can vote and suggest translations: [url=https://gw2enhanced.wilssola.dev]gw2enhanced.wilssola.dev[/url]
[*]Source code and every release: [url=https://github.com/wilssola/GW2Enhanced]GitHub[/url]
[*]GW2 Enhanced, the same translation together with HD textures and graphics in one mod: see its own page
[/list]

[size=4][b]Shout outs[/b][/size]
[list]
[*][b]OPUS-MT[/b] (Jörg Tiedemann, Santhosh Thottingal and the Helsinki-NLP team) and [b]Argos Translate[/b], for the translation models (CC-BY 4.0)
[*][b]CTranslate2[/b] (OpenNMT) and [b]ONNX Runtime[/b] / [b]DirectML[/b] (Microsoft), which run the local translator
[*]The authors of [b]hudhook[/b] and [b]Dear ImGui[/b], used for the in-game overlay
[*]Everyone who votes and suggests translations on the website
[*]ArenaNet, for Guild Wars 2
[/list]

[size=2]Unofficial fan project. Not affiliated with or endorsed by ArenaNet or NCSOFT. Third-party programs that hook the game client are used at your own risk; read ArenaNet's policy on third-party programs before using it. Guild Wars 2 © ArenaNet, LLC. All rights reserved.[/size]
```

## Generative AI usage

Tags: **Mod content: AI Generated** (machine translation); **Media: AI-Generated** (page text).

Describe how AI was used:

> The pt-BR dictionary was machine translated from the game's English text with NVIDIA Nemotron, a language model, and
> is improved over time by players' votes and suggestions on the project's website, which go through a review before
> they are used. Text missing from the dictionary is translated on the player's PC by OPUS-MT, an offline neural
> machine translation model.

## Images

`images/translated-card.png` (16:9, page header and thumbnail) and `images/translated-banner.png` (1280x400,
transparent background). In-game screenshots of translated quests, menus and the Trading Post help most.
