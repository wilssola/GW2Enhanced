"""Derive per-texture class hints from gw2tex JSONL outputs.

Inputs (<GW2REM_DATA>/textures): survey.jsonl, models.jsonl, refs.jsonl.
Output: texture_class_hints.jsonl (+ texture_class_hints_summary.json).

Each texture collects evidence (hint, priority, via); the highest priority
wins, ties broken by evidence count. Rules are listed in RULES below and in
the summary file.
Run with: python -I texture_hints.py [dir]
"""

from __future__ import annotations

import collections
import json
import sys
from pathlib import Path

RACE_SKELETONS = {13869, 16150, 15212, 27572, 18454, 29383, 21551, 30305}
ARMOR_TOKENS = {"armor", "armoropaque", "backdyeable"}
CHAR_TOKENS = {"skin", "hair", "hairtan", "eye", "eyelash"}
FX_TOKENS = {"fx", "effecttarget"}
HINTS = ["character", "armor", "weapon", "mount", "creature", "scenery", "terrain", "effect", "ui", "unknown"]

# Content types (cntc root type index) observed in this build; names are
# inferred from what their objects reference (type names are not shipped).
CONTENT_SKIN = 66  # MODL + ATEU icon per object: equipment skins
CONTENT_CHARACTER = 67  # MODL (mostly external skeletons) + AMSP: NPC/creature models
CONTENT_EFFECT = 183  # MODL (mostly fx) + AMSP
CONTENT_PROP = {199, 30, 27}  # MODL mostly 'opaque' props / gadgets
CONTENT_TERRAIN = {286}  # -> PIMG

RULES = [
    "p10 armor: cmpc comp raceSexData[].fileData[].{textureBase,textureNormal,maskDye1-4,maskCut} (armor composite)",
    "p10 character: cmpc comp raceSexData[].skinPatterns[].* (body skin patterns)",
    "p9 armor: texture of a MODL on a race skeleton whose material_token is armor/armoropaque/backdyeable, or a MODL referenced by cmpc fileData.meshBase/meshOverlap",
    "p9 character: texture of a MODL on a race skeleton with skin/hair/eye material tokens",
    "p8 weapon: texture of a MODL referenced by a content skin (type66) that is not on a race/external skeleton and not fx",
    "p8 terrain: mapc trn materials.texFileArray[], PIMG strippedPages[] (paged terrain), trn.pagedImage chain",
    "p7 creature: texture of a MODL with an external non-race skeleton, or referenced by content type67",
    "p6 effect: material_token fx/effecttarget, or MODL from content type183",
    "p5 scenery: MODL placed by maps (mapc prp2/zon2, prlt, cmaC) or content props (199/30/27); map env/rive/dcal/lght/prp2 blitTextures",
    "p4 scenery: MODL material_token opaque without external skeleton",
    "p4 ui: ATEU/ATEX referenced directly by content objects without a model (icons)",
    "p3 effect: one-mesh unskinned MODL with bound_radius 0 (billboards/quads)",
    "p2 scenery: cinematic (CINP) props",
    "p(n-1) inherit: unreferenced texture at a referenced texture's file id +3/+1/+2 with >= size (high-res sidecar)",
    "p1 terrain: ATEP kind with no other evidence; p1 ui: ATEU kind with no other evidence",
    "p0 unknown",
]


def jl(p: Path):
    with p.open(encoding="utf-8") as f:
        for line in f:
            if line.strip():
                yield json.loads(line)


