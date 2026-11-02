"""Why a texture is not in the HD pack: `reasons.bin`, read by the mod's "Debug colors".

Same rules as build_jobs.py, applied to every ATEX texture of DXT1/DXT5 format (the ones the mod can
tint) that has no job. One byte per file id:

  0 nothing to say (has a job, or not a tintable format)
  1 effect: effects are left alone on purpose
  2 small member of a low/high pair whose big member is already at the target size
  3 already at the target size (1024, 2048 for weapons/armor/hair)
  4 under 128 px
  5 no job for another reason
  6 source of a character piece (the composite pipeline owns it, e.g. the low twin of a piece)

File: magic "GW2RSN1\\0", u32 count, then `count` bytes indexed by file id.

Usage: GW2REM_DATA=<data dir> python build_reasons.py [out]     (default <data>/pack/reasons.bin)
"""
import collections
import json
import os
import re
import struct
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from policy import composite_ids_path  # noqa: E402

MAGIC = b"GW2RSN1\0"
TINTABLE = {"DXT1", "DXT5"}
BIG_TARGET = {"weapon", "armor", "character"}
SKIP_HINTS = {"effect", "ui"}


def classify(survey: dict, hint: dict, via: dict, jobs: set, comp: frozenset = frozenset()) -> dict:
    parent = {}

    def find(x):
        parent.setdefault(x, x)
        while parent[x] != x:
            parent[x] = parent[parent[x]]
            x = parent[x]
        return x

    for i, v in via.items():
        m = re.match(r"hires-variant of (\d+)", v)
        if m:
            a, b = find(i), find(int(m.group(1)))
            if a != b:
                parent[a] = b
    groups = collections.defaultdict(list)
    for i in survey:
        groups[find(i)].append(i)

    out = {}
    for members in groups.values():
        top = collections.Counter(hint.get(m, "unknown") for m in members).most_common(1)[0][0]
        target = 2048 if top in BIG_TARGET else 1024
        side = max(max(survey[m]["width"], survey[m]["height"]) for m in members)
        for i in members:
            s = survey[i]
            if i in jobs or s["kind"] != "ATEX" or s["format"] not in TINTABLE:
                continue
            mine = max(s["width"], s["height"])
            if i in comp:
                out[i] = 6
            elif top == "effect":
                out[i] = 1
            elif top == "ui":
                continue
            elif side < 128:
                out[i] = 4
            elif side >= target:
                out[i] = 2 if mine < side else 3
            else:
                out[i] = 5
    return out


def main():
    data = Path(os.environ["GW2REM_DATA"])
    tex = data / "textures"
    survey, hint, via = {}, {}, {}
    for l in open(tex / "survey.jsonl"):
        r = json.loads(l)
        survey[r["file_id"]] = r
    for l in open(tex / "texture_class_hints.jsonl"):
        r = json.loads(l)
        hint[r["file_id"]] = r["hint"]
        via[r["file_id"]] = r["via"]
    jobs = set()
    for name in ("jobs.jsonl", "jobs_composite_4x.jsonl"):
        if (tex / name).exists():
            for l in open(tex / name):
                jobs.add(json.loads(l)["file_id"])
    comp = frozenset(int(x) for x in composite_ids_path(tex).read_text().split())
    codes = classify(survey, hint, via, jobs, comp)
    n = max(codes) + 1
    arr = bytearray(n)
    for i, c in codes.items():
        arr[i] = c
    out = Path(sys.argv[1]) if len(sys.argv) > 1 else data / "pack" / "reasons.bin"
    out.write_bytes(MAGIC + struct.pack("<I", n) + bytes(arr))
    print(f"{out}: {len(codes)} textures marked", dict(collections.Counter(codes.values())))


if __name__ == "__main__":
    main()
