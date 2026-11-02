# After a Guild Wars 2 patch

GW2 is patched often. A patch can change two things:

1. **`Gw2-64.exe`**: function addresses (RVAs) change. The DLL does not use fixed addresses; it finds
   each function by byte signature. If a signature stops matching, only that feature is turned off
   (with the reason in the log) and the game carries on with the original textures.
2. **`Gw2.dat`**: new, changed or removed textures.

To rebuild the whole pack for the new version instead of patching it, run `tools/build_pack.py` on a new work
folder (see the README); the steps below update an existing pack and redo only what changed.

Paths below use two variables:

| Variable | Meaning |
|---|---|
| `GW2REM_DATA` | Folder with the heavy data: `Gw2.dat` copy, `textures/`, `pack/`, `models/` |
| `GW2_DIR` | The game folder (where `Gw2.dat` and `Gw2-64.exe` live) |

## Baseline

`$env:GW2REM_DATA\textures\baseline.json` records the version the pack was built from:

| Item | What it holds |
|---|---|
| Date | When the survey and hashes were taken |
| `Gw2-64.exe` sha256 | Identifies the executable the signatures were checked against |
| `Gw2.dat` | Size and number of textures |
| Hashes | `texture_hashes.jsonl`: XXH3-128 of each texture's **decompressed** content |

The hash covers the exact bytes the game's decoder receives. That is why the same value works for the
diff between versions and for the runtime check (below).

## Step by step

With the game **closed** (it locks `Gw2.dat`). Work on a copy of the `.dat` inside `GW2REM_DATA`, so
the game stays playable while the textures are processed.

```powershell
# 1. Copy the updated .dat
Copy-Item "$env:GW2_DIR\Gw2.dat" "$env:GW2REM_DATA\Gw2.dat"

# 2. New survey and new hashes, in a new dated folder
$v = "$env:GW2REM_DATA\textures\$(Get-Date -Format yyyy-MM-dd)"
New-Item -ItemType Directory $v | Out-Null
gw2tex --dat "$env:GW2REM_DATA\Gw2.dat" survey --out "$v\survey.jsonl"
texhash "$env:GW2REM_DATA\Gw2.dat" "$v\survey.jsonl" "$v\texture_hashes.jsonl" 8

# 3. Diff against the previous hashes and against each pack: new, changed and removed textures
$packs = "$env:GW2REM_DATA\pack", "$env:GW2REM_DATA\pack\composite_4x"
python tools/texhash/diff.py "$v\texture_hashes.jsonl" "$env:GW2REM_DATA\textures\texture_hashes.jsonl" "$v\diff" @packs

# 4. Redo what changed: drop it from the pack index, rebuild the job list, run the queue again
python tools/batch/pack_archive.py redo "$env:GW2REM_DATA\pack" --ids-file "$v\diff\redo_pack.txt"
python tools/batch/build_jobs.py "$env:GW2REM_DATA\textures" "$env:GW2REM_DATA\textures\jobs.jsonl"
python tools/batch/run_batch.py "$env:GW2REM_DATA\textures\jobs.jsonl" "$env:GW2REM_DATA\pack"

# 5. If the executable changed: start the game once and read GW2Remastered\gw2remastered_log.txt.
#    Features whose signature no longer matches are listed there and stay off.
```

Then update `baseline.json` and the reference hash files.

## Runtime protection (planned)

Each HD pack entry stores the `xxh3_128` of the original texture it was generated from. Before
substituting, the DLL hashes the bytes the game just read:

- equal: deliver the HD DDS;
- different (ArenaNet changed the texture): deliver the original and log the file id to
  `GW2Remastered\stale_textures.txt`, which feeds step 4.

This way a patch never shows an outdated or wrong HD texture.

## Reproducibility

`run_batch.py` records progress in `batch_log.txt` and adds one line per texture to the pack
`index.jsonl`, with the hash of the source it was made from. The model file name and the tile, padding and precision settings live in
`tools/upscale/upscale.py` and `tools/batch/run_batch.py`; reprocessing a changed texture with the same
model gives the same visual result as the others.
