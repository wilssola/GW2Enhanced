# After every Guild Wars 2 patch

Do everything with the **game closed** (GW2 locks `Gw2.dat` for itself). The commands below use `$env:GW2_DIR`, the game folder, and run from `translated\`. `..\re\` is a work folder outside git.

```powershell
$env:GW2_DIR = "<game folder>"
```

## 1. Does the DLL still fit the game?

```powershell
cargo run --release --example check_exe -- "$env:GW2_DIR\Gw2-64.exe"
```

Every item should be OK. A failed item tells you which signature or layout changed; the mod disables only that part of the translation and the game keeps working. What each item is, is described in `docs/reverse-engineering.md`.

## 2. ID map (translation with the game in any language)

```powershell
$dat = "$env:GW2_DIR\Gw2.dat"
..\target\release\gw2-dat-tool.exe --dat $dat text-files   --lang 0 --out ..\re\lang0_files.txt
..\target\release\gw2-dat-tool.exe --dat $dat extract-text --lang 0 --out ..\re\strs_en
cargo run --release --example build_id_map -- ..\re\lang0_files.txt ..\re\strs_en <decrypted.jsonl> data\languages\pt-BR\dictionary.jsonl
```

If `text-files` says the manifest is not in the ATOC, the patch changed the `TextPackManifest` file_id: find the new one with `gw2-dat-tool --dat $dat find-type txtm` and pass `--manifest <file_id>`.

## 3. New strings to translate

```powershell
# what showed up in game and has no translation yet
cargo run --release --example export_pending -- "$env:GW2_DIR\GW2Translated" data\languages\pt-BR\dictionary.jsonl data\pending\pending_strings.jsonl
```

`export_pending` writes `{"en":"…"}` lines, ready for the `--input` of `gw2-translate`. After translating, merge into `dictionary.jsonl` (`docs/translation.md`) and repeat step 2.

## RC4 keys from other players

```powershell
cargo run --release --example merge_decrypted -- data\pending\decrypted_merged.jsonl <your decrypted.jsonl> <others...>
```
