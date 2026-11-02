# Architecture

```text
Guild Wars 2\
├─ msimg32.dll                         ← GW2 Enhanced (enhanced/), with the translation feature (translated/src/)
└─ GW2Translated\
   ├─ config.ini
   ├─ gw2translated_log.txt
   ├─ discovered.jsonl                 ← strings seen without a translation (collection)
   ├─ decrypted.jsonl                  ← encrypted strings already decrypted (collection)
   ├─ upload_state.json                ← how far each file was sent (contribution)
   ├─ languages\<language>\
   │  ├─ dictionary.jsonl              ← English → language, with text IDs (any client language)
   │  ├─ web.jsonl                     ← texts of the game's web pages
   │  ├─ local.jsonl                   ← translations made on the player's PC
   │  ├─ web_local.jsonl               ← web page texts translated on the player's PC (never uploaded)
   │  └─ release\                      ← dictionary releases downloaded from the server
   │     ├─ current.txt                ← version in use
   │     ├─ state.json                 ← time of the last check
   │     └─ v<N>\                      ← manifest.json (signed) + dictionary-NNN.jsonl.gz; the previous version is kept
   └─ translator\
      ├─ gw2translated-translator.exe
      ├─ directml\                     ← ONNX Runtime and DirectML (DirectML mode)
      ├─ gpu\                          ← cuBLAS (CUDA mode)
      └─ models\<language>[-big]\      ← local translation models
```

Collection always records the original English, so it lives outside the language folders.

## The translation feature (`src/`)

The crate `gw2translated` is a library compiled into the GW2 Enhanced `msimg32.dll` (cargo feature `translation` of `enhanced/`). Windows loads the `msimg32.dll` in the game folder before the system one; `enhanced/src/proxy.rs` forwards its 5 exports to the System32 copy, and `enhanced` calls `gw2translated::init` on a thread of its own inside a panic guard (never in `DllMain`). `init` does nothing unless the process is `Gw2-64.exe`. Its overlay panel is the **Translation** tab of the GW2 Enhanced overlay (see [the architecture of the mod](../../docs/specs/unified-mod-design.md)).

