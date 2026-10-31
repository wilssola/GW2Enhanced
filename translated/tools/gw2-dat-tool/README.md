# gw2-dat-tool

Rust CLI that reads **Guild Wars 2**'s `Gw2.dat`: lists and extracts MFT
entries, reads the text manifest (`TextPackManifest`) and extracts the text
files (`strs`) of each language.

It never writes to `Gw2.dat`; it only reads it and writes files elsewhere. The
game must be **closed**: it locks the file for exclusive use.

## Build

```powershell
cargo build --release -p gw2-dat-tool
# binary: target/release/gw2-dat-tool.exe
```

Every command takes `--dat <path to Gw2.dat>`.

## Text commands (used by the translation feature)

| Command | What it does |
|---|---|
| `text-files [--lang 0] [--manifest <fid>] --out <txt>` | Lists the file_ids of a language's `strs` according to the `TextPackManifest`; line `i` covers text IDs `i*1024 … i*1024+1023`. |
| `extract-text [--lang 0] [--manifest <fid>] --out <folder>` | Extracts all `strs` of that language as `<fid>.bin`. |
| `find-type <fourcc> [--max-size N]` | Finds the first entry whose real type is `<fourcc>` (e.g. `txtm`) and prints its file_id; use it when a patch changes the manifest's file_id. |
| `extract-ids --ids <txt> --out <folder>` | Extracts the listed file_ids (one per line). |

Languages in the manifest: 0 English, 2 French, 3 German, 4 Spanish, 5 Chinese
(1 is empty). Full procedure: `docs/after-a-game-patch.md`.

## General commands

| Command | What it does |
|---|---|
| `info` | File header: version, chunk size, MFT, number of entries. |
| `list [--filter <s>]` | MFT entries (index, offset, compression, size, type). |
| `dump <index> [--bytes N]` | Hexdump of the first (still compressed) bytes of an entry. |
| `extract <index> --out <file>` | Decompresses an entry. |
| `scan [--filter <s>] [--limit N]` | Decompresses entries to find their real type (slow). |

## Layout

| File | Role |
|---|---|
| `src/dat.rs` | `Gw2.dat` reader: header, MFT, ATOC, decompression. |
| `src/compression.rs` | GW2 decompressor (Huffman/LZ); skips each 64 KiB block's CRC by position. |
| `src/main.rs` | CLI and `TextPackManifest` reader. |

License: MIT (see `LICENSE`).
