#!/usr/bin/env python3
"""Compares two per-draw logs (draws-<unix>.log from the GW2 Enhanced Graphics overlay) and lists the draw groups
that exist only in the first one, ranked by how much they look like a skinned character.

Take two dumps with "Dump frame" in the overlay: one frame with the character on screen (the charr), one
without (same place and camera, same graphics settings if possible), then:

    python draw_diff.py with-charr.log without-charr.log [--top 20] [--shader-only] [--min-score 0]

A group is (vertex shader hash, pixel shader hash, texture set). The texture set is the format, size and
mip count of each pixel-shader texture, so a body with a different armour texture size shows up as new even if
the shaders are old: --shader-only compares by (vs, ps) alone, which hides that noise but also hides a charr
that shares both shaders with something already on screen.

The score is a heuristic, 0..100, with the reasons printed. It only orders the list; the player (or a later
step that colours the candidate draws in game) decides.
"""
import argparse
import re
import sys

TOKEN = re.compile(r"(\w+)=(\S*)")


def parse_log(text):
    """Returns (header dict, list of group dicts). Group: vs, ps, tex (str), draws, max_idx, vcb_max, ... ints."""
    header = {}
    groups = []
    for raw in text.splitlines():
        line = raw.strip()
        if not line:
            continue
        if line.startswith("#"):
            header.update(dict(TOKEN.findall(line)))
            continue
        if not line.startswith("G "):
            continue
        f = dict(TOKEN.findall(line))
        try:
            vcb_min, vcb_max = (int(x) for x in f["vcb"].split(".."))
            g = {
                "vs": f["vs"],
                "ps": f["ps"],
                "tex": f["tex"],
                "draws": int(f["draws"]),
                "kinds": f.get("kinds", ""),
                "indices": int(f["indices"]),
                "max_idx": int(f["max_idx"]),
                "vertices": int(f["vertices"]),
                "max_vtx": int(f["max_vtx"]),
                "instances": int(f["instances"]),
                "vcb_min": vcb_min,
                "vcb_max": vcb_max,
                "vcbs": [int(x) for x in f["vcbs"].split(",")],
                "pcbs": [int(x) for x in f["pcbs"].split(",")],
                "layouts": int(f["layouts"]),
                "strides": [int(x) for x in f["strides"].split(",") if x],
                "texptrs": int(f["texptrs"]),
            }
        except (KeyError, ValueError) as e:
            raise ValueError(f"bad group line ({e}): {line[:120]}") from None
        groups.append(g)
    return header, groups


def tex_count(g):
    return 0 if g["tex"] == "-" else len(g["tex"].split(","))


def key_of(g, shader_only=False):
    return (g["vs"], g["ps"]) if shader_only else (g["vs"], g["ps"], g["tex"])


def only_in_first(a, b, shader_only=False):
    """Groups of `a` whose key is not in `b`. With shader_only, groups of one (vs, ps) pair are merged
    (draws and sizes added, texture sets joined) so that the pair is listed once."""
    seen = {key_of(g, shader_only) for g in b}
    out = []
    merged = {}
    for g in a:
        k = key_of(g, shader_only)
        if k in seen:
            continue
        if not shader_only:
            out.append(g)
            continue
        m = merged.get(k)
        if m is None:
            merged[k] = dict(g, tex_sets=[g["tex"]])
        else:
            m["draws"] += g["draws"]
            m["indices"] += g["indices"]
            m["vertices"] += g["vertices"]
            m["instances"] += g["instances"]
            m["max_idx"] = max(m["max_idx"], g["max_idx"])
            m["max_vtx"] = max(m["max_vtx"], g["max_vtx"])
            m["vcb_max"] = max(m["vcb_max"], g["vcb_max"])
            m["vcb_min"] = min(m["vcb_min"], g["vcb_min"])
            m["texptrs"] += g["texptrs"]
            m["tex_sets"].append(g["tex"])
    return out if not shader_only else list(merged.values())