def main() -> None:
    if len(sys.argv) != 2:
        raise SystemExit("usage: texture_hints.py <textures_dir>")
    d = Path(sys.argv[1])
    survey = {r["mft"]: r for r in jl(d / "survey.jsonl")}
    ev: dict[int, list[tuple[int, str, str]]] = collections.defaultdict(list)

    def add(mft, hint, prio, via):
        if mft in survey:
            ev[mft].append((prio, hint, via))

    # ---- refs: chains file -> file
    refs = list(jl(d / "refs.jsonl"))
    fid2mft: dict[int, int] = {}
    for r in refs:
        fid2mft[r["src_file_id"]] = r["src_mft"]
        fid2mft[r["target_file_id"]] = r["target_mft"]

    model_hint: dict[int, tuple[int, str, str]] = {}  # model mft -> best (prio, hint, via)

    def model_ev(mft, hint, prio, via):
        cur = model_hint.get(mft)
        if cur is None or prio > cur[0]:
            model_hint[mft] = (prio, hint, via)

    # content objects: (src_mft, obj) -> refs
    objs: dict[tuple[int, str], list[dict]] = collections.defaultdict(list)
    for r in refs:
        src, chunk, path, tk, tm = r["src_fourcc"], r["chunk"], r["field_path"], r["target_kind"], r["target_mft"]
        if src == "cmpc":
            leaf = path.rsplit(".", 1)[-1]
            if ".fileData[]." in path:
                if tk == "MODL":
                    model_ev(tm, "armor", 9, f"cmpc:{leaf}")
                else:
                    add(tm, "armor", 10, f"cmpc:{path}")
            elif ".skinPatterns[]." in path:
                add(tm, "character", 10, f"cmpc:{path}")
            elif tk == "MODL":
                model_ev(tm, "character", 8, f"cmpc:{path}")
        elif src == "mapc":
            if chunk == "trn " or chunk == "trn":
                add(tm, "terrain", 8, f"mapc:{chunk.strip()}:{path}")
            elif tk == "MODL":
                model_ev(tm, "scenery", 5, f"mapc:{chunk.strip()}:{path}")
            else:
                add(tm, "scenery", 5, f"mapc:{chunk.strip()}:{path}")
        elif src == "PIMG":
            add(tm, "terrain", 8, f"PIMG:{path}")
        elif src in ("prlt", "cmaC") and tk == "MODL":
            model_ev(tm, "scenery", 5, f"{src}:{path}")
        elif src == "CINP":
            if tk == "MODL":
                model_ev(tm, "scenery", 2, "CINP")
            else:
                add(tm, "scenery", 2, f"CINP:{path}")
        elif src == "cntc":
            parts = path.split("/")
            objs[(r["src_mft"], parts[2])].append(r)
        elif src not in ("MODL",):
            add(tm, "unknown", 0, f"{src}:{path}")

    # ---- models.jsonl: per-model classes
    models: dict[int, dict] = {}
    for r in jl(d / "models.jsonl"):
        mf = fid2mft.get(r["model_file_id"])
        m = models.setdefault(r["model_file_id"], {"mft": mf, "rows": [], "skel": r["skel_file_ref"],
                                                   "toks": set(), "r": r["bound_radius"], "meshes": r["mesh_count"],
                                                   "skinned": r["skinned"]})
        m["rows"].append(r)
        m["toks"].add(r["material_token"])
    mft2model = {m["mft"]: fid for fid, m in models.items() if m["mft"] is not None}

    # content objects -> model / texture evidence
    ctype_objs = collections.Counter()
    for (src_mft, obj), rs in objs.items():
        root = int(rs[0]["field_path"].split("/")[1][4:])
        ctype_objs[root] += 1
        has_model = any(r["target_kind"] == "MODL" for r in rs)
        for r in rs:
            tm, tk = r["target_mft"], r["target_kind"]
            if tk == "MODL":
                fid = mft2model.get(tm)
                m = models.get(fid) if fid else None
                ext = m is not None and m["skel"] not in (0, None) and m["skel"] not in RACE_SKELETONS
                race = m is not None and m["skel"] in RACE_SKELETONS
                fx = m is not None and bool(m["toks"] & FX_TOKENS)
                if root == CONTENT_SKIN and m is not None and not race and not ext and not fx:
                    model_ev(tm, "weapon", 8, "cntc:type66 skin model")
                elif root == CONTENT_CHARACTER:
                    model_ev(tm, "creature", 7, "cntc:type67 character model")
                elif root == CONTENT_EFFECT:
                    model_ev(tm, "effect", 6, "cntc:type183 effect model")
                elif root in CONTENT_PROP:
                    model_ev(tm, "scenery", 5, f"cntc:type{root} prop model")
            elif tk == "PIMG" or root in CONTENT_TERRAIN:
                pass
            elif not has_model:
                add(tm, "ui", 4, f"cntc:type{root} (no model)")
            else:
                add(tm, "unknown", 0, f"cntc:type{root} with model")

    # models -> textures
    for fid, m in models.items():
        mh = model_hint.get(m["mft"]) if m["mft"] is not None else None
        race = m["skel"] in RACE_SKELETONS
        ext = m["skel"] not in (0, None) and not race
        for r in m["rows"]:
            tm = r["texture_mft"]
            if tm is None or r["texture_file_id"] == 0:
                continue
            tok = r["material_token"]
            if race and tok in ARMOR_TOKENS:
                add(tm, "armor", 9, f"MODL:race-skel {r['skel_file_ref']} token {tok}")
            elif race and tok in CHAR_TOKENS:
                add(tm, "character", 9, f"MODL:race-skel {r['skel_file_ref']} token {tok}")
            if tok in FX_TOKENS:
                add(tm, "effect", 6, f"MODL:token {tok}")
            if mh:
                add(tm, mh[1], mh[0], f"model {fid} <- {mh[2]}")
            if ext:
                add(tm, "creature", 7, f"MODL:ext-skel {r['skel_file_ref']}")
            elif tok == "opaque":
                add(tm, "scenery", 4, "MODL:token opaque")
            if not m["skinned"] and m["meshes"] == 1 and (m["r"] or 0) == 0:
                add(tm, "effect", 3, "MODL:unskinned quad radius 0")
            if race and tok not in ARMOR_TOKENS | CHAR_TOKENS | FX_TOKENS:
                add(tm, "character", 5, f"MODL:race-skel {r['skel_file_ref']} token '{tok}'")

    # ---- output
    in_models = {r["texture_mft"] for m in models.values() for r in m["rows"] if r["texture_mft"] is not None}
    out = d / "texture_class_hints.jsonl"
    counts = collections.Counter()
    counts_new = collections.Counter()
    examples = collections.defaultdict(list)
    recs: dict[int, dict] = {}
    for mft, s in survey.items():
        e = ev.get(mft, [])
        by = collections.defaultdict(lambda: [0, 0, ""])
        for p, h, v in e:
            b = by[h]
            if p > b[0] or (p == b[0] and not b[2]):
                b[0], b[2] = p, v
            b[1] += 1
        if by:
            hint, (prio, n, via) = max(by.items(), key=lambda kv: (kv[1][0], kv[1][1]))
            others = sorted((h for h in by if h != hint), key=lambda h: -by[h][0])
        else:
            hint, prio, n, via, others = "unknown", 0, 0, "no reference", []
        recs[mft] = {"mft": mft, "file_id": s["file_id"], "hint": hint, "via": via, "priority": prio,
                     "evidence": n, "other_hints": others, "kind": s["kind"], "width": s["width"],
                     "height": s["height"], "in_models": mft in in_models}

    # High-res sidecars: an unreferenced texture whose file id is a referenced
    # texture's id + 3 (or +1/+2) with the same or larger size inherits its hint.
    by_fid = {r["file_id"]: r for r in recs.values()}
    sidecars = collections.Counter()
    for r in recs.values():
        if r["via"] != "no reference":
            continue
        for dlt in (3, 1, 2):
            g = by_fid.get(r["file_id"] - dlt)
            if g and g["via"] != "no reference" and not g["via"].startswith("hires") and r["width"] * r["height"] >= g["width"] * g["height"]:
                r.update(hint=g["hint"], priority=max(g["priority"] - 1, 0), evidence=1,
                         via=f"hires-variant of {g['file_id']} (+{dlt}, {g['width']}x{g['height']} -> {r['width']}x{r['height']}) <- {g['via']}")
                sidecars[(dlt, g["hint"])] += 1
                break
    for r in recs.values():
        if r["via"] == "no reference":
            if r["kind"] == "ATEP":
                r.update(hint="terrain", priority=1, via="kind ATEP")
            elif r["kind"] == "ATEU":
                r.update(hint="ui", priority=1, via="kind ATEU")

    with out.open("w", encoding="utf-8") as f:
        for mft, rec in recs.items():
            f.write(json.dumps(rec) + "\n")
            hint = rec["hint"]
            counts[hint] += 1
            if mft not in in_models:
                counts_new[hint] += 1
            if len(examples[hint]) < 12 and (hint not in ("armor", "terrain") or mft not in in_models):
                examples[hint].append(rec)
    summary = {
        "rules": RULES,
        "textures": len(survey),
        "counts": dict(counts.most_common()),
        "not_in_models_counts": dict(counts_new.most_common()),
        "hires_sidecars": {f"+{k[0]}:{k[1]}": v for k, v in sidecars.most_common()},
        "content_root_type_objects": {str(k): v for k, v in ctype_objs.most_common(40)},
        "examples": examples,
    }
    (d / "texture_class_hints_summary.json").write_text(json.dumps(summary, indent=1), encoding="utf-8")
    print(json.dumps({"counts": summary["counts"], "not_in_models": summary["not_in_models_counts"]}, indent=1))


if __name__ == "__main__":
    main()
