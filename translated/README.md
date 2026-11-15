# GW2 Enhanced: Translation

A plug-and-play translation of **Guild Wars 2**, the translation feature of [GW2 Enhanced](../README.md) (crate `gw2translated`, formerly the separate mod GW2 Translated). Currently available in **Brazilian Portuguese** (pt-BR).

Compiled into the mod's single proxy DLL, `msimg32.dll`, placed in the game folder, it replaces the text in memory, at the moment the game builds it. `Gw2.dat` is never modified.

- **pt-BR dictionary** with ~205k strings, translated with Nemotron, **kept up to date**: the reviewed translations from the project's server are downloaded in the background (signed releases, checked before use).
- **Any length**: a translation does not have to fit in the space of the English text.
- **Protected strings** (RC4) are translated too, as soon as the server releases their key.
- **Any client language**: with the game in Spanish, for example, everything that has a translation shows up in pt-BR and the rest stays in Spanish (translation by ID).
- **Local translation of whatever is missing**, on your PC and offline, on the CPU or the graphics card. Map dialogue is translated before it appears, and a speech bubble stays up longer while its translation is being made.
- **Web pages too**: the Trading Post, Gem Store and Wizard's Vault are translated in place, and whatever is missing is translated on your PC (never sent anywhere), so a page that changes needs no update of the mod.
- **In-game overlay** for options, first-run setup and translator metrics.
- **Patch resilient**: if the game changes, the DLL disables only the affected feature and logs the reason; the game keeps working.

Player chat is never translated, saved or sent.

## Installation