def score(g):
    """Likelihood (0..100) that the group is a skinned character: a big vertex constant buffer (bone
    matrices), 3-4 textures (diffuse, normal, mask/spec), index counts in the thousands, not instanced."""
    pts = 0
    why = []
    v = g["vcb_max"]
    if v >= 4096:
        pts += 40
        why.append(f"vertex constant buffer {v} B (>=4096)")
    elif v >= 2048:
        pts += 32
        why.append(f"vertex constant buffer {v} B (>=2048)")
    elif v >= 1024:
        pts += 20
        why.append(f"vertex constant buffer {v} B (>=1024)")
    elif v >= 512:
        pts += 8
        why.append(f"vertex constant buffer {v} B (>=512)")
    n = tex_count(g)
    if n in (3, 4):
        pts += 25
        why.append(f"{n} textures")
    elif n in (2, 5):
        pts += 12
        why.append(f"{n} textures")
    m = g["max_idx"]
    if 1000 <= m <= 60000:
        pts += 20
        why.append(f"up to {m} indices per draw")
    elif 500 <= m < 1000:
        pts += 10
        why.append(f"up to {m} indices per draw (small)")
    elif m > 60000:
        pts += 5
        why.append(f"up to {m} indices per draw (huge)")
    if g["draws"] and g["instances"] > 2 * g["draws"]:
        pts -= 10
        why.append(f"instanced ({g['instances']} instances in {g['draws']} draws)")
    else:
        pts += 5
    if g["draws"] <= 8:
        pts += 5
        why.append(f"few draws ({g['draws']})")
    if g["strides"] and max(g["strides"]) >= 32:
        pts += 5
        why.append(f"vertex stride {max(g['strides'])} B")
    return max(0, min(100, pts)), why


def ranked(groups):
    r = [(score(g), g) for g in groups]
    r.sort(key=lambda x: (-x[0][0], -x[1]["draws"], x[1]["vs"], x[1]["ps"], x[1]["tex"]))
    return [(s, why, g) for ((s, why), g) in r]


def report(a_text, b_text, top=20, shader_only=False, min_score=0):
    ha, ga = parse_log(a_text)
    hb, gb = parse_log(b_text)
    new = only_in_first(ga, gb, shader_only)
    rows = [r for r in ranked(new) if r[0] >= min_score]
    out = []
    out.append(f"first : {len(ga)} groups, {sum(g['draws'] for g in ga)} draws (frame {ha.get('frame', '?')}, pass {ha.get('pass', '?')})")
    out.append(f"second: {len(gb)} groups, {sum(g['draws'] for g in gb)} draws (frame {hb.get('frame', '?')}, pass {hb.get('pass', '?')})")
    out.append(f"only in the first ({'vs+ps' if shader_only else 'vs+ps+textures'}): {len(new)} groups, {sum(g['draws'] for g in new)} draws")
    if ha.get("how", "classifier") != "classifier" or hb.get("how", "classifier") != "classifier":
        out.append("warning: the opaque pass was not found by the classifier in one of the logs; the pass may be wrong")
    out.append("")
    for i, (s, why, g) in enumerate(rows[:top], 1):
        out.append(f"#{i} score {s}  vs={g['vs']} ps={g['ps']}  draws={g['draws']} kinds={g['kinds']}")
        texs = g.get("tex_sets") or [g["tex"]]
        for t in texs[:4]:
            out.append(f"     tex {t}")
        if len(texs) > 4:
            out.append(f"     ... {len(texs) - 4} more texture sets")
        out.append(f"     indices={g['indices']} (max {g['max_idx']}) vertices={g['vertices']} instances={g['instances']} vcb={g['vcb_min']}..{g['vcb_max']} pcbs={g['pcbs']} strides={g['strides']} layouts={g['layouts']}")
        out.append(f"     why: {'; '.join(why) if why else 'nothing stands out'}")
    if not rows:
        out.append("nothing only in the first log (or all below --min-score)")
    return "\n".join(out) + "\n"


def main(argv=None):
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    p.add_argument("with_target", help="log of the frame WITH the character")
    p.add_argument("without_target", help="log of the frame WITHOUT the character")
    p.add_argument("--top", type=int, default=20)
    p.add_argument("--shader-only", action="store_true", help="compare by (vs, ps) only, ignore the texture sets")
    p.add_argument("--min-score", type=int, default=0)
    a = p.parse_args(argv)
    with open(a.with_target, encoding="utf-8") as f:
        ta = f.read()
    with open(a.without_target, encoding="utf-8") as f:
        tb = f.read()
    sys.stdout.write(report(ta, tb, a.top, a.shader_only, a.min_score))
    return 0


if __name__ == "__main__":
    sys.exit(main())
