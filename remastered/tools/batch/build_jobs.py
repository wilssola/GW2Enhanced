"""Build the remaster job list (jobs.jsonl) from the survey and class hints.

Policy: groups by content, with a maximum output side (see policy.py):
  scenery, objects, NPCs, creatures  -> up to 1024 px (factor <= 4)
  weapons, armor, hair/mane/eyes     -> up to 2048 px
  effects                            -> up to 1024 px
  UI, or a source already at the group's size -> untouched
  skin/face (composite)              -> 2x for everyone else (4x for the player: composite_4x jobs)
Composite pieces (armor/skin/face sources loaded into the per-character atlas)
must all use the same factor (2). Device textures that come in low/high pairs
(GrTex2d m_file / m_fileHighRes, ids +1..+3 apart, 2x size) must share one factor
so the engine's UpRes/DownRes "exactly 2x" rule still holds.

Also writes two variant job lists next to the output, for the optional 4x packs:
  jobs_composite_4x.jsonl  every atlas piece at 4x (4x skin/face/armor for the local player or everyone)
  jobs_character_4x.jsonl  hair/mane/eyes at 4x for the local player (ids in data/character_4x_ids.txt)

Usage: python build_jobs.py <textures_dir> <out jobs.jsonl>
"""
import json
import math
import re
import sys
from collections import Counter, defaultdict
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from policy import composite_ids_path, max_side, shipped_ids_path  # noqa: E402

COLOR = {"DXT1", "DXT5", "DXT4", "BC7X"}
MASK = {"DXTA", "DXTL"}
NORMAL = {"3DCX"}


def role(fmt: str) -> str | None:
    if fmt in COLOR:
        return "color"
    if fmt in MASK:
        return "mask"
    if fmt in NORMAL:
        return "normal"
    return None  # DXT3/DXTN: not packed yet


def write_variants(jobs, out: Path):
    """The 4x packs: same textures as their 2x jobs (atlas pieces) or the listed hair/mane/eye ids, factor 4."""
    d = out.parent
    ids4 = {int(x) for x in shipped_ids_path(d, "character_4x_ids.txt").read_text().split()}
    for name, pick in (
        ("composite_4x", lambda j: j["group"] == "composite"),
        ("character_4x", lambda j: j["group"] == "character_parts" and j["file_id"] in ids4),
    ):
        n = 0
        with open(d / f"{out.stem}_{name}.jsonl", "w") as f:
            for j in jobs:
                if pick(j):
                    v = dict(j, factor=4, group=name)
                    f.write(json.dumps(v, separators=(",", ":")) + "\n")
                    n += 1
        print(name, n)


def main():
    d = Path(sys.argv[1])
    out = Path(sys.argv[2])
    survey = {}
    for l in open(d / "survey.jsonl"):
        r = json.loads(l)
        survey[r["file_id"]] = r
    hint, via = {}, {}
    for l in open(d / "texture_class_hints.jsonl"):
        r = json.loads(l)
        hint[r["file_id"]] = r["hint"]
        via[r["file_id"]] = r["via"]
    comp = {int(x) for x in composite_ids_path(d).read_text().split()}

    # --- composite: the high-res variant is what the game loads (high quality);
    # a cmpc id with a same-format 2x variant at +1..+3 is the low one -> skipped.
    comp_jobs = {}
    for i in comp:
        f = survey.get(i)
        # BC7X atlas pieces stay out: the atlas takes DXT5 and the mod converts those at runtime.
        if not f or f["kind"] != "ATEX" or not role(f["format"]) or f["format"] == "BC7X":
            continue
        hi = None
        for k in (3, 1, 2):
            g = survey.get(i + k)
            if g and g["kind"] == "ATEX" and g["format"] == f["format"] and g["width"] == 2 * f["width"] and g["height"] == 2 * f["height"]:
                hi = i + k
                break
        for t in ([hi] if hi else [i]):
            g = survey[t]
            if max(g["width"], g["height"]) * 2 <= 4096:
                comp_jobs[t] = {"file_id": t, "role": role(g["format"]), "factor": 2, "group": "composite"}

    # --- device texture pairs (union of hires-variant links)
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
    groups = defaultdict(list)
    for i in survey:
        groups[find(i)].append(i)

    dev_jobs = {}
    for members in groups.values():
        members = [m for m in members if m not in comp_jobs and m not in comp]
        members = [m for m in members if survey[m]["kind"] == "ATEX" and role(survey[m]["format"])]
        if not members:
            continue
        hints = Counter(hint.get(m, "unknown") for m in members)
        h = hints.most_common(1)[0][0]
        if h in ("weapon", "armor"):
            grp = "weapon_armor"
        elif h == "character":  # hair/mane/eyes outside the composite
            grp = "character_parts"
        elif h in ("scenery", "terrain", "creature", "mount", "unknown"):
            grp = "world"
        elif h == "effect":
            grp = "effect"
        else:
            continue  # ui
        target = max_side(grp)
        side = max(max(survey[m]["width"], survey[m]["height"]) for m in members)
        if side < 128 or side >= target:
            continue
        factor = min(2 ** int(math.log2(target / side)), 4)
        if factor < 2:
            continue
        for m in members:
            dev_jobs[m] = {"file_id": m, "role": role(survey[m]["format"]), "factor": factor, "group": grp}

    # --- terrain tiles (ATEP): the game loads them as CPU textures, so they are not "device" textures and the
    # mod serves them from their own list (terrain_ids.txt). Every tile gets a job; the ones listed in
    # terrain_seen_ids.txt (collected from the mod's logs) come first, so the maps in use are done first.
    terrain_jobs = {}
    seen = d / "terrain_seen_ids.txt"
    first = [int(x) for x in seen.read_text().split()] if seen.exists() else []
    atep = sorted(i for i, f in survey.items() if f["kind"] == "ATEP")
    for i in dict.fromkeys(first + atep):
        f = survey.get(i)
        if not f or f["kind"] != "ATEP" or not role(f["format"]):
            continue
        side = max(f["width"], f["height"])
        if side < 128 or side >= max_side("terrain"):
            continue
        terrain_jobs[i] = {"file_id": i, "role": role(f["format"]), "factor": min(2 ** int(math.log2(max_side("terrain") / side)), 4), "group": "terrain"}
    terrain_rank = {i: n for n, i in enumerate(terrain_jobs)}  # seen tiles first, then by id

    jobs = list(comp_jobs.values()) + list(dev_jobs.values()) + list(terrain_jobs.values())
    order = {"composite": 0, "character_parts": 1, "weapon_armor": 2, "world": 3, "effect": 4, "terrain": 5}
    jobs.sort(key=lambda j: (order[j["group"]], terrain_rank.get(j["file_id"], j["file_id"])))
    with open(out, "w") as f:
        for j in jobs:
            s = survey[j["file_id"]]
            j["format"], j["width"], j["height"] = s["format"], s["width"], s["height"]
            f.write(json.dumps(j, separators=(",", ":")) + "\n")
    write_variants(jobs, out)
    c = Counter((j["group"], j["role"]) for j in jobs)
    for k, v in sorted(c.items()):
        print(k, v)
    print("total", len(jobs))


if __name__ == "__main__":
    main()
