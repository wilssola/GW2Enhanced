"""What a game patch changed, and what the HD packs must redo.

Compares the hashes of a new Gw2.dat (texhash output) with the hash each pack entry was
made from (`src_xxh3_128` in its index.jsonl), and with the previous hash list for
textures the packs do not have.

Usage: diff.py <new_hashes> <old_hashes> <out_dir> <pack_dir> [<pack_dir>...]

Writes to out_dir:
  redo_<pack>.txt  ids to feed `pack_archive.py redo <pack> --ids-file`
  new_ids.txt      textures that did not exist before (candidates for new jobs)
  removed_ids.txt  textures the game no longer has (their HD files are just never asked for)
"""
import csv
import json
import sys
from pathlib import Path


def load_hashes(path: Path) -> dict:
    out = {}
    with open(path, encoding="utf-8", newline="") as f:
        if path.suffix == ".jsonl":
            for l in f:
                r = json.loads(l)
                out[int(r["file_id"])] = r["xxh3_128"]
        else:
            for r in csv.DictReader(f):
                out[int(r["file_id"])] = r["xxh3_128"]
    return out


def stale_entries(index: Path, new: dict) -> tuple:
    """(changed in the patch, gone from the game, unchecked: no source hash recorded)."""
    changed, gone, unchecked = [], [], 0
    for l in open(index, encoding="utf-8"):
        try:
            r = json.loads(l)
            i = r["file_id"]
        except (ValueError, KeyError):
            continue
        src = r.get("src_xxh3_128")
        if i not in new:
            gone.append(i)
        elif not src:
            unchecked += 1
        elif src != new[i]:
            changed.append(i)
    return changed, gone, unchecked


def main(argv) -> int:
    if len(argv) < 4:
        print(__doc__)
        return 2
    new, old = load_hashes(Path(argv[0])), load_hashes(Path(argv[1]))
    out = Path(argv[2])
    out.mkdir(parents=True, exist_ok=True)
    print(f"textures: {len(old)} before, {len(new)} now")
    new_ids = sorted(set(new) - set(old))
    removed = sorted(set(old) - set(new))
    (out / "new_ids.txt").write_text("".join(f"{i}\n" for i in new_ids))
    (out / "removed_ids.txt").write_text("".join(f"{i}\n" for i in removed))
    print(f"new in the game: {len(new_ids)}; removed: {len(removed)}; changed overall: {sum(new[i] != old[i] for i in set(new) & set(old))}")
    for p in map(Path, argv[3:]):
        changed, gone, unchecked = stale_entries(p / "index.jsonl", new)
        (out / f"redo_{p.name}.txt").write_text("".join(f"{i}\n" for i in sorted(changed)))
        print(f"{p.name}: {len(changed)} to redo, {len(gone)} no longer in the game, {unchecked} without a source hash")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
