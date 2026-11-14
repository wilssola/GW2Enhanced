#!/usr/bin/env python3
"""Summarises a draws-fx-<unix>.log (effects passes capture of the GW2 Enhanced Graphics overlay).

    python fx_summary.py a debug log [--top 8]

Per pass (the `# fxpass=` header lines: colour/depth format, draws) it prints the top groups, quad-like groups first
(screen-aligned quads / small meshes: the usual hard-edged effect rectangles), then by draw count. Each line shows the
blend state of RT0, draws, shaders and the texture set.
"""
import argparse
import re
import sys

TOKEN = re.compile(r"(\w+)=(\S*)")


def parse_fx_log(text):
    """Returns (header dict, {pass: info dict}, list of group dicts)."""
    header = {}
    passes = {}
    groups = []
    for raw in text.splitlines():
        line = raw.strip()
        if not line:
            continue
        f = dict(TOKEN.findall(line))
        if line.startswith("#"):
            if "fxpass" in f:
                try:
                    passes[int(f["fxpass"])] = {"rt0": f.get("rt0", "?"), "depth": f.get("depth", "?"),
                                                "pass_draws": int(f.get("pass_draws", 0)),
                                                "captured": int(f.get("captured", 0))}
                except ValueError:
                    raise ValueError(f"bad fxpass line: {line[:120]}") from None
            else:
                header.update(f)
            continue
        if not line.startswith("G "):
            continue
        try:
            groups.append({
                "pass": int(f["pass"]),
                "blend": f["blend"],
                "quad": f["quad"] == "1",
                "draws": int(f["draws"]),
                "vs": f["vs"],
                "ps": f["ps"],
                "max_idx": int(f["max_idx"]),
                "max_vtx": int(f["max_vtx"]),
                "instances": int(f["instances"]),
                "tex": f["tex"],
            })
        except (KeyError, ValueError) as e:
            raise ValueError(f"bad group line ({e}): {line[:120]}") from None
    return header, passes, groups


def rank(groups):
    """Quad-like groups first, then more draws first; ties by shader pair so the order is stable."""
    return sorted(groups, key=lambda g: (not g["quad"], -g["draws"], g["vs"], g["ps"], g["blend"]))


def is_additive(blend):
    return blend.startswith("ADD:") and blend.split("|")[0].endswith(":ONE")


def report(header, passes, groups, top=8):
    out = [f"effects capture: frame={header.get('frame', '?')} opaque_pass={header.get('opaque_pass', '?')} "
           f"captured={header.get('captured', '?')} dropped={header.get('dropped', '?')}"]
    by_pass = {}
    for g in groups:
        by_pass.setdefault(g["pass"], []).append(g)
    for p in sorted(set(passes) | set(by_pass)):
        info = passes.get(p, {})
        gs = rank(by_pass.get(p, []))
        quads = sum(g["draws"] for g in gs if g["quad"])
        total = sum(g["draws"] for g in gs)
        out.append(f"pass #{p}  rt0={info.get('rt0', '?')} depth={info.get('depth', '?')} draws={info.get('pass_draws', '?')} "
                   f"captured={total} quad-like={quads} groups={len(gs)}")
        for g in gs[:top]:
            flags = ("QUAD " if g["quad"] else "     ") + ("ADDITIVE " if is_additive(g["blend"]) else "")
            out.append(f"  {flags}draws={g['draws']} blend={g['blend']} vs={g['vs']} ps={g['ps']} "
                       f"max_idx={g['max_idx']} max_vtx={g['max_vtx']} instances={g['instances']} tex={g['tex']}")
        if len(gs) > top:
            out.append(f"  ... {len(gs) - top} more groups")
    return "\n".join(out)


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("log")
    ap.add_argument("--top", type=int, default=8, help="groups shown per pass (default 8)")
    a = ap.parse_args(argv)
    with open(a.log, encoding="utf-8") as f:
        text = f.read()
    try:
        header, passes, groups = parse_fx_log(text)
    except ValueError as e:
        print(f"error: {e}", file=sys.stderr)
        return 2
    print(report(header, passes, groups, a.top))
    return 0


if __name__ == "__main__":
    sys.exit(main())
