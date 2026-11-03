# GW2 Enhanced: HD Textures

HD textures for Guild Wars 2, upscaled offline with AI and swapped in as the game loads them. The game's
files are never modified. Unofficial mod, not affiliated with ArenaNet.

It is the HD textures feature of [GW2 Enhanced](../README.md): a library (`native/runtime`, crate `gw2remastered`)
compiled into the mod's single proxy DLL, `msimg32.dll`, next to the translation and graphics features. It runs
alongside ReShade (`dxgi.dll`).

## How it works

1. An offline pipeline decodes the game's textures from `Gw2.dat`, upscales them with the 4x-UltraSharp
   model (masks and normal maps use Lanczos, since a model would invent data in them), and packs the
   result in the original block format.
2. At run time the DLL hooks the game's file layer. When the engine asks for a texture that is in the
   pack, it receives the HD version instead. Everything else loads as usual.
3. The **Textures** tab of the overlay turns HD on and off live, shows memory use, and can color every texture by where
   it came from (**Debug colors**), so anything left in its original look is easy to spot.

HD off means the original textures, exactly as without the mod.

## What gets upscaled

Textures are grouped by what they are, not by size. A group has a maximum output side, and a source that
already reaches it is left alone.

| Group | What | Result |
|---|---|---|
| Character atlas | Skin, face, fur and armor pieces | 2x for everyone, 4x for your character (`composite_4x_all`: 4x for every player) |
| `character_parts` | Hair, mane, eyes (outside the atlas) | up to 2048 px |
| `weapon_armor` | Weapon and armor models | up to 2048 px |
| `world` | Scenery, objects, creatures, mounts | up to 1024 px |
| `effect` | Spell and ambient effects | up to 1024 px |
| `terrain` | Terrain tiles (ATEP) | up to 1024 px |
| Not upscaled | UI, cinematic art, textures already at the group's maximum, textures under 128 px | original |

Low/high pairs of a model texture always keep the exact 2x relation the engine requires, so a group is
processed or left alone as a whole.

Terrain tiles (`ATEP`) are served from their own list (`terrain_ids.txt`). `BC7X` textures are produced as real
BC7 and delivered as native ATEX; that path is new and still being validated in game. Not covered yet: the
`DXT3`/`DXTN` formats.

## Install