| Module | Role |
|---|---|
| `lib.rs` | Startup: config, log, dictionaries, hooks. |
| `paths.rs` | `GW2Translated\` folder, per language. |
| `config.rs` | `config.ini` (adds missing keys, preserves the rest). |
| `runtime.rs` | Settings that the overlay changes while the game runs. |
| `log.rs` | `gw2translated_log.txt`. |
| `records.rs` | The JSON Lines data files. |
| `dict.rs` | Dictionaries by text and by ID; normalization; markup checks; `[null]` protection; the merged dictionary in use, swappable while the game runs. |
| `release.rs` | Dictionary releases from the server: signed manifest, checked download, install with rollback. |
| `game.rs` | The exe image in memory; reading instruction operands. |
| `sig.rs` | Byte signatures of every function used. |
| `layout.rs` | Field offsets read from the game code + `check_exe`. |
| `hooks.rs` | `F_materialize` (translation), `reg-insert` and `bulk_insert` (collection). |
| `refresh.rs` | Refresh of text already on screen (automatic and hotkey). |
| `coded.rs` | The game's coded text: finds the string IDs in bubble and chat lines. |
| `chat.rs` | Chat tabs: rebuilt so NPC lines show their translation. |
| `bubble.rs` | Speech bubbles: stay up longer while their translation is being made. |
| `rc4.rs` | Decrypts protected strings. |
| `collect.rs` | Collection thread: writes the JSONL files and matches entries with keys. |
| `live.rs` | Local translation: queues, translator process, `local.jsonl`. |
| `overlay.rs`, `ui_text.rs` | In-game overlay (Dear ImGui) and its text in each language. |
| `gpu.rs`, `procstats.rs` | Graphics cards found; RAM, CPU, VRAM and GPU use of the translator. |
| `upload.rs`, `net.rs` | Opt-in sending of collected texts, through WinHTTP (HTTPS to the project's host only, no redirects). |
| `web.rs`, `web_inject.js` | The game's web pages: Trading Post, Gem Store, Wizard's Vault. |

### Life of a string

1. The game decodes the string (`F_materialize`), including encrypted ones.
2. The hook looks up a translation by the original text, then among the session's local translations and, finally, by ID.
3. Found: it writes the translation into the game's own array (`Reserve`), with no size limit. Not found: the original stays, goes to collection and to the local translation queue; when the translation comes back, a refresh applies it.
4. The game's `CParser` resolves plurals and variables on the translated text.

Priority order: downloaded release → package dictionary → local translation → English. The release and the package dictionary are merged into one dictionary at startup (a release text replaces the package one, by text and by ID; texts the release lacks stay); `local.jsonl` only fills what neither has, so a translation made on this PC never replaces a reviewed one. The mod has no translations typed by the player: corrections are made on the site and reach the game through the next release.

### Dictionary releases

The server builds a release of the reviewed translations every day (`backend/docs/api.md`): a manifest with the version and the SHA-256 and size of each `dictionary-NNN.jsonl.gz`, signed with ed25519. `release.rs` keeps the installed copy current, on its own thread, never on a game thread:

1. While `update_dictionary` is on (config and overlay), at most once every 6 h (the last check is in `release\state.json`; offline, it tries again 15 minutes later), it asks `GET /v1/releases/<language>/latest`. Requests go over HTTPS to the project's host only, never follow redirects, and have timeouts and size caps (manifest 64 KB, file 8 MB, release 64 MB, 16 MB per file once gunzipped).
2. The signature is checked with the public key built into the DLL, over the same canonical JSON the server signs (keys sorted, no whitespace, without `signature`). Language, file names (`dictionary-NNN.jsonl.gz`, never a path) and sizes are checked too. An unsigned or tampered manifest, or one signed by another key, is ignored.
3. The same version as the one in use: nothing to do. Any other version is installed, an older one too: the server only signs an older one again when it rolls a bad release back.
4. Files already on disk with the same SHA-256 are copied; the rest are downloaded. Each one is checked for size, SHA-256, gunzipped size and records, into `v<N>.part\`. Only when all passed is it renamed to `v<N>\` and `current.txt` rewritten (through a temporary file). Any failure deletes `v<N>.part\` and leaves the version in use as it was. The previous version is kept; older ones are deleted.
5. The new merged dictionary is built on that thread and swapped in (`dict::Shared`): a lookup takes a reference-counted handle and holds no lock while it uses it, so a lookup in progress finishes with the old dictionary, which is freed on the release thread once nobody holds it (freeing ~200k strings on a game thread would be a hitch). Then a refresh shows the new texts on screen.

At startup, the version in `current.txt` is checked again (signature and every SHA-256) before it is loaded; if it fails, the newest other version on disk that passes is used and becomes current (rollback), and with none the package dictionary is used alone. With `update_dictionary=false`, nothing is downloaded and only the package dictionary is used.

### Local pre-translation

Speech bubbles and chat lines keep the text they received and do not request it again. So when the game loads the map's text files (`reg-insert`), untranslated strings go to the translator's pre-translation queue, before they appear. Queues, by priority: on-screen text → about to be shown → pre-translation. Only on-screen text triggers a refresh (at most one every 30 s).

### Chat and speech bubbles

Each chat tab (`ChatLog`) stores its messages and builds its lines from them. NPC lines store a reference to the string: rebuilding the tab (done on every refresh) decodes them again, already translated. Messages typed by players are left as they are. Tabs are tracked through their creation and destruction in the control's message handler, so only live tabs are rebuilt.

A speech bubble whose translation is still being made stays up longer (`bubble_wait` seconds at most, longer lines wait longer), and shows the translation as soon as it arrives.

Tabs are saved in `Local.dat` with their name as plain text, so a tab saved before the translation keeps its English name. `chat.rs` translates the default names (`Main`, `Combat`) as the tabs are added, and the game then saves the translated name. Names the player chose are not touched.

### Web pages

The Trading Post, Gem Store and Wizard's Vault are web pages in an embedded Chromium (CEF), and their text comes from ArenaNet's servers, so it never reaches `F_materialize`. The game's process is CEF's browser process: `web.rs` hooks `cef_browser_host_create_browser`, wraps the client's load handler and, when a page finishes loading, runs `web_inject.js` in it with `web.jsonl` and `web_local.jsonl`, a few dozen KB. The dictionary itself is never sent to a page: as a ~20 MB script it held the page up for seconds. The script rewrites text nodes, `placeholder` and `title`, and watches for redraws. Texts with values, like `{days} days ago`, match as patterns. What the player types is left alone, and searching the Trading Post still takes English names.

Every other text is reported by the script as a console message (`\u0001gw2t-missing:` followed by a JSON list), which `web.rs` reads by wrapping the client's display handler and hides from the console. The DLL answers at once with what the dictionary has; the rest goes to the local translator in a queue of its own, served right after on-screen game text; the result is written to `web_local.jsonl` and sent back to the page (`__gw2t.add`). `web_local.jsonl` is not in the upload list, and it is read at startup, so each text is translated once. The `web_translate` setting (also in the overlay) turns the whole feature on and off; it needs the English client, like the rest of local translation.

### Robustness

- Every function is found by signature and every field is read from the instruction that uses it. If something changes in a patch, only that feature is disabled, with the reason in the log; the game keeps running.
- All code inside a hook runs under `catch_unwind`: an error returns the original text instead of crashing the game.
- The C/C++ runtime is static: nothing depends on the Visual C++ Redistributable.

## `gw2translated-translator` (`translator/`)

Local translation, outside the game process: an HTTP server on `127.0.0.1` on a free port, with the LibreTranslate API (`POST /translate`).

| Device | Runs on | Model | Engine |
|---|---|---|---|
| `cpu` | Any PC | Small (Argos, OPUS-MT) | CTranslate2 |
| `cuda` | NVIDIA cards | Big (`opus-mt-tc-big-en-pt`) | CTranslate2 |
| `directml` | Any card | Big | ONNX Runtime (`dml.rs`) |

`auto` tries CUDA, then DirectML (with at least 1.5 GB of free video memory and the big model present), then the CPU. Any failure on the card falls back to the CPU.

CTranslate2 has no DirectML backend, so that mode runs on ONNX Runtime, with its own beam search. It needs no second model: `encoder.onnx` and `decoder.onnx` hold only the graph, half a megabyte, and their weights are byte offsets into CUDA's `model.bin` (`scripts/export_onnx.py`). Convert that `model.bin` again and the graphs must be rewritten (`scripts/convert-big-model.ps1 -GraphsOnly`).

Measured on a current NVIDIA card with 300 dictionary texts, one request per text: CPU small 44 ms, CUDA big 233 ms, DirectML big 657 ms per text (the decoder has no cache). 200 of the 300 DirectML translations were identical to CUDA's.

- Game markup is split from the text before it reaches the model (`segment.rs`), so it is never lost; the text is also split into sentences and at a leading `(emote)`, because OPUS-MT models translate one sentence at a time; `%%` is treated as markup.
- The DLL only starts it when there is something to translate, at low priority and with no window, and only with the English client. It exits on its own when the game closes.

The small model lives in `data/models/<language>/` (installed to `GW2Translated\translator\models\<language>\`): `model/` (CTranslate2), `sentencepiece.model`, and the `README.md` and `metadata.json` from the original `.argosmodel`, which is a ZIP; its `stanza/` folder, used by Python, is not needed. The big model is built by `scripts/convert-big-model.ps1` and is not in git.

## Tools

| Where | What |
|---|---|
| `../installer/` | The GW2 Enhanced installer (`GW2Enhanced-Installer.exe`). |
| `tools/gw2-dat-tool` | Reads `Gw2.dat`: text manifest, `strs` extraction. |
| `tools/gw2-translate` | Translates pending strings through an OpenAI-compatible API. |
| `examples/check_exe.rs` | Checks whether the DLL fits a `Gw2-64.exe`. |
| `examples/build_id_map.rs` | Fills the text IDs (`ids`) of the dictionary records. |
| `examples/export_pending.rs` | Exports what was collected and has no translation. |
| `examples/merge_translations.rs` | Merges new translations into the dictionary (checks markup; `--replace` overwrites). |
| `examples/merge_decrypted.rs` | Merges `decrypted.jsonl` files from several players. |
| `examples/fake_gw2.rs` | Simulates the game to test DLL startup (it loads the DLL under the name `version.dll` from its own folder). |
