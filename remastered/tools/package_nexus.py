"""Packages the finished HD pack for Nexus Mods (or any file host).

    python package_nexus.py <pack folder> <output folder> [--part-gb 20]

Only what the runtime reads goes in (see `diag::start` in native/runtime/src/diag.rs): the archive (`pack.idx` +
`pack_NNNNN.bin`), `index.jsonl` and the id lists, for the main pack and its `composite_4x` / `character_4x` sub-packs.
Build logs and old index copies are left out.

Every zip holds the path `GW2Remastered/pack/...`, so extracting all of them into the game folder (the folder with
`Gw2-64.exe`) gives the layout the mod looks for. The data is already zstd compressed, so the zips only store it.
Big groups are split into parts of at most `--part-gb` (each part is a normal zip; extract them all).
A `SHA256SUMS.txt` lists every zip.
"""

import argparse
import hashlib
import os
import subprocess
import sys
from pathlib import Path

# Files the runtime reads next to the archive volumes.
MAIN_FILES = ["pack.idx", "index.jsonl", "composite_ids.txt", "device_ids.txt", "reasons.bin", "pair_skip.txt", "terrain_ids.txt"]
SUB_FILES = ["pack.idx", "index.jsonl"]

GROUPS = [
    # (zip name stem, sub folder, description)
    ("GW2Enhanced-HD-Textures", "", "main HD pack (required)"),
    ("GW2Enhanced-HD-Composite4x", "composite_4x", "4x armor/skin of your character (optional)"),
    ("GW2Enhanced-HD-Character4x", "character_4x", "4x face/hair of your character (optional)"),
]


def group_files(pack: Path, sub: str) -> list[Path]:
    d = pack / sub if sub else pack
    names = MAIN_FILES if not sub else SUB_FILES
    files = [d / n for n in names if (d / n).is_file()]
    files += sorted(d.glob("pack_*.bin"))
    loose = list(d.glob("*.dds.zst"))
    if loose:
        # Loose textures are served too, but a release should hold only archives (one open per volume, not per file).
        raise SystemExit(f"{d} still has {len(loose)} loose textures: run `batch/pack_archive.py add {d} --delete` first")
    return files


def split(files: list[Path], limit: int) -> list[list[Path]]:
    parts, cur, size = [], [], 0
    for f in files:
        s = f.stat().st_size
        if cur and size + s > limit:
            parts.append(cur)
            cur, size = [], 0
        cur.append(f)
        size += s
    if cur:
        parts.append(cur)
    return parts


def sha256(path: Path) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        while chunk := f.read(16 << 20):
            h.update(chunk)
    return h.hexdigest()


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("pack", type=Path)
    ap.add_argument("out", type=Path)
    ap.add_argument("--part-gb", type=float, default=20.0)
    ap.add_argument("--seven-zip", default="7z")
    a = ap.parse_args()
    if not (a.pack / "pack.idx").is_file():
        print(f"{a.pack} has no pack.idx", file=sys.stderr)
        return 1
    a.out.mkdir(parents=True, exist_ok=True)
    # 7z stores paths relative to the working directory: stage the layout with hard links (same volume, no copy)
    # when possible, else run from a folder that already is `.../GW2Remastered/pack`.
    root = a.pack.parent  # .../GW2Remastered (the folder name inside the zip comes from here)
    if root.name != "GW2Remastered":
        print(f"expected the pack inside a folder named GW2Remastered, got {root}", file=sys.stderr)
        return 1
    base = root.parent
    sums = []
    for stem, sub, what in GROUPS:
        files = group_files(a.pack, sub)
        if not files:
            print(f"skip {stem}: nothing in {sub or 'pack'}")
            continue
        parts = split(files, int(a.part_gb * (1 << 30)))
        for i, part in enumerate(parts, 1):
            name = f"{stem}.zip" if len(parts) == 1 else f"{stem}-part{i}of{len(parts)}.zip"
            dst = a.out / name
            if dst.exists():
                dst.unlink()
            listfile = a.out / (name + ".list")
            listfile.write_text("\n".join(str(f.relative_to(base)) for f in part) + "\n", encoding="utf-8")
            gb = sum(f.stat().st_size for f in part) / (1 << 30)
            print(f"{name}: {len(part)} files, {gb:.1f} GB ({what})", flush=True)
            subprocess.run([a.seven_zip, "a", "-tzip", "-mx0", "-bd", "-bso0", str(dst), f"@{listfile}"], cwd=base, check=True)
            listfile.unlink()
            sums.append(f"{sha256(dst)}  {name}")
    (a.out / "SHA256SUMS.txt").write_text("\n".join(sums) + "\n", encoding="utf-8")
    print("done:", a.out)
    return 0


if __name__ == "__main__":
    sys.exit(main())