The mod and the HD textures are separate: the textures are not distributed (they are derived from the game's own
files and are very large), you generate them once from your copy of the game, see [Build the HD pack](#build-the-hd-pack).

1. Close the game. Install GW2 Enhanced (`All` or `Textures` package) with its installer, or copy `msimg32.dll`
   into the game folder (next to `Gw2-64.exe`); see the [root README](../README.md#installing).
2. Start the game and press `Shift+Home`. On the first run the overlay opens by itself and asks for the HD texture
   folder: pick the folder that holds `pack.idx` (**Browse...** or paste the path, then **Apply**), or **Skip**.
3. Restart the game: the pack is read at start. `GW2Remastered\config.ini` stores the choice (`pack_dir`).

Without a pack (not chosen, skipped, or the folder has none) the game runs with its original textures, exactly as
without the mod. The folder can be changed any time in the overlay.

To uninstall, run the installer with `--uninstall` or delete `msimg32.dll`.

### Settings (`config.ini`)

| Key | Default | Meaning |
|---|---|---|
| `remaster` | `true` | Serve the HD textures (also a toggle in the overlay) |
| `composite_4x` | `true` | Allow 4x characters at all (`false` = 2x for everyone) |
| `composite_4x_all` | `false` | 4x for every player on screen, not only your own (uses more memory) |
| `overlay_key` | `Shift+Home` | Hotkey of the GW2 Enhanced overlay in every build with this feature: modifiers plus a key, or a virtual code |
| `pack_dir` | `GW2Remastered\pack` next to the game | Folder with the HD pack (also set in the overlay); no pack there = original textures |
| `setup_done` | `false` | The first-run folder prompt was answered or skipped |
| `live_reload_npc` | `true` | Reload NPCs when HD is toggled; set `false` if toggling crashes |
| `live_reload_models` | `true` | Reload model textures (scenery, weapons) when HD is toggled |
| `terrain_hd` | `true` | Serve the upscaled terrain tiles (the ones listed in `terrain_ids.txt` in the pack; read at start) |

The log is `GW2Remastered\gw2remastered_log.txt`, off by default (`log_textures=1` in `GW2Enhanced\config.ini`, or
**Logs** in the overlay's Health tab); the previous session's log is kept as `.prev.txt`.

## Repository layout

| Path | What |
|---|---|
| `native/runtime` | The feature (compiled into `msimg32.dll` by `enhanced/`): file hooks, character atlas scaling, live reload, overlay panel |
| `tools/build_pack.py` | One command that builds the whole pack from your game (see below) |
| `tools/batch` | Pack production: `build_jobs.py` (job list), `run_batch.py` (resumable queue), `pack_archive.py` (pack volumes), `check_groups.py` and `build_reasons.py` (consistency and debug data), `policy.py` (what gets upscaled and how far) |
| `tools/upscale` | AI upscale with tiling (spandrel) |
| `tools/gw2tex` | Read-only `Gw2.dat` access: survey, extraction, class hints |
| `tools/texhash` | Original texture hashes; `diff.py` lists what a game patch changed |
| `tools/texpack`, `tools/texrdo` | Encode to the original block format; block-reuse RDO and zstd |
| `tools/datscan` | Scans `Gw2.dat` by file type; its dumps feed the runtime tests |
| `docs` | [Engine reverse engineering](docs/texture-reverse-engineering.md), [what to do after a game patch](docs/after-a-game-patch.md) |

## Build the mod

From the repository root:

```sh
cargo build --release              # target/release/msimg32.dll with every feature
cargo mod-textures                 # target/release/msimg32.dll with only the HD textures
cargo test --release -p gw2remastered
```

Rust 2024 edition, Windows x64.

## Build the HD pack

The pack is generated on your PC from the textures of the game you own, so nothing of the game's art is
redistributed. One command does everything, and it is resumable (Ctrl+C is safe, rerun to continue):

```sh
pip install -e ".[batch]"        # Python 3.11+; PyTorch must be a CUDA build (pytorch.org)
python tools/build_pack.py --game "<Guild Wars 2 folder>"
```

Needs: Windows x64, an NVIDIA GPU (CUDA, 12 GB+ VRAM recommended), [Rust](https://rustup.rs), about 130 GB of free
disk (the finished pack is about 95 GB: main 64 GB, 4x characters 30 GB), and the game closed (`Gw2.dat`, about 90 GB,
is read in place; `--copy-dat` works on a copy instead). It takes days of GPU time for the full pack, so use
`--duty 0.5` to keep the PC usable, or `--skip-4x` to leave out the optional 4x character packs (about 30 GB less).
The result is `<data>/pack` (default work folder: `~/GW2RemasteredData`, or `--data`); then pick it in the overlay.

The steps it runs, each one skipped when already done (`--only` / `--skip` select steps):

| Step | What |
|---|---|
| `tools` | Builds the Rust tools (`gw2tex`, `texhash`, `texpack`, `texrdo`) |
| `model` | Downloads 4x-UltraSharp (checked against its SHA-256) |
| `survey` | Lists every texture of `Gw2.dat`, hashes them, finds who uses them, classifies them |
| `jobs` | Applies the policy: what to upscale and how far (`build_jobs.py` + `policy.py`) |
| `pack` | Upscales and encodes: main pack, then `composite_4x` and `character_4x` (`run_batch.py`) |
| `archive` | Moves finished textures into a few volumes (`pack_archive.py`) |
| `finish` | `pair_skip.txt` and `reasons.bin` (`check_groups.py`, `build_reasons.py`) |
| `verify` | Compares your job lists with `data/reference.json` (same game build = identical lists) |

The job lists come out identical for the same game build. The AI upscale itself runs on your GPU, so pixel values can
differ in the last bits between GPUs and drivers; the result is the same textures, in the same format and sizes.

The list of character-atlas piece ids (`data/composite_ids.txt`) and of the hair/mane/eye ids of the 4x pack
(`data/character_4x_ids.txt`) ship in the repo; they are tied to the game version. Details of each tool:
`tools/batch/*.py` (docstrings), and [after a game patch](docs/after-a-game-patch.md).

### Packs

| Folder | Contents | Used for |
|---|---|---|
| `pack` | Everything except the 4x character packs | all HD textures |
| `pack/composite_4x` | Skin, face, fur and armor atlas pieces at 4x | your character (or everyone: `composite_4x_all`) |
| `pack/character_4x` | Hair, mane and eyes at 4x | your character |

Opening one file per texture is slow on a normal PC because the antivirus can scan each open, so the
pack ships as a few volumes (`pack_NNNNN.bin`, up to 1.9 GB each) plus `pack.idx`. No antivirus
exclusion is needed.

After a game patch, see [docs/after-a-game-patch.md](docs/after-a-game-patch.md).

## Tests

```sh
cargo test --release -p gw2remastered      # from the repository root
python tools/batch/test_policy.py
python tools/batch/test_pack_archive.py
python tools/batch/test_check_groups.py
python tools/batch/test_build_reasons.py
python tools/texhash/test_diff.py
```
