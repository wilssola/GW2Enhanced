# Translation cycle

Each language's dictionary (`data/languages/<language>/dictionary.jsonl`) is generated outside the game, by AI (so far, NVIDIA's Nemotron). The game collects what is missing; it gets translated and shipped back.

```text
English game files + decrypted strings (decrypted.jsonl)
                         │ export_pending (list of untranslated strings)
                         ▼
                pending_strings.jsonl
                         │ gw2-translate (OpenAI-compatible API)
                         ▼
                translated_new.jsonl
                         │ merge_translations (checks markup)
                         ▼
      data/languages/pt-BR/dictionary.jsonl ◄── build_id_map (fills ids)
```

## Format

JSON Lines in UTF-8 without BOM: one JSON object per line, line breaks inside a text escaped as `\n` (see `src/records.rs`). `ids` are the game's text IDs with that English (the same in every client language), so the dictionary also translates a game running in another language.

| File | Line |
|---|---|
| `dictionary.jsonl` | `{"en":"…","tr":"…","src":"nemotron","ids":[1601,1602]}` (`src` and `ids` are optional) |
| `local.jsonl` (player's PC) | `{"en":"…","tr":"…","src":"argos"}` |
| `discovered.jsonl`, pending files | `{"en":"…"}` |
| `decrypted.jsonl` | `{"id":7,"en":"…"}` |

## Rules for a good translation

- Markup identical to the English: `<br>`, `<c=@…>`/`</c>`, `%str1%`, `%num1%`, `%%`. `gw2-translate` and `merge_translations` reject any pair that does not match.
- `[s]`, `[pl:"…"]` and `[lbracket]`/`[rbracket]` are game codes: keep them.
- Never translate to `[null]`; the DLL ignores such pairs.

## Which strings are missing

The list comes from the game files themselves, not from collection: every legitimate English string is either in the English `strs` files (extracted with `gw2-dat-tool`, see `after-a-game-patch.md`) or among the decrypted strings (`decrypted.jsonl`). The pending strings are that set minus the dictionary. This way no text in another language (from a session with the client in Spanish, for example) gets into the dictionary.

## `gw2-translate`

```powershell
$env:OPENAI_API_KEY = "<key>"            # never save the key to a file
$env:OPENAI_BASE_URL = "<API base URL, up to /v1>"
$env:GW2TRANSLATED_MODEL = "<model name>"

.\target\release\gw2-translate.exe `
  --input data\pending\pending_strings.jsonl `
  --existing data\languages\pt-BR\dictionary.jsonl `
  --output data\pending\translated_new.jsonl --batch 25 --workers 8
```

Any OpenAI-compatible API works, such as [NVIDIA NIM](https://build.nvidia.com) (`https://integrate.api.nvidia.com/v1`). Inputs are any JSON Lines with an `en` field; the output is `Translation` lines with `src` set to the model. It saves after every batch; running it again resumes where it stopped.

## Merge and publish

```powershell
cargo run --release --example merge_translations -- data\languages\pt-BR\dictionary.jsonl data\pending\translated_new.jsonl
# --replace overwrites entries already in the dictionary (retranslations); ids are always united
# then: step 2 of after-a-game-patch.md (fills the ids of the new entries)
```

## Web pages

`data/languages/<language>/web.jsonl` holds the texts of the game's web pages, as `{"en":"…","tr":"…"}` lines, and takes priority over the dictionary in those pages. It exists because the pages need their own wording: `Back` is a piece of armor there, not a button. It is optional: anything missing from it and from the dictionary is translated on the player's PC and kept in `web_local.jsonl`, which is never uploaded. `{name}` values must be kept. Texts with HTML tags are not matched, since the page splits them into several nodes. See `docs/reverse-engineering.md` for where the original texts are.

## Keys from other players

The RC4 keys are the same for everyone. Merge collections with `examples/merge_decrypted.rs` and pass the result to `build_id_map`.