Close the game and run `GW2Enhanced-Installer.exe` from the package (`All` or one of the `Translation` packages, see the [root README](../README.md#downloads)). It finds the game folder on its own (Steam or the official launcher) or lets you choose it. A `msimg32.dll` from another mod is backed up and restored on uninstall. The `version.dll` of the earlier separate GW2 Translated is removed (and a `version.dll` of another mod that it had backed up is put back); `GW2Translated\` keeps its settings and collected data.

```text
GW2Enhanced-Installer.exe                install
GW2Enhanced-Installer.exe "<folder>"     install into a specific folder
GW2Enhanced-Installer.exe --uninstall    uninstall
```

The graphics card modes need the big model, a separate package (`GW2Translated-model-<language>-big-v<version>.zip`). Unzip it into `GW2Translated\translator\models\<language>-big\`. Without it, the translator uses the CPU.

## Usage

- **Shift+Home** opens the GW2 Enhanced overlay, with the **Translation** tab (`hotkey_translation` in `GW2Enhanced\config.ini`, Version tab; `Shift+Home` by default). While it is open, keyboard and mouse go to the overlay.
- **F10** reapplies the translation to the text already on screen.
- `GW2Translated\gw2translated_log.txt` shows what the translation did, once its log is on (`log_translation=1` in
  `GW2Enhanced\config.ini`, or **Logs** in the overlay's Health tab; off by default).

### `GW2Translated\config.ini`

Most of these can also be changed in the overlay.

| Key | Default | What it does |
|---|---|---|
| `language` | `pt-BR` | Language (folder under `languages\`). |
| `translate` | `true` | Applies the translation. |
| `autocollect` | `true` | Writes untranslated strings to `discovered.jsonl`. |
| `seed_decode` | `true` | Decrypts protected strings and writes `decrypted.jsonl`. |
| `live_translate` | `true` | Translates whatever is missing on your PC (English client only). |
| `web_translate` | `true` | Translates the game's web pages (Trading Post, Gem Store, Wizard's Vault). |
| `live_refresh` | `true` | Reapplies the translation to text already on screen. |
| `refresh_key` | `0x79` | Refresh key (virtual-key code; `0x79` = F10). |
| `overlay` | `true` | Shows the overlay. |
| `overlay_key` | `0x2D` | No longer used: the hotkeys are `hotkey` / `hotkey_translation` in `GW2Enhanced\config.ini` (Version tab), `Shift+Home` by default. |
| `contribute` | `ask` | Sends game texts and local translations to improve the project (`ask`, `true`, `false`). |
| `update_dictionary` | `true` | Downloads dictionary updates from the project's server, at most every 6 hours (`false` = only the dictionary installed with the mod). |
| `bubble_wait` | `8` | Most extra seconds a speech bubble stays up while its translation is made (`0` = off). |
| `translator_device` | `auto` | Where the local translator runs: `auto`, `cuda` (NVIDIA), `directml` (any card) or `cpu`. |
| `translator_threads` | `0` | CPU threads of the local translator (`0` = automatic). |
| `onboarded` | `false` | First-run setup completed (`false` shows it again). |

### Game folder

```text
GW2Translated\
├─ config.ini, gw2translated_log.txt
├─ discovered.jsonl, decrypted.jsonl       collection (always the original English)
├─ languages\pt-BR\                        dictionary.jsonl, web.jsonl, local.jsonl, web_local.jsonl
│  └─ release\                             downloaded dictionary releases (current.txt, v<N>\)
└─ translator\                             gw2translated-translator.exe, models\pt-BR\
```

## Building

Requires Rust (2024 edition) and Visual Studio with C++. The model binaries are stored in **Git LFS**: run `git lfs pull` after cloning.

```powershell
.\scripts\fetch-ctranslate2.ps1              # translator runtime, once
.\scripts\fetch-directml.ps1                 # DirectML runtime, once
cargo build --release --workspace            # everything
cargo mod-translation                        # msimg32.dll with only the translation: ..\target\release\msimg32.dll
cargo test --release -p gw2translated        # tests
.\scripts\fetch-model.ps1                    # small model without Git LFS (same files, from Argos' public index)
.\scripts\fetch-cublas.ps1                   # cuBLAS for the CUDA mode (NVIDIA's redistributable)
..\scripts\package.ps1                       # release zips in ..\dist\ (see the root README)
```

`scripts\convert-big-model.ps1` builds the big model in `data\models\<language>-big\` (not in git; the root `scripts\package.ps1` runs it and ships it in the CUDA and DirectML packages). It needs [uv](https://docs.astral.sh/uv/).

| Folder | Contents |
|---|---|
| `src/` | The translation feature (library compiled into `msimg32.dll`). |
| `translator/` | Local translator (CTranslate2 and ONNX Runtime). |
| `tools/gw2-dat-tool` | `Gw2.dat` reader. |
| `tools/gw2-translate` | Translates pending strings through an OpenAI-compatible API. |
| `examples/` | Maintenance tools (`check_exe`, `build_id_map`, `export_pending`…). |
| `scripts/` | Runtime download, model conversion, packaging. |
| `data/languages/<language>/` | Dictionaries for each language. |
| `data/models/<language>/` | Local translation model for each language. |

## Adding a language

1. Generate `data/languages/<code>/dictionary.jsonl` (`{"en":…,"tr":…}` lines) and fill its `ids` with `build_id_map` (see [Translation cycle](docs/translation.md)).
2. If there is an Argos `en → language` model, unzip the `.argosmodel` into `data/models/<code>/` (without the `stanza/` folder).
3. The installer copies every language; the player picks one in the overlay or with `language=`.

## Documentation

- [Architecture](docs/architecture.md): how the pieces work.
- [Reverse engineering](docs/reverse-engineering.md): the game's text system and chat.
- [Translation cycle](docs/translation.md): from collection to dictionary.
- [After a game patch](docs/after-a-game-patch.md): what to check and regenerate.
- [Backend API](../backend/docs/api.md): what the mod sends and downloads.

## Status

| | |
|---|---|
| In-game translation (by text and ID, any length) | done |
| Collection and decryption of protected strings | done |
| Refresh (automatic and F10), including chat | done |
| Local translation on CPU, CUDA and DirectML | done |
| Installer and overlay | done |
| Contribution upload (opt-in) | done; waits for the backend to be deployed |
| Dictionary download with signature check | done |
| Automatic updates | done (the GW2 Enhanced updater) |
| Download of the big model and the CUDA runtime from the overlay | to do |

## Credits and licenses

- GW2 Enhanced translation: [MIT](../LICENSE).
- `tools/gw2-dat-tool`: MIT (see the `LICENSE` in that folder).
- Local translator: [CTranslate2](https://github.com/OpenNMT/CTranslate2) (MIT), via [`ct2rs`](https://github.com/jkawamoto/ctranslate2-rs) (MIT).
- DirectML mode: [ONNX Runtime](https://github.com/microsoft/onnxruntime) (MIT), via [`ort`](https://github.com/pykeio/ort) (MIT or Apache-2.0), and Microsoft's [DirectML](https://github.com/microsoft/DirectML) redistributable (its license goes with the DLL, in `translator\directml\`).
- `translate-en_pb` model from [Argos Translate](https://github.com/argosopentech/argos-translate) and big model `Helsinki-NLP/opus-mt-tc-big-en-pt`, both from OPUS-MT — Jörg Tiedemann and Santhosh Thottingal, *OPUS-MT — Building open translation services for the World*, EAMT 2020 — licensed under **CC-BY 4.0**.

Guild Wars 2 is a trademark of ArenaNet and NCSOFT. This project is not affiliated with them. Use at your own risk.
