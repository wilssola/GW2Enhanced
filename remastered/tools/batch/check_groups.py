"""Textures that must stay original because their low/high group is only half in the pack.

The engine pairs a texture's small and big files and grows the small one to the big one, exactly 2x
at a time. If only some members of a group are HD, the sizes no longer line up: the grow request is
refused and the texture can stay smaller than the original. So every served texture of such a group
is listed in `pack/pair_skip.txt` and the mod serves none of them (the group stays original).

A group is "half in the pack" when it has at least one served member and at least one that is not:
usually a member in BC7X, which the pipeline does not process yet.

Usage: GW2REM_DATA=<data dir> python check_groups.py       (writes <data>/pack/pair_skip.txt)
"""
import collections
import json
import os
import re
from pathlib import Path


def half_served_ids(survey: dict, via: dict, served: set) -> set:
    """Served ids that belong to a group (2+ members) where not every member is served."""
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
    out = set()
    for members in groups.values():
        if len(members) < 2:
            continue
        got = [i for i in members if i in served]
        if got and len(got) < len(members):
            out |= set(got)
    return out


def main():
    data = Path(os.environ["GW2REM_DATA"])
    tex, pack = data / "textures", data / "pack"
    survey, via = {}, {}
    for l in open(tex / "survey.jsonl"):
        r = json.loads(l)
        survey[r["file_id"]] = r
    for l in open(tex / "texture_class_hints.jsonl"):
        r = json.loads(l)
        via[r["file_id"]] = r["via"]
    in_pack = {json.loads(l)["file_id"] for l in open(pack / "index.jsonl")}
    device = {int(x) for x in (pack / "device_ids.txt").read_text().split()}
    in_char4x = {json.loads(l)["file_id"] for l in open(pack / "character_4x" / "index.jsonl")}
    served = (in_pack & device) | in_char4x
    skip = half_served_ids(survey, via, served)
    (pack / "pair_skip.txt").write_text("\n".join(map(str, sorted(skip))))
    print(f"{pack / 'pair_skip.txt'}: {len(skip)} of {len(served)} served textures stay original")


if __name__ == "__main__":
    main()
